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
    /// User-supplied stdio MCP launched from an arbitrary command line
    /// (BYO custom MCP, schema-compatible with Claude Desktop / Cursor
    /// `mcpServers` entries).
    Process {
        /// Executable name resolved against PATH, or an absolute path.
        command: String,
        /// CLI arguments passed verbatim to the child process.
        #[serde(default)]
        args: Vec<String>,
        /// Environment variables exposed to the child. Each binding is
        /// resolved at spawn time; vault-backed values never appear in
        /// persisted config.
        #[serde(default)]
        env: Vec<EnvBinding>,
        /// Optional working directory for the child process.
        #[serde(default)]
        working_dir: Option<String>,
    },
}

/// A single environment-variable binding for a `Process` MCP source.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvBinding {
    /// Variable name exposed inside the child process (e.g. `GITHUB_TOKEN`).
    pub key: String,
    /// How the value is resolved at spawn time.
    pub value: EnvValue,
}

/// Resolution strategy for an env-binding value.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EnvValue {
    /// Inline literal value. Persisted in plaintext — caller is responsible
    /// for routing secrets through `Vault` instead.
    Plain { value: String },
    /// Read at spawn time from the credential vault.
    Vault { scope: String, key: String },
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::risk::BaseImpact;

    #[test]
    fn process_source_roundtrip_with_vault_env() {
        let entry = McpCatalogEntry {
            id: "github".into(),
            display_name: "GitHub".into(),
            description: "BYO GitHub MCP".into(),
            icon: None,
            config_schema: serde_json::json!({}),
            source: McpSource::Process {
                command: "npx".into(),
                args: vec!["-y".into(), "@modelcontextprotocol/server-github".into()],
                env: vec![
                    EnvBinding {
                        key: "GITHUB_TOKEN".into(),
                        value: EnvValue::Vault {
                            scope: "mcp:github".into(),
                            key: "token".into(),
                        },
                    },
                    EnvBinding {
                        key: "GITHUB_HOST".into(),
                        value: EnvValue::Plain {
                            value: "github.com".into(),
                        },
                    },
                ],
                working_dir: None,
            },
            base_risk: BaseImpact::WritePersist,
        };

        let json = serde_json::to_value(&entry).unwrap();
        // Schema lock so we don't accidentally diverge from Claude Desktop's
        // `mcpServers` shape — `kind: "process"` + `command` + `args` + `env`
        // are load-bearing.
        assert_eq!(json["source"]["kind"], "process");
        assert_eq!(json["source"]["command"], "npx");
        assert_eq!(json["source"]["args"][0], "-y");
        assert_eq!(json["source"]["env"][0]["key"], "GITHUB_TOKEN");
        assert_eq!(json["source"]["env"][0]["value"]["kind"], "vault");
        assert_eq!(json["source"]["env"][0]["value"]["scope"], "mcp:github");
        assert_eq!(json["source"]["env"][1]["value"]["kind"], "plain");
        assert_eq!(json["source"]["env"][1]["value"]["value"], "github.com");

        let back: McpCatalogEntry = serde_json::from_value(json).unwrap();
        match back.source {
            McpSource::Process {
                command,
                args,
                env,
                working_dir,
            } => {
                assert_eq!(command, "npx");
                assert_eq!(args.len(), 2);
                assert_eq!(env.len(), 2);
                assert_eq!(env[0].key, "GITHUB_TOKEN");
                assert!(working_dir.is_none());
                match &env[0].value {
                    EnvValue::Vault { scope, key } => {
                        assert_eq!(scope, "mcp:github");
                        assert_eq!(key, "token");
                    }
                    EnvValue::Plain { .. } => panic!("expected vault"),
                }
                match &env[1].value {
                    EnvValue::Plain { value } => assert_eq!(value, "github.com"),
                    EnvValue::Vault { .. } => panic!("expected plain"),
                }
            }
            _ => panic!("expected Process source"),
        }
    }

    #[test]
    fn bundled_source_still_serializes_unchanged() {
        let src = McpSource::Bundled {
            binary_name: "athen-mcp-files".into(),
        };
        let json = serde_json::to_value(&src).unwrap();
        assert_eq!(json["kind"], "bundled");
        assert_eq!(json["binary_name"], "athen-mcp-files");
    }
}
