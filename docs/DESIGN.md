# nibdex — Design Document

**Status:** Draft v0.2.1 (2026-05-26)
**License:** MIT

**Version log:**
- v0.2.1 (2026-05-26) — Project name locked: **nibdex** (coined portmanteau pen-nib + index-dex; cleared a 4-round namespace audit across crates.io / npm / pypi / GitHub). O2 closed.
- v0.2 (2026-05-25) — MVP scope expanded to include git history as fourth indexable corpus; O6 closed (daemon mode LOCKED MVP-required); new O9 filed for further dev-environment corpora (high-bar gate); **§5.5 (instrumentation primitive) + §8.4 (cost-savings measurement framework) added** as MVP-required infrastructure — the load-bearing measurement layer for §3 thesis falsifiability and §12.1 inverted-development discipline; **MVP scope LOCKED**.
- v0.1 (2026-05-25) — Initial design; O1 closed (D2-alone MVP); O3 closed (MIT license).

---

## 0. Document purpose

This is the consolidation point for an architectural dialogue conducted in 2026-05, building on prior empirical investigation via a retrieval-eval harness. It exists to:

1. Capture the design philosophy in a form that can be stepped back from and reviewed.
2. Hold the line against scope drift by phasing differentiators rather than dropping them.
3. Serve as the gating artifact for "should we start building" once reviewed and refined.

The design is for a Rust MCP server that exposes local workspace knowledge to AI clients (Claude Code / Claude Desktop / any MCP-conformant client) in a way that minimizes recurring cost and maximizes day-one workspace coverage. It is **not** a clone of any existing product, including the curated-corpus knowledge-base tool that has served as the reference benchmark.

---

## 1. Mission & constraints

### 1.1 Mission

Build an open-source MCP-conformant local knowledge tool that allows budget-constrained developers to use AI clients (primarily Claude, but architecturally any MCP client) effectively against their own workspace, by pushing the expensive "grind" work to local CPU and reserving paid AI tokens for the genuinely irreplaceable step: synthesis.

The mission has four named beneficiaries:

- **The author personally**: stretches a fixed AI budget; keeps a programming career viable on retirement income.
- **Other budget-constrained developers**: especially those on fixed incomes, in less wealthy economies, or in early-career circumstances where subscription stacks are unaffordable.
- **The open-source ecosystem**: a credible artifact that ships and works, demonstrating that the "AI for indie devs" niche can be served without VC-backed SaaS.
- **The author's consulting funnel**: a polished, well-documented, working tool serves as a credibility signal for paid engagement work.

### 1.2 Hard constraints

These are non-negotiable design constraints. Any feature that would violate one of these does not ship.

- **Zero recurring infrastructure cost** once installed. No required cloud services, no required subscriptions beyond what the user has already chosen (e.g., their Claude API key).
- **Runs on modest hardware.** A 7-year-old laptop with 8 GB RAM and no GPU must be a fully supported deployment target. No required GPU, no required >2 GB RAM working set.
- **MCP-conformant** by design, not as an afterthought. Distribution moat depends on every Claude-using developer being able to add this tool with a single config line.
- **No required content authoring.** A new user installs the tool and gets value immediately against their existing workspace. No "first write 100 wiki entries" friction.
- **Local-first.** If the user's network goes down or the tool's developer disappears, the tool keeps working. No telemetry that requires a remote endpoint to function.
- **Single static binary** distribution where possible (`cargo install`, homebrew tap, GitHub release). No `pip install -e` developer-experience workarounds.

### 1.3 Soft constraints

These shape decisions but allow case-by-case judgment.

- **Boring tech wins ties.** SQLite over a custom format; ripgrep over a hand-rolled scanner; tree-sitter over hand-written parsers.
- **Prefer composition over framework.** Tools as discrete callable units; don't build a "platform."
- **Documentation must reach strangers.** Tier-1 README has to be valuable in 60 seconds of reading. Tier-2 docs get the user productive in 10 minutes. Tier-3 docs document deep configuration.
- **Optimize for the consulting funnel signal.** Repo, commits, tests, CI quality are themselves the artifact's marketing.

### 1.4 Guiding principles

The discipline nibdex holds itself to. These are **design principles, not guarantees** — where a principle
names an outcome (tokens saved), its honest form is an *estimate until measured* (§11). They are the north
star behind every feature decision; an idea that can't satisfy them doesn't graduate from the incubator
to built work.

1. **Lean and high-leverage.** Do the small slice of retrieval work that removes most of the
   "go re-read the whole repo" cost. A feature earns its keep or it doesn't exist.
2. **Pay for itself in *net* tokens and speed.** Judged end-to-end at the agent loop (what the AI client
   spends), not by how clever nibdex is internally. Cost tolerance scales with benefit but has a ceiling —
   a high-value feature earns a bigger budget, no feature gets an unbounded one — and the keep/drop call is
   re-made against measured reality (the cost ledger, §8.4), not the up-front estimate.
3. **A silent, self-managing worker.** It does the grunge work the developer and their AI client shouldn't
   have to think about. Control is Claude-driven via *optional* near-free hints; there is nothing a human is
   expected to tune. Prefer autonomy (sensible defaults, shed/rehydrate on demand) over any feature that
   *requires* steering.
4. **Under-promise, over-deliver — honest measurement, never hype.** Savings are reported as estimates
   until calibrated against real use (`calibration_confidence`, §11); no unverifiable percentage claims.
   Roadmap is labeled roadmap, never stated as present capability. We would rather claim less than nibdex
   does and let the real numbers exceed expectations than market ahead of the evidence.

---

## 2. Empirical foundation

The design is not speculative. A prior sequence of retrieval-evaluation investigations — a Python eval harness running a locked query suite against a real developer-workspace corpus, graded by an LLM judge (Opus 4.7 with adaptive thinking and structured outputs) — established the qualitative findings below. The specific per-backend scores were measured on a corpus that is not part of this public project; they are being re-baselined on the public dogfood corpus (§13.2), so this section summarizes the findings that carried into the design rather than the raw benchmark table.

### 2.1 What the comparison established

Three retrieval strategies were compared on a top-5 distinct-answer-files metric (of 50 possible answer files), with per-query cost tracked alongside. This is what the author measured for **their own broad-coverage workflow** — not a verdict on any tool; a different workflow (heavier curation, a narrower corpus) could reasonably land somewhere else.

- **Plain lexical retrieval** (ripgrep + file-aggregation) over the *full* workspace was the strongest and cheapest here — broad coverage at ~$0/query.
- **Lexical + LLM rerank** scored *lower* despite the added token cost — on these corpora the rerank step didn't earn its price.
- **A paid curated-corpus tool** scored lowest *on this coverage-oriented metric* — by design it indexes only manually-authored slices (~5% of this workspace), so it returned nothing on the queries whose topics the curated corpus hadn't been authored to cover. That's the curated model working exactly as intended for a *precision-over-coverage* need; it is simply the opposite of what a broad-coverage workflow wants.

(Specific per-backend scores were measured on a non-public corpus and are being re-baselined on the public dogfood corpus per §13.2. The ordering and the structural conclusions below are what carried into the design.)

### 2.2 Key findings

1. **Lexical retrieval with structural priors beats LLM rerank** on code/prose corpora. File-aggregation (rolling per-line hits up to per-file scores) combined with path-class boost (memory entries and design docs weighted higher than third-party code) is a near-optimal prior on this kind of query.

2. **"Lazy beats aggressive when consumer-amortizable."** Validated twice independently: graph propagation Crux 2 (decay=0 beat decay=0.5 and decay=0.8) and LLM rerank Crux 3 (no-rerank beat rerank). The pattern: when the downstream consumer (LLM synthesizer, file-agg scorer) can amortize cleanup work, doing less upstream wins.

3. **Coverage dominates retrieval sophistication** for budget-constrained users. The simplest retrieval (ripgrep) over 100% of the workspace beat sophisticated retrieval (semantic rerank) over ~5% of the workspace by a wide margin on this benchmark.

4. **The curated-corpus model scales cost with coverage — a tradeoff, not a flaw.** A curated-corpus tool charges proportionally to how much you curate. For a *broad-coverage* need that's a structural mismatch — the author's own pain point, "95% of my workspace is invisible without spending more money," is not a pricing complaint but a sign that the curated model and the broad-coverage need pull in opposite directions. For a *curation-first* need, that same tradeoff reads as a feature.

5. **Information asymmetry between rerank and judge matters.** Rerank operates on 200-char snippets; judge sees 800-char widened snippets. Same model family, different effective context. (Promotes future work on snippet widening.)

### 2.3 What the empirical record does not yet establish

- Whether session-history indexing (the proposed novel differentiator) measurably beats raw ripgrep over CLAUDE.md.
- Whether a derived graph layer adds usable signal on top of file-aggregation.
- Whether a provenance-aware answer cache achieves >20% hit rate on real workloads.
- Whether local semantic search (frozen small embedding model) earns its complexity vs. lexical+graph.

These are the gates each phase must pass before committing further.

### 2.4 In-session dogfood moments (anecdotal)

The 2.1 numbers are rigorous against a locked corpus. Two organic incidents during the drafting of this design document complement them with live experience.

**Moment 1 — Toolchain archaeology.** The author requested a rendered PDF of this markdown. Without nibdex, reconstructing the workspace's existing PDF-rendering convention required a probe cascade: `which` against multiple candidate tools (most not installed); survey of language runtimes and browser availability to choose a substrate; full directory listing of `scripts/` (~50 entries) to find any existing renderer; `grep -rln "print-to-pdf"` across docs to recover the right incantation; `stat` on `{md,html,pdf}` triplets to confirm the in-tree filing convention; full read of an existing renderer script (~226 lines) to crib its CSS. Estimated cost: **8–12k input tokens** to reconstruct knowledge already implicit in the workspace.

