//! Port for the MCP (Model Context Protocol) subsystem.
//!
//! `MCP servers` are external processes Athen spawns to expose extra tools to
//! the agent (filesystem access, calendar APIs, etc.). The catalog is a
//! curated, branded list (e.g. "Files", "Calendar"); the registry tracks
//! which entries the user has enabled and lazily spawns the underlying
//! processes on demand.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::risk::BaseImpact;

/// One catalog entry — a "branded" MCP available for the user to enable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpCatalogEntry {
    /// Stable identifier (e.g. "files", "calendar").
    pub id: String,
    /// User-facing name shown in the UI (e.g. "Files").
    pub display_name: String,
    /// Short description shown in the catalog.
    pub description: String,
    /// Optional emoji or icon name for the UI.
    pub icon: Option<String>,
    /// JSON Schema describing the per-instance configuration the user must
    /// supply when enabling this MCP. May be an empty object.
    pub config_schema: serde_json::Value,
    /// How the MCP is delivered. Bundled binaries ship with Athen; downloads
    /// are fetched on first enable (not yet implemented).
    pub source: McpSource,
    /// Default base risk for tool calls coming from this MCP.
    pub base_risk: BaseImpact,
}

/// Where the MCP server binary comes from.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum McpSource {
    /// Ships in the Athen install directory. `binary_name` is the file name
    /// looked up next to the main executable (or in PATH as a fallback).
    Bundled { binary_name: String },
    /// Reserved for future use — fetch a binary from a URL on first enable.
    Download { url: String, binary_name: String },
}

/// Tool exposed by an MCP server, exactly as advertised by the server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolDescriptor {
    /// MCP entry id this tool belongs to (e.g. "files").
    pub mcp_id: String,
    /// Bare tool name as the server advertised it (e.g. "read_file").
    pub name: String,
    /// Optional human description.
    pub description: Option<String>,
    /// JSON Schema for the tool's input.
    pub input_schema: serde_json::Value,
}

/// Result of an MCP tool call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpCallOutcome {
    /// Whether the server reported success.
    pub success: bool,
    /// Concatenated text content blocks from the server's response.
    pub text: String,
    /// The full structured content array, in case the caller wants more
    /// than just the joined text.
    pub raw: serde_json::Value,
}

/// Port for the MCP runtime: list available tools across all enabled
/// servers and dispatch tool calls. Implementations are responsible for
/// lazy-spawning child processes and routing by tool name.
#[async_trait]
pub trait McpClient: Send + Sync {
    /// List every tool exposed by every enabled MCP server.
    async fn list_tools(&self) -> Result<Vec<McpToolDescriptor>>;

    /// Invoke a tool on the MCP server identified by `mcp_id`.
    async fn call_tool(
        &self,
        mcp_id: &str,
        tool: &str,
        args: serde_json::Value,
    ) -> Result<McpCallOutcome>;
}
