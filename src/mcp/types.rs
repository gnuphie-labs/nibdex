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
    ///
    /// ⚠️ `#[serde(default)]` is load-bearing, not decoration. `schemars` derives
    /// `required` from the Rust type, so a bare `bool` is advertised as mandatory
    /// and then dropped by `skip_serializing_if` on every response where broadening
    /// did NOT fire — which is nearly all of them. That made 7 of 8 tools violate
    /// their own `outputSchema` on ordinary successful calls. `default` is what
    /// tells schemars the field is omissible, and it states the true semantics:
    /// absent means false. Same class as `retired_corpora` below; gated by
    /// `envelope_emits_every_field_its_schema_requires`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub query_broadened: bool,
    /// Why `results` is empty — present ONLY on a zero-result response, because
    /// that is the only time the caller cannot tell the two cases apart.
    ///
    /// `false` = the corpus holds rows, the query simply matched none of them.
    /// `true` = the corpus is EMPTY, so no query could have matched. An empty
    /// result is then a statement about the index, not about the codebase, and a
    /// caller that reads it as "this workspace has no such thing" is being misled
    /// by silence. Same reasoning as `query_broadened`: when nibdex quietly does
    /// something other than what the caller assumed, it says so.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub corpus_empty: Option<bool>,
    /// The newest item this corpus contains, ISO-8601 — the answer to "is what I
    /// searched current?". Present alongside `corpus_empty` on a zero-result
    /// response when the corpus is non-empty.
    ///
    /// ONE meaning across all five corpora: the most recent thing in there, as of
    /// the last index — newest edit, newest commit, newest file modification.
    /// Never "when nibdex last ran", which is a different question and mixing the
    /// two makes the field untrustworthy for either.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub corpus_indexed_through: Option<String>,
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

/// Which corpus a `find_*`/`recent_*` tool searched — the input to the
/// zero-result diagnosis (`corpus_empty` / `corpus_indexed_through`).
///
/// Each variant names its table and the clock for "the newest thing in here".
/// Where the corpus stores its own content time (an edit, a commit) that is the
/// answer; where it does not, the owning document's `mtime` is — the newest
/// source/doc/memory file the index holds.
///
/// ⚠️ NOT `documents.indexed_at`, which looks like the obvious choice and is
/// wrong. The write-amplification fix deliberately skips unchanged files without
/// refreshing `indexed_at`, so a repo whose source has not changed in two months
/// reports a two-month-old stamp on a freshly-built index — telling the caller
/// their index is stale when it is current, which is the same misdiagnosis this
/// whole field exists to prevent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Corpus {
    SessionEdges,
    CommitEntries,
    DesignDocSections,
    MemoryEntries,
    SourceChunks,
}

impl Corpus {
    /// `(count-query, newest-item-query)`. The second returns a unix timestamp.
    pub(crate) fn probe_sql(self) -> (&'static str, &'static str) {
        match self {
            Corpus::SessionEdges => (
                "SELECT COUNT(*) FROM session_edges",
                "SELECT MAX(edited_at) FROM session_edges",
            ),
            // `committed_at`, NOT `authored_at`. The author date is caller-supplied
            // (`git commit --date`) and is PRESERVED across a rebase, so it says when
            // the work was written, not when this history came to hold it. Two ways
            // that breaks a freshness answer: a future-dated commit makes the corpus
            // claim permanently-future content, and the ordinary case — a rebased or
            // cherry-picked branch — reports a freshly-indexed corpus as weeks stale.
            // The committer date is stamped by git when the object is written, which
            // is the question this field asks.
            Corpus::CommitEntries => (
                "SELECT COUNT(*) FROM commit_entries",
                "SELECT MAX(committed_at) FROM commit_entries",
            ),
            Corpus::DesignDocSections => (
                "SELECT COUNT(*) FROM design_doc_sections",
                "SELECT MAX(mtime) FROM documents WHERE kind = 'design_doc'",
            ),
            Corpus::MemoryEntries => (
                "SELECT COUNT(*) FROM memory_entries",
                "SELECT MAX(mtime) FROM documents WHERE kind = 'memory'",
            ),
            Corpus::SourceChunks => (
                "SELECT COUNT(*) FROM source_chunks",
                "SELECT MAX(mtime) FROM documents WHERE kind = 'source'",
            ),
        }
    }
}

