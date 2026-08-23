// SPDX-License-Identifier: MIT

//! D1a — the source extractor (docs/D1_SCOPE.md §5 "the first dish").
//!
//! Indexes a repo's **git-tracked** files (auto-respects `.gitignore`, excludes
//! `target/`, `node_modules/`, build junk) as the 5th corpus, in the proven
//! `documents → child-table → search_index` mold. The file CONTENT read is the
//! WORKING TREE at index time (not the HEAD blob) — see LIMITATIONS "find_code
//! indexes the working tree" — while provenance is last-touch at HEAD:
//!   - one `documents` row per file (`kind='source'`),
//!   - fixed line-window `source_chunks` rows (no overlap — D1_SCOPE §4.#3 MVP),
//!   - one `search_index` FTS5 row per chunk (`source_table='source_chunks'`).
//!
//! Provenance rides WITH the code: a single `git log --name-only` pass maps each
//! file to the most-recent commit that touched it, and every chunk is stamped
//! with that commit's `commit_entries.id` (so a code hit carries its commit
//! message, and through it the design/session links — D1_SCOPE §1, §7).
//!
//! Graduated from the gears 1–4 spike: now driven by `indexer::full_scan` +
//! re-indexed on commit by the watcher's `GitRefs` path, and queried through the
//! `find_code` MCP tool. Provenance + file enumeration go through **libgit2**
//! (`git2`), so no `git` binary on PATH is required (the D1b git2 migration —
//! docs/GIT2_MIGRATION_PLAN.md). Run the commits extractor first (full_scan does)
//! so `commit_entries` exists for provenance to resolve against.
//!
//! ONE-CORPUS-PER-FILE (D1_SCOPE §10 gear-1..4 "path-collision wrinkle"): the
//! `documents` table is `UNIQUE(path)` and its `ON CONFLICT(path)` upsert does NOT
//! rewrite `kind`, so a file indexed by two corpora would grow children under one
//! mislabeled row. Source therefore SKIPS files another corpus owns — design docs
//! (`docs/**/*.md`) and the session `CLAUDE.md` — see `is_owned_by_other_corpus`.

use std::collections::HashMap;
use std::path::Path;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;

/// Skip files larger than this (D1_SCOPE §3 workstream A: "skip > N KB").
const MAX_FILE_BYTES: u64 = 256 * 1024;
/// Fixed line-window size, no overlap (D1_SCOPE §4.#3 MVP recommendation).
const CHUNK_LINES: usize = 50;

#[derive(Debug, Default)]
pub struct SourceIndexStats {
    pub files_tracked: usize,
    pub files_indexed: usize,
    pub skipped_binary: usize,
    pub skipped_large: usize,
    pub skipped_unreadable: usize,
    /// Git-tracked symlinks whose target resolves OUTSIDE the repo root — read
    /// through, they would index another tree's content under this repo's path
    /// (and, in domain mode, another domain's content into this db — RC1 review
    /// 1.2). In-repo symlinks are also skipped: their target is indexed at its
    /// own path already.
    pub skipped_symlink: usize,
    /// Files skipped because another corpus owns them (design docs, session
    /// CLAUDE.md) — the one-corpus-per-file guard (D1_SCOPE §10 path-collision).
    pub skipped_other_corpus: usize,
    /// Files whose content hash matched the stored `documents.content_hash` —
    /// their chunks + FTS rows were left untouched (the write-amp fix: a commit
    /// no longer rewrites the whole corpus, only the files it changed).
    pub files_unchanged: usize,
    /// Files that were indexed by a prior pass but are no longer git-tracked
    /// (deleted, renamed, or untracked since) — their document + chunks + FTS
    /// rows were removed. Without this the corpus only ever grew, and a
    /// deleted file kept answering `find_code` as `file_missing` forever while
    /// `check()` reported it healthy (RC1 review 1.6).
    pub files_pruned: usize,
    pub chunks: usize,
    /// Files whose provenance commit resolved to a `commit_entries.id`.
    pub provenance_hits: usize,
    /// Files with no resolvable provenance (commit not indexed, or untracked-in-log).
    pub provenance_misses: usize,
    pub elapsed_ms: u128,
}

pub async fn index_source_repo(pool: &SqlitePool, repo: &Path) -> Result<SourceIndexStats> {
    let started = Instant::now();
    let mut stats = SourceIndexStats::default();

    // 1. Provenance backfill — ONE history walk → file → most-recent commit hash.
    //    (Avoids a per-file walk.) A freshly-init'd repo with no commits has no
    //    provenance (unborn HEAD), matching the commits extractor's graceful
    //    handling: skip provenance, still index tracked files.
    // 2. Enumerate git-tracked files (the index) at HEAD.
    //    Both use libgit2 (no `git` binary on PATH required); the git2 handle is
    //    opened + dropped here, before any `.await`, so the source-index future
    //    stays `Send`.
    let (provenance, files) = {
        let git_repo = git2::Repository::open(repo)
            .with_context(|| format!("git2::Repository::open({repo:?})"))?;
        let provenance = if repo_has_commits(&git_repo) {
            build_provenance_map(&git_repo)?
        } else {
            HashMap::new()
        };
        let files = git_ls_files(&git_repo)?;
        (provenance, files)
    };
    stats.files_tracked = files.len();

    // Stamped on every chunk so a `find_code` hit says which tree it is in.
    // Same derivation as `commit_entries.repo_path` (that extractor takes the
    // `to_string_lossy` of this same `&Path`), so the two agree and a caller can
    // filter code and commits by one repo string.
    let repo_path_str = repo.to_string_lossy().into_owned();

    // commit_hash → commit_entries.id, so each file costs at most one lookup.
    let mut commit_id_cache: HashMap<String, Option<i64>> = HashMap::new();

    // ONE transaction for the whole repo pass (write-amp fix: ~1,100 per-statement
    // autocommit fsyncs per fire collapsed into a single WAL commit — the diagnosed
    // 4.3 s / 60-file reindex was mostly this). Readers are unaffected (WAL).
    let mut tx = pool.begin().await?;

    for rel in &files {
        // One-corpus-per-file: skip files the design-doc / session extractors own,
        // so we never grow a second child table under their `documents` row (whose
        // `kind` the upsert won't rewrite). See module docs + D1_SCOPE §10.
        if is_owned_by_other_corpus(rel) {
            stats.skipped_other_corpus += 1;
            continue;
        }
        let abs = repo.join(rel);
        // Git stores a symlink as a blob holding the link text; `fs::read` would
        // follow it. Never read through a link: the content is either outside
        // this repo (foreign — possibly another IP domain's) or already indexed
        // at its real path.
        if std::fs::symlink_metadata(&abs).is_ok_and(|m| m.file_type().is_symlink()) {
            stats.skipped_symlink += 1;
            continue;
        }
        let bytes = match std::fs::read(&abs) {
            Ok(b) => b,
            Err(_) => {
                stats.skipped_unreadable += 1;
                continue;
            }
        };
        if bytes.len() as u64 > MAX_FILE_BYTES {
            stats.skipped_large += 1;
            continue;
        }
        if bytes.contains(&0u8) {
            // Null-byte sniff = binary (matches git's own text/binary heuristic).
            stats.skipped_binary += 1;
            continue;
        }
        // Require valid UTF-8. A NUL-free file that still isn't valid UTF-8 is
        // either a binary the null-byte sniff missed or a legacy non-UTF-8
        // encoding; a lossy decode would index replacement-char mojibake either
        // way, so skip it rather than pollute the FTS index. Tracked source is
        // UTF-8 by modern convention.
        let text = match std::str::from_utf8(&bytes) {
            Ok(t) => t,
            Err(_) => {
                stats.skipped_binary += 1;
                continue;
            }
        };

        // Resolve provenance for this file's most-recent commit.
        let last_commit_id = match provenance.get(rel) {
            Some(hash) => resolve_commit_id(&mut tx, &mut commit_id_cache, hash).await?,
            None => None,
        };
        if last_commit_id.is_some() {
            stats.provenance_hits += 1;
        } else {
            stats.provenance_misses += 1;
        }

        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        let hash = format!("{:x}", hasher.finalize());
        let mtime = file_mtime(&abs);

        // CONTENT-HASH SKIP (write-amp fix): when the file is byte-identical to
        // what's indexed, leave its chunks + FTS rows untouched — a commit then
        // rewrites only the files it actually changed, not the whole corpus.
        let stored: Option<(i64, String, i64)> =
            sqlx::query_as("SELECT id, content_hash, mtime FROM documents WHERE path = ?")
                .bind(abs.to_string_lossy().as_ref())
                .fetch_optional(&mut *tx)
                .await?;
        if let Some((doc_id, stored_hash, stored_mtime)) = stored
            && stored_hash == hash
        {
            stats.files_unchanged += 1;
            // Keep the freshness gate's stat fast-path warm: a touch/checkout that
            // moved mtime without changing content would otherwise force the gate
            // onto its hash-confirm slow path on every query.
            if mtime != stored_mtime {
                sqlx::query("UPDATE documents SET mtime = ? WHERE id = ?")
                    .bind(mtime)
                    .bind(doc_id)
                    .execute(&mut *tx)
                    .await?;
            }
            // Provenance-only edge: a later commit re-touched the file without a
            // net content change (revert-to-identical). Refresh the code↔commit
            // join in place — no body change, so no FTS rewrite needed.
            let current: Option<Option<i64>> = sqlx::query_scalar(
                "SELECT last_commit_id FROM source_chunks WHERE document_id = ? LIMIT 1",
            )
            .bind(doc_id)
            .fetch_optional(&mut *tx)
            .await?;
            if let Some(current) = current
                && current != last_commit_id
            {
                sqlx::query("UPDATE source_chunks SET last_commit_id = ? WHERE document_id = ?")
                    .bind(last_commit_id)
                    .bind(doc_id)
                    .execute(&mut *tx)
                    .await?;
            }
            continue;
        }

        let language = language_of(rel);
        let document_id = upsert_source_document(&mut tx, &abs, &hash, mtime).await?;
        // Incremental wipe-and-reinsert (the mold) so re-runs are idempotent.
        wipe_existing_chunks(&mut tx, document_id).await?;

        let n =
            index_file_chunks(&mut tx, document_id, &repo_path_str, rel, language, text, last_commit_id)
                .await?;
        stats.chunks += n;
        stats.files_indexed += 1;
    }

    // PRUNE: any `documents(kind='source')` row under this repo whose path is
    // no longer in the git index has left the tree (deleted, renamed, or
    // untracked). Drop its chunks + FTS rows + document so the corpus tracks
    // the tree instead of accreting every path it ever saw. Only paths that
    // are DIRECT children of this repo root are considered — a nested repo's
    // files belong to its own pass. Working-tree-only deletions (file removed
    // but still in the index) are NOT pruned here: they stay `file_missing`
    // at query time and count under `check().orphans.source_chunks`.
    let prefix = format!("{}/", repo_path_str.trim_end_matches('/'));
    let stored_docs: Vec<(i64, String)> =
        sqlx::query_as("SELECT id, path FROM documents WHERE kind = 'source' AND substr(path, 1, ?) = ?")
            .bind(prefix.chars().count() as i64)
            .bind(&prefix)
            .fetch_all(&mut *tx)
            .await?;
    let tracked: std::collections::HashSet<String> =
        files.iter().map(|rel| format!("{prefix}{rel}")).collect();
    for (doc_id, path) in stored_docs {
        if tracked.contains(&path) {
            continue;
        }
        // A NESTED repo's files sit under this prefix too (`--include-nested-repos`
        // with a workspace root that is itself a repo) but are tracked by THAT
        // repo, not this one — they must not be pruned by the container's pass.
        // Seen live: the container pruned 268 nested-repo documents, which the
        // nested passes then rewrote from scratch on every index. Skip anything
        // that has a `.git` (dir or worktree file) between it and this root.
        if belongs_to_nested_repo(repo, Path::new(&path)) {
            continue;
        }
        wipe_existing_chunks(&mut tx, doc_id).await?;
        sqlx::query("DELETE FROM documents WHERE id = ?")
            .bind(doc_id)
            .execute(&mut *tx)
            .await?;
        stats.files_pruned += 1;
    }

    tx.commit().await?;
    stats.elapsed_ms = started.elapsed().as_millis();
    Ok(stats)
}

