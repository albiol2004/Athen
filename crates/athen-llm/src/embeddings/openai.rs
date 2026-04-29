//! OpenAI-compatible embedding provider.
//!
//! Works with any server that exposes the OpenAI `/v1/embeddings` endpoint:
//! OpenAI itself, vLLM, LiteLLM, text-generation-webui, and any other
//! compatible endpoint. The API key is optional for local servers.

use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::debug;

use athen_core::error::{AthenError, Result};
use athen_core::traits::embedding::EmbeddingProvider;

const DEFAULT_BASE_URL: &str = "https://api.openai.com";
const DEFAULT_MODEL: &str = "text-embedding-3-small";
const DEFAULT_DIMENSIONS: usize = 1536;

/// OpenAI-compatible embedding provider.
///
/// Works with any server that implements the `/v1/embeddings` endpoint.
/// The API key is optional — local servers (vLLM, LiteLLM) typically
/// don't require one.
///
/// # Examples
///
/// ```rust,no_run
/// use athen_llm::embeddings::openai::OpenAiEmbedding;
///
/// // OpenAI proper
/// let openai = OpenAiEmbedding::openai("sk-...");
///
/// // Any OpenAI-compatible endpoint (no auth)
/// let local = OpenAiEmbedding::compatible("http://localhost:8080")
///     .with_model("bge-large-en");
/// ```
pub struct OpenAiEmbedding {
    client: Client,
    base_url: String,
    model: String,
    api_key: Option<String>,
    provider_id: String,
    /// Cached dimension count. Set to a known default for OpenAI models,
    /// updated from the first response for unknown models.
    dimensions: std::sync::RwLock<usize>,
}

impl OpenAiEmbedding {
    /// Create a provider for OpenAI proper (with API key).
    ///
    /// Uses `text-embedding-3-small` (1536 dimensions) by default.
    pub fn openai(api_key: &str) -> Self {
        Self {
            client: Client::new(),
            base_url: DEFAULT_BASE_URL.to_string(),
            model: DEFAULT_MODEL.to_string(),
            api_key: Some(api_key.to_string()),
            provider_id: "openai".to_string(),
            dimensions: std::sync::RwLock::new(DEFAULT_DIMENSIONS),
        }
    }

    /// Create a provider for any OpenAI-compatible endpoint.
    ///
    /// No API key or model is set — configure with [`with_api_key`] and
    /// [`with_model`] as needed. Dimensions are auto-detected on first call.
    pub fn compatible(base_url: &str) -> Self {
        Self {
            client: Client::new(),
            base_url: base_url.to_string(),
            model: "default".to_string(),
            api_key: None,
            provider_id: "openai-compatible".to_string(),
            dimensions: std::sync::RwLock::new(0),
        }
    }

    /// Set the API key. The `Authorization: Bearer` header is only sent
    /// when an API key is present.
    pub fn with_api_key(mut self, key: &str) -> Self {
        self.api_key = Some(key.to_string());
        self
    }

    /// Override the default model name.
    pub fn with_model(mut self, model: &str) -> Self {
        self.model = model.to_string();
        // Update default dimensions for known OpenAI models.
        let dim = known_dimensions(model);
        if dim > 0 {
            if let Ok(mut d) = self.dimensions.write() {
                *d = dim;
            }
        }
        self
    }

    /// Override the provider identifier.
    pub fn with_provider_id(mut self, id: &str) -> Self {
        self.provider_id = id.to_string();
        self
    }

    /// Override the HTTP client (useful for testing or custom TLS configs).
    pub fn with_client(mut self, client: Client) -> Self {
        self.client = client;
        self
    }

    /// Build the HTTP request with optional auth header.
    fn build_request(&self, url: &str, body: &EmbeddingRequest) -> reqwest::RequestBuilder {
        let mut req = self
            .client
            .post(url)
            .header("Content-Type", "application/json")
            .json(body);

        if let Some(ref key) = self.api_key {
            req = req.header("Authorization", format!("Bearer {}", key));
        }

        req
    }

