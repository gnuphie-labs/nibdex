// SPDX-License-Identifier: MIT

//! D1 gear 7 — the RAW-TRANSCRIPT session→code edge index (docs/D1_SCOPE.md §10
//! "Session-coverage probe").
//!
//! The probe settled the session-frontier fork toward raw transcripts: a perfect
//! CLAUDE.md path-parser recovers only ~25% of a session's edit-edges because the
//! curated note names code by *concept, not path* — "a `build` block on `check()`"
//! never spells `src/mcp/check.rs`. The transcript carries every edge LOSSLESSLY,
//! timestamped, and branch-anchored. This gear reads it.
//!
//! For each `~/.claude/projects/<slug>/*.jsonl` it extracts every `Edit`/`Write`
//! tool-call as one `session_edges` row: the file it touched (the CHANGE), the
//! nearest preceding assistant text (the RATIONALE), and the per-line `gitBranch`
//! + `cwd` + `timestamp` — the §1 commit join key, present for free.
//!
//! It then makes a best-effort session-to-commit binding to the oldest commit on
//! that repo which captured the file at/after the edit (the next commit to commit
//! it).
//!
//! SCRAPPY ON PURPOSE: pure `std::fs` + `serde_json` line walk, driven off a CLI
//! subcommand, perturbs nothing in indexer.rs. Run `nibdex index` first on the
//! same `--db` so `commit_entries` exists for the binding to resolve against.
//!
//! SPIKE SCOPE (see migration): indexes the EDGE (file + when + why), NOT the
//! verbatim diff body — that already lives in `diff_hunks`/`source_chunks`, and
//! omitting it keeps volume down and sidesteps the worst IP-scrub concern. Read-
//! edges (design context) are a recorded lever, not indexed here.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result};
use chrono::DateTime;
use serde_json::Value;
use sqlx::SqlitePool;

#[derive(Debug, Default)]
pub struct SessionIndexStats {
    pub transcripts_seen: usize,
    pub sessions_seen: usize,
    pub lines_total: usize,
    pub lines_parse_err: usize,
    pub edges_indexed: usize,
    pub edits: usize,
    pub writes: usize,
    /// Edges bound to an indexed `commit_entries.id` (the session→commit join hit).
    pub commit_bound: usize,
    /// Edges with no capturing commit indexed (binding precision is a §10 unknown).
    pub commit_unbound: usize,
    /// Edges already present (dedupe-key hit) — skipped by the additive merge.
    pub edges_duplicate: usize,
    /// Edges from a uuid-less message — cannot be safely deduped, so skipped.
    pub edges_skipped_no_uuid: usize,
    pub elapsed_ms: u128,
}

/// One write-edge extracted from a transcript, pre-binding.
struct SessionEdge {
    session_id: String,
    message_uuid: String,
    /// 0-based position of this Edit/Write among the captured tool-calls of its
    /// assistant message. `(message_uuid, edge_ordinal)` is the dedupe key.
    edge_ordinal: i64,
    tool: String,
    /// Repo-relative when `repo_path` is a prefix; otherwise left absolute.
    file_path: String,
    repo_path: Option<String>,
    git_branch: Option<String>,
    edited_at: i64,
    rationale: String,
}

/// Default transcript root: `~/.claude/projects`. Each child dir is a workspace
/// slug holding that workspace's `*.jsonl` session transcripts.
pub fn default_projects_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".claude/projects"))
}

/// Index every transcript under `projects_dir`. If `slug` is given, only that
/// child dir is scanned (e.g. `-Users-you-workspace`); otherwise all slugs.
///
/// ADDITIVE MERGE (docs/SESSION_SCOPE_DESIGN.md §5): re-running only ever grows
/// the corpus — `INSERT OR IGNORE` on the `(message_uuid, edge_ordinal)` dedupe
/// key skips already-indexed edges, so retention-expired transcripts' edges are
/// preserved, never erased. `rebuild = true` opts into the old wipe-first
/// behavior (a deliberate from-scratch reset). The whole pass runs in one
/// transaction (idempotent, and no per-row autocommit).
pub async fn index_sessions(
    pool: &SqlitePool,
    projects_dir: &Path,
    slug: Option<&str>,
    rebuild: bool,
) -> Result<SessionIndexStats> {
    let started = Instant::now();
    let mut stats = SessionIndexStats::default();

    if rebuild {
        // Deliberate from-scratch reset: drop the prior rows + their FTS entries.
        wipe_all_session_edges(pool).await?;
    }

    let transcripts = collect_transcripts(projects_dir, slug)?;
    let mut sessions: HashSet<String> = HashSet::new();

    let mut tx = pool.begin().await?;
    for path in &transcripts {
        stats.transcripts_seen += 1;
        let edges = parse_transcript(path, &mut stats)?;
        for edge in edges {
            sessions.insert(edge.session_id.clone());
            insert_edge(&mut tx, &edge, &mut stats).await?;
        }
    }
    tx.commit().await?;

    stats.sessions_seen = sessions.len();
    stats.elapsed_ms = started.elapsed().as_millis();
    Ok(stats)
}

