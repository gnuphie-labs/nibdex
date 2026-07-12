// SPDX-License-Identifier: MIT

//! Memory-entry extractor.
//!
//! Parses YAML frontmatter at the top of `~/.claude/projects/.../memory/*.md` files.
//!
//! The corpus has two coexisting shapes for the type field (verified against
//! the live memory dir, 2026-05-26: 176 top-level vs 58 nested). Both must
//! be accepted:
//!
//! ```text
//! ---
//! name: feedback-something
//! description: one-line summary
//! type: feedback             # shape A — top-level (more common in this corpus)
//! ---
//! ```
//!
//! ```text
//! ---
//! name: feedback-something
//! description: one-line summary
//! metadata:
//!   type: feedback           # shape B — nested under metadata (auto-memory template)
//! ---
//! ```
//!
//! If both are present, nested wins (matches the documented template). Other
//! metadata keys (`node_type`, `originSessionId`, etc.) are tolerated and ignored.
//!
//! `serde_yaml` is unmaintained upstream; the corpus shape is small and known, so
//! we hand-roll a 4-field parser instead of pulling a full YAML dep. Files without
//! valid frontmatter (e.g. the `MEMORY.md` index) return `None` and are skipped.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryEntry {
    pub name: String,
    pub memory_type: String,
    pub description: Option<String>,
    pub body: String,
}

/// Parse a memory file's content. Returns `None` if frontmatter is missing or
/// missing the required `name` / `metadata.type` fields.
pub fn extract_memory_entry(content: &str) -> Option<MemoryEntry> {
    let (frontmatter, body) = split_frontmatter(content)?;
    let parsed = parse_frontmatter(frontmatter);
    let name = parsed.name?;
    // Nested `metadata.type` wins over top-level `type` if both are present.
    let memory_type = parsed.metadata_type.or(parsed.top_level_type)?;
    Some(MemoryEntry {
        name,
        memory_type,
        description: parsed.description,
        body: body.to_string(),
    })
}

/// Split `---\n<frontmatter>\n---\n<body>`. Returns `(frontmatter, body)`.
/// The opening `---` MUST be the very first line (no leading whitespace, no BOM).
fn split_frontmatter(content: &str) -> Option<(&str, &str)> {
    let after_open = content
        .strip_prefix("---\n")
        .or_else(|| content.strip_prefix("---\r\n"))?;
    // Find the next line that is exactly `---` (LF- or CRLF-terminated, or EOF).
    let mut offset = 0;
    for line in after_open.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if trimmed == "---" {
            let frontmatter = &after_open[..offset];
            let body_start = offset + line.len();
            let body = &after_open[body_start..];
            // Strip a single leading blank line from the body for readability.
            let body = body.strip_prefix('\n').unwrap_or(body);
            let body = body.strip_prefix("\r\n").unwrap_or(body);
            return Some((frontmatter, body));
        }
        offset += line.len();
    }
    None
}

#[derive(Debug, Default)]
struct ParsedFrontmatter {
    name: Option<String>,
    description: Option<String>,
    metadata_type: Option<String>,
    top_level_type: Option<String>,
}

fn parse_frontmatter(frontmatter: &str) -> ParsedFrontmatter {
    let mut out = ParsedFrontmatter::default();
    let mut in_metadata = false;
    for raw_line in frontmatter.lines() {
        let line = raw_line.trim_end();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if !line.starts_with(' ') && !line.starts_with('\t') {
            // Top-level key.
            in_metadata = false;
            if let Some((key, value)) = split_kv(line) {
                match key {
                    "name" => out.name = clean_value(value),
                    "description" => out.description = clean_value(value),
                    "type" => out.top_level_type = clean_value(value),
                    "metadata" => {
                        // Either block-form (value empty, fields follow indented) or
                        // inline-flow like `metadata: { type: feedback }` (not used in
                        // this corpus, but tolerate the empty-value case).
                        in_metadata = value.trim().is_empty();
                    }
                    _ => {}
                }
            }
        } else if in_metadata {
            let stripped = line.trim_start();
            if let Some((key, value)) = split_kv(stripped)
                && key == "type"
            {
                out.metadata_type = clean_value(value);
            }
        }
    }
    out
}

fn split_kv(line: &str) -> Option<(&str, &str)> {
    let colon = line.find(':')?;
    let key = line[..colon].trim();
    let value = &line[colon + 1..];
    Some((key, value))
}

/// Strip surrounding whitespace and matched single/double quotes.
fn clean_value(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let stripped = strip_matched_quotes(trimmed);
    if stripped.is_empty() {
        None
    } else {
        Some(stripped.to_string())
    }
}

