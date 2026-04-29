//! Integration tests for athen-memory with SQLite-backed storage.
//!
//! These tests exercise the full memory system end-to-end: knowledge graph
//! exploration, vector similarity search, the Memory facade, and persistence
//! across connection lifetimes.

use std::sync::{Arc, Mutex};

use rusqlite::Connection;

use athen_core::traits::memory::{
    Entity, EntityType, ExploreParams, KnowledgeGraph, MemoryItem, MemoryStore, VectorIndex,
};
use athen_llm::embeddings::keyword::KeywordEmbedding;
use athen_memory::sqlite::{SqliteGraph, SqliteVectorIndex};
use athen_memory::Memory;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn in_memory_conn() -> Arc<Mutex<Connection>> {
    Arc::new(Mutex::new(Connection::open_in_memory().unwrap()))
}

fn entity(name: &str, entity_type: EntityType) -> Entity {
    Entity {
        id: None,
        entity_type,
        name: name.to_string(),
        metadata: serde_json::json!({}),
    }
}

// ---------------------------------------------------------------------------
// Test 1: Knowledge graph exploration with realistic data
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_build_and_explore_contact_network() {
    let conn = in_memory_conn();
    let graph = SqliteGraph::new(conn).unwrap();

    // Add entities
    let juan = graph
        .add_entity(entity("Juan", EntityType::Person))
        .await
        .unwrap();
    let maria = graph
        .add_entity(entity("María", EntityType::Person))
        .await
        .unwrap();
    let empresa_x = graph
        .add_entity(entity("Empresa X", EntityType::Organization))
        .await
        .unwrap();
    let alpha = graph
        .add_entity(entity("Alpha", EntityType::Project))
        .await
        .unwrap();

    // Add relations
    graph
        .add_relation(juan, "trabaja_en", empresa_x)
        .await
        .unwrap();
    graph
        .add_relation(maria, "trabaja_en", empresa_x)
        .await
        .unwrap();
    graph.add_relation(juan, "participó", alpha).await.unwrap();
    graph.add_relation(maria, "participó", alpha).await.unwrap();

    // Also add reverse relations so BFS can discover María through shared nodes
    graph
        .add_relation(empresa_x, "emplea_a", juan)
        .await
        .unwrap();
    graph
        .add_relation(empresa_x, "emplea_a", maria)
        .await
        .unwrap();
    graph
        .add_relation(alpha, "participante", juan)
        .await
        .unwrap();
    graph
        .add_relation(alpha, "participante", maria)
        .await
        .unwrap();

    // Explore from Juan with max_depth=2 — should reach Empresa X, Alpha,
    // and through them María.
    let params_depth2 = ExploreParams {
        max_depth: 2,
        max_nodes: 50,
        relevance_threshold: 0.0,
        ..Default::default()
    };

    let nodes = graph.explore(juan, params_depth2).await.unwrap();
    let names: Vec<&str> = nodes.iter().map(|n| n.entity.name.as_str()).collect();

    assert!(names.contains(&"Juan"), "Entry node Juan must be present");
    assert!(
        names.contains(&"Empresa X"),
        "Direct neighbor Empresa X must be present"
    );
    assert!(
        names.contains(&"Alpha"),
        "Direct neighbor Alpha must be present"
    );
    assert!(
        names.contains(&"María"),
        "María should be reachable at depth 2 through shared org/project"
    );

    // Verify depth values
    let juan_node = nodes.iter().find(|n| n.entity.name == "Juan").unwrap();
    assert_eq!(juan_node.depth, 0, "Juan is the entry node at depth 0");

    let empresa_node = nodes.iter().find(|n| n.entity.name == "Empresa X").unwrap();
    assert_eq!(empresa_node.depth, 1, "Empresa X is at depth 1");

    let alpha_node = nodes.iter().find(|n| n.entity.name == "Alpha").unwrap();
    assert_eq!(alpha_node.depth, 1, "Alpha is at depth 1");

    let maria_node = nodes.iter().find(|n| n.entity.name == "María").unwrap();
    assert_eq!(maria_node.depth, 2, "María is at depth 2");

    // Explore from Juan with max_depth=1 — should find Empresa X and Alpha
    // but NOT María.
    let params_depth1 = ExploreParams {
        max_depth: 1,
        max_nodes: 50,
        relevance_threshold: 0.0,
        ..Default::default()
    };

    let nodes_shallow = graph.explore(juan, params_depth1).await.unwrap();
    let shallow_names: Vec<&str> = nodes_shallow
        .iter()
        .map(|n| n.entity.name.as_str())
        .collect();

    assert!(shallow_names.contains(&"Juan"));
    assert!(shallow_names.contains(&"Empresa X"));
    assert!(shallow_names.contains(&"Alpha"));
    assert!(
        !shallow_names.contains(&"María"),
        "María should NOT be reachable at max_depth=1"
    );
}

