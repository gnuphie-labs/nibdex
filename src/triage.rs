// SPDX-License-Identifier: MIT

//! `nibdex audit --triage` — walk the subdirectories that are in no IP domain
//! and let the user assign each one, then write the decisions back to
//! `.nibdex-domains.toml`.
//!
//! THE LINE THIS MODULE HOLDS: it never proposes a domain. It gathers evidence
//! — is this a git repo, what is its remote, who last committed, how big is it —
//! and shows that to the person, who decides. A tool that guessed "this looks
//! like client-a's" would be performing exactly the inference that mis-routes
//! IP, and it would be trusted precisely because it is usually right. Facts to
//! the human; the decision stays in the config file.
//!
//! WRITING IS THE RISKY PART. `.nibdex-domains.toml` is the security boundary,
//! and it carries the user's own comments. So: edits are additive only (nothing
//! is ever removed or reordered), the exact diff is shown and confirmed before
//! anything is written, and the write goes through `toml_edit` so comments and
//! formatting survive. A boundary file people cannot audit by eye is not a
//! boundary.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::domains::DomainConfig;

/// What the user decided about one unassigned subdirectory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Label it for these domains. Several = deliberately shared.
    Label(Vec<String>),
    /// Reviewed, deliberately in no domain.
    Acknowledge,
    /// Record it in `[unassigned] undecided` — written into the config so the
    /// decision can be made by editing the real file, not by answering a prompt.
    /// Grants nothing; the audit keeps reporting it until it is moved out.
    Undecided,
    /// Leave it untriaged; it will be reported again next run.
    Skip,
}

/// Comment written above `[unassigned]` when nibdex CREATES that table. A table
/// the user already wrote keeps their words — this never overwrites.
const UNASSIGNED_TABLE_COMMENT: &str = "\n\
    # Subdirectory paths in no IP domain. Nothing listed here reaches any index,\n\
    # and everything listed here still TAINTS sessions that touch it.\n\
    #\n\
    # At top level these read as annotations, since an unlabeled directory is\n\
    # already indexed by nobody. At depth they do real work: an entry WITHDRAWS\n\
    # that path from any enclosing label's reach — which is how you exclude a\n\
    # tree sitting inside a labeled directory.\n";

/// Comment written above `acknowledged` when nibdex creates that key.
const ACKNOWLEDGED_KEY_COMMENT: &str =
    "\n# Reviewed, deliberately in no domain. `nibdex audit` stays quiet about these.\n";

/// Comment written above `undecided` when nibdex creates that key. This is the
/// instruction sheet for the whole editable-config workflow, and it lives in the
/// file being edited rather than in documentation the editor cannot see.
const UNDECIDED_KEY_COMMENT: &str = "\n\
    # Discovered by `nibdex audit --stage-undecided`, still awaiting a decision.\n\
    # Move an entry into a domain's list under [domains] to index it, or into\n\
    # `acknowledged` to record that it stays out. Removing it from this list is\n\
    # what silences the audit. No domain is ever suggested for you: guessing which\n\
    # domain a directory belongs to is the one inference that would mis-route IP.\n";

/// Facts about a subdirectory, gathered to inform a human. Every field is
/// observed, never interpreted.
#[derive(Debug, Default)]
pub struct Evidence {
    pub is_git_repo: bool,
    pub remote: Option<String>,
    pub last_commit: Option<String>,
    pub file_count: usize,
}

impl Evidence {
    pub fn render(&self) -> String {
        let mut bits: Vec<String> = Vec::new();
        bits.push(if self.is_git_repo {
            match &self.remote {
                Some(r) => format!("git repo (remote: {r})"),
                None => "git repo (no remote)".to_string(),
            }
        } else {
            "not a git repo".to_string()
        });
        bits.push(format!("{} file(s)", self.file_count));
        if let Some(c) = &self.last_commit {
            bits.push(format!("last commit {c}"));
        }
        bits.join(" · ")
    }
}

