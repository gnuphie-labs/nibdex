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
| AI session-history edges (`session_edges`, opt-in `index-sessions`) | Drawn from `~/.claude/projects/<slug>`, **outside** the workspace tree | When populated, mirrors your own assistant-conversation rationale + touched file paths into the index; scoped to the workspace slug you pass |
| Metrics ledger / JSONL sink | Local disk, opt-in, `off` by default | None (local file; disable with `--metrics-sink off`) |
| MCP query surface | Loopback socket or stdio pipe | Loopback-only; no remote reachability |
| `metrics-export` payload | A file you generate and choose to hand over | The only deliberate egress — scrubbed, see below |

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
nibdex index          --domain client-a --workspace ~/ws --db ~/client-a.db
nibdex index-sessions --domain client-a --workspace ~/ws --all-slugs --db ~/client-a.db
nibdex mcp            --db ~/client-a.db     # this domain's query server (read-only)
```

> **Domain databases are index-only — do not point the file-watching daemon
> (`nibdex serve` / `nibdex watch`) at one over the shared workspace.** The watcher
> is not domain-aware: it re-indexes every repository and the memory directory it
> discovers into whatever database it holds, which would write *another* domain's
> commits and source back into this one and break the guarantee below. In domain
> mode a database is a point-in-time snapshot — refresh it by re-running
> `nibdex index --domain client-a …` (and `index-sessions`). A domain-aware daemon
> is planned; until then, keep domain databases on the manual-reindex path and
> query them with a per-database `nibdex mcp` (which only reads).

`--domain client-a` writes **only** that domain's labeled subdirs into `client-a.db`
— across source, commits, design sections, **and** session edits. The guarantee,
which is **mechanical and needle-testable** (it is enforced by an invariant test
that fails the build on any cross-domain byte):

- A domain's database never contains **files, commits, design sections, or session
  edits from another domain's tree**, and never contains **rationale prose from a
  session that also touched another domain's tree**. When a single Claude session
  edits across domains — or merely *reads/greps* another domain's file — that
  session's rationale is withheld (replaced by a constant marker, never indexed)
  from the first cross point onward, so the reasoning text can't launder foreign
  content across. A foreign checkout's working-directory path and branch name are
  dropped from the row.

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
boundary. Session indexing also **requires** an explicit `--slug` or `--all-slugs`,
so it never silently pulls another workspace's transcripts into the wrong database.

**Coverage note — a domain database indexes less.** It is built from that domain's
labeled subdirs only, so two things are absent *by design*: the **memory corpus**
(`find_memory` returns nothing — memory is workspace-global and isn't attributed to a
domain), and **workspace-root files** that belong to no subdir (a root-level README or
tracker is attributed to no domain, so it is indexed into none). Neither is a leak;
both are fail-narrow omissions worth knowing when a domain db's `find_memory` or a
root-doc lookup comes back empty.

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
