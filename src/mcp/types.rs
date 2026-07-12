// SPDX-License-Identifier: MIT

//! Contract types for the MCP tool surface: tunable consts, the `ToolEnvelope`
//! wrapper, the per-tool result/request structs, the `check()` result shapes,
//! and the `Stages` timing helper. Pure data — no I/O. Relocated from `mcp.rs`
//! by gh#6 (see `docs/MCP_SPLIT_PLAN.md`).

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::build_info::BuildInfo;
use crate::cost_ledger::CostSavingsLedger;

/// MVP cap per D-6.1.1: `limit` defaults to 10, max 50.
pub(crate) const MAX_LIMIT: i64 = 50;
pub(crate) const DEFAULT_LIMIT: i64 = 10;
pub(crate) const DEFAULT_DAYS: i64 = 30;

/// Per D-6.1.2 summary key: first 200 chars of body, ASCII-safe word-boundary truncation.
pub(crate) const SUMMARY_CHAR_LIMIT: usize = 200;
/// D-10.11: per-section inline body cap for `find_design_doc` (chars). Larger than
/// `SUMMARY_CHAR_LIMIT` so a section is usually usable inline, but bounded so one fat
/// section can't dominate the result.
pub(crate) const DESIGN_DOC_BODY_CHAR_LIMIT: usize = 1_200;
/// D-10.11: total inline-body budget across all `find_design_doc` results (chars).
/// Once exceeded, further results carry an empty body (excerpt + line range remain).
/// Bounds the worst case to roughly this many chars regardless of `limit`.
pub(crate) const DESIGN_DOC_TOTAL_BODY_BUDGET: usize = 16_000;

/// D1a: per-chunk inline body cap for `find_code` (chars). A source chunk is a
/// fixed 50-line window, so this is sized to carry a whole window inline in the
/// common case while bounding a pathological long-line chunk.
pub(crate) const SOURCE_BODY_CHAR_LIMIT: usize = 2_000;
/// D1a: total inline-body budget across all `find_code` results (chars), mirroring
/// the D-10.11 design-doc guard. Once exceeded, further hits carry an empty body
/// (excerpt + path + line range remain, so the agent can open the exact lines).
pub(crate) const SOURCE_TOTAL_BODY_BUDGET: usize = 20_000;

/// Rolling perf window for `check()` (D-6.3.3). 1h matches the design pass.
pub(crate) const PERF_WINDOW_SECS: i64 = 3600;

/// `check()` schema_version (D-6.3.3). Bump only when the envelope shape changes.
pub(crate) const CHECK_SCHEMA_VERSION: i64 = 1;
// =====================================================================================
// Shared envelopes and result structs
// =====================================================================================

