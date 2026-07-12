// SPDX-License-Identifier: MIT

//! Session-history extractor.
//!
//! Parses entries under `## ... Recent session history` in the workspace CLAUDE.md.
//! Live format is bullet-style — `^- \*\*#(\d+)\*\*:` — though DESIGN §5.3 originally
//! sketched the `^### #(\d+)` heading format. Live format wins; old format support
//! can land Day 9-10 if a real CLAUDE.md.bak surfaces with that shape.
//!
//! Per-entry parsing extracts:
//! - `entry_date`: first occurrence of `(20\d{2}-\d{2}-\d{2})` in body
//! - `files_touched`: path-like substrings (`dir/file.ext`) that resolve to existing files
//!   under the workspace root
//! - `todos_mentioned`: integers from `TODO #(\d+)`, deduped, sorted, range [1,9999]
//! - `decisions_made`: which of {CLOSED, SHIPPED, LOCKED, FILED} appear in body

use std::collections::HashSet;
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEntry {
    pub session_number: i64,
    pub entry_date: Option<String>,
    pub body: String,
    pub files_touched: Vec<String>,
    pub todos_mentioned: Vec<i64>,
    pub decisions_made: Vec<String>,
}

static SECTION_HEADING: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^##\s+.*[Rr]ecent session history\b").unwrap());
static NEXT_TOP_HEADING: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^##\s").unwrap());
// Broad "is this file session-flavored at all" probe for the diagnose path.
// Matches any `^##` heading whose text contains the word "session" (case-insensitive).
// Used by `diagnose_empty_extraction` to differentiate "static README with no log"
// from "session log in a non-matching format".
static SESSION_LIKE_HEADING: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?im)^##\s+.*\bsession\b.*$").unwrap());
// Matches both single (`- **#610**:`) and range (`- **#569–#579**:` or `- **#569-#579**:`)
// bullets. The first session number in the range is captured as the row key; the body
// preserves the full range marker for FTS5 search.
static ENTRY_HEADING: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^-\s+\*\*#(\d+)(?:[–\-]#\d+)?\*\*:").unwrap());
static DATE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(20\d{2}-\d{2}-\d{2})").unwrap());
static FILE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?x)
        \b
        (
            [A-Za-z0-9_][A-Za-z0-9_./-]*
            \.
            (?: rs | ts | tsx | md | sql | toml | json | js | jsx | py | sh | yaml | yml | html | css | rb | go | java | proto )
        )
        \b
        ",
    )
    .unwrap()
});
static TODO_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bTODO\s*#(\d{1,4})\b").unwrap());
static DECISION_KEYWORDS: &[&str] = &["CLOSED", "SHIPPED", "LOCKED", "FILED"];

/// Extract all session-history entries from a CLAUDE.md content string.
/// Returns entries in encounter order (most recent first, matching live file convention).
pub fn extract_session_history(content: &str, workspace: &Path) -> Vec<SessionEntry> {
    let Some(section) = locate_section(content) else {
        return Vec::new();
    };
    let raw_entries = split_entries(section);
    raw_entries
        .into_iter()
        .map(|raw| parse_entry(raw, workspace))
        .collect()
}

/// Lightweight pass — return just the session_numbers parsed from CLAUDE.md's
/// "Recent session history" section. Skips body/file/decision parsing. Used by
/// `check()` orphan computation (D-6.3.1) to set-diff against the DB without
/// paying the full extractor cost.
pub fn extract_session_numbers(content: &str) -> Vec<i64> {
    let Some(section) = locate_section(content) else {
        return Vec::new();
    };
    split_entries(section)
        .into_iter()
        .map(|raw| raw.session_number)
        .collect()
}

