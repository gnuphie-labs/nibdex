// SPDX-License-Identifier: MIT

//! rmcp server surface: `NibdexServer::new()`, the `#[tool_router]` impl
//! holding all seven `#[tool]` handlers (one block — the rmcp macro only
//! collects methods syntactically present in the block it annotates, plan
//! §2.1), the `#[tool_handler]` `ServerHandler` impl, and `serve_stdio()`.
//! Relocated from `mcp.rs` by gh#6 (see `docs/MCP_SPLIT_PLAN.md`).

use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use rmcp::ServerHandler;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::transport::io::stdio;
use rmcp::{Json, ServiceExt, tool, tool_handler, tool_router};
use sqlx::SqlitePool;

use crate::calibration::CalibrationModel;
use crate::metrics::Op;
use crate::metrics_sink::MetricsSink;

use super::NibdexServer;
use super::check::run_check;
use super::fts5::explain_query_error;
use super::query::*;
use super::types::*;

impl NibdexServer {
    /// `started_at` is the *process* start instant, passed in by the caller
    /// rather than minted here. The HTTP transport (`StreamableHttpService`)
    /// constructs a fresh `NibdexServer` per MCP session via its factory
    /// closure; if each minted its own `Instant::now()`, `daemon_uptime_s`
    /// would reset to ~0 on every new `claude` connection (it reads
    /// `self.started_at.elapsed()`) and silently diverge from `/healthz`,
    /// which captures the instant once. Threading one shared instant keeps
    /// the MCP `check()` uptime honest — and equal to healthz. Surfaced as a
    /// D-10 self-dogfood finding (session #651).
    pub fn new(
        pool: SqlitePool,
        started_at: Instant,
        metrics_sink: Option<Arc<MetricsSink>>,
        calibration: Option<Arc<CalibrationModel>>,
    ) -> Self {
        Self {
            pool,
            tool_router: Self::tool_router(),
            started_at,
            metrics_sink,
            calibration,
        }
    }
}


// =====================================================================================
// Tool router — registers all 6 query tools + check()
// =====================================================================================

