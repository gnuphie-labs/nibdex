// SPDX-License-Identifier: MIT

//! Pure query layer: the six `run_*` retrieval functions (recency + FTS5
//! search over sessions/commits/memory/design docs) plus their cutoff
//! helpers. Exposed for unit tests without the rmcp transport. Relocated
//! from `mcp.rs` by gh#6 (see `docs/MCP_SPLIT_PLAN.md`).

use std::time::Instant;

use anyhow::Result;
use sqlx::SqlitePool;

use super::format::{
    SessionRow, build_commit_result, build_session_result, finish_commit_envelope,
    finish_session_envelope, summarize,
};
use super::fts5::{
    fts5_or_broadened, like_substring, match_count_with_or_fallback, sanitize_fts5_query,
};
use super::types::{
    CodeResult, CommitResult, DEFAULT_DAYS, DEFAULT_LIMIT, DESIGN_DOC_BODY_CHAR_LIMIT,
    DESIGN_DOC_TOTAL_BODY_BUDGET, DesignDocResult, FindCodeRequest, FindCommitRequest,
    FindDesignDocRequest, FindMemoryRequest, FindSessionRequest, MAX_LIMIT, MemoryResult,
    RecentCommitsRequest, RecentSessionsRequest, SOURCE_BODY_CHAR_LIMIT, SOURCE_TOTAL_BODY_BUDGET,
    SUMMARY_CHAR_LIMIT, SessionResult, Stages, ToolEnvelope,
};

pub async fn run_recent_sessions(
    pool: &SqlitePool,
    req: &RecentSessionsRequest,
    stages: &mut Stages,
) -> Result<ToolEnvelope<SessionResult>> {
    let limit = req.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let days = req.days.unwrap_or(DEFAULT_DAYS).max(1);
    let cutoff_iso = sqlite_date_cutoff_iso(pool, days).await?;

    if let Some(filter) = req.filter.as_deref().filter(|s| !s.trim().is_empty()) {
        run_recent_sessions_with_filter(pool, filter, &cutoff_iso, limit, stages).await
    } else {
        run_recent_sessions_without_filter(pool, &cutoff_iso, limit, stages).await
    }
}

async fn sqlite_date_cutoff_iso(pool: &SqlitePool, days: i64) -> Result<String> {
    let modifier = format!("-{days} days");
    let (cutoff,): (String,) = sqlx::query_as("SELECT date('now', ?)")
        .bind(modifier)
        .fetch_one(pool)
        .await?;
    Ok(cutoff)
}

async fn sqlite_unix_cutoff(pool: &SqlitePool, days: i64) -> Result<i64> {
    let modifier = format!("-{days} days");
    let (cutoff,): (i64,) = sqlx::query_as("SELECT CAST(strftime('%s','now', ?) AS INTEGER)")
        .bind(modifier)
        .fetch_one(pool)
        .await?;
    Ok(cutoff)
}

async fn run_recent_sessions_without_filter(
    pool: &SqlitePool,
    cutoff_iso: &str,
    limit: i64,
    stages: &mut Stages,
) -> Result<ToolEnvelope<SessionResult>> {
    let t0 = Instant::now();
    let (total,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM session_entries \
         WHERE entry_date IS NULL OR entry_date >= ?",
    )
    .bind(cutoff_iso)
    .fetch_one(pool)
    .await?;
    stages.join_ms = t0.elapsed().as_millis() as u64;

    let t1 = Instant::now();
    let rows: Vec<SessionRow> = sqlx::query_as(
        // D-10.12: order by session_number, the only monotonic, always-present chronology
        // signal. `entry_date` is the first YYYY-MM-DD found in the body text (often a
        // quoted soak deadline or NULL for the newest entries), so ordering by it surfaced
        // 30-session-old entries as "most recent" and dropped the newest ones entirely.
        "SELECT session_number, entry_date, body, files_touched, todos_mentioned, decisions_made \
             FROM session_entries \
             WHERE entry_date IS NULL OR entry_date >= ? \
             ORDER BY session_number DESC \
             LIMIT ?",
    )
    .bind(cutoff_iso)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    stages.join_ms += t1.elapsed().as_millis() as u64;

    let t2 = Instant::now();
    let results = rows
        .into_iter()
        .map(|r| build_session_result(r, None))
        .collect();
    let envelope = finish_session_envelope(results, total, "recent_sessions", false);
    stages.shape_response_ms = t2.elapsed().as_millis() as u64;
    Ok(envelope)
}

