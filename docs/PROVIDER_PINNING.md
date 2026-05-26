# Provider Pinning for In-Flight Arcs

When the user switches the active provider while an arc has a task in flight,
the next executor iteration rehydrates messages authored under provider A's
quirks and feeds them to provider B's wire format. The rehydration breaks in
provider-specific ways:

- **Anthropic → anything else**: prior `thinking` content blocks carry
  encrypted `signature` fields. Providers without thinking either reject the
  block or silently drop reasoning continuity. Multi-turn cache breakpoints
  invalidate too.
- **Gemini 3 → anything else**: prior `functionCall` parts carry
  `thoughtSignature` (see [[feedback_gemini_thought_signature]]). Stripping it
  loses state; forwarding it to OpenAI/DeepSeek is HTTP 400.
- **Local Qwen/Gemma → cloud**: `<think>...</think>` inline tool calls
  (see [[feedback_quirks_scan_reasoning_content]]) get re-parsed as content
  on cloud providers that don't recognise the tags.
- **OpenAI Responses API → Chat Completions**: reasoning_items have no
  cross-API mapping; switching strips them and forces re-thinking.
- **Tool-call shape drift**: same JSON tool-call gets re-parsed under a
  different family's `ToolExtraction` quirk (see [PER_MODEL_QUIRKS.md](PER_MODEL_QUIRKS.md)).

The user's intent when switching active is "from now on, new arcs use the new
provider." Breaking in-flight arcs is collateral damage, not the goal.

**Status:** SHIPPED end-to-end (2026-05-23). Pin store + resolver + per-arc router all load-bearing. Earlier landing (2026-05-19) only plumbed the pin into compaction/temperature lookups — actual per-arc routing wasn't load-bearing until the 2026-05-23 fix.

Live today:
- `ArcMeta.pinned_provider_id` + `ArcMeta.pinned_slug` columns at `crates/athen-persistence/src/arcs.rs` (~lines 115/123), with `init_schema` migration that `ALTER TABLE`-adds them to legacy DBs (~lines 342-348).
- Lifecycle methods `set_pinned_provider_if_unset` (~line 690, idempotent on first call) and `clear_pinned_provider` (~line 719).
- `EffectiveProviderTarget { provider_id, pinned_slug }` at `crates/athen-app/src/state.rs` ~line 3705. `resolve_effective_provider_for_arc_with_config` (~line 3748) reads the active Bundle's tier pick, installs the pin on first call, and returns the resolved target.
- `arc_router_for` (~line 4145): fast-paths to the shared global router when no pin is in force; otherwise calls `build_router_for_provider_from_config_with_pinned_slug` to build a per-arc router with `override_slug` collapsing every tier to the pinned slug.
- `build_router_for_bundle` (~line 4469): builds the global router from the active Bundle's tier picks; called on Bundle switch.
- Unit-tested: `test_pinned_provider_lifecycle` and `test_pinned_provider_migration_on_legacy_db` in `arcs.rs`; `override_slug_collapses_every_tier_to_pinned_key` and `empty_override_slug_treated_as_none` in `state.rs`.

Still pending (post-ship gaps):
- UI surface (arc-card "pinned: X" badge, manual unpin in arc settings) — backend pin is silent today; no frontend indicator.
- `pinned_slug` is captured and routing-effective, but the warn-on-slug-drift path (log + surface in UI when same `(provider, tier)` re-resolves to a different slug) is not yet wired.
- User-set durable `ArcSettings.preferred_provider_id` (the intent override, distinct from the protective pin) — not implemented.

The design doc below remains the reference for lifecycle rules and edge-case decisions.

## Today's behaviour (the bug)

`AppState.active_provider_id: Mutex<String>` is snapshotted at every LLM call
site (executor turn, judge, memory extractor, etc.). Mid-turn switching is
fine because a single call holds its snapshot, but **cross-turn switching
within one task hits a different provider on the next iteration**. A task with
3 tool calls = 4 LLM calls = 4 independent snapshots — any switch between
them flips mid-flight.

## Mechanism: arc-level pin

Add a transient pin field to the arc row:

```rust
// athen-core/src/types.rs (or athen-persistence)
pub struct ArcRow {
    // ... existing fields ...
    /// Provider this arc is currently locked to. `None` = follow global
    /// `active_provider_id`. Set when a task starts, cleared when the task
    /// completes. Distinct from a user-set per-arc override (which would be
    /// a separate, durable field on `ArcSettings`).
    pub pinned_provider_id: Option<String>,
    /// Concrete model slug that resolved on the first LLM call. Captured
    /// so a future "warn on slug drift" path can flag the case where the
    /// user edits `tier_models` mid-task and the same `(provider, tier)`
    /// pair now resolves to a different slug. Today the column is stored
    /// but routing only consults `pinned_provider_id`.
    pub pinned_slug: Option<String>,
}
```

### Lifecycle

1. **Pin set**: at the *first* LLM call of a task. If the arc was idle and a
   sense event / user message wakes it up, the call site reads the current
   `active_provider_id`, writes it onto the arc, and uses it.
2. **Pin honoured**: every subsequent LLM call within that task reads
   `arc.pinned_provider_id` first; falls through to `active_provider_id`
   only when None.
