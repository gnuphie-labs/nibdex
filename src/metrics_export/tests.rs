// SPDX-License-Identifier: MIT

//! Tests for the metrics-export scrub. Three jobs:
//!  1. **Transforms** behave per §5 (bucket ladder edges, operator detection).
//!  2. **IP never leaks** — the scrubbed payload carries no query text, error
//!     message, path, or out-of-window row, and losses survive (§6).
//!  3. **Drift guard** (§7.3) — the field-name set of every internal source
//!     struct equals the set this contract classifies, so a newly-added field
//!     fails the build until it is deliberately classified (default-deny).

use std::collections::BTreeSet;

use chrono::{TimeZone, Utc};
use serde_json::{Value, json};

use super::*;

// --------------------------------------------------------------------------
// §5 transforms
// --------------------------------------------------------------------------

#[test]
fn count_bucket_ladder_edges() {
    assert_eq!(count_bucket(0), "0");
    assert_eq!(count_bucket(1), "1");
    assert_eq!(count_bucket(2), "2-3");
    assert_eq!(count_bucket(3), "2-3");
    assert_eq!(count_bucket(4), "4-7");
    assert_eq!(count_bucket(63), "32-63");
    assert_eq!(count_bucket(64), "64-127");
    assert_eq!(count_bucket(1023), "512-1023");
    assert_eq!(count_bucket(1024), "1024-4095");
    assert_eq!(count_bucket(65535), "16384-65535");
    assert_eq!(count_bucket(65536), "65536+");
    assert_eq!(count_bucket(10_000_000), "65536+");
    // A stray negative must not leak as a distinct bucket.
    assert_eq!(count_bucket(-5), "0");
}

#[test]
fn length_bucket_uses_the_same_ladder() {
    assert_eq!(length_bucket(20), count_bucket(20));
    assert_eq!(length_bucket(20), "16-31");
}

#[test]
fn fts5_operator_detection() {
    assert!(!had_fts5_operators("plain two terms"));
    assert!(had_fts5_operators("foo OR bar"));
    assert!(had_fts5_operators("foo NOT bar"));
    assert!(had_fts5_operators("foo NEAR/3 bar"));
    assert!(had_fts5_operators("\"quoted phrase\""));
    assert!(had_fts5_operators("prefix*"));
    assert!(had_fts5_operators("col:term"));
    // lowercase "or" is a literal term in FTS5, not an operator.
    assert!(!had_fts5_operators("apples or oranges"));
}

// --------------------------------------------------------------------------
// scrub_row — the IP surface
// --------------------------------------------------------------------------

/// A realistic raw row carrying corpus codenames + a verbatim error message.
fn sensitive_raw_row() -> Value {
    json!({
        "schema_version": 1,
        "ts": "2026-06-04T11:00:00.123Z",
        "tool": "find_session",
        "query": "acme-dashboard formsvc-cf",
        "params": { "query_len": 25, "limit_requested": 10 },
        "wall_ms": 7,
        "stages_ms": { "fts5_query": 2, "rank": 1, "join": 3, "shape_response": 1 },
        "candidate_count": { "fts5": 0, "after_rank": 0 },
        "result_token_estimate": 120,
        "cache_hit": null,
        "daemon_uptime_s": 4242,
        "calibration_confidence": "estimated",
        "query_broadened": true,
        "outcome": "error",
        "error": { "kind": "fts5_syntax", "message": "no such column: dashboard" }
    })
}

#[test]
fn scrub_row_drops_query_text_message_and_ts() {
    let row = scrub_row(&sensitive_raw_row());
    let v = serde_json::to_value(&row).unwrap();
    let s = serde_json::to_string(&v).unwrap();

    // No raw query, no codename, no error message, no timestamp.
    assert!(!s.contains("acme"), "leaked query codename: {s}");
    assert!(!s.contains("dashboard"), "leaked query/message term: {s}");
    assert!(!s.contains("formsvc"), "leaked query codename: {s}");
    assert!(!s.contains("no such column"), "leaked error message: {s}");
    assert!(v.get("ts").is_none(), "ts must be dropped");
    assert!(v.get("query").is_none(), "raw query must be dropped");
    assert!(v["params"].get("query_len").is_none(), "query_len folded into shape");

    // The shape survives, derived only — term_count bucketed (not exact).
    assert_eq!(v["query_shape"]["term_count"], "2-3");
    assert_eq!(v["query_shape"]["length_bucket"], "16-31");
    assert_eq!(v["query_shape"]["and_matched_zero"], true);
    assert_eq!(v["query_broadened"], true);
    // error.kind survives; error.message does not.
    assert_eq!(v["error"]["kind"], "fts5_syntax");
    assert!(v["error"].get("message").is_none());
}

