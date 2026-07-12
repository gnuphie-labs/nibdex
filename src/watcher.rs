// SPDX-License-Identifier: MIT

//! File-watcher daemon — DESIGN §5.3 + design-pass D-6.2 kill-path semantics.
//!
//! Drives `notify-debouncer-mini` over the three Day-6 corpus subscriptions
//! (CLAUDE.md / memory dir / design-doc dir) with the locked 500ms debounce
//! window. Events drain into an async task that dispatches per-event into the
//! `indexer::reindex_*` / `delete_document_by_path` helpers. The singleton
//! `file_watcher_state` row carries runtime stats so a separate `nibdex mcp`
//! invocation can surface them via `check().file_watcher`.
//!
//! D-6.4.1: stdio MCP mode does NOT spawn this watcher (process dies at session
//! end). The watcher lives behind `nibdex watch` (this commit) and graduates
//! into the HTTP daemon in commit 6.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use notify::RecursiveMode;
use notify_debouncer_mini::{DebounceEventResult, DebouncedEvent, Debouncer, new_debouncer};
use serde_json::json;
use sqlx::SqlitePool;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::indexer;

/// D-6.2.5: debounce window. Coalesces bursts (CLAUDE.md save, fleet refactor).
pub const DEBOUNCE_MS: u64 = 500;
/// Heartbeat cadence — `check()` from a separate process treats the singleton
/// state row as live when `now - last_heartbeat_ts < LIVENESS_WINDOW_SECS`.
const HEARTBEAT_INTERVAL_SECS: u64 = 10;
pub const LIVENESS_WINDOW_SECS: i64 = 60;

/// One filesystem subscription the watcher honors. The discriminant carries
/// enough context for the dispatcher to route events to the right corpus
/// reindex helper without re-deriving the corpus from the path.
///
/// `GitRefs(repo_path)` carries the repo root (parent of `.git`). Per D-6.2.4
/// the watcher registers TWO notify watches per `GitRefs`: `<repo>/.git/HEAD`
/// (via parent `.git` NonRecursive) for branch checkouts + `<repo>/.git/refs/heads/`
/// Recursive for commits to local branches. `refs/remotes/` (fetch noise) and
/// `objects/` (loose-object flood) are deliberately NOT watched.
#[derive(Debug, Clone)]
pub enum Subscription {
    ClaudeMd { path: PathBuf, workspace: PathBuf },
    MemoryDir(PathBuf),
    DesignDir(PathBuf),
    /// The anchor ROOT dir, watched non-recursively for depth-1 `*.md` files
    /// (BUG_TRIAGE.md et al.) that join the design corpus (2026-07-09). Distinct
    /// from `DesignDir` (which recurses `docs/**`): `classify()` matches only
    /// direct children of this dir, excluding CLAUDE.md.
    RootMarkdown(PathBuf),
    GitRefs(PathBuf),
}

impl Subscription {
    pub fn watch_path(&self) -> &Path {
        match self {
            Subscription::ClaudeMd { path, .. } => path,
            Subscription::MemoryDir(p)
            | Subscription::DesignDir(p)
            | Subscription::RootMarkdown(p)
            | Subscription::GitRefs(p) => p,
        }
    }

    /// Resolve symlinks so `notify` (macOS FSEvents in particular) receives the
    /// same canonical path it delivers events under. `/var/...` ↔ `/private/var/...`
    /// is the macOS tempdir case; user-supplied workspace paths often hit it
    /// too via Homebrew symlinks. Without this, `classify()` silently drops
    /// every event because string-equality and `starts_with` miss.
    pub fn canonicalize(self) -> Result<Self> {
        match self {
            Subscription::ClaudeMd { path, workspace } => {
                let path = path
                    .canonicalize()
                    .with_context(|| format!("canonicalize CLAUDE.md {path:?}"))?;
                let workspace = workspace
                    .canonicalize()
                    .with_context(|| format!("canonicalize workspace {workspace:?}"))?;
                Ok(Subscription::ClaudeMd { path, workspace })
            }
            Subscription::MemoryDir(p) => {
                let p = p
                    .canonicalize()
                    .with_context(|| format!("canonicalize memory dir {p:?}"))?;
                Ok(Subscription::MemoryDir(p))
            }
            Subscription::DesignDir(p) => {
                let p = p
                    .canonicalize()
                    .with_context(|| format!("canonicalize design dir {p:?}"))?;
                Ok(Subscription::DesignDir(p))
            }
            Subscription::RootMarkdown(p) => {
                let p = p
                    .canonicalize()
                    .with_context(|| format!("canonicalize root-markdown dir {p:?}"))?;
                Ok(Subscription::RootMarkdown(p))
            }
            Subscription::GitRefs(p) => {
                let p = p
                    .canonicalize()
                    .with_context(|| format!("canonicalize repo path {p:?}"))?;
                Ok(Subscription::GitRefs(p))
            }
        }
    }

