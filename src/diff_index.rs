// SPDX-License-Identifier: MIT

//! D1 gear 5 — the diff/content-tracing provenance probe (SPIKE; docs/D1_SCOPE.md §10).
//!
//! The gear-4 eyeball proved file-last-touch provenance breaks on refactors: a
//! `find_code` hit on the OR-broaden fn resolved to `refactor: extract fts5.rs`
//! (the *mover*), not the `D-10.13` commit that *authored* it. Line-blame fails
//! identically (a move IS the last line-touch). The fix (§10 "Reframe"): stop
//! stamping a static label — **process the commit's content**.
//!
//! This module indexes the **added lines of every commit's diff** as the join
//! atom (`{file, line, code, message}` natively bound). A moved line is an
//! ADDITION in both the author's commit and the mover's, so a content query
//! matches both — and the **oldest** matching change (min `authored_at`) is the
//! true author, reached through every mechanical move. That's `git log -S`
//! pickaxe semantics, precomputed into an index instead of probed per query.
//!
//! SCRAPPY ON PURPOSE: shells out to `git log -p` (like source_index.rs), drives
//! off CLI subcommands, perturbs nothing in indexer.rs. Run `nibdex index` first
//! on the same `--db` so `commit_entries` exists for the provenance FK to resolve.
//!
//! Distinct artifact from `source_chunks` (D1_SCOPE §10 split): source_chunks =
//! the searchable code SNAPSHOT; diff_hunks = the change-map for PROVENANCE.

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use sqlx::SqlitePool;

/// Skip an added block larger than this (defend against pathological diffs, e.g.
/// a vendored blob or generated file landing in one commit).
const MAX_BLOCK_BYTES: usize = 64 * 1024;
/// Field separator inside the commit-marker line (0x1F unit separator — can't
/// appear in a hash/epoch and is vanishingly unlikely in a subject line).
const FS: char = '\u{1f}';

#[derive(Debug, Default)]
pub struct DiffIndexStats {
    pub commits_seen: usize,
    pub hunks_indexed: usize,
    pub blocks_skipped_large: usize,
    /// Added blocks whose commit resolved to a `commit_entries.id`.
    pub provenance_hits: usize,
    /// Added blocks whose commit was not in the index.
    pub provenance_misses: usize,
    pub elapsed_ms: u128,
}

/// One contiguous run of `+` lines within a hunk — the spike's unit of "change".
struct AddedBlock {
    file_path: String,
    new_start: i64,
    body: String,
}