/// Observe `dir` without judging it. Failures degrade to "unknown" rather than
/// erroring — evidence is a convenience, and a missing fact must never block a
/// decision the user is entitled to make.
pub fn gather_evidence(dir: &Path) -> Evidence {
    let mut ev = Evidence { is_git_repo: dir.join(".git").exists(), ..Default::default() };
    ev.file_count = walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_entry(|e| {
            let n = e.file_name().to_string_lossy();
            !(e.file_type().is_dir() && (n == ".git" || n == "node_modules" || n == "target"))
        })
        .flatten()
        .filter(|e| e.file_type().is_file())
        .count();
    if ev.is_git_repo && let Ok(repo) = git2::Repository::open(dir) {
        ev.remote = repo
            .find_remote("origin")
            .ok()
            .and_then(|r| r.url().ok().map(str::to_string));
        ev.last_commit = repo.head().ok().and_then(|h| h.peel_to_commit().ok()).map(|c| {
            let who = c.author().name().unwrap_or("unknown").to_string();
            let summary: String = c
                .summary()
                .ok()
                .flatten()
                .unwrap_or("")
                .chars()
                .take(48)
                .collect();
            format!("by {who} — \"{summary}\"")
        });
    }
    ev
}

/// Prompt for one subdirectory. `input`/`output` are injected so the loop is
/// testable without a terminal.
fn prompt_one(
    name: &str,
    ev: &Evidence,
    domains: &[&str],
    input: &mut dyn BufRead,
    out: &mut dyn Write,
) -> Result<Decision> {
    writeln!(out, "\n'{name}/' is in no IP domain")?;
    writeln!(out, "    {}", ev.render())?;
    writeln!(out, "\n  Which domain owns it?")?;
    for (i, d) in domains.iter().enumerate() {
        writeln!(out, "    [{}] {d}", i + 1)?;
    }
    writeln!(out, "    [s] shared — several domains (comma-separated numbers)")?;
    writeln!(out, "    [a] acknowledge — reviewed, deliberately in no domain")?;
    writeln!(out, "    [ ] skip — decide later (reported again next run)")?;
    write!(out, "  > ")?;
    out.flush()?;

    let mut line = String::new();
    if input.read_line(&mut line)? == 0 {
        return Ok(Decision::Skip); // EOF — treat as skip, never as a label
    }
    let ans = line.trim().to_lowercase();
    if ans.is_empty() {
        return Ok(Decision::Skip);
    }
    if ans == "a" {
        return Ok(Decision::Acknowledge);
    }
    let picks = if let Some(rest) = ans.strip_prefix('s') {
        rest.trim_start_matches([':', ' ']).to_string()
    } else {
        ans.clone()
    };
    let mut chosen: Vec<String> = Vec::new();
    for tok in picks.split(',') {
        let tok = tok.trim();
        if tok.is_empty() {
            continue;
        }
        match tok.parse::<usize>() {
            Ok(n) if n >= 1 && n <= domains.len() => {
                let d = domains[n - 1].to_string();
                if !chosen.contains(&d) {
                    chosen.push(d);
                }
            }
            _ => {
                writeln!(out, "  (didn't understand {tok:?} — skipping this one)")?;
                return Ok(Decision::Skip);
            }
        }
    }
    if chosen.is_empty() {
        Ok(Decision::Skip)
    } else {
        Ok(Decision::Label(chosen))
    }
}

/// Render the decisions as the config change they imply, for review BEFORE any
/// write. This is the artifact the user actually approves.
///
/// `staged` is the current `[unassigned] undecided` list, so a decision that
/// also retires a staged entry shows BOTH halves of the move — a confirmation
/// prompt that understated the change would not be a confirmation.
pub fn render_plan(decisions: &[(String, Decision)], staged: &[String]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let mut any = false;
    let retire = |out: &mut String, name: &String| {
        if staged.contains(name) {
            let _ = writeln!(out, "  [unassigned] undecided    -= \"{name}\"  (now decided)");
        }
    };
    for (name, d) in decisions {
        match d {
            Decision::Label(ds) => {
                let _ = writeln!(out, "  [domains] {}  += \"{name}\"", ds.join(" + "));
                retire(&mut out, name);
                any = true;
            }
            Decision::Acknowledge => {
                let _ = writeln!(out, "  [unassigned] acknowledged += \"{name}\"");
                retire(&mut out, name);
                any = true;
            }
            Decision::Undecided => {
                let _ = writeln!(out, "  [unassigned] undecided    += \"{name}\"");
                any = true;
            }
            Decision::Skip => {}
        }
    }
    if !any {
        out.push_str("  (nothing to change)\n");
    }
    out
}

