# Query quality — the failure that survives adoption

**Status:** diagnosis, 2026-08-21. Publishable: the case below is deliberately
abstracted — no corpus contents, document titles, or third-party material.

Companion to [`ADOPTION.md`](ADOPTION.md) (why nibdex goes unused) and
[`TOOL_SURFACE_DESIGN.md`](TOOL_SURFACE_DESIGN.md) (what to build instead).

> **The claim in one line:** `ADOPTION.md` diagnoses *never invoked*. This
> document is about what happens **after** that is fixed — an assistant that
> invokes nibdex, asks one badly-worded question, receives ten plausible hits,
> concludes the corpus is empty, and leaves for `grep`. **Current instrumentation
> cannot see this at all**, because from every angle it looks like a success.

---

## 1. The new failure mode

`ADOPTION.md`'s measurement was **0 / 12 sessions**, 0.0%. The current
`check().adoption` on the work corpus reads **4.2%** — 120 nibdex queries against
2,741 retrievals elsewhere over 104 sessions. Adoption moved off the floor.

**And the first well-documented post-adoption failure is a different animal.**
Observed 2026-08-21 during a multi-hour historical investigation on the work
corpus:

1. `find_memory` was queried. **It worked** — returned the single most valuable
   artifact of the investigation on the first try.
2. `find_design_doc` was queried **once**, using vocabulary drawn from the
   assistant's own working hypothesis: schema identifiers, table names, and an
   internal finding-code.
3. It returned ten superficially reasonable sections. **None was the answer.**
4. The assistant concluded the archive did not contain the material, fell back to
   `grep` and `git log`, and **for the remainder of the session repeatedly
   reported "the record does not support this"** — to a human who knew perfectly
   well that it did.
5. The material was there the whole time. Re-querying afterwards **in the register
   the documents were actually written in** — blunt human phrasing: complaints,
   judgements, plain verbs — returned the decisive sections at **ranks 1–4 in
   51 ms**, including one document the assistant never opened at all.

**The corpus was complete. The daemon was healthy. The index was current. The
hook fired. The rule was in the project instructions in bold.** None of that
helped, because none of it operates after the query is sent.

---

## 2. Why the existing signals cannot see it

`nibdex hook --stats` reports a three-way split — `served` / `no_hits` /
`no_index` — and a high `no_hits` share is treated as the actionable signal.

🔴 **The failure above scores as `served`.** Ten hits came back. By every counter
nibdex maintains, that query succeeded.

| Signal | What it sees | Saw today's failure? |
|---|---|---|
| `no_hits` share | zero-result queries | **No** — ten results returned |
| `no_index` | daemon/index missing | **No** — index healthy, 31,575 sections |
| `check().adoption` | queries vs. retrieval elsewhere | **Yes, but only in aggregate, and only if someone calls `check()`** |
| p50/p95 latency | speed | **No** — 51 ms |

⇒ **The instrumented failure is "the index had nothing." The expensive failure is
"the index had it and the question was wrong."** Only the first is measured, and
it is the rarer of the two once a corpus is populated.

⚠️ **Do not read this as "the hook stats are wrong."** They measure the hook,
which is a different surface with a different job. The gap is that the **MCP tool
responses carry no equivalent at all** — they return results and nothing about
whether those results are any good.

---

## 3. What the response already knows and does not say

Every `find_*` response already contains the makings of the signal:

- **`rank`** — bm25, per hit. In the observed case the failed query's top rank and
  the successful query's top rank differed by **≈ 17.5**. The failure was legible
  in the response, in a field the caller received, and nothing interpreted it.
- **`total_matched`** — the failed query matched a modest set; a broadened form of
  the correct query matched **67**. A large `total_matched` against a weak top
  rank is close to a signature for *wrong register, right corpus*.
- **Distribution shape** — the successful query's hits clustered hard at the top
  (−26.9, −24.7, −23.6, −23.2); the failed one was flat. **A flat rank profile
  means "nothing here is much better than anything else,"** which is what a
  vocabulary miss looks like.

**The caller cannot compute any of this**, because bm25 ranks are not comparable
across corpora or query shapes without a baseline the caller does not have.
**nibdex has that baseline.**

---

## 4. Three proposals, cheapest first

### 4.1 Make a weak query announce itself

Add an advisory field to `find_*` responses:

```jsonc
"retrieval_quality": {
  "verdict": "weak",            // strong | mixed | weak
  "why": "top rank -11.2 is weak for this corpus; profile is flat (spread 1.4)",
  "hint": "67 sections match a broadened form of this query — try different wording"
}
```