pub async fn index_diffs(pool: &SqlitePool, repo: &Path) -> Result<DiffIndexStats> {
    let started = Instant::now();
    let mut stats = DiffIndexStats::default();

    // Idempotent re-runs: wipe the spike's prior rows + their FTS entries first.
    wipe_all_diff_hunks(pool).await?;

    // One `git log -p` pass over the whole history. `--unified=0` strips context
    // (we only want `+` lines); `--no-renames` makes a rename show as delete+add
    // so the moved content appears as additions oldest-wins can resolve.
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "log",
            "-p",
            "--unified=0",
            "--no-renames",
            "--no-color",
            &format!("--format=__C__%H{FS}%at{FS}%s"),
        ])
        .output()
        .with_context(|| format!("running `git log -p` in {repo:?}"))?;
    if !out.status.success() {
        bail!(
            "`git log -p` failed in {repo:?}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let text = String::from_utf8_lossy(&out.stdout);

    // commit_hash → commit_entries.id, memoized (one lookup per commit at most).
    let mut commit_id_cache: HashMap<String, Option<i64>> = HashMap::new();

    // Parser state.
    let mut cur_hash: Option<String> = None;
    let mut cur_at: i64 = 0;
    let mut cur_summary = String::new();
    let mut cur_file: Option<String> = None;
    let mut new_line: i64 = 0; // running new-file line number inside a hunk
    let mut block: Option<AddedBlock> = None;

    for line in text.lines() {
        // --- commit marker -------------------------------------------------
        if let Some(rest) = line.strip_prefix("__C__") {
            flush_block(pool, &mut stats, &mut commit_id_cache, &cur_hash, cur_at, &cur_summary, &mut block).await?;
            let mut parts = rest.splitn(3, FS);
            cur_hash = parts.next().map(|s| s.to_string());
            cur_at = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            cur_summary = parts.next().unwrap_or("").to_string();
            cur_file = None;
            stats.commits_seen += 1;
            continue;
        }
        // --- file header (new-side path) -----------------------------------
        if let Some(p) = line.strip_prefix("+++ ") {
            flush_block(pool, &mut stats, &mut commit_id_cache, &cur_hash, cur_at, &cur_summary, &mut block).await?;
            cur_file = if p == "/dev/null" {
                None
            } else {
                // Strip the `b/` prefix git adds; tolerate its absence.
                Some(p.strip_prefix("b/").unwrap_or(p).to_string())
            };
            continue;
        }
        if line.starts_with("--- ") || line.starts_with("diff --git ") {
            // Old-side header / file boundary — flush, file set by the +++ line.
            flush_block(pool, &mut stats, &mut commit_id_cache, &cur_hash, cur_at, &cur_summary, &mut block).await?;
            continue;
        }
        // --- hunk header: @@ -a,b +c,d @@ ----------------------------------
        if let Some(c) = parse_hunk_new_start(line) {
            flush_block(pool, &mut stats, &mut commit_id_cache, &cur_hash, cur_at, &cur_summary, &mut block).await?;
            new_line = c;
            continue;
        }
        // --- added line ----------------------------------------------------
        if let Some(content) = line.strip_prefix('+') {
            // (`+++` already handled above.)
            if let Some(ref file) = cur_file {
                match block {
                    Some(ref mut b) => b.body.push_str(content),
                    None => {
                        block = Some(AddedBlock {
                            file_path: file.clone(),
                            new_start: new_line,
                            body: content.to_string(),
                        });
                    }
                }
                if let Some(ref mut b) = block {
                    b.body.push('\n');
                }
            }
            new_line += 1;
            continue;
        }
        // --- removed line: ignore, new-line unchanged ----------------------
        if line.starts_with('-') {
            flush_block(pool, &mut stats, &mut commit_id_cache, &cur_hash, cur_at, &cur_summary, &mut block).await?;
            continue;
        }
        // Any other line (context with --unified=0 shouldn't occur, blank, etc.)
        // ends the current added run.
        flush_block(pool, &mut stats, &mut commit_id_cache, &cur_hash, cur_at, &cur_summary, &mut block).await?;
    }
    // Trailing block at EOF.
    flush_block(pool, &mut stats, &mut commit_id_cache, &cur_hash, cur_at, &cur_summary, &mut block).await?;

    stats.elapsed_ms = started.elapsed().as_millis();
    Ok(stats)
}

/// Persist one accumulated added block (if any) as a `diff_hunks` row + its FTS
/// row, then clear it. Whitespace-only / oversize blocks are dropped.
#[allow(clippy::too_many_arguments)]
async fn flush_block(
    pool: &SqlitePool,
    stats: &mut DiffIndexStats,
    cache: &mut HashMap<String, Option<i64>>,
    hash: &Option<String>,
    authored_at: i64,
    summary: &str,
    block: &mut Option<AddedBlock>,
) -> Result<()> {
    let Some(b) = block.take() else { return Ok(()) };
    let Some(hash) = hash else { return Ok(()) };
    if b.body.trim().is_empty() {
        return Ok(());
    }
    if b.body.len() > MAX_BLOCK_BYTES {
        stats.blocks_skipped_large += 1;
        return Ok(());
    }
    let commit_id = resolve_commit_id(pool, cache, hash).await?;
    if commit_id.is_some() {
        stats.provenance_hits += 1;
    } else {
        stats.provenance_misses += 1;
    }
    let row: (i64,) = sqlx::query_as(
        r#"
        INSERT INTO diff_hunks
            (commit_id, commit_hash, authored_at, summary, file_path, new_start, added_body)
        VALUES (?, ?, ?, ?, ?, ?, ?)
        RETURNING id
        "#,
    )
    .bind(commit_id)
    .bind(hash)
    .bind(authored_at)
    .bind(summary)
    .bind(&b.file_path)
    .bind(b.new_start)
    .bind(&b.body)
    .fetch_one(pool)
    .await?;
    sqlx::query(
        "INSERT INTO search_index (body, kind, rowid_ref, source_table) \
         VALUES (?, 'diff', ?, 'diff_hunks')",
    )
    .bind(&b.body)
    .bind(row.0)
    .execute(pool)
    .await?;
    stats.hunks_indexed += 1;
    Ok(())
}

/// `@@ -a,b +c,d @@` → the new-file start line `c`. None if not a hunk header.
fn parse_hunk_new_start(line: &str) -> Option<i64> {
    let rest = line.strip_prefix("@@ ")?;
    // Find the `+c[,d]` token.
    let plus = rest.split_whitespace().find(|t| t.starts_with('+'))?;
    let num = plus.trim_start_matches('+');
    let num = num.split(',').next().unwrap_or(num);
    num.parse().ok()
}

async fn resolve_commit_id(
    pool: &SqlitePool,
    cache: &mut HashMap<String, Option<i64>>,
    hash: &str,
) -> Result<Option<i64>> {
    if let Some(hit) = cache.get(hash) {
        return Ok(*hit);
    }
    let id: Option<i64> =
        sqlx::query_scalar("SELECT id FROM commit_entries WHERE commit_hash = ? LIMIT 1")
            .bind(hash)
            .fetch_optional(pool)
            .await?;
    cache.insert(hash.to_string(), id);
    Ok(id)
}

async fn wipe_all_diff_hunks(pool: &SqlitePool) -> Result<()> {
    sqlx::query("DELETE FROM search_index WHERE source_table = 'diff_hunks'")
        .execute(pool)
        .await?;
    sqlx::query("DELETE FROM diff_hunks").execute(pool).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// The eyeball — `trace-code` (D1_SCOPE §10 next-probe). FTS5 MATCH over the
// change-map, results ordered OLDEST-FIRST so the authoring commit surfaces at
// the top (vs find_code's snapshot last-touch). The probe's deliverable:
// does `broadened` now resolve to `D-10.13 OR-fallback`, not `refactor`?
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct DiffHit {
    pub commit_hash: String,
    pub authored_at: i64,
    pub summary: String,
    pub file_path: String,
    pub new_start: i64,
    pub rank: f64,
    pub body: String,
}

/// Return matching added-blocks ordered by `authored_at ASC` (oldest = authoring
/// candidate first), capped at `limit`. SPIKE: raw query bound to MATCH (no
/// sanitize / OR-broaden — productionizing reuses `mcp::fts5`).
pub async fn trace_code(pool: &SqlitePool, query: &str, limit: i64) -> Result<Vec<DiffHit>> {
    type Row = (String, i64, String, String, i64, f64, String);
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT dh.commit_hash, dh.authored_at, dh.summary, dh.file_path, \
                dh.new_start, bm25(search_index) AS rank, dh.added_body \
         FROM search_index s \
         JOIN diff_hunks dh ON dh.id = s.rowid_ref AND s.source_table = 'diff_hunks' \
         WHERE s.body MATCH ? \
         ORDER BY dh.authored_at ASC \
         LIMIT ?",
    )
    .bind(query)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| DiffHit {
            commit_hash: r.0,
            authored_at: r.1,
            summary: r.2,
            file_path: r.3,
            new_start: r.4,
            rank: r.5,
            body: r.6,
        })
        .collect())
}

