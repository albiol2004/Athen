use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::Result;

pub type EntityId = Uuid;

/// A memory row scored against a query in a single index pass, carrying the
/// ranking signals the fusion layer needs. Returned by [`VectorIndex::scan_scored`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RankedRow {
    pub id: String,
    /// Cosine similarity of this row's embedding against the query embedding.
    pub cosine: f32,
    /// When the memory was first stored (None for legacy rows pre-migration).
    pub created_at: Option<DateTime<Utc>>,
    /// When the memory was last surfaced by a genuine recall (None if never).
    pub last_recalled_at: Option<DateTime<Utc>>,
    /// How many times the memory has been surfaced by a genuine recall.
    pub recall_count: u32,
    pub metadata: serde_json::Value,
}

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

    /// Score *every* stored memory against the query in one pass, returning the
    /// cosine similarity plus the per-memory ranking signals (timestamps +
    /// recall count). This is the semantic arm of hybrid recall: the brute-force
    /// scan already computes cosine for all rows, so emitting them all is free.
    ///
    /// Default implementation degrades to `search` over the whole store with no
    /// signal columns (so non-SQLite backends still function, just without
    /// recency/frequency fusion).
    async fn scan_scored(&self, query_embedding: Vec<f32>) -> Result<Vec<RankedRow>> {
        let results = self.search(query_embedding, usize::MAX).await?;
        Ok(results
            .into_iter()
            .map(|r| RankedRow {
                id: r.id,
                cosine: r.score,
                created_at: None,
                last_recalled_at: None,
                recall_count: 0,
                metadata: r.metadata,
            })
            .collect())
    }

    /// Bump the consult signals (`recall_count`, `last_recalled_at`) for the
    /// given memory ids. Called out-of-band from genuine recall sites, never
    /// from write-time dedup. Default is a no-op (backends without signal
    /// columns simply don't track usage).
    async fn bump_recall_stats(&self, _ids: &[&str]) -> Result<()> {
        Ok(())
    }
}

/// Lexical (BM25 / full-text) search over memory content. The keyword arm of
/// hybrid recall, complementary to the semantic [`VectorIndex`]. Backed by
/// SQLite FTS5 in production.
#[async_trait]
pub trait LexicalIndex: Send + Sync {
    /// Index (or re-index) a memory's text under its id.
    async fn upsert(&self, id: &str, text: &str) -> Result<()>;
    /// Return `(memory_id, score)` best-first, where `score` is normalized to
    /// `[0,1]` (higher = better match).
    async fn search(&self, query: &str, top_k: usize) -> Result<Vec<(String, f32)>>;
    /// Remove a memory from the lexical index.
    async fn delete(&self, id: &str) -> Result<()>;
}

/// Weights + thresholds for the hybrid-recall fusion ranker. Each retrieval
/// arm contributes a signal in `[0,1]`; the final score is their weighted sum.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct FusionWeights {
    /// Semantic (cosine) weight.
    pub w_sem: f32,
    /// Lexical (BM25) weight.
    pub w_lex: f32,
    /// Graph relation-strength weight.
    pub w_graph: f32,
    /// Recency-of-consult weight.
    pub w_recency: f32,
    /// Consult-frequency weight.
    pub w_freq: f32,
    /// A candidate is admitted to ranking if its cosine clears this floor, OR
    /// it has any lexical match, OR it is graph-linked. This is the real noise
    /// gate; `min_final` only trims near-zero fused scores afterwards.
    pub cosine_floor: f32,
    /// Anti-noise floor on the fused score. Kept low so a strong single-arm
    /// hit (e.g. graph-only ≈ `w_graph`, lexical-only ≈ `w_lex`) still passes —
    /// admission is decided by `cosine_floor`/lexical/graph, ordering by fusion.
    pub min_final: f32,
}

impl Default for FusionWeights {
    fn default() -> Self {
        Self {
            w_sem: 0.45,
            w_lex: 0.25,
            w_graph: 0.15,
            w_recency: 0.10,
            w_freq: 0.05,
            cosine_floor: 0.35,
            // Deliberately low: the admission gate (cosine_floor OR lexical OR
            // graph) is the real relevance filter; min_final only drops
            // degenerate near-zero fused scores, so a strong graph/lexical-only
            // hit (which can have low cosine) still survives.
            min_final: 0.08,
        }
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

    /// Look up an entity ID by its name (case-insensitive).
    ///
    /// Used by hybrid recall to pivot from a name (extracted from the
    /// query or from a vector hit's metadata) into the graph for a
    /// neighbor hop. Returns `None` if no entity matches.
    ///
    /// Default implementation scans `list_entities` and does a
    /// case-insensitive name compare. Storage-backed impls (SQLite)
    /// should override with an indexed lookup.
    async fn find_entity_by_name(&self, name: &str) -> Result<Option<EntityId>> {
        let entities = self.list_entities().await?;
        for e in entities {
            if e.name.eq_ignore_ascii_case(name) {
                if let Some(id) = e.id {
                    return Ok(Some(id));
                }
            }
        }
        Ok(None)
    }

    /// Link a memory to the entities it mentions. This is the memory↔entity
    /// edge that lets the graph arm of hybrid recall walk from an entity (found
    /// in the query or a semantic hit) to the memories that mention it — an
    /// indexed join, replacing the old full-scan string match over metadata.
    /// Default is a no-op for backends without a mentions store.
    async fn link_memory(&self, _memory_id: &str, _entity_ids: &[EntityId]) -> Result<()> {
        Ok(())
    }

    /// Return the ids of all memories that mention any of the given entities.
    /// Default returns empty (no mentions store).
    async fn memories_for_entities(&self, _entity_ids: &[EntityId]) -> Result<Vec<String>> {
        Ok(vec![])
    }

    /// Remove all memory↔entity links for a memory (used by `forget`).
    /// Default is a no-op.
    async fn unlink_memory(&self, _memory_id: &str) -> Result<()> {
        Ok(())
    }
}

/// Unified memory facade.
#[async_trait]
pub trait MemoryStore: Send + Sync {
    async fn remember(&self, item: MemoryItem) -> Result<()>;
    async fn recall(&self, query: &str, limit: usize) -> Result<Vec<MemoryItem>>;
    async fn forget(&self, id: &str) -> Result<()>;

    /// Record that the given memories were surfaced by a *genuine* recall
    /// (the `memory_recall` tool or auto-recall injection) — bumps each
    /// memory's consult signals (`recall_count`/`last_recalled_at`) and
    /// reinforces its linked entities. MUST NOT be called from write-time
    /// dedup recalls, or the frequency signal would be inflated by stores.
    /// Default is a no-op.
    async fn note_recalled(&self, _ids: &[&str]) -> Result<()> {
        Ok(())
    }
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
