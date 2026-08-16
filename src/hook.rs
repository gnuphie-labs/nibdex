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
///
/// ⚠️ `-e` IS NOT ONE OF THESE, and it was. Treated like `-A 5` — a flag whose
/// value must be skipped — `-e` had its value skipped too, and the value of `-e`
/// is exactly the pattern. So the next bare token was taken instead: the PATH.
/// Driving the release binary, `grep -e resolve_capturing_commit src` injected 25
/// hits for `"src"`, labelled, provenance-stamped, freshness-stamped and logged
/// `served` — a confident, well-formed answer to a question nobody asked. That is
/// the failure this whole file exists to avoid, and it is worse than a miss
/// because nothing prompts the caller to doubt it; it also counted as a delivery,
/// so it inflated the one adoption number that measures this path.
const VALUED_FLAGS: [&str; 12] = [
    "-f", "-m", "-A", "-B", "-C", "-t", "-g", "--include", "--exclude",
    "--exclude-dir", "--max-count", "--type",
];

/// What `args[i]` says about the pattern, when it says anything at all.
enum PatternArg {
    /// The pattern itself, and how many tokens carried it.
    Pattern(String, usize),
    /// The pattern is not in `argv`: `-f FILE` supplies patterns from a file we
    /// are not going to read, and a pattern flag with nothing after it is
    /// malformed. Either way the question cannot be known, so it is declined.
    /// Guessing here is what produced the wrong-question injection above.
    Decline,
}

/// Read the `-e` family — the flags whose VALUE IS the pattern — at `args[i]`.
///
/// Shared by `pattern_from` and `paths_from` so the two cannot disagree about
/// which token was the pattern. They did disagree before: one took the path as
/// the pattern while the other consumed the path as the pattern and reported no
/// scope at all, so a `-e` search was both answered for the wrong term AND
/// answered unscoped.
fn pattern_arg_at(args: &[&String], i: usize) -> Option<PatternArg> {
    let a = args[i].as_str();
    let next = args.get(i + 1).map(|s| s.as_str());
    let with_value = |v: Option<&str>, used: usize| {
        Some(match v {
            Some(v) => PatternArg::Pattern(v.to_string(), used),
            None => PatternArg::Decline,
        })
    };
    if a == "-f" || a == "--file" || a.starts_with("--file=") {
        return Some(PatternArg::Decline);
    }
    if a == "-e" || a == "--regexp" {
        return with_value(next, 2);
    }
    if let Some(v) = a.strip_prefix("--regexp=") {
        return Some(PatternArg::Pattern(v.to_string(), 1));
    }
    // Attached short form. `-eneedle` is `-e needle`, and so is `-en` — grep
    // reads everything after `-e` as the value, so this is not a guess.
    if !a.starts_with("--")
        && let Some(v) = a.strip_prefix("-e")
        && !v.is_empty()
    {
        return Some(PatternArg::Pattern(v.to_string(), 1));
    }
    // A short cluster whose LAST flag is `-e`: `grep -rne needle`.
    if a.len() > 2 && a.starts_with('-') && !a.starts_with("--") && a.ends_with('e') {
        return with_value(next, 2);
    }
    None
}

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
fn log_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".nibdex").join("hook-log.jsonl"))
}

/// One JSONL line. Pure, so the escaping can be tested without a filesystem.
///
/// Serialized, never hand-formatted. The hand-rolled writer STRIPPED `\` and `"`
/// from the term and escaped nothing else, so a term or a path holding a control
/// character wrote a line that is not JSON — which `parse_log` then counts as
/// unreadable, making the WRITER's bug read as a broken log. Stripping also
/// silently altered the caller's own query, in the one field recorded to tell a
/// useful firing from a noisy one.
fn log_line(
    now: u64,
    outcome: &str,
    pat: &str,
    scoped: bool,
    hits: usize,
    db: Option<&Path>,
) -> String {
    format!(
        "{}\n",
        json!({
            "ts": now,
            "outcome": outcome,
            "term_len": pat.chars().count(),
            "term": pat,
            "scoped": scoped,
            "hits": hits,
            "db": db.map(|d| d.display().to_string()).unwrap_or_default(),
        })
    )
}

fn log_firing(outcome: &str, pat: &str, scoped: bool, hits: usize, db: Option<&Path>) {
    let Some(path) = log_path() else { return };
    let Some(dir) = path.parent().map(Path::to_path_buf) else { return };
    let _ = std::fs::create_dir_all(&dir);
    let now = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let line = log_line(now, outcome, pat, scoped, hits, db);
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = f.write_all(line.as_bytes());
    }
}

/// One parsed firing. Kept separate from the write side so a malformed or
/// hand-edited line degrades to "skipped", never to a wrong total.
struct Firing {
    ts: u64,
    outcome: String,
    hits: usize,
    scoped: bool,
    db: String,
}

/// Parse the log, discarding anything unreadable. Returns the survivors and the
/// number of lines that could not be read — reporting the discard count matters
/// because a silently-dropped line is the difference between "the hook is quiet"
/// and "the log is broken", and those must not look alike.
fn parse_log(text: &str) -> (Vec<Firing>, usize) {
    let mut out = Vec::new();
    let mut skipped = 0usize;
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            skipped += 1;
            continue;
        };
        let (Some(ts), Some(outcome)) = (
            v.get("ts").and_then(Value::as_u64),
            v.get("outcome").and_then(Value::as_str),
        ) else {
            skipped += 1;
            continue;
        };
        out.push(Firing {
            ts,
            outcome: outcome.to_string(),
            hits: v.get("hits").and_then(Value::as_u64).unwrap_or(0) as usize,
            scoped: v.get("scoped").and_then(Value::as_bool).unwrap_or(false),
            db: v.get("db").and_then(Value::as_str).unwrap_or_default().to_string(),
        });
    }
    (out, skipped)
}

