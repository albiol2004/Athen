//! TF-IDF-style keyword embedding fallback.
//!
//! Converts text into a fixed-dimension sparse vector by hashing words
//! into buckets and applying TF weighting with L2 normalization. No
//! external dependencies — always available as a fallback when no
//! embedding server is running.
//!
//! This won't match the quality of real neural embeddings, but it
//! provides basic semantic matching for the memory system when no
//! model is available.

use async_trait::async_trait;

use athen_core::error::Result;
use athen_core::traits::embedding::EmbeddingProvider;

const DEFAULT_DIMENSIONS: usize = 384;

/// Keyword-based embedding provider using TF hash projection.
///
/// Each word is hashed (FNV-1a) to a bucket in a fixed-size vector.
/// The weight per bucket is the term frequency (count / total words).
/// The final vector is L2-normalized for cosine similarity compatibility.
///
/// Short words (< 3 chars) are filtered out to reduce noise from
/// articles, prepositions, etc.
pub struct KeywordEmbedding {
    dimensions: usize,
}

impl KeywordEmbedding {
    /// Create a new keyword embedding provider with 384 dimensions.
    pub fn new() -> Self {
        Self {
            dimensions: DEFAULT_DIMENSIONS,
        }
    }

    /// Create with custom dimensionality.
    pub fn with_dimensions(dimensions: usize) -> Self {
        Self { dimensions }
    }

    /// Embed a single text into a vector.
    fn embed_sync(&self, text: &str) -> Vec<f32> {
        let mut vector = vec![0.0f32; self.dimensions];

        let words: Vec<&str> = text
            .split_whitespace()
            .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()))
            .filter(|w| !w.is_empty() && w.len() > 2)
            .collect();

        if words.is_empty() {
            return vector;
        }

        let tf = 1.0 / words.len() as f32;
        for word in &words {
            let bucket = fnv1a_hash(word) % self.dimensions;
            vector[bucket] += tf;
        }

        // L2 normalize.
        let norm: f32 = vector.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for v in &mut vector {
                *v /= norm;
            }
        }

        vector
    }
}

impl Default for KeywordEmbedding {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl EmbeddingProvider for KeywordEmbedding {
    fn provider_id(&self) -> &str {
        "keyword"
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        Ok(self.embed_sync(text))
    }

    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|t| self.embed_sync(t)).collect())
    }

    async fn is_available(&self) -> bool {
        true
    }
}

/// FNV-1a hash for consistent word-to-bucket mapping.
fn fnv1a_hash(s: &str) -> usize {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in s.to_lowercase().as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash as usize
}

/// Compute cosine similarity between two vectors.
#[cfg(test)]
fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a * norm_b)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_embed_produces_correct_dimensions() {
        let provider = KeywordEmbedding::new();
        let embedding = provider.embed("hello world test embedding").await.unwrap();
        assert_eq!(embedding.len(), 384);
    }

    #[tokio::test]
    async fn test_embed_custom_dimensions() {
        let provider = KeywordEmbedding::with_dimensions(128);
        let embedding = provider.embed("some text here").await.unwrap();
        assert_eq!(embedding.len(), 128);
        assert_eq!(provider.dimensions(), 128);
    }

    #[tokio::test]
    async fn test_embed_empty_text() {
        let provider = KeywordEmbedding::new();
        let embedding = provider.embed("").await.unwrap();
        assert_eq!(embedding.len(), 384);
        // All zeros.
        assert!(embedding.iter().all(|&v| v == 0.0));
    }

    #[tokio::test]
    async fn test_embed_short_words_filtered() {
        let provider = KeywordEmbedding::new();
        // All words are <= 2 chars, should produce zero vector.
        let embedding = provider.embed("a an to of in").await.unwrap();
        assert!(embedding.iter().all(|&v| v == 0.0));
    }

    #[tokio::test]
    async fn test_embed_similar_texts() {
        let provider = KeywordEmbedding::new();
        let a = provider
            .embed("the quick brown fox jumps over the lazy dog")
            .await
            .unwrap();
        let b = provider
            .embed("the fast brown fox leaps over the lazy dog")
            .await
            .unwrap();
        let sim = cosine_similarity(&a, &b);
        // Similar texts should have high similarity.
        assert!(sim > 0.5, "similarity was {}, expected > 0.5", sim);
    }

    #[tokio::test]
    async fn test_embed_different_texts() {
        let provider = KeywordEmbedding::new();
        let a = provider
            .embed("quantum physics nuclear reactor energy")
            .await
            .unwrap();
        let b = provider
            .embed("chocolate cake recipe baking dessert")
            .await
            .unwrap();
        let sim = cosine_similarity(&a, &b);
        // Dissimilar texts should have lower similarity than similar ones.
        let c = provider
            .embed("quantum mechanics nuclear fusion energy")
            .await
            .unwrap();
        let sim_same = cosine_similarity(&a, &c);
        assert!(
            sim_same > sim,
            "same-topic similarity {} should be > cross-topic {}",
            sim_same,
            sim
        );
    }

    #[tokio::test]
    async fn test_embed_is_normalized() {
        let provider = KeywordEmbedding::new();
        let embedding = provider
            .embed("testing vector normalization here")
            .await
            .unwrap();
        let norm: f32 = embedding.iter().map(|x| x * x).sum::<f32>().sqrt();
        // Should be approximately 1.0 (L2 normalized).
        assert!(
            (norm - 1.0).abs() < 0.001,
            "norm was {}, expected ~1.0",
            norm
        );
    }

    #[tokio::test]
    async fn test_embed_deterministic() {
        let provider = KeywordEmbedding::new();
        let a = provider.embed("deterministic output test").await.unwrap();
        let b = provider.embed("deterministic output test").await.unwrap();
        assert_eq!(a, b);
    }

    #[tokio::test]
    async fn test_embed_batch() {
        let provider = KeywordEmbedding::new();
        let texts = vec![
            "first text here".to_string(),
            "second text there".to_string(),
        ];
        let results = provider.embed_batch(&texts).await.unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].len(), 384);
        assert_eq!(results[1].len(), 384);
    }

    #[tokio::test]
    async fn test_is_always_available() {
        let provider = KeywordEmbedding::new();
        assert!(provider.is_available().await);
    }

    #[test]
    fn test_provider_id() {
        let provider = KeywordEmbedding::new();
        assert_eq!(provider.provider_id(), "keyword");
    }

    #[test]
    fn test_default() {
        let provider = KeywordEmbedding::default();
        assert_eq!(provider.dimensions(), 384);
    }

    #[test]
    fn test_fnv1a_case_insensitive() {
        assert_eq!(fnv1a_hash("Hello"), fnv1a_hash("hello"));
        assert_eq!(fnv1a_hash("WORLD"), fnv1a_hash("world"));
    }
}
