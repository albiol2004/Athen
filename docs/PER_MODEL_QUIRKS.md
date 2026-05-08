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

## 5. Seed table (latest models, 2026-05)

Initial entries cover the latest of each family the research covered.
Older versions (Qwen 2.x, Llama 2/3, Mistral 7B v0.1, DeepSeek V2)
land via PR on demand per the "latest first, backwards on community
feedback" rule.

| Family preset (UI label) | Default slug | tool_extraction | reasoning_surface | template_strictness | tool_arg_repair | other |
|---|---|---|---|---|---|---|
| Claude Opus 4.7 | `claude-opus-4-7` | Structured | NativeContentBlock | Lenient | — | — |
| Claude Sonnet 4.6 | `claude-sonnet-4-6` | Structured | NativeContentBlock | Lenient | — | — |
| Claude Haiku 4.5 | `claude-haiku-4-5` | Structured | NativeContentBlock | Lenient | — | — |
| GPT-5 | `gpt-5` | Structured | HiddenServerSide | Lenient | — | — |
| OpenAI o3 | `o3` | Structured | HiddenServerSide | Lenient | — | — |
| Gemini 3 Pro | `gemini-3-pro` | Structured | NativeContentBlock | Lenient | — | — |
| DeepSeek-V4 chat | `deepseek-chat` | Structured | None | Lenient | ControlCharsToUnicodeEscape | — |
| DeepSeek-R1 reasoner | `deepseek-reasoner` | Structured | SeparateField | Lenient | ControlCharsToUnicodeEscape | echo_reasoning_on_tool_turn |
| Qwen 3.5 (local) | `qwen3.5-9b-instruct` | InlineXmlQwenStyle | InlineThinkTags | SystemMustBeFirst | — | — |
| Qwen 3.6 (local) | `qwen3.6-27b-instruct` | InlineXmlQwenStyle | InlineThinkTags | SystemMustBeFirst | — | — |
| Gemma 4 (local) | `gemma-4-27b-it` | Structured | InlineThinkTags | SystemMustBeFirst | UnescapeDoubleEncodedJsonArrays | system_message_required |
| Kimi K2.6 cloud | `kimi-k2-6` | Structured | SeparateField | Lenient | — | — |
| MiniMax M2.5 cloud | `minimax-m2-5` | InlineXmlVendorTagged("minimax") | InlineThinkTags | Lenient | — | — |
| Llama 3.2 instruct | `llama-3.2-90b-instruct` | InlineJsonLlama | None | Lenient | — | — |
| Llama 4 instruct | `llama-4-maverick-instruct` | InlineJsonLlama | None | Lenient | — | — |
| Mistral Large 3 | `mistral-large-latest` | Structured (cloud) / SpecialTokenBlock("[TOOL_CALLS]") (local) | None | SystemAbsorbedIntoUser | — | — |
| Magistral Medium | `magistral-medium-latest` | Structured | InlineThinkTags | SystemAbsorbedIntoUser | — | — |
| Codestral 25.08 | `codestral-latest` | Structured | None | SystemAbsorbedIntoUser | — | — |
| **(Default — fallback)** | _whatever_ | Structured | None | Lenient | — | — |

The default row is the safety net: any slug whose family the user
hasn't picked falls through to it and reproduces today's behavior.

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

## 10. Open questions / future work

- **Streaming semantics for inline tool extraction**: the non-streaming
  path lands in `quirks::apply_to_response` and is wired into every
  provider's `complete()`. The streaming path
  (`complete_streaming`) does not yet run extractors — currently the
  desktop app is non-functional on Qwen3.5 in streaming mode (cloud
  providers stay fine because their `Structured` strategy is a no-op).
  Tracked as the next quirks-system slice.
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
