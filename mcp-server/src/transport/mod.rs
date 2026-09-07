//! Transport adapters (transport-spec): one shared MCPod capability layer,
//! multiple transport adapters.
//!
//! - `streamable_http`: rmcp's Streamable HTTP service at `POST /mcp` (primary)
//! - `legacy_sse`: thin 2024-11-05 HTTP+SSE adapter at `GET /sse` +
//!   `POST /messages` (compatibility only)
//!
//! Both build their MCP handler from the same [`crate::server::McpodServer`]
//! factory, so tools, schemas, resources, and instructions are identical.

pub mod legacy_sse;
pub mod streamable_http;
