// SPDX-License-Identifier: MIT

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

use crate::metrics_sink::MetricsSinkSpec;

#[derive(Parser, Debug)]
#[command(
    name = "nibdex",
    version,
    about = "MCP knowledge tool — derived index over workspace."
)]
pub struct Args {
    #[command(subcommand)]
    pub command: Command,
}

/// Output format for `find-code`.
#[derive(ValueEnum, Clone, Copy, Debug, Default)]
pub enum FindCodeFormat {
    /// Human-readable, multi-line (default).
    #[default]
    Pretty,
    /// One `path:line:excerpt` line per hit — NeoVim quickfix-friendly
    /// (`:cexpr system(...)` with `errorformat=%f:%l:%m`).
    Grep,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Answer a shell search from the index, as a Claude Code `PreToolUse` hook.
    ///
    /// Reads the hook event on stdin and, when the command is a search, prints
    /// JSON attaching nibdex's answer as `additionalContext`. The search still
    /// runs — this AUGMENTS it rather than replacing it, so the live result
    /// stays authoritative for uncommitted work.
    ///
    /// Exists because nibdex's MCP tools are deferred by the host: reaching one
    /// costs an extra call that `grep` never pays. `Bash` is resident, so
    /// riding it delivers the index's answer at `grep`'s price.
    ///
    /// Fails open: any error, missing index, or unservable query prints nothing
    /// and the tool proceeds untouched. `NIBDEX_HOOK_OFF=1` disables it.
    ///
    /// Wire it up in `<workspace>/.claude/settings.json`:
    ///
    ///   {"hooks": {"PreToolUse": [{"matcher": "Bash|Grep",
    ///     "hooks": [{"type": "command",
    ///                "command": "nibdex hook 2>/dev/null || true"}]}]}}
    ///
    /// ⚠️ Wire it with the `2>/dev/null || true` suffix, not bare. This command
    /// fails open, but only once it is REACHED: an older binary without this
    /// subcommand makes clap exit non-zero first, and a non-zero PreToolUse hook
    /// BLOCKS the tool. Observed for real — a stale binary on another machine
    /// blocked every Bash call in the session. The suffix guarantees exit 0 for
    /// a missing binary, a missing subcommand, or any crash, while still passing
    /// valid JSON through on stdout.
    Hook,

    /// Run a full scan over the workspace and populate the documents table.
    Index {
        /// Workspace root. Defaults to the current directory.
        #[arg(long)]
        workspace: Option<PathBuf>,

        /// Override the auto-detected Claude memory directory.
        #[arg(long)]
        memory_dir: Option<PathBuf>,

        /// Override the auto-detected Claude transcript root (`~/.claude/projects`).
        ///
        /// An edit joins the `find_session` corpus only when BOTH ends are in
        /// scope: the session was working inside `--workspace`, and the file it
        /// touched is inside `--workspace` too (or is this workspace's own Claude
        /// directory or scratchpad). Everything else is skipped and counted.
        /// Point this at an empty directory to index no sessions at all.
        #[arg(long)]
        projects_dir: Option<PathBuf>,

        /// SQLite database path.
        #[arg(long, default_value = "nibdex.db")]
        db: PathBuf,

        /// Max walk depth from workspace root when discovering `.git/` directories.
        #[arg(long, default_value_t = 3)]
        git_max_depth: usize,

        /// Include nested repos whose root is a descendant of another discovered repo.
        /// Default (off) matches DESIGN §14.4; turn on for monorepo workspaces.
        #[arg(long, default_value_t = false)]
        include_nested_repos: bool,

        /// IP domain to index — a key in `.nibdex-domains.toml` at the workspace
        /// root. Writes ONLY that domain's labeled subdirs into `--db`, so a
        /// domain's db never holds another domain's content. Omit to index the
        /// whole workspace. See docs/IP_DOMAINS.md.
        #[arg(long)]
        domain: Option<String>,

        /// Cap per repo to defend against pathological histories (DESIGN §9.12).
        #[arg(long, default_value_t = 50_000)]
        max_commits_per_repo: usize,
    },

    /// D1 SPIKE (docs/D1_SCOPE.md §9, gear 2): index a repo's git-tracked
    /// source files into the `source_chunks` corpus (the 5th corpus). A single
    /// `git log --name-only` pass stamps each chunk with the commit that last
    /// touched its file. Run `nibdex index` first on the same `--db` so
    /// `commit_entries` exists for provenance to resolve against.
    #[command(hide = true)]
    IndexSource {
        /// Repo to index (must be a git work tree). Defaults to current dir.
        #[arg(long)]
        repo: Option<PathBuf>,

        /// SQLite database path.
        #[arg(long, default_value = "nibdex.db")]
        db: PathBuf,
    },

