use async_trait::async_trait;

use crate::error::Result;
use crate::tool::{ToolDefinition, ToolResult};

/// Client-side interface for invoking tools.
/// The agent calls tools through this trait; the implementation
/// resolves the appropriate backend (MCP, shell, script, HTTP).
#[async_trait]
pub trait ToolRegistry: Send + Sync {
    /// List all available tools and their schemas.
    async fn list_tools(&self) -> Result<Vec<ToolDefinition>>;

    /// Invoke a tool by name with the given arguments.
    async fn call_tool(
        &self,
        name: &str,
        args: serde_json::Value,
    ) -> Result<ToolResult>;
}

/// Lifecycle management for tool backend processes (MCPs, etc).
#[async_trait]
pub trait ToolProcessManager: Send + Sync {
    async fn start(&self, tool_name: &str) -> Result<()>;
    async fn stop(&self, tool_name: &str) -> Result<()>;
    async fn is_running(&self, tool_name: &str) -> bool;
}