#[test]
fn scrub_row_clamps_unknown_error_kind_to_unknown() {
    // A schema-drift / tampered row whose error.kind embeds a codename must NOT
    // ride through verbatim — it collapses to "unknown" (allowlist-on-read).
    let mut raw = sensitive_raw_row();
    raw["error"] = json!({ "kind": "no such column: acme_dashboard_q3", "message": "x" });
    let v = serde_json::to_value(scrub_row(&raw)).unwrap();
    assert_eq!(v["error"]["kind"], "unknown");
    let s = serde_json::to_string(&v).unwrap();
    assert!(!s.contains("acme"), "leaked codename via error.kind: {s}");
    // A legitimate bucket is preserved.
    raw["error"] = json!({ "kind": "sqlite", "message": "x" });
    let v = serde_json::to_value(scrub_row(&raw)).unwrap();
    assert_eq!(v["error"]["kind"], "sqlite");
}

#[test]
fn scrub_row_drops_tampered_literal_fields() {
    // tool / outcome / calibration_confidence must not echo a tampered value.
    let mut raw = sensitive_raw_row();
    raw["tool"] = json!("/Users/x/secret-proj");
    raw["outcome"] = json!("acme-dashboard");
    raw["calibration_confidence"] = json!("formsvc-cf");
    raw["params"] = json!({ "filter_set": "acme", "days": "formsvc", "limit_requested": {} });
    let v = serde_json::to_value(scrub_row(&raw)).unwrap();
    let s = serde_json::to_string(&v).unwrap();
    assert_eq!(v["tool"], "unknown");
    assert!(v.get("outcome").is_none(), "tampered outcome must be dropped");
    assert_eq!(v["calibration_confidence"], "estimated");
    // Forged string params under allowlisted keys coerce to None and vanish.
    assert!(v["params"].get("filter_set").is_none());
    assert!(v["params"].get("days").is_none());
    assert!(!s.contains("acme") && !s.contains("formsvc") && !s.contains("secret-proj"), "leak: {s}");
}

#[test]
fn scrub_row_preserves_signed_loss() {
    let mut raw = sensitive_raw_row();
    raw["tokens_saved_p50"] = json!(-1234);
    raw["dollars_saved_p50_usd"] = json!(-0.05);
    let v = serde_json::to_value(scrub_row(&raw)).unwrap();
    assert_eq!(v["tokens_saved_p50"], -1234, "a loss must survive (§6.2)");
    assert_eq!(v["dollars_saved_p50_usd"], -0.05);
}

#[test]
fn scrub_row_without_query_omits_shape() {
    // A check()-style row: no query, no FTS5 path.
    let raw = json!({
        "schema_version": 1, "ts": "2026-06-04T11:00:00Z", "tool": "check",
        "params": {}, "wall_ms": 1,
        "stages_ms": { "fts5_query": 0, "rank": 0, "join": 0, "shape_response": 1 },
        "candidate_count": { "fts5": 0, "after_rank": 0 },
        "result_token_estimate": 0, "daemon_uptime_s": 10,
        "calibration_confidence": "estimated"
    });
    let v = serde_json::to_value(scrub_row(&raw)).unwrap();
    assert!(v.get("query_shape").is_none(), "no query → no query_shape");
    assert!(v.get("query_broadened").is_none(), "off FTS5 path → absent");
}

// --------------------------------------------------------------------------
// run_export — end-to-end
// --------------------------------------------------------------------------

