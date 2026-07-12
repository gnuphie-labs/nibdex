// SPDX-License-Identifier: MIT

//! `nibdex metrics-export` — the IP-safe, opt-in metrics handoff.
//!
//! Engineering realization of the field-by-field allow/deny contract in
//! `docs/METRICS_EXPORT_SPEC.md`. The maintainer runs nibdex against a real
//! (possibly proprietary) corpus; this command derives a **scrubbed, readable,
//! self-describing** payload that improves nibdex while carrying zero IP.
//!
//! Two governing rules from the spec, enforced here:
//!  1. **ALLOWLIST / default-deny (§2).** The payload is built from fresh
//!     `Export*` structs, field by field — never by serializing an internal
//!     struct and post-filtering. A field added to `MetricsEvent` /
//!     `CheckResult` later is *dropped by default*; the drift test in
//!     `tests` fails the build until it is classified in the spec.
//!  2. **Zero network egress (§6.4).** This writes a *file*. nibdex never
//!     transmits it — "sharing" is the user inspecting that file and choosing
//!     to hand it over (§7.2 approval flow). The loopback-only posture holds.
//!
//! The two sources are `MetricsEvent` rows (the JSONL stream) and a recomputed
//! `CheckResult` snapshot. Per-call rows are read as `serde_json::Value` and
//! only allowlisted keys are pulled (`MetricsEvent` is `Serialize`-only); the
//! snapshot is scrubbed from the typed `CheckResult`. Both paths name every
//! field they keep — nothing rides through implicitly.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::SqlitePool;

use crate::calibration::CalibrationModel;
use crate::mcp::{CheckResult, Stages, run_check};

/// Pinned reference to the contract this command realizes. The format_version
/// rides in the payload header so a reader can pin the scrub semantics.
const SCRUB_SPEC: &str = "docs/METRICS_EXPORT_SPEC.md";
const FORMAT_VERSION: u32 = 1;

/// The §5.2 count-bucket ladder (powers-of-two edges, widening at the top).
/// Shipped verbatim in the payload legend so a reader sees the granularity.
const BUCKET_LADDER: &str = "0 · 1 · 2-3 · 4-7 · 8-15 · 16-31 · 32-63 · 64-127 · \
128-255 · 256-511 · 512-1023 · 1024-4095 · 4096-16383 · 16384-65535 · 65536+";

/// Transparency summary returned to the caller (printed by `main`), so a silent
/// truncation never reads as full coverage (§6.1 + the "no silent caps" norm).
#[derive(Debug)]
pub struct ExportSummary {
    pub out_path: PathBuf,
    pub rows_total: usize,
    pub rows_in_window: usize,
    pub rows_unparseable_ts: usize,
    pub error_rows: usize,
    pub loss_rows: usize,
    pub window_days: Option<u32>,
    /// Verbatim-map keys (`documents`/`perf`/`extractor`) dropped by the §2
    /// key allowlist. Expected 0 in normal operation; >0 flags drift/tampering.
    pub dropped_unsafe_keys: usize,
}

// =====================================================================================
// §5 transforms — pure, lossy, IP-erasing. Raw input never leaves; only the
// derived value is emitted.
// =====================================================================================

/// §5.2 count → log-scale bucket. Negative inputs clamp to 0 (counts are
/// non-negative; a stray sign must not leak through as a distinct bucket).
fn count_bucket(n: i64) -> String {
    let n = n.max(0);
    let label = match n {
        0 => "0",
        1 => "1",
        2..=3 => "2-3",
        4..=7 => "4-7",
        8..=15 => "8-15",
        16..=31 => "16-31",
        32..=63 => "32-63",
        64..=127 => "64-127",
        128..=255 => "128-255",
        256..=511 => "256-511",
        512..=1023 => "512-1023",
        1024..=4095 => "1024-4095",
        4096..=16383 => "4096-16383",
        16384..=65535 => "16384-65535",
        _ => "65536+",
    };
    label.to_string()
}

