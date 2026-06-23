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

    /// A *transient* LLM provider failure that is safe to retry: HTTP 429
    /// (rate limit), 5xx (server overloaded / gateway), or a connection
    /// reset / dropped socket. Distinct from [`AthenError::LlmProvider`],
    /// which carries permanent failures (auth, bad request, unknown model,
    /// context-length, content filter) that must NOT be retried.
    ///
    /// `retry_after_secs` carries a provider-supplied `Retry-After` hint
    /// (parsed from the header on a 429/503) when present; the router caps
    /// it to a sane maximum before honoring it.
    #[error("LLM provider transient error: {provider}: {message}")]
    LlmTransient {
        provider: String,
        message: String,
        retry_after_secs: Option<u64>,
    },

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

    /// Whether this error represents a *transient* failure that is safe to
    /// retry with backoff. True for [`AthenError::LlmTransient`] (429 / 5xx /
    /// connection reset) and [`AthenError::Timeout`]. False for everything
    /// else — in particular auth, bad-request, unknown-model, and
    /// context-length failures (carried as [`AthenError::LlmProvider`]) must
    /// fail fast: retrying them wastes time and money.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            AthenError::LlmTransient { .. } | AthenError::Timeout(_)
        )
    }

    /// Provider-supplied `Retry-After` hint in whole seconds, if this error
    /// carries one (only [`AthenError::LlmTransient`] does). The router treats
    /// this as advisory and caps it before sleeping.
    pub fn retry_after_secs(&self) -> Option<u64> {
        match self {
            AthenError::LlmTransient {
                retry_after_secs, ..
            } => *retry_after_secs,
            _ => None,
        }
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
    fn is_retryable_classifies_transient_and_timeout_only() {
        let transient = AthenError::LlmTransient {
            provider: "openai".into(),
            message: "rate limited".into(),
            retry_after_secs: Some(3),
        };
        assert!(transient.is_retryable());
        assert_eq!(transient.retry_after_secs(), Some(3));

        let timeout = AthenError::Timeout(std::time::Duration::from_secs(120));
        assert!(timeout.is_retryable());
        assert_eq!(timeout.retry_after_secs(), None);

        // Permanent provider errors (auth, bad request, unknown model) are
        // carried as LlmProvider and must NOT be retried.
        let permanent = AthenError::LlmProvider {
            provider: "openai".into(),
            message: "auth error: bad key".into(),
        };
        assert!(!permanent.is_retryable());
        assert_eq!(permanent.retry_after_secs(), None);

        let other = AthenError::Config("nope".into());
        assert!(!other.is_retryable());
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
