// SPDX-License-Identifier: MIT

//! Design-doc section extractor.
//!
//! Splits a markdown file into sections keyed by heading hierarchy. Each section
//! captures its `heading_path` (slash-joined ancestor chain), the 1-indexed
//! `line_start` of the heading itself, the `line_end` of the line before the next
//! same-or-higher heading (or EOF), and the body text spanning that range.
//!
//! Per DESIGN §5.3 "section body = text between heading and next same-or-higher
//! heading" — descendant subsections are intentionally included in their
//! ancestor's body. Each section is independently queryable via FTS5 by its own
//! heading_path, so the duplication only costs index bytes, not retrieval clarity.
//!
//! Code-fence-aware: triple-backtick or triple-tilde fences mask any `^#+` lines
//! inside them. No other markdown structure is interpreted — for the docs/design
//! corpus this is sufficient. If we hit a doc that needs full AST handling we
//! swap in pulldown-cmark.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocSection {
    /// Slash-joined ancestor heading chain, e.g. `5.2/Storage schema`.
    pub heading_path: String,
    /// 1-indexed line number of the heading line itself.
    pub line_start: usize,
    /// 1-indexed line number of the last line in this section (inclusive).
    pub line_end: usize,
    /// Text from `line_start` through `line_end`, inclusive. Includes the
    /// heading and any descendant content up to the next same-or-higher heading.
    pub body: String,
}

#[derive(Debug, Clone, Copy)]
struct HeadingMatch<'a> {
    level: usize,
    title: &'a str,
    line_no: usize,
}

/// Extract all heading-delimited sections from a markdown document. Returns
/// sections in document order. If the document has no headings, returns empty.
pub fn extract_sections(content: &str) -> Vec<DocSection> {
    let lines: Vec<&str> = content.lines().collect();
    let headings = find_headings(&lines);
    if headings.is_empty() {
        return Vec::new();
    }

    let mut sections = Vec::with_capacity(headings.len());
    let mut stack: Vec<&str> = Vec::new();

    for (idx, h) in headings.iter().enumerate() {
        // Pop ancestors of equal or deeper level.
        while stack.len() >= h.level {
            stack.pop();
        }
        // Pad with empty placeholders if we jumped a level (e.g. ### with no ##).
        while stack.len() < h.level - 1 {
            stack.push("");
        }
        stack.push(h.title);

        let heading_path = stack
            .iter()
            .filter(|s| !s.is_empty())
            .copied()
            .collect::<Vec<_>>()
            .join("/");

        // Section runs from this heading's line through the line before the
        // next same-or-higher heading (or EOF).
        let line_end = headings[idx + 1..]
            .iter()
            .find(|next| next.level <= h.level)
            .map(|next| next.line_no - 1)
            .unwrap_or(lines.len());

        let body = lines[h.line_no - 1..line_end].join("\n");

        sections.push(DocSection {
            heading_path,
            line_start: h.line_no,
            line_end,
            body,
        });
    }

    sections
}

fn find_headings<'a>(lines: &[&'a str]) -> Vec<HeadingMatch<'a>> {
    let mut out = Vec::new();
    let mut in_fence = false;
    let mut fence_marker: Option<char> = None;
    for (i, line) in lines.iter().enumerate() {
        if let Some(marker) = detect_fence(line) {
            match fence_marker {
                None => {
                    in_fence = true;
                    fence_marker = Some(marker);
                }
                Some(open) if open == marker => {
                    in_fence = false;
                    fence_marker = None;
                }
                _ => {}
            }
            continue;
        }
        if in_fence {
            continue;
        }
        if let Some(m) = parse_heading(line) {
            out.push(HeadingMatch {
                line_no: i + 1,
                ..m
            });
        }
    }
    out
}

/// Returns `Some('`')` for triple-backtick fence opener/closer, `Some('~')` for
/// triple-tilde. Otherwise `None`.
fn detect_fence(line: &str) -> Option<char> {
    let trimmed = line.trim_start();
    if trimmed.starts_with("```") {
        Some('`')
    } else if trimmed.starts_with("~~~") {
        Some('~')
    } else {
        None
    }
}

