// SPDX-License-Identifier: MIT

//! D1 gear 9 — the §7 MEASUREMENT gear (docs/D1_SCOPE.md §7 "the design→commit→
//! code thread, measured not assumed").
//!
//! Gears 6/8 produced *anecdotal* thread readings (`broadened` −13m, `bm25` −47m,
//! `rescore` −2m). §7's actual question is not "do a few feel tight" — it is "is
//! there a measurable thread, **how strong, and how does it vary**," and the
//! *shape* of the Δ-distribution across many terms is the fingerprint. This gear
//! turns the anecdotes into that distribution.
//!
//! Method (read-only; no tables, no persistence — the parked "Condensed edge-
//! chain" stays gated behind a real number here): draw a term population from the
//! corpus's OWN vocabulary (`fts5vocab` over `search_index`, which hands us each
//! term's document frequency = IDF for free), run the gear-8 `resolve_provenance`
//! on each (parity-by-construction — the metric measures exactly what the tool
//! resolves), and aggregate. It carries the three §7 honesty guards explicitly:
//!   (a) IDF discount — terms are tiered by distinctiveness so we can SEE whether
//!       the thread holds for rare terms but dissolves into boilerplate;
//!   (b) coverage normalization — session-feeder coverage is reported against the
//!       transcript-window denominator (a term authored before the window CANNOT
//!       have a session feeder — gear-8 Finding 3), and feeder-absence is first-
//!       class ("no feeder exists" ≠ "feeder exists but doesn't correlate");
//!   (c) the Δ-distribution SHAPE (a per-layer magnitude histogram), not one mean.
//!
//! Honesty bound baked into the report: nibdex/ClearView are single-author and
//! doc-rich — unrepresentative (§7.4). The `author_email` stratification is wired
//! but degenerate until a genuinely multi-author repo runs.

use std::collections::HashMap;
use std::time::Instant;

use anyhow::Result;
use sqlx::SqlitePool;

use crate::diff_index::resolve_provenance_in;

/// Per-term measurement, the unit the pure summarizer aggregates.
#[derive(Debug, Clone)]
pub struct TermMeasurement {
    pub term: String,
    pub df: i64,
    pub has_author: bool,
    /// The resolved code-author commit time, if any. Carried per-term so gear-10
    /// can compare the anchor position between two tokenizers (the symbol index
    /// surfaces earlier compound-identifier code uses, moving the anchor back).
    pub author_at: Option<i64>,
    pub author_lang: Option<String>,
    /// Tightest predating PROSE feeder Δ (closest-to-author negative), if any.
    pub prose_delta: Option<i64>,
    /// Tightest predating SESSION feeder Δ (closest-to-author negative), if any.
    pub session_delta: Option<i64>,
    /// False when the author predates the transcript window → a session feeder is
    /// structurally impossible, so this term must NOT count against session
    /// coverage (gear-8 Finding 3 normalization).
    pub session_possible: bool,
}

/// A per-layer Δ-magnitude histogram — the §7(c) "shape, not one number."
#[derive(Debug, Default, PartialEq)]
pub struct DeltaShape {
    pub sub_hour: usize,
    pub hour_to_day: usize,
    pub day_to_week: usize,
    pub week_plus: usize,
    pub n: usize,
    pub p50_secs: Option<i64>,
}

/// Coverage within one IDF tier — the §7(a) denominator guard made visible.
#[derive(Debug, PartialEq)]
pub struct TierCoverage {
    pub label: String,
    pub df_range: (i64, i64),
    pub n_terms: usize,
    pub with_author: usize,
    pub with_feeder: usize,
}

/// Coverage stratified by the authoring file's language (§7 step 4).
#[derive(Debug, PartialEq)]
pub struct LangCoverage {
    pub lang: String,
    pub n_authored: usize,
    pub with_feeder: usize,
    pub p50_tightest_delta: Option<i64>,
}

#[derive(Debug)]
pub struct ThreadReport {
    pub terms_measured: usize,
    pub with_author: usize,
    // Coverage among authored terms (absence first-class):
    pub prose_predating: usize,
    pub session_predating: usize,
    /// Denominator for normalized session coverage: authored terms whose author
    /// falls within the transcript window (so a session feeder is *possible*).
    pub session_possible: usize,
    pub either_feeder: usize,
    pub neither_feeder: usize,
    /// Example authored terms with NO recoverable feeder — eyeball whether they're
    /// genuinely feeder-less or just common (the §7 "verbs have no noun" cases).
    pub neither_examples: Vec<String>,
    // The shape (§7c):
    pub prose_shape: DeltaShape,
    pub session_shape: DeltaShape,
    // The guards:
    pub tiers: Vec<TierCoverage>,
    pub langs: Vec<LangCoverage>,
    // Corpus facts / caveats:
    pub distinct_author_emails: i64,
    pub transcript_window: Option<(i64, i64)>,
    pub elapsed_ms: u128,
}

