# Limitations

This document is the honest counterpart to README + EXAMPLES. It enumerates what nibdex **does not do**, what it does **imperfectly**, and where the rough edges are. Every limit listed here is intentional, scoped, or known-and-deferred — nothing is silently swept.

If a limit below blocks your use case, file an issue. Dogfood evidence is how the bars to deviate (DESIGN §6) get re-examined.

## 1. Non-goals (out of scope indefinitely)

These are scope-fence items from DESIGN §6. Including them in proposals is a red flag.

- **Foundation model training, hosting, or fine-tuning.** nibdex consumes models; it does not produce them.
- **Cloud-hosted SaaS version.** The whole point is local-first. A hosted version would invert the mission.
- **Multi-tenant features.** Single user, single workspace.
- **Authentication, authorization, RBAC.** Local tool — the user has filesystem access, that is the authorization model. The optional HTTP transport binds loopback-only and refuses non-loopback bind addresses at startup (DESIGN D-6.4.3).
- **Real-time collaboration.** No CRDTs, no sync, no presence.
- **Graphical user interface.** The MCP client is the UI.
- **Wiki-style content authoring.** nibdex reads; it does not provide editing surfaces.
- **General-purpose vector database.** Semantic search is a Phase 3 fallback, not a database we offer.
- **General-purpose graph database.** The graph is derived from workspace signals for the retrieval use case; it is not a Neo4j replacement.
- **Compatibility with a curated-corpus tool's data formats.** Structural divergence is the point. Migration tooling for such users would be reasonable; format compatibility is not.

## 2. Known operational limits in the current release

### Query syntax is raw FTS5 — hyphens are the NOT operator

The `query` parameter on all `find_*` tools is passed to SQLite FTS5 as a `MATCH` expression unchanged (DESIGN D-6.1.4). FTS5 treats `-` as the NOT operator, so `find_commit(query: "fan-out")` parses as `fan NOT out` and errors with `no such column: out`.

**Workaround:** quote the term — `find_commit(query: "\"fan-out\"")` matches the phrase verbatim.

**Why we don't auto-quote:** the tool intentionally exposes raw FTS5 semantics so power-user queries (`bb8 AND pool`, `"exact phrase"`, `prefix*`) work without translation. Auto-quoting would hide the operator without solving the underlying mismatch between natural-language compounds and FTS5 grammar. A future minor bump may surface FTS5 syntax errors as structured JSON-RPC errors with a hint, or add a `mode: "phrase"` parameter.

### `--metrics-sink stdout` interleaves with stdio MCP transport

If you run `nibdex mcp --metrics-sink stdout`, the JSONL event stream lands on the same file descriptor as the MCP JSON-RPC wire. A strict MCP client parsing line-by-line may error on the non-JSON-RPC metrics objects.

**Production combinations:**
- stdio MCP + `--metrics-sink jsonl:<path>` (recommended).
- stdio MCP + `--metrics-sink off` (default — zero overhead, no ledger).
- HTTP MCP (`nibdex serve --http`) + any sink — stdout is separate from the MCP wire.

### Cost ledger requires the JSONL sink to be enabled

`--metrics-sink off` (the default for stdio mode) short-circuits the Layer-2 cost-ledger insert as well as the Layer-1 JSONL emit. The ledger is meaningful only alongside the event stream that consumers correlate with.

If `check()` reports `cost_savings: null`, calibration is not loaded. If `cost_savings.window_*.queries_served` is zero, calibration is loaded but the sink is `off`. Pass `--metrics-sink jsonl:<path>` to populate the ledger.

### Calibration is a single-model rate, Sonnet-anchored

The seeded `calibration.toml` uses Anthropic's Sonnet input-token rate (currently `$3 / Mtok`) as the dollar denominator. Users on Opus will see savings figures that under-report by roughly 5×; users on Haiku will see figures that over-report by roughly 10×.

**Workaround:** edit `calibration.toml`'s `input_rate_usd_per_mtok` to your model's actual rate before deploying.

**Why we don't auto-detect:** nibdex doesn't see the model in use — it serves an MCP client, and the client makes its own model choices per-call. Multi-model rate selection is Phase 2.

### `calibration_confidence: "estimated"` is the honest tag, not a placeholder

Every event in the cost ledger carries `calibration_confidence: "estimated"`. The seeded counterfactual numbers are anchored to the author's dogfood corpus shape and the DESIGN §2.4 anecdotes, not to measured A/B comparisons of instrumented client sessions with and without nibdex available.

