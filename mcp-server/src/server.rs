//! MCP server: tool handlers, resource handlers, initialize instructions,
//! and the axum router that mounts the MCP endpoint next to `/health`.

use std::sync::Arc;
use std::time::Instant;

use axum::http::StatusCode;
use axum::middleware::{from_fn, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CacheScope, CallToolResult, ContentBlock, Implementation, ListResourcesResult, ListToolsResult,
    PaginatedRequestParams, ProtocolVersion, ReadResourceRequestParams, ReadResourceResponse,
    ReadResourceResult, ResourceContents, ResultType, ServerCapabilities, ServerInfo,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler};
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::auth;
use crate::config::Config;
use crate::tools::text_result;
use crate::environment;
use crate::instructions;
use crate::tools;
use crate::transport::legacy_sse::LegacySseManager;
use crate::transport::{legacy_sse, streamable_http};

pub struct McpodServer {
    tool_router: ToolRouter<Self>,
    config: Arc<Config>,
}

impl McpodServer {
    pub fn new(config: Arc<Config>) -> Self {
        Self { tool_router: Self::tool_router(), config }
    }
}

/// Tool order follows pi-spec §1: read, bash, edit, write. Descriptions are
/// the pi-spec §40 texts so agents get the same affordances as Pi.
#[tool_router]
impl McpodServer {
    #[tool(
        description = "Read the contents of a file. Supports text files and images.\nText output is limited to the first 2000 lines or 50KB,\nwhichever is reached first. Use offset/limit to continue reading\nlarge files.",
        output_schema = rmcp::handler::server::tool::schema_for_type::<tools::ReadOutput>(),
    )]
    async fn read(&self, params: Parameters<tools::ReadParams>) -> Result<CallToolResult, McpError> {
        let params = params.0;
        let config = self.config.clone();
        let output = match run_tool("read", params.path.clone(), async move {
            tools::read::run(&config, &params.path, params.offset, params.limit).await
        })
        .await
        {
            Ok(output) => output,
            Err(message) => {
                let McpError { message, .. } = message;
                return Ok(tool_error(message.to_string()));
            }
        };
        // Images ride along as an MCP image block next to the structured text.
        if output.image_mime_type.is_some() {
            let resolved = crate::fs::path::resolve_in_workspace(&self.config.workspace, &output.path)
                .map_err(|e| McpError::invalid_params(e, None))?;
            let bytes = tokio::fs::read(&resolved)
                .await
                .map_err(|e| McpError::internal_error(format!("failed to re-read image: {e}"), None))?;
            let (encoded, mime) = tools::read::encode_image_for_content(&output.path, &bytes)
                .map_err(|e| McpError::internal_error(e, None))?;
            let mut result = CallToolResult::success(vec![
                ContentBlock::text(output.content.clone()),
                ContentBlock::image(encoded, mime),
            ]);
            result.structured_content =
                Some(serde_json::to_value(&output).expect("ReadOutput serializes"));
            return Ok(result);
        }
        let mut result = text_result(output.content.clone());
        result.structured_content =
            Some(serde_json::to_value(&output).expect("ReadOutput serializes"));
        Ok(result)
    }

    #[tool(
        description = "Execute a bash command in the current working directory.\nOutput is limited to the last 2000 lines or 50KB.\nWhen truncated, the complete output is saved to a temporary file.\nOptionally provide a timeout in seconds. There is no default timeout.",
        output_schema = rmcp::handler::server::tool::schema_for_type::<tools::BashOutput>(),
    )]
    async fn bash(&self, params: Parameters<tools::BashParams>) -> Result<CallToolResult, McpError> {
        let params = params.0;
        let timeout = match tools::bash::validate_timeout(params.timeout) {
            Ok(timeout) => timeout,
            Err(message) => return Ok(CallToolResult::error(vec![ContentBlock::text(message)])),
        };
        let config = self.config.clone();
        let output = match run_tool("bash", params.command.clone(), async move {
            tools::bash::run(&config.workspace, &params.command, timeout).await
        })
        .await
        {
            Ok(output) => output,
            Err(message) => {
                let McpError { message, .. } = message;
                return Ok(tool_error(message.to_string()));
            }
        };
        let is_error = output.exit_code != 0 || output.timed_out == Some(true);
        let mut result = if is_error {
            CallToolResult::error(vec![ContentBlock::text(output.output.clone())])
        } else {
            CallToolResult::success(vec![ContentBlock::text(output.output.clone())])
        };
        result.structured_content =
            Some(serde_json::to_value(&output).expect("BashOutput serializes"));
        Ok(result)
    }

    #[tool(
        description = "Edit one file using targeted text replacements.\nEvery edits[].oldText must identify one unique, non-overlapping\nregion of the original file. Multiple disjoint edits can be\nperformed in one call.",
        output_schema = rmcp::handler::server::tool::schema_for_type::<tools::EditOutput>(),
    )]
    async fn edit(&self, params: Parameters<tools::EditParams>) -> Result<CallToolResult, McpError> {
        let params = params.0;
        let config = self.config.clone();
        let output = match run_tool("edit", params.path.clone(), async move {
            tools::edit::run(&config, &params).await
        })
        .await
        {
            Ok(output) => output,
            Err(message) => {
                let McpError { message, .. } = message;
                return Ok(tool_error(message.to_string()));
            }
        };
        let mut result = text_result(format!(
            "Successfully replaced {} block(s) in {}.",
            output.replacements, output.path
        ));
        result.structured_content =
            Some(serde_json::to_value(&output).expect("EditOutput serializes"));
        Ok(result)
    }

    #[tool(
        description = "Write content to a file. Creates the file if it does not exist,\noverwrites it if it does, and automatically creates parent\ndirectories.",
        output_schema = rmcp::handler::server::tool::schema_for_type::<tools::WriteOutput>(),
    )]
    async fn write(&self, params: Parameters<tools::WriteParams>) -> Result<CallToolResult, McpError> {
        let params = params.0;
        let config = self.config.clone();
        let output = match run_tool("write", params.path.clone(), async move {
            tools::write::run(&config, &params).await
        })
        .await
        {
            Ok(output) => output,
            Err(message) => {
                let McpError { message, .. } = message;
                return Ok(tool_error(message.to_string()));
            }
        };
        let mut result = text_result(format!(
            "Successfully wrote {} bytes to {}",
            output.bytes, output.path
        ));
        result.structured_content =
            Some(serde_json::to_value(&output).expect("WriteOutput serializes"));
        Ok(result)
    }
}

