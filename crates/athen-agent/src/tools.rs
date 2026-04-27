//! Built-in tool registry backed by shell execution and filesystem operations.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use serde_json::json;
use tokio::sync::Mutex;

use athen_core::contact::TrustLevel;
use athen_core::error::{AthenError, Result};
use athen_core::paths;
use athen_core::risk::{BaseImpact, DataSensitivity, RiskContext, RiskLevel};
use athen_core::sandbox::{SandboxLevel, SandboxProfile};
use athen_core::tool::{ToolBackend, ToolDefinition, ToolResult};
use athen_core::traits::shell::ShellExecutor;
use athen_core::traits::tool::ToolRegistry;
use athen_risk::rules::RuleEngine;
use athen_sandbox::UnifiedSandbox;
use athen_shell::Shell;

/// Provider used by the shell tool to discover the per-arc set of writable
/// directories that should be exposed to the sandbox in addition to the
/// hardcoded default writable set (`/tmp` plus the data dir).
///
/// The `app_tools` layer wires this against the `GrantStore`. When no
/// provider is set the shell still works — the agent just won't be able to
/// write outside the default safe locations.
#[async_trait]
pub trait ShellExtraWritableProvider: Send + Sync {
    async fn extra_writable_paths(&self) -> Vec<PathBuf>;
}

/// A [`ToolRegistry`] that provides built-in tools for shell execution,
/// filesystem operations, and in-session key-value memory,
/// backed by [`athen_shell::Shell`].
pub struct ShellToolRegistry {
    shell: Shell,
    memory: Arc<Mutex<HashMap<String, String>>>,
    sandbox: Option<UnifiedSandbox>,
    rule_engine: RuleEngine,
    extra_writable: Option<Arc<dyn ShellExtraWritableProvider>>,
}

impl ShellToolRegistry {
    /// Create a new registry, auto-detecting the available shell backend
    /// and sandbox capabilities.
    pub async fn new() -> Self {
        let sandbox = match UnifiedSandbox::new().await {
            Ok(sb) => {
                let caps = sb.capabilities();
                if caps.bubblewrap || caps.landlock || caps.macos_sandbox || caps.windows_sandbox {
                    tracing::info!("Sandbox available for shell tool execution");
                    Some(sb)
                } else {
                    tracing::info!(
                        "No OS-native sandbox capabilities detected, \
                         shell commands will run unsandboxed"
                    );
                    None
                }
            }
            Err(e) => {
                tracing::warn!("Failed to initialize sandbox, proceeding without: {e}");
                None
            }
        };

        Self {
            shell: Shell::new().await,
            memory: Arc::new(Mutex::new(HashMap::new())),
            sandbox,
            rule_engine: RuleEngine::new(),
            extra_writable: None,
        }
    }

    /// Inject a provider that supplies additional writable paths for the
    /// shell sandbox, typically derived from the active arc's grants.
    pub fn with_extra_writable(
        mut self,
        provider: Arc<dyn ShellExtraWritableProvider>,
    ) -> Self {
        self.extra_writable = Some(provider);
        self
    }

