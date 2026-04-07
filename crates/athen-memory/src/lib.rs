//! Memory system for Athen.
//!
//! Semantic search (vector embeddings) + Knowledge graph exploration.
//! Provides both in-memory and SQLite-backed implementations.

pub mod graph;
pub mod sqlite;
pub mod vector;

use async_trait::async_trait;

use athen_core::error::Result;
use athen_core::traits::embedding::EmbeddingProvider;
use athen_core::traits::memory::{
    Entity, EntityType, KnowledgeGraph, MemoryItem, MemoryStore, SearchResult, VectorIndex,
};

/// Unified memory facade combining vector search and knowledge graph.
pub struct Memory {
    vector: Box<dyn VectorIndex>,
    graph: Box<dyn KnowledgeGraph>,
    embedder: Option<Box<dyn EmbeddingProvider>>,
}

impl Memory {
    pub fn new(vector: Box<dyn VectorIndex>, graph: Box<dyn KnowledgeGraph>) -> Self {
        Self {
            vector,
            graph,
            embedder: None,
        }
    }

    /// Attach an embedding provider for real semantic search.
    pub fn with_embedder(mut self, embedder: Box<dyn EmbeddingProvider>) -> Self {
        self.embedder = Some(embedder);
        self
    }
}

#[async_trait]
impl MemoryStore for Memory {
    async fn remember(&self, item: MemoryItem) -> Result<()> {
        // Generate embedding from content if an embedder is available.
        let embedding = if let Some(ref embedder) = self.embedder {
            embedder.embed(&item.content).await?
        } else {
            vec![0.0f32; 0]
        };

        // Store content inside metadata so we can reconstruct it on recall.
        let mut metadata = item.metadata.clone();
        if let serde_json::Value::Object(ref mut map) = metadata {
            map.insert(
                "_content".to_string(),
                serde_json::Value::String(item.content.clone()),
            );
        }

        self.vector
            .upsert(&item.id, embedding, metadata)
            .await?;

        // If metadata contains entity information, add to graph.
        if let Some(entities) = item.metadata.get("entities").and_then(|v| v.as_array()) {
            for entity_val in entities {
                let name = entity_val
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let entity_type = match entity_val
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Concept")
                {
                    "Person" => EntityType::Person,
                    "Organization" => EntityType::Organization,
                    "Project" => EntityType::Project,
                    "Event" => EntityType::Event,
                    "Document" => EntityType::Document,
                    _ => EntityType::Concept,
                };

                let entity = Entity {
                    id: None,
                    entity_type,
                    name: name.to_string(),
                    metadata: entity_val.clone(),
                };
                let _ = self.graph.add_entity(entity).await?;
            }
        }

        Ok(())
    }

    async fn recall(&self, query: &str, limit: usize) -> Result<Vec<MemoryItem>> {
        // Embed the query if an embedder is available.
        let query_embedding = if let Some(ref embedder) = self.embedder {
            embedder.embed(query).await?
        } else {
            vec![0.0f32; 0]
        };

        let results: Vec<SearchResult> =
            self.vector.search(query_embedding, limit).await?;

        let items = results
            .into_iter()
            .map(|r| {
                // Extract original content from metadata if stored.
                let content = r
                    .metadata
                    .get("_content")
                    .and_then(|v| v.as_str())
                    .unwrap_or(query)
                    .to_string();
                MemoryItem {
                    id: r.id,
                    content,
                    metadata: r.metadata,
                }
            })
            .collect();

        Ok(items)
    }

    async fn forget(&self, id: &str) -> Result<()> {
        self.vector.delete(id).await?;
        // Note: We don't remove from graph since entities may be referenced
        // by other memory items. Graph cleanup would require reference counting.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::InMemoryGraph;
    use crate::vector::InMemoryVectorIndex;
    use athen_llm::embeddings::keyword::KeywordEmbedding;

    fn make_memory() -> Memory {
        Memory::new(
            Box::new(InMemoryVectorIndex::new()),
            Box::new(InMemoryGraph::new()),
        )
        .with_embedder(Box::new(KeywordEmbedding::new()))
    }

    #[tokio::test]
    async fn test_remember_and_recall() {
        let mem = make_memory();

        let item = MemoryItem {
            id: "mem-1".to_string(),
            content: "Meeting with Alice about the project".to_string(),
            metadata: serde_json::json!({
                "source": "calendar",
                "entities": [
                    {"name": "Alice", "type": "Person"},
                    {"name": "Project X", "type": "Project"}
                ]
            }),
        };

        mem.remember(item).await.unwrap();

        let results = mem.recall("meeting", 10).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "mem-1");
    }

    #[tokio::test]
    async fn test_remember_without_entities() {
        let mem = make_memory();

        let item = MemoryItem {
            id: "simple".to_string(),
            content: "A simple note".to_string(),
            metadata: serde_json::json!({"tag": "note"}),
        };

        mem.remember(item).await.unwrap();

        let results = mem.recall("note", 5).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "simple");
    }

    #[tokio::test]
    async fn test_forget() {
        let mem = make_memory();

        let item = MemoryItem {
            id: "forget-me".to_string(),
            content: "Temporary info".to_string(),
            metadata: serde_json::json!({}),
        };

        mem.remember(item).await.unwrap();
        mem.forget("forget-me").await.unwrap();

        let results = mem.recall("anything", 10).await.unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_remember_multiple_and_recall() {
        let mem = make_memory();

        let items = [
            ("item-0", "Rust programming language tutorial"),
            ("item-1", "Python data science overview"),
            ("item-2", "JavaScript web development guide"),
            ("item-3", "Database design patterns"),
            ("item-4", "Cloud infrastructure management"),
        ];

        for (id, content) in &items {
            let item = MemoryItem {
                id: id.to_string(),
                content: content.to_string(),
                metadata: serde_json::json!({}),
            };
            mem.remember(item).await.unwrap();
        }

        // Recall with limit 3 — should return 3 results
        let results = mem.recall("Rust programming", 3).await.unwrap();
        assert_eq!(results.len(), 3);

        // The Rust item should rank highest
        assert_eq!(results[0].id, "item-0");
    }

    #[tokio::test]
    async fn test_forget_nonexistent_is_ok() {
        let mem = make_memory();
        // Forgetting something that doesn't exist should not error
        mem.forget("does-not-exist").await.unwrap();
    }

    #[tokio::test]
    async fn test_semantic_similarity_ranking() {
        let mem = make_memory();

        mem.remember(MemoryItem {
            id: "rust".to_string(),
            content: "Rust programming tutorial".to_string(),
            metadata: serde_json::json!({}),
        })
        .await
        .unwrap();

        mem.remember(MemoryItem {
            id: "python".to_string(),
            content: "Python machine learning".to_string(),
            metadata: serde_json::json!({}),
        })
        .await
        .unwrap();

        mem.remember(MemoryItem {
            id: "javascript".to_string(),
            content: "JavaScript web development".to_string(),
            metadata: serde_json::json!({}),
        })
        .await
        .unwrap();

        // Query for Rust — the Rust item should rank first
        let results = mem.recall("programming in Rust", 3).await.unwrap();
        assert_eq!(results.len(), 3);
        assert_eq!(
            results[0].id, "rust",
            "Rust item should rank first for 'programming in Rust' query"
        );

        // Verify content was reconstructed from metadata
        assert!(
            results[0].content.contains("Rust"),
            "Content should be the original stored content"
        );
    }
}
