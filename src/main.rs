// SPDX-License-Identifier: MIT

#![forbid(unsafe_code)]

mod audit;
mod build_info;
mod calibration;
mod cli;
mod cost_ledger;
mod db;
mod diff_index;
mod domains;
mod extractor;
mod hash;
mod hook;
mod http_server;
mod indexer;
mod mcp;
mod metrics;
mod metrics_export;
mod metrics_sink;
mod rescore;
mod schema_index;
mod session_index;
mod source_index;
mod triage;
mod symbol_index;
mod thread_metric;
mod watcher;

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use clap::Parser;

use crate::calibration::CalibrationModel;
use crate::metrics_sink::{MetricsSink, MetricsSinkSpec};

#[tokio::main]
async fn main() -> Result<()> {
    let args = cli::Args::parse();
    match args.command {
        cli::Command::Hook { stats } => {
            // Never returns; every path exits 0 so a hook failure can never
            // break the caller's own search.
            if stats {
                hook::stats();
            }
            hook::run().await;
        }
        cli::Command::SchemaDumpQuery { dialect } => {
            match schema_index::dump_query(&dialect) {
                Some(q) => print!("{q}"),
                // Named alternatives rather than a bare "unknown": the whole
                // point of shipping the query is that the user should not have
                // to go looking, and an error that sends them looking anyway
                // has given the cost back.
                None => anyhow::bail!(
                    "unknown dialect {dialect:?} — supported: postgres, mssql"
                ),
            }
        }
        cli::Command::Index {
            workspace,
            memory_dir,
            projects_dir,
            db,
            git_max_depth,
            include_nested_repos,
            domain,
            max_commits_per_repo,
        } => {
            let workspace = workspace.unwrap_or(std::env::current_dir()?);
            let pool = db::open(&db).await?;
            let prior_scan = metrics::last_measurement(&pool, "indexer.full_scan").await?;
            let prior_session = metrics::last_measurement(&pool, "extract.session_history").await?;
            let prior_memory = metrics::last_measurement(&pool, "extract.memory").await?;
            let prior_design = metrics::last_measurement(&pool, "extract.design_docs").await?;
            let prior_commits = metrics::last_measurement(&pool, "extract.commits").await?;
            let git_opts = indexer::GitOptions {
                max_depth: git_max_depth,
                nested_mode: if include_nested_repos {
                    extractor::git_commits::NestedMode::Include
                } else {
                    extractor::git_commits::NestedMode::Skip
                },
                max_commits_per_repo,
            };
            // Canonicalize so `documents.path` for memory files is absolute: a
            // relative `--memory-dir ./mem` used to be stored relative and then
            // read as "missing" by `check()` from any other cwd — false orphans.
            let memory_dir = memory_dir.map(|m| m.canonicalize().unwrap_or(m));
            let stats = indexer::full_scan(
                &pool,
                &workspace,
                memory_dir.as_deref(),
                projects_dir.as_deref(),
                git_opts,
                domain.as_deref(),
            )
            .await?;
            pool.close().await;
            print_summary(
                &stats,
                prior_scan.as_ref(),
                prior_session.as_ref(),
                prior_memory.as_ref(),
                prior_design.as_ref(),
                prior_commits.as_ref(),
            );
        }
        cli::Command::IndexSource { repo, db } => {
            let repo = repo.unwrap_or(std::env::current_dir()?);
            let pool = db::open(&db).await?;
            let stats = source_index::index_source_repo(&pool, &repo).await?;
            pool.close().await;
            println!(
                "Indexed {} source file(s) → {} chunk(s) ({} unchanged, of {} tracked) in {}ms.",
                stats.files_indexed,
                stats.chunks,
                stats.files_unchanged,
                stats.files_tracked,
                stats.elapsed_ms
            );
            println!(
                "  skipped: {} binary, {} large (>256KB), {} unreadable",
                stats.skipped_binary, stats.skipped_large, stats.skipped_unreadable
            );
            println!(
                "  provenance: {} file(s) stamped with a commit, {} unresolved \
                 (run `nibdex index` first to populate commits)",
                stats.provenance_hits, stats.provenance_misses
            );
        }
        cli::Command::FindCode {
            query,
            db,
            limit,
            format,
        } => {
            let pool = db::open(&db).await?;
            // Same FTS5-syntax safety as the MCP tool: a raw hyphenated query
            // ("match-centered") crashes bare MATCH with a column error.
            let sanitized = mcp::sanitize_fts5_query(&query);
            let hits = source_index::find_code(&pool, &sanitized, limit).await?;
            pool.close().await;
            match format {
                cli::FindCodeFormat::Grep => {
                    // One `path:line:excerpt` line per hit — NeoVim quickfix-friendly.
                    for h in &hits {
                        println!("{}", source_index::grep_line(h));
                    }
                }
                cli::FindCodeFormat::Pretty => {
                    if hits.is_empty() {
                        println!("no code hits for {query:?}");
                    } else {
                        println!("{} hit(s) for {query:?}:\n", hits.len());
                        // `path` is repo-relative, so name the repo when the index
                        // spans more than one — otherwise the hit is not openable.
                        let repos: std::collections::BTreeSet<&str> =
                            hits.iter().filter_map(|h| h.repo_path.as_deref()).collect();
                        if repos.len() > 1 {
                            println!("  spanning {} repos: {}\n", repos.len(),
                                     repos.iter().copied().collect::<Vec<_>>().join(", "));
                        }
                        for (i, h) in hits.iter().enumerate() {
                            let lang = h.language.as_deref().unwrap_or("?");
                            // Freshness gate: flag any non-verified location loudly
                            // (the line numbers may have drifted off the live file).
                            let loc = match h.location {
                                source_index::LocationStatus::Verified => String::new(),
                                other => format!("  ⚠ {}", other.as_str()),
                            };
                            println!(
                                "{}. {}:{}  (chunk {}-{})  [{}]  rank={:.3}{}",
                                i + 1,
                                h.path,
                                h.match_line,
                                h.line_start,
                                h.line_end,
                                lang,
                                h.rank,
                                loc
                            );
                            match (&h.commit_sha, &h.commit_summary) {
                                (Some(sha), Some(msg)) => {
                                    println!("   ↳ via {}  {}", &sha[..sha.len().min(8)], msg)
                                }
                                _ => println!("   ↳ via (no provenance commit)"),
                            }
                            // Bounded excerpt — first few non-blank lines (D-10.11 spirit).
                            for line in h.body.lines().filter(|l| !l.trim().is_empty()).take(4) {
                                println!("   | {}", line);
                            }
                            println!();
                        }
                    }
                }
            }
        }
        cli::Command::IndexDiffs { repo, db } => {
            let repo = repo.unwrap_or(std::env::current_dir()?);
            let pool = db::open(&db).await?;
            let stats = diff_index::index_diffs(&pool, &repo).await?;
            pool.close().await;
            println!(
                "Indexed {} added-block(s) across {} commit(s) in {}ms.",
                stats.hunks_indexed, stats.commits_seen, stats.elapsed_ms
            );
            println!(
                "  provenance: {} block(s) bound to an indexed commit, {} unresolved \
                 (run `nibdex index` first)",
                stats.provenance_hits, stats.provenance_misses
            );
            if stats.blocks_skipped_large > 0 {
                println!("  skipped: {} oversize block(s)", stats.blocks_skipped_large);
            }
        }
        cli::Command::TraceCode { query, db, limit } => {
            let pool = db::open(&db).await?;
            let hits = diff_index::trace_code(&pool, &query, limit).await?;
            pool.close().await;
            if hits.is_empty() {
                println!("no diff hits for {query:?}");
            } else {
                println!(
                    "{} change(s) introducing {query:?}, OLDEST FIRST \
                     (top = authoring candidate):\n",
                    hits.len()
                );
                for (i, h) in hits.iter().enumerate() {
                    let marker = if i == 0 { "⇒" } else { " " };
                    println!(
                        "{} {}. {}  {}",
                        marker,
                        i + 1,
                        &h.commit_hash[..h.commit_hash.len().min(8)],
                        h.summary
                    );
                    println!(
                        "     {}:{}  rank={:.3}  authored_at={}",
                        h.file_path, h.new_start, h.rank, h.authored_at
                    );
                    for line in h.body.lines().filter(|l| !l.trim().is_empty()).take(3) {
                        println!("     | {}", line);
                    }
                    println!();
                }
            }
        }
        cli::Command::ResolveCode { query, db, cap } => {
            let pool = db::open(&db).await?;
            let res = diff_index::resolve_provenance(&pool, &query, cap).await?;
            pool.close().await;
            print_resolution(&res);
        }
        cli::Command::IndexSessions {
            projects_dir,
            slug,
            workspace_scoped,
            all_slugs,
            rebuild,
            workspace,
            domain,
            db,
        } => {
            let projects_dir = match projects_dir.or_else(session_index::default_projects_dir) {
                Some(p) => p,
                None => anyhow::bail!("could not resolve ~/.claude/projects (set $HOME or --projects-dir)"),
            };
            // Fail-narrow: scope MUST be explicit so a run never silently pulls
            // transcripts from another workspace / IP domain (SESSION_SCOPE_DESIGN §2).
            let scope = match (slug.as_deref(), workspace_scoped, all_slugs) {
                (None, false, false) => anyhow::bail!(
                    "scope required: --workspace-scoped for this workspace's sessions, \
                     --slug=<s> for one slug dir, or --all-slugs for machine-global"
                ),
                (Some(s), false, false) => session_index::SessionScope::Slug(s),
                (None, true, false) => session_index::SessionScope::Workspace,
                (None, false, true) => session_index::SessionScope::AllSlugs,
                _ => anyhow::bail!(
                    "pass exactly ONE of --workspace-scoped, --slug=<s>, or --all-slugs"
                ),
            };
            let workspace = workspace.unwrap_or_else(|| std::path::PathBuf::from("."));
            let pool = db::open(&db).await?;
            let stats = session_index::index_sessions(
                &pool,
                &projects_dir,
                scope,
                rebuild,
                &workspace,
                domain.as_deref(),
            )
            .await?;
            pool.close().await;
            println!(
                "Indexed {} write-edge(s) ({} Edit, {} Write) across {} session(s) \
                 from {} transcript(s) in {}ms.",
                stats.edges_indexed,
                stats.edits,
                stats.writes,
                stats.sessions_seen,
                stats.transcripts_seen,
                stats.elapsed_ms
            );
            if stats.edges_duplicate > 0
                || stats.edges_skipped_no_uuid > 0
                || stats.edges_skipped_no_timestamp > 0
            {
                println!(
                    "  merge: {} already indexed (skipped), {} skipped (no message uuid), {} skipped (no timestamp)",
                    stats.edges_duplicate,
                    stats.edges_skipped_no_uuid,
                    stats.edges_skipped_no_timestamp
                );
            }
            if stats.edges_late_bound > 0 {
                println!(
                    "  late binding: {} previously-unbound edge(s) acquired their capturing commit",
                    stats.edges_late_bound
                );
            }
            // Same silent-zero this release is closing on the query side: without
            // this line a permissions problem under the transcript root reads as
            // "0 write-edge(s)" with no hint that anything was skipped.
            if stats.transcripts_unreadable > 0 {
                println!(
                    "  skipped: {} transcript(s) could not be read (permissions, non-UTF-8, \
                     or pruned mid-scan)",
                    stats.transcripts_unreadable
                );
            }
            println!(
                "  binding: {} edge(s) bound to a capturing commit, {} unbound \
                 (run `nibdex index` first, or the edit predates HEAD)",
                stats.commit_bound, stats.commit_unbound
            );
            if domain.is_some() {
                println!(
                    "  domain [{}]: {} edge(s) dropped (foreign target), {} rationale(s) \
                     withheld (cross-domain session)",
                    domain.as_deref().unwrap_or(""),
                    stats.edges_dropped_foreign_domain,
                    stats.rationales_withheld
                );
            }
            if workspace_scoped {
                println!(
                    "  workspace scope [{}]: {} edge(s) dropped (foreign session), \
                     {} dropped (in-workspace session wrote outside)",
                    workspace.display(),
                    stats.edges_dropped_foreign_workspace,
                    stats.edges_dropped_foreign_target
                );
            }
            if stats.lines_parse_err > 0 {
                println!(
                    "  parsed {} line(s), {} unparseable (skipped)",
                    stats.lines_total, stats.lines_parse_err
                );
            }
        }
        cli::Command::FindSessionEdge { query, db, limit } => {
            let pool = db::open(&db).await?;
            let hits = session_index::find_session_edge(&pool, &query, limit).await?;
            pool.close().await;
            if hits.is_empty() {
                println!("no session edges for {query:?}");
            } else {
                println!("{} session edge(s) for {query:?}:\n", hits.len());
                for (i, h) in hits.iter().enumerate() {
                    let branch = h.git_branch.as_deref().unwrap_or("?");
                    println!(
                        "{}. {} {}  [{}@{}]  rank={:.3}",
                        i + 1,
                        h.tool,
                        h.file_path,
                        branch,
                        h.edited_at,
                        h.rank
                    );
                    match (&h.commit_hash, &h.commit_summary) {
                        (Some(sha), Some(msg)) => {
                            println!("   ↳ captured by {}  {}", &sha[..sha.len().min(8)], msg)
                        }
                        _ => println!("   ↳ (no capturing commit bound)"),
                    }
                    let rationale = h.rationale.trim();
                    if !rationale.is_empty() {
                        let snippet: String = rationale.chars().take(160).collect();
                        println!("   | {snippet}");
                    }
                    println!("   | session {}", &h.session_id[..h.session_id.len().min(8)]);
                    println!();
                }
            }
        }
        cli::Command::MeasureThread { db, terms, cap } => {
            let pool = db::open(&db).await?;
            let report = thread_metric::measure_thread(&pool, terms, cap).await?;
            pool.close().await;
            print_thread_report(&report);
        }
        cli::Command::CompareTokenizers { db, terms, cap } => {
            let pool = db::open(&db).await?;
            let cmp = symbol_index::compare_tokenizers(&pool, terms, cap).await?;
            pool.close().await;
            print_tokenizer_comparison(&cmp);
        }
        cli::Command::Mcp {
            db,
            metrics_sink,
            calibration_toml,
        } => {
            // A query server must not manufacture its own corpus. `db::open`
            // creates a missing file and migrates it, so a typo'd or
            // cwd-relative `--db` in an `.mcp.json` used to yield a permanently
            // empty index that answered `corpus_empty: true` to everything with
            // no other symptom (RC1 review 1.7). Refuse instead: the fix is one
            // path away and the error names it.
            if !db.exists() {
                anyhow::bail!(
                    "nibdex mcp: no index at {} (resolved from cwd {}). Run \
                     `nibdex index --db <path>` first, or pass the same --db path \
                     you indexed to. Refusing to create an empty database here, \
                     because it would answer every query with an empty result.",
                    db.display(),
                    std::env::current_dir()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|_| "?".to_string()),
                );
            }
            let pool = db::open(&db).await?;
            reconcile_cost_ledger(&pool, &metrics_sink).await;
            let sink = resolve_metrics_sink(metrics_sink)?;
            let calibration = resolve_calibration(&calibration_toml);
            mcp::serve_stdio(pool, sink, calibration).await?;
        }
        cli::Command::Watch {
            workspace,
            memory_dir,
            db,
            git_max_depth,
            include_nested_repos,
            max_commits_per_repo,
        } => {
            let workspace = workspace.unwrap_or(std::env::current_dir()?);
            let pool = db::open(&db).await?;
            let git_opts = subscription_git_opts(git_max_depth, include_nested_repos);
            let subscriptions = resolve_subscriptions(&workspace, memory_dir.as_deref(), git_opts)?;
            let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
            tokio::spawn(async move {
                wait_for_shutdown_signal().await;
                let _ = shutdown_tx.send(());
            });
            eprintln!(
                "nibdex watch — {} subscription(s); 500ms debounce; ctrl-c to exit.",
                subscriptions.len()
            );
            for sub in &subscriptions {
                eprintln!("  watching {}", sub.watch_path().display());
            }
            watcher::serve_with(
                pool,
                subscriptions,
                shutdown_rx,
                watcher::WatcherConfig { max_commits_per_repo },
            )
            .await?;
            eprintln!("nibdex watch — exited.");
        }
        cli::Command::Serve {
            http,
            workspace,
            memory_dir,
            db,
            metrics_sink,
            calibration_toml,
            git_max_depth,
            include_nested_repos,
            max_commits_per_repo,
        } => {
            let workspace = workspace.unwrap_or(std::env::current_dir()?);
            // Loopback gate FIRST — before the db is opened or the watcher spawned.
            // `http_server::serve` re-checks, but by then the watcher had already
            // registered FS watches and written `file_watcher_state`, then hung
            // for the 2 s drain (RC1 review, sev-2). A refused bind must be inert.
            if !http.ip().is_loopback() {
                anyhow::bail!(
                    "nibdex serve: bind address {http} is not loopback. D-6.4.3 \
                     requires 127.0.0.1 / [::1] at MVP."
                );
            }
            // The watcher is not domain-aware: it re-indexes every repo it discovers
            // into whatever db it holds. Say so when a domains file is present, since
            // pointing this at a per-domain db silently breaks the partition
            // (docs/IP_DOMAINS.md "Domain databases are index-only").
            if workspace.join(".nibdex-domains.toml").exists() {
                eprintln!(
                    "[nibdex serve] warning: {} has a .nibdex-domains.toml, but the \
                     file-watching daemon is not domain-aware — it re-indexes every \
                     discovered repo into {}. Do NOT point it at a per-domain database; \
                     query those with `nibdex mcp --db <domain.db>` and refresh with \
                     `nibdex index --domain <name>`.",
                    workspace.display(),
                    db.display()
                );
            }
            let pool = db::open(&db).await?;
            reconcile_cost_ledger(&pool, &metrics_sink).await;
            let sink = resolve_metrics_sink(metrics_sink)?;
            let calibration = resolve_calibration(&calibration_toml);
            let git_opts = subscription_git_opts(git_max_depth, include_nested_repos);
            let memory_dir = memory_dir.map(|m| m.canonicalize().unwrap_or(m));
            let subscriptions = resolve_subscriptions(&workspace, memory_dir.as_deref(), git_opts)?;

            // One outer shutdown signal fans out to watcher + HTTP server via
            // two child oneshot channels. SIGINT/SIGTERM cancels both.
            let (watcher_tx, watcher_rx) = tokio::sync::oneshot::channel();
            let (http_tx, http_rx) = tokio::sync::oneshot::channel();
            tokio::spawn(async move {
                wait_for_shutdown_signal().await;
                let _ = watcher_tx.send(());
                let _ = http_tx.send(());
            });

            eprintln!(
                "nibdex serve — {} watcher subscription(s); HTTP at {}; ctrl-c to exit.",
                subscriptions.len(),
                http,
            );
            for sub in &subscriptions {
                eprintln!("  watching {}", sub.watch_path().display());
            }

            let watcher_pool = pool.clone();
            let watcher_handle = tokio::spawn(async move {
                watcher::serve_with(
                    watcher_pool,
                    subscriptions,
                    watcher_rx,
                    watcher::WatcherConfig { max_commits_per_repo },
                )
                .await
            });

            let http_result = http_server::serve(pool, http, http_rx, sink, calibration).await;

            // Drain the watcher's drain window even on HTTP failure so the
            // 1s D-6.2.6 guarantee carries forward end-to-end.
            let watcher_result =
                match tokio::time::timeout(std::time::Duration::from_secs(2), watcher_handle).await
                {
                    Ok(join) => join?,
                    Err(_) => {
                        eprintln!("nibdex serve — watcher did not exit within 2s drain.");
                        Ok(())
                    }
                };

            // Surface the first error if either side failed.
            http_result?;
            watcher_result?;
        }
        cli::Command::PrintMcpConfig {
            transport,
            http,
            db,
            binary,
            name,
        } => {
            let snippet = match transport {
                cli::McpTransport::Http => {
                    serde_json::json!({
                        "mcpServers": {
                            &name: {
                                "type": "http",
                                "url": format!("http://{http}/mcp"),
                            }
                        }
                    })
                }
                cli::McpTransport::Stdio => {
                    let bin = match binary {
                        Some(b) => b,
                        None => std::env::current_exe()?,
                    };
                    let db_string = db.to_string_lossy().into_owned();
                    serde_json::json!({
                        "mcpServers": {
                            &name: {
                                "command": bin.to_string_lossy(),
                                "args": ["mcp", "--db", db_string],
                            }
                        }
                    })
                }
            };
            println!("{}", serde_json::to_string_pretty(&snippet)?);
        }
        cli::Command::Audit {
            workspace,
            domain,
            db,
            config_only,
            json,
            triage,
            stage_undecided,
        } => {
            let pool = db::open(&db).await?;
            let report = audit::run(&pool, &workspace, &domain, config_only).await?;
            let ws_c = workspace.canonicalize()?;
            let loaded = domains::DomainConfig::load(&ws_c)?;
            let pending = loaded
                .as_ref()
                .map(|c| triage::unassigned_subdirs(c, &ws_c))
                .unwrap_or_default();
            let staged: Vec<String> =
                loaded.as_ref().map(|c| c.undecided().to_vec()).unwrap_or_default();
            if json {
                println!("{}", audit::render_json(&report, &pending, &staged));
            } else {
                print!("{}", audit::render(&report));
            }

            if stage_undecided {
                let ws = &ws_c;
                let cfg = loaded.as_ref().ok_or_else(|| {
                    anyhow::anyhow!("no .nibdex-domains.toml at {}", ws.display())
                })?;
                let decisions = triage::stage_undecided(cfg, &pending);
                if decisions.is_empty() {
                    println!(
                        "\nStaging: nothing new — every subdirectory is labeled, \
                         acknowledged, or already in `undecided`."
                    );
                } else {
                    println!("\nPlanned change to {}:", triage::config_path(ws).display());
                    print!("{}", triage::render_plan(&decisions, cfg.undecided()));
                    let n = triage::apply(ws, &decisions)?;
                    println!(
                        "\nStaged {n} subdirector{} in [unassigned] undecided.\n\
                         Decide by editing {} — move each entry into a domain's list \
                         under [domains],\nor into `acknowledged`. Re-index the affected \
                         domain(s) afterwards.",
                        if n == 1 { "y" } else { "ies" },
                        triage::config_path(ws).display()
                    );
                }
            }

            if triage {
                let ws = ws_c;
                let cfg = domains::DomainConfig::load(&ws)?
                    .ok_or_else(|| anyhow::anyhow!("no .nibdex-domains.toml at {}", ws.display()))?;
                if pending.is_empty() {
                    println!("\nTriage: nothing unassigned. Every subdirectory has a decision.");
                } else if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
                    // Never prompt into a pipe: an unread prompt answered by
                    // stray input could label a directory for the wrong domain.
                    println!(
                        "\nTriage needs a terminal ({} subdir(s) pending). Re-run interactively.",
                        pending.len()
                    );
                } else {
                    let names = cfg.domain_names();
                    let stdin = std::io::stdin();
                    let mut lock = stdin.lock();
                    let mut out = std::io::stdout();
                    let decisions =
                        triage::run_triage(&ws, &pending, &names, &mut lock, &mut out)?;

                    println!("\nPlanned change to {}:", triage::config_path(&ws).display());
                    print!("{}", triage::render_plan(&decisions, cfg.undecided()));
                    print!("\nApply? [y/N] ");
                    use std::io::Write as _;
                    std::io::stdout().flush()?;
                    let mut answer = String::new();
                    std::io::BufRead::read_line(&mut lock, &mut answer)?;
                    if answer.trim().eq_ignore_ascii_case("y") {
                        let n = triage::apply(&ws, &decisions)?;
                        println!("Applied {n} change(s). Re-index the affected domain(s).");
                    } else {
                        println!("Nothing written.");
                    }
                }
            }
            // Exit non-zero only on ERROR-severity findings: config that cannot
            // work as written. WARN/INFO are for a human to weigh, not for a
            // script to fail on.
            if report.worst() == Some(audit::Severity::Error) {
                std::process::exit(1);
            }
        }
        cli::Command::Label {
            subdir,
            domain,
            acknowledge,
            workspace,
            dry_run,
        } => {
            let ws = workspace.canonicalize()?;
            // Take a domain; never derive one. Requiring an explicit choice is
            // the whole safety property — the command IS the confirmation that
            // the interactive path gets from shown-and-confirmed.
            let decision = if acknowledge {
                triage::Decision::Acknowledge
            } else if domain.is_empty() {
                anyhow::bail!(
                    "pass --domain <name> (repeat to share across domains) or --acknowledge; \
                     nibdex will not choose a domain for you"
                );
            } else {
                triage::Decision::Label(domain)
            };
            let decisions = vec![(subdir.clone(), decision)];
            // Load the config purely so the plan can show a `undecided` entry
            // being retired alongside the decision that retires it.
            let staged: Vec<String> = domains::DomainConfig::load(&ws)?
                .map(|c| c.undecided().to_vec())
                .unwrap_or_default();
            println!("Planned change to {}:", triage::config_path(&ws).display());
            print!("{}", triage::render_plan(&decisions, &staged));
            if dry_run {
                println!("\n--dry-run: nothing written.");
            } else {
                let n = triage::apply(&ws, &decisions)?;
                if n == 0 {
                    println!("\nAlready recorded; nothing to do.");
                } else {
                    println!("\nApplied {n} change(s). Re-index the affected domain(s).");
                }
            }
        }
        cli::Command::MetricsExport {
            metrics_jsonl,
            db,
            calibration_toml,
            days,
            out,
        } => {
            let pool = db::open(&db).await?;
            let calibration = resolve_calibration(&calibration_toml);
            let summary = metrics_export::run_export(
                &pool,
                calibration.as_deref(),
                &metrics_jsonl,
                days,
                chrono::Utc::now(),
                &out,
            )
            .await?;
            pool.close().await;

            // §7.2 approval flow: we wrote a candidate file. The user inspects
            // it and chooses to hand it over — nibdex never transmits it.
            println!(
                "Wrote candidate metrics export → {}",
                summary.out_path.display()
            );
            let window = summary
                .window_days
                .map(|d| format!(" (last {d}d)"))
                .unwrap_or_default();
            println!(
                "  rows: {} total, {} in window{}",
                summary.rows_total, summary.rows_in_window, window
            );
            if summary.rows_unparseable_ts > 0 {
                println!(
                    "  {} row(s) had an unparseable ts — KEPT, not silently dropped",
                    summary.rows_unparseable_ts
                );
            }
            println!(
                "  honesty: {} error row(s) and {} loss row(s) (negative tokens_saved) included",
                summary.error_rows, summary.loss_rows
            );
            if summary.dropped_unsafe_keys > 0 {
                println!(
                    "  WARNING: {} verbatim-map key(s) dropped by the §2 allowlist — \
                     unexpected (drift or tampering); inspect before sharing",
                    summary.dropped_unsafe_keys
                );
            }
            println!();
            println!(
                "NEXT: inspect the file in full, then hand it over yourself if you approve."
            );
            println!(
                "nibdex never transmits it (zero network egress). Contract: docs/METRICS_EXPORT_SPEC.md"
            );
        }
        cli::Command::Rescore {
            export,
            calibration_toml,
        } => {
            let Some(model) = resolve_calibration(&calibration_toml) else {
                anyhow::bail!(
                    "no calibration model at {} — nothing to re-score against (Layer-1-only)",
                    calibration_toml.display()
                );
            };
            let report = rescore::run_rescore(&export, &model)?;
            print!("{}", rescore::render(&report));
        }
        cli::Command::Version { json } => {
            let b = build_info::build_info();
            if json {
                println!("{}", serde_json::to_string_pretty(&b)?);
            } else {
                println!("nibdex {}", b.crate_version);
                println!("  git:    {} ({})", b.git_sha, b.git_describe);
                println!("  commit: {}", b.commit_time);
            }
        }
    }
    Ok(())
}

