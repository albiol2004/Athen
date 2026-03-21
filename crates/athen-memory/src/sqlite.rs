//! SQLite-backed persistent versions of VectorIndex and KnowledgeGraph.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
use uuid::Uuid;

use athen_core::error::{AthenError, Result};
use athen_core::traits::memory::{
    Entity, EntityId, EntityType, ExploreParams, GraphEdge, GraphNode,
    KnowledgeGraph, SearchResult, VectorIndex,
};

// ---------------------------------------------------------------------------
// SqliteVectorIndex
// ---------------------------------------------------------------------------

/// SQLite-backed vector index. Stores embeddings as binary blobs and loads
/// them into memory for brute-force cosine similarity search.
pub struct SqliteVectorIndex {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteVectorIndex {
    pub fn new(conn: Arc<Mutex<Connection>>) -> Result<Self> {
        {
            let c = conn.lock().map_err(|e| AthenError::Other(e.to_string()))?;
            c.execute_batch(
                "CREATE TABLE IF NOT EXISTS vectors (
                    id TEXT PRIMARY KEY,
                    embedding BLOB NOT NULL,
                    metadata_json TEXT NOT NULL
                );",
            )
            .map_err(|e| AthenError::Other(e.to_string()))?;
        }
        Ok(Self { conn })
    }
}

fn embedding_to_bytes(embedding: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(embedding.len() * 4);
    for &v in embedding {
        bytes.extend_from_slice(&v.to_le_bytes());
    }
    bytes
}

fn bytes_to_embedding(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect()
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    let denom = na.sqrt() * nb.sqrt();
    if denom == 0.0 {
        0.0
    } else {
        dot / denom
    }
}

#[async_trait]
impl VectorIndex for SqliteVectorIndex {
    async fn upsert(&self, id: &str, embedding: Vec<f32>, metadata: serde_json::Value) -> Result<()> {
        let conn = self.conn.lock().map_err(|e| AthenError::Other(e.to_string()))?;
        let blob = embedding_to_bytes(&embedding);
        let json = serde_json::to_string(&metadata)?;
        conn.execute(
            "INSERT INTO vectors (id, embedding, metadata_json) VALUES (?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET embedding = excluded.embedding, metadata_json = excluded.metadata_json",
            params![id, blob, json],
        )
        .map_err(|e| AthenError::Other(e.to_string()))?;
        Ok(())
    }

    async fn search(
        &self,
        query_embedding: Vec<f32>,
        top_k: usize,
    ) -> Result<Vec<SearchResult>> {
        let conn = self.conn.lock().map_err(|e| AthenError::Other(e.to_string()))?;
        let mut stmt = conn
            .prepare("SELECT id, embedding, metadata_json FROM vectors")
            .map_err(|e| AthenError::Other(e.to_string()))?;

        let mut scored: Vec<(f32, String, serde_json::Value)> = Vec::new();

        let rows = stmt
            .query_map([], |row| {
                let id: String = row.get(0)?;
                let blob: Vec<u8> = row.get(1)?;
                let meta_str: String = row.get(2)?;
                Ok((id, blob, meta_str))
            })
            .map_err(|e| AthenError::Other(e.to_string()))?;

        for row in rows {
            let (id, blob, meta_str) = row.map_err(|e| AthenError::Other(e.to_string()))?;
            let emb = bytes_to_embedding(&blob);
            let score = cosine_similarity(&query_embedding, &emb);
            let metadata: serde_json::Value =
                serde_json::from_str(&meta_str).unwrap_or(serde_json::Value::Null);
            scored.push((score, id, metadata));
        }

        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

        Ok(scored
            .into_iter()
            .take(top_k)
            .map(|(score, id, metadata)| SearchResult { id, score, metadata })
            .collect())
    }

