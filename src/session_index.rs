// SPDX-License-Identifier: MIT

//! D1 gear 7 — the RAW-TRANSCRIPT session→code edge index (docs/D1_SCOPE.md §10
//! "Session-coverage probe").
//!
//! The probe settled the session-frontier fork toward raw transcripts: a perfect
//! CLAUDE.md path-parser recovers only ~25% of a session's edit-edges because the
//! curated note names code by *concept, not path* — "a `build` block on `check()`"
//! never spells `src/mcp/check.rs`. The transcript carries every edge LOSSLESSLY,
//! timestamped, and branch-anchored. This gear reads it.
//!
//! For each `~/.claude/projects/<slug>/*.jsonl` it extracts every `Edit`/`Write`
//! tool-call as one `session_edges` row: the file it touched (the CHANGE), the
//! nearest preceding assistant text (the RATIONALE), and the per-line `gitBranch`
//! + `cwd` + `timestamp` — the §1 commit join key, present for free.
//!
//! It then makes a best-effort session-to-commit binding to the oldest commit on
//! that repo which captured the file at/after the edit (the next commit to commit
//! it).
//!
//! SCRAPPY ON PURPOSE: pure `std::fs` + `serde_json` line walk, driven off a CLI
//! subcommand, perturbs nothing in indexer.rs. Run `nibdex index` first on the
//! same `--db` so `commit_entries` exists for the binding to resolve against.
//!
//! SPIKE SCOPE (see migration): indexes the EDGE (file + when + why), NOT the
//! verbatim diff body — that already lives in `diff_hunks`/`source_chunks`, and
//! omitting it keeps volume down and sidesteps the worst IP-scrub concern. Read-
//! edges (design context) are a recorded lever, not indexed here.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result};
use chrono::DateTime;
use serde_json::Value;
use sqlx::SqlitePool;

use crate::domains::{normalize_lexical, DomainConfig};

#[derive(Debug, Default)]
pub struct SessionIndexStats {
    pub transcripts_seen: usize,
    pub sessions_seen: usize,
    pub lines_total: usize,
    pub lines_parse_err: usize,
    pub edges_indexed: usize,
    pub edits: usize,
    pub writes: usize,
    /// Edges bound to an indexed `commit_entries.id` (the session→commit join hit).
    pub commit_bound: usize,
    /// Edges with no capturing commit indexed (binding precision is a §10 unknown).
    pub commit_unbound: usize,
    /// Edges already present (dedupe-key hit) — skipped by the additive merge.
    pub edges_duplicate: usize,
    /// Edges from a uuid-less message — cannot be safely deduped, so skipped.
    pub edges_skipped_no_uuid: usize,
    /// Domain mode only: edges whose target file is not in the indexed domain —
    /// written to no db (GEAR2_DESIGN §1.2). Over-drop is a visibility cost, not
    /// a leak; surfaced so the fail-narrow rate is a number, not a vibe.
    pub edges_dropped_foreign_domain: usize,
    /// Domain mode only: edges whose rationale was replaced by the constant
    /// withheld-marker because their session had touched another domain's tree
    /// (the ratchet, GEAR2_DESIGN §2). Read the dogfood before adding any escape
    /// hatch (§7.4).
    pub rationales_withheld: usize,
    pub elapsed_ms: u128,
}

/// One write-edge extracted from a transcript, pre-binding.
struct SessionEdge {
    session_id: String,
    message_uuid: String,
    /// 0-based position of this Edit/Write among the captured tool-calls of its
    /// assistant message. `(message_uuid, edge_ordinal)` is the dedupe key.
    edge_ordinal: i64,
    tool: String,
    /// Repo-relative when `repo_path` is a prefix; otherwise left absolute.
    file_path: String,
    repo_path: Option<String>,
    git_branch: Option<String>,
    edited_at: i64,
    rationale: String,
    /// The UNTOUCHED `input.file_path` — Gear 2 routes the edge off this raw
    /// absolute target, never off the split pair (GEAR2_DESIGN §1.1). Unused in
    /// unpartitioned mode.
    raw_abs_path: String,
    /// The transcript line's `cwd` — the metadata-sanitization input (§1.4).
    cwd: Option<String>,
    /// The `message.id` of the logical group this edge belongs to — the ratchet's
    /// admission granularity (§2). Not a storage key (storage stays per-line).
    group_id: String,
}

/// The paths one logical-message group (`message.id`) touched via ANY tool_use
/// input — Edit/Write/Read/Grep/Glob/NotebookEdit (GEAR2_DESIGN §2 taint set).
/// Ordered as they appear; the ratchet accumulates cleanliness across groups.
struct GroupTaint {
    session_id: String,
    group_id: String,
    /// Raw absolute paths (`file_path`/`path`/`notebook_path`) seen in the group.
    paths: Vec<String>,
}

/// One transcript's parse: the write-edges (per-line, as today) plus the ordered
/// per-group taint sets the domain ratchet reasons over. Domain-agnostic — the
/// per-domain admission decision happens in `index_sessions`.
struct TranscriptParse {
    edges: Vec<SessionEdge>,
    groups: Vec<GroupTaint>,
}

/// The constant stored in a withheld edge's `rationale` column. NEVER derived
/// from the transcript and NEVER indexed into FTS (GEAR2_DESIGN §2 (i)/(iii)).
const WITHHELD_MARKER: &str = "[rationale withheld: cross-domain session]";

/// Default transcript root: `~/.claude/projects`. Each child dir is a workspace
/// slug holding that workspace's `*.jsonl` session transcripts.
pub fn default_projects_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".claude/projects"))
}

