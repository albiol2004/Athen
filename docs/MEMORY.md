# Memory

Athen has two persistent stores that look superficially similar and are easy to confuse:

| Store | Purpose | Lives in | Filled by |
|---|---|---|---|
| **Identity** | Stable "who Athen is and who the user is" — personality, rules, personal facts, team | Static prompt prefix, every request | User edits + agent's `identity_add` tool |
| **Memory** (this doc) | Episodic facts auto-recalled per query — past projects, decisions, conversational context | Volatile per-turn injection (top hits, hybrid-ranked) | Agent's `memory_store` tool + post-turn auto-judge |

This doc covers Memory. For Identity see [`IDENTITY.md`](IDENTITY.md).

## Backing store — three-arm hybrid fusion

`crates/athen-memory/src/lib.rs::Memory` runs three retrieval arms and fuses them
into one ranking. Built at runtime by `state.rs::build_memory` over a single shared
SQLite connection.

1. **Semantic arm** — `VectorIndex::scan_scored` computes cosine similarity for
   *every* stored memory against the query embedding in one pass (the brute-force
   scan already touches every row, so emitting all scores is free) and returns the
   per-memory ranking-signal columns alongside it. Embedder comes from
   `build_embedding_router` (Ollama / OpenAI / Gemini, or the keyword fallback).
2. **Lexical arm** — `LexicalIndex` (`SqliteLexicalIndex`) is a real **BM25** index
   over an FTS5 virtual table (`memory_fts`). `bm25()` scores are min-max normalized
   to `[0,1]`. This catches exact-term / rare-token matches that dense embeddings miss.
3. **Graph arm** — `graph_arm` pivots on entities found in the query *and* in the
   top semantic hits, walks **one hop** via `graph.explore` (whose `edge_score`
   already weighs relation strength / recency / importance), then maps the pivot +
   neighbor entities back to the memories that mention them through the indexed
   **`mentions(memory_id, entity_id)`** table. Pivot-mentioned memories carry full
   strength; one-hop-neighbor memories a discount.

The `mentions` table is the memory↔entity edge — the load-bearing link the older
implementation lacked. Previously the graph→memory bridge was a full-table scan of
every memory's metadata matched by entity *name string*; now it is an indexed join,
and the old per-entity-name re-embedding loop (one serial `embed()` call per entity)
is gone, so recall is both more correct and faster.

### Fusion

For the union of admitted candidates, each arm contributes a signal in `[0,1]` and
the final score is their weighted sum (`FusionWeights`, `athen-core`):

```
final = w_sem*cosine + w_lex*bm25 + w_graph*graph_strength
      + w_recency*recency_decay(last_recalled_at|created_at) + w_freq*freq_sat(recall_count)
```

Defaults: `w_sem 0.45, w_lex 0.25, w_graph 0.15, w_recency 0.10, w_freq 0.05`.
`recency_decay` is exponential with a 30-day half-life; `freq_sat = n/(n+3)` saturates.

**Admission vs. min_final.** A candidate enters ranking if its cosine clears
`cosine_floor` (0.35) **OR** it has a lexical hit **OR** it is graph-linked — this is
the real relevance gate (lower than the old pure-cosine 0.6 threshold because the
lexical + graph arms now add recall and the fused ranking + `limit` cap control bloat).
`min_final` (0.08) only trims degenerate near-zero fused
scores, kept deliberately low so a strong graph/lexical-only hit (which can have low
cosine) still survives. `with_min_score(x)` is a back-compat shim that sets
`cosine_floor` (so `with_min_score(0.0)` = no semantic gate); for full control use
`with_fusion(FusionWeights { .. })`.

```rust
pub trait MemoryStore: Send + Sync {
    async fn remember(&self, item: MemoryItem) -> Result<()>;
    async fn recall(&self, query: &str, limit: usize) -> Result<Vec<MemoryItem>>;
    async fn forget(&self, id: &str) -> Result<()>;
    /// Record a *genuine* recall (not write-time dedup) — bumps each memory's
    /// recall_count/last_recalled_at and reinforces its linked entities.
    async fn note_recalled(&self, ids: &[&str]) -> Result<()>;
}
```

