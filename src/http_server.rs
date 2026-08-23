// SPDX-License-Identifier: MIT

//! HTTP transport for MCP + `/healthz` endpoint (D-6.4.2-4).
//!
//! Day 6 commit 6: closes smoke gate 4 (cross-session warmth). The HTTP daemon
//! is the long-lived process that owns the file-watcher per D-6.4.1 — stdio
//! mode skips the watcher (process dies at session end); HTTP mode runs the
//! watcher (daemon stays up).
//!
//! Security posture per D-6.4.3: bind `127.0.0.1` only, no auth at MVP, the
//! authorization model is filesystem access. A future `--bind` flag that
//! opts into `0.0.0.0` would cross the §13 publication checklist gate.
//!
//! This module owns only the HTTP side. The watcher is composed at the CLI
//! layer (`main.rs::Serve` arm) so this module stays unit-testable without
//! spawning real filesystem watches.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use axum::{Json, Router, extract::State, http::StatusCode, response::IntoResponse, routing::get};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use serde::Serialize;
use sqlx::SqlitePool;
use tokio_util::sync::CancellationToken;

use crate::calibration::CalibrationModel;
use crate::mcp::{IndexerCounts, NibdexServer, OrphanCounts, Stages, run_check};
use crate::metrics_sink::MetricsSink;

/// Subset of `check()` returned by `GET /healthz` per D-6.4.3.
///
/// Excluded from full `check()`: `schema_version`, `perf_p50_ms`,
/// `perf_p95_ms`, `file_watcher`, `extractors_last_run_ms`. Health probes
/// (`launchd` / `systemd`) want a small payload — full forensic state stays
/// behind the MCP `check()` tool.
#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub daemon_uptime_s: i64,
    pub indexer: IndexerCounts,
    pub orphans: OrphanCounts,
    pub shallow_repos: Vec<String>,
    /// Build provenance of the running daemon — `crate_version` + `git_sha` so
    /// a `curl /healthz` confirms WHICH build is live (the deploy signal). The
    /// full block (with `git_describe`/`commit_time`) is in the `check()` tool.
    pub crate_version: String,
    pub git_sha: String,
    /// True when no corpus holds a single row. `serve` performs no initial
    /// scan, so a daemon on a fresh database stays empty until a watched file
    /// changes; without this a probe cannot tell "never indexed" from
    /// "indexed and quiet" — both are all-zero counts behind a 200.
    /// gnuphie-labs/nibdex#15.
    pub index_empty: bool,
}

#[derive(Clone)]
struct HealthState {
    pool: SqlitePool,
    started_at: Instant,
}

/// Build the router with MCP at `/mcp` + `/healthz`. Exposed `pub(crate)`
/// so unit tests can drive it via `tower::ServiceExt::oneshot` without
/// binding a real port.
///
/// `metrics_sink` is `None` when telemetry is disabled (`--metrics-sink off`);
/// `Some(_)` for the `stdout` and `jsonl:<path>` variants. Day 7 commit 1
/// plumbed it through; commit 2 wired emission inside the 7 `tool.*`
/// handlers in `mcp.rs`.
///
/// `calibration` is `None` when `calibration.toml` was missing at startup;
/// `Some(_)` when the §8.4 Layer-2 model loaded cleanly. Day 8 commit 1
/// plumbs it through; commit 2 wires the 6 additive Layer-2 fields +
/// `cost_ledger_events` insert + `check()` rollup.
pub(crate) fn build_router(
    pool: SqlitePool,
    started_at: Instant,
    metrics_sink: Option<Arc<MetricsSink>>,
    calibration: Option<Arc<CalibrationModel>>,
) -> Router {
    let mcp_pool = pool.clone();
    let mcp_sink = metrics_sink.clone();
    let mcp_calibration = calibration.clone();
    let mcp_service: StreamableHttpService<NibdexServer, LocalSessionManager> =
        StreamableHttpService::new(
            // `started_at` is `Copy` — every per-session server gets the SAME
            // process-start instant, so MCP `check().daemon_uptime_s` reports
            // true process uptime (matching `/healthz`) instead of resetting
            // on each new MCP session. D-10 finding, session #651.
            move || {
                Ok(NibdexServer::new(
                    mcp_pool.clone(),
                    started_at,
                    mcp_sink.clone(),
                    mcp_calibration.clone(),
                ))
            },
            Arc::new(LocalSessionManager::default()),
            StreamableHttpServerConfig::default(),
        );
    let state = HealthState { pool, started_at };
    Router::new()
        .route("/healthz", get(healthz))
        .with_state(state)
        .nest_service("/mcp", mcp_service)
}