    /// Build the JSON Schema for the `shell_execute` tool parameters.
    fn shell_execute_schema() -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The shell command to execute"
                }
            },
            "required": ["command"]
        })
    }

    /// Build the JSON Schema for the `read_file` tool parameters.
    fn read_file_schema() -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Absolute path to the file to read"
                }
            },
            "required": ["path"]
        })
    }

    /// Build the JSON Schema for the `write_file` tool parameters.
    fn write_file_schema() -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Absolute path to the file to write"
                },
                "content": {
                    "type": "string",
                    "description": "Content to write to the file"
                }
            },
            "required": ["path", "content"]
        })
    }

    /// Build the JSON Schema for the `list_directory` tool parameters.
    fn list_directory_schema() -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Absolute path to the directory to list"
                }
            },
            "required": ["path"]
        })
    }

    /// Execute a shell command and return the result.
    ///
    /// When a sandbox is available, the command is executed inside an
    /// OS-native sandbox with a read-only filesystem profile. If sandbox
    /// execution fails, the method falls back to unsandboxed shell
    /// execution so that functionality is never broken.
    async fn do_shell_execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let command = args
            .get("command")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AthenError::Other("missing 'command' parameter".to_string()))?;

        tracing::info!(tool = "shell_execute", command, "Executing shell command");

        // Pre-execution risk check: evaluate the ACTUAL command (not user's
        // natural language) through the rule engine. This catches dangerous
        // commands like `rm -rf` regardless of what language the user spoke.
        let risk_ctx = RiskContext {
            trust_level: TrustLevel::AuthUser,
            data_sensitivity: DataSensitivity::Plain,
            llm_confidence: Some(1.0),
            accumulated_risk: 0,
        };
        if let Some(score) = self.rule_engine.evaluate(command, &risk_ctx) {
            if score.level == RiskLevel::Danger || score.level == RiskLevel::Critical {
                tracing::warn!(
                    tool = "shell_execute",
                    command,
                    risk_score = score.total,
                    risk_level = ?score.level,
                    "Command blocked by risk evaluation"
                );
                return Ok(ToolResult {
                    success: false,
                    output: json!({
                        "error": "Command blocked by safety system",
                        "reason": format!(
                            "This command was classified as {:?} risk (score: {:.0}). \
                             It cannot be executed without explicit user approval.",
                            score.level, score.total
                        ),
                        "command": command,
                    }),
                    error: Some(format!(
                        "Blocked: {:?} risk command (score {:.0})",
                        score.level, score.total
                    )),
                    execution_time_ms: 0,
                });
            }
        }

        let start = Instant::now();

        // Try sandboxed execution first, fall back to unsandboxed shell.
        let (stdout, stderr, exit_code) = if let Some(ref sandbox) = self.sandbox {
            // Default writable set: /tmp, the Athen data dir, and cwd.
            // $HOME is intentionally NOT included by default — explicit
            // grants via the GrantStore push entries through `extra_writable`.
            let mut allowed: Vec<PathBuf> = vec![PathBuf::from("/tmp")];
            if let Some(data) = paths::athen_data_dir() {
                allowed.push(data);
            }
            if let Ok(cwd) = std::env::current_dir() {
                allowed.push(cwd);
            }
            if let Some(provider) = self.extra_writable.as_ref() {
                for p in provider.extra_writable_paths().await {
                    if !paths::is_system_path(&p) {
                        allowed.push(p);
                    }
                }
            }
            let level = SandboxLevel::OsNative {
                profile: SandboxProfile::RestrictedWrite {
                    allowed_paths: allowed,
                },
            };
            match sandbox
                .execute_sandboxed("sh", &["-c", command], &level)
                .await
            {
                Ok(output) => {
                    // Detect sandbox infrastructure failures (e.g. bwrap can't
                    // create namespaces on restricted CI runners). If stderr
                    // contains sandbox-specific errors, fall back to unsandboxed.
                    let is_sandbox_failure = output.exit_code != 0
                        && (output.stderr.contains("bwrap:")
                            || output.stderr.contains("sandbox-exec:")
                            || output.stderr.contains("creating new namespace"));
                    if is_sandbox_failure {
                        tracing::warn!(
                            tool = "shell_execute",
                            stderr = %output.stderr.trim(),
                            "Sandbox infrastructure failed, falling back to unsandboxed shell"
                        );
                        let output = self.shell.execute(command).await?;
                        (output.stdout, output.stderr, output.exit_code)
                    } else {
                        tracing::debug!(tool = "shell_execute", "Command executed inside sandbox");
                        (output.stdout, output.stderr, output.exit_code)
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        tool = "shell_execute",
                        error = %e,
                        "Sandbox execution failed, falling back to unsandboxed shell"
                    );
                    let output = self.shell.execute(command).await?;
                    (output.stdout, output.stderr, output.exit_code)
                }
            }
        } else {
            tracing::trace!(
                tool = "shell_execute",
                "No sandbox available, executing unsandboxed"
            );
            let output = self.shell.execute(command).await?;
            (output.stdout, output.stderr, output.exit_code)
        };

        let elapsed_ms = start.elapsed().as_millis() as u64;
        let success = exit_code == 0;
        let result_output = json!({
            "stdout": stdout,
            "stderr": stderr,
            "exit_code": exit_code,
        });

        Ok(ToolResult {
            success,
            output: result_output,
            error: if success {
                None
            } else {
                Some(format!(
                    "exit code {}: {}",
                    exit_code,
                    stderr.trim()
                ))
            },
            execution_time_ms: elapsed_ms,
        })
    }

    /// Read a file and return its contents.
    async fn do_read_file(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AthenError::Other("missing 'path' parameter".to_string()))?;

        tracing::info!(tool = "read_file", path, "Reading file");
        tracing::trace!(tool = "read_file", "Filesystem tools use tokio::fs directly (unsandboxed)");

        let start = Instant::now();
        match tokio::fs::read_to_string(path).await {
            Ok(content) => {
                let elapsed_ms = start.elapsed().as_millis() as u64;
                Ok(ToolResult {
                    success: true,
                    output: json!({ "content": content }),
                    error: None,
                    execution_time_ms: elapsed_ms,
                })
            }
            Err(e) => {
                let elapsed_ms = start.elapsed().as_millis() as u64;
                Ok(ToolResult {
                    success: false,
                    output: json!({ "error": e.to_string() }),
                    error: Some(e.to_string()),
                    execution_time_ms: elapsed_ms,
                })
            }
        }
    }

    /// Write content to a file.
    async fn do_write_file(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AthenError::Other("missing 'path' parameter".to_string()))?;

        let content = args
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AthenError::Other("missing 'content' parameter".to_string()))?;

        tracing::info!(tool = "write_file", path, "Writing file");
        tracing::trace!(tool = "write_file", "Filesystem tools use tokio::fs directly (unsandboxed)");

        let start = Instant::now();
        match tokio::fs::write(path, content).await {
            Ok(()) => {
                let elapsed_ms = start.elapsed().as_millis() as u64;
                Ok(ToolResult {
                    success: true,
                    output: json!({ "path": path, "bytes_written": content.len() }),
                    error: None,
                    execution_time_ms: elapsed_ms,
                })
            }
            Err(e) => {
                let elapsed_ms = start.elapsed().as_millis() as u64;
                Ok(ToolResult {
                    success: false,
                    output: json!({ "error": e.to_string() }),
                    error: Some(e.to_string()),
                    execution_time_ms: elapsed_ms,
                })
            }
        }
    }

    /// Build the JSON Schema for the `memory_store` tool parameters.
    fn memory_store_schema() -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "string",
                    "description": "The key to store the value under"
                },
                "value": {
                    "type": "string",
                    "description": "The value to store"
                }
            },
            "required": ["key", "value"]
        })
    }

    /// Build the JSON Schema for the `memory_recall` tool parameters.
    fn memory_recall_schema() -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "string",
                    "description": "The key to recall. If omitted, returns all stored keys."
                }
            },
            "required": []
        })
    }

    /// Store a key-value pair in in-session memory.
    async fn do_memory_store(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let key = args
            .get("key")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AthenError::Other("missing 'key' parameter".to_string()))?;

        let value = args
            .get("value")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AthenError::Other("missing 'value' parameter".to_string()))?;

        tracing::info!(tool = "memory_store", key, "Storing value in memory");

        let start = Instant::now();
        self.memory
            .lock()
            .await
            .insert(key.to_string(), value.to_string());
        let elapsed_ms = start.elapsed().as_millis() as u64;

        Ok(ToolResult {
            success: true,
            output: json!({ "stored": key }),
            error: None,
            execution_time_ms: elapsed_ms,
        })
    }

    /// Recall a value by key, or list all keys if no key is provided.
    async fn do_memory_recall(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let key = args.get("key").and_then(|v| v.as_str());

        tracing::info!(tool = "memory_recall", ?key, "Recalling from memory");

        let start = Instant::now();
        let memory = self.memory.lock().await;

        let output = match key {
            Some(k) => match memory.get(k) {
                Some(v) => json!({ "key": k, "value": v }),
                None => json!({ "key": k, "found": false }),
            },
            None => {
                let keys: Vec<&String> = memory.keys().collect();
                json!({ "keys": keys })
            }
        };

        let elapsed_ms = start.elapsed().as_millis() as u64;
        Ok(ToolResult {
            success: true,
            output,
            error: None,
            execution_time_ms: elapsed_ms,
        })
    }

    /// List entries in a directory.
    async fn do_list_directory(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AthenError::Other("missing 'path' parameter".to_string()))?;

        tracing::info!(tool = "list_directory", path, "Listing directory");
        tracing::trace!(tool = "list_directory", "Filesystem tools use tokio::fs directly (unsandboxed)");

        let start = Instant::now();
        match tokio::fs::read_dir(path).await {
            Ok(mut reader) => {
                let mut entries = Vec::new();
                while let Ok(Some(entry)) = reader.next_entry().await {
                    let name = entry.file_name().to_string_lossy().to_string();
                    let file_type = match entry.file_type().await {
                        Ok(ft) => {
                            if ft.is_dir() {
                                "directory"
                            } else if ft.is_symlink() {
                                "symlink"
                            } else {
                                "file"
                            }
                        }
                        Err(_) => "unknown",
                    };
                    entries.push(json!({
                        "name": name,
                        "type": file_type,
                    }));
                }
                let elapsed_ms = start.elapsed().as_millis() as u64;
                Ok(ToolResult {
                    success: true,
                    output: json!({ "entries": entries, "count": entries.len() }),
                    error: None,
                    execution_time_ms: elapsed_ms,
                })
            }
            Err(e) => {
                let elapsed_ms = start.elapsed().as_millis() as u64;
                Ok(ToolResult {
                    success: false,
                    output: json!({ "error": e.to_string() }),
                    error: Some(e.to_string()),
                    execution_time_ms: elapsed_ms,
                })
            }
        }
    }
}

