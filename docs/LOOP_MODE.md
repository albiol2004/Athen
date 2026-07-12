# Loop Mode (Design Doc)

**Status (2026-07-11):** SHIPPED Phases 1 + 2 on `feat/loop-mode` (commits
4936497 backend, 23183d7 dual-surface UI), pending merge + a manual smoke
test on each surface. Phase 1 = orchestrator/profiles/arc columns/state
glue/client-only command surface/guardrail tests; Phase 2 = desktop
(`frontend/`) + web (`web/`) trigger dialog, live in-thread progress card,
cooperative Stop, reload/arc-switch re-hydration, Inspector mailbox views,
and the auth-gated traversal-safe `get_loop_file` mailbox read endpoint.
Phases 3–4 below (per-role Bundle picker, restart reconciliation, FollowUp
wake-up generalization) remain open. Code is authoritative; sections below
remain the conceptual reference.

**Phase 4a (2026-07-11) — agent-callable `start_loop` shipped.** The loop can
now be started by the agent from chat ("loop on this until it ships"), not only
by the client button/command, via a fenced, risk-gated `start_loop` agent tool.
Wiring mirrors the `deep_research` tool exactly: an optional `LoopStartRunner`
closure on `AppToolRegistry` (`app_tools.rs`), injected ONLY in the top-level
`assemble_app_tool_registry` (`state.rs`); every sub-agent / worker /
loop-role (`loop_lead`/`loop_evaluator`) / save-only registry gets `None`, so an
agent inside a loop or delegation **structurally cannot start another loop** (the
no-recursion guarantee, now enforced by structural tests, not a blanket
"absent"). The tool reuses `loop_start_core` end-to-end (double-start refusal,
`loop_*` columns, `loop-progress`/`loop-done` events), so the existing web +
desktop cards render agent-started loops with zero UI change. Three fences:
(1) **sense fence** — refuses on any arc whose origin is not `ArcSource::UserInput`
(email/calendar/telegram/system) and fails closed on ambiguity, regardless of
security mode; (2) **risk gate** — Bunker denies outright, Assistant requires an
approval round-trip (approval card via the `ApprovalRouter`), Yolo proceeds
unattended (tool-local Bunker-deny — no other gate denies in Bunker); (3) the
injection-only-at-top no-recursion wiring above. `max_iterations` defaults to 10,
clamped to `[1, 25]`.

**Phase 4b (2026-07-12) — mailbox-collision guards.** Two loops can no longer
corrupt each other's mailbox, and Athen is now wire-compatible with the trio
driver's on-disk lock. (1) **Per-arc mailbox everywhere.** The loop root always
nests a `loop-runs/<sanitized-arc-id>/` subdir — under Code Mode the anchor is
the arc's repo root, otherwise the shared workspace dir. Two Code-Mode arcs
rooted at the SAME repo previously shared one mailbox; now each gets its own.
The resolution lives once in `run_loop_for_arc` (`state.rs`) and is mirrored in
`loop_start_core`'s ack (`commands.rs`); the persisted `loop_root` column stays
the source of truth for `loop_status` / `get_loop_file`, so an arc with an
already-stamped root keeps it. (2) **Mailbox ownership lock**, wire-compatible
with `agent-trio-template/portable/driver.sh`: before seeding, `loop_start_core`
atomically creates `<loop_root>/.lock/` (via `std::fs::create_dir`) holding a
`pid` (this process) + `arc_id` file. A live foreign pid (liveness checked via
`sysinfo`, the `kill -0` equivalent) is refused **synchronously** with the same
in-band `Err(String)` UX as the double-start refusal; a stale (dead-pid) lock is
taken over with a `tracing::warn!`. The lock is a `LoopLock` RAII guard moved
into the run's background task and dropped at task end — releasing on
completion, error, cooperative stop, AND panic (Drop runs during unwind), the
same all-paths discipline as the cancel-flag deregistration. The six-file
`get_loop_file` allow-list never exposes `.lock` / `pid` / `arc_id`.

Loop Mode is a per-arc, UI-triggered **Lead → Evaluator iteration loop** for
goal-shaped work: the user states a goal with acceptance criteria, a Lead
agent plans and implements the smallest verifiable increment, an adversarial
Evaluator independently verifies it, and the cycle repeats until the
Evaluator rules SHIP (ready for human review), BLOCKED (needs a human
decision), or the iteration budget runs out. It is the native, in-app
equivalent of the external `agent-trio-template` bash driver
(`portable/driver.sh` alternating `athen-cli --prompt` one-shots), which
works today but forfeits everything that makes Athen good: model profiles,
the vault, sub-agent fan-out, risk gating, notifications, and any UI.

