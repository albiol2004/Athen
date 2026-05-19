# Per-Model Quirks

LLM providers and open-weights models differ in *concrete*, *cross-cutting*
ways that breaks Athen's executor when handled generically: where tool calls
appear, where reasoning text appears, what the chat template will accept,
how tool arguments are serialized. The per-model quirks system encodes
these differences as a small typed table and routes provider response
parsing through it.

This is a major Athen subsystem and will be redone many times as new
models ship. This doc is the design of record. Update it whenever the
shape changes.

## 1. Why this exists (concrete failure modes that drove it)

Each of these triggered a real bug found in the wild before this system
existed. They're the seed of every axis below.

- **Qwen3.5-9B against llama.cpp + `--jinja`**: emits tool calls inline as
  `<tool_call><function=NAME><parameter=KEY>VAL</parameter></function></tool_call>`
  in content text instead of structured `tool_calls`. llama.cpp's parser
  bails when prose precedes the call. Athen sees `tool_calls: []` and
  empty content (the answer went to `reasoning_content`), fires its
  hardcoded "I don't have enough information" fallback, and the user
  sees gibberish. (`agent_stdout.txt` artifact, `bugfix_001` benchmark.)

- **Qwen / Gemma 4 / DeepSeek V3.1 chat templates** raise
  `'System message must be at the beginning'` if any system message
  appears past position 0. Athen's `commands.rs` historically pushed
  memory recall, attachment surfacing, and compaction summaries as
  mid-stream `Role::System` messages — DeepSeek/OpenAI/Anthropic
  silently accept this; Qwen/Gemma return HTTP 500 from llama.cpp.

- **DeepSeek streaming** concatenates tool argument deltas containing
  raw control characters (0x00–0x1F), which break `serde_json` on the
  receiving side. (`feedback_deepseek_tool_args_repair.md`.)

- **DeepSeek-R1 multi-turn**: `reasoning_content` must be echoed back
  on the next turn iff a tool call fired, omitted otherwise. Sending
  it when not required returns 400; omitting it when required corrupts
  state.

- **MiniMax M2.5** advertises OpenAI-compat but emits
  `<minimax:tool_call><invoke name=...><parameter name=...>` XML
  inline in content. The OpenAI shim does not convert.

- **Llama 3.2 / 4** emit `[{"name": ..., "parameters": ...}]` JSON
  (or pythonic `[func(p=v)]` on 1B/3B) inline in content. Same
  llama.cpp `--jinja` extraction failure.

- **Mistral local** emits a `[TOOL_CALLS]` special token followed by
  JSON; without the right Jinja template, this leaks as content text.

If you find yourself adding a `match` on the model name inside the
provider's response parser, that match belongs in this table instead.

## 2. The five axes

Every model surveyed reduces to a tuple along these axes. Adding a new
provider/model rarely needs a new axis — usually just a new variant in
an existing enum.

### Axis 1 — `ToolExtractionStrategy`

How tool calls are recovered from the response.

```
Structured                            // trust the response's tool_calls field
InlineXmlQwenStyle                    // <tool_call><function=N><parameter=K>V
InlineXmlVendorTagged(&'static str)   // <minimax:tool_call><invoke name=N>
InlineJsonLlama                       // [{"name":N, "parameters":{...}}]
InlinePythonicLlama                   // [func(param=value)]
SpecialTokenBlock(&'static str)       // [TOOL_CALLS], <｜DSML｜...>, etc.
```

When `Structured` is wrong, the strategy gets the response and is
expected to (a) extract any tool calls into synthetic `ToolCall`s and
(b) strip them from `content` so the prose remains clean. The same
control-char repair (`feedback_deepseek_tool_args_repair.md`) runs on
the resulting JSON args before dispatch — every strategy must produce
output that flows through that pipeline, not bypass it.

**Reasoning-content fallback for inline strategies.** SHIPPED (2026-05-19). When the
provider routes the entire reply (think tags + tool call markup) into
`reasoning_content` instead of `content` — which llama.cpp does under
`--jinja --reasoning-format` for Qwen-class models — the inline
extractor scans `reasoning_content` as a fallback when `content`
is empty and no `tool_calls` came through (crates/athen-llm/src/quirks/mod.rs:213–221). Without this, Qwen3.5/3.6
under `--reasoning-format` returns "no tool calls" and the executor
fires its hardcoded fallback string. See
`feedback_quirks_scan_reasoning_content.md`.

