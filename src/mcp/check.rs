// SPDX-License-Identifier: MIT

//! `check()` health surface (D-6.3.3): index/entry counts, orphan detection,
//! shallow-repo list, per-tool latency percentiles, extractor last-run times,
//! and file-watcher state. Relocated from `mcp.rs` by gh#6
//! (see `docs/MCP_SPLIT_PLAN.md`).

use std::collections::{BTreeMap, HashSet};
use std::time::Instant;

use anyhow::Result;
use sqlx::{QueryBuilder, SqlitePool};

use crate::calibration::CalibrationModel;
use crate::cost_ledger;
use crate::extractor::session_history;
use crate::watcher;

use super::format::percentile;
use super::types::{
    CHECK_SCHEMA_VERSION, CheckResult, FileWatcherStats, IndexerCounts, OrphanCounts,
    Adoption, PERF_WINDOW_SECS, RetiredCorpus, Stages,
};

pub async fn run_check(
    pool: &SqlitePool,
    uptime_s: i64,
    calibration: Option<&CalibrationModel>,
    stages: &mut Stages,
) -> Result<CheckResult> {
    // D-7.3: check() has no FTS5/rank/join phases; the entire run
    // lands in `shape_response_ms` as documented best-fit.
    let t = Instant::now();
    let indexer = compute_indexer_counts(pool).await?;
    let orphans = compute_orphans(pool).await?;
    let shallow_repos = fetch_shallow_repos(pool).await?;
    let (perf_p50_ms, perf_p95_ms) = compute_tool_percentiles(pool, PERF_WINDOW_SECS).await?;
    let extractors_last_run_ms = compute_extractors_last_run(pool).await?;
    let file_watcher = read_file_watcher_stats(pool).await?;
    // §8.4 Layer 2 rollup: when calibration model is loaded, aggregate
    // the ledger over 1d/7d/30d windows + per-tool. Absent calibration
    // → None → `cost_savings` omitted from the JSON envelope.
    let cost_savings = match calibration {
        Some(model) => Some(cost_ledger::aggregate_ledger(pool, model.model.version.clone()).await?),
        None => None,
    };

    let retired_corpora = retired_corpora(&indexer);
    let adoption = compute_adoption(pool).await?;

    let result = CheckResult {
        schema_version: CHECK_SCHEMA_VERSION,
        daemon_uptime_s: uptime_s,
        indexer,
        orphans,
        shallow_repos,
        perf_p50_ms,
        perf_p95_ms,
        file_watcher,
        extractors_last_run_ms,
        cost_savings,
        build: crate::build_info::build_info(),
        retired_corpora,
        adoption,
    };
    stages.shape_response_ms = t.elapsed().as_millis() as u64;
    Ok(result)
}

/// Name the corpora whose counts are deliberately dead, so a reader does not
/// take them for damage.
///
/// Only reported when rows actually survive — a workspace that never had a
/// CLAUDE.md-format session log gets a clean `check()` with no archaeology in it.
fn retired_corpora(indexer: &IndexerCounts) -> Option<Vec<RetiredCorpus>> {
    let mut out = Vec::new();
    if indexer.session_entries > 0 {
        out.push(RetiredCorpus {
            corpus: "session_entries".to_string(),
            rows: indexer.session_entries,
            superseded_by: "session_edges".to_string(),
        });
    }
    (!out.is_empty()).then_some(out)
}

/// The denominator (see [`Adoption`]). `None` when no session activity has been
/// indexed, so a workspace that has never run the session pass gets a clean
/// `check()` rather than a misleading 0%.
async fn compute_adoption(pool: &SqlitePool) -> Result<Option<Adoption>> {
    let row: Option<(i64, i64, i64, i64)> = sqlx::query_as(
        "SELECT COUNT(*), \
                COALESCE(SUM(nibdex_calls > 0), 0), \
                COALESCE(SUM(retrieval_calls), 0), \
                COALESCE(SUM(nibdex_calls), 0) \
         FROM session_activity",
    )
    .fetch_optional(pool)
    .await?;
    let Some((sessions_seen, sessions_using_nibdex, retrieval_elsewhere, nibdex_queries)) = row
    else {
        return Ok(None);
    };
    if sessions_seen == 0 {
        return Ok(None);
    }
    let total = retrieval_elsewhere + nibdex_queries;
    Ok(Some(Adoption {
        sessions_seen,
        sessions_using_nibdex,
        retrieval_elsewhere,
        nibdex_queries,
        // Rounded to one decimal so a tiny share reads as a tiny share rather
        // than disappearing into 0.
        nibdex_share_pct: if total == 0 {
            0.0
        } else {
            (nibdex_queries as f64 * 1000.0 / total as f64).round() / 10.0
        },
    }))
}

