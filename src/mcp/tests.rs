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
    // gnuphie-labs#21: a memory hit must say WHERE it came from. Without this the
    // corpus is a flat namespace keyed on `name`, and an entry under a subdirectory
    // — `_archive/` for retired entries is a real convention — is indistinguishable
    // from a live one. `documents.path` always held it; the query did not join it.
    assert!(
        envelope.results[0].path.ends_with(".md"),
        "a memory hit carries its file path, got {:?}",
        envelope.results[0].path
    );
}

/// The consequence #21 is actually about: two entries whose `name`, `memory_type`
/// and `description` give the caller nothing to choose between, distinguished only
/// by the directory they live in. If the path is dropped, retired guidance and live
/// guidance are the same object to a caller.
#[tokio::test]
async fn find_memory_distinguishes_an_archived_entry_by_its_path() {
    let pool = fresh_pool().await;
    seed_all(&pool).await;

    for (path, name) in [
        ("/ws/memory/feedback-vendor-x.md", "feedback-vendor-x"),
        ("/ws/memory/_archive/feedback-vendor-x-old.md", "feedback-vendor-x-old"),
    ] {
        let doc: (i64,) = sqlx::query_as(
            "INSERT INTO documents (path, kind, mtime, content_hash, indexed_at) \
             VALUES (?, 'memory', 0, 'h', 0) RETURNING id",
        )
        .bind(path)
        .fetch_one(&pool)
        .await
        .unwrap();
        let entry: (i64,) = sqlx::query_as(
            "INSERT INTO memory_entries (document_id, name, memory_type, description, body) \
             VALUES (?, ?, 'feedback', 'vendor guidance', 'vendorx authentication guidance') \
             RETURNING id",
        )
        .bind(doc.0)
        .bind(name)
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO search_index (body, source_table, rowid_ref) \
             VALUES ('vendorx authentication guidance', 'memory_entries', ?)",
        )
        .bind(entry.0)
        .execute(&pool)
        .await
        .unwrap();
    }

    let envelope = run_find_memory(
        &pool,
        &FindMemoryRequest { query: "vendorx".into(), limit: Some(10) },
        &mut Stages::default(),
    )
    .await
    .unwrap();

    assert_eq!(envelope.results.len(), 2, "both entries match the query");
    let archived: Vec<_> = envelope
        .results
        .iter()
        .filter(|r| r.path.contains("/_archive/"))
        .collect();
    assert_eq!(
        archived.len(),
        1,
        "the archived entry is identifiable FROM THE RESPONSE, not by opening files"
    );
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
    // A hit whose body survived the budget carries no excerpt: the body opens with
    // those same characters, so emitting both would pay twice to say it once.
    assert!(!envelope.results[0].body.is_empty());
    assert!(envelope.results[0].body_excerpt.is_empty());
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
        &FindCodeRequest { query: "provenance".into(), repo: None, limit: Some(5) },
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
    // Body present ⇒ no excerpt (see `find_design_doc` above for the reasoning).
    assert!(!hit.body.is_empty());
    assert!(hit.body_excerpt.is_empty());
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
        &FindCodeRequest { query: "freshcheck".into(), repo: None, limit: Some(5) },
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
        &FindCodeRequest { query: "relocheck".into(), repo: None, limit: Some(5) },
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
        &FindCodeRequest { query: "no_such_symbol_anywhere".into(), repo: None, limit: None },
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
        &FindCodeRequest { query: "DEEPCODETOKEN".into(), repo: None, limit: Some(5)  },
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
    // The other half of the same contract: while the body is present the excerpt is
    // pure duplication and must not be paid for. Asserting only the `dropped` half
    // would let an unconditional excerpt back in without failing anything.
    let kept: Vec<_> = envelope.results.iter().filter(|r| !r.body.is_empty()).collect();
    assert!(!kept.is_empty(), "some hits must fit inside the budget for this to test anything");
    for r in kept {
        assert!(r.body_excerpt.is_empty(), "a present body must not also ship an excerpt");
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
/// `check().orphans.source_chunks` counts chunks whose file is gone from disk
/// and stays 0 for a file that exists. Before this class existed a deleted file
/// left `check()` reporting all-zero orphans while `find_code` returned it as
/// `file_missing` (RC1 review 1.6). Mutation this catches: mapping
/// `OrphanChild::SourceChunk` to another table, or dropping the class.
#[tokio::test]
async fn check_counts_source_chunk_orphans_by_missing_file() {
    let pool = fresh_pool().await;
    let dir = tempfile::tempdir().unwrap();
    let live = dir.path().join("live.rs");
    std::fs::write(&live, "fn live() {}\n").unwrap();
    let gone = dir.path().join("gone.rs");
    for (path, n_chunks) in [(&live, 1), (&gone, 2)] {
        let doc_id: i64 = sqlx::query_scalar(
            "INSERT INTO documents (path, kind, content_hash, mtime, indexed_at) \
             VALUES (?, 'source', 'h', 0, 0) RETURNING id",
        )
        .bind(path.to_string_lossy().into_owned())
        .fetch_one(&pool)
        .await
        .unwrap();
        for i in 0..n_chunks {
            sqlx::query(
                "INSERT INTO source_chunks (document_id, path, line_start, line_end, body) \
                 VALUES (?, 'x.rs', ?, ?, 'body')",
            )
            .bind(doc_id)
            .bind(i * 50 + 1)
            .bind(i * 50 + 50)
            .execute(&pool)
            .await
            .unwrap();
        }
    }
    let result = run_check(&pool, 0, None, &mut Stages::default()).await.unwrap();
    assert_eq!(result.orphans.source_chunks, 2, "only the missing file's chunks are orphans");
    assert_eq!(result.orphans.memory_entries, 0);
    assert_eq!(result.orphans.design_doc_sections, 0);
}

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

// ---- corpus_empty / corpus_indexed_through -------------------------------
// "Absence is not a miss": an empty `results` array cannot say WHICH kind of
// empty it is, and a caller that guesses wrong makes a decision on a fact
// nibdex never established.

/// The case the fold-in of session indexing was fixing, seen from the query
/// side: a corpus with nothing in it says so, rather than handing back a bare
/// empty array that reads like "this workspace has no such thing".
#[tokio::test]
async fn find_session_reports_an_empty_corpus_as_empty() {
    let pool = fresh_pool().await; // migrations only — nothing indexed

    let envelope = run_find_session(
        &pool,
        &FindSessionRequest {
            query: "anything".into(),
            limit: Some(5),
        },
        &mut Stages::default(),
    )
    .await
    .unwrap();

    assert_eq!(envelope.total_matched, 0);
    assert_eq!(
        envelope.corpus_empty,
        Some(true),
        "a zero-result response over an EMPTY corpus must say so"
    );
    assert!(
        envelope.corpus_indexed_through.is_none(),
        "an empty corpus has no newest item to report"
    );

    let json = serde_json::to_value(&envelope).unwrap();
    assert_eq!(json["corpus_empty"], serde_json::json!(true));
    assert!(json.get("corpus_indexed_through").is_none());
}

/// The other half of the distinction: the corpus HOLDS rows, this query just
/// matched none of them. `corpus_empty: false` plus a freshness stamp says
/// "nibdex looked, and here is how current what it looked at was."
#[tokio::test]
async fn find_session_distinguishes_no_match_from_no_corpus() {
    let pool = fresh_pool().await;
    seed_all(&pool).await;

    let envelope = run_find_session(
        &pool,
        &FindSessionRequest {
            query: "zzzznosuchtoken".into(),
            limit: Some(5),
        },
        &mut Stages::default(),
    )
    .await
    .unwrap();

    assert_eq!(envelope.total_matched, 0);
    assert_eq!(
        envelope.corpus_empty,
        Some(false),
        "the corpus has rows — this is a genuine miss, not an empty index"
    );
    let through = envelope
        .corpus_indexed_through
        .as_deref()
        .expect("a non-empty corpus reports how current it is");
    assert!(
        through.starts_with("20") && through.ends_with('Z'),
        "expected an ISO-8601 UTC stamp, got {through:?}"
    );
}

/// A hit path pays nothing and says nothing — the fields exist to explain an
/// absence, so on a response that HAS results they must be absent themselves.
/// This is what keeps the addition purely additive (no `schema_version` bump).
#[tokio::test]
async fn corpus_diagnosis_is_absent_when_results_are_returned() {
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

    assert!(envelope.returned > 0);
    assert!(envelope.corpus_empty.is_none());
    assert!(envelope.corpus_indexed_through.is_none());

    let json = serde_json::to_value(&envelope).unwrap();
    assert!(json.get("corpus_empty").is_none());
    assert!(json.get("corpus_indexed_through").is_none());
}

/// Every `find_*` tool gets the diagnosis, not just `find_session` — the
/// silent-zero family was five separately-filed items because each tool was
/// looked at on its own.
#[tokio::test]
async fn every_find_tool_diagnoses_its_own_empty_corpus() {
    let pool = fresh_pool().await; // nothing indexed, all five corpora empty
    let mut stages = Stages::default();
    let q = "zzzznosuchtoken";

    let session = run_find_session(
        &pool,
        &FindSessionRequest { query: q.into(), limit: Some(5) },
        &mut stages,
    )
    .await
    .unwrap();
    let commit = run_find_commit(
        &pool,
        &FindCommitRequest { query: q.into(), limit: Some(5), repo: None },
        &mut stages,
    )
    .await
    .unwrap();
    let memory = run_find_memory(
        &pool,
        &FindMemoryRequest { query: q.into(), limit: Some(5) },
        &mut stages,
    )
    .await
    .unwrap();
    let design = run_find_design_doc(
        &pool,
        &FindDesignDocRequest { query: q.into(), limit: Some(5) },
        &mut stages,
    )
    .await
    .unwrap();
    let code = run_find_code(
        &pool,
        &FindCodeRequest { query: q.into(), repo: None, limit: Some(5)  },
        &mut stages,
    )
    .await
    .unwrap();

    for (tool, empty) in [
        ("find_session", session.corpus_empty),
        ("find_commit", commit.corpus_empty),
        ("find_memory", memory.corpus_empty),
        ("find_design_doc", design.corpus_empty),
        ("find_code", code.corpus_empty),
    ] {
        assert_eq!(empty, Some(true), "{tool} did not diagnose its empty corpus");
    }
}

/// `check()` names a deliberately-retired corpus instead of letting its rows read
/// as index damage. `session_entries` holds data from a CLAUDE.md format nothing
/// writes any more; on a real second corpus every surviving row was orphaned
/// while `session_edges` was healthy and serving. The number stays true and
/// visible — what changes is that it is now interpretable.
#[tokio::test]
async fn check_names_retired_corpora_rather_than_flagging_them() {
    let pool = fresh_pool().await;
    seed_all(&pool).await;

    let result = run_check(&pool, 1, None, &mut Stages::default()).await.unwrap();

    assert!(
        result.indexer.session_entries > 0,
        "fixture must actually have legacy rows for this to mean anything"
    );
    let retired = result
        .retired_corpora
        .as_deref()
        .unwrap_or_default()
        .iter()
        .find(|r| r.corpus == "session_entries")
        .cloned()
        .expect("a populated legacy corpus must be named as retired");
    assert_eq!(retired.rows, result.indexer.session_entries);
    assert_eq!(retired.superseded_by, "session_edges");
}

/// ... and a workspace that never had one gets a clean `check()` — the field is
/// absent from the JSON entirely, so no archaeology appears on a fresh install.
#[tokio::test]
async fn check_omits_retired_corpora_when_there_are_none() {
    let pool = fresh_pool().await;

    let result = run_check(&pool, 1, None, &mut Stages::default()).await.unwrap();

    assert_eq!(result.indexer.session_entries, 0);
    assert!(result.retired_corpora.is_none());
    let json = serde_json::to_value(&result).unwrap();
    assert!(json.get("retired_corpora").is_none());
}

/// THE CLASS GATE. Every field a tool's `outputSchema` marks `required` must
/// actually appear in the JSON that tool emits.
///
/// `schemars` derives `required` from the Rust type and is blind to serde's
/// `skip_serializing_if`, so any field pairing a skip attribute with a type that
/// is neither `Option` nor `#[serde(default)]` is advertised as mandatory and then
/// omitted — breaking the advertised contract for a validating client. Two
/// separate fields have now shipped with exactly that shape (`retired_corpora`,
/// `query_broadened`); the second put 7 of 8 tools in violation on ordinary
/// successful responses, so this gates the shape rather than the instances.
///
/// The envelope is built in its QUIET state — every omissible field at the value
/// that triggers omission — because that is the response shape the defect hides in.
#[test]
fn envelope_emits_every_field_its_schema_requires() {
    let envelope: ToolEnvelope<CodeResult> = ToolEnvelope {
        results: vec![],
        total_matched: 0,
        returned: 0,
        tool: "find_code".to_string(),
        query_broadened: false,
        corpus_empty: None,
        corpus_indexed_through: None,
        returned_full_tokens: 0,
        neighbourhood_terms: Vec::new(),
        retrieval_shape: None,
        also_matched: Vec::new(),
    };

    let schema = serde_json::to_value(schemars::schema_for!(ToolEnvelope<CodeResult>)).unwrap();
    let required: Vec<&str> = schema
        .get("required")
        .and_then(|r| r.as_array())
        .expect("the envelope schema declares required fields")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert!(
        required.contains(&"results"),
        "sanity: the schema probe itself must be reading real required fields, got {required:?}"
    );

    let emitted = serde_json::to_value(&envelope).unwrap();
    let missing: Vec<&str> =
        required.iter().copied().filter(|f| emitted.get(*f).is_none()).collect();
    assert!(
        missing.is_empty(),
        "outputSchema requires {missing:?} but a quiet response omits them; \
         give each field `Option<_>` or `#[serde(default)]` so schemars stops \
         advertising it as mandatory. Emitted: {emitted}"
    );
}

/// A malformed query must come back as an ACTIONABLE error, not raw sqlx text.
///
/// `find_code("parse_config(")` is an ordinary thing to ask; `(` is FTS5 grouping
/// syntax, so SQLite rejects the query. The old reply was
/// `error returned from database: (code: 1) fts5: syntax error near ""`, which
/// names no offending character and suggests no repair — a caller (human or
/// model) learns only that the tool failed, and reaches for grep instead.
#[test]
fn a_malformed_query_error_tells_the_caller_how_to_fix_it() {
    let raw = "error returned from database: (code: 1) fts5: syntax error near \"\"";
    let explained = explain_query_error(raw);

    assert!(
        explained.contains("FTS5"),
        "must name the query language the caller got wrong: {explained}"
    );
    assert!(
        explained.contains('"'),
        "must show the repair — quoting the literal term: {explained}"
    );
    assert!(
        explained.contains("NOT an empty or broken index"),
        "must separate 'you typed it wrong' from 'the index is broken': {explained}"
    );
    for leak in ["sqlx", "code: 1", "error returned from database"] {
        assert!(
            !explained.contains(leak),
            "must not leak the storage engine ({leak}): {explained}"
        );
    }
}

/// ... and it must NOT swallow a genuine failure into a syntax lecture.
///
/// The whole point of an actionable error is telling the caller which of the two
/// situations they are in. A rewrite that fires on every error destroys exactly
/// the distinction it was added to create.
#[test]
fn a_real_database_failure_is_not_rewritten_as_a_syntax_complaint() {
    for real in [
        "error returned from database: disk I/O error",
        "no such table: source_chunks",
        "database is locked",
    ] {
        assert_eq!(
            explain_query_error(real),
            real,
            "a genuine failure must reach the caller unchanged"
        );
    }
}

/// The end-to-end shape: the query layer really does reject this, so the
/// explanation is reachable rather than theoretical.
#[tokio::test]
async fn an_unbalanced_paren_query_actually_fails_and_is_explained() {
    let pool = fresh_pool().await;
    seed_all(&pool).await;

    let err = run_find_code(
        &pool,
        &FindCodeRequest { query: "parse_config(".into(), repo: None, limit: Some(5)  },
        &mut Stages::default(),
    )
    .await
    .expect_err("an unbalanced paren is rejected by FTS5, not silently repaired");

    let explained = explain_query_error(&err.to_string());
    assert!(
        explained.contains("FTS5") && !explained.contains("code: 1"),
        "the live error path must reach the actionable message: {explained}"
    );
}

/// `corpus_indexed_through` must report the newest thing the corpus CONTAINS,
/// not when indexing last ran.
///
/// `documents.indexed_at` is the tempting source and is wrong: the
/// write-amplification fix deliberately skips unchanged files without refreshing
/// it, so a repo whose source has not changed in months reports a months-old
/// stamp on a freshly-built index — telling the caller their index is stale when
/// it is current, the exact misdiagnosis this field exists to prevent.
#[tokio::test]
async fn corpus_indexed_through_is_content_time_not_index_time() {
    let pool = fresh_pool().await;

    let fresh_mtime: i64 = 1_800_000_000;
    let stale_indexed_at: i64 = 1_000_000_000;
    sqlx::query(
        "INSERT INTO documents (path, kind, content_hash, mtime, indexed_at) \
         VALUES ('/ws/src/lib.rs', 'source', 'h', ?, ?)",
    )
    .bind(fresh_mtime)
    .bind(stale_indexed_at)
    .execute(&pool)
    .await
    .unwrap();
    let doc_id: i64 = sqlx::query_scalar("SELECT id FROM documents WHERE path='/ws/src/lib.rs'")
        .fetch_one(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO source_chunks (document_id, path, line_start, line_end, body) \
         VALUES (?, '/ws/src/lib.rs', 1, 10, 'fn alpha() {}')",
    )
    .bind(doc_id)
    .execute(&pool)
    .await
    .unwrap();

    let envelope = run_find_code(
        &pool,
        &FindCodeRequest { query: "qqxwvzzblorptunk".into(), repo: None, limit: Some(5)  },
        &mut Stages::default(),
    )
    .await
    .unwrap();

    assert_eq!(envelope.total_matched, 0);
    assert_eq!(envelope.corpus_empty, Some(false));
    let through = envelope.corpus_indexed_through.expect("non-empty corpus reports freshness");
    assert_eq!(
        through,
        format::unix_to_iso(fresh_mtime),
        "reported the stale indexed_at instead of the file's real mtime"
    );
}

/// The commits corpus reports freshness from `committed_at`, never `authored_at`.
///
/// The author date is caller-supplied (`git commit --date`) and is PRESERVED by a
/// rebase, so it records when the work was written rather than when this history
/// came to hold it. A rebased or cherry-picked branch is the ordinary case: every
/// commit keeps its original author date while git stamps a fresh committer date,
/// so reading the wrong column reports a corpus indexed moments ago as months
/// stale — the exact misdiagnosis this field exists to prevent. The pathological
/// case is worse: one future-dated commit makes the corpus claim, permanently,
/// that it contains content from the future.
#[tokio::test]
async fn commit_freshness_reads_committer_date_not_author_date() {
    let pool = fresh_pool().await;
    // A rebased commit: authored years ago, committed just now. The two columns
    // must be far enough apart that reading the wrong one is unmistakable.
    let authored = 1_600_000_000_i64;
    let committed = 1_800_000_000_i64;
    sqlx::query(
        "INSERT INTO commit_entries \
           (repo_path, commit_hash, author_email, author_name, authored_at, \
            committed_at, message_summary, files_changed) \
         VALUES ('/ws/proj', 'ccc3333', 'a@b', 'a', ?, ?, 'rebased work', '[]')",
    )
    .bind(authored)
    .bind(committed)
    .execute(&pool)
    .await
    .unwrap();

    // A query that matches nothing, because the freshness probe fires only on a
    // zero-result response — the one moment the caller cannot tell an empty
    // corpus from a missed query.
    let envelope = run_find_commit(
        &pool,
        &FindCommitRequest { query: "zzz_no_such_term".into(), repo: None, limit: Some(10) },
        &mut Stages::default(),
    )
    .await
    .unwrap();

    assert_eq!(envelope.total_matched, 0, "the probe only runs on a zero-result response");
    assert_eq!(envelope.corpus_empty, Some(false), "the corpus holds a row");
    let through = envelope.corpus_indexed_through.expect("a non-empty corpus reports freshness");
    assert_eq!(
        through,
        unix_to_iso(committed),
        "freshness must come from the committer date — when this history took the commit"
    );
    assert_ne!(
        through,
        unix_to_iso(authored),
        "reading authored_at reports a just-rebased corpus as years stale"
    );
}

/// A repo whose name contains a LIKE wildcard must be filterable by its real
/// name, and a wildcard typed by the caller must stay literal.
///
/// `like_substring` escapes `%`/`_`/`\` with a backslash, but that only works if
/// the statement declares `ESCAPE '\'` — without it SQLite treats the backslash
/// as an ordinary character, so `proj\_x` can never match `proj_x` and the filter
/// silently returns nothing. Underscores in repo names are ubiquitous, and this
/// release makes the miss worse by stamping `corpus_empty: false` on it — telling
/// the caller with new authority that their query simply missed a populated
/// corpus, which is the confident-wrong-diagnosis the field exists to prevent.
#[tokio::test]
async fn repo_filter_matches_underscores_and_keeps_wildcards_literal() {
    let pool = fresh_pool().await;
    for (repo, hash, summary) in [
        ("/ws/proj_x", "aaa1111", "underscore repo commit"),
        ("/ws/projyx", "bbb2222", "decoy that a bare _ would match"),
    ] {
        sqlx::query(
            "INSERT INTO commit_entries \
               (repo_path, commit_hash, author_email, author_name, authored_at, \
                committed_at, message_summary, files_changed) \
             VALUES (?, ?, 'a@b', 'a', 1800000000, 1800000000, ?, '[]')",
        )
        .bind(repo)
        .bind(hash)
        .bind(summary)
        .execute(&pool)
        .await
        .unwrap();
    }

    let recent = |repo: &str| {
        let pool = pool.clone();
        let repo = repo.to_string();
        async move {
            run_recent_commits(
                &pool,
                &RecentCommitsRequest {
                    filter: None,
                    repo: Some(repo),
                    days: Some(36500),
                    limit: Some(10),
                },
                &mut Stages::default(),
            )
            .await
            .unwrap()
        }
    };

    // The real name matches its own repo ...
    let exact = recent("proj_x").await;
    assert_eq!(exact.total_matched, 1, "an underscore repo name must be filterable");
    assert_eq!(exact.results[0].repo_path, "/ws/proj_x");

    // ... and `_` is NOT treated as a single-character wildcard, so the decoy
    // `projyx` is excluded rather than swept in.
    assert!(
        exact.results.iter().all(|r| r.repo_path != "/ws/projyx"),
        "`_` leaked through as a wildcard and matched a different repo"
    );

    // A caller-typed `%` stays literal too.
    assert_eq!(recent("proj%x").await.total_matched, 0);
}

/// `check()` reports the DENOMINATOR — retrieval nibdex did not serve.
///
/// Every other instrument in nibdex fires only when a nibdex tool is called, so
/// they are survivorship-biased by construction: `cost_savings` can only ever
/// report good news, and a period of total non-use looks identical to a quiet
/// one. This is the only field that can say "you have a problem".
#[tokio::test]
async fn check_reports_retrieval_nibdex_did_not_serve() {
    let pool = fresh_pool().await;

    // Absent entirely before any session activity is indexed — a fresh install
    // gets a clean check(), not a misleading 0%.
    let empty = run_check(&pool, 1, None, &mut Stages::default()).await.unwrap();
    assert!(empty.adoption.is_none());
    let json = serde_json::to_value(&empty).unwrap();
    assert!(json.get("adoption").is_none());

    for (sid, retrieval, nibdex) in [("s1", 88, 0), ("s2", 39, 0), ("s3", 10, 10)] {
        sqlx::query(
            "INSERT INTO session_activity \
               (session_id, first_seen, retrieval_calls, nibdex_calls) VALUES (?, 1, ?, ?)",
        )
        .bind(sid)
        .bind(retrieval)
        .bind(nibdex)
        .execute(&pool)
        .await
        .unwrap();
    }

    let a = run_check(&pool, 1, None, &mut Stages::default())
        .await
        .unwrap()
        .adoption
        .expect("adoption must be reported once session activity exists");

    assert_eq!(a.sessions_seen, 3);
    assert_eq!(a.sessions_using_nibdex, 1, "only s3 ever called nibdex");
    assert_eq!(a.retrieval_elsewhere, 137, "88 + 39 + 10 went elsewhere");
    assert_eq!(a.nibdex_queries, 10);
    // 10 of 147 total retrieval calls.
    assert!(
        (a.nibdex_share_pct - 6.8).abs() < 0.05,
        "share should be ~6.8%, got {}",
        a.nibdex_share_pct
    );
}

// ---- find_code repo scope -------------------------------------------------
//
// `source_chunks.path` is repo-relative by design (it joins to
// `commit_entries.files_changed`). Without the repo alongside it, a hit is not
// openable on a multi-repo index and identical paths in different trees are
// indistinguishable — the gap that forced `nibdex hook` to tail-match a
// directory name, which admits any repo's `src/`.

/// Seed one source chunk in a named repo. Returns nothing; the caller queries.
async fn seed_chunk_in_repo(pool: &SqlitePool, repo: &str, rel: &str, body: &str) {
    let abs = format!("{repo}/{rel}");
    let doc_id: (i64,) = sqlx::query_as(
        "INSERT INTO documents (path, kind, content_hash, mtime, indexed_at) \
         VALUES (?, 'source', 'h', 0, 0) RETURNING id",
    )
    .bind(&abs)
    .fetch_one(pool)
    .await
    .unwrap();
    let chunk_id: (i64,) = sqlx::query_as(
        "INSERT INTO source_chunks \
            (document_id, repo_path, path, line_start, line_end, language, body, last_commit_id) \
         VALUES (?, ?, ?, 1, 50, 'rust', ?, NULL) RETURNING id",
    )
    .bind(doc_id.0)
    .bind(repo)
    .bind(rel)
    .bind(body)
    .fetch_one(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO search_index (body, kind, rowid_ref, source_table) \
         VALUES (?, 'source', ?, 'source_chunks')",
    )
    .bind(body)
    .bind(chunk_id.0)
    .execute(pool)
    .await
    .unwrap();
}

fn find_code_req(query: &str, repo: Option<&str>, limit: i64) -> FindCodeRequest {
    FindCodeRequest {
        query: query.to_string(),
        repo: repo.map(str::to_string),
        limit: Some(limit),
    }
}

/// THE case this column exists for: the same relative path in two repos.
/// Without `repo_path` the two hits are indistinguishable and neither is openable.
#[tokio::test]
async fn find_code_hits_say_which_repo_they_are_in() {
    let pool = fresh_pool().await;
    seed_chunk_in_repo(&pool, "/ws/alpha", "src/main.rs", "fn widget_alpha() {}").await;
    seed_chunk_in_repo(&pool, "/ws/beta", "src/main.rs", "fn widget_beta() {}").await;

    let envelope =
        run_find_code(&pool, &find_code_req("widget", None, 10), &mut Stages::default())
            .await
            .unwrap();

    assert_eq!(envelope.total_matched, 2, "both repos match");
    for r in &envelope.results {
        assert_eq!(r.path, "src/main.rs", "the relative path alone cannot disambiguate");
    }
    let mut repos: Vec<&str> =
        envelope.results.iter().filter_map(|r| r.repo_path.as_deref()).collect();
    repos.sort_unstable();
    assert_eq!(
        repos,
        vec!["/ws/alpha", "/ws/beta"],
        "each hit must carry the repo that makes it openable"
    );
}

/// The `repo` filter scopes both the results AND `total_matched`. A count taken
/// over the whole index while results are scoped would tell the caller there is
/// more to page through than the scope can ever return.
#[tokio::test]
async fn find_code_repo_scope_excludes_other_repos() {
    let pool = fresh_pool().await;
    seed_chunk_in_repo(&pool, "/ws/alpha", "src/main.rs", "fn widget_alpha() {}").await;
    seed_chunk_in_repo(&pool, "/ws/beta", "src/main.rs", "fn widget_beta() {}").await;

    let envelope =
        run_find_code(&pool, &find_code_req("widget", Some("beta"), 10), &mut Stages::default())
            .await
            .unwrap();

    assert_eq!(envelope.total_matched, 1, "the count must describe the SCOPED corpus");
    assert_eq!(envelope.results.len(), 1);
    assert_eq!(envelope.results[0].repo_path.as_deref(), Some("/ws/beta"));
    assert!(
        envelope.results[0].body.contains("beta"),
        "the surviving hit is beta's, not alpha's"
    );
}

/// The scope must be applied INSIDE the query, before `LIMIT` — not to the rows
/// that come back after it.
///
/// This is a defect class that has already shipped once here: filtering a
/// truncated result set makes a CORRECT narrow scope return nothing, and the
/// narrower and more correct the scope, the more reliably it fails. Ranking puts
/// the noise repo's chunks first, so a post-limit filter returns zero.
#[tokio::test]
async fn find_code_repo_scope_is_applied_before_the_limit() {
    let pool = fresh_pool().await;
    for i in 0..20 {
        seed_chunk_in_repo(
            &pool,
            "/ws/noise",
            &format!("src/f{i}.rs"),
            "fn widget_noise() { widget widget widget }",
        )
        .await;
    }
    seed_chunk_in_repo(&pool, "/ws/wanted", "src/only.rs", "fn widget_wanted() {}").await;

    let envelope =
        run_find_code(&pool, &find_code_req("widget", Some("wanted"), 5), &mut Stages::default())
            .await
            .unwrap();

    assert_eq!(
        envelope.total_matched, 1,
        "scoping after the limit would report the noise repo's rows too"
    );
    assert_eq!(envelope.results.len(), 1, "the wanted repo's only hit must survive the limit");
    assert_eq!(envelope.results[0].repo_path.as_deref(), Some("/ws/wanted"));
}

/// A repo whose name contains a LIKE wildcard stays filterable — the same
/// escaping trap already fixed for `recent_commits`, which this filter mirrors.
#[tokio::test]
async fn find_code_repo_scope_matches_underscores_literally() {
    let pool = fresh_pool().await;
    seed_chunk_in_repo(&pool, "/ws/proj_x", "src/a.rs", "fn widget_one() {}").await;
    seed_chunk_in_repo(&pool, "/ws/projyx", "src/b.rs", "fn widget_two() {}").await;

    let envelope =
        run_find_code(&pool, &find_code_req("widget", Some("proj_x"), 10), &mut Stages::default())
            .await
            .unwrap();

    assert_eq!(envelope.total_matched, 1, "an underscore repo name must be filterable");
    assert_eq!(envelope.results[0].repo_path.as_deref(), Some("/ws/proj_x"));
}

/// Each corpus's emptiness probe must count ITS OWN table.
///
/// Proven necessary, not assumed: pointing `Corpus::MemoryEntries` at
/// `session_edges` left all 313 tests green. Nothing anywhere asserted that a
/// corpus counts itself, so four of the five mappings could be silently swapped.
///
/// The consequence is the exact failure this release exists to remove. Seed only
/// memory, and a swapped mapping makes `find_memory` answer `corpus_empty: false`
/// — "the corpus has rows, your query simply missed" — when the corpus it speaks
/// for is empty. That is a confident wrong diagnosis wearing the badge of the
/// field added to prevent confident wrong diagnoses.
///
/// Written behaviourally rather than by asserting the SQL string: a test that
/// compares `probe_sql()` to a literal passes whether or not the literal is the
/// right table, which is how the gap survived in the first place.
#[tokio::test]
async fn each_corpus_probe_counts_its_own_table() {
    // (label, the statements that seed ONLY that corpus)
    let corpora: Vec<(&str, Vec<&str>)> = vec![
        (
            "memory",
            vec![
                "INSERT INTO documents (id, path, kind, content_hash, mtime, indexed_at) \
                 VALUES (1, '/w/m.md', 'memory', 'h', 100, 100)",
                "INSERT INTO memory_entries (document_id, name, memory_type, description, body) \
                 VALUES (1, 'n', 'user', 'd', 'b')",
            ],
        ),
        (
            "session",
            vec![
                "INSERT INTO session_edges \
                   (session_id, message_uuid, tool, file_path, edited_at, rationale) \
                 VALUES ('s', 'u1', 'Edit', '/w/a.rs', 100, 'r')",
            ],
        ),
        (
            "commit",
            vec![
                "INSERT INTO commit_entries \
                   (repo_path, commit_hash, author_email, author_name, authored_at, \
                    committed_at, message_summary, files_changed) \
                 VALUES ('/w', 'h1', 'a@b', 'a', 100, 100, 's', '[]')",
            ],
        ),
    ];

    for (label, seed_stmts) in corpora {
        let pool = fresh_pool().await;
        for stmt in &seed_stmts {
            sqlx::query(stmt).execute(&pool).await.unwrap();
        }

        // The corpus that WAS seeded must report itself non-empty ...
        let own = match label {
            "memory" => {
                run_find_memory(
                    &pool,
                    &FindMemoryRequest { query: "zzz_no_match".into(), limit: Some(5) },
                    &mut Stages::default(),
                )
                .await
                .unwrap()
                .corpus_empty
            }
            "session" => {
                run_find_session(
                    &pool,
                    &FindSessionRequest { query: "zzz_no_match".into(), limit: Some(5) },
                    &mut Stages::default(),
                )
                .await
                .unwrap()
                .corpus_empty
            }
            _ => {
                run_find_commit(
                    &pool,
                    &FindCommitRequest {
                        query: "zzz_no_match".into(),
                        repo: None,
                        limit: Some(5),
                    },
                    &mut Stages::default(),
                )
                .await
                .unwrap()
                .corpus_empty
            }
        };
        assert_eq!(
            own,
            Some(false),
            "{label}: its own corpus holds a row, so the probe must report NOT empty"
        );

        // ... and a corpus that was NOT seeded must still report itself empty.
        // This is the half that catches a swap: a probe reading someone else's
        // table inherits their rows and claims to be populated.
        let untouched = run_find_code(
            &pool,
            &FindCodeRequest { query: "zzz_no_match".into(), repo: None, limit: Some(5) },
            &mut Stages::default(),
        )
        .await
        .unwrap()
        .corpus_empty;
        assert_eq!(
            untouched,
            Some(true),
            "{label} was seeded but source was not; source must not borrow another corpus's rows"
        );
    }
}

// ---- RC1 review 1.10 — gates for filters the suite let through --------------------
//
// Mutation testing during the rc.1 review found these clauses ungated: the
// `days` cutoff (every seeded row was in-window), the `repo` clause on the
// filtered `recent_commits` path and on `find_commit` (no fixture passed
// `repo` there), bm25/recency ORDER BY (single-row fixtures), and the
// `MAX_LIMIT` clamp (asserted by a tautology). Each test here seeds the row
// that the mutation would have let through and asserts it is excluded.

/// Extra rows on top of `seed_all`: an OUT-OF-WINDOW commit that matches the
/// same terms as an in-window one, a second in-window `bb8` commit in the OTHER
/// repo, and an out-of-window session edge.
async fn seed_window_and_scope_probes(pool: &SqlitePool) -> i64 {
    let now: i64 = sqlx::query_scalar("SELECT CAST(strftime('%s','now') AS INTEGER)")
        .fetch_one(pool)
        .await
        .unwrap();
    for (repo, hash, at, summary, fts) in [
        (
            "/tmp/formsvc",
            "cccccccccccccccccccccccccccccccccccccccc",
            now - 40 * 86_400,
            "fix(rustFetch): ancient cleanup",
            "fix rustFetch ancient cleanup",
        ),
        (
            "/tmp/webhooksvc",
            "dddddddddddddddddddddddddddddddddddddddd",
            now - 2 * 3_600,
            "feat(bb8): webhook pool metrics",
            "feat bb8 webhook pool metrics",
        ),
    ] {
        let (id,): (i64,) = sqlx::query_as(
            "INSERT INTO commit_entries \
                (repo_path, commit_hash, parent_hashes, author_email, author_name, \
                 authored_at, committed_at, message_summary, message_body, files_changed) \
             VALUES (?, ?, '[]', 'me@example.com', 'Me', ?, ?, ?, NULL, '[]') RETURNING id",
        )
        .bind(repo)
        .bind(hash)
        .bind(at)
        .bind(at)
        .bind(summary)
        .fetch_one(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO search_index (body, kind, rowid_ref, source_table) \
             VALUES (?, 'commit', ?, 'commit_entries')",
        )
        .bind(fts)
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
    }
    let (eid,): (i64,) = sqlx::query_as(
        "INSERT INTO session_edges \
            (session_id, message_uuid, edge_ordinal, tool, file_path, repo_path, \
             git_branch, edited_at, rationale, commit_id) \
         VALUES ('sess-ancient', 'u-ancient', 0, 'Edit', 'src/auth.rs', '/tmp/webhooksvc', \
                 'main', ?, 'ancient auth work', NULL) RETURNING id",
    )
    .bind(now - 40 * 86_400)
    .fetch_one(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO search_index (body, kind, rowid_ref, source_table) \
         VALUES ('ancient auth work src/auth.rs', 'session_edge', ?, 'session_edges')",
    )
    .bind(eid)
    .execute(pool)
    .await
    .unwrap();
    now
}

fn hashes<T>(env: &ToolEnvelope<T>, f: impl Fn(&T) -> String) -> Vec<String> {
    env.results.iter().map(f).collect()
}

/// `days` actually excludes: the 40-day-old commit is out at `days: 7` and in at
/// `days: 36500`, on both the unfiltered and the filtered path. Mutation caught:
/// dropping `authored_at >= ?`, or hard-coding the cutoff.
#[tokio::test]
async fn recent_commits_days_cutoff_excludes_old_rows() {
    let pool = fresh_pool().await;
    seed_all(&pool).await;
    seed_window_and_scope_probes(&pool).await;

    let req = |filter: Option<&str>, days: i64| RecentCommitsRequest {
        filter: filter.map(str::to_string),
        days: Some(days),
        repo: None,
        limit: Some(50),
    };
    let short = run_recent_commits(&pool, &req(None, 7), &mut Stages::default()).await.unwrap();
    assert_eq!(short.total_matched, 3, "aaaa (now), dddd (-2h), bbbb (-1d); cccc (-40d) excluded");
    assert!(!hashes(&short, |c| c.commit_hash.clone()).contains(&"ccccccc".to_string()));
    let long = run_recent_commits(&pool, &req(None, 36_500), &mut Stages::default()).await.unwrap();
    assert_eq!(long.total_matched, 4);

    let short_f = run_recent_commits(&pool, &req(Some("rustFetch"), 7), &mut Stages::default())
        .await
        .unwrap();
    assert_eq!(short_f.total_matched, 1, "filtered path: only the in-window rustFetch commit");
    assert_eq!(short_f.results[0].commit_hash, "aaaaaaa");
    let long_f = run_recent_commits(&pool, &req(Some("rustFetch"), 36_500), &mut Stages::default())
        .await
        .unwrap();
    assert_eq!(long_f.total_matched, 2);
}

/// Recency order is DESC on the unfiltered path (three rows, distinct times).
/// Mutation caught: flipping to ASC or dropping ORDER BY.
#[tokio::test]
async fn recent_commits_are_ordered_newest_first() {
    let pool = fresh_pool().await;
    seed_all(&pool).await;
    seed_window_and_scope_probes(&pool).await;
    let env = run_recent_commits(
        &pool,
        &RecentCommitsRequest { filter: None, days: Some(7), repo: None, limit: Some(50) },
        &mut Stages::default(),
    )
    .await
    .unwrap();
    assert_eq!(
        hashes(&env, |c| c.commit_hash.clone()),
        vec!["aaaaaaa", "ddddddd", "bbbbbbb"],
        "now, -2h, -1d"
    );
    assert!(env.results.windows(2).all(|w| w[0].authored_at_unix >= w[1].authored_at_unix));
}

/// `repo` narrows on the FILTERED `recent_commits` path and on `find_commit`:
/// two in-window `bb8` commits live in different repos; scoping to `formsvc`
/// must return exactly the formsvc one. Mutation caught: replacing the repo
/// clause with a tautology in either function.
#[tokio::test]
async fn repo_scope_applies_on_filtered_recent_commits_and_find_commit() {
    let pool = fresh_pool().await;
    seed_all(&pool).await;
    seed_window_and_scope_probes(&pool).await;

    let unscoped = run_recent_commits(
        &pool,
        &RecentCommitsRequest {
            filter: Some("bb8".into()),
            days: Some(7),
            repo: None,
            limit: Some(50),
        },
        &mut Stages::default(),
    )
    .await
    .unwrap();
    assert_eq!(unscoped.total_matched, 2, "bbbb (formsvc) + dddd (webhooksvc)");
    let scoped = run_recent_commits(
        &pool,
        &RecentCommitsRequest {
            filter: Some("bb8".into()),
            days: Some(7),
            repo: Some("formsvc".into()),
            limit: Some(50),
        },
        &mut Stages::default(),
    )
    .await
    .unwrap();
    assert_eq!(scoped.total_matched, 1);
    assert_eq!(scoped.results[0].repo_path, "/tmp/formsvc");

    let fc_unscoped = run_find_commit(
        &pool,
        &FindCommitRequest { query: "bb8".into(), repo: None, limit: Some(50) },
        &mut Stages::default(),
    )
    .await
    .unwrap();
    assert_eq!(fc_unscoped.total_matched, 2);
    let fc_scoped = run_find_commit(
        &pool,
        &FindCommitRequest { query: "bb8".into(), repo: Some("formsvc".into()), limit: Some(50) },
        &mut Stages::default(),
    )
    .await
    .unwrap();
    assert_eq!(fc_scoped.total_matched, 1, "total_matched describes the SCOPED set");
    assert_eq!(fc_scoped.returned, 1);
    assert_eq!(fc_scoped.results[0].commit_hash, "bbbbbbb");
    // And a repo string matching nothing is an honest zero, not an error.
    let none = run_find_commit(
        &pool,
        &FindCommitRequest { query: "bb8".into(), repo: Some("no-such-repo".into()), limit: None },
        &mut Stages::default(),
    )
    .await
    .unwrap();
    assert_eq!(none.total_matched, 0);
    assert_eq!(none.corpus_empty, Some(false));
}

/// `recent_sessions` `days` excludes the 40-day-old session (unfiltered and
/// filtered), and the count is DISTINCT sessions. Mutation caught: dropping
/// `edited_at >= ?` on either path.
#[tokio::test]
async fn recent_sessions_days_cutoff_excludes_old_sessions() {
    let pool = fresh_pool().await;
    seed_all(&pool).await;
    seed_window_and_scope_probes(&pool).await;
    let req = |filter: Option<&str>, days: i64| RecentSessionsRequest {
        filter: filter.map(str::to_string),
        days: Some(days),
        limit: Some(50),
    };
    let short = run_recent_sessions(&pool, &req(None, 7), &mut Stages::default()).await.unwrap();
    assert!(
        !short.results.iter().any(|r| r.session_id == "sess-ancient"),
        "40-day-old session must be outside a 7-day window"
    );
    let long = run_recent_sessions(&pool, &req(None, 36_500), &mut Stages::default()).await.unwrap();
    assert!(long.results.iter().any(|r| r.session_id == "sess-ancient"));
    assert_eq!(long.total_matched, short.total_matched + 1);

    let short_f =
        run_recent_sessions(&pool, &req(Some("ancient"), 7), &mut Stages::default()).await.unwrap();
    assert_eq!(short_f.total_matched, 0, "the only 'ancient' edge is out of window");
    let long_f =
        run_recent_sessions(&pool, &req(Some("ancient"), 36_500), &mut Stages::default()).await.unwrap();
    assert_eq!(long_f.total_matched, 1);
}

/// bm25 ordering on a `find_*` tool with TWO hits of different density: the
/// commit whose message repeats the term must rank first. Mutation caught:
/// removing `rank ASC` from the ORDER BY (falls back to rowid order, which puts
/// the sparse row first here because it is inserted first).
#[tokio::test]
async fn find_commit_orders_by_bm25_rank() {
    let pool = fresh_pool().await;
    let now: i64 = sqlx::query_scalar("SELECT CAST(strftime('%s','now') AS INTEGER)")
        .fetch_one(&pool)
        .await
        .unwrap();
    for (hash, fts) in [
        ("1111111111111111111111111111111111111111", "zzterm once, then a lot of other words about other things entirely"),
        ("2222222222222222222222222222222222222222", "zzterm zzterm zzterm"),
    ] {
        let (id,): (i64,) = sqlx::query_as(
            "INSERT INTO commit_entries \
                (repo_path, commit_hash, parent_hashes, author_email, author_name, \
                 authored_at, committed_at, message_summary, message_body, files_changed) \
             VALUES ('/tmp/r', ?, '[]', 'a@b', 'A', ?, ?, ?, NULL, '[]') RETURNING id",
        )
        .bind(hash)
        .bind(now)
        .bind(now)
        .bind(fts)
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO search_index (body, kind, rowid_ref, source_table) \
             VALUES (?, 'commit', ?, 'commit_entries')",
        )
        .bind(fts)
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    }
    let env = run_find_commit(
        &pool,
        &FindCommitRequest { query: "zzterm".into(), repo: None, limit: Some(10) },
        &mut Stages::default(),
    )
    .await
    .unwrap();
    assert_eq!(hashes(&env, |c| c.commit_hash.clone()), vec!["2222222", "1111111"]);
    assert!(env.results[0].rank.unwrap() < env.results[1].rank.unwrap(), "bm25: lower is better");
}

