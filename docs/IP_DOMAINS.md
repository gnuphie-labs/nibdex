# Keeping IP domains separate

If you work on more than one client's or employer's code on the same machine — a
consultant with several clients, or someone whose personal projects live next to
work they're paid for — nibdex can keep each one's index in its own database, so a
query against one never surfaces another's code, commits, or reasoning.

This is an **opt-in** feature. If you don't set it up, nibdex indexes your whole
workspace into one database exactly as it always has, and the word "domain" never
comes up. Turn it on only when you have IP that must not mix.

A word on what this is and isn't, up front, because it shapes everything below:
nibdex guarantees that **its own index** never commingles one domain's content into
another's. It is **not** an operating-system vault, and it can't make the rest of
your machine — your shell, your editor, git, any other tool — respect the boundary.
If your isolation requirements are strict enough to need that, the honest answer is
separate machines or separate user accounts. What nibdex offers is narrower and,
for a lot of people, exactly enough: *nibdex won't be the thing that leaks one
client's IP into another's context.*

---

## Why this exists

**The problem.** Most of what nibdex indexes is naturally tied to a project: source
lives in a git repo, commits belong to that repo, design docs sit alongside it. But
one corpus isn't — your Claude Code **session transcripts**. Claude Code keeps every
session for every project in a single machine-wide directory, keyed by the working
directory it ran in. So the moment nibdex indexes sessions, it's reaching across a
boundary that the other corpora respect for free: one flat pool holding your work
for *every* project on the machine.

For most people that's a feature — "which session did I work out the retry logic in,
in *any* project?" is a good question to be able to ask. But if some of those
projects are a client's and some are your own, an unfiltered session index quietly
becomes a place where one client's reasoning sits next to another's. The same is
true, more subtly, of source and commits when several projects share one workspace.

**Why a derived index makes this worse, not better.** nibdex is a search index — it
*copies* content out of your files into an FTS database so it can answer questions
fast. That's the whole point, but it means the isolation you might assume from
"these are different folders" doesn't automatically carry over. Once the text is in
one shared index, a single query can pull from all of it. The index is exactly where
domains can bleed together if nothing keeps them apart.

**Why separate databases, and not a filter.** The obvious fix is a `domain` column and
a `WHERE domain = ?` clause on every query. We deliberately didn't do that, for one
reason worth being precise about: SQLite has no row-level access control — no users,
no roles, no per-row permissions. In a single shared database, domain separation is
*only* as good as remembering to write that `WHERE` clause on every query, forever.
One forgotten clause, one query path that skips it, and it's a leak. A filter is a
promise to be careful; it isn't a boundary.

So a domain gets its **own database file**. The server that answers a domain's
queries opens **only that file** and never attaches another. The other domain's rows
aren't filtered out of the results — they were never in a file the process opened.
The leak isn't prevented by a correct query; it's *structurally impossible*, because
the content isn't in any file the process opened. To be precise about what kind of
boundary that is: it comes from **process configuration** — which single database
file each server opens — not from OS permissions. Both databases are your own files
on your own account, so the filesystem isn't enforcing anything *between* them; the
guarantee is that a given server is only ever handed its own database and never
attaches another. That's a boundary you get on one ordinary machine with a little
extra disk for the index — which matters, because that's who nibdex is for.

**Who this is for, and why the boundary has to be nibdex's own.** nibdex is built for
the resource-constrained solo and indie developer — someone stretching a fixed AI
budget, not backed by a company that can hand out a second laptop per client. For
that person, "just use separate hardware" or "spin up a VM per client" isn't frugal
advice, it's out of reach, and telling them to buy their way to isolation would
contradict the whole reason nibdex exists. So the domain boundary nibdex provides
isn't a consolation prize for people who can't afford the real thing — for this user
it *is* the mechanism, which is why it's built to a real, testable guarantee rather
than a best-effort filter.