/// Δ magnitude → coarse bucket. |Δ| because we only bucket *predating* feeders
/// (the sign already classified them as feeders, not descendants).
fn bucket(shape: &mut DeltaShape, delta: i64) {
    let mag = delta.unsigned_abs();
    if mag < 3_600 {
        shape.sub_hour += 1;
    } else if mag < 86_400 {
        shape.hour_to_day += 1;
    } else if mag < 604_800 {
        shape.day_to_week += 1;
    } else {
        shape.week_plus += 1;
    }
}

/// Median (p50) of a slice of values (|Δ|), or None if empty.
fn p50(values: &mut [i64]) -> Option<i64> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    Some(values[values.len() / 2])
}

/// The authoring file's language (extension), normalized. "?" when unknown.
pub fn lang_of(path: &str) -> String {
    match path.rsplit('.').next() {
        Some(ext) if ext != path => ext.to_lowercase(),
        _ => "?".to_string(),
    }
}

/// PURE (DB-free, unit-tested): aggregate per-term measurements into the report.
/// `measurements` must already be ordered however the caller likes; tiering is
/// computed here from the IDF spread so all aggregation lives in one place.
pub fn summarize(
    measurements: Vec<TermMeasurement>,
    distinct_author_emails: i64,
    transcript_window: Option<(i64, i64)>,
    elapsed_ms: u128,
) -> ThreadReport {
    let terms_measured = measurements.len();

    let authored: Vec<&TermMeasurement> = measurements.iter().filter(|m| m.has_author).collect();
    let with_author = authored.len();
    let prose_predating = authored.iter().filter(|m| m.prose_delta.is_some()).count();
    let session_predating = authored
        .iter()
        .filter(|m| m.session_delta.is_some())
        .count();
    let session_possible = authored.iter().filter(|m| m.session_possible).count();
    let either_feeder = authored
        .iter()
        .filter(|m| m.prose_delta.is_some() || m.session_delta.is_some())
        .count();
    let neither_feeder = with_author - either_feeder;
    let neither_examples: Vec<String> = authored
        .iter()
        .filter(|m| m.prose_delta.is_none() && m.session_delta.is_none())
        .take(10)
        .map(|m| m.term.clone())
        .collect();

    // The shape (§7c): per-layer magnitude histogram + p50.
    let mut prose_shape = DeltaShape::default();
    let mut prose_mags: Vec<i64> = Vec::new();
    let mut session_shape = DeltaShape::default();
    let mut session_mags: Vec<i64> = Vec::new();
    for m in &authored {
        if let Some(d) = m.prose_delta {
            bucket(&mut prose_shape, d);
            prose_mags.push(d.abs());
        }
        if let Some(d) = m.session_delta {
            bucket(&mut session_shape, d);
            session_mags.push(d.abs());
        }
    }
    prose_shape.n = prose_mags.len();
    prose_shape.p50_secs = p50(&mut prose_mags);
    session_shape.n = session_mags.len();
    session_shape.p50_secs = p50(&mut session_mags);

    // IDF tiers (§7a): split the measured terms into thirds by df (low df = high
    // IDF = distinctive). Sort a ref view by df ascending so tier 0 = rarest.
    let mut by_df: Vec<&TermMeasurement> = measurements.iter().collect();
    by_df.sort_by_key(|m| m.df);
    let tiers = build_tiers(&by_df);

    // Language stratification (§7 step 4), keyed on the authoring file's ext.
    let langs = build_langs(&authored);

    ThreadReport {
        terms_measured,
        with_author,
        prose_predating,
        session_predating,
        session_possible,
        either_feeder,
        neither_feeder,
        neither_examples,
        prose_shape,
        session_shape,
        tiers,
        langs,
        distinct_author_emails,
        transcript_window,
        elapsed_ms,
    }
}