async fn run_recent_sessions_with_filter(
    pool: &SqlitePool,
    filter: &str,
    cutoff_iso: &str,
    limit: i64,
    stages: &mut Stages,
) -> Result<ToolEnvelope<SessionResult>> {
    let match_filter = sanitize_fts5_query(filter);
    let t0 = Instant::now();
    let (total,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) \
         FROM search_index s \
         JOIN session_entries e ON e.id = s.rowid_ref AND s.source_table = 'session_entries' \
         WHERE s.body MATCH ? \
           AND (e.entry_date IS NULL OR e.entry_date >= ?)",
    )
    .bind(&match_filter)
    .bind(cutoff_iso)
    .fetch_one(pool)
    .await?;
    stages.fts5_query_ms = t0.elapsed().as_millis() as u64;

    type Row = (
        i64,
        Option<String>,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        f64,
    );
    let t1 = Instant::now();
    let rows: Vec<Row> = sqlx::query_as(
        // D-10.12: recency by session_number (see run_recent_sessions_without_filter),
        // bm25 rank only as a tiebreaker among equal — i.e. never — session numbers.
        "SELECT e.session_number, e.entry_date, e.body, \
                e.files_touched, e.todos_mentioned, e.decisions_made, \
                bm25(search_index) AS rank \
         FROM search_index s \
         JOIN session_entries e ON e.id = s.rowid_ref AND s.source_table = 'session_entries' \
         WHERE s.body MATCH ? \
           AND (e.entry_date IS NULL OR e.entry_date >= ?) \
         ORDER BY e.session_number DESC, rank ASC \
         LIMIT ?",
    )
    .bind(&match_filter)
    .bind(cutoff_iso)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    stages.join_ms = t1.elapsed().as_millis() as u64;

    let t2 = Instant::now();
    let results = rows
        .into_iter()
        .map(|(sn, ed, b, ft, tm, dm, rank)| {
            build_session_result((sn, ed, b, ft, tm, dm), Some(rank))
        })
        .collect();
    let envelope = finish_session_envelope(results, total, "recent_sessions", false);
    stages.shape_response_ms = t2.elapsed().as_millis() as u64;
    Ok(envelope)
}

pub async fn run_find_session(
    pool: &SqlitePool,
    req: &FindSessionRequest,
    stages: &mut Stages,
) -> Result<ToolEnvelope<SessionResult>> {
    let query = req.query.trim();
    if query.is_empty() {
        return Err(anyhow::anyhow!("query must not be empty"));
    }
    let sanitized = sanitize_fts5_query(query);
    let limit = req.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);

    const COUNT_SQL: &str = "SELECT COUNT(*) \
         FROM search_index s \
         JOIN session_entries e ON e.id = s.rowid_ref AND s.source_table = 'session_entries' \
         WHERE s.body MATCH ?";
    let t0 = Instant::now();
    let (match_query, total, broadened) =
        match_count_with_or_fallback(pool, COUNT_SQL, query, &sanitized).await?;
    stages.fts5_query_ms = t0.elapsed().as_millis() as u64;

    type Row = (
        i64,
        Option<String>,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        f64,
    );
    let t1 = Instant::now();
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT e.session_number, e.entry_date, e.body, \
                e.files_touched, e.todos_mentioned, e.decisions_made, \
                bm25(search_index) AS rank \
         FROM search_index s \
         JOIN session_entries e ON e.id = s.rowid_ref AND s.source_table = 'session_entries' \
         WHERE s.body MATCH ? \
         ORDER BY rank ASC, e.session_number DESC \
         LIMIT ?",
    )
    .bind(&match_query)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    stages.join_ms = t1.elapsed().as_millis() as u64;

    let t2 = Instant::now();
    let results = rows
        .into_iter()
        .map(|(sn, ed, b, ft, tm, dm, rank)| {
            build_session_result((sn, ed, b, ft, tm, dm), Some(rank))
        })
        .collect();
    let envelope = finish_session_envelope(results, total, "find_session", broadened);
    stages.shape_response_ms = t2.elapsed().as_millis() as u64;
    Ok(envelope)
}

