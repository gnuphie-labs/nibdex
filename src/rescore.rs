// SPDX-License-Identifier: MIT

//! `nibdex rescore <export.json>` — offline re-scoring of an archived metrics
//! export under the *current* `calibration.toml` savings model.
//!
//! Why this exists: a `nibdex metrics-export` payload stamps each call's
//! `tokens_saved_p50` with the calibration model that was live when the call
//! happened (per-row `calibration_model_version`). When the savings *model*
//! changes — e.g. the v0.1→v0.2 honesty fix (D-8.11) — the already-archived
//! rows keep their old figures. This command re-derives every row's savings
//! under the loaded model so a maintainer can see the full before/after on a
//! real corpus **without** redeploying the daemon or re-running anything.
//!
//! Parity by construction: re-scoring calls the *same* [`Layer2Fields::compute`]
//! the live emit path uses (`mcp::telemetry::emit_metrics`). There is no second
//! copy of the savings formula here to drift out of sync — the export rows
//! carry the only two raw inputs the model needs (`result_token_estimate` and
//! `candidate_count.after_rank`), so the re-score is fully reproducible offline.
//!
//! Error rows are honored, not re-scored: the live path suppresses Layer-2 on a
//! failed call (errors have no counterfactual to clamp against, §11 honesty), so
//! a source row carrying an `error` is reported as such and excluded from the
//! re-scored totals — mirroring `emit_error_metrics`.
//!
//! Zero side effects: reads the export JSON + the calibration model, writes
//! nothing, transmits nothing. Output is a human-readable diff to stdout.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;

use crate::calibration::CalibrationModel;
use crate::cost_ledger::Layer2Fields;

/// What happened to one call row when re-scored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowStatus {
    /// Re-scored under the loaded model (tool resolved to a counterfactual).
    Scored,
    /// Source row carried an `error` → no Layer-2, mirroring the live error
    /// path. Excluded from re-scored totals.
    ErrorRow,
    /// Tool is absent from the loaded `calibration.toml` → no counterfactual,
    /// mirroring `Layer2Fields::compute` returning `None` (forward-compat with
    /// tools added before an operator re-tunes their model).
    ToolNotInModel,
}

/// One re-scored call row: the recorded (as-exported) savings alongside the
/// freshly recomputed value, so the per-row delta is self-evident.
#[derive(Debug, Clone)]
pub struct RescoredRow {
    /// Position in the export's `calls` array (0-based), for cross-reference.
    pub index: usize,
    pub tool: String,
    /// `candidate_count.after_rank` — the match-gate input (0 = found nothing).
    pub after_rank: i64,
    pub result_token_estimate: u64,
    /// `tokens_saved_p50` as it stands in the export (the model live at capture).
    /// `None` for source error rows (no Layer-2 was emitted).
    pub recorded_saved: Option<i64>,
    /// `calibration_model_version` as stamped in the export. `None` on error rows.
    pub recorded_version: Option<String>,
    /// Savings recomputed under the loaded model. `None` for error rows and
    /// tools-not-in-model.
    pub rescored_saved: Option<i64>,
    pub status: RowStatus,
}

impl RescoredRow {
    /// Per-row change, defined only when both figures exist (a `Scored` row
    /// that also carried a recorded value in the source).
    pub fn delta(&self) -> Option<i64> {
        match (self.recorded_saved, self.rescored_saved) {
            (Some(rec), Some(new)) => Some(new - rec),
            _ => None,
        }
    }
}

/// Full re-score result for one export file.
#[derive(Debug, Clone)]
pub struct RescoreReport {
    pub export_path: PathBuf,
    /// `version` of the loaded `calibration.toml` (the re-score model).
    pub model_version: String,
    pub rows: Vec<RescoredRow>,
}

/// Per-tool rollup line (sum over that tool's rows).
#[derive(Debug, Clone)]
pub struct ToolRollup {
    pub tool: String,
    pub calls: usize,
    pub recorded_saved: i64,
    pub rescored_saved: i64,
}