### Axis 2 — `ReasoningSurface`

Where the model's chain-of-thought lives in the response.

```
None                  // no reasoning emitted (Llama base, GPT-4 class)
SeparateField         // reasoning_content (DeepSeek-R1, Kimi K2 thinking)
InlineThinkTags       // <think>...</think> in content (Qwen, Gemma 4, Magistral)
NativeContentBlock    // Anthropic thinking blocks, Gemini part.thought:true
HiddenServerSide      // OpenAI o-series — only token counts visible
```

Sub-quirks for `SeparateField` and `InlineThinkTags`:

- If `content` is empty AND `tool_calls` is empty AND reasoning is
  non-empty: promote reasoning into `content` so the executor can
  render it. This is purely a JSON-field handler; no message scanning.
- If `InlineThinkTags`: strip a *single* leading `<think>...</think>`
  block from content before showing to user. Don't try to be clever
  with nested or partial tags — emit as-is when malformed.

**Streaming caveat (frontend rescue path).** llama.cpp's
`--jinja --reasoning-format` routes the entire `<think>...</think>`
envelope into the streaming `reasoning_content` field. For Qwen-class
models that wrap a *complete* reply (final answer + chain-of-thought)
in one `<think>` block, the SSE stream contains only
`is_thinking: true` chunks and the executor's `try_streaming_call`
returns `result.content = ""`. The executor then issues a non-streaming
fallback call where `apply_to_response`'s promotion rule above kicks in
correctly — but the FE's "finalize streaming bubble" branch had
already run, leaving the user with a "Thinking..." block and no answer
bubble. The FE response handler now patches a bubble from
`response.content` whenever the streamed row exists but contains no
`.message-bubble`. This is a UI symptom of the empty-content streaming
case; the backend promotion rule is unchanged. Cost: one wasted LLM
roundtrip per all-thinking turn — a future optimization in
`try_streaming_call` could promote `thinking → content` at end-of-stream
so the fallback isn't needed, but doing so would either duplicate the
text in the UI (bubble + already-streamed thinking) or require deleting
the thinking deltas mid-render; neither is obviously better than the
current rescue.

### Axis 3 — `TemplateStrictness`

Constraint the chat template enforces. Only matters for local
inference (llama.cpp / vLLM / SGLang); cloud APIs handle it
server-side.

```
Lenient                       // cloud APIs, Kimi GGUF, Llama 3.2/4 templates
SystemMustBeFirst             // Qwen3.5/3.6, Gemma 4, DeepSeek V3.1
SystemAbsorbedIntoUser        // Mistral [INST] wrap
```

`SystemMustBeFirst` is the constraint that drove the
`AgentBuilder::external_system_suffix` plumbing. Athen no longer pushes
mid-stream `Role::System` for any host (memory, attachments,
compaction) — content is folded into the leading system message via
the suffix. Keep it that way.

### Axis 4 — `ToolArgRepair` (bitflags)

Pre-parse fixes applied to tool argument strings.

```
ControlCharsToUnicodeEscape      // DeepSeek streaming control chars
UnescapeDoubleEncodedJsonArrays  // Gemma 4 via Ollama (arrays come back as escaped strings)
```

These compose. New repairs append to the bitflag enum.

### Axis 5 — Sub-flags

Discrete booleans that don't justify their own enum:

- `echo_reasoning_on_tool_turn: bool` — DeepSeek-R1 needs prior turn's
  `reasoning_content` echoed back when a tool call fired.
- `system_message_required: bool` — Gemma 4 requires a system message
  to exist (any first-position system or developer message satisfies).

Add new sub-flags here when a one-off quirk doesn't fit an existing
axis. Promote to a full enum when a second model needs the same flag
with a different value.

## 3. Data model

