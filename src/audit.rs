// SPDX-License-Identifier: MIT

//! `nibdex audit` — a user-reviewable report on what a domain's index actually
//! covers, and on config that cannot work as written.
//!
//! WHY THIS EXISTS. The IP-domain partition fails quietly in both directions.
//! Content that reaches no domain produces no error — a workspace-root tracker,
//! a labeled directory that isn't a git repo, an untracked file — it is simply
//! absent, and the user discovers it as "why can't nibdex find my TODO list?"
//! months later. Meanwhile the isolation guarantee is mechanical for *artifacts*
//! but can never be mechanical for *sentences* (SECURITY.md §6), so the residual
//! is judged by a human or not at all.
//!
//! Both problems have the same answer: stop asking the tool to decide, and give
//! the human a lens. This module reports; it never infers a domain for anything.
//! Guessing which domain a directory belongs to is precisely the inference that
//! would mis-route IP, so the audit presents facts and leaves the decision where
//! it belongs — in `.nibdex-domains.toml`, where it is auditable in one place.
//!
//! It is a DETECTIVE control, not a preventive one. It complements the §4
//! invariant test; if it ever becomes the reason to relax a mechanical
//! guarantee, that is a regression.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sqlx::SqlitePool;

use crate::domains::DomainConfig;

/// How much a finding should worry the reader. Ordered worst-first so a report
/// can be sorted and so a caller can gate on `Error`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// Config that cannot work as written — indexing will silently miss things.
    Error,
    /// Works, but covers less than the user almost certainly intends.
    Warn,
    /// Worth a decision, not necessarily a problem.
    Info,
}

impl Severity {
    pub fn label(self) -> &'static str {
        match self {
            Self::Error => "ERROR",
            Self::Warn => "WARN",
            Self::Info => "INFO",
        }
    }
}

#[derive(Debug)]
pub struct Finding {
    pub severity: Severity,
    /// Short machine-ish category, e.g. `label-not-a-repo`.
    pub kind: &'static str,
    /// What was found, in one line.
    pub detail: String,
    /// What the user can do about it. Never phrased as something nibdex will do.
    pub remedy: &'static str,
}

#[derive(Debug, Default)]
pub struct AuditReport {
    pub findings: Vec<Finding>,
    pub domain: String,
    /// Documents present in the audited db, by corpus kind.
    pub indexed_docs: usize,
}

impl AuditReport {
    pub fn worst(&self) -> Option<Severity> {
        self.findings.iter().map(|f| f.severity).min()
    }

    fn push(&mut self, severity: Severity, kind: &'static str, detail: String, remedy: &'static str) {
        self.findings.push(Finding { severity, kind, detail, remedy });
    }
}

/// Directories that are build output or vendored deps — never interesting as
/// "content the user expected to be indexed".
const SKIP_DIRS: &[&str] = &[
    ".git", "node_modules", "target", ".venv", "venv", "dist", "build", "__pycache__", ".next",
];

/// Extensions worth reporting as missing. Deliberately narrow: an audit that
/// lists every `.png` and `.lock` is an audit nobody reads.
const CONTENT_EXTS: &[&str] = &[
    "md", "rs", "py", "ts", "tsx", "js", "jsx", "sql", "toml", "yaml", "yml", "sh", "go", "java",
];

fn is_content_file(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| CONTENT_EXTS.contains(&e))
}

