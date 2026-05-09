# Prompt Caching — Provider Audit & Fix Plan

Status: **design / not yet shipped**. Audit performed 2026-05-09 against current docs for Anthropic, OpenAI, DeepSeek, Google.

Athen's prompt-build path is already cache-friendly in shape: the system message is byte-stable across turns (persona + identity + workspace + tools + revealed tool schemas), and per-turn volatile content (current time, recalled memories, attachments, compaction state) lives in a `<CONTEXT>...</CONTEXT>` wrapper at the top of the first user message. See `feedback_volatile_content_belongs_in_body.md` and `crates/athen-agent/src/executor.rs::build_context_preamble`.

Despite that, none of the four cloud providers actually realise the savings. This doc captures what each provider needs and the order in which to ship the fixes.

---

## Per-provider state

### DeepSeek — `crates/athen-llm/src/providers/deepseek.rs`

**How it works.** Fully automatic Context Caching on Disk. 64-token block granularity, exact-prefix-match. Response carries `usage.prompt_cache_hit_tokens` + `usage.prompt_cache_miss_tokens`. Pricing roughly 10× discount on hits ($0.014/M vs $0.14/M for `deepseek-chat`, verify against current table). No TTL guarantee; best-effort eviction. Works with tool calls and streaming. Ref: <https://api-docs.deepseek.com/guides/kv_cache>.

**Athen state.** Caching is firing on the wire — we just don't see it. The shared `OpenAiUsage` struct (around `openai.rs:1196`) deserialises only `prompt_tokens` / `completion_tokens` / `total_tokens`. The DeepSeek response handler at `deepseek.rs:300–318` then calls `estimate_deepseek_cost(prompt_tokens, …)` on the gross count, so the cost UI overstates by up to ~10× whenever a hit lands. Tools and request fields serialise in deterministic order — no cache-buster there.

**Fix.**
- Extend `OpenAiUsage` (or split a DeepSeek-specific struct) with `prompt_cache_hit_tokens` + `prompt_cache_miss_tokens` (`#[serde(default)]`).
- Plumb into `TokenUsage` (see cross-cutting section below).
- Update `estimate_deepseek_cost` to bill `cache_miss_tokens × $0.14/M + cache_hit_tokens × $0.014/M`.

### OpenAI — `crates/athen-llm/src/providers/openai.rs`

**How it works.** Fully automatic, ≥1024 token prefix, exact byte match across `system + messages + tools + response_format`. Optional `prompt_cache_key` for sticky routing across requests (otherwise hits are probabilistic across backend shards). Response carries `usage.prompt_tokens_details.cached_tokens`. Pricing: cached input at 50% off for GPT-4o family, 75% off for o-series. TTL 5–10 min idle, up to 1h off-peak. Ref: <https://platform.openai.com/docs/guides/prompt-caching>.

**Athen state.** Auto-cache is happening but invisible (no `cached_tokens` parsing). We never set `prompt_cache_key`, so multi-arc / multi-user dispatch lands on different shards turn-to-turn — hit rate is left to chance. System message structure is correct (stable, time injected into first user turn).

**Fix.**
- Add `PromptTokensDetails { cached_tokens: Option<u32> }` sub-struct on `OpenAiUsage`.
- Add `prompt_cache_key: Option<String>` to `OpenAiRequestOut`; the executor should set it to the arc ID so all turns of one arc route together.
- Plumb `cached_tokens` into `TokenUsage`. Update cost estimator with the 50%/75% discount.

### Anthropic — `crates/athen-llm/src/providers/anthropic.rs`

**How it works.** **Opt-in only.** Add `cache_control: { type: "ephemeral" }` markers on individual content blocks in `system`, `messages`, or `tools`. Up to 4 breakpoints per request. Default 5-minute TTL (write = 1.25× input rate); optional 1-hour TTL (write = 2× input rate). Cache reads = 0.1× input. Minimums vary by model: 4096 tokens for Claude Opus 4.5–4.7 + Haiku 4.5; 1024 for Sonnet 4–4.5 / Opus 4–4.1; 2048 for Haiku 3.5. Below the threshold it silently skips, no error. Response carries `cache_read_input_tokens` + `cache_creation_input_tokens`. Cache invalidates on changes to `tools`, `system`, `tool_choice`, or image presence. Ref: <https://docs.claude.com/en/docs/build-with-claude/prompt-caching>.

