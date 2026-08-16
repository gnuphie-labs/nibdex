# Metrics-Export Scrub Spec (the contract)

> **Status:** IMPLEMENTED + adversarially pre-flighted (2026-06-04). The v1
> command `nibdex metrics-export` (`src/metrics_export.rs`) realizes this contract
> as a pure function of it. The §7.3 multi-agent adversarial IP-erasure pre-flight
> ran 2026-06-04 — **see §10 attestation.** Three hardening fixes landed
> (error.kind allowlist-on-read, map-key boundary allowlist, term_count bucketing)
> plus tamper clamps on the verbatim literal fields; every ALLOW/TRANSFORM field
> now has an adversarial no-leak test. **One residual before a real
> proprietary-corpus payload:** review `calibration_model_version` at the §7.2
> approval gate (the single free-text, maintainer-config-sourced field).
> **Charter** lives in `docs/DESIGN.md` §5.6 + README "Privacy & metrics" (public).
> This spec is the engineering realization of that charter.
>
> **Publication:** the `nibdex metrics-export` command has shipped, so this
> contract is public — it is the spec you can audit to see exactly what a
> voluntary metrics payload does and does not contain.

---

## 1. Why a contract before code

The goal: the maintainer runs nibdex as a deployed OSS tool against a **real,
proprietary multi-repo corpus**, then hands over a metrics
payload that improves nibdex while carrying **zero IP and zero sensitive data of
any kind**. A scrub gap on that corpus leaks proprietary project names, query
terms, and paths — not merely personal-corpus exposure. So the scrub must be
correct *by construction*, not by careful enumeration of what to hide.

This document is the load-bearing contract: the export command is a pure
function of it. Everything the command emits must trace to an `ALLOW` row here.

## 2. Governing principle — ALLOWLIST (default-deny)

**The export is an allowlist, never a denylist.** Only fields explicitly marked
`ALLOW` (verbatim) or `TRANSFORM` (via a named, IP-erasing derivation) appear in
the payload. **Everything else — including any field added to the internal
telemetry structs in the future — is excluded by default.**