Counterfactual nibdex call:

```
nibdex search "render markdown to HTML and PDF in this workspace"
→ scripts/render_<doc>_html.py          (canonical template + CSS)
→ docs/<area>/<doc>.{md,html,pdf}       (filing convention)
→ docs/design/<related_doc>.md          (Chrome --print-to-pdf reference)
→ session entries — recent uses of the pattern
```

Estimated cost: **<500 input tokens** + small synthesis cost.

**Moment 2 — Workspace memory.** After adding Moment 1 to this doc, the path to the doc itself was needed for the next edit. The reflexive curated-knowledge-base search returned unrelated sections because the curated corpus had no view into the in-session design work just created on disk. Recovery required conversation memory of the path, not a queryable representation.

Counterfactual nibdex call:

```
nibdex search "nibdex design doc"
→ docs/DESIGN.md                            (top hit, line snippet)
→ scripts/render_design_html.py             (companion renderer)
→ docs/DESIGN.html                          (rendered output)
→ docs/DESIGN.pdf                           (rendered output)
```

Estimated cost: **<100 input tokens.**

**Why these matter together.** The two moments are different flavors of the same gap:

- Moment 1 is **toolchain archaeology** — "what's the established way to do X here?" — typically fires once per recurring task category.
- Moment 2 is **workspace memory** — "where did we put Y?" — fires on every artifact creation, especially across fresh sessions when conversation context resets.

The curated-corpus baseline discussed in §2.1 cannot help with either: it has no view into the workspace beyond manually-authored slices. The hypothesized nibdex closes both gaps at trivial token cost. These two moments together are filed as supporting evidence for the **D1 + D2 MVP scope cut** in Section 10, O1.

---

## 3. Thesis (one sentence)

> **An always-available MCP knowledge tool for the keystone resources every dev environment already has — workspace files, git history, AI session history — where the graph is derived not curated, and the cache invalidates with the code.**

Three commitments unpacked from that sentence:

- **"Always-available"** — a long-running local daemon serving warm queries with file-watcher-driven incremental indexing. Drop in, configure once, forget about it. Cold-start cost is amortized; the AI client never waits on rglob warmup mid-session.
- **"Keystone resources every dev already has"** — the tool indexes the structured artifacts that already exist in every developer's environment: workspace files, git commit history, AI session history (CLAUDE.md / memory dirs), design docs. No content authoring required for day-one value. *Derived, not curated*: relationships between entities are extracted from signals (code references, file mentions, commit co-occurrence, session-entry cross-refs) rather than authored by humans.
- **"Cache invalidates with the code"** — when source files change, dependent cached AI syntheses are automatically marked stale. The cost moat is durable, not brittle.

### 3.1 Why those three together are the bet

Each commitment alone is unremarkable. Ripgrep handles "workspace files you already have" trivially. `git log` handles git history trivially. Code-index tools (Sourcegraph, OpenGrok, IDE indexers) handle "derived" within code. Build systems handle "cache invalidates with code." Long-running daemons are a 50-year-old idea. The bet is that **combining all of these — always-on, multi-corpus (files + git + session history + design docs + memory), derived linkage, cache-with-code — at the prose layer** is the differentiation. The curated-corpus baseline ships the cache-with-code commitment (via wiki-link checks) but explicitly requires the *opposite* of the first two: a human-curated slice corpus, no daemon, no git/session integration.

The empirical case is that the **keystone-resources commitment dominates**. A naïve ripgrep over the full workspace beats a sophisticated retrieval over a handful of curated slices by a wide margin on the same query suite. That is not a tuning gap; it is a structural gap. The novelty in this design is making "derived not curated" produce *more* knowledge from the same source material than any curated approach can — at the same zero-incremental-cost the ripgrep baseline already proved — and extending that to the corpora the author already pays attention to (git, session history, memory) that no existing tool indexes together.

The bet is falsifiable. If D2 (the structured-workspace-history corpus: session + git + memory + design docs) ships and the eval harness measures < 2/50 lift over ripgrep-only on the combined query suite, the bet failed and we regroup. That is §9.2 below.

---

## 4. Differentiators (phased)

Every architectural advantage identified in the design dialogue is captured here. None are dropped; some are phased. The phase tag is enforceable scope discipline.

### MVP — Phase 1a (D2 expanded + always-on daemon)

| ID | Differentiator | Why novel | Implementation cost |
|---|---|---|---|
| **D2** | Structured workspace history indexer (session history + git commits + memory + design docs, all as queryable typed records sharing one FTS surface) | No existing tool indexes CLAUDE.md session entries, git commit corpus, memory files, and design docs *together* as structured records. Git is universally present in dev workspaces but no AI-facing tool surfaces it as a first-class searchable corpus with cross-corpus linkage. Combined, these cover ~100% of "how did this workspace get to here?" queries — a class of question no existing retrieval tool answers cheaply. | Medium (regex + libgit2 + schema) |
| **D0** | Always-available local daemon (long-running process with file-watcher incremental indexing, warm SQLite, no per-query rglob cold-start) | Closes the §10 O6 latency gate. The curated-corpus baseline does not ship as a daemon; ripgrep is per-invocation by nature. Combined with D2 this is the "drop in and forget" experience, indexed corpus stays current as the workspace edits, AI client never waits on warmup. | Low-Medium (rmcp stdio + `notify` crate + optional HTTP transport for cross-session warmth) |

**Rationale (closes O1 + O6):** D2's broadened scope reflects the §3 thesis update — the *keystone resources* framing demands the indexer cover what the user already pays attention to in a typical dev environment, not just the prose corpora. Git is the cheapest, highest-coverage addition (every repo has it; `git2` crate is mature; `git log --since=<last_oid>` makes incremental indexing trivial). D0 closes O6: the "always-available helper" framing makes daemon-mode non-negotiable. D1 (workspace lexical search) and D3 (MCP wrapping) are valuable but not differentiating — a tool that does *only* D1 is "yet another local-search tool," and D3 is mechanical glue. Shipping D2+D0 first stakes the differentiation flag publicly while the runway-to-flip window is still open. The Moment 1 and Moment 2 dogfood anecdotes at §2.4 already capture two of three empirical crux classes; the git-corpus addition addresses a third class (commit-history archaeology + cross-corpus *why-did-this-change* queries) flagged as a known recurring pain — captured as a predicted-not-observed crux to be empirically validated during the §13.2 private dogfood window.

**Scope LOCK:** The MVP D-list above is closed. Adding any further D-entry to MVP requires crossing the §9.1 bar (named empirical retrieval-quality gap in the eval harness that no existing D1–D9 differentiator addresses). Defaults to defer.

### MVP — Phase 1b (D1 + D3, follows D2)

| ID | Differentiator | Why novel | Implementation cost |
|---|---|---|---|
| **D1** | Full-workspace lexical search with no curation step | The curated-corpus baseline and most KB products require authoring; this delivers day-one coverage of the existing workspace | Low (ripgrep crate + SQLite FTS5) |
| **D3** | MCP server wrapping L1+L2 | Distribution leverage; addressable surface = all Claude users | Low (rmcp crate or hand-rolled JSON-RPC) |

D1 and D3 are explicitly not abandoned — they are sequenced after D2 has shipped publicly so the differentiation framing isn't diluted at first contact with the open-source audience. D3 in particular is mechanical and generates no empirical signal; deferring it costs nothing.

### Phase 2

| ID | Differentiator | Why novel | Implementation cost |
|---|---|---|---|
| **D4** | Derived graph layer | Graph edges extracted from tree-sitter code analysis, prose file-mentions via regex, git commit co-occurrence — without human linking | Medium-High (tree-sitter integration) |
| **D5** | Provenance metadata on every retrieved record | Sources tracked through the pipeline so downstream consumers (cache, judge, AI) know what derived from what | Low (just schema + bookkeeping) |
| **D6** | Source-change cache invalidation | Cache entries tagged by source-file hashes; touched files invalidate dependent records automatically | Medium (file-watcher or check-on-read) |

### Phase 3

| ID | Differentiator | Why novel | Implementation cost |
|---|---|---|---|
| **D7** | Provenance-aware answer cache | Past AI syntheses stored, keyed by question + source-hash; served instead of re-synthesizing when source unchanged. Most LLM caches are dumb key-value. | Medium |
| **D8** | Local semantic search as fallback | Frozen small embedding model (e.g., bge-small-en-v1.5 ~33 MB) called only when lexical + graph come up thin. Cost defaults to zero. | Medium (candle-rs or ort runtime) |
| **D9** | PG harness as alternative to SQLite | Power-user option; lets existing PG-fleet users plug in their own store without abandoning the project | Low-Medium (trait abstraction work) |

### Later (no commitment yet)

- Other AI clients beyond Claude (will track MCP ecosystem maturity)
- Multi-workspace federation
- Network-shared knowledge bases
- Plugin/extension framework for third-party tool authors

---

## 5. Architecture sketch

The system is a Rust MCP server fronting a layered local knowledge backend.

```
┌─────────────────────────────────────────────────────────────────┐
│  MCP CLIENT (Claude Code, Claude Desktop, etc.)                 │
└──────────────────────────────┬──────────────────────────────────┘
                               │ stdio / HTTP (MCP JSON-RPC)
┌──────────────────────────────▼──────────────────────────────────┐
│  MCP SERVER (Rust binary)                                       │
│  • search / section / refs / recent_sessions / answer_cache    │
└──────┬──────────────────────────────────────────────────────────┘
       │
       ├─► L1: Lexical search  (ripgrep + FTS5)            [MVP]
       ├─► L2: Session-history records  (SQLite tables)     [MVP]
       ├─► L3: Derived graph  (SQLite edges, tree-sitter) [Phase 2]
       ├─► L4: Provenance metadata                          [Phase 2]
       ├─► L5: Answer cache  (SQLite KV + source-hash)     [Phase 3]
       └─► L6: Local semantic  (bge-small via candle-rs)    [Phase 3]
                                                            (optional)

  Storage substrate: SQLite (primary). PG harness optional [Phase 3].
```

