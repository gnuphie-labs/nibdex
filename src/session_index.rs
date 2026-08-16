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
//! nearest preceding assistant text (the RATIONALE), and the per-line `gitBranch`,
//! `cwd`, and `timestamp` — stored as metadata (`cwd` is where Claude was
//! launched, not necessarily a repo root).
//!
//! It then makes a best-effort session-to-commit binding: the edit's absolute
//! target path is resolved against the repo roots this db holds commits for
//! (innermost containing root wins), and the oldest commit on that repo whose
//! `files_changed` names the repo-relative path with a COMMITTER date at/after
//! the edit is the capturing commit. `gitBranch` is not a predicate — see
//! `resolve_capturing_commit`.
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
    /// Already-indexed edges that were stored unbound and have now acquired their
    /// capturing commit, because that commit did not exist when they were first
    /// seen. The normal case for anything indexed before it was committed.
    pub edges_late_bound: usize,
    /// Edges from a uuid-less message — cannot be safely deduped, so skipped.
    pub edges_skipped_no_uuid: usize,
    /// Edges whose line carried no parseable RFC-3339 `timestamp` — `edited_at`
    /// is the recency key and the commit-binding key, so such an edit cannot be
    /// stored honestly and is skipped. Counted so the loss is visible rather than
    /// silent (RC1 review, sev-2).
    pub edges_skipped_no_timestamp: usize,
    /// Domain mode only: edges whose target file is not in the indexed domain —
    /// written to no db (GEAR2_DESIGN §1.2). Over-drop is a visibility cost, not
    /// a leak; surfaced so the fail-narrow rate is a number, not a vibe.
    pub edges_dropped_foreign_domain: usize,
    /// Domain mode only: edges whose rationale was replaced by the constant
    /// withheld-marker because their session had touched another domain's tree
    /// (the ratchet, GEAR2_DESIGN §2). Read the dogfood before adding any escape
    /// hatch (§7.4).
    pub rationales_withheld: usize,
    /// [`SessionScope::Workspace`] only: edges whose line `cwd` is not under the
    /// workspace root — another workspace's session, or a line carrying no `cwd`
    /// at all. Dropped before any routing runs. Surfaced so the derived scope's
    /// reach is a number rather than an assumption.
    pub edges_dropped_foreign_workspace: usize,
    /// [`SessionScope::Workspace`] only: edges from an in-workspace session whose
    /// TARGET is neither in the workspace nor domain-neutral — this workspace's
    /// own session reaching into someone else's tree (see [`target_in_scope`]).
    pub edges_dropped_foreign_target: usize,
    /// Transcripts that could not be READ at all and were skipped. Distinct from
    /// `lines_parse_err`, which is a malformed line inside a readable file.
    ///
    /// The transcript root is machine-global, so a pass routinely walks files
    /// this workspace has no claim on: another user's, one with restrictive
    /// permissions, one holding non-UTF-8 bytes, or one Claude Code pruned
    /// between listing the directory and opening the file (a live race, since
    /// transcripts rotate). None of those is a reason to fail an index run.
    pub transcripts_unreadable: usize,
    pub elapsed_ms: u128,
}

/// Which transcripts (and which of their edges) a pass admits.
///
/// The transcript root is machine-global — `~/.claude/projects/<slug>/` holds a
/// dir per cwd Claude was launched from, and an employer's workspace sits in the
/// same parent as a personal one. So scope is never implicit.
#[derive(Debug, Clone, Copy)]
pub enum SessionScope<'a> {
    /// Exactly one slug dir, by name. The explicit operator form.
    Slug(&'a str),
    /// Every slug under the transcript root — machine-global, no filtering.
    /// Deliberately hard to reach: on a shared box this crosses workspaces.
    AllSlugs,
    /// DERIVED from the workspace root, and the scope `nibdex index` uses.
    ///
    /// Every slug is *read*, but an edge is admitted only if its own line's `cwd`
    /// is inside the workspace. This is the structural form of the rule: there is
    /// no flag to forget, and it is correct in the two places a slug-name rule is
    /// not — "own slug" is not one slug (this workspace already spans four cwds
    /// across `workspace`, `workspace/nibdex`, `workspace/team-docs`,
    /// `workspace/clearview-business`), and the slug encoding is lossy (`/`, `_`
    /// and `.` all become `-`, so `-Users-me-workspace-old` is ambiguous between
    /// `~/workspace-old` and `~/workspace/old` and cannot be decoded back).
    Workspace,
}

impl SessionScope<'_> {
    /// The single slug dir to read, if this scope names one.
    fn slug(&self) -> Option<&str> {
        match self {
            SessionScope::Slug(s) => Some(s),
            SessionScope::AllSlugs | SessionScope::Workspace => None,
        }
    }
}

/// Is `raw` inside `workspace` (already canonicalized)?
///
/// Component-wise via `Path::starts_with`, never `str::starts_with` — the sibling
/// trap this project has now been bitten by twice (`split_repo_relative`, the
/// `proj-a-extra` prefix decoy) says a string prefix would admit
/// `~/workspace-old` under `~/workspace`.
///
/// Fail-narrow in all three failure modes: an empty value, a `..` that escapes
/// what we can resolve, and a non-absolute path all return false. A relative path
/// would canonicalize against the INDEXER's cwd rather than the session's — the
/// same reasoning as [`DomainRouter::visible`].
/// Resolve `p` against the filesystem as far as it actually exists, keeping the
/// rest lexically.
///
/// Plain `canonicalize` is all-or-nothing, and edges routinely name files that
/// are GONE by index time — a scratch file written and removed inside the
/// session. For those, `canonicalize` fails and a caller falling back to the raw
/// lexical path is then comparing an unresolved path against a resolved
/// workspace root. Under a symlinked prefix (macOS `/tmp` → `/private/tmp`, and
/// this project's own `/tmp/nibdex-verify` release clone) the two never match,
/// so a perfectly in-workspace edge is silently dropped as foreign.
///
/// Canonicalizing the deepest existing ancestor fixes both halves: symlinks in
/// the prefix resolve, and the missing tail is preserved.
fn resolve_best_effort(p: &Path) -> PathBuf {
    let mut suffix: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = p.to_path_buf();
    loop {
        if let Ok(c) = cur.canonicalize() {
            let mut out = c;
            for part in suffix.iter().rev() {
                out.push(part);
            }
            return out;
        }
        match (cur.parent().map(Path::to_path_buf), cur.file_name()) {
            (Some(parent), Some(name)) => {
                suffix.push(name.to_os_string());
                cur = parent;
            }
            // Reached the root without finding anything that exists.
            _ => return p.to_path_buf(),
        }
    }
}

/// Is `resolved` (already run through [`resolve_best_effort`]) inside `root`?
///
/// BOTH SIDES must be resolved the same way or the comparison is meaningless —
/// and getting that wrong is silent, since a mismatch just reads as "foreign".
/// macOS makes it concrete: `/home` is a firmlink to `/System/Volumes/Data/home`,
/// so a resolved target under it never matches an unresolved root.
///
/// The unresolved check runs FIRST because callers already hand us a canonical
/// workspace, so the common case costs no extra syscall; the resolve only
/// happens when that misses.
fn resolved_under(resolved: &Path, root: &Path) -> bool {
    resolved.starts_with(root) || resolved.starts_with(resolve_best_effort(root))
}

fn path_in_workspace(raw: &str, workspace: &Path) -> bool {
    let p = Path::new(raw);
    if !p.is_absolute() {
        return false;
    }
    let Some(norm) = normalize_lexical(p) else {
        return false;
    };
    resolved_under(&resolve_best_effort(&norm), workspace)
}

/// Was the SESSION working inside this workspace? (`None` — a line carrying no
/// `cwd` — is not ours.)
fn cwd_in_workspace(cwd: Option<&str>, workspace: &Path) -> bool {
    cwd.is_some_and(|c| path_in_workspace(c, workspace))
}

/// May this edge's TARGET be written into this workspace's index?
///
/// Judged on the UNTOUCHED `input.file_path`, exactly as domain routing is
/// (GEAR2_DESIGN §1.1) — never on the split repo-relative form, which has already
/// lost the information the decision needs.
///
/// The session's cwd alone is not sufficient. A session sitting in this workspace
/// can and does write elsewhere: on the author's own box, four edges from
/// `~/workspace` sessions wrote briefings into an employer tree, carrying host
/// paths and ports in their rationale prose. Scoping by cwd only would file those
/// under the personal index.
///
/// But a flat "target must be inside the workspace" is too blunt in the other
/// direction — it also discards the 14 edges into this workspace's own
/// `~/.claude/…/memory/` (the session↔memory link D1_SCOPE §10 gear 7 called out
/// as a bonus of transcript indexing) and its own scratchpad. So the escape is
/// the SAME [`is_neutral_path`] set the ratchet already uses: places whose
/// contents are this workspace's own scaffolding or scratch, and which therefore
/// cannot be another domain's IP. One predicate, two consumers — a second,
/// slightly-different notion of "neutral" is how the two drift apart.
fn target_in_scope(raw_abs_path: &str, workspace: &Path, cwd: Option<&str>) -> bool {
    if path_in_workspace(raw_abs_path, workspace) || is_neutral_path(raw_abs_path, workspace) {
        return true;
    }

    // THE SESSION'S OWN Claude directory, which is not always the workspace
    // ROOT's. `is_neutral_path` keys rules (2) and (3) off
    // `claude_slug_dir(workspace)` — one slug, encoding the workspace root path.
    // But a workspace spans one slug per directory Claude was launched from (the
    // very fact that makes the scope rule necessary), so a session started in
    // `<ws>/nibdex` keeps its transcripts and its `memory/` under a SIBLING slug.
    // Component-wise matching correctly refuses that sibling, which would drop
    // the session↔memory edge the neutral escape exists to preserve.
    //
    // Safe because `cwd` has already been proven in-workspace by the caller: we
    // are admitting this workspace's own Claude directory, reached through the
    // session that legitimately owns it.
    let Some(cwd) = cwd else { return false };
    let Some(slug_dir) = crate::indexer::claude_slug_dir(Path::new(cwd)) else {
        return false;
    };
    let Some(norm) = normalize_lexical(Path::new(raw_abs_path)) else {
        return false;
    };
    Path::new(raw_abs_path).is_absolute()
        && resolved_under(&resolve_best_effort(&norm), &slug_dir)
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
    /// Per-(session, cwd) retrieval tallies — the denominator for nibdex's own
    /// usefulness. Counts only; see the `session_activity` migration. Keyed by
    /// the tool-call line's `cwd` as well as its session so `index_sessions` can
    /// apply the SAME workspace/domain scope it applies to edges: without that
    /// the denominator counted every session on the machine (RC1 review 1.3).
    activity: HashMap<(String, Option<String>), SessionActivity>,
}

/// How much retrieval a session did, and how much of it nibdex served.
#[derive(Default, Clone, Copy)]
struct SessionActivity {
    first_seen: i64,
    retrieval_calls: i64,
    nibdex_calls: i64,
}

/// Shell commands that ARE a retrieval, whatever wraps them.
///
/// Counting only the dedicated search tools understates the competition badly:
/// in practice most retrieval happens as a `grep` inside a shell call, and that
/// is precisely the work nibdex exists to serve. Measured on one real session:
/// 4 `Grep`-tool calls against 70 shell searches.
const SHELL_SEARCH: [&str; 6] = ["grep ", "rg ", "ripgrep", "find ", "ack ", "ag "];

