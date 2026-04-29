//! llama.cpp provider adapter.
//!
//! Thin wrapper around [`OpenAiCompatibleProvider`] for llama.cpp's
//! OpenAI-compatible server (`llama-server --port 8080`).

use async_trait::async_trait;
use reqwest::Client;
use tracing::debug;

use athen_core::error::Result;
use athen_core::llm::*;
use athen_core::traits::llm::LlmProvider;

use super::openai::{OpenAiCompatibleProvider, ZeroCostEstimator};

const DEFAULT_BASE_URL: &str = "http://localhost:8080";

/// llama.cpp provider for local model inference.
///
/// llama.cpp's `llama-server` exposes an OpenAI-compatible
/// `/v1/chat/completions` endpoint, so this delegates all LLM logic to
/// [`OpenAiCompatibleProvider`] and only adds llama.cpp-specific behaviour:
/// the default base URL, zero-cost estimation, and a real health check via
/// `/health`.
pub struct LlamaCppProvider {
    inner: OpenAiCompatibleProvider,
    /// Keep the base URL for the health check.
    base_url: String,
    /// Shared HTTP client for the health check.
    client: Client,
}

impl LlamaCppProvider {
    /// Create a new llama.cpp provider.
    ///
    /// Connects to `http://localhost:8080` by default.
    pub fn new(base_url: String, model: String) -> Self {
        let client = Client::new();
        Self {
            inner: OpenAiCompatibleProvider::new(base_url.clone())
                .with_model(model)
                .with_provider_id("llamacpp".to_string())
                .with_cost_estimator(Box::new(ZeroCostEstimator))
                .with_client(client.clone()),
            base_url,
            client,
        }
    }

    /// Create a provider using the default base URL (`http://localhost:8080`).
    pub fn localhost(model: String) -> Self {
        Self::new(DEFAULT_BASE_URL.to_string(), model)
    }

    /// Override the HTTP client (useful for testing).
    pub fn with_client(mut self, client: Client) -> Self {
        self.client = client.clone();
        self.inner = OpenAiCompatibleProvider::new(self.base_url.clone())
            .with_model(self.inner.provider_id().to_string()) // rebuild
            .with_provider_id("llamacpp".to_string())
            .with_cost_estimator(Box::new(ZeroCostEstimator))
            .with_client(client);
        self
    }
}

#[async_trait]
impl LlmProvider for LlamaCppProvider {
    fn provider_id(&self) -> &str {
        "llamacpp"
    }

    async fn complete(&self, request: &LlmRequest) -> Result<LlmResponse> {
        self.inner.complete(request).await
    }

    async fn complete_streaming(&self, request: &LlmRequest) -> Result<LlmStream> {
        self.inner.complete_streaming(request).await
    }

    async fn is_available(&self) -> bool {
        let url = format!("{}/health", self.base_url);
        debug!(url = %url, "checking llama.cpp availability");
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
        let provider = LlamaCppProvider::localhost("my-model".into());
        assert_eq!(provider.provider_id(), "llamacpp");
    }

    #[test]
    fn test_default_base_url() {
        let provider = LlamaCppProvider::localhost("my-model".into());
        assert_eq!(provider.base_url, "http://localhost:8080");
    }

    #[test]
    fn test_custom_base_url() {
        let provider = LlamaCppProvider::new("http://gpu:9090".into(), "my-model".into());
        assert_eq!(provider.base_url, "http://gpu:9090");
    }

    #[tokio::test]
    async fn test_is_available_when_not_running() {
        let provider = LlamaCppProvider::new("http://127.0.0.1:19998".into(), "model".into());
        assert!(!provider.is_available().await);
    }
}
