# Changelog

Notable changes per release. Versioning policy: [`docs/VERSIONING.md`](docs/VERSIONING.md).

## 0.2.0-rc.4 — an empty index announces itself; a miss can come back as a pointer; a response can hand back vocabulary (2026-08-23)

Seven changes, four of them fixes for issues filed against rc.3.

**This is alpha software, and "rc" overstates it.** The identifier is a sequence
token kept for version-ordering reasons ([`docs/VERSIONING.md`](docs/VERSIONING.md)),
not a claim that this could ship as final. Preparing this release turned up a wire
field missing from its own notes and a release gate that had been validating an
already-published version — the kind of thing a beta should not still be finding.

Two are worth reading before the rest. The vocabulary feature is new enough that
its caveats matter more than its description, and it is disclosed that way. And
`body_excerpt` is the **one change here that alters an existing output shape** — it
is no longer emitted beside a body that already contains it. An index built by rc.3
keeps working untouched; a consumer that reads `body_excerpt` unconditionally does
not. See **Upgrading** for the whole picture in one place.

**And one thing this release deliberately does not fix.** Several changes here make
nibdex *find* more; none makes it *rank* better, and ranking is the weaker half by a
clear margin. That is stated in full below under **The known limitation this release
does not fix, and is not waiting for** — read it before deciding whether the tool
earns a place in your workflow.

### An empty index is a state, not silence

`serve` does not index — that split is deliberate and unchanged — but a database
that had never been indexed came back from a restart with `/healthz` answering
`200`, every corpus at `0`, and no indication anywhere that anything was wrong.
Only a manual poll caught it.

rc.3 made `nibdex hook` the front door, which raises the stakes: the hook fails
open silently when it finds no index, so the primary path degrades to returning
nothing with no signal at either end. A tool that quietly stops helping is worse
than one that fails, because nobody goes looking.

`/healthz` now carries `index_empty`, so a probe can tell *never indexed* from
*indexed and quiet* without knowing the corpus schema, and startup prints a
warning naming the database path and the exact command that repairs it.

**The honest limit:** that warning goes to stderr, which under a service manager
lands in a log file nothing reads. This was confirmed in the field within hours
of the fix — a daemon was SIGKILLed and its log went on displaying the previous
run's cheerful "watching" lines while the service was gone. **The `/healthz`
field is the robust half.** Making the empty state *unmissable* rather than
merely discoverable needs a surface this release does not add.

### `find_memory` and `find_design_doc` can hand back the corpus's own vocabulary

The failure this addresses is not a retrieval failure, and it scores as a success
on every counter nibdex keeps: a caller invokes the right tool, asks one
badly-worded question, receives ten plausible hits, concludes the corpus does not
contain the material, and leaves for `grep`. Ten results came back. Nothing was
broken. The words simply were not the ones the documents were written in.

Documents are written in the register of a person and an occasion. Identifiers,
table names and codes are vocabulary imposed later, by the reader. A caller
cannot guess the first from the second — but the index holds both.

Two additive fields:

- **`neighbourhood_terms`** — up to eight distinctive terms of the wider region
  around the query, scored by term frequency against corpus-wide document
  frequency. They are drawn from an OR-broadened form of the query, never from
  the hits it already returned: terms taken from your own hits reinforce the
  neighbourhood you already reached, and the value is in the region you missed.
  Emitted only when that broadened neighbourhood is **strictly larger** than what
  the query matched — a fact about the retrieval (*there is more here than you
  reached*), not a judgement about whether the results were any good.
- **`retrieval_shape`** — `top_rank`, `rank_spread` and `neighbourhood_matched`.
  bm25 ranks are not interpretable across corpora or query shapes without a
  baseline the caller does not have, and the response has always carried the
  makings of this signal while saying nothing about it.

**A `strong | mixed | weak` verdict was deliberately not built.** It needs a
per-corpus percentile baseline, and a mis-calibrated advisory is worse than no
advisory — it teaches callers to ignore the field, which is much harder to undo
than adding one later.

#### What is wrong with it, stated up front

**It is days old.** This is a release candidate in the honest sense: the feature
has had a fraction of the use the rest of the tool has had, and it is being
published early on purpose, because whether it helps anyone other than its author
is not something more time on one machine can answer.

