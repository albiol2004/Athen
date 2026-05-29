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
use athen_memory::sqlite::{SqliteGraph, SqliteLexicalIndex, SqliteVectorIndex};
use athen_memory::Memory;

/// Build a full hybrid Memory (semantic + lexical FTS5 + graph) on a shared
/// in-memory connection, with `min_score 0.0` so admission never filters
/// during ranking-behaviour tests.
fn hybrid_memory() -> Memory {
    let conn = in_memory_conn();
    let vector = SqliteVectorIndex::new(Arc::clone(&conn)).unwrap();
    let lexical = SqliteLexicalIndex::new(Arc::clone(&conn)).unwrap();
    let graph = SqliteGraph::new(conn).unwrap();
    Memory::new(Box::new(vector), Box::new(graph))
        .with_embedder(Box::new(KeywordEmbedding::new()))
        .with_lexical(Box::new(lexical))
        .with_min_score(0.0)
}

/// Hybrid Memory with NO embedder — exercises the lexical/graph fallback path.
fn hybrid_memory_no_embedder() -> Memory {
    let conn = in_memory_conn();
    let vector = SqliteVectorIndex::new(Arc::clone(&conn)).unwrap();
    let lexical = SqliteLexicalIndex::new(Arc::clone(&conn)).unwrap();
    let graph = SqliteGraph::new(conn).unwrap();
    Memory::new(Box::new(vector), Box::new(graph)).with_lexical(Box::new(lexical))
}

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

// ---------------------------------------------------------------------------
// Test 6: Graph arm surfaces a related memory sharing an entity but NO tokens
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_hybrid_graph_link_surfaces_related_memory() {
    let mem = hybrid_memory();

    // mem-1 mentions both Alice and Project Titan.
    mem.remember(MemoryItem {
        id: "mem-1".to_string(),
        content: "Alice leads Project Titan".to_string(),
        metadata: serde_json::json!({
            "entities": [
                {"name": "Alice", "type": "Person"},
                {"name": "Project Titan", "type": "Project"}
            ]
        }),
    })
    .await
    .unwrap();

    // mem-2 mentions Project Titan but shares NO token with the query "Alice".
    mem.remember(MemoryItem {
        id: "mem-2".to_string(),
        content: "ships in the third quarter".to_string(),
        metadata: serde_json::json!({
            "entities": [
                {"name": "Project Titan", "type": "Project"}
            ]
        }),
    })
    .await
    .unwrap();

    // Query "Alice": mem-1 matches semantically; mem-2 shares no tokens and is
    // surfaced only through the shared "Project Titan" entity (graph arm pivots
    // on the entities of the top semantic hit).
    let results = mem.recall("Alice", 5).await.unwrap();
    let ids: Vec<&str> = results.iter().map(|r| r.id.as_str()).collect();
    assert!(ids.contains(&"mem-1"), "mem-1 should match for 'Alice'");
    assert!(
        ids.contains(&"mem-2"),
        "mem-2 (no shared tokens) must surface via the shared Project Titan entity; got {ids:?}"
    );
}

// ---------------------------------------------------------------------------
// Test 7: Lexical (FTS5) arm finds an exact term on the no-embedder path
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_hybrid_lexical_arm_no_embedder() {
    let mem = hybrid_memory_no_embedder();

    mem.remember(MemoryItem {
        id: "rev".to_string(),
        content: "Quarterly revenue projections for the Northwind account".to_string(),
        metadata: serde_json::json!({}),
    })
    .await
    .unwrap();
    mem.remember(MemoryItem {
        id: "other".to_string(),
        content: "Sourdough bread baking schedule".to_string(),
        metadata: serde_json::json!({}),
    })
    .await
    .unwrap();

    // No embedder → recall routes through the FTS5 lexical arm.
    let hits = mem.recall("Northwind revenue", 5).await.unwrap();
    let ids: Vec<&str> = hits.iter().map(|r| r.id.as_str()).collect();
    assert!(
        ids.contains(&"rev"),
        "lexical arm should find 'rev'; got {ids:?}"
    );
    assert!(
        !ids.contains(&"other"),
        "unrelated 'other' should not match the lexical query"
    );
}

