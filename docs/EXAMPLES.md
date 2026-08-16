# Examples

Q1–Q4 and `check()` are **real output from real queries** — captured by driving the MCP server over stdio and snapshotting the responses; nothing there is estimated, hand-constructed, or redacted. Q5 (`find_session`) and Q6 (IP domains) are the exceptions and are **labeled as illustrative**: their corpora are your own machine's transcripts and directory layout, so they can't be reproduced from a clone — the shapes shown are representative, not a captured run.

To keep the reproducible examples reproducible, those queries were run against **nibdex's own repository** as the corpus (`nibdex index --workspace path/to/nibdex`). Clone nibdex, index it, and you'll see the same shapes against the same code — the ranks, counts, and line numbers will track whatever commit you're on. (Absolute paths below use `~/…` in place of a real home directory; the tool returns your actual absolute path.)

Captured on 0.2.0-rc.1 against the maintainer's full-history checkout (315 commits); the shapes are what to expect, the numbers track your checkout. **Two honest notes on reproducing from a public clone:** (1) `github.com/gnuphie-labs/nibdex` carries squashed release history (a handful of commits), so the commit-corpus figures — `commit_entries: 315`, Q2's hit, Q4's list — reflect the private development history and will be much smaller for you; `find_code`/`find_design_doc` (Q1, Q3, `check()` census) reproduce as shown. (2) On 0.2.0-rc.2 the Q1 query `loopback bind refused` no longer needs OR-broadening: rc.2 added a guard whose text contains all three words, so the strict AND matches once (`total_matched: 1`, `src/main.rs`, no `query_broadened`) — a small live illustration that the corpus is the code, and the code moved. `check().orphans` now also carries `source_chunks`.

The corpus these queries ran against — nibdex indexing itself:

| Corpus | Count |
|---|---|
| `source_chunks` (60 source files) | 630 |
| `commit_entries` (1 repo) | 315 |
| `design_doc_sections` (15 docs) | 217 |
| `search_index` (FTS5) total | 1,162 |
| `session_edges` | 0 |
| `memory_entries` | 0 |

`session_edges` and `memory_entries` are **0 here on purpose**, and the reason is worth being precise about: this capture indexes a *fresh clone* in a scratch directory, and nobody has ever run a Claude Code session in it. `nibdex index` does build the session→code map, but only from sessions that were working inside the workspace you point it at — so a throwaway clone correctly has none, while your own workspace populates. Likewise there's no memory directory inside the repo. The three corpora that carry a project's substance (`find_code`, `find_commit`, `find_design_doc`) work on any git repo with no setup, which is why the tour leads with `find_code`.

---

## Q1 — `find_code`: "Where is the loopback-bind enforcement?"

```jsonc
// Call
{ "tool": "find_code", "arguments": { "query": "loopback bind refused", "limit": 3 } }

// Response (top 2 of 3 returned; bodies truncated for readability)
{
  "tool": "find_code",
  "total_matched": 95,
  "returned": 3,
  "query_broadened": true,
  "results": [
    {
      "repo_path": "~/src/nibdex",
      "path": "src/http_server.rs",
      "line_start": 301, "line_end": 333, "match_line": 310,
      "language": "rust",
      "location": "verified",
      "line_shift": null,
      "commit_sha": "44dba992f50923008a9cb993aea2149ee26c6dd2",
      "commit_summary": "chore: add SPDX-License-Identifier: MIT headers to all Rust sources",
      "rank": -10.48,
      "body": "…/// `serve` rejects non-loopback bind addresses per D-6.4.3.\n    #[tokio::test]\n    async fn serve_rejects_non_loopback_bind() -> Result<()> { …"
    },
    {
      "repo_path": "~/src/nibdex",
      "path": "src/http_server.rs",
      "line_start": 101, "line_end": 150, "match_line": 138,
      "language": "rust",
      "location": "verified",
      "line_shift": null,
      "commit_sha": "44dba992f50923008a9cb993aea2149ee26c6dd2",
      "commit_summary": "chore: add SPDX-License-Identifier: MIT headers to all Rust sources",
      "rank": -9.64,
      "body": "…) -> Result<()> {\n    if !bind.ip().is_loopback() {\n        anyhow::bail!(\n            \"nibdex serve: bind address {bind} is not loopback. D-6.4.3 \\\n             requires 127.0.0.1 at MVP.\"\n        );\n    } …"
    }
    /* … 1 more */
  ]
}
```

