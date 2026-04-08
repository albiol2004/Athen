//! Vector embeddings and semantic search.

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::RwLock;

use athen_core::error::Result;
use athen_core::traits::memory::{SearchResult, VectorIndex};

struct VectorEntry {
    id: String,
    embedding: Vec<f32>,
    metadata: Value,
}

/// In-memory brute-force vector index for semantic search.
pub struct InMemoryVectorIndex {
    entries: RwLock<Vec<VectorEntry>>,
}

impl InMemoryVectorIndex {
    pub fn new() -> Self {
        Self {
            entries: RwLock::new(Vec::new()),
        }
    }
}

impl Default for InMemoryVectorIndex {
    fn default() -> Self {
        Self::new()
    }
}

/// Compute cosine similarity between two vectors.
fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }

    let mut dot = 0.0f32;
    let mut norm_a = 0.0f32;
    let mut norm_b = 0.0f32;

    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        norm_a += x * x;
        norm_b += y * y;
    }

    let denom = norm_a.sqrt() * norm_b.sqrt();
    if denom == 0.0 {
        return 0.0;
    }

    dot / denom
}

#[async_trait]
impl VectorIndex for InMemoryVectorIndex {
    async fn upsert(&self, id: &str, embedding: Vec<f32>, metadata: Value) -> Result<()> {
        let mut entries = self.entries.write().await;
        if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
            entry.embedding = embedding;
            entry.metadata = metadata;
        } else {
            entries.push(VectorEntry {
                id: id.to_string(),
                embedding,
                metadata,
            });
        }
        Ok(())
    }

    async fn search(
        &self,
        query_embedding: Vec<f32>,
        top_k: usize,
    ) -> Result<Vec<SearchResult>> {
        let entries = self.entries.read().await;

        let mut scored: Vec<(f32, &VectorEntry)> = entries
            .iter()
            .map(|e| (cosine_similarity(&query_embedding, &e.embedding), e))
            .collect();

        // Sort descending by score
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

        let results = scored
            .into_iter()
            .take(top_k)
            .map(|(score, entry)| SearchResult {
                id: entry.id.clone(),
                score,
                metadata: entry.metadata.clone(),
            })
            .collect();

        Ok(results)
    }

    async fn delete(&self, id: &str) -> Result<()> {
        let mut entries = self.entries.write().await;
        entries.retain(|e| e.id != id);
        Ok(())
    }

    async fn list_all(&self) -> Result<Vec<SearchResult>> {
        let entries = self.entries.read().await;
        Ok(entries
            .iter()
            .map(|e| SearchResult {
                id: e.id.clone(),
                score: 1.0,
                metadata: e.metadata.clone(),
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_cosine_similarity_identical() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![1.0, 0.0, 0.0];
        let sim = cosine_similarity(&a, &b);
        assert!((sim - 1.0).abs() < 1e-6);
    }

    #[tokio::test]
    async fn test_cosine_similarity_orthogonal() {
        let a = vec![1.0, 0.0];
        let b = vec![0.0, 1.0];
        let sim = cosine_similarity(&a, &b);
        assert!(sim.abs() < 1e-6);
    }

    #[tokio::test]
    async fn test_cosine_similarity_opposite() {
        let a = vec![1.0, 0.0];
        let b = vec![-1.0, 0.0];
        let sim = cosine_similarity(&a, &b);
        assert!((sim - (-1.0)).abs() < 1e-6);
    }

    #[tokio::test]
    async fn test_upsert_and_search() {
        let index = InMemoryVectorIndex::new();

        index
            .upsert("a", vec![1.0, 0.0, 0.0], serde_json::json!({"label": "a"}))
            .await
            .unwrap();
        index
            .upsert("b", vec![0.0, 1.0, 0.0], serde_json::json!({"label": "b"}))
            .await
            .unwrap();
        index
            .upsert("c", vec![0.9, 0.1, 0.0], serde_json::json!({"label": "c"}))
            .await
            .unwrap();

        let results = index.search(vec![1.0, 0.0, 0.0], 2).await.unwrap();
        assert_eq!(results.len(), 2);
        // "a" should be first (exact match), "c" should be second (close)
        assert_eq!(results[0].id, "a");
        assert_eq!(results[1].id, "c");
        assert!((results[0].score - 1.0).abs() < 1e-6);
    }

    #[tokio::test]
    async fn test_upsert_updates_existing() {
        let index = InMemoryVectorIndex::new();

        index
            .upsert("a", vec![1.0, 0.0], serde_json::json!({"v": 1}))
            .await
            .unwrap();
        index
            .upsert("a", vec![0.0, 1.0], serde_json::json!({"v": 2}))
            .await
            .unwrap();

        let results = index.search(vec![0.0, 1.0], 10).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "a");
        assert!((results[0].score - 1.0).abs() < 1e-6);
        assert_eq!(results[0].metadata["v"], 2);
    }

    #[tokio::test]
    async fn test_delete() {
        let index = InMemoryVectorIndex::new();

        index
            .upsert("a", vec![1.0, 0.0], serde_json::json!({}))
            .await
            .unwrap();
        index
            .upsert("b", vec![0.0, 1.0], serde_json::json!({}))
            .await
            .unwrap();

        index.delete("a").await.unwrap();

        let results = index.search(vec![1.0, 0.0], 10).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "b");
    }

    #[tokio::test]
    async fn test_top_k_ordering() {
        let index = InMemoryVectorIndex::new();

        // Create entries with known similarities to query [1, 0, 0]
        index
            .upsert("exact", vec![1.0, 0.0, 0.0], serde_json::json!({}))
            .await
            .unwrap();
        index
            .upsert("close", vec![0.9, 0.4, 0.0], serde_json::json!({}))
            .await
            .unwrap();
        index
            .upsert("medium", vec![0.5, 0.5, 0.5], serde_json::json!({}))
            .await
            .unwrap();
        index
            .upsert("far", vec![0.0, 0.0, 1.0], serde_json::json!({}))
            .await
            .unwrap();

        let results = index.search(vec![1.0, 0.0, 0.0], 3).await.unwrap();
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].id, "exact");
        assert_eq!(results[1].id, "close");
        assert_eq!(results[2].id, "medium");
        // Verify scores are descending
        assert!(results[0].score >= results[1].score);
        assert!(results[1].score >= results[2].score);
    }

    #[tokio::test]
    async fn test_search_empty_index() {
        let index = InMemoryVectorIndex::new();
        let results = index.search(vec![1.0, 0.0], 5).await.unwrap();
        assert!(results.is_empty());
    }
}