/// The `limit` clamp is enforced in PRODUCTION code, not re-implemented in a
/// test: 60 matching commits, `limit: 999` → exactly `MAX_LIMIT` returned while
/// `total_matched` still says 60. Mutation caught: `.clamp(1, MAX_LIMIT)` →
/// `.max(1)`.
#[tokio::test]
async fn find_commit_limit_is_clamped_in_production() {
    let pool = fresh_pool().await;
    let now: i64 = sqlx::query_scalar("SELECT CAST(strftime('%s','now') AS INTEGER)")
        .fetch_one(&pool)
        .await
        .unwrap();
    for i in 0..60 {
        let hash = format!("{i:040x}");
        let (id,): (i64,) = sqlx::query_as(
            "INSERT INTO commit_entries \
                (repo_path, commit_hash, parent_hashes, author_email, author_name, \
                 authored_at, committed_at, message_summary, message_body, files_changed) \
             VALUES ('/tmp/r', ?, '[]', 'a@b', 'A', ?, ?, 'bulkterm commit', NULL, '[]') RETURNING id",
        )
        .bind(&hash)
        .bind(now - i)
        .bind(now - i)
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO search_index (body, kind, rowid_ref, source_table) \
             VALUES ('bulkterm commit', 'commit', ?, 'commit_entries')",
        )
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    }
    let env = run_find_commit(
        &pool,
        &FindCommitRequest { query: "bulkterm".into(), repo: None, limit: Some(999) },
        &mut Stages::default(),
    )
    .await
    .unwrap();
    assert_eq!(env.total_matched, 60);
    assert_eq!(env.returned, MAX_LIMIT);
    assert_eq!(env.results.len() as i64, MAX_LIMIT);
    let rc = run_recent_commits(
        &pool,
        &RecentCommitsRequest { filter: None, days: Some(7), repo: None, limit: Some(999) },
        &mut Stages::default(),
    )
    .await
    .unwrap();
    assert_eq!(rc.returned, MAX_LIMIT);
}