**Known weakness — corpus-local boilerplate survives.** Document frequency is
computed over the whole index, so a term that is ubiquitous *inside one corpus*
but unremarkable across all of them keeps a high score. On a corpus of
template-shaped files — every file carrying the same headings — the top
"distinctive" terms came back as the template's own words. A local ceiling (a
term appearing in more than 60% of the sampled sections is treated as structure,
not register) removes the worst of it and is what ships. **It does not fully fix
it.** The real answer is a corpus-scoped background sample, so the score becomes
contrastive — frequent *here* relative to this corpus generally — and that is not
built. It was left unbuilt deliberately: the ceiling was already one adjustment
made against a handful of probes, and a second would be tuning on known
positives, which measures the base rate rather than the signal.

**`find_code` does not have it.** Its count query carries a repo filter with
extra bindings that the current helper does not fit. Filling the field with the
unbroadened total would have stated a fact nobody checked, and an unverified fact
is worse than an absent one.

**It costs bytes.** A handful of words and three numbers, once per response, not
per hit — but caller-side bytes are the scarce resource here, and this spends
some of them on a field whose value is unproven.

### Results stop paying twice for their own opening line

`find_design_doc` and `find_code` shipped both a `body` and a `body_excerpt` —
the first 200 characters of that same body, word-boundary truncated. Where the
body was present, the excerpt was a strict prefix of text the caller already had.

Measured on 273 real calls in this workspace's own metrics stream, that
duplication is the single largest removable share of a response payload:

| tool | calls | median payload | excerpt share |
|---|---|---|---|
| `find_design_doc` | 121 | 1,199 tokens | **20.9%** |
| `find_code` | 152 | 2,190 tokens | **18.3%** |

The excerpt is now emitted **only when the total-body budget dropped `body` to
empty** — the case where it is not a duplicate but the sole content the hit
carries, alongside `body_truncated: true` and the line range needed to read the
rest. Present body, no excerpt; dropped body, excerpt.

**This is the one output-shape change in this release.** A consumer that reads
`body_excerpt` unconditionally now sees an absent field on most hits and must
fall back to `body`. The field is omitted rather than emitted empty.

**The near-miss worth recording:** the change was first scoped as "drop
`body_excerpt`, it is a strict prefix of `body`, so no information is lost." That
is true exactly while a body is present, and false for the budget-dropped tail,
where removing it would have returned hits carrying a heading and a line number
and no content at all. A test asserting `"a dropped body still carries an
orienting excerpt"` is what caught it. The test now pins **both** halves — a
dropped body keeps its excerpt, a present body must not ship one — because
asserting only the first half would let the unconditional excerpt back in without
failing anything.

### Scan deeper than you render — a miss can now come back as a pointer

The measurement that drove this is the uncomfortable one: on a labelled set of real
searches, **52 of 53 single-term misses used words the index already held**. Callers
were not asking in the wrong register. The right document existed, used their words,
and sat below the window.

Widening the window confirms it and isolates where the problem is not:

| window | found | hit@1 | hit@3 |
|---|---|---|---|
| 10 | 39.3% | 14.9% | 25.6% |
| 25 | 45.8% | 14.9% | 25.6% |
| 50 | 48.2% | 14.9% | 25.6% |

So nibdex now scans to a fixed depth of 40 and renders `limit`. Everything between
comes back as **`also_matched`** — one pointer per *file*, deduped, carrying the best
matching line and a count, never a body.

- of the misses, the tail points at **14.7%** of them
- **answered somehow — body or pointer — is 48.2%, up from 39.3%**
- that is exactly the limit-50 recall at the byte cost of rendering 10

**A pointer is weaker than a body and is counted separately.** It is never folded into
`hit@k`; the headline retrieval numbers are unchanged and are meant to be.

**Byte accounting, driven on the real binary rather than reasoned about:**
`find_design_doc("hysteresis")` renders 10 bodies (4,064 chars) and 6 tail pointers
(~715 chars) — the tail is **15% of payload**, against the **~21%** freed in the same
release by dropping the redundant `body_excerpt`. Responses are still net smaller.
Dedupe-by-file is what makes that true: ranks 11–40 are frequently several chunks of
one document.

**A predictor was tried first and there isn't one — recorded so nobody rebuilds it.**
bm25 spread runs 2.916 on misses against 4.075 on hits, window saturation 76% against
67%: both directional, both overlapping far too heavily to gate on. No predictor is
needed, because the costs are asymmetric — query latency is nearly free and caller
bytes are not. So never decide *whether* to look deeper. Always look, and control cost
at the rendering end.

**What this does not fix, stated plainly:** the head of the ranking. `hit@1` is 14.9%
against a plain shell search's 28%, and it did not move at any window depth. Every
gain here is in the tail. That remains the open problem, and it is the one a caller
notices first.

### `find-code --format grep` now emits a path you can actually use

