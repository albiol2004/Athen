//! Application state management.
//!
//! Builds the coordinator, LLM router, and risk evaluator, wiring them
//! together as the composition root for the Athen desktop app.
//! Configuration is loaded from TOML files (`~/.athen/` or `./config/`)
//! with environment variable overrides.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Serialize;
use tokio::sync::{Mutex, RwLock};
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
use athen_llm::providers::llamacpp::LlamaCppProvider;
use athen_llm::providers::ollama::OllamaProvider;
use athen_llm::providers::openai::OpenAiCompatibleProvider;
use athen_llm::router::DefaultLlmRouter;
use athen_persistence::chat::ChatStore;
use athen_persistence::Database;
use athen_risk::llm_fallback::LlmRiskEvaluator;
use athen_risk::CombinedRiskEvaluator;

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

/// Top-level application state managed by Tauri.
pub struct AppState {
    pub coordinator: Coordinator,
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
    /// Current session identifier (format: `session_YYYYMMDD_HHMMSS`).
    pub session_id: Mutex<String>,
    /// Persistent chat storage backed by SQLite.
    pub chat_store: Option<ChatStore>,
    /// Keep the database alive so the connection is not dropped.
    _database: Option<Database>,
    /// Cancellation flag for the currently running agent executor.
    /// Set to `true` to cancel the in-progress task immediately.
    pub cancel_flag: Arc<AtomicBool>,
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
        let (coordinator, database) = build_coordinator_with_persistence(&router).await;

        // Build the chat store and try to restore history from the latest session.
        let chat_store = database.as_ref().map(|db| db.chat_store());
        let (session_id, history) = restore_or_create_session(&chat_store).await;

        Self {
            coordinator,
            router,
            active_provider_id: Mutex::new(active_id),
            history: Mutex::new(history),
            pending_message: Mutex::new(None),
            model_name: Mutex::new(model_name),
            session_id: Mutex::new(session_id),
            chat_store,
            _database: database,
            cancel_flag: Arc::new(AtomicBool::new(false)),
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
        AuthType::ApiKey(key) if !key.is_empty() && !key.starts_with("${") => {
            Some(key.clone())
        }
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
        "llamacpp" => {
            Box::new(LlamaCppProvider::new(base_url.to_string(), model.to_string()))
        }
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
                p = p.with_cost_estimator(Box::new(
                    athen_llm::providers::openai::ZeroCostEstimator,
                ));
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

    // Create a new session with metadata.
    let new_id = generate_session_id();
    if let Some(store) = chat_store {
        if let Err(e) = store.create_session(&new_id, "New Chat").await {
            warn!("Failed to create initial session metadata: {e}");
        }
    }
    (new_id, Vec::new())
}

/// Build the coordinator with the combined (rules + LLM) risk evaluator
/// and optional SQLite persistence at `~/.athen/athen.db`.
async fn build_coordinator_with_persistence(
    router: &Arc<RwLock<Arc<DefaultLlmRouter>>>,
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
