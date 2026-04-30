//! Path-based permission gate for file-touching tools.
//!
//! Sits between the agent's tool calls and the underlying executors
//! (built-in tokio::fs ops, Files MCP). Every call carrying a path is
//! routed through `PathRiskEvaluator`, which classifies the access into
//! one of four bands; the gate then either runs the operation, asks the
//! user via `pending_grants`, or rejects it outright.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde::Serialize;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter};
use tokio::sync::{oneshot, Mutex};
use uuid::Uuid;

use athen_agent::tools::ShellExtraWritableProvider;
use athen_core::contact::TrustLevel;
use athen_core::error::{AthenError, Result};
use athen_core::traits::approval::ApprovalSink;
use athen_core::paths;
use athen_core::risk::{DataSensitivity, RiskContext, RiskDecision};
use athen_core::tool::ToolResult;
use athen_persistence::grants::{Access, GrantStore};
use athen_risk::path_eval::{GrantLookup, PathAccess, PathRiskEvaluator};

/// Stable namespace UUID used to derive a `Uuid` from the human-readable
/// arc identifier strings (`arc_YYYYMMDD_HHMMSS`). Uses UUID v5 (SHA-1)
/// so the mapping is portable across machines and stable across runs.
const ARC_NAMESPACE: Uuid = Uuid::from_bytes([
    0x8a, 0x3f, 0x4b, 0x1d, 0x7c, 0x89, 0x4e, 0x21, 0x9f, 0x0c, 0x2b, 0x1d, 0x4e, 0x5f, 0x6a, 0x78,
]);

/// Map an arc string identifier to a deterministic UUID for grant lookups.
pub fn arc_uuid(arc_id: &str) -> Uuid {
    Uuid::new_v5(&ARC_NAMESPACE, arc_id.as_bytes())
}

/// Decision returned by the user when resolving a pending grant.
#[derive(Debug, Clone, Copy, serde::Deserialize, Serialize, PartialEq, Eq)]
pub enum GrantDecision {
    Allow,
    AllowAlways,
    Deny,
}

/// Snapshot of a pending grant request, safe to send across threads to
/// the frontend.
#[derive(Debug, Clone, Serialize)]
pub struct PendingGrantSummary {
    pub id: Uuid,
    pub arc_id: String,
    pub paths: Vec<String>,
    pub access: String,
    pub tool: String,
}

/// In-flight request for a directory grant, parked until the user
/// responds via `resolve_pending_grant`.
pub struct PendingGrantRequest {
    pub arc_id: String,
    pub paths: Vec<PathBuf>,
    pub access: Access,
    pub tool: String,
    pub responder: oneshot::Sender<GrantDecision>,
}

impl PendingGrantRequest {
    pub fn summary(&self, id: Uuid) -> PendingGrantSummary {
        PendingGrantSummary {
            id,
            arc_id: self.arc_id.clone(),
            paths: self.paths.iter().map(|p| p.display().to_string()).collect(),
            access: match self.access {
                Access::Read => "read".to_string(),
                Access::Write => "write".to_string(),
            },
            tool: self.tool.clone(),
        }
    }
}

pub type PendingGrants = Arc<Mutex<HashMap<Uuid, PendingGrantRequest>>>;

/// Adapter that lets `PathRiskEvaluator` query the SQLite-backed
/// `GrantStore` through the `GrantLookup` trait.
pub struct GrantStoreLookup {
    store: Arc<GrantStore>,
}

