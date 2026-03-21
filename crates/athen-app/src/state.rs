//! Application state management.
//!
//! Builds the coordinator, LLM router, and risk evaluator, wiring them
//! together as the composition root for the Athen desktop app.
//! Configuration is loaded from TOML files (`~/.athen/` or `./config/`)
//! with environment variable overrides.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Serialize;
use tokio::sync::Mutex;
use tracing::{info, warn};

use athen_core::config::{AthenConfig, AuthType, ProfileConfig};
use athen_core::config_loader;
use athen_core::error::Result;
use athen_core::llm::{
    BudgetStatus, ChatMessage, LlmRequest, LlmResponse, LlmStream, MessageContent, ModelProfile,
    Role,
};
use athen_core::traits::llm::{LlmProvider, LlmRouter};
use athen_coordinador::Coordinator;
use athen_llm::budget::BudgetTracker;
use athen_llm::providers::deepseek::DeepSeekProvider;
use athen_llm::router::DefaultLlmRouter;
use athen_persistence::chat::ChatStore;
use athen_persistence::Database;
use athen_risk::llm_fallback::LlmRiskEvaluator;
use athen_risk::CombinedRiskEvaluator;

/// Wrapper to share the router via `Arc` while satisfying the `LlmRouter` trait.
pub(crate) struct SharedRouter(pub Arc<DefaultLlmRouter>);

