// SPDX-License-Identifier: MIT

//! D1 gear 10 — the SYMBOL-AWARE TOKENIZER spike (docs/D1_SCOPE.md §4 #2, earned
//! by gear-9's measurement).
//!
//! Gear-9 measured the §7 thread as *frequency-confounded*: rare CONCEPT terms
//! that should thread don't co-occur across layers, because FTS5's default
//! `unicode61` folds a compound identifier into one opaque token — `queryBroadened`
//! indexes as `querybroadened`, which never matches prose's `query` + `broadened`.
//! So the code side of the thread is invisible to the prose side wherever the
//! shared vocabulary lives *inside* an identifier. unicode61 already splits
//! `snake_case` (it treats `_` as a separator); the gap is camelCase / PascalCase /
//! digit-letter boundaries / acronym runs.
//!
//! This gear is a build-to-discover A/B, not a productionized tokenizer: it builds
//! a **symbol-expanded shadow** of the diff-hunk FTS rows (each body re-emitted as
//! original tokens PLUS their identifier components) and lets the gear-9 thread
//! measurement run against BOTH indexes over the SAME term population. The question
//! it answers with a NUMBER: does identifier-splitting recover the mid/rare-tier
//! concept threads gear-9 showed pure unicode61 misses — and is the recovery worth
//! a real custom tokenizer (§4 #2 option C) over the shipped unicode61 (option A)?
//!
//! Scope kept smallest (gear ethos): shadow only the `diff_hunks` rows (the ones
//! `resolve_provenance` actually queries); the session leg (gear-7/8) is held
//! CONSTANT across A and B so the measured difference is purely the prose↔code
//! co-occurrence tokenization. Registering a real FTS5 tokenizer via the C API is
//! deferred until this A/B says the recovery justifies it.

use anyhow::Result;
use sqlx::SqlitePool;

/// Split ONE alphanumeric token into its identifier components, lowercased.
/// Boundaries: lower/digit→upper (camelCase), an upper-run's last char before a
/// lowercase (acronym end: `HTTPServer`→`HTTP`,`Server`), and letter↔digit. A
/// token with no internal boundary returns a single element (itself, lowercased).
pub fn split_token(token: &str) -> Vec<String> {
    let chars: Vec<char> = token.chars().collect();
    if chars.is_empty() {
        return Vec::new();
    }
    let mut parts: Vec<String> = Vec::new();
    let mut cur = String::new();
    for i in 0..chars.len() {
        let c = chars[i];
        if i > 0 {
            let prev = chars[i - 1];
            let boundary = if prev.is_lowercase() && c.is_uppercase() {
                // camelCase: query|Broadened
                true
            } else if prev.is_uppercase()
                && c.is_uppercase()
                && chars.get(i + 1).is_some_and(|n| n.is_lowercase())
            {
                // acronym end: HTTP|Server (split before the last upper of a run
                // that is followed by a lowercase letter)
                true
            } else {
                // letter↔digit either direction: utf|8, line|2|col
                (prev.is_alphabetic() && c.is_ascii_digit())
                    || (prev.is_ascii_digit() && c.is_alphabetic())
            };
            if boundary && !cur.is_empty() {
                parts.push(std::mem::take(&mut cur));
            }
        }
        cur.push(c);
    }
    if !cur.is_empty() {
        parts.push(cur);
    }
    parts.iter().map(|p| p.to_lowercase()).collect()
}

/// Symbol-expand a body for indexing: emit every maximal alphanumeric run as its
/// whole lowercased token PLUS its identifier components (when it splits into more
/// than one). Non-alphanumerics are dropped (unicode61 would split on them anyway).
/// Keeping the whole token preserves precision (`querybroadened` still matches a
/// whole-identifier query); the added components make the parts matchable too.
pub fn split_identifiers(text: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for run in text.split(|c: char| !c.is_alphanumeric()) {
        if run.is_empty() {
            continue;
        }
        let whole = run.to_lowercase();
        let parts = split_token(run);
        // Whole token first (precision), then its components (recall).
        out.push(whole.clone());
        if parts.len() > 1 {
            for p in parts {
                if p != whole {
                    out.push(p);
                }
            }
        }
    }
    out.join(" ")
}

/// The shadow FTS table name — a symbol-expanded mirror of the diff-hunk rows.
pub const SYMBOL_INDEX: &str = "symbol_search_index";

/// Build (or rebuild) the symbol-expanded shadow of the `diff_hunks` rows in
/// `search_index`. Same 4-column shape so `resolve_provenance_in` can swap the
/// FROM table unchanged; `rowid_ref`/`source_table` are preserved so the join to
/// `diff_hunks` is identical. Returns the number of rows shadowed.
pub async fn build_symbol_shadow(pool: &SqlitePool) -> Result<usize> {
    sqlx::query(&format!("DROP TABLE IF EXISTS {SYMBOL_INDEX}"))
        .execute(pool)
        .await?;
    sqlx::query(&format!(
        "CREATE VIRTUAL TABLE {SYMBOL_INDEX} USING fts5(\
            body, kind UNINDEXED, rowid_ref UNINDEXED, source_table UNINDEXED)"
    ))
    .execute(pool)
    .await?;

    // Read every diff-hunk row's body, symbol-expand it, re-insert under the same
    // rowid_ref/source_table so the downstream join is byte-for-byte the same query.
    let rows: Vec<(String, i64, String)> = sqlx::query_as(
        "SELECT body, rowid_ref, kind FROM search_index WHERE source_table = 'diff_hunks'",
    )
    .fetch_all(pool)
    .await?;

    let n = rows.len();
    for (body, rowid_ref, kind) in rows {
        let expanded = split_identifiers(&body);
        sqlx::query(&format!(
            "INSERT INTO {SYMBOL_INDEX} (body, kind, rowid_ref, source_table) \
             VALUES (?, ?, ?, 'diff_hunks')"
        ))
        .bind(expanded)
        .bind(kind)
        .bind(rowid_ref)
        .execute(pool)
        .await?;
    }
    Ok(n)
}