async fn healthz(State(state): State<HealthState>) -> impl IntoResponse {
    let uptime_s = state.started_at.elapsed().as_secs() as i64;
    // `/healthz` doesn't emit metrics — pass a throwaway Stages. Also
    // passes `None` for calibration: probe is liveness, not a cost
    // reporting surface (HealthResponse strips cost_savings anyway).
    match run_check(&state.pool, uptime_s, None, &mut Stages::default()).await {
        Ok(check) => (
            StatusCode::OK,
            Json(HealthResponse {
                daemon_uptime_s: check.daemon_uptime_s,
                index_empty: check.indexer.is_empty(),
                indexer: check.indexer,
                orphans: check.orphans,
                shallow_repos: check.shallow_repos,
                crate_version: check.build.crate_version.clone(),
                git_sha: check.build.git_sha.clone(),
            }),
        )
            .into_response(),
        Err(e) => {
            eprintln!("[nibdex serve] /healthz error: {e:#}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("healthz error: {e}"),
            )
                .into_response()
        }
    }
}

/// Run the HTTP daemon (MCP + `/healthz`) until `shutdown_rx` resolves.
/// Binds `127.0.0.1` only — D-6.4.3 enforces loopback-only at MVP.
pub async fn serve(
    pool: SqlitePool,
    bind: SocketAddr,
    shutdown_rx: tokio::sync::oneshot::Receiver<()>,
    metrics_sink: Option<Arc<MetricsSink>>,
    calibration: Option<Arc<CalibrationModel>>,
) -> Result<()> {
    if !bind.ip().is_loopback() {
        anyhow::bail!(
            "nibdex serve: bind address {bind} is not loopback. D-6.4.3 \
             requires 127.0.0.1 at MVP."
        );
    }
    let started_at = Instant::now();
    let router = build_router(pool, started_at, metrics_sink, calibration);
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("binding {bind}"))?;
    let local_addr = listener.local_addr()?;
    eprintln!(
        "nibdex serve — HTTP MCP at http://{local_addr}/mcp, health at http://{local_addr}/healthz; ctrl-c to exit."
    );

    let cancel = CancellationToken::new();
    let cancel_for_serve = cancel.clone();
    tokio::spawn(async move {
        let _ = shutdown_rx.await;
        cancel.cancel();
    });

    axum::serve(listener, router)
        .with_graceful_shutdown(async move { cancel_for_serve.cancelled().await })
        .await?;
    eprintln!("nibdex serve — exited.");
    Ok(())
}

