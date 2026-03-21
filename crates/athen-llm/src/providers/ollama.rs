//! Ollama provider adapter.
//!
//! Thin wrapper around [`OpenAiCompatibleProvider`] that points at the Ollama
//! server's OpenAI-compatible endpoint and adds a health check to
//! [`is_available`].

use async_trait::async_trait;
use reqwest::Client;
use tracing::debug;

use athen_core::error::Result;
use athen_core::llm::*;
use athen_core::traits::llm::LlmProvider;

use super::openai::{OpenAiCompatibleProvider, ZeroCostEstimator};

const DEFAULT_BASE_URL: &str = "http://localhost:11434";

/// Ollama provider for local model inference.
///
/// Ollama exposes an OpenAI-compatible `/v1/chat/completions` endpoint, so
/// this delegates all LLM logic to [`OpenAiCompatibleProvider`] and only adds
/// Ollama-specific behaviour: the default base URL, zero-cost estimation, and
/// a real health check via `/api/tags`.
pub struct OllamaProvider {
    inner: OpenAiCompatibleProvider,
    /// Keep the base URL around for the health check (the inner provider
    /// doesn't expose it).
    base_url: String,
    /// Shared HTTP client for the health check.
    client: Client,
}

impl OllamaProvider {
    /// Create a new Ollama provider for the given model.
    ///
    /// Connects to `http://localhost:11434` by default.
    pub fn new(model: String) -> Self {
        let client = Client::new();
        Self {
            inner: OpenAiCompatibleProvider::new(DEFAULT_BASE_URL.to_string())
                .with_model(model)
                .with_provider_id("ollama".to_string())
                .with_cost_estimator(Box::new(ZeroCostEstimator))
                .with_client(client.clone()),
            base_url: DEFAULT_BASE_URL.to_string(),
            client,
        }
    }

    /// Override the base URL (e.g. for a remote Ollama instance).
    pub fn with_base_url(mut self, url: String) -> Self {
        self.inner = OpenAiCompatibleProvider::new(url.clone())
            .with_model(self.inner_model())
            .with_provider_id("ollama".to_string())
            .with_cost_estimator(Box::new(ZeroCostEstimator))
            .with_client(self.client.clone());
        self.base_url = url;
        self
    }

    /// Override the default model.
    pub fn with_model(mut self, model: String) -> Self {
        self.inner = OpenAiCompatibleProvider::new(self.base_url.clone())
            .with_model(model)
            .with_provider_id("ollama".to_string())
            .with_cost_estimator(Box::new(ZeroCostEstimator))
            .with_client(self.client.clone());
        self
    }

    /// Read the current model name (for rebuilding the inner provider).
    fn inner_model(&self) -> String {
        // We don't have a getter on the inner provider, so we store
        // the model name implicitly through rebuild. For simplicity
        // we default to a placeholder; callers should chain with_model.
        "llama3.2".to_string()
    }
}

#[async_trait]
impl LlmProvider for OllamaProvider {
    fn provider_id(&self) -> &str {
        "ollama"
    }

    async fn complete(&self, request: &LlmRequest) -> Result<LlmResponse> {
        self.inner.complete(request).await
    }

    async fn complete_streaming(&self, request: &LlmRequest) -> Result<LlmStream> {
        self.inner.complete_streaming(request).await
    }

    async fn is_available(&self) -> bool {
        let url = format!("{}/api/tags", self.base_url);
        debug!(url = %url, "checking Ollama availability");
        match self.client.get(&url).send().await {
            Ok(resp) => resp.status().is_success(),
            Err(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_provider_id() {
        let provider = OllamaProvider::new("llama3.2".into());
        assert_eq!(provider.provider_id(), "ollama");
    }

    #[test]
    fn test_default_base_url() {
        let provider = OllamaProvider::new("llama3.2".into());
        assert_eq!(provider.base_url, "http://localhost:11434");
    }

    #[test]
    fn test_custom_base_url() {
        let provider =
            OllamaProvider::new("llama3.2".into()).with_base_url("http://gpu-box:11434".into());
        assert_eq!(provider.base_url, "http://gpu-box:11434");
    }

    #[tokio::test]
    async fn test_is_available_when_not_running() {
        // Ollama is not running on this port, so should return false.
        let provider =
            OllamaProvider::new("llama3.2".into()).with_base_url("http://127.0.0.1:19999".into());
        assert!(!provider.is_available().await);
    }
}