```rust
// athen-llm/src/quirks/mod.rs
pub struct ModelQuirks {
    pub tool_extraction: ToolExtractionStrategy,
    pub reasoning_surface: ReasoningSurface,
    pub template_strictness: TemplateStrictness,
    pub tool_arg_repair: ToolArgRepair,
    pub echo_reasoning_on_tool_turn: bool,
    pub system_message_required: bool,
}

impl Default for ModelQuirks {
    /// Safe default for unknown models: trust the structured fields,
    /// no reasoning extraction, no template constraints, no repair.
    /// Reproduces today's behavior for every model we haven't profiled.
    fn default() -> Self {
        Self {
            tool_extraction: ToolExtractionStrategy::Structured,
            reasoning_surface: ReasoningSurface::None,
            template_strictness: TemplateStrictness::Lenient,
            tool_arg_repair: ToolArgRepair::empty(),
            echo_reasoning_on_tool_turn: false,
            system_message_required: false,
        }
    }
}
```

## 4. How users select a quirks profile (the "family" model)

We do **not** auto-detect quirks from the model slug. Auto-detection
fails the moment a vendor ships `claude-haiku-4-6-22062026` with the
same wire format as 4.5 — a regex match would either over-match
(treating an unrelated slug as Haiku) or miss (forcing the user to
wait for our table update).

Instead, the UI surfaces two independent fields per provider config:

1. **Model family** (dropdown) — the canonical preset Athen knows
   about: `Claude Haiku 4.5`, `DeepSeek-R1`, `Qwen3.5`,
   `Llama 3.2 instruct`, etc. Each entry maps to one `ModelQuirks`
   value via the seed table (§5).
2. **Model slug** (free-text) — the literal string sent to the
   provider's API. Pre-filled from the family default but always
   editable.

Result:

- Day a new model drops with the **same wire format**: user edits the
  slug to the new id, ships immediately. Quirks ride on the family
  selection.
- Day a new model drops with a **different wire format**: Athen ships
  an update adding the new family. User opens settings, switches
  family in the dropdown.

This makes adding a new model a one-line PR (new row in the seed
table + a UI label) rather than a parser change.

## 5. Seed table (the wire-format families the code knows about)

The code carries one row per **wire format**, not per model SKU. A user
running any GPT-5.x chat model picks `Gpt5`; the slug field is where
they put their exact model id. The full per-vendor SKU list lives in §5b.

Last verified: **2026-05-19** (code audit: quirks/mod.rs reasoning_content fallback shipped at lines 213–221; all 5 axes confirmed in seed.rs and extractors/).

| Family preset (code) | UI label | Default slug | tool_extraction | reasoning_surface | template_strictness | tool_arg_repair | sub-flags |
|---|---|---|---|---|---|---|---|
| `ClaudeOpus47` | Claude Opus 4.7 | `claude-opus-4-7` | Structured | NativeContentBlock | Lenient | — | — |
| `ClaudeSonnet46` | Claude Sonnet 4.6 | `claude-sonnet-4-6` | Structured | NativeContentBlock | Lenient | — | — |
| `ClaudeHaiku45` | Claude Haiku 4.5 | `claude-haiku-4-5` | Structured | None | Lenient | — | — |
| `Gpt5` | GPT-5 chat (5 / 5.4 / 5.5 family) | `gpt-5.5` | Structured | None | Lenient | — | — |
| `OpenAiO3` | OpenAI o-series (o3, o4-mini) | `o4-mini` | Structured | HiddenServerSide | Lenient | — | — |
| `Gemini3Pro` | Gemini 3 / 3.1 Pro / Deep Think | `gemini-3.1-pro-preview` | Structured | NativeContentBlock | Lenient | — | — |
| `Gemini3Flash` | Gemini 3 Flash | `gemini-3-flash-preview` | Structured | NativeContentBlock | Lenient | — | — |
| `DeepSeekV4Chat` | DeepSeek V4 chat (V4-Flash) | `deepseek-v4-flash` | Structured | None | Lenient | ControlCharsToUnicodeEscape | — |
| `DeepSeekV4Pro` | DeepSeek V4 Pro | `deepseek-v4-pro` | Structured | None | Lenient | ControlCharsToUnicodeEscape | — |
| `DeepSeekR1` | DeepSeek-R1 / V4 thinking mode | `deepseek-reasoner` | Structured | SeparateField | Lenient | ControlCharsToUnicodeEscape | echo_reasoning_on_tool_turn |
| `Qwen35Local` | Qwen 3.5 (local) | `qwen3.5-9b-instruct` | InlineXmlQwenStyle | InlineThinkTags | SystemMustBeFirst | — | — |
| `Qwen36Local` | Qwen 3.6 (local) | `qwen3.6-27b-instruct` | InlineXmlQwenStyle | InlineThinkTags | SystemMustBeFirst | — | — |
| `Qwen3CoderNext` | Qwen3-Coder Next | `Qwen/Qwen3-Coder-Next` | InlineXmlQwenStyle [a] | InlineThinkTags | SystemMustBeFirst | — | — |
| `Gemma4Local` | Gemma 4 (local) | `gemma-4-27b-it` | Structured [b] | InlineThinkTags | SystemMustBeFirst | UnescapeDoubleEncodedJsonArrays | system_message_required |
| `KimiK26Cloud` | Kimi K2.6 cloud | `kimi-k2-6` | Structured | SeparateField | Lenient | — | — |
| `MiniMaxM25Cloud` | MiniMax M2.5 cloud | `minimax-m2-5` | InlineXmlVendorTagged("minimax") | InlineThinkTags | Lenient | — | — |
| `Llama32Instruct` | Llama 3.2 (Vision / 70B class) | `llama-3.2-90b-vision-instruct` | InlineJsonLlama | None | Lenient | — | — |
| `Llama33Instruct` | Llama 3.3 70B instruct | `meta-llama/Llama-3.3-70B-Instruct` | InlineJsonLlama | None | Lenient | — | — |
| `Llama4Instruct` | Llama 4 (Scout / Maverick) | `llama-4-maverick-17b-128e-instruct` | InlinePythonicLlama | None | Lenient | — | — |
| `MistralLarge3` | Mistral Large 3 | `mistral-large-latest` | Structured | None | SystemAbsorbedIntoUser | — | — |
| `MagistralMedium` | Magistral Medium | `magistral-medium-latest` | Structured | InlineThinkTags | SystemAbsorbedIntoUser | — | — |
| `Codestral2508` | Codestral 25.08 | `codestral-latest` | Structured | None | SystemAbsorbedIntoUser | — | — |
| `Grok4` | xAI Grok 4 | `grok-4` | Structured | None | Lenient | — | — |
| `Default` | _(fallback)_ | _whatever_ | Structured | None | Lenient | — | — |

