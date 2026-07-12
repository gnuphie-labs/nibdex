// SPDX-License-Identifier: MIT

//! §8.4 Layer 2 — cost-savings ledger writes + rolling-window rollup.
//!
//! Day 8 commit 2 wires this module:
//!
//! - `record_event` is called by `NibdexServer::emit_metrics` on every
//!   successful tool call that carries Layer-2 fields (calibration model
//!   loaded + tool entry resolves). One row per emission per D-8.4.
//! - `aggregate_ledger` is called by `run_check` to populate
//!   `CheckResult.cost_savings`. Three `WHERE ts >= ?` aggregates
//!   (1d/7d/30d) + one per-tool `GROUP BY` per D-8.5.
//!
//! Persistence: SQLite `cost_ledger_events` (migration
//! `20260528000001_cost_ledger.sql`). Append-only — no UPDATE/DELETE
//! paths.
//!
//! Per-row `calibration_model_version` stamping means recalibration
//! (v0.1 → v0.2) does NOT retroactively change historical aggregates.
//! Today's `CheckResult` reports a single `calibration_model_version`
//! string sourced from the currently loaded model — historical-vs-current
//! partitioning is a Phase-2 lift if the user wants it.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{Duration, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::calibration::{CalibrationModel, ToolCounterfactual};
use crate::metrics_sink::MetricsEvent;

