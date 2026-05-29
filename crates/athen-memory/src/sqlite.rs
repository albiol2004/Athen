//! SQLite-backed persistent versions of VectorIndex and KnowledgeGraph.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
use uuid::Uuid;

use athen_core::error::{AthenError, Result};
use athen_core::traits::memory::{
    Entity, EntityId, EntityType, ExploreParams, GraphEdge, GraphNode, KnowledgeGraph,
    LexicalIndex, RankedRow, SearchResult, VectorIndex,
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
                    metadata_json TEXT NOT NULL,
                    created_at TEXT,
                    last_recalled_at TEXT,
                    recall_count INTEGER NOT NULL DEFAULT 0
                );",
            )
            .map_err(|e| AthenError::Other(e.to_string()))?;

            // Migration for pre-existing DBs: add the ranking-signal columns.
            // SQLite has no IF NOT EXISTS for ADD COLUMN, so ignore the error
            // when the column already exists (matches the edges-table pattern).
            let _ = c.execute_batch("ALTER TABLE vectors ADD COLUMN created_at TEXT;");
            let _ = c.execute_batch("ALTER TABLE vectors ADD COLUMN last_recalled_at TEXT;");
            let _ = c.execute_batch(
                "ALTER TABLE vectors ADD COLUMN recall_count INTEGER NOT NULL DEFAULT 0;",
            );
        }
        Ok(Self { conn })
    }
}