fn strip_matched_quotes(s: &str) -> &str {
    if s.len() >= 2 {
        let bytes = s.as_bytes();
        let first = bytes[0];
        let last = bytes[s.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return &s[1..s.len() - 1];
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_canonical_frontmatter() {
        let content = "\
---
name: feedback-doc-tone
description: frame decisions as process benefits, not personnel directives
metadata:
  type: feedback
---

Body lives here.

And spans multiple lines.
";
        let entry = extract_memory_entry(content).expect("entry extracted");
        assert_eq!(entry.name, "feedback-doc-tone");
        assert_eq!(entry.memory_type, "feedback");
        assert_eq!(
            entry.description.as_deref(),
            Some("frame decisions as process benefits, not personnel directives")
        );
        assert!(entry.body.contains("Body lives here"));
        assert!(entry.body.contains("multiple lines"));
    }

    #[test]
    fn no_frontmatter_returns_none() {
        // MEMORY.md index file shape — no frontmatter.
        let content = "# Sample Workspace - Key Learnings\n\nSome index content.\n";
        assert!(extract_memory_entry(content).is_none());
    }

    #[test]
    fn missing_required_field_returns_none() {
        // Has frontmatter but no metadata.type.
        let content = "---\nname: foo\ndescription: bar\n---\nbody\n";
        assert!(extract_memory_entry(content).is_none());

        // Has frontmatter but no name.
        let content = "---\ndescription: bar\nmetadata:\n  type: feedback\n---\nbody\n";
        assert!(extract_memory_entry(content).is_none());
    }

    #[test]
    fn description_is_optional() {
        let content = "\
---
name: project-bare
metadata:
  type: project
---
body
";
        let entry = extract_memory_entry(content).expect("entry extracted");
        assert_eq!(entry.name, "project-bare");
        assert_eq!(entry.memory_type, "project");
        assert!(entry.description.is_none());
    }

    #[test]
    fn quoted_values_unwrap() {
        let content = "\
---
name: \"feedback-quoted\"
description: 'single quoted'
metadata:
  type: \"feedback\"
---
body
";
        let entry = extract_memory_entry(content).expect("entry extracted");
        assert_eq!(entry.name, "feedback-quoted");
        assert_eq!(entry.memory_type, "feedback");
        assert_eq!(entry.description.as_deref(), Some("single quoted"));
    }

    #[test]
    fn crlf_line_endings_work() {
        let content = "---\r\nname: foo\r\nmetadata:\r\n  type: feedback\r\n---\r\nbody\r\n";
        let entry = extract_memory_entry(content).expect("entry extracted");
        assert_eq!(entry.name, "foo");
        assert_eq!(entry.memory_type, "feedback");
    }

    #[test]
    fn description_with_colons_preserved() {
        let content = "\
---
name: feedback-colon
description: prefer X: do Y over Z
metadata:
  type: feedback
---
body
";
        let entry = extract_memory_entry(content).expect("entry extracted");
        assert_eq!(entry.description.as_deref(), Some("prefer X: do Y over Z"));
    }

    #[test]
    fn empty_body_ok() {
        let content = "---\nname: foo\nmetadata:\n  type: feedback\n---\n";
        let entry = extract_memory_entry(content).expect("entry extracted");
        assert_eq!(entry.body, "");
    }

    #[test]
    fn top_level_type_accepted() {
        // Live corpus shape A (176/234 of the memory files use this form).
        let content = "\
---
name: feedback-dashboard-naming
description: acme-dashboard should be called AcmeDashboard
type: feedback
originSessionId: c88902e0-9085-48f4-8878-db4876403755
---
body
";
        let entry = extract_memory_entry(content).expect("entry extracted");
        assert_eq!(entry.name, "feedback-dashboard-naming");
        assert_eq!(entry.memory_type, "feedback");
    }

    #[test]
    fn nested_metadata_wins_over_top_level() {
        let content = "\
---
name: foo
type: project
metadata:
  type: feedback
---
body
";
        let entry = extract_memory_entry(content).expect("entry extracted");
        assert_eq!(entry.memory_type, "feedback");
    }

    #[test]
    fn metadata_with_other_keys_tolerated() {
        // Live corpus: metadata block often has node_type, originSessionId etc.
        // alongside type. Only `type` matters; the rest are silently ignored.
        let content = "\
---
name: feedback-todo-420
description: doc tone
metadata:
  node_type: memory
  type: feedback
  originSessionId: d6f910ea-0f31-4af0-b3b0-19d2c8c0b5a4
---
body
";
        let entry = extract_memory_entry(content).expect("entry extracted");
        assert_eq!(entry.memory_type, "feedback");
    }

    #[test]
    fn unterminated_frontmatter_returns_none() {
        let content = "---\nname: foo\nmetadata:\n  type: feedback\n\n(no closing fence)\n";
        assert!(extract_memory_entry(content).is_none());
    }
}