`recall` returns `Vec<MemoryItem>` *without* scores — admission + fusion are applied
internally. Keeps the trait small; callers don't reason about cosines.

### Consult signals (recency + frequency)

The `vectors` table carries `created_at`, `last_recalled_at`, `recall_count`. These
feed the recency/frequency fusion terms. They are updated *out of band* by
`note_recalled`, called only from genuine recall sites (the `memory_recall` tool +
the three auto-recall injection sites). It is **never** called from write-time dedup
recalls inside `remember()` / `memory_store` — that would inflate the frequency
signal every time a fact is stored. `note_recalled` also reinforces the linked
entities (`reinforce_entity`) so a frequently consulted relation stays strong.

### Forget purges all arms

`forget(id)` deletes from the vector index, the FTS5 lexical index, and unlinks the
memory's `mentions` rows (entity nodes themselves remain — they may be shared).

### Backfill

DBs predating the rework have `_entities` metadata but no `mentions` links or FTS5
rows. `Memory::backfill_hybrid` walks every stored memory, (re-)links its entities
and indexes its content; `build_memory` runs it once, guarded by a
`memory_migrations` marker row (`hybrid_backfill_v1`). Idempotent.

## Auto-recall (per-turn injection)

On every user turn, three call sites in `commands.rs` (chat / approved-task / dispatched-task) auto-inject up to **8** relevant memories into the agent's context:

```rust
if let Ok(items) = memory.recall(&message, 8).await {
    // ... fold into system_suffix as "BACKGROUND RECALL FROM PRIOR CONVERSATIONS..."
}
// then, since this is a genuine recall:
let ids: Vec<&str> = all_items.iter().map(|i| i.id.as_str()).collect();
let _ = memory.note_recalled(&ids).await;
```

Earlier versions also fanned out across extracted key terms (`memory.recall(term, N)` per term). That fan-out was removed: a single full-message recall through the hybrid fusion ranker gives high-confidence hits without flooding context with marginal matches.

Each of these three sites (and the `memory_recall` tool) calls `note_recalled` after a successful recall — this is a *genuine* consult, so the surfaced memories' recency/frequency signals climb and their linked entities are reinforced. The write-time dedup recall inside `remember()` / `memory_store` deliberately does **not** call it. When you change one auto-recall site, change all three.

The injected block sits in the host's `external_system_suffix` (which the executor folds into the first user turn's `<CONTEXT>` block — see `feedback_volatile_content_belongs_in_body` in memory). The framing tells the agent these are pre-loaded and not to re-fetch the same entities via `memory_recall`.

## Auto-judge (post-turn capture)

After the agent's response lands, three sites in `commands.rs` spawn a fire-and-forget task that calls `judge_worth_remembering`. This is a Cheap-tier LLM call (timeout 60s) that:

0. **Pre-filter:** `is_substantive_user_msg` runs first (see `commands.rs:726`). Short imperatives ("Do it", "Ok", "Delete it"), single-word acks, and other filler are rejected immediately without an LLM call. This prevents poisoned recall — a stored "Delete it" would match a future "Delete it" perfectly and surface the wrong referent. If the user message is not substantive, `judge_worth_remembering` returns `None` with a debug log entry.
1. Recalls the **top 10 existing memories** that overlap with the user's message, hybrid-ranked. (This is a dedup recall, so it does *not* call `note_recalled`.)
2. Folds them into the prompt as `MEMORIES ALREADY STORED that may overlap`.
3. Asks the judge to summarise any genuinely new facts in 1–2 sentences, or respond `SKIP`.
4. If non-SKIP, stores the summary as a fresh `MemoryItem` with `metadata.source = "conversation"`.