**Cost:** small. Ranks and `total_matched` are already computed; the corpus-level
baseline is a percentile table refreshed per indexing pass.

⚠️ **The verdict must be calibrated per corpus, not hardcoded.** A −11 top rank
is weak in a 31,575-section corpus and unremarkable in a small one. **A
mis-calibrated advisory is worse than none** — it teaches callers to ignore the
field, which is harder to undo than adding it late.

### 4.2 Hand back the vocabulary the caller lacks

The root cause was not laziness. **The assistant could not guess the words a human
wrote in the moment.** Documents are written in the register of a person and an
occasion; identifiers, codes and table names are vocabulary imposed later, by the
reader.

Return the distinctive terms of the matched neighbourhood:

```jsonc
"neighbourhood_terms": ["<high-tf-idf terms from the matched region>"]
```

One re-query, in the corpus's own words, instead of an abandoned search.
**nibdex knows these terms; the caller structurally cannot.**

⚠️ **Open question — the strongest objection to this proposal.** Terms drawn from
*hits the query already found* may simply reinforce the wrong neighbourhood.
Expanding from a slightly-broadened query is more likely to help and costs a
second internal search. **Untested. Do not build before measuring which variant
actually recovers a known-missed document.**

### 4.3 Surface the abandonment signal *in the flow*

nibdex already counts queries against `retrieval_elsewhere` per session. In the
observed case it could have known, at the time, that the session had made **one**
`find_design_doc` call and then a long run of shell searches.

**That number lives in `check()`, which nobody calls mid-investigation.**

Proposal: when a session's pattern matches *(few indexed queries) + (rising
external retrieval)*, say so — on the next tool response, or via the hook:

```
nibdex: 1 indexed query this session, 14 external searches since.
        The last one ranked weak. Consider re-querying in different wording.
```

🔑 **This is the one that matters most, and the argument for it is not a nibdex
argument.** A separate estate measured what happens to inference delivered as a
*destination* versus as a *step inside an existing action*: an assistant surface
nobody visited logged **0 events in 30 days**; a generated-insights table reached
**7,630 rows with 0 ever acknowledged**; a dedicated review page recorded **0 page
views, ever**. The same class of inference, placed *inside* an action people were
already performing, immediately surfaced four real defects nobody had found.

**`check()` is a destination.** A retrieval-share counter that only exists behind a
health endpoint is an instrument firing at nobody — the exact pattern that
produced 7,630 unread rows. **Attached to the response, it is a step.**

⚠️ **And the discipline that has to come with it: something must decay or suppress
the notice once acknowledged.** In the estate above, the reason 7,630 insights
became worthless was that suppression was gated on acknowledgement that never
happened, so a **67,427-occurrence false positive outranked real findings by
seniority**. A nag that fires every session, forever, will be filtered out and
will have made things worse.

---

## 5. What would falsify this

This document argues from **one well-documented case** plus counters that were
never designed to detect it. That is thin, and the honest response is to measure
before building:

1. **Replay the work corpus for the signature** — sessions with ≥1 indexed query
   followed by ≥5 external searches and no further indexed query. **If that shape
   is rare, §4.3 is not worth building.** `dogfood/nibdex-adoption.py` already
   parses what is needed.
2. **Test the rank heuristic against known outcomes** — take queries whose target
   document is known, and check whether "weak top rank + high `total_matched` +
   flat profile" actually separates misses from hits. ⚠️ **A threshold tuned on
   known positives measures the base rate, not the signal.** Hold out a set.
3. **Test §4.2's two expansion variants** against known-missed documents. If
   neither recovers the target, drop it — a plausible mechanism is not evidence.

⚠️ **`ADOPTION.md`'s own history is the warning here: its central estimate moved
4% → 9% → 61% under scrutiny, because the classifier was wrong twice in opposite
directions.** Anything in this document that survives only as an argument, and not
as a measurement, should be treated the same way.

---

## 6. Relationship to the existing backlog

- **`ADOPTION.md`** — *never invoked*. Still the larger problem: 4.2% means ~96%
  of retrieval never reaches nibdex. **This document does not compete with that
  and should not displace it.**
- **`TOOL_SURFACE_DESIGN.md`** — better verbs, so intent maps to a tool. Reduces
  §4.2's problem at the *tool-choice* layer; does not touch query *wording*.
- **This document** — what remains once a caller does invoke the right tool and
  still walks away empty.

🔑 **The order is deliberate: fixing query quality first would be optimising a path
19 callers in 20 never take.** The value here is that it is cheap, it is mostly
additive fields on responses already being sent, and — unlike adoption — **it has
a concrete reproduction to test against.**

---

## 6a. A/B result — `neighbourhood_terms` is invisible to a rank metric (2026-08-23)

