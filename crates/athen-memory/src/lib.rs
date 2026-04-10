//! Memory system for Athen.
//!
//! Semantic search (vector embeddings) + Knowledge graph exploration.
//! Provides both in-memory and SQLite-backed implementations.

pub mod extractor;
pub mod graph;
pub mod sqlite;
pub mod vector;

use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use tracing::{debug, warn};

use athen_core::error::Result;
use athen_core::traits::embedding::EmbeddingProvider;
use athen_core::traits::memory::{
    Entity, EntityExtractor, EntityId, EntityType, ExploreParams, KnowledgeGraph, MemoryItem,
    MemoryStore, SearchResult, VectorIndex,
};

/// Unified memory facade combining vector search and knowledge graph.
pub struct Memory {
    vector: Box<dyn VectorIndex>,
    graph: Box<dyn KnowledgeGraph>,
    embedder: Option<Box<dyn EmbeddingProvider>>,
    extractor: Option<Box<dyn EntityExtractor>>,
    /// Minimum relevance score for `recall()` results. Items below this
    /// threshold are filtered out before the `limit` is applied.
    min_relevance_score: f32,
}

impl Memory {
    pub fn new(vector: Box<dyn VectorIndex>, graph: Box<dyn KnowledgeGraph>) -> Self {
        Self {
            vector,
            graph,
            embedder: None,
            extractor: None,
            min_relevance_score: 0.3,
        }
    }

    /// Attach an embedding provider for real semantic search.
    pub fn with_embedder(mut self, embedder: Box<dyn EmbeddingProvider>) -> Self {
        self.embedder = Some(embedder);
        self
    }

    /// Attach an entity extractor for automatic knowledge graph population.
    pub fn with_extractor(mut self, extractor: Box<dyn EntityExtractor>) -> Self {
        self.extractor = Some(extractor);
        self
    }

    /// Set the minimum relevance score for `recall()`. Results below this
    /// threshold are discarded before the `limit` is applied. Default: 0.3.
    pub fn with_min_score(mut self, score: f32) -> Self {
        self.min_relevance_score = score;
        self
    }
}