/// Apply decisions to `.nibdex-domains.toml`, preserving comments and layout.
///
/// ADDITIVE ONLY, with exactly one retraction. Labels and acknowledgements are
/// only ever appended — never removed, reordered, or rewritten — because a
/// triage helper that could unlabel something would be a way to silently widen a
/// domain's reach.
///
/// The single exception: deciding a subdir also drops it from
/// `[unassigned] undecided`. That is not a widening — `undecided` grants nothing
/// in either direction — it is the second half of the move the user just asked
/// for. Leaving it behind would produce exactly the contradiction the loader
/// rejects, so the three triage avenues (prompt, config edit, `nibdex label`)
/// would fight each other instead of composing.
pub fn apply(workspace: &Path, decisions: &[(String, Decision)]) -> Result<usize> {
    let path = workspace.join(".nibdex-domains.toml");
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    let mut doc: toml_edit::DocumentMut = raw
        .parse()
        .with_context(|| format!("parsing {}", path.display()))?;
    let mut applied = 0usize;

    for (name, decision) in decisions {
        match decision {
            Decision::Label(domains) => {
                for d in domains {
                    let arr = doc["domains"][d.as_str()]
                        .as_array_mut()
                        .ok_or_else(|| anyhow::anyhow!("[domains] {d} is not an array"))?;
                    if !arr.iter().any(|v| v.as_str() == Some(name.as_str())) {
                        arr.push(name.as_str());
                        applied += 1;
                    }
                }
                applied += usize::from(retire_from_undecided(&mut doc, name));
            }
            Decision::Acknowledge => {
                let arr =
                    unassigned_array_mut(&mut doc, "acknowledged", ACKNOWLEDGED_KEY_COMMENT)?;
                if !arr.iter().any(|v| v.as_str() == Some(name.as_str())) {
                    arr.push(name.as_str());
                    applied += 1;
                }
                applied += usize::from(retire_from_undecided(&mut doc, name));
            }
            Decision::Undecided => {
                let arr = unassigned_array_mut(&mut doc, "undecided", UNDECIDED_KEY_COMMENT)?;
                if !arr.iter().any(|v| v.as_str() == Some(name.as_str())) {
                    arr.push(name.as_str());
                    // Evidence rides beside the entry as a comment. The person
                    // deciding needs facts about the directory, and the file is
                    // where they are now deciding — so the facts belong here, not
                    // in terminal output they have already scrolled past. Observed
                    // facts only; never a suggested domain.
                    let ev = gather_evidence(&workspace.join(name)).render();
                    let last = arr.len() - 1;
                    if let Some(v) = arr.get_mut(last) {
                        v.decor_mut().set_prefix(format!("\n    # {ev}\n    "));
                    }
                    // One entry per line, so deciding is a line move in an editor.
                    arr.set_trailing_comma(true);
                    arr.set_trailing("\n");
                    applied += 1;
                }
            }
            Decision::Skip => {}
        }
    }

    if applied > 0 {
        let rendered = doc.to_string();
        // Validate the CANDIDATE before it replaces the boundary file. Writing
        // first and validating after leaves a rejected config on disk and makes
        // recovery the user's problem; refusing here leaves the file untouched.
        DomainConfig::parse_validated(&rendered, &path.display().to_string())
            .context("refusing to write: the resulting config would not be valid")?;
        std::fs::write(&path, rendered)
            .with_context(|| format!("writing {}", path.display()))?;
        // Belt and braces: re-read from disk through the real loader, so a bad
        // WRITE (truncation, encoding) is caught too, not just a bad document.
        DomainConfig::load(workspace)
            .context("the written config failed validation — restore from git and report this")?;
    }
    Ok(applied)
}

/// Drop `name` from `[unassigned] undecided`, reporting whether it was there.
///
/// The entry's evidence comment is its own decoration, so it leaves with it —
/// no orphaned comment is left hanging over the next entry. This is the ONLY
/// removal this module performs; see [`apply`] for why it is not a widening.
fn retire_from_undecided(doc: &mut toml_edit::DocumentMut, name: &str) -> bool {
    let Some(arr) = doc
        .get_mut("unassigned")
        .and_then(toml_edit::Item::as_table_mut)
        .and_then(|t| t.get_mut("undecided"))
        .and_then(toml_edit::Item::as_array_mut)
    else {
        return false;
    };
    let Some(idx) = arr.iter().position(|v| v.as_str() == Some(name)) else {
        return false;
    };
    arr.remove(idx);
    if arr.is_empty() {
        // Collapse the emptied list to `[]` rather than leaving a two-line husk.
        // The key stays: it is where the next staging run writes, and its
        // explanatory comment is worth keeping in front of the user.
        arr.set_trailing("");
        arr.set_trailing_comma(false);
    }
    true
}