    /// D1 SPIKE (docs/D1_SCOPE.md §9, gear 3): bare lexical code search over the
    /// `source_chunks` corpus. FTS5 MATCH → bounded excerpt + path + line range +
    /// bm25 rank + the provenance commit (the code↔commit join). `query` is raw
    /// FTS5 MATCH syntax. Run after `index-source` on the same `--db`.
    #[command(hide = true)]
    FindCode {
        /// FTS5 MATCH query, e.g. "provenance" or "calibration honesty".
        query: String,

        /// SQLite database path.
        #[arg(long, default_value = "nibdex.db")]
        db: PathBuf,

        /// Max hits to return.
        #[arg(long, default_value_t = 5)]
        limit: i64,

        /// Output format: `pretty` (default) or `grep`.
        #[arg(long, default_value = "pretty")]
        format: FindCodeFormat,
    },

    /// D1 SPIKE (docs/D1_SCOPE.md §10 "Reframe", gear 5): index the ADDED LINES
    /// of every commit's diff into the `diff_hunks` change-map — the content-
    /// tracing provenance probe. One `git log -p --unified=0` pass; each added
    /// block is bound to the commit that introduced it. Run `nibdex index` first
    /// on the same `--db` so the provenance FK resolves.
    #[command(hide = true)]
    IndexDiffs {
        /// Repo to index (must be a git work tree). Defaults to current dir.
        #[arg(long)]
        repo: Option<PathBuf>,

        /// SQLite database path.
        #[arg(long, default_value = "nibdex.db")]
        db: PathBuf,
    },

    /// D1 SPIKE (docs/D1_SCOPE.md §10 next-probe): content-trace a code query
    /// against the `diff_hunks` change-map. Results are ordered OLDEST-FIRST so
    /// the AUTHORING commit surfaces at the top (vs `find-code`'s file-last-touch
    /// snapshot). `query` is raw FTS5 MATCH syntax. Run after `index-diffs`.
    #[command(hide = true)]
    TraceCode {
        /// FTS5 MATCH query, e.g. "broadened" or "bm25 rank".
        query: String,

        /// SQLite database path.
        #[arg(long, default_value = "nibdex.db")]
        db: PathBuf,

        /// Max hits to return (oldest-first).
        #[arg(long, default_value_t = 8)]
        limit: i64,
    },

    /// D1 SPIKE (docs/D1_SCOPE.md §10 gear-6): the LAYER-AWARE resolver. Content-
    /// traces a code query, then resolves the **authoring** commit (oldest CODE
    /// change) separately from the **feeders** (design/session/memory prose),
    /// reporting the author↔feeder time-delta — the first concrete §7 thread
    /// signal. `query` is raw FTS5 MATCH syntax. Run after `index-diffs`.
    #[command(hide = true)]
    ResolveCode {
        /// FTS5 MATCH query, e.g. "broadened" or "bm25 rank".
        query: String,

        /// SQLite database path.
        #[arg(long, default_value = "nibdex.db")]
        db: PathBuf,

        /// Cap on matching diff-hunk rows pulled (oldest-first).
        #[arg(long, default_value_t = 500)]
        cap: i64,
    },

