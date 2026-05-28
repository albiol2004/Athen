//! Bundled local embedding provider (fastembed-rs / ONNX).
//!
//! Provides a pure-Rust multilingual embedder so non-technical users
//! get semantic recall out of the box, without installing Ollama or
//! signing up for OpenAI. The ONNX weights are downloaded to disk
//! on first `embed()` call (~270 MB) and cached under the configured
//! cache directory; subsequent runs load from cache.
//!
//! Phase 1 hardwires a single model — `intfloat/multilingual-e5-small`
//! — and is gated behind the `bundled-embeddings` cargo feature **and**
//! the `ATHEN_BUNDLED_EMBEDDINGS=1` env var at construction time. A UI
//! tier picker comes in a later phase.

#![cfg(feature = "bundled-embeddings")]

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::{Mutex, OnceCell};
use tracing::{debug, info};

use athen_core::error::{AthenError, Result};
use athen_core::traits::embedding::EmbeddingProvider;

use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};

/// Stable provider id surfaced through `EmbeddingProvider::provider_id`.
const PROVIDER_ID: &str = "bundled-e5-small";

/// Stable model id (informational; not part of the trait surface).
const MODEL_ID: &str = "multilingual-e5-small";

/// Output dimensionality for `intfloat/multilingual-e5-small`.
const DIMENSIONS: usize = 384;

/// Bundled local embedding provider backed by fastembed-rs.
///
/// Construction is cheap (just stores the cache path). The underlying
/// ONNX session is initialised lazily on the first `embed()` call so
/// users who never trigger memory recall in a session don't pay the
/// startup cost — or the 270 MB download.
pub struct BundledEmbedding {
    cache_dir: PathBuf,
    /// Lazily-initialised model handle. `embed()` requires `&mut self`
    /// on `TextEmbedding`, so we wrap in a `Mutex`. The `OnceCell`
    /// ensures we only pay the load/download cost once per process.
    inner: OnceCell<Arc<Mutex<TextEmbedding>>>,
}

impl BundledEmbedding {
    /// Construct a new bundled embedder. The cache directory will be
    /// created on first use if it does not already exist.
    pub fn new(cache_dir: PathBuf) -> Self {
        Self {
            cache_dir,
            inner: OnceCell::new(),
        }
    }

    /// Informational model identifier (not part of the trait surface
    /// today; surfaced for future logging / UI hooks).
    pub fn model_id(&self) -> &str {
        MODEL_ID
    }

    /// Ensure the underlying fastembed `TextEmbedding` is initialised.
    /// First call may download ~270 MB; subsequent calls are cached.
    async fn ensure_model(&self) -> Result<Arc<Mutex<TextEmbedding>>> {
        let cache_dir = self.cache_dir.clone();
        let handle = self
            .inner
            .get_or_try_init(|| async move {
                debug!(
                    cache_dir = %cache_dir.display(),
                    model = MODEL_ID,
                    "initialising bundled embedding model"
                );
                // fastembed is synchronous and may block on I/O
                // (download + ONNX session build) — keep the async
                // runtime free.
                let model = tokio::task::spawn_blocking(move || {
                    TextEmbedding::try_new(
                        InitOptions::new(EmbeddingModel::MultilingualE5Small)
                            .with_cache_dir(cache_dir)
                            .with_show_download_progress(true),
                    )
                })
                .await
                .map_err(|e| AthenError::LlmProvider {
                    provider: PROVIDER_ID.to_string(),
                    message: format!("model init task panicked: {}", e),
                })?
                .map_err(|e| AthenError::LlmProvider {
                    provider: PROVIDER_ID.to_string(),
                    message: format!("fastembed init failed: {}", e),
                })?;
                Ok::<_, AthenError>(Arc::new(Mutex::new(model)))
            })
            .await?;
        Ok(handle.clone())
    }
}

#[async_trait]
impl EmbeddingProvider for BundledEmbedding {
    fn provider_id(&self) -> &str {
        PROVIDER_ID
    }

    fn dimensions(&self) -> usize {
        DIMENSIONS
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let model = self.ensure_model().await?;
        let input = text.to_string();
        let vectors = tokio::task::spawn_blocking(move || {
            // Lock inside the blocking task — fastembed::embed needs &mut self.
            let mut guard = model.blocking_lock();
            guard.embed(vec![input], None)
        })
        .await
        .map_err(|e| AthenError::LlmProvider {
            provider: PROVIDER_ID.to_string(),
            message: format!("embed task panicked: {}", e),
        })?
        .map_err(|e| AthenError::LlmProvider {
            provider: PROVIDER_ID.to_string(),
            message: format!("fastembed embed failed: {}", e),
        })?;

        vectors
            .into_iter()
            .next()
            .ok_or_else(|| AthenError::LlmProvider {
                provider: PROVIDER_ID.to_string(),
                message: "fastembed returned empty embedding batch".to_string(),
            })
        // fastembed already L2-normalises its outputs in
        // `text_embedding::output::transformer_with_precedence` via
        // `common::normalize`, so no extra normalisation needed here.
    }

    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let model = self.ensure_model().await?;
        let inputs: Vec<String> = texts.to_vec();
        let vectors = tokio::task::spawn_blocking(move || {
            let mut guard = model.blocking_lock();
            guard.embed(inputs, None)
        })
        .await
        .map_err(|e| AthenError::LlmProvider {
            provider: PROVIDER_ID.to_string(),
            message: format!("embed_batch task panicked: {}", e),
        })?
        .map_err(|e| AthenError::LlmProvider {
            provider: PROVIDER_ID.to_string(),
            message: format!("fastembed embed_batch failed: {}", e),
        })?;
        Ok(vectors)
    }

    async fn is_available(&self) -> bool {
        // Availability for a bundled model is a build-time / opt-in
        // decision, handled at construction by the caller. Once we're
        // built and asked, we're always considered available — if the
        // first `embed()` then fails (e.g. offline + no cached weights)
        // that error propagates from `ensure_model` and the router
        // moves on to the next provider.
        info!(provider = PROVIDER_ID, "bundled embedder marked available");
        true
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_id_and_dimensions() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let p = BundledEmbedding::new(tmp.path().to_path_buf());
        assert_eq!(p.provider_id(), "bundled-e5-small");
        assert_eq!(p.dimensions(), 384);
        assert_eq!(p.model_id(), "multilingual-e5-small");
    }

    #[tokio::test]
    async fn is_available_without_download() {
        // We deliberately do NOT call `embed()` here: that would
        // trigger a ~270 MB model download, which is a non-starter
        // for CI. `is_available()` must answer true without any I/O.
        let tmp = tempfile::tempdir().expect("tempdir");
        let p = BundledEmbedding::new(tmp.path().to_path_buf());
        assert!(p.is_available().await);
    }
}