#[derive(Debug, Serialize, JsonSchema)]
pub struct ToolEnvelope<T> {
    pub results: Vec<T>,
    pub total_matched: i64,
    pub returned: i64,
    pub tool: String,
    /// True iff the caller's multi-term query AND-matched nothing and nibdex
    /// automatically retried it OR-broadened (D-10.13). Present only when broadening
    /// fired, so the caller knows these results are a relevance net cast wider than
    /// what they literally asked for.
    #[serde(skip_serializing_if = "is_false")]
    pub query_broadened: bool,
    /// Summed FULL (untruncated) token estimate of the hits in `results`, before
    /// any snippet/body-budget trimming — the "what you'd have read by hand once
    /// located" size. This is the Phase-1 raw input for the grounded
    /// counterfactual (`nibdex rescore` derives a per-query savings from it
    /// instead of the flat per-tool anchor). Metrics-internal only:
    /// `serde(skip)` + `schemars(skip)` keep it out of the caller payload AND
    /// the tool output schema, so it can never inflate `result_token_estimate`.
    #[serde(skip)]
    #[schemars(skip)]
    pub returned_full_tokens: u64,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SessionResult {
    pub session_number: i64,
    pub entry_date: Option<String>,
    pub summary: String,
    pub body: String,
    pub files_touched: Value,
    pub todos_mentioned: Value,
    pub decisions_made: Value,
    pub rank: Option<f64>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct CommitResult {
    /// Short SHA (first 7 chars) for display.
    pub commit_hash: String,
    /// Full 40-char SHA for canonical reference.
    pub commit_hash_full: String,
    pub repo_path: String,
    pub authored_at_iso: String,
    pub authored_at_unix: i64,
    pub author_email: Option<String>,
    pub author_name: Option<String>,
    pub message_summary: String,
    pub message_body: Option<String>,
    pub files_changed: Value,
    pub parent_hashes: Value,
    /// Joined from `indexed_repos`; true if the local clone is shallow (history truncated).
    pub is_shallow: bool,
    pub rank: Option<f64>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct MemoryResult {
    pub name: String,
    pub memory_type: String,
    pub description: Option<String>,
    pub body: String,
    pub rank: Option<f64>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct DesignDocResult {
    pub doc_path: String,
    pub heading_path: String,
    pub line_start: i64,
    pub line_end: i64,
    /// Section body, bounded to `DESIGN_DOC_BODY_CHAR_LIMIT` chars (and omitted once the
    /// per-call total-body budget is spent). When bounded, `body_truncated` is true —
    /// use `doc_path` + `line_start`/`line_end` to read the full section.
    pub body: String,
    /// First 200 chars of body, word-boundary truncated.
    pub body_excerpt: String,
    /// True when `body` was capped or omitted by the D-10.11 budget (full text lives at
    /// `doc_path` lines `line_start`..=`line_end`).
    pub body_truncated: bool,
    /// D-10.16: the exact line where the returned snippet begins (the matched passage),
    /// not just the section start — so `doc_path:match_line` jumps straight to it with no
    /// second search. Falls back to `line_start` if the snippet can't be located (e.g. an
    /// empty body). Always within `line_start..=line_end`.
    pub match_line: i64,
    pub rank: Option<f64>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct CodeResult {
    /// Repo-relative path of the source file (joins to `commit_entries.files_changed`).
    pub path: String,
    pub line_start: i64,
    pub line_end: i64,
    /// Best-effort language tag from extension (NULL when unrecognized).
    pub language: Option<String>,
    /// Chunk body, bounded to `SOURCE_BODY_CHAR_LIMIT` chars (and omitted once the
    /// per-call total-body budget is spent). When bounded, `body_truncated` is true —
    /// use `path` + `line_start`/`line_end` to read the full chunk.
    pub body: String,
    /// First 200 chars of body, word-boundary truncated.
    pub body_excerpt: String,
    /// True when `body` was capped or omitted by the budget (full text lives at
    /// `path` lines `line_start`..=`line_end`).
    pub body_truncated: bool,
    /// D-10.16: the exact line where the returned snippet begins (the matched code), not
    /// just the chunk start — `path:match_line` jumps straight to it. Falls back to
    /// `line_start` if the snippet can't be located. Always within `line_start..=line_end`.
    pub match_line: i64,
    /// Freshness of the returned location vs the live working tree, established at
    /// query time (DESIGN §9.4 explicit freshness signal): `"verified"` (file unchanged
    /// since indexing — the line range is current), `"relocated"` (file changed and the
    /// chunk was re-anchored — the line numbers in THIS result are already corrected;
    /// `line_shift` carries the move), `"stale"` (file changed and the chunk could not
    /// be re-anchored — line numbers may be wrong), or `"file_missing"` (the indexed
    /// file no longer exists at this path). The index heals on commit, so non-verified
    /// statuses mark uncommitted working-tree drift.
    pub location: String,
    /// `Some(n)` only when `location == "relocated"`: the chunk moved n lines
    /// (+down / −up) since indexing; the corrected range already includes it.
    pub line_shift: Option<i64>,
    /// The PROVENANCE commit that last touched this chunk's file — the code↔commit
    /// join (D1_SCOPE §1, §7). NULL when the commit isn't indexed (e.g. capped out).
    pub commit_sha: Option<String>,
    pub commit_summary: Option<String>,
    pub rank: Option<f64>,
}

// =====================================================================================
// check() envelope (D-6.3.3)
// =====================================================================================

#[derive(Debug, Serialize, JsonSchema)]
pub struct CheckResult {
    pub schema_version: i64,
    pub daemon_uptime_s: i64,
    pub indexer: IndexerCounts,
    pub orphans: OrphanCounts,
    pub shallow_repos: Vec<String>,
    /// Per-tool p50 latency (ms) over the last hour. Empty until tools are exercised.
    pub perf_p50_ms: BTreeMap<String, i64>,
    pub perf_p95_ms: BTreeMap<String, i64>,
    /// Populated by Day 6 commit 4 file-watcher integration. Null in stdio mode (D-6.4.1).
    pub file_watcher: Option<FileWatcherStats>,
    /// Latest duration_ms per `extract.*` op_name.
    pub extractors_last_run_ms: BTreeMap<String, i64>,
    /// §8.4 Layer 2 cost-savings rollup. `None` when calibration model
    /// is not loaded (Layer-1-only mode); `Some` when calibration.toml
    /// resolved cleanly (D-8.6). Serializes as `null` when None.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_savings: Option<CostSavingsLedger>,
    /// Compile-time build provenance (crate version + git sha/describe/commit
    /// time). Lets a caller confirm WHICH binary is answering — the deploy
    /// signal `calibration_model_version` (config, not binary) can't give.
    /// DROP-classified for `metrics-export` (§4.2): an interrogation/health
    /// surface, kept off the metrics payload.
    pub build: BuildInfo,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct IndexerCounts {
    pub documents: BTreeMap<String, i64>,
    pub session_entries: i64,
    pub memory_entries: i64,
    pub design_doc_sections: i64,
    pub source_chunks: i64,
    pub commit_entries: i64,
    pub indexed_repos: i64,
    pub search_index_total: i64,
}

/// Orphan counts. All zero at commit 2 (computation lands commit 3 per D-6.3.1).
#[derive(Debug, Serialize, JsonSchema)]
pub struct OrphanCounts {
    pub session_entries: i64,
    pub memory_entries: i64,
    pub design_doc_sections: i64,
    pub indexed_repos: i64,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct FileWatcherStats {
    pub events_total: i64,
    pub events_coalesced_total: i64,
    pub last_event_ts: Option<i64>,
    pub subscriptions: Vec<String>,
}

// =====================================================================================
// Request structs — one per tool. D-6.1.1 contract.
// =====================================================================================

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RecentSessionsRequest {
    /// Optional FTS5 MATCH expression narrowing the body text. Pass raw FTS5 syntax —
    /// nibdex does not transform the query (D-6.1.4). Omit for plain recency ordering.
    #[serde(default)]
    pub filter: Option<String>,

    /// Day window (defaults to 30) against `entry_date`.
    #[serde(default)]
    pub days: Option<i64>,

    /// Maximum rows to return. Defaults to 10, capped at 50 (D-6.1.1).
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FindSessionRequest {
    /// FTS5 MATCH expression. Required.
    pub query: String,
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RecentCommitsRequest {
    /// Optional FTS5 MATCH expression narrowing commit messages.
    #[serde(default)]
    pub filter: Option<String>,
    /// Day window against `authored_at` (default 30).
    #[serde(default)]
    pub days: Option<i64>,
    /// Substring match against `commit_entries.repo_path`. Case-sensitive (filesystem).
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FindCommitRequest {
    pub query: String,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FindMemoryRequest {
    pub query: String,
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FindDesignDocRequest {
    pub query: String,
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FindCodeRequest {
    pub query: String,
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct CheckRequest {}

// =====================================================================================
// Stages helper (D-7.3)
// =====================================================================================

/// Per-call stage timings consumed by `MetricsEvent.stages_ms`. Four
/// buckets, all milliseconds.
///
/// Mapping today: `fts5_query_ms` = the COUNT roundtrip (FTS5 MATCH
/// enumerates candidates); `join_ms` = the SELECT roundtrip (joins to
/// the source table + bm25 rank when filtered); `rank_ms` = 0 because
/// bm25 is folded into the SELECT in current SQL; `shape_response_ms`
/// = the post-query envelope construction. Honest framing per D-7.3.
///
/// NOT persisted to `op_measurements` — that would inflate row counts
/// ~5× and break the existing D-6.3.3 aggregation queries. Stages
/// stay emission-only.
#[derive(Debug, Default, Clone)]
pub struct Stages {
    pub fts5_query_ms: u64,
    pub rank_ms: u64,
    pub join_ms: u64,
    pub shape_response_ms: u64,
}

impl Stages {
    /// Render as the 4-key JSON object that lands in
    /// `MetricsEvent.stages_ms`. Key order matches D-7.3.
    pub fn to_json(&self) -> Value {
        serde_json::json!({
            "fts5_query": self.fts5_query_ms,
            "rank": self.rank_ms,
            "join": self.join_ms,
            "shape_response": self.shape_response_ms,
        })
    }
}
