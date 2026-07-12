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
| Phase 1a (D2 + D0 — current MVP) | `0.1.x` | Public debut |
| Phase 1b (D1 + D3) | `0.2.x` | Workspace lexical search + richer MCP surface |
| Phase 2 (D4–D6) | `0.3.x` – `0.5.x` | Derived graph + provenance + source-change cache invalidation |
| Phase 3 (D7–D9) | `0.6.x` – `0.9.x` | Answer cache + semantic fallback + alternative storage backends |

Phases are **not promises with dates.** They are the lane order. A phase may take one minor bump or three depending on what the dogfood signal surfaces.

## Why this posture

Experienced developers don't trust `1.0` / `2.0` versions on visibly-immature projects. SemVer's stability commitment at `1.0` is *binding*: every change after that must either preserve the public contract or carry a major bump. Under-promising and over-delivering is cheaper than the reverse.

Precedents that vindicate the long-`0.x` path: tokio (4 years sub-1.0 with millions of dependents), serde, pip, requests, numpy, FastAPI — all spent years at `0.x` while being widely depended on.

When in doubt between bumping minor and patch on a sub-1.0 release, prefer the smaller bump. Error toward the "anything can change" signal until the gates above actually clear.
