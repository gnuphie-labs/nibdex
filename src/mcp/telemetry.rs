// SPDX-License-Identifier: MIT

//! Metrics emission for `NibdexServer`: the §5.5 Layer-1 success event
//! (`emit_metrics`) and the §8.4 Layer-2 error event (`emit_error_metrics`).
//! Both no-op when the sink is None and swallow failures per D-7.6.
//! Relocated from `mcp.rs` by gh#6 (see `docs/MCP_SPLIT_PLAN.md`).

use serde_json::Value;

use crate::cost_ledger::{self, Layer2Fields};
use crate::metrics_sink::{
    ErrorDetail, MetricsEvent, classify_handler_error, token_estimate_from_serialized,
    truncate_error_message,
};

use super::NibdexServer;
use super::types::Stages;

impl NibdexServer {
    /// Build + emit a §5.5 `MetricsEvent` for a successful tool call.
    /// No-op when the sink is `None`. Failures log to stderr and are
    /// swallowed per D-7.6 (metrics are observation, not policy).
    ///
    /// `had_fts5` drives `candidate_count.fts5`: `total_matched` if the
    /// call went down an FTS5 path, `0` otherwise (no-filter `recent_*`
    /// and `check()`).
    #[allow(clippy::too_many_arguments)] // 11 args is the natural shape — extracted ctx struct
    // adds boilerplate without payoff at one call site per tool.
    pub(crate) async fn emit_metrics(
        &self,
        tool: &str,
        query_text: Option<&str>,
        params_json: Value,
        stages: &Stages,
        wall_ms: u64,
        total_matched: i64,
        returned: i64,
        returned_full_tokens: u64,
        had_fts5: bool,
        query_broadened: bool,
        serialized_envelope: &str,
    ) {
        // Layer 1 sink short-circuit. If sink is off, Layer-2 ledger
        // INSERT is also skipped — the ledger is meaningful only
        // alongside the JSONL stream that consumers correlate with.
        let Some(sink) = self.metrics_sink.as_ref() else {
            return;
        };
        let result_token_estimate = token_estimate_from_serialized(serialized_envelope);

        // §8.4 Layer 2: when calibration model is loaded AND the tool
        // resolves to an entry, populate the 6 additive fields. None
        // anywhere → Layer-1-only event (D-8.7 + D-8.8).
        let layer2 = self
            .calibration
            .as_ref()
            .and_then(|model| Layer2Fields::compute(model, tool, result_token_estimate, returned));

        let event = MetricsEvent {
            schema_version: 1,
            ts: MetricsEvent::now_ts(),
            tool: tool.to_string(),
            query: query_text.map(|s| s.to_string()),
            params: params_json,
            wall_ms,
            stages_ms: stages.to_json(),
            candidate_count: serde_json::json!({
                "fts5": if had_fts5 { total_matched } else { 0 },
                "after_rank": returned,
            }),
            result_token_estimate,
            cache_hit: None,
            daemon_uptime_s: self.started_at.elapsed().as_secs(),
            calibration_confidence: "estimated",
            counterfactual_tokens_p50: layer2.as_ref().map(|l| l.counterfactual_tokens_p50),
            counterfactual_tokens_p95: layer2.as_ref().map(|l| l.counterfactual_tokens_p95),
            tokens_saved_p50: layer2.as_ref().map(|l| l.tokens_saved_p50),
            dollars_saved_p50_usd: layer2.as_ref().map(|l| l.dollars_saved_p50_usd),
            counterfactual_wall_ms_p50: layer2.as_ref().map(|l| l.counterfactual_wall_ms_p50),
            calibration_model_version: layer2.as_ref().map(|l| l.calibration_model_version.clone()),
            // D-10.13: record the OR-broaden flag only on an FTS5 path; `None`
            // off it (check, no-filter recent_*) so the broaden-rate denominator
            // stays clean (METRICS_EXPORT_SPEC §5.1).
            query_broadened: had_fts5.then_some(query_broadened),
            // Grounded-counterfactual raw input (Phase 1, record-only). Gated to the
            // FTS5 path like query_broadened — absent for check + no-filter recent_*
            // so it never enters a non-retrieval row. Live savings still use the flat
            // anchor; `nibdex rescore` consumes this offline to A/B the grounded model.
            returned_full_tokens: had_fts5.then_some(returned_full_tokens),
            // Layer-1 success arm — outcome/error absent (Day 8.5).
            outcome: None,
            error: None,
        };
        if let Err(e) = sink.emit(&event) {
            eprintln!("[nibdex] metrics sink emit failed (tool={tool}): {e:#}");
        }
        // Layer-2 ledger insert — only when fields are populated.
        // Failures are logged + swallowed per D-7.6 (metrics are
        // observation, not policy).
        if layer2.is_some()
            && let Err(e) = cost_ledger::record_event(&self.pool, &event).await
        {
            eprintln!("[nibdex] cost_ledger record_event failed (tool={tool}): {e:#}");
        }
    }

    /// Build + emit a §5.5 `MetricsEvent` for a FAILED tool call (Day 8.5).
    /// Mirrors `emit_metrics` but populates `outcome: Some("error")` +
    /// `error: Some(ErrorDetail{...})`, zeroes out candidate_count +
    /// result_token_estimate (no envelope to serialize), and skips the
    /// cost-ledger insert entirely (errors have no counterfactual to
    /// clamp against per §11 honesty).
    ///
    /// Without this method, the JSONL stream would silently drop every
    /// failing call — distorting the eval harness's win/loss math against
    /// the ripgrep baseline.
    ///
    /// `error_msg` is classified into a coarse bucket via
    /// `classify_handler_error` and truncated to 500 chars.
    pub(crate) async fn emit_error_metrics(
        &self,
        tool: &str,
        query_text: Option<&str>,
        params_json: Value,
        stages: &Stages,
        wall_ms: u64,
        error_msg: &str,
    ) {
        let Some(sink) = self.metrics_sink.as_ref() else {
            return;
        };
        let event = MetricsEvent {
            schema_version: 1,
            ts: MetricsEvent::now_ts(),
            tool: tool.to_string(),
            query: query_text.map(|s| s.to_string()),
            params: params_json,
            wall_ms,
            stages_ms: stages.to_json(),
            // No candidates were returned — both buckets zero per the
            // G7.7 expected envelope shape.
            candidate_count: serde_json::json!({ "fts5": 0, "after_rank": 0 }),
            // No envelope was serialized — token estimate is zero.
            result_token_estimate: 0,
            cache_hit: None,
            daemon_uptime_s: self.started_at.elapsed().as_secs(),
            calibration_confidence: "estimated",
            // Layer-2 stays absent on error rows — errors have no
            // counterfactual to clamp against (§11 honesty).
            counterfactual_tokens_p50: None,
            counterfactual_tokens_p95: None,
            tokens_saved_p50: None,
            dollars_saved_p50_usd: None,
            counterfactual_wall_ms_p50: None,
            calibration_model_version: None,
            // No envelope on the error path — broadening is unknowable here.
            query_broadened: None,
            // No envelope serialized → no returned-hit bodies to size.
            returned_full_tokens: None,
            outcome: Some("error"),
            error: Some(ErrorDetail {
                kind: classify_handler_error(error_msg).to_string(),
                message: truncate_error_message(error_msg),
            }),
        };
        if let Err(e) = sink.emit(&event) {
            eprintln!("[nibdex] metrics sink error-emit failed (tool={tool}): {e:#}");
        }
        // NO cost_ledger::record_event call — Layer 2 stays success-only.
    }
}