/// Borrow (creating if absent) an array inside the `[unassigned]` table.
///
/// The table and key comments are attached ONLY when nibdex is the one creating
/// them, so a file the user has already written or annotated keeps their words —
/// the comment-preservation contract this module exists to honor.
fn unassigned_array_mut<'a>(
    doc: &'a mut toml_edit::DocumentMut,
    key: &str,
    key_comment: &str,
) -> Result<&'a mut toml_edit::Array> {
    if doc.get("unassigned").is_none() {
        let mut t = toml_edit::Table::new();
        t.decor_mut().set_prefix(UNASSIGNED_TABLE_COMMENT);
        doc["unassigned"] = toml_edit::Item::Table(t);
    }
    let tbl = doc["unassigned"]
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("[unassigned] is present but is not a table"))?;
    if !tbl.contains_key(key) {
        tbl[key] = toml_edit::value(toml_edit::Array::new());
        if let Some(mut k) = tbl.key_mut(key) {
            k.leaf_decor_mut().set_prefix(key_comment);
        }
    }
    tbl[key]
        .as_array_mut()
        .ok_or_else(|| anyhow::anyhow!("[unassigned] {key} is present but is not an array"))
}

/// Stage every currently-unassigned subdir into `[unassigned] undecided`.
///
/// This is the whole of "the file IS the config": rather than answering prompts
/// that write the config for you, the discovered set is written into the real
/// file with its evidence, and you decide by editing it. Returns the decisions
/// so the caller can show them before applying — the same show-then-write shape
/// every other write in this module uses.
///
/// Entries already staged are filtered out, so the plan shown to the user states
/// what will actually change rather than re-listing the whole set every run.
pub fn stage_undecided(cfg: &DomainConfig, pending: &[String]) -> Vec<(String, Decision)> {
    pending
        .iter()
        .filter(|n| !cfg.is_undecided(n))
        .map(|n| (n.clone(), Decision::Undecided))
        .collect()
}

/// The interactive loop. Returns the decisions; the caller renders and applies.
pub fn run_triage(
    workspace: &Path,
    unassigned: &[String],
    domains: &[&str],
    input: &mut dyn BufRead,
    out: &mut dyn Write,
) -> Result<Vec<(String, Decision)>> {
    let mut decisions = Vec::new();
    for name in unassigned {
        let ev = gather_evidence(&workspace.join(name));
        let d = prompt_one(name, &ev, domains, input, out)?;
        decisions.push((name.clone(), d));
    }
    Ok(decisions)
}

const SKIP: &[&str] = &[".git", "node_modules", "target", "dist", "build", "vendor"];

/// How far below the workspace root to look. A quarantined tree is only worth
/// reporting if a human would recognize it, and every level costs a directory
/// walk of every LABELED tree — so this is a reporting bound, never a routing
/// one. `includes` has no depth limit; a nested repo deeper than this is still
/// correctly indexed by nobody, it just is not enumerated here.
const MAX_REPORT_DEPTH: usize = 6;

/// Which subdir paths are in no domain and not already annotated? Shared with
/// the audit's `subdir-unassigned` check so the two can never disagree.
///
/// Depth-aware, and deliberately derived from [`DomainConfig::includes`] rather
/// than from a comparison against the config text: "unassigned" is DEFINED here
/// as "no declared domain admits it", so this can never drift from what routing
/// actually does. That matters most for the two rules a text comparison cannot
/// see — an unlabeled nested repo (D6) and an unlisted child of a mixed parent
/// (D7) — which are exactly the shapes that would otherwise be dropped from the
/// index in silence. The partition fails silently; this is the lens.
pub fn unassigned_subdirs(cfg: &DomainConfig, workspace: &Path) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    collect_unassigned(cfg, workspace, workspace, 0, &mut out);
    out.sort();
    out
}

