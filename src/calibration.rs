// SPDX-License-Identifier: MIT

//! §8.4 Layer 2 — per-tool counterfactual model loaded from `calibration.toml`.
//!
//! Day 8 commit 1 lands the config layer with zero behavioral change visible
//! from the MCP surface: the model is loaded, plumbed through `serve_stdio`
//! and `http_server::serve` into `NibdexServer`, and held as
//! `Option<Arc<CalibrationModel>>` — but commit-1 handlers do not yet read
//! it. Commit 2 wires emission (6 additive optional fields on `MetricsEvent`
//! + `cost_ledger_events` row insert + `check()` rollup).
//!
//! Loading semantics (D-8.1):
//!
//! - Missing file → `tracing::warn!` + return `None`. Daemon runs Layer-1
//!   only. Not a startup error: users may legitimately run nibdex without
//!   the cost-savings ledger (e.g. against differently-shaped corpora that
//!   the seed numbers don't represent honestly).
//! - Bad TOML / unparseable structure → startup `panic!()` with file path
//!   and `toml::de::Error`. Daemon refuses to start. Mirrors D-7.6
//!   misconfig invariant: surface misconfig before first tool call, not
//!   on a per-call code path that swallows errors.
//!
//! Schema policy: this struct + `ToolCounterfactual` is the public contract
//! consumers (EXAMPLES.md, the eval harness) hard-code against. Adding a
//! new tool entry is free (forward-compat). Adding a new field to
//! `[tools.<name>]` is additive — old TOML files parse cleanly because
//! `serde` defaults absent fields. Renaming or dropping fields is a v0.2
//! model bump; old `cost_ledger_events` rows survive via their per-row
//! `calibration_model_version` stamp.

use std::path::Path;

use serde::Deserialize;

/// Top-level shape of `calibration.toml`. The `[model]` block carries
/// version + pricing metadata; `[tools.<name>]` blocks carry per-tool
/// counterfactual estimates.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct CalibrationModel {
    pub model: ModelMetadata,
    /// Per-tool entries keyed by tool name (without the `tool.` prefix).
    /// `BTreeMap` so iteration order is stable for tests + diagnostics.
    pub tools: std::collections::BTreeMap<String, ToolCounterfactual>,
}

/// `[model]` block. `version` is stamped per-row into `cost_ledger_events`
/// so historical aggregates survive recalibration without retroactive
/// change.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ModelMetadata {
    pub version: String,
    pub generated_from: String,
    /// Anthropic input-token pricing for the calibration's reference model.
    /// USD per million input tokens. Seed = 3.0 (Sonnet 4.6, the Claude
    /// Code default as of 2026-05-27). Users on Opus 4.7 / Haiku 4.5 see
    /// proportionally scaled dollar figures.
    pub input_rate_usd_per_mtok: f64,

    // ---- v0.2 savings-model knobs (D-8.11) ---------------------------------
    // The flat per-tool counterfactual is only an *average-path* anchor. On
    // its own (`saved = anchor − result`) it mis-scores two ways the first
    // real work-seat export surfaced: (a) a query that returned *nothing*
    // banked the full anchor as "savings" ("found nothing, saved 60k"); (b) a
    // legitimately large result (a big design-doc section) scored a nonsensical
    // loss because a retrieval can't honestly cost *more* than the by-hand
    // baseline to obtain the same content. These three knobs correct both,
    // and — living in calibration.toml — let an operator (or a future
    // self-tuning loop) re-tune from their own exports without a recompile.
    // All `serde(default)` so a v0.1 file predating them parses cleanly.
    /// Fixed tokens a non-nibdex baseline spends *locating* an answer (grep +
    /// open candidate files) beyond reading it. Floors a successful retrieval
    /// to at least this much saving. Default 0 (no floor credit).
    #[serde(default)]
    pub search_overhead_tokens: u64,
    /// Multiplier (≥ 1.0) on the returned token estimate in the size-floor:
    /// the by-hand baseline reads at least `result × factor` (a section comes
    /// from a larger doc you read around to find). 1.0 = baseline reads
    /// exactly what nibdex returned. Default 1.0.
    #[serde(default = "default_recovery_factor")]
    pub result_recovery_factor: f64,
    /// Savings credited to a call that returned nothing (`after_rank == 0`).
    /// Default 0 — finding nothing saved nothing. Negative models a wasted
    /// call as a small honest cost.
    #[serde(default)]
    pub zero_match_savings_tokens: i64,

    // ---- Phase-2 time-saved model knobs (record-then-rescore; INERT today) ----
    // The live emit path computes only TOKEN savings. `nibdex rescore` (offline)
    // will use these two knobs + the per-call `returned_full_tokens` to derive a
    // TIME saving — the value of nibdex *locating* data vs the AI searching for
    // it at need. Two components: (a) read/generate time avoided ≈ grounded
    // counterfactual tokens × `ai_read_ms_per_ktok`; (b) round-trip latency
    // avoided ≈ returned-hit count × `roundtrip_ms_per_call` (the by-hand path
    // is grep + one open per hit vs nibdex's single sub-100ms call). Both
    // `serde(default)` so a file predating them parses; tune from your exports.
    // Default 0.0/0.0 → zero time credit until the grounded model is turned on.
    /// Milliseconds the by-hand baseline (AI reading located context) spends per
    /// 1k tokens of counterfactual read. Seed ≈ reproduces the v0.2 per-tool
    /// wall anchors. Default 0.0.
    #[serde(default)]
    pub ai_read_ms_per_ktok: f64,
    /// Milliseconds credited per eliminated tool-call round-trip (a grep /
    /// open-file cycle the by-hand search would have spent). Default 0.0.
    #[serde(default)]
    pub roundtrip_ms_per_call: f64,
}