    /// Call the `/v1/embeddings` endpoint.
    async fn call_embed(&self, input: Vec<String>) -> Result<Vec<Vec<f32>>> {
        let url = format!("{}/v1/embeddings", self.base_url);
        let body = EmbeddingRequest {
            model: self.model.clone(),
            input,
        };

        debug!(
            url = %url,
            model = %self.model,
            "sending OpenAI embedding request"
        );

        let response =
            self.build_request(&url, &body)
                .send()
                .await
                .map_err(|e| AthenError::LlmProvider {
                    provider: self.provider_id.clone(),
                    message: format!("embedding request failed: {}", e),
                })?;

        let status = response.status();
        if !status.is_success() {
            let error_body = response.text().await.unwrap_or_default();
            let message = if status == reqwest::StatusCode::UNAUTHORIZED {
                format!("auth error: {}", error_body)
            } else if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                format!("rate_limit: {}", error_body)
            } else {
                format!("HTTP {}: {}", status, error_body)
            };
            return Err(AthenError::LlmProvider {
                provider: self.provider_id.clone(),
                message,
            });
        }

        let embed_response: EmbeddingResponse =
            response.json().await.map_err(|e| AthenError::LlmProvider {
                provider: self.provider_id.clone(),
                message: format!("failed to parse embedding response: {}", e),
            })?;

        // Sort by index to ensure correct ordering.
        let mut data = embed_response.data;
        data.sort_by_key(|d| d.index);

        let embeddings: Vec<Vec<f32>> = data.into_iter().map(|d| d.embedding).collect();

        // Cache dimensions from first response.
        if let Some(first) = embeddings.first() {
            let dim = first.len();
            if dim > 0 {
                if let Ok(mut cached) = self.dimensions.write() {
                    if *cached == 0 {
                        *cached = dim;
                        debug!(dimensions = dim, "auto-detected embedding dimensions");
                    }
                }
            }
        }

        Ok(embeddings)
    }
}

#[async_trait]
impl EmbeddingProvider for OpenAiEmbedding {
    fn provider_id(&self) -> &str {
        &self.provider_id
    }

    fn dimensions(&self) -> usize {
        self.dimensions.read().map(|d| *d).unwrap_or(0)
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let results = self.call_embed(vec![text.to_string()]).await?;
        results
            .into_iter()
            .next()
            .ok_or_else(|| AthenError::LlmProvider {
                provider: self.provider_id.clone(),
                message: "embedding response contained no data".to_string(),
            })
    }

    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        self.call_embed(texts.to_vec()).await
    }

    async fn is_available(&self) -> bool {
        // For cloud providers with an API key, assume available.
        if self.api_key.is_some() && self.base_url == DEFAULT_BASE_URL {
            return true;
        }

        // For local/custom endpoints, try to reach the server.
        let url = format!("{}/v1/models", self.base_url);
        debug!(url = %url, "checking OpenAI embedding availability");
        match self.client.get(&url).send().await {
            Ok(resp) => resp.status().is_success(),
            Err(_) => false,
        }
    }
}

/// Return known dimensions for common OpenAI embedding models.
fn known_dimensions(model: &str) -> usize {
    if model.contains("text-embedding-3-large") {
        3072
    } else if model.contains("text-embedding-3-small") || model.contains("text-embedding-ada-002") {
        1536
    } else {
        0
    }
}

// ---------------------------------------------------------------------------
// OpenAI embeddings API wire types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct EmbeddingRequest {
    model: String,
    input: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct EmbeddingResponse {
    data: Vec<EmbeddingData>,
    #[allow(dead_code)]
    model: String,
    #[allow(dead_code)]
    usage: Option<EmbeddingUsage>,
}

#[derive(Debug, Deserialize)]
struct EmbeddingData {
    embedding: Vec<f32>,
    index: usize,
}

#[derive(Debug, Deserialize)]
struct EmbeddingUsage {
    #[allow(dead_code)]
    prompt_tokens: u32,
    #[allow(dead_code)]
    total_tokens: u32,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_provider_id_openai() {
        let provider = OpenAiEmbedding::openai("sk-test");
        assert_eq!(provider.provider_id(), "openai");
    }

