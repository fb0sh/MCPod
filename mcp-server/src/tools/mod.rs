//! The four MCP tools (pi-spec): read, bash, edit, write.
//!
//! Tool-level failures come back as `CallToolResult::error` text — structured
//! and helpful for agents (§39), never bare "error".

pub mod bash;
pub mod edit;
pub mod read;
pub mod write;

use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::schemars::JsonSchema;
use serde::{Deserialize, Serialize};



#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadParams {
    /// File path, relative to the workspace root or absolute inside it.
    /// Supports text files and images.
    pub path: String,
    /// 1-based line number to start reading from. Defaults to 1.
    pub offset: Option<u32>,
    /// Maximum number of lines to return. Defaults to 2000.
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BashParams {
    /// Shell command to execute via `/bin/bash -c`. The working directory is
    /// the workspace root. There are no command restrictions — the container
    /// is the security boundary.
    pub command: String,
    /// Optional timeout in seconds. There is no default timeout: without it
    /// a command may run indefinitely. Must be a finite number greater than
    /// zero.
    pub timeout: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct EditEntry {
    /// Exact text in the current file to replace. Must match exactly one
    /// unique, non-overlapping region of the original file.
    pub old_text: String,
    /// Replacement text.
    pub new_text: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct EditParams {
    /// File path, relative to the workspace root or absolute inside it.
    pub path: String,
    /// One or more replacements. Each oldText is matched against the original
    /// file contents (before any replacement is applied); all edits are
    /// applied atomically in a single call.
    pub edits: Vec<EditEntry>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WriteParams {
    /// File path, relative to the workspace root or absolute inside it.
    /// Parent directories are created as needed.
    pub path: String,
    /// The complete new file content.
    pub content: String,
}

/// Success result carrying a single text block.
pub(crate) fn text_result(text: String) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(text)])
}

/// Append a status note after existing content, separated by a blank line
/// (or as the whole content when nothing precedes it).
pub(crate) fn append_note(text: &mut String, note: String) {
    if text.trim().is_empty() {
        *text = note;
    } else {
        while text.ends_with('\n') || text.ends_with('\r') {
            text.pop();
        }
        text.push_str("\n\n");
        text.push_str(&note);
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use crate::config::Config;

    pub fn config(workspace: &std::path::Path) -> Config {
        Config {
            token: Some("t".into()),
            host: "127.0.0.1".parse().unwrap(),
            port: 0,
            workspace: workspace.canonicalize().unwrap(),
            allowed_hosts: vec![],
            allowed_origins: vec![],
            sse_session_ttl: std::time::Duration::from_secs(1800),
            sse_keepalive: std::time::Duration::from_secs(15),
        }
    }
}
