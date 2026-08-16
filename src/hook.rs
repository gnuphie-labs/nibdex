// SPDX-License-Identifier: MIT

//! `nibdex hook` — attach nibdex's answer to a shell search, at grep's price.
//!
//! # Why this exists
//!
//! nibdex's MCP tools are **deferred** by the host: reaching one costs a
//! `ToolSearch` before the call, which `grep` never pays. Measured across 110
//! sessions on an 18-repo corpus, all 53 that used nibdex fetched its schemas
//! first. The obvious remedy is dead — a 2-tool server in an otherwise empty
//! workspace is deferred too, so shrinking the tool surface cannot win residency.
//!
//! But `Bash` and `Grep` are **resident and free**. This rides them: when the
//! caller runs a search, the hook answers the same question from the index and
//! attaches the result. nibdex's answer arrives at grep's price — the one thing
//! collapsing the tool surface could not achieve.
//!
//! # Augment, never intercept
//!
//! The search still runs. The caller gets the live result AND the index's
//! structure, both labelled. Substituting would risk an index answer posing as a
//! live one, could loop if a denied search were retried, and would hide nibdex's
//! contribution from any measurement that classifies by tool name.
//!
//! # Three rules
//!
//! 1. **Fail open.** Any error, timeout, or missing index emits nothing and
//!    allows the tool. nibdex being unavailable must never break retrieval.
//! 2. **Free on the common path.** This fires on EVERY `Bash` call — 267 in one
//!    measured session, mostly `cargo test` and `git commit`. A non-search must
//!    be rejected before any I/O.
//! 3. **Label it, with freshness.** Unlabelled output is indistinguishable from
//!    the tool's own, which is the failure mode nibdex exists to prevent.

use std::io::Read;
use std::str::FromStr;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::{json, Value};

/// One indexed hit as the renderer needs it: path, line, the matching text, and
/// the commit that last touched it.
pub(crate) type Hit = (String, i64, String, Option<String>);

/// Hits pulled from the index before grouping.
const MAX_HITS: i64 = 25;
/// Lines shown from the largest location.
const SHOW: usize = 6;
/// Shortest term worth querying — below this the index returns noise.
const MIN_TERM: usize = 3;

/// Search tools whose output answers "where is X", as opposed to filtering
/// another command's output. Must LEAD a pipeline segment: `cargo test | grep x`
/// is trimming a build log, not asking the index anything.
const SEARCH_TOOLS: [&str; 5] = ["grep", "rg", "ripgrep", "ack", "ag"];

/// Flags that consume the next argument, so it is not mistaken for the pattern.
const VALUED_FLAGS: [&str; 13] = [
    "-e", "-f", "-m", "-A", "-B", "-C", "-t", "-g", "--include", "--exclude",
    "--exclude-dir", "--max-count", "--type",
];

/// Append one line recording what this firing did.
///
/// WHY THIS IS NOT OPTIONAL. Arguing for augment-over-intercept, I claimed the
/// adoption instrument would still see nibdex's contribution because the tool
/// name is unchanged. That is false: the instrument counts TOOL CALLS, and a
/// hook injection is not one — so an answer delivered this way registers as
/// neither grep nor nibdex. It is invisible. Building a feature whose effect
/// cannot be measured is the exact mistake the adoption work exists to correct,
/// so the hook records its own firings.
///
/// Counts and shapes only: the search TERM is recorded because it is the
/// caller's own query and is needed to tell a useful firing from a noisy one,
/// but no file contents, no result bodies, and no command text. JSONL, appended,
/// best-effort — a logging failure must never affect the caller.
fn log_firing(outcome: &str, pat: &str, scoped: bool, hits: usize, db: Option<&Path>) {
    let Some(home) = std::env::var_os("HOME") else { return };
    let dir = PathBuf::from(home).join(".nibdex");
    let _ = std::fs::create_dir_all(&dir);
    let now = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let line = format!(
        "{{\"ts\":{},\"outcome\":\"{}\",\"term_len\":{},\"term\":\"{}\",\
         \"scoped\":{},\"hits\":{},\"db\":\"{}\"}}\n",
        now,
        outcome,
        pat.chars().count(),
        pat.replace(['\\', '"'], ""),
        scoped,
        hits,
        db.map(|d| d.display().to_string()).unwrap_or_default(),
    );
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("hook-log.jsonl"))
    {
        let _ = f.write_all(line.as_bytes());
    }
}