pub async fn run_recent_commits(
    pool: &SqlitePool,
    req: &RecentCommitsRequest,
    stages: &mut Stages,
) -> Result<ToolEnvelope<CommitResult>> {
    let limit = req.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let days = req.days.unwrap_or(DEFAULT_DAYS).max(1);
    let cutoff_unix = sqlite_unix_cutoff(pool, days).await?;
    let repo_like = req.repo.as_deref().map(like_substring);

    if let Some(filter) = req.filter.as_deref().filter(|s| !s.trim().is_empty()) {
        run_recent_commits_with_filter(
            pool,
            filter,
            cutoff_unix,
            repo_like.as_deref(),
            limit,
            stages,
        )
        .await
    } else {
        run_recent_commits_without_filter(pool, cutoff_unix, repo_like.as_deref(), limit, stages)
            .await
    }
}

async fn run_recent_commits_without_filter(
    pool: &SqlitePool,
    cutoff_unix: i64,
    repo_like: Option<&str>,
    limit: i64,
    stages: &mut Stages,
) -> Result<ToolEnvelope<CommitResult>> {
    type Row = (
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        i64,
        i64,
        String,
        Option<String>,
        Option<String>,
        Option<i64>,
    );

    let t0 = Instant::now();
    let (total,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM commit_entries c \
         WHERE c.authored_at >= ? AND (? IS NULL OR c.repo_path LIKE ?)",
    )
    .bind(cutoff_unix)
    .bind(repo_like)
    .bind(repo_like)
    .fetch_one(pool)
    .await?;
    stages.join_ms = t0.elapsed().as_millis() as u64;

    let t1 = Instant::now();
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT c.commit_hash, c.repo_path, c.parent_hashes, c.author_email, c.author_name, \
                c.authored_at, c.committed_at, c.message_summary, c.message_body, c.files_changed, \
                r.is_shallow \
         FROM commit_entries c \
         LEFT JOIN indexed_repos r ON r.repo_path = c.repo_path \
         WHERE c.authored_at >= ? AND (? IS NULL OR c.repo_path LIKE ?) \
         ORDER BY c.authored_at DESC, c.commit_hash \
         LIMIT ?",
    )
    .bind(cutoff_unix)
    .bind(repo_like)
    .bind(repo_like)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    stages.join_ms += t1.elapsed().as_millis() as u64;

    let t2 = Instant::now();
    let results: Vec<CommitResult> = rows
        .into_iter()
        .map(|r| build_commit_result(r, None))
        .collect();
    let envelope = finish_commit_envelope(results, total, "recent_commits", false);
    stages.shape_response_ms = t2.elapsed().as_millis() as u64;
    Ok(envelope)
}

async fn run_recent_commits_with_filter(
    pool: &SqlitePool,
    filter: &str,
    cutoff_unix: i64,
    repo_like: Option<&str>,
    limit: i64,
    stages: &mut Stages,
) -> Result<ToolEnvelope<CommitResult>> {
    type Row = (
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        i64,
        i64,
        String,
        Option<String>,
        Option<String>,
        Option<i64>,
        f64,
    );

    let match_filter = sanitize_fts5_query(filter);
    let t0 = Instant::now();
    let (total,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) \
         FROM search_index s \
         JOIN commit_entries c ON c.id = s.rowid_ref AND s.source_table = 'commit_entries' \
         WHERE s.body MATCH ? \
           AND c.authored_at >= ? \
           AND (? IS NULL OR c.repo_path LIKE ?)",
    )
    .bind(&match_filter)
    .bind(cutoff_unix)
    .bind(repo_like)
    .bind(repo_like)
    .fetch_one(pool)
    .await?;
    stages.fts5_query_ms = t0.elapsed().as_millis() as u64;

    let t1 = Instant::now();
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT c.commit_hash, c.repo_path, c.parent_hashes, c.author_email, c.author_name, \
                c.authored_at, c.committed_at, c.message_summary, c.message_body, c.files_changed, \
                r.is_shallow, bm25(search_index) AS rank \
         FROM search_index s \
         JOIN commit_entries c ON c.id = s.rowid_ref AND s.source_table = 'commit_entries' \
         LEFT JOIN indexed_repos r ON r.repo_path = c.repo_path \
         WHERE s.body MATCH ? \
           AND c.authored_at >= ? \
           AND (? IS NULL OR c.repo_path LIKE ?) \
         ORDER BY c.authored_at DESC, rank ASC, c.commit_hash \
         LIMIT ?",
    )
    .bind(&match_filter)
    .bind(cutoff_unix)
    .bind(repo_like)
    .bind(repo_like)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    stages.join_ms = t1.elapsed().as_millis() as u64;

    let t2 = Instant::now();
    let results: Vec<CommitResult> = rows
        .into_iter()
        .map(|(ch, rp, ph, ae, an, aa, ca, ms, mb, fc, sh, rank)| {
            build_commit_result((ch, rp, ph, ae, an, aa, ca, ms, mb, fc, sh), Some(rank))
        })
        .collect();
    let envelope = finish_commit_envelope(results, total, "recent_commits", false);
    stages.shape_response_ms = t2.elapsed().as_millis() as u64;
    Ok(envelope)
}