### 5.1 MCP tool surface — D2 + D0 MVP (Phase 1a)

The MVP exposes only the tools needed for the D2 (multi-corpus structured workspace history) + D0 (always-available daemon) differentiation. Each tool is named for what it returns (digest-shaped output, not raw data):

- `recent_sessions(filter?: string, days?: int, limit?: int)` → digest of session-history entries matching filter, with metadata: session_number, date, summary, files_touched, todos_mentioned, decisions_made. Returns top-N by recency-weighted relevance.
- `find_session(query: string, limit?: int)` → ranked session entries by content match against the query (lexical, not semantic). Returns the same structured shape as `recent_sessions` but filtered by relevance instead of recency.
- `recent_commits(filter?: string, days?: int, repo?: string, limit?: int)` → digest of git commit entries by recency-weighted relevance, with metadata: commit_hash (short), authored_at, author_email, repo_path, message_summary, files_changed, parent_hashes. Optional `filter` runs against message text (summary + body; `files_changed` is returned but not yet searchable); optional `repo` scopes to one nested repository.
- `find_commit(query: string, repo?: string, limit?: int)` → ranked commit entries by content match against `message_summary || message_body` (lexical, not semantic; `files_changed` is returned but not yet indexed). Returns a structured commit record with summary, body, files changed. (Cross-refs to session entries mentioning the same files: planned, not returned today.)
- `find_memory(query: string, limit?: int)` → ranked memory entries (`feedback_*`, `project_*`, `reference_*`, `user_*` files) by content match. Returns name, type, body, related links.
- `find_design_doc(query: string, limit?: int)` → ranked design-doc sections by content match. Returns doc path, section heading path, body excerpt, references in/out.
- `check()` → validates the indexer's view of the workspace + git repos against the current filesystem; reports drift, per-corpus extraction stats, repos indexed vs skipped (shallow / monorepo-nested), and daemon health. Parity with the curated-corpus tool's discipline mechanism.

**Deliberately NOT in MVP:**
- General-purpose `search` over arbitrary source files → that's D1, Phase 1b.
- `section`, `refs`, `graph_traverse`, `provenance` → those are Phase 2 (graph layer).
- `cached_answer`, `semantic_search` → Phase 3.
- Cross-corpus join tools (e.g., "sessions touching files in a given commit set") — emergent from individual tools' output for now; will promote to first-class only if D2 dogfood shows a clear repeated query shape.

The MVP tool surface is narrow on purpose. Six query tools + one validation tool. A reviewer looking at the README should be able to predict the entire MVP behavior from the tool list alone.

### 5.2 Storage schema — D2 + D0 MVP

The MVP needs six tables plus the FTS5 virtual table. All in SQLite; single file on disk. Five typed content tables (session entries / commit entries / memory entries / design-doc sections / documents-as-files) plus one per-repo indexer-cursor table.

```sql
-- The raw unit of indexed file-based content (CLAUDE.md, memory files, design docs).
-- Git commits are NOT files in this sense; they live in commit_entries directly.
CREATE TABLE documents (
    id              INTEGER PRIMARY KEY,
    path            TEXT NOT NULL UNIQUE,         -- absolute or workspace-root-relative
    kind            TEXT NOT NULL,                 -- 'session_history' | 'memory' | 'design_doc'
    content_hash    TEXT NOT NULL,                 -- sha256, used for change detection
    mtime           INTEGER NOT NULL,              -- file mtime (unix seconds)
    indexed_at      INTEGER NOT NULL
);

-- Session-history entries extracted from CLAUDE.md.
-- One row per "### #NNN" entry under "Recent session history".
CREATE TABLE session_entries (
    id              INTEGER PRIMARY KEY,
    document_id     INTEGER NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
    session_number  INTEGER NOT NULL,              -- e.g. 606
    entry_date      TEXT,                          -- 'YYYY-MM-DD' if extractable
    body            TEXT NOT NULL,                 -- full entry text
    files_touched   TEXT,                          -- JSON array of paths mentioned
    todos_mentioned TEXT,                          -- JSON array of '#NNN' ids
    decisions_made  TEXT,                          -- JSON array of short decision strings
    UNIQUE(session_number)
);

-- Git commit entries from one or more repositories under the workspace.
-- Source = `.git` directories discovered during scan; one row per commit per repo.
CREATE TABLE commit_entries (
    id              INTEGER PRIMARY KEY,
    repo_path       TEXT NOT NULL,                  -- workspace-relative path containing the .git dir
    commit_hash     TEXT NOT NULL,                  -- full 40-char SHA
    parent_hashes   TEXT,                           -- JSON array (first parent first; merges have ≥2)
    author_email    TEXT,
    author_name     TEXT,
    authored_at     INTEGER NOT NULL,               -- unix seconds (author timestamp)
    committed_at    INTEGER NOT NULL,               -- unix seconds (committer timestamp)
    message_summary TEXT NOT NULL,                  -- first line of commit message
    message_body    TEXT,                           -- everything after the first blank line
    files_changed   TEXT,                           -- JSON array of paths touched (add/mod/del; via Tree::diff_to_tree)
    branch_refs     TEXT,                           -- JSON array of refs containing this commit at index time
    UNIQUE(repo_path, commit_hash)
);
CREATE INDEX idx_commit_repo_authored_at ON commit_entries(repo_path, authored_at DESC);

-- Per-repo indexer cursor for incremental git extraction.
-- One row per discovered .git dir under the workspace.
CREATE TABLE indexed_repos (
    repo_path           TEXT PRIMARY KEY,
    last_indexed_oid    TEXT NOT NULL,             -- commit hash up to which we've indexed
    is_shallow          INTEGER NOT NULL DEFAULT 0,-- 1 = shallow clone detected, flagged in check()
    commit_count        INTEGER NOT NULL DEFAULT 0,
    last_indexed_at     INTEGER NOT NULL
);

-- Memory entries from ~/.claude/projects/.../memory/*.md.
CREATE TABLE memory_entries (
    id              INTEGER PRIMARY KEY,
    document_id     INTEGER NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
    name            TEXT NOT NULL,                 -- frontmatter 'name:' slug
    memory_type     TEXT NOT NULL,                 -- 'user' | 'feedback' | 'project' | 'reference'
    description     TEXT,                          -- frontmatter description
    body            TEXT NOT NULL,
    UNIQUE(name)
);

-- Design-doc sections from docs/design/*.md.
CREATE TABLE design_doc_sections (
    id              INTEGER PRIMARY KEY,
    document_id     INTEGER NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
    heading_path    TEXT NOT NULL,                 -- e.g. '5.2/Storage schema' (slash-joined ancestors)
    line_start      INTEGER NOT NULL,
    line_end        INTEGER NOT NULL,
    body            TEXT NOT NULL
);

-- FTS5 virtual table indexing all bodies. Single FTS table across all content kinds.
CREATE VIRTUAL TABLE search_index USING fts5(
    body,
    kind UNINDEXED,           -- so we can filter by kind at query time
    rowid_ref UNINDEXED,      -- foreign-id back to the source table
    source_table UNINDEXED    -- 'session_entries' | 'commit_entries' | 'memory_entries' | 'design_doc_sections'
);
```

The FTS5 table is the single retrieval surface. The six query tools above (`recent_sessions`, `find_session`, `recent_commits`, `find_commit`, `find_memory`, `find_design_doc`) are thin wrappers that constrain the FTS query by `kind` and join back to the typed source table.

**Note on monorepo shape.** Many dev workspaces are a parent directory over multiple nested service repos (each with its own `.git`). `indexed_repos` naturally handles this — one row per repo, one `last_indexed_oid` per repo. `commit_entries.repo_path` keeps the corpora distinct while sharing the FTS surface; queries can filter by repo or span them all. See §9.12 for the configurable depth caps that keep this safe for very-large or pathological histories.

### 5.3 Indexer model — D2 + D0 MVP

The indexer runs inside the always-on daemon (D0) as two operations: **full scan** (on first launch or after `check()` detects drift) and **incremental update** (triggered continuously by file-watcher events while the daemon is running).

**Full scan:**

1. Walk three file-target directories with deterministic ordering:
   - `$WORKSPACE/CLAUDE.md` (single file, session-history extraction)
   - `$CLAUDE_PROJECT_MEMORY_DIR` (memory entries — auto-detected from `~/.claude/projects/<encoded-path>/memory/`)
   - `$WORKSPACE/docs/design/**/*.md` (design docs)