/// When `extract_session_history` returns an empty Vec for a CLAUDE.md that was
/// discovered on disk, this helper classifies *why* into a one-line, user-facing
/// hint. Called by `indexer::run_session_history_extractor` to turn silent
/// 0-entry parses into actionable feedback (G2 fix #1 from
/// `dogfood/FRESH_INSTALL_NOTES.md`).
///
/// Returns the most informative case it can:
/// - `## Recent session history` heading present → "heading found but no
///   `- **#N**:` bullets matched" (probable: bullet shape differs).
/// - No exact heading, but other session-flavored headings present → lists them
///   up to three (probable: different convention; LIMITATIONS §6.1).
/// - Neither → "looks like a static project README, not a session log" (no
///   action needed).
pub fn diagnose_empty_extraction(content: &str) -> String {
    if SECTION_HEADING.find(content).is_some() {
        return "found '## Recent session history' heading but no `- **#N**:` bullets matched — entry shape may differ (see docs/LIMITATIONS.md §6.1)".to_string();
    }
    let session_like: Vec<String> = SESSION_LIKE_HEADING
        .find_iter(content)
        .map(|m| m.as_str().trim().to_string())
        .take(3)
        .collect();
    if session_like.is_empty() {
        "no '## Recent session history' heading and no other session-flavored headings — likely a static project README, not a session log".to_string()
    } else {
        format!(
            "no '## Recent session history' heading; found other session-flavored heading(s) [{}] — likely a different convention (see docs/LIMITATIONS.md §6.1)",
            session_like.join(" | ")
        )
    }
}

fn locate_section(content: &str) -> Option<&str> {
    let heading = SECTION_HEADING.find(content)?;
    let after_heading = heading.end();
    let remainder = &content[after_heading..];
    let end_offset = NEXT_TOP_HEADING
        .find(remainder)
        .map(|m| after_heading + m.start())
        .unwrap_or(content.len());
    Some(&content[after_heading..end_offset])
}

#[derive(Debug)]
struct RawEntry<'a> {
    session_number: i64,
    body: &'a str,
}

fn split_entries(section: &str) -> Vec<RawEntry<'_>> {
    let matches: Vec<_> = ENTRY_HEADING.captures_iter(section).collect();
    let mut out = Vec::with_capacity(matches.len());
    for (i, cap) in matches.iter().enumerate() {
        // group 0 (the whole match) is always present on a successful capture
        let m = cap.get(0).unwrap();
        let session_number: i64 = match cap[1].parse() {
            Ok(n) => n,
            Err(_) => continue,
        };
        let next_start = matches
            .get(i + 1)
            // group 0 always present (same reason as above)
            .map(|c| c.get(0).unwrap().start())
            .unwrap_or(section.len());
        let body = section[m.start()..next_start].trim_end();
        out.push(RawEntry {
            session_number,
            body,
        });
    }
    out
}

