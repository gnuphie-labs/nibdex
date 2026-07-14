// SPDX-License-Identifier: MIT

//! Unit + integration tests for the MCP tool surface — relocated wholesale
//! from `mcp.rs` by gh#6 (see `docs/MCP_SPLIT_PLAN.md`). Drives the `run_*`
//! query layer end-to-end and the `NibdexServer` emission paths; the shared
//! fixtures (`fresh_pool` / `seed_all` / `make_rs_request` / …) live here.

use super::*;
use super::fts5::*;
use super::format::*;
use super::query::*;
use super::types::*;

async fn fresh_pool() -> SqlitePool {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();
    pool
}

/// Seeds sessions, memory entries, design-doc sections, commits, and one shallow repo.
/// All in-window so day filters don't accidentally skip rows.
async fn seed_all(pool: &SqlitePool) {
    // documents (1) — CLAUDE.md
    sqlx::query(
        "INSERT INTO documents (path, kind, content_hash, mtime, indexed_at) \
         VALUES ('/tmp/CLAUDE.md', 'session_history', 'h', 0, 0)",
    )
    .execute(pool)
    .await
    .unwrap();

    // Seed entry_date RELATIVE to now (mirrors the commit seed below) so the
    // `days`-window tests (e.g. days=Some(7)) stay green as wall-clock advances.
    // Hardcoded absolute dates were a time-bomb: they fell outside a 7-day
    // window once "now" crossed ~7 days past them. #618 is the newer entry.
    let recent_date: String = sqlx::query_scalar("SELECT date('now', '-1 day')")
        .fetch_one(pool)
        .await
        .unwrap();
    let older_date: String = sqlx::query_scalar("SELECT date('now', '-3 days')")
        .fetch_one(pool)
        .await
        .unwrap();
    for (sn, date, body) in [
        (
            618,
            recent_date.as_str(),
            "Day 5 SHIPPED — memory extractor lands. bb8 mentioned once.",
        ),
        (
            594,
            older_date.as_str(),
            "formsvc-cf wedge recurrence; rustFetch FD leak diagnosis.",
        ),
    ] {
        let row: (i64,) = sqlx::query_as(
            "INSERT INTO session_entries (document_id, session_number, entry_date, body) \
             VALUES (1, ?, ?, ?) RETURNING id",
        )
        .bind(sn)
        .bind(date)
        .bind(body)
        .fetch_one(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO search_index (body, kind, rowid_ref, source_table) \
             VALUES (?, 'session_history', ?, 'session_entries')",
        )
        .bind(body)
        .bind(row.0)
        .execute(pool)
        .await
        .unwrap();
    }

    // documents (2) — a memory file
    sqlx::query(
        "INSERT INTO documents (path, kind, content_hash, mtime, indexed_at) \
         VALUES ('/tmp/mem-hydration.md', 'memory', 'h', 0, 0)",
    )
    .execute(pool)
    .await
    .unwrap();
    let row: (i64,) = sqlx::query_as(
        "INSERT INTO memory_entries (document_id, name, memory_type, description, body) \
         VALUES (2, 'feedback-hydration-audit', 'feedback', \
                 'audit React #418 hydration mismatches across render-time non-determinism', \
                 'When auditing React #418 errors, grep function BODIES not NAMES for new Date(), Math.random(), locale formatters.') \
         RETURNING id",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO search_index (body, kind, rowid_ref, source_table) \
         VALUES ('audit React hydration mismatches new Date Math.random render-time', \
                 'memory', ?, 'memory_entries')",
    )
    .bind(row.0)
    .execute(pool)
    .await
    .unwrap();

    // documents (3) — a design doc
    sqlx::query(
        "INSERT INTO documents (path, kind, content_hash, mtime, indexed_at) \
         VALUES ('/tmp/F45_DESIGN.md', 'design_doc', 'h', 0, 0)",
    )
    .execute(pool)
    .await
    .unwrap();
    let row: (i64,) = sqlx::query_as(
        "INSERT INTO design_doc_sections (document_id, heading_path, line_start, line_end, body) \
         VALUES (3, 'F45/Path Comparison', 12, 48, \
                 'Path 1 vs Path 2 classification approach for F45 stan_bridge linkage analysis.') \
         RETURNING id",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO search_index (body, kind, rowid_ref, source_table) \
         VALUES ('Path 1 vs Path 2 classification comparison F45 stan_bridge linkage', \
                 'design_doc', ?, 'design_doc_sections')",
    )
    .bind(row.0)
    .execute(pool)
    .await
    .unwrap();

    // commits — one in webhooksvc, one in formsvc; today's epoch (close to now())
    let now: i64 = sqlx::query_scalar("SELECT CAST(strftime('%s','now') AS INTEGER)")
        .fetch_one(pool)
        .await
        .unwrap();
    let yesterday = now - 86400;

    let row: (i64,) = sqlx::query_as(
        "INSERT INTO commit_entries \
            (repo_path, commit_hash, parent_hashes, author_email, author_name, \
             authored_at, committed_at, message_summary, message_body, files_changed) \
         VALUES ('/tmp/webhooksvc', 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', '[]', \
                 'me@example.com', 'Me', ?, ?, \
                 'fix(rustFetch): defensive socket cleanup', \
                 'Closes #427 layer 4 — defensive cleanup paths for mid-body errors.', \
                 '[\"src/rust-fetch.ts\"]') \
         RETURNING id",
    )
    .bind(now)
    .bind(now)
    .fetch_one(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO search_index (body, kind, rowid_ref, source_table) \
         VALUES ('fix rustFetch defensive socket cleanup Closes #427 layer 4', \
                 'commit', ?, 'commit_entries')",
    )
    .bind(row.0)
    .execute(pool)
    .await
    .unwrap();

    let row: (i64,) = sqlx::query_as(
        "INSERT INTO commit_entries \
            (repo_path, commit_hash, parent_hashes, author_email, author_name, \
             authored_at, committed_at, message_summary, message_body, files_changed) \
         VALUES ('/tmp/formsvc', 'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb', '[]', \
                 'me@example.com', 'Me', ?, ?, \
                 'feat(bb8): instrumentation per callsite', \
                 'Adds pool.state() snapshot on bb8::Pool::get() callsites.', \
                 '[\"src/bb8_instrument.rs\"]') \
         RETURNING id",
    )
    .bind(yesterday)
    .bind(yesterday)
    .fetch_one(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO search_index (body, kind, rowid_ref, source_table) \
         VALUES ('feat bb8 instrumentation per callsite pool state snapshot', \
                 'commit', ?, 'commit_entries')",
    )
    .bind(row.0)
    .execute(pool)
    .await
    .unwrap();

    // indexed_repos — webhooksvc deep, formsvc shallow (for shallow_repos test)
    sqlx::query(
        "INSERT INTO indexed_repos \
            (repo_path, last_indexed_oid, is_shallow, commit_count, last_indexed_at) \
         VALUES ('/tmp/webhooksvc', 'a', 0, 1, ?)",
    )
    .bind(now)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO indexed_repos \
            (repo_path, last_indexed_oid, is_shallow, commit_count, last_indexed_at) \
         VALUES ('/tmp/formsvc', 'b', 1, 1, ?)",
    )
    .bind(now)
    .execute(pool)
    .await
    .unwrap();

    // session_edges (child table #7) — the raw-transcript session→code map that
    // `find_session` now serves (punch-list #3). The rationales mirror the
    // `session_entries` bodies above so the find_session query tests exercise the
    // same term semantics (rustFetch / memory extractor / OR-broaden) against the
    // new corpus. Seeded AFTER commit_entries so the `commit_id` FK resolves.
    // Edge A is bound to commit 1 (the rustFetch commit, id 1) to exercise the
    // provenance LEFT JOIN; edge B is unbound (commit_id NULL). FTS body =
    // "rationale + file_path", kind='session_edge' — matching production
    // (`session_index::insert_edge`).
    // Distinct edited_at so recency ordering is testable: sess-618 is newest (now),
    // sess-594 is 2 days old (still inside a 7-day window).
    for (uuid, sid, tool, file, rationale, commit_id, edited_at) in [
        (
            "uuid-594-1",
            "sess-594-formsvc",
            "Write",
            "src/formsvc.rs",
            "formsvc-cf wedge recurrence; rustFetch FD leak diagnosis.",
            Some(1_i64),
            now - 2 * 86400,
        ),
        (
            "uuid-618-1",
            "sess-618-memory",
            "Edit",
            "src/extractor/memory.rs",
            "Day 5 SHIPPED — memory extractor lands. bb8 mentioned once.",
            None,
            now,
        ),
    ] {
        let row: (i64,) = sqlx::query_as(
            "INSERT INTO session_edges \
                (session_id, message_uuid, edge_ordinal, tool, file_path, repo_path, \
                 git_branch, edited_at, rationale, commit_id) \
             VALUES (?, ?, 0, ?, ?, '/tmp/webhooksvc', 'main', ?, ?, ?) RETURNING id",
        )
        .bind(sid)
        .bind(uuid)
        .bind(tool)
        .bind(file)
        .bind(edited_at)
        .bind(rationale)
        .bind(commit_id)
        .fetch_one(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO search_index (body, kind, rowid_ref, source_table) \
             VALUES (?, 'session_edge', ?, 'session_edges')",
        )
        .bind(format!("{rationale} {file}"))
        .bind(row.0)
        .execute(pool)
        .await
        .unwrap();
    }
}

// ---- find_session ---------------------------------------------------------------

#[tokio::test]
async fn find_session_happy_path() {
    let pool = fresh_pool().await;
    seed_all(&pool).await;

    let envelope = run_find_session(
        &pool,
        &FindSessionRequest {
            query: "rustFetch".into(),
            limit: Some(5),
        },
        &mut Stages::default(),
    )
    .await
    .unwrap();

    assert_eq!(envelope.tool, "find_session");
    assert_eq!(envelope.total_matched, 1);
    assert_eq!(envelope.returned, 1);
    let hit = &envelope.results[0];
    assert_eq!(hit.session_id, "sess-594-formsvc");
    assert_eq!(hit.file_path, "src/formsvc.rs");
    assert_eq!(hit.tool, "Write");
    // The provenance LEFT JOIN surfaced the capturing commit (edge A → commit 1).
    assert_eq!(hit.commit_hash.as_deref(), Some("aaaaaaa"));
    assert_eq!(hit.commit_hash_full.as_deref().map(str::len), Some(40));
    assert!(hit.commit_summary.as_deref().unwrap().contains("rustFetch"));
    assert!(hit.rank.is_some());
}