    async fn delete(&self, id: &str) -> Result<()> {
        let conn = self.conn.lock().map_err(|e| AthenError::Other(e.to_string()))?;
        conn.execute("DELETE FROM vectors WHERE id = ?1", params![id])
            .map_err(|e| AthenError::Other(e.to_string()))?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// SqliteGraph
// ---------------------------------------------------------------------------

/// SQLite-backed knowledge graph with persistent entity and edge storage.
pub struct SqliteGraph {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteGraph {
    pub fn new(conn: Arc<Mutex<Connection>>) -> Result<Self> {
        {
            let c = conn.lock().map_err(|e| AthenError::Other(e.to_string()))?;
            c.execute_batch(
                "CREATE TABLE IF NOT EXISTS entities (
                    id TEXT PRIMARY KEY,
                    entity_type TEXT NOT NULL,
                    name TEXT NOT NULL,
                    metadata_json TEXT NOT NULL
                );
                CREATE TABLE IF NOT EXISTS edges (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    from_entity TEXT NOT NULL REFERENCES entities(id),
                    relation TEXT NOT NULL,
                    to_entity TEXT NOT NULL REFERENCES entities(id),
                    weight REAL NOT NULL DEFAULT 1.0,
                    created_at TEXT NOT NULL
                );",
            )
            .map_err(|e| AthenError::Other(e.to_string()))?;
        }
        Ok(Self { conn })
    }
}

fn entity_type_to_str(t: &EntityType) -> &'static str {
    match t {
        EntityType::Person => "Person",
        EntityType::Organization => "Organization",
        EntityType::Project => "Project",
        EntityType::Event => "Event",
        EntityType::Document => "Document",
        EntityType::Concept => "Concept",
    }
}

fn str_to_entity_type(s: &str) -> EntityType {
    match s {
        "Person" => EntityType::Person,
        "Organization" => EntityType::Organization,
        "Project" => EntityType::Project,
        "Event" => EntityType::Event,
        "Document" => EntityType::Document,
        "Concept" => EntityType::Concept,
        _ => EntityType::Concept,
    }
}

struct SqliteEdge {
    from: EntityId,
    relation: String,
    to: EntityId,
    weight: f32,
    created_at: DateTime<Utc>,
}

fn edge_score(edge: &SqliteEdge, params: &ExploreParams) -> f32 {
    let age_secs = (Utc::now() - edge.created_at).num_seconds().max(0) as f64;
    let half_life_secs = 7.0 * 24.0 * 3600.0;
    let recency = (-age_secs * (2.0f64.ln()) / half_life_secs).exp() as f32;
    let frequency = edge.weight.min(1.0);
    let importance = edge.weight.min(1.0);

    params.recency_weight * recency
        + params.frequency_weight * frequency
        + params.importance_weight * importance
}

#[async_trait]
impl KnowledgeGraph for SqliteGraph {
    async fn add_entity(&self, mut entity: Entity) -> Result<EntityId> {
        let id = entity.id.unwrap_or_else(Uuid::new_v4);
        entity.id = Some(id);

        let conn = self.conn.lock().map_err(|e| AthenError::Other(e.to_string()))?;
        let type_str = entity_type_to_str(&entity.entity_type);
        let meta_json = serde_json::to_string(&entity.metadata)?;

        conn.execute(
            "INSERT INTO entities (id, entity_type, name, metadata_json) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(id) DO UPDATE SET entity_type = excluded.entity_type, name = excluded.name, metadata_json = excluded.metadata_json",
            params![id.to_string(), type_str, entity.name, meta_json],
        )
        .map_err(|e| AthenError::Other(e.to_string()))?;

        Ok(id)
    }

    async fn add_relation(&self, from: EntityId, relation: &str, to: EntityId) -> Result<()> {
        let conn = self.conn.lock().map_err(|e| AthenError::Other(e.to_string()))?;
        let now = Utc::now().to_rfc3339();

        conn.execute(
            "INSERT INTO edges (from_entity, relation, to_entity, weight, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![from.to_string(), relation, to.to_string(), 1.0f64, now],
        )
        .map_err(|e| AthenError::Other(e.to_string()))?;

        Ok(())
    }