#[async_trait]
impl ToolRegistry for ShellToolRegistry {
    async fn list_tools(&self) -> Result<Vec<ToolDefinition>> {
        Ok(vec![
            ToolDefinition {
                name: "shell_execute".to_string(),
                description: "Run a shell command and return its output (stdout, stderr, exit code)".to_string(),
                parameters: Self::shell_execute_schema(),
                backend: ToolBackend::Shell {
                    command: String::new(),
                    native: false,
                },
                base_risk: BaseImpact::WritePersist,
            },
            ToolDefinition {
                name: "read_file".to_string(),
                description: "Read the contents of a file at the given path".to_string(),
                parameters: Self::read_file_schema(),
                backend: ToolBackend::Shell {
                    command: String::new(),
                    native: false,
                },
                base_risk: BaseImpact::Read,
            },
            ToolDefinition {
                name: "write_file".to_string(),
                description: "Write content to a file at the given path, creating or overwriting it".to_string(),
                parameters: Self::write_file_schema(),
                backend: ToolBackend::Shell {
                    command: String::new(),
                    native: false,
                },
                base_risk: BaseImpact::WritePersist,
            },
            ToolDefinition {
                name: "list_directory".to_string(),
                description: "List all files and directories in the given directory path".to_string(),
                parameters: Self::list_directory_schema(),
                backend: ToolBackend::Shell {
                    command: String::new(),
                    native: false,
                },
                base_risk: BaseImpact::Read,
            },
            ToolDefinition {
                name: "memory_store".to_string(),
                description: "Store a key-value pair in in-session memory for later recall".to_string(),
                parameters: Self::memory_store_schema(),
                backend: ToolBackend::Shell {
                    command: String::new(),
                    native: false,
                },
                base_risk: BaseImpact::Read,
            },
            ToolDefinition {
                name: "memory_recall".to_string(),
                description: "Recall a value by key from in-session memory, or list all stored keys if no key is given".to_string(),
                parameters: Self::memory_recall_schema(),
                backend: ToolBackend::Shell {
                    command: String::new(),
                    native: false,
                },
                base_risk: BaseImpact::Read,
            },
        ])
    }