/// D-8.6 envelope: `Option<CostSavingsLedger>` sub-block of `CheckResult`.
/// Serialized as `null` when calibration is absent (Layer-1-only mode).
#[derive(Debug, Serialize, JsonSchema)]
pub struct CostSavingsLedger {
    pub calibration_model_version: String,
    pub window_1d: CostSavingsWindow,
    pub window_7d: CostSavingsWindow,
    pub window_30d: CostSavingsWindow,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct CostSavingsWindow {
    pub queries_served: i64,
    pub tokens_returned: i64,
    pub counterfactual_tokens_p50: i64,
    pub tokens_saved_p50: i64,
    pub dollars_saved_p50_usd: f64,
    /// Per-tool breakdown — sorted by tool name (`BTreeMap`) for stable
    /// JSON ordering across runs.
    pub per_tool: BTreeMap<String, CostSavingsPerTool>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct CostSavingsPerTool {
    pub queries: i64,
    pub tokens_saved_p50: i64,
    pub dollars_saved_p50_usd: f64,
}

/// Compute the 6 Layer-2 field values for a tool call. Returns a struct
/// of `Option`s so `NibdexServer::emit_metrics` can fold them into
/// `MetricsEvent` and so a single deterministic helper covers both the
/// emit path and the ledger-insert path.
///
/// `result_token_estimate` mirrors `MetricsEvent.result_token_estimate`
/// (chars÷4 of the serialized envelope, D-7.4). The savings calculation
/// is `counterfactual_p50 - result_token_estimate` and `tokens_saved_p50`
/// is intentionally signed (`i64`) per D-8.4.
///
/// `wall_ms` is the same `wall_ms` populated on the v1 envelope; we
/// echo `counterfactual_wall_ms_p50` from the calibration model
/// unchanged (savings derivation is the consumer's job — Phase 2).
#[derive(Debug, Clone)]
pub struct Layer2Fields {
    pub counterfactual_tokens_p50: u64,
    pub counterfactual_tokens_p95: u64,
    pub tokens_saved_p50: i64,
    pub dollars_saved_p50_usd: f64,
    pub counterfactual_wall_ms_p50: u64,
    pub calibration_model_version: String,
}

impl Layer2Fields {
    /// Compute from a calibration model + observed result token estimate.
    /// Returns `None` when the model has no entry for `tool` (forward-
    /// compat with future tools added to nibdex before the user re-tunes
    /// `calibration.toml`).
    pub fn compute(
        model: &CalibrationModel,
        tool: &str,
        result_token_estimate: u64,
        returned: i64,
    ) -> Option<Self> {
        let entry = model.tool(tool)?;
        Some(Self::from_entry(model, entry, result_token_estimate, returned))
    }

    /// Pure helper: build the Layer-2 numbers from a resolved tool entry.
    /// Split out so unit tests can drive specific p50/p95/rate combinations
    /// without authoring a full TOML round-trip.
    ///
    /// Sentinel: `counterfactual_tokens_p50 == 0` marks an admin tool
    /// (check) per D-8.2 — no AI substitution path, no "what would Claude
    /// have done without nibdex" comparison. The savings calc clamps to
    /// zero in that case so the headline ledger number isn't polluted by
    /// daemon-overhead noise. Honest framing: daemon overhead is measurable
    /// via `daemon_uptime_s` + tool-call rate, not via fake-negative
    /// savings on the admin path. For non-zero counterfactuals, savings
    /// stays signed (a query returning more text than the counterfactual is
    /// a genuine outcome per D-8.4 — preserved here).
    pub fn from_entry(
        model: &CalibrationModel,
        entry: &ToolCounterfactual,
        result_token_estimate: u64,
        returned: i64,
    ) -> Self {
        let anchor_p50 = entry.counterfactual_tokens_p50 as i64;
        let result_i64 = result_token_estimate as i64;

        // v0.2 savings model. We emit the *effective* per-call counterfactual
        // (not the flat anchor) as `counterfactual_tokens_p50`, so for the two
        // common branches the payload reconciles: `saved = counterfactual −
        // result`. A reader (or re-scoring loop) can recompute from the raw
        // inputs (`result_token_estimate`, `after_rank`) + the toml knobs.
        let (effective_p50, tokens_saved_p50) = if entry.counterfactual_tokens_p50 == 0 {
            // Admin tool (check): no AI-substitution path (D-8.2). Keep the
            // explicit-zero sentinel so downstream rollups stay unpolluted.
            (0, 0)
        } else if returned == 0 {
            // Match-gate: a call that returned nothing saved nothing. Credited
            // saving is the `zero_match_savings_tokens` knob (default 0) — kills
            // the pre-v0.2 "found nothing, banked the full anchor" inflation.
            // The displayed counterfactual reconciles (clamped at 0; an
            // aggressive negative wasted-call knob on a tiny empty envelope is
            // the one degenerate case where it floors instead).
            let saved = model.model.zero_match_savings_tokens;
            ((result_i64 + saved).max(0), saved)
        } else {
            // Size-floor: a retrieval can't honestly cost more than the by-hand
            // baseline to obtain the same content. The effective counterfactual
            // is at least `result × recovery_factor + overhead`, so a large but
            // legitimate result floors to the search overhead it saved instead
            // of scoring a loss against the flat average-path anchor.
            let floor = (result_i64 as f64 * model.model.result_recovery_factor).round() as i64
                + model.model.search_overhead_tokens as i64;
            let eff = anchor_p50.max(floor);
            (eff, eff - result_i64)
        };
        // Keep p95 ≥ the (possibly floored) p50 so the pair stays monotone.
        let counterfactual_tokens_p95 = (entry.counterfactual_tokens_p95 as i64)
            .max(effective_p50)
            .max(0) as u64;
        let dollars_saved_p50_usd =
            (tokens_saved_p50 as f64 / 1_000_000.0) * model.model.input_rate_usd_per_mtok;
        Self {
            counterfactual_tokens_p50: effective_p50.max(0) as u64,
            counterfactual_tokens_p95,
            tokens_saved_p50,
            dollars_saved_p50_usd,
            counterfactual_wall_ms_p50: entry.counterfactual_wall_ms_p50,
            calibration_model_version: model.model.version.clone(),
        }
    }
}

/// INSERT one `cost_ledger_events` row from an already-built
/// `MetricsEvent` whose Layer-2 fields are populated. Returns Ok(()) on
/// success; caller (emit_metrics) should log + swallow failures per
/// D-7.6 (metrics are observation, not policy).
///
/// Caller is responsible for skipping the insert when Layer-2 fields are
/// `None` (the event check below is a defensive assertion only).
pub async fn record_event(pool: &SqlitePool, event: &MetricsEvent) -> Result<()> {
    let (Some(cf_p50), Some(cf_p95), Some(saved), Some(dollars), Some(cf_wall), Some(ver)) = (
        event.counterfactual_tokens_p50,
        event.counterfactual_tokens_p95,
        event.tokens_saved_p50,
        event.dollars_saved_p50_usd,
        event.counterfactual_wall_ms_p50,
        event.calibration_model_version.as_ref(),
    ) else {
        anyhow::bail!("cost_ledger::record_event called with Layer-2 fields unset");
    };

    insert_ledger_row(
        pool,
        &event.ts,
        &event.tool,
        event.result_token_estimate as i64,
        cf_p50 as i64,
        cf_p95 as i64,
        saved,
        dollars,
        event.wall_ms as i64,
        cf_wall as i64,
        ver,
    )
    .await
}

/// The single `cost_ledger_events` INSERT, shared by the live emit path
/// (`record_event`) and the startup sink reconcile (`backfill_from_jsonl`)
/// so the two writers can never drift on column order, count, or types.
#[allow(clippy::too_many_arguments)] // 11 columns; an args struct buys nothing
// at two internal call sites that pass them positionally either way.
async fn insert_ledger_row(
    pool: &SqlitePool,
    ts: &str,
    tool: &str,
    result_tokens: i64,
    counterfactual_tokens_p50: i64,
    counterfactual_tokens_p95: i64,
    tokens_saved_p50: i64,
    dollars_saved_p50_usd: f64,
    wall_ms: i64,
    counterfactual_wall_ms_p50: i64,
    calibration_model_version: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO cost_ledger_events (
            ts, tool,
            result_tokens, counterfactual_tokens_p50, counterfactual_tokens_p95,
            tokens_saved_p50, dollars_saved_p50_usd,
            wall_ms, counterfactual_wall_ms_p50,
            calibration_model_version
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(ts)
    .bind(tool)
    .bind(result_tokens)
    .bind(counterfactual_tokens_p50)
    .bind(counterfactual_tokens_p95)
    .bind(tokens_saved_p50)
    .bind(dollars_saved_p50_usd)
    .bind(wall_ms)
    .bind(counterfactual_wall_ms_p50)
    .bind(calibration_model_version)
    .execute(pool)
    .await
    .context("INSERT cost_ledger_events")?;

    Ok(())
}

/// Just the `cost_ledger_events` columns, read back from a durable JSONL
/// metrics sink. A success or `check` row carries all Layer-2 fields; an
/// error row omits them (`None`) and is skipped, mirroring the live
/// `record_event` gate. Every other `MetricsEvent` field (query, params,
/// stages, …) is ignored by serde — this is deliberately NOT `MetricsEvent`
/// (which is `Serialize`-only and IP-laden) so the reconcile reads exactly
/// the non-sensitive ledger projection and nothing more.
#[derive(Debug, Deserialize)]
struct LedgerSinkRow {
    ts: String,
    tool: String,
    #[serde(default)]
    result_token_estimate: i64,
    #[serde(default)]
    wall_ms: i64,
    #[serde(default)]
    counterfactual_tokens_p50: Option<i64>,
    #[serde(default)]
    counterfactual_tokens_p95: Option<i64>,
    #[serde(default)]
    tokens_saved_p50: Option<i64>,
    #[serde(default)]
    dollars_saved_p50_usd: Option<f64>,
    #[serde(default)]
    counterfactual_wall_ms_p50: Option<i64>,
    #[serde(default)]
    calibration_model_version: Option<String>,
}

/// Outcome of a startup sink→ledger reconcile. The invariant
/// `inserted + skipped_existing + skipped_no_layer2 + parse_errors ==
/// scanned` holds (blank lines are not scanned). All-`usize` so the caller
/// can log a one-line summary and tests can assert exact counts.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct BackfillReport {
    pub scanned: usize,
    pub inserted: usize,
    pub skipped_existing: usize,
    pub skipped_no_layer2: usize,
    pub parse_errors: usize,
}

/// Reconcile `cost_ledger_events` from a durable JSONL metrics sink.
///
/// A JSONL sink survives a DB rebuild; the ledger does not (a fresh
/// `nibdex.db` starts with the table empty), so after a rebuild
/// `check().cost_savings` silently under-reports until new traffic re-fills
/// it. Run at daemon startup, this re-derives the ledger from the sink's
/// append-only event log so the rolling-window rollup reflects the full
/// history again — the sink is the durable event log, the ledger a
/// queryable projection of it.
///
/// Idempotent: an event is inserted only when its `(ts, tool)` pair is not
/// already in the ledger, so a normal restart (ledger already current) is a
/// no-op and a re-run never double-counts. `ts` is RFC3339-millis and a
/// single tool is not called twice within one millisecond in practice, so
/// the only loss is a pathological same-ms same-tool pair — harmless to the
/// aggregates. Error-path rows (no Layer-2 fields) are skipped. A missing
/// sink file or an unparseable line is tolerated (best-effort observability,
/// never a startup blocker — D-7.6).
pub async fn backfill_from_jsonl(pool: &SqlitePool, path: &Path) -> Result<BackfillReport> {
    let mut report = BackfillReport::default();

    let contents = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(report),
        Err(e) => {
            return Err(anyhow::Error::from(e)
                .context(format!("reading metrics sink {}", path.display())));
        }
    };

    // Pre-load existing (ts, tool) keys so the reconcile is idempotent
    // without a UNIQUE constraint (the ledger is append-only by design and
    // the live path carries no natural key to dedup on). The set also
    // guards against duplicate lines within a single sink file.
    let existing: Vec<(String, String)> =
        sqlx::query_as("SELECT ts, tool FROM cost_ledger_events")
            .fetch_all(pool)
            .await
            .context("loading existing cost_ledger_events keys")?;
    let mut seen: HashSet<(String, String)> = existing.into_iter().collect();

    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        report.scanned += 1;

        let row: LedgerSinkRow = match serde_json::from_str(line) {
            Ok(r) => r,
            Err(_) => {
                report.parse_errors += 1;
                continue;
            }
        };

        // Same gate as `record_event`: only Layer-2-complete rows belong in
        // the ledger. Error rows omit these fields entirely (→ `None`).
        let (Some(cf_p50), Some(cf_p95), Some(saved), Some(dollars), Some(cf_wall), Some(ver)) = (
            row.counterfactual_tokens_p50,
            row.counterfactual_tokens_p95,
            row.tokens_saved_p50,
            row.dollars_saved_p50_usd,
            row.counterfactual_wall_ms_p50,
            row.calibration_model_version.as_ref(),
        ) else {
            report.skipped_no_layer2 += 1;
            continue;
        };

        let key = (row.ts.clone(), row.tool.clone());
        if seen.contains(&key) {
            report.skipped_existing += 1;
            continue;
        }

        insert_ledger_row(
            pool,
            &row.ts,
            &row.tool,
            row.result_token_estimate,
            cf_p50,
            cf_p95,
            saved,
            dollars,
            row.wall_ms,
            cf_wall,
            ver,
        )
        .await?;
        seen.insert(key);
        report.inserted += 1;
    }

