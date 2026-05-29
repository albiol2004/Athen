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
use chrono::{DateTime, Utc};
use tracing::{debug, warn};

use athen_core::error::Result;
use athen_core::traits::embedding::EmbeddingProvider;
use athen_core::traits::memory::{
    Entity, EntityExtractor, EntityId, EntityType, ExploreParams, FusionWeights, KnowledgeGraph,
    LexicalIndex, MemoryItem, MemoryStore, RankedRow, VectorIndex,
};

/// Unified memory facade combining semantic, lexical, and graph retrieval.
pub struct Memory {
    vector: Box<dyn VectorIndex>,
    graph: Box<dyn KnowledgeGraph>,
    /// Lexical (BM25/FTS5) arm. `None` falls back to substring keyword search.
    lexical: Option<Box<dyn LexicalIndex>>,
    embedder: Option<Box<dyn EmbeddingProvider>>,
    extractor: Option<Box<dyn EntityExtractor>>,
    /// Weights + thresholds for the hybrid-recall fusion ranker.
    fusion: FusionWeights,
}

impl Memory {
    pub fn new(vector: Box<dyn VectorIndex>, graph: Box<dyn KnowledgeGraph>) -> Self {
        Self {
            vector,
            graph,
            lexical: None,
            embedder: None,
            extractor: None,
            fusion: FusionWeights::default(),
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

    /// Attach the lexical (BM25/FTS5) retrieval arm.
    pub fn with_lexical(mut self, lexical: Box<dyn LexicalIndex>) -> Self {
        self.lexical = Some(lexical);
        self
    }

    /// Override the full set of fusion weights/thresholds.
    pub fn with_fusion(mut self, fusion: FusionWeights) -> Self {
        self.fusion = fusion;
        self
    }

    /// Back-compat shim: sets the semantic admission floor (`cosine_floor`).
    /// A candidate is admitted to ranking if its cosine clears this floor, OR it
    /// has a lexical/graph hit — so raising this forces the vector path to miss
    /// without suppressing graph/lexical-surfaced memories (those are gated by
    /// the low `min_final`, not by `cosine_floor`). Prefer `with_fusion` for
    /// full control.
    pub fn with_min_score(mut self, score: f32) -> Self {
        self.fusion.cosine_floor = score;
        self
    }
}

/// Exponential recency decay in `[0,1]`: 1.0 now, 0.5 at 30 days, →0 beyond.
/// `ts` is the last consult (or, absent that, the creation) time.
fn recency_decay(now: DateTime<Utc>, ts: DateTime<Utc>) -> f32 {
    let age_secs = (now - ts).num_seconds().max(0) as f64;
    let half_life_secs = 30.0 * 24.0 * 3600.0;
    (-age_secs * 2.0f64.ln() / half_life_secs).exp() as f32
}

/// Saturating frequency signal in `[0,1)`: 0 consults → 0, 3 → 0.5, 9 → 0.75.
fn freq_sat(recall_count: u32) -> f32 {
    let c = recall_count as f32;
    c / (c + 3.0)
}

/// Build a `MemoryItem` from a stored row's metadata (content lives under
/// `_content`). Never substitutes the query — absent content stays empty.
fn item_from_metadata(id: String, metadata: &serde_json::Value) -> MemoryItem {
    let content = metadata
        .get("_content")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    MemoryItem {
        id,
        content,
        metadata: metadata.clone(),
    }
}

/// Normalize text for duplicate comparison: trim, lowercase, strip
/// trailing ASCII punctuation. Conservative — we only want to collapse
/// trivial differences (case, trailing period) before falling through
/// to the more permissive Jaccard check.
fn normalize_for_dedup(s: &str) -> String {
    s.trim()
        .trim_end_matches(|c: char| c.is_ascii_punctuation())
        .to_lowercase()
}

/// Whitespace-tokenize + lowercase + strip surrounding punctuation per
/// token. Used by `jaccard_similarity`. Empty tokens are dropped.
fn dedup_tokens(s: &str) -> std::collections::HashSet<String> {
    s.split_whitespace()
        .map(|t| {
            t.trim_matches(|c: char| !c.is_alphanumeric())
                .to_lowercase()
        })
        .filter(|t| !t.is_empty())
        .collect()
}

/// Jaccard similarity over whitespace-split tokens. Returns 0.0 when
/// either side is empty so very short strings can't accidentally match.
fn jaccard_similarity(a: &str, b: &str) -> f32 {
    let ta = dedup_tokens(a);
    let tb = dedup_tokens(b);
    if ta.is_empty() || tb.is_empty() {
        return 0.0;
    }
    let intersection = ta.intersection(&tb).count() as f32;
    let union = ta.union(&tb).count() as f32;
    if union == 0.0 {
        0.0
    } else {
        intersection / union
    }
}

/// Tokenize a recall query into lowercased keyword tokens. Strips
/// trivial punctuation and drops common English/Spanish stopwords plus
/// tokens shorter than 3 chars. Used by the keyword-fallback path that
/// runs when no embedder is configured.
fn tokenize_query(query: &str) -> Vec<String> {
    const STOPWORDS: &[&str] = &[
        "the", "and", "for", "with", "that", "this", "from", "what", "when", "where", "which",
        "about", "los", "las", "del", "que", "con", "por", "para", "una", "uno",
    ];
    query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() >= 3)
        .map(|t| t.to_lowercase())
        .filter(|t| !STOPWORDS.contains(&t.as_str()))
        .collect()
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

/// Cheap entity-name extraction from a query string when no extractor
/// is wired (or it times out / fails). Picks out capitalized tokens
/// (e.g. "Alice", "Acme Corp") and drops common stopwords / pronouns
/// that often appear capitalized at sentence start. Conservative on
/// purpose — false positives just cost an extra graph lookup that
/// returns `None`.
fn extract_entity_names_cheap(query: &str) -> Vec<String> {
    // Pronouns and common interrogatives that often appear capitalized
    // at sentence start but are never real entity names. NOTE: "User"
    // is intentionally NOT in this list — it's the conventional name
    // of the owner entity in Athen's graph and a legitimate pivot.
    const PRONOUN_STOP: &[&str] = &[
        "I", "Me", "My", "Mine", "You", "Your", "Yours", "We", "Us", "Our", "He", "She", "Him",
        "Her", "His", "Hers", "It", "Its", "They", "Them", "Their", "What", "When", "Where",
        "Which", "Who", "Why", "How", "Tell", "Show", "Find", "List", "Give", "Get", "About",
    ];
    let mut out: Vec<String> = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    for raw_tok in query.split_whitespace() {
        // Strip surrounding punctuation but keep internal hyphens/apostrophes.
        let tok = raw_tok.trim_matches(|c: char| !c.is_alphanumeric());
        if tok.is_empty() {
            if !current.is_empty() {
                out.push(current.join(" "));
                current.clear();
            }
            continue;
        }
        let first = tok.chars().next().unwrap();
        if first.is_uppercase() && !PRONOUN_STOP.contains(&tok) {
            current.push(tok);
        } else {
            if !current.is_empty() {
                out.push(current.join(" "));
                current.clear();
            }
        }
    }
    if !current.is_empty() {
        out.push(current.join(" "));
    }
    out
}

#[async_trait]
impl MemoryStore for Memory {
    async fn remember(&self, item: MemoryItem) -> Result<()> {
        // Write-time dedup. The agent-tool path (`memory_store`)
        // pre-recalls and skips, but the LLM auto-judge path
        // (`judge_worth_remembering`) calls remember() directly and
        // historically piled up near-duplicate entries (e.g. three
        // copies of "pet in August"). Catch duplicates here so EVERY
        // caller benefits.
        //
        // Skip condition: incoming item has no pre-existing ID (i.e.
        // not an explicit update), AND a recall surfaces something
        // whose content is text-equal (case + trailing-punct normalized)
        // OR Jaccard > 0.85 over whitespace tokens. Explicit-id
        // overwrites fall through to vector.upsert which handles them.
        let id_already_exists = {
            // We treat "id already in the store" as an explicit update
            // and skip dedup. list_all is the cheapest extant API.
            let all = self.vector.list_all().await.unwrap_or_default();
            all.iter().any(|r| r.id == item.id)
        };

        if !id_already_exists {
            if let Some(dup) = self.find_duplicate(&item.content).await {
                debug!(
                    "Skipping near-duplicate memory: existing={:?} new={:?}",
                    dup.content, item.content
                );
                return Ok(());
            }
        }

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
                        if let (Some(&from_id), Some(&to_id)) = (
                            name_to_id.get(from_name.as_str()),
                            name_to_id.get(to_name.as_str()),
                        ) {
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

        // Link this memory to the entities it mentions (the memory↔entity edge
        // the graph arm of recall walks), and index its text in the lexical arm.
        let mention_ids: Vec<EntityId> = entity_ids.iter().map(|(_, id)| *id).collect();
        if !mention_ids.is_empty() {
            if let Err(e) = self.graph.link_memory(&item.id, &mention_ids).await {
                warn!("failed to link memory {} to entities: {e}", item.id);
            }
        }
        if let Some(ref lexical) = self.lexical {
            if let Err(e) = lexical.upsert(&item.id, &item.content).await {
                warn!("failed to index memory {} in lexical arm: {e}", item.id);
            }
        }

        Ok(())
    }

    async fn recall(&self, query: &str, limit: usize) -> Result<Vec<MemoryItem>> {
        // Embed the query. No embedder (or a transient embed failure) drops to
        // the lexical/graph path — never `found:false` when a name lookup
        // should just work.
        let query_embedding = match self.embedder.as_ref() {
            Some(embedder) => match embedder.embed(query).await {
                Ok(emb) => emb,
                Err(e) => {
                    warn!("embedder failed during recall ({e}); using lexical/graph fallback");
                    return self.recall_no_embedder(query, limit).await;
                }
            },
            None => return self.recall_no_embedder(query, limit).await,
        };

        // Arm 1 — semantic: cosine over every row, plus the ranking-signal
        // columns. The brute-force scan already computes cosine for all rows,
        // so emitting them all (instead of an early top-k truncation) is free
        // and lets the lexical/graph arms reuse the cached metadata + signals.
        let rows = self.vector.scan_scored(query_embedding).await?;
        let mut by_id: HashMap<String, RankedRow> = HashMap::with_capacity(rows.len());
        for r in rows {
            by_id.insert(r.id.clone(), r);
        }

        // Arm 2 — lexical (BM25/FTS5).
        let mut lex: HashMap<String, f32> = HashMap::new();
        if let Some(ref lexical) = self.lexical {
            let k = (limit * 4).max(20);
            if let Ok(hits) = lexical.search(query, k).await {
                for (id, score) in hits {
                    lex.insert(id, score);
                }
            }
        }

        // Arm 3 — graph: pivot on query entities + entities named in the
        // strongest semantic hits, walk one hop, then join entities → memories
        // through the indexed `mentions` table.
        let graph = self.graph_arm(query, &by_id, limit).await;

        // Fuse over the union of admitted candidates.
        let w = self.fusion;
        let now = Utc::now();
        let mut candidates: HashSet<String> = HashSet::new();
        for (id, r) in &by_id {
            if r.cosine >= w.cosine_floor {
                candidates.insert(id.clone());
            }
        }
        candidates.extend(lex.keys().cloned());
        candidates.extend(graph.keys().cloned());

        let mut scored: Vec<(String, f32)> = Vec::with_capacity(candidates.len());
        for id in candidates {
            let row = by_id.get(&id);
            let cosine = row.map(|r| r.cosine.max(0.0)).unwrap_or(0.0);
            let lexical_score = lex.get(&id).copied().unwrap_or(0.0);
            let graph_score = graph.get(&id).copied().unwrap_or(0.0);
            let recency = row
                .and_then(|r| r.last_recalled_at.or(r.created_at))
                .map(|ts| recency_decay(now, ts))
                .unwrap_or(0.0);
            let frequency = row.map(|r| freq_sat(r.recall_count)).unwrap_or(0.0);
            let final_score = w.w_sem * cosine
                + w.w_lex * lexical_score
                + w.w_graph * graph_score
                + w.w_recency * recency
                + w.w_freq * frequency;
            if final_score >= w.min_final {
                scored.push((id, final_score));
            }
        }
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(limit);

        // Hydrate from the cached metadata — every candidate id came from
        // `by_id` (which holds all rows), so no empty-metadata fallback.
        Ok(scored
            .into_iter()
            .filter_map(|(id, _)| by_id.get(&id).map(|r| item_from_metadata(id, &r.metadata)))
            .collect())
    }

    async fn forget(&self, id: &str) -> Result<()> {
        self.vector.delete(id).await?;
        if let Some(ref lexical) = self.lexical {
            if let Err(e) = lexical.delete(id).await {
                warn!("failed to remove memory {id} from lexical index: {e}");
            }
        }
        // Drop the memory↔entity links. Entity nodes themselves stay (they may
        // be referenced by other memories — graph GC is a separate concern).
        if let Err(e) = self.graph.unlink_memory(id).await {
            warn!("failed to unlink memory {id} from graph: {e}");
        }
        Ok(())
    }

    async fn note_recalled(&self, ids: &[&str]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        // Bump per-memory consult signals (recency + frequency).
        self.vector.bump_recall_stats(ids).await?;

        // Reinforce the entities those memories mention, so a frequently
        // consulted relation stays strong (and otherwise decays). Bounded:
        // `ids` is at most the recall limit (~8). Entity names come from the
        // memories' stored metadata.
        let want: HashSet<&str> = ids.iter().copied().collect();
        let all = self.vector.list_all().await.unwrap_or_default();
        let mut names: HashSet<String> = HashSet::new();
        for r in all {
            if want.contains(r.id.as_str()) {
                for n in extract_entity_names_from_metadata(&r.metadata) {
                    names.insert(n);
                }
            }
        }
        for name in names {
            if let Err(e) = self.reinforce_by_name(&name, 0.1).await {
                debug!("reinforce_by_name({name}) failed during note_recalled: {e}");
            }
        }
        Ok(())
    }
}

impl Memory {
    /// Keyword-overlap fallback used when no embedder is configured.
    ///
    /// Tokenizes the query into lowercased words (>=3 chars, stripped of
    /// trivial punctuation) and scores each stored memory by how many
    /// query tokens appear as substrings in its `_content`. A handful of
    /// extremely common stopwords are dropped so single-word queries
    /// don't return everything. Returns the top `limit` items with
    /// score >= 1.
    ///
    /// This isn't semantic, but it's the *expected* behavior when the
    /// user explicitly turned embeddings off — far better than the
    /// previous "everything scores 0 → filtered out → found:false".
    async fn recall_keyword(&self, query: &str, limit: usize) -> Result<Vec<MemoryItem>> {
        let tokens = tokenize_query(query);
        if tokens.is_empty() {
            return Ok(Vec::new());
        }
        let all = self.vector.list_all().await?;
        let mut scored: Vec<(usize, MemoryItem)> = all
            .into_iter()
            .filter_map(|r| {
                let content_lc = r
                    .metadata
                    .get("_content")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_lowercase();
                if content_lc.is_empty() {
                    return None;
                }
                let score = tokens
                    .iter()
                    .filter(|t| content_lc.contains(t.as_str()))
                    .count();
                if score == 0 {
                    return None;
                }
                let content = r
                    .metadata
                    .get("_content")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                Some((
                    score,
                    MemoryItem {
                        id: r.id,
                        content,
                        metadata: r.metadata,
                    },
                ))
            })
            .collect();
        scored.sort_by_key(|b| std::cmp::Reverse(b.0));
        Ok(scored.into_iter().take(limit).map(|(_, m)| m).collect())
    }

    /// One-time migration for memories stored before the hybrid rework: they
    /// have `_entities` in their metadata but no `mentions` graph links and no
    /// lexical-index rows. Walk every stored memory, (re-)create its entity
    /// nodes (dedup by name), link them via `link_memory`, and index the
    /// content in the lexical arm. Idempotent — `link_memory` is INSERT OR
    /// IGNORE and lexical `upsert` delete-then-inserts — so it is safe to run
    /// more than once, but callers should guard it with a marker to avoid the
    /// per-boot O(memories) cost. Returns the number of memories processed.
    pub async fn backfill_hybrid(&self) -> Result<usize> {
        let all = self.vector.list_all().await?;
        let mut processed = 0usize;
        for r in &all {
            let content = r
                .metadata
                .get("_content")
                .and_then(|v| v.as_str())
                .unwrap_or("");

            // Re-create + link the entities named in the memory's metadata.
            let names = extract_entity_names_from_metadata(&r.metadata);
            if !names.is_empty() {
                let mut ids: Vec<EntityId> = Vec::with_capacity(names.len());
                for name in names {
                    let entity = Entity {
                        id: None,
                        entity_type: EntityType::Concept,
                        name,
                        metadata: serde_json::json!({}),
                    };
                    if let Ok(id) = self.graph.add_entity(entity).await {
                        ids.push(id);
                    }
                }
                if !ids.is_empty() {
                    let _ = self.graph.link_memory(&r.id, &ids).await;
                }
            }

            // Index in the lexical arm.
            if !content.is_empty() {
                if let Some(ref lexical) = self.lexical {
                    let _ = lexical.upsert(&r.id, content).await;
                }
            }
            processed += 1;
        }
        Ok(processed)
    }

    /// Return an existing memory that is a *genuine* near-duplicate of
    /// `content` — text-equal (case + trailing-punctuation normalized) OR
    /// Jaccard token overlap > 0.85 — or `None` if there is no true duplicate.
    ///
    /// This is the single source of truth for "is this a duplicate?", shared
    /// by `remember`'s write-time dedup and the `memory_store` tool's
    /// pre-store check. It deliberately re-checks similarity on the recall
    /// candidates rather than trusting that `recall` returned *anything*:
    /// hybrid recall admits at a low cosine floor (plus lexical/graph hits),
    /// so "recall returned a row" is NOT evidence of a duplicate.
    pub async fn find_duplicate(&self, content: &str) -> Option<MemoryItem> {
        let candidates = self.recall(content, 3).await.unwrap_or_default();
        let new_norm = normalize_for_dedup(content);
        for cand in candidates {
            let existing_norm = normalize_for_dedup(&cand.content);
            let is_text_equal = !new_norm.is_empty() && new_norm == existing_norm;
            let jaccard = jaccard_similarity(content, &cand.content);
            if is_text_equal || jaccard > 0.85 {
                return Some(cand);
            }
        }
        None
    }

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
        self.vector.upsert(id, embedding, metadata).await?;
        // Keep the lexical arm consistent with the new content.
        if let Some(ref lexical) = self.lexical {
            if let Err(e) = lexical.upsert(id, new_content).await {
                warn!("failed to re-index updated memory {id} in lexical arm: {e}");
            }
        }
        Ok(())
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

    /// Pull candidate entity names from the recall query. Tries the
    /// LLM extractor first under a 5s budget; if it isn't wired, times
    /// out, or errors, falls back to the cheap regex-ish path.
    /// Always returns *something* (possibly empty) — never blocks the
    /// caller.
    async fn extract_query_entity_names(&self, query: &str) -> Vec<String> {
        if let Some(ref extractor) = self.extractor {
            let fut = extractor.extract(query);
            match tokio::time::timeout(std::time::Duration::from_secs(5), fut).await {
                Ok(Ok(result)) => {
                    let mut names: Vec<String> =
                        result.entities.into_iter().map(|e| e.name).collect();
                    // Augment with cheap-path names — extractors miss
                    // single-token proper nouns sometimes.
                    for n in extract_entity_names_cheap(query) {
                        if !names.iter().any(|x| x.eq_ignore_ascii_case(&n)) {
                            names.push(n);
                        }
                    }
                    return names;
                }
                Ok(Err(e)) => {
                    debug!("query entity extraction failed ({e}); using cheap fallback");
                }
                Err(_) => {
                    debug!("query entity extraction timed out; using cheap fallback");
                }
            }
        }
        extract_entity_names_cheap(query)
    }

    /// Graph arm of hybrid recall. Pivots on entity names from the query and
    /// from the strongest semantic hits, walks one hop, then maps the pivot +
    /// neighbor entities back to the memories that mention them through the
    /// indexed `mentions` table (replacing the old full-scan string match).
    /// Returns `memory_id -> graph_strength` in `[0,1]`: pivot-mentioned
    /// memories carry full strength, one-hop-neighbor memories a discount.
    async fn graph_arm(
        &self,
        query: &str,
        by_id: &HashMap<String, RankedRow>,
        limit: usize,
    ) -> HashMap<String, f32> {
        // Candidate pivot names: query entities + entities named in the
        // *genuinely relevant* semantic hits (cosine clears the admission
        // floor). Harvesting from every top-N hit regardless of score would
        // make each memory its own pivot when the store is small, diluting the
        // graph signal (an unrelated memory must NOT self-promote — it should
        // surface only when it shares an entity with a real hit).
        let mut names = self.extract_query_entity_names(query).await;
        if !by_id.is_empty() {
            let mut relevant: Vec<&RankedRow> = by_id
                .values()
                .filter(|r| r.cosine >= self.fusion.cosine_floor)
                .collect();
            relevant.sort_by(|a, b| {
                b.cosine
                    .partial_cmp(&a.cosine)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            for r in relevant.into_iter().take(limit.max(5)) {
                for n in extract_entity_names_from_metadata(&r.metadata) {
                    if !names.iter().any(|x| x.eq_ignore_ascii_case(&n)) {
                        names.push(n);
                    }
                }
            }
        }
        if names.is_empty() {
            return HashMap::new();
        }

        // Resolve names → entity ids, exploring one hop. Pivot entities carry
        // full strength; one-hop neighbors carry a discounted strength.
        const PIVOT_STRENGTH: f32 = 1.0;
        const NEIGHBOR_STRENGTH: f32 = 0.6;
        let mut entity_strength: HashMap<EntityId, f32> = HashMap::new();
        let mut seen_names: HashSet<String> = HashSet::new();
        for name in &names {
            if !seen_names.insert(name.to_lowercase()) {
                continue;
            }
            let pivot = match self.graph.find_entity_by_name(name).await {
                Ok(Some(id)) => id,
                Ok(None) => continue,
                Err(e) => {
                    debug!("find_entity_by_name({name}) failed: {e}");
                    continue;
                }
            };
            entity_strength
                .entry(pivot)
                .and_modify(|s| *s = s.max(PIVOT_STRENGTH))
                .or_insert(PIVOT_STRENGTH);

            let params = ExploreParams {
                max_depth: 1,
                max_nodes: 8,
                relevance_threshold: 0.0,
                ..Default::default()
            };
            if let Ok(nodes) = self.graph.explore(pivot, params).await {
                for node in nodes {
                    if node.depth == 0 {
                        continue; // the pivot itself
                    }
                    if let Some(nid) = node.entity.id {
                        entity_strength
                            .entry(nid)
                            .and_modify(|s| *s = s.max(NEIGHBOR_STRENGTH))
                            .or_insert(NEIGHBOR_STRENGTH);
                    }
                }
            }
        }
        if entity_strength.is_empty() {
            return HashMap::new();
        }

        // Map entities → memories, keeping the strongest contributing entity's
        // strength per memory.
        let mut out: HashMap<String, f32> = HashMap::new();
        for (eid, strength) in &entity_strength {
            if let Ok(mem_ids) = self.graph.memories_for_entities(&[*eid]).await {
                for mid in mem_ids {
                    out.entry(mid)
                        .and_modify(|s| *s = s.max(*strength))
                        .or_insert(*strength);
                }
            }
        }
        out
    }

    /// Recall without a working embedder: lexical (FTS5) + graph arms, fused by
    /// their weights. Falls back to the legacy substring keyword scan only if
    /// neither arm produces anything (e.g. no lexical index configured).
    async fn recall_no_embedder(&self, query: &str, limit: usize) -> Result<Vec<MemoryItem>> {
        let w = self.fusion;
        let mut scores: HashMap<String, f32> = HashMap::new();
        if let Some(ref lexical) = self.lexical {
            let k = (limit * 4).max(20);
            if let Ok(hits) = lexical.search(query, k).await {
                for (id, s) in hits {
                    scores.insert(id, w.w_lex * s);
                }
            }
        }
        let empty: HashMap<String, RankedRow> = HashMap::new();
        for (id, s) in self.graph_arm(query, &empty, limit).await {
            scores
                .entry(id)
                .and_modify(|v| *v += w.w_graph * s)
                .or_insert(w.w_graph * s);
        }
        if scores.is_empty() {
            return self.recall_keyword(query, limit).await;
        }

        // Hydrate metadata for the surviving ids.
        let meta_by_id: HashMap<String, serde_json::Value> = self
            .vector
            .list_all()
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|r| (r.id, r.metadata))
            .collect();
        let mut scored: Vec<(String, f32)> = scores.into_iter().collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(limit);
        Ok(scored
            .into_iter()
            .filter_map(|(id, _)| meta_by_id.get(&id).map(|m| item_from_metadata(id, m)))
            .collect())
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

    /// Memory with NO embedder configured — exercises the keyword
    /// fallback path in `recall`.
    fn make_memory_no_embedder() -> Memory {
        Memory::new(
            Box::new(InMemoryVectorIndex::new()),
            Box::new(InMemoryGraph::new()),
        )
    }

    #[tokio::test]
    async fn recall_without_embedder_uses_keyword_fallback() {
        let mem = make_memory_no_embedder();
        for (id, content) in &[
            ("a", "Aldaran Analytics landing page audit notes"),
            ("b", "Rust workspace structure overview"),
            ("c", "Telegram approval routing details"),
        ] {
            mem.remember(MemoryItem {
                id: id.to_string(),
                content: content.to_string(),
                metadata: serde_json::json!({}),
            })
            .await
            .unwrap();
        }
        let hits = mem.recall("Aldaran landing page audit", 5).await.unwrap();
        assert!(!hits.is_empty(), "expected keyword hit for Aldaran query");
        assert_eq!(hits[0].id, "a");
    }

    /// Embedding provider that always errors on `embed`. Used to verify
    /// `recall` falls back to keyword search when the API is down /
    /// rate-limited / network-timed-out, instead of surfacing the error.
    struct FailingEmbedder;

    #[async_trait]
    impl athen_core::traits::embedding::EmbeddingProvider for FailingEmbedder {
        fn provider_id(&self) -> &str {
            "failing"
        }
        fn dimensions(&self) -> usize {
            8
        }
        async fn embed(&self, _text: &str) -> Result<Vec<f32>> {
            Err(athen_core::error::AthenError::Other(
                "simulated embedder API failure".to_string(),
            ))
        }
        async fn is_available(&self) -> bool {
            false
        }
    }

    #[tokio::test]
    async fn recall_falls_back_to_keyword_when_embedder_errors() {
        // Stash entries with a working embedder so they have non-empty
        // embeddings, then swap to a failing one for the recall to
        // trigger the fallback path. We do this by remembering with the
        // keyword embedder, then constructing a fresh Memory that
        // shares the same vector store via list_all-style content.
        //
        // Simpler: build Memory with the failing embedder from the
        // start. `remember` will hit the failing embed too, so we use
        // the trait's default behavior (empty vec on error not allowed
        // here) — instead, store entries via the no-embedder Memory and
        // then assert keyword fallback works whenever embed errors. We
        // approximate that by wiring a Memory with FailingEmbedder and
        // pre-populating through the vector index directly.
        use crate::vector::InMemoryVectorIndex;
        let vector = InMemoryVectorIndex::new();
        // Pre-populate via the underlying index so we don't need to
        // call remember (which would itself hit the failing embedder).
        vector
            .upsert(
                "a",
                vec![],
                serde_json::json!({ "_content": "Aldaran landing page audit notes" }),
            )
            .await
            .unwrap();
        let mem = Memory::new(Box::new(vector), Box::new(InMemoryGraph::new()))
            .with_embedder(Box::new(FailingEmbedder));

        let hits = mem.recall("Aldaran landing", 5).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "a");
    }

    #[tokio::test]
    async fn recall_without_embedder_returns_empty_on_no_match() {
        let mem = make_memory_no_embedder();
        mem.remember(MemoryItem {
            id: "x".into(),
            content: "completely different topic".into(),
            metadata: serde_json::json!({}),
        })
        .await
        .unwrap();
        let hits = mem.recall("zebra panda elephant", 5).await.unwrap();
        assert!(hits.is_empty());
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
            async fn add_entity(
                &self,
                entity: Entity,
            ) -> Result<athen_core::traits::memory::EntityId> {
                self.0.add_entity(entity).await
            }
            async fn add_relation(
                &self,
                from: athen_core::traits::memory::EntityId,
                relation: &str,
                to: athen_core::traits::memory::EntityId,
            ) -> Result<()> {
                self.0.add_relation(from, relation, to).await
            }
            async fn explore(
                &self,
                entry: athen_core::traits::memory::EntityId,
                params: athen_core::traits::memory::ExploreParams,
            ) -> Result<Vec<athen_core::traits::memory::GraphNode>> {
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

        assert!(
            result.is_ok(),
            "remember() should succeed despite extractor failure"
        );

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
            async fn add_entity(&self, entity: Entity) -> Result<EntityId> {
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
            async fn reinforce_entity(&self, entity_id: EntityId, amount: f32) -> Result<()> {
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
            assert!(
                (edges[0].strength - 0.5).abs() < 0.001,
                "Initial strength should be 0.5"
            );
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

    // --- Graph-traversal recall tests (the bug this fixes) ---

    /// Memory linked to a `User` entity surfaces on a recall whose
    /// literal tokens don't appear in the content, because the graph
    /// hop walks from the `User` pivot. Before this fix, the recall
    /// was vector-only — `things about me` wouldn't surface
    /// `Job: AI Engineer` because the embedding similarity between the
    /// query and the content was below threshold.
    #[tokio::test]
    async fn recall_surfaces_memory_via_graph_pivot() {
        let mem = Memory::new(
            Box::new(InMemoryVectorIndex::new()),
            Box::new(InMemoryGraph::new()),
        )
        .with_embedder(Box::new(KeywordEmbedding::new()))
        .with_min_score(0.5); // Force the vector path to miss the job memory.

        // Pre-seed the graph with a `User` entity so the pivot lookup
        // works. Real `remember()` calls would normally add this via
        // the metadata.entities path.
        mem.graph
            .add_entity(Entity {
                id: None,
                entity_type: EntityType::Person,
                name: "User".to_string(),
                metadata: serde_json::json!({}),
            })
            .await
            .unwrap();

        // Store a memory linked to `User` via metadata.entities. Its
        // literal content has no overlap with "things about me", so
        // a keyword/embedding match would miss it.
        mem.remember(MemoryItem {
            id: "job".to_string(),
            content: "Job: AI Engineer".to_string(),
            metadata: serde_json::json!({
                "entities": [
                    {"name": "User", "type": "Person"}
                ]
            }),
        })
        .await
        .unwrap();

        // Query mentions "User" so the cheap extractor picks it up
        // as a pivot. Embedding similarity to "Job: AI Engineer" is
        // low (well below min_score 0.5), so the vector path filters
        // it out. The KG hop should put it back.
        let hits = mem.recall("tell me about User", 5).await.unwrap();
        assert!(
            hits.iter().any(|m| m.id == "job"),
            "expected 'job' memory to surface via KG pivot; got {:?}",
            hits.iter().map(|m| &m.id).collect::<Vec<_>>()
        );
    }

    /// Recall on a query with no entity matches and no graph
    /// connections behaves exactly as the pre-fix vector-only path.
    /// This is the regression guard.
    #[tokio::test]
    async fn recall_with_no_entities_matches_vector_only_path() {
        let mem = make_memory().with_min_score(0.0);

        // No entities anywhere — pure-content memories.
        for (id, content) in &[
            ("a", "Rust programming language tutorial"),
            ("b", "Python data science notes"),
            ("c", "JavaScript web development guide"),
        ] {
            mem.remember(MemoryItem {
                id: id.to_string(),
                content: content.to_string(),
                metadata: serde_json::json!({}),
            })
            .await
            .unwrap();
        }

        // Query with no capitalized tokens → cheap extractor returns
        // empty → merge_graph_hops short-circuits → vector path is
        // the only thing that runs.
        let results = mem.recall("rust programming", 5).await.unwrap();
        assert!(!results.is_empty(), "vector path should still hit");
        // Rust item should win.
        assert_eq!(results[0].id, "a");
        // No extras came from the KG path (graph is empty anyway).
        assert!(results
            .iter()
            .all(|m| ["a", "b", "c"].contains(&m.id.as_str())));
    }

    /// Storing a near-identical sentence (case + trailing punctuation
    /// differences only) should be skipped silently. Cures the
    /// "3 copies of 'pet in August'" bug from the auto-judge path.
    #[tokio::test]
    async fn remember_skips_near_duplicate_text() {
        let mem = make_memory().with_min_score(0.0);

        mem.remember(MemoryItem {
            id: "first".into(),
            content: "User likes coffee".into(),
            metadata: serde_json::json!({}),
        })
        .await
        .unwrap();

        // Different ID, same content modulo case + trailing period.
        mem.remember(MemoryItem {
            id: "second".into(),
            content: "user likes coffee.".into(),
            metadata: serde_json::json!({}),
        })
        .await
        .unwrap();

        let all = mem.list_all().await.unwrap();
        assert_eq!(
            all.len(),
            1,
            "near-duplicate should be skipped; got {:?}",
            all.iter().map(|m| &m.content).collect::<Vec<_>>()
        );
    }

    /// Storing with an ID that already exists is an update, not a
    /// duplicate. The dedup check must let it through so the new
    /// content replaces the old via vector.upsert semantics.
    #[tokio::test]
    async fn remember_allows_explicit_id_update() {
        let mem = make_memory().with_min_score(0.0);

        mem.remember(MemoryItem {
            id: "stable-id".into(),
            content: "first content version".into(),
            metadata: serde_json::json!({}),
        })
        .await
        .unwrap();

        mem.remember(MemoryItem {
            id: "stable-id".into(),
            content: "second completely different content version replacement".into(),
            metadata: serde_json::json!({}),
        })
        .await
        .unwrap();

        let all = mem.list_all().await.unwrap();
        assert_eq!(all.len(), 1, "id-based update should not add a row");
        assert!(
            all[0].content.contains("second"),
            "update should replace content; got {:?}",
            all[0].content
        );
    }

    /// Genuinely different content should both land. Regression guard
    /// for over-eager dedup.
    #[tokio::test]
    async fn remember_allows_genuinely_different_content() {
        let mem = make_memory().with_min_score(0.0);

        mem.remember(MemoryItem {
            id: "coffee".into(),
            content: "User likes coffee".into(),
            metadata: serde_json::json!({}),
        })
        .await
        .unwrap();

        mem.remember(MemoryItem {
            id: "dog".into(),
            content: "User has a dog".into(),
            metadata: serde_json::json!({}),
        })
        .await
        .unwrap();

        let all = mem.list_all().await.unwrap();
        assert_eq!(
            all.len(),
            2,
            "distinct memories should both store; got {:?}",
            all.iter().map(|m| &m.content).collect::<Vec<_>>()
        );
    }

    /// With no extractor wired and no embedder, the cheap entity
    /// extraction from the query still drives a successful KG hop.
    /// Confirms the keyword-fallback path also benefits from the fix.
    #[tokio::test]
    async fn recall_extractor_disabled_still_uses_graph() {
        // No embedder → keyword path. No extractor → cheap regex path
        // for query entity names.
        let mem = make_memory_no_embedder();

        // Seed graph with an "Acme" entity by hand (no extractor to
        // do it for us during remember).
        let acme_id = mem
            .graph
            .add_entity(Entity {
                id: None,
                entity_type: EntityType::Organization,
                name: "Acme".to_string(),
                metadata: serde_json::json!({}),
            })
            .await
            .unwrap();
        let payroll_id = mem
            .graph
            .add_entity(Entity {
                id: None,
                entity_type: EntityType::Concept,
                name: "Payroll".to_string(),
                metadata: serde_json::json!({}),
            })
            .await
            .unwrap();
        mem.graph
            .add_relation(acme_id, "has", payroll_id)
            .await
            .unwrap();

        // Store via remember() so the memory↔entity link (the `mentions`
        // edge the graph arm joins through) is created. Content has zero
        // token overlap with "Acme" so the lexical/keyword path can't find
        // it — only the KG hop (Acme --has--> Payroll, doc mentions Payroll)
        // can. The `Payroll` entity from metadata dedups onto the one we
        // hand-seeded above.
        mem.remember(MemoryItem {
            id: "doc".to_string(),
            content: "Quarterly compensation breakdown for engineering".to_string(),
            metadata: serde_json::json!({
                "entities": [
                    {"name": "Payroll", "type": "Concept"}
                ]
            }),
        })
        .await
        .unwrap();

        let hits = mem.recall("info about Acme", 5).await.unwrap();
        assert!(
            hits.iter().any(|m| m.id == "doc"),
            "expected 'doc' memory to surface via cheap-extractor + KG hop on the keyword path; got {:?}",
            hits.iter().map(|m| &m.id).collect::<Vec<_>>()
        );
    }
}