The design is deliberately thin: Loop Mode is an **orchestrator over
machinery that already exists**, in the exact way Deep Research is. No new
concurrency engine, no new agent kind, no scheduler dependency.

## Why a Lead/Evaluator split (research base, imported)

The template this derives from was audited against practitioner sources
(Karpathy, Cherny, Huntley's Ralph loop, Anthropic's engineering blog) and
the actor-critic literature (Reflexion, self-preference bias). The findings
that shape this design:

- A **fresh-context grader separate from the worker** is the one split you
  must never remove; merging planner into the Lead is fine.
- Same-model judges **over-trust confident reports** → the Evaluator must
  form its verdict from the diff and its own test runs BEFORE reading the
  Lead's report (anti-sycophancy read order).
- Reward hacking is real → mandatory test-integrity audit (deleted/weakened/
  hardcoded assertions = automatic ITERATE).
- Loops oscillate and plateau → failure memory (approaches tried and
  rejected), plateau check from iteration ~4, two identical ITERATE verdicts
  escalate to BLOCKED.
- A weak critic measurably hurts (91.4%→82.8% in one study) → the Evaluator's
  model tier must be ≥ the Lead's.

Athen has one **structural advantage** no CLI harness has: cross-vendor
Bundles. Running the Lead on one vendor and the Evaluator on another
(e.g. DeepSeek lead, Claude evaluator) attacks self-preference bias directly
instead of just prompting around it.

## Core mapping onto existing primitives

