# nibdex

> **An always-available MCP knowledge tool for the keystone resources every dev environment already has — source code, git history, design docs, memory, and AI session history — surfaced together and kept current, derived from the workspace rather than hand-curated.**

**Status:** pre-1.0, and honestly early. The core works end-to-end on real workspaces — five corpora indexed, seven query tools live, every code hit carrying the git commit that last touched its file — and it's dogfooded daily on the author's own projects. It's also young: the usage numbers are small, several rough edges are written down in [`docs/LIMITATIONS.md`](docs/LIMITATIONS.md), and the [roadmap](#roadmap) is still most of the story. The aim isn't to look finished — it's to be honest about where it stands and grow it into something genuinely solid, in the open.

**Platform support:** macOS and Linux. nibdex uses libgit2 (no external `git` binary at runtime) and a target-native filesystem watcher (FSEvents on macOS, inotify on Linux). Windows support is **in progress** — the code is cross-platform and the build is target-gated (vendored libgit2), but it has not yet been built or run on real Windows hardware. See [`WINDOWS_BUILD.md`](WINDOWS_BUILD.md).

---

## What it is

A single-binary Rust MCP server that indexes **five corpora** — source code, git commits, AI session history, memory files, and design docs — into one SQLite + FTS5 surface, exposes them as **seven query tools plus `check()`**, and emits a JSONL event stream with an optional per-call cost-savings ledger. Code hits come back with the git commit that last touched their file, so retrieval carries its own provenance; on a working tree that has drifted since indexing, a hit reports whether its location is still `verified`, was `relocated` to the current line, has gone `stale`, or is `file_missing`.

The differentiation claim (DESIGN §3): no existing tool surfaces **source code + git commits + session history + memory + design docs** *together* as structured records sharing one ranking surface, code and session hits anchored to a provenance commit. Per-corpus tools (`git log`, `ripgrep`, a curated-KB tool) each cover a slice; nibdex covers the cross-corpus join those tools can't reach without an AI synthesis step.

## First useful query in 10 minutes

```bash
# 1. Install — from crates.io (pre-release, so the version is explicit) …
cargo install nibdex --version 0.2.0-rc.2
#    … or from source
git clone https://github.com/gnuphie-labs/nibdex.git nibdex && cd nibdex && cargo build --release
#    (then use ./target/release/nibdex in place of nibdex below)

# 2. Point it at your workspace and run the initial scan (writes ./nibdex.db)
nibdex index --workspace ~/your/workspace

# 3. Start the MCP server over stdio against the scanned DB
nibdex mcp --db ./nibdex.db
```

Requires a Rust toolchain (1.88+) and, on macOS/Linux, a system `libgit2` for the `git2` crate (Homebrew `libgit2`, or `apt install libgit2-dev`); no `git` or `sqlite3` binary is needed at runtime. The optional cost-savings figures need a `calibration.toml` next to where you run `nibdex mcp`/`serve` (copy the one from the repo root, or pass `--calibration-toml`); without it every tool still works and `check()` simply omits `cost_savings`.

Then, from any MCP client (Claude Code, Claude Desktop, or a stdio JSON-RPC harness), call:

```jsonc
{ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
  "params": { "name": "find_code", "arguments": { "query": "connection pool", "limit": 5 } } }
```

You should get back a structured envelope with ranked code chunks — each with its repo, file path, line range, a match-centered snippet, and the git commit that last touched it — in a few milliseconds. Paths are repo-relative, so a hit's full location is its `repo_path` + `path`; pass `repo` to narrow a search to one of them. `find_code` works against any git repository with no setup beyond the index, which makes it the quickest way to see nibdex do something useful. Concrete query examples with real output (and the cost-ledger figures from those queries) are in [`docs/EXAMPLES.md`](docs/EXAMPLES.md).

