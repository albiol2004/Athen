//! Knowledge graph storage and exploration.

use std::collections::{HashMap, HashSet, VecDeque};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use tokio::sync::RwLock;
use uuid::Uuid;

use athen_core::error::Result;
use athen_core::traits::memory::{
    Entity, EntityId, EntityType, ExploreParams, GraphEdge, GraphNode, KnowledgeGraph,
};

pub(crate) struct Edge {
    pub(crate) from: EntityId,
    pub(crate) relation: String,
    pub(crate) to: EntityId,
    pub(crate) weight: f32,
    pub(crate) created_at: DateTime<Utc>,
}

/// In-memory knowledge graph with BFS exploration.
pub struct InMemoryGraph {
    pub(crate) entities: RwLock<HashMap<EntityId, Entity>>,
    pub(crate) edges: RwLock<Vec<Edge>>,
}

impl InMemoryGraph {
    pub fn new() -> Self {
        Self {
            entities: RwLock::new(HashMap::new()),
            edges: RwLock::new(Vec::new()),
        }
    }
}

impl Default for InMemoryGraph {
    fn default() -> Self {
        Self::new()
    }
}

/// Compute a combined score for an edge based on explore params.
fn edge_score(edge: &Edge, params: &ExploreParams) -> f32 {
    // Recency: exponential decay, half-life of 7 days
    let age_secs = (Utc::now() - edge.created_at).num_seconds().max(0) as f64;
    let half_life_secs = 7.0 * 24.0 * 3600.0;
    let recency = (-age_secs * (2.0f64.ln()) / half_life_secs).exp() as f32;

    // Frequency: use weight as a proxy (higher weight = more frequent interaction)
    let frequency = edge.weight.min(1.0);

    // Importance: also based on weight
    let importance = edge.weight.min(1.0);

    params.recency_weight * recency
        + params.frequency_weight * frequency
        + params.importance_weight * importance
}

#[async_trait]
impl KnowledgeGraph for InMemoryGraph {
    async fn add_entity(&self, mut entity: Entity) -> Result<EntityId> {
        let id = entity.id.unwrap_or_else(Uuid::new_v4);
        entity.id = Some(id);
        self.entities.write().await.insert(id, entity);
        Ok(id)
    }

    async fn add_relation(&self, from: EntityId, relation: &str, to: EntityId) -> Result<()> {
        self.edges.write().await.push(Edge {
            from,
            relation: relation.to_string(),
            to,
            weight: 1.0,
            created_at: Utc::now(),
        });
        Ok(())
    }