// ---- RC1 review 1.10 — schema-vs-emit, every tool, every shape ------------------

/// Walk `value` against `schema` (rooted at `root` for `$ref` resolution) and
/// collect `<path>: missing <field>` for every object whose schema declares
/// `required` keys the emitted JSON lacks. Tolerant of the shapes schemars 1.x
/// emits for `Option<T>` (`anyOf`/`oneOf` with a null branch, or a type list).
fn schema_required_violations(root: &serde_json::Value, schema: &serde_json::Value, value: &serde_json::Value, path: &str, out: &mut Vec<String>) {
    use serde_json::Value;
    let mut schema = schema;
    if let Some(r) = schema.get("$ref").and_then(Value::as_str) {
        let key = r.trim_start_matches("#/$defs/").trim_start_matches("#/definitions/");
        let Some(resolved) = root.get("$defs").or_else(|| root.get("definitions")).and_then(|d| d.get(key)) else {
            return;
        };
        schema = resolved;
    }
    if value.is_null() {
        return;
    }
    for alt_key in ["anyOf", "oneOf", "allOf"] {
        if let Some(alts) = schema.get(alt_key).and_then(Value::as_array) {
            for alt in alts {
                if alt.get("type").and_then(Value::as_str) == Some("null") {
                    continue;
                }
                schema_required_violations(root, alt, value, path, out);
            }
            return;
        }
    }
    if let Some(obj) = value.as_object() {
        if let Some(req) = schema.get("required").and_then(Value::as_array) {
            for f in req.iter().filter_map(Value::as_str) {
                if !obj.contains_key(f) {
                    out.push(format!("{path}: missing required `{f}`"));
                }
            }
        }
        if let Some(props) = schema.get("properties").and_then(Value::as_object) {
            for (k, sub) in props {
                if let Some(v) = obj.get(k) {
                    schema_required_violations(root, sub, v, &format!("{path}.{k}"), out);
                }
            }
        }
        if let Some(extra) = schema.get("additionalProperties")
            && extra.is_object()
        {
            for (k, v) in obj {
                schema_required_violations(root, extra, v, &format!("{path}.{k}"), out);
            }
        }
    } else if let Some(arr) = value.as_array()
        && let Some(items) = schema.get("items")
    {
        for (i, v) in arr.iter().enumerate() {
            schema_required_violations(root, items, v, &format!("{path}[{i}]"), out);
        }
    }
}