/// Is `abs` inside a git repo nested BELOW `root` (a `.git` dir or worktree file
/// on some ancestor strictly between `abs`'s parent and `root`)?
fn belongs_to_nested_repo(root: &Path, abs: &Path) -> bool {
    let mut dir = abs.parent();
    while let Some(d) = dir {
        if d == root {
            return false;
        }
        if d.join(".git").exists() {
            return true;
        }
        dir = d.parent();
    }
    false
}

/// Modified-time as unix secs — the one derivation shared by the extractor and
/// the freshness gate, so equality always means "unchanged since indexing".
fn file_mtime(abs: &Path) -> i64 {
    std::fs::metadata(abs)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Repo-relative tracked paths (mirrors `git ls-files`): the git index entries.
/// Paths are stored as bytes, repo-relative, forward-slash separated.
fn git_ls_files(repo: &git2::Repository) -> Result<Vec<String>> {
    let index = repo.index().context("reading the git index")?;
    let mut files = Vec::with_capacity(index.len());
    for entry in index.iter() {
        if let Ok(path) = String::from_utf8(entry.path)
            && !path.is_empty()
        {
            files.push(path);
        }
    }
    Ok(files)
}

/// True when the repo has at least one commit (a resolvable `HEAD`). A repo with
/// none (`git init`, nothing committed) has an unborn HEAD, so `head()` errors.
fn repo_has_commits(repo: &git2::Repository) -> bool {
    repo.head().is_ok()
}

/// One history walk → file → most-recent commit hash. Mirrors the old
/// `git log --name-only --no-renames` pass:
///   - newest-first order (`TOPOLOGICAL | TIME`, i.e. `git log --topo-order`), so
///     the first time a path is seen is its most-recent-touching commit;
///   - renames are NOT followed — a rename shows as delete+add, listing BOTH
///     paths (git2 `diff_tree_to_tree` does no rename detection unless asked);
///   - a merge commit contributes NO paths (`git log --name-only` lists none for
///     a merge by default) — we skip commits with >1 parent;
///   - the root commit diffs against the empty tree, so all its files are listed.
fn build_provenance_map(repo: &git2::Repository) -> Result<HashMap<String, String>> {
    let mut walk = repo.revwalk()?;
    walk.set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::TIME)?;
    if walk.push_head().is_err() {
        // Unborn HEAD (guarded by repo_has_commits, but be defensive).
        return Ok(HashMap::new());
    }
    let mut map: HashMap<String, String> = HashMap::new();
    for oid_res in walk {
        let oid = match oid_res {
            Ok(o) => o,
            Err(_) => continue,
        };
        let commit = match repo.find_commit(oid) {
            Ok(c) => c,
            Err(_) => continue,
        };
        // Merge commits: `git log --name-only` lists no paths for them by default.
        if commit.parent_count() > 1 {
            continue;
        }
        let tree = commit.tree()?;
        let parent_tree = if commit.parent_count() == 1 {
            Some(commit.parent(0)?.tree()?)
        } else {
            None // root commit → diff against the empty tree (all files).
        };
        let mut opts = git2::DiffOptions::new();
        let diff = repo.diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), Some(&mut opts))?;
        let hash = oid.to_string();
        diff.foreach(
            &mut |delta, _| {
                let path = delta.new_file().path().or_else(|| delta.old_file().path());
                if let Some(p) = path {
                    // Keep only the first (most-recent) sighting of each path.
                    map.entry(p.to_string_lossy().into_owned())
                        .or_insert_with(|| hash.clone());
                }
                true
            },
            None,
            None,
            None,
        )?;
    }
    Ok(map)
}

/// commit_hash → commit_entries.id, memoized. `None` when the commit isn't in the
/// index (e.g. `nibdex index` wasn't run, or the commit was capped out).
async fn resolve_commit_id(
    conn: &mut sqlx::SqliteConnection,
    cache: &mut HashMap<String, Option<i64>>,
    hash: &str,
) -> Result<Option<i64>> {
    if let Some(hit) = cache.get(hash) {
        return Ok(*hit);
    }
    let id: Option<i64> =
        sqlx::query_scalar("SELECT id FROM commit_entries WHERE commit_hash = ? LIMIT 1")
            .bind(hash)
            .fetch_optional(&mut *conn)
            .await?;
    cache.insert(hash.to_string(), id);
    Ok(id)
}

/// Insert/refresh the `documents` row for a source file. `documents.path` is the
/// absolute path (its own UNIQUE namespace); `source_chunks.path` is repo-relative
/// (so it joins to `commit_entries.files_changed`, which git stores relative).
/// `hash`/`mtime` arrive precomputed — the caller already derived them for the
/// content-hash skip check.
async fn upsert_source_document(
    conn: &mut sqlx::SqliteConnection,
    abs: &Path,
    hash: &str,
    mtime: i64,
) -> Result<i64> {
    let indexed_at = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
    let path_str = abs.to_string_lossy().into_owned();

    let row: (i64,) = sqlx::query_as(
        r#"
        INSERT INTO documents (path, kind, content_hash, mtime, indexed_at)
        VALUES (?, 'source', ?, ?, ?)
        ON CONFLICT(path) DO UPDATE SET
            content_hash = excluded.content_hash,
            mtime        = excluded.mtime,
            indexed_at   = excluded.indexed_at
        RETURNING id
        "#,
    )
    .bind(path_str)
    .bind(hash)
    .bind(mtime)
    .bind(indexed_at)
    .fetch_one(&mut *conn)
    .await?;
    Ok(row.0)
}