/// §5.1 query length → bucket. Uses the same §5.2 ladder (the canonical one;
/// the `16-63` shown in the §5.1 example was illustrative, not a distinct edge).
fn length_bucket(len: i64) -> String {
    count_bucket(len)
}

/// §5.1 `had_fts5_operators` — any FTS5 operator present. Word operators are
/// uppercase per FTS5 (`OR`/`NOT`/`NEAR`); the symbol set is `" * + - : ( )`.
/// Only the boolean is ever emitted — never a token of the query.
fn had_fts5_operators(query: &str) -> bool {
    let has_symbol = query
        .chars()
        .any(|c| matches!(c, '"' | '*' | '+' | '-' | ':' | '(' | ')'));
    let has_word_op = query
        .split_whitespace()
        .any(|t| matches!(t, "OR" | "NOT" | "NEAR") || t.starts_with("NEAR/"));
    has_symbol || has_word_op
}

/// Defense-in-depth allowlist for map KEYS that ride through verbatim
/// (`indexer.documents`, `perf_p*_ms`, `extractors_last_run_ms`). Their keys are
/// nibdex-controlled closed sets *today* — doc-type / `tool.*` / `extract.*`
/// names, provenance-audited 2026-06-04 — but §2 default-deny must be *enforced*
/// at the export boundary, not assumed: a future dynamic key (or a tampered db)
/// must not ride a path/query/codename through as a verbatim key. A safe key is
/// identifier-ish — only `[a-z0-9_.]`, non-empty, bounded. Paths (`/`, uppercase),
/// queries (spaces), and hyphenated codenames are rejected.
fn is_safe_map_key(k: &str) -> bool {
    !k.is_empty()
        && k.len() <= 64
        && k.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'.')
}

/// Drop unsafe keys from a verbatim-key map, returning the kept map + the count
/// dropped (surfaced in the summary — no silent truncation).
fn filter_safe_keys<V>(m: &BTreeMap<String, V>) -> (BTreeMap<String, V>, usize)
where
    V: Clone,
{
    let mut dropped = 0;
    let kept = m
        .iter()
        .filter(|(k, _)| {
            let ok = is_safe_map_key(k);
            if !ok {
                dropped += 1;
            }
            ok
        })
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    (kept, dropped)
}

// =====================================================================================
// Export structs — the fresh, construct-by-field payload shapes (§7.3). A new
// internal field cannot appear here without a deliberate edit + a spec edit.
// =====================================================================================

#[derive(Debug, Serialize)]
struct Bundle {
    nibdex_metrics_export: BundleMeta,
    legend: Value,
    snapshot: ExportSnapshot,
    calls: Vec<ExportRow>,
}

#[derive(Debug, Serialize)]
struct BundleMeta {
    format_version: u32,
    scrub_spec: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    window_days: Option<u32>,
    rows_in_window: usize,
}

/// §4.1 — one scrubbed per-call row. `ts` and the raw `query` are dropped;
/// the query survives only as `query_shape` (§5.1).
#[derive(Debug, Serialize)]
struct ExportRow {
    schema_version: u64,
    tool: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    query_shape: Option<QueryShape>,
    params: ExportParams,
    wall_ms: u64,
    stages_ms: StagesMs,
    candidate_count: CandidateCount,
    result_token_estimate: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_hit: Option<bool>,
    daemon_uptime_s: u64,
    /// §4.1 MANDATORY — must ride through so no token/$ figure reads as measured.
    calibration_confidence: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    counterfactual_tokens_p50: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    counterfactual_tokens_p95: Option<u64>,
    /// Signed — a negative value (a loss) is honest data and MUST survive (§6.2).
    #[serde(skip_serializing_if = "Option::is_none")]
    tokens_saved_p50: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dollars_saved_p50_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    counterfactual_wall_ms_p50: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    calibration_model_version: Option<String>,
    /// D-10.13 retrieval-quality signal — verbatim ALLOW (a bool, IP-free).
    #[serde(skip_serializing_if = "Option::is_none")]
    query_broadened: Option<bool>,
    /// Grounded-counterfactual raw input — a token count (size), IP-free, ALLOW
    /// verbatim like `result_token_estimate`. Lets the export carry the material
    /// `nibdex rescore` needs to A/B the grounded savings model offline.
    #[serde(skip_serializing_if = "Option::is_none")]
    returned_full_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    outcome: Option<String>,
    /// Only `kind` survives — `error.message` is DROPped (echoes query terms).
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<ExportError>,
}