Measured, not argued. `dogfood/score-labelled.py` replayed the 416-row labelled set
against two builds on **one database snapshot** (91,025 search-index rows, identical
in both arms — the arms differ only in binary):

| | pre-feature `058ad23` | current `ff09c95` |
|---|---|---|
| scored rows | 168 | 168 |
| found the read file | 66 (39.3%) | 66 (39.3%) |
| hit@1 | 14.9% | 14.9% |
| hit@3 | 25.6% | 25.6% |
| MRR | 0.218 | 0.218 |

**Per-row: 0 of 166 rows changed rank.** The null is exact, and it is a fact about
the *instrument*, not about the feature. `neighbourhood_terms` is additive — the
vocabulary comes from a separate SELECT and `rank_span` derives shape from results
that already exist. Nothing reorders. **A rank metric therefore cannot detect this
feature, and reporting "no improvement" from it would be a false negative.**

🔴 **The reach ceiling, which is the more useful half.** Of the 168 scored rows,
**69.6% were answered by `find_code`, where the feature is not wired at all** (§4.2
records that omission deliberately: the COUNT carries the repo filter's extra binds,
and filling `neighbourhood_matched` with `total` would state a fact nobody checked).
Of the **102 misses** — the population any recovery mechanism must act on —
`neighbourhood_terms` can even *see* **33 (32.4%)**; the other **69 are `find_code`**.
So the feature is structurally blind to two thirds of the failures it exists to fix.

▶ **THE INSTRUMENT THAT WOULD ACTUALLY MEASURE IT — a two-stage recovery replay.**
The feature's claim is not *better first-query rank*; it is *a caller who missed can
recover without leaving*. That is directly testable offline against these same
labels, with no adoption required:

1. Stage 1 — replay the original query. Keep only the **misses** (the read file is
   absent from the results). That is 33 reachable rows today.
2. Stage 2 — take the `neighbourhood_terms` the response actually handed back,
   re-query with them, and ask whether the read file now appears, and at what rank.

The measured quantity is **recovery rate: of the queries that missed, what fraction
does the offered vocabulary rescue?** ⚠️ It is an **upper bound on value, not a
prediction of it** — it assumes a caller who uses the terms, and whether real callers
do is a separate question this cannot answer. Say so wherever the number is quoted.

⚠️ **And a caution the first arm nearly buried: the pre-feature binary REFUSED the
snapshot** — `migration 20260822000001 was previously applied but is missing in the
resolved migrations`. A newer db than the binary serving it, exactly as the deploy
runbook warns. The vocab migration had to be dropped from the snapshot (an
`fts5vocab` view, zero storage; `search_index` verified unchanged at 91,025 rows
either side) before the old arm would run at all. **Any future A/B across a migration
boundary hits this**, and the failure mode is a dead process, not a wrong number.

## 6b. Recovery replay — the rescue rate is 1 in 14, and the funnel is the finding (2026-08-23)

`score-labelled.py --recovery` implements §6a's two-stage design: replay each labelled
question, keep the **misses**, re-query with the `neighbourhood_terms` the response
actually handed back, and ask whether the file the session opened now appears.

```
stage-1 scored            168
  found the file           66
  MISSED                  102
    unreachable (find_code, field not wired)   69
    reachable                                  33
      no terms offered (emit gate held)        19
      terms offered                            14
        rescued, terms only                     1  (7.1%)
        rescued, augmented                      0  (0.0%)
```

**End to end: 1 of 102 misses, under an assumption generous to the feature.** The one
rescue landed at rank 10. ⚠️ Upper bound, not a prediction — it assumes a caller who
reads the terms and re-queries with them.

🔴 **THE FUNNEL IS THE RESULT, NOT THE RATIO.** The feature reaches 14 of 102 failures
before its quality is even in question. Two thirds are `find_code`, where the field is
not wired; of what remains, the emit gate correctly withholds on 19 (the broadened
neighbourhood was not larger, so there was nothing honest to offer). **Improving the
term-selection would move a number whose denominator is 14.** Wiring `find_code` moves
one whose denominator is 69.