/// Do two path strings name the same file? Canonicalises where it can, because
/// the log records whatever path the hook resolved while `check()` reports
/// whatever path the pool was opened with, and those can differ by a symlink or
/// a relative prefix while naming one database. Falls back to string equality
/// when a path cannot be resolved — a deleted or moved db must not make the
/// comparison panic or silently match everything.
/// `canonical` is the caller's db, ALREADY resolved — resolving it here meant
/// doing it once per log line.
fn same_db(a: &str, canonical: &Path) -> bool {
    if a.is_empty() {
        return false;
    }
    // The overwhelmingly common case is the same db writing and reading the same
    // string, which needs no syscall at all.
    if Path::new(a) == canonical {
        return true;
    }
    match std::fs::canonicalize(a) {
        Ok(x) => x == canonical,
        // Unresolvable and not string-equal: a different database, or one that is
        // gone. Failing open here would inflate every workspace's count.
        Err(_) => false,
    }
}

/// How many answers THIS index has delivered through the hook.
///
/// WHY IT IS SCOPED TO ONE DB, and this is the load-bearing part: the log is
/// machine-global — every workspace's firings append to the same file — while
/// `check().adoption` is deliberately workspace-scoped, because rc.2 had to fix
/// it counting every session on the machine. Folding an unscoped count into a
/// scoped instrument would re-introduce exactly that bug in a new place, so a
/// firing counts only when it names this database.
///
/// Only a SERVED firing counts, either intent. A `no_hits` firing means the hook
/// ran and had nothing, which is not a delivery; `--stats` is where that split
/// belongs.
///
/// ⚠️ The whole log is read into memory on every call, and `check()` calls this.
/// That is fine at today's sizes and is NOT fine forever — the log is
/// machine-global, append-only and nothing rotates it. Naming it here because the
/// fix is a rotation policy, which is a decision about how much history the
/// measurement is allowed to lose, and that is not a decision this function can
/// make on its own.
pub(crate) fn deliveries_for(db: &Path) -> i64 {
    let Some(path) = log_path() else { return 0 };
    let Ok(text) = std::fs::read_to_string(path) else { return 0 };
    let (firings, _) = parse_log(&text);
    // Canonicalise the CALLER's db ONCE. `same_db` used to resolve both sides
    // inside the filter, so this cost two syscalls per log line, on a file that
    // only ever grows, every time `check()` ran.
    let canonical = std::fs::canonicalize(db).unwrap_or_else(|_| db.to_path_buf());
    firings
        .iter()
        .filter(|f| is_delivery(&f.outcome) && same_db(&f.db, &canonical))
        .count() as i64
}

/// An answer actually handed to the caller, either intent.
///
/// `schema_served` belongs here: the index answered and the answer reached the
/// caller, which is what a delivery is. Leaving it out meant a workspace whose
/// searches all miss but whose SQL calls all hit would report
/// `hook_deliveries: 0` — "nibdex is not being used" when the truth is the
/// opposite, on the one instrument built to measure this path.
fn is_delivery(outcome: &str) -> bool {
    outcome == "served" || outcome == "schema_served"
}

/// The outcomes this build knows, by intent. `render_stats` shows everything else
/// as `other` rather than counting it into a total it never displays.
const SEARCH_OUTCOMES: [&str; 4] = ["served", "no_hits", "no_index", "query_error"];
const SCHEMA_OUTCOMES: [&str; 3] = ["schema_served", "schema_no_hits", "schema_no_index"];

/// Median of an already-collected sample. Returns `None` for an empty sample
/// rather than 0, because "no served firings" and "served firings returning
/// nothing" are different facts and a zero would conflate them.
fn median(mut v: Vec<usize>) -> Option<usize> {
    if v.is_empty() {
        return None;
    }
    v.sort_unstable();
    Some(v[v.len() / 2])
}

fn days_ago(ts: u64, now: u64) -> String {
    let secs = now.saturating_sub(ts);
    match secs {
        s if s < 3_600 => format!("{} min ago", s / 60),
        s if s < 86_400 => format!("{} h ago", s / 3_600),
        s => format!("{} d ago", s / 86_400),
    }
}