fn parse_entry(raw: RawEntry<'_>, workspace: &Path) -> SessionEntry {
    let body_string = raw.body.to_string();

    let entry_date = DATE_RE.find(raw.body).map(|m| m.as_str().to_string());

    let mut files_touched = Vec::new();
    let mut seen_files = HashSet::new();
    for cap in FILE_RE.captures_iter(raw.body) {
        let candidate = cap[1].to_string();
        if !seen_files.insert(candidate.clone()) {
            continue;
        }
        if workspace.join(&candidate).is_file() {
            files_touched.push(candidate);
        }
    }

    let mut todos_mentioned: Vec<i64> = Vec::new();
    let mut seen_todos = HashSet::new();
    for cap in TODO_RE.captures_iter(raw.body) {
        let Ok(n) = cap[1].parse::<i64>() else {
            continue;
        };
        if (1..=9999).contains(&n) && seen_todos.insert(n) {
            todos_mentioned.push(n);
        }
    }
    todos_mentioned.sort_unstable();

    let mut decisions_made: Vec<String> = Vec::new();
    for &kw in DECISION_KEYWORDS {
        if raw.body.contains(kw) {
            decisions_made.push(kw.to_string());
        }
    }

    SessionEntry {
        session_number: raw.session_number,
        entry_date,
        body: body_string,
        files_touched,
        todos_mentioned,
        decisions_made,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn ws() -> PathBuf {
        // Workspace path that doesn't exist — file checks always fail.
        // Tests that need real files use a tmp path.
        PathBuf::from("/tmp/nibdex-test-no-workspace")
    }

    #[test]
    fn locate_section_finds_recent_session_history() {
        let content = "\
# Top\n\n## 🗂️ Recent session history (one-line)\n\n- **#610**: foo\n- **#609**: bar\n\n## Next section\n\nblah\n";
        let section = locate_section(content).expect("section found");
        assert!(section.contains("#610"));
        assert!(section.contains("#609"));
        assert!(!section.contains("blah"));
    }

    #[test]
    fn locate_section_handles_no_terminator() {
        // No "## Next" follows — section runs to EOF.
        let content = "## Recent session history\n\n- **#1**: only\n";
        let section = locate_section(content).expect("section found");
        assert!(section.contains("#1"));
    }

    #[test]
    fn split_entries_separates_bullets() {
        let section = "\n\n- **#610**: Day 2 SHIPPED.\n  more text here.\n- **#609**: Day 1.\n";
        let entries = split_entries(section);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].session_number, 610);
        assert!(entries[0].body.contains("Day 2 SHIPPED"));
        assert!(entries[0].body.contains("more text here"));
        assert_eq!(entries[1].session_number, 609);
        assert!(entries[1].body.contains("Day 1"));
    }

    #[test]
    fn parse_entry_extracts_date() {
        let raw = RawEntry {
            session_number: 588,
            body: "- **#588**: TODO #420 Phase 1a VERIFIED 2026-05-22. blah blah 2026-05-24.",
        };
        let entry = parse_entry(raw, &ws());
        assert_eq!(entry.entry_date.as_deref(), Some("2026-05-22"));
    }

    #[test]
    fn parse_entry_extracts_todos_dedup_sort() {
        let raw = RawEntry {
            session_number: 588,
            body: "- **#588**: TODO #420 Phase 1a; TODO #427 wedge; TODO #420 again; [[TODO #427]] also.",
        };
        let entry = parse_entry(raw, &ws());
        assert_eq!(entry.todos_mentioned, vec![420, 427]);
    }

    #[test]
    fn parse_entry_extracts_decisions() {
        let raw = RawEntry {
            session_number: 568,
            body: "- **#568**: TODO #412 Step 1 SHIPPED DEV + STAGING. Architecture LOCKED.",
        };
        let entry = parse_entry(raw, &ws());
        assert_eq!(entry.decisions_made, vec!["SHIPPED", "LOCKED"]);
    }

    #[test]
    fn parse_entry_skips_missing_files() {
        let raw = RawEntry {
            session_number: 100,
            body: "Touched src/imaginary.rs and docs/never-existed.md.",
        };
        let entry = parse_entry(raw, &ws());
        assert!(entry.files_touched.is_empty());
    }

    #[test]
    fn extract_session_history_end_to_end_no_section() {
        let content = "# Just a top heading\n\nNo recent session history here.\n";
        let entries = extract_session_history(content, &ws());
        assert!(entries.is_empty());
    }

    #[test]
    fn diagnose_static_readme_classifies_as_no_log() {
        // Shape: clearview/CLAUDE.md — static project README, no session
        // tracking at all.
        let content = "\
# ClearView\n\n## Project Overview\n\nThing.\n\n## Tech Stack\n\nStack.\n\n## Build & Run\n\nRun it.\n";
        let hint = diagnose_empty_extraction(content);
        assert!(hint.contains("static project README"), "got: {hint}");
        assert!(!hint.contains("LIMITATIONS"));
    }

    #[test]
    fn diagnose_different_convention_lists_seen_headings() {
        // Shape: a workspace CLAUDE.md — session log but in a
        // `## SESSION STATUS` / `## Session Handoff` convention.
        let content = "\
# Sample Workspace\n\n## 🚨 SESSION STATUS\n\nstuff\n\n## 🔄 Session Handoff (BEFORE /clear)\n\nmore\n";
        let hint = diagnose_empty_extraction(content);
        assert!(hint.contains("different convention"), "got: {hint}");
        assert!(hint.contains("SESSION STATUS"), "got: {hint}");
        assert!(hint.contains("LIMITATIONS"));
    }

    #[test]
    fn diagnose_heading_present_but_bullets_missing() {
        // Heading is exactly right, but entries use the wrong bullet shape.
        let content = "\
# Top\n\n## Recent session history\n\n* item one\n* item two\n";
        let hint = diagnose_empty_extraction(content);
        assert!(hint.contains("heading but no"), "got: {hint}");
        assert!(hint.contains("LIMITATIONS"));
    }

    #[test]
    fn split_entries_handles_range_bullets() {
        let section = "\n- **#610**: single.\n- **#569–#579**: range em-dash.\n- **#250-#312**: range hyphen.\n";
        let entries = split_entries(section);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].session_number, 610);
        assert_eq!(entries[1].session_number, 569);
        assert!(entries[1].body.contains("#569–#579"));
        assert_eq!(entries[2].session_number, 250);
        assert!(entries[2].body.contains("#250-#312"));
    }
}
