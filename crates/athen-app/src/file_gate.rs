//! Path-based permission gate for file-touching tools.
//!
//! Sits between the agent's tool calls and the underlying executors
//! (built-in tokio::fs ops). Every call carrying a path is routed
//! through `PathRiskEvaluator`, which classifies the access into one of
//! four bands; the gate then either runs the operation, asks the user
//! via `pending_grants`, or rejects it outright.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde::Serialize;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter};
use tokio::sync::{oneshot, Mutex};
use uuid::Uuid;

use athen_agent::tools::{ShellExtraWritableProvider, ToolboxApprovalGate};
use athen_core::contact::TrustLevel;
use athen_core::error::{AthenError, Result};
use athen_core::paths::{self, DetectedRoot};
use athen_core::risk::{DataSensitivity, RiskContext, RiskDecision};
use athen_core::tool::ToolResult;
use athen_core::traits::approval::ApprovalSink;
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
///
/// `AllowProjectRoot` carries the detected project-root path (git root,
/// Cargo workspace, npm package, etc.) so the gate can issue a directory
/// grant scoped to the whole project rather than just the touched file.
/// Wire shape from the frontend uses serde's externally-tagged default,
/// e.g. `"Allow"`, `"AllowAlways"`, `"Deny"`, or `{"AllowProjectRoot":"/path"}`.
#[derive(Debug, Clone, serde::Deserialize, Serialize, PartialEq, Eq)]
pub enum GrantDecision {
    Allow,
    AllowAlways,
    AllowProjectRoot(PathBuf),
    Deny,
}

/// Snapshot of a pending grant request, safe to send across threads to
/// the frontend.
///
/// `detected_root` is populated when [`paths::detect_project_root`] finds
/// a project marker (git, Cargo, npm, Python, Go, Maven, Gradle) at or
/// above the first requested path. The frontend uses it to render a
/// "Allow <root> (<marker>)" recommended button that grants the whole
/// project tree rather than just the touched file.
#[derive(Debug, Clone, Serialize)]
pub struct PendingGrantSummary {
    pub id: Uuid,
    pub arc_id: String,
    pub paths: Vec<String>,
    pub access: String,
    pub tool: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detected_root: Option<DetectedRootSummary>,
}

/// Serializable mirror of [`DetectedRoot`] for the frontend. Uses
/// `camelCase` since the FE consumes it through Tauri events.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DetectedRootSummary {
    pub path: String,
    pub path_display: String,
    pub marker: String,
}

impl DetectedRootSummary {
    fn from_detected(root: &DetectedRoot) -> Self {
        Self {
            path: root.path.display().to_string(),
            path_display: friendly_path(&root.path),
            marker: root.marker.label().to_string(),
        }
    }
}

/// Render a path with `$HOME` substituted for `~` when applicable.
fn friendly_path(path: &Path) -> String {
    if let Some(home) = paths::home_dir() {
        if let Ok(rest) = path.strip_prefix(&home) {
            if rest.as_os_str().is_empty() {
                return "~".to_string();
            }
            return format!("~/{}", rest.display());
        }
    }
    path.display().to_string()
}

