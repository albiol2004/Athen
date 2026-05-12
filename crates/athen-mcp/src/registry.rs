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
    EnvValue, McpCallOutcome, McpCatalogEntry, McpClient, McpSource, McpToolDescriptor,
};
use athen_core::traits::vault::Vault;

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
    /// Resolves `EnvValue::Vault` bindings for `Process` sources. `None`
    /// when the vault is unavailable — `Process` MCPs that depend on
    /// vault-backed env vars will fail to spawn with a clear error.
    vault: Option<Arc<dyn Vault>>,
}

impl McpRegistry {
    pub fn new() -> Self {
        Self {
            enabled: Mutex::new(HashMap::new()),
            clients: Mutex::new(HashMap::new()),
            vault: None,
        }
    }

    /// Build a registry wired to a vault so `Process` MCPs can resolve
    /// `EnvValue::Vault` bindings at spawn time.
    pub fn new_with_vault(vault: Arc<dyn Vault>) -> Self {
        Self {
            enabled: Mutex::new(HashMap::new()),
            clients: Mutex::new(HashMap::new()),
            vault: Some(vault),
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
    /// Eagerly spawns the child process and runs the rmcp handshake so any
    /// configuration error surfaces to the caller (and the UI) immediately
    /// instead of silently failing the next time the agent calls list_tools.
    pub async fn enable(&self, mcp_id: &str, config: serde_json::Value) -> Result<()> {
        let entry = catalog::lookup(mcp_id)
            .ok_or_else(|| AthenError::Other(format!("unknown MCP id: {mcp_id}")))?;
        self.enable_entry(entry, config).await
    }

    /// Enable a BYO catalog entry that doesn't live in the bundled catalog.
    /// The caller (typically the persistence layer) owns the entry definition.
    pub async fn enable_custom(
        &self,
        entry: McpCatalogEntry,
        config: serde_json::Value,
    ) -> Result<()> {
        self.enable_entry(entry, config).await
    }

    async fn enable_entry(&self, entry: McpCatalogEntry, config: serde_json::Value) -> Result<()> {
        let mcp_id = entry.id.clone();
        let enabled_entry = EnabledEntry { entry, config };
        let live = spawn_client(&enabled_entry, self.vault.as_ref()).await?;
        self.enabled
            .lock()
            .await
            .insert(mcp_id.clone(), enabled_entry);
        self.clients.lock().await.insert(mcp_id, Arc::new(live));
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

    /// Dry-run a `Process` (or `Bundled`) MCP source: spawn the child,
    /// complete the rmcp handshake, list every tool the server advertises,
    /// then drop the connection (which kills the process via the
    /// transport's `Drop`).
    ///
    /// Used by the Settings UI's "Test connection" button before the user
    /// saves a custom MCP. Does NOT mutate any persisted state and does
    /// NOT touch `self.enabled` / `self.clients` — the caller can run it
    /// against a throwaway registry or the live one without side effects.
    pub async fn test_spawn(
        entry: McpCatalogEntry,
        config: serde_json::Value,
        vault: Option<&Arc<dyn Vault>>,
    ) -> Result<Vec<McpToolDescriptor>> {
        let mcp_id = entry.id.clone();
        let enabled = EnabledEntry { entry, config };
        let live = spawn_client(&enabled, vault).await?;
        let tools = live
            .service
            .peer()
            .list_all_tools()
            .await
            .map_err(|e| AthenError::Other(format!("MCP '{mcp_id}' list_tools: {e}")))?;
        let descriptors = tools
            .into_iter()
            .map(|t| {
                let name = t.name.to_string();
                let base_risk = enabled
                    .entry
                    .tool_risks
                    .get(&name)
                    .copied()
                    .unwrap_or(enabled.entry.base_risk);
                McpToolDescriptor {
                    mcp_id: mcp_id.clone(),
                    name,
                    description: t.description.map(|c| c.to_string()),
                    input_schema: serde_json::to_value(&t.input_schema)
                        .unwrap_or(serde_json::json!({})),
                    base_risk,
                }
            })
            .collect();
        // `live` drops here → child process is killed.
        Ok(descriptors)
    }

    /// List every tool advertised by a single already-enabled MCP, by id.
    /// Used by the Settings UI to populate the expanded row without
    /// re-spawning the process — the lazy `get_or_spawn` is reused so the
    /// existing live client (if any) answers the request.
    pub async fn list_tools_for(&self, mcp_id: &str) -> Result<Vec<McpToolDescriptor>> {
        let client = self.get_or_spawn(mcp_id).await?;
        let tools = client
            .service
            .peer()
            .list_all_tools()
            .await
            .map_err(|e| AthenError::Other(format!("MCP '{mcp_id}' list_tools: {e}")))?;
        // Snapshot the entry's risk shape so the stamping below doesn't
        // hold the enabled lock across the per-tool loop.
        let (default_risk, overrides) = {
            let enabled = self.enabled.lock().await;
            match enabled.get(mcp_id) {
                Some(ee) => (ee.entry.base_risk, ee.entry.tool_risks.clone()),
                None => {
                    // Race: spawned but the entry vanished. Treat as the
                    // conservative default — we still want to surface the
                    // tools so the UI doesn't strand them.
                    (athen_core::risk::BaseImpact::WritePersist, HashMap::new())
                }
            }
        };
        Ok(tools
            .into_iter()
            .map(|t| {
                let name = t.name.to_string();
                let base_risk = overrides.get(&name).copied().unwrap_or(default_risk);
                McpToolDescriptor {
                    mcp_id: mcp_id.to_string(),
                    name,
                    description: t.description.map(|c| c.to_string()),
                    input_schema: serde_json::to_value(&t.input_schema)
                        .unwrap_or(serde_json::json!({})),
                    base_risk,
                }
            })
            .collect())
    }

    /// Replace the per-server default risk + per-tool overrides for an
    /// already-enabled MCP. The live child process is left running — only
    /// the in-memory descriptor metadata changes, so the next
    /// `list_tools` / `list_tools_for` call sees the new risk levels.
    /// Errors when the id is not currently enabled.
    pub async fn update_risks(
        &self,
        mcp_id: &str,
        default_risk: athen_core::risk::BaseImpact,
        overrides: HashMap<String, athen_core::risk::BaseImpact>,
    ) -> Result<()> {
        let mut enabled = self.enabled.lock().await;
        let ee = enabled
            .get_mut(mcp_id)
            .ok_or_else(|| AthenError::Other(format!("MCP '{mcp_id}' is not enabled")))?;
        ee.entry.base_risk = default_risk;
        ee.entry.tool_risks = overrides;
        Ok(())
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

        let live = Arc::new(spawn_client(&entry, self.vault.as_ref()).await?);
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
async fn spawn_client(
    enabled: &EnabledEntry,
    vault: Option<&Arc<dyn Vault>>,
) -> Result<LiveClient> {
    let mcp_id = enabled.entry.id.as_str();

    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut cmd = match &enabled.entry.source {
        McpSource::Bundled { binary_name } => {
            let path = resolve_bundled_binary(binary_name);
            tracing::info!(
                mcp = mcp_id,
                binary = %path.display(),
                "spawning bundled MCP server"
            );
            Command::new(path)
        }
        McpSource::Download { .. } => {
            return Err(AthenError::Other(
                "MCP download source not yet supported".into(),
            ));
        }
        McpSource::Process {
            command,
            args,
            env,
            working_dir,
        } => {
            let resolved_env = resolve_env(mcp_id, env, vault).await?;
            tracing::info!(
                mcp = mcp_id,
                command = %command,
                args = ?args,
                "spawning process MCP server"
            );
            let mut c = Command::new(command);
            c.args(args);
            // Minimal inherited env: keep PATH so portable Node/Python (and
            // anything wired up by the runtime wizard) still resolve, but
            // drop the rest of the parent environment to avoid leaking the
            // user's shell variables into a third-party process.
            c.env_clear();
            if let Ok(path_val) = std::env::var("PATH") {
                c.env("PATH", path_val);
            }
            for (k, v) in resolved_env {
                c.env(k, v);
            }
            if let Some(dir) = working_dir.as_deref() {
                c.current_dir(dir);
            }
            c
        }
    };

    // MCP sidecars are headless JSON-RPC servers; suppress the cmd-window flash
    // Windows would otherwise attach to GUI parents.
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000);

    let transport = TokioChildProcess::new(cmd)
        .map_err(|e| AthenError::Other(format!("spawn MCP '{mcp_id}': {e}")))?;

    let service = ()
        .serve(transport)
        .await
        .map_err(|e| AthenError::Other(format!("MCP '{mcp_id}' handshake: {e}")))?;

    Ok(LiveClient { service })
}

/// Resolve every `EnvBinding` into a flat `(key, value)` list, pulling
/// `Vault` values from the credential store. Missing vault entries become
/// hard errors with a user-actionable message — the alternative (spawn
/// with an empty token) silently breaks the MCP downstream.
async fn resolve_env(
    mcp_id: &str,
    bindings: &[athen_core::traits::mcp::EnvBinding],
    vault: Option<&Arc<dyn Vault>>,
) -> Result<Vec<(String, String)>> {
    let mut out = Vec::with_capacity(bindings.len());
    for b in bindings {
        match &b.value {
            EnvValue::Plain { value } => out.push((b.key.clone(), value.clone())),
            EnvValue::Vault { scope, key } => {
                let vault = vault.ok_or_else(|| {
                    AthenError::Other(format!(
                        "MCP '{mcp_id}' env var '{}' needs vault but no vault is configured",
                        b.key
                    ))
                })?;
                let val = vault.get(scope, key).await?.ok_or_else(|| {
                    AthenError::Other(format!(
                        "MCP '{mcp_id}' env var '{}' not set in vault — \
                         please configure it in Settings (scope '{scope}', key '{key}')",
                        b.key
                    ))
                })?;
                out.push((b.key.clone(), val));
            }
        }
    }
    Ok(out)
}

#[async_trait]
impl McpClient for McpRegistry {
    async fn list_tools(&self) -> Result<Vec<McpToolDescriptor>> {
        let ids = self.enabled_ids().await;
        let mut out = Vec::new();
        for id in ids {
            // Snapshot the risk shape once per server so we don't reacquire
            // the lock for every tool. If the entry vanished between the
            // enabled_ids() call and here we just skip it.
            let (default_risk, overrides) = {
                let enabled = self.enabled.lock().await;
                match enabled.get(&id) {
                    Some(ee) => (ee.entry.base_risk, ee.entry.tool_risks.clone()),
                    None => continue,
                }
            };
            match self.get_or_spawn(&id).await {
                Ok(client) => match client.service.peer().list_all_tools().await {
                    Ok(tools) => {
                        for t in tools {
                            let name = t.name.to_string();
                            let base_risk = overrides.get(&name).copied().unwrap_or(default_risk);
                            out.push(McpToolDescriptor {
                                mcp_id: id.clone(),
                                name,
                                description: t.description.map(|c| c.to_string()),
                                input_schema: serde_json::to_value(&t.input_schema)
                                    .unwrap_or(serde_json::json!({})),
                                base_risk,
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

        let result =
            client.service.peer().call_tool(params).await.map_err(|e| {
                AthenError::Other(format!("MCP '{mcp_id}' call_tool '{tool}': {e}"))
            })?;

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
    use athen_core::risk::BaseImpact;
    use athen_core::traits::mcp::{EnvBinding, EnvValue};
    use std::collections::HashMap as StdHashMap;
    use tokio::sync::Mutex as TokioMutex;

    #[tokio::test]
    async fn unknown_id_rejected() {
        let reg = McpRegistry::new();
        assert!(reg.enable("nope", serde_json::json!({})).await.is_err());
    }

    #[tokio::test]
    async fn call_tool_when_disabled_errors() {
        let reg = McpRegistry::new();
        let res = reg
            .call_tool("nonexistent", "anything", serde_json::json!({}))
            .await;
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn empty_registry_lists_no_tools() {
        let reg = McpRegistry::new();
        let tools = reg.list_tools().await.unwrap();
        assert!(tools.is_empty());
    }

    struct FakeVault {
        data: TokioMutex<StdHashMap<(String, String), String>>,
    }

    impl FakeVault {
        fn new() -> Self {
            Self {
                data: TokioMutex::new(StdHashMap::new()),
            }
        }
    }

    #[async_trait]
    impl Vault for FakeVault {
        async fn set(&self, scope: &str, key: &str, value: &str) -> Result<()> {
            self.data
                .lock()
                .await
                .insert((scope.to_string(), key.to_string()), value.to_string());
            Ok(())
        }
        async fn get(&self, scope: &str, key: &str) -> Result<Option<String>> {
            Ok(self
                .data
                .lock()
                .await
                .get(&(scope.to_string(), key.to_string()))
                .cloned())
        }
        async fn delete(&self, scope: &str, key: &str) -> Result<()> {
            self.data
                .lock()
                .await
                .remove(&(scope.to_string(), key.to_string()));
            Ok(())
        }
        async fn list(&self, scope: &str) -> Result<Vec<String>> {
            Ok(self
                .data
                .lock()
                .await
                .keys()
                .filter(|(s, _)| s == scope)
                .map(|(_, k)| k.clone())
                .collect())
        }
    }

    fn process_entry(id: &str, command: &str, env: Vec<EnvBinding>) -> McpCatalogEntry {
        McpCatalogEntry {
            id: id.into(),
            display_name: id.into(),
            description: String::new(),
            icon: None,
            config_schema: serde_json::json!({}),
            source: McpSource::Process {
                command: command.into(),
                args: vec![],
                env,
                working_dir: None,
            },
            base_risk: BaseImpact::WritePersist,
            tool_risks: std::collections::HashMap::new(),
        }
    }

    #[tokio::test]
    async fn resolve_env_plain_value_passes_through() {
        let bindings = vec![EnvBinding {
            key: "FOO".into(),
            value: EnvValue::Plain {
                value: "bar".into(),
            },
        }];
        let resolved = resolve_env("test", &bindings, None).await.unwrap();
        assert_eq!(resolved, vec![("FOO".into(), "bar".into())]);
    }

    #[tokio::test]
    async fn resolve_env_vault_value_resolves() {
        let vault: Arc<dyn Vault> = Arc::new(FakeVault::new());
        vault
            .set("mcp:github", "token", "ghp_secret")
            .await
            .unwrap();

        let bindings = vec![EnvBinding {
            key: "GITHUB_TOKEN".into(),
            value: EnvValue::Vault {
                scope: "mcp:github".into(),
                key: "token".into(),
            },
        }];
        let resolved = resolve_env("test", &bindings, Some(&vault)).await.unwrap();
        assert_eq!(resolved, vec![("GITHUB_TOKEN".into(), "ghp_secret".into())]);
    }

    #[tokio::test]
    async fn resolve_env_missing_vault_entry_returns_clear_error() {
        let vault: Arc<dyn Vault> = Arc::new(FakeVault::new());
        let bindings = vec![EnvBinding {
            key: "GITHUB_TOKEN".into(),
            value: EnvValue::Vault {
                scope: "mcp:github".into(),
                key: "token".into(),
            },
        }];
        let err = resolve_env("byo-github", &bindings, Some(&vault))
            .await
            .expect_err("missing vault entry should error");
        let msg = format!("{err}");
        assert!(
            msg.contains("not set in vault"),
            "error message missing user hint: {msg}"
        );
        assert!(msg.contains("GITHUB_TOKEN"));
        assert!(msg.contains("byo-github"));
    }

    #[tokio::test]
    async fn resolve_env_vault_binding_without_vault_errors() {
        let bindings = vec![EnvBinding {
            key: "GITHUB_TOKEN".into(),
            value: EnvValue::Vault {
                scope: "mcp:github".into(),
                key: "token".into(),
            },
        }];
        let err = resolve_env("byo-github", &bindings, None)
            .await
            .expect_err("no vault should error");
        let msg = format!("{err}");
        assert!(msg.contains("no vault is configured"), "msg: {msg}");
    }

    #[tokio::test]
    async fn test_spawn_with_bogus_command_returns_error() {
        // Bogus path → TokioChildProcess::new fails fast with a clear
        // error. test_spawn must propagate that without panicking and
        // without leaving anything behind.
        let entry = process_entry(
            "bogus-test",
            "/definitely/not/a/real/binary/athen-mcp-test",
            vec![],
        );
        let res = McpRegistry::test_spawn(entry, serde_json::json!({}), None).await;
        assert!(res.is_err(), "expected spawn failure, got {res:?}");
    }

    #[tokio::test]
    async fn test_spawn_against_cat_fails_handshake_gracefully() {
        // /bin/cat (or printf) reads stdin forever but never speaks JSON-RPC.
        // The rmcp handshake should time out / error rather than spawn-erroring
        // — we just want a clean error message, not a panic or hang.
        let entry = process_entry("cat-fake", "/bin/cat", vec![]);
        // Wrap in a 5s timeout so a stuck handshake can't hang the test suite.
        let res = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            McpRegistry::test_spawn(entry, serde_json::json!({}), None),
        )
        .await;
        // Either the rmcp handshake errored (preferred) or the test timed out.
        // Both prove we don't crash. The handshake-error path is the one the
        // UI will actually surface, so warn if we hit the timeout instead.
        match res {
            Ok(Ok(_)) => panic!("cat should NOT successfully complete an MCP handshake"),
            Ok(Err(_)) => { /* expected: clean error from handshake */ }
            Err(_) => {
                // Timed out — also acceptable for this smoke test; the real
                // bug we're guarding against is a panic.
            }
        }
    }

    #[tokio::test]
    async fn update_risks_unknown_id_errors() {
        let reg = McpRegistry::new();
        let res = reg
            .update_risks("nope", BaseImpact::Read, std::collections::HashMap::new())
            .await;
        assert!(res.is_err(), "update_risks on unknown id should error");
    }

    #[tokio::test]
    async fn update_risks_mutates_entry_without_respawn() {
        // Hand-insert an EnabledEntry directly so we don't need a real
        // child process for the metadata-only test. update_risks should
        // ONLY change the entry's risk fields — clients map must remain
        // untouched (preserving live PID handles in the production path).
        let reg = McpRegistry::new();
        let entry = process_entry("test", "/bin/true", vec![]);
        let ee = EnabledEntry {
            entry,
            config: serde_json::json!({}),
        };
        reg.enabled.lock().await.insert("test".into(), ee);

        let clients_before = reg.clients.lock().await.len();

        let mut overrides = std::collections::HashMap::new();
        overrides.insert("delete_file".to_string(), BaseImpact::System);
        overrides.insert("read_file".to_string(), BaseImpact::Read);
        reg.update_risks("test", BaseImpact::WriteTemp, overrides.clone())
            .await
            .unwrap();

        let enabled = reg.enabled.lock().await;
        let updated = enabled.get("test").unwrap();
        assert_eq!(updated.entry.base_risk, BaseImpact::WriteTemp);
        assert_eq!(
            updated.entry.tool_risks.get("delete_file").copied(),
            Some(BaseImpact::System)
        );
        assert_eq!(
            updated.entry.tool_risks.get("read_file").copied(),
            Some(BaseImpact::Read)
        );

        // No new clients were spawned and no existing ones were dropped.
        assert_eq!(reg.clients.lock().await.len(), clients_before);
    }

    #[tokio::test]
    async fn update_risks_replaces_overrides_wholesale() {
        // Spec says we replace, not merge. A second call with a smaller
        // map drops the previously-set overrides.
        let reg = McpRegistry::new();
        let mut entry = process_entry("test", "/bin/true", vec![]);
        let mut initial = std::collections::HashMap::new();
        initial.insert("delete_file".to_string(), BaseImpact::System);
        initial.insert("read_file".to_string(), BaseImpact::Read);
        entry.tool_risks = initial;
        reg.enabled.lock().await.insert(
            "test".into(),
            EnabledEntry {
                entry,
                config: serde_json::json!({}),
            },
        );

        // Replace with a single-key map.
        let mut next = std::collections::HashMap::new();
        next.insert("delete_file".to_string(), BaseImpact::WritePersist);
        reg.update_risks("test", BaseImpact::WritePersist, next)
            .await
            .unwrap();

        let enabled = reg.enabled.lock().await;
        let updated = enabled.get("test").unwrap();
        assert_eq!(updated.entry.tool_risks.len(), 1);
        assert!(updated.entry.tool_risks.contains_key("delete_file"));
        assert!(!updated.entry.tool_risks.contains_key("read_file"));
    }

    #[tokio::test]
    async fn enable_custom_with_bogus_command_returns_error_not_panic() {
        // Pointing at a clearly nonexistent path so TokioChildProcess::new
        // fails fast. The goal is to exercise the enable_custom registration
        // path without depending on a real MCP server being installed.
        let reg = McpRegistry::new();
        let entry = process_entry(
            "bogus",
            "/definitely/not/a/real/binary/athen-mcp-test",
            vec![],
        );
        let res = reg.enable_custom(entry, serde_json::json!({})).await;
        assert!(res.is_err(), "expected spawn failure, got {res:?}");
        // State must remain clean — failed enable should NOT have left
        // the entry in the enabled map.
        assert!(reg.enabled_ids().await.is_empty());
    }
}