**The honest edge of the guarantee.** Being precise about what "separate" means: a
domain's database will never contain another domain's *files, commits, or session
edits*, and never the reasoning from a session that read or edited another domain's
files (with one disclosed gap, for tools that don't name a path — see
[How it works](#how-it-works)). That part is mechanical and testable, and the
How-it-works section explains how. What it does **not** do is scrub *sentences*. If, in an otherwise
single-domain session, you type a sentence that mentions another client by name,
that sentence is admitted — nibdex prevents domains' *artifacts* from mixing, but it
is not a semantic censor of things you freely type. This isn't a corner case to
bury; it's common enough that the docs say it plainly, and [Using
it](#using-it) covers what that means for you in practice. It's the same line
nibdex has always held: a commit message in one project can already mention another,
and nibdex indexes it as written.

## How it works

Two setup files and a couple of commands do the whole job. Underneath, the whole
feature is built around one small predicate that a test hammers on.

### The label file

You describe your domains once, in a `.nibdex-domains.toml` at the workspace root:

```toml
[domains]
personal = ["my-app", "my-lib", "oss-lib"]
client-a = ["acme-api", "acme-web", "oss-lib"]  # oss-lib shared with both
```

Each key is a domain name — pick whatever names you like, there are no reserved
words — and each value lists the top-level subdirectories that belong to it. A
subdirectory can appear under more than one domain; that's how you share a common
library without duplicating it on disk (`oss-lib` above is indexed into both). If the
file isn't there, nibdex runs unpartitioned, exactly as before.

### Routing source, commits, and design docs (index time)

When you run `nibdex index --domain client-a --db client-a.db`, nibdex reads the
whole workspace but writes into `client-a.db` **only** the content whose top-level
subdirectory is labeled for `client-a`. A repository under `acme-api/` goes in; one
under `my-app/` doesn't. A shared subdirectory goes into every domain that lists it.
Files sitting loose at the workspace root belong to no domain and are indexed into
none. That's the invariant, and it's a rule the build actually checks:

> A domain's database contains content only from subdirectories labeled for that
> domain (plus any it shares). Content from an unlabeled or foreign subdirectory
> never appears in it — source, commits, design docs, or sessions.

### Routing sessions (the harder half)

Sessions are the machine-global corpus, so they can't be sorted by "which repo is
this" the way the others can — one session can edit files across several projects.
nibdex routes them **one edit at a time**. For every `Edit`/`Write` recovered from a
transcript, it looks at the raw path the edit targeted and asks a single question:
does this path belong to the domain being indexed? Only if the answer is yes does the
edit land in that domain's database.

That question is the one safety-critical primitive in the whole feature. It
normalizes the path first — lexically resolving any `.`/`..` segments, then resolving
symlinks — and only then checks it against the label map. Anything it can't
confidently place under a labeled subdirectory — a foreign subdir, an unlabeled one,
the workspace root, a path outside the workspace, a `..` that escapes what can be
resolved — is treated as **not** belonging, and the edit is dropped rather than
guessed at. The whole thing is built to **fail toward leaving content out**: when
nibdex isn't sure, it forgets, and forgetting costs you a search result, never a leak.

### The ratchet — why reasoning is sometimes withheld

Routing the *files* per edit is mechanical. The reasoning attached to them is not.
Each session edit carries a rationale — the assistant text that explained it — and
that text is shared across every edit in the same message. If one message edits a
file in two different domains, naively copying its rationale into both databases would
put one domain's reasoning into the other's index, even though the file routing was
correct.

nibdex closes that with a **ratchet**. As it walks a session, it tracks every file
path the session names through a **path-bearing tool** — an `Edit`, `Write`, `Read`,
`Grep`, `Glob`, or `NotebookEdit` target — not just the edits, so reading or grepping
another domain's file by path counts too. (One honest gap, worth stating plainly:
tools that carry no parseable path — a `Bash` command like `cat ../other/x`, a
path-less `Grep`, a `Task` sub-agent — fall outside this set and don't taint, so
content pulled in that way can still reach a later rationale. It's disclosed in
[`SECURITY.md`](../SECURITY.md) and is common enough to keep in mind.) As long as
everything the session has named so far belongs to the domain being indexed, the
rationale is stored in full. The moment the session
touches anything outside that domain, nibdex stops trusting that session's prose for
this domain: from that edit on, every rationale is replaced with a fixed marker —
`[rationale withheld: cross-domain session]` — and the marker is never added to the
search index, so it can't be matched. The ratchet never resets within a session; once
crossed, it stays crossed. The file, timestamp, and tool of each edit still land
(those are safe) — only the prose is held back, and only from the point the session
first left the domain.

For the same reason, a session's recorded working directory and git branch are
dropped when that directory isn't in the domain: a branch name like
`client-a/hotfix-cve-1234` is itself IP.

### The release gate

None of this is trusted to review. The feature ships behind an **invariant test**
that builds a synthetic two-domain workspace — including the traps that broke earlier
drafts, like a sibling directory whose name is a string-prefix of a labeled one —
replays a scripted session that deliberately edits across domains (and reads one
domain's file, then quotes it while editing another's), indexes it into two separate
databases, and then sweeps **every text column and the full-text index** of each for
any token that belongs to the other domain. The test passes only when that count is
zero. It's been mutation-checked, too: deliberately breaking the routing makes the
sweep fail — which is how we know the sweep can actually see a leak. That test is the
real guarantee behind everything above.

## Using it

### Set it up

1. Write `.nibdex-domains.toml` at your workspace root (see [above](#the-label-file)).
2. Build one database per domain:
   ```
   nibdex index --domain personal --workspace ~/ws --db ~/personal.db
   nibdex index --domain client-a --workspace ~/ws --db ~/client-a.db
   ```
3. Index each domain's sessions into the same database. Session scope is required —
   pass `--all-slugs` to scan every project's transcripts (the domain filter still
   keeps only this domain's edits), or `--slug=<dir>` to restrict to one:
   ```
   nibdex index-sessions --domain personal --all-slugs --workspace ~/ws --db ~/personal.db
   nibdex index-sessions --domain client-a --all-slugs --workspace ~/ws --db ~/client-a.db
   ```
4. Run one query server per database — each opens only its own file:
   ```
   nibdex mcp --db ~/personal.db     # stdio, read-only
   nibdex mcp --db ~/client-a.db     # a separate server for the other domain
   ```
   Point each Claude Code workspace at the server for its domain. Use `nibdex mcp`
   (stdio, read-only) for a domain database — **not** `nibdex serve`: the
   file-watching daemon isn't domain-aware yet (see the first rule below), so pointing
   it at a domain db would write foreign content back in. A domain-aware daemon is planned.

If your workspace is itself a git repository wrapping the project repos (a monorepo
layout), add `--include-nested-repos` to the `index` commands, or nibdex will see
only the outer wrapper.

### Three rules that keep the boundary intact

- **Domain databases are index-only.** Don't point the file-watching daemon
  (`serve`/`watch`) at a domain database over the shared workspace. The watcher isn't
  domain-aware yet — it would re-index every repository it discovers back into
  whichever database it holds, writing another domain's content into this one.
  Refresh a domain database by re-running `nibdex index --domain …` /
  `index-sessions --domain …` yourself. (A domain-aware daemon is planned.)
- **Born in domain mode.** Create a domain database with `--domain` from the start.
  Don't take an existing unpartitioned database and re-index it with `--domain` — its
  pre-existing rows aren't retro-filtered.
- **Narrowing labels needs a rebuild.** Indexing only ever adds. If you remove a
  subdirectory from a domain, that subdir's already-indexed rows don't disappear on
  the next run — rebuild that domain's database from scratch (the simplest way is to
  delete the database file and index it again). `index-sessions` also takes a
  `--rebuild` flag that wipes just the session edges.

A domain database also **indexes less than the whole workspace**, by design: it holds
only that domain's labeled subdirs, so `find_memory` returns nothing (memory is
workspace-global and isn't attributed to a domain) and workspace-root files that
belong to no subdir aren't indexed into any domain. Neither is a leak — just expect an
empty `find_memory` in domain mode. (Full note in [`SECURITY.md`](../SECURITY.md).)

### Reading the counters

In domain mode, `nibdex index-sessions` adds a line to its summary:

```
domain [client-a]: 2870 edge(s) dropped (foreign target), 30 rationale(s) withheld (cross-domain session)
```

- **dropped (foreign target)** — edits that targeted another domain, or no domain,
  and were correctly left out.
- **rationale(s) withheld** — edits kept, but with their reasoning withheld because
  the session had already touched another domain (the ratchet).

A high withhold or drop count on a mixed workspace is expected, not a bug — it's the
price of the guarantee, and nibdex surfaces it as a number rather than hiding it. If
it's punishingly high for how you actually work, that's the signal to look at how your
sessions cross domains.

### What you can and can't rely on

- **You can rely on:** a domain's database never containing another domain's files,
  commits, design sections, or session edits, and never the reasoning from a session
  that read, edited, grep'd, or glob'd another domain's files by path (the path-bearing
  tools the ratchet tracks — see the escape note under "How it works"). That part is
  mechanical and tested.
- **You can't rely on:** the database being free of any *mention* of another domain.
  A commit message or design note in this domain's own tree can name another and is
  indexed as written; a sentence you type in an otherwise single-domain session — with
  no tool call touching the other domain — is admitted. nibdex separates domains'
  *artifacts*; it doesn't censor sentences. The practical mitigation is the discipline
  you'd keep anyway: don't paste one client's proprietary detail into another's
  session.

The full security posture, including the boundaries nibdex explicitly does *not*
enforce, is in [SECURITY.md](../SECURITY.md) and
[LIMITATIONS.md](LIMITATIONS.md).