pub async fn run_find_commit(
    pool: &SqlitePool,
    req: &FindCommitRequest,
    stages: &mut Stages,
) -> Result<ToolEnvelope<CommitResult>> {
    let query = req.query.trim();
    if query.is_empty() {
        return Err(anyhow::anyhow!("query must not be empty"));
    }
    let sanitized = sanitize_fts5_query(query);
    let limit = req.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let repo_like = req.repo.as_deref().map(like_substring);
    let repo_bind = repo_like.as_deref();

    type Row = (
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        i64,
        i64,
        String,
        Option<String>,
        Option<String>,
        Option<i64>,
        f64,
    );

    // D-10.13 OR-fallback, inlined here (not via match_count_with_or_fallback) because the
    // COUNT carries the optional repo filter's extra binds.
    let count_commits = |q: String| async move {
        let (n,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) \
             FROM search_index s \
             JOIN commit_entries c ON c.id = s.rowid_ref AND s.source_table = 'commit_entries' \
             WHERE s.body MATCH ? \
               AND (? IS NULL OR c.repo_path LIKE ?)",
        )
        .bind(q)
        .bind(repo_bind)
        .bind(repo_bind)
        .fetch_one(pool)
        .await?;
        Ok::<i64, anyhow::Error>(n)
    };
    let t0 = Instant::now();
    let (match_query, total, broadened) = {
        let total = count_commits(sanitized.clone()).await?;
        if total == 0 {
            if let Some(b) = fts5_or_broadened(query, &sanitized) {
                let bt = count_commits(b.clone()).await?;
                if bt > 0 {
                    (b, bt, true)
                } else {
                    (sanitized.clone(), total, false)
                }
            } else {
                (sanitized.clone(), total, false)
            }
        } else {
            (sanitized.clone(), total, false)
        }
    };
    stages.fts5_query_ms = t0.elapsed().as_millis() as u64;

    let t1 = Instant::now();
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT c.commit_hash, c.repo_path, c.parent_hashes, c.author_email, c.author_name, \
                c.authored_at, c.committed_at, c.message_summary, c.message_body, c.files_changed, \
                r.is_shallow, bm25(search_index) AS rank \
         FROM search_index s \
         JOIN commit_entries c ON c.id = s.rowid_ref AND s.source_table = 'commit_entries' \
         LEFT JOIN indexed_repos r ON r.repo_path = c.repo_path \
         WHERE s.body MATCH ? \
           AND (? IS NULL OR c.repo_path LIKE ?) \
         ORDER BY rank ASC, c.authored_at DESC, c.commit_hash \
         LIMIT ?",
    )
    .bind(&match_query)
    .bind(repo_bind)
    .bind(repo_bind)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    stages.join_ms = t1.elapsed().as_millis() as u64;

    let t2 = Instant::now();
    let results: Vec<CommitResult> = rows
        .into_iter()
        .map(|(ch, rp, ph, ae, an, aa, ca, ms, mb, fc, sh, rank)| {
            build_commit_result((ch, rp, ph, ae, an, aa, ca, ms, mb, fc, sh), Some(rank))
        })
        .collect();
    let envelope = finish_commit_envelope(results, total, "find_commit", broadened);
    stages.shape_response_ms = t2.elapsed().as_millis() as u64;
    Ok(envelope)
}