#[tokio::test]
async fn find_session_empty_is_not_an_error() {
    let pool = fresh_pool().await;
    seed_all(&pool).await;

    let envelope = run_find_session(
        &pool,
        &FindSessionRequest {
            query: "no_such_term_xyz".into(),
            limit: None,
        },
        &mut Stages::default(),
    )
    .await
    .unwrap();

    assert_eq!(envelope.total_matched, 0);
    assert!(envelope.results.is_empty());
}

#[tokio::test]
async fn find_session_invalid_fts5_errors() {
    let pool = fresh_pool().await;
    seed_all(&pool).await;

    let result = run_find_session(
        &pool,
        &FindSessionRequest {
            query: "\"unclosed".into(),
            limit: None,
        },
        &mut Stages::default(),
    )
    .await;
    assert!(result.is_err());
}

// ---- D-10.13 OR-fallback --------------------------------------------------------

#[tokio::test]
async fn find_session_or_fallback_broadens_when_and_matches_nothing() {
    let pool = fresh_pool().await;
    seed_all(&pool).await;

    // "memory" lives only in the sess-618 edge, "rustFetch" only in sess-594 — they
    // never co-occur, so the implicit-AND query matches zero. OR-fallback rescues it.
    let envelope = run_find_session(
        &pool,
        &FindSessionRequest {
            query: "memory rustFetch".into(),
            limit: Some(5),
        },
        &mut Stages::default(),
    )
    .await
    .unwrap();

    assert!(envelope.query_broadened, "expected OR-broadening to fire");
    assert_eq!(envelope.total_matched, 2);
    assert_eq!(envelope.returned, 2);
}

#[tokio::test]
async fn find_session_does_not_broaden_when_and_already_matches() {
    let pool = fresh_pool().await;
    seed_all(&pool).await;

    // Both terms co-occur in the sess-618 edge → AND matches → no broadening.
    let envelope = run_find_session(
        &pool,
        &FindSessionRequest {
            query: "memory extractor".into(),
            limit: Some(5),
        },
        &mut Stages::default(),
    )
    .await
    .unwrap();

    assert!(!envelope.query_broadened);
    assert_eq!(envelope.total_matched, 1);
    assert_eq!(envelope.results[0].session_id, "sess-618-memory");
    // This edge is unbound (commit_id NULL) — the LEFT JOIN yields no provenance.
    assert!(envelope.results[0].commit_hash.is_none());
}

#[test]
fn fts5_or_broadened_rules() {
    // Plain multi-term prose → OR-joined.
    assert_eq!(
        fts5_or_broadened("memory rustFetch", "memory rustFetch").as_deref(),
        Some("memory OR rustFetch")
    );
    // Hyphenated prose: sanitize quotes the hostile token, OR-join stays bind-safe.
    assert_eq!(
        fts5_or_broadened("acme-dashboard schedule", "\"acme-dashboard\" schedule")
            .as_deref(),
        Some("\"acme-dashboard\" OR schedule")
    );
    // Single term → nothing to broaden.
    assert_eq!(fts5_or_broadened("memory", "memory"), None);
    // Deliberate FTS5 syntax is respected, never rewritten.
    assert_eq!(fts5_or_broadened("memory OR bb8", "memory OR bb8"), None);
    assert_eq!(fts5_or_broadened("memory NOT bb8", "memory NOT bb8"), None);
    assert_eq!(
        fts5_or_broadened("\"exact phrase\"", "\"exact phrase\""),
        None
    );
    assert_eq!(fts5_or_broadened("(a b)", "(a b)"), None);
    assert_eq!(fts5_or_broadened("title:foo bar", "title:foo bar"), None);
    assert_eq!(fts5_or_broadened("+must other", "+must other"), None);
}

// ---- recent_commits -------------------------------------------------------------

#[tokio::test]
async fn recent_commits_happy_path_filter_match() {
    let pool = fresh_pool().await;
    seed_all(&pool).await;

    let envelope = run_recent_commits(
        &pool,
        &RecentCommitsRequest {
            filter: Some("rustFetch".into()),
            days: Some(7),
            repo: None,
            limit: Some(5),
        },
        &mut Stages::default(),
    )
    .await
    .unwrap();

    assert_eq!(envelope.tool, "recent_commits");
    assert_eq!(envelope.total_matched, 1);
    assert_eq!(envelope.results[0].repo_path, "/tmp/webhooksvc");
    assert_eq!(envelope.results[0].commit_hash, "aaaaaaa");
    assert_eq!(envelope.results[0].commit_hash_full.len(), 40);
    assert!(!envelope.results[0].is_shallow); // webhooksvc is_shallow=0
}

#[tokio::test]
async fn recent_commits_repo_filter_narrows() {
    let pool = fresh_pool().await;
    seed_all(&pool).await;

    let envelope = run_recent_commits(
        &pool,
        &RecentCommitsRequest {
            filter: None,
            days: Some(7),
            repo: Some("formsvc".into()),
            limit: Some(10),
        },
        &mut Stages::default(),
    )
    .await
    .unwrap();

    assert_eq!(envelope.total_matched, 1);
    assert_eq!(envelope.results[0].repo_path, "/tmp/formsvc");
    // formsvc is_shallow=1 in seed; flag should propagate.
    assert!(envelope.results[0].is_shallow);
}

#[tokio::test]
async fn recent_commits_empty_is_not_an_error() {
    let pool = fresh_pool().await;
    seed_all(&pool).await;

    let envelope = run_recent_commits(
        &pool,
        &RecentCommitsRequest {
            filter: Some("no_such_term_zzz".into()),
            days: Some(30),
            repo: None,
            limit: None,
        },
        &mut Stages::default(),
    )
    .await
    .unwrap();
    assert_eq!(envelope.total_matched, 0);
}

#[tokio::test]
async fn recent_commits_invalid_fts5_errors() {
    let pool = fresh_pool().await;
    seed_all(&pool).await;

    let result = run_recent_commits(
        &pool,
        &RecentCommitsRequest {
            filter: Some("\"unclosed".into()),
            days: None,
            repo: None,
            limit: None,
        },
        &mut Stages::default(),
    )
    .await;
    assert!(result.is_err());
}

// ---- find_commit ----------------------------------------------------------------

#[tokio::test]
async fn find_commit_happy_path() {
    let pool = fresh_pool().await;
    seed_all(&pool).await;

    let envelope = run_find_commit(
        &pool,
        &FindCommitRequest {
            query: "bb8".into(),
            repo: None,
            limit: Some(5),
        },
        &mut Stages::default(),
    )
    .await
    .unwrap();

    assert_eq!(envelope.tool, "find_commit");
    assert_eq!(envelope.total_matched, 1);
    assert_eq!(envelope.results[0].repo_path, "/tmp/formsvc");
    assert!(envelope.results[0].rank.is_some());
}

#[tokio::test]
async fn find_commit_empty_is_not_an_error() {
    let pool = fresh_pool().await;
    seed_all(&pool).await;

    let envelope = run_find_commit(
        &pool,
        &FindCommitRequest {
            query: "no_such_term_qqq".into(),
            repo: None,
            limit: None,
        },
        &mut Stages::default(),
    )
    .await
    .unwrap();
    assert_eq!(envelope.total_matched, 0);
}

#[tokio::test]
async fn find_commit_invalid_fts5_errors() {
    let pool = fresh_pool().await;
    seed_all(&pool).await;

    let result = run_find_commit(
        &pool,
        &FindCommitRequest {
            query: "\"unclosed".into(),
            repo: None,
            limit: None,
        },
        &mut Stages::default(),
    )
    .await;
    assert!(result.is_err());
}

// ---- find_memory ----------------------------------------------------------------

#[tokio::test]
async fn find_memory_happy_path() {
    let pool = fresh_pool().await;
    seed_all(&pool).await;

    let envelope = run_find_memory(
        &pool,
        &FindMemoryRequest {
            query: "hydration".into(),
            limit: Some(5),
        },
        &mut Stages::default(),
    )
    .await
    .unwrap();

    assert_eq!(envelope.tool, "find_memory");
    assert_eq!(envelope.total_matched, 1);
    assert_eq!(envelope.results[0].name, "feedback-hydration-audit");
    assert_eq!(envelope.results[0].memory_type, "feedback");
    assert!(envelope.results[0].description.is_some());
}

#[tokio::test]
async fn find_memory_empty_is_not_an_error() {
    let pool = fresh_pool().await;
    seed_all(&pool).await;

    let envelope = run_find_memory(
        &pool,
        &FindMemoryRequest {
            query: "no_such_xyz".into(),
            limit: None,
        },
        &mut Stages::default(),
    )
    .await
    .unwrap();
    assert_eq!(envelope.total_matched, 0);
}

#[tokio::test]
async fn find_memory_invalid_fts5_errors() {
    let pool = fresh_pool().await;
    seed_all(&pool).await;

    let result = run_find_memory(
        &pool,
        &FindMemoryRequest {
            query: "\"unclosed".into(),
            limit: None,
        },
        &mut Stages::default(),
    )
    .await;
    assert!(result.is_err());
}

// ---- find_design_doc ------------------------------------------------------------

#[tokio::test]
async fn find_design_doc_happy_path() {
    let pool = fresh_pool().await;
    seed_all(&pool).await;

    let envelope = run_find_design_doc(
        &pool,
        &FindDesignDocRequest {
            query: "F45 path comparison".into(),
            limit: Some(5),
        },
        &mut Stages::default(),
    )
    .await
    .unwrap();

    assert_eq!(envelope.tool, "find_design_doc");
    assert_eq!(envelope.total_matched, 1);
    assert_eq!(envelope.results[0].doc_path, "/tmp/F45_DESIGN.md");
    assert_eq!(envelope.results[0].heading_path, "F45/Path Comparison");
    assert_eq!(envelope.results[0].line_start, 12);
    assert_eq!(envelope.results[0].line_end, 48);
    assert!(!envelope.results[0].body_excerpt.is_empty());
}