    /// Index Claude session transcripts (`~/.claude/projects/<slug>/*.jsonl`) into
    /// the `session_edges` map — one row per `Edit`/`Write` tool-call (the change)
    /// plus the nearest preceding assistant text (the rationale), branch/cwd/time-
    /// anchored and best-effort bound to the commit that captured it. Scope is
    /// REQUIRED (`--workspace-scoped`, `--slug`, or `--all-slugs`) so a run never
    /// silently ingests another workspace's transcripts. With `--domain`, only that
    /// domain's labeled subdirs' edits are written (per-domain session isolation).
    ///
    /// `nibdex index` already indexes sessions `--workspace-scoped`; reach for this
    /// command to re-scope a pass, or to refresh sessions without a full re-index.
    IndexSessions {
        /// Transcript root. Defaults to `~/.claude/projects`.
        #[arg(long)]
        projects_dir: Option<PathBuf>,

        /// Restrict to one workspace slug dir (e.g. `-Users-you-workspace`).
        /// Pass this, `--workspace-scoped`, or `--all-slugs` — scope is REQUIRED
        /// so a run never silently ingests transcripts from another workspace /
        /// IP domain.
        ///
        /// Every real slug begins with `-`, so both `--slug=<s>` and
        /// `--slug <s>` are accepted here (hyphen-leading values are not parsed
        /// as flags).
        #[arg(long, allow_hyphen_values = true)]
        slug: Option<String>,

        /// Scan every slug, keeping only the edits that belong to `--workspace` at
        /// BOTH ends: the session was working inside it, and the file it touched
        /// is inside it (or is this workspace's own Claude directory or
        /// scratchpad). Both drop counts are reported.
        ///
        /// This is what `nibdex index` does, and it is the right default: a
        /// workspace's sessions are spread across one slug per directory Claude
        /// was launched from, so no single `--slug` covers it.
        #[arg(long, default_value_t = false)]
        workspace_scoped: bool,

        /// Scan every slug under `projects_dir` (machine-global), with NO
        /// workspace filtering. Explicit opt-in; on a shared box this crosses
        /// into other workspaces — prefer `--workspace-scoped`.
        #[arg(long, default_value_t = false)]
        all_slugs: bool,

        /// Re-derive the edges of every session this pass admits (drop that session's
        /// prior rows, then re-insert from its transcripts). Sessions outside the
        /// scope — other slugs, other domains, transcripts that have since rotated
        /// away — are left untouched; this is not a table wipe. Default is
        /// an additive merge that never drops previously-indexed edges.
        #[arg(long, default_value_t = false)]
        rebuild: bool,

        /// Workspace root. Bounds `--workspace-scoped`, and (when `--domain` is
        /// set) is where `.nibdex-domains.toml` lives. Defaults to the current
        /// directory.
        #[arg(long)]
        workspace: Option<PathBuf>,

        /// IP domain to index sessions for — a key in `.nibdex-domains.toml`.
        /// Writes ONLY edits under that domain's labeled subdirs into `--db`, and
        /// withholds rationale prose from any session that also touched another
        /// domain's tree (see docs/IP_DOMAINS.md). Omit for unpartitioned mode
        /// (whole workspace, byte-for-byte today's behavior).
        #[arg(long)]
        domain: Option<String>,

        /// SQLite database path.
        #[arg(long, default_value = "nibdex.db")]
        db: PathBuf,
    },

    /// D1 SPIKE (docs/D1_SCOPE.md §10 gear 7): bare lexical search over the
    /// `session_edges` map. FTS5 MATCH (over rationale + path) → the file an
    /// edit touched + its rationale + branch/time + the capturing commit. The
    /// deliverable: does a CONCEPT query ("build provenance") surface the edges
    /// the curated note named only by concept? Run after `index-sessions`.
    #[command(hide = true)]
    FindSessionEdge {
        /// FTS5 MATCH query, e.g. "build provenance" or "rescore".
        query: String,

        /// SQLite database path.
        #[arg(long, default_value = "nibdex.db")]
        db: PathBuf,

        /// Max hits to return.
        #[arg(long, default_value_t = 8)]
        limit: i64,
    },

    /// D1 SPIKE (docs/D1_SCOPE.md §7, gear 9): the THREAD-MEASUREMENT gear. Draws
    /// a term population from the corpus's own vocabulary (IDF via `fts5vocab`),
    /// runs `resolve-code` on each, and reports the design→commit→code thread as a
    /// Δ-distribution SHAPE — with the §7 guards: IDF-tier discount, session-
    /// coverage normalization (transcript window), and language stratification.
    /// Read-only. Run after `index` + `index-diffs` + `index-sessions`.
    #[command(hide = true)]
    MeasureThread {
        /// SQLite database path.
        #[arg(long, default_value = "nibdex.db")]
        db: PathBuf,

        /// Size of the sampled term population (stratified across the IDF spectrum).
        #[arg(long, default_value_t = 200)]
        terms: usize,

        /// Per-term resolver cap (oldest-first rows pulled).
        #[arg(long, default_value_t = 300)]
        cap: i64,
    },

