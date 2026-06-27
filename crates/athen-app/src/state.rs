//! Application state management.
//!
//! Builds the coordinator, LLM router, and risk evaluator, wiring them
//! together as the composition root for the Athen desktop app.
//! Configuration is loaded from TOML files (`~/.athen/` or `./config/`)
//! with environment variable overrides.

use std::collections::{HashMap, HashSet};
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use uuid::Uuid;

use async_trait::async_trait;
use futures::FutureExt;
use serde::Serialize;
use tokio::sync::{Mutex, RwLock};
use tracing::{info, warn};

use athen_contacts::trust::TrustManager;
use athen_core::config::{AthenConfig, AuthType, Bundle, ProfileConfig, ACTIVE_BUNDLE_KEY};
use athen_core::config_loader;
use athen_core::traits::notification::NotificationChannel;
use athen_persistence::contacts::SqliteContactStore;

use crate::notifier::{InAppChannel, NotificationOrchestrator, TelegramChannel};
use athen_coordinador::Coordinator;
use athen_core::error::Result;
use athen_core::llm::{
    BudgetStatus, ChatMessage, LlmRequest, LlmResponse, LlmStream, MessageContent, ModelProfile,
    Role,
};
use athen_core::traits::llm::{LlmProvider, LlmRouter};
use athen_llm::budget::BudgetTracker;
use athen_llm::providers::anthropic::AnthropicProvider;
use athen_llm::providers::deepseek::DeepSeekProvider;
use athen_llm::providers::google::GoogleProvider;
use athen_llm::providers::llamacpp::LlamaCppProvider;
use athen_llm::providers::ollama::OllamaProvider;
use athen_llm::providers::openai::OpenAiCompatibleProvider;
use athen_llm::quirks::seed as quirks_seed;
use athen_llm::router::DefaultLlmRouter;
use athen_mcp::McpRegistry;
use athen_memory::Memory;
use athen_persistence::arcs::ArcStore;
use athen_persistence::calendar::CalendarStore;
use athen_persistence::grants::GrantStore;
use athen_persistence::mcp::McpStore;
use athen_persistence::Database;
use athen_risk::llm_fallback::LlmRiskEvaluator;
use athen_risk::CombinedRiskEvaluator;
use athen_web::{
    BraveSearch, DuckDuckGoSearch, MultiSearchProvider, ProviderSlot, TavilySearch,
    WebSearchProvider,
};

use crate::file_gate::PendingGrants;

/// Poison-recovery helpers for the long-lived `std::sync` locks below.
///
/// These locks guard simple data holders (a hot-swappable provider `Arc`, an
/// optional sender, a shutdown channel) — there are no multi-step invariants
/// that a mid-operation panic could leave half-applied. A poisoned guard here
/// therefore means "some other thread panicked while holding the lock", NOT
/// "the protected data is corrupt". Athen is a long-running daemon: a single
/// poisoned lock in a hot path would otherwise turn every later `.expect()`
/// into a cascading panic and brick the UI for the rest of the session. So we
/// recover the inner guard (`into_inner`) instead of propagating the poison.
trait LockRecover {
    type Guard<'a>
    where
        Self: 'a;
    /// Lock, recovering the guard even if the lock was poisoned by a prior
    /// panic. See [`LockRecover`] for why this is safe for these holders.
    fn lock_recover(&self) -> Self::Guard<'_>;
}

trait RwLockRecover {
    type Read<'a>
    where
        Self: 'a;
    type Write<'a>
    where
        Self: 'a;
    /// Read-lock, recovering the guard even if poisoned. See [`LockRecover`].
    fn read_recover(&self) -> Self::Read<'_>;
    /// Write-lock, recovering the guard even if poisoned. See [`LockRecover`].
    fn write_recover(&self) -> Self::Write<'_>;
}

impl<T> LockRecover for std::sync::Mutex<T> {
    type Guard<'a>
        = std::sync::MutexGuard<'a, T>
    where
        T: 'a;
    fn lock_recover(&self) -> Self::Guard<'_> {
        self.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl<T> RwLockRecover for std::sync::RwLock<T> {
    type Read<'a>
        = std::sync::RwLockReadGuard<'a, T>
    where
        T: 'a;
    type Write<'a>
        = std::sync::RwLockWriteGuard<'a, T>
    where
        T: 'a;
    fn read_recover(&self) -> Self::Read<'_> {
        self.read().unwrap_or_else(|e| e.into_inner())
    }
    fn write_recover(&self) -> Self::Write<'_> {
        self.write().unwrap_or_else(|e| e.into_inner())
    }
}

/// Per-profile embedding cache: profile id → (the profile's `updated_at`
/// at the time we cached, the embedding vector). The `updated_at` doubles
/// as the cache key — when a user edits a profile we re-embed.
pub type ProfileEmbeddingCache =
    Arc<tokio::sync::RwLock<HashMap<String, (chrono::DateTime<chrono::Utc>, Vec<f32>)>>>;

/// Per-arc queue of user messages submitted while a task is running.
/// Inserted by `send_message` at task start, drained by the executor
/// between iterations, removed on task completion. Lets the user steer
/// the running agent mid-task without cancelling and restarting.
pub type PendingInputSlot = std::sync::Arc<std::sync::Mutex<Vec<String>>>;

/// A Telegram-triggered Deep Research request parked between the `/deepresearch`
/// command (which sends the depth-choice keyboard) and the owner tapping a depth
/// button (which carries this entry's token in its `callback_data`). See
/// [`AppState::stash_pending_deep_research`].
pub(crate) struct PendingDeepResearch {
    pub arc_id: String,
    pub chat_id: i64,
    pub question: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Maps coordinator task ids to the arc the originating sense event
/// landed in. The sense_router populates this when it hands an event to
/// the coordinator; the dispatch loop consumes it to know which arc to
/// persist the executor's reply into. We need this side table because
/// `Task` itself doesn't carry an arc id (and the architecture rule says
/// not to extend `Task` in athen-core for app-layer concerns).
pub type TaskArcMap = Arc<tokio::sync::RwLock<HashMap<Uuid, String>>>;

/// IMAP coordinates needed to mark a single message `\Seen` after the
/// agent successfully acts on it. Stored in [`PendingEmailMarks`] keyed
/// by the coordinator task id.
#[derive(Debug, Clone)]
pub struct EmailMarkInfo {
    pub uid: u32,
    pub folder: String,
}

/// Maps coordinator task ids → the IMAP UID/folder to mark `\Seen` on
/// success. Populated by the sense router for any decision that could
/// lead to an autonomous run (`SilentApprove`, `NotifyAndProceed`,
/// `HumanConfirm`); consumed by the dispatch loop after the agent
/// finishes successfully. Failed runs drop the entry without marking,
/// so the next IMAP poll re-triggers the email.
pub type PendingEmailMarks = Arc<tokio::sync::RwLock<HashMap<Uuid, EmailMarkInfo>>>;

/// Maps coordinator task ids → the wake-up id that originated them. Set
/// by [`crate::wakeup_sink::CoordinatorWakeupSink`] alongside
/// [`TaskArcMap`]; read by the dispatch loop so it can fetch the full
/// `Wakeup` row and apply per-fire restrictions (autonomy band, tool
/// allowlists, contact allowlists). `None` for sense-originated and
/// user-driven tasks. We don't extend `Task` for this — the rule is
/// app-layer concerns stay in side tables.
pub type TaskWakeupMap = Arc<tokio::sync::RwLock<HashMap<Uuid, Uuid>>>;

/// Wrapper to share the router via `Arc<RwLock<Arc<...>>>` while satisfying
/// the `LlmRouter` trait.  The `RwLock` allows the inner router to be swapped
/// at runtime (e.g. when the user switches active provider).
pub(crate) struct SharedRouter(pub Arc<RwLock<Arc<DefaultLlmRouter>>>);

#[async_trait]
impl LlmRouter for SharedRouter {
    async fn route(&self, request: &LlmRequest) -> Result<LlmResponse> {
        let router = self.0.read().await.clone();
        router.route(request).await
    }
    async fn route_streaming(&self, request: &LlmRequest) -> Result<LlmStream> {
        let router = self.0.read().await.clone();
        router.route_streaming(request).await
    }
    async fn budget_remaining(&self) -> Result<BudgetStatus> {
        let router = self.0.read().await.clone();
        router.budget_remaining().await
    }
}

/// A task that has been flagged for human approval by the risk system.
#[derive(Debug, Clone, Serialize)]
pub struct PendingApproval {
    pub task_id: String,
    pub description: String,
    pub risk_score: f64,
    pub risk_level: String,
}

/// In-flight approval task ids: a task is inserted when its execution
/// helper starts and removed when it finishes. Both `approve_task` (the
/// in-app Tauri command) and `spawn_router_approval` (the Telegram path)
/// race to drive execution after a risk-flagged task is approved; without
/// a guard the same approval would execute twice — once for each channel.
/// First caller to insert wins, the other no-ops.
pub type InflightApprovals = Arc<Mutex<HashSet<Uuid>>>;

/// Live snapshot of the UI-controlled Remote Access runtime (Settings →
/// Remote Access). Read by the `remote_access_status` command; written by
/// [`AppState::start_remote_access`] / [`AppState::stop_remote_access`].
/// See [`docs/REMOTE_ACCESS.md`].
#[derive(Clone, Debug, Default, Serialize)]
pub struct RemoteAccessStatus {
    /// Whether the HTTP listener is currently bound.
    pub listening: bool,
    /// `http://127.0.0.1:<port>` when listening.
    pub local_url: Option<String>,
    /// The `*.trycloudflare.com` URL once the quick-tunnel is up.
    pub tunnel_url: Option<String>,
    /// Whether a usable `cloudflared` binary was found/installed.
    pub cloudflared_installed: bool,
    /// Last tunnel/listener error surfaced to the panel (never a secret).
    pub last_error: Option<String>,
}

/// Top-level application state managed by Tauri.
pub struct AppState {
    pub coordinator: Arc<Coordinator>,
    /// Live security posture (mode + thresholds), hot-swappable so a
    /// Settings → General save applies without a restart.
    ///
    /// Enforcement (task #312): `SecurityMode` is read at task creation via
    /// `resolve_security_mode_for_arc` (the live `.mode` here ⊕ the arc's
    /// `security_mode_override`), then drives two gates —
    /// `coerce_for_security_mode` at the coordinator triage decision and the
    /// per-action shell gate in the executor (`shell_upstream_for_mode`).
    /// New-arcs-only: the effective mode is snapshotted at creation (the same
    /// boundary where `ToolRegistryDeps` / `ApprovedTaskCtx` snapshot the
    /// other live components), so a running arc keeps the posture it started
    /// with. The `auto_approve_below` / `max_steps_per_task` /
    /// `max_task_duration_minutes` thresholds in `SecurityConfig` remain
    /// unwired — only `mode` is load-bearing today.
    pub security: arc_swap::ArcSwap<athen_core::config::SecurityConfig>,
    /// Cached parsed `AthenConfig` (the plain, NON-vault-hydrated
    /// `load_config()` result), so the autonomous dispatch hot path doesn't
    /// re-read + re-parse + re-migrate the TOML from disk on every dispatched
    /// task. Wrapped in `Arc<ArcSwap<_>>` so the dispatch loop (which can't
    /// borrow `self`) can clone a cheap handle and `.load()` it lock-free per
    /// task, while live Settings saves swap a freshly-loaded config in via
    /// [`Self::reload_config_cache`]. Holds the plain (un-hydrated) config to
    /// exactly match what the old per-task `load_config()` returned; consumers
    /// that need vault secrets (e.g. the email-mark-seen branch) still hydrate
    /// an owned clone locally.
    pub config_cache: Arc<arc_swap::ArcSwap<AthenConfig>>,
    /// The LLM router, wrapped in `RwLock` so it can be swapped at runtime
    /// when the user switches active provider.
    pub router: Arc<RwLock<Arc<DefaultLlmRouter>>>,
    /// The ID of the currently active LLM provider (e.g. "deepseek", "ollama").
    pub active_provider_id: Mutex<String>,
    /// In-memory conversation history for the current session.
    pub history: Mutex<Vec<ChatMessage>>,
    /// The user's original message for a task pending approval, so it can
    /// be replayed through the executor once approved.
    pub pending_message: Mutex<Option<String>>,
    /// The synthesized `event_id` for composer uploads attached to a
    /// pending-approval turn. On approval, the dispatched task persists
    /// the user-message arc entry — this lets that persist stamp the
    /// `attachment_event_id` metadata so reload-time thumbnail
    /// hydration still works in the approval flow. Cleared alongside
    /// `pending_message` after replay.
    pub pending_upload_event_id: Mutex<Option<uuid::Uuid>>,
    /// The model name reported to the frontend (from config or default).
    pub model_name: Mutex<String>,
    /// Current active Arc identifier (format: `arc_YYYYMMDD_HHMMSS`).
    pub active_arc_id: Mutex<String>,
    /// Currently active Project identifier, if any. `None` means the
    /// active arc is not scoped to a Project. Mirrors `active_arc_id`'s
    /// `Mutex` type (tokio). Part of the Projects feature substrate.
    pub active_project_id: Mutex<Option<String>>,
    /// Project-summary compaction mode: `"auto"` (fold on arc-leave),
    /// `"manual"` (only the "Update summary now" button folds), or
    /// `"off"` (never fold). Read by the `maybe_fold_leaving_arc` gate and
    /// the `update_project_summary` command. Surfaced to the UI via the
    /// `get_project_summary_mode` / `set_project_summary_mode` commands.
    pub project_summary_mode: Mutex<String>,
    /// Persistent Arc storage backed by SQLite.
    pub arc_store: Option<ArcStore>,
    /// Persistent calendar event storage backed by SQLite.
    pub calendar_store: Option<CalendarStore>,
    /// Trust manager for contact-aware risk evaluation and contact management.
    pub trust_manager: Option<TrustManager>,
    /// Direct access to the shared contact store for operations that
    /// TrustManager doesn't expose (unblock, delete).
    pub contact_store: Option<SqliteContactStore>,
    /// Keep the database alive so the connection is not dropped.
    _database: Option<Database>,
    /// Cancellation flag for the currently running agent executor.
    /// Set to `true` to cancel the in-progress task immediately.
    pub cancel_flag: Arc<AtomicBool>,
    /// Per-arc queue of user messages submitted while a task is running.
    /// Created on task start, drained by the executor between iterations,
    /// removed on task completion. Lets the user steer the agent mid-task
    /// without cancelling.
    pub pending_user_inputs:
        std::sync::Arc<tokio::sync::RwLock<std::collections::HashMap<String, PendingInputSlot>>>,
    /// Telegram-triggered Deep Research requests awaiting the owner's depth
    /// choice (the inline-keyboard buttons). Keyed by a short token carried in
    /// the button `callback_data`; consumed when a depth button is tapped.
    pub(crate) pending_deep_research:
        std::sync::Mutex<std::collections::HashMap<String, PendingDeepResearch>>,
    /// Shutdown sender for the email monitor background task. Behind a Mutex
    /// so the monitor can be stopped + respawned from a `&self` Settings-save
    /// handler (`restart_email_monitor`) without an app restart.
    pub email_shutdown: std::sync::Mutex<Option<tokio::sync::broadcast::Sender<()>>>,
    /// Shutdown sender for the Telegram monitor background task. Behind a Mutex
    /// for the same live-restart reason as `email_shutdown`.
    pub telegram_shutdown: std::sync::Mutex<Option<tokio::sync::broadcast::Sender<()>>>,
    /// Shutdown sender for the UI-controlled Remote Access HTTP listener
    /// (Settings → Remote Access). Oneshot so it can be started/stopped from a
    /// `&self` Settings-save handler without an app restart. See
    /// [`docs/REMOTE_ACCESS.md`].
    pub remote_access_shutdown: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    /// Live cloudflared quick-tunnel handle; killing it (Drop or `stop`) tears
    /// down the public link. `None` when no tunnel is up.
    pub tunnel: std::sync::Mutex<Option<crate::tunnel::TunnelHandle>>,
    /// Snapshot of the Remote Access runtime for the status command/UI poll.
    pub remote_access_status: std::sync::Mutex<RemoteAccessStatus>,
    /// Shutdown sender for the calendar monitor background task. Set by
    /// [`Self::start_calendar_monitor`]; consumed by [`Self::shutdown_all`].
    pub calendar_shutdown: Option<tokio::sync::broadcast::Sender<()>>,
    /// Shutdown sender for the remote-calendar sync loops. Set by
    /// [`Self::start_calendar_sync`]; consumed by [`Self::shutdown_all`].
    pub calendar_sync_shutdown: Option<tokio::sync::broadcast::Sender<()>>,
    /// Shutdown sender for the attachment TTL purger loop. Set by
    /// [`Self::start_attachment_purger`]; consumed by [`Self::shutdown_all`].
    pub attachment_purger_shutdown: Option<tokio::sync::broadcast::Sender<()>>,
    /// Notification orchestrator for delivering notifications through the
    /// best available channel (in-app, Telegram, etc.) with quiet-hours
    /// support and escalation.  Initialized after setup via `init_notifier`.
    /// Hot-swappable: `save_notification_settings` rebuilds it (channels +
    /// preferred order + quiet hours) and stores it so the change applies
    /// without a restart. `ArcSwapOption` (not `RwLock`) because it's a
    /// concrete type read at many `if let Some(..)` sites inside async fns —
    /// `.load_full()` hands back an owned `Option<Arc<_>>` with no guard to
    /// hold across `.await`.
    pub notifier: arc_swap::ArcSwapOption<NotificationOrchestrator>,
    /// Approval router for routing approve/deny questions to the user
    /// across reply channels (InApp + Telegram), with escalation when
    /// the primary channel doesn't answer in time.
    pub approval_router: Option<Arc<crate::approval::ApprovalRouter>>,
    /// Direct handle on the InApp approval sink so the
    /// `submit_approval` Tauri command can resolve pending questions
    /// when the frontend's approve/deny UI fires.
    pub inapp_approval_sink: Option<Arc<crate::approval::InAppApprovalSink>>,
    /// Direct handle on the Telegram approval sink so the Telegram
    /// poll loop can forward `callback_query` events back to it.
    pub telegram_approval_sink: Option<Arc<crate::approval::TelegramApprovalSink>>,
    /// Persistent semantic memory (vector search + knowledge graph).
    /// Used for auto-injecting relevant context before LLM calls and
    /// auto-remembering important interactions after task completion.
    pub memory: Option<Arc<Memory>>,
    /// MCP runtime registry. Holds enabled-state and lazy-spawned client
    /// connections for branded MCP servers (Files, etc.).
    pub mcp: Arc<McpRegistry>,
    /// SQLite-backed persistence for which MCPs the user has enabled.
    pub mcp_store: Option<McpStore>,
    /// Directory of per-group markdown tool schemas (typically
    /// `~/.athen/tools/`). The agent reads `<dir>/<group>.md` on demand for
    /// any tool whose schema isn't already revealed. Refreshed at startup
    /// and after every MCP enable/disable.
    pub tool_doc_dir: Option<std::path::PathBuf>,
    /// Per-arc and global directory grants. Backs the path-permission gate
    /// and the settings UI for managing grants. Always wired against the
    /// same SQLite connection as `_database`.
    pub grant_store: Option<Arc<GrantStore>>,
    /// SQLite-backed agent profile store. Holds the seeded `default`
    /// profile plus any user-authored profiles. Looked up per-arc via
    /// `arcs.active_profile_id` so each conversation can run under its
    /// own persona + tool surface.
    pub profile_store: Option<Arc<athen_persistence::profiles::SqliteProfileStore>>,
    /// SQLite-backed identity store. Holds the user's hand-maintained
    /// personality / rules / knowledge / team statements (plus any custom
    /// categories). Read at prompt-build time and folded into the static
    /// system header so every agent shares the same "who Athen is".
    pub identity_store: Option<Arc<athen_persistence::identity::SqliteIdentityStore>>,
    /// SQLite-backed project store. Holds the user's Projects (the
    /// ChatGPT/Claude-style containers that group many arcs around common
    /// work). Built from `database` like `identity_store`. Part of the
    /// Projects feature substrate; see `docs/PROJECTS.md`.
    pub project_store: Option<Arc<athen_persistence::projects::ProjectStore>>,
    /// Filesystem + SQLite skill store. Bodies live on disk under
    /// `<data_dir>/skills/<slug>/SKILL.md`; the index is in SQLite. Read at
    /// prompt-build time to inject the SKILLS listing (name+description per
    /// skill, profile-filtered) into the static prefix, and at agent-call
    /// time when `load_skill(slug)` pulls a body. Distinct from
    /// [`identity_store`] (always-on persona) and from `memory`
    /// (auto-recalled episodic facts). See `docs/SKILLS.md`.
    pub skill_store: Option<Arc<athen_persistence::skills::SqliteSkillStore>>,
    /// SQLite-backed wake-up store. Holds scheduled / recurring / one-shot
    /// proactive triggers (see `docs/WAKEUPS.md`). Read by the wake-up
    /// scheduler background task; written by Tauri commands and (Phase 5)
    /// the agent's `create_wakeup` tool.
    pub wakeup_store: Option<Arc<athen_persistence::wakeups::SqliteWakeupStore>>,
    /// Shutdown signal for the wake-up scheduler loop. `None` until
    /// `start_wakeup_scheduler` has been called. Sending on this channel
    /// causes the scheduler to exit at the next select boundary.
    /// Wrapped in `std::sync::Mutex` so `shutdown_all` (which takes `&self`)
    /// can `take()` and `send` without needing `&mut self`. The
    /// oneshot sender isn't Clone, so unlike the broadcast channels we
    /// can't just clone-and-send.
    pub wakeup_scheduler_shutdown: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    /// Embedding provider used for semantic profile routing. Always wired
    /// to a router that falls back to keyword embeddings when no neural
    /// provider is available, so the per-call code path can assume `Some`.
    /// Wrapped in `Arc` so background tasks (e.g. sense_router) can hold a
    /// reference for the lifetime of an async task.
    /// Hot-swappable: `save_embedding_settings` rebuilds the embedding router
    /// from current config and swaps it under the lock, so an embedding-mode
    /// change applies without a restart. Read via `.read()` per use.
    /// Shared hot-swappable embedder cell — see [`EmbedderCell`]. The memory
    /// store holds a [`SwappableEmbedder`] over the *same* cell, so
    /// `reload_embedder` updates both the profile path and memory at once.
    pub profile_embedder: EmbedderCell,
    /// In-memory cache of embedded profile text, keyed by profile id and
    /// invalidated by the profile's `updated_at`. Populated lazily during
    /// routing — embedding 12 short strings is cheap, but caching makes
    /// repeat routing on the same profile set instantaneous.
    pub profile_embedding_cache: ProfileEmbeddingCache,
    /// Outstanding grant requests parked waiting for the user. Each entry
    /// holds a oneshot sender that resolves with the user's choice.
    pub pending_grants: PendingGrants,
    /// Long-lived map of processes started via `shell_spawn`, shared into
    /// every per-message `ShellToolRegistry` via
    /// `with_spawned_processes`. Without this the spawn-tracking HashMap
    /// would be lost between messages — the model could spawn a server in
    /// turn N but be unable to kill or inspect it in turn N+1.
    ///
    /// Cleared on graceful shutdown via [`Self::shutdown_all`] →
    /// [`athen_agent::kill_all_spawned`], and persisted continuously to
    /// [`Self::pidfile_path`] via [`Self::spawn_persistence`] so a crash
    /// leaves a recoverable record for next startup's
    /// [`crate::spawn_pidfile::reconcile_orphans`] sweep.
    pub spawned_processes: athen_agent::SpawnedProcessMap,
    /// Persistence hook fired on every `spawned_processes` mutation. Wired
    /// to a JSON pidfile under `<data_dir>/spawned_pids.json` so a
    /// crash/power-loss leaves behind a recoverable record of orphans.
    /// `None` only in CLI/test contexts with no data dir.
    pub spawn_persistence: Option<Arc<dyn athen_agent::SpawnPersistenceHook>>,
    /// Resolved path to the persistent spawn pidfile. Stored alongside
    /// `spawn_persistence` so [`Self::shutdown_all`] can write the empty
    /// snapshot one last time after `kill_all_spawned` returns. `None`
    /// when no data dir is configured.
    pub pidfile_path: Option<PathBuf>,
    /// Single-slot hint recording the most recent outbound Telegram
    /// notification's arc + timestamp. Written by `TelegramChannel::send`,
    /// read by `execute_owner_telegram_message` to bias arc matching for
    /// short follow-ups that arrive right after a notification fires.
    /// See `docs/MULTI_INTENT_ROUTING.md` for the multi-arc extension.
    pub telegram_outbound_hint: crate::notifier::TelegramOutboundHint,
    /// Per-`chat_id` Telegram transcript store. Records inbound +
    /// outbound messages independently of arc routing, so when a new
    /// owner-Telegram message arrives we can prepend the last 4 turns
    /// as system context — giving the agent continuity even when arc
    /// routing picks the wrong arc (or creates a fresh one).
    ///
    /// `None` only on test/CLI builds without a data dir. Future:
    /// gate injection behind small-vs-large model size modes so tight
    /// context-window models don't pay the ~500-token cost.
    pub telegram_chat_log:
        Option<std::sync::Arc<athen_persistence::telegram_chat_log::TelegramChatLogStore>>,
    /// Approvals currently being executed. See [`InflightApprovals`].
    pub inflight_approvals: InflightApprovals,
    /// Maps coordinator task ids to the arc the originating sense
    /// event landed in. Populated by `sense_router::process_sense_event`
    /// when it dispatches an autonomous task; consumed by the dispatch
    /// loop to persist replies into the right arc.
    pub task_arc_map: TaskArcMap,
    /// Maps task ids → the wake-up id that fired them. Populated by
    /// `CoordinatorWakeupSink`; consumed by the dispatch loop so the
    /// executor can apply per-fire restrictions (autonomy band, tool /
    /// contact allowlists). See [`TaskWakeupMap`].
    pub task_wakeup_map: TaskWakeupMap,
    /// Maps task ids → the IMAP UID/folder to flag `\Seen` once the
    /// agent successfully acts on the originating email. Populated by
    /// the sense router for any decision that could lead to an
    /// autonomous run; drained by `start_dispatch_loop` after success.
    /// Failed runs drop the entry (no mark), so the email re-triggers
    /// on the next IMAP poll — that's the source of truth for
    /// "still needs handling".
    pub pending_email_marks: PendingEmailMarks,
    /// Notifies the dispatch loop when a new sense-originated task has
    /// been enqueued. The loop also wakes on a 2-second tick as a
    /// belt-and-brace; `notify_one` keeps latency low when events do
    /// arrive between ticks.
    pub dispatch_signal: Arc<tokio::sync::Notify>,
    /// Shutdown sender for the autonomous dispatch loop. `None` until
    /// `start_dispatch_loop` has been called.
    pub dispatch_loop_shutdown: Option<tokio::sync::broadcast::Sender<()>>,
    /// Arc compactor — the executor's gateway into arc history. The
    /// context-build path goes through `load_context_view` so the
    /// compaction summary (and tool-cache) replace raw entries when an
    /// arc has been compacted. Direct reads of `arc_store.load_entries`
    /// from the executor path are forbidden — see
    /// `docs/ARC_COMPACTION.md` §8 ("the discipline rule"). Wired only
    /// when `arc_store` is also present; `None` falls back to
    /// load_entries-based context (for legacy boot paths and tests).
    pub compactor: Option<Arc<dyn athen_core::traits::compaction::ArcCompactor>>,
    /// Web search backend handed to every per-arc tool registry. Built once
    /// from `config.web_search` so all three call sites
    /// (`refresh_tools_doc`, `build_tool_registry`, owner-Telegram exec)
    /// stay consistent. The chain prefers Brave → Tavily → DDG; cooldowns
    /// for keyed providers are tracked inside the `MultiSearchProvider`.
    /// Hot-swappable: `save_web_search_settings` rebuilds the provider chain
    /// (Brave → Tavily → DDG …) from current config and swaps it under the
    /// lock, so a key change applies without a restart. Read via `.read()`.
    pub web_search: std::sync::RwLock<Arc<dyn WebSearchProvider>>,
    /// SMTP outbound. Built from `config.email` when SMTP fields are
    /// populated. `None` means the `email_send` tool will refuse with a
    /// "not configured" error until the user wires SMTP via Settings.
    /// Hot-swappable: `save_smtp_settings` rebuilds and swaps this under the
    /// lock so SMTP changes apply without a restart. New arcs/tasks read the
    /// current sender via `.read()`; in-flight arcs keep the one they already
    /// snapshotted. (`RwLock` rather than `arc-swap` because a `dyn` trait
    /// object is a fat pointer that arc-swap's single-word atomic can't hold.)
    pub email_sender:
        std::sync::RwLock<Option<Arc<dyn athen_core::traits::email_sender::EmailSender>>>,
    /// Outbound Telegram. Built from `config.telegram` when the bot
    /// token is populated. The bot's owner chat (from `owner_user_id`)
    /// is the default destination — `send_telegram` calls without an
    /// explicit `chat_id` go there and skip the approval gate.
    /// `None` means the `send_telegram` tool will refuse with a "not
    /// configured" error until the user wires the bot via Settings.
    /// Hot-swappable: `save_telegram_settings` rebuilds and swaps this.
    pub telegram_sender:
        std::sync::RwLock<Option<Arc<dyn athen_core::traits::telegram_sender::TelegramSender>>>,
    /// Encrypted credential vault. Backs registered HTTP endpoints,
    /// IMAP/SMTP credentials, OAuth tokens, and any other at-rest secret.
    /// Always `Some` whenever a data directory is available; falls back
    /// from the OS keychain to an encrypted file if the keychain is
    /// unreachable. `None` only on test/CLI builds without a data dir.
    /// Encrypted credential vault — primary store for secrets going
    /// forward. Hydrates `config` at startup (see `vault_creds::
    /// hydrate_secrets_from_vault`) so the existing build paths are
    /// unchanged. Save commands write here and blank the corresponding
    /// `config.toml` field, so secrets stop appearing on disk in
    /// plaintext after the next save.
    pub vault: Option<Arc<dyn athen_core::traits::vault::Vault>>,
    /// Resolves `AgentProfile.github_identity` → env-var bundle so the
    /// agent's `shell_execute` git/gh commands authenticate as the
    /// configured bot account (or as the user). Built when the vault is
    /// available; `None` on builds with no data dir.
    pub github_identity_resolver: Option<Arc<dyn athen_agent::tools::GithubIdentityResolver>>,
    /// Registered HTTP endpoints store. Backs the `http_request` agent
    /// tool and the Settings → Cloud APIs panel. `None` only on test/CLI
    /// builds without a data dir.
    pub http_endpoint_store:
        Option<Arc<athen_persistence::http_endpoints::SqliteHttpEndpointStore>>,
    /// Process-wide rate limiter for `http_request`. Shared across every
    /// per-arc registry so per-endpoint per-minute caps are honoured even
    /// when multiple arcs run concurrently.
    pub http_rate_limiter: Arc<crate::http_rate_limiter::HttpRateLimiter>,
    /// Long-lived `reqwest` client used by `http_request` so connection
    /// pooling survives across arcs and tool calls.
    pub http_client: reqwest::Client,
    /// Path to the auto-generated `cloud_apis.md` catalogue. Rendered
    /// from the registered-endpoint store + preset library so the agent
    /// can `read` it on demand to discover what endpoints exist, what
    /// auth shape each takes, and a sample path. The `http_request`
    /// tool description carries a one-line pointer at this file so
    /// the agent doesn't try blindly. Refreshed at startup and after
    /// every endpoint mutation.
    pub cloud_apis_doc_path: Option<std::path::PathBuf>,
    /// Durable record of every agent execution. Backs the "watch the
    /// agents work" panel's recent-runs view and is pruned to a 30-day
    /// window by [`AppState::start_agent_run_pruner`]. `None` only on
    /// CLI/test builds without a data dir.
    pub agent_run_store: Option<Arc<athen_persistence::agent_runs::SqliteAgentRunStore>>,
    /// Live in-memory view of agents currently executing. Wired after
    /// startup via [`AppState::init_agent_registry`] (needs an
    /// `AppHandle` to emit the `agents-changed` event). Read by every
    /// executor entry point so it can register/finalize and by the
    /// `list_active_agents` Tauri command.
    pub agent_registry: Option<Arc<crate::agent_registry::AgentRegistry>>,
    /// Git-backed snapshot store powering agent-action undo. `None` only
    /// on builds without a data dir (CLI/tests). When present, the tool
    /// registry's `write`/`edit` hooks call into it before the tool
    /// mutates the filesystem so the user can revert later.
    pub checkpoint_store: Option<Arc<dyn athen_core::traits::checkpoint::CheckpointStore>>,
    /// Proactive hint dismissal store. Tracks which setup nudges the user
    /// has permanently dismissed so the background checker skips them.
    pub hint_dismissal_store: Option<athen_persistence::hint_dismissals::HintDismissalStore>,
}

/// Snapshot of every AppState field the per-arc tool registry needs.
///
/// Built once via [`AppState::tool_registry_deps`] and cloned per-spawn
/// (cheap — every field is an `Arc`, a clone-of-Arc'd store, or a
/// `PathBuf`). Routed through [`assemble_app_tool_registry`] so the
/// in-app dispatch path AND the owner-Telegram trust-bypass path go
/// through one assembly site — adding a new tool only requires
/// touching the helper.
#[derive(Clone)]
pub(crate) struct ToolRegistryDeps {
    pub spawned_processes: athen_agent::SpawnedProcessMap,
    pub spawn_persistence: Option<Arc<dyn athen_agent::SpawnPersistenceHook>>,
    pub web_search: Arc<dyn WebSearchProvider>,
    pub email_sender: Option<Arc<dyn athen_core::traits::email_sender::EmailSender>>,
    pub telegram_sender: Option<Arc<dyn athen_core::traits::telegram_sender::TelegramSender>>,
    pub owner_check: Option<Arc<dyn athen_agent::OwnerDestinationCheck>>,
    pub github_identity_resolver: Option<Arc<dyn athen_agent::tools::GithubIdentityResolver>>,
    pub checkpoint_store: Option<Arc<dyn athen_core::traits::checkpoint::CheckpointStore>>,
    pub grant_store: Option<Arc<GrantStore>>,
    pub approval_router: Option<Arc<crate::approval::ApprovalRouter>>,
    pub telegram_approval_sink: Option<Arc<crate::approval::TelegramApprovalSink>>,
    pub telegram_outbound_hint: crate::notifier::TelegramOutboundHint,
    pub telegram_chat_log: Option<Arc<athen_persistence::telegram_chat_log::TelegramChatLogStore>>,
    pub pending_grants: PendingGrants,
    pub calendar_store: Option<CalendarStore>,
    pub contact_store: Option<SqliteContactStore>,
    pub memory: Option<Arc<Memory>>,
    pub mcp: Arc<McpRegistry>,
    pub attachment_store: Option<athen_persistence::attachments::AttachmentStore>,
    pub identity_store: Option<Arc<athen_persistence::identity::SqliteIdentityStore>>,
    /// Project store, plumbed alongside `identity_store` so the per-arc
    /// registry build can later resolve an arc's project folder. Wiring of
    /// the actual save/slug resolution is a separate Projects slice.
    pub project_store: Option<Arc<athen_persistence::projects::ProjectStore>>,
    pub skill_store: Option<Arc<athen_persistence::skills::SqliteSkillStore>>,
    pub http_endpoint_store:
        Option<Arc<athen_persistence::http_endpoints::SqliteHttpEndpointStore>>,
    pub vault: Option<Arc<dyn athen_core::traits::vault::Vault>>,
    pub http_rate_limiter: Arc<crate::http_rate_limiter::HttpRateLimiter>,
    pub http_client: reqwest::Client,
    pub cloud_apis_doc_path: Option<std::path::PathBuf>,
    pub calendar_source_store:
        Option<Arc<dyn athen_core::traits::calendar_source_config::CalendarSourceConfigStore>>,
    pub profile_store: Option<Arc<athen_persistence::profiles::SqliteProfileStore>>,
    pub arc_store: Option<ArcStore>,
    pub tool_doc_dir: Option<std::path::PathBuf>,
    pub router: Arc<RwLock<Arc<DefaultLlmRouter>>>,
    pub wakeup_store: Option<Arc<dyn athen_core::traits::wakeup::WakeupStore>>,
    /// Wired only when the voice subsystem is fully assembled (vault +
    /// http_endpoint_store + approval_router present + notifier
    /// optional). Drives the `place_call` agent tool.
    pub notifier: Option<Arc<NotificationOrchestrator>>,
    pub active_provider_id: String,
    /// Live global security posture (`AppState::security.load().mode`),
    /// snapshotted at registry-build time. The base assembler resolves
    /// the per-arc effective mode (`security_mode_override` ⊕ this) and
    /// hands it to the `FileGate` so out-of-workspace writes skip the
    /// approval prompt under `Yolo`. See `resolve_security_mode_for_arc`.
    pub global_security_mode: athen_core::config::SecurityMode,
}

/// Single source of truth for assembling the per-arc agent tool
/// registry. Called by:
///   - [`AppState::build_tool_registry`] (in-app dispatch, sense
///     events, manual chat, wake-up tool inventory).
///   - `execute_owner_telegram_message` (owner-Telegram trust-bypass
///     path that runs without risk/coordinator but with the SAME
///     tool surface).
///
/// Adding a new `.with_*` here picks it up for both paths
/// automatically — that was the whole point of #248.
/// Build the `place_call` telephony deps bundle. Returns `None` when any
/// of the three core wirings (approval router, vault, http endpoint
/// store) is unavailable — the tool then stays hidden from the agent.
///
/// Lives here (next to `assemble_app_tool_registry`) so the in-app and
/// background registry build sites can call into the same code path,
/// matching the #248 consolidation rule. Without this, the wake-up and
/// sense-event registries silently drop `place_call` even when in-app
/// chat advertises it — the kind of registry drift the
/// `feedback_owner_telegram_registry_drift` memory was filed against.
#[allow(clippy::too_many_arguments)] // mirrors the per-arc registry deps; a struct wrapper would churn all 4 call sites
pub(crate) async fn build_telephony_deps(
    arc_id: &str,
    approval_router: Option<Arc<crate::approval::ApprovalRouter>>,
    vault: Option<Arc<dyn athen_core::traits::vault::Vault>>,
    http_endpoint_store: Option<Arc<athen_persistence::http_endpoints::SqliteHttpEndpointStore>>,
    notifier: Option<Arc<crate::notifier::NotificationOrchestrator>>,
    active_provider_id: String,
    security_mode: athen_core::config::SecurityMode,
    identity_store: Option<Arc<athen_persistence::identity::SqliteIdentityStore>>,
) -> Option<crate::place_call::TelephonyDeps> {
    let (router, vault, store) = match (approval_router, vault, http_endpoint_store) {
        (Some(r), Some(v), Some(s)) => (r, v, s),
        _ => return None,
    };
    // Use the full merged loader (config.toml + models.toml) — NOT
    // load_main_config_public(), which reads config.toml only and leaves
    // models.providers / models.bundles empty. Voice LLM resolution
    // (pick_voice_llm) needs the populated Bundle + providers, exactly like
    // a normal chat arc. `cfg.voice` is still sourced from config.toml here,
    // so the voice blob is preserved.
    let mut cfg = load_config();
    // Hydrate provider api_keys from the vault (OS keychain). On-disk
    // models.toml blanks each key to `auth = "None"` post-migration — the
    // live secret only exists in the vault. Without this, a provider whose
    // key lives solely in the keychain (e.g. opencode_go) resolves to an
    // empty api_key and `resolve_llm` wrongly rejects it. Mirrors the
    // startup hydrate in `AppState::new`.
    crate::vault_creds::hydrate_models_from_vault(Some(&vault), &mut cfg.models).await;
    let voice_config: athen_voice::VoiceConfig =
        serde_json::from_value(cfg.voice.clone()).unwrap_or_default();
    let gate: Arc<dyn athen_voice::TelephonyApprovalGate> = Arc::new(
        crate::telephony_gate::RouterTelephonyApprovalGate::new(router, Some(arc_id.to_string()))
            .with_security_mode(security_mode),
    );
    Some(crate::place_call::TelephonyDeps {
        gate,
        vault,
        http_endpoint_store: store,
        notifier,
        active_provider_id,
        voice_config,
        config: cfg,
        identity_store,
    })
}

/// Build the BARE per-arc tool registry — everything up to and including the
/// file gate / telephony, but WITHOUT the delegation or wake-up authoring
/// wrappers. GitHub identity and the active profile are resolved from
/// `arc_id`, so calling this for a sub-arc produces a registry correctly
/// scoped to that arc + its profile. Sub-agents get exactly this (bare ⇒ no
/// `spawn_subagent` ⇒ depth=1). The public `assemble_app_tool_registry`
/// wraps this with delegation + wake-up authoring for top-level agents.
pub(crate) async fn assemble_base_app_tool_registry(
    deps: ToolRegistryDeps,
    arc_id: &str,
    ui: Option<crate::ui_bridge::UiBridge>,
    deep_research_runner: Option<crate::app_tools::DeepResearchRunner>,
) -> Arc<dyn athen_core::traits::tool::ToolRegistry> {
    // Resolve the active profile's GitHub identity once at registry
    // build time so every shell_execute in this arc uses the same
    // creds. Falls back to `None` whenever lookup is unavailable
    // (CLI builds, missing profile_store, etc.) — never errors.
    let github_identity = resolve_github_identity_for_arc(
        deps.profile_store.as_ref(),
        deps.arc_store.as_ref(),
        arc_id,
    )
    .await;
    // Resolve the arc's effective security posture (per-arc override ⊕
    // live global) ONCE here, then thread it into every per-action
    // approval gate (file/email/telegram/toolbox/telephony). Under Yolo
    // each gate skips its prompt and proceeds; HardBlock-equivalent
    // refusals stay upstream in the risk/coordinator gates.
    let security_mode =
        resolve_security_mode_for_arc(deps.arc_store.as_ref(), arc_id, deps.global_security_mode)
            .await;
    // Resolve whether this arc is an active Code-Mode session and, if so,
    // its real repo root. When active, the shadow gix snapshot store is
    // skipped (decision D5 — real git is the undo surface) and the shell
    // cwd + file-tool fs base are pinned to the repo root. When inactive
    // this is (false, None) and every downstream call is byte-identical to
    // pre-Code-Mode behavior.
    let code_mode_root = resolve_code_mode_for_arc(deps.arc_store.as_ref(), arc_id).await;
    // TODO(plan-cake): wire UserNotifier impl that forwards a
    // Notification to the same plumbing proactive_hints.rs uses
    // (app_handle.emit("notification", ...) + NotificationOrchestrator)
    // so the sandbox-fallback warning reaches the user. Pass it via
    // `.with_notifier_opt(deps.user_notifier.clone())`.
    let mut shell = athen_agent::ShellToolRegistry::new()
        .await
        .with_spawned_processes(deps.spawned_processes.clone())
        .with_spawn_persistence_hook_opt(deps.spawn_persistence.clone())
        .with_web_search(deps.web_search.clone())
        .with_email_sender_opt(deps.email_sender.clone())
        .with_telegram_sender_opt(deps.telegram_sender.clone())
        .with_owner_check_opt(deps.owner_check.clone())
        .with_github_identity(github_identity)
        .with_github_identity_resolver_opt(deps.github_identity_resolver.clone())
        // The shadow checkpoint store stays active in Code Mode so agent
        // write/edit/shell actions keep per-action file-level undo (Changes
        // rail). Real git is the *visualization* + manual-discard surface, not
        // a replacement for the shadow store — CODE_MODE.md §6 (b).
        .with_checkpoint_store_opt(deps.checkpoint_store.clone())
        .with_checkpoint_arc_id(arc_id)
        .with_working_dir(code_mode_root.clone())
        .with_fs_base(code_mode_root.clone());
    if let Some(store) = deps.grant_store.clone() {
        let provider = Arc::new(crate::file_gate::ArcWritableProvider {
            arc_id: crate::file_gate::arc_uuid(arc_id),
            store,
        });
        shell = shell.with_extra_writable(provider);
    }
    if let Some(router) = deps.approval_router.clone() {
        shell = shell.with_toolbox_approval(Arc::new(
            crate::file_gate::RouterToolboxApprovalGate::new(
                router.clone(),
                Some(arc_id.to_string()),
            )
            .with_security_mode(security_mode),
        ));
        let gate: Arc<dyn athen_agent::EmailSendApprovalGate> = Arc::new(
            crate::email_gate::RouterEmailApprovalGate::new(
                router.clone(),
                Some(arc_id.to_string()),
            )
            .with_security_mode(security_mode),
        );
        shell = shell.with_email_approval(gate);
        let tg_gate: Arc<dyn athen_agent::tools::TelegramSendApprovalGate> = Arc::new(
            crate::email_gate::RouterTelegramApprovalGate::new(router, Some(arc_id.to_string()))
                .with_security_mode(security_mode),
        );
        shell = shell.with_telegram_approval(tg_gate);
    }
    // Cross-channel arc routing: stamp the outbound hint after a
    // successful send_telegram so the user's Telegram reply lands
    // back in this arc instead of being re-triaged as fresh.
    let tg_recorder: Arc<dyn athen_agent::tools::TelegramOutboundRecorder> =
        Arc::new(crate::email_gate::ArcAwareTelegramOutboundRecorder::new(
            deps.telegram_outbound_hint.clone(),
            Some(arc_id.to_string()),
            deps.telegram_chat_log.clone(),
        ));
    shell = shell.with_telegram_outbound_recorder(tg_recorder);
    let mut registry = crate::app_tools::AppToolRegistry::new(
        shell,
        deps.calendar_store.clone(),
        deps.contact_store.clone(),
        deps.memory.clone(),
    )
    .with_mcp(deps.mcp.clone() as Arc<dyn athen_core::traits::mcp::McpClient>);
    if let Some(arc_s) = deps.arc_store.clone() {
        registry = registry.with_arc_store(arc_s, arc_id);
    }
    if let Some(astore) = deps.attachment_store.clone() {
        registry = registry.with_attachments(astore);
    }
    if let Some(istore) = deps.identity_store.clone() {
        registry = registry.with_identity(istore);
    }
    if let Some(sstore) = deps.skill_store.clone() {
        registry = registry.with_skills(sstore);
    }
    if let (Some(estore), Some(vault)) = (deps.http_endpoint_store.clone(), deps.vault.clone()) {
        registry = registry.with_http_endpoints(
            estore,
            vault,
            deps.http_rate_limiter.clone(),
            deps.http_client.clone(),
            deps.cloud_apis_doc_path.clone(),
        );
    }
    if let Some(cstore) = deps.calendar_source_store.clone() {
        registry = registry.with_calendar_remote(cstore);
    }
    // Standalone vault wiring for setup tools (vault may already be
    // set via with_http_endpoints above, but this covers the case
    // where http endpoints aren't configured yet).
    if let Some(vault) = deps.vault.clone() {
        registry = registry.with_vault_standalone(vault);
    }

    // Telephony wiring for the `place_call` tool. Delegated to the
    // shared helper so the wake-up + sense-event registry builds in
    // `commands.rs` stay in lockstep — drift here is what kept the tool
    // hidden from background paths until 2026-05-28.
    if let (Some(telephony), Some(handle)) = (
        build_telephony_deps(
            arc_id,
            deps.approval_router.clone(),
            deps.vault.clone(),
            deps.http_endpoint_store.clone(),
            deps.notifier.clone(),
            deps.active_provider_id.clone(),
            security_mode,
            deps.identity_store.clone(),
        )
        .await,
        ui.as_ref().and_then(|u| u.tauri_handle()).cloned(),
    ) {
        registry = registry.with_telephony(telephony).with_app_handle(handle);
    }
    // Resolve the active profile so setup tools are conditionally registered.
    let active_profile_id = crate::commands::active_profile_id_for_arc(
        deps.profile_store.as_ref(),
        deps.arc_store.as_ref(),
        arc_id,
    )
    .await;
    registry = registry.with_active_profile_id(active_profile_id);
    // Resolve the arc's active project folder slug (if any) so `save_file`
    // defaults writes into the project workspace. None when the arc has no
    // project or the store is absent ⇒ inert.
    if let (Some(ps), Some(ar)) = (deps.project_store.as_ref(), deps.arc_store.as_ref()) {
        let project_slug = match ar
            .get_arc(arc_id)
            .await
            .ok()
            .flatten()
            .and_then(|m| m.project_id)
        {
            Some(pid) => ps
                .get_project(&pid)
                .await
                .ok()
                .flatten()
                .map(|p| p.folder_slug),
            None => None,
        };
        if project_slug.is_some() {
            registry = registry.with_active_project(project_slug);
        }
    }
    if let Some(grants) = deps.grant_store.clone() {
        // `security_mode` (resolved once at the top of this fn) lets the
        // file gate lower out-of-workspace write prompts under Yolo,
        // mirroring the executor's per-action shell gate.
        let mut gate = crate::file_gate::FileGate::new(
            arc_id.to_string(),
            grants,
            deps.pending_grants.clone(),
            ui.clone(),
        )
        .with_security_mode(security_mode);
        if let Some(ref sink) = deps.telegram_approval_sink {
            gate = gate.with_telegram_approval(sink.clone());
        }
        registry = registry.with_file_gate(Arc::new(gate));
    }

    // Agent-callable deep_research tool: wired only when the caller passes a
    // runner (top-level interactive arcs). `None` for sub-agent / worker /
    // deep-research-base registries keeps the tool unlisted there.
    registry = registry.with_deep_research_runner(deep_research_runner);

    Arc::new(registry)
}

/// Lightweight panic supervisor for a long-lived background monitor loop.
///
/// Each "attempt" runs `factory()` (the full monitor loop) as its own spawned
/// task and awaits its `JoinHandle`. If the task **panics** (`JoinError`) or
/// returns unexpectedly while shutdown has NOT been requested, the supervisor
/// logs the cause and respawns the loop after `backoff` (so a tight crash-loop
/// can't peg the CPU). A clean stop — signalled via `should_stop()` returning
/// `true`, or the JoinHandle being cancelled — ends the supervisor without a
/// respawn.
///
/// This is the *outer* safety net. The dominant panic risk (the inline
/// per-iteration work: `process_sense_event`, `process_updates_with_owner`,
/// the wake-up `sink.fire`) is contained *inside* each loop with
/// `catch_unwind`, so the loop normally survives a panic without ever falling
/// through to the supervisor. The supervisor only fires for panics outside the
/// guarded iteration body (loop scaffolding, `monitor.poll`, etc.).
///
/// `factory` is `FnMut` so it can be called once per attempt to rebuild a
/// fresh loop future (each monitor re-subscribes its broadcast shutdown
/// receiver and re-clones its deps inside the closure).
///
/// ## Backoff contract
///
/// `initial_backoff` is the delay before the *first* respawn; each
/// consecutive unexpected exit doubles it up to [`SUPERVISION_BACKOFF_CAP`]
/// (so a permanently-failing task settles at the ceiling instead of
/// hot-looping the CPU). The backoff is **reset to `initial_backoff` after a
/// run that survived at least [`SUPERVISION_HEALTHY_RUN`]** — i.e. a monitor
/// that ran fine for a while and then dies once doesn't inherit the penalty
/// of an earlier crash storm. A clean stop never sleeps or respawns.
async fn spawn_supervised<F, Fut, S>(
    name: &'static str,
    initial_backoff: std::time::Duration,
    mut should_stop: S,
    mut factory: F,
) where
    F: FnMut() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
    S: FnMut() -> bool + Send + 'static,
{
    let mut backoff = initial_backoff;
    loop {
        if should_stop() {
            info!(
                monitor = name,
                "Supervised monitor stop requested; not respawning"
            );
            return;
        }

        let attempt_started = std::time::Instant::now();
        let handle = tauri::async_runtime::spawn(factory());
        match handle.await {
            Ok(()) => {
                // The loop returned. For our monitors a clean return only
                // happens on a shutdown signal — but be defensive: if the
                // loop exits while no stop was requested, treat it as an
                // unexpected exit and respawn.
                if should_stop() {
                    info!(monitor = name, "Supervised monitor exited cleanly");
                    return;
                }
                warn!(
                    monitor = name,
                    "Supervised monitor returned unexpectedly (no shutdown requested); respawning after backoff"
                );
            }
            Err(join_err) => {
                // `tauri::async_runtime::spawn` surfaces the underlying tokio
                // JoinError as `tauri::Error::JoinError`. A cancelled task
                // (runtime shutting down) must not be respawned.
                if let tauri::Error::JoinError(ref je) = join_err {
                    if je.is_cancelled() {
                        info!(
                            monitor = name,
                            "Supervised monitor task cancelled; not respawning"
                        );
                        return;
                    }
                }
                tracing::error!(
                    monitor = name,
                    error = %join_err,
                    "Supervised monitor task PANICKED at top level; respawning after backoff"
                );
            }
        }

        if should_stop() {
            info!(
                monitor = name,
                "Supervised monitor stop requested after exit; not respawning"
            );
            return;
        }

        // A run that stayed up past the "healthy" threshold proves the task
        // can run — treat the next failure as a fresh incident and reset the
        // ramp. Otherwise keep climbing toward the cap so a tight crash-loop
        // (panic-on-startup) can't peg a core.
        if attempt_started.elapsed() >= SUPERVISION_HEALTHY_RUN {
            backoff = initial_backoff;
        }
        warn!(
            monitor = name,
            backoff_secs = backoff.as_secs(),
            "Supervised monitor respawning after backoff"
        );
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(SUPERVISION_BACKOFF_CAP);
    }
}

/// Ceiling for [`spawn_supervised`]'s exponential backoff. A monitor that
/// keeps dying settles here rather than retrying forever at sub-second
/// intervals.
const SUPERVISION_BACKOFF_CAP: std::time::Duration = std::time::Duration::from_secs(60);

/// A supervised attempt that ran at least this long before exiting is treated
/// as "was healthy"; the backoff ramp resets so an isolated late failure
/// doesn't inherit an earlier crash storm's penalty.
const SUPERVISION_HEALTHY_RUN: std::time::Duration = std::time::Duration::from_secs(120);

/// Build a sub-agent's registry + router factories from the live `AppState`
/// (reached through the [`crate::ui_bridge::UiBridge`] — Tauri managed state
/// on desktop, the published headless singleton in daemon mode). Returns
/// `(None, None)` when no bridge is available (CLI / tests) so delegation
/// falls back to reusing the parent's base registry + shared router.
///
/// Cost note: the router factory rebuilds a provider router only when the
/// sub-arc carries a pin that differs from the global active provider
/// (`arc_router_for`'s fast path returns the shared global cell otherwise).
/// Delegation is rare, so one build per pinned delegation is acceptable.
pub(crate) fn build_subagent_factories(
    ui: Option<&crate::ui_bridge::UiBridge>,
) -> (
    Option<crate::delegation::SubRegistryFactory>,
    Option<crate::delegation::SubRouterFactory>,
) {
    let Some(bridge) = ui else {
        return (None, None);
    };
    let ui_reg = bridge.clone();
    let reg: crate::delegation::SubRegistryFactory = Arc::new(move |sub_arc, _sub_profile_id| {
        let ui = ui_reg.clone();
        Box::pin(async move {
            // Snapshot deps and drop the state borrow before awaiting.
            let deps = ui.app_state().tool_registry_deps();
            // Sub-agents never get the deep_research tool (no runner) — keeps
            // the bare worker registry bare and prevents recursive runs.
            let base =
                assemble_base_app_tool_registry(deps, &sub_arc, Some(ui.clone()), None).await;
            Box::new(crate::delegation::ArcRegistryAdapter(base))
                as Box<dyn athen_core::traits::tool::ToolRegistry>
        })
    });
    let ui_rt = bridge.clone();
    let rt: crate::delegation::SubRouterFactory = Arc::new(move |sub_arc: String| {
        let ui = ui_rt.clone();
        Box::pin(async move {
            // Snapshot the global router + arc store, drop the state borrow,
            // then resolve the sub-arc's effective provider and build its
            // per-arc router (honoring the pin propagated by delegation).
            let (global_router, arc_store, vault) = {
                let state = ui.app_state();
                (
                    Arc::clone(&state.router),
                    state._database.as_ref().map(|db| db.arc_store()),
                    state.vault.clone(),
                )
            };
            let cfg = load_config();
            let active_id = resolve_active_provider(&cfg);
            let target = resolve_effective_provider_for_arc_with_config(
                arc_store.as_ref(),
                &sub_arc,
                &active_id,
                ModelProfile::Powerful,
                &cfg,
            )
            .await;
            arc_router_for(&global_router, &target, &active_id, &cfg, vault.as_ref()).await
        })
    });
    (Some(reg), Some(rt))
}

/// Single source of truth for building a [`crate::delegation::DelegationContext`].
/// Every registry-build site (in-app, owner-Telegram, and the two commands.rs
/// dispatch paths) goes through here, so the sub-agent factory wiring can't
/// drift between them — adding a field to `DelegationContext` means editing
/// this one function, not three call sites.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_delegation_context(
    profile_store: Arc<athen_persistence::profiles::SqliteProfileStore>,
    arc_store: ArcStore,
    identity_store: Option<Arc<athen_persistence::identity::SqliteIdentityStore>>,
    skill_store: Option<Arc<athen_persistence::skills::SqliteSkillStore>>,
    http_endpoint_store: Option<Arc<athen_persistence::http_endpoints::SqliteHttpEndpointStore>>,
    tool_doc_dir: Option<std::path::PathBuf>,
    llm_router: Arc<RwLock<Arc<DefaultLlmRouter>>>,
    parent_arc_id: String,
    ui: Option<crate::ui_bridge::UiBridge>,
    wakeup_restrictions: Option<crate::wakeup_registry::WakeupSubagentRestrictions>,
) -> crate::delegation::DelegationContext {
    let (sub_registry_factory, sub_router_factory) = build_subagent_factories(ui.as_ref());
    crate::delegation::DelegationContext {
        profile_store,
        identity_store,
        skill_store,
        http_endpoint_store,
        arc_store,
        llm_router,
        parent_arc_id,
        tool_doc_dir,
        ui,
        wakeup_restrictions,
        sub_registry_factory,
        sub_router_factory,
    }
}

/// Single source of truth for the per-arc tool registry handed to a
/// top-level agent: the bare base (see [`assemble_base_app_tool_registry`])
/// wrapped with the delegation layer (`spawn_subagent`) and the wake-up
/// authoring layer (`create_wakeup`). Sub-agents get the bare base only, so
/// they cannot delegate further (depth=1).
pub(crate) async fn assemble_app_tool_registry(
    deps: ToolRegistryDeps,
    arc_id: &str,
    ui: Option<crate::ui_bridge::UiBridge>,
) -> Box<dyn athen_core::traits::tool::ToolRegistry> {
    // Clone the bits the delegation + wake-up wrappers need before `deps` is
    // moved into the base assembler.
    let profile_store = deps.profile_store.clone();
    let arc_store_for_delegation = deps.arc_store.clone();
    let identity_store = deps.identity_store.clone();
    let skill_store = deps.skill_store.clone();
    let http_endpoint_store = deps.http_endpoint_store.clone();
    let tool_doc_dir = deps.tool_doc_dir.clone();
    let router = Arc::clone(&deps.router);
    let wakeup_store = deps.wakeup_store.clone();
    let approval_router = deps.approval_router.clone();

    // Build the deep_research runner closure for this top-level arc. Requires a
    // UiBridge (it resolves AppState + emits progress events through it), so
    // CLI / test builds with no bridge get `None` and simply don't see the tool.
    // It first applies the extend-vs-new gate: if a paper already exists and the
    // agent gave no mode, it returns NeedsDecision so the tool can ask the user.
    let dr_runner: Option<crate::app_tools::DeepResearchRunner> = ui.as_ref().map(|bridge| {
        let ui = bridge.clone();
        let arc_id = arc_id.to_string();
        std::sync::Arc::new(
            move |question: String, depth: Option<String>, mode: Option<String>| {
                let ui = ui.clone();
                let arc_id = arc_id.clone();
                Box::pin(async move {
                    let state = ui.app_state();
                    // extend-vs-new: if a paper already exists and no mode was
                    // given, ask first.
                    if mode.is_none() {
                        if let Some(arc_store) = state.arc_store.as_ref() {
                            if let Ok(Some(arc)) = arc_store.get_arc(&arc_id).await {
                                if arc.research_paper_path.is_some() {
                                    return Ok(crate::app_tools::DeepResearchRun::NeedsDecision {
                                        existing_question: arc.research_question,
                                    });
                                }
                            }
                        }
                    }
                    crate::commands::deep_research_core(
                        arc_id,
                        question,
                        depth,
                        mode,
                        state,
                        ui.clone(),
                    )
                    .await
                    .map(crate::app_tools::DeepResearchRun::Done)
                })
                    as std::pin::Pin<
                        Box<
                            dyn std::future::Future<
                                    Output = std::result::Result<
                                        crate::app_tools::DeepResearchRun,
                                        String,
                                    >,
                                > + Send,
                        >,
                    >
            },
        ) as crate::app_tools::DeepResearchRunner
    });

    let base = assemble_base_app_tool_registry(deps, arc_id, ui.clone(), dr_runner).await;

    let with_delegation: Box<dyn athen_core::traits::tool::ToolRegistry> =
        if let (Some(profile_store), Some(arc_store)) = (profile_store, arc_store_for_delegation) {
            let ctx = build_delegation_context(
                profile_store,
                arc_store,
                identity_store,
                skill_store,
                http_endpoint_store,
                tool_doc_dir,
                router,
                arc_id.to_string(),
                ui,
                None,
            );
            Box::new(crate::delegation::DelegationToolRegistry::new(base, ctx))
        } else {
            Box::new(crate::delegation::ArcRegistryAdapter(base))
        };

    // Wake-up authoring layer sits OUTSIDE delegation so a wake-up declaring
    // `spawn_subagent` in its allowlist still works; sits INSIDE the wake-up
    // restriction wrapper (added by the firing path in commands.rs) so a
    // locked-down wake-up can still hide create_wakeup. Skipped when no
    // wakeup_store is wired (CLI / test builds).
    if let Some(store) = wakeup_store {
        let ctx = crate::wakeup_tool::WakeupToolContext {
            wakeup_store: store,
            approval_router,
            parent_arc_id: arc_id.to_string(),
        };
        Box::new(crate::wakeup_tool::WakeupAuthoringRegistry::new(
            with_delegation,
            ctx,
        ))
    } else {
        with_delegation
    }
}

impl AppState {
    /// Create a new `AppState`, loading configuration from TOML files and
    /// environment variables.
    ///
    /// This is async because the database initialization requires a tokio
    /// runtime. Call via `tauri::async_runtime::block_on` in the Tauri
    /// setup hook.
    ///
    /// Config discovery order:
    /// 1. `~/.athen/config.toml` (user-level)
    /// 2. `./config/config.toml` (project-local)
    /// 3. Built-in defaults
    ///
    /// `DEEPSEEK_API_KEY` env var always takes precedence over config file values.
    pub async fn new() -> Self {
        let mut config = load_config();
        // Snapshot the plain, NON-vault-hydrated config for the dispatch-hot-path
        // cache BEFORE the in-memory `config` is hydrated with vault secrets
        // below. The dispatch loop's per-task resolvers historically read a plain
        // `load_config()`, so the cache must hold that same shape to preserve
        // behavior exactly.
        let config_cache = Arc::new(arc_swap::ArcSwap::from_pointee(config.clone()));

        // Open the credential vault FIRST so secrets stored there can hydrate
        // the in-memory config before any consumer (router, email, web search,
        // telegram, …) reads it. Failure is non-fatal — we log and fall back
        // to the legacy plaintext fields in config.toml.
        let vault: Option<Arc<dyn athen_core::traits::vault::Vault>> = match ensure_data_dir() {
            Some(dir) => match athen_vault::open_vault(&dir, "athen").await {
                Ok(v) => Some(Arc::from(v)),
                Err(e) => {
                    warn!("Vault unavailable: {e} — credential-backed tools will fail until fixed");
                    None
                }
            },
            None => None,
        };
        crate::vault_creds::hydrate_secrets_from_vault(vault.as_ref(), &mut config).await;

        // Git-backed snapshot store for agent action undo. Opens or
        // initializes a bare repo at `<data_dir>/athen-snapshots`.
        // Failure is non-fatal — without it, write/edit just don't get
        // snapshotted (no undo for that action) but the rest of the app
        // continues to work.
        let checkpoint_store: Option<Arc<dyn athen_core::traits::checkpoint::CheckpointStore>> =
            match ensure_data_dir() {
                Some(dir) => match athen_checkpoint::GixCheckpointStore::open(&dir) {
                    Ok(store) => {
                        info!(
                            "Checkpoint store ready at {}/athen-snapshots",
                            dir.display()
                        );
                        Some(Arc::new(store))
                    }
                    Err(e) => {
                        warn!(
                        "Checkpoint store unavailable: {e} — agent actions will not be revertable"
                    );
                        None
                    }
                },
                None => None,
            };

        // Resolver for AgentProfile.github_identity → env-var bundle on
        // `shell_execute`. Per-identity `GH_CONFIG_DIR` lives under
        // `<data_dir>/github/<bot|user>` so the bot's gh state never
        // collides with the user's. Wired only when the vault is up;
        // without it, profiles that opt into a github identity behave
        // as if identity were `None` (no env injection).
        let github_identity_resolver: Option<Arc<dyn athen_agent::tools::GithubIdentityResolver>> =
            vault.as_ref().map(|v| {
                let gh_base = ensure_data_dir().map(|d| d.join("github"));
                let resolver: Arc<dyn athen_agent::tools::GithubIdentityResolver> = Arc::new(
                    crate::github_identity::VaultGithubIdentityResolver::new(v.clone(), gh_base),
                );
                resolver
            });

        // Determine which provider to activate on startup. Prefer the
        // active Bundle when present — it's the post-2026-05-23 source of
        // truth for per-tier `(connection, slug)` picks. Fall back to the
        // legacy single-provider router for pre-Bundles configs and for
        // the degenerate case where the active Bundle id no longer
        // resolves (deleted bundle without a fresh selection).
        let (router, active_id, model_name) = build_startup_router(&config);
        let router = Arc::new(RwLock::new(router));

        let (coordinator, database, contact_store) =
            build_coordinator_with_persistence(&router).await;

        // Build the arc store and run migration from legacy chat tables.
        let arc_store = database.as_ref().map(|db| db.arc_store());
        let calendar_store = database.as_ref().map(|db| db.calendar_store());
        if let Some(ref store) = arc_store {
            match store.migrate_from_chat_tables().await {
                Ok(0) => {}
                Ok(n) => info!("Migrated {n} legacy sessions to arcs"),
                Err(e) => warn!("Arc migration from chat tables failed: {e}"),
            }
        }
        let (active_arc_id, history) = restore_or_create_arc(&arc_store).await;

        // Shared, hot-swappable embedder cell. Built once here and shared by
        // both the memory store (via SwappableEmbedder) and the profile path,
        // so `reload_embedder` / `start_embedder_warmup` updates every consumer
        // with a single swap — no rebuild, no restart.
        let initial_embedder: Arc<dyn athen_core::traits::embedding::EmbeddingProvider> =
            Arc::new(build_embedding_router(&config.embeddings));
        let embedder_cell: EmbedderCell = Arc::new(std::sync::RwLock::new(initial_embedder));

        // Build persistent memory (vector search + knowledge graph).
        let memory = build_memory(&router, embedder_cell.clone()).await;

        // Build the MCP registry and load persisted enabled state.
        // Pass the vault through so BYO `Process` MCPs can resolve
        // `EnvValue::Vault` bindings at spawn time.
        let mcp = Arc::new(match vault.clone() {
            Some(v) => McpRegistry::new_with_vault(v),
            None => McpRegistry::new(),
        });
        let mcp_store = database.as_ref().map(|db| db.mcp_store());
        if let Some(ref store) = mcp_store {
            if let Err(e) = restore_enabled_mcps(&mcp, store).await {
                warn!("Failed to restore enabled MCPs: {e}");
            }
        }

        let tool_doc_dir = ensure_data_dir().map(|d| d.join("tools"));

        let grant_store = database.as_ref().map(|db| Arc::new(db.grant_store()));
        let profile_store = database.as_ref().map(|db| Arc::new(db.profile_store()));
        let identity_store = database.as_ref().map(|db| Arc::new(db.identity_store()));
        let project_store = database.as_ref().map(|db| Arc::new(db.project_store()));
        // Seed the opinionated workspace folder skeleton
        // (UserInfo/Downloads/Projects/...) on boot. Best-effort and purely
        // additive — never fail construction on it. See `docs/PROJECTS.md`.
        if let Err(e) = athen_core::paths::seed_workspace_skeleton() {
            tracing::warn!("failed to seed workspace skeleton: {e}");
        }
        // Hydrate the persisted project-summary compaction mode so a user's
        // choice (e.g. "off" to avoid token spend on local models) survives
        // restarts instead of silently resetting to "auto".
        let project_summary_mode_init = match project_store.as_ref() {
            Some(ps) => ps
                .get_meta("summary_mode")
                .await
                .ok()
                .flatten()
                .unwrap_or_else(|| "auto".to_string()),
            None => "auto".to_string(),
        };
        // Skill store: bodies live under <data_dir>/skills/. Construction is
        // gated on a known data_dir so an in-memory / data_dir-less boot
        // still works (skills just won't be available, same as identity).
        let skill_store = match (database.as_ref(), ensure_data_dir()) {
            (Some(db), Some(data_dir)) => Some(Arc::new(db.skill_store(data_dir.join("skills")))),
            _ => None,
        };
        let wakeup_store = database.as_ref().map(|db| Arc::new(db.wakeup_store()));
        let http_endpoint_store = database
            .as_ref()
            .map(|db| Arc::new(db.http_endpoint_store()));
        let agent_run_store = database.as_ref().map(|db| Arc::new(db.agent_run_store()));
        let telegram_chat_log = database
            .as_ref()
            .map(|db| Arc::new(db.telegram_chat_log_store()));
        let hint_dismissal_store = database.as_ref().map(|db| db.hint_dismissal_store());
        let http_rate_limiter = Arc::new(crate::http_rate_limiter::HttpRateLimiter::new());
        // Single reqwest client reused across every per-arc registry —
        // avoids per-call connection setup cost. Defaults are fine; the
        // workspace already pins gzip/brotli/deflate + rustls.
        let http_client = reqwest::Client::builder()
            .user_agent(concat!("Athen/", env!("CARGO_PKG_VERSION")))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        // Sit alongside `tools/`, NOT inside it — `write_per_group`
        // sweeps stray .md files out of the tools dir on every refresh,
        // which would nuke a sibling endpoint catalogue.
        let cloud_apis_doc_path = ensure_data_dir().map(|d| d.join("cloud_apis.md"));
        // Profile routing shares the same hot-swappable `embedder_cell` built
        // above (so a settings/warmup swap reaches it too). Until a neural
        // provider is wired, the cell holds the keyword fallback, which still
        // produces a usable cosine signal across short strings.
        let profile_embedding_cache = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
        let pending_grants = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let spawned_processes: athen_agent::SpawnedProcessMap =
            Arc::new(Mutex::new(HashMap::new()));

        // Persistent pidfile for `shell_spawn`'d processes. The hook fires
        // on every map mutation so a crash leaves a recoverable record.
        // `reconcile_orphans` is called from `lib.rs` setup() AFTER state
        // is built but BEFORE any monitor can start spawning new shells.
        let pidfile_path: Option<PathBuf> =
            ensure_data_dir().map(|d| crate::spawn_pidfile::pidfile_path(&d));
        let spawn_persistence: Option<Arc<dyn athen_agent::SpawnPersistenceHook>> =
            pidfile_path.clone().map(|p| {
                let hook: Arc<dyn athen_agent::SpawnPersistenceHook> =
                    crate::spawn_pidfile::PidFilePersistence::new(p);
                hook
            });

        // Wire the compactor whenever an arc store exists. The router
        // shares the same RwLock the executor uses so a provider switch
        // is reflected here too.
        let compactor: Option<Arc<dyn athen_core::traits::compaction::ArcCompactor>> =
            arc_store.as_ref().map(|store| {
                let c: Arc<dyn athen_core::traits::compaction::ArcCompactor> = Arc::new(
                    crate::compaction::LlmArcCompactor::new(store.clone(), router.clone()),
                );
                c
            });

        let web_search = build_web_search_provider(&config.web_search);
        let email_sender: Option<Arc<dyn athen_core::traits::email_sender::EmailSender>> =
            build_email_sender(&config.email);
        // Resolve the Telegram owner chat id from the unified contact
        // store first, then fall back to the legacy
        // `TelegramConfig::owner_user_id`. Users who set their owner via
        // the "My Contact Info" panel never write to `owner_user_id`, so
        // without this lookup `send_telegram` would refuse with "no
        // chat_id given and no owner default configured".
        let owner_chat_id_override = resolve_owner_telegram_chat_id(contact_store.as_ref()).await;
        let telegram_sender: Option<Arc<dyn athen_core::traits::telegram_sender::TelegramSender>> =
            build_telegram_sender(&config.telegram, owner_chat_id_override);

        let state = Self {
            coordinator: Arc::new(coordinator),
            security: arc_swap::ArcSwap::from_pointee(config.security.clone()),
            config_cache,
            router,
            active_provider_id: Mutex::new(active_id),
            history: Mutex::new(history),
            pending_message: Mutex::new(None),
            pending_upload_event_id: Mutex::new(None),
            model_name: Mutex::new(model_name),
            active_arc_id: Mutex::new(active_arc_id),
            active_project_id: Mutex::new(None),
            project_summary_mode: Mutex::new(project_summary_mode_init),
            arc_store,
            calendar_store,
            trust_manager: contact_store
                .as_ref()
                .map(|cs| TrustManager::new(Box::new(cs.clone()))),
            contact_store,
            _database: database,
            cancel_flag: Arc::new(AtomicBool::new(false)),
            pending_user_inputs: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            pending_deep_research: std::sync::Mutex::new(HashMap::new()),
            email_shutdown: std::sync::Mutex::new(None),
            telegram_shutdown: std::sync::Mutex::new(None),
            remote_access_shutdown: std::sync::Mutex::new(None),
            tunnel: std::sync::Mutex::new(None),
            remote_access_status: std::sync::Mutex::new(RemoteAccessStatus::default()),
            calendar_shutdown: None,
            calendar_sync_shutdown: None,
            attachment_purger_shutdown: None,
            notifier: arc_swap::ArcSwapOption::empty(),
            approval_router: None,
            inapp_approval_sink: None,
            telegram_approval_sink: None,
            memory,
            mcp,
            mcp_store,
            tool_doc_dir,
            grant_store,
            profile_store,
            identity_store,
            project_store,
            skill_store,
            wakeup_store,
            wakeup_scheduler_shutdown: std::sync::Mutex::new(None),
            profile_embedder: embedder_cell,
            profile_embedding_cache,
            pending_grants,
            spawned_processes,
            spawn_persistence,
            pidfile_path,
            telegram_outbound_hint: std::sync::Arc::new(std::sync::Mutex::new(None)),
            telegram_chat_log,
            inflight_approvals: Arc::new(Mutex::new(HashSet::new())),
            task_arc_map: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            task_wakeup_map: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            pending_email_marks: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            dispatch_signal: Arc::new(tokio::sync::Notify::new()),
            dispatch_loop_shutdown: None,
            compactor,
            web_search: std::sync::RwLock::new(web_search),
            email_sender: std::sync::RwLock::new(email_sender),
            telegram_sender: std::sync::RwLock::new(telegram_sender),
            vault,
            github_identity_resolver,
            http_endpoint_store,
            http_rate_limiter,
            http_client,
            cloud_apis_doc_path,
            agent_run_store,
            agent_registry: None,
            checkpoint_store,
            hint_dismissal_store,
        };

        if let Err(e) = state.refresh_tools_doc().await {
            warn!("Failed to write initial per-group tool docs: {e}");
        }
        if let Err(e) = state.refresh_cloud_apis_doc().await {
            warn!("Failed to write initial cloud_apis catalogue: {e}");
        }
        // Reconcile the skills index against the filesystem so hand-edited
        // or freshly git-cloned skill folders show up without restart. The
        // sync is idempotent and cheap when nothing changed.
        if let Some(skills) = state.skill_store.as_ref() {
            use athen_core::traits::skill::SkillStore;

            // Seed the builtin "athen-docs" skill that teaches the agent
            // about the `athen_docs` tool. Written once; survives sync
            // because it has a real folder+file on disk.
            let docs_slug = "athen-docs";
            let docs_body = include_str!("../../../skills/system/athen-docs/SKILL.md");
            match skills.get(docs_slug).await {
                Ok(None) => match athen_core::skill::parse_skill_md(docs_body) {
                    Ok((front, body)) => {
                        if let Err(e) = skills.upsert(docs_slug, &front, &body).await {
                            warn!("Failed to seed athen-docs skill: {e}");
                        } else {
                            info!("Seeded builtin skill: athen-docs");
                        }
                    }
                    Err(e) => warn!("Failed to parse athen-docs SKILL.md: {e}"),
                },
                Ok(Some(_)) => {} // already exists, don't overwrite
                Err(e) => warn!("Failed to check athen-docs skill: {e}"),
            }

            match skills.sync().await {
                Ok(report) => {
                    if report.inserted + report.updated + report.deleted > 0 {
                        info!(
                            inserted = report.inserted,
                            updated = report.updated,
                            deleted = report.deleted,
                            "Skills index reconciled with filesystem"
                        );
                    }
                }
                Err(e) => warn!("Skills sync failed at boot: {e}"),
            }
        }

        state
    }

    /// Graceful shutdown coordinator. Called from the tray-menu Quit
    /// path, the auto-updater restart path, and on SIGTERM. Each step
    /// is bounded by a short timeout so a wedged subsystem can't trap
    /// the user — total budget is roughly 5 seconds.
    ///
    /// Order matters:
    /// 1. Signal every monitor loop to exit (broadcast, fire-and-forget).
    /// 2. Mark every live agent in the registry as `Cancelled` so the
    ///    SQLite history doesn't lie about runs that never completed.
    /// 3. SIGKILL / taskkill every `shell_spawn`'d process so wake-ups
    ///    can't keep watchers alive past the app's lifetime.
    /// 4. Force-write the pidfile to `[]` — defensive, also fires the
    ///    persistence hook once during kill_all_spawned but the explicit
    ///    write here is idempotent and covers the no-hook case.
    /// 5. `PRAGMA wal_checkpoint(TRUNCATE)` so a power loss right after
    ///    exit doesn't lose committed-but-WAL'd writes.
    ///
    /// Individual step failures are logged but never propagate — the
    /// goal is to make exit as clean as possible, not to guarantee it.
    pub async fn shutdown_all(&self) {
        use std::time::Duration;
        use tokio::time::timeout;

        tracing::info!("graceful shutdown: starting");

        // 1. Fire-and-forget the broadcast / oneshot signals. The
        // receivers may already be gone (loop exited early on its own,
        // CLI build that never started them) — that's fine, we just
        // care that any live receiver flips out of its sleep / poll.
        macro_rules! pulse {
            ($sender:expr, $label:literal) => {
                if let Some(tx) = $sender {
                    if tx.send(()).is_err() {
                        tracing::debug!(concat!($label, " shutdown signal had no listeners"));
                    }
                }
            };
        }
        // We take() conceptually but only have &self here, so just clone
        // the inner Sender — broadcast senders are cheap to clone and
        // can be dropped freely after the send.
        pulse!(self.email_shutdown.lock_recover().clone(), "email monitor");
        pulse!(
            self.telegram_shutdown.lock_recover().clone(),
            "telegram monitor"
        );
        pulse!(self.calendar_shutdown.as_ref().cloned(), "calendar monitor");
        pulse!(
            self.calendar_sync_shutdown.as_ref().cloned(),
            "calendar sync"
        );
        pulse!(
            self.attachment_purger_shutdown.as_ref().cloned(),
            "attachment purger"
        );
        pulse!(
            self.dispatch_loop_shutdown.as_ref().cloned(),
            "dispatch loop"
        );
        // Wake-up scheduler uses a oneshot — take() the inner via the
        // wrapping `std::sync::Mutex` so we can move-send it. Dropping
        // the sender on a take() also fires the rx side, so both paths
        // (explicit send + Drop) wake the scheduler.
        {
            let mut guard = self.wakeup_scheduler_shutdown.lock_recover();
            if let Some(tx) = guard.take() {
                if tx.send(()).is_err() {
                    tracing::debug!("wakeup scheduler shutdown signal had no listener");
                }
            }
        }
        // Remote Access listener (oneshot) + cloudflared tunnel (Drop kills the
        // child). Best-effort; ignore absence.
        if let Some(tx) = self.remote_access_shutdown.lock_recover().take() {
            let _ = tx.send(());
        }
        drop(self.tunnel.lock_recover().take());

        // 2. Mark live agent runs as Cancelled with reason "app_shutdown".
        if let Some(reg) = self.agent_registry.as_ref() {
            let reg = Arc::clone(reg);
            let res = timeout(Duration::from_millis(1000), async move {
                let n = reg.finalize_all_as_cancelled("app_shutdown").await;
                if n > 0 {
                    tracing::info!(count = n, "finalized live agents as Cancelled");
                }
            })
            .await;
            if res.is_err() {
                tracing::warn!("graceful shutdown: agent finalize step timed out");
            }
        }

        // 3. Kill every `shell_spawn`'d process. Passing the persistence
        // hook means kill_all_spawned itself fires a final empty
        // snapshot — step 4 below is defense in depth.
        let map = self.spawned_processes.clone();
        let hook = self.spawn_persistence.clone();
        let res = timeout(Duration::from_millis(2000), async move {
            athen_agent::kill_all_spawned(&map, hook.as_ref()).await
        })
        .await;
        match res {
            Ok(n) if n > 0 => tracing::info!(count = n, "killed spawned processes on shutdown"),
            Ok(_) => {}
            Err(_) => tracing::warn!("graceful shutdown: kill_all_spawned timed out"),
        }

        // 4. Explicitly truncate the pidfile. Idempotent — same target
        // state as the hook-fired write above.
        if let Some(path) = self.pidfile_path.as_ref() {
            let path = path.clone();
            let res = timeout(Duration::from_millis(500), async move {
                crate::spawn_pidfile::write_pidfile(&path, &[]).await
            })
            .await;
            if res.is_err() {
                tracing::warn!("graceful shutdown: pidfile truncate timed out");
            }
        }

        // 5. WAL checkpoint so a power-loss right after exit doesn't
        // strand committed writes in the WAL.
        if let Some(db) = self._database.as_ref() {
            let res = timeout(Duration::from_millis(2000), db.checkpoint_wal()).await;
            if res.is_err() {
                tracing::warn!("graceful shutdown: WAL checkpoint timed out");
            }
        }

        tracing::info!("graceful shutdown: done");
    }

    /// Borrow the attachment-ref store backed by the same SQLite
    /// connection as `arc_store`. Returns `None` when no database is
    /// wired (CLI/test builds). The store is cloneable (it's an
    /// `Arc<Mutex<Connection>>` wrapper), so callers can move the value
    /// freely into background tasks.
    pub fn attachment_store(&self) -> Option<athen_persistence::attachments::AttachmentStore> {
        self._database.as_ref().map(|db| db.attachment_store())
    }

    /// SQLite-backed store for calendar source configurations. Returns
    /// `None` in CLI/test builds without a database.
    pub fn calendar_source_store(
        &self,
    ) -> Option<athen_persistence::calendar_sources::SqliteCalendarSourceStore> {
        self._database.as_ref().map(|db| db.calendar_source_store())
    }

    /// Build an `OwnerLookup` from the shared contact store. Returns
    /// `None` when no store is wired (CLI / test builds without a DB).
    /// Cheap to call repeatedly — the underlying store is cloneable and
    /// the lookup wraps it in an `Arc<dyn ContactStore>`.
    pub fn owner_lookup(&self) -> Option<Arc<athen_contacts::OwnerLookup>> {
        let store = self.contact_store.as_ref()?;
        let arc: Arc<dyn athen_contacts::ContactStore> = Arc::new(store.clone());
        Some(Arc::new(athen_contacts::OwnerLookup::new(arc)))
    }

    /// Build an `OwnerDestinationCheck` adapter around `owner_lookup()`,
    /// ready to feed into `ShellToolRegistry::with_owner_check_opt`. Same
    /// `None` semantics — when the lookup is unavailable, the agent-side
    /// owner self-send bypass simply doesn't fire and `email_send`
    /// preserves today's gate-every-send behaviour.
    pub fn owner_destination_check(&self) -> Option<Arc<dyn athen_agent::OwnerDestinationCheck>> {
        let lookup = self.owner_lookup()?;
        Some(Arc::new(crate::email_gate::OwnerLookupAdapter::new(lookup)))
    }

    /// Load `config.toml` and overlay any vault-stored secrets on top.
    /// Use this anywhere that currently calls `load_config()` and then
    /// reads a credential field — IMAP password, SMTP password,
    /// Telegram bot token, web-search keys, provider api_keys. Pure
    /// non-credential reads can keep using bare `load_config()`.
    ///
    /// `load_hydrated_config_sync` is the sync-context companion for
    /// the Tauri startup hooks that aren't async; prefer this async
    /// one wherever possible.
    pub fn load_hydrated_config_sync(&self) -> AthenConfig {
        let mut config = load_config();
        if let Some(vault) = self.vault.as_ref() {
            let vault = vault.clone();
            let cfg = std::mem::take(&mut config);
            let hydrate = async move {
                let mut c = cfg;
                crate::vault_creds::hydrate_secrets_from_vault(Some(&vault), &mut c).await;
                c
            };
            // This fn is sync but is reached from two kinds of caller:
            //   - boot hooks (no runtime is driving this thread), and
            //   - restart paths invoked *from inside* an async Tauri command
            //     (e.g. `save_smtp_settings` → `restart_email_monitor` →
            //     `start_email_monitor`).
            // A bare `block_on` panics with "Cannot start a runtime from
            // within a runtime" in the latter case. So if a runtime handle is
            // already current, run the (vault-I/O) future on it via
            // `block_in_place` (legal on Tauri's multi-thread runtime);
            // otherwise fall back to the standalone `block_on`.
            config = match tokio::runtime::Handle::try_current() {
                Ok(handle) => tokio::task::block_in_place(move || handle.block_on(hydrate)),
                Err(_) => tauri::async_runtime::block_on(hydrate),
            };
        }
        config
    }

    /// Re-read the plain config from disk and swap it into [`Self::config_cache`].
    /// Call this at the END of any runtime path that persists config to disk
    /// (Settings save handlers) so the autonomous dispatch loop observes the
    /// change without a restart, while still avoiding a per-task disk read +
    /// TOML parse + legacy-id migration. Cheap and lock-free for readers.
    pub fn reload_config_cache(&self) {
        self.config_cache.store(Arc::new(load_config()));
    }

    /// Generate per-group markdown schema files into `tool_doc_dir`. Called
    /// at startup and whenever the available tool set changes (i.e. after a
    /// user enables or disables an MCP). Silently no-ops when no directory
    /// is configured (no data dir).
    pub async fn refresh_tools_doc(&self) -> athen_core::error::Result<()> {
        let Some(dir) = self.tool_doc_dir.clone() else {
            return Ok(());
        };
        // Build the same registry the executor sees so the docs reflect
        // exactly what the agent has access to. The file gate is not
        // attached here — listing tools never invokes them.
        let mut shell_registry = athen_agent::ShellToolRegistry::new()
            .await
            .with_spawned_processes(self.spawned_processes.clone())
            .with_spawn_persistence_hook_opt(self.spawn_persistence.clone())
            .with_web_search(self.web_search.read_recover().clone())
            .with_email_sender_opt(self.email_sender.read_recover().clone())
            .with_telegram_sender_opt(self.telegram_sender.read_recover().clone())
            .with_owner_check_opt(self.owner_destination_check());
        if let Some(router) = self.approval_router.clone() {
            shell_registry = shell_registry.with_toolbox_approval(Arc::new(
                crate::file_gate::RouterToolboxApprovalGate::new(router.clone(), None),
            ));
            let gate: Arc<dyn athen_agent::EmailSendApprovalGate> = Arc::new(
                crate::email_gate::RouterEmailApprovalGate::new(router.clone(), None),
            );
            shell_registry = shell_registry.with_email_approval(gate);
            let tg_gate: Arc<dyn athen_agent::tools::TelegramSendApprovalGate> = Arc::new(
                crate::email_gate::RouterTelegramApprovalGate::new(router, None),
            );
            shell_registry = shell_registry.with_telegram_approval(tg_gate);
        }
        // No arc context here (`refresh_tools_doc` is a global tool
        // listing), so the recorder is a no-op stamp. Wired for parity.
        let recorder: Arc<dyn athen_agent::tools::TelegramOutboundRecorder> =
            Arc::new(crate::email_gate::ArcAwareTelegramOutboundRecorder::new(
                self.telegram_outbound_hint.clone(),
                None,
                self.telegram_chat_log.clone(),
            ));
        shell_registry = shell_registry.with_telegram_outbound_recorder(recorder);
        let mut registry = crate::app_tools::AppToolRegistry::new(
            shell_registry,
            self.calendar_store.clone(),
            self.contact_store.clone(),
            self.memory.clone(),
        )
        .with_mcp(self.mcp.clone() as Arc<dyn athen_core::traits::mcp::McpClient>);
        if let Some(astore) = self.attachment_store() {
            registry = registry.with_attachments(astore);
        }
        if let Some(istore) = self.identity_store.clone() {
            registry = registry.with_identity(istore);
        }
        if let Some(sstore) = self.skill_store.clone() {
            registry = registry.with_skills(sstore);
        }
        if let (Some(estore), Some(vault)) = (self.http_endpoint_store.clone(), self.vault.clone())
        {
            registry = registry.with_http_endpoints(
                estore,
                vault,
                self.http_rate_limiter.clone(),
                self.http_client.clone(),
                self.cloud_apis_doc_path.clone(),
            );
        }
        if let Some(cstore) = self.calendar_source_store() {
            let cstore: Arc<
                dyn athen_core::traits::calendar_source_config::CalendarSourceConfigStore,
            > = Arc::new(cstore);
            registry = registry.with_calendar_remote(cstore);
        }
        let tools = athen_core::traits::tool::ToolRegistry::list_tools(&registry).await?;
        let written = athen_agent::tools_doc::write_per_group(&dir, &tools).map_err(|e| {
            athen_core::error::AthenError::Other(format!(
                "write tool docs into {}: {e}",
                dir.display()
            ))
        })?;
        info!(
            "Wrote {} group(s) of tool schemas under {}",
            written.len(),
            dir.display()
        );
        Ok(())
    }

    /// Refresh the registered-endpoint reference docs.
    ///
    /// Two-tier shape — same idea as `tools/<group>.md`:
    /// - `<data_dir>/cloud_apis.md` is a small INDEX. One line per
    ///   endpoint with name + one-liner + path to its detail file.
    /// - `<data_dir>/cloud_apis/<slug>.md` is the per-endpoint DETAIL
    ///   (auth shape, sample paths, free-tier blurb, notes). The agent
    ///   reads only the file it needs.
    ///
    /// Stale detail files (whose endpoint was deleted) are swept on
    /// every call so renames don't leave orphans.
    pub async fn refresh_cloud_apis_doc(&self) -> athen_core::error::Result<()> {
        use athen_core::traits::http_endpoint::HttpEndpointStore;
        let Some(index_path) = self.cloud_apis_doc_path.clone() else {
            return Ok(());
        };
        let Some(store) = self.http_endpoint_store.clone() else {
            return Ok(());
        };
        let endpoints = store.list().await?;
        let presets = crate::http_presets::presets();
        let detail_dir = match index_path.parent() {
            Some(p) => p.join("cloud_apis"),
            None => return Ok(()),
        };
        if let Err(e) = std::fs::create_dir_all(&detail_dir) {
            warn!(
                "Failed to create cloud_apis detail dir {}: {e}",
                detail_dir.display()
            );
            return Ok(());
        }

        // Detail files keyed by sanitized endpoint name. Track which we
        // wrote this run so stale ones can be removed.
        let mut current_files: std::collections::HashSet<std::path::PathBuf> =
            std::collections::HashSet::new();
        let mut index_rows: Vec<(String, String, std::path::PathBuf)> =
            Vec::with_capacity(endpoints.len());
        for ep in &endpoints {
            let slug = sanitize_endpoint_filename(&ep.name);
            let detail_path = detail_dir.join(format!("{slug}.md"));
            let preset = presets
                .iter()
                .find(|p| p.base_url == ep.base_url || p.label == ep.name);
            let detail = render_endpoint_detail(ep, preset);
            if let Err(e) = std::fs::write(&detail_path, detail) {
                warn!(
                    "Failed to write {} detail to {}: {e}",
                    ep.name,
                    detail_path.display()
                );
                continue;
            }
            current_files.insert(detail_path.clone());
            let one_liner = endpoint_one_liner(ep, preset);
            index_rows.push((ep.name.clone(), one_liner, detail_path));
        }

        // Sweep stray detail files (renamed / deleted endpoints).
        if let Ok(entries) = std::fs::read_dir(&detail_dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.extension().and_then(|s| s.to_str()) == Some("md")
                    && !current_files.contains(&p)
                {
                    let _ = std::fs::remove_file(p);
                }
            }
        }

        let index = render_cloud_apis_index(&index_rows);
        if let Err(e) = std::fs::write(&index_path, index) {
            warn!(
                "Failed to write cloud_apis index to {}: {e}",
                index_path.display()
            );
        }
        Ok(())
    }

    /// Snapshot the AppState fields the per-arc registry needs.
    /// Cheap — every field is `Arc`-backed or a clone-of-Arc'd store.
    pub(crate) fn tool_registry_deps(&self) -> ToolRegistryDeps {
        ToolRegistryDeps {
            spawned_processes: self.spawned_processes.clone(),
            spawn_persistence: self.spawn_persistence.clone(),
            web_search: self.web_search.read_recover().clone(),
            email_sender: self.email_sender.read_recover().clone(),
            telegram_sender: self.telegram_sender.read_recover().clone(),
            owner_check: self.owner_destination_check(),
            github_identity_resolver: self.github_identity_resolver.clone(),
            checkpoint_store: self.checkpoint_store.clone(),
            grant_store: self.grant_store.clone(),
            approval_router: self.approval_router.clone(),
            telegram_approval_sink: self.telegram_approval_sink.clone(),
            telegram_outbound_hint: self.telegram_outbound_hint.clone(),
            telegram_chat_log: self.telegram_chat_log.clone(),
            pending_grants: self.pending_grants.clone(),
            calendar_store: self.calendar_store.clone(),
            contact_store: self.contact_store.clone(),
            memory: self.memory.clone(),
            mcp: self.mcp.clone(),
            attachment_store: self.attachment_store(),
            identity_store: self.identity_store.clone(),
            project_store: self.project_store.clone(),
            skill_store: self.skill_store.clone(),
            http_endpoint_store: self.http_endpoint_store.clone(),
            vault: self.vault.clone(),
            http_rate_limiter: self.http_rate_limiter.clone(),
            http_client: self.http_client.clone(),
            cloud_apis_doc_path: self.cloud_apis_doc_path.clone(),
            calendar_source_store: self.calendar_source_store().map(|s| {
                Arc::new(s)
                    as Arc<
                        dyn athen_core::traits::calendar_source_config::CalendarSourceConfigStore,
                    >
            }),
            profile_store: self.profile_store.clone(),
            arc_store: self._database.as_ref().map(|db| db.arc_store()),
            tool_doc_dir: self.tool_doc_dir.clone(),
            router: Arc::clone(&self.router),
            wakeup_store: self
                .wakeup_store
                .clone()
                .map(|s| s as Arc<dyn athen_core::traits::wakeup::WakeupStore>),
            notifier: self.notifier.load_full(),
            active_provider_id: self
                .active_provider_id
                .try_lock()
                .map(|g| g.clone())
                .unwrap_or_default(),
            global_security_mode: self.security.load().mode,
        }
    }

    /// Build a per-arc tool registry wired with the file-permission gate
    /// and the shell sandbox grant provider. Thin wrapper around
    /// [`assemble_app_tool_registry`] — see there for the actual
    /// assembly. Same helper is used by the owner-Telegram dispatch
    /// path so the tool surface stays consistent.
    pub async fn build_tool_registry(
        &self,
        arc_id: &str,
        ui: Option<crate::ui_bridge::UiBridge>,
    ) -> Box<dyn athen_core::traits::tool::ToolRegistry> {
        assemble_app_tool_registry(self.tool_registry_deps(), arc_id, ui).await
    }

    /// Park a Telegram-triggered Deep Research request until the owner taps a
    /// depth button, returning a short token to embed in the button
    /// `callback_data`. Opportunistically prunes entries older than an hour so
    /// an abandoned request never lingers.
    pub(crate) fn stash_pending_deep_research(&self, pending: PendingDeepResearch) -> String {
        let full = uuid::Uuid::new_v4().simple().to_string();
        let token = full[..10].to_string();
        let mut map = self.pending_deep_research.lock_recover();
        let now = chrono::Utc::now();
        map.retain(|_, p| now.signed_duration_since(p.created_at).num_seconds() < 3600);
        map.insert(token.clone(), pending);
        token
    }

    /// Consume the parked request for `token` (the depth button was tapped).
    pub(crate) fn take_pending_deep_research(&self, token: &str) -> Option<PendingDeepResearch> {
        self.pending_deep_research.lock_recover().remove(token)
    }

    /// Drive a full Deep Research run for `arc_id` and return the synthesized
    /// paper (see `docs/DEEP_RESEARCH.md`). Pure orchestration — this does NOT
    /// persist the paper or stamp arc metadata; the tool/command layer (a
    /// later wave) owns that.
    ///
    /// Workers are spawned through the existing delegation seam
    /// (`run_delegation`) under the `deep_research_worker` profile, each in
    /// its own parent-linked sub-arc, so they surface in the arc tree exactly
    /// like any other delegated specialist. Fan-out concurrency + partial-
    /// result tolerance live in [`crate::deep_research::run_deep_research`].
    ///
    /// `ui` is required (not read from `self`) because `AppState` never holds
    /// a `UiBridge` — it is threaded in by every caller. It is used both to
    /// emit `"deep-research-progress"` events and to wire the workers' sub-arc
    /// auditor so their tool calls persist under their own sub-arc.
    pub(crate) async fn run_deep_research_for_arc(
        &self,
        arc_id: &str,
        question: &str,
        depth: Option<&str>,
        prior_paper: Option<String>,
        ui: crate::ui_bridge::UiBridge,
    ) -> Result<crate::deep_research::ResearchOutcome> {
        use athen_core::error::AthenError;

        // Required stores. Mirror how other AppState methods guard `Option`
        // stores — return a clear error rather than panicking.
        let profile_store = self.profile_store.clone().ok_or_else(|| {
            AthenError::Other("Deep research unavailable: profile store not configured".to_string())
        })?;
        let arc_store = self
            ._database
            .as_ref()
            .map(|db| db.arc_store())
            .ok_or_else(|| {
                AthenError::Other("Deep research unavailable: arc store not configured".to_string())
            })?;

        // Build the BARE base tool registry for this arc — the same bare base
        // a sub-agent receives (no delegation/wake-up wrappers ⇒ depth=1).
        // `run_delegation` re-scopes it to each worker's own sub-arc via the
        // context's `sub_registry_factory`.
        let base: Arc<dyn athen_core::traits::tool::ToolRegistry> =
            assemble_base_app_tool_registry(
                self.tool_registry_deps(),
                arc_id,
                Some(ui.clone()),
                // No deep_research tool for the worker base — prevents a
                // deep-research worker from recursively triggering another run.
                None,
            )
            .await;

        // Build the delegation context once (cloned per worker). Parent arc =
        // this research arc, so worker sub-arcs hang off it. No wake-up
        // restrictions — this is a user/agent-triggered run.
        let ctx = build_delegation_context(
            profile_store,
            arc_store,
            self.identity_store.clone(),
            self.skill_store.clone(),
            self.http_endpoint_store.clone(),
            self.tool_doc_dir.clone(),
            Arc::clone(&self.router),
            arc_id.to_string(),
            Some(ui.clone()),
            None,
        );

        // Per-worker spawn closure. `Fn` (called N times), so it captures
        // clonable `Arc`s / clonable context — never moved-once values.
        let spawn_worker = move |brief: String| {
            let base = base.clone();
            let ctx = ctx.clone();
            async move {
                let (_sub_arc, content, success, _verified, _note) =
                    crate::delegation::run_delegation(
                        base,
                        ctx,
                        crate::delegation::DelegateArgs {
                            target_profile_id: "deep_research_worker".to_string(),
                            brief,
                            reasoning_effort: None,
                        },
                    )
                    .await?;
                Ok::<String, AthenError>(if success { content } else { String::new() })
            }
        };

        // Progress closure: cheap, non-blocking emit of a UI event.
        let arc_id_owned = arc_id.to_string();
        let ui_for_progress = ui.clone();
        let progress = move |p: crate::deep_research::Progress| {
            ui_for_progress.emit(
                "deep-research-progress",
                serde_json::json!({
                    "arc_id": arc_id_owned,
                    "phase": p.phase,
                    "detail": p.detail,
                    "workers_total": p.workers_total,
                    "workers_done": p.workers_done,
                    "workers_ok": p.workers_ok,
                }),
            );
        };

        crate::deep_research::run_deep_research(
            Arc::clone(&self.router),
            question,
            crate::deep_research::Depth::parse(depth),
            prior_paper.as_deref(),
            spawn_worker,
            progress,
        )
        .await
    }

    /// Initialize the notification orchestrator.
    ///
    /// Must be called after `AppState::new()` but before `app.manage()`.
    /// Channels are built from the current config: InApp is added when a
    /// Tauri handle exists (desktop mode), Telegram is added only if the
    /// bot is configured with an owner. Headless mode therefore gets a
    /// Telegram-only (or empty) channel set.
    pub fn init_notifier(&mut self, ui: crate::ui_bridge::UiBridge) {
        let config = self.load_hydrated_config_sync();
        let notifier = Arc::new(self.build_notifier(&config, &ui));
        // Load persisted notifications from a previous session.
        tauri::async_runtime::block_on(notifier.load_persisted());
        self.notifier.store(Some(notifier));
    }

    /// Build (but do not persist-load or store) a `NotificationOrchestrator`
    /// from `config`. Shared by `init_notifier` (boot) and `reload_notifier`
    /// (live Settings save) so the channel set, preferred order, quiet hours,
    /// and escalation timeout are assembled identically in both paths.
    fn build_notifier(
        &self,
        config: &athen_core::config::AthenConfig,
        ui: &crate::ui_bridge::UiBridge,
    ) -> NotificationOrchestrator {
        let mut channels: Vec<Box<dyn NotificationChannel>> = Vec::new();

        // InApp needs a WebView to render into — desktop mode only.
        if let Some(handle) = ui.tauri_handle() {
            channels.push(Box::new(InAppChannel::new(handle.clone())));
        }

        // Add Telegram channel if the bot is configured and has an owner.
        if config.telegram.enabled {
            let token = &config.telegram.bot_token;
            if let Some(owner_id) = config.telegram.owner_user_id {
                if !token.is_empty() {
                    channels.push(Box::new(
                        TelegramChannel::new(token.clone(), owner_id)
                            .with_outbound_hint(self.telegram_outbound_hint.clone()),
                    ));
                }
            }
        }

        // Re-order channels to match preferred order from config.
        let preferred = &config.notifications.preferred_channels;
        if !preferred.is_empty() {
            channels.sort_by_key(|ch| {
                preferred
                    .iter()
                    .position(|k| *k == ch.channel_kind())
                    .unwrap_or(usize::MAX)
            });
        }

        let llm_router: Box<dyn athen_core::traits::llm::LlmRouter> =
            Box::new(SharedRouter(Arc::clone(&self.router)));

        let mut orchestrator =
            NotificationOrchestrator::new(config.notifications.clone(), channels)
                .with_llm_router(llm_router);

        // Attach the notification store for persistence.
        if let Some(ref db) = self._database {
            orchestrator = orchestrator.with_store(db.notification_store());
        }

        orchestrator
    }

    /// Rebuild the notification orchestrator from current (vault-hydrated)
    /// config and hot-swap it in. Called by `save_notification_settings` so
    /// preferred-channel order, quiet hours, and escalation timeout apply
    /// without a restart. Re-loads persisted history from the store so the
    /// swap doesn't drop the notification list.
    pub(crate) async fn reload_notifier(&self, ui: crate::ui_bridge::UiBridge) {
        let mut config = crate::settings::load_main_config_public();
        crate::vault_creds::hydrate_secrets_from_vault(self.vault.as_ref(), &mut config).await;
        let notifier = Arc::new(self.build_notifier(&config, &ui));
        notifier.load_persisted().await;
        self.notifier.store(Some(notifier));
    }

    /// Initialize the approval router and its sinks.
    ///
    /// Must be called after `AppState::new()` but before `app.manage()`.
    /// The InApp sink is created only in desktop mode (it parks a oneshot
    /// that only the WebView can resolve — wiring it headless would hang
    /// every approval). The router is wired against the existing arc store
    /// so it can pick the right channel based on each arc's
    /// `primary_reply_channel` (or its source as a fallback).
    pub fn init_approval_router(&mut self, ui: crate::ui_bridge::UiBridge) {
        use crate::approval::{ApprovalRouter, InAppApprovalSink, TelegramApprovalSink};
        use athen_core::traits::approval::ApprovalSink;

        let config = self.load_hydrated_config_sync();

        // The "in-app" surface is the WebView in desktop mode, or a
        // remote HTTP client consuming the SSE event stream in headless
        // mode (event bus active). With neither, skip the sink — an
        // unanswerable question parked here would just burn the
        // escalation timeout before Telegram gets a shot.
        let inapp = (ui.tauri_handle().is_some() || crate::ui_bridge::UiBridge::event_bus_active())
            .then(|| Arc::new(InAppApprovalSink::new(ui.clone())));
        let mut sinks: Vec<Arc<dyn ApprovalSink>> = Vec::new();
        if let Some(ref s) = inapp {
            sinks.push(s.clone() as Arc<dyn ApprovalSink>);
        }

        let mut telegram_sink: Option<Arc<TelegramApprovalSink>> = None;
        if config.telegram.enabled {
            let token = &config.telegram.bot_token;
            // Use the configured owner_user_id as the approval chat.
            // For a private chat with the bot, chat_id == user_id.
            if let Some(owner_id) = config.telegram.owner_user_id {
                if !token.is_empty() {
                    let s = Arc::new(TelegramApprovalSink::new(token.clone(), owner_id));
                    telegram_sink = Some(s.clone());
                    sinks.push(s as Arc<dyn ApprovalSink>);
                }
            }
        }

        let mut router = ApprovalRouter::new(sinks);
        if let Some(store) = self._database.as_ref().map(|db| db.arc_store()) {
            router = router.with_arc_store(store);
        }
        // Use the configured notification escalation as a starting point
        // for approval escalation too — it's the same "user not present"
        // heuristic.
        let escalation_secs = config.notifications.escalation_timeout_secs.max(15);
        router = router.with_escalation_after(std::time::Duration::from_secs(escalation_secs));

        // Mirror the user's notifier preference into the approval router
        // so approval prompts go to the same channel as completion pings
        // (Telegram-first when configured). Without this, the user
        // would see the completion message land on Telegram but the
        // approval-needed prompt arrive only in-app.
        let preferred: Vec<athen_core::approval::ReplyChannelKind> = config
            .notifications
            .preferred_channels
            .iter()
            .map(|k| match k {
                athen_core::config::NotificationChannelKind::InApp => {
                    athen_core::approval::ReplyChannelKind::InApp
                }
                athen_core::config::NotificationChannelKind::Telegram => {
                    athen_core::approval::ReplyChannelKind::Telegram
                }
            })
            .collect();
        if !preferred.is_empty() {
            router = router.with_preferred_channels(preferred);
        }

        self.approval_router = Some(Arc::new(router));
        self.inapp_approval_sink = inapp;
        self.telegram_approval_sink = telegram_sink;

        info!(
            "Approval router initialized (escalation after {}s)",
            escalation_secs
        );
    }

    /// Initialize the live agent registry. Must run after `AppState::new()`
    /// but before `app.manage()` because it needs an `AppHandle` to emit
    /// `agents-changed` events. Idempotent — re-init replaces the previous
    /// registry, which is fine since live state is also empty at startup.
    pub fn init_agent_registry(&mut self, ui: crate::ui_bridge::UiBridge) {
        let registry = crate::agent_registry::AgentRegistry::new(ui, self.agent_run_store.clone());
        self.agent_registry = Some(registry);
    }

    /// Spawn a background loop that prunes finalized agent_runs rows
    /// older than 30 days. Runs once at startup, then every 6 hours.
    /// No-op when `agent_run_store` is unwired (CLI / test builds).
    pub fn start_agent_run_pruner(&self) {
        let Some(store) = self.agent_run_store.clone() else {
            tracing::debug!("No agent_run_store wired; skipping pruner");
            return;
        };
        tauri::async_runtime::spawn(async move {
            let interval = std::time::Duration::from_secs(6 * 60 * 60);
            let mut ticker = tokio::time::interval(interval);
            // First tick fires immediately, kicking off the startup sweep.
            loop {
                ticker.tick().await;
                let cutoff = chrono::Utc::now() - chrono::Duration::days(30);
                match store.prune_older_than(cutoff).await {
                    Ok(0) => {
                        tracing::debug!("agent_runs pruner: nothing to prune");
                    }
                    Ok(n) => {
                        tracing::info!("agent_runs pruner: removed {n} stale row(s)");
                    }
                    Err(e) => {
                        tracing::warn!("agent_runs pruner failed: {e}");
                    }
                }
            }
        });
    }

    /// Start the proactive help hint checker.
    ///
    /// Runs 60 seconds after startup (to let monitors settle), then every
    /// 15 minutes. Rate-limited internally to at most 1 hint per hour.
    /// Emits a `proactive-hint` Tauri event for the frontend to render.
    pub fn start_proactive_hint_checker(&self, ui: crate::ui_bridge::UiBridge) {
        let Some(store) = self.hint_dismissal_store.clone() else {
            tracing::debug!("No hint_dismissal_store wired; skipping hint checker");
            return;
        };
        let notifier = self.notifier.load_full();
        let config_snapshot = self.load_hydrated_config_sync();
        let active_id = self.active_provider_id.blocking_lock().clone();
        let cal_source_store = self.calendar_source_store();

        let checker = std::sync::Arc::new(crate::proactive_hints::ProactiveHintChecker::new(store));

        tauri::async_runtime::spawn(async move {
            // Initial delay so the app has time to finish loading.
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;

            let interval = std::time::Duration::from_secs(15 * 60);
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

            loop {
                ticker.tick().await;

                let cal_count = if let Some(ref cs) = cal_source_store {
                    use athen_core::traits::calendar_source_config::CalendarSourceConfigStore;
                    cs.list().await.map(|v| v.len()).unwrap_or(0)
                } else {
                    0
                };

                let is_local = matches!(active_id.as_str(), "ollama" | "llamacpp");

                let ctx = crate::proactive_hints::HintContext {
                    config: config_snapshot.clone(),
                    calendar_source_count: cal_count,
                    active_provider_id: active_id.clone(),
                    is_local_provider: is_local,
                };

                checker.check_and_emit(ctx, &ui, notifier.as_ref()).await;
            }
        });
    }

    /// Start the email monitor background polling task.
    ///
    /// This must be called after the `AppState` is constructed but before it
    /// is handed to `app.manage()`, because we need the `AppHandle` to emit
    /// Tauri events to the frontend.
    ///
    /// The monitor polls IMAP for new emails, then sends each email to the
    /// LLM for relevance triage.  Only emails classified as `medium` or
    /// `high` relevance are forwarded to the frontend as actionable cards.
    /// Spam and irrelevant messages are silently logged and discarded.
    pub fn start_email_monitor(&self, ui: crate::ui_bridge::UiBridge) {
        use athen_core::traits::sense::SenseMonitor;
        use athen_sentidos::email::EmailMonitor;

        let config = self.load_hydrated_config_sync();
        if !config.email.enabled {
            info!("Email monitor disabled in config, skipping startup");
            return;
        }

        if config.email.imap_server.is_empty() {
            warn!("Email monitor enabled but no IMAP server configured");
            return;
        }

        let (shutdown_tx, _) = tokio::sync::broadcast::channel::<()>(1);
        *self.email_shutdown.lock_recover() = Some(shutdown_tx.clone());

        // Deps captured by the factory closure. They are all `Arc`/`Clone`, so
        // each supervised respawn re-clones a fresh set and re-subscribes a
        // new broadcast receiver — the monitor self-heals after a top-level
        // panic without a full app restart.
        let owner_lookup = self.owner_lookup();
        let email_config = config.clone();
        let router = Arc::clone(&self.router);
        let arc_store_ref = self._database.as_ref().map(|db| db.arc_store());
        let attachment_store_ref = self._database.as_ref().map(|db| db.attachment_store());
        let contact_store_ref = self.contact_store.clone();
        let profile_store_ref = self.profile_store.clone();
        let profile_embedder_ref = self.profile_embedder.read_recover().clone();
        let profile_embedding_cache_ref = Arc::clone(&self.profile_embedding_cache);
        let notifier = self.notifier.load_full();
        let coordinator_ref = Arc::clone(&self.coordinator);
        let task_arc_map_ref = Arc::clone(&self.task_arc_map);
        let pending_email_marks_ref = Arc::clone(&self.pending_email_marks);
        let dispatch_signal_ref = Arc::clone(&self.dispatch_signal);
        let approval_router_ref = self.approval_router.clone();

        // Set once when the loop observes a real shutdown signal. The
        // supervisor reads it to distinguish a clean stop (no respawn) from a
        // panic / unexpected exit (respawn).
        let stopped = Arc::new(AtomicBool::new(false));
        let stopped_for_check = Arc::clone(&stopped);

        tauri::async_runtime::spawn(spawn_supervised(
            "email",
            std::time::Duration::from_secs(5),
            move || stopped_for_check.load(std::sync::atomic::Ordering::Relaxed),
            move || {
                let mut monitor = EmailMonitor::new();
                if let Some(lookup) = owner_lookup.clone() {
                    monitor = monitor.with_owner_lookup(lookup);
                }
                let email_config = email_config.clone();
                let router = Arc::clone(&router);
                let arc_store_ref = arc_store_ref.clone();
                let attachment_store_ref = attachment_store_ref.clone();
                let contact_store_ref = contact_store_ref.clone();
                let profile_store_ref = profile_store_ref.clone();
                let profile_embedder_ref = Arc::clone(&profile_embedder_ref);
                let profile_embedding_cache_ref = Arc::clone(&profile_embedding_cache_ref);
                let notifier = notifier.clone();
                let coordinator_ref = Arc::clone(&coordinator_ref);
                let task_arc_map_ref = Arc::clone(&task_arc_map_ref);
                let pending_email_marks_ref = Arc::clone(&pending_email_marks_ref);
                let dispatch_signal_ref = Arc::clone(&dispatch_signal_ref);
                let approval_router_ref = approval_router_ref.clone();
                let ui = ui.clone();
                let mut shutdown = shutdown_tx.subscribe();
                let stopped = Arc::clone(&stopped);

                async move {
                    if let Err(e) = monitor.init(&email_config).await {
                        tracing::error!("Failed to initialize email monitor: {e}");
                        return;
                    }

                    let poll_interval = monitor.poll_interval();
                    info!("Email monitor started, polling every {:?}", poll_interval);

                    loop {
                        match monitor.poll().await {
                            Ok(events) if !events.is_empty() => {
                                info!("Email monitor received {} new event(s)", events.len());
                                for event in events {
                                    // Contain a panic in the inline dispatch
                                    // per-event: log it and move on so one bad
                                    // message can't kill the email sense.
                                    let res =
                                        AssertUnwindSafe(crate::sense_router::process_sense_event(
                                            &event,
                                            &router,
                                            &arc_store_ref,
                                            &profile_store_ref,
                                            &profile_embedder_ref,
                                            &profile_embedding_cache_ref,
                                            &ui,
                                            notifier.as_ref(),
                                            Some(&coordinator_ref),
                                            Some(&task_arc_map_ref),
                                            Some(&dispatch_signal_ref),
                                            approval_router_ref.as_ref(),
                                            Some(&pending_email_marks_ref),
                                            attachment_store_ref.as_ref(),
                                            contact_store_ref.as_ref(),
                                            None,
                                        ))
                                        .catch_unwind()
                                        .await;
                                    if res.is_err() {
                                        tracing::error!(
                                            sense = "email",
                                            "process_sense_event PANICKED; skipping event, monitor continues"
                                        );
                                    }
                                }
                            }
                            Ok(_) => {
                                tracing::debug!("Email poll: no new messages");
                            }
                            Err(e) => {
                                warn!("Email poll error: {e}");
                            }
                        }

                        tokio::select! {
                            _ = tokio::time::sleep(poll_interval) => {}
                            _ = shutdown.recv() => {
                                info!("Email monitor shutdown signal received");
                                stopped.store(true, std::sync::atomic::Ordering::Relaxed);
                                break;
                            }
                        }
                    }

                    if let Err(e) = monitor.shutdown().await {
                        warn!("Email monitor shutdown error: {e}");
                    }
                    info!("Email monitor stopped");
                }
            },
        ));
    }

    /// Stop the running email monitor (if any) and start a fresh one from
    /// current config. Called by `save_email_settings` so server/credential/
    /// interval/enabled changes apply without an app restart. When the panel
    /// disables email, the old loop is signalled to stop and `start_email_monitor`
    /// returns early — leaving no monitor running.
    pub fn restart_email_monitor(&self, ui: crate::ui_bridge::UiBridge) {
        if let Some(tx) = self.email_shutdown.lock_recover().take() {
            let _ = tx.send(());
        }
        self.start_email_monitor(ui);
    }

    /// Start the calendar monitor background task.
    ///
    /// Polls the local calendar database every 60 seconds for upcoming events
    /// and fires reminder SenseEvents through the sense router.
    /// Spawn the attachment TTL purger. No-op when no database is wired
    /// (CLI / test paths). Reads `byte_ttl_days` from the persisted
    /// `AttachmentPolicy` so user changes in Settings take effect on
    /// restart; falls back to the policy default if the config can't be
    /// loaded (fresh install, parse error).
    pub fn start_attachment_purger(&mut self) {
        let Some(store) = self.attachment_store() else {
            tracing::debug!("No attachment store wired; skipping TTL purger");
            return;
        };
        let cfg = crate::settings::load_main_config_public();
        let ttl_days = cfg.attachment_policy.byte_ttl_days;
        let (shutdown_tx, shutdown_rx) = tokio::sync::broadcast::channel::<()>(1);
        self.attachment_purger_shutdown = Some(shutdown_tx);
        // JoinHandle deliberately dropped — the loop runs until either the
        // process exits or the graceful-shutdown coordinator fires the
        // shutdown signal, same as the calendar/email/telegram monitors.
        drop(crate::attachment_purger::spawn_loop(
            store,
            ttl_days,
            crate::attachment_purger::DEFAULT_SWEEP_INTERVAL,
            shutdown_rx,
        ));
    }

    /// Rebuild the SMTP sender from current (vault-hydrated) config and
    /// hot-swap it in. Called by `save_smtp_settings` so SMTP changes apply
    /// without an app restart. New `email_send` calls `.load_full()` the
    /// fresh sender; arcs already mid-flight keep the one they snapshotted.
    pub(crate) async fn reload_email_sender(&self) {
        let mut cfg = crate::settings::load_main_config_public();
        crate::vault_creds::hydrate_secrets_from_vault(self.vault.as_ref(), &mut cfg).await;
        let rebuilt = build_email_sender(&cfg.email);
        *self.email_sender.write_recover() = rebuilt;
    }

    /// Rebuild the Telegram outbound sender from current (vault-hydrated)
    /// config and hot-swap it in. Called by `save_telegram_settings`.
    pub(crate) async fn reload_telegram_sender(&self) {
        let mut cfg = crate::settings::load_main_config_public();
        crate::vault_creds::hydrate_secrets_from_vault(self.vault.as_ref(), &mut cfg).await;
        let owner_chat_id_override =
            resolve_owner_telegram_chat_id(self.contact_store.as_ref()).await;
        let rebuilt = build_telegram_sender(&cfg.telegram, owner_chat_id_override);
        *self.telegram_sender.write_recover() = rebuilt;
    }

    /// Clone the current Remote Access status snapshot (the `lock_recover`
    /// trait is private to this module, so the command layer reads through
    /// here).
    pub fn remote_access_status_snapshot(&self) -> RemoteAccessStatus {
        let mut st = self.remote_access_status.lock_recover();
        // Refresh the install flag opportunistically — cheap PATH probe.
        st.cloudflared_installed = crate::tunnel::cloudflared_path().is_some();
        st.clone()
    }

    /// Start (or restart) the UI-controlled Remote Access HTTP listener on
    /// `127.0.0.1:port`. Idempotent — stops any existing listener first. When
    /// `tunnel_enabled`, brings up a cloudflared quick-tunnel in the background
    /// and stamps its URL into `remote_access_status` once it resolves, so the
    /// caller returns immediately. See [`docs/REMOTE_ACCESS.md`].
    pub async fn start_remote_access(
        &self,
        ui: crate::ui_bridge::UiBridge,
        port: u16,
        basic: Option<crate::http_api::BasicCreds>,
        tunnel_enabled: bool,
    ) {
        self.stop_remote_access().await;

        let data_dir =
            athen_core::paths::athen_data_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
        let token = crate::http_api::resolve_token(&data_dir);
        let addr: std::net::SocketAddr = ([127, 0, 0, 1], port).into();
        let cfg = crate::http_api::HttpApiConfig::from_settings(addr, token, basic);

        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        *self.remote_access_shutdown.lock_recover() = Some(tx);

        {
            let mut st = self.remote_access_status.lock_recover();
            st.listening = true;
            st.local_url = Some(format!("http://127.0.0.1:{port}"));
            st.tunnel_url = None;
            st.last_error = None;
            st.cloudflared_installed = crate::tunnel::cloudflared_path().is_some();
        }

        let ui_for_serve = ui.clone();
        tokio::spawn(async move {
            if let Err(e) = crate::http_api::serve_with_shutdown(cfg, ui_for_serve, rx).await {
                tracing::error!(error = %e, "Remote Access HTTP listener exited with error");
            }
        });
        tracing::info!(%addr, tunnel = tunnel_enabled, "Remote Access listener started");

        if tunnel_enabled {
            // cloudflared bring-up (download + handshake) can take ~20s. Do it
            // off the caller's path; resolve `&AppState` through the moved-in
            // `ui` since `&self` can't cross into a 'static task.
            tokio::spawn(async move {
                let state = ui.app_state();
                let path = match crate::tunnel::ensure_cloudflared(None).await {
                    Ok(p) => {
                        state
                            .remote_access_status
                            .lock_recover()
                            .cloudflared_installed = true;
                        p
                    }
                    Err(e) => {
                        state.remote_access_status.lock_recover().last_error =
                            Some(format!("cloudflared install failed: {e}"));
                        return;
                    }
                };
                match crate::tunnel::start_quick_tunnel(&path, port).await {
                    Ok(handle) => {
                        let url = handle.url().to_string();
                        *state.tunnel.lock_recover() = Some(handle);
                        let mut st = state.remote_access_status.lock_recover();
                        st.tunnel_url = Some(url.clone());
                        st.last_error = None;
                        drop(st);
                        tracing::info!(%url, "Cloudflare quick-tunnel up");
                    }
                    Err(e) => {
                        state.remote_access_status.lock_recover().last_error =
                            Some(format!("tunnel start failed: {e}"));
                    }
                }
            });
        }
    }

    /// Stop the Remote Access listener and tear down any live tunnel. Safe to
    /// call when nothing is running.
    pub async fn stop_remote_access(&self) {
        if let Some(tx) = self.remote_access_shutdown.lock_recover().take() {
            let _ = tx.send(());
        }
        let handle = self.tunnel.lock_recover().take();
        if let Some(h) = handle {
            h.stop().await;
        }
        *self.remote_access_status.lock_recover() = RemoteAccessStatus::default();
    }

    /// Rebuild the web-search provider chain from current (vault-hydrated)
    /// config and hot-swap it in. Called by `save_web_search_settings`.
    pub(crate) async fn reload_web_search(&self) {
        let mut cfg = crate::settings::load_main_config_public();
        crate::vault_creds::hydrate_secrets_from_vault(self.vault.as_ref(), &mut cfg).await;
        let rebuilt = build_web_search_provider(&cfg.web_search);
        *self.web_search.write_recover() = rebuilt;
    }

    /// Rebuild the embedding router from current (vault-hydrated) config and
    /// hot-swap it in. Called by `save_embedding_settings` so an embedding-mode
    /// change applies without a restart. Note: bundled-tier models that need a
    /// download will fall back to keyword search until the model is present
    /// (same as the boot path), so the swap never blocks on a download.
    pub(crate) async fn reload_embedder(&self) {
        let mut cfg = crate::settings::load_main_config_public();
        crate::vault_creds::hydrate_secrets_from_vault(self.vault.as_ref(), &mut cfg).await;
        let rebuilt: Arc<dyn athen_core::traits::embedding::EmbeddingProvider> =
            Arc::new(build_embedding_router(&cfg.embeddings));
        *self.profile_embedder.write_recover() = rebuilt;
    }

    /// Ensure a real (neural) embedder is available out of the box, so
    /// memory "just works" for users who never touch the Embedding
    /// settings. Spawns a background task that, when the effective mode
    /// would otherwise leave us on the weak keyword fallback, downloads
    /// the bundled Light tier (~270 MB, multilingual-e5-small) and then
    /// hot-swaps the live embedder via `reload_embedder`.
    ///
    /// Trigger rules (evaluated against the *hydrated* config):
    /// - `Automatic` with **no** cloud embedding api_key → ensure Light
    ///   (the builtin fallback that sits below Ollama/cloud in the chain).
    /// - `Bundled { tier }` → ensure that explicit tier is present, in
    ///   case a Settings download was interrupted.
    /// - `Off` / `Cloud` / `Specific` / `LocalOnly` → do nothing; the
    ///   user has either disabled memory or picked a provider explicitly.
    ///
    /// Idempotent and cheap when the weights already exist (it just
    /// returns). Network failures are non-fatal — we stay on keyword and
    /// retry on the next launch. Runs after `app.manage()` so the task
    /// can fetch the managed `AppState` to call `reload_embedder`.
    pub fn start_embedder_warmup(&self, ui: crate::ui_bridge::UiBridge) {
        let vault = self.vault.clone();
        tauri::async_runtime::spawn(async move {
            let mut cfg = crate::settings::load_main_config_public();
            crate::vault_creds::hydrate_secrets_from_vault(vault.as_ref(), &mut cfg).await;

            use athen_core::config::{BundledTier, EmbeddingMode};
            let tier = match cfg.embeddings.mode {
                EmbeddingMode::Off => None,
                EmbeddingMode::Bundled { tier } => Some(tier),
                EmbeddingMode::Automatic => {
                    let has_cloud_key = cfg
                        .embeddings
                        .api_key
                        .as_deref()
                        .is_some_and(|s| !s.is_empty());
                    if has_cloud_key {
                        None
                    } else {
                        Some(BundledTier::Light)
                    }
                }
                // Explicit provider modes — respect the user's choice.
                EmbeddingMode::Cloud | EmbeddingMode::Specific | EmbeddingMode::LocalOnly => None,
            };
            let Some(tier) = tier else {
                return;
            };

            let Some(data_dir) = athen_core::paths::athen_data_dir() else {
                return;
            };
            let cache_dir = data_dir.join("embeddings");

            #[cfg(feature = "bundled-embeddings")]
            {
                if crate::bundled_embeddings::is_tier_downloaded(&cache_dir, tier) {
                    // Already present — the boot-time router build already
                    // saw it (Automatic arm / Bundled arm), nothing to do.
                    return;
                }
                tracing::info!(
                    tier = ?tier,
                    "Embedder warmup: downloading bundled model so memory works out of the box"
                );
                ui.emit(
                    "embedding-download-progress",
                    serde_json::json!({
                        "tier": tier,
                        "phase": "starting",
                        "message": "Setting up local memory…",
                    }),
                );
                // fastembed exposes init only through embed() — a single
                // warmup string triggers download + ONNX load and returns
                // once both are done. The vector itself is discarded.
                let provider =
                    athen_llm::embeddings::bundled::BundledEmbedding::new(cache_dir, tier);
                let result = {
                    use athen_core::traits::embedding::EmbeddingProvider;
                    provider.embed("warmup").await
                };
                match result {
                    Ok(_) => {
                        tracing::info!(
                            "Embedder warmup: bundled model ready; hot-swapping live embedder"
                        );
                        ui.emit(
                            "embedding-download-progress",
                            serde_json::json!({
                                "tier": tier,
                                "phase": "complete",
                                "message": "Local memory ready.",
                            }),
                        );
                        ui.app_state().reload_embedder().await;
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "Embedder warmup: download failed; staying on keyword fallback until next launch"
                        );
                        ui.emit(
                            "embedding-download-progress",
                            serde_json::json!({
                                "tier": tier,
                                "phase": "failed",
                                "message": format!("Local memory setup failed: {e}"),
                            }),
                        );
                    }
                }
            }
            #[cfg(not(feature = "bundled-embeddings"))]
            {
                let _ = (cache_dir, tier);
            }
        });
    }

    /// Spawn the wake-up scheduler loop. Idempotent — does nothing if the
    /// store isn't wired or the loop is already running. Calls
    /// `arm_unscheduled(now)` first so freshly-created rows that lack a
    /// `next_fire_at` get armed before the first tick.
    pub fn start_wakeup_scheduler(&mut self, ui: crate::ui_bridge::UiBridge) {
        let Some(store) = self.wakeup_store.clone() else {
            tracing::debug!("No wake-up store wired; skipping scheduler");
            return;
        };
        if self.wakeup_scheduler_shutdown.lock_recover().is_some() {
            tracing::debug!("Wake-up scheduler already running");
            return;
        }
        let arc_store = self.arc_store.clone();
        // Phase 3b: coordinator-backed sink. Each fire becomes a synthetic
        // sense event the coordinator can risk-evaluate and queue, with
        // the resulting task registered for the dispatch loop. Phase 3c
        // will layer AutonomyBand + tool/contact allowlists on top.
        let sink: Arc<dyn athen_core::traits::wakeup::WakeupFireSink> =
            Arc::new(crate::wakeup_sink::CoordinatorWakeupSink::new(
                Arc::clone(&self.coordinator),
                arc_store,
                Arc::clone(&self.task_arc_map),
                Arc::clone(&self.task_wakeup_map),
                Arc::clone(&self.dispatch_signal),
                Some(ui),
            ));
        // The public shutdown API is a oneshot (see `wakeup_scheduler_shutdown`
        // + `shutdown_scheduler`). `WakeupScheduler::run` consumes a oneshot rx
        // exactly once, but the supervisor may respawn `run` several times. We
        // bridge the two: the public oneshot, when fired, sets `stopped` and
        // fires whichever per-attempt oneshot is currently parked, so the
        // running attempt unblocks and the supervisor sees a clean stop.
        let (public_tx, public_rx) = tokio::sync::oneshot::channel::<()>();
        *self.wakeup_scheduler_shutdown.lock_recover() = Some(public_tx);

        let stopped = Arc::new(AtomicBool::new(false));
        // Holds the current attempt's run() shutdown sender so the public
        // shutdown can unblock the in-flight `run`.
        let attempt_shutdown: Arc<Mutex<Option<tokio::sync::oneshot::Sender<()>>>> =
            Arc::new(Mutex::new(None));

        // Forward the single public shutdown into `stopped` + the parked
        // per-attempt sender.
        {
            let stopped = Arc::clone(&stopped);
            let attempt_shutdown = Arc::clone(&attempt_shutdown);
            tauri::async_runtime::spawn(async move {
                if public_rx.await.is_ok() {
                    stopped.store(true, std::sync::atomic::Ordering::Relaxed);
                    if let Some(tx) = attempt_shutdown.lock().await.take() {
                        let _ = tx.send(());
                    }
                }
            });
        }

        // Tick every 5 seconds. Fast enough that "remind me in 30 seconds"
        // feels prompt; slow enough that an idle laptop burns no real CPU.
        // Production-grade scheduling would key off the earliest
        // next_fire_at; for v1 a coarse poll is fine.
        let period = std::time::Duration::from_secs(5);
        let armed_once = Arc::new(AtomicBool::new(false));
        let stopped_for_check = Arc::clone(&stopped);

        // Tauri's setup hook is synchronous; use the Tauri-managed async
        // runtime for the same reason the email/calendar/telegram monitors
        // do — `tokio::spawn` here panics because no reactor is running on
        // this thread. The supervisor restarts `run` if it ever panics at a
        // level above the per-tick `catch_unwind` already inside the scheduler.
        tauri::async_runtime::spawn(spawn_supervised(
            "wakeup-scheduler",
            std::time::Duration::from_secs(5),
            move || stopped_for_check.load(std::sync::atomic::Ordering::Relaxed),
            move || {
                let store = store.clone();
                let sink = Arc::clone(&sink);
                let attempt_shutdown = Arc::clone(&attempt_shutdown);
                let armed_once = Arc::clone(&armed_once);
                async move {
                    let scheduler = athen_scheduler::WakeupScheduler::new(store, sink);
                    // Arm fresh rows once (first attempt only); a respawn must
                    // not re-arm rows the running scheduler already advanced.
                    if !armed_once.swap(true, std::sync::atomic::Ordering::Relaxed) {
                        match scheduler.arm_unscheduled(chrono::Utc::now()).await {
                            Ok(0) => {}
                            Ok(n) => tracing::info!("Armed {n} fresh wake-up(s)"),
                            Err(e) => tracing::warn!("Failed to arm fresh wake-ups: {e}"),
                        }
                    }
                    let (attempt_tx, attempt_rx) = tokio::sync::oneshot::channel::<()>();
                    *attempt_shutdown.lock().await = Some(attempt_tx);
                    scheduler.run(period, attempt_rx).await;
                    tracing::info!("Wake-up scheduler loop exited");
                }
            },
        ));
    }

    /// Spawn one background sync task per configured remote calendar source.
    ///
    /// Loads `calendar_sources` from SQLite, builds a `CalendarSource`
    /// adapter for each enabled row (pulling its password from the vault),
    /// then kicks off a per-source poll loop that reconciles `RemoteEvent`s
    /// into the local `CalendarStore`. The local `CalendarMonitor` polls
    /// that table independently for reminders, so the two pipelines stay
    /// decoupled.
    pub fn start_calendar_sync(&mut self, app_handle: Option<tauri::AppHandle>) {
        use std::sync::Arc as StdArc;

        let Some(db) = self._database.as_ref() else {
            tracing::debug!("Calendar sync skipped: no database");
            return;
        };
        let Some(calendar_store) = self.calendar_store.clone() else {
            tracing::debug!("Calendar sync skipped: no calendar_store");
            return;
        };
        let Some(vault) = self.vault.clone() else {
            tracing::debug!("Calendar sync skipped: no vault");
            return;
        };
        let cfg_store: StdArc<
            dyn athen_core::traits::calendar_source_config::CalendarSourceConfigStore,
        > = StdArc::new(db.calendar_source_store());

        let (shutdown_tx, _) = tokio::sync::broadcast::channel::<()>(1);
        self.calendar_sync_shutdown = Some(shutdown_tx.clone());

        let cfg_store_for_load = cfg_store.clone();
        tauri::async_runtime::spawn(async move {
            let sources = match cfg_store_for_load.list().await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("Calendar sync: failed to list sources: {e}");
                    return;
                }
            };
            if sources.is_empty() {
                tracing::info!("Calendar sync: no remote sources configured");
                return;
            }
            tracing::info!(
                count = sources.len(),
                "Calendar sync: spawning per-source loops"
            );
            crate::calendar_sources::spawn_sync_loops(
                sources,
                vault,
                calendar_store,
                cfg_store,
                shutdown_tx,
                app_handle,
            );
        });
    }

    pub fn start_calendar_monitor(&mut self, ui: crate::ui_bridge::UiBridge) {
        use athen_core::traits::sense::SenseMonitor;
        use athen_sentidos::calendar::CalendarMonitor;

        let config = load_config();
        let router = Arc::clone(&self.router);
        let arc_store_ref = self._database.as_ref().map(|db| db.arc_store());
        let attachment_store_ref = self._database.as_ref().map(|db| db.attachment_store());
        let contact_store_ref = self.contact_store.clone();
        let profile_store_ref = self.profile_store.clone();
        let profile_embedder_ref = self.profile_embedder.read_recover().clone();
        let profile_embedding_cache_ref = Arc::clone(&self.profile_embedding_cache);
        let notifier = self.notifier.load_full();
        let coordinator_ref = Arc::clone(&self.coordinator);
        let task_arc_map_ref = Arc::clone(&self.task_arc_map);
        let pending_email_marks_ref = Arc::clone(&self.pending_email_marks);
        let dispatch_signal_ref = Arc::clone(&self.dispatch_signal);
        let approval_router_ref = self.approval_router.clone();

        let (shutdown_tx, _) = tokio::sync::broadcast::channel::<()>(1);
        self.calendar_shutdown = Some(shutdown_tx.clone());

        let stopped = Arc::new(AtomicBool::new(false));
        let stopped_for_check = Arc::clone(&stopped);

        tauri::async_runtime::spawn(spawn_supervised(
            "calendar",
            std::time::Duration::from_secs(5),
            move || stopped_for_check.load(std::sync::atomic::Ordering::Relaxed),
            move || {
                let mut monitor = CalendarMonitor::new();
                let config = config.clone();
                let router = Arc::clone(&router);
                let arc_store_ref = arc_store_ref.clone();
                let attachment_store_ref = attachment_store_ref.clone();
                let contact_store_ref = contact_store_ref.clone();
                let profile_store_ref = profile_store_ref.clone();
                let profile_embedder_ref = Arc::clone(&profile_embedder_ref);
                let profile_embedding_cache_ref = Arc::clone(&profile_embedding_cache_ref);
                let notifier = notifier.clone();
                let coordinator_ref = Arc::clone(&coordinator_ref);
                let task_arc_map_ref = Arc::clone(&task_arc_map_ref);
                let pending_email_marks_ref = Arc::clone(&pending_email_marks_ref);
                let dispatch_signal_ref = Arc::clone(&dispatch_signal_ref);
                let approval_router_ref = approval_router_ref.clone();
                let ui = ui.clone();
                let mut shutdown = shutdown_tx.subscribe();
                let stopped = Arc::clone(&stopped);

                async move {
                    if let Err(e) = monitor.init(&config).await {
                        tracing::error!("Failed to initialize calendar monitor: {e}");
                        return;
                    }

                    let poll_interval = monitor.poll_interval();
                    info!(
                        "Calendar monitor started, polling every {:?}",
                        poll_interval
                    );

                    loop {
                        // Select the sleep so a shutdown signal during the sleep
                        // unblocks immediately. The poll itself isn't interruptible
                        // here — if it ever became slow we'd want to wrap it too.
                        tokio::select! {
                            _ = shutdown.recv() => {
                                info!("Calendar monitor shutdown signal received");
                                stopped.store(true, std::sync::atomic::Ordering::Relaxed);
                                break;
                            }
                            _ = tokio::time::sleep(poll_interval) => {}
                        }

                        match monitor.poll().await {
                            Ok(events) if !events.is_empty() => {
                                info!("Calendar monitor: {} reminder(s)", events.len());
                                for event in events {
                                    // Per-event panic containment (see email monitor).
                                    let res =
                                        AssertUnwindSafe(crate::sense_router::process_sense_event(
                                            &event,
                                            &router,
                                            &arc_store_ref,
                                            &profile_store_ref,
                                            &profile_embedder_ref,
                                            &profile_embedding_cache_ref,
                                            &ui,
                                            notifier.as_ref(),
                                            Some(&coordinator_ref),
                                            Some(&task_arc_map_ref),
                                            Some(&dispatch_signal_ref),
                                            approval_router_ref.as_ref(),
                                            Some(&pending_email_marks_ref),
                                            attachment_store_ref.as_ref(),
                                            contact_store_ref.as_ref(),
                                            None,
                                        ))
                                        .catch_unwind()
                                        .await;
                                    if res.is_err() {
                                        tracing::error!(
                                            sense = "calendar",
                                            "process_sense_event PANICKED; skipping event, monitor continues"
                                        );
                                    }
                                }
                            }
                            Ok(_) => {}
                            Err(e) => {
                                warn!("Calendar poll error: {e}");
                            }
                        }
                    }
                    info!("Calendar monitor stopped");
                }
            },
        ));
    }

    /// Start the Telegram bot monitor background polling task.
    ///
    /// Polls the Telegram Bot API via `getUpdates` for new messages and routes
    /// each through the sense router for LLM triage and arc creation.
    ///
    /// **Owner auto-execution**: messages from the owner (identified by
    /// `owner_user_id` in the Telegram config) have `source_risk == Safe`
    /// set by `TelegramMonitor`.  After normal sense routing (arc creation,
    /// triage, frontend notification), these messages are additionally
    /// executed through the agent — exactly as if the user typed them in
    /// the chat UI.  Non-owner messages continue through the standard
    /// sense router triage only.
    pub fn start_telegram_monitor(&self, ui: crate::ui_bridge::UiBridge) {
        use athen_core::traits::sense::SenseMonitor;
        use athen_sentidos::telegram::TelegramMonitor;

        let config = self.load_hydrated_config_sync();
        if !config.telegram.enabled {
            info!("Telegram monitor disabled in config, skipping startup");
            return;
        }

        if config.telegram.bot_token.is_empty() {
            warn!("Telegram monitor enabled but no bot token configured");
            return;
        }

        let (shutdown_tx, _) = tokio::sync::broadcast::channel::<()>(1);
        *self.telegram_shutdown.lock_recover() = Some(shutdown_tx.clone());

        let owner_lookup = self.owner_lookup();
        let telegram_settings = config.telegram.clone();
        let bot_token = config.telegram.bot_token.clone();
        let telegram_config = config.clone();
        let router = Arc::clone(&self.router);
        let arc_store_ref = self._database.as_ref().map(|db| db.arc_store());
        let attachment_store_ref = self._database.as_ref().map(|db| db.attachment_store());
        // Refs that the non-owner branch (process_sense_event) still
        // needs at the outer scope. Everything else the owner-Telegram
        // executor wants lives in `tool_registry_deps_ref` below.
        let profile_store_ref = self.profile_store.clone();
        let profile_embedder_ref = self.profile_embedder.read_recover().clone();
        let profile_embedding_cache_ref = Arc::clone(&self.profile_embedding_cache);
        let contact_store_ref = self.contact_store.clone();
        let notifier = self.notifier.load_full();
        let telegram_approval_sink = self.telegram_approval_sink.clone();
        let approval_router_ref = self.approval_router.clone();
        let coordinator_ref = Arc::clone(&self.coordinator);
        let task_arc_map_ref = Arc::clone(&self.task_arc_map);
        let pending_email_marks_ref = Arc::clone(&self.pending_email_marks);
        let dispatch_signal_ref = Arc::clone(&self.dispatch_signal);
        let telegram_chat_log_ref = self.telegram_chat_log.clone();
        let agent_registry_ref = self.agent_registry.clone();
        // Single bundle of everything the per-arc tool registry needs.
        // The owner-Telegram dispatch path clones this and hands it to
        // `assemble_app_tool_registry`, so the registry it sees is
        // structurally identical to what `AppState::build_tool_registry`
        // produces for the in-app flow.
        let tool_registry_deps_ref = self.tool_registry_deps();

        let stopped = Arc::new(AtomicBool::new(false));
        let stopped_for_check = Arc::clone(&stopped);

        tauri::async_runtime::spawn(spawn_supervised(
            "telegram",
            std::time::Duration::from_secs(5),
            move || stopped_for_check.load(std::sync::atomic::Ordering::Relaxed),
            move || {
                let mut monitor = TelegramMonitor::new(telegram_settings.clone());
                if let Some(lookup) = owner_lookup.clone() {
                    monitor = monitor.with_owner_lookup(lookup);
                }
                let bot_token = bot_token.clone();
                let telegram_config = telegram_config.clone();
                let router = Arc::clone(&router);
                let arc_store_ref = arc_store_ref.clone();
                let attachment_store_ref = attachment_store_ref.clone();
                let profile_store_ref = profile_store_ref.clone();
                let profile_embedder_ref = Arc::clone(&profile_embedder_ref);
                let profile_embedding_cache_ref = Arc::clone(&profile_embedding_cache_ref);
                let contact_store_ref = contact_store_ref.clone();
                let notifier = notifier.clone();
                let telegram_approval_sink = telegram_approval_sink.clone();
                let approval_router_ref = approval_router_ref.clone();
                let coordinator_ref = Arc::clone(&coordinator_ref);
                let task_arc_map_ref = Arc::clone(&task_arc_map_ref);
                let pending_email_marks_ref = Arc::clone(&pending_email_marks_ref);
                let dispatch_signal_ref = Arc::clone(&dispatch_signal_ref);
                let telegram_chat_log_ref = telegram_chat_log_ref.clone();
                let agent_registry_ref = agent_registry_ref.clone();
                let tool_registry_deps_ref = tool_registry_deps_ref.clone();
                let ui = ui.clone();
                let mut shutdown = shutdown_tx.subscribe();
                let stopped = Arc::clone(&stopped);

                async move {
                    if let Err(e) = monitor.init(&telegram_config).await {
                        tracing::error!("Failed to initialize Telegram monitor: {e}");
                        return;
                    }

                    let poll_interval = monitor.poll_interval();
                    info!(
                        "Telegram monitor started, polling every {:?}",
                        poll_interval
                    );

                    loop {
                        match monitor.poll().await {
                            Ok(events) if !events.is_empty() => {
                                info!("Telegram monitor received {} new event(s)", events.len());
                                for event in &events {
                                    let is_owner =
                                        event.source_risk == athen_core::risk::RiskLevel::Safe;

                                    if is_owner {
                                        // Owner messages skip triage/notification and go
                                        // straight to agent execution (like typing in the
                                        // chat).  Arc creation is handled inside.
                                        let text = event
                                            .content
                                            .body
                                            .get("text")
                                            .and_then(|v| v.as_str())
                                            .filter(|s| !s.is_empty())
                                            .or_else(|| {
                                                event
                                                    .content
                                                    .summary
                                                    .as_deref()
                                                    .filter(|s| !s.is_empty())
                                            })
                                            .unwrap_or("");

                                        let chat_id = event
                                            .content
                                            .body
                                            .get("chat_id")
                                            .and_then(|v| v.as_i64())
                                            .unwrap_or(0);

                                        // Treat a message as actionable if it has either
                                        // text OR attachments. A bare photo (caption-less)
                                        // arrives with text="[photo]" anyway, but a future
                                        // change that sends only attachments still needs
                                        // to reach the executor.
                                        let has_payload = !text.is_empty()
                                            || !event.content.attachments.is_empty();
                                        if has_payload && chat_id != 0 {
                                            // Spawn the handler so the poll loop keeps
                                            // ticking. If we awaited inline, callbacks
                                            // (Telegram inline-keyboard taps) would
                                            // pile up at Telegram while this message's
                                            // approval sat blocked — the user would
                                            // tap Approve and see no response until
                                            // the agent finished some other way.
                                            let text_owned = text.to_string();
                                            let bot_token_c = bot_token.clone();
                                            let ui_c = ui.clone();
                                            let notifier_c = notifier.clone();
                                            let profile_embedder_c =
                                                Arc::clone(&profile_embedder_ref);
                                            let profile_embedding_cache_c =
                                                Arc::clone(&profile_embedding_cache_ref);
                                            let agent_registry_c = agent_registry_ref.clone();
                                            let deps_c = tool_registry_deps_ref.clone();
                                            let event_id = event.id;
                                            let attachments_owned =
                                                event.content.attachments.clone();
                                            tauri::async_runtime::spawn(async move {
                                                // Contain a panic in the owner-message
                                                // executor so it can't silently abort
                                                // this handler task with no trace.
                                                let res = AssertUnwindSafe(
                                                    execute_owner_telegram_message(
                                                        &text_owned,
                                                        chat_id,
                                                        &bot_token_c,
                                                        event_id,
                                                        &attachments_owned,
                                                        &ui_c,
                                                        notifier_c.as_ref(),
                                                        &profile_embedder_c,
                                                        &profile_embedding_cache_c,
                                                        agent_registry_c.as_ref(),
                                                        deps_c,
                                                    ),
                                                )
                                                .catch_unwind()
                                                .await;
                                                if res.is_err() {
                                                    tracing::error!(
                                                sense = "telegram",
                                                "execute_owner_telegram_message PANICKED; message dropped, monitor unaffected"
                                            );
                                                }
                                            });
                                        }
                                    } else {
                                        // Non-owner messages go through the full sense
                                        // router: LLM triage, arc creation, notification,
                                        // and (when triage says it's action-worthy) hand
                                        // off to the coordinator for autonomous execution.
                                        // Per-event panic containment (see email monitor).
                                        let res = AssertUnwindSafe(
                                            crate::sense_router::process_sense_event(
                                                event,
                                                &router,
                                                &arc_store_ref,
                                                &profile_store_ref,
                                                &profile_embedder_ref,
                                                &profile_embedding_cache_ref,
                                                &ui,
                                                notifier.as_ref(),
                                                Some(&coordinator_ref),
                                                Some(&task_arc_map_ref),
                                                Some(&dispatch_signal_ref),
                                                approval_router_ref.as_ref(),
                                                Some(&pending_email_marks_ref),
                                                attachment_store_ref.as_ref(),
                                                contact_store_ref.as_ref(),
                                                telegram_chat_log_ref.as_ref(),
                                            ),
                                        )
                                        .catch_unwind()
                                        .await;
                                        if res.is_err() {
                                            tracing::error!(
                                        sense = "telegram",
                                        "process_sense_event PANICKED; skipping event, monitor continues"
                                    );
                                        }
                                    }
                                }
                            }
                            Ok(_) => {
                                tracing::debug!("Telegram poll: no new messages");
                            }
                            Err(e) => {
                                warn!("Telegram poll error: {e}");
                            }
                        }

                        // Drain any inline-keyboard taps captured during this
                        // poll and forward them to the approval sink. Done
                        // *after* poll() so a tap is resolved on the same
                        // iteration it arrives, not the next one.
                        let callbacks = monitor.take_callbacks();
                        if !callbacks.is_empty() {
                            info!(count = callbacks.len(), "Draining Telegram callback events");
                            for cb in callbacks {
                                // Deep Research depth-button taps (`dr|<token>|<depth>`)
                                // are ours, not approval-router questions — handle them
                                // before falling through to the approval sink.
                                if cb.data.starts_with("dr|") {
                                    handle_telegram_deepresearch_callback(&cb, &bot_token, &ui)
                                        .await;
                                    continue;
                                }
                                if let Some(ref sink) = telegram_approval_sink {
                                    let resolved =
                                        sink.resolve_callback(&cb.callback_id, &cb.data).await;
                                    info!(
                                        callback_id = %cb.callback_id,
                                        data = %cb.data,
                                        resolved,
                                        "Telegram callback dispatched"
                                    );
                                } else {
                                    warn!(
                                        callback_id = %cb.callback_id,
                                        "Telegram callback dropped — no approval sink configured"
                                    );
                                }
                            }
                        }

                        tokio::select! {
                            _ = tokio::time::sleep(poll_interval) => {}
                            _ = shutdown.recv() => {
                                info!("Telegram monitor shutdown signal received");
                                stopped.store(true, std::sync::atomic::Ordering::Relaxed);
                                break;
                            }
                        }
                    }

                    if let Err(e) = monitor.shutdown().await {
                        warn!("Telegram monitor shutdown error: {e}");
                    }
                    info!("Telegram monitor stopped");
                }
            },
        ));
    }

    /// Stop the running Telegram monitor (if any) and start a fresh one from
    /// current config. Called by `save_telegram_settings` so token/allowlist/
    /// interval/enabled changes apply without an app restart. The outbound
    /// sender is hot-swapped separately by `reload_telegram_sender`.
    pub fn restart_telegram_monitor(&self, ui: crate::ui_bridge::UiBridge) {
        if let Some(tx) = self.telegram_shutdown.lock_recover().take() {
            let _ = tx.send(());
        }
        self.start_telegram_monitor(ui);
    }

    /// Spawn the autonomous-execution dispatch loop.
    ///
    /// The loop pops sense-originated tasks from the coordinator queue
    /// and runs each through the agent in autonomous mode. It only acts
    /// on tasks whose id is registered in `task_arc_map` — i.e. tasks
    /// the sense_router enqueued — so user-driven `send_message` flows
    /// (which dispatch inline) are unaffected.
    ///
    /// Wakes on three triggers:
    /// 1. `dispatch_signal.notify_one()` (low-latency: sense_router
    ///    fires this right after enqueueing).
    /// 2. A 2-second tick (belt-and-brace; covers signals dropped while
    ///    the loop was busy with the previous batch).
    /// 3. The shutdown channel (clean teardown).
    ///
    /// Must be called AFTER `state.coordinator` is fully wired and
    /// AFTER the agent has been registered with the dispatcher,
    /// otherwise `dispatch_next_with_task` will keep returning `None`.
    pub fn start_dispatch_loop(&mut self, ui: crate::ui_bridge::UiBridge) {
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::broadcast::channel::<()>(1);
        self.dispatch_loop_shutdown = Some(shutdown_tx);

        // Snapshot every dependency the loop's per-task work needs into
        // owned/Arc-cloned form. The spawned task can't borrow `self`.
        let coordinator = Arc::clone(&self.coordinator);
        let dispatch_signal = Arc::clone(&self.dispatch_signal);
        let task_arc_map = Arc::clone(&self.task_arc_map);
        let task_wakeup_map = Arc::clone(&self.task_wakeup_map);
        let wakeup_store = self.wakeup_store.clone();
        let pending_email_marks = Arc::clone(&self.pending_email_marks);
        let router = Arc::clone(&self.router);
        let arc_store = self._database.as_ref().map(|db| db.arc_store());
        let calendar_store = self.calendar_store.clone();
        let contact_store = self.contact_store.clone();
        let memory = self.memory.clone();
        let mcp = Arc::clone(&self.mcp);
        let tool_doc_dir = self.tool_doc_dir.clone();
        let profile_store = self.profile_store.clone();
        let identity_store = self.identity_store.clone();
        let skill_store_dispatch = self.skill_store.clone();
        let http_endpoint_store_dispatch = self.http_endpoint_store.clone();
        let grant_store = self.grant_store.clone();
        let pending_grants = self.pending_grants.clone();
        let spawned_processes = self.spawned_processes.clone();
        let spawn_persistence = self.spawn_persistence.clone();
        let telegram_approval_sink = self.telegram_approval_sink.clone();
        let approval_router = self.approval_router.clone();
        let notifier = self.notifier.load_full();
        let compactor = self.compactor.clone();
        let web_search = self.web_search.read_recover().clone();
        let email_sender = self.email_sender.read_recover().clone();
        let telegram_sender_dispatch = self.telegram_sender.read_recover().clone();
        let telegram_outbound_hint_dispatch = self.telegram_outbound_hint.clone();
        let telegram_chat_log_dispatch = self.telegram_chat_log.clone();
        let owner_check_dispatch = self.owner_destination_check();
        let github_identity_resolver_dispatch = self.github_identity_resolver.clone();
        let checkpoint_store_dispatch = self.checkpoint_store.clone();
        // Snapshot the vault so the per-task IMAP mark-seen flow can
        // hydrate the IMAP password from it (the password lives in the
        // vault for installs that have re-saved their email settings).
        let vault_snapshot = self.vault.clone();
        // Snapshot the projects store so the dispatch loop can inject project
        // context into each autonomous task's prompt + registry.
        let project_store_dispatch = self.project_store.clone();
        // Cheap handle on the cached parsed config. The per-task resolvers
        // `.load()` it lock-free instead of re-reading + re-parsing the TOML
        // off disk every dispatched task. Live Settings saves swap a freshly
        // loaded config in via `reload_config_cache`, so a mid-session
        // active-provider switch is still observed by new tasks; in-flight arcs
        // ride their existing pin (see `docs/PROVIDER_PINNING.md`).
        let config_cache = Arc::clone(&self.config_cache);
        let attachment_store_loop = self.attachment_store();
        let inflight = Arc::clone(&self.inflight_approvals);
        let agent_registry_loop = self.agent_registry.clone();

        tauri::async_runtime::spawn(async move {
            use athen_core::traits::coordinator::TaskQueue;
            info!("Autonomous dispatch loop started");
            // The dispatch loop is the spine of proactivity: if it dies,
            // sense-enqueued tasks pile up forever and the agent goes deaf
            // with no surface signal. Its JoinHandle is dropped (fire-and-
            // forget), so a panic in the orchestration body would otherwise
            // vanish. Contain it: a panic in one iteration logs loudly and the
            // loop is re-entered rather than silently terminating. Clean
            // shutdown breaks out of the inner loop and returns past this.
            let mut clean_shutdown = false;
            while !clean_shutdown {
                let body = AssertUnwindSafe(async {
                    loop {
                        // Wait for the next wake-up trigger.
                        tokio::select! {
                            _ = dispatch_signal.notified() => {}
                            _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {}
                            _ = shutdown_rx.recv() => {
                                info!("Dispatch loop shutdown signal received");
                                return true;
                            }
                        }

                        // Drain everything the queue currently has. We keep
                        // looping until dispatch_next returns Ok(None) or the
                        // re-enqueue counter says we've cycled through tasks
                        // we don't own — preventing a tight infinite loop when
                        // the queue holds non-sense tasks.
                        let mut foreign_seen: usize = 0;
                        loop {
                            let dispatched = match coordinator.dispatch_next_with_task().await {
                                Ok(Some(pair)) => pair,
                                Ok(None) => break,
                                Err(e) => {
                                    warn!(error = %e, "dispatch_next_with_task failed");
                                    break;
                                }
                            };
                            let (task, _agent_id) = dispatched;

                            // Only act on tasks the sense_router registered.
                            // user-driven send_message tasks dispatch inline
                            // and never appear in task_arc_map.
                            let arc_id = task_arc_map.read().await.get(&task.id).cloned();
                            let Some(arc_id) = arc_id else {
                                // Re-enqueue and bail: this task belongs to a
                                // different code path (e.g. user send_message).
                                // CRITICAL: `dispatch_next_with_task` already pulled
                                // an agent out of the pool for this task. Hand it
                                // back before re-enqueueing — otherwise the (size-1)
                                // pool drains on the first foreign task and every
                                // later autonomous dispatch returns `None` forever,
                                // silently wedging proactivity until a user message
                                // happens to trigger `force_release_all`.
                                if let Err(e) =
                                    coordinator.dispatcher().release_agent(task.id).await
                                {
                                    tracing::debug!(task_id = %task.id, error = %e, "release_agent on foreign task (already released?)");
                                }
                                // Track foreign_seen so we eventually stop if
                                // the queue is dominated by non-sense tasks —
                                // otherwise we'd burn CPU recycling them.
                                foreign_seen = foreign_seen.saturating_add(1);
                                if let Err(e) = coordinator.queue().enqueue(task).await {
                                    warn!(error = %e, "Failed to re-enqueue foreign task");
                                }
                                if foreign_seen >= 8 {
                                    tracing::debug!(
                                        "Dispatch loop yielding after {foreign_seen} foreign tasks"
                                    );
                                    break;
                                }
                                continue;
                            };

                            let task_id = task.id;
                            // Was this task fired by a wake-up? If so, fetch the
                            // full row so the executor can apply the declared
                            // autonomy band + (Phase 3c2) tool/contact allowlists.
                            // Look-up failures fall through to "no wake-up" — the
                            // task still runs, just without restrictions.
                            let wakeup_id_opt = task_wakeup_map.read().await.get(&task.id).cloned();
                            let wakeup_for_ctx = if let (Some(id), Some(store)) =
                                (wakeup_id_opt, wakeup_store.as_ref())
                            {
                                use athen_core::traits::wakeup::WakeupStore;
                                match store.get(id).await {
                                    Ok(Some(w)) => Some(w),
                                    Ok(None) => {
                                        tracing::debug!(wakeup_id = %id, "wake-up row missing at dispatch");
                                        None
                                    }
                                    Err(e) => {
                                        warn!(wakeup_id = %id, error = %e, "wake-up lookup failed");
                                        None
                                    }
                                }
                            } else {
                                None
                            };
                            // A wake-up fire is the START of a fresh task (a
                            // synthetic sense event with a clock trigger), so it
                            // must not inherit a provider pin left behind by an
                            // earlier task on this arc. Without this, a recurring
                            // wake-up keeps using the model that was active when
                            // it first fired — even after the user switched
                            // Bundles or deleted that model — because the pin
                            // column survived (e.g. a previous run was killed
                            // before its clear, or the arc is long-lived).
                            // Clearing here lets the resolve below install a
                            // fresh pin from the current active Bundle. In-flight
                            // protection is unaffected: any task already running
                            // on this arc captured its router into its own ctx at
                            // dispatch and never re-reads this column.
                            if wakeup_id_opt.is_some() {
                                crate::state::clear_provider_pin_for_arc(
                                    arc_store.as_ref(),
                                    &arc_id,
                                )
                                .await;
                            }
                            // Resolve compaction budget per task. Reads the cached
                            // parsed config (lock-free `ArcSwap::load`) instead of
                            // re-reading + re-parsing the TOML off disk each dispatch;
                            // live Settings saves swap in a fresh config via
                            // `reload_config_cache`, so the user can still tune
                            // compaction / switch the active provider without a restart.
                            let cfg_arc = config_cache.load_full();
                            let cfg_for_resolvers: &AthenConfig = &cfg_arc;
                            let active_id_now =
                                crate::state::resolve_active_provider(cfg_for_resolvers);
                            let effective_target =
                                crate::state::resolve_effective_provider_for_arc(
                                    arc_store.as_ref(),
                                    &arc_id,
                                    &active_id_now,
                                    athen_core::llm::ModelProfile::Powerful,
                                )
                                .await;
                            let effective_provider_id = effective_target.provider_id.clone();
                            let (compaction_trigger_tokens, compaction_target_tokens) =
                                crate::compaction::resolve_compaction_budget(
                                    cfg_for_resolvers,
                                    &effective_provider_id,
                                );
                            let sampling_temperature =
                                crate::compaction::resolve_provider_temperature(
                                    cfg_for_resolvers,
                                    &effective_provider_id,
                                );
                            let reasoning_effort = crate::state::resolve_reasoning_effort_for_arc(
                                arc_store.as_ref(),
                                &arc_id,
                            )
                            .await;
                            let security_mode = crate::state::resolve_security_mode_for_arc(
                                arc_store.as_ref(),
                                &arc_id,
                                cfg_for_resolvers.security.mode,
                            )
                            .await;
                            // Per-arc router build: keeps the global router when
                            // no pin is in force, swaps in a slug-locked router
                            // when the arc has captured `(provider, slug)`. See
                            // `arc_router_for` and `docs/PROVIDER_PINNING.md`.
                            let arc_router = crate::state::arc_router_for(
                                &router,
                                &effective_target,
                                &active_id_now,
                                cfg_for_resolvers,
                                vault_snapshot.as_ref(),
                            )
                            .await;
                            let ctx = crate::commands::ApprovedTaskCtx {
                                coordinator: Arc::clone(&coordinator),
                                router: arc_router,
                                arc_store: arc_store.clone(),
                                calendar_store: calendar_store.clone(),
                                contact_store: contact_store.clone(),
                                memory: memory.clone(),
                                mcp: Arc::clone(&mcp),
                                tool_doc_dir: tool_doc_dir.clone(),
                                grant_store: grant_store.clone(),
                                profile_store: profile_store.clone(),
                                identity_store: identity_store.clone(),
                                skill_store: skill_store_dispatch.clone(),
                                http_endpoint_store: http_endpoint_store_dispatch.clone(),
                                pending_grants: pending_grants.clone(),
                                spawned_processes: spawned_processes.clone(),
                                spawn_persistence: spawn_persistence.clone(),
                                telegram_approval_sink: telegram_approval_sink.clone(),
                                cancel_flag: Arc::new(AtomicBool::new(false)),
                                active_arc_id: arc_id.clone(),
                                inflight: Arc::clone(&inflight),
                                ui: ui.clone(),
                                turn_id: uuid::Uuid::new_v4().to_string(),
                                message_override: None,
                                approval_router: approval_router.clone(),
                                notifier: notifier.clone(),
                                compactor: compactor.clone(),
                                web_search: Arc::clone(&web_search),
                                email_sender: email_sender.clone(),
                                telegram_sender: telegram_sender_dispatch.clone(),
                                telegram_outbound_hint: telegram_outbound_hint_dispatch.clone(),
                                telegram_chat_log: telegram_chat_log_dispatch.clone(),
                                owner_check: owner_check_dispatch.clone(),
                                github_identity_resolver: github_identity_resolver_dispatch.clone(),
                                checkpoint_store: checkpoint_store_dispatch.clone(),
                                initial_user_images: Vec::new(),
                                attachment_store: attachment_store_loop.clone(),
                                compaction_trigger_tokens,
                                compaction_target_tokens,
                                sampling_temperature,
                                reasoning_effort,
                                security_mode,
                                wakeup: wakeup_for_ctx,
                                // Sense-originated tasks don't carry composer
                                // uploads; the surfacing path uses the original
                                // event_id stamped on the email/Telegram arc
                                // entry, not on the user-message entry.
                                upload_event_id: None,
                                wakeup_store: wakeup_store
                                    .clone()
                                    .map(|s| s as Arc<dyn athen_core::traits::wakeup::WakeupStore>),
                                agent_registry: agent_registry_loop.clone(),
                                vault: vault_snapshot.clone(),
                                active_provider_id: effective_provider_id.clone(),
                                project_store: project_store_dispatch.clone(),
                            };

                            let task_arc_map_clone = Arc::clone(&task_arc_map);
                            let task_wakeup_map_clone = Arc::clone(&task_wakeup_map);
                            let pending_email_marks_clone = Arc::clone(&pending_email_marks);
                            let vault_snapshot = vault_snapshot.clone();
                            let config_cache = Arc::clone(&config_cache);
                            tauri::async_runtime::spawn(async move {
                                // Fire-and-forget finalization task: its JoinHandle is
                                // dropped, so a panic inside `execute_dispatched_task`
                                // would otherwise vanish silently and the cleanup
                                // below (map drains, email-mark handling) would never
                                // run. Contain the panic, log it loudly, and convert it
                                // into a failed outcome so the source email is left
                                // UNSEEN (re-triggers next poll) — same as any error.
                                let outcome = match AssertUnwindSafe(
                                    crate::commands::execute_dispatched_task(
                                        task,
                                        arc_id.clone(),
                                        ctx,
                                    ),
                                )
                                .catch_unwind()
                                .await
                                {
                                    Ok(o) => o,
                                    Err(_) => {
                                        tracing::error!(
                                            task_id = %task_id,
                                            arc = %arc_id,
                                            "Autonomous task PANICKED; treating as failure, cleaning up"
                                        );
                                        Err("dispatched task panicked".to_string())
                                    }
                                };

                                // Did the agent actually succeed at this task?
                                // Only on a true success do we flag the source
                                // email `\Seen` — failures must leave the email
                                // UNSEEN so the next IMAP poll re-triggers it.
                                let succeeded = matches!(&outcome, Ok(Some(o)) if o.success);

                                match &outcome {
                                    Ok(Some(o)) => {
                                        info!(
                                            task_id = %task_id,
                                            arc = %arc_id,
                                            success = o.success,
                                            "Autonomous task finished"
                                        );
                                    }
                                    Ok(None) => {
                                        tracing::debug!(
                                            task_id = %task_id,
                                            "Autonomous task skipped (already running on another channel)"
                                        );
                                    }
                                    Err(e) => {
                                        warn!(task_id = %task_id, arc = %arc_id, error = %e, "Autonomous task failed");
                                    }
                                }
                                // Always remove the mapping entry so the table
                                // doesn't leak even on failure.
                                task_arc_map_clone.write().await.remove(&task_id);
                                task_wakeup_map_clone.write().await.remove(&task_id);

                                // Drain the pending-email-mark entry too. On
                                // success: spawn a fire-and-forget IMAP STORE
                                // call. On failure (or skip): just drop it —
                                // the source email stays UNSEEN and will
                                // re-trigger on next poll, which is the user's
                                // explicit requirement.
                                let mark_info =
                                    pending_email_marks_clone.write().await.remove(&task_id);
                                if let Some(info) = mark_info {
                                    if succeeded {
                                        // Clone the cached parsed config into an owned
                                        // value for local mutation (vault hydration)
                                        // instead of re-reading + re-parsing the TOML.
                                        let mut config = (*config_cache.load_full()).clone();
                                        crate::vault_creds::hydrate_secrets_from_vault(
                                            vault_snapshot.as_ref(),
                                            &mut config,
                                        )
                                        .await;
                                        let email_config = config.email.clone();
                                        tokio::spawn(async move {
                                            match athen_sentidos::email::mark_uid_seen(
                                                &email_config,
                                                &info.folder,
                                                info.uid,
                                            )
                                            .await
                                            {
                                                Ok(()) => {
                                                    info!(
                                                        uid = info.uid,
                                                        folder = %info.folder,
                                                        "Marked email \\Seen after successful autonomous run"
                                                    );
                                                }
                                                Err(e) => {
                                                    warn!(
                                                        uid = info.uid,
                                                        folder = %info.folder,
                                                        error = %e,
                                                        "Failed to mark email \\Seen; will re-trigger on next poll"
                                                    );
                                                }
                                            }
                                        });
                                    } else {
                                        tracing::debug!(
                                            task_id = %task_id,
                                            uid = info.uid,
                                            folder = %info.folder,
                                            "task failed, leaving email UNSEEN, will re-trigger on next poll"
                                        );
                                    }
                                }
                            });
                        }
                    }
                });

                match body.catch_unwind().await {
                    Ok(stop) => clean_shutdown = stop,
                    Err(_) => {
                        // The orchestration body panicked. Log loudly and
                        // re-enter the loop after a short pause so a tight
                        // panic-loop can't peg a core. Per-task executor work
                        // already runs in its own panic-contained spawn, so
                        // this only fires for panics in the drain scaffolding.
                        tracing::error!(
                            "Autonomous dispatch loop PANICKED in orchestration body; re-entering after backoff"
                        );
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    }
                }
            }
            info!("Autonomous dispatch loop stopped");
        });
    }
}

// ---------------------------------------------------------------------------
// Owner Telegram auto-execution
// ---------------------------------------------------------------------------

/// Parse a leading `/deepresearch` (or `/deep_research`) command, tolerating a
/// `@botname` suffix on the command token. Returns the trimmed topic that
/// follows (which may be empty, e.g. a bare `/deepresearch`). `None` when the
/// message isn't this command.
fn parse_deepresearch_command(text: &str) -> Option<String> {
    let t = text.trim_start();
    if !t.starts_with('/') {
        return None;
    }
    let (head, rest) = match t.split_once(char::is_whitespace) {
        Some((h, r)) => (h, r),
        None => (t, ""),
    };
    // Strip an optional `@botname` so "/deepresearch@AthenBot topic" matches.
    let cmd = head.split('@').next().unwrap_or(head).to_ascii_lowercase();
    if cmd == "/deepresearch" || cmd == "/deep_research" {
        Some(rest.trim().to_string())
    } else {
        None
    }
}

/// Handle `/deepresearch <topic>` from the owner: create a dedicated research
/// arc, then send an inline keyboard offering the three depth levels. The
/// actual run is kicked off when a depth button is tapped (see
/// [`handle_telegram_deepresearch_callback`]).
#[allow(clippy::too_many_arguments)]
async fn handle_telegram_deepresearch_command(
    topic: &str,
    chat_id: i64,
    bot_token: &str,
    ui: &crate::ui_bridge::UiBridge,
    deps: &ToolRegistryDeps,
    profile_embedder: &Arc<dyn athen_core::traits::embedding::EmbeddingProvider>,
    profile_embedding_cache: &ProfileEmbeddingCache,
) {
    let topic = topic.trim();
    if topic.is_empty() {
        let _ = athen_sentidos::telegram::send_message(
            bot_token,
            chat_id,
            "Usage: /deepresearch <topic>\n\nExample:\n/deepresearch state of EU right-to-repair law vs California",
        )
        .await;
        return;
    }

    let Some(arc_store) = deps.arc_store.as_ref() else {
        let _ = athen_sentidos::telegram::send_message(
            bot_token,
            chat_id,
            "⚠️ Research is unavailable right now (no conversation store).",
        )
        .await;
        return;
    };

    // Dedicated arc per research, so its paper + follow-ups stay self-contained.
    let arc_id = crate::sense_router::generate_arc_id();
    let name = if topic.chars().count() > 40 {
        let cap = topic.floor_char_boundary(37);
        format!("{}...", &topic[..cap])
    } else {
        topic.to_string()
    };
    if let Err(e) = arc_store
        .create_arc(&arc_id, &name, athen_persistence::arcs::ArcSource::Messaging)
        .await
    {
        warn!("deepresearch: failed to create arc: {e}");
        let _ = athen_sentidos::telegram::send_message(
            bot_token,
            chat_id,
            "⚠️ Couldn't start research (failed to create a conversation).",
        )
        .await;
        return;
    }
    crate::sense_router::route_new_arc_to_profile(
        Some(arc_store),
        deps.profile_store.as_ref(),
        profile_embedder,
        profile_embedding_cache,
        Some(&deps.router),
        &arc_id,
        "user_input",
        &name,
        topic,
    )
    .await;

    // Park the request; the token rides in each button's callback_data.
    let token = ui.app_state().stash_pending_deep_research(PendingDeepResearch {
        arc_id,
        chat_id,
        question: topic.to_string(),
        created_at: chrono::Utc::now(),
    });

    let quick = format!("dr|{token}|quick");
    let standard = format!("dr|{token}|standard");
    let deep = format!("dr|{token}|deep");
    let buttons: Vec<(&str, &str)> = vec![
        ("⚡ Quick", quick.as_str()),
        ("📚 Standard", standard.as_str()),
        ("🔬 Deep", deep.as_str()),
    ];
    let prompt = format!(
        "🔎 Deep Research: \"{topic}\"\n\nChoose a depth:\n\
         • ⚡ Quick — fastest, 3 angles\n\
         • 📚 Standard — balanced, 6 angles\n\
         • 🔬 Deep — most thorough, 10 angles + a gap-fill pass (slowest)"
    );
    if let Err(e) =
        athen_sentidos::telegram::send_message_with_keyboard(bot_token, chat_id, &prompt, &buttons)
            .await
    {
        warn!("deepresearch: failed to send depth keyboard: {e}");
        let _ = ui.app_state().take_pending_deep_research(&token);
        let _ = athen_sentidos::telegram::send_message(
            bot_token,
            chat_id,
            "⚠️ Couldn't send the depth options. Please try again.",
        )
        .await;
    }
}

/// Resolve a `dr|<token>|<depth>` depth-button tap: ack it, confirm in-thread,
/// then run Deep Research in the background and deliver the paper. Returns fast
/// (spawns the run) so the Telegram poll loop keeps ticking during the
/// minutes-long research.
async fn handle_telegram_deepresearch_callback(
    cb: &athen_sentidos::telegram::TelegramCallbackEvent,
    bot_token: &str,
    ui: &crate::ui_bridge::UiBridge,
) {
    let parts: Vec<&str> = cb.data.splitn(3, '|').collect();
    if parts.len() != 3 {
        let _ = athen_sentidos::telegram::answer_callback_query(bot_token, &cb.callback_id, "").await;
        return;
    }
    let token = parts[1];
    let depth = parts[2].to_string();

    let pending = match ui.app_state().take_pending_deep_research(token) {
        Some(p) => p,
        None => {
            let _ = athen_sentidos::telegram::answer_callback_query(
                bot_token,
                &cb.callback_id,
                "That research request expired — send /deepresearch again.",
            )
            .await;
            return;
        }
    };

    // Clear the button spinner immediately.
    let _ = athen_sentidos::telegram::answer_callback_query(bot_token, &cb.callback_id, "Starting…")
        .await;

    // Replace the keyboard message with a running note (best-effort).
    let running = format!(
        "🔬 Researching ({depth}): \"{}\"\n\nThis can take a few minutes — I'll send the paper when it's ready.",
        pending.question
    );
    match cb.message_id {
        Some(mid) => {
            let _ = athen_sentidos::telegram::edit_message_text(
                bot_token,
                pending.chat_id,
                mid,
                &running,
            )
            .await;
        }
        None => {
            let _ = athen_sentidos::telegram::send_message(bot_token, pending.chat_id, &running)
                .await;
        }
    }

    // Run in the background so the poll loop is never blocked.
    let bot_token = bot_token.to_string();
    let ui = ui.clone();
    tauri::async_runtime::spawn(async move {
        let res = AssertUnwindSafe(crate::commands::deep_research_core(
            pending.arc_id.clone(),
            pending.question.clone(),
            Some(depth.clone()),
            None,
            ui.app_state(),
            ui.clone(),
        ))
        .catch_unwind()
        .await;
        match res {
            Ok(Ok(result)) => {
                deliver_deepresearch_paper_telegram(&bot_token, pending.chat_id, &result).await;
                // Freshen the outbound hint so the owner's immediate follow-up
                // routes back to the research arc (the 2-min routing window).
                ui.app_state()
                    .telegram_outbound_hint
                    .lock_recover()
                    .replace((result.arc_id.clone(), chrono::Utc::now()));
            }
            Ok(Err(e)) => {
                warn!("deepresearch: run failed: {e}");
                let _ = athen_sentidos::telegram::send_message(
                    &bot_token,
                    pending.chat_id,
                    &format!("⚠️ Research failed: {e}"),
                )
                .await;
            }
            Err(_) => {
                tracing::error!("deepresearch: run PANICKED");
                let _ = athen_sentidos::telegram::send_message(
                    &bot_token,
                    pending.chat_id,
                    "⚠️ Research crashed unexpectedly. Please try again.",
                )
                .await;
            }
        }
    });
}

/// Deliver a finished research paper to Telegram: the `.md` file as a document
/// with a short stats caption. Falls back to a text note if the upload fails.
async fn deliver_deepresearch_paper_telegram(
    bot_token: &str,
    chat_id: i64,
    result: &crate::commands::DeepResearchResult,
) {
    use athen_core::traits::telegram_sender::{
        OutboundTelegramMessage, TelegramAttachment, TelegramAttachmentKind, TelegramSender,
    };

    let abs = athen_core::paths::resolve_in_workspace(std::path::Path::new(&result.paper_path));
    let kind = if result.extended {
        "extended"
    } else {
        "complete"
    };
    let caption = format!(
        "✅ Research {kind}: \"{}\"\n{} of {} researchers reported · depth: {}\n\nFull paper attached. Reply here to ask follow-up questions about it.",
        result.question, result.workers_ok, result.workers_total, result.depth
    );

    match athen_sentidos::telegram_send::BotApiTelegramSender::new(bot_token, Some(chat_id)) {
        Ok(sender) => {
            let msg = OutboundTelegramMessage {
                chat_id: Some(chat_id),
                text: Some(caption.clone()),
                attachments: vec![TelegramAttachment {
                    path: abs,
                    kind: TelegramAttachmentKind::Document,
                    caption: None,
                }],
                reply_to_message_id: None,
            };
            if let Err(e) = sender.send(&msg).await {
                warn!("deepresearch: failed to send paper document: {e}");
                let _ = athen_sentidos::telegram::send_message(
                    bot_token,
                    chat_id,
                    &format!(
                        "✅ Research {kind}: \"{}\". The paper is saved at {} — ask me about it here.",
                        result.question, result.paper_path
                    ),
                )
                .await;
            }
        }
        Err(e) => {
            warn!("deepresearch: telegram sender init failed: {e}");
        }
    }
}

/// Execute a Telegram message from the owner through the agent, just like
/// `send_message` does for direct UI input.
///
/// This skips risk evaluation (owner messages are trusted) and goes straight
/// to agent execution.  The response is persisted to the most recent arc
/// (created by the sense router moments before) and streamed to the frontend.
#[allow(clippy::too_many_arguments)]
async fn execute_owner_telegram_message(
    text: &str,
    chat_id: i64,
    bot_token: &str,
    event_id: uuid::Uuid,
    attachments: &[athen_core::event::Attachment],
    ui: &crate::ui_bridge::UiBridge,
    notifier: Option<&Arc<NotificationOrchestrator>>,
    profile_embedder: &Arc<dyn athen_core::traits::embedding::EmbeddingProvider>,
    profile_embedding_cache: &ProfileEmbeddingCache,
    agent_registry: Option<&Arc<crate::agent_registry::AgentRegistry>>,
    deps: ToolRegistryDeps,
) {
    use std::time::Duration;

    use crate::commands::{new_tool_log, spawn_stream_forwarder, AgentProgress, TauriAuditor};
    use athen_agent::AgentBuilder;
    use athen_core::task::{DomainType, Task, TaskPriority, TaskStatus};
    use athen_core::traits::agent::AgentExecutor;

    // Local aliases so the body keeps reading like the original — every
    // collaborator reaches into `deps` so the registry build below uses
    // the SAME values we surface to the executor / arc-matching / footer
    // logic. Owner-Telegram now goes through `assemble_app_tool_registry`
    // exactly like the in-app path (#248).
    let router = &deps.router;
    let arc_store = &deps.arc_store;
    let attachment_store = deps.attachment_store.as_ref();
    let profile_store = &deps.profile_store;
    let telegram_chat_log = deps.telegram_chat_log.as_ref();
    let http_endpoint_store = deps.http_endpoint_store.as_ref();
    let telegram_outbound_hint = &deps.telegram_outbound_hint;

    info!("Executing owner Telegram message through agent: {}", text);

    // `/deepresearch <topic>` is a dedicated command: rather than running a
    // normal agent turn, create a research arc and offer depth buttons. The
    // tap (a callback) kicks off the run. Intercept before any /newarc/arc
    // routing so the command never reaches the executor.
    if let Some(topic) = parse_deepresearch_command(text) {
        handle_telegram_deepresearch_command(
            &topic,
            chat_id,
            bot_token,
            ui,
            &deps,
            profile_embedder,
            profile_embedding_cache,
        )
        .await;
        return;
    }

    // Stable id for every entry produced by this Telegram turn — user msg,
    // tool calls, assistant reply — so the UI groups them on rehydration.
    let turn_id = uuid::Uuid::new_v4().to_string();

    // Strip a leading `/newarc` command. When present, force a fresh arc
    // regardless of any heuristic match — this is the user's escape hatch
    // when arc routing guesses wrong. Stripped text gets sent to the agent
    // so a bare `/newarc` produces a new empty arc with no agent action.
    let (force_new_arc, stripped_text) = parse_newarc_command(text);
    let text = stripped_text.as_str();

    // Track whether the arc was reused vs freshly created so we can append
    // a visibility footer to reused arcs only — fresh arcs are obviously
    // theirs, the bot's reply is the first content in them.
    let mut arc_was_reused = false;
    let mut arc_match_reason: Option<&'static str> = None;

    // Find or create an arc for this Telegram conversation. Priority order:
    //   1. /newarc command → force fresh.
    //   2. Most recent outbound Telegram notification (≤ 2 min) → highest
    //      signal that this short reply is about that arc, even cross-
    //      channel (an Email arc can match here).
    //   3. Active arc with primary_reply_channel = "telegram" updated in
    //      the last 5 min → ongoing Telegram thread.
    //   4. Any Messaging-source arc updated in the last 5 min → fallback.
    //   5. Create new.
    let target_arc_id: Option<String> = if !force_new_arc {
        if let Some(store) = arc_store {
            // Tier 2: outbound-notification hint. Read the slot once,
            // then verify the arc still exists and isn't archived (the
            // hint is in-memory and survives DB archive/delete writes).
            let hint_match =
                telegram_outbound_hint
                    .lock_recover()
                    .clone()
                    .and_then(|(arc_id, ts)| {
                        let now = chrono::Utc::now();
                        if now.signed_duration_since(ts).num_seconds() < 120 {
                            Some(arc_id)
                        } else {
                            None
                        }
                    });
            match hint_match {
                Some(arc_id) => match store.get_arc(&arc_id).await {
                    Ok(Some(meta)) if meta.status == athen_persistence::arcs::ArcStatus::Active => {
                        info!(
                            arc = %arc_id,
                            "Routing owner Telegram message via outbound-notification hint"
                        );
                        arc_match_reason = Some("notification_hint");
                        Some(arc_id)
                    }
                    _ => None,
                },
                None => None,
            }
        } else {
            None
        }
    } else {
        None
    };

    // Tier 3+4 + creation, only if tiers 1 and 2 didn't claim the arc.
    let target_arc_id = if let Some(id) = target_arc_id {
        arc_was_reused = true;
        Some(id)
    } else if let Some(store) = arc_store {
        match store.list_arcs().await {
            Ok(arcs) => {
                let now = chrono::Utc::now();
                let within_window = |a: &athen_persistence::arcs::ArcMeta, secs: i64| -> bool {
                    chrono::DateTime::parse_from_rfc3339(&a.updated_at)
                        .map(|t| now.signed_duration_since(t).num_seconds() < secs)
                        .unwrap_or(false)
                };

                // Tier 3: arcs the owner has been actively replying to via
                // Telegram. primary_reply_channel is set to "telegram" by
                // this same handler (line below) every time the owner
                // engages, so multi-turn Telegram threads stay sticky.
                let tier3 = if force_new_arc {
                    None
                } else {
                    arcs.iter()
                        .filter(|a| a.status == athen_persistence::arcs::ArcStatus::Active)
                        .filter(|a| a.primary_reply_channel.as_deref() == Some("telegram"))
                        .find(|a| within_window(a, 300))
                        .map(|a| a.id.clone())
                };

                // Tier 4: any recent Messaging-source arc. Today's
                // pre-#149 behaviour, kept as a last-resort fallback.
                let tier4 = if force_new_arc {
                    None
                } else {
                    arcs.iter()
                        .filter(|a| {
                            a.source == athen_persistence::arcs::ArcSource::Messaging
                                && a.status == athen_persistence::arcs::ArcStatus::Active
                        })
                        .find(|a| within_window(a, 300))
                        .map(|a| a.id.clone())
                };

                // Tier 3.5 (LLM): when tiers 3 *and* 4 produce different
                // candidates (or only tier 4 fires — i.e. a Messaging arc
                // exists but it wasn't a Telegram reply thread), use an
                // LLM with the per-chat transcript to decide whether
                // this new turn continues one of the candidates or
                // starts something new. Skipped on /newarc and when no
                // candidate exists.
                let llm_pick: Option<String> = if force_new_arc {
                    None
                } else {
                    let candidates: Vec<athen_persistence::arcs::ArcMeta> = arcs
                        .iter()
                        .filter(|a| {
                            a.status == athen_persistence::arcs::ArcStatus::Active
                                && (a.source == athen_persistence::arcs::ArcSource::Messaging
                                    || a.primary_reply_channel.as_deref() == Some("telegram"))
                        })
                        .filter(|a| within_window(a, 1800))
                        .take(6)
                        .cloned()
                        .collect();
                    if candidates.is_empty() {
                        None
                    } else {
                        let chat_history = match telegram_chat_log {
                            Some(s) => s.recent(chat_id, 4).await.unwrap_or_default(),
                            None => Vec::new(),
                        };
                        pick_arc_with_llm(router, text, &candidates, &chat_history, store).await
                    }
                };

                if let Some(id) = llm_pick {
                    info!(arc = %id, "Routing owner Telegram message via LLM arc-pick");
                    arc_match_reason = Some("llm_pick");
                    arc_was_reused = true;
                    Some(id)
                } else if let Some(id) = tier3 {
                    info!(arc = %id, "Routing owner Telegram message via primary_reply_channel hint");
                    arc_match_reason = Some("primary_reply_channel");
                    arc_was_reused = true;
                    Some(id)
                } else if let Some(id) = tier4 {
                    info!(arc = %id, "Routing owner Telegram message via recent-messaging fallback");
                    arc_match_reason = Some("recent_messaging");
                    arc_was_reused = true;
                    Some(id)
                } else {
                    let arc_id = crate::sense_router::generate_arc_id();
                    let name = if text.len() > 30 {
                        let cap = text.floor_char_boundary(27);
                        format!("{}...", &text[..cap])
                    } else if text.is_empty() {
                        // Bare /newarc: give it a placeholder name. The
                        // agent's first real message will rename it.
                        "New Telegram arc".to_string()
                    } else {
                        text.to_string()
                    };
                    if let Err(e) = store
                        .create_arc(
                            &arc_id,
                            &name,
                            athen_persistence::arcs::ArcSource::Messaging,
                        )
                        .await
                    {
                        warn!("Failed to create arc for Telegram message: {e}");
                    }
                    info!("Created new Telegram arc: {}", arc_id);
                    // Owner DMs are direct user input, not an inbound message
                    // forwarded by a third party — pass `source="user_input"`
                    // so classify_task runs domain inference from keywords
                    // instead of forcing DomainTag::Messaging.
                    crate::sense_router::route_new_arc_to_profile(
                        Some(store),
                        profile_store.as_ref(),
                        profile_embedder,
                        profile_embedding_cache,
                        Some(router),
                        &arc_id,
                        "user_input",
                        &name,
                        text,
                    )
                    .await;
                    Some(arc_id)
                }
            }
            Err(e) => {
                // Surface, don't drop: arc routing failed, so we can't place
                // this message anywhere. Previously we returned None and let
                // the handler limp on doing nothing — the owner's Telegram
                // message just vanished with no reply. Tell them to retry and
                // stop here rather than spinning the executor against no arc.
                warn!("Failed to list arcs for owner message: {e}");
                if let Err(e2) = athen_sentidos::telegram::send_message(
                    bot_token,
                    chat_id,
                    "⚠️ Sorry, I couldn't process that just now (internal error reading your conversations). Please try again.",
                )
                .await
                {
                    warn!("Failed to send arc-routing failure notice on Telegram: {e2}");
                }
                return;
            }
        }
    } else {
        None
    };

    // If `/newarc` arrived with no follow-up text, the agent has nothing
    // to do — confirm the reset and return without spinning the executor.
    if force_new_arc && text.is_empty() {
        // Ack reliably: this is the ONLY feedback the owner gets for a bare
        // /newarc, so a single failed send used to leave them staring at an
        // unacknowledged command. Retry once with a short backoff, and log
        // loudly (error, not warn) if it still can't be delivered.
        let ack = "📍 New arc started. Send your message.";
        let mut acked = false;
        for attempt in 0..2 {
            match athen_sentidos::telegram::send_message(bot_token, chat_id, ack).await {
                Ok(()) => {
                    acked = true;
                    break;
                }
                Err(e) => {
                    warn!(attempt = attempt + 1, "Failed to send /newarc ack: {e}");
                    if attempt == 0 {
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    }
                }
            }
        }
        if !acked {
            tracing::error!(
                chat_id = %chat_id,
                "Could not acknowledge /newarc on Telegram after retries — owner has no confirmation their reset landed"
            );
        }
        return;
    }

    // Mark any pending notifications for this arc as read — the owner is
    // actively engaging via Telegram, so in-app notifications are redundant.
    if let (Some(notifier), Some(ref arc_id)) = (notifier, &target_arc_id) {
        notifier.mark_arc_read(arc_id).await;
    }

    // Record the channel the owner just engaged through. The approval
    // router uses this to bias follow-up questions toward the channel
    // the user is already actively reading.
    if let (Some(store), Some(ref arc_id)) = (arc_store, &target_arc_id) {
        if let Err(e) = store.set_primary_reply_channel(arc_id, "telegram").await {
            tracing::debug!("Failed to update primary_reply_channel: {e}");
        }
    }

    // Goal-intent triage: if the arc has a blocked goal, classify the user's
    // message as CONTINUE (reactivate goal) or ABANDON (clear goal) before
    // the executor runs. Same logic as the in-app path in commands.rs.
    if let (Some(store), Some(ref arc_id)) = (arc_store, &target_arc_id) {
        if let Ok(Some(meta)) = store.get_arc(arc_id).await {
            if meta.goal_status.as_deref() == Some("blocked") {
                if let (Some(ref goal), Some(ref reason)) =
                    (&meta.user_goal, &meta.goal_blocked_reason)
                {
                    let router_guard = router.read().await;
                    let router_clone = router_guard.clone();
                    drop(router_guard);
                    let should_abandon = crate::commands::classify_goal_intent(
                        router_clone.as_ref(),
                        text,
                        goal,
                        reason,
                    )
                    .await;
                    if should_abandon {
                        let _ = store.clear_user_goal(arc_id).await;
                        tracing::info!(arc = %arc_id, "Goal abandoned by user intent (Telegram)");
                    } else {
                        let _ = store.set_goal_active(arc_id).await;
                        tracing::info!(arc = %arc_id, "Goal reactivated by user intent (Telegram)");
                    }
                }
            }
        }
    }

    // Load conversation history from the arc for context continuity.
    let mut context = if let (Some(store), Some(ref arc_id)) = (arc_store, &target_arc_id) {
        match store.load_entries(arc_id).await {
            Ok(entries) => entries
                .into_iter()
                .filter(|e| e.entry_type == athen_persistence::arcs::EntryType::Message)
                .filter_map(|e| {
                    let role = match e.source.as_str() {
                        "user" => athen_core::llm::Role::User,
                        "assistant" => athen_core::llm::Role::Assistant,
                        "system" => athen_core::llm::Role::System,
                        _ => return None,
                    };
                    Some(athen_core::llm::ChatMessage {
                        role,
                        content: athen_core::llm::MessageContent::Text(e.content),
                    })
                })
                .collect::<Vec<_>>(),
            Err(_) => vec![],
        }
    } else {
        vec![]
    };

    // Persist attachment refs so refetch / lookup tools can resolve by id
    // later, and so prepare_attachment_surfacing can read them. Owner
    // Telegram messages bypass process_sense_event entirely, so the
    // insert that the email/non-owner path gets at sense_router::Step 3
    // has to be done explicitly here.
    if let Some(astore) = attachment_store {
        for att in attachments {
            if let Err(e) = astore.insert(event_id, att).await {
                warn!(
                    event_id = %event_id,
                    attachment = %att.name,
                    error = %e,
                    "Failed to persist owner-Telegram attachment ref"
                );
            }
        }
    }

    // Chat-history context injection (safety net for arc-routing
    // failures). For a freshly-created arc, the agent has zero history
    // to lean on — pull the last 4 messages from the per-`chat_id`
    // transcript and inject them as a System bubble so the conversation
    // stays coherent across arc boundaries. Reused arcs already loaded
    // their own history into `context` above, so we skip injection
    // there to avoid duplicating tokens on every turn.
    //
    // TODO: when prompt-size modes ship (Compact/Balanced/Full), gate
    // this on Balanced+ so small-context-window models don't pay.
    if !arc_was_reused {
        if let Some(store) = telegram_chat_log {
            match store.recent(chat_id, 4).await {
                Ok(rows) if !rows.is_empty() => {
                    let mut buf = String::from(
                        "<CONTEXT type=\"telegram-chat-history\">\n\
                         Recent messages with this Telegram chat (newest last). \
                         Use for conversational continuity — the user may be \
                         referring back to one of these.\n",
                    );
                    for row in &rows {
                        let who = match row.direction {
                            athen_persistence::telegram_chat_log::TelegramLogDirection::Inbound => "user",
                            athen_persistence::telegram_chat_log::TelegramLogDirection::Outbound => "assistant",
                        };
                        buf.push_str(&format!(
                            "[{ts}] {who}: {body}\n",
                            ts = row.ts,
                            who = who,
                            body = row.text
                        ));
                    }
                    buf.push_str("</CONTEXT>");
                    context.push(athen_core::llm::ChatMessage {
                        role: athen_core::llm::Role::System,
                        content: athen_core::llm::MessageContent::Text(buf),
                    });
                    tracing::info!(
                        chat_id,
                        injected = rows.len(),
                        "Injected Telegram chat-history context (fresh arc)"
                    );
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(error = %e, chat_id, "telegram_chat_log recent failed");
                }
            }
        }
    }
    // Log this inbound turn AFTER fetching the context window above, so
    // the injection contains the *prior* exchange, not the message
    // we're about to feed to the agent.
    if let Some(store) = telegram_chat_log {
        if let Err(e) = store
            .append(
                chat_id,
                athen_persistence::telegram_chat_log::TelegramLogDirection::Inbound,
                text,
                !attachments.is_empty(),
            )
            .await
        {
            tracing::warn!(error = %e, chat_id, "telegram_chat_log append (inbound) failed");
        }
    }

    // Surface attachments into the executor's first turn — images go to
    // the multimodal user content (when the active provider has vision)
    // and PDF text sidecars get inlined as a System turn. This is the
    // same path execute_dispatched_task takes for non-owner sense events.
    let mut surfaced_images: Vec<athen_core::llm::ImageInput> = Vec::new();
    if let Some(astore) = attachment_store {
        let router_guard = router.read().await;
        let supports_vision = router_guard.any_provider_supports_vision();
        let supports_documents = router_guard.any_provider_supports_documents();
        drop(router_guard);
        let surfacing = crate::commands::prepare_attachment_surfacing(
            event_id,
            astore,
            supports_vision,
            supports_documents,
        )
        .await;
        if let Some(msg) = surfacing.system_message {
            tracing::info!(
                event_id = %event_id,
                images = surfacing.images.len(),
                "Surfacing attachments to owner-Telegram executor"
            );
            context.push(athen_core::llm::ChatMessage {
                role: athen_core::llm::Role::System,
                content: athen_core::llm::MessageContent::Text(msg),
            });
        }
        surfaced_images = surfacing.images;
    }

    // Build the executor (mirrors send_message logic but without risk/coordinator).
    let exec_router: Box<dyn athen_core::traits::llm::LlmRouter> =
        Box::new(SharedRouter(Arc::clone(router)));
    // Single shared assembly site — same helper the in-app dispatch
    // path uses. Owner-Telegram now gets delegation + wake-up + per-arc
    // GitHub identity automatically (no more silent feature carve-outs).
    let registry = assemble_app_tool_registry(
        deps.clone(),
        target_arc_id.as_deref().unwrap_or(""),
        Some(ui.clone()),
    )
    .await;
    // Shared list of successful tool names; the auditor appends as steps
    // finish, and we read it after execute to build the Telegram footer.
    let tool_log = new_tool_log();

    // Live progress reporter: a single status message edited in place as
    // tools fire, plus a 4s typing-indicator loop. Without this the bot
    // is mute from "send" until "final reply" — visibly broken on tasks
    // that take >5s. Posted before execute so the user sees activity
    // immediately, finalized below regardless of success/failure.
    let progress = Arc::new(crate::telegram_progress::TelegramProgressReporter::new(
        bot_token.to_string(),
        chat_id,
    ));
    progress.start().await;

    // Pre-allocate executor task id so the live agent registry can
    // address it BEFORE execute() begins.
    let task_id_for_run = uuid::Uuid::new_v4();

    // Register with the live agent registry so the desktop "watch the
    // agents work" panel sees this Telegram-driven run too.
    let agent_guard = if let Some(reg) = agent_registry {
        let now = chrono::Utc::now();
        let title = crate::commands::truncate_title(text, 200);
        Some(
            reg.register(crate::agent_registry::ActiveAgent {
                task_id: task_id_for_run.to_string(),
                arc_id: target_arc_id.clone(),
                parent_arc_id: None,
                source: crate::agent_registry::AgentSource::Telegram,
                title,
                started_at: now,
                last_step_at: now,
                current_tool: None,
                current_action: None,
                step_count: 0,
                profile_id: None,
                model: None,
                turn_id: Some(turn_id.clone()),
            })
            .await,
        )
    } else {
        None
    };

    let mut auditor = TauriAuditor::new(
        ui.clone(),
        arc_store.clone(),
        target_arc_id.clone().unwrap_or_default(),
        turn_id.clone(),
        tool_log.clone(),
    )
    .with_telegram_progress(Arc::clone(&progress));
    if let Some(reg) = agent_registry {
        auditor = auditor.with_agent_tracking(Arc::clone(reg), task_id_for_run);
    }
    let stream_tx = spawn_stream_forwarder(ui, target_arc_id.clone());
    // Per-run cancel flag: prefer the one minted by the registry guard
    // so a Telegram-driven run can also be stopped from the desktop
    // Agent Control panel. Falls back to a freshly-allocated flag when
    // the registry isn't wired (early startup).
    let cancel_flag = agent_guard
        .as_ref()
        .map(|g| g.cancel_flag())
        .unwrap_or_else(|| Arc::new(std::sync::atomic::AtomicBool::new(false)));

    // Resolve sampling-temperature override for the active provider here
    // — this Telegram path doesn't carry an `ApprovedTaskCtx`, so the
    // load_config + resolver pair is the simplest way to honor the
    // provider's Advanced setting. Cheap (small TOML, fires per Telegram
    // owner message) and matches the same snapshot-per-task semantics as
    // the in-app and dispatched paths.
    let sampling_temperature = {
        let cfg = crate::state::load_config();
        let active_id = resolve_active_provider(&cfg);
        crate::compaction::resolve_provider_temperature(&cfg, &active_id)
    };

    // Pin the user's enabled HTTP endpoints into the static prefix so
    // the Telegram path's agent ALSO knows what's pre-configured (the
    // ElevenLabs failure was on this exact path).
    let endpoints_block =
        crate::endpoints_render::render_endpoints_block(http_endpoint_store).await;
    // Owner Telegram turns don't go through the risk LLM, so capture
    // is a no-op — but a plan persisted earlier on the same arc still
    // renders into the prompt AND still feeds the completion judge.
    let mission_block = if let Some(id) = target_arc_id.as_ref() {
        crate::mission_render::render_mission_block(arc_store.as_ref(), id).await
    } else {
        None
    };
    let acceptance_criteria = if let Some(id) = target_arc_id.as_ref() {
        crate::mission_render::read_acceptance_criteria(arc_store.as_ref(), id).await
    } else {
        None
    };
    // If this arc already has a Deep Research paper (e.g. one triggered earlier
    // via `/deepresearch`), make the agent aware of it so Telegram follow-ups
    // read the paper instead of answering blind — parity with the in-app path.
    let research_suffix = if let Some(id) = target_arc_id.as_ref() {
        crate::commands::render_research_paper_volatile_block(arc_store.as_ref(), id).await
    } else {
        None
    };

    let mut builder = AgentBuilder::new()
        .llm_router(exec_router)
        .tool_registry(registry)
        .auditor(Box::new(auditor))
        .timeout(Duration::from_secs(300))
        .context_messages(context)
        .stream_sender(stream_tx)
        .cancel_flag(cancel_flag)
        .endpoints_block(endpoints_block)
        .mission_block(mission_block)
        .acceptance_criteria(acceptance_criteria)
        .external_system_suffix(research_suffix)
        .enable_default_reminders(true)
        .default_temperature(sampling_temperature);
    // Per-call shell classifier needs a GrantLookup + arc UUID. Wire
    // them when both the grant store and a target arc id are available
    // (owner-Telegram may run before the arc is allocated, in which
    // case `LowerToSilent` simply never fires — `ForceHumanConfirm`
    // still does, which is the safety-critical path).
    if let (Some(store), Some(arc_str)) = (deps.grant_store.clone(), target_arc_id.as_ref()) {
        builder = builder
            .grant_lookup(Arc::new(crate::file_gate::GrantStoreLookup::new(store)))
            .arc_uuid(crate::file_gate::arc_uuid(arc_str));
    }
    if let Some(p) = deps.tool_doc_dir.as_deref() {
        builder = builder.tool_doc_dir(p.to_path_buf());
    }
    builder = builder
        .toolbox_info(athen_agent::toolbox::ToolboxPromptInfo::load().await)
        .shell_kind(athen_agent::detect_shell_kind().await);
    if !surfaced_images.is_empty() {
        builder = builder.initial_user_images(surfaced_images);
    }
    let executor = match builder.build() {
        Ok(e) => e,
        Err(e) => {
            tracing::error!("Failed to build agent for owner Telegram message: {e}");
            return;
        }
    };

    let task = Task {
        id: task_id_for_run,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        source_event: Some(event_id),
        domain: DomainType::Base,
        description: text.to_string(),
        priority: TaskPriority::Normal,
        status: TaskStatus::InProgress,
        risk_score: None,
        risk_budget: None,
        risk_used: 0,
        assigned_agent: None,
        steps: vec![],
        deadline: None,
    };

    // Emit a progress event so the frontend knows execution started.
    // Skip when we have no arc — the event would land in whatever arc
    // the user is viewing, which is exactly the bug arc_id guards against.
    if let Some(ref arc_id) = target_arc_id {
        ui.emit(
            "agent-progress",
            AgentProgress {
                step: 0,
                tool_name: "Processing Telegram message...".to_string(),
                status: "InProgress".to_string(),
                detail: Some(text.chars().take(200).collect()),
                arc_id: arc_id.clone(),
                args: None,
                result: None,
                error: None,
            },
        );
    }

    // Persist user msg before the executor runs so its DB id sits before any
    // tool_call rows the auditor writes during execution.
    if let (Some(store), Some(ref arc_id)) = (arc_store, &target_arc_id) {
        if let Err(e) = store
            .add_entry(
                arc_id,
                athen_persistence::arcs::EntryType::Message,
                "user",
                text,
                None,
                Some(&turn_id),
            )
            .await
        {
            warn!("Failed to persist owner Telegram user entry: {e}");
        }
    }

    let result = match executor.execute(task).await {
        Ok(r) => r,
        Err(e) => {
            if let Some(g) = agent_guard {
                g.fail(e.to_string()).await;
            }
            let raw = e.to_string();
            tracing::error!("Agent execution failed for owner Telegram message: {raw}");

            // Surface the failure in both places the user can see it:
            // the persisted arc (so the in-app sidebar reflects what
            // happened) and the Telegram chat (so they aren't left
            // wondering why the bot went silent).
            let user_msg = if raw.contains("Timeout") {
                "Sorry, the task took too long and timed out. Try a simpler request or break it into smaller steps."
                    .to_string()
            } else {
                format!(
                    "Sorry, the task failed: {}",
                    crate::commands::simplify_error_public(&raw)
                )
            };

            if let (Some(store), Some(ref arc_id)) = (arc_store, &target_arc_id) {
                if let Err(e) = store
                    .add_entry(
                        arc_id,
                        athen_persistence::arcs::EntryType::Message,
                        "assistant",
                        &user_msg,
                        None,
                        Some(&turn_id),
                    )
                    .await
                {
                    warn!("Failed to persist owner Telegram error reply: {e}");
                }
                if let Err(e) = store.touch_arc(arc_id).await {
                    warn!("Failed to touch arc on error path: {e}");
                }
            }

            // Replace the live status message with the error so the
            // user sees what went wrong in the same place they were
            // watching for progress.
            progress.finalize_with_text(&user_msg).await;
            return;
        }
    };
    if let Some(g) = agent_guard {
        if result.success {
            g.complete().await;
        } else {
            g.fail("agent stopped before finishing").await;
        }
    }

    // Extract the response text (same logic as send_message).
    let content = if !result.success {
        let reason = result
            .output
            .as_ref()
            .and_then(|o| o.get("reason"))
            .and_then(|r| r.as_str())
            .unwrap_or("unknown");
        if reason == "cancelled" {
            "Task cancelled.".to_string()
        } else {
            format!(
                "Ran out of steps ({} used) before finishing.",
                result.steps_completed
            )
        }
    } else {
        let response_text = result
            .output
            .as_ref()
            .and_then(|o| o.get("response"))
            .and_then(|r| r.as_str())
            .unwrap_or("")
            .to_string();
        if response_text.is_empty() {
            result
                .output
                .as_ref()
                .map(|o| serde_json::to_string_pretty(o).unwrap_or_default())
                .unwrap_or_else(|| "Task completed.".to_string())
        } else {
            response_text
        }
    };

    // Persist the assistant response. (User msg was already persisted before
    // the executor ran.)
    if let (Some(store), Some(ref arc_id)) = (arc_store, &target_arc_id) {
        if let Err(e) = store
            .add_entry(
                arc_id,
                athen_persistence::arcs::EntryType::Message,
                "assistant",
                &content,
                None,
                Some(&turn_id),
            )
            .await
        {
            warn!("Failed to persist owner Telegram assistant entry: {e}");
        }
        if let Err(e) = store.touch_arc(arc_id).await {
            warn!("Failed to touch arc: {e}");
        }
    }

    // Notify the frontend so the sidebar refreshes.
    if let Some(ref arc_id) = target_arc_id {
        ui.emit("arc-updated", serde_json::json!({ "arc_id": arc_id }));
    }

    // Replace the live status message with the final response, plus
    // the deduplicated tools footer. Even though the user watched
    // each tool appear live, they may scroll back later (or open the
    // chat fresh on another device) — preserving the footer keeps
    // the trail visible as a single self-contained message.
    let footer = build_telegram_tools_footer(&tool_log);
    let arc_footer = if arc_was_reused {
        // Surface which arc the message landed on + how to escape.
        // Only when an existing arc was reused — fresh arcs are obviously
        // owned by this very turn and would just be noise.
        match (arc_store, target_arc_id.as_ref()) {
            (Some(store), Some(arc_id)) => match store.get_arc(arc_id).await {
                Ok(Some(meta)) => {
                    let reason_label = match arc_match_reason {
                        Some("notification_hint") => "matched recent notification",
                        Some("primary_reply_channel") => "ongoing Telegram thread",
                        Some("recent_messaging") => "recent message fallback",
                        _ => "reused",
                    };
                    Some(format!(
                        "📍 Arc: \"{}\" ({}). Send /newarc to start fresh.",
                        meta.name, reason_label
                    ))
                }
                _ => None,
            },
            _ => None,
        }
    } else {
        None
    };
    let outbound = match (footer.is_empty(), arc_footer) {
        (true, None) => content.clone(),
        (false, None) => format!("{content}\n\n{footer}"),
        (true, Some(af)) => format!("{content}\n\n{af}"),
        (false, Some(af)) => format!("{content}\n\n{footer}\n\n{af}"),
    };
    progress.finalize_with_text(&outbound).await;

    info!(
        "Owner Telegram message executed, response length: {} chars",
        content.len()
    );
}

/// Parse a leading `/newarc` command from an owner Telegram message.
///
/// Returns `(force_new_arc, remaining_text)`. The command must appear as
/// the very first token (whitespace allowed before it); a `/newarc`
/// embedded mid-message is treated as content, not a command — this
/// matches Telegram bot conventions where slash-commands lead the line.
///
/// Trailing text after `/newarc` is preserved as the actual message
/// content, so `/newarc check my email` resets the arc AND sends the
/// follow-up to the agent in the fresh arc.
pub(crate) fn parse_newarc_command(text: &str) -> (bool, String) {
    let trimmed = text.trim_start();
    if let Some(rest) = trimmed.strip_prefix("/newarc") {
        // Require a word boundary after /newarc so `/newarchaeology` (a
        // hypothetical user message) doesn't trigger. Either end-of-input
        // or whitespace counts.
        if rest.is_empty() || rest.starts_with(char::is_whitespace) {
            return (true, rest.trim_start().to_string());
        }
    }
    (false, text.to_string())
}

/// Build a plain-text "Tools used" footer from the tools the agent ran.
/// Returns an empty string when nothing useful happened so the caller can
/// skip appending entirely.
///
/// Tools are de-duplicated in order of first appearance, with a `×N` suffix
/// for repeated invocations, e.g. `Tools used: shell_execute ×3, read`.
pub(crate) fn build_telegram_tools_footer(tool_log: &crate::commands::ToolLog) -> String {
    let names = tool_log.lock_recover().clone();
    if names.is_empty() {
        return String::new();
    }

    // Map raw tool names to their UI labels (e.g. `shell_execute` →
    // `Run`, `list_directory` → `List`) so the footer matches what
    // the user just watched scroll past in the live status message
    // and what the in-app UI shows for the same tool calls.
    let mut order: Vec<String> = Vec::new();
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for name in names {
        let label = crate::telegram_progress::pretty_tool_label(&name);
        if label.is_empty() {
            continue;
        }
        let entry = counts.entry(label.clone()).or_insert(0);
        if *entry == 0 {
            order.push(label);
        }
        *entry += 1;
    }

    let parts: Vec<String> = order
        .iter()
        .map(|n| {
            let count = counts.get(n).copied().unwrap_or(1);
            if count > 1 {
                format!("{n} \u{00d7}{count}")
            } else {
                n.clone()
            }
        })
        .collect();

    format!("— Tools used: {}", parts.join(", "))
}

/// Send a text message to a Telegram chat via the Bot API.
///
/// Delegates to [`athen_sentidos::telegram::send_message`].
pub(crate) async fn send_telegram_reply(
    bot_token: &str,
    chat_id: i64,
    text: &str,
) -> std::result::Result<(), String> {
    athen_sentidos::telegram::send_message(bot_token, chat_id, text).await
}

// ---------------------------------------------------------------------------
// Config loading
// ---------------------------------------------------------------------------

/// Resolve the config directory, trying in order:
/// 1. Athen's per-user data dir (`~/.athen` / `%APPDATA%\Athen`)
/// 2. `./config/` (project-local fallback for development)
///
/// Returns the directory whenever it exists, regardless of whether
/// `config.toml` or `models.toml` is present individually.
/// `load_config_dir` then loads each file independently — a user who has
/// only configured an LLM provider via Settings (writing `models.toml`
/// but never `config.toml`) must still have their provider keys loaded.
/// LLM arc-picker for the owner-Telegram routing fallback. Fires only
/// after tiers 1 (`/newarc`) and 2 (outbound hint) miss but there *are*
/// candidate arcs — handing the model the cross-chat transcript +
/// each candidate's last entry so it can decide whether the new turn
/// continues one of them or starts fresh.
///
/// Returns `Some(arc_id)` only when the LLM picks one of the supplied
/// candidates AND the id round-trips against the store; any other
/// outcome (parse fail, timeout, unknown id, "new") returns `None` so
/// the caller falls through to the existing tier-3/4/create-new logic.
async fn pick_arc_with_llm(
    router: &Arc<RwLock<Arc<DefaultLlmRouter>>>,
    text: &str,
    candidates: &[athen_persistence::arcs::ArcMeta],
    chat_history: &[athen_persistence::telegram_chat_log::TelegramLogEntry],
    store: &ArcStore,
) -> Option<String> {
    use athen_core::llm::{
        ChatMessage as LlmChatMessage, LlmRequest, MessageContent as LlmContent, ModelProfile,
        Role as LlmRole,
    };

    if candidates.is_empty() {
        return None;
    }

    // Render history block.
    let mut history_block = String::new();
    if !chat_history.is_empty() {
        history_block.push_str("\nRecent exchange with this Telegram chat (newest last):\n");
        for entry in chat_history {
            let who = match entry.direction {
                athen_persistence::telegram_chat_log::TelegramLogDirection::Inbound => "them",
                athen_persistence::telegram_chat_log::TelegramLogDirection::Outbound => "us",
            };
            history_block.push_str(&format!("  [{}] {}: {}\n", entry.ts, who, entry.text));
        }
    }

    // Render candidate arcs with their last user/assistant entry (best-effort).
    let mut arcs_block = String::from("\nCandidate arcs to consider:\n");
    for arc in candidates {
        let snippet: String = match store.load_entries(&arc.id).await {
            Ok(entries) => entries
                .into_iter()
                .rev()
                .find(|e| e.entry_type == athen_persistence::arcs::EntryType::Message)
                .map(|e| {
                    let body = e.content;
                    let cap = body.chars().take(200).collect::<String>();
                    cap
                })
                .unwrap_or_default(),
            Err(_) => String::new(),
        };
        arcs_block.push_str(&format!(
            "- id: \"{}\" | name: \"{}\" | last: \"{}\"\n",
            arc.id, arc.name, snippet
        ));
    }

    let prompt = format!(
        r#"You route incoming Telegram messages to the right ongoing conversation thread (arc).

New incoming message from the user:
"{text}"
{history_block}{arcs_block}
Decide whether this new message continues one of the candidate arcs or starts something new. Use the recent exchange to judge continuity — if the user's last reply to us was "yes", "ok", "do it", or a one-word follow-up, they're almost certainly continuing whatever we last said. Otherwise lean on topic match.

Respond with ONLY one of:
- The arc id (just the id, exactly as shown above) if it continues one of them
- The literal word NEW if it's a different topic

No explanation. No markdown."#
    );

    let request = LlmRequest {
        messages: vec![LlmChatMessage {
            role: LlmRole::User,
            content: LlmContent::Text(prompt),
        }],
        profile: ModelProfile::Judges,
        max_tokens: Some(40),
        temperature: Some(0.1),
        tools: None,
        system_prompt: None,
        reasoning_effort: athen_core::llm::ReasoningEffort::default(),
    };

    let llm_router = router.read().await.clone();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        llm_router.route(&request),
    )
    .await;
    let raw = match result {
        Ok(Ok(resp)) => resp.content.trim().to_string(),
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "LLM arc-pick failed; falling through to heuristics");
            return None;
        }
        Err(_) => {
            tracing::warn!("LLM arc-pick timed out; falling through to heuristics");
            return None;
        }
    };

    // Models sometimes wrap the answer in quotes or backticks.
    let stripped = raw
        .trim_matches(|c: char| c == '"' || c == '`' || c == '\'' || c.is_whitespace())
        .to_string();
    if stripped.eq_ignore_ascii_case("new") || stripped.is_empty() {
        return None;
    }
    // Only accept ids that are actually in the candidate set — defense
    // against the model inventing or mangling an id.
    if candidates.iter().any(|c| c.id == stripped) {
        Some(stripped)
    } else {
        tracing::warn!(
            llm_response = %stripped,
            "LLM arc-pick returned id not in candidate set; falling through"
        );
        None
    }
}

fn find_config_dir() -> Option<PathBuf> {
    if let Some(data_dir) = athen_core::paths::athen_data_dir() {
        if data_dir.exists() {
            return Some(data_dir);
        }
    }

    let local_config = PathBuf::from("config");
    if local_config.exists() {
        return Some(local_config);
    }

    None
}

/// Build the web-search provider chain from the user's configured keys.
///
/// Order: Brave (when key set) → Tavily (when key set) → DuckDuckGo (always
/// last, no key, never cools down). Keyed providers enter cooldown when they
/// return rate-limit / quota errors; the wrapper tries the next one and only
/// surfaces an error if every provider in the chain fails.
fn build_web_search_provider(
    config: &athen_core::config::WebSearchConfig,
) -> Arc<dyn WebSearchProvider> {
    let mut slots: Vec<ProviderSlot> = Vec::new();

    let brave_key = config.brave_api_key.trim();
    if !brave_key.is_empty() {
        let provider: Arc<dyn WebSearchProvider> = Arc::new(BraveSearch::new(brave_key));
        slots.push(ProviderSlot::keyed(provider));
    }

    let tavily_key = config.tavily_api_key.trim();
    if !tavily_key.is_empty() {
        let provider: Arc<dyn WebSearchProvider> = Arc::new(TavilySearch::new(tavily_key));
        slots.push(ProviderSlot::keyed(provider));
    }

    // DDG floor — always-available fallback, never cools down.
    let ddg: Arc<dyn WebSearchProvider> = Arc::new(DuckDuckGoSearch::new());
    slots.push(ProviderSlot::floor(ddg));

    info!(
        "Web search chain: brave={}, tavily={}, ddg=floor",
        !brave_key.is_empty(),
        !tavily_key.is_empty()
    );
    Arc::new(MultiSearchProvider::new(slots))
}

/// Build the Telegram outbound sender from `config.telegram`. Returns
/// `None` when the bot token is empty. The owner's chat is wired as the
/// default destination — the override (resolved from the unified
/// contact store) wins over the legacy `owner_user_id` field. For
/// private 1-on-1 chats Telegram's chat_id equals the user's id.
fn build_telegram_sender(
    cfg: &athen_core::config::TelegramConfig,
    owner_chat_id_override: Option<i64>,
) -> Option<Arc<dyn athen_core::traits::telegram_sender::TelegramSender>> {
    if !cfg.enabled || cfg.bot_token.trim().is_empty() {
        tracing::info!("Telegram sender not configured; send_telegram tool will refuse");
        return None;
    }
    let default_chat_id = owner_chat_id_override.or(cfg.owner_user_id);
    match athen_sentidos::telegram_send::BotApiTelegramSender::new(
        cfg.bot_token.clone(),
        default_chat_id,
    ) {
        Ok(sender) => {
            tracing::info!(
                owner_default_chat = ?default_chat_id,
                source = if owner_chat_id_override.is_some() {
                    "contact_store"
                } else if cfg.owner_user_id.is_some() {
                    "legacy_config"
                } else {
                    "none"
                },
                "Telegram sender configured"
            );
            Some(Arc::new(sender))
        }
        Err(e) => {
            tracing::warn!(error = %e, "Failed to build Telegram sender; send_telegram disabled");
            None
        }
    }
}

/// Walk the owner contact's identifiers for a Telegram user id and
/// return it as an `i64`. The contact store stores Telegram identifiers
/// as the numeric `user_id` string; private 1-on-1 chats use that same
/// numeric id for `chat_id` on the Bot API side. Returns `None` when
/// the contact store is absent, no owner is set, the owner has no
/// Telegram identifier, or the stored value isn't a valid `i64`.
async fn resolve_owner_telegram_chat_id(store: Option<&SqliteContactStore>) -> Option<i64> {
    use athen_contacts::ContactStore as _;
    let store = store?;
    let owner = store.find_owner().await.ok().flatten()?;
    owner
        .identifiers
        .iter()
        .find(|i| i.kind == athen_core::contact::IdentifierKind::Telegram)
        .and_then(|i| i.value.trim().parse::<i64>().ok())
}

/// Build the SMTP outbound sender from `config.email`. Returns `None`
/// when SMTP isn't configured — the `email_send` tool then refuses with
/// a clear error rather than silently dropping mail.
fn build_email_sender(
    cfg: &athen_core::config::EmailConfig,
) -> Option<Arc<dyn athen_core::traits::email_sender::EmailSender>> {
    if cfg.smtp_server.trim().is_empty() || cfg.from_address.trim().is_empty() {
        tracing::info!("SMTP sender not configured; email_send tool will refuse");
        return None;
    }
    let settings = athen_sentidos::email_send::SmtpSettings::from_email_config(cfg);
    match athen_sentidos::email_send::LettreSmtpSender::new(settings) {
        Ok(sender) => {
            tracing::info!(
                smtp_server = %cfg.smtp_server,
                smtp_port = cfg.smtp_port,
                smtp_use_tls = cfg.smtp_use_tls,
                "SMTP sender configured"
            );
            Some(Arc::new(sender))
        }
        Err(e) => {
            tracing::warn!(error = %e, "Failed to build SMTP sender; email_send disabled");
            None
        }
    }
}

/// Load configuration from TOML files, falling back to defaults.
pub(crate) fn load_config() -> AthenConfig {
    match find_config_dir() {
        Some(dir) => {
            tracing::debug!("Loading config from: {}", dir.display());
            match config_loader::load_config_dir(&dir) {
                Ok(mut c) => {
                    // load_config_dir already ran the migration, but we
                    // re-run here to surface the report at info level. The
                    // second call is a no-op once the legacy ids are gone.
                    let report = c.migrate_legacy_provider_ids();
                    if report.changed() {
                        if let Some(old) = &report.renamed_active {
                            info!(
                                from = %old,
                                to = "opencode_go",
                                "Migrated active_provider id (legacy → unified opencode_go)"
                            );
                        }
                        if let Some((from, into)) = &report.merged_provider {
                            info!(
                                from = %from,
                                into = %into,
                                "Merged legacy opencode_go_anthropic provider entry into unified opencode_go"
                            );
                        }
                    }
                    c
                }
                Err(e) => {
                    warn!("Error loading config: {e}. Falling back to defaults.");
                    AthenConfig::default()
                }
            }
        }
        None => {
            info!("No config file found, using defaults.");
            AthenConfig::default()
        }
    }
}

// ---------------------------------------------------------------------------
// System initialisation
// ---------------------------------------------------------------------------

/// Resolve Athen's data directory, creating it if needed. Platform-aware:
/// `~/.athen` on Unix, `%APPDATA%\Athen` on Windows.
fn ensure_data_dir() -> Option<PathBuf> {
    let data_dir = match athen_core::paths::athen_data_dir() {
        Some(d) => d,
        None => {
            warn!("Cannot resolve Athen data directory (no home).");
            return None;
        }
    };
    if let Err(e) = std::fs::create_dir_all(&data_dir) {
        warn!(
            "Failed to create data directory {}: {e}",
            data_dir.display()
        );
        return None;
    }
    Some(data_dir)
}

/// Resolved provider target for an arc-scoped LLM call.
///
/// `pinned_slug` is `Some` only when the arc has an active pin AND the
/// pinned provider id is still present in config (so the slug actually
/// belongs to the provider id we're returning). The downstream router
/// builder treats `Some(slug)` as "override every tier with this slug",
/// effectively freezing the model choice for the rest of the arc — see
/// `docs/PROVIDER_PINNING.md` and the "captured but never read" gap
/// it was created to close.
///
/// When the pinned provider has been removed from config and we fall
/// back to the active provider, `pinned_slug` is forced to `None` —
/// sending a slug captured against provider X to provider Y would
/// almost certainly fail at the wire layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EffectiveProviderTarget {
    pub provider_id: String,
    /// Pinned model slug captured on the first LLM call of the arc.
    /// `None` means the caller should consult the provider's live
    /// `tier_models[tier]` (legacy behaviour for unpinned arcs).
    pub pinned_slug: Option<String>,
}

/// Is `slug` still a model the user actually routes to for connection
/// `connection_id`?
///
/// This is the guard against a *stale slug pin*: a pin freezes the
/// concrete slug that resolved on a task's first LLM call, but the model
/// the user routes to can change underneath it. When this returns false
/// the resolver drops the slug pin and falls back to live tier
/// resolution — the fix for "my wake-up still uses minimax even though
/// my Bundle only has deepseek".
///
/// Authority follows the same precedence as routing itself:
/// - **Active Bundle set** → Bundles are the single source of truth. A
///   slug is live *only* if some Bundle tier points this connection at
///   it. The Connection's legacy `default_model` / `tier_models` fields
///   are deliberately ignored: a model sitting in `default_model` but in
///   no Bundle is exactly the leak the user hit, and honouring it would
///   re-introduce the bug.
/// - **No active Bundle** (legacy / pre-Bundles config) → fall back to
///   the Connection's own `default_model` + `tier_models`, since that's
///   what routing uses in that mode.
fn pinned_slug_still_configured(cfg: &AthenConfig, connection_id: &str, slug: &str) -> bool {
    let has_active_bundle = cfg
        .models
        .assignments
        .get(ACTIVE_BUNDLE_KEY)
        .and_then(|id| cfg.models.bundles.get(id))
        .is_some();
    if has_active_bundle {
        return cfg.models.bundles.values().any(|b| {
            b.tiers
                .values()
                .any(|t| t.connection_id == connection_id && t.slug == slug)
        });
    }
    cfg.models
        .providers
        .get(connection_id)
        .is_some_and(|p| p.default_model == slug || p.tier_models.values().any(|s| s == slug))
}

/// Resolve the effective provider id (and pinned slug) for an arc-scoped
/// LLM call, honouring an existing pin or installing one if none is
/// present.
///
/// First-call-wins semantics: when the arc has no pin yet, the current
/// active provider id is snapshotted onto the arc row alongside the
/// resolved slug for the supplied `tier`. Subsequent calls against the
/// same arc read both values back, isolating the arc's in-flight task
/// from a mid-flight active-provider switch *and* from a tier_models
/// edit on the same provider (see `docs/PROVIDER_PINNING.md`).
///
/// If the pinned provider has been removed from config the function
/// logs a warning and returns the current active id with `pinned_slug:
/// None` — recoverability over purity, and we refuse to send a foreign
/// slug to a different provider.
pub(crate) async fn resolve_effective_provider_for_arc(
    arc_store: Option<&ArcStore>,
    arc_id: &str,
    active_provider_id: &str,
    tier: ModelProfile,
) -> EffectiveProviderTarget {
    let cfg = load_config();
    resolve_effective_provider_for_arc_with_config(
        arc_store,
        arc_id,
        active_provider_id,
        tier,
        &cfg,
    )
    .await
}

/// Test-friendly variant of `resolve_effective_provider_for_arc` that
/// takes the config explicitly instead of reading it from disk. The
/// public wrapper above just loads the config and delegates here.
pub(crate) async fn resolve_effective_provider_for_arc_with_config(
    arc_store: Option<&ArcStore>,
    arc_id: &str,
    active_provider_id: &str,
    tier: ModelProfile,
    cfg: &AthenConfig,
) -> EffectiveProviderTarget {
    let Some(store) = arc_store else {
        return EffectiveProviderTarget {
            provider_id: active_provider_id.to_string(),
            pinned_slug: None,
        };
    };
    let arc = match store.get_arc(arc_id).await {
        Ok(Some(arc)) => arc,
        Ok(None) => {
            return EffectiveProviderTarget {
                provider_id: active_provider_id.to_string(),
                pinned_slug: None,
            }
        }
        Err(e) => {
            warn!(arc_id = %arc_id, error = %e, "pin lookup failed; using active provider");
            return EffectiveProviderTarget {
                provider_id: active_provider_id.to_string(),
                pinned_slug: None,
            };
        }
    };
    if let Some(pinned) = arc.pinned_provider_id.as_deref() {
        if cfg.models.providers.contains_key(pinned) {
            // Slug pin only flows through when it's paired with the
            // provider we're actually returning — sending an
            // OpenAI-captured slug to an Anthropic adapter would 4xx
            // immediately, and a missing slug column simply collapses
            // back to the provider's live tier_models map.
            //
            // It must ALSO still be a model the user has configured: a
            // pin freezes the concrete slug from a task's first call,
            // but the user can later delete that model from every
            // Bundle while keeping the connection. Honouring a dead
            // slug is what made wake-ups keep reaching for a model no
            // longer in Bundles — drop it and fall back to the
            // connection's live tier resolution instead.
            let pinned_slug = arc
                .pinned_slug
                .clone()
                .filter(|s| !s.is_empty())
                .filter(|s| {
                    let live = pinned_slug_still_configured(cfg, pinned, s);
                    if !live {
                        warn!(
                            arc_id = %arc_id,
                            pinned_provider_id = %pinned,
                            stale_slug = %s,
                            "pinned model no longer configured for connection; dropping slug pin and falling back to live tier resolution"
                        );
                    }
                    live
                });
            return EffectiveProviderTarget {
                provider_id: pinned.to_string(),
                pinned_slug,
            };
        }
        warn!(
            arc_id = %arc_id,
            pinned_provider_id = %pinned,
            "pinned provider missing from config; falling back to active (slug pin dropped)"
        );
        return EffectiveProviderTarget {
            provider_id: active_provider_id.to_string(),
            pinned_slug: None,
        };
    }
    // Unpinned path. Try the active Bundle first (the world Bundles
    // unlocks: cross-provider per-tier mixing) — fall back to the
    // legacy `active_provider + tier_models` shape when no Bundle is
    // set or the picked connection has vanished from config.
    let (provider_id, slug) = resolve_unpinned_target(cfg, active_provider_id, tier);
    if let Err(e) = store
        .set_pinned_provider_if_unset(arc_id, &provider_id, &slug)
        .await
    {
        warn!(arc_id = %arc_id, error = %e, "failed to install provider pin");
    }
    // First call: we just installed the pin atomically above. From the
    // caller's perspective we behave like an unpinned resolve — the
    // caller will use the freshly built router whose tiers map to
    // exactly the same slugs we just persisted, so honouring the slug
    // here would be a no-op. Subsequent calls take the
    // `pinned_provider_id.is_some()` branch above and surface the slug.
    EffectiveProviderTarget {
        provider_id,
        pinned_slug: None,
    }
}

/// Pick the `(provider_id, slug)` pair to pin onto an arc on its first
/// LLM call, honouring the active Bundle when present.
///
/// Resolution order:
/// 1. **Active Bundle** (`models.assignments["active_bundle"]`): look up
///    the tier in the bundle, applying the sparse-tier fallback ladder
///    (Code→Fast→Cheap, Powerful→Fast→Cheap, Fast→Cheap). The bundle's
///    `connection_id` must still exist in `models.providers` — if it
///    doesn't (user deleted the Connection without picking a different
///    active Bundle), we fall through with a warning.
/// 2. **Legacy `active_provider + tier_models`**: today's behaviour for
///    users who haven't gone through the Bundles migration, or whose
///    migration was skipped (no active provider set). Reads
///    `provider.tier_models[tier]` and falls back to `default_model`.
///
/// Both branches return *something*: an empty-string slug if both
/// branches fail to resolve. That mirrors the pre-Bundles behaviour and
/// lets the wire layer surface the actual misconfiguration on the next
/// call instead of panicking here.
fn resolve_unpinned_target(
    cfg: &AthenConfig,
    active_provider_id: &str,
    tier: ModelProfile,
) -> (String, String) {
    if let Some(pick) = resolve_from_active_bundle(cfg, tier) {
        return pick;
    }
    // Legacy fallback.
    let slug = cfg
        .models
        .providers
        .get(active_provider_id)
        .map(|p| {
            p.tier_models
                .get(&tier)
                .filter(|s| !s.is_empty())
                .cloned()
                .unwrap_or_else(|| p.default_model.clone())
        })
        .unwrap_or_default();
    (active_provider_id.to_string(), slug)
}

/// Returns the `(connection_id, slug)` pick for `tier` from the active
/// Bundle, applying the sparse-tier fallback ladder. `None` when no
/// active Bundle is set, the active Bundle id doesn't resolve, no tier
/// in the ladder is filled, or the picked connection has been deleted.
fn resolve_from_active_bundle(cfg: &AthenConfig, tier: ModelProfile) -> Option<(String, String)> {
    let bundle_id = cfg.models.assignments.get(ACTIVE_BUNDLE_KEY)?;
    let bundle = cfg.models.bundles.get(bundle_id)?;
    let (connection_id, slug) = pick_bundle_tier(bundle, tier)?;
    if !cfg.models.providers.contains_key(&connection_id) {
        warn!(
            connection_id = %connection_id,
            bundle = %bundle.name,
            "active Bundle references a deleted Connection; falling back to legacy active_provider path"
        );
        return None;
    }
    Some((connection_id, slug))
}

/// Sparse-tier fallback ladder per `docs/BUNDLES.md`. A Bundle that only
/// sets Cheap is a valid config — every other tier collapses onto it.
/// Local stays isolated (no cross-tier fallback) because Local routing
/// is meaningfully different from cloud tiers.
///
/// For cloud tiers there is a final catch-all: if neither the strict
/// ladder nor a sensible cross-tier order resolves, fall back to *any*
/// cloud tier the Bundle does fill. This guarantees that as long as a
/// Bundle has one cloud tier set, every cloud-tier request resolves to a
/// model **from the Bundle** — never silently dropping through to the
/// Connection's legacy `default_model` (the field that leaked
/// `minimax-m2.7` into a deepseek-only Bundle). The Connection model
/// field is bootstrap/test-only now; the active Bundle is authoritative.
fn pick_bundle_tier(bundle: &Bundle, tier: ModelProfile) -> Option<(String, String)> {
    // Local is deliberately isolated: never borrow a cloud tier for it,
    // and never let it satisfy a cloud request.
    if tier == ModelProfile::Local {
        return bundle
            .tiers
            .get(&ModelProfile::Local)
            .map(|bt| (bt.connection_id.clone(), bt.slug.clone()));
    }
    let ladder: &[ModelProfile] = match tier {
        ModelProfile::Code => &[ModelProfile::Code, ModelProfile::Fast, ModelProfile::Judges],
        ModelProfile::Powerful => &[
            ModelProfile::Powerful,
            ModelProfile::Fast,
            ModelProfile::Judges,
        ],
        ModelProfile::Fast => &[ModelProfile::Fast, ModelProfile::Judges],
        ModelProfile::Judges => &[ModelProfile::Judges],
        ModelProfile::Local => unreachable!("Local handled above"),
    };
    for t in ladder {
        if let Some(bt) = bundle.tiers.get(t) {
            return Some((bt.connection_id.clone(), bt.slug.clone()));
        }
    }
    // Catch-all: any cloud tier the Bundle fills, in a sensible order, so
    // a request for an unfilled tier still resolves inside the Bundle
    // instead of falling back to the Connection default_model.
    for t in [
        ModelProfile::Powerful,
        ModelProfile::Fast,
        ModelProfile::Code,
        ModelProfile::Judges,
    ] {
        if let Some(bt) = bundle.tiers.get(&t) {
            return Some((bt.connection_id.clone(), bt.slug.clone()));
        }
    }
    None
}

/// Clear an arc's provider pin. Best-effort: logs and swallows errors
/// since pin clearing is a hygiene step, not load-bearing for the task
/// that just finished.
pub(crate) async fn clear_provider_pin_for_arc(arc_store: Option<&ArcStore>, arc_id: &str) {
    if let Some(store) = arc_store {
        if let Err(e) = store.clear_pinned_provider(arc_id).await {
            warn!(arc_id = %arc_id, error = %e, "failed to clear provider pin");
        }
    }
}

/// Resolve the effective `ReasoningEffort` for an arc-scoped LLM call.
///
/// Reads the arc's `reasoning_effort_override` column (a user-set
/// durable preference) and parses it. Missing arc, missing override,
/// parse failure, or store error all fall through to
/// `ReasoningEffort::Default` — which providers map to "omit the field
/// on the wire" so behaviour matches today's call paths for any arc
/// that hasn't opted in. Per-tier defaults from Settings are a
/// follow-up; not consulted here.
pub(crate) async fn resolve_reasoning_effort_for_arc(
    arc_store: Option<&ArcStore>,
    arc_id: &str,
) -> athen_core::llm::ReasoningEffort {
    use athen_core::llm::ReasoningEffort;
    use std::str::FromStr;
    let Some(store) = arc_store else {
        return ReasoningEffort::Default;
    };
    match store.get_arc(arc_id).await {
        Ok(Some(arc)) => arc
            .reasoning_effort_override
            .as_deref()
            .and_then(|s| ReasoningEffort::from_str(s).ok())
            .unwrap_or(ReasoningEffort::Default),
        _ => ReasoningEffort::Default,
    }
}

/// Resolve the effective `SecurityMode` for an arc: the per-arc
/// `security_mode_override` wins over the passed live `global_default`
/// (snapshot of `AppState::security.load().mode`). Missing arc / missing
/// override / parse failure / store error all fall through to the global.
/// Resolve this once at task/arc creation (new-arcs-only contract).
pub(crate) async fn resolve_security_mode_for_arc(
    arc_store: Option<&ArcStore>,
    arc_id: &str,
    global_default: athen_core::config::SecurityMode,
) -> athen_core::config::SecurityMode {
    use std::str::FromStr;
    let Some(store) = arc_store else {
        return global_default;
    };
    match store.get_arc(arc_id).await {
        Ok(Some(arc)) => arc
            .security_mode_override
            .as_deref()
            .and_then(|s| athen_core::config::SecurityMode::from_str(s).ok())
            .unwrap_or(global_default),
        _ => global_default,
    }
}

/// Resolve the active Code-Mode repo root for `arc_id`, or `None` when the arc
/// is not an active Code-Mode session. Active iff code_mode == Some(true) AND
/// code_mode_root points at an existing directory. Returns `None` on any miss /
/// store error / missing root — never errors. The root anchors the shell cwd,
/// file-tool resolution, and sandbox allow-list; the shadow checkpoint store
/// stays active in Code Mode (per-action undo) — see CODE_MODE.md §6 (b).
pub(crate) async fn resolve_code_mode_for_arc(
    arc_store: Option<&ArcStore>,
    arc_id: &str,
) -> Option<std::path::PathBuf> {
    let store = arc_store?;
    let meta = store.get_arc(arc_id).await.ok().flatten()?;
    if meta.code_mode != Some(true) {
        return None;
    }
    match meta.code_mode_root {
        Some(root) => {
            let p = std::path::PathBuf::from(&root);
            if p.is_dir() {
                Some(p)
            } else {
                None
            }
        }
        None => None,
    }
}

/// Parse the wire form (`ModelProfile` variant name) of a stored tier
/// override into a `ModelProfile`. Returns `None` for unknown strings so
/// the resolver can warn and fall through rather than silently
/// substituting a wrong tier.
fn parse_model_profile_wire(s: &str) -> Option<ModelProfile> {
    match s.trim() {
        "Powerful" => Some(ModelProfile::Powerful),
        "Fast" => Some(ModelProfile::Fast),
        "Code" => Some(ModelProfile::Code),
        "Judges" => Some(ModelProfile::Judges),
        // Legacy wire string from before the Cheap→Judges rename.
        "Cheap" => Some(ModelProfile::Judges),
        "Local" => Some(ModelProfile::Local),
        _ => None,
    }
}

/// Resolve the effective `ModelProfile` for the main-executor call of a
/// task. Resolution order (highest first):
/// 1. Arc's `tier_override` (user said "force this tier for this arc")
/// 2. Task's `risk_score` signals (`complexity` + `is_code_task`) mapped
///    via `athen_core::risk::resolve_tier_from_signals`. The `Code` tier
///    wins for Low/Medium coding tasks; `High` complexity still routes to
///    `Powerful` regardless of the code flag.
/// 3. Caller-supplied `default_label` (the static per-call-site tier)
///
/// Only the main executor should call this — the memory extractor,
/// completion judge, and risk LLM itself keep their static tier labels
/// (Cheap / Cheap / Fast respectively) regardless of task complexity.
/// Logs once at info-level when the chosen tier diverges from the
/// caller's default so users can see why their task ran on a different
/// model.
pub(crate) async fn resolve_effective_tier_for_arc(
    arc_store: Option<&ArcStore>,
    arc_id: &str,
    task_complexity: Option<athen_core::risk::ComplexityTag>,
    task_is_code: bool,
    default_label: ModelProfile,
) -> ModelProfile {
    let (chosen, reason): (ModelProfile, &'static str) = if let Some(store) = arc_store {
        match store.get_arc(arc_id).await {
            Ok(Some(arc)) => match arc.tier_override.as_deref() {
                Some(raw) => match parse_model_profile_wire(raw) {
                    Some(t) => (t, "override"),
                    None => {
                        warn!(
                            arc_id = %arc_id,
                            tier_override = %raw,
                            "unknown tier_override wire value; ignoring"
                        );
                        complexity_path(task_complexity, task_is_code, default_label)
                    }
                },
                None => complexity_path(task_complexity, task_is_code, default_label),
            },
            _ => complexity_path(task_complexity, task_is_code, default_label),
        }
    } else {
        complexity_path(task_complexity, task_is_code, default_label)
    };

    if chosen != default_label {
        tracing::info!(
            arc_id = %arc_id,
            chosen = ?chosen,
            default = ?default_label,
            reason = %reason,
            "executor: tier diverges from static label"
        );
    }
    chosen
}

/// Sub-resolver for the signal-then-default fallthrough. Pulled out so
/// the override branch above stays readable — the reason string is what
/// differs between the override path and this one. Reason strings are
/// chosen so logs cleanly distinguish "complexity_*" vs "code_task" vs
/// "default" — useful when debugging why a tier diverged.
fn complexity_path(
    task_complexity: Option<athen_core::risk::ComplexityTag>,
    task_is_code: bool,
    default_label: ModelProfile,
) -> (ModelProfile, &'static str) {
    let chosen =
        athen_core::risk::resolve_tier_from_signals(task_complexity, task_is_code, default_label);
    let reason = match (task_complexity, task_is_code) {
        (Some(athen_core::risk::ComplexityTag::High), _) => "complexity_high",
        (_, true) => "code_task",
        (Some(athen_core::risk::ComplexityTag::Low), false) => "complexity_low",
        (Some(athen_core::risk::ComplexityTag::Medium), false) => "complexity_medium",
        (None, false) => "default",
    };
    (chosen, reason)
}

/// Determine the active provider ID from config, falling back to "deepseek".
///
/// Looks for `active_provider` in `config.models.assignments` (we reuse the
/// existing assignments map with a special key), or defaults to "deepseek".
pub(crate) fn resolve_active_provider(config: &AthenConfig) -> String {
    config
        .models
        .assignments
        .get("active_provider")
        .filter(|s| !s.is_empty())
        .cloned()
        .unwrap_or_else(|| "deepseek".to_string())
}

/// Build a router for the given provider ID, reading configuration from
/// the supplied `AthenConfig`.  Returns `(Arc<DefaultLlmRouter>, model_name)`.
///
/// Thin wrapper around `build_router_for_provider_from_config_with_pinned_slug`
/// with `pinned_slug = None`. Kept for the bootstrap / non-arc paths
/// (e.g. risk LLM, compaction LLM) that have no notion of an arc and
/// should always honour the live `tier_models` map.
fn build_router_for_provider_from_config(
    provider_id: &str,
    config: &AthenConfig,
) -> (Arc<DefaultLlmRouter>, String) {
    build_router_for_provider_from_config_with_pinned_slug(provider_id, config, None)
}

/// Build a router for the given provider ID, optionally overriding the
/// per-tier slug for arcs whose pin is in force. Returns
/// `(Arc<DefaultLlmRouter>, model_name)`.
///
/// When `pinned_slug` is `Some(s)`, every tier (Cheap / Fast / Code /
/// Powerful) is wired to a single provider instance built for slug `s`,
/// regardless of what `tier_models` says. This is the load-bearing
/// behaviour for arc pinning: once an arc has captured `(provider, slug)`
/// on its first LLM call, a subsequent user edit to `tier_models` on the
/// same provider must not redirect the in-flight task to a different
/// model. See `docs/PROVIDER_PINNING.md`.
///
/// `model_name` in the returned tuple reflects what the router will
/// actually use as its primary slug — the pinned slug when set,
/// otherwise the provider's `default_model`.
fn build_router_for_provider_from_config_with_pinned_slug(
    provider_id: &str,
    config: &AthenConfig,
    pinned_slug: Option<&str>,
) -> (Arc<DefaultLlmRouter>, String) {
    let provider_cfg = config.models.providers.get(provider_id);

    let base_url = provider_cfg
        .and_then(|c| c.endpoint.as_deref())
        .unwrap_or_else(|| default_base_url_for(provider_id))
        .to_string();

    let model = provider_cfg
        .map(|c| c.default_model.as_str())
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| default_model_for(provider_id))
        .to_string();

    // Resolve API key: env var first, then config.
    let api_key = resolve_api_key_for(provider_id, provider_cfg);

    let supports_vision = provider_cfg.is_some_and(|c| c.supports_vision);
    let supports_documents = provider_cfg.is_some_and(|c| c.supports_documents);
    let family = provider_cfg
        .map(|c| c.family)
        .unwrap_or(athen_core::llm::ModelFamily::Default);
    let empty_tier_models = HashMap::new();
    let tier_models = provider_cfg
        .map(|c| &c.tier_models)
        .unwrap_or(&empty_tier_models);

    let router = build_router_for_provider(
        provider_id,
        &base_url,
        &model,
        api_key.as_deref(),
        supports_vision,
        supports_documents,
        family,
        tier_models,
        pinned_slug,
    );
    let primary_model = pinned_slug
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or(model);
    (router, primary_model)
}

/// Resolve the router an arc-bound execution should hand to
/// `ApprovedTaskCtx.router`. Returns the shared global router when the
/// arc has no pin (or pins to the current active provider with no slug
/// captured — degenerate case), and a freshly built per-arc router
/// otherwise.
///
/// The arc-specific router is wrapped in its own `Arc<RwLock<_>>` so it
/// matches the existing context field type without forcing the caller to
/// know which branch we took. The freshly built variant lives only for
/// the duration of the executor run; once the task completes and
/// `clear_provider_pin_for_arc` fires, the next dispatch on the same arc
/// resolves back to the global router via the no-pin branch.
pub(crate) async fn arc_router_for(
    global_router: &Arc<tokio::sync::RwLock<Arc<DefaultLlmRouter>>>,
    target: &EffectiveProviderTarget,
    active_provider_id: &str,
    config: &AthenConfig,
    vault: Option<&Arc<dyn athen_core::traits::vault::Vault>>,
) -> Arc<tokio::sync::RwLock<Arc<DefaultLlmRouter>>> {
    // No pin in force AND no provider switch ⇒ keep using the shared
    // global router. This is the fast path on the very first call of an
    // arc (resolver returns `pinned_slug: None` immediately after
    // installing the pin — see `resolve_effective_provider_for_arc`)
    // and on every call of an arc that was never pinned. The global
    // router is built with vault-hydrated credentials, so no vault touch
    // here.
    if target.pinned_slug.is_none() && target.provider_id == active_provider_id {
        return Arc::clone(global_router);
    }
    // Per-arc (slow) path. `config` here comes from `load_config()`, which
    // is NOT vault-hydrated — vault-backed providers carry `auth = None`.
    // Hydrate the single provider we're about to build for, otherwise the
    // rebuilt router is keyless and every call fails with "Missing API
    // key" (the global router above does not have this problem because it
    // was built from hydrated providers). See `docs/PROVIDER_PINNING.md`.
    let (router, _model) = if vault.is_some() {
        let mut cfg = config.clone();
        crate::vault_creds::hydrate_one_provider_from_vault(
            vault,
            &mut cfg.models,
            &target.provider_id,
        )
        .await;
        build_router_for_provider_from_config_with_pinned_slug(
            &target.provider_id,
            &cfg,
            target.pinned_slug.as_deref(),
        )
    } else {
        build_router_for_provider_from_config_with_pinned_slug(
            &target.provider_id,
            config,
            target.pinned_slug.as_deref(),
        )
    };
    Arc::new(tokio::sync::RwLock::new(router))
}

/// Shared, hot-swappable embedder cell whose inner provider is replaced
/// atomically by `reload_embedder` / `start_embedder_warmup`. Every holder of a
/// clone sees the new provider on its next call — no rebuild, no restart.
pub(crate) type EmbedderCell =
    Arc<std::sync::RwLock<Arc<dyn athen_core::traits::embedding::EmbeddingProvider>>>;

/// `EmbeddingProvider` view over an [`EmbedderCell`]. The memory store holds one
/// of these (boxed) while the profile path holds the cell directly, so a single
/// swap of the cell updates both consumers at once. Reads clone the current
/// provider `Arc` out from under the lock before awaiting, so the (sync) lock is
/// never held across an `.await`.
pub(crate) struct SwappableEmbedder {
    cell: EmbedderCell,
}

impl SwappableEmbedder {
    pub(crate) fn new(cell: EmbedderCell) -> Self {
        Self { cell }
    }

    fn current(&self) -> Arc<dyn athen_core::traits::embedding::EmbeddingProvider> {
        self.cell.read_recover().clone()
    }
}

#[async_trait::async_trait]
impl athen_core::traits::embedding::EmbeddingProvider for SwappableEmbedder {
    fn provider_id(&self) -> &str {
        "swappable"
    }

    fn dimensions(&self) -> usize {
        self.current().dimensions()
    }

    async fn embed(&self, text: &str) -> athen_core::error::Result<Vec<f32>> {
        let provider = self.current();
        provider.embed(text).await
    }

    async fn embed_batch(&self, texts: &[String]) -> athen_core::error::Result<Vec<Vec<f32>>> {
        let provider = self.current();
        provider.embed_batch(texts).await
    }

    async fn is_available(&self) -> bool {
        let provider = self.current();
        provider.is_available().await
    }
}

/// Build an `EmbeddingRouter` from `config.embeddings`.
///
/// Mode behaviour:
/// - `Off` → empty router, keyword fallback only.
/// - `LocalOnly` → Ollama provider (default model `nomic-embed-text`,
///   default host `http://localhost:11434`).
/// - `Cloud` → OpenAI-compatible provider with the configured api_key,
///   defaulting to OpenAI's endpoint and `text-embedding-3-small` if
///   the user didn't override `base_url` / `model`.
/// - `Specific` → uses `cfg.provider` to pick (`"ollama"` →
///   OllamaEmbedding, anything else → OpenAiEmbedding::compatible).
/// - `Automatic` → Ollama (if running) → cloud (if api_key) → bundled
///   builtin (if its weights are on disk) → keyword. The disk-gated
///   builtin push is what `start_embedder_warmup` populates in the
///   background so memory works out of the box.
fn build_embedding_router(
    cfg: &athen_core::config::EmbeddingConfig,
) -> athen_llm::embeddings::router::EmbeddingRouter {
    use athen_core::config::EmbeddingMode;
    use athen_core::traits::embedding::EmbeddingProvider;
    use athen_llm::embeddings::ollama::OllamaEmbedding;
    use athen_llm::embeddings::openai::OpenAiEmbedding;
    use athen_llm::embeddings::router::EmbeddingRouter;

    let mut providers: Vec<Box<dyn EmbeddingProvider>> = Vec::new();

    let model_or = |default: &str| -> String {
        cfg.model
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| default.to_string())
    };

    match cfg.mode {
        EmbeddingMode::Off => {
            info!("Embeddings: mode=Off, keyword fallback only");
        }
        EmbeddingMode::Bundled { tier } => {
            // Explicit user choice — make this the SOLE provider so
            // we don't silently fall through to Ollama/OpenAI.
            #[cfg(feature = "bundled-embeddings")]
            {
                if let Some(data_dir) = ensure_data_dir() {
                    let cache_dir = data_dir.join("embeddings");
                    info!(
                        cache_dir = %cache_dir.display(),
                        tier = ?tier,
                        "Embeddings: Bundled fastembed (user-selected tier)"
                    );
                    providers.push(Box::new(
                        athen_llm::embeddings::bundled::BundledEmbedding::new(cache_dir, tier),
                    ));
                } else {
                    warn!(
                        "EmbeddingMode::Bundled selected but no data_dir available; falling back to keyword"
                    );
                }
            }
            #[cfg(not(feature = "bundled-embeddings"))]
            {
                let _ = tier;
                warn!(
                    "EmbeddingMode::Bundled selected but the bundled-embeddings cargo feature is OFF; falling back to keyword"
                );
            }
        }
        EmbeddingMode::LocalOnly => {
            let model = model_or("nomic-embed-text");
            let mut p = OllamaEmbedding::new(&model);
            if let Some(url) = cfg.base_url.as_deref().filter(|s| !s.is_empty()) {
                p = p.with_base_url(url);
            }
            info!(model = %model, "Embeddings: LocalOnly via Ollama");
            providers.push(Box::new(p));
        }
        EmbeddingMode::Cloud => {
            let key = cfg.api_key.as_deref().unwrap_or("");
            if key.is_empty() {
                warn!("Embeddings: Cloud mode but no api_key set; falling back to keyword");
            } else {
                let mut p = match cfg.base_url.as_deref().filter(|s| !s.is_empty()) {
                    Some(url) => OpenAiEmbedding::compatible(url).with_api_key(key),
                    None => OpenAiEmbedding::openai(key),
                };
                if let Some(m) = cfg.model.as_deref().filter(|s| !s.is_empty()) {
                    p = p.with_model(m);
                }
                info!("Embeddings: Cloud via OpenAI-compatible");
                providers.push(Box::new(p));
            }
        }
        EmbeddingMode::Specific => match cfg.provider.as_deref() {
            Some("ollama") => {
                let model = model_or("nomic-embed-text");
                let mut p = OllamaEmbedding::new(&model);
                if let Some(url) = cfg.base_url.as_deref().filter(|s| !s.is_empty()) {
                    p = p.with_base_url(url);
                }
                info!(model = %model, "Embeddings: Specific=ollama");
                providers.push(Box::new(p));
            }
            Some(other) => {
                let url = cfg
                    .base_url
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .unwrap_or("http://localhost:8080");
                let mut p = OpenAiEmbedding::compatible(url).with_provider_id(other);
                if let Some(m) = cfg.model.as_deref().filter(|s| !s.is_empty()) {
                    p = p.with_model(m);
                }
                if let Some(k) = cfg.api_key.as_deref().filter(|s| !s.is_empty()) {
                    p = p.with_api_key(k);
                }
                info!(provider = %other, base_url = %url, "Embeddings: Specific OpenAI-compatible");
                providers.push(Box::new(p));
            }
            None => {
                warn!("Embeddings: Specific mode but no provider id; falling back to keyword");
            }
        },
        EmbeddingMode::Automatic => {
            // Resolution order: Ollama (if running) → cloud (if an api_key
            // is set) → bundled builtin (if its weights are already on
            // disk) → keyword. The builtin arm is what makes memory "just
            // work" for users who configure nothing: `start_embedder_warmup`
            // downloads the Light tier in the background at startup, and
            // once it lands this push has something to find (after a
            // `reload_embedder`). Keyword stays the genuine last resort —
            // it only wins when there's no Ollama, no key, and no
            // downloaded model (e.g. a first run that's still offline).

            // Try Ollama at the default endpoint first. is_available()
            // pings /api/tags and returns false fast when nothing's
            // listening, so this is cheap. Cloud requires explicit
            // api_key so we don't surprise users with outbound calls.
            let model = model_or("nomic-embed-text");
            let ollama = OllamaEmbedding::new(&model);
            providers.push(Box::new(ollama));
            if let Some(key) = cfg.api_key.as_deref().filter(|s| !s.is_empty()) {
                let mut p = match cfg.base_url.as_deref().filter(|s| !s.is_empty()) {
                    Some(url) => OpenAiEmbedding::compatible(url).with_api_key(key),
                    None => OpenAiEmbedding::openai(key),
                };
                if let Some(m) = cfg.model.as_deref().filter(|s| !s.is_empty()) {
                    p = p.with_model(m);
                }
                providers.push(Box::new(p));
            }
            // Bundled builtin fallback. Only pushed when the weights are
            // already cached — BundledEmbedding::is_available() is always
            // true, so pushing it before download would make the router
            // pick it and then error offline instead of degrading to
            // keyword. Disk-gating keeps keyword as the no-connection floor.
            #[cfg(feature = "bundled-embeddings")]
            {
                if let Some(data_dir) = ensure_data_dir() {
                    let cache_dir = data_dir.join("embeddings");
                    let tier = athen_core::config::BundledTier::Light;
                    if crate::bundled_embeddings::is_tier_downloaded(&cache_dir, tier) {
                        info!(
                            tier = ?tier,
                            "Embeddings: Automatic — bundled builtin available as fallback"
                        );
                        providers.push(Box::new(
                            athen_llm::embeddings::bundled::BundledEmbedding::new(cache_dir, tier),
                        ));
                    }
                }
            }
            info!(
                provider_count = providers.len(),
                "Embeddings: Automatic — Ollama → cloud/builtin → keyword"
            );
        }
    }

    EmbeddingRouter::new(providers)
}

/// Default base URL for known provider IDs.
fn default_base_url_for(id: &str) -> &str {
    match id {
        "deepseek" => "https://api.deepseek.com",
        "openai" => "https://api.openai.com",
        "anthropic" => "https://api.anthropic.com",
        "google" => "https://generativelanguage.googleapis.com",
        "opencode_go" => "https://opencode.ai/zen/go",
        "minimax" => "https://api.minimax.io",
        "minimax_anthropic" => "https://api.minimax.io/anthropic",
        "kimi" => "https://api.moonshot.ai",
        "kimi_code" => "https://api.kimi.com/coding",
        "ollama" => "http://localhost:11434",
        "llamacpp" => "http://localhost:8080",
        _ => "http://localhost:8080",
    }
}

/// Default model for known provider IDs.
fn default_model_for(id: &str) -> &str {
    // Keep aligned with `settings::default_model` — both must return the
    // same slug or the bootstrap path will route a stale model.
    match id {
        "deepseek" => "deepseek-v4-flash",
        "openai" => "gpt-5.4-mini",
        "anthropic" => "claude-sonnet-4-6",
        "google" => "gemini-3.1-flash-lite-preview",
        "opencode_go" => "deepseek-v4-flash",
        "minimax" | "minimax_anthropic" => "MiniMax-M2.7",
        "kimi" => "kimi-k2.7-code",
        "kimi_code" => "kimi-for-coding",
        "ollama" => "llama3",
        "llamacpp" => "default",
        _ => "default",
    }
}

/// Resolve an API key for a provider, checking environment variables first,
/// then the config file value.
fn resolve_api_key_for(
    provider_id: &str,
    provider_cfg: Option<&athen_core::config::ProviderConfig>,
) -> Option<String> {
    // Config file takes priority — the user explicitly saved this key via Settings.
    if let Some(key) = provider_cfg.and_then(|c| match &c.auth {
        AuthType::ApiKey(key) if !key.is_empty() && !key.starts_with("${") => Some(key.clone()),
        _ => None,
    }) {
        return Some(key);
    }

    // Fall back to environment variable (e.g. DEEPSEEK_API_KEY, OPENAI_API_KEY).
    let env_var = format!("{}_API_KEY", provider_id.to_uppercase());
    if let Ok(key) = std::env::var(&env_var) {
        if !key.is_empty() {
            return Some(key);
        }
    }

    None
}

/// Build a `DefaultLlmRouter` for a specific provider.
///
/// Provider-specific adapters are used when the wire format diverges from
/// the OpenAI Chat Completions shape (DeepSeek, Anthropic, Ollama, llama.cpp).
/// Everything else (OpenAI proper, Mistral, OpenRouter, custom OpenAI-compat
/// endpoints) goes through `OpenAiCompatibleProvider`.
///
/// When `override_slug` is `Some(s)`, every tier is wired to a single
/// provider instance built for slug `s` instead of consulting
/// `tier_models`. This is how an arc with a captured pin keeps its
/// model stable across tier_models edits — see `docs/PROVIDER_PINNING.md`
/// and the `EffectiveProviderTarget::pinned_slug` field.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_router_for_provider(
    provider_id: &str,
    base_url: &str,
    model: &str,
    api_key: Option<&str>,
    supports_vision: bool,
    supports_documents: bool,
    family: athen_core::llm::ModelFamily,
    tier_models: &HashMap<ModelProfile, String>,
    override_slug: Option<&str>,
) -> Arc<DefaultLlmRouter> {
    let mut providers: HashMap<String, Box<dyn LlmProvider>> = HashMap::new();
    let mut profiles: HashMap<ModelProfile, ProfileConfig> = HashMap::new();

    // Build a provider instance for `slug`, key it under `key`, and remember
    // the key so duplicate slugs (e.g. Cheap and Fast both pointing at the
    // same model) reuse one provider instance instead of constructing four
    // independent reqwest::Clients.
    let mut slug_to_key: HashMap<String, String> = HashMap::new();
    let ensure_slug = |slug: &str,
                       providers: &mut HashMap<String, Box<dyn LlmProvider>>,
                       slug_to_key: &mut HashMap<String, String>|
     -> String {
        if let Some(k) = slug_to_key.get(slug) {
            return k.clone();
        }
        let key = format!("{}.{}", provider_id, slug);
        let provider = build_provider_instance(
            provider_id,
            base_url,
            slug,
            api_key,
            supports_vision,
            supports_documents,
            family,
        );
        providers.insert(key.clone(), provider);
        slug_to_key.insert(slug.to_string(), key.clone());
        key
    };

    // Normalise the override: an empty string is treated as "no
    // override" so a stale empty `pinned_slug` column can't blank-out
    // the slug at request time.
    let override_slug = override_slug.filter(|s| !s.is_empty());

    // Resolve each tier: when `override_slug` is set, use it for every
    // tier (pin path — slug is frozen for the rest of the arc). Else
    // consult caller-supplied `tier_models`, falling back to the parent
    // provider's `model`. Empty map ⇒ all four tiers point at one
    // provider instance under the default slug, preserving the legacy
    // single-model behaviour for configs that predate the per-tier field.
    for tier in [
        ModelProfile::Judges,
        ModelProfile::Fast,
        ModelProfile::Code,
        ModelProfile::Powerful,
    ] {
        let slug = if let Some(pinned) = override_slug {
            tracing::debug!(
                provider_id = %provider_id,
                tier = ?tier,
                pinned_slug = %pinned,
                "honoring pinned slug instead of tier_models"
            );
            pinned
        } else {
            tier_models
                .get(&tier)
                .map(|s| s.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or(model)
        };
        let key = ensure_slug(slug, &mut providers, &mut slug_to_key);
        profiles.insert(
            tier,
            ProfileConfig {
                description: format!("{} {:?}", provider_id, tier),
                priority: vec![key],
                fallback: None,
            },
        );
    }

    Arc::new(DefaultLlmRouter::new(
        providers,
        profiles,
        BudgetTracker::new(None),
    ))
}

/// Build a global `LlmRouter` from a Bundle's per-tier `(connection,
/// slug)` picks. Each unique `(connection_id, slug)` pair becomes one
/// provider instance keyed `"{connection_id}.{slug}"`; the router's
/// `profiles` map points each `ModelProfile` at the matching key.
///
/// Cross-vendor by construction: Cheap tier may be DeepSeek, Code tier
/// Anthropic, Powerful tier OpenAI — each lands on its own adapter built
/// from its own Connection's credentials.
///
/// Sparse Bundles use the [`pick_bundle_tier`] fallback ladder
/// (Code→Fast→Cheap, Powerful→Fast→Cheap, Fast→Cheap). Tier slots that
/// still resolve to nothing — empty Bundle, deleted Connection — are
/// skipped: the router still builds, but requests for that profile fail
/// at dispatch time with "no provider for profile X". This mirrors how
/// [`build_router_for_provider`] handles a missing active provider: keep
/// the surface alive, surface the misconfiguration at request time.
///
/// `connections` must be the *hydrated* `models.providers` map (vault
/// secrets already filled into each `ProviderConfig.auth`). Caller is
/// `set_active_bundle` and friends, which all go through
/// `load_models_config_hydrated`.
pub(crate) fn build_router_for_bundle(
    bundle: &Bundle,
    connections: &HashMap<String, athen_core::config::ProviderConfig>,
) -> Arc<DefaultLlmRouter> {
    let mut providers: HashMap<String, Box<dyn LlmProvider>> = HashMap::new();
    let mut profiles: HashMap<ModelProfile, ProfileConfig> = HashMap::new();
    // Dedup by `(connection_id, slug)` so two tiers sharing the same
    // pick (e.g. Cheap=Fast=`deepseek-v4-flash` on `deepseek`) reuse one
    // provider instance instead of constructing two reqwest::Clients.
    let mut built_keys: HashMap<String, String> = HashMap::new();

    for tier in [
        ModelProfile::Judges,
        ModelProfile::Fast,
        ModelProfile::Code,
        ModelProfile::Powerful,
    ] {
        let Some((cid, slug)) = pick_bundle_tier(bundle, tier) else {
            continue;
        };
        let Some(cfg) = connections.get(&cid) else {
            warn!(
                tier = ?tier,
                connection_id = %cid,
                bundle = %bundle.name,
                "Bundle references unknown Connection; tier will be undispatchable"
            );
            continue;
        };

        let pair_key = format!("{cid}::{slug}");
        let provider_key = if let Some(k) = built_keys.get(&pair_key) {
            k.clone()
        } else {
            let key = format!("{cid}.{slug}");
            // Credential resolution: prefer the hydrated `auth` field;
            // fall back to `{CID}_API_KEY` env var (preserves the
            // env-only legacy path used by CLI smoke runs). Empty +
            // unsubstituted-template (`${VAR}`) auth values fall through
            // to env to avoid sending placeholder text upstream.
            let api_key = match &cfg.auth {
                AuthType::ApiKey(k) if !k.is_empty() && !k.starts_with("${") => Some(k.clone()),
                _ => std::env::var(format!("{}_API_KEY", cid.to_uppercase()))
                    .ok()
                    .filter(|k| !k.is_empty()),
            };
            let base_url = cfg
                .endpoint
                .clone()
                .unwrap_or_else(|| crate::settings::default_base_url(&cid).to_string());
            let provider = build_provider_instance(
                &cid,
                &base_url,
                &slug,
                api_key.as_deref(),
                cfg.supports_vision,
                cfg.supports_documents,
                cfg.family,
            );
            providers.insert(key.clone(), provider);
            built_keys.insert(pair_key, key.clone());
            key
        };

        profiles.insert(
            tier,
            ProfileConfig {
                description: format!("{cid} {tier:?}"),
                priority: vec![provider_key],
                fallback: None,
            },
        );
    }

    Arc::new(DefaultLlmRouter::new(
        providers,
        profiles,
        BudgetTracker::new(None),
    ))
}

/// Pick the right startup router for the AppState constructor.
///
/// Resolution order:
/// 1. **Active Bundle** (`models.assignments["active_bundle"]`): build a
///    cross-vendor router from its per-tier `(connection, slug)` picks
///    via [`build_router_for_bundle`]. `active_provider_id` is set to
///    [`derive_primary_connection_pair`]'s pick so legacy snapshot
///    readers (vision-check, fallback router rebuilds in commands.rs)
///    stay coherent with the Bundle's "primary" tier.
/// 2. **Legacy single-provider** (`models.assignments["active_provider"]`):
///    today's behaviour for pre-Bundles configs or migrated configs whose
///    active Bundle id has gone stale. Builds the router via
///    [`build_router_for_provider_from_config`].
///
/// Returns `(router, active_provider_id, model_name)`. Callers stamp the
/// last two onto `state.active_provider_id` and `state.model_name`.
fn build_startup_router(config: &AthenConfig) -> (Arc<DefaultLlmRouter>, String, String) {
    if let Some(bundle_id) = config.models.assignments.get(ACTIVE_BUNDLE_KEY) {
        if let Some(bundle) = config.models.bundles.get(bundle_id) {
            let router = build_router_for_bundle(bundle, &config.models.providers);
            let (cid, slug) = derive_primary_connection_pair(bundle).unwrap_or_else(|| {
                // Bundle has no tiers at all (degenerate). Fall back to
                // the legacy active_provider so the rest of the app has a
                // sane snapshot to read.
                let legacy = resolve_active_provider(config);
                let model = config
                    .models
                    .providers
                    .get(&legacy)
                    .map(|p| p.default_model.clone())
                    .unwrap_or_else(|| default_model_for(&legacy).to_string());
                (legacy, model)
            });
            info!(
                bundle_name = %bundle.name,
                primary_connection = %cid,
                primary_slug = %slug,
                "Startup router built from active Bundle"
            );
            return (router, cid, slug);
        }
        warn!(
            stale_bundle_id = %bundle_id,
            "active_bundle id does not resolve in models.bundles; using legacy single-provider router"
        );
    }
    let active_id = resolve_active_provider(config);
    let (router, model_name) = build_router_for_provider_from_config(&active_id, config);
    (router, active_id, model_name)
}

/// `bundle_settings::derive_primary_connection` mirror that doesn't
/// require pulling in the `bundle_settings` module from state.rs (which
/// would create a cycle: bundle_settings already depends on state).
/// Prefers `Fast → Cheap → Code → Powerful → Local` — the everyday-loop
/// tier first, then the cheapest fallback.
fn derive_primary_connection_pair(bundle: &Bundle) -> Option<(String, String)> {
    for tier in [
        ModelProfile::Fast,
        ModelProfile::Judges,
        ModelProfile::Code,
        ModelProfile::Powerful,
        ModelProfile::Local,
    ] {
        if let Some(bt) = bundle.tiers.get(&tier) {
            return Some((bt.connection_id.clone(), bt.slug.clone()));
        }
    }
    None
}

/// Heuristic: does this slug name a MiniMax M2.x model on the OpenCode
/// Go relay? Used to dispatch the unified `opencode_go` provider id to
/// the Anthropic-compat wire (`/v1/messages`) vs the OpenAI-compat wire
/// (`/v1/chat/completions`).
///
/// Match rule (case-insensitive): the slug starts with `minimax-m2`
/// followed by either end-of-string, `.`, `-`, or `_`. This catches
/// `minimax-m2`, `minimax-m2.5`, `minimax-m2.7`, `minimax-m2-foo`,
/// `minimax-m2_bar` and rejects future generations like `minimax-m3`
/// (different wire shape may apply — re-evaluate when shipped) plus
/// non-MiniMax slugs like `kimi-k2.5`.
pub(crate) fn is_minimax_slug(slug: &str) -> bool {
    let lower = slug.to_ascii_lowercase();
    let Some(rest) = lower.strip_prefix("minimax-m2") else {
        return false;
    };
    // Bare "minimax-m2" or one of the expected separators next.
    matches!(
        rest.as_bytes().first().copied(),
        None | Some(b'.') | Some(b'-') | Some(b'_')
    )
}

/// Construct a single adapter instance for `provider_id` with the given
/// model slug. Factored out of `build_router_for_provider` so the per-tier
/// loop can build N instances against the same credentials + base URL.
fn build_provider_instance(
    provider_id: &str,
    base_url: &str,
    model: &str,
    api_key: Option<&str>,
    supports_vision: bool,
    supports_documents: bool,
    family: athen_core::llm::ModelFamily,
) -> Box<dyn LlmProvider> {
    // Per-slug quirks (Bundles Phase 3): if the slug appears in the
    // registry, its family wins over the Connection-level fallback. The
    // Connection's `family` field stays as the safety net for Custom slugs
    // the registry has never seen, preserving today's behaviour for
    // self-hosted or unprofiled models.
    let family = quirks_seed::lookup_slug_quirks(provider_id, model)
        .map(|q| q.family)
        .unwrap_or(family);
    match provider_id {
        "deepseek" => {
            let key = api_key.unwrap_or_default().to_string();
            let mut p = DeepSeekProvider::new(key).with_family(family);
            if base_url != "https://api.deepseek.com" {
                p = p.with_base_url(base_url.to_string());
            }
            if model != "deepseek-chat" {
                p = p.with_model(model.to_string());
            }
            Box::new(p)
        }
        "anthropic" | "minimax_anthropic" | "kimi_code" => {
            // minimax_anthropic and kimi_code route through
            // AnthropicProvider for the /v1/messages wire format. MiniMax
            // Token Plan exposes it at api.minimax.io/anthropic (with
            // prompt-cache); Kimi Code Plan at api.kimi.com/coding.
            // Provider adapter is identical; only the base URL differs.
            let key = api_key.unwrap_or_default().to_string();
            // MiniMax slugs go on the wire lowercase regardless of the
            // casing the user picked / persisted. Native Anthropic models
            // (`claude-*`) are unaffected — `is_minimax_slug` only matches
            // the `minimax-m2*` family.
            let model_for_wire = if provider_id == "minimax_anthropic" || is_minimax_slug(model) {
                model.to_ascii_lowercase()
            } else {
                model.to_string()
            };
            let mut p = AnthropicProvider::new(key, model_for_wire)
                .with_family(family)
                .with_vision(supports_vision)
                .with_documents(supports_documents);
            if base_url != "https://api.anthropic.com" && !base_url.is_empty() {
                p = p.with_base_url(base_url.to_string());
            }
            Box::new(p)
        }
        "opencode_go" => {
            // OpenCode Go is one logical provider covering two wire
            // formats on the same relay host: OpenAI-compat
            // `/v1/chat/completions` for DeepSeek/Qwen/Kimi/GLM/MiMo and
            // Anthropic-compat `/v1/messages` for MiniMax M2.x. Wire
            // dispatch is by slug (`is_minimax_slug`); per-slug family
            // (DeepSeekV4Chat vs DeepSeekV4Pro vs MiniMaxM25Cloud vs
            // KimiK26Cloud vs Qwen3CoderNext) was resolved at the top of
            // this fn via the per-slug quirks registry.
            let key = api_key.unwrap_or_default().to_string();
            if is_minimax_slug(model) {
                // OpenCode Go's Anthropic-compat relay rejects mixed-case
                // ids (`MiniMax-M2.7` → "Model … is not supported"); the
                // canonical wire id is lowercase. Normalise here so any
                // historical or user-typed casing still resolves.
                let wire_slug = model.to_ascii_lowercase();
                let mut p = AnthropicProvider::new(key, wire_slug)
                    .with_family(family)
                    .with_vision(supports_vision)
                    .with_documents(supports_documents);
                if !base_url.is_empty() {
                    p = p.with_base_url(base_url.to_string());
                }
                Box::new(p)
            } else {
                let mut p = OpenAiCompatibleProvider::new(base_url.to_string())
                    .with_model(model.to_string())
                    .with_provider_id(provider_id.to_string())
                    .with_family(family)
                    .with_vision(supports_vision)
                    .with_documents(supports_documents);
                p = p.with_api_key(key);
                Box::new(p)
            }
        }
        "google" => {
            let key = api_key.unwrap_or_default().to_string();
            let mut p = GoogleProvider::new(key, model.to_string())
                .with_family(family)
                .with_vision(supports_vision)
                .with_documents(supports_documents);
            if base_url != "https://generativelanguage.googleapis.com" && !base_url.is_empty() {
                p = p.with_base_url(base_url.to_string());
            }
            Box::new(p)
        }
        "ollama" => {
            let mut p = OllamaProvider::new(model.to_string()).with_family(family);
            if base_url != "http://localhost:11434" {
                p = p.with_base_url(base_url.to_string());
            }
            Box::new(p)
        }
        "llamacpp" => Box::new(
            LlamaCppProvider::new(base_url.to_string(), model.to_string()).with_family(family),
        ),
        _ => {
            let mut p = OpenAiCompatibleProvider::new(base_url.to_string())
                .with_model(model.to_string())
                .with_provider_id(provider_id.to_string())
                .with_family(family)
                .with_vision(supports_vision)
                .with_documents(supports_documents);
            if let Some(key) = api_key {
                p = p.with_api_key(key.to_string());
            }
            Box::new(p)
        }
    }
}

/// Generate a human-readable Arc identifier: `arc_YYYYMMDD_HHMMSS`.
fn generate_arc_id() -> String {
    chrono::Utc::now().format("arc_%Y%m%d_%H%M%S").to_string()
}

/// Look up the active agent profile's `github_identity` for a given arc.
///
/// Falls back to `GithubIdentity::None` on every missing-piece path: no
/// arc_store, no profile_store, arc isn't persisted, profile lookup
/// fails. The shell tool's env injection is also a no-op for `None`, so
/// this whole chain degrades gracefully — at worst, git/gh commands run
/// without auth instead of erroring at command time.
pub(crate) async fn resolve_github_identity_for_arc(
    profile_store: Option<&Arc<athen_persistence::profiles::SqliteProfileStore>>,
    arc_store: Option<&athen_persistence::arcs::ArcStore>,
    arc_id: &str,
) -> athen_core::agent_profile::GithubIdentity {
    use athen_core::traits::profile::ProfileStore;
    let Some(pstore) = profile_store else {
        return athen_core::agent_profile::GithubIdentity::None;
    };
    // Determine the active profile id: explicit on the arc row, falling
    // back to the default profile.
    let active_id: Option<String> = if let Some(astore) = arc_store {
        match astore.get_arc(arc_id).await {
            Ok(Some(meta)) => meta.active_profile_id,
            _ => None,
        }
    } else {
        None
    };
    match pstore.get_or_default(active_id.as_ref()).await {
        Ok(p) => p.github_identity,
        Err(_) => athen_core::agent_profile::GithubIdentity::None,
    }
}

/// Try to restore the most recent active Arc from persistent storage.
/// If the store is unavailable or empty, create a new Arc with empty history.
async fn restore_or_create_arc(arc_store: &Option<ArcStore>) -> (String, Vec<ChatMessage>) {
    if let Some(store) = arc_store {
        match store.list_root_arcs().await {
            Ok(arcs) if !arcs.is_empty() => {
                // Find the most recent active arc.
                let active = arcs
                    .iter()
                    .find(|a| a.status == athen_persistence::arcs::ArcStatus::Active);
                if let Some(arc) = active {
                    match store.load_entries(&arc.id).await {
                        Ok(entries) => {
                            let history: Vec<ChatMessage> = entries
                                .into_iter()
                                .filter(|e| {
                                    e.entry_type == athen_persistence::arcs::EntryType::Message
                                })
                                .map(|e| ChatMessage {
                                    role: match e.source.as_str() {
                                        "user" => Role::User,
                                        "assistant" => Role::Assistant,
                                        "system" => Role::System,
                                        "tool" => Role::Tool,
                                        _ => Role::User,
                                    },
                                    content: MessageContent::Text(e.content),
                                })
                                .collect();
                            info!("Restored {} messages from arc '{}'", history.len(), arc.id);
                            return (arc.id.clone(), history);
                        }
                        Err(e) => {
                            warn!("Failed to load entries for arc '{}': {e}", arc.id);
                        }
                    }
                }
            }
            Err(e) => warn!("Failed to list arcs: {e}"),
            _ => {}
        }
    }

    let new_id = generate_arc_id();
    if let Some(store) = arc_store {
        if let Err(e) = store
            .create_arc(
                &new_id,
                "New Arc",
                athen_persistence::arcs::ArcSource::UserInput,
            )
            .await
        {
            warn!("Failed to create initial arc: {e}");
        }
    }
    (new_id, Vec::new())
}

/// Build the coordinator with the combined (rules + LLM) risk evaluator,
/// trust manager, and optional SQLite persistence at `~/.athen/athen.db`.
async fn build_coordinator_with_persistence(
    router: &Arc<RwLock<Arc<DefaultLlmRouter>>>,
) -> (Coordinator, Option<Database>, Option<SqliteContactStore>) {
    let risk_router: Box<dyn LlmRouter> = Box::new(SharedRouter(Arc::clone(router)));
    let llm_evaluator = LlmRiskEvaluator::new(risk_router);
    let combined = CombinedRiskEvaluator::new(llm_evaluator);
    let mut coordinator = Coordinator::new(Box::new(combined));

    // Try to open the database for persistence.
    if let Some(data_dir) = ensure_data_dir() {
        let db_path = data_dir.join("athen.db");
        match Database::new(&db_path).await {
            Ok(db) => {
                let store = db.store();
                let contact_store = db.contact_store();
                info!("Database opened at {}", db_path.display());

                // Wire trust manager with SQLite-backed contact store.
                let trust_manager = TrustManager::new(Box::new(contact_store.clone()));
                coordinator = coordinator
                    .with_persistence(Box::new(store))
                    .with_trust_manager(trust_manager);

                return (coordinator, Some(db), Some(contact_store));
            }
            Err(e) => {
                warn!(
                    "Failed to open database at {}: {e}. Running without persistence.",
                    db_path.display()
                );
            }
        }
    }

    (coordinator, None, None)
}

/// Restore the set of enabled MCPs from the SQLite store into the registry.
/// Bundled entries are matched against the in-code catalog; BYO entries
/// deserialize their full `McpCatalogEntry` from the `mcp_custom_entries`
/// table.
async fn restore_enabled_mcps(registry: &Arc<McpRegistry>, store: &McpStore) -> Result<()> {
    // Build a lookup of custom definitions so we can resolve enabled rows
    // that have no bundled-catalog counterpart.
    let custom_defs: std::collections::HashMap<String, athen_core::traits::mcp::McpCatalogEntry> =
        store
            .list_custom()
            .await
            .unwrap_or_else(|e| {
                warn!("Failed to load custom MCP definitions: {e}");
                Vec::new()
            })
            .into_iter()
            .map(|e| (e.id.clone(), e))
            .collect();

    let rows = store.list_enabled().await?;
    let mut entries = Vec::new();
    for row in rows {
        if let Some(entry) = athen_mcp::lookup(&row.mcp_id) {
            entries.push(athen_mcp::EnabledEntry {
                entry,
                config: row.config,
            });
        } else if let Some(entry) = custom_defs.get(&row.mcp_id).cloned() {
            entries.push(athen_mcp::EnabledEntry {
                entry,
                config: row.config,
            });
        } else {
            warn!(
                "Persisted MCP id '{}' not found in catalog or custom registry; skipping",
                row.mcp_id
            );
        }
    }
    let count = entries.len();
    registry.set_enabled(entries).await;
    info!("Restored {count} enabled MCP(s)");
    Ok(())
}

/// Build the persistent memory system (vector search + knowledge graph)
/// backed by a separate SQLite connection to `~/.athen/athen.db`.
///
/// Uses keyword embeddings as fallback (always available, near-instant)
/// and an LLM entity extractor for automatic knowledge graph population.
/// Returns `None` if the data directory or database cannot be opened.
async fn build_memory(
    router: &Arc<RwLock<Arc<DefaultLlmRouter>>>,
    embedder_cell: EmbedderCell,
) -> Option<Arc<Memory>> {
    use athen_memory::extractor::LlmEntityExtractor;
    use athen_memory::sqlite::{SqliteGraph, SqliteLexicalIndex, SqliteVectorIndex};

    let data_dir = ensure_data_dir()?;
    let db_path = data_dir.join("athen.db");

    // Open a separate rusqlite connection for the memory subsystem.
    // Memory uses std::sync::Mutex while Database uses tokio::sync::Mutex,
    // so they cannot share a connection. SQLite handles concurrent access
    // from multiple connections to the same file safely.
    let conn = match rusqlite::Connection::open(&db_path) {
        Ok(c) => {
            // WAL + the same performance pragma block the persistence layer
            // sets: NORMAL sync (durable under WAL, far fewer fsyncs), a
            // busy_timeout so concurrent writers retry instead of erroring,
            // a larger page cache, mmap-backed reads, and in-memory temp
            // tables. `execute_batch` (not `execute`) because `journal_mode`
            // returns a row.
            let _ = c.execute_batch(
                "PRAGMA journal_mode=WAL;\n\
                 PRAGMA synchronous=NORMAL;\n\
                 PRAGMA busy_timeout=5000;\n\
                 PRAGMA cache_size=-16000;\n\
                 PRAGMA mmap_size=134217728;\n\
                 PRAGMA temp_store=MEMORY;",
            );
            c
        }
        Err(e) => {
            warn!(
                "Failed to open memory database at {}: {e}",
                db_path.display()
            );
            return None;
        }
    };

    let conn = std::sync::Arc::new(std::sync::Mutex::new(conn));

    let vector = match SqliteVectorIndex::new(conn.clone()) {
        Ok(v) => v,
        Err(e) => {
            warn!("Failed to create vector index: {e}");
            return None;
        }
    };
    let lexical = match SqliteLexicalIndex::new(conn.clone()) {
        Ok(l) => l,
        Err(e) => {
            warn!("Failed to create lexical (FTS5) index: {e}");
            return None;
        }
    };
    // Keep a handle to the shared connection for the backfill marker check.
    let marker_conn = conn.clone();
    let graph = match SqliteGraph::new(conn) {
        Ok(g) => g,
        Err(e) => {
            warn!("Failed to create knowledge graph: {e}");
            return None;
        }
    };

    // Embedder is the shared hot-swappable cell (see EmbedderCell): a
    // `reload_embedder` / `start_embedder_warmup` swap reaches the memory store
    // live, no rebuild. When no neural provider is configured the cell holds an
    // EmbeddingRouter that collapses to the keyword fallback inside `resolve`.
    let extractor_router: Box<dyn LlmRouter> = Box::new(SharedRouter(Arc::clone(router)));
    let extractor = LlmEntityExtractor::new(extractor_router);

    // Default fusion weights (cosine_floor 0.45 admission, low min_final).
    // The hybrid ranker fuses semantic + lexical (BM25) + graph + recency +
    // frequency; see athen_core::traits::memory::FusionWeights.
    let memory = Memory::new(Box::new(vector), Box::new(graph))
        .with_embedder(Box::new(SwappableEmbedder::new(embedder_cell)))
        .with_extractor(Box::new(extractor))
        .with_lexical(Box::new(lexical));

    // One-time backfill for DBs predating the hybrid rework: populate the
    // `mentions` links + lexical (FTS5) index from each memory's stored
    // metadata. Guarded by a marker row so it runs at most once.
    let needs_backfill = {
        // Memory's connection is a std::sync::Mutex (see note above); recover
        // the guard if a prior panic poisoned it rather than skipping the
        // backfill — the protected Connection itself is still usable.
        let c = marker_conn.lock_recover();
        let _ =
            c.execute_batch("CREATE TABLE IF NOT EXISTS memory_migrations (key TEXT PRIMARY KEY);");
        let done: bool = c
            .query_row(
                "SELECT 1 FROM memory_migrations WHERE key = 'hybrid_backfill_v1'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        !done
    };
    if needs_backfill {
        match memory.backfill_hybrid().await {
            Ok(n) => {
                info!("Memory hybrid backfill: processed {n} existing memories");
                let _ = marker_conn.lock_recover().execute(
                    "INSERT OR IGNORE INTO memory_migrations (key) VALUES ('hybrid_backfill_v1')",
                    [],
                );
            }
            Err(e) => warn!("Memory hybrid backfill failed (will retry next boot): {e}"),
        }
    }

    info!("Memory system initialized with SQLite persistence (hybrid recall)");
    Some(Arc::new(memory))
}

/// Sanitize an endpoint name for use as a filename. Lowercase, ASCII
/// alphanumerics + `_`. Empty result falls back to `endpoint` so we
/// never write an unnamed file.
fn sanitize_endpoint_filename(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('_') {
            out.push('_');
        }
    }
    let trimmed = out.trim_matches('_').to_string();
    if trimmed.is_empty() {
        "endpoint".to_string()
    } else {
        trimmed
    }
}

fn auth_method_short(am: &athen_core::http_endpoint::AuthMethod) -> String {
    use athen_core::http_endpoint::AuthMethod as A;
    match am {
        A::None => "no auth".to_string(),
        A::BearerToken => "Bearer token".to_string(),
        A::Header { name } => format!("header `{name}`"),
        A::HeaderPrefixed { name, prefix } => format!("header `{name}` (prefix `{prefix}`)"),
        A::QueryParam { name } => format!("query param `{name}`"),
        A::BasicAuth { user } => format!("basic auth (user `{user}`)"),
    }
}

/// One-line summary used in the index. Pulls the preset's blurb when
/// available so the agent gets a shape hint without reading the detail.
fn endpoint_one_liner(
    ep: &athen_core::http_endpoint::RegisteredEndpoint,
    preset: Option<&crate::http_presets::EndpointPreset>,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    parts.push(auth_method_short(&ep.auth_method));
    if let Some(p) = preset {
        parts.push(p.free_tier_blurb.to_string());
    }
    if let Some(notes) = &ep.notes {
        if !notes.is_empty() {
            parts.push(notes.clone());
        }
    }
    if !ep.enabled {
        parts.push("DISABLED".to_string());
    }
    parts.join(" — ")
}

fn render_cloud_apis_index(rows: &[(String, String, std::path::PathBuf)]) -> String {
    let mut out = String::new();
    out.push_str("# Registered HTTP endpoints (`http_request`)\n\n");
    if rows.is_empty() {
        out.push_str(
            "No endpoints registered. The user manages this list in \
             Settings → Cloud APIs. Until at least one is registered, \
             the `http_request` tool has nothing to call.\n",
        );
        return out;
    }
    out.push_str(
        "Index of every endpoint Athen can reach via `http_request`. \
         For the full per-endpoint usage (sample paths, auth specifics, \
         free-tier limits), `read` the linked detail file — they are \
         tiny so reads stay cheap.\n\n",
    );
    for (name, blurb, path) in rows {
        out.push_str(&format!(
            "- **{name}** — {blurb}\n  Detail: `{}`\n",
            path.display()
        ));
    }
    out.push('\n');
    out.push_str(
        "Call shape: `http_request(endpoint=\"<name>\", path=\"<rest>\", \
         method=\"GET\"|\"POST\"|...)`. The path is joined onto the \
         endpoint's base_url; query parameters can ride in `path` or \
         in a structured `query` object.\n",
    );
    out
}

fn render_endpoint_detail(
    ep: &athen_core::http_endpoint::RegisteredEndpoint,
    preset: Option<&crate::http_presets::EndpointPreset>,
) -> String {
    let mut out = String::new();
    out.push_str(&format!("# {}\n\n", ep.name));
    out.push_str(&format!("Provider: {}\n\n", ep.provider));
    out.push_str(&format!("Base URL: `{}`\n\n", ep.base_url));
    out.push_str(&format!("Auth: {}\n\n", auth_method_short(&ep.auth_method)));
    if !ep.enabled {
        out.push_str("⚠ Endpoint is currently DISABLED — calls will refuse.\n\n");
    }
    if let Some(p) = preset {
        out.push_str(&format!("Free tier: {}\n\n", p.free_tier_blurb));
        out.push_str(&format!("Sign-up / docs: {}\n\n", p.signup_url));
        if !p.test_path.is_empty() {
            out.push_str("## Sample call\n\n```json\n");
            out.push_str(
                &serde_json::to_string_pretty(&serde_json::json!({
                    "endpoint": ep.name,
                    "method": "GET",
                    "path": p.test_path,
                }))
                .unwrap_or_default(),
            );
            out.push_str("\n```\n\n");
        }
    }
    if let Some(p) = preset {
        if !p.usage_hints.is_empty() {
            out.push_str("## Usage hints\n\n");
            out.push_str(p.usage_hints);
            out.push_str("\n\n");
        }
    }
    if let Some(notes) = &ep.notes {
        if !notes.is_empty() {
            out.push_str("## Notes (user-provided)\n\n");
            out.push_str(notes);
            out.push_str("\n\n");
        }
    }
    if !ep.default_headers.is_empty() {
        out.push_str("## Default headers (sent on every call)\n\n");
        for (k, v) in &ep.default_headers {
            out.push_str(&format!("- `{k}: {v}`\n"));
        }
        out.push('\n');
    }
    if !ep.default_query_params.is_empty() {
        out.push_str("## Default query params (sent on every call)\n\n");
        for (k, v) in &ep.default_query_params {
            out.push_str(&format!("- `{k}={v}`\n"));
        }
        out.push('\n');
    }
    if let Some(rl) = ep.rate_limit {
        if rl.requests_per_minute > 0 {
            out.push_str(&format!(
                "Rate limit: {} req/min (in-process; exceeding returns a structured `rate_limited` error).\n\n",
                rl.requests_per_minute
            ));
        }
    }
    out
}

#[cfg(test)]
mod supervisor_tests {
    use super::spawn_supervised;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[tokio::test]
    async fn respawns_after_a_panicking_attempt_then_stops_cleanly() {
        // Attempt 1 panics (top-level panic in the spawned task). The
        // supervisor must catch it (as a JoinError), respawn, and on attempt 2
        // the factory runs cleanly; `should_stop` then ends the supervisor.
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_factory = Arc::clone(&attempts);
        let attempts_stop = Arc::clone(&attempts);

        spawn_supervised(
            "test-monitor",
            std::time::Duration::from_millis(1),
            // Stop once the second attempt has completed (>= 2 runs).
            move || attempts_stop.load(Ordering::SeqCst) >= 2,
            move || {
                let n = attempts_factory.fetch_add(1, Ordering::SeqCst);
                async move {
                    if n == 0 {
                        panic!("intentional first-attempt panic");
                    }
                    // Second attempt: return cleanly.
                }
            },
        )
        .await;

        // Factory was invoked at least twice: once (panicked) + a respawn.
        assert!(
            attempts.load(Ordering::SeqCst) >= 2,
            "supervisor should have respawned after the panicking attempt"
        );
    }

    #[tokio::test]
    async fn does_not_spawn_when_stop_already_requested() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_factory = Arc::clone(&attempts);

        spawn_supervised(
            "test-monitor",
            std::time::Duration::from_millis(1),
            || true, // already stopped
            move || {
                attempts_factory.fetch_add(1, Ordering::SeqCst);
                async move {}
            },
        )
        .await;

        assert_eq!(
            attempts.load(Ordering::SeqCst),
            0,
            "factory must not run when stop is requested up front"
        );
    }
}

#[cfg(test)]
mod newarc_command_tests {
    use super::parse_newarc_command;

    #[test]
    fn bare_newarc_resets_with_empty_remainder() {
        let (force, rest) = parse_newarc_command("/newarc");
        assert!(force);
        assert!(rest.is_empty());
    }

    #[test]
    fn newarc_with_followup_keeps_followup() {
        let (force, rest) = parse_newarc_command("/newarc check my email");
        assert!(force);
        assert_eq!(rest, "check my email");
    }

    #[test]
    fn leading_whitespace_does_not_block_command() {
        let (force, rest) = parse_newarc_command("   /newarc reset please");
        assert!(force);
        assert_eq!(rest, "reset please");
    }

    #[test]
    fn newarc_mid_message_is_content() {
        // The command only fires when /newarc is the leading token. A
        // user typing "we should /newarc this thread" is not asking for
        // a reset — they're discussing the command in the body of a
        // genuine message. Don't swallow it.
        let (force, rest) = parse_newarc_command("we should /newarc this thread");
        assert!(!force);
        assert_eq!(rest, "we should /newarc this thread");
    }

    #[test]
    fn similar_command_prefix_does_not_match() {
        // Hypothetical pathology: /newarchaeology — must not trigger.
        let (force, rest) = parse_newarc_command("/newarchaeology check");
        assert!(!force);
        assert_eq!(rest, "/newarchaeology check");
    }

    #[test]
    fn newarc_with_tab_separator() {
        let (force, rest) = parse_newarc_command("/newarc\tafter tab");
        assert!(force);
        assert_eq!(rest, "after tab");
    }

    #[test]
    fn deepresearch_command_parsing() {
        use super::parse_deepresearch_command;
        // Topic captured, command + bot suffix tolerated, case-insensitive.
        assert_eq!(
            parse_deepresearch_command("/deepresearch EU right to repair"),
            Some("EU right to repair".to_string())
        );
        assert_eq!(
            parse_deepresearch_command("  /DeepResearch@AthenBot   spaced topic  "),
            Some("spaced topic".to_string())
        );
        assert_eq!(
            parse_deepresearch_command("/deep_research underscore variant"),
            Some("underscore variant".to_string())
        );
        // Bare command → empty topic (the handler then prompts for usage).
        assert_eq!(parse_deepresearch_command("/deepresearch"), Some(String::new()));
        // Not the command.
        assert_eq!(parse_deepresearch_command("just a message"), None);
        assert_eq!(parse_deepresearch_command("/newarc something"), None);
        // No false-positive on a longer command token.
        assert_eq!(parse_deepresearch_command("/deepresearchx topic"), None);
    }
}

#[cfg(test)]
mod resolve_effective_tier_tests {
    use super::resolve_effective_tier_for_arc;
    use athen_core::llm::ModelProfile;
    use athen_core::risk::ComplexityTag;
    use athen_persistence::arcs::{ArcSource, ArcStore};
    use rusqlite::Connection;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    async fn setup_store() -> ArcStore {
        let conn = Connection::open_in_memory().expect("open in-memory db");
        let store = ArcStore::new(Arc::new(Mutex::new(conn)));
        store.init_schema().await.expect("init arc schema");
        store
    }

    /// Override beats both complexity and the static default — even when
    /// the task-complexity tag would point somewhere else.
    #[tokio::test]
    async fn override_wins_over_complexity_and_default() {
        let store = setup_store().await;
        store
            .create_arc("arc1", "t", ArcSource::UserInput)
            .await
            .unwrap();
        store
            .set_tier_override("arc1", Some("Cheap"))
            .await
            .unwrap();

        let tier = resolve_effective_tier_for_arc(
            Some(&store),
            "arc1",
            Some(ComplexityTag::High),
            false,
            ModelProfile::Fast,
        )
        .await;
        assert_eq!(tier, ModelProfile::Judges);
    }

    /// No override but a complexity tag is present — the tag maps to a
    /// tier and beats the static call-site default.
    #[tokio::test]
    async fn complexity_wins_when_no_override() {
        let store = setup_store().await;
        store
            .create_arc("arc2", "t", ArcSource::UserInput)
            .await
            .unwrap();

        let tier = resolve_effective_tier_for_arc(
            Some(&store),
            "arc2",
            Some(ComplexityTag::High),
            false,
            ModelProfile::Fast,
        )
        .await;
        assert_eq!(tier, ModelProfile::Powerful);
    }

    /// No override and no complexity tag — fall through to the static
    /// caller-supplied default (Road 1's call-site label).
    #[tokio::test]
    async fn default_used_when_no_override_no_complexity() {
        let store = setup_store().await;
        store
            .create_arc("arc3", "t", ArcSource::UserInput)
            .await
            .unwrap();

        let tier =
            resolve_effective_tier_for_arc(Some(&store), "arc3", None, false, ModelProfile::Fast)
                .await;
        assert_eq!(tier, ModelProfile::Fast);
    }

    /// Unknown wire string in `tier_override` must not crash or
    /// substitute the wrong tier — it falls through to the complexity /
    /// default path, with a warn-level log to surface the malformed row.
    #[tokio::test]
    async fn unknown_override_string_falls_through() {
        let store = setup_store().await;
        store
            .create_arc("arc4", "t", ArcSource::UserInput)
            .await
            .unwrap();
        store
            .set_tier_override("arc4", Some("Bogus"))
            .await
            .unwrap();

        // High complexity → Powerful, distinct from the Fast default, so a
        // pass proves the unknown override fell through to the complexity
        // path rather than silently using the default or a wrong tier.
        let tier = resolve_effective_tier_for_arc(
            Some(&store),
            "arc4",
            Some(ComplexityTag::High),
            false,
            ModelProfile::Fast,
        )
        .await;
        assert_eq!(tier, ModelProfile::Powerful);
    }

    /// `is_code_task` flips the resolved tier to `Code` for Low/Medium
    /// complexity tasks — the whole point of this signal. Regression
    /// guard for the original bug: `ModelProfile::Code` was unreachable
    /// from the auto-tier path.
    #[tokio::test]
    async fn code_task_flag_routes_to_code_tier() {
        let store = setup_store().await;
        store
            .create_arc("arc5", "t", ArcSource::UserInput)
            .await
            .unwrap();

        let tier = resolve_effective_tier_for_arc(
            Some(&store),
            "arc5",
            Some(ComplexityTag::Medium),
            true,
            ModelProfile::Fast,
        )
        .await;
        assert_eq!(tier, ModelProfile::Code);
    }

    /// High-complexity coding tasks still escalate to `Powerful`. Code
    /// tier is a small/fast specialist, not a "harder than Fast" upgrade.
    #[tokio::test]
    async fn high_complexity_beats_code_flag() {
        let store = setup_store().await;
        store
            .create_arc("arc6", "t", ArcSource::UserInput)
            .await
            .unwrap();

        let tier = resolve_effective_tier_for_arc(
            Some(&store),
            "arc6",
            Some(ComplexityTag::High),
            true,
            ModelProfile::Fast,
        )
        .await;
        assert_eq!(tier, ModelProfile::Powerful);
    }

    /// Arc tier override still wins even when the task is flagged as
    /// code — the user's explicit per-arc choice trumps auto-routing.
    #[tokio::test]
    async fn override_beats_code_task_flag() {
        let store = setup_store().await;
        store
            .create_arc("arc7", "t", ArcSource::UserInput)
            .await
            .unwrap();
        store
            .set_tier_override("arc7", Some("Powerful"))
            .await
            .unwrap();

        let tier = resolve_effective_tier_for_arc(
            Some(&store),
            "arc7",
            Some(ComplexityTag::Low),
            true,
            ModelProfile::Fast,
        )
        .await;
        assert_eq!(tier, ModelProfile::Powerful);
    }
}

#[cfg(test)]
mod resolve_security_mode_tests {
    use super::resolve_security_mode_for_arc;
    use athen_core::config::SecurityMode;
    use athen_persistence::arcs::{ArcSource, ArcStore};
    use rusqlite::Connection;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    async fn setup_store() -> ArcStore {
        let conn = Connection::open_in_memory().expect("open in-memory db");
        let store = ArcStore::new(Arc::new(Mutex::new(conn)));
        store.init_schema().await.expect("init arc schema");
        store
    }

    #[tokio::test]
    async fn override_wins_over_global() {
        let store = setup_store().await;
        store
            .create_arc("a", "t", ArcSource::UserInput)
            .await
            .unwrap();
        store
            .set_security_mode_override("a", Some("yolo"))
            .await
            .unwrap();
        let mode = resolve_security_mode_for_arc(Some(&store), "a", SecurityMode::Bunker).await;
        assert_eq!(mode, SecurityMode::Yolo);
    }

    #[tokio::test]
    async fn no_override_falls_through_to_global() {
        let store = setup_store().await;
        store
            .create_arc("a", "t", ArcSource::UserInput)
            .await
            .unwrap();
        let mode = resolve_security_mode_for_arc(Some(&store), "a", SecurityMode::Bunker).await;
        assert_eq!(mode, SecurityMode::Bunker);
    }

    #[tokio::test]
    async fn missing_arc_and_no_store_fall_through() {
        let store = setup_store().await;
        // Unknown arc id → global.
        assert_eq!(
            resolve_security_mode_for_arc(Some(&store), "nope", SecurityMode::Yolo).await,
            SecurityMode::Yolo
        );
        // No store at all → global.
        assert_eq!(
            resolve_security_mode_for_arc(None, "a", SecurityMode::Assistant).await,
            SecurityMode::Assistant
        );
    }
}

#[cfg(test)]
mod is_minimax_slug_tests {
    use super::is_minimax_slug;

    #[test]
    fn matches_known_minimax_slugs() {
        // Canonical: M2.7 / M2.5 / bare M2.
        assert!(is_minimax_slug("minimax-m2.7"));
        assert!(is_minimax_slug("minimax-m2.5"));
        assert!(is_minimax_slug("minimax-m2"));
        // Hyphen / underscore suffix variants stay routed to Anthropic
        // wire — relay-side variant slugs follow this shape.
        assert!(is_minimax_slug("minimax-m2-preview"));
        assert!(is_minimax_slug("minimax-m2_foo"));
    }

    #[test]
    fn case_insensitive() {
        assert!(is_minimax_slug("MiniMax-M2.7"));
        assert!(is_minimax_slug("MINIMAX-M2.5"));
        assert!(is_minimax_slug("Minimax-m2"));
    }

    #[test]
    fn rejects_other_slugs() {
        // DeepSeek / Kimi / Qwen / GLM all hit the OpenAI-compat wire.
        assert!(!is_minimax_slug("deepseek-v4-flash"));
        assert!(!is_minimax_slug("deepseek-v4-pro"));
        assert!(!is_minimax_slug("kimi-k2.5"));
        assert!(!is_minimax_slug("qwen-3-coder"));
        assert!(!is_minimax_slug("glm-4-air"));
        assert!(!is_minimax_slug(""));
        assert!(!is_minimax_slug("minimax"));
    }

    #[test]
    fn rejects_future_or_adjacent_generations() {
        // Future generations may need their own quirks profile — opt them
        // in explicitly rather than silently routing to Anthropic wire.
        assert!(!is_minimax_slug("minimax-m3"));
        assert!(!is_minimax_slug("minimax-m3.0"));
        assert!(!is_minimax_slug("minimax-m25"));
        assert!(!is_minimax_slug("minimax-m20"));
        // Sneaky-looking close-match.
        assert!(!is_minimax_slug("not-minimax-m2.7"));
    }
}

#[cfg(test)]
mod resolve_effective_provider_tests {
    use super::{resolve_effective_provider_for_arc_with_config, EffectiveProviderTarget};
    use athen_core::config::{AthenConfig, AuthType, ProviderConfig};
    use athen_core::llm::{ModelFamily, ModelProfile};
    use athen_persistence::arcs::{ArcSource, ArcStore};
    use rusqlite::Connection;
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    async fn setup_store() -> ArcStore {
        let conn = Connection::open_in_memory().expect("open in-memory db");
        let store = ArcStore::new(Arc::new(Mutex::new(conn)));
        store.init_schema().await.expect("init arc schema");
        store
    }

    fn mk_provider(default_model: &str) -> ProviderConfig {
        ProviderConfig {
            auth: AuthType::None,
            default_model: default_model.to_string(),
            endpoint: None,
            context_window_tokens: 128_000,
            compaction_trigger_pct: 65,
            compaction_target_pct: 30,
            supports_vision: false,
            supports_documents: false,
            family: ModelFamily::Default,
            temperature: None,
            tier_models: HashMap::new(),
        }
    }

    fn mk_config(provider_ids: &[(&str, &str)]) -> AthenConfig {
        let mut cfg = AthenConfig::default();
        for (id, default_model) in provider_ids {
            cfg.models
                .providers
                .insert((*id).to_string(), mk_provider(default_model));
        }
        cfg
    }

    /// Arc with no pin yet, first call: resolver installs a pin against
    /// the active provider and returns `pinned_slug: None` (no override
    /// needed — the freshly-built router already uses the persisted slug
    /// for every tier).
    #[tokio::test]
    async fn first_call_installs_pin_returns_none_slug() {
        let store = setup_store().await;
        store
            .create_arc("arc_a", "t", ArcSource::UserInput)
            .await
            .unwrap();
        let cfg = mk_config(&[("opencode_go", "deepseek-v4-flash")]);

        let target = resolve_effective_provider_for_arc_with_config(
            Some(&store),
            "arc_a",
            "opencode_go",
            ModelProfile::Powerful,
            &cfg,
        )
        .await;

        assert_eq!(
            target,
            EffectiveProviderTarget {
                provider_id: "opencode_go".to_string(),
                pinned_slug: None,
            }
        );

        // The pin row should now exist with the resolved slug captured.
        let arc = store.get_arc("arc_a").await.unwrap().unwrap();
        assert_eq!(arc.pinned_provider_id.as_deref(), Some("opencode_go"));
        assert_eq!(arc.pinned_slug.as_deref(), Some("deepseek-v4-flash"));
    }

    /// Arc already pinned to a provider that's still in config:
    /// resolver returns both the pinned provider and the pinned slug,
    /// regardless of what the active provider currently is.
    #[tokio::test]
    async fn pinned_arc_returns_both_provider_and_slug() {
        let store = setup_store().await;
        store
            .create_arc("arc_p", "t", ArcSource::UserInput)
            .await
            .unwrap();
        store
            .set_pinned_provider_if_unset("arc_p", "opencode_go", "minimax-m2.7")
            .await
            .unwrap();
        // The pinned slug must still be a model the user has configured
        // for the connection, otherwise the resolver now (correctly)
        // drops the stale slug — so wire it as opencode_go's model here.
        let cfg = mk_config(&[
            ("opencode_go", "minimax-m2.7"),
            ("deepseek", "deepseek-v4-flash"),
        ]);

        // Even with a different "active" provider, the pin wins.
        let target = resolve_effective_provider_for_arc_with_config(
            Some(&store),
            "arc_p",
            "deepseek",
            ModelProfile::Powerful,
            &cfg,
        )
        .await;

        assert_eq!(
            target,
            EffectiveProviderTarget {
                provider_id: "opencode_go".to_string(),
                pinned_slug: Some("minimax-m2.7".to_string()),
            }
        );
    }

    /// Arc pinned to a provider that has since been removed from
    /// config: resolver falls back to the active provider AND clears
    /// the slug pin (we won't ship a foreign slug to a different
    /// provider — the wire format would almost certainly mismatch).
    #[tokio::test]
    async fn pinned_provider_missing_drops_slug() {
        let store = setup_store().await;
        store
            .create_arc("arc_x", "t", ArcSource::UserInput)
            .await
            .unwrap();
        store
            .set_pinned_provider_if_unset("arc_x", "deleted_provider", "kimi-k2.5")
            .await
            .unwrap();
        let cfg = mk_config(&[("deepseek", "deepseek-v4-flash")]);

        let target = resolve_effective_provider_for_arc_with_config(
            Some(&store),
            "arc_x",
            "deepseek",
            ModelProfile::Powerful,
            &cfg,
        )
        .await;

        assert_eq!(
            target,
            EffectiveProviderTarget {
                provider_id: "deepseek".to_string(),
                pinned_slug: None,
            }
        );
    }

    /// Arc pinned to a provider that still exists, but to a *model*
    /// the user has since removed from every Bundle / tier: the
    /// resolver keeps the provider pin (in-flight protection) but drops
    /// the stale slug so the connection's live tier resolution takes
    /// over. This is the wake-up "still reaching for a model I removed"
    /// fix — the slug pin must not outlive the model's presence in
    /// config.
    #[tokio::test]
    async fn pinned_slug_no_longer_configured_is_dropped() {
        let store = setup_store().await;
        store
            .create_arc("arc_s", "t", ArcSource::UserInput)
            .await
            .unwrap();
        store
            .set_pinned_provider_if_unset("arc_s", "opencode_go", "minimax-m2.7")
            .await
            .unwrap();
        // opencode_go survives, but its only configured model is now
        // deepseek-v4-flash — minimax-m2.7 is gone from config.
        let cfg = mk_config(&[("opencode_go", "deepseek-v4-flash")]);

        let target = resolve_effective_provider_for_arc_with_config(
            Some(&store),
            "arc_s",
            "opencode_go",
            ModelProfile::Powerful,
            &cfg,
        )
        .await;

        assert_eq!(
            target,
            EffectiveProviderTarget {
                provider_id: "opencode_go".to_string(),
                pinned_slug: None,
            }
        );
    }

    /// A stale slug that's still referenced by a Bundle tier for the
    /// same connection counts as live and is honoured — "removed" means
    /// "in no Bundle", not "not the requested tier's pick".
    #[tokio::test]
    async fn pinned_slug_referenced_by_a_bundle_is_kept() {
        let store = setup_store().await;
        store
            .create_arc("arc_b", "t", ArcSource::UserInput)
            .await
            .unwrap();
        store
            .set_pinned_provider_if_unset("arc_b", "opencode_go", "minimax-m2.7")
            .await
            .unwrap();
        // Connection's default_model is something else, but the ACTIVE
        // Bundle still points opencode_go at minimax-m2.7 → live.
        let mut cfg = mk_config(&[("opencode_go", "deepseek-v4-flash")]);
        install_active_bundle(
            &mut cfg,
            mk_bundle(
                "b1",
                &[(ModelProfile::Powerful, "opencode_go", "minimax-m2.7")],
            ),
        );

        let target = resolve_effective_provider_for_arc_with_config(
            Some(&store),
            "arc_b",
            "opencode_go",
            ModelProfile::Powerful,
            &cfg,
        )
        .await;

        assert_eq!(
            target,
            EffectiveProviderTarget {
                provider_id: "opencode_go".to_string(),
                pinned_slug: Some("minimax-m2.7".to_string()),
            }
        );
    }

    /// The exact reported bug: same Connection (opencode_go) for both,
    /// the active Bundle only has deepseek, but the Connection's
    /// `default_model` is minimax-m2.7 (a testing default). A wake-up
    /// arc pinned to minimax must NOT keep using it — Bundles are
    /// authoritative, so the stale slug is dropped and live tier
    /// resolution (deepseek) takes over.
    #[tokio::test]
    async fn pinned_slug_in_default_model_but_not_in_active_bundle_is_dropped() {
        let store = setup_store().await;
        store
            .create_arc("arc_bug", "t", ArcSource::UserInput)
            .await
            .unwrap();
        store
            .set_pinned_provider_if_unset("arc_bug", "opencode_go", "minimax-m2.7")
            .await
            .unwrap();
        // Connection default is minimax (testing default), but the
        // active Bundle only routes opencode_go to deepseek.
        let mut cfg = mk_config(&[("opencode_go", "minimax-m2.7")]);
        install_active_bundle(
            &mut cfg,
            mk_bundle(
                "main",
                &[
                    (ModelProfile::Powerful, "opencode_go", "deepseek-v4-pro"),
                    (ModelProfile::Judges, "opencode_go", "deepseek-v4-flash"),
                ],
            ),
        );

        let target = resolve_effective_provider_for_arc_with_config(
            Some(&store),
            "arc_bug",
            "opencode_go",
            ModelProfile::Powerful,
            &cfg,
        )
        .await;

        assert_eq!(
            target,
            EffectiveProviderTarget {
                provider_id: "opencode_go".to_string(),
                pinned_slug: None,
            }
        );
    }

    /// Arc with no `arc_store` available at all (CLI tests, etc.): the
    /// resolver collapses to the active provider with no slug pin.
    #[tokio::test]
    async fn no_arc_store_returns_active_only() {
        let cfg = mk_config(&[("openai", "gpt-5.4-mini")]);
        let target = resolve_effective_provider_for_arc_with_config(
            None,
            "arc_irrelevant",
            "openai",
            ModelProfile::Powerful,
            &cfg,
        )
        .await;

        assert_eq!(
            target,
            EffectiveProviderTarget {
                provider_id: "openai".to_string(),
                pinned_slug: None,
            }
        );
    }

    // ---- Bundles Phase 1b: active Bundle takes precedence over the
    // ---- legacy active_provider + tier_models path.

    fn mk_bundle(
        name: &str,
        tiers: &[(ModelProfile, &str, &str)], // (tier, connection_id, slug)
    ) -> athen_core::config::Bundle {
        let mut map: HashMap<ModelProfile, athen_core::config::BundleTier> = HashMap::new();
        for (tier, cid, slug) in tiers {
            map.insert(
                *tier,
                athen_core::config::BundleTier {
                    connection_id: (*cid).to_string(),
                    slug: (*slug).to_string(),
                },
            );
        }
        let now = chrono::Utc::now();
        athen_core::config::Bundle {
            id: uuid::Uuid::new_v4(),
            name: name.to_string(),
            created_at: now,
            updated_at: now,
            tiers: map,
        }
    }

    fn install_active_bundle(cfg: &mut AthenConfig, bundle: athen_core::config::Bundle) {
        let id = bundle.id.to_string();
        cfg.models.bundles.insert(id.clone(), bundle);
        cfg.models
            .assignments
            .insert(athen_core::config::ACTIVE_BUNDLE_KEY.to_string(), id);
    }

    /// Active Bundle present + tier filled: resolver pins the bundle's
    /// `(connection_id, slug)` *regardless* of what `active_provider_id`
    /// the caller passed. This is the load-bearing cross-vendor mixing
    /// guarantee.
    #[tokio::test]
    async fn bundle_tier_overrides_active_provider() {
        let store = setup_store().await;
        store
            .create_arc("arc_b", "t", ArcSource::UserInput)
            .await
            .unwrap();
        let mut cfg = mk_config(&[
            ("opencode_go", "deepseek-v4-flash"),
            ("anthropic", "claude-sonnet-4-6"),
        ]);
        // Bundle picks Anthropic for Code even though caller's "active"
        // provider is OpenCode Go.
        install_active_bundle(
            &mut cfg,
            mk_bundle(
                "Default",
                &[(ModelProfile::Code, "anthropic", "claude-opus-4-7")],
            ),
        );

        let target = resolve_effective_provider_for_arc_with_config(
            Some(&store),
            "arc_b",
            "opencode_go",
            ModelProfile::Code,
            &cfg,
        )
        .await;

        assert_eq!(
            target,
            EffectiveProviderTarget {
                provider_id: "anthropic".to_string(),
                pinned_slug: None,
            }
        );

        // Pin row captured the Bundle's pick.
        let arc = store.get_arc("arc_b").await.unwrap().unwrap();
        assert_eq!(arc.pinned_provider_id.as_deref(), Some("anthropic"));
        assert_eq!(arc.pinned_slug.as_deref(), Some("claude-opus-4-7"));
    }

    /// Sparse Bundle (only Cheap set): Powerful tier falls through to
    /// Fast, then to Cheap.
    #[tokio::test]
    async fn sparse_bundle_falls_through_to_cheap() {
        let store = setup_store().await;
        store
            .create_arc("arc_sparse", "t", ArcSource::UserInput)
            .await
            .unwrap();
        let mut cfg = mk_config(&[("deepseek", "deepseek-chat")]);
        install_active_bundle(
            &mut cfg,
            mk_bundle(
                "Cheap-only",
                &[(ModelProfile::Judges, "deepseek", "deepseek-v4-flash")],
            ),
        );

        let target = resolve_effective_provider_for_arc_with_config(
            Some(&store),
            "arc_sparse",
            "deepseek",
            ModelProfile::Powerful,
            &cfg,
        )
        .await;

        let arc = store.get_arc("arc_sparse").await.unwrap().unwrap();
        assert_eq!(arc.pinned_provider_id.as_deref(), Some("deepseek"));
        assert_eq!(arc.pinned_slug.as_deref(), Some("deepseek-v4-flash"));
        assert_eq!(target.provider_id, "deepseek");
    }

    /// Bundle exists but points at a Connection that was deleted from
    /// `models.providers`. Resolver must NOT pin a dead provider — it
    /// falls back to the legacy `active_provider + tier_models` path.
    #[tokio::test]
    async fn bundle_pointing_at_deleted_connection_falls_back() {
        let store = setup_store().await;
        store
            .create_arc("arc_dead", "t", ArcSource::UserInput)
            .await
            .unwrap();
        let mut cfg = mk_config(&[("deepseek", "deepseek-chat")]);
        // Bundle references "ghost" which is not in cfg.models.providers.
        install_active_bundle(
            &mut cfg,
            mk_bundle(
                "Broken",
                &[(ModelProfile::Powerful, "ghost", "ghost-model")],
            ),
        );

        let target = resolve_effective_provider_for_arc_with_config(
            Some(&store),
            "arc_dead",
            "deepseek",
            ModelProfile::Powerful,
            &cfg,
        )
        .await;

        assert_eq!(target.provider_id, "deepseek");
        let arc = store.get_arc("arc_dead").await.unwrap().unwrap();
        assert_eq!(arc.pinned_provider_id.as_deref(), Some("deepseek"));
        assert_eq!(arc.pinned_slug.as_deref(), Some("deepseek-chat"));
    }

    /// No active Bundle assignment: behaviour matches pre-Bundles
    /// (legacy `active_provider + default_model` path). Existing users
    /// without a migration must observe zero change.
    #[tokio::test]
    async fn no_active_bundle_uses_legacy_path() {
        let store = setup_store().await;
        store
            .create_arc("arc_legacy", "t", ArcSource::UserInput)
            .await
            .unwrap();
        let cfg = mk_config(&[("deepseek", "deepseek-chat")]);
        assert!(cfg.models.bundles.is_empty());

        let target = resolve_effective_provider_for_arc_with_config(
            Some(&store),
            "arc_legacy",
            "deepseek",
            ModelProfile::Fast,
            &cfg,
        )
        .await;

        assert_eq!(target.provider_id, "deepseek");
        let arc = store.get_arc("arc_legacy").await.unwrap().unwrap();
        assert_eq!(arc.pinned_slug.as_deref(), Some("deepseek-chat"));
    }
}

#[cfg(test)]
mod build_router_override_slug_tests {
    use super::build_router_for_provider;
    use athen_core::llm::{ModelFamily, ModelProfile};
    use std::collections::HashMap;

    /// `override_slug=Some(s)` collapses every tier to a single
    /// provider key derived from `s`, even when `tier_models` would
    /// pick something else per tier. This is the load-bearing slug
    /// freeze the arc pin relies on.
    #[test]
    fn override_slug_collapses_every_tier_to_pinned_key() {
        // Tier map says Cheap=A, Fast=B, Code=C, Powerful=D — would
        // pick a different slug per tier under normal routing.
        let mut tiers: HashMap<ModelProfile, String> = HashMap::new();
        tiers.insert(ModelProfile::Judges, "cheap-slug".into());
        tiers.insert(ModelProfile::Fast, "fast-slug".into());
        tiers.insert(ModelProfile::Code, "code-slug".into());
        tiers.insert(ModelProfile::Powerful, "powerful-slug".into());

        let router = build_router_for_provider(
            "openai", // OpenAI-compatible adapter doesn't make network calls at build time.
            "http://localhost:0",
            "default-model",
            Some("test-key"),
            false,
            false,
            ModelFamily::Default,
            &tiers,
            Some("kimi-k2.5"),
        );

        let expected = vec!["openai.kimi-k2.5".to_string()];
        // Every tier collapses to the pinned slug's single provider key.
        for tier in [
            ModelProfile::Judges,
            ModelProfile::Fast,
            ModelProfile::Code,
            ModelProfile::Powerful,
        ] {
            assert_eq!(
                router.profile_provider_keys(tier),
                expected.as_slice(),
                "tier {:?} should map to the pinned slug's key",
                tier
            );
        }
    }

    /// `override_slug=None` preserves the existing tier_models routing:
    /// each tier maps to whichever slug-keyed provider its tier_models
    /// entry names, with `default_model` filling in for unset tiers.
    #[test]
    fn no_override_preserves_tier_models_routing() {
        let mut tiers: HashMap<ModelProfile, String> = HashMap::new();
        tiers.insert(ModelProfile::Judges, "cheap-slug".into());
        tiers.insert(ModelProfile::Powerful, "powerful-slug".into());
        // Fast + Code are unset → fall back to default_model.

        let router = build_router_for_provider(
            "openai",
            "http://localhost:0",
            "default-model",
            Some("test-key"),
            false,
            false,
            ModelFamily::Default,
            &tiers,
            None,
        );

        assert_eq!(
            router.profile_provider_keys(ModelProfile::Judges),
            &["openai.cheap-slug".to_string()]
        );
        assert_eq!(
            router.profile_provider_keys(ModelProfile::Powerful),
            &["openai.powerful-slug".to_string()]
        );
        // Unset tiers fall through to default_model.
        assert_eq!(
            router.profile_provider_keys(ModelProfile::Fast),
            &["openai.default-model".to_string()]
        );
        assert_eq!(
            router.profile_provider_keys(ModelProfile::Code),
            &["openai.default-model".to_string()]
        );
    }

    /// `override_slug=Some("")` is treated as no override — a stale
    /// empty `pinned_slug` column must not blank-out the slug at
    /// request time (would route to provider key `openai.` and 404).
    #[test]
    fn empty_override_slug_treated_as_none() {
        let mut tiers: HashMap<ModelProfile, String> = HashMap::new();
        tiers.insert(ModelProfile::Powerful, "powerful-slug".into());

        let router = build_router_for_provider(
            "openai",
            "http://localhost:0",
            "default-model",
            Some("test-key"),
            false,
            false,
            ModelFamily::Default,
            &tiers,
            Some(""),
        );

        // Powerful tier still consults tier_models, not the empty pin.
        assert_eq!(
            router.profile_provider_keys(ModelProfile::Powerful),
            &["openai.powerful-slug".to_string()]
        );
        // Unset tier falls through to default_model.
        assert_eq!(
            router.profile_provider_keys(ModelProfile::Fast),
            &["openai.default-model".to_string()]
        );
    }
}