// ---------------------------------------------------------------------------
// Test 2: Vector search finds semantically similar entries
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_vector_search_finds_semantically_similar() {
    let conn = in_memory_conn();
    let index = SqliteVectorIndex::new(conn).unwrap();

    // Insert 5 entries with known embeddings
    index
        .upsert(
            "meeting_notes",
            vec![1.0, 0.0, 0.0, 0.0],
            serde_json::json!({"label": "meeting notes"}),
        )
        .await
        .unwrap();

    index
        .upsert(
            "calendar_event",
            vec![0.9, 0.1, 0.0, 0.0],
            serde_json::json!({"label": "calendar event"}),
        )
        .await
        .unwrap();

    index
        .upsert(
            "python_code",
            vec![0.0, 0.0, 1.0, 0.0],
            serde_json::json!({"label": "python code"}),
        )
        .await
        .unwrap();

    index
        .upsert(
            "rust_code",
            vec![0.0, 0.0, 0.9, 0.1],
            serde_json::json!({"label": "rust code"}),
        )
        .await
        .unwrap();

    index
        .upsert(
            "email_draft",
            vec![0.5, 0.5, 0.0, 0.0],
            serde_json::json!({"label": "email draft"}),
        )
        .await
        .unwrap();

    // Search with query embedding matching "meeting notes" exactly
    let results = index.search(vec![1.0, 0.0, 0.0, 0.0], 3).await.unwrap();

    assert_eq!(results.len(), 3, "Should return top 3 results");

    // "meeting notes" should be first (exact match, cosine similarity = 1.0)
    assert_eq!(
        results[0].id, "meeting_notes",
        "Exact match should be first"
    );
    assert!(
        (results[0].score - 1.0).abs() < 1e-5,
        "Score for exact match should be ~1.0"
    );

    // "calendar event" should be second (very similar direction)
    assert_eq!(
        results[1].id, "calendar_event",
        "calendar_event should be second"
    );

    // "email draft" should be third (somewhat similar)
    assert_eq!(results[2].id, "email_draft", "email_draft should be third");

    // Verify that python_code and rust_code are NOT in the top 3
    let top3_ids: Vec<&str> = results.iter().map(|r| r.id.as_str()).collect();
    assert!(
        !top3_ids.contains(&"python_code"),
        "python_code should not be in top 3"
    );
    assert!(
        !top3_ids.contains(&"rust_code"),
        "rust_code should not be in top 3"
    );

    // Scores should be monotonically descending
    assert!(results[0].score >= results[1].score);
    assert!(results[1].score >= results[2].score);
}