That same `index` run also picks up your Claude Code sessions for this workspace, so [`find_session`](#find_session-recent_sessions-and-the-sessioncode-map) works from the quickstart too — searching past edits by the *reasoning* behind them. And when a query comes back with nothing, the response says whether nothing matched or whether that corpus is simply empty, so an empty result is never mistaken for an answer.

For an always-on daemon with file-watcher incremental indexing instead of stdio:

```bash
./target/release/nibdex serve --http 127.0.0.1:7878 --workspace ~/your/workspace
```

The HTTP MCP transport binds loopback-only at MVP (see [`docs/LIMITATIONS.md`](docs/LIMITATIONS.md) §1).

## Wiring to your MCP client

After `nibdex index` populates a DB, expose it to an MCP client one of two ways. `nibdex print-mcp-config` emits the right JSON for your install if you'd rather generate than hand-edit.

### HTTP (recommended for active workspaces)

Requires `nibdex serve` running. Cross-session warmth and file-watcher incremental indexing. Add to `<workspace>/.mcp.json`:

```json
{
  "mcpServers": {
    "nibdex": {
      "type": "http",
      "url": "http://127.0.0.1:7878/mcp"
    }
  }
}
```

Or generate it: `nibdex print-mcp-config --transport http --http 127.0.0.1:7878 > .mcp.json`

### Stdio (no daemon, per-session cold cache)

Reads the on-disk DB; one nibdex process per MCP session, exits at session end. Claude Code one-liner:

```bash
claude mcp add nibdex -- /path/to/nibdex mcp --db /path/to/nibdex.db
```

Or hand-write to `<workspace>/.mcp.json`:

```json
{
  "mcpServers": {
    "nibdex": {
      "command": "/path/to/nibdex",
      "args": ["mcp", "--db", "/path/to/nibdex.db"]
    }
  }
}
```

Or generate it: `nibdex print-mcp-config --transport stdio --db ./nibdex.db > .mcp.json`

Other MCP clients (Claude Desktop, Cursor, etc.) accept the same JSON shape with client-specific config-file locations — check your client's docs.

## The MCP tool surface

Seven query tools plus `check()`:

| Tool | Purpose | Returns |
|---|---|---|
| `find_code` | FTS5 search over source-code chunks across indexed repos; optional `repo` narrows to one | ranked chunks with `repo_path` + repo-relative path + line range + match-centered snippet + `location` (`verified`/`relocated`/`stale`/`file_missing`) + provenance commit |
| `find_commit` | FTS5 search over commit messages across indexed repos | ranked commits with full SHA + author + body + files_changed |
| `find_design_doc` | FTS5 search over design-doc `#` sections | ranked sections with heading path + line range + body |
| `find_memory` | FTS5 search over memory files (`~/.claude/projects/.../memory/`) | ranked memory entries with name + type + description + body |
| `find_session` | FTS5 search over the session→code map (Edit/Write actions recovered from Claude Code transcripts) | ranked edits with file + rationale + capturing commit |
| `recent_commits` | Recency view across all indexed repos | commits ordered by `authored_at_unix DESC` |
| `recent_sessions` | Recency view over sessions (optional FTS filter) | one representative row per session — its most-recent edit — ordered by latest edit |
| `check` | Index health, perf p50/p95, cost-savings rollup | structured snapshot for ops |

Query parameters take SQLite FTS5 `MATCH` syntax (no NL translation). Tokens containing punctuation FTS5 would reject (`fan-out`, `v0.1.3`) are auto-quoted; a multi-term query that AND-matches nothing is retried OR-broadened and the response says so (`query_broadened`). Grouping characters `( )` are left to FTS5 — quote a code fragment like `"parse_config("` yourself. See LIMITATIONS §2.

## `find_session`, `recent_sessions`, and the session→code map

`find_session` and `recent_sessions` search the **session→code map**: every `Edit` and `Write` nibdex recovers from your Claude Code transcripts (`~/.claude/projects/`), each stored with the file it touched, the assistant rationale that explained it, and the commit that later captured it. So you can ask "which session worked out the retry logic?" and get the actual edits back — matched by their *reasoning*, not just a filename — each pointing at its file and provenance commit. `find_session` ranks by relevance; `recent_sessions` returns one representative row per session (its most-recent edit), most-recently-active first.

`nibdex index` builds this corpus along with the other four — there is nothing extra to run. It reads the transcripts belonging to sessions that were working **inside `--workspace`**, which it works out from each transcript rather than from a flag: your sessions are spread across one transcript directory per directory you launched Claude from, so no single one covers a workspace. Sessions from another workspace on the same machine are left alone, as are edits an in-workspace session made outside it.

*(Until 0.2.0-rc.1 this needed a separate `nibdex index-sessions` step, and a quickstart that skipped it got an empty `find_session`. That command still exists for re-scoping a pass — `--workspace-scoped`, `--slug=<s>`, or `--all-slugs` — but you no longer need it to get started.)*

Two honest caveats specific to this corpus:

- **It's inherently your-machine-specific.** The source is your own transcripts, so unlike `find_code`/`find_commit` (which work against any clone), `find_session` can't be reproduced from a fresh checkout — there's nothing to clone. [`docs/EXAMPLES.md`](docs/EXAMPLES.md) shows the shape against the author's own history.
- **Recall starts at first index and grows forward.** Claude Code prunes old transcripts (~30 days by default), so nibdex indexes them additively: once an edit is captured it's kept, but edits from before you first indexed — whose transcripts have since rotated out — can't be recovered.

If you work across multiple clients or employers on one machine, this is the corpus most likely to mix them — see [Separating IP domains](#separating-ip-domains-optional) below.

> **Legacy CLAUDE.md session format.** Earlier nibdex parsed session entries from a specific `## Recent session history` CLAUDE.md shape. That corpus is still extracted but **no longer queried** — the transcript map above replaces it — and it will be removed in a future release.

## Optional: ride `grep` with `nibdex hook`

`nibdex hook` is a Claude Code `PreToolUse` hook that, when the tool call is a shell search (`grep`/`rg`/`ack`/`ag`) or the `Grep` tool, attaches the index's answer as `additionalContext` — the search still runs, so the live result stays authoritative. It fails open (any error → nothing emitted, tool proceeds), finds the nearest `nibdex.db` that covers the current directory (or `NIBDEX_HOOK_DB`), reads it without creating or migrating anything, and labels every answer with the age of the last indexing pass. Wire it as `nibdex hook --help` shows (with the `2>/dev/null || true` suffix). Two things to know: it appends one JSON line per firing — outcome, the search term, hit count, db path; never file contents — to `~/.nibdex/hook-log.jsonl` (see [`SECURITY.md`](SECURITY.md)), and `NIBDEX_HOOK_OFF=1` disables it entirely.

## Separating IP domains (optional)

If you keep more than one client's or employer's code on the same machine, nibdex can index each into its own database, so a query against one never surfaces another's code, commits, or session reasoning. You label your top-level subdirectories in a `.nibdex-domains.toml`, build one database per domain with `--domain`, and run one query server per database — each opens only its own file, so a domain's content is *physically absent* from the others' index rather than filtered out of results.

The guarantee is mechanical and needle-tested: a domain's database never contains another domain's files, commits, or session edits, nor the reasoning from a session that read or edited another domain's files inside the workspace (with two disclosed gaps: tools that don't name a path, like `Bash`, and a small neutral set of domain-less locations that is judged by place rather than content — see the guide). It is deliberately **not** an OS-level vault — nibdex keeps its *own* index from commingling domains, but can't police the rest of your machine; if you need a harder boundary the honest answer is separate accounts or machines. The full rationale, mechanism, and honest limits are in [`docs/IP_DOMAINS.md`](docs/IP_DOMAINS.md), with the security posture in [`SECURITY.md`](SECURITY.md).

This is off by default: with no `.nibdex-domains.toml`, nibdex indexes your whole workspace into one database exactly as before.

## Security & privacy

nibdex's threat model, redaction stance, and vulnerability-disclosure process are stated in full in [`SECURITY.md`](SECURITY.md). In short: the authorization model is filesystem access, the daemon binds loopback-only (a non-loopback bind is refused at startup), there is no network egress, and the local index stores workspace content verbatim — treat the `.db` with the same sensitivity as the workspace it mirrors.

nibdex is local-first and emits no telemetry. It makes no outbound network connections of its own — the daemon listens only on loopback, the index is local SQLite, and nothing is ever sent anywhere. The metrics it records (the cost-savings ledger / JSONL event stream) are written to a local path you control, and you can disable them entirely.

An **opt-in** `nibdex metrics-export` lets you *voluntarily* share a metrics payload to help improve nibdex, under strict rules: it is **initiated only by you**, it produces a **human-readable** bundle you **inspect and approve before anything is shared**, it carries **no IP and no sensitive data of any kind** — no source, no file contents, no workspace IP, no secrets, no personal data (queries reduced to shape features, paths to anonymized ordinals, error text to error-kind, author identities dropped) — and it is **honest** (losses included, no cherry-picking). When in doubt, a field is excluded. nibdex never transmits it — "sharing" is a local file you choose to hand over, never a phone-home. It does not change the zero-network-connection posture: nibdex never transmits the file, and producing one is something you run deliberately. See [`docs/DESIGN.md`](docs/DESIGN.md) §5.6.

## Documentation

| Doc | What it covers |
|---|---|
| [`docs/DESIGN.md`](docs/DESIGN.md) | Architecture, decisions, tenets, non-goals, and a retrospective build story |
| [`docs/EXAMPLES.md`](docs/EXAMPLES.md) | Concrete queries with real output + cost-ledger numbers |
| [`docs/LIMITATIONS.md`](docs/LIMITATIONS.md) | What it doesn't do and why; non-goals fence; operational limits |
| [`SECURITY.md`](SECURITY.md) | Threat model, redaction stance, vulnerability disclosure |
| [`docs/VERSIONING.md`](docs/VERSIONING.md) | Conservative sub-1.0 SemVer policy — the bar for `1.0.0` |
| [`LICENSE`](LICENSE) | MIT |
| [`LICENSING.md`](LICENSING.md) | Licensing intent — MIT core, open-core funding model, why a CLA |
| [`CLA.md`](CLA.md) | Contributor License Agreement |
| [`GOVERNANCE.md`](GOVERNANCE.md) | How nibdex is maintained, decisions made, and contributions accepted |
| [`CONTRIBUTING.md`](CONTRIBUTING.md) | How to propose a change and the in-repo CLA sign-off |

## Roadmap

The phased differentiator list from [DESIGN §4](docs/DESIGN.md#4-differentiators-phased) maps to the version progression in [`docs/VERSIONING.md`](docs/VERSIONING.md):

- **Phase 1a (`0.1.x`)** — D2 (structured workspace history: session + git commits + memory + design docs) + **D1a (source-code indexing with commit-anchored provenance — `find_code`)** + D0 (always-available local daemon with file-watcher incremental indexing).
- **Phase 1b (current, `0.2.x`)** — the D1 tail: live working-tree freshness, tree-sitter-aware code chunking, and D3 (richer MCP wrapping). *(The transcript-based session index that replaced the CLAUDE.md-format dependency has **landed** — `find_session`/`recent_sessions` read it now.)*
- **Phase 2 (`0.3.x` – `0.5.x`)** — D4 (derived graph from tree-sitter + regex + commit co-occurrence), D5 (provenance metadata), D6 (source-change cache invalidation).
- **Phase 3 (`0.6.x` – `0.9.x`)** — D7 (provenance-aware answer cache), D8 (local semantic search fallback), D9 (PG harness as alternative storage backend).

**Cross-cutting — now shipped:** the [IP-domain partition](#separating-ip-domains-optional) is an isolation capability that spans every corpus, **orthogonal** to the D0–D9 retrieval lanes — it changes *which database content goes into*, not what gets retrieved. It is the headline of the `0.2.x` line.

Phases are not promises with dates. The lane order is the commitment; the per-phase shape gets revised as dogfood surfaces what matters.

## About

nibdex is a personal open-source project by Richard Dunn — independent work on the author's own equipment and personal software licenses, with no compensation, sponsorship, or direction from any employer or third party. The author may use nibdex in workplace contexts the same way any developer uses an open-source tool, but the codebase, design, and direction are the author's alone.

## License

[MIT](LICENSE)