    async fn explore(
        &self,
        entry: EntityId,
        params: ExploreParams,
    ) -> Result<Vec<GraphNode>> {
        let conn = self.conn.lock().map_err(|e| AthenError::Other(e.to_string()))?;

        // Load all entities into memory for exploration
        let mut entities: HashMap<EntityId, Entity> = HashMap::new();
        {
            let mut stmt = conn
                .prepare("SELECT id, entity_type, name, metadata_json FROM entities")
                .map_err(|e| AthenError::Other(e.to_string()))?;
            let rows = stmt
                .query_map([], |row| {
                    let id_str: String = row.get(0)?;
                    let type_str: String = row.get(1)?;
                    let name: String = row.get(2)?;
                    let meta_str: String = row.get(3)?;
                    Ok((id_str, type_str, name, meta_str))
                })
                .map_err(|e| AthenError::Other(e.to_string()))?;

            for row in rows {
                let (id_str, type_str, name, meta_str) =
                    row.map_err(|e| AthenError::Other(e.to_string()))?;
                let id = Uuid::parse_str(&id_str)
                    .map_err(|e| AthenError::Other(e.to_string()))?;
                let metadata: serde_json::Value =
                    serde_json::from_str(&meta_str).unwrap_or(serde_json::Value::Null);
                entities.insert(
                    id,
                    Entity {
                        id: Some(id),
                        entity_type: str_to_entity_type(&type_str),
                        name,
                        metadata,
                    },
                );
            }
        }

        // Load all edges
        let mut all_edges: Vec<SqliteEdge> = Vec::new();
        {
            let mut stmt = conn
                .prepare("SELECT from_entity, relation, to_entity, weight, created_at FROM edges")
                .map_err(|e| AthenError::Other(e.to_string()))?;
            let rows = stmt
                .query_map([], |row| {
                    let from_str: String = row.get(0)?;
                    let relation: String = row.get(1)?;
                    let to_str: String = row.get(2)?;
                    let weight: f64 = row.get(3)?;
                    let created_str: String = row.get(4)?;
                    Ok((from_str, relation, to_str, weight, created_str))
                })
                .map_err(|e| AthenError::Other(e.to_string()))?;

            for row in rows {
                let (from_str, relation, to_str, weight, created_str) =
                    row.map_err(|e| AthenError::Other(e.to_string()))?;
                let from = Uuid::parse_str(&from_str)
                    .map_err(|e| AthenError::Other(e.to_string()))?;
                let to = Uuid::parse_str(&to_str)
                    .map_err(|e| AthenError::Other(e.to_string()))?;
                let created_at = DateTime::parse_from_rfc3339(&created_str)
                    .map(|dt| dt.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now());

                all_edges.push(SqliteEdge {
                    from,
                    relation,
                    to,
                    weight: weight as f32,
                    created_at,
                });
            }
        }

        // BFS exploration (same logic as InMemoryGraph)
        let mut visited: HashSet<EntityId> = HashSet::new();
        let mut result: Vec<GraphNode> = Vec::new();
        let mut queue: VecDeque<(EntityId, u8)> = VecDeque::new();

        if !entities.contains_key(&entry) {
            return Ok(result);
        }

        queue.push_back((entry, 0));
        visited.insert(entry);

        while let Some((current_id, depth)) = queue.pop_front() {
            if result.len() >= params.max_nodes as usize {
                break;
            }

            let entity = match entities.get(&current_id) {
                Some(e) => e.clone(),
                None => continue,
            };

            let mut node_relations: Vec<GraphEdge> = Vec::new();

            for edge in all_edges.iter() {
                if edge.from == current_id {
                    let score = edge_score(edge, &params);
                    if score >= params.relevance_threshold || depth == 0 {
                        node_relations.push(GraphEdge {
                            relation: edge.relation.clone(),
                            target: edge.to,
                            weight: edge.weight,
                        });

                        if depth < params.max_depth && !visited.contains(&edge.to)
                            && edge.weight >= params.relevance_threshold {
                                visited.insert(edge.to);
                                queue.push_back((edge.to, depth + 1));
                            }
                    }
                }
            }

            result.push(GraphNode {
                entity,
                relations: node_relations,
                depth,
            });
        }

        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use athen_core::traits::memory::EntityType;

    fn in_memory_conn() -> Arc<Mutex<Connection>> {
        Arc::new(Mutex::new(Connection::open_in_memory().unwrap()))
    }

    fn person(name: &str) -> Entity {
        Entity {
            id: None,
            entity_type: EntityType::Person,
            name: name.to_string(),
            metadata: serde_json::json!({}),
        }
    }

    // -- SqliteVectorIndex tests --

    #[tokio::test]
    async fn test_sqlite_vector_upsert_and_search() {
        let conn = in_memory_conn();
        let index = SqliteVectorIndex::new(conn).unwrap();

        index
            .upsert("a", vec![1.0, 0.0, 0.0], serde_json::json!({"label": "a"}))
            .await
            .unwrap();
        index
            .upsert("b", vec![0.0, 1.0, 0.0], serde_json::json!({"label": "b"}))
            .await
            .unwrap();

        let results = index.search(vec![1.0, 0.0, 0.0], 2).await.unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].id, "a");
        assert!((results[0].score - 1.0).abs() < 1e-6);
    }