    Ok(report)
}

/// Aggregate the ledger over 1d/7d/30d windows + per-tool. Called by
/// `run_check` to build `CheckResult.cost_savings`.
///
/// Window cut: `Utc::now() - Duration::days(N)`, formatted as RFC3339
/// millis to match `MetricsEvent.ts` formatting (lexicographic string
/// comparison on RFC3339 yields correct chronological ordering, D-8.5).
pub async fn aggregate_ledger(
    pool: &SqlitePool,
    calibration_model_version: String,
) -> Result<CostSavingsLedger> {
    Ok(CostSavingsLedger {
        calibration_model_version,
        window_1d: aggregate_window(pool, 1).await?,
        window_7d: aggregate_window(pool, 7).await?,
        window_30d: aggregate_window(pool, 30).await?,
    })
}

async fn aggregate_window(pool: &SqlitePool, days: i64) -> Result<CostSavingsWindow> {
    let cutoff = (Utc::now() - Duration::days(days))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);

    let row: (i64, i64, i64, i64, f64) = sqlx::query_as(
        "SELECT
            COUNT(*),
            COALESCE(SUM(result_tokens), 0),
            COALESCE(SUM(counterfactual_tokens_p50), 0),
            COALESCE(SUM(tokens_saved_p50), 0),
            COALESCE(SUM(dollars_saved_p50_usd), 0.0)
        FROM cost_ledger_events
        WHERE ts >= ?",
    )
    .bind(&cutoff)
    .fetch_one(pool)
    .await
    .context("aggregate cost_ledger_events window")?;

    let per_tool_rows: Vec<(String, i64, i64, f64)> = sqlx::query_as(
        "SELECT
            tool,
            COUNT(*),
            COALESCE(SUM(tokens_saved_p50), 0),
            COALESCE(SUM(dollars_saved_p50_usd), 0.0)
        FROM cost_ledger_events
        WHERE ts >= ?
        GROUP BY tool",
    )
    .bind(&cutoff)
    .fetch_all(pool)
    .await
    .context("aggregate cost_ledger_events per-tool")?;

    let per_tool: BTreeMap<String, CostSavingsPerTool> = per_tool_rows
        .into_iter()
        .map(|(tool, queries, tokens_saved_p50, dollars_saved_p50_usd)| {
            (
                tool,
                CostSavingsPerTool {
                    queries,
                    tokens_saved_p50,
                    dollars_saved_p50_usd,
                },
            )
        })
        .collect();

    Ok(CostSavingsWindow {
        queries_served: row.0,
        tokens_returned: row.1,
        counterfactual_tokens_p50: row.2,
        tokens_saved_p50: row.3,
        dollars_saved_p50_usd: row.4,
        per_tool,
    })
}