fn assert_matches_schema<T: serde::Serialize + schemars::JsonSchema>(label: &str, value: &T) {
    let root = serde_json::to_value(schemars::schema_for!(T)).unwrap();
    let emitted = serde_json::to_value(value).unwrap();
    let mut out = Vec::new();
    schema_required_violations(&root, &root, &emitted, label, &mut out);
    assert!(out.is_empty(), "{label}: emitted JSON violates its own outputSchema: {out:#?}\n{emitted}");
}

/// EVERY tool's real response — a hit response AND a zero-result response —
/// validates against the outputSchema it advertises, recursively (nested result
/// structs included), plus `check()`. The single-type, top-level-only probe above
/// let a re-introduced `skip_serializing_if` on `CheckResult.shallow_repos` or
/// `CodeResult.body_truncated` through (RC1 review 1.10); this walks the whole
/// tree of every tool. Mutation caught: any `skip_serializing_if` on a non-Option,
/// non-`serde(default)` field of any result type.
#[tokio::test]
async fn every_tool_response_validates_against_its_output_schema() {
    let pool = fresh_pool().await;
    seed_all(&pool).await;
    seed_source(&pool).await;
    let mut st = Stages::default();

    let hit = run_find_code(&pool, &FindCodeRequest { query: "resolve_provenance".into(), repo: None, limit: None }, &mut st).await.unwrap();
    assert!(hit.returned > 0, "fixture must produce a hit");
    assert_matches_schema("find_code(hit)", &hit);
    let miss = run_find_code(&pool, &FindCodeRequest { query: "no_such_zzz".into(), repo: None, limit: None }, &mut st).await.unwrap();
    assert_matches_schema("find_code(miss)", &miss);

    let hit = run_find_commit(&pool, &FindCommitRequest { query: "rustFetch".into(), repo: None, limit: None }, &mut st).await.unwrap();
    assert!(hit.returned > 0);
    assert_matches_schema("find_commit(hit)", &hit);
    let miss = run_find_commit(&pool, &FindCommitRequest { query: "no_such_zzz".into(), repo: None, limit: None }, &mut st).await.unwrap();
    assert_matches_schema("find_commit(miss)", &miss);

    let hit = run_find_design_doc(&pool, &FindDesignDocRequest { query: "F45".into(), limit: None }, &mut st).await.unwrap();
    assert!(hit.returned > 0, "seed_all design section mentions F45: {hit:?}");
    assert_matches_schema("find_design_doc(hit)", &hit);
    let miss = run_find_design_doc(&pool, &FindDesignDocRequest { query: "no_such_zzz".into(), limit: None }, &mut st).await.unwrap();
    assert_matches_schema("find_design_doc(miss)", &miss);

    let hit = run_find_memory(&pool, &FindMemoryRequest { query: "hydration".into(), limit: None }, &mut st).await.unwrap();
    assert!(hit.returned > 0, "seed_all memory mentions hydration: {hit:?}");
    assert_matches_schema("find_memory(hit)", &hit);
    let miss = run_find_memory(&pool, &FindMemoryRequest { query: "no_such_zzz".into(), limit: None }, &mut st).await.unwrap();
    assert_matches_schema("find_memory(miss)", &miss);

    let hit = run_find_session(&pool, &FindSessionRequest { query: "wedge".into(), limit: None }, &mut st).await.unwrap();
    assert!(hit.returned > 0, "seed_all session edge mentions wedge: {hit:?}");
    assert_matches_schema("find_session(hit)", &hit);
    let miss = run_find_session(&pool, &FindSessionRequest { query: "no_such_zzz".into(), limit: None }, &mut st).await.unwrap();
    assert_matches_schema("find_session(miss)", &miss);

    let hit = run_recent_commits(&pool, &RecentCommitsRequest { filter: None, days: Some(7), repo: None, limit: None }, &mut st).await.unwrap();
    assert!(hit.returned > 0);
    assert_matches_schema("recent_commits(hit)", &hit);
    let miss = run_recent_commits(&pool, &RecentCommitsRequest { filter: Some("no_such_zzz".into()), days: Some(7), repo: None, limit: None }, &mut st).await.unwrap();
    assert_matches_schema("recent_commits(miss)", &miss);

    let hit = run_recent_sessions(&pool, &RecentSessionsRequest { filter: None, days: Some(36_500), limit: None }, &mut st).await.unwrap();
    assert!(hit.returned > 0);
    assert_matches_schema("recent_sessions(hit)", &hit);
    let miss = run_recent_sessions(&pool, &RecentSessionsRequest { filter: Some("no_such_zzz".into()), days: Some(7), limit: None }, &mut st).await.unwrap();
    assert_matches_schema("recent_sessions(miss)", &miss);

    let check = run_check(&pool, 1, None, &mut st).await.unwrap();
    assert_matches_schema("check", &check);
}