#[tokio::test]
async fn find_design_doc_empty_is_not_an_error() {
    let pool = fresh_pool().await;
    seed_all(&pool).await;

    let envelope = run_find_design_doc(
        &pool,
        &FindDesignDocRequest {
            query: "no_such_design_term".into(),
            limit: None,
        },
        &mut Stages::default(),
    )
    .await
    .unwrap();
    assert_eq!(envelope.total_matched, 0);
}

#[tokio::test]
async fn find_design_doc_invalid_fts5_errors() {
    let pool = fresh_pool().await;
    seed_all(&pool).await;

    let result = run_find_design_doc(
        &pool,
        &FindDesignDocRequest {
            query: "\"unclosed".into(),
            limit: None,
        },
        &mut Stages::default(),
    )
    .await;
    assert!(result.is_err());
}

// ---- find_code (D1a) ------------------------------------------------------------

/// Seed one source file with a provenance commit + two chunks for find_code tests.
async fn seed_source(pool: &SqlitePool) {
    // A commit for provenance to resolve to.
    sqlx::query(
        "INSERT INTO commit_entries \
            (repo_path, commit_hash, author_name, author_email, authored_at, \
             committed_at, message_summary, message_body, files_changed) \
         VALUES ('/repo', 'abc123', 'A', 'a@x', 100, 100, 'add resolver', '', 'src/resolve.rs')",
    )
    .execute(pool)
    .await
    .unwrap();
    let commit_id: i64 = sqlx::query_scalar("SELECT id FROM commit_entries WHERE commit_hash='abc123'")
        .fetch_one(pool)
        .await
        .unwrap();

    sqlx::query(
        "INSERT INTO documents (path, kind, content_hash, mtime, indexed_at) \
         VALUES ('/repo/src/resolve.rs', 'source', 'h', 0, 0)",
    )
    .execute(pool)
    .await
    .unwrap();
    let doc_id: i64 = sqlx::query_scalar("SELECT id FROM documents WHERE path='/repo/src/resolve.rs'")
        .fetch_one(pool)
        .await
        .unwrap();

    let chunk_id: i64 = sqlx::query_scalar(
        "INSERT INTO source_chunks \
            (document_id, path, line_start, line_end, language, body, last_commit_id) \
         VALUES (?, 'src/resolve.rs', 1, 50, 'rust', \
                 'fn resolve_provenance() { let author = oldest_code_commit(); }', ?) \
         RETURNING id",
    )
    .bind(doc_id)
    .bind(commit_id)
    .fetch_one(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO search_index (body, kind, rowid_ref, source_table) \
         VALUES ('fn resolve_provenance() { let author = oldest_code_commit(); }', \
                 'source', ?, 'source_chunks')",
    )
    .bind(chunk_id)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn find_code_happy_path_carries_provenance() {
    let pool = fresh_pool().await;
    seed_source(&pool).await;

    let envelope = run_find_code(
        &pool,
        &FindCodeRequest {
            query: "provenance".into(),
            limit: Some(5),
        },
        &mut Stages::default(),
    )
    .await
    .unwrap();

    assert_eq!(envelope.tool, "find_code");
    assert_eq!(envelope.total_matched, 1);
    let hit = &envelope.results[0];
    assert_eq!(hit.path, "src/resolve.rs");
    assert_eq!(hit.line_start, 1);
    assert_eq!(hit.language.as_deref(), Some("rust"));
    // The code↔commit provenance join rides with the hit.
    assert_eq!(hit.commit_sha.as_deref(), Some("abc123"));
    assert_eq!(hit.commit_summary.as_deref(), Some("add resolver"));
    assert!(!hit.body_excerpt.is_empty());
    // Freshness gate: the seeded documents row points at '/repo/src/resolve.rs',
    // which doesn't exist on this machine — honestly reported, not invented.
    assert_eq!(hit.location, "file_missing");
}

/// Freshness gate end-to-end (DESIGN §9.4): a hit whose `documents` row matches
/// the live file is `verified`; one whose stored hash no longer matches is `stale`.
#[tokio::test]
async fn find_code_location_verified_vs_stale_against_live_files() {
    use sha2::{Digest, Sha256};
    use std::io::Write;
    let pool = fresh_pool().await;

    let dir = tempfile::tempdir().unwrap();
    let abs = dir.path().join("fresh.rs");
    let body = "fn freshcheck_alpha() { /* unchanged since indexing */ }";
    let mut f = std::fs::File::create(&abs).unwrap();
    f.write_all(body.as_bytes()).unwrap();
    drop(f);
    let mtime = std::fs::metadata(&abs)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let mut hasher = Sha256::new();
    hasher.update(body.as_bytes());
    let hash = format!("{:x}", hasher.finalize());

    // Seed a documents row that matches the live file exactly (the indexed state),
    // and one whose stored hash predates an edit (mtime moved, content differs).
    for (path, h, m, chunk_path, chunk_body) in [
        (abs.to_string_lossy().into_owned(), hash.clone(), mtime, "src/fresh.rs", body),
        (
            abs.to_string_lossy().into_owned() + ".stale",
            "0123deadbeef".to_string(),
            mtime - 100,
            "src/stale.rs",
            "fn freshcheck_beta() { /* edited after indexing */ }",
        ),
    ] {
        // The stale row needs a real file on disk whose content differs from the
        // stored hash — write it (content irrelevant, hash mismatch is the point).
        if path.ends_with(".stale") {
            std::fs::write(&path, "fn freshcheck_beta_edited() {}").unwrap();
        }
        let doc_id: i64 = sqlx::query_scalar(
            "INSERT INTO documents (path, kind, content_hash, mtime, indexed_at) \
             VALUES (?, 'source', ?, ?, 0) RETURNING id",
        )
        .bind(&path)
        .bind(&h)
        .bind(m)
        .fetch_one(&pool)
        .await
        .unwrap();
        let chunk_id: i64 = sqlx::query_scalar(
            "INSERT INTO source_chunks \
                (document_id, path, line_start, line_end, language, body, last_commit_id) \
             VALUES (?, ?, 1, 1, 'rust', ?, NULL) RETURNING id",
        )
        .bind(doc_id)
        .bind(chunk_path)
        .bind(chunk_body)
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO search_index (body, kind, rowid_ref, source_table) \
             VALUES (?, 'source', ?, 'source_chunks')",
        )
        .bind(chunk_body)
        .bind(chunk_id)
        .execute(&pool)
        .await
        .unwrap();
    }

    let envelope = run_find_code(
        &pool,
        &FindCodeRequest {
            query: "freshcheck".into(),
            limit: Some(5),
        },
        &mut Stages::default(),
    )
    .await
    .unwrap();
    assert_eq!(envelope.returned, 2);
    let by_path = |p: &str| {
        envelope.results.iter().find(|r| r.path == p).unwrap_or_else(|| panic!("no hit {p}"))
    };
    assert_eq!(by_path("src/fresh.rs").location, "verified");
    // The stale chunk is single-line (< 3 distinctive lines), so the re-locator
    // declines to guess — honest "stale", stored range unchanged.
    assert_eq!(by_path("src/stale.rs").location, "stale");
    assert_eq!(by_path("src/stale.rs").line_shift, None);
}

/// The re-locator end-to-end over MCP (the §9.1 gear's exact failure case): an
/// UNCOMMITTED insert above the chunk → the hit comes back `relocated` with the
/// corrected line numbers and the shift, instead of the silent pre-edit line.
#[tokio::test]
async fn find_code_relocates_after_uncommitted_insert_above() {
    use sha2::{Digest, Sha256};
    let pool = fresh_pool().await;

    let dir = tempfile::tempdir().unwrap();
    let abs = dir.path().join("drifted.rs");
    let stored_body =
        "fn relocheck_entry() {\n    let anchor = first_line();\n    second_line(anchor)\n}";
    // The LIVE file: 12 lines inserted above the indexed chunk (an uncommitted edit).
    std::fs::write(&abs, format!("{}{stored_body}", "// inserted above\n".repeat(12))).unwrap();
    let mtime = std::fs::metadata(&abs)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    // The documents row holds the INDEX-TIME state: the pre-edit hash, an older mtime.
    let mut hasher = Sha256::new();
    hasher.update(stored_body.as_bytes());
    let indexed_hash = format!("{:x}", hasher.finalize());
    let doc_id: i64 = sqlx::query_scalar(
        "INSERT INTO documents (path, kind, content_hash, mtime, indexed_at) \
         VALUES (?, 'source', ?, ?, 0) RETURNING id",
    )
    .bind(abs.to_string_lossy().into_owned())
    .bind(&indexed_hash)
    .bind(mtime - 100)
    .fetch_one(&pool)
    .await
    .unwrap();
    let chunk_id: i64 = sqlx::query_scalar(
        "INSERT INTO source_chunks \
            (document_id, path, line_start, line_end, language, body, last_commit_id) \
         VALUES (?, 'src/drifted.rs', 1, 4, 'rust', ?, NULL) RETURNING id",
    )
    .bind(doc_id)
    .bind(stored_body)
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO search_index (body, kind, rowid_ref, source_table) \
         VALUES (?, 'source', ?, 'source_chunks')",
    )
    .bind(stored_body)
    .bind(chunk_id)
    .execute(&pool)
    .await
    .unwrap();

    let envelope = run_find_code(
        &pool,
        &FindCodeRequest {
            query: "relocheck".into(),
            limit: Some(5),
        },
        &mut Stages::default(),
    )
    .await
    .unwrap();
    assert_eq!(envelope.returned, 1);
    let hit = &envelope.results[0];
    assert_eq!(hit.location, "relocated");
    assert_eq!(hit.line_shift, Some(12));
    // Stored 1..=4 → corrected 13..=16; match_line rides the same shift.
    assert_eq!(hit.line_start, 13);
    assert_eq!(hit.line_end, 16);
    assert_eq!(hit.match_line, 13);
}