#[tokio::test]
async fn run_export_scrubs_windows_and_keeps_losses() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("export.db");
    let jsonl = tmp.path().join("metrics.jsonl");
    let out = tmp.path().join("payload.json");

    let in_window = "2026-06-04T11:00:00.000Z";
    let now = Utc.with_ymd_and_hms(2026, 6, 4, 12, 0, 0).unwrap();

    let rows = [
        // (1) in-window success carrying a savings figure
        json!({
            "schema_version": 1, "ts": in_window, "tool": "find_session",
            "query": "acme dashboard", "params": { "query_len": 15, "limit_requested": 10 },
            "wall_ms": 6, "stages_ms": {"fts5_query":2,"rank":1,"join":2,"shape_response":1},
            "candidate_count": {"fts5":2,"after_rank":2}, "result_token_estimate": 300,
            "daemon_uptime_s": 100, "calibration_confidence": "estimated",
            "query_broadened": false, "tokens_saved_p50": 5000
        }),
        // (2) in-window error with a verbatim message
        json!({
            "schema_version": 1, "ts": in_window, "tool": "find_memory",
            "query": "formsvc-cf", "params": { "query_len": 12 },
            "wall_ms": 3, "stages_ms": {"fts5_query":1,"rank":0,"join":0,"shape_response":0},
            "candidate_count": {"fts5":0,"after_rank":0}, "result_token_estimate": 0,
            "daemon_uptime_s": 101, "calibration_confidence": "estimated",
            "outcome": "error", "error": {"kind":"fts5_syntax","message":"no such column: formsvc"}
        }),
        // (3) in-window LOSS + broadened
        json!({
            "schema_version": 1, "ts": in_window, "tool": "find_commit",
            "query": "zeta", "params": { "query_len": 4 },
            "wall_ms": 4, "stages_ms": {"fts5_query":1,"rank":0,"join":1,"shape_response":0},
            "candidate_count": {"fts5":0,"after_rank":0}, "result_token_estimate": 99,
            "daemon_uptime_s": 102, "calibration_confidence": "estimated",
            "query_broadened": true, "tokens_saved_p50": -1200, "dollars_saved_p50_usd": -0.03
        }),
        // (4) OUT of the 1-day window — must be excluded
        json!({
            "schema_version": 1, "ts": "2020-01-01T00:00:00Z", "tool": "find_session",
            "query": "OLDSECRET", "params": { "query_len": 9 },
            "wall_ms": 5, "stages_ms": {"fts5_query":1,"rank":0,"join":0,"shape_response":0},
            "candidate_count": {"fts5":1,"after_rank":1}, "result_token_estimate": 50,
            "daemon_uptime_s": 1, "calibration_confidence": "estimated"
        }),
    ];
    let body: String = rows
        .iter()
        .map(|r| serde_json::to_string(r).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&jsonl, body).unwrap();

    let pool = crate::db::open(&db_path).await.unwrap();
    let summary = run_export(&pool, None, &jsonl, Some(1), now, &out)
        .await
        .expect("export ok");
    pool.close().await;

    assert_eq!(summary.rows_total, 4);
    assert_eq!(summary.rows_in_window, 3, "the 2020 row is out of the 1d window");
    assert_eq!(summary.error_rows, 1);
    assert_eq!(summary.loss_rows, 1);

    let raw = std::fs::read_to_string(&out).unwrap();
    // No IP anywhere in the file.
    for needle in ["acme", "dashboard", "formsvc", "no such column", "OLDSECRET", "zeta"] {
        assert!(!raw.contains(needle), "payload leaked {needle:?}");
    }

    let v: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(v["nibdex_metrics_export"]["format_version"], 1);
    assert_eq!(v["nibdex_metrics_export"]["window_days"], 1);
    assert!(v["legend"]["count_bucket_ladder"].is_string());
    // Snapshot present; bucketed counts are strings (empty corpus → "0").
    assert_eq!(v["snapshot"]["indexer"]["search_index_total"], "0");

    let calls = v["calls"].as_array().unwrap();
    assert_eq!(calls.len(), 3);
    for c in calls {
        assert!(c.get("ts").is_none());
        assert!(c.get("query").is_none());
    }
    // The loss survived with its sign.
    let loss = calls.iter().find(|c| c["tool"] == "find_commit").unwrap();
    assert_eq!(loss["tokens_saved_p50"], -1200);
    assert_eq!(loss["query_broadened"], true);
    assert_eq!(loss["query_shape"]["and_matched_zero"], true);
    // The error row kept kind, dropped message.
    let err = calls.iter().find(|c| c["tool"] == "find_memory").unwrap();
    assert_eq!(err["error"]["kind"], "fts5_syntax");
    assert!(err["error"].get("message").is_none());
}

// --------------------------------------------------------------------------
// §2 map-key allowlist — enforce the closed-key-domain at the export boundary
// --------------------------------------------------------------------------

#[test]
fn safe_map_key_accepts_nibdex_keys_rejects_ip() {
    // nibdex-controlled keys (doc types, tool.* / extract.* op names).
    for k in ["session_history", "memory", "design_doc", "tool.find_session", "extract.commits"] {
        assert!(is_safe_map_key(k), "should accept {k}");
    }
    // IP-shaped keys: paths, uppercase, spaces, hyphenated codenames, empties.
    for k in [
        "/Users/x/secret-proj", "acme-dashboard", "Formsvc", "two words", "", &"x".repeat(65),
    ] {
        assert!(!is_safe_map_key(k), "should reject {k:?}");
    }
}

