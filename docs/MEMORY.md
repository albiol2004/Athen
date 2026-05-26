# Memory

Athen has two persistent stores that look superficially similar and are easy to confuse:

| Store | Purpose | Lives in | Filled by |
|---|---|---|---|
| **Identity** | Stable "who Athen is and who the user is" — personality, rules, personal facts, team | Static prompt prefix, every request | User edits + agent's `identity_add` tool |
| **Memory** (this doc) | Episodic facts auto-recalled per query — past projects, decisions, conversational context | Volatile per-turn injection (top-3 hits) | Agent's `memory_store` tool + post-turn auto-judge |

This doc covers Memory. For Identity see [`IDENTITY.md`](IDENTITY.md).

## Backing store

`crates/athen-memory/src/lib.rs::Memory` is hybrid:

- **Vector index** (SQLite-backed) for semantic recall when an embedder is configured. Default at runtime via `state.rs::build_memory` — uses whichever provider's embedding model is configured (Ollama / OpenAI / Gemini).
- **Knowledge graph** for entity-aware traversal — `recall` extracts entities from the query, finds memories mentioning those entities, and boosts their score.
- **Keyword fallback** when no embedder is wired. `tokenize_query` strips stopwords (English + Spanish) and intersects against stored content tokens.

`MemoryStore` trait (`crates/athen-core/src/traits/memory.rs`):

```rust
pub trait MemoryStore: Send + Sync {
    async fn remember(&self, item: MemoryItem) -> Result<()>;
    async fn recall(&self, query: &str, limit: usize) -> Result<Vec<MemoryItem>>;
    async fn forget(&self, id: &str) -> Result<()>;
}
```

`recall` returns `Vec<MemoryItem>` *without* scores — the relevance threshold is applied internally before the return. This is intentional: keeps the trait small, callers don't need to think about embedding cosines.

## Relevance threshold

Global threshold is `min_relevance_score: 0.6` (`state.rs::build_memory`). Anything below 0.6 cosine similarity is dropped before `recall` returns. The default in the builder is 0.3 — Athen overrides it because 0.3 was producing too much noise (low-confidence matches against random stored junk getting injected into prompts).

If you need more or fewer matches in a specific call site, the right answer is **not** to fiddle with the threshold per-call (the trait doesn't expose scores). Instead:

- Want stricter? Phrase the query more specifically.
- Want looser? Lower the global threshold (and accept the bloat tradeoff).

For a recall path that genuinely needs scores (e.g. UI showing "match strength"), add a separate trait method that returns `Vec<(MemoryItem, f32)>` rather than mutating `recall`.

## Auto-recall (per-turn injection)

On every user turn, three call sites in `commands.rs` (chat / approved-task / dispatched-task) auto-inject up to **3** relevant memories into the agent's context:

```rust
if let Ok(items) = memory.recall(&message, 3).await {
    // ... fold into system_suffix as "MEMORIES ALREADY LOADED..."
}
```

Earlier versions also fanned out across extracted key terms (`memory.recall(term, 3)` per term). That fan-out was removed: a single full-message recall against the 0.6 threshold gives high-confidence hits without flooding context with marginal matches.

The injected block sits in the host's `external_system_suffix` (which the executor folds into the first user turn's `<CONTEXT>` block — see `feedback_volatile_content_belongs_in_body` in memory). The framing tells the agent these are pre-loaded and not to re-fetch the same entities via `memory_recall`.

## Auto-judge (post-turn capture)

After the agent's response lands, three sites in `commands.rs` spawn a fire-and-forget task that calls `judge_worth_remembering`. This is a Cheap-tier LLM call (timeout 60s) that:

0. **Pre-filter:** `is_substantive_user_msg` runs first (see `commands.rs:726`). Short imperatives ("Do it", "Ok", "Delete it"), single-word acks, and other filler are rejected immediately without an LLM call. This prevents poisoned recall — a stored "Delete it" would match a future "Delete it" perfectly and surface the wrong referent. If the user message is not substantive, `judge_worth_remembering` returns `None` with a debug log entry.
1. Recalls the **top 3 existing memories** that overlap with the user's message, threshold-gated.
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

**Pre-store dedup.** Before `memory.remember()`, the tool calls `memory.recall(&format!("{key}: {value}"), 1)`. Any hit (passing the 0.6 threshold) is treated as a duplicate: the tool returns success with `status: "skipped", reason: "already_known", existing_id, existing_content, hint`. The LLM sees the existing memory and, per the hint, can re-call with a genuinely new fact instead of inserting a duplicate.

The `success: true` shape is deliberate — LLMs treat tool errors as "retry harder", but a structured "skipped" payload is information the model uses to revise its plan.

### `memory_recall`

Searches the persistent memory by embedding similarity, returns top-N hits. If called without a `key` argument, returns an empty list with a hint to provide a search query. No special handling beyond the global threshold.

### `memory_forget`

Not yet wired as an agent-callable tool (tracked as #217). Settings → Memory has a UI list with per-row delete; that's the primary path for users. The `MemoryStore::forget` trait method exists and is tested, but no tool dispatch entry in `app_tools.rs` calls it yet.

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