// ---- RC1 review 1.10 — corpus probes: table AND clock, all five, pairwise -------

/// Every `Corpus` probe reads ITS OWN table and ITS OWN clock. For each corpus,
/// seed exactly one row (with `documents.indexed_at` deliberately far from the
/// content clock), then ask ALL five probes: only the seeded one reports
/// non-empty, and its `corpus_indexed_through` is the content clock — mtime /
/// committed_at / edited_at — never `indexed_at`, never `authored_at`, never
/// MIN. Mutations caught: swapping any probe's count table (incl. design_doc,
/// which the older test left unpinned), `MAX(indexed_at)` for memory / design /
/// source, `authored_at` for commits, `MIN` for sessions.
#[tokio::test]
async fn corpus_probes_read_their_own_table_and_clock() {
    const CONTENT_TS: i64 = 1_700_000_000; // the clock the field must report
    const DECOY_TS: i64 = 1_600_000_000; //  indexed_at / authored_at / an older edit
    let expected_iso = unix_to_iso(CONTENT_TS);
    let corpora: Vec<(Corpus, Vec<String>)> = vec![
        (
            Corpus::MemoryEntries,
            vec![
                format!("INSERT INTO documents (id, path, kind, content_hash, mtime, indexed_at) VALUES (1, '/w/m.md', 'memory', 'h', {CONTENT_TS}, {DECOY_TS})"),
                "INSERT INTO memory_entries (document_id, name, memory_type, description, body) VALUES (1, 'n', 'user', 'd', 'b')".to_string(),
            ],
        ),
        (
            Corpus::DesignDocSections,
            vec![
                format!("INSERT INTO documents (id, path, kind, content_hash, mtime, indexed_at) VALUES (2, '/w/d.md', 'design_doc', 'h', {CONTENT_TS}, {DECOY_TS})"),
                "INSERT INTO design_doc_sections (document_id, heading_path, line_start, line_end, body) VALUES (2, 'H', 1, 2, 'b')".to_string(),
            ],
        ),
        (
            Corpus::SourceChunks,
            vec![
                format!("INSERT INTO documents (id, path, kind, content_hash, mtime, indexed_at) VALUES (3, '/w/s.rs', 'source', 'h', {CONTENT_TS}, {DECOY_TS})"),
                "INSERT INTO source_chunks (document_id, path, line_start, line_end, body) VALUES (3, 's.rs', 1, 2, 'b')".to_string(),
            ],
        ),
        (
            Corpus::CommitEntries,
            vec![format!(
                "INSERT INTO commit_entries (repo_path, commit_hash, author_email, author_name, authored_at, committed_at, message_summary, files_changed) \
                 VALUES ('/w', 'h1', 'a@b', 'a', {DECOY_TS}, {CONTENT_TS}, 's', '[]')"
            )],
        ),
        (
            Corpus::SessionEdges,
            vec![
                format!("INSERT INTO session_edges (session_id, message_uuid, edge_ordinal, tool, file_path, edited_at, rationale) VALUES ('s', 'u1', 0, 'Edit', 'a.rs', {DECOY_TS}, 'r')"),
                format!("INSERT INTO session_edges (session_id, message_uuid, edge_ordinal, tool, file_path, edited_at, rationale) VALUES ('s', 'u2', 0, 'Edit', 'b.rs', {CONTENT_TS}, 'r')"),
            ],
        ),
    ];
    let all = [
        Corpus::MemoryEntries,
        Corpus::DesignDocSections,
        Corpus::SourceChunks,
        Corpus::CommitEntries,
        Corpus::SessionEdges,
    ];
    for (seeded, stmts) in &corpora {
        let pool = fresh_pool().await;
        for stmt in stmts {
            sqlx::query(stmt).execute(&pool).await.unwrap();
        }
        for probe in all {
            let mut env: ToolEnvelope<()> = ToolEnvelope {
                results: vec![],
                total_matched: 0,
                returned: 0,
                tool: format!("{probe:?}"),
                query_broadened: false,
                corpus_empty: None,
                corpus_indexed_through: None,
                returned_full_tokens: 0,
        neighbourhood_terms: Vec::new(),
        retrieval_shape: None,
        also_matched: Vec::new(),
            };
            annotate_empty_result(&mut env, &pool, probe).await;
            if probe == *seeded {
                assert_eq!(env.corpus_empty, Some(false), "{seeded:?}: own probe must see its row");
                assert_eq!(
                    env.corpus_indexed_through.as_deref(),
                    Some(expected_iso.as_str()),
                    "{seeded:?}: newest-item clock must be the CONTENT clock, not indexed_at/authored_at/MIN"
                );
            } else {
                assert_eq!(
                    env.corpus_empty,
                    Some(true),
                    "{probe:?} must report empty when only {seeded:?} is seeded (table swap)"
                );
                assert!(env.corpus_indexed_through.is_none());
            }
        }
    }
}