/// Drop a document's existing chunks + their FTS rows before reinsert (FTS5 is
/// not FK-aware, so prune `search_index` explicitly — mirrors `indexer.rs`).
async fn wipe_existing_chunks(conn: &mut sqlx::SqliteConnection, document_id: i64) -> Result<()> {
    let ids: Vec<i64> = sqlx::query_scalar("SELECT id FROM source_chunks WHERE document_id = ?")
        .bind(document_id)
        .fetch_all(&mut *conn)
        .await?;
    if ids.is_empty() {
        return Ok(());
    }
    for id in &ids {
        sqlx::query(
            "DELETE FROM search_index WHERE source_table = 'source_chunks' AND rowid_ref = ?",
        )
        .bind(id)
        .execute(&mut *conn)
        .await?;
    }
    sqlx::query("DELETE FROM source_chunks WHERE document_id = ?")
        .bind(document_id)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Split a file into fixed line-windows and persist each as a `source_chunks`
/// row + its `search_index` row. Whitespace-only windows are skipped (no FTS value).
async fn index_file_chunks(
    conn: &mut sqlx::SqliteConnection,
    document_id: i64,
    repo_path: &str,
    rel: &str,
    language: Option<&str>,
    text: &str,
    last_commit_id: Option<i64>,
) -> Result<usize> {
    let lines: Vec<&str> = text.lines().collect();
    let mut count = 0usize;
    let mut start = 0usize;
    while start < lines.len() {
        let end = (start + CHUNK_LINES).min(lines.len());
        let body = lines[start..end].join("\n");
        if body.trim().is_empty() {
            start = end;
            continue;
        }
        let line_start = (start + 1) as i64; // 1-based, inclusive
        let line_end = end as i64;
        let row: (i64,) = sqlx::query_as(
            r#"
            INSERT INTO source_chunks
                (document_id, repo_path, path, line_start, line_end, language, body,
                 last_commit_id)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?)
            RETURNING id
            "#,
        )
        .bind(document_id)
        .bind(repo_path)
        .bind(rel)
        .bind(line_start)
        .bind(line_end)
        .bind(language)
        .bind(&body)
        .bind(last_commit_id)
        .fetch_one(&mut *conn)
        .await?;

        sqlx::query(
            "INSERT INTO search_index (body, kind, rowid_ref, source_table) \
             VALUES (?, 'source', ?, 'source_chunks')",
        )
        .bind(&body)
        .bind(row.0)
        .execute(&mut *conn)
        .await?;

        count += 1;
        start = end;
    }
    Ok(count)
}

// ---------------------------------------------------------------------------
// Freshness gate (D1_SCOPE "Known limitation — stale indexed LOCATION";
// DESIGN §9.4 "explicit freshness signals in tool output"). The index is a
// snapshot of the last-indexed state and is healed only on commit, so a hit's
// line range goes stale the moment the working tree changes. Every find_code
// hit therefore carries a LocationStatus established at query time: stat() the
// live file (µs — the whole cost when the tree is clean), and only when mtime
// moved, hash-confirm (a touch without change is still Verified). The §9.1
// measurement gear (2026-06-12) showed 3/3 post-edit hits hand back the
// pre-edit line; this gate makes that failure honest instead of silent. The
// re-locator that CORRECTS the range is the follow-up commit.
// ---------------------------------------------------------------------------

/// How a hit's returned location relates to the live working tree at query time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocationStatus {
    /// The live file is byte-identical to what was indexed — the range is current.
    Verified,
    /// The file changed since indexing and the chunk was re-anchored in the live
    /// file — the returned range is corrected (`line_shift` carries the move).
    Relocated,
    /// The file changed since indexing and the chunk could NOT be re-anchored —
    /// the returned range is the stored one and may be wrong.
    Stale,
    /// The indexed file no longer exists at its recorded path.
    FileMissing,
}

impl LocationStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            LocationStatus::Verified => "verified",
            LocationStatus::Relocated => "relocated",
            LocationStatus::Stale => "stale",
            LocationStatus::FileMissing => "file_missing",
        }
    }
}

/// Memoized per-file working-tree state so multiple hits in one file pay the
/// stat/read once. `Changed` carries the live text — the relocator's input —
/// so the hash-confirm read is reused, never repeated.
enum FileState {
    Fresh,
    Missing,
    Changed(String),
}

/// Per-query memo (key = the `documents` absolute path).
pub struct FreshnessCache {
    map: HashMap<String, FileState>,
}

impl FreshnessCache {
    pub fn new() -> Self {
        FreshnessCache { map: HashMap::new() }
    }
}

impl Default for FreshnessCache {
    fn default() -> Self {
        Self::new()
    }
}

/// A hit's location after the freshness gate + (when needed) the re-locator:
/// possibly-corrected line numbers, the status, and the shift that was applied.
pub struct ResolvedLocation {
    pub status: LocationStatus,
    pub line_start: i64,
    pub line_end: i64,
    pub match_line: i64,
    /// `Some(n)` only when `status == Relocated`: the chunk moved n lines (+down/-up).
    pub line_shift: Option<i64>,
}

/// A chunk's stored (index-time) coordinates, as the DB recorded them — the
/// re-locator's input alongside the live file.
pub struct StoredChunk<'a> {
    pub body: &'a str,
    pub line_start: i64,
    pub line_end: i64,
    pub match_line: i64,
}

/// The full per-hit resolution: stat-first freshness gate (mtime, then
/// hash-confirm), and on a changed file a fix-up-on-read re-location of the
/// chunk in the live text. READ-ONLY by design — the index is never written
/// from here (the §1 LOCK: committed state stays the source of truth; only the
/// *returned location* is made live).
pub fn resolve_location(
    cache: &mut FreshnessCache,
    abs_path: &str,
    stored_mtime: i64,
    stored_hash: &str,
    chunk: &StoredChunk<'_>,
) -> ResolvedLocation {
    let state = cache
        .map
        .entry(abs_path.to_string())
        .or_insert_with(|| check_file(Path::new(abs_path), stored_mtime, stored_hash));
    let stored = |status| ResolvedLocation {
        status,
        line_start: chunk.line_start,
        line_end: chunk.line_end,
        match_line: chunk.match_line,
        line_shift: None,
    };
    match state {
        FileState::Fresh => stored(LocationStatus::Verified),
        FileState::Missing => stored(LocationStatus::FileMissing),
        FileState::Changed(live_text) => {
            match relocate_chunk(chunk.body, chunk.line_start, live_text) {
                Some(shift) => {
                    let file_lines = live_text.lines().count() as i64;
                    let new_start = (chunk.line_start + shift).max(1);
                    let new_end = (chunk.line_end + shift).min(file_lines).max(new_start);
                    ResolvedLocation {
                        status: LocationStatus::Relocated,
                        line_start: new_start,
                        line_end: new_end,
                        match_line: (chunk.match_line + shift).clamp(new_start, new_end),
                        line_shift: Some(shift),
                    }
                }
                None => stored(LocationStatus::Stale),
            }
        }
    }
}

/// The unmemoized file check. Derives mtime exactly as `upsert_source_document`
/// stored it (modified-time as unix secs) so equality means "unchanged since
/// indexing". On a content change the live text rides back in `Changed` for the
/// re-locator. Text is normalized through `from_utf8_lossy` + `lines()` exactly
/// as the extractor built chunk bodies, so "verbatim" means the same thing on
/// both sides.
fn check_file(abs: &Path, stored_mtime: i64, stored_hash: &str) -> FileState {
    if std::fs::metadata(abs).is_err() {
        return FileState::Missing;
    }
    // Same modified-secs derivation the extractor stores (file_mtime — shared fn).
    if file_mtime(abs) == stored_mtime {
        return FileState::Fresh;
    }
    // mtime moved — confirm by content before declaring change (parity with the
    // extractor: sha256 over the raw bytes).
    let Ok(bytes) = std::fs::read(abs) else {
        return FileState::Missing;
    };
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    if format!("{:x}", hasher.finalize()) == stored_hash {
        FileState::Fresh
    } else {
        FileState::Changed(String::from_utf8_lossy(&bytes).into_owned())
    }
}

/// Re-anchor a stored chunk in the live file text; returns the whole-chunk line
/// shift, or `None` when no confident anchor exists (→ `Stale`, the honest
/// fallback). Pure — no I/O — so every branch is unit-testable.
///
/// Stage 1 — exact whole-body match: find every position where the chunk's full
/// line sequence appears verbatim; nearest-to-stored wins (duplicate boilerplate
/// windows tie-break toward the stored position). This alone resolves the
/// dominant real case: an edit ABOVE the chunk shifted it wholesale.
///
/// Stage 2 — distinctive-line offset voting (the edit landed INSIDE the chunk, so
/// the whole body no longer exists verbatim): each distinctive chunk line that
/// reappears in the live file votes for the offset it implies; the winning offset
/// needs a ≥50% quorum of the distinctive lines (minimum 3, else stage-1-only).
/// Lines are compared TRIMMED so a re-indent doesn't defeat the vote. A torn
/// chunk (insertion mid-chunk splits the votes) can fail quorum → `None`; that is
/// deliberate — a range genuinely torn in two has no single honest shift.
fn relocate_chunk(chunk_body: &str, stored_line_start: i64, file_text: &str) -> Option<i64> {
    let chunk: Vec<&str> = chunk_body.lines().collect();
    let file: Vec<&str> = file_text.lines().collect();
    if chunk.is_empty() || file.is_empty() {
        return None;
    }
    let stored_idx = stored_line_start - 1; // 0-based index the chunk was stored at

    // Stage 1 — exact whole-body match, nearest-to-stored wins.
    let mut best: Option<i64> = None;
    if chunk.len() <= file.len() {
        for start in 0..=(file.len() - chunk.len()) {
            if file[start..start + chunk.len()] == chunk[..] {
                let shift = start as i64 - stored_idx;
                if best.is_none_or(|b: i64| shift.abs() < b.abs()) {
                    best = Some(shift);
                }
            }
        }
    }
    if best.is_some() {
        return best;
    }

    // Stage 2 — distinctive-line offset voting.
    let distinctive: Vec<(usize, &str)> = chunk
        .iter()
        .enumerate()
        .filter_map(|(ci, l)| {
            let t = l.trim();
            is_distinctive(t).then_some((ci, t))
        })
        .collect();
    if distinctive.len() < 3 {
        return None;
    }
    let mut occurrences: HashMap<&str, Vec<usize>> = HashMap::new();
    for (fi, l) in file.iter().enumerate() {
        let t = l.trim();
        if is_distinctive(t) {
            occurrences.entry(t).or_default().push(fi);
        }
    }
    let mut votes: HashMap<i64, usize> = HashMap::new();
    for (ci, t) in &distinctive {
        if let Some(positions) = occurrences.get(t) {
            for &fi in positions {
                let shift = fi as i64 - (stored_idx + *ci as i64);
                *votes.entry(shift).or_default() += 1;
            }
        }
    }
    // Winner = most votes; ties break toward the smallest |shift| (nearest to the
    // stored position). Quorum: the winner must carry ≥ half the distinctive lines.
    let (&shift, &n) = votes
        .iter()
        .max_by(|a, b| a.1.cmp(b.1).then(b.0.abs().cmp(&a.0.abs())))?;
    if n * 2 < distinctive.len() {
        return None;
    }
    Some(shift)
}