/// Index every transcript under `projects_dir`. If `slug` is given, only that
/// child dir is scanned (e.g. `-Users-you-workspace`); otherwise all slugs.
///
/// ADDITIVE MERGE (docs/SESSION_SCOPE_DESIGN.md §5): re-running only ever grows
/// the corpus — `INSERT OR IGNORE` on the `(message_uuid, edge_ordinal)` dedupe
/// key skips already-indexed edges, so retention-expired transcripts' edges are
/// preserved, never erased. `rebuild = true` opts into the old wipe-first
/// behavior (a deliberate from-scratch reset). The whole pass runs in one
/// transaction (idempotent, and no per-row autocommit).
pub async fn index_sessions(
    pool: &SqlitePool,
    projects_dir: &Path,
    slug: Option<&str>,
    rebuild: bool,
    workspace: &Path,
    domain: Option<&str>,
) -> Result<SessionIndexStats> {
    let started = Instant::now();
    let mut stats = SessionIndexStats::default();

    // Domain routing (GEAR2_DESIGN §1, mirrors indexer::full_scan): with a
    // `--domain`, load `.nibdex-domains.toml`, validate the domain, and route
    // every edge + its rationale through the per-edge rules + the ratchet. With
    // `None`, this is byte-for-byte the unpartitioned behavior — no router built.
    let workspace = workspace
        .canonicalize()
        .with_context(|| format!("canonicalize workspace {workspace:?}"))?;
    let config = DomainConfig::load(&workspace)?;
    let router: Option<DomainRouter> = match domain {
        Some(d) => {
            let config = config.as_ref().ok_or_else(|| {
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
            Some(DomainRouter {
                config,
                workspace: &workspace,
                domain: d,
            })
        }
        None => None,
    };

    if rebuild {
        // Deliberate from-scratch reset: drop the prior rows + their FTS entries.
        wipe_all_session_edges(pool).await?;
    }

    let transcripts = collect_transcripts(projects_dir, slug)?;
    let mut sessions: HashSet<String> = HashSet::new();

    let mut tx = pool.begin().await?;
    for path in &transcripts {
        stats.transcripts_seen += 1;
        let parse = parse_transcript(path, &mut stats)?;
        // Domain mode: precompute this transcript's per-group rationale admission
        // (the ratchet, §2) BEFORE emitting any edge — a group is evaluated
        // atomically, so an edit can't be admitted until its whole group is known.
        let admit = router.as_ref().map(|r| admission_map(&parse.groups, r));
        for edge in &parse.edges {
            sessions.insert(edge.session_id.clone());
            route_and_insert(&mut tx, edge, router.as_ref(), admit.as_ref(), &mut stats).await?;
        }
    }
    tx.commit().await?;

    stats.sessions_seen = sessions.len();
    stats.elapsed_ms = started.elapsed().as_millis();
    Ok(stats)
}

/// The per-domain routing context for one `index_sessions` pass (GEAR2_DESIGN §1).
/// Bundles the loaded config, the canonicalized workspace root, and the target
/// domain so a raw tool-input path can be tested for domain-visibility.
struct DomainRouter<'a> {
    config: &'a DomainConfig,
    workspace: &'a Path,
    domain: &'a str,
}

impl DomainRouter<'_> {
    /// Is `raw` (an untouched absolute tool-input path) visible to this domain?
    /// Lexically normalize first (the fallback for a deleted/`..`/symlink target
    /// that defeats `canonicalize`), then `canonicalize` with fallback to the
    /// normalized path, then ask `includes` (GEAR2_DESIGN §1.1 / §3 B3). An
    /// unresolvable `..` → not visible. This is the ONE safety-critical primitive
    /// — a foreign path it wrongly accepts is the only routing-side leak vector.
    fn visible(&self, raw: &str) -> bool {
        // Fail-narrow on a non-absolute path. A relative tool-input path
        // `canonicalize`s against the INDEXER's cwd (not the session's), so a
        // foreign relative path like `src` — run in another domain's checkout —
        // could resolve to a workspace subdir and be wrongly admitted (Review A
        // P1-4). Only an absolute path can be placed under a domain with certainty;
        // a relative one is treated as not-visible, so it taints rather than leaks.
        if !Path::new(raw).is_absolute() {
            return false;
        }
        let Some(norm) = normalize_lexical(Path::new(raw)) else {
            return false;
        };
        let resolved = norm.canonicalize().unwrap_or(norm);
        self.config.includes(self.domain, self.workspace, &resolved)
    }
}

/// The ratchet (GEAR2_DESIGN §2): walk a transcript's groups IN ORDER; a group
/// is clean iff every path it touched is domain-visible; once a session hits an
/// unclean group it is tainted for the rest of the transcript (permanent, never
/// resets). Returns, per `(session_id, group_id)`, whether that group's rationale
/// is ADMITTED. Keyed by session too, so the rare two-sessions-in-one-file case
/// still ratchets independently.
fn admission_map(
    groups: &[GroupTaint],
    router: &DomainRouter,
) -> HashMap<(String, String), bool> {
    let mut admit = HashMap::new();
    let mut tainted: HashSet<&str> = HashSet::new();
    for g in groups {
        let group_clean = g.paths.iter().all(|p| router.visible(p));
        if !group_clean {
            tainted.insert(g.session_id.as_str());
        }
        let admitted = !tainted.contains(g.session_id.as_str());
        admit.insert((g.session_id.clone(), g.group_id.clone()), admitted);
    }
    admit
}

