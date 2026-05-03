//! Built-in tool registry backed by shell execution and filesystem operations.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
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
use athen_web::{DuckDuckGoSearch, HybridReader, PageReader, WebSearchProvider};

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

/// Metadata for a process started via `shell_spawn`. Kept in
/// [`ShellToolRegistry::spawned`] so subsequent `shell_logs`/`shell_kill`
/// calls can find the log file and validate the PID is one we own.
#[derive(Clone, Debug)]
pub struct SpawnedProcess {
    pub pid: u32,
    pub command: String,
    pub label: Option<String>,
    pub log_path: PathBuf,
    pub started_at: chrono::DateTime<chrono::Utc>,
}

/// Shared map of spawned processes. Hosted by `AppState` so it survives
/// across the per-message `ShellToolRegistry` instances and the agent can
/// kill in turn N a process it spawned in turn N-1.
pub type SpawnedProcessMap = Arc<Mutex<HashMap<u32, SpawnedProcess>>>;

/// A [`ToolRegistry`] that provides built-in tools for shell execution,
/// filesystem operations, and in-session key-value memory,
/// backed by [`athen_shell::Shell`].
pub struct ShellToolRegistry {
    shell: Shell,
    memory: Arc<Mutex<HashMap<String, String>>>,
    sandbox: Option<UnifiedSandbox>,
    rule_engine: RuleEngine,
    extra_writable: Option<Arc<dyn ShellExtraWritableProvider>>,
    /// Per-path blake3 hash of the file contents the agent has most recently
    /// `read`. Used by `edit` and `write` to enforce read-before-modify and
    /// detect external changes between reads and writes.
    read_state: Arc<Mutex<HashMap<PathBuf, blake3::Hash>>>,
    /// Processes started via `shell_spawn`, keyed by PID. We only ever
    /// kill PIDs in this map — refusing to touch arbitrary system PIDs.
    spawned: Arc<Mutex<HashMap<u32, SpawnedProcess>>>,
    /// Backend for the `web_search` tool. Defaults to bundled DuckDuckGo
    /// HTML scraping (no key). Can be swapped for Tavily etc. via
    /// [`Self::with_web_search`].
    web_search: Arc<dyn WebSearchProvider>,
    /// Backend for the `web_fetch` tool. Defaults to a local reqwest-based
    /// reader. Can be swapped for Cloudflare's Browser Rendering via
    /// [`Self::with_page_reader`].
    page_reader: Arc<dyn PageReader>,
}

/// Defense-in-depth: tolerate args delivered as a JSON-encoded string
/// instead of a JSON object. Some LLM providers occasionally emit the
/// `arguments` field as a string when its value contains tricky escapes
/// (large HTML/code with raw newlines, etc.). The provider layer
/// repairs and parses these eagerly with a lenient pass, but if a
/// `Value::String` ever reaches a `do_*` function we attempt the same
/// repair here so the tool still works.
///
/// Returns a reference to either the original `args` (when it's already
/// an object/array) or the parsed-into-`owned` value when it was a
/// JSON-encoded string. If the string can't be parsed even after
/// repair, returns the original — the caller surfaces a clean
/// "missing 'X'" error.
fn coerce_args<'a>(
    args: &'a serde_json::Value,
    owned: &'a mut serde_json::Value,
) -> &'a serde_json::Value {
    if let Some(s) = args.as_str() {
        // Strict parse first.
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(s) {
            *owned = parsed;
            return owned;
        }
        // Same lenient repair the provider layer uses: re-escape raw
        // control characters inside string literals and try again.
        let repaired = repair_unescaped_control_chars(s);
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&repaired) {
            tracing::warn!(
                "Tool args reached do_* as Value::String; repaired and parsed (len={})",
                s.len()
            );
            *owned = parsed;
            return owned;
        }
    }
    args
}

/// Build the LLM-facing error returned when tool args arrive at a
/// `do_*` helper as a `Value::String`. By the time we get here, the
/// provider's strict + control-char + aggressive-quote repair passes
/// have all failed — the overwhelmingly common cause is the LLM
/// hitting the output token limit mid-string, leaving the args
/// truncated. We tell the model exactly that, plus a tool-specific
/// suggestion for how to retry without looping on the same too-large
/// payload.
fn args_truncated_result(tool: &str, raw: &str, suggestion: &str, started: Instant) -> ToolResult {
    let head: String = raw.chars().take(120).collect();
    let tail: String = {
        let s: String = raw.chars().rev().take(120).collect();
        s.chars().rev().collect()
    };
    let msg = format!(
        "{tool}: tool-call arguments could not be parsed as JSON \
         (raw len={}). The most likely cause is that the model output \
         was truncated by the token limit mid-string — the args buffer \
         ends without a closing quote/brace. Suggestion: {suggestion}.",
        raw.len()
    );
    ToolResult {
        success: false,
        output: serde_json::json!({
            "error": msg,
            "raw_len": raw.len(),
            "raw_head": head,
            "raw_tail": tail,
        }),
        error: Some(msg),
        execution_time_ms: started.elapsed().as_millis() as u64,
    }
}

/// Walk the input and escape unescaped control characters inside JSON
/// string literals only. Mirrors `escape_control_chars_in_strings` in
/// the provider layer; duplicated here to avoid an inter-crate
/// dependency on a private helper.
fn repair_unescaped_control_chars(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 16);
    let mut in_string = false;
    let mut prev_backslash = false;
    for ch in input.chars() {
        if in_string {
            if prev_backslash {
                out.push(ch);
                prev_backslash = false;
                continue;
            }
            match ch {
                '\\' => {
                    out.push(ch);
                    prev_backslash = true;
                }
                '"' => {
                    out.push(ch);
                    in_string = false;
                }
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if (c as u32) < 0x20 => {
                    out.push_str(&format!("\\u{:04x}", c as u32));
                }
                c => out.push(c),
            }
        } else {
            if ch == '"' {
                in_string = true;
            }
            out.push(ch);
        }
    }
    out
}

impl ShellToolRegistry {
    /// Wrap a shell command so it runs in the agent workspace directory by
    /// default. Returns the command unchanged if the workspace dir can't be
    /// determined. The user's command runs inside `( ... )` so embedded
    /// `;` doesn't escape the cd (e.g. `cd ws && echo a; echo b` would
    /// otherwise leak `echo b` outside the workspace context).
    fn workspace_wrap(command: &str) -> String {
        let Some(ws) = paths::athen_workspace_dir() else {
            return command.to_string();
        };
        let escaped = format!("'{}'", ws.to_string_lossy().replace('\'', "'\\''"));
        format!("cd {escaped} && ( {command} )")
    }

    /// Best-effort: create the agent workspace dir so relative paths in
    /// `read`/`edit`/`write`/`grep` and shell `cd <workspace>` wrapping have
    /// a real directory to land in. Failure is logged but not fatal — we'll
    /// surface a real error on first use if the path is genuinely unwritable.
    async fn ensure_workspace_dir() {
        if let Some(ws) = paths::athen_workspace_dir() {
            if let Err(e) = tokio::fs::create_dir_all(&ws).await {
                tracing::warn!(
                    workspace = %ws.display(),
                    error = %e,
                    "Failed to create agent workspace dir"
                );
            }
        }
    }

