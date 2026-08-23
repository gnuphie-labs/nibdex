# Uptake stability — raise the floor, not the mean

**Status:** diagnosis + design direction, 2026-08-22. Publishable: counts and
ratios only — no corpus contents, document titles, identifiers, or third-party
material.

Companion to [`ADOPTION.md`](ADOPTION.md) (*never invoked*),
[`QUERY_QUALITY_DESIGN.md`](QUERY_QUALITY_DESIGN.md) (*invoked, wrong question*),
and [`TOOL_SURFACE_DESIGN.md`](TOOL_SURFACE_DESIGN.md) (*which verb*).

> **The claim in one line:** those three ask why a caller fails to get value.
> This one asks why value that *does* arrive fails to compound — and argues the
> answer is **variance, not average**. A retrieval tool whose misses are
> expensive cannot accumulate trust, because one expensive miss restores the
> prior that it does not help.

---

## 1. What was measured

**Method.** Reuses `scan()` and `classify()` from `dogfood/nibdex-adoption.py`
so the retrieval denominator matches the existing baseline — in particular its
rule that a shell search counts as retrieval only when it *heads* a pipeline
segment, not when it filters another command's output. Window 2026-07-15 to
08-21: **95 sessions, 2,865 external searches, 125 indexed queries.**

⚠️ **Two extraction paths were used and they disagree by ~3% on the call count
(125 vs 121 in §1.8).** The discrepancy has not been chased; it moves no
conclusion here, but **no figure in this document should be treated as exact to
the last unit.**

### 1.1 The abandonment signature exists

`QUERY_QUALITY_DESIGN.md` §5 item 1 set a falsification test: sessions with
≥1 indexed query **followed by** ≥5 external searches **and no further indexed
query** — *"if that shape is rare, §4.3 is not worth building."*

⚠️ **A per-session count table cannot test it.** "Followed by" is an ordering
claim, and counts have no order in them. The proxy a count table *can* answer
(≥1 indexed query **and** ≥5 external searches, same session) returns 50 of 53 —
a spectacular-looking confirmation that means almost nothing. Anchoring instead
on the **last** indexed query in each session makes the "no further" clause
automatic: everything after it is by construction followed by nothing.

| | n | |
|---|---:|---|
| never invoked the index | 48 | 50.5% — `ADOPTION.md`'s problem, unchanged |
| invoked it | 47 | 49.5% |
| — **signature**: ≥5 external after the **last** indexed query | **27** | **57.4% of those who invoked it** |
| — held: <5 external after | 20 | 42.6% |

**683 external searches — 23.8% of all external retrieval in the window — are
issued after the caller has already stopped using the index.**

The purest form: **12 sessions made exactly one indexed query and never came
back**, with 259 external searches after. That is the reproduction
`QUERY_QUALITY_DESIGN.md` §1 was written from. It is not an anecdote; it recurs
twelve times in five weeks.

⇒ **The shape is not rare. `QUERY_QUALITY_DESIGN.md` §4.3 survives its own falsification test.**

### 1.2 It is not session length

A long session satisfies "≥5 after" almost by construction, so that confound was
tested. Restricted to sessions with comparable external-search volume (10–60) the
split holds at 57%. What separates the two groups is **where the last indexed
query falls** in the session, normalised 0.0 (first action) to 1.0 (last):

| | median position of last indexed query |
|---|---:|
| signature sessions | **0.24** — dropped a quarter of the way in |
| held sessions | **0.94** — used to the end |
| *(restricted to comparable lengths)* | **0.18** vs **0.95** |

This is a sharper discriminator than the rank heuristic of `QUERY_QUALITY_DESIGN.md` §4.1, and it costs
nothing to compute.

⚠️ **Honest about its shape: bimodal with a real shoulder.** 14 sessions drop
early (<0.3), 22 run to the end (>0.7), and **11 (23%) sit in the middle**. Clear
modes, not a clean gap — **any threshold on position will misclassify some.**

### 1.3 What a call returns, and whether the answer is used

Each indexed query was attributed to the human turn it was made under, and scored
on what happened next.

| prompted by | n | empty/thin | answer **used** | next action **external** | median response |
|---|---:|---:|---:|---:|---:|
| the reminder hook | 36 | 14% | 14% | 44% | 5.8 KB |
| nothing (caller chose) | 77 | 10% | 21% | 56% | 7.8 KB |
| the operator, typed | 12 | 0% | 25% | 42% | 6.0 KB |

*empty/thin* = under 200 bytes or an explicitly empty hit list. *used* = a file
read within the next two retrieval actions. *external* = the very next retrieval
action was a shell search.

🔴 **The headline is the row-independent part: whatever prompts it, about one
call in five has its answer used, and about half are immediately followed by a
shell search anyway.** The index returns 6–8 KB and the caller goes and greps.
**That is the overhead-to-use problem, and it now has a baseline number.**

### 1.4 Two things recorded because they were wrong

- **A hypothesis that died.** The reminder hook fires on *prompt shape*, before
  anything knows whether the index can answer — so it looked like a machine for
  generating unhelpful calls and manufacturing exactly the negative experiences
  §2 describes. **Measured, it is false:** hook-prompted calls are slightly *less*
  likely to be abandoned than caller-chosen ones (44% vs 56%). Dropped.
- **A clean zero that was a bug.** The first attribution run reported **0**
  hook-prompted calls against 212 occurrences of the hook's own output string.
  The hook's stdout arrives as **its own transcript record**, not inside the user
  message; the parser read only user messages. Caught solely because the zero was
  too clean. *A suspiciously perfect number is a bug report.*

### 1.5 The evidence itself is decaying

Re-running the original baseline's measurement finds only **74 of its 110
sessions still present as transcripts**. They age out oldest-first; the earliest
five days are already gone. **The derived table now outlives the evidence it was
derived from.**

⇒ Any measurement in `ADOPTION.md` or here is reproducible only for as long as
its transcripts survive. **Derived tables must be committed, not recomputed on
demand** — and a figure that cannot be re-derived should say so.

### 1.6 The release era matters, and reach moved while retention did not