#[tokio::test]
async fn find_code_empty_is_not_an_error() {
    let pool = fresh_pool().await;
    seed_source(&pool).await;

    let envelope = run_find_code(
        &pool,
        &FindCodeRequest {
            query: "no_such_symbol_anywhere".into(),
            limit: None,
        },
        &mut Stages::default(),
    )
    .await
    .unwrap();
    assert_eq!(envelope.total_matched, 0);
    assert!(envelope.results.is_empty());
}

/// G (D-10.16) — find_code's body is a MATCH-centered snippet too: a token
/// deep in a large chunk surfaces, where head-truncation would have returned
/// only the chunk's opening lines.
#[tokio::test]
async fn find_code_snippet_centers_on_deep_match() {
    let pool = fresh_pool().await;
    sqlx::query(
        "INSERT INTO documents (path, kind, content_hash, mtime, indexed_at) \
         VALUES ('/repo/src/big.rs', 'source', 'h', 0, 0)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let doc_id: i64 = sqlx::query_scalar("SELECT id FROM documents WHERE path='/repo/src/big.rs'")
        .fetch_one(&pool)
        .await
        .unwrap();
    // Filler code, then a unique identifier deep past the per-body cap.
    let body = format!("{}let DEEPCODETOKEN = compute();", "fn filler() {}\n".repeat(300));
    assert!(body.chars().count() > SOURCE_BODY_CHAR_LIMIT, "match is past the head cap");
    let chunk_id: i64 = sqlx::query_scalar(
        "INSERT INTO source_chunks \
            (document_id, path, line_start, line_end, language, body, last_commit_id) \
         VALUES (?, 'src/big.rs', 1, 600, 'rust', ?, NULL) RETURNING id",
    )
    .bind(doc_id)
    .bind(&body)
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO search_index (body, kind, rowid_ref, source_table) \
         VALUES (?, 'source', ?, 'source_chunks')",
    )
    .bind(&body)
    .bind(chunk_id)
    .execute(&pool)
    .await
    .unwrap();

    let envelope = run_find_code(
        &pool,
        &FindCodeRequest { query: "DEEPCODETOKEN".into(), limit: Some(5) },
        &mut Stages::default(),
    )
    .await
    .unwrap();

    let hit = &envelope.results[0];
    assert!(
        hit.body.contains("DEEPCODETOKEN"),
        "snippet must center on the matched identifier, got: {:?}",
        hit.body
    );
    assert!(hit.body.chars().count() <= SOURCE_BODY_CHAR_LIMIT, "still bounded");
    assert!(hit.body_truncated, "a windowed snippet of a larger chunk is truncated");
    assert_eq!(hit.line_start, 1);
    assert_eq!(hit.line_end, 600);
    // 300 newline-separated filler lines precede the match, so match_line advances
    // well past the chunk start and stays within the chunk.
    assert!(hit.match_line > hit.line_start + 100, "match_line advanced, got {}", hit.match_line);
    assert!(hit.match_line <= hit.line_end, "located line stays within the chunk");
}

// ---- D-10.10: hyphen/dot sanitizer ----------------------------------------------

#[test]
fn sanitize_fts5_quotes_only_parser_hostile_tokens() {
    // Hyphenated product names and version strings get wrapped as phrase literals…
    assert_eq!(
        sanitize_fts5_query("acme-dashboard production schedule revenue"),
        "\"acme-dashboard\" production schedule revenue"
    );
    assert_eq!(sanitize_fts5_query("formsvc-cf"), "\"formsvc-cf\"");
    assert_eq!(sanitize_fts5_query("v0.1.316"), "\"v0.1.316\"");
    // …while deliberate FTS5 syntax passes through untouched.
    assert_eq!(sanitize_fts5_query("bb8 OR rustFetch"), "bb8 OR rustFetch");
    assert_eq!(
        sanitize_fts5_query("production NOT schedule"),
        "production NOT schedule"
    );
    assert_eq!(sanitize_fts5_query("hydrat*"), "hydrat*");
    assert_eq!(
        sanitize_fts5_query("NEAR(production schedule, 5)"),
        "NEAR(production schedule, 5)"
    );
    // A token the caller already quoted is left for FTS5 (incl. multi-word phrases).
    assert_eq!(
        sanitize_fts5_query("\"acme-dashboard\" income"),
        "\"acme-dashboard\" income"
    );
    // An unbalanced quote is still handed to FTS5 verbatim so it errors as before.
    assert_eq!(sanitize_fts5_query("\"unclosed"), "\"unclosed");
}

#[tokio::test]
async fn find_session_hyphenated_query_does_not_crash() {
    // Pre-D-10.10 this crashed with `no such column: cf`. The seeded sess-594 edge
    // rationale contains "formsvc-cf", so the sanitized phrase must match it.
    let pool = fresh_pool().await;
    seed_all(&pool).await;
    let envelope = run_find_session(
        &pool,
        &FindSessionRequest {
            query: "formsvc-cf".into(),
            limit: None,
        },
        &mut Stages::default(),
    )
    .await
    .unwrap();
    assert_eq!(envelope.total_matched, 1);
    assert_eq!(envelope.results[0].session_id, "sess-594-formsvc");
}

#[tokio::test]
async fn find_commit_hyphenated_query_does_not_crash() {
    // Pre-D-10.10 `socket-cleanup` crashed with `no such column: cleanup`; the
    // sanitized phrase matches the adjacent "socket cleanup" in commit 1's body.
    let pool = fresh_pool().await;
    seed_all(&pool).await;
    let envelope = run_find_commit(
        &pool,
        &FindCommitRequest {
            query: "socket-cleanup".into(),
            repo: None,
            limit: None,
        },
        &mut Stages::default(),
    )
    .await
    .unwrap();
    assert!(envelope.total_matched >= 1);
}

// ---- D-10.11: design-doc body cap -----------------------------------------------

#[tokio::test]
async fn find_design_doc_caps_oversized_body() {
    let pool = fresh_pool().await;
    sqlx::query(
        "INSERT INTO documents (path, kind, content_hash, mtime, indexed_at) \
         VALUES ('/tmp/BIG_DESIGN.md', 'design_doc', 'h', 0, 0)",
    )
    .execute(&pool)
    .await
    .unwrap();
    // A section body far larger than the per-result cap.
    let big = format!("nibdex retrieval substitution {}", "padding ".repeat(4000));
    assert!(big.chars().count() > DESIGN_DOC_BODY_CHAR_LIMIT * 4);
    let row: (i64,) = sqlx::query_as(
        "INSERT INTO design_doc_sections (document_id, heading_path, line_start, line_end, body) \
         VALUES (1, 'Big/Section', 1, 9000, ?) RETURNING id",
    )
    .bind(&big)
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO search_index (body, kind, rowid_ref, source_table) \
         VALUES (?, 'design_doc', ?, 'design_doc_sections')",
    )
    .bind(&big)
    .bind(row.0)
    .execute(&pool)
    .await
    .unwrap();

    let envelope = run_find_design_doc(
        &pool,
        &FindDesignDocRequest {
            query: "nibdex retrieval substitution".into(),
            limit: Some(5),
        },
        &mut Stages::default(),
    )
    .await
    .unwrap();

    assert_eq!(envelope.total_matched, 1);
    let r = &envelope.results[0];
    assert!(r.body_truncated, "oversized body must be flagged truncated");
    assert!(
        r.body.chars().count() <= DESIGN_DOC_BODY_CHAR_LIMIT,
        "inline body must be bounded, got {}",
        r.body.chars().count()
    );
    // The line range is still present so the caller can read the full section.
    assert_eq!(r.line_start, 1);
    assert_eq!(r.line_end, 9000);
}

/// G (D-10.16) — the returned body is a MATCH-centered snippet, not the
/// section's head. The match token sits deep in a large section; head
/// truncation would return only the leading filler and miss it entirely.
#[tokio::test]
async fn find_design_doc_snippet_centers_on_deep_match() {
    let pool = fresh_pool().await;
    sqlx::query(
        "INSERT INTO documents (path, kind, content_hash, mtime, indexed_at) \
         VALUES ('/tmp/DEEP.md', 'design_doc', 'h', 0, 0)",
    )
    .execute(&pool)
    .await
    .unwrap();
    // ~12 KB of filler, then a unique token near the end of the section.
    let body = format!("{}ZZUNIQUEDEEPTOKEN trailing passage", "alpha ".repeat(2000));
    assert!(body.chars().count() > DESIGN_DOC_BODY_CHAR_LIMIT * 4, "match is past the head cap");
    let row: (i64,) = sqlx::query_as(
        "INSERT INTO design_doc_sections (document_id, heading_path, line_start, line_end, body) \
         VALUES (1, 'Deep/Section', 10, 400, ?) RETURNING id",
    )
    .bind(&body)
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO search_index (body, kind, rowid_ref, source_table) \
         VALUES (?, 'design_doc', ?, 'design_doc_sections')",
    )
    .bind(&body)
    .bind(row.0)
    .execute(&pool)
    .await
    .unwrap();

    let envelope = run_find_design_doc(
        &pool,
        &FindDesignDocRequest { query: "ZZUNIQUEDEEPTOKEN".into(), limit: Some(5) },
        &mut Stages::default(),
    )
    .await
    .unwrap();

    let r = &envelope.results[0];
    // The headline: the bounded body CONTAINS the deep match (head-truncation
    // would have returned only "alpha alpha …" and failed this).
    assert!(
        r.body.contains("ZZUNIQUEDEEPTOKEN"),
        "snippet must be centered on the match, got: {:?}",
        r.body
    );
    assert!(r.body.chars().count() <= DESIGN_DOC_BODY_CHAR_LIMIT, "still bounded");
    assert!(r.body_truncated, "a windowed snippet of a larger section is truncated");
    assert_eq!(r.line_start, 10);
    assert_eq!(r.line_end, 400);
    // Single-line section (no newlines): the snippet starts at the section's first line.
    assert_eq!(r.match_line, r.line_start);
}