/// Open the metrics sink at CLI parse → runtime time. `Off` becomes
/// `None` so handlers can short-circuit on the `Option` before any
/// match-on-variant work; non-Off specs return `Some(Arc<_>)`.
fn resolve_metrics_sink(spec: MetricsSinkSpec) -> Result<Option<Arc<MetricsSink>>> {
    if matches!(spec, MetricsSinkSpec::Off) {
        return Ok(None);
    }
    let sink = MetricsSink::from_spec(spec)?;
    Ok(Some(Arc::new(sink)))
}

/// Reconcile the cost-ledger from a durable JSONL sink at startup. The sink
/// outlives a DB rebuild but `cost_ledger_events` does not, so without this
/// `check().cost_savings` silently resets to near-zero after every rebuild
/// (it reads the ledger, not the sink). Only meaningful for the `jsonl:`
/// spec; `off`/`stdout` have no durable log to reconcile from. Best-effort:
/// a failure logs and does not block startup (metrics are observation).
async fn reconcile_cost_ledger(pool: &sqlx::SqlitePool, spec: &MetricsSinkSpec) {
    let MetricsSinkSpec::Jsonl(path) = spec else {
        return;
    };
    match cost_ledger::backfill_from_jsonl(pool, path).await {
        Ok(r) if r.inserted > 0 => eprintln!(
            "nibdex — reconciled {} cost-ledger event(s) from sink ({} scanned, {} already present)",
            r.inserted, r.scanned, r.skipped_existing
        ),
        Ok(_) => {}
        Err(e) => eprintln!("[nibdex] cost-ledger sink reconcile failed: {e:#}"),
    }
}

