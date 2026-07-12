// SPDX-License-Identifier: MIT

//! §5.5 Layer 1 — JSONL event-stream sink for MCP tool calls.
//!
//! Day 7 commit 1 landed the scaffold; commit 2 wires the 7 `tool.*`
//! handlers + adds the `Stages` helper in `mcp.rs`. The CLI parses
//! `--metrics-sink off|stdout|jsonl:<path>`, the chosen sink is plumbed
//! through `serve_stdio` and `http_server::serve` into `NibdexServer`,
//! and every successful tool call emits one `MetricsEvent` line.
//!
//! Schema policy (DESIGN §5.5): `schema_version` starts at 1. Additive
//! field changes don't bump; renames / drops / type changes do. Per
//! §11 honesty norms, every v1 event carries `calibration_confidence
//! = "estimated"` — Day 8's §8.4 ledger consumes the tag rather than
//! removing it.
//!
//! Buffering: line-buffered `BufWriter` with an explicit `flush()` per
//! emit (D-7.5). The §14.7 smoke bar is "<1s buffering delay"; per-
//! line flush is the simplest way to hit it. Cost is one syscall per
//! tool call, dwarfed by typical 1-50ms tool latency.
//!
//! Rotation: append-mode (`OpenOptions::append(true).create(true)`).
//! The "rotated on daemon restart" framing in §5.5 means *consumers*
//! treat restart as a natural boundary via file timestamps — nibdex
//! never truncates. LIMITATIONS.md documents this at the §13 flip.

use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use chrono::SecondsFormat;
use serde::Serialize;
use serde_json::Value;

/// Parsed form of `--metrics-sink`. Lives separately from `MetricsSink`
/// so parsing is pure (no I/O) and `MetricsSink::from_spec` carries the
/// fallible file-open step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetricsSinkSpec {
    /// `off` — emission disabled. No file opened, no stdout writes.
    Off,
    /// `stdout` — one JSONL line per emit to process stdout.
    Stdout,
    /// `jsonl:<path>` — append-mode line-buffered writes to `<path>`.
    Jsonl(PathBuf),
}

// `Default` impl reserved for commit-2 wiring; the CLI `default_value =
// "off"` already covers the parse path. Skipped at v1 to avoid a stale
// API surface — re-add if commit 2 or Day 8 needs a `Default` bound.

impl FromStr for MetricsSinkSpec {
    type Err = anyhow::Error;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw {
            "off" => Ok(MetricsSinkSpec::Off),
            "stdout" => Ok(MetricsSinkSpec::Stdout),
            other => {
                if let Some(rest) = other.strip_prefix("jsonl:") {
                    if rest.trim().is_empty() {
                        anyhow::bail!(
                            "--metrics-sink jsonl: requires a path (got empty after `jsonl:`)"
                        );
                    }
                    Ok(MetricsSinkSpec::Jsonl(PathBuf::from(rest)))
                } else {
                    anyhow::bail!(
                        "--metrics-sink expects `off`, `stdout`, or `jsonl:<path>` (got `{other}`)"
                    )
                }
            }
        }
    }
}

/// Runtime sink. Three variants per D-7.1:
///
/// - `Disabled` — zero-cost no-op; `emit()` returns immediately.
/// - `Stdout` — one JSONL line per emit to process stdout, flushed.
/// - `JsonlFile` — append-mode line-buffered writer + explicit flush.
///
/// The `Arc<Mutex<_>>` on the file variant lets the sink be cloned
/// cheaply across the seven tool handlers + Day 8's cost ledger
/// consumer. Mutex hold time is bounded to one `write_all` + `flush`
/// pair (~1ms worst-case for a sub-kB line), so contention is a
/// non-issue under MVP workloads.
pub enum MetricsSink {
    Disabled,
    Stdout,
    JsonlFile(Arc<Mutex<BufWriter<File>>>),
}

