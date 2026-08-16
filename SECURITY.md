# Security Policy

nibdex is a **local-first** developer tool. It indexes a workspace you already
own — git history, session notes, memory files, design docs, and source — into a
local SQLite database, and serves that index to an MCP client on the same
machine. It makes **no outbound network connections of its own**. This document
states the threat model explicitly and tells you how to report a vulnerability.

## TL;DR

- **The authorization model is filesystem access.** nibdex adds no privilege
  boundary of its own. Anyone who can read your workspace can already read the
  content nibdex indexes; the index is a derived local cache of files that sit in
  plaintext on the same disk.
- **No network egress.** The daemon listens on loopback only and refuses to bind
  a non-loopback address. Nothing is ever sent anywhere. There is no telemetry,
  no phone-home, no analytics.
- **The only path that can cross the machine boundary is `nibdex metrics-export`**
  — and it is opt-in, user-initiated, produces a human-readable file you inspect
  before sharing, and is scrubbed to structural features only (no content, no
  paths, no identities). See [Redaction stance](#redaction-stance) below.

## Threat model

### What nibdex protects

The trust boundary is **the local machine and its filesystem permissions**. The
index (`nibdex.db` and its WAL/SHM siblings) and the optional metrics ledger live
in a directory you control, protected by the same OS permissions as the workspace
they were derived from. nibdex does not weaken that boundary:

- **Loopback-only HTTP.** When run as a daemon (`nibdex serve --http`), the
  server binds `127.0.0.1` only. A non-loopback bind address is **rejected at
  startup before the listener is created** (`src/http_server.rs`, D-6.4.3;
  covered by the `serve_rejects_non_loopback_bind` test). There is no
  configuration that exposes the MCP surface to the network at MVP.
- **No outbound connections.** The daemon opens no sockets except the loopback
  listener. Indexing reads the filesystem and the local git object store (via
  `libgit2`); it never fetches, pushes, or resolves anything remotely.
- **No code execution from indexed content.** Indexed files are treated as text
  to be tokenized, never as anything to evaluate. Malformed input (huge files,
  binary blobs, half-rebased repositories) is handled defensively — file reads
  are size-capped and lossily UTF-8-decoded, git operations return errors that
  propagate rather than panic, and the parsers were audited for input-reachable
  panics (see the publication-readiness audit).

### What nibdex explicitly does *not* protect against

These are documented design decisions (see
[`docs/LIMITATIONS.md`](docs/LIMITATIONS.md) §1), not vulnerabilities:

- **No authentication, authorization, or RBAC.** Single user, single workspace.
  The MCP client and any local process with loopback access and filesystem
  permission are trusted. If untrusted local users share the machine, standard OS
  file permissions on the workspace and the index directory are your control —
  nibdex adds none.
- **No encryption at rest.** The index is a plain SQLite file. It contains a
  **verbatim copy of indexed workspace content** (see below). Protect the `.db`
  with the same care as the workspace itself; do not copy or share the database
  file expecting it to be scrubbed — it is not.
- **No multi-tenant isolation.** There is one index per workspace, no tenancy
  model.

### Assets and exposure

| Asset | Where it lives | Exposure nibdex adds |
|---|---|---|
| The FTS5 index (`nibdex.db`) | Local disk, user-controlled dir | None beyond the source files it mirrors |
| AI session-history edges (`session_edges`, built by `nibdex index`) | Drawn from `~/.claude/projects/`, **outside** the workspace tree | Mirrors your own assistant-conversation rationale + touched file paths into the index. Scope is derived, not opt-in — see the note below |
| Metrics ledger / JSONL sink | Local disk, opt-in, `off` by default | None (local file; disable with `--metrics-sink off`) |
| MCP query surface | Loopback socket or stdio pipe | Loopback-only; no remote reachability |
| `metrics-export` payload | A file you generate and choose to hand over | The only deliberate egress — scrubbed, see below |
| `nibdex hook` firing log (`~/.nibdex/hook-log.jsonl`) | Your home directory, **outside** the workspace; written only if you wire the optional `PreToolUse` hook | One JSON line per firing: timestamp, outcome, hit count, the db path, and the **term** — the search string the caller typed, or, for a SQL firing, the **table names** their statement referenced. No file contents, no result bodies, no command text, and never the statement itself. Read it back with `nibdex hook --stats`; delete the file or set `NIBDEX_HOOK_OFF=1` to stop it |
| Database schema corpus (`schema_objects`) | Local disk, in the index; present only if you place a `*.nibdex-schema.json` dump in the workspace | Mirrors the **structure** of a database — table, view and column names, types, and view/function bodies — into the index. No row data, and nibdex never connects: it reads the file you produced. Treat the dump and the `.db` with the sensitivity of the database they describe; a schema map is a useful thing for an attacker to have |

> **Changed in 0.2.0-rc.1 — session indexing is no longer a separate opt-in step.**
> `nibdex index` previously left `session_edges` empty unless you also ran
> `index-sessions`, which meant a documented quickstart produced an empty
> `find_session`. It now builds that corpus in the same pass, so **a plain
> `nibdex index` reads your Claude Code transcripts**. Be aware of that if you
> were relying on the old default to keep conversation rationale out of the
> index.
>
> What it reads is **derived from `--workspace`, not from a flag**, and both
> ends are scoped: an edit is indexed only if the session was working inside
> the workspace *and* the file it touched is inside the workspace or in the
> built-in neutral set (this workspace's own Claude directory and scratchpad).
> So another workspace's sessions are not swept in, and neither are the writes
> an in-workspace session makes into someone else's tree. Path matching is
> component-wise, so a sibling directory sharing a name prefix is not admitted.
>
> Both drop counts are reported in the `nibdex index` summary. If you want the
> corpus left alone entirely, point `--projects-dir` at an empty directory.

### Separating IP domains (multiple clients or employers on one machine)

If you keep separate IP domains on the same machine — personal vs. employer, or
several clients — there are two ways to keep their indexes apart, strongest first.

**1. Physically separate workspaces (strongest).** Run one nibdex instance per
domain, each with its own `--db`, `--workspace`, and (for `serve`) `--http` port:

```
nibdex serve --http 127.0.0.1:17878 --workspace ~/personal --db ~/personal.db
nibdex serve --http 127.0.0.1:17879 --workspace ~/client-a --db ~/client-a.db
```

Each daemon opens only its own database file, so a domain's index *physically*
cannot contain another domain's content — the boundary is the filesystem and
process, not a query filter, and no query bug can cross it.

**2. One workspace, several domains (the `--domain` partition).** When the domains
share a workspace (e.g. one repo folder mixing personal and client subdirs), label
each top-level subdir in a `.nibdex-domains.toml` at the workspace root:

```toml
[domains]
personal = ["my-app", "my-lib", "oss-lib"]   # a subdir listed under two
client-a = ["acme-api", "acme-web", "oss-lib"] # domains is shared with both
```

Then build one **separate database per domain** with `--domain`, and query each
through its own stdio MCP server pointed at that database:

```
nibdex index --domain client-a --workspace ~/ws --db ~/client-a.db   # incl. sessions
nibdex mcp   --db ~/client-a.db     # this domain's query server (never re-indexes)
```

> **Domain databases are index-only — do not point the file-watching daemon
> (`nibdex serve` / `nibdex watch`) at one over the shared workspace.** The watcher
> is not domain-aware: it re-indexes every repository and the memory directory it
> discovers into whatever database it holds, which would write *another* domain's
> commits and source back into this one and break the guarantee below. In domain
> mode a database is a point-in-time snapshot — refresh it by re-running
> `nibdex index --domain client-a …`, which covers that domain's sessions too. A domain-aware daemon
> is planned; until then, keep domain databases on the manual-reindex path and
> query them with a per-database `nibdex mcp` (which never re-indexes; it does write per-call latency rows to `op_measurements` in that db, and refuses to start if the db file does not exist).

`--domain client-a` writes **only** that domain's labeled subdirs into `client-a.db`
— across source, commits, design sections, **and** session edits. The guarantee,
which is **mechanical and needle-testable** (it is enforced by an invariant test
that fails the build on any cross-domain byte):

**Two deliberate exceptions to "labeled subdirs only" — one a silent coverage gap
with a documented remedy, one an explicit opt-in. Both are worth understanding before
you rely on the boundary:**

1. **Workspace-root files reach no domain.** They belong to no labeled subdir, so no
   domain indexes them — silently. Put cross-cutting docs in a subdir that is both
   labeled and its own git repo (see
   [IP_DOMAINS.md](docs/IP_DOMAINS.md#two-things-that-need-a-convention)). This is a
   coverage gap, not a leak.
2. **The memory corpus is skipped unless claimed.** A domain may claim the workspace's
   memory directory with a `[memory]` table. **This is the one place the boundary
   rests on your assertion rather than on a check** — nibdex can verify a subdir label
   against the filesystem, but it cannot verify what a memory note is about. The
   directory is per-workspace, so a single-domain workspace's memory holds only that
   domain's *files*; it may still contain *sentences* naming another domain (measured
   on the author's own machine: 8 of 25 files, inside this project's own notes). If
   one workspace holds several domains, they share a memory directory and the claim
   would be untrue — leave it unclaimed.

**The ratchet's neutral set — a location rule, not a content check.** A session is
tainted by touching another domain's tree, but *not* by touching paths that belong to
no domain at all: files directly in the workspace root, **this workspace's own**
`~/.claude` slug directory, and its own temp scratch. Deliberately NOT `~/.claude`
wholesale (it holds other workspaces' transcripts) and NOT temp dirs wholesale (a
client checkout in `/tmp` is normal) — an adversarial review removed both. What
remains is still judged by *place*, not by content: nibdex cannot verify that a
root-level tracker or scratch file is free of another client's material, so a
cross-client note left at the workspace root will not withhold. Move such notes per
the convention. An **unlabeled subdirectory still
taints** — that is the shape an unlabeled client tree takes. The narrowing is measured,
not assumed: the previous rule withheld 89.5% of reasoning on a single-domain machine
with a 100% false-alarm rate (partly definitional — the box had no second domain),
while a companion replay of the same machine's work-workspace transcripts showed 90.8%
of taints genuine. One developer's machine, n=344 edges — design motivation, not a
safety claim.

- A domain's database never contains **files, commits, design sections, or session
  edits from another domain's tree**. When a single Claude session edits across
  domains — or merely *reads/greps* another domain's file inside the workspace —
  that session's rationale is withheld (replaced by a constant marker, never
  indexed) **from the first cross point to the end of the transcript**, so a session
  that has crossed cannot go on laundering foreign content. A foreign checkout's
  working-directory path and branch name are dropped from the row. On the limits of
  that withholding, see the forward-only note below.

What this **cannot** claim:

- A domain's database contains no *information about* another domain. nibdex
  prevents mechanical *commingling* of one domain's artifacts into another's index;
  it is **not a semantic censor** of freely-typed sentences. A commit message or a
  design note in domain A's own tree can name domain B ("refactor auth like the
  acme flow"), and that line is indexed into A's database as written. Likewise a
  sentence you type in an otherwise single-domain session — "mirror client-a's
  rotation policy here" — with no tool call touching client-a's files taints
  nothing, so it is admitted. Closing this would require judging what a sentence is
  "about," which is unreliable and — worse — unauditable, so nibdex draws the line
  at what it can prove.
- That a **whole session** is scrubbed once it crosses. The ratchet is
  **forward-only**. Rationale attached to edits made *before* the session's first
  cross point is retained verbatim, and assistant prose routinely looks ahead
  ("next I'll wire this into the acme flow") — so a forward reference written
  before the crossing can reach this domain's database, even though the session
  went on to touch another domain. The ratchet stops a crossed session from
  *continuing* to launder; it does not retract what it already stored. Two
  sessions with identical content but a different edit *order* therefore withhold
  differently. Whole-session retraction was weighed and not taken: one stray read
  late in a long session would erase hours of legitimate rationale, and this
  project has already measured what an over-broad withholding predicate costs —
  89.5% of rationales withheld on a single-domain box, essentially all false
  positives — so widening the ratchet is not a free correction. Treat a session
  you know crossed domains as one whose *early* rationale may name the other side.
- A **complete** guard against every foreign-content vector. The taint set that
  drives the withholding is built from **path-bearing tool inputs** —
  `Edit`/`Write`/`Read`/`Grep`/`Glob`/`NotebookEdit` file targets. Vectors that
  carry no parseable path do **not** taint the session: a `Bash` command
  (`cat ../client-a/secret.env`), a path-less `Grep` that scans the working
  directory, or a `Task`/sub-agent doing foreign work. Content pulled in those ways
  can still reach a later rationale — a real gap, common enough to state plainly.
  The ratchet closes the tool-mediated *file* vectors; the context-separation
  discipline below covers the rest.

The mitigation for that residual is the same discipline nibdex is built around:
classify work by which context is active and don't cross-pollinate one domain's
detail into another domain's session. For strict contractual isolation, option 1
(separate workspaces) or separate user accounts/machines remain the stronger
boundary. Session scope is **derived from `--workspace`** rather than requested by a
flag (see the 0.2.0-rc.1 note above): an edit is indexed only when the session was
working inside the workspace *and* wrote inside it or into the built-in neutral set.
`index-sessions` still takes an explicit scope — `--workspace-scoped`, `--slug=<s>`,
or `--all-slugs` — and `--all-slugs` is the one that ignores workspace bounds
entirely, so on a machine holding more than one domain, prefer the other two.

**Coverage note — a domain database indexes less, unless you act.** It is built from
that domain's labeled subdirs, so two things are absent until you do something about
them: the **memory corpus** (absent unless a domain claims it via `[memory]` — see the
exceptions above; `find_memory` returns nothing until then), and **workspace-root
files** that belong to no subdir (absent unless you move them into a labeled subdir
that is also its own repo — the convention in
[IP_DOMAINS.md](docs/IP_DOMAINS.md#two-things-that-need-a-convention)). Neither is a
leak; both are fail-narrow omissions, and both are silent — worth knowing when a
domain db's `find_memory` or a root-doc lookup comes back empty.

**Operational note — domain databases are born in domain mode.** A per-domain
database must be created by a `--domain` run from the start; do not convert an
unpartitioned database by re-indexing it with `--domain` (its pre-existing rows are
not retro-filtered). And because indexing only ever *adds*, **narrowing** a domain's
labels (removing a subdir from it) does not un-index the now-foreign rows — rebuild
that domain's database after narrowing (the simplest way is to delete the database
file and re-index; `index-sessions --rebuild` wipes just the session edges).

## Redaction stance

**The primary index is not redacted, and this is out-of-scope by design.**

Content indexed into the local FTS5 tables — commit messages, session
transcripts, memory entries, design-doc prose, and source code — is stored
**verbatim**. nibdex does **not** scrub secrets, credentials, or PII out of the
corpus itself. The reasoning:

1. **Scrubbing the index protects nothing.** The index is a derived cache of
   content that already exists in plaintext on the same disk, under the same
   permissions. An attacker who can read `nibdex.db` can already read the source
   files it was built from. Redacting the cache while leaving the source
   unprotected would be security theater.
2. **Redaction would break retrieval.** A token that looks like a secret may be
   exactly what you are searching for (an API name, a config key, a hash in a
   commit message). Scrubbing the corpus would silently drop real results and
   make the tool's misses inexplicable.
3. **The boundary is drawn at egress, not at rest.** The one artifact that can
   leave the machine — the `metrics-export` payload — **is** rigorously scrubbed:
   an allowlist contract (`docs/METRICS_EXPORT_SPEC.md`), a build-time drift test
   that fails the build if an unclassified field appears, and an adversarial
   multi-agent IP-erasure pre-flight. Queries are reduced to shape features,
   paths to anonymized ordinals, error text to error-kind, and author identities
   are dropped. When a field's safety is in doubt, it is excluded.

**Practical implication:** treat the index database with the same sensitivity as
the workspace it indexes. If your workspace contains secrets, so does the `.db` —
keep it on the same trust footing (do not commit it, do not copy it off the
machine, do not share it). If you want a corpus that never absorbs a particular
file, keep that file out of the indexed workspace; there is no per-file redaction
layer, and adding one is not planned (it would not change the on-disk exposure of
the source file it mirrors).

## Reporting a vulnerability

Report suspected vulnerabilities **privately** — do not open a public issue.

- **Preferred:** GitHub's private vulnerability reporting on this repository
  (the **Security** tab → **Report a vulnerability**). This opens a private
  advisory visible only to you and the maintainer.
- Please include a description, reproduction steps, and the version or commit
  (`nibdex version`) you observed the issue on.
- Allow a reasonable window for a fix before any public disclosure. As a
  single-maintainer personal project (see [`GOVERNANCE.md`](GOVERNANCE.md)),
  there is no SLA, but reports are taken seriously and acknowledged.

Because the authorization model is filesystem access, note that "a local user
with read access to the workspace can read the index" is the intended posture,
not a vulnerability. Reports that turn on **breaking a boundary nibdex claims to
hold** — a non-loopback bind slipping through, network egress from the daemon,
the metrics-export scrub leaking content or IP, a malformed-input panic taking
down the daemon, or a dependency advisory — are exactly what this process is for.

## Supported versions

nibdex is pre-1.0. Only the latest release (and `main`) is supported; fixes land
forward, not as backports. Dependency advisories are tracked with `cargo audit`
and `cargo deny` (see the CI and `deny.toml` / `.cargo/audit.toml`).
