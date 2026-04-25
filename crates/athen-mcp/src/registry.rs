//! Runtime registry of enabled MCP servers.
//!
//! Owns the spawned child processes (one per enabled server) and routes
//! tool calls through the rmcp client. Spawning is lazy — the first tool
//! call (or first `list_tools`) for a given mcp triggers the spawn.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use rmcp::model::CallToolRequestParams;
use rmcp::service::{RoleClient, RunningService, ServiceExt};
use rmcp::transport::TokioChildProcess;
use tokio::process::Command;
use tokio::sync::Mutex;

use athen_core::error::{AthenError, Result};
use athen_core::traits::mcp::{
    McpCallOutcome, McpCatalogEntry, McpClient, McpSource, McpToolDescriptor,
};

use crate::catalog;

/// What an enabled MCP looks like to the registry: the catalog entry plus
/// the user-supplied configuration blob.
#[derive(Debug, Clone)]
pub struct EnabledEntry {
    pub entry: McpCatalogEntry,
    pub config: serde_json::Value,
}

/// Lazy-spawned client connection to one MCP server.
struct LiveClient {
    service: RunningService<RoleClient, ()>,
}

/// Resolves a `Bundled { binary_name }` source to an absolute path:
///   1. Next to the current executable (production install layout).
///   2. In `target/debug/` or `target/release/` (development layout).
///   3. Bare name, leaving PATH lookup to the OS.
fn resolve_bundled_binary(binary_name: &str) -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join(binary_name);
            if candidate.exists() {
                return candidate;
            }
            // dev fallback: same target/ directory.
            let dev_candidate = dir.join(if cfg!(windows) {
                format!("{binary_name}.exe")
            } else {
                binary_name.to_string()
            });
            if dev_candidate.exists() {
                return dev_candidate;
            }
        }
    }
    PathBuf::from(binary_name)
}

/// MCP runtime: holds enabled-state + lazy-spawned client connections.
pub struct McpRegistry {
    enabled: Mutex<HashMap<String, EnabledEntry>>,
    clients: Mutex<HashMap<String, Arc<LiveClient>>>,
}

impl McpRegistry {
    pub fn new() -> Self {
        Self {
            enabled: Mutex::new(HashMap::new()),
            clients: Mutex::new(HashMap::new()),
        }
    }

    /// Replace the enabled set. Drops any live clients whose entry is no
    /// longer present (which kills the underlying child process via
    /// `TokioChildProcess`'s `Drop`).
    pub async fn set_enabled(&self, entries: Vec<EnabledEntry>) {
        let mut map = self.enabled.lock().await;
        map.clear();
        for e in entries {
            map.insert(e.entry.id.clone(), e);
        }
        drop(map);

        // Drop clients for ids that are no longer enabled.
        let enabled_ids: Vec<String> = self.enabled.lock().await.keys().cloned().collect();
        let mut clients = self.clients.lock().await;
        clients.retain(|id, _| enabled_ids.contains(id));
    }

    /// Enable a single mcp by catalog id with the supplied configuration.
    /// If it was already enabled, the configuration is replaced and any live
    /// connection is dropped so the next call re-spawns with the new config.
    pub async fn enable(&self, mcp_id: &str, config: serde_json::Value) -> Result<()> {
        let entry = catalog::lookup(mcp_id)
            .ok_or_else(|| AthenError::Other(format!("unknown MCP id: {mcp_id}")))?;
        self.enabled
            .lock()
            .await
            .insert(mcp_id.to_string(), EnabledEntry { entry, config });
        // Drop any pre-existing client so the next call uses the new config.
        self.clients.lock().await.remove(mcp_id);
        Ok(())
    }

    /// Disable an mcp, dropping its live client (which kills the process).
    pub async fn disable(&self, mcp_id: &str) {
        self.enabled.lock().await.remove(mcp_id);
        self.clients.lock().await.remove(mcp_id);
    }