Notes:
- **[a]** Qwen3-Coder Next emits `<TOOL_NAME>...<parameter=KEY>VAL</parameter></TOOL_NAME>`
  (tool name as the wrapper tag) instead of the standard `<tool_call><function=NAME>...`.
  Today this rides on the same extractor as a best-effort fallback; a
  dedicated `InlineXmlQwenCoderStyle` strategy is tracked but not yet
  implemented — open a PR if you hit a real Qwen3-Coder payload that
  doesn't extract.
- **[b]** Gemma 4's tool-call wire format is contested: Anthropic-style
  `<|tool_call>...<tool_call|>` special tokens have been observed when
  running Gemma 4 via Ollama with `--functions`; the official Google
  guide describes the same format. Athen currently treats Gemma 4 as
  `Structured` because the Ollama OpenAI-compat shim usually parses the
  special tokens before they reach us. Revisit if a user reports raw
  `<|tool_call>` markup leaking into responses.

The `Default` row is the safety net: any provider whose user hasn't
picked a family falls through to it and reproduces today's OpenAI-compat
baseline behavior.

## 5b. Per-vendor model registry

Every shipping API SKU we know about, mapped to the family in §5. The
slug column shows the **canonical 2026 slug**; deprecated aliases are
noted. When a vendor ships a new model with the same wire format,
update this table, leave the family unchanged, and users edit the slug
field in Settings to opt in.

### Anthropic Claude

| Model | Canonical slug | Maps to family | Notes |
|---|---|---|---|
| Claude Opus 4.7 | `claude-opus-4-7` | `ClaudeOpus47` | Flagship reasoning, 1M ctx, adaptive thinking required |
| Claude Sonnet 4.6 | `claude-sonnet-4-6` | `ClaudeSonnet46` | Speed + intelligence, 1M ctx, adaptive thinking |
| Claude Haiku 4.5 | `claude-haiku-4-5` (alias `-20251001`) | `ClaudeHaiku45` | Cost-optimized, 200k ctx, **no extended thinking** |
| Claude Opus 4.6 (legacy) | `claude-opus-4-6` | `ClaudeOpus47` | Migrate to 4.7; same wire format |
| Claude Sonnet 4.5 (legacy) | `claude-sonnet-4-5-20250929` | `ClaudeSonnet46` | Same wire format |
| Claude Opus 4.5 (legacy) | `claude-opus-4-5-20251101` | `ClaudeOpus47` | Same wire format |
| Claude Mythos Preview | `claude-mythos-preview` | `ClaudeOpus47` | Invitation-only; defensive cybersecurity focus |