2. Discover git repos: find every `.git` directory under `$WORKSPACE` to a configurable depth (default 3); seed `indexed_repos` rows with `last_indexed_oid=NULL`. Skip nested repos whose parent is also a repo by default (configurable for monorepo cases where nested histories matter).
3. For each file-target: compute `content_hash`; if unchanged in `documents`, skip. Otherwise re-extract.
4. For each discovered repo: walk commits from HEAD ancestry (and optionally other named branches, configurable) until reaching `last_indexed_oid` (full scan from root if NULL); cap walk at `max_commits_per_repo` (default 50,000, configurable) to defend against pathological histories.
5. Extraction is per-kind:
   - **Session history:** regex split on `^### #(\d+)` headings within `## Recent session history`; per-entry parsing pulls date (`(20\d{2}-\d{2}-\d{2})`), file paths (matches against existing-file check), TODO numbers (`#(\d{3})\b` referencing known TODO range), decision keywords (`CLOSED`, `SHIPPED`, `LOCKED`, `FILED`).
   - **Git commits:** libgit2 walk via `git2` crate (HEAD ancestry by default). Per-commit extraction: full SHA, parent hashes JSON, `Signature::email`/`name`/`when` for author + committer, message split on first blank line into summary + body, `files_changed` via `Tree::diff_to_tree(previous)` name-only enumeration, optional `branch_refs` snapshot via `Repository::branches()`. Stop walk at `last_indexed_oid`; on success, update cursor.
   - **Memory:** YAML frontmatter parser for `name:`, `description:`, `metadata.type:`; body is everything after the second `---`.
   - **Design docs:** markdown AST walk (pulldown-cmark or comrak crate); heading hierarchy → `heading_path`; section body = text between heading and next same-or-higher heading.
6. Upsert rows; rebuild `search_index` entries for changed sections.

**Incremental update** (daemon-resident, file-watcher driven via `notify` crate):

- **Filesystem events** on `$WORKSPACE/CLAUDE.md`, `$CLAUDE_PROJECT_MEMORY_DIR`, `$WORKSPACE/docs/design/**` → debounce 500ms; coalesce bursts; for each touched file, mtime + hash compare against `documents`; same-hash bumps mtime; new-hash re-extracts.
- **`.git/refs/heads/**` and `.git/HEAD` events** → for the affected repo, walk new commits from current `HEAD` back to `last_indexed_oid`; insert deltas; update cursor.
- **File deletions** → cascade-delete dependent rows via FK.

**Extractor brittleness is intentional MVP scope.** Regex extractors will miss edge cases; the MVP accepts this. `check()` surfaces extraction stats (e.g. "606 session entries indexed, 0 unparseable; 9 repos × ~50k commits indexed, 0 shallow, 0 monorepo-nested skipped") so the user can see where it's failing and report or fix. Phase 2+ may move to AST-based extraction for code; D2's prose + commit corpora stay regex + libgit2.

### 5.4 Concrete queries D2 answers (that nothing else does today)

The differentiation case is most vivid through example queries the MVP must handle. Each example shows the query, the current cost without nibdex, and the nibdex cost.

**Q1 — "When did I last touch the connection-pool config, and what did I decide?"**

Today: open CLAUDE.md, scroll through hundreds of session entries, ctrl-F for the pool keyword, read context for each hit. Or ask the AI client to summarize session history (consumes ~30k input tokens at $3/MTok = $0.09 per ask, repeated each session).

With nibdex: `find_session(query: "connection pool", limit: 5)` → structured digest in ~200 tokens. Same answer, ~150× cheaper, ~10s faster.

**Q2 — "Which memory entries are relevant to a webhook regression?"**

Today: ls `~/.claude/projects/.../memory/` and read filenames; open candidates; or paste full MEMORY.md (~10 KB) into context and hope for the best.

With nibdex: `find_memory(query: "webhook regression", limit: 5)` → top-5 ranked with bodies. Surfaces relevant `feedback_*` and `reference_*` files directly.

**Q3 — "What did the indexing-strategy design doc decide about path classification?"**

Today: find the doc by name guess, open in editor, ctrl-F to relevant section. Or fall back to a curated-knowledge-base search — which returns 0 hits if no human has authored a slice for that topic (the Moment 2 anecdote from §2.4).

With nibdex: `find_design_doc(query: "path classification decision", limit: 3)` → top section with heading path + body excerpt + cross-refs. Indexes the doc regardless of whether anyone has authored a curated slice for it.

**Q4 — "Show me recent sessions touching the same files I'm editing now."**

Today: not really possible without manual cross-referencing.

With nibdex: `recent_sessions(filter: "files_touched contains 'src/api/handlers.rs'", days: 30)` — surface every entry where that file was edited and its surrounding decisions. This is the killer use case for the `files_touched` JSON column.

**Q5 — "When did I introduce the request-streaming wedge bug, and what did the commit message say?"**

Today: `git log --all --source -S '<symbol>' --oneline | head -20`, then `git show` on each candidate; or scroll session history hoping the prose mentions a SHA; or accept defeat and re-derive via timeline. Typically 4–6 manual commands across two tools.

With nibdex: `find_commit(query: "request-streaming wedge", limit: 5)` → top-5 commits ranked by message match, each returned with hash + summary + body + files_changed + the cross-referenced session entries that discussed the same files in the same week. Bridges code-level commit data to prose-level decision history at retrieval time — this is the cross-corpus unlock D2 enables that no per-corpus tool (`git log`, `ripgrep`, curated KB) can match without an AI synthesis step.

**Q6 — "What changed in `src/api/handlers.rs` last month, and why?"**

Today: `git log --since=30.days --oneline -- src/api/handlers.rs` for the *what*; then ask the AI client to read session log + commit bodies for the *why*, burning ~10–30k input tokens per ask.

With nibdex: `recent_commits(filter: "files_changed contains 'src/api/handlers.rs'", days: 30)` → structured list with commit messages (the *why* is usually right there in conventional-commit-style bodies), automatically joined to surrounding `recent_sessions(filter: same)` entries. ~300 tokens total digest. The *what* and the *why* surface in one round-trip.

These six queries plus `check()` exercise the entire MVP tool surface. They are the eval suite for D2 + D0 gate clearance (§8.2).

### 5.5 Internal metrics & instrumentation (the *how*)

Internal instrumentation is MVP-required, not Phase 2. The reasoning is in §8.4 — without per-query measurement, the cost-moat thesis (§3) is unfalsifiable. This section covers the instrumentation primitive; §8.4 covers what the measurements *mean*.

**Shape.** Each MCP tool call inside the daemon emits one structured event to an opt-in JSONL sink:

```jsonc
{
  "ts": "2026-05-25T19:14:08.412Z",
  "tool": "find_session",
  "query": "connection pool",           // verbatim; user has filesystem access anyway
  "params": { "limit": 5 },
  "wall_ms": 23,                        // total handler wall time
  "stages_ms": {                        // per-pipeline-stage breakdown
    "fts5_query": 4,
    "rank": 1,
    "join": 6,
    "shape_response": 12
  },
  "candidate_count": { "fts5": 47, "after_rank": 5 },
  "result_token_estimate": 211,         // tokens we returned to the AI client
  "cache_hit": false,                   // Phase 3 only; null in MVP
  "daemon_uptime_s": 4128
}
```

**Configuration.** Three sink modes via CLI flag:

- `--metrics-sink off` (default in stdio mode for Claude Code sessions — zero overhead).
- `--metrics-sink stdout` (human-readable; useful during hand-testing).
- `--metrics-sink jsonl:<path>` (machine-readable; appended; rotated on daemon restart). Recommended default for daemon mode.

**Schema policy.** The event schema is documented in `DESIGN.md` and versioned (`schema_version: 1` field included). Additive changes (new fields) bump nothing; breaking changes bump the version. Downstream consumers (the eval harness, future per-commit regression-delta tooling) read the schema, not the source.

**Adjacent surfaces.** `check()` (§5.1) reports point-in-time totals derived from the same event stream when daemon-resident: total events emitted, p50/p95 per-tool wall times, parse-failure counts per extractor kind, indexed_repos cursor lag.

**What this is NOT.** No telemetry leaves the machine. No phone-home. No anonymized aggregate sent to a project server. The JSONL sink writes to a local path the user controls; users who want zero observability disable the sink and the daemon emits nothing. Same posture as the cache substrate — local-first, user-sovereign.

### 5.6 Voluntary metrics sharing (opt-in, IP-safe, user-initiated)

The local-first posture above is absolute and is *not* relaxed by this section: nibdex never transmits anything. But there is real value — to nibdex's improvement — in being able to learn from how the tool actually performs on other people's workspaces. To make that possible *without breaking local-first*, nibdex intends to offer a **voluntary, transparent metrics payload** the user can choose to hand over. It is a planned feature, not yet built; this section is the binding charter for what it must be when it ships.

Five hard constraints, all required:

1. **IP-safe AND free of sensitive data (absolute).** The payload carries **no intellectual property and no sensitive data of any kind** — no source, no file contents, no workspace IP, and no secrets, credentials, personal data (e.g. author names/emails), or business-confidential information. Anything that could identify a private codebase *or* expose sensitive data is reduced to non-identifying shape: verbatim queries → query *shape* features (term count, length bucket, had-operators, matched-zero, broadened); error text → error *kind* only; absolute paths and project names → anonymized ordinals (`repo_0..repo_n`) and corpus-size buckets; author identities are dropped entirely. What survives is content-free, non-sensitive signal: stage timings, token/dollar counterfactuals (nibdex's own retrieval cost, not any organization's finances), outcome distribution, per-tool call frequency, daemon uptime. When in doubt about a field, it is **excluded** — the default is omission, not inclusion.
2. **User-initiated.** The payload is produced only by an explicit user action (a `nibdex metrics-export` command the user runs). nibdex never generates or sends it on its own; there is no background path from internal metrics to an outbound artifact.
3. **Fully transparent — the user sees exactly what it contains.** The flow is *generate candidate → user inspects the complete payload → user approves → only then is it shareable.* The approval step is load-bearing, not optional.
4. **Human-readable + self-describing.** The payload is not an opaque blob. It is readable text in which every field is labeled with what it measures, its units, and its confidence (`estimated` vs `measured`, per §11), so neither the user nor the project can be misled about what a number claims.
5. **Honest.** It is a truthful valuation of nibdex's real operation, not a flattering selection — losses and net-negative results survive into the payload, not just wins. All-or-nothing over a window; no cherry-picking which rows export.

