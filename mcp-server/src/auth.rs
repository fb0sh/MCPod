//! HTTP middleware: bearer-token authentication (§10, transport-spec §18) and
//! Origin validation (transport-spec §21), plus request logging with a
//! transport label (§32).

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::config::Config;

/// Reject requests to MCP endpoints without a valid
/// `Authorization: Bearer <token>` header. When no token is configured,
/// authentication is disabled and every request passes. The token value is
/// never logged (§19).
pub async fn require_bearer(
    State(config): State<Arc<Config>>,
    request: Request,
    next: Next,
) -> Response {
    if let Some(expected) = config.token.as_deref() {
        let provided = request
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "));
        let authorized = provided
            .is_some_and(|token| constant_time_eq(token.as_bytes(), expected.as_bytes()));
        if !authorized {
            tracing::warn!(
                transport = transport_of(request.uri().path()),
                path = %request.uri().path(),
                "authentication failed"
            );
            return (
                StatusCode::UNAUTHORIZED,
                [(header::WWW_AUTHENTICATE, "Bearer")],
                "Unauthorized",
            )
                .into_response();
        }
    }
    next.run(request).await
}

/// Reject requests whose `Origin` header is not on the allowlist (§21).
/// Missing `Origin` (non-browser clients) always passes.
pub async fn require_allowed_origin(
    State(config): State<Arc<Config>>,
    request: Request,
    next: Next,
) -> Response {
    let origin = request
        .headers()
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok());
    if let Some(origin) = origin.filter(|origin| !config.origin_allowed(origin)) {
        tracing::warn!(transport = transport_of(request.uri().path()), %origin, "origin rejected");
        return (StatusCode::FORBIDDEN, "Forbidden: origin not allowed").into_response();
    }
    next.run(request).await
}

/// Log every request arrival and completion (§30, §32) with a transport label.
pub async fn log_requests(request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let transport = transport_of(&path);
    let start = std::time::Instant::now();
    tracing::info!(transport, %method, %path, "request received");
    let response = next.run(request).await;
    tracing::info!(
        transport,
        %method,
        %path,
        status = %response.status(),
        elapsed_ms = start.elapsed().as_millis() as u64,
        "request completed"
    );
    response
}

/// Transport label derived from the endpoint (§24): the URL path alone
/// decides which transport a request belongs to.
pub fn transport_of(path: &str) -> &'static str {
    match path {
        "/mcp" => "streamable_http",
        "/sse" | "/messages" => "legacy_sse",
        _ => "http",
    }
}

/// Comparison whose runtime does not depend on where the first difference
/// sits, so timing cannot leak token content.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |diff, (x, y)| diff | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::{constant_time_eq, transport_of};

    #[test]
    fn matches_equal_and_rejects_different() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(!constant_time_eq(b"", b"a"));
    }

    #[test]
    fn transport_labels_follow_endpoint() {
        assert_eq!(transport_of("/mcp"), "streamable_http");
        assert_eq!(transport_of("/sse"), "legacy_sse");
        assert_eq!(transport_of("/messages"), "legacy_sse");
        assert_eq!(transport_of("/health"), "http");
    }
}
