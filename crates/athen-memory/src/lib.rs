//! Memory system for Athen.
//!
//! Semantic search (vector embeddings) + Knowledge graph exploration.
//! Provides both in-memory and SQLite-backed implementations.

pub mod graph;
pub mod sqlite;
pub mod vector;

use async_trait::async_trait;

use athen_core::error::Result;
use athen_core::traits::memory::{
    Entity, EntityType, KnowledgeGraph, MemoryItem, MemoryStore, SearchResult, VectorIndex,
};

/// Unified memory facade combining vector search and knowledge graph.
pub struct Memory {
    vector: Box<dyn VectorIndex>,
    graph: Box<dyn KnowledgeGraph>,
}

impl Memory {
    pub fn new(vector: Box<dyn VectorIndex>, graph: Box<dyn KnowledgeGraph>) -> Self {
        Self { vector, graph }
    }
}

#[async_trait]
impl MemoryStore for Memory {
    async fn remember(&self, item: MemoryItem) -> Result<()> {
        // Store in vector index with empty embedding placeholder.
        // In production, an embedding model would generate the vector from item.content.
        let embedding = vec![0.0f32; 0];
        self.vector
            .upsert(&item.id, embedding, item.metadata.clone())
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
        // Search vector index. Since we store empty embeddings for now,
        // we use a zero-vector query. In production, the query string
        // would be converted to an embedding first.
        let query_embedding = vec![0.0f32; 0];
        let results: Vec<SearchResult> =
            self.vector.search(query_embedding, limit).await?;

        let items = results
            .into_iter()
            .map(|r| MemoryItem {
                id: r.id,
                content: query.to_string(),
                metadata: r.metadata,
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

    fn make_memory() -> Memory {
        Memory::new(
            Box::new(InMemoryVectorIndex::new()),
            Box::new(InMemoryGraph::new()),
        )
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

        for i in 0..5 {
            let item = MemoryItem {
                id: format!("item-{}", i),
                content: format!("Content {}", i),
                metadata: serde_json::json!({"index": i}),
            };
            mem.remember(item).await.unwrap();
        }

        let results = mem.recall("query", 3).await.unwrap();
        assert_eq!(results.len(), 3);
    }

    #[tokio::test]
    async fn test_forget_nonexistent_is_ok() {
        let mem = make_memory();
        // Forgetting something that doesn't exist should not error
        mem.forget("does-not-exist").await.unwrap();
    }
}