#[tool_router(router = tool_router)]
impl NibdexServer {
    /// Recently-active sessions by recency, optionally narrowed by FTS5 filter.
    #[tool(
        name = "recent_sessions",
        description = "Recently-active sessions from the session→code map — one representative row \
                       per session (its most-recent Edit/Write, with the file, rationale, and \
                       capturing commit), ordered by that latest edit DESC. \
                       `filter` is an OPTIONAL FTS5 MATCH expression against the edit rationale + path \
                       (raw FTS5 syntax, not natural language) that narrows which sessions appear. \
                       `days` defaults to 30 (window on edit time). `limit` defaults to 10, max 50. \
                       Returns envelope with results array + total_matched (distinct sessions) + returned + tool."
    )]
    pub async fn recent_sessions(
        &self,
        params: Parameters<RecentSessionsRequest>,
    ) -> Result<Json<ToolEnvelope<SessionEdgeResult>>, String> {
        let req = params.0;
        let op = Op::start("tool.recent_sessions");
        let call_start = Instant::now();
        let mut stages = Stages::default();
        match run_recent_sessions(&self.pool, &req, &mut stages).await {
            Ok(envelope) => {
                let _ = op
                    .complete(
                        &self.pool,
                        Some(envelope.total_matched),
                        Some(envelope.returned),
                        serde_json::json!({
                            "filter_set": req.filter.is_some(),
                            "days": req.days.unwrap_or(DEFAULT_DAYS),
                        }),
                    )
                    .await;
                let serialized = serde_json::to_string(&envelope).unwrap_or_default();
                self.emit_metrics(
                    "recent_sessions",
                    req.filter.as_deref(),
                    serde_json::json!({
                        "filter_set": req.filter.is_some(),
                        "days": req.days.unwrap_or(DEFAULT_DAYS),
                        "limit_requested": req.limit,
                    }),
                    &stages,
                    call_start.elapsed().as_millis() as u64,
                    envelope.total_matched,
                    envelope.returned,
                    envelope.returned_full_tokens,
                    req.filter.is_some(),
                    envelope.query_broadened,
                    &serialized,
                )
                .await;
                Ok(Json(envelope))
            }
            Err(e) => {
                let msg = e.to_string();
                let _ = op.complete_err(&self.pool, &msg).await;
                self.emit_error_metrics(
                    "recent_sessions",
                    req.filter.as_deref(),
                    serde_json::json!({
                        "filter_set": req.filter.is_some(),
                        "days": req.days.unwrap_or(DEFAULT_DAYS),
                        "limit_requested": req.limit,
                    }),
                    &stages,
                    call_start.elapsed().as_millis() as u64,
                    &msg,
                )
                .await;
                Err(format!("recent_sessions: {}", explain_query_error(&msg)))
            }
        }
    }

    /// Session→code edits (file + rationale + capturing commit) ranked by FTS5 relevance.
    #[tool(
        name = "find_session",
        description = "Search the session→code map by FTS5 relevance: past Edit/Write actions \
                       recovered from Claude Code transcripts, each returning the file it touched, \
                       the assistant rationale for the edit, and the commit that captured it (when \
                       bound). Matches on the rationale + path, so a CONCEPT query (\"loopback \
                       enforcement\") surfaces edits by their reasoning, not just their filename. \
                       `query` is a REQUIRED FTS5 MATCH expression (raw FTS5 syntax, not natural \
                       language); a multi-term query that AND-matches nothing is auto-retried \
                       OR-broadened. Results ordered by bm25 rank ASC (best first), then most \
                       recent edit. `limit` defaults to 10, max 50."
    )]
    pub async fn find_session(
        &self,
        params: Parameters<FindSessionRequest>,
    ) -> Result<Json<ToolEnvelope<SessionEdgeResult>>, String> {
        let req = params.0;
        let op = Op::start("tool.find_session");
        let call_start = Instant::now();
        let mut stages = Stages::default();
        match run_find_session(&self.pool, &req, &mut stages).await {
            Ok(envelope) => {
                let _ = op
                    .complete(
                        &self.pool,
                        Some(envelope.total_matched),
                        Some(envelope.returned),
                        serde_json::json!({ "query_len": req.query.len() }),
                    )
                    .await;
                let serialized = serde_json::to_string(&envelope).unwrap_or_default();
                self.emit_metrics(
                    "find_session",
                    Some(req.query.as_str()),
                    serde_json::json!({
                        "query_len": req.query.len(),
                        "limit_requested": req.limit,
                    }),
                    &stages,
                    call_start.elapsed().as_millis() as u64,
                    envelope.total_matched,
                    envelope.returned,
                    envelope.returned_full_tokens,
                    true,
                    envelope.query_broadened,
                    &serialized,
                )
                .await;
                Ok(Json(envelope))
            }
            Err(e) => {
                let msg = e.to_string();
                let _ = op.complete_err(&self.pool, &msg).await;
                self.emit_error_metrics(
                    "find_session",
                    Some(req.query.as_str()),
                    serde_json::json!({
                        "query_len": req.query.len(),
                        "limit_requested": req.limit,
                    }),
                    &stages,
                    call_start.elapsed().as_millis() as u64,
                    &msg,
                )
                .await;
                Err(format!("find_session: {}", explain_query_error(&msg)))
            }
        }
    }

    /// Recent git commits by authored_at DESC, optionally filtered.
    #[tool(
        name = "recent_commits",
        description = "Recent git commits across indexed repositories, ordered by authored_at DESC. \
                       `filter` is an OPTIONAL FTS5 MATCH expression against commit message. \
                       `repo` is an OPTIONAL substring match against repo_path. \
                       `days` defaults to 30. `limit` defaults to 10, max 50."
    )]
    pub async fn recent_commits(
        &self,
        params: Parameters<RecentCommitsRequest>,
    ) -> Result<Json<ToolEnvelope<CommitResult>>, String> {
        let req = params.0;
        let op = Op::start("tool.recent_commits");
        let call_start = Instant::now();
        let mut stages = Stages::default();
        match run_recent_commits(&self.pool, &req, &mut stages).await {
            Ok(envelope) => {
                let _ = op
                    .complete(
                        &self.pool,
                        Some(envelope.total_matched),
                        Some(envelope.returned),
                        serde_json::json!({
                            "filter_set": req.filter.is_some(),
                            "repo_set": req.repo.is_some(),
                            "days": req.days.unwrap_or(DEFAULT_DAYS),
                        }),
                    )
                    .await;
                let serialized = serde_json::to_string(&envelope).unwrap_or_default();
                self.emit_metrics(
                    "recent_commits",
                    req.filter.as_deref(),
                    serde_json::json!({
                        "filter_set": req.filter.is_some(),
                        "repo_set": req.repo.is_some(),
                        "days": req.days.unwrap_or(DEFAULT_DAYS),
                        "limit_requested": req.limit,
                    }),
                    &stages,
                    call_start.elapsed().as_millis() as u64,
                    envelope.total_matched,
                    envelope.returned,
                    envelope.returned_full_tokens,
                    req.filter.is_some(),
                    envelope.query_broadened,
                    &serialized,
                )
                .await;
                Ok(Json(envelope))
            }
            Err(e) => {
                let msg = e.to_string();
                let _ = op.complete_err(&self.pool, &msg).await;
                self.emit_error_metrics(
                    "recent_commits",
                    req.filter.as_deref(),
                    serde_json::json!({
                        "filter_set": req.filter.is_some(),
                        "repo_set": req.repo.is_some(),
                        "days": req.days.unwrap_or(DEFAULT_DAYS),
                        "limit_requested": req.limit,
                    }),
                    &stages,
                    call_start.elapsed().as_millis() as u64,
                    &msg,
                )
                .await;
                Err(format!("recent_commits: {}", explain_query_error(&msg)))
            }
        }
    }

    /// Git commits ranked by FTS5 relevance, optionally scoped to a repo.
    #[tool(
        name = "find_commit",
        description = "Search git commits by FTS5 relevance over message_summary + message_body. \
                       `query` is a REQUIRED FTS5 MATCH expression. \
                       `repo` is an OPTIONAL substring match against repo_path. \
                       Results ordered by bm25 rank ASC, then authored_at DESC. \
                       `limit` defaults to 10, max 50."
    )]
    pub async fn find_commit(
        &self,
        params: Parameters<FindCommitRequest>,
    ) -> Result<Json<ToolEnvelope<CommitResult>>, String> {
        let req = params.0;
        let op = Op::start("tool.find_commit");
        let call_start = Instant::now();
        let mut stages = Stages::default();
        match run_find_commit(&self.pool, &req, &mut stages).await {
            Ok(envelope) => {
                let _ = op
                    .complete(
                        &self.pool,
                        Some(envelope.total_matched),
                        Some(envelope.returned),
                        serde_json::json!({
                            "query_len": req.query.len(),
                            "repo_set": req.repo.is_some(),
                        }),
                    )
                    .await;
                let serialized = serde_json::to_string(&envelope).unwrap_or_default();
                self.emit_metrics(
                    "find_commit",
                    Some(req.query.as_str()),
                    serde_json::json!({
                        "query_len": req.query.len(),
                        "repo_set": req.repo.is_some(),
                        "limit_requested": req.limit,
                    }),
                    &stages,
                    call_start.elapsed().as_millis() as u64,
                    envelope.total_matched,
                    envelope.returned,
                    envelope.returned_full_tokens,
                    true,
                    envelope.query_broadened,
                    &serialized,
                )
                .await;
                Ok(Json(envelope))
            }
            Err(e) => {
                let msg = e.to_string();
                let _ = op.complete_err(&self.pool, &msg).await;
                self.emit_error_metrics(
                    "find_commit",
                    Some(req.query.as_str()),
                    serde_json::json!({
                        "query_len": req.query.len(),
                        "repo_set": req.repo.is_some(),
                        "limit_requested": req.limit,
                    }),
                    &stages,
                    call_start.elapsed().as_millis() as u64,
                    &msg,
                )
                .await;
                Err(format!("find_commit: {}", explain_query_error(&msg)))
            }
        }
    }

    /// Memory entries ranked by FTS5 relevance.
    #[tool(
        name = "find_memory",
        description = "Search Claude Code memory entries (~/.claude/projects/.../memory/*.md) by FTS5 relevance. \
                       `query` is a REQUIRED FTS5 MATCH expression over description + body. \
                       Results ordered by bm25 rank ASC. `limit` defaults to 10, max 50."
    )]
    pub async fn find_memory(
        &self,
        params: Parameters<FindMemoryRequest>,
    ) -> Result<Json<ToolEnvelope<MemoryResult>>, String> {
        let req = params.0;
        let op = Op::start("tool.find_memory");
        let call_start = Instant::now();
        let mut stages = Stages::default();
        match run_find_memory(&self.pool, &req, &mut stages).await {
            Ok(envelope) => {
                let _ = op
                    .complete(
                        &self.pool,
                        Some(envelope.total_matched),
                        Some(envelope.returned),
                        serde_json::json!({ "query_len": req.query.len() }),
                    )
                    .await;
                let serialized = serde_json::to_string(&envelope).unwrap_or_default();
                self.emit_metrics(
                    "find_memory",
                    Some(req.query.as_str()),
                    serde_json::json!({
                        "query_len": req.query.len(),
                        "limit_requested": req.limit,
                    }),
                    &stages,
                    call_start.elapsed().as_millis() as u64,
                    envelope.total_matched,
                    envelope.returned,
                    envelope.returned_full_tokens,
                    true,
                    envelope.query_broadened,
                    &serialized,
                )
                .await;
                Ok(Json(envelope))
            }
            Err(e) => {
                let msg = e.to_string();
                let _ = op.complete_err(&self.pool, &msg).await;
                self.emit_error_metrics(
                    "find_memory",
                    Some(req.query.as_str()),
                    serde_json::json!({
                        "query_len": req.query.len(),
                        "limit_requested": req.limit,
                    }),
                    &stages,
                    call_start.elapsed().as_millis() as u64,
                    &msg,
                )
                .await;
                Err(format!("find_memory: {}", explain_query_error(&msg)))
            }
        }
    }

    /// Design-doc sections ranked by FTS5 relevance.
    #[tool(
        name = "find_design_doc",
        description = "Search design-doc sections (root-level *.md and docs/**/*.md, split by heading) by FTS5 relevance. \
                       `query` is a REQUIRED FTS5 MATCH expression against section body. \
                       Results ordered by bm25 rank ASC. `limit` defaults to 10, max 50."
    )]
    pub async fn find_design_doc(
        &self,
        params: Parameters<FindDesignDocRequest>,
    ) -> Result<Json<ToolEnvelope<DesignDocResult>>, String> {
        let req = params.0;
        let op = Op::start("tool.find_design_doc");
        let call_start = Instant::now();
        let mut stages = Stages::default();
        match run_find_design_doc(&self.pool, &req, &mut stages).await {
            Ok(envelope) => {
                let _ = op
                    .complete(
                        &self.pool,
                        Some(envelope.total_matched),
                        Some(envelope.returned),
                        serde_json::json!({ "query_len": req.query.len() }),
                    )
                    .await;
                let serialized = serde_json::to_string(&envelope).unwrap_or_default();
                self.emit_metrics(
                    "find_design_doc",
                    Some(req.query.as_str()),
                    serde_json::json!({
                        "query_len": req.query.len(),
                        "limit_requested": req.limit,
                    }),
                    &stages,
                    call_start.elapsed().as_millis() as u64,
                    envelope.total_matched,
                    envelope.returned,
                    envelope.returned_full_tokens,
                    true,
                    envelope.query_broadened,
                    &serialized,
                )
                .await;
                Ok(Json(envelope))
            }
            Err(e) => {
                let msg = e.to_string();
                let _ = op.complete_err(&self.pool, &msg).await;
                self.emit_error_metrics(
                    "find_design_doc",
                    Some(req.query.as_str()),
                    serde_json::json!({
                        "query_len": req.query.len(),
                        "limit_requested": req.limit,
                    }),
                    &stages,
                    call_start.elapsed().as_millis() as u64,
                    &msg,
                )
                .await;
                Err(format!("find_design_doc: {}", explain_query_error(&msg)))
            }
        }
    }

    /// Source-code chunks ranked by FTS5 relevance, each carrying its provenance commit.
    #[tool(
        name = "find_code",
        description = "Search indexed source code (git-tracked files, fixed line-window chunks) \
                       by FTS5 relevance. `query` is a REQUIRED FTS5 MATCH expression against \
                       code body — raw FTS5 syntax, not natural language, so quote any term \
                       containing punctuation (\"parse_config(\"). Each hit returns repo_path + \
                       path + line range + a bounded body excerpt AND the commit that last \
                       touched it (the code↔commit→design/session provenance join). `path` is \
                       REPO-RELATIVE, so open a hit as repo_path + path. `repo` is an OPTIONAL \
                       scope: a substring of the repo's absolute path (e.g. \"nibdex\"), the \
                       same repo string find_commit/recent_commits take. Results ordered by \
                       bm25 rank ASC. `limit` defaults to 10, max 50."
    )]
    pub async fn find_code(
        &self,
        params: Parameters<FindCodeRequest>,
    ) -> Result<Json<ToolEnvelope<CodeResult>>, String> {
        let req = params.0;
        let op = Op::start("tool.find_code");
        let call_start = Instant::now();
        let mut stages = Stages::default();
        match run_find_code(&self.pool, &req, &mut stages).await {
            Ok(envelope) => {
                let _ = op
                    .complete(
                        &self.pool,
                        Some(envelope.total_matched),
                        Some(envelope.returned),
                        serde_json::json!({ "query_len": req.query.len() }),
                    )
                    .await;
                let serialized = serde_json::to_string(&envelope).unwrap_or_default();
                self.emit_metrics(
                    "find_code",
                    Some(req.query.as_str()),
                    serde_json::json!({
                        "query_len": req.query.len(),
                        "limit_requested": req.limit,
                    }),
                    &stages,
                    call_start.elapsed().as_millis() as u64,
                    envelope.total_matched,
                    envelope.returned,
                    envelope.returned_full_tokens,
                    true,
                    envelope.query_broadened,
                    &serialized,
                )
                .await;
                Ok(Json(envelope))
            }
            Err(e) => {
                let msg = e.to_string();
                let _ = op.complete_err(&self.pool, &msg).await;
                self.emit_error_metrics(
                    "find_code",
                    Some(req.query.as_str()),
                    serde_json::json!({
                        "query_len": req.query.len(),
                        "limit_requested": req.limit,
                    }),
                    &stages,
                    call_start.elapsed().as_millis() as u64,
                    &msg,
                )
                .await;
                Err(format!("find_code: {}", explain_query_error(&msg)))
            }
        }
    }

    /// Index health snapshot — counts, orphans, perf percentiles, last-run extractor times.
    #[tool(
        name = "check",
        description = "Index health snapshot: document and entry counts, orphan classes, shallow repos, \
                       per-tool p50/p95 latency over the last hour, last extractor run times, and file-watcher state. \
                       Returns the D-6.3.3 schema_version=1 envelope. Orphan counts are computed live from the \
                       index — a class is orphaned when its parent document's source file (or repo) no longer exists on disk \
                       (memory, design-doc, source, session_entries, indexed_repos)."
    )]
    pub async fn check(
        &self,
        _params: Parameters<CheckRequest>,
    ) -> Result<Json<CheckResult>, String> {
        let op = Op::start("tool.check");
        let call_start = Instant::now();
        let mut stages = Stages::default();
        let uptime_s = self.started_at.elapsed().as_secs() as i64;
        match run_check(
            &self.pool,
            uptime_s,
            self.calibration.as_deref(),
            &mut stages,
        )
        .await
        {
            Ok(result) => {
                let _ = op
                    .complete(
                        &self.pool,
                        None,
                        None,
                        serde_json::json!({
                            "search_index_total": result.indexer.search_index_total,
                            "shallow_repo_count": result.shallow_repos.len(),
                        }),
                    )
                    .await;
                let serialized = serde_json::to_string(&result).unwrap_or_default();
                // `check()` has no FTS5 path + no envelope-style total_matched/returned;
                // candidate_count goes to {fts5: 0, after_rank: 0} via had_fts5=false +
                // returned=0. D-7.3 captures this as the documented best-fit shape.
                self.emit_metrics(
                    "check",
                    None,
                    serde_json::json!({}),
                    &stages,
                    call_start.elapsed().as_millis() as u64,
                    0,
                    0,
                    0,
                    false,
                    false,
                    &serialized,
                )
                .await;
                Ok(Json(result))
            }
            Err(e) => {
                let msg = e.to_string();
                let _ = op.complete_err(&self.pool, &msg).await;
                self.emit_error_metrics(
                    "check",
                    None,
                    serde_json::json!({}),
                    &stages,
                    call_start.elapsed().as_millis() as u64,
                    &msg,
                )
                .await;
                Err(format!("check: {msg}"))
            }
        }
    }
}

#[tool_handler(
    router = self.tool_router,
    name = "nibdex",
    instructions = "Derived MCP index over a workspace's source code, git commits, design docs, memory, and AI session history. \
                    Query tools take FTS5 MATCH expressions (raw FTS5 syntax, not natural language). \
                    `recent_*` tools order by recency; `find_*` tools order by FTS5 bm25 rank. \
                    `check()` returns index health and per-tool latency percentiles."
)]
impl ServerHandler for NibdexServer {}

/// Drive the MCP server over stdio until the client disconnects (DESIGN D-6.4.1).
pub async fn serve_stdio(
    pool: SqlitePool,
    metrics_sink: Option<Arc<MetricsSink>>,
    calibration: Option<Arc<CalibrationModel>>,
) -> Result<()> {
    let server = NibdexServer::new(pool, Instant::now(), metrics_sink, calibration);
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