/// Route one edge into the (single) domain db this pass writes, or into no db
/// (GEAR2_DESIGN §1). `None` router = unpartitioned = today's behavior verbatim.
async fn route_and_insert(
    conn: &mut sqlx::SqliteConnection,
    edge: &SessionEdge,
    router: Option<&DomainRouter<'_>>,
    admit: Option<&HashMap<(String, String), bool>>,
    stats: &mut SessionIndexStats,
) -> Result<()> {
    let Some(r) = router else {
        // Unpartitioned — the original path, unchanged: full rationale + FTS body.
        return insert_edge(
            conn,
            edge,
            edge.repo_path.as_deref(),
            edge.git_branch.as_deref(),
            &edge.rationale,
            true,
            stats,
        )
        .await;
    };

    // §1.2 — route by the RAW target file. Foreign-domain / unlabeled / workspace-
    // root / outside-workspace targets go to NO db (fail-narrow), and are counted.
    if !r.visible(&edge.raw_abs_path) {
        stats.edges_dropped_foreign_domain += 1;
        return Ok(());
    }

    // §1.4 — keep repo_path + git_branch only when the edit's cwd is itself
    // domain-visible; otherwise NULL both (a foreign checkout's cwd + branch name
    // are IP — e.g. a cross-cwd edit of an in-domain file from a foreign tree).
    let cwd_visible = edge.cwd.as_deref().is_some_and(|c| r.visible(c));
    let (repo_path, git_branch) = if cwd_visible {
        (edge.repo_path.as_deref(), edge.git_branch.as_deref())
    } else {
        (None, None)
    };

    // §2 — admit the rationale iff this session stayed in-domain through this
    // edge's group; else store the constant marker (never FTS-indexed). Default
    // to WITHHOLD on a missing key: admission requires affirmative proof, so any
    // gap fails toward redaction, never toward a leak.
    let admitted = admit
        .and_then(|m| m.get(&(edge.session_id.clone(), edge.group_id.clone())).copied())
        .unwrap_or(false);
    let rationale = if admitted {
        edge.rationale.as_str()
    } else {
        stats.rationales_withheld += 1;
        WITHHELD_MARKER
    };

    insert_edge(conn, edge, repo_path, git_branch, rationale, admitted, stats).await
}

/// `<projects_dir>/<slug>/*.jsonl`. With `slug=None`, every child dir's jsonl.
fn collect_transcripts(projects_dir: &Path, slug: Option<&str>) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let slug_dirs: Vec<PathBuf> = match slug {
        Some(s) => vec![projects_dir.join(s)],
        None => std::fs::read_dir(projects_dir)
            .with_context(|| format!("reading projects dir {projects_dir:?}"))?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect(),
    };
    for dir in slug_dirs {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.filter_map(|e| e.ok()) {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                out.push(p);
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Walk one JSONL transcript, emitting a write-edge per `Edit`/`Write` tool-call.
///
/// Rationale rule (D1_SCOPE §10: "tool_use = change, adjacent text = rationale"):
/// the assistant's text precedes its tool calls in a message, so the rationale is
/// the nearest non-empty text block AT OR BEFORE the call — the in-message text if
/// present, else the most recent text from a prior message (`last_text`).
fn parse_transcript(path: &Path, stats: &mut SessionIndexStats) -> Result<TranscriptParse> {
    let content =
        std::fs::read_to_string(path).with_context(|| format!("reading transcript {path:?}"))?;
    let mut edges = Vec::new();
    // Ordered per-`message.id`-group taint sets (§2). Assistant lines of one
    // logical message are contiguous (tool_result user-lines interleave but are
    // skipped), so a group is "the last one iff (session,group_id) still match."
    let mut groups: Vec<GroupTaint> = Vec::new();
    let mut last_text = String::new();
    // `last_text` carries a prior message's text into a text-less edit as its
    // fallback rationale — but that fallback must stay WITHIN one session.
    // `last_session` scopes it: in a multi-session transcript file, session A's
    // (possibly foreign-tainted) prose must not become session B's inherited
    // rationale, which would bypass B's own clean taint and admit A's text
    // (the cross-session `last_text` bleed). Reset on any session change; an
    // over-reset only drops a fallback (fail-toward-withhold), never leaks.
    let mut last_session = String::new();

    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        stats.lines_total += 1;
        let obj: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => {
                stats.lines_parse_err += 1;
                continue;
            }
        };
        if obj.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(content_blocks) = obj
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(Value::as_array)
        else {
            continue;
        };

        let session_id = obj.get("sessionId").and_then(Value::as_str).unwrap_or("");
        // Scope the `last_text` fallback to one session (see its declaration).
        if session_id != last_session {
            last_text.clear();
            last_session = session_id.to_string();
        }
        let message_uuid = obj.get("uuid").and_then(Value::as_str).unwrap_or("");
        // `message.id` — the logical-message group (the ratchet's granularity,
        // §2). Distinct from the per-line `uuid` (the storage dedupe key).
        let group_id = obj
            .get("message")
            .and_then(|m| m.get("id"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let cwd = obj.get("cwd").and_then(Value::as_str);
        let git_branch = obj.get("gitBranch").and_then(Value::as_str);
        let edited_at = obj
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_rfc3339);

        // The taint bucket for this line's group: reuse the last group when it is
        // the same (session_id, group_id), else open a new one.
        let same_group = groups
            .last()
            .is_some_and(|g| g.session_id == session_id && g.group_id == group_id);
        if !same_group {
            groups.push(GroupTaint {
                session_id: session_id.to_string(),
                group_id: group_id.to_string(),
                paths: Vec::new(),
            });
        }
        let cur_group = groups.last_mut().expect("just pushed or matched");

        // `cur_text` = the latest non-empty text block seen so far in THIS message.
        let mut cur_text = String::new();
        // 0-based position of each captured Edit/Write within this message — the
        // second half of the `(message_uuid, edge_ordinal)` dedupe key.
        let mut edge_ordinal: i64 = 0;
        for block in content_blocks {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(t) = block.get("text").and_then(Value::as_str)
                        && !t.trim().is_empty()
                    {
                        cur_text = t.to_string();
                    }
                }
                Some("tool_use") => {
                    let tool = block.get("name").and_then(Value::as_str).unwrap_or("");
                    let input = block.get("input");
                    // Taint set (§2): EVERY path-bearing tool_use — Edit/Write/Read/
                    // Grep/Glob/NotebookEdit — feeds the ratchet, not just edits.
                    if let Some(p) = input.and_then(tool_input_path) {
                        cur_group.paths.push(p.to_string());
                    }
                    if tool != "Edit" && tool != "Write" {
                        continue;
                    }
                    let Some(abs_path) =
                        input.and_then(|i| i.get("file_path")).and_then(Value::as_str)
                    else {
                        continue;
                    };
                    let Some(at) = edited_at else { continue };
                    let rationale = if cur_text.trim().is_empty() {
                        last_text.clone()
                    } else {
                        cur_text.clone()
                    };
                    let (repo_path, rel) = split_repo_relative(abs_path, cwd);
                    edges.push(SessionEdge {
                        session_id: session_id.to_string(),
                        message_uuid: message_uuid.to_string(),
                        edge_ordinal,
                        tool: tool.to_string(),
                        file_path: rel,
                        repo_path,
                        git_branch: git_branch.map(str::to_string),
                        edited_at: at,
                        rationale: truncate_rationale(&rationale),
                        raw_abs_path: abs_path.to_string(),
                        cwd: cwd.map(str::to_string),
                        group_id: group_id.to_string(),
                    });
                    edge_ordinal += 1;
                }
                _ => {}
            }
        }
        if !cur_text.trim().is_empty() {
            last_text = cur_text;
        }
    }
    Ok(TranscriptParse { edges, groups })
}