// ---------------------------------------------------------------------------
// Gear 6 — the LAYER-AWARE resolver (D1_SCOPE.md §10 "Gear-5" finding 2).
//
// Gear 5 proved oldest-across-ALL-files is wrong: doc prose that *names* a term
// predates the code that *implements* it, so naive oldest-wins returns a design
// doc, not the author. The fix is layer-aware:
//   - author  = the OLDEST **code**-layer commit introducing matching content
//               (reaches the true author through mechanical moves);
//   - feeders = the non-code (prose) matches — design / session / memory — each
//               carrying its commit + the **author↔feeder time-delta**, which is
//               the first concrete §7 thread signal (feeder older than code =
//               design-discussed-then-built = a real through-line; newer = doc lag).
// Dedupe by commit throughout (a commit recurs once per added block).
// ---------------------------------------------------------------------------

/// Which knowledge layer a changed file belongs to. Heuristic + environment-
/// specific (path conventions) on purpose — the load-bearing split is code vs
/// prose; the finer session/design/memory partition is best-effort (D1_SCOPE §10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layer {
    Code,
    Design,
    Session,
    Memory,
    Config,
    Other,
}

impl Layer {
    pub fn label(self) -> &'static str {
        match self {
            Layer::Code => "code",
            Layer::Design => "design",
            Layer::Session => "session",
            Layer::Memory => "memory",
            Layer::Config => "config",
            Layer::Other => "other",
        }
    }
    /// Prose feeders are the §7 feeder layers (everything that *describes* code).
    pub fn is_feeder(self) -> bool {
        matches!(self, Layer::Design | Layer::Session | Layer::Memory)
    }
}