/// `result_recovery_factor` default: a by-hand baseline reads at least as much
/// as nibdex returned (no inflation). Overridable per calibration.toml.
fn default_recovery_factor() -> f64 {
    1.0
}

/// Per-tool counterfactual estimates. p50/p95 token counts + wall-time
/// p50 ms anchor the savings calculation when the tool fires; `check()`
/// is seeded with explicit zeroes to prevent fake-savings inflation on
/// the admin inspection path (D-8.2).
#[derive(Debug, Clone, Deserialize, PartialEq, Default)]
pub struct ToolCounterfactual {
    pub counterfactual_tokens_p50: u64,
    pub counterfactual_tokens_p95: u64,
    pub counterfactual_wall_ms_p50: u64,
}

/// The seven `tool.*` query handlers nibdex exposes via MCP today. Used by
/// `load_or_warn` to surface "expected tool entry missing from
/// calibration.toml" warnings without making them a startup error
/// (forward-compat: future tools shouldn't bork an old calibration.toml).
pub const KNOWN_TOOL_NAMES: &[&str] = &[
    "recent_sessions",
    "find_session",
    "find_memory",
    "find_design_doc",
    "find_code",
    "recent_commits",
    "find_commit",
    "check",
];

impl CalibrationModel {
    /// Resolve a `calibration.toml` path into an optional model.
    ///
    /// - `Ok(Some(_))` — file present + parsed cleanly. The daemon runs
    ///   Layer 1 + Layer 2.
    /// - `Ok(None)` — file absent. Warns to stderr and returns `None`;
    ///   daemon runs Layer 1 only (D-8.1 graceful degradation).
    /// - **panic** — file present but unparseable. D-8.1 + D-7.6
    ///   misconfig invariant: bad config refuses startup, not silent
    ///   degradation, because the user explicitly opted in by writing
    ///   the file.
    ///
    /// The panic-on-bad-TOML branch is deliberate: this is configuration,
    /// not data. A daemon that ignores an unparseable user-authored file
    /// and silently falls back to "no savings" hides a misconfiguration
    /// the operator wants to know about immediately.
    pub fn load_or_warn(path: &Path) -> Option<Self> {
        let raw = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(_) => {
                eprintln!(
                    "[calibration] no model at {}; running Layer-1 only",
                    path.display()
                );
                return None;
            }
        };
        let model: Self = match toml::from_str(&raw) {
            Ok(m) => m,
            Err(e) => {
                panic!(
                    "calibration.toml at {} parse failure: {}",
                    path.display(),
                    e
                );
            }
        };
        eprintln!(
            "[calibration] loaded {} from {} ({} tools)",
            model.model.version,
            path.display(),
            model.tools.len()
        );
        for name in KNOWN_TOOL_NAMES {
            if !model.tools.contains_key(*name) {
                eprintln!(
                    "[calibration] note: tool '{}' missing from {} — treating as zero counterfactual",
                    name,
                    path.display()
                );
            }
        }
        Some(model)
    }

    /// Look up the per-tool counterfactual entry. Returns `None` when the
    /// tool name isn't in the loaded model — callers (commit 2) treat
    /// `None` as zero counterfactual rather than erroring (forward-compat
    /// with future tool additions).
    #[allow(dead_code)] // Consumed by commit 2's emission path.
    pub fn tool(&self, name: &str) -> Option<&ToolCounterfactual> {
        self.tools.get(name)
    }
}

