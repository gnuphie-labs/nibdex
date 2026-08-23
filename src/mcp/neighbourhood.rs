// SPDX-License-Identifier: MIT

//! `QUERY_QUALITY_DESIGN` §4.2 — hand back the vocabulary the caller lacks.
//!
//! The failure this exists for is documented in that design and it is not a
//! retrieval failure: a caller asks one badly-worded question, receives ten
//! plausible hits, concludes the corpus is empty and leaves. **Every counter
//! nibdex keeps scores that query as a success.** The observed case re-queried
//! afterwards *in the register the documents were actually written in* and got
//! the decisive sections at ranks 1–4 in 51 ms.
//!
//! The root cause is not laziness. **Documents are written in the register of a
//! person and an occasion; identifiers, table names and codes are vocabulary
//! imposed later by the reader.** A caller cannot guess the words a human wrote
//! in the moment — but the index knows them.
//!
//! ## Two decisions worth not re-deriving
//!
//! 1. **The neighbourhood is drawn from the OR-BROADENED query, never from the
//!    caller's own hits.** §4.2 records the strongest objection to this feature:
//!    terms taken from hits the query already found merely reinforce the wrong
//!    neighbourhood. Broadening is what reaches the region the caller missed —
//!    it is the difference between "more of what you found" and "what you did
//!    not find". This is the variant the design says to prefer, and it costs one
//!    extra FTS query, which is free: the measured budget is 10–20 ms indexed
//!    against 400–540 ms for a shell search.
//!
//! 2. **Terms are emitted ONLY when the broadened neighbourhood is strictly
//!    larger than what the caller's query matched.** That is a FACT about the
//!    retrieval — *there is more here than you reached* — not a judgement about
//!    whether the results were any good. It needs no threshold, no percentile
//!    table and no per-corpus calibration, which is the whole hazard §4.1 warns
//!    about: a mis-calibrated advisory teaches callers to ignore the field, and
//!    that is harder to undo than adding it late.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use sqlx::SqlitePool;

use super::fts5::fts5_or_broadened;

/// How many top-ranked sections of the broadened neighbourhood to read terms from.
/// Small on purpose: this is a vocabulary sample, not a summary.
const NEIGHBOURHOOD_SAMPLE: i64 = 24;

/// How many terms to return. Bytes are the scarce resource — the measured envelope
/// is ~340 B per hit — so this is a handful of words, once per response.
const MAX_TERMS: usize = 8;

/// A term must appear in at least this many of the sampled sections. One-off
/// vocabulary is noise; a term shared across the neighbourhood is its register.
const MIN_LOCAL_DOCS: usize = 2;

/// …and in no more than this FRACTION of them. The ceiling is the half that had to
/// be learned by running it: the first build returned `apply`, `related` and
/// `feedback` for a memory-corpus query — the memory FORMAT's own boilerplate, which
/// appears in nearly every file in that corpus.
///
/// 🔑 **Index-wide IDF cannot see that.** `fts5vocab` counts documents across the
/// WHOLE index, so a term that is ubiquitous inside one corpus but unremarkable
/// across all of them keeps a high IDF and wins. A local ceiling catches exactly
/// that class without needing a per-corpus baseline table: whatever nearly every
/// section in this neighbourhood says is structure, not register.
const MAX_LOCAL_FRACTION: f64 = 0.6;

/// Terms shorter than this are dropped. Deliberately 3, not 4: the vocabulary that
/// matters in a domain corpus is often a short acronym, and a 4-char floor would
/// silently discard exactly the terms a caller most needs handed to them.
const MIN_TERM_CHARS: usize = 3;

/// Structural English that carries no register. Kept deliberately tiny — an
/// aggressive stop list is a judgement about content, and the corpus's own IDF
/// already demotes anything genuinely ubiquitous.
const STOPWORDS: &[&str] = &[
    "the", "and", "for", "that", "this", "with", "from", "was", "were", "are", "but", "not",
    "all", "any", "can", "has", "had", "have", "its", "our", "out", "than", "then", "they",
    "them", "there", "these", "those", "what", "when", "which", "will", "would", "you", "your",
    "into", "over", "only", "one", "two", "how", "why", "who", "his", "her", "their", "been",
    "because", "should", "could", "does", "did", "each", "more", "most", "some", "such", "very",
];

/// Split a body into index-shaped terms. Mirrors FTS5's `unicode61` default closely
/// enough for the purpose: lowercase, split on anything not alphanumeric.
///
/// ⚠️ `char::is_alphanumeric`, NOT `is_ascii_alphanumeric`. A corpus carrying Korean,
/// accented or otherwise non-ASCII vocabulary is precisely the case where a caller
/// cannot guess the words — restricting to ASCII would drop the terms with the
/// highest value and would do it silently.
fn tokenize(body: &str) -> impl Iterator<Item = String> + '_ {
    body.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_lowercase())
}

fn is_candidate(term: &str) -> bool {
    term.chars().count() >= MIN_TERM_CHARS
        // A bare number is never register — it is a row count, a year or an id.
        && !term.chars().all(|c| c.is_numeric())
        && !STOPWORDS.contains(&term)
}