// =====================================================================================
// Tests — Day 8 commit 2.
// =====================================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
    use std::str::FromStr;

    async fn fresh_pool() -> SqlitePool {
        let options = SqliteConnectOptions::from_str("sqlite::memory:")
            .unwrap()
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Memory);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    fn sample_model() -> CalibrationModel {
        use crate::calibration::{ModelMetadata, ToolCounterfactual};
        let mut tools = BTreeMap::new();
        tools.insert(
            "find_session".to_string(),
            ToolCounterfactual {
                counterfactual_tokens_p50: 60_000,
                counterfactual_tokens_p95: 85_000,
                counterfactual_wall_ms_p50: 8_500,
            },
        );
        tools.insert(
            "check".to_string(),
            ToolCounterfactual::default(),
        );
        CalibrationModel {
            model: ModelMetadata {
                version: "v0.1-test".to_string(),
                generated_from: "unit test".to_string(),
                input_rate_usd_per_mtok: 3.0,
                // No-op knobs: floor never dominates when result << anchor,
                // so the existing anchor−result assertions below stay valid.
                search_overhead_tokens: 0,
                result_recovery_factor: 1.0,
                zero_match_savings_tokens: 0,
                ai_read_ms_per_ktok: 0.0,
                roundtrip_ms_per_call: 0.0,
            },
            tools,
        }
    }

    fn event_with_layer2(tool: &str, ts: &str, fields: &Layer2Fields, result_tokens: u64, wall_ms: u64) -> MetricsEvent {
        MetricsEvent {
            schema_version: 1,
            ts: ts.to_string(),
            tool: tool.to_string(),
            query: None,
            params: serde_json::json!({}),
            wall_ms,
            stages_ms: serde_json::json!({"fts5_query":0,"rank":0,"join":0,"shape_response":0}),
            candidate_count: serde_json::json!({"fts5":0,"after_rank":0}),
            result_token_estimate: result_tokens,
            cache_hit: None,
            daemon_uptime_s: 0,
            calibration_confidence: "estimated",
            counterfactual_tokens_p50: Some(fields.counterfactual_tokens_p50),
            counterfactual_tokens_p95: Some(fields.counterfactual_tokens_p95),
            tokens_saved_p50: Some(fields.tokens_saved_p50),
            dollars_saved_p50_usd: Some(fields.dollars_saved_p50_usd),
            counterfactual_wall_ms_p50: Some(fields.counterfactual_wall_ms_p50),
            calibration_model_version: Some(fields.calibration_model_version.clone()),
            query_broadened: None,
            returned_full_tokens: None,
            outcome: None,
            error: None,
        }
    }

    /// G — `Layer2Fields::compute` returns expected values for a known
    /// tool entry. Covers commit-2 gate "layer-2 derivation correctness"
    /// + tokens_saved_p50 = counterfactual_p50 - result_tokens.
    #[test]
    fn layer2_compute_derives_savings_correctly() {
        let model = sample_model();
        // returned > 0 → retrieval branch; result (500) << anchor (60k) so the
        // floor is a no-op and savings is the plain anchor−result.
        let fields = Layer2Fields::compute(&model, "find_session", 500, 5).unwrap();
        assert_eq!(fields.counterfactual_tokens_p50, 60_000);
        assert_eq!(fields.counterfactual_tokens_p95, 85_000);
        assert_eq!(fields.tokens_saved_p50, 60_000 - 500);
        // (59_500 / 1_000_000) * 3.0 = 0.1785
        assert!((fields.dollars_saved_p50_usd - 0.1785).abs() < 1e-9);
        assert_eq!(fields.counterfactual_wall_ms_p50, 8_500);
        assert_eq!(fields.calibration_model_version, "v0.1-test");

        // Unknown tool → None (forward-compat per D-8.8).
        assert!(Layer2Fields::compute(&model, "does_not_exist", 100, 5).is_none());
    }

    /// G — `Layer2Fields::compute` on a zero-counterfactual entry (the
    /// `check()` admin-tool sentinel from D-8.2) always reports zero
    /// savings, even when the tool returns a non-zero envelope. Backstops
    /// G8.6 at the unit-test layer: admin tools cannot pollute the headline
    /// savings number with daemon-overhead noise. Non-zero counterfactual
    /// branch preserves the signed-savings honesty signal — covered
    /// separately in `layer2_compute_derives_savings_correctly`.
    #[test]
    fn layer2_zero_counterfactual_clamps_savings_to_zero() {
        let model = sample_model();
        // check() entry is all zeros per seed.
        let fields = Layer2Fields::compute(&model, "check", 0, 0).unwrap();
        assert_eq!(fields.counterfactual_tokens_p50, 0);
        assert_eq!(fields.tokens_saved_p50, 0);
        assert!(fields.dollars_saved_p50_usd.abs() < f64::EPSILON);

        // Even when check() returns a non-trivial envelope, savings stays
        // pinned to zero — this is the D-8.2 admin-tool sentinel, closed
        // by G8.6.
        let fields_with_load = Layer2Fields::compute(&model, "check", 2_500, 0).unwrap();
        assert_eq!(fields_with_load.tokens_saved_p50, 0);
        assert!(fields_with_load.dollars_saved_p50_usd.abs() < f64::EPSILON);
    }

    /// Clone `sample_model` with the three v0.2 savings knobs overridden.
    fn model_with_knobs(overhead: u64, factor: f64, zero_match: i64) -> CalibrationModel {
        let mut m = sample_model();
        m.model.search_overhead_tokens = overhead;
        m.model.result_recovery_factor = factor;
        m.model.zero_match_savings_tokens = zero_match;
        m
    }

    /// G (v0.2 / D-8.11) — the size-floor rescues a legitimately large result
    /// that the flat-anchor model would have scored as a loss. find_session
    /// anchor is 60k; a 80k-token result is `−20k` under `saved = anchor −
    /// result`, but a retrieval can't cost more than the by-hand baseline, so
    /// the effective counterfactual floors to `result × 1.0 + overhead` and the
    /// saving becomes the search overhead. This is the find_design_doc fix.
    #[test]
    fn layer2_v02_size_floor_rescues_large_result() {
        let model = model_with_knobs(2_000, 1.0, 0);
        let fields = Layer2Fields::compute(&model, "find_session", 80_000, 10).unwrap();
        // Pre-v0.2 this would have been 60_000 − 80_000 = −20_000 (a loss).
        assert_eq!(fields.tokens_saved_p50, 2_000, "floored to overhead, not a loss");
        // Effective counterfactual is emitted (floor dominates the anchor) and
        // reconciles: saved == counterfactual − result.
        assert_eq!(fields.counterfactual_tokens_p50, 82_000);
        assert_eq!(
            fields.tokens_saved_p50,
            fields.counterfactual_tokens_p50 as i64 - 80_000
        );
        // p95 stays ≥ p50.
        assert!(fields.counterfactual_tokens_p95 >= fields.counterfactual_tokens_p50);
    }

    /// G (v0.2 / D-8.11) — the match-gate stops a zero-result call banking the
    /// full anchor. Under v0.1, find_session returning nothing (16-token empty
    /// envelope) scored `60_000 − 16 = 59_984` "saved". Now `after_rank == 0`
    /// credits only `zero_match_savings_tokens` (default 0).
    #[test]
    fn layer2_v02_match_gate_zero_result_saves_nothing() {
        let model = model_with_knobs(2_000, 1.0, 0);
        let fields = Layer2Fields::compute(&model, "find_session", 16, 0).unwrap();
        assert_eq!(fields.tokens_saved_p50, 0, "found nothing => saved nothing");
        assert!(fields.dollars_saved_p50_usd.abs() < f64::EPSILON);
        // A returning call on the same tool/size is still credited (gate keys
        // on after_rank, not result size) — guards against over-correction.
        let returning = Layer2Fields::compute(&model, "find_session", 16, 3).unwrap();
        assert!(returning.tokens_saved_p50 > 0);
    }

    /// G (v0.2 / D-8.11) — knobs actually move the numbers: a higher recovery
    /// factor raises the floor, and a negative zero-match models a wasted call
    /// as a small honest cost.
    #[test]
    fn layer2_v02_knobs_tune_floor_and_wasted_call() {
        // recovery_factor 1.5 → baseline reads 1.5× what nibdex returned.
        let model = model_with_knobs(0, 1.5, -50);
        let fields = Layer2Fields::compute(&model, "find_session", 80_000, 10).unwrap();
        // floor = 80_000 * 1.5 = 120_000; saved = 120_000 − 80_000 = 40_000.
        assert_eq!(fields.tokens_saved_p50, 40_000);
        // Wasted-call knob: a zero-result call is charged the negative credit.
        let wasted = Layer2Fields::compute(&model, "find_session", 1_000, 0).unwrap();
        assert_eq!(wasted.tokens_saved_p50, -50);
    }

    /// G — `record_event` INSERTs one row whose columns match the event.
    /// Covers commit-2 gate "cost_ledger_events row insert correctness".
    #[tokio::test]
    async fn record_event_inserts_row_with_expected_columns() {
        let pool = fresh_pool().await;
        let model = sample_model();
        let fields = Layer2Fields::compute(&model, "find_session", 500, 5).unwrap();
        let event = event_with_layer2(
            "find_session",
            &MetricsEvent::now_ts(),
            &fields,
            500,
            42,
        );

        record_event(&pool, &event).await.expect("insert");

        let row: (String, String, i64, i64, i64, i64, f64, i64, i64, String) = sqlx::query_as(
            "SELECT ts, tool, result_tokens, counterfactual_tokens_p50,
                    counterfactual_tokens_p95, tokens_saved_p50,
                    dollars_saved_p50_usd, wall_ms, counterfactual_wall_ms_p50,
                    calibration_model_version
             FROM cost_ledger_events",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.1, "find_session");
        assert_eq!(row.2, 500);
        assert_eq!(row.3, 60_000);
        assert_eq!(row.4, 85_000);
        assert_eq!(row.5, 59_500);
        assert!((row.6 - 0.1785).abs() < 1e-9);
        assert_eq!(row.7, 42);
        assert_eq!(row.8, 8_500);
        assert_eq!(row.9, "v0.1-test");
    }

    /// G — `record_event` errors when called with Layer-2 fields unset.
    /// Defensive — `emit_metrics` is responsible for the precondition.
    #[tokio::test]
    async fn record_event_errors_without_layer2_fields() {
        let pool = fresh_pool().await;
        // Build a bare v1 event with all Layer-2 fields None.
        let event = MetricsEvent {
            schema_version: 1,
            ts: MetricsEvent::now_ts(),
            tool: "find_session".to_string(),
            query: None,
            params: serde_json::json!({}),
            wall_ms: 0,
            stages_ms: serde_json::json!({"fts5_query":0,"rank":0,"join":0,"shape_response":0}),
            candidate_count: serde_json::json!({"fts5":0,"after_rank":0}),
            result_token_estimate: 0,
            cache_hit: None,
            daemon_uptime_s: 0,
            calibration_confidence: "estimated",
            counterfactual_tokens_p50: None,
            counterfactual_tokens_p95: None,
            tokens_saved_p50: None,
            dollars_saved_p50_usd: None,
            counterfactual_wall_ms_p50: None,
            calibration_model_version: None,
            query_broadened: None,
            returned_full_tokens: None,
            outcome: None,
            error: None,
        };
        let err = record_event(&pool, &event).await.unwrap_err();
        assert!(err.to_string().contains("Layer-2 fields unset"));
    }

    /// G — `backfill_from_jsonl` reconciles the ledger from a durable sink:
    /// Layer-2-complete rows (incl. `check`'s zeros) insert, error rows (no
    /// Layer-2) and malformed lines skip, blank lines aren't scanned, and a
    /// re-run is a no-op. This is the fix for "`check().cost_savings` resets
    /// to near-zero after a DB rebuild because the ledger doesn't survive but
    /// the sink does."
    #[tokio::test]
    async fn backfill_reconciles_layer2_rows_skips_errors_and_is_idempotent() {
        let pool = fresh_pool().await;

        let lines = [
            r#"{"schema_version":1,"ts":"2026-06-09T15:00:00.001Z","tool":"find_session","result_token_estimate":1700,"wall_ms":12,"counterfactual_tokens_p50":60000,"counterfactual_tokens_p95":85000,"tokens_saved_p50":58300,"dollars_saved_p50_usd":0.1749,"counterfactual_wall_ms_p50":8500,"calibration_model_version":"v0.2-test"}"#,
            r#"{"schema_version":1,"ts":"2026-06-09T15:00:00.002Z","tool":"check","result_token_estimate":640,"wall_ms":7,"counterfactual_tokens_p50":0,"counterfactual_tokens_p95":0,"tokens_saved_p50":0,"dollars_saved_p50_usd":0.0,"counterfactual_wall_ms_p50":0,"calibration_model_version":"v0.2-test"}"#,
            "",
            r#"{"schema_version":1,"ts":"2026-06-09T15:00:00.003Z","tool":"find_commit","result_token_estimate":0,"wall_ms":3,"outcome":"error","error":{"kind":"fts5_syntax"}}"#,
            "{ this is not json",
        ];
        let f = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(f.path(), lines.join("\n")).unwrap();

        let r = backfill_from_jsonl(&pool, f.path()).await.unwrap();
        assert_eq!(r.scanned, 4, "two success + one error + one malformed; blank not scanned");
        assert_eq!(r.inserted, 2);
        assert_eq!(r.skipped_no_layer2, 1, "error row has no Layer-2 fields");
        assert_eq!(r.parse_errors, 1);
        assert_eq!(r.skipped_existing, 0);
        // Invariant: every scanned line lands in exactly one bucket.
        assert_eq!(
            r.inserted + r.skipped_existing + r.skipped_no_layer2 + r.parse_errors,
            r.scanned
        );

        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM cost_ledger_events")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, 2);

        // Re-run on the same sink → idempotent no-op (both rows now present).
        let r2 = backfill_from_jsonl(&pool, f.path()).await.unwrap();
        assert_eq!(r2.inserted, 0);
        assert_eq!(r2.skipped_existing, 2);
        let n2: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM cost_ledger_events")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n2, 2, "no double-counting on re-run");
    }

    /// G — a missing sink file is tolerated (best-effort; never a startup
    /// blocker per D-7.6). Returns an all-zero report, not an error.
    #[tokio::test]
    async fn backfill_missing_sink_file_is_noop() {
        let pool = fresh_pool().await;
        let missing = std::path::Path::new("/nonexistent/nibdex/metrics.jsonl");
        let r = backfill_from_jsonl(&pool, missing).await.unwrap();
        assert_eq!(r, BackfillReport::default());
    }

    /// G — the end-to-end bug fix: a fresh (post-rebuild) DB has an empty
    /// ledger → zero rollup; after reconciling from the durable sink, the
    /// `check()` rollup reflects the full history again.
    #[tokio::test]
    async fn backfill_then_aggregate_restores_rollup_after_rebuild() {
        let pool = fresh_pool().await;
        // Recent ts so the events land in all three rolling windows.
        let ts = MetricsEvent::now_ts();
        // Two distinct tools share the ts → distinct (ts, tool) keys.
        let lines = [
            format!(
                r#"{{"schema_version":1,"ts":"{ts}","tool":"find_session","result_token_estimate":1700,"wall_ms":12,"counterfactual_tokens_p50":60000,"counterfactual_tokens_p95":85000,"tokens_saved_p50":58300,"dollars_saved_p50_usd":0.1749,"counterfactual_wall_ms_p50":8500,"calibration_model_version":"v0.2-test"}}"#
            ),
            format!(
                r#"{{"schema_version":1,"ts":"{ts}","tool":"find_commit","result_token_estimate":800,"wall_ms":9,"counterfactual_tokens_p50":9000,"counterfactual_tokens_p95":18000,"tokens_saved_p50":8200,"dollars_saved_p50_usd":0.0246,"counterfactual_wall_ms_p50":3000,"calibration_model_version":"v0.2-test"}}"#
            ),
        ];
        let f = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(f.path(), lines.join("\n")).unwrap();

        // Pre-reconcile: the bug's symptom — empty ledger, zero rollup.
        let before = aggregate_ledger(&pool, "v0.2-test".to_string()).await.unwrap();
        assert_eq!(before.window_30d.queries_served, 0);

        let r = backfill_from_jsonl(&pool, f.path()).await.unwrap();
        assert_eq!(r.inserted, 2);

        // Post-reconcile: history restored.
        let after = aggregate_ledger(&pool, "v0.2-test".to_string()).await.unwrap();
        assert_eq!(after.window_30d.queries_served, 2);
        assert_eq!(after.window_30d.tokens_saved_p50, 58_300 + 8_200);
        assert_eq!(after.window_30d.per_tool.len(), 2);
    }

    /// G — `aggregate_ledger` over an empty ledger returns three zero
    /// windows + no per-tool entries + the calibration version passed in.
    /// Covers commit-2 gate "empty ledger graceful aggregate".
    #[tokio::test]
    async fn aggregate_ledger_empty_yields_zero_windows() {
        let pool = fresh_pool().await;
        let ledger = aggregate_ledger(&pool, "v0.1-test".to_string())
            .await
            .expect("aggregate empty");
        assert_eq!(ledger.calibration_model_version, "v0.1-test");
        for w in [&ledger.window_1d, &ledger.window_7d, &ledger.window_30d] {
            assert_eq!(w.queries_served, 0);
            assert_eq!(w.tokens_returned, 0);
            assert_eq!(w.counterfactual_tokens_p50, 0);
            assert_eq!(w.tokens_saved_p50, 0);
            assert!(w.dollars_saved_p50_usd.abs() < f64::EPSILON);
            assert!(w.per_tool.is_empty());
        }
    }

    /// G — `aggregate_ledger` window monotonicity invariant: 1d ≤ 7d ≤
    /// 30d for every numeric field on every fresh-corpus run (D-8.5,
    /// G8.5 smoke gate). Backstops the smoke gate at the unit-test layer
    /// using synthetic rows at known timestamps.
    #[tokio::test]
    async fn aggregate_ledger_window_monotonicity() {
        let pool = fresh_pool().await;
        let model = sample_model();
        let now = Utc::now();

        // 3 rows: now (in 1d), 3 days ago (in 7d+30d), 15 days ago (in 30d only)
        for offset_days in &[0, 3, 15] {
            let ts = (now - Duration::days(*offset_days))
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
            let fields = Layer2Fields::compute(&model, "find_session", 100, 5).unwrap();
            let event = event_with_layer2("find_session", &ts, &fields, 100, 5);
            record_event(&pool, &event).await.unwrap();
        }

        let ledger = aggregate_ledger(&pool, "v0.1-test".to_string()).await.unwrap();
        assert_eq!(ledger.window_1d.queries_served, 1);
        assert_eq!(ledger.window_7d.queries_served, 2);
        assert_eq!(ledger.window_30d.queries_served, 3);
        assert!(ledger.window_1d.tokens_saved_p50 <= ledger.window_7d.tokens_saved_p50);
        assert!(ledger.window_7d.tokens_saved_p50 <= ledger.window_30d.tokens_saved_p50);
        assert!(ledger.window_1d.dollars_saved_p50_usd <= ledger.window_7d.dollars_saved_p50_usd);
        assert!(ledger.window_7d.dollars_saved_p50_usd <= ledger.window_30d.dollars_saved_p50_usd);
    }

    /// G — Per-tool breakdown groups correctly + dollars match a manual
    /// recompute. Covers commit-2 gate "per-tool aggregate shape".
    #[tokio::test]
    async fn aggregate_ledger_per_tool_breakdown() {
        let pool = fresh_pool().await;
        let model = sample_model();
        let now_ts = MetricsEvent::now_ts();

        // 2 find_session calls + 3 check calls (check produces 0 savings).
        for _ in 0..2 {
            let fields = Layer2Fields::compute(&model, "find_session", 500, 5).unwrap();
            let event = event_with_layer2("find_session", &now_ts, &fields, 500, 5);
            record_event(&pool, &event).await.unwrap();
        }
        for _ in 0..3 {
            let fields = Layer2Fields::compute(&model, "check", 100, 0).unwrap();
            let event = event_with_layer2("check", &now_ts, &fields, 100, 5);
            record_event(&pool, &event).await.unwrap();
        }

        let ledger = aggregate_ledger(&pool, "v0.1-test".to_string()).await.unwrap();
        let per_tool = &ledger.window_1d.per_tool;
        assert_eq!(per_tool.len(), 2);
        let fs = per_tool.get("find_session").expect("find_session entry");
        assert_eq!(fs.queries, 2);
        assert_eq!(fs.tokens_saved_p50, 2 * 59_500);
        let check = per_tool.get("check").expect("check entry");
        assert_eq!(check.queries, 3);
        // check has counterfactual=0 (admin-tool sentinel, D-8.2) → savings
        // clamped to 0 regardless of result tokens. Closes G8.6 at aggregate
        // layer.
        assert_eq!(check.tokens_saved_p50, 0);
        assert!(check.dollars_saved_p50_usd.abs() < f64::EPSILON);
    }
}
