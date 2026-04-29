//! Ollama embedding provider.
//!
//! Uses the Ollama `/api/embed` endpoint which supports batch embedding.
//! Dimensions are auto-detected on the first call and cached.

use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::debug;

use athen_core::error::{AthenError, Result};
use athen_core::traits::embedding::EmbeddingProvider;

const DEFAULT_BASE_URL: &str = "http://localhost:11434";

/// Ollama embedding provider for local vector generation.
///
/// Connects to a running Ollama instance and uses the `/api/embed`
/// endpoint. Dimensions are auto-detected from the first response
/// and cached for subsequent calls.
///
/// # Examples
///
/// ```rust,no_run
/// use athen_llm::embeddings::ollama::OllamaEmbedding;
///
/// let provider = OllamaEmbedding::new("nomic-embed-text")
///     .with_base_url("http://gpu-box:11434");
/// ```
pub struct OllamaEmbedding {
    client: Client,
    base_url: String,
    model: String,
    /// Cached dimension count, detected on first embed call.
    dimensions: std::sync::RwLock<usize>,
}

impl OllamaEmbedding {
    /// Create a new Ollama embedding provider for the given model.
    ///
    /// Connects to `http://localhost:11434` by default.
    pub fn new(model: &str) -> Self {
        Self {
            client: Client::new(),
            base_url: DEFAULT_BASE_URL.to_string(),
            model: model.to_string(),
            dimensions: std::sync::RwLock::new(0),
        }
    }

    /// Override the base URL (e.g. for a remote Ollama instance).
    pub fn with_base_url(mut self, url: &str) -> Self {
        self.base_url = url.to_string();
        self
    }

    /// Override the HTTP client (useful for testing).
    pub fn with_client(mut self, client: Client) -> Self {
        self.client = client;
        self
    }

    /// Call the Ollama `/api/embed` endpoint with the given inputs.
    async fn call_embed(&self, input: Vec<String>) -> Result<Vec<Vec<f32>>> {
        let url = format!("{}/api/embed", self.base_url);
        let body = OllamaEmbedRequest {
            model: self.model.clone(),
            input,
        };

        debug!(
            url = %url,
            model = %self.model,
            "sending Ollama embed request"
        );

        let response = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| AthenError::LlmProvider {
                provider: "ollama-embed".to_string(),
                message: format!("embed request failed: {}", e),
            })?;

        let status = response.status();
        if !status.is_success() {
            let error_body = response.text().await.unwrap_or_default();
            return Err(AthenError::LlmProvider {
                provider: "ollama-embed".to_string(),
                message: format!("HTTP {}: {}", status, error_body),
            });
        }

        let embed_response: OllamaEmbedResponse =
            response.json().await.map_err(|e| AthenError::LlmProvider {
                provider: "ollama-embed".to_string(),
                message: format!("failed to parse embed response: {}", e),
            })?;

        // Cache dimensions from first response.
        if let Some(first) = embed_response.embeddings.first() {
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

        Ok(embed_response.embeddings)
    }
}

#[async_trait]
impl EmbeddingProvider for OllamaEmbedding {
    fn provider_id(&self) -> &str {
        "ollama"
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
                provider: "ollama-embed".to_string(),
                message: "embed response contained no embeddings".to_string(),
            })
    }

    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        self.call_embed(texts.to_vec()).await
    }

    async fn is_available(&self) -> bool {
        // Check if Ollama is running and our model is available.
        let url = format!("{}/api/tags", self.base_url);
        debug!(url = %url, "checking Ollama embedding availability");

        let resp = match self.client.get(&url).send().await {
            Ok(r) => r,
            Err(_) => return false,
        };

        if !resp.status().is_success() {
            return false;
        }

        // Check if our model is in the list.
        if let Ok(body) = resp.json::<serde_json::Value>().await {
            if let Some(models) = body.get("models").and_then(|m| m.as_array()) {
                return models.iter().any(|m| {
                    m.get("name")
                        .and_then(|n| n.as_str())
                        .map(|n| n.starts_with(&self.model))
                        .unwrap_or(false)
                });
            }
        }

        false
    }
}

// ---------------------------------------------------------------------------
// Ollama embed API wire types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct OllamaEmbedRequest {
    model: String,
    input: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct OllamaEmbedResponse {
    #[serde(default)]
    embeddings: Vec<Vec<f32>>,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_provider_id() {
        let provider = OllamaEmbedding::new("nomic-embed-text");
        assert_eq!(provider.provider_id(), "ollama");
    }

    #[test]
    fn test_default_base_url() {
        let provider = OllamaEmbedding::new("nomic-embed-text");
        assert_eq!(provider.base_url, "http://localhost:11434");
    }

    #[test]
    fn test_custom_base_url() {
        let provider =
            OllamaEmbedding::new("nomic-embed-text").with_base_url("http://gpu-box:11434");
        assert_eq!(provider.base_url, "http://gpu-box:11434");
    }

    #[test]
    fn test_dimensions_initially_zero() {
        let provider = OllamaEmbedding::new("nomic-embed-text");
        assert_eq!(provider.dimensions(), 0);
    }

    #[tokio::test]
    async fn test_is_available_when_not_running() {
        let provider =
            OllamaEmbedding::new("nomic-embed-text").with_base_url("http://127.0.0.1:19999");
        assert!(!provider.is_available().await);
    }

    #[test]
    fn test_embed_request_format() {
        let body = OllamaEmbedRequest {
            model: "nomic-embed-text".to_string(),
            input: vec!["hello world".to_string(), "test".to_string()],
        };
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["model"], "nomic-embed-text");
        assert_eq!(json["input"].as_array().unwrap().len(), 2);
        assert_eq!(json["input"][0], "hello world");
    }

    #[test]
    fn test_embed_response_parsing() {
        let json = r#"{"embeddings": [[0.1, 0.2, 0.3], [0.4, 0.5, 0.6]]}"#;
        let resp: OllamaEmbedResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.embeddings.len(), 2);
        assert_eq!(resp.embeddings[0].len(), 3);
        assert!((resp.embeddings[0][0] - 0.1).abs() < f32::EPSILON);
    }

    #[test]
    fn test_embed_response_empty() {
        let json = r#"{"embeddings": []}"#;
        let resp: OllamaEmbedResponse = serde_json::from_str(json).unwrap();
        assert!(resp.embeddings.is_empty());
    }

    #[tokio::test]
    async fn test_embed_batch_empty() {
        // With an unreachable server, empty batch should return Ok(empty).
        let provider =
            OllamaEmbedding::new("nomic-embed-text").with_base_url("http://127.0.0.1:19999");
        let result = provider.embed_batch(&[]).await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_empty());
    }
}