nibdex still makes **zero network connections**; "sharing" means the user hands over a local file they have read and approved, never a transmission nibdex performs. This sharing remains valuable specifically because the project intends nibdex to help organizations make their AI-assistant spend go further and faster — and improving the tool against real, varied workloads is how that intent is honestly verified — but the value is *never* allowed to override constraints 1–5.

---

## 6. Non-goals

This list exists to prevent scope drift. Anything below is **out of scope** for the indefinite future. Including these in proposals is a red flag.

- **Foundation model training, hosting, or fine-tuning.** This project consumes models; it does not produce them.
- **Cloud-hosted SaaS version.** The whole point is local-first; a hosted version would invert the mission.
- **Multi-tenant features.** Tool is single-user, single-workspace.
- **Authentication, authorization, RBAC.** It is a local tool. The user has filesystem access; that is the authorization model.
- **Real-time collaboration.** No CRDTs, no sync, no presence.
- **A graphical user interface.** The MCP client is the UI.
- **Wiki-style content authoring tools.** The tool reads; it does not provide editing surfaces.
- **General-purpose vector database functionality.** Semantic search is a Phase 3 fallback for retrieval, not a database we offer.
- **General-purpose graph database functionality.** The graph is derived from workspace signals for the use case at hand; it is not a Neo4j replacement.
- **Compatibility with the reference tool's data formats.** Structural divergence is the point. Migration tooling for its users to import their slices is reasonable; format compatibility is not.

---

## 7. Tech stack

### 7.1 Locked decisions

| Concern | Choice | Rationale |
|---|---|---|
| Implementation language | **Rust** | 3 yrs author experience; single-binary distribution; ecosystem fit (ripgrep, tree-sitter, MCP) |
| Edition | Rust 2024 | Use latest stable; matches author's other Rust services |
| Unsafe-code policy | `#![forbid(unsafe_code)]` at crate root | Compile-time enforcement of zero new unsafe surface in our code. Dependencies retain their own (vetted) unsafe; dep tree footprint measured via `cargo geiger` and disclosed in DESIGN.md. The forecast for D2's scope is that no unsafe will ever be needed — all realistic candidates (mmap, SIMD, FFI) live in upstream crates like `rusqlite`, `tree-sitter`, `rmcp`, `ripgrep`, `git2`. If a real crux later forces unsafe, the decision (alternative crate / alternative design / feature-flagged carve-out) is made then, not pre-designed now. |
| Primary storage | **SQLite** | Boring, embedded, zero-config, FTS5 builtin; matches "runs on a laptop" constraint |
| Storage API | `sqlx` async (likely) or `rusqlite` sync | Bias toward `sqlx` because D0 daemon mode + `notify` event loop benefits from an async substrate. Final pick at MVP-start once `rmcp` transport choice is concrete. |
| Search backend | `ripgrep` (via `grep` crate) + SQLite FTS5 hybrid | Empirically validated retrieval shape; both free, both fast |
| Code parsing | `tree-sitter` | Universal, multi-language, mature |
| **Git extraction** | **`git2` crate (libgit2 binding)** | **Mature MIT-licensed binding to libgit2 — used by `cargo` itself in production. Covers commit walk, parent traversal, tree-diff for `files_changed`, ref enumeration. No `git` subprocess dependency.** |
| **File-watcher** | **`notify` crate** | **Cross-platform abstraction over inotify (Linux) / FSEvents (macOS) / ReadDirectoryChangesW (Windows). Powers D0 daemon incremental indexing. Mature, BSD-licensed.** |
| MCP transport | stdio (Claude Code default) **AND HTTP for cross-session daemon** | Both locked, both shipped in MVP. stdio gives per-session warmness for free via rmcp; HTTP daemon (opt-in via `--http :PORT` flag) gives cross-session warmth + file-watcher-driven background indexing. See §10 O6 (CLOSED MVP-required). |
| MCP framework | `rmcp` crate (preferred) or hand-rolled JSON-RPC | Decide at MVP-start based on crate maturity audit |
| Evaluation harness | Python eval harness (retained) | Already exists; no need to rewrite; eval-only, not production code |

### 7.2 Deferred decisions

| Concern | Options on the table | Decide by |
|---|---|---|
| Embedding model | bge-small-en-v1.5 / nomic-embed-text-v1.5 / others | Phase 3 kickoff |
| Embedding runtime | candle-rs / ort (ONNX Runtime) | Phase 3 kickoff |
| PG harness driver | sqlx-postgres / tokio-postgres | Phase 3 kickoff |
| Repo host | GitHub / Codeberg / self-host | Before first public release |
| Project name | TBD (let it emerge) | Before first public release |

License is no longer deferred — see §10 O3 (MIT, locked).

---

## 8. Comparison & evaluation plan

The eval harness is the continuous-eval discipline. The curated-corpus subscription is retained for a dual-running window before any cancellation decision.

### 8.1 Metrics

- **Distinct-answer-files in top-5** (prior benchmark: ripgrep-only strongest, curated-corpus baseline weakest)
- **$/query Anthropic cost** (input token consumption of tool output × current pricing)
- **Latency** p50 / p95 (current ripgrep-only: 944 ms / 21 s — the p95 is rglob cold-start; daemon mode is the fix)
- **Coverage as workspace grows** (the curated-corpus tool scales with slice authoring; this tool scales with the workspace itself)
- **Cache hit rate** (Phase 3+ only; gates Phase 3 commit)

### 8.2 Phase gates

| Phase | Ships only if... |
|---|---|
| **MVP** | ≥ ripgrep-only baseline AND > the curated-corpus baseline on the same benchmark AND latency p50 < 1 s warm |
| **Phase 2** | Adds ≥3/50 over MVP from graph + provenance alone (without cache) |
| **Phase 3** | Cache hit rate > 20% on real session traffic over a 2-week window AND semantic fallback adds ≥1/50 on queries lexical+graph misses |

Failure to clear a gate triggers a regroup, not a forced ship.

### 8.3 Baseline comparison protocol

- Run the eval harness with both backends (the curated-corpus baseline, and nibdex via MCP) over the same locked query suite during the comparison window.
- Track delta in all four metrics over time.
- The curated-corpus tool is the benchmark; if nibdex isn't winning on at least coverage + $/query by the end of the comparison window, the design is wrong and we regroup before continuing.

### 8.4 Cost-savings measurement framework (the *what we measure and why*)