**Athen state.** **Zero cache benefit on Claude today.** No `cache_control` markers anywhere in `build_request_body`. `system` is a bare `Option<String>`; would need to become `Option<Vec<ContentBlock>>` to attach markers. `AnthropicUsage` ignores both cache token fields. **Separate bug caught by the audit:** `build_request_body` does not include the `tools` field at all — Claude is being called without any tools defined.

**Fix.**
- Refactor `system` to a `Vec<ContentBlock>` form so a `cache_control` marker can sit on the last block.
- Add the missing `tools` field, with a `cache_control` marker on the last tool definition.
- Extend `AnthropicUsage` with `cache_read_input_tokens` + `cache_creation_input_tokens`.
- Plumb into `TokenUsage`. Update `estimate_anthropic_cost` to apply 0.1× / 1.25× / 2× rates for read / 5min-write / 1h-write.
- Choose default TTL: 5min is the safe pick (most agent loops finish inside it); 1h only when `Wakeup` or other long-lived flows justify the 2× write premium.

### Google (Gemini) — `crates/athen-llm/src/providers/google.rs`

**How it works.** Two mechanisms. **Implicit** caching is automatic on Gemini 2.5 / 3.x; min prefix 1024 tokens (Flash) / 4096 (Pro); ~75% discount; surfaces via `usageMetadata.cachedContentTokenCount`. **Explicit** caching: `POST /v1beta/cachedContents` (with `model`, `contents`, `systemInstruction`, `tools`, optional `ttl`) returns a `name` like `cachedContents/abc123`; pass `cached_content: "cachedContents/abc123"` on subsequent `generateContent` calls instead of re-sending the system + tools. Storage billed per-token-per-hour. Ref: <https://ai.google.dev/gemini-api/docs/caching>.

**Athen state.** Provider is a stub — both `complete()` and `complete_streaming()` return "not yet implemented". No caching to audit.

**Fix.** When the stub is filled in:
- Send Athen's stable system prompt via `systemInstruction` (separate from `contents`), not as a synthetic user turn.
- Parse `usageMetadata.cachedContentTokenCount` into `TokenUsage.cached_tokens`.
- Optional later: add a `cached_content_name: Option<String>` field on `LlmRequest` plus a `create_cached_content()` provider method for callers who want to mint an explicit cache and reuse it across many turns (e.g. for an agent profile with a giant identity block).

---

## Cross-cutting: `TokenUsage` shape

Every audit ends in the same place: `crates/athen-core/src/llm.rs::TokenUsage` needs cache-aware fields. One unified shape covers all four providers:

```rust
pub struct TokenUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
    pub estimated_cost_usd: Option<f64>,
    /// Cache hits: DeepSeek `prompt_cache_hit_tokens`, OpenAI `cached_tokens`,
    /// Anthropic `cache_read_input_tokens`, Gemini `cachedContentTokenCount`.
    pub cached_tokens: Option<u32>,
    /// Anthropic-only: tokens written into a fresh cache entry, billed at
    /// 1.25× (5min) or 2× (1h) input rate.
    pub cache_creation_tokens: Option<u32>,
}
```

Every provider's response handler subtracts `cached_tokens` from `prompt_tokens` before billing the base rate, then adds the discounted cache-hit + (Anthropic-only) cache-write costs.

---

## Recommended ship order

1. **DeepSeek observability** — smallest diff, biggest user-visible impact (cost UI stops lying). ~30 LoC.
2. **OpenAI** — `cached_tokens` parsing + `prompt_cache_key = arc_id` request field. ~40 LoC.
3. **Anthropic** — refactor `system` to content blocks, add `cache_control: ephemeral`, fix the missing `tools` bug, parse cache-hit + cache-write usage. ~150 LoC.
4. **Google** — bake cache parsing into the provider when the stub lands.
5. **UI** — surface "X cached / Y new" on the usage badge so users can see the cache working; update arc-cost roll-ups to use the cache-aware estimator output.

---

## Cache-buster checklist (apply when touching any prompt-build path)

- Append, never prepend. `context.insert(0, ...)` is forbidden in turn-build paths.
- `HashMap` iteration is non-deterministic — use `BTreeMap` or sort keys before serialising into the prompt.
- No timestamps, transient IDs, or elapsed times in the static prefix.
- Tool definitions in stable order across turns (`Vec` is fine; never sort by hash).
- `serde_json::Value` round-trips can reorder map keys — avoid them in the prefix path.
- Arc-history queries sort by `(created_at, id)` ascending, never `created_at` alone (ties flip).
