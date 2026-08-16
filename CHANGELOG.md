# Changelog

Notable changes per release. Versioning policy: [`docs/VERSIONING.md`](docs/VERSIONING.md).

## 0.2.0-rc.3 — the hook becomes the front door; a schema corpus (unreleased)

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