/// A line is distinctive enough to vote when, trimmed, it is ≥ 8 chars and has at
/// least one alphanumeric — filters blank lines, lone braces/brackets, and
/// punctuation-only fragments that would vote everywhere.
fn is_distinctive(trimmed: &str) -> bool {
    trimmed.len() >= 8 && trimmed.chars().any(|c| c.is_alphanumeric())
}

// ---------------------------------------------------------------------------
// Gear 3 — bare `find_code` (D1_SCOPE §9). FTS5 MATCH over source_chunks →
// bounded excerpt + path + line range + bm25 rank + the PROVENANCE commit
// (the code↔commit join — the free-ground-truth edge from §7).
// ---------------------------------------------------------------------------

/// One `find_code` hit. SPIKE: the raw query is bound straight to `MATCH` (no
/// sanitize / OR-broaden yet). Productionizing reuses
/// `mcp::fts5::{sanitize_fts5_query, match_count_with_or_fallback}` and wires
/// this as a real MCP tool + calibration entry — deferred until the eyeball
/// proves the lexical code↔commit thread is worth it.
#[derive(Debug)]
pub struct CodeHit {
    /// Absolute root of the repo this chunk lives in. `path` is repo-RELATIVE,
    /// so this is the half that makes a hit openable — and on a multi-repo index
    /// the only thing distinguishing one repo's `src/main.rs` from another's.
    pub repo_path: Option<String>,
    pub path: String,
    pub line_start: i64,
    pub line_end: i64,
    /// D-10.16: exact line where the match sits inside the chunk (the quickfix jump
    /// target), computed the same way as `mcp::query::run_find_code`. Falls back to
    /// `line_start` when the match can't be located.
    pub match_line: i64,
    pub language: Option<String>,
    pub rank: f64,
    pub body: String,
    pub commit_sha: Option<String>,
    pub commit_summary: Option<String>,
    /// Freshness of the returned location vs the live working tree (query-time
    /// stat/hash gate + re-locator — see the freshness-gate section above). When
    /// `Relocated`, the line numbers above are already corrected.
    pub location: LocationStatus,
    /// `Some(n)` only when `location == Relocated`: the chunk moved n lines.
    pub line_shift: Option<i64>,
}

pub async fn find_code(pool: &SqlitePool, query: &str, limit: i64) -> Result<Vec<CodeHit>> {
    find_code_in_repo(pool, query, limit, None).await
}

/// `find_code`, optionally constrained to ONE repo.
///
/// The scope is applied in SQL, not to the returned rows, for the same reason the
/// hook over-fetches: filtering after `LIMIT` means the globally top-ranked N may
/// contain none of the caller's repo, so the narrower and more correct the scope,
/// the likelier the answer is empty.
///
/// Exact equality on the stored root, never a substring. A substring test is what
/// produced the defect this exists to fix — a search scoped to `src/` inside one
/// repo was answered from another repo's `src/`, because every repo has one.
///
/// ⚠️ THE QUERY IS SANITIZED HERE, INSIDE, and that placement is the fix.
/// `sanitize_fts5_query`'s own contract says the repair "lives here and is applied
/// at all of those sites" — and it was, at the CLI arm (`main.rs`) and at every
/// `mcp::query` tool, but NOT at this function, which is the one `hook` calls on
/// every shell search. So a term holding a `.` or a `:` — a filename, a Rust path,
/// a method call, a version string — reached FTS5's bareword grammar raw and came
/// back a syntax error, which the hook's fail-open then swallowed whole. Driving
/// the release binary: `grep -rn std::fs src/` and `grep -rn schema_index.rs src/`
/// each emitted nothing, while `render_object` served 3 hits. Corroborated on a
/// real 30-firing log: ZERO firings carried a term containing `.` or `:`, in a
/// Rust and TypeScript workspace. Sanitizing at the call sites left the most
/// important caller uncovered for as long as it existed; sanitizing in here means
/// no future caller can forget. Double-sanitizing is a no-op — an already-quoted
/// token contains `"` and passes through untouched — so `main.rs` is unaffected.
///
/// NOT broadened, deliberately: `mcp::query` retries an unsatisfiable multi-term
/// query as an OR, but the hook is unsolicited, so widening the caller's question
/// is exactly the thing it must not do. Sanitizing repairs the grammar; broadening
/// would change the question.
pub async fn find_code_in_repo(
    pool: &SqlitePool,
    query: &str,
    limit: i64,
    repo: Option<&str>,
) -> Result<Vec<CodeHit>> {
    // Mirror the proven find_* shape: `FROM search_index s ... bm25(search_index)
    // ... WHERE s.body MATCH ? ORDER BY rank`. LEFT JOIN so a chunk with no
    // resolved provenance still returns (commit fields NULL).
    type Row = (
        Option<String>,
        String,
        i64,
        i64,
        i64,
        Option<String>,
        f64,
        String,
        Option<String>,
        Option<String>,
        String,
        i64,
        String,
    );
    // `match_line` via the same trick as `mcp::query::run_find_code` (D-10.16): an
    // empty-ellipsis `snippet()` is an exact substring of the body, `instr()` locates
    // it, and the newline count to that offset + `line_start` is the match line — all
    // server-side. (This spike `find_code` still returns the full chunk body for the
    // CLI excerpt; the MCP tool is the snippet-bounded one. Unifying the two is a
    // logged follow-up.)
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT repo_path, path, line_start, line_end, \
                CASE WHEN off > 0 \
                     THEN line_start + (length(substr(body, 1, off - 1)) \
                                        - length(replace(substr(body, 1, off - 1), char(10), ''))) \
                     ELSE line_start END AS match_line, \
                language, rank, body, commit_sha, commit_summary, \
                doc_path, doc_mtime, doc_hash \
         FROM ( \
             SELECT sc.repo_path AS repo_path, \
                    sc.path AS path, sc.line_start AS line_start, sc.line_end AS line_end, \
                    sc.language AS language, bm25(search_index) AS rank, sc.body AS body, \
                    ce.commit_hash AS commit_sha, ce.message_summary AS commit_summary, \
                    d.path AS doc_path, d.mtime AS doc_mtime, d.content_hash AS doc_hash, \
                    instr(sc.body, snippet(search_index, 0, '', '', '', 64)) AS off \
             FROM search_index s \
             JOIN source_chunks sc ON sc.id = s.rowid_ref AND s.source_table = 'source_chunks' \
             JOIN documents d ON d.id = sc.document_id \
             LEFT JOIN commit_entries ce ON ce.id = sc.last_commit_id \
             WHERE s.body MATCH ? \
               AND (? IS NULL OR sc.repo_path = ?) \
             ORDER BY rank ASC \
             LIMIT ? \
         )",
    )
    .bind(crate::mcp::sanitize_fts5_query(query))
    .bind(repo)
    .bind(repo)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    // Freshness gate + re-locator per hit, memoized per file (the `documents` row
    // carries the absolute path; `source_chunks.path` is repo-relative and
    // unusable for I/O). Line numbers come back corrected when the chunk moved.
    let mut cache = FreshnessCache::new();
    Ok(rows
        .into_iter()
        .map(|r| {
            let stored = StoredChunk {
                body: &r.7,
                line_start: r.2,
                line_end: r.3,
                match_line: r.4,
            };
            let loc = resolve_location(&mut cache, &r.10, r.11, &r.12, &stored);
            CodeHit {
                repo_path: r.0,
                path: r.1,
                line_start: loc.line_start,
                line_end: loc.line_end,
                match_line: loc.match_line,
                language: r.5,
                rank: r.6,
                body: r.7,
                commit_sha: r.8,
                commit_summary: r.9,
                location: loc.status,
                line_shift: loc.line_shift,
            }
        })
        .collect())
}