Without this framework, the §3 thesis ("reduce drain on AI costs") is unfalsifiable. The §1.1 mission ("reserving paid AI tokens for the genuinely irreplaceable step: synthesis") is unverifiable. The §2.4 Moment 1 + Moment 2 anecdotes stay as estimates instead of becoming measurements. The §13.2 dogfood window produces *impressions* rather than *evidence*. This section closes that gap. It is **MVP-required**, MVP-scope-lock-compatible (measurement infrastructure, not a D-feature; §9.1 doesn't trigger).

**Two-layer model:**

**Layer 1 — direct measurement (per-query ledger).** Every MCP tool call records what *did* happen: `result_token_estimate` (tokens nibdex returned to the AI client), `wall_ms` (round-trip time), and the structured event from §5.5 with full pipeline-stage breakdown. This layer is purely factual; it ships with the §5.5 instrumentation primitive at MVP.

**Layer 2 — counterfactual estimation.** For each query, nibdex records what *would have happened* without nibdex — the "AI-only counterfactual cost." Three approaches considered; **chosen: (C) harness-calibrated static estimation**.

- (A) *Static estimation alone* — each tool carries a hardcoded counterfactual model (e.g., `find_session(q)` returns ~200 tokens; counterfactual = "Claude reads CLAUDE.md = ~67k tokens at 3 KB/Ktok input rate"). Cheap but un-calibrated; assumptions drift silently.
- (B) *Live sampled comparison* — every Nth query also routes through a no-nibdex counterfactual path (Claude + raw rg, no nibdex digest). Real measurement, rigorous, but adds latency + ongoing API cost. Phase 2 candidate; defer at MVP.
- (C) *Harness-calibrated static estimation* — **CHOSEN.** The retrieval-eval harness establishes actual per-backend costs on a locked query suite; those anchors feed a per-tool counterfactual model maintained as a TOML config file alongside the binary. Re-calibration is a deliberate config-edit event, not a silent drift, and each anchor carries a `calibration_confidence` tag so a reader never mistakes an estimate for a measurement.

**Per-query savings event** (emitted alongside the §5.5 instrumentation event when Layer 2 is enabled):

```jsonc
{
  "ts": "2026-05-25T19:14:08.412Z",
  "tool": "find_session",
  "result_tokens": 211,                 // measured (Layer 1)
  "counterfactual_tokens_p50": 67200,   // estimated from calibration model
  "counterfactual_tokens_p95": 91000,
  "tokens_saved_p50": 66989,
  "dollars_saved_p50_usd": 0.20,        // current Anthropic input-token rate
  "wall_ms": 23,                        // measured
  "counterfactual_wall_ms_p50": 8500,   // ~human attention budget estimated from Moment 1
  "calibration_model_version": "v0.1-2026-05-25",
  "calibration_confidence": "estimated"  // 'measured' once Layer (B) sampling is wired
}
```

**Cumulative ledger.** Daemon maintains a rolling aggregate (rolling 1d / 7d / 30d) computed from the JSONL stream: total queries served, total `result_tokens` returned, total `counterfactual_tokens_p50` saved, total `dollars_saved_p50_usd`, broken down per tool. `check()` surfaces the current rolling totals as a sub-block (shape shown; figures below are from a real window, not illustrative):

```
$ nibdex check
...
Cost-savings ledger (calibration model v0.2-2026-06-05):
  Queries served:                 47
  Tokens returned by nibdex:      83,847   (measured)
  Counterfactual tokens (p50):   ~824K     (estimated)
  Tokens saved (p50):            ~740K     (estimated)
  Dollars saved (p50):           ~$2.22    (estimated)
```

**First real dogfood evidence (scrubbed export, 2026-06).** A ~5-day real-usage window produced a scrubbed, IP-safe `metrics-export` (§5.6): **47 queries served, ~740K tokens saved** (`calibration_confidence: estimated`) at **~$2.22**. Composition matters for honesty — the four corpus-agnostic tools that work on any git repo (`find_code`, `find_design_doc`, `find_commit`, `find_memory`) accounted for **~279K saved across 26 queries**; the remainder came mostly from `find_session`, whose coverage is CLAUDE.md-format-specific today (§9.9), so the universal-tool figure is the more transferable one. `tokens_returned` and `candidate_count` are measured; `tokens_saved` is estimated from the calibration anchors until Layer 2-B live sampling (approach B above) ships.

**Ground-truth honesty.** The framework reports `calibration_confidence: 'estimated'` until live-sampling (Layer 2-B) ships. The ledger output explicitly tags estimates so a reader never mistakes them for measurements. The §2.4 Moment 1 + Moment 2 anecdotes upgrade from "~8–12k tokens" prose-estimates to ledger-tracked measurements once the daemon is running with metrics on — that upgrade is the empirical case the §13.1 Gate 2 needs.

**What this is NOT.** Not a marketing dashboard. Not exported as part of an MCP tool surface (no `cost_savings_summary` tool — would be self-aggrandizing and adds zero value to the AI client). The ledger is for the author + future-reviewers + the public-flip pitch. It lives behind `check()` and the JSONL sink, not behind a user-facing API.

**Cross-references.** Feeds §3 thesis verification; feeds §8.2 phase gates ($/query and dollars-saved-rate); feeds §13.1 Gate 2 ("measurably address at least one crux"); upgrades §2.4 anecdote framings; constrained by §6 non-goals (no telemetry leaves the machine).

---

## 9. Risks & failure modes

Listed in rough order of likelihood × impact.

### 9.1 Scope drift ("Project That's Scope Got Too Big So It Died")

- **Risk:** The differentiator list contains 10 entries (D0 + D1–D9). The temptation to build them all in parallel is real. The design has already absorbed one scope expansion (git corpus + daemon mode); the door must close behind it.
- **Mitigation:** Phase tags are scope discipline. Each phase has explicit gates (Section 8.2). The non-goals list (Section 6) is the defended boundary. **MVP scope LOCK:** further D-additions to MVP (Phase 1a) require crossing a named bar — *a measured retrieval-quality gap in the eval harness that no existing D0–D9 differentiator can close, with the gap reproducible across ≥3 distinct queries*. Aesthetic, exploratory, or "wouldn't it be nice" additions do not cross this bar. Phase 1b candidates (D1, D3, plus the O9 dev-environment-corpora list) follow MVP shipping, not parallel to it.

### 9.2 Session-history bet fails to pay off

- **Risk:** If AI doesn't actually benefit from indexed session-history more than from raw ripgrep over CLAUDE.md, D2 (the most novel MVP differentiator) collapses.
- **Mitigation:** the eval harness surfaces this within MVP build. Run D2-on vs D2-off A/B in the harness; if delta is < 2/50 distinct-files, drop D2 to Phase 2 and reconsider.

### 9.3 Coverage illusion

- **Risk:** Full-workspace lexical might surface high volumes of low-relevance matches that dilute top-5 quality.
- **Mitigation:** File-agg + path-class boost (already validated in the eval harness) prevents low-signal results from displacing canonical answers. Watch the metric, not just the count.

### 9.4 Cache invalidation correctness

- **Risk:** Stale cache entries served confidently. Cache invalidation is the hardest CS problem; we will get it wrong somewhere.
- **Mitigation:** Pessimistic invalidation (any source touched → invalidate all dependents), explicit freshness signals in tool output (`derived_at`, `sources_unchanged_since`), and Phase 3 hit-rate metric is reported so degradation is visible.

### 9.5 Solo-dev sustainability

- **Risk:** Author's health, time, or financial situation changes; project stalls mid-phase.
- **Mitigation:** Phase boundaries are natural pause points; each phase ships a working, tested, useful artifact. No phase requires another phase to be valuable. MVP alone is shippable.

### 9.6 Anthropic dependency

- **Risk:** Claude pricing changes, API changes, or MCP spec changes break the integration.
- **Mitigation:** MCP is increasingly multi-vendor (OpenAI, Continue, Zed, Cline, OpenWebUI). The architecture stays client-agnostic; if Claude changes, swap clients.

### 9.7 Tree-sitter grammar fragmentation

- **Risk:** Phase 2's code-ref extraction depends on tree-sitter grammars, which vary in quality across languages and break occasionally on edge cases.
- **Mitigation:** Limit Phase 2 to a curated grammar set (Rust, Python, TypeScript, SQL initially); fall back to regex-based file-mention extraction for unsupported languages. Graph quality degrades gracefully rather than failing.

### 9.8 Distribution friction killing adoption

- **Risk:** Even a great tool that takes 30 minutes to install is invisible.
- **Mitigation:** `cargo install <name>` working from day-one of public release. Homebrew tap and pre-built binary release follow. Single MCP config snippet in README sufficient to wire to Claude Code.

### 9.9 Session-history format is author-specific

- **Risk:** The author's CLAUDE.md session-history convention (`### #NNN: <summary>` etc.) is not standard. Other Claude Code users may have completely different layouts, no session history at all, or use external tracking (Linear, Notion). The regex extractors won't fire on their corpora; D2 silently produces empty session_entries for them.
- **Mitigation:** v0.1 ships with the author's format as the only supported extractor. README is explicit that "session history" indexing assumes CLAUDE.md convention X. If adoption signals other shapes matter, a Phase 1b extractor-plugin model can absorb them. The MVP defends a narrow but real use case rather than a generic empty surface.

### 9.10 Regex extractor brittleness on prose

- **Risk:** Unstructured session entries, design docs with idiosyncratic heading conventions, memory files with malformed frontmatter — any of these produces silent partial extraction.
- **Mitigation:** `check()` reports per-source-kind parse stats; the MVP surfaces failures rather than hiding them. Author dogfooding (§13.2) catches the common failure modes before public flip. Edge-case fixes are easy if visible.

### 9.11 Eval-harness regression suite drift

- **Risk:** The 10-query suite was authored against ripgrep/rerank/curated-KB backends. The query mix may not exercise D2's structured-history strengths well, leading to false negatives at the §8.2 phase gate.
- **Mitigation:** Extend the suite with the six Q1–Q6 queries from §5.4 specifically as D2-targeted scenarios (Q1–Q4 cover session/memory/design-doc/files_touched; Q5–Q6 cover git-commit and cross-corpus *why-did-this-change*). Document this in the harness — separate the "fleet baseline" 10-query suite from the "D2-targeted" 6-query suite so neither contaminates the other's interpretation. The phase gate requires beating ripgrep-only on the *combined* 16-query suite.

### 9.12 Git corpus shape variance

- **Risk:** Real-world repos vary wildly: shallow clones (CI checkouts), workspaces without `.git` at all (vendored snapshots, downloaded tarballs), monorepo subtrees with foreign histories, very-large histories (Linux kernel = ~1M commits — libgit2 walk is fast but not free), repos with binary-noise commit messages (generated commits, "wip" / "asdf"), or histories with surprising structures (orphan branches, grafts, replaced objects). Naïve indexer assumptions break on any of these.
- **Mitigation:** v0.1 indexes shallow clones gracefully — `is_shallow` flag in `indexed_repos`, `check()` reports the state honestly so the user knows what is and isn't reachable. Workspaces without `.git` simply produce zero `commit_entries` rows; no crash. Monorepo nested `.git` dirs are skipped by default (configurable via a `nested_repos_mode = include | skip | warn` setting). Per-repo `max_commits_per_repo` cap (default 50,000, configurable) prevents pathological scans on kernel-sized repos. Noise filtering is *not* attempted in MVP — junk commits go into the index alongside good ones; if the FTS retrieval surfaces them, that's a precision problem we address in Phase 2 (path-class boost, message-length filters), not now.

---

## 10. Open decisions

These are explicit pending items. Each is captured here so it doesn't get lost.

| ID | Question | Notes / leaning |
|---|---|---|
| **O1** | MVP scope final cut: D1 only / D1+D2 / D1+D2+D3 | **CLOSED: D2 alone first.** Stakes the differentiation flag publicly. D1 and D3 follow as Phase 1b. **Extended:** D2's scope broadened to include git history as a fourth indexable corpus; D0 (always-on daemon) added as MVP-required. See §4 for rationale. **Scope LOCKED** — see §9.1 for the bar required to expand again. |
| **O2** | Project name | **CLOSED: nibdex.** Coined portmanteau (pen-nib + index-dex). Cleared a 4-round namespace audit across crates.io / npm / pypi / GitHub. Earlier candidates ruled out: cairn (clash with cairn-dev/cairn AI tool), scree (clash with crates.io 2021 dormant crate), ramora (namespace adjacency to dipanshu-tiwari/Ramora). |
| **O3** | License (MIT vs Apache 2.0) | **CLOSED: MIT.** Apache 2.0's only meaningful upside (explicit patent grant + NOTICE requirements) is low-value for retrieval tooling; MIT's lower-friction adoption (dominant Rust crate license) matters more. SPDX header in source files. If a contributor later brings patented technique, re-evaluate at that point. |
| **O4** | Repo location final | Likely GitHub under personal account; decide before public release. Initial development in **private** repo; flip to public only after §13 publication prereqs all pass. |
| **O5** | MCP framework: `rmcp` crate vs hand-rolled | Audit `rmcp` maturity at MVP-start; default to it if production-ready |
| **O6** | Daemon mode for warm latency | **CLOSED: LOCKED MVP-required.** Two parts: **(a) per-session warmness** via rmcp stdio — effectively free, since Claude Code keeps the spawned MCP process alive for the entire session; cold-start is amortized over a session, not per query. **(b) cross-session warmness + background incremental indexing** via HTTP transport + `notify` file-watcher — opt-in flag for users who want the indexed corpus kept fresh between Claude Code invocations. The "always-available helper" framing (§3 thesis update) makes daemon-mode the default expectation; without it, the tool is a per-query cold-start hit and fails the latency gate at §8.1. Implementation listed in §7.1 (sqlx async substrate likely; `notify` and `git2` crates locked). |
| **O7** | Tree-sitter grammar set for Phase 2 | Rust, Python, TypeScript, SQL initially. Markdown handled separately. |
| **O8** | How to handle the user's CLAUDE.md / memory directory as an indexable corpus | Probably first-class indexing target, but raises a privacy question for users who don't want personal notes indexed by default — opt-in flag? |
| **O9** | Other dev-environment corpora beyond the four MVP kinds (files / session / git / memory + design docs) — shell history, package manifests, editor LSP state, browser history, etc. | **High-bar gate.** Defaults to defer. The four shipped MVP corpora cover the irreducible "keystone resources" set. Each O9 candidate (shell history `~/.zsh_history` / `~/.bash_history`, package manifests `Cargo.toml` / `package.json` / `pyproject.toml`, editor LSP state, browser history) requires an empirical case before Phase 1b consideration: a named recurring query class the MVP cannot answer that the candidate would cleanly address. Privacy + portability concerns weigh heavily (shell history may leak secrets; browser history is sensitive; LSP state is per-editor). Filing this as a single decision rather than nine prevents one-by-one scope creep. |

---

## 11. What this document is NOT yet

To prevent over-commitment:

- This is **not** an implementation plan. It is a design doc. The MVP work plan comes after this is reviewed and refined.
- This is **not** a final API specification. Tool names and schemas in Section 5.1 are sketches subject to refinement during the MVP build.
- This is **not** a commitment to ship a public release on any timeline. The phasing approach explicitly allows pausing between phases.
- This is **not** a competitor pitch against the curated-corpus tool. It is the benchmark and the trial that established the empirical foundation; the subscription continues during the comparison window.

---

## 12. Next steps

O1 + O3 closed; O6 closed + O9 filed. Remaining sequence:

1. **Audit the `rmcp` crate** + the tree-sitter / `git2` / `notify` / candle-rs / ort ecosystems for production-readiness (research session, ~2 hours). Closes O5.
2. **Sketch the first SQLite schema migration** as a concrete artifact — six tables per §5.2 (documents + session_entries + commit_entries + indexed_repos + memory_entries + design_doc_sections) plus FTS5 virtual table. Sibling file or appendix to this doc.
3. **Extend the eval harness** to add an nibdex backend slot so the new tool can be plugged in for eval the moment its first MCP response returns. The harness's locked query suite is the inherited regression bedrock; reuse it as nibdex's commit-1 eval harness. This is the inverted-development DNA — every commit gets empirically scored against real workload, no commit ships on faith.
4. **Decide repo location + scaffold the private repo** with README + this design doc (trimmed) + LICENSE (MIT) + `Cargo.toml` with `#![forbid(unsafe_code)]`. No business logic yet. Closes O4 for the private-development phase.
5. **Build D2 + D0 MVP** against the locked scope. Inverted-development discipline: real workload through cheapest pipeline, identify the first crux, solve only that, repeat.
6. **Private dogfood window** (3–7 days, see §13) once the daemon returns its first useful response. Use it against real workspace work; capture cruxes as they surface — particularly whether the git corpus addition delivers the predicted commit-history-archaeology lift.
7. **Complete §13 publication prerequisites**; flip private → public only when all gates pass.
8. **Phase 1b (D1 + D3) follows the public flip**, sequenced to land while open-source momentum is fresh. O9 corpora (shell history / manifests / etc.) only if a named bar-crossing case emerges from dogfood.

### 12.1 Build methodology note — inverted development, compressed

The build approach is the inverted-development methodology: simulate end-state workload first, run against cheapest pipeline, identify the first actually-broken crux, solve just that, repeat.

This MVP compresses the methodology's standard "observation phase" because runway pressure is a real constraint. Compression is justified by two captured-not-predicted cruxes already in hand:

- **Moment 1 (§2.4)** — toolchain archaeology cost ~8–12k tokens of probing to find an in-tree convention.
- **Moment 2 (§2.4)** — a curated-knowledge-base search returned zero relevant hits for a freshly-created in-session design doc, recoverable only via conversation context.

These are not design predictions; they are real engineering scenarios surfaced during work. Using them as the empirical seed (instead of running D2 for a week to surface fresh cruxes) is consistent with inverted development as long as the cruxes were observed — which they were. The eval harness (item 3 above) prevents this compression from sliding into design-up-front by enforcing empirical scoring on every commit.

---

## 13. Publication readiness (retrospective)

nibdex is an early, admittedly immature project, and this section doesn't claim otherwise. The goal before making it public wasn't a finished product — it was an *honest* v0.1: what's claimed is real, what's rough is disclosed, and there's nothing misleading for the first outside reader to trip on. Plenty is still unfinished (see LIMITATIONS.md and the §4 roadmap), and the numbers here are early and small. The intent is to grow this into something genuinely solid over time, in the open. The basic diligence done before publishing is summarized below and tracked in more detail in the project's internal checklist.

### 13.1 The checks

1. **License.** MIT, single `LICENSE` at repo root, SPDX header (`// SPDX-License-Identifier: MIT`) in every source file.
2. **A real, honestly-measured reason to exist.** The multi-corpus join (D2: session + git + memory + design docs as structured corpora) plus the always-on daemon (D0) work end-to-end, and the value claim is **measured via the §8.4 cost-savings framework** and honestly tagged — estimates are labeled as estimates, not dressed up as measurements.
3. **Documented limitations + roadmap.** README + LIMITATIONS.md are explicit about what works, what doesn't, and why the current limits are known-or-intentional rather than hidden; the §6 non-goals and §4 roadmap carry into the public docs.
4. **Readable documentation.** README (a working install and a first useful query without a fight), LICENSE, this DESIGN.md (trimmed), EXAMPLES.md (concrete queries with real output), LIMITATIONS.md.
5. **Basic code hygiene + IP cleanliness.** `#![forbid(unsafe_code)]` in nibdex's own code; clean `cargo audit` and `cargo deny check` (permissive licenses only); `cargo clippy --deny warnings`; an independent implementation (only ever referencing the benchmark tool's public CLI/MCP surface, never its source); and no host-environment leakage. On unsafe, the honest framing is *"no unsafe in nibdex's own code; the dependency tree's unsafe was audited via `cargo geiger` and lives entirely in vetted upstream — C FFI (`libsqlite3-sys`/`git2`), the `tokio` runtime, and SIMD/byte crates (`memchr`, `bytes`)"* — not a misleading blanket "no unsafe."