The dedup-via-prompt step is the key bit: without it, the same fact gets re-stored every turn it's mentioned. With it, the judge sees `User's girlfriend is Nadia` already exists and answers SKIP on subsequent mentions; only a *new* fact (e.g. her birthday) yields a fresh memory.

Three sites, identical shape:

- `commands.rs:2107` — chat path
- `commands.rs:3051` — approved-task path
- `commands.rs:3941` — dispatched-task path

When you change one, change all three.

## Agent tools

### `memory_store`

`crates/athen-app/src/app_tools.rs::do_persistent_memory_store`. Stores a `(key, value)` as `MemoryItem { id: "agent_{key}", content: "{key}: {value}", metadata: { source: "agent_tool" } }`.

**Pre-store dedup.** Before `memory.remember()`, the tool calls `memory.recall(&format!("{key}: {value}"), 1)`. Any hybrid-ranked hit is treated as a duplicate: the tool returns success with `status: "skipped", reason: "already_known", existing_id, existing_content, hint`. The LLM sees the existing memory and, per the hint, can re-call with a genuinely new fact instead of inserting a duplicate. (`remember()` *also* has its own write-time dedup — text-equality + Jaccard > 0.85 — so the auto-judge path is covered too.)

The `success: true` shape is deliberate — LLMs treat tool errors as "retry harder", but a structured "skipped" payload is information the model uses to revise its plan.

### `memory_recall`

Searches the persistent memory via the hybrid fusion ranker (semantic + lexical + graph), returns top-N hits, and calls `note_recalled` on them. If called without a `key` argument, returns an empty list with a hint to provide a search query.

### `memory_forget`

Not yet wired as an agent-callable tool (tracked as #217). Settings → Memory has a UI list with per-row delete; that's the primary path for users. The `MemoryStore::forget` trait method exists and is tested — and now purges all three arms (vector + FTS5 lexical + `mentions` graph links) — but no tool dispatch entry in `app_tools.rs` calls it yet.

## Identity vs memory: when to put a fact where

The agent has both `identity_add` and `memory_store`. Heuristic for the agent:

- **`identity_add`** for facts that are **always-on** and should shape every future agent response: personality, hard rules, who-the-user-is, team / org chart, recurring contexts. Lives in the static prompt prefix; every request pays its tokens. Use sparingly.
- **`memory_store`** for facts that are **episodic** and should surface only when relevant: project decisions, past conversations, transient context. Lives in the per-turn recall — only tokens spent when it matches the current message.

Identity has its own dedup (the entire identity block is in every prompt, so the agent sees what's already there). Memory has tool-level dedup as described above.

## Cache friendliness

Memory injection is volatile (changes per turn) so it lives in the user-side `<CONTEXT>` wrapper, not the static system prefix. This preserves the static prefix for prefix-cache LCP (DeepSeek auto-cache, Anthropic with `cache_control` markers — see [`PROMPT_CACHING.md`](PROMPT_CACHING.md)).

Anti-patterns to avoid (from `feedback_prompt_cache_optimization`):

- **Don't prepend memory to messages** — `context.insert(0, ...)` was the previous bug, fixed by routing through `external_system_suffix`.
- **Don't sort recall results by anything that varies turn-to-turn** — the embedding scores are stable for the same query against the same stored items, so the order is deterministic.
- **Don't include timestamps in memory body** — the body is part of the recalled prompt content; a turn-by-turn timestamp would invalidate any partial cache.

## Pruning + observability

- **UI**: Settings → Memory shows the live list with per-row delete. Use this to clear junk that accumulated before the dedup fixes landed.
- **Logs**: `tracing::info!("Memory judge: worth remembering")` and `"Skipping duplicate memory_store; similar entry already known"` are the two key signals. Log level `debug` for the SKIP cases.
- **No automatic eviction.** Memory is unbounded in size; the user prunes manually. If this becomes a problem in practice, an LRU eviction by `metadata.timestamp` is the obvious lever.
