// SPDX-License-Identifier: MIT

//! Git-commits extractor — libgit2 HEAD-ancestry walk per repository.
//!
//! DESIGN §5.3 (libgit2 walk via `git2`) + §14.4 (Day 4 build steps).
//! Discovery seeds `indexed_repos`; the walk produces `CommitRow` records that
//! the indexer persists to `commit_entries`. Incremental via `revwalk.hide(last_oid)`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use git2::{Commit, Repository, Sort};
use walkdir::WalkDir;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NestedMode {
    Skip,
    Include,
}

#[derive(Debug, Clone)]
pub struct CommitRow {
    pub commit_hash: String,
    pub parent_hashes: Vec<String>,
    pub author_email: Option<String>,
    pub author_name: Option<String>,
    pub authored_at: i64,
    pub committed_at: i64,
    pub message_summary: String,
    pub message_body: Option<String>,
    pub files_changed: Vec<String>,
}

#[derive(Debug)]
pub struct ExtractResult {
    pub commits: Vec<CommitRow>,
    pub head_oid: Option<String>,
    pub is_shallow: bool,
    pub capped: bool,
}

/// Walk the workspace to `max_depth` and collect repo roots (parent of any `.git/` directory).
///
/// `mode = Skip` drops repos whose root is a descendant of another kept repo (common
/// case). `mode = Include` keeps every discovered repo — needed for monorepo
/// workspaces where the parent and each nested service repo all carry history.
pub fn discover_repos(workspace: &Path, max_depth: usize, mode: NestedMode) -> Vec<PathBuf> {
    let mut repo_roots: Vec<PathBuf> = Vec::new();
    let mut it = WalkDir::new(workspace).max_depth(max_depth).into_iter();
    loop {
        let entry = match it.next() {
            None => break,
            Some(Err(_)) => continue,
            Some(Ok(e)) => e,
        };
        if entry.file_type().is_dir() && entry.file_name() == ".git" {
            if let Some(parent) = entry.path().parent() {
                repo_roots.push(parent.to_path_buf());
            }
            it.skip_current_dir();
        }
    }
    sort_and_filter(repo_roots, mode)
}

fn sort_and_filter(mut roots: Vec<PathBuf>, mode: NestedMode) -> Vec<PathBuf> {
    roots.sort_by(|a, b| {
        a.components()
            .count()
            .cmp(&b.components().count())
            .then_with(|| a.cmp(b))
    });
    match mode {
        NestedMode::Include => roots,
        NestedMode::Skip => {
            let mut kept: Vec<PathBuf> = Vec::new();
            for r in roots {
                let nested = kept.iter().any(|k| r != *k && r.starts_with(k));
                if !nested {
                    kept.push(r);
                }
            }
            kept
        }
    }
}

/// Walk HEAD ancestry of `repo_path`, returning new commits since `last_indexed_oid`
/// (or all commits if None). Cap respects `max_commits` to defend against pathological
/// histories per DESIGN §9.12.
pub fn extract_commits(
    repo_path: &Path,
    last_indexed_oid: Option<&str>,
    max_commits: usize,
) -> Result<ExtractResult> {
    let repo = Repository::open(repo_path)
        .with_context(|| format!("git2::Repository::open({repo_path:?})"))?;
    let is_shallow = repo.is_shallow();
    let head_oid = match repo.head() {
        Ok(reference) => reference.peel_to_commit().ok().map(|c| c.id().to_string()),
        Err(_) => None,
    };

    let mut walk = repo.revwalk()?;
    walk.set_sorting(Sort::TIME)?;
    if walk.push_head().is_err() {
        // Empty repository (no HEAD); return early with no commits.
        return Ok(ExtractResult {
            commits: Vec::new(),
            head_oid,
            is_shallow,
            capped: false,
        });
    }
    if let Some(oid_str) = last_indexed_oid
        && let Ok(oid) = git2::Oid::from_str(oid_str)
    {
        // Tolerate stale cursors (force-push, history rewrite): if the OID isn't
        // present in this repo, hide() errors and we silently fall back to a full walk.
        let _ = walk.hide(oid);
    }

    let mut commits: Vec<CommitRow> = Vec::with_capacity(64);
    let mut capped = false;
    for oid_res in walk {
        if commits.len() >= max_commits {
            capped = true;
            break;
        }
        let oid = match oid_res {
            Ok(o) => o,
            Err(_) => continue,
        };
        let commit = match repo.find_commit(oid) {
            Ok(c) => c,
            Err(_) => continue,
        };
        commits.push(commit_to_row(&repo, &commit)?);
    }
    Ok(ExtractResult {
        commits,
        head_oid,
        is_shallow,
        capped,
    })
}

