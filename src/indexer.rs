// SPDX-License-Identifier: MIT

use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde_json::json;
use sqlx::SqlitePool;
use walkdir::WalkDir;

use crate::domains::DomainConfig;
use crate::extractor::design_doc;
use crate::extractor::git_commits::{self, NestedMode};
use crate::extractor::memory;
use crate::extractor::session_history;
use crate::hash::sha256_file;
use crate::metrics::Op;
use crate::session_index;

#[derive(Debug, Default)]
pub struct ScanStats {
    pub session_history: u32,
    pub memory: u32,
    pub design_doc: u32,
    pub session_entries: u32,
    pub memory_entries: u32,
    pub memory_skipped_no_frontmatter: u32,
    pub design_sections: u32,
    pub repos_indexed: u32,
    pub repos_capped: u32,
    pub repos_shallow: u32,
    pub commits_inserted: u32,
    /// D1a: source files indexed (kind='source') across all repos.
    pub source_files: u32,
    /// D1a: source chunks (fixed line-windows) inserted.
    pub source_chunks: u32,
    /// D1a: source files skipped because another corpus owns them (design/session).
    pub source_skipped_other_corpus: u32,
    /// D1a: previously-indexed source files no longer git-tracked, removed this pass.
    pub source_files_pruned: u32,
    /// Transcript write-edges indexed (the `find_session` corpus).
    pub session_edges: u32,
    /// Transcripts read while gathering those edges, across every slug.
    pub session_transcripts: u32,
    /// Edges dropped because their session was working outside this workspace.
    pub session_edges_dropped_foreign_workspace: u32,
    /// Edges dropped because an in-workspace session wrote outside the workspace
    /// (and outside the domain-neutral set).
    pub session_edges_dropped_foreign_target: u32,
    /// Transcripts skipped because they could not be read at all.
    pub session_transcripts_unreadable: u32,
    /// Edits skipped because their transcript line had no parseable timestamp.
    pub session_edges_skipped_no_timestamp: u32,
    /// Already-indexed edges that acquired their capturing commit on this pass.
    pub session_edges_late_bound: u32,
    /// Edges already present and skipped by the additive merge. Reported so a
    /// re-index's `session_edges: 0` reads as "nothing NEW" rather than "nothing
    /// there" — the same ambiguity `corpus_empty` removes on the query side.
    pub session_edges_already_indexed: u32,
    /// Why the session pass failed, when it did. The other five corpora are
    /// unaffected — this is reported, not propagated.
    pub session_index_error: Option<String>,
    pub extract_session_history_ms: u128,
    pub extract_memory_ms: u128,
    pub extract_design_docs_ms: u128,
    pub extract_commits_ms: u128,
    pub extract_source_ms: u128,
    pub extract_session_edges_ms: u128,
    pub elapsed_ms: u128,
}

impl ScanStats {
    pub fn total(&self) -> u32 {
        self.session_history + self.memory + self.design_doc + self.source_files
    }
}

#[derive(Debug, Clone, Copy)]
pub struct GitOptions {
    pub max_depth: usize,
    pub nested_mode: NestedMode,
    pub max_commits_per_repo: usize,
}

impl Default for GitOptions {
    fn default() -> Self {
        // DESIGN §14.4: depth=3 / skip-nested / max=50,000 per repo.
        Self {
            max_depth: 3,
            nested_mode: NestedMode::Skip,
            max_commits_per_repo: 50_000,
        }
    }
}

/// Project anchors for the per-project corpora (session_history, design_doc).
///
/// Returns `{workspace_root} ∪ git_commits::discover_repos(workspace, ...)`,
/// canonicalized, sorted, and deduped. Each anchor is a candidate root under
/// which `CLAUDE.md` and `docs/` may live.
///
/// Resolves G1 from `dogfood/FRESH_INSTALL_NOTES.md`: before this helper, the
/// indexer only checked `$WORKSPACE/CLAUDE.md` and `$WORKSPACE/docs/design/`,
/// which silently missed every nested project in a workspace-of-projects
/// layout. Sharing this helper between `full_scan` and `resolve_subscriptions`
/// keeps the watcher's subscription set in lockstep with the cold-scan
/// coverage.
pub fn discover_project_anchors(workspace: &Path, git_opts: GitOptions) -> Vec<PathBuf> {
    let mut anchors: Vec<PathBuf> = Vec::new();
    if let Ok(root) = workspace.canonicalize() {
        anchors.push(root);
    } else {
        anchors.push(workspace.to_path_buf());
    }
    for repo in git_commits::discover_repos(workspace, git_opts.max_depth, git_opts.nested_mode) {
        anchors.push(repo.canonicalize().unwrap_or(repo));
    }
    anchors.sort();
    anchors.dedup();
    anchors
}

/// Derive the Claude memory dir for a given workspace.
///
/// Convention: collapse `/`, `_`, and `.` in the absolute workspace path to `-`.
/// E.g. `/Users/foo/projects` → `~/.claude/projects/-Users-foo-projects/memory/`.
/// `~/.claude/projects/<encoded-workspace-path>` — Claude Code's per-workspace
/// ("slug") directory: this workspace's transcripts AND its `memory/`. Claude
/// Code owns the layout; the encoding mirrors its Unix scheme.
///
/// The SLUG dir, not `~/.claude` as a whole, is the workspace-segregated unit —
/// `~/.claude` also holds OTHER workspaces' slugs, a flat cross-workspace
/// `history.jsonl`, and an unpartitioned `file-history/`. Anything reasoning
/// about "this workspace's Claude state" must anchor here.
pub fn claude_slug_dir(workspace: &Path) -> Option<PathBuf> {
    // Home dir: `$HOME` on Unix, `%USERPROFILE%` on Windows.
    // NOTE (Windows port): the `/ _ .` → `-` encoding below was derived from
    // Claude Code's Unix project-dir scheme; Windows canonical paths add a
    // drive letter and `\\?\` prefix that this encoding does not yet collapse,
    // so the resulting memory dir may be wrong on Windows. Verify against a
    // real Claude Code install on Windows before trusting memory indexing there.
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })?;
    let canonical = workspace.canonicalize().ok()?;
    let encoded: String = canonical
        .to_string_lossy()
        .chars()
        .map(|c| if matches!(c, '/' | '_' | '.') { '-' } else { c })
        .collect();
    Some(PathBuf::from(home).join(".claude/projects").join(encoded))
}

/// `<claude_slug_dir>/memory` — the memory dir for this workspace.
pub fn default_memory_dir(workspace: &Path) -> Option<PathBuf> {
    Some(claude_slug_dir(workspace)?.join("memory"))
}

/// Which subset of the workspace an index pass writes: a single IP domain
/// (docs/SESSION_SCOPE_DESIGN.md §0), or the whole workspace when `None`. `Copy`
/// — it is two references, passed by value into the discovery filters.
#[derive(Clone, Copy)]
struct DomainScope<'a> {
    config: &'a DomainConfig,
    domain: &'a str,
}

impl DomainScope<'_> {
    /// Does `path` (a repo/anchor path) belong to this domain? Canonicalizes
    /// `path` so it lines up with the already-canonicalized `workspace`.
    fn keeps(&self, workspace: &Path, path: &Path) -> bool {
        let canon = path.canonicalize();
        let path = canon.as_deref().unwrap_or(path);
        self.config.includes(self.domain, workspace, path)
    }

    /// Does this domain claim the workspace's memory dir (`[memory]`)?
    fn claims_memory(&self) -> bool {
        self.config.claims_memory(self.domain)
    }
}

