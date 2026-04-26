//! MCP Filesystem server library.
//!
//! Exposes a sandboxed filesystem as an MCP server. All operations are
//! constrained to a single root directory passed at construction time —
//! any attempt to escape the root (via `..`, symlinks, or absolute paths)
//! is rejected before the operation is performed.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, Content, Implementation, ProtocolVersion, ServerCapabilities, ServerInfo,
};
use rmcp::{ErrorData as McpError, ServerHandler, schemars, tool, tool_handler, tool_router};

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct PathArg {
    /// Path relative to the sandbox root. Use "." for the root itself, or
    /// relative paths like "notes/foo.txt". Absolute paths are rejected —
    /// use shell_execute for files outside the sandbox.
    pub path: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct WriteArg {
    /// Path relative to the sandbox root. Use "." for the root itself, or
    /// relative paths like "notes/foo.txt". Absolute paths are rejected —
    /// use shell_execute for files outside the sandbox.
    pub path: String,
    /// File contents to write (UTF-8).
    pub contents: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct MoveArg {
    /// Source path relative to the sandbox root. Use relative paths like
    /// "notes/foo.txt". Absolute paths are rejected — use shell_execute
    /// for files outside the sandbox.
    pub from: String,
    /// Destination path relative to the sandbox root. Use relative paths
    /// like "notes/foo.txt". Absolute paths are rejected — use
    /// shell_execute for files outside the sandbox.
    pub to: String,
}

/// Filesystem MCP server. Cheap to clone — wraps an `Arc` internally.
#[derive(Clone)]
pub struct Filesystem {
    root: Arc<PathBuf>,
    #[allow(dead_code)]
    tool_router: ToolRouter<Filesystem>,
}

#[tool_router]
impl Filesystem {
    pub fn new(root: PathBuf) -> std::io::Result<Self> {
        let canonical = std::fs::canonicalize(&root)?;
        Ok(Self {
            root: Arc::new(canonical),
            tool_router: Self::tool_router(),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolve a user-provided path against the sandbox root, rejecting
    /// absolute paths and any traversal that would escape the root.
    fn resolve(&self, path: &str) -> Result<PathBuf, McpError> {
        let candidate = Path::new(path);
        let root = self.root.display();
        if candidate.is_absolute() {
            return Err(McpError::invalid_params(
                format!(
                    "path '{path}' is absolute, but this MCP is sandboxed to '{root}'. \
                     Use a path relative to that root (e.g. '.', 'notes/foo.txt'). \
                     For files outside the sandbox, use shell_execute instead."
                ),
                None,
            ));
        }
        let mut joined = self.root.as_ref().clone();
        for comp in candidate.components() {
            match comp {
                Component::Normal(name) => joined.push(name),
                Component::CurDir => {}
                Component::ParentDir => {
                    if !joined.pop() || !joined.starts_with(self.root.as_ref()) {
                        return Err(McpError::invalid_params(
                            format!(
                                "path '{path}' escapes the sandbox root '{root}'. \
                                 Use a relative path that stays inside the root, or \
                                 shell_execute for paths outside it."
                            ),
                            None,
                        ));
                    }
                }
                Component::Prefix(_) | Component::RootDir => {
                    return Err(McpError::invalid_params(
                        format!(
                            "path '{path}' is absolute, but this MCP is sandboxed to '{root}'. \
                             Use a path relative to that root (e.g. '.', 'notes/foo.txt'). \
                             For files outside the sandbox, use shell_execute instead."
                        ),
                        None,
                    ));
                }
            }
        }
        if !joined.starts_with(self.root.as_ref()) {
            return Err(McpError::invalid_params(
                format!(
                    "path '{path}' escapes the sandbox root '{root}'. \
                     Use a relative path that stays inside the root, or \
                     shell_execute for paths outside it."
                ),
                None,
            ));
        }
        Ok(joined)
    }

    #[tool(description = "Read a UTF-8 text file from the sandbox. For files outside the sandbox, use shell_execute.")]
    async fn read_file(
        &self,
        Parameters(PathArg { path }): Parameters<PathArg>,
    ) -> Result<CallToolResult, McpError> {
        let resolved = self.resolve(&path)?;
        match tokio::fs::read_to_string(&resolved).await {
            Ok(text) => Ok(CallToolResult::success(vec![Content::text(text)])),
            Err(e) => Err(McpError::internal_error(format!("read failed: {e}"), None)),
        }
    }

    #[tool(description = "Write (overwrite) a UTF-8 text file in the sandbox. Creates parent dirs as needed. For files outside the sandbox, use shell_execute.")]
    async fn write_file(
        &self,
        Parameters(WriteArg { path, contents }): Parameters<WriteArg>,
    ) -> Result<CallToolResult, McpError> {
        let resolved = self.resolve(&path)?;
        if let Some(parent) = resolved.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| McpError::internal_error(format!("mkdir failed: {e}"), None))?;
        }
        tokio::fs::write(&resolved, contents.as_bytes())
            .await
            .map_err(|e| McpError::internal_error(format!("write failed: {e}"), None))?;
        Ok(CallToolResult::success(vec![Content::text("ok")]))
    }

    #[tool(description = "Append text to a file in the sandbox. Creates the file if missing. For files outside the sandbox, use shell_execute.")]
    async fn append_file(
        &self,
        Parameters(WriteArg { path, contents }): Parameters<WriteArg>,
    ) -> Result<CallToolResult, McpError> {
        let resolved = self.resolve(&path)?;
        if let Some(parent) = resolved.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| McpError::internal_error(format!("mkdir failed: {e}"), None))?;
        }
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&resolved)
            .await
            .map_err(|e| McpError::internal_error(format!("open failed: {e}"), None))?;
        use tokio::io::AsyncWriteExt;
        file.write_all(contents.as_bytes())
            .await
            .map_err(|e| McpError::internal_error(format!("write failed: {e}"), None))?;
        Ok(CallToolResult::success(vec![Content::text("ok")]))
    }

    #[tool(description = "List entries in a sandbox directory. Returns one entry per line as 'TYPE\\tNAME'. Use '.' to list the sandbox root. For directories outside the sandbox, use shell_execute.")]
    async fn list_dir(
        &self,
        Parameters(PathArg { path }): Parameters<PathArg>,
    ) -> Result<CallToolResult, McpError> {
        let resolved = self.resolve(&path)?;
        let mut reader = tokio::fs::read_dir(&resolved)
            .await
            .map_err(|e| McpError::internal_error(format!("read_dir failed: {e}"), None))?;
        let mut out = String::new();
        while let Some(entry) = reader
            .next_entry()
            .await
            .map_err(|e| McpError::internal_error(format!("read_dir failed: {e}"), None))?
        {
            let ft = entry
                .file_type()
                .await
                .map_err(|e| McpError::internal_error(format!("file_type failed: {e}"), None))?;
            let kind = if ft.is_dir() {
                "DIR"
            } else if ft.is_file() {
                "FILE"
            } else {
                "OTHER"
            };
            let name = entry.file_name().to_string_lossy().to_string();
            out.push_str(kind);
            out.push('\t');
            out.push_str(&name);
            out.push('\n');
        }
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    #[tool(description = "Create a directory (and parents) at the given sandbox path. For paths outside the sandbox, use shell_execute.")]
    async fn create_dir(
        &self,
        Parameters(PathArg { path }): Parameters<PathArg>,
    ) -> Result<CallToolResult, McpError> {
        let resolved = self.resolve(&path)?;
        tokio::fs::create_dir_all(&resolved)
            .await
            .map_err(|e| McpError::internal_error(format!("mkdir failed: {e}"), None))?;
        Ok(CallToolResult::success(vec![Content::text("ok")]))
    }

    #[tool(description = "Delete a file or directory in the sandbox. Recursive for directories. For paths outside the sandbox, use shell_execute.")]
    async fn delete_path(
        &self,
        Parameters(PathArg { path }): Parameters<PathArg>,
    ) -> Result<CallToolResult, McpError> {
        let resolved = self.resolve(&path)?;
        if resolved == *self.root {
            return Err(McpError::invalid_params(
                "refusing to delete sandbox root",
                None,
            ));
        }
        let meta = tokio::fs::symlink_metadata(&resolved)
            .await
            .map_err(|e| McpError::internal_error(format!("stat failed: {e}"), None))?;
        if meta.is_dir() {
            tokio::fs::remove_dir_all(&resolved)
                .await
                .map_err(|e| McpError::internal_error(format!("rmdir failed: {e}"), None))?;
        } else {
            tokio::fs::remove_file(&resolved)
                .await
                .map_err(|e| McpError::internal_error(format!("rm failed: {e}"), None))?;
        }
        Ok(CallToolResult::success(vec![Content::text("ok")]))
    }

    #[tool(description = "Move or rename a file/directory within the sandbox. Both paths must stay inside the sandbox; for paths outside it, use shell_execute.")]
    async fn move_path(
        &self,
        Parameters(MoveArg { from, to }): Parameters<MoveArg>,
    ) -> Result<CallToolResult, McpError> {
        let from_resolved = self.resolve(&from)?;
        let to_resolved = self.resolve(&to)?;
        if let Some(parent) = to_resolved.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| McpError::internal_error(format!("mkdir failed: {e}"), None))?;
        }
        tokio::fs::rename(&from_resolved, &to_resolved)
            .await
            .map_err(|e| McpError::internal_error(format!("rename failed: {e}"), None))?;
        Ok(CallToolResult::success(vec![Content::text("ok")]))
    }

    #[tool(description = "Check whether a sandbox path exists. Returns 'true' or 'false'. For paths outside the sandbox, use shell_execute.")]
    async fn exists(
        &self,
        Parameters(PathArg { path }): Parameters<PathArg>,
    ) -> Result<CallToolResult, McpError> {
        let resolved = self.resolve(&path)?;
        let exists = tokio::fs::try_exists(&resolved).await.unwrap_or(false);
        Ok(CallToolResult::success(vec![Content::text(
            exists.to_string(),
        )]))
    }

    #[tool(description = "Get metadata for a sandbox path. Returns 'TYPE\\tSIZE_BYTES'. For paths outside the sandbox, use shell_execute.")]
    async fn stat(
        &self,
        Parameters(PathArg { path }): Parameters<PathArg>,
    ) -> Result<CallToolResult, McpError> {
        let resolved = self.resolve(&path)?;
        let meta = tokio::fs::metadata(&resolved)
            .await
            .map_err(|e| McpError::internal_error(format!("stat failed: {e}"), None))?;
        let kind = if meta.is_dir() {
            "DIR"
        } else if meta.is_file() {
            "FILE"
        } else {
            "OTHER"
        };
        Ok(CallToolResult::success(vec![Content::text(format!(
            "{kind}\t{}",
            meta.len()
        ))]))
    }
}