/// Parse an optional RFC3339 timestamp column into `Option<DateTime<Utc>>`.
fn parse_opt_ts(s: Option<String>) -> Option<DateTime<Utc>> {
    s.filter(|v| !v.is_empty())
        .and_then(|v| DateTime::parse_from_rfc3339(&v).ok())
        .map(|dt| dt.with_timezone(&Utc))
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
    async fn upsert(
        &self,
        id: &str,
        embedding: Vec<f32>,
        metadata: serde_json::Value,
    ) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AthenError::Other(e.to_string()))?;
        let blob = embedding_to_bytes(&embedding);
        let json = serde_json::to_string(&metadata)?;
        let now = Utc::now().to_rfc3339();
        // Stamp created_at on first insert; preserve it (and the consult
        // signals) on update so re-storing a memory doesn't reset its age.
        conn.execute(
            "INSERT INTO vectors (id, embedding, metadata_json, created_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(id) DO UPDATE SET embedding = excluded.embedding, metadata_json = excluded.metadata_json",
            params![id, blob, json, now],
        )
        .map_err(|e| AthenError::Other(e.to_string()))?;
        Ok(())
    }

    async fn search(&self, query_embedding: Vec<f32>, top_k: usize) -> Result<Vec<SearchResult>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AthenError::Other(e.to_string()))?;
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
            .map(|(score, id, metadata)| SearchResult {
                id,
                score,
                metadata,
            })
            .collect())
    }

    async fn delete(&self, id: &str) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AthenError::Other(e.to_string()))?;
        conn.execute("DELETE FROM vectors WHERE id = ?1", params![id])
            .map_err(|e| AthenError::Other(e.to_string()))?;
        Ok(())
    }

    async fn list_all(&self) -> Result<Vec<SearchResult>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AthenError::Other(e.to_string()))?;
        let mut stmt = conn
            .prepare("SELECT id, metadata_json FROM vectors ORDER BY rowid DESC")
            .map_err(|e| AthenError::Other(e.to_string()))?;

        let rows = stmt
            .query_map([], |row| {
                let id: String = row.get(0)?;
                let meta_str: String = row.get(1)?;
                Ok((id, meta_str))
            })
            .map_err(|e| AthenError::Other(e.to_string()))?;

        let mut results = Vec::new();
        for row in rows {
            let (id, meta_str) = row.map_err(|e| AthenError::Other(e.to_string()))?;
            let metadata: serde_json::Value =
                serde_json::from_str(&meta_str).unwrap_or(serde_json::Value::Null);
            results.push(SearchResult {
                id,
                score: 1.0,
                metadata,
            });
        }
        Ok(results)
    }

    async fn scan_scored(&self, query_embedding: Vec<f32>) -> Result<Vec<RankedRow>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AthenError::Other(e.to_string()))?;
        let mut stmt = conn
            .prepare(
                "SELECT id, embedding, metadata_json, created_at, last_recalled_at, recall_count
                 FROM vectors",
            )
            .map_err(|e| AthenError::Other(e.to_string()))?;

        let rows = stmt
            .query_map([], |row| {
                let id: String = row.get(0)?;
                let blob: Vec<u8> = row.get(1)?;
                let meta_str: String = row.get(2)?;
                let created_at: Option<String> = row.get(3)?;
                let last_recalled_at: Option<String> = row.get(4)?;
                let recall_count: i64 = row.get(5)?;
                Ok((
                    id,
                    blob,
                    meta_str,
                    created_at,
                    last_recalled_at,
                    recall_count,
                ))
            })
            .map_err(|e| AthenError::Other(e.to_string()))?;

        let mut out: Vec<RankedRow> = Vec::new();
        for row in rows {
            let (id, blob, meta_str, created_at, last_recalled_at, recall_count) =
                row.map_err(|e| AthenError::Other(e.to_string()))?;
            let emb = bytes_to_embedding(&blob);
            let cosine = cosine_similarity(&query_embedding, &emb);
            let metadata: serde_json::Value =
                serde_json::from_str(&meta_str).unwrap_or(serde_json::Value::Null);
            out.push(RankedRow {
                id,
                cosine,
                created_at: parse_opt_ts(created_at),
                last_recalled_at: parse_opt_ts(last_recalled_at),
                recall_count: recall_count.max(0) as u32,
                metadata,
            });
        }
        Ok(out)
    }

    async fn bump_recall_stats(&self, ids: &[&str]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let conn = self
            .conn
            .lock()
            .map_err(|e| AthenError::Other(e.to_string()))?;
        let now = Utc::now().to_rfc3339();
        for id in ids {
            conn.execute(
                "UPDATE vectors SET recall_count = recall_count + 1, last_recalled_at = ?1 WHERE id = ?2",
                params![now, id],
            )
            .map_err(|e| AthenError::Other(e.to_string()))?;
        }
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
                    created_at TEXT NOT NULL,
                    strength REAL NOT NULL DEFAULT 0.5,
                    importance REAL NOT NULL DEFAULT 0.5,
                    last_used TEXT NOT NULL DEFAULT ''
                );",
            )
            .map_err(|e| AthenError::Other(e.to_string()))?;

            // Migration: add columns for existing databases.
            // SQLite doesn't support IF NOT EXISTS for ALTER TABLE ADD COLUMN,
            // so we ignore errors (column already exists).
            let _ =
                c.execute_batch("ALTER TABLE edges ADD COLUMN strength REAL NOT NULL DEFAULT 0.5;");
            let _ = c.execute_batch(
                "ALTER TABLE edges ADD COLUMN importance REAL NOT NULL DEFAULT 0.5;",
            );
            let _ =
                c.execute_batch("ALTER TABLE edges ADD COLUMN last_used TEXT NOT NULL DEFAULT '';");

            // memory↔entity links: which entities a given memory mentions.
            // The graph arm of hybrid recall joins through this (indexed)
            // instead of full-scanning every memory's metadata for name matches.
            c.execute_batch(
                "CREATE TABLE IF NOT EXISTS mentions (
                    memory_id TEXT NOT NULL,
                    entity_id TEXT NOT NULL REFERENCES entities(id),
                    PRIMARY KEY (memory_id, entity_id)
                );
                CREATE INDEX IF NOT EXISTS idx_mentions_entity ON mentions(entity_id);
                CREATE INDEX IF NOT EXISTS idx_mentions_memory ON mentions(memory_id);",
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
    strength: f32,
    importance: f32,
    last_used: DateTime<Utc>,
}

/// Calculate effective strength with time-based decay.
/// Half-life: 30 days. Strength never drops below 0.01.
fn decay_strength(base_strength: f32, last_used: &DateTime<Utc>) -> f32 {
    let age_secs = (Utc::now() - *last_used).num_seconds().max(0) as f64;
    let half_life_secs = 30.0 * 24.0 * 3600.0; // 30 days
    let decay = (-age_secs * 2.0f64.ln() / half_life_secs).exp() as f32;
    (base_strength * decay).max(0.01)
}

