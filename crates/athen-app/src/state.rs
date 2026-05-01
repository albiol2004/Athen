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

use crate::file_gate::PendingGrants;

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
    /// Approvals currently being executed. See [`InflightApprovals`].
    pub inflight_approvals: InflightApprovals,
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
        let pending_grants = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let spawned_processes: athen_agent::SpawnedProcessMap =
            Arc::new(Mutex::new(HashMap::new()));

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
            pending_grants,
            spawned_processes,
            inflight_approvals: Arc::new(Mutex::new(HashSet::new())),
        };

        if let Err(e) = state.refresh_tools_doc().await {
            warn!("Failed to write initial per-group tool docs: {e}");
        }

        state
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
        let shell_registry = athen_agent::ShellToolRegistry::new()
            .await
            .with_spawned_processes(self.spawned_processes.clone());
        let registry = crate::app_tools::AppToolRegistry::new(
            shell_registry,
            self.calendar_store.clone(),
            self.contact_store.clone(),
            self.memory.clone(),
        )
        .with_mcp(self.mcp.clone() as Arc<dyn athen_core::traits::mcp::McpClient>);
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
    ) -> crate::app_tools::AppToolRegistry {
        let mut shell = athen_agent::ShellToolRegistry::new()
            .await
            .with_spawned_processes(self.spawned_processes.clone());
        if let Some(store) = self.grant_store.clone() {
            let provider = Arc::new(crate::file_gate::ArcWritableProvider {
                arc_id: crate::file_gate::arc_uuid(arc_id),
                store,
            });
            shell = shell.with_extra_writable(provider);
        }
        let mut registry = crate::app_tools::AppToolRegistry::new(
            shell,
            self.calendar_store.clone(),
            self.contact_store.clone(),
            self.memory.clone(),
        )
        .with_mcp(self.mcp.clone() as Arc<dyn athen_core::traits::mcp::McpClient>);
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
        registry
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
                    channels.push(Box::new(TelegramChannel::new(token.clone(), owner_id)));
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
        use crate::approval::{
            ApprovalRouter, InAppApprovalSink, TelegramApprovalSink,
        };
        use athen_core::traits::approval::ApprovalSink;

        let config = load_config();

        let inapp = Arc::new(InAppApprovalSink::new(app_handle));
        let mut sinks: Vec<Arc<dyn ApprovalSink>> =
            vec![inapp.clone() as Arc<dyn ApprovalSink>];

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
        router = router
            .with_escalation_after(std::time::Duration::from_secs(escalation_secs));

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
        let profile_store_ref = self.profile_store.clone();
        let notifier = self.notifier.clone();

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
                                &app_handle,
                                notifier.as_ref(),
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
    pub fn start_calendar_monitor(&mut self, app_handle: tauri::AppHandle) {
        use athen_core::traits::sense::SenseMonitor;
        use athen_sentidos::calendar::CalendarMonitor;

        let mut monitor = CalendarMonitor::new();
        let config = load_config();
        let router = Arc::clone(&self.router);
        let arc_store_ref = self._database.as_ref().map(|db| db.arc_store());
        let profile_store_ref = self.profile_store.clone();
        let notifier = self.notifier.clone();

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
                                &app_handle,
                                notifier.as_ref(),
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
        let profile_store_ref = self.profile_store.clone();
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

                                if !text.is_empty() && chat_id != 0 {
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
                                    let telegram_approval_sink_c =
                                        telegram_approval_sink.clone();
                                    tauri::async_runtime::spawn(async move {
                                        execute_owner_telegram_message(
                                            &text_owned,
                                            chat_id,
                                            &bot_token_c,
                                            &router_c,
                                            &arc_store_c,
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
                                        )
                                        .await;
                                    });
                                }
                            } else {
                                // Non-owner messages go through the full sense
                                // router: LLM triage, arc creation, notification.
                                crate::sense_router::process_sense_event(
                                    event,
                                    &router,
                                    &arc_store_ref,
                                    &profile_store_ref,
                                    &app_handle,
                                    notifier.as_ref(),
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
                    info!(
                        count = callbacks.len(),
                        "Draining Telegram callback events"
                    );
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
    router: &Arc<RwLock<Arc<DefaultLlmRouter>>>,
    arc_store: &Option<ArcStore>,
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

    // Find or create an arc for this Telegram conversation.
    // Use a 5-minute time window: if there's a recent Messaging arc, reuse it.
    let target_arc_id = if let Some(store) = arc_store {
        match store.list_arcs().await {
            Ok(arcs) => {
                let now = chrono::Utc::now();
                // Look for a recent active Messaging arc within 5 minutes.
                let recent = arcs
                    .iter()
                    .filter(|a| {
                        a.source == athen_persistence::arcs::ArcSource::Messaging
                            && a.status == athen_persistence::arcs::ArcStatus::Active
                    })
                    .find(|a| {
                        chrono::DateTime::parse_from_rfc3339(&a.updated_at)
                            .map(|t| now.signed_duration_since(t).num_seconds() < 300)
                            .unwrap_or(false)
                    })
                    .map(|a| a.id.clone());

                if let Some(id) = recent {
                    info!("Reusing recent Telegram arc: {}", id);
                    Some(id)
                } else {
                    // Create a new arc.
                    let arc_id = crate::sense_router::generate_arc_id();
                    let name = if text.len() > 30 {
                        format!("{}...", &text[..27])
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
    let context = if let (Some(store), Some(ref arc_id)) = (arc_store, &target_arc_id) {
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

    // Build the executor (mirrors send_message logic but without risk/coordinator).
    let exec_router: Box<dyn athen_core::traits::llm::LlmRouter> =
        Box::new(SharedRouter(Arc::clone(router)));
    let mut shell_registry = ShellToolRegistry::new()
        .await
        .with_spawned_processes(spawned_processes.clone());
    if let (Some(store), Some(arc_id_str)) = (grant_store, target_arc_id.as_ref()) {
        let provider = Arc::new(crate::file_gate::ArcWritableProvider {
            arc_id: crate::file_gate::arc_uuid(arc_id_str),
            store: store.clone(),
        });
        shell_registry = shell_registry.with_extra_writable(provider);
    }
    let mut registry = AppToolRegistry::new(
        shell_registry,
        calendar_store.clone(),
        contact_store.clone(),
        memory.clone(),
    )
    .with_mcp(mcp.clone() as Arc<dyn athen_core::traits::mcp::McpClient>);
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
    let auditor = TauriAuditor::new(
        app_handle.clone(),
        arc_store.clone(),
        target_arc_id.clone().unwrap_or_default(),
        turn_id.clone(),
        tool_log.clone(),
    );
    let stream_tx = spawn_stream_forwarder(app_handle, target_arc_id.clone());
    let cancel_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let mut builder = AgentBuilder::new()
        .llm_router(exec_router)
        .tool_registry(Box::new(registry))
        .auditor(Box::new(auditor))
        .max_steps(50)
        .timeout(Duration::from_secs(300))
        .context_messages(context)
        .stream_sender(stream_tx)
        .cancel_flag(cancel_flag);
    if let Some(p) = tool_doc_dir {
        builder = builder.tool_doc_dir(p.to_path_buf());
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
        source_event: None,
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
    let _ = app_handle.emit(
        "agent-progress",
        AgentProgress {
            step: 0,
            tool_name: "Processing Telegram message...".to_string(),
            status: "InProgress".to_string(),
            detail: Some(text.chars().take(200).collect()),
        },
    );

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
                format!("Sorry, the task failed: {}", crate::commands::simplify_error_public(&raw))
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

            if let Err(e) = send_telegram_reply(bot_token, chat_id, &user_msg).await {
                warn!("Failed to send Telegram error reply: {e}");
            }
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

    // Send the response back to Telegram, with a "Tools used" footer when
    // the agent ran any. The Telegram client doesn't render our SVG icons,
    // so this is a plain-text de-duplicated list.
    let footer = build_telegram_tools_footer(&tool_log);
    let outbound = if footer.is_empty() {
        content.clone()
    } else {
        format!("{content}\n\n{footer}")
    };
    if let Err(e) = send_telegram_reply(bot_token, chat_id, &outbound).await {
        warn!("Failed to send Telegram reply: {e}");
    }

    info!(
        "Owner Telegram message executed, response length: {} chars",
        content.len()
    );
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

    let mut order: Vec<String> = Vec::new();
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for name in names {
        let entry = counts.entry(name.clone()).or_insert(0);
        if *entry == 0 {
            order.push(name);
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
/// 1. `~/.athen/config.toml`
/// 2. `./config/config.toml` (project-local fallback)
///
/// Returns the directory path if a config file is found, or None for defaults.
fn find_config_dir() -> Option<PathBuf> {
    // Try ~/.athen/
    if let Some(home) = std::env::var_os("HOME") {
        let home_config = PathBuf::from(home).join(".athen");
        if home_config.join("config.toml").exists() {
            return Some(home_config);
        }
    }

    // Try ./config/
    let local_config = PathBuf::from("config");
    if local_config.join("config.toml").exists() {
        return Some(local_config);
    }

    None
}

/// Load configuration from TOML files, falling back to defaults.
fn load_config() -> AthenConfig {
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

/// Resolve the data directory (`~/.athen/`), creating it if needed.
fn ensure_data_dir() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("HOME") {
        let data_dir = PathBuf::from(home).join(".athen");
        if let Err(e) = std::fs::create_dir_all(&data_dir) {
            warn!(
                "Failed to create data directory {}: {e}",
                data_dir.display()
            );
            return None;
        }
        Some(data_dir)
    } else {
        warn!("HOME not set, cannot create data directory.");
        None
    }
}

/// Determine the active provider ID from config, falling back to "deepseek".
///
/// Looks for `active_provider` in `config.models.assignments` (we reuse the
/// existing assignments map with a special key), or defaults to "deepseek".
fn resolve_active_provider(config: &AthenConfig) -> String {
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

    let router = build_router_for_provider(provider_id, &base_url, &model, api_key.as_deref());
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
/// Uses the appropriate provider type based on the ID:
/// - `"deepseek"` -> `DeepSeekProvider`
/// - `"ollama"` -> `OllamaProvider`
/// - `"llamacpp"` -> `LlamaCppProvider`
/// - anything else -> `OpenAiCompatibleProvider`
pub(crate) fn build_router_for_provider(
    provider_id: &str,
    base_url: &str,
    model: &str,
    api_key: Option<&str>,
) -> Arc<DefaultLlmRouter> {
    let provider: Box<dyn LlmProvider> = match provider_id {
        "deepseek" => {
            let key = api_key.unwrap_or_default().to_string();
            let mut p = DeepSeekProvider::new(key);
            if base_url != "https://api.deepseek.com" {
                p = p.with_base_url(base_url.to_string());
            }
            if model != "deepseek-chat" {
                p = p.with_model(model.to_string());
            }
            Box::new(p)
        }
        "ollama" => {
            let mut p = OllamaProvider::new(model.to_string());
            if base_url != "http://localhost:11434" {
                p = p.with_base_url(base_url.to_string());
            }
            Box::new(p)
        }
        "llamacpp" => Box::new(LlamaCppProvider::new(
            base_url.to_string(),
            model.to_string(),
        )),
        _ => {
            // Generic OpenAI-compatible provider (openai, anthropic, custom).
            let mut p = OpenAiCompatibleProvider::new(base_url.to_string())
                .with_model(model.to_string())
                .with_provider_id(provider_id.to_string());
            if let Some(key) = api_key {
                p = p.with_api_key(key.to_string());
            }
            // Local providers use zero-cost estimation.
            if matches!(provider_id, "ollama" | "llamacpp") {
                p = p
                    .with_cost_estimator(Box::new(athen_llm::providers::openai::ZeroCostEstimator));
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
        match store.list_arcs().await {
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