/// `summarize` truncates on a WORD boundary when one exists past the midpoint,
/// and hard-cuts otherwise. Mutation caught: dropping the boundary branch
/// (`out` would be 200 chars of `a` + `…`).
#[test]
fn summarize_truncates_on_the_word_boundary_itself() {
    let body = format!("{} {}", "a".repeat(150), "b".repeat(150));
    let out = summarize(&body, 200);
    assert_eq!(out, format!("{}…", "a".repeat(150)), "cut at the space, not at 200");
    // No whitespace past the midpoint → hard cut at max_chars.
    let dense = "x".repeat(400);
    assert_eq!(summarize(&dense, 200), format!("{}…", "x".repeat(200)));
    // Whitespace only BEFORE the midpoint is not a boundary worth using.
    let early = format!("ab {}", "c".repeat(400));
    assert_eq!(summarize(&early, 200).chars().count(), 201);
    assert!(summarize(&early, 200).starts_with("ab c"));
    // Short bodies pass through untouched.
    assert_eq!(summarize("short", 200), "short");
}

/// `check()` percentiles use only the last hour and only successful calls;
/// `extractors_last_run_ms` picks the latest SUCCESSFUL run. Mutations caught:
/// dropping `started_at >= ?` (the 2-hour-old 900 ms rows would lift p95 to 900),
/// dropping `error IS NULL` from either query (the 5000 ms errored call would
/// become p95 / the "latest" extractor run).
#[tokio::test]
async fn check_percentiles_honour_the_window_and_skip_errors() {
    let pool = fresh_pool().await;
    let now: i64 = sqlx::query_scalar("SELECT CAST(strftime('%s','now') AS INTEGER)")
        .fetch_one(&pool)
        .await
        .unwrap();
    let insert = |op: &'static str, at: i64, ms: i64, err: Option<&'static str>| {
        let pool = pool.clone();
        async move {
            sqlx::query(
                "INSERT INTO op_measurements (op_name, started_at, duration_ms, error, extra_json) \
                 VALUES (?, ?, ?, ?, '{}')",
            )
            .bind(op)
            .bind(at)
            .bind(ms)
            .bind(err)
            .execute(&pool)
            .await
            .unwrap();
        }
    };
    // In-window successes: [5, 7, 9].
    for ms in [5, 7, 9] {
        insert("tool.find_code", now - 60, ms, None).await;
    }
    // Out-of-window successes (2 h ago) — must not count.
    for _ in 0..5 {
        insert("tool.find_code", now - 2 * 3600, 900, None).await;
    }
    // In-window ERROR — must not count.
    insert("tool.find_code", now - 30, 5000, Some("boom")).await;
    // Extractors: an older success (40 ms), then a newer FAILURE (9000 ms).
    insert("extract.commits", now - 120, 40, None).await;
    insert("extract.commits", now - 10, 9000, Some("git2 open failed")).await;

    let r = run_check(&pool, 0, None, &mut Stages::default()).await.unwrap();
    assert_eq!(r.perf_p50_ms.get("tool.find_code"), Some(&7));
    assert_eq!(r.perf_p95_ms.get("tool.find_code"), Some(&9), "neither the stale 900s nor the errored 5000 count");
    assert_eq!(
        r.extractors_last_run_ms.get("extract.commits"),
        Some(&40),
        "latest SUCCESSFUL run, not the newer errored one"
    );
}