async fn read_file_watcher_stats(pool: &SqlitePool) -> Result<Option<FileWatcherStats>> {
    // D-6.4.1: stdio MCP runs in its own process, so the watcher's runtime
    // state must live in the DB. `watcher::read_live_state` enforces the
    // 60s heartbeat liveness gate — a stale row (watcher crashed) surfaces
    // here as None and `file_watcher: null` lands in the envelope.
    let live = watcher::read_live_state(pool).await?;
    Ok(live.map(|row| FileWatcherStats {
        events_total: row.events_total,
        events_coalesced_total: row.events_coalesced_total,
        last_event_ts: row.last_event_ts,
        subscriptions: row.subscriptions,
    }))
}

async fn compute_indexer_counts(pool: &SqlitePool) -> Result<IndexerCounts> {
    let doc_rows: Vec<(String, i64)> =
        sqlx::query_as("SELECT kind, COUNT(*) FROM documents GROUP BY kind ORDER BY kind")
            .fetch_all(pool)
            .await?;
    let documents: BTreeMap<String, i64> = doc_rows.into_iter().collect();

    let (session_entries,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM session_entries")
        .fetch_one(pool)
        .await?;
    let (session_edges,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM session_edges")
        .fetch_one(pool)
        .await?;
    let (memory_entries,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM memory_entries")
        .fetch_one(pool)
        .await?;
    let (design_doc_sections,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM design_doc_sections")
        .fetch_one(pool)
        .await?;
    let (source_chunks,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM source_chunks")
        .fetch_one(pool)
        .await?;
    let (commit_entries,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM commit_entries")
        .fetch_one(pool)
        .await?;
    let (indexed_repos,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM indexed_repos")
        .fetch_one(pool)
        .await?;
    let (search_index_total,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM search_index")
        .fetch_one(pool)
        .await?;

    Ok(IndexerCounts {
        documents,
        session_entries,
        session_edges,
        memory_entries,
        design_doc_sections,
        source_chunks,
        commit_entries,
        indexed_repos,
        search_index_total,
    })
}

async fn fetch_shallow_repos(pool: &SqlitePool) -> Result<Vec<String>> {
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT repo_path FROM indexed_repos WHERE is_shallow = 1 ORDER BY repo_path",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(p,)| p).collect())
}

// =====================================================================================
// Orphan computation (D-6.3.1)
// =====================================================================================

async fn compute_orphans(pool: &SqlitePool) -> Result<OrphanCounts> {
    Ok(OrphanCounts {
        session_entries: compute_session_orphans(pool).await?,
        memory_entries: compute_orphans_by_missing_doc(pool, "memory", OrphanChild::Memory).await?,
        design_doc_sections: compute_orphans_by_missing_doc(
            pool,
            "design_doc",
            OrphanChild::DesignSection,
        )
        .await?,
        source_chunks: compute_orphans_by_missing_doc(pool, "source", OrphanChild::SourceChunk)
            .await?,
        indexed_repos: compute_repo_orphans(pool).await?,
    })
}

async fn compute_session_orphans(pool: &SqlitePool) -> Result<i64> {
    // Locate the (at most one) session_history document. If absent, surface any
    // session_entries that survived its delete as orphans — should be zero under
    // ON DELETE CASCADE, defensive otherwise.
    let doc: Option<(i64, String)> =
        sqlx::query_as("SELECT id, path FROM documents WHERE kind = 'session_history' LIMIT 1")
            .fetch_optional(pool)
            .await?;
    let Some((document_id, path)) = doc else {
        let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM session_entries")
            .fetch_one(pool)
            .await?;
        return Ok(n);
    };

    // If the source file is unreadable we can't compute the diff — every DB row
    // is orphaned in the sense that its source is gone.
    let content = match tokio::fs::read_to_string(&path).await {
        Ok(s) => s,
        Err(_) => {
            let (n,): (i64,) =
                sqlx::query_as("SELECT COUNT(*) FROM session_entries WHERE document_id = ?")
                    .bind(document_id)
                    .fetch_one(pool)
                    .await?;
            return Ok(n);
        }
    };

    let live: HashSet<i64> = session_history::extract_session_numbers(&content)
        .into_iter()
        .collect();

    let db_numbers: Vec<(i64,)> =
        sqlx::query_as("SELECT session_number FROM session_entries WHERE document_id = ?")
            .bind(document_id)
            .fetch_all(pool)
            .await?;
    Ok(db_numbers.iter().filter(|(n,)| !live.contains(n)).count() as i64)
}

/// Which child-table count to aggregate after identifying missing parent docs.
#[derive(Copy, Clone)]
enum OrphanChild {
    Memory,
    DesignSection,
    SourceChunk,
}

async fn compute_orphans_by_missing_doc(
    pool: &SqlitePool,
    doc_kind: &str,
    child: OrphanChild,
) -> Result<i64> {
    let docs: Vec<(i64, String)> = sqlx::query_as("SELECT id, path FROM documents WHERE kind = ?")
        .bind(doc_kind)
        .fetch_all(pool)
        .await?;

    let missing_ids: Vec<i64> = docs
        .into_iter()
        .filter(|(_, p)| std::fs::metadata(p).is_err())
        .map(|(id, _)| id)
        .collect();
    if missing_ids.is_empty() {
        return Ok(0);
    }

    // Table name is a literal per `child` discriminant — no dynamic injection.
    let table = match child {
        OrphanChild::Memory => "memory_entries",
        OrphanChild::DesignSection => "design_doc_sections",
        OrphanChild::SourceChunk => "source_chunks",
    };
    let sql = format!("SELECT COUNT(*) FROM {table} WHERE document_id IN (");
    let mut qb: QueryBuilder<'_, sqlx::Sqlite> = QueryBuilder::new(sql);
    let mut sep = qb.separated(", ");
    for id in &missing_ids {
        sep.push_bind(*id);
    }
    qb.push(")");
    let (n,): (i64,) = qb.build_query_as().fetch_one(pool).await?;
    Ok(n)
}

async fn compute_repo_orphans(pool: &SqlitePool) -> Result<i64> {
    let rows: Vec<(String,)> = sqlx::query_as("SELECT repo_path FROM indexed_repos")
        .fetch_all(pool)
        .await?;
    Ok(rows
        .into_iter()
        .filter(|(p,)| !std::path::Path::new(p).is_dir())
        .count() as i64)
}

async fn compute_tool_percentiles(
    pool: &SqlitePool,
    window_secs: i64,
) -> Result<(BTreeMap<String, i64>, BTreeMap<String, i64>)> {
    let cutoff: i64 = sqlx::query_scalar("SELECT CAST(strftime('%s','now') AS INTEGER) - ?")
        .bind(window_secs)
        .fetch_one(pool)
        .await?;

    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT op_name, duration_ms \
         FROM op_measurements \
         WHERE op_name LIKE 'tool.%' \
           AND error IS NULL \
           AND started_at >= ? \
         ORDER BY op_name",
    )
    .bind(cutoff)
    .fetch_all(pool)
    .await?;

    let mut grouped: BTreeMap<String, Vec<i64>> = BTreeMap::new();
    for (op, ms) in rows {
        grouped.entry(op).or_default().push(ms);
    }

    let mut p50 = BTreeMap::new();
    let mut p95 = BTreeMap::new();
    for (op, mut durations) in grouped {
        if durations.is_empty() {
            continue;
        }
        durations.sort_unstable();
        p50.insert(op.clone(), percentile(&durations, 0.50));
        p95.insert(op, percentile(&durations, 0.95));
    }
    Ok((p50, p95))
}

async fn compute_extractors_last_run(pool: &SqlitePool) -> Result<BTreeMap<String, i64>> {
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT op_name, duration_ms FROM op_measurements \
         WHERE id IN ( \
             SELECT MAX(id) FROM op_measurements \
             WHERE op_name LIKE 'extract.%' AND error IS NULL \
             GROUP BY op_name \
         ) \
         ORDER BY op_name",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().collect())
}