impl ToolRollup {
    pub fn delta(&self) -> i64 {
        self.rescored_saved - self.recorded_saved
    }
}

impl RescoreReport {
    /// Sum of recorded `tokens_saved_p50` over rows that carried one.
    pub fn recorded_total(&self) -> i64 {
        self.rows.iter().filter_map(|r| r.recorded_saved).sum()
    }

    /// Sum of re-scored `tokens_saved_p50` over rows the loaded model scored.
    pub fn rescored_total(&self) -> i64 {
        self.rows.iter().filter_map(|r| r.rescored_saved).sum()
    }

    pub fn error_rows(&self) -> usize {
        self.rows
            .iter()
            .filter(|r| r.status == RowStatus::ErrorRow)
            .count()
    }

    pub fn unscored_tool_rows(&self) -> usize {
        self.rows
            .iter()
            .filter(|r| r.status == RowStatus::ToolNotInModel)
            .count()
    }

    /// Rows whose figure actually moved under the new model.
    pub fn changed_rows(&self) -> usize {
        self.rows
            .iter()
            .filter(|r| matches!(r.delta(), Some(d) if d != 0))
            .count()
    }

    /// Per-tool rollups, sorted by tool name for stable output.
    pub fn per_tool(&self) -> Vec<ToolRollup> {
        let mut map: std::collections::BTreeMap<String, ToolRollup> = std::collections::BTreeMap::new();
        for r in &self.rows {
            let entry = map.entry(r.tool.clone()).or_insert_with(|| ToolRollup {
                tool: r.tool.clone(),
                calls: 0,
                recorded_saved: 0,
                rescored_saved: 0,
            });
            entry.calls += 1;
            entry.recorded_saved += r.recorded_saved.unwrap_or(0);
            entry.rescored_saved += r.rescored_saved.unwrap_or(0);
        }
        map.into_values().collect()
    }

    /// Rows with the largest absolute corrections first (the honesty fix's
    /// biggest movers). Ties broken by source index for determinism.
    pub fn largest_corrections(&self, n: usize) -> Vec<&RescoredRow> {
        let mut moved: Vec<&RescoredRow> = self
            .rows
            .iter()
            .filter(|r| matches!(r.delta(), Some(d) if d != 0))
            .collect();
        moved.sort_by(|a, b| {
            let (da, db) = (a.delta().unwrap_or(0).abs(), b.delta().unwrap_or(0).abs());
            db.cmp(&da).then(a.index.cmp(&b.index))
        });
        moved.truncate(n);
        moved
    }
}

/// Re-score every call row in an export file under `model`.
///
/// Reads the file, parses the `calls` array, and recomputes each row's
/// `tokens_saved_p50` via [`Layer2Fields::compute`]. Pure: no writes, no
/// network. Returns the structured report; rendering is the caller's job.
pub fn run_rescore(export_path: &Path, model: &CalibrationModel) -> Result<RescoreReport> {
    let raw = std::fs::read_to_string(export_path)
        .with_context(|| format!("read export file {}", export_path.display()))?;
    let doc: Value = serde_json::from_str(&raw)
        .with_context(|| format!("parse export JSON {}", export_path.display()))?;
    let calls = doc
        .get("calls")
        .and_then(Value::as_array)
        .context("export JSON has no `calls` array — is this a nibdex metrics export?")?;

    let mut rows = Vec::with_capacity(calls.len());
    for (index, call) in calls.iter().enumerate() {
        let tool = call
            .get("tool")
            .and_then(Value::as_str)
            .unwrap_or("<unknown>")
            .to_string();
        let after_rank = call
            .get("candidate_count")
            .and_then(|c| c.get("after_rank"))
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let result_token_estimate = call
            .get("result_token_estimate")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let recorded_saved = call.get("tokens_saved_p50").and_then(Value::as_i64);
        let recorded_version = call
            .get("calibration_model_version")
            .and_then(Value::as_str)
            .map(str::to_string);
        // An error row carries a non-null `error` object; the live path emits
        // no Layer-2 for it (emit_error_metrics), so we don't re-score it.
        let is_error = call.get("error").map(|e| !e.is_null()).unwrap_or(false);

        let (rescored_saved, status) = if is_error {
            (None, RowStatus::ErrorRow)
        } else {
            match Layer2Fields::compute(model, &tool, result_token_estimate, after_rank) {
                Some(l2) => (Some(l2.tokens_saved_p50), RowStatus::Scored),
                None => (None, RowStatus::ToolNotInModel),
            }
        };

        rows.push(RescoredRow {
            index,
            tool,
            after_rank,
            result_token_estimate,
            recorded_saved,
            recorded_version,
            rescored_saved,
            status,
        });
    }

    Ok(RescoreReport {
        export_path: export_path.to_path_buf(),
        model_version: model.model.version.clone(),
        rows,
    })
}