// ---- deep-scan tail (QUERY_QUALITY_DESIGN §6d) ---------------------------------

use std::collections::HashSet;

/// Ranks 11..40 are frequently several chunks of the SAME file. Collapsing them is
/// what makes the tail affordable, so this pins the collapse rather than trusting it.
#[test]
fn also_matched_dedupes_by_file_and_keeps_the_best_line() {
    let head = HashSet::new();
    let tail = vec![
        ("/repo/src/a.rs".to_string(), 40),
        ("/repo/src/a.rs".to_string(), 91),
        ("/repo/src/b.rs".to_string(), 7),
        ("/repo/src/a.rs".to_string(), 120),
    ];
    let out = build_also_matched(&head, tail);
    assert_eq!(out.len(), 2, "three chunks of a.rs collapse to one pointer");
    assert_eq!(out[0].path, "/repo/src/a.rs");
    assert_eq!(
        out[0].match_line, 40,
        "rank order is input order, so the FIRST sighting is the best hit"
    );
    assert_eq!(out[0].matches, 3, "and it says how many passages matched");
    assert_eq!(out[1].path, "/repo/src/b.rs");
    assert_eq!(out[1].matches, 1);
}

/// A file whose body the caller already has must not be re-advertised as a pointer —
/// that spends bytes to say something already on screen.
#[test]
fn also_matched_excludes_files_already_rendered_in_the_head() {
    let head: HashSet<String> = ["/repo/src/a.rs".to_string()].into_iter().collect();
    let tail = vec![
        ("/repo/src/a.rs".to_string(), 40),
        ("/repo/src/b.rs".to_string(), 7),
    ];
    let out = build_also_matched(&head, tail);
    assert_eq!(out.len(), 1, "the head's own file is dropped from the tail");
    assert_eq!(out[0].path, "/repo/src/b.rs");
}

/// Nothing below the window ⇒ nothing to say. Guards the `skip_serializing_if` path:
/// an empty vec must stay empty rather than becoming a field full of noise.
#[test]
fn also_matched_is_empty_when_the_tail_adds_nothing() {
    let head: HashSet<String> = ["/repo/src/a.rs".to_string()].into_iter().collect();
    let out = build_also_matched(&head, vec![("/repo/src/a.rs".to_string(), 40)]);
    assert!(out.is_empty());
}