/// Classify a repo-relative path into a layer. Code = programming languages (the
/// authoring layer); CLAUDE.md/MEMORY.md + memory dir = session/memory by
/// convention; markdown/text = design; build/config files = config.
pub fn layer_of(path: &str) -> Layer {
    let base = path.rsplit('/').next().unwrap_or(path);
    if base == "CLAUDE.md" {
        return Layer::Session;
    }
    if base == "MEMORY.md" || path.contains("/memory/") {
        return Layer::Memory;
    }
    let ext = path.rsplit('.').next().unwrap_or("");
    match ext {
        "rs" | "py" | "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" | "go" | "c" | "cc" | "cpp"
        | "h" | "hpp" | "java" | "rb" | "kt" | "swift" | "scala" | "sh" | "bash" => Layer::Code,
        "toml" | "yaml" | "yml" | "json" | "sql" | "lock" | "cfg" | "ini" => Layer::Config,
        "md" | "markdown" | "txt" | "rst" | "adoc" => Layer::Design,
        _ => Layer::Other,
    }
}

/// A commit that introduced matching content, deduped (best rank kept).
#[derive(Debug, Clone)]
pub struct CommitRef {
    pub commit_hash: String,
    pub authored_at: i64,
    pub summary: String,
    pub file_path: String, // representative (best-rank) matched file
    pub new_start: i64,
    pub rank: f64,
}

/// A prose feeder + its time relation to the code author.
#[derive(Debug, Clone)]
pub struct Feeder {
    pub layer: Layer,
    pub commit: CommitRef,
    /// feeder.authored_at − author.authored_at. <0 = feeder predates code
    /// (design-first through-line); >0 = doc lag; None when there's no author.
    pub delta_secs: Option<i64>,
}

/// A session edit (gear 7) matching the query — the SHARPEST feeder the corpus
/// has: the actual authoring *moment* (an `Edit`/`Write` tool-call) with its
/// rationale, bound to the commit that captured it. Distinct from a prose
/// `Feeder` (a doc *commit* describing code): this is the transcript edge, so its
/// Δ is between the **edit** and the authoring commit — typically minutes, the
/// closest-possible through-line (D1_SCOPE §10 "Gear-7" → "fold as a feeder layer").
#[derive(Debug, Clone)]
pub struct SessionFeeder {
    pub session_id: String,
    pub tool: String,
    pub file_path: String,
    pub rationale: String,
    pub edited_at: i64,
    /// `layer_of(file_path)` — usually Code (the edit touched code), but a session
    /// can also edit docs/memory; kept for parity with the prose feeder display.
    pub layer: Layer,
    /// The capturing commit (the next commit that committed the edit), when bound.
    pub commit_hash: Option<String>,
    pub commit_summary: Option<String>,
    pub rank: f64,
    /// `edited_at − author.authored_at`. <0 = the edit predates the authoring
    /// commit (the closest-possible feeder); None when there's no code author.
    pub delta_secs: Option<i64>,
}