| Loop concept | Athen primitive |
|---|---|
| Orchestrator | new `run_trio_loop` in `athen-app/src/loop_mode.rs`, modeled on `deep_research.rs` (pure fn, decoupled from `AppState` via a role-spawn closure + progress callback) |
| One role invocation | one delegated sub-agent via the `run_delegation` seam — own arc, fresh context per role per iteration (the loop's load-bearing property, for free) |
| Lead role | built-in `loop_lead` AgentProfile — full tool groups, may itself call `spawn_subagent` for scout/build fan-out on cheaper tiers |
| Evaluator role | built-in `loop_evaluator` AgentProfile — `ToolSelection::Deny` on `write`, `edit`, `delete_file`, `save_file`: "the grader never fixes" becomes **enforced**, not prompted |
| Goal + acceptance criteria | the existing goal-mode `acceptance_criteria` (risk.rs `TaskSpec`) + `loop/GOAL.md` |
| Mailbox (PLAN/REPORT/VERDICT/STATE/LOG) | plain files under `<root>/loop/` — auditable flight recorder, human-steerable between iterations by just editing them; DB stores only run metadata |
| Verdict | first line of `loop/VERDICT.md` (`VERDICT: SHIP\|ITERATE\|BLOCKED`) stays the human-readable source of truth; the evaluator delegation ALSO returns it as a structured deliverable so the orchestrator never string-parses alone |
| Workspace root | composes with Code Mode: if the arc has `code_mode_root`, that is the loop root (per-arc cwd via `working_dir_override`/`fs_base_override`); else a `Projects/<slug>/` folder |
| Autonomy | wake-up-style **pre-approved capability**: per-loop `AutonomyBand` + iteration cap; the per-action risk gate still fires — an approval request pauses the loop and notifies (tray/Telegram) instead of failing closed |
| Progress UI | per-arc agentic card in the chat thread, like the Deep Research stepper: `iteration N/M — Lead implementing → Evaluator verifying`, last verdict line; PLAN/REPORT/VERDICT open in the right-side Inspector drawer |
| Model tiers | per-role model-profile hints on the two built-in profiles (both default Powerful; workers Fast/Code), overridable per loop in the trigger dialog — cross-vendor via Bundles |

## The loop, concretely

```
start(goal, criteria, root, max_iter, autonomy)      ← UI dialog / Tauri cmd
  write loop/GOAL.md + loop/STATE.md if absent
  for iter in 1..=max_iter:
    lead     = delegate(loop_lead, LEAD_PROMPT)      ← reads GOAL/VERDICT/STATE,
                                                       updates PLAN.md, implements,
                                                       writes REPORT.md
    verdict  = delegate(loop_evaluator, EVAL_PROMPT) ← reads GOAL/PLAN + diff +
                                                       own test runs FIRST, then
                                                       REPORT; writes VERDICT.md
    match verdict:
      SHIP | BLOCKED        → stop, notify user
      ITERATE (same twice)  → coerce to BLOCKED, stop, notify
      ITERATE               → continue
  cap reached               → stop, notify
```

Sequential await chain inside one background tokio task on the loop's arc —
this is strictly simpler than Deep Research (no fan-out at the orchestrator
level; fan-out belongs to the Lead via `spawn_subagent`). The role prompts
ship as embedded templates (same mechanism as the Deep Research worker
briefs), carrying the hardened contract verbatim from the template repo:
anti-sycophancy read order, test-integrity audit, ITERATE-only-on-blocking,
failure memory, plateau check, `DECISION:` flags for human veto.

## Rules

- **NEVER sense-triggered.** Like Code Mode: desktop/web client only. A loop
  is an expensive, standing grant of autonomy; email can't start one.
- **The loop never commits.** It always ends at an uncommitted tree for human
  review (Code Mode's Changes rail + `GixCheckpointStore` undo apply as-is).
- **`loop/GOAL.md` is human-owned.** Agents never edit it; the user editing
  it (or dropping guidance into VERDICT.md) between iterations is the
  steering mechanism, and the next Lead run picks it up for free because
  every role invocation is fresh-context.
- **No recursion.** `loop_lead` may spawn sub-agents; neither role may start
  loops. The agent-callable `start_loop` tool (Phase 4a, shipped) got the
  Deep-Research treatment: an optional `LoopStartRunner` closure injected only
  into the top-level registry, `None` for sub-agent/worker/loop-role/save-only
  registries — so a loop role structurally cannot start another loop.
- **Evaluator tier ≥ Lead tier.** The trigger dialog should warn if the user
  configures it the other way around.

## Phases

1. **MVP (core loop, desktop).** `loop_mode.rs` orchestrator + `loop_lead`/
   `loop_evaluator` built-in profiles (profiles.rs) + embedded role prompts +
   `loop/` file protocol + stop conditions + progress events + `start_loop`/
   `stop_loop`/`loop_status` Tauri commands + chat-thread progress card +
   Inspector views of PLAN/REPORT/VERDICT. Run metadata on `arcs`
   (`loop_state`, `loop_iteration`, `loop_max_iterations`, `loop_root` —
   mirrors the `research_*`/`code_mode*` column pattern).
2. **Parity + reach.** HTTP routes (`POST /api/arcs/{id}/loop`, status/stop) +
   web UI parity; SHIP/BLOCKED/approval-needed notifications through the
   existing channels (tray, ntfy webhooks, Telegram); Telegram `/loop`
   trigger with confirm buttons, mirroring `/deepresearch`.
3. **Tiering + resume.** Per-role Bundle/profile picker in the trigger
   dialog (cross-vendor evaluator); resume-across-restart (state is already
   on disk in `loop/` — the orchestrator re-reads STATE.md/VERDICT.md and
   continues, same property the bash driver has).
4. **Generalization (optional).** A conditional **FollowUp wake-up** trigger —
   "when task X completes, re-dispatch unless `<file first line>` matches
   `<pattern>`, at most N times" — would re-express Loop Mode as scheduler
   vocabulary and give headless daemons the same capability with no UI. Keep
   it out of MVP; it is a superset, not a prerequisite.

## What is deliberately NOT here

- No new concurrency machinery (reuse `run_delegation`; the loop itself is
  sequential by design — the literature validates the pipeline shape).
- No autonomous multi-loop scheduling, no loop-starts-loop.
- No verdict "AI arbiter" above the Evaluator; disagreement resolution is the
  human's job (that's what BLOCKED means).
- The external `portable/` driver stays supported for headless/benchmark use;
  Loop Mode is the desktop-native path, not a replacement.

## Implementation map (when built)

- `crates/athen-app/src/loop_mode.rs` — orchestrator (`run_trio_loop`),
  verdict parsing, stop conditions, prompt templates.
- `crates/athen-app/src/state.rs` — `AppState::run_loop_for_arc` (builds the
  role delegation contexts, wires progress to UiBridge events), mirroring
  `run_deep_research_for_arc`.
- `crates/athen-persistence/src/profiles.rs` — seed `loop_lead` /
  `loop_evaluator` built-ins.
- `crates/athen-persistence/src/arcs.rs` — `loop_*` columns on `ArcMeta`.
- `crates/athen-app/src/commands.rs` + `http_api.rs` — `loop_core` + Tauri
  commands + HTTP routes.
- `frontend/` + `web/src/` — trigger dialog, progress card, Inspector tabs.
- Role prompt source of truth: `agent-trio-template/portable/prompts/`
  (lead.md / evaluator.md) — port verbatim, then adapt the "this harness has
  no subagents" line, which stops being true here.