/// The distinctive vocabulary of the neighbourhood around `raw`, or an empty vec.
///
/// `body_sql` must be a two-placeholder statement selecting one body column,
/// ordered best-first, binding `(match_expression, limit)` — the caller supplies it
/// because each corpus joins its own tables.
///
/// Returns empty (never an error to the caller) whenever the feature cannot help:
/// no broadened form exists, the neighbourhood is no larger than what was already
/// matched, or nothing clears the frequency floor. **A silent empty is correct
/// here** — this is an advisory field, and failing to produce one must never fail a
/// retrieval that otherwise worked.
pub(crate) async fn neighbourhood_terms(
    pool: &SqlitePool,
    body_sql: &str,
    count_sql: &str,
    raw: &str,
    sanitized: &str,
    already_matched: i64,
) -> Result<(Vec<String>, i64)> {
    // 1. The broadened form. No broadened form (single term, or deliberate FTS5
    //    syntax the caller meant precisely) ⇒ nothing to say.
    let Some(broadened) = fts5_or_broadened(raw, sanitized) else {
        return Ok((Vec::new(), already_matched));
    };

    // 2. Is the neighbourhood actually wider? This is the gate, and it is a fact.
    let (neighbourhood_total,): (i64,) = sqlx::query_as(count_sql)
        .bind(&broadened)
        .fetch_one(pool)
        .await?;
    if neighbourhood_total <= already_matched {
        return Ok((Vec::new(), neighbourhood_total));
    }

    // 3. Read the top of the neighbourhood.
    let bodies: Vec<(String,)> = sqlx::query_as(body_sql)
        .bind(&broadened)
        .bind(NEIGHBOURHOOD_SAMPLE)
        .fetch_all(pool)
        .await?;
    if bodies.is_empty() {
        return Ok((Vec::new(), neighbourhood_total));
    }

    // 4. Local document frequency — in how many of the sampled sections does the
    //    term appear. Per-section dedupe, so one section repeating a word loudly
    //    cannot dominate the neighbourhood.
    let mut local: HashMap<String, usize> = HashMap::new();
    for (body,) in &bodies {
        for term in tokenize(body).filter(|t| is_candidate(t)).collect::<HashSet<_>>() {
            *local.entry(term).or_default() += 1;
        }
    }

    // Never hand back the caller's own words — that is the one thing they already had.
    let query_terms: HashSet<String> = tokenize(raw).collect();
    let ceiling = ((bodies.len() as f64) * MAX_LOCAL_FRACTION).ceil() as usize;
    local.retain(|term, n| {
        *n >= MIN_LOCAL_DOCS && *n <= ceiling && !query_terms.contains(term)
    });
    if local.is_empty() {
        return Ok((Vec::new(), neighbourhood_total));
    }

    // 5. Corpus document frequency, from the index's own vocabulary. This is the
    //    number the caller structurally cannot compute, and the reason a term like
    //    "system" loses to a term that is rare everywhere but dense here.
    let (corpus_docs,): (i64,) = sqlx::query_as("SELECT count(*) FROM search_index")
        .fetch_one(pool)
        .await?;
    let corpus_docs = corpus_docs.max(1) as f64;

    let terms: Vec<&String> = local.keys().collect();
    let placeholders = vec!["?"; terms.len()].join(",");
    let vocab_sql =
        format!("SELECT term, doc FROM search_index_vocab WHERE term IN ({placeholders})");
    let mut q = sqlx::query_as::<_, (String, i64)>(&vocab_sql);
    for t in &terms {
        q = q.bind((*t).clone());
    }
    let df: HashMap<String, i64> = q.fetch_all(pool).await?.into_iter().collect();

    // 6. Score and rank. tf-idf over the neighbourhood: how many of these sections
    //    carry the term, weighted by how unusual it is in the corpus as a whole.
    let mut scored: Vec<(f64, String)> = local
        .into_iter()
        .map(|(term, n)| {
            // A term absent from the vocab lookup is rarer than anything present.
            let doc = df.get(&term).copied().unwrap_or(1).max(1) as f64;
            let idf = (corpus_docs / doc).ln().max(0.0);
            (n as f64 * idf, term)
        })
        .filter(|(score, _)| *score > 0.0)
        .collect();
    // Deterministic: score desc, then term asc so equal scores never reorder between
    // runs. A field that shuffles under an identical query reads as instability.
    scored.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.cmp(&b.1))
    });

    Ok((
        scored.into_iter().take(MAX_TERMS).map(|(_, t)| t).collect(),
        neighbourhood_total,
    ))
}

/// `(top, spread)` over the returned bm25 ranks, or `None` if nothing ranked.
///
/// bm25 is negative and more-negative is better, so `top` is the minimum and the
/// spread is how far the returned set fans out from it. Both are handed over as
/// numbers: §4.1's hazard is a verdict computed from an uncalibrated threshold,
/// not the arithmetic itself.
pub(crate) fn rank_span(ranks: impl Iterator<Item = f64>) -> Option<(f64, f64)> {
    let mut top = f64::MAX;
    let mut worst = f64::MIN;
    let mut seen = false;
    for r in ranks {
        seen = true;
        if r < top {
            top = r;
        }
        if r > worst {
            worst = r;
        }
    }
    seen.then_some((top, worst - top))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenize_lowercases_and_splits_on_punctuation() {
        let got: Vec<String> = tokenize("Order-Contract: Target_Cost (WBS)").collect();
        assert_eq!(got, vec!["order", "contract", "target", "cost", "wbs"]);
    }

    #[test]
    fn tokenize_keeps_non_ascii_terms() {
        // The whole point of the feature: vocabulary a caller cannot guess.
        let got: Vec<String> = tokenize("the 매출총이익 line").collect();
        assert!(got.contains(&"매출총이익".to_string()), "got {got:?}");
    }

    #[test]
    fn candidate_filter_drops_short_numeric_and_stopwords() {
        assert!(!is_candidate("of"));
        assert!(!is_candidate("the"));
        assert!(!is_candidate("2026"));
        assert!(is_candidate("wbs"), "a 3-char acronym is exactly what to keep");
        assert!(is_candidate("bushing"));
    }

    #[test]
    fn candidate_keeps_alphanumeric_mixtures() {
        // Part numbers and codes are register too.
        assert!(is_candidate("p260130"));
        assert!(is_candidate("24kv"));
    }
}