#[test]
fn scrub_snapshot_drops_unsafe_map_keys() {
    use crate::mcp::{CheckResult, IndexerCounts, OrphanCounts};
    use std::collections::BTreeMap;

    let check = CheckResult {
        schema_version: 1,
        daemon_uptime_s: 0,
        indexer: IndexerCounts {
            // a legit doc-type key + an injected path-shaped key
            documents: BTreeMap::from([
                ("session_history".to_string(), 3),
                ("/Users/you/work/acme-dashboard".to_string(), 1),
            ]),
            session_entries: 3,
            session_edges: 0,
            memory_entries: 0,
            design_doc_sections: 0,
            source_chunks: 0,
            commit_entries: 0,
            indexed_repos: 0,
            search_index_total: 3,
        },
        orphans: OrphanCounts {
            session_entries: 0,
            memory_entries: 0,
            design_doc_sections: 0,
            indexed_repos: 0,
        },
        shallow_repos: vec![],
        perf_p50_ms: BTreeMap::from([
            ("tool.find_session".to_string(), 5),
            ("/evil/path".to_string(), 9),
        ]),
        perf_p95_ms: BTreeMap::new(),
        file_watcher: None,
        extractors_last_run_ms: BTreeMap::new(),
        cost_savings: None,
        build: crate::build_info::build_info(),
    };

    let (snap, dropped) = scrub_snapshot(&check);
    assert_eq!(dropped, 2, "the path-shaped documents key + the /evil/path perf key");
    let v = serde_json::to_value(&snap).unwrap();
    let s = serde_json::to_string(&v).unwrap();
    assert!(!s.contains("acme"), "leaked a documents key: {s}");
    assert!(!s.contains("evil"), "leaked a perf key: {s}");
    // the safe keys survive
    assert!(v["indexer"]["documents"].get("session_history").is_some());
    assert!(v["perf_p50_ms"].get("tool.find_session").is_some());
}

// --------------------------------------------------------------------------
// §7.3 drift guard — the load-bearing default-deny mechanism
// --------------------------------------------------------------------------

fn keys(v: &Value) -> BTreeSet<String> {
    v.as_object()
        .expect("expected a JSON object")
        .keys()
        .cloned()
        .collect()
}

fn expected(fields: &[&str]) -> BTreeSet<String> {
    fields.iter().map(|s| s.to_string()).collect()
}

/// Every `MetricsEvent` field is classified in METRICS_EXPORT_SPEC §4.1.
/// Adding a field to the struct without classifying it fails HERE.
#[test]
fn drift_metrics_event_fields_are_all_classified() {
    use crate::metrics_sink::{ErrorDetail, MetricsEvent};
    // Fully-populated so every skip_serializing_if Option appears.
    let ev = MetricsEvent {
        schema_version: 1,
        ts: "2026-06-04T00:00:00Z".to_string(),
        tool: "find_session".to_string(),
        query: Some("q".to_string()),
        params: json!({}),
        wall_ms: 1,
        stages_ms: json!({"fts5_query":0,"rank":0,"join":0,"shape_response":0}),
        candidate_count: json!({"fts5":0,"after_rank":0}),
        result_token_estimate: 0,
        cache_hit: Some(false),
        daemon_uptime_s: 0,
        calibration_confidence: "estimated",
        counterfactual_tokens_p50: Some(1),
        counterfactual_tokens_p95: Some(1),
        tokens_saved_p50: Some(1),
        dollars_saved_p50_usd: Some(1.0),
        counterfactual_wall_ms_p50: Some(1),
        calibration_model_version: Some("v".to_string()),
        query_broadened: Some(true),
        returned_full_tokens: Some(1),
        outcome: Some("error"),
        error: Some(ErrorDetail { kind: "k".to_string(), message: "m".to_string() }),
    };
    let classified = expected(&[
        "schema_version", "ts", "tool", "query", "params", "wall_ms", "stages_ms",
        "candidate_count", "result_token_estimate", "cache_hit", "daemon_uptime_s",
        "calibration_confidence", "counterfactual_tokens_p50", "counterfactual_tokens_p95",
        "tokens_saved_p50", "dollars_saved_p50_usd", "counterfactual_wall_ms_p50",
        "calibration_model_version", "query_broadened", "returned_full_tokens", "outcome", "error",
    ]);
    assert_eq!(
        keys(&serde_json::to_value(&ev).unwrap()),
        classified,
        "MetricsEvent fields drifted from METRICS_EXPORT_SPEC §4.1 — classify the new field first"
    );
}