⚠️ **This window is not one deployment.** The box these sessions ran on carried
**rc.0 from July 14** until 2026-08-16 — daemon, binary, and a database whose
newest migration was 2026-07-11. Every rc.1 and rc.2 fix was **inert on the
machine where the work happened.** Split on the day it was brought to HEAD:

| | rc.0 era (07-15→08-15) | rc.3 era (08-16→08-21) |
|---|---:|---:|
| sessions | 72 | 16 |
| invoked the index | 44.4% | 87.5% |
| share of retrieval | 3.5% | 6.9% |
| **signature (abandoned)** | **56.2%** | **57.1%** |

🔑 **Reach roughly doubled. Retention did not move.** The work so far widened the
top of the funnel and left the leak where it was — which is the floor argument in
one table.

⚠️ **The adoption jump is confounded and should not be quoted:** 08-16→08-21 is
exactly when the maintainer was working *on* the tool, and sessions about a tool
use that tool. Sixteen sessions, six days. **The signature figure is far less
exposed to that confound; lean on it and not on the adoption number.**

⇒ **Standing methodological rule for this lane: a measurement spanning a release
boundary is two measurements.** What was true of an older build is not evidence
about the current one, in either direction.

### 1.7 The cold-seat run — nine real responses, graded blind

Nine real (task, query, response) triples were extracted from live sessions:
**five where the caller went to the shell next, four where the caller read a
file next**, shuffled and unlabelled. Each went to a fresh grader with no
knowledge of the tool, the experiment, or the arms, instructed to judge from the
artifact alone and to report the urge to go looking rather than act on it.

🔴 **The verdicts do not track the behaviour.** Of the five where the caller
bailed, one was graded ANSWERED and one PARTIAL. Of the four where the caller
read a file, two were graded worthless. ⇒ **"the next action was a file read" is
a weak proxy for "the response was good"** — listed as an assumption in §6, now
measured and worse than implied.

🔴 **Five of nine responses were rated actively WORSE than never having
searched** — by graders who did not know that was the thesis of §2.

**What the graders said irrespective of arm:**

- **Seven of nine** flagged that `body` and `body_excerpt` carry the same text,
  the excerpt being a strict prefix — a third to a half of every payload, for
  nothing.
- **Six of nine** flagged duplicate hits: the same passage returned two and three
  times at different heading depths, sometimes at the *same match line*, burning
  result slots on one match.
- **Six of nine** wanted the top hit **untruncated**, and one joined the two
  complaints: *the truncation is one field-width away from making the trip
  unnecessary.* ⇒ **The duplication is paying for the truncation.**
- Bulk judged **30–90% cuttable**, clustering near 80%.
- Rank called uninterpretable — no scale, no threshold, so a caller cannot tell a
  good hit from a bad one.
- Line numbers marked relocated with a shift, i.e. the tool reporting its own
  numbers as stale and asking the caller to do arithmetic.