impl MetricsSink {
    /// Resolve a parsed spec into a runtime sink. Opens the file at this
    /// point so misconfiguration surfaces at startup instead of first
    /// tool call. Returns the spec-shaped error context per
    /// `anyhow::Context`.
    pub fn from_spec(spec: MetricsSinkSpec) -> Result<Self> {
        match spec {
            MetricsSinkSpec::Off => Ok(MetricsSink::Disabled),
            MetricsSinkSpec::Stdout => Ok(MetricsSink::Stdout),
            MetricsSinkSpec::Jsonl(path) => {
                let file = OpenOptions::new()
                    .append(true)
                    .create(true)
                    .open(&path)
                    .with_context(|| format!("opening metrics jsonl sink at {}", path.display()))?;
                Ok(MetricsSink::JsonlFile(Arc::new(Mutex::new(BufWriter::new(
                    file,
                )))))
            }
        }
    }

    /// Serialize the event as one JSONL line and write it to the
    /// configured destination. Returns `io::Error` on failure; callers
    /// MUST log + ignore (D-7.6 — metrics are observation, not policy).
    ///
    /// Note: synchronous I/O. Lines are sub-kB, flushed under a
    /// short-held mutex; blocking the executor briefly is acceptable
    /// under MVP load. If contention becomes load-bearing, spawn_blocking
    /// is the obvious upgrade — schema_version stays 1.
    pub fn emit(&self, event: &MetricsEvent) -> io::Result<()> {
        if matches!(self, MetricsSink::Disabled) {
            return Ok(());
        }
        let line = serde_json::to_string(event)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        match self {
            MetricsSink::Disabled => unreachable!("checked above"),
            MetricsSink::Stdout => {
                let mut stdout = io::stdout().lock();
                stdout.write_all(line.as_bytes())?;
                stdout.write_all(b"\n")?;
                stdout.flush()?;
            }
            MetricsSink::JsonlFile(writer) => {
                let mut guard = writer
                    .lock()
                    .map_err(|_| io::Error::other("metrics sink mutex poisoned"))?;
                guard.write_all(line.as_bytes())?;
                guard.write_all(b"\n")?;
                guard.flush()?;
            }
        }
        Ok(())
    }
}

/// Per-call event shape. Fields match DESIGN §5.5 exactly, plus the v1
/// `calibration_confidence` tag landed early per D-7.2 so the public-
/// flip contract carries honest framing from first event.
///
/// `schema_version` is the FIRST field so downstream JSONL parsers
/// can pin to a known shape before parsing the rest. Field order does
/// not constrain serialization beyond that — `serde_json` emits in
/// declaration order, but consumers should treat the JSON object as
/// unordered per RFC 8259.
#[derive(Debug, Serialize)]
pub struct MetricsEvent {
    /// Hard-coded `1` at v1. Bump only on breaking changes per §5.5.
    pub schema_version: u32,
    /// RFC3339 with millisecond precision, e.g. `"2026-05-27T19:14:08.412Z"`.
    pub ts: String,
    /// Tool name without the `tool.` prefix (`recent_sessions`, `check`, ...).
    pub tool: String,
    /// Verbatim user query when present; `None` for `check()`. Users
    /// already have FS access to indexed corpus, so logging the query
    /// adds no privacy surface beyond the existing OS audit log.
    pub query: Option<String>,
    /// Per-tool parameter dictionary, e.g. `{"limit": 5, "days": 30}`.
    pub params: Value,
    /// Total handler wall time in milliseconds.
    pub wall_ms: u64,
    /// Per-stage timing breakdown per D-7.3. Object with 4 keys:
    /// `fts5_query`, `rank`, `join`, `shape_response`.
    pub stages_ms: Value,
    /// `{"fts5": N, "after_rank": M}` per D-7.3. Today `fts5 == after_rank`
    /// post-LIMIT; the split anticipates a Phase-3 re-rank stage.
    pub candidate_count: Value,
    /// `chars ÷ 4` of the serialized envelope, integer floor (D-7.4).
    pub result_token_estimate: u64,
    /// `None` at v1; Phase 3 cache-hit telemetry promotes this to bool.
    pub cache_hit: Option<bool>,
    /// Seconds since the daemon (stdio or HTTP) started.
    pub daemon_uptime_s: u64,
    /// `"estimated"` for all v1 events per §11 honesty norms + §14.8.
    /// Day 8 calibration work consumes this tag without bumping
    /// `schema_version` — the schema policy is additive on field
    /// content as long as the field key + type stay stable.
    pub calibration_confidence: &'static str,

