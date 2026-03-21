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

impl Default for PersistenceConfig {
    fn default() -> Self {
        Self {
            db_path: PathBuf::from("data/athen.db"),
            checkpoint_interval_secs: 30,
            completed_retention_days: 7,
        }
    }
}