3. **Pin cleared**: when the executor returns a final answer with no pending
   tool, no `CONTINUE` from the completion judge, no scheduled wake-up
   created by this task. Effectively: when the arc transitions back to idle.

### What counts as "task start" vs "still the same task"

- New user message into an idle arc → new task → re-pin (may switch if user
  switched active in the meantime).
- Approval coming back for a pending tool → same task → pin stays.
- Wake-up fires inside the arc → same task → pin stays (the wake-up was
  authored by the original provider's reasoning; honour its choice).
- Sense event routed into the arc → ambiguous; lean toward **new task**
  (provider may have been switched intentionally between events).

## Surface (UI)

- **Arc list badge** when `pinned_provider_id != active_provider_id`: small
  "pinned: OpenAI" chip on the arc card. Clicking opens the override menu.
- **Active-provider switcher**: when switching, *don't* warn — pinning is
  silent and Just Works. Don't nag the user with "you have N in-flight
  arcs that won't switch."
- **Manual unpin** (advanced, in arc settings): "Switch this arc to active
  provider now." Big warning that rehydration may break. For users who
  knowingly accept the cost.

## What this is NOT

Not the same as the user-set per-arc override discussed in
[[project_per_task_model_selection]] (Road 2). That override expresses
*intent*: "this arc should always use Claude." Pinning expresses *protection*:
"don't break my in-flight state."

Resolution order with both in play (highest precedence first):

1. `delegate_to_agent` call-time `provider` param (if ever added)
2. User-set `ArcSettings.preferred_provider_id` (durable, intentional)
3. `ArcRow.pinned_provider_id` (transient, protective)
4. Global `active_provider_id`

So if a user explicitly pinned an arc to Anthropic via settings, that wins
even if a different provider was active at task start.

## Edge cases

- **Pinned provider deleted from config**: arc falls back to global active,
  rehydration may break. Log a warning; surface in arc as "pin lost". Don't
  block the user from deleting providers.
- **Pinned provider's API key removed but provider entry kept**: router fails
  the call; current failover chain catches it. Pin stays (user might re-add
  the key); next call retries.
- **Provider's `family` quirks changed mid-task** (user switched the family
  dropdown without switching active): treat as same-provider, accept the
  drift. The family dropdown is a power-user knob and toggling it mid-task is
  rare. Document the foot-gun.
- **Compaction**: ran by whoever the compaction call's provider was. If
  compaction runs on the pinned provider, no issue. If a future tier-routing
  change makes compaction route elsewhere, audit then.

## Why `pinned_slug` and not `pinned_tier`

An earlier landing of this feature stored `pinned_tier` (the `ModelProfile`
label of the first call: `Cheap` / `Fast` / `Code` / `Powerful`). That was
the wrong primitive for two reasons:

1. **Different call sites within one task legitimately use different
   tiers**. The executor turn labels itself `Code`; the completion judge
   labels itself `Cheap`; the memory extractor labels itself `Cheap`.
   Pinning the tier at task start and forcing every subsequent call onto
   that tier would 3× the bill for no benefit.
2. **The thing that breaks rehydration is the slug change, not the tier
   change**. If Settings has Code → `deepseek-v4-pro` at task start and
   the user edits it to Code → `qwen3-coder-next` mid-task, the next
   "Code" call lands on a different family with different quirks (think
   tags, tool-call extraction) and rehydration of prior turns breaks.

So the right pin is the *resolved slug*, captured on first call. Today
it's diagnostic-only (routing still flows through current `tier_models`);
the warn-on-drift path is the natural follow-up — when `(provider, tier)`
re-resolves to a slug ≠ `pinned_slug`, log + surface in UI but don't
silently downgrade.

## Migration

`pinned_provider_id` and `pinned_slug` start `NULL` on every existing arc.
Pinning takes effect on the next task each arc starts; existing in-flight
arcs at deploy time get pinned at their next iteration (already on whatever
provider is active). No backfill needed.

Dev DBs that ran the earlier `pinned_tier`-bearing migration get the column
dropped via `ALTER TABLE ... DROP COLUMN pinned_tier` (SQLite 3.35+). On
older SQLite runtimes the drop silently no-ops and the dead column lingers
— readers don't reference it, so it's harmless.

## Implementation footprint

Small.

- `ArcRow`: 2 new nullable columns + serde defaults. ~10 lines + migration.
- Executor entry point: read `arc.pinned_provider_id`, fall through to
  active. Write the pin on first call of a task. ~15 lines.
- Executor exit: clear pin when arc transitions to idle. ~10 lines.
- Frontend: badge on arc card + advanced unpin button. ~30 lines.

Total: ~70 lines. The pin is the easy part; the *task-boundary detection* is
the real work — which executor branches count as "task completed". Audit
those before writing code.

## Open questions

- **Failover within a pinned arc**: if the pinned provider goes down, do we
  honour the existing failover chain (and accept potential rehydration
  breakage), or hard-fail and let the user retry? Lean toward failover —
  rehydration breakage is recoverable, a stuck arc isn't.
- **Should the pin survive a process restart?** Yes — it's persisted on
  `ArcRow`. If Athen crashes mid-task and restarts, the pin re-applies.
- **Pin in delegate_to_agent**: when an agent delegates a sub-task, does the
  sub-task inherit the parent's pin? Lean yes — sub-task is the same arc.