pub async fn full_scan(
    pool: &SqlitePool,
    workspace: &Path,
    memory_dir: Option<&Path>,
    projects_dir: Option<&Path>,
    git_opts: GitOptions,
    domain: Option<&str>,
) -> Result<ScanStats> {
    let op = Op::start("indexer.full_scan");
    let started = Instant::now();
    let mut stats = ScanStats::default();

    let workspace = workspace
        .canonicalize()
        .with_context(|| format!("canonicalize workspace {workspace:?}"))?;

    // Domain scope (docs/SESSION_SCOPE_DESIGN.md §0): with `--domain`, this pass
    // writes ONLY that domain's labeled subdirs (per `.nibdex-domains.toml`) into
    // the db, so a domain's db never holds another domain's content. `None` =
    // index the whole workspace (unpartitioned; today's behavior).
    let domain_config = DomainConfig::load(&workspace)?;
    let scope: Option<DomainScope> = match domain {
        Some(d) => {
            let config = domain_config.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "--domain {d} requested but no .nibdex-domains.toml at {}",
                    workspace.display()
                )
            })?;
            if !config.has_domain(d) {
                anyhow::bail!(
                    "--domain {d} is not declared in .nibdex-domains.toml (declared: {:?})",
                    config.domain_names()
                );
            }
            Some(DomainScope { config, domain: d })
        }
        None => None,
    };
    let keeps = |p: &Path| scope.is_none_or(|s| s.keeps(&workspace, p));

    // Project anchors drive session_history + design_doc discovery. Each anchor
    // = the workspace root or a discovered git repo root (see
    // `discover_project_anchors`). Per-anchor scope means a nested CLAUDE.md
    // resolves its `files_touched` references against its own project root,
    // not the parent workspace. In domain mode the anchors are filtered to the
    // domain's subdirs (the workspace root belongs to no domain, so it drops).
    let anchors: Vec<PathBuf> = discover_project_anchors(&workspace, git_opts)
        .into_iter()
        .filter(|a| keeps(a))
        .collect();

    // 1. session_history — one `$ANCHOR/CLAUDE.md` per project anchor.
    for anchor in &anchors {
        let claude_md = anchor.join("CLAUDE.md");
        // `exists()` follows symlinks; a CLAUDE.md linked from another tree is
        // that tree's content (RC1 review 1.2 — see `stays_within`).
        if claude_md.exists() && stays_within(anchor, &claude_md) {
            let document_id = upsert_document(pool, &claude_md, "session_history").await?;
            stats.session_history += 1;
            let session_entries =
                run_session_history_extractor(pool, document_id, &claude_md, anchor).await?;
            stats.session_entries += session_entries.rows;
            stats.extract_session_history_ms += session_entries.duration_ms;
        }
    }

    // 2. memory — all *.md under $CLAUDE_PROJECT_MEMORY_DIR. Single dir,
    // keyed off the workspace root (not per-anchor) by Claude Code convention.
    let memory = memory_dir
        .map(Path::to_path_buf)
        .or_else(|| default_memory_dir(&workspace));
    // Memory is workspace-global (one dir, not per-subdir), so it is not
    // filesystem-attributable the way a subdir is — and Claude Code owns its
    // path, so it cannot be relocated into a labeled subdir the way workspace-
    // root docs can (tracker 2026-07-18). In domain mode it is therefore skipped
    // UNLESS the domain explicitly claims it via the `[memory]` table, which is
    // an unverifiable user assertion (see `DomainConfig::claims_memory`).
    // Fail-narrow: no claim → skipped, byte-for-byte the prior behavior.
    let memory_claimed = scope.is_none_or(|s| s.claims_memory());
    if memory_claimed
        && let Some(mem) = memory.as_ref()
        && mem.exists()
    {
        let mem_result = run_memory_extractor(pool, mem).await?;
        stats.memory = mem_result.files_seen;
        stats.memory_entries = mem_result.rows;
        stats.memory_skipped_no_frontmatter = mem_result.skipped;
        stats.extract_memory_ms = mem_result.duration_ms;
    }

    // 3. design_doc — root-level `$ANCHOR/*.md` (minus CLAUDE.md) ∪ all `*.md`
    // under `$ANCHOR/docs/**` (recursive). Two broadenings:
    //  - `docs/design/ → docs/` (2026-06-05): most repos keep design docs directly
    //    under `docs/`, not a `docs/design/` subdir (D1_SCOPE §10 gear-6 finding 3).
    //  - root-level `*.md` (2026-07-09): the TODO/bug source-of-truth (BUG_TRIAGE.md
    //    et al.) lives ABOVE `docs/` and can exceed the source-index size cap, so it
    //    reached no corpus — see `list_root_markdown`. The extractor always attempts
    //    root files, so no `docs.exists()` guard here.
    for anchor in &anchors {
        let design_result = run_design_doc_extractor(pool, anchor).await?;
        stats.design_doc += design_result.files_seen;
        stats.design_sections += design_result.rows;
        stats.extract_design_docs_ms += design_result.duration_ms;
    }

    // 4. git commits — HEAD ancestry walk across every discovered repo.
    let commit_stats = run_git_commits_extractor(pool, &workspace, git_opts, scope).await?;
    stats.repos_indexed = commit_stats.repos_indexed;
    stats.repos_capped = commit_stats.repos_capped;
    stats.repos_shallow = commit_stats.repos_shallow;
    stats.commits_inserted = commit_stats.commits_inserted;
    stats.extract_commits_ms = commit_stats.duration_ms;

    // 5. source code (D1a, D1_SCOPE §5) — git-tracked files → line-window chunks,
    // each stamped with its provenance commit. Runs LAST on purpose: commits (step
    // 4) must exist for provenance to resolve, and design/session docs (steps 1, 3)
    // must be indexed first so the one-corpus-per-file skip leaves their files to
    // them (source_index::is_owned_by_other_corpus). One repo at a time.
    let source_op = Op::start("extract.source");
    let mut source_bytes_hint: i64 = 0;
    for repo in git_commits::discover_repos(&workspace, git_opts.max_depth, git_opts.nested_mode) {
        if !keeps(&repo) {
            continue;
        }
        let src = crate::source_index::index_source_repo(pool, &repo).await?;
        stats.source_files += src.files_indexed as u32;
        stats.source_chunks += src.chunks as u32;
        stats.source_skipped_other_corpus += src.skipped_other_corpus as u32;
        stats.source_files_pruned += src.files_pruned as u32;
        stats.extract_source_ms += src.elapsed_ms;
        source_bytes_hint += src.files_tracked as i64;
    }
    // Recorded like the other four extractors so `check().extractors_last_run_ms`
    // carries `extract.source` too (it listed only four of six corpora before).
    source_op
        .complete(
            pool,
            Some(source_bytes_hint),
            Some(stats.source_chunks as i64),
            json!({
                "files": stats.source_files,
                "chunks": stats.source_chunks,
                "pruned": stats.source_files_pruned,
                "skipped_other_corpus": stats.source_skipped_other_corpus,
            }),
        )
        .await?;

    // 6. session transcripts (the `find_session` corpus). Runs LAST because the
    // session→commit binding resolves against `commit_entries` from step 4.
    //
    // This is folded in rather than left to `index-sessions` because a corpus a
    // user has to know to populate is a corpus that stays empty: the README
    // quickstart's `find_session` returned nothing for anyone who had only run
    // `nibdex index`. Scope is DERIVED from the workspace root, never a flag —
    // see `SessionScope::Workspace`.
    let session_started = Instant::now();
    let session_op = Op::start("extract.session_edges");
    let projects = projects_dir
        .map(PathBuf::from)
        .or_else(session_index::default_projects_dir);
    match projects {
        Some(projects_dir) if projects_dir.is_dir() => {
            // NON-FATAL BY DESIGN, and the second half of a defence the file-level
            // skip already covers. Five corpora have committed by the time this
            // runs; failing the whole scan over the newest and most fragile one
            // would report total failure for a mostly-successful index and leave
            // the caller with a half-populated db and no summary explaining it.
            //
            // Reported, never swallowed: the reason lands in `stats` and prints in
            // the CLI summary. Diagnosed is the floor — the same bar the
            // `corpus_empty` work sets on the query side.
            match session_index::index_sessions(
                pool,
                &projects_dir,
                session_index::SessionScope::Workspace,
                false, // additive merge — a re-index must never drop indexed edges
                &workspace,
                domain,
            )
            .await
            {
                Ok(sess) => {
                    stats.session_edges = sess.edges_indexed as u32;
                    stats.session_transcripts = sess.transcripts_seen as u32;
                    stats.session_edges_dropped_foreign_workspace =
                        sess.edges_dropped_foreign_workspace as u32;
                    stats.session_edges_dropped_foreign_target =
                        sess.edges_dropped_foreign_target as u32;
                    stats.session_transcripts_unreadable = sess.transcripts_unreadable as u32;
                    stats.session_edges_skipped_no_timestamp = sess.edges_skipped_no_timestamp as u32;
                    stats.session_edges_already_indexed = sess.edges_duplicate as u32;
                    stats.session_edges_late_bound = sess.edges_late_bound as u32;
                }
                Err(e) => {
                    stats.session_index_error = Some(format!("{e:#}"));
                }
            }
        }
        // No transcript root (no $HOME, or Claude Code was never run here). Not an
        // error — the other five corpora are unaffected, and `check()` reports the
        // empty session corpus.
        _ => {}
    }
    stats.extract_session_edges_ms = session_started.elapsed().as_millis();
    match &stats.session_index_error {
        Some(e) => {
            session_op.complete_err(pool, e).await?;
        }
        None => {
            session_op
                .complete(
                    pool,
                    Some(stats.session_transcripts as i64),
                    Some(stats.session_edges as i64),
                    json!({
                        "transcripts": stats.session_transcripts,
                        "new_edges": stats.session_edges,
                        "already_indexed": stats.session_edges_already_indexed,
                    }),
                )
                .await?;
        }
    }

    stats.elapsed_ms = started.elapsed().as_millis();

    let extra = json!({
        "documents": {
            "session_history": stats.session_history,
            "memory": stats.memory,
            "design_doc": stats.design_doc,
            "source": stats.source_files,
        },
        "extractors": {
            "session_history": {
                "rows": stats.session_entries,
                "duration_ms": stats.extract_session_history_ms,
            },
            "memory": {
                "rows": stats.memory_entries,
                "skipped_no_frontmatter": stats.memory_skipped_no_frontmatter,
                "duration_ms": stats.extract_memory_ms,
            },
            "design_docs": {
                "rows": stats.design_sections,
                "duration_ms": stats.extract_design_docs_ms,
            },
            "commits": {
                "repos_indexed": stats.repos_indexed,
                "repos_capped": stats.repos_capped,
                "repos_shallow": stats.repos_shallow,
                "rows": stats.commits_inserted,
                "duration_ms": stats.extract_commits_ms,
            },
            "source": {
                "files": stats.source_files,
                "chunks": stats.source_chunks,
                "skipped_other_corpus": stats.source_skipped_other_corpus,
                "pruned": stats.source_files_pruned,
                "duration_ms": stats.extract_source_ms,
            },
            "session_edges": {
                "rows": stats.session_edges,
                "transcripts": stats.session_transcripts,
                "transcripts_unreadable": stats.session_transcripts_unreadable,
                "skipped_no_timestamp": stats.session_edges_skipped_no_timestamp,
                "already_indexed": stats.session_edges_already_indexed,
                "late_bound": stats.session_edges_late_bound,
                "dropped_foreign_workspace": stats.session_edges_dropped_foreign_workspace,
                "dropped_foreign_target": stats.session_edges_dropped_foreign_target,
                "error": stats.session_index_error,
                "duration_ms": stats.extract_session_edges_ms,
            },
        },
    });
    op.complete(pool, None, Some(stats.total() as i64), extra)
        .await?;

    Ok(stats)
}

/// Is `path` (a file or dir directly under `anchor`) really INSIDE `anchor` once
/// symlinks are resolved? A symlink is admitted only when its canonical target
/// is under the canonical anchor. Everything the indexer reads beneath a kept
/// anchor must satisfy this: in domain mode the domain filter runs on anchor
/// ROOTS, so a symlink under a labeled dir pointing at another domain's tree
/// (`acme/docs -> ../beta/docs`, `acme/notes.md -> ../beta/plan.md`) used to
/// pull that tree's content into this domain's db — the isolation the README
/// calls "physically absent" was one `ln -s` away from false (RC1 review 1.2).
/// Non-symlinks pass without a canonicalize (they cannot escape the anchor).
pub(crate) fn stays_within(anchor: &Path, path: &Path) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if !meta.file_type().is_symlink() {
        return true;
    }
    let (Ok(anchor_c), Ok(target_c)) = (anchor.canonicalize(), path.canonicalize()) else {
        return false;
    };
    target_c.starts_with(&anchor_c)
}