pub async fn run_find_memory(
    pool: &SqlitePool,
    req: &FindMemoryRequest,
    stages: &mut Stages,
) -> Result<ToolEnvelope<MemoryResult>> {
    let query = req.query.trim();
    if query.is_empty() {
        return Err(anyhow::anyhow!("query must not be empty"));
    }
    let sanitized = sanitize_fts5_query(query);
    let limit = req.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);

    const COUNT_SQL: &str = "SELECT COUNT(*) \
         FROM search_index s \
         JOIN memory_entries m ON m.id = s.rowid_ref AND s.source_table = 'memory_entries' \
         WHERE s.body MATCH ?";
    let t0 = Instant::now();
    let (match_query, total, broadened) =
        match_count_with_or_fallback(pool, COUNT_SQL, query, &sanitized).await?;
    stages.fts5_query_ms = t0.elapsed().as_millis() as u64;

    type Row = (String, String, Option<String>, String, f64);
    let t1 = Instant::now();
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT m.name, m.memory_type, m.description, m.body, bm25(search_index) AS rank \
         FROM search_index s \
         JOIN memory_entries m ON m.id = s.rowid_ref AND s.source_table = 'memory_entries' \
         WHERE s.body MATCH ? \
         ORDER BY rank ASC, m.name \
         LIMIT ?",
    )
    .bind(&match_query)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    stages.join_ms = t1.elapsed().as_millis() as u64;

    let t2 = Instant::now();
    let results: Vec<MemoryResult> = rows
        .into_iter()
        .map(
            |(name, memory_type, description, body, rank)| MemoryResult {
                name,
                memory_type,
                description,
                body,
                rank: Some(rank),
            },
        )
        .collect();

    let returned = results.len() as i64;
    // Memory bodies are returned untrimmed → the result body is the full read size.
    let returned_full_tokens =
        (results.iter().map(|r| r.body.chars().count()).sum::<usize>() / 4) as u64;
    let envelope = ToolEnvelope {
        results,
        total_matched: total,
        returned,
        tool: "find_memory".to_string(),
        query_broadened: broadened,
        returned_full_tokens,
    };
    stages.shape_response_ms = t2.elapsed().as_millis() as u64;
    Ok(envelope)
}

pub async fn run_find_design_doc(
    pool: &SqlitePool,
    req: &FindDesignDocRequest,
    stages: &mut Stages,
) -> Result<ToolEnvelope<DesignDocResult>> {
    let query = req.query.trim();
    if query.is_empty() {
        return Err(anyhow::anyhow!("query must not be empty"));
    }
    let sanitized = sanitize_fts5_query(query);
    let limit = req.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);

    const COUNT_SQL: &str = "SELECT COUNT(*) \
         FROM search_index s \
         JOIN design_doc_sections d ON d.id = s.rowid_ref AND s.source_table = 'design_doc_sections' \
         WHERE s.body MATCH ?";
    let t0 = Instant::now();
    let (match_query, total, broadened) =
        match_count_with_or_fallback(pool, COUNT_SQL, query, &sanitized).await?;
    stages.fts5_query_ms = t0.elapsed().as_millis() as u64;

    type Row = (String, String, i64, i64, String, i64, f64, i64);
    let t1 = Instant::now();
    // D-10.16: `snippet(search_index, 0, …, 64)` returns a MATCH-centered window of
    // the section body (FTS5 column 0 = `body`), not its head — so a hit deep in a
    // large section shows the matching passage, not the section's opening paragraph.
    // `match_line` is the snippet's exact start line: a second `snippet()` with empty
    // ellipsis is an exact substring of the body, `instr()` locates it, and the
    // newline count up to that offset (all server-side — the body never crosses the
    // wire) plus `line_start` is the line to jump to. `length(d.body)` carries the
    // full section length for the truncation flag without moving the body.
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT path, heading_path, line_start, line_end, snip, full_chars, rank, \
                CASE WHEN off > 0 \
                     THEN line_start + (length(substr(body, 1, off - 1)) \
                                        - length(replace(substr(body, 1, off - 1), char(10), ''))) \
                     ELSE line_start END AS match_line \
         FROM ( \
             SELECT docs.path AS path, d.heading_path AS heading_path, \
                    d.line_start AS line_start, d.line_end AS line_end, d.body AS body, \
                    snippet(search_index, 0, '', '', '…', 64) AS snip, \
                    length(d.body) AS full_chars, \
                    bm25(search_index) AS rank, \
                    instr(d.body, snippet(search_index, 0, '', '', '', 64)) AS off \
             FROM search_index s \
             JOIN design_doc_sections d ON d.id = s.rowid_ref AND s.source_table = 'design_doc_sections' \
             JOIN documents docs ON docs.id = d.document_id \
             WHERE s.body MATCH ? \
             ORDER BY rank ASC, docs.path, d.line_start \
             LIMIT ? \
         )",
    )
    .bind(&match_query)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    stages.join_ms = t1.elapsed().as_millis() as u64;

    let t2 = Instant::now();
    // D-10.11/D-10.16: the snippet is already match-centered and short, but we still
    // bound each body to `DESIGN_DOC_BODY_CHAR_LIMIT` and enforce a total-payload
    // budget as a backstop (the original ~246 KB blow-up was many fat sections at
    // once). Truncation is never silent: a bounded/dropped body sets `body_truncated`,
    // and `doc_path` + the `line_start`/`line_end` range let the caller Read the full
    // section. The excerpt is the head of the snippet, so it is match-relevant too and
    // stays present even when the body is budget-dropped.
    let mut spent_chars = 0usize;
    // Grounded-counterfactual raw input: sum the FULL section length (pre-trim) of
    // every returned hit — the by-hand read size before nibdex's snippet/budget cut.
    let mut returned_full_chars = 0usize;
    let results: Vec<DesignDocResult> = rows
        .into_iter()
        .map(
            |(doc_path, heading_path, line_start, line_end, snippet_body, full_chars, rank, match_line)| {
                let full_len = full_chars.max(0) as usize;
                returned_full_chars += full_len;
                let body_out = if spent_chars >= DESIGN_DOC_TOTAL_BODY_BUDGET {
                    // Budget exhausted: drop the inline body entirely; excerpt + line
                    // range still point the caller at the section.
                    String::new()
                } else {
                    let bounded = summarize(&snippet_body, DESIGN_DOC_BODY_CHAR_LIMIT);
                    spent_chars += bounded.chars().count();
                    bounded
                };
                DesignDocResult {
                    doc_path,
                    heading_path,
                    line_start,
                    line_end,
                    body_excerpt: summarize(&snippet_body, SUMMARY_CHAR_LIMIT),
                    body_truncated: body_out.chars().count() < full_len,
                    body: body_out,
                    match_line,
                    rank: Some(rank),
                }
            },
        )
        .collect();

    let returned = results.len() as i64;
    let returned_full_tokens = (returned_full_chars / 4) as u64;
    let envelope = ToolEnvelope {
        results,
        total_matched: total,
        returned,
        tool: "find_design_doc".to_string(),
        query_broadened: broadened,
        returned_full_tokens,
    };
    stages.shape_response_ms = t2.elapsed().as_millis() as u64;
    Ok(envelope)
}

