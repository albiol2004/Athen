//! Application state management.
//!
//! Builds the coordinator, LLM router, and risk evaluator, wiring them
//! together as the composition root for the Athen desktop app.
//! Configuration is loaded from TOML files (`~/.athen/` or `./config/`)
//! with environment variable overrides.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use uuid::Uuid;

use async_trait::async_trait;
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

/// Top-level application state managed by Tauri.
pub struct AppState {
    pub coordinator: Arc<Coordinator>,
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
    /// Shutdown sender for the email monitor background task.
    pub email_shutdown: Option<tokio::sync::broadcast::Sender<()>>,
    /// Shutdown sender for the Telegram monitor background task.
    pub telegram_shutdown: Option<tokio::sync::broadcast::Sender<()>>,
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
    pub notifier: Option<Arc<NotificationOrchestrator>>,
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
    pub profile_embedder: Arc<dyn athen_core::traits::embedding::EmbeddingProvider>,
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
    pub web_search: Arc<dyn WebSearchProvider>,
    /// SMTP outbound. Built from `config.email` when SMTP fields are
    /// populated. `None` means the `email_send` tool will refuse with a
    /// "not configured" error until the user wires SMTP via Settings.
    pub email_sender: Option<Arc<dyn athen_core::traits::email_sender::EmailSender>>,
    /// Outbound Telegram. Built from `config.telegram` when the bot
    /// token is populated. The bot's owner chat (from `owner_user_id`)
    /// is the default destination — `send_telegram` calls without an
    /// explicit `chat_id` go there and skip the approval gate.
    /// `None` means the `send_telegram` tool will refuse with a "not
    /// configured" error until the user wires the bot via Settings.
    pub telegram_sender: Option<Arc<dyn athen_core::traits::telegram_sender::TelegramSender>>,
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
pub(crate) async fn assemble_app_tool_registry(
    deps: ToolRegistryDeps,
    arc_id: &str,
    app_handle: Option<tauri::AppHandle>,
) -> Box<dyn athen_core::traits::tool::ToolRegistry> {
    let delegation_app_handle = app_handle.clone();
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
        .with_checkpoint_store_opt(deps.checkpoint_store.clone())
        .with_checkpoint_arc_id(arc_id);
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
            ),
        ));
        let gate: Arc<dyn athen_agent::EmailSendApprovalGate> =
            Arc::new(crate::email_gate::RouterEmailApprovalGate::new(
                router.clone(),
                Some(arc_id.to_string()),
            ));
        shell = shell.with_email_approval(gate);
        let tg_gate: Arc<dyn athen_agent::tools::TelegramSendApprovalGate> = Arc::new(
            crate::email_gate::RouterTelegramApprovalGate::new(router, Some(arc_id.to_string())),
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
    if let Some(grants) = deps.grant_store.clone() {
        let mut gate = crate::file_gate::FileGate::new(
            arc_id.to_string(),
            grants,
            deps.pending_grants.clone(),
            app_handle,
        );
        if let Some(ref sink) = deps.telegram_approval_sink {
            gate = gate.with_telegram_approval(sink.clone());
        }
        registry = registry.with_file_gate(Arc::new(gate));
    }

    // Wrap the registry with the delegation layer when a profile store
    // is available. The wrapped registry exposes `delegate_to_agent`
    // on top of every other tool. Sub-agents spawned via that tool
    // receive the bare AppToolRegistry — no delegate_to_agent — which
    // is how depth=1 is enforced.
    let base: Arc<dyn athen_core::traits::tool::ToolRegistry> = Arc::new(registry);
    let with_delegation: Box<dyn athen_core::traits::tool::ToolRegistry> =
        if let (Some(profile_store), Some(arc_store)) =
            (deps.profile_store.clone(), deps.arc_store.clone())
        {
            let ctx = crate::delegation::DelegationContext {
                profile_store,
                identity_store: deps.identity_store.clone(),
                skill_store: deps.skill_store.clone(),
                http_endpoint_store: deps.http_endpoint_store.clone(),
                arc_store,
                llm_router: Arc::clone(&deps.router),
                parent_arc_id: arc_id.to_string(),
                tool_doc_dir: deps.tool_doc_dir.clone(),
                app_handle: delegation_app_handle,
                wakeup_restrictions: None,
            };
            Box::new(crate::delegation::DelegationToolRegistry::new(base, ctx))
        } else {
            Box::new(crate::delegation::ArcRegistryAdapter(base))
        };

    // Wrap with the wake-up authoring layer so the agent can call
    // `create_wakeup` to schedule its own follow-ups. Sits OUTSIDE
    // delegation so a wake-up declaring `delegate_to_agent` in its
    // allowlist still works; sits INSIDE the wake-up restriction
    // wrapper (which the firing path adds in commands.rs) so a
    // locked-down wake-up's tool_allowlist can hide create_wakeup.
    // Skipped when no wakeup_store is wired (CLI / test builds).
    if let Some(store) = deps.wakeup_store.clone() {
        let ctx = crate::wakeup_tool::WakeupToolContext {
            wakeup_store: store,
            approval_router: deps.approval_router.clone(),
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

        // Build persistent memory (vector search + knowledge graph).
        let memory = build_memory(&router, &config.embeddings).await;

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
        // Build an embedding router for profile routing. Same shape as the
        // memory subsystem's embedder: real providers can be wired later
        // from settings; until then it falls back to keyword embeddings,
        // which still produce a usable cosine signal across short strings.
        let profile_embedder: Arc<dyn athen_core::traits::embedding::EmbeddingProvider> =
            Arc::new(build_embedding_router(&config.embeddings));
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
            router,
            active_provider_id: Mutex::new(active_id),
            history: Mutex::new(history),
            pending_message: Mutex::new(None),
            pending_upload_event_id: Mutex::new(None),
            model_name: Mutex::new(model_name),
            active_arc_id: Mutex::new(active_arc_id),
            arc_store,
            calendar_store,
            trust_manager: contact_store
                .as_ref()
                .map(|cs| TrustManager::new(Box::new(cs.clone()))),
            contact_store,
            _database: database,
            cancel_flag: Arc::new(AtomicBool::new(false)),
            pending_user_inputs: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            email_shutdown: None,
            telegram_shutdown: None,
            calendar_shutdown: None,
            calendar_sync_shutdown: None,
            attachment_purger_shutdown: None,
            notifier: None,
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
            skill_store,
            wakeup_store,
            wakeup_scheduler_shutdown: std::sync::Mutex::new(None),
            profile_embedder,
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
            web_search,
            email_sender,
            telegram_sender,
            vault,
            github_identity_resolver,
            http_endpoint_store,
            http_rate_limiter,
            http_client,
            cloud_apis_doc_path,
            agent_run_store,
            agent_registry: None,
            checkpoint_store,
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
        pulse!(self.email_shutdown.as_ref().cloned(), "email monitor");
        pulse!(self.telegram_shutdown.as_ref().cloned(), "telegram monitor");
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
        if let Ok(mut guard) = self.wakeup_scheduler_shutdown.lock() {
            if let Some(tx) = guard.take() {
                if tx.send(()).is_err() {
                    tracing::debug!("wakeup scheduler shutdown signal had no listener");
                }
            }
        }

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
            config = tauri::async_runtime::block_on(async move {
                let mut c = cfg;
                crate::vault_creds::hydrate_secrets_from_vault(Some(&vault), &mut c).await;
                c
            });
        }
        config
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
            .with_web_search(self.web_search.clone())
            .with_email_sender_opt(self.email_sender.clone())
            .with_telegram_sender_opt(self.telegram_sender.clone())
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
            web_search: self.web_search.clone(),
            email_sender: self.email_sender.clone(),
            telegram_sender: self.telegram_sender.clone(),
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
        app_handle: Option<tauri::AppHandle>,
    ) -> Box<dyn athen_core::traits::tool::ToolRegistry> {
        assemble_app_tool_registry(self.tool_registry_deps(), arc_id, app_handle).await
    }

    /// Initialize the notification orchestrator.
    ///
    /// Must be called after `AppState::new()` but before `app.manage()`,
    /// because it needs the Tauri `AppHandle` to create the `InAppChannel`.
    /// Channels are built from the current config: InApp is always added,
    /// Telegram is added only if the bot is configured with an owner.
    pub fn init_notifier(&mut self, app_handle: tauri::AppHandle) {
        let config = self.load_hydrated_config_sync();
        let mut channels: Vec<Box<dyn NotificationChannel>> = Vec::new();

        // InApp is always available.
        channels.push(Box::new(InAppChannel::new(app_handle)));

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

        let notifier = Arc::new(orchestrator);

        // Load persisted notifications from a previous session.
        tauri::async_runtime::block_on(notifier.load_persisted());

        self.notifier = Some(notifier);
    }

    /// Initialize the approval router and its sinks.
    ///
    /// Must be called after `AppState::new()` but before `app.manage()`,
    /// because it needs the Tauri `AppHandle` to create the `InApp`
    /// sink. The router is wired against the existing arc store so it
    /// can pick the right channel based on each arc's
    /// `primary_reply_channel` (or its source as a fallback).
    pub fn init_approval_router(&mut self, app_handle: tauri::AppHandle) {
        use crate::approval::{ApprovalRouter, InAppApprovalSink, TelegramApprovalSink};
        use athen_core::traits::approval::ApprovalSink;

        let config = self.load_hydrated_config_sync();

        let inapp = Arc::new(InAppApprovalSink::new(app_handle));
        let mut sinks: Vec<Arc<dyn ApprovalSink>> = vec![inapp.clone() as Arc<dyn ApprovalSink>];

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
        self.inapp_approval_sink = Some(inapp);
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
    pub fn init_agent_registry(&mut self, app_handle: tauri::AppHandle) {
        let registry =
            crate::agent_registry::AgentRegistry::new(app_handle, self.agent_run_store.clone());
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
    pub fn start_email_monitor(&mut self, app_handle: tauri::AppHandle) {
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

        let (shutdown_tx, shutdown_rx) = tokio::sync::broadcast::channel::<()>(1);
        self.email_shutdown = Some(shutdown_tx);

        let mut monitor = EmailMonitor::new();
        if let Some(lookup) = self.owner_lookup() {
            monitor = monitor.with_owner_lookup(lookup);
        }
        let email_config = config.clone();
        let router = Arc::clone(&self.router);
        let arc_store_ref = self._database.as_ref().map(|db| db.arc_store());
        let attachment_store_ref = self._database.as_ref().map(|db| db.attachment_store());
        let contact_store_ref = self.contact_store.clone();
        let profile_store_ref = self.profile_store.clone();
        let profile_embedder_ref = Arc::clone(&self.profile_embedder);
        let profile_embedding_cache_ref = Arc::clone(&self.profile_embedding_cache);
        let notifier = self.notifier.clone();
        let coordinator_ref = Arc::clone(&self.coordinator);
        let task_arc_map_ref = Arc::clone(&self.task_arc_map);
        let pending_email_marks_ref = Arc::clone(&self.pending_email_marks);
        let dispatch_signal_ref = Arc::clone(&self.dispatch_signal);
        let approval_router_ref = self.approval_router.clone();

        tauri::async_runtime::spawn(async move {
            if let Err(e) = monitor.init(&email_config).await {
                tracing::error!("Failed to initialize email monitor: {e}");
                return;
            }

            let poll_interval = monitor.poll_interval();
            info!("Email monitor started, polling every {:?}", poll_interval);

            let mut shutdown = shutdown_rx;
            loop {
                match monitor.poll().await {
                    Ok(events) if !events.is_empty() => {
                        info!("Email monitor received {} new event(s)", events.len());
                        for event in events {
                            crate::sense_router::process_sense_event(
                                &event,
                                &router,
                                &arc_store_ref,
                                &profile_store_ref,
                                &profile_embedder_ref,
                                &profile_embedding_cache_ref,
                                &app_handle,
                                notifier.as_ref(),
                                Some(&coordinator_ref),
                                Some(&task_arc_map_ref),
                                Some(&dispatch_signal_ref),
                                approval_router_ref.as_ref(),
                                Some(&pending_email_marks_ref),
                                attachment_store_ref.as_ref(),
                                contact_store_ref.as_ref(),
                                None,
                            )
                            .await;
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
                        break;
                    }
                }
            }

            if let Err(e) = monitor.shutdown().await {
                warn!("Email monitor shutdown error: {e}");
            }
            info!("Email monitor stopped");
        });
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

    /// Spawn the wake-up scheduler loop. Idempotent — does nothing if the
    /// store isn't wired or the loop is already running. Calls
    /// `arm_unscheduled(now)` first so freshly-created rows that lack a
    /// `next_fire_at` get armed before the first tick.
    pub fn start_wakeup_scheduler(&mut self, app_handle: tauri::AppHandle) {
        let Some(store) = self.wakeup_store.clone() else {
            tracing::debug!("No wake-up store wired; skipping scheduler");
            return;
        };
        if self
            .wakeup_scheduler_shutdown
            .lock()
            .ok()
            .map(|g| g.is_some())
            .unwrap_or(false)
        {
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
                Some(app_handle),
            ));
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        if let Ok(mut guard) = self.wakeup_scheduler_shutdown.lock() {
            *guard = Some(tx);
        }

        // Tick every 5 seconds. Fast enough that "remind me in 30 seconds"
        // feels prompt; slow enough that an idle laptop burns no real CPU.
        // Production-grade scheduling would key off the earliest
        // next_fire_at; for v1 a coarse poll is fine.
        let period = std::time::Duration::from_secs(5);
        // Tauri's setup hook is synchronous; use the Tauri-managed async
        // runtime for the same reason the email/calendar/telegram monitors
        // do — `tokio::spawn` here panics because no reactor is running on
        // this thread.
        tauri::async_runtime::spawn(async move {
            let scheduler = athen_scheduler::WakeupScheduler::new(store, sink);
            // Arm any rows that lack next_fire_at (created via Tauri
            // command without pre-computing the time).
            match scheduler.arm_unscheduled(chrono::Utc::now()).await {
                Ok(0) => {}
                Ok(n) => tracing::info!("Armed {n} fresh wake-up(s)"),
                Err(e) => tracing::warn!("Failed to arm fresh wake-ups: {e}"),
            }
            scheduler.run(period, rx).await;
            tracing::info!("Wake-up scheduler loop exited");
        });
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

    pub fn start_calendar_monitor(&mut self, app_handle: tauri::AppHandle) {
        use athen_core::traits::sense::SenseMonitor;
        use athen_sentidos::calendar::CalendarMonitor;

        let mut monitor = CalendarMonitor::new();
        let config = load_config();
        let router = Arc::clone(&self.router);
        let arc_store_ref = self._database.as_ref().map(|db| db.arc_store());
        let attachment_store_ref = self._database.as_ref().map(|db| db.attachment_store());
        let contact_store_ref = self.contact_store.clone();
        let profile_store_ref = self.profile_store.clone();
        let profile_embedder_ref = Arc::clone(&self.profile_embedder);
        let profile_embedding_cache_ref = Arc::clone(&self.profile_embedding_cache);
        let notifier = self.notifier.clone();
        let coordinator_ref = Arc::clone(&self.coordinator);
        let task_arc_map_ref = Arc::clone(&self.task_arc_map);
        let pending_email_marks_ref = Arc::clone(&self.pending_email_marks);
        let dispatch_signal_ref = Arc::clone(&self.dispatch_signal);
        let approval_router_ref = self.approval_router.clone();

        let (shutdown_tx, shutdown_rx) = tokio::sync::broadcast::channel::<()>(1);
        self.calendar_shutdown = Some(shutdown_tx);

        tauri::async_runtime::spawn(async move {
            if let Err(e) = monitor.init(&config).await {
                tracing::error!("Failed to initialize calendar monitor: {e}");
                return;
            }

            let poll_interval = monitor.poll_interval();
            info!(
                "Calendar monitor started, polling every {:?}",
                poll_interval
            );

            let mut shutdown = shutdown_rx;
            loop {
                // Select the sleep so a shutdown signal during the sleep
                // unblocks immediately. The poll itself isn't interruptible
                // here — if it ever became slow we'd want to wrap it too.
                tokio::select! {
                    _ = shutdown.recv() => {
                        info!("Calendar monitor shutdown signal received");
                        break;
                    }
                    _ = tokio::time::sleep(poll_interval) => {}
                }

                match monitor.poll().await {
                    Ok(events) if !events.is_empty() => {
                        info!("Calendar monitor: {} reminder(s)", events.len());
                        for event in events {
                            crate::sense_router::process_sense_event(
                                &event,
                                &router,
                                &arc_store_ref,
                                &profile_store_ref,
                                &profile_embedder_ref,
                                &profile_embedding_cache_ref,
                                &app_handle,
                                notifier.as_ref(),
                                Some(&coordinator_ref),
                                Some(&task_arc_map_ref),
                                Some(&dispatch_signal_ref),
                                approval_router_ref.as_ref(),
                                Some(&pending_email_marks_ref),
                                attachment_store_ref.as_ref(),
                                contact_store_ref.as_ref(),
                                None,
                            )
                            .await;
                        }
                    }
                    Ok(_) => {}
                    Err(e) => {
                        warn!("Calendar poll error: {e}");
                    }
                }
            }
            info!("Calendar monitor stopped");
        });
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
    pub fn start_telegram_monitor(&mut self, app_handle: tauri::AppHandle) {
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

        let (shutdown_tx, shutdown_rx) = tokio::sync::broadcast::channel::<()>(1);
        self.telegram_shutdown = Some(shutdown_tx);

        let mut monitor = TelegramMonitor::new(config.telegram.clone());
        if let Some(lookup) = self.owner_lookup() {
            monitor = monitor.with_owner_lookup(lookup);
        }
        let bot_token = config.telegram.bot_token.clone();
        let telegram_config = config.clone();
        let router = Arc::clone(&self.router);
        let arc_store_ref = self._database.as_ref().map(|db| db.arc_store());
        let attachment_store_ref = self._database.as_ref().map(|db| db.attachment_store());
        // Refs that the non-owner branch (process_sense_event) still
        // needs at the outer scope. Everything else the owner-Telegram
        // executor wants lives in `tool_registry_deps_ref` below.
        let profile_store_ref = self.profile_store.clone();
        let profile_embedder_ref = Arc::clone(&self.profile_embedder);
        let profile_embedding_cache_ref = Arc::clone(&self.profile_embedding_cache);
        let contact_store_ref = self.contact_store.clone();
        let notifier = self.notifier.clone();
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

        tauri::async_runtime::spawn(async move {
            if let Err(e) = monitor.init(&telegram_config).await {
                tracing::error!("Failed to initialize Telegram monitor: {e}");
                return;
            }

            let poll_interval = monitor.poll_interval();
            info!(
                "Telegram monitor started, polling every {:?}",
                poll_interval
            );

            let mut shutdown = shutdown_rx;
            loop {
                match monitor.poll().await {
                    Ok(events) if !events.is_empty() => {
                        info!("Telegram monitor received {} new event(s)", events.len());
                        for event in &events {
                            let is_owner = event.source_risk == athen_core::risk::RiskLevel::Safe;

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
                                        event.content.summary.as_deref().filter(|s| !s.is_empty())
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
                                let has_payload =
                                    !text.is_empty() || !event.content.attachments.is_empty();
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
                                    let app_handle_c = app_handle.clone();
                                    let notifier_c = notifier.clone();
                                    let profile_embedder_c = Arc::clone(&profile_embedder_ref);
                                    let profile_embedding_cache_c =
                                        Arc::clone(&profile_embedding_cache_ref);
                                    let agent_registry_c = agent_registry_ref.clone();
                                    let deps_c = tool_registry_deps_ref.clone();
                                    let event_id = event.id;
                                    let attachments_owned = event.content.attachments.clone();
                                    tauri::async_runtime::spawn(async move {
                                        execute_owner_telegram_message(
                                            &text_owned,
                                            chat_id,
                                            &bot_token_c,
                                            event_id,
                                            &attachments_owned,
                                            &app_handle_c,
                                            notifier_c.as_ref(),
                                            &profile_embedder_c,
                                            &profile_embedding_cache_c,
                                            agent_registry_c.as_ref(),
                                            deps_c,
                                        )
                                        .await;
                                    });
                                }
                            } else {
                                // Non-owner messages go through the full sense
                                // router: LLM triage, arc creation, notification,
                                // and (when triage says it's action-worthy) hand
                                // off to the coordinator for autonomous execution.
                                crate::sense_router::process_sense_event(
                                    event,
                                    &router,
                                    &arc_store_ref,
                                    &profile_store_ref,
                                    &profile_embedder_ref,
                                    &profile_embedding_cache_ref,
                                    &app_handle,
                                    notifier.as_ref(),
                                    Some(&coordinator_ref),
                                    Some(&task_arc_map_ref),
                                    Some(&dispatch_signal_ref),
                                    approval_router_ref.as_ref(),
                                    Some(&pending_email_marks_ref),
                                    attachment_store_ref.as_ref(),
                                    contact_store_ref.as_ref(),
                                    telegram_chat_log_ref.as_ref(),
                                )
                                .await;
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
                    if let Some(ref sink) = telegram_approval_sink {
                        for cb in callbacks {
                            let resolved = sink.resolve_callback(&cb.callback_id, &cb.data).await;
                            info!(
                                callback_id = %cb.callback_id,
                                data = %cb.data,
                                resolved,
                                "Telegram callback dispatched"
                            );
                        }
                    } else {
                        warn!(
                            count = callbacks.len(),
                            "Telegram callbacks dropped — no approval sink configured"
                        );
                    }
                }

                tokio::select! {
                    _ = tokio::time::sleep(poll_interval) => {}
                    _ = shutdown.recv() => {
                        info!("Telegram monitor shutdown signal received");
                        break;
                    }
                }
            }

            if let Err(e) = monitor.shutdown().await {
                warn!("Telegram monitor shutdown error: {e}");
            }
            info!("Telegram monitor stopped");
        });
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
    pub fn start_dispatch_loop(&mut self, app_handle: tauri::AppHandle) {
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
        let notifier = self.notifier.clone();
        let compactor = self.compactor.clone();
        let web_search = Arc::clone(&self.web_search);
        let email_sender = self.email_sender.clone();
        let telegram_sender_dispatch = self.telegram_sender.clone();
        let telegram_outbound_hint_dispatch = self.telegram_outbound_hint.clone();
        let telegram_chat_log_dispatch = self.telegram_chat_log.clone();
        let owner_check_dispatch = self.owner_destination_check();
        let github_identity_resolver_dispatch = self.github_identity_resolver.clone();
        let checkpoint_store_dispatch = self.checkpoint_store.clone();
        // Snapshot the vault so the per-task IMAP mark-seen flow can
        // hydrate the IMAP password from it (the password lives in the
        // vault for installs that have re-saved their email settings).
        let vault_snapshot = self.vault.clone();
        // Re-read the active provider id off `load_config()` per dispatched
        // task instead of cloning a one-shot snapshot here. New tasks see
        // a mid-session active-provider switch; in-flight arcs ride their
        // existing pin (see `docs/PROVIDER_PINNING.md`).
        let attachment_store_loop = self.attachment_store();
        let inflight = Arc::clone(&self.inflight_approvals);
        let agent_registry_loop = self.agent_registry.clone();

        tauri::async_runtime::spawn(async move {
            use athen_core::traits::coordinator::TaskQueue;
            info!("Autonomous dispatch loop started");
            loop {
                // Wait for the next wake-up trigger.
                tokio::select! {
                    _ = dispatch_signal.notified() => {}
                    _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {}
                    _ = shutdown_rx.recv() => {
                        info!("Dispatch loop shutdown signal received");
                        break;
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
                    // Resolve compaction budget per task. Re-reading the
                    // config TOML each dispatch is cheap (small file, only
                    // fires on user-driven sense events) and lets the user
                    // tune compaction without restarting the loop.
                    let cfg_for_resolvers = crate::state::load_config();
                    let active_id_now = crate::state::resolve_active_provider(&cfg_for_resolvers);
                    let effective_target = crate::state::resolve_effective_provider_for_arc(
                        arc_store.as_ref(),
                        &arc_id,
                        &active_id_now,
                        athen_core::llm::ModelProfile::Powerful,
                    )
                    .await;
                    let effective_provider_id = effective_target.provider_id.clone();
                    let (compaction_trigger_tokens, compaction_target_tokens) =
                        crate::compaction::resolve_compaction_budget(
                            &cfg_for_resolvers,
                            &effective_provider_id,
                        );
                    let sampling_temperature = crate::compaction::resolve_provider_temperature(
                        &cfg_for_resolvers,
                        &effective_provider_id,
                    );
                    let reasoning_effort =
                        crate::state::resolve_reasoning_effort_for_arc(arc_store.as_ref(), &arc_id)
                            .await;
                    // Per-arc router build: keeps the global router when
                    // no pin is in force, swaps in a slug-locked router
                    // when the arc has captured `(provider, slug)`. See
                    // `arc_router_for` and `docs/PROVIDER_PINNING.md`.
                    let arc_router = crate::state::arc_router_for(
                        &router,
                        &effective_target,
                        &active_id_now,
                        &cfg_for_resolvers,
                    );
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
                        app_handle: app_handle.clone(),
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
                    };

                    let task_arc_map_clone = Arc::clone(&task_arc_map);
                    let task_wakeup_map_clone = Arc::clone(&task_wakeup_map);
                    let pending_email_marks_clone = Arc::clone(&pending_email_marks);
                    let vault_snapshot = vault_snapshot.clone();
                    tauri::async_runtime::spawn(async move {
                        let outcome =
                            crate::commands::execute_dispatched_task(task, arc_id.clone(), ctx)
                                .await;

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
                        let mark_info = pending_email_marks_clone.write().await.remove(&task_id);
                        if let Some(info) = mark_info {
                            if succeeded {
                                let mut config = load_config();
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
            info!("Autonomous dispatch loop stopped");
        });
    }
}

// ---------------------------------------------------------------------------
// Owner Telegram auto-execution
// ---------------------------------------------------------------------------

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
    app_handle: &tauri::AppHandle,
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
    use tauri::Emitter;

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
            let hint_match = telegram_outbound_hint
                .lock()
                .ok()
                .and_then(|guard| guard.clone())
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
                warn!("Failed to list arcs for owner message: {e}");
                None
            }
        }
    } else {
        None
    };

    // If `/newarc` arrived with no follow-up text, the agent has nothing
    // to do — confirm the reset and return without spinning the executor.
    if force_new_arc && text.is_empty() {
        if let Err(e) = athen_sentidos::telegram::send_message(
            bot_token,
            chat_id,
            "📍 New arc started. Send your message.",
        )
        .await
        {
            warn!("Failed to send /newarc ack: {e}");
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
        Some(app_handle.clone()),
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
        app_handle.clone(),
        arc_store.clone(),
        target_arc_id.clone().unwrap_or_default(),
        turn_id.clone(),
        tool_log.clone(),
    )
    .with_telegram_progress(Arc::clone(&progress));
    if let Some(reg) = agent_registry {
        auditor = auditor.with_agent_tracking(Arc::clone(reg), task_id_for_run);
    }
    let stream_tx = spawn_stream_forwarder(app_handle, target_arc_id.clone());
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
        .enable_default_reminders(true)
        .default_temperature(sampling_temperature);
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
        let _ = app_handle.emit(
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
        let _ = app_handle.emit("arc-updated", serde_json::json!({ "arc_id": arc_id }));
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
    let names = match tool_log.lock() {
        Ok(g) => g.clone(),
        Err(_) => return String::new(),
    };
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
        profile: ModelProfile::Cheap,
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
            info!("Loading config from: {}", dir.display());
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
            let pinned_slug = arc.pinned_slug.clone().filter(|s| !s.is_empty());
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
fn pick_bundle_tier(bundle: &Bundle, tier: ModelProfile) -> Option<(String, String)> {
    let ladder: &[ModelProfile] = match tier {
        ModelProfile::Code => &[ModelProfile::Code, ModelProfile::Fast, ModelProfile::Cheap],
        ModelProfile::Powerful => &[
            ModelProfile::Powerful,
            ModelProfile::Fast,
            ModelProfile::Cheap,
        ],
        ModelProfile::Fast => &[ModelProfile::Fast, ModelProfile::Cheap],
        ModelProfile::Cheap => &[ModelProfile::Cheap],
        ModelProfile::Local => &[ModelProfile::Local],
    };
    for t in ladder {
        if let Some(bt) = bundle.tiers.get(t) {
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

/// Parse the wire form (`ModelProfile` variant name) of a stored tier
/// override into a `ModelProfile`. Returns `None` for unknown strings so
/// the resolver can warn and fall through rather than silently
/// substituting a wrong tier.
fn parse_model_profile_wire(s: &str) -> Option<ModelProfile> {
    match s.trim() {
        "Powerful" => Some(ModelProfile::Powerful),
        "Fast" => Some(ModelProfile::Fast),
        "Code" => Some(ModelProfile::Code),
        "Cheap" => Some(ModelProfile::Cheap),
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
pub(crate) fn arc_router_for(
    global_router: &Arc<tokio::sync::RwLock<Arc<DefaultLlmRouter>>>,
    target: &EffectiveProviderTarget,
    active_provider_id: &str,
    config: &AthenConfig,
) -> Arc<tokio::sync::RwLock<Arc<DefaultLlmRouter>>> {
    // No pin in force AND no provider switch ⇒ keep using the shared
    // global router. This is the fast path on the very first call of an
    // arc (resolver returns `pinned_slug: None` immediately after
    // installing the pin — see `resolve_effective_provider_for_arc`)
    // and on every call of an arc that was never pinned.
    if target.pinned_slug.is_none() && target.provider_id == active_provider_id {
        return Arc::clone(global_router);
    }
    let (router, _model) = build_router_for_provider_from_config_with_pinned_slug(
        &target.provider_id,
        config,
        target.pinned_slug.as_deref(),
    );
    Arc::new(tokio::sync::RwLock::new(router))
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
/// - `Automatic` → opportunistic: build providers from any populated
///   field, with Ollama probed first via `is_available`. If nothing
///   configured, falls through to keyword.
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
            info!(
                provider_count = providers.len(),
                "Embeddings: Automatic — Ollama first then keyword"
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
        ModelProfile::Cheap,
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
        ModelProfile::Cheap,
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
        ModelProfile::Cheap,
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
        "anthropic" | "minimax_anthropic" => {
            // minimax_anthropic routes through AnthropicProvider for the
            // /v1/messages wire format. MiniMax Token Plan exposes it at
            // api.minimax.io/anthropic (with prompt-cache). Provider
            // adapter is identical; only the base URL differs.
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
    embeddings: &athen_core::config::EmbeddingConfig,
) -> Option<Arc<Memory>> {
    use athen_memory::extractor::LlmEntityExtractor;
    use athen_memory::sqlite::{SqliteGraph, SqliteVectorIndex};

    let data_dir = ensure_data_dir()?;
    let db_path = data_dir.join("athen.db");

    // Open a separate rusqlite connection for the memory subsystem.
    // Memory uses std::sync::Mutex while Database uses tokio::sync::Mutex,
    // so they cannot share a connection. SQLite handles concurrent access
    // from multiple connections to the same file safely.
    let conn = match rusqlite::Connection::open(&db_path) {
        Ok(c) => {
            // Enable WAL mode for better concurrent access.
            let _ = c.execute_batch("PRAGMA journal_mode=WAL;");
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
    let graph = match SqliteGraph::new(conn) {
        Ok(g) => g,
        Err(e) => {
            warn!("Failed to create knowledge graph: {e}");
            return None;
        }
    };

    // Build the embedding router from config — when no neural provider
    // is configured this collapses to the keyword fallback inside
    // `EmbeddingRouter::resolve`.
    let embedding_router = build_embedding_router(embeddings);
    // LLM entity extractor for automatic knowledge graph population.
    let extractor_router: Box<dyn LlmRouter> = Box::new(SharedRouter(Arc::clone(router)));
    let extractor = LlmEntityExtractor::new(extractor_router);

    let memory = Memory::new(Box::new(vector), Box::new(graph))
        .with_embedder(Box::new(embedding_router))
        .with_extractor(Box::new(extractor))
        .with_min_score(0.6);

    info!("Memory system initialized with SQLite persistence");
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
        assert_eq!(tier, ModelProfile::Cheap);
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

        let tier = resolve_effective_tier_for_arc(
            Some(&store),
            "arc4",
            Some(ComplexityTag::Low),
            false,
            ModelProfile::Fast,
        )
        .await;
        assert_eq!(tier, ModelProfile::Cheap);
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
        let cfg = mk_config(&[
            ("opencode_go", "deepseek-v4-flash"),
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
                &[(ModelProfile::Cheap, "deepseek", "deepseek-v4-flash")],
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
        tiers.insert(ModelProfile::Cheap, "cheap-slug".into());
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
            ModelProfile::Cheap,
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
        tiers.insert(ModelProfile::Cheap, "cheap-slug".into());
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
            router.profile_provider_keys(ModelProfile::Cheap),
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
