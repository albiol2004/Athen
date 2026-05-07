# Multi-Intent Routing (Design Doc)

**Status:** Design only — not yet implemented. Tracked as task #152.

A single user message — typically arriving via Telegram — can carry instructions for **multiple arcs at once**. The current pipeline assumes one event = one task = one arc. Lifting that assumption is the natural endpoint of "the user shouldn't have to think about arcs"; this doc captures the design so we don't have to re-derive it later.

## Motivating example

Two notifications buzz the user's phone in quick succession:

1. "Email from Boss: Q3 plan — please reply"
2. "Calendar: Standup with team in 30 min"

The user replies once on Telegram:

> "Reply to him with looks great, ship it. And for the meeting postpone it to Friday."

Today, the whole reply lands on a single arc — whichever one the heuristic guessed — and the agent does either nothing useful or the wrong thing on the other arc.

The right behaviour: split the reply into two intents, route each to its arc, execute independently, return one merged Telegram reply.

## Pipeline shape

```
Telegram owner message
  │
  ▼
Intent splitter (LLM)  ── candidates: recent notifications + active arcs
  │
  ▼
[ {arc_a, "reply to him with looks great, ship it"},
  {arc_b, "postpone the meeting to Friday"} ]
  │
  ▼
Fan out to N tasks, each with its own arc context
  │      │
  ▼      ▼
Risk    Risk    ◄─── Per-arc risk decisions, not one for the whole message
  │      │
  ▼      ▼
Exec   Exec    ◄─── Parallel execution
  │      │
  ▼      ▼
[ outcome_a, outcome_b ]
  │
  ▼
Reply aggregator → single Telegram reply with per-arc bullets
```

## The intent-splitter prompt

Inputs:
- Owner's message text.
- List of recent Telegram notifications (last ~5 min): `(arc_id, arc_name, summary)`.
- List of active arcs the owner has been engaging with (last ~30 min): `(arc_id, arc_name, last_entry_snippet)`.
- Owner's `primary_reply_channel = "telegram"` arcs.

Output:
```json
{
  "split": "single" | "multi",
  "intents": [
    { "arc_id": "<existing-id>", "sub_message": "<verbatim slice>" }
  ]
}
```

For `single`, return a single intent with the original message; the existing single-intent path takes over.