/// Load `calibration.toml` at startup. Missing file → `None` + warn;
/// bad TOML panics inside `load_or_warn` (D-8.1 misconfig invariant).
/// Mirrors the Day 7 `resolve_metrics_sink` shape — `Option<Arc<_>>`
/// so handlers can short-circuit on `None` before any model lookup.
fn resolve_calibration(path: &Path) -> Option<Arc<CalibrationModel>> {
    CalibrationModel::load_or_warn(path).map(Arc::new)
}

/// Build the `GitOptions` the watcher's repo discovery uses, from the `serve`/
/// `watch` flags. Only `max_depth` + `nested_mode` matter here (commit-cap is a
/// full-scan concern); the cap keeps the `Default`.
fn subscription_git_opts(git_max_depth: usize, include_nested_repos: bool) -> indexer::GitOptions {
    indexer::GitOptions {
        max_depth: git_max_depth,
        nested_mode: if include_nested_repos {
            extractor::git_commits::NestedMode::Include
        } else {
            extractor::git_commits::NestedMode::Skip
        },
        ..indexer::GitOptions::default()
    }
}

fn resolve_subscriptions(
    workspace: &std::path::Path,
    memory_dir_override: Option<&std::path::Path>,
    git_opts: indexer::GitOptions,
) -> anyhow::Result<Vec<watcher::Subscription>> {
    let mut subs = Vec::new();

    // Per-project anchors drive ClaudeMd + DesignDir subscriptions so the
    // watcher mirrors `indexer::full_scan` coverage in a workspace-of-projects
    // layout (G1 fix). Memory is single-rooted by Claude Code convention.
    // `git_opts` (depth + nested mode) is passed in so the watcher's repo
    // discovery matches the `index`/`serve` flags — critical when the workspace
    // root is itself a repo (nested projects must be Included, not Skipped).
    let anchors = indexer::discover_project_anchors(workspace, git_opts);

    let mut had_claude_md = false;
    for anchor in &anchors {
        let claude_md = anchor.join("CLAUDE.md");
        if claude_md.exists() {
            subs.push(watcher::Subscription::ClaudeMd {
                path: claude_md,
                workspace: anchor.clone(),
            });
            had_claude_md = true;
        }
    }
    if !had_claude_md {
        eprintln!(
            "[nibdex watch] note: no CLAUDE.md found under {} or any discovered project — session-history subscription skipped.",
            workspace.display()
        );
    }

    let memory = memory_dir_override
        .map(std::path::PathBuf::from)
        .or_else(|| indexer::default_memory_dir(workspace));
    if let Some(mem) = memory {
        if mem.exists() {
            subs.push(watcher::Subscription::MemoryDir(mem));
        } else {
            eprintln!(
                "[nibdex watch] note: memory dir {} not found — memory subscription skipped.",
                mem.display()
            );
        }
    }

    let mut had_design_dir = false;
    for anchor in &anchors {
        // Watch all of `docs/` (was `docs/design/`) so the live-freshness set
        // mirrors full_scan's broadened design-doc coverage (D1 gear-6 fix).
        let design = anchor.join("docs");
        if design.exists() {
            subs.push(watcher::Subscription::DesignDir(design));
            had_design_dir = true;
        }
    }
    if !had_design_dir {
        eprintln!(
            "[nibdex watch] note: no docs/ directory found under {} or any discovered project — design-doc subscription skipped.",
            workspace.display()
        );
    }

    // Root-level *.md (BUG_TRIAGE.md et al.) also feed the design corpus (2026-07-09).
    // One non-recursive watch per anchor root so those files re-index live — lockstep
    // with full_scan's `list_root_markdown`. The anchor always exists (it's a
    // discovered repo / the workspace root); the OS watch is deduped against
    // ClaudeMd's identical anchor watch in spawn_debouncer.
    for anchor in &anchors {
        subs.push(watcher::Subscription::RootMarkdown(anchor.clone()));
    }

    // D-6.2.4: one `GitRefs` subscription per discovered repo. Discovery shape
    // mirrors the `index`/`serve` `git_opts` (depth + nested mode) so the watcher
    // and the full-scan path agree on which repos belong to this workspace.
    let repos = extractor::git_commits::discover_repos(
        workspace,
        git_opts.max_depth,
        git_opts.nested_mode,
    );
    for repo_path in repos {
        subs.push(watcher::Subscription::GitRefs(repo_path));
    }
    if subs.is_empty() {
        anyhow::bail!("no watch targets found under {}", workspace.display());
    }
    Ok(subs)
}