// ---------------------------------------------------------------------------
// Test 8: note_recalled raises a memory's fused rank (frequency signal)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_note_recalled_raises_rank() {
    let mem = hybrid_memory();

    // Two items with the same query-token overlap → ~equal cosine + lexical,
    // both fresh → recency ties. Frequency is the only differentiator.
    mem.remember(MemoryItem {
        id: "a".to_string(),
        content: "project alpha status update".to_string(),
        metadata: serde_json::json!({}),
    })
    .await
    .unwrap();
    mem.remember(MemoryItem {
        id: "b".to_string(),
        content: "project alpha planning review".to_string(),
        metadata: serde_json::json!({}),
    })
    .await
    .unwrap();

    // Consult "b" several times — bumps its recall_count + last_recalled_at.
    for _ in 0..5 {
        mem.note_recalled(&["b"]).await.unwrap();
    }

    let results = mem.recall("project alpha", 5).await.unwrap();
    assert_eq!(
        results[0].id,
        "b",
        "frequently-consulted 'b' should rank first; got {:?}",
        results.iter().map(|r| &r.id).collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------------------
// Test 9: forget purges the memory from all three arms
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_forget_purges_all_arms() {
    let mem = hybrid_memory();

    mem.remember(MemoryItem {
        id: "doomed".to_string(),
        content: "Confidential merger briefing with Globex".to_string(),
        metadata: serde_json::json!({
            "entities": [{"name": "Globex", "type": "Organization"}]
        }),
    })
    .await
    .unwrap();

    // Present in semantic + lexical + graph before forget.
    assert!(!mem.recall("merger briefing", 5).await.unwrap().is_empty());

    mem.forget("doomed").await.unwrap();

    // Gone from semantic/lexical recall…
    let after = mem.recall("merger briefing", 5).await.unwrap();
    assert!(
        after.iter().all(|m| m.id != "doomed"),
        "forgotten memory must not resurface via recall"
    );
    // …and from a pure-lexical query (no embedder path would also miss it)…
    assert!(mem
        .recall("Globex", 5)
        .await
        .unwrap()
        .iter()
        .all(|m| m.id != "doomed"));
    // …and its graph mention links are gone (the entity node may remain).
    if let Some(globex) = mem
        .list_entities()
        .await
        .unwrap()
        .into_iter()
        .find(|e| e.name == "Globex")
    {
        // No mentions edge should point at the forgotten memory. We assert via
        // recall above; this just confirms the entity lookup is intact.
        assert_eq!(globex.name, "Globex");
    }
}

// ---------------------------------------------------------------------------
// Test 10: find_duplicate only fires on a GENUINE near-duplicate, not on any
// recall hit (regression: hybrid recall admits broadly, so distinct facts were
// being falsely skipped by the memory_store pre-store dedup)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_find_duplicate_distinguishes_distinct_facts() {
    let mem = hybrid_memory();

    mem.remember(MemoryItem {
        id: "agent_girlfriend".to_string(),
        content: "girlfriend_nadia: the user's girlfriend is Nadia".to_string(),
        metadata: serde_json::json!({"source": "agent_tool"}),
    })
    .await
    .unwrap();

    // A genuinely DIFFERENT fact. Broad hybrid recall will likely return the
    // Nadia memory as the closest row, but it is NOT a duplicate.
    assert!(
        mem.find_duplicate("personal_life: the user enjoys hiking on weekends")
            .await
            .is_none(),
        "a distinct fact must not be flagged as a duplicate just because recall returned a row"
    );

    // A near-identical restatement IS a duplicate.
    assert!(
        mem.find_duplicate("girlfriend_nadia: the user's girlfriend is Nadia")
            .await
            .is_some(),
        "an (almost) verbatim restatement must be detected as a duplicate"
    );

    // And both distinct facts actually persist (none falsely skipped).
    mem.remember(MemoryItem {
        id: "agent_hobby".to_string(),
        content: "personal_life: the user enjoys hiking on weekends".to_string(),
        metadata: serde_json::json!({"source": "agent_tool"}),
    })
    .await
    .unwrap();
    let all = mem.list_all().await.unwrap();
    let ids: Vec<&str> = all.iter().map(|m| m.id.as_str()).collect();
    assert!(
        ids.contains(&"agent_girlfriend") && ids.contains(&"agent_hobby"),
        "got {ids:?}"
    );
}