### OpenAI

| Model | Canonical slug | Maps to family | Notes |
|---|---|---|---|
| GPT-5.5 | `gpt-5.5` | `Gpt5` | Current default flagship (Apr 2026); 1M ctx |
| GPT-5.5 Pro | `gpt-5.5-pro` | `Gpt5` | Responses API only; extended compute |
| GPT-5.4 | `gpt-5.4` | `Gpt5` | Lower cost frontier, enhanced vision |
| GPT-5.4 Mini | `gpt-5.4-mini` | `Gpt5` | Production fast-path |
| GPT-5.3-Codex | `gpt-5.3-codex` | `Gpt5` | Agentic coding specialization |
| o4-mini | `o4-mini` | `OpenAiO3` | Small reasoning model; 50% o3-mini cost |
| o3 | `o3` | `OpenAiO3` | Advanced reasoning; CoT persists across tool calls |
| o3-mini (deprecated) | `o3-mini` | `OpenAiO3` | Superseded by o4-mini |

### Google Gemini

| Model | Canonical slug | Maps to family | Notes |
|---|---|---|---|
| Gemini 3.1 Pro | `gemini-3.1-pro-preview` | `Gemini3Pro` | Flagship; 1–2M ctx; adaptive thinking |
| Gemini 3.1 Deep Think | `gemini-3.1-deep-think` | `Gemini3Pro` | Early access; PhD-level math/science |
| Gemini 3 Flash | `gemini-3-flash-preview` | `Gemini3Flash` | Fast tier; NativeContentBlock reasoning (same wire as 3.1 Pro) |
| Gemini 3.1 Flash-Lite | `gemini-3.1-flash-lite` | `Default` | Ultra-low-latency; high-volume |
| Gemini 3.1 Flash Live | `gemini-3.1-flash-live-preview` | `Default` | Real-time streaming dialogue |
| Gemini 2.5 Pro (legacy) | `gemini-2.5-pro` | `Default` | Mature predecessor; reasoning hidden |
| Gemini 2.5 Flash (legacy) | `gemini-2.5-flash` | `Default` | Best price/perf prior gen |

### DeepSeek

| Model | Canonical slug | Maps to family | Notes |
|---|---|---|---|
| DeepSeek V4 Pro | `deepseek-v4-pro` | `DeepSeekV4Pro` | Flagship MoE (1.6T / 49B active); 1M ctx |
| DeepSeek V4 Flash | `deepseek-v4-flash` | `DeepSeekV4Chat` | Cost-efficient (284B / 13B active); 1M ctx |
| (Both above with thinking enabled) | same slug + `thinking: true` | `DeepSeekR1` | Reasoning surfaces as `reasoning_content`; must be echoed on tool turns |
| DeepSeek-V4 chat (BC alias) | `deepseek-chat` | `DeepSeekV4Chat` | Backward-compatible alias for V4 Flash; deprecated 2026-07-24 |
| DeepSeek reasoner (BC alias) | `deepseek-reasoner` | `DeepSeekR1` | Backward-compatible alias for V4 Flash thinking-mode; deprecated 2026-07-24 |
| DeepSeek-R1 (open weights) | (HuggingFace) | `DeepSeekR1` | Self-hosted; same wire shape |

### Qwen (local / open-weights)

| Model | HF slug | Maps to family | Notes |
|---|---|---|---|
| Qwen 3.5 (4B/9B/27B/MoE) | `Qwen/Qwen3.5-{SIZE}` | `Qwen35Local` | Hermes-JSON in `<tool_call>`; multimodal |
| Qwen 3.6 (27B/35B-A3B) | `Qwen/Qwen3.6-{SIZE}` | `Qwen36Local` | Same wire format as 3.5; MoE variant |
| Qwen3-Coder Next | `Qwen/Qwen3-Coder-Next` | `Qwen3CoderNext` | 80B MoE / 3B active; **different XML shape** ([a] in §5) |
| Qwen-VL (vision) | `Qwen/Qwen3.5-VL-*` | `Qwen35Local` | Multimodal variant; same tool-call format |