#[cfg(unix)]
async fn wait_for_shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut int = signal(SignalKind::interrupt()).expect("install SIGINT handler");
    let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    tokio::select! {
        _ = int.recv() => {}
        _ = term.recv() => {}
    }
}

#[cfg(not(unix))]
async fn wait_for_shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

fn print_summary(
    stats: &indexer::ScanStats,
    prior_scan: Option<&metrics::Measurement>,
    prior_session: Option<&metrics::Measurement>,
    prior_memory: Option<&metrics::Measurement>,
    prior_design: Option<&metrics::Measurement>,
    prior_commits: Option<&metrics::Measurement>,
) {
    println!(
        "Indexed {} documents in {}ms (session_history={}, memory={}, design_doc={}, source={}, schema={}).",
        stats.total(),
        stats.elapsed_ms,
        stats.session_history,
        stats.memory,
        stats.design_doc,
        stats.source_files,
        stats.schema_dumps
    );
    println!(
        "  session_entries: {} (extract.session_history {}ms)",
        stats.session_entries, stats.extract_session_history_ms
    );
    println!(
        "  memory_entries: {} ({} skipped: no frontmatter, or frontmatter without name/type) (extract.memory {}ms)",
        stats.memory_entries, stats.memory_skipped_no_frontmatter, stats.extract_memory_ms
    );
    println!(
        "  design_sections: {} across {} doc(s) (extract.design_docs {}ms)",
        stats.design_sections, stats.design_doc, stats.extract_design_docs_ms
    );
    println!(
        "  commits: {} across {} repo(s) ({} shallow, {} capped) (extract.commits {}ms)",
        stats.commits_inserted,
        stats.repos_indexed,
        stats.repos_shallow,
        stats.repos_capped,
        stats.extract_commits_ms,
    );
    println!(
        "  source: {} chunk(s) across {} file(s) ({} skipped to design/session, {} pruned: no longer tracked) (extract.source {}ms)",
        stats.source_chunks,
        stats.source_files,
        stats.source_skipped_other_corpus,
        stats.source_files_pruned,
        stats.extract_source_ms,
    );
    // Printed even at zero, unlike the other lines' details: a workspace with no
    // dump should SEE that the corpus exists and is empty, because the whole
    // failure mode of this corpus is a user who never learns it is available.
    println!(
        "  schema: {} object(s) across {} dump(s){} (extract.schema {}ms)",
        stats.schema_objects,
        stats.schema_dumps,
        if stats.schema_dumps_failed > 0 {
            format!(" ({} failed to parse)", stats.schema_dumps_failed)
        } else if stats.schema_dumps == 0 {
            " — none found; `nibdex schema-dump-query --help` to add one".to_string()
        } else {
            String::new()
        },
        stats.extract_schema_ms,
    );
    println!(
        "  session_edges: {} new, {} already indexed, across {} transcript(s) \
         ({} dropped: foreign session, {} dropped: in-workspace session wrote outside, \
         {} skipped: no timestamp) (extract.session_edges {}ms)",
        stats.session_edges,
        stats.session_edges_already_indexed,
        stats.session_transcripts,
        stats.session_edges_dropped_foreign_workspace,
        stats.session_edges_dropped_foreign_target,
        stats.session_edges_skipped_no_timestamp,
        stats.extract_session_edges_ms,
    );
    if stats.session_edges_late_bound > 0 {
        println!(
            "    {} previously-unbound edge(s) acquired their capturing commit",
            stats.session_edges_late_bound
        );
    }
    if stats.session_transcripts_unreadable > 0 {
        println!(
            "    note: {} transcript(s) could not be read and were skipped \
             (permissions, non-UTF-8, or pruned mid-scan) — the rest indexed normally",
            stats.session_transcripts_unreadable
        );
    }
    if let Some(err) = &stats.session_index_error {
        println!(
            "    WARNING: the session pass failed, so `find_session` may be stale or empty.\n\
             \x20             The other five corpora above indexed normally.\n\
             \x20             Reason: {err}"
        );
    }
    print_delta("indexer.full_scan", stats.elapsed_ms as i64, prior_scan);
    print_delta(
        "extract.session_history",
        stats.extract_session_history_ms as i64,
        prior_session,
    );
    print_delta(
        "extract.memory",
        stats.extract_memory_ms as i64,
        prior_memory,
    );
    print_delta(
        "extract.design_docs",
        stats.extract_design_docs_ms as i64,
        prior_design,
    );
    print_delta(
        "extract.commits",
        stats.extract_commits_ms as i64,
        prior_commits,
    );
}

