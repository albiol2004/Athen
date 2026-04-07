//! Embedding router with automatic fallback.
//!
//! Tries providers in priority order and falls back to the keyword
//! provider if no neural embedding provider is available.

use async_trait::async_trait;
use tracing::{debug, warn};

use athen_core::error::Result;
use athen_core::traits::embedding::EmbeddingProvider;

use super::keyword::KeywordEmbedding;

/// Routes embedding requests to the best available provider.
///
/// Tries each configured provider in priority order. If none are
/// available, falls back to the keyword embedding provider which
/// is always available.
pub struct EmbeddingRouter {
    providers: Vec<Box<dyn EmbeddingProvider>>,
    fallback: KeywordEmbedding,
}

impl EmbeddingRouter {
    /// Create a new router with the given providers in priority order.
    ///
    /// The keyword fallback is always appended automatically.
    pub fn new(providers: Vec<Box<dyn EmbeddingProvider>>) -> Self {
        Self {
            providers,
            fallback: KeywordEmbedding::new(),
        }
    }

    /// Find the first available provider, or return the fallback.
    async fn resolve(&self) -> &dyn EmbeddingProvider {
        for provider in &self.providers {
            if provider.is_available().await {
                debug!(provider = provider.provider_id(), "using embedding provider");
                return provider.as_ref();
            }
        }
        warn!("no neural embedding provider available, falling back to keyword");
        &self.fallback
    }
}

#[async_trait]
impl EmbeddingProvider for EmbeddingRouter {
    fn provider_id(&self) -> &str {
        "router"
    }

    fn dimensions(&self) -> usize {
        // Return dimensions from the first provider, or fallback.
        for provider in &self.providers {
            let d = provider.dimensions();
            if d > 0 {
                return d;
            }
        }
        self.fallback.dimensions()
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        self.resolve().await.embed(text).await
    }

    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        self.resolve().await.embed_batch(texts).await
    }

    async fn is_available(&self) -> bool {
        // The router is always available because the keyword fallback is.
        true
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_router_always_available() {
        let router = EmbeddingRouter::new(vec![]);
        assert!(router.is_available().await);
    }

    #[tokio::test]
    async fn test_router_falls_back_to_keyword() {
        let router = EmbeddingRouter::new(vec![]);
        let embedding = router.embed("hello world test").await.unwrap();
        // Keyword provider produces 384-dim vectors.
        assert_eq!(embedding.len(), 384);
    }

    #[tokio::test]
    async fn test_router_dimensions_from_fallback() {
        let router = EmbeddingRouter::new(vec![]);
        assert_eq!(router.dimensions(), 384);
    }

    #[test]
    fn test_router_provider_id() {
        let router = EmbeddingRouter::new(vec![]);
        assert_eq!(router.provider_id(), "router");
    }

    #[tokio::test]
    async fn test_router_uses_available_provider() {
        // The keyword provider is always available, so use it as a "real" provider.
        let keyword = Box::new(KeywordEmbedding::with_dimensions(128));
        let router = EmbeddingRouter::new(vec![keyword]);
        let embedding = router.embed("test text here").await.unwrap();
        // Should use the 128-dim provider, not the 384-dim fallback.
        assert_eq!(embedding.len(), 128);
    }
}