#[derive(Debug, Serialize)]
struct QueryShape {
    /// Bucketed via the §5.2 ladder (not exact) — the one query-derived count,
    /// laddered like every other count so it can't be an exact fingerprint.
    term_count: String,
    length_bucket: String,
    had_fts5_operators: bool,
    and_matched_zero: bool,
}

/// §4.3 — key-allowlisted. `query_len` is DROPped here (folded into
/// `query_shape.length_bucket`); the raw `filter`/`repo`/`query` strings were
/// never in `params` to begin with.
#[derive(Debug, Serialize)]
struct ExportParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    filter_set: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo_set: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    days: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    limit_requested: Option<i64>,
}

#[derive(Debug, Serialize)]
struct StagesMs {
    fts5_query: u64,
    rank: u64,
    join: u64,
    shape_response: u64,
}

#[derive(Debug, Serialize)]
struct CandidateCount {
    fts5: i64,
    after_rank: i64,
}

#[derive(Debug, Serialize)]
struct ExportError {
    kind: String,
}

/// §4.2 — scrubbed `CheckResult` snapshot. Path lists collapse to counts;
/// corpus/orphan counts bucket; wall-clock timestamps drop.
#[derive(Debug, Serialize)]
struct ExportSnapshot {
    schema_version: i64,
    /// 0 for a CLI-recomputed snapshot (one-shot); the per-call rows carry the
    /// real daemon uptime at emit time. Documented in the legend.
    daemon_uptime_s: i64,
    indexer: ExportIndexer,
    orphans: ExportOrphans,
    /// §5.3 — `shallow_repos` values are paths (UNSAFE); only the count ships.
    shallow_repo_count: usize,
    perf_p50_ms: BTreeMap<String, i64>,
    perf_p95_ms: BTreeMap<String, i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    file_watcher: Option<ExportFileWatcher>,
    extractors_last_run_ms: BTreeMap<String, i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cost_savings: Option<ExportCostSavings>,
}

/// Count values are bucket strings (§5.2); `documents` keys are doc-*type*
/// categories (not repo names — verified `check.rs`), so keys ride through.
#[derive(Debug, Serialize)]
struct ExportIndexer {
    documents: BTreeMap<String, String>,
    session_entries: String,
    memory_entries: String,
    design_doc_sections: String,
    source_chunks: String,
    commit_entries: String,
    indexed_repos: String,
    search_index_total: String,
}

#[derive(Debug, Serialize)]
struct ExportOrphans {
    session_entries: String,
    memory_entries: String,
    design_doc_sections: String,
    indexed_repos: String,
}

/// `last_event_ts` DROPped (wall-clock fingerprint); `subscriptions` → count
/// only (§5.3, absolute watched paths with project names are UNSAFE).
#[derive(Debug, Serialize)]
struct ExportFileWatcher {
    events_total: i64,
    events_coalesced_total: i64,
    subscription_count: usize,
}

/// All ALLOW — IP-free aggregates. The D-10.14 `find_design_doc` net-negative
/// finding lives in `per_tool` and MUST survive (§6.2).
#[derive(Debug, Serialize)]
struct ExportCostSavings {
    calibration_model_version: String,
    window_1d: ExportWindow,
    window_7d: ExportWindow,
    window_30d: ExportWindow,
}