### Meta Llama

| Model | HF / cloud slug | Maps to family | Notes |
|---|---|---|---|
| Llama 4 Scout 17B-16E | `llama-4-scout-17b-16e-instruct` | `Llama4Instruct` | MoE (16 experts); pythonic tool calls |
| Llama 4 Maverick 17B-128E | `llama-4-maverick-17b-128e-instruct` | `Llama4Instruct` | MoE (128 experts); pythonic tool calls |
| Llama 3.3 70B | `meta-llama/Llama-3.3-70B-Instruct` | `Llama33Instruct` | Pure text successor to 3.1; JSON arrays |
| Llama 3.2 11B / 90B Vision | `meta-llama/Llama-3.2-{11B,90B}-Vision-Instruct` | `Llama32Instruct` | JSON arrays; tool-calling disables when images present |
| Llama 3.2 1B / 3B | `meta-llama/Llama-3.2-{1B,3B}-Instruct` | `Llama4Instruct` [c] | **Pythonic** tool calls — same shape as Llama 4 |

[c] Small Llama 3.2 (1B / 3B) reuses the `Llama4Instruct` family because
both emit pythonic `[func(p=v)]`. Eventually a dedicated `Llama32Edge`
row would clarify intent, but the wire format is identical so today
this is fine.

### Mistral

| Model | Slug | Maps to family | Notes |
|---|---|---|---|
| Mistral Large 3 | `mistral-large-latest` (`-2512`) | `MistralLarge3` | 256k ctx; 41B active / 675B total MoE |
| Mistral Small 4 | `mistral-small-4-latest` | `MagistralMedium` | Unified reasoning + vision + code; `<think>` tags |
| Mistral Medium 3.5 | `mistral-medium-3.5-latest` | `MagistralMedium` | 8x cheaper than Large; reasoning + agents |
| Magistral Medium | `magistral-medium-latest` (`-2509`) | `MagistralMedium` | Reasoning-tuned; tokenized `<think>` |
| Codestral 25.08 | `codestral-latest` (`-2508`) | `Codestral2508` | Code-specific; agentic workflows |
| Ministral (3B / 8B / 14B) | `ministral-{SIZE}-2512` | `MistralLarge3` | Edge variants; same wire format |
| Pixtral / Devstral | (integrated in Small 4) | `MagistralMedium` | Vision / code specialists merged into Small 4 |

### Other

| Model | Slug | Maps to family | Notes |
|---|---|---|---|
| Kimi K2.6 cloud | `kimi-k2-6` | `KimiK26Cloud` | `reasoning_content` field; preserve-thinking |
| Kimi K2 Thinking | `kimi-k2-thinking` | `KimiK26Cloud` | Explicit thinking mode |
| MiniMax M2.5 | `minimax-m2-5` | `MiniMaxM25Cloud` | `<minimax:tool_call>` namespaced XML when not via vLLM |
| Gemma 4 (Apache 2.0) | `gemma-4-27b-it` | `Gemma4Local` | Special-token tool format; verify on report |
| xAI Grok 4 | `grok-4` | `Grok4` | 1M ctx; OpenAI-compat structured |
| Yi-Lightning | `yi-lightning` | `Default` | OpenAI-compat baseline; no special quirks |
| Nemotron 70B | `llama-3.1-nemotron-70b-instruct` | `Llama33Instruct` | Llama-3.1 base; reuse Llama family |

**Skipped (different API entirely, not OpenAI-compat agent backends):**
Cohere Command (native `tool_plan`/`tool_calls` shape), Perplexity Sonar
(built-in tools only, no custom function calling), Inflection Pi (no
public API yet).

**Updating this table:** when a vendor ships a new model with the same
wire format as a row above, just append a new line under that vendor.
When a new wire format appears, see §7 (adding a new family) and §8
(adding a new axis).

## 6. Where this plugs in

`OpenAiCompatibleProvider::complete()` (and `complete_streaming()`)
get the active `ModelQuirks` from the `LlmRequest` (threaded through
from the executor / app). After the provider parses the structured
response, it consults the quirks:

1. If `tool_calls` is empty AND `tool_extraction != Structured`, run
   the strategy's content extractor.
