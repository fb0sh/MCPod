//! Streamable HTTP transport (primary, transport-spec §2, §7): rmcp's
//! `StreamableHttpService` mounted at `/mcp`, stateless (§9).

use std::sync::Arc;

use axum::middleware::from_fn_with_state;
use axum::Router;
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use tokio_util::sync::CancellationToken;

use crate::auth;
use crate::config::Config;
use crate::server::McpodServer;

/// The `/mcp` endpoint: rmcp Streamable HTTP behind bearer auth and Origin
/// validation. Stateless — every request stands alone (§9).
pub fn router(config: Arc<Config>, cancellation: CancellationToken) -> Router {
    let mut mcp_config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_sse_keep_alive(None)
        .with_cancellation_token(cancellation);
    if !config.allowed_hosts.is_empty() {
        mcp_config = mcp_config.with_allowed_hosts(config.allowed_hosts.clone());
    }

    // Shared server factory (§25): every request gets a fresh McpodServer
    // built from the same Arc<Config>-backed state.
    let factory_config = config.clone();
    let service = StreamableHttpService::new(
        move || Ok(McpodServer::new(factory_config.clone())),
        Arc::new(NeverSessionManager::default()),
        mcp_config,
    );

    Router::new()
        .fallback_service(service)
        .layer(from_fn_with_state(config, auth::require_bearer))
}