🟢 **THE BOILERPLATE WEAKNESS IS NOW MEASURED, NOT SPECULATED — and it is the clearest
improvement on the board.** `find_memory` offered `apply` · `reference` · `related` ·
`feedback` · `session` · `keep` · `own` · `canonical` · `never` · `shape` across its
rows: **those are the memory-file TEMPLATE's own words** (`**Why:**`, `**How to
apply:**`, `Related: [[…]]`), not the register of any document. Three of four
`find_memory` cases are polluted this way. Index-wide IDF cannot see a term that is
ubiquitous *inside one corpus* but unremarkable across all of them — exactly the
defect LIMITATIONS §2 records. **Corpus-scoped IDF now has evidence behind it.**

🔴 **`augmented` scored WORSE than `terms_only` (0 vs 1) — do not ship "add these terms
to your query" as the advice.** OR-ing the original wording back in re-admits the
original wrong-register hits, which outrank the newly-reachable ones and push the
target out of the window. If the feature ever grows a suggested action, it must be
*replace your query*, not *extend it*.

⚠️ **THE LABEL CARRIES NOISE, AND IT INFLATES THE MISS COUNT.** The label is "the file
the session read next", which is not always the answer to the query: one row searched
`PO Date` and then opened `palette.md`. Several others wanted `BUG_TRIAGE.md` or
`CLAUDE.md` — files so large that "the session opened it" says little about which
question it answered. **So 102 is an upper bound on real retrieval failures**, and any
rate computed against it is a floor. Separating "read because the search found it"
from "read next for an unrelated reason" is unbuilt, and is the same silent-success
problem UPTAKE_STABILITY_DESIGN §6 item 15 names from the other direction.

## 6c. 🔴 The reach ceiling is the SINGLE-TERM QUERY, not the `find_code` gap (2026-08-23)

§6b closed by recommending `find_code` be wired, on the grounds that it would move a
denominator of 69 rather than 14. **That recommendation was wrong, and it was wrong in
this document's own favourite way: it compared a RAW denominator against an EFFECTIVE
one.** Retracted here rather than quietly dropped.

Measured against the labelled set:

| `find_code` misses | 69 |
|---|---|
| structurally unreachable (`.png`, `.output`, scratchpad artefacts — nothing indexes these, nor should it) | −12 |
| addressable | 57 |
| **single-term queries, where the gate CANNOT fire** | **−31 (54%)** |
| where the gate could fire at all | **26** |

The gate fired on 14 of 33 reachable design/memory cases (42%). Applying that rate and
then §6b's observed 7.1% rescue: **wiring `find_code` buys roughly ONE additional
rescue** — doubling a rate of 1%, for the real cost §4.2 describes. Not a clear win.

🔴 **THE STRUCTURAL FINDING, which is worth more than the recommendation was.**
`neighbourhood_terms` cannot fire on a single-term query. This is not a bug and not an
oversight — `neighbourhood.rs` step 1 says it outright: *"No broadened form (single
term, or deliberate FTS5 syntax the caller meant precisely) ⇒ nothing to say."* And
single-term queries are **60% of every query in the labelled set**:

| tool | single-term share |
|---|---|
| `find_code` | 78 of 117 (67%) |
| `find_design_doc` | 19 of 39 (49%) |
| `find_memory` | 4 of 12 (33%) |
| **all** | **101 of 168 (60%)** |

⇒ **The ceiling is not a wiring gap on one tool. The most common query shape in the
corpus is the one shape the feature can never help, on ANY tool, by design.** The gate
uses *"OR-broadening matched more"* as its proxy for *"there is more here than you
reached"* — and that proxy is structurally unavailable exactly where callers most often
are. Wiring `find_code` inherits this ceiling rather than lifting it: two thirds of its
traffic is single-term.

▶ **The real question is therefore *what does a single-term miss deserve?*, and it must
not be answered on momentum.** It is also the shape most likely to be a genuine register
mismatch — one word, nothing useful back, and no idea what the corpus calls the thing.
Any answer needs a different signal than OR-broadening, because there is nothing to
broaden. **Do not build until that signal is named and measured** — §4.1's warning
applies with full force: a mis-calibrated advisory teaches callers to ignore the field,
and that is harder to undo than adding one late.

## 6d. The deep-scan tail — BUILT (2026-08-23)

§6c left the question *what does a single-term miss deserve?* open. The answer turned
out not to be a cleverer suggestion. Measured on the labelled set: **52 of 53
single-term misses used words the index already contained.** Only one query
(`migrat`, a hand-truncated stem) was genuinely absent. Callers are not asking in the
wrong register — **the right document exists, uses their words, and sits below the
window.**

Widening the window confirms it, and isolates where the problem is NOT:

| window | found | hit@1 | hit@3 |
|---|---|---|---|
| 10 | 39.3% | 14.9% | 25.6% |
| 25 | 45.8% | 14.9% | 25.6% |
| 50 | 48.2% | 14.9% | 25.6% |

**`hit@1` and `hit@3` do not move at all.** Depth is orthogonal to the head of the
ranking. The 15 rows recovered between 10 and 50 sit at ranks 11–37, median 19.

🔑 **THE RULE, and it needs no intelligence, no threshold and nothing to calibrate:
SCAN DEEPER THAN YOU RENDER.** nibdex now scans to `DEEP_SCAN_DEPTH` (40, the depth
that recovers all 15) and renders `limit`. Everything between comes back as
`also_matched` — **one pointer per FILE**, deduped, carrying the best match line and a
count, never a body.

🔴 **A PREDICTOR WAS TRIED FIRST AND THERE ISN'T ONE — recorded so nobody rebuilds it.**
bm25 spread runs 2.916 on misses vs 4.075 on hits; window saturation 76% vs 67%. Both
directional, both overlapping far too heavily to gate on. **But no predictor is needed**,
because the costs are asymmetric (DESIGN §3.1): query latency is nearly free, caller
bytes are not. So never decide *whether* to look deeper — always look, and control cost
at the **rendering** end. That also sidesteps §4.1 entirely: there is no advisory to
mis-calibrate and nothing to teach callers to ignore.

🟢 **MEASURED AFTER BUILDING, on the same labelled set:**

- of the 102 misses, the tail points at **15 (14.7%)**
- **answered somehow (body or pointer): 48.2%, up from 39.3%**
- **that is exactly the limit-50 recall, at the byte cost of rendering 10**

⚠️ **A pointer is weaker than a body and is counted separately — never folded into
`hit@k`.** The headline retrieval numbers are unchanged and are supposed to be.

**Byte accounting, driven on the real binary:** `find_design_doc("hysteresis")` renders
10 bodies (4,064 chars) and 6 tail pointers (~715 chars) — the tail is **15% of
payload**, against the **~21%** freed the same day by dropping the redundant
`body_excerpt` (`ff09c95`). **Responses are still net smaller than they were that
morning.** Dedupe-by-file is what makes that true; ranks 11–40 are frequently several
chunks of one document.

▶ **What this does NOT fix, stated plainly: the head of the ranking.** `hit@1` is 14.9%
against the shell's 28%. Every gain here is in the tail. **That is the remaining work,
and it is the thing a caller notices first.**

## 7. As built — 2026-08-22

**§4.2 and the fact-half of §4.1 are shipped** on `find_memory` and `find_design_doc`
(`src/mcp/neighbourhood.rs`, migration `20260822000001_search_index_vocab.sql`).

**What went in:**

- `neighbourhood_terms` — up to 8 distinctive terms, drawn from the **OR-broadened**
  neighbourhood, never from the caller's own hits. §4.2 named that objection and this
  is the variant it says to prefer.
- `retrieval_shape` — `top_rank`, `rank_spread`, `neighbourhood_matched`. **Facts, no
  verdict.** §4.1's `strong|mixed|weak` was deliberately NOT built: it needs a
  per-corpus percentile table, and a mis-calibrated advisory is worse than none.
- The gate is a fact too: **terms appear only when the broadened neighbourhood is
  strictly larger than what the query matched** — *there is more here than you
  reached*. No threshold, no calibration.

**Verified against nibdex's own corpus, and reproducible from a clone** — index this
repository (`nibdex index --workspace <clone> --db <scratch>`), then drive
`nibdex mcp --db <scratch>`:

| Query (identifier register) | matched | neighbourhood | terms returned |
|---|---|---|---|
| `search_index rowid_ref source_table` | 12 | **58** | `upsert` · `incremental` · `indexer` · `joined` · `notify` · `oid` |
| `bm25 rank threshold calibration` | 5 | **180** | `heuristic` · `queries` · `response` · `filter` · `fts5` |
| `launchd plist bootstrap` | 1 | **28** | `systemd` · `stdio` · `discovery` · `production` |

The first row is the shape §1 describes: a query written in schema identifiers reaches
a twelfth of the region it is aimed at, and the words that would have reached the rest
are the prose the design is actually written in — `upsert`, `incremental`, `indexer` —
which the caller has no way to guess from a table name.

The third row is the same mechanism at its most compact: one hit, and the neighbourhood
hands back **`systemd`** — the sibling concept the query never mentioned and the caller
would not have thought to ask for.

**One defect found by running it, and fixed:** index-wide IDF cannot see corpus-local
boilerplate, so the memory corpus returned its own template's words. A local ceiling
ships; the corpus-scoped background sample that would fix it properly does not. See
`LIMITATIONS.md` §2.

**Not built, deliberately:** `find_code` (its COUNT carries the repo filter's extra
binds, so the single-bind helper does not fit — and filling `neighbourhood_matched`
with `total` would state a fact nobody checked); §4.3's in-flow abandonment notice,
which is still gated on the §5 replay.