fn edge_score(edge: &SqliteEdge, params: &ExploreParams) -> f32 {
    let effective_strength = decay_strength(edge.strength, &edge.last_used);

    let age_secs = (Utc::now() - edge.created_at).num_seconds().max(0) as f64;
    let half_life_secs = 7.0 * 24.0 * 3600.0;
    let recency = (-age_secs * (2.0f64.ln()) / half_life_secs).exp() as f32;
    let frequency = effective_strength;
    let importance = edge.importance.min(1.0);

    params.recency_weight * recency
        + params.frequency_weight * frequency
        + params.importance_weight * importance
}

#[async_trait]
impl KnowledgeGraph for SqliteGraph {
    async fn add_entity(&self, mut entity: Entity) -> Result<EntityId> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AthenError::Other(e.to_string()))?;

        // Deduplicate by name: if an entity with the same name exists, return its ID.
        let existing: Option<String> = conn
            .query_row(
                "SELECT id FROM entities WHERE name = ?1 COLLATE NOCASE LIMIT 1",
                params![entity.name],
                |row| row.get(0),
            )
            .ok();

        if let Some(existing_id) = existing {
            if let Ok(uuid) = Uuid::parse_str(&existing_id) {
                return Ok(uuid);
            }
        }

        let id = entity.id.unwrap_or_else(Uuid::new_v4);
        entity.id = Some(id);

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
        let conn = self
            .conn
            .lock()
            .map_err(|e| AthenError::Other(e.to_string()))?;
        let now = Utc::now().to_rfc3339();

        conn.execute(
            "INSERT INTO edges (from_entity, relation, to_entity, weight, created_at, strength, importance, last_used) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![from.to_string(), relation, to.to_string(), 1.0f64, now, 0.5f64, 0.5f64, now],
        )
        .map_err(|e| AthenError::Other(e.to_string()))?;

        Ok(())
    }

    async fn add_relation_weighted(
        &self,
        from: EntityId,
        relation: &str,
        to: EntityId,
        importance: f32,
    ) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AthenError::Other(e.to_string()))?;
        let now = Utc::now().to_rfc3339();
        let importance_clamped = importance.clamp(0.0, 1.0) as f64;

        conn.execute(
            "INSERT INTO edges (from_entity, relation, to_entity, weight, created_at, strength, importance, last_used) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![from.to_string(), relation, to.to_string(), 1.0f64, now, 0.5f64, importance_clamped, now],
        )
        .map_err(|e| AthenError::Other(e.to_string()))?;

        Ok(())
    }

    async fn reinforce_entity(&self, entity_id: EntityId, amount: f32) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AthenError::Other(e.to_string()))?;
        let now = Utc::now().to_rfc3339();
        let id_str = entity_id.to_string();

        conn.execute(
            "UPDATE edges SET strength = MIN(strength + ?1, 1.0), last_used = ?2 WHERE from_entity = ?3 OR to_entity = ?3",
            params![amount as f64, now, id_str],
        )
        .map_err(|e| AthenError::Other(e.to_string()))?;

        Ok(())
    }

    async fn explore(&self, entry: EntityId, params: ExploreParams) -> Result<Vec<GraphNode>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AthenError::Other(e.to_string()))?;

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
                let id = Uuid::parse_str(&id_str).map_err(|e| AthenError::Other(e.to_string()))?;
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
                .prepare("SELECT from_entity, relation, to_entity, weight, created_at, strength, importance, last_used FROM edges")
                .map_err(|e| AthenError::Other(e.to_string()))?;
            let rows = stmt
                .query_map([], |row| {
                    let from_str: String = row.get(0)?;
                    let relation: String = row.get(1)?;
                    let to_str: String = row.get(2)?;
                    let weight: f64 = row.get(3)?;
                    let created_str: String = row.get(4)?;
                    let strength: f64 = row.get(5)?;
                    let importance: f64 = row.get(6)?;
                    let last_used_str: String = row.get(7)?;
                    Ok((
                        from_str,
                        relation,
                        to_str,
                        weight,
                        created_str,
                        strength,
                        importance,
                        last_used_str,
                    ))
                })
                .map_err(|e| AthenError::Other(e.to_string()))?;

            for row in rows {
                let (
                    from_str,
                    relation,
                    to_str,
                    weight,
                    created_str,
                    strength,
                    importance,
                    last_used_str,
                ) = row.map_err(|e| AthenError::Other(e.to_string()))?;
                let from =
                    Uuid::parse_str(&from_str).map_err(|e| AthenError::Other(e.to_string()))?;
                let to = Uuid::parse_str(&to_str).map_err(|e| AthenError::Other(e.to_string()))?;
                let created_at = DateTime::parse_from_rfc3339(&created_str)
                    .map(|dt| dt.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now());
                let last_used = DateTime::parse_from_rfc3339(&last_used_str)
                    .map(|dt| dt.with_timezone(&Utc))
                    .unwrap_or(created_at);

                all_edges.push(SqliteEdge {
                    from,
                    relation,
                    to,
                    weight: weight as f32,
                    created_at,
                    strength: strength as f32,
                    importance: importance as f32,
                    last_used,
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

                        if depth < params.max_depth
                            && !visited.contains(&edge.to)
                            && edge.weight >= params.relevance_threshold
                        {
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

    async fn list_entities(&self) -> Result<Vec<Entity>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AthenError::Other(e.to_string()))?;
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

        let mut entities = Vec::new();
        for row in rows {
            let (id_str, type_str, name, meta_str) =
                row.map_err(|e| AthenError::Other(e.to_string()))?;
            let id = Uuid::parse_str(&id_str).map_err(|e| AthenError::Other(e.to_string()))?;
            let metadata: serde_json::Value =
                serde_json::from_str(&meta_str).unwrap_or(serde_json::Value::Null);
            entities.push(Entity {
                id: Some(id),
                entity_type: str_to_entity_type(&type_str),
                name,
                metadata,
            });
        }
        Ok(entities)
    }

    async fn list_relations(&self) -> Result<Vec<(EntityId, String, String, EntityId, String)>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AthenError::Other(e.to_string()))?;
        let mut stmt = conn
            .prepare(
                "SELECT e.from_entity, ef.name, e.relation, e.to_entity, et.name
                 FROM edges e
                 JOIN entities ef ON ef.id = e.from_entity
                 JOIN entities et ON et.id = e.to_entity",
            )
            .map_err(|e| AthenError::Other(e.to_string()))?;

        let rows = stmt
            .query_map([], |row| {
                let from_str: String = row.get(0)?;
                let from_name: String = row.get(1)?;
                let relation: String = row.get(2)?;
                let to_str: String = row.get(3)?;
                let to_name: String = row.get(4)?;
                Ok((from_str, from_name, relation, to_str, to_name))
            })
            .map_err(|e| AthenError::Other(e.to_string()))?;

        let mut relations = Vec::new();
        for row in rows {
            let (from_str, from_name, relation, to_str, to_name) =
                row.map_err(|e| AthenError::Other(e.to_string()))?;
            let from_id =
                Uuid::parse_str(&from_str).map_err(|e| AthenError::Other(e.to_string()))?;
            let to_id = Uuid::parse_str(&to_str).map_err(|e| AthenError::Other(e.to_string()))?;
            relations.push((from_id, from_name, relation, to_id, to_name));
        }
        Ok(relations)
    }

    async fn update_entity(
        &self,
        id: EntityId,
        name: Option<String>,
        entity_type: Option<EntityType>,
    ) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AthenError::Other(e.to_string()))?;
        if let Some(new_name) = &name {
            conn.execute(
                "UPDATE entities SET name = ?1 WHERE id = ?2",
                params![new_name, id.to_string()],
            )
            .map_err(|e| AthenError::Other(e.to_string()))?;
        }
        if let Some(new_type) = &entity_type {
            conn.execute(
                "UPDATE entities SET entity_type = ?1 WHERE id = ?2",
                params![entity_type_to_str(new_type), id.to_string()],
            )
            .map_err(|e| AthenError::Other(e.to_string()))?;
        }
        Ok(())
    }

    async fn delete_entity(&self, id: EntityId) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AthenError::Other(e.to_string()))?;
        let id_str = id.to_string();
        conn.execute(
            "DELETE FROM edges WHERE from_entity = ?1 OR to_entity = ?1",
            params![id_str],
        )
        .map_err(|e| AthenError::Other(e.to_string()))?;
        conn.execute("DELETE FROM entities WHERE id = ?1", params![id_str])
            .map_err(|e| AthenError::Other(e.to_string()))?;
        Ok(())
    }

    async fn delete_relation(&self, from: EntityId, to: EntityId, relation: &str) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AthenError::Other(e.to_string()))?;
        conn.execute(
            "DELETE FROM edges WHERE from_entity = ?1 AND to_entity = ?2 AND relation = ?3",
            params![from.to_string(), to.to_string(), relation],
        )
        .map_err(|e| AthenError::Other(e.to_string()))?;
        Ok(())
    }

    async fn find_entity_by_name(&self, name: &str) -> Result<Option<EntityId>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AthenError::Other(e.to_string()))?;
        let existing: Option<String> = conn
            .query_row(
                "SELECT id FROM entities WHERE name = ?1 COLLATE NOCASE LIMIT 1",
                params![name],
                |row| row.get(0),
            )
            .ok();
        Ok(existing.and_then(|s| Uuid::parse_str(&s).ok()))
    }

    async fn link_memory(&self, memory_id: &str, entity_ids: &[EntityId]) -> Result<()> {
        if entity_ids.is_empty() {
            return Ok(());
        }
        let conn = self
            .conn
            .lock()
            .map_err(|e| AthenError::Other(e.to_string()))?;
        for eid in entity_ids {
            conn.execute(
                "INSERT OR IGNORE INTO mentions (memory_id, entity_id) VALUES (?1, ?2)",
                params![memory_id, eid.to_string()],
            )
            .map_err(|e| AthenError::Other(e.to_string()))?;
        }
        Ok(())
    }

    async fn memories_for_entities(&self, entity_ids: &[EntityId]) -> Result<Vec<String>> {
        if entity_ids.is_empty() {
            return Ok(vec![]);
        }
        let conn = self
            .conn
            .lock()
            .map_err(|e| AthenError::Other(e.to_string()))?;
        // Build a parameterized IN-list (?1, ?2, ...).
        let placeholders = (1..=entity_ids.len())
            .map(|i| format!("?{i}"))
            .collect::<Vec<_>>()
            .join(", ");
        let sql =
            format!("SELECT DISTINCT memory_id FROM mentions WHERE entity_id IN ({placeholders})");
        let id_strings: Vec<String> = entity_ids.iter().map(|e| e.to_string()).collect();
        let param_refs: Vec<&dyn rusqlite::ToSql> = id_strings
            .iter()
            .map(|s| s as &dyn rusqlite::ToSql)
            .collect();
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| AthenError::Other(e.to_string()))?;
        let rows = stmt
            .query_map(param_refs.as_slice(), |row| row.get::<_, String>(0))
            .map_err(|e| AthenError::Other(e.to_string()))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| AthenError::Other(e.to_string()))?);
        }
        Ok(out)
    }

    async fn unlink_memory(&self, memory_id: &str) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AthenError::Other(e.to_string()))?;
        conn.execute(
            "DELETE FROM mentions WHERE memory_id = ?1",
            params![memory_id],
        )
        .map_err(|e| AthenError::Other(e.to_string()))?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// SqliteLexicalIndex (FTS5 / BM25)
