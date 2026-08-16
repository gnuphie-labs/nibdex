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

### Query syntax is FTS5, and some punctuation still bites

The `query` parameter on all `find_*` tools is an FTS5 `MATCH` expression. nibdex does **not** pass it through untouched: any whitespace-separated token containing a character FTS5's parser would choke on is wrapped in a phrase literal before the query runs. So `find_commit(query: "fan-out")` searches for the phrase `fan-out` and returns results rather than erroring — the hyphen is not treated as a NOT operator.

Power-user syntax still works, because the wrapping is per-token and leaves FTS5's own grammar alone: `bb8 AND pool`, `"exact phrase"`, and `prefix*` all behave as FTS5 defines them.

**What still bites.** Parentheses and commas are deliberately left unquoted, because they are FTS5 grouping syntax — so a query containing them can still fail:

```
find_code(query: "parse_config(")   →  error: fts5: syntax error near ""
find_code(query: "foo(bar)")        →  error: fts5: syntax error near "foo"
```

Searching for a call site is an ordinary thing to want, so this is a real rough edge. **Workaround:** quote it yourself — `find_code(query: "\"parse_config(\"")` — or drop the punctuation and search `parse_config`.

An FTS5 syntax error surfaces as a tool error whose text names the cause and the fix ("the query is not valid FTS5 MATCH syntax … wrap it in double quotes … This is a malformed query, NOT an empty or broken index"), so a client can tell malformed-query from broken-index. Auto-quoting the paren family is still deliberately not done — `( )` are grouping syntax a power user may mean.

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

### The session corpus is transcript-derived, and machine-specific

`find_session` and `recent_sessions` are backed by the **session→code map** — `Edit`/`Write` actions recovered from your Claude Code transcripts, built by `nibdex index` (see the README). Two consequences: it's only as complete as your retained transcripts (recall starts at first index and grows forward — Claude Code prunes old transcripts, ~30 days by default), and it's inherently machine-specific, so unlike the other corpora it can't be reproduced from a clone.

A third, subtler one: **the index only advances when you run `nibdex index`.** The file watcher does not yet watch the transcript directory, so a long-running daemon keeps serving whatever the last indexing pass captured — sessions since then are not in it. Re-run `nibdex index` (or `nibdex index-sessions --workspace-scoped`, which does only this corpus) to catch up. Watching transcripts live is planned.

And a cost worth knowing about: **each pass re-reads every transcript**, including those belonging to other workspaces on the machine, which are parsed and then discarded by the scope rule. Indexing is additive, so nothing is rewritten — a re-run reports how many edges were new versus already indexed — but the *reading* is repeated. On the author's machine that is ~16 MB and ~50 ms; with a much larger transcript history it will be proportionally slower. Resuming from a stored offset instead of re-reading is a planned improvement, not something the current release does.

**Legacy note.** Earlier nibdex parsed session entries from a specific `## Recent session history` CLAUDE.md shape into a `session_entries` corpus. That extractor still runs but **no query tool reads it** — the transcript map replaced it — and it will be removed in a future release. `check()` lists it under `retired_corpora` when it still holds rows, so a non-zero count there is not a sign of a damaged index. If `find_session` comes back empty, the response itself now tells you which case you are in: `corpus_empty: true` means nothing has been indexed, while `corpus_empty: false` (with `corpus_indexed_through`) means the corpus has content and your query simply missed it.

### `find_code` indexes the working tree at index time; provenance is last-touch at HEAD