#[derive(Debug, Serialize)]
struct ExportWindow {
    queries_served: i64,
    tokens_returned: i64,
    counterfactual_tokens_p50: i64,
    tokens_saved_p50: i64,
    dollars_saved_p50_usd: f64,
    per_tool: BTreeMap<String, ExportPerTool>,
}

#[derive(Debug, Serialize)]
struct ExportPerTool {
    queries: i64,
    tokens_saved_p50: i64,
    dollars_saved_p50_usd: f64,
}

// =====================================================================================
// Scrub functions — construct-fresh, field by field (§7.3).
// =====================================================================================

/// Scrub one raw `MetricsEvent` JSONL row (a `serde_json::Value`) into an
/// `ExportRow`, pulling only allowlisted keys. Missing scalars default to the
/// IP-free zero; missing `calibration_confidence` defaults to `"estimated"`
/// (it is mandatory in well-formed rows). Returns the row plus two flags the
/// caller aggregates for transparency: whether it is an error row and whether
/// it carries a loss (negative `tokens_saved_p50`).
fn scrub_row(v: &Value) -> ExportRow {
    let fts5 = v["candidate_count"]["fts5"].as_i64().unwrap_or(0);

    // query_shape: only when a raw query was present. term_count + length come
    // from the raw query / recorded query_len; nothing of the text is emitted.
    let query_shape = v.get("query").and_then(Value::as_str).map(|q| {
        let len = v["params"]["query_len"]
            .as_i64()
            .unwrap_or_else(|| q.chars().count() as i64);
        QueryShape {
            term_count: count_bucket(q.split_whitespace().count() as i64),
            length_bucket: length_bucket(len),
            had_fts5_operators: had_fts5_operators(q),
            and_matched_zero: fts5 == 0,
        }
    });

    let params = ExportParams {
        filter_set: v["params"]["filter_set"].as_bool(),
        repo_set: v["params"]["repo_set"].as_bool(),
        days: v["params"]["days"].as_i64(),
        limit_requested: v["params"]["limit_requested"].as_i64(),
    };

    // Allowlist-on-read (NOT denylist-by-assumption): the verbatim handler
    // buckets are a closed set (classify_handler_error). Anything else — a
    // future dynamic `kind` that embeds a column/path/query term, or a tampered
    // row — collapses to "unknown" so it can't echo IP through error.kind.
    let error = v.get("error").and_then(|e| {
        e.get("kind").and_then(Value::as_str).map(|k| {
            let kind = match k {
                "fts5_syntax" | "sqlite" | "internal" => k,
                _ => "unknown",
            };
            ExportError {
                kind: kind.to_string(),
            }
        })
    });

    // Allowlist-on-read for the verbatim literal-typed fields too: at the source
    // these are nibdex's own fixed values, but the export must not trust the file
    // (schema drift / tampering). `tool` must be identifier-ish; an unsafe value
    // collapses to "unknown".
    let tool = {
        let t = v["tool"].as_str().unwrap_or("unknown");
        if is_safe_map_key(t) {
            t.to_string()
        } else {
            "unknown".to_string()
        }
    };

    ExportRow {
        schema_version: v["schema_version"].as_u64().unwrap_or(0),
        tool,
        query_shape,
        params,
        wall_ms: v["wall_ms"].as_u64().unwrap_or(0),
        stages_ms: StagesMs {
            fts5_query: v["stages_ms"]["fts5_query"].as_u64().unwrap_or(0),
            rank: v["stages_ms"]["rank"].as_u64().unwrap_or(0),
            join: v["stages_ms"]["join"].as_u64().unwrap_or(0),
            shape_response: v["stages_ms"]["shape_response"].as_u64().unwrap_or(0),
        },
        candidate_count: CandidateCount {
            fts5,
            after_rank: v["candidate_count"]["after_rank"].as_i64().unwrap_or(0),
        },
        result_token_estimate: v["result_token_estimate"].as_u64().unwrap_or(0),
        cache_hit: v.get("cache_hit").and_then(Value::as_bool),
        daemon_uptime_s: v["daemon_uptime_s"].as_u64().unwrap_or(0),
        // Clamp to the known honesty tags; unknown/tampered → the most
        // conservative ("estimated"), never a free-text passthrough.
        calibration_confidence: match v["calibration_confidence"].as_str() {
            Some("measured") => "measured",
            Some("derived") => "derived",
            _ => "estimated",
        }
        .to_string(),
        counterfactual_tokens_p50: v.get("counterfactual_tokens_p50").and_then(Value::as_u64),
        counterfactual_tokens_p95: v.get("counterfactual_tokens_p95").and_then(Value::as_u64),
        tokens_saved_p50: v.get("tokens_saved_p50").and_then(Value::as_i64),
        dollars_saved_p50_usd: v.get("dollars_saved_p50_usd").and_then(Value::as_f64),
        counterfactual_wall_ms_p50: v.get("counterfactual_wall_ms_p50").and_then(Value::as_u64),
        calibration_model_version: v
            .get("calibration_model_version")
            .and_then(Value::as_str)
            .map(str::to_string),
        query_broadened: v.get("query_broadened").and_then(Value::as_bool),
        returned_full_tokens: v.get("returned_full_tokens").and_then(Value::as_u64),
        // Closed set; unknown/tampered values dropped (not echoed verbatim).
        outcome: v.get("outcome").and_then(Value::as_str).and_then(|o| match o {
            "error" | "degraded" => Some(o.to_string()),
            _ => None,
        }),
        error,
    }
}