/// `<projects_dir>/<slug>/*.jsonl`. With `slug=None`, every child dir's jsonl.
fn collect_transcripts(projects_dir: &Path, slug: Option<&str>) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let slug_dirs: Vec<PathBuf> = match slug {
        Some(s) => vec![projects_dir.join(s)],
        None => std::fs::read_dir(projects_dir)
            .with_context(|| format!("reading projects dir {projects_dir:?}"))?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect(),
    };
    for dir in slug_dirs {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.filter_map(|e| e.ok()) {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                out.push(p);
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Walk one JSONL transcript, emitting a write-edge per `Edit`/`Write` tool-call.
///
/// Rationale rule (D1_SCOPE §10: "tool_use = change, adjacent text = rationale"):
/// the assistant's text precedes its tool calls in a message, so the rationale is
/// the nearest non-empty text block AT OR BEFORE the call — the in-message text if
/// present, else the most recent text from a prior message (`last_text`).
fn parse_transcript(path: &Path, stats: &mut SessionIndexStats) -> Result<Vec<SessionEdge>> {
    let content =
        std::fs::read_to_string(path).with_context(|| format!("reading transcript {path:?}"))?;
    let mut edges = Vec::new();
    let mut last_text = String::new();

    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        stats.lines_total += 1;
        let obj: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => {
                stats.lines_parse_err += 1;
                continue;
            }
        };
        if obj.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(content_blocks) = obj
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(Value::as_array)
        else {
            continue;
        };

        let session_id = obj.get("sessionId").and_then(Value::as_str).unwrap_or("");
        let message_uuid = obj.get("uuid").and_then(Value::as_str).unwrap_or("");
        let cwd = obj.get("cwd").and_then(Value::as_str);
        let git_branch = obj.get("gitBranch").and_then(Value::as_str);
        let edited_at = obj
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_rfc3339);

        // `cur_text` = the latest non-empty text block seen so far in THIS message.
        let mut cur_text = String::new();
        // 0-based position of each captured Edit/Write within this message — the
        // second half of the `(message_uuid, edge_ordinal)` dedupe key.
        let mut edge_ordinal: i64 = 0;
        for block in content_blocks {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(t) = block.get("text").and_then(Value::as_str)
                        && !t.trim().is_empty()
                    {
                        cur_text = t.to_string();
                    }
                }
                Some("tool_use") => {
                    let tool = block.get("name").and_then(Value::as_str).unwrap_or("");
                    if tool != "Edit" && tool != "Write" {
                        continue;
                    }
                    let Some(abs_path) = block
                        .get("input")
                        .and_then(|i| i.get("file_path"))
                        .and_then(Value::as_str)
                    else {
                        continue;
                    };
                    let Some(at) = edited_at else { continue };
                    let rationale = if cur_text.trim().is_empty() {
                        last_text.clone()
                    } else {
                        cur_text.clone()
                    };
                    let (repo_path, rel) = split_repo_relative(abs_path, cwd);
                    edges.push(SessionEdge {
                        session_id: session_id.to_string(),
                        message_uuid: message_uuid.to_string(),
                        edge_ordinal,
                        tool: tool.to_string(),
                        file_path: rel,
                        repo_path,
                        git_branch: git_branch.map(str::to_string),
                        edited_at: at,
                        rationale: truncate_rationale(&rationale),
                    });
                    edge_ordinal += 1;
                }
                _ => {}
            }
        }
        if !cur_text.trim().is_empty() {
            last_text = cur_text;
        }
    }
    Ok(edges)
}

