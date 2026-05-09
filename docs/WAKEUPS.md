# Wake-ups

**Status:** Implemented (2026-05-09). All planned phases shipped:
core types + scheduler, real coordinator dispatch, autonomy directive,
tool/contact allowlist enforcement, UI panel, sub-agent inheritance,
agent-authored `create_wakeup`. See `docs/IMPLEMENTATION.md` for the
phase-by-phase wiring summary; this doc remains the conceptual
reference for *why* it works the way it does.

Athen is proactive on two axes. **Senses** cover *something happened*
(email arrived, calendar invite, Telegram message). **Wake-ups** cover
*the time has come* — reminders, scheduled digests, recurring jobs,
deferred follow-ups. A wake-up is just a synthetic sense event with a
clock as its trigger; it goes through the existing coordinator path
so risk evaluation, dispatch, and notification all reuse machinery
that already works.

The user creates wake-ups through the UI ("remind me…", "every Monday
8am summarize…"). The agent creates wake-ups through a tool call
("the user asked me to follow up in two hours; schedule it"). Both
produce the same artifact and run the same way.

## Motivating examples

- **Reminder.** "Remind me in 2 hours to check the PR." One-shot
  schedule, instruction = "ping the user about the PR they were
  reviewing in arc <id>", origin = user.
- **Recurring digest.** "Daily 8am: brief me on tech news." Cron
  schedule, instruction = "summarize today's notable tech news; if
  nothing new since yesterday, skip", autonomy = `safe-only`,
  preferred_channel = InApp, origin = user.
- **Weekly job.** "Every Sunday: tally last week's spending from
  email receipts." Cron schedule, instruction declares its own output
  destination ("write to file `~/Documents/expenses.md` and notify
  via InApp"), origin = user.
- **Agent-authored follow-up.** Mid-conversation the agent realizes
  a contractor said they'd reply by Friday. It calls
  `create_wakeup(when="Friday 18:00", instruction="check if Joan
  replied to the contractor email; if not, draft a follow-up")`,
  origin = agent. Goes through the risk gate at *creation*.

## The shape

```rust
struct Wakeup {
    id: Uuid,
    schedule: Schedule,
    instruction: String,             // free-text, like a sense event payload
    autonomy: AutonomyBand,
    preferred_channel: Option<NotificationChannel>,
    tool_allowlist: Option<Vec<ToolId>>,    // None = profile defaults
    contact_allowlist: Option<Vec<ContactId>>, // None = profile defaults; outbound restricted to these
    profile: AgentProfileId,
    arc_id: Option<Uuid>,            // if set, this wake-up resumes/appends to that arc
    origin: WakeupOrigin,            // User | Agent { authoring_arc_id }
    created_at: DateTime<Utc>,
    last_fired_at: Option<DateTime<Utc>>,
    next_fire_at: Option<DateTime<Utc>>, // computed; null if disabled or completed
    enabled: bool,
}

enum Schedule {
    OneShot { at: DateTime<Utc> },
    Cron { expr: String, tz: Tz },                    // e.g. "0 8 * * *"
    Interval { every: Duration, anchor: DateTime<Utc> },
}

enum AutonomyBand {
    Auto,         // run anything the risk system allows; pause only on Critical
    SafeOnly,     // auto-execute below configured risk threshold; pause anything above
    NotifyOnly,   // never act outward; produce output and ping the user
}

enum NotificationChannel {
    InApp,
    Telegram,    // requires Send-Telegram tool; out-of-scope until that lands
    Email,
    Silent,      // result is persisted, no ping
}

enum WakeupOrigin {
    User,
    Agent { authoring_arc_id: Uuid },
}
```

Schedule and instruction are orthogonal. The schedule defines *when*;
the instruction defines *what*. A "preset" (daily news, weekly
expenses) is just a `(Schedule, Instruction, AutonomyBand,
tool_allowlist)` tuple the user saved and re-uses — no special
type, no separate concept.

## Risk model

The hardest design question is **who approves at 3 a.m.** Three
options were considered and rejected:

- **Pre-approve a plan at creation.** LLM drafts the plan, user
  approves once, all future fires are trusted. Rejected: inputs at
  fire time can carry prompt injection the planner never saw. The
  plan is approved; the *content slipping through* is unaudited.
- **Pre-approve everything.** Maximum autonomy. Rejected: same
  injection problem, no defense in depth.
- **Block at fire time on any risk above threshold.** Honest but
  awful UX — agent sits stuck at 3 a.m. waiting for a tap.

The chosen model: **pre-approve capability, not content.**

1. **The per-action risk gate stays exactly as today.** It evaluates
   the actual call with actual args at fire time, which is the only
   place injection actually shows up. We don't trust pre-approval to
   replace per-action evaluation.
2. **Each wake-up declares an `AutonomyBand`** at creation. The band
   determines what happens *when* the risk gate trips:
   - `Auto`: only `Critical` actions pause. Everything else runs.
   - `SafeOnly` (default for agent-authored): below-threshold runs;
     at-or-above pauses.
   - `NotifyOnly`: outbound tools are stripped from the agent's
     surface entirely; the wake-up can read and summarize but cannot
     act on the world.
3. **`tool_allowlist` and `contact_allowlist` at creation are the
   real injection defense.** A "daily news brief" wake-up declares
   read-only tools; injection in scraped content has nowhere to go
   because no outbound channel exists on its tool surface. A
   "Telegram digest" wake-up declares `contact_allowlist =
   [owner_chat_id]`; even if the LLM is convinced to forward the
   summary somewhere, it can't.
4. **Pause does not block.** When the risk gate trips above the
   wake-up's autonomy band, the agent persists its partial work
   (existing arc + checkpoint machinery) and exits. The pending
   approval lands in the same in-app approval queue used for
   foreground work. Next time the user opens Athen — or next time
   the dispatcher decides to escalate via Telegram per the existing
   approval router — they see it.

Agent-authored wake-ups go through the risk gate **at creation
time**, scoring the *instruction text + declared allowlists*. A
wake-up whose declared scope is risky (e.g. "send money to X every
month") is not approved-at-fire; it's approved-at-creation, and the
user agrees to the recurring capability when they say yes.

## Catch-up policy

If Athen was off when a wake-up should have fired:

- **Coalesce per-schedule, not globally.** Three missed daily-news
  fires (laptop closed all weekend) collapse into one task whose
  instruction reads "catch up on news from <last_run> to now,
  briefly." Two *different* wake-ups (news + expenses) that both
  missed run as two independent tasks; they have nothing to do with
  each other.
- **One-shots that missed their `at`** run once on next wake, with
  the original instruction unchanged. The user's mental model of
  "remind me at 5pm" tolerates "at 5:12pm because the laptop just
  woke up"; it does not tolerate "silently dropped because I was
  late."
- **A configurable `max_lateness` per schedule** allows
  time-sensitive wake-ups ("nudge me before the meeting") to skip
  catch-up. If the meeting already happened, don't fire.

The dispatcher computes `next_fire_at` after every fire and on
process startup. Startup recomputation is what implements catch-up.

## Visibility surface

Wake-ups need a dedicated tab in the UI alongside Memory and
Calendar. Without it, agent-authored schedules are invisible debt —
the user doesn't know what's queued, can't audit, can't cancel.

Required columns: instruction (truncated), next fire, schedule expr,
origin (user / agent + originating arc link), autonomy band,
enabled toggle, last fire result. Inline edit for instruction and
schedule. Cancel button. Filter by origin.

When the agent creates a wake-up, the originating arc gets a small
inline marker: "→ scheduled wake-up #N for <when>" with a click-
through to the wake-ups tab. This is the same pattern as the
calendar event marker we already render.

## Output destination

Output destination is **part of the instruction**, not a separate
field. The agent has tools (Write file, Send email, Telegram, append
to arc) and decides where the result goes based on what the
instruction says — exactly as it does for foreground tasks. The
`preferred_channel` field is for the *completion ping*, not for the
work itself: "tell me when you're done, on InApp." A user who wants
the digest itself delivered via Telegram says so in the instruction
("…and post the summary to my Telegram") and the Send-Telegram tool
handles it.

This keeps wake-ups symmetric with sense-driven tasks. A daily-news
wake-up and a "summarize this email I just got" sense event run
through the same execution path.

## What's intentionally not in v1

- **Conditional pre-checks** ("only run if unread > 0"). The agent
  can implement the check inside its instruction
  ("…if there's nothing notable, skip and don't ping me"). A
  separate predicate primitive is more machinery than the use case
  needs yet.
- **Chained schedules** (wake-up B fires N hours after wake-up A
  completes). Express as a one-shot wake-up created by A on
  completion. Same effect, less surface area.
- **Cross-device sync**. Wake-ups live on the device that authored
  them. If the user has Athen on two laptops, schedules don't
  replicate. Solved when (if) device sync lands.
- **Saved presets in a dedicated UI**. The UX story is just "user
  edits an existing wake-up's schedule" or "user duplicates and
  tweaks." Adding a preset library is a follow-up, not v1.

## Dependencies

- **Send-Telegram tool** (separate task). Several digest use cases
  want Telegram as the output channel. Wake-ups can ship without
  it — the channel is just unavailable until that tool lands —
  but the two are natural neighbours.
- **Auditor-based loop cap (#166).** A weekly digest legitimately
  takes longer than a foreground task. The current iteration cap
  will fight this. Wake-ups can ship with the existing cap, but
  expect to bump #166 forward soon after.
- **Tool-call parameter visibility (#167).** When wake-ups run
  unattended, the user reviews what happened by reading the arc
  later. If tool-call args are hidden in the UI, wake-up audits are
  blind. Fix #167 before declaring wake-ups done.

## Open questions

- **Cron expression UX.** Power users want raw cron; most users
  don't. Probably ship a small set of presets ("daily at HH:MM",
  "every Monday at HH:MM", "every N hours") plus an "advanced"
  field for raw cron. Match how Reminders apps handle this.
- **Risk-gate UI for agent-authored wake-ups.** When the agent
  proposes a wake-up via tool call, does the user see the
  instruction text + declared allowlists in a dialog and approve
  the whole bundle? Probably yes — same shape as today's risky-
  action approval, just with "this will happen on schedule X"
  appended.
- **Re-arming on partial failure.** If a fire pauses for approval
  and the user denies, does the schedule re-arm for the next slot
  or disable itself? Probably re-arm; one denied fire is not a
  signal to kill a recurring schedule. But cap consecutive denials
  → auto-disable + notify.
