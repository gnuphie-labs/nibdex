// SPDX-License-Identifier: MIT

//! Op timer — generic per-operation perf ledger.
//!
//! One primitive serves indexer batch ops today and rmcp tool handlers Day 6.
//! Captures latency (Instant deltas) + RSS delta (memory-stats crate, one syscall, cross-platform).
//!
//! Usage:
//! ```ignore
//! let op = Op::start("indexer.full_scan");
//! // ... do work ...
//! op.complete(&pool, rows_in, rows_out, extra_json).await?;
//! ```
//!
//! Memory caveat: RSS deltas in a multi-threaded async runtime conflate concurrent
//! allocations from other tasks. Useful for trend detection across runs, not for
//! per-call attribution under load. JSONL sink + per-thread tracking can replace
//! this when §5.5 lands (Day 7) if precision becomes load-bearing.

use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde_json::Value;
use sqlx::SqlitePool;

pub struct Op {
    op_name: String,
    started_at_unix: i64,
    started_at_instant: Instant,
    rss_before_bytes: Option<usize>,
}

impl Op {
    pub fn start(op_name: impl Into<String>) -> Self {
        let started_at_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        Self {
            op_name: op_name.into(),
            started_at_unix,
            started_at_instant: Instant::now(),
            rss_before_bytes: memory_stats::memory_stats().map(|m| m.physical_mem),
        }
    }

    pub async fn complete(
        self,
        pool: &SqlitePool,
        rows_in: Option<i64>,
        rows_out: Option<i64>,
        extra: Value,
    ) -> Result<()> {
        self.persist(pool, rows_in, rows_out, extra, None).await
    }

    #[allow(dead_code)]
    pub async fn complete_err(self, pool: &SqlitePool, error: &str) -> Result<()> {
        self.persist(
            pool,
            None,
            None,
            Value::Object(Default::default()),
            Some(error),
        )
        .await
    }

    async fn persist(
        self,
        pool: &SqlitePool,
        rows_in: Option<i64>,
        rows_out: Option<i64>,
        extra: Value,
        error: Option<&str>,
    ) -> Result<()> {
        let duration_ms = self.started_at_instant.elapsed().as_millis() as i64;
        let rss_after = memory_stats::memory_stats().map(|m| m.physical_mem);
        let rss_delta: Option<i64> = match (self.rss_before_bytes, rss_after) {
            (Some(b), Some(a)) => Some(a as i64 - b as i64),
            _ => None,
        };
        let rss_after_bytes: Option<i64> = rss_after.map(|v| v as i64);
        let extra_str = serde_json::to_string(&extra)?;

        sqlx::query(
            r#"
            INSERT INTO op_measurements
                (op_name, started_at, duration_ms, rss_delta_bytes, rss_after_bytes,
                 rows_in, rows_out, error, extra_json)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(&self.op_name)
        .bind(self.started_at_unix)
        .bind(duration_ms)
        .bind(rss_delta)
        .bind(rss_after_bytes)
        .bind(rows_in)
        .bind(rows_out)
        .bind(error)
        .bind(extra_str)
        .execute(pool)
        .await?;

        Ok(())
    }
}

/// Row shape returned from `op_measurements` queries: (started_at, duration_ms,
/// rss_delta_bytes, rss_after_bytes, rows_in, rows_out, extra_json).
type MeasurementRow = (i64, i64, Option<i64>, Option<i64>, Option<i64>, Option<i64>, String);

/// Most recent prior measurement for the given op_name at time of call, or None.
/// Call this BEFORE starting a new run so it returns the previous one (not yours).
pub async fn last_measurement(pool: &SqlitePool, op_name: &str) -> Result<Option<Measurement>> {
    let row: Option<MeasurementRow> = sqlx::query_as(
        r#"
        SELECT started_at, duration_ms, rss_delta_bytes, rss_after_bytes, rows_in, rows_out, extra_json
        FROM op_measurements
        WHERE op_name = ? AND error IS NULL
        ORDER BY started_at DESC, id DESC
        LIMIT 1
        "#,
    )
    .bind(op_name)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(
        |(
            started_at,
            duration_ms,
            rss_delta_bytes,
            rss_after_bytes,
            rows_in,
            rows_out,
            extra_json,
        )| {
            Measurement {
                started_at,
                duration_ms,
                rss_delta_bytes,
                rss_after_bytes,
                rows_in,
                rows_out,
                extra_json,
            }
        },
    ))
}

#[derive(Debug, Clone)]
pub struct Measurement {
    // Surfaced for future "ran N seconds ago" rendering; not yet printed.
    #[allow(dead_code)]
    pub started_at: i64,
    pub duration_ms: i64,
    pub rss_delta_bytes: Option<i64>,
    pub rss_after_bytes: Option<i64>,
    pub rows_in: Option<i64>,
    pub rows_out: Option<i64>,
    pub extra_json: String,
}