// =====================================================================================
// Tests — Day 8 commit 1: 5 unit tests per D-8.10.
// =====================================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Path to the seed calibration.toml at repo root. Tests run from the
    /// crate root, so the relative path resolves to the in-tree seed.
    fn seed_path() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("calibration.toml")
    }

    /// G — `calibration.toml` seed at repo root parses cleanly and round-
    /// trips through `serde`. Covers D-8.10 commit-1 gate "TOML round-trip
    /// on seed file".
    #[test]
    fn seed_calibration_toml_round_trip() {
        let model = CalibrationModel::load_or_warn(&seed_path())
            .expect("seed calibration.toml should parse cleanly");
        assert_eq!(model.model.version, "v0.2-2026-06-05");
        assert!(
            (model.model.input_rate_usd_per_mtok - 3.0).abs() < f64::EPSILON,
            "seed rate should be 3.0 USD/Mtok (Sonnet 4.6)"
        );
        // v0.2 savings knobs present in the seed (D-8.11).
        assert_eq!(model.model.search_overhead_tokens, 2_000);
        assert!((model.model.result_recovery_factor - 1.0).abs() < f64::EPSILON);
        assert_eq!(model.model.zero_match_savings_tokens, 0);
    }

    /// G — a v0.1-era calibration.toml (no `[model]` savings knobs) still parses
    /// cleanly via `serde(default)`, getting the honest defaults (D-8.11
    /// back-compat: existing operator configs don't break on upgrade).
    #[test]
    fn pre_v02_toml_without_knobs_uses_defaults() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("calibration.toml");
        std::fs::write(
            &path,
            "[model]\n\
             version = \"v0.1-legacy\"\n\
             generated_from = \"test\"\n\
             input_rate_usd_per_mtok = 3.0\n\
             [tools.find_session]\n\
             counterfactual_tokens_p50 = 60000\n\
             counterfactual_tokens_p95 = 85000\n\
             counterfactual_wall_ms_p50 = 8500\n",
        )
        .expect("write legacy toml");
        let model = CalibrationModel::load_or_warn(&path).expect("legacy toml parses");
        assert_eq!(model.model.search_overhead_tokens, 0);
        assert!((model.model.result_recovery_factor - 1.0).abs() < f64::EPSILON);
        assert_eq!(model.model.zero_match_savings_tokens, 0);
    }

    /// G — Missing file returns `None` without panicking. Covers D-8.10
    /// commit-1 gate "missing-file warn returns None" — graceful degrade
    /// per D-8.1.
    #[test]
    fn missing_file_warns_and_returns_none() {
        let nonexistent = std::path::PathBuf::from("/this/path/should/not/exist/calibration.toml");
        let result = CalibrationModel::load_or_warn(&nonexistent);
        assert!(
            result.is_none(),
            "missing calibration.toml should return None, not panic"
        );
    }

    /// G — Bad TOML panics at load time with the file path in the message.
    /// Covers D-8.10 commit-1 gate "bad-TOML startup panic" + D-8.1
    /// misconfig invariant.
    #[test]
    #[should_panic(expected = "calibration.toml")]
    fn bad_toml_panics_on_load() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let bad = tmp.path().join("calibration.toml");
        std::fs::write(&bad, "this is not valid toml ][[[ key = ").expect("write bad toml");
        // load_or_warn should panic — caught by #[should_panic].
        let _ = CalibrationModel::load_or_warn(&bad);
    }

    /// G — All 7 `KNOWN_TOOL_NAMES` are present in the seed file. Covers
    /// D-8.10 commit-1 gate "all 7 tools present in seed".
    #[test]
    fn seed_contains_all_known_tools() {
        let model = CalibrationModel::load_or_warn(&seed_path())
            .expect("seed calibration.toml should parse cleanly");
        for name in KNOWN_TOOL_NAMES {
            assert!(
                model.tools.contains_key(*name),
                "seed calibration.toml missing tool entry: {name}"
            );
        }
        // check() gets explicit zeroes per D-8.2 — verified at the schema
        // level so commit 2's G8.6 smoke gate has a unit-test backstop.
        let check_entry = model
            .tool("check")
            .expect("check() entry must exist in seed");
        assert_eq!(check_entry.counterfactual_tokens_p50, 0);
        assert_eq!(check_entry.counterfactual_tokens_p95, 0);
        assert_eq!(check_entry.counterfactual_wall_ms_p50, 0);
    }

    /// G — `tool()` returns `Some` for known names + `None` for unknown.
    /// Covers D-8.10 commit-1 gate "per-tool lookup helper" — used by
    /// commit-2 emission path as the "is Layer 2 active for this tool?"
    /// gate (None => zero counterfactual => no Layer 2 fields emitted).
    #[test]
    fn per_tool_lookup_helper() {
        let model = CalibrationModel::load_or_warn(&seed_path())
            .expect("seed calibration.toml should parse cleanly");
        let find_session = model
            .tool("find_session")
            .expect("find_session entry must exist in seed");
        assert!(
            find_session.counterfactual_tokens_p50 > 0,
            "find_session p50 should be non-zero (counterfactual reads CLAUDE.md)"
        );
        assert!(
            find_session.counterfactual_tokens_p95 >= find_session.counterfactual_tokens_p50,
            "p95 must be >= p50"
        );
        assert!(
            model.tool("does_not_exist").is_none(),
            "unknown tool should return None"
        );
    }
}