impl GrantStoreLookup {
    pub fn new(store: Arc<GrantStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl GrantLookup for GrantStoreLookup {
    async fn check(&self, arc_id: Uuid, path: &Path, write: bool) -> Result<bool> {
        let access = if write { Access::Write } else { Access::Read };
        Ok(self.store.check(arc_id, path, access).await?.is_some())
    }
}

/// Provides write-grant directories for the shell sandbox's
/// `extra_writable` slot. Built per-arc so each shell session sees
/// only its own grants plus globals.
pub struct ArcWritableProvider {
    pub arc_id: Uuid,
    pub store: Arc<GrantStore>,
}

#[async_trait]
impl ShellExtraWritableProvider for ArcWritableProvider {
    async fn extra_writable_paths(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        if let Ok(arc_grants) = self.store.list_arc(self.arc_id).await {
            for g in arc_grants {
                if g.access == Access::Write && !paths::is_system_path(&g.path) {
                    out.push(g.path);
                }
            }
        }
        if let Ok(global_grants) = self.store.list_global().await {
            for g in global_grants {
                if g.access == Access::Write && !paths::is_system_path(&g.path) {
                    out.push(g.path);
                }
            }
        }
        out
    }
}

/// Wrapper that intercepts file-touching tool calls before they reach
/// the underlying registry.
pub struct FileGate {
    arc_id_str: String,
    arc_uuid: Uuid,
    evaluator: PathRiskEvaluator<GrantStoreLookup>,
    grants: Arc<GrantStore>,
    pending: PendingGrants,
    app_handle: Option<AppHandle>,
    /// Optional Telegram approval sink. When set, `ask_user` races the
    /// in-app `grant-requested` event against a Telegram inline-keyboard
    /// question so the user can also answer from Telegram if they're
    /// not at the UI.
    telegram_approval_sink: Option<Arc<crate::approval::TelegramApprovalSink>>,
}

impl FileGate {
    pub fn new(
        arc_id_str: String,
        grants: Arc<GrantStore>,
        pending: PendingGrants,
        app_handle: Option<AppHandle>,
    ) -> Self {
        let arc_uuid = arc_uuid(&arc_id_str);
        let evaluator = PathRiskEvaluator::new(GrantStoreLookup::new(grants.clone()));
        Self {
            arc_id_str,
            arc_uuid,
            evaluator,
            grants,
            pending,
            app_handle,
            telegram_approval_sink: None,
        }
    }

    /// Attach a Telegram approval sink so file-permission prompts also
    /// surface on Telegram while the in-app card is showing. The first
    /// channel to answer wins; the loser is cancelled.
    pub fn with_telegram_approval(
        mut self,
        sink: Arc<crate::approval::TelegramApprovalSink>,
    ) -> Self {
        self.telegram_approval_sink = Some(sink);
        self
    }

    /// Returns true when this tool name is a file-touching operation the
    /// gate must intercept. Covers built-in tools and the `files__*`
    /// MCP tools.
    pub fn is_file_tool(name: &str) -> bool {
        matches!(
            name,
            "read"
                | "edit"
                | "write"
                | "grep"
                | "list_directory"
                | "files__read_file"
                | "files__write_file"
                | "files__append_file"
                | "files__list_dir"
                | "files__create_dir"
                | "files__delete_path"
                | "files__move_path"
                | "files__exists"
                | "files__stat"
        )
    }

    /// Map a tool name to the access kind it needs, for risk classification.
    fn access_for(name: &str) -> PathAccess {
        match name {
            "read" | "grep" | "list_directory" | "files__read_file" | "files__list_dir"
            | "files__exists" | "files__stat" => PathAccess::Read,
            _ => PathAccess::Write,
        }
    }

    /// Pull the path argument(s) from the JSON payload. `move_path`
    /// carries two; `grep` defaults to the current working directory
    /// when `path` is omitted; everything else carries a single
    /// required `path`.
    fn paths_from_args(name: &str, args: &Value) -> Result<Vec<PathBuf>> {
        let extract = |key: &str| -> Result<PathBuf> {
            let s = args
                .get(key)
                .and_then(|v| v.as_str())
                .ok_or_else(|| AthenError::Other(format!("missing '{key}' parameter")))?;
            Ok(PathBuf::from(s))
        };
        match name {
            "files__move_path" => Ok(vec![extract("from")?, extract("to")?]),
            "grep" => {
                let p = args
                    .get("path")
                    .and_then(|v| v.as_str())
                    .map(PathBuf::from)
                    .unwrap_or_else(|| {
                        // Default to the agent workspace dir so risk eval
                        // matches what `do_grep` will actually search.
                        paths::athen_workspace_dir().unwrap_or_else(|| PathBuf::from("."))
                    });
                Ok(vec![p])
            }
            _ => Ok(vec![extract("path")?]),
        }
    }

    /// Resolve a possibly-relative path against the agent's workspace dir
    /// and lexically normalize. Must agree with how the built-in file tools
    /// resolve paths internally — otherwise risk eval and the actual file
    /// op would target different locations.
    fn absolutize(p: &Path) -> PathBuf {
        let resolved = paths::resolve_in_workspace(p);
        paths::canonicalize_loose(&resolved)
    }

    fn risk_ctx() -> RiskContext {
        RiskContext {
            trust_level: TrustLevel::AuthUser,
            data_sensitivity: DataSensitivity::Plain,
            llm_confidence: Some(1.0),
            accumulated_risk: 0,
        }
    }

    /// Evaluate every path; returns the strictest decision so callers
    /// react once per call (e.g. `move_path` evaluates both endpoints).
    async fn evaluate_all(&self, paths_in: &[PathBuf], access: PathAccess) -> Result<RiskDecision> {
        let ctx = Self::risk_ctx();
        let mut worst = RiskDecision::SilentApprove;
        for p in paths_in {
            let score = self
                .evaluator
                .evaluate(self.arc_uuid, p, access, &ctx)
                .await?;
            let d = score.decision();
            worst = strictest(worst, d);
        }
        Ok(worst)
    }

    /// Park a request and wait for the user. Emits `grant-requested` so the
    /// frontend can surface the prompt. When a Telegram approval sink is
    /// attached, races the in-app oneshot against a Telegram
    /// inline-keyboard question; whichever channel answers first wins.
    async fn ask_user(
        &self,
        paths_in: Vec<PathBuf>,
        access: Access,
        tool: &str,
    ) -> Result<GrantDecision> {
        let (tx, rx) = oneshot::channel();
        let req = PendingGrantRequest {
            arc_id: self.arc_id_str.clone(),
            paths: paths_in.clone(),
            access,
            tool: tool.to_string(),
            responder: tx,
        };
        let id = Uuid::new_v4();
        let summary = req.summary(id);
        self.pending.lock().await.insert(id, req);

        if let Some(handle) = &self.app_handle {
            let _ = handle.emit("grant-requested", &summary);
        }

        // Without Telegram, the in-app event channel is the only path.
        let Some(telegram) = self.telegram_approval_sink.clone() else {
            return rx
                .await
                .map_err(|_| AthenError::Other("pending grant cancelled".to_string()));
        };

        // With Telegram, race the in-app oneshot vs a Telegram question.
        let question = build_grant_question(
            &paths_in,
            access,
            tool,
            Some(self.arc_id_str.clone()),
        );
        let q_id = question.id;

        let pending_for_cleanup = self.pending.clone();
        let app_handle_for_cancel = self.app_handle.clone();

        tokio::select! {
            // In-app answered first.
            inapp = rx => {
                // Cancel the Telegram question so its keyboard message is edited.
                let _ = telegram.cancel(q_id).await;
                inapp.map_err(|_| AthenError::Other("pending grant cancelled".to_string()))
            }
            // Telegram answered first.
            tg = telegram.ask(question) => {
                // Drop the parked in-app entry so its responder oneshot is
                // dropped (returning a "cancelled" error if anyone awaits
                // it later) and tell the frontend to dismiss the prompt.
                pending_for_cleanup.lock().await.remove(&id);
                if let Some(handle) = app_handle_for_cancel {
                    let _ = handle.emit("grant-resolved-elsewhere", id.to_string());
                }
                tg.map(approval_choice_to_grant_decision)
            }
        }
    }

    /// Top-level entry: classify, optionally ask, and dispatch the call
    /// to either tokio::fs (absolute paths outside the sandbox) or the
    /// underlying registry (paths inside the sandbox).
    pub async fn handle(
        &self,
        name: &str,
        args: Value,
        dispatch_inside_sandbox: impl FnOnce(
            Value,
        )
            -> futures::future::BoxFuture<'static, Result<ToolResult>>,
    ) -> Result<ToolResult> {
        let raw_paths = Self::paths_from_args(name, &args)?;
        let abs_paths: Vec<PathBuf> = raw_paths.iter().map(|p| Self::absolutize(p)).collect();

        let access = Self::access_for(name);
        let access_kind = if access == PathAccess::Write {
            Access::Write
        } else {
            Access::Read
        };

        let decision = self.evaluate_all(&abs_paths, access).await?;

        match decision {
            RiskDecision::SilentApprove | RiskDecision::NotifyAndProceed => {}
            RiskDecision::HumanConfirm => {
                let user = self.ask_user(abs_paths.clone(), access_kind, name).await?;
                match user {
                    GrantDecision::Deny => {
                        return Err(AthenError::Other(format!(
                            "User denied access to {} ({} access)",
                            display_paths(&abs_paths),
                            access_label(access_kind),
                        )));
                    }
                    GrantDecision::AllowAlways => {
                        for p in &abs_paths {
                            self.grants.grant_arc(self.arc_uuid, p, access_kind).await?;
                        }
                    }
                    GrantDecision::Allow => {}
                }
            }
            RiskDecision::HardBlock => {
                return Err(AthenError::Other(format!(
                    "Refusing {} access to {} (system path, no override)",
                    access_label(access_kind),
                    display_paths(&abs_paths),
                )));
            }
        }

        // Dispatch. The MCP `files__*` tools accept only relative paths
        // anchored to the sandbox root; when the user pointed at a path
        // inside the sandbox we rewrite + delegate. The new built-in
        // file tools (`read`, `edit`, `write`, `grep`) carry stateful
        // read-state in the inner registry, so we always route them
        // through the dispatch closure rather than reimplementing here.
        // `list_directory` is stateless and runs directly via tokio::fs.
        let is_mcp = name.starts_with("files__");
        if is_mcp {
            let sandbox_root = paths::athen_files_sandbox();
            let inside_sandbox = sandbox_root
                .as_ref()
                .map(|root| abs_paths.iter().all(|p| paths::path_within(p, root)))
                .unwrap_or(false);
            if inside_sandbox {
                let root = sandbox_root.unwrap();
                let rewritten = rewrite_args_relative(name, &args, &root)?;
                return dispatch_inside_sandbox(rewritten).await;
            }
        }

        if matches!(name, "read" | "edit" | "write" | "grep") {
            return dispatch_inside_sandbox(args).await;
        }

        execute_direct(name, &abs_paths, &args).await
    }
}

fn strictest(a: RiskDecision, b: RiskDecision) -> RiskDecision {
    fn rank(d: RiskDecision) -> u8 {
        match d {
            RiskDecision::SilentApprove => 0,
            RiskDecision::NotifyAndProceed => 1,
            RiskDecision::HumanConfirm => 2,
            RiskDecision::HardBlock => 3,
        }
    }
    if rank(a) >= rank(b) {
        a
    } else {
        b
    }
}

fn display_paths(p: &[PathBuf]) -> String {
    p.iter()
        .map(|x| x.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

fn access_label(a: Access) -> &'static str {
    match a {
        Access::Read => "read",
        Access::Write => "write",
    }
}

/// Build the [`ApprovalQuestion`] sent through the Telegram sink for a
/// path-permission prompt.
///
/// Three choices map cleanly to [`GrantDecision`]:
///   - "allow"        → Allow once (this call only)
///   - "allow_always" → AllowAlways (grant stored, future calls auto-approve)
///   - "deny"         → Deny
fn build_grant_question(
    paths_in: &[PathBuf],
    access: Access,
    tool: &str,
    arc_id: Option<String>,
) -> athen_core::approval::ApprovalQuestion {
    use athen_core::approval::{ApprovalChoice, ApprovalChoiceKind, ApprovalQuestion};
    use athen_core::notification::{NotificationOrigin, NotificationUrgency};

    let prompt = format!(
        "Allow {} access via {}?",
        access_label(access),
        tool,
    );
    let description = if paths_in.is_empty() {
        None
    } else {
        Some(format!("Path: {}", display_paths(paths_in)))
    };
    ApprovalQuestion {
        id: Uuid::new_v4(),
        prompt,
        description,
        choices: vec![
            ApprovalChoice {
                key: "allow".to_string(),
                label: "Allow once".to_string(),
                kind: ApprovalChoiceKind::AllowOnce,
            },
            ApprovalChoice {
                key: "allow_always".to_string(),
                label: "Allow always".to_string(),
                kind: ApprovalChoiceKind::AllowAlways,
            },
            ApprovalChoice {
                key: "deny".to_string(),
                label: "Deny".to_string(),
                kind: ApprovalChoiceKind::Deny,
            },
        ],
        arc_id,
        task_id: None,
        origin: NotificationOrigin::SenseRouter,
        urgency: NotificationUrgency::High,
        created_at: chrono::Utc::now(),
    }
}

/// Map an [`ApprovalAnswer`] choice key back to [`GrantDecision`].
/// Unknown keys default to `Deny` — fail-closed for permission prompts.
fn approval_choice_to_grant_decision(answer: athen_core::approval::ApprovalAnswer) -> GrantDecision {
    match answer.choice_key.as_str() {
        "allow" => GrantDecision::Allow,
        "allow_always" => GrantDecision::AllowAlways,
        _ => GrantDecision::Deny,
    }
}

/// Re-write absolute paths in the arg payload so they become relative to
/// the Files MCP sandbox root. Used only on the "inside sandbox" branch.
fn rewrite_args_relative(name: &str, args: &Value, root: &Path) -> Result<Value> {
    let mut new_args = args.clone();
    let rewrite_one = |key: &str, val: &mut Value| -> Result<()> {
        let s = val
            .get(key)
            .and_then(|v| v.as_str())
            .ok_or_else(|| AthenError::Other(format!("missing '{key}' parameter")))?
            .to_string();
        let abs = FileGate::absolutize(Path::new(&s));
        let rel = abs
            .strip_prefix(root)
            .map(|p| p.to_path_buf())
            .unwrap_or(abs);
        let rel_str = if rel.as_os_str().is_empty() {
            ".".to_string()
        } else {
            rel.display().to_string()
        };
        val[key] = Value::String(rel_str);
        Ok(())
    };

    match name {
        "files__move_path" => {
            rewrite_one("from", &mut new_args)?;
            rewrite_one("to", &mut new_args)?;
        }
        _ => rewrite_one("path", &mut new_args)?,
    }
    Ok(new_args)
}

/// Run the file operation directly against the absolute path with
/// `tokio::fs`. Used when the target is outside the Files MCP sandbox.
async fn execute_direct(name: &str, abs: &[PathBuf], args: &Value) -> Result<ToolResult> {
    let start = std::time::Instant::now();
    let path = &abs[0];
    let res: std::result::Result<Value, String> = match name {
        "files__read_file" => match tokio::fs::read_to_string(path).await {
            Ok(c) => Ok(json!({ "content": c })),
            Err(e) => Err(e.to_string()),
        },
        "files__write_file" => {
            let contents = args
                .get("contents")
                .and_then(|v| v.as_str())
                .ok_or_else(|| AthenError::Other("missing 'contents' parameter".to_string()))?;
            ensure_parent_dir(path).await;
            match tokio::fs::write(path, contents).await {
                Ok(()) => Ok(
                    json!({ "path": path.display().to_string(), "bytes_written": contents.len() }),
                ),
                Err(e) => Err(e.to_string()),
            }
        }
        "files__append_file" => {
            let contents = args
                .get("contents")
                .and_then(|v| v.as_str())
                .ok_or_else(|| AthenError::Other("missing 'contents' parameter".to_string()))?;
            ensure_parent_dir(path).await;
            use tokio::io::AsyncWriteExt;
            match tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .await
            {
                Ok(mut f) => match f.write_all(contents.as_bytes()).await {
                    Ok(()) => Ok(
                        json!({ "path": path.display().to_string(), "bytes_appended": contents.len() }),
                    ),
                    Err(e) => Err(e.to_string()),
                },
                Err(e) => Err(e.to_string()),
            }
        }
        "list_directory" | "files__list_dir" => match tokio::fs::read_dir(path).await {
            Ok(mut reader) => {
                let mut entries = Vec::new();
                while let Ok(Some(entry)) = reader.next_entry().await {
                    let name = entry.file_name().to_string_lossy().to_string();
                    let kind = match entry.file_type().await {
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
                    entries.push(json!({ "name": name, "type": kind }));
                }
                let count = entries.len();
                Ok(json!({ "entries": entries, "count": count }))
            }
            Err(e) => Err(e.to_string()),
        },
        "files__create_dir" => match tokio::fs::create_dir_all(path).await {
            Ok(()) => Ok(json!({ "path": path.display().to_string(), "created": true })),
            Err(e) => Err(e.to_string()),
        },
        "files__delete_path" => {
            let meta = tokio::fs::metadata(path).await;
            let res = match meta {
                Ok(m) if m.is_dir() => tokio::fs::remove_dir_all(path).await,
                Ok(_) => tokio::fs::remove_file(path).await,
                Err(e) => return Ok(tool_err(e.to_string(), start)),
            };
            match res {
                Ok(()) => Ok(json!({ "path": path.display().to_string(), "deleted": true })),
                Err(e) => Err(e.to_string()),
            }
        }
        "files__move_path" => {
            let to = &abs[1];
            ensure_parent_dir(to).await;
            match tokio::fs::rename(path, to).await {
                Ok(()) => Ok(
                    json!({ "from": path.display().to_string(), "to": to.display().to_string() }),
                ),
                Err(e) => Err(e.to_string()),
            }
        }
        "files__exists" => match tokio::fs::metadata(path).await {
            Ok(_) => Ok(json!({ "exists": true })),
            Err(_) => Ok(json!({ "exists": false })),
        },
        "files__stat" => match tokio::fs::metadata(path).await {
            Ok(m) => {
                let kind = if m.is_dir() { "directory" } else { "file" };
                Ok(json!({ "type": kind, "size_bytes": m.len() }))
            }
            Err(e) => Err(e.to_string()),
        },
        other => return Err(AthenError::ToolNotFound(other.to_string())),
    };

    Ok(match res {
        Ok(out) => ToolResult {
            success: true,
            output: out,
            error: None,
            execution_time_ms: start.elapsed().as_millis() as u64,
        },
        Err(e) => tool_err(e, start),
    })
}

fn tool_err(msg: String, start: std::time::Instant) -> ToolResult {
    ToolResult {
        success: false,
        output: json!({ "error": msg }),
        error: Some(msg.clone()),
        execution_time_ms: start.elapsed().as_millis() as u64,
    }
}

async fn ensure_parent_dir(p: &Path) {
    if let Some(parent) = p.parent() {
        if !parent.as_os_str().is_empty() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use std::sync::Arc;
    use tokio::sync::Mutex as TokioMutex;

    async fn fresh_grants() -> Arc<GrantStore> {
        let conn = Connection::open_in_memory().expect("open db");
        let store = GrantStore::new(Arc::new(TokioMutex::new(conn)));
        store.init_schema().await.expect("schema");
        Arc::new(store)
    }

    fn pending() -> PendingGrants {
        Arc::new(TokioMutex::new(HashMap::new()))
    }

    #[test]
    fn arc_uuid_is_deterministic() {
        let a = arc_uuid("arc_20260101_120000");
        let b = arc_uuid("arc_20260101_120000");
        assert_eq!(a, b);
        let c = arc_uuid("arc_20260101_120001");
        assert_ne!(a, c);
    }

    #[test]
    fn paths_from_args_handles_move() {
        let args = json!({ "from": "/tmp/a", "to": "/tmp/b" });
        let v = FileGate::paths_from_args("files__move_path", &args).unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v[0], PathBuf::from("/tmp/a"));
        assert_eq!(v[1], PathBuf::from("/tmp/b"));
    }

    #[test]
    fn paths_from_args_single_path() {
        let args = json!({ "path": "/tmp/x" });
        let v = FileGate::paths_from_args("read", &args).unwrap();
        assert_eq!(v, vec![PathBuf::from("/tmp/x")]);
    }

    #[test]
    fn missing_path_errors() {
        let r = FileGate::paths_from_args("read", &json!({}));
        assert!(r.is_err());
    }

    #[test]
    fn grep_path_defaults_to_workspace() {
        let r = FileGate::paths_from_args("grep", &json!({ "pattern": "x" })).unwrap();
        assert_eq!(r.len(), 1);
        let ws = paths::athen_workspace_dir().unwrap();
        assert_eq!(r[0], ws);
    }

    #[test]
    fn absolutize_relative_uses_workspace() {
        let abs = FileGate::absolutize(Path::new("test.html"));
        let ws = paths::athen_workspace_dir().unwrap();
        // canonicalize_loose may resolve symlinks if the dir exists; check
        // that the resolved path lives under the workspace either way.
        assert!(
            paths::path_within(&abs, &ws),
            "expected {} to be under workspace {}",
            abs.display(),
            ws.display(),
        );
    }

    #[test]
    fn classifies_read_vs_write() {
        assert_eq!(FileGate::access_for("read"), PathAccess::Read);
        assert_eq!(FileGate::access_for("grep"), PathAccess::Read);
        assert_eq!(FileGate::access_for("files__exists"), PathAccess::Read);
        assert_eq!(FileGate::access_for("write"), PathAccess::Write);
        assert_eq!(FileGate::access_for("edit"), PathAccess::Write);
        assert_eq!(
            FileGate::access_for("files__delete_path"),
            PathAccess::Write
        );
        assert_eq!(FileGate::access_for("files__move_path"), PathAccess::Write);
    }

    #[test]
    fn detects_file_tools() {
        assert!(FileGate::is_file_tool("read"));
        assert!(FileGate::is_file_tool("edit"));
        assert!(FileGate::is_file_tool("write"));
        assert!(FileGate::is_file_tool("grep"));
        assert!(FileGate::is_file_tool("files__write_file"));
        assert!(!FileGate::is_file_tool("shell_execute"));
        assert!(!FileGate::is_file_tool("calendar_create"));
        assert!(!FileGate::is_file_tool("read_file"));
        assert!(!FileGate::is_file_tool("write_file"));
    }

    #[test]
    fn strictest_orders_decisions() {
        assert_eq!(
            strictest(RiskDecision::SilentApprove, RiskDecision::HumanConfirm),
            RiskDecision::HumanConfirm
        );
        assert_eq!(
            strictest(RiskDecision::HardBlock, RiskDecision::SilentApprove),
            RiskDecision::HardBlock
        );
        assert_eq!(
            strictest(RiskDecision::HumanConfirm, RiskDecision::NotifyAndProceed),
            RiskDecision::HumanConfirm
        );
    }

    #[tokio::test]
    async fn write_to_athen_data_dir_is_silent() {
        let grants = fresh_grants().await;
        let pending = pending();
        let gate = FileGate::new("arc_test_silent".into(), grants, pending, None);
        let data = paths::athen_data_dir().expect("data dir");
        let target = data.join("scratch_phaseB.txt");

        let dec = gate
            .evaluate_all(std::slice::from_ref(&target), PathAccess::Write)
            .await
            .unwrap();
        assert_eq!(dec, RiskDecision::SilentApprove);
    }

    #[tokio::test]
    async fn write_to_system_path_is_blocked() {
        let grants = fresh_grants().await;
        let pending = pending();
        let gate = FileGate::new("arc_test_block".into(), grants, pending, None);

        let dec = gate
            .evaluate_all(&[PathBuf::from("/etc/passwd")], PathAccess::Write)
            .await
            .unwrap();
        assert_eq!(dec, RiskDecision::HardBlock);
    }

    #[tokio::test]
    async fn write_to_home_requires_confirm() {
        let grants = fresh_grants().await;
        let pending = pending();
        let gate = FileGate::new("arc_test_home".into(), grants, pending, None);
        let home = paths::home_dir().expect("home");
        let target = home.join("phaseB_confirm.txt");

        let dec = gate
            .evaluate_all(&[target], PathAccess::Write)
            .await
            .unwrap();
        assert_eq!(dec, RiskDecision::HumanConfirm);
    }

    #[tokio::test]
    async fn granted_arc_path_becomes_silent() {
        let grants = fresh_grants().await;
        let pending = pending();
        let arc_str = "arc_test_grantflow";
        let arc_id = arc_uuid(arc_str);
        let dir = std::env::temp_dir().join(format!("athen_phaseB_grant_{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        grants.grant_arc(arc_id, &dir, Access::Write).await.unwrap();

        let gate = FileGate::new(arc_str.into(), grants, pending, None);
        let dec = gate
            .evaluate_all(&[dir.join("file.txt")], PathAccess::Write)
            .await
            .unwrap();
        assert_eq!(dec, RiskDecision::SilentApprove);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn move_strictest_path_wins() {
        let grants = fresh_grants().await;
        let pending = pending();
        let gate = FileGate::new("arc_test_move".into(), grants, pending, None);
        // /etc -> system; /tmp -> ok. Strictest is HardBlock.
        let dec = gate
            .evaluate_all(
                &[PathBuf::from("/tmp/x"), PathBuf::from("/etc/x")],
                PathAccess::Write,
            )
            .await
            .unwrap();
        assert_eq!(dec, RiskDecision::HardBlock);
    }

    #[tokio::test]
    async fn pending_grant_records_request_then_resolves() {
        let grants = fresh_grants().await;
        let pending = pending();
        let arc_str = "arc_test_park";
        let gate = FileGate::new(arc_str.into(), grants.clone(), pending.clone(), None);
        let target = paths::home_dir().unwrap().join("phaseB_ask.txt");

        // Spawn a task that races against the resolver.
        let p_clone = pending.clone();
        let join = tokio::spawn(async move {
            gate.ask_user(vec![target.clone()], Access::Write, "write")
                .await
        });

        // Wait briefly for the request to land in the map.
        for _ in 0..50 {
            if !p_clone.lock().await.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        // Grab the only id, send Allow, ensure ask_user returns it.
        let id = {
            let map = p_clone.lock().await;
            *map.keys().next().expect("expected pending id")
        };
        let req = p_clone.lock().await.remove(&id).unwrap();
        req.responder.send(GrantDecision::Allow).unwrap();

        let decision = join.await.unwrap().unwrap();
        assert_eq!(decision, GrantDecision::Allow);
    }

    #[tokio::test]
    async fn allow_always_records_grant() {
        let grants = fresh_grants().await;
        let pending = pending();
        let arc_str = "arc_test_always";
        let arc_id = arc_uuid(arc_str);
        let target = std::env::temp_dir().join(format!("athen_phaseB_always_{}", Uuid::new_v4()));
        std::fs::create_dir_all(&target).unwrap();

        let gate = FileGate::new(arc_str.into(), grants.clone(), pending.clone(), None);

        // Pre-park: invoke handle indirectly by using ask_user, then simulate
        // AllowAlways directly through the public flow via grant_arc.
        // Here we mimic the wrapper's AllowAlways branch.
        let p = pending.clone();
        let g_clone = grants.clone();
        let target_clone = target.clone();
        let join = tokio::spawn(async move {
            let dec = gate
                .ask_user(vec![target_clone.clone()], Access::Write, "write")
                .await
                .unwrap();
            if dec == GrantDecision::AllowAlways {
                g_clone
                    .grant_arc(arc_id, &target_clone, Access::Write)
                    .await
                    .unwrap();
            }
            dec
        });

        for _ in 0..50 {
            if !p.lock().await.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let id = *p.lock().await.keys().next().unwrap();
        let req = p.lock().await.remove(&id).unwrap();
        req.responder.send(GrantDecision::AllowAlways).unwrap();
        join.await.unwrap();

        let scope = grants.check(arc_id, &target, Access::Write).await.unwrap();
        assert!(scope.is_some());
        let _ = std::fs::remove_dir_all(&target);
    }

    #[test]
    fn approval_choice_keys_map_to_grant_decisions() {
        use athen_core::approval::ApprovalAnswer;
        let q = Uuid::new_v4();
        for (key, expected) in [
            ("allow", GrantDecision::Allow),
            ("allow_always", GrantDecision::AllowAlways),
            ("deny", GrantDecision::Deny),
        ] {
            let answer = ApprovalAnswer {
                question_id: q,
                choice_key: key.to_string(),
            };
            assert_eq!(approval_choice_to_grant_decision(answer), expected);
        }
    }

    #[test]
    fn unknown_choice_key_fails_closed_to_deny() {
        let answer = athen_core::approval::ApprovalAnswer {
            question_id: Uuid::new_v4(),
            choice_key: "garbage".into(),
        };
        assert_eq!(
            approval_choice_to_grant_decision(answer),
            GrantDecision::Deny
        );
    }

    #[test]
    fn build_grant_question_carries_paths_in_description_and_three_choices() {
        let paths = vec![PathBuf::from("/tmp/a"), PathBuf::from("/tmp/b")];
        let q = build_grant_question(&paths, Access::Write, "write", Some("arc_x".into()));
        assert!(q.prompt.contains("write"));
        let desc = q.description.expect("description present");
        assert!(desc.contains("/tmp/a"));
        assert!(desc.contains("/tmp/b"));
        assert_eq!(q.choices.len(), 3);
        assert_eq!(q.choices[0].key, "allow");
        assert_eq!(q.choices[1].key, "allow_always");
        assert_eq!(q.choices[2].key, "deny");
        assert_eq!(q.arc_id.as_deref(), Some("arc_x"));
    }
}