    /// Notify watches this subscription registers. Most subscriptions return
    /// a single (path, mode) pair; `GitRefs` returns two so we cover `HEAD`
    /// (branch checkouts) plus `refs/heads/` (commits) without watching
    /// `.git/objects/` per D-6.2.4.
    fn notify_watches(&self) -> Vec<(PathBuf, RecursiveMode)> {
        match self {
            Subscription::ClaudeMd { path, .. } => {
                // CLAUDE.md is a single file; FSEvents on macOS works against
                // its parent dir non-recursively. classify() filters siblings.
                let root = path
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| path.clone());
                vec![(root, RecursiveMode::NonRecursive)]
            }
            Subscription::MemoryDir(p) | Subscription::DesignDir(p) => {
                vec![(p.clone(), RecursiveMode::Recursive)]
            }
            Subscription::RootMarkdown(dir) => {
                // Root markdown is depth-1 only; NonRecursive so we don't watch
                // src/, target/, or nested repos. classify() filters to direct
                // `*.md` children. Often the SAME dir ClaudeMd watches — the
                // registration loop dedups the OS watch (see spawn_debouncer).
                vec![(dir.clone(), RecursiveMode::NonRecursive)]
            }
            Subscription::GitRefs(repo_path) => {
                let git_dir = repo_path.join(".git");
                let refs_heads = git_dir.join("refs").join("heads");
                vec![
                    // NonRecursive on .git/ catches HEAD updates (and ignores
                    // .git/objects/ + .git/refs/ subdirs by virtue of depth=0).
                    (git_dir, RecursiveMode::NonRecursive),
                    // Recursive on refs/heads/ catches commits + branch
                    // create/delete/rename across nested ref names.
                    (refs_heads, RecursiveMode::Recursive),
                ]
            }
        }
    }
    fn label(&self) -> String {
        match self {
            Subscription::ClaudeMd { path, .. } => format!("claude_md:{}", path.display()),
            Subscription::MemoryDir(p) => format!("memory:{}", p.display()),
            Subscription::DesignDir(p) => format!("design:{}", p.display()),
            Subscription::RootMarkdown(p) => format!("root_md:{}", p.display()),
            Subscription::GitRefs(p) => format!("git_refs:{}", p.display()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Corpus {
    Session,
    Memory,
    Design,
    Commits,
}

/// Classify a filesystem event path against the active subscriptions. Returns
/// the corpus + the subscription that owns it. Longest-prefix wins so a memory
/// dir nested under the workspace doesn't shadow CLAUDE.md and vice versa.
///
/// `GitRefs` matches only `<repo>/.git/HEAD` and descendants of
/// `<repo>/.git/refs/heads/`. Loose-object writes under `.git/objects/`,
/// fetch updates under `.git/refs/remotes/`, tags under `.git/refs/tags/`,
/// and the `.git/index` / `.git/FETCH_HEAD` churn class all return None —
/// they're delivered by the NonRecursive `.git/` watch but filtered here.
fn classify(path: &Path, subs: &[Subscription]) -> Option<(Corpus, Subscription)> {
    let mut best: Option<(usize, Corpus, Subscription)> = None;
    for sub in subs {
        let matched = match sub {
            Subscription::ClaudeMd { path: claude, .. } => path == claude.as_path(),
            Subscription::MemoryDir(dir) | Subscription::DesignDir(dir) => {
                path.starts_with(dir) && path.extension().and_then(|s| s.to_str()) == Some("md")
            }
            Subscription::RootMarkdown(anchor) => {
                // Depth-1 only: the file's PARENT is the anchor (not a descendant),
                // it's markdown, and it's not CLAUDE.md (owned by the session corpus).
                path.parent() == Some(anchor.as_path())
                    && path.extension().and_then(|s| s.to_str()) == Some("md")
                    && path.file_name().and_then(|s| s.to_str()) != Some("CLAUDE.md")
            }
            Subscription::GitRefs(repo) => {
                let git_dir = repo.join(".git");
                let head = git_dir.join("HEAD");
                let refs_heads = git_dir.join("refs").join("heads");
                path == head.as_path() || path.starts_with(&refs_heads)
            }
        };
        if !matched {
            continue;
        }
        let depth = sub.watch_path().components().count();
        let corpus = match sub {
            Subscription::ClaudeMd { .. } => Corpus::Session,
            Subscription::MemoryDir(_) => Corpus::Memory,
            Subscription::DesignDir(_) | Subscription::RootMarkdown(_) => Corpus::Design,
            Subscription::GitRefs(_) => Corpus::Commits,
        };
        if best.as_ref().is_none_or(|(d, _, _)| depth >= *d) {
            best = Some((depth, corpus, sub.clone()));
        }
    }
    best.map(|(_, corpus, sub)| (corpus, sub))
}

/// Dispatch a single resolved event. Returns true on a successful side-effect.
///
/// `Corpus::Commits` events route to `reindex_commits_for_repo` for the owning
/// repo regardless of whether the path itself still exists — branch-delete
/// (`refs/heads/feature` removed) is NOT a signal to drop commits per D-6.2.4
/// ("commits are immutable content; deletion of a ref doesn't mean the commits
/// should disappear from history archaeology"). The path-existence check stays
/// load-bearing for the file-corpus variants only.
async fn dispatch_one(
    pool: &SqlitePool,
    path: &Path,
    corpus: Corpus,
    sub: &Subscription,
) -> Result<bool> {
    if corpus == Corpus::Commits {
        if let Subscription::GitRefs(repo_path) = sub {
            // Commits first so the new history exists for source provenance to
            // resolve against, THEN re-index the source corpus for the same repo:
            // a commit can change both file content at HEAD and the provenance
            // commit each chunk points at (D1a "re-index-on-commit via GitRefs",
            // D1_SCOPE §5; live working-tree freshness is deferred to D1b).
            indexer::reindex_commits_for_repo(pool, repo_path).await?;
            indexer::reindex_source_for_repo(pool, repo_path).await?;
            return Ok(true);
        }
        return Ok(false);
    }
    let exists = tokio::fs::metadata(path).await.is_ok();
    if exists {
        match corpus {
            Corpus::Session => {
                if let Subscription::ClaudeMd { workspace, .. } = sub {
                    indexer::reindex_claude_md(pool, path, workspace).await?;
                }
            }
            Corpus::Memory => indexer::reindex_memory_file(pool, path).await?,
            Corpus::Design => indexer::reindex_design_file(pool, path).await?,
            Corpus::Commits => unreachable!("Commits handled above"),
        }
        Ok(true)
    } else {
        Ok(indexer::delete_document_by_path(pool, path).await?)
    }
}

/// Run the watcher until the receiver-side shutdown channel resolves.
/// Returns to the caller after a 1s drain window (D-6.2.6).
pub async fn serve(
    pool: SqlitePool,
    subscriptions: Vec<Subscription>,
    mut shutdown_rx: tokio::sync::oneshot::Receiver<()>,
) -> Result<()> {
    if subscriptions.is_empty() {
        anyhow::bail!("watcher requires at least one subscription");
    }
    let subscriptions: Vec<Subscription> = subscriptions
        .into_iter()
        .map(Subscription::canonicalize)
        .collect::<Result<_>>()?;
    init_state(&pool, &subscriptions).await?;

    let (event_tx, event_rx) = mpsc::unbounded_channel::<Vec<DebouncedEvent>>();
    let _debouncer = spawn_debouncer(&subscriptions, event_tx.clone())?;

    let dispatch_handle = tokio::spawn(run_dispatch_loop(pool.clone(), subscriptions, event_rx));
    let heartbeat_handle = tokio::spawn(run_heartbeat_loop(pool.clone()));

    // Block until shutdown signal lands. ShutdownChannel resolves on
    // SIGINT/SIGTERM (CLI wiring) or on test-suite drop.
    let _ = (&mut shutdown_rx).await;

    // Close the event channel so the dispatch loop sees `None` and returns.
    drop(event_tx);
    heartbeat_handle.abort();

    // 1s drain window per D-6.2.6. No guarantees on in-flight events.
    let _ = tokio::time::timeout(Duration::from_secs(1), dispatch_handle).await;
    Ok(())
}

async fn run_dispatch_loop(
    pool: SqlitePool,
    subs: Vec<Subscription>,
    mut rx: UnboundedReceiver<Vec<DebouncedEvent>>,
) {
    while let Some(batch) = rx.recv().await {
        if batch.is_empty() {
            continue;
        }
        let now_ts = now_unix();
        let batch_len = batch.len() as i64;
        for ev in &batch {
            if let Some((corpus, sub)) = classify(&ev.path, &subs)
                && let Err(e) = dispatch_one(&pool, &ev.path, corpus, &sub).await
            {
                eprintln!("[nibdex watcher] dispatch error on {:?}: {e:?}", ev.path);
            }
        }
        if let Err(e) = record_batch(&pool, batch_len, now_ts).await {
            eprintln!("[nibdex watcher] state update failed: {e:?}");
        }
    }
}

async fn run_heartbeat_loop(pool: SqlitePool) {
    let mut tick = tokio::time::interval(Duration::from_secs(HEARTBEAT_INTERVAL_SECS));
    // Discard the immediate tick — `init_state` already wrote a fresh ts.
    tick.tick().await;
    loop {
        tick.tick().await;
        if let Err(e) = update_heartbeat(&pool).await {
            eprintln!("[nibdex watcher] heartbeat error: {e:?}");
        }
    }
}

fn spawn_debouncer(
    subscriptions: &[Subscription],
    event_tx: UnboundedSender<Vec<DebouncedEvent>>,
) -> Result<Debouncer<notify::RecommendedWatcher>> {
    let mut debouncer = new_debouncer(
        Duration::from_millis(DEBOUNCE_MS),
        move |res: DebounceEventResult| match res {
            Ok(events) if !events.is_empty() => {
                let _ = event_tx.send(events);
            }
            Ok(_) => {}
            Err(e) => eprintln!("[nibdex watcher] notify error: {e:?}"),
        },
    )
    .context("create debouncer")?;
    // Distinct subscriptions can target the SAME directory — notably ClaudeMd and
    // RootMarkdown both watch the anchor root non-recursively. `classify()` routes
    // every delivered event across ALL subscriptions regardless of which one
    // registered the OS watch, so one watch per unique path suffices; double-
    // watching a path is backend-dependent (can error). Dedup by path. (Every
    // shared path here is requested at the same NonRecursive mode, so first-wins
    // is exact; docs/ and memory dirs are distinct paths registered Recursive.)
    let mut watched: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    for sub in subscriptions {
        for (root, mode) in sub.notify_watches() {
            // A subscription target may not exist yet (e.g. `.git/refs/heads/`
            // for an empty repo with no local branches). Skip with a friendly
            // note rather than aborting the entire watcher — the next event
            // after the path appears will be classified normally.
            if !root.exists() {
                eprintln!("[nibdex watcher] note: watch path {root:?} not present yet — skipping.");
                continue;
            }
            if !watched.insert(root.clone()) {
                continue; // already watching this directory via another subscription
            }
            debouncer
                .watcher()
                .watch(&root, mode)
                .with_context(|| format!("watch {root:?}"))?;
        }
    }
    Ok(debouncer)
}

// =====================================================================================
// file_watcher_state singleton — read by `check()` in a separate process.
// =====================================================================================

#[derive(Debug, Clone)]
pub struct WatcherStateRow {
    /// Surfaced for future "daemon_uptime_s" parity from cross-process check();
    /// not yet read by an active consumer.
    #[allow(dead_code)]
    pub started_at_ts: i64,
    /// Surfaced for forensic visibility; the liveness gate in `read_live_state`
    /// applies this internally before returning Some(_).
    #[allow(dead_code)]
    pub last_heartbeat_ts: i64,
    pub last_event_ts: Option<i64>,
    pub events_total: i64,
    pub events_coalesced_total: i64,
    pub subscriptions: Vec<String>,
}

pub async fn init_state(pool: &SqlitePool, subscriptions: &[Subscription]) -> Result<()> {
    let now = now_unix();
    let labels: Vec<String> = subscriptions.iter().map(Subscription::label).collect();
    let labels_json = serde_json::to_string(&labels)?;
    sqlx::query(
        r#"
        INSERT INTO file_watcher_state
            (id, started_at_ts, last_heartbeat_ts, last_event_ts,
             events_total, events_coalesced_total, subscriptions_json)
        VALUES (1, ?, ?, NULL, 0, 0, ?)
        ON CONFLICT(id) DO UPDATE SET
            started_at_ts          = excluded.started_at_ts,
            last_heartbeat_ts      = excluded.last_heartbeat_ts,
            last_event_ts          = NULL,
            events_total           = 0,
            events_coalesced_total = 0,
            subscriptions_json     = excluded.subscriptions_json
        "#,
    )
    .bind(now)
    .bind(now)
    .bind(labels_json)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn update_heartbeat(pool: &SqlitePool) -> Result<()> {
    let now = now_unix();
    sqlx::query("UPDATE file_watcher_state SET last_heartbeat_ts = ? WHERE id = 1")
        .bind(now)
        .execute(pool)
        .await?;
    Ok(())
}

async fn record_batch(pool: &SqlitePool, batch_size: i64, now_ts: i64) -> Result<()> {
    // `events_total` counts post-debounce events delivered to the watcher
    // (the sum of every batch's length). `events_coalesced_total` counts
    // debouncer drains — one per batch regardless of how many paths were
    // folded into it. The natural relationship is
    // `events_total >= events_coalesced_total`, with the gap measuring the
    // coalescing yield.
    sqlx::query(
        r#"
        UPDATE file_watcher_state
        SET events_total           = events_total + ?,
            events_coalesced_total = events_coalesced_total + 1,
            last_event_ts          = ?,
            last_heartbeat_ts      = ?
        WHERE id = 1
        "#,
    )
    .bind(batch_size)
    .bind(now_ts)
    .bind(now_ts)
    .execute(pool)
    .await?;
    Ok(())
}

/// Read the singleton state row + liveness gate. Returns None when no row
/// exists or the heartbeat is older than `LIVENESS_WINDOW_SECS` — that way
/// `check()` reports `file_watcher: null` after a watcher crash instead of
/// confidently surfacing stale stats.
pub async fn read_live_state(pool: &SqlitePool) -> Result<Option<WatcherStateRow>> {
    let row: Option<(i64, i64, Option<i64>, i64, i64, String)> = sqlx::query_as(
        "SELECT started_at_ts, last_heartbeat_ts, last_event_ts, \
                events_total, events_coalesced_total, subscriptions_json \
         FROM file_watcher_state WHERE id = 1",
    )
    .fetch_optional(pool)
    .await?;
    let Some((
        started_at_ts,
        last_heartbeat_ts,
        last_event_ts,
        events_total,
        events_coalesced_total,
        subscriptions_json,
    )) = row
    else {
        return Ok(None);
    };
    let now = now_unix();
    if now - last_heartbeat_ts > LIVENESS_WINDOW_SECS {
        return Ok(None);
    }
    let subscriptions: Vec<String> = serde_json::from_str(&subscriptions_json).unwrap_or_default();
    Ok(Some(WatcherStateRow {
        started_at_ts,
        last_heartbeat_ts,
        last_event_ts,
        events_total,
        events_coalesced_total,
        subscriptions,
    }))
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// silence unused import on platforms where json! macro isn't invoked
#[allow(dead_code)]
fn _json_anchor() -> serde_json::Value {
    json!({})
}

// =====================================================================================
// Tests
// =====================================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use std::sync::Arc;
    use tempfile::TempDir;
    use tokio::sync::Notify;

    async fn fresh_pool() -> (TempDir, SqlitePool) {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("nibdex.db");
        let pool = db::open(&db_path).await.unwrap();
        (tmp, pool)
    }

    fn make_subs(workspace: &Path) -> Vec<Subscription> {
        let claude = workspace.join("CLAUDE.md");
        // Pre-create CLAUDE.md as an empty file so `canonicalize()` resolves it.
        std::fs::write(&claude, "").unwrap();
        let memory = workspace.join("memory");
        let design = workspace.join("design");
        std::fs::create_dir_all(&memory).unwrap();
        std::fs::create_dir_all(&design).unwrap();
        vec![
            Subscription::ClaudeMd {
                path: claude,
                workspace: workspace.to_path_buf(),
            },
            Subscription::MemoryDir(memory),
            Subscription::DesignDir(design),
            Subscription::RootMarkdown(workspace.to_path_buf()),
        ]
        .into_iter()
        .map(|s| s.canonicalize().unwrap())
        .collect()
    }

    #[tokio::test]
    async fn classify_routes_paths_to_their_corpus() {
        let tmp = TempDir::new().unwrap();
        let subs = make_subs(tmp.path());
        // Canonical root matches the subscriptions; tmp.path() may still be
        // a symlink (macOS /var → /private/var) so re-resolve here.
        let root = tmp.path().canonicalize().unwrap();
        let claude_path = root.join("CLAUDE.md");
        let mem_path = root.join("memory").join("feedback_x.md");
        let design_path = root.join("design").join("FOO.md");
        let unrelated = root.join("Cargo.toml");

        assert!(matches!(
            classify(&claude_path, &subs),
            Some((Corpus::Session, _))
        ));
        assert!(matches!(
            classify(&mem_path, &subs),
            Some((Corpus::Memory, _))
        ));
        assert!(matches!(
            classify(&design_path, &subs),
            Some((Corpus::Design, _))
        ));
        // Root-level *.md (BUG_TRIAGE.md et al.) route to the design corpus via
        // RootMarkdown (2026-07-09).
        let root_md = root.join("BUG_TRIAGE.md");
        assert!(matches!(
            classify(&root_md, &subs),
            Some((Corpus::Design, _))
        ));
        // CLAUDE.md at root still routes to Session — RootMarkdown excludes it, and
        // the exact ClaudeMd match owns it.
        assert!(matches!(
            classify(&claude_path, &subs),
            Some((Corpus::Session, _))
        ));
        // Non-md files inside memory dir don't classify (sidecars, .DS_Store).
        let sidecar = root.join("memory").join(".DS_Store");
        assert!(classify(&sidecar, &subs).is_none());
        // A root-level non-md file (Cargo.toml) is not claimed by RootMarkdown.
        assert!(classify(&unrelated, &subs).is_none());
    }

    #[tokio::test]
    async fn dispatch_one_reindexes_then_deletes_memory_file() {
        let (workspace, pool) = fresh_pool().await;
        let subs = make_subs(workspace.path());
        let root = workspace.path().canonicalize().unwrap();
        let mem_dir = root.join("memory");
        let mem_file = mem_dir.join("feedback_dispatch_round_trip.md");
        tokio::fs::write(
            &mem_file,
            "---\nname: feedback-dispatch-round-trip\ndescription: smoke\ntype: feedback\n---\nbody\n",
        )
        .await
        .unwrap();

        // Create path → reindex_memory_file → row appears.
        let (corpus, sub) = classify(&mem_file, &subs).unwrap();
        let ok = dispatch_one(&pool, &mem_file, corpus, &sub).await.unwrap();
        assert!(ok);
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM memory_entries")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count.0, 1);

        // Delete path → delete_document_by_path cascades.
        tokio::fs::remove_file(&mem_file).await.unwrap();
        let ok = dispatch_one(&pool, &mem_file, corpus, &sub).await.unwrap();
        assert!(ok, "delete path should report success on first match");
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM memory_entries")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count.0, 0, "FK cascade should drop the memory row");
        let fts: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM search_index WHERE source_table = 'memory_entries'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(fts.0, 0, "search_index sweep should drop the FTS row");
        pool.close().await;
    }

    #[tokio::test]
    async fn dispatch_one_wipes_and_rebuilds_design_doc_on_edit() {
        let (workspace, pool) = fresh_pool().await;
        let subs = make_subs(workspace.path());
        let root = workspace.path().canonicalize().unwrap();
        let path = root.join("design").join("FOO.md");

        tokio::fs::write(&path, "# Top\nfirst\n\n## Old\nsection a\n")
            .await
            .unwrap();
        let (corpus, sub) = classify(&path, &subs).unwrap();
        dispatch_one(&pool, &path, corpus, &sub).await.unwrap();
        let first: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM design_doc_sections")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(first.0, 2);

        // Rewrite with a different heading shape; prior sections must be swept.
        tokio::fs::write(&path, "# Top\nbody only\n").await.unwrap();
        dispatch_one(&pool, &path, corpus, &sub).await.unwrap();
        let second: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM design_doc_sections")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(second.0, 1);
        let fts: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM search_index WHERE source_table = 'design_doc_sections'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(fts.0, 1);
        pool.close().await;
    }

    #[tokio::test]
    async fn dispatch_one_reindexes_claude_md_with_sweep() {
        let (workspace, pool) = fresh_pool().await;
        let subs = make_subs(workspace.path());
        let root = workspace.path().canonicalize().unwrap();
        let claude = root.join("CLAUDE.md");

        let body = "# x\n\n## Recent session history\n\n- **#1**: alpha bb8 fixture\n- **#2**: bravo bb8 fixture\n";
        tokio::fs::write(&claude, body).await.unwrap();
        let (corpus, sub) = classify(&claude, &subs).unwrap();
        dispatch_one(&pool, &claude, corpus, &sub).await.unwrap();
        let first: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM session_entries")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(first.0, 2);

        // Trim CLAUDE.md so #2 disappears → UPSERT-then-sweep should drop the orphan.
        let trimmed = "# x\n\n## Recent session history\n\n- **#1**: alpha bb8 fixture\n";
        tokio::fs::write(&claude, trimmed).await.unwrap();
        dispatch_one(&pool, &claude, corpus, &sub).await.unwrap();
        let second: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM session_entries")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(second.0, 1, "sweep should remove the dropped session entry");
        pool.close().await;
    }

    #[tokio::test]
    async fn init_state_and_heartbeat_round_trip_through_read_live_state() {
        let (workspace, pool) = fresh_pool().await;
        let subs = make_subs(workspace.path());
        init_state(&pool, &subs).await.unwrap();
        let live = read_live_state(&pool)
            .await
            .unwrap()
            .expect("fresh state is live");
        assert_eq!(live.events_total, 0);
        assert_eq!(live.subscriptions.len(), 4);
        assert!(
            live.subscriptions
                .iter()
                .any(|s| s.starts_with("claude_md:"))
        );
        assert!(live.subscriptions.iter().any(|s| s.starts_with("memory:")));
        assert!(live.subscriptions.iter().any(|s| s.starts_with("design:")));
        assert!(live.subscriptions.iter().any(|s| s.starts_with("root_md:")));

        record_batch(&pool, 4, now_unix()).await.unwrap();
        let after = read_live_state(&pool).await.unwrap().unwrap();
        assert_eq!(after.events_total, 4);
        assert_eq!(after.events_coalesced_total, 1);
        assert!(after.last_event_ts.is_some());
        // Two more drains, of 5 and 1 events respectively → 4+5+1=10 events
        // delivered across 1+1+1=3 batches. events_total ≥ events_coalesced_total
        // is the natural relationship.
        record_batch(&pool, 5, now_unix()).await.unwrap();
        record_batch(&pool, 1, now_unix()).await.unwrap();
        let final_state = read_live_state(&pool).await.unwrap().unwrap();
        assert_eq!(final_state.events_total, 10);
        assert_eq!(final_state.events_coalesced_total, 3);
        assert!(final_state.events_total >= final_state.events_coalesced_total);
        pool.close().await;
    }

    #[tokio::test]
    async fn stale_heartbeat_returns_none() {
        let (_workspace, pool) = fresh_pool().await;
        let subs = vec![Subscription::ClaudeMd {
            path: PathBuf::from("/tmp/CLAUDE.md"),
            workspace: PathBuf::from("/tmp"),
        }];
        init_state(&pool, &subs).await.unwrap();
        // Force a stale heartbeat.
        sqlx::query("UPDATE file_watcher_state SET last_heartbeat_ts = 0 WHERE id = 1")
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            read_live_state(&pool).await.unwrap().is_none(),
            "stale heartbeat must surface as None so check() reports null"
        );
        pool.close().await;
    }

    #[tokio::test]
    async fn serve_drains_burst_into_single_batch_and_exits_on_shutdown() {
        let (workspace, pool) = fresh_pool().await;
        let subs = make_subs(workspace.path());
        let root = workspace.path().canonicalize().unwrap();
        let mem_dir = root.join("memory");
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let ready = Arc::new(Notify::new());
        let ready_clone = ready.clone();
        let serve_pool = pool.clone();
        let serve_subs = subs.clone();

        let handle = tokio::spawn(async move {
            // Defer `ready` notify until init_state has written the row so the
            // test's "write file" step can't race ahead of subscription wiring.
            serve_with_ready_notify(serve_pool, serve_subs, shutdown_rx, ready_clone).await
        });

        // Wait for the watcher to be primed.
        tokio::time::timeout(Duration::from_secs(2), ready.notified())
            .await
            .expect("watcher init must complete");
        // FSEvents on macOS takes a short beat to actually start delivering
        // events from a freshly registered watcher. Without this settle the
        // burst writes race ahead of the subscription and are silently lost.
        tokio::time::sleep(Duration::from_millis(300)).await;

        // 3 rapid writes inside the debounce window → 1 batch.
        for i in 0..3u8 {
            let path = mem_dir.join(format!("feedback_burst_{i}.md"));
            tokio::fs::write(
                &path,
                format!(
                    "---\nname: feedback-burst-{i}\ndescription: smoke\ntype: feedback\n---\nbody {i}\n"
                ),
            )
            .await
            .unwrap();
        }

        // Wait long enough for the debouncer to drain + dispatch.
        tokio::time::sleep(Duration::from_millis(DEBOUNCE_MS + 1500)).await;

        let live = read_live_state(&pool)
            .await
            .unwrap()
            .expect("watcher should be live");
        assert!(
            live.events_total >= 3,
            "expected ≥3 post-debounce events, got {}",
            live.events_total
        );
        assert!(
            live.events_coalesced_total >= 1,
            "expected ≥1 batch drained, got {}",
            live.events_coalesced_total
        );
        // Coalescing: batches should be < total events — the whole point of
        // the debouncer. Allow == for slow CI where FSEvents fires one-by-one.
        assert!(
            live.events_coalesced_total <= live.events_total,
            "coalesced batches ({}) must not exceed events_total ({})",
            live.events_coalesced_total,
            live.events_total
        );

        let mem_rows: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM memory_entries")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(mem_rows.0, 3, "all 3 memory files should have landed");

        // Shutdown discipline (D-6.2.6): signal + serve returns inside the
        // 1s drain window.
        shutdown_tx.send(()).unwrap();
        let result = tokio::time::timeout(Duration::from_secs(3), handle).await;
        assert!(result.is_ok(), "serve must return after shutdown signal");
        pool.close().await;
    }

    /// Test-only wrapper that signals readiness after init_state writes the
    /// singleton row, so the test can avoid racing the debouncer's setup.
    async fn serve_with_ready_notify(
        pool: SqlitePool,
        subscriptions: Vec<Subscription>,
        mut shutdown_rx: tokio::sync::oneshot::Receiver<()>,
        ready: Arc<Notify>,
    ) -> Result<()> {
        init_state(&pool, &subscriptions).await?;
        let (event_tx, event_rx) = mpsc::unbounded_channel::<Vec<DebouncedEvent>>();
        let _debouncer = spawn_debouncer(&subscriptions, event_tx.clone())?;
        let dispatch_handle =
            tokio::spawn(run_dispatch_loop(pool.clone(), subscriptions, event_rx));
        let heartbeat_handle = tokio::spawn(run_heartbeat_loop(pool.clone()));
        ready.notify_one();
        let _ = (&mut shutdown_rx).await;
        drop(event_tx);
        heartbeat_handle.abort();
        let _ = tokio::time::timeout(Duration::from_secs(1), dispatch_handle).await;
        Ok(())
    }

    // =================================================================================
    // D-6.2.4: GitRefs subscription tests (commit 5).
    // =================================================================================

    /// Initialize a tiny git repo at `path` with one commit. Returns the
    /// repository so the caller can add more commits. Configures author so
    /// commits succeed without relying on the host's global git config.
    fn init_repo_with_initial_commit(path: &Path) -> git2::Repository {
        let repo = git2::Repository::init(path).unwrap();
        {
            let mut cfg = repo.config().unwrap();
            cfg.set_str("user.email", "test@nibdex.invalid").unwrap();
            cfg.set_str("user.name", "nibdex tests").unwrap();
        }
        commit_file(&repo, "README", "hello\n", "initial commit");
        repo
    }

    fn commit_file(repo: &git2::Repository, name: &str, body: &str, msg: &str) -> git2::Oid {
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
        repo.commit(Some("HEAD"), &sig, &sig, msg, &tree, &parent_refs)
            .unwrap()
    }

    /// HEAD + refs/heads/foo → Commits; loose objects + remotes refs + tags +
    /// packed-refs + .git/index churn all return None. Closes D-6.2.4 noise-floor.
    #[tokio::test]
    async fn classify_routes_git_paths_to_commits_corpus() {
        let tmp = TempDir::new().unwrap();
        let repo_path = tmp.path().canonicalize().unwrap();
        let _repo = init_repo_with_initial_commit(&repo_path);
        let subs = vec![
            Subscription::GitRefs(repo_path.clone())
                .canonicalize()
                .unwrap(),
        ];

        let git_dir = repo_path.join(".git");
        let head = git_dir.join("HEAD");
        let main_branch = git_dir.join("refs").join("heads").join("main");
        let nested_branch = git_dir
            .join("refs")
            .join("heads")
            .join("feature")
            .join("nested");
        let remote_ref = git_dir
            .join("refs")
            .join("remotes")
            .join("origin")
            .join("main");
        let tag_ref = git_dir.join("refs").join("tags").join("v1.0");
        let loose_object = git_dir.join("objects").join("aa").join("bbcc");
        let packed_refs = git_dir.join("packed-refs");
        let index_file = git_dir.join("index");

        assert!(matches!(classify(&head, &subs), Some((Corpus::Commits, _))));
        assert!(matches!(
            classify(&main_branch, &subs),
            Some((Corpus::Commits, _))
        ));
        // Nested branch names (e.g. `feature/nested`) live as sub-paths.
        assert!(matches!(
            classify(&nested_branch, &subs),
            Some((Corpus::Commits, _))
        ));
        // Explicit drops per D-6.2.4: fetch noise / tags / objects / packed-refs / index.
        assert!(classify(&remote_ref, &subs).is_none());
        assert!(classify(&tag_ref, &subs).is_none());
        assert!(classify(&loose_object, &subs).is_none());
        assert!(classify(&packed_refs, &subs).is_none());
        assert!(classify(&index_file, &subs).is_none());
    }

    /// `notify_watches()` for GitRefs registers two paths: `.git/` NonRecursive
    /// (for HEAD) and `.git/refs/heads/` Recursive (for commits + branch CRUD).
    /// Pure unit — no filesystem mutation needed.
    #[test]
    fn git_refs_notify_watches_includes_git_dir_and_refs_heads() {
        let repo = PathBuf::from("/tmp/example-repo");
        let sub = Subscription::GitRefs(repo.clone());
        let watches = sub.notify_watches();
        assert_eq!(watches.len(), 2, "GitRefs must register two notify roots");

        let git_dir = repo.join(".git");
        let refs_heads = git_dir.join("refs").join("heads");
        let has_git_dir = watches
            .iter()
            .any(|(p, m)| p == &git_dir && matches!(m, RecursiveMode::NonRecursive));
        let has_refs_heads = watches
            .iter()
            .any(|(p, m)| p == &refs_heads && matches!(m, RecursiveMode::Recursive));
        assert!(
            has_git_dir,
            ".git/ must be watched NonRecursive (HEAD class)"
        );
        assert!(
            has_refs_heads,
            ".git/refs/heads/ must be watched Recursive (branch class)"
        );
        // .git/objects/ + .git/refs/remotes/ are NOT in the watch set per D-6.2.4.
        let mentions_objects = watches.iter().any(|(p, _)| p.ends_with("objects"));
        let mentions_remotes = watches.iter().any(|(p, _)| p.ends_with("remotes"));
        assert!(!mentions_objects, "objects/ must not be watched (flood)");
        assert!(
            !mentions_remotes,
            "refs/remotes/ must not be watched (fetch noise)"
        );
    }

    /// dispatch_one on a Commits event re-indexes commits for the repo, lands
    /// the cursor in `indexed_repos`, and re-running on the same HEAD is a no-op.
    #[tokio::test]
    async fn dispatch_one_reindexes_commits_on_head_change() {
        let (workspace, pool) = fresh_pool().await;
        let repo_path = workspace.path().canonicalize().unwrap();
        let repo = init_repo_with_initial_commit(&repo_path);
        let sub = Subscription::GitRefs(repo_path.clone())
            .canonicalize()
            .unwrap();
        let head_path = repo_path.join(".git").join("HEAD");

        // First dispatch: 1 commit lands, cursor written.
        let ok = dispatch_one(&pool, &head_path, Corpus::Commits, &sub)
            .await
            .unwrap();
        assert!(ok);
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM commit_entries")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count.0, 1);
        let cursor: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM indexed_repos")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            cursor.0, 1,
            "indexed_repos row must be present after first dispatch"
        );

        // Add a second commit and re-dispatch: count must rise.
        commit_file(&repo, "README", "hello v2\n", "second commit");
        dispatch_one(&pool, &head_path, Corpus::Commits, &sub)
            .await
            .unwrap();
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM commit_entries")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count.0, 2);

        // Third dispatch with no new commits: idempotent (count unchanged).
        dispatch_one(&pool, &head_path, Corpus::Commits, &sub)
            .await
            .unwrap();
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM commit_entries")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count.0, 2, "no-new-commit dispatch must be a no-op");
        pool.close().await;
    }

    /// Force-push fallback: a stale `last_indexed_oid` that's not present in
    /// the repo must not error — `git_commits::extract_commits` silently falls
    /// through to a full walk (via `walk.hide(oid)` returning Err). All
    /// commits land via UPSERT on (repo_path, commit_hash) so no duplicates.
    #[tokio::test]
    async fn dispatch_one_handles_force_push_stale_cursor() {
        let (workspace, pool) = fresh_pool().await;
        let repo_path = workspace.path().canonicalize().unwrap();
        let _repo = init_repo_with_initial_commit(&repo_path);
        let sub = Subscription::GitRefs(repo_path.clone())
            .canonicalize()
            .unwrap();
        let head_path = repo_path.join(".git").join("HEAD");

        // First dispatch seeds commit_entries + indexed_repos cursor.
        dispatch_one(&pool, &head_path, Corpus::Commits, &sub)
            .await
            .unwrap();
        let baseline: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM commit_entries")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(baseline.0, 1);

        // Clobber the cursor to a synthetic OID that doesn't exist in the repo
        // (simulates a force-push that rewrote history beyond our cursor).
        let bogus_oid = "0000000000000000000000000000000000000000";
        let repo_path_str = repo_path.to_string_lossy().into_owned();
        sqlx::query("UPDATE indexed_repos SET last_indexed_oid = ? WHERE repo_path = ?")
            .bind(bogus_oid)
            .bind(&repo_path_str)
            .execute(&pool)
            .await
            .unwrap();

        // Re-dispatch: must succeed, no duplicates, cursor recovers to real HEAD.
        dispatch_one(&pool, &head_path, Corpus::Commits, &sub)
            .await
            .unwrap();
        let after: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM commit_entries")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            after.0, 1,
            "UPSERT must keep commit count at 1, not duplicate"
        );
        let cursor: Option<String> =
            sqlx::query_scalar("SELECT last_indexed_oid FROM indexed_repos WHERE repo_path = ?")
                .bind(&repo_path_str)
                .fetch_optional(&pool)
                .await
                .unwrap();
        assert_ne!(
            cursor.as_deref(),
            Some(bogus_oid),
            "cursor must recover from the stale bogus OID to the real HEAD"
        );
        pool.close().await;
    }

    /// Two repos under separate roots: dispatching an event from repo A only
    /// touches repo A's commits + indexed_repos row; repo B is unaffected.
    /// Closes D-6.2.4 multi-repo dispatch behavior.
    #[tokio::test]
    async fn multi_repo_dispatch_only_indexes_target_repo() {
        let (_workspace_holder, pool) = fresh_pool().await;
        let tmp_a = TempDir::new().unwrap();
        let tmp_b = TempDir::new().unwrap();
        let repo_a = tmp_a.path().canonicalize().unwrap();
        let repo_b = tmp_b.path().canonicalize().unwrap();
        let _ra = init_repo_with_initial_commit(&repo_a);
        let rb = init_repo_with_initial_commit(&repo_b);
        let sub_a = Subscription::GitRefs(repo_a.clone())
            .canonicalize()
            .unwrap();
        let _sub_b = Subscription::GitRefs(repo_b.clone())
            .canonicalize()
            .unwrap();

        // Seed both repos so each has a baseline row.
        dispatch_one(
            &pool,
            &repo_a.join(".git").join("HEAD"),
            Corpus::Commits,
            &sub_a,
        )
        .await
        .unwrap();
        dispatch_one(
            &pool,
            &repo_b.join(".git").join("HEAD"),
            Corpus::Commits,
            &_sub_b,
        )
        .await
        .unwrap();

        let count_a: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM commit_entries WHERE repo_path = ?")
                .bind(repo_a.to_string_lossy().into_owned())
                .fetch_one(&pool)
                .await
                .unwrap();
        let count_b: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM commit_entries WHERE repo_path = ?")
                .bind(repo_b.to_string_lossy().into_owned())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(count_a.0, 1);
        assert_eq!(count_b.0, 1);

        // Add a second commit to repo B only. Dispatch an event for repo A:
        // count_a stays unchanged, count_b stays at 1 too (no event for B).
        commit_file(&rb, "README", "b v2\n", "B's second commit");
        dispatch_one(
            &pool,
            &repo_a.join(".git").join("HEAD"),
            Corpus::Commits,
            &sub_a,
        )
        .await
        .unwrap();
        let count_a_after: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM commit_entries WHERE repo_path = ?")
                .bind(repo_a.to_string_lossy().into_owned())
                .fetch_one(&pool)
                .await
                .unwrap();
        let count_b_after: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM commit_entries WHERE repo_path = ?")
                .bind(repo_b.to_string_lossy().into_owned())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(count_a_after.0, 1, "repo A unchanged (no new commit)");
        assert_eq!(
            count_b_after.0, 1,
            "repo B NOT re-indexed by A's event (cross-repo isolation)"
        );

        // Now dispatch B's HEAD event: B picks up the second commit.
        dispatch_one(
            &pool,
            &repo_b.join(".git").join("HEAD"),
            Corpus::Commits,
            &_sub_b,
        )
        .await
        .unwrap();
        let final_b: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM commit_entries WHERE repo_path = ?")
                .bind(repo_b.to_string_lossy().into_owned())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(final_b.0, 2);
        pool.close().await;
    }
}