    async fn call_tool(
        &self,
        name: &str,
        args: serde_json::Value,
    ) -> Result<ToolResult> {
        match name {
            "shell_execute" => self.do_shell_execute(&args).await,
            "read_file" => self.do_read_file(&args).await,
            "write_file" => self.do_write_file(&args).await,
            "list_directory" => self.do_list_directory(&args).await,
            "memory_store" => self.do_memory_store(&args).await,
            "memory_recall" => self.do_memory_recall(&args).await,
            _ => Err(AthenError::ToolNotFound(name.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::{NamedTempFile, TempDir};

    #[tokio::test]
    async fn test_list_tools_returns_expected_tools() {
        let registry = ShellToolRegistry::new().await;
        let tools = registry.list_tools().await.unwrap();

        assert_eq!(tools.len(), 6);

        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"shell_execute"));
        assert!(names.contains(&"read_file"));
        assert!(names.contains(&"write_file"));
        assert!(names.contains(&"list_directory"));
        assert!(names.contains(&"memory_store"));
        assert!(names.contains(&"memory_recall"));

        // Each tool should have a non-empty description and valid parameters schema.
        for tool in &tools {
            assert!(!tool.description.is_empty());
            assert!(tool.parameters.is_object());
            assert!(tool.parameters.get("properties").is_some());
        }
    }

    #[tokio::test]
    async fn test_shell_execute() {
        let registry = ShellToolRegistry::new().await;
        let result = registry
            .call_tool("shell_execute", json!({"command": "echo hello"}))
            .await
            .unwrap();

        assert!(result.success);
        let stdout = result.output["stdout"].as_str().unwrap();
        assert!(stdout.trim().contains("hello"));
    }

    #[tokio::test]
    async fn test_shell_execute_failure() {
        let registry = ShellToolRegistry::new().await;
        let result = registry
            .call_tool("shell_execute", json!({"command": "exit 42"}))
            .await
            .unwrap();

        assert!(!result.success);
        assert_eq!(result.output["exit_code"], 42);
        assert!(result.error.is_some());
    }

    #[tokio::test]
    async fn test_read_file() {
        let mut tmp = NamedTempFile::new().unwrap();
        write!(tmp, "hello from test").unwrap();
        let path = tmp.path().to_str().unwrap().to_string();

        let registry = ShellToolRegistry::new().await;
        let result = registry
            .call_tool("read_file", json!({"path": path}))
            .await
            .unwrap();

        assert!(result.success);
        let content = result.output["content"].as_str().unwrap();
        assert_eq!(content, "hello from test");
    }

    #[tokio::test]
    async fn test_read_file_not_found() {
        let registry = ShellToolRegistry::new().await;
        let result = registry
            .call_tool("read_file", json!({"path": "/tmp/__athen_nonexistent_file__"}))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.error.is_some());
    }

    #[tokio::test]
    async fn test_write_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test_write.txt");
        let path_str = path.to_str().unwrap().to_string();

        let registry = ShellToolRegistry::new().await;
        let result = registry
            .call_tool(
                "write_file",
                json!({"path": path_str, "content": "written by test"}),
            )
            .await
            .unwrap();

        assert!(result.success);
        assert_eq!(result.output["bytes_written"], 15);

        // Verify the file was actually written.
        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(content, "written by test");
    }

