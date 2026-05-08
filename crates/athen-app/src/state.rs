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
use athen_core::config::{AthenConfig, AuthType, ProfileConfig};
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
use athen_llm::providers::llamacpp::LlamaCppProvider;
use athen_llm::providers::ollama::OllamaProvider;
use athen_llm::providers::openai::OpenAiCompatibleProvider;
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
    /// Shutdown sender for the email monitor background task.
    pub email_shutdown: Option<tokio::sync::broadcast::Sender<()>>,
    /// Shutdown sender for the Telegram monitor background task.
    pub telegram_shutdown: Option<tokio::sync::broadcast::Sender<()>>,
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
    // TODO: kill spawned on shutdown — Tauri exposes only a sync window
    // event hook here, not an async one suitable for awaiting our locks.
    // Workaround: process group leadership + parent-death signal would be
    // a cleaner OS-level fix than wiring a custom Drop.
    pub spawned_processes: athen_agent::SpawnedProcessMap,
    /// Single-slot hint recording the most recent outbound Telegram
    /// notification's arc + timestamp. Written by `TelegramChannel::send`,
    /// read by `execute_owner_telegram_message` to bias arc matching for
    /// short follow-ups that arrive right after a notification fires.
    /// See `docs/MULTI_INTENT_ROUTING.md` for the multi-arc extension.
    pub telegram_outbound_hint: crate::notifier::TelegramOutboundHint,
    /// Approvals currently being executed. See [`InflightApprovals`].
    pub inflight_approvals: InflightApprovals,
    /// Maps coordinator task ids to the arc the originating sense
    /// event landed in. Populated by `sense_router::process_sense_event`
    /// when it dispatches an autonomous task; consumed by the dispatch
    /// loop to persist replies into the right arc.
    pub task_arc_map: TaskArcMap,
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
        let config = load_config();

        // Determine which provider to activate on startup.
        let active_id = resolve_active_provider(&config);
        let (router, model_name) = build_router_for_provider_from_config(&active_id, &config);

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
        let memory = build_memory(&router).await;

        // Build the MCP registry and load persisted enabled state.
        let mcp = Arc::new(McpRegistry::new());
        let mcp_store = database.as_ref().map(|db| db.mcp_store());
        if let Some(ref store) = mcp_store {
            if let Err(e) = restore_enabled_mcps(&mcp, store).await {
                warn!("Failed to restore enabled MCPs: {e}");
            }
        }

        let tool_doc_dir = ensure_data_dir().map(|d| d.join("tools"));

        let grant_store = database.as_ref().map(|db| Arc::new(db.grant_store()));
        let profile_store = database.as_ref().map(|db| Arc::new(db.profile_store()));
        // Build an embedding router for profile routing. Same shape as the
        // memory subsystem's embedder: real providers can be wired later
        // from settings; until then it falls back to keyword embeddings,
        // which still produce a usable cosine signal across short strings.
        let profile_embedder: Arc<dyn athen_core::traits::embedding::EmbeddingProvider> =
            Arc::new(athen_llm::embeddings::router::EmbeddingRouter::new(vec![]));
        let profile_embedding_cache = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
        let pending_grants = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let spawned_processes: athen_agent::SpawnedProcessMap =
            Arc::new(Mutex::new(HashMap::new()));

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

        let state = Self {
            coordinator: Arc::new(coordinator),
            router,
            active_provider_id: Mutex::new(active_id),
            history: Mutex::new(history),
            pending_message: Mutex::new(None),
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
            email_shutdown: None,
            telegram_shutdown: None,
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
            profile_embedder,
            profile_embedding_cache,
            pending_grants,
            spawned_processes,
            telegram_outbound_hint: std::sync::Arc::new(std::sync::Mutex::new(None)),
            inflight_approvals: Arc::new(Mutex::new(HashSet::new())),
            task_arc_map: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            pending_email_marks: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            dispatch_signal: Arc::new(tokio::sync::Notify::new()),
            dispatch_loop_shutdown: None,
            compactor,
            web_search,
            email_sender,
        };

        if let Err(e) = state.refresh_tools_doc().await {
            warn!("Failed to write initial per-group tool docs: {e}");
        }

        state
    }

    /// Borrow the attachment-ref store backed by the same SQLite
    /// connection as `arc_store`. Returns `None` when no database is
    /// wired (CLI/test builds). The store is cloneable (it's an
    /// `Arc<Mutex<Connection>>` wrapper), so callers can move the value
    /// freely into background tasks.
    pub fn attachment_store(&self) -> Option<athen_persistence::attachments::AttachmentStore> {
        self._database.as_ref().map(|db| db.attachment_store())
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
            .with_web_search(self.web_search.clone())
            .with_email_sender_opt(self.email_sender.clone());
        if let Some(router) = self.approval_router.clone() {
            shell_registry = shell_registry.with_toolbox_approval(Arc::new(
                crate::file_gate::RouterToolboxApprovalGate::new(router.clone(), None),
            ));
            let gate: Arc<dyn athen_agent::EmailSendApprovalGate> = Arc::new(
                crate::email_gate::RouterEmailApprovalGate::new(router, None),
            );
            shell_registry = shell_registry.with_email_approval(gate);
        }
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

    /// Build a per-arc tool registry wired with the file-permission gate
    /// and the shell sandbox grant provider. Called from every code path
    /// that constructs an executor so the agent always sees the same
    /// permission picture.
    pub async fn build_tool_registry(
        &self,
        arc_id: &str,
        app_handle: Option<tauri::AppHandle>,
    ) -> Box<dyn athen_core::traits::tool::ToolRegistry> {
        let delegation_app_handle = app_handle.clone();
        let mut shell = athen_agent::ShellToolRegistry::new()
            .await
            .with_spawned_processes(self.spawned_processes.clone())
            .with_web_search(self.web_search.clone())
            .with_email_sender_opt(self.email_sender.clone());
        if let Some(store) = self.grant_store.clone() {
            let provider = Arc::new(crate::file_gate::ArcWritableProvider {
                arc_id: crate::file_gate::arc_uuid(arc_id),
                store,
            });
            shell = shell.with_extra_writable(provider);
        }
        if let Some(router) = self.approval_router.clone() {
            shell = shell.with_toolbox_approval(Arc::new(
                crate::file_gate::RouterToolboxApprovalGate::new(
                    router.clone(),
                    Some(arc_id.to_string()),
                ),
            ));
            let gate: Arc<dyn athen_agent::EmailSendApprovalGate> = Arc::new(
                crate::email_gate::RouterEmailApprovalGate::new(router, Some(arc_id.to_string())),
            );
            shell = shell.with_email_approval(gate);
        }
        let mut registry = crate::app_tools::AppToolRegistry::new(
            shell,
            self.calendar_store.clone(),
            self.contact_store.clone(),
            self.memory.clone(),
        )
        .with_mcp(self.mcp.clone() as Arc<dyn athen_core::traits::mcp::McpClient>);
        if let Some(astore) = self.attachment_store() {
            registry = registry.with_attachments(astore);
        }
        if let Some(grants) = self.grant_store.clone() {
            let mut gate = crate::file_gate::FileGate::new(
                arc_id.to_string(),
                grants,
                self.pending_grants.clone(),
                app_handle,
            );
            // Attach the Telegram approval sink so file-permission
            // prompts also surface on Telegram alongside the in-app
            // card; whichever channel responds first wins.
            if let Some(ref sink) = self.telegram_approval_sink {
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
        if let (Some(profile_store), Some(arc_store)) = (
            self.profile_store.clone(),
            self._database.as_ref().map(|db| db.arc_store()),
        ) {
            let ctx = crate::delegation::DelegationContext {
                profile_store,
                arc_store,
                llm_router: Arc::clone(&self.router),
                parent_arc_id: arc_id.to_string(),
                tool_doc_dir: self.tool_doc_dir.clone(),
                app_handle: delegation_app_handle,
            };
            Box::new(crate::delegation::DelegationToolRegistry::new(base, ctx))
        } else {
            Box::new(crate::delegation::ArcRegistryAdapter(base))
        }
    }

    /// Initialize the notification orchestrator.
    ///
    /// Must be called after `AppState::new()` but before `app.manage()`,
    /// because it needs the Tauri `AppHandle` to create the `InAppChannel`.
    /// Channels are built from the current config: InApp is always added,
    /// Telegram is added only if the bot is configured with an owner.
    pub fn init_notifier(&mut self, app_handle: tauri::AppHandle) {
        let config = load_config();
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

        let config = load_config();

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

        self.approval_router = Some(Arc::new(router));
        self.inapp_approval_sink = Some(inapp);
        self.telegram_approval_sink = telegram_sink;

        info!(
            "Approval router initialized (escalation after {}s)",
            escalation_secs
        );
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

        let config = load_config();
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
        let email_config = config.clone();
        let router = Arc::clone(&self.router);
        let arc_store_ref = self._database.as_ref().map(|db| db.arc_store());
        let attachment_store_ref = self._database.as_ref().map(|db| db.attachment_store());
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
    pub fn start_attachment_purger(&self) {
        let Some(store) = self.attachment_store() else {
            tracing::debug!("No attachment store wired; skipping TTL purger");
            return;
        };
        let cfg = crate::settings::load_main_config_public();
        let ttl_days = cfg.attachment_policy.byte_ttl_days;
        // JoinHandle deliberately dropped — the loop runs until the
        // process exits, same as the calendar/email/telegram monitors.
        drop(crate::attachment_purger::spawn_loop(
            store,
            ttl_days,
            crate::attachment_purger::DEFAULT_SWEEP_INTERVAL,
        ));
    }

    pub fn start_calendar_monitor(&mut self, app_handle: tauri::AppHandle) {
        use athen_core::traits::sense::SenseMonitor;
        use athen_sentidos::calendar::CalendarMonitor;

        let mut monitor = CalendarMonitor::new();
        let config = load_config();
        let router = Arc::clone(&self.router);
        let arc_store_ref = self._database.as_ref().map(|db| db.arc_store());
        let attachment_store_ref = self._database.as_ref().map(|db| db.attachment_store());
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
                            )
                            .await;
                        }
                    }
                    Ok(_) => {}
                    Err(e) => {
                        warn!("Calendar poll error: {e}");
                    }
                }

                tokio::time::sleep(poll_interval).await;
            }
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

        let config = load_config();
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
        let bot_token = config.telegram.bot_token.clone();
        let telegram_config = config.clone();
        let router = Arc::clone(&self.router);
        let arc_store_ref = self._database.as_ref().map(|db| db.arc_store());
        let attachment_store_ref = self._database.as_ref().map(|db| db.attachment_store());
        let profile_store_ref = self.profile_store.clone();
        let profile_embedder_ref = Arc::clone(&self.profile_embedder);
        let profile_embedding_cache_ref = Arc::clone(&self.profile_embedding_cache);
        let calendar_store_ref = self.calendar_store.clone();
        let contact_store_ref = self.contact_store.clone();
        let memory_ref = self.memory.clone();
        let mcp_ref = self.mcp.clone();
        let tool_doc_dir_ref = self.tool_doc_dir.clone();
        let notifier = self.notifier.clone();
        let grant_store_ref = self.grant_store.clone();
        let pending_grants_ref = self.pending_grants.clone();
        let spawned_processes_ref = self.spawned_processes.clone();
        let telegram_approval_sink = self.telegram_approval_sink.clone();
        let approval_router_ref = self.approval_router.clone();
        let coordinator_ref = Arc::clone(&self.coordinator);
        let task_arc_map_ref = Arc::clone(&self.task_arc_map);
        let pending_email_marks_ref = Arc::clone(&self.pending_email_marks);
        let dispatch_signal_ref = Arc::clone(&self.dispatch_signal);
        let web_search_ref = Arc::clone(&self.web_search);
        let email_sender_ref = self.email_sender.clone();
        let telegram_outbound_hint_ref = self.telegram_outbound_hint.clone();

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
                                    let router_c = Arc::clone(&router);
                                    let arc_store_c = arc_store_ref.clone();
                                    let attachment_store_c = attachment_store_ref.clone();
                                    let calendar_store_c = calendar_store_ref.clone();
                                    let contact_store_c = contact_store_ref.clone();
                                    let memory_c = memory_ref.clone();
                                    let mcp_c = Arc::clone(&mcp_ref);
                                    let tool_doc_dir_c = tool_doc_dir_ref.clone();
                                    let app_handle_c = app_handle.clone();
                                    let notifier_c = notifier.clone();
                                    let grant_store_c = grant_store_ref.clone();
                                    let pending_grants_c = pending_grants_ref.clone();
                                    let spawned_processes_c = spawned_processes_ref.clone();
                                    let telegram_approval_sink_c = telegram_approval_sink.clone();
                                    let profile_store_c = profile_store_ref.clone();
                                    let profile_embedder_c = Arc::clone(&profile_embedder_ref);
                                    let profile_embedding_cache_c =
                                        Arc::clone(&profile_embedding_cache_ref);
                                    let approval_router_c = approval_router_ref.clone();
                                    let web_search_c = Arc::clone(&web_search_ref);
                                    let email_sender_c = email_sender_ref.clone();
                                    let telegram_outbound_hint_c =
                                        telegram_outbound_hint_ref.clone();
                                    let event_id = event.id;
                                    let attachments_owned = event.content.attachments.clone();
                                    tauri::async_runtime::spawn(async move {
                                        execute_owner_telegram_message(
                                            &text_owned,
                                            chat_id,
                                            &bot_token_c,
                                            event_id,
                                            &attachments_owned,
                                            &router_c,
                                            &arc_store_c,
                                            attachment_store_c.as_ref(),
                                            &calendar_store_c,
                                            &contact_store_c,
                                            &memory_c,
                                            &mcp_c,
                                            tool_doc_dir_c.as_deref(),
                                            &app_handle_c,
                                            notifier_c.as_ref(),
                                            grant_store_c.as_ref(),
                                            &pending_grants_c,
                                            &spawned_processes_c,
                                            telegram_approval_sink_c.as_ref(),
                                            &profile_store_c,
                                            &profile_embedder_c,
                                            &profile_embedding_cache_c,
                                            approval_router_c.as_ref(),
                                            &web_search_c,
                                            &email_sender_c,
                                            &telegram_outbound_hint_c,
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
        let pending_email_marks = Arc::clone(&self.pending_email_marks);
        let router = Arc::clone(&self.router);
        let arc_store = self._database.as_ref().map(|db| db.arc_store());
        let calendar_store = self.calendar_store.clone();
        let contact_store = self.contact_store.clone();
        let memory = self.memory.clone();
        let mcp = Arc::clone(&self.mcp);
        let tool_doc_dir = self.tool_doc_dir.clone();
        let profile_store = self.profile_store.clone();
        let grant_store = self.grant_store.clone();
        let pending_grants = self.pending_grants.clone();
        let spawned_processes = self.spawned_processes.clone();
        let telegram_approval_sink = self.telegram_approval_sink.clone();
        let approval_router = self.approval_router.clone();
        let notifier = self.notifier.clone();
        let compactor = self.compactor.clone();
        let web_search = Arc::clone(&self.web_search);
        let email_sender = self.email_sender.clone();
        // Snapshot the active provider id so the dispatch loop can resolve
        // the per-arc compaction budget on each iteration. A mid-session
        // provider switch won't propagate into already-spawned dispatch
        // loops — acceptable, and clearly bounded scope. TODO: switch to
        // an Arc<Mutex<String>> on AppState if we ever need live reloads.
        let active_provider_id_snapshot = self
            .active_provider_id
            .try_lock()
            .map(|g| g.clone())
            .unwrap_or_default();
        let attachment_store_loop = self.attachment_store();
        let inflight = Arc::clone(&self.inflight_approvals);

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
                    // Resolve compaction budget per task. Re-reading the
                    // config TOML each dispatch is cheap (small file, only
                    // fires on user-driven sense events) and lets the user
                    // tune compaction without restarting the loop.
                    let cfg_for_resolvers = crate::state::load_config();
                    let (compaction_trigger_tokens, compaction_target_tokens) =
                        crate::compaction::resolve_compaction_budget(
                            &cfg_for_resolvers,
                            &active_provider_id_snapshot,
                        );
                    let sampling_temperature = crate::compaction::resolve_provider_temperature(
                        &cfg_for_resolvers,
                        &active_provider_id_snapshot,
                    );
                    let ctx = crate::commands::ApprovedTaskCtx {
                        coordinator: Arc::clone(&coordinator),
                        router: Arc::clone(&router),
                        arc_store: arc_store.clone(),
                        calendar_store: calendar_store.clone(),
                        contact_store: contact_store.clone(),
                        memory: memory.clone(),
                        mcp: Arc::clone(&mcp),
                        tool_doc_dir: tool_doc_dir.clone(),
                        grant_store: grant_store.clone(),
                        profile_store: profile_store.clone(),
                        pending_grants: pending_grants.clone(),
                        spawned_processes: spawned_processes.clone(),
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
                        initial_user_images: Vec::new(),
                        attachment_store: attachment_store_loop.clone(),
                        compaction_trigger_tokens,
                        compaction_target_tokens,
                        sampling_temperature,
                    };

                    let task_arc_map_clone = Arc::clone(&task_arc_map);
                    let pending_email_marks_clone = Arc::clone(&pending_email_marks);
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

                        // Drain the pending-email-mark entry too. On
                        // success: spawn a fire-and-forget IMAP STORE
                        // call. On failure (or skip): just drop it —
                        // the source email stays UNSEEN and will
                        // re-trigger on next poll, which is the user's
                        // explicit requirement.
                        let mark_info = pending_email_marks_clone.write().await.remove(&task_id);
                        if let Some(info) = mark_info {
                            if succeeded {
                                let config = load_config();
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
    router: &Arc<RwLock<Arc<DefaultLlmRouter>>>,
    arc_store: &Option<ArcStore>,
    attachment_store: Option<&athen_persistence::attachments::AttachmentStore>,
    calendar_store: &Option<CalendarStore>,
    contact_store: &Option<SqliteContactStore>,
    memory: &Option<Arc<Memory>>,
    mcp: &Arc<McpRegistry>,
    tool_doc_dir: Option<&std::path::Path>,
    app_handle: &tauri::AppHandle,
    notifier: Option<&Arc<NotificationOrchestrator>>,
    grant_store: Option<&Arc<GrantStore>>,
    pending_grants: &PendingGrants,
    spawned_processes: &athen_agent::SpawnedProcessMap,
    telegram_approval_sink: Option<&Arc<crate::approval::TelegramApprovalSink>>,
    profile_store: &Option<Arc<athen_persistence::profiles::SqliteProfileStore>>,
    profile_embedder: &Arc<dyn athen_core::traits::embedding::EmbeddingProvider>,
    profile_embedding_cache: &ProfileEmbeddingCache,
    approval_router: Option<&Arc<crate::approval::ApprovalRouter>>,
    web_search: &Arc<dyn WebSearchProvider>,
    email_sender: &Option<Arc<dyn athen_core::traits::email_sender::EmailSender>>,
    telegram_outbound_hint: &crate::notifier::TelegramOutboundHint,
) {
    use std::time::Duration;

    use crate::app_tools::AppToolRegistry;
    use crate::commands::{new_tool_log, spawn_stream_forwarder, AgentProgress, TauriAuditor};
    use athen_agent::{AgentBuilder, ShellToolRegistry};
    use athen_core::task::{DomainType, Task, TaskPriority, TaskStatus};
    use athen_core::traits::agent::AgentExecutor;
    use tauri::Emitter;

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

                if let Some(id) = tier3 {
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
    let mut shell_registry = ShellToolRegistry::new()
        .await
        .with_spawned_processes(spawned_processes.clone())
        .with_web_search(web_search.clone())
        .with_email_sender_opt(email_sender.clone());
    if let (Some(store), Some(arc_id_str)) = (grant_store, target_arc_id.as_ref()) {
        let provider = Arc::new(crate::file_gate::ArcWritableProvider {
            arc_id: crate::file_gate::arc_uuid(arc_id_str),
            store: store.clone(),
        });
        shell_registry = shell_registry.with_extra_writable(provider);
    }
    if let Some(router) = approval_router {
        shell_registry = shell_registry.with_toolbox_approval(Arc::new(
            crate::file_gate::RouterToolboxApprovalGate::new(
                Arc::clone(router),
                target_arc_id.clone(),
            ),
        ));
        let gate: Arc<dyn athen_agent::EmailSendApprovalGate> =
            Arc::new(crate::email_gate::RouterEmailApprovalGate::new(
                Arc::clone(router),
                target_arc_id.clone(),
            ));
        shell_registry = shell_registry.with_email_approval(gate);
    }
    let mut registry = AppToolRegistry::new(
        shell_registry,
        calendar_store.clone(),
        contact_store.clone(),
        memory.clone(),
    )
    .with_mcp(mcp.clone() as Arc<dyn athen_core::traits::mcp::McpClient>);
    if let Some(astore) = attachment_store {
        registry = registry.with_attachments(astore.clone());
    }
    if let (Some(store), Some(arc_id_str)) = (grant_store, target_arc_id.as_ref()) {
        let mut gate = crate::file_gate::FileGate::new(
            arc_id_str.clone(),
            store.clone(),
            pending_grants.clone(),
            Some(app_handle.clone()),
        );
        if let Some(sink) = telegram_approval_sink {
            gate = gate.with_telegram_approval(sink.clone());
        }
        registry = registry.with_file_gate(Arc::new(gate));
    }
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

    let auditor = TauriAuditor::new(
        app_handle.clone(),
        arc_store.clone(),
        target_arc_id.clone().unwrap_or_default(),
        turn_id.clone(),
        tool_log.clone(),
    )
    .with_telegram_progress(Arc::clone(&progress));
    let stream_tx = spawn_stream_forwarder(app_handle, target_arc_id.clone());
    let cancel_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));

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

    let mut builder = AgentBuilder::new()
        .llm_router(exec_router)
        .tool_registry(Box::new(registry))
        .auditor(Box::new(auditor))
        .max_steps(50)
        .timeout(Duration::from_secs(300))
        .context_messages(context)
        .stream_sender(stream_tx)
        .cancel_flag(cancel_flag)
        .default_temperature(sampling_temperature);
    if let Some(p) = tool_doc_dir {
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
        id: uuid::Uuid::new_v4(),
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
    // `Run`, `files__list_dir` → `List`) so the footer matches what
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
                Ok(c) => c,
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

/// Build a router for the given provider ID, reading configuration from the
/// supplied `AthenConfig`.  Returns `(Arc<DefaultLlmRouter>, model_name)`.
fn build_router_for_provider_from_config(
    provider_id: &str,
    config: &AthenConfig,
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

    let router = build_router_for_provider(
        provider_id,
        &base_url,
        &model,
        api_key.as_deref(),
        supports_vision,
        supports_documents,
        family,
    );
    (router, model)
}

/// Default base URL for known provider IDs.
fn default_base_url_for(id: &str) -> &str {
    match id {
        "deepseek" => "https://api.deepseek.com",
        "openai" => "https://api.openai.com",
        "anthropic" => "https://api.anthropic.com",
        "ollama" => "http://localhost:11434",
        "llamacpp" => "http://localhost:8080",
        _ => "http://localhost:8080",
    }
}

/// Default model for known provider IDs.
fn default_model_for(id: &str) -> &str {
    match id {
        "deepseek" => "deepseek-chat",
        "openai" => "gpt-4o",
        "anthropic" => "claude-sonnet-4-20250514",
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
pub(crate) fn build_router_for_provider(
    provider_id: &str,
    base_url: &str,
    model: &str,
    api_key: Option<&str>,
    supports_vision: bool,
    supports_documents: bool,
    family: athen_core::llm::ModelFamily,
) -> Arc<DefaultLlmRouter> {
    let provider: Box<dyn LlmProvider> = match provider_id {
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
        "anthropic" => {
            let key = api_key.unwrap_or_default().to_string();
            let mut p = AnthropicProvider::new(key, model.to_string())
                .with_family(family)
                .with_vision(supports_vision)
                .with_documents(supports_documents);
            if base_url != "https://api.anthropic.com" && !base_url.is_empty() {
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
    };

    let mut providers: HashMap<String, Box<dyn LlmProvider>> = HashMap::new();
    providers.insert(provider_id.into(), provider);

    let profile = ProfileConfig {
        description: format!("{} default", provider_id),
        priority: vec![provider_id.into()],
        fallback: None,
    };

    let mut profiles = HashMap::new();
    profiles.insert(ModelProfile::Powerful, profile.clone());
    profiles.insert(ModelProfile::Fast, profile.clone());
    profiles.insert(ModelProfile::Code, profile.clone());
    profiles.insert(ModelProfile::Cheap, profile);

    Arc::new(DefaultLlmRouter::new(
        providers,
        profiles,
        BudgetTracker::new(None),
    ))
}

/// Generate a human-readable Arc identifier: `arc_YYYYMMDD_HHMMSS`.
fn generate_arc_id() -> String {
    chrono::Utc::now().format("arc_%Y%m%d_%H%M%S").to_string()
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
async fn restore_enabled_mcps(registry: &Arc<McpRegistry>, store: &McpStore) -> Result<()> {
    let rows = store.list_enabled().await?;
    let mut entries = Vec::new();
    for row in rows {
        match athen_mcp::lookup(&row.mcp_id) {
            Some(entry) => {
                entries.push(athen_mcp::EnabledEntry {
                    entry,
                    config: row.config,
                });
            }
            None => {
                warn!(
                    "Persisted MCP id '{}' not found in catalog; skipping",
                    row.mcp_id
                );
            }
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
async fn build_memory(router: &Arc<RwLock<Arc<DefaultLlmRouter>>>) -> Option<Arc<Memory>> {
    use athen_llm::embeddings::router::EmbeddingRouter;
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

    // Use keyword embeddings as the default fallback (always available).
    let embedding_router = EmbeddingRouter::new(vec![]);
    // LLM entity extractor for automatic knowledge graph population.
    let extractor_router: Box<dyn LlmRouter> = Box::new(SharedRouter(Arc::clone(router)));
    let extractor = LlmEntityExtractor::new(extractor_router);

    let memory = Memory::new(Box::new(vector), Box::new(graph))
        .with_embedder(Box::new(embedding_router))
        .with_extractor(Box::new(extractor));

    info!("Memory system initialized with SQLite persistence");
    Some(Arc::new(memory))
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