/// Render the report as a human-readable diff for stdout.
pub fn render(report: &RescoreReport) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();

    // Distinct recorded versions present in the source, for the header.
    let mut recorded_versions: Vec<String> = report
        .rows
        .iter()
        .filter_map(|r| r.recorded_version.clone())
        .collect();
    recorded_versions.sort();
    recorded_versions.dedup();
    let recorded_label = if recorded_versions.is_empty() {
        "(none recorded)".to_string()
    } else {
        recorded_versions.join(", ")
    };

    let _ = writeln!(out, "nibdex rescore — {}", report.export_path.display());
    let _ = writeln!(
        out,
        "  recorded model: {}    →    re-score model: {}",
        recorded_label, report.model_version
    );
    let _ = writeln!(out, "  {} call row(s)", report.rows.len());
    let _ = writeln!(out);

    // Per-row table.
    let _ = writeln!(
        out,
        "  {:>3}  {:<16} {:>5} {:>11} {:>13} {:>13} {:>12}",
        "#", "tool", "rank", "result_tok", "recorded", "re-scored", "Δ"
    );
    let _ = writeln!(out, "  {}", "-".repeat(78));
    for r in &report.rows {
        let recorded = r
            .recorded_saved
            .map(commas)
            .unwrap_or_else(|| "—".to_string());
        let (rescored, delta) = match r.status {
            RowStatus::Scored => (
                r.rescored_saved.map(commas).unwrap_or_default(),
                r.delta().map(signed).unwrap_or_default(),
            ),
            RowStatus::ErrorRow => ("error".to_string(), String::new()),
            RowStatus::ToolNotInModel => ("not-in-model".to_string(), String::new()),
        };
        let _ = writeln!(
            out,
            "  {:>3}  {:<16} {:>5} {:>11} {:>13} {:>13} {:>12}",
            r.index, r.tool, r.after_rank, r.result_token_estimate, recorded, rescored, delta
        );
    }
    let _ = writeln!(out);

    // Largest corrections.
    let movers = report.largest_corrections(5);
    if !movers.is_empty() {
        let _ = writeln!(out, "  Largest corrections:");
        for r in movers {
            let _ = writeln!(
                out,
                "    [{:>2}] {:<16} rank={:<3} result_tok={:>7}  {:>11} → {:>11}  ({})",
                r.index,
                r.tool,
                r.after_rank,
                r.result_token_estimate,
                r.recorded_saved.map(commas).unwrap_or_default(),
                r.rescored_saved.map(commas).unwrap_or_default(),
                signed(r.delta().unwrap_or(0)),
            );
        }
        let _ = writeln!(out);
    }

    // Per-tool rollup.
    let _ = writeln!(
        out,
        "  {:<16} {:>6} {:>13} {:>13} {:>12}",
        "per tool", "calls", "recorded", "re-scored", "Δ"
    );
    let _ = writeln!(out, "  {}", "-".repeat(64));
    for t in report.per_tool() {
        let _ = writeln!(
            out,
            "  {:<16} {:>6} {:>13} {:>13} {:>12}",
            t.tool,
            t.calls,
            commas(t.recorded_saved),
            commas(t.rescored_saved),
            signed(t.delta()),
        );
    }
    let _ = writeln!(out);

    // Totals.
    let rec = report.recorded_total();
    let new = report.rescored_total();
    let _ = writeln!(out, "  TOTAL tokens_saved_p50");
    let _ = writeln!(out, "    recorded : {:>15}", commas(rec));
    let _ = writeln!(out, "    re-scored: {:>15}", commas(new));
    let _ = writeln!(out, "    Δ        : {:>15}", signed(new - rec));
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "  {} row(s) changed · {} error row(s) excluded · {} tool-not-in-model row(s)",
        report.changed_rows(),
        report.error_rows(),
        report.unscored_tool_rows(),
    );

    out
}