// ---------------------------------------------------------------------------

/// SQLite FTS5-backed lexical index over memory content. The keyword arm of
/// hybrid recall: `bm25()` ranking, complementary to the semantic cosine arm.
pub struct SqliteLexicalIndex {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteLexicalIndex {
    pub fn new(conn: Arc<Mutex<Connection>>) -> Result<Self> {
        {
            let c = conn.lock().map_err(|e| AthenError::Other(e.to_string()))?;
            // Contentless-ish FTS5 table: `content` is indexed, `memory_id`
            // is stored UNINDEXED so we can map matches back to memory ids.
            // Requires SQLite built with FTS5 (rusqlite `bundled` ships it).
            c.execute_batch(
                "CREATE VIRTUAL TABLE IF NOT EXISTS memory_fts USING fts5(
                    content,
                    memory_id UNINDEXED,
                    tokenize = 'porter unicode61'
                );",
            )
            .map_err(|e| {
                AthenError::Other(format!(
                    "FTS5 init failed (is SQLite built with FTS5?): {e}"
                ))
            })?;
        }
        Ok(Self { conn })
    }
}

/// Turn an arbitrary user query into a safe FTS5 MATCH expression: extract
/// alphanumeric tokens (len >= 2), quote each (so punctuation can't break the
/// MATCH grammar), and OR them together for recall breadth. Returns None when
/// the query has no usable tokens.
fn fts5_match_query(query: &str) -> Option<String> {
    let tokens: Vec<String> = query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.chars().count() >= 2)
        .map(|t| format!("\"{}\"", t.to_lowercase()))
        .collect();
    if tokens.is_empty() {
        None
    } else {
        Some(tokens.join(" OR "))
    }
}