    // ----- §8.4 Layer 2 additive fields (D-8.7) -----
    // All optional; `serde(skip_serializing_if = "Option::is_none")`
    // omits the field entirely from the JSONL line when Layer-2 is off
    // (calibration model absent OR tool not in model). Adding optional
    // fields does NOT bump `schema_version` per §5.5 + D-8.7.

    /// Counterfactual tokens p50 from the calibration model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub counterfactual_tokens_p50: Option<u64>,

    /// Counterfactual tokens p95 from the calibration model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub counterfactual_tokens_p95: Option<u64>,

    /// `counterfactual_p50 - result_token_estimate`. Signed — a query
    /// returning many matches can legitimately produce more text than
    /// the counterfactual would have read (D-8.4).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_saved_p50: Option<i64>,

    /// `(tokens_saved_p50 / 1_000_000) * model.input_rate_usd_per_mtok`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dollars_saved_p50_usd: Option<f64>,

    /// Counterfactual wall time p50 from the calibration model. Estimated,
    /// not measured (Layer-2-A; Layer-2-B sampling is Phase 2).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub counterfactual_wall_ms_p50: Option<u64>,

    /// `model.version` from `calibration.toml`. Stamped per-event so
    /// historical rows survive calibration recalibration without
    /// retroactive change.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub calibration_model_version: Option<String>,

    // ----- D-10.13 retrieval-quality signal (additive, optional) -----
    // The OR-broaden flag, persisted so the IP-safe metrics export can carry
    // a retrieval-quality signal and not just speed + savings
    // (METRICS_EXPORT_SPEC §5.1). `Some(true)` when the caller's multi-term
    // query AND-matched nothing and nibdex auto-retried OR-broadened;
    // `Some(false)` when an FTS5 query ran without broadening; `None` on
    // non-FTS5 paths (`check`, no-filter `recent_*`) and error rows, where
    // broadening is not a meaningful concept — keeping those out of the
    // numerator AND denominator of any broaden-rate. Optional → absent from
    // the JSONL line off the FTS5 path; does NOT bump `schema_version`
    // (additive-field policy, §5.5 + D-8.7).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query_broadened: Option<bool>,

    // ----- Grounded-counterfactual raw input (record-only, additive, optional) -----
    /// Summed FULL (untruncated) token estimate of the hits returned by this call,
    /// before snippet/body-budget trimming — the "what you'd have read by hand once
    /// located" size. Recorded as raw material so `nibdex rescore` can derive a
    /// per-query counterfactual offline (the grounded model) and A/B it against the
    /// flat per-tool anchor, WITHOUT changing any live savings figure. `Some` only
    /// on an FTS5 retrieval path (gated by `had_fts5`); `None` on `check`,
    /// no-filter `recent_*`, and error rows. Optional → absent from the JSONL line
    /// off that path; does NOT bump `schema_version` (additive-field policy,
    /// §5.5 + D-8.7), exactly like `query_broadened`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub returned_full_tokens: Option<u64>,

    // ----- §5.5 Layer-1 error path (Day 8.5) -----
    // Both `outcome` and `error` are absent on success envelopes
    // (skip_serializing_if), so the existing G7.4 12-key schema stays
    // byte-identical for happy-path callers. They appear together on
    // error rows. `schema_version` stays at 1 — additive fields don't
    // bump it (§5.5 + D-8.7 policy).
    /// Present only on error rows. `Some("error")` at v1; reserved as
    /// `Option<&'static str>` to permit additional variants without
    /// breaking the wire (e.g. `"degraded"` if a future tool returns a
    /// partial result with a recoverable warning).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<&'static str>,

    /// Structured error detail when the tool handler returned an `Err`.
    /// Populated together with `outcome: Some("error")`. Absent on
    /// success.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorDetail>,
}

/// Companion struct surfaced inside `MetricsEvent.error` on Layer-1
/// error rows. `kind` is a coarse, classifier-driven bucket meant for
/// aggregation; `message` is the verbatim handler error string,
/// truncated to 500 chars so a long anyhow chain doesn't blow the line
/// budget. Downstream consumers correlate `error.kind` against the
/// query string to identify silent-fail-out classes.
#[derive(Debug, Serialize)]
pub struct ErrorDetail {
    pub kind: String,
    pub message: String,
}