    /// D1 SPIKE (docs/D1_SCOPE.md §4 #2, gear 10): the SYMBOL-AWARE TOKENIZER A/B.
    /// Builds a symbol-expanded shadow of the diff-hunk index (camelCase/PascalCase/
    /// digit/acronym splitting), then runs the gear-9 thread measurement over the
    /// SAME term population under both the shipped unicode61 index and the shadow —
    /// reporting whether identifier-splitting recovers the rare/mid-tier concept
    /// threads gear-9 measured as frequency-confounded. Read-only (the shadow is a
    /// scratch table). Run after `index` + `index-diffs` + `index-sessions`.
    #[command(hide = true)]
    CompareTokenizers {
        /// SQLite database path.
        #[arg(long, default_value = "nibdex.db")]
        db: PathBuf,

        /// Size of the sampled term population (stratified across the IDF spectrum).
        #[arg(long, default_value_t = 200)]
        terms: usize,

        /// Per-term resolver cap (oldest-first rows pulled).
        #[arg(long, default_value_t = 300)]
        cap: i64,
    },

    /// Start an MCP server speaking JSON-RPC over stdio (DESIGN §5.1, D-6.4.1).
    Mcp {
        /// SQLite database path.
        #[arg(long, default_value = "nibdex.db")]
        db: PathBuf,

        /// §5.5 Layer 1 metrics sink: `off` (default), `stdout`, or
        /// `jsonl:<path>` (append, line-buffered, flushed per emit).
        /// Every successful tool call emits one JSONL line with the
        /// D-7.2 envelope (schema_version=1, tool, query, params,
        /// wall_ms, stages_ms, candidate_count, result_token_estimate,
        /// daemon_uptime_s, calibration_confidence="estimated").
        #[arg(long, default_value = "off")]
        metrics_sink: MetricsSinkSpec,

        /// §8.4 Layer 2 calibration model. Path to a `calibration.toml`
        /// file with per-tool counterfactual estimates. Missing file
        /// warns + degrades to Layer-1-only; bad TOML refuses startup
        /// (D-8.1). Default `./calibration.toml`.
        #[arg(long, default_value = "./calibration.toml")]
        calibration_toml: PathBuf,
    },

    /// Run the file-watcher daemon (D-6.2). Re-indexes CLAUDE.md, memory files,
    /// design docs, and git refs as they change. SIGINT/SIGTERM triggers a 1s
    /// drain window then exits. Use `serve --http` to run the watcher alongside
    /// an HTTP MCP transport in one process.
    Watch {
        /// Workspace root. Defaults to the current directory.
        #[arg(long)]
        workspace: Option<PathBuf>,

        /// Override the auto-detected Claude memory directory.
        #[arg(long)]
        memory_dir: Option<PathBuf>,

        /// SQLite database path.
        #[arg(long, default_value = "nibdex.db")]
        db: PathBuf,

        /// Max walk depth from workspace root when discovering `.git/` directories.
        #[arg(long, default_value_t = 3)]
        git_max_depth: usize,

        /// Include nested repos whose root is a descendant of another discovered repo
        /// (turn ON for a workspace-container layout — see `serve --include-nested-repos`).
        #[arg(long, default_value_t = false)]
        include_nested_repos: bool,

        /// Cap per repo for the on-commit reindex — same knob as `index
        /// --max-commits-per-repo` (DESIGN §9.12).
        #[arg(long, default_value_t = 50_000)]
        max_commits_per_repo: usize,
    },

    /// Run the HTTP MCP daemon (D-6.4.2-4) + file-watcher in one process.
    /// Cross-session MCP clients hit a warm SQLite page cache. Loopback-only
    /// at MVP per D-6.4.3.
    Serve {
        /// Loopback bind address, e.g. `127.0.0.1:7878`.
        #[arg(long)]
        http: std::net::SocketAddr,

        /// Workspace root. Defaults to the current directory.
        #[arg(long)]
        workspace: Option<PathBuf>,

        /// Override the auto-detected Claude memory directory.
        #[arg(long)]
        memory_dir: Option<PathBuf>,

        /// SQLite database path.
        #[arg(long, default_value = "nibdex.db")]
        db: PathBuf,

        /// §5.5 Layer 1 metrics sink: `off` (default), `stdout`, or
        /// `jsonl:<path>` (append, line-buffered, flushed per emit).
        /// Every successful tool call emits one JSONL line with the
        /// D-7.2 envelope (schema_version=1, tool, query, params,
        /// wall_ms, stages_ms, candidate_count, result_token_estimate,
        /// daemon_uptime_s, calibration_confidence="estimated").
        #[arg(long, default_value = "off")]
        metrics_sink: MetricsSinkSpec,

        /// §8.4 Layer 2 calibration model. Path to a `calibration.toml`
        /// file with per-tool counterfactual estimates. Missing file
        /// warns + degrades to Layer-1-only; bad TOML refuses startup
        /// (D-8.1). Default `./calibration.toml`.
        #[arg(long, default_value = "./calibration.toml")]
        calibration_toml: PathBuf,

        /// Max walk depth from workspace root when discovering `.git/` directories.
        #[arg(long, default_value_t = 3)]
        git_max_depth: usize,

        /// Include nested repos whose root is a descendant of another discovered repo.
        /// Default (off) matches DESIGN §14.4; turn ON when the workspace root is itself
        /// a git repo wrapping the real projects (a workspace-container layout), else the
        /// nested projects are skipped and the daemon indexes only the container.
        #[arg(long, default_value_t = false)]
        include_nested_repos: bool,

        /// Cap per repo for the on-commit reindex — same knob as `index
        /// --max-commits-per-repo` (DESIGN §9.12).
        #[arg(long, default_value_t = 50_000)]
        max_commits_per_repo: usize,
    },