**What you get.** Ranked source chunks, each with its `repo_path` + repo-relative `path`, `line_start`/`line_end`, a match-centered snippet, and — the part per-corpus tools can't give you in the same call — the **git commit that last touched that file** (`commit_sha` + summary), so retrieval and provenance arrive together. Here one query surfaced both the *enforcement* (`if !bind.ip().is_loopback() { … }`, line 138) and its *test* (line 310) — the exact code `SECURITY.md` describes.

Four honest notes on this output:
- **`total_matched: 95` against `returned: 3` is not a typo, and `query_broadened: true` is why.** No chunk contains all of *loopback*, *bind*, and *refused* — `refused` appears nowhere in the source — so the strict AND match found nothing, and nibdex retried the query OR-broadened rather than reporting an empty result. That widened net matches 95 chunks; you are seeing the top 3 by rank. Read `total_matched` as the size of the net, not as 95 good answers. When `query_broadened` is absent, the count means what you'd expect.
- **`repo_path` is what makes a hit openable.** `path` is repo-relative, because it joins to the commit corpus; open a result as `repo_path` + `path`. On an index spanning several repos this is also the only thing distinguishing one repo's `src/main.rs` from another's. Pass `repo` to search just one.
- **`location: "verified"`** means the file on disk still matches what was indexed, so `match_line` points at the live line. If you'd edited above that chunk since indexing, you'd see `"relocated"` with a `line_shift`, or `"stale"` if the passage was torn apart — nibdex tells you which rather than handing back a silently-wrong line number.
- **`commit_sha` is the commit that last *touched the file*** — here that's the sweeping SPDX-header commit, not the line's original author. Tracing a line to its true origin *through* refactors is a deeper capability (it exists in the spike tooling); the shipped `find_code` reports last-touch, honestly.

---

## Q2 — `find_commit`: "When did the git2 provenance walk land, and what did it touch?"

```jsonc
// Call
{ "tool": "find_commit", "arguments": { "query": "libgit2 provenance revwalk", "limit": 3 } }

// Response
{
  "tool": "find_commit",
  "total_matched": 1,
  "returned": 1,
  "results": [
    {
      "commit_hash": "0e206ec",
      "commit_hash_full": "0e206ec…",
      "message_summary": "port(cross-platform): git2 file/provenance walk + target-gated notify/git2",
      "files_changed": ["Cargo.toml", "src/indexer.rs", "src/source_index.rs"],
      "authored_at_iso": "2026-07-11T…Z",
      "is_shallow": false,
      "rank": -13.90
    }
  ]
}
```

**What you get.** Commits ranked by message-body relevance, each reified with full SHA + author + ISO timestamp + body + `files_changed`. `is_shallow: false` confirms the local clone has full history for that commit — the indexer marks shallow clones explicitly, so coverage is honestly bounded rather than silently partial. The counterfactual is `git log --all -S … | head`, then `git show` on each candidate, in the right repo — several commands across one tool, collapsed into one structured response.

---

## Q3 — `find_design_doc`: "What did the design doc decide about cost-savings measurement?"