/// RFC3339 (`2026-06-06T01:13:22.173Z`) → unix epoch seconds.
fn parse_rfc3339(s: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(s).ok().map(|dt| dt.timestamp())
}

/// If `cwd` is a path prefix of `abs_path`, return `(Some(cwd), relative)`;
/// otherwise `(cwd, abs_path)` — a cross-cwd edit stays absolute, still recorded.
fn split_repo_relative(abs_path: &str, cwd: Option<&str>) -> (Option<String>, String) {
    if let Some(c) = cwd
        && let Some(rest) = abs_path.strip_prefix(c)
    {
        let rel = rest.trim_start_matches('/');
        if !rel.is_empty() {
            return (Some(c.to_string()), rel.to_string());
        }
    }
    (cwd.map(str::to_string), abs_path.to_string())
}

/// Keep the rationale bounded — a session's reasoning paragraph, not an essay.
/// (The FTS body is what we search; the first ~500 chars carry the intent.)
fn truncate_rationale(s: &str) -> String {
    const MAX: usize = 500;
    let s = s.trim();
    if s.len() <= MAX {
        return s.to_string();
    }
    let mut end = MAX;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

async fn insert_edge(
    conn: &mut sqlx::SqliteConnection,
    edge: &SessionEdge,
    stats: &mut SessionIndexStats,
) -> Result<()> {
    // An edge from a message with no uuid has no stable dedupe identity (all
    // would collide on ("", ordinal)); skip + count rather than risk silently
    // dropping distinct edges under the UNIQUE key.
    if edge.message_uuid.is_empty() {
        stats.edges_skipped_no_uuid += 1;
        return Ok(());
    }

    let commit_id = resolve_capturing_commit(&mut *conn, edge).await?;

    // Additive merge: INSERT OR IGNORE on the (message_uuid, edge_ordinal) dedupe
    // key. RETURNING yields no row when the edge already exists, so a re-index
    // neither double-inserts the row nor re-emits its FTS entry.
    let inserted: Option<(i64,)> = sqlx::query_as(
        r#"
        INSERT OR IGNORE INTO session_edges
            (session_id, message_uuid, edge_ordinal, tool, file_path, repo_path,
             git_branch, edited_at, rationale, commit_id)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        RETURNING id
        "#,
    )
    .bind(&edge.session_id)
    .bind(&edge.message_uuid)
    .bind(edge.edge_ordinal)
    .bind(&edge.tool)
    .bind(&edge.file_path)
    .bind(&edge.repo_path)
    .bind(&edge.git_branch)
    .bind(edge.edited_at)
    .bind(&edge.rationale)
    .bind(commit_id)
    .fetch_optional(&mut *conn)
    .await?;

    let Some(row) = inserted else {
        stats.edges_duplicate += 1;
        return Ok(());
    };

    // FTS body = rationale + path, so a query finds the thread by its reasoning
    // OR by the file it touched.
    let body = format!("{} {}", edge.rationale, edge.file_path);
    sqlx::query(
        "INSERT INTO search_index (body, kind, rowid_ref, source_table) \
         VALUES (?, 'session_edge', ?, 'session_edges')",
    )
    .bind(&body)
    .bind(row.0)
    .execute(&mut *conn)
    .await?;

    if commit_id.is_some() {
        stats.commit_bound += 1;
    } else {
        stats.commit_unbound += 1;
    }
    stats.edges_indexed += 1;
    match edge.tool.as_str() {
        "Edit" => stats.edits += 1,
        "Write" => stats.writes += 1,
        _ => {}
    }
    Ok(())
}

/// Best-effort session→commit binding: the OLDEST commit on `repo_path` that
/// changed `file_path` at/after `edited_at` — the next commit that captured the
/// edit. `files_changed` is a JSON array of quoted repo-relative paths, so we
/// match on the quoted token. None when no such commit is indexed.
async fn resolve_capturing_commit(
    conn: &mut sqlx::SqliteConnection,
    edge: &SessionEdge,
) -> Result<Option<i64>> {
    let Some(repo) = edge.repo_path.as_deref() else {
        return Ok(None);
    };
    // Match the path as a whole quoted JSON element to avoid partial-name hits.
    let needle = format!("%\"{}\"%", edge.file_path);
    let id: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM commit_entries \
         WHERE repo_path = ? AND authored_at >= ? AND files_changed LIKE ? \
         ORDER BY authored_at ASC LIMIT 1",
    )
    .bind(repo)
    .bind(edge.edited_at)
    .bind(needle)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(id)
}