/// Three IDF tiers by document frequency. `measurements` is sorted by df asc.
fn build_tiers(measurements: &[&TermMeasurement]) -> Vec<TierCoverage> {
    if measurements.is_empty() {
        return Vec::new();
    }
    let n = measurements.len();
    let third = n.div_ceil(3);
    let labels = [
        "distinctive (rare, high-IDF)",
        "mid",
        "common (low-IDF, boilerplate risk)",
    ];
    let mut tiers = Vec::new();
    for (i, label) in labels.iter().enumerate() {
        let start = (i * third).min(n);
        let end = ((i + 1) * third).min(n);
        let slice = &measurements[start..end];
        if slice.is_empty() {
            continue;
        }
        let df_lo = slice.iter().map(|m| m.df).min().unwrap_or(0);
        let df_hi = slice.iter().map(|m| m.df).max().unwrap_or(0);
        let with_author = slice.iter().filter(|m| m.has_author).count();
        let with_feeder = slice
            .iter()
            .filter(|m| m.has_author && (m.prose_delta.is_some() || m.session_delta.is_some()))
            .count();
        tiers.push(TierCoverage {
            label: label.to_string(),
            df_range: (df_lo, df_hi),
            n_terms: slice.len(),
            with_author,
            with_feeder,
        });
    }
    tiers
}

/// Coverage + tightest-Δ p50 per authoring language.
fn build_langs(authored: &[&TermMeasurement]) -> Vec<LangCoverage> {
    let mut by_lang: HashMap<String, (usize, usize, Vec<i64>)> = HashMap::new();
    for m in authored {
        let lang = m.author_lang.clone().unwrap_or_else(|| "?".to_string());
        let entry = by_lang.entry(lang).or_insert((0, 0, Vec::new()));
        entry.0 += 1;
        let tightest = match (m.prose_delta, m.session_delta) {
            // "Tightest" = the predating feeder closest to the author (max, since
            // both are negative) — the strongest thread for this term.
            (Some(p), Some(s)) => Some(p.max(s)),
            (Some(p), None) => Some(p),
            (None, Some(s)) => Some(s),
            (None, None) => None,
        };
        if let Some(t) = tightest {
            entry.1 += 1;
            entry.2.push(t.abs());
        }
    }
    let mut langs: Vec<LangCoverage> = by_lang
        .into_iter()
        .map(|(lang, (n_authored, with_feeder, mut mags))| LangCoverage {
            lang,
            n_authored,
            with_feeder,
            p50_tightest_delta: p50(&mut mags),
        })
        .collect();
    // Most-authored languages first.
    langs.sort_by(|a, b| b.n_authored.cmp(&a.n_authored).then(a.lang.cmp(&b.lang)));
    langs
}

/// Sample a deterministic, IDF-stratified term population from the corpus's OWN
/// vocabulary (`fts5vocab` over `search_index`). Factored out so gear-10 can resolve
/// the SAME population under two index tokenizers (D1_SCOPE §10 "Gear-10").
pub async fn sample_terms(pool: &SqlitePool, max_terms: usize) -> Result<Vec<(String, i64)>> {
    // fts5vocab must live in the same schema as the FTS table. Create transiently.
    sqlx::query("CREATE VIRTUAL TABLE IF NOT EXISTS vocab_si USING fts5vocab('search_index','row')")
        .execute(pool)
        .await?;

    // Candidate terms: appears in ≥2 rows (a thread is *possible*), ≥4 chars,
    // purely lowercase-alpha (drop numbers/identifiers-fragments/punctuation; the
    // default tokenizer already lowercases + splits, so terms are single words).
    let candidates: Vec<(String, i64)> = sqlx::query_as(
        "SELECT term, doc FROM vocab_si \
         WHERE doc >= 2 AND length(term) >= 4 AND term NOT GLOB '*[^a-z]*' \
         ORDER BY doc DESC",
    )
    .fetch_all(pool)
    .await?;
    sqlx::query("DROP TABLE vocab_si").execute(pool).await?;

    // Stratified sample across the df spectrum (common → rare), deterministic — so
    // the population spans IDF tiers and the (a) discount is observable. No RNG.
    Ok(stratified_sample(candidates, max_terms))
}

/// Run the measurement: sample the corpus vocabulary, resolve each term, summarize.
pub async fn measure_thread(pool: &SqlitePool, max_terms: usize, cap: i64) -> Result<ThreadReport> {
    let sampled = sample_terms(pool, max_terms).await?;
    measure_sampled(pool, "search_index", sampled, cap).await
}

