use async_trait::async_trait;

use crate::error::Result;

/// A provider that converts text into vector embeddings.
///
/// Implementations include local models (ONNX, Ollama), cloud APIs
/// (OpenAI, OpenAI-compatible), and a keyword fallback (TF-IDF).
#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    /// Unique identifier for this provider (e.g. "ollama", "openai", "onnx").
    fn provider_id(&self) -> &str;

    /// Dimensionality of the vectors produced by this provider.
    fn dimensions(&self) -> usize;

    /// Generate an embedding for a single text input.
    async fn embed(&self, text: &str) -> Result<Vec<f32>>;

    /// Generate embeddings for multiple texts in a single call.
    /// Default implementation calls `embed()` sequentially.
    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let mut results = Vec::with_capacity(texts.len());
        for text in texts {
            results.push(self.embed(text).await?);
        }
        Ok(results)
    }

    /// Check if this provider is currently available (e.g. service running).
    async fn is_available(&self) -> bool;
}