/// Classify one `tool_use` by whether it is retrieval, and whose.
///
/// Deliberately crude and deliberately generous to the COMPETITION: a shell
/// command that merely contains a search counts as retrieval even if it also did
/// something else. An honest denominator errs toward making nibdex look worse,
/// because the failure being measured is nibdex going unused.
fn classify_tool_call(name: &str, input: Option<&Value>) -> Option<ToolClass> {
    if name.to_ascii_lowercase().contains("nibdex") {
        return Some(ToolClass::Nibdex);
    }
    if name == "Grep" || name == "Glob" {
        return Some(ToolClass::Retrieval);
    }
    if name == "Bash" {
        let cmd = input
            .and_then(|i| i.get("command"))
            .and_then(Value::as_str)
            .unwrap_or("");
        // A search tool must lead some pipeline segment. Otherwise it is
        // filtering another command's output — `cargo test | grep "^test result"`,
        // `git log | grep -c` — which was never a question nibdex could answer.
        //
        // Measured on one real session: 120 of 173 grep-bearing calls were output
        // filters. Counting them inflated the denominator 2.3x. An honest
        // denominator has to be honest in BOTH directions; over-counting the
        // competition is still a wrong number, even though it flatters nobody.
        for seg in cmd.split([';', '&']) {
            let head = seg.split('|').next().unwrap_or("").trim_start();
            let head = head.strip_prefix("sudo ").unwrap_or(head).trim_start();
            if SHELL_SEARCH.iter().any(|t| head.starts_with(t)) {
                return Some(ToolClass::Retrieval);
            }
        }
    }
    None
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ToolClass {
    Retrieval,
    Nibdex,
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

/// Index transcripts under `projects_dir`, admitting them per `scope` (see
/// [`SessionScope`] — one slug, every slug, or the derived workspace scope).
///
/// ADDITIVE MERGE (docs/SESSION_SCOPE_DESIGN.md §5): re-running only ever grows
/// the corpus — `INSERT OR IGNORE` on the `(message_uuid, edge_ordinal)` dedupe
/// key skips already-indexed edges, so retention-expired transcripts' edges are
/// preserved, never erased. `rebuild = true` re-derives the edges of every
/// session this pass admits — each such session's prior rows (+ FTS) are dropped
/// just before its first edge is re-inserted — and touches nothing else. It is
/// NOT a table wipe: that erased other slugs'/domains'/rotated transcripts' edges
/// from a shared db on a `--slug A --rebuild` (RC1 review, sev-2). The whole pass
/// runs in one transaction (idempotent, and no per-row autocommit).
pub async fn index_sessions(
    pool: &SqlitePool,
    projects_dir: &Path,
    scope: SessionScope<'_>,
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

    let transcripts = collect_transcripts(projects_dir, scope.slug())?;
    let mut sessions: HashSet<String> = HashSet::new();
    // Repo roots this db holds commits for — the binding candidates (1.1). Read
    // once: `full_scan` runs the commits pass before this one, so they are current.
    let roots = known_repo_roots(pool).await?;
    // `rebuild`: sessions whose prior rows have been dropped this pass. One drop
    // per session, however many transcript files (resumes) carry it — a second
    // drop would erase the rows the first file just re-inserted.
    let mut rebuilt: HashSet<String> = HashSet::new();

    let mut tx = pool.begin().await?;
    for path in &transcripts {
        stats.transcripts_seen += 1;
        // An unreadable transcript is SKIPPED, never fatal. The same reasoning
        // that makes a torn final line non-fatal applies with more force at the
        // file level: the transcript root is machine-global, so this walk touches
        // files belonging to other workspaces and other users, any of which may
        // be permission-denied, non-UTF-8, or pruned by Claude Code between the
        // directory listing and the open. Propagating would let one such file
        // fail an entire `nibdex index` — including the five corpora that already
        // succeeded.
        let parse = match parse_transcript(path, &mut stats) {
            Ok(p) => p,
            Err(_) => {
                stats.transcripts_unreadable += 1;
                continue;
            }
        };
        // Domain mode: precompute this transcript's per-group rationale admission
        // (the ratchet, §2) BEFORE emitting any edge — a group is evaluated
        // atomically, so an edit can't be admitted until its whole group is known.
        // Persist the retrieval tallies — SCOPED exactly like the edges below.
        // The transcript root is machine-global, so without this filter the
        // adoption denominator counted every workspace's sessions (a scratch
        // workspace with zero in-scope edits reported 13 sessions / 221 retrieval
        // calls) and a `--domain` db received other domains' session ids. A tally
        // line counts iff its cwd is inside `--workspace` (workspace scope) and,
        // in domain mode, inside this domain's labeled subdirs. Per-cwd tallies
        // for one session are summed after filtering, then MAX-merged: a
        // transcript that has since rotated away must not zero the counts it
        // contributed, mirroring the additive merge on the edges themselves.
        let mut scoped_activity: HashMap<&str, SessionActivity> = HashMap::new();
        for ((sid, cwd), act) in &parse.activity {
            if matches!(scope, SessionScope::Workspace)
                && !cwd_in_workspace(cwd.as_deref(), &workspace)
            {
                continue;
            }
            if let Some(r) = router.as_ref()
                && !cwd.as_deref().is_some_and(|c| r.visible(c))
            {
                continue;
            }
            let a = scoped_activity.entry(sid.as_str()).or_default();
            if a.first_seen == 0 || (act.first_seen != 0 && act.first_seen < a.first_seen) {
                a.first_seen = act.first_seen;
            }
            a.retrieval_calls += act.retrieval_calls;
            a.nibdex_calls += act.nibdex_calls;
        }
        for (sid, act) in &scoped_activity {
            sqlx::query(
                "INSERT INTO session_activity \
                   (session_id, first_seen, retrieval_calls, nibdex_calls) \
                 VALUES (?, ?, ?, ?) \
                 ON CONFLICT(session_id) DO UPDATE SET \
                   first_seen      = MIN(first_seen, excluded.first_seen), \
                   retrieval_calls = MAX(retrieval_calls, excluded.retrieval_calls), \
                   nibdex_calls    = MAX(nibdex_calls, excluded.nibdex_calls)",
            )
            .bind(sid)
            .bind(act.first_seen)
            .bind(act.retrieval_calls)
            .bind(act.nibdex_calls)
            .execute(&mut *tx)
            .await?;
        }
        let admit = router.as_ref().map(|r| admission_map(&parse.groups, r));
        for edge in &parse.edges {
            // Workspace scope: drop foreign edges BEFORE anything else observes
            // them — before `sessions` (so a wholly-foreign transcript does not
            // inflate `sessions_seen`) and before routing. Per-EDGE, not per-
            // transcript, matching Gear 2's per-edge routing granularity: `cwd`
            // varies line-to-line *within* one transcript, so a whole-file verdict
            // would be wrong in both directions.
            //
            // BOTH ends must be in scope — where the session was working AND what
            // it wrote. The two counters stay separate because they mean different
            // things: a foreign SESSION is another workspace's work (expected, and
            // most of the volume), while a foreign TARGET is this workspace's own
            // session reaching outside, which is the interesting number.
            if matches!(scope, SessionScope::Workspace) {
                if !cwd_in_workspace(edge.cwd.as_deref(), &workspace) {
                    stats.edges_dropped_foreign_workspace += 1;
                    continue;
                }
                if !target_in_scope(&edge.raw_abs_path, &workspace, edge.cwd.as_deref()) {
                    stats.edges_dropped_foreign_target += 1;
                    continue;
                }
            }
            sessions.insert(edge.session_id.clone());
            if rebuild && rebuilt.insert(edge.session_id.clone()) {
                wipe_session_edges_for(&mut tx, &edge.session_id).await?;
            }
            route_and_insert(&mut tx, &roots, edge, router.as_ref(), admit.as_ref(), &mut stats).await?;
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
    /// Does touching `raw` taint the session for the ratchet (GEAR2_DESIGN §2)?
    ///
    /// This is DELIBERATELY WEAKER than [`Self::visible`] and is consumed ONLY by
    /// [`admission_map`] — never by routing (§1.2) or metadata sanitization
    /// (§1.4), which stay on the strict predicate. The two answer different
    /// questions: `visible` asks "may this content be STORED in this domain's
    /// db?", `taints` asks "does having touched this mean the session's PROSE can
    /// no longer be attested clean?".
    ///
    /// Conflating them was the §7.4 defect (tracker 2026-07-18): a domain-visible
    /// check treats "belongs to no domain" (a scratchpad write, the workspace's
    /// own root tracker, Claude's per-slug memory) identically to "belongs to
    /// ANOTHER domain" — so on a clean single-domain box the ratchet fired on
    /// benign housekeeping in 89.5% of edges, with **100% of those firings false
    /// positives**, while the genuine cross-domain signal (90.8% of taints in the
    /// mirror two-domain view) is what it exists to catch.
    ///
    /// Fix = default-deny with a small BUILT-IN neutral set (`is_neutral_path`).
    /// Everything not neutral still taints, so an unlabeled foreign tree — which
    /// is how a second domain usually looks, being absent from THIS domain's
    /// config rather than labeled elsewhere — taints exactly as before.
    fn taints(&self, raw: &str) -> bool {
        !self.visible(raw) && !is_neutral_path(raw, self.workspace)
    }

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

/// The BUILT-IN domain-neutral set — paths that belong to NO domain and so
/// cannot carry another domain's IP (§7.4, tracker 2026-07-18). Consulted only
/// by [`DomainRouter::taints`]; being neutral never admits content into a db.
///
/// Three classes, deliberately narrow — this is a default-deny list, so every
/// entry is a place whose *contents* are workspace scaffolding or this session's
/// own scratch, never a client's source tree:
///
///  1. **Depth-1 regular files in the workspace root** (`CLAUDE.md`,
///     `BUG_TRIAGE.md`, `.mcp.json`). Files ONLY — an unlabeled *subdirectory*
///     at depth 1 is exactly how a second client's repo tends to sit in a shared
///     workspace, so directories keep tainting (the `proj-a-extra` sibling trap).
///  2. **This workspace's OWN Claude slug dir** (`~/.claude/projects/<encoded>/`)
///     — its transcripts and its `memory/`. NOT `~/.claude` as a whole: that
///     also holds OTHER workspaces' slugs (another domain's verbatim source and
///     prose in `tool_result` blocks), a flat cross-workspace `history.jsonl`,
///     and an unpartitioned `file-history/`. The slug dir is the segregated
///     unit; the parent is not.
///  3. **This workspace's own scratchpad under a temp root** — identified by the
///     same encoded-slug component. NOT temp dirs wholesale: a full client
///     checkout in `/tmp` is a real workflow (nibdex's own release runbook
///     clones to `/tmp/nibdex-verify`), so a blanket temp exemption would let
///     another domain's tree go untainted.
///
/// Fail-narrow throughout: a relative or unresolvable path is NOT neutral, so it
/// still taints (mirrors [`DomainRouter::visible`]'s non-absolute rejection).
fn is_neutral_path(raw: &str, workspace: &Path) -> bool {
    if !Path::new(raw).is_absolute() {
        return false;
    }
    let Some(norm) = normalize_lexical(Path::new(raw)) else {
        return false;
    };
    let p = norm.canonicalize().unwrap_or(norm);

    // ORDER IS LOAD-BEARING. Anything INSIDE the workspace is domain territory
    // and is judged ONLY by rule (1) — a workspace subdir is a candidate client
    // tree whether or not it is labeled, so it must never fall through to the
    // rules below. Without this gate a workspace that happens to live under a
    // temp path (as `tempfile::tempdir()` does, which is how the §4 invariant
    // test caught it) would be neutral in its ENTIRETY and the ratchet would
    // never fire at all.
    if p.starts_with(workspace) {
        // (1) a regular FILE sitting directly in the workspace root — not a dir.
        return p.parent() == Some(workspace) && p.is_file();
    }

    // Match a prefix in BOTH its literal and resolved forms. `p` is only
    // canonicalized when it EXISTS, and a scratch file is routinely gone by
    // index time — so a lexical `/tmp/x` must still match a root that
    // canonicalizes to `/private/tmp` (macOS symlinks `/tmp`). Checking one form
    // only silently drops deleted scratch back into the tainting set.
    let under = |root: &Path| {
        p.starts_with(root) || root.canonicalize().is_ok_and(|r| p.starts_with(r))
    };

    // (2) this workspace's OWN slug dir — never `~/.claude` wholesale.
    let slug_dir = crate::indexer::claude_slug_dir(workspace);
    if slug_dir.as_deref().is_some_and(under) {
        return true;
    }

    // (3) this workspace's own scratchpad under a temp root. Requiring the
    // encoded-slug COMPONENT (not just a temp prefix) is what keeps a foreign
    // checkout or another workspace's scratchpad in the tainting set — and it
    // also defuses an absurd `TMPDIR` (`/`, `$HOME`), since a foreign path
    // would still lack the component.
    let Some(slug) = slug_dir
        .as_deref()
        .and_then(Path::file_name)
        .map(std::ffi::OsStr::to_owned)
    else {
        return false;
    };
    let mut temps: Vec<PathBuf> = vec!["/tmp".into(), "/private/tmp".into(), "/var/folders".into()];
    if let Some(t) = std::env::var_os("TMPDIR") {
        temps.push(PathBuf::from(t));
    }
    if !temps.iter().any(|t| under(t)) {
        return false;
    }
    // Inside a temp root, the same file-vs-directory logic as rule (1): a loose
    // FILE is scratch (`/tmp/notes.md`), a DIRECTORY TREE is a checkout — and a
    // client tree cloned to `/tmp` is a real workflow, not a hypothetical
    // (nibdex's own release runbook clones to `/tmp/nibdex-verify`). So a temp
    // path is neutral only if it is a loose file directly in a temp root, or
    // carries this workspace's encoded-slug component (the agent scratchpad,
    // `$TMPDIR/claude-<uid>/<slug>/…`). Requiring one of those also defuses an
    // absurd `TMPDIR` (`/`, `$HOME`): a foreign tree under it satisfies neither.
    if p.components().any(|c| c.as_os_str() == slug) {
        return true;
    }
    temps
        .iter()
        .any(|t| p.parent() == Some(t) || t.canonicalize().is_ok_and(|r| p.parent() == Some(&*r)))
        && p.is_file()
}

/// The ratchet (GEAR2_DESIGN §2): walk a transcript's groups IN ORDER; a group
/// is clean iff no path it touched TAINTS (see [`DomainRouter::taints`] — every
/// path that is neither domain-visible nor domain-neutral); once a session hits
/// an unclean group it is tainted for the rest of the transcript (permanent,
/// never resets). Returns, per `(session_id, group_id)`, whether that group's
/// rationale is ADMITTED. Keyed by session too, so the rare two-sessions-in-one-
/// file case still ratchets independently.
fn admission_map(
    groups: &[GroupTaint],
    router: &DomainRouter,
) -> HashMap<(String, String), bool> {
    let mut admit = HashMap::new();
    let mut tainted: HashSet<&str> = HashSet::new();
    for g in groups {
        let group_clean = !g.paths.iter().any(|p| router.taints(p));
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
    roots: &[String],
    edge: &SessionEdge,
    router: Option<&DomainRouter<'_>>,
    admit: Option<&HashMap<(String, String), bool>>,
    stats: &mut SessionIndexStats,
) -> Result<()> {
    let Some(r) = router else {
        // Unpartitioned — the original path, unchanged: full rationale + FTS body.
        return insert_edge(
            conn,
            roots,
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

    insert_edge(conn, roots, edge, repo_path, git_branch, rationale, admitted, stats).await
}

/// `<projects_dir>/<slug>/*.jsonl`. With `slug=None`, every child dir's jsonl.
fn collect_transcripts(projects_dir: &Path, slug: Option<&str>) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let slug_dirs: Vec<PathBuf> = match slug {
        Some(s) => {
            // A NAMED slug that does not exist is an error, not an empty result.
            // The walk below swallows an unreadable directory (correct when
            // sweeping every slug — one bad neighbour must not fail the pass),
            // but that same swallow turns a typo'd `--slug` into "0 write-edges,
            // exit 0", which reads as "this workspace has no sessions". `--slug`
            // also takes hyphen-leading values now, so `--slug --all-slugs`
            // parses as a slug NAMED `--all-slugs` — caught here rather than
            // silently indexing nothing.
            let dir = projects_dir.join(s);
            if !dir.is_dir() {
                anyhow::bail!(
                    "--slug={s} does not name a directory under {} \
                     (list the available slugs with `ls {}`)",
                    projects_dir.display(),
                    projects_dir.display()
                );
            }
            vec![dir]
        }
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
    // Bytes + lossy decode, NOT `read_to_string`. A transcript is append-only and
    // the one being read is usually the LIVE session — frequently the very
    // session running `nibdex index` — so the final line is routinely a partial
    // write. `read_to_string` is all-or-nothing on UTF-8 validity, so a tail cut
    // mid-multi-byte-sequence fails the WHOLE file and discards every complete
    // edge in it, while the same cut on an ASCII boundary costs only the torn
    // line. Tolerating a torn tail has to mean tolerating it at the BYTE level;
    // per-line JSON parsing already discards the mangled remnant and counts it.
    let bytes = std::fs::read(path).with_context(|| format!("reading transcript {path:?}"))?;
    let content = String::from_utf8_lossy(&bytes);
    let mut edges = Vec::new();
    // Ordered per-`message.id`-group taint sets (§2). Assistant lines of one
    // logical message are contiguous (tool_result user-lines interleave but are
    // skipped), so a group is "the last one iff (session,group_id) still match."
    let mut groups: Vec<GroupTaint> = Vec::new();
    let mut activity: HashMap<(String, Option<String>), SessionActivity> = HashMap::new();
    let mut last_text = String::new();
    // `last_text` carries a prior message's text into a text-less edit as its
    // fallback rationale — but that fallback must stay WITHIN one session.
    // `last_session` scopes it: in a multi-session transcript file, session A's
    // (possibly foreign-tainted) prose must not become session B's inherited
    // rationale, which would bypass B's own clean taint and admit A's text
    // (the cross-session `last_text` bleed). Reset on any session change; an
    // over-reset only drops a fallback (fail-toward-withhold), never leaks.
    let mut last_session = String::new();
    // ... AND on any `cwd` change, for the same reason one scope wider.
    //
    // Session scoping alone was sufficient while transcript ingestion was an
    // opt-in command usually run with `--domain`, where the ratchet withholds
    // prose from any session that touched another tree. It is NOT sufficient now
    // that `nibdex index` ingests transcripts by default and UNPARTITIONED, where
    // no ratchet exists: `cwd` varies line-to-line *within one session*, so a
    // foreign-cwd line's prose would survive as the inherited fallback rationale
    // of the next in-workspace text-less edit. The edge-level scope rule drops
    // the foreign EDGE but cannot see prose that edge already deposited here —
    // demonstrated leaking an employer host/port string into a workspace index,
    // FTS-searchable, while the run's own summary reported that session dropped.
    //
    // Costs only a fallback when a session legitimately moves directory, which is
    // the same fail-toward-withhold trade the session reset already accepted.
    let mut last_cwd: Option<String> = None;

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
        // Scope the fallback to one working directory too (see its declaration).
        // Deliberately treats absent->present and present->absent as changes:
        // an unknown cwd cannot be shown to be the same context.
        if cwd.map(str::to_string) != last_cwd {
            last_text.clear();
            last_cwd = cwd.map(str::to_string);
        }
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
                    // Tally retrieval BEFORE narrowing to Edit/Write. This is the
                    // denominator: the calls nibdex was competing for, whether or
                    // not it won them. Counted for every tool_use, including the
                    // ones that never produce an edge.
                    if let Some(class) = classify_tool_call(tool, input) {
                        let a = activity
                            .entry((session_id.to_string(), cwd.map(str::to_string)))
                            .or_default();
                        // Only a real timestamp may stamp first_seen: `unwrap_or(0)`
                        // put epoch-1970 rows into `session_activity` for a
                        // timestamp-less first call (RC1 review, sev-2).
                        if a.first_seen == 0
                            && let Some(t) = edited_at
                        {
                            a.first_seen = t;
                        }
                        match class {
                            ToolClass::Retrieval => a.retrieval_calls += 1,
                            ToolClass::Nibdex => a.nibdex_calls += 1,
                        }
                    }
                    if tool != "Edit" && tool != "Write" {
                        continue;
                    }
                    let Some(abs_path) =
                        input.and_then(|i| i.get("file_path")).and_then(Value::as_str)
                    else {
                        continue;
                    };
                    let Some(at) = edited_at else {
                        stats.edges_skipped_no_timestamp += 1;
                        continue;
                    };
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
    Ok(TranscriptParse { edges, groups, activity })
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
/// marker must never be indexed). Commit-binding runs against the edit's absolute
/// target path and the repos THIS db holds commits for (see
/// `resolve_capturing_commit`); a §1.4-sanitized `repo_path` (NULL) does not
/// prevent binding, since in domain mode the db's commits are this domain's own.
#[allow(clippy::too_many_arguments)]
async fn insert_edge(
    conn: &mut sqlx::SqliteConnection,
    roots: &[String],
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

    // Absolute target: the raw tool-input path when it is absolute; else rebuilt
    // from cwd + relative (a relative `file_path` with a NULL cwd cannot bind).
    let abs_target: Option<String> = if Path::new(&edge.raw_abs_path).is_absolute() {
        Some(edge.raw_abs_path.clone())
    } else {
        edge.cwd.as_deref().map(|c| format!("{}/{}", c.trim_end_matches('/'), edge.file_path))
    };
    let commit_id = match abs_target.as_deref() {
        Some(abs) => resolve_capturing_commit(&mut *conn, roots, abs, edge.edited_at).await?,
        None => None,
    };

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
        // LATE BINDING. The dedupe key skips the row, but `commit_id` was
        // resolved at FIRST insert — and at that moment the capturing commit
        // usually does not exist yet. You work, you index, and only then do you
        // commit; `resolve_capturing_commit` looks for the oldest commit at/after
        // the edit, so it finds nothing and stores NULL. Without this pass that
        // NULL is permanent, because every later run hits the dedupe key and
        // skips the row whole.
        //
        // That matters because the capturing commit is the provenance every
        // `find_session` hit carries — the "commit that later captured it" the
        // docs lead with. Folding session indexing into `nibdex index` made the
        // index-before-commit order the NORMAL one rather than an unlucky one,
        // so the corpus would have decayed toward unbound for exactly the
        // workflow the README recommends.
        //
        // Narrow on purpose: only NULL → Some, never a re-bind of an already
        // bound edge (the oldest-at-or-after answer is stable once found), and
        // only when this pass actually has a commit to offer.
        if commit_id.is_some() {
            let res = sqlx::query(
                "UPDATE session_edges SET commit_id = ? \
                 WHERE message_uuid = ? AND edge_ordinal = ? AND commit_id IS NULL",
            )
            .bind(commit_id)
            .bind(&edge.message_uuid)
            .bind(edge.edge_ordinal)
            .execute(&mut *conn)
            .await?;
            if res.rows_affected() > 0 {
                stats.edges_late_bound += 1;
            }
        }
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

/// The repo roots this db knows commits for — the candidate set for
/// [`resolve_capturing_commit`]. Loaded once per pass (see `index_sessions`).
async fn known_repo_roots(pool: &SqlitePool) -> Result<Vec<String>> {
    Ok(sqlx::query_scalar::<_, String>("SELECT DISTINCT repo_path FROM commit_entries")
        .fetch_all(pool)
        .await?)
}

/// Which known repo root contains `abs_path`, and the path relative to it —
/// the LONGEST root that is a component-wise prefix (innermost repo wins, the
/// same choice `find_code` makes by indexing each discovered repo's own tracked
/// files). `None` when no indexed repo contains the file.
///
/// Decided during the RC1 review (1.1): the binding key is the edit's absolute
/// target path resolved against the repos the db actually holds — NOT the
/// transcript `cwd`. `cwd` is where Claude was launched, which in the README's
/// own recommended layout (`--workspace ~/ws` with repos as subdirs) is the
/// workspace container, not any repo root; binding on it could never match a
/// `commit_entries.repo_path` and `find_session` shipped unbound for exactly the
/// setup the quickstart produces. Windows separators are normalized to `/`
/// because `files_changed` stores git's forward-slash paths.
pub(crate) fn repo_root_and_rel<'a>(roots: &'a [String], abs_path: &str) -> Option<(&'a str, String)> {
    // Lexically resolve `.`/`..` first: a raw tool-input path like
    // `<repo>/docs/../CLAUDE.md` must match `files_changed` = `["CLAUDE.md"]`
    // (seen unbound in the rc.2 dogfood pass). An unresolvable `..` → no root.
    let normalized = crate::domains::normalize_lexical(Path::new(abs_path))?;
    let target = normalized.to_string_lossy().replace('\\', "/");
    let mut best: Option<(&str, String)> = None;
    for root in roots {
        let r = root.trim_end_matches('/');
        if r.is_empty() {
            continue;
        }
        if let Some(rest) = target.strip_prefix(r)
            && let Some(rel) = rest.strip_prefix('/')
            && !rel.is_empty()
            && best.as_ref().is_none_or(|(b, _)| r.len() > b.len())
        {
            best = Some((root.as_str(), rel.to_string()));
        }
    }
    best
}

/// Best-effort session→commit binding: the OLDEST commit, on the repo that
/// contains the edited file, whose `files_changed` names that file and whose
/// COMMITTER date is at/after `edited_at` — the next commit that captured the
/// edit. `None` when no known repo contains the file or no such commit is
/// indexed.
///
/// `committed_at`, not `authored_at` (RC1 review 1.1): the author date is
/// caller-supplied and preserved across rebase / cherry-pick / `--amend --date`,
/// so a rewritten commit that captured the edit could carry an author date
/// BEFORE the edit and be excluded — and two rewritten commits can be ordered
/// against each other wrongly. The committer date is stamped when the object is
/// written, which is when the edit was captured.
///
/// `git_branch` is stored on the edge but is deliberately NOT a predicate: the
/// commit side has no branch column populated (`branch_refs` is NULL), and the
/// walk is HEAD-only, so an unmerged side-branch commit is never a candidate.
async fn resolve_capturing_commit(
    conn: &mut sqlx::SqliteConnection,
    roots: &[String],
    abs_path: &str,
    edited_at: i64,
) -> Result<Option<i64>> {
    let Some((repo, rel)) = repo_root_and_rel(roots, abs_path) else {
        return Ok(None);
    };
    // Match the path as a whole quoted JSON element to avoid partial-name hits.
    // Escape LIKE wildcards in the path, and pair it with an ESCAPE clause below.
    // Unescaped, `_` matches ANY single character — including `/` — so an edit to
    // `src/mcp_query.rs` binds to a commit that only touched `src/mcp/query.rs`,
    // attributing the wrong provenance. `%` in a path matches anything at all.
    // Both shapes are ordinary in real trees.
    let escaped = rel
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    let needle = format!("%\"{escaped}\"%");
    let id: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM commit_entries \
         WHERE repo_path = ? AND committed_at >= ? AND files_changed LIKE ? ESCAPE '\\' \
         ORDER BY committed_at ASC, id ASC LIMIT 1",
    )
    .bind(repo)
    .bind(edited_at)
    .bind(needle)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(id)
}

/// Drop ONE session's edges + their FTS rows (the `--rebuild` unit).
async fn wipe_session_edges_for(conn: &mut sqlx::SqliteConnection, session_id: &str) -> Result<()> {
    sqlx::query(
        "DELETE FROM search_index WHERE source_table = 'session_edges' \
           AND rowid_ref IN (SELECT id FROM session_edges WHERE session_id = ?)",
    )
    .bind(session_id)
    .execute(&mut *conn)
    .await?;
    sqlx::query("DELETE FROM session_edges WHERE session_id = ?")
        .bind(session_id)
        .execute(&mut *conn)
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
            "proj-a/neu.rs",
            "proj-a/sib.rs",
            "proj-a/fs.rs",
            "proj-a/tc.rs",
            "proj-b/y.rs",
            "proj-b/beta.rs",
            "proj-shared/s.rs",
            "proj-a-extra/e.rs",
        ] {
            std::fs::write(ws.join(rel), "x").unwrap();
        }
        std::fs::write(ws.join("BUG_TRIAGE.md"), "workspace-level tracker").unwrap();
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
        // Another workspace's slug under ~/.claude — NOT this workspace's own.
        let foreign_slug = format!(
            "{}/.claude/projects/-Users-someone-other-workspace/deadbeef.jsonl",
            std::env::var("HOME").unwrap_or_else(|_| "/tmp/nohome".into())
        );

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
            // (7) §7.4 NEUTRAL TOUCH — reading a depth-1 workspace-root file is
            //     domain-NEUTRAL (belongs to no domain), so it must NOT taint:
            //     this rationale is ADMITTED VERBATIM. Before the taints() split
            //     this session withheld, which is the 89.5%/all-false-positive
            //     behavior on a clean single-domain box (tracker 2026-07-18).
            line("neutral", "n1", "g9", root, "main", read(&abs("BUG_TRIAGE.md"))),
            line("neutral", "n2", "g9", root, "main", txt("neutral_note while editing alpha_widget")),
            line("neutral", "n3", "g9", root, "main", edit(&abs("proj-a/neu.rs"))),
            // (8) §7.4 PROTECTION PRESERVED — an UNLABELED workspace SUBDIR is how
            //     a second client's tree usually sits (absent from this config, not
            //     labeled elsewhere), so it must STILL taint. Prose names the
            //     sibling; if taints() ever neutralized unlabeled subdirs this
            //     rationale would be admitted and the `proj-a-extra` needle sweep
            //     below would fail. This is the mutation proof for the new rule.
            line("sibling", "b1", "g10", root, "main", read(&abs("proj-a-extra/e.rs"))),
            line("sibling", "b2", "g11", root, "main", txt("recalling proj-a-extra contents")),
            line("sibling", "b3", "g11", root, "main", edit(&abs("proj-a/sib.rs"))),
            // (9) ADVERSARIAL-REVIEW P1 — reading ANOTHER workspace's Claude slug
            //     must taint. `~/.claude` as a whole holds other workspaces'
            //     transcripts (another domain's verbatim source + prose in
            //     tool_result blocks); only THIS workspace's own slug is neutral.
            //     The path need not exist — rules 2/3 are path-shaped, and a
            //     stale transcript naming a since-deleted file is the normal case.
            line("foreignslug", "fs1", "g12", root, "main", read(&foreign_slug)),
            line("foreignslug", "fs2", "g13", root, "main", txt("recalling beta_gadget from the other workspace")),
            line("foreignslug", "fs3", "g13", root, "main", edit(&abs("proj-a/fs.rs"))),
            // (10) CONFIRM-PASS P3 — a CHECKOUT in a temp dir must taint. Temp is
            //      neutral only for a loose FILE or this workspace's own scratch;
            //      cloning a tree to /tmp is a normal workflow (this repo's own
            //      release runbook does it), so a temp SUBDIRECTORY is foreign.
            //      Without this the release gate passed a widened temp rule.
            line("tmpcheckout", "tc1", "g14", root, "main", read("/tmp/fake-checkout/beta_src.rs")),
            line("tmpcheckout", "tc2", "g15", root, "main", txt("porting beta_gadget from the temp checkout")),
            line("tmpcheckout", "tc3", "g15", root, "main", edit(&abs("proj-a/tc.rs"))),
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
            index_sessions(&alpha, projects.path(), SessionScope::Slug(slug), true, &ws, Some("alpha"))
                .await
                .unwrap();
        index_sessions(&beta, projects.path(), SessionScope::Slug(slug), true, &ws, Some("beta"))
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
        // alpha keeps: control + g2-a + g3 + g5 + g6 + g7-shared + g7-a
        //              + g9 neutral (admitted) + g11 sibling (withheld)
        //              + g13 foreign-slug + g15 temp-checkout (both withheld) = 11
        assert_eq!(edge_count(&alpha).await, 11);
        // beta keeps only proj-b/y + proj-shared/s = 2
        assert_eq!(edge_count(&beta).await, 2);
        // counters: alpha dropped proj-b/y + proj-a-extra/e = 2; withheld g2-a,g3,g5 = 3
        assert_eq!(a_stats.edges_dropped_foreign_domain, 2);
        // withheld g2-a,g3,g5 + g11 sibling + g13 foreign-slug + g15 temp-checkout
        // = 6 (g9 neutral is ADMITTED, §7.4)
        assert_eq!(a_stats.rationales_withheld, 6);

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

        // === §7.4 taints() — BOTH directions must hold ===
        // (7) a depth-1 workspace-root read is NEUTRAL: rationale ADMITTED verbatim.
        //     Mutation proof: make `is_neutral_path` return false and this fails.
        assert_eq!(
            rationale_where(&alpha, "%neu.rs").await.as_deref(),
            Some("neutral_note while editing alpha_widget"),
            "a neutral workspace-root touch must NOT taint (§7.4)"
        );
        assert!(!find_session_edge(&alpha, "neutral_note", 10).await.unwrap().is_empty());
        // (8) an UNLABELED workspace SUBDIR still taints: rationale WITHHELD.
        //     Mutation proof: make `is_neutral_path` return true for workspace
        //     subdirs and this fails — and the `proj-a-extra` needle sweep above
        //     fails with it, since the admitted prose names the sibling.
        assert_eq!(
            rationale_where(&alpha, "%sib.rs").await.as_deref(),
            Some(WITHHELD_MARKER),
            "an unlabeled sibling subdir must STILL taint (§7.4 protection)"
        );

        // (9) reading ANOTHER workspace's slug taints — mutation proof: widening
        //     rule 2 back to all of `~/.claude` fails this AND the beta_gadget
        //     needle sweep above, since the admitted prose names the token.
        assert_eq!(
            rationale_where(&alpha, "%fs.rs").await.as_deref(),
            Some(WITHHELD_MARKER),
            "a foreign workspace's Claude slug must STILL taint (review P1)"
        );

        // (10) a temp-dir CHECKOUT taints — mutation proof: widening rule 3 back
        //      to temp-wholesale fails this and the beta_gadget sweep.
        assert_eq!(
            rationale_where(&alpha, "%tc.rs").await.as_deref(),
            Some(WITHHELD_MARKER),
            "a checkout in a temp dir must STILL taint (confirm-pass P3)"
        );

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

    /// §7.4 — the built-in neutral set is DEFAULT-DENY and workspace-scoped.
    /// The workspace gate is load-bearing: a workspace under a temp dir (which is
    /// exactly what `tempfile::tempdir()` produces, and how the §4 invariant test
    /// caught this) must NOT be neutral in its entirety.
    #[test]
    fn neutral_set_is_default_deny_and_workspace_scoped() {
        let ws_tmp = tempfile::tempdir().unwrap();
        let ws = ws_tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(ws.join("proj-a")).unwrap();
        std::fs::create_dir_all(ws.join("unlabeled-client")).unwrap();
        std::fs::write(ws.join("BUG_TRIAGE.md"), "x").unwrap();
        std::fs::write(ws.join("proj-a/src.rs"), "x").unwrap();
        std::fs::write(ws.join("unlabeled-client/secret.rs"), "x").unwrap();
        let n = |p: &std::path::Path| is_neutral_path(&p.to_string_lossy(), &ws);

        // (1) depth-1 workspace-root FILE -> neutral
        assert!(n(&ws.join("BUG_TRIAGE.md")));
        // ...but a depth-1 DIRECTORY is not (it may be a client tree)
        assert!(!n(&ws.join("proj-a")));
        assert!(!n(&ws.join("unlabeled-client")));
        // ...and nothing inside any subdir is, labeled or not. THE leak guard:
        // the workspace lives under a temp dir, so without the workspace gate
        // rule (3) would swallow these.
        assert!(!n(&ws.join("proj-a/src.rs")));
        assert!(!n(&ws.join("unlabeled-client/secret.rs")));

        // (2) this workspace's OWN slug dir -> neutral. (3) a temp path carrying
        // the same encoded-slug component -> neutral.
        let slug_dir = crate::indexer::claude_slug_dir(&ws).unwrap();
        let slug = slug_dir.file_name().unwrap().to_string_lossy().into_owned();
        assert!(is_neutral_path(
            &slug_dir.join("memory/M.md").to_string_lossy(),
            &ws
        ));
        assert!(is_neutral_path(&format!("/tmp/claude-501/{slug}/scratch.sh"), &ws));

        // THE P1 REGRESSION GUARD (adversarial review 2026-07-18): `~/.claude` as
        // a whole is NOT neutral. It holds OTHER workspaces' slugs -- another
        // domain's verbatim source and prose in transcript tool_result blocks --
        // plus a flat cross-workspace history.jsonl and an unpartitioned
        // file-history/. Reading any of those must still taint.
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap();
        let claude = home.join(".claude");
        assert!(!is_neutral_path(
            &claude.join("projects/-Users-me-other-workspace/x.jsonl").to_string_lossy(),
            &ws
        ));
        assert!(!is_neutral_path(&claude.join("history.jsonl").to_string_lossy(), &ws));
        assert!(!is_neutral_path(&claude.join("file-history/f").to_string_lossy(), &ws));
        // and a temp path WITHOUT this workspace's slug is not neutral either --
        // a full client checkout in /tmp is a real workflow, not session scratch.
        assert!(!is_neutral_path("/tmp/nibdex-verify/src/lib.rs", &ws));
        assert!(!is_neutral_path("/tmp/claude-501/-Users-me-other-ws/notes.md", &ws));

        // fail-narrow: relative + unresolvable are NOT neutral, so they taint
        assert!(!is_neutral_path("src/main.rs", &ws));
        assert!(!is_neutral_path("", &ws));
        // an arbitrary foreign absolute tree is NOT neutral -> still taints. This
        // is the shape a second client's repo takes when it is simply absent from
        // this domain's config rather than labeled elsewhere.
        assert!(!is_neutral_path("/Users/someone/other-client/src/lib.rs", &ws));
    }

    // ===================== LABEL-DEPTH SPECS =====================
    // docs/LABEL_DEPTH_DESIGN.md. Landed as a mechanical oracle BEFORE the
    // implementation and `#[ignore]`d while they failed, because a gate fails on
    // its own while a review only reasons -- and reasoning is what missed this
    // shape twice. The implementation landed 2026-07-23; the attributes are gone
    // and these now run in the release gate.
    //
    // THE SHAPE THEY CAUGHT: `includes` used to route through `top_subdir`,
    // which read only the FIRST path component, so a tree checked out beneath a
    // labeled directory (`learn/acme-workspace`, from this project's dogfood
    // notes) was admitted wholesale AND never tainted -- admission and taint
    // share one predicate, so the safety net was blind exactly where the hole
    // was.
    //
    // Each spec isolates ONE rule, so a failure names the missing rule rather
    // than "something about depth". Keep it that way: the two continuity rows
    // (`labeled_nested_repo_is_restored`, `bulk_...`) passed before the work AND
    // after, and are the mutation guard against a "fix" that simply excludes
    // every nested repo -- which would satisfy every quarantine assertion while
    // silently breaking the `learn/python-sandbox` case.

    struct DepthRig {
        alpha: SqlitePool,
        beta: SqlitePool,
        _ws_tmp: tempfile::TempDir,
        _projects: tempfile::TempDir,
    }

    /// The rig shared by the depth specs. Everything hangs off alpha's `proj-a`:
    ///
    /// ```text
    /// proj-a/ok.rs           plainly in-domain
    /// proj-a/nested/         the foreign tree one level down  (THE shape)
    /// proj-a/nested-notes/   a string-PREFIX sibling of it
    /// proj-a/other/          another child directory
    /// proj-b/                beta's own tree
    /// ```
    ///
    /// Each location gets a distinct token, edited in its OWN message group, and
    /// every clean group is ordered BEFORE the group that touches the nested
    /// tree: the ratchet is forward-only, so a clean edit placed after the
    /// crossing could not be told apart from a correctly withheld one.
    ///
    /// `nested_is_repo` plants a `.git` marker, which is what the quarantine
    /// rule (D6) keys on -- repo-root-ness decides "ask the human", never
    /// "route it there".
    async fn depth_rig(config: &str, nested_is_repo: bool) -> DepthRig {
        use serde_json::{json, Value};

        let ws_tmp = tempfile::tempdir().unwrap();
        let ws = ws_tmp.path().canonicalize().unwrap();
        for d in ["proj-a/nested", "proj-a/nested-notes", "proj-a/other", "proj-b"] {
            std::fs::create_dir_all(ws.join(d)).unwrap();
        }
        if nested_is_repo {
            std::fs::create_dir_all(ws.join("proj-a/nested/.git")).unwrap();
        }
        for f in [
            "proj-a/ok.rs",
            "proj-a/nested/secret.rs",
            "proj-a/nested-notes/n.rs",
            "proj-a/other/o.rs",
            "proj-b/b.rs",
        ] {
            std::fs::write(ws.join(f), "x").unwrap();
        }
        std::fs::write(ws.join(".nibdex-domains.toml"), config).unwrap();

        let wss = ws.to_string_lossy().to_string();
        let abs = |rel: &str| format!("{wss}/{rel}");
        let mut ts = 0;
        let mut line = |uuid: &str, group: &str, block: Value| -> Value {
            ts += 1;
            json!({
                "type": "assistant", "sessionId": "depth", "uuid": uuid, "cwd": wss,
                "gitBranch": "main",
                "timestamp": format!("2026-07-18T10:00:{ts:02}.000Z"),
                "message": { "id": group, "content": [ block ] }
            })
        };
        let txt = |t: &str| json!({"type": "text", "text": t});
        let edit = |p: &str| json!({"type": "tool_use", "name": "Edit", "input": {"file_path": p}});

        let lines: Vec<Value> = vec![
            // clean groups first -- see the ordering note above
            line("s1", "g1", txt("sibling_token note")),
            line("s2", "g1", edit(&abs("proj-a/nested-notes/n.rs"))),
            line("o1", "g2", txt("other_token note")),
            line("o2", "g2", edit(&abs("proj-a/other/o.rs"))),
            // the crossing: an edit INSIDE the nested tree
            line("n1", "g3", txt("porting vendor_secret_token from the nested tree")),
            line("n2", "g3", edit(&abs("proj-a/nested/secret.rs"))),
            // ...and the laundering shape the ratchet exists to stop
            line("k1", "g4", txt("recalling vendor_secret_token while back here")),
            line("k2", "g4", edit(&abs("proj-a/ok.rs"))),
        ];

        let projects = tempfile::tempdir().unwrap();
        let slug_dir = projects.path().join("-ws");
        std::fs::create_dir_all(&slug_dir).unwrap();
        std::fs::write(
            slug_dir.join("session.jsonl"),
            lines.iter().map(Value::to_string).collect::<Vec<_>>().join("\n"),
        )
        .unwrap();

        let alpha = fresh_pool().await;
        let beta = fresh_pool().await;
        index_sessions(&alpha, projects.path(), SessionScope::Slug("-ws"), true, &ws, Some("alpha"))
            .await
            .unwrap();
        index_sessions(&beta, projects.path(), SessionScope::Slug("-ws"), true, &ws, Some("beta"))
            .await
            .unwrap();
        DepthRig { alpha, beta, _ws_tmp: ws_tmp, _projects: projects }
    }

    /// D1/D2 — a deep label routes the nested tree to the domain that CLAIMS it,
    /// out of the enclosing domain, and touching it taints the enclosing one.
    /// D7 — `proj-a` now has a deeper entry beneath it, so it is a mixed parent:
    /// its other child directories must be listed to be admitted, which is why
    /// `nested-notes` is listed and `other` is not.
    #[tokio::test]
    async fn depth_spec_deep_label_routes_to_the_claiming_domain() {
        let r = depth_rig(
            "[domains]\nalpha = [\"proj-a\", \"proj-a/nested-notes\"]\n\
             beta = [\"proj-b\", \"proj-a/nested\"]\n",
            false,
        )
        .await;
        for needle in ["vendor_secret_token", "nested/"] {
            assert_eq!(needle_hits(&r.alpha, needle).await, 0, "alpha leaked {needle}");
        }
        assert!(needle_hits(&r.beta, "nested/").await > 0, "beta must receive what it claims");
        assert_eq!(
            rationale_where(&r.alpha, "%ok.rs").await.as_deref(),
            Some(WITHHELD_MARKER),
            "touching a nested tree owned elsewhere must taint"
        );
        // the string-prefix sibling is NOT swept along with `proj-a/nested`
        assert!(needle_hits(&r.alpha, "sibling_token").await > 0, "sibling must stay in alpha");
        assert_eq!(needle_hits(&r.beta, "sibling_token").await, 0, "sibling is not beta's");
    }

    /// D7 via a deep LABEL rather than a withdrawal. The mixed-parent rule keys
    /// on "any deeper entry", so a label belonging to another domain must
    /// disable the parent's sweep exactly as a carve-out does. Split out of the
    /// routing spec above so each spec fails for one reason: depth-aware
    /// attribution alone satisfies that one and NOT this one.
    #[tokio::test]
    async fn depth_spec_a_deep_label_also_makes_the_parent_mixed() {
        let r = depth_rig(
            "[domains]\nalpha = [\"proj-a\", \"proj-a/nested-notes\"]\n\
             beta = [\"proj-b\", \"proj-a/nested\"]\n",
            false,
        )
        .await;
        assert_eq!(
            needle_hits(&r.alpha, "other_token").await,
            0,
            "an unlisted child of a mixed parent must reach no domain"
        );
        assert!(needle_hits(&r.alpha, "sibling_token").await > 0, "listed child still admitted");
    }

    /// D3/D4 — withdrawal via `[unassigned]`, the case where the owning domain
    /// does not exist in this config at all. It must reach NO db, and it must
    /// still taint: a withdrawal that conferred neutrality would be a one-line
    /// ratchet off-switch.
    #[tokio::test]
    async fn depth_spec_withdrawn_nested_tree_reaches_no_domain_and_still_taints() {
        let r = depth_rig(
            "[domains]\nalpha = [\"proj-a\", \"proj-a/nested-notes\"]\nbeta = [\"proj-b\"]\n\n\
             [unassigned]\nacknowledged = [\"proj-a/nested\"]\n",
            false,
        )
        .await;
        for pool in [&r.alpha, &r.beta] {
            assert_eq!(needle_hits(pool, "vendor_secret_token").await, 0, "withdrawn content leaked");
        }
        assert_eq!(
            rationale_where(&r.alpha, "%ok.rs").await.as_deref(),
            Some(WITHHELD_MARKER),
            "a withdrawn path must STILL taint (D4)"
        );
        assert!(needle_hits(&r.alpha, "sibling_token").await > 0, "sibling must survive the carve");
    }

    /// D6 — an unlabeled git repo root under a labeled parent is quarantined:
    /// indexed by nobody, and it taints. Nothing in the config mentions it, so
    /// this is the safety DEFAULT, not an expressed intent.
    #[tokio::test]
    async fn depth_spec_unlabeled_nested_repo_is_quarantined() {
        let r = depth_rig("[domains]\nalpha = [\"proj-a\"]\nbeta = [\"proj-b\"]\n", true).await;
        for pool in [&r.alpha, &r.beta] {
            assert_eq!(needle_hits(pool, "vendor_secret_token").await, 0, "quarantine leaked");
        }
        assert_eq!(
            rationale_where(&r.alpha, "%ok.rs").await.as_deref(),
            Some(WITHHELD_MARKER),
            "an unlabeled nested repo must taint"
        );
        // proj-a has NO deeper entry, so it is not a mixed parent: its other
        // children are swept normally. Quarantine keys on repo-ness, not depth.
        assert!(needle_hits(&r.alpha, "sibling_token").await > 0);
        assert!(needle_hits(&r.alpha, "other_token").await > 0, "plain children still sweep");
    }

    /// D6 continuity — the `learn/python-sandbox` case. Once the nested repo is
    /// labeled for its parent's own domain, it is fully indexed again and stops
    /// tainting. This is the other half of the mutation proof: without it, a rule
    /// that simply excluded every nested repo would also pass the test above.
    #[tokio::test]
    async fn depth_spec_labeled_nested_repo_is_restored() {
        let r = depth_rig(
            "[domains]\nalpha = [\"proj-a\", \"proj-a/nested\", \"proj-a/nested-notes\", \
             \"proj-a/other\"]\nbeta = [\"proj-b\"]\n",
            true,
        )
        .await;
        assert!(
            needle_hits(&r.alpha, "vendor_secret_token").await > 0,
            "a nested repo labeled for its own domain must be indexed"
        );
        assert_eq!(
            rationale_where(&r.alpha, "%ok.rs").await.as_deref(),
            Some("recalling vendor_secret_token while back here"),
            "an in-domain nested repo must NOT taint"
        );
        assert_eq!(needle_hits(&r.beta, "vendor_secret_token").await, 0, "still not beta's");
    }

    /// D7 in isolation — a mixed parent stops auto-sweeping its other child
    /// directories. This is what retires the dangling-entry problem: with no
    /// implicit sweep past a carve-out, nothing MISSING can ever cause admission,
    /// so routing depends on the config alone and not on disk state.
    ///
    /// ⚠️ ORDERING — this spec is the ONE config under which the rig's "every
    /// clean group is ordered BEFORE the crossing" premise does not hold, and
    /// the first cut of the spec quietly depended on it. Here `nested-notes` is
    /// an UNLISTED child of the mixed parent, so the rig's very first group is
    /// already a crossing. Admission must therefore be asserted on the EDGE, not
    /// on a rationale token: a withheld rationale would otherwise read as "the
    /// listed child was denied" when it was admitted exactly as D7 requires.
    ///
    /// So the withholding is asserted too, rather than sidestepped. It is D5
    /// (one predicate for admission and taint) and D4 (withdrawal never confers
    /// neutrality) doing their job: an unlisted child of a mixed parent is not
    /// visible, therefore it taints, and prose after it can no longer be
    /// attested clean.
    #[tokio::test]
    async fn depth_spec_mixed_parent_does_not_sweep_unlisted_children() {
        let r = depth_rig(
            "[domains]\nalpha = [\"proj-a\", \"proj-a/other\"]\nbeta = [\"proj-b\"]\n\n\
             [unassigned]\nacknowledged = [\"proj-a/nested\"]\n",
            false,
        )
        .await;
        // listed child -> admitted (the D7 claim)
        assert!(
            needle_hits(&r.alpha, "other/o.rs").await > 0,
            "listed child of a mixed parent must be admitted"
        );
        // ...and touching the unlisted sibling first withheld its prose (D4/D5)
        assert_eq!(
            rationale_where(&r.alpha, "%other/o.rs").await.as_deref(),
            Some(WITHHELD_MARKER),
            "an unlisted child of a mixed parent must taint, not merely go unindexed"
        );
        // unlisted child of a mixed parent -> nobody
        assert_eq!(
            needle_hits(&r.alpha, "sibling_token").await,
            0,
            "unlisted child of a mixed parent must reach no domain"
        );
        assert_eq!(
            needle_hits(&r.alpha, "nested-notes").await,
            0,
            "...and neither does its path"
        );
        assert_eq!(needle_hits(&r.alpha, "vendor_secret_token").await, 0, "carve leaked");
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
        index_sessions(&alpha, projects.path(), SessionScope::Slug(slug), true, &ws, Some("alpha"))
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

    // ---- SessionScope::Workspace — the derived scope `nibdex index` uses -------

    /// THE sibling trap, at the scope boundary. `~/workspace-old` is a string
    /// prefix of nothing that matters, but `str::starts_with` would admit it under
    /// `~/workspace` — the same defect class as `split_repo_relative`'s and the
    /// `proj-a-extra` decoy's. Component-wise matching is what makes it a non-issue.
    #[test]
    fn workspace_scope_rejects_sibling_string_prefix() {
        let ws = Path::new("/home/me/workspace");
        assert!(cwd_in_workspace(Some("/home/me/workspace"), ws));
        assert!(cwd_in_workspace(Some("/home/me/workspace/nibdex"), ws));
        assert!(
            !cwd_in_workspace(Some("/home/me/workspace-old"), ws),
            "a sibling sharing a string prefix must not be admitted"
        );
        assert!(!cwd_in_workspace(Some("/home/me/work/acme-workspace"), ws));
        assert!(!cwd_in_workspace(Some("/home/me"), ws));
    }

    /// Fail-narrow in every direction a `cwd` can be unusable: absent (a line that
    /// carries none), relative (would canonicalize against the INDEXER's cwd, not
    /// the session's — `DomainRouter::visible`'s reasoning), and `..`-escaping.
    #[test]
    fn workspace_scope_is_fail_narrow_on_unusable_cwd() {
        let ws = Path::new("/home/me/workspace");
        assert!(!cwd_in_workspace(None, ws), "no cwd → not ours");
        assert!(!cwd_in_workspace(Some("workspace"), ws), "relative → not ours");
        assert!(!cwd_in_workspace(Some(""), ws));
        assert!(
            !cwd_in_workspace(Some("/home/me/workspace/../elsewhere"), ws),
            "`..` resolving outside the root must not be admitted"
        );
        // A `..` that stays inside still resolves in.
        assert!(cwd_in_workspace(
            Some("/home/me/workspace/nibdex/../clearview"),
            ws
        ));
    }

    /// Per-EDGE, not per-transcript. `cwd` varies line-to-line inside one file (a
    /// real transcript here spans `workspace`, `workspace/nibdex`,
    /// `workspace/team-docs` and `workspace/clearview-business`), so a
    /// whole-file verdict is wrong in both directions: "any line matches" would
    /// admit a foreign session's edits, "every line matches" would discard a real
    /// one's.
    #[tokio::test]
    async fn workspace_scope_filters_per_edge_within_one_transcript() {
        use serde_json::{json, Value};

        let ws_tmp = tempfile::tempdir().unwrap();
        let ws = ws_tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(ws.join("nibdex")).unwrap();
        let foreign_tmp = tempfile::tempdir().unwrap();
        let foreign = foreign_tmp.path().canonicalize().unwrap();

        let mut ts = 0;
        let mut line = |cwd: &Path, uuid: &str, block: Value| -> Value {
            ts += 1;
            json!({
                "type": "assistant",
                "sessionId": "s1",
                "uuid": uuid,
                "cwd": cwd.to_string_lossy(),
                "gitBranch": "main",
                "timestamp": format!("2026-08-14T09:00:{ts:02}.000Z"),
                "message": { "id": format!("g{ts}"), "content": [ block ] }
            })
        };
        let edit = |p: &Path, note: &str| {
            json!({"type": "tool_use", "name": "Edit",
                   "input": {"file_path": p.to_string_lossy(), "old_string": note}})
        };

        // One transcript, three cwds: the workspace root, a subdir of it, and a
        // wholly separate workspace.
        let lines: Vec<Value> = vec![
            line(&ws, "u1", edit(&ws.join("root.rs"), "keepme_root")),
            line(
                &ws.join("nibdex"),
                "u2",
                edit(&ws.join("nibdex/deep.rs"), "keepme_deep"),
            ),
            line(
                &foreign,
                "u3",
                edit(&foreign.join("other.rs"), "foreign_gadget"),
            ),
        ];

        let projects = tempfile::tempdir().unwrap();
        let slug_dir = projects.path().join("-mixed");
        std::fs::create_dir_all(&slug_dir).unwrap();
        std::fs::write(
            slug_dir.join("session.jsonl"),
            lines.iter().map(Value::to_string).collect::<Vec<_>>().join("\n"),
        )
        .unwrap();

        let pool = fresh_pool().await;
        let stats = index_sessions(
            &pool,
            projects.path(),
            SessionScope::Workspace,
            true,
            &ws,
            None,
        )
        .await
        .unwrap();

        assert_eq!(stats.edges_indexed, 2, "both in-workspace edges kept");
        assert_eq!(stats.edges_dropped_foreign_workspace, 1);
        assert_eq!(edge_count(&pool).await, 2);
        // The foreign edge left no trace anywhere in the db — not the path, not
        // the rationale, not the FTS body.
        assert_eq!(
            needle_hits(&pool, "foreign_gadget").await,
            0,
            "a foreign-cwd edge leaked into the workspace-scoped db"
        );
    }

    /// The employer-slug case: a slug sitting in the same parent whose sessions
    /// never worked inside this workspace contributes nothing — and does not
    /// inflate `sessions_seen` either.
    #[tokio::test]
    async fn workspace_scope_drops_a_foreign_workspace_slug_entirely() {
        use serde_json::{json, Value};

        let ws_tmp = tempfile::tempdir().unwrap();
        let ws = ws_tmp.path().canonicalize().unwrap();
        let foreign_tmp = tempfile::tempdir().unwrap();
        let foreign = foreign_tmp.path().canonicalize().unwrap();

        let make = |cwd: &Path, session: &str, needle: &str| -> String {
            let v: Value = json!({
                "type": "assistant",
                "sessionId": session,
                "uuid": format!("{session}-u1"),
                "cwd": cwd.to_string_lossy(),
                "timestamp": "2026-08-14T09:00:01.000Z",
                "message": { "id": format!("{session}-g1"), "content": [
                    {"type": "tool_use", "name": "Write",
                     "input": {"file_path": cwd.join("f.rs").to_string_lossy(),
                               "content": needle}}
                ]}
            });
            v.to_string()
        };

        let projects = tempfile::tempdir().unwrap();
        for (slug, cwd, session, needle) in [
            ("-ws", ws.as_path(), "mine", "keepme_local"),
            ("-employer", foreign.as_path(), "theirs", "employer_secret"),
        ] {
            let d = projects.path().join(slug);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("s.jsonl"), make(cwd, session, needle)).unwrap();
        }

        let pool = fresh_pool().await;
        let stats = index_sessions(
            &pool,
            projects.path(),
            SessionScope::Workspace,
            true,
            &ws,
            None,
        )
        .await
        .unwrap();

        assert_eq!(stats.transcripts_seen, 2, "both slugs are READ ...");
        assert_eq!(stats.edges_indexed, 1, "... but only ours is indexed");
        assert_eq!(stats.edges_dropped_foreign_workspace, 1);
        assert_eq!(
            stats.sessions_seen, 1,
            "a wholly-foreign transcript must not inflate sessions_seen"
        );
        assert_eq!(needle_hits(&pool, "employer_secret").await, 0);

        // And the control: `--all-slugs` on the same fixture DOES take both, so the
        // assertion above is measuring the scope rule and not an empty fixture.
        let wide = fresh_pool().await;
        let wide_stats =
            index_sessions(&wide, projects.path(), SessionScope::AllSlugs, true, &ws, None)
                .await
                .unwrap();
        assert_eq!(wide_stats.edges_indexed, 2);
        assert_eq!(wide_stats.edges_dropped_foreign_workspace, 0);
    }

    /// An edge indexed BEFORE its commit exists must acquire that commit on a
    /// later pass. The dedupe key skips the row, so `commit_id` was frozen at
    /// whatever was resolvable at first insert — and the natural order is work,
    /// index, then commit, which resolves to nothing. Folding session indexing
    /// into `nibdex index` made that the normal order, so without late binding
    /// the provenance every `find_session` hit carries decays toward unbound.
    #[tokio::test]
    async fn workspace_scope_late_binds_an_edge_indexed_before_its_commit() {
        use serde_json::{json, Value};

        let ws_tmp = tempfile::tempdir().unwrap();
        let ws = ws_tmp.path().canonicalize().unwrap();


        let line: Value = json!({
            "type": "assistant",
            "sessionId": "s1",
            "uuid": "u1",
            "cwd": ws.to_string_lossy(),
            "timestamp": "2027-01-15T12:00:00.000Z",
            "message": { "id": "g1", "content": [
                {"type": "text", "text": "add the helper"},
                {"type": "tool_use", "name": "Edit",
                 "input": {"file_path": ws.join("lib.rs").to_string_lossy()}}
            ]}
        });
        let projects = tempfile::tempdir().unwrap();
        let slug_dir = projects.path().join("-ws");
        std::fs::create_dir_all(&slug_dir).unwrap();
        std::fs::write(slug_dir.join("s.jsonl"), line.to_string()).unwrap();

        let pool = fresh_pool().await;
        let bound = |p: &SqlitePool| {
            let p = p.clone();
            async move {
                sqlx::query_scalar::<_, i64>(
                    "SELECT COUNT(*) FROM session_edges WHERE commit_id IS NOT NULL",
                )
                .fetch_one(&p)
                .await
                .unwrap()
            }
        };

        // Pass 1: no commit captures the file yet.
        let s1 = index_sessions(&pool, projects.path(), SessionScope::Workspace, true, &ws, None)
            .await
            .unwrap();
        assert_eq!(s1.edges_indexed, 1);
        assert_eq!(bound(&pool).await, 0, "nothing to bind to yet");

        // The commit lands AFTER the edit, capturing that file. Read the stored
        // repo_path/file_path/edited_at back rather than assuming their shape —
        // `file_path` is repo-RELATIVE, and `resolve_capturing_commit` matches it
        // as a quoted element of `files_changed`.
        let (repo_path, file_path, edited_at): (String, String, i64) =
            sqlx::query_as("SELECT repo_path, file_path, edited_at FROM session_edges")
                .fetch_one(&pool)
                .await
                .unwrap();
        sqlx::query(
            "INSERT INTO commit_entries \
               (repo_path, commit_hash, author_email, author_name, authored_at, committed_at, \
                message_summary, files_changed) \
             VALUES (?, 'cafe1234', 'a@b', 'a', ?, ?, 'add the helper', ?)",
        )
        .bind(&repo_path)
        .bind(edited_at + 60)
        .bind(edited_at + 60)
        .bind(format!("[\"{file_path}\"]"))
        .execute(&pool)
        .await
        .unwrap();

        // Pass 2: the edge is a dedupe hit, and must STILL pick up the commit.
        let s2 = index_sessions(&pool, projects.path(), SessionScope::Workspace, false, &ws, None)
            .await
            .unwrap();
        assert_eq!(s2.edges_indexed, 0, "no new rows");
        assert_eq!(s2.edges_duplicate, 1);
        assert_eq!(s2.edges_late_bound, 1, "the skipped row must be re-bound");
        assert_eq!(bound(&pool).await, 1, "provenance recovered, not lost forever");
    }

    /// The tally reaches the database with nibdex's calls on the RIGHT side.
    ///
    /// The classifier test above covers the decision; this covers the wiring.
    /// Swapping the two counters at the call site left both the classifier test
    /// and the aggregation test green, while inverting the very number the field
    /// exists to report.
    #[tokio::test]
    async fn session_activity_records_nibdex_and_rival_calls_separately() {
        use serde_json::{json, Value};

        let ws_tmp = tempfile::tempdir().unwrap();
        let ws = ws_tmp.path().canonicalize().unwrap();

        let mut ts = 0;
        let mut call = |uuid: &str, name: &str, input: Value| -> Value {
            ts += 1;
            json!({
                "type": "assistant", "sessionId": "s1", "uuid": uuid,
                "cwd": ws.to_string_lossy(),
                "timestamp": format!("2026-08-14T15:00:{ts:02}.000Z"),
                "message": { "id": format!("g{ts}"), "content": [
                    {"type": "tool_use", "name": name, "input": input}
                ]}
            })
        };
        // Two rival searches, one nibdex query.
        let lines = [
            call("u1", "Bash", json!({"command": "grep -rn needle src/"})),
            call("u2", "Grep", json!({"pattern": "needle"})),
            call("u3", "mcp__nibdex__find_code", json!({"query": "needle"})),
            // ...and a non-search shell call, which must not inflate either side.
            call("u4", "Bash", json!({"command": "cargo test"})),
        ];

        let projects = tempfile::tempdir().unwrap();
        let slug_dir = projects.path().join("-ws");
        std::fs::create_dir_all(&slug_dir).unwrap();
        std::fs::write(
            slug_dir.join("s.jsonl"),
            lines.iter().map(Value::to_string).collect::<Vec<_>>().join("\n"),
        )
        .unwrap();

        let pool = fresh_pool().await;
        index_sessions(&pool, projects.path(), SessionScope::Workspace, true, &ws, None)
            .await
            .unwrap();

        let (retrieval, nibdex): (i64, i64) =
            sqlx::query_as("SELECT retrieval_calls, nibdex_calls FROM session_activity")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(retrieval, 2, "the shell grep and the Grep tool, not `cargo test`");
        assert_eq!(nibdex, 1, "nibdex's own call must land on nibdex's side");
    }

    /// `--rebuild` is scoped to the sessions the pass admits. Slug A's rebuild
    /// must not erase slug B's edges from a shared db — the old `DELETE FROM
    /// session_edges` did exactly that (RC1 review, sev-2). Mutation this
    /// catches: a table-wide wipe leaves only s-a after the second run.
    #[tokio::test]
    async fn rebuild_reindexes_only_the_admitted_sessions() {
        use serde_json::{json, Value};

        let ws_tmp = tempfile::tempdir().unwrap();
        let ws = ws_tmp.path().canonicalize().unwrap();
        let line = |sid: &str, uuid: &str, file: &str| -> Value {
            json!({
                "type": "assistant", "sessionId": sid, "uuid": uuid,
                "cwd": ws.to_string_lossy(),
                "timestamp": "2026-08-14T15:00:01.000Z",
                "message": { "id": format!("m-{uuid}"), "content": [
                    {"type": "text", "text": format!("editing {file}")},
                    {"type": "tool_use", "name": "Edit",
                     "input": {"file_path": ws.join(file).to_string_lossy()}}
                ]}
            })
        };
        let projects = tempfile::tempdir().unwrap();
        for (slug, sid, uuid, file) in [
            ("-slug-a", "s-a", "ua", "a.rs"),
            ("-slug-b", "s-b", "ub", "b.rs"),
        ] {
            let dir = projects.path().join(slug);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("t.jsonl"), line(sid, uuid, file).to_string()).unwrap();
        }

        let pool = fresh_pool().await;
        index_sessions(&pool, projects.path(), SessionScope::Slug("-slug-a"), false, &ws, None)
            .await
            .unwrap();
        index_sessions(&pool, projects.path(), SessionScope::Slug("-slug-b"), false, &ws, None)
            .await
            .unwrap();
        // Rebuild ONLY slug A.
        index_sessions(&pool, projects.path(), SessionScope::Slug("-slug-a"), true, &ws, None)
            .await
            .unwrap();

        let mut sids: Vec<String> =
            sqlx::query_scalar("SELECT DISTINCT session_id FROM session_edges ORDER BY session_id")
                .fetch_all(&pool)
                .await
                .unwrap();
        sids.sort();
        assert_eq!(sids, vec!["s-a".to_string(), "s-b".to_string()], "slug B survives A's rebuild");
        let fts: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM search_index WHERE source_table = 'session_edges'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(fts, 2, "one FTS row per surviving edge — no duplicates, no loss");
    }

    /// RC1 review 1.1 — binding is by the edit's ABSOLUTE target against known
    /// repo roots, not by cwd. Claude launched at the workspace container edits a
    /// file inside a sub-repo; the commit lives on that sub-repo. Before the fix
    /// (`repo_path = cwd` equality + cwd-relative `file_path`) this could never
    /// bind — the README quickstart's own layout. Mutation this catches: binding
    /// on `edge.repo_path`/`edge.file_path` → 0 bound.
    #[tokio::test]
    async fn binds_to_the_sub_repo_when_launched_from_the_workspace_container() {
        use serde_json::{json, Value};
        let ws_tmp = tempfile::tempdir().unwrap();
        let ws = ws_tmp.path().canonicalize().unwrap();
        let repo = ws.join("proj");
        std::fs::create_dir_all(&repo).unwrap();

        let line: Value = json!({
            "type": "assistant", "sessionId": "s1", "uuid": "u1",
            "cwd": ws.to_string_lossy(),                       // the CONTAINER
            "timestamp": "2027-01-15T12:00:00.000Z",
            "message": { "id": "g1", "content": [
                {"type": "text", "text": "add the helper"},
                {"type": "tool_use", "name": "Edit",
                 "input": {"file_path": repo.join("src").join("code.rs").to_string_lossy()}}
            ]}
        });
        let projects = tempfile::tempdir().unwrap();
        let slug_dir = projects.path().join("-ws");
        std::fs::create_dir_all(&slug_dir).unwrap();
        std::fs::write(slug_dir.join("s.jsonl"), line.to_string()).unwrap();

        let pool = fresh_pool().await;
        let edited_at = parse_rfc3339("2027-01-15T12:00:00.000Z").unwrap();
        // The capturing commit: on the SUB-REPO, repo-relative path, committed after.
        sqlx::query(
            "INSERT INTO commit_entries \
               (repo_path, commit_hash, author_email, author_name, authored_at, committed_at, \
                message_summary, files_changed) \
             VALUES (?, 'cafe1234', 'a@b', 'a', ?, ?, 'add the helper', '[\"src/code.rs\"]')",
        )
        .bind(repo.to_string_lossy().into_owned())
        .bind(edited_at + 60)
        .bind(edited_at + 60)
        .execute(&pool)
        .await
        .unwrap();
        let s = index_sessions(&pool, projects.path(), SessionScope::Workspace, false, &ws, None)
            .await
            .unwrap();
        assert_eq!(s.edges_indexed, 1);
        assert_eq!(s.commit_bound, 1, "container-launched edit binds to the sub-repo's commit");
        let (repo_path, file_path, commit_id): (Option<String>, String, Option<i64>) =
            sqlx::query_as("SELECT repo_path, file_path, commit_id FROM session_edges")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(commit_id.is_some());
        // Stored metadata is unchanged by the fix (cwd + cwd-relative path).
        assert_eq!(repo_path.as_deref(), Some(ws.to_string_lossy().as_ref()));
        assert_eq!(file_path, "proj/src/code.rs");
    }

    /// RC1 review 1.1 — the clock is `committed_at`. A commit authored BEFORE the
    /// edit but committed after (rebase / cherry-pick / `--amend --date`) is the
    /// capturing commit and must bind; between two capturing commits the earlier
    /// COMMITTER date wins even when author dates say otherwise. Mutation this
    /// catches: `authored_at` in either the predicate or the ORDER BY.
    #[tokio::test]
    async fn binding_uses_committer_date_not_author_date() {
        use serde_json::{json, Value};
        let ws_tmp = tempfile::tempdir().unwrap();
        let ws = ws_tmp.path().canonicalize().unwrap();
        let line: Value = json!({
            "type": "assistant", "sessionId": "s1", "uuid": "u1",
            "cwd": ws.to_string_lossy(),
            "timestamp": "2027-06-01T12:00:00.000Z",
            "message": { "id": "g1", "content": [
                {"type": "text", "text": "edit shared"},
                {"type": "tool_use", "name": "Edit",
                 "input": {"file_path": ws.join("shared.rs").to_string_lossy()}}
            ]}
        });
        let projects = tempfile::tempdir().unwrap();
        let slug_dir = projects.path().join("-ws");
        std::fs::create_dir_all(&slug_dir).unwrap();
        std::fs::write(slug_dir.join("s.jsonl"), line.to_string()).unwrap();
        let edited_at = parse_rfc3339("2027-06-01T12:00:00.000Z").unwrap();
        let day = 86_400;
        let pool = fresh_pool().await;
        for (hash, authored, committed) in [
            // Rebased: authored a month BEFORE the edit, committed 9 days after.
            ("aaaa", edited_at - 30 * day, edited_at + 9 * day),
            // Authored right after the edit but committed later than aaaa.
            ("bbbb", edited_at + day, edited_at + 20 * day),
        ] {
            sqlx::query(
                "INSERT INTO commit_entries \
                   (repo_path, commit_hash, author_email, author_name, authored_at, committed_at, \
                    message_summary, files_changed) \
                 VALUES (?, ?, 'a@b', 'a', ?, ?, 'touch shared', '[\"shared.rs\"]')",
            )
            .bind(ws.to_string_lossy().into_owned())
            .bind(hash)
            .bind(authored)
            .bind(committed)
            .execute(&pool)
            .await
            .unwrap();
        }
        let s = index_sessions(&pool, projects.path(), SessionScope::Workspace, false, &ws, None)
            .await
            .unwrap();
        assert_eq!(s.commit_bound, 1);
        let bound_hash: String = sqlx::query_scalar(
            "SELECT ce.commit_hash FROM session_edges se JOIN commit_entries ce ON ce.id = se.commit_id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(bound_hash, "aaaa", "earliest COMMITTER date at/after the edit wins");
    }

    /// `repo_root_and_rel`: longest containing root wins (innermost repo), a
    /// sibling sharing a name prefix is not a container, and a file outside every
    /// root resolves to nothing.
    #[test]
    fn repo_root_resolution_is_component_wise_and_innermost() {
        let roots = vec![
            "/ws".to_string(),
            "/ws/proj".to_string(),
            "/ws/proj-x".to_string(),
        ];
        assert_eq!(
            repo_root_and_rel(&roots, "/ws/proj/src/a.rs"),
            Some(("/ws/proj", "src/a.rs".to_string()))
        );
        assert_eq!(
            repo_root_and_rel(&roots, "/ws/proj-x/b.rs"),
            Some(("/ws/proj-x", "b.rs".to_string()))
        );
        assert_eq!(
            repo_root_and_rel(&roots, "/ws/other/c.rs"),
            Some(("/ws", "other/c.rs".to_string()))
        );
        assert_eq!(repo_root_and_rel(&roots, "/elsewhere/d.rs"), None);
        assert_eq!(
            repo_root_and_rel(&roots, "/ws/proj/docs/../CLAUDE.md"),
            Some(("/ws/proj", "CLAUDE.md".to_string())),
            "`..` in a raw tool path is resolved before matching"
        );
        assert_eq!(repo_root_and_rel(&roots, "/ws/proj/../../etc/x"), None, "escapes every root");
        // A path equal to a root is not inside that root, but may be a file of an outer one.
        assert_eq!(repo_root_and_rel(&roots, "/ws/proj"), Some(("/ws", "proj".to_string())));
        assert_eq!(repo_root_and_rel(&roots, "/ws"), None);
        let only_sub = vec!["/ws/proj".to_string()];
        assert_eq!(repo_root_and_rel(&only_sub, "/ws/projects/e.rs"), None, "prefix, not component");
    }

    /// The `last_text` fallback rationale does not survive a `cwd` change (RC1
    /// review 1.10: the guard at the top of `parse_transcript` had no test — the
    /// documented employer host/port leak came back with it deleted while every
    /// test stayed green). One session: a FOREIGN-cwd assistant text carrying a
    /// secret, then an in-workspace text-less Edit. The edit's rationale must be
    /// empty and the secret must not be FTS-searchable. Mutation caught:
    /// removing the `last_cwd` reset (rationale = the foreign prose, 1 FTS hit).
    #[tokio::test]
    async fn fallback_rationale_does_not_cross_a_cwd_change() {
        use serde_json::{json, Value};

        let ws_tmp = tempfile::tempdir().unwrap();
        let ws = ws_tmp.path().canonicalize().unwrap();
        let foreign_tmp = tempfile::tempdir().unwrap();
        let foreign = foreign_tmp.path().canonicalize().unwrap();

        let lines = [
            json!({
                "type": "assistant", "sessionId": "s1", "uuid": "u1",
                "cwd": foreign.to_string_lossy(),
                "timestamp": "2026-08-14T09:00:01.000Z",
                "message": { "id": "g1", "content": [
                    {"type": "text", "text": "pointing at SEKRIT_HOST_9000 for the employer"}
                ]}
            }),
            json!({
                "type": "assistant", "sessionId": "s1", "uuid": "u2",
                "cwd": ws.to_string_lossy(),
                "timestamp": "2026-08-14T09:00:02.000Z",
                "message": { "id": "g2", "content": [
                    {"type": "tool_use", "name": "Edit",
                     "input": {"file_path": ws.join("a.rs").to_string_lossy()}}
                ]}
            }),
        ];
        let projects = tempfile::tempdir().unwrap();
        let slug_dir = projects.path().join("-ws");
        std::fs::create_dir_all(&slug_dir).unwrap();
        std::fs::write(
            slug_dir.join("s.jsonl"),
            lines.iter().map(Value::to_string).collect::<Vec<_>>().join("\n"),
        )
        .unwrap();

        let pool = fresh_pool().await;
        let stats = index_sessions(&pool, projects.path(), SessionScope::Workspace, false, &ws, None)
            .await
            .unwrap();
        assert_eq!(stats.edges_indexed, 1);
        let rationale: String = sqlx::query_scalar("SELECT rationale FROM session_edges")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(
            rationale.trim().is_empty(),
            "an in-workspace edit must not inherit foreign-cwd prose: {rationale:?}"
        );
        let hits: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM search_index WHERE source_table = 'session_edges' AND body MATCH 'SEKRIT_HOST_9000'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(hits, 0, "the foreign secret must not be searchable from this workspace's index");
    }

    /// The denominator is SCOPED like the edges: a session whose tool calls ran
    /// in another workspace's cwd contributes nothing to `session_activity`,
    /// and a session that moved between a foreign cwd and this workspace
    /// contributes only its in-workspace calls. Before this gate, a scratch
    /// workspace with zero in-scope edits reported every session on the machine
    /// under `check().adoption` (RC1 review 1.3). Mutation this catches: tally
    /// persisted before the scope check → 3 rows / retrieval 4.
    #[tokio::test]
    async fn session_activity_is_scoped_to_the_workspace() {
        use serde_json::{json, Value};

        let ws_tmp = tempfile::tempdir().unwrap();
        let ws = ws_tmp.path().canonicalize().unwrap();
        let foreign_tmp = tempfile::tempdir().unwrap();
        let foreign = foreign_tmp.path().canonicalize().unwrap();

        let mut ts = 0;
        let mut call = |sid: &str, cwd: &Path, uuid: &str, name: &str, input: Value| -> Value {
            ts += 1;
            json!({
                "type": "assistant", "sessionId": sid, "uuid": uuid,
                "cwd": cwd.to_string_lossy(),
                "timestamp": format!("2026-08-14T15:00:{ts:02}.000Z"),
                "message": { "id": format!("g{ts}"), "content": [
                    {"type": "tool_use", "name": name, "input": input}
                ]}
            })
        };
        let lines = [
            // s-foreign: worked entirely elsewhere — must not appear at all.
            call("s-foreign", &foreign, "u1", "Grep", json!({"pattern": "a"})),
            call("s-foreign", &foreign, "u2", "Grep", json!({"pattern": "b"})),
            // s-mixed: one call in the foreign tree, one in this workspace.
            call("s-mixed", &foreign, "u3", "Grep", json!({"pattern": "c"})),
            call("s-mixed", &ws, "u4", "Grep", json!({"pattern": "d"})),
            // s-here: entirely in this workspace.
            call("s-here", &ws, "u5", "Grep", json!({"pattern": "e"})),
        ];

        let projects = tempfile::tempdir().unwrap();
        let slug_dir = projects.path().join("-ws");
        std::fs::create_dir_all(&slug_dir).unwrap();
        std::fs::write(
            slug_dir.join("s.jsonl"),
            lines.iter().map(Value::to_string).collect::<Vec<_>>().join("\n"),
        )
        .unwrap();

        let pool = fresh_pool().await;
        index_sessions(&pool, projects.path(), SessionScope::Workspace, true, &ws, None)
            .await
            .unwrap();

        let mut rows: Vec<(String, i64)> =
            sqlx::query_as("SELECT session_id, retrieval_calls FROM session_activity")
                .fetch_all(&pool)
                .await
                .unwrap();
        rows.sort();
        assert_eq!(
            rows,
            vec![("s-here".to_string(), 1), ("s-mixed".to_string(), 1)],
            "foreign-cwd tallies must not land; mixed sessions keep only in-workspace calls"
        );
    }

    /// The denominator's classifier, tested directly.
    ///
    /// The aggregation test in `mcp::tests` inserts into `session_activity`
    /// straight, so it never reaches this function — counting a nibdex call as
    /// retrieval-elsewhere, or dropping shell searches from the denominator,
    /// both left that test green. The classifier needs its own gate, and it is
    /// the half that decides whether the number is honest.
    #[test]
    fn retrieval_classifier_counts_shell_searches_against_nibdex() {
        use serde_json::json;
        let bash = |c: &str| json!({ "command": c });

        // nibdex's own tools, however the host prefixes them.
        assert!(matches!(
            classify_tool_call("mcp__nibdex__find_code", None),
            Some(ToolClass::Nibdex)
        ));

        // The dedicated search tools.
        assert!(matches!(classify_tool_call("Grep", None), Some(ToolClass::Retrieval)));
        assert!(matches!(classify_tool_call("Glob", None), Some(ToolClass::Retrieval)));

        // THE POINT: a grep inside a shell call is retrieval. Counting only the
        // Grep tool understates the competition badly — one real session had 4
        // Grep-tool calls against 70 shell searches, so omitting these would
        // have reported a flattering share of a denominator that was 95% missing.
        for c in [
            "grep -n 'fn full_scan' src/indexer.rs",
            "rg --files-with-matches TODO",
            "find . -name '*.rs'",
            "cd /x && grep -rl needle src/ | head",
            "sudo grep -rn needle /etc",
            "ls -l; grep -n needle src/lib.rs",
        ] {
            assert!(
                matches!(classify_tool_call("Bash", Some(&bash(c))), Some(ToolClass::Retrieval)),
                "not counted as retrieval: {c}"
            );
        }

        // A shell call that is not a search is not retrieval — the denominator
        // must not be inflated by ordinary build/test/git traffic either.
        // A search DOWNSTREAM of a pipe is filtering output, not asking nibdex's
        // question. On one real session this was 120 of 173 grep-bearing calls —
        // counting them inflated the denominator 2.3x.
        for c in [
            "cargo test",
            "git commit -m x",
            "ls -l",
            "sqlite3 x.db 'SELECT 1'",
            "cargo test 2>&1 | grep -E '^test result'",
            "git log --oneline | grep -c fix",
            "curl -s localhost/healthz | grep git_sha",
        ] {
            assert!(
                classify_tool_call("Bash", Some(&bash(c))).is_none(),
                "wrongly counted as retrieval: {c}"
            );
        }

        // Tools that are neither.
        assert!(classify_tool_call("Edit", None).is_none());
        assert!(classify_tool_call("Read", None).is_none());
    }

    /// A transcript whose final line is cut MID-MULTI-BYTE-SEQUENCE must still
    /// yield every complete edge before it.
    ///
    /// Transcripts are append-only and the one being indexed is usually the LIVE
    /// session — often the very session running `nibdex index` — so a partial
    /// final write is the normal case, not an exotic one. Reading via
    /// `read_to_string` is all-or-nothing on UTF-8 validity, so a tail cut on a
    /// multi-byte boundary silently discarded the WHOLE file while the identical
    /// cut on an ASCII boundary cost only the torn line.
    #[tokio::test]
    async fn workspace_scope_survives_a_tail_torn_mid_utf8_sequence() {
        use serde_json::{json, Value};

        let ws_tmp = tempfile::tempdir().unwrap();
        let ws = ws_tmp.path().canonicalize().unwrap();

        let good: Value = json!({
            "type": "assistant",
            "sessionId": "s1",
            "uuid": "u1",
            "cwd": ws.to_string_lossy(),
            "timestamp": "2026-08-14T10:00:00.000Z",
            "message": { "id": "g1", "content": [
                {"type": "text", "text": "keepme complete edge"},
                {"type": "tool_use", "name": "Edit",
                 "input": {"file_path": ws.join("a.rs").to_string_lossy()}}
            ]}
        });

        let projects = tempfile::tempdir().unwrap();
        let slug_dir = projects.path().join("-ws");
        std::fs::create_dir_all(&slug_dir).unwrap();

        // One complete line, then a tail severed inside an em-dash's 3-byte
        // encoding — the exact shape of an interrupted append.
        let mut raw = good.to_string().into_bytes();
        raw.push(b'\n');
        let torn = "{\"type\":\"assi\u{2014}".as_bytes();
        raw.extend_from_slice(&torn[..torn.len() - 1]);
        std::fs::write(slug_dir.join("s.jsonl"), &raw).unwrap();

        let pool = fresh_pool().await;
        let stats = index_sessions(
            &pool,
            projects.path(),
            SessionScope::Workspace,
            true,
            &ws,
            None,
        )
        .await
        .unwrap();

        assert_eq!(stats.transcripts_unreadable, 0, "a torn tail is not unreadable");
        assert_eq!(
            stats.edges_indexed, 1,
            "the complete edge before the torn tail must survive"
        );
        assert!(stats.lines_parse_err >= 1, "the torn remnant is counted, not hidden");
        assert!(needle_hits(&pool, "keepme").await > 0);
    }

    /// A session launched in a workspace SUBDIRECTORY keeps its memory under a
    /// different slug than the workspace root's, and component-wise matching
    /// correctly refuses that sibling — which would drop the session-to-memory
    /// edge the neutral escape exists to preserve. `target_in_scope` therefore
    /// also admits the slug directory of the edge's OWN cwd.
    #[tokio::test]
    async fn workspace_scope_keeps_memory_of_a_subdirectory_session() {
        let ws_tmp = tempfile::tempdir().unwrap();
        let ws = ws_tmp.path().canonicalize().unwrap();
        let sub = ws.join("nibdex");
        std::fs::create_dir_all(&sub).unwrap();

        let root_slug = crate::indexer::claude_slug_dir(&ws).unwrap();
        let sub_slug = crate::indexer::claude_slug_dir(&sub).unwrap();
        assert_ne!(root_slug, sub_slug, "the rig requires two distinct slugs");

        let sub_memory = sub_slug.join("memory/MEMORY.md");
        let sub_cwd = sub.to_string_lossy().into_owned();

        // The root slug's memory was always fine (is_neutral_path rule 2) ...
        assert!(target_in_scope(
            &root_slug.join("memory/MEMORY.md").to_string_lossy(),
            &ws,
            Some(&ws.to_string_lossy())
        ));
        // ... and the SUBDIRECTORY session's memory must be kept too.
        assert!(
            target_in_scope(&sub_memory.to_string_lossy(), &ws, Some(&sub_cwd)),
            "a subdir session's own memory dir was dropped as foreign"
        );
        // But it is admitted via the SESSION's slug, not blanket-trusted: the same
        // path with no cwd to justify it stays out, and so does a foreign tree.
        assert!(!target_in_scope(&sub_memory.to_string_lossy(), &ws, None));
        assert!(!target_in_scope("/somewhere/else/secret.md", &ws, Some(&sub_cwd)));
    }

    /// A file DELETED since the edit, under a symlinked prefix, must still count
    /// as in-workspace. `canonicalize` is all-or-nothing and fails on a missing
    /// path, so a naive lexical fallback compares `/tmp/...` against a workspace
    /// resolved to `/private/tmp/...` and silently drops a real edge.
    #[test]
    fn workspace_scope_resolves_a_deleted_path_under_a_symlinked_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("realdir");
        std::fs::create_dir_all(real.join("sub")).unwrap();
        let link = tmp.path().join("linkdir");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let ws = real.canonicalize().unwrap();

        // Reached through the symlink, and the file itself never existed.
        let via_link = link.join("sub/gone.rs");
        assert!(
            path_in_workspace(&via_link.to_string_lossy(), &ws),
            "a missing file under a symlinked prefix must still resolve in-workspace"
        );
        // The existing-file case keeps working, and a true outsider stays out.
        assert!(path_in_workspace(&real.join("sub").to_string_lossy(), &ws));
        assert!(!path_in_workspace(
            &tmp.path().join("elsewhere/x.rs").to_string_lossy(),
            &ws
        ));
    }

    /// An unreadable transcript is SKIPPED, not fatal. The transcript root is
    /// machine-global, so a pass walks files this workspace has no claim on — any
    /// of which may be permission-denied, non-UTF-8, or pruned by Claude Code
    /// between the directory listing and the open. Before this was tolerated, one
    /// such file failed an entire `nibdex index` with a non-zero exit and a
    /// half-populated database, after five other corpora had already succeeded.
    ///
    /// Same principle as the torn final line, which was already handled; it bites
    /// harder at the file level because the file need not be ours at all.
    #[tokio::test]
    async fn workspace_scope_skips_unreadable_transcripts_without_failing() {
        use serde_json::{json, Value};

        let ws_tmp = tempfile::tempdir().unwrap();
        let ws = ws_tmp.path().canonicalize().unwrap();
        let projects = tempfile::tempdir().unwrap();
        let slug_dir = projects.path().join("-ws");
        std::fs::create_dir_all(&slug_dir).unwrap();

        // A perfectly good transcript ...
        let good: Value = json!({
            "type": "assistant",
            "sessionId": "s1",
            "uuid": "u1",
            "cwd": ws.to_string_lossy(),
            "timestamp": "2026-08-14T12:00:00.000Z",
            "message": { "id": "g1", "content": [
                {"type": "text", "text": "keepme good transcript"},
                {"type": "tool_use", "name": "Edit",
                 "input": {"file_path": ws.join("ok.rs").to_string_lossy()}}
            ]}
        });
        std::fs::write(slug_dir.join("good.jsonl"), good.to_string()).unwrap();

        // ... alongside one that genuinely cannot be READ: a DIRECTORY carrying
        // the `.jsonl` extension, which `collect_transcripts` picks up by
        // extension and `fs::read` then refuses. Sorted order puts it FIRST, so a
        // propagating error would kill the good one too.
        //
        // NOT non-UTF-8 bytes: those are decoded lossily on purpose (see
        // `parse_transcript`), because a live transcript's final line is often a
        // partial write and losing the whole file over it is the bug, not the
        // guard.
        std::fs::create_dir(slug_dir.join("bad.jsonl")).unwrap();

        let pool = fresh_pool().await;
        let stats = index_sessions(
            &pool,
            projects.path(),
            SessionScope::Workspace,
            true,
            &ws,
            None,
        )
        .await
        .expect("an unreadable transcript must not fail the pass");

        assert_eq!(stats.transcripts_seen, 2);
        assert_eq!(stats.transcripts_unreadable, 1, "the bad one is counted, not hidden");
        assert_eq!(stats.edges_indexed, 1, "the good one still indexed");
        assert!(needle_hits(&pool, "keepme").await > 0);
    }

    /// BOTH ends are scoped. An in-workspace session that writes into someone
    /// else's tree is dropped — the case found on the author's real corpus, where
    /// `~/workspace` sessions wrote briefings into an employer tree with host
    /// paths and ports in the rationale.
    ///
    /// The escape is the ratchet's own neutral set, not a second one: a write to
    /// this workspace's `~/.claude/…/memory/` is still kept, because discarding it
    /// would cost the session↔memory link for no isolation gain.
    #[tokio::test]
    async fn workspace_scope_drops_foreign_targets_but_keeps_neutral_ones() {
        use serde_json::{json, Value};

        let ws_tmp = tempfile::tempdir().unwrap();
        let ws = ws_tmp.path().canonicalize().unwrap();
        let employer_tmp = tempfile::tempdir().unwrap();
        let employer = employer_tmp.path().canonicalize().unwrap();
        // A real client tree, not a loose scratch file — rule (3) must not exempt it.
        std::fs::create_dir_all(employer.join("svc")).unwrap();

        // This workspace's OWN memory dir (neutral rule 2).
        let memory = crate::indexer::default_memory_dir(&ws).expect("HOME is set under test");

        let mut ts = 0;
        let mut line = |uuid: &str, target: String, note: &str| -> Value {
            ts += 1;
            json!({
                "type": "assistant",
                "sessionId": "s1",
                "uuid": uuid,
                // Every line's session is squarely inside the workspace — so the
                // cwd rule admits all three, isolating the TARGET rule under test.
                "cwd": ws.to_string_lossy(),
                "timestamp": format!("2026-08-14T11:00:{ts:02}.000Z"),
                "message": { "id": format!("g{ts}"), "content": [
                    {"type": "text", "text": note},
                    {"type": "tool_use", "name": "Edit", "input": {"file_path": target}}
                ]}
            })
        };

        let lines: Vec<Value> = vec![
            line(
                "u1",
                ws.join("src/lib.rs").to_string_lossy().into_owned(),
                "keepme in-workspace edit",
            ),
            line(
                "u2",
                memory.join("note.md").to_string_lossy().into_owned(),
                "keepme memory edit",
            ),
            line(
                "u3",
                employer.join("svc/briefing.md").to_string_lossy().into_owned(),
                "rotate the oidc token on host prod-07 port 8443",
            ),
        ];

        let projects = tempfile::tempdir().unwrap();
        let slug_dir = projects.path().join("-ws");
        std::fs::create_dir_all(&slug_dir).unwrap();
        std::fs::write(
            slug_dir.join("s.jsonl"),
            lines.iter().map(Value::to_string).collect::<Vec<_>>().join("\n"),
        )
        .unwrap();

        let pool = fresh_pool().await;
        let stats = index_sessions(
            &pool,
            projects.path(),
            SessionScope::Workspace,
            true,
            &ws,
            None,
        )
        .await
        .unwrap();

        assert_eq!(
            stats.edges_dropped_foreign_workspace, 0,
            "every line's session is in-workspace — only the target rule may fire"
        );
        assert_eq!(stats.edges_dropped_foreign_target, 1, "the employer write");
        assert_eq!(stats.edges_indexed, 2, "in-workspace + neutral memory kept");

        // The memory edge survived — the whole reason for the neutral escape.
        assert!(
            rationale_where(&pool, "%note.md%").await.is_some()
                || edge_count(&pool).await == 2,
            "the session↔memory edge must not be collateral damage"
        );
        // And the employer rationale is nowhere — not the path, not the prose.
        assert_eq!(
            needle_hits(&pool, "prod-07").await,
            0,
            "an in-workspace session's write into a foreign tree leaked"
        );
        assert_eq!(needle_hits(&pool, "briefing").await, 0);
    }
}
