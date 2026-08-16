// SPDX-License-Identifier: MIT

//! FTS5 query-language helpers: input sanitizing (`sanitize_fts5_query`),
//! the zero-result OR-broadening fallback (`fts5_or_broadened`,
//! `match_count_with_or_fallback`), and LIKE escaping (`like_substring`).
//! The cluster where D-10.15 phrase-fragment/proximity work will land.
//! Relocated from `mcp.rs` by gh#6 (see `docs/MCP_SPLIT_PLAN.md`).

use anyhow::Result;
use sqlx::SqlitePool;

pub(crate) fn like_substring(needle: &str) -> String {
    let escaped = needle
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    format!("%{escaped}%")
}

/// Make a user-supplied FTS5 MATCH expression robust against the bareword grammar.
///
/// FTS5's query parser rejects barewords containing characters such as `-`, `.`, or
/// `:` — e.g. `acme-dashboard` fails with `no such column: dashboard` and `v0.1.316`
/// with `syntax error near "."`. Those shapes (hyphenated product names, version
/// strings) are pervasive in this corpus, so an agent's natural query for a hyphenated
/// subsystem would *crash* the tool rather than return results (D-10.10). This is a
/// shared defect across every `find_*`/`recent_*` tool that binds a user query to
/// `body MATCH ?`, so the repair lives here and is applied at all of those sites.
///
/// Each whitespace-delimited token that contains a parser-hostile character is wrapped
/// in a double-quoted phrase literal, where FTS5 takes the contents as a tokenized
/// string. Tokens that are already valid FTS5 — plain barewords, operators
/// (`AND`/`OR`/`NOT`/`NEAR`), grouping (`(` `)` `,`), prefix (`*`), first-token (`^`),
/// and anything the caller already quoted (`"…"`) — pass through untouched, so
/// deliberate FTS5 syntax (including a genuinely malformed expression like an unclosed
/// quote) still behaves as before. Splitting on whitespace and rejoining with single
/// spaces preserves multi-word quoted phrases verbatim.
pub(crate) fn sanitize_fts5_query(raw: &str) -> String {
    fn token_is_fts5_safe(tok: &str) -> bool {
        // Defer any token the caller has quoted (or partially quoted) to FTS5 itself —
        // that's their phrase syntax, and an unbalanced quote should still surface as
        // the same parse error it does today.
        if tok.contains('"') {
            return true;
        }
        // Otherwise the token is safe iff every character is a bareword char or part of
        // FTS5 operator/grouping syntax. A `-`, `.`, `:`, `/`, `@`, … fails this and
        // forces the token to be quoted.
        tok.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '*' | '^' | '(' | ')' | ','))
    }

    raw.split_whitespace()
        .map(|tok| {
            if token_is_fts5_safe(tok) {
                tok.to_string()
            } else {
                format!("\"{}\"", tok.replace('"', "\"\""))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// D-10.13: bare space-separated terms are an *implicit AND* in FTS5, so a natural-language
/// query like `AcmeDashboard Alex follow-up loose ends` requires all five tokens to co-occur
/// in one entry — usually unsatisfiable — and silently returns zero. Given the original `raw`
/// query and its `sanitized` form, return an OR-joined variant to retry with IFF the query is
/// plain multi-term prose. We deliberately do NOT rewrite a query that uses explicit FTS5
/// syntax (phrase quotes, grouping, column filters, boolean/proximity keywords, or `+`/`-`
/// prefix operators) — that's the caller being precise on purpose; broadening it would be wrong.
pub(crate) fn fts5_or_broadened(raw: &str, sanitized: &str) -> Option<String> {
    // Respect any deliberate FTS5 syntax in the ORIGINAL query.
    if raw.contains(['"', '(', ')', ':']) {
        return None;
    }
    if raw.split_whitespace().any(|t| {
        matches!(t, "OR" | "AND" | "NOT" | "NEAR") || t.starts_with(['+', '-'])
    }) {
        return None;
    }
    // Operate on the sanitized tokens (hostile tokens already quoted, none containing an
    // interior space) so the OR-join is bind-safe. Single term: AND and OR are identical.
    let tokens: Vec<&str> = sanitized.split_whitespace().collect();
    if tokens.len() < 2 {
        return None;
    }
    Some(tokens.join(" OR "))
}

/// Run the body-MATCH COUNT for `sanitized`; if it matches nothing and the query is plain
/// multi-term prose, retry once OR-broadened (D-10.13). `count_sql` must be a single-`?`
/// COUNT statement binding the MATCH expression. Returns the query that actually produced
/// `total` (so the caller binds it for the follow-up SELECT) plus whether broadening fired.
pub(crate) async fn match_count_with_or_fallback(
    pool: &SqlitePool,
    count_sql: &str,
    raw: &str,
    sanitized: &str,
) -> Result<(String, i64, bool)> {
    let count = |q: String| async move {
        let (n,): (i64,) = sqlx::query_as(count_sql).bind(q).fetch_one(pool).await?;
        Ok::<i64, anyhow::Error>(n)
    };
    let total = count(sanitized.to_string()).await?;
    if total == 0
        && let Some(broadened) = fts5_or_broadened(raw, sanitized)
    {
        let bt = count(broadened.clone()).await?;
        if bt > 0 {
            return Ok((broadened, bt, true));
        }
    }
    Ok((sanitized.to_string(), total, false))
}

/// Turn a raw database error into something the caller can act on.
///
/// An FTS5 parse failure is the one error an ordinary caller triggers by
/// accident. `find_code("parse_config(")` is a natural thing to ask, and `(` is
/// FTS5 grouping syntax, so SQLite rejects the whole query. What came back was
/// `error returned from database: (code: 1) fts5: syntax error near ""` — which
/// names no offending character, suggests no repair, and leaks the storage
/// engine besides. A model that hits that once has no reason to try the tool
/// again, which makes this an adoption defect as much as a contract one.
///
/// Erroring is the RIGHT behaviour here — the ordering is actionable error >
/// silent adjustment > empty result, and quietly re-sanitizing until the query
/// parses would answer a different question than the one asked, unmarked. What
/// was missing is the actionable half, so this supplies it and drops the raw
/// sqlx text. Non-FTS5 errors pass through untouched: this must not swallow a
/// genuine "index is broken" into a syntax lecture.
pub(crate) fn explain_query_error(msg: &str) -> String {
    let lower = msg.to_ascii_lowercase();
    let is_query_syntax = lower.contains("fts5: syntax error")
        || lower.contains("fts5: phrase")
        || lower.contains("unterminated string")
        || lower.contains("no such column"); // an unquoted `foo:` parses as a column filter
    if !is_query_syntax {
        return msg.to_string();
    }
    "the query is not valid FTS5 MATCH syntax. nibdex takes FTS5 expressions, not \
     natural language, and ( ) \" * ^ : are operators there — so a bare code \
     fragment like parse_config( is read as syntax rather than as text. To search \
     for it literally, wrap it in double quotes: \"parse_config(\". An unbalanced \
     double quote fails the same way. This is a malformed query, NOT an empty or \
     broken index."
        .to_string()
}