/// Recursive `*.md` walk under `root` — which must itself be a real directory
/// under `anchor` (a symlinked `docs/` is skipped by `stays_within`, since
/// walkdir follows the ROOT link even with `follow_links(false)`). Symlinked
/// files/dirs beneath are not followed (walkdir default), so nothing under a
/// kept root can read outside it.
fn walk_markdown(anchor: &Path, root: &Path) -> Vec<PathBuf> {
    if !stays_within(anchor, root) {
        return Vec::new();
    }
    WalkDir::new(root)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("md"))
        .map(|e| e.into_path())
        .collect()
}

/// Root-level `*.md` files sitting *directly* under an anchor (depth-1, NON-recursive),
/// excluding `CLAUDE.md` (owned by the session_history corpus). These join the
/// design-doc corpus so root-level prose a repo keeps at its top level — a TODO/bug
/// tracker (`BUG_TRIAGE.md`, `TODO.md`, `ISSUES.md`), `README.md`, `CHANGELOG.md`,
/// `ARCHITECTURE.md` — is reachable via `find_design_doc`, not just `docs/**`.
///
/// Broadened 2026-07-09 to close the "TODO source-of-truth" recall blind spot: the
/// tracker lived *above* `docs/` (so the design extractor never saw it) AND — being
/// large — exceeded the source-index `MAX_FILE_BYTES` cap (so `find_code` skipped it
/// too), leaving it in no corpus at all. This is the dev-agnostic tier-1 default; a
/// future per-workspace config / AI-learned convention layer would OVERRIDE this
/// target set (see `docs/IDEAS_SCRATCH.md` → "Learned corpus conventions").
fn list_root_markdown(anchor: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = match std::fs::read_dir(anchor) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_file())
            // `is_file()` follows symlinks; admit a link only if it resolves
            // inside the anchor (RC1 review 1.2 — see `stays_within`).
            .filter(|p| stays_within(anchor, p))
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("md"))
            .filter(|p| p.file_name().and_then(|s| s.to_str()) != Some("CLAUDE.md"))
            .collect(),
        Err(_) => Vec::new(),
    };
    out.sort();
    out
}

async fn upsert_document(pool: &SqlitePool, path: &Path, kind: &str) -> Result<i64> {
    let metadata = std::fs::metadata(path).with_context(|| format!("stat {path:?}"))?;
    let mtime = metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let hash = sha256_file(path).await?;
    let indexed_at = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
    let path_str = path.to_string_lossy().into_owned();

    let row: (i64,) = sqlx::query_as(
        r#"
        INSERT INTO documents (path, kind, content_hash, mtime, indexed_at)
        VALUES (?, ?, ?, ?, ?)
        ON CONFLICT(path) DO UPDATE SET
            content_hash = excluded.content_hash,
            mtime        = excluded.mtime,
            indexed_at   = excluded.indexed_at
        RETURNING id
        "#,
    )
    .bind(path_str)
    .bind(kind)
    .bind(hash)
    .bind(mtime)
    .bind(indexed_at)
    .fetch_one(pool)
    .await?;

    Ok(row.0)
}

struct ExtractorResult {
    rows: u32,
    duration_ms: u128,
}

async fn run_session_history_extractor(
    pool: &SqlitePool,
    document_id: i64,
    claude_md: &Path,
    workspace: &Path,
) -> Result<ExtractorResult> {
    let op = Op::start("extract.session_history");
    let started = Instant::now();
    let content = tokio::fs::read_to_string(claude_md)
        .await
        .with_context(|| format!("read {claude_md:?}"))?;
    let entries = session_history::extract_session_history(&content, workspace);
    let rows = entries.len() as u32;

    for entry in &entries {
        persist_session_entry(pool, document_id, entry).await?;
    }

    // G2 fix #1: silent 0-entry parses turn into a one-line, user-facing hint
    // explaining *why* (no heading vs heading-but-no-bullets vs different
    // convention). Static project READMEs that legitimately have no session
    // log get the friendlier "looks like a static README" framing.
    if entries.is_empty() {
        eprintln!(
            "[nibdex index] {}: {}",
            claude_md.display(),
            session_history::diagnose_empty_extraction(&content),
        );
    }

    // D-6.2.1: UPSERT-then-sweep. After the UPSERT loop has refreshed live
    // session entries, sweep rows that survived from a prior extraction whose
    // session_number no longer appears in the current CLAUDE.md. Empty extraction
    // is treated as a parse failure (skip the sweep — orphans surfacing in
    // `check()` is the correct signal, not a silent wipe).
    let swept = if entries.is_empty() {
        SweepStats::default()
    } else {
        sweep_session_entries(pool, document_id, &entries).await?
    };

    let duration_ms = started.elapsed().as_millis();
    let extra = json!({
        "claude_md_bytes": content.len(),
        "entries_extracted": rows,
        "swept_session_entries": swept.session_rows,
        "swept_search_index_rows": swept.search_rows,
    });
    op.complete(pool, Some(content.len() as i64), Some(rows as i64), extra)
        .await?;
    Ok(ExtractorResult { rows, duration_ms })
}

#[derive(Default)]
struct SweepStats {
    session_rows: u64,
    search_rows: u64,
}

async fn sweep_session_entries(
    pool: &SqlitePool,
    document_id: i64,
    entries: &[session_history::SessionEntry],
) -> Result<SweepStats> {
    use sqlx::QueryBuilder;

    // Sweep session_entries first; the search_index sweep then runs against the
    // live id set so stale rowid_refs disappear in one pass.
    let mut qb: QueryBuilder<'_, sqlx::Sqlite> =
        QueryBuilder::new("DELETE FROM session_entries WHERE document_id = ");
    qb.push_bind(document_id);
    qb.push(" AND session_number NOT IN (");
    let mut sep = qb.separated(", ");
    for entry in entries {
        sep.push_bind(entry.session_number);
    }
    qb.push(")");
    let result = qb.build().execute(pool).await?;
    let session_rows = result.rows_affected();

    let search_result = sqlx::query(
        "DELETE FROM search_index \
         WHERE source_table = 'session_entries' \
           AND rowid_ref NOT IN ( \
               SELECT id FROM session_entries WHERE document_id = ? \
           )",
    )
    .bind(document_id)
    .execute(pool)
    .await?;
    let search_rows = search_result.rows_affected();

    Ok(SweepStats {
        session_rows,
        search_rows,
    })
}

struct MemoryExtractorResult {
    files_seen: u32,
    rows: u32,
    skipped: u32,
    duration_ms: u128,
}

async fn run_memory_extractor(pool: &SqlitePool, mem_dir: &Path) -> Result<MemoryExtractorResult> {
    let op = Op::start("extract.memory");
    let started = Instant::now();

    let mut files_seen: u32 = 0;
    let mut rows: u32 = 0;
    let mut skipped: u32 = 0;
    let mut total_bytes: i64 = 0;

    // Anchor = the dir itself: a memory dir that is a symlink (dotfiles setups)
    // is fine, links BENEATH it are not followed.
    for path in walk_markdown(mem_dir, mem_dir) {
        let document_id = upsert_document(pool, &path, "memory").await?;
        files_seen += 1;
        let outcome = extract_memory_file_into_db(pool, document_id, &path).await?;
        total_bytes += outcome.bytes;
        if outcome.persisted {
            rows += 1;
        } else {
            skipped += 1;
        }
    }

    let duration_ms = started.elapsed().as_millis();
    let extra = json!({
        "files_seen": files_seen,
        "skipped_no_frontmatter": skipped,
        "rows_persisted": rows,
    });
    op.complete(pool, Some(total_bytes), Some(rows as i64), extra)
        .await?;
    Ok(MemoryExtractorResult {
        files_seen,
        rows,
        skipped,
        duration_ms,
    })
}

struct DesignDocExtractorResult {
    files_seen: u32,
    rows: u32,
    duration_ms: u128,
}

async fn run_design_doc_extractor(
    pool: &SqlitePool,
    anchor: &Path,
) -> Result<DesignDocExtractorResult> {
    let op = Op::start("extract.design_docs");
    let started = Instant::now();

    let mut files_seen: u32 = 0;
    let mut rows: u32 = 0;
    let mut total_bytes: i64 = 0;

    // Design-doc targets = root-level `*.md` (BUG_TRIAGE.md et al., minus CLAUDE.md)
    // ∪ every `*.md` under `docs/**`. Root and `docs/` are disjoint by construction;
    // sort+dedup guards against an odd symlink surfacing a path twice. (Tier-1
    // default target set; a config / learned-convention layer would override it.)
    let mut paths = list_root_markdown(anchor);
    let docs = anchor.join("docs");
    if docs.exists() {
        paths.extend(walk_markdown(anchor, &docs));
    }
    paths.sort();
    paths.dedup();

    for path in paths {
        let document_id = upsert_document(pool, &path, "design_doc").await?;
        files_seen += 1;
        let outcome = extract_design_file_into_db(pool, document_id, &path).await?;
        total_bytes += outcome.bytes;
        rows += outcome.sections;
    }

    let duration_ms = started.elapsed().as_millis();
    let extra = json!({
        "files_seen": files_seen,
        "sections_persisted": rows,
    });
    op.complete(pool, Some(total_bytes), Some(rows as i64), extra)
        .await?;
    Ok(DesignDocExtractorResult {
        files_seen,
        rows,
        duration_ms,
    })
}

// =====================================================================================
// Per-file re-extraction helpers (D-6.2.2 / D-6.2.3 file-watcher kill-path).
// =====================================================================================

struct MemoryFileOutcome {
    bytes: i64,
    persisted: bool,
}

async fn extract_memory_file_into_db(
    pool: &SqlitePool,
    document_id: i64,
    path: &Path,
) -> Result<MemoryFileOutcome> {
    let content = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("read {path:?}"))?;
    let bytes = content.len() as i64;
    match memory::extract_memory_entry(&content) {
        Some(entry) => {
            persist_memory_entry(pool, document_id, &entry).await?;
            Ok(MemoryFileOutcome {
                bytes,
                persisted: true,
            })
        }
        None => Ok(MemoryFileOutcome {
            bytes,
            persisted: false,
        }),
    }
}

struct DesignFileOutcome {
    bytes: i64,
    sections: u32,
}

