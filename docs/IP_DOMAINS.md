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
files (with two disclosed gaps — tools that don't name a path, and the neutral set
below — see [How it works](#how-it-works)). That part is mechanical and testable, and the
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

Symlinks are part of that rule, not a hole in it: a tracked file, a `docs/`
directory, a root-level `*.md`, or a `CLAUDE.md` under a labeled subdirectory that
resolves *outside* it is skipped, never read through. (Symlinks resolving inside
the same subdirectory are also skipped on the source side, since their target is
indexed at its real path.) The invariant test plants a link to another domain's
tree in each of those four places and asserts none of that content lands.

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
touches anything belonging to *another* domain, nibdex stops trusting that session's
prose for this domain: from that edit on, every rationale is replaced with a fixed marker —
`[rationale withheld: cross-domain session]` — and the marker is never added to the
search index, so it can't be matched. The ratchet never resets within a session; once
crossed, it stays crossed. The file, timestamp, and tool of each edit still land
(those are safe) — only the prose is held back, and only from the point the session
first left the domain.

That last clause is a real limit, so state it plainly: the ratchet is **forward-only**.
Rationale on edits made *before* the crossing is kept, and assistant prose looks ahead
constantly ("next I'll wire this into the acme flow"), so a forward reference written
before the session crossed can still reach this domain's database. Edit *order* changes
the outcome for two otherwise identical sessions. Retracting a whole session once it
crosses would close this, and is deliberately not done — one stray read late in a long
session would erase hours of legitimate reasoning, and an over-broad predicate has
already been measured here at an 89.5% withhold rate that was almost entirely false
positives. The ratchet keeps a crossed session from *continuing* to launder; it does
not undo what it already stored.

**What does *not* trip it.** A small built-in set of paths belongs to no domain at
all, and touching them leaves the ratchet alone: files sitting directly in your
workspace root, **this workspace's own** `~/.claude` slug directory (its transcripts
and `memory/`), and its own temp scratch — a loose file in a temp root, or the agent
scratchpad for this workspace.

Note what is deliberately *excluded* from that list. Not `~/.claude` as a whole: it
also holds **other** workspaces' slugs, whose transcripts carry another domain's
source and prose verbatim. Not temp directories wholesale: a full client checkout in
`/tmp` is a normal thing to do. Both were in an earlier version of this set and an
adversarial review removed them.

Be clear about what the remaining exemptions are, though: they are judged **by
location, not by content**. nibdex cannot check that a root-level tracker or a scratch
file is free of another client's material — it trusts the *place*. If you keep
cross-client notes at your workspace root, that trust is misplaced; move them per
[the convention below](#two-things-that-need-a-convention). Everything else still taints,
including **an unlabeled subdirectory of your workspace**: that is exactly the shape a
second client's repo takes when you simply haven't labeled it, so it is treated as
foreign, not as neutral.

This distinction is load-bearing rather than a nicety. An earlier version asked only
"is this path in the domain?", which treats a scratchpad write the same as reading a
client's source. Measured on a clean single-domain machine that withheld **89.5%** of
all reasoning — and every one of those was a false alarm, since there was no second
domain on the box at all — a 100% false-alarm rate that is partly *definitional*,
since there was nothing true to catch. A companion replay over the same machine's 8
work-workspace transcripts showed **90.8%** of taints genuine. Both are one
developer's machine (n=344 edges; n=552 touches on the smaller work side) — enough to
motivate a design change, not enough to be a safety claim. The mechanism was right;
the question it asked was wrong.

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

0. **First, move any cross-cutting docs out of your workspace root** — see
   [Two things that need a convention](#two-things-that-need-a-convention). Root files
   reach no domain and there is no error when they don't; doing this before your first
   index saves you discovering it as a silent absence.
1. Write `.nibdex-domains.toml` at your workspace root (see [above](#the-label-file)).
2. Build one database per domain:
   ```
   nibdex index --domain personal --workspace ~/ws --db ~/personal.db
   nibdex index --domain client-a --workspace ~/ws --db ~/client-a.db
   ```
   Each of those also indexes that domain's sessions — the domain filter and the
   workspace scope both apply, so a domain database gets only the edits that are
   inside the workspace *and* inside that domain's labelled subdirectories. There
   is no separate session step to remember.

   To refresh only the session corpus (without re-walking source and commits), run
   `nibdex index-sessions --workspace-scoped --domain <d> --workspace ~/ws --db …`.
   That command also still takes `--slug=<dir>` for one transcript directory, or
   `--all-slugs` for every project on the machine with no workspace filtering —
   the latter is the widest option and worth avoiding on a shared box.
3. Run one query server per database — each opens only its own file:
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

### Two things that need a convention

A domain database holds only that domain's **labeled subdirs**. Two kinds of content
fall outside that by construction, and both need a deliberate choice from you.

**1. Workspace-root files → move them into a labeled subdir that is also its own repo.**
A file sitting at your workspace root (`BUG_TRIAGE.md`, a runbook, an architecture
note) belongs to no subdir, so **no domain indexes it**. There is no error — it is
simply absent, which is easy to miss. Labeling the file directly in
`.nibdex-domains.toml` does *not* work either: discovery filters whole anchors before
it looks at individual files.

The fix is a convention, not a flag. Give each domain its own docs directory, and make
it a **git repository**:

```
~/ws/my-docs/             <- its own repo, listed under `personal`
~/ws/client-a-docs/       <- its own repo, listed under `client-a`
```

Both properties are required. The **label** routes it into the domain; the **repo**
makes it a discovery anchor, since design-doc discovery looks at `$ANCHOR/*.md` and
`$ANCHOR/docs/**` where an anchor is a repository root. A labeled directory that isn't
a repo gets no design-doc coverage at all.

What should stay at the root is what genuinely belongs to no domain: `README.md`,
`CLAUDE.md`, `.gitignore`, `LICENSE`, your MCP config.

**2. Memory → claim it explicitly, if you're willing to assert it.**
The memory directory can't be moved — Claude Code owns its path — so it can't follow
the convention above. By default **no domain indexes memory**, and `find_memory`
returns nothing in domain mode. A domain can claim it:

```toml
[domains]
personal = ["my-app", "my-lib", "my-docs"]

[memory]
personal = "*"        # this domain owns the workspace's memory directory
```

Omit the table and nothing changes. A domain name that isn't declared, or any value
other than `"*"`, is a hard error at load rather than a silent no-op.

**Understand what you're asserting.** nibdex checks a subdir label against the
filesystem; it *cannot* check that a memory file belongs to the claiming domain. The
directory is per-workspace, so a single-domain workspace's memory holds only that
domain's **files** — but not necessarily only its **words**. Measured on nibdex's own
development machine: 8 of 25 memory files named another domain's projects, inside
*this* project's notes (a work/personal split, a pre-publication scrub checklist), not
as that domain's material. Per-file labeling wouldn't fix that — the files naming the
other domain include core memories of this one. If your memory notes routinely discuss
a client by name, don't claim it.

If one workspace holds **several** domains, they share one memory directory and a
whole-directory claim would be untrue. Leave it unclaimed — and note this is
enforced, not merely advised: **two domains claiming memory is a load-time error**,
since both databases would receive the same content while "claim" reads as exclusive.

### Checking your setup — `nibdex audit`

The partition fails quietly. Content that reaches no domain produces no error; it
is simply absent, and you find out months later when a query comes back empty.
`nibdex audit` is the lens for that:

```
nibdex audit --domain personal --db ~/personal.db --workspace ~/ws
```

It reports two things — **config that cannot work as written** (a label that
doesn't exist, a label that's a file, a labeled directory that isn't a git repo
and so is invisible to design-doc discovery, subdirectories in no domain), and
**coverage** (workspace-root files that reached no domain, and content under your
own labels that never made it in — usually because it's untracked in git, which
source indexing walks by design). Pass `--config-only` to run it before your
first index, which is when the config checks are most useful. It exits non-zero
only on `ERROR` findings.

Two things it deliberately does **not** do. It never infers which domain
something belongs to — guessing that is exactly the inference that would
mis-route your material, so it reports facts and leaves the decision in the
config file where it's auditable in one place. And it cannot tell you whether
text already in a database *discusses* another domain; that residual is semantic
and no index can judge it. **A clean audit means nothing was found, not that
nothing is there.**

#### Assigning what it finds — `--triage`

Reporting a gap isn't the same as closing one. `--triage` walks each
subdirectory that is in no domain and asks:

```
nibdex audit --domain personal --triage

'oss-tool/' is in no IP domain
    git repo (remote: github.com/you/oss-tool) · 214 file(s) · last commit by You — "bump deps"

  Which domain owns it?
    [1] personal
    [2] client-a
    [s] shared — several domains (comma-separated numbers)
    [a] acknowledge — reviewed, deliberately in no domain
    [ ] skip — decide later (reported again next run)
  >
```

It shows you **evidence** — is it a repo, what remote, how big, who last
committed — and never a suggestion. Guessing which domain a directory belongs to
is the one inference that would mis-route your material, and it would be trusted
precisely because it would usually be right. The facts are nibdex's job; the
decision is yours.

Answer `s 1,2` to share a directory across domains — that writes it under both,
which is how sharing is expressed here.

Nothing is written until you see the exact change and confirm it:

```
Planned change to ~/ws/.nibdex-domains.toml:
  [domains] personal + client-a  += "oss-tool"
  [unassigned] acknowledged += "scratch"

Apply? [y/N]
```

Edits are **additive only** — triage can add a label or an acknowledgement, never
remove or reorder one, so it can't quietly widen a domain's reach. Your comments
and formatting survive. Anything unparseable, an empty answer, or end-of-input
all mean *skip*, never a label. And it refuses to run without a terminal, so a
stray character in a pipe can't answer a prompt you never saw.

Re-index the affected domains afterwards — triage changes labels, not indexes.

#### Deciding in the config file itself — `--stage-undecided`

Prompts are one way to decide. Editing the config is another, and if you already
live in an editor it is the better one. `--stage-undecided` writes the
subdirectories that are in no domain **into `.nibdex-domains.toml`**, each with
its evidence beside it as a comment:

```
nibdex audit --domain personal --stage-undecided
```

```toml
# Discovered by `nibdex audit --stage-undecided`, still awaiting a decision.
# Move an entry into a domain's list under [domains] to index it, or into
# `acknowledged` to record that it stays out. Removing it from this list is
# what silences the audit. No domain is ever suggested for you: guessing which
# domain a directory belongs to is the one inference that would mis-route IP.
undecided = [
    # git repo (remote: github.com/you/oss-tool) · 214 file(s) · last commit by You — "bump deps"
    "oss-tool",
    # not a git repo · 3 file(s)
    "scratch",
]
```

You then decide by editing: move `"oss-tool"` down into a domain's list, move
`"scratch"` into `acknowledged`. There is no second format to learn, no import
step, and nothing to keep in sync — the file you edit *is* the config, and it is
already the one auditable record of the boundary.

`undecided` grants nothing, exactly like `acknowledged`. The only difference is
that `acknowledged` is silent while `undecided` is **reported**, so the list
converges to empty as you work through it. An annotation that were silent would
just be a slower way of forgetting.

Two mistakes are caught rather than absorbed. *Copying* an entry into a domain
instead of moving it — leaving it in both places — is a hard error at the next
command, because a file that reads "undecided" while the directory is in fact
being indexed is worse than no annotation at all. Listing something as both
`acknowledged` and `undecided` is the same class of half-finished edit:

```
[unassigned] lists as undecided "oss-tool", but it is also labeled in [domains] —
one of the two is stale. Remove whichever no longer applies.
```

Staging is idempotent — re-running adds nothing and rewrites nothing — additive,
and comment-preserving. Re-index the affected domains once you have decided.

#### Without a terminal — scripts and setup automation

`--triage` needs a terminal by design. For a setup script, a CI config check, or
a new-machine bootstrap, use the non-interactive pair:

```
nibdex audit --domain personal --json          # machine-readable findings
nibdex label oss-tool --domain personal --domain client-a   # share it
nibdex label scratch --acknowledge
nibdex label vendor --domain personal --dry-run             # plan only
```

The JSON carries an `unassigned` list — the actionable subset — so a caller
doesn't have to parse prose out of the report to find what needs deciding, plus
an `undecided` list of the ones already staged in the config, so a caller can
tell "never triaged" from "staged, still awaiting a decision."

`label` takes a domain and **never derives one**; omitting both `--domain` and
`--acknowledge` is an error rather than a guess. It shares the rest of triage's
fencing: additive-only, comment-preserving, validated *before* it writes (a
change that would leave the config contradictory is refused with the file
untouched), and idempotent, so re-running a provisioning script changes nothing. What it does
*not* have is the shown-and-confirmed step — the explicit arguments are the
confirmation, which is the normal CLI contract. Use `--dry-run` when you want
the review step back.

To keep it worth reading, record decisions you've already made:

```toml
[unassigned]
# Reviewed; deliberately in no domain.
acknowledged = ["nvim", "scratch"]
```

Acknowledged directories stop being reported, so a clean run means "everything
here has had a decision made about it" rather than "nothing is broken." It grants
nothing: an acknowledged directory is still indexed by no domain and still taints
sessions that touch it. Acknowledging is a statement about coverage, not trust.

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
  that read, edited, grep'd, or glob'd another domain's files **inside the workspace**
  by path — labeled elsewhere or unlabeled alike (the path-bearing tools the ratchet
  tracks — see the escape note under "How it works"). That part is mechanical and tested.
- **You can't rely on the neutral set being content-aware.** the built-in neutral set — files directly in your workspace root, this workspace's own `~/.claude` slug, and its own temp scratch — is exempted **by location, not by content**, so a root tracker or scratch file that happens to discuss another domain does not withhold.
  On the author's own machine an earlier, wider version of that set admitted two
  rationales naming another domain's project names — the same typed-prose residual
  below, reached through a different door. The set has since been narrowed to this
  workspace's own slug and its own scratch, but it is still a *location* rule.
- **You can't rely on:** anything you claimed via `[memory]` being verified — that one
  is your assertion, not a check nibdex can perform.
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