    /// Create a new registry, auto-detecting the available shell backend
    /// and sandbox capabilities.
    pub async fn new() -> Self {
        Self::ensure_workspace_dir().await;
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
            read_state: Arc::new(Mutex::new(HashMap::new())),
            spawned: Arc::new(Mutex::new(HashMap::new())),
            web_search: Arc::new(DuckDuckGoSearch::new()),
            // HybridReader chains Local → Jina → Wayback so SPAs and
            // paywalled/blocked pages still produce content without any
            // user configuration.
            page_reader: Arc::new(HybridReader::new()),
        }
    }

    /// Replace the default DuckDuckGo search backend (e.g. with Tavily).
    pub fn with_web_search(mut self, provider: Arc<dyn WebSearchProvider>) -> Self {
        self.web_search = provider;
        self
    }

    /// Replace the default local page reader (e.g. with Cloudflare's
    /// Browser Rendering for JS-heavy sites).
    pub fn with_page_reader(mut self, reader: Arc<dyn PageReader>) -> Self {
        self.page_reader = reader;
        self
    }

    /// Inject a provider that supplies additional writable paths for the
    /// shell sandbox, typically derived from the active arc's grants.
    pub fn with_extra_writable(mut self, provider: Arc<dyn ShellExtraWritableProvider>) -> Self {
        self.extra_writable = Some(provider);
        self
    }

    /// Share the map of `shell_spawn`-tracked processes with another owner
    /// (typically `AppState`). Without this, each per-message registry has
    /// its own empty map and the agent can't kill in turn N+1 a server it
    /// spawned in turn N.
    pub fn with_spawned_processes(mut self, spawned: SpawnedProcessMap) -> Self {
        self.spawned = spawned;
        self
    }

    /// Build the JSON Schema for the `shell_execute` tool parameters.
    fn shell_execute_schema() -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The shell command to execute. For long-lived servers/daemons \
                                    use shell_spawn instead — bare `&` leaves stdio inherited and \
                                    hangs this call until timeout."
                },
                "timeout_ms": {
                    "type": "integer",
                    "description": "Hard timeout in milliseconds (default 60000, max 600000). \
                                    On timeout the process is killed."
                }
            },
            "required": ["command"]
        })
    }

    /// Build the JSON Schema for the `read` tool parameters.
    fn read_schema() -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Absolute path to the file"
                },
                "offset": {
                    "type": "integer",
                    "description": "1-based starting line (default 1)"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of lines to return (default 2000)"
                }
            },
            "required": ["path"]
        })
    }

    /// Build the JSON Schema for the `edit` tool parameters.
    fn edit_schema() -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Absolute path to the file"
                },
                "old_string": {
                    "type": "string",
                    "description": "Exact text to replace (must be unique unless replace_all=true)"
                },
                "new_string": {
                    "type": "string",
                    "description": "Replacement text"
                },
                "replace_all": {
                    "type": "boolean",
                    "description": "Replace every occurrence (default false)"
                }
            },
            "required": ["path", "old_string", "new_string"]
        })
    }

    /// Build the JSON Schema for the `write` tool parameters.
    fn write_schema() -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Absolute path to the file"
                },
                "content": {
                    "type": "string",
                    "description": "Full file contents"
                }
            },
            "required": ["path", "content"]
        })
    }

    /// Build the JSON Schema for the `grep` tool parameters.
    fn grep_schema() -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Regex pattern to search for"
                },
                "path": {
                    "type": "string",
                    "description": "File or directory to search (default '.')"
                },
                "glob": {
                    "type": "string",
                    "description": "Glob filter, e.g. '*.rs'"
                },
                "case_insensitive": {
                    "type": "boolean",
                    "description": "Case-insensitive match"
                },
                "line_numbers": {
                    "type": "boolean",
                    "description": "Include line numbers in content mode (default true)"
                },
                "context_before": {
                    "type": "integer",
                    "description": "Lines of context before each match"
                },
                "context_after": {
                    "type": "integer",
                    "description": "Lines of context after each match"
                },
                "output_mode": {
                    "type": "string",
                    "description": "'files_with_matches' (default), 'content', or 'count'"
                },
                "max_count": {
                    "type": "integer",
                    "description": "Cap on number of results (default 100)"
                }
            },
            "required": ["pattern"]
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

        let timeout_ms = args
            .get("timeout_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(60_000)
            .min(600_000);

        // Default-cwd to the agent workspace. Risk evaluation and the
        // sandbox-allowed paths see the original command — the wrapper is
        // only what actually runs.
        let wrapped_command = Self::workspace_wrap(command);

        tracing::info!(
            tool = "shell_execute",
            command,
            timeout_ms,
            "Executing shell command"
        );

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
        // The whole exec is wrapped below in `tokio::time::timeout`; bwrap
        // and NativeShell both set `kill_on_drop`, so dropping the future
        // SIGKILLs the spawned process group.
        let exec_future = async {
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
                    .execute_sandboxed("sh", &["-c", &wrapped_command], &level)
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
                            let output = self.shell.execute(&wrapped_command).await?;
                            (output.stdout, output.stderr, output.exit_code)
                        } else {
                            tracing::debug!(
                                tool = "shell_execute",
                                "Command executed inside sandbox"
                            );
                            (output.stdout, output.stderr, output.exit_code)
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            tool = "shell_execute",
                            error = %e,
                            "Sandbox execution failed, falling back to unsandboxed shell"
                        );
                        let output = self.shell.execute(&wrapped_command).await?;
                        (output.stdout, output.stderr, output.exit_code)
                    }
                }
            } else {
                tracing::trace!(
                    tool = "shell_execute",
                    "No sandbox available, executing unsandboxed"
                );
                let output = self.shell.execute(&wrapped_command).await?;
                (output.stdout, output.stderr, output.exit_code)
            };
            Ok::<_, AthenError>((stdout, stderr, exit_code))
        };

        let (stdout, stderr, exit_code) =
            match tokio::time::timeout(std::time::Duration::from_millis(timeout_ms), exec_future)
                .await
            {
                Ok(inner) => inner?,
                Err(_) => {
                    tracing::warn!(
                        tool = "shell_execute",
                        command,
                        timeout_ms,
                        "Command timed out — process killed"
                    );
                    let msg = format!(
                        "command timed out after {}ms and was killed. \
                     For long-lived servers/daemons use shell_spawn (returns a PID + log file). \
                     For commands that legitimately need longer, pass `timeout_ms` (max 600000).",
                        timeout_ms
                    );
                    return Ok(ToolResult {
                        success: false,
                        output: json!({ "error": &msg, "timed_out": true }),
                        error: Some(msg),
                        execution_time_ms: start.elapsed().as_millis() as u64,
                    });
                }
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
                Some(format!("exit code {}: {}", exit_code, stderr.trim()))
            },
            execution_time_ms: elapsed_ms,
        })
    }

    /// Canonicalize a path for read-state map keys. Falls back to a
    /// workspace-anchored absolute path when the file doesn't yet exist.
    fn canonical_key(p: &Path) -> PathBuf {
        if let Ok(c) = std::fs::canonicalize(p) {
            return c;
        }
        paths::resolve_in_workspace(p)
    }

    /// Record the blake3 hash of `content` for `path`. Always hashes the
    /// whole file regardless of which slice the caller used.
    pub async fn record_hash(&self, path: &Path, content: &[u8]) {
        let key = Self::canonical_key(path);
        let hash = blake3::hash(content);
        self.read_state.lock().await.insert(key, hash);
    }

    /// Verify that `current` matches the stored hash for `path`. Returns
    /// a clear error when the file was never read or has been modified
    /// externally since the last read.
    pub async fn check_hash(&self, path: &Path, current: &[u8]) -> Result<()> {
        let key = Self::canonical_key(path);
        let map = self.read_state.lock().await;
        match map.get(&key) {
            None => Err(AthenError::Other(format!(
                "must Read this file first before editing or overwriting: {}",
                path.display()
            ))),
            Some(stored) => {
                let now = blake3::hash(current);
                if *stored == now {
                    Ok(())
                } else {
                    Err(AthenError::Other(format!(
                        "file changed on disk since last read: {} — re-read it before editing",
                        path.display()
                    )))
                }
            }
        }
    }

    /// Read a file (or a slice of it) and return numbered lines.
    async fn do_read(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let mut owned = serde_json::Value::Null;
        let args = coerce_args(args, &mut owned);
        let path_arg = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AthenError::Other("missing 'path' parameter".to_string()))?;
        let offset = args.get("offset").and_then(|v| v.as_u64()).unwrap_or(1);
        let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(2000);
        let offset = offset.max(1) as usize;
        let limit = limit.max(1) as usize;

        // Relative paths resolve against the agent workspace, not the
        // process cwd — that's where the agent's own files live.
        let resolved = paths::resolve_in_workspace(Path::new(path_arg));
        let path = resolved.to_string_lossy().to_string();
        let path = path.as_str();

        tracing::info!(tool = "read", path, offset, limit, "Reading file");
        let start = Instant::now();
        let p = resolved.as_path();

        // Reject directories early with a clear message.
        match tokio::fs::metadata(p).await {
            Ok(m) if m.is_dir() => {
                let msg = format!("'{path}' is a directory, not a file");
                return Ok(ToolResult {
                    success: false,
                    output: json!({ "error": msg }),
                    error: Some(msg),
                    execution_time_ms: start.elapsed().as_millis() as u64,
                });
            }
            Ok(_) => {}
            Err(e) => {
                let msg = format!("cannot read '{path}': {e}");
                return Ok(ToolResult {
                    success: false,
                    output: json!({ "error": msg }),
                    error: Some(msg),
                    execution_time_ms: start.elapsed().as_millis() as u64,
                });
            }
        }

        let bytes = match tokio::fs::read(p).await {
            Ok(b) => b,
            Err(e) => {
                let msg = e.to_string();
                return Ok(ToolResult {
                    success: false,
                    output: json!({ "error": msg }),
                    error: Some(msg),
                    execution_time_ms: start.elapsed().as_millis() as u64,
                });
            }
        };

        // Binary-file detection: any NUL byte and we bail. Cheap and
        // matches what most editors and tools do.
        if bytes.contains(&0u8) {
            let msg = format!("'{path}' appears to be a binary file (contains NUL bytes)");
            return Ok(ToolResult {
                success: false,
                output: json!({ "error": msg }),
                error: Some(msg),
                execution_time_ms: start.elapsed().as_millis() as u64,
            });
        }

        let content = String::from_utf8_lossy(&bytes).to_string();

        // Slice the requested line range, then number lines `cat -n` style.
        let total_lines = content.lines().count();
        let mut out = String::new();
        let mut returned = 0usize;
        for (i, line) in content.lines().enumerate() {
            let line_no = i + 1;
            if line_no < offset {
                continue;
            }
            if returned >= limit {
                break;
            }
            out.push_str(&format!("{line_no}\t{line}\n"));
            returned += 1;
        }

        // Always hash the *whole* file content so a partial read still
        // protects subsequent edits.
        self.record_hash(p, &bytes).await;

        let elapsed_ms = start.elapsed().as_millis() as u64;
        Ok(ToolResult {
            success: true,
            output: json!({
                "content": out,
                "lines_returned": returned,
                "total_lines": total_lines,
                "offset": offset,
            }),
            error: None,
            execution_time_ms: elapsed_ms,
        })
    }

    /// Edit a file by exact-string replacement.
    async fn do_edit(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let mut owned = serde_json::Value::Null;
        let args = coerce_args(args, &mut owned);
        // Same truncation guard as do_write: if args remained a string
        // after coercion, the JSON tool call was cut off mid-payload.
        if let Some(raw) = args.as_str() {
            return Ok(args_truncated_result(
                "edit",
                raw,
                "shrink the new_string (or chunk replacements across \
                 multiple edit calls) so the JSON tool call fits the \
                 model output budget",
                Instant::now(),
            ));
        }
        let path_arg = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AthenError::Other("missing 'path' parameter".to_string()))?;
        let old_string = args
            .get("old_string")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AthenError::Other("missing 'old_string' parameter".to_string()))?;
        let new_string = args
            .get("new_string")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AthenError::Other("missing 'new_string' parameter".to_string()))?;
        let replace_all = args
            .get("replace_all")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if old_string == new_string {
            return Err(AthenError::Other(
                "old_string and new_string are identical (no-op)".to_string(),
            ));
        }

        let resolved = paths::resolve_in_workspace(Path::new(path_arg));
        let path = resolved.to_string_lossy().to_string();
        let path = path.as_str();

        tracing::info!(tool = "edit", path, replace_all, "Editing file");
        let start = Instant::now();
        let p = resolved.as_path();

        let current = tokio::fs::read(p)
            .await
            .map_err(|e| AthenError::Other(format!("cannot read '{path}': {e}")))?;

        // Read-before-write guard. Returns its own clear error message.
        self.check_hash(p, &current).await?;

        let text = String::from_utf8_lossy(&current).to_string();
        let count = text.matches(old_string).count();
        if count == 0 {
            return Err(AthenError::Other(format!(
                "old_string not found in '{path}'"
            )));
        }
        let new_text = if replace_all {
            text.replace(old_string, new_string)
        } else {
            if count > 1 {
                return Err(AthenError::Other(format!(
                    "old_string matches {count} times in '{path}'; \
                     add surrounding context to make it unique, or pass replace_all=true"
                )));
            }
            text.replacen(old_string, new_string, 1)
        };

        atomic_write(p, new_text.as_bytes()).await?;
        self.record_hash(p, new_text.as_bytes()).await;

        let elapsed_ms = start.elapsed().as_millis() as u64;
        Ok(ToolResult {
            success: true,
            output: json!({
                "path": path,
                "replacements": if replace_all { count } else { 1 },
                "bytes_written": new_text.len(),
            }),
            error: None,
            execution_time_ms: elapsed_ms,
        })
    }

    /// Write the full contents of a file. New paths skip the read-required
    /// check; existing paths must have been Read first.
    async fn do_write(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let mut owned = serde_json::Value::Null;
        let args = coerce_args(args, &mut owned);
        // If args is still a Value::String here, the provider could not
        // parse the JSON tool call — most often because the LLM hit the
        // output token limit mid-content and the args buffer is
        // truncated. Surface that diagnosis so the model retries by
        // chunking instead of looping on the same too-large write.
        if let Some(raw) = args.as_str() {
            return Ok(args_truncated_result(
                "write",
                raw,
                "split the file into ~3-5KB chunks and emit them across \
                 multiple write/edit calls (write the first chunk, then \
                 use edit with replace_all=false to append the rest)",
                Instant::now(),
            ));
        }
        let path_arg = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AthenError::Other("missing 'path' parameter".to_string()))?;
        let content = args
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AthenError::Other("missing 'content' parameter".to_string()))?;

        let resolved = paths::resolve_in_workspace(Path::new(path_arg));
        let path = resolved.to_string_lossy().to_string();
        let path = path.as_str();

        // Make sure the parent dir exists — relative paths often land in a
        // freshly-resolved workspace subdir that doesn't exist yet.
        if let Some(parent) = resolved.parent() {
            if !parent.as_os_str().is_empty() {
                let _ = tokio::fs::create_dir_all(parent).await;
            }
        }

        tracing::info!(tool = "write", path, "Writing file");
        let start = Instant::now();
        let p = resolved.as_path();

        if let Ok(existing) = tokio::fs::read(p).await {
            self.check_hash(p, &existing).await?;
        }
        // (else: new file — allowed without prior read.)

        atomic_write(p, content.as_bytes()).await?;
        self.record_hash(p, content.as_bytes()).await;

        let elapsed_ms = start.elapsed().as_millis() as u64;
        Ok(ToolResult {
            success: true,
            output: json!({
                "path": path,
                "bytes_written": content.len(),
            }),
            error: None,
            execution_time_ms: elapsed_ms,
        })
    }

    /// Search files using ripgrep.
    async fn do_grep(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let pattern = args
            .get("pattern")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AthenError::Other("missing 'pattern' parameter".to_string()))?;
        // Resolve the search target against the workspace dir so a bare
        // `grep { pattern: "foo" }` searches the agent's own files, not
        // whatever the host process happened to be launched from.
        let path_arg = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
        let resolved_path = paths::resolve_in_workspace(Path::new(path_arg));
        let path = resolved_path.to_string_lossy().to_string();
        let path = path.as_str();
        let glob = args.get("glob").and_then(|v| v.as_str());
        let case_insensitive = args
            .get("case_insensitive")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let line_numbers = args
            .get("line_numbers")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let context_before = args.get("context_before").and_then(|v| v.as_u64());
        let context_after = args.get("context_after").and_then(|v| v.as_u64());
        let output_mode = args
            .get("output_mode")
            .and_then(|v| v.as_str())
            .unwrap_or("files_with_matches");
        let max_count = args
            .get("max_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(100);

        tracing::info!(tool = "grep", pattern, path, output_mode, "ripgrep search");
        let start = Instant::now();

        let mut cmd = tokio::process::Command::new("rg");
        cmd.arg("--color=never");
        if case_insensitive {
            cmd.arg("-i");
        }
        match output_mode {
            "files_with_matches" => {
                cmd.arg("-l");
            }
            "count" => {
                cmd.arg("-c");
            }
            "content" => {
                if line_numbers {
                    cmd.arg("-n");
                }
                if let Some(b) = context_before {
                    cmd.arg("-B").arg(b.to_string());
                }
                if let Some(a) = context_after {
                    cmd.arg("-A").arg(a.to_string());
                }
                // Per-file cap on individual matches is friendlier than a
                // hard global cap when scanning many files.
                cmd.arg("-m").arg(max_count.to_string());
            }
            other => {
                return Err(AthenError::Other(format!(
                    "invalid output_mode '{other}': use files_with_matches, content, or count"
                )));
            }
        }
        if let Some(g) = glob {
            cmd.arg("--glob").arg(g);
        }
        cmd.arg("--").arg(pattern).arg(path);

        let output = match cmd.output().await {
            Ok(o) => o,
            Err(e) => {
                let msg = if e.kind() == std::io::ErrorKind::NotFound {
                    "ripgrep ('rg') not found on PATH — install it from \
                     https://github.com/BurntSushi/ripgrep#installation"
                        .to_string()
                } else {
                    format!("failed to invoke rg: {e}")
                };
                return Ok(ToolResult {
                    success: false,
                    output: json!({ "error": msg }),
                    error: Some(msg),
                    execution_time_ms: start.elapsed().as_millis() as u64,
                });
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        // rg exits 1 when there are no matches — that's not a failure.
        let exit = output.status.code().unwrap_or(-1);
        let success = exit == 0 || exit == 1;

        // Post-truncate the output to `max_count` lines so non-content
        // modes also respect the cap.
        let truncated: String = stdout
            .lines()
            .take(max_count as usize)
            .collect::<Vec<_>>()
            .join("\n");
        let display = if truncated.trim().is_empty() {
            "No matches".to_string()
        } else {
            truncated
        };

        let elapsed_ms = start.elapsed().as_millis() as u64;
        Ok(ToolResult {
            success,
            output: json!({
                "matches": display,
                "exit_code": exit,
            }),
            error: if success {
                None
            } else {
                Some(format!("rg failed (exit {exit}): {}", stderr.trim()))
            },
            execution_time_ms: elapsed_ms,
        })
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

    /// Build the JSON Schema for the `web_search` tool parameters.
    fn web_search_schema() -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Natural-language search query."
                },
                "max_results": {
                    "type": "integer",
                    "description": "Maximum number of hits to return (default 5, max 20)."
                }
            },
            "required": ["query"]
        })
    }

    /// Build the JSON Schema for the `web_fetch` tool parameters.
    fn web_fetch_schema() -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "Absolute http(s) URL to fetch and convert to markdown."
                }
            },
            "required": ["url"]
        })
    }

    /// Run a web search via the configured provider.
    async fn do_web_search(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AthenError::Other("missing 'query' parameter".to_string()))?;
        let max_results = args
            .get("max_results")
            .and_then(|v| v.as_u64())
            .unwrap_or(5)
            .clamp(1, 20) as usize;

        tracing::info!(
            tool = "web_search",
            provider = self.web_search.name(),
            query,
            max_results,
            "Running web search"
        );

        let start = Instant::now();
        match self.web_search.search(query, max_results).await {
            Ok(results) => {
                let json_results: Vec<serde_json::Value> = results
                    .iter()
                    .map(|r| {
                        json!({
                            "title": r.title,
                            "url": r.url,
                            "snippet": r.snippet,
                        })
                    })
                    .collect();
                Ok(ToolResult {
                    success: true,
                    output: json!({
                        "provider": self.web_search.name(),
                        "query": query,
                        "results": json_results,
                    }),
                    error: None,
                    execution_time_ms: start.elapsed().as_millis() as u64,
                })
            }
            Err(e) => {
                let msg = e.to_string();
                Ok(ToolResult {
                    success: false,
                    output: json!({ "error": msg, "provider": self.web_search.name() }),
                    error: Some(msg),
                    execution_time_ms: start.elapsed().as_millis() as u64,
                })
            }
        }
    }

    /// Fetch a URL and convert it to clean markdown.
    async fn do_web_fetch(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let url = args
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AthenError::Other("missing 'url' parameter".to_string()))?;

        // Reject obviously bad inputs early so we don't burn a network round-trip.
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            let msg = format!("web_fetch requires an http(s) URL, got: {url}");
            return Ok(ToolResult {
                success: false,
                output: json!({ "error": msg }),
                error: Some(msg),
                execution_time_ms: 0,
            });
        }

        tracing::info!(
            tool = "web_fetch",
            reader = self.page_reader.name(),
            url,
            "Fetching URL"
        );

        let start = Instant::now();
        match self.page_reader.fetch(url).await {
            Ok(result) => Ok(ToolResult {
                success: true,
                output: json!({
                    "url": result.url,
                    "title": result.title,
                    "content": result.content,
                    "source": result.source,
                    "content_chars": result.content.chars().count(),
                }),
                error: None,
                execution_time_ms: start.elapsed().as_millis() as u64,
            }),
            Err(e) => {
                let msg = e.to_string();
                Ok(ToolResult {
                    success: false,
                    output: json!({ "error": msg, "reader": self.page_reader.name() }),
                    error: Some(msg),
                    execution_time_ms: start.elapsed().as_millis() as u64,
                })
            }
        }
    }

    /// Build the JSON Schema for the `shell_spawn` tool parameters.
    fn shell_spawn_schema() -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The shell command to spawn detached. stdout+stderr are captured to a log file. The process survives this call."
                },
                "label": {
                    "type": "string",
                    "description": "Optional human label (e.g. 'http-server') used in the log file name."
                }
            },
            "required": ["command"]
        })
    }

    /// Build the JSON Schema for the `shell_kill` tool parameters.
    fn shell_kill_schema() -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "pid": {
                    "type": "integer",
                    "description": "PID returned by shell_spawn"
                },
                "force": {
                    "type": "boolean",
                    "description": "Use SIGKILL immediately instead of SIGTERM (default false)"
                }
            },
            "required": ["pid"]
        })
    }

    /// Build the JSON Schema for the `shell_logs` tool parameters.
    fn shell_logs_schema() -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "pid": {
                    "type": "integer",
                    "description": "PID returned by shell_spawn"
                },
                "tail": {
                    "type": "integer",
                    "description": "Number of lines from the end (default 100, max 5000)"
                }
            },
            "required": ["pid"]
        })
    }

    /// Spawn a detached shell command. Returns the PID and a log file path.
    /// The child process outlives this call; use `shell_kill` to stop it
    /// and `shell_logs` to inspect its output.
    ///
    /// NOT routed through the sandbox — bwrap's lifetime is bound to its
    /// child, so sandboxed daemons don't really work. The rule engine still
    /// blocks dangerous commands.
    async fn do_shell_spawn(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let command = args
            .get("command")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AthenError::Other("missing 'command' parameter".to_string()))?;
        let label = args
            .get("label")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        tracing::info!(
            tool = "shell_spawn",
            command,
            ?label,
            "Spawning detached process"
        );
        let start = Instant::now();

        // Same risk check as shell_execute — a long-lived daemon can do as
        // much damage as a one-shot command.
        let risk_ctx = RiskContext {
            trust_level: TrustLevel::AuthUser,
            data_sensitivity: DataSensitivity::Plain,
            llm_confidence: Some(1.0),
            accumulated_risk: 0,
        };
        if let Some(score) = self.rule_engine.evaluate(command, &risk_ctx) {
            if score.level == RiskLevel::Danger || score.level == RiskLevel::Critical {
                tracing::warn!(
                    tool = "shell_spawn",
                    command,
                    risk_score = score.total,
                    risk_level = ?score.level,
                    "Spawn blocked by risk evaluation"
                );
                return Ok(ToolResult {
                    success: false,
                    output: json!({
                        "error": "Command blocked by safety system",
                        "reason": format!(
                            "This command was classified as {:?} risk (score: {:.0}). \
                             It cannot be spawned without explicit user approval.",
                            score.level, score.total
                        ),
                        "command": command,
                    }),
                    error: Some(format!(
                        "Blocked: {:?} risk command (score {:.0})",
                        score.level, score.total
                    )),
                    execution_time_ms: start.elapsed().as_millis() as u64,
                });
            }
        }

        // Pick a log directory under ~/.athen/spawn-logs. Falls back to
        // /tmp/athen-spawn-logs when home isn't resolvable.
        let log_dir = paths::athen_data_dir()
            .map(|p| p.join("spawn-logs"))
            .or_else(|| paths::home_dir().map(|h| h.join(".athen").join("spawn-logs")))
            .unwrap_or_else(|| PathBuf::from("/tmp/athen-spawn-logs"));

        if let Err(e) = tokio::fs::create_dir_all(&log_dir).await {
            let msg = format!("failed to create log dir '{}': {e}", log_dir.display());
            return Ok(ToolResult {
                success: false,
                output: json!({ "error": msg }),
                error: Some(msg),
                execution_time_ms: start.elapsed().as_millis() as u64,
            });
        }

        let timestamp = chrono::Utc::now().format("%Y%m%d-%H%M%S-%f").to_string();
        let stem = label.clone().unwrap_or_else(|| "spawn".to_string());
        // Sanitize label for filesystem use.
        let safe_stem: String = stem
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        let log_path = log_dir.join(format!("{safe_stem}-{timestamp}.log"));

        // Open the log file for stdout, then open it again (append) for
        // stderr so both streams land in the same file. Two distinct OS
        // file handles are simpler/safer than dup'ing.
        let stdout_file = match std::fs::File::create(&log_path) {
            Ok(f) => f,
            Err(e) => {
                let msg = format!("failed to create log file '{}': {e}", log_path.display());
                return Ok(ToolResult {
                    success: false,
                    output: json!({ "error": msg }),
                    error: Some(msg),
                    execution_time_ms: start.elapsed().as_millis() as u64,
                });
            }
        };
        let stderr_file = match std::fs::OpenOptions::new().append(true).open(&log_path) {
            Ok(f) => f,
            Err(e) => {
                let msg = format!(
                    "failed to open log file for stderr '{}': {e}",
                    log_path.display()
                );
                return Ok(ToolResult {
                    success: false,
                    output: json!({ "error": msg }),
                    error: Some(msg),
                    execution_time_ms: start.elapsed().as_millis() as u64,
                });
            }
        };

        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c")
            .arg(command)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::from(stdout_file))
            .stderr(std::process::Stdio::from(stderr_file));
        // Default cwd to the agent workspace so daemons like
        // `python3 -m http.server` serve the agent's own files instead of
        // whatever directory the host process happened to launch from.
        if let Some(ws) = paths::athen_workspace_dir() {
            cmd.current_dir(ws);
        }
        // On Unix, put the child in its own process group so signals to us
        // don't propagate, and so `shell_kill` can target the whole tree.
        #[cfg(unix)]
        cmd.process_group(0);

        let child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                let msg = format!("failed to spawn '{command}': {e}");
                return Ok(ToolResult {
                    success: false,
                    output: json!({ "error": msg }),
                    error: Some(msg),
                    execution_time_ms: start.elapsed().as_millis() as u64,
                });
            }
        };

        let pid = match child.id() {
            Some(p) => p,
            None => {
                let msg = "spawned process has no PID (already exited?)".to_string();
                return Ok(ToolResult {
                    success: false,
                    output: json!({ "error": msg }),
                    error: Some(msg),
                    execution_time_ms: start.elapsed().as_millis() as u64,
                });
            }
        };

        // Drop the Child handle so the OS keeps the process alive after
        // this function returns. We track it via PID + log path instead.
        std::mem::drop(child);

        let entry = SpawnedProcess {
            pid,
            command: command.to_string(),
            label,
            log_path: log_path.clone(),
            started_at: chrono::Utc::now(),
        };
        self.spawned.lock().await.insert(pid, entry);

        let elapsed_ms = start.elapsed().as_millis() as u64;
        Ok(ToolResult {
            success: true,
            output: json!({
                "pid": pid,
                "log_path": log_path.to_string_lossy(),
                "command": command,
            }),
            error: None,
            execution_time_ms: elapsed_ms,
        })
    }

    /// Kill a process previously started via `shell_spawn`. Refuses to
    /// touch PIDs that aren't in our map.
    async fn do_shell_kill(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let pid_i64 = args
            .get("pid")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| AthenError::Other("missing 'pid' parameter".to_string()))?;
        if pid_i64 <= 0 || pid_i64 > u32::MAX as i64 {
            return Err(AthenError::Other(format!("invalid pid: {pid_i64}")));
        }
        let pid: u32 = pid_i64 as u32;
        let force = args.get("force").and_then(|v| v.as_bool()).unwrap_or(false);

        tracing::info!(tool = "shell_kill", pid, force, "Killing spawned process");
        let start = Instant::now();

        // Safety boundary: we only kill PIDs we spawned ourselves.
        {
            let map = self.spawned.lock().await;
            if !map.contains_key(&pid) {
                let msg = "PID not managed by shell_spawn; refusing to kill arbitrary processes"
                    .to_string();
                return Ok(ToolResult {
                    success: false,
                    output: json!({ "error": &msg, "pid": pid }),
                    error: Some(msg),
                    execution_time_ms: start.elapsed().as_millis() as u64,
                });
            }
        }

        #[cfg(unix)]
        let signal_used = {
            use nix::sys::signal::{kill, Signal};
            use nix::unistd::Pid;

            // Negative PID = process group, since we set process_group(0).
            let pgid = Pid::from_raw(-(pid as i32));
            let direct = Pid::from_raw(pid as i32);

            let initial_sig = if force {
                Signal::SIGKILL
            } else {
                Signal::SIGTERM
            };
            // Best-effort: try the group first, then the bare PID as a
            // fallback (group send fails if we're not the group leader).
            let _ = kill(pgid, initial_sig);
            let _ = kill(direct, initial_sig);

            if force {
                "SIGKILL"
            } else {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                // Probe with signal 0 (no-op) to see if it's still alive.
                let alive = kill(direct, None).is_ok();
                if alive {
                    let _ = kill(pgid, Signal::SIGKILL);
                    let _ = kill(direct, Signal::SIGKILL);
                    "SIGKILL"
                } else {
                    "SIGTERM"
                }
            }
        };

        #[cfg(windows)]
        let signal_used = {
            let mut cmd = tokio::process::Command::new("taskkill");
            cmd.arg("/PID").arg(pid.to_string()).arg("/T");
            if force {
                cmd.arg("/F");
            }
            let _ = cmd.output().await;
            "taskkill"
        };

        self.spawned.lock().await.remove(&pid);

        let elapsed_ms = start.elapsed().as_millis() as u64;
        Ok(ToolResult {
            success: true,
            output: json!({ "pid": pid, "signal": signal_used }),
            error: None,
            execution_time_ms: elapsed_ms,
        })
    }

    /// Read recent stdout/stderr from a process spawned via `shell_spawn`.
    async fn do_shell_logs(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let pid_i64 = args
            .get("pid")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| AthenError::Other("missing 'pid' parameter".to_string()))?;
        if pid_i64 <= 0 || pid_i64 > u32::MAX as i64 {
            return Err(AthenError::Other(format!("invalid pid: {pid_i64}")));
        }
        let pid: u32 = pid_i64 as u32;
        let tail = args
            .get("tail")
            .and_then(|v| v.as_u64())
            .unwrap_or(100)
            .min(5000) as usize;

        tracing::info!(tool = "shell_logs", pid, tail, "Reading spawn logs");
        let start = Instant::now();

        let entry = {
            let map = self.spawned.lock().await;
            match map.get(&pid) {
                Some(e) => e.clone(),
                None => {
                    let msg = "PID not managed by shell_spawn".to_string();
                    return Ok(ToolResult {
                        success: false,
                        output: json!({ "error": &msg, "pid": pid }),
                        error: Some(msg),
                        execution_time_ms: start.elapsed().as_millis() as u64,
                    });
                }
            }
        };

        let contents = match tokio::fs::read_to_string(&entry.log_path).await {
            Ok(c) => c,
            Err(e) => {
                let msg = format!(
                    "failed to read log file '{}': {e}",
                    entry.log_path.display()
                );
                return Ok(ToolResult {
                    success: false,
                    output: json!({ "error": msg, "log_path": entry.log_path.to_string_lossy() }),
                    error: Some(msg),
                    execution_time_ms: start.elapsed().as_millis() as u64,
                });
            }
        };

        let all_lines: Vec<&str> = contents.lines().collect();
        let total = all_lines.len();
        let take = total.min(tail);
        let logs: String = all_lines[total - take..].join("\n");

        let alive = process_alive(pid);

        let elapsed_ms = start.elapsed().as_millis() as u64;
        Ok(ToolResult {
            success: true,
            output: json!({
                "logs": logs,
                "log_path": entry.log_path.to_string_lossy(),
                "alive": alive,
                "lines_returned": take,
                "total_lines": total,
            }),
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
        tracing::trace!(
            tool = "list_directory",
            "Filesystem tools use tokio::fs directly (unsandboxed)"
        );

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
                description: "Run a shell command and return its output (stdout, stderr, exit code). For fetching web pages or searching the web do NOT use curl/wget/lynx — use web_fetch and web_search, which return clean markdown/snippets.".to_string(),
                parameters: Self::shell_execute_schema(),
                backend: ToolBackend::Shell {
                    command: String::new(),
                    native: false,
                },
                base_risk: BaseImpact::WritePersist,
            },
            ToolDefinition {
                name: "read".to_string(),
                description: "Read a file with optional offset/limit. Returns lines numbered cat -n style. ALWAYS use this instead of cat/head/tail.".to_string(),
                parameters: Self::read_schema(),
                backend: ToolBackend::Shell {
                    command: String::new(),
                    native: false,
                },
                base_risk: BaseImpact::Read,
            },
            ToolDefinition {
                name: "edit".to_string(),
                description: "Replace exact text in a file. Requires a prior read. Prefer this over sed/awk.".to_string(),
                parameters: Self::edit_schema(),
                backend: ToolBackend::Shell {
                    command: String::new(),
                    native: false,
                },
                base_risk: BaseImpact::WritePersist,
            },
            ToolDefinition {
                name: "write".to_string(),
                description: "Overwrite or create a file. For partial changes prefer edit. Existing files require a prior read.".to_string(),
                parameters: Self::write_schema(),
                backend: ToolBackend::Shell {
                    command: String::new(),
                    native: false,
                },
                base_risk: BaseImpact::WritePersist,
            },
            ToolDefinition {
                name: "grep".to_string(),
                description: "Search files with ripgrep. Use this instead of grep/find for code search.".to_string(),
                parameters: Self::grep_schema(),
                backend: ToolBackend::Shell {
                    command: String::new(),
                    native: false,
                },
                base_risk: BaseImpact::Read,
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
                name: "shell_spawn".to_string(),
                description: "Spawn a long-lived shell command detached (servers, watchers). Returns a PID and a log file path. The process outlives this call. NOT sandboxed; the rule engine still blocks dangerous commands. Use shell_kill to stop it and shell_logs to inspect output.".to_string(),
                parameters: Self::shell_spawn_schema(),
                backend: ToolBackend::Shell {
                    command: String::new(),
                    native: false,
                },
                base_risk: BaseImpact::WritePersist,
            },
            ToolDefinition {
                name: "shell_kill".to_string(),
                description: "Kill a process previously started by shell_spawn. Refuses unmanaged PIDs. Sends SIGTERM, then SIGKILL if it doesn't exit (or SIGKILL immediately when force=true).".to_string(),
                parameters: Self::shell_kill_schema(),
                backend: ToolBackend::Shell {
                    command: String::new(),
                    native: false,
                },
                base_risk: BaseImpact::WritePersist,
            },
            ToolDefinition {
                name: "shell_logs".to_string(),
                description: "Read the last N lines (default 100, max 5000) of stdout/stderr from a process spawned via shell_spawn. Also reports whether the process is still alive.".to_string(),
                parameters: Self::shell_logs_schema(),
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
            ToolDefinition {
                name: "web_search".to_string(),
                description: "Search the web and return ranked hits (title, url, snippet). Use this for current/factual info the model may not know, or when web_fetch returns near-empty content on a JS-heavy site — the snippets are often enough to answer.".to_string(),
                parameters: Self::web_search_schema(),
                backend: ToolBackend::Shell {
                    command: String::new(),
                    native: false,
                },
                base_risk: BaseImpact::Read,
            },
            ToolDefinition {
                name: "web_fetch".to_string(),
                description: "Fetch a URL and return its readable content as markdown. Use this instead of curl/wget for any web page. Auto-falls-back through a static fetch → JS-rendering reader → web archive, so JS-heavy SPAs and paywalled/blocked pages are usually still readable. The `source` field in the result tells you which tier produced the content (`local-*`, `jina`, or `wayback`).".to_string(),
                parameters: Self::web_fetch_schema(),
                backend: ToolBackend::Shell {
                    command: String::new(),
                    native: false,
                },
                base_risk: BaseImpact::Read,
            },
        ])
    }

    async fn call_tool(&self, name: &str, args: serde_json::Value) -> Result<ToolResult> {
        match name {
            "shell_execute" => self.do_shell_execute(&args).await,
            "shell_spawn" => self.do_shell_spawn(&args).await,
            "shell_kill" => self.do_shell_kill(&args).await,
            "shell_logs" => self.do_shell_logs(&args).await,
            "read" => self.do_read(&args).await,
            "edit" => self.do_edit(&args).await,
            "write" => self.do_write(&args).await,
            "grep" => self.do_grep(&args).await,
            "list_directory" => self.do_list_directory(&args).await,
            "memory_store" => self.do_memory_store(&args).await,
            "memory_recall" => self.do_memory_recall(&args).await,
            "web_search" => self.do_web_search(&args).await,
            "web_fetch" => self.do_web_fetch(&args).await,
            _ => Err(AthenError::ToolNotFound(name.to_string())),
        }
    }
}