async fn extract_design_file_into_db(
    pool: &SqlitePool,
    document_id: i64,
    path: &Path,
) -> Result<DesignFileOutcome> {
    let content = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("read {path:?}"))?;
    let bytes = content.len() as i64;
    let sections = design_doc::extract_sections(&content);

    // D-6.2.3: wipe-and-rebuild per document — correct under heading renames.
    // FTS5 lacks JOIN, so the IN-subquery resolves rowid_refs against
    // design_doc_sections BEFORE we delete those rows.
    sqlx::query(
        "DELETE FROM search_index \
         WHERE source_table = 'design_doc_sections' \
           AND rowid_ref IN (SELECT id FROM design_doc_sections WHERE document_id = ?)",
    )
    .bind(document_id)
    .execute(pool)
    .await?;
    sqlx::query("DELETE FROM design_doc_sections WHERE document_id = ?")
        .bind(document_id)
        .execute(pool)
        .await?;

    let mut count: u32 = 0;
    for section in &sections {
        persist_design_doc_section(pool, document_id, section).await?;
        count += 1;
    }
    Ok(DesignFileOutcome {
        bytes,
        sections: count,
    })
}

/// Re-extract a CLAUDE.md edit observed by the file-watcher. Wraps the existing
/// UPSERT-then-sweep path (D-6.2.1) with a dedicated Op row so per-event work
/// shows up in `op_measurements` distinct from the full-scan baseline.
pub async fn reindex_claude_md(
    pool: &SqlitePool,
    claude_md: &Path,
    workspace: &Path,
) -> Result<()> {
    let op = Op::start("watcher.reindex_claude_md");
    let document_id = upsert_document(pool, claude_md, "session_history").await?;
    let inner = run_session_history_extractor(pool, document_id, claude_md, workspace).await?;
    op.complete(
        pool,
        None,
        Some(inner.rows as i64),
        json!({ "duration_ms": inner.duration_ms }),
    )
    .await?;
    Ok(())
}

/// Re-extract a single memory file (create or edit) per D-6.2.2.
pub async fn reindex_memory_file(pool: &SqlitePool, path: &Path) -> Result<()> {
    let op = Op::start("watcher.reindex_memory_file");
    let document_id = upsert_document(pool, path, "memory").await?;
    let outcome = extract_memory_file_into_db(pool, document_id, path).await?;
    op.complete(
        pool,
        Some(outcome.bytes),
        Some(if outcome.persisted { 1 } else { 0 }),
        json!({ "persisted": outcome.persisted }),
    )
    .await?;
    Ok(())
}

/// Re-extract a single design-doc file (create or edit) per D-6.2.3.
pub async fn reindex_design_file(pool: &SqlitePool, path: &Path) -> Result<()> {
    let op = Op::start("watcher.reindex_design_file");
    let document_id = upsert_document(pool, path, "design_doc").await?;
    let outcome = extract_design_file_into_db(pool, document_id, path).await?;
    op.complete(
        pool,
        Some(outcome.bytes),
        Some(outcome.sections as i64),
        json!({ "sections": outcome.sections }),
    )
    .await?;
    Ok(())
}

/// Drop the document row for `path` if present. FK cascades clean up
/// `memory_entries` / `design_doc_sections` + their `search_index` siblings
/// via the post-delete sweep below. Returns true if a row was actually deleted.
pub async fn delete_document_by_path(pool: &SqlitePool, path: &Path) -> Result<bool> {
    let op = Op::start("watcher.delete_document");
    let path_str = path.to_string_lossy().into_owned();

    let row: Option<(i64, String)> =
        sqlx::query_as("SELECT id, kind FROM documents WHERE path = ?")
            .bind(&path_str)
            .fetch_optional(pool)
            .await?;

    let Some((document_id, kind)) = row else {
        op.complete(pool, None, Some(0), json!({ "matched": false }))
            .await?;
        return Ok(false);
    };

    // Capture child rowid sets BEFORE the cascade fires so we can prune
    // search_index entries that lost their backing row.
    let session_ids: Vec<i64> =
        sqlx::query_scalar("SELECT id FROM session_entries WHERE document_id = ?")
            .bind(document_id)
            .fetch_all(pool)
            .await?;
    let memory_ids: Vec<i64> =
        sqlx::query_scalar("SELECT id FROM memory_entries WHERE document_id = ?")
            .bind(document_id)
            .fetch_all(pool)
            .await?;
    let design_ids: Vec<i64> =
        sqlx::query_scalar("SELECT id FROM design_doc_sections WHERE document_id = ?")
            .bind(document_id)
            .fetch_all(pool)
            .await?;

    sqlx::query("DELETE FROM documents WHERE id = ?")
        .bind(document_id)
        .execute(pool)
        .await?;

    // FK cascade removed the rows above; FTS5 is not FK-aware so prune its
    // matching rowid_refs explicitly per source_table.
    sweep_search_index_by_ids(pool, "session_entries", &session_ids).await?;
    sweep_search_index_by_ids(pool, "memory_entries", &memory_ids).await?;
    sweep_search_index_by_ids(pool, "design_doc_sections", &design_ids).await?;

    op.complete(
        pool,
        None,
        Some(1),
        json!({
            "matched": true,
            "kind": kind,
            "session_rows_cleared": session_ids.len() as i64,
            "memory_rows_cleared": memory_ids.len() as i64,
            "design_rows_cleared": design_ids.len() as i64,
        }),
    )
    .await?;
    Ok(true)
}

async fn sweep_search_index_by_ids(
    pool: &SqlitePool,
    source_table: &str,
    ids: &[i64],
) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let mut qb: sqlx::QueryBuilder<'_, sqlx::Sqlite> =
        sqlx::QueryBuilder::new("DELETE FROM search_index WHERE source_table = ");
    qb.push_bind(source_table);
    qb.push(" AND rowid_ref IN (");
    let mut sep = qb.separated(", ");
    for id in ids {
        sep.push_bind(*id);
    }
    qb.push(")");
    qb.build().execute(pool).await?;
    Ok(())
}

async fn persist_memory_entry(
    pool: &SqlitePool,
    document_id: i64,
    entry: &memory::MemoryEntry,
) -> Result<()> {
    let row: (i64,) = sqlx::query_as(
        r#"
        INSERT INTO memory_entries
            (document_id, name, memory_type, description, body)
        VALUES (?, ?, ?, ?, ?)
        ON CONFLICT(name) DO UPDATE SET
            document_id  = excluded.document_id,
            memory_type  = excluded.memory_type,
            description  = excluded.description,
            body         = excluded.body
        RETURNING id
        "#,
    )
    .bind(document_id)
    .bind(&entry.name)
    .bind(&entry.memory_type)
    .bind(entry.description.as_deref())
    .bind(&entry.body)
    .fetch_one(pool)
    .await?;
    let memory_id = row.0;

    sqlx::query("DELETE FROM search_index WHERE rowid_ref = ? AND source_table = 'memory_entries'")
        .bind(memory_id)
        .execute(pool)
        .await?;

    // Index the description + body together so name-adjacent prose is searchable.
    let fts_body = match entry.description.as_deref() {
        Some(d) if !d.is_empty() => format!("{}\n\n{}", d, entry.body),
        _ => entry.body.clone(),
    };
    sqlx::query(
        "INSERT INTO search_index (body, kind, rowid_ref, source_table) \
         VALUES (?, 'memory', ?, 'memory_entries')",
    )
    .bind(fts_body)
    .bind(memory_id)
    .execute(pool)
    .await?;

    Ok(())
}

async fn persist_design_doc_section(
    pool: &SqlitePool,
    document_id: i64,
    section: &design_doc::DocSection,
) -> Result<()> {
    let row: (i64,) = sqlx::query_as(
        r#"
        INSERT INTO design_doc_sections
            (document_id, heading_path, line_start, line_end, body)
        VALUES (?, ?, ?, ?, ?)
        RETURNING id
        "#,
    )
    .bind(document_id)
    .bind(&section.heading_path)
    .bind(section.line_start as i64)
    .bind(section.line_end as i64)
    .bind(&section.body)
    .fetch_one(pool)
    .await?;
    let section_id = row.0;

    sqlx::query(
        "INSERT INTO search_index (body, kind, rowid_ref, source_table) \
         VALUES (?, 'design_doc', ?, 'design_doc_sections')",
    )
    .bind(&section.body)
    .bind(section_id)
    .execute(pool)
    .await?;

    Ok(())
}

struct GitExtractorResult {
    repos_indexed: u32,
    repos_capped: u32,
    repos_shallow: u32,
    commits_inserted: u32,
    duration_ms: u128,
}

async fn run_git_commits_extractor(
    pool: &SqlitePool,
    workspace: &Path,
    opts: GitOptions,
    scope: Option<DomainScope<'_>>,
) -> Result<GitExtractorResult> {
    let op = Op::start("extract.commits");
    let started = Instant::now();

    let repos: Vec<PathBuf> = git_commits::discover_repos(workspace, opts.max_depth, opts.nested_mode)
        .into_iter()
        .filter(|r| scope.is_none_or(|s| s.keeps(workspace, r)))
        .collect();
    let mut commits_inserted: u32 = 0;
    let mut repos_capped: u32 = 0;
    let mut repos_shallow: u32 = 0;

    for repo_path in &repos {
        let outcome = index_one_repo(pool, repo_path, opts.max_commits_per_repo).await?;
        commits_inserted += outcome.commits_inserted;
        if outcome.capped {
            repos_capped += 1;
        }
        if outcome.is_shallow {
            repos_shallow += 1;
        }
    }

    let duration_ms = started.elapsed().as_millis();
    let extra = json!({
        "repos_discovered": repos.len(),
        "repos_capped": repos_capped,
        "repos_shallow": repos_shallow,
        "commits_inserted": commits_inserted,
    });
    op.complete(pool, None, Some(commits_inserted as i64), extra)
        .await?;

    Ok(GitExtractorResult {
        repos_indexed: repos.len() as u32,
        repos_capped,
        repos_shallow,
        commits_inserted,
        duration_ms,
    })
}

/// Per-repo extract + persist + cursor update. Shared between the full-scan
/// path (`run_git_commits_extractor`) and the file-watcher path
/// (`reindex_commits_for_repo`). Honors the existing Day 4 force-push fallback
/// (the `walk.hide(oid)` in `git_commits::extract_commits` tolerates an unknown
/// last cursor and falls through to a full walk).
struct RepoIndexOutcome {
    commits_inserted: u32,
    is_shallow: bool,
    capped: bool,
}

