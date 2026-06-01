use thiserror::Error;

use crate::redaction::redact_known_secret_shapes;

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

    #[error("Vault error: {0}")]
    Vault(String),

    #[error("{0}")]
    Other(String),
}

impl AthenError {
    /// Render this error for user-facing channels without exposing
    /// secret-looking values that may have been included by a provider,
    /// tool, shell process, or third-party API.
    ///
    /// This does not replace structured logging or debugging output; it is a
    /// safe default for UI, Telegram, email, and other external surfaces.
    pub fn user_safe_message(&self) -> String {
        redact_known_secret_shapes(&self.to_string())
    }
}

pub type Result<T> = std::result::Result<T, AthenError>;

#[cfg(test)]
mod tests {
    use super::AthenError;

    fn joined(parts: &[&str]) -> String {
        parts.concat()
    }

    fn token(prefix_parts: &[&str], body: &str) -> String {
        format!("{}{}", joined(prefix_parts), body)
    }

    #[test]
    fn user_safe_message_redacts_provider_errors() {
        let secret = token(&["s", "k", "-"], "1234567890abcdefghijklmnopqrstuvwxyz");
        let err = AthenError::LlmProvider {
            provider: "openai-compatible".into(),
            message: format!("request failed while using {secret}"),
        };

        let rendered = err.user_safe_message();

        assert!(rendered.contains("sk-…[redacted]"));
        assert!(!rendered.contains("abcdefghijklmnopqrstuvwxyz"));
    }

    #[test]
    fn user_safe_message_redacts_configuration_errors() {
        let secret = token(&["github", "_pat", "_"], "1234567890_SECRET_PART");
        let err = AthenError::Config(format!("invalid GitHub token {secret}"));

        let rendered = err.user_safe_message();

        assert!(rendered.contains("github_pat_…[redacted]"));
        assert!(!rendered.contains("SECRET_PART"));
    }

    #[test]
    fn user_safe_message_preserves_non_sensitive_errors() {
        let err = AthenError::ToolNotFound("calendar.create_event".into());

        assert_eq!(
            err.user_safe_message(),
            "Tool not found: calendar.create_event"
        );
    }
}