/// The per-term population plus the corpus facts the summarizer needs. Returned by
/// `measure_terms` so callers (gear-9 `measure_sampled`, gear-10 `compare_tokenizers`)
/// can either summarize it or diff two populations term-by-term.
pub struct MeasuredPopulation {
    pub measurements: Vec<TermMeasurement>,
    pub distinct_author_emails: i64,
    pub window: Option<(i64, i64)>,
    pub elapsed_ms: u128,
}

/// Resolve a fixed term population against `index_table` into per-term measurements.
/// The vocab (sampling) always comes from `search_index`; only the *resolution* index
/// varies, so gear-10 measures the identical population under unicode61 vs symbol.
pub async fn measure_terms(
    pool: &SqlitePool,
    index_table: &str,
    sampled: Vec<(String, i64)>,
    cap: i64,
) -> Result<MeasuredPopulation> {
    let started = Instant::now();

    // Transcript window: a term authored before this CANNOT have a session feeder.
    let window: Option<(i64, i64)> = sqlx::query_as(
        "SELECT min(edited_at), max(edited_at) FROM session_edges",
    )
    .fetch_optional(pool)
    .await?
    .and_then(|(lo, hi): (Option<i64>, Option<i64>)| match (lo, hi) {
        (Some(l), Some(h)) => Some((l, h)),
        _ => None,
    });
    let window_start = window.map(|(lo, _)| lo);

    let distinct_author_emails: i64 =
        sqlx::query_scalar("SELECT count(DISTINCT author_email) FROM commit_entries")
            .fetch_one(pool)
            .await?;

    let mut measurements = Vec::with_capacity(sampled.len());
    for (term, df) in sampled {
        // Quote the term so an FTS5 keyword (near/and/or) is treated literally.
        let res =
            resolve_provenance_in(pool, index_table, &format!("\"{term}\""), cap).await?;

        let author_at = res.author.as_ref().map(|a| a.authored_at);
        let author_lang = res.author.as_ref().map(|a| lang_of(&a.file_path));

        // Tightest predating feeder per layer = the negative Δ closest to 0 (max).
        let prose_delta = res
            .feeders
            .iter()
            .filter_map(|f| f.delta_secs.filter(|d| *d < 0))
            .max();
        let session_delta = res
            .session_feeders
            .iter()
            .filter_map(|f| f.delta_secs.filter(|d| *d < 0))
            .max();

        let session_possible = match (author_at, window_start) {
            (Some(at), Some(ws)) => at >= ws,
            _ => false, // no transcripts, or prose-only term → session impossible
        };

        measurements.push(TermMeasurement {
            term,
            df,
            has_author: res.author.is_some(),
            author_at,
            author_lang,
            prose_delta,
            session_delta,
            session_possible,
        });
    }

    Ok(MeasuredPopulation {
        measurements,
        distinct_author_emails,
        window,
        elapsed_ms: started.elapsed().as_millis(),
    })
}

/// Resolve a fixed term population against `index_table` and summarize.
pub async fn measure_sampled(
    pool: &SqlitePool,
    index_table: &str,
    sampled: Vec<(String, i64)>,
    cap: i64,
) -> Result<ThreadReport> {
    let pop = measure_terms(pool, index_table, sampled, cap).await?;
    Ok(summarize(
        pop.measurements,
        pop.distinct_author_emails,
        pop.window,
        pop.elapsed_ms,
    ))
}