fn print_delta(label: &str, current_ms: i64, prior: Option<&metrics::Measurement>) {
    match prior {
        Some(p) => {
            let delta_ms = current_ms - p.duration_ms;
            let pct = if p.duration_ms > 0 {
                (delta_ms as f64 / p.duration_ms as f64) * 100.0
            } else {
                0.0
            };
            print!(
                "  Δ vs prior {}: {:+}ms ({:+.1}%, prior={}ms",
                label, delta_ms, pct, p.duration_ms
            );
            if let Some(rss_delta) = p.rss_delta_bytes {
                print!(", prior rss_delta={}KB", rss_delta / 1024);
            }
            if let Some(rss_after) = p.rss_after_bytes {
                print!(", prior rss_after={}KB", rss_after / 1024);
            }
            if let Some(rows_in) = p.rows_in {
                print!(", prior rows_in={}", rows_in);
            }
            if let Some(rows_out) = p.rows_out {
                print!(", prior rows_out={}", rows_out);
            }
            println!(")");
            if !p.extra_json.is_empty() && p.extra_json != "{}" {
                println!("    prior extra: {}", p.extra_json);
            }
        }
        None => println!("  Δ vs prior {}: (no prior run)", label),
    }
}

/// Render a layer-aware provenance resolution (D1 gear-6 spike).
fn print_resolution(res: &diff_index::ProvenanceResolution) {
    let short = |sha: &str| sha[..sha.len().min(8)].to_string();

    println!("Provenance for {:?}:\n", res.query);
    match &res.author {
        Some(a) => {
            println!("AUTHOR (oldest code change):");
            println!("  {}  {}", short(&a.commit_hash), a.summary);
            println!("  {}:{}", a.file_path, a.new_start);
        }
        None => println!("AUTHOR: none — term appears only in prose (no code change matched)."),
    }

    if !res.other_code.is_empty() {
        println!(
            "\nlater code changes touching it ({} — movers/relateds):",
            res.other_code.len()
        );
        for c in &res.other_code {
            println!("  {}  {}  ({})", short(&c.commit_hash), c.summary, c.file_path);
        }
    }

    // Session feeders (gear 7) — the authoring MOMENTS: transcript Edit/Write
    // edges with their rationale. The same Δ-sign discriminator the prose feeders
    // get (gear-6): an edit BEFORE its authoring commit fed the code (the sharpest
    // feeder the corpus has — minutes, not a separate doc commit); an edit AFTER
    // merely references it later (D1_SCOPE §10 "Gear-7"). NOTE: the session layer
    // only reaches back as far as the transcript corpus exists — code authored
    // before the transcript window has no session feeder (a coverage boundary).
    let print_session = |f: &diff_index::SessionFeeder| {
        let delta = match f.delta_secs {
            Some(d) => humanize_delta(d),
            None => "(no author)".to_string(),
        };
        println!("  [{:<7}] {} {}  Δ {}", f.layer.label(), f.tool, f.file_path, delta);
        match (&f.commit_hash, &f.commit_summary) {
            (Some(sha), Some(msg)) => println!(
                "            ↳ session {}  captured by {}  {}",
                short(&f.session_id),
                short(sha),
                msg
            ),
            _ => println!(
                "            ↳ session {}  (no capturing commit — uncommitted/cross-repo)",
                short(&f.session_id)
            ),
        }
        let why = f.rationale.trim();
        if !why.is_empty() {
            let snippet: String = why.chars().take(140).collect();
            println!("            | {snippet}");
        }
    };
    let session_feeders: Vec<_> = res
        .session_feeders
        .iter()
        .filter(|f| f.delta_secs.is_some_and(|d| d < 0))
        .collect();
    let session_descendants: Vec<_> = res
        .session_feeders
        .iter()
        .filter(|f| f.delta_secs.is_none_or(|d| d >= 0))
        .collect();
    if session_feeders.is_empty() {
        if !res.session_feeders.is_empty() {
            println!(
                "\nSESSION FEEDERS (transcript edits predating the code): none \
                 ({} later edit(s) reference it — see below).",
                session_descendants.len()
            );
        }
    } else {
        println!(
            "\nSESSION FEEDERS — transcript edits authoring this ({}):",
            session_feeders.len()
        );
        for f in &session_feeders {
            print_session(f);
        }
    }
    if !session_descendants.is_empty() {
        println!(
            "\nsession descendants — later edits referencing it ({}; not provenance):",
            session_descendants.len()
        );
        for f in &session_descendants {
            print_session(f);
        }
    }

    // The Δ sign splits prose into FEEDERS (predate the code = fed it) vs
    // DESCENDANTS (postdate = document/reference it later). This directionality
    // is the §7 thread discriminator the gear-6 run surfaced (D1_SCOPE §10).
    let feeders: Vec<_> = res
        .feeders
        .iter()
        .filter(|f| f.delta_secs.is_some_and(|d| d < 0))
        .collect();
    let descendants: Vec<_> = res
        .feeders
        .iter()
        .filter(|f| f.delta_secs.is_none_or(|d| d >= 0))
        .collect();

    let print_feeder = |f: &diff_index::Feeder| {
        let delta = match f.delta_secs {
            Some(d) => humanize_delta(d),
            None => "(no author)".to_string(),
        };
        println!(
            "  [{:<7}] {}  {}",
            f.layer.label(),
            short(&f.commit.commit_hash),
            f.commit.summary
        );
        println!("            {}  Δ {}", f.commit.file_path, delta);
    };

    if feeders.is_empty() {
        println!("\nFEEDERS (prose predating the code = through-line): none.");
    } else {
        println!("\nFEEDERS — prose predating the code ({}):", feeders.len());
        for f in &feeders {
            print_feeder(f);
        }
    }
    if !descendants.is_empty() {
        println!(
            "\ndescendants — prose referencing it AFTER ({}; not provenance):",
            descendants.len()
        );
        for f in &descendants {
            print_feeder(f);
        }
    }

    if !res.other.is_empty() {
        println!("\nother (config/misc, {}):", res.other.len());
        for c in &res.other {
            println!("  {}  {}  ({})", short(&c.commit_hash), c.summary, c.file_path);
        }
    }
    println!(
        "\n— {} code commit(s); {} prose match(es), {} predating; \
         {} session edit(s), {} predating (through-line).",
        res.code_commit_count,
        res.feeders.len(),
        res.feeders_predating_author,
        res.session_feeders.len(),
        res.session_feeders_predating_author
    );
}