// ---------------------------------------------------------------------------
// Test 3: Memory facade remember and recall
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_memory_facade_remember_and_recall() {
    let conn = in_memory_conn();
    let vector = SqliteVectorIndex::new(Arc::clone(&conn)).unwrap();
    let graph = SqliteGraph::new(conn).unwrap();

    let mem = Memory::new(Box::new(vector), Box::new(graph))
        .with_embedder(Box::new(KeywordEmbedding::new()))
        .with_min_score(0.0);

    // Remember three items
    mem.remember(MemoryItem {
        id: "item-meeting".to_string(),
        content: "Meeting with Juan about Project Alpha on January 15".to_string(),
        metadata: serde_json::json!({
            "source": "calendar",
            "entities": [
                {"name": "Juan", "type": "Person"},
                {"name": "Project Alpha", "type": "Project"}
            ]
        }),
    })
    .await
    .unwrap();

    mem.remember(MemoryItem {
        id: "item-email".to_string(),
        content: "Email from María about budget concerns".to_string(),
        metadata: serde_json::json!({
            "source": "email",
            "entities": [
                {"name": "María", "type": "Person"}
            ]
        }),
    })
    .await
    .unwrap();

    mem.remember(MemoryItem {
        id: "item-code".to_string(),
        content: "Code review for the authentication module".to_string(),
        metadata: serde_json::json!({
            "source": "dev",
        }),
    })
    .await
    .unwrap();

    // With real keyword embeddings, "Juan" should rank the meeting item highest
    // (it contains "Juan" in its content).
    let results = mem.recall("Juan", 5).await.unwrap();
    assert_eq!(results.len(), 3, "All 3 items should be recalled");
    assert_eq!(
        results[0].id, "item-meeting",
        "Meeting item (mentions Juan) should rank first for 'Juan' query"
    );

    // Recall for "budget" — the email item should rank first.
    let budget_results = mem.recall("budget", 5).await.unwrap();
    assert_eq!(
        budget_results[0].id, "item-email",
        "Email item (about budget) should rank first for 'budget' query"
    );

    // Verify content was reconstructed from stored metadata.
    assert!(
        budget_results[0].content.contains("budget"),
        "Content should be the original stored content"
    );

    // Forget the meeting item
    mem.forget("item-meeting").await.unwrap();

    // Recall again — meeting item should be gone
    let after_forget = mem.recall("Juan", 5).await.unwrap();
    let after_ids: Vec<&str> = after_forget.iter().map(|r| r.id.as_str()).collect();
    assert!(
        !after_ids.contains(&"item-meeting"),
        "Meeting item should be forgotten"
    );
    assert_eq!(after_forget.len(), 2, "Only 2 items should remain");
}

// ---------------------------------------------------------------------------
// Test 4: SQLite persistence across connections (file-backed)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_sqlite_persistence_across_connections() {
    let tmp_dir = tempfile::tempdir().unwrap();
    let db_path = tmp_dir.path().join("test_memory.db");
    let db_path_str = db_path.to_str().unwrap();

    // Scope 1: create graph and vector index, add data, then drop them
    let (juan_id, _empresa_id) = {
        let conn = Arc::new(Mutex::new(Connection::open(db_path_str).unwrap()));
        let graph = SqliteGraph::new(Arc::clone(&conn)).unwrap();
        let vector = SqliteVectorIndex::new(Arc::clone(&conn)).unwrap();

        let juan = graph
            .add_entity(entity("Juan", EntityType::Person))
            .await
            .unwrap();
        let empresa = graph
            .add_entity(entity("Empresa X", EntityType::Organization))
            .await
            .unwrap();
        graph
            .add_relation(juan, "trabaja_en", empresa)
            .await
            .unwrap();

        vector
            .upsert(
                "doc-1",
                vec![1.0, 0.0, 0.0],
                serde_json::json!({"title": "persistent doc"}),
            )
            .await
            .unwrap();

        (juan, empresa)
    };
    // graph, vector, and conn are all dropped here — simulates app closing

    // Scope 2: reopen from the same file
    {
        let conn2 = Arc::new(Mutex::new(Connection::open(db_path_str).unwrap()));
        let graph2 = SqliteGraph::new(Arc::clone(&conn2)).unwrap();
        let vector2 = SqliteVectorIndex::new(Arc::clone(&conn2)).unwrap();

        // Graph data should still be there
        let params = ExploreParams {
            max_depth: 1,
            max_nodes: 50,
            relevance_threshold: 0.0,
            ..Default::default()
        };

        let nodes = graph2.explore(juan_id, params).await.unwrap();
        assert!(
            !nodes.is_empty(),
            "Graph data should persist across connections"
        );

        let names: Vec<&str> = nodes.iter().map(|n| n.entity.name.as_str()).collect();
        assert!(names.contains(&"Juan"), "Juan should persist");
        assert!(
            names.contains(&"Empresa X"),
            "Empresa X should persist as a neighbor"
        );

        // Vector data should still be there
        let search_results = vector2.search(vec![1.0, 0.0, 0.0], 5).await.unwrap();
        assert_eq!(search_results.len(), 1, "Vector entry should persist");
        assert_eq!(search_results[0].id, "doc-1");
        assert_eq!(search_results[0].metadata["title"], "persistent doc");
    }

    // Cleanup happens automatically when tmp_dir is dropped
}