/// Deterministic stride sample to `max` items spanning the (df-sorted) input.
fn stratified_sample(candidates: Vec<(String, i64)>, max: usize) -> Vec<(String, i64)> {
    if max == 0 || candidates.len() <= max {
        return candidates;
    }
    let n = candidates.len();
    // Even stride across the whole range so both common and rare terms appear.
    (0..max)
        .map(|i| candidates[i * n / max].clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tm(
        term: &str,
        df: i64,
        author: bool,
        lang: &str,
        prose: Option<i64>,
        session: Option<i64>,
        session_possible: bool,
    ) -> TermMeasurement {
        TermMeasurement {
            term: term.to_string(),
            df,
            has_author: author,
            author_at: author.then_some(1000),
            author_lang: author.then(|| lang.to_string()),
            prose_delta: prose,
            session_delta: session,
            session_possible,
        }
    }

    #[test]
    fn coverage_treats_absence_as_first_class() {
        let m = vec![
            // authored, prose feeder only
            tm("alpha", 3, true, "rs", Some(-600), None, true),
            // authored, session feeder only
            tm("beta", 4, true, "rs", None, Some(-120), true),
            // authored, BOTH feeders
            tm("gamma", 2, true, "tsx", Some(-3000), Some(-60), true),
            // authored, NO feeder (the "verbs have no noun" case)
            tm("delta", 5, true, "rs", None, None, true),
            // authored, but predates transcript window → session impossible
            tm("epsilon", 6, true, "rs", Some(-100), None, false),
            // prose-only term, no code author at all
            tm("zeta", 2, false, "?", None, None, false),
        ];
        let r = summarize(m, 1, Some((1000, 2000)), 0);
        assert_eq!(r.terms_measured, 6);
        assert_eq!(r.with_author, 5);
        assert_eq!(r.prose_predating, 3); // alpha, gamma, epsilon
        assert_eq!(r.session_predating, 2); // beta, gamma
        assert_eq!(r.session_possible, 4); // all authored except epsilon
        assert_eq!(r.either_feeder, 4); // alpha,beta,gamma,epsilon
        assert_eq!(r.neither_feeder, 1); // delta
    }

    #[test]
    fn shape_buckets_by_magnitude_and_p50() {
        let m = vec![
            tm("a", 2, true, "rs", Some(-600), None, true),    // sub-hour
            tm("b", 2, true, "rs", Some(-7200), None, true),   // hour–day
            tm("c", 2, true, "rs", Some(-200_000), None, true), // day–week
            tm("d", 2, true, "rs", Some(-1_000_000), None, true), // week+
        ];
        let r = summarize(m, 1, None, 0);
        assert_eq!(r.prose_shape.sub_hour, 1);
        assert_eq!(r.prose_shape.hour_to_day, 1);
        assert_eq!(r.prose_shape.day_to_week, 1);
        assert_eq!(r.prose_shape.week_plus, 1);
        assert_eq!(r.prose_shape.n, 4);
        // |Δ| = [600, 7200, 200000, 1000000]; p50 at index 2 = 200000.
        assert_eq!(r.prose_shape.p50_secs, Some(200_000));
    }

    #[test]
    fn tiers_split_by_df_and_count_feeders() {
        // 6 terms; df 2,2,5,5,30,30 → thirds: [2,2],[5,5],[30,30].
        let m = vec![
            tm("r1", 2, true, "rs", Some(-100), None, true),
            tm("r2", 2, true, "rs", Some(-100), None, true),
            tm("m1", 5, true, "rs", None, None, true),
            tm("m2", 5, true, "rs", Some(-100), None, true),
            tm("c1", 30, true, "rs", None, None, true),
            tm("c2", 30, false, "?", None, None, false),
        ];
        let r = summarize(m, 1, None, 0);
        assert_eq!(r.tiers.len(), 3);
        // Distinctive tier (rarest df): both have feeders.
        assert_eq!(r.tiers[0].df_range, (2, 2));
        assert_eq!(r.tiers[0].with_feeder, 2);
        // Common tier: one authored-no-feeder, one no-author → 0 feeders.
        assert_eq!(r.tiers[2].df_range, (30, 30));
        assert_eq!(r.tiers[2].with_author, 1);
        assert_eq!(r.tiers[2].with_feeder, 0);
    }

    #[test]
    fn langs_group_and_take_tightest_predating() {
        let m = vec![
            // gamma has both; tightest = max(-3000,-60) = -60.
            tm("gamma", 2, true, "tsx", Some(-3000), Some(-60), true),
            tm("alpha", 3, true, "rs", Some(-600), None, true),
            tm("beta", 4, true, "rs", None, Some(-120), true),
        ];
        let r = summarize(m, 1, None, 0);
        // rs has 2 authored; tsx has 1. rs first (more authored).
        assert_eq!(r.langs[0].lang, "rs");
        assert_eq!(r.langs[0].n_authored, 2);
        assert_eq!(r.langs[0].with_feeder, 2);
        let tsx = r.langs.iter().find(|l| l.lang == "tsx").unwrap();
        assert_eq!(tsx.p50_tightest_delta, Some(60)); // |−60|
    }

    #[test]
    fn stratified_sample_spans_and_caps() {
        let c: Vec<(String, i64)> = (0..100).map(|i| (format!("t{i}"), i as i64)).collect();
        let s = stratified_sample(c.clone(), 10);
        assert_eq!(s.len(), 10);
        // First sample is the first element (stride from 0).
        assert_eq!(s[0].0, "t0");
        // Under the cap → returned whole.
        assert_eq!(stratified_sample(c, 1000).len(), 100);
    }
}