// =====================================================================================
// Tests — D-6.4 unit gates: bind-and-respond + /healthz schema.
// =====================================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use std::net::{IpAddr, Ipv4Addr};
    use tokio::sync::oneshot;
    use tower::ServiceExt;

    async fn fresh_pool() -> SqlitePool {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    /// `/healthz` returns 200 + JSON with the D-6.4.3 subset keys.
    /// Drives the router in-process via `tower::ServiceExt::oneshot` —
    /// no port bind, no watcher.
    #[tokio::test]
    async fn healthz_returns_200_with_expected_keys() -> Result<()> {
        let pool = fresh_pool().await;
        let router = build_router(pool, Instant::now(), None, None);

        let response = router
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            content_type.starts_with("application/json"),
            "expected application/json, got {content_type}"
        );

        let body_bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let payload: serde_json::Value = serde_json::from_slice(&body_bytes)?;

        // D-6.4.3 contract: daemon_uptime_s + indexer + orphans + shallow_repos.
        // Excluded from full check(): schema_version, perf_*_ms, file_watcher,
        // extractors_last_run_ms — assert their absence so a future widening
        // doesn't drift silently.
        assert!(
            payload.get("daemon_uptime_s").is_some(),
            "missing daemon_uptime_s"
        );
        assert!(payload.get("indexer").is_some(), "missing indexer");
        assert!(payload.get("orphans").is_some(), "missing orphans");
        assert!(
            payload.get("shallow_repos").is_some(),
            "missing shallow_repos"
        );
        // Build provenance mirrored into the probe (deploy signal).
        assert!(
            payload.get("crate_version").is_some(),
            "missing crate_version"
        );
        assert!(payload.get("git_sha").is_some(), "missing git_sha");
        assert!(
            payload.get("schema_version").is_none(),
            "schema_version should not leak into /healthz"
        );
        assert!(
            payload.get("perf_p50_ms").is_none(),
            "perf_p50_ms should not leak into /healthz"
        );
        assert!(
            payload.get("file_watcher").is_none(),
            "file_watcher should not leak into /healthz"
        );

        // Indexer subobject shape — empty DB has zero rows but the keys exist.
        let indexer = &payload["indexer"];
        assert!(indexer.get("session_entries").is_some());
        assert!(indexer.get("commit_entries").is_some());
        assert!(indexer.get("search_index_total").is_some());
        Ok(())
    }

    /// `serve` binds to an ephemeral loopback port and responds to a real
    /// HTTP request. Verifies the full bind + accept + dispatch path that
    /// `tower::oneshot` skips.
    #[tokio::test]
    async fn serve_binds_loopback_and_responds_then_shuts_down() -> Result<()> {
        let pool = fresh_pool().await;
        let bind: SocketAddr = (IpAddr::V4(Ipv4Addr::LOCALHOST), 0).into();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();

        // Bind first so the test can discover the assigned port.
        let listener = tokio::net::TcpListener::bind(bind).await?;
        let local_addr = listener.local_addr()?;

        let router = build_router(pool, Instant::now(), None, None);
        let cancel = CancellationToken::new();
        let cancel_for_serve = cancel.clone();
        tokio::spawn(async move {
            let _ = shutdown_rx.await;
            cancel.cancel();
        });

        let server_handle = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(async move { cancel_for_serve.cancelled().await })
                .await
        });

        // GET /healthz over a real TCP connection.
        let client = reqwest::Client::builder().no_proxy().build()?;
        let response = client
            .get(format!("http://{local_addr}/healthz"))
            .send()
            .await?;
        assert_eq!(response.status(), 200);
        let payload: serde_json::Value = response.json().await?;
        assert!(payload.get("indexer").is_some());
        // `fresh_pool` is an empty database, which is exactly the state a
        // `serve` on a new workspace leaves behind. The probe must be able to
        // SEE that, not just report 200 over all-zero counts.
        // gnuphie-labs/nibdex#15.
        assert_eq!(
            payload.get("index_empty").and_then(|v| v.as_bool()),
            Some(true),
            "an empty index must be visible on /healthz, got: {payload}"
        );

        // Trigger graceful shutdown and confirm the server exits within the
        // drain window. 2s ceiling — axum's graceful shutdown waits for
        // in-flight requests; our test issued one + finished it.
        shutdown_tx.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), server_handle)
            .await
            .expect("server did not exit within 2s of shutdown signal")
            .unwrap()
            .unwrap();
        Ok(())
    }

    /// `serve` rejects non-loopback bind addresses per D-6.4.3.
    #[tokio::test]
    async fn serve_rejects_non_loopback_bind() -> Result<()> {
        let pool = fresh_pool().await;
        let bind: SocketAddr = (IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 0).into();
        let (_tx, rx) = oneshot::channel();
        let err = serve(pool, bind, rx, None, None).await.unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("loopback"),
            "expected loopback rejection, got: {msg}"
        );
        Ok(())
    }
}