/// Every `CheckResult` field (and the nested structs the contract classifies)
/// is accounted for in METRICS_EXPORT_SPEC §4.2.
#[test]
fn drift_check_result_fields_are_all_classified() {
    use crate::cost_ledger::{CostSavingsLedger, CostSavingsPerTool, CostSavingsWindow};
    use crate::mcp::{CheckResult, FileWatcherStats, IndexerCounts, OrphanCounts};
    use std::collections::BTreeMap;

    let window = || CostSavingsWindow {
        queries_served: 1,
        tokens_returned: 1,
        counterfactual_tokens_p50: 1,
        tokens_saved_p50: 1,
        dollars_saved_p50_usd: 1.0,
        per_tool: BTreeMap::from([(
            "find_session".to_string(),
            CostSavingsPerTool { queries: 1, tokens_saved_p50: 1, dollars_saved_p50_usd: 1.0 },
        )]),
    };
    let check = CheckResult {
        schema_version: 1,
        daemon_uptime_s: 1,
        indexer: IndexerCounts {
            documents: BTreeMap::from([("session_history".to_string(), 1)]),
            session_entries: 1,
            session_edges: 1,
            memory_entries: 1,
            design_doc_sections: 1,
            source_chunks: 1,
            commit_entries: 1,
            indexed_repos: 1,
            search_index_total: 1,
        },
        orphans: OrphanCounts {
            session_entries: 0,
            memory_entries: 0,
            design_doc_sections: 0,
            indexed_repos: 0,
        },
        shallow_repos: vec!["/path/to/repo".to_string()],
        perf_p50_ms: BTreeMap::new(),
        perf_p95_ms: BTreeMap::new(),
        file_watcher: Some(FileWatcherStats {
            events_total: 1,
            events_coalesced_total: 1,
            last_event_ts: Some(1),
            subscriptions: vec!["/watched/path".to_string()],
        }),
        extractors_last_run_ms: BTreeMap::new(),
        cost_savings: Some(CostSavingsLedger {
            calibration_model_version: "v".to_string(),
            window_1d: window(),
            window_7d: window(),
            window_30d: window(),
        }),
        build: crate::build_info::build_info(),
    };
    let v = serde_json::to_value(&check).unwrap();

    assert_eq!(
        keys(&v),
        expected(&[
            "schema_version", "daemon_uptime_s", "indexer", "orphans", "shallow_repos",
            "perf_p50_ms", "perf_p95_ms", "file_watcher", "extractors_last_run_ms", "cost_savings",
            // `build` is classified DROP in §4.2 — accounted for here (so this
            // guard stays exhaustive) but never construct-fresh'd into the
            // export payload (see scrub_snapshot).
            "build",
        ]),
        "CheckResult fields drifted from METRICS_EXPORT_SPEC §4.2"
    );
    assert_eq!(
        keys(&v["indexer"]),
        expected(&[
            "documents", "session_entries", "session_edges", "memory_entries",
            "design_doc_sections", "source_chunks", "commit_entries", "indexed_repos",
            "search_index_total",
        ]),
        "IndexerCounts drifted"
    );
    assert_eq!(
        keys(&v["orphans"]),
        expected(&["session_entries", "memory_entries", "design_doc_sections", "indexed_repos"]),
        "OrphanCounts drifted"
    );
    assert_eq!(
        keys(&v["file_watcher"]),
        expected(&["events_total", "events_coalesced_total", "last_event_ts", "subscriptions"]),
        "FileWatcherStats drifted — note last_event_ts + subscriptions must stay DROP/count-only"
    );
    assert_eq!(
        keys(&v["cost_savings"]),
        expected(&["calibration_model_version", "window_1d", "window_7d", "window_30d"]),
        "CostSavingsLedger drifted"
    );
    assert_eq!(
        keys(&v["cost_savings"]["window_1d"]),
        expected(&[
            "queries_served", "tokens_returned", "counterfactual_tokens_p50",
            "tokens_saved_p50", "dollars_saved_p50_usd", "per_tool",
        ]),
        "CostSavingsWindow drifted"
    );
    assert_eq!(
        keys(&v["cost_savings"]["window_1d"]["per_tool"]["find_session"]),
        expected(&["queries", "tokens_saved_p50", "dollars_saved_p50_usd"]),
        "CostSavingsPerTool drifted"
    );
}