/// Scrub the typed `CheckResult` snapshot into an `ExportSnapshot` (§4.2).
/// Returns the snapshot plus the count of verbatim-map keys dropped by the
/// §2 key allowlist (surfaced in the summary — no silent truncation).
fn scrub_snapshot(c: &CheckResult) -> (ExportSnapshot, usize) {
    // §2 default-deny on verbatim map keys (see `is_safe_map_key`).
    let (safe_docs, d_docs) = filter_safe_keys(&c.indexer.documents);
    let (perf_p50_ms, d_p50) = filter_safe_keys(&c.perf_p50_ms);
    let (perf_p95_ms, d_p95) = filter_safe_keys(&c.perf_p95_ms);
    let (extractors_last_run_ms, d_ext) = filter_safe_keys(&c.extractors_last_run_ms);
    let dropped_unsafe_keys = d_docs + d_p50 + d_p95 + d_ext;

    let documents = safe_docs
        .iter()
        .map(|(k, n)| (k.clone(), count_bucket(*n)))
        .collect();

    let file_watcher = c.file_watcher.as_ref().map(|fw| ExportFileWatcher {
        events_total: fw.events_total,
        events_coalesced_total: fw.events_coalesced_total,
        // last_event_ts DROPped; subscriptions → count only (§5.3).
        subscription_count: fw.subscriptions.len(),
    });

    let cost_savings = c.cost_savings.as_ref().map(|cs| {
        let window = |w: &crate::cost_ledger::CostSavingsWindow| ExportWindow {
            queries_served: w.queries_served,
            tokens_returned: w.tokens_returned,
            counterfactual_tokens_p50: w.counterfactual_tokens_p50,
            tokens_saved_p50: w.tokens_saved_p50,
            dollars_saved_p50_usd: w.dollars_saved_p50_usd,
            per_tool: w
                .per_tool
                .iter()
                .map(|(t, pt)| {
                    (
                        t.clone(),
                        ExportPerTool {
                            queries: pt.queries,
                            tokens_saved_p50: pt.tokens_saved_p50,
                            dollars_saved_p50_usd: pt.dollars_saved_p50_usd,
                        },
                    )
                })
                .collect(),
        };
        ExportCostSavings {
            calibration_model_version: cs.calibration_model_version.clone(),
            window_1d: window(&cs.window_1d),
            window_7d: window(&cs.window_7d),
            window_30d: window(&cs.window_30d),
        }
    });

    let snapshot = ExportSnapshot {
        schema_version: c.schema_version,
        daemon_uptime_s: c.daemon_uptime_s,
        indexer: ExportIndexer {
            documents,
            session_entries: count_bucket(c.indexer.session_entries),
            memory_entries: count_bucket(c.indexer.memory_entries),
            design_doc_sections: count_bucket(c.indexer.design_doc_sections),
            source_chunks: count_bucket(c.indexer.source_chunks),
            commit_entries: count_bucket(c.indexer.commit_entries),
            indexed_repos: count_bucket(c.indexer.indexed_repos),
            search_index_total: count_bucket(c.indexer.search_index_total),
        },
        orphans: ExportOrphans {
            session_entries: count_bucket(c.orphans.session_entries),
            memory_entries: count_bucket(c.orphans.memory_entries),
            design_doc_sections: count_bucket(c.orphans.design_doc_sections),
            indexed_repos: count_bucket(c.orphans.indexed_repos),
        },
        shallow_repo_count: c.shallow_repos.len(),
        perf_p50_ms,
        perf_p95_ms,
        file_watcher,
        extractors_last_run_ms,
        cost_savings,
    };
    (snapshot, dropped_unsafe_keys)
}