// ---------------------------------------------------------------------------
// Test 5: Graph explore respects params (max_nodes, relevance_threshold, max_depth)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_graph_explore_respects_params() {
    let conn = in_memory_conn();
    let graph = SqliteGraph::new(conn).unwrap();

    // Build a larger graph: a hub entity connected to 12 leaf entities,
    // plus some second-level connections.
    let hub = graph
        .add_entity(entity("Hub", EntityType::Organization))
        .await
        .unwrap();

    let mut leaf_ids = Vec::new();
    for i in 0..12 {
        let leaf = graph
            .add_entity(entity(&format!("Leaf-{}", i), EntityType::Person))
            .await
            .unwrap();
        graph
            .add_relation(hub, &format!("connected_{}", i), leaf)
            .await
            .unwrap();
        leaf_ids.push(leaf);
    }

    // Add second-level connections: each leaf connects to a "deep" node
    for (i, leaf_id) in leaf_ids.iter().enumerate() {
        let deep = graph
            .add_entity(entity(&format!("Deep-{}", i), EntityType::Concept))
            .await
            .unwrap();
        graph
            .add_relation(*leaf_id, "links_to", deep)
            .await
            .unwrap();
    }

    // Additional cross-links between leaves for density (15+ total relations)
    graph
        .add_relation(leaf_ids[0], "collaborates", leaf_ids[1])
        .await
        .unwrap();
    graph
        .add_relation(leaf_ids[2], "collaborates", leaf_ids[3])
        .await
        .unwrap();
    graph
        .add_relation(leaf_ids[4], "collaborates", leaf_ids[5])
        .await
        .unwrap();

    // --- Test max_nodes=3 ---
    let params_max3 = ExploreParams {
        max_depth: 10,
        max_nodes: 3,
        relevance_threshold: 0.0,
        ..Default::default()
    };

    let nodes = graph.explore(hub, params_max3).await.unwrap();
    assert!(
        nodes.len() <= 3,
        "Explore with max_nodes=3 should return at most 3 nodes, got {}",
        nodes.len()
    );

    // --- Test max_depth=1 ---
    let params_depth1 = ExploreParams {
        max_depth: 1,
        max_nodes: 100,
        relevance_threshold: 0.0,
        ..Default::default()
    };

    let nodes_shallow = graph.explore(hub, params_depth1).await.unwrap();
    for node in &nodes_shallow {
        assert!(
            node.depth <= 1,
            "All nodes should be at depth 0 or 1, found depth {}",
            node.depth
        );
    }
    // Should have hub + 12 leaves = 13 nodes (no Deep nodes)
    let has_deep = nodes_shallow
        .iter()
        .any(|n| n.entity.name.starts_with("Deep"));
    assert!(!has_deep, "Deep nodes should NOT appear with max_depth=1");

    // --- Test relevance_threshold=0.9 ---
    // With default weights (recency=0.4, frequency=0.2, importance=0.3) and
    // weight=1.0, the edge score for a fresh edge is approximately:
    //   0.4 * ~1.0 + 0.2 * 1.0 + 0.3 * 1.0 = ~0.9
    // Using a threshold of 0.95 should filter edges whose recency has
    // decayed even slightly. The entry node (depth 0) always passes, but
    // enqueuing neighbors requires edge.weight >= threshold.
    let params_high_threshold = ExploreParams {
        max_depth: 5,
        max_nodes: 100,
        relevance_threshold: 0.95,
        ..Default::default()
    };

    let nodes_strict = graph.explore(hub, params_high_threshold).await.unwrap();
    // The hub itself is always included (depth 0 bypass). Neighbors are only
    // enqueued when edge.weight >= relevance_threshold. Since all edges have
    // weight=1.0 and threshold is 0.95, neighbors ARE enqueued. But at depth 1
    // the score check (non-depth-0) filters edges where score < threshold,
    // which may limit further expansion.
    // At minimum, the hub must be present.
    assert!(
        !nodes_strict.is_empty(),
        "At least the hub node should be returned"
    );
    assert_eq!(
        nodes_strict[0].entity.name, "Hub",
        "First node should be the entry point"
    );
}