/// The outcome of the gear-10 A/B over one fixed term population.
pub struct TokenizerComparison {
    pub baseline: crate::thread_metric::ThreadReport,
    pub symbol: crate::thread_metric::ThreadReport,
    pub shadow_rows: usize,
    /// Terms whose resolved code-author anchor moved EARLIER under the symbol index
    /// (it surfaced an earlier compound-identifier code use baseline couldn't see) —
    /// the real positive signal, and the cause of any feeder-coverage drop.
    pub authors_moved_earlier: usize,
    /// Terms that gained a code author at all under the symbol index (none before).
    pub authors_gained: usize,
    /// p50 of the earlier-shift magnitudes, in seconds (how far back the anchor moved).
    pub shift_p50_secs: Option<i64>,
}

/// The gear-10 A/B: build the symbol shadow, sample one fixed term population from
/// the corpus vocabulary, and resolve it under both the shipped unicode61 index and
/// the symbol-expanded shadow. The per-tier feeder coverage delta answers "does
/// identifier-splitting recover the rare-concept threads gear-9 missed," and the
/// author-anchor shift answers "does it sharpen the code↔commit provenance join."
pub async fn compare_tokenizers(
    pool: &SqlitePool,
    max_terms: usize,
    cap: i64,
) -> Result<TokenizerComparison> {
    let shadow_rows = build_symbol_shadow(pool).await?;
    let sampled = crate::thread_metric::sample_terms(pool, max_terms).await?;
    let base_pop =
        crate::thread_metric::measure_terms(pool, "search_index", sampled.clone(), cap).await?;
    let sym_pop = crate::thread_metric::measure_terms(pool, SYMBOL_INDEX, sampled, cap).await?;

    // Diff the two populations term-by-term on the author anchor (same order, same
    // terms — both resolved the identical sampled population).
    let mut authors_moved_earlier = 0;
    let mut authors_gained = 0;
    let mut shifts: Vec<i64> = Vec::new();
    for (b, s) in base_pop.measurements.iter().zip(sym_pop.measurements.iter()) {
        match (b.author_at, s.author_at) {
            (Some(ba), Some(sa)) if sa < ba => {
                authors_moved_earlier += 1;
                shifts.push(ba - sa);
            }
            (None, Some(_)) => authors_gained += 1,
            _ => {}
        }
    }
    shifts.sort_unstable();
    let shift_p50_secs = (!shifts.is_empty()).then(|| shifts[shifts.len() / 2]);

    Ok(TokenizerComparison {
        baseline: crate::thread_metric::summarize(
            base_pop.measurements,
            base_pop.distinct_author_emails,
            base_pop.window,
            base_pop.elapsed_ms,
        ),
        symbol: crate::thread_metric::summarize(
            sym_pop.measurements,
            sym_pop.distinct_author_emails,
            sym_pop.window,
            sym_pop.elapsed_ms,
        ),
        shadow_rows,
        authors_moved_earlier,
        authors_gained,
        shift_p50_secs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_token_camel_and_pascal() {
        assert_eq!(split_token("queryBroadened"), vec!["query", "broadened"]);
        assert_eq!(
            split_token("resolveProvenanceIn"),
            vec!["resolve", "provenance", "in"]
        );
        assert_eq!(split_token("BuildInfo"), vec!["build", "info"]);
    }

    #[test]
    fn split_token_acronym_run() {
        // The upper-run boundary: HTTP|Server, not H|T|T|P|Server.
        assert_eq!(split_token("HTTPServer"), vec!["http", "server"]);
        assert_eq!(split_token("parseURLPath"), vec!["parse", "url", "path"]);
    }

    #[test]
    fn split_token_letter_digit() {
        assert_eq!(split_token("utf8"), vec!["utf", "8"]);
        assert_eq!(split_token("get2Items"), vec!["get", "2", "items"]);
        assert_eq!(split_token("fts5vocab"), vec!["fts", "5", "vocab"]);
    }

    #[test]
    fn split_token_no_boundary_is_identity() {
        assert_eq!(split_token("broaden"), vec!["broaden"]);
        assert_eq!(split_token("BROADEN"), vec!["broaden"]);
        assert_eq!(split_token(""), Vec::<String>::new());
    }

    #[test]
    fn split_identifiers_emits_whole_plus_parts() {
        // snake_case is split by the text tokenizer (`_` is a non-alnum separator),
        // so each piece is its own run; camelCase is split inside the run.
        let out = split_identifiers("let queryBroadened = resolve_provenance();");
        let toks: Vec<&str> = out.split_whitespace().collect();
        assert!(toks.contains(&"let"));
        // whole identifier kept for precision...
        assert!(toks.contains(&"querybroadened"));
        // ...plus its components for recall — the gear-9 gap closer.
        assert!(toks.contains(&"query"));
        assert!(toks.contains(&"broadened"));
        // snake pieces (already separate runs)
        assert!(toks.contains(&"resolve"));
        assert!(toks.contains(&"provenance"));
    }

    #[test]
    fn split_identifiers_drops_punctuation_and_lowercases() {
        let out = split_identifiers("Foo::Bar(x) // a-comment");
        let toks: Vec<&str> = out.split_whitespace().collect();
        assert!(toks.contains(&"foo"));
        assert!(toks.contains(&"bar"));
        assert!(toks.contains(&"comment"));
        assert!(!out.contains("::"));
    }
}
