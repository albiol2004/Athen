# Bundles

Per-tier model loadouts. A Bundle is a named set of `(provider, slug)` picks — one per `ModelProfile` tier — that the user can switch between as a single unit. Replaces the current "active provider + per-tier slug overrides" coupling with a flatter "active Bundle" model that natively supports cross-vendor mixing.

**Status:** Design only. Not yet implemented. Builds on shipped [Provider Pinning](PROVIDER_PINNING.md) (2026-05-23 fix made pinning load-bearing for routing, not just compaction/temperature — this design depends on that).

**Sequencing:** Bundles is a Settings-layer rework + a small data-model change. The hard parts (per-arc pinning of (provider, slug), per-slug quirks dispatch inside one provider) already shipped via [PROVIDER_PINNING](PROVIDER_PINNING.md) and the OpenCode Go merge (task #254). The remaining work is mostly UX + persistence + a quirks registry.

## Problem

Today's Settings → Models panel asks for a single "active provider" with:
- a `default_model` slug
- a `family` (wire-format / quirks preset)
- a `tier_models: HashMap<ModelProfile, String>` of per-tier slug overrides

The wire format is locked at the provider row. Slugs the relay knows about under a different wire format (e.g. OpenCode Go relays both DeepSeek/Kimi via OpenAI shape AND MiniMax via Anthropic shape) cannot coexist in one provider entry. The OpenCode Go merge (#254) fixed that case via per-slug internal dispatch inside one logical provider, but the underlying confusion remains for genuinely cross-vendor configs:

- User has API keys for OpenAI + Anthropic + DeepSeek. They want Cheap=DeepSeek, Code=Anthropic, Powerful=OpenAI. No clean way to express that today. `tier_models` is intra-provider only.
- "Family preset" at the top of the form implies one wire format. Letting an advanced section override that per tier would silently contradict the preset.
- Mental model leak: users think in terms of vendors ("I want to use Claude"), not in terms of "active provider + family + per-tier slug overrides."

## Mental model

Two orthogonal concerns, two separate UI surfaces:

1. **Connections** — "what credentials do I have?" Provider + endpoint + API key + sane defaults. Pure credential management. No model picking happens here.
2. **Bundles** — "for this run, which providers and slugs do I want each tier to pull from?" A Bundle references one or more Connections and binds each `ModelProfile` tier to a `(connection_id, slug)` pair.

The active Bundle is a singleton (one bundle is "live" at a time). Per-arc Bundle override is a future axis (analogous to today's per-arc `tier_override`).

## Data model

### New types in `athen-core`

```rust
/// A named set of per-tier (connection, slug) picks. One active at a time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bundle {
    pub id: Uuid,
    pub name: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Per-tier picks. Sparse — a missing tier falls back to a tier-adjacent
    /// pick (Code falls back to Fast; Fast to Cheap; Powerful to Fast).
    pub tiers: HashMap<ModelProfile, BundleTier>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleTier {
    /// References a Connection row (renamed from today's ProviderConfig).
    pub connection_id: String,
    /// Wire-format model slug to send. May be a curated catalog entry or
    /// a user-typed Custom slug.
    pub slug: String,
}
```

### `ProviderConfig` rename → `Connection`

The existing `ProviderConfig` (in `crates/athen-core/src/config.rs`) becomes `Connection`. Field-level changes:

- **Remove `tier_models`.** This responsibility moves to Bundles. Persisted maps migrate into a synthesized "Default" Bundle on first load (see Migration).
- **Remove `default_model`** (or repurpose as "preferred slug" — the value that pre-populates Bundle dropdowns when this Connection is added without a tier already filled).
- **Remove `family`.** Wire-format quirks move to the per-slug registry (next section). Connections become pure credentials. (Exception: providers like OpenCode Go that need per-slug dispatch inside one Connection keep that internal logic — already shipped.)
- **Keep:** endpoint, api_key reference, supports_vision, supports_documents, default per-Connection sampling temperature.

### `AthenConfig.models` shape change

- `models.providers: HashMap<String, ProviderConfig>` → `models.connections: HashMap<String, Connection>`
- New `models.bundles: HashMap<Uuid, Bundle>`
- `models.assignments["active_provider"]` → `models.assignments["active_bundle"]` (UUID string)

### Per-slug quirks registry

Today, `ModelFamily` is the per-Connection quirks selector. Under Bundles, the slug is the granular unit:

```rust
pub struct SlugQuirks {
    /// Wire-format adapter family (existing ModelFamily enum).
    pub family: ModelFamily,
    /// Reasoning effort default, if the slug has a sensible one (e.g.
    /// "deepseek-v4-pro" defaults to thinking-on).
    pub default_reasoning: Option<ReasoningEffort>,
    /// Curated badge for the model picker — "recommended", "preview",
    /// "deprecated", or None.
    pub catalog_label: Option<CatalogLabel>,
}

pub trait SlugQuirksRegistry {
    /// Look up quirks for a (connection, slug) pair. Connection scope
    /// matters because the same slug (e.g. "claude-sonnet-4-6") can live
    /// behind multiple Connections (anthropic-direct + opencode-relay).
    fn lookup(&self, connection_id: &str, slug: &str) -> SlugQuirks;
    
    /// Default fallback for Custom slugs — provider's vanilla family.
    fn vanilla_for(&self, connection_id: &str) -> SlugQuirks;
}
```

A static `BUILTIN_QUIRKS: &[(connection_pattern, slug_pattern, SlugQuirks)]` table lives in `athen-llm/src/quirks/seed.rs` (extends today's tiny seed). Order matters — first match wins. Missing entries fall back to `vanilla_for(connection_id)`.

## Resolution flow

```
arc has pin?
├── yes → resolve to (pinned_connection_id, pinned_slug)   ← already wired
└── no  → tier = resolve_effective_tier_for_arc(...)         ← already wired
         bundle = active Bundle (or per-arc Bundle override)
         (connection_id, slug) = bundle.tiers[tier]
                                  .or(bundle.tiers[fallback_tier])
                                  .or(error to user — "no Cheap tier set")
         install pin(connection_id, slug)                    ← already wired
         build router for (connection_id, slug)              ← already wired
```

All four steps after "no pin" are infrastructure that already exists. The Bundles work changes step 2 input ("read from active Bundle, not active Connection's tier_models") and step 3 dispatch ("look up quirks for (connection, slug), not for the Connection alone").

## Settings UI structure

```
Settings
  ├─ Connections          ← credential CRUD
  │   ├─ Add Connection (provider preset picker, like today's PROVIDER_IDS)
  │   ├─ Per-row: endpoint, api key (vault-backed), test-connection button
  │   └─ Per-row: supports_vision / documents toggles, sampling temp default
  ├─ Bundles              ← tier loadouts
  │   ├─ Active Bundle dropdown (header)
  │   ├─ Per-Bundle card: name, per-tier rows (Cheap / Fast / Code / Powerful)
  │   │   └─ Each tier row: Connection dropdown + Slug dropdown (live-fetched
  │   │      from /models when available + hardcoded curated catalog)
  │   ├─ Per-tier "Custom slug" escape hatch (user types; warning badge
  │   │   if quirks registry has no entry; optional Probe button)
  │   └─ "Duplicate Bundle" / "Delete Bundle" actions
  ├─ Profiles             ← unchanged (coder/devops/etc, personas + tool tiers)
  ├─ Identity             ← unchanged
  ├─ ...
```

Non-power users see one auto-created "Default" Bundle covering their existing config and never have to look at the Bundles panel. Power users create named Bundles ("Cloud Premium", "Free Tier", "Local Only") and switch between them via the header dropdown.

## Model catalog

Bundle tier dropdowns populate slugs from:

1. **Live fetch** of the Connection's `/models` (or equivalent) endpoint when available. Cached per-Connection for ~1h. Providers known to expose this: OpenAI, Anthropic, OpenRouter, Mistral, DeepSeek, Groq.
2. **Hardcoded curated catalog** in `athen-llm/src/quirks/seed.rs` per Connection preset. Always shown. Athen vouches for quirks on these.
3. **Custom slug** input — last resort, badge "Custom — quirks not validated."

The dropdown UI separates them visually:

```
┌─────────────────────────────────┐
│ ⭐ Recommended                  │
│   - deepseek-v4-pro             │
│   - deepseek-v4-flash           │
├─────────────────────────────────┤
│ Available (live)                │
│   - deepseek-coder-v3           │
│   - deepseek-reasoner-v1.2      │
├─────────────────────────────────┤
│ + Custom slug...                │
└─────────────────────────────────┘
```

Live-fetch failure (offline, key rate-limited) is non-fatal — UI falls back to hardcoded list silently.

## Migration

Run on first load after upgrade in `athen-core::config_loader::load_config`:

1. If `models.bundles` is empty:
   - Read `models.assignments["active_provider"]` → e.g. `"opencode_go"`
   - Read that Connection's old `tier_models` map
   - Synthesize one Bundle named "Default" with `tiers[tier] = BundleTier { connection_id: active_provider, slug: tier_models.get(tier).unwrap_or(default_model) }` for each ModelProfile present.
   - Mark this Bundle's UUID in `models.assignments["active_bundle"]`.
2. Rename `models.providers` → `models.connections` (TOML key rewrite on save).
3. Drop `tier_models`, `default_model`, `family` from each Connection row (or migrate `default_model` → Connection.preferred_slug).
4. Log one `info!` per migration run summarizing the synthesized Bundle.

Round-trip safety: existing users observe zero behavioural change on first launch. The Bundles panel shows their "Default" bundle pre-populated.

## Edge cases

### Connection deleted while in-flight arcs are pinned to it
Pinning resolver currently warns and falls back to active. Under Bundles, falling back to "active bundle's slug for this tier" may be a different Connection entirely — exact hazard pinning was built to prevent.

**Fix:** Block Connection deletion if any non-idle arc is pinned to it. UI surfaces the count: "3 arcs are pinned to OpenAI — finish or stop them first." Idle arcs holding stale pins are harmless (pin clears on next task boundary).

### Bundle tier removed mid-arc
Arc pinned to `(openai, gpt-5)` keeps running on `gpt-5` even if the user edits the Bundle to drop the Cheap tier. Pinning shields the arc; the Bundle edit affects only new arcs and the resolver path for non-pinned arcs.

### Sparse Bundle tiers (user only sets Cheap + Powerful)
Fallback ladder: Code→Fast→Cheap, Powerful→Fast→Cheap, Fast→Cheap, Cheap→error. Bundle must have at least one tier set; UI enforces. Sparse Bundles are valid configs — they just mean "I don't care to distinguish."

### Custom slug, no quirks registered
Vanilla family for the Connection. UI badge "Custom — quirks not validated." Optional `Probe` button (sends a 1-turn smoke: tool call + reasoning round-trip) — green check on success, red error with provider response on failure. Without Probe, the first real arc to hit it eats the wire failure.

### Switching active Bundle mid-arc
Safe by construction. Pin holds the in-flight arc's (Connection, slug); new arcs use the new Bundle. UI may show a brief toast: "Bundle switched — 2 in-flight arcs continue on their pinned models."

### Two Bundles reference the same Connection
Fine. Both bundle definitions are persisted independently; only the active Bundle's tier picks are consulted on a non-pinned resolution. No reference counting needed.

### Wake-ups crossing Bundle switches
Wake-up fires → fresh arc → uses currently-active Bundle. User intent at schedule time may have been Bundle A. Two options: (a) capture `bundle_id` on the wake-up row at schedule time and prefer it over active; (b) accept drift, treat wake-ups like any new arc. Default to (b) — wake-up logic shouldn't be coupled to model choice; user expectations of "now" Bundle being live are simpler.

### Bundle deletion (the active one)
Block in UI. Force the user to pick a different active Bundle first, then allow deletion.

### Embedding provider
Stays orthogonal in its own Settings → Embeddings panel. Bundles cover *chat/agent* model routing only.

## Open questions

- **Naming.** "Bundles" is the working name (consistent with [[project_per_task_model_selection]]). Alternatives: Packs, Loadouts, Lineups. Decide before UI ships — name leaks into help text and onboarding.
- **Per-profile Bundle override.** Some users may want "the coder profile always uses Bundle X". Adds a precedence layer (per-call > per-arc > per-profile > active). Defer; start simple.
- **Connection-level reasoning effort default vs Bundle tier-level vs per-arc.** Today Reasoning Effort design (see [REASONING_EFFORT.md](REASONING_EFFORT.md)) lives on the Connection. Should it move to the BundleTier (so "Code tier always uses High reasoning regardless of model")? Probably yes for clean orthogonality, but more migration weight. Decide alongside the REASONING_EFFORT.md implementation.
- **Bundle export/import.** Useful for "share my model setup with a friend" and for fixture configs in benchmarks. JSON blob; redact api_key references (they're vault-keyed, not embedded).
- **Connection probe in Connections panel.** Test-connection button (already exists in some panels) + 1-turn LLM probe to validate the credential AND the wire format. Reuse the Custom-slug Probe affordance.

## Out of scope (for v1)

- Cross-Bundle inheritance / template Bundles
- Bundle versioning / undo
- Auto-fallback Bundle when active Bundle's Connection is offline (the existing failover/circuit-breaker covers per-call failure within one Connection; Bundle-level swap on outage is a future feature)
- LLM-driven Bundle suggestions ("you mostly do code work; want a code-tuned Bundle?")
- Per-Bundle budget caps

## Related

- [PROVIDER_PINNING.md](PROVIDER_PINNING.md) — the safety substrate Bundles depends on
- [PER_MODEL_QUIRKS.md](PER_MODEL_QUIRKS.md) — quirks taxonomy that becomes per-slug under Bundles
- [REASONING_EFFORT.md](REASONING_EFFORT.md) — coordinate Bundle tier reasoning defaults with the effort design
- [CONFIGURATION.md](CONFIGURATION.md) — current "active provider + tier_models" structure being replaced
- [[project_per_task_model_selection]] — memory that flagged the Bundle direction
- [[project_provider_pinning_design]] — memory tracking the pinning fix that unlocked this design