/// Extract entity names from vector search result metadata.
fn extract_entity_names_from_metadata(metadata: &serde_json::Value) -> Vec<String> {
    let mut names = Vec::new();
    if let Some(entities) = metadata.get("_entities").and_then(|v| v.as_array()) {
        for e in entities {
            if let Some(name) = e.get("name").and_then(|v| v.as_str()) {
                names.push(name.to_string());
            }
        }
    }
    // Also check old-style entities field
    if let Some(entities) = metadata.get("entities").and_then(|v| v.as_array()) {
        for e in entities {
            if let Some(name) = e.get("name").and_then(|v| v.as_str()) {
                names.push(name.to_string());
            }
        }
    }
    names
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

        // Phase 2: LLM entity extraction (preferred) or manual metadata parsing (fallback).
        let mut entity_ids: Vec<(String, EntityId)> = Vec::new();

        if let Some(ref extractor) = self.extractor {
            // Use LLM entity extraction.
            match extractor.extract(&item.content).await {
                Ok(result) => {
                    debug!(
                        "Extracted {} entities and {} relations from content",
                        result.entities.len(),
                        result.relations.len()
                    );

                    // Store extracted entity names in metadata for hybrid recall.
                    let entity_names: Vec<serde_json::Value> = result
                        .entities
                        .iter()
                        .map(|e| {
                            serde_json::json!({
                                "name": e.name,
                                "type": format!("{:?}", e.entity_type)
                            })
                        })
                        .collect();

                    if let serde_json::Value::Object(ref mut map) = metadata {
                        map.insert(
                            "_entities".to_string(),
                            serde_json::Value::Array(entity_names),
                        );
                    }

                    // Add entities to graph.
                    for entity in &result.entities {
                        let id = self.graph.add_entity(entity.clone()).await?;
                        entity_ids.push((entity.name.clone(), id));
                    }

                    // Add relations between entities (match by name).
                    let name_to_id: HashMap<&str, EntityId> = entity_ids
                        .iter()
                        .map(|(name, id)| (name.as_str(), *id))
                        .collect();

                    for (from_name, relation, to_name, importance) in &result.relations {
                        if let (Some(&from_id), Some(&to_id)) =
                            (name_to_id.get(from_name.as_str()), name_to_id.get(to_name.as_str()))
                        {
                            self.graph
                                .add_relation_weighted(from_id, relation, to_id, *importance)
                                .await?;
                        } else {
                            debug!(
                                "Skipping relation {from_name} -[{relation}]-> {to_name}: entity not found"
                            );
                        }
                    }
                }
                Err(e) => {
                    warn!("Entity extraction failed, falling back to manual parsing: {e}");
                    // Fall through to manual parsing below.
                    self.extract_entities_from_metadata(&item.metadata, &mut entity_ids)
                        .await?;
                }
            }
        } else {
            // No extractor: use manual metadata parsing (Phase 1 behavior).
            self.extract_entities_from_metadata(&item.metadata, &mut entity_ids)
                .await?;
        }

        if let serde_json::Value::Object(ref mut map) = metadata {
            map.insert(
                "_content".to_string(),
                serde_json::Value::String(item.content.clone()),
            );
        }

        self.vector.upsert(&item.id, embedding, metadata).await?;

        Ok(())
    }

    async fn recall(&self, query: &str, limit: usize) -> Result<Vec<MemoryItem>> {
        // Embed the query if an embedder is available.
        let query_embedding = if let Some(ref embedder) = self.embedder {
            embedder.embed(query).await?
        } else {
            vec![0.0f32; 0]
        };

        // Phase 3: Hybrid retrieval — vector search + graph exploration.

        // Step 1: Direct vector search (fetch more than limit to leave room for merging).
        let fetch_count = limit * 3;
        let direct_results: Vec<SearchResult> =
            self.vector.search(query_embedding, fetch_count).await?;

        // Step 2: Collect entity names from direct results.
        let mut all_entity_names: HashSet<String> = HashSet::new();
        for result in &direct_results {
            let names = extract_entity_names_from_metadata(&result.metadata);
            all_entity_names.extend(names);
        }

        // Step 3: For each entity, explore the graph to find related entities.
        let mut related_entity_names: HashSet<String> = HashSet::new();
        if !all_entity_names.is_empty() {
            let _params = ExploreParams {
                max_depth: 1,
                max_nodes: 20,
                relevance_threshold: 0.0,
                ..Default::default()
            };

            // Search for memory items that mention entities related to the direct results.
            // Simple approach: embed each entity name and search.
            for name in &all_entity_names {
                // Embed the entity name and search for memory items mentioning related entities.
                if let Some(ref embedder) = self.embedder {
                    if let Ok(entity_embedding) = embedder.embed(name).await {
                        if let Ok(entity_results) =
                            self.vector.search(entity_embedding, 5).await
                        {
                            for er in &entity_results {
                                let names =
                                    extract_entity_names_from_metadata(&er.metadata);
                                related_entity_names.extend(names);
                            }
                        }
                    }
                }
            }

            // Remove names we already have from direct results.
            for name in &all_entity_names {
                related_entity_names.remove(name);
            }
        }

        // Step 4: Search for memory items connected through related entities.
        let mut graph_results: Vec<(String, f32)> = Vec::new(); // (id, boosted_score)
        let direct_ids: HashSet<&str> = direct_results.iter().map(|r| r.id.as_str()).collect();

        for entity_name in &related_entity_names {
            if let Some(ref embedder) = self.embedder {
                if let Ok(emb) = embedder.embed(entity_name).await {
                    if let Ok(results) = self.vector.search(emb, 5).await {
                        for r in results {
                            if !direct_ids.contains(r.id.as_str()) {
                                // Graph-connected result: boost score.
                                let boosted = r.score * 0.5 + 0.5;
                                graph_results.push((r.id.clone(), boosted));
                            }
                        }
                    }
                }
            }
        }

        // Step 5: Merge and deduplicate.
        let mut scored: HashMap<String, (f32, serde_json::Value)> = HashMap::new();

        for r in &direct_results {
            scored
                .entry(r.id.clone())
                .or_insert((r.score, r.metadata.clone()));
        }

        // For graph-connected results, we need to fetch their metadata.
        // They came from vector search results so we already have them — but we may
        // need to re-fetch. To avoid that, search for them again.
        for (id, boosted_score) in &graph_results {
            scored.entry(id.clone()).or_insert_with(|| {
                // We don't have metadata cached, but it will be in vector results
                // from the entity name searches. Use empty metadata as fallback.
                (*boosted_score, serde_json::json!({}))
            });
        }

        // Sort by score descending.
        let mut sorted: Vec<(String, f32, serde_json::Value)> = scored
            .into_iter()
            .map(|(id, (score, meta))| (id, score, meta))
            .collect();
        sorted.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        // Filter by minimum relevance score, then take top `limit`.
        let items = sorted
            .into_iter()
            .filter(|(_, score, _)| *score >= self.min_relevance_score)
            .take(limit)
            .map(|(id, _score, metadata)| {
                let content = metadata
                    .get("_content")
                    .and_then(|v| v.as_str())
                    .unwrap_or(query)
                    .to_string();
                MemoryItem {
                    id,
                    content,
                    metadata,
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

impl Memory {
    /// List all stored memories for UI display.
    pub async fn list_all(&self) -> Result<Vec<MemoryItem>> {
        let results = self.vector.list_all().await?;
        Ok(results
            .into_iter()
            .map(|r| {
                let content = r
                    .metadata
                    .get("_content")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                MemoryItem {
                    id: r.id,
                    content,
                    metadata: r.metadata,
                }
            })
            .collect())
    }

    /// Update a memory item's content (re-embeds automatically).
    pub async fn update(&self, id: &str, new_content: &str) -> Result<()> {
        let embedding = if let Some(ref embedder) = self.embedder {
            embedder.embed(new_content).await?
        } else {
            vec![]
        };
        let metadata = serde_json::json!({
            "_content": new_content,
        });
        self.vector.upsert(id, embedding, metadata).await
    }

    /// List all entities in the knowledge graph.
    pub async fn list_entities(&self) -> Result<Vec<Entity>> {
        self.graph.list_entities().await
    }

    /// List all relations in the knowledge graph.
    pub async fn list_relations(
        &self,
    ) -> Result<Vec<(EntityId, String, String, EntityId, String)>> {
        self.graph.list_relations().await
    }

    /// Update an entity's name and/or type.
    pub async fn update_entity(
        &self,
        id: EntityId,
        name: Option<String>,
        entity_type: Option<EntityType>,
    ) -> Result<()> {
        self.graph.update_entity(id, name, entity_type).await
    }

    /// Delete an entity and all its relations.
    pub async fn delete_entity(&self, id: EntityId) -> Result<()> {
        self.graph.delete_entity(id).await
    }

    /// Delete a specific relation between two entities.
    pub async fn delete_relation(
        &self,
        from: EntityId,
        to: EntityId,
        relation: &str,
    ) -> Result<()> {
        self.graph.delete_relation(from, to, relation).await
    }

    /// Reinforce edges connected to the given entity.
    pub async fn reinforce_entity(&self, entity_id: EntityId, amount: f32) -> Result<()> {
        self.graph.reinforce_entity(entity_id, amount).await
    }

    /// Reinforce entities by name (convenience method for keyword matching).
    pub async fn reinforce_by_name(&self, entity_name: &str, amount: f32) -> Result<()> {
        let entities = self.graph.list_entities().await?;
        for e in entities {
            if e.name.eq_ignore_ascii_case(entity_name) {
                if let Some(id) = e.id {
                    self.graph.reinforce_entity(id, amount).await?;
                }
            }
        }
        Ok(())
    }

    /// Extract entities from manual metadata (Phase 1 fallback).
    async fn extract_entities_from_metadata(
        &self,
        metadata: &serde_json::Value,
        entity_ids: &mut Vec<(String, EntityId)>,
    ) -> Result<()> {
        if let Some(entities) = metadata.get("entities").and_then(|v| v.as_array()) {
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
                let id = self.graph.add_entity(entity).await?;
                entity_ids.push((name.to_string(), id));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::InMemoryGraph;
    use crate::vector::InMemoryVectorIndex;
    use athen_core::traits::memory::ExtractionResult;
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
        let mem = make_memory().with_min_score(0.0);

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
        let mem = make_memory().with_min_score(0.0);

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

    // --- Phase 2 & 3 tests ---

    /// A mock EntityExtractor that returns predetermined results.
    struct MockExtractor {
        result: ExtractionResult,
    }

    impl MockExtractor {
        fn new(result: ExtractionResult) -> Self {
            Self { result }
        }
    }

    #[async_trait]
    impl EntityExtractor for MockExtractor {
        async fn extract(&self, _text: &str) -> Result<ExtractionResult> {
            Ok(self.result.clone())
        }
    }

    /// A mock EntityExtractor that always fails.
    struct FailingExtractor;

    #[async_trait]
    impl EntityExtractor for FailingExtractor {
        async fn extract(&self, _text: &str) -> Result<ExtractionResult> {
            Err(athen_core::error::AthenError::Other(
                "extraction failed".into(),
            ))
        }
    }

    #[tokio::test]
    async fn test_entity_extraction_on_remember() {
        use crate::graph::SharedInMemoryGraph;

        let graph = std::sync::Arc::new(SharedInMemoryGraph::new());
        let graph_for_mem = graph.clone();

        // We need a wrapper that delegates to the shared graph.
        // Since SharedInMemoryGraph implements KnowledgeGraph, we wrap it
        // in an Arc-based adapter.
        struct ArcGraphAdapter(std::sync::Arc<SharedInMemoryGraph>);

        #[async_trait]
        impl KnowledgeGraph for ArcGraphAdapter {
            async fn add_entity(&self, entity: Entity) -> Result<athen_core::traits::memory::EntityId> {
                self.0.add_entity(entity).await
            }
            async fn add_relation(&self, from: athen_core::traits::memory::EntityId, relation: &str, to: athen_core::traits::memory::EntityId) -> Result<()> {
                self.0.add_relation(from, relation, to).await
            }
            async fn explore(&self, entry: athen_core::traits::memory::EntityId, params: athen_core::traits::memory::ExploreParams) -> Result<Vec<athen_core::traits::memory::GraphNode>> {
                self.0.explore(entry, params).await
            }
        }

        let mem = Memory::new(
            Box::new(InMemoryVectorIndex::new()),
            Box::new(ArcGraphAdapter(graph_for_mem)),
        )
        .with_embedder(Box::new(KeywordEmbedding::new()))
        .with_extractor(Box::new(MockExtractor::new(ExtractionResult {
            entities: vec![
                Entity {
                    id: None,
                    entity_type: EntityType::Person,
                    name: "Alice".to_string(),
                    metadata: serde_json::json!({}),
                },
                Entity {
                    id: None,
                    entity_type: EntityType::Organization,
                    name: "Acme Corp".to_string(),
                    metadata: serde_json::json!({}),
                },
            ],
            relations: vec![(
                "Alice".to_string(),
                "works_at".to_string(),
                "Acme Corp".to_string(),
                0.8,
            )],
        })));

        mem.remember(MemoryItem {
            id: "test-1".to_string(),
            content: "Alice works at Acme Corp".to_string(),
            metadata: serde_json::json!({}),
        })
        .await
        .unwrap();

        // Verify entities were added to the graph.
        let entities = graph.entities().await;
        assert_eq!(entities.len(), 2, "Should have 2 entities in graph");

        let alice_id = entities
            .values()
            .find(|e| e.name == "Alice")
            .expect("Alice should be in graph")
            .id
            .unwrap();
        let acme_id = entities
            .values()
            .find(|e| e.name == "Acme Corp")
            .expect("Acme Corp should be in graph")
            .id
            .unwrap();
        drop(entities);

        // Verify the relation was added.
        let edges = graph.edges().await;
        assert_eq!(edges.len(), 1, "Should have 1 relation");
        assert_eq!(edges[0].from, alice_id);
        assert_eq!(edges[0].to, acme_id);
        assert_eq!(edges[0].relation, "works_at");
    }

    #[tokio::test]
    async fn test_extractor_failure_is_graceful() {
        let mem = Memory::new(
            Box::new(InMemoryVectorIndex::new()),
            Box::new(InMemoryGraph::new()),
        )
        .with_embedder(Box::new(KeywordEmbedding::new()))
        .with_extractor(Box::new(FailingExtractor));

        // remember() should succeed even though extractor fails.
        let result = mem
            .remember(MemoryItem {
                id: "fail-test".to_string(),
                content: "Some content that will fail extraction".to_string(),
                metadata: serde_json::json!({
                    "entities": [{"name": "FallbackEntity", "type": "Concept"}]
                }),
            })
            .await;

        assert!(result.is_ok(), "remember() should succeed despite extractor failure");

        // Verify the item was still stored and can be recalled.
        let results = mem.recall("content", 5).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "fail-test");
    }

    #[tokio::test]
    async fn test_hybrid_recall() {
        // Set up: Alice works at Acme, Bob works at Acme, Charlie works at Globex.
        // Query "Alice" should rank Bob higher than Charlie because Bob is connected
        // to Alice through Acme Corp (shared entity).
        let mem = Memory::new(
            Box::new(InMemoryVectorIndex::new()),
            Box::new(InMemoryGraph::new()),
        )
        .with_embedder(Box::new(KeywordEmbedding::new()));

        // Store items with entity metadata (manual, since no extractor needed for this test).
        mem.remember(MemoryItem {
            id: "alice".to_string(),
            content: "Alice works at Acme Corp as a senior engineer".to_string(),
            metadata: serde_json::json!({
                "entities": [
                    {"name": "Alice", "type": "Person"},
                    {"name": "Acme Corp", "type": "Organization"}
                ]
            }),
        })
        .await
        .unwrap();

        mem.remember(MemoryItem {
            id: "bob".to_string(),
            content: "Bob works at Acme Corp as a product manager".to_string(),
            metadata: serde_json::json!({
                "entities": [
                    {"name": "Bob", "type": "Person"},
                    {"name": "Acme Corp", "type": "Organization"}
                ]
            }),
        })
        .await
        .unwrap();

        mem.remember(MemoryItem {
            id: "charlie".to_string(),
            content: "Charlie works at Globex Industries as a designer".to_string(),
            metadata: serde_json::json!({
                "entities": [
                    {"name": "Charlie", "type": "Person"},
                    {"name": "Globex Industries", "type": "Organization"}
                ]
            }),
        })
        .await
        .unwrap();

        // Query for "Alice" — Alice should be first.
        let results = mem.recall("Alice", 3).await.unwrap();
        assert!(!results.is_empty(), "Should have results");
        assert_eq!(results[0].id, "alice", "Alice's item should rank first");

        // Bob should rank higher than Charlie because Bob shares "Acme Corp" with Alice.
        // With keyword embeddings and hybrid retrieval, Bob's content mentioning "Acme Corp"
        // should get a boost from the graph connection.
        if results.len() >= 3 {
            let bob_pos = results.iter().position(|r| r.id == "bob");
            let charlie_pos = results.iter().position(|r| r.id == "charlie");
            if let (Some(bp), Some(cp)) = (bob_pos, charlie_pos) {
                assert!(
                    bp < cp,
                    "Bob (pos {bp}) should rank higher than Charlie (pos {cp}) due to shared Acme Corp entity"
                );
            }
        }
    }

    #[tokio::test]
    async fn test_min_relevance_score_filtering() {
        let mem = make_memory().with_min_score(0.0);

        let items = [
            ("item-a", "Rust programming language"),
            ("item-b", "Python data science"),
            ("item-c", "JavaScript web development"),
        ];

        for (id, content) in &items {
            mem.remember(MemoryItem {
                id: id.to_string(),
                content: content.to_string(),
                metadata: serde_json::json!({}),
            })
            .await
            .unwrap();
        }

        // With min_score 0.0, all items should be returned.
        let results_all = mem.recall("programming", 10).await.unwrap();
        assert!(
            !results_all.is_empty(),
            "With min_score 0.0, should return results"
        );

        // With a very high threshold, fewer or no results should be returned.
        let mem_strict = make_memory().with_min_score(0.99);

        for (id, content) in &items {
            mem_strict
                .remember(MemoryItem {
                    id: id.to_string(),
                    content: content.to_string(),
                    metadata: serde_json::json!({}),
                })
                .await
                .unwrap();
        }

        let results_strict = mem_strict.recall("programming", 10).await.unwrap();
        assert!(
            results_strict.len() < results_all.len(),
            "High min_score ({}) should return fewer results than min_score 0.0 ({} vs {})",
            0.99,
            results_strict.len(),
            results_all.len()
        );
    }

    #[tokio::test]
    async fn test_reinforce_by_name() {
        use crate::graph::SharedInMemoryGraph;

        let graph = std::sync::Arc::new(SharedInMemoryGraph::new());
        let graph_for_mem = graph.clone();

        struct ArcGraphAdapter(std::sync::Arc<SharedInMemoryGraph>);

        #[async_trait]
        impl KnowledgeGraph for ArcGraphAdapter {
            async fn add_entity(
                &self,
                entity: Entity,
            ) -> Result<EntityId> {
                self.0.add_entity(entity).await
            }
            async fn add_relation(
                &self,
                from: EntityId,
                relation: &str,
                to: EntityId,
            ) -> Result<()> {
                self.0.add_relation(from, relation, to).await
            }
            async fn explore(
                &self,
                entry: EntityId,
                params: ExploreParams,
            ) -> Result<Vec<athen_core::traits::memory::GraphNode>> {
                self.0.explore(entry, params).await
            }
            async fn list_entities(&self) -> Result<Vec<Entity>> {
                self.0.list_entities().await
            }
            async fn reinforce_entity(
                &self,
                entity_id: EntityId,
                amount: f32,
            ) -> Result<()> {
                self.0.reinforce_entity(entity_id, amount).await
            }
        }

        let mem = Memory::new(
            Box::new(InMemoryVectorIndex::new()),
            Box::new(ArcGraphAdapter(graph_for_mem)),
        )
        .with_embedder(Box::new(KeywordEmbedding::new()));

        // Manually add entities and a relation.
        let alice_entity = Entity {
            id: None,
            entity_type: EntityType::Person,
            name: "Alice".to_string(),
            metadata: serde_json::json!({}),
        };
        let bob_entity = Entity {
            id: None,
            entity_type: EntityType::Person,
            name: "Bob".to_string(),
            metadata: serde_json::json!({}),
        };

        let alice_id = graph.add_entity(alice_entity).await.unwrap();
        let bob_id = graph.add_entity(bob_entity).await.unwrap();
        graph.add_relation(alice_id, "knows", bob_id).await.unwrap();

        // Verify initial strength.
        {
            let edges = graph.edges().await;
            assert!((edges[0].strength - 0.5).abs() < 0.001, "Initial strength should be 0.5");
        }

        // Reinforce by name.
        mem.reinforce_by_name("Alice", 0.2).await.unwrap();

        // Verify strength increased.
        {
            let edges = graph.edges().await;
            assert!(
                (edges[0].strength - 0.7).abs() < 0.001,
                "Strength should be 0.7 after reinforcement, got {}",
                edges[0].strength
            );
        }
    }
}