async fn index_one_repo(
    pool: &SqlitePool,
    repo_path: &Path,
    max_commits_per_repo: usize,
) -> Result<RepoIndexOutcome> {
    let repo_path_str = repo_path.to_string_lossy().into_owned();
    let last_oid: Option<String> =
        sqlx::query_scalar("SELECT last_indexed_oid FROM indexed_repos WHERE repo_path = ?")
            .bind(&repo_path_str)
            .fetch_optional(pool)
            .await?;

    let result =
        git_commits::extract_commits(repo_path, last_oid.as_deref(), max_commits_per_repo)?;

    let mut commits_inserted: u32 = 0;
    for commit in &result.commits {
        persist_commit(pool, &repo_path_str, commit).await?;
        commits_inserted += 1;
    }

    // Cursor update: prefer current HEAD; fall back to most-recent extracted commit.
    let new_cursor = result
        .head_oid
        .clone()
        .or_else(|| result.commits.first().map(|c| c.commit_hash.clone()));
    if let Some(oid) = new_cursor {
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM commit_entries WHERE repo_path = ?")
                .bind(&repo_path_str)
                .fetch_one(pool)
                .await?;
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
        sqlx::query(
            r#"
            INSERT INTO indexed_repos
                (repo_path, last_indexed_oid, is_shallow, commit_count, last_indexed_at)
            VALUES (?, ?, ?, ?, ?)
            ON CONFLICT(repo_path) DO UPDATE SET
                last_indexed_oid = excluded.last_indexed_oid,
                is_shallow       = excluded.is_shallow,
                commit_count     = excluded.commit_count,
                last_indexed_at  = excluded.last_indexed_at
            "#,
        )
        .bind(&repo_path_str)
        .bind(oid)
        .bind(if result.is_shallow { 1_i64 } else { 0 })
        .bind(count)
        .bind(now)
        .execute(pool)
        .await?;
    }

    Ok(RepoIndexOutcome {
        commits_inserted,
        is_shallow: result.is_shallow,
        capped: result.capped,
    })
}

/// File-watcher entry point: re-extract commits for a single repo observed
/// via a `.git/HEAD` or `.git/refs/heads/**` event (D-6.2.4). Wraps
/// `index_one_repo` in `Op::start("watcher.reindex_commits")` so per-event
/// work shows up in `op_measurements` distinct from the full-scan baseline.
/// `max_commits_per_repo` is the daemon's `--max-commits-per-repo` (defaults to
/// the same 50,000 as `nibdex index`) — previously hard-wired to the default
/// regardless of the flag (RC1 review, sev-2).
pub async fn reindex_commits_for_repo(
    pool: &SqlitePool,
    repo_path: &Path,
    max_commits_per_repo: usize,
) -> Result<()> {
    let op = Op::start("watcher.reindex_commits");
    let outcome = index_one_repo(pool, repo_path, max_commits_per_repo).await?;
    op.complete(
        pool,
        None,
        Some(outcome.commits_inserted as i64),
        json!({
            "repo_path": repo_path.to_string_lossy(),
            "commits_inserted": outcome.commits_inserted,
            "is_shallow": outcome.is_shallow,
            "capped": outcome.capped,
        }),
    )
    .await?;
    Ok(())
}

/// File-watcher entry point: re-index source code for a single repo after a commit
/// (a `.git/HEAD` or `.git/refs/heads/**` event). D1a "re-index-on-commit via
/// GitRefs" (D1_SCOPE §5) — a commit can change file content at HEAD AND the
/// provenance commit each chunk points at, so source is re-extracted alongside the
/// commits corpus. (Live working-tree freshness is deferred to D1b.) Runs AFTER
/// `reindex_commits_for_repo` so the new commits exist for provenance to resolve.
pub async fn reindex_source_for_repo(pool: &SqlitePool, repo_path: &Path) -> Result<()> {
    let op = Op::start("watcher.reindex_source");
    let src = crate::source_index::index_source_repo(pool, repo_path).await?;
    op.complete(
        pool,
        None,
        Some(src.files_indexed as i64),
        json!({
            "repo_path": repo_path.to_string_lossy(),
            "source_files": src.files_indexed,
            "source_chunks": src.chunks,
            "source_unchanged": src.files_unchanged,
            "skipped_other_corpus": src.skipped_other_corpus,
        }),
    )
    .await?;
    Ok(())
}

async fn persist_commit(
    pool: &SqlitePool,
    repo_path: &str,
    commit: &git_commits::CommitRow,
) -> Result<()> {
    let parents_json = serde_json::to_string(&commit.parent_hashes)?;
    let files_json = serde_json::to_string(&commit.files_changed)?;

    let row: (i64,) = sqlx::query_as(
        r#"
        INSERT INTO commit_entries
            (repo_path, commit_hash, parent_hashes,
             author_email, author_name, authored_at, committed_at,
             message_summary, message_body, files_changed, branch_refs)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL)
        ON CONFLICT(repo_path, commit_hash) DO UPDATE SET
            parent_hashes   = excluded.parent_hashes,
            author_email    = excluded.author_email,
            author_name     = excluded.author_name,
            authored_at     = excluded.authored_at,
            committed_at    = excluded.committed_at,
            message_summary = excluded.message_summary,
            message_body    = excluded.message_body,
            files_changed   = excluded.files_changed
        RETURNING id
        "#,
    )
    .bind(repo_path)
    .bind(&commit.commit_hash)
    .bind(parents_json)
    .bind(&commit.author_email)
    .bind(&commit.author_name)
    .bind(commit.authored_at)
    .bind(commit.committed_at)
    .bind(&commit.message_summary)
    .bind(&commit.message_body)
    .bind(files_json)
    .fetch_one(pool)
    .await?;
    let commit_entry_id = row.0;

    // FTS5 lacks UPSERT; wipe + insert for this rowid_ref + source_table pair.
    sqlx::query("DELETE FROM search_index WHERE rowid_ref = ? AND source_table = 'commit_entries'")
        .bind(commit_entry_id)
        .execute(pool)
        .await?;

    // Index the combined summary + body for retrieval; body may be empty for one-liners.
    let fts_body = match &commit.message_body {
        Some(b) => format!("{}\n\n{}", commit.message_summary, b),
        None => commit.message_summary.clone(),
    };
    sqlx::query(
        "INSERT INTO search_index (body, kind, rowid_ref, source_table) \
         VALUES (?, 'commit', ?, 'commit_entries')",
    )
    .bind(fts_body)
    .bind(commit_entry_id)
    .execute(pool)
    .await?;

    Ok(())
}