/// The path an arbitrary tool_use touched, for the taint set: `file_path`
/// (Edit/Write/Read), `path` (Grep/Glob), or `notebook_path` (NotebookEdit).
/// `None` for path-less tools (Bash, Task, …) — outside the taint set by
/// construction (GEAR2_DESIGN §7.2: Bash arg parsing is unreliable, so a
/// `cat ../other-domain/x` is a disclosed residual, not a taint input).
fn tool_input_path(input: &Value) -> Option<&str> {
    input
        .get("file_path")
        .and_then(Value::as_str)
        .or_else(|| input.get("path").and_then(Value::as_str))
        .or_else(|| input.get("notebook_path").and_then(Value::as_str))
}

/// RFC3339 (`2026-06-06T01:13:22.173Z`) → unix epoch seconds.
fn parse_rfc3339(s: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(s).ok().map(|dt| dt.timestamp())
}

/// If `cwd` is a path prefix of `abs_path`, return `(Some(cwd), relative)`;
/// otherwise `(cwd, abs_path)` — a cross-cwd edit stays absolute, still recorded.
///
/// The prefix match is on a COMPONENT boundary, not a raw string prefix: the
/// stripped remainder must be empty (`abs_path == cwd`) or begin with the path
/// separator. Without this, cwd `/ws/proj` editing `/ws/proj-business/
/// plan.md` yields `(repo_path="/ws/proj", file_path="-business/plan.md")` —
/// real SIBLING dirs, so anything routing off the stored pair misroutes the
/// sibling's file deterministically (GEAR2_DESIGN §3 finding B2; pre-existing
/// Gear-0 bug affecting unpartitioned correctness too).
fn split_repo_relative(abs_path: &str, cwd: Option<&str>) -> (Option<String>, String) {
    if let Some(c) = cwd {
        // Tolerate a trailing separator on cwd so the boundary check is exact.
        let c_trimmed = c.trim_end_matches('/');
        if let Some(rest) = abs_path.strip_prefix(c_trimmed)
            && (rest.is_empty() || rest.starts_with('/'))
        {
            let rel = rest.trim_start_matches('/');
            if !rel.is_empty() {
                return (Some(c.to_string()), rel.to_string());
            }
        }
    }
    (cwd.map(str::to_string), abs_path.to_string())
}

/// Keep the rationale bounded — a session's reasoning paragraph, not an essay.
/// (The FTS body is what we search; the first ~500 chars carry the intent.)
fn truncate_rationale(s: &str) -> String {
    const MAX: usize = 500;
    let s = s.trim();
    if s.len() <= MAX {
        return s.to_string();
    }
    let mut end = MAX;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// Insert one edge with already-RESOLVED row values: `repo_path`/`git_branch`
/// are the (possibly §1.4-sanitized) metadata, `rationale` is the stored value
/// (the real prose or the §2 marker), and `fts_include_rationale` decides whether
/// the FTS body carries the rationale (admitted) or the path only (withheld — the
/// marker must never be indexed). Commit-binding runs against the resolved
/// `repo_path`, so a sanitized (NULL) row binds honestly-unbound.
async fn insert_edge(
    conn: &mut sqlx::SqliteConnection,
    edge: &SessionEdge,
    repo_path: Option<&str>,
    git_branch: Option<&str>,
    rationale: &str,
    fts_include_rationale: bool,
    stats: &mut SessionIndexStats,
) -> Result<()> {
    // An edge from a message with no uuid has no stable dedupe identity (all
    // would collide on ("", ordinal)); skip + count rather than risk silently
    // dropping distinct edges under the UNIQUE key.
    if edge.message_uuid.is_empty() {
        stats.edges_skipped_no_uuid += 1;
        return Ok(());
    }

    let commit_id = resolve_capturing_commit(&mut *conn, repo_path, &edge.file_path, edge.edited_at)
        .await?;

    // Additive merge: INSERT OR IGNORE on the (message_uuid, edge_ordinal) dedupe
    // key. RETURNING yields no row when the edge already exists, so a re-index
    // neither double-inserts the row nor re-emits its FTS entry.
    let inserted: Option<(i64,)> = sqlx::query_as(
        r#"
        INSERT OR IGNORE INTO session_edges
            (session_id, message_uuid, edge_ordinal, tool, file_path, repo_path,
             git_branch, edited_at, rationale, commit_id)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        RETURNING id
        "#,
    )
    .bind(&edge.session_id)
    .bind(&edge.message_uuid)
    .bind(edge.edge_ordinal)
    .bind(&edge.tool)
    .bind(&edge.file_path)
    .bind(repo_path)
    .bind(git_branch)
    .bind(edge.edited_at)
    .bind(rationale)
    .bind(commit_id)
    .fetch_optional(&mut *conn)
    .await?;

    let Some(row) = inserted else {
        stats.edges_duplicate += 1;
        return Ok(());
    };

    // FTS body = rationale + path (admitted), so a query finds the thread by its
    // reasoning OR its file; when withheld, path ONLY — the constant marker is
    // never indexed (GEAR2_DESIGN §2 (iii)).
    let body = if fts_include_rationale {
        format!("{} {}", rationale, edge.file_path)
    } else {
        edge.file_path.clone()
    };
    sqlx::query(
        "INSERT INTO search_index (body, kind, rowid_ref, source_table) \
         VALUES (?, 'session_edge', ?, 'session_edges')",
    )
    .bind(&body)
    .bind(row.0)
    .execute(&mut *conn)
    .await?;

    if commit_id.is_some() {
        stats.commit_bound += 1;
    } else {
        stats.commit_unbound += 1;
    }
    stats.edges_indexed += 1;
    match edge.tool.as_str() {
        "Edit" => stats.edits += 1,
        "Write" => stats.writes += 1,
        _ => {}
    }
    Ok(())
}

/// Best-effort session→commit binding: the OLDEST commit on `repo_path` that
/// changed `file_path` at/after `edited_at` — the next commit that captured the
/// edit. `files_changed` is a JSON array of quoted repo-relative paths, so we
/// match on the quoted token. None when `repo_path` is NULL (unbound, incl.
/// §1.4-sanitized rows) or no such commit is indexed.
async fn resolve_capturing_commit(
    conn: &mut sqlx::SqliteConnection,
    repo_path: Option<&str>,
    file_path: &str,
    edited_at: i64,
) -> Result<Option<i64>> {
    let Some(repo) = repo_path else {
        return Ok(None);
    };
    // Match the path as a whole quoted JSON element to avoid partial-name hits.
    let needle = format!("%\"{file_path}\"%");
    let id: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM commit_entries \
         WHERE repo_path = ? AND authored_at >= ? AND files_changed LIKE ? \
         ORDER BY authored_at ASC LIMIT 1",
    )
    .bind(repo)
    .bind(edited_at)
    .bind(needle)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(id)
}