    #[test]
    fn test_provider_id_compatible() {
        let provider = OpenAiEmbedding::compatible("http://localhost:8080");
        assert_eq!(provider.provider_id(), "openai-compatible");
    }

    #[test]
    fn test_custom_provider_id() {
        let provider =
            OpenAiEmbedding::compatible("http://localhost:8080").with_provider_id("vllm");
        assert_eq!(provider.provider_id(), "vllm");
    }

    #[test]
    fn test_openai_default_dimensions() {
        let provider = OpenAiEmbedding::openai("sk-test");
        assert_eq!(provider.dimensions(), 1536);
    }

    #[test]
    fn test_dimensions_for_known_models() {
        let p1 = OpenAiEmbedding::openai("sk-test").with_model("text-embedding-3-large");
        assert_eq!(p1.dimensions(), 3072);

        let p2 = OpenAiEmbedding::openai("sk-test").with_model("text-embedding-3-small");
        assert_eq!(p2.dimensions(), 1536);

        let p3 = OpenAiEmbedding::openai("sk-test").with_model("text-embedding-ada-002");
        assert_eq!(p3.dimensions(), 1536);
    }

    #[test]
    fn test_compatible_dimensions_initially_zero() {
        let provider = OpenAiEmbedding::compatible("http://localhost:8080");
        assert_eq!(provider.dimensions(), 0);
    }

    #[test]
    fn test_compatible_no_auth() {
        let provider = OpenAiEmbedding::compatible("http://localhost:8080");
        assert!(provider.api_key.is_none());
    }

    #[test]
    fn test_embed_request_format() {
        let body = EmbeddingRequest {
            model: "text-embedding-3-small".to_string(),
            input: vec!["hello world".to_string()],
        };
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["model"], "text-embedding-3-small");
        assert_eq!(json["input"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn test_embed_response_parsing() {
        let json = r#"{
            "data": [
                {"embedding": [0.1, 0.2, 0.3], "index": 0},
                {"embedding": [0.4, 0.5, 0.6], "index": 1}
            ],
            "model": "text-embedding-3-small",
            "usage": {"prompt_tokens": 5, "total_tokens": 5}
        }"#;
        let resp: EmbeddingResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.data.len(), 2);
        assert_eq!(resp.data[0].index, 0);
        assert_eq!(resp.data[0].embedding.len(), 3);
    }

    #[test]
    fn test_embed_response_reorders_by_index() {
        // Responses may arrive out of order.
        let json = r#"{
            "data": [
                {"embedding": [0.4, 0.5], "index": 1},
                {"embedding": [0.1, 0.2], "index": 0}
            ],
            "model": "test",
            "usage": null
        }"#;
        let resp: EmbeddingResponse = serde_json::from_str(json).unwrap();
        let mut data = resp.data;
        data.sort_by_key(|d| d.index);
        assert!((data[0].embedding[0] - 0.1).abs() < f32::EPSILON);
        assert!((data[1].embedding[0] - 0.4).abs() < f32::EPSILON);
    }

    #[tokio::test]
    async fn test_is_available_openai_with_key() {
        let provider = OpenAiEmbedding::openai("sk-test");
        // Cloud provider with key is assumed available.
        assert!(provider.is_available().await);
    }

    #[tokio::test]
    async fn test_is_available_local_not_running() {
        let provider = OpenAiEmbedding::compatible("http://127.0.0.1:19999");
        assert!(!provider.is_available().await);
    }

    #[tokio::test]
    async fn test_embed_batch_empty() {
        let provider = OpenAiEmbedding::compatible("http://127.0.0.1:19999");
        let result = provider.embed_batch(&[]).await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_empty());
    }

    #[test]
    fn test_known_dimensions() {
        assert_eq!(known_dimensions("text-embedding-3-small"), 1536);
        assert_eq!(known_dimensions("text-embedding-3-large"), 3072);
        assert_eq!(known_dimensions("text-embedding-ada-002"), 1536);
        assert_eq!(known_dimensions("some-unknown-model"), 0);
    }
}