/// G (D-10.16) — `match_line` pinpoints the snippet's start line so the caller
/// jumps straight to the passage. The section starts at file line 100 with 200
/// newline-separated filler lines before the match, so the located line must be
/// well past `line_start` (proving the server-side newline count) and no later
/// than the match's own line.
#[tokio::test]
async fn find_design_doc_reports_snippet_start_line() {
    let pool = fresh_pool().await;
    sqlx::query(
        "INSERT INTO documents (path, kind, content_hash, mtime, indexed_at) \
         VALUES ('/tmp/LINES.md', 'design_doc', 'h', 0, 0)",
    )
    .execute(&pool)
    .await
    .unwrap();
    // Section spans file lines 100..400; 200 filler lines, then the match on line 300.
    let body = format!("{}MATCHANCHORTOKEN passage", "filler line\n".repeat(200));
    let row: (i64,) = sqlx::query_as(
        "INSERT INTO design_doc_sections (document_id, heading_path, line_start, line_end, body) \
         VALUES (1, 'Lines/Section', 100, 400, ?) RETURNING id",
    )
    .bind(&body)
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO search_index (body, kind, rowid_ref, source_table) \
         VALUES (?, 'design_doc', ?, 'design_doc_sections')",
    )
    .bind(&body)
    .bind(row.0)
    .execute(&pool)
    .await
    .unwrap();

    let envelope = run_find_design_doc(
        &pool,
        &FindDesignDocRequest { query: "MATCHANCHORTOKEN".into(), limit: Some(5) },
        &mut Stages::default(),
    )
    .await
    .unwrap();

    let r = &envelope.results[0];
    assert!(
        r.match_line > r.line_start + 150,
        "snippet start counted the newlines past the section start, got {}",
        r.match_line
    );
    assert!(r.match_line <= 300, "snippet start cannot be past the match line, got {}", r.match_line);
    assert!(r.match_line <= r.line_end, "located line stays within the section");
    // And the snippet itself contains the match (passage + pinpoint together).
    assert!(r.body.contains("MATCHANCHORTOKEN"));
}

/// G (D-10.11/D-10.16) — the total-body budget holds across many matching
/// sections (the original ~246 KB blow-up was breadth, not one fat section):
/// inline bodies sum within budget, the tail is dropped to empty, and a
/// dropped body still carries `body_truncated` + a non-empty excerpt.
#[tokio::test]
async fn find_design_doc_enforces_total_body_budget_across_results() {
    let pool = fresh_pool().await;
    sqlx::query(
        "INSERT INTO documents (path, kind, content_hash, mtime, indexed_at) \
         VALUES ('/tmp/MANY.md', 'design_doc', 'h', 0, 0)",
    )
    .execute(&pool)
    .await
    .unwrap();
    // 60 fat matching sections (> MAX_LIMIT); each snippet is well under the
    // per-body cap, so the TOTAL budget (not the per-body cap) is what bites the
    // tail across the limited result set.
    let body = format!("budgetfiller {}", "lorem ".repeat(400));
    for i in 0..60 {
        let row: (i64,) = sqlx::query_as(
            "INSERT INTO design_doc_sections (document_id, heading_path, line_start, line_end, body) \
             VALUES (1, ?, ?, ?, ?) RETURNING id",
        )
        .bind(format!("Sec/{i}"))
        .bind(i * 10 + 1)
        .bind(i * 10 + 9)
        .bind(&body)
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO search_index (body, kind, rowid_ref, source_table) \
             VALUES (?, 'design_doc', ?, 'design_doc_sections')",
        )
        .bind(&body)
        .bind(row.0)
        .execute(&pool)
        .await
        .unwrap();
    }

    let envelope = run_find_design_doc(
        &pool,
        &FindDesignDocRequest { query: "budgetfiller".into(), limit: Some(MAX_LIMIT) },
        &mut Stages::default(),
    )
    .await
    .unwrap();

    assert_eq!(envelope.returned, MAX_LIMIT, "result set capped at the limit");
    let total_body: usize = envelope.results.iter().map(|r| r.body.chars().count()).sum();
    assert!(
        total_body <= DESIGN_DOC_TOTAL_BODY_BUDGET + DESIGN_DOC_BODY_CHAR_LIMIT,
        "total inline body must stay within budget (+ at most one overshoot body), got {total_body}"
    );
    let dropped: Vec<_> = envelope.results.iter().filter(|r| r.body.is_empty()).collect();
    assert!(!dropped.is_empty(), "the tail past the budget must be dropped to empty bodies");
    for r in dropped {
        assert!(r.body_truncated, "a dropped body is flagged truncated");
        assert!(!r.body_excerpt.is_empty(), "a dropped body still carries an orienting excerpt");
    }
}

// ---- recent_sessions ordering ---------------------------------------------------

#[tokio::test]
async fn recent_sessions_orders_by_edit_recency() {
    // recent_sessions returns one representative row per session, most-recently-edited
    // first, ordered by edited_at. (The old D-10.12 body-date-vs-session_number
    // confound was a session_entries-only artifact; session_edges carry a real epoch.)
    let pool = fresh_pool().await;
    let now: i64 = sqlx::query_scalar("SELECT CAST(strftime('%s','now') AS INTEGER)")
        .fetch_one(&pool)
        .await
        .unwrap();
    for (uuid, sid, edited_at, rationale) in [
        ("u-old", "sess-old", now - 5 * 86400, "older session touched auth"),
        ("u-new", "sess-new", now - 86400, "newer session touched pool"),
    ] {
        let row: (i64,) = sqlx::query_as(
            "INSERT INTO session_edges \
                (session_id, message_uuid, edge_ordinal, tool, file_path, edited_at, rationale) \
             VALUES (?, ?, 0, 'Edit', 'src/x.rs', ?, ?) RETURNING id",
        )
        .bind(sid)
        .bind(uuid)
        .bind(edited_at)
        .bind(rationale)
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO search_index (body, kind, rowid_ref, source_table) \
             VALUES (?, 'session_edge', ?, 'session_edges')",
        )
        .bind(rationale)
        .bind(row.0)
        .execute(&pool)
        .await
        .unwrap();
    }

    let req = make_rs_request(None, Some(3650), Some(10));
    let envelope = run_recent_sessions(&pool, &req, &mut Stages::default())
        .await
        .unwrap();
    assert_eq!(envelope.results.len(), 2);
    assert_eq!(
        envelope.results[0].session_id, "sess-new",
        "most-recent edit must lead"
    );
    assert_eq!(envelope.results[1].session_id, "sess-old");
}

// ---- check() --------------------------------------------------------------------

#[tokio::test]
async fn check_returns_d633_envelope() {
    let pool = fresh_pool().await;
    seed_all(&pool).await;

    let result = run_check(&pool, 42, None, &mut Stages::default()).await.unwrap();

    // D-6.3.3 contract — schema_version is the load-bearing key.
    assert_eq!(result.schema_version, CHECK_SCHEMA_VERSION);
    assert_eq!(result.daemon_uptime_s, 42);

    // Indexer counts must reflect seed data.
    assert_eq!(result.indexer.session_entries, 2);
    assert_eq!(result.indexer.session_edges, 2);
    assert_eq!(result.indexer.memory_entries, 1);
    assert_eq!(result.indexer.design_doc_sections, 1);
    assert_eq!(result.indexer.commit_entries, 2);
    assert_eq!(result.indexer.indexed_repos, 2);
    // 2 session_entries + 1 memory + 1 design + 2 commits + 2 session_edges.
    assert_eq!(result.indexer.search_index_total, 8);
    assert_eq!(result.indexer.documents.get("session_history"), Some(&1));
    assert_eq!(result.indexer.documents.get("memory"), Some(&1));
    assert_eq!(result.indexer.documents.get("design_doc"), Some(&1));

    // Orphan detection live as of commit 3 (D-6.3.1). `seed_all` uses fake
    // `/tmp/...` paths so every parent doc + repo registers as missing on
    // disk → each class reports the count of child rows whose parent is
    // gone. CLAUDE.md doc exists but its path is unreadable → all 2 session
    // rows orphaned; same shape for memory (1) + design (1) + repos (2).
    assert_eq!(result.orphans.session_entries, 2);
    assert_eq!(result.orphans.memory_entries, 1);
    assert_eq!(result.orphans.design_doc_sections, 1);
    assert_eq!(result.orphans.indexed_repos, 2);

    // Shallow repos surface from indexed_repos.is_shallow=1.
    assert_eq!(result.shallow_repos, vec!["/tmp/formsvc".to_string()]);

    // file_watcher None at commit 2 (commit 4 wires this).
    assert!(result.file_watcher.is_none());
}

#[tokio::test]
async fn check_emits_tool_percentiles_from_op_measurements() {
    let pool = fresh_pool().await;
    seed_all(&pool).await;

    // Seed a few op_measurements rows in the tool.* namespace.
    for ms in [5_i64, 7, 9, 11, 13] {
        sqlx::query(
            "INSERT INTO op_measurements \
                (op_name, started_at, duration_ms, extra_json) \
             VALUES ('tool.find_session', CAST(strftime('%s','now') AS INTEGER), ?, '{}')",
        )
        .bind(ms)
        .execute(&pool)
        .await
        .unwrap();
    }

    let result = run_check(&pool, 0, None, &mut Stages::default()).await.unwrap();
    let p50 = result.perf_p50_ms.get("tool.find_session").copied();
    let p95 = result.perf_p95_ms.get("tool.find_session").copied();
    assert_eq!(p50, Some(9), "median of [5,7,9,11,13] = 9");
    assert_eq!(
        p95,
        Some(13),
        "near-rank p95 of 5 values rounds to last = 13"
    );
}