The source corpus reads each git-tracked file **from the working tree** when `nibdex index` (or the watcher's on-commit reindex) runs — not the blob at HEAD. So an uncommitted edit that is on disk at index time is searchable, and its chunk still carries `commit_sha` = the commit that last touched that *file* at HEAD, which does not contain the uncommitted lines. `location: "verified"` means "the file has not changed since indexing", not "this content is committed". Read `commit_sha` as file-level last-touch provenance, and treat any hit whose text you cannot find in `git show <commit_sha>:<path>` as working-tree content. Indexing HEAD blobs instead is a design change under consideration.

### The commit corpus keeps commits that history has rewritten

Commits are added by walking HEAD and never removed. After `git commit --amend`, an interactive rebase, or a squash, the *pre-rewrite* commits stay in `commit_entries` alongside their replacements — `recent_commits` and `find_commit` return both, and `total_matched` counts both. This is deliberate for branch deletion (a deleted branch's commits are still history you may want to search) but is a real over-report after a rewrite. There is no `nibdex` command to purge them yet; deleting the db and re-running `nibdex index` is the reset. Source files that leave the git index *are* pruned on the next pass, and files deleted from disk but not yet from git show as `location: "file_missing"` and count under `check().orphans.source_chunks`.

### File-watcher is daemon-only

The `nibdex mcp` stdio transport does **not** spawn the watcher — the process exits at session end. Between sessions, the index reflects whatever the most recent `nibdex index` run captured.

For cross-session warmth + incremental indexing, run `nibdex serve --http 127.0.0.1:<port>` or `nibdex watch` under your OS init system (`launchd` / `systemd`). The daemon shapes are documented in DESIGN §5.3 + §5.4.

### Memory-directory auto-detection is Claude-Code-specific

The default memory-directory resolver encodes the workspace path with Claude Code's convention (`/` and `_` and `.` → `-`). Other MCP-speaking clients with their own memory conventions need `--memory-dir <path>` passed explicitly.

### `bm25` ranking surfaces density, not always relevance

For cross-corpus terms, `find_commit("rustFetch")` ranks the densest occurrence (the canonical fan-out commit) above commits that *fix* or *audit* the same term. This is usually right (the canonical change is the touchstone), but a query like `find_commit("rustFetch wedge fix")` would benefit from phrase or proximity boosts. Filed for dogfood-pattern review, not a current default change.

### Always-on indexing requires `git2`-readable repositories

The git-commits corpus uses `libgit2` (via the `git2` crate). Shallow clones are flagged in `check().shallow_repos`. A repository `git2` cannot *open* at all (e.g. an empty or corrupted `.git` directory) currently fails the whole `nibdex index` run with the git2 error rather than being skipped — remove or repair it and re-run. Git *worktrees* and submodules (whose `.git` is a file, not a directory) are not discovered yet: a workspace made only of worktree checkouts indexes nothing. Both are known limits, not silent drops — the first is loud, the second shows as zero repos in `check()`.

### The IP-domain partition isolates artifacts, not sentences

With `.nibdex-domains.toml` + `--domain`, a per-domain database is guaranteed — by a
build-gating invariant test — to hold no **files, commits, design sections, or
session edits** from another domain's tree, and to withhold **rationale prose from
the point a session first touches another domain's tree onward**. That guarantee is
mechanical and needle-testable.

The withholding is **forward-only**, which is a real limit and not a wording detail:
rationale attached to edits made *before* that first cross point is kept verbatim, so
prose looking ahead to work not yet started ("next I'll wire this into the acme
flow") can still land in this domain's database even though the session later
crossed. Order matters — two sessions with identical content but different edit
order withhold differently. Whole-session retraction would close it and is not done:
one stray read late in a long session would erase hours of legitimate rationale.

What it does **not** do is judge what a sentence is *about*. A commit message or
design note in a domain's own tree can name another domain and is indexed verbatim;
a sentence typed in an otherwise single-domain session, with no tool call touching
the other domain, taints nothing and is admitted. nibdex prevents mechanical
commingling; it is not a semantic censor. The full CAN/CANNOT statement and the
mitigation (context-separation discipline; separate workspaces for strict isolation)
are in [SECURITY.md](../SECURITY.md#separating-ip-domains-multiple-clients-or-employers-on-one-machine).

**Two things a domain database does not contain unless you act.** Workspace-**root**
files belong to no labeled subdir, so no domain indexes them — silently, with no
error. The fix is a convention: put cross-cutting docs in a subdir that is both
labeled *and* its own git repository (the label routes it; the repo makes it a
design-doc discovery anchor). Labeling a root file directly in the config does not
work — discovery filters anchors before it reaches individual files. Separately, the
**memory** corpus is skipped in domain mode unless a domain claims it via `[memory]`;
that claim is an assertion nibdex cannot verify, since it can check a subdir against
the filesystem but not what a memory note is about. Both are documented in
[IP_DOMAINS.md](IP_DOMAINS.md#two-things-that-need-a-convention).

**The ratchet does not fire on domain-less paths.** Files directly in the workspace
root, this workspace's **own** `~/.claude` slug, and its own temp scratch belong to no
domain and do not taint a session; an **unlabeled subdirectory does**, since that is
how an unlabeled client tree looks. Not `~/.claude` or `/tmp` wholesale — those hold
other workspaces' transcripts and client checkouts respectively. The exemption is by
*location*, not content: a cross-client note at your workspace root will not withhold.
This narrowing is measured — the earlier rule withheld 89.5% of reasoning on one
single-domain machine, all false alarms (n=344 edges, one developer's box).

Two finer edges of the withholding, both spelled out in SECURITY.md: the taint
tracking watches **path-bearing tool inputs** (`Edit`/`Write`/`Read`/`Grep`/`Glob`/
`NotebookEdit`), so a `Bash` command, a path-less grep, or a `Task`/sub-agent doing
foreign work does not taint a session; and the ratchet is scoped to a **transcript
file**, so one logical session that resumes into a second `.jsonl` file is not
tainted there by a foreign touch in the first. Both follow from preferring
auditable, mechanical rules over guesswork — and, per the project's threat model,
are graded by plausibility under normal single-developer use.

**Two operational rules:** a per-domain database must be *born* in domain mode (do
not re-index an unpartitioned db with `--domain`), and **narrowing** a domain's
labels requires rebuilding that domain's db (indexing only adds; it never
un-indexes a now-foreign row). Over-redaction — a session's rationale withheld
because it touched another domain's, or an unlabeled subdirectory's, file — is
surfaced as a `rationales_withheld` count, not hidden. (One surprising case: a
workspace-root path is neutral only while the file still *exists*. Relocate a root
tracker and historical sessions that named it start tainting, since nibdex can no
longer tell a deleted file from a deleted directory and fails narrow.)

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