    /// Emit an MCP-client config snippet tailored to this install. Pipe to
    /// `.mcp.json` or `pbcopy` to wire nibdex into Claude Code, Claude
    /// Desktop, Cursor, etc. without hand-editing JSON.
    PrintMcpConfig {
        /// Transport to wire the client over. `http` requires a running
        /// `nibdex serve` daemon; `stdio` spawns nibdex per-session.
        #[arg(long, value_enum, default_value_t = McpTransport::Http)]
        transport: McpTransport,

        /// Loopback bind for `--transport http`. Must match the running
        /// `serve` invocation.
        #[arg(long, default_value = "127.0.0.1:7878")]
        http: std::net::SocketAddr,

        /// SQLite database path for `--transport stdio`. Recorded literally
        /// in the snippet — pass the same path you'd give `nibdex mcp --db`.
        #[arg(long, default_value = "./nibdex.db")]
        db: PathBuf,

        /// Override the binary path written into the stdio snippet. Defaults
        /// to `std::env::current_exe()` so a snippet generated by `./target/
        /// release/nibdex` wires that exact binary.
        #[arg(long)]
        binary: Option<PathBuf>,

        /// Server name used as the JSON key under `mcpServers`.
        #[arg(long, default_value = "nibdex")]
        name: String,
    },

    /// Report what a domain's index actually covers, and config that cannot
    /// work as written.
    ///
    /// The IP-domain partition fails quietly: content that reaches no domain
    /// produces no error, it is simply absent. This surfaces those gaps —
    /// labeled directories that are not git repositories (and so are invisible
    /// to design-doc discovery), subdirectories in no domain, workspace-root
    /// files, and content under your own labels that never made it in.
    ///
    /// It reports; it never infers which domain anything belongs to. It also
    /// cannot tell you whether text in the database DISCUSSES another domain —
    /// that residual is semantic (see SECURITY.md). Exits non-zero on ERROR
    /// findings only.
    Audit {
        /// Workspace root — where `.nibdex-domains.toml` lives.
        #[arg(long, default_value = ".")]
        workspace: PathBuf,

        /// The IP domain to audit — a key in `.nibdex-domains.toml`.
        #[arg(long)]
        domain: String,

        /// The domain's database.
        #[arg(long, default_value = "nibdex.db")]
        db: PathBuf,

        /// Skip the coverage check (which needs an indexed db) and report only
        /// config problems. Useful BEFORE a first index.
        #[arg(long)]
        config_only: bool,

        /// Emit findings as JSON instead of a report — for scripts, setup
        /// automation, and CI config checks. Includes an `unassigned` list of
        /// the subdirectories a caller would feed to `nibdex label`.
        #[arg(long)]
        json: bool,

        /// After reporting, walk each subdirectory that is in no domain and ask
        /// which domain owns it (or record it as deliberately unassigned).
        /// Shows the exact config change and asks before writing anything.
        /// Needs a terminal; edits are additive and never remove a label.
        #[arg(long, conflicts_with_all = ["json", "stage_undecided"])]
        triage: bool,

        /// Write the subdirectories that are in no domain into `[unassigned]
        /// undecided` in `.nibdex-domains.toml`, each with the observed evidence
        /// as a comment — then decide by editing that file, moving entries into
        /// a domain's list or into `acknowledged`.
        ///
        /// The config file is the only place the decision is auditable, so it is
        /// also where the decision is made: no second format, no prompts, no
        /// import step. Never writes a suggested domain. Edits are additive,
        /// comments are preserved, and the audit reports whatever is still in
        /// `undecided` until you move it out.
        #[arg(long, conflicts_with_all = ["json", "triage"])]
        stage_undecided: bool,
    },