/// One session→code edge (Gear 7/8): a single Edit/Write tool-call recovered
/// from a raw Claude Code transcript — the file it touched, the assistant text
/// that reasoned about it, and the commit that later captured it (when bound).
/// This is the flat per-edit shape `find_session` returns now that it reads the
/// `session_edges` corpus instead of the format-locked `session_entries` table
/// (punch-list #3; grouped-by-session is a POST-MVP fast-follow).
#[derive(Debug, Serialize, JsonSchema)]
pub struct SessionEdgeResult {
    /// The transcript's sessionId — the session these edges belong to.
    pub session_id: String,
    /// The tool that produced the edge: `Edit` or `Write`.
    pub tool: String,
    /// Repo-relative path the edit targeted (cwd-stripped at index time).
    pub file_path: String,
    /// The cwd at edit time (the repo-root candidate); NULL when unrecorded or
    /// NULLed because the edit's cwd was not domain-visible (Gear 2).
    pub repo_path: Option<String>,
    /// Per-line `gitBranch` at edit time — the commit join key.
    pub git_branch: Option<String>,
    pub edited_at_iso: String,
    pub edited_at_unix: i64,
    /// The nearest preceding assistant text — the "why" of the edit. This is the
    /// FTS body (rationale + path), so a concept query surfaces the edit by its
    /// reasoning, not just its path. `[rationale withheld: cross-domain session]`
    /// when the Gear 2 ratchet withheld it.
    pub rationale: String,
    /// Short SHA (first 7) of the capturing commit, when the session→commit
    /// binding surfaced. NULL when no indexed commit captured this edit.
    pub commit_hash: Option<String>,
    /// Full 40-char SHA of the capturing commit, when bound.
    pub commit_hash_full: Option<String>,
    /// Summary line of the capturing commit, when bound.
    pub commit_summary: Option<String>,
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
    /// A MATCH-centered window of the section (FTS5 `snippet`, ≤64 tokens), further
    /// bounded to `DESIGN_DOC_BODY_CHAR_LIMIT` chars and omitted once the per-call
    /// total-body budget is spent. NOT the whole section: any section longer than the
    /// window comes back with `body_truncated: true` — use `doc_path` +
    /// `line_start`/`line_end` to read it in full.
    pub body: String,
    /// First 200 chars of body, word-boundary truncated.
    pub body_excerpt: String,
    /// True when `body` was capped or omitted by the D-10.11 budget (full text lives at
    /// `doc_path` lines `line_start`..=`line_end`).
    pub body_truncated: bool,
    /// D-10.16: the line where the returned `body` window BEGINS — the match sits within
    /// the next few lines of it (the window is centered on the match, so this is the
    /// window's first line, not the match's own line). Falls back to `line_start` if the
    /// window can't be located (e.g. an empty body). Always within `line_start..=line_end`.
    pub match_line: i64,
    pub rank: Option<f64>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct CodeResult {
    /// Absolute path of the repo this chunk lives in — the missing half of `path`.
    ///
    /// `path` is repo-RELATIVE (it joins to `commit_entries.files_changed`), which
    /// makes an unscoped hit unopenable on a multi-repo index: `src/mcp/query.rs`
    /// names a different file in every tree that has one, and the caller cannot
    /// tell which was meant. `repo_path` + `path` is the openable location.
    ///
    /// `None` only for chunks indexed before this column existed and not yet
    /// reindexed — read it as UNKNOWN, never as "no repo".
    pub repo_path: Option<String>,
    /// Repo-relative path of the source file (joins to `commit_entries.files_changed`).
    pub path: String,
    pub line_start: i64,
    pub line_end: i64,
    /// Best-effort language tag from extension (NULL when unrecognized).
    pub language: Option<String>,
    /// A MATCH-centered window of the chunk (FTS5 `snippet`, ≤64 tokens), further
    /// bounded to `SOURCE_BODY_CHAR_LIMIT` chars and omitted once the per-call
    /// total-body budget is spent. NOT the whole 50-line chunk: any chunk longer than
    /// the window comes back with `body_truncated: true` — use `repo_path` + `path` +
    /// `line_start`/`line_end` to read it in full.
    pub body: String,
    /// First 200 chars of body, word-boundary truncated.
    pub body_excerpt: String,
    /// True when `body` was capped or omitted by the budget (full text lives at
    /// `path` lines `line_start`..=`line_end`).
    pub body_truncated: bool,
    /// D-10.16: the line where the returned `body` window BEGINS — the match sits within
    /// the next few lines of it (the window is centered on the match, so this is the
    /// window's first line, not the match's own line). Falls back to `line_start` if the
    /// window can't be located. Always within `line_start..=line_end`.
    pub match_line: i64,
    /// Freshness of the returned location vs the live working tree, established at
    /// query time (DESIGN §9.4 explicit freshness signal): `"verified"` (file unchanged
    /// since indexing — the line range is current), `"relocated"` (file changed and the
    /// chunk was re-anchored — the line numbers in THIS result are already corrected;
    /// `line_shift` carries the move), `"stale"` (file changed and the chunk could not
    /// be re-anchored — line numbers may be wrong), or `"file_missing"` (the indexed
    /// file no longer exists at this path). "Since indexing" is the working tree as it
    /// was when `nibdex index` / the on-commit reindex last ran — the extractor reads
    /// the working tree, not HEAD — so `verified` means unchanged since then, not
    /// "committed". The index re-reads on commit, so non-verified statuses usually
    /// mark uncommitted drift made after that pass.
    pub location: String,
    /// `Some(n)` only when `location == "relocated"`: the chunk moved n lines
    /// (+down / −up) since indexing; the corrected range already includes it.
    pub line_shift: Option<i64>,
    /// The PROVENANCE commit that last touched this chunk's FILE at HEAD when it was
    /// indexed — the code↔commit join (D1_SCOPE §1, §7). File-level last-touch, not
    /// line-level blame; and because the chunk text is the working tree at index
    /// time, uncommitted content in `body` is NOT in this commit. NULL when the
    /// commit isn't indexed (e.g. capped out) or the repo has no commits.
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
    /// Live file-watcher state, read from the `file_watcher_state` row a running
    /// `serve`/`watch` daemon heartbeats into this db (60 s liveness gate). Null when
    /// no daemon is live on this db — including plain stdio use with no daemon.
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
    /// Corpora that are DELIBERATELY retired: still in the schema, possibly still
    /// holding rows, but read by no query tool. Their counts — including their
    /// orphan counts — are not index damage, and must not be read as a defect.
    ///
    /// `session_entries` is the standing case. Its rows come from a CLAUDE.md
    /// format nothing writes any more; `find_session`/`recent_sessions` moved to
    /// `session_edges`. On a real second corpus every surviving row was orphaned
    /// while `session_edges` was healthy and serving — which is what settled the
    /// fix-vs-supersede fork toward supersede. Reviving the parser would have
    /// needed synthetic identity plus a schema migration to recover rows nothing
    /// reads.
    ///
    /// Naming it here is the honest half: the number stays visible and true, and
    /// stops being mistaken for a broken index. Additive-optional (absent when
    /// nothing is retired), so no `CHECK_SCHEMA_VERSION` bump.
    ///
    /// ⚠️ `Option<Vec<_>>` + `Option::is_none`, NOT `Vec` + `Vec::is_empty`.
    /// `schemars` derives `required` from the Rust type, not from serde's skip
    /// attribute, so a bare `Vec` is advertised in the tool's `outputSchema` as
    /// mandatory and then omitted whenever it is empty — which is the healthy
    /// case. That makes every `check()` on a clean index fail schema validation
    /// while a damaged one passes. `Option` is the only shape that makes the
    /// advertised contract and the emitted JSON agree.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retired_corpora: Option<Vec<RetiredCorpus>>,
    /// THE DENOMINATOR. How much retrieval work happened in the sessions this
    /// index has seen, and how much of it nibdex actually served.
    ///
    /// Every other instrument here — the JSONL sink, the cost ledger,
    /// `cost_savings` below — fires only when a nibdex tool is CALLED, which
    /// makes them survivorship-biased by construction. They report savings in
    /// proportion to use and are structurally blind to non-use: a period in
    /// which nibdex was never called records nothing, which reads as a quiet
    /// stretch rather than as a total miss. `cost_savings` can only ever
    /// deliver good news.
    ///
    /// This is the number that can say "you have a problem". Absent when no
    /// session activity has been indexed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adoption: Option<Adoption>,
}

/// Retrieval share across the indexed sessions — counts only, never content.
#[derive(Debug, Serialize, JsonSchema)]
pub struct Adoption {
    /// Sessions with any retrieval activity at all.
    pub sessions_seen: i64,
    /// ...of which, sessions that called nibdex at least once.
    pub sessions_using_nibdex: i64,
    /// Retrieval calls that went elsewhere (built-in search tools, shell greps).
    pub retrieval_elsewhere: i64,
    /// Retrieval calls that went to nibdex.
    pub nibdex_queries: i64,
    /// nibdex's share of all retrieval, as a percentage. The headline.
    pub nibdex_share_pct: f64,
}

/// One retired corpus, and why its non-zero counts are not a defect.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RetiredCorpus {
    /// The table name, as it appears under `indexer` and `orphans`.
    pub corpus: String,
    /// Rows still present.
    pub rows: i64,
    /// What superseded it, so the reader knows where the live data went.
    pub superseded_by: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct IndexerCounts {
    pub documents: BTreeMap<String, i64>,
    /// Legacy CLAUDE.md-format session corpus. Still extracted, but no query tool
    /// reads it since `find_session`/`recent_sessions` moved to `session_edges`;
    /// kept for the transition (empty on most workspaces). Full removal is deferred.
    pub session_entries: i64,
    /// The raw-transcript session→code map (`session_edges`) — the live corpus
    /// behind `find_session` and `recent_sessions`.
    pub session_edges: i64,
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
    /// Source chunks whose `documents` row names a file that no longer exists on
    /// disk. `nibdex index` prunes files that left the git index, so a non-zero
    /// count here means uncommitted deletions or a repo indexed then removed.
    pub source_chunks: i64,
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
    /// Optional FTS5 MATCH expression narrowing the rationale + path text. FTS5
    /// syntax; tokens FTS5 would reject (`fan-out`, `v0.1.3`) are auto-quoted, and
    /// nothing else is rewritten (no OR-broadening on the recency path). Omit for
    /// plain recency ordering.
    #[serde(default)]
    pub filter: Option<String>,

    /// Day window (defaults to 30) against each session's edit time (`edited_at`).
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
    /// Optional repo scope: a substring of the repo's absolute path (e.g. `"nibdex"`).
    /// Matches the same repo string `recent_commits`/`find_commit` take, so one
    /// value scopes code and commits alike. Absent = every indexed repo.
    #[serde(default)]
    pub repo: Option<String>,
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
