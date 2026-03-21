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
    async fn search(
        &self,
        query_embedding: Vec<f32>,
        top_k: usize,
    ) -> Result<Vec<SearchResult>>;
    async fn delete(&self, id: &str) -> Result<()>;
}

/// Structured knowledge graph for entity relationships.
#[async_trait]
pub trait KnowledgeGraph: Send + Sync {
    async fn add_entity(&self, entity: Entity) -> Result<EntityId>;
    async fn add_relation(
        &self,
        from: EntityId,
        relation: &str,
        to: EntityId,
    ) -> Result<()>;
    async fn explore(
        &self,
        entry: EntityId,
        params: ExploreParams,
    ) -> Result<Vec<GraphNode>>;
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryItem {
    pub id: String,
    pub content: String,
    pub metadata: serde_json::Value,
}