fn commit_to_row(repo: &Repository, commit: &Commit) -> Result<CommitRow> {
    let author = commit.author();
    let committer = commit.committer();
    let (summary, body) = split_message(commit.message().unwrap_or(""));
    let files_changed = files_changed(repo, commit).unwrap_or_default();
    Ok(CommitRow {
        commit_hash: commit.id().to_string(),
        parent_hashes: commit.parent_ids().map(|p| p.to_string()).collect(),
        author_email: author.email().ok().map(|s| s.to_string()),
        author_name: author.name().ok().map(|s| s.to_string()),
        authored_at: author.when().seconds(),
        committed_at: committer.when().seconds(),
        message_summary: summary,
        message_body: body,
        files_changed,
    })
}

/// Split a raw commit message into (summary, body).
/// Summary = first line; body = remainder after the first blank line, trimmed.
pub fn split_message(raw: &str) -> (String, Option<String>) {
    let raw = raw.trim_end_matches('\n');
    let (first, rest) = match raw.find('\n') {
        Some(idx) => (&raw[..idx], &raw[idx + 1..]),
        None => (raw, ""),
    };
    let summary = first.trim().to_string();
    let body_trimmed = rest.trim_start_matches('\n').trim_end().to_string();
    let body = if body_trimmed.is_empty() {
        None
    } else {
        Some(body_trimmed)
    };
    (summary, body)
}

fn files_changed(repo: &Repository, commit: &Commit) -> Result<Vec<String>> {
    let tree = commit.tree()?;
    let parent_tree = if commit.parent_count() > 0 {
        Some(commit.parent(0)?.tree()?)
    } else {
        None
    };
    let mut opts = git2::DiffOptions::new();
    let diff = repo.diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), Some(&mut opts))?;
    let mut files: Vec<String> = Vec::new();
    diff.foreach(
        &mut |delta, _| {
            let path = delta.new_file().path().or_else(|| delta.old_file().path());
            if let Some(p) = path {
                files.push(p.to_string_lossy().into_owned());
            }
            true
        },
        None,
        None,
        None,
    )?;
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_message_summary_only() {
        let (s, b) = split_message("docs: tidy up");
        assert_eq!(s, "docs: tidy up");
        assert!(b.is_none());
    }

    #[test]
    fn split_message_summary_and_body() {
        let (s, b) = split_message("feat: thing\n\nbody line 1\nbody line 2\n");
        assert_eq!(s, "feat: thing");
        assert_eq!(b.as_deref(), Some("body line 1\nbody line 2"));
    }

    #[test]
    fn split_message_summary_only_with_trailing_newline() {
        let (s, b) = split_message("fix: bug\n");
        assert_eq!(s, "fix: bug");
        assert!(b.is_none());
    }

    #[test]
    fn split_message_no_blank_line_between() {
        // git allows messages without a blank line; the second line still counts as body.
        let (s, b) = split_message("subj\nmore");
        assert_eq!(s, "subj");
        assert_eq!(b.as_deref(), Some("more"));
    }

    #[test]
    fn sort_and_filter_skip_drops_nested() {
        let roots = vec![
            PathBuf::from("/ws/a"),
            PathBuf::from("/ws"),
            PathBuf::from("/ws/b"),
            PathBuf::from("/ws/a/sub"),
        ];
        let kept = sort_and_filter(roots, NestedMode::Skip);
        assert_eq!(kept, vec![PathBuf::from("/ws")]);
    }

    #[test]
    fn sort_and_filter_include_keeps_all_sorted() {
        let roots = vec![
            PathBuf::from("/ws/a"),
            PathBuf::from("/ws"),
            PathBuf::from("/ws/b"),
        ];
        let kept = sort_and_filter(roots, NestedMode::Include);
        assert_eq!(
            kept,
            vec![
                PathBuf::from("/ws"),
                PathBuf::from("/ws/a"),
                PathBuf::from("/ws/b"),
            ]
        );
    }

    #[test]
    fn sort_and_filter_skip_no_overlap_keeps_siblings() {
        let roots = vec![PathBuf::from("/ws/a"), PathBuf::from("/ws/b")];
        let kept = sort_and_filter(roots, NestedMode::Skip);
        assert_eq!(kept, vec![PathBuf::from("/ws/a"), PathBuf::from("/ws/b")]);
    }

    #[test]
    fn sort_and_filter_skip_distinguishes_prefix_neighbors() {
        // /ws/aa must NOT be filtered as nested under /ws/a.
        let roots = vec![PathBuf::from("/ws/a"), PathBuf::from("/ws/aa")];
        let kept = sort_and_filter(roots, NestedMode::Skip);
        assert_eq!(kept, vec![PathBuf::from("/ws/a"), PathBuf::from("/ws/aa")]);
    }
}