    async fn explore(
        &self,
        entry: EntityId,
        params: ExploreParams,
    ) -> Result<Vec<GraphNode>> {
        let entities = self.entities.read().await;
        let edges = self.edges.read().await;

        let mut visited: HashSet<EntityId> = HashSet::new();
        let mut result: Vec<GraphNode> = Vec::new();

        // BFS queue: (entity_id, depth)
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

            // Find outgoing edges from current entity
            let mut node_relations: Vec<GraphEdge> = Vec::new();

            for edge in edges.iter() {
                if edge.from == current_id {
                    let score = edge_score(edge, &params);
                    if score >= params.relevance_threshold || depth == 0 {
                        node_relations.push(GraphEdge {
                            relation: edge.relation.clone(),
                            target: edge.to,
                            weight: edge.weight,
                        });

                        // Enqueue neighbor if within depth limit and not visited
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

    async fn list_entities(&self) -> Result<Vec<Entity>> {
        let entities = self.entities.read().await;
        Ok(entities.values().cloned().collect())
    }

    async fn list_relations(&self) -> Result<Vec<(EntityId, String, String, EntityId, String)>> {
        let entities = self.entities.read().await;
        let edges = self.edges.read().await;
        let mut result = Vec::new();
        for edge in edges.iter() {
            let from_name = entities
                .get(&edge.from)
                .map(|e| e.name.clone())
                .unwrap_or_default();
            let to_name = entities
                .get(&edge.to)
                .map(|e| e.name.clone())
                .unwrap_or_default();
            result.push((edge.from, from_name, edge.relation.clone(), edge.to, to_name));
        }
        Ok(result)
    }

    async fn update_entity(
        &self,
        id: EntityId,
        name: Option<String>,
        entity_type: Option<EntityType>,
    ) -> Result<()> {
        let mut entities = self.entities.write().await;
        if let Some(entity) = entities.get_mut(&id) {
            if let Some(new_name) = name {
                entity.name = new_name;
            }
            if let Some(new_type) = entity_type {
                entity.entity_type = new_type;
            }
        }
        Ok(())
    }

    async fn delete_entity(&self, id: EntityId) -> Result<()> {
        self.edges.write().await.retain(|e| e.from != id && e.to != id);
        self.entities.write().await.remove(&id);
        Ok(())
    }

    async fn delete_relation(
        &self,
        from: EntityId,
        to: EntityId,
        relation: &str,
    ) -> Result<()> {
        self.edges
            .write()
            .await
            .retain(|e| !(e.from == from && e.to == to && e.relation == relation));
        Ok(())
    }
}

/// A shared wrapper around `InMemoryGraph` that allows inspecting internal state
/// while also being usable as a `Box<dyn KnowledgeGraph>`.
pub struct SharedInMemoryGraph {
    inner: std::sync::Arc<InMemoryGraph>,
}

impl Default for SharedInMemoryGraph {
    fn default() -> Self {
        Self::new()
    }
}

impl SharedInMemoryGraph {
    pub fn new() -> Self {
        Self {
            inner: std::sync::Arc::new(InMemoryGraph::new()),
        }
    }

    /// Access the internal entities for inspection (e.g. in tests).
    pub async fn entities(&self) -> tokio::sync::RwLockReadGuard<'_, HashMap<EntityId, Entity>> {
        self.inner.entities.read().await
    }

    /// Access the internal edges for inspection (e.g. in tests).
    #[allow(dead_code)]
    pub(crate) async fn edges(&self) -> tokio::sync::RwLockReadGuard<'_, Vec<Edge>> {
        self.inner.edges.read().await
    }
}

#[async_trait]
impl KnowledgeGraph for SharedInMemoryGraph {
    async fn add_entity(&self, entity: Entity) -> Result<EntityId> {
        self.inner.add_entity(entity).await
    }

    async fn add_relation(&self, from: EntityId, relation: &str, to: EntityId) -> Result<()> {
        self.inner.add_relation(from, relation, to).await
    }

    async fn explore(&self, entry: EntityId, params: ExploreParams) -> Result<Vec<GraphNode>> {
        self.inner.explore(entry, params).await
    }

    async fn list_entities(&self) -> Result<Vec<Entity>> {
        self.inner.list_entities().await
    }

    async fn list_relations(&self) -> Result<Vec<(EntityId, String, String, EntityId, String)>> {
        self.inner.list_relations().await
    }

    async fn update_entity(
        &self,
        id: EntityId,
        name: Option<String>,
        entity_type: Option<EntityType>,
    ) -> Result<()> {
        self.inner.update_entity(id, name, entity_type).await
    }

    async fn delete_entity(&self, id: EntityId) -> Result<()> {
        self.inner.delete_entity(id).await
    }

    async fn delete_relation(
        &self,
        from: EntityId,
        to: EntityId,
        relation: &str,
    ) -> Result<()> {
        self.inner.delete_relation(from, to, relation).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use athen_core::traits::memory::EntityType;

    fn person(name: &str) -> Entity {
        Entity {
            id: None,
            entity_type: EntityType::Person,
            name: name.to_string(),
            metadata: serde_json::json!({}),
        }
    }

    #[tokio::test]
    async fn test_add_entity_generates_id() {
        let graph = InMemoryGraph::new();
        let id = graph.add_entity(person("Alice")).await.unwrap();
        assert_ne!(id, Uuid::nil());
    }

    #[tokio::test]
    async fn test_add_entity_preserves_given_id() {
        let graph = InMemoryGraph::new();
        let given_id = Uuid::new_v4();
        let entity = Entity {
            id: Some(given_id),
            entity_type: EntityType::Person,
            name: "Bob".to_string(),
            metadata: serde_json::json!({}),
        };
        let id = graph.add_entity(entity).await.unwrap();
        assert_eq!(id, given_id);
    }

    #[tokio::test]
    async fn test_add_relation_and_explore() {
        let graph = InMemoryGraph::new();
        let alice = graph.add_entity(person("Alice")).await.unwrap();
        let bob = graph.add_entity(person("Bob")).await.unwrap();

        graph
            .add_relation(alice, "knows", bob)
            .await
            .unwrap();

        let params = ExploreParams {
            max_depth: 1,
            max_nodes: 10,
            relevance_threshold: 0.0,
            ..Default::default()
        };

        let nodes = graph.explore(alice, params).await.unwrap();
        assert_eq!(nodes.len(), 2); // Alice and Bob
        assert_eq!(nodes[0].entity.name, "Alice");
        assert_eq!(nodes[0].depth, 0);
        assert_eq!(nodes[0].relations.len(), 1);
        assert_eq!(nodes[0].relations[0].relation, "knows");
        assert_eq!(nodes[1].entity.name, "Bob");
        assert_eq!(nodes[1].depth, 1);
    }

    #[tokio::test]
    async fn test_explore_depth_limit() {
        let graph = InMemoryGraph::new();
        let a = graph.add_entity(person("A")).await.unwrap();
        let b = graph.add_entity(person("B")).await.unwrap();
        let c = graph.add_entity(person("C")).await.unwrap();
        let d = graph.add_entity(person("D")).await.unwrap();

        graph.add_relation(a, "knows", b).await.unwrap();
        graph.add_relation(b, "knows", c).await.unwrap();
        graph.add_relation(c, "knows", d).await.unwrap();

        // max_depth=1 should only get A and B
        let params = ExploreParams {
            max_depth: 1,
            max_nodes: 50,
            relevance_threshold: 0.0,
            ..Default::default()
        };

        let nodes = graph.explore(a, params).await.unwrap();
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0].entity.name, "A");
        assert_eq!(nodes[1].entity.name, "B");
    }

    #[tokio::test]
    async fn test_explore_max_nodes() {
        let graph = InMemoryGraph::new();
        let a = graph.add_entity(person("A")).await.unwrap();
        let b = graph.add_entity(person("B")).await.unwrap();
        let c = graph.add_entity(person("C")).await.unwrap();

        graph.add_relation(a, "knows", b).await.unwrap();
        graph.add_relation(a, "knows", c).await.unwrap();

        let params = ExploreParams {
            max_depth: 3,
            max_nodes: 2,
            relevance_threshold: 0.0,
            ..Default::default()
        };

        let nodes = graph.explore(a, params).await.unwrap();
        assert_eq!(nodes.len(), 2);
    }

    #[tokio::test]
    async fn test_explore_nonexistent_entry() {
        let graph = InMemoryGraph::new();
        let fake_id = Uuid::new_v4();

        let params = ExploreParams::default();
        let nodes = graph.explore(fake_id, params).await.unwrap();
        assert!(nodes.is_empty());
    }

    #[tokio::test]
    async fn test_explore_no_edges() {
        let graph = InMemoryGraph::new();
        let a = graph.add_entity(person("Isolated")).await.unwrap();

        let params = ExploreParams {
            relevance_threshold: 0.0,
            ..Default::default()
        };
        let nodes = graph.explore(a, params).await.unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].entity.name, "Isolated");
        assert!(nodes[0].relations.is_empty());
    }
}
