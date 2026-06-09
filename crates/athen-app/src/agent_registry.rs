//! Live registry of running agents.
//!
//! In-memory snapshot of every executor task currently in flight, plus a
//! mirror to the SQLite `agent_runs` table for the historical record.
//! The frontend's "watch the agents work" panel reads `snapshot()` and
//! re-fetches whenever the `agents-changed` Tauri event fires.
//!
//! Lifecycle is RAII via [`RegistrationGuard`]: callers `register()`, hold
//! the guard for the duration of `executor.execute()`, and either
//! `complete().await` / `fail(...).await` on the way out — or let the
//! guard's `Drop` impl finalize as `Cancelled` if the path was abandoned
//! (panic, early return without explicit finalize).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use crate::ui_bridge::UiBridge;
use tokio::sync::RwLock;
use uuid::Uuid;

use athen_persistence::agent_runs::{AgentRunRecord, SqliteAgentRunStore};

/// Where an agent run was triggered from. Maps onto the `source` column
/// of `agent_runs` and onto the icon mapping in the frontend.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentSource {
    UserChat,
    Telegram,
    Email,
    Calendar,
    Wakeup,
    Subagent,
    Other,
}

impl AgentSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::UserChat => "user_chat",
            Self::Telegram => "telegram",
            Self::Email => "email",
            Self::Calendar => "calendar",
            Self::Wakeup => "wakeup",
            Self::Subagent => "subagent",
            Self::Other => "other",
        }
    }
}

/// Live view of one running agent. Serialised straight to the frontend
/// via the `list_active_agents` Tauri command.
#[derive(Debug, Clone, Serialize)]
pub struct ActiveAgent {
    pub task_id: String,
    pub arc_id: Option<String>,
    pub source: AgentSource,
    pub title: String,
    pub started_at: DateTime<Utc>,
    pub last_step_at: DateTime<Utc>,
    pub current_tool: Option<String>,
    pub current_action: Option<String>,
    pub step_count: u32,
    pub profile_id: Option<String>,
    pub model: Option<String>,
    /// Conversation-turn id this run belongs to. Surfaced to the FE so
    /// the Agent Control "expand a card" view can look up the per-turn
    /// `tool_call` arc entries that make up this run's step timeline.
    pub turn_id: Option<String>,
}

/// Terminal status for a finalized run.
#[derive(Debug, Clone, Copy)]
pub enum FinishStatus {
    Completed,
    Failed,
    Cancelled,
}

impl FinishStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

/// Internal map entry pairing the public [`ActiveAgent`] snapshot with
/// the per-run cancellation flag. The flag is intentionally NOT on
/// `ActiveAgent` (which is `Serialize` / shipped to the frontend) — it
/// stays Rust-side and is handed to the executor via `AgentBuilder`.
struct AgentEntry {
    agent: ActiveAgent,
    cancel_flag: Arc<AtomicBool>,
}

/// Live registry. Constructed once during app startup with a
/// [`UiBridge`] for emitting change events and an optional store for
/// persisting historical runs.
pub struct AgentRegistry {
    app: UiBridge,
    inner: RwLock<HashMap<Uuid, AgentEntry>>,
    store: Option<Arc<SqliteAgentRunStore>>,
}

impl AgentRegistry {
    pub fn new(app: UiBridge, store: Option<Arc<SqliteAgentRunStore>>) -> Arc<Self> {
        Arc::new(Self {
            app,
            inner: RwLock::new(HashMap::new()),
            store,
        })
    }

    /// Add a new live agent to the registry, persist a `running` row to
    /// the store (best-effort), and return a guard that finalizes on
    /// `complete()` / `fail()` or — failing those — on Drop as
    /// `Cancelled`. Caller must parse `agent.task_id` from a Uuid.
    ///
    /// The returned [`RegistrationGuard`] also exposes the freshly-minted
    /// per-run cancel flag via [`RegistrationGuard::cancel_flag`], which
    /// must be passed to `AgentBuilder::cancel_flag(...)` so the
    /// executor checks it between steps. The flag starts `false`.
    pub async fn register(self: &Arc<Self>, agent: ActiveAgent) -> RegistrationGuard {
        let task_id_uuid = Uuid::parse_str(&agent.task_id).unwrap_or_else(|_| Uuid::new_v4());

        if let Some(store) = self.store.as_ref() {
            let record = AgentRunRecord {
                task_id: agent.task_id.clone(),
                arc_id: agent.arc_id.clone(),
                source: agent.source.as_str().to_string(),
                title: agent.title.clone(),
                started_at: agent.started_at,
                finished_at: None,
                status: "running".to_string(),
                step_count: agent.step_count,
                profile_id: agent.profile_id.clone(),
                model: agent.model.clone(),
                error: None,
                turn_id: agent.turn_id.clone(),
            };
            if let Err(e) = store.start(&record).await {
                tracing::warn!(task_id = %agent.task_id, error = %e, "agent_runs.start failed");
            }
        }

        let cancel_flag = Arc::new(AtomicBool::new(false));

        {
            let mut map = self.inner.write().await;
            map.insert(
                task_id_uuid,
                AgentEntry {
                    agent,
                    cancel_flag: Arc::clone(&cancel_flag),
                },
            );
        }
        self.emit_changed();

        RegistrationGuard {
            reg: Arc::clone(self),
            task_id: task_id_uuid,
            finalized: AtomicBool::new(false),
            cancel_flag,
        }
    }