/// Humanize an author↔feeder time-delta in seconds. Negative = feeder predates
/// the code (design-first through-line); positive = doc lag.
fn humanize_delta(secs: i64) -> String {
    let sign = if secs < 0 { "−" } else { "+" };
    let mag = secs.unsigned_abs();
    let days = mag / 86_400;
    let hours = (mag % 86_400) / 3_600;
    let mins = (mag % 3_600) / 60;
    let body = if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {mins}m")
    } else {
        format!("{mins}m")
    };
    let tag = if secs < 0 { " before code" } else { " after code" };
    format!("{sign}{body}{tag}")
}

/// Render a duration in seconds compactly (no sign/tag) — for Δ p50 readouts.
fn humanize_dur(secs: i64) -> String {
    let mag = secs.unsigned_abs();
    let days = mag / 86_400;
    let hours = (mag % 86_400) / 3_600;
    let mins = (mag % 3_600) / 60;
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {mins}m")
    } else {
        format!("{mins}m")
    }
}

fn print_thread_report(r: &thread_metric::ThreadReport) {
    let pct = |num: usize, den: usize| -> String {
        if den == 0 {
            "n/a".to_string()
        } else {
            format!("{:.0}%", 100.0 * num as f64 / den as f64)
        }
    };
    let shape_line = |s: &thread_metric::DeltaShape| -> String {
        let p50 = s.p50_secs.map(humanize_dur).unwrap_or_else(|| "—".to_string());
        format!(
            "<1h:{}  1h–1d:{}  1d–1w:{}  >1w:{}   (n={}, p50 {})",
            s.sub_hour, s.hour_to_day, s.day_to_week, s.week_plus, s.n, p50
        )
    };

    println!("§7 thread measurement — {} terms sampled", r.terms_measured);
    match r.transcript_window {
        Some((lo, hi)) => println!(
            "  transcript window: {} … {}  ({} day span) — the session-feeder reach\n  \
             corpus authors: {} distinct author_email(s){}",
            lo,
            hi,
            (hi - lo) / 86_400,
            r.distinct_author_emails,
            if r.distinct_author_emails <= 1 {
                " — SINGLE-AUTHOR (author stratification degenerate; needs a multi-author repo)"
            } else {
                ""
            }
        ),
        None => println!("  no session transcripts indexed — session-feeder leg empty"),
    }

    println!("\nCOVERAGE (of the {} terms with a code author):", r.with_author);
    println!(
        "  prose feeder predating ......... {} ({})",
        r.prose_predating,
        pct(r.prose_predating, r.with_author)
    );
    println!(
        "  session feeder predating ....... {} ({} of authored; {} of the {} session-POSSIBLE)",
        r.session_predating,
        pct(r.session_predating, r.with_author),
        pct(r.session_predating, r.session_possible),
        r.session_possible
    );
    println!(
        "  either feeder .................. {} ({})",
        r.either_feeder,
        pct(r.either_feeder, r.with_author)
    );
    println!(
        "  NEITHER (code with no recoverable feeder — \"the verbs have no noun\") ... {} ({})",
        r.neither_feeder,
        pct(r.neither_feeder, r.with_author)
    );
    if !r.neither_examples.is_empty() {
        println!("    e.g. {}", r.neither_examples.join(", "));
    }

    println!("\nΔ-DISTRIBUTION SHAPE (predating feeders — the fingerprint, not one number):");
    println!("  prose   {}", shape_line(&r.prose_shape));
    println!("  session {}", shape_line(&r.session_shape));

    println!("\nIDF-TIER DISCOUNT (does the thread hold for rare terms but dissolve into boilerplate?):");
    for t in &r.tiers {
        println!(
            "  {:<36} df {}–{:<4} n={:<3} authored {:<3} feeder {} ({})",
            t.label,
            t.df_range.0,
            t.df_range.1,
            t.n_terms,
            t.with_author,
            t.with_feeder,
            pct(t.with_feeder, t.with_author)
        );
    }

    println!("\nBY LANGUAGE (authoring file extension):");
    for l in &r.langs {
        let p50 = l
            .p50_tightest_delta
            .map(humanize_dur)
            .unwrap_or_else(|| "—".to_string());
        println!(
            "  {:<6} authored {:<3} feeder {:<3} ({:<4}) tightest-Δ p50 {}",
            l.lang,
            l.n_authored,
            l.with_feeder,
            pct(l.with_feeder, l.n_authored),
            p50
        );
    }

    println!(
        "\n— measured in {}ms. Honesty: nibdex/ClearView are single-author + doc-rich \
         (§7.4 unrepresentative); near-zero coverage is a valid finding, not a failure.",
        r.elapsed_ms
    );
}

