// SPDX-License-Identifier: MIT

//! Result shaping: row→struct builders (`build_session_edge_result`,
//! `build_commit_result`), envelope finishers, and the small formatting
//! helpers (`summarize`, `unix_to_iso`, `percentile`, `parse_json_array`).
//! Relocated from `mcp.rs` by gh#6 (see `docs/MCP_SPLIT_PLAN.md`).

use chrono::{TimeZone, Utc};
use serde_json::Value;

use super::types::{CommitResult, SessionEdgeResult, ToolEnvelope};

/// Row shape returned by `session_edges` queries (rank passed separately, as with
/// the other builders): (session_id, tool, file_path, repo_path, git_branch,
/// edited_at, rationale, commit_hash_full, commit_summary).
pub(crate) type SessionEdgeRow = (
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    i64,
    String,
    Option<String>,
    Option<String>,
);

pub(crate) fn build_session_edge_result(row: SessionEdgeRow, rank: Option<f64>) -> SessionEdgeResult {
    let (
        session_id,
        tool,
        file_path,
        repo_path,
        git_branch,
        edited_at,
        rationale,
        commit_hash_full,
        commit_summary,
    ) = row;
    let commit_hash = commit_hash_full
        .as_ref()
        .map(|h| h.chars().take(7).collect());
    SessionEdgeResult {
        session_id,
        tool,
        file_path,
        repo_path,
        git_branch,
        edited_at_iso: unix_to_iso(edited_at),
        edited_at_unix: edited_at,
        rationale,
        commit_hash,
        commit_hash_full,
        commit_summary,
        rank,
    }
}

pub(crate) fn finish_session_edge_envelope(
    results: Vec<SessionEdgeResult>,
    total: i64,
    tool: &str,
    query_broadened: bool,
) -> ToolEnvelope<SessionEdgeResult> {
    let returned = results.len() as i64;
    // The by-hand counterfactual for a session edge is reading its rationale
    // (the "why" the transcript preserves); that is the untrimmed read size
    // (chars ÷ 4, matching `token_estimate_from_serialized`).
    let returned_full_tokens =
        (results.iter().map(|r| r.rationale.chars().count()).sum::<usize>() / 4) as u64;
    ToolEnvelope {
        results,
        total_matched: total,
        returned,
        tool: tool.to_string(),
        query_broadened,
        returned_full_tokens,
    }
}

#[allow(clippy::type_complexity)]
pub(crate) fn build_commit_result(
    row: (
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
    ),
    rank: Option<f64>,
) -> CommitResult {
    let (
        commit_hash_full,
        repo_path,
        parent_hashes_json,
        author_email,
        author_name,
        authored_at,
        _committed_at,
        message_summary,
        message_body,
        files_changed_json,
        is_shallow,
    ) = row;
    let short: String = commit_hash_full.chars().take(7).collect();
    CommitResult {
        commit_hash: short,
        commit_hash_full,
        repo_path,
        authored_at_iso: unix_to_iso(authored_at),
        authored_at_unix: authored_at,
        author_email,
        author_name,
        message_summary,
        message_body,
        files_changed: parse_json_array(files_changed_json.as_deref()),
        parent_hashes: parse_json_array(parent_hashes_json.as_deref()),
        is_shallow: is_shallow.unwrap_or(0) != 0,
        rank,
    }
}

pub(crate) fn finish_commit_envelope(
    results: Vec<CommitResult>,
    total: i64,
    tool: &str,
    query_broadened: bool,
) -> ToolEnvelope<CommitResult> {
    let returned = results.len() as i64;
    // A by-hand commit lookup reads the full message (summary + body); that is the
    // untrimmed read size for the grounded counterfactual (chars ÷ 4).
    let returned_full_tokens = (results
        .iter()
        .map(|r| {
            r.message_summary.chars().count()
                + r.message_body.as_deref().map_or(0, |b| b.chars().count())
        })
        .sum::<usize>()
        / 4) as u64;
    ToolEnvelope {
        results,
        total_matched: total,
        returned,
        tool: tool.to_string(),
        query_broadened,
        returned_full_tokens,
    }
}


pub(crate) fn unix_to_iso(unix: i64) -> String {
    Utc.timestamp_opt(unix, 0)
        .single()
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_else(|| format!("@{unix}"))
}

/// Nearest-rank percentile. `q` in [0, 1]. `durations` must be sorted ascending.
pub(crate) fn percentile(sorted: &[i64], q: f64) -> i64 {
    if sorted.is_empty() {
        return 0;
    }
    let q = q.clamp(0.0, 1.0);
    let idx = ((sorted.len() - 1) as f64 * q).round() as usize;
    sorted[idx]
}

/// Best-effort decode of JSON-array TEXT columns. Returns Null if absent, Array on success,
/// falls back to a string-wrapped value if the column held non-JSON text (defensive).
pub(crate) fn parse_json_array(raw: Option<&str>) -> Value {
    match raw {
        None => Value::Null,
        Some("") => Value::Null,
        Some(s) => serde_json::from_str(s).unwrap_or_else(|_| Value::String(s.to_string())),
    }
}

/// Truncate `body` to at most `max_chars` chars on a word boundary. Char-aware so multibyte
/// graphemes don't get split mid-encoding.
pub(crate) fn summarize(body: &str, max_chars: usize) -> String {
    if body.chars().count() <= max_chars {
        return body.to_string();
    }
    let truncated: String = body.chars().take(max_chars).collect();
    match truncated.rfind(|c: char| c.is_whitespace()) {
        Some(idx) if idx > max_chars / 2 => {
            let mut s = truncated[..idx].to_string();
            s.push('…');
            s
        }
        _ => {
            let mut s = truncated;
            s.push('…');
            s
        }
    }
}