/// One FTS-matched transcript edge feeding the pure session-feeder resolver.
#[derive(Debug, Clone)]
pub struct SessionMatch {
    pub session_id: String,
    pub tool: String,
    pub file_path: String,
    pub rationale: String,
    pub edited_at: i64,
    pub commit_hash: Option<String>,
    pub commit_summary: Option<String>,
    pub rank: f64,
}

#[derive(Debug)]
pub struct ProvenanceResolution {
    pub query: String,
    /// Oldest code-layer commit = the authoring candidate (None = prose-only term).
    pub author: Option<CommitRef>,
    /// Later code commits also touching matching content (the movers/relateds).
    pub other_code: Vec<CommitRef>,
    /// Prose feeders (design/session/memory), each with its Δ to the author.
    pub feeders: Vec<Feeder>,
    /// Session-derived feeders (gear 7): `Edit`/`Write` transcript edges matching
    /// the query, deduped by (session, file), each with its Δ to the code author.
    pub session_feeders: Vec<SessionFeeder>,
    /// Config/other matches, deduped — shown but not author- or feeder-eligible.
    pub other: Vec<CommitRef>,
    pub code_commit_count: usize,
    pub feeders_predating_author: usize,
    pub session_feeders_predating_author: usize,
}

/// One classified match row feeding the pure resolver.
#[derive(Debug, Clone)]
pub struct Match {
    pub layer: Layer,
    pub commit: CommitRef,
}

/// PURE resolution rule (DB-free, unit-tested): dedupe by commit, partition by
/// layer, oldest code commit = author, prose = feeders with Δ, rest = other.
/// `matches` may arrive in any order.
pub fn resolve_from_matches(query: &str, matches: Vec<Match>) -> ProvenanceResolution {
    // Dedupe by commit, keeping the best (lowest bm25) rank as representative.
    let mut by_commit: HashMap<String, Match> = HashMap::new();
    for m in matches {
        by_commit
            .entry(m.commit.commit_hash.clone())
            .and_modify(|e| {
                if m.commit.rank < e.commit.rank {
                    *e = m.clone();
                }
            })
            .or_insert(m);
    }

    let mut code: Vec<CommitRef> = Vec::new();
    let mut feeders_raw: Vec<(Layer, CommitRef)> = Vec::new();
    let mut other: Vec<CommitRef> = Vec::new();
    for (_, m) in by_commit {
        match m.layer {
            Layer::Code => code.push(m.commit),
            l if l.is_feeder() => feeders_raw.push((l, m.commit)),
            _ => other.push(m.commit),
        }
    }

    // Author = oldest code commit; ties broken by best rank.
    code.sort_by(|a, b| {
        a.authored_at
            .cmp(&b.authored_at)
            .then(a.rank.total_cmp(&b.rank))
    });
    let code_commit_count = code.len();
    let author = code.first().cloned();
    let other_code: Vec<CommitRef> = code.into_iter().skip(1).collect();

    let author_at = author.as_ref().map(|a| a.authored_at);
    let mut feeders: Vec<Feeder> = feeders_raw
        .into_iter()
        .map(|(layer, commit)| {
            let delta_secs = author_at.map(|at| commit.authored_at - at);
            Feeder {
                layer,
                commit,
                delta_secs,
            }
        })
        .collect();
    // Show feeders oldest-first (the design-first ones lead).
    feeders.sort_by_key(|f| f.commit.authored_at);
    other.sort_by_key(|c| c.authored_at);

    let feeders_predating_author = feeders
        .iter()
        .filter(|f| f.delta_secs.is_some_and(|d| d < 0))
        .count();

    ProvenanceResolution {
        query: query.to_string(),
        author,
        other_code,
        feeders,
        // Session feeders are attached by `resolve_provenance` once the author is
        // known (they need the author's `authored_at` to compute Δ). The pure
        // diff-hunk resolver leaves them empty so its tests stay isolated.
        session_feeders: Vec::new(),
        other,
        code_commit_count,
        feeders_predating_author,
        session_feeders_predating_author: 0,
    }
}

