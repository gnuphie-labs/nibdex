# Versioning policy

nibdex follows [SemVer 2.0.0](https://semver.org/) with a deliberately conservative posture on the major version: **stay sub-1.0 until empirical maturity, not feature checklist.**

This document codifies that posture so the version number is honest signaling.

## The bar for 1.0.0

`1.0.0` requires **all three** to be true:

1. **At least 3 unrelated users** in production use (not contributors, not test installs — actual workspaces).
2. **At least 6 months public** without a backwards-incompatible change.
3. **Tool surface stable across at least 3 consecutive minor releases** (no removed tools, no renamed parameters, no changed return shapes on the MCP tool envelopes).

No date commitment. `1.0.0` ships when those gates clear, not before — and not because a feature checklist filled up.

## What `0.x` means here

`0.x` is not "broken." It is "the public contract may change between minors, so pin a version if that matters to you." Concretely:

- Each minor bump (`0.1` → `0.2`) may rename or remove MCP tools, change parameter shapes, or alter ledger row format. The CHANGELOG calls these out explicitly.
- Patch bumps (`0.1.0` → `0.1.1`) never change the tool surface or schema — only behavior fixes, performance, or new optional fields with `#[serde(skip_serializing_if = "Option::is_none")]`.
- The JSONL event stream (DESIGN §5.5) and cost ledger (§8.4) carry their own `schema_version` and `calibration_model_version` fields. Those bump independently of the crate version — see DESIGN for the schema policy.

## Phasing

The phased differentiator roadmap in DESIGN §4 maps loosely to minor bumps:

| Roadmap phase | Earliest version | Notes |
|---|---|---|
| Phase 1a (D2 + D0) | `0.1.x` | Public debut |
| Phase 1b (D1 + D3 — current) | `0.2.x` | Transcript session index (`find_session`/`recent_sessions`) + IP-domain partition + the D1 tail + richer MCP surface |
| Phase 2 (D4–D6) | `0.3.x` – `0.5.x` | Derived graph + provenance + source-change cache invalidation |
| Phase 3 (D7–D9) | `0.6.x` – `0.9.x` | Answer cache + semantic fallback + alternative storage backends |

Phases are **not promises with dates.** They are the lane order. A phase may take one minor bump or three depending on what the dogfood signal surfaces.

### The `0.2.x` line — coherence + isolation

Two things define `0.2.x`, and both illustrate why `0.x` reserves the right to change the public contract between minors:

- **The transcript-based session index.** `find_session` and `recent_sessions` now return per-edit records recovered from Claude Code transcripts — the file, the rationale, and the capturing commit — replacing the empty CLAUDE.md-format corpus. This **changes their return shape** from `0.1.x` (no `session_number`/`todos_mentioned`/`decisions_made`; a flat per-edit record instead). Exactly the kind of contract change the policy above reserves for a minor bump.
- **The IP-domain partition** — a cross-cutting isolation capability ([`IP_DOMAINS.md`](IP_DOMAINS.md)) that spans every corpus, orthogonal to the D-lane roadmap. It is the visible headline of this line.

`0.2.0-rc.1` adds to that line without changing the tool contract: session
indexing folds into `nibdex index`, and `corpus_empty` /
`corpus_indexed_through` / `retired_corpora` are additive-optional fields that
serialize only when they have something to say. Under the patch rule above that
is a within-`0.2.0` change, not a new minor — the release candidate is still
settling, so it stays an `-rc`. It does change one **default**: a plain
`nibdex index` now reads your Claude Code transcripts. That is behavior, not
contract, but it is called out in [`CHANGELOG.md`](../CHANGELOG.md) and
[`SECURITY.md`](../SECURITY.md) rather than left to be discovered.

## Why this posture

Experienced developers don't trust `1.0` / `2.0` versions on visibly-immature projects. SemVer's stability commitment at `1.0` is *binding*: every change after that must either preserve the public contract or carry a major bump. Under-promising and over-delivering is cheaper than the reverse.

Precedents that vindicate the long-`0.x` path: tokio (4 years sub-1.0 with millions of dependents), serde, pip, requests, numpy, FastAPI — all spent years at `0.x` while being widely depended on.

When in doubt between bumping minor and patch on a sub-1.0 release, prefer the smaller bump. Error toward the "anything can change" signal until the gates above actually clear.