fn collect_unassigned(
    cfg: &DomainConfig,
    workspace: &Path,
    dir: &Path,
    depth: usize,
    out: &mut Vec<String>,
) {
    if depth >= MAX_REPORT_DEPTH {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let domains = cfg.domain_names();
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let path = e.path();
        if !path.is_dir() || name.starts_with('.') || SKIP.contains(&name.as_str()) {
            continue;
        }
        let Ok(rel) = path.strip_prefix(workspace) else {
            continue;
        };
        // Config entries are written with `/`, so report in the same alphabet
        // the user has to type back.
        let rel = rel.to_string_lossy().replace('\\', "/");

        if domains.iter().any(|d| cfg.includes(d, workspace, &path)) {
            // Admitted somewhere — but a nested repo BELOW it is quarantined
            // (D6), and past a carve-out its siblings are too (D7), so a claimed
            // directory is exactly where the interesting findings hide.
            collect_unassigned(cfg, workspace, &path, depth + 1, out);
        } else if !cfg.is_acknowledged(&rel) {
            // NOTE `undecided` is deliberately NOT filtered here: staged-but-
            // undecided is still unassigned, and this function answers "what
            // does no domain admit", not "what is unannotated". Callers that
            // want the unannotated set filter it themselves — `stage_undecided`
            // to keep staging idempotent, the audit to report a staged subdir
            // once, under its real state.
            //
            // Reported ONCE, as a whole tree, and not descended into: an
            // unassigned tree's children are all unassigned too, and listing
            // them would bury the one finding the user can act on under its own
            // contents.
            out.push(rel);
        }
    }
}