async fn wipe_all_session_edges(pool: &SqlitePool) -> Result<()> {
    sqlx::query("DELETE FROM search_index WHERE source_table = 'session_edges'")
        .execute(pool)
        .await?;
    sqlx::query("DELETE FROM session_edges")
        .execute(pool)
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// The eyeball — `find-session-edge`. FTS5 MATCH over the session map (rationale
// + path), bm25-ranked (find_* semantics). The deliverable: does a CONCEPT query
// ("build provenance") surface the edges the curated note named only by concept —
// the files a path-regex over CLAUDE.md could never reach (D1_SCOPE §10)?
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct SessionEdgeHit {
    pub session_id: String,
    pub tool: String,
    pub file_path: String,
    pub git_branch: Option<String>,
    pub edited_at: i64,
    pub rationale: String,
    pub rank: f64,
    /// The capturing commit, when bound (the session→commit join surfaced).
    pub commit_hash: Option<String>,
    pub commit_summary: Option<String>,
}

/// Return matching session edges ordered by bm25 rank, capped at `limit`. SPIKE:
/// raw query bound to MATCH (no sanitize / OR-broaden — productionizing reuses
/// `mcp::fts5`).
pub async fn find_session_edge(
    pool: &SqlitePool,
    query: &str,
    limit: i64,
) -> Result<Vec<SessionEdgeHit>> {
    type Row = (
        String,
        String,
        String,
        Option<String>,
        i64,
        String,
        f64,
        Option<String>,
        Option<String>,
    );
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT se.session_id, se.tool, se.file_path, se.git_branch, se.edited_at, \
                se.rationale, bm25(search_index) AS rank, \
                ce.commit_hash, ce.message_summary \
         FROM search_index s \
         JOIN session_edges se ON se.id = s.rowid_ref AND s.source_table = 'session_edges' \
         LEFT JOIN commit_entries ce ON ce.id = se.commit_id \
         WHERE s.body MATCH ? \
         ORDER BY rank \
         LIMIT ?",
    )
    .bind(query)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| SessionEdgeHit {
            session_id: r.0,
            tool: r.1,
            file_path: r.2,
            git_branch: r.3,
            edited_at: r.4,
            rationale: r.5,
            rank: r.6,
            commit_hash: r.7,
            commit_summary: r.8,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domain_router_visible_rejects_relative_paths_fail_narrow() {
        // #5d: a relative tool-input path canonicalizes against the indexer's cwd,
        // not the session's, so it must never be admitted — fail-narrow to false.
        let cfg: DomainConfig = toml::from_str("[domains]\nalpha = [\"proj-a\"]\n").unwrap();
        let ws = Path::new("/ws");
        let router = DomainRouter {
            config: &cfg,
            workspace: ws,
            domain: "alpha",
        };
        // Relative paths are rejected outright — even ones that "look" in-domain.
        assert!(!router.visible("proj-a/src/x.rs"));
        assert!(!router.visible("src/x.rs"));
        // An absolute in-domain path is still visible (the guard didn't over-reject;
        // canonicalize falls back to the lexical path for a nonexistent target).
        assert!(router.visible("/ws/proj-a/src/x.rs"));
        // An absolute foreign path stays not-visible.
        assert!(!router.visible("/ws/proj-b/y.rs"));
    }

    #[test]
    fn split_repo_relative_strips_cwd_prefix() {
        let (repo, rel) = split_repo_relative(
            "/Users/x/workspace/nibdex/src/mcp/check.rs",
            Some("/Users/x/workspace/nibdex"),
        );
        assert_eq!(repo.as_deref(), Some("/Users/x/workspace/nibdex"));
        assert_eq!(rel, "src/mcp/check.rs");
    }

    #[test]
    fn split_repo_relative_keeps_absolute_when_outside_cwd() {
        let (repo, rel) =
            split_repo_relative("/etc/hosts", Some("/Users/x/workspace/nibdex"));
        // cwd recorded, but the path is left absolute since it isn't under cwd.
        assert_eq!(repo.as_deref(), Some("/Users/x/workspace/nibdex"));
        assert_eq!(rel, "/etc/hosts");
    }

    #[test]
    fn split_repo_relative_handles_missing_cwd() {
        let (repo, rel) = split_repo_relative("/some/abs/path.rs", None);
        assert_eq!(repo, None);
        assert_eq!(rel, "/some/abs/path.rs");
    }

    #[test]
    fn split_repo_relative_rejects_sibling_string_prefix() {
        // B2: cwd is only a STRING prefix of a real sibling dir — must NOT strip.
        // `/ws/proj` editing `/ws/proj-business/plan.md` stays absolute
        // (cwd still recorded), so downstream routing can't misplace the sibling.
        let (repo, rel) = split_repo_relative(
            "/ws/proj-business/plan.md",
            Some("/ws/proj"),
        );
        assert_eq!(repo.as_deref(), Some("/ws/proj"));
        assert_eq!(rel, "/ws/proj-business/plan.md");
    }

    #[test]
    fn split_repo_relative_tolerates_trailing_slash_cwd() {
        let (repo, rel) = split_repo_relative(
            "/ws/nibdex/src/main.rs",
            Some("/ws/nibdex/"),
        );
        assert_eq!(repo.as_deref(), Some("/ws/nibdex/"));
        assert_eq!(rel, "src/main.rs");
    }

    #[test]
    fn parse_rfc3339_to_epoch() {
        // 2026-06-06T01:13:22.173Z — the transcript timestamp format.
        let epoch = parse_rfc3339("2026-06-06T01:13:22.173Z").unwrap();
        // Round-trips to the same instant (sub-second truncated).
        assert_eq!(epoch, 1780708402);
    }

    #[test]
    fn truncate_rationale_bounds_and_keeps_char_boundary() {
        let short = "a tidy reason";
        assert_eq!(truncate_rationale(short), short);
        let long = "x".repeat(600);
        let out = truncate_rationale(&long);
        assert!(out.chars().count() <= 501); // 500 + ellipsis
        assert!(out.ends_with('…'));
    }

    // ------------------------------------------------------------------------
    // GEAR 2 — the invariant test (GEAR2_DESIGN §4). THE RELEASE GATE: a domain's
    // db must contain ZERO of another domain's files, rationale prose, repo/branch
    // metadata, or FTS body — mechanical and needle-testable. Exercises all seven
    // §4 scenarios (control / cross-domain message / fallback carrier / read-
    // laundering B1 / cross-cwd / shared+private / subagents decoy) plus the
    // string-prefix sibling trap (proj-a-extra), run per-domain into two pools.
    // ------------------------------------------------------------------------

    use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};

    async fn fresh_pool() -> SqlitePool {
        use std::str::FromStr;
        let options = SqliteConnectOptions::from_str("sqlite::memory:")
            .unwrap()
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Memory);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    /// COUNT of rows in `pool` where `needle` appears in ANY session_edges text
    /// column OR the FTS body of a session_edges row — the invariant needle sweep.
    async fn needle_hits(pool: &SqlitePool, needle: &str) -> i64 {
        let pat = format!("%{needle}%");
        sqlx::query_scalar::<_, i64>(
            "SELECT \
               (SELECT COUNT(*) FROM session_edges \
                 WHERE rationale LIKE ? OR file_path LIKE ? \
                    OR IFNULL(repo_path,'') LIKE ? OR IFNULL(git_branch,'') LIKE ?) \
             + (SELECT COUNT(*) FROM search_index \
                 WHERE source_table = 'session_edges' AND body LIKE ?)",
        )
        .bind(&pat)
        .bind(&pat)
        .bind(&pat)
        .bind(&pat)
        .bind(&pat)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn edge_count(pool: &SqlitePool) -> i64 {
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM session_edges")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    /// The stored rationale of the single edge whose file_path matches `like`.
    async fn rationale_where(pool: &SqlitePool, like: &str) -> Option<String> {
        sqlx::query_scalar::<_, String>(
            "SELECT rationale FROM session_edges WHERE file_path LIKE ? LIMIT 1",
        )
        .bind(like)
        .fetch_optional(pool)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn gear2_domain_isolation_invariant() {
        use serde_json::{json, Value};

        // --- synthetic 2-domain workspace (proj-shared under both; proj-a-extra
        //     an UNLABELED sibling — the string-prefix trap) ---
        let ws_tmp = tempfile::tempdir().unwrap();
        let ws = ws_tmp.path().canonicalize().unwrap();
        for sub in ["proj-a", "proj-b", "proj-shared", "proj-a-extra"] {
            std::fs::create_dir_all(ws.join(sub)).unwrap();
        }
        // A real file per referenced path so `canonicalize` in the router resolves.
        for rel in [
            "proj-a/alpha.rs",
            "proj-a/x.rs",
            "proj-a/z.rs",
            "proj-a/l.rs",
            "proj-a/cc.rs",
            "proj-a/sp.rs",
            "proj-a/decoy.rs",
            "proj-b/y.rs",
            "proj-b/beta.rs",
            "proj-shared/s.rs",
            "proj-a-extra/e.rs",
        ] {
            std::fs::write(ws.join(rel), "x").unwrap();
        }
        std::fs::write(
            ws.join(".nibdex-domains.toml"),
            "[domains]\nalpha = [\"proj-a\", \"proj-shared\"]\nbeta = [\"proj-b\", \"proj-shared\"]\n",
        )
        .unwrap();

        let wss = ws.to_string_lossy().to_string();
        let abs = |rel: &str| format!("{wss}/{rel}");

        // --- transcript builders (one content block per line — the real format) ---
        let mut ts = 0;
        let mut line = |session: &str, uuid: &str, group: &str, cwd: &str, branch: &str, block: Value| -> Value {
            ts += 1;
            json!({
                "type": "assistant",
                "sessionId": session,
                "uuid": uuid,
                "cwd": cwd,
                "gitBranch": branch,
                "timestamp": format!("2026-07-12T10:00:{ts:02}.000Z"),
                "message": { "id": group, "content": [ block ] }
            })
        };
        let txt = |t: &str| json!({"type": "text", "text": t});
        let edit = |p: &str| json!({"type": "tool_use", "name": "Edit", "input": {"file_path": p}});
        let read = |p: &str| json!({"type": "tool_use", "name": "Read", "input": {"file_path": p}});

        let a = abs("proj-a"); // cwd launched from inside proj-a (keeps metadata)
        let root = &wss; // cwd = workspace root (metadata NULLed — §1.4 / §8)
        let pb = abs("proj-b"); // a FOREIGN checkout cwd

        let lines: Vec<Value> = vec![
            // (1) CONTROL — clean single-domain, launched from proj-a: rationale
            //     verbatim, metadata KEPT.
            line("clean", "c1", "g1", &a, "main", txt("reasoning about alpha_widget")),
            line("clean", "c2", "g1", &a, "main", edit(&abs("proj-a/alpha.rs"))),
            // (2) CROSS-DOMAIN MESSAGE — one group edits proj-a AND proj-b, prose
            //     names both tokens: taints "cross"; a-edge kept w/ MARKER, b-edge
            //     dropped from alpha.
            line("cross", "x1", "g2", root, "main", txt("touch alpha_widget and beta_gadget across proj-a and /ws/proj-b")),
            line("cross", "x2", "g2", root, "main", edit(&abs("proj-a/x.rs"))),
            line("cross", "x3", "g2", root, "main", edit(&abs("proj-b/y.rs"))),
            // (3) FALLBACK CARRIER — a later text-less edit inherits g2's cross
            //     prose via last_text; must still be withheld (tainted).
            line("cross", "x4", "g3", root, "main", edit(&abs("proj-a/z.rs"))),
            // (4) READ-LAUNDERING (B1) — Read proj-b taints, then a later group
            //     quotes beta_gadget while editing proj-a: prose must NOT survive.
            line("launder", "l1", "g4", root, "main", read(&abs("proj-b/beta.rs"))),
            line("launder", "l2", "g5", root, "main", txt("now editing proj-a, recalling beta_gadget")),
            line("launder", "l3", "g5", root, "main", edit(&abs("proj-a/l.rs"))),
            // (5) CROSS-CWD — cwd/branch name a FOREIGN checkout while editing an
            //     in-domain file: routes to alpha, but repo_path + branch NULLed.
            line("crosscwd", "cc1", "g6", root, "main", txt("cross cwd note")),
            line("crosscwd", "cc2", "g6", &pb, "beta-secret-branch", edit(&abs("proj-a/cc.rs"))),
            // (6) SHARED + PRIVATE — fresh clean session edits proj-shared + proj-a:
            //     alpha keeps both rationales; beta gets only the shared edge, MARKER.
            line("shared", "s1", "g7", root, "main", txt("shared_cog and alpha_widget together")),
            line("shared", "s2", "g7", root, "main", edit(&abs("proj-shared/s.rs"))),
            line("shared", "s3", "g7", root, "main", edit(&abs("proj-a/sp.rs"))),
            // (extra) the string-prefix sibling trap — proj-a-extra must route NOWHERE.
            line("extra", "e1", "g8", root, "main", txt("extra_thing note")),
            line("extra", "e2", "g8", root, "main", edit(&abs("proj-a-extra/e.rs"))),
        ];

        // --- write the transcript + a subagents/ decoy (must be ignored) ---
        let projects = tempfile::tempdir().unwrap();
        let slug = "-ws";
        let slug_dir = projects.path().join(slug);
        std::fs::create_dir_all(slug_dir.join("subagents")).unwrap();
        let body: String = lines
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(slug_dir.join("session.jsonl"), body).unwrap();
        let decoy = [
            line("decoy", "d1", "gd", root, "main", txt("decoy_token here")),
            line("decoy", "d2", "gd", root, "main", edit(&abs("proj-a/decoy.rs"))),
        ];
        std::fs::write(
            slug_dir.join("subagents/agent-x.jsonl"),
            decoy.iter().map(Value::to_string).collect::<Vec<_>>().join("\n"),
        )
        .unwrap();

        // --- run the session pass per-domain into two FRESH pools ---
        let alpha = fresh_pool().await;
        let beta = fresh_pool().await;
        let a_stats =
            index_sessions(&alpha, projects.path(), Some(slug), true, &ws, Some("alpha"))
                .await
                .unwrap();
        index_sessions(&beta, projects.path(), Some(slug), true, &ws, Some("beta"))
            .await
            .unwrap();

        // === THE INVARIANT NEEDLE SWEEP — zero foreign content in alpha.db ===
        for needle in ["beta_gadget", "proj-b", "beta-secret-branch", "proj-a-extra", "decoy_token"] {
            assert_eq!(needle_hits(&alpha, needle).await, 0, "alpha leaked {needle}");
        }
        // symmetric — zero alpha content in beta.db
        for needle in ["alpha_widget", "proj-a/", "decoy_token"] {
            assert_eq!(needle_hits(&beta, needle).await, 0, "beta leaked {needle}");
        }

        // === edge presence ===
        // alpha keeps: control + g2-a + g3 + g5 + g6 + g7-shared + g7-a = 7
        assert_eq!(edge_count(&alpha).await, 7);
        // beta keeps only proj-b/y + proj-shared/s = 2
        assert_eq!(edge_count(&beta).await, 2);
        // counters: alpha dropped proj-b/y + proj-a-extra/e = 2; withheld g2-a,g3,g5 = 3
        assert_eq!(a_stats.edges_dropped_foreign_domain, 2);
        assert_eq!(a_stats.rationales_withheld, 3);

        // === rationale bodies ===
        // control verbatim; metadata kept (cwd was proj-a, in-domain)
        let ctrl = sqlx::query_as::<_, (String, Option<String>, Option<String>)>(
            "SELECT rationale, repo_path, git_branch FROM session_edges WHERE file_path = 'alpha.rs'",
        )
        .fetch_one(&alpha)
        .await
        .unwrap();
        assert_eq!(ctrl.0, "reasoning about alpha_widget");
        assert!(ctrl.1.is_some(), "in-domain cwd repo_path must be kept");
        assert_eq!(ctrl.2.as_deref(), Some("main"));
        // tainted proj-a edits carry the exact marker
        assert_eq!(rationale_where(&alpha, "%z.rs").await.as_deref(), Some(WITHHELD_MARKER));
        assert_eq!(rationale_where(&alpha, "%l.rs").await.as_deref(), Some(WITHHELD_MARKER));
        // shared+private: both rationales verbatim in alpha, MARKER in beta
        assert!(rationale_where(&alpha, "%proj-shared%").await.unwrap().contains("shared_cog"));
        assert_eq!(rationale_where(&beta, "%proj-shared%").await.as_deref(), Some(WITHHELD_MARKER));

        // === cross-cwd row: metadata NULLed (foreign checkout + secret branch) ===
        let cc = sqlx::query_as::<_, (Option<String>, Option<String>)>(
            "SELECT repo_path, git_branch FROM session_edges WHERE file_path LIKE '%cc.rs'",
        )
        .fetch_one(&alpha)
        .await
        .unwrap();
        assert_eq!(cc, (None, None));

        // === FTS behavior ===
        assert!(!find_session_edge(&alpha, "alpha_widget", 10).await.unwrap().is_empty());
        assert!(find_session_edge(&alpha, "beta_gadget", 10).await.unwrap().is_empty());
        // the marker is never indexed → its words are unsearchable
        assert!(find_session_edge(&alpha, "withheld", 10).await.unwrap().is_empty());
        // subagents decoy was not ingested at all
        assert!(find_session_edge(&alpha, "decoy_token", 10).await.unwrap().is_empty());
    }

    /// Regression for the cross-session `last_text` bleed (Gear 2 review, P0).
    /// Two sessions share ONE transcript file: session "beta_sess" names a foreign
    /// token in its prose, then a SEPARATE clean session "alpha_sess" makes a
    /// TEXT-LESS in-domain edit. That edit's fallback rationale must be scoped to
    /// its own session (empty), never inherit session 1's prose — otherwise the
    /// clean session's admission laundered a foreign token into alpha.db.
    #[tokio::test]
    async fn gear2_last_text_does_not_bleed_across_sessions() {
        use serde_json::{json, Value};

        let ws_tmp = tempfile::tempdir().unwrap();
        let ws = ws_tmp.path().canonicalize().unwrap();
        for sub in ["proj-a", "proj-b"] {
            std::fs::create_dir_all(ws.join(sub)).unwrap();
        }
        for rel in ["proj-a/x.rs", "proj-b/beta.rs"] {
            std::fs::write(ws.join(rel), "x").unwrap();
        }
        std::fs::write(
            ws.join(".nibdex-domains.toml"),
            "[domains]\nalpha = [\"proj-a\"]\nbeta = [\"proj-b\"]\n",
        )
        .unwrap();

        let wss = ws.to_string_lossy().to_string();
        let abs = |rel: &str| format!("{wss}/{rel}");

        let mut ts = 0;
        let mut line = |session: &str, uuid: &str, group: &str, block: Value| -> Value {
            ts += 1;
            json!({
                "type": "assistant",
                "sessionId": session,
                "uuid": uuid,
                "cwd": wss,
                "gitBranch": "main",
                "timestamp": format!("2026-07-12T11:00:{ts:02}.000Z"),
                "message": { "id": group, "content": [ block ] }
            })
        };
        let txt = |t: &str| json!({"type": "text", "text": t});
        let edit = |p: &str| json!({"type": "tool_use", "name": "Edit", "input": {"file_path": p}});

        // Session 1 (beta_sess): prose names a foreign token, then edits proj-b —
        //   this leaves `last_text` = the foreign prose.
        // Session 2 (alpha_sess): a text-less edit to an in-domain proj-a file. Its
        //   fallback rationale MUST reset at the session boundary.
        let lines: Vec<Value> = vec![
            line("beta_sess", "b1", "g1", txt("planning beta_gadget rotation")),
            line("beta_sess", "b2", "g1", edit(&abs("proj-b/beta.rs"))),
            line("alpha_sess", "a1", "g2", edit(&abs("proj-a/x.rs"))),
        ];

        let projects = tempfile::tempdir().unwrap();
        let slug = "-ws";
        let slug_dir = projects.path().join(slug);
        std::fs::create_dir_all(&slug_dir).unwrap();
        std::fs::write(
            slug_dir.join("session.jsonl"),
            lines.iter().map(Value::to_string).collect::<Vec<_>>().join("\n"),
        )
        .unwrap();

        let alpha = fresh_pool().await;
        index_sessions(&alpha, projects.path(), Some(slug), true, &ws, Some("alpha"))
            .await
            .unwrap();

        // The in-domain edge is kept (routes off proj-a/x.rs) ...
        assert_eq!(edge_count(&alpha).await, 1);
        // ... but its rationale did NOT inherit session 1's foreign prose.
        assert_eq!(rationale_where(&alpha, "%x.rs").await.as_deref(), Some(""));
        // THE INVARIANT: no foreign token anywhere in alpha.db.
        assert_eq!(
            needle_hits(&alpha, "beta_gadget").await,
            0,
            "cross-session last_text bleed into alpha.db"
        );
    }
}