**Implication:** the dollar figures are good for **trend and order-of-magnitude**, not for billing-grade accounting. Phase 2 introduces sampled counterfactual measurement to promote individual tool calibrations from `estimated` to `measured`. Until then, treat the ledger as evidence the tool is shifting tokens out of the prompt, not as a precise invoice.

### Session-history extractor is author-formatted

nibdex's session-history extractor parses the `## Recent session history (one-line)` section of the workspace's `CLAUDE.md` using a regex anchored to a specific entry shape (the format the author uses in this workspace). Workspaces whose `CLAUDE.md` follows a different convention will see zero or partial coverage in the `session_entries` corpus.

**Workaround today:** if your session-history shape differs and you want indexing, the extractor is in `src/extractor/session_history.rs`. The other corpora (git commits, memory entries, design docs) work regardless.

**Phase 1b candidate:** a `--session-history-format <preset>` flag, or pluggable extractor configuration.

### File-watcher is daemon-only

The `nibdex mcp` stdio transport does **not** spawn the watcher — the process exits at session end. Between sessions, the index reflects whatever the most recent `nibdex index` run captured.

For cross-session warmth + incremental indexing, run `nibdex serve --http 127.0.0.1:<port>` or `nibdex watch` under your OS init system (`launchd` / `systemd`). The daemon shapes are documented in DESIGN §5.3 + §5.4.

### Memory-directory auto-detection is Claude-Code-specific

The default memory-directory resolver encodes the workspace path with Claude Code's convention (`/` and `_` and `.` → `-`). Other MCP-speaking clients with their own memory conventions need `--memory-dir <path>` passed explicitly.

### `bm25` ranking surfaces density, not always relevance

For cross-corpus terms, `find_commit("rustFetch")` ranks the densest occurrence (the canonical fan-out commit) above commits that *fix* or *audit* the same term. This is usually right (the canonical change is the touchstone), but a query like `find_commit("rustFetch wedge fix")` would benefit from phrase or proximity boosts. Filed for dogfood-pattern review, not a current default change.

### Always-on indexing requires `git2`-readable repositories

The git-commits corpus uses `libgit2` (via the `git2` crate). Repositories whose layout `git2` cannot read (corrupted refs, partial clones with missing-but-referenced objects, non-standard packed-ref formats) are reported as "shallow" or skipped in `check().indexed_repos`. The `indexer` field surfaces this so you know coverage is partial; nothing is silently dropped.

### No telemetry — by design

No anonymized aggregate reaches a project server. No phone-home. The JSONL sink writes to a local path the user controls; users who want zero observability pass `--metrics-sink off` (the default) and the daemon emits nothing. The cost ledger lives in the local SQLite file alongside the index — same posture as the cache substrate.

This is a *limitation* only in the sense that the project receives no operational visibility into real-world use. Reports from dogfooders are the only signal that drives bar-to-deviate re-examination on the items above.

## 3. Related work / what nibdex is NOT

nibdex sits adjacent to several tool classes. Knowing where it doesn't apply is part of knowing where it does.

- **Shell-output filters / token killers** (e.g., tools that intercept command stdout and compress it before it lands in the LLM context). Different layer — those operate **post-command** on output the model already requested. nibdex operates **pre-command** by serving structured retrieval as a substitute for ad-hoc greps, file reads, and CLAUDE.md scrolls. The two are complementary, not competitive.
- **Curated knowledge bases** (curated-corpus KB tools, Obsidian-driven AI assistants, hand-authored wikis). nibdex is the inverse posture — the graph is *derived* from workspace signals (filesystem, git, memory directory) without authoring overhead. Migration tooling for curated KBs is reasonable; format compatibility is not.
- **General-purpose code search** (ripgrep, `git grep`, IDE search). Those are excellent for in-file lexical matches. nibdex adds the cross-corpus join — git commits + session decisions + memory rules + design-doc sections, queryable together with one ranking surface — which per-corpus tools can't reach without an AI synthesis step.
- **Vector / semantic search** (LSP-shaped tools, embedding databases). Phase 3 introduces local semantic search as a fallback for lexical+graph misses. Until then, nibdex is honestly lexical.

## 4. Where the limits get reconsidered

Each limit above has a *bar to deviate* — a specific dogfood signal that, if observed, justifies revisiting the design. DESIGN §6 enumerates the bars in full. The short version: if a real workflow trips on a limit listed here, file an issue with the workflow shape and the workaround you tried. Anecdotal pressure on a bar accumulates until the bar tips.