    pub async fn enabled_ids(&self) -> Vec<String> {
        self.enabled.lock().await.keys().cloned().collect()
    }

    pub async fn enabled_entries(&self) -> Vec<EnabledEntry> {
        self.enabled.lock().await.values().cloned().collect()
    }

    /// Lazily acquire (or spawn) the live client for a given mcp id.
    async fn get_or_spawn(&self, mcp_id: &str) -> Result<Arc<LiveClient>> {
        if let Some(c) = self.clients.lock().await.get(mcp_id).cloned() {
            return Ok(c);
        }

        // Not yet spawned — look up the entry and start the process.
        let entry = self
            .enabled
            .lock()
            .await
            .get(mcp_id)
            .cloned()
            .ok_or_else(|| AthenError::Other(format!("MCP '{mcp_id}' is not enabled")))?;

        let live = Arc::new(spawn_client(&entry).await?);
        self.clients
            .lock()
            .await
            .insert(mcp_id.to_string(), live.clone());
        Ok(live)
    }
}

impl Default for McpRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Build the `Command` line for an enabled entry, then spawn it as an MCP
/// child process and complete the rmcp handshake.
async fn spawn_client(enabled: &EnabledEntry) -> Result<LiveClient> {
    let binary_name = match &enabled.entry.source {
        McpSource::Bundled { binary_name } => binary_name.clone(),
        McpSource::Download { .. } => {
            return Err(AthenError::Other(
                "MCP download source not yet supported".into(),
            ));
        }
    };
    let path = resolve_bundled_binary(&binary_name);

    // Per-entry argument shaping. For now only "files" needs a sandbox root.
    let mut cmd = Command::new(&path);
    if enabled.entry.id == "files" {
        let root = enabled
            .config
            .get("sandbox_root")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AthenError::Other("Files: missing 'sandbox_root' in config".into()))?;
        // Make sure the directory exists before handing it to the binary —
        // mcp-filesystem will refuse to start if the path isn't there.
        if let Err(e) = std::fs::create_dir_all(root) {
            return Err(AthenError::Other(format!(
                "Files: cannot create sandbox root {root}: {e}"
            )));
        }
        cmd.arg(root);
    }

    tracing::info!(
        mcp = enabled.entry.id,
        binary = %path.display(),
        "spawning MCP server"
    );

    let transport = TokioChildProcess::new(cmd).map_err(|e| {
        AthenError::Other(format!(
            "spawn MCP '{}' ({}): {e}",
            enabled.entry.id,
            path.display()
        ))
    })?;

    let service = ()
        .serve(transport)
        .await
        .map_err(|e| AthenError::Other(format!("MCP '{}' handshake: {e}", enabled.entry.id)))?;

    Ok(LiveClient { service })
}

#[async_trait]
impl McpClient for McpRegistry {
    async fn list_tools(&self) -> Result<Vec<McpToolDescriptor>> {
        let ids = self.enabled_ids().await;
        let mut out = Vec::new();
        for id in ids {
            match self.get_or_spawn(&id).await {
                Ok(client) => match client.service.peer().list_all_tools().await {
                    Ok(tools) => {
                        for t in tools {
                            out.push(McpToolDescriptor {
                                mcp_id: id.clone(),
                                name: t.name.to_string(),
                                description: t.description.map(|c| c.to_string()),
                                input_schema: serde_json::to_value(&t.input_schema)
                                    .unwrap_or(serde_json::json!({})),
                            });
                        }
                    }
                    Err(e) => {
                        tracing::warn!(mcp = %id, error = %e, "list_tools failed");
                    }
                },
                Err(e) => {
                    tracing::warn!(mcp = %id, error = %e, "spawn failed");
                }
            }
        }
        Ok(out)
    }