/// Classify a handler error string into a coarse bucket for Layer-1
/// aggregation. Pure function; case-insensitive substring matching.
///
/// Three buckets at v1:
/// - `fts5_syntax`: malformed FTS5 MATCH expression (hyphen-as-NOT
///   cliff, unclosed quotes, no-such-column). Most-common dogfood
///   failure per #628 sub-finding (a).
/// - `sqlite`: connection / schema / IO failures from sqlx-sqlite.
/// - `internal`: everything else (anyhow Other, serde failures,
///   unexpected None unwraps surfaced via `?`).
///
/// Input-validation errors (bad limits, missing required params) are
/// caught by serde at handler entry and never reach this path, so the
/// classifier intentionally has no `bad_input` bucket.
pub fn classify_handler_error(msg: &str) -> &'static str {
    let m = msg.to_ascii_lowercase();
    if m.contains("no such column")
        || m.contains("malformed match")
        || m.contains("fts5: syntax error")
        || m.contains("syntax error near")
        || m.contains("unrecognized token")
    {
        "fts5_syntax"
    } else if m.contains("sqlx") || m.contains("sqlite") || m.contains("database") {
        "sqlite"
    } else {
        "internal"
    }
}

/// Truncate `msg` to at most 500 chars on a UTF-8 character boundary.
/// Pulled out of `emit_error_metrics` so handler edits stay one-liners.
pub fn truncate_error_message(msg: &str) -> String {
    const MAX: usize = 500;
    if msg.chars().count() <= MAX {
        return msg.to_string();
    }
    msg.chars().take(MAX).collect()
}

impl MetricsEvent {
    /// Build the RFC3339-millis timestamp for `ts`. Pulled into a helper
    /// so per-handler emit blocks stay one-liners.
    pub fn now_ts() -> String {
        chrono::Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
    }
}

/// `chars ÷ 4` of the JSON-serialized envelope per D-7.4. The handler
/// serializes its envelope once for the token-estimate input; rmcp re-
/// serializes for the wire response. Cost: ~1ms extra per tool call.
pub fn token_estimate_from_serialized(serialized: &str) -> u64 {
    (serialized.chars().count() / 4) as u64
}

/// Convenience: turn a `&Path` into a `Jsonl` spec for tests/wiring.
#[allow(dead_code)]
pub fn jsonl_spec(path: &Path) -> MetricsSinkSpec {
    MetricsSinkSpec::Jsonl(path.to_path_buf())
}

