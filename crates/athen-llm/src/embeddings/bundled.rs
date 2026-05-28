//! Bundled local embedding provider (fastembed-rs / ONNX).
//!
//! Provides a pure-Rust multilingual embedder so non-technical users
//! get semantic recall out of the box, without installing Ollama or
//! signing up for OpenAI. The ONNX weights are downloaded to disk
//! on first `embed()` call (~270 MB / 530 MB / 1.2 GB depending on
//! tier) and cached under the configured cache directory; subsequent
//! runs load from cache.
//!
//! Phase 2c made the embedder tier-aware: the caller passes a
//! `BundledTier` from `athen-core::config` and the model selection
//! happens internally. Activation is no longer env-var gated — the
//! router constructs this provider only when the user has explicitly
//! selected `EmbeddingMode::Bundled { tier }` in Settings.

#![cfg(feature = "bundled-embeddings")]

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::{Mutex, OnceCell};
use tracing::{debug, info};

use athen_core::config::BundledTier;
use athen_core::error::{AthenError, Result};
use athen_core::traits::embedding::EmbeddingProvider;

use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};

/// Bundled local embedding provider backed by fastembed-rs.
///
/// Construction is cheap (just stores the cache path + tier). The
/// underlying ONNX session is initialised lazily on the first
/// `embed()` call so users who never trigger memory recall in a
/// session don't pay the startup cost — or the per-tier download.
pub struct BundledEmbedding {
    cache_dir: PathBuf,
    tier: BundledTier,
    /// Lazily-initialised model handle. `embed()` requires `&mut self`
    /// on `TextEmbedding`, so we wrap in a `Mutex`. The `OnceCell`
    /// ensures we only pay the load/download cost once per process.
    inner: OnceCell<Arc<Mutex<TextEmbedding>>>,
}

impl BundledEmbedding {
    /// Construct a new bundled embedder for the given tier. The cache
    /// directory will be created on first use if it does not already
    /// exist.
    pub fn new(cache_dir: PathBuf, tier: BundledTier) -> Self {
        Self {
            cache_dir,
            tier,
            inner: OnceCell::new(),
        }
    }

    /// Informational model identifier (not part of the trait surface
    /// today; surfaced for future logging / UI hooks).
    pub fn model_id(&self) -> &'static str {
        match self.tier {
            BundledTier::Light => "multilingual-e5-small",
            BundledTier::Standard => "multilingual-e5-base",
            BundledTier::HighQuality => "bge-m3",
        }
    }

    /// Map the active tier to the fastembed enum variant.
    fn fastembed_model(&self) -> EmbeddingModel {
        match self.tier {
            BundledTier::Light => EmbeddingModel::MultilingualE5Small,
            BundledTier::Standard => EmbeddingModel::MultilingualE5Base,
            BundledTier::HighQuality => EmbeddingModel::BGEM3,
        }
    }

    /// Stable provider id surfaced through `EmbeddingProvider::provider_id`.
    /// One id per tier so telemetry can distinguish them.
    fn provider_id_static(tier: BundledTier) -> &'static str {
        match tier {
            BundledTier::Light => "bundled-e5-small",
            BundledTier::Standard => "bundled-e5-base",
            BundledTier::HighQuality => "bundled-bge-m3",
        }
    }

    /// Ensure the underlying fastembed `TextEmbedding` is initialised.
    /// First call may download tens to hundreds of MB; subsequent
    /// calls are cached.
    async fn ensure_model(&self) -> Result<Arc<Mutex<TextEmbedding>>> {
        let cache_dir = self.cache_dir.clone();
        let model_variant = self.fastembed_model();
        let provider_id = Self::provider_id_static(self.tier);
        let model_label = self.model_id();
        let handle = self
            .inner
            .get_or_try_init(|| async move {
                debug!(
                    cache_dir = %cache_dir.display(),
                    model = model_label,
                    "initialising bundled embedding model"
                );
                // fastembed is synchronous and may block on I/O
                // (download + ONNX session build) — keep the async
                // runtime free.
                let model = tokio::task::spawn_blocking(move || {
                    TextEmbedding::try_new(
                        InitOptions::new(model_variant)
                            .with_cache_dir(cache_dir)
                            .with_show_download_progress(true),
                    )
                })
                .await
                .map_err(|e| AthenError::LlmProvider {
                    provider: provider_id.to_string(),
                    message: format!("model init task panicked: {}", e),
                })?
                .map_err(|e| AthenError::LlmProvider {
                    provider: provider_id.to_string(),
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
        Self::provider_id_static(self.tier)
    }

    fn dimensions(&self) -> usize {
        self.tier.dimensions()
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let model = self.ensure_model().await?;
        let input = text.to_string();
        let provider_id = Self::provider_id_static(self.tier);
        let vectors = tokio::task::spawn_blocking(move || {
            // Lock inside the blocking task — fastembed::embed needs &mut self.
            let mut guard = model.blocking_lock();
            guard.embed(vec![input], None)
        })
        .await
        .map_err(|e| AthenError::LlmProvider {
            provider: provider_id.to_string(),
            message: format!("embed task panicked: {}", e),
        })?
        .map_err(|e| AthenError::LlmProvider {
            provider: provider_id.to_string(),
            message: format!("fastembed embed failed: {}", e),
        })?;

        vectors
            .into_iter()
            .next()
            .ok_or_else(|| AthenError::LlmProvider {
                provider: provider_id.to_string(),
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
        let provider_id = Self::provider_id_static(self.tier);
        let vectors = tokio::task::spawn_blocking(move || {
            let mut guard = model.blocking_lock();
            guard.embed(inputs, None)
        })
        .await
        .map_err(|e| AthenError::LlmProvider {
            provider: provider_id.to_string(),
            message: format!("embed_batch task panicked: {}", e),
        })?
        .map_err(|e| AthenError::LlmProvider {
            provider: provider_id.to_string(),
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
        info!(
            provider = Self::provider_id_static(self.tier),
            "bundled embedder marked available"
        );
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
        let p = BundledEmbedding::new(tmp.path().to_path_buf(), BundledTier::Light);
        assert_eq!(p.provider_id(), "bundled-e5-small");
        assert_eq!(p.dimensions(), 384);
        assert_eq!(p.model_id(), "multilingual-e5-small");
    }

    #[test]
    fn provider_id_per_tier() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let std_p = BundledEmbedding::new(tmp.path().to_path_buf(), BundledTier::Standard);
        assert_eq!(std_p.provider_id(), "bundled-e5-base");
        assert_eq!(std_p.dimensions(), 768);
        let hq_p = BundledEmbedding::new(tmp.path().to_path_buf(), BundledTier::HighQuality);
        assert_eq!(hq_p.provider_id(), "bundled-bge-m3");
        assert_eq!(hq_p.dimensions(), 1024);
    }

    #[tokio::test]
    async fn is_available_without_download() {
        // We deliberately do NOT call `embed()` here: that would
        // trigger a model download, which is a non-starter for CI.
        // `is_available()` must answer true without any I/O.
        let tmp = tempfile::tempdir().expect("tempdir");
        let p = BundledEmbedding::new(tmp.path().to_path_buf(), BundledTier::Light);
        assert!(p.is_available().await);
    }
}
