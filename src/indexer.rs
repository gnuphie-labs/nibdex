// SPDX-License-Identifier: MIT

use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde_json::json;
use sqlx::SqlitePool;
use walkdir::WalkDir;

use crate::extractor::design_doc;
use crate::extractor::git_commits::{self, NestedMode};
use crate::extractor::memory;
use crate::extractor::session_history;
use crate::hash::sha256_file;
use crate::metrics::Op;

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
    pub extract_session_history_ms: u128,
    pub extract_memory_ms: u128,
    pub extract_design_docs_ms: u128,
    pub extract_commits_ms: u128,
    pub extract_source_ms: u128,
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
pub fn default_memory_dir(workspace: &Path) -> Option<PathBuf> {
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
    Some(
        PathBuf::from(home)
            .join(".claude/projects")
            .join(encoded)
            .join("memory"),
    )
}

pub async fn full_scan(
    pool: &SqlitePool,
    workspace: &Path,
    memory_dir: Option<&Path>,
    git_opts: GitOptions,
) -> Result<ScanStats> {
    let op = Op::start("indexer.full_scan");
    let started = Instant::now();
    let mut stats = ScanStats::default();

    let workspace = workspace
        .canonicalize()
        .with_context(|| format!("canonicalize workspace {workspace:?}"))?;

    // Project anchors drive session_history + design_doc discovery. Each anchor
    // = the workspace root or a discovered git repo root (see
    // `discover_project_anchors`). Per-anchor scope means a nested CLAUDE.md
    // resolves its `files_touched` references against its own project root,
    // not the parent workspace.
    let anchors = discover_project_anchors(&workspace, git_opts);

    // 1. session_history — one `$ANCHOR/CLAUDE.md` per project anchor.
    for anchor in &anchors {
        let claude_md = anchor.join("CLAUDE.md");
        if claude_md.exists() {
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
    if let Some(mem) = memory.as_ref()
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
    let commit_stats = run_git_commits_extractor(pool, &workspace, git_opts).await?;
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
    for repo in git_commits::discover_repos(&workspace, git_opts.max_depth, git_opts.nested_mode) {
        let src = crate::source_index::index_source_repo(pool, &repo).await?;
        stats.source_files += src.files_indexed as u32;
        stats.source_chunks += src.chunks as u32;
        stats.source_skipped_other_corpus += src.skipped_other_corpus as u32;
        stats.extract_source_ms += src.elapsed_ms;
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
                "duration_ms": stats.extract_source_ms,
            },
        },
    });
    op.complete(pool, None, Some(stats.total() as i64), extra)
        .await?;

    Ok(stats)
}

fn walk_markdown(root: &Path) -> Vec<PathBuf> {
    WalkDir::new(root)
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

    for path in walk_markdown(mem_dir) {
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
        paths.extend(walk_markdown(&docs));
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
) -> Result<GitExtractorResult> {
    let op = Op::start("extract.commits");
    let started = Instant::now();

    let repos = git_commits::discover_repos(workspace, opts.max_depth, opts.nested_mode);
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
/// Uses the conservative Day 4 cap (50,000) to defend against pathological
/// histories at first-touch.
pub async fn reindex_commits_for_repo(pool: &SqlitePool, repo_path: &Path) -> Result<()> {
    let op = Op::start("watcher.reindex_commits");
    let outcome =
        index_one_repo(pool, repo_path, GitOptions::default().max_commits_per_repo).await?;
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

        let stats = full_scan(&pool, root, None, GitOptions::default())
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

        let stats = full_scan(&pool, root, None, GitOptions::default())
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

        let stats = full_scan(&pool, root, None, GitOptions::default())
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
}