/// Best-effort liveness check for a PID. On Unix this uses `kill(pid, 0)`
/// which doesn't actually send a signal — it just probes whether the
/// kernel still has a process by that PID we're allowed to signal.
#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    use nix::sys::signal::kill;
    use nix::unistd::Pid;
    kill(Pid::from_raw(pid as i32), None).is_ok()
}

#[cfg(windows)]
fn process_alive(pid: u32) -> bool {
    // Cheap probe via tasklist; sync std::process to keep this helper sync.
    let out = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .output();
    match out {
        Ok(o) => {
            let s = String::from_utf8_lossy(&o.stdout);
            s.contains(&pid.to_string())
        }
        Err(_) => false,
    }
}

/// Write `bytes` to `path` atomically: write to a sibling tmp file then
/// rename. Falls back to a direct write when the rename can't happen
/// (e.g. tmp dir on a different filesystem). Used by `edit` and `write`.
async fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = match path.file_name() {
        Some(name) => {
            let mut tmp_name = std::ffi::OsString::from(".athen-tmp-");
            tmp_name.push(name);
            tmp_name.push(format!(".{}", std::process::id()));
            path.with_file_name(tmp_name)
        }
        None => {
            return Err(AthenError::Other(format!(
                "invalid file path '{}'",
                path.display()
            )))
        }
    };

    if let Err(e) = tokio::fs::write(&tmp, bytes).await {
        // If the directory doesn't exist, surface that directly.
        return Err(AthenError::Other(format!(
            "failed to write tmp file '{}': {}",
            tmp.display(),
            e
        )));
    }
    if let Err(e) = tokio::fs::rename(&tmp, path).await {
        // Best-effort cleanup, then fall back to a direct write so
        // callers on quirky filesystems aren't dead in the water.
        let _ = tokio::fs::remove_file(&tmp).await;
        tokio::fs::write(path, bytes).await.map_err(|e2| {
            AthenError::Other(format!(
                "atomic rename failed ({e}); fallback write also failed: {e2}"
            ))
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod coerce_args_tests {
    use super::*;

    #[test]
    fn coerce_args_passes_through_objects_unchanged() {
        let v = serde_json::json!({"path": "/tmp/x"});
        let mut owned = serde_json::Value::Null;
        let coerced = coerce_args(&v, &mut owned);
        assert_eq!(coerced["path"], "/tmp/x");
    }

    #[test]
    fn coerce_args_parses_json_strings() {
        let v = serde_json::Value::String(r#"{"path":"/tmp/x","content":"hi"}"#.into());
        let mut owned = serde_json::Value::Null;
        let coerced = coerce_args(&v, &mut owned);
        assert_eq!(coerced["path"], "/tmp/x");
        assert_eq!(coerced["content"], "hi");
    }

    #[test]
    fn coerce_args_returns_original_when_string_is_not_json() {
        let v = serde_json::Value::String("just a string".into());
        let mut owned = serde_json::Value::Null;
        let coerced = coerce_args(&v, &mut owned);
        // Still a string — caller will surface a "missing 'path'" error.
        assert!(coerced.is_string());
    }

    #[test]
    fn do_write_via_coerce_extracts_path_from_stringified_args() {
        // Simulates the bad path: provider couldn't parse args, fell
        // back to Value::String. The coerce helper must still let us
        // pull `path` and `content` out.
        let stringified =
            serde_json::Value::String(r#"{"path":"/tmp/coerce_x.html","content":"hello"}"#.into());
        let mut owned = serde_json::Value::Null;
        let coerced = coerce_args(&stringified, &mut owned);
        assert_eq!(
            coerced.get("path").and_then(|v| v.as_str()),
            Some("/tmp/coerce_x.html")
        );
        assert_eq!(
            coerced.get("content").and_then(|v| v.as_str()),
            Some("hello")
        );
    }

    #[test]
    fn args_truncated_result_emits_actionable_message() {
        // Truncated raw payload (no closing quote/brace) — exactly what
        // the user hit when DeepSeek ran out of output tokens mid-write.
        let raw = "{\"path\":\"/tmp/x.html\",\"content\":\"<!DOCTYPE html>\n<html>...mid string";
        let result = args_truncated_result("write", raw, "chunk via edit", Instant::now());
        assert!(!result.success);
        let err = result.error.unwrap_or_default();
        assert!(err.contains("write"));
        assert!(err.contains("token limit") || err.contains("truncated"));
        assert!(err.contains("chunk via edit"));
        // Output JSON exposes raw_len + head/tail for the LLM to see
        // the cut-off shape and reason about how to retry.
        assert!(result.output.get("raw_len").is_some());
        assert!(result.output.get("raw_head").is_some());
        assert!(result.output.get("raw_tail").is_some());
    }

    #[test]
    fn coerce_repairs_raw_control_chars_in_stringified_args() {
        // The actual production failure mode: DeepSeek emits args as a
        // string with literal newlines inside the content value.
        // serde_json::from_str rejects per spec; coerce_args must
        // re-escape and parse.
        let raw =
            "{\"path\":\"/tmp/coerce_y.html\",\"content\":\"<html>\nhi\n</html>\"}".to_string();
        let stringified = serde_json::Value::String(raw);
        let mut owned = serde_json::Value::Null;
        let coerced = coerce_args(&stringified, &mut owned);
        assert_eq!(
            coerced.get("path").and_then(|v| v.as_str()),
            Some("/tmp/coerce_y.html")
        );
        assert_eq!(
            coerced.get("content").and_then(|v| v.as_str()),
            Some("<html>\nhi\n</html>")
        );
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

        // shell_execute, shell_spawn, shell_kill, shell_logs, read, edit,
        // write, grep, list_directory, memory_store, memory_recall,
        // web_search, web_fetch
        assert_eq!(tools.len(), 13);

        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"shell_execute"));
        assert!(names.contains(&"shell_spawn"));
        assert!(names.contains(&"shell_kill"));
        assert!(names.contains(&"shell_logs"));
        assert!(names.contains(&"read"));
        assert!(names.contains(&"edit"));
        assert!(names.contains(&"write"));
        assert!(names.contains(&"grep"));
        assert!(names.contains(&"list_directory"));
        assert!(names.contains(&"memory_store"));
        assert!(names.contains(&"memory_recall"));
        assert!(names.contains(&"web_search"));
        assert!(names.contains(&"web_fetch"));

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
    async fn test_read_basic_numbers_lines() {
        let mut tmp = NamedTempFile::new().unwrap();
        writeln!(tmp, "alpha").unwrap();
        writeln!(tmp, "beta").unwrap();
        writeln!(tmp, "gamma").unwrap();
        let path = tmp.path().to_str().unwrap().to_string();

        let registry = ShellToolRegistry::new().await;
        let result = registry
            .call_tool("read", json!({"path": path}))
            .await
            .unwrap();

        assert!(result.success);
        let content = result.output["content"].as_str().unwrap();
        assert!(content.contains("1\talpha"));
        assert!(content.contains("2\tbeta"));
        assert!(content.contains("3\tgamma"));
        assert_eq!(result.output["lines_returned"], 3);
        assert_eq!(result.output["total_lines"], 3);
    }

    #[tokio::test]
    async fn test_read_with_offset_and_limit() {
        let mut tmp = NamedTempFile::new().unwrap();
        for i in 1..=10 {
            writeln!(tmp, "line{i}").unwrap();
        }
        let path = tmp.path().to_str().unwrap().to_string();

        let registry = ShellToolRegistry::new().await;
        let result = registry
            .call_tool("read", json!({"path": path, "offset": 4, "limit": 2}))
            .await
            .unwrap();

        assert!(result.success);
        let content = result.output["content"].as_str().unwrap();
        assert!(content.contains("4\tline4"));
        assert!(content.contains("5\tline5"));
        assert!(!content.contains("3\tline3"));
        assert!(!content.contains("6\tline6"));
        assert_eq!(result.output["lines_returned"], 2);
    }

    #[tokio::test]
    async fn test_read_rejects_directory() {
        let dir = TempDir::new().unwrap();
        let registry = ShellToolRegistry::new().await;
        let result = registry
            .call_tool("read", json!({"path": dir.path().to_str().unwrap()}))
            .await
            .unwrap();
        assert!(!result.success);
        let err = result.error.unwrap();
        assert!(err.contains("directory"));
    }

    #[tokio::test]
    async fn test_read_rejects_binary_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("binary.bin");
        std::fs::write(&path, [0x48, 0x00, 0x49]).unwrap();

        let registry = ShellToolRegistry::new().await;
        let result = registry
            .call_tool("read", json!({"path": path.to_str().unwrap()}))
            .await
            .unwrap();
        assert!(!result.success);
        let err = result.error.unwrap();
        assert!(err.contains("binary"));
    }

    #[tokio::test]
    async fn test_read_not_found() {
        let registry = ShellToolRegistry::new().await;
        let result = registry
            .call_tool("read", json!({"path": "/tmp/__athen_nonexistent_file__"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.is_some());
    }

    #[tokio::test]
    async fn test_edit_rejects_without_prior_read() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("e.txt");
        std::fs::write(&path, "hello world").unwrap();

        let registry = ShellToolRegistry::new().await;
        let result = registry
            .call_tool(
                "edit",
                json!({
                    "path": path.to_str().unwrap(),
                    "old_string": "hello",
                    "new_string": "goodbye",
                }),
            )
            .await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.to_lowercase().contains("read this file first"));
    }

    #[tokio::test]
    async fn test_edit_rejects_on_external_change() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("e.txt");
        std::fs::write(&path, "version1").unwrap();

        let registry = ShellToolRegistry::new().await;
        // Read first to record the hash.
        registry
            .call_tool("read", json!({"path": path.to_str().unwrap()}))
            .await
            .unwrap();

        // External modification.
        std::fs::write(&path, "version2").unwrap();

        let result = registry
            .call_tool(
                "edit",
                json!({
                    "path": path.to_str().unwrap(),
                    "old_string": "version2",
                    "new_string": "version3",
                }),
            )
            .await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("changed on disk"));
    }

    #[tokio::test]
    async fn test_edit_rejects_ambiguous_old_string() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("e.txt");
        std::fs::write(&path, "foo bar foo bar foo").unwrap();

        let registry = ShellToolRegistry::new().await;
        registry
            .call_tool("read", json!({"path": path.to_str().unwrap()}))
            .await
            .unwrap();

        let result = registry
            .call_tool(
                "edit",
                json!({
                    "path": path.to_str().unwrap(),
                    "old_string": "foo",
                    "new_string": "qux",
                }),
            )
            .await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("matches 3 times"));
    }

    #[tokio::test]
    async fn test_edit_replace_all() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("e.txt");
        std::fs::write(&path, "foo bar foo bar foo").unwrap();

        let registry = ShellToolRegistry::new().await;
        registry
            .call_tool("read", json!({"path": path.to_str().unwrap()}))
            .await
            .unwrap();

        let result = registry
            .call_tool(
                "edit",
                json!({
                    "path": path.to_str().unwrap(),
                    "old_string": "foo",
                    "new_string": "qux",
                    "replace_all": true,
                }),
            )
            .await
            .unwrap();
        assert!(result.success);
        assert_eq!(result.output["replacements"], 3);

        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(content, "qux bar qux bar qux");
    }

    #[tokio::test]
    async fn test_edit_unique_replace() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("e.txt");
        std::fs::write(&path, "alpha beta gamma").unwrap();

        let registry = ShellToolRegistry::new().await;
        registry
            .call_tool("read", json!({"path": path.to_str().unwrap()}))
            .await
            .unwrap();

        let result = registry
            .call_tool(
                "edit",
                json!({
                    "path": path.to_str().unwrap(),
                    "old_string": "beta",
                    "new_string": "BETA",
                }),
            )
            .await
            .unwrap();
        assert!(result.success);

        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(content, "alpha BETA gamma");
    }

    #[tokio::test]
    async fn test_edit_then_edit_uses_updated_hash() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("e.txt");
        std::fs::write(&path, "v1").unwrap();

        let registry = ShellToolRegistry::new().await;
        registry
            .call_tool("read", json!({"path": path.to_str().unwrap()}))
            .await
            .unwrap();
        registry
            .call_tool(
                "edit",
                json!({
                    "path": path.to_str().unwrap(),
                    "old_string": "v1",
                    "new_string": "v2",
                }),
            )
            .await
            .unwrap();
        // Second edit without re-reading — should still succeed because
        // the first edit updated the recorded hash.
        let result = registry
            .call_tool(
                "edit",
                json!({
                    "path": path.to_str().unwrap(),
                    "old_string": "v2",
                    "new_string": "v3",
                }),
            )
            .await
            .unwrap();
        assert!(result.success);
    }

    #[tokio::test]
    async fn test_write_new_file_no_read_required() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("brand_new.txt");

        let registry = ShellToolRegistry::new().await;
        let result = registry
            .call_tool(
                "write",
                json!({"path": path.to_str().unwrap(), "content": "hi"}),
            )
            .await
            .unwrap();
        assert!(result.success);
        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(content, "hi");
    }

    #[tokio::test]
    async fn test_write_existing_file_requires_read() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("existing.txt");
        std::fs::write(&path, "original").unwrap();

        let registry = ShellToolRegistry::new().await;
        let result = registry
            .call_tool(
                "write",
                json!({"path": path.to_str().unwrap(), "content": "overwritten"}),
            )
            .await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.to_lowercase().contains("read this file first"));
    }

    #[tokio::test]
    async fn test_write_existing_file_after_read_succeeds() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("existing.txt");
        std::fs::write(&path, "original").unwrap();

        let registry = ShellToolRegistry::new().await;
        registry
            .call_tool("read", json!({"path": path.to_str().unwrap()}))
            .await
            .unwrap();
        let result = registry
            .call_tool(
                "write",
                json!({"path": path.to_str().unwrap(), "content": "overwritten"}),
            )
            .await
            .unwrap();
        assert!(result.success);
        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(content, "overwritten");
    }

    #[tokio::test]
    async fn test_grep_basic_match() {
        if which::which("rg").is_err() {
            // Skip if ripgrep isn't installed in this environment.
            eprintln!("skipping: rg not on PATH");
            return;
        }
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("a.txt"),
            "needle in a haystack\nother line\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("b.txt"), "no match here\n").unwrap();

        let registry = ShellToolRegistry::new().await;
        let result = registry
            .call_tool(
                "grep",
                json!({
                    "pattern": "needle",
                    "path": dir.path().to_str().unwrap(),
                    "output_mode": "files_with_matches",
                }),
            )
            .await
            .unwrap();
        assert!(result.success);
        let matches = result.output["matches"].as_str().unwrap();
        assert!(matches.contains("a.txt"));
        assert!(!matches.contains("b.txt"));
    }

    #[tokio::test]
    async fn test_grep_no_matches_returns_text() {
        if which::which("rg").is_err() {
            eprintln!("skipping: rg not on PATH");
            return;
        }
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();

        let registry = ShellToolRegistry::new().await;
        let result = registry
            .call_tool(
                "grep",
                json!({
                    "pattern": "zzznotthere",
                    "path": dir.path().to_str().unwrap(),
                }),
            )
            .await
            .unwrap();
        assert!(result.success);
        assert_eq!(result.output["matches"].as_str().unwrap(), "No matches");
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

        let names: Vec<&str> = entries
            .iter()
            .map(|e| e["name"].as_str().unwrap())
            .collect();
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
    async fn test_shell_execute_default_timeout_is_60s() {
        // Schema description should advertise the new default.
        let schema = ShellToolRegistry::shell_execute_schema();
        let desc = schema["properties"]["timeout_ms"]["description"]
            .as_str()
            .unwrap();
        assert!(
            desc.contains("60000"),
            "schema desc should mention 60000ms default: {desc}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_shell_spawn_returns_pid_and_log_path() {
        let registry = ShellToolRegistry::new().await;
        let result = registry
            .call_tool(
                "shell_spawn",
                json!({"command": "sleep 5", "label": "test-sleep"}),
            )
            .await
            .unwrap();
        assert!(result.success, "spawn failed: {:?}", result.error);
        let pid = result.output["pid"].as_u64().unwrap();
        assert!(pid > 0);
        let log_path = result.output["log_path"].as_str().unwrap();
        assert!(std::path::Path::new(log_path).exists());

        // Cleanup.
        let _ = registry
            .call_tool("shell_kill", json!({"pid": pid, "force": true}))
            .await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_shell_kill_rejects_unmanaged_pid() {
        let registry = ShellToolRegistry::new().await;
        // PID 1 is init — definitely not in our map.
        let result = registry
            .call_tool("shell_kill", json!({"pid": 1}))
            .await
            .unwrap();
        assert!(!result.success);
        let err = result.error.unwrap();
        assert!(
            err.to_lowercase().contains("not managed"),
            "expected refusal, got: {err}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_shell_kill_terminates_spawned_process() {
        use nix::sys::signal::kill;
        use nix::unistd::Pid;

        let registry = ShellToolRegistry::new().await;
        let spawn = registry
            .call_tool(
                "shell_spawn",
                json!({"command": "sleep 30", "label": "kill-target"}),
            )
            .await
            .unwrap();
        assert!(spawn.success);
        let pid = spawn.output["pid"].as_u64().unwrap() as u32;

        // Confirm it's alive before kill.
        assert!(kill(Pid::from_raw(pid as i32), None).is_ok());

        let kill_result = registry
            .call_tool("shell_kill", json!({"pid": pid, "force": true}))
            .await
            .unwrap();
        assert!(kill_result.success, "kill failed: {:?}", kill_result.error);

        // Give the kernel a moment to reap.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(
            kill(Pid::from_raw(pid as i32), None).is_err(),
            "process {pid} should be gone after SIGKILL"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_shell_logs_returns_content() {
        let registry = ShellToolRegistry::new().await;
        let spawn = registry
            .call_tool(
                "shell_spawn",
                json!({"command": "sh -c 'echo hi; sleep 1'", "label": "log-test"}),
            )
            .await
            .unwrap();
        assert!(spawn.success);
        let pid = spawn.output["pid"].as_u64().unwrap();

        // Wait for the echo + sleep to finish.
        tokio::time::sleep(std::time::Duration::from_millis(1300)).await;

        let logs = registry
            .call_tool("shell_logs", json!({"pid": pid, "tail": 50}))
            .await
            .unwrap();
        assert!(logs.success);
        let log_text = logs.output["logs"].as_str().unwrap();
        assert!(log_text.contains("hi"), "expected 'hi' in logs: {log_text}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_shell_logs_reports_alive_false_after_exit() {
        let registry = ShellToolRegistry::new().await;
        let spawn = registry
            .call_tool(
                "shell_spawn",
                json!({"command": "sh -c 'echo done'", "label": "exit-fast"}),
            )
            .await
            .unwrap();
        let pid = spawn.output["pid"].as_u64().unwrap();

        // Give it time to exit and be reaped by the OS. Spawn dropped the
        // Child handle, so the process becomes a zombie until init reaps it
        // — but `kill(pid, 0)` returns ESRCH once the entry is fully gone.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        let logs = registry
            .call_tool("shell_logs", json!({"pid": pid}))
            .await
            .unwrap();
        assert!(logs.success);
        // Note: a freshly-exited child might briefly still appear "alive" as
        // a zombie. Poll a bit before asserting.
        let mut alive = logs.output["alive"].as_bool().unwrap();
        for _ in 0..10 {
            if !alive {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            let again = registry
                .call_tool("shell_logs", json!({"pid": pid}))
                .await
                .unwrap();
            alive = again.output["alive"].as_bool().unwrap();
        }
        assert!(!alive, "process should not be alive after exit");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_spawned_map_is_shareable_across_registries() {
        // Simulates the bug fix: turn 1's registry spawns a process,
        // turn 2's registry — built fresh but with the same shared map —
        // can find it and kill it. Without `with_spawned_processes`, the
        // second registry would have an empty map and refuse the kill.
        let shared: SpawnedProcessMap = Arc::new(Mutex::new(HashMap::new()));

        let reg1 = ShellToolRegistry::new()
            .await
            .with_spawned_processes(shared.clone());
        let reg2 = ShellToolRegistry::new()
            .await
            .with_spawned_processes(shared.clone());

        let spawn = reg1
            .call_tool(
                "shell_spawn",
                json!({"command": "sleep 30", "label": "cross-registry"}),
            )
            .await
            .unwrap();
        assert!(spawn.success, "spawn failed: {:?}", spawn.error);
        let pid = spawn.output["pid"].as_u64().unwrap() as u32;

        // The shared map is visible to both.
        assert!(
            shared.lock().await.contains_key(&pid),
            "shared map should contain spawned PID"
        );

        // reg2 (a *different* registry instance) can kill the process
        // spawned by reg1 — proving the map is actually shared, not just
        // that an Arc was stored.
        let kill_result = reg2
            .call_tool("shell_kill", json!({"pid": pid, "force": true}))
            .await
            .unwrap();
        assert!(
            kill_result.success,
            "second registry could not kill PID spawned by first: {:?}",
            kill_result.error
        );

        // After kill, both views agree the entry is gone.
        assert!(!shared.lock().await.contains_key(&pid));
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

    // ── Web tool wiring ────────────────────────────────────────────────────
    //
    // These stubs let us verify dispatch + output envelope shape without any
    // network access. The real backends (DuckDuckGo, HybridReader, ...) have
    // their own coverage in `athen-web`.

    use athen_core::error::{AthenError, Result as CoreResult};
    use athen_web::{PageReader, ReadResult, SearchResult, WebSearchProvider};

    struct StubSearch {
        results: Vec<SearchResult>,
    }

    #[async_trait]
    impl WebSearchProvider for StubSearch {
        fn name(&self) -> &'static str {
            "stub-search"
        }
        async fn search(&self, _query: &str, max_results: usize) -> CoreResult<Vec<SearchResult>> {
            Ok(self.results.iter().take(max_results).cloned().collect())
        }
    }

    struct StubReader {
        outcome: std::result::Result<ReadResult, String>,
    }

    #[async_trait]
    impl PageReader for StubReader {
        fn name(&self) -> &'static str {
            "stub-reader"
        }
        async fn fetch(&self, _url: &str) -> CoreResult<ReadResult> {
            self.outcome.clone().map_err(AthenError::Other)
        }
    }

    #[tokio::test]
    async fn web_search_dispatches_to_provider_and_caps_results() {
        let stub = StubSearch {
            results: vec![
                SearchResult {
                    title: "First".into(),
                    url: "https://a.example/".into(),
                    snippet: "snippet a".into(),
                },
                SearchResult {
                    title: "Second".into(),
                    url: "https://b.example/".into(),
                    snippet: "snippet b".into(),
                },
                SearchResult {
                    title: "Third".into(),
                    url: "https://c.example/".into(),
                    snippet: "snippet c".into(),
                },
            ],
        };
        let registry = ShellToolRegistry::new()
            .await
            .with_web_search(Arc::new(stub));

        let result = registry
            .call_tool("web_search", json!({ "query": "rust", "max_results": 2 }))
            .await
            .unwrap();

        assert!(result.success);
        assert_eq!(result.output["provider"], "stub-search");
        let arr = result.output["results"].as_array().unwrap();
        assert_eq!(arr.len(), 2, "max_results must be honored");
        assert_eq!(arr[0]["title"], "First");
        assert_eq!(arr[0]["url"], "https://a.example/");
        assert_eq!(arr[0]["snippet"], "snippet a");
    }

    #[tokio::test]
    async fn web_search_clamps_max_results_to_safe_range() {
        let stub = StubSearch {
            results: Vec::new(),
        };
        let registry = ShellToolRegistry::new()
            .await
            .with_web_search(Arc::new(stub));

        // 0 must be raised to >=1, 999 must be clamped to <=20. The stub
        // returns empty either way; success is what we're checking — a
        // panic from `clamp` on an out-of-range max would surface here.
        let r = registry
            .call_tool("web_search", json!({ "query": "x", "max_results": 0 }))
            .await
            .unwrap();
        assert!(r.success);

        let r = registry
            .call_tool("web_search", json!({ "query": "x", "max_results": 999 }))
            .await
            .unwrap();
        assert!(r.success);
    }

    #[tokio::test]
    async fn web_fetch_dispatches_to_reader_and_emits_full_envelope() {
        let stub = StubReader {
            outcome: Ok(ReadResult {
                url: "https://example.com/".into(),
                title: Some("Hello".into()),
                content: "# Hello\n\nbody text".into(),
                source: "stub-reader".into(),
            }),
        };
        let registry = ShellToolRegistry::new()
            .await
            .with_page_reader(Arc::new(stub));

        let result = registry
            .call_tool("web_fetch", json!({ "url": "https://example.com/" }))
            .await
            .unwrap();

        assert!(result.success);
        assert_eq!(result.output["url"], "https://example.com/");
        assert_eq!(result.output["title"], "Hello");
        assert_eq!(result.output["source"], "stub-reader");
        assert_eq!(result.output["content"], "# Hello\n\nbody text");
        assert_eq!(result.output["content_chars"], 18);
    }

    #[tokio::test]
    async fn web_fetch_rejects_non_http_url_without_calling_reader() {
        // Reader would error if invoked — proves the URL guard short-circuits
        // before any network round-trip.
        let stub = StubReader {
            outcome: Err("reader must not be called".into()),
        };
        let registry = ShellToolRegistry::new()
            .await
            .with_page_reader(Arc::new(stub));

        let result = registry
            .call_tool("web_fetch", json!({ "url": "ftp://nope/" }))
            .await
            .unwrap();

        assert!(!result.success);
        let err = result.error.as_deref().unwrap_or("");
        assert!(
            err.contains("http(s)"),
            "expected scheme guard error, got: {err}"
        );
    }

    #[tokio::test]
    async fn web_fetch_surfaces_reader_error_as_soft_failure() {
        let stub = StubReader {
            outcome: Err("simulated reader failure".into()),
        };
        let registry = ShellToolRegistry::new()
            .await
            .with_page_reader(Arc::new(stub));

        let result = registry
            .call_tool("web_fetch", json!({ "url": "https://example.com/" }))
            .await
            .unwrap();

        assert!(!result.success);
        let err = result.error.as_deref().unwrap_or("");
        assert!(err.contains("simulated reader failure"));
        // The reader name is reported so the agent can see which tier failed.
        assert_eq!(result.output["reader"], "stub-reader");
    }
}
