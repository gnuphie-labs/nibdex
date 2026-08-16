# Changelog

Notable changes per release. Versioning policy: [`docs/VERSIONING.md`](docs/VERSIONING.md).

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