/// Where a `PathBuf` is wanted by the caller.
pub fn config_path(workspace: &Path) -> PathBuf {
    workspace.join(".nibdex-domains.toml")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn ws(toml: &str, dirs: &[&str]) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        for x in dirs {
            std::fs::create_dir_all(d.path().join(x)).unwrap();
        }
        std::fs::write(d.path().join(".nibdex-domains.toml"), toml).unwrap();
        d
    }

    /// THE MIGRATION CASE, and the reason discovery had to become depth-aware:
    /// `learn/python-sandbox` is a repo nested inside a labeled directory, so
    /// under D6 it is now indexed by NOBODY. Constraint 2 forbids dropping it
    /// silently, and the audit is the only lens on a partition that otherwise
    /// fails quietly — so it must be enumerated, as a PATH the user can hand
    /// straight back to `nibdex label`.
    #[test]
    fn unassigned_reports_a_nested_repo_as_a_path() {
        let d = ws(
            "[domains]\npersonal = [\"learn\", \"app\"]\n",
            &["app/.git", "learn/python-sandbox/.git", "learn/notes"],
        );
        let ws_p = d.path().canonicalize().unwrap();
        let cfg = DomainConfig::load(&ws_p).unwrap().unwrap();
        let pending = unassigned_subdirs(&cfg, &ws_p);

        assert!(
            pending.contains(&"learn/python-sandbox".to_string()),
            "a quarantined nested repo must be reported, as a path: {pending:?}"
        );
        // Its labeled parent and the parent's plain (non-repo) child are NOT
        // findings: `learn` has no deeper CONFIG entry yet, so it is not a mixed
        // parent and still sweeps `notes` normally.
        assert!(!pending.contains(&"learn".to_string()), "the labeled parent is fine");
        assert!(!pending.contains(&"learn/notes".to_string()), "a plain child still sweeps");

        // Deciding it is one command's worth of config, and it clears.
        let cfg = DomainConfig::parse_validated(
            "[domains]\npersonal = [\"learn\", \"app\", \"learn/python-sandbox\"]\n",
            "t",
        )
        .unwrap();
        let pending = unassigned_subdirs(&cfg, &ws_p);
        assert!(!pending.contains(&"learn/python-sandbox".to_string()), "labeling clears it");
        // ...but labeling it made `learn` a MIXED parent (D7), so its other
        // child directories now need listing too. That is the cost of the rule,
        // and it must show up here rather than as silently missing content.
        assert!(
            pending.contains(&"learn/notes".to_string()),
            "an unlisted child of a mixed parent must now be reported: {pending:?}"
        );
    }

    fn ask(answer: &str, domains: &[&str]) -> Decision {
        let mut input = Cursor::new(format!("{answer}\n"));
        let mut out: Vec<u8> = Vec::new();
        prompt_one("x", &Evidence::default(), domains, &mut input, &mut out).unwrap()
    }

    #[test]
    fn parses_the_answer_forms() {
        let d = &["personal", "client-a"];
        assert_eq!(ask("1", d), Decision::Label(vec!["personal".into()]));
        assert_eq!(ask("2", d), Decision::Label(vec!["client-a".into()]));
        assert_eq!(ask("a", d), Decision::Acknowledge);
        assert_eq!(ask("", d), Decision::Skip);
        // shared, both spellings
        assert_eq!(
            ask("s 1,2", d),
            Decision::Label(vec!["personal".into(), "client-a".into()])
        );
        assert_eq!(
            ask("1,2", d),
            Decision::Label(vec!["personal".into(), "client-a".into()])
        );
    }

    /// Anything unparseable must SKIP, never label. A misread answer that
    /// assigned a domain would silently widen that domain's reach.
    #[test]
    fn unparseable_input_skips_rather_than_guessing() {
        let d = &["personal", "client-a"];
        assert_eq!(ask("9", d), Decision::Skip);
        assert_eq!(ask("yes", d), Decision::Skip);
        assert_eq!(ask("-1", d), Decision::Skip);
        // EOF (piped, no input) must also skip
        let mut empty = Cursor::new(String::new());
        let mut out: Vec<u8> = Vec::new();
        assert_eq!(
            prompt_one("x", &Evidence::default(), d, &mut empty, &mut out).unwrap(),
            Decision::Skip
        );
    }

    #[test]
    fn apply_labels_and_preserves_comments() {
        let d = ws(
            "# my boundary file\n[domains]\n# personal things\npersonal = [\"app\"]\n",
            &["app", "scratch"],
        );
        let n = apply(
            d.path(),
            &[("scratch".into(), Decision::Label(vec!["personal".into()]))],
        )
        .unwrap();
        assert_eq!(n, 1);
        let s = std::fs::read_to_string(d.path().join(".nibdex-domains.toml")).unwrap();
        assert!(s.contains("# my boundary file"), "comments must survive: {s}");
        assert!(s.contains("# personal things"), "comments must survive: {s}");
        assert!(s.contains("scratch"), "{s}");
        let cfg = DomainConfig::load(d.path()).unwrap().unwrap();
        assert!(cfg.subdirs_of("personal").contains(&"scratch"));
    }

    #[test]
    fn apply_acknowledges_and_is_idempotent() {
        let d = ws("[domains]\npersonal = [\"app\"]\n", &["app", "junk"]);
        let dec = vec![("junk".to_string(), Decision::Acknowledge)];
        assert_eq!(apply(d.path(), &dec).unwrap(), 1);
        // second run changes nothing — re-triage must not duplicate entries
        assert_eq!(apply(d.path(), &dec).unwrap(), 0);
        let cfg = DomainConfig::load(d.path()).unwrap().unwrap();
        assert!(cfg.is_acknowledged("junk"));
    }

    /// A shared decision writes the subdir under EVERY chosen domain — that is
    /// how "shared" is expressed in this config model.
    #[test]
    fn apply_shares_across_domains() {
        let d = ws(
            "[domains]\npersonal = [\"app\"]\nclient-a = [\"acme\"]\n",
            &["app", "acme", "oss"],
        );
        apply(
            d.path(),
            &[(
                "oss".into(),
                Decision::Label(vec!["personal".into(), "client-a".into()]),
            )],
        )
        .unwrap();
        let cfg = DomainConfig::load(d.path()).unwrap().unwrap();
        assert!(cfg.subdirs_of("personal").contains(&"oss"));
        assert!(cfg.subdirs_of("client-a").contains(&"oss"));
    }

    /// Skip must be inert — the whole point of "decide later".
    #[test]
    fn skip_writes_nothing() {
        let d = ws("[domains]\npersonal = [\"app\"]\n", &["app", "later"]);
        let before = std::fs::read_to_string(d.path().join(".nibdex-domains.toml")).unwrap();
        assert_eq!(apply(d.path(), &[("later".into(), Decision::Skip)]).unwrap(), 0);
        let after = std::fs::read_to_string(d.path().join(".nibdex-domains.toml")).unwrap();
        assert_eq!(before, after);
    }

    /// The shape of the staged file IS the feature: one entry per line, with its
    /// evidence beside it, inside the real config — so deciding is a line move in
    /// an editor rather than a prompt session.
    #[test]
    fn staging_writes_one_entry_per_line_with_evidence_and_keeps_comments() {
        let d = ws(
            "# my boundary file\n[domains]\n# personal things\npersonal = [\"app\"]\n",
            &["app/.git", "mystery", "scratch"],
        );
        let ws_p = d.path().canonicalize().unwrap();
        std::fs::write(ws_p.join("mystery/a.rs"), "fn main() {}").unwrap();
        let cfg = DomainConfig::load(&ws_p).unwrap().unwrap();
        let pending = unassigned_subdirs(&cfg, &ws_p);
        assert_eq!(pending, vec!["mystery".to_string(), "scratch".to_string()]);

        let decisions = stage_undecided(&cfg, &pending);
        assert_eq!(apply(&ws_p, &decisions).unwrap(), 2);
        let s = std::fs::read_to_string(ws_p.join(".nibdex-domains.toml")).unwrap();
        // The user's own comments survive — the whole comment-preservation contract.
        assert!(s.contains("# my boundary file"), "{s}");
        assert!(s.contains("# personal things"), "{s}");
        // Evidence rides beside each entry, and it is OBSERVED, never a guess.
        assert!(s.contains("# not a git repo · 1 file(s)\n    \"mystery\","), "{s}");
        assert!(s.contains("# not a git repo · 0 file(s)\n    \"scratch\","), "{s}");
        assert!(!s.to_lowercase().contains("personal?"), "no suggested domain: {s}");
        // It reads back as exactly what was staged, and grants nothing.
        let cfg = DomainConfig::load(&ws_p).unwrap().unwrap();
        assert_eq!(cfg.undecided(), ["mystery".to_string(), "scratch".to_string()]);
        assert!(!cfg.includes("personal", &ws_p, &ws_p.join("mystery/a.rs")));
    }

    /// Re-staging must be a no-op: the audit is meant to be re-run, and a writer
    /// that appended a duplicate (or a second evidence comment) every run would
    /// make the boundary file unreadable — which is how it stops being audited.
    #[test]
    fn staging_is_idempotent() {
        let d = ws("[domains]\npersonal = [\"app\"]\n", &["app/.git", "mystery"]);
        let ws_p = d.path().canonicalize().unwrap();
        let cfg = DomainConfig::load(&ws_p).unwrap().unwrap();
        let pending = unassigned_subdirs(&cfg, &ws_p);
        assert_eq!(apply(&ws_p, &stage_undecided(&cfg, &pending)).unwrap(), 1);
        let first = std::fs::read_to_string(ws_p.join(".nibdex-domains.toml")).unwrap();

        // Reload: `mystery` is now staged, so there is nothing left to plan...
        let cfg = DomainConfig::load(&ws_p).unwrap().unwrap();
        let pending = unassigned_subdirs(&cfg, &ws_p);
        assert!(pending.contains(&"mystery".to_string()), "still unassigned");
        assert!(stage_undecided(&cfg, &pending).is_empty(), "nothing new to stage");
        // ...and applying the un-filtered set anyway still changes nothing.
        let all = vec![("mystery".to_string(), Decision::Undecided)];
        assert_eq!(apply(&ws_p, &all).unwrap(), 0);
        let second = std::fs::read_to_string(ws_p.join(".nibdex-domains.toml")).unwrap();
        assert_eq!(first, second, "re-staging must not touch the file");
    }

    /// Staging coexists with acknowledgement: the two lists carry opposite
    /// meanings and both must survive in one table.
    #[test]
    fn staging_and_acknowledgement_share_the_unassigned_table() {
        let d = ws("[domains]\npersonal = [\"app\"]\n", &["app/.git", "junk", "mystery"]);
        let ws_p = d.path().canonicalize().unwrap();
        apply(&ws_p, &[("junk".into(), Decision::Acknowledge)]).unwrap();
        apply(&ws_p, &[("mystery".into(), Decision::Undecided)]).unwrap();
        let cfg = DomainConfig::load(&ws_p).unwrap().unwrap();
        assert!(cfg.is_acknowledged("junk"));
        assert!(cfg.is_undecided("mystery"));
        assert!(!cfg.is_undecided("junk"));
        assert!(!cfg.is_acknowledged("mystery"));
    }

    /// Deciding = moving the string out of `undecided`. Once moved, the entry is
    /// gone and the directory is labeled — the file is the whole interface.
    #[test]
    fn moving_an_entry_out_of_undecided_labels_it() {
        let d = ws("[domains]\npersonal = [\"app\"]\n", &["app/.git", "mystery"]);
        let ws_p = d.path().canonicalize().unwrap();
        apply(&ws_p, &[("mystery".into(), Decision::Undecided)]).unwrap();
        // The edit a person would make in their editor: cut from one list, paste
        // into the other.
        let s = std::fs::read_to_string(ws_p.join(".nibdex-domains.toml")).unwrap();
        let moved = s
            .replace("personal = [\"app\"]", "personal = [\"app\", \"mystery\"]")
            .replace("\"mystery\",", "");
        std::fs::write(ws_p.join(".nibdex-domains.toml"), moved).unwrap();

        let cfg = DomainConfig::load(&ws_p).unwrap().unwrap();
        assert!(cfg.undecided().is_empty(), "the entry is gone");
        assert!(cfg.includes("personal", &ws_p, &ws_p.join("mystery/a.rs")));
        assert!(unassigned_subdirs(&cfg, &ws_p).is_empty(), "audit converges to quiet");
    }

    /// A half-finished move (copied instead of cut) must be REFUSED before the
    /// file is touched. Writing first and validating after would leave the
    /// security boundary broken on disk and make recovery the user's problem.
    #[test]
    fn apply_refuses_to_write_a_contradictory_config_and_leaves_it_untouched() {
        let d = ws("[domains]\npersonal = [\"app\"]\n", &["app/.git"]);
        let ws_p = d.path().canonicalize().unwrap();
        let before = std::fs::read_to_string(ws_p.join(".nibdex-domains.toml")).unwrap();
        // Staging something that is ALREADY labeled is the contradiction.
        let err = apply(&ws_p, &[("app".into(), Decision::Undecided)])
            .unwrap_err()
            .to_string();
        assert!(err.contains("refusing to write"), "{err}");
        let after = std::fs::read_to_string(ws_p.join(".nibdex-domains.toml")).unwrap();
        assert_eq!(before, after, "a rejected write must not touch the file");
    }

    #[test]
    fn render_plan_shows_staging_as_its_own_line() {
        let out = render_plan(&[("mystery".into(), Decision::Undecided)], &[]);
        assert!(out.contains("[unassigned] undecided"), "{out}");
        assert!(out.contains("\"mystery\""), "{out}");
    }

    /// Deciding a STAGED subdir has to complete the move, not collide with it.
    /// Without this the three triage avenues fight: stage a directory, then try
    /// to label it, and the loader rejects the result as a contradiction.
    #[test]
    fn deciding_a_staged_subdir_retires_the_staged_entry() {
        for (decision, check) in [
            (Decision::Label(vec!["personal".into()]), true),
            (Decision::Acknowledge, false),
        ] {
            let d = ws("[domains]\npersonal = [\"app\"]\n", &["app/.git", "mystery"]);
            let ws_p = d.path().canonicalize().unwrap();
            apply(&ws_p, &[("mystery".into(), Decision::Undecided)]).unwrap();
            let staged = std::fs::read_to_string(ws_p.join(".nibdex-domains.toml")).unwrap();
            assert!(staged.contains("\"mystery\""), "staged first: {staged}");

            // The decision must apply cleanly and leave a VALID config.
            apply(&ws_p, &[("mystery".to_string(), decision)]).unwrap();
            let cfg = DomainConfig::load(&ws_p).unwrap().unwrap();
            assert!(cfg.undecided().is_empty(), "the staged entry is retired");
            if check {
                assert!(cfg.subdirs_of("personal").contains(&"mystery"));
            } else {
                assert!(cfg.is_acknowledged("mystery"));
            }
            // The evidence comment leaves with its entry — no orphan left
            // hanging over whatever entry follows it.
            let after = std::fs::read_to_string(ws_p.join(".nibdex-domains.toml")).unwrap();
            assert!(!after.contains("file(s)"), "evidence comment retired too: {after}");
            assert!(unassigned_subdirs(&cfg, &ws_p).is_empty(), "audit converges");
        }
    }

    /// A confirmation prompt that understated the change would not be a
    /// confirmation — the retirement is part of what the user is approving.
    #[test]
    fn render_plan_shows_both_halves_of_the_move() {
        let staged = vec!["mystery".to_string()];
        let out = render_plan(&[("mystery".into(), Decision::Label(vec!["personal".into()]))], &staged);
        assert!(out.contains("[domains] personal  += \"mystery\""), "{out}");
        assert!(out.contains("undecided    -= \"mystery\""), "{out}");
        // An unstaged subdir shows only the half that happens.
        let out = render_plan(&[("other".into(), Decision::Acknowledge)], &staged);
        assert!(!out.contains("-="), "{out}");
    }

    #[test]
    fn unassigned_excludes_labeled_and_acknowledged() {
        let d = ws(
            "[domains]\npersonal = [\"app\"]\n\n[unassigned]\nacknowledged = [\"junk\"]\n",
            &["app", "junk", "mystery"],
        );
        let ws_p = d.path().canonicalize().unwrap();
        let cfg = DomainConfig::load(&ws_p).unwrap().unwrap();
        assert_eq!(unassigned_subdirs(&cfg, &ws_p), vec!["mystery".to_string()]);
    }
}