/// Shared wrapper: emit §30 tool logs and map tool outcomes to
/// agent-friendly results (§39). Tool failures are results with
/// `isError: true`, not protocol errors, so the message reaches the agent.
/// Convert a tool-level error message into an isError result (§39).
fn tool_error(message: String) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(message)])
}

async fn run_tool<T, F>(tool: &str, subject: String, run: F) -> Result<T, McpError>
where
    T: serde::Serialize,
    F: std::future::Future<Output = Result<T, String>>,
{
    let started = Instant::now();
    tracing::info!(tool, subject = %log_clip(&subject), "tool called");
    match run.await {
        Ok(result) => {
            tracing::info!(
                tool,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "tool completed"
            );
            Ok(result)
        }
        Err(message) => {
            tracing::info!(
                tool,
                error = %message,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "tool failed"
            );
            // Tool-level failure: an isError result (not a protocol error) so
            // the message reaches the agent's client (§39).
            Err(McpError::internal_error(message, None))
        }
    }
}

fn log_clip(value: &str) -> String {
    const MAX: usize = 300;
    if value.len() <= MAX {
        value.to_string()
    } else {
        let mut cut = MAX;
        while !value.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}…", &value[..cut])
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for McpodServer {
    /// ToolRouter::list_all sorts alphabetically; expose tools in the
    /// pi-spec §1 order (read, bash, edit, write) instead.
    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let supports_cache_hints = context
            .protocol_version()
            .is_some_and(|version| version >= ProtocolVersion::V_2026_07_28);
        const ORDER: [&str; 4] = ["read", "bash", "edit", "write"];
        let mut tools = self.tool_router.list_all();
        tools.sort_by_key(|tool| ORDER.iter().position(|name| *name == tool.name).unwrap_or(usize::MAX));
        Ok(ListToolsResult {
            result_type: Some(ResultType::COMPLETE),
            tools,
            meta: None,
            next_cursor: None,
            ttl_ms: supports_cache_hints.then_some(0),
            cache_scope: supports_cache_hints.then_some(CacheScope::Public),
        })
    }

    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
        )
        .with_server_info(Implementation::new("mcpod", env!("CARGO_PKG_VERSION")))
        .with_instructions(instructions::text(&self.config.workspace))
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        Ok(ListResourcesResult::with_all_items(vec![environment::resource()]))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, McpError> {
        if request.uri != environment::RESOURCE_URI {
            return Err(McpError::resource_not_found(
                format!("unknown resource: {}", request.uri),
                None,
            ));
        }
        let info = environment::info(&self.config).await;
        let text = serde_json::to_string_pretty(&info).map_err(|e| {
            McpError::internal_error(format!("failed to serialize environment: {e}"), None)
        })?;
        Ok(ReadResourceResponse::Complete(ReadResourceResult::new(vec![
            ResourceContents::text(text, environment::RESOURCE_URI)
                .with_mime_type("application/json"),
        ])))
    }
}

/// Build the full HTTP surface (transport-spec §23):
/// - `GET  /health`   — unauthenticated liveness probe (§11)
/// - `POST /mcp`      — Streamable HTTP MCP endpoint (primary, §2)
/// - `GET  /sse`      — Legacy HTTP+SSE endpoint (compatibility, §3)
/// - `POST /messages` — Legacy SSE message ingestion (compatibility, §3)
///
/// Both transports build handlers from the same [`McpodServer`] factory
/// (§25), share the same bearer/origin policy (§18, §21), and are routed by
/// endpoint only — never by sniffing (§24).
pub fn build_router(config: Arc<Config>, cancellation: CancellationToken) -> Router {
    let streamable = streamable_http::router(config.clone(), cancellation.clone());

    let legacy_manager = Arc::new(LegacySseManager::new(config.clone()));
    let legacy = legacy_sse::router(legacy_manager, cancellation);

    Router::new()
        .route("/health", get(health))
        .nest("/mcp", streamable)
        .merge(legacy)
        .layer(from_fn_with_state(config.clone(), auth::require_allowed_origin))
        .layer(from_fn(auth::log_requests))
}

async fn health() -> Response {
    (StatusCode::OK, Json(json!({"status": "ok"}))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_clip_truncates_long_subjects() {
        assert_eq!(log_clip("short"), "short");
        let long = "x".repeat(400);
        assert_eq!(log_clip(&long).chars().count(), 301); // 300 chars + ellipsis
    }
}