```jsonc
// Call
{ "tool": "find_design_doc", "arguments": { "query": "cost savings measurement framework", "limit": 2 } }

// Response (both returned)
{
  "tool": "find_design_doc",
  "total_matched": 7,
  "returned": 2,
  "results": [
    {
      "doc_path": "~/src/nibdex/docs/EXAMPLES.md",
      "heading_path": "Examples/Q3 — `find_design_doc`: \"What did the design doc decide about cost-savings measurement?\"",
      "line_start": 97, "line_end": 124, "match_line": 97,
      "rank": -20.14,
      "body": "## Q3 — `find_design_doc`: \"What did the design doc decide about cost-savings measurement?\"\n\n```jsonc\n// Call\n{ \"tool\": \"find_design_doc\", …"
    },
    {
      "doc_path": "~/src/nibdex/docs/DESIGN.md",
      "heading_path": "nibdex — Design Document/8. Comparison & evaluation plan/8.4 Cost-savings measurement framework (the *what we measure and why*)",
      "line_start": 567, "line_end": 621, "match_line": 567,
      "rank": -13.80,
      "body": "### 8.4 Cost-savings measurement framework (the *what we measure and why*)\n\nWithout this framework, the §3 thesis (\"reduce drain on AI costs\") is unfalsifiable. …"
    }
  ]
}
```

**What you get.** The section's full heading path (one level per `#`), exact line range, the body, and a match-centered start line — enough for an MCP client to show the heading inline or open the file right at the section. nibdex indexes every `#` section across every markdown doc it finds; there's no curated slice to author first.

**And a real limitation, visible right here rather than described elsewhere.** The top hit is *this page* — the section you are reading — not the design decision it points to. This file's own heading repeats the query terms almost verbatim, and bm25 rewards that density, so the document *asking* the question outranks the document that *answered* it. The answer is still returned, one rank down. It is a genuine lexical-retrieval hazard and it will reproduce for you, because indexing nibdex indexes this file too: a page discussing a topic can outrank the page that decided it. Ask for more than one result when the top hit looks like a restatement of your question.

---

## Q4 — `recent_commits`: "What changed recently?"

```jsonc
// Call
{ "tool": "recent_commits", "arguments": { "days": 30, "limit": 5 } }

// Response (top 3 shown, ordered by authored_at_unix DESC)
// Captured 2026-07-11. Unlike the ranked queries above, a recency view cannot be
// pinned in a static document — run it yourself and you will see your own latest
// commits, not these.
{
  "tool": "recent_commits",
  "results": [
    { "commit_hash": "6455ea2", "authored_at_iso": "2026-07-11T…Z", "message_summary": "docs(readme): truth-sync — document find_code, re-point quickstart, humble voice (P1)" },
    { "commit_hash": "8afe832", "authored_at_iso": "2026-07-11T…Z", "message_summary": "docs(state): DESIGN.md trim DONE; resume-here -> README truth-sync" },
    { "commit_hash": "a2c4eaf", "authored_at_iso": "2026-07-11T…Z", "message_summary": "docs(design): trim DESIGN.md for publication — IP-clean, real evidence, honest voice" }
    /* … 2 more */
  ]
}
```

**What you get.** A recency view ordered by `authored_at` (no bm25 — `rank` is `null` on the `recent_*` path). Across a multi-repo workspace this is one envelope instead of one `git log` per repo. `filter` is an FTS5 expression over the commit **message** (summary + body) only — `files_changed` is returned on every hit but is not searchable yet, so `filter: "src/foo.rs"` finds commits whose message names the file, not every commit that touched it.

---

## Q5 — `find_session`: the session→code map *(illustrative — your-machine-specific)*

`find_session` and `recent_sessions` are the one pair that can't be reproduced from a clone. Their corpus is the **session→code map** — the `Edit`/`Write` actions nibdex recovers from *your* Claude Code transcripts (`~/.claude/projects/`). `nibdex index` builds it along with everything else, scoped to the sessions that were working inside your `--workspace`.

Against a database where nothing has been indexed yet, the call is an honest empty — never an error (the "empty is not error" contract holds, so retry-wrapping clients stay well-behaved) — and it says *which kind* of empty it is:

```jsonc
{ "tool": "find_session", "total_matched": 0, "returned": 0, "results": [], "corpus_empty": true }
```

`corpus_empty: true` means there was nothing to match, so the result is a statement about the index rather than about your history. A query that simply missed a populated corpus reports `"corpus_empty": false` alongside `"corpus_indexed_through"` — the newest edit the corpus holds — so you can tell "no such thing" from "not indexed yet" from "indexed, but stale". On a call that *returns* results both fields are absent; they exist only to explain an absence.

Once the workspace is indexed, the same query returns the actual edits, matched by their *rationale*. The shape a populated call returns (illustrative — yours reflects your own history):

```jsonc
// Call
{ "tool": "find_session", "arguments": { "query": "loopback enforcement", "limit": 1 } }

// Response (shape)
{
  "tool": "find_session",
  "total_matched": 4,
  "returned": 1,
  "results": [
    {
      "session_id": "…",
      "tool": "Edit",
      "file_path": "src/http_server.rs",
      "edited_at_iso": "2026-06-05T18:22:10Z",
      "rationale": "Refuse a non-loopback bind at startup rather than trusting config — the authorization model is filesystem access, so a public bind would be a footgun.",
      "commit_hash": "94a7901",
      "commit_summary": "loopback-only MCP transport",
      "rank": -3.11
    }
  ]
}
```

The match is on the rationale + path, so a *concept* query ("loopback enforcement") surfaces the edit by its reasoning, not just its filename — and each hit carries the commit that captured it. `recent_sessions` returns the same record shape, one representative row per session, most-recently-active first.

---

## Q6 — Separating IP domains: two clients, two databases *(illustrative)*

If more than one client's or employer's code lives in one workspace, label the top-level subdirectories and build one database per domain. Given a `.nibdex-domains.toml` at the workspace root:

```toml
[domains]
personal = ["my-app", "my-lib"]
client-a = ["acme-api", "acme-web"]
```

```bash
nibdex index          --domain client-a --workspace ~/ws --db ~/client-a.db
nibdex index-sessions --domain client-a --all-slugs --workspace ~/ws --db ~/client-a.db
```

The session pass reports what it routed and what it held back (numbers illustrative):

```
Indexed 1204 write-edge(s) ... from 6 session(s)
  domain [client-a]: 2310 edge(s) dropped (foreign target), 41 rationale(s) withheld (cross-domain session)
```

- **dropped (foreign target)** — edits under `my-app`/`my-lib` (or unlabeled dirs), correctly left out of `client-a.db`.
- **withheld** — edits kept, but with their rationale withheld because the session had also touched another domain's, or an unlabeled subdirectory's, tree (the ratchet — see [`IP_DOMAINS.md`](IP_DOMAINS.md)). A small set of domain-less locations — workspace-root files, this workspace's own `~/.claude` slug, its own temp scratch — does *not* trip it.