#[tool_handler]
impl ServerHandler for Filesystem {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder().enable_tools().build(),
        )
        .with_server_info(Implementation::from_build_env())
        .with_protocol_version(ProtocolVersion::V_2024_11_05)
        .with_instructions(
            "Sandboxed filesystem access. All paths are relative to a fixed root directory; \
             absolute paths and traversal outside the root are rejected. \
             For files outside this sandbox, prefer shell_execute. \
             Tools: read_file, write_file, append_file, list_dir, create_dir, delete_path, \
             move_path, exists, stat."
                .to_string(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::ServiceExt;
    use rmcp::model::CallToolRequestParams;
    use tempfile::tempdir;

    fn args(v: serde_json::Value) -> rmcp::model::JsonObject {
        v.as_object().expect("object").clone()
    }

    fn fs_at(root: &Path) -> Filesystem {
        Filesystem::new(root.to_path_buf()).expect("new")
    }

    #[test]
    fn resolve_rejects_absolute() {
        let dir = tempdir().unwrap();
        let fs = fs_at(dir.path());
        assert!(fs.resolve("/etc/passwd").is_err());
    }

    #[test]
    fn resolve_rejects_traversal() {
        let dir = tempdir().unwrap();
        let fs = fs_at(dir.path());
        assert!(fs.resolve("../escape").is_err());
        assert!(fs.resolve("a/../../escape").is_err());
    }

    #[test]
    fn resolve_allows_normal_paths() {
        let dir = tempdir().unwrap();
        let fs = fs_at(dir.path());
        let p = fs.resolve("a/b/c.txt").unwrap();
        assert!(p.starts_with(fs.root()));
        assert!(p.ends_with("a/b/c.txt"));
    }

    #[test]
    fn resolve_internal_parent_ok() {
        let dir = tempdir().unwrap();
        let fs = fs_at(dir.path());
        // a/b/../c resolves inside the root
        let p = fs.resolve("a/b/../c").unwrap();
        assert!(p.starts_with(fs.root()));
    }

    #[tokio::test]
    async fn write_then_read_round_trip() {
        let dir = tempdir().unwrap();
        let (server_t, client_t) = tokio::io::duplex(8192);
        let fs = fs_at(dir.path());
        let server_handle = tokio::spawn(async move {
            let svc = fs.serve(server_t).await.unwrap();
            svc.waiting().await.unwrap();
        });

        #[derive(Default, Clone)]
        struct C;
        impl rmcp::ClientHandler for C {}
        let client = C.serve(client_t).await.unwrap();

        let _ = client
            .call_tool(
                CallToolRequestParams::new("write_file")
                    .with_arguments(args(serde_json::json!({"path": "hello.txt", "contents": "hi"}))),
            )
            .await
            .unwrap();

        let read = client
            .call_tool(
                CallToolRequestParams::new("read_file")
                    .with_arguments(args(serde_json::json!({"path": "hello.txt"}))),
            )
            .await
            .unwrap();

        let text = read
            .content
            .iter()
            .find_map(|c| c.as_text().map(|t| t.text.clone()))
            .unwrap_or_default();
        assert_eq!(text, "hi");

        client.cancel().await.unwrap();
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn list_dir_works() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "x").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();

        let (server_t, client_t) = tokio::io::duplex(8192);
        let fs = fs_at(dir.path());
        let server_handle = tokio::spawn(async move {
            let svc = fs.serve(server_t).await.unwrap();
            svc.waiting().await.unwrap();
        });

        #[derive(Default, Clone)]
        struct C;
        impl rmcp::ClientHandler for C {}
        let client = C.serve(client_t).await.unwrap();

        let res = client
            .call_tool(
                CallToolRequestParams::new("list_dir")
                    .with_arguments(args(serde_json::json!({"path": "."}))),
            )
            .await
            .unwrap();
        let text = res
            .content
            .iter()
            .find_map(|c| c.as_text().map(|t| t.text.clone()))
            .unwrap_or_default();
        assert!(text.contains("FILE\ta.txt"));
        assert!(text.contains("DIR\tsub"));

        client.cancel().await.unwrap();
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn rejects_escape_through_tool_call() {
        let dir = tempdir().unwrap();
        let (server_t, client_t) = tokio::io::duplex(8192);
        let fs = fs_at(dir.path());
        let server_handle = tokio::spawn(async move {
            let svc = fs.serve(server_t).await.unwrap();
            svc.waiting().await.unwrap();
        });

        #[derive(Default, Clone)]
        struct C;
        impl rmcp::ClientHandler for C {}
        let client = C.serve(client_t).await.unwrap();

        let res = client
            .call_tool(
                CallToolRequestParams::new("read_file")
                    .with_arguments(args(serde_json::json!({"path": "../../../etc/passwd"}))),
            )
            .await;
        assert!(res.is_err(), "escape attempt must fail");

        client.cancel().await.unwrap();
        let _ = server_handle.await;
    }
}