/// Render the report. Pure, so the shape can be tested without a filesystem.
fn render_stats(f: &[Firing], skipped: usize, now: u64, path: &Path) -> String {
    let mut s = String::new();
    let total = f.len();
    if total == 0 {
        s.push_str(&format!(
            "nibdex hook — no firings recorded.\n\n  log  {}\n\n\
             The hook has either never been wired, or has never seen a search it\n\
             could answer. Those are different problems and this log cannot tell\n\
             them apart: check that `nibdex hook` is in your PreToolUse settings,\n\
             then run a `grep` inside an indexed tree and look again.\n",
            path.display()
        ));
        return s;
    }

    let count = |o: &str| f.iter().filter(|x| x.outcome == o).count();
    let (served, no_hits, no_index) = (count("served"), count("no_hits"), count("no_index"));
    let query_error = count("query_error");
    let (sc_served, sc_no_hits, sc_no_index) =
        (count("schema_served"), count("schema_no_hits"), count("schema_no_index"));
    let searches: usize = SEARCH_OUTCOMES.iter().map(|o| count(o)).sum();
    let schemas: usize = SCHEMA_OUTCOMES.iter().map(|o| count(o)).sum();
    // Anything this build does not recognise. The report used to bucket three
    // outcomes while totalling all of them, so the schema intent's three arrived
    // counted-but-unshown and the percentages silently summed to less than 100
    // with no line to explain the gap. Naming the remainder means the next
    // outcome added cannot go quietly missing the same way.
    let other = total.saturating_sub(searches + schemas);
    let pct = |n: usize| (n as f64) * 100.0 / (total as f64);
    let first = f.iter().map(|x| x.ts).min().unwrap_or(now);
    let last = f.iter().map(|x| x.ts).max().unwrap_or(now);

    s.push_str(&format!(
        "nibdex hook — {total} firing{}, {} → {}\n\n",
        if total == 1 { "" } else { "s" },
        days_ago(first, now),
        days_ago(last, now),
    ));
    s.push_str(&format!("  served    {served:5}  {:5.1}%", pct(served)));
    match median(f.iter().filter(|x| x.outcome == "served").map(|x| x.hits).collect()) {
        Some(m) => s.push_str(&format!("   median {m} hits\n")),
        None => s.push('\n'),
    }
    s.push_str(&format!("  no_hits   {no_hits:5}  {:5.1}%\n", pct(no_hits)));
    s.push_str(&format!("  no_index  {no_index:5}  {:5.1}%", pct(no_index)));
    if no_index > 0 {
        s.push_str("   ← searches in a tree no index covers\n");
    } else {
        s.push('\n');
    }
    if query_error > 0 {
        s.push_str(&format!(
            "  refused   {query_error:5}  {:5.1}%   ← the index could not run the query\n",
            pct(query_error)
        ));
    }
    // The schema intent gets its own block rather than a share of the lines
    // above: it answers a different question ("what shape is this table"), and
    // folding its counts into the search lines would make a healthy SQL workspace
    // read as a mediocre search one. Shown only when it has fired, so the common
    // report stays short.
    if schemas > 0 {
        s.push_str(&format!("\n  schema    {schemas:5}\n"));
        s.push_str(&format!("    served  {sc_served:5}  {:5.1}%", pct(sc_served)));
        match median(f.iter().filter(|x| x.outcome == "schema_served").map(|x| x.hits).collect()) {
            Some(m) => s.push_str(&format!("   median {m} object(s)\n")),
            None => s.push('\n'),
        }
        s.push_str(&format!("    no_hits {sc_no_hits:5}  {:5.1}%", pct(sc_no_hits)));
        if sc_no_hits > 0 {
            s.push_str("   ← SQL naming tables no dump covers\n");
        } else {
            s.push('\n');
        }
        s.push_str(&format!("    no_index{sc_no_index:5}  {:5.1}%\n", pct(sc_no_index)));
    }
    if other > 0 {
        s.push_str(&format!(
            "\n  other     {other:5}  {:5.1}%   ← outcomes this build does not know\n",
            pct(other)
        ));
    }
    s.push_str(&format!(
        "\n  scoped     {} of {searches} search(es)\n  log        {}\n",
        f.iter().filter(|x| x.scoped).count(),
        path.display()
    ));
    if skipped > 0 {
        s.push_str(&format!("  unreadable {skipped} line(s) skipped\n"));
    }
    // Say what the number does NOT mean. The savings ledger carries the same
    // honest limit: an answer offered is not an answer used, and a report that
    // let "served" read as "helped" would be the flattering instrument this
    // whole lane exists to replace.
    s.push_str(
        "\n  `served` means the answer was attached, not that it was used —\n  \
         whether the model acted on it is not observable from here.\n",
    );
    s
}