Note what a domain database also *omits*: the memory corpus (unless that domain claims it via `[memory]`) and workspace-root files (unless moved into a labeled subdir that is also its own repo). Both are silent — see [`IP_DOMAINS.md`](IP_DOMAINS.md#two-things-that-need-a-convention).

`nibdex mcp --db ~/client-a.db` then answers only from client-a's content: a `find_code` or `find_session` here can't surface `my-app` code, commits, or reasoning, because none of it is in this database file — not filtered out, structurally absent. The full mechanism and honest limits are in [`IP_DOMAINS.md`](IP_DOMAINS.md).

---

## `check()` — corpus census, perf, and honest cost figures

```jsonc
// Call
{ "tool": "check", "arguments": {} }

// Response (indexer + perf shown; cost_savings, adoption, file_watcher omitted)
{
  "schema_version": 1,
  "indexer": {
    "source_chunks": 630,
    "commit_entries": 315,
    "design_doc_sections": 217,
    "documents": { "source": 60, "design_doc": 15 },
    "indexed_repos": 1,
    "memory_entries": 0,
    "session_entries": 0,
    "session_edges": 0,
    "search_index_total": 1162
  },
  "orphans": { "design_doc_sections": 0, "memory_entries": 0, "session_entries": 0, "source_chunks": 0, "indexed_repos": 0 },
  "perf_p50_ms": { "tool.find_code": 6, "tool.find_commit": 3, "tool.find_design_doc": 4, "tool.recent_commits": 4 },
  "shallow_repos": [],
  "build": { "git_sha": "…", "crate_version": "0.2.0-rc.1" }
}
```

`check()` is the one-call health surface: the corpus census, live per-tool latency percentiles (single-to-low-double-digit milliseconds warm), orphan counts (rows whose source file has since disappeared — all `0` here), any shallow repos, and the build provenance of the running binary. It also carries a `cost_savings` rollup and an `adoption` block (both omitted above) when a metrics sink is enabled.

Two absences above are themselves the point. `perf_p50_ms` lists only the four tools this capture actually called — percentiles are measured, never assumed, so a tool you have not exercised simply is not there. And `retired_corpora` is absent entirely rather than empty: it appears only when a superseded corpus still holds rows, so a clean index says nothing instead of showing you an empty list to interpret.

## Cost figures — real, and honestly estimated

With `--metrics-sink jsonl:<path>`, every call writes one event. These are the **real** figures from the five calls above (nibdex indexing itself is a small corpus, so the savings are modest — that's the point of showing a real run rather than a flattering one):

| Tool | tokens returned | tokens saved (p50) | $ saved | wall |
|---|---|---|---|---|
| `find_design_doc` | 563 | 17,437 | $0.052 | 4 ms |
| `find_code` | 906 | 11,094 | $0.033 | 5 ms |
| `find_commit` | 572 | 8,928 | $0.027 | 3 ms |
| `recent_commits` | 2,742 | 2,758 | $0.008 | 3 ms |
| `check` | 654 | 0 | $0.000 | 6 ms |

Every one of these carries `calibration_confidence: "estimated"`, and that word is load-bearing: `tokens_returned` and timings are **measured**, but `tokens_saved` is an *estimate* — nibdex's returned size differenced against a per-tool counterfactual anchor, not an A/B measurement of a real session with and without nibdex. The anchors are seeded in `calibration.toml` and priced at a Sonnet input rate; edit them for your model and workflow. Larger real-usage aggregates (from actual dogfooding) are in [`DESIGN.md`](DESIGN.md) §8.4, and the honesty caveats are in [`LIMITATIONS.md`](LIMITATIONS.md).

The verbatim JSONL event for the `find_code` call above:

```jsonc
{
  "schema_version": 1,
  "ts": "2026-08-15T…Z",
  "tool": "find_code",
  "query": "loopback bind refused",
  "params": { "limit_requested": 3, "query_len": 21 },
  "wall_ms": 5,
  "stages_ms": { "fts5_query": 3, "join": 1, "rank": 0, "shape_response": 0 },
  "candidate_count": { "fts5": 95, "after_rank": 3 },
  "result_token_estimate": 906,
  "returned_full_tokens": 1279,
  "query_broadened": true,
  "calibration_confidence": "estimated",
  "counterfactual_tokens_p50": 12000,
  "counterfactual_tokens_p95": 30000,
  "tokens_saved_p50": 11094,
  "dollars_saved_p50_usd": 0.033282,
  "counterfactual_wall_ms_p50": 6000,
  "calibration_model_version": "v0.2-2026-06-05"
}
```

(The `query` field is written to your *local* sink verbatim; the separate, opt-in `nibdex metrics-export` reduces it to shape features before anything can be shared — see [`SECURITY.md`](../SECURITY.md) and DESIGN §5.6.)

---

None of the figures here are projections. They're what nibdex actually returned, on the day of capture, indexing its own source. Your ranks and counts will differ with your corpus and your commit — the *shapes* are what to expect.