/// CHECK 1 — config sanity. Pure filesystem + config; needs no database, so it
/// works before a first index (which is when it is most useful).
fn check_config(report: &mut AuditReport, cfg: &DomainConfig, workspace: &Path, domain: &str) {
    for sub in cfg.subdirs_of(domain) {
        let p = workspace.join(sub);
        if !p.exists() {
            report.push(
                Severity::Error,
                "label-missing",
                format!("'{sub}' is labeled for '{domain}' but does not exist"),
                "Remove it from .nibdex-domains.toml, or create it.",
            );
        } else if p.is_file() {
            report.push(
                Severity::Error,
                "label-is-a-file",
                format!("'{sub}' is a file; domain labels must be directories"),
                "Labels are top-level subdirectories. Move the file into one.",
            );
        } else if !p.join(".git").exists() {
            // The convention trap: a labeled dir that is not a repo is not a
            // design-doc discovery anchor, so its *.md and docs/** reach nothing.
            report.push(
                Severity::Warn,
                "label-not-a-repo",
                format!("'{sub}' is labeled but is not a git repository"),
                "Design-doc discovery anchors on repo roots, so this directory's \
                 *.md and docs/** are not indexed. Run `git init` in it, or label \
                 the repositories inside it instead.",
            );
        }
    }

    // Subdirs no domain admits: indexed by nobody, AND they taint sessions.
    // Sourced from `triage::unassigned_subdirs` — the SAME function the triage
    // verbs act on, itself derived from `includes` — so the report, the staging
    // and the routing cannot disagree. `undecided` entries are filtered there
    // and reported separately below, so a staged subdir produces one finding
    // about its real state, not two.
    for name in crate::triage::unassigned_subdirs(cfg, workspace)
        .into_iter()
        .filter(|n| !cfg.is_undecided(n))
    {
        // A nested tree is the case the user cannot guess at: it reads as
        // covered because its PARENT is labeled. Say which rule withdrew it and
        // show the evidence, never a suggested domain.
        let nested = name.contains('/');
        let why = if !nested {
            String::new()
        } else if workspace.join(&name).join(".git").exists() {
            format!(
                " — it is a git repository nested inside a labeled directory, so it is \
                 indexed by NOBODY until it is named ({name} is not covered by its parent's label)"
            )
        } else {
            " — a deeper entry exists under its parent, so that parent no longer sweeps \
             its other child directories; this one must be listed to be indexed"
                .to_string()
        };
        report.push(
            Severity::Info,
            "subdir-unassigned",
            format!("'{name}/' is in no domain and has not been acknowledged{why}"),
            "Label it under a domain (`nibdex label <path> --domain X`, or list it \
             under several to share it), or record the decision in [unassigned] \
             acknowledged = [...]. To decide in the config file itself, run \
             `nibdex audit --stage-undecided`. Either way it still taints sessions \
             that touch it — acknowledging is a statement about coverage, not about \
             trust.",
        );
    }

    // Staged-but-still-undecided. Reported so the list converges to empty: an
    // annotation that were silent would just be a slower way of forgetting.
    for name in cfg.undecided() {
        let detail = if workspace.join(name).is_dir() {
            format!("'{name}/' is staged in [unassigned] undecided — no decision yet")
        } else {
            format!("'{name}' is staged in [unassigned] undecided but is not a directory here")
        };
        report.push(
            Severity::Info,
            "subdir-undecided",
            detail,
            "Edit .nibdex-domains.toml: move it into a domain's list under [domains] \
             to index it, or into [unassigned] acknowledged to record that it stays \
             out — or delete the entry if the directory is gone. Removing it from \
             `undecided` is what silences this; the entry itself grants nothing.",
        );
    }

    if !cfg.claims_memory(domain) {
        report.push(
            Severity::Info,
            "memory-unclaimed",
            "no domain claims the workspace memory dir".to_string(),
            "find_memory will return nothing for this domain. Add a [memory] table \
             to claim it — but only if this workspace holds a single domain, since \
             the claim is an assertion nibdex cannot verify.",
        );
    }
}

