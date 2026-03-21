use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use url::Url;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
    pub backend: ToolBackend,
    pub base_risk: crate::risk::BaseImpact,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ToolBackend {
    /// Compiled Rust MCP binary (stdio JSON-RPC)
    NativeMcp { binary_path: PathBuf },

    /// Shell command (via Nushell or native)
    Shell { command: String, native: bool },

    /// Python/script execution
    Script {
        runtime: ScriptRuntime,
        source: String,
    },

    /// Direct HTTP API call
    HttpApi {
        endpoint: Url,
        method: HttpMethod,
        auth: Option<AuthConfig>,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ScriptRuntime {
    Python,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Delete,
    Patch,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AuthConfig {
    Bearer { token: String },
    ApiKey { header: String, value: String },
    Basic { username: String, password: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub success: bool,
    pub output: serde_json::Value,
    pub error: Option<String>,
    pub execution_time_ms: u64,
}