2. If `content` is empty AND `tool_calls` is empty AND `reasoning_surface`
   produces text, promote it.
3. If `tool_arg_repair` has flags, run them over each tool call's
   `arguments` string.
4. Hand the cleaned `LlmResponse` back up.

`AnthropicProvider` and any other native provider that doesn't go
through the OpenAI-compat path uses the same quirks struct but applies
only the bits that make sense for its wire format (e.g.
`NativeContentBlock` reasoning is read directly from
`thinking` content blocks; the inline-XML strategies don't apply).

`TemplateStrictness` is enforced **upstream** of the provider, in the
executor / app: `SystemMustBeFirst` means no mid-stream `Role::System`
ever, so memory/attachments/compaction must use
`AgentBuilder::external_system_suffix`. The provider itself never
receives a request that violates the constraint.

## 7. Adding a new family

1. Add a row to the seed table in `athen-llm/src/quirks/seed.rs`.
2. Add a `ModelFamily` enum variant + display label in `athen-core`.
3. Add a UI dropdown entry that maps the variant to the seed row.
4. (Rare) If a new wire format appears, add a variant to the relevant
   axis enum and a strategy implementation in
   `athen-llm/src/quirks/extractors/`.

That's it. No regex, no auto-detect, no provider-internal `match`.

## 8. Adding a new axis

When you find yourself wanting to add a sixth axis: first check whether
it's actually a new variant on an existing enum. Most of the time it is.

If it really is a new orthogonal dimension:

1. Add the field to `ModelQuirks` with a sensible `Default` value.
2. Update §2 of this doc with the new axis.
3. Update §5 (seed table) — adding a column is fine; every existing row
   should pick the default unless you actively researched it.
4. Wire it into the provider where it applies.

The doc is the spec. If a code change disagrees with §2-§5, the doc
wins until the code or doc gets updated.

## 9. What this is *not*

- **Not** a chat-template renderer. We don't build Jinja templates;
  we just enforce the constraints they impose on us
  (`TemplateStrictness`).
- **Not** a model router. Picking *which* model to use for a given
  task lives in `LlmRouter` / model profiles. Quirks describe how to
  *talk to* a chosen model.
- **Not** an auto-detector. The user picks the family. Auto-detection
  is a footgun (see §4).
- **Not** the place for performance/cache settings (KV-cache size,
  prefix-cache strategy, etc.) — those belong in provider config.
- **Not** the place for credential handling — keys and base URLs stay
  in `ProviderConfig`.
- **Not** the place for sampling parameters. Temperature is per-provider
  (`ProviderConfig.temperature: Option<f32>`, surfaced in Settings UI as
  `provider-temperature`) and applied at request construction, not via
  this table. A future per-task / per-profile override mechanism is
  tracked in `project_per_task_model_selection.md`.

## 10. Open questions / future work

- **Streaming markup leak**: the streaming path now runs the inline tool-call
  extractor at end-of-stream via `quirks::extract_streaming_tail`, so the
  agent loop receives the recovered tool calls correctly. *However*, the
  raw `<tool_call>...</tool_call>` markup is still streamed to the consumer
  in the visible deltas — we can't retroactively edit chunks already sent.
  The agent works; the chat UI briefly shows the markup before the model
  finalises its turn. Two ways to fix: (a) buffer all visible content for
  non-`Structured` strategies and only emit at end-of-stream (loses
  streaming UX for those models, but Qwen local is fast), or (b) frontend
  CSS / regex filter on rendered chat content. Frontend filter is cheaper
  and is the intended path.
- **Vision/multimodal axis**: image input shape varies (URL vs
  base64 vs Files API; Bedrock-Llama is PNG-only). Not yet modeled
  here because it lives in request construction, not response
  parsing. Add as a sixth axis (`MultimodalShape`) when the multimodal
  surface area grows.
- **Cache-control axis**: Anthropic uses `cache_control` markers,
  OpenAI auto-caches at ≥1024-token prefixes, Gemini has its own
  controls, llama.cpp needs `--swa-full` for SWA models. Currently
  handled outside this doc; consolidate when the patterns stabilize.
- **Per-family default `ModelProfile` mapping**: e.g. a `coder` profile
  picking the right "Code" model bundle by family. Tracked in
  `project_per_task_model_selection.md`.