/// The self-describing `legend` (§7.1): every emitted key → {what, units,
/// confidence}, so neither the user nor we can misread a number.
fn build_legend() -> Value {
    let measured = |what: &str, units: &str| json!({"what": what, "units": units, "confidence": "measured"});
    let estimated = |what: &str, units: &str| json!({"what": what, "units": units, "confidence": "estimated"});
    let derived = |what: &str, units: &str| json!({"what": what, "units": units, "confidence": "derived"});
    json!({
        "count_bucket_ladder": BUCKET_LADDER,
        "snapshot.daemon_uptime_s": measured("daemon uptime; 0 for a CLI-recomputed snapshot (per-call rows carry real uptime)", "s"),
        "snapshot.indexer.*": derived("corpus counts, mapped to the count_bucket_ladder", "bucket"),
        "snapshot.orphans.*": derived("index-health orphan counts, bucketed", "bucket"),
        "snapshot.shallow_repo_count": measured("number of shallow repos (paths dropped, §5.3)", "count"),
        "snapshot.file_watcher.subscription_count": measured("number of watched paths (paths dropped, §5.3)", "count"),
        "calls[].query_shape.term_count": derived("whitespace-split token count of the (dropped) query, mapped to count_bucket_ladder", "bucket"),
        "calls[].query_shape.length_bucket": derived("query length, mapped to count_bucket_ladder", "bucket"),
        "calls[].query_shape.had_fts5_operators": derived("any FTS5 operator present in the (dropped) query", "bool"),
        "calls[].query_shape.and_matched_zero": derived("the strict AND query matched nothing (candidate_count.fts5 == 0)", "bool"),
        "calls[].query_broadened": measured("D-10.13 OR-fallback fired; absent off the FTS5 path", "bool"),
        "calls[].wall_ms": measured("tool handler wall time", "ms"),
        "calls[].stages_ms": measured("per-stage handler timing", "ms"),
        "calls[].candidate_count": measured("FTS5 matched / after-rank counts", "count"),
        "calls[].result_token_estimate": measured("serialized envelope size ÷ 4", "tokens"),
        "calls[].returned_full_tokens": measured("summed untruncated size of the returned hits ÷ 4 — the by-hand read size for the grounded counterfactual; absent off the FTS5 path", "tokens"),
        "calls[].counterfactual_tokens_p50": estimated("modelled tokens a non-nibdex baseline would have read", "tokens"),
        "calls[].tokens_saved_p50": estimated("counterfactual_p50 − result_token_estimate; SIGNED (negative = a loss)", "tokens"),
        "calls[].dollars_saved_p50_usd": estimated("tokens_saved_p50 priced at the model input rate", "usd"),
        "calls[].error.kind": measured("coarse error bucket; the verbatim message is dropped", "enum"),
        "snapshot.cost_savings.*.tokens_saved_p50": estimated("windowed savings; SIGNED, losses included", "tokens"),
    })
}