    async fn call_tool(
        &self,
        mcp_id: &str,
        tool: &str,
        args: serde_json::Value,
    ) -> Result<McpCallOutcome> {
        let client = self.get_or_spawn(mcp_id).await?;

        let arguments = match args {
            serde_json::Value::Object(map) => Some(map),
            serde_json::Value::Null => None,
            other => {
                return Err(AthenError::Other(format!(
                    "MCP tool args must be an object, got: {other}"
                )));
            }
        };

        let mut params = CallToolRequestParams::new(tool.to_string());
        if let Some(a) = arguments {
            params = params.with_arguments(a);
        }

        let result = client
            .service
            .peer()
            .call_tool(params)
            .await
            .map_err(|e| AthenError::Other(format!("MCP '{mcp_id}' call_tool '{tool}': {e}")))?;

        let raw = serde_json::to_value(&result.content).unwrap_or(serde_json::json!([]));
        let text = result
            .content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.clone()))
            .collect::<Vec<_>>()
            .join("\n");
        let success = !result.is_error.unwrap_or(false);

        Ok(McpCallOutcome { success, text, raw })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn enable_disable_tracks_state() {
        let reg = McpRegistry::new();
        reg.enable("files", serde_json::json!({"sandbox_root": "/tmp/athen-test"}))
            .await
            .unwrap();
        assert_eq!(reg.enabled_ids().await, vec!["files"]);
        reg.disable("files").await;
        assert!(reg.enabled_ids().await.is_empty());
    }

    #[tokio::test]
    async fn unknown_id_rejected() {
        let reg = McpRegistry::new();
        assert!(reg.enable("nope", serde_json::json!({})).await.is_err());
    }

    #[tokio::test]
    async fn call_tool_when_disabled_errors() {
        let reg = McpRegistry::new();
        let res = reg.call_tool("files", "read_file", serde_json::json!({})).await;
        assert!(res.is_err());
    }

    /// End-to-end: spawn the real `mcp-filesystem` binary (if it has been
    /// built) and exercise list_tools + a write/read round trip. Skipped
    /// silently when the binary isn't on disk.
    #[tokio::test]
    async fn end_to_end_with_real_binary() {
        let candidates = ["target/debug/mcp-filesystem", "target/release/mcp-filesystem"];
        let workspace_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap();
        let bin = candidates
            .iter()
            .map(|c| workspace_root.join(c))
            .find(|p| p.exists());
        let Some(bin_path) = bin else {
            eprintln!("skipping: mcp-filesystem binary not built");
            return;
        };
        // Override resolution by symlinking into the current_exe parent.
        // Simpler: spawn directly using a custom enabled entry whose
        // catalog source path resolves to our binary by name + cwd.
        let tmp = tempfile::tempdir().unwrap();
        let reg = McpRegistry::new();
        // Force absolute path by stuffing it as binary_name; the resolver
        // returns the path unchanged when it doesn't exist next to current_exe.
        // We work around that by symlinking next to the test exe's dir.
        let exe_dir = std::env::current_exe().unwrap().parent().unwrap().to_path_buf();
        let link = exe_dir.join("mcp-filesystem");
        let _ = std::fs::remove_file(&link);
        if std::os::unix::fs::symlink(&bin_path, &link).is_err() {
            eprintln!("skipping: could not create symlink for binary");
            return;
        }
        reg.enable("files", serde_json::json!({"sandbox_root": tmp.path()}))
            .await
            .unwrap();

        let tools = reg.list_tools().await.unwrap();
        assert!(
            tools.iter().any(|t| t.name == "read_file"),
            "expected read_file in tools: {:?}",
            tools.iter().map(|t| &t.name).collect::<Vec<_>>()
        );

        let _ = reg
            .call_tool(
                "files",
                "write_file",
                serde_json::json!({"path": "hello.txt", "contents": "hi"}),
            )
            .await
            .unwrap();
        let read = reg
            .call_tool("files", "read_file", serde_json::json!({"path": "hello.txt"}))
            .await
            .unwrap();
        assert!(read.success);
        assert_eq!(read.text, "hi");

        // Cleanup
        let _ = std::fs::remove_file(&link);
    }
}