#[tokio::test]
async fn check_extractors_last_run_ms_picks_latest_per_op() {
    let pool = fresh_pool().await;
    seed_all(&pool).await;

    // Two rows per op_name; the higher id wins.
    sqlx::query(
        "INSERT INTO op_measurements (op_name, started_at, duration_ms, extra_json) \
         VALUES ('extract.memory', 100, 100, '{}'), ('extract.memory', 200, 555, '{}')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let result = run_check(&pool, 0, None, &mut Stages::default()).await.unwrap();
    assert_eq!(
        result.extractors_last_run_ms.get("extract.memory"),
        Some(&555)
    );
}

// ---- orphan detection (D-6.3.1) -------------------------------------------------

/// Session-orphan path with a real CLAUDE.md on disk whose entry set is a
/// strict subset of the DB. Exercises the set-diff branch (not the
/// unreadable-file branch).
#[tokio::test]
async fn compute_session_orphans_detects_extra_db_entries() {
    let pool = fresh_pool().await;
    let tmp = tempfile::tempdir().unwrap();
    let claude_md = tmp.path().join("CLAUDE.md");
    std::fs::write(
        &claude_md,
        "# top\n\n## Recent session history\n\n- **#100**: a.\n- **#200**: b.\n",
    )
    .unwrap();

    let path_str = claude_md.to_string_lossy().to_string();
    sqlx::query("INSERT INTO documents (path, kind, content_hash, mtime, indexed_at) VALUES (?, 'session_history', 'h', 0, 0)")
        .bind(&path_str).execute(&pool).await.unwrap();

    for sn in [100_i64, 200, 999] {
        sqlx::query(
            "INSERT INTO session_entries (document_id, session_number, entry_date, body) \
             VALUES (1, ?, '2026-05-26', 'body')",
        )
        .bind(sn)
        .execute(&pool)
        .await
        .unwrap();
    }

    let result = run_check(&pool, 0, None, &mut Stages::default()).await.unwrap();
    assert_eq!(
        result.orphans.session_entries, 1,
        "DB has 100/200/999, file has 100/200 → 1 orphan"
    );
}

/// Memory + design + repo orphan classes all report zero when every parent
/// doc and repo path resolves on disk. Companion to the
/// `check_returns_d633_envelope` test which covers the all-fake case.
#[tokio::test]
async fn compute_orphans_all_zero_with_real_files() {
    let pool = fresh_pool().await;
    let tmp = tempfile::tempdir().unwrap();

    // Real memory doc + entry pointing at it.
    let mem_path = tmp.path().join("mem.md");
    std::fs::write(&mem_path, "body").unwrap();
    sqlx::query("INSERT INTO documents (path, kind, content_hash, mtime, indexed_at) VALUES (?, 'memory', 'h', 0, 0)")
        .bind(mem_path.to_string_lossy().as_ref()).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO memory_entries (document_id, name, memory_type, body) VALUES (1, 'real-mem', 'feedback', 'body')")
        .execute(&pool).await.unwrap();

    // Real design doc + section pointing at it.
    let design_path = tmp.path().join("design.md");
    std::fs::write(&design_path, "body").unwrap();
    sqlx::query("INSERT INTO documents (path, kind, content_hash, mtime, indexed_at) VALUES (?, 'design_doc', 'h', 0, 0)")
        .bind(design_path.to_string_lossy().as_ref()).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO design_doc_sections (document_id, heading_path, line_start, line_end, body) VALUES (2, 'Top', 1, 5, 'body')")
        .execute(&pool).await.unwrap();

    // Real repo dir.
    let repo_path = tmp.path().join("repo");
    std::fs::create_dir(&repo_path).unwrap();
    sqlx::query("INSERT INTO indexed_repos (repo_path, last_indexed_oid, is_shallow, commit_count, last_indexed_at) VALUES (?, 'a', 0, 0, 0)")
        .bind(repo_path.to_string_lossy().as_ref()).execute(&pool).await.unwrap();

    let result = run_check(&pool, 0, None, &mut Stages::default()).await.unwrap();
    assert_eq!(
        result.orphans.session_entries, 0,
        "no CLAUDE.md indexed + no session_entries → 0"
    );
    assert_eq!(result.orphans.memory_entries, 0);
    assert_eq!(result.orphans.design_doc_sections, 0);
    assert_eq!(result.orphans.indexed_repos, 0);
}

/// Repo orphan fires after the directory is removed mid-flight — the
/// real-life case that motivated this class (branch cleanup, archive-and-rm).
#[tokio::test]
async fn compute_repo_orphan_detected_after_dir_removal() {
    let pool = fresh_pool().await;
    let tmp = tempfile::tempdir().unwrap();
    let alive = tmp.path().join("alive");
    let doomed = tmp.path().join("doomed");
    std::fs::create_dir(&alive).unwrap();
    std::fs::create_dir(&doomed).unwrap();

    for p in [&alive, &doomed] {
        sqlx::query("INSERT INTO indexed_repos (repo_path, last_indexed_oid, is_shallow, commit_count, last_indexed_at) VALUES (?, 'a', 0, 0, 0)")
            .bind(p.to_string_lossy().as_ref()).execute(&pool).await.unwrap();
    }

    // Steady state: both alive.
    let r = run_check(&pool, 0, None, &mut Stages::default()).await.unwrap();
    assert_eq!(r.orphans.indexed_repos, 0);

    std::fs::remove_dir(&doomed).unwrap();
    let r = run_check(&pool, 0, None, &mut Stages::default()).await.unwrap();
    assert_eq!(r.orphans.indexed_repos, 1);
}

/// Session-orphan unreadable-file branch. Doc row points at a path that
/// never existed → every DB session_entry under it is reported as orphaned.
#[tokio::test]
async fn compute_session_orphans_unreadable_file_reports_all_rows() {
    let pool = fresh_pool().await;
    // Use a path that definitely doesn't exist.
    sqlx::query("INSERT INTO documents (path, kind, content_hash, mtime, indexed_at) VALUES ('/tmp/nibdex-test-vanished-claude.md', 'session_history', 'h', 0, 0)")
        .execute(&pool).await.unwrap();
    for sn in [1_i64, 2, 3] {
        sqlx::query(
            "INSERT INTO session_entries (document_id, session_number, entry_date, body) \
             VALUES (1, ?, '2026-05-26', 'body')",
        )
        .bind(sn)
        .execute(&pool)
        .await
        .unwrap();
    }

    let result = run_check(&pool, 0, None, &mut Stages::default()).await.unwrap();
    assert_eq!(
        result.orphans.session_entries, 3,
        "all 3 rows orphaned when source unreadable"
    );
}

// ---- helper unit tests ----------------------------------------------------------

#[test]
fn summarize_short_body_returns_whole() {
    assert_eq!(summarize("short", 200), "short");
}

#[test]
fn summarize_long_body_truncates_on_word_boundary() {
    let body = "a".repeat(150) + " " + &"b".repeat(150);
    let out = summarize(&body, 200);
    assert!(out.chars().count() <= 201);
    assert!(out.ends_with('…'));
}

#[test]
fn percentile_nearest_rank_matches_design() {
    let v = vec![1_i64, 2, 3, 4, 5, 6, 7, 8, 9, 10];
    assert_eq!(percentile(&v, 0.50), 6); // round((10-1)*0.5)=round(4.5)=5 → v[5]=6
    assert_eq!(percentile(&v, 0.95), 10);
    assert_eq!(percentile(&v, 0.0), 1);
}

#[test]
fn like_substring_escapes_wildcards() {
    // % and _ in repo substrings must not act as wildcards.
    let s = like_substring("my_repo");
    assert!(s.contains("\\_"));
    let s = like_substring("100%");
    assert!(s.contains("\\%"));
}

#[test]
fn unix_to_iso_round_trips_known_epoch() {
    // Epoch 0 = Unix's birthday.
    assert_eq!(unix_to_iso(0), "1970-01-01T00:00:00Z");
    // 1700000000 = 2023-11-14T22:13:20Z (manually verified).
    assert_eq!(unix_to_iso(1700000000), "2023-11-14T22:13:20Z");
}

// ---- recent_sessions (kept from commit 1) ---------------------------------------

fn make_rs_request(
    filter: Option<&str>,
    days: Option<i64>,
    limit: Option<i64>,
) -> RecentSessionsRequest {
    RecentSessionsRequest {
        filter: filter.map(|s| s.to_string()),
        days,
        limit,
    }
}

#[tokio::test]
async fn recent_sessions_happy_path_filter_match() {
    let pool = fresh_pool().await;
    seed_all(&pool).await;

    let req = make_rs_request(Some("rustFetch"), Some(7), Some(5));
    let envelope = run_recent_sessions(&pool, &req, &mut Stages::default()).await.unwrap();
    assert_eq!(envelope.tool, "recent_sessions");
    assert_eq!(envelope.total_matched, 1);
    assert_eq!(envelope.results[0].session_id, "sess-594-formsvc");
}

#[tokio::test]
async fn recent_sessions_empty_is_not_an_error() {
    let pool = fresh_pool().await;
    seed_all(&pool).await;

    let req = make_rs_request(Some("zzz_no_term_zzz"), Some(30), None);
    let envelope = run_recent_sessions(&pool, &req, &mut Stages::default()).await.unwrap();
    assert_eq!(envelope.total_matched, 0);
}

#[tokio::test]
async fn recent_sessions_invalid_fts5_errors() {
    let pool = fresh_pool().await;
    seed_all(&pool).await;

    let req = make_rs_request(Some("\"unclosed"), Some(30), None);
    let result = run_recent_sessions(&pool, &req, &mut Stages::default()).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn recent_sessions_no_filter_orders_by_edit_recency_desc() {
    let pool = fresh_pool().await;
    seed_all(&pool).await;

    let req = make_rs_request(None, Some(30), Some(10));
    let envelope = run_recent_sessions(&pool, &req, &mut Stages::default()).await.unwrap();
    // Two distinct sessions within the window; the newer edit (sess-618, now) leads
    // the older (sess-594, 2 days ago). Recency ordering carries no bm25 rank.
    assert_eq!(envelope.total_matched, 2);
    assert_eq!(envelope.results[0].session_id, "sess-618-memory");
    assert_eq!(envelope.results[1].session_id, "sess-594-formsvc");
    assert!(envelope.results[0].rank.is_none());
}

#[test]
fn recent_sessions_limit_is_clamped_to_max() {
    let req = make_rs_request(None, None, Some(999));
    let limit = req.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    assert_eq!(limit, MAX_LIMIT);
}

// =====================================================================
// §5.5 Layer 1 emission tests (Day 7 commit 2 G7.* gates)
// =====================================================================
//
// Each test routes a tool call through a `NibdexServer` configured with
// a `MetricsSink::JsonlFile` pointed at a tempfile, then reads the file
// back and asserts the documented D-7.2 shape + per-tool field values.
// Per-line flush in `MetricsSink::emit` (D-7.5) means lines are readable
// without dropping the server.

use crate::metrics_sink::{MetricsSink, MetricsSinkSpec};
use rmcp::handler::server::wrapper::Parameters;

/// Build a NibdexServer wired to a JsonlFile sink at `jsonl_path`.
/// Day 8 commit 1: calibration is `None` — Layer 2 emission is off
/// for Day 7's G7.* gates, which only assert Layer 1 envelope shape.
/// Commit 2's G8.* gates pass `Some(_)` to test the additive fields.
fn server_with_jsonl_sink(pool: SqlitePool, jsonl_path: &std::path::Path) -> NibdexServer {
    let sink = MetricsSink::from_spec(MetricsSinkSpec::Jsonl(jsonl_path.to_path_buf()))
        .expect("open jsonl sink for test");
    NibdexServer::new(pool, Instant::now(), Some(Arc::new(sink)), None)
}

/// Read all JSONL lines from `path`, parsing each into a `serde_json::Value`.
fn read_jsonl(path: &std::path::Path) -> Vec<serde_json::Value> {
    let raw = std::fs::read_to_string(path).expect("read jsonl");
    raw.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str::<serde_json::Value>(l).expect("parse jsonl line"))
        .collect()
}

/// Assert the D-7.2 envelope keys are present + v1 constants pinned. Used
/// by every per-tool emission test + the cross-tool consistency check.
fn assert_d72_envelope(line: &serde_json::Value, expected_tool: &str) {
    for key in [
        "schema_version",
        "ts",
        "tool",
        "query",
        "params",
        "wall_ms",
        "stages_ms",
        "candidate_count",
        "result_token_estimate",
        "cache_hit",
        "daemon_uptime_s",
        "calibration_confidence",
    ] {
        assert!(line.get(key).is_some(), "missing key: {key} in {line}");
    }
    assert_eq!(line["schema_version"], 1);
    assert_eq!(line["calibration_confidence"], "estimated");
    assert_eq!(line["tool"], expected_tool);
    // stages_ms carries the 4 D-7.3 keys.
    for stage in ["fts5_query", "rank", "join", "shape_response"] {
        assert!(
            line["stages_ms"].get(stage).is_some(),
            "stages_ms missing {stage}: {line}"
        );
    }
    // candidate_count carries the 2 D-7.3 keys.
    assert!(line["candidate_count"].get("fts5").is_some());
    assert!(line["candidate_count"].get("after_rank").is_some());
}

#[tokio::test]
async fn emission_recent_sessions_writes_one_line_with_filter() {
    let pool = fresh_pool().await;
    seed_all(&pool).await;
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("d7.jsonl");
    let server = server_with_jsonl_sink(pool, &path);

    let _ = server
        .recent_sessions(Parameters(RecentSessionsRequest {
            filter: Some("rustFetch".into()),
            days: Some(7),
            limit: Some(5),
        }))
        .await
        .expect("recent_sessions ok");

    let lines = read_jsonl(&path);
    assert_eq!(lines.len(), 1, "exactly one emission");
    let l = &lines[0];
    assert_d72_envelope(l, "recent_sessions");
    assert_eq!(l["query"], "rustFetch");
    assert_eq!(l["params"]["filter_set"], true);
    assert_eq!(l["params"]["days"], 7);
    // had_fts5 path → candidate_count.fts5 == total_matched (1 in seed).
    assert_eq!(l["candidate_count"]["fts5"], 1);
    assert_eq!(l["candidate_count"]["after_rank"], 1);
    assert!(l["result_token_estimate"].as_u64().unwrap() > 0);
}

#[tokio::test]
async fn emission_find_session_writes_one_line() {
    let pool = fresh_pool().await;
    seed_all(&pool).await;
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("d7.jsonl");
    let server = server_with_jsonl_sink(pool, &path);

    let _ = server
        .find_session(Parameters(FindSessionRequest {
            query: "rustFetch".into(),
            limit: Some(5),
        }))
        .await
        .expect("find_session ok");

    let lines = read_jsonl(&path);
    assert_eq!(lines.len(), 1);
    let l = &lines[0];
    assert_d72_envelope(l, "find_session");
    assert_eq!(l["query"], "rustFetch");
    // find_session always FTS5 → candidate_count.fts5 == total_matched.
    assert_eq!(l["candidate_count"]["fts5"], 1);
    assert!(l["result_token_estimate"].as_u64().unwrap() > 0);
}

/// D-10.13: `query_broadened` is persisted into the JSONL row, but only on an
/// FTS5 path — present-and-true when the OR-fallback fires, present-and-false
/// when an FTS5 query ran without broadening, and absent entirely off the FTS5
/// path (no-filter `recent_*`, `check`). This keeps the broaden-rate's
/// denominator clean for the IP-safe metrics export (METRICS_EXPORT_SPEC §5.1).
#[tokio::test]
async fn emission_query_broadened_recorded_on_fts5_paths_only() {
    let pool = fresh_pool().await;
    seed_all(&pool).await;
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("broaden.jsonl");
    let server = server_with_jsonl_sink(pool, &path);

    // (1) AND-terms never co-occur (#594 vs #618) → OR-fallback fires.
    let _ = server
        .find_session(Parameters(FindSessionRequest {
            query: "memory rustFetch".into(),
            limit: Some(5),
        }))
        .await
        .expect("find_session ok");
    // (2) terms co-occur (#618) → FTS5 path, no broadening.
    let _ = server
        .find_session(Parameters(FindSessionRequest {
            query: "memory extractor".into(),
            limit: Some(5),
        }))
        .await
        .expect("find_session ok");
    // (3) no-filter recent_sessions → not an FTS5 path.
    let _ = server
        .recent_sessions(Parameters(RecentSessionsRequest {
            filter: None,
            days: Some(7),
            limit: Some(5),
        }))
        .await
        .expect("recent_sessions ok");
    // (4) check → no query at all.
    let _ = server
        .check(Parameters(CheckRequest::default()))
        .await
        .expect("check ok");

    let lines = read_jsonl(&path);
    assert_eq!(lines.len(), 4, "one emission per call");

    assert_eq!(lines[0]["tool"], "find_session");
    assert_eq!(lines[0]["query_broadened"], true, "OR-fallback fired: {}", lines[0]);

    assert_eq!(lines[1]["tool"], "find_session");
    assert_eq!(
        lines[1]["query_broadened"], false,
        "FTS5 path, no broaden → present-and-false: {}",
        lines[1]
    );

    assert_eq!(lines[2]["tool"], "recent_sessions");
    assert!(
        lines[2].get("query_broadened").is_none(),
        "no-filter recent_* must omit query_broadened: {}",
        lines[2]
    );

    assert_eq!(lines[3]["tool"], "check");
    assert!(
        lines[3].get("query_broadened").is_none(),
        "check must omit query_broadened: {}",
        lines[3]
    );
}

/// Phase-1 grounded-counterfactual capture: `returned_full_tokens` (the summed
/// untruncated read-size of the returned hits) is recorded on an FTS5 retrieval
/// path with a positive value, and absent off it (no-filter `recent_*`, `check`)
/// — gated exactly like `query_broadened` so non-retrieval rows stay clean.
/// Record-only: this does NOT change any live savings figure.
#[tokio::test]
async fn emission_returned_full_tokens_recorded_on_fts5_paths_only() {
    let pool = fresh_pool().await;
    seed_all(&pool).await;
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("rft.jsonl");
    let server = server_with_jsonl_sink(pool, &path);

    // (1) FTS5 retrieval with a real hit → present-and-positive.
    let _ = server
        .find_session(Parameters(FindSessionRequest {
            query: "memory extractor".into(),
            limit: Some(5),
        }))
        .await
        .expect("find_session ok");
    // (2) no-filter recent_sessions → not an FTS5 path.
    let _ = server
        .recent_sessions(Parameters(RecentSessionsRequest {
            filter: None,
            days: Some(7),
            limit: Some(5),
        }))
        .await
        .expect("recent_sessions ok");
    // (3) check → no retrieval.
    let _ = server
        .check(Parameters(CheckRequest::default()))
        .await
        .expect("check ok");

    let lines = read_jsonl(&path);
    assert_eq!(lines.len(), 3, "one emission per call");

    assert_eq!(lines[0]["tool"], "find_session");
    assert!(
        lines[0]["returned_full_tokens"].as_u64().is_some_and(|n| n > 0),
        "FTS5 hit must record a positive returned_full_tokens: {}",
        lines[0]
    );

    assert_eq!(lines[1]["tool"], "recent_sessions");
    assert!(
        lines[1].get("returned_full_tokens").is_none(),
        "no-filter recent_* must omit returned_full_tokens: {}",
        lines[1]
    );

    assert_eq!(lines[2]["tool"], "check");
    assert!(
        lines[2].get("returned_full_tokens").is_none(),
        "check must omit returned_full_tokens: {}",
        lines[2]
    );
}

#[tokio::test]
async fn emission_recent_commits_writes_one_line() {
    let pool = fresh_pool().await;
    seed_all(&pool).await;
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("d7.jsonl");
    let server = server_with_jsonl_sink(pool, &path);

    let _ = server
        .recent_commits(Parameters(RecentCommitsRequest {
            filter: Some("rustFetch".into()),
            days: Some(7),
            repo: None,
            limit: Some(5),
        }))
        .await
        .expect("recent_commits ok");

    let lines = read_jsonl(&path);
    assert_eq!(lines.len(), 1);
    let l = &lines[0];
    assert_d72_envelope(l, "recent_commits");
    assert_eq!(l["query"], "rustFetch");
    assert_eq!(l["params"]["filter_set"], true);
    assert_eq!(l["candidate_count"]["fts5"], 1);
}

#[tokio::test]
async fn emission_find_commit_writes_one_line() {
    let pool = fresh_pool().await;
    seed_all(&pool).await;
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("d7.jsonl");
    let server = server_with_jsonl_sink(pool, &path);

    let _ = server
        .find_commit(Parameters(FindCommitRequest {
            query: "bb8".into(),
            repo: None,
            limit: Some(5),
        }))
        .await
        .expect("find_commit ok");

    let lines = read_jsonl(&path);
    assert_eq!(lines.len(), 1);
    let l = &lines[0];
    assert_d72_envelope(l, "find_commit");
    assert_eq!(l["query"], "bb8");
    assert_eq!(l["candidate_count"]["fts5"], 1);
}

#[tokio::test]
async fn emission_find_memory_writes_one_line() {
    let pool = fresh_pool().await;
    seed_all(&pool).await;
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("d7.jsonl");
    let server = server_with_jsonl_sink(pool, &path);

    let _ = server
        .find_memory(Parameters(FindMemoryRequest {
            query: "hydration".into(),
            limit: Some(5),
        }))
        .await
        .expect("find_memory ok");

    let lines = read_jsonl(&path);
    assert_eq!(lines.len(), 1);
    let l = &lines[0];
    assert_d72_envelope(l, "find_memory");
    assert_eq!(l["query"], "hydration");
    assert_eq!(l["candidate_count"]["fts5"], 1);
}

#[tokio::test]
async fn emission_find_design_doc_writes_one_line() {
    let pool = fresh_pool().await;
    seed_all(&pool).await;
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("d7.jsonl");
    let server = server_with_jsonl_sink(pool, &path);

    let _ = server
        .find_design_doc(Parameters(FindDesignDocRequest {
            query: "F45 path comparison".into(),
            limit: Some(5),
        }))
        .await
        .expect("find_design_doc ok");

    let lines = read_jsonl(&path);
    assert_eq!(lines.len(), 1);
    let l = &lines[0];
    assert_d72_envelope(l, "find_design_doc");
    assert_eq!(l["query"], "F45 path comparison");
    assert_eq!(l["candidate_count"]["fts5"], 1);
}

/// G7.4 — fire all 7 tools sequentially against the same sink; every
/// line round-trips into the documented shape; check() has the
/// zero-fts5 + null-query carve-out documented in D-7.3.
#[tokio::test]
async fn emission_all_7_tools_share_consistent_schema() {
    let pool = fresh_pool().await;
    seed_all(&pool).await;
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("d7.jsonl");
    let server = server_with_jsonl_sink(pool, &path);

    let _ = server
        .recent_sessions(Parameters(RecentSessionsRequest {
            filter: Some("rustFetch".into()),
            days: Some(7),
            limit: Some(5),
        }))
        .await
        .expect("recent_sessions");
    let _ = server
        .find_session(Parameters(FindSessionRequest {
            query: "rustFetch".into(),
            limit: Some(5),
        }))
        .await
        .expect("find_session");
    let _ = server
        .recent_commits(Parameters(RecentCommitsRequest {
            filter: Some("rustFetch".into()),
            days: Some(7),
            repo: None,
            limit: Some(5),
        }))
        .await
        .expect("recent_commits");
    let _ = server
        .find_commit(Parameters(FindCommitRequest {
            query: "bb8".into(),
            repo: None,
            limit: Some(5),
        }))
        .await
        .expect("find_commit");
    let _ = server
        .find_memory(Parameters(FindMemoryRequest {
            query: "hydration".into(),
            limit: Some(5),
        }))
        .await
        .expect("find_memory");
    let _ = server
        .find_design_doc(Parameters(FindDesignDocRequest {
            query: "F45 path comparison".into(),
            limit: Some(5),
        }))
        .await
        .expect("find_design_doc");
    let _ = server
        .check(Parameters(CheckRequest::default()))
        .await
        .expect("check");

    let lines = read_jsonl(&path);
    assert_eq!(lines.len(), 7, "one line per tool call");

    let tools = [
        "recent_sessions",
        "find_session",
        "recent_commits",
        "find_commit",
        "find_memory",
        "find_design_doc",
        "check",
    ];
    for (line, expected) in lines.iter().zip(tools.iter()) {
        assert_d72_envelope(line, expected);
    }

    // check() carve-out: no FTS5 + null query + zero candidate_count.
    let check_line = &lines[6];
    assert_eq!(check_line["query"], serde_json::Value::Null);
    assert_eq!(check_line["candidate_count"]["fts5"], 0);
    assert_eq!(check_line["candidate_count"]["after_rank"], 0);
    // G7.5 stages_ms sanity: sum should not exceed 1.5× wall_ms.
    for line in &lines {
        let wall = line["wall_ms"].as_u64().unwrap();
        let stages_sum: u64 = ["fts5_query", "rank", "join", "shape_response"]
            .iter()
            .map(|k| line["stages_ms"][k].as_u64().unwrap_or(0))
            .sum();
        // Allow generous slack — stages are best-effort wall splits;
        // strict ≤ wall_ms can fail on macOS jitter at sub-ms scales.
        assert!(
            stages_sum <= wall.saturating_mul(3).max(20),
            "stages sum {stages_sum} exceeds 3× wall {wall} for {line}"
        );
    }
}

// ===== Day 8.5 G7.7 — error path emits one JSONL line per failed call =====

/// G7.7 — A `find_session` call with malformed FTS5 (canonical
/// unclosed-quote case shared with `recent_sessions_invalid_fts5_errors`
/// at line 2778) emits exactly one JSONL line with `outcome: "error"`,
/// structured `error: {kind, message}`, zeroed candidate_count, and
/// the full Layer-1 envelope (schema_version=1, tool, query, params,
/// stages, wall_ms, daemon_uptime_s, ts, calibration_confidence).
/// The handler still returns an MCP error to the client — the JSONL
/// row is purely observation, not policy (D-7.6).
#[tokio::test]
async fn find_session_error_emits_jsonl_line() {
    let pool = fresh_pool().await;
    seed_all(&pool).await;
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("g77.jsonl");
    let server = server_with_jsonl_sink(pool, &path);

    // Unclosed quote → FTS5 syntax error. Reuses the canonical
    // bad-MATCH case from `recent_sessions_invalid_fts5_errors`.
    let result = server
        .find_session(Parameters(FindSessionRequest {
            query: "\"unclosed".into(),
            limit: Some(5),
        }))
        .await;
    assert!(result.is_err(), "expected MCP-level error from bad FTS5");

    let lines = read_jsonl(&path);
    assert_eq!(lines.len(), 1, "exactly one JSONL line per failed call");
    let l = &lines[0];

    assert_eq!(l["schema_version"], 1);
    assert_eq!(l["tool"], "find_session");
    assert_eq!(l["query"], "\"unclosed");
    assert_eq!(l["outcome"], "error");
    // Bucket is one of the three documented kinds. Exact text depends
    // on sqlx-sqlite's wrapper format, which is outside our control —
    // assert the invariant ("classifier produces a documented bucket")
    // rather than a specific match.
    let kind = l["error"]["kind"].as_str().unwrap();
    assert!(
        ["fts5_syntax", "sqlite", "internal"].contains(&kind),
        "error.kind must be a documented bucket; got {kind}"
    );
    let message = l["error"]["message"].as_str().unwrap();
    assert!(
        !message.is_empty() && message.chars().count() <= 500,
        "error.message must be non-empty + capped at 500 chars; got {message:?}"
    );
    assert_eq!(l["candidate_count"]["fts5"], 0);
    assert_eq!(l["candidate_count"]["after_rank"], 0);
    assert_eq!(l["result_token_estimate"], 0);
    assert_eq!(l["calibration_confidence"], "estimated");
    // 4-key stages_ms object still present so consumers don't need to
    // special-case error rows.
    for stage in ["fts5_query", "rank", "join", "shape_response"] {
        assert!(l["stages_ms"].get(stage).is_some(), "missing stage: {stage}");
    }
}

/// G7.7 — Mixed error-then-success sequence produces two distinct
/// JSONL lines: line 1 carries `outcome: "error"`, line 2 is a
/// byte-identical Day-7-era success envelope (no `outcome` / `error`
/// keys). Guards the additive-not-breaking invariant under a real
/// workload pattern Day 9 will exercise.
#[tokio::test]
async fn find_session_success_after_error_still_emits_clean_envelope() {
    let pool = fresh_pool().await;
    seed_all(&pool).await;
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("g77-seq.jsonl");
    let server = server_with_jsonl_sink(pool, &path);

    // 1) Error call.
    let _ = server
        .find_session(Parameters(FindSessionRequest {
            query: "\"unclosed".into(),
            limit: Some(5),
        }))
        .await;
    // 2) Success call against the same sink.
    let _ = server
        .find_session(Parameters(FindSessionRequest {
            query: "rustFetch".into(),
            limit: Some(5),
        }))
        .await
        .expect("success arm should work after a prior error");

    let lines = read_jsonl(&path);
    assert_eq!(lines.len(), 2, "one line per call regardless of outcome");

    // Line 1: error — outcome present, kind is a documented bucket.
    assert_eq!(lines[0]["outcome"], "error");
    let kind = lines[0]["error"]["kind"].as_str().unwrap();
    assert!(["fts5_syntax", "sqlite", "internal"].contains(&kind));

    // Line 2: success — outcome + error keys absent
    // (skip_serializing_if), G7.4 12-key schema intact.
    assert!(
        lines[1].get("outcome").is_none(),
        "success envelope must not carry `outcome`: {}",
        lines[1]
    );
    assert!(
        lines[1].get("error").is_none(),
        "success envelope must not carry `error`: {}",
        lines[1]
    );
    assert_d72_envelope(&lines[1], "find_session");
    assert_eq!(lines[1]["query"], "rustFetch");
    assert!(lines[1]["result_token_estimate"].as_u64().unwrap() > 0);
}