/// True when a git-tracked path is already indexed by another corpus, so the
/// source extractor must leave it alone (the one-corpus-per-file guard). Mirrors
/// the coverage of the design-doc extractor (`anchor/docs/**/*.md`) and the
/// session extractor (`CLAUDE.md`). `rel` is repo-relative, `/`-separated (the
/// shape `git ls-files` emits).
fn is_owned_by_other_corpus(rel: &str) -> bool {
    // Session: the per-project CLAUDE.md (at repo root, or any nested anchor).
    if rel == "CLAUDE.md" || rel.ends_with("/CLAUDE.md") {
        return true;
    }
    // Design docs: any markdown under a top-level `docs/` tree (the design-doc
    // extractor roots at `anchor/docs` and walks `**/*.md`).
    if rel.starts_with("docs/")
        && let Some(ext) = Path::new(rel).extension().and_then(|s| s.to_str())
        && matches!(ext, "md" | "markdown")
    {
        return true;
    }
    // Design docs: ROOT-LEVEL markdown (no path separator — directly under the
    // repo/anchor root). The design-doc extractor now claims `$ANCHOR/*.md`
    // (BUG_TRIAGE.md et al.; see `indexer::list_root_markdown`), so source must
    // yield them or the file double-indexes under two corpora — the same
    // one-corpus-per-file rule as `docs/` above (2026-07-09). CLAUDE.md is already
    // covered by the session guard; a non-root `sub/dir/notes.md` stays source.
    if !rel.contains('/')
        && let Some(ext) = Path::new(rel).extension().and_then(|s| s.to_str())
        && matches!(ext, "md" | "markdown")
    {
        return true;
    }
    false
}

/// Best-effort language tag from extension. `None` (NULL) for unrecognized — the
/// chunk is still indexed; language is a filter/fingerprint dimension (D1_SCOPE §7).
fn language_of(rel: &str) -> Option<&'static str> {
    let ext = Path::new(rel).extension().and_then(|s| s.to_str())?;
    Some(match ext {
        "rs" => "rust",
        "md" | "markdown" => "markdown",
        "sql" => "sql",
        "toml" => "toml",
        "yml" | "yaml" => "yaml",
        "json" => "json",
        "sh" | "bash" => "shell",
        "js" | "mjs" | "cjs" => "javascript",
        "ts" => "typescript",
        "tsx" => "typescriptreact",
        "jsx" => "javascriptreact",
        "py" => "python",
        "html" => "html",
        "css" => "css",
        _ => return None,
    })
}

