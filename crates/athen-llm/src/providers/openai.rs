//! OpenAI provider adapter.

use async_trait::async_trait;
use reqwest::Client;

use athen_core::error::{AthenError, Result};
use athen_core::llm::*;
use athen_core::traits::llm::LlmProvider;

const DEFAULT_BASE_URL: &str = "https://api.openai.com";

/// OpenAI LLM provider (GPT-4, o3, etc.).
#[allow(dead_code)]
pub struct OpenAiProvider {
    api_key: String,
    default_model: String,
    client: Client,
    base_url: String,
}

impl OpenAiProvider {
    /// Create a new OpenAI provider.
    pub fn new(api_key: String, default_model: String) -> Self {
        Self {
            api_key,
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
impl LlmProvider for OpenAiProvider {
    fn provider_id(&self) -> &str {
        "openai"
    }

    async fn complete(&self, _request: &LlmRequest) -> Result<LlmResponse> {
        Err(AthenError::LlmProvider {
            provider: "openai".into(),
            message: "not yet implemented".into(),
        })
    }

    async fn complete_streaming(&self, _request: &LlmRequest) -> Result<LlmStream> {
        Err(AthenError::LlmProvider {
            provider: "openai".into(),
            message: "not yet implemented".into(),
        })
    }

    async fn is_available(&self) -> bool {
        !self.api_key.is_empty()
    }
}
