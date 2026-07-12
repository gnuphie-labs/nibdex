# Examples

This is **real output from real queries** — captured by driving the MCP server over stdio and snapshotting the responses. Nothing here is estimated, hand-constructed, or redacted.

To keep every example reproducible, these queries were run against **nibdex's own repository** as the corpus (`nibdex index --workspace path/to/nibdex`). Clone nibdex, index it, and you'll see the same shapes against the same code — the ranks and line numbers will track whatever commit you're on. (Absolute paths below use `~/…` in place of a real home directory; the tool returns your actual absolute path.)

The corpus these queries ran against — nibdex indexing itself:

| Corpus | Count |
|---|---|
| `source_chunks` (57 source files) | 484 |
| `commit_entries` (1 repo) | 179 |
| `design_doc_sections` (18 docs) | 342 |
| `search_index` (FTS5) total | 1,005 |
| `session_entries` | 0 |
| `memory_entries` | 0 |

`session_entries` and `memory_entries` are **0 here on purpose**, and it's worth being honest about why: nibdex's own `CLAUDE.md` isn't written in the session-history format `find_session` parses (see the README), and there's no memory directory inside the repo. On a workspace that *does* have those, both corpora populate — but the three that carry a project's substance (`find_code`, `find_commit`, `find_design_doc`) work on any git repo with no setup. So the tour leads with `find_code`.

---

## Q1 — `find_code`: "Where is the loopback-bind enforcement?"

```jsonc
// Call
{ "tool": "find_code", "arguments": { "query": "loopback bind refused", "limit": 3 } }

// Response (top 2 of 3 returned; bodies truncated for readability)
{
  "tool": "find_code",
  "total_matched": 3,
  "returned": 3,
  "query_broadened": true,
  "results": [
    {
      "path": "src/http_server.rs",
      "line_start": 301, "line_end": 333, "match_line": 310,
      "language": "rust",
      "location": "verified",
      "commit_sha": "44dba99",
      "commit_summary": "chore: add SPDX-License-Identifier: MIT headers to all Rust sources",
      "rank": -10.51,
      "body": "…/// `serve` rejects non-loopback bind addresses per D-6.4.3.\n    #[tokio::test]\n    async fn serve_rejects_non_loopback_bind() -> Result<()> { …"
    },
    {
      "path": "src/http_server.rs",
      "line_start": 101, "line_end": 150, "match_line": 138,
      "language": "rust",
      "location": "verified",
      "commit_sha": "44dba99",
      "commit_summary": "chore: add SPDX-License-Identifier: MIT headers to all Rust sources",
      "rank": -9.92,
      "body": "…) -> Result<()> {\n    if !bind.ip().is_loopback() {\n        anyhow::bail!(\n            \"nibdex serve: bind address {bind} is not loopback. D-6.4.3 \\\n             requires 127.0.0.1 at MVP.\"\n        );\n    } …"
    }
    /* … 1 more */
  ]
}
```

**What you get.** Ranked source chunks, each with its `path`, `line_start`/`line_end`, a match-centered snippet, and — the part per-corpus tools can't give you in the same call — the **git commit that last touched that file** (`commit_sha` + summary), so retrieval and provenance arrive together. Here one query surfaced both the *enforcement* (`if !bind.ip().is_loopback() { … }`, line 138) and its *test* (line 310) — the exact code `SECURITY.md` describes.

Two honest notes on this output:
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
      "rank": -9.91
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

// Response (top hit)
{
  "tool": "find_design_doc",
  "total_matched": 5,
  "returned": 2,
  "results": [
    {
      "doc_path": "~/workspace/nibdex/docs/DESIGN.md",
      "heading_path": "nibdex — Design Document/8. Comparison & evaluation plan/8.4 Cost-savings measurement framework (the *what we measure and why*)",
      "line_start": 567, "line_end": 621, "match_line": 567,
      "rank": -13.60,
      "body": "### 8.4 Cost-savings measurement framework (the *what we measure and why*)\n\nWithout this framework, the §3 thesis (\"reduce drain on AI costs\") is unfalsifiable. …"
    }
    /* … 1 more section */
  ]
}
```

**What you get.** The section's full heading path (one level per `#`), exact line range, the body, and a match-centered start line — enough for an MCP client to show the heading inline or open the file right at the section. nibdex indexes every `#` section across every markdown doc it finds; there's no curated slice to author first.

---

## Q4 — `recent_commits`: "What changed recently?"

```jsonc
// Call
{ "tool": "recent_commits", "arguments": { "days": 30, "limit": 5 } }

// Response (top 3 shown, ordered by authored_at_unix DESC)
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

**What you get.** A recency view ordered by `authored_at` (no bm25 — `rank` is `null` on the `recent_*` path). Across a multi-repo workspace this is one envelope instead of one `git log` per repo; add `filter: "<file>"` to join against `files_changed` and get the *what* and the *why* for a specific file in one round-trip.

---

## Q5 — `find_session` on a repo without the format: an honest empty

```jsonc
// Call
{ "tool": "find_session", "arguments": { "query": "anything", "limit": 3 } }