    #[tokio::test]
    async fn test_list_directory() {
        let dir = TempDir::new().unwrap();
        // Create a couple of files.
        std::fs::write(dir.path().join("alpha.txt"), "a").unwrap();
        std::fs::write(dir.path().join("beta.txt"), "b").unwrap();
        std::fs::create_dir(dir.path().join("subdir")).unwrap();

        let registry = ShellToolRegistry::new().await;
        let result = registry
            .call_tool(
                "list_directory",
                json!({"path": dir.path().to_str().unwrap()}),
            )
            .await
            .unwrap();

        assert!(result.success);
        let entries = result.output["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(result.output["count"], 3);

        let names: Vec<&str> = entries.iter().map(|e| e["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"alpha.txt"));
        assert!(names.contains(&"beta.txt"));
        assert!(names.contains(&"subdir"));
    }

    #[tokio::test]
    async fn test_unknown_tool_returns_error() {
        let registry = ShellToolRegistry::new().await;
        let result = registry.call_tool("nonexistent_tool", json!({})).await;

        assert!(result.is_err());
        match result.unwrap_err() {
            AthenError::ToolNotFound(name) => assert_eq!(name, "nonexistent_tool"),
            other => panic!("Expected ToolNotFound, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_shell_execute_missing_param() {
        let registry = ShellToolRegistry::new().await;
        let result = registry.call_tool("shell_execute", json!({})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_memory_store_and_recall() {
        let registry = ShellToolRegistry::new().await;

        let store_result = registry
            .call_tool("memory_store", json!({"key": "color", "value": "blue"}))
            .await
            .unwrap();
        assert!(store_result.success);
        assert_eq!(store_result.output["stored"], "color");

        let recall_result = registry
            .call_tool("memory_recall", json!({"key": "color"}))
            .await
            .unwrap();
        assert!(recall_result.success);
        assert_eq!(recall_result.output["key"], "color");
        assert_eq!(recall_result.output["value"], "blue");
    }

    #[tokio::test]
    async fn test_memory_recall_missing_key() {
        let registry = ShellToolRegistry::new().await;

        let result = registry
            .call_tool("memory_recall", json!({"key": "nonexistent"}))
            .await
            .unwrap();
        assert!(result.success);
        assert_eq!(result.output["key"], "nonexistent");
        assert_eq!(result.output["found"], false);
    }

    #[tokio::test]
    async fn test_memory_recall_all_keys() {
        let registry = ShellToolRegistry::new().await;

        // Store multiple values.
        registry
            .call_tool("memory_store", json!({"key": "a", "value": "1"}))
            .await
            .unwrap();
        registry
            .call_tool("memory_store", json!({"key": "b", "value": "2"}))
            .await
            .unwrap();

        // Recall without a key to list all keys.
        let result = registry
            .call_tool("memory_recall", json!({}))
            .await
            .unwrap();
        assert!(result.success);

        let keys = result.output["keys"].as_array().unwrap();
        assert_eq!(keys.len(), 2);

        let key_strs: Vec<&str> = keys.iter().map(|k| k.as_str().unwrap()).collect();
        assert!(key_strs.contains(&"a"));
        assert!(key_strs.contains(&"b"));
    }
}
