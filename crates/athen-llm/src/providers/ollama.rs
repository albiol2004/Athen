//! Ollama (local models) provider adapter.

use async_trait::async_trait;
use reqwest::Client;

use athen_core::error::{AthenError, Result};
use athen_core::llm::*;
use athen_core::traits::llm::LlmProvider;

const DEFAULT_BASE_URL: &str = "http://localhost:11434";

/// Ollama provider for local model inference.
#[allow(dead_code)]
pub struct OllamaProvider {
    default_model: String,
    client: Client,
    base_url: String,
}

impl OllamaProvider {
    /// Create a new Ollama provider.
    pub fn new(default_model: String) -> Self {
        Self {
            default_model,
            client: Client::new(),
            base_url: DEFAULT_BASE_URL.to_string(),
        }
    }

    /// Override the base URL.
    pub fn with_base_url(mut self, url: String) -> Self {
        self.base_url = url;
        self
    }
}

#[async_trait]
impl LlmProvider for OllamaProvider {
    fn provider_id(&self) -> &str {
        "ollama"
    }

    async fn complete(&self, _request: &LlmRequest) -> Result<LlmResponse> {
        Err(AthenError::LlmProvider {
            provider: "ollama".into(),
            message: "not yet implemented".into(),
        })
    }

    async fn complete_streaming(&self, _request: &LlmRequest) -> Result<LlmStream> {
        Err(AthenError::LlmProvider {
            provider: "ollama".into(),
            message: "not yet implemented".into(),
        })
    }

    async fn is_available(&self) -> bool {
        // In a real implementation, this would ping the Ollama server.
        false
    }
}