🔑 **Two graders independently derived the substance of
[#17](https://github.com/gnuphie-labs/nibdex/issues/17)** from the artifact
alone: both received a broadened query with a very large match count and
concluded the response should have said *your literal query matched nothing;
these are unrelated fallback*, or returned nothing at all.

🔑 **Every grader reported the urge to leave.** Two made it diagnostic: *the
output's only real effect on me was to generate an urge to leave it*, and
*nothing in this output reduced that pull by any amount.*

⚠️ **One incidental find, and it bears on the never-fake-benefit rule:** the
health endpoint's cost-savings block was called **seductive** — it *tempts a
reader to conclude the tool is working well and therefore its silence on the task
is meaningful. It is not — the tool was never asked the question.*

### 1.8 The incumbent, measured with the same classifier

The comparison the earlier sections never made: **the same next-action classifier
applied to shell searches.**

| | indexed queries | shell searches |
|---|---:|---:|
| calls | 121 | 2,761 |
| median result | 6,090 B | 579 B |
| thin/empty (<200 B) | 9.1% | **27.4%** |
| answer used | 19.8% | 24.7% |
| **next action = same tool again** | 36.4% | **81.0%** |
| next action = switched tools | 51.2% | 1.9% |
| total bytes over the window | 1.02 MB | **3.13 MB** |

🔴 **The label was the bias, and it was ours.** An indexed query followed by a
shell search was scored *abandonment*. A shell search followed by a shell search
— **81% of the time** — is scored as nothing at all; it is called *refining*.
Same behaviour, two words. Applied symmetrically the incumbent is "abandoned"
four times as often.

🔴 **The incumbent fails more and is forgiven for it:** it returns essentially
nothing **27.4%** of the time against 9.1%. It has never faced the cold seat's
cost question, because **only the challenger was graded.**

🔴 **And the harness had already institutionalised it:** scoring retrieval against
`git grep` as ground truth means the index **cannot win by construction** — only
match or fall short. That is the same defect that produced the near-tautological
precision result.

**What survives as a real defect, not a double standard:** only **2.2%** of shell
searches return more than the index's *median*. Per call it really is roughly ten
times fatter, and the bulk complaint is earned. But in aggregate the scoreboard
inverts — 3.13 MB against 1.02 MB — because the incumbent is called 23× as often
and retried 81% of the time. ⇒ **The index is expensive per question; the shell is
expensive per answer.**

🔑 **The synthesis: the incumbent survives a 27% miss rate because each miss costs
579 bytes and a reflex.** Its floor is already on the ground. That is why *"it
doesn't help me"* never attaches to it despite failing far more often — and it
makes "raise the floor" not an aspiration but **a description of how the
incumbent already wins.**

### 1.9 Re-querying does not currently pay — so the low retry rate is rational

The obvious follow-up to §1.8 is that callers under-retry the index out of
prejudice. **Measured, that is false:**

| | n | answer used | thin/empty |
|---|---:|---:|---:|
| first query in a run | 77 | 19.5% | 6.5% |
| an immediate re-query | 44 | 20.5% | 13.6% |

A re-query buys nothing and goes empty twice as often. **Declining to retry is
sound judgement, not bias.**

🔑 **That relocates the problem, and improves it. A shell retry is productive
because the tool is transparent** — the caller saw exactly what matched, knows
precisely what the tool does, and the miss says what to change. **An index retry
is unproductive because the tool is opaque**, so the retry is a blind tweak of the
same vocabulary and lands in the same wrong register.

⇒ **A real capability difference sits underneath the perceptual bias, and it is
the larger of the two.** That is better news: information deficits are fixable,
priors are not.

### 1.10 The morsel pre-test — run, and inconclusive for an instructive reason

§6 item 12 asked for a cheap pre-test before building item 5: **re-score the nine
cold cases against a synthesised morsel and count how many graders would still
have needed the expansion.** Done 2026-08-22.

**Method.** Each of the nine real responses was rebuilt as a morsel — a frame of
mechanical facts, the **top hit read untruncated from disk**, one-line pointers
for the rest, and an explicit statement that the remainder was spooled and
retrievable by handle at negligible cost. Graded blind by fresh seats with the
same six questions plus a seventh: *would you actually ask for the expansion?*
**Total 34.7 KB against 87.0 KB** — eight of nine smaller by 33–83%.

**Result: 1 of 9 asked to expand.** No round-trip tax appeared.

🔴 **But the test is inconclusive, and the reason matters more than the number.
Six of the nine queries were simply bad**, so most of the eight refusals mean
*"the query was wrong and more of it will not help"* rather than *"the morsel was
sufficient."* Verbatim: *expanding wrong hits just gives me more wrong at higher
resolution* · *the query needs replacing, not extending.* On the subset where the
query was sound it is **1 of 2**. ⇒ **A morsel cannot be evaluated on a corpus of
bad queries.** The run mostly re-measured query quality, which is item 4's
territory, not item 5's.

**Where the query was sound, the morsel won clearly.** Case 03 moved from *BETTER*
to *"BETTER, clearly — one search, one section, task answered; this is the
best-case shape for a retrieval tool"*, and its bulk judgement fell from 60%
cuttable to **20–25%**. Two other cases fell from 90%/85% to 70%/50%.

🔑 **The strongest finding, and it amends item 1.** The frame was noticed and
endorsed — and then pushed further than it was written:

> *"The header honestly admits the query didn't match — which is to its credit —
> **but the correct behaviour after that admission is to return little or
> nothing**, not twelve confident-looking fallbacks."*
> *"The only line worth keeping is the first one. Everything after it is the tool
> filling space rather than admitting defeat."*

⇒ **The verdict must GATE the payload, not merely label it.** A `broadened` tag
sitting on top of twelve fallback hits is still an expensive miss. **#17 is a
switch that decides how much comes back, not an advisory field.**

⚠️ **One case regressed, and it was the synthesiser's fault — recorded because
the underlying constraint is real.** Case 09 went ANSWERED/BETTER →
NO/WORSE. The morsel was built by reading `line_start`–`line_end` from disk and
capping from the top; that "section" is **3,873 lines**, so the actual match at
line 1205 fell outside the cap and the grader received two unrelated entries
instead. **The bug is mine. The constraint is the index's:** *"return the top hit
in full"* is meaningless when a section spans a quarter of a file. ⇒ **A morsel
must be a bounded window centred on the match line, never "the whole section."**

**Two format defects, both concrete and both in the pointers:**

- **Code pointers are labelled with the commit that last touched the region.** One
  commit touching five places yields five identical one-liners — *"that label
  carries zero discriminating information; I cannot tell those five apart, so I
  cannot choose among them."*
- **Doc pointers show the file's own top-level title**, so two hits render
  identically at the same line — *"worse than omitting them."*
- ⇒ *"If the tool wants expansion used, the one-liners need to show **why** each
  hit matched."*

🔑 And one grader stated item 6's cross-corpus tag unprompted, as an indictment:
**"it never once said 'your query is a symbol, your task is a question.'"**

### 1.11 Hysteresis — every measurement here describes a build that has moved

⚠️ **Raised by the maintainer, and it is a property of the method rather than a
slip in any one measurement.** There are three lags, and only two are escapable.

**1. Literal version lag, and it is most of the evidence.** §1.6 records that the
box generating this corpus ran a build **five weeks and three releases old**
until midway through the window. **72 of the 95 sessions measured here describe
that stale build.** Every fix shipped in between was inert where the work
happened.

**2. Transcript archaeology is structurally retrospective.** Any metric derived
from recorded sessions can only score builds that have *already accumulated
history*. At the observed call volume — and **n=8** rankable labelled events over
five weeks (§1.10 / `dogfood/read-rank.py`) — the lag before a **new** build has a
scoreable sample is **months**. ⇒ **The method itself guarantees you evaluate a
version behind.** No individual measurement can fix that.

**3. The measurement DEFINITION lags too, and this one is invisible.** Every
figure in this lane runs through the same `classify()` in
`dogfood/nibdex-adoption.py`. **When the definition is wrong, every number moves
together, so the error cannot be seen in any comparison.** §1.10 found exactly
that: it counts as "retrieval" a population that is **74% non-rankable** — counts,
pipes, error text. That flaw is inherited by every prior figure here, silently,
across time. **Same hysteresis shape, but in the ruler.**

🔑 **WHAT ESCAPES IT, and it is what `read-rank.py` is actually for: replay
against a labelled set breaks the lag completely, because the strategy under test
is CURRENT CODE and only the ground truth is historical.** The strategy being
scored never had to be the one that ran — so the labelled set can score a build
that **does not exist yet**. §5 item 7's shadow evaluation is the same trick,
performed live.

🔴 **THE CHEAP FIX, AND IT CANNOT BE APPLIED RETROACTIVELY: nothing stamps a build
on a logged call.** The hook log carries `db`, `hits`, `outcome`, `scoped`,
`term`, `term_len`, `ts` — and no version. Responses carry none either. ⇒ **§1.6's
release split was made on a deploy date remembered in a project note, not from
data.** Had the upgrade happened a week from where the note says, that whole table
would be wrong and nothing would reveal it. **Stamp the build on every hook-log
line and every response** and any future measurement can partition by the version
that produced it. Filed as
[gnuphie-labs/nibdex#20](https://github.com/gnuphie-labs/nibdex/issues/20).

⚠️ **THE LAYER THAT CANNOT BE ESCAPED, and it sharpens §3: the caller's
disposition lags the code.** A repaired tool is still met by a prior formed on the
broken one. §1.9 measured that re-querying does not pay — on data that is mostly
from the stale build. If the current build made it pay, **the caller has no way to
learn that**, because the measured re-query rate (36%) is too low to discover it.

⇒ 🔑 **Behavioural hysteresis is longer than deployment hysteresis.** That is one
more argument for protecting the floor: **a negative dip costs the incident AND
the recovery window after it**, and the recovery window is governed by a retry
rate that the dip itself suppressed.

#### 1.11a Resonance, not merely lag

⚠️ **The maintainer's escalation, and it is the sharper framing: a feedback loop
with a long enough delay and enough gain does not just lag — it rings.**

**The record already shows the signature:**

- **`ADOPTION.md`'s central estimate moved 4% → 9% → 61%** — its own note says the
  classifier was *wrong twice in opposite directions*. **The corrections grew
  rather than shrank**, which is the amplifying signature, not the converging one.
- This journal has carried **five stale claims, "one of which had already been
  'corrected' once."** A correction that itself needed correcting is ringing.
- **This session produced two confident hypotheses, both wrong, in opposite
  directions** — *the reminder nudge manufactures bad calls* and *the shell is
  84× faster* — each killed by measurement within the hour.

**The mechanism, stated plainly:** measure stale behaviour → conclude the tool is
bad at X → change X → **the caller's disposition, formed on the previous build,
does not move** → measure again, still bad → **push harder on X.** The long delay
in layer 3 above is precisely what makes that overshoot possible, and pushing
harder is what turns overshoot into oscillation.

🔴 **And the journal is INSIDE the loop.** This file is read at session start and
shapes what the next session does; its claims are true at write time and consumed
much later. Its own recorded defect — **the entry written mid-session while the
session outlives it, seven instances, one of them a session never journalled at
all** — is therefore **a phase error in a feedback path, not a tidiness problem.**

⚠️ **Distinguishing resonance from ordinary noise, honestly:** shrinking
corrections mean noise handled well; growing corrections mean ringing. The
4% → 9% → 61% sequence has the growing signature. This session's two reversals
were opposite-direction but **caught in-session and did not propagate** — that is
damping working, and it is the behaviour to preserve.

**What damps it, in order of leverage:**

1. **Cut the delay.** §5 item 7's shadow evaluation is effectively zero-delay —
   every arm scored on every call.
2. **Cut the gain.** Never act on a single measurement; the standing hold-out rule
   in §6 is the same instinct.
3. **Stop closing the loop on stale state** — [#20](https://github.com/gnuphie-labs/nibdex/issues/20)'s
   build stamp, plus §1.6's rule that a measurement spanning a release boundary is
   two measurements.
4. 🔑 **Replay decouples the loop outright.** Scoring **current code** against
   **historical labels** drives the delay to near zero, because nothing waits for
   behaviour to accumulate. That is the third distinct problem in this document
   that the labelled set turns out to answer.

⇒ 🔑 **The one layer replay cannot reach is the caller's disposition, and it stays
slow whatever is built. That layer is managed by not exciting it** — which is the
floor thesis restated in control terms. **A tool that never dips negative never
requires the caller to re-learn, so the slow loop is never driven.** ⇒ **§3 is not
only a value argument; it is the anti-resonance policy.**

---

## 2. The finding: a miss is not neutral

The instinctive model is that a retrieval miss costs nothing — you asked, got
nothing useful, moved on. **The measurements say otherwise.** A miss today costs:

1. **6–8 KB of context**, spent on results that will not be used.
2. **A turn**, and the shell searches that follow it anyway.
3. **Occasionally, a wrong conclusion.** In the reproduction behind
   `QUERY_QUALITY_DESIGN.md`, ten plausible-but-wrong sections led to *"the
   record does not support this"* being reported repeatedly, to a human who knew
   it did. **The material was present the whole time.**

⇒ **A miss is not benefit ≈ 0. It is genuinely negative — worse than never
having asked.** Every one is a deposit in the *"it doesn't help me"* account, and
that account is what `ADOPTION.md` §9.8 names as the circularity: *low adoption
generates no evidence of value, and the absence reads as absence of value.*

---

## 3. The thesis: raise the floor

**You cannot make retrieval reliably positive.** It will miss; corpora are
incomplete and questions are badly worded. So *mean* benefit is the wrong target.

**But you can make a miss cheap.** Small, fast, and honest about being a miss. A
confident *"I don't have this"* converts a negative outcome into a neutral one at
almost no cost — and **a tool that never goes negative cannot start the vicious
cycle**, even with an unremarkable average.

🔑 This is the operator's own rule, already recorded in this project's history and
here pointed at the tool itself:

> *even though path A is better on all metrics 99% of the time, the 1% where that
> is not the case refutes the entire argument.*

⇒ **Engineer the 1%, not the 99%.** Concretely: **optimise the worst case of a
query, not the best case.** Every proposal below is scored on what it does to a
*miss*, not to a hit.

⚠️ **And the constraint that bounds all of it: the benefit must be real.** A
confident "no" that is *wrong* — the corpus had it — is the worst possible
outcome, strictly worse than ten weak hits, because it forecloses the re-query.
**Cheapness of a miss must never be bought with certainty the index has not
earned.** The vocabulary hand-back in `QUERY_QUALITY_DESIGN.md` §4.2 exists
precisely so a "no" stays recoverable.

### 3.1 The cost asymmetry — index-time work is nearly free

Floor-raising is affordable because **the two sides of this ledger are not
priced the same:**

- **Caller-side work is expensive.** Context bytes, latency *inside* a turn, and
  the caller's attention. Every KB returned is charged against the thing the
  caller was actually trying to do.
- **Index-side work is nearly free.** The daemon is a background process on
  otherwise idle hardware. It runs *before* anyone is waiting, and nothing is
  blocked while it runs.

⇒ 🔑 **nibdex should be willing to do ten times the work at index time to remove
one kilobyte, or one decision, from the response.** That trade is almost always
winning, and it is currently almost never taken.

This is the project's own precompute axis, already named in its history — *the
daemon exists so slow work happens before it is wanted* — and **every
floor-raising proposal in §5 sits on the cheap side of it:**

| proposal | what it needs | when it can be computed |
|---|---|---|
| #17 weak-query verdict | a per-corpus rank percentile baseline | per indexing pass; a lookup at query time |
| #18 vocabulary hand-back | distinctive terms per neighbourhood | expensive over a corpus, trivial as a stored column |
| #9 open command | line anchors per hit | known at chunk time, discarded today |

**None of these is a query-time cost.** They are index-time work that has simply
never been done. ⇒ **The response is thin on judgement not because judgement is
expensive, but because it was never precomputed.**

⚠️ **The bound, and it is the one that bites: precompute must not become another
staleness surface.** A percentile baseline that lags the corpus asserts a
*calibrated* verdict on uncalibrated evidence — a confident wrong "no", which §3
just named as the worst available outcome. **Anything precomputed must be
invalidated by the same indexing pass that invalidates the rows it summarises**,
which is the identical failure the schema-dump watcher gap
([#1](https://github.com/gnuphie-labs/nibdex/issues/1)) already documents.

### 3.2 The second asymmetry — latency is free, bytes are not

§3.1 priced index-time work. The same argument extends to **query time**, and the
numbers are not close. Measured on the same workspace, same term:

| | elapsed |
|---|---:|
| an indexed query (CLI, **including** process start) | **10–20 ms** |
| the equivalent shell search | **400–540 ms** |

**The shell is 20–40× slower.** ⇒ **Roughly 25–30 indexed queries fit inside the
time of one shell search.**

⚠️ **An earlier attempt at this measurement said the opposite** — that the shell
was 84× *faster* — because it timed the gap between transcript records, which is
harness and model wall-clock rather than tool execution. **A real quantity,
attributed to the wrong thing.** Treat the figures above as *tens of
milliseconds*, not exact: the CLI number includes process start that the resident
path does not pay.

🔑 **The two tools bill in different currencies, and only one is counted.** The
index costs **bytes** — visible, in-context, displacing the caller's reasoning
space. The shell costs **milliseconds** — invisible, because time consumes no
context. For a machine caller that weighting is arguably correct: bytes crowd out
thinking and milliseconds do not.

⇒ **Latency is this tool's abundant currency and bytes are its scarce one.**
Reconnaissance is therefore close to free: probing all corpora is ~5 queries and
still five times faster than one shell search. **Spend milliseconds lavishly;
spend bytes never.**

### 3.3 The per-hit tax — certainty is the lever, not trimming

Measured across the §1.7 responses (59 hits, 61 KB):

| | share of payload |
|---|---:|
| **envelope** — every non-content field, per hit | **32.7%** (**340 B per hit**) |
| `body_excerpt` duplicating `body` | ~21% |
| duplicate source locations | 5.9% |
| ⇒ **not content anyone asked for** | **~55–60%** |

🔑 **In one case the envelope alone cost ~4.0 KB against a 1.1 KB top hit: the
scaffolding on the hits nobody used cost roughly four times the answer.**

**Every hit costs 340 bytes before a single byte of content**, and the tool
returns 5–12 hits **because it does not know which one is right.** ⇒ Eliminating
a hit removes its envelope, its duplicated excerpt *and* its body. **Cutting
twelve hits to three is not a 75% cut in content; it is a ~75% cut in
everything.**

⇒ 🔑 **Bulk is a symptom, not the disease. The lever is certainty, and §3.2 says
certainty is cheap.** Trimming fields is worth doing and is item 0 in §5 — but it
is cosmetic beside returning three hits instead of twelve.

⚠️ **Duplicates are a design-doc phenomenon, not a code one** — 4 of 27 doc hits,
zero across code hits, caused by the same passage being emitted once per enclosing
heading scope. ⚠️ **A first measurement reported zero duplicates**, contradicting
six graders, because it keyed on `line_start` — the one field that differs between
heading-scope duplicates while the match line is identical.

---

## 4. Friction inventory

Two independent surfaces. Both must move; fixing one alone leaves the other as
the binding constraint.

**Invoke-side — the cost of asking:**

- Tool schemas are **deferred**: a `ToolSearch` call precedes the first question.
- **Five verbs** to choose between before the question is even formed.
- Queries take **raw FTS5 MATCH syntax, not natural language** — which is also
  the direct cause of the wrong-register failure in `QUERY_QUALITY_DESIGN.md` §1.
- 🔴 **The reminder hook exists to paper over all three, and carries 28.8% of all
  indexed queries.** Under a floor-raising frame **that share is debt, not
  success**: it is usage the tool has not earned, and it disappears the moment
  the nudge does. It is also the wrong instrument for the job — it fires on
  prompt shape, so it structurally cannot see the case `QUERY_QUALITY_DESIGN.md` §4.3 targets (five shell
  searches since the last indexed query), because by then the prompt is long past.

**Response-side — the cost of using the answer:**

- **6–8 KB median**, for an answer used one time in five.
- **No ready-to-run way to act on a hit** — the caller must still decide which
  file, and where in it, then open it themselves.
- **A weak result is shaped exactly like a strong one.** Ten plausible sections
  from a wrong-register query are indistinguishable, on the response, from ten
  correct ones.

---

## 5. Sequencing

Ordered by effect on the **floor**, not the mean.

**0. Stop sending `body_excerpt` alongside `body`, and collapse duplicate
heading-scope hits.** Not on the tracker; found by the §1.7 run, where seven of
nine and six of nine graders raised them unprompted. The excerpt is a strict
prefix of the body, and the duplicates are one match reported up to three times.
🔑 **Cutting them funds the untruncated top-hit body every grader asked for** —
smaller *and* more useful in one change. **Do this first; it is the cheapest item
in the document and it was invisible until someone read the artifact.**

🟢 **The excerpt half SHIPPED 2026-08-23 (`ff09c95`), measured at 20.9% of a
median `find_design_doc` payload and 18.3% of `find_code` over 273 live calls.
The heading-scope-duplicate half is still open.**

🔴 **This paragraph originally closed "with no losing case, so it needs no
per-scenario proof before shipping," and that clause was wrong — it is struck
above deliberately rather than quietly edited.** There is a losing case, and it
is the one the whole document is about: the floor, not the mean. `body` is
emptied for every hit past `DESIGN_DOC_TOTAL_BODY_BUDGET`, and on those hits the
excerpt is not a duplicate — it is the only content the result carries. Dropping
it unconditionally would have returned a heading, a line number and nothing else,
on precisely the tail results that were already worst-served. **"No losing case"
is a claim about a distribution, and this one was made by looking at the typical
hit.** The shipped change is conditional; a pre-existing assertion caught it.

🔴 **#17 and #18 ship together or not at all.** §1.9 is why: #17 alone honestly
reports a miss to a caller who has no way to recover from it — a cheaper dead end
is still a dead end, and the measured re-query rate says they will not try again.
#18 is the only proposal that gives a retry new information.

1. 🔴 **[#17](https://github.com/gnuphie-labs/nibdex/issues/17) — let the response
   say whether the query was weak or the corpus empty.** The highest-value item
   under this frame, and not because of what it adds to a good answer: it is what
   lets the tool say *"I don't have this"* instead of returning ten plausible
   wrong sections. **This is the miss made cheap** — the entire §2 cost collapses
   if a miss is 200 bytes and a clear verdict. ⚠️ Calibrated per corpus; `QUERY_QUALITY_DESIGN.md` §4.1's
   own warning stands, that a mis-calibrated advisory is worse than none.

   🔴 **AMENDED by §1.10: the verdict GATES the payload.** Blind graders, shown a
   response that correctly announced its own miss and then returned twelve
   fallback hits anyway, said the honest behaviour after that admission is to
   **return little or nothing**. A tag on top of a full payload is still an
   expensive miss. **#17 decides how much comes back.**
2. **[#9](https://github.com/gnuphie-labs/nibdex/issues/9) — a ready-to-run open
   command on every hit.** Attacks the 21%-used / 56%-external split directly:
   it stops the index competing with reading the file and makes it the thing that
   says *which lines to open*.
3. **[#18](https://github.com/gnuphie-labs/nibdex/issues/18) — hand back the
   corpus's own vocabulary on a miss.** Makes a miss **recoverable in one
   re-query** rather than terminal. Deliberately third: it only matters once a
   miss is detectable (#17), and `QUERY_QUALITY_DESIGN.md` §4.2 records an untested objection to it —
   terms drawn from hits the wrong query already found may reinforce the wrong
   neighbourhood.

4. 🆕 **The disclosed pivot — the index makes the recovery move itself.**
   *(Proposed 2026-08-22. Not filed; creating public artifacts is the
   maintainer's call.)*

   Instead of handing the caller vocabulary to re-query with, the tool performs
   the better query itself and returns **its result**, labelled honestly:

   > *0 instances of XYZ. In this corpus XYZ is associated with XWZ — 25 solid
   > hits for that:*

   🔑 **This is response-side friction reduction, not retry support, and that is
   the point.** #18 assumes a second round trip; §1.9 measured that the round
   trip buys nothing and usually does not happen. **The pivot removes it.** The
   caller gets the recovered answer in the call they already made.

   🔴 **The rule that makes it safe is already recorded.** Issue
   [#12](https://github.com/gnuphie-labs/nibdex/issues/12) rejected *silently*
   searching a transformed query, on the grounds that a silent transform is worse
   than declining. **This is the disclosed form:** it always states what was
   asked, that nothing matched it, what it pivoted to, and why. **Disclosure is
   the entire difference — never substitute silently.**

   **Three candidate mechanisms, cheapest and safest first:**

   - **(a) Cross-corpus pivot — deterministic, no statistics at all.** If the
     code index returns nothing and commits or design docs return 25, say so.
     🔑 **Four of the nine cold cases said, unprompted, that the answer lived in
     a different index from the one queried** — the single most repeated
     diagnosis in the run, and it needs no model to act on.
   - **(b) Corpus-wide term association (PMI or log-likelihood), computed at
     index time.** Associates are drawn from the whole corpus rather than from
     the weak query's own hits, which **dodges the objection already recorded
     against `QUERY_QUALITY_DESIGN.md` §4.2** — that terms taken from hits a
     wrong query found will reinforce the wrong neighbourhood. This is classical
     query drift, and (b) is the variant that avoids it.
   - **(c) Pseudo-relevance feedback from the top-k.** Standard, and carries
     exactly the drift risk (b) is designed to avoid. **Last, and only against a
     held-out set.**

   ⚠️ **The tempting empirical variant is not yet viable:** learning which
   queries historically preceded a successful one needs far more than 121 calls,
   and tuning on known positives measures the base rate rather than the signal.

   🔑 **All three are §3.1 work** — index-time or single-call. None adds a round
   trip, and (a) and (b) add no query-time cost at all.

5. 🆕 **The morsel with a handle — return an answer, and say what is spooled.**
   *(Proposed 2026-08-22. Not filed.)*

   The top hit is ~1 KB and near-constant regardless of response size, while
   responses run 4.5–13 KB. A default of **the top hit in full, a verdict, and
   one-line pointers for the rest** lands at ~1.5–2 KB — **a 60–85% cut that is
   simultaneously more useful**, because the top body finally arrives untruncated,
   which six of nine graders asked for.

   🔑 **The morsel is not a smaller search result; it is a different object.** Not
   *"here is a section that matched"* but **an answer plus the address to verify
   it** — usable *and without further processing*. That promotes
   [#9](https://github.com/gnuphie-labs/nibdex/issues/9) from a convenience to the
   shape of the response. ⇒ **Today the tool returns search results. It should
   return an answer with an address.**

   🔴 **The failure mode is predicted by §1.9 and must be designed against:** the
   *"more if you need it"* half needs a second call, and second calls mostly do
   not happen. If the morsel is insufficient and the expansion never fires, this
   is **strictly worse than today** — fewer bytes *and* no answer. Three
   conditions:
   - **A deterministic handle, not a re-query.** Expansion must return exactly the
     spooled result — a cache hit on recon already performed, ~1 ms. §1.9's
     retries fail because *re-querying is uncertain*; a cursor is not.
   - **A specific offer.** *"More available"* is ignored. *"3 more in design docs,
     2 in commits; hit 1's full body is 4.2 KB"* can be acted on.
   - **The morsel must stand alone in the large majority of calls.** Measurable
     against the nine cases; if expansion is needed often, this has merely added a
     round trip to every query.

   ⚠️ Snippet-plus-expand is well-trodden and its known failure in human
   interfaces is that nobody expands. **Do not bet the design on expansion
   firing.**

   🔴 **AMENDED by §1.10 — the morsel preparation rules, each earned by a
   failure in the pre-test:**
   - **Centre the window on the match line and bound it.** Never emit "the
     section": one measured section is **3,873 lines**. Capping a large span from
     its start silently excludes the match — which is exactly how the pre-test
     regressed its own best case.
   - **Never cap from the top.** Cap outward from the match, symmetrically.
   - **Assert the morsel is smaller than the full payload** at build time. One of
     nine was 69% *larger*; that must fail loudly rather than ship.
   - **Label a pointer by what matched, not by provenance.** A commit subject
     repeated across five hits, or a file's own title repeated across two, carries
     no discriminating information and is worse than omitting the line.
   - **The builder must be a pure function of (query, hits, corpus)** so it can be
     replayed offline against a labelled set. A morsel that can only be produced
     live cannot be tuned.

6. 🆕 **Mechanical provenance tags — say what was queried and how, without
   understanding anything.** *(Proposed 2026-08-22. Not filed.)*

   🔴 **The governing constraint, and it is a build constraint, not a preference:
   this must require no intelligence in the tool.** The tags are **facts about the
   retrieval**, derivable from what the index actually did — never judgements
   about content, which the tool cannot make and must not fake. **The caller
   supplies the intelligence; the index supplies valid, meaningful provenance.**

   Nearly every §1.7 complaint was a framing failure rather than a data failure —
   **the tool knew and did not say.** `query_broadened` sat in a field while the
   response looked like ten real hits; a very large `total_matched` had to be
   decoded by a grader as *"this is the whole corpus"*; relocated line numbers
   asked the caller to do arithmetic.

   A vocabulary that is entirely mechanical:

   | tag | the mechanical fact behind it |
   |---|---|
   | `exact` | the literal query terms matched |
   | `broadened` | the literal query matched nothing; state what was relaxed |
   | `cross_corpus` | corpus A was asked; this came from corpus B |
   | `associated` | reached via a precomputed term association; name the pair |
   | `duplicate_of` | same source location as an earlier hit |
   | `stale_lines` | the source moved since indexing |
   | `rank_profile` | clustered or flat — a distribution shape, not a verdict |
   | `withheld` | how many were not returned, and on what rule |

   **None requires reading the content.** ⇒ A tag like *relevant* or *this answers
   your question* is **invalid** — the tool cannot know it. *"Matched after
   relaxing three terms to one"* is always true and always meaningful.

   🔑 **Framing is byte-NEGATIVE, which is why it does not fight §3.2.**
   *"Nothing matched the literal query; broadened, so these are unrelated"* is
   ~70 bytes and replaces kilobytes of misleading hits. **And it is what licenses
   returning less: three hits instead of twelve is hiding things unless the
   response can honestly say why the other nine went.**

   ⚠️ **The frame must discriminate or it is decoration.** If every response opens
   with the same reassurance it becomes boilerplate and is skipped — the fate of
   the reminder nudge and of 7,630 unread generated insights. **A weak result must
   be visibly framed differently from a strong one.**

   ⚠️ **Structured first, one sentence second.** Open-ended prose reintroduces the
   bulk this whole section exists to remove.

🔑 **Items 4, 5 and 6 are one mechanism, not three features:** free latency buys
recon · recon buys certainty · certainty collapses the per-hit tax · **and the
tags are what make returning less honest rather than lossy.**

7. 🆕 **The knobs need an instrument before they need values.**
   *(Raised 2026-08-22. Not filed.)*

   Items 4, 5 and 6 each carry a choice — **which pivot mechanism** (cross-corpus,
   corpus-wide association, or relevance feedback), **how much to return**, **when
   the verdict gates**, **how wide the morsel window is**. ⚠️ **None of those can be
   set by argument.** `ADOPTION.md`'s central estimate moved 4% → 9% → 61% under
   scrutiny; a knob set from a plausible story will be wrong in the same way.

   🔑 **§3.2 licenses an unusually strong evaluation design: because latency is
   the abundant currency, every strategy can be computed on every query and only
   the chosen one paid for in bytes.** That is shadow evaluation without a traffic
   split — all arms observed on every call, no cohort ever served a worse
   response. The daemon does the extra work; the caller never sees it.

   🔴 **But it is blocked on ground truth, and that has been outstanding since
   2026-08-19: the Read-based ranking metric — which file did the session actually
   read after the search, and at what rank.** Neither tool can game it, because
   the label is what the session did next. **Until it exists there is nothing to
   score a shadow arm against, and every knob is set by taste.** ⇒ **Build the
   metric before building items 4–6, not after.**

   ⚠️ **And the standing trap applies to all of it:** a threshold tuned on known
   positives measures the base rate, not the signal. **Hold out a set.**

   🔑 **And the labelled set should become a GOLDEN SET, not merely a tuning
   input.** Classification practice keeps a frozen labelled corpus that must never
   regress, checked on every build; this repo already has `dogfood/release-gate.sh`
   to hang it on. **That is damping applied at the point of change rather than at
   the point of measurement** — a change that improved the mean while degrading
   the floor would fail the gate instead of shipping and being discovered five
   weeks later, which is the §1.11a failure mode exactly.

**Upstream of all seven** sits the invoke-side list in §4 above. It is not sequenced
here because it is a surface decision, not a defect —
`TOOL_SURFACE_DESIGN.md` owns it.

---

## 6. What would falsify this, and what is not measured

1. **One operator, one corpus.** Everything here is n=1. `ADOPTION.md`'s own
   central estimate moved **4% → 9% → 61%** under scrutiny, because its
   classifier was wrong twice in opposite directions. ⚠️ **Treat every ratio in
   this document the same way.**
2. **"Caller chose" is relative, not absolute.** The project instruction file
   carries a standing rule to query the index first, so nothing measured here is
   unprompted in any strict sense — only free of a *fresh* nudge.
3. 🔴 **"Used" is a proxy, and §1.7 measured it to be a weak one.** It counts a
   file read within two retrieval actions. Against blind graders it did not
   separate good responses from bad ones at all — two "used" cases were graded
   worthless and two "bailed" cases were graded useful. **Treat every
   used-vs-bailed split in this document as suggestive only.** It undercounts an
   answer used as context with no follow-up read, and cannot see an answer used
   to *stop* searching.
4. **The empty/thin threshold (200 bytes) is a heuristic**, not calibrated
   against known-good and known-bad responses.
5. **Ranks were not examined.** `QUERY_QUALITY_DESIGN.md` §4.1's rank/`total_matched`/flat-profile heuristic
   remains untested; that document's §5 item 2 warning applies — a threshold tuned on known
   positives measures the base rate, not the signal. **Hold out a set.**
6. **The cost asymmetry in §3.1 is asserted, not measured.** "Index-time work
   is nearly free" is true of *the caller's* budget. It is not free of CPU, disk
   or battery on the machine the daemon runs on, and that cost has never been
   quantified. ⚠️ **Ten times the index-time work is a cheap trade only until the
   daemon becomes something a person notices.**
7. **The strongest test of §3 is not in this document.** Nothing here shows that
   making a miss cheap *actually* changes caller behaviour. It is an argument, and
   `QUERY_QUALITY_DESIGN.md` §5 is right that an argument which survives only as
   an argument should be distrusted. **The measurement that would settle it:
   ship #17, then re-run §1.1 and see whether the signature share moves.**

8. **The cold-seat run had no control arm.** Nine responses from one tool were
   graded; **no shell-search output was ever put in front of a grader.** §1.8
   shows the incumbent fails more often on the one axis both were measured on, so
   its cost and bulk scores are simply unknown. ⚠️ **Until that control is run,
   every qualitative judgement in §1.7 is a scored defendant beside an unscored
   incumbent.**
9. **The disclosed pivot (§5 item 4) is a mechanism, not a result.** Nothing
   shows it recovers a known-missed document. **The test that would settle it:
   take the reproduction behind `QUERY_QUALITY_DESIGN.md` §1 — where re-querying
   in the corpus's own register returned the decisive sections at ranks 1–4 —
   and check whether variant (a) or (b) reaches them from the original failing
   query.** If neither does, it is a plausible mechanism and nothing more.
10. **Nine cases, one grader each, one corpus.** No inter-rater agreement was
   measured, and the graders were not fully cold — they ran inside a workspace
   whose instructions mention the tool. The prior cold review was run from a
   separate empty checkout; **this was an approximation of that, and a weaker
   one.**

11. **The latency figures are a spot measurement, not a benchmark.** One term,
   one workspace, three runs, warm caches, and a CLI path that pays process
   startup the resident path does not. The direction (tens of milliseconds versus
   hundreds) is robust; **the ratio is not, and 25–30 recon queries per shell
   search should be read as an order of magnitude, not a budget.**
12. 🟡 **The morsel pre-test was RUN and came back inconclusive — see §1.10.**
   Expansion was requested in 1 of 9, so no round-trip tax appeared; but six of
   the nine queries were bad, so most refusals meant *more of wrong is still
   wrong* rather than *the morsel sufficed*. **On sound queries it is 1 of 2.**
   ⇒ **Item 5 is neither validated nor killed.** A conclusive test needs a case
   set where the query is known-good — which is the same labelled set §5 item 7
   is blocked on.
13. **The tag vocabulary (§5 item 6) is asserted to be mechanically derivable, and
   that has not been checked against the code.** `rank_profile` in particular
   needs a per-corpus baseline to be meaningful, which is the same calibration
   `QUERY_QUALITY_DESIGN.md` §4.1 warns is worse than nothing if it is wrong.
   **Confirm each tag can be computed from state the query path already holds
   before treating the list as a spec.**

14. 🔴 **No error costs have been assigned, so the labelled set can RANK strategies
   but cannot CHOOSE a threshold.** §3 argues a miss is negative rather than
   neutral, and never says **how negative relative to the value of a hit.** Every
   knob in §5 item 7 is a threshold, and a threshold cannot be set from `hit@k`
   and MRR alone — those score ordering, not the cost of being wrong. ⚠️ **This is
   the standard shape in classification work and it is not optional there:** a
   filter is tuned against a cost-weighted objective precisely because a false
   positive and a false negative are not worth the same, and optimising plain
   accuracy silently picks whichever error is more common. ⇒ **Write the ratio
   down before tuning anything.** A defensible first cut follows from §2: a miss
   costs 6–8 KB, a turn, and occasionally a wrong conclusion about what the
   archive holds — so it is worth **several** hits, not a fraction of one. ⚠️ **A
   guessed ratio stated openly is far safer than an implicit one**, because an
   unstated cost model still exists, it is just unauditable.
15. 🔴 **The silent-success class is unexamined, and it sits in the denominator.**
   `read-rank.py` records **108 indexed calls with no read following**, and this
   document has treated that as neutral. It is not: *no read followed* can equally
   mean **the answer ended the search** — the best possible outcome, scored as
   nothing. ⚠️ **This is the complaint-stream bias in its usual form:** the visible
   failures are reported and the invisible successes are not, so anything tuned on
   what is observable drifts toward whichever error leaves a trace. **Every
   behavioural figure in §1 shares this blind spot**, including the abandonment
   signature itself — a session that stopped searching because it got its answer
   is indistinguishable, at this resolution, from one that gave up. ▶ **The
   discriminator to test: what the session did NEXT that was not retrieval** — an
   edit, a write, a reply — versus falling silent or changing subject.
   ⇒ **Until that is separated, treat every "abandonment" count in this document
   as an upper bound.**

---

## 7. Relationship to the backlog

- **`ADOPTION.md`** — *never invoked*, still the larger number (50.5% of sessions).
  This document does not displace it.
- **`QUERY_QUALITY_DESIGN.md`** — *invoked, wrong question*. §1.1 here supplies
  the measurement its §5 item 1 asked for, and confirms the shape is common.
- **`TOOL_SURFACE_DESIGN.md`** — owns the invoke-side friction listed in §4 above.
- **This document** — why value that arrives does not compound, and the argument
  that the lever is the floor rather than the average.