// Response
{ "tool": "find_session", "total_matched": 0, "returned": 0, "results": [] }
```

This is not a failure — it's the honest result of pointing `find_session` at a repo whose `CLAUDE.md` isn't in the session-history format it parses (nibdex's own repo, here). An empty envelope comes back, never an error (per the "empty is not error" contract), so a client that wraps calls in retry-on-failure logic stays well-behaved. On a workspace with a session-formatted `CLAUDE.md`, the same call returns ranked entries with `files_touched` / `todos_mentioned` / `decisions_made`. A transcript-based session index that doesn't depend on the format is the Phase-1b plan — see the roadmap.

---

## `check()` — corpus census, perf, and honest cost figures

```jsonc
// Call
{ "tool": "check", "arguments": {} }

// Response (indexer + perf shown)
{
  "schema_version": 1,
  "indexer": {
    "source_chunks": 484,
    "commit_entries": 179,
    "design_doc_sections": 342,
    "documents": { "source": 57, "design_doc": 18, "session_history": 1 },
    "indexed_repos": 1,
    "memory_entries": 0,
    "session_entries": 0,
    "search_index_total": 1005
  },
  "orphans": { "source_chunks": 0, "design_doc_sections": 0, "memory_entries": 0, "session_entries": 0, "indexed_repos": 0 },
  "perf_p50_ms": { "tool.find_code": 3, "tool.find_commit": 2, "tool.find_design_doc": 4, "tool.find_session": 2, "tool.recent_commits": 1 },
  "shallow_repos": [],
  "build": { "git_sha": "…", "crate_version": "0.1.0-rc.0" }
}
```

`check()` is the one-call health surface: the corpus census, live per-tool latency percentiles (single-to-low-double-digit milliseconds warm), orphan counts (rows whose source file has since disappeared — all `0` here), any shallow repos, and the build provenance of the running binary. It also carries a `cost_savings` rollup (omitted above) when a metrics sink is enabled.

## Cost figures — real, and honestly estimated

With `--metrics-sink jsonl:<path>`, every call writes one event. These are the **real** figures from the five calls above (nibdex indexing itself is a small corpus, so the savings are modest — that's the point of showing a real run rather than a flattering one):

| Tool | tokens returned | tokens saved (p50) | $ saved | wall |
|---|---|---|---|---|
| `find_design_doc` | 476 | 17,524 | $0.053 | 4 ms |
| `find_code` | 812 | 11,188 | $0.034 | 5 ms |
| `find_commit` | 552 | 8,948 | $0.027 | 3 ms |
| `recent_commits` | 1,380 | 4,120 | $0.012 | 3 ms |
| `check` | 502 | 0 | $0.000 | 6 ms |

Every one of these carries `calibration_confidence: "estimated"`, and that word is load-bearing: `tokens_returned` and timings are **measured**, but `tokens_saved` is an *estimate* — nibdex's returned size differenced against a per-tool counterfactual anchor, not an A/B measurement of a real session with and without nibdex. The anchors are seeded in `calibration.toml` and priced at a Sonnet input rate; edit them for your model and workflow. Larger real-usage aggregates (from actual dogfooding) are in [`DESIGN.md`](DESIGN.md) §8.4, and the honesty caveats are in [`LIMITATIONS.md`](LIMITATIONS.md).

The verbatim JSONL event for the `find_code` call above:

```jsonc
{
  "schema_version": 1,
  "ts": "2026-07-11T…Z",
  "tool": "find_code",
  "query": "loopback bind refused",
  "params": { "limit_requested": 3, "query_len": 21 },
  "wall_ms": 5,
  "stages_ms": { "fts5_query": 3, "join": 1, "rank": 0, "shape_response": 0 },
  "candidate_count": { "fts5": 80, "after_rank": 3 },
  "result_token_estimate": 812,
  "returned_full_tokens": 1279,
  "calibration_confidence": "estimated",
  "counterfactual_tokens_p50": 12000,
  "tokens_saved_p50": 11188,
  "dollars_saved_p50_usd": 0.033564,
  "calibration_model_version": "v0.2-2026-06-05"
}
```

(The `query` field is written to your *local* sink verbatim; the separate, opt-in `nibdex metrics-export` reduces it to shape features before anything can be shared — see [`SECURITY.md`](../SECURITY.md) and DESIGN §5.6.)

---

None of the figures here are projections. They're what nibdex actually returned, on the day of capture, indexing its own source. Your ranks and counts will differ with your corpus and your commit — the *shapes* are what to expect.