/// In-flight request for a directory grant, parked until the user
/// responds via `resolve_pending_grant`.
pub struct PendingGrantRequest {
    pub arc_id: String,
    pub paths: Vec<PathBuf>,
    pub access: Access,
    pub tool: String,
    pub detected_root: Option<DetectedRoot>,
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
            detected_root: self
                .detected_root
                .as_ref()
                .map(DetectedRootSummary::from_detected),
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

/// Routes `install_package` approval prompts through the cross-channel
/// [`crate::approval::ApprovalRouter`] (in-app card + Telegram inline
/// keyboard, with escalation). Returns `true` only when the user
/// taps "Approve" — every other outcome (Deny, timeout, router error,
/// bad answer) maps to `false` so installs fail closed.
pub struct RouterToolboxApprovalGate {
    router: Arc<crate::approval::ApprovalRouter>,
    arc_id: Option<String>,
}

impl RouterToolboxApprovalGate {
    pub fn new(router: Arc<crate::approval::ApprovalRouter>, arc_id: Option<String>) -> Self {
        Self { router, arc_id }
    }
}

#[async_trait]
impl ToolboxApprovalGate for RouterToolboxApprovalGate {
    async fn confirm_install(&self, runtime: &str, package: &str, reason: &str) -> bool {
        use athen_core::approval::{ApprovalChoice, ApprovalQuestion};
        use athen_core::notification::{NotificationOrigin, NotificationUrgency};

        let question = ApprovalQuestion {
            id: Uuid::new_v4(),
            prompt: format!("Install {package} ({runtime})?"),
            description: Some(reason.to_string()),
            choices: vec![ApprovalChoice::approve(), ApprovalChoice::deny()],
            arc_id: self.arc_id.clone(),
            task_id: None,
            origin: NotificationOrigin::RiskSystem,
            urgency: NotificationUrgency::High,
            created_at: chrono::Utc::now(),
        };
        let primary = self.router.pick_primary(self.arc_id.as_deref()).await;
        match self.router.ask_with_escalation(question, primary).await {
            Ok(answer) => answer.choice_key == "approve",
            Err(e) => {
                tracing::warn!(
                    package,
                    runtime,
                    error = %e,
                    "toolbox install approval router failed; treating as deny"
                );
                false
            }
        }
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
    /// gate must intercept.
    pub fn is_file_tool(name: &str) -> bool {
        matches!(name, "read" | "edit" | "write" | "grep" | "list_directory")
    }

    /// Map a tool name to the access kind it needs, for risk classification.
    fn access_for(name: &str) -> PathAccess {
        match name {
            "read" | "grep" | "list_directory" => PathAccess::Read,
            _ => PathAccess::Write,
        }
    }

    /// Pull the path argument from the JSON payload. `grep` defaults to
    /// the agent workspace when `path` is omitted; everything else
    /// carries a single required `path`.
    fn paths_from_args(name: &str, args: &Value) -> Result<Vec<PathBuf>> {
        let extract = |key: &str| -> Result<PathBuf> {
            let s = args
                .get(key)
                .and_then(|v| v.as_str())
                .ok_or_else(|| AthenError::Other(format!("missing '{key}' parameter")))?;
            Ok(PathBuf::from(s))
        };
        match name {
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
        detected_root: Option<DetectedRoot>,
    ) -> Result<GrantDecision> {
        let (tx, rx) = oneshot::channel();
        let req = PendingGrantRequest {
            arc_id: self.arc_id_str.clone(),
            paths: paths_in.clone(),
            access,
            tool: tool.to_string(),
            detected_root: detected_root.clone(),
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
        // `detected_root` is captured here so the Telegram answer can
        // reconstruct `GrantDecision::AllowProjectRoot(path)` without
        // round-tripping the path through the choice key (Option A).
        let question = build_grant_question(
            &paths_in,
            access,
            tool,
            Some(self.arc_id_str.clone()),
            detected_root.as_ref(),
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
                tg.map(|ans| approval_choice_to_grant_decision(ans, detected_root.as_ref()))
            }
        }
    }

    /// Top-level entry: classify, optionally ask, and dispatch the call
    /// to either tokio::fs (for `list_directory`, which is stateless) or
    /// the underlying registry (for `read`/`edit`/`write`/`grep`, which
    /// carry stateful read-state in the inner registry).
    pub async fn handle(
        &self,
        name: &str,
        args: Value,
        dispatch_inner: impl FnOnce(Value) -> futures::future::BoxFuture<'static, Result<ToolResult>>,
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
                // Best-effort project-root detection so the user gets a
                // "Allow ~/myrepo (git root)" recommended choice that
                // grants the whole project, not just this single file.
                let detected_root = abs_paths
                    .first()
                    .and_then(|p| paths::detect_project_root(p));
                let user = self
                    .ask_user(abs_paths.clone(), access_kind, name, detected_root)
                    .await?;
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
                    GrantDecision::AllowProjectRoot(root) => {
                        // For Access::Read access requests, the
                        // grant_arc with access_kind=Read is honored
                        // directly; write grants imply read via the
                        // existing GrantStore rule.
                        self.grants
                            .grant_arc(self.arc_uuid, &root, access_kind)
                            .await?;
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

        // `read`/`edit`/`write`/`grep` carry stateful read-state in the
        // inner registry, so they must be routed through the dispatch
        // closure. `list_directory` is stateless and runs directly via
        // tokio::fs.
        if matches!(name, "read" | "edit" | "write" | "grep") {
            return dispatch_inner(args).await;
        }

        execute_direct(name, &abs_paths).await
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
/// Choices map cleanly to [`GrantDecision`]:
///   - "allow_root"   → AllowProjectRoot (only when `detected_root` is `Some`;
///     prepended as the recommended first button)
///   - "allow"        → Allow once (this call only)
///   - "allow_always" → AllowAlways (grant stored, future calls auto-approve)
///   - "deny"         → Deny
fn build_grant_question(
    paths_in: &[PathBuf],
    access: Access,
    tool: &str,
    arc_id: Option<String>,
    detected_root: Option<&DetectedRoot>,
) -> athen_core::approval::ApprovalQuestion {
    use athen_core::approval::{ApprovalChoice, ApprovalChoiceKind, ApprovalQuestion};
    use athen_core::notification::{NotificationOrigin, NotificationUrgency};

    let prompt = format!("Allow {} access via {}?", access_label(access), tool,);
    let description = if paths_in.is_empty() {
        None
    } else {
        Some(format!("Path: {}", display_paths(paths_in)))
    };

    let mut choices = Vec::with_capacity(4);
    if let Some(root) = detected_root {
        choices.push(ApprovalChoice {
            key: "allow_root".to_string(),
            label: format!(
                "Allow {} ({})",
                friendly_path(&root.path),
                root.marker.label()
            ),
            // Kind is just an icon hint per the existing comment; the
            // server side disambiguates on `key`.
            kind: ApprovalChoiceKind::AllowOnce,
        });
    }
    choices.extend([
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
    ]);

    ApprovalQuestion {
        id: Uuid::new_v4(),
        prompt,
        description,
        choices,
        arc_id,
        task_id: None,
        origin: NotificationOrigin::SenseRouter,
        urgency: NotificationUrgency::High,
        created_at: chrono::Utc::now(),
    }
}

/// Map an [`ApprovalAnswer`] choice key back to [`GrantDecision`].
/// Unknown keys default to `Deny` — fail-closed for permission prompts.
///
/// `detected_root` must be the same root passed into
/// [`build_grant_question`]; it's how the `"allow_root"` choice round-trips
/// its `PathBuf` payload server-side (Option A from the design plan —
/// the choice key itself stays a plain string, no path encoding).
fn approval_choice_to_grant_decision(
    answer: athen_core::approval::ApprovalAnswer,
    detected_root: Option<&DetectedRoot>,
) -> GrantDecision {
    match answer.choice_key.as_str() {
        "allow" => GrantDecision::Allow,
        "allow_always" => GrantDecision::AllowAlways,
        "allow_root" => match detected_root {
            Some(root) => GrantDecision::AllowProjectRoot(root.path.clone()),
            // Telegram somehow returned `allow_root` without a stashed
            // root (shouldn't happen — the choice is only offered when
            // we had one). Fall back to fail-closed.
            None => GrantDecision::Deny,
        },
        _ => GrantDecision::Deny,
    }
}

/// Run the file operation directly against the absolute path with
/// `tokio::fs`. Used for the stateless `list_directory` tool. The
/// stateful built-ins (`read`/`edit`/`write`/`grep`) route through the
/// inner registry instead so their per-arc read-state survives.
async fn execute_direct(name: &str, abs: &[PathBuf]) -> Result<ToolResult> {
    let start = std::time::Instant::now();
    let path = &abs[0];
    let res: std::result::Result<Value, String> = match name {
        "list_directory" => match tokio::fs::read_dir(path).await {
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
        other => return Err(AthenError::ToolNotFound(other.to_string())),
    };

    Ok(match res {
        Ok(out) => ToolResult {
            success: true,
            output: out,
            error: None,
            execution_time_ms: start.elapsed().as_millis() as u64,
        },
        Err(e) => ToolResult {
            success: false,
            output: json!({ "error": e }),
            error: Some(e),
            execution_time_ms: start.elapsed().as_millis() as u64,
        },
    })
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
        assert_eq!(FileGate::access_for("list_directory"), PathAccess::Read);
        assert_eq!(FileGate::access_for("write"), PathAccess::Write);
        assert_eq!(FileGate::access_for("edit"), PathAccess::Write);
    }

    #[test]
    fn detects_file_tools() {
        assert!(FileGate::is_file_tool("read"));
        assert!(FileGate::is_file_tool("edit"));
        assert!(FileGate::is_file_tool("write"));
        assert!(FileGate::is_file_tool("grep"));
        assert!(FileGate::is_file_tool("list_directory"));
        assert!(!FileGate::is_file_tool("shell_execute"));
        assert!(!FileGate::is_file_tool("calendar_create"));
        assert!(!FileGate::is_file_tool("read_file"));
        assert!(!FileGate::is_file_tool("write_file"));
        assert!(!FileGate::is_file_tool("files__read_file"));
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
            gate.ask_user(vec![target.clone()], Access::Write, "write", None)
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
                .ask_user(vec![target_clone.clone()], Access::Write, "write", None)
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
            assert_eq!(approval_choice_to_grant_decision(answer, None), expected);
        }
    }

    #[test]
    fn unknown_choice_key_fails_closed_to_deny() {
        let answer = athen_core::approval::ApprovalAnswer {
            question_id: Uuid::new_v4(),
            choice_key: "garbage".into(),
        };
        assert_eq!(
            approval_choice_to_grant_decision(answer, None),
            GrantDecision::Deny
        );
    }

    #[test]
    fn build_grant_question_carries_paths_in_description_and_three_choices() {
        let paths = vec![PathBuf::from("/tmp/a"), PathBuf::from("/tmp/b")];
        let q = build_grant_question(&paths, Access::Write, "write", Some("arc_x".into()), None);
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

    #[test]
    fn build_grant_question_prepends_allow_root_when_root_present() {
        use athen_core::paths::RootMarker;
        let root = DetectedRoot {
            path: PathBuf::from("/tmp/myproj"),
            marker: RootMarker::Git,
        };
        let paths = vec![PathBuf::from("/tmp/myproj/src/main.rs")];
        let q = build_grant_question(
            &paths,
            Access::Write,
            "write",
            Some("arc_x".into()),
            Some(&root),
        );
        assert_eq!(q.choices.len(), 4);
        assert_eq!(q.choices[0].key, "allow_root");
        assert!(q.choices[0].label.contains("git root"));
        assert!(q.choices[0].label.contains("/tmp/myproj"));
        assert_eq!(q.choices[1].key, "allow");
        assert_eq!(q.choices[2].key, "allow_always");
        assert_eq!(q.choices[3].key, "deny");
    }

    #[test]
    fn allow_root_choice_key_carries_root_path_via_stash() {
        use athen_core::approval::ApprovalAnswer;
        use athen_core::paths::RootMarker;
        let root = DetectedRoot {
            path: PathBuf::from("/tmp/myrepo"),
            marker: RootMarker::Cargo,
        };
        let answer = ApprovalAnswer {
            question_id: Uuid::new_v4(),
            choice_key: "allow_root".into(),
        };
        let dec = approval_choice_to_grant_decision(answer, Some(&root));
        assert_eq!(dec, GrantDecision::AllowProjectRoot(root.path.clone()));
    }

    #[test]
    fn allow_root_without_stashed_root_fails_closed_to_deny() {
        use athen_core::approval::ApprovalAnswer;
        let answer = ApprovalAnswer {
            question_id: Uuid::new_v4(),
            choice_key: "allow_root".into(),
        };
        assert_eq!(
            approval_choice_to_grant_decision(answer, None),
            GrantDecision::Deny
        );
    }

    #[test]
    fn friendly_path_substitutes_home_with_tilde() {
        let Some(home) = paths::home_dir() else {
            return;
        };
        let sub = home.join("projects/foo");
        let rendered = friendly_path(&sub);
        assert_eq!(rendered, "~/projects/foo");
        // Exact home should render as "~".
        assert_eq!(friendly_path(&home), "~");
        // Non-home paths render verbatim.
        let other = PathBuf::from("/tmp/somewhere");
        assert_eq!(friendly_path(&other), "/tmp/somewhere");
    }
}
