use thiserror::Error;

#[derive(Debug, Error)]
pub enum AthenError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("Task not found: {0}")]
    TaskNotFound(String),

    #[error("Tool not found: {0}")]
    ToolNotFound(String),

    #[error("LLM provider error: {provider}: {message}")]
    LlmProvider { provider: String, message: String },

    #[error("Risk threshold exceeded: score {score}")]
    RiskThresholdExceeded { score: f64 },

    #[error("Timeout after {0:?}")]
    Timeout(std::time::Duration),

    #[error("Sandbox error: {0}")]
    Sandbox(String),

    #[error("IPC error: {0}")]
    Ipc(String),

    #[error("Configuration error: {0}")]
    Config(String),

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, AthenError>;
