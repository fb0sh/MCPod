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

const HELP: &str = "\
MCPod — MCP-controlled development server.

Run `mcpod` from the directory the agent should work in; all configuration
comes from environment variables (no CLI options):

  MCPOD_TOKEN            Bearer token for /mcp, /sse and /messages.
                         Unset/empty disables authentication (dangerous).
  MCPOD_PORT             Listen port (default 3000).
  MCPOD_HOST             Bind address (default 127.0.0.1; the Docker image
                         sets 0.0.0.0 and relies on the port mapping).
  MCPOD_WORKSPACE        Workspace root the file tools are jailed in.
                         Default: the current directory (the Docker image
                         presets /workspace).
  MCPOD_ALLOWED_HOSTS    Comma-separated Host-header allowlist
                         (default: unrestricted).
  MCPOD_ALLOWED_ORIGINS  Browser Origin allowlist (default: localhost on
                         any port).
  MCPOD_SSE_SESSION_TTL  Disconnected legacy SSE session TTL (default 30m).
  MCPOD_SSE_KEEPALIVE    SSE keepalive interval (default 15s).

Example (host mode):

  MCPOD_TOKEN=\"$(openssl rand -hex 32)\" ./mcpod

Docs: https://github.com/fb0sh/MCPod
";

#[tokio::main]
async fn main() -> Result<()> {
    // Minimal CLI surface: --help/--version only. Everything else is env
    // config; unknown args are ignored to keep container entrypoints that
    // pass extra "$@" harmless.
    match std::env::args().nth(1).as_deref() {
        Some("-h") | Some("--help") => {
            print!("{HELP}");
            return Ok(());
        }
        Some("-V") | Some("--version") => {
            println!("mcpod {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        _ => {}
    }

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