/// Run the export: recompute the snapshot, read + scrub the JSONL rows within
/// the window, and write the candidate payload to `out`. Pure w.r.t. `now`
/// (passed in, not read from a hidden clock) so it is deterministically
/// testable. Writes a file and returns — it never transmits (§6.4).
pub async fn run_export(
    pool: &SqlitePool,
    calibration: Option<&CalibrationModel>,
    metrics_jsonl: &Path,
    days: Option<u32>,
    now: DateTime<Utc>,
    out: &Path,
) -> Result<ExportSummary> {
    // Snapshot: recompute via run_check (uptime 0 — one-shot CLI).
    let mut stages = Stages::default();
    let check = run_check(pool, 0, calibration, &mut stages)
        .await
        .context("recompute check() snapshot for export")?;
    let (snapshot, dropped_unsafe_keys) = scrub_snapshot(&check);

    // Per-call rows.
    let raw = std::fs::read_to_string(metrics_jsonl)
        .with_context(|| format!("read metrics jsonl: {}", metrics_jsonl.display()))?;
    let cutoff = days.map(|d| now - Duration::days(d as i64));

    let mut rows_total = 0usize;
    let mut rows_unparseable_ts = 0usize;
    let mut error_rows = 0usize;
    let mut loss_rows = 0usize;
    let mut calls: Vec<ExportRow> = Vec::new();

    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: Value = serde_json::from_str(line)
            .with_context(|| format!("parse metrics jsonl line: {line}"))?;
        rows_total += 1;

        // Window filter (§6.1 all-or-nothing within the window). We read `ts`
        // to filter, then drop it from the payload. Unparseable ts is kept and
        // counted — never silently dropped.
        if let Some(cutoff) = cutoff {
            match v.get("ts").and_then(Value::as_str) {
                Some(ts) => match DateTime::parse_from_rfc3339(ts) {
                    Ok(dt) => {
                        if dt.with_timezone(&Utc) < cutoff {
                            continue; // outside the window
                        }
                    }
                    Err(_) => rows_unparseable_ts += 1, // kept, but flagged
                },
                None => rows_unparseable_ts += 1, // kept, but flagged
            }
        }

        let row = scrub_row(&v);
        if row.outcome.as_deref() == Some("error") || row.error.is_some() {
            error_rows += 1;
        }
        if row.tokens_saved_p50.is_some_and(|t| t < 0) {
            loss_rows += 1;
        }
        calls.push(row);
    }

    let rows_in_window = calls.len();
    let bundle = Bundle {
        nibdex_metrics_export: BundleMeta {
            format_version: FORMAT_VERSION,
            scrub_spec: SCRUB_SPEC.to_string(),
            window_days: days,
            rows_in_window,
        },
        legend: build_legend(),
        snapshot,
        calls,
    };

    let payload = serde_json::to_string_pretty(&bundle).context("serialize export bundle")?;
    std::fs::write(out, payload.as_bytes())
        .with_context(|| format!("write export payload: {}", out.display()))?;

    Ok(ExportSummary {
        out_path: out.to_path_buf(),
        rows_total,
        rows_in_window,
        rows_unparseable_ts,
        error_rows,
        loss_rows,
        window_days: days,
        dropped_unsafe_keys,
    })
}

#[cfg(test)]
mod tests;
