// SPDX-License-Identifier: MIT

//! MCP server surface — DESIGN §5.1 + design-pass D-6.1 / D-6.3 contracts.
//!
//! Day 6 commit 1: walking-skeleton — `recent_sessions` only.
//! Day 6 commit 2: remaining 5 query tools (`find_session`, `recent_commits`,
//!   `find_commit`, `find_memory`, `find_design_doc`) + `check()` shape.
//!   Orphan-class computation lands commit 3; file-watcher commit 4.
//!
//! Transport: stdio (D-6.4.1). File-watcher daemon lives outside this module.

use std::sync::Arc;
use std::time::Instant;

use rmcp::handler::server::router::tool::ToolRouter;
use sqlx::SqlitePool;

use crate::calibration::CalibrationModel;
use crate::metrics_sink::MetricsSink;

mod check;
mod format;
mod fts5;
mod neighbourhood;
mod query;
mod server;
mod telemetry;
mod types;

#[cfg(test)]
mod tests;

// Re-exports preserving the `crate::mcp::` surface (§5): `http_server` imports
// `IndexerCounts`/`OrphanCounts`/`Stages`/`run_check`; `main` imports `serve_stdio`,
// and the CLI `find-code` arm imports `sanitize_fts5_query` so the human-typed
// quickfix path gets the same FTS5-syntax safety as the MCP tools (a raw hyphen
// query crashes bare MATCH — the §9.1 measurement gear's bonus finding 2).
pub(crate) use self::check::{compute_indexer_counts, run_check};
pub(crate) use self::fts5::sanitize_fts5_query;
pub(crate) use self::server::serve_stdio;
pub(crate) use self::types::{CheckResult, IndexerCounts, OrphanCounts, Stages};
// `FileWatcherStats` is named only by the metrics-export drift test (to build a
// fully-populated `CheckResult` sample); scrub code reaches its fields through
// `CheckResult.file_watcher` without naming the type.
#[cfg(test)]
pub(crate) use self::types::{Adoption, FileWatcherStats, RetiredCorpus};

#[derive(Clone)]
pub struct NibdexServer {
    pool: SqlitePool,
    tool_router: ToolRouter<Self>,
    started_at: Instant,
    /// §5.5 Layer 1 sink. `None` when telemetry is disabled. Every
    /// successful tool call emits one `MetricsEvent` line via
    /// `emit_metrics`. Failure paths skip emission per D-7.6.
    metrics_sink: Option<Arc<MetricsSink>>,
    /// §8.4 Layer 2 calibration model. `None` when `calibration.toml`
    /// was missing at startup. When `Some`, every successful tool call
    /// emits 6 additive Layer-2 fields on the v1 envelope (D-8.7) AND
    /// writes one `cost_ledger_events` row (D-8.4) consumed by
    /// `check()`'s `cost_savings` rollup (D-8.6).
    calibration: Option<Arc<CalibrationModel>>,
}