    /// Bump step counters and update the live "what is the agent doing
    /// right now" fields. `tool` is the tool that just ran; `summary` is
    /// a short human-readable detail line (paths, queries, etc.).
    pub async fn record_step(&self, task_id: Uuid, tool: Option<&str>, summary: Option<String>) {
        let new_count = {
            let mut map = self.inner.write().await;
            if let Some(entry) = map.get_mut(&task_id) {
                let agent = &mut entry.agent;
                agent.step_count = agent.step_count.saturating_add(1);
                agent.last_step_at = Utc::now();
                if tool.is_some() {
                    agent.current_tool = tool.map(|s| s.to_string());
                }
                if summary.is_some() {
                    agent.current_action = summary;
                }
                Some(agent.step_count)
            } else {
                None
            }
        };

        if let (Some(count), Some(store)) = (new_count, self.store.as_ref()) {
            let task_id_str = task_id.to_string();
            if let Err(e) = store.bump_step(&task_id_str, count).await {
                tracing::debug!(task_id = %task_id, error = %e, "agent_runs.bump_step failed");
            }
        }

        if new_count.is_some() {
            self.emit_changed();
        }
    }

    /// Snapshot of every currently-running agent. Order is unspecified;
    /// the `list_active_agents` command sorts by `started_at` DESC.
    pub async fn snapshot(&self) -> Vec<ActiveAgent> {
        let map = self.inner.read().await;
        map.values().map(|e| e.agent.clone()).collect()
    }

    /// Flip the per-run cancel flag for a specific task. Returns `true`
    /// if the task was found in the registry. The executor checks the
    /// flag between steps and short-circuits with a Cancelled outcome.
    pub async fn cancel(&self, task_id: Uuid) -> bool {
        let map = self.inner.read().await;
        if let Some(entry) = map.get(&task_id) {
            entry.cancel_flag.store(true, Ordering::Relaxed);
            true
        } else {
            false
        }
    }

    /// Flip every per-run cancel flag in the registry. Returns the count
    /// of flags flipped. Backs the global `cancel_task` Tauri command.
    pub async fn cancel_all(&self) -> usize {
        let map = self.inner.read().await;
        let mut n = 0usize;
        for entry in map.values() {
            entry.cancel_flag.store(true, Ordering::Relaxed);
            n += 1;
        }
        n
    }

    /// Drain every live entry and finalize each as `Cancelled` with the
    /// given reason recorded in the error column. Used by the graceful
    /// shutdown coordinator: setting `cancel_flag = true` (via
    /// [`Self::cancel_all`]) only reaches the executor at its next
    /// between-steps poll, but on exit we want the persistent record to
    /// reflect immediately that these runs didn't complete naturally.
    /// Returns the count of entries finalized.
    pub async fn finalize_all_as_cancelled(&self, reason: &str) -> usize {
        // Snapshot the task ids under the read lock so `finalize` can
        // re-acquire the write lock to remove each entry without
        // deadlocking ourselves.
        let task_ids: Vec<Uuid> = {
            let map = self.inner.read().await;
            map.keys().copied().collect()
        };
        let count = task_ids.len();
        for id in task_ids {
            self.finalize(id, FinishStatus::Cancelled, Some(reason.to_string()))
                .await;
        }
        count
    }

    async fn finalize(&self, task_id: Uuid, status: FinishStatus, error: Option<String>) {
        {
            let mut map = self.inner.write().await;
            map.remove(&task_id);
        }
        if let Some(store) = self.store.as_ref() {
            let task_id_str = task_id.to_string();
            if let Err(e) = store
                .finalize(&task_id_str, status.as_str(), error.as_deref(), Utc::now())
                .await
            {
                tracing::warn!(task_id = %task_id, error = %e, "agent_runs.finalize failed");
            }
        }
        self.emit_changed();
    }

    fn emit_changed(&self) {
        // Bare pulse — the FE re-fetches via `list_active_agents` so the
        // payload schema stays decoupled from this event. Failures here
        // don't matter (no listener attached, or webview is down).
        self.app.emit("agents-changed", ());
    }
}

/// RAII guard returned by [`AgentRegistry::register`]. Holders MUST call
/// `complete()` / `fail()` on the success / error paths so the store
/// gets a clean status. The Drop impl is a safety net for early returns.
pub struct RegistrationGuard {
    reg: Arc<AgentRegistry>,
    task_id: Uuid,
    finalized: AtomicBool,
    cancel_flag: Arc<AtomicBool>,
}

impl RegistrationGuard {
    /// Public for callers that want to correlate the guard with the task
    /// id later (e.g. logging). Currently unused inside the crate.
    #[allow(dead_code)]
    pub fn task_id(&self) -> Uuid {
        self.task_id
    }

    /// Per-run cancel flag, freshly minted by [`AgentRegistry::register`].
    /// Hand this to `AgentBuilder::cancel_flag(...)` so the executor
    /// honors per-agent stop requests. Independently flippable from the
    /// registry's [`AgentRegistry::cancel`] / `cancel_all` methods.
    pub fn cancel_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.cancel_flag)
    }

    pub async fn complete(self) {
        self.finalize_inner(FinishStatus::Completed, None).await;
    }

    pub async fn fail(self, err: impl Into<String>) {
        self.finalize_inner(FinishStatus::Failed, Some(err.into()))
            .await;
    }

    async fn finalize_inner(self, status: FinishStatus, err: Option<String>) {
        if !self.finalized.swap(true, Ordering::Relaxed) {
            self.reg.finalize(self.task_id, status, err).await;
        }
    }
}

impl Drop for RegistrationGuard {
    fn drop(&mut self) {
        // Only fire the cancellation finalize if neither complete() nor
        // fail() got there first. The atomic swap doubles as a
        // double-finalize guard: register() → complete() → drop is a
        // no-op here.
        if !self.finalized.swap(true, Ordering::Relaxed) {
            let reg = Arc::clone(&self.reg);
            let id = self.task_id;
            tokio::spawn(async move {
                reg.finalize(id, FinishStatus::Cancelled, None).await;
            });
        }
    }
}