The CLI printed hits as `src/source_index.rs:1203:…` — repo-relative, with no repo.
On a multi-repo index several repos have a `src/main.rs`, so that string resolves
correctly only if the reader happens to be in the right directory, and resolves to
the *wrong file* rather than to nothing if they are in a different one.

`CodeHit` already carried `repo_path`, and its own doc comment called it "the half
that makes a hit openable". The formatter simply never used it. Hits are now anchored
to an absolute path; a quickfix `%f` takes one happily, and an absolute path cannot
resolve against the wrong repo. Where the repo root is genuinely unknown the bare
relative path is still emitted, because a guess would be worse than an honest partial.

Closes [gnuphie-labs#8](https://github.com/gnuphie-labs/nibdex/issues/8). The MCP
surface was already correct (`repo_path` + `path` as separate fields) — this was the
un-migrated CLI half, and the same defect cost a working session once already, when a
replay harness compared these relative paths against absolute ones and scored 0.000
on 150 of 150.

### `find_memory` results now say where they came from

Every other corpus returns its hit's location. `find_memory` did not — and the
consequence was worse than not being able to open a result.

A memory directory can hold subdirectories, and `_archive/` for retired entries is a
real convention. Nothing in `name`, `memory_type`, `description` or the frontmatter
distinguishes a retired entry from a live one, so without a path they were the same
object to a caller. Measured on a real corpus: a query about a since-replaced
authentication vendor returned two archived entries about that vendor **ranked above
the live entry recording that it had been replaced**. Superseded guidance competed
with current guidance, silently.

`documents.path` held the location the whole time. `run_find_memory` joined
`search_index → memory_entries` and stopped; `find_design_doc` and `find_code` both
join `documents` and return the path. So the memory corpus behaved as a flat
namespace keyed on `name` — an impression `UNIQUE(name)` on `memory_entries`
reinforces — while storage knew better.

The fix is that join plus a `path` field on `MemoryResult`. Closes the reported half
of [gnuphie-labs#21](https://github.com/gnuphie-labs/nibdex/issues/21); the other
half, addressing named `##` sub-sections *inside* a memory file, is still open.

**Additive:** callers that ignore `path` see no change.

### A warning cited a documentation section that does not exist

The retired `session_entries` extractor warns about entry shape and pointed at a
`LIMITATIONS.md` section number that was never there. It now cites the section by
name. Minor, but a diagnostic that names a nonexistent reference costs the reader
more than silence would.

### The known limitation this release does not fix, and is not waiting for

Three of the seven changes above make nibdex find more. **None of them makes it rank
better, and ranking is the part a caller notices first.**

Measured against a labelled set of real searches, nibdex places the file the session
actually opened at rank 1 **14.9%** of the time and in the top three **25.6%** of the
time. A plain shell search over the same material manages roughly 28% and 52%. The
comparison is not clean — different denominators, different query populations, and
the labelled set is the author's own sessions rather than anything a reader can
reproduce — so read it as a direction rather than a benchmark. What can be said
flatly is that **nothing measured shows nibdex winning here.**

The deep-scan tail in this release raises how often the target is found *anywhere*
from 39.3% to 48.2%. It moves `hit@1` and `hit@3` by exactly nothing, at any depth
tried. That is not a disappointing result, it is a structural one: depth and ranking
are separate problems, and this release solved the one that was tractable.

**This is disclosed rather than deferred because the alternative was to keep not
shipping.** The work that makes ranking addressable — a replayable labelled set and a
scoring harness, so a change can be scored instead of argued about — landed days ago
and did not exist before. Ranking is the next lane, and it is being worked on. If you
are evaluating nibdex, `docs/LIMITATIONS.md` §2.y states the same thing with the full
numbers and the caveats, and tells you which questions the tool is currently good at.

### Upgrading from 0.2.0-rc.3

One migration is added, and it runs automatically at startup. **No reindex is
needed:** it creates an `fts5vocab` virtual table over the existing search index —
a view, not storage, always exactly as current as the index it reads. No new
column, so the additive-column wipe-reindex rule does not apply.

**Your index needs nothing.** No reindex, no rebuild, no wipe. Migrations are
compiled into the binary and run on every database open, so an index built by rc.3
is picked up as-is.

#### The steps

1. **Install.** `cargo install nibdex --version 0.2.0-rc.4`, or rebuild your
   checkout.
2. **Restart the daemon**, if you run one. On macOS use `launchctl bootout` then
   `bootstrap` — **`kickstart` does not pick up a replaced binary** and will leave
   you running the old build while reporting success (issue #5).
3. **Restart your MCP client.** A stdio session holds the binary image it started
   with, so an upgraded binary does not reach a session that is already open. This
   is the step people miss.
4. **Verify it took.** `check()` reports the `git_sha` of the build that is
   *running*; `nibdex version` reports what is on disk. If those disagree, step 2
   or step 3 did not happen. Checking both is the point — either one alone looks
   fine.

#### What changed in the response shape

Four new fields, three of them additive and omitted when they have nothing to say,
so a caller that ignores them sees no change:

| Field | On | Notes |
|---|---|---|
| `neighbourhood_terms` | `find_memory`, `find_design_doc` | omitted unless the broadened neighbourhood is larger |
| `retrieval_shape` | `find_memory`, `find_design_doc` | omitted with the above |
| `also_matched` | queries with a deep-scan tail | omitted when empty |
| `path` | `find_memory` hits | always present; new, not replacing anything |

**`body_excerpt` is the one that needs a caller-side change.** It is now absent
whenever `body` is populated. Read `body` first and treat `body_excerpt` as the
fallback for hits where `body` is empty and `body_truncated` is true. If you parse
responses programmatically and read `body_excerpt` unconditionally, this is the
only line in the release that will bite you.

#### Two notes on the mechanics

- **Building from a local checkout:** `cargo`'s fingerprint does not track
  `migrations/`. If a future release changes only that directory,
  `cargo install --path . --force` can report success without recompiling, leaving
  a binary that does not know about a migration already applied to the database.
  Touch any `.rs` file first. (Not a hazard for this release — source changed too,
  and an install from crates.io compiles fresh regardless.)
- **Downgrading is not a supported path** at this stage of the project, and is not
  tested. The index is a derived cache — if anything about a build looks wrong, the
  supported recovery is to delete the database and re-index rather than to go
  backwards.

### What we would like to hear

The vocabulary feature is the first thing in nibdex shipped without knowing
whether it helps. If you try it:

- Did the returned terms lead you to something the original query missed, or were
  they noise?
- Did they come back as your corpus's boilerplate rather than its register? That
  is the known weakness above, and a second corpus exhibiting it would justify
  building the real fix.
- Did it fire when you did not want it, or stay quiet when you did?

Issues [#17](https://github.com/gnuphie-labs/nibdex/issues/17) and
[#18](https://github.com/gnuphie-labs/nibdex/issues/18) are the right places, and
a report that it did not help is more useful than silence — a null result belongs
in `LIMITATIONS.md` and will be put there.

## 0.2.0-rc.3 — the hook becomes the front door; a schema corpus (2026-08-16)

### `nibdex hook` is documented as the main path, not an optional extra
It was listed last in the README under "Optional". On the author's own machine it
is where essentially all real use happens, and the reason is structural: an MCP
client can hold a tool's schema *deferred*, so reaching a query tool costs a
lookup call that `grep` never pays, while `Bash` and `Grep` are resident and
free. The README now leads with it, states that reason in one sentence, and
carries the full honest list of what it will and won't do — Claude Code only, a
`Bash` search must lead a pipeline segment, literal terms of 3+ characters,
regex declined silently, needs an index covering your cwd, only as current as
the last pass, fires on every `Bash` call, and logs search terms. New worked
example in `docs/EXAMPLES.md` Q7, reproducible from a clone.

### A sixth corpus: database schema
`nibdex schema-dump-query --dialect postgres|mssql` prints an introspection
query; save its output anywhere in the workspace as `*.nibdex-schema.json` and
the next `nibdex index` picks it up. Tables, views and functions, with columns,
types, widths, `NOT NULL` and defaults.

nibdex reads the file — it holds no credentials and opens no socket, so the
no-network posture is unchanged. The cost is that a dump is a snapshot; every
answer states its age and defers to the database on disagreement.

The payoff arrives through the hook, which now has a second intent: a shell
command running SQL gets the shape of the tables it names, attached to a call
you were already making. This is aimed at a measured cost — in one large
workspace, 62% of sessions spent calls doing nothing but re-deriving a schema,
and more failed on a guessed column name. The width half matters as much as the
name half: a value compared against a column too narrow to hold it returns zero
rows and reads as an empty result rather than a bug.

Introspection queries (`information_schema`, `sys.*`) are deliberately left
alone — answering those from our own cached introspection would be circular and
would shadow the live answer you went to the database for.

### `nibdex hook --stats`
Reads `~/.nibdex/hook-log.jsonl` and reports firings, the served / no-hits /
no-index split, the median hit count of served firings, a `refused` count for
queries the index could not run, and a separate block for the schema intent —
plus an `other` row naming any outcome the build does not recognise, so a
future one cannot go missing from the buckets while still counting toward the
total. The log already existed; nothing read it. A hook injection is not a tool call, so this and
`check().adoption`'s new `hook_deliveries` are the only views of that path.

`hook_deliveries` sits beside `nibdex_share_pct` rather than being folded into
it: a delivery rides on a search already counted elsewhere, and the two numbers
disagreeing is the finding, not a discrepancy to average away.

### Seven fixes to the hook, found by driving it rather than reading it

Every one of these survived a code review and a green suite, and every one shows
up the moment you actually run the binary.

- **`grep -e TERM path` searched for the PATH.** `-e` was treated as a flag whose
  value must be skipped so it is not mistaken for the pattern — but its value
  *is* the pattern, so the next bare argument was taken instead. The result was
  not a miss: it was a labelled, provenance-stamped, freshness-stamped answer to
  a question you did not ask, which is worse, because nothing prompts you to
  doubt it. `-f` (patterns from a file) is now declined outright rather than
  guessed at.
- **A term containing `.` or `::` returned nothing at all.** The hook's query
  path bound your term to FTS5 raw while every other caller sanitized first, so a
  filename, a Rust path, a method call or a version string met the bareword
  grammar and came back a syntax error — which the hook's fail-open then
  swallowed. Sanitizing now happens inside the query function, where no caller
  can forget it.
- **A refused query is now counted.** That failure was invisible partly because
  the query-error exit was the one outcome that wrote no log line, so `--stats`
  could not report what it never recorded. It appears as `refused`.
- **`--stats` and `hook_deliveries` were blind to the schema intent.** Every
  firing counted toward the total while only the three search outcomes were
  shown, so the percentages summed to under 100 with nothing explaining the gap,
  and a workspace answering on every `psql` call could still report
  `hook_deliveries: 0`. There is now a schema block, and an `other` row that
  names any outcome this build does not recognise.
- **A long line holding a non-ASCII character crashed the hook.** The hit body
  was cut with a byte index, which panics mid-character. Under the documented
  `2>/dev/null || true` wiring the panic is swallowed, so the answer is lost
  silently — and since the log line is written after the render, the loss never
  reached `--stats` either.
- **The schema age described our indexing run, not your dump.** It read the time
  the document was last indexed, which is refreshed on every pass, so a dump
  taken a month ago and re-indexed this morning reported "indexed today". It now
  reports the dump file's own timestamp, and says "taken", not "indexed".
- **A workspace holding a dump and no indexed source was undiscoverable.** The
  hook required a non-empty source corpus before a database counted as usable,
  which is precisely the workspace the schema intent exists for. Either corpus
  now qualifies.

### The SQL Server dump query emitted storage bytes as a column width

`sys.columns.max_length` is bytes and is populated for every type, so a dump
described `id` as `bigint(8)` and — the damaging one — an `nvarchar(50)` column
as `nvarchar(100)`, because a Unicode string stores two bytes per character. A
width is a fact you act on without re-checking, so a wrong one is worse than
none. The same query selected no column default at all, leaving that half of the
rendering dead on SQL Server while PostgreSQL carried it.

Both shared one cause worth stating plainly: that query had never been run against
a real SQL Server. The PostgreSQL side was verified against a real dump; the SQL
Server side against fixtures shaped like the PostgreSQL output, which cannot
disagree with it.

**It has now been run** — against two real SQL Server databases, with every width
compared against the server's own `INFORMATION_SCHEMA.CHARACTER_MAXIMUM_LENGTH`.
The old expression was wrong on **more than nine columns in ten**; the new one
matches on every column of both. Doing that turned up two further defects that no
amount of reading would have found:

- **Widths are decided on the BASE system type**, not the declared one. `sysname`
  is an alias for `nvarchar(128)` and reports 256 storage bytes under its own type
  name, so a rule matching type names left it — and every
  `CREATE TYPE … FROM nvarchar(n)` — with no width at all.
- **The query now sets `NOCOUNT`**, because `sqlcmd` otherwise appends
  `(N rows affected)` and the output is therefore not JSON. Anyone following the
  old instructions got a file that could not be indexed.

The PostgreSQL query needed no such repair, and the reason is the useful part: it
asks `information_schema` for `character_maximum_length` and `column_default`
rather than deriving them from storage bytes. Delegating beat computing.

### Upgrading from 0.2.0-rc.2

**Nothing is required.** Everything here is additive: no tool signature changed,
no existing output shape changed, and an index built by rc.2 keeps working.

If you want the new parts:

- **Schema corpus** — there is nothing to enable. Produce a dump
  (`nibdex schema-dump-query --help`) and run `nibdex index` once with the same
  `--workspace`/`--db` your setup already uses. The `index` summary prints a
  `schema:` line even when it finds none, so you can tell "no dump" from "not
  looking". A migration adds the `schema_objects` table and runs automatically
  at startup — but note that a **newer database than the binary serving it** is
  an error, so upgrade every nibdex on that machine (the daemon, `~/.cargo/bin`,
  and whatever path your hook invokes) rather than only one.
- **The dump is not watched.** Re-running the dump does not re-index it; run
  `nibdex index` after. Answers state the dump's age, so staleness is visible.
- **`hook --stats`** reads a log that only starts accumulating once the hook is
  wired, so an empty report on a fresh install is expected and says so.
- **rc.2's source-prune** could delete a nested repo's code from `find_code` on
  a commit to a container repo (a workspace root that is itself a repo, plus
  `--include-nested-repos`). Fixed here. After upgrading, run `nibdex index`
  once and confirm `check().indexer.documents.source` is unchanged.
- **Regenerate any SQL Server dump taken with an earlier build.** Its
  `nvarchar`/`nchar` widths are doubled and its fixed-width types carry a
  meaningless one, and re-indexing cannot repair that — the wrong number is in
  the file. Re-run `nibdex schema-dump-query --dialect mssql`, save over the old
  dump, then `nibdex index`. PostgreSQL dumps are unaffected.
- ⚠️ **The `sqlcmd` invocation changed, and the previous one never worked.** It
  was documented as `sqlcmd -h -1 -y 0 -W …`; sqlcmd rejects that outright —
  `-h` and `-W` are each mutually exclusive with `-y 0`, which is itself required
  or the JSON is silently truncated mid-object. Use what the query header now
  prints:

  ```bash
  sqlcmd -S SERVER -U USER -d YOURDB -y 0 -i this.sql | tr -d '\n' > db.nibdex-schema.json
  ```

  `sqlcmd` wraps the single long value across lines without altering it, so the
  `tr` is a lossless reassembly. The PostgreSQL recipe is unchanged and was
  re-verified against a live database.
- **The hook's log writer now serializes properly** instead of stripping
  characters from the recorded term. Lines written by earlier builds still parse;
  there is nothing to do.

## 0.2.0-rc.2 — cold-review fixes (2026-08-15)

Findings from an adversarial cold pre-publication review
(`docs/reviews/2026-08-15-rc1-cold-review-triage.md`, maintainer-internal), fixed here. Grouped by what a user would notice.

### Session→commit binding now works for the layout the quickstart produces
The capturing commit for a `find_session` / `recent_sessions` hit was resolved by
matching the transcript's `cwd` against `commit_entries.repo_path`. `cwd` is where
Claude was launched — with `--workspace ~/ws` and repos as subdirectories that is
the container, never a repo root — so edges in that layout could never bind. The
binding now resolves the edit's absolute target path against the repo roots the
db holds commits for (innermost containing repo wins), and uses the COMMITTER
date (`committed_at >= edited_at`, earliest wins) rather than the author date,
which rebase / cherry-pick / `--amend --date` preserve and which excluded or
mis-ordered rewritten commits. Stored edge metadata is unchanged; re-run
`nibdex index` and existing unbound edges pick up their commit via late binding.
`git_branch` was never a binding predicate and the docs no longer say it is.

### IP-domain isolation is no longer defeated by symlinks
A symlink under a labeled subdirectory pointing at another domain's tree — a
tracked file, `docs/`, a root-level `*.md`, or `CLAUDE.md` — was read through and
its content indexed into this domain's db. All four walks now admit a link only
if it resolves inside the anchor; the source extractor skips git-tracked symlinks
outright. The invariant test plants all four and asserts nothing lands.

### `check().adoption` counts only this workspace's sessions
`session_activity` was written for every transcript on the machine before the
workspace/domain scope was applied, so a scratch workspace reported other
workspaces' sessions and a `--domain` db received foreign session ids. Tallies
are now scoped exactly like edges (per tool-call `cwd`), and `first_seen` is
never stamped from a missing timestamp.

### The index tracks the tree instead of only growing
- Source files that leave the git index are pruned on the next pass (document,
  chunks, FTS); the `index` summary reports `pruned`.
- `check().orphans` gains `source_chunks` (chunks whose file is gone from disk).
  Additive field; `CHECK_SCHEMA_VERSION` unchanged. `metrics-export` buckets it
  like the other orphan counts.
- `check().extractors_last_run_ms` now includes `extract.source` and
  `extract.session_edges`.
- Commits rewritten out of history (amend / rebase / squash) still persist —
  documented in LIMITATIONS as a known over-report; not fixed here.

### Guardrails
- `nibdex mcp --db <missing>` refuses to start instead of creating an empty db
  that answered `corpus_empty: true` to everything.
- `nibdex serve` checks the loopback bind before opening the db or spawning the
  watcher (previously the watcher started, wrote state, then hung 2 s), and
  warns when a `.nibdex-domains.toml` is present (the watcher is not domain-aware).
- `index-sessions --rebuild` re-derives only the sessions the pass admits;
  it no longer wipes the whole table (a `--slug A --rebuild` erased slug B).
- Edits whose transcript line has no parseable timestamp are counted
  (`skipped: no timestamp`) instead of vanishing.
- `--memory-dir` is canonicalized, so a relative path no longer produces false
  `memory_entries` orphans from another cwd.
- `serve` / `watch` gained `--max-commits-per-repo`; the on-commit reindex
  previously used the built-in 50,000 cap regardless of the flag.

### `nibdex hook`
- Freshness is measured from the last indexing pass
  (`MAX(indexed_repos.last_indexed_at)`), not the db file's mtime — every MCP
  query touches the file, so "index current" was reporting query time.
- No `sqlite3` CLI dependency: probes use sqlx. The probe handle is opened
  read-write-without-create rather than read-only, because a WAL db with no
  `-shm` sidecar (any db after a clean close by another client) cannot be opened
  read-only at all and the hook was silently disabling itself.
- Documented in the README and SECURITY.md, including the
  `~/.nibdex/hook-log.jsonl` firing log (search term, outcome, hit count, db
  path; never content) and `NIBDEX_HOOK_OFF`.

### Design docs
Text before the first `#` heading, and headingless `.md` files, are now indexed
as a section with an empty `heading_path` (`line_start = 1`). Previously that
text was in no corpus.

### Documentation corrections
- README no longer says design-doc hits carry a provenance commit (they don't).
- EXAMPLES / DESIGN no longer claim `recent_commits(filter:…)` / `find_commit`
  search `files_changed` — the FTS body is the commit message only.
- Field docs for `body` (a ≤64-token match window, not the whole chunk/section)
  and `match_line` (the window's first line, not the match's own line) now say
  what is returned.
- SECURITY.md: `nibdex mcp` writes per-call latency rows; it does not re-index.
- LIMITATIONS: FTS5 syntax errors are actionable (not raw); git worktrees /
  submodules are not discovered; an unopenable `.git` fails the run; the source
  corpus reads the working tree at index time.

### Tests
Gates added for the clauses mutation testing found unguarded: `days` cutoffs,
`repo` on the filtered `recent_commits` path and on `find_commit`, bm25 and
recency ordering, the `MAX_LIMIT` clamp, a recursive schema-vs-emit check over
every tool's hit and zero-result responses plus `check()`, source-orphan
counting, pruning, `--rebuild` scoping, session-activity scoping, symlink
containment, container-layout binding, committer-date binding, design-doc
preambles, the relocator's quorum / distinctiveness / range clamp, `summarize`'s
word boundary, `check()`'s percentile window + error exclusion, per-corpus probe
table + clock pairing, the `last_cwd` fallback-rationale reset, and the watcher's
commit cap. Every new test was run against the mutation it exists to catch.

## 0.2.0-rc.1 — nibdex stops being silently wrong about its own state

Two changes with one theme: when nibdex hands back nothing, you should be able to
tell *why*.

### `nibdex index` now builds the session corpus

`find_session` and `recent_sessions` search edits recovered from your Claude Code
transcripts. Until now that corpus was populated only by a separate
`index-sessions` command — so following the README quickstart, running
`nibdex index`, and calling `find_session` returned an empty result, and nothing
said the corpus had simply never been built. The documented first experience did
not work.

`nibdex index` now builds it in the same pass, with nothing extra to run.

**What it reads is derived from `--workspace`, not chosen by a flag.** Your
sessions are spread across one transcript directory per directory you launched
Claude from, so no single directory covers a workspace. An edit is indexed only
when the session was working inside the workspace *and* the file it touched is
inside the workspace or in the built-in neutral set (this workspace's own Claude
directory and scratchpad). Both drop counts appear in the `index` summary.

Path matching is component-wise throughout, so a sibling directory sharing a name
prefix is never admitted.

⚠️ **This changes a default.** A plain `nibdex index` now reads your Claude Code
transcripts, where before it did not. If you were relying on the old behavior to
keep conversation rationale out of the index, point `--projects-dir` at an empty
directory. See [`SECURITY.md`](SECURITY.md).

**Edits keep acquiring their commit.** Each edge carries the commit that later
captured it, resolved when the edge is first indexed — but the natural order is
work, index, *then* commit, so at first index there is usually nothing to bind
to. Those edges now pick up their capturing commit on a later pass instead of
staying unbound forever. Without this, folding session indexing into `index`
would have made unbound the normal state for the provenance `find_session` leads
with.

A transcript that cannot be read — permissions, non-UTF-8 content, or one Claude
Code pruned while the scan was running — is skipped and counted, never fatal. The
transcript root is machine-global, so a pass legitimately encounters files it has
no claim on; none of them is a reason to fail an index run. If the session pass
fails outright, the other five corpora still index and the reason is reported.

`index-sessions` remains, and gained `--workspace-scoped` (the same derived scope,
for refreshing this corpus alone). `--slug` now accepts the leading-hyphen values
that real transcript directories have — previously only `--slug=<value>` parsed.

### Empty results say which kind of empty they are

A zero-result response now carries `corpus_empty`, and when the corpus is
non-empty, `corpus_indexed_through` — the newest item it holds:

```jsonc
{ "tool": "find_session", "total_matched": 0, "returned": 0, "results": [],
  "corpus_empty": true }
```

`true` means there was nothing to match, so the result describes the index rather
than your codebase. `false` plus a freshness stamp means the corpus has content
and this query missed it. On a response that *returns* results both fields are
absent — they exist only to explain an absence, and cost nothing on the hit path.

`corpus_indexed_through` means one thing across all five corpora: the newest item
the corpus *contains* — newest edit, newest commit, newest file modification —
never "when indexing last ran".

All five corpora report it, not just `find_session`.

### `check()` names retired corpora

`session_entries` (the old CLAUDE.md-format session corpus) is read by no query
tool. When it still holds rows, `check()` now lists it under `retired_corpora`
with what superseded it, so a non-zero count there is not mistaken for a damaged
index. Absent entirely when nothing is retired.

### `find_code` says which repo a hit is in, and can be scoped to one

`find_code` returned a repo-relative path — `src/main.rs` — with nothing saying
which repo that was. On a workspace holding more than one repo the result was not
openable, and the same path in two repos was indistinguishable. There was no way
to scope the search either.

Every hit now carries `repo_path`; open a result as `repo_path` + `path`. The new
optional `repo` argument narrows a search to one repo, taking the same repo string
`find_commit` and `recent_commits` accept.

**This needs a reindex, and an upgrade performs it for you.** Because `nibdex
index` skips files whose content has not changed, a plain reindex after upgrading
would leave the new field empty on every existing row — and a `repo` filter would
then match nothing on a perfectly good index. The upgrade therefore backfills the
field from data already stored, so existing indexes keep working without a
rebuild. A row that cannot be derived is left unset rather than guessed.

### Malformed queries explain themselves

`find_code("parse_config(")` failed with `error returned from database: (code: 1)
fts5: syntax error near ""`. Query arguments are FTS5 MATCH expressions, where
`(` is grouping syntax — but the error named no offending character and suggested
no repair, and an unhelpful failure is indistinguishable from a broken index.

All seven query tools now explain what was malformed and how to fix it (quote the
literal term: `"parse_config("`). Genuine database failures are passed through
unchanged, so the two remain distinguishable.

### Fixes

- **Every query tool violated its own declared `outputSchema`.** `query_broadened`
  was advertised as a required field but omitted whenever query broadening did not
  fire — nearly every response — so a client validating against the schema would
  reject almost all of them. Seven of eight tools were affected.
- **Commit freshness read the wrong date.** `corpus_indexed_through` for commits
  reported the author date, which is preserved across a rebase, so a freshly
  indexed corpus could report itself as months stale.

### Known limitation

The file watcher does not yet watch transcript directories, so a long-running
daemon serves whatever the last `nibdex index` captured. Re-run `nibdex index`,
or `nibdex index-sessions --workspace-scoped`, to catch up. Watching them live is
planned.

---

## 0.2.0-rc.0 — 2026-07-14

Transcript-backed `find_session`/`recent_sessions` replacing the CLAUDE.md-format
corpus, and the IP-domain partition — a per-domain database boundary for keeping
several clients' or employers' content apart on one machine. See
[`docs/IP_DOMAINS.md`](docs/IP_DOMAINS.md).

## 0.1.0-rc.0 — 2026-07-12

First public release. Five corpora, seven query tools plus `check()`, provenance
on code and doc hits, loopback-only MCP transport.