/// Emit nothing: the tool runs exactly as it would have. Rule 1 and rule 2 both
/// land here, and it is the only exit an error may take.
fn allow_silently() -> ! {
    std::process::exit(0);
}

/// Shell-ish tokenizer: splits on whitespace but keeps quoted runs intact, and
/// emits `;` `&&` `||` `|` as their own tokens.
///
/// Naive `split_whitespace` loses the pattern in `grep -n "two words" f.rs`,
/// returning only `two` — and a wrong pattern means the hook answers a
/// DIFFERENT question than the caller asked, which is the exact failure this
/// design exists to avoid. Splitting the raw string on separators has the mirror
/// bug: it would cut `grep "a;b" f` in half.
fn tokenize(command: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut chars = command.chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                } else {
                    cur.push(c);
                }
            }
            None => match c {
                '"' | '\'' => quote = Some(c),
                c if c.is_whitespace() => {
                    if !cur.is_empty() {
                        out.push(std::mem::take(&mut cur));
                    }
                }
                // A LONE `&` is not a separator — `2>&1` is a redirect, and
                // treating it as one splits `cargo test 2>&1 | grep x` such that
                // grep appears to LEAD a segment, turning a build-log filter
                // into a false search. Only `&&` separates.
                '&' if chars.peek() != Some(&'&') => cur.push(c),
                ';' | '|' | '&' => {
                    if !cur.is_empty() {
                        out.push(std::mem::take(&mut cur));
                    }
                    if chars.peek() == Some(&c) {
                        chars.next();
                        out.push(format!("{c}{c}")); // `&&` / `||`
                    } else {
                        out.push(c.to_string());     // `;` / `|`
                    }
                }
                _ => cur.push(c),
            },
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// The search term, or `None` if this command is not a search we can serve.
pub(crate) fn pattern_from(command: &str) -> Option<String> {
    let tokens = tokenize(command);
    // Split into STATEMENTS, then take only the head of each pipeline. A `grep`
    // after a pipe is filtering the previous command's output — `cargo test |
    // grep '^test result'` asks the index nothing — so a segment that merely
    // begins with a search tool is not enough; it must begin the statement.
    for stmt in tokens.split(|t| t == ";" || t == "&&" || t == "||") {
        let head: Vec<&String> = stmt.iter().take_while(|t| *t != "|").collect();
        let mut it = head.into_iter();
        let Some(first) = it.next() else { continue };
        let first = if first == "sudo" {
            match it.next() {
                Some(t) => t,
                None => continue,
            }
        } else {
            first
        };
        let Some(tool) = Path::new(first).file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if !SEARCH_TOOLS.contains(&tool) {
            // Only a segment that LEADS with a search counts; a later `| grep`
            // is filtering another command's output, not asking the index.
            continue;
        }
        let args: Vec<&String> = it.collect();
        let mut i = 0;
        while i < args.len() {
            let a = args[i].as_str();
            if VALUED_FLAGS.contains(&a) {
                i += 2;
                continue;
            }
            if a.starts_with('-') {
                i += 1;
                continue;
            }
            return Some(args[i].clone());
        }
        return None;
    }
    None
}