/// CHECK 2 — coverage. What exists in the workspace but is absent from this
/// domain's database. Needs the db, so it runs after an index.
async fn check_coverage(
    report: &mut AuditReport,
    pool: &SqlitePool,
    cfg: &DomainConfig,
    workspace: &Path,
    domain: &str,
) -> Result<()> {
    let rows: Vec<(String,)> = sqlx::query_as("SELECT path FROM documents")
        .fetch_all(pool)
        .await
        .context("reading indexed document paths")?;
    let indexed: BTreeSet<PathBuf> = rows.into_iter().map(|(p,)| PathBuf::from(p)).collect();
    report.indexed_docs = indexed.len();

    // Workspace-root content files. Container metadata living here is correct;
    // anything else is the silent-absence trap the convention exists to fix.
    let mut root_missing: Vec<String> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(workspace) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_file() && is_content_file(&p) && !indexed.contains(&p) {
                root_missing.push(e.file_name().to_string_lossy().into_owned());
            }
        }
    }
    if !root_missing.is_empty() {
        root_missing.sort();
        report.push(
            Severity::Info,
            "root-file-unindexed",
            format!(
                "{} workspace-root file(s) reached no domain: {}",
                root_missing.len(),
                root_missing.join(", ")
            ),
            "Expected for container metadata (README, CLAUDE.md, LICENSE, configs). \
             If any is cross-cutting CONTENT, move it into a labeled subdirectory \
             that is also its own git repo — root files reach no domain and there \
             is no error when they don't.",
        );
    }

    // Content under THIS domain's own labels that did not make it in. Unlike the
    // root case this is not expected, so it ranks higher.
    for sub in cfg.subdirs_of(domain) {
        let root = workspace.join(sub);
        let mut missing: Vec<String> = Vec::new();
        for entry in walkdir::WalkDir::new(&root)
            .into_iter()
            .filter_entry(|e| {
                let n = e.file_name().to_string_lossy();
                !(e.file_type().is_dir() && (SKIP_DIRS.contains(&n.as_ref()) || n.starts_with('.')))
            })
            .flatten()
        {
            let p = entry.path();
            if p.is_file() && is_content_file(p) && !indexed.contains(p) {
                missing.push(
                    p.strip_prefix(workspace)
                        .unwrap_or(p)
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
        if !missing.is_empty() {
            missing.sort();
            let shown: Vec<String> = missing.iter().take(8).cloned().collect();
            report.push(
                Severity::Warn,
                "labeled-content-unindexed",
                format!(
                    "{} file(s) under labeled '{sub}' are not indexed: {}{}",
                    missing.len(),
                    shown.join(", "),
                    if missing.len() > shown.len() { ", …" } else { "" }
                ),
                "Common causes: the file is untracked in git (source indexing walks \
                 the git index, so uncommitted files are invisible by design), or \
                 the directory is not a git repository. Commit them, or run \
                 `git init`.",
            );
        }
    }
    Ok(())
}

/// Run the audit for one domain against one database.
pub async fn run(
    pool: &SqlitePool,
    workspace: &Path,
    domain: &str,
    skip_coverage: bool,
) -> Result<AuditReport> {
    let workspace = workspace
        .canonicalize()
        .with_context(|| format!("canonicalize workspace {workspace:?}"))?;
    let cfg = DomainConfig::load(&workspace)?.ok_or_else(|| {
        anyhow::anyhow!(
            "no .nibdex-domains.toml at {} — audit is for domain-partitioned \
             workspaces; an unpartitioned index has no domain boundary to audit",
            workspace.display()
        )
    })?;
    if !cfg.has_domain(domain) {
        anyhow::bail!(
            "domain {domain:?} is not declared in .nibdex-domains.toml (declared: {:?})",
            cfg.domain_names()
        );
    }

    let mut report = AuditReport { domain: domain.to_string(), ..Default::default() };
    check_config(&mut report, &cfg, &workspace, domain);
    if !skip_coverage {
        check_coverage(&mut report, pool, &cfg, &workspace, domain).await?;
    }
    report.findings.sort_by_key(|f| f.severity);
    Ok(report)
}

/// Machine-readable rendering, for scripts and setup automation.
///
/// Hand-built rather than derived: the shape is a CONTRACT for whatever consumes
/// it, and deriving it would let an internal field rename silently break a
/// caller. `unassigned` is lifted out as its own list because assigning those is
/// the whole point of consuming this — a caller should not have to parse prose
/// out of `detail` to find them. `undecided` is the SUBSET of `unassigned`
/// already staged into the config file; both appear because a caller needs to
/// know which entries are new and which are merely still awaiting a decision.
pub fn render_json(report: &AuditReport, unassigned: &[String], undecided: &[String]) -> String {
    let findings: Vec<serde_json::Value> = report
        .findings
        .iter()
        .map(|f| {
            serde_json::json!({
                "severity": f.severity.label(),
                "kind": f.kind,
                "detail": f.detail,
                "remedy": f.remedy,
            })
        })
        .collect();
    serde_json::json!({
        "domain": report.domain,
        "indexed_docs": report.indexed_docs,
        "worst_severity": report.worst().map(Severity::label),
        "findings": findings,
        // The actionable subset: subdirs in no domain and not acknowledged.
        "unassigned": unassigned,
        // Of those, the ones already written into [unassigned] undecided.
        "undecided": undecided,
        "note": "Reports coverage and config only. Cannot judge whether text in \
                 this database discusses another domain; that residual is semantic \
                 (SECURITY.md). No findings means none were found, not that none exist.",
    })
    .to_string()
}

/// Human-readable rendering. Deliberately states what the audit CANNOT do —
/// a report that reads as a clean bill of health would manufacture exactly the
/// false confidence this feature exists to replace.
pub fn render(report: &AuditReport) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(out, "nibdex audit — domain '{}'", report.domain);
    let _ = writeln!(out, "  {} document(s) indexed", report.indexed_docs);
    if report.findings.is_empty() {
        let _ = writeln!(out, "\n  No findings.");
    } else {
        let _ = writeln!(out);
        for f in &report.findings {
            let _ = writeln!(out, "  {:5}  [{}] {}", f.severity.label(), f.kind, f.detail);
            let _ = writeln!(out, "         → {}", f.remedy);
        }
    }
    let _ = writeln!(
        out,
        "\nThis audit reports COVERAGE and CONFIG. It does not — and cannot — tell you\n\
         whether text in this database discusses another domain: that residual is\n\
         semantic, and no index can judge it (SECURITY.md). A clean audit means\n\
         nothing was found, not that nothing is there."
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws_with(toml: &str, subdirs: &[&str]) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        for s in subdirs {
            std::fs::create_dir_all(d.path().join(s)).unwrap();
        }
        std::fs::write(d.path().join(".nibdex-domains.toml"), toml).unwrap();
        d
    }

    fn findings_of(cfg: &DomainConfig, ws: &Path, domain: &str) -> Vec<(Severity, &'static str)> {
        let mut r = AuditReport { domain: domain.into(), ..Default::default() };
        check_config(&mut r, cfg, ws, domain);
        r.findings.iter().map(|f| (f.severity, f.kind)).collect()
    }

    #[test]
    fn flags_a_labeled_dir_that_is_not_a_repo() {
        let d = ws_with("[domains]\npersonal = [\"app\"]\n", &["app"]);
        let ws = d.path().canonicalize().unwrap();
        let cfg = DomainConfig::load(&ws).unwrap().unwrap();
        let f = findings_of(&cfg, &ws, "personal");
        assert!(f.contains(&(Severity::Warn, "label-not-a-repo")), "{f:?}");
        // ...and stops flagging once it IS a repo.
        std::fs::create_dir_all(ws.join("app/.git")).unwrap();
        let f = findings_of(&cfg, &ws, "personal");
        assert!(!f.iter().any(|(_, k)| *k == "label-not-a-repo"), "{f:?}");
    }

    #[test]
    fn flags_a_label_that_does_not_exist_or_is_a_file() {
        let d = ws_with("[domains]\npersonal = [\"ghost\", \"afile\"]\n", &[]);
        let ws = d.path().canonicalize().unwrap();
        std::fs::write(ws.join("afile"), "x").unwrap();
        let cfg = DomainConfig::load(&ws).unwrap().unwrap();
        let f = findings_of(&cfg, &ws, "personal");
        assert!(f.contains(&(Severity::Error, "label-missing")), "{f:?}");
        assert!(f.contains(&(Severity::Error, "label-is-a-file")), "{f:?}");
    }

    /// THE point of `[unassigned]`: the audit must converge to silence once the
    /// user has made a decision, or it becomes noise and stops being read.
    #[test]
    fn acknowledgement_silences_an_unassigned_subdir() {
        let d = ws_with("[domains]\npersonal = [\"app\"]\n", &["app/.git", "scratch"]);
        let ws = d.path().canonicalize().unwrap();
        let cfg = DomainConfig::load(&ws).unwrap().unwrap();
        let f = findings_of(&cfg, &ws, "personal");
        assert!(f.contains(&(Severity::Info, "subdir-unassigned")), "{f:?}");

        std::fs::write(
            ws.join(".nibdex-domains.toml"),
            "[domains]\npersonal = [\"app\"]\n\n[unassigned]\nacknowledged = [\"scratch\"]\n",
        )
        .unwrap();
        let cfg = DomainConfig::load(&ws).unwrap().unwrap();
        let f = findings_of(&cfg, &ws, "personal");
        assert!(!f.iter().any(|(_, k)| *k == "subdir-unassigned"), "{f:?}");
    }

    /// A staged subdir must produce ONE finding describing its real state, not
    /// two describing it twice. An audit is only read while it stays short.
    #[test]
    fn staging_replaces_the_unassigned_finding_rather_than_adding_one() {
        let d = ws_with("[domains]\npersonal = [\"app\"]\n", &["app/.git", "mystery"]);
        let ws = d.path().canonicalize().unwrap();
        let cfg = DomainConfig::load(&ws).unwrap().unwrap();
        let f = findings_of(&cfg, &ws, "personal");
        assert!(f.contains(&(Severity::Info, "subdir-unassigned")), "{f:?}");
        assert!(!f.iter().any(|(_, k)| *k == "subdir-undecided"), "{f:?}");

        std::fs::write(
            ws.join(".nibdex-domains.toml"),
            "[domains]\npersonal = [\"app\"]\n\n[unassigned]\nundecided = [\"mystery\"]\n",
        )
        .unwrap();
        let cfg = DomainConfig::load(&ws).unwrap().unwrap();
        let f = findings_of(&cfg, &ws, "personal");
        assert!(f.contains(&(Severity::Info, "subdir-undecided")), "{f:?}");
        assert!(!f.iter().any(|(_, k)| *k == "subdir-unassigned"), "{f:?}");
        assert_eq!(
            f.iter().filter(|(_, k)| k.starts_with("subdir-")).count(),
            1,
            "exactly one finding per subdirectory: {f:?}"
        );
    }

    /// `undecided` has to converge to silence once the decision is made, or it
    /// is merely a slower way of forgetting — the same bar `acknowledged` met.
    #[test]
    fn deciding_an_undecided_entry_silences_the_audit() {
        let d = ws_with(
            "[domains]\npersonal = [\"app\", \"mystery\"]\n",
            &["app/.git", "mystery/.git"],
        );
        let ws = d.path().canonicalize().unwrap();
        let cfg = DomainConfig::load(&ws).unwrap().unwrap();
        let f = findings_of(&cfg, &ws, "personal");
        assert!(
            !f.iter().any(|(_, k)| k.starts_with("subdir-")),
            "a decided workspace reports no subdir findings: {f:?}"
        );
    }

    /// An entry left behind after its directory is gone still reports, but says
    /// which case it is — "move it into a domain" is not the remedy for a name
    /// that no longer refers to anything.
    #[test]
    fn an_undecided_entry_for_a_vanished_directory_says_so() {
        let d = ws_with(
            "[domains]\npersonal = [\"app\"]\n\n[unassigned]\nundecided = [\"ghost\"]\n",
            &["app/.git"],
        );
        let ws = d.path().canonicalize().unwrap();
        let cfg = DomainConfig::load(&ws).unwrap().unwrap();
        let mut r = AuditReport { domain: "personal".into(), ..Default::default() };
        check_config(&mut r, &cfg, &ws, "personal");
        let found = r
            .findings
            .iter()
            .find(|f| f.kind == "subdir-undecided")
            .expect("a staged entry is always reported");
        assert!(found.detail.contains("not a directory"), "{}", found.detail);
    }

    /// The JSON shape is a contract for setup automation: a caller needs to tell
    /// "never triaged" from "staged, still awaiting a decision".
    #[test]
    fn json_carries_both_the_unassigned_and_the_staged_lists() {
        let r = AuditReport { domain: "personal".into(), ..Default::default() };
        let out = render_json(&r, &["mystery".into(), "scratch".into()], &["scratch".into()]);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["unassigned"], serde_json::json!(["mystery", "scratch"]));
        assert_eq!(v["undecided"], serde_json::json!(["scratch"]));
    }

    /// Acknowledging something that is also labeled is a contradiction, not a
    /// silent precedence rule.
    #[test]
    fn acknowledging_a_labeled_subdir_is_a_load_error() {
        let d = ws_with(
            "[domains]\npersonal = [\"app\"]\n\n[unassigned]\nacknowledged = [\"app\"]\n",
            &["app"],
        );
        let err = DomainConfig::load(d.path()).unwrap_err().to_string();
        assert!(err.contains("also labeled"), "{err}");
    }

    /// Acknowledgement is an audit annotation only — it must NOT make a subdir
    /// visible to the domain, or it would become a backdoor label.
    #[test]
    fn acknowledgement_grants_no_visibility() {
        let d = ws_with(
            "[domains]\npersonal = [\"app\"]\n\n[unassigned]\nacknowledged = [\"other\"]\n",
            &["app", "other"],
        );
        let ws = d.path().canonicalize().unwrap();
        let cfg = DomainConfig::load(&ws).unwrap().unwrap();
        assert!(cfg.is_acknowledged("other"));
        assert!(
            !cfg.includes("personal", &ws, &ws.join("other/x.rs")),
            "acknowledgement must never grant domain visibility"
        );
    }
}