// =====================================================================================
// Tests — Day 7 commit 1: 4 unit tests per D-7.8.
// =====================================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_event() -> MetricsEvent {
        MetricsEvent {
            schema_version: 1,
            ts: "2026-05-27T19:14:08.412Z".to_string(),
            tool: "recent_sessions".to_string(),
            query: None,
            params: serde_json::json!({ "limit": 5, "days": 30 }),
            wall_ms: 12,
            stages_ms: serde_json::json!({
                "fts5_query": 0,
                "rank": 0,
                "join": 3,
                "shape_response": 9
            }),
            candidate_count: serde_json::json!({ "fts5": 0, "after_rank": 5 }),
            result_token_estimate: 412,
            cache_hit: None,
            daemon_uptime_s: 47,
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
        }
    }

    fn sample_error_event() -> MetricsEvent {
        let mut event = sample_event();
        event.tool = "find_session".to_string();
        event.query = Some("\"D-7\"".to_string());
        event.candidate_count = serde_json::json!({ "fts5": 0, "after_rank": 0 });
        event.result_token_estimate = 0;
        event.outcome = Some("error");
        event.error = Some(ErrorDetail {
            kind: "fts5_syntax".to_string(),
            message: "no such column: 7".to_string(),
        });
        event
    }

    /// G — `MetricsSinkSpec::from_str` accepts the three documented
    /// forms and rejects everything else with a meaningful error.
    /// Covers commit-1 gate "enum FromStr".
    #[test]
    fn metrics_sink_spec_from_str_round_trip() {
        assert_eq!("off".parse::<MetricsSinkSpec>().unwrap(), MetricsSinkSpec::Off);
        assert_eq!(
            "stdout".parse::<MetricsSinkSpec>().unwrap(),
            MetricsSinkSpec::Stdout
        );
        assert_eq!(
            "jsonl:/tmp/x.jsonl".parse::<MetricsSinkSpec>().unwrap(),
            MetricsSinkSpec::Jsonl(PathBuf::from("/tmp/x.jsonl"))
        );

        // Empty path after `jsonl:` is rejected — common typo case.
        let err = "jsonl:".parse::<MetricsSinkSpec>().unwrap_err();
        assert!(
            err.to_string().contains("requires a path"),
            "unexpected error: {err}"
        );

        // Unknown form rejected with the documented prefix list.
        let err = "garbage".parse::<MetricsSinkSpec>().unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("off") && msg.contains("stdout") && msg.contains("jsonl"),
            "unexpected error: {msg}"
        );
    }

    /// G — `MetricsSink::from_spec(Jsonl(...))` errors on an unopenable
    /// path and succeeds on a writable temp path. Covers commit-1 gate
    /// "JsonlFile open-or-error".
    #[test]
    fn metrics_sink_jsonl_open_or_error() {
        // A path under a nonexistent directory cannot be opened in
        // append+create mode (the parent dir must exist). Avoid
        // `unwrap_err()` so `MetricsSink` itself doesn't need `Debug`
        // (the `JsonlFile` variant wraps `BufWriter<File>` which is
        // not `Debug` — and Debug on a file handle is low signal).
        let bad = MetricsSinkSpec::Jsonl(PathBuf::from(
            "/this/directory/should/not/exist/nibdex.jsonl",
        ));
        match MetricsSink::from_spec(bad) {
            Err(err) => assert!(
                err.to_string().contains("opening metrics jsonl sink"),
                "unexpected error: {err:#}"
            ),
            Ok(_) => panic!("expected open error for nonexistent parent dir"),
        }

        // Writable temp path succeeds.
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("nibdex.jsonl");
        let sink = MetricsSink::from_spec(MetricsSinkSpec::Jsonl(path.clone()))
            .expect("open jsonl sink");
        assert!(matches!(sink, MetricsSink::JsonlFile(_)));
        assert!(path.exists(), "jsonl file should be created on open");
    }

    /// G — Emitted JSONL line round-trips through serde_json with all
    /// 12 fields preserved + v1 schema constants intact. Covers commit-1
    /// gate "emission shape round-trip".
    #[test]
    fn metrics_event_emission_shape_round_trip() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("nibdex.jsonl");
        let sink =
            MetricsSink::from_spec(MetricsSinkSpec::Jsonl(path.clone())).expect("open sink");
        let event = sample_event();
        sink.emit(&event).expect("emit");
        drop(sink); // close underlying file so the read below sees flushed bytes.

        let raw = std::fs::read_to_string(&path).expect("read jsonl");
        let line = raw.trim_end_matches('\n');
        let parsed: serde_json::Value = serde_json::from_str(line).expect("parse jsonl line");

        // 12 fields per D-7.2, all present.
        for key in [
            "schema_version",
            "ts",
            "tool",
            "query",
            "params",
            "wall_ms",
            "stages_ms",
            "candidate_count",
            "result_token_estimate",
            "cache_hit",
            "daemon_uptime_s",
            "calibration_confidence",
        ] {
            assert!(parsed.get(key).is_some(), "missing field: {key}");
        }
        assert_eq!(parsed["schema_version"], 1);
        assert_eq!(parsed["calibration_confidence"], "estimated");
        assert_eq!(parsed["tool"], "recent_sessions");
        // `stages_ms` carries the 4 documented keys (D-7.3) — verified
        // at the schema level so commit-2 emission stays consistent.
        for stage in ["fts5_query", "rank", "join", "shape_response"] {
            assert!(
                parsed["stages_ms"].get(stage).is_some(),
                "stages_ms missing key: {stage}"
            );
        }
        // `candidate_count` carries the 2 documented keys (D-7.3).
        assert!(parsed["candidate_count"].get("fts5").is_some());
        assert!(parsed["candidate_count"].get("after_rank").is_some());
    }

    /// G — D-8.7 additive Layer-2 fields are omitted from the JSONL line
    /// when `None` (`skip_serializing_if`). Covers commit-2 gate "Layer 2
    /// optional fields omitted when None".
    #[test]
    fn layer2_fields_omitted_when_none() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("d8.jsonl");
        let sink = MetricsSink::from_spec(MetricsSinkSpec::Jsonl(path.clone())).expect("open");
        let event = sample_event(); // all Layer-2 fields None
        sink.emit(&event).expect("emit");
        drop(sink);
        let raw = std::fs::read_to_string(&path).expect("read");
        let parsed: serde_json::Value =
            serde_json::from_str(raw.trim_end_matches('\n')).expect("parse");
        for absent in [
            "counterfactual_tokens_p50",
            "counterfactual_tokens_p95",
            "tokens_saved_p50",
            "dollars_saved_p50_usd",
            "counterfactual_wall_ms_p50",
            "calibration_model_version",
        ] {
            assert!(
                parsed.get(absent).is_none(),
                "Layer-2 field {absent} should be omitted when None: {parsed}"
            );
        }
        // schema_version stays at 1 — additive fields don't bump it.
        assert_eq!(parsed["schema_version"], 1);
    }

    /// G — D-8.7 additive Layer-2 fields land on the JSONL line when
    /// populated; signed `tokens_saved_p50` round-trips through serde.
    /// Covers commit-2 gate "Layer 2 optional fields serialized when Some"
    /// + the signedness invariant from D-8.4.
    #[test]
    fn layer2_fields_serialized_when_some_including_negative_savings() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("d8b.jsonl");
        let sink = MetricsSink::from_spec(MetricsSinkSpec::Jsonl(path.clone())).expect("open");
        let mut event = sample_event();
        event.counterfactual_tokens_p50 = Some(60_000);
        event.counterfactual_tokens_p95 = Some(85_000);
        event.tokens_saved_p50 = Some(-1_234); // result_tokens > counterfactual case
        event.dollars_saved_p50_usd = Some(-0.003702);
        event.counterfactual_wall_ms_p50 = Some(8_500);
        event.calibration_model_version = Some("v0.1-2026-05-27".to_string());
        sink.emit(&event).expect("emit");
        drop(sink);
        let raw = std::fs::read_to_string(&path).expect("read");
        let parsed: serde_json::Value =
            serde_json::from_str(raw.trim_end_matches('\n')).expect("parse");
        assert_eq!(parsed["counterfactual_tokens_p50"], 60_000);
        assert_eq!(parsed["counterfactual_tokens_p95"], 85_000);
        assert_eq!(parsed["tokens_saved_p50"], -1_234);
        assert!(parsed["dollars_saved_p50_usd"].as_f64().unwrap() < 0.0);
        assert_eq!(parsed["counterfactual_wall_ms_p50"], 8_500);
        assert_eq!(parsed["calibration_model_version"], "v0.1-2026-05-27");
        assert_eq!(parsed["schema_version"], 1);
    }

    /// G7.7 — Error events serialize with `outcome: "error"` + structured
    /// `error: {kind, message}`, alongside the full Layer-1 envelope
    /// (schema_version=1, tool, query, params, stages, wall_ms,
    /// daemon_uptime_s, ts, calibration_confidence). Covers Day 8.5 gate
    /// "error path emits one JSONL line per failed call".
    #[test]
    fn error_event_serializes_with_outcome_and_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("err.jsonl");
        let sink =
            MetricsSink::from_spec(MetricsSinkSpec::Jsonl(path.clone())).expect("open sink");
        let event = sample_error_event();
        sink.emit(&event).expect("emit");
        drop(sink);

        let raw = std::fs::read_to_string(&path).expect("read jsonl");
        let parsed: serde_json::Value =
            serde_json::from_str(raw.trim_end_matches('\n')).expect("parse jsonl line");

        assert_eq!(parsed["schema_version"], 1);
        assert_eq!(parsed["calibration_confidence"], "estimated");
        assert_eq!(parsed["outcome"], "error");
        assert_eq!(parsed["error"]["kind"], "fts5_syntax");
        assert_eq!(parsed["error"]["message"], "no such column: 7");
        assert_eq!(parsed["tool"], "find_session");
        assert_eq!(parsed["query"], "\"D-7\"");
        assert_eq!(parsed["candidate_count"]["fts5"], 0);
        assert_eq!(parsed["candidate_count"]["after_rank"], 0);
        assert_eq!(parsed["result_token_estimate"], 0);
        // Layer-2 stays absent on error rows — errors have no
        // counterfactual to clamp against (§11 honesty).
        for absent in [
            "counterfactual_tokens_p50",
            "tokens_saved_p50",
            "dollars_saved_p50_usd",
            "calibration_model_version",
        ] {
            assert!(
                parsed.get(absent).is_none(),
                "Layer-2 field {absent} should be absent on error rows: {parsed}"
            );
        }
    }

    /// G7.7 — Success envelopes stay byte-identical to the Day 7 12-key
    /// schema: `outcome` and `error` are absent (skip_serializing_if).
    /// Guards the additive-not-breaking invariant from §5.5 + D-8.7 +
    /// Day 8.5 plan. Without this guard, a future contributor could
    /// flip `outcome` to a non-Option type and silently break every
    /// existing Day-7-era consumer.
    #[test]
    fn success_event_omits_outcome_and_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("ok.jsonl");
        let sink =
            MetricsSink::from_spec(MetricsSinkSpec::Jsonl(path.clone())).expect("open sink");
        let event = sample_event(); // outcome/error both None
        sink.emit(&event).expect("emit");
        drop(sink);

        let raw = std::fs::read_to_string(&path).expect("read jsonl");
        let parsed: serde_json::Value =
            serde_json::from_str(raw.trim_end_matches('\n')).expect("parse jsonl line");

        assert!(
            parsed.get("outcome").is_none(),
            "outcome must be absent on success envelopes: {parsed}"
        );
        assert!(
            parsed.get("error").is_none(),
            "error must be absent on success envelopes: {parsed}"
        );
        // schema_version stays at 1 — additive fields don't bump it.
        assert_eq!(parsed["schema_version"], 1);
    }

    /// G7.7 — `classify_handler_error` maps documented dogfood failure
    /// modes into the right bucket. FTS5 hyphen-as-NOT cliff (#628
    /// sub-finding a) is the load-bearing case; the others guard the
    /// `sqlite` and `internal` fallbacks.
    #[test]
    fn classify_handler_error_buckets() {
        let cases = [
            ("no such column: 7", "fts5_syntax"),
            ("fts5: syntax error near \"-\"", "fts5_syntax"),
            ("malformed MATCH expression", "fts5_syntax"),
            ("unrecognized token: \"-\"", "fts5_syntax"),
            ("syntax error near 'WHERE'", "fts5_syntax"),
            ("sqlx error: connection refused", "sqlite"),
            ("SQLite: database is locked", "sqlite"),
            ("database table not found", "sqlite"),
            ("unexpected None during shape_response", "internal"),
            ("anyhow: shutdown signal during fetch", "internal"),
        ];
        for (msg, expected) in cases {
            assert_eq!(
                classify_handler_error(msg),
                expected,
                "msg={msg:?} expected={expected}"
            );
        }
    }

    /// G7.7 — `truncate_error_message` caps at 500 chars on a UTF-8
    /// boundary, never panics on multi-byte input.
    #[test]
    fn truncate_error_message_honors_500_char_cap() {
        // Short message passes through verbatim.
        assert_eq!(truncate_error_message("short"), "short");
        // Exactly-500-char message stays full.
        let exact = "a".repeat(500);
        assert_eq!(truncate_error_message(&exact).chars().count(), 500);
        // 600-char message truncates to 500.
        let big = "b".repeat(600);
        assert_eq!(truncate_error_message(&big).chars().count(), 500);
        // Multi-byte char (Cyrillic, 2 bytes / 1 char) at boundary
        // doesn't panic — uses char count not byte count.
        let multi = "\u{0444}".repeat(600); // 600 chars, 1200 bytes
        assert_eq!(truncate_error_message(&multi).chars().count(), 500);
    }

    /// G — `MetricsSink::Disabled` emission is a no-op: returns Ok and
    /// touches no file. Covers commit-1 gate "disabled no-op".
    #[test]
    fn metrics_sink_disabled_emit_is_no_op() {
        let sink = MetricsSink::from_spec(MetricsSinkSpec::Off).expect("open disabled");
        assert!(matches!(sink, MetricsSink::Disabled));
        let event = sample_event();
        // Calling emit on Disabled is harmless + repeated calls stay
        // harmless (no transient state to corrupt).
        for _ in 0..10 {
            sink.emit(&event).expect("disabled emit returns Ok");
        }
    }
}