    #[tokio::test]
    async fn test_sqlite_vector_upsert_updates() {
        let conn = in_memory_conn();
        let index = SqliteVectorIndex::new(conn).unwrap();

        index
            .upsert("x", vec![1.0, 0.0], serde_json::json!({"v": 1}))
            .await
            .unwrap();
        index
            .upsert("x", vec![0.0, 1.0], serde_json::json!({"v": 2}))
            .await
            .unwrap();

        let results = index.search(vec![0.0, 1.0], 10).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].metadata["v"], 2);
    }

    #[tokio::test]
    async fn test_sqlite_vector_delete() {
        let conn = in_memory_conn();
        let index = SqliteVectorIndex::new(conn).unwrap();

        index
            .upsert("a", vec![1.0, 0.0], serde_json::json!({}))
            .await
            .unwrap();
        index.delete("a").await.unwrap();

        let results = index.search(vec![1.0, 0.0], 10).await.unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_sqlite_vector_persistence_roundtrip() {
        let conn = in_memory_conn();
        let index = SqliteVectorIndex::new(Arc::clone(&conn)).unwrap();

        index
            .upsert("persist", vec![0.5, 0.5, 0.5], serde_json::json!({"key": "val"}))
            .await
            .unwrap();

        // Create a new index instance sharing the same connection (simulates reopening)
        let index2 = SqliteVectorIndex::new(conn).unwrap();
        let results = index2.search(vec![0.5, 0.5, 0.5], 1).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "persist");
        assert_eq!(results[0].metadata["key"], "val");
    }

    // -- SqliteGraph tests --

    #[tokio::test]
    async fn test_sqlite_graph_add_entity() {
        let conn = in_memory_conn();
        let graph = SqliteGraph::new(conn).unwrap();

        let id = graph.add_entity(person("Alice")).await.unwrap();
        assert_ne!(id, Uuid::nil());
    }

    #[tokio::test]
    async fn test_sqlite_graph_add_relation_and_explore() {
        let conn = in_memory_conn();
        let graph = SqliteGraph::new(conn).unwrap();

        let alice = graph.add_entity(person("Alice")).await.unwrap();
        let bob = graph.add_entity(person("Bob")).await.unwrap();
        graph.add_relation(alice, "knows", bob).await.unwrap();

        let params = ExploreParams {
            max_depth: 1,
            max_nodes: 10,
            relevance_threshold: 0.0,
            ..Default::default()
        };

        let nodes = graph.explore(alice, params).await.unwrap();
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0].entity.name, "Alice");
        assert_eq!(nodes[1].entity.name, "Bob");
    }

    #[tokio::test]
    async fn test_sqlite_graph_persistence_roundtrip() {
        let conn = in_memory_conn();
        let graph = SqliteGraph::new(Arc::clone(&conn)).unwrap();

        let alice = graph.add_entity(person("Alice")).await.unwrap();
        let bob = graph.add_entity(person("Bob")).await.unwrap();
        graph.add_relation(alice, "works_with", bob).await.unwrap();

        // Create new graph instance on same connection
        let graph2 = SqliteGraph::new(conn).unwrap();
        let params = ExploreParams {
            max_depth: 1,
            max_nodes: 10,
            relevance_threshold: 0.0,
            ..Default::default()
        };

        let nodes = graph2.explore(alice, params).await.unwrap();
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0].entity.name, "Alice");
        assert_eq!(nodes[1].entity.name, "Bob");
        assert_eq!(nodes[0].relations[0].relation, "works_with");
    }

    #[tokio::test]
    async fn test_sqlite_graph_explore_depth_limit() {
        let conn = in_memory_conn();
        let graph = SqliteGraph::new(conn).unwrap();

        let a = graph.add_entity(person("A")).await.unwrap();
        let b = graph.add_entity(person("B")).await.unwrap();
        let c = graph.add_entity(person("C")).await.unwrap();

        graph.add_relation(a, "knows", b).await.unwrap();
        graph.add_relation(b, "knows", c).await.unwrap();

        let params = ExploreParams {
            max_depth: 1,
            max_nodes: 50,
            relevance_threshold: 0.0,
            ..Default::default()
        };

        let nodes = graph.explore(a, params).await.unwrap();
        assert_eq!(nodes.len(), 2); // A and B only
    }
}