/// The path arguments of a search command — the scope the caller already stated.
///
/// ⚠️ Ignoring this answers a DIFFERENT question than the one asked. Observed
/// live: `grep -rn "AppState" formsvc/src/` was answered from the whole
/// 18-repo index, so bm25 surfaced a different project's `AppState` and the
/// top-25 cap meant the searched directory barely appeared. The caller had
/// already said where to look; not using it is both less accurate and more work.
pub(crate) fn paths_from(command: &str) -> Vec<String> {
    let tokens = tokenize(command);
    for stmt in tokens.split(|t| t == ";" || t == "&&" || t == "||") {
        let head: Vec<&String> = stmt.iter().take_while(|t| *t != "|").collect();
        let mut it = head.into_iter();
        let Some(first) = it.next() else { continue };
        let first = if first == "sudo" { match it.next() { Some(t) => t, None => continue } } else { first };
        let Some(tool) = Path::new(first).file_name().and_then(|s| s.to_str()) else { continue };
        if !SEARCH_TOOLS.contains(&tool) {
            continue;
        }
        let args: Vec<&String> = it.collect();
        let mut i = 0;
        let mut seen_pattern = false;
        let mut out = Vec::new();
        while i < args.len() {
            let a = args[i].as_str();
            if VALUED_FLAGS.contains(&a) {
                i += 2;
                continue;
            }
            if a.starts_with('-') {
                i += 1;
                continue;
            }
            if !seen_pattern {
                seen_pattern = true;      // the first bare arg is the pattern
            } else if a != "." {          // "." adds no narrowing
                out.push(a.trim_end_matches('/').to_string());
            }
            i += 1;
        }
        return out;
    }
    Vec::new()
}

/// A term the FTS index can match usefully. Regex metacharacters mean the caller
/// is expressing something the index cannot honour, and answering a *different*
/// question than the one asked is the failure this whole design avoids.
pub(crate) fn term_is_indexable(pat: &str) -> bool {
    pat.chars().count() >= MIN_TERM
        && !pat.contains(|c| "^$[]()\\|+*?{}".contains(c))
}

/// Open `db` for the hook's probes. `create_if_missing(false)` and NO
/// migrations — deliberately not `db::open`, which would create a stray db or
/// silently migrate one the hook has no business touching (observed for real: a
/// pipe-test migrated an unrelated stale db at the workspace root). Not
/// `read_only(true)` either: a WAL database whose `-shm` sidecar is absent (any
/// db after a clean close by another SQLite client) cannot be opened read-only at
/// all — SQLite needs to create the sidecar — and the hook then failed silently
/// on exactly the freshly-indexed db it should have served (RC1 review 1.8).
/// A read-write handle that only ever runs SELECTs writes nothing but sidecars.
/// The earlier `sqlite3` CLI shell-outs had the same `-readonly` defect and an
/// undocumented external-binary dependency; both are gone.
async fn open_probe(db: &Path) -> Option<sqlx::SqlitePool> {
    let opts = sqlx::sqlite::SqliteConnectOptions::from_str(&format!("sqlite://{}", db.display()))
        .ok()?
        .create_if_missing(false);
    sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .ok()
}

/// `indexed_repos.repo_path` rows, or `None` when the db can't answer.
async fn indexed_repo_paths(pool: &sqlx::SqlitePool) -> Option<Vec<String>> {
    sqlx::query_scalar::<_, String>("SELECT repo_path FROM indexed_repos")
        .fetch_all(pool)
        .await
        .ok()
}

/// Does this database index the tree being searched?
///
/// ⚠️ THE DEFECT THIS EXISTS FOR. "First candidate with content" stops an EMPTY
/// database shadowing a real one, but not a WRONG one. Observed live: a search
/// in an 18-repo work tree was answered from a database indexing an unrelated
/// personal project — returning that project's `AppState`, with provenance, and
/// a confident "index current". Well-formed, well-labelled, wrong codebase.
///
/// So content is necessary and relevance is the test: a candidate qualifies only
/// if it indexes a repo that contains, or is contained by, the directory being
/// searched.
async fn db_indexes(pool: &sqlx::SqlitePool, cwd: &Path) -> bool {
    let Some(repos) = indexed_repo_paths(pool).await else {
        return false;
    };
    let relevant = repos.iter().any(|repo| {
        let repo = Path::new(repo.trim());
        !repo.as_os_str().is_empty() && (cwd.starts_with(repo) || repo.starts_with(cwd))
    });
    // ...and it must be able to ANSWER. A half-built database can carry
    // `indexed_repos` while lacking `source_chunks` entirely — observed for real
    // beside a working index, where it qualified on relevance, was checked
    // first, and then failed the query, silencing the hook. Relevant is not the
    // same as usable.
    relevant
        && sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM source_chunks")
            .fetch_one(pool)
            .await
            .is_ok_and(|n| n > 0)
}

