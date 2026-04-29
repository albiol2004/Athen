use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::Result;

pub type EntityId = Uuid;

/// Semantic vector search over stored knowledge.
#[async_trait]
pub trait VectorIndex: Send + Sync {
    async fn upsert(
        &self,
        id: &str,
        embedding: Vec<f32>,
        metadata: serde_json::Value,
    ) -> Result<()>;
    async fn search(&self, query_embedding: Vec<f32>, top_k: usize) -> Result<Vec<SearchResult>>;
    async fn delete(&self, id: &str) -> Result<()>;

    /// List all stored entries.
    /// Default implementation returns empty (not all backends support this).
    async fn list_all(&self) -> Result<Vec<SearchResult>> {
        Ok(vec![])
    }
}

/// Structured knowledge graph for entity relationships.
#[async_trait]
pub trait KnowledgeGraph: Send + Sync {
    async fn add_entity(&self, entity: Entity) -> Result<EntityId>;
    async fn add_relation(&self, from: EntityId, relation: &str, to: EntityId) -> Result<()>;
    async fn explore(&self, entry: EntityId, params: ExploreParams) -> Result<Vec<GraphNode>>;

    /// List all entities in the graph.
    /// Default implementation returns empty (not all backends support this).
    async fn list_entities(&self) -> Result<Vec<Entity>> {
        Ok(vec![])
    }

    /// List all relations as (from_id, from_name, relation, to_id, to_name).
    /// Default implementation returns empty.
    async fn list_relations(&self) -> Result<Vec<(EntityId, String, String, EntityId, String)>> {
        Ok(vec![])
    }

    /// Update an entity's name and/or type.
    async fn update_entity(
        &self,
        _id: EntityId,
        _name: Option<String>,
        _entity_type: Option<EntityType>,
    ) -> Result<()> {
        Ok(())
    }

    /// Delete an entity and all its relations.
    async fn delete_entity(&self, _id: EntityId) -> Result<()> {
        Ok(())
    }

    /// Delete a specific relation between two entities.
    async fn delete_relation(&self, _from: EntityId, _to: EntityId, _relation: &str) -> Result<()> {
        Ok(())
    }

    /// Add a relation with an explicit importance weight.
    /// Default delegates to `add_relation` (ignoring importance).
    async fn add_relation_weighted(
        &self,
        from: EntityId,
        relation: &str,
        to: EntityId,
        _importance: f32,
    ) -> Result<()> {
        self.add_relation(from, relation, to).await
    }

    /// Reinforce edges connected to the given entity.
    /// Increases strength by `amount` (clamped to 1.0) and updates last_used.
    async fn reinforce_entity(&self, _entity_id: EntityId, _amount: f32) -> Result<()> {
        Ok(())
    }
}

/// Unified memory facade.
#[async_trait]
pub trait MemoryStore: Send + Sync {
    async fn remember(&self, item: MemoryItem) -> Result<()>;
    async fn recall(&self, query: &str, limit: usize) -> Result<Vec<MemoryItem>>;
    async fn forget(&self, id: &str) -> Result<()>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub id: String,
    pub score: f32,
    pub metadata: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entity {
    pub id: Option<EntityId>,
    pub entity_type: EntityType,
    pub name: String,
    pub metadata: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum EntityType {
    Person,
    Organization,
    Project,
    Event,
    Document,
    Concept,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphNode {
    pub entity: Entity,
    pub relations: Vec<GraphEdge>,
    pub depth: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphEdge {
    pub relation: String,
    pub target: EntityId,
    pub weight: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExploreParams {
    pub recency_weight: f32,
    pub frequency_weight: f32,
    pub importance_weight: f32,
    pub max_depth: u8,
    pub max_nodes: u16,
    pub relevance_threshold: f32,
}

impl Default for ExploreParams {
    fn default() -> Self {
        Self {
            recency_weight: 0.4,
            frequency_weight: 0.2,
            importance_weight: 0.3,
            max_depth: 3,
            max_nodes: 50,
            relevance_threshold: 0.5,
        }
    }
}

/// Extracts entities and relationships from text content.
#[async_trait]
pub trait EntityExtractor: Send + Sync {
    /// Extract entities and their relationships from text.
    /// Returns a list of entities and a list of (from_name, relation, to_name) tuples.
    async fn extract(&self, text: &str) -> Result<ExtractionResult>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractionResult {
    pub entities: Vec<Entity>,
    /// (from_name, relation, to_name, importance) tuples.
    /// importance: 0.0–1.0, where 0.9 = critical, 0.5 = notable, 0.2 = minor.
    pub relations: Vec<(String, String, String, f32)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryItem {
    pub id: String,
    pub content: String,
    pub metadata: serde_json::Value,
}