/// PURE (DB-free, unit-tested): turn FTS-matched transcript edges into session
/// feeders. Dedupe by **(session, file)** keeping the best (lowest bm25) rank —
/// this collapses the gear-7 "repeated edits to one file read as near-duplicates"
/// noise into one representative per file per session. Attach each one's Δ to the
/// code author and sort oldest-first (edits predating the author lead — they are
/// the through-line). `author_at` is None for a prose-only term → all Δ are None.
pub fn resolve_session_feeders(
    author_at: Option<i64>,
    matches: Vec<SessionMatch>,
) -> Vec<SessionFeeder> {
    let mut by_key: HashMap<(String, String), SessionMatch> = HashMap::new();
    for m in matches {
        let key = (m.session_id.clone(), m.file_path.clone());
        by_key
            .entry(key)
            .and_modify(|e| {
                if m.rank < e.rank {
                    *e = m.clone();
                }
            })
            .or_insert(m);
    }

    let mut feeders: Vec<SessionFeeder> = by_key
        .into_values()
        .map(|m| {
            let delta_secs = author_at.map(|at| m.edited_at - at);
            SessionFeeder {
                layer: layer_of(&m.file_path),
                session_id: m.session_id,
                tool: m.tool,
                file_path: m.file_path,
                rationale: m.rationale,
                edited_at: m.edited_at,
                commit_hash: m.commit_hash,
                commit_summary: m.commit_summary,
                rank: m.rank,
                delta_secs,
            }
        })
        .collect();
    feeders.sort_by(|a, b| {
        a.edited_at
            .cmp(&b.edited_at)
            .then(a.rank.total_cmp(&b.rank))
    });
    feeders
}

/// Fetch matching diff hunks and run the pure resolver. `cap` bounds the rows
/// pulled (ordered oldest-first, so a cap keeps the author-relevant tail).
pub async fn resolve_provenance(
    pool: &SqlitePool,
    query: &str,
    cap: i64,
) -> Result<ProvenanceResolution> {
    // The shipped path resolves against the default-tokenized `search_index`.
    // Gear-10 (symbol_index) reuses the SAME resolver against a symbol-expanded
    // shadow by passing a different index table — parity-by-construction.
    resolve_provenance_in(pool, "search_index", query, cap).await
}