/// `nibdex hook --stats` — read the firing log and report what the hook has done.
///
/// WHY THIS EXISTS. A hook injection is not a tool call, so every other
/// instrument here is blind to it: `check().adoption` counts tool calls, and the
/// savings ledger only fires when an MCP tool is called. The log was added so the
/// augment path was not invisible — but nothing read it, and a measurement no
/// one can see is the same as no measurement.
pub fn stats() -> ! {
    let Some(path) = log_path() else {
        println!("No $HOME is set, so there is no hook log to read.");
        std::process::exit(0);
    };
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let (firings, skipped) = parse_log(&text);
    let now = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    print!("{}", render_stats(&firings, skipped, now, &path));
    std::process::exit(0);
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
            // The `-e` family FIRST: its value is the pattern, so it has to be
            // read before the skip rules below can swallow it.
            if let Some(p) = pattern_arg_at(&args, i) {
                return match p {
                    PatternArg::Pattern(v, _) => Some(v),
                    PatternArg::Decline => None,
                };
            }
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
            // The `-e` family carries the pattern, so every bare argument after it
            // is a PATH. Without this the first path was consumed as the pattern
            // and the search then ran with no scope at all.
            if let Some(p) = pattern_arg_at(&args, i) {
                match p {
                    PatternArg::Pattern(_, used) => {
                        seen_pattern = true;
                        i += used;
                    }
                    // `pattern_from` declines the whole search here, and a scope
                    // for a question we are not answering means nothing.
                    PatternArg::Decline => return Vec::new(),
                }
                continue;
            }
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
    if !relevant {
        return false;
    }
    if sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM source_chunks")
        .fetch_one(pool)
        .await
        .is_ok_and(|n| n > 0)
    {
        return true;
    }
    // EITHER corpus counts, because the hook has TWO intents. A workspace holding
    // a schema dump and no indexed source is precisely the case the second intent
    // was built for, and demanding `source_chunks` made that workspace
    // undiscoverable — the schema answer worked there only when a
    // `NIBDEX_HOOK_DB` override skipped this check entirely, which is why the
    // documented example needs one and a real DBA tree would not have known to.
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM schema_objects")
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

/// Truncate to at most `max` BYTES, never mid-character.
///
/// `&s[..110]` PANICS when byte 110 lands inside a multi-byte character, and this
/// corpus is full of them — every ⚠️, → and — in a comment is multi-byte, and the
/// hook's own source is written that way. The consequence is worse than a crash
/// report: under the README's documented `2>/dev/null || true` wiring the panic is
/// swallowed, so the answer is simply lost, and because the `served` log line is
/// written AFTER the render, the loss is invisible to `--stats` as well.
///
/// This project's P1 audit concluded every byte-slice site was "char-boundary /
/// bounds / length-guarded". This one was not.
fn truncate_bytes(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
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
            .map(|c| format!("  (via {})", truncate_bytes(c, 7)))
            .unwrap_or_default();
        let body = h.2.trim();
        out.push(format!(
            "    {}:{}: {}{}",
            h.0,
            h.1,
            truncate_bytes(body, 110),
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

/// SQL statements worth reading table names out of. A `SELECT` or an `UPDATE`
/// names tables whose shape the caller has to know; `CREATE`/`ALTER` are the
/// caller telling US the shape, and answering those from a dump that predates
/// their change would be confidently stale.
const SQL_LEAD: [&str; 4] = ["select", "update", "delete", "insert"];

/// A SQL keyword alone is NOT enough to call something a query, and finding that
/// out cost a test: `git commit -m 'select the best option from the list'`
/// matched `from` and extracted the table `the`. Ordinary English contains SQL
/// keywords, and taxing every commit message would be a perverse outcome for a
/// hook whose first rule is to stay free on the common path.
///
/// So a client must be named too. Matched as a substring rather than a leading
/// token, because the real invocation is routinely wrapped —
/// `ssh host "psql -c '…'"` — and the wrapper is not the point.
const SQL_TOOLS: [&str; 7] =
    ["psql", "sqlcmd", "mysql", "mariadb", "sqlite3", "duckdb", "clickhouse-client"];

/// Table names mentioned in a shell command that contains SQL.
///
/// WHY THIS IS A SECOND INTENT AND NOT AN EXTENSION OF THE FIRST. The hook's
/// search classifier asks "where is this string"; this asks "what shape is this
/// table". Measured across 107 sessions in one large workspace, SQL-bearing
/// shell calls were 73% as numerous as search-bearing ones — a stream nearly as
/// large as the one the hook already serves, and served by nothing. Within it,
/// 253 calls did nothing but re-derive a schema and 27 failed on a guessed
/// column name.
///
/// DELIBERATELY CRUDE. This is not a SQL parser and must never become one: it
/// runs on every Bash call and the budget is microseconds. It takes the token
/// after FROM, JOIN, INTO or UPDATE and lets the lookup decide — an unknown
/// name simply misses, which costs one indexed probe that returns nothing. The
/// failure mode of over-matching is a miss; the failure mode of parsing is
/// spending real time on `cargo test`.
fn tables_from_sql(command: &str) -> Vec<String> {
    let lower = command.to_lowercase();
    if !SQL_TOOLS.iter().any(|t| lower.contains(t)) || !SQL_LEAD.iter().any(|k| lower.contains(k)) {
        return Vec::new();
    }
    let mut out: Vec<String> = Vec::new();
    let toks: Vec<&str> = command
        .split(|c: char| c.is_whitespace() || c == '(' || c == ')' || c == ',' || c == ';')
        .filter(|t| !t.is_empty())
        .collect();
    for (i, t) in toks.iter().enumerate() {
        // The keyword can arrive wearing the shell's quote: `-c 'update x …`
        // tokenizes as `'update`, which matched nothing and silently dropped
        // every single-quoted UPDATE. Caught by a test, not in the field.
        let kw = t.trim_matches(|c: char| c == '"' || c == '\'').to_lowercase();
        if !matches!(kw.as_str(), "from" | "join" | "into" | "update") {
            continue;
        }
        let Some(next) = toks.get(i + 1) else { continue };
        // Two passes, and the order matters. The first drops shell punctuation
        // while KEEPING the quoting that is part of a SQL identifier, so
        // `"public"."bids"` survives intact. The second then strips the outer
        // quote a shell argument leaves behind (`from orders"` → `orders`) —
        // without it the trailing quote reaches the log and the dedupe, where
        // `orders` and `orders"` read as two different tables.
        let name = next
            .trim_matches(|c: char| {
                !(c.is_alphanumeric() || c == '_' || c == '.' || c == '[' || c == ']' || c == '"')
            })
            .trim_matches(|c: char| c == '"' || c == '\'');
        // A subquery (`FROM (SELECT`), a placeholder, or a bare number is not a
        // table. So is `information_schema.columns` — answering an introspection
        // query with our own cached introspection would be circular and, worse,
        // would shadow the live answer the caller deliberately went for.
        let low = name.to_lowercase();
        if name.is_empty()
            || low.starts_with("select")
            || low.starts_with("information_schema")
            || low.starts_with("sys.")
            || low.starts_with("pragma")
            || name.chars().next().is_some_and(|c| c.is_numeric())
        {
            continue;
        }
        let name = name.to_string();
        if !out.iter().any(|e| e.eq_ignore_ascii_case(&name)) {
            out.push(name);
        }
        // Two is enough to answer a join without turning an injection into a
        // wall of text. A caller who needs a third can ask.
        if out.len() == 2 {
            break;
        }
    }
    out
}

/// Render the schema answer.
///
/// Labelled and dated for the same reason every other injection is: unlabelled
/// text is indistinguishable from the tool's own output. Dated more emphatically
/// here, because a schema dump is a SNAPSHOT the user took by hand — it can be
/// arbitrarily old, and unlike the code index nothing re-derives it on a commit.
/// A stale schema presented as current is precisely the confidently-wrong answer
/// this corpus exists to prevent.
pub(crate) fn render_schema(hits: &[crate::schema_index::SchemaHit], age: &str) -> Option<String> {
    if hits.is_empty() {
        return None;
    }
    let mut out = vec![format!(
        "[nibdex] schema for {} — from an indexed dump, {}.",
        hits.iter().map(|h| h.name.as_str()).collect::<Vec<_>>().join(", "),
        age
    )];
    // A wide table is the normal case, not a pathological one — the first live
    // firing returned 60 columns — and the whole column list IS the answer, so
    // truncating hard would defeat the purpose. The cap exists for the genuinely
    // pathological table, and it announces what it dropped: a silent cut would
    // let a caller conclude a column does not exist.
    const MAX_LINES: usize = 80;
    for h in hits.iter().take(2) {
        let lines: Vec<&str> = h.body.lines().collect();
        for line in lines.iter().take(MAX_LINES) {
            out.push(format!("  {line}"));
        }
        if lines.len() > MAX_LINES {
            out.push(format!(
                "    …{} more line(s) — ask the database, not this summary",
                lines.len() - MAX_LINES
            ));
        }
    }
    out.push(
        "  (a dump is a snapshot, not a live read — if this disagrees with the \
         database, the database is right)"
            .to_string(),
    );
    Some(out.join("\n"))
}

/// How old the newest schema dump in this index is, in words.
///
/// ⚠️ MEASURED FROM THE DUMP FILE'S MTIME, NOT FROM `indexed_at`, and the
/// difference is the whole point of the number. `indexed_at` is refreshed by
/// `upsert_document` on EVERY indexing pass — the schema step re-upserts
/// unconditionally, since the content-hash skip lives in `source_index` — so it
/// records when nibdex last ran, not when the human last took the dump. A dump
/// generated a month ago and re-indexed this morning reported "indexed today".
///
/// That matters more here than anywhere else in this file. A schema dump is a
/// hand-taken snapshot that NOTHING re-derives on a commit, so its age is the
/// only thing standing between the caller and a confidently-stale answer — and
/// this project's stated mitigation for the un-watched-dump gap is precisely
/// that "every answer states its age". It stated an age that measured the wrong
/// event.
///
/// The dump's own `generated_at` is more authoritative still — it is the
/// database's clock at dump time, it is already parsed, and `persist_dump` then
/// discards it. Storing it needs a migration, so it is the next step, not this
/// one. mtime is honest in every case except a dump COPIED into place, where it
/// reports the copy.
async fn schema_age(pool: &sqlx::SqlitePool) -> String {
    let row: Option<(Option<i64>,)> =
        sqlx::query_as("SELECT MAX(mtime) FROM documents WHERE kind = 'schema'")
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();
    let Some((Some(ts),)) = row else { return "age unknown".to_string() };
    // `upsert_document` stores 0 when the filesystem could not say. Reporting
    // that as a date in 1970 would be worse than admitting we do not know.
    if ts <= 0 {
        return "age unknown".to_string();
    }
    let now = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    match now.saturating_sub(ts) {
        s if s < 86_400 => "taken today".to_string(),
        s if s < 172_800 => "taken yesterday".to_string(),
        s => format!("taken {} days ago", s / 86_400),
    }
}

/// Emit the injection and exit. The one place that writes to stdout.
fn emit(text: &str) -> ! {
    println!(
        "{}",
        json!({"hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "allow",
            "additionalContext": text}})
    );
    std::process::exit(0);
}

/// The SQL intent: answer "what shape is this table" from the indexed dump.
///
/// Reached only when the command is NOT a usable search, so it can never
/// displace the search answer — the two intents do not compete for one call.
/// Every path exits 0 (rule 1).
async fn answer_schema_or_exit(command: &str, cwd: &Path) -> ! {
    let tables = tables_from_sql(command);
    if tables.is_empty() {
        // The fast reject, and it is the common case: most Bash calls are
        // neither a search nor SQL. Not logged, for the same reason the search
        // reject is not — it fires on nearly every call and would swamp the log.
        allow_silently();
    }
    let term = tables.join(",");
    let Some((db, pool)) = find_db(cwd).await else {
        log_firing("schema_no_index", &term, false, 0, None);
        allow_silently();
    };
    let mut hits = Vec::new();
    for t in &tables {
        match crate::schema_index::lookup_object(&pool, t).await {
            Ok(mut h) => hits.append(&mut h),
            // Fail open per table, not per call: one unreadable lookup must not
            // cost the caller the other table's answer.
            Err(_) => continue,
        }
    }
    let age = schema_age(&pool).await;
    pool.close().await;

    match render_schema(&hits, &age) {
        Some(text) => {
            log_firing("schema_served", &term, false, hits.len(), Some(&db));
            emit(&text);
        }
        None => {
            // SQL against tables this index has never heard of. Distinguishing
            // it from "no index at all" is the same distinction `corpus_empty`
            // makes on the query side, and it is the number that will say
            // whether the dump covers the databases actually in use.
            log_firing("schema_no_hits", &term, false, 0, Some(&db));
            allow_silently();
        }
    }
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
    let cwd = ev
        .get("cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));

    // Rule 2: reject the common case before touching the filesystem.
    let pat = if tool == "Grep" {
        input
            .get("pattern")
            .and_then(Value::as_str)
            .map(str::to_string)
    } else {
        let cmd = input.get("command").and_then(Value::as_str).unwrap_or("");
        let p = if SEARCH_TOOLS.iter().any(|t| cmd.contains(t)) {
            pattern_from(cmd)
        } else {
            None
        };
        // SECOND INTENT. Falling through on "not a usable search" rather than
        // on "contains no search tool" matters: `psql -c "select …" | grep x`
        // holds both, and `pattern_from` correctly declines it because grep is
        // trimming output rather than asking the index anything. Exiting there
        // would drop a SQL call that the schema corpus can answer. Never
        // returns.
        match p.as_deref().filter(|s| term_is_indexable(s)) {
            Some(_) => p,
            None => answer_schema_or_exit(cmd, &cwd).await,
        }
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
    let Ok(hits) = hits else {
        // The ONE terminal state that logged nothing, and that is how a whole
        // class of failure stayed invisible: the index refused the query, the
        // fail-open swallowed the error, and every instrument here counts firings
        // from this log — so the failure could not be seen, counted, or believed.
        // Still fails open. Now it is counted.
        log_firing("query_error", &pat, !scope.is_empty(), 0, Some(&db));
        allow_silently()
    };

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

    /// `-e` carries the pattern; it is not a flag whose value gets skipped.
    ///
    /// The mutation that proves this test: put `-e` back in `VALUED_FLAGS` and
    /// every assertion below returns the PATH — which is what shipped, and what
    /// made the hook inject 25 confident hits for `"src"` against a search for
    /// something else entirely.
    #[test]
    fn the_e_family_carries_the_pattern_not_the_path() {
        for cmd in [
            "grep -e needle src/",
            "rg --regexp needle src/",
            "grep --regexp=needle src/",
            "grep -eneedle src/",
            "grep -rne needle src/",
        ] {
            assert_eq!(pattern_from(cmd).as_deref(), Some("needle"), "pattern of: {cmd}");
        }
        // ...and the path is then a SCOPE. Before, it was consumed as the pattern,
        // so the search was answered for the wrong term AND left unscoped.
        assert_eq!(paths_from("grep -e needle src/"), vec!["src"]);
        assert_eq!(paths_from("grep -rne needle src/ tests/"), vec!["src", "tests"]);
        // The ordinary forms must not regress.
        assert_eq!(pattern_from("grep -rn needle src/").as_deref(), Some("needle"));
        assert_eq!(pattern_from("grep -A 5 needle f.rs").as_deref(), Some("needle"));
    }

    /// When the pattern is NOT in the command, decline. `-f` reads patterns from a
    /// file we are not going to open, and a bare `-e` is malformed — guessing at
    /// either produces an answer to a question the caller never asked, which is
    /// strictly worse than staying silent.
    #[test]
    fn a_pattern_we_cannot_know_is_declined_not_guessed() {
        assert_eq!(pattern_from("grep -f patterns.txt src/"), None);
        assert_eq!(pattern_from("grep --file=patterns.txt src/"), None);
        assert_eq!(pattern_from("grep -e"), None);
        assert!(paths_from("grep -f patterns.txt src/").is_empty());
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

    /// A long line holding a multi-byte character must RENDER, not panic.
    ///
    /// The mutation that proves it: restore `truncate_bytes(body, 110)` and
    /// this panics on a byte index inside the arrow. In the field that panic is
    /// swallowed by the documented `2>/dev/null || true` wiring, so the answer is
    /// lost silently — and since the `served` line is logged after the render, the
    /// loss never reaches `--stats` either.
    #[test]
    fn a_long_non_ascii_line_renders_instead_of_panicking() {
        // 108 ASCII bytes, then a 3-byte arrow occupying 108..111 — so the old
        // cut at byte 110 lands inside it.
        let body = format!("{}→ and more text past the boundary", "x".repeat(108));
        assert!(!body.is_char_boundary(110), "fixture must straddle the cut");
        let hits = vec![("a/x.rs".to_string(), 4, body, None)];
        let out = render("needle", &hits, "index current", "/x/nibdex.db").expect("must render");
        assert!(out.contains("a/x.rs:4"), "{out}");
        // A short line is untouched, and a non-ASCII sha prefix cannot panic either.
        assert_eq!(truncate_bytes("short", 110), "short");
        assert_eq!(truncate_bytes("→→→", 4), "→");
    }

    fn firing(ts: u64, outcome: &str, hits: usize, scoped: bool) -> Firing {
        Firing { ts, outcome: outcome.to_string(), hits, scoped, db: "/x/nibdex.db".into() }
    }

    /// The scoping that keeps a machine-global log out of a workspace-scoped
    /// instrument. Without it, `check().adoption` on one workspace would count
    /// deliveries made to another — the bug rc.2 fixed, reappearing by a new
    /// route.
    #[test]
    fn same_db_rejects_a_different_database_and_an_empty_field() {
        let dir = std::env::temp_dir();
        let mine = dir.join("nibdex-samedb-mine.db");
        let theirs = dir.join("nibdex-samedb-theirs.db");
        std::fs::write(&mine, b"x").unwrap();
        std::fs::write(&theirs, b"x").unwrap();

        assert!(same_db(&mine.display().to_string(), &mine));
        assert!(!same_db(&theirs.display().to_string(), &mine), "another workspace must not count");
        // A `no_index` firing records an empty db; it belongs to no index and
        // must never be attributed to the one asking.
        assert!(!same_db("", &mine));
        // Unresolvable paths fall back to string equality rather than matching
        // everything — failing open here would inflate every workspace's count.
        assert!(!same_db("/nope/a.db", Path::new("/nope/b.db")));
        assert!(same_db("/nope/a.db", Path::new("/nope/a.db")));

        let _ = std::fs::remove_file(mine);
        let _ = std::fs::remove_file(theirs);
    }

    /// A malformed line must be COUNTED as unreadable, never silently dropped and
    /// never parsed into a firing. A log that quietly loses lines reports a
    /// smaller, healthier-looking number than the truth — the flattering-
    /// instrument failure this whole lane exists to correct.
    #[test]
    fn parse_log_skips_malformed_lines_without_absorbing_them() {
        let text = concat!(
            r#"{"ts":100,"outcome":"served","hits":9,"scoped":true}"#,
            "\n",
            "not json at all\n",
            "\n",
            r#"{"outcome":"served","hits":3}"#, // no ts
            "\n",
            r#"{"ts":200,"outcome":"no_index","hits":0,"scoped":false}"#,
            "\n",
        );
        let (f, skipped) = parse_log(text);
        assert_eq!(f.len(), 2, "only the two well-formed lines are firings");
        assert_eq!(skipped, 2, "the junk line and the ts-less line are both counted");
        assert_eq!(f[0].hits, 9);
        assert_eq!(f[1].outcome, "no_index");
    }

    /// The empty case is the one that matters most in the field: an unwired hook
    /// and a hook that never found anything to answer look IDENTICAL from here,
    /// so the report must name both rather than implying the tool is idle.
    #[test]
    fn stats_on_an_empty_log_names_both_causes() {
        let out = render_stats(&[], 0, 1_000, Path::new("/h/.nibdex/hook-log.jsonl"));
        assert!(out.contains("no firings recorded"), "{out}");
        assert!(out.contains("never been wired"), "must offer the wiring cause: {out}");
        assert!(out.contains("never seen a search"), "must offer the quiet cause: {out}");
        assert!(out.contains("/h/.nibdex/hook-log.jsonl"), "must say where it looked: {out}");
    }

    /// The split and the median are the whole report. The median is taken over
    /// SERVED firings only — including the zero-hit rows would drag it toward 0
    /// and make a healthy hook look useless.
    #[test]
    fn stats_counts_outcomes_and_medians_only_the_served() {
        let f = vec![
            firing(10, "served", 4, true),
            firing(20, "served", 20, false),
            firing(30, "served", 12, false),
            firing(40, "no_hits", 0, true),
            firing(50, "no_index", 0, false),
        ];
        let out = render_stats(&f, 0, 60, Path::new("/x/log.jsonl"));
        assert!(out.contains("5 firings"), "{out}");
        assert!(out.contains("median 12 hits"), "median of 4/12/20, not of all five: {out}");
        assert!(out.contains("60.0%"), "3 of 5 served: {out}");
        assert!(out.contains("scoped     2 of 5"), "{out}");
        assert!(
            out.contains("← searches in a tree no index covers"),
            "a no_index firing must be called out, not left as a bare number: {out}"
        );
        // The honest limit travels with the number or the number overstates.
        assert!(
            out.contains("not that it was used"),
            "must not let `served` read as `helped`: {out}"
        );
    }

    /// The schema intent has to APPEAR. Every firing counts toward the total, so
    /// bucketing only the three search outcomes made the percentages sum to less
    /// than 100 with no line saying why — and a workspace whose SQL calls all hit
    /// read as one where the hook does nothing.
    #[test]
    fn stats_reports_the_schema_intent_and_names_what_it_cannot_bucket() {
        let f = vec![
            firing(10, "served", 4, true),
            firing(20, "schema_served", 2, false),
            firing(30, "schema_served", 6, false),
            firing(40, "schema_no_hits", 0, false),
            firing(50, "query_error", 0, false),
            firing(60, "an_outcome_from_the_future", 0, false),
        ];
        let out = render_stats(&f, 0, 70, Path::new("/x/log.jsonl"));
        assert!(out.contains("median 6 object(s)"), "schema served needs its own median: {out}");
        assert!(
            out.contains("← SQL naming tables no dump covers"),
            "a schema miss is the number that says whether the dump covers the databases in use: {out}"
        );
        assert!(
            out.contains("← the index could not run the query"),
            "a refused query must be visible, not merely absent: {out}"
        );
        assert!(
            out.contains("outcomes this build does not know"),
            "an unrecognised outcome must be NAMED, not quietly dropped from the buckets: {out}"
        );
        // The scoped denominator is searches, not every firing — schema firings
        // are never scoped, so counting them would understate it forever.
        assert!(out.contains("scoped     1 of 2 search(es)"), "{out}");
    }

    /// A workspace with a schema dump and NO indexed source must still be
    /// discoverable — that is the DBA case the second intent exists for, and the
    /// guard demanded `source_chunks`, so the schema answer only ever worked
    /// behind a `NIBDEX_HOOK_DB` override.
    #[tokio::test]
    async fn a_schema_only_index_can_still_answer() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let pool = crate::db::open(&tmp.path().join("nibdex.db")).await.unwrap();
        sqlx::query(
            "INSERT INTO indexed_repos (repo_path, last_indexed_oid, last_indexed_at) \
             VALUES (?, '', 0)",
        )
        .bind(cwd.display().to_string())
        .execute(&pool)
        .await
        .unwrap();

        // Relevant, but every corpus empty: not usable, and that must stay true.
        assert!(!db_indexes(&pool, &cwd).await, "an empty index must not shadow a real one");

        let doc: (i64,) = sqlx::query_as(
            "INSERT INTO documents (path, kind, content_hash, mtime, indexed_at) \
             VALUES ('/w/db.nibdex-schema.json', 'schema', 'h', 1, 1) RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO schema_objects \
             (document_id, database_name, schema_name, object_name, object_type, \
              columns_json, body) \
             VALUES (?, 'shopdb', 'public', 'orders', 'table', '[]', 'shopdb.public.orders')",
        )
        .bind(doc.0)
        .execute(&pool)
        .await
        .unwrap();

        assert!(
            db_indexes(&pool, &cwd).await,
            "a dump with no indexed source is the whole point of the schema intent"
        );
    }

    /// A schema answer IS a delivery. Counting only `served` meant a workspace
    /// answering on every `psql` call still reported `hook_deliveries: 0`, which
    /// reads as "nibdex is not being used" when the opposite is true.
    #[test]
    fn a_schema_answer_counts_as_a_delivery() {
        assert!(is_delivery("served"));
        assert!(is_delivery("schema_served"));
        assert!(!is_delivery("no_hits"));
        assert!(!is_delivery("schema_no_hits"));
        assert!(!is_delivery("query_error"));
    }

    /// A hostile character must still produce ONE parseable line. The hand-rolled
    /// writer made the log look broken when the path was merely unusual.
    #[test]
    fn a_firing_with_hostile_characters_still_round_trips() {
        let db = PathBuf::from("/tmp/we\"ird\\path/nibdex.db");
        let line = log_line(7, "served", "a\"b\\c\nd", true, 3, Some(&db));
        assert_eq!(line.lines().count(), 1, "must be exactly one line: {line}");
        let (f, skipped) = parse_log(&line);
        assert_eq!(skipped, 0, "must parse as JSON: {line}");
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].hits, 3);
        assert_eq!(f[0].db, db.display().to_string(), "the path must survive verbatim");
    }

    /// `no_index` is the actionable outcome — it means the caller asked something
    /// answerable and nothing covered their tree. When there are none, the report
    /// must NOT print the pointer, or it reads as a standing complaint.
    #[test]
    fn stats_omits_the_no_index_pointer_when_there_are_none() {
        let f = vec![firing(10, "served", 4, false)];
        let out = render_stats(&f, 0, 20, Path::new("/x/log.jsonl"));
        assert!(!out.contains("no index covers"), "{out}");
    }

    /// Unreadable lines must surface. Reporting only the survivors is how a
    /// broken log passes as a quiet one.
    #[test]
    fn stats_surfaces_unreadable_lines() {
        let f = vec![firing(10, "served", 4, false)];
        let out = render_stats(&f, 3, 20, Path::new("/x/log.jsonl"));
        assert!(out.contains("unreadable 3 line(s) skipped"), "{out}");
    }

    /// Rule 2 for the SECOND intent, and it carries the same weight as the
    /// first: this runs on every Bash call. A build, a git command or a test run
    /// must produce no tables and therefore no I/O at all.
    #[test]
    fn non_sql_commands_yield_no_tables() {
        for cmd in [
            "cargo test",
            "git commit -m 'select the best option from the list'",
            "ls -la",
            "npm run build",
            // The word appears, but as prose in a message — no FROM/JOIN/INTO
            // follows, so nothing is extracted.
            "echo 'insert your name here'",
        ] {
            assert!(tables_from_sql(cmd).is_empty(), "must not fire on: {cmd}");
        }
    }

    /// The shapes that actually appear in a shell: a quoted -c argument, a
    /// schema-qualified name, a join, and bracket quoting.
    #[test]
    fn sql_commands_yield_their_tables() {
        assert_eq!(tables_from_sql("psql -c \"select * from orders\""), vec!["orders"]);
        assert_eq!(
            tables_from_sql("psql -c 'select o.id from sales.orders o join customers c on 1=1'"),
            vec!["sales.orders", "customers"],
            "a join names both, and the namespace is kept"
        );
        assert_eq!(
            tables_from_sql("sqlcmd -Q \"SELECT TOP 1 * FROM [dbo].[Employees]\""),
            vec!["[dbo].[Employees]"]
        );
        assert_eq!(tables_from_sql("psql -c 'update orders set x=1'"), vec!["orders"]);
        assert_eq!(tables_from_sql("psql -c 'insert into orders values (1)'"), vec!["orders"]);
    }

    /// Introspection must NOT be answered from our own cached introspection.
    /// It is circular, and worse, it would shadow the live answer a caller
    /// deliberately went to the database for.
    #[test]
    fn introspection_queries_are_left_alone() {
        assert!(tables_from_sql("psql -c \"select * from information_schema.columns\"").is_empty());
        assert!(tables_from_sql("sqlcmd -Q \"select * from sys.columns\"").is_empty());
    }

    /// A subquery is not a table, and neither is a number. Both would otherwise
    /// become a lookup for a name that cannot exist.
    #[test]
    fn subqueries_and_junk_are_not_tables() {
        let t = tables_from_sql("psql -c 'select * from (select 1) x join real_table r on 1=1'");
        assert!(!t.iter().any(|s| s.to_lowercase().starts_with("select")), "{t:?}");
        assert!(t.contains(&"real_table".to_string()), "{t:?}");
    }

    /// Two is the cap. An injection that grows with the join count stops being
    /// an answer and becomes a wall of text the reader learns to skip.
    #[test]
    fn at_most_two_tables_are_answered() {
        let t = tables_from_sql("psql -c 'select 1 from a join b on 1=1 join c on 1=1 join d'");
        assert_eq!(t.len(), 2, "{t:?}");
    }

    /// The rendering must date itself. A schema dump is a hand-taken snapshot
    /// with nothing re-deriving it on a commit, so presenting it as current is
    /// the confidently-wrong answer this corpus exists to prevent.
    #[test]
    fn schema_rendering_is_labelled_dated_and_defers_to_the_database() {
        let hits = vec![crate::schema_index::SchemaHit {
            database: "shopdb".into(),
            schema: "public".into(),
            name: "orders".into(),
            object_type: "table".into(),
            body: "shopdb.public.orders (table)\n  id integer NOT NULL\n".into(),
        }];
        let out = render_schema(&hits, "taken 3 days ago").expect("hits render");
        assert!(out.contains("[nibdex]"), "must be attributable: {out}");
        // "taken", not "indexed": the age must describe when the HUMAN dumped the
        // schema, not when nibdex last walked the workspace. The old wording was
        // true and measured the wrong event.
        assert!(out.contains("taken 3 days ago"), "must state its age: {out}");
        assert!(out.contains("id integer NOT NULL"), "{out}");
        assert!(
            out.contains("the database is right"),
            "must defer to the live source on disagreement: {out}"
        );
        // Nothing to say, nothing injected.
        assert!(render_schema(&[], "indexed today").is_none());
    }

    /// An empty sample must not report a median of zero — "no served firings" and
    /// "served firings that returned nothing" are different facts.
    #[test]
    fn median_of_nothing_is_none_not_zero() {
        assert_eq!(median(vec![]), None);
        assert_eq!(median(vec![5]), Some(5));
        assert_eq!(median(vec![20, 4, 12]), Some(12), "must sort before taking the middle");
    }
}