/// Which indexed repo the caller is standing in — the LONGEST `indexed_repos`
/// entry that contains `cwd`.
///
/// Without this the hook answers across repos. Observed live on 2026-08-15:
/// `grep -rn AppState src/` run inside the nibdex repo came back with another
/// project's `backend/src/handlers/transactions.rs`, logged `scoped: true`. The
/// caller's scope is honoured as a path *substring* against a repo-RELATIVE
/// stored path, and every repo has a `src/`, so the scope matched everywhere.
///
/// Worse than an ordinary miss because the hook is unsolicited: nothing prompted
/// the caller to doubt the answer, and it arrived carrying provenance and an
/// "index current" stamp. Well-formed, well-labelled, wrong repo.
///
/// Longest match, because a workspace container can itself be an indexed repo:
/// both `~/ws` and `~/ws/proj` may contain `cwd`, and the answer wanted is the
/// innermost. Returns `None` when nothing contains `cwd` — the caller then leaves
/// the query unconstrained rather than inventing a scope.
async fn repo_root_for(pool: &sqlx::SqlitePool, cwd: &Path) -> Option<String> {
    indexed_repo_paths(pool)
        .await?
        .into_iter()
        .map(|r| r.trim().to_string())
        .filter(|r| !r.is_empty() && cwd.starts_with(Path::new(r)))
        .max_by_key(|r| r.len())
}

/// Nearest index that actually COVERS `cwd`, searching upward. Returns the path
/// and an open probe handle on it (so the covering check and the query share
/// one open).
async fn find_db(cwd: &Path) -> Option<(PathBuf, sqlx::SqlitePool)> {
    if let Some(env) = std::env::var_os("NIBDEX_HOOK_DB") {
        let p = PathBuf::from(env);
        if !p.exists() {
            return None;
        }
        let pool = open_probe(&p).await?;
        return Some((p, pool));
    }
    let mut dir = cwd.to_path_buf();
    loop {
        for cand in [dir.join("nibdex.db"), dir.join("nibdex").join("nibdex.db")] {
            if cand.exists()
                && let Some(pool) = open_probe(&cand).await
            {
                if db_indexes(&pool, cwd).await {
                    return Some((cand, pool));
                }
                pool.close().await;
            }
        }
        if !dir.pop() {
            // No index covers this tree, so say nothing. An answer drawn from an
            // index that does not cover the question is worse than no answer,
            // because it is indistinguishable from a good one.
            return None;
        }
    }
}

/// How current the index is. Reported on every answer, never omitted — an index
/// answer without its age is precisely the silent-staleness failure.
///
/// Measured from the newest INDEXING pass recorded in the db — the latest
/// `indexed_repos.last_indexed_at` (stamped by both `nibdex index` and the
/// watcher's on-commit reindex) — NOT from the db file's mtime. Every MCP query
/// writes a latency row, so the file (and its `-wal`) is touched by *reads*; the
/// old mtime-based stamp reported "index current" on an index last built weeks
/// earlier, as long as anything had queried it in the last five minutes (RC1
/// review 1.8). Falls back to "age unknown" when the db can't say.
pub(crate) async fn freshness(pool: &sqlx::SqlitePool) -> String {
    let newest: Option<i64> =
        sqlx::query_scalar::<_, Option<i64>>("SELECT MAX(last_indexed_at) FROM indexed_repos")
            .fetch_one(pool)
            .await
            .ok()
            .flatten();
    let Some(t) = newest else {
        return "age unknown".to_string();
    };
    let now = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    freshness_label((now - t).max(0) as u64)
}

/// The age → label mapping, split out so it is unit-testable without a db.
pub(crate) fn freshness_label(age_secs: u64) -> String {
    match age_secs {
        0..=299 => "index current".to_string(),
        300..=86_399 => format!("index {}h old", age_secs / 3600),
        _ => format!(
            "⚠ index {} days old — may predate recent work",
            age_secs / 86_400
        ),
    }
}