For `multi`, the splitter:
- Slices the original text into N non-overlapping spans (approximate is fine — we don't enforce that the spans concatenate back to the original).
- Maps each span to an existing arc id from the candidates list. Refuses to invent a new arc here — multi-intent is for *existing* threads. (A new-arc intent rolls back to single-intent behaviour: create one fresh arc with the whole message.)
- If any span can't be confidently mapped, the splitter returns `single` and lets the heuristic fall back. We prefer "wrong arc for one merged message" over "right arc for half, missing arc for the other half".

## Risk decisions

Each intent goes through the coordinator independently. This is critical:

- "Reply to him with looks great, ship it" → likely Safe (text reply, known recipient).
- "Postpone the meeting to Friday" → likely Caution or HighRisk (mutates calendar, may need approval).

If the whole message were one task, the highest-risk action would gate the entire batch — the user would be asked to approve the email reply just because the calendar move needs approval. Splitting first keeps risk decisions tight to the action they apply to.

**Edge case: one intent needs approval, another is auto.**
Split outcomes can be:
- Both auto → execute both, merge replies.
- Both need approval → send one Telegram approval card listing both, with per-intent approve/deny.
- Mixed → execute the auto one immediately, send approval for the other. Reply mentions the executed intent's outcome AND the pending approval.

The mixed case is the trickiest — we don't want the user to see "✅ Replied to John, ⏳ awaiting approval for meeting move" and then approve the meeting from a stale context. The approval card needs to be self-contained.

## Reply aggregator

Today's `execute_owner_telegram_message` ends with one Telegram message containing the agent's response + the tools-used footer. With N intents that becomes:

```
✅ Email reply: drafted to Boss, sent.
📅 Calendar: standup moved to Friday 10:00 — invite sent.

Tools used: email_send, calendar_update, contacts_get
```

Per-arc bullets, one merged tools footer (deduplicated across all intents). The arc indicator we added in #149 expands: `📍 Arcs: "Q3 plan", "Standup" — /newarc to start fresh`.

## Failure modes

- **Splitter hallucinates an arc.** Intents reference an arc id not in the candidates list. Validate before dispatch; on mismatch, fall back to single-intent.
- **Splitter misroutes a clause.** "And for the meeting postpone it" lands on Q3-plan instead of Calendar. The `/newarc` command handles full resets; per-intent corrections need a richer affordance — likely an inline keyboard "↶ Move this to a different arc" on each per-arc bullet in the reply. Out of scope for v1.
- **Two clauses contradict.** "Reply to him with yes" + "actually no, tell him no" — the splitter could return both as separate intents on the same arc. The single-arc executor must handle conflicting instructions in the same task description (today's behaviour: it picks one). v1 collapses same-arc intents back into one merged intent before dispatch.

## What v1 explicitly does NOT do

- **Cross-arc data flow.** "Reply to him about the meeting" doesn't pull meeting details into the email reply. Each intent runs in its own arc context with no cross-pollination.
- **Parallel approval orchestration.** Mixed-risk batches send the auto intents immediately and the approval cards sequentially, not in a single multi-pane UI.
- **New-arc creation during multi-intent.** All intents must map to existing arcs from the candidates list.

## Adjacent idea: coordinator-as-agent with standing instructions

The intent splitter above treats the coordinator as a *router* — pure function from message + state to N tasks. The next level up is treating it as an **agent in its own right** with persistent memory and time-bounded standing instructions that bias *every* downstream routing/risk/notification decision until they expire.

Motivating example:

> User about to head into a 4-hour meeting types into the chat composer:
> *"For the next 4 hours, reply to everything on Telegram, and don't auto-send any emails — just draft and queue them for me to approve when I'm back."*

Today this would have to be encoded as configuration changes (notification channel, security mode, …) per-arc. The coordinator-as-agent reads that as a single utterance, persists it as a `StandingInstruction { scope: "all_arcs", expires_at: now + 4h, rules: [...] }`, and applies it to every dispatch / notification / risk decision until 4h elapse.

### What a standing instruction looks like

```rust
struct StandingInstruction {
    id: Uuid,
    created_at: DateTime<Utc>,
    expires_at: Option<DateTime<Utc>>,  // None = until manually revoked
    scope: InstructionScope,            // AllArcs | ArcsMatching{filter} | SingleArc{id}
    directives: Vec<Directive>,
    source_message: String,             // the verbatim user text, for audit
}

enum Directive {
    PreferReplyChannel(ChannelKind),       // "reply on Telegram"
    SuppressAutoSend { categories: Vec<DomainTag> }, // "draft but don't send emails"
    ElevateRiskGate { min_human_confirm: RiskLevel }, // "ping me for anything riskier than Safe"
    SilenceNotifications { except: Vec<NotificationUrgency> }, // "only critical pings"
    QueueForReview,                        // "hold all autonomous actions for me"
    // … extensible
}
```

### Where it plugs into the existing pipeline

```
Sense event
  │
  ▼
Triage  (unchanged)
  │
  ▼
Coordinator decision  ◄─── consults active StandingInstructions before defaulting
  │
  ▼
Risk gate          ◄─── elevated by ElevateRiskGate directives
  │
  ▼
Notification       ◄─── filtered by SilenceNotifications, channel forced by PreferReplyChannel
  │
  ▼
Dispatch / Approval / Reply
```

Every existing decision point gains a "consult standing instructions first" step. Defaults still fire when no instruction matches.

### Risk interaction (the non-obvious part)

Standing instructions can conflict with intrinsic risk decisions in ways that need careful sequencing:

- **`ElevateRiskGate`** *raises* the bar: a Safe action becomes Caution-with-confirm because the user said "ping me for everything today". The coordinator must not silently downgrade — explicit elevation always wins.
- **`SuppressAutoSend`** *also* raises the bar but selectively. The action is allowed to be drafted (executed up to but not including the side-effecting tool call), then queued for approval. Existing risk infra gates the call; the standing instruction just changes the gate's threshold for that specific category.
- **`QueueForReview`** is the strongest: every autonomous action becomes a draft regardless of risk level. Not even Safe actions auto-execute. The user explicitly said "I want to see everything before it goes out" — respect that even when it costs latency.

What standing instructions can *NOT* do: lower a risk gate. A user can't issue "approve everything for the next hour" because that breaks the principle that risk decisions are derived from action, not user mood. (The escape hatch for chronically-over-cautious gates is the security mode in Settings — a deliberate, persistent choice, not a one-line Telegram message.)

### Memory shape

These are first-class entries in `athen_memory`, not config rows. Two reasons:

1. **Audit.** "Why did the agent draft instead of send?" must always trace back to a concrete instruction with timestamp + source message. Memory entries already serialize that cleanly.
2. **Recall.** When the agent is composing a reply, it should *know* that a standing instruction is in effect ("the user is in a 4-hour meeting; this draft is for review later"). That context belongs in the same memory recall path that surfaces user facts today.

Special memory category — `StandingInstructionMemory` — with a TTL filter applied at recall time so expired instructions stop influencing decisions automatically without needing a sweeper.

### Sequencing vs multi-intent

Standing instructions are a separate axis from multi-intent splitting:

- **Multi-intent** = one message → N concurrent tasks routed to N arcs.
- **Standing instructions** = one message → 1 long-lived directive applied to all future tasks for some scope.

A single user message could absolutely do both: *"For the next 4 hours, reply on Telegram. And right now: ack the standup invite and tell Marta the report's coming Friday."* — that's one standing instruction + two multi-intent tasks. The intent splitter prompt needs to be aware of the standing-instruction shape so it can return both kinds of output:

```json
{
  "standing_instructions": [{ "scope": "all_arcs", "expires_at": "...", "directives": [...] }],
  "intents": [{ "arc_id": "...", "sub_message": "..." }, ...]
}
```

### What v1 of this feature would NOT do

- **Conflict resolution between standing instructions.** If the user issues two contradicting ones ("for 4h reply on Telegram", "for 2h reply in-app"), v1 picks the most-recent and warns. v2 could merge / scope by arc.
- **LLM-derived expiry.** "Until I'm back from vacation" needs the agent to track an external state (return date). v1 only honors explicit durations or absolute datetimes.
- **Recursive instructions.** A standing instruction can't issue another standing instruction. Whatever the user typed once is what's in effect.

This sketch lives here because it's the natural extension of "the coordinator is an agent" and shares the same intent-splitter prompt surface as multi-intent routing — building both at once would let them inform each other's design. Tracked separately whenever the underlying multi-intent feature lands.

## Sequencing

The heuristic fix (notification-hint slot, `/newarc`, arc footer in replies — task #149) handles the single-intent case well enough to ship. Multi-intent (#152) goes on top once we see how often the heuristic fails in practice; if 80% of mis-routings are single-message-single-arc, multi-intent is a polish feature, not a fix.

Building both at once would muddle testing — wrong-arc bugs would shadow each other.