    /// Assign a subdirectory to IP domain(s), or record it as deliberately
    /// unassigned — the non-interactive counterpart to `audit --triage`.
    ///
    /// Takes a domain; never derives one. Edits are additive (this can add a
    /// label, never remove or reorder one), comments and formatting in
    /// `.nibdex-domains.toml` are preserved, and the result is re-validated
    /// before the command returns. Re-index the affected domains afterwards —
    /// this changes labels, not indexes.
    Label {
        /// The top-level subdirectory to assign, relative to the workspace root.
        subdir: String,

        /// Domain to label it for. Repeat to share across several domains.
        #[arg(long, conflicts_with = "acknowledge")]
        domain: Vec<String>,

        /// Record it as reviewed and deliberately in no domain. Grants nothing:
        /// it stays unindexed and still taints sessions that touch it.
        #[arg(long, conflicts_with = "domain")]
        acknowledge: bool,

        /// Workspace root — where `.nibdex-domains.toml` lives.
        #[arg(long, default_value = ".")]
        workspace: PathBuf,

        /// Print the change that would be made and exit without writing.
        #[arg(long)]
        dry_run: bool,
    },

    /// Derive an IP-safe, scrubbed metrics payload from the JSONL stream + a
    /// recomputed `check()` snapshot. Writes a candidate file for you to
    /// INSPECT and then hand over yourself — nibdex never transmits it (zero
    /// network egress). Default-deny allowlist: only contract-classified
    /// fields appear; raw queries, paths, and error messages are dropped or
    /// transformed.
    ///
    /// This is the ONLY command that produces a file intended to leave the
    /// machine, so it is deliberately explicit: you run it, you read the
    /// output, you decide whether to share it.
    MetricsExport {
        /// Path to the metrics JSONL stream (the `jsonl:<path>` sink target).
        #[arg(long, default_value = "./metrics.jsonl")]
        metrics_jsonl: PathBuf,

        /// SQLite database path (for the recomputed `check()` snapshot).
        #[arg(long, default_value = "nibdex.db")]
        db: PathBuf,

        /// §8.4 calibration model — enables the cost-savings rollup in the
        /// snapshot. Missing file degrades to no rollup (Layer-1-only).
        #[arg(long, default_value = "./calibration.toml")]
        calibration_toml: PathBuf,

        /// Export only calls within the last N days — all-or-nothing within
        /// the window, no per-row cherry-picking (§6.1). Omit for the full stream.
        #[arg(long)]
        days: Option<u32>,

        /// Output path for the candidate payload to inspect before sharing.
        #[arg(long, default_value = "./nibdex-metrics-export.json")]
        out: PathBuf,
    },

    /// Re-score an archived `metrics-export` payload under the current
    /// `calibration.toml` savings model and print a before/after diff. Each
    /// export row carries the raw inputs the model needs
    /// (`result_token_estimate` + `candidate_count.after_rank`), so the
    /// re-score is fully reproducible offline — no daemon, no DB, no network.
    /// Uses the same `Layer2Fields::compute` as the live emit path, so the
    /// numbers can't drift from production. Read-only: writes nothing.
    Rescore {
        /// Path to an archived `nibdex metrics-export` JSON payload.
        export: PathBuf,

        /// §8.4 calibration model to re-score against (the new savings model).
        /// Missing file degrades to no re-score (Layer-1-only); bad TOML
        /// refuses to run (D-8.1).
        #[arg(long, default_value = "./calibration.toml")]
        calibration_toml: PathBuf,
    },

    /// Print this binary's build provenance — crate version, git sha,
    /// `git describe --dirty`, and commit time. Verifies *which* build you
    /// have without a running daemon (e.g. against `target/release/nibdex`
    /// after a deploy). The live equivalent is the `build` block in the
    /// `check()` MCP tool / `/healthz`.
    Version {
        /// Emit the build block as JSON instead of human-readable lines.
        #[arg(long)]
        json: bool,
    },
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
pub enum McpTransport {
    Http,
    Stdio,
}