/// Gear-10 A/B: same term population, two tokenizers. The headline is whether the
/// symbol-aware index lifts feeder coverage (esp. in the rare/mid IDF tiers gear-9
/// found confounded), and at what cost to the boilerplate tiers.
fn print_tokenizer_comparison(cmp: &symbol_index::TokenizerComparison) {
    let baseline = &cmp.baseline;
    let symbol = &cmp.symbol;
    let pct = |num: usize, den: usize| -> String {
        if den == 0 {
            "n/a".to_string()
        } else {
            format!("{:.0}%", 100.0 * num as f64 / den as f64)
        }
    };
    let delta = |a: usize, b: usize| -> String {
        let d = b as i64 - a as i64;
        if d > 0 {
            format!("+{d}")
        } else {
            d.to_string()
        }
    };

    println!(
        "§4#2 SYMBOL-AWARE TOKENIZER A/B — {} terms, symbol shadow = {} diff-hunk rows\n\
         (baseline = shipped unicode61 · symbol = camelCase/PascalCase/digit/acronym split)\n",
        baseline.terms_measured, cmp.shadow_rows
    );

    println!("CODE-AUTHOR / PROVENANCE RECOVERY (the join accuracy — D1's actual value, §6):");
    println!(
        "  with author ......... baseline {}  →  symbol {}  ({})",
        baseline.with_author,
        symbol.with_author,
        delta(baseline.with_author, symbol.with_author)
    );
    println!(
        "  authors gained (none → found) .................. {}",
        cmp.authors_gained
    );
    println!(
        "  authors moved EARLIER (truer anchor; symbol saw\n  an earlier compound-identifier code use) ....... {} (shift p50 {})",
        cmp.authors_moved_earlier,
        cmp.shift_p50_secs
            .map(humanize_dur)
            .unwrap_or_else(|| "—".to_string())
    );

    println!("\nFEEDER COVERAGE (authored terms with a predating prose/session feeder):");
    println!("  NB: feeder coverage is anchor-RELATIVE — a truer (earlier) code anchor reclassifies");
    println!("  intervening prose feeder→descendant, so a drop here is the flip-side of the shift above.");
    println!(
        "  either feeder ....... baseline {} ({})  →  symbol {} ({})  ({})",
        baseline.either_feeder,
        pct(baseline.either_feeder, baseline.with_author),
        symbol.either_feeder,
        pct(symbol.either_feeder, symbol.with_author),
        delta(baseline.either_feeder, symbol.either_feeder)
    );
    println!(
        "  prose predating ..... baseline {}  →  symbol {}  ({})",
        baseline.prose_predating,
        symbol.prose_predating,
        delta(baseline.prose_predating, symbol.prose_predating)
    );

    println!("\nPER-IDF-TIER feeder coverage (the gear-9 confound — does splitting lift the rare/mid tiers?):");
    println!(
        "  {:<36} {:>16}   {:>16}",
        "tier", "baseline", "symbol"
    );
    for (b, s) in baseline.tiers.iter().zip(symbol.tiers.iter()) {
        // Tiers are computed from the same df spread, so they align positionally.
        println!(
            "  {:<36} {:>5}/{:<4} ({:>4})   {:>5}/{:<4} ({:>4})",
            b.label,
            b.with_feeder,
            b.with_author,
            pct(b.with_feeder, b.with_author),
            s.with_feeder,
            s.with_author,
            pct(s.with_feeder, s.with_author)
        );
    }

    println!(
        "\n— baseline {}ms / symbol {}ms. Verdict (§4#2): the symbol tokenizer's payoff is\n\
         PROVENANCE ACCURACY (anchors moved earlier, above), strongest on camelCase code and\n\
         a no-op on snake_case (unicode61 already splits `_`). It does NOT lift the rare-tier\n\
         FEEDER thread (still ~0%) — that gap is structural (no prose was written), not lexical,\n\
         so no tokenizer closes it. Lexical-first is near its feeder-thread ceiling.",
        baseline.elapsed_ms, symbol.elapsed_ms
    );
}