#[async_trait]
impl LexicalIndex for SqliteLexicalIndex {
    async fn upsert(&self, id: &str, text: &str) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AthenError::Other(e.to_string()))?;
        // FTS5 has no UPSERT; delete any prior rows for this id then insert.
        conn.execute("DELETE FROM memory_fts WHERE memory_id = ?1", params![id])
            .map_err(|e| AthenError::Other(e.to_string()))?;
        conn.execute(
            "INSERT INTO memory_fts (content, memory_id) VALUES (?1, ?2)",
            params![text, id],
        )
        .map_err(|e| AthenError::Other(e.to_string()))?;
        Ok(())
    }

    async fn search(&self, query: &str, top_k: usize) -> Result<Vec<(String, f32)>> {
        let Some(match_expr) = fts5_match_query(query) else {
            return Ok(vec![]);
        };
        let conn = self
            .conn
            .lock()
            .map_err(|e| AthenError::Other(e.to_string()))?;
        let mut stmt = conn
            .prepare(
                "SELECT memory_id, bm25(memory_fts) AS rank
                 FROM memory_fts
                 WHERE memory_fts MATCH ?1
                 ORDER BY rank
                 LIMIT ?2",
            )
            .map_err(|e| AthenError::Other(e.to_string()))?;

        // bm25() returns smaller (more negative) = better. Collect raw scores,
        // convert to "higher = better", then min-max normalize the batch to
        // [0,1] so the fusion layer sees a comparable signal.
        let rows = stmt
            .query_map(params![match_expr, top_k as i64], |row| {
                let id: String = row.get(0)?;
                let rank: f64 = row.get(1)?;
                Ok((id, rank))
            })
            .map_err(|e| AthenError::Other(e.to_string()))?;

        let mut raw: Vec<(String, f64)> = Vec::new();
        for row in rows {
            let (id, rank) = row.map_err(|e| AthenError::Other(e.to_string()))?;
            raw.push((id, -rank)); // negate: higher = better match
        }
        if raw.is_empty() {
            return Ok(vec![]);
        }

        let max = raw.iter().map(|(_, s)| *s).fold(f64::MIN, f64::max);
        let min = raw.iter().map(|(_, s)| *s).fold(f64::MAX, f64::min);
        let span = max - min;
        Ok(raw
            .into_iter()
            .map(|(id, s)| {
                let norm = if span > f64::EPSILON {
                    (s - min) / span
                } else {
                    1.0
                };
                (id, norm as f32)
            })
            .collect())
    }

    async fn delete(&self, id: &str) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AthenError::Other(e.to_string()))?;
        conn.execute("DELETE FROM memory_fts WHERE memory_id = ?1", params![id])
            .map_err(|e| AthenError::Other(e.to_string()))?;
        Ok(())
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

    // -- C2 de-risk: FTS5 availability + mentions + scan_scored --

    #[tokio::test]
    async fn test_fts5_lexical_index_roundtrip() {
        let conn = in_memory_conn();
        let lex = SqliteLexicalIndex::new(conn).unwrap();
        lex.upsert("m1", "the quarterly revenue report for Acme")
            .await
            .unwrap();
        lex.upsert("m2", "a recipe for sourdough bread")
            .await
            .unwrap();

        let hits = lex.search("revenue report", 10).await.unwrap();
        assert!(hits.iter().any(|(id, _)| id == "m1"), "expected m1 hit");
        assert!(
            !hits.iter().any(|(id, _)| id == "m2"),
            "m2 should not match"
        );
        for (_, score) in &hits {
            assert!((0.0..=1.0).contains(score), "score normalized: {score}");
        }

        // Re-index then delete.
        lex.upsert("m1", "completely different text about gardening")
            .await
            .unwrap();
        assert!(lex
            .search("revenue", 10)
            .await
            .unwrap()
            .iter()
            .all(|(id, _)| id != "m1"));
        lex.delete("m1").await.unwrap();
        assert!(lex.search("gardening", 10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_mentions_link_and_query() {
        let conn = in_memory_conn();
        let graph = SqliteGraph::new(conn).unwrap();
        let acme = graph.add_entity(person("Acme")).await.unwrap();
        let beta = graph.add_entity(person("Beta")).await.unwrap();

        graph.link_memory("mem-1", &[acme]).await.unwrap();
        graph.link_memory("mem-2", &[acme, beta]).await.unwrap();

        let mut for_acme = graph.memories_for_entities(&[acme]).await.unwrap();
        for_acme.sort();
        assert_eq!(for_acme, vec!["mem-1".to_string(), "mem-2".to_string()]);

        let for_beta = graph.memories_for_entities(&[beta]).await.unwrap();
        assert_eq!(for_beta, vec!["mem-2".to_string()]);

        graph.unlink_memory("mem-2").await.unwrap();
        let for_acme_after = graph.memories_for_entities(&[acme]).await.unwrap();
        assert_eq!(for_acme_after, vec!["mem-1".to_string()]);
    }

    #[tokio::test]
    async fn test_scan_scored_and_bump_recall() {
        let conn = in_memory_conn();
        let index = SqliteVectorIndex::new(conn).unwrap();
        index
            .upsert("a", vec![1.0, 0.0], serde_json::json!({"_content": "a"}))
            .await
            .unwrap();
        index
            .upsert("b", vec![0.0, 1.0], serde_json::json!({"_content": "b"}))
            .await
            .unwrap();

        let rows = index.scan_scored(vec![1.0, 0.0]).await.unwrap();
        assert_eq!(rows.len(), 2);
        let a = rows.iter().find(|r| r.id == "a").unwrap();
        assert!((a.cosine - 1.0).abs() < 1e-6);
        assert!(a.created_at.is_some(), "created_at stamped on upsert");
        assert_eq!(a.recall_count, 0);
        assert!(a.last_recalled_at.is_none());

        index.bump_recall_stats(&["a", "a"]).await.unwrap();
        let rows2 = index.scan_scored(vec![1.0, 0.0]).await.unwrap();
        let a2 = rows2.iter().find(|r| r.id == "a").unwrap();
        assert_eq!(a2.recall_count, 2);
        assert!(a2.last_recalled_at.is_some());
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
            .upsert(
                "persist",
                vec![0.5, 0.5, 0.5],
                serde_json::json!({"key": "val"}),
            )
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

    #[tokio::test]
    async fn test_sqlite_graph_entity_deduplication_by_name() {
        let conn = in_memory_conn();
        let graph = SqliteGraph::new(conn).unwrap();

        // Adding "Nadia" twice should return the same EntityId.
        let id1 = graph.add_entity(person("Nadia")).await.unwrap();
        let id2 = graph.add_entity(person("Nadia")).await.unwrap();
        assert_eq!(id1, id2, "Same name should deduplicate to same ID");

        // Case-insensitive: "nadia" should also match.
        let id3 = graph.add_entity(person("nadia")).await.unwrap();
        assert_eq!(id1, id3, "Case-insensitive name should deduplicate");

        // A different name should get a different ID.
        let id4 = graph.add_entity(person("Bob")).await.unwrap();
        assert_ne!(id1, id4, "Different name should get different ID");
    }

    #[tokio::test]
    async fn test_sqlite_vector_list_all_newest_first() {
        let conn = in_memory_conn();
        let index = SqliteVectorIndex::new(conn).unwrap();

        index
            .upsert("a", vec![1.0, 0.0], serde_json::json!({"label": "A"}))
            .await
            .unwrap();
        index
            .upsert("b", vec![0.0, 1.0], serde_json::json!({"label": "B"}))
            .await
            .unwrap();
        index
            .upsert("c", vec![0.5, 0.5], serde_json::json!({"label": "C"}))
            .await
            .unwrap();

        let results = index.list_all().await.unwrap();
        assert_eq!(results.len(), 3);
        // ORDER BY rowid DESC → newest first: C, B, A
        assert_eq!(results[0].id, "c");
        assert_eq!(results[1].id, "b");
        assert_eq!(results[2].id, "a");
    }

    // -- Decay + Reinforcement tests --

    #[test]
    fn test_decay_strength() {
        // A fresh edge should have approximately base_strength.
        let now = Utc::now();
        let result = decay_strength(0.5, &now);
        assert!(
            (result - 0.5).abs() < 0.01,
            "Fresh edge should be ~0.5, got {result}"
        );

        // A 30-day-old edge should be approximately half of base_strength.
        let thirty_days_ago = now - chrono::Duration::days(30);
        let result = decay_strength(0.5, &thirty_days_ago);
        assert!(
            (result - 0.25).abs() < 0.02,
            "30-day-old edge should be ~0.25 (half of 0.5), got {result}"
        );

        // A very old edge should not drop below 0.01.
        let ancient = now - chrono::Duration::days(365);
        let result = decay_strength(0.5, &ancient);
        assert!(
            (result - 0.01).abs() < 0.001,
            "Very old edge should be ~0.01, got {result}"
        );
    }

    #[tokio::test]
    async fn test_reinforce_entity() {
        let conn = in_memory_conn();
        let graph = SqliteGraph::new(conn).unwrap();

        let alice = graph.add_entity(person("Alice")).await.unwrap();
        let bob = graph.add_entity(person("Bob")).await.unwrap();
        graph.add_relation(alice, "knows", bob).await.unwrap();

        // Default strength is 0.5. Reinforce by 0.2 -> should become 0.7.
        graph.reinforce_entity(alice, 0.2).await.unwrap();

        // Verify by reading the edge strength from the database.
        let c = graph.conn.lock().unwrap();
        let strength: f64 = c
            .query_row(
                "SELECT strength FROM edges WHERE from_entity = ?1",
                params![alice.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            (strength - 0.7).abs() < 0.001,
            "Strength should be 0.7, got {strength}"
        );
    }

    #[tokio::test]
    async fn test_strength_clamped_to_one() {
        let conn = in_memory_conn();
        let graph = SqliteGraph::new(conn).unwrap();

        let alice = graph.add_entity(person("Alice")).await.unwrap();
        let bob = graph.add_entity(person("Bob")).await.unwrap();
        graph.add_relation(alice, "knows", bob).await.unwrap();

        // Reinforce by 0.9 (0.5 + 0.9 = 1.4, should clamp to 1.0).
        graph.reinforce_entity(alice, 0.9).await.unwrap();

        let c = graph.conn.lock().unwrap();
        let strength: f64 = c
            .query_row(
                "SELECT strength FROM edges WHERE from_entity = ?1",
                params![alice.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            (strength - 1.0).abs() < 0.001,
            "Strength should be clamped to 1.0, got {strength}"
        );
    }

    #[tokio::test]
    async fn test_add_relation_weighted() {
        let conn = in_memory_conn();
        let graph = SqliteGraph::new(conn).unwrap();

        let alice = graph.add_entity(person("Alice")).await.unwrap();
        let bob = graph.add_entity(person("Bob")).await.unwrap();
        graph
            .add_relation_weighted(alice, "married_to", bob, 0.9)
            .await
            .unwrap();

        let c = graph.conn.lock().unwrap();
        let importance: f64 = c
            .query_row(
                "SELECT importance FROM edges WHERE from_entity = ?1",
                params![alice.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            (importance - 0.9).abs() < 0.001,
            "Importance should be 0.9, got {importance}"
        );
    }
}