/// Resolve provenance against a named FTS index table over the diff-hunk rows.
/// `index_table` must mirror `search_index`'s (body, kind, rowid_ref, source_table)
/// shape and tag `diff_hunks` rows identically. The session-edge feeder leg is held
/// CONSTANT (always `find_session_edge`), so swapping the table isolates the
/// prose↔code co-occurrence tokenizer — the gear-10 A/B (D1_SCOPE §10 "Gear-10").
pub async fn resolve_provenance_in(
    pool: &SqlitePool,
    index_table: &str,
    query: &str,
    cap: i64,
) -> Result<ProvenanceResolution> {
    type Row = (String, i64, String, String, i64, f64);
    // `index_table` is a controlled constant (never user input) — safe to inline.
    let sql = format!(
        "SELECT dh.commit_hash, dh.authored_at, dh.summary, dh.file_path, \
                dh.new_start, bm25({index_table}) AS rank \
         FROM {index_table} s \
         JOIN diff_hunks dh ON dh.id = s.rowid_ref AND s.source_table = 'diff_hunks' \
         WHERE s.body MATCH ? \
         ORDER BY dh.authored_at ASC \
         LIMIT ?"
    );
    let rows: Vec<Row> = sqlx::query_as(&sql)
        .bind(query)
        .bind(cap)
        .fetch_all(pool)
        .await?;
    let matches = rows
        .into_iter()
        .map(|r| Match {
            layer: layer_of(&r.3),
            commit: CommitRef {
                commit_hash: r.0,
                authored_at: r.1,
                summary: r.2,
                file_path: r.3,
                new_start: r.4,
                rank: r.5,
            },
        })
        .collect();
    let mut resolution = resolve_from_matches(query, matches);

    // Fold in gear-7 session edges as a feeder layer. Reuse the gear's OWN
    // retrieval (`find_session_edge`) so the feeders are parity-by-construction
    // with `nibdex find-session-edge` — no second FTS query to drift. The author's
    // `authored_at` (just computed) is the Δ anchor; a session edit minutes before
    // it is the sharpest feeder the corpus has (D1_SCOPE §10 "Gear-7").
    let session_hits = crate::session_index::find_session_edge(pool, query, cap).await?;
    let session_matches: Vec<SessionMatch> = session_hits
        .into_iter()
        .map(|h| SessionMatch {
            session_id: h.session_id,
            tool: h.tool,
            file_path: h.file_path,
            rationale: h.rationale,
            edited_at: h.edited_at,
            commit_hash: h.commit_hash,
            commit_summary: h.commit_summary,
            rank: h.rank,
        })
        .collect();
    let author_at = resolution.author.as_ref().map(|a| a.authored_at);
    let session_feeders = resolve_session_feeders(author_at, session_matches);
    resolution.session_feeders_predating_author = session_feeders
        .iter()
        .filter(|f| f.delta_secs.is_some_and(|d| d < 0))
        .count();
    resolution.session_feeders = session_feeders;
    Ok(resolution)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cref(sha: &str, at: i64, path: &str, rank: f64) -> CommitRef {
        CommitRef {
            commit_hash: sha.to_string(),
            authored_at: at,
            summary: format!("{sha} summary"),
            file_path: path.to_string(),
            new_start: 1,
            rank,
        }
    }
    fn m(layer: Layer, c: CommitRef) -> Match {
        Match { layer, commit: c }
    }

    #[test]
    fn layer_classification() {
        assert_eq!(layer_of("src/mcp/fts5.rs"), Layer::Code);
        assert_eq!(layer_of("CLAUDE.md"), Layer::Session);
        assert_eq!(layer_of("docs/DESIGN.md"), Layer::Design);
        assert_eq!(layer_of("a/memory/foo.md"), Layer::Memory);
        assert_eq!(layer_of("Cargo.toml"), Layer::Config);
        assert_eq!(layer_of("migrations/x.sql"), Layer::Config);
        assert_eq!(layer_of("LICENSE"), Layer::Other);
    }

    #[test]
    fn author_is_oldest_code_not_oldest_prose() {
        // The gear-5 scenario: a design doc names the term (oldest), the code is
        // authored later, a refactor re-adds it later still. Author must be the
        // oldest CODE commit, not the older design doc.
        let matches = vec![
            m(Layer::Design, cref("design0", 100, "docs/DESIGN.md", -3.0)),
            m(Layer::Code, cref("author1", 200, "src/mcp.rs", -5.0)),
            m(Layer::Code, cref("mover2", 300, "src/mcp/fts5.rs", -4.0)),
            m(Layer::Session, cref("sess3", 250, "CLAUDE.md", -2.0)),
        ];
        let r = resolve_from_matches("broadened", matches);
        assert_eq!(r.author.as_ref().unwrap().commit_hash, "author1");
        assert_eq!(r.other_code.len(), 1); // the mover
        assert_eq!(r.other_code[0].commit_hash, "mover2");
        // Two feeders: design (predates code, Δ=-100) and session (Δ=+50).
        assert_eq!(r.feeders.len(), 2);
        assert_eq!(r.feeders_predating_author, 1);
        let design = r.feeders.iter().find(|f| f.layer == Layer::Design).unwrap();
        assert_eq!(design.delta_secs, Some(-100));
    }

    #[test]
    fn dedupes_by_commit_keeping_best_rank() {
        // One commit touching matching content in 3 places → one CommitRef,
        // best (lowest) rank kept.
        let matches = vec![
            m(Layer::Code, cref("c", 200, "src/a.rs", -2.0)),
            m(Layer::Code, cref("c", 200, "src/a.rs", -7.0)),
            m(Layer::Code, cref("c", 200, "src/a.rs", -4.0)),
        ];
        let r = resolve_from_matches("q", matches);
        assert!(r.other_code.is_empty());
        assert_eq!(r.author.as_ref().unwrap().rank, -7.0);
    }

    #[test]
    fn prose_only_term_has_no_author() {
        let matches = vec![
            m(Layer::Design, cref("d", 100, "docs/DESIGN.md", -3.0)),
            m(Layer::Session, cref("s", 150, "CLAUDE.md", -2.0)),
        ];
        let r = resolve_from_matches("vaporware", matches);
        assert!(r.author.is_none());
        assert_eq!(r.feeders.len(), 2);
        // No author → deltas are None, none counted as predating.
        assert_eq!(r.feeders_predating_author, 0);
        assert!(r.feeders.iter().all(|f| f.delta_secs.is_none()));
    }

    fn sm(session: &str, file: &str, why: &str, at: i64, rank: f64) -> SessionMatch {
        SessionMatch {
            session_id: session.to_string(),
            tool: "Edit".to_string(),
            file_path: file.to_string(),
            rationale: why.to_string(),
            edited_at: at,
            commit_hash: Some(format!("{session}commit")),
            commit_summary: Some(format!("{session} captured")),
            rank,
        }
    }

    #[test]
    fn session_feeders_dedupe_by_session_file_and_compute_delta() {
        let author_at = Some(1000);
        let matches = vec![
            // Two edits to the same file in one session → collapse to best rank.
            sm("sessA", "src/x.rs", "first pass", 940, -2.0),
            sm("sessA", "src/x.rs", "better rationale", 950, -6.0),
            // A different file edited earlier (predates author).
            sm("sessA", "src/y.rs", "y reason", 900, -3.0),
            // The SAME file in a DIFFERENT session is a distinct edge (after author).
            sm("sessB", "src/x.rs", "later edit", 1100, -1.0),
        ];
        let f = resolve_session_feeders(author_at, matches);

        // (sessA, src/x.rs) collapsed → 3 feeders, not 4.
        assert_eq!(f.len(), 3);
        // Sorted oldest-first: y(900) leads.
        assert_eq!(f[0].file_path, "src/y.rs");
        assert_eq!(f[0].edited_at, 900);
        assert_eq!(f[0].delta_secs, Some(-100));
        // The collapsed (sessA, x) keeps the best-rank representative.
        let xa = f
            .iter()
            .find(|e| e.session_id == "sessA" && e.file_path == "src/x.rs")
            .unwrap();
        assert_eq!(xa.rationale, "better rationale");
        assert_eq!(xa.rank, -6.0);
        assert_eq!(xa.delta_secs, Some(-50)); // 950 − 1000
        // Two of three predate the author (y at −100, sessA/x at −50); sessB/x is +100.
        let pre = f
            .iter()
            .filter(|e| e.delta_secs.is_some_and(|d| d < 0))
            .count();
        assert_eq!(pre, 2);
        assert_eq!(f[0].layer, Layer::Code);
    }

    #[test]
    fn session_feeders_no_author_gives_none_deltas() {
        let f = resolve_session_feeders(None, vec![sm("s", "src/x.rs", "r", 100, -2.0)]);
        assert_eq!(f.len(), 1);
        assert!(f[0].delta_secs.is_none());
    }
}