This is the single design decision that makes the feature safe. A denylist
("drop the fields we know leak") silently ships the next leaky field someone
adds to `MetricsEvent`. An allowlist cannot: a new field defaults to dropped,
and adding it to the payload requires a deliberate edit to *this* file plus the
adversarial review in §7. **When in doubt, a field is excluded** (DESIGN §5.6 #1).

Implementation requirement: the export code must **not** serialize the internal
structs and post-filter. It must **construct a fresh export struct** field by
field from this contract. A `#[deny]`-style discipline (round-trip test that
fails if an internal struct gains a field not classified here) enforces it — §7.

## 3. The two internal sources

| Source | Where | Shape |
|---|---|---|
| `MetricsEvent` (+ nested `ErrorDetail`) | `src/metrics_sink.rs:162` | one JSONL row per tool call in `metrics.jsonl` |
| `CheckResult` (+ nested counts/ledger) | `src/mcp/types.rs:122`, `src/cost_ledger.rs:36` | the live `check()` snapshot |

Nothing else is a source. The raw `metrics.jsonl` and raw `check()` output are
**never** the payload (germ §"So shared metrics must be a separate, derived,
scrubbed export").

---

## 4. Field-by-field contract

Legend: **ALLOW** = copied verbatim (already IP-free). **TRANSFORM** = passed
through a named derivation in §5. **DROP** = never appears in the payload.

### 4.1 `MetricsEvent` → exported per-call row

| Field | Type | Verdict | Rationale |
|---|---|---|---|
| `schema_version` | u32 | **ALLOW** | export metadata |
| `ts` | String (RFC3339 ms) | **DROP** | per-event wall-clock = a timeline of when the user works; a behavioral fingerprint, no nibdex-improvement value (latency is in `wall_ms`). See §8 open-decision 2 |
| `tool` | String | **ALLOW** | nibdex's own tool name (`find_session`, `check`, …) |
| `query` | Option\<String\> | **TRANSFORM → `query_shape`** (§5.1) | verbatim user query — carries private repo/project names. The one field whose doc-comment's "no privacy surface" rationale **inverts** off-machine |
| `params` | Value | **ALLOW (key-allowlisted)** | already shape-only by construction (see §4.3): `filter_set`/`repo_set`/`days`/`limit_requested` ALLOW; `query_len` **TRANSFORM → length bucket** (§5.1) |
| `wall_ms` | u64 | **ALLOW** | latency — core perf signal |
| `stages_ms` | Value `{fts5_query,rank,join,shape_response}` | **ALLOW** | stage timings — perf tuning |
| `candidate_count` | Value `{fts5,after_rank}` | **ALLOW** | counts; also yields the AND-matched-zero signal (`fts5 == 0`) |
| `result_token_estimate` | u64 | **ALLOW** | value-counterfactual input |
| `returned_full_tokens` | Option\<u64\> | **ALLOW** | summed untruncated size of the returned hits ÷ 4 — the by-hand read size that grounds a per-query counterfactual (vs the flat anchor); raw material for `nibdex rescore`'s grounded-model A/B. `Some` only on an FTS5 path (gated like `query_broadened`); absent off it and on error rows. A token count — IP-free, copied verbatim |
| `cache_hit` | Option\<bool\> | **ALLOW** | None at v1; perf |
| `daemon_uptime_s` | u64 | **ALLOW** | longevity |
| `calibration_confidence` | &str | **ALLOW — MANDATORY** | `"estimated"`; **must ride through unchanged** so token/$ figures never read as measured (§6, DESIGN §11) |
| `counterfactual_tokens_p50` | Option\<u64\> | **ALLOW** | value claim (estimated) |
| `counterfactual_tokens_p95` | Option\<u64\> | **ALLOW** | value claim (estimated) |
| `tokens_saved_p50` | Option\<i64\> | **ALLOW (signed)** | **losses must survive** — a negative value is honest data (§6) |
| `dollars_saved_p50_usd` | Option\<f64\> | **ALLOW** | value claim (estimated) |
| `counterfactual_wall_ms_p50` | Option\<u64\> | **ALLOW** | value claim (estimated) |
| `calibration_model_version` | Option\<String\> | **ALLOW** | nibdex's own model version string |
| `query_broadened` | Option\<bool\> | **ALLOW** | D-10.13 OR-broaden flag. `Some(true/false)` only on an FTS5 path; absent off it (`check`, no-filter `recent_*`) and on error rows. A bool — IP-free. This is the retrieval-quality signal §5.1 was built around; **persisted as of 2026-06-04** (was the named gap below). Copied verbatim — *not* derived from the raw query, so no transform |
| `outcome` | Option\<&str\> | **ALLOW** | `"error"`/absent — outcome distribution |
| `error.kind` | String | **ALLOW (allowlist-on-read)** | coarse bucket (`fts5_syntax`/`sqlite`/`internal`). The export RE-CLAMPS to this closed set on read — any other value (schema drift / tampered row) collapses to `"unknown"`. It does NOT trust the file's verbatim `kind` (§10 finding 1) |
| `error.message` | String | **DROP** | verbatim handler error — echoes query terms (`no such column: dashboard`) |

### 4.2 `CheckResult` → exported snapshot

| Field | Type | Verdict | Rationale |
|---|---|---|---|
| `schema_version` | i64 | **ALLOW** | metadata |
| `daemon_uptime_s` | i64 | **ALLOW** | longevity |
| `indexer.documents` | BTreeMap\<kind,count\> | **keys ALLOW, counts TRANSFORM → bucket** (§5.2) | keys are doc-*type* categories (`session_history`, `git_commit`), not repo names — verified `check.rs:78` |
| `indexer.{session_entries,session_edges,memory_entries,design_doc_sections,source_chunks,commit_entries,indexed_repos,search_index_total}` | i64 | **TRANSFORM → bucket** (§5.2) | exact corpus counts are a weak fingerprint; buckets preserve the "does value scale with corpus size" signal. `source_chunks` (D1a) is the source-corpus analogue of `design_doc_sections`; `session_edges` is the raw-transcript session corpus behind `find_session`/`recent_sessions`. See §8 open-decision 1 |
| `orphans.*` (5 counts: session_entries, memory_entries, design_doc_sections, source_chunks, indexed_repos) | i64 | **TRANSFORM → bucket** (§5.2) | index-health signal, bucketed |
| `shallow_repos` | Vec\<String\> | **TRANSFORM → count only** (§5.3) | values are `repo_path` strings (`check.rs:113`) — **paths/names, UNSAFE**. The *count* of shallow repos is the only useful signal |
| `perf_p50_ms` / `perf_p95_ms` | BTreeMap\<tool,ms\> | **ALLOW** | keys are nibdex tool names |
| `file_watcher.events_total` | i64 | **ALLOW** | watcher activity |
| `file_watcher.events_coalesced_total` | i64 | **ALLOW** | watcher activity |
| `file_watcher.last_event_ts` | Option\<i64\> | **DROP** | wall-clock timestamp (same fingerprint class as `ts`) |
| `file_watcher.subscriptions` | Vec\<String\> | **TRANSFORM → count only** (§5.3) | **absolute watched paths with project names — UNSAFE.** Count is the signal |
| `extractors_last_run_ms` | BTreeMap\<extractor,ms\> | **ALLOW** | keys are nibdex extractor names |
| `cost_savings.calibration_model_version` | String | **ALLOW** | model version |
| `cost_savings.window_{1d,7d,30d}.*` | counts/tokens/$ | **ALLOW** | `queries_served`, `tokens_returned`, `counterfactual_tokens_p50`, `tokens_saved_p50` (signed), `dollars_saved_p50_usd` — all IP-free aggregates |
| `cost_savings.window_*.per_tool` | BTreeMap\<tool,{queries,tokens_saved_p50,$}\> | **ALLOW** | tool-name keys; **the D-10.14 `find_design_doc` net-negative finding lives here and MUST survive** (§6) |
| `build` | `{crate_version,git_sha,git_describe,commit_time}` | **DROP** | Compile-time build provenance (`src/build_info.rs`). IP-free and individually safe, but it's an *interrogation/health* surface (`check()` tool · `/healthz` · `nibdex version`), not a metrics signal — so by the §2 default-deny / "exclude when in doubt" posture it is **not** construct-fresh'd into the payload. Revisit as a deliberate ALLOW only if field reports need build correlation. Accounted for in the §7.3 drift guard so adding it didn't silently widen the export. |
| `retired_corpora` | `[{corpus,rows,superseded_by}]` | **DROP** | Names corpora that are deliberately dead (`session_entries`, superseded by `session_edges`) so a non-zero count is not misread as index damage. Same class as `build`: an *interrogation/health* surface, not a metrics signal, so it is **not** construct-fresh'd into the payload. The table names are fixed internal identifiers and carry no IP, but the §2 default-deny posture governs and there is no field-report question this would answer. Accounted for in the §7.3 drift guard — and the guard's fixture populates it deliberately, because the field is `skip_serializing_if` and an empty sample would pass the guard without ever exercising it. |
| `adoption` | `{sessions_seen,sessions_using_nibdex,retrieval_elsewhere,nibdex_queries,nibdex_share_pct}` | **DROP** (revisit deliberately) | The denominator: how much retrieval happened versus how much nibdex served. Counts only — no queries, no paths, no identities — so unlike most DROP rows this one is **not** excluded for IP reasons. It is excluded because widening the attested payload is a decision to take on purpose. Worth reconsidering as a deliberate ALLOW: it is the single most useful thing a field install could report back, since every other metric here fires only when nibdex is CALLED and is therefore blind to nibdex being ignored. Accounted for in the §7.3 drift guard with a populated fixture. |

### 4.3 Why `params` is already safe (verified, not assumed)

Every tool handler in `src/mcp/server.rs` reduces its raw arguments to derived
booleans/lengths **before** they reach `MetricsEvent.params`:

- `recent_sessions` / `recent_commits` → `{filter_set: req.filter.is_some(), repo_set: req.repo.is_some(), days, limit_requested}` (`server.rs:92`, `:226`)
- `find_*` → `{query_len: req.query.len(), limit_requested}` (`server.rs:158`)

The verbatim `filter` / `repo` / `query` strings are passed to `emit_metrics` as
the **separate `query` argument** (which §4.1 transforms), never folded into
`params`. So `params` carries no raw text by construction. The contract still
allowlists it key-by-key (not "ALLOW the whole blob") so a future param that
embeds a raw string can't ride through silently.

---

## 5. Transform derivations (the IP-erasing functions)

Each transform is a **pure, lossy, on-machine** function. The raw input never
leaves; only the derived value is emitted.

### 5.1 `query` (+ `query_len`) → `query_shape`

Replace the verbatim string with an object of shape features:

```jsonc
"query_shape": {
  "term_count": "4-7",        // whitespace-split token count, BUCKETED (§5.2); never exact (§10 finding 3)
  "length_bucket": "16-31",   // see bucket ladder §5.2; never the exact length
  "had_fts5_operators": true, // any of: OR NOT NEAR " * + - : ( )
  "and_matched_zero": false   // derived from candidate_count.fts5 == 0
}
```

- `term_count` and `had_fts5_operators` are computed locally from the raw query;
  only the count/flag is emitted. No token, substring, or n-gram of the query
  text is ever emitted — **no exceptions** (this is the line that protects
  private repo/project names).
- `length_bucket` replaces the exact `query_len` from `params`.
- **`query_broadened` (D-10.13) — gap CLOSED 2026-06-04.** Did the OR-fallback
  fire? This was previously *not* a `MetricsEvent` field — the single most
  valuable retrieval-quality signal we lacked. It is now persisted as
  `MetricsEvent.query_broadened: Option<bool>` (`Some` only on an FTS5 path;
  absent off it and on error rows), so the export carries it **verbatim as a
  top-level `ALLOW` field** (§4.1) rather than the placeholder
  `query_broadened: "not_recorded"`. It is *not* folded into `query_shape`: that
  object holds features derived locally from the raw (dropped) query string,
  whereas `query_broadened` is an already-IP-free bool that needs no derivation.
  Done as the cheapest-first telemetry step *before* the export build so the
  contract is complete (no placeholder to write then rip out, no drift-test
  churn when the field lands).

### 5.2 Count → bucket (log-scale ladder)

All corpus/orphan counts and `query_len` map to a fixed, documented ladder so
the payload reveals scale-of-magnitude, not an exact fingerprint:

```
0 · 1 · 2-3 · 4-7 · 8-15 · 16-31 · 32-63 · 64-127 · 128-255 · 256-511 ·
512-1023 · 1024-4095 · 4096-16383 · 16384-65535 · 65536+
```

(Powers-of-two edges; the ladder string itself ships in the payload legend so a
reader can see the granularity.) See §8 open-decision 1 — exact counts are more
actionable for "does nibdex's value scale with corpus size," at a small
fingerprint cost.

### 5.3 Path list → count only

`shallow_repos` and `file_watcher.subscriptions` emit **only their length**
(`shallow_repo_count`, `subscription_count`). Repo paths/names carry no
nibdex-improvement value and are pure IP. (If a future analysis genuinely needs
per-repo correlation, the fallback is anonymized ordinals `repo_0..repo_n` with
a stable within-payload mapping — explicitly **not** in v1; §8 open-decision 4.)

---

## 6. Honesty guardrails (constraint #3 — losses survive)

A payload that hides losses fails the honesty constraint as badly as one that
leaks IP fails the privacy constraint. Enforced rules:

1. **All-or-nothing window.** No cherry-picking which rows export. The command
   exports every row in the chosen window (e.g. `--days 30`), or none.
2. **Losses are mandatory fields, not opt-outs.** `tokens_saved_p50` stays
   **signed**; negative values export. Error rows (`outcome: "error"`,
   `error.kind`) export. The `per_tool` net-negative finding
   (D-10.14 `find_design_doc`) exports. Removing any of these from a payload is
   a contract violation, not a formatting choice.
3. **`calibration_confidence` rides through unchanged.** Every token/$ figure in
   the payload is accompanied by `"estimated"` so no number can be misread as
   measured truth (DESIGN §11). The self-describe legend (§7) restates this.
4. **Zero network egress.** The command writes a **file**. nibdex never
   transmits it. "Sharing" is the user handing over that file. The loopback-only
   posture is unchanged.

---

## 7. Payload format + enforcement

### 7.1 Self-describing (constraint #5)

The payload is human-readable JSON with a top-level `legend` block: every metric
key maps to `{what, units, confidence}`. A reader (the maintainer inspecting
before approval; us receiving it) can understand every number without the source.

```jsonc
{
  "nibdex_metrics_export": { "format_version": 1, "scrub_spec": "METRICS_EXPORT_SPEC.md@<git-sha>" },
  "legend": {
    "wall_ms": { "what": "tool handler wall time", "units": "ms", "confidence": "measured" },
    "tokens_saved_p50": { "what": "counterfactual_p50 − result_token_estimate; signed", "units": "tokens", "confidence": "estimated" },
    "count_bucket_ladder": "0 · 1 · 2-3 · 4-7 · … · 65536+"
    // … one entry per emitted field
  },
  "snapshot": { /* scrubbed CheckResult per §4.2 */ },
  "calls": [ /* scrubbed MetricsEvent rows per §4.1, ts dropped */ ]
}
```

### 7.2 Approval flow (constraint #2 + #4)

`nibdex metrics-export` **generates a candidate file → the user inspects the
full readable file → the user explicitly approves → only then is it a shareable
bundle.** No silent path from internal metrics to an outbound artifact. The
approval gate is load-bearing: it is the human backstop that catches any residual
scrub gap on the high-stakes first proprietary-corpus export.

### 7.3 Enforcement (so the allowlist can't rot)

- **Construct, don't filter.** The export builds a fresh `ExportRow` /
  `ExportSnapshot` struct field-by-field. It must not `serde_json::to_value` an
  internal struct and post-strip.
- **Drift test.** A unit test asserts the set of `MetricsEvent` / `CheckResult`
  field names equals the set classified in this doc. Adding a field to either
  internal struct **fails the build** until it is classified here. This is the
  mechanical guarantee behind §2's default-deny claim.
- **Adversarial pre-flight (the high-effort step).** Before the *first*
  proprietary-corpus payload leaves the machine: for each `ALLOW`/`TRANSFORM` field,
  construct an input where it could carry IP and confirm the scrub erases it.
  This is the moment to spend max effort / a multi-agent adversarial review —
  not the writing of this contract.

---

## 8. Decisions (RATIFIED 2026-06-04)

All five resolved per the recommendations below (tracker checkpoint 2026-06-04).
Re-open only with a new tracker entry.

1. **Count granularity — buckets vs exact.** Spec defaults to log-scale buckets
   (§5.2). Exact corpus counts are more actionable for the "value scales with
   corpus size" question, at a small fingerprint cost. Bucket = safer; exact =
   richer. **Recommendation: buckets for v1**, revisit if the scaling question
   needs exact N.
2. **Per-event `ts` — drop vs coarsen-to-date.** Spec defaults to DROP (§4.1).
   Coarsening to date-only would let us see weekday/session-clustering patterns
   but reintroduces a behavioral timeline. **Recommendation: drop.**
3. **Per-event rows vs aggregate-only.** Spec keeps scrubbed per-event rows
   (minus `ts`) because the retrieval-quality signal (`query_shape`,
   `and_matched_zero`, per-call latency) is per-event. Aggregate-only is the
   strictest privacy posture but loses that signal. **Recommendation: per-event
   rows, ts dropped** — this is the biggest call; flagging explicitly.
4. **Path lists — count-only vs anonymized ordinals.** Spec defaults to
   count-only (§5.3). Ordinals add per-repo correlation if ever needed.
   **Recommendation: count-only for v1.**
5. **Publish this doc at flip?** Strong trust artifact, but pre-feature and names
   internal fields. **Recommendation: internal until the command ships, then
   public.**

---

## 9. What this delivers (the actionable half)

From an airtight, IP-free payload, nibdex improvement gets: per-tool latency
distributions (`wall_ms`/`stages_ms`), retrieval-quality signal (term-count
distribution, AND-matched-zero rate, and — once telemetry adds it —
broadened rate), outcome/error-kind distribution, value counterfactuals with
honest `estimated` tagging (losses included), per-tool call frequency (does each
tool earn its keep), and corpus-scale buckets (does value scale with size). That
is real, actionable data — carrying zero IP — which is exactly the
large-real-corpus signal the thin personal (Air) corpus cannot give.

---

## 10. Adversarial pre-flight attestation (2026-06-04)

The §7.3 pre-flight ran as a four-cluster multi-agent adversarial review: each
cluster was tasked to **break** the scrub (leak query text, paths, error
messages, codenames, or a re-identifying fingerprint) rather than to confirm it.
Clusters: (A) query-derived per-call fields; (B) snapshot path/count/timestamp
fields; (C) a **code-provenance audit** of the verbatim map keys; (D) transform
internals, the error path, and the `ts`-window logic.

### Findings + dispositions

1. **`error.kind` was verbatim-from-file (LEAK).** The export trusted the file's
   `error.kind` instead of re-asserting the closed handler-bucket set — a
   denylist-by-assumption, the precise §2 anti-pattern. A schema-drift kind that
   embedded a column/path/query term, or a tampered row, would ride out.
   **Fixed:** allowlist-on-read clamp to `{fts5_syntax, sqlite, internal}`, else
   `"unknown"`. Test: `scrub_row_clamps_unknown_error_kind_to_unknown`.

2. **Verbatim map keys were unguarded at the export boundary (NEEDS-GUARD).**
   Cluster C *proved by exhaustive provenance audit* that `documents.kind`
   (3 literals: `session_history`/`memory`/`design_doc` — note: git commits are
   NOT `documents`, so the earlier `git_commit` example was inaccurate) and the
   `perf_*`/`extractors_last_run_ms` keys (`tool.*`/`extract.*` `op_name`
   literals) are nibdex-controlled closed sets **today**. But `scrub_snapshot`
   cloned whatever keys existed, and the §7.3 drift test guards struct *field
   names*, not map *key domains* — so a future dynamic key (or tampered db) would
   leak verbatim and pass drift green. **Fixed:** `is_safe_map_key` charset
   allowlist (`[a-z0-9_.]`, bounded) enforced at the boundary; unsafe keys
   dropped and **counted** in the summary (no silent truncation; a >0 count warns
   the operator). Tests: `safe_map_key_*`, `scrub_snapshot_drops_unsafe_map_keys`.

3. **`term_count` shipped exact/un-bucketed (MAYBE → fixed).** No text escaped,
   but it was the one query-derived integer not on the §5.2 ladder — a
   higher-resolution behavioral fingerprint than the design's own principle.
   **Fixed:** bucketed via `count_bucket`. Spec §5.1 example updated.

4. **Tamper clamps on verbatim literal-typed fields (defense-in-depth).**
   `tool`/`outcome`/`calibration_confidence` are nibdex's own fixed values at the
   source but were copied from the file untrusted. **Hardened:** `tool` →
   charset-guarded (`"unknown"` if unsafe), `outcome` → `{error, degraded}` else
   dropped, `calibration_confidence` → `{estimated, measured, derived}` else the
   conservative `"estimated"`. Test: `scrub_row_drops_tampered_literal_fields`.

### Verified CLEAN (no change needed)
Paths → count-only (`shallow_repos`, `file_watcher.subscriptions`);
`last_event_ts` dropped; `count_bucket`/`length_bucket` fully clamp negatives and
`i64` extremes; `ts` provably never reaches any `ExportRow` field (window-filter
reads it then drops it); `cost_savings` is numeric aggregates with closed-set
`per_tool` keys (the D-10.14 net-negative survives); `params` carries no raw
strings (source discipline + key/type allowlist at the scrub); `query`,
`error.message` provably absent from `ExportRow`. No substring/n-gram of any
query escapes via `term_count`/`had_fts5_operators` (both emit only count/bool).

### Residual (one, low-risk)
`calibration_model_version` is a free-text string from the maintainer's own
`calibration.toml` — not corpus-derived, but the one verbatim non-literal value.
It is human-reviewed in the readable payload at the §7.2 approval gate. **Action
before a real proprietary-corpus payload:** eyeball it (and consider a benign-pattern
warning if this is automated later).

### Outcome
Every ALLOW/TRANSFORM field now has either an adversarial no-leak test or a
documented transform; the closed-key-domain assumption is **enforced at the
boundary**, not merely a code invariant the drift test can't see. Suite 147,
clippy clean; live re-run over the real dogfood corpus: zero leakage, zero
unexpected key drops.