/// Group hits by directory and render the caller-facing text.
///
/// Grouping is the whole value: measured on an 18-repo corpus, grep returns
/// 3,611 lines conflating 121 distinct locations while a typical location holds
/// 16. Naming the location is the gain, and it needs no parser — only the path
/// nibdex already stores per chunk.
pub(crate) fn render(pat: &str, hits: &[Hit], fresh: &str, db: &str) -> Option<String> {
    if hits.is_empty() {
        return None;
    }
    let mut groups: std::collections::BTreeMap<String, Vec<&Hit>> = Default::default();
    for h in hits {
        let dir = Path::new(&h.0)
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| ".".to_string());
        groups.entry(dir).or_default().push(h);
    }
    let mut by_size: Vec<_> = groups.iter().collect();
    by_size.sort_by_key(|(_, v)| std::cmp::Reverse(v.len()));

    let mut out = vec![format!(
        "[nibdex] {} indexed hit(s) for {:?} across {} location(s) — {}, from {}.",
        hits.len(),
        pat,
        groups.len(),
        fresh,
        db
    )];
    if groups.len() > 1 {
        out.push(format!(
            "  locations: {}",
            by_size
                .iter()
                .take(5)
                .map(|(d, v)| format!("{} ({})", d, v.len()))
                .collect::<Vec<_>>()
                .join(" · ")
        ));
    }
    let (top_dir, top) = by_size[0];
    out.push(format!("  from {top_dir}:"));
    for h in top.iter().take(SHOW) {
        let commit = h
            .3
            .as_deref()
            .map(|c| format!("  (via {})", &c[..c.len().min(7)]))
            .unwrap_or_default();
        let body = h.2.trim();
        out.push(format!(
            "    {}:{}: {}{}",
            h.0,
            h.1,
            &body[..body.len().min(110)],
            commit
        ));
    }
    if top.len() > SHOW {
        out.push(format!("    …{} more here", top.len() - SHOW));
    }
    out.push(
        "  (each hit carries the commit that last touched it; the shell search \
         below still ran and is authoritative for uncommitted work)"
            .to_string(),
    );
    Some(out.join("\n"))
}

