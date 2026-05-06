use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

use crate::llm::ModelProfile;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AthenConfig {
    pub workspace_path: PathBuf,
    pub operation: OperationConfig,
    pub models: ModelsConfig,
    pub domains: HashMap<String, DomainConfig>,
    pub security: SecurityConfig,
    pub persistence: PersistenceConfig,
    pub email: EmailConfig,
    pub telegram: TelegramConfig,
    #[serde(default)]
    pub notifications: NotificationConfig,
    #[serde(default)]
    pub embeddings: EmbeddingConfig,
    #[serde(default)]
    pub web_search: WebSearchConfig,
}

impl Default for AthenConfig {
    fn default() -> Self {
        Self {
            workspace_path: PathBuf::from(".athen"),
            operation: OperationConfig::default(),
            models: ModelsConfig::default(),
            domains: default_domains(),
            security: SecurityConfig::default(),
            persistence: PersistenceConfig::default(),
            email: EmailConfig::default(),
            telegram: TelegramConfig::default(),
            notifications: NotificationConfig::default(),
            embeddings: EmbeddingConfig::default(),
            web_search: WebSearchConfig::default(),
        }
    }
}

fn default_domains() -> HashMap<String, DomainConfig> {
    let mut m = HashMap::new();
    m.insert(
        "base".into(),
        DomainConfig {
            description: "Generic tasks".into(),
            model_profile: ModelProfile::Fast,
            max_steps: 50,
            timeout_minutes: 5,
            options: HashMap::new(),
        },
    );
    m.insert(
        "communication".into(),
        DomainConfig {
            description: "Emails, messages, responses".into(),
            model_profile: ModelProfile::Fast,
            max_steps: 20,
            timeout_minutes: 3,
            options: HashMap::new(),
        },
    );
    m.insert(
        "code".into(),
        DomainConfig {
            description: "Programming, debugging, refactoring".into(),
            model_profile: ModelProfile::Powerful,
            max_steps: 100,
            timeout_minutes: 15,
            options: HashMap::new(),
        },
    );
    m.insert(
        "agenda".into(),
        DomainConfig {
            description: "Calendar, reminders, scheduling".into(),
            model_profile: ModelProfile::Fast,
            max_steps: 15,
            timeout_minutes: 2,
            options: HashMap::new(),
        },
    );
    m.insert(
        "files".into(),
        DomainConfig {
            description: "Document management".into(),
            model_profile: ModelProfile::Fast,
            max_steps: 30,
            timeout_minutes: 5,
            options: HashMap::new(),
        },
    );
    m.insert(
        "research".into(),
        DomainConfig {
            description: "Web search, synthesis".into(),
            model_profile: ModelProfile::Powerful,
            max_steps: 50,
            timeout_minutes: 10,
            options: HashMap::new(),
        },
    );
    m
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum OperationMode {
    AlwaysOn,
    WakeTimer,
    CloudRelay,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct OperationConfig {
    pub mode: OperationMode,
    pub wake_interval_minutes: Option<u32>,
}

impl Default for OperationConfig {
    fn default() -> Self {
        Self {
            mode: OperationMode::AlwaysOn,
            wake_interval_minutes: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ModelsConfig {
    pub providers: HashMap<String, ProviderConfig>,
    pub profiles: HashMap<String, ProfileConfig>,
    pub assignments: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub auth: AuthType,
    pub default_model: String,
    pub endpoint: Option<String>,
    /// Authoritative context-window ceiling for `default_model`. Used by
    /// the arc compactor to size the trigger and target budgets. Defaulted
    /// to 128k for legacy/UI-deserialised entries that predate the field;
    /// new entries should set this explicitly.
    #[serde(default = "default_context_window_tokens")]
    pub context_window_tokens: u32,
    /// Compact when the estimated arc-context size exceeds
    /// `context_window_tokens * compaction_trigger_pct / 100`.
    #[serde(default = "default_compaction_trigger_pct")]
    pub compaction_trigger_pct: u8,
    /// Aim for the compacted view to fit within
    /// `context_window_tokens * compaction_target_pct / 100`.
    #[serde(default = "default_compaction_target_pct")]
    pub compaction_target_pct: u8,
}

fn default_context_window_tokens() -> u32 {
    128_000
}

fn default_compaction_trigger_pct() -> u8 {
    65
}

fn default_compaction_target_pct() -> u8 {
    30
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AuthType {
    ApiKey(String),
    OAuth,
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileConfig {
    pub description: String,
    pub priority: Vec<String>,
    pub fallback: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DomainConfig {
    pub description: String,
    pub model_profile: ModelProfile,
    pub max_steps: u32,
    pub timeout_minutes: u32,
    #[serde(default)]
    pub options: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SecurityConfig {
    pub mode: SecurityMode,
    pub auto_approve_below: u32,
    pub max_steps_per_task: u32,
    pub max_task_duration_minutes: u32,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            mode: SecurityMode::Assistant,
            auto_approve_below: 20,
            max_steps_per_task: 50,
            max_task_duration_minutes: 5,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum SecurityMode {
    /// Everything L2+ needs approval
    Bunker,
    /// Standard risk evaluation
    Assistant,
    /// Only L4 needs approval
    Yolo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PersistenceConfig {
    pub db_path: PathBuf,
    pub checkpoint_interval_secs: u32,
    pub completed_retention_days: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EmailConfig {
    pub enabled: bool,
    pub imap_server: String,
    pub imap_port: u16,
    pub username: String,
    /// Stored encrypted or as app password reference
    pub password: String,
    pub use_tls: bool,
    pub folders: Vec<String>,
    pub poll_interval_secs: u64,
    /// Only process emails newer than this many hours
    pub lookback_hours: u32,
    pub smtp_server: String,
    pub smtp_port: u16,
    pub smtp_username: String,
    pub smtp_password: String,
    /// `true` = implicit TLS (typically port 465); `false` = STARTTLS upgrade (587).
    pub smtp_use_tls: bool,
    /// `"Alex <alex@example.com>"` or just `"alex@example.com"`.
    pub from_address: String,
}

impl Default for EmailConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            imap_server: String::new(),
            imap_port: 993,
            username: String::new(),
            password: String::new(),
            use_tls: true,
            folders: vec!["INBOX".to_string()],
            poll_interval_secs: 60,
            lookback_hours: 24,
            smtp_server: String::new(),
            smtp_port: 587,
            smtp_username: String::new(),
            smtp_password: String::new(),
            smtp_use_tls: false,
            from_address: String::new(),
        }
    }
}

fn default_telegram_poll_interval() -> u64 {
    5
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TelegramConfig {
    pub enabled: bool,
    pub bot_token: String,
    /// Telegram user ID of the owner (messages from this ID get AuthUser trust)
    pub owner_user_id: Option<i64>,
    /// Only process messages from these chat IDs (empty = all)
    pub allowed_chat_ids: Vec<i64>,
    /// Poll interval in seconds
    #[serde(default = "default_telegram_poll_interval")]
    pub poll_interval_secs: u64,
}

impl Default for TelegramConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bot_token: String::new(),
            owner_user_id: None,
            allowed_chat_ids: Vec::new(),
            poll_interval_secs: default_telegram_poll_interval(),
        }
    }
}

/// Notification delivery configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NotificationConfig {
    pub preferred_channels: Vec<NotificationChannelKind>,
    pub escalation_timeout_secs: u64,
    pub quiet_hours: Option<QuietHours>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuietHours {
    pub start_hour: u32,
    pub start_minute: u32,
    pub end_hour: u32,
    pub end_minute: u32,
    pub allow_critical: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum NotificationChannelKind {
    InApp,
    Telegram,
}

impl Default for NotificationConfig {
    fn default() -> Self {
        Self {
            preferred_channels: vec![
                NotificationChannelKind::InApp,
                NotificationChannelKind::Telegram,
            ],
            escalation_timeout_secs: 300,
            quiet_hours: None,
        }
    }
}

impl Default for PersistenceConfig {
    fn default() -> Self {
        Self {
            db_path: PathBuf::from("data/athen.db"),
            checkpoint_interval_secs: 30,
            completed_retention_days: 7,
        }
    }
}

/// Embedding provider configuration for the memory/RAG system.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EmbeddingConfig {
    /// Provider selection mode.
    pub mode: EmbeddingMode,
    /// Specific provider ID when mode is `Specific` (e.g. "ollama", "openai").
    pub provider: Option<String>,
    /// Model name (e.g. "nomic-embed-text", "text-embedding-3-small").
    pub model: Option<String>,
    /// Base URL for OpenAI-compatible endpoints.
    pub base_url: Option<String>,
    /// API key for cloud providers.
    pub api_key: Option<String>,
}

/// How the embedding provider is selected.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum EmbeddingMode {
    /// Auto-detect best available provider (NPU > GPU > Ollama > CPU > keyword).
    Automatic,
    /// Use a cloud provider (requires API key).
    Cloud,
    /// Force local-only (no network calls for embeddings).
    LocalOnly,
    /// Use a specific provider by ID.
    Specific,
    /// Disable memory/embeddings entirely.
    Off,
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        Self {
            mode: EmbeddingMode::Automatic,
            provider: None,
            model: None,
            base_url: None,
            api_key: None,
        }
    }
}

/// Web search provider keys. The runtime builds a quota-aware chain from
/// whichever keys are present (Brave → Tavily → DuckDuckGo as the keyless
/// floor). Empty strings mean "not configured", and the chain skips them.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WebSearchConfig {
    /// Brave Search API token (`X-Subscription-Token`). Free tier is
    /// generous (2k queries/month) and is the default first-tier provider
    /// when set.
    pub brave_api_key: String,
    /// Tavily API key. Lower free tier (~1k/month) but answer-ready snippets
    /// — used as the second tier when present.
    pub tavily_api_key: String,
}