### 13.2 Private dogfood window

Before the public flip, the tool is used against real workspace work for several days. If something bites — wrong results, a common query it misses, install friction, a doc gap, a surprising failure — it gets fixed or written down first. The point isn't to prove it's finished (it isn't); it's to make sure the core is trustworthy enough to be worth someone else's time.

### 13.3 Publication checklist

These checks are tracked in a simple checkbox artifact, so the bar is marked off once rather than re-derived each session.

---

## 14. How it was built — the first-week sketch (retrospective)

This is the original first-week build plan, kept as-written. A reader — a code reviewer, or another resource-constrained dev sizing up the project — learns more from watching a design touch the ground than from a polished after-the-fact summary. And it is honestly *not* the whole story: the plan met reality and diverged after the MVP. The D1 source-indexing pivot, the git2/cross-platform migration, and the metrics/relocator work all came later. This public repository begins from a squashed initial release, so those later chapters live in the project's development record rather than in these commits — this section is the retrospective that stands in for them. That divergence is the point — this was built and used, then reshaped by what the use revealed, not spec'd once and frozen.

### 14.1 Day 1 — scaffold and decisions

- Create private GitHub repo (`nibdex` — see §10 O2).
- `cargo new --bin <name>` → minimal Cargo.toml with `edition = "2024"`.
- Add `#![forbid(unsafe_code)]` at `src/main.rs`.
- Add `LICENSE` (MIT).
- Copy in this design doc as `docs/DESIGN.md` (trimmed: drop internal-workspace anecdote framing, keep architecture + tenets + non-goals).
- Stub `README.md` with the thesis sentence + "WIP, not yet public" note.
- Audit `rmcp` crate (§10 O5): `cargo doc --open` on it, read examples, decide rmcp vs hand-rolled JSON-RPC. Default to rmcp unless a blocker surfaces.
- First commit. `cargo build` should succeed on an empty binary.

### 14.2 Day 2 — schema and indexer skeleton

- Add `sqlx` (async, preferred) or `rusqlite` (sync) — pick based on rmcp transport choice from Day 1; lean toward `sqlx` since D0 daemon mode wants an async substrate alongside the `notify` event loop.
- Implement the six-table schema (§5.2): `documents` + `session_entries` + `commit_entries` + `indexed_repos` + `memory_entries` + `design_doc_sections` + the `search_index` FTS5 virtual table. As a `migrations/` directory if sqlx, or inline `CREATE TABLE` if rusqlite.
- Write `Indexer::full_scan` for `documents` only — just walk the three file-target directories, hash files, insert rows. No extraction yet. Repos can wait until Day 4.
- Test on the author's actual CLAUDE.md + memory dir + docs/design — `cargo run -- index` should populate `documents` with ~50 rows in under 5s.
- Smoke test: `sqlite3 nibdex.db 'SELECT kind, COUNT(*) FROM documents GROUP BY kind'` shows the expected breakdown.

### 14.3 Day 3 — session-history extractor

- Implement session-entry regex extractor against CLAUDE.md.
- Write the row-population logic for `session_entries`: parse `### #NNN: <summary>` headings, capture body until next session heading or EOF.
- Extract `files_touched` (paths matching workspace-rooted file existence), `todos_mentioned` (`#NNN` patterns in TODO range), `decisions_made` (heuristic on `CLOSED`/`SHIPPED`/`LOCKED`/`FILED` keywords in proximity).
- Populate `search_index` FTS5 rows for each session entry.
- Smoke test: `find_session(query: "<recent topic>")` returns the right entries.

### 14.4 Day 4 — git-commits extractor

- Add `git2` crate to `Cargo.toml`.
- Implement `Indexer::discover_repos`: find every `.git` dir under `$WORKSPACE` to configurable depth (default 3); insert `indexed_repos` rows with `last_indexed_oid=NULL` for first-time encounters.
- Implement `Indexer::extract_commits(repo_path)`: libgit2 HEAD-ancestry walk, stop at `last_indexed_oid` for incremental; per-commit row population with hash, parent hashes, author/committer identity + timestamps, message split (summary + body), `files_changed` via `Tree::diff_to_tree(parent)` name-only enumeration, `branch_refs` snapshot.
- Apply `max_commits_per_repo` cap (default 50,000); set `is_shallow` flag if `Repository::is_shallow()` returns true.
- Populate `search_index` FTS5 rows for each commit (body = `message_summary || ' ' || message_body || ' ' || files_changed_paths`).
- Multi-repo smoke test: a workspace with multiple nested service repos — `find_commit(query: "<recent symbol>")` should return commits across the relevant nested repos.

### 14.5 Day 5 — memory and design-doc extractors

- Memory: YAML frontmatter parser via `serde_yaml`; body is post-second-`---`.
- Design docs: markdown AST via `pulldown-cmark` or `comrak`; heading hierarchy → `heading_path` (slash-joined).
- Populate `memory_entries` + `design_doc_sections` + their `search_index` rows.
- Smoke test: all four `find_*` tools (session / commit / memory / design_doc) return reasonable results against real data.

### 14.6 Day 6 — MCP server surface + daemon mode

- Wire `rmcp` (or hand-rolled) JSON-RPC server with **stdio transport** (Claude Code default; gives per-session warmness for free).
- Implement the seven MVP tool handlers (§5.1: `recent_sessions`, `find_session`, `recent_commits`, `find_commit`, `find_memory`, `find_design_doc`, `check()`) as thin wrappers over the SQL queries.
- Implement `check()` — re-walks directories + repos, compares hashes + `last_indexed_oid`, reports per-corpus stats, repos-skipped reasons, shallow flags, daemon uptime.
- Wire `notify` file-watcher for **D0 daemon-mode incremental indexing** — debounce 500ms; coalesce bursts; handle filesystem events on CLAUDE.md / memory dir / design-docs / `.git/refs/heads/**` and `.git/HEAD` per §5.3.
- Add **HTTP transport** behind `--http :PORT` flag for cross-session daemon mode (opt-in; stdio remains the default for Claude Code).
- Smoke test: stdio invocation from Claude Code session; daemon mode kept alive across multiple Claude Code sessions; file-edit on CLAUDE.md triggers incremental update within 1s; commit on any nested repo triggers `commit_entries` insert within 1s.

### 14.7 Day 7 — instrumentation primitive (§5.5 Layer 1)

- Define the `MetricsSink` trait + three impls: `Disabled` (default), `Stdout`, `JsonlFile(path)`. Selected via `--metrics-sink off|stdout|jsonl:<path>` CLI flag.
- Add `MetricsEvent` struct matching the §5.5 schema; bump `schema_version` to 1.
- Wire emission into each of the seven MCP tool handlers — capture per-stage `Instant::elapsed()` deltas, `result_token_estimate` from response shape, `candidate_count` from query intermediates.
- Smoke test: `--metrics-sink stdout` mode prints one JSONL event per tool call; `--metrics-sink jsonl:/tmp/nibdex.jsonl` writes events without buffering delay > 1s.

### 14.8 Day 8 — cost-savings framework (§8.4 Layer 2)

- Author `calibration.toml` (or equivalent JSON) with per-tool counterfactual cost models seeded from the eval-harness baseline anchors (e.g., `find_session` → counterfactual_tokens_p50 = 67000, sourced from "Claude reads CLAUDE.md ~200 KB at 3 KB/Ktok").
- Wire a `CostLedger` aggregator that reads the §5.5 event stream live and maintains rolling 1d/7d/30d totals in memory (persisted to SQLite on daemon shutdown for survival across restarts).
- Extend `check()` output with the cost-savings ledger sub-block from §8.4.
- Tag every Layer 2 event with `calibration_confidence: "estimated"` until live-sampling lands in Phase 2.
- Smoke test: run a handful of `find_session` / `find_commit` queries from a Claude Code session; `nibdex check` reports a non-zero "Tokens saved (p50)" line with the calibration model version stamped.

### 14.9 Day 9 — eval-harness integration

- Add nibdex as a backend in the eval harness. Spawn the daemon (or one-shot stdio process), send MCP JSON-RPC over stdio, parse responses.
- Wire the eval harness to consume `--metrics-sink jsonl:<path>` output for per-commit regression-delta capture (timings per pipeline stage, candidate counts).
- Run the existing 10-query suite. Capture the baseline numbers.
- Author the 6 D2-targeted queries from §5.4 (Q1–Q6) as additions to the suite — including the new git-shaped Q5 and cross-corpus Q6.
- Run the combined 16-query suite. Run LLM-judge over all three backends (ripgrep-only, the curated-corpus baseline, nibdex). Record results in the eval harness's baselines directory.
- Cross-check: the costs the eval harness measures externally should align with the §8.4 ledger's Layer-1 figures. Any drift surfaces a calibration bug — fix before Day 10.

### 14.10 Day 10 — assess

- Read the eval-harness numbers honestly. Did nibdex clear ≥ ripgrep-only on combined 16-query suite + > the curated-corpus baseline on combined suite + p50 < 1s warm (§8.2 MVP gate)?
- Read the §8.4 ledger honestly. Does the projected daily/weekly savings number look real, or does the calibration model produce numbers that fail a smell test? (E.g., "$1000 saved per week from 50 queries" is implausible; "$5 saved per week from 50 queries" is plausibly the real signal.) Tune calibration assumptions if smell-test fails — *and document the tuning explicitly* so future-me can see the assumption history.
- If both the eval harness + ledger pass: write LIMITATIONS.md + EXAMPLES.md + roadmap; enter §13.2 private dogfood window. The dogfood window then produces *measured* M-anecdote updates, not estimated ones.
- If either fails: identify the first crux that's blocking. Solve only that. Re-run. Repeat. This is exactly the inverted-development pattern from §12.1.

This sequence is a planning artifact, not a commitment. Realistic calendar duration is likely 2–3× the "day" counts above, depending on what the rmcp audit + extractor edge cases + `git2` multi-repo discovery + calibration-model smell-test edge cases surface. The point is that every "day" produces a falsifiable artifact, and the gate at Day 10 is empirical, not vibe.

---

## Appendix A — Glossary

- **MCP** — Model Context Protocol. Open spec published by Anthropic for AI clients to discover and call local tools via JSON-RPC over stdio or HTTP. Multi-vendor adoption increasing through 2025–2026.
- **Curated-corpus baseline** — An existing paid product providing a curated-slice knowledge base with MCP integration; the reference benchmark for this design.
- **Eval harness** — Python simulator built to empirically test retrieval backends against a locked query suite with LLM-judge grading.
- **File-aggregation (file-agg)** — Retrieval scoring pattern: roll per-line lexical hits up to per-file scores by summing or weighting; ranks files by total signal rather than per-line proximity. Empirically validated as a near-optimal prior for prose-shape queries.
- **Path-class boost** — Categorical weighting of matches by their containing directory's semantic class (memory entries / design docs / third-party code) to favor canonical answers in retrieval ranking.
- **Provenance** — Metadata recording which source files / sections a derived record was produced from. Required for correct cache invalidation.
- **Session-history** — The author's accumulated session-by-session work log in CLAUDE.md (currently ~605 entries). Treated as first-class indexable corpus in this design — a genuine differentiator.

---

*End of design doc. **MVP scope LOCKED**: D2 (session history + git commits + memory + design docs) + D0 (always-available daemon with file-watcher incremental indexing) + §5.5 instrumentation primitive + §8.4 cost-savings measurement framework — all MVP-required. O1 (MVP scope) + O3 (license = MIT) closed; O6 (daemon mode = MVP-required) closed + O9 (additional dev-environment corpora = high-bar gate, defer default) filed; O2 (project name = nibdex) closed. Further D-additions require crossing the §9.1 bar; instrumentation/measurement additions are separately bounded by §6 non-goals (no telemetry leaves the machine) and §11 honesty norms (estimates tagged, never silently asserted as measurements). Next: §12 sequence — `rmcp` + `git2` + `notify` maturity audit, six-table schema sketch, eval-harness nibdex backend slot, private repo scaffold, D2+D0 MVP build with §5.5+§8.4 wired in by Day 8, dogfood, §13 publication gates, public flip.*