fn parse_heading(line: &str) -> Option<HeadingMatch<'_>> {
    // Must start at column 0 — indented `#` is not a heading.
    if !line.starts_with('#') {
        return None;
    }
    let bytes = line.as_bytes();
    let mut level = 0;
    while level < bytes.len() && bytes[level] == b'#' {
        level += 1;
    }
    if level == 0 || level > 6 {
        return None;
    }
    // Must be followed by whitespace or be a bare `#` line (we treat the latter as not-a-heading).
    if level >= line.len() {
        return None;
    }
    let after = &line[level..];
    if !after.starts_with(' ') && !after.starts_with('\t') {
        return None;
    }
    let title = after.trim();
    if title.is_empty() {
        return None;
    }
    Some(HeadingMatch {
        level,
        title,
        line_no: 0, // filled in by caller
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_heading_single_section() {
        let content = "# Top\n\nBody line A\nBody line B\n";
        let sections = extract_sections(content);
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].heading_path, "Top");
        assert_eq!(sections[0].line_start, 1);
        assert_eq!(sections[0].line_end, 4);
        assert!(sections[0].body.contains("Body line A"));
        assert!(sections[0].body.contains("Body line B"));
    }

    #[test]
    fn nested_headings_build_path() {
        let content = "\
# Top
intro
## Sub A
content of A
### Deeper
deep content
## Sub B
content of B
";
        let sections = extract_sections(content);
        assert_eq!(sections.len(), 4);
        assert_eq!(sections[0].heading_path, "Top");
        assert_eq!(sections[1].heading_path, "Top/Sub A");
        assert_eq!(sections[2].heading_path, "Top/Sub A/Deeper");
        assert_eq!(sections[3].heading_path, "Top/Sub B");
    }

    #[test]
    fn body_includes_descendants_per_design() {
        let content = "\
# Top
intro
## Sub
sub content
";
        let sections = extract_sections(content);
        let top = &sections[0];
        assert_eq!(top.heading_path, "Top");
        // Per DESIGN §5.3: body runs to next same-or-higher heading; descendants included.
        assert!(top.body.contains("intro"));
        assert!(top.body.contains("## Sub"));
        assert!(top.body.contains("sub content"));
    }

    #[test]
    fn section_line_end_is_last_inclusive_line() {
        let content = "# A\nx\ny\n# B\nz\n";
        let sections = extract_sections(content);
        assert_eq!(sections.len(), 2);
        // A spans lines 1..=3 (heading, x, y); B starts at 4.
        assert_eq!(sections[0].line_start, 1);
        assert_eq!(sections[0].line_end, 3);
        assert_eq!(sections[1].line_start, 4);
        assert_eq!(sections[1].line_end, 5);
    }

    #[test]
    fn no_headings_returns_empty() {
        let content = "just\nsome\nlines\n";
        assert!(extract_sections(content).is_empty());
    }

    #[test]
    fn code_fences_mask_headings() {
        let content = "\
# Real
content
```
# Not a heading
```
## Also Real
more
";
        let sections = extract_sections(content);
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].heading_path, "Real");
        assert_eq!(sections[1].heading_path, "Real/Also Real");
    }

    #[test]
    fn tilde_fences_also_mask() {
        let content = "\
# Real
~~~
# Not heading
~~~
## After
done
";
        let sections = extract_sections(content);
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[1].heading_path, "Real/After");
    }

    #[test]
    fn indented_hash_is_not_heading() {
        let content = "# Real\n   # Indented not heading\ndone\n";
        let sections = extract_sections(content);
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].heading_path, "Real");
    }

    #[test]
    fn skipped_level_pads_with_placeholders() {
        // ## with no preceding # — the synthetic empty ancestor is filtered out
        // of heading_path so we get a clean "Orphan" rather than "/Orphan".
        let content = "## Orphan\ncontent\n";
        let sections = extract_sections(content);
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].heading_path, "Orphan");
    }

    #[test]
    fn bare_hash_not_heading() {
        // `#` with nothing after is not a markdown heading.
        let content = "#\n# Real\ncontent\n";
        let sections = extract_sections(content);
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].heading_path, "Real");
    }

    #[test]
    fn level_seven_not_heading() {
        // ATX headings are 1-6; seven hashes is not a heading.
        let content = "####### too deep\n# Real\ncontent\n";
        let sections = extract_sections(content);
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].heading_path, "Real");
    }
}
