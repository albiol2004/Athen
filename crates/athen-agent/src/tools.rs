//! Built-in tool registry backed by shell execution and filesystem operations.

use std::time::Instant;

use async_trait::async_trait;
use serde_json::json;

use athen_core::error::{AthenError, Result};
use athen_core::risk::BaseImpact;
use athen_core::tool::{ToolBackend, ToolDefinition, ToolResult};
use athen_core::traits::shell::ShellExecutor;
use athen_core::traits::tool::ToolRegistry;
use athen_shell::Shell;

/// A [`ToolRegistry`] that provides built-in tools for shell execution
/// and filesystem operations, backed by [`athen_shell::Shell`].
pub struct ShellToolRegistry {
    shell: Shell,
}

impl ShellToolRegistry {
    /// Create a new registry, auto-detecting the available shell backend.
    pub async fn new() -> Self {
        Self {
            shell: Shell::new().await,
        }
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
    async fn do_shell_execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let command = args
            .get("command")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AthenError::Other("missing 'command' parameter".to_string()))?;

        tracing::info!(tool = "shell_execute", command, "Executing shell command");

        let start = Instant::now();
        let output = self.shell.execute(command).await?;
        let elapsed_ms = start.elapsed().as_millis() as u64;

        let success = output.exit_code == 0;
        let result_output = json!({
            "stdout": output.stdout,
            "stderr": output.stderr,
            "exit_code": output.exit_code,
        });

        Ok(ToolResult {
            success,
            output: result_output,
            error: if success {
                None
            } else {
                Some(format!(
                    "exit code {}: {}",
                    output.exit_code,
                    output.stderr.trim()
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

    /// List entries in a directory.
    async fn do_list_directory(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AthenError::Other("missing 'path' parameter".to_string()))?;

        tracing::info!(tool = "list_directory", path, "Listing directory");

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

        assert_eq!(tools.len(), 4);

        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"shell_execute"));
        assert!(names.contains(&"read_file"));
        assert!(names.contains(&"write_file"));
        assert!(names.contains(&"list_directory"));

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
}