#[async_trait]
impl LlmRouter for SharedRouter {
    async fn route(&self, request: &LlmRequest) -> Result<LlmResponse> {
        self.0.route(request).await
    }
    async fn route_streaming(&self, request: &LlmRequest) -> Result<LlmStream> {
        self.0.route_streaming(request).await
    }
    async fn budget_remaining(&self) -> Result<BudgetStatus> {
        self.0.budget_remaining().await
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

/// Top-level application state managed by Tauri.
pub struct AppState {
    pub coordinator: Coordinator,
    pub router: Arc<DefaultLlmRouter>,
    /// In-memory conversation history for the current session.
    pub history: Mutex<Vec<ChatMessage>>,
    /// The user's original message for a task pending approval, so it can
    /// be replayed through the executor once approved.
    pub pending_message: Mutex<Option<String>>,
    /// The model name reported to the frontend (from config or default).
    pub model_name: String,
    /// Current session identifier (format: `session_YYYYMMDD_HHMMSS`).
    pub session_id: Mutex<String>,
    /// Persistent chat storage backed by SQLite.
    pub chat_store: Option<ChatStore>,
    /// Keep the database alive so the connection is not dropped.
    _database: Option<Database>,
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
        let (api_key, model_name) = resolve_api_key_and_model(&config);

        let router = build_router(api_key);
        let (coordinator, database) = build_coordinator_with_persistence(&router).await;

        // Build the chat store and try to restore history from the latest session.
        let chat_store = database.as_ref().map(|db| db.chat_store());
        let (session_id, history) = restore_or_create_session(&chat_store).await;

        Self {
            coordinator,
            router,
            history: Mutex::new(history),
            pending_message: Mutex::new(None),
            model_name,
            session_id: Mutex::new(session_id),
            chat_store,
            _database: database,
        }
    }
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

/// Resolve the DeepSeek API key and model name from env var + config.
///
/// Env var `DEEPSEEK_API_KEY` takes precedence over config file values.
/// Placeholder values like `${DEEPSEEK_API_KEY}` in the config are treated
/// as unresolved.
fn resolve_api_key_and_model(config: &AthenConfig) -> (String, String) {
    let mut model_name = "deepseek-chat".to_string();

    // Try env var first.
    let api_key = match std::env::var("DEEPSEEK_API_KEY") {
        Ok(key) if !key.is_empty() => {
            // Still pick up the model name from config if available.
            if let Some(provider) = config.models.providers.get("deepseek") {
                if !provider.default_model.is_empty() {
                    model_name = provider.default_model.clone();
                }
            }
            key
        }
        _ => {
            // Try to get from config providers.
            match config.models.providers.get("deepseek") {
                Some(provider) => {
                    if !provider.default_model.is_empty() {
                        model_name = provider.default_model.clone();
                    }
                    match &provider.auth {
                        AuthType::ApiKey(key)
                            if !key.is_empty() && !key.starts_with("${") =>
                        {
                            key.clone()
                        }
                        _ => {
                            warn!(
                                "No DEEPSEEK_API_KEY env var and no valid API key in config. \
                                 Chat will not work until an API key is provided."
                            );
                            String::new()
                        }
                    }
                }
                None => {
                    warn!(
                        "No DEEPSEEK_API_KEY env var and no deepseek provider in config. \
                         Chat will not work until an API key is provided."
                    );
                    String::new()
                }
            }
        }
    };

    (api_key, model_name)
}

// ---------------------------------------------------------------------------
// System initialisation
// ---------------------------------------------------------------------------

/// Resolve the data directory (`~/.athen/`), creating it if needed.
fn ensure_data_dir() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("HOME") {
        let data_dir = PathBuf::from(home).join(".athen");
        if let Err(e) = std::fs::create_dir_all(&data_dir) {
            warn!("Failed to create data directory {}: {e}", data_dir.display());
            return None;
        }
        Some(data_dir)
    } else {
        warn!("HOME not set, cannot create data directory.");
        None
    }
}

/// Build the LLM router with the DeepSeek provider and default profiles.
fn build_router(api_key: String) -> Arc<DefaultLlmRouter> {
    let provider = DeepSeekProvider::new(api_key);

    let mut providers: HashMap<String, Box<dyn LlmProvider>> = HashMap::new();
    providers.insert("deepseek".into(), Box::new(provider));

    let profile = ProfileConfig {
        description: "DeepSeek default".into(),
        priority: vec!["deepseek".into()],
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

/// Generate a human-readable session identifier: `session_YYYYMMDD_HHMMSS`.
fn generate_session_id() -> String {
    chrono::Utc::now().format("session_%Y%m%d_%H%M%S").to_string()
}

/// Try to restore the most recent session from persistent chat storage.
/// If the store is unavailable or empty, create a new session with empty history.
async fn restore_or_create_session(
    chat_store: &Option<ChatStore>,
) -> (String, Vec<ChatMessage>) {
    if let Some(store) = chat_store {
        match store.list_sessions().await {
            Ok(sessions) if !sessions.is_empty() => {
                let latest = &sessions[0];
                match store.load_messages(latest).await {
                    Ok(persisted) => {
                        let history: Vec<ChatMessage> = persisted
                            .into_iter()
                            .map(|m| ChatMessage {
                                role: match m.role.as_str() {
                                    "user" => Role::User,
                                    "assistant" => Role::Assistant,
                                    "system" => Role::System,
                                    "tool" => Role::Tool,
                                    _ => Role::User,
                                },
                                content: if m.content_type == "structured" {
                                    match serde_json::from_str(&m.content) {
                                        Ok(v) => MessageContent::Structured(v),
                                        Err(_) => MessageContent::Text(m.content),
                                    }
                                } else {
                                    MessageContent::Text(m.content)
                                },
                            })
                            .collect();
                        info!(
                            "Restored {} messages from session '{}'",
                            history.len(),
                            latest
                        );
                        return (latest.clone(), history);
                    }
                    Err(e) => {
                        warn!("Failed to load messages for session '{}': {e}", latest);
                    }
                }
            }
            Err(e) => {
                warn!("Failed to list sessions: {e}");
            }
            _ => {}
        }
    }

    (generate_session_id(), Vec::new())
}

/// Build the coordinator with the combined (rules + LLM) risk evaluator
/// and optional SQLite persistence at `~/.athen/athen.db`.
async fn build_coordinator_with_persistence(
    router: &Arc<DefaultLlmRouter>,
) -> (Coordinator, Option<Database>) {
    let risk_router: Box<dyn LlmRouter> = Box::new(SharedRouter(Arc::clone(router)));
    let llm_evaluator = LlmRiskEvaluator::new(risk_router);
    let combined = CombinedRiskEvaluator::new(llm_evaluator);
    let coordinator = Coordinator::new(Box::new(combined));

    // Try to open the database for persistence.
    if let Some(data_dir) = ensure_data_dir() {
        let db_path = data_dir.join("athen.db");
        match Database::new(&db_path).await {
            Ok(db) => {
                let store = db.store();
                info!("Database opened at {}", db_path.display());
                let coordinator = coordinator.with_persistence(Box::new(store));
                return (coordinator, Some(db));
            }
            Err(e) => {
                warn!(
                    "Failed to open database at {}: {e}. Running without persistence.",
                    db_path.display()
                );
            }
        }
    }

    (coordinator, None)
}
