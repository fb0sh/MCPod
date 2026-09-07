//! MCPod — MCP-controlled development container server.
//!
//! Architecture (docs/plan.md §4): AI Agent → MCP Streamable HTTP → this
//! server (auth / protocol / tools) → Docker container (Debian 13 + mise).

use std::sync::Arc;

use anyhow::{Context, Result};
use tokio_util::sync::CancellationToken;
use tracing::info;

use mcpod::config::Config;
use mcpod::server;

#[tokio::main]
async fn main() -> Result<()> {
    // Logs go to stderr only (§30): stdout stays clean for the HTTP service.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let config = Arc::new(Config::from_env().context("invalid configuration")?);
    match config.token.as_deref() {
        Some(_) => info!("authentication enabled (bearer token required)"),
        None => tracing::warn!(
            "MCPOD_TOKEN is not set: authentication is DISABLED. \
             Anyone who can reach this port can control the container."
        ),
    }
    let cancellation = CancellationToken::new();

    let app = server::build_router(config.clone(), cancellation.clone());
    let addr = std::net::SocketAddr::new(config.host, config.port);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind {addr}"))?;
    info!(%addr, workspace = %config.workspace.display(), "mcpod server started");

    let shutdown_cancellation = cancellation.clone();
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(shutdown_cancellation))
        .await
        .context("server error")?;
    info!("mcpod server stopped");
    Ok(())
}

async fn shutdown_signal(cancellation: CancellationToken) {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut terminate = tokio::signal::unix::signal(
            tokio::signal::unix::SignalKind::terminate(),
        )
        .expect("failed to install SIGTERM handler");
        tokio::select! {
            _ = ctrl_c => {},
            _ = terminate.recv() => {},
        }
    }
    #[cfg(not(unix))]
    {
        let _ = ctrl_c.await;
    }
    info!("shutdown signal received");
    cancellation.cancel();
}