/// Group an integer with thousands separators (e.g. `59084` → `59,084`).
/// Handles negatives. Pure ASCII; no locale dependency.
fn commas(n: i64) -> String {
    let neg = n < 0;
    let digits = n.unsigned_abs().to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3 + 1);
    let bytes = digits.as_bytes();
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(*b as char);
    }
    if neg {
        format!("-{grouped}")
    } else {
        grouped
    }
}

/// Like [`commas`] but always carries an explicit sign (for delta columns).
fn signed(n: i64) -> String {
    if n >= 0 {
        format!("+{}", commas(n))
    } else {
        commas(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Load the in-tree v0.2 seed `calibration.toml` (same file the daemon
    /// ships). Tests run from the crate root.
    fn seed_model() -> CalibrationModel {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("calibration.toml");
        CalibrationModel::load_or_warn(&path).expect("seed calibration.toml parses")
    }

    /// Write a tiny synthetic export with the shape `run_rescore` reads and
    /// return its path inside `dir`.
    fn write_export(dir: &std::path::Path, calls: serde_json::Value) -> std::path::PathBuf {
        let doc = serde_json::json!({
            "nibdex_metrics_export": { "format_version": 1 },
            "calls": calls,
        });
        let path = dir.join("export.json");
        std::fs::write(&path, serde_json::to_string_pretty(&doc).unwrap()).unwrap();
        path
    }

    fn row(
        tool: &str,
        after_rank: i64,
        result_tok: u64,
        recorded_saved: Option<i64>,
        version: Option<&str>,
        error_kind: Option<&str>,
    ) -> serde_json::Value {
        let mut v = serde_json::json!({
            "tool": tool,
            "candidate_count": { "fts5": after_rank, "after_rank": after_rank },
            "result_token_estimate": result_tok,
            "tokens_saved_p50": recorded_saved,
            "calibration_model_version": version,
        });
        if let Some(k) = error_kind {
            v.as_object_mut()
                .unwrap()
                .insert("error".to_string(), serde_json::json!({ "kind": k }));
        }
        v
    }

    /// The four canonical v0.2 branches plus an error row, re-scored from the
    /// seed model: admin-zero, zero-match gate, size-floor on a large result,
    /// a well-calibrated row left unchanged, and a suppressed error row.
    #[test]
    fn rescores_all_branches_against_seed() {
        let tmp = tempfile::tempdir().unwrap();
        let calls = serde_json::json!([
            // check: anchor 0 → always 0 (D-8.2 admin tool).
            row("check", 0, 348, Some(0), Some("v0.1-2026-05-27"), None),
            // zero-match find_session: v0.1 banked 60000-16=59984; v0.2 → 0.
            row("find_session", 0, 16, Some(59_984), Some("v0.1-2026-05-27"), None),
            // big find_design_doc: v0.1 scored a loss (18000-61690=-43690);
            // v0.2 size-floor → max(18000, 61690+2000) - 61690 = 2000.
            row("find_design_doc", 3, 61_690, Some(-43_690), Some("v0.1-2026-05-27"), None),
            // well-calibrated find_session: anchor dominates the floor, so v0.2
            // == v0.1 (60000-916=59084) — unchanged.
            row("find_session", 3, 916, Some(59_084), Some("v0.1-2026-05-27"), None),
            // error row: no Layer-2 in source, not re-scored.
            row("find_commit", 0, 0, None, None, Some("fts5_syntax")),
        ]);
        let path = write_export(tmp.path(), calls);

        let report = run_rescore(&path, &seed_model()).expect("rescore");
        assert_eq!(report.rows.len(), 5);

        // check → 0, unchanged.
        assert_eq!(report.rows[0].status, RowStatus::Scored);
        assert_eq!(report.rows[0].rescored_saved, Some(0));
        assert_eq!(report.rows[0].delta(), Some(0));

        // zero-match → 0 (the +59,984 honesty bug, corrected).
        assert_eq!(report.rows[1].rescored_saved, Some(0));
        assert_eq!(report.rows[1].delta(), Some(-59_984));

        // size-floor → +2000, a big upward correction from the v0.1 loss.
        assert_eq!(report.rows[2].rescored_saved, Some(2_000));
        assert_eq!(report.rows[2].delta(), Some(2_000 - (-43_690)));

        // well-calibrated → unchanged.
        assert_eq!(report.rows[3].rescored_saved, Some(59_084));
        assert_eq!(report.rows[3].delta(), Some(0));

        // error row → suppressed, excluded from totals.
        assert_eq!(report.rows[4].status, RowStatus::ErrorRow);
        assert_eq!(report.rows[4].rescored_saved, None);
        assert_eq!(report.rows[4].delta(), None);

        // Totals reconcile.
        assert_eq!(report.recorded_total(), 59_984 - 43_690 + 59_084);
        assert_eq!(report.rescored_total(), 2_000 + 59_084);
        assert_eq!(report.error_rows(), 1);
        assert_eq!(report.changed_rows(), 2); // zero-match + size-floor
    }

    /// A tool absent from the model is reported, not silently zeroed — mirrors
    /// `Layer2Fields::compute` returning `None`.
    #[test]
    fn unknown_tool_is_flagged_not_scored() {
        let tmp = tempfile::tempdir().unwrap();
        let calls = serde_json::json!([
            row("some_future_tool", 2, 500, Some(123), Some("v0.1-2026-05-27"), None),
        ]);
        let path = write_export(tmp.path(), calls);
        let report = run_rescore(&path, &seed_model()).unwrap();
        assert_eq!(report.rows[0].status, RowStatus::ToolNotInModel);
        assert_eq!(report.rows[0].rescored_saved, None);
        assert_eq!(report.unscored_tool_rows(), 1);
    }

    /// Re-scoring is parity-by-construction with the live emit path: the value
    /// equals `Layer2Fields::compute` called directly with the row's raw inputs.
    #[test]
    fn rescore_matches_compute_directly() {
        let tmp = tempfile::tempdir().unwrap();
        let model = seed_model();
        let calls = serde_json::json!([
            row("find_memory", 4, 1_200, Some(0), Some("v0.1-2026-05-27"), None),
        ]);
        let path = write_export(tmp.path(), calls);
        let report = run_rescore(&path, &model).unwrap();
        let direct = Layer2Fields::compute(&model, "find_memory", 1_200, 4)
            .unwrap()
            .tokens_saved_p50;
        assert_eq!(report.rows[0].rescored_saved, Some(direct));
    }

    /// A non-export JSON (no `calls`) fails cleanly rather than panicking.
    #[test]
    fn missing_calls_array_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nope.json");
        std::fs::write(&path, "{\"hello\": 1}").unwrap();
        let err = run_rescore(&path, &seed_model()).unwrap_err();
        assert!(err.to_string().contains("calls"), "got: {err}");
    }

    #[test]
    fn commas_groups_and_signs() {
        assert_eq!(commas(0), "0");
        assert_eq!(commas(916), "916");
        assert_eq!(commas(59_084), "59,084");
        assert_eq!(commas(-43_690), "-43,690");
        assert_eq!(signed(2_000), "+2,000");
        assert_eq!(signed(-59_984), "-59,984");
    }
}