async fn persist_session_entry(
    pool: &SqlitePool,
    document_id: i64,
    entry: &session_history::SessionEntry,
) -> Result<()> {
    let files_json = serde_json::to_string(&entry.files_touched)?;
    let todos_json = serde_json::to_string(&entry.todos_mentioned)?;
    let decisions_json = serde_json::to_string(&entry.decisions_made)?;

    let row: (i64,) = sqlx::query_as(
        r#"
        INSERT INTO session_entries
            (document_id, session_number, entry_date, body,
             files_touched, todos_mentioned, decisions_made)
        VALUES (?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(session_number) DO UPDATE SET
            document_id     = excluded.document_id,
            entry_date      = excluded.entry_date,
            body            = excluded.body,
            files_touched   = excluded.files_touched,
            todos_mentioned = excluded.todos_mentioned,
            decisions_made  = excluded.decisions_made
        RETURNING id
        "#,
    )
    .bind(document_id)
    .bind(entry.session_number)
    .bind(&entry.entry_date)
    .bind(&entry.body)
    .bind(files_json)
    .bind(todos_json)
    .bind(decisions_json)
    .fetch_one(pool)
    .await?;
    let session_entry_id = row.0;

    // FTS5 has no native UPSERT — wipe + insert for this rowid_ref + source_table pair.
    sqlx::query(
        "DELETE FROM search_index WHERE rowid_ref = ? AND source_table = 'session_entries'",
    )
    .bind(session_entry_id)
    .execute(pool)
    .await?;

    sqlx::query(
        "INSERT INTO search_index (body, kind, rowid_ref, source_table) \
         VALUES (?, 'session_history', ?, 'session_entries')",
    )
    .bind(&entry.body)
    .bind(session_entry_id)
    .execute(pool)
    .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn fresh_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    /// D-6.2.1 end-to-end: extracting CLAUDE.md A then a trimmed B leaves the
    /// DB with B's entry set exactly — orphans from A's first pass disappear
    /// via the sweep. Closes the 146-vs-135 class Day 5 surfaced.
    #[tokio::test]
    async fn run_session_history_extractor_sweeps_dropped_entries() {
        let pool = fresh_pool().await;
        let tmp = tempfile::tempdir().unwrap();
        let claude_md = tmp.path().join("CLAUDE.md");

        // Round 1: 3 entries.
        std::fs::write(
            &claude_md,
            "# top\n\n## Recent session history\n\n\
             - **#100**: alpha.\n\
             - **#200**: beta.\n\
             - **#300**: gamma.\n",
        )
        .unwrap();
        let doc_id = upsert_document(&pool, &claude_md, "session_history")
            .await
            .unwrap();
        let r1 = run_session_history_extractor(&pool, doc_id, &claude_md, tmp.path())
            .await
            .unwrap();
        assert_eq!(r1.rows, 3);

        let (count_before,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM session_entries")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count_before, 3);

        // Round 2: drop #200.
        std::fs::write(
            &claude_md,
            "# top\n\n## Recent session history\n\n\
             - **#100**: alpha-updated.\n\
             - **#300**: gamma-updated.\n",
        )
        .unwrap();
        let doc_id = upsert_document(&pool, &claude_md, "session_history")
            .await
            .unwrap();
        let r2 = run_session_history_extractor(&pool, doc_id, &claude_md, tmp.path())
            .await
            .unwrap();
        assert_eq!(r2.rows, 2);

        let (count_after,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM session_entries")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count_after, 2, "sweep dropped #200");

        let still_there: Vec<(i64,)> =
            sqlx::query_as("SELECT session_number FROM session_entries ORDER BY session_number")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(
            still_there.iter().map(|(n,)| *n).collect::<Vec<_>>(),
            vec![100, 300]
        );

        let (fts_rows,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM search_index WHERE source_table='session_entries'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(fts_rows, 2, "search_index sweep dropped the orphan FTS row");
    }

    /// Empty extraction is treated as a parse failure → sweep does NOT fire.
    /// Prevents catastrophic wipe on a future malformed CLAUDE.md.
    #[tokio::test]
    async fn run_session_history_extractor_skips_sweep_on_empty_extraction() {
        let pool = fresh_pool().await;
        let tmp = tempfile::tempdir().unwrap();
        let claude_md = tmp.path().join("CLAUDE.md");

        std::fs::write(
            &claude_md,
            "# top\n\n## Recent session history\n\n- **#42**: only one.\n",
        )
        .unwrap();
        let doc_id = upsert_document(&pool, &claude_md, "session_history")
            .await
            .unwrap();
        run_session_history_extractor(&pool, doc_id, &claude_md, tmp.path())
            .await
            .unwrap();

        // Corrupt CLAUDE.md so the section heading vanishes → zero extracted.
        std::fs::write(&claude_md, "# top\n\nno session history section here\n").unwrap();
        let doc_id = upsert_document(&pool, &claude_md, "session_history")
            .await
            .unwrap();
        let r2 = run_session_history_extractor(&pool, doc_id, &claude_md, tmp.path())
            .await
            .unwrap();
        assert_eq!(r2.rows, 0);

        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM session_entries")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1, "empty extraction skipped sweep — #42 preserved");
    }

    /// G1 regression: a workspace containing two nested git projects, each with
    /// its own CLAUDE.md and `docs/design/`, gets fully indexed. Pre-fix this
    /// scenario silently dropped both nested CLAUDE.md files and both
    /// `docs/design/` trees because discovery was anchored to the workspace
    /// root only.
    #[tokio::test]
    async fn full_scan_walks_nested_project_anchors() {
        let pool = fresh_pool().await;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        for (proj, base) in [("proj_a", 100), ("proj_b", 300)] {
            let proj_dir = root.join(proj);
            std::fs::create_dir_all(&proj_dir).unwrap();
            git2::Repository::init(&proj_dir).unwrap();
            std::fs::write(
                proj_dir.join("CLAUDE.md"),
                format!(
                    "# {proj}\n\n## Recent session history\n\n\
                     - **#{a}**: alpha in {proj}.\n\
                     - **#{b}**: beta in {proj}.\n",
                    a = base,
                    b = base + 1,
                ),
            )
            .unwrap();
            let design = proj_dir.join("docs").join("design");
            std::fs::create_dir_all(&design).unwrap();
            std::fs::write(
                design.join("THING.md"),
                format!("# {proj} thing\n\n## Section A\n\nbody A\n\n## Section B\n\nbody B\n"),
            )
            .unwrap();
        }

        let stats = full_scan(&pool, root, None, Some(&root.join(".no-transcripts")), GitOptions::default(), None)
            .await
            .unwrap();

        assert_eq!(stats.session_history, 2, "indexed both nested CLAUDE.mds");
        assert_eq!(stats.session_entries, 4, "two entries from each CLAUDE.md");
        assert_eq!(stats.design_doc, 2, "indexed both nested design dirs");
        assert!(
            stats.design_sections >= 4,
            "expected >=4 sections (1 doc heading + 2 sections × 2 docs); got {}",
            stats.design_sections,
        );
    }

    /// RC1 review 1.2 — the domain invariant survives SYMLINKS. A labeled dir that
    /// links into another domain's tree (a tracked file link, a root `*.md` link,
    /// a `docs/` link, a `CLAUDE.md` link) must not pull that tree's content into
    /// this domain's db, in any of the three file corpora. Before `stays_within`
    /// and the source-side symlink skip, all four leaked. Mutation this catches:
    /// removing any one of the four guards → its needle appears in alpha.db.
    #[cfg(unix)]
    #[tokio::test]
    async fn full_scan_domain_isolation_is_not_defeated_by_symlinks() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let root = root.as_path();
        // beta: the victim tree — a repo with a secret source file, docs, a root md, a CLAUDE.md.
        let beta = root.join("beta");
        std::fs::create_dir_all(beta.join("docs")).unwrap();
        std::fs::write(beta.join("secret.rs"), "pub const BETASECRET_TOKEN_ZZ: u8 = 1;").unwrap();
        std::fs::write(beta.join("docs").join("plan.md"), "# Plan\n\nBETADOC_NEEDLE_ZZ here.\n").unwrap();
        std::fs::write(beta.join("ROADMAP.md"), "# Roadmap\n\nBETAROOT_NEEDLE_ZZ here.\n").unwrap();
        std::fs::write(
            beta.join("CLAUDE.md"),
            "# beta\n\n## Recent session history\n\n### #1 — 2026-01-01\n- BETACLAUDE_NEEDLE_ZZ\n",
        )
        .unwrap();
        // acme: the labeled dir — a repo whose tracked file, docs dir, root md and
        // CLAUDE.md are ALL symlinks into beta.
        let acme = root.join("acme");
        std::fs::create_dir_all(&acme).unwrap();
        symlink(beta.join("secret.rs"), acme.join("leak.rs")).unwrap();
        symlink(beta.join("docs"), acme.join("docs")).unwrap();
        symlink(beta.join("ROADMAP.md"), acme.join("notes.md")).unwrap();
        symlink(beta.join("CLAUDE.md"), acme.join("CLAUDE.md")).unwrap();
        std::fs::write(acme.join("own.rs"), "pub fn acme_own_fn() {}").unwrap();
        let repo = git2::Repository::init(&acme).unwrap();
        let mut idx = repo.index().unwrap();
        idx.add_path(Path::new("leak.rs")).unwrap();
        idx.add_path(Path::new("own.rs")).unwrap();
        idx.write().unwrap();
        let tree = repo.find_tree(idx.write_tree().unwrap()).unwrap();
        let sig = git2::Signature::now("t", "t@t").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "acme initial", &tree, &[]).unwrap();
        std::fs::write(
            root.join(".nibdex-domains.toml"),
            "[domains]\nalpha = [\"acme\"]\nother = [\"beta\"]\n",
        )
        .unwrap();

        let alpha = fresh_pool().await;
        let stats = full_scan(&alpha, root, None, Some(&root.join(".no-transcripts")), GitOptions::default(), Some("alpha"))
            .await
            .unwrap();
        assert!(stats.source_files >= 1, "acme's own file is indexed");

        for needle in ["BETASECRET_TOKEN_ZZ", "BETADOC_NEEDLE_ZZ", "BETAROOT_NEEDLE_ZZ", "BETACLAUDE_NEEDLE_ZZ"] {
            let hits: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM search_index WHERE body MATCH ?")
                .bind(needle)
                .fetch_one(&alpha)
                .await
                .unwrap();
            assert_eq!(hits, 0, "alpha.db holds beta's {needle} via a symlink under acme/");
        }
        let own: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM search_index WHERE body MATCH 'acme_own_fn'")
            .fetch_one(&alpha)
            .await
            .unwrap();
        assert_eq!(own, 1, "acme's real file is still indexed");
        let leaked_paths: Vec<String> =
            sqlx::query_scalar("SELECT path FROM documents WHERE path LIKE '%beta%'")
                .fetch_all(&alpha)
                .await
                .unwrap();
        assert!(leaked_paths.is_empty(), "no beta path in alpha.db: {leaked_paths:?}");
    }

    /// SESSION_SCOPE_DESIGN §0 — the domain-isolation INVARIANT: a `--domain` pass
    /// writes ONLY that domain's labeled subdirs, so a domain's db never holds
    /// another domain's content. This is the guardrail every later gear must keep
    /// green; it fails loudly the instant routing leaks across domains.
    #[tokio::test]
    async fn full_scan_domain_isolates_source_and_commits_per_db() {
        // Two labeled projects in ONE workspace, each a git repo with a committed
        // source file. `.nibdex-domains.toml` maps proj-a→alpha, proj-b→beta.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        for (proj, file, body, msg) in [
            ("proj-a", "alpha.rs", "pub fn alpha_widget() {}", "proj-a: alpha"),
            ("proj-b", "beta.rs", "pub fn beta_gadget() {}", "proj-b: beta"),
        ] {
            let dir = root.join(proj);
            std::fs::create_dir_all(&dir).unwrap();
            let repo = git2::Repository::init(&dir).unwrap();
            std::fs::write(dir.join(file), body).unwrap();
            let mut idx = repo.index().unwrap();
            idx.add_path(Path::new(file)).unwrap();
            idx.write().unwrap();
            let tree = repo.find_tree(idx.write_tree().unwrap()).unwrap();
            let sig = git2::Signature::now("t", "t@t").unwrap();
            repo.commit(Some("HEAD"), &sig, &sig, msg, &tree, &[]).unwrap();
        }
        std::fs::write(
            root.join(".nibdex-domains.toml"),
            "[domains]\nalpha = [\"proj-a\"]\nbeta = [\"proj-b\"]\n",
        )
        .unwrap();

        // Index each domain into its own db.
        let alpha = fresh_pool().await;
        full_scan(&alpha, root, None, Some(&root.join(".no-transcripts")), GitOptions::default(), Some("alpha"))
            .await
            .unwrap();
        let beta = fresh_pool().await;
        full_scan(&beta, root, None, Some(&root.join(".no-transcripts")), GitOptions::default(), Some("beta"))
            .await
            .unwrap();

        // Count content (source docs + commits) whose path/repo mentions `needle`.
        async fn mentions(pool: &SqlitePool, needle: &str) -> i64 {
            let like = format!("%{needle}%");
            let docs: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM documents WHERE kind='source' AND path LIKE ?")
                    .bind(&like)
                    .fetch_one(pool)
                    .await
                    .unwrap();
            let commits: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM commit_entries WHERE repo_path LIKE ?")
                    .bind(&like)
                    .fetch_one(pool)
                    .await
                    .unwrap();
            docs + commits
        }

        // THE INVARIANT: each db holds its own domain's content and ZERO of the other's.
        assert!(mentions(&alpha, "proj-a").await > 0, "alpha.db must contain proj-a");
        assert_eq!(mentions(&alpha, "proj-b").await, 0, "alpha.db must NOT contain proj-b");
        assert!(mentions(&beta, "proj-b").await > 0, "beta.db must contain proj-b");
        assert_eq!(mentions(&beta, "proj-a").await, 0, "beta.db must NOT contain proj-a");
    }

    // ============ LABEL-DEPTH SPECS — BULK CORPORA (source + commits) ============
    // docs/LABEL_DEPTH_DESIGN.md. The session-side specs live in session_index.rs
    // and cover edges + taint; these cover what `full_scan` admits, which is where
    // D6's "indexed by nobody" claim actually has to hold -- the leak was measured
    // here, with the foreign repo's source AND its entire commit history landing in
    // alpha.db. Implemented 2026-07-23; these now run in the release gate.
    //
    // NOTE the flag: nested repos are only DISCOVERABLE with `--include-nested-repos`
    // (NestedMode::Include). That is not an exotic setting — the workspace-container
    // layout REQUIRES it (nibdex commit 48fee0b) and the personal box's launchd plist
    // carries it. So the same flag that makes a legitimate nested project visible is
    // what makes a foreign nested repo admissible. A decoy run under the default
    // Skip mode would prove nothing.

    /// Sweep every bulk surface a nested repo could reach: document + chunk paths,
    /// chunk bodies, and commit repo/message/files. Path-only counting would miss
    /// content whose path happens not to carry the needle.
    async fn bulk_needle_hits(pool: &SqlitePool, needle: &str) -> i64 {
        let pat = format!("%{needle}%");
        sqlx::query_scalar::<_, i64>(
            "SELECT (SELECT COUNT(*) FROM documents WHERE path LIKE ?) \
             + (SELECT COUNT(*) FROM source_chunks WHERE path LIKE ? OR body LIKE ?) \
             + (SELECT COUNT(*) FROM commit_entries WHERE repo_path LIKE ? \
                  OR message_summary LIKE ? OR IFNULL(message_body,'') LIKE ? \
                  OR IFNULL(files_changed,'') LIKE ?)",
        )
        .bind(&pat).bind(&pat).bind(&pat).bind(&pat).bind(&pat).bind(&pat).bind(&pat)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    /// A git repo at `dir` holding one committed file.
    fn repo_with(dir: &Path, file: &str, body: &str, msg: &str) {
        std::fs::create_dir_all(dir).unwrap();
        let repo = git2::Repository::init(dir).unwrap();
        std::fs::write(dir.join(file), body).unwrap();
        let mut idx = repo.index().unwrap();
        idx.add_path(Path::new(file)).unwrap();
        idx.write().unwrap();
        let tree = repo.find_tree(idx.write_tree().unwrap()).unwrap();
        let sig = git2::Signature::now("t", "t@t").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, msg, &tree, &[]).unwrap();
    }

    /// A workspace whose labeled `proj-a` contains a FOREIGN repo one level down.
    fn depth_bulk_ws(config: &str) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        repo_with(&root.join("proj-a"), "alpha.rs", "pub fn alpha_widget() {}", "proj-a: alpha");
        repo_with(
            &root.join("proj-a/nested"),
            "secret.rs",
            "pub fn vendor_secret_token() {}",
            "nested: vendor_secret_token wiring",
        );
        std::fs::write(root.join(".nibdex-domains.toml"), config).unwrap();
        tmp
    }

    async fn scan_domain(root: &Path, domain: &str) -> SqlitePool {
        let pool = fresh_pool().await;
        let opts = GitOptions { nested_mode: NestedMode::Include, ..Default::default() };
        full_scan(&pool, root, None, Some(&root.join(".no-transcripts")), opts, Some(domain)).await.unwrap();
        pool
    }

    /// D6 (bulk) — an unlabeled nested repo under a labeled parent must reach NO
    /// database. Today `top_subdir` reads `proj-a` and admits its source and its
    /// entire commit history into alpha.
    #[tokio::test]
    async fn depth_spec_bulk_unlabeled_nested_repo_is_quarantined() {
        let tmp = depth_bulk_ws("[domains]\nalpha = [\"proj-a\"]\n");
        let alpha = scan_domain(tmp.path(), "alpha").await;
        assert!(bulk_needle_hits(&alpha, "alpha_widget").await > 0, "the parent is still indexed");
        for needle in ["vendor_secret_token", "nested"] {
            assert_eq!(
                bulk_needle_hits(&alpha, needle).await,
                0,
                "alpha.db holds {needle} from an unlabeled nested repo"
            );
        }
    }

    /// D3/D4 (bulk) — an explicitly withdrawn nested tree reaches no database
    /// either, which is the case where the owning domain does not exist here.
    #[tokio::test]
    async fn depth_spec_bulk_withdrawn_nested_repo_reaches_no_domain() {
        let tmp = depth_bulk_ws(
            "[domains]\nalpha = [\"proj-a\"]\n\n\
             [unassigned]\nacknowledged = [\"proj-a/nested\"]\n",
        );
        let alpha = scan_domain(tmp.path(), "alpha").await;
        assert!(bulk_needle_hits(&alpha, "alpha_widget").await > 0);
        assert_eq!(
            bulk_needle_hits(&alpha, "vendor_secret_token").await,
            0,
            "a withdrawn nested tree must not be indexed"
        );
    }

    /// D6 continuity (bulk) — the `learn/python-sandbox` case. Once labeled for
    /// its parent's own domain the nested repo is fully indexed again. This must
    /// pass BOTH before and after the depth work: it is the mutation guard
    /// against a "fix" that simply excludes every nested repo.
    #[tokio::test]
    async fn depth_spec_bulk_labeled_nested_repo_is_restored() {
        let tmp = depth_bulk_ws("[domains]\nalpha = [\"proj-a\", \"proj-a/nested\"]\n");
        let alpha = scan_domain(tmp.path(), "alpha").await;
        assert!(
            bulk_needle_hits(&alpha, "vendor_secret_token").await > 0,
            "a nested repo labeled for its own domain must be indexed"
        );
    }

    /// D1 gear-6 fix: design docs directly under `docs/` (NOT a `docs/design/`
    /// subdir) must be indexed. Pre-fix the extractor only looked in
    /// `docs/design/`, so nibdex's own `docs/*.md` were silently missed
    /// (D1_SCOPE §10 cross-corpus finding 3). `docs/` subsumes `docs/design/`,
    /// so both a top-level doc and a nested-subdir doc are picked up.
    #[tokio::test]
    async fn full_scan_indexes_design_docs_directly_under_docs() {
        let pool = fresh_pool().await;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        git2::Repository::init(root).unwrap();

        // A doc directly under docs/ (the nibdex layout) ...
        let docs = root.join("docs");
        std::fs::create_dir_all(docs.join("plans")).unwrap();
        std::fs::write(
            docs.join("DESIGN.md"),
            "# Design\n\n## Goals\n\nthe goals body\n",
        )
        .unwrap();
        // ... and one in a nested subdir (the ClearView layout) — both must land.
        std::fs::write(
            docs.join("plans").join("PLAN.md"),
            "# Plan\n\n## Phase 1\n\nphase one body\n",
        )
        .unwrap();

        let stats = full_scan(&pool, root, None, Some(&root.join(".no-transcripts")), GitOptions::default(), None)
            .await
            .unwrap();

        assert_eq!(
            stats.design_doc, 2,
            "both docs/DESIGN.md and docs/plans/PLAN.md indexed"
        );
        assert!(stats.design_sections >= 2, "sections from both docs");
    }

    /// 2026-07-09 blind-spot fix: root-level `*.md` (BUG_TRIAGE.md et al.) join the
    /// design-doc corpus, CLAUDE.md is NOT double-indexed, and a `docs/` doc still
    /// lands — all in one pass.
    #[tokio::test]
    async fn full_scan_indexes_root_markdown_but_not_claude_md() {
        let pool = fresh_pool().await;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        git2::Repository::init(root).unwrap();

        // Session journal at root — owned by session_history, must NOT become a design doc.
        std::fs::write(root.join("CLAUDE.md"), "# claude\n\ndev journal\n").unwrap();
        // The TODO source-of-truth at root — the file the fix exists to reach.
        std::fs::write(
            root.join("BUG_TRIAGE.md"),
            "# Triage\n\n## TODO #623\n\nsimilar bids search body\n",
        )
        .unwrap();
        // A sibling root doc rides along.
        std::fs::write(root.join("README.md"), "# Readme\n\n## Usage\n\nusage body\n").unwrap();
        // A conventional docs/ doc still lands.
        let docs = root.join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(docs.join("DESIGN.md"), "# Design\n\n## Goals\n\ngoals body\n").unwrap();

        let stats = full_scan(&pool, root, None, Some(&root.join(".no-transcripts")), GitOptions::default(), None)
            .await
            .unwrap();

        // BUG_TRIAGE.md + README.md + docs/DESIGN.md = 3 design docs; CLAUDE.md excluded.
        assert_eq!(
            stats.design_doc, 3,
            "root BUG_TRIAGE.md + README.md + docs/DESIGN.md indexed as design docs"
        );
        assert_eq!(stats.session_history, 1, "CLAUDE.md is the lone session doc");

        // CLAUDE.md stays session_history and gets ZERO design sections (no double-parse).
        let claude_kind: String =
            sqlx::query_scalar("SELECT kind FROM documents WHERE path LIKE '%/CLAUDE.md'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(claude_kind, "session_history");
        let claude_design_sections: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM design_doc_sections s JOIN documents d ON s.document_id = d.id \
             WHERE d.path LIKE '%/CLAUDE.md'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(claude_design_sections, 0, "CLAUDE.md not parsed as a design doc");

        // BUG_TRIAGE.md is a design_doc with searchable sections (the recall win).
        let triage_kind: String =
            sqlx::query_scalar("SELECT kind FROM documents WHERE path LIKE '%/BUG_TRIAGE.md'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(triage_kind, "design_doc");
        let triage_hits: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM search_index WHERE source_table = 'design_doc_sections' \
             AND search_index MATCH 'similar bids'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(triage_hits >= 1, "BUG_TRIAGE.md content is FTS-searchable via find_design_doc");
    }

    /// THE public defect this closes: a new user follows the README quickstart,
    /// runs `nibdex index`, calls `find_session` — and gets nothing, because the
    /// transcript corpus only ever populated from a separate command they had no
    /// reason to know existed. One `full_scan`, no flags, must leave the session
    /// corpus searchable.
    ///
    /// The fixture also carries a second workspace's transcript in a sibling slug,
    /// so the test proves the scope is DERIVED (not "whatever was in the projects
    /// dir") in the same pass.
    #[tokio::test]
    async fn full_scan_indexes_this_workspaces_session_edges() {
        use serde_json::{json, Value};

        let pool = fresh_pool().await;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        git2::Repository::init(&root).unwrap();
        let foreign_tmp = tempfile::tempdir().unwrap();
        let foreign = foreign_tmp.path().canonicalize().unwrap();

        let transcript = |cwd: &std::path::Path, session: &str, rationale: &str| -> String {
            let v: Value = json!({
                "type": "assistant",
                "sessionId": session,
                "uuid": format!("{session}-u1"),
                "cwd": cwd.to_string_lossy(),
                "gitBranch": "main",
                "timestamp": "2026-08-14T10:00:00.000Z",
                "message": { "id": format!("{session}-g1"), "content": [
                    {"type": "text", "text": rationale},
                    {"type": "tool_use", "name": "Edit",
                     "input": {"file_path": cwd.join("src/lib.rs").to_string_lossy()}}
                ]}
            });
            v.to_string()
        };

        let projects = tmp.path().join("projects");
        for (slug, cwd, session, rationale) in [
            ("-ws", root.as_path(), "mine", "wire the debouncer to the watcher"),
            (
                "-employer",
                foreign.as_path(),
                "theirs",
                "rotate the acmecorp billing token",
            ),
        ] {
            let d = projects.join(slug);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("s.jsonl"), transcript(cwd, session, rationale)).unwrap();
        }

        let stats = full_scan(&pool, &root, None, Some(&projects), GitOptions::default(), None)
            .await
            .unwrap();

        assert_eq!(stats.session_transcripts, 2, "both slugs read");
        assert_eq!(stats.session_edges, 1, "only this workspace's edge indexed");
        assert_eq!(stats.session_edges_dropped_foreign_workspace, 1);

        // The corpus `find_session` reads is populated and searchable BY RATIONALE
        // — the whole point of the transcript corpus over a path parser.
        let hits: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM search_index WHERE source_table = 'session_edges' \
             AND search_index MATCH 'debouncer'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(hits >= 1, "find_session corpus is empty after a plain `nibdex index`");

        // ... and the other workspace's rationale is nowhere in the db.
        let leaked: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM session_edges WHERE rationale LIKE '%acmecorp%'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(leaked, 0);
    }

    /// A session-pass failure must NOT discard the five corpora that already
    /// committed. Before this, an error here propagated out of `full_scan`, so
    /// `nibdex index` exited non-zero with a half-populated database and printed
    /// no summary — reporting total failure for a mostly-successful index.
    ///
    /// Triggered with an UNREADABLE transcript root: `is_dir()` still passes, so
    /// the pass is attempted, and the `read_dir` inside it fails. That is a real
    /// shape (a root owned by another user, or one racing a permission change),
    /// not a contrived one.
    #[cfg(unix)]
    #[tokio::test]
    async fn full_scan_survives_a_failing_session_pass() {
        use std::os::unix::fs::PermissionsExt;

        let pool = fresh_pool().await;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        git2::Repository::init(root).unwrap();
        std::fs::write(root.join("README.md"), "# R\n\n## Usage\n\nbody\n").unwrap();

        let projects = root.join("projects");
        std::fs::create_dir_all(&projects).unwrap();
        std::fs::set_permissions(&projects, std::fs::Permissions::from_mode(0o000)).unwrap();

        // Root ignores the mode bits, so the condition cannot be expressed here.
        // Say so and stop rather than assert something the environment isn't doing.
        if std::fs::read_dir(&projects).is_ok() {
            let _ = std::fs::set_permissions(&projects, std::fs::Permissions::from_mode(0o755));
            eprintln!("skipping: this environment can read a 0o000 dir (running as root?)");
            return;
        }

        let stats = full_scan(&pool, root, None, Some(&projects), GitOptions::default(), None)
            .await
            .expect("a failing session pass must not fail the whole scan");

        // Restore before the tempdir teardown, which needs to descend into it.
        std::fs::set_permissions(&projects, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(stats.design_doc, 1, "the other corpora still indexed");
        assert_eq!(stats.session_edges, 0);
        assert!(
            stats.session_index_error.is_some(),
            "the failure must be REPORTED, not silently swallowed"
        );
    }

    /// `nibdex index --domain X` must isolate the SESSION corpus per domain, not
    /// just source and commits.
    ///
    /// This had no gate at all, and the blind spot was self-inflicted: folding
    /// session indexing into `full_scan` meant every pre-existing domain test
    /// suddenly ran it, so they were all pointed at an absent transcript root to
    /// keep them focused — which left the newly-reachable domain path executed by
    /// nothing. Dropping `domain` from the `index_sessions` call kept the whole
    /// suite green. Per-domain isolation is this project's central security
    /// claim; it cannot rest on a fixture that never reaches the code.
    #[tokio::test]
    async fn full_scan_isolates_session_edges_per_domain() {
        use serde_json::{json, Value};

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        for sub in ["alpha-proj", "beta-proj"] {
            std::fs::create_dir_all(root.join(sub)).unwrap();
            std::fs::write(root.join(sub).join("x.rs"), "fn x() {}").unwrap();
        }
        std::fs::write(
            root.join(".nibdex-domains.toml"),
            "[domains]\nalpha = [\"alpha-proj\"]\nbeta = [\"beta-proj\"]\n",
        )
        .unwrap();

        // ONE session that edits both domains' trees.
        let mut ts = 0;
        let mut line = |uuid: &str, target: std::path::PathBuf, note: &str| -> Value {
            ts += 1;
            json!({
                "type": "assistant",
                "sessionId": "s1",
                "uuid": uuid,
                "cwd": root.to_string_lossy(),
                "timestamp": format!("2026-08-14T13:00:{ts:02}.000Z"),
                "message": { "id": format!("g{ts}"), "content": [
                    {"type": "text", "text": note},
                    {"type": "tool_use", "name": "Edit",
                     "input": {"file_path": target.to_string_lossy()}}
                ]}
            })
        };
        let lines = [
            line("u1", root.join("alpha-proj/x.rs"), "alpha_needle work"),
            line("u2", root.join("beta-proj/x.rs"), "beta_needle work"),
        ];
        let projects = root.join("projects");
        std::fs::create_dir_all(projects.join("-ws")).unwrap();
        std::fs::write(
            projects.join("-ws").join("s.jsonl"),
            lines.iter().map(Value::to_string).collect::<Vec<_>>().join("\n"),
        )
        .unwrap();

        let alpha = fresh_pool().await;
        full_scan(&alpha, &root, None, Some(&projects), GitOptions::default(), Some("alpha"))
            .await
            .unwrap();

        let paths: Vec<String> =
            sqlx::query_scalar("SELECT file_path FROM session_edges").fetch_all(&alpha).await.unwrap();
        assert_eq!(paths.len(), 1, "alpha.db must hold only alpha's edit");
        assert!(paths[0].contains("alpha-proj"), "got {paths:?}");

        // THE INVARIANT: beta's needle appears nowhere in alpha's database —
        // not in a path, not in a rationale, not in the FTS body.
        let leaked: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM search_index WHERE source_table = 'session_edges' \
             AND search_index MATCH 'beta_needle'",
        )
        .fetch_one(&alpha)
        .await
        .unwrap();
        assert_eq!(leaked, 0, "beta content leaked into alpha.db via the session pass");
    }

    /// A re-index must never DROP already-indexed edges.
    ///
    /// Claude Code prunes transcripts (~30 days), so the additive merge is what
    /// keeps an edge after its source transcript is gone. `full_scan` passes
    /// `rebuild = false` to guarantee that, and flipping it to `true` kept the
    /// whole suite green — a silent data-loss path guarded only by a comment.
    #[tokio::test]
    async fn full_scan_never_drops_edges_whose_transcript_has_rotated_away() {
        use serde_json::{json, Value};

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        git2::Repository::init(&root).unwrap();
        std::fs::write(root.join("a.rs"), "fn a() {}").unwrap();

        let line: Value = json!({
            "type": "assistant",
            "sessionId": "s1",
            "uuid": "u1",
            "cwd": root.to_string_lossy(),
            "timestamp": "2026-08-14T14:00:00.000Z",
            "message": { "id": "g1", "content": [
                {"type": "text", "text": "keepme rotated away"},
                {"type": "tool_use", "name": "Edit",
                 "input": {"file_path": root.join("a.rs").to_string_lossy()}}
            ]}
        });
        let projects = root.join("projects");
        std::fs::create_dir_all(projects.join("-ws")).unwrap();
        let transcript = projects.join("-ws").join("s.jsonl");
        std::fs::write(&transcript, line.to_string()).unwrap();

        let pool = fresh_pool().await;
        let first = full_scan(&pool, &root, None, Some(&projects), GitOptions::default(), None)
            .await
            .unwrap();
        assert_eq!(first.session_edges, 1);

        // Claude Code prunes the transcript, exactly as it does at ~30 days.
        std::fs::remove_file(&transcript).unwrap();

        let second = full_scan(&pool, &root, None, Some(&projects), GitOptions::default(), None)
            .await
            .unwrap();
        assert_eq!(second.session_transcripts, 0, "the source really is gone");

        let kept: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM session_edges")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(kept, 1, "a re-index wiped an edge whose transcript had rotated away");
        let fts: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM search_index WHERE source_table = 'session_edges'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(fts, 1, "its FTS row must survive too, or it is unfindable");
    }

    /// A machine with no transcript root at all (Claude Code never run, or no
    /// `$HOME`) must still index the other five corpora. The session corpus being
    /// empty is a fact to report, never a reason to fail the scan.
    #[tokio::test]
    async fn full_scan_survives_a_missing_transcript_root() {
        let pool = fresh_pool().await;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        git2::Repository::init(root).unwrap();
        std::fs::write(root.join("README.md"), "# R\n\n## Usage\n\nbody\n").unwrap();

        let stats = full_scan(
            &pool,
            root,
            None,
            Some(&root.join("definitely-not-here")),
            GitOptions::default(),
            None,
        )
        .await
        .unwrap();

        assert_eq!(stats.session_edges, 0);
        assert_eq!(stats.session_transcripts, 0);
        assert_eq!(stats.design_doc, 1, "the other corpora are unaffected");
    }
}
