# Reasoning Effort

Cross-provider design for exposing the "think harder / think less" knob in Athen.
Synthesized 2026-05-13 from 5 parallel Haiku research streams (OpenAI / Anthropic /
Google / DeepSeek+xAI+Mistral / local-via-llamacpp).

Companion to [PER_MODEL_QUIRKS.md](PER_MODEL_QUIRKS.md): that doc covers
*response* parsing (where thinking output lands), this one covers *request*
control (how to ask for more or less of it).

**Status:** Shipped (enum + `ChatRequest` wiring + per-provider mapping for OpenAI, Anthropic, Google, DeepSeek; 2026-05-13).

Live today:
- `ReasoningEffort` enum with all 7 variants at `crates/athen-core/src/llm.rs` (`Default`, `Off`, `Minimal`, `Low`, `Medium`, `High`, `Max`), serde + `FromStr` round-tripping.
- Carried on `ChatRequest.reasoning_effort` and threaded into every cloud-provider adapter.
- Per-provider mappers with unit tests: `anthropic.rs::map_reasoning_effort` (family-aware: Opus locked to adaptive, Sonnet/Haiku use `budget_tokens`), `openai.rs::map_reasoning_effort` (model-clamped enum), `google.rs` (`thinkingLevel` for 3.x / `thinkingBudget` for 2.5), `deepseek.rs::map_deepseek_reasoning_effort` (`high`/`max` opt-in).

Still pending:
- Local providers (`llamacpp.rs`, `ollama.rs`) ignore the field today — Qwen/Gemma `enable_thinking` template-kwarg path not wired.
- Per-arc `ArcSettings.reasoning_effort` setting and the segmented control in the arc settings panel.
- `delegate_to_agent` per-call `reasoning_effort` parameter.
- Per-tier Settings UI column for power-user defaults.

The design doc below remains the reference for the 7 wire shapes and the per-provider mapping table.

## The wire-format zoo

Every provider exposes reasoning effort differently. Today there are seven
distinct shapes Athen would have to speak:

| Provider | Wire shape | Knob type | Range |
|---|---|---|---|
| OpenAI (gpt-5.4 / gpt-5.5 / o-series) | `reasoning: { effort: "..." }` | enum | `none`, `low`, `medium`, `high`, `xhigh` (gpt-5.5; `o4-mini` lacks `none`/`xhigh`; `gpt-5.4-mini` ceiling is `high`) |
| Anthropic Opus 4.7 | `thinking: { type: "adaptive" }` | locked to adaptive | dynamic only — `type: "enabled"` rejected |
| Anthropic Sonnet 4.6 | `thinking: { type: "adaptive" \| "enabled", budget_tokens: N }` | adaptive or token budget | `budget_tokens` 1–64k |
| Anthropic Haiku 4.5 | `thinking: { type: "enabled", budget_tokens: N }` | token budget only | `budget_tokens` 1–64k, no adaptive |
| Gemini 3.x | `generationConfig.thinkingConfig.thinkingLevel: "..."` | enum | `minimal`, `low`, `medium`, `high` |
| Gemini 2.5 | `generationConfig.thinkingConfig.thinkingBudget: N` | token budget | 128–32k (Pro can't disable), 0–24k (Flash) |
| DeepSeek V4 (flash/pro) | `reasoning_effort: "..."` | enum, opt-in | `high`, `max` only (omit = off) |
| DeepSeek Reasoner (R1) | n/a — always on | none | model variant |
| xAI Grok 4.3 | `reasoning_effort: "..."` | enum | `none`, `low`, `medium`, `high` |
| Mistral magistral-* | n/a — always on | none | model variant |
| Mistral mistral-small / medium-3-5 | `reasoning_effort: "..."` | enum | `low`, `medium`, `high` |
| Local Qwen 3.5/3.6/Coder-Next | `chat_template_kwargs: { enable_thinking: true }` | boolean toggle | on/off (3.6 default on, 3.5 default off, both buggy to fully disable) |
| Local Gemma 4 | `enable_thinking` in template kwargs + system prompt token | boolean toggle | on/off |
| Local DeepSeek R1 distill | always on, `--reasoning-budget N` server flag | token budget at server level | server-side, not per-request |
| Local Llama 3.3 / 4 | n/a | none | no reasoning support |

Three observations:

1. **Three knob types**: enum (most cloud), token budget (Anthropic + Gemini 2.5),
   boolean toggle (local). No single wire format covers all.
2. **Defaults vary wildly**: Gemini 3 Pro defaults to `high`, Gemini 3 Flash-Lite
   to `minimal`, OpenAI gpt-5.4-mini to `none`, DeepSeek V4 to off,
   Anthropic to off. "Omit the field" means different things on different APIs.
3. **Some models can't disable**: Gemini 2.5 Pro, DeepSeek R1, Magistral —
   reasoning is the product, not a toggle.

## Athen's unifying abstraction

Single typed enum on the request, providers map it to their wire format:

```rust
// athen-core/src/llm.rs
pub enum ReasoningEffort {
    /// Provider default. Don't send any field. Recommended for
    /// most tasks since defaults are usually sane.
    Default,
    /// Disable reasoning entirely where the provider supports it.
    /// Equivalent to `effort: "none"`, `thinking: { type: "disabled" }` etc.
    /// On always-thinking models (R1, Magistral, Gemini 2.5 Pro),
    /// silently downgrades to the lowest available setting.
    Off,
    /// Lowest-effort reasoning. "minimal"/"low" depending on provider.
    Minimal,
    Low,
    Medium,
    High,
    /// Highest available. Maps to `xhigh` (OpenAI 5.5), `budget_tokens: 64k`
    /// (Anthropic), `thinkingLevel: high` (Gemini 3), `effort: max` (DeepSeek).
    Max,
}
```

Six levels, not five — `Default` (don't send anything) is distinct from
`Off` (explicitly disable). The distinction matters because Anthropic Opus
**rejects** `type: "enabled"` and DeepSeek **interprets omission as off**, so
`Default` lets us send the empty request and let the provider decide.

### Per-provider mapping table

| Athen level | OpenAI | Anthropic Sonnet | Anthropic Haiku | Anthropic Opus | Gemini 3 | Gemini 2.5 Flash | DeepSeek V4 | Grok 4.3 | Mistral tunable | Qwen local |
|---|---|---|---|---|---|---|---|---|---|---|
| `Default` | omit | omit | omit | omit (auto-adaptive) | omit | omit (-1 dynamic) | omit | omit | omit | omit |
| `Off` | `none` | omit | omit | adaptive (forced) | `minimal` | `thinkingBudget: 0` | omit | `none` | `low` | `enable_thinking: false` |
| `Minimal` | `low` | `budget_tokens: 1024` | `budget_tokens: 1024` | adaptive | `minimal` | `thinkingBudget: 1024` | omit | `low` | `low` | `enable_thinking: true` |
| `Low` | `low` | `budget_tokens: 4096` | `budget_tokens: 4096` | adaptive | `low` | `thinkingBudget: 4096` | `high` | `low` | `low` | `enable_thinking: true` |
| `Medium` | `medium` | `budget_tokens: 16k` | `budget_tokens: 16k` | adaptive | `medium` | `thinkingBudget: 12k` | `high` | `medium` | `medium` | `enable_thinking: true` |
| `High` | `high` | `budget_tokens: 32k` | `budget_tokens: 32k` | adaptive | `high` | `thinkingBudget: 24k` | `high` | `high` | `high` | `enable_thinking: true` |
| `Max` | `xhigh` | `budget_tokens: 64k` | `budget_tokens: 64k` | adaptive | `high` | `thinkingBudget: 24k` | `max` | `high` | `high` | `enable_thinking: true` |

Models without any knob (Llama 3.x, Magistral, DeepSeek R1) silently ignore
the field. Per-provider clamping (Sonnet 4.6 max 64k, Haiku 4.5 max 64k, etc.)
lives inside the provider adapter, not in the surface enum.

`Off` on always-thinking models (Opus, R1, Magistral) is best-effort: we send
the lowest available, but the model will still reason.

## Where the knob lives

Three surfaces, layered:

### 1. Per-arc setting (primary)

Mirror the agent-profile pattern: arcs already carry `active_profile_id` and
will eventually carry tier overrides. Add `reasoning_effort: Option<ReasoningEffort>`.

```rust
// athen-core/src/types.rs
pub struct ArcSettings {
    pub active_profile_id: Option<String>,
    pub reasoning_effort: Option<ReasoningEffort>,
    // ... future: model_tier_override, allowed_routes, ...
}
```

`None` = inherit from profile/global default. Surfaced in arc settings panel as
a 6-option segmented control (`Default` selected by default — never send the
field; user has to opt in to override).

### 2. `delegate_to_agent` tool parameter (per-call override)

When the coordinator delegates a sub-task, it should be able to dial effort
explicitly for that one call. Add `reasoning_effort: Option<String>` to the
tool's JSON schema (enum: `default | off | minimal | low | medium | high | max`).

This is the agent-authored escape hatch — coordinator looks at the task and
says "this needs Max" without changing the arc's default.

### 3. Global default per provider (Settings UI)

Next to the per-tier slug rows from Road 1 (PROVIDER_CONFIG.tier_models), add
one extra optional column: per-tier `ReasoningEffort` default. Empty = `Default`.

So Cheap might default to `Off`, Code to `Medium`, Powerful to `High`. Power
users tune this; the rest get the catalog seed.

### Resolution order (highest precedence first)

1. `delegate_to_agent` call-time param (if set)
2. Arc setting (if not `None`)
3. Provider tier default (if not `Default`)
4. Provider's built-in default (omit the field on the wire)

## Default policy

Ship with **`Default` everywhere**. Don't preemptively turn reasoning on or
off. Reasons:

- Providers tuned their defaults for their models; we're worse at guessing.
- Reasoning tokens are billed at the output rate — turning on `High`
  everywhere can 3–5× bill silently.
- Users can opt in per-arc or per-call when they need it.

The exception is the per-tier default in Settings (#3 above): power users who
want "Cheap = Off, Powerful = Max" can wire that themselves.

## Interaction with future Road 2 (per-task complexity classifier)

When [per_task_model_selection](../../.claude/projects/-home-alex-pruebas-Athen/memory/project_per_task_model_selection.md)
Road 2 lands (complexity tag in risk step), the same classifier output can
auto-set `ReasoningEffort` per-task: `ComplexityTag::Low → Off`,
`Medium → Low`, `High → Medium`. Until then, user-driven.

## Implementation footprint (estimate)

Small. The mapping is the work, not the plumbing.

- `athen-core/src/llm.rs`: add `ReasoningEffort` enum, attach to `ChatRequest`
  or carry through `RoutedRequest`. ~30 lines.
- Per-provider adapter: 7 providers × ~20 lines each = ~140 lines. Mostly the
  mapping table above expressed in code, plus per-model clamping.
- `athen-core/src/types.rs`: `ArcSettings.reasoning_effort` field with serde
  default. ~5 lines + migration.
- `athen-agent/src/tools/delegate.rs`: add `reasoning_effort` param to the
  tool schema, plumb to the dispatched request. ~15 lines.
- Settings UI (per-tier default column): ~50 lines frontend + ~10 lines
  catalog. Optional; can ship later.
- Arc settings panel: 6-option segmented control. ~30 lines frontend.

Total: ~300 lines for the full surface, half that for an MVP (per-arc setting +
delegate param, no Settings UI column).

## Gotchas to remember

- **Thought signatures**: Gemini 3 stamps `thoughtSignature` on `functionCall`
  parts that must be echoed back unchanged or HTTP 400. Already handled
  via [`gemini_thought_signature`](../../.claude/projects/-home-alex-pruebas-Athen/memory/feedback_gemini_thought_signature.md);
  enabling reasoning increases the surface area where this matters.
- **Anthropic cache breakage**: changing `thinking` mode invalidates the
  message-level cache breakpoint (system prompt + tools stay cached). Per-arc
  setting is stable enough; per-call delegate override will pay the cache cost.
- **Always-thinking models**: Opus, Magistral, DeepSeek-R1, Gemini 2.5 Pro
  silently ignore `Off`. Don't show a "Disabled" badge in the UI for these.
- **Local Qwen 3.5 disable bug**: `enable_thinking: false` doesn't actually
  disable — known upstream issue. Custom jinja template is the workaround.
- **Cost multiplier**: reasoning tokens billed at output rate. `Max` on
  Sonnet = up to 64k output tokens billed. Make this visible in the UI so
  users don't accidentally crank it on a chatty arc.