/// Render one `find_code` hit as a single `path:line:excerpt` line for
/// NeoVim's quickfix (`errorformat=%f:%l:%m`): `%f`=path, `%l`=line,
/// `%m`=everything after the 2nd colon. `%l` is `match_line` (D-10.16) so the
/// editor jumps to the matched passage, not the chunk start. The message is the
/// first non-blank body line + the provenance commit, collapsed to one line.
/// A non-Verified location appends an honesty marker to `%m` — the jump may
/// land off-target, and the consumer deserves to know before trusting it.
pub fn grep_line(hit: &CodeHit) -> String {
    let first = hit
        .body
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    let lang = hit.language.as_deref().unwrap_or("?");
    let via = match (&hit.commit_sha, &hit.commit_summary) {
        (Some(sha), Some(msg)) => format!("  (via {} {})", &sha[..sha.len().min(8)], msg),
        _ => String::new(),
    };
    let flag = match hit.location {
        LocationStatus::Verified => String::new(),
        LocationStatus::Relocated => {
            format!("  [moved {:+}]", hit.line_shift.unwrap_or(0))
        }
        LocationStatus::Stale => "  [stale: file changed since index]".to_string(),
        LocationStatus::FileMissing => "  [missing: file gone]".to_string(),
    };
    // gnuphie-labs#8: `path` is repo-RELATIVE, so emitting it alone hands the
    // consumer a string that only resolves if they happen to be in the right
    // directory — and on a multi-repo index several repos have a `src/main.rs`.
    // Anchor it: a quickfix `%f` takes an absolute path happily, and an absolute
    // path cannot resolve against the wrong repo. Falls back to the bare relative
    // path only when the repo root is genuinely unknown.
    let located = match &hit.repo_path {
        Some(root) => format!("{}/{}", root.trim_end_matches('/'), hit.path),
        None => hit.path.clone(),
    };
    format!("{}:{}:[{}] {}{}{}", located, hit.match_line, lang, first, via, flag)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn other_corpus_owns_design_docs_and_claude_md() {
        // Design docs (any markdown under docs/) — owned by the design_doc corpus.
        assert!(is_owned_by_other_corpus("docs/DESIGN.md"));
        assert!(is_owned_by_other_corpus("docs/design/D1_SCOPE.md"));
        assert!(is_owned_by_other_corpus("docs/plans/flip.markdown"));
        // Session CLAUDE.md (root or nested anchor) — owned by the session corpus.
        assert!(is_owned_by_other_corpus("CLAUDE.md"));
        assert!(is_owned_by_other_corpus("subproject/CLAUDE.md"));
        // Root-level markdown — now owned by the design_doc corpus (2026-07-09),
        // so the source corpus yields it (no double-index). The TODO/bug tracker
        // is the load-bearing case; README/CHANGELOG ride along.
        assert!(is_owned_by_other_corpus("BUG_TRIAGE.md"));
        assert!(is_owned_by_other_corpus("README.md"));
        assert!(is_owned_by_other_corpus("TODO.markdown"));
    }

    #[test]
    fn other_corpus_leaves_code_and_nested_markdown_to_source() {
        // Code is the source corpus's job.
        assert!(!is_owned_by_other_corpus("src/source_index.rs"));
        assert!(!is_owned_by_other_corpus("migrations/0001_init.sql"));
        // Non-markdown under docs/ (e.g. an asset) is still source.
        assert!(!is_owned_by_other_corpus("docs/diagram.svg"));
        // Non-markdown at the root is still source.
        assert!(!is_owned_by_other_corpus("Cargo.toml"));
        // A `docs` substring that isn't the top-level docs/ tree stays source, and
        // markdown NESTED under a non-docs subdir is not root-level → still source
        // (only depth-1 root markdown + docs/** are design-doc owned).
        assert!(!is_owned_by_other_corpus("src/docs_gen/template.md"));
        assert!(!is_owned_by_other_corpus("subdir/notes.md"));
    }

    /// A repo-scoped search must not be answered from a different repo.
    ///
    /// The live failure this gates: `grep -rn AppState src/` inside one repo came
    /// back with another project's `backend/src/handlers/`, because the hook
    /// honours the caller's scope as a substring of a repo-RELATIVE path and every
    /// repo has a `src/`. The answer arrived with provenance and an "index
    /// current" stamp — well-formed, well-labelled, wrong codebase — and nothing
    /// prompted the caller to doubt it, because they had not asked nibdex anything.
    #[tokio::test]
    async fn a_repo_scoped_search_never_answers_from_another_repo() {
        let (_tmp, pool) = fresh_pool().await;
        for (repo, rel, body) in [
            ("/ws/alpha", "src/main.rs", "struct AppState { alpha_field: u8 }"),
            ("/ws/beta", "src/main.rs", "struct AppState { beta_field: u8 }"),
        ] {
            let doc: (i64,) = sqlx::query_as(
                "INSERT INTO documents (path, kind, content_hash, mtime, indexed_at) \
                 VALUES (?, 'source', 'h', 0, 0) RETURNING id",
            )
            .bind(format!("{repo}/{rel}"))
            .fetch_one(&pool)
            .await
            .unwrap();
            let chunk: (i64,) = sqlx::query_as(
                "INSERT INTO source_chunks \
                   (document_id, repo_path, path, line_start, line_end, language, body) \
                 VALUES (?, ?, ?, 1, 50, 'rust', ?) RETURNING id",
            )
            .bind(doc.0).bind(repo).bind(rel).bind(body)
            .fetch_one(&pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO search_index (body, kind, rowid_ref, source_table) \
                 VALUES (?, 'source', ?, 'source_chunks')",
            )
            .bind(body).bind(chunk.0)
            .execute(&pool)
            .await
            .unwrap();
        }

        // Unscoped: both repos answer, and each hit says which one it is.
        let all = find_code(&pool, "AppState", 10).await.unwrap();
        assert_eq!(all.len(), 2, "both repos define AppState");
        assert!(
            all.iter().all(|h| h.repo_path.is_some()),
            "every hit must carry the repo that makes it openable"
        );

        // Scoped: ONLY the named repo, even though the other has an identical
        // relative path and an equally good rank.
        let scoped = find_code_in_repo(&pool, "AppState", 10, Some("/ws/beta")).await.unwrap();
        assert_eq!(scoped.len(), 1, "the other repo's identical path must not answer");
        assert_eq!(scoped[0].repo_path.as_deref(), Some("/ws/beta"));
        assert!(scoped[0].body.contains("beta_field"), "returned beta's code, not alpha's");

        // An unknown repo yields nothing rather than falling back to everything —
        // silence beats a confident answer from the wrong tree.
        let none = find_code_in_repo(&pool, "AppState", 10, Some("/ws/nope")).await.unwrap();
        assert!(none.is_empty(), "an unmatched scope must not degrade to unscoped");
    }

    #[test]
    fn grep_line_is_quickfix_shaped() {
        let hit = CodeHit {
            repo_path: Some("/ws/proj".into()),
            path: "src/rescore.rs".into(),
            line_start: 42,
            line_end: 50,
            match_line: 45,
            language: Some("rust".into()),
            rank: -3.5,
            body: "\n\n    fn compute() {}\n".into(),
            commit_sha: Some("5de5a45abcdef".into()),
            commit_summary: Some("v0.2 honest savings".into()),
            location: LocationStatus::Verified,
            line_shift: None,
        };
        // `%l` is match_line (45), the jump target — not the chunk start (42).
        // Verified adds NO marker — the common (clean-tree) case stays noise-free.
        assert_eq!(
            grep_line(&hit),
            "/ws/proj/src/rescore.rs:45:[rust] fn compute() {}  (via 5de5a45a v0.2 honest savings)"
        );
    }

    #[test]
    fn grep_line_flags_non_verified_locations() {
        let mut hit = CodeHit {
            repo_path: Some("/ws/proj".into()),
            path: "src/a.rs".into(),
            line_start: 1,
            line_end: 50,
            match_line: 7,
            language: Some("rust".into()),
            rank: -1.0,
            body: "fn x() {}".into(),
            commit_sha: None,
            commit_summary: None,
            location: LocationStatus::Stale,
            line_shift: None,
        };
        // The marker rides in `%m` (after the 2nd colon) so `%f:%l` stays parseable.
        assert_eq!(
            grep_line(&hit),
            "/ws/proj/src/a.rs:7:[rust] fn x() {}  [stale: file changed since index]"
        );
        hit.location = LocationStatus::FileMissing;
        assert_eq!(grep_line(&hit), "/ws/proj/src/a.rs:7:[rust] fn x() {}  [missing: file gone]");
        // Relocated: the line numbers are already corrected; the marker just says so.
        hit.location = LocationStatus::Relocated;
        hit.line_shift = Some(30);
        assert_eq!(grep_line(&hit), "/ws/proj/src/a.rs:7:[rust] fn x() {}  [moved +30]");

        // gnuphie-labs#8: with no known repo root there is nothing to anchor to,
        // so the bare relative path is the honest output rather than a guess.
        hit.repo_path = None;
        assert_eq!(grep_line(&hit), "src/a.rs:7:[rust] fn x() {}  [moved +30]");
    }

    #[test]
    fn freshness_gate_verified_missing_and_memo() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lib.rs");
        std::fs::write(&path, "fn live() {}\n").unwrap();
        let real_mtime = std::fs::metadata(&path)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let mut hasher = Sha256::new();
        hasher.update(b"fn live() {}\n");
        let real_hash = format!("{:x}", hasher.finalize());
        let abs = path.to_string_lossy().into_owned();

        // mtime equal → Verified on the stat alone, stored range untouched.
        let mut cache = FreshnessCache::new();
        let one = StoredChunk { body: "fn live() {}", line_start: 1, line_end: 1, match_line: 1 };
        let loc = resolve_location(&mut cache, &abs, real_mtime, "ignored", &one);
        assert_eq!(loc.status, LocationStatus::Verified);
        assert_eq!((loc.line_start, loc.line_end, loc.match_line, loc.line_shift), (1, 1, 1, None));
        // mtime moved but content identical (a touch) → hash-confirm → Verified.
        let mut cache = FreshnessCache::new();
        let loc = resolve_location(&mut cache, &abs, real_mtime + 7, &real_hash, &one);
        assert_eq!(loc.status, LocationStatus::Verified);
        // No such file → FileMissing, stored range returned (the best prior).
        let gone = dir.path().join("gone.rs").to_string_lossy().into_owned();
        let mut cache = FreshnessCache::new();
        let chunk345 = StoredChunk { body: "fn x() {}", line_start: 3, line_end: 5, match_line: 4 };
        let loc = resolve_location(&mut cache, &gone, real_mtime, &real_hash, &chunk345);
        assert_eq!(loc.status, LocationStatus::FileMissing);
        assert_eq!((loc.line_start, loc.line_end, loc.match_line), (3, 5, 4));
        // Memo: the second call must come from the cache (delete the file between
        // calls; the verdict must not change within one query).
        let mut cache = FreshnessCache::new();
        let first = resolve_location(&mut cache, &abs, real_mtime, "ignored", &one);
        std::fs::remove_file(&path).unwrap();
        let second = resolve_location(&mut cache, &abs, real_mtime, "ignored", &one);
        assert_eq!(first.status, second.status);
    }

    #[test]
    fn freshness_gate_relocates_on_changed_file() {
        // The end-to-end Changed path: live file = 5 inserted lines + the stored
        // chunk verbatim → Relocated with shift +5 and all three lines corrected.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("moved.rs");
        let stored_body = "fn alpha_one() {}\nfn beta_two() {}\nfn gamma_three() {}";
        let live = format!("{}{}", "// filler\n".repeat(5), stored_body);
        std::fs::write(&path, &live).unwrap();
        let real_mtime = std::fs::metadata(&path)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let abs = path.to_string_lossy().into_owned();
        let mut cache = FreshnessCache::new();
        // stored mtime/hash represent index time: both differ from the live file.
        let stored =
            StoredChunk { body: stored_body, line_start: 1, line_end: 3, match_line: 2 };
        let loc = resolve_location(&mut cache, &abs, real_mtime - 100, "0ddhash", &stored);
        assert_eq!(loc.status, LocationStatus::Relocated);
        assert_eq!(loc.line_shift, Some(5));
        assert_eq!((loc.line_start, loc.line_end, loc.match_line), (6, 8, 7));
    }

    // -- relocate_chunk: the pure re-anchoring core ---------------------------------

    const CHUNK: &str = "fn resolve_provenance() {\n    let author = oldest_code();\n    let feeders = prose_before(author);\n    summarize(author, feeders)\n}";

    #[test]
    fn relocate_finds_chunk_at_stored_position() {
        // Unchanged position (file changed elsewhere) → shift 0 via stage 1.
        let file = format!("// head\n{CHUNK}\n// tail");
        assert_eq!(relocate_chunk(CHUNK, 2, &file), Some(0));
    }

    #[test]
    fn relocate_resolves_insert_above_and_delete_above() {
        // The dominant real case (the §9.1 gear's 3/3): K lines inserted above.
        let file = format!("{}{CHUNK}", "// inserted\n".repeat(30));
        assert_eq!(relocate_chunk(CHUNK, 1, &file), Some(30));
        // And the negative twin: lines REMOVED above the stored position.
        assert_eq!(relocate_chunk(CHUNK, 31, CHUNK), Some(-30));
    }

    #[test]
    fn relocate_interior_edit_resolves_by_quorum() {
        // One line edited INSIDE the chunk → whole-body match fails, but the
        // unedited distinctive majority still votes the offset coherently.
        let edited = CHUNK.replace("prose_before(author)", "prose_before(author, idf)");
        let file = format!("{}{edited}", "// shift\n".repeat(4));
        assert_eq!(relocate_chunk(CHUNK, 1, &file), Some(4));
    }

    #[test]
    fn relocate_lost_when_content_deleted() {
        let file = "fn unrelated() {}\nfn other() {}\nfn third_thing() {}";
        assert_eq!(relocate_chunk(CHUNK, 1, file), None);
    }

    #[test]
    fn relocate_duplicate_windows_tie_break_nearest_to_stored() {
        // The same boilerplate window appears twice; the one nearest the stored
        // position wins (here: stored at line 8 → the copy at index 7, not 0).
        let dup = "fn a_repeated() {}\nfn b_repeated() {}";
        let file = format!("{dup}\n// gap\n// gap\n// gap\n// gap\n// gap\n{dup}");
        // Stored line_start=8 (0-based 7): the second copy sits at index 7 → shift 0
        // beats the first copy's −7. Stored at 1: the first copy (shift 0) beats +7.
        assert_eq!(relocate_chunk(dup, 8, &file), Some(0));
        assert_eq!(relocate_chunk(dup, 1, &file), Some(0));
    }

    #[test]
    fn relocate_no_false_quorum_from_brace_soup() {
        // A chunk of braces/blanks has < 3 distinctive lines → stage-1-only; when
        // its content changed it must come back None (Stale), not a guess.
        let braces = "}\n{\n}\n\n{";
        let file = "fn totally_different() {\n    body();\n}";
        assert_eq!(relocate_chunk(braces, 1, file), None);
    }

    #[test]
    fn relocate_clamps_handled_by_resolver_not_panicking_on_short_file() {
        // File now SHORTER than the stored range: stage 1 can't fit the window,
        // stage 2 finds no quorum → None; and a partial survivor still resolves.
        assert_eq!(relocate_chunk(CHUNK, 40, "fn tiny() {}"), None);
    }

    // -- RC1 review 1.10: the relocator's guards, each with a mutation it catches --

    #[test]
    fn relocate_below_quorum_is_stale_not_a_guess() {
        // CHUNK has 4 distinctive lines; the live file keeps only ONE of them
        // (1/4 < 50%) at a new offset. Stage 1 cannot match; stage 2 must refuse
        // the single vote. Mutation caught: dropping the `n * 2 < len` quorum
        // check → Some(1).
        let file = "// x\n// y\n    let author = oldest_code();\n// z";
        assert_eq!(relocate_chunk(CHUNK, 2, file), None);
    }

    #[test]
    fn relocate_short_lines_do_not_vote() {
        // Every line is < 8 chars, so none is distinctive; a torn copy of the
        // chunk (one line changed, so stage 1 fails) shifted by 2 must NOT be
        // relocated by three short-line votes. Mutation caught: `is_distinctive`
        // threshold 8 → 1 gives 3/4 votes for shift 2 → Some(2).
        let chunk = "a = 1;\nb = 2;\nc = 3;\nd = 4;";
        let file = "// p\n// q\na = 1;\nb = 2;\nX\nd = 4;";
        assert_eq!(relocate_chunk(chunk, 1, file), None);
        assert!(!is_distinctive("a = 1;"));
        assert!(is_distinctive("let x = 1;"));
        assert!(!is_distinctive("--------"), "punctuation-only never votes");
    }

    #[test]
    fn freshness_gate_clamps_relocated_range_to_the_live_file() {
        // The stored range (1..=6) is longer than the live file allows after a
        // +2 shift: a 6-line file cannot host lines 3..=8. `line_end` clamps to
        // the file length and `match_line` stays inside the range. Mutation
        // caught: removing the `.min(file_lines)` clamp → line_end 8.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("short.rs");
        let stored_body = "fn alpha_one() {}\nfn beta_two() {}\nfn gamma_three() {}";
        let live = format!("{}{}\n// tail", "// filler\n".repeat(2), stored_body);
        std::fs::write(&path, &live).unwrap(); // 6 lines: 2 filler + 3 chunk + tail
        let real_mtime = std::fs::metadata(&path)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let abs = path.to_string_lossy().into_owned();
        let mut cache = FreshnessCache::new();
        let stored = StoredChunk { body: stored_body, line_start: 1, line_end: 6, match_line: 6 };
        let loc = resolve_location(&mut cache, &abs, real_mtime - 100, "0ddhash", &stored);
        assert_eq!(loc.status, LocationStatus::Relocated);
        assert_eq!(loc.line_shift, Some(2));
        assert_eq!(loc.line_start, 3);
        assert_eq!(loc.line_end, 6, "clamped to the 6-line file, not 8");
        assert!(loc.match_line >= loc.line_start && loc.match_line <= loc.line_end);
        assert_eq!(loc.match_line, 6);
    }

    // -- the write-amp fix: content-hash skip + provenance-only refresh -------------

    async fn fresh_pool() -> (tempfile::TempDir, SqlitePool) {
        let tmp = tempfile::TempDir::new().unwrap();
        let pool = crate::db::open(&tmp.path().join("nibdex.db")).await.unwrap();
        (tmp, pool)
    }

    /// git2 repo fixture (the watcher tests' mold): init + author config + a commit.
    fn init_repo(path: &Path) -> git2::Repository {
        let repo = git2::Repository::init(path).unwrap();
        {
            let mut cfg = repo.config().unwrap();
            cfg.set_str("user.email", "test@nibdex.invalid").unwrap();
            cfg.set_str("user.name", "nibdex tests").unwrap();
        }
        repo
    }

    fn commit_file(repo: &git2::Repository, name: &str, body: &str, msg: &str) {
        let workdir = repo.workdir().unwrap().to_path_buf();
        std::fs::write(workdir.join(name), body).unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new(name)).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let sig = repo.signature().unwrap();
        let parents: Vec<git2::Commit> = match repo.head() {
            Ok(head) => vec![head.peel_to_commit().unwrap()],
            Err(_) => Vec::new(),
        };
        let parent_refs: Vec<&git2::Commit> = parents.iter().collect();
        repo.commit(Some("HEAD"), &sig, &sig, msg, &tree, &parent_refs).unwrap();
    }

    /// Like `commit_file` but writes raw bytes — for binary / non-UTF-8 fixtures.
    fn commit_bytes(repo: &git2::Repository, name: &str, body: &[u8], msg: &str) {
        let workdir = repo.workdir().unwrap().to_path_buf();
        std::fs::write(workdir.join(name), body).unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new(name)).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let sig = repo.signature().unwrap();
        let parents: Vec<git2::Commit> = match repo.head() {
            Ok(head) => vec![head.peel_to_commit().unwrap()],
            Err(_) => Vec::new(),
        };
        let parent_refs: Vec<&git2::Commit> = parents.iter().collect();
        repo.commit(Some("HEAD"), &sig, &sig, msg, &tree, &parent_refs).unwrap();
    }

    async fn chunk_ids(pool: &SqlitePool) -> Vec<i64> {
        sqlx::query_scalar("SELECT id FROM source_chunks ORDER BY id")
            .fetch_all(pool)
            .await
            .unwrap()
    }

    /// A term holding `.` or `::` must reach FTS5 SANITIZED — the shapes a
    /// developer actually greps for.
    ///
    /// This function is the hook's query path, and it bound the raw term to
    /// `body MATCH ?` while every other caller sanitized. FTS5's bareword grammar
    /// then rejected the query outright, the hook's fail-open swallowed the error,
    /// and the search silently produced nothing. The mutation that proves this
    /// test: drop the `sanitize_fts5_query` call and every `is_ok()` below fails
    /// with a syntax error, which is exactly what shipped.
    #[tokio::test]
    async fn a_dotted_or_colon_term_reaches_fts5_sanitized() {
        let (_tmp, pool) = fresh_pool().await;
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path());
        commit_file(&repo, "a.rs", "use std::fs;\nfn read_schema_index() {}\n", "add a");
        index_source_repo(&pool, dir.path()).await.unwrap();

        for term in ["std::fs", "schema_index.rs", "v0.2.0-rc.3", "acme-dashboard"] {
            let hits = find_code(&pool, term, 10).await;
            assert!(hits.is_ok(), "FTS5 rejected {term:?}: {:?}", hits.err());
        }
        // Surviving is not enough — the repaired query must still ANSWER.
        let hits = find_code(&pool, "std::fs", 10).await.unwrap();
        assert!(!hits.is_empty(), "sanitized `std::fs` must still find the line it is on");
    }

    /// The extractor must actually STAMP the repo on every chunk it writes.
    ///
    /// The query-layer repo-scope tests seed `source_chunks` directly, so all of
    /// them would still pass if this bound NULL — the write path needs its own
    /// gate or the scope silently degrades to "matches nothing" on a real index.
    /// Also pins the value to the same string the commits extractor stores, since
    /// one `repo` argument is documented to scope code and commits alike.
    #[tokio::test]
    async fn every_indexed_chunk_is_stamped_with_its_repo() {
        let (_tmp, pool) = fresh_pool().await;
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path());
        commit_file(&repo, "a.rs", "fn alpha_entry() {\n    alpha_body();\n}\n", "add a");

        let stats = index_source_repo(&pool, dir.path()).await.unwrap();
        assert!(stats.chunks > 0, "the fixture must actually produce chunks");

        let unstamped: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM source_chunks WHERE repo_path IS NULL")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(unstamped, 0, "every chunk written by the extractor carries its repo");

        let stored: Vec<String> =
            sqlx::query_scalar("SELECT DISTINCT repo_path FROM source_chunks")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(
            stored,
            vec![dir.path().to_string_lossy().into_owned()],
            "the stamp must equal the repo root the commits extractor also stores"
        );
    }

    /// The headline write-amp behavior: a reindex over an UNCHANGED corpus rewrites
    /// nothing — chunk rowids (and FTS rows) survive byte-identical.
    #[tokio::test]
    async fn reindex_skips_unchanged_files_keeping_chunk_rows() {
        let (_tmp, pool) = fresh_pool().await;
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path());
        commit_file(&repo, "a.rs", "fn alpha_entry() {\n    alpha_body();\n}\n", "add a");
        commit_file(&repo, "b.rs", "fn beta_entry() {\n    beta_body();\n}\n", "add b");

        let first = index_source_repo(&pool, dir.path()).await.unwrap();
        assert_eq!(first.files_indexed, 2);
        assert_eq!(first.files_unchanged, 0);
        let ids_before = chunk_ids(&pool).await;
        assert!(!ids_before.is_empty());

        // The echo-fire / docs-commit case: nothing changed → nothing rewritten.
        let second = index_source_repo(&pool, dir.path()).await.unwrap();
        assert_eq!(second.files_indexed, 0, "no file rewritten");
        assert_eq!(second.files_unchanged, 2);
        assert_eq!(second.chunks, 0);
        assert_eq!(chunk_ids(&pool).await, ids_before, "chunk rows untouched");
    }

    /// A commit touching ONE file rewrites only that file's chunks; the rest skip.
    #[tokio::test]
    async fn reindex_rewrites_only_the_changed_file() {
        let (_tmp, pool) = fresh_pool().await;
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path());
        commit_file(&repo, "a.rs", "fn alpha_entry() {\n    alpha_body();\n}\n", "add a");
        commit_file(&repo, "b.rs", "fn beta_entry() {\n    beta_body();\n}\n", "add b");
        index_source_repo(&pool, dir.path()).await.unwrap();
        let b_ids_before: Vec<i64> = sqlx::query_scalar(
            "SELECT id FROM source_chunks WHERE path = 'b.rs' ORDER BY id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();

        commit_file(&repo, "a.rs", "fn alpha_entry() {\n    alpha_v2();\n}\n", "change a");
        let stats = index_source_repo(&pool, dir.path()).await.unwrap();
        assert_eq!(stats.files_indexed, 1, "only a.rs rewritten");
        assert_eq!(stats.files_unchanged, 1, "b.rs skipped");
        let b_ids_after: Vec<i64> = sqlx::query_scalar(
            "SELECT id FROM source_chunks WHERE path = 'b.rs' ORDER BY id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(b_ids_after, b_ids_before, "untouched file's rows survive");
        // And the rewritten file's body is the new content.
        let a_body: String =
            sqlx::query_scalar("SELECT body FROM source_chunks WHERE path = 'a.rs'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(a_body.contains("alpha_v2"));
    }

    /// A file that leaves the git index (deleted + committed) leaves the corpus
    /// on the next pass: its document, chunks, and FTS rows are removed and the
    /// pass reports it as pruned. Before this the corpus only grew, so a deleted
    /// file kept answering `find_code` (as `file_missing`) forever while
    /// `check()` reported it healthy (RC1 review 1.6). Mutation this catches:
    /// removing the prune loop leaves 2 documents / 2 FTS rows and `pruned == 0`.
    #[tokio::test]
    async fn reindex_prunes_files_no_longer_tracked() {
        let (_tmp, pool) = fresh_pool().await;
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path());
        commit_file(&repo, "keep.rs", "fn keep_entry() {\n    keep_body();\n}\n", "add keep");
        commit_file(&repo, "gone.rs", "fn gone_entry() {\n    gone_body();\n}\n", "add gone");
        index_source_repo(&pool, dir.path()).await.unwrap();
        let docs_before: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM documents WHERE kind = 'source'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(docs_before, 2);

        // Delete gone.rs from the tree AND the git index, then commit.
        std::fs::remove_file(dir.path().join("gone.rs")).unwrap();
        {
            let mut index = repo.index().unwrap();
            index.remove_path(Path::new("gone.rs")).unwrap();
            index.write().unwrap();
            let tree_id = index.write_tree().unwrap();
            let tree = repo.find_tree(tree_id).unwrap();
            let sig = repo.signature().unwrap();
            let head = repo.head().unwrap().peel_to_commit().unwrap();
            repo.commit(Some("HEAD"), &sig, &sig, "rm gone", &tree, &[&head]).unwrap();
        }
        let stats = index_source_repo(&pool, dir.path()).await.unwrap();
        assert_eq!(stats.files_pruned, 1, "gone.rs must be reported as pruned");

        let docs_after: Vec<String> =
            sqlx::query_scalar("SELECT path FROM documents WHERE kind = 'source' ORDER BY path")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(docs_after.len(), 1);
        assert!(docs_after[0].ends_with("keep.rs"), "only keep.rs survives: {docs_after:?}");
        let gone_chunks: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM source_chunks WHERE path = 'gone.rs'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(gone_chunks, 0, "chunks of the pruned file are gone");
        let fts_hits: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM search_index WHERE source_table = 'source_chunks' AND body MATCH 'gone_body'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(fts_hits, 0, "FTS rows of the pruned file are gone");
    }

    /// The container repo's prune pass must not touch a NESTED repo's documents
    /// (workspace root that is itself a repo + `--include-nested-repos`). Seen
    /// live on the rc.2 deploy: 268 nested docs pruned and rewritten every pass.
    /// Mutation this catches: dropping the `belongs_to_nested_repo` guard →
    /// the nested doc is gone after the container's pass (files_pruned == 1).
    #[tokio::test]
    async fn container_prune_leaves_nested_repo_documents_alone() {
        let (_tmp, pool) = fresh_pool().await;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let outer = init_repo(&root);
        commit_file(&outer, "top.rs", "fn top_entry() {}\n", "outer");
        let inner_dir = root.join("inner");
        std::fs::create_dir_all(&inner_dir).unwrap();
        let inner = init_repo(&inner_dir);
        commit_file(&inner, "deep.rs", "fn deep_entry() {}\n", "inner");

        // Index the nested repo first, then the container (the order the
        // discovery sort produces is container-first, but either order must hold).
        index_source_repo(&pool, &inner_dir).await.unwrap();
        let s = index_source_repo(&pool, &root).await.unwrap();
        assert_eq!(s.files_pruned, 0, "the container must not prune inner/deep.rs");
        let docs: Vec<String> =
            sqlx::query_scalar("SELECT path FROM documents WHERE kind = 'source' ORDER BY path")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(docs.len(), 2, "{docs:?}");
        assert!(docs.iter().any(|p| p.ends_with("inner/deep.rs")));
        // And a second container pass is a no-op rewrite-wise.
        let s2 = index_source_repo(&pool, &root).await.unwrap();
        assert_eq!((s2.files_pruned, s2.files_indexed), (0, 0));
        assert_eq!(s2.files_unchanged, 1);
    }

    /// Binary and non-UTF-8 files are skipped, not lossily indexed: a NUL-bearing
    /// blob (the classic sniff) AND a NUL-free but invalid-UTF-8 file (the gap the
    /// null-byte check alone misses) both count as `skipped_binary`; only the real
    /// text file lands in the index.
    #[tokio::test]
    async fn skips_binary_and_non_utf8_files() {
        let (_tmp, pool) = fresh_pool().await;
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path());
        commit_file(&repo, "real.rs", "fn keeper() {\n    body();\n}\n", "add text");
        // NUL byte → classic binary sniff.
        commit_bytes(&repo, "blob.bin", b"MZ\x00\x00\x90\x00binary", "add nul blob");
        // NUL-free but invalid UTF-8 (lone 0xFF / bare continuation byte) → would
        // otherwise decode to replacement-char mojibake under from_utf8_lossy.
        commit_bytes(&repo, "latin.dat", b"caf\xe9 na\xefve \xff data", "add non-utf8");

        let stats = index_source_repo(&pool, dir.path()).await.unwrap();
        assert_eq!(stats.files_indexed, 1, "only the real text file is indexed");
        assert_eq!(stats.skipped_binary, 2, "both the blob and the non-utf8 file");

        // The index holds the text file and nothing decoded from the others.
        let paths: Vec<String> =
            sqlx::query_scalar("SELECT DISTINCT path FROM source_chunks ORDER BY path")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(paths, vec!["real.rs".to_string()]);
        // No replacement char (U+FFFD) leaked into any indexed body.
        let bodies: Vec<String> = sqlx::query_scalar("SELECT body FROM source_chunks")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert!(bodies.iter().all(|b| !b.contains('\u{FFFD}')));
    }

    /// The provenance-only edge: a later commit reverts a file to byte-identical
    /// content. Hash matches (skip the FTS rewrite) but the code↔commit join must
    /// still advance to the newest touching commit — refreshed in place.
    #[tokio::test]
    async fn reindex_refreshes_provenance_without_chunk_rewrite() {
        let (_tmp, pool) = fresh_pool().await;
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path());
        let original = "fn gamma_entry() {\n    gamma_body();\n}\n";
        commit_file(&repo, "g.rs", original, "add g");
        // Commits corpus first (the watcher's order) so provenance resolves.
        crate::indexer::reindex_commits_for_repo(&pool, dir.path(), 50_000).await.unwrap();
        index_source_repo(&pool, dir.path()).await.unwrap();
        let (ids_before, prov_before): (Vec<i64>, Option<i64>) = (
            chunk_ids(&pool).await,
            sqlx::query_scalar("SELECT last_commit_id FROM source_chunks WHERE path = 'g.rs'")
                .fetch_one(&pool)
                .await
                .unwrap(),
        );
        assert!(prov_before.is_some(), "provenance resolved on first index");

        // Change then REVERT to byte-identical content — two new commits.
        commit_file(&repo, "g.rs", "fn gamma_entry() {\n    gamma_v2();\n}\n", "change g");
        commit_file(&repo, "g.rs", original, "revert g");
        crate::indexer::reindex_commits_for_repo(&pool, dir.path(), 50_000).await.unwrap();
        let stats = index_source_repo(&pool, dir.path()).await.unwrap();

        assert_eq!(stats.files_unchanged, 1, "hash matches the stored state");
        assert_eq!(chunk_ids(&pool).await, ids_before, "no chunk rewrite");
        let prov_after: Option<i64> =
            sqlx::query_scalar("SELECT last_commit_id FROM source_chunks WHERE path = 'g.rs'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(prov_after.is_some());
        assert_ne!(prov_after, prov_before, "join advanced to the revert commit");
    }

    // -- git2 provenance walk (GIT2_MIGRATION_PLAN step 4) --------------------------

    #[test]
    fn provenance_map_keeps_most_recent_touch_and_lists_root_files() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = init_repo(tmp.path());
        commit_file(&repo, "a.rs", "v1", "c1 add a"); // root commit → lists a.rs
        let c1 = repo.head().unwrap().peel_to_commit().unwrap().id().to_string();
        commit_file(&repo, "b.rs", "v1", "c2 add b");
        let c2 = repo.head().unwrap().peel_to_commit().unwrap().id().to_string();
        commit_file(&repo, "a.rs", "v2", "c3 edit a");
        let c3 = repo.head().unwrap().peel_to_commit().unwrap().id().to_string();

        let map = build_provenance_map(&repo).unwrap();
        // a.rs was touched by the root (c1) and again by c3 → most-recent (c3) wins.
        // (c1/c3 may share a wall-clock second; TOPOLOGICAL ordering still puts the
        // child c3 first, so first-seen is c3 — the reason TIME alone isn't enough.)
        assert_eq!(map.get("a.rs"), Some(&c3));
        // b.rs was only ever touched by c2.
        assert_eq!(map.get("b.rs"), Some(&c2));
        // c1's a.rs is superseded → the root commit now owns no path.
        assert!(!map.values().any(|h| h == &c1));
    }

    #[test]
    fn repo_has_commits_and_provenance_empty_on_unborn_head() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = init_repo(tmp.path());
        assert!(!repo_has_commits(&repo), "unborn HEAD → no commits");
        assert!(
            build_provenance_map(&repo).unwrap().is_empty(),
            "no history → empty provenance"
        );
        commit_file(&repo, "a.rs", "v1", "first");
        assert!(repo_has_commits(&repo), "HEAD resolves after first commit");
    }
}
