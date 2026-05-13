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

Design doc. Not yet implemented.

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
    /// Tier the task started on. Pinned alongside the provider so per-tier
    /// slug changes in Settings don't flip the model mid-task either.
    pub pinned_tier: Option<ModelProfile>,
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

## Migration

`pinned_provider_id` and `pinned_tier` start `NULL` on every existing arc.
Pinning takes effect on the next task each arc starts; existing in-flight
arcs at deploy time get pinned at their next iteration (already on whatever
provider is active). No backfill needed.

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