/// D1a `find_code` (D1_SCOPE §5): FTS5 search over the `source_chunks` corpus →
/// bounded excerpt + path + line range + bm25 rank + the PROVENANCE commit (the
/// code↔commit join). Mirrors `run_find_design_doc` exactly — same sanitize +
/// OR-broaden + total-body budget — over the source schema, with a LEFT JOIN to
/// `commit_entries` so a chunk with no resolved provenance still returns.
pub async fn run_find_code(
    pool: &SqlitePool,
    req: &FindCodeRequest,
    stages: &mut Stages,
) -> Result<ToolEnvelope<CodeResult>> {
    let query = req.query.trim();
    if query.is_empty() {
        return Err(anyhow::anyhow!("query must not be empty"));
    }
    let sanitized = sanitize_fts5_query(query);
    let limit = req.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);

    const COUNT_SQL: &str = "SELECT COUNT(*) \
         FROM search_index s \
         JOIN source_chunks sc ON sc.id = s.rowid_ref AND s.source_table = 'source_chunks' \
         WHERE s.body MATCH ?";
    let t0 = Instant::now();
    let (match_query, total, broadened) =
        match_count_with_or_fallback(pool, COUNT_SQL, query, &sanitized).await?;
    stages.fts5_query_ms = t0.elapsed().as_millis() as u64;

    type Row = (
        String,
        i64,
        i64,
        Option<String>,
        String,
        i64,
        f64,
        Option<String>,
        Option<String>,
        i64,
        String,
        i64,
        String,
        String,
    );
    let t1 = Instant::now();
    // D-10.16: match-centered `snippet()` over the chunk body (FTS5 column 0), plus the
    // exact snippet start line via the empty-ellipsis-anchor + `instr()` + newline-count
    // trick (body stays server-side) — same passage-selection + pinpoint fix as
    // find_design_doc, so a hit deep in a chunk shows the matching code AND `path:match_line`
    // jumps straight to it. `length(sc.body)` carries the full length for the truncation flag.
    // The `documents` join carries the absolute path + stored mtime/hash for the
    // query-time freshness gate (source_index::location_status).
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT path, line_start, line_end, language, snip, full_chars, rank, \
                commit_sha, commit_summary, \
                CASE WHEN off > 0 \
                     THEN line_start + (length(substr(body, 1, off - 1)) \
                                        - length(replace(substr(body, 1, off - 1), char(10), ''))) \
                     ELSE line_start END AS match_line, \
                doc_path, doc_mtime, doc_hash, body \
         FROM ( \
             SELECT sc.path AS path, sc.line_start AS line_start, sc.line_end AS line_end, \
                    sc.language AS language, sc.body AS body, \
                    snippet(search_index, 0, '', '', '…', 64) AS snip, \
                    length(sc.body) AS full_chars, \
                    bm25(search_index) AS rank, \
                    ce.commit_hash AS commit_sha, ce.message_summary AS commit_summary, \
                    d.path AS doc_path, d.mtime AS doc_mtime, d.content_hash AS doc_hash, \
                    instr(sc.body, snippet(search_index, 0, '', '', '', 64)) AS off \
             FROM search_index s \
             JOIN source_chunks sc ON sc.id = s.rowid_ref AND s.source_table = 'source_chunks' \
             JOIN documents d ON d.id = sc.document_id \
             LEFT JOIN commit_entries ce ON ce.id = sc.last_commit_id \
             WHERE s.body MATCH ? \
             ORDER BY rank ASC, sc.path, sc.line_start \
             LIMIT ? \
         )",
    )
    .bind(&match_query)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    stages.join_ms = t1.elapsed().as_millis() as u64;

    // Same total-body budget as find_design_doc (D-10.11): bound each inline body and
    // cap the per-call payload. Truncation is never silent — a capped body ends in `…`
    // with `body_truncated` set, and path + line range let the caller Read full text.
    let t2 = Instant::now();
    let mut spent_chars = 0usize;
    // Grounded-counterfactual raw input: sum the FULL chunk length (pre-trim) of
    // every returned hit — the by-hand read size before nibdex's snippet/budget cut.
    let mut returned_full_chars = 0usize;
    let mut freshness = crate::source_index::FreshnessCache::new();
    let results: Vec<CodeResult> = rows
        .into_iter()
        .map(
            |(path, line_start, line_end, language, snippet_body, full_chars, rank, commit_sha, commit_summary, match_line, doc_path, doc_mtime, doc_hash, full_body)| {
                let full_len = full_chars.max(0) as usize;
                returned_full_chars += full_len;
                let body_out = if spent_chars >= SOURCE_TOTAL_BODY_BUDGET {
                    String::new()
                } else {
                    let bounded = summarize(&snippet_body, SOURCE_BODY_CHAR_LIMIT);
                    spent_chars += bounded.chars().count();
                    bounded
                };
                // Freshness gate + re-locator. `full_body` (the stored chunk) is the
                // relocation anchor — fetched from SQLite in-process, never serialized
                // into the MCP payload, so the D-10.11 budget discipline is untouched.
                let stored = crate::source_index::StoredChunk {
                    body: &full_body,
                    line_start,
                    line_end,
                    match_line,
                };
                let loc = crate::source_index::resolve_location(
                    &mut freshness,
                    &doc_path,
                    doc_mtime,
                    &doc_hash,
                    &stored,
                );
                CodeResult {
                    path,
                    line_start: loc.line_start,
                    line_end: loc.line_end,
                    language,
                    body_excerpt: summarize(&snippet_body, SUMMARY_CHAR_LIMIT),
                    body_truncated: body_out.chars().count() < full_len,
                    body: body_out,
                    match_line: loc.match_line,
                    location: loc.status.as_str().to_string(),
                    line_shift: loc.line_shift,
                    commit_sha,
                    commit_summary,
                    rank: Some(rank),
                }
            },
        )
        .collect();

    let returned = results.len() as i64;
    let returned_full_tokens = (returned_full_chars / 4) as u64;
    let envelope = ToolEnvelope {
        results,
        total_matched: total,
        returned,
        tool: "find_code".to_string(),
        query_broadened: broadened,
        returned_full_tokens,
    };
    stages.shape_response_ms = t2.elapsed().as_millis() as u64;
    Ok(envelope)
}