async fn wipe_all_session_edges(pool: &SqlitePool) -> Result<()> {
    sqlx::query("DELETE FROM search_index WHERE source_table = 'session_edges'")
        .execute(pool)
        .await?;
    sqlx::query("DELETE FROM session_edges")
        .execute(pool)
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// The eyeball — `find-session-edge`. FTS5 MATCH over the session map (rationale
// + path), bm25-ranked (find_* semantics). The deliverable: does a CONCEPT query
// ("build provenance") surface the edges the curated note named only by concept —
// the files a path-regex over CLAUDE.md could never reach (D1_SCOPE §10)?
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct SessionEdgeHit {
    pub session_id: String,
    pub tool: String,
    pub file_path: String,
    pub git_branch: Option<String>,
    pub edited_at: i64,
    pub rationale: String,
    pub rank: f64,
    /// The capturing commit, when bound (the session→commit join surfaced).
    pub commit_hash: Option<String>,
    pub commit_summary: Option<String>,
}

/// Return matching session edges ordered by bm25 rank, capped at `limit`. SPIKE:
/// raw query bound to MATCH (no sanitize / OR-broaden — productionizing reuses
/// `mcp::fts5`).
pub async fn find_session_edge(
    pool: &SqlitePool,
    query: &str,
    limit: i64,
) -> Result<Vec<SessionEdgeHit>> {
    type Row = (
        String,
        String,
        String,
        Option<String>,
        i64,
        String,
        f64,
        Option<String>,
        Option<String>,
    );
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT se.session_id, se.tool, se.file_path, se.git_branch, se.edited_at, \
                se.rationale, bm25(search_index) AS rank, \
                ce.commit_hash, ce.message_summary \
         FROM search_index s \
         JOIN session_edges se ON se.id = s.rowid_ref AND s.source_table = 'session_edges' \
         LEFT JOIN commit_entries ce ON ce.id = se.commit_id \
         WHERE s.body MATCH ? \
         ORDER BY rank \
         LIMIT ?",
    )
    .bind(query)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| SessionEdgeHit {
            session_id: r.0,
            tool: r.1,
            file_path: r.2,
            git_branch: r.3,
            edited_at: r.4,
            rationale: r.5,
            rank: r.6,
            commit_hash: r.7,
            commit_summary: r.8,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_repo_relative_strips_cwd_prefix() {
        let (repo, rel) = split_repo_relative(
            "/Users/x/workspace/nibdex/src/mcp/check.rs",
            Some("/Users/x/workspace/nibdex"),
        );
        assert_eq!(repo.as_deref(), Some("/Users/x/workspace/nibdex"));
        assert_eq!(rel, "src/mcp/check.rs");
    }

    #[test]
    fn split_repo_relative_keeps_absolute_when_outside_cwd() {
        let (repo, rel) =
            split_repo_relative("/etc/hosts", Some("/Users/x/workspace/nibdex"));
        // cwd recorded, but the path is left absolute since it isn't under cwd.
        assert_eq!(repo.as_deref(), Some("/Users/x/workspace/nibdex"));
        assert_eq!(rel, "/etc/hosts");
    }

    #[test]
    fn split_repo_relative_handles_missing_cwd() {
        let (repo, rel) = split_repo_relative("/some/abs/path.rs", None);
        assert_eq!(repo, None);
        assert_eq!(rel, "/some/abs/path.rs");
    }

    #[test]
    fn parse_rfc3339_to_epoch() {
        // 2026-06-06T01:13:22.173Z — the transcript timestamp format.
        let epoch = parse_rfc3339("2026-06-06T01:13:22.173Z").unwrap();
        // Round-trips to the same instant (sub-second truncated).
        assert_eq!(epoch, 1780708402);
    }

    #[test]
    fn truncate_rationale_bounds_and_keeps_char_boundary() {
        let short = "a tidy reason";
        assert_eq!(truncate_rationale(short), short);
        let long = "x".repeat(600);
        let out = truncate_rationale(&long);
        assert!(out.chars().count() <= 501); // 500 + ellipsis
        assert!(out.ends_with('…'));
    }
}