/// Read the `PreToolUse` event, decide, and emit. Never returns an error to the
/// caller: every failure path exits 0 having printed nothing (rule 1).
pub async fn run() -> ! {
    if std::env::var_os("NIBDEX_HOOK_OFF").is_some() {
        allow_silently();
    }
    let mut raw = String::new();
    if std::io::stdin().read_to_string(&mut raw).is_err() {
        allow_silently();
    }
    let Ok(ev): Result<Value, _> = serde_json::from_str(&raw) else {
        allow_silently();
    };

    let tool = ev.get("tool_name").and_then(Value::as_str).unwrap_or("");
    if tool != "Bash" && tool != "Grep" {
        allow_silently();
    }
    let input = ev.get("tool_input").cloned().unwrap_or(Value::Null);

    // Rule 2: reject the common case before touching the filesystem.
    let pat = if tool == "Grep" {
        input
            .get("pattern")
            .and_then(Value::as_str)
            .map(str::to_string)
    } else {
        let cmd = input.get("command").and_then(Value::as_str).unwrap_or("");
        if !SEARCH_TOOLS.iter().any(|t| cmd.contains(t)) {
            allow_silently();
        }
        pattern_from(cmd)
    };
    let scope = if tool == "Grep" {
        input.get("path").and_then(Value::as_str).map(|p| vec![p.to_string()]).unwrap_or_default()
    } else {
        paths_from(input.get("command").and_then(Value::as_str).unwrap_or(""))
    };
    let Some(pat) = pat.filter(|p| term_is_indexable(p)) else {
        // Not logged: this is the fast reject for non-searches and unusable
        // terms, and it fires on nearly every Bash call. Logging it would cost
        // an open+write on the path that must stay free (rule 2), and would
        // swamp the signal with build and git traffic.
        allow_silently();
    };

    let cwd = ev
        .get("cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let Some((db, pool)) = find_db(&cwd).await else {
        // A real miss worth counting: the caller asked something answerable and
        // no index covered their tree.
        log_firing("no_index", &pat, !scope.is_empty(), 0, None);
        allow_silently();
    };
    // `pool` is the probe handle from `open_probe`: no create, no migrations, and
    // nothing below runs anything but SELECTs. See `open_probe` for why it is not
    // opened read-only.
    // OVER-FETCH WHEN SCOPED. Filtering after the limit is backwards: the
    // globally top-ranked N may contain none of the caller's directory, so the
    // narrower — and more correct — their scope, the likelier the answer is
    // empty. Observed live: `grep AppState formsvc/src/` on an 18-repo index
    // returned nothing, because the top 25 for that term were all other repos.
    let fetch = if scope.is_empty() { MAX_HITS } else { MAX_HITS * 40 };
    // Constrain to the repo the caller is actually standing in. The path scope
    // below is a substring test against repo-relative paths, so on its own it
    // admits every repo that happens to have a directory of the same name — see
    // `repo_root_for`. Applied in SQL so the fetch limit is spent inside the
    // right repo rather than on other repos' top-ranked rows.
    let repo = repo_root_for(&pool, &cwd).await;
    let hits = crate::source_index::find_code_in_repo(&pool, &pat, fetch, repo.as_deref()).await;
    let fresh = freshness(&pool).await;
    pool.close().await;
    let Ok(hits) = hits else { allow_silently() };

    // `find_code` reports the snippet's start line, which does not always hold
    // the term — a hit can surface as a bare `}`. Junk in an injected answer
    // teaches the reader to discount the channel, so keep only real matches.
    let needle = pat.to_lowercase();
    let rows: Vec<Hit> = hits
        .into_iter()
        .filter_map(|h| {
            // Honour the caller's stated scope. Their path is workspace-relative
            // or repo-relative; the index stores repo-relative, so a substring
            // match on the tail is the reliable comparison.
            if !scope.is_empty() {
                let hp = h.path.replace('\\', "/");
                if !scope.iter().any(|s| {
                    let s = s.trim_start_matches("./");
                    hp.starts_with(s) || hp.contains(&format!("/{s}"))
                        || s.split('/').next_back().is_some_and(|tail| hp.starts_with(tail))
                }) {
                    return None;
                }
            }
            let line = h
                .body
                .lines()
                .find(|l| l.to_lowercase().contains(&needle))?
                .to_string();
            Some((h.path, h.match_line, line, h.commit_sha))
        })
        .take(MAX_HITS as usize)   // cap AFTER scoping, not before
        .collect();

    let Some(text) = render(&pat, &rows, &fresh, &db.display().to_string()) else {
        // Answerable question, index present, nothing matched. Distinguishing
        // this from "no index" is the same corpus_empty distinction the query
        // layer already makes.
        log_firing("no_hits", &pat, !scope.is_empty(), 0, Some(&db));
        allow_silently();
    };
    log_firing("served", &pat, !scope.is_empty(), rows.len(), Some(&db));
    println!(
        "{}",
        json!({"hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "allow",
            "additionalContext": text}})
    );
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rule 2, the load-bearing one: a non-search must be rejected outright.
    /// Getting this wrong taxes the one retrieval path that is currently free.
    #[test]
    fn only_leading_search_commands_yield_a_pattern() {
        // real searches
        assert_eq!(pattern_from("grep -rn needle src/").as_deref(), Some("needle"));
        assert_eq!(pattern_from("rg --files-with-matches TODO").as_deref(), Some("TODO"));
        assert_eq!(pattern_from("sudo grep -n secret /etc/x").as_deref(), Some("secret"));
        assert_eq!(pattern_from("grep -n \"quoted term\" f.rs").as_deref(), Some("quoted term"));

        // flags that swallow their value must not be read as the pattern
        assert_eq!(pattern_from("grep -A 5 needle f.rs").as_deref(), Some("needle"));
        assert_eq!(pattern_from("grep --include *.rs needle src/").as_deref(), Some("needle"));

        // NOT searches: filtering another command's output
        // `2>&1` is a redirect, not a separator. Treating it as one made grep
        // appear to LEAD a segment and turned a build-log filter into a search.
        assert_eq!(pattern_from("cargo test 2>&1 | grep '^test result'"), None);
        assert_eq!(pattern_from("./x 2>&1 | grep needle"), None);
        // ...but `&&` genuinely does separate, and a search after one counts.
        assert_eq!(pattern_from("cd src && grep -rn needle .").as_deref(), Some("needle"));
        assert_eq!(pattern_from("git log --oneline | grep fix"), None);
        assert_eq!(pattern_from("curl -s localhost/healthz | grep git_sha"), None);
        // not a search at all
        assert_eq!(pattern_from("cargo build"), None);
        assert_eq!(pattern_from("ls -l"), None);
    }

    /// A term the index cannot honour must be declined, not approximated —
    /// answering a different question than the one asked is the failure mode
    /// this whole design exists to avoid.
    /// The caller's path arguments ARE the scope, and ignoring them answers a
    /// different question. Live failure: `grep -rn "AppState" formsvc/src/`
    /// was answered from an entire 18-repo index, so another project's matches
    /// outranked the searched directory and the cap hid it almost entirely.
    #[test]
    fn path_arguments_are_extracted_as_scope() {
        assert_eq!(paths_from("grep -rn AppState formsvc/src/"), vec!["formsvc/src"]);
        assert_eq!(paths_from("grep -rn needle src/ tests/"), vec!["src", "tests"]);
        // flags that swallow a value must not contribute a path
        assert_eq!(paths_from("grep -A 5 needle src/"), vec!["src"]);
        assert_eq!(paths_from("grep --include *.rs needle src/"), vec!["src"]);
        // the pattern itself is never a path
        assert!(paths_from("grep -rn AppState").is_empty());
        // "." narrows nothing, so it must not become a filter that excludes all
        assert!(paths_from("grep -rn needle .").is_empty());
        // not a search at all
        assert!(paths_from("cargo test 2>&1 | grep '^test result'").is_empty());
    }

    #[test]
    fn regex_and_tiny_terms_are_declined() {
        assert!(term_is_indexable("session_edges"));
        assert!(term_is_indexable("AppState"));
        assert!(!term_is_indexable("ab"), "too short to be useful");
        assert!(!term_is_indexable("^fn "), "anchored regex");
        assert!(!term_is_indexable("foo.*bar"), "regex");
        assert!(!term_is_indexable("a|b"), "alternation");
        assert!(!term_is_indexable("parse_config("), "unbalanced paren");
    }

    /// The output must ALWAYS name itself and state the index's age. Unlabelled,
    /// it is indistinguishable from the tool's own output.
    #[test]
    fn rendered_text_is_labelled_and_carries_freshness() {
        let hits = vec![
            ("a/x.rs".to_string(), 4, "fn needle() {}".to_string(), Some("deadbeef1".to_string())),
            ("a/y.rs".to_string(), 9, "needle()".to_string(), None),
            ("b/z.rs".to_string(), 1, "let needle = 1;".to_string(), None),
        ];
        let out = render("needle", &hits, "index current", "/x/nibdex.db").expect("hits render");
        assert!(out.starts_with("[nibdex]"), "must name itself: {out}");
        assert!(out.contains("index current"), "must state freshness: {out}");
        // Naming the index is what makes a wrong-tree answer detectable. Without
        // it, "index current" reads as a claim about the caller's tree.
        assert!(out.contains("/x/nibdex.db"), "must name WHICH index answered: {out}");
        assert!(out.contains("2 location(s)"), "must group by location: {out}");
        assert!(out.contains("via deadbee"), "must carry provenance (7-char sha): {out}");
        assert!(
            out.contains("still ran and is authoritative"),
            "must not imply it replaced the search: {out}"
        );
        // empty in, nothing out — never inject an empty banner
        assert!(render("needle", &[], "index current", "/x/nibdex.db").is_none());
    }
}
