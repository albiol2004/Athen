# Identity (Design Doc)

**Status:** Shipped (storage, UI, prompt injection, agent-write tool, as of 2026-05-19).

Athen needs a place for the user to write down, by hand, who Athen is and
what it knows: personality, rules, facts about the user and their team,
coding preferences, anything that should be true *across every agent*.
This is the "soul" of the assistant — distinct from per-task memory,
distinct from per-profile system prompts, and distinct from auto-learned
episodic memory.

OpenClaw and similar projects ship fixed files (`SOUL.md`, `AGENT.md`,
`KNOWLEDGE.md`). We're going further: **categories are user-editable**.
A user who wants a `coding_style` category, or a `relationship` category,
or `medical_history`, just adds it. The four seeds we ship are starting
points, not a closed enum.

## Motivating examples

- **Personality coherence across agents.** The personal-assistant agent
  replies to your sister with the same warmth as the outreach agent
  replies to a stranger — because both read the same `personality`
  section. The coder agent does not load that section (waste of
  tokens), but if you ever ask the coder to write a Slack bot, the
  voice it generates matches.
- **Hard rules that survive profile switches.** "Never auto-send email
  to legal@. Always confirm." goes in `rules`, not in any one agent's
  prompt. Switching from personal-assistant to outreach mid-arc cannot
  drop the rule.
- **Team belonging (business).** "I work at Tonus AI; CTO is Marta;
  escalation chain is Marta → Pere → board." goes in `team`. Every
  agent that drafts external messages reads it; the home-personal-
  assistant profile doesn't (irrelevant overhead).
- **User-invented categories.** Alex adds a `coding_style` category:
  "Rust prefer tracing over println, never `unwrap()` in non-test
  code, always `#[async_trait]` on traits." Tagged `applies_to:
  [coder]`. The personal-assistant never sees it.

Memory, by contrast, is *episodic* and *auto-recalled per-query* —
"user's mother's name is Inés", "the project Atlas is paused since
March", "Boss's timezone is UTC+2". Identity is *stable* and
*always-on for matching profiles*. They share storage characteristics
(SQLite, hand-editable) but live in different tables and feed the
prompt at different stages.

## The shape

Every entry is `(category, body, applies_to)`:

```rust
struct IdentityEntry {
    id: Uuid,
    category: String,             // "personality", "rules", or anything user-named
    body: String,                 // free-text markdown
    applies_to: Vec<ProfileTag>,  // [Always], [Profile("coder")], [Profile("outreach"), Profile("personal_assistant")]
    pinned: bool,                 // user-flagged "always include even if budget is tight"
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

enum ProfileTag {
    Always,                       // every agent reads this
    Profile(String),              // only agents with this profile id
    NotProfile(String),           // every agent EXCEPT this one (rare; kept for "never_coder")
}

struct IdentityCategory {
    name: String,                 // primary key — must be unique per user
    description: String,          // user's note about what goes here
    default_applies_to: Vec<ProfileTag>,  // suggested tag for new entries in this category
    order: u32,                   // display order in UI
    is_seed: bool,                // shipped by Athen; can be renamed/deleted but flagged
}
```

Categories are first-class and user-editable. The four seeds Athen
ships are:

| Category | Description | Default `applies_to` |
|---|---|---|
| `personality` | Voice, warmth, refusal style, humor level | `[Always]` (often refined to specific profiles) |
| `rules` | Hard constraints — "never X", "always Y" | `[Always]` |
| `knowledge` | General facts and recurring contexts — projects, tools, places, anything that's not specifically about you as a person | `[Always]` |
| `user` | Personal facts about you — relationships, family, preferences, hobbies, dietary, location. The agent adds entries here as it learns about you | `[Always]` |
| `team` | Org chart, business identity, escalation chain | `[Profile("personal_assistant"), Profile("outreach")]` |

The user can rename them, delete them, add new ones, reorder them.
Renaming or deleting a seed category is allowed but shows a confirm
("This category was suggested by Athen — keep it?"). Re-creating it
later just makes a new user category with the same name; the seed
flag does not return.

## Storage

A new SQLite table in `athen-persistence`, **separate from
`arc_entries` and from the memory store**:

```sql
CREATE TABLE identity_categories (
    name TEXT PRIMARY KEY,
    description TEXT NOT NULL DEFAULT '',
    default_applies_to JSON NOT NULL,
    sort_order INTEGER NOT NULL,
    is_seed BOOLEAN NOT NULL DEFAULT 0
);

CREATE TABLE identity_entries (
    id TEXT PRIMARY KEY,
    category TEXT NOT NULL REFERENCES identity_categories(name) ON DELETE CASCADE,
    body TEXT NOT NULL,
    applies_to JSON NOT NULL,
    pinned BOOLEAN NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE INDEX idx_identity_entries_category ON identity_entries(category);
```

`applies_to` as JSON keeps the schema simple while preserving
queryability — the prompt builder reads everything for a given profile
in one query. Alternative (junction table) is over-engineered for v1.

## Agent-write

The agent can add identity entries via the `identity_add` tool
(`crates/athen-app/src/app_tools.rs`). It accepts `category`, `body`,
and an optional `applies_to` (default `["Always"]`). New entries are
persisted with `proposed_by_agent: true` and become live in the prompt
prefix immediately — the same arc that learned a fact about the user
can use it on the very next turn. The chat tool-call card serves as
the notification: there is no approval prompt, no risk gate, no
deferred review queue. Settings → Identity surfaces a small
"added by agent" chip on each `proposed_by_agent` entry with a
one-click dismiss; for the `rules` category the chip is louder
("New rule — review") because adding a rule has cross-arc blast radius.
The agent never edits or deletes existing entries — only adds — so the
user retains full ownership.

## Where it plugs into the prompt

Identity sits in the **static header**, after the agent's system prompt
and before tools. This matches the cache-friendly layout described in
`docs/ARC_COMPACTION.md` §10:

```
[tools]                                       ← API position 1
  stable tools first, breakpoint, variable tools after

[system]                                      ← API position 2
  agent system prompt
  --- IDENTITY ---
  ## personality
  <body of every personality entry that applies_to this profile>

  ## rules
  <body of every rules entry that applies_to this profile>

  ## team
  <...>
  --- /IDENTITY ---
  --- REFERENCE DOCS ---
  <profile-level CLAUDE.md analogue, if any>
  --- /REFERENCE DOCS ---

[messages]                                    ← API position 3
  arc summary + history + current turn
```

The identity block is **profile-filtered at build time** — only entries
where `applies_to` matches the active profile (or `Always`) are
included. The coder profile never sees `personality` entries tagged
`[Profile("personal_assistant")]`, so its tokens are not spent on
warmth instructions it would never use.

Section ordering is by `IdentityCategory.sort_order`, then by entry
`updated_at DESC` within each category — newer edits float up. Both
are stable across requests, so the prefix cache stays valid.

## Cache friendliness

Identity content is appended to the static header, which is part of
the prompt-cache prefix. Editing an entry invalidates the cache once
(next request rebuilds the prefix); subsequent requests cache-hit
again. This is the same cost model as editing the system prompt —
acceptable for a hand-maintained store.

Anti-patterns to avoid (cribbed from `feedback_prompt_cache_optimization.md`):

- **No timestamps in body** — the user might write "as of 2026-05-08, Marta is on leave"; that's fine because the entry's content is stable until they edit it. But the *prompt builder* must not inject "current date" into the identity section.
- **Deterministic serialization** — categories sorted by `sort_order`; entries within category by `(updated_at DESC, id ASC)` — never `HashMap` iteration.
- **Profile filter is read-once per request** — no live binding to `applies_to` that re-evaluates mid-stream.

## Token budget — the disclaimer

The Settings → Identity editor shows a live token estimate at the top:

```
Identity: 1,247 tokens · Smallest configured model: Llama 3.2 3B (8K)
~15.6% of context — safe for capable models, may degrade output quality on smaller ones.
```

Color-coded:

- **Green** (<5%): no concern
- **Yellow** (5–15%): visible
- **Red** (>15%): warning banner: "Long identity blocks crowd out task context on smaller models. Consider trimming or tagging entries to fewer profiles."

The estimate is `chars / 4` (same heuristic as compaction). It's
per-profile, since different profiles see different subsets — the UI
defaults to "All profiles" but lets the user click a profile chip to
see that profile's effective identity size.

We **do not** auto-truncate. Identity is the user's territory — if
they wrote it, they meant it. The disclaimer is information, not
gating.

## UI shape

Settings → **Identity** (top-level entry, sibling of Providers / Senses):

```
┌─────────────────────────────┬─────────────────────────────────────────┐
│ Identity                    │  personality                            │
│                             │  ─────────────────────────────────────  │
│ ▸ personality          (4)  │  Athen replies in the user's language.  │
│   rules                (2)  │  Voice is warm but not chatty;          │
│ ▸ knowledge            (7)  │  prefers concise answers over long      │
│   team                 (1)  │  preambles. Never apologises for        │
│   coding_style         (3)  │  things outside its control.            │
│                             │                                         │
│ + Add category              │  applies_to: [Always]    📌 pinned      │
│                             │  ─────────────────────────────────────  │
│ ───                         │  + Add entry                            │
│ Tokens: 1,247 (15.6%)       │                                         │
│ ⚠ Crowds smaller models     │                                         │
└─────────────────────────────┴─────────────────────────────────────────┘
```

- **Left panel:** category list with entry counts. Click to expand.
  Drag to reorder. `+ Add category` opens a modal (name, description,
  default `applies_to`).
- **Right panel:** entries for the selected category. Each entry
  shows body (markdown editor), `applies_to` chip, pin toggle.
  `+ Add entry` adds a new one.
- **Footer:** total token estimate with the warning banner described
  above. Profile selector at the top changes the estimate and
  filters the visible entries.

`applies_to` chip is a small popover: `[Always]` or a multi-select of
configured agent profiles, with `Not (profile)` as a power-user mode.

## Multi-agent / multi-profile interaction

Athen will eventually have multiple profiles (`personal_assistant`,
`outreach`, `coder`, etc. — see `project_onboarding_and_model_filtering.md`).
Identity is the **shared substrate** under all of them.

Profiles can override identity by *adding* to it (their own system
prompt extends the personality), but cannot subtract from it inside
the profile prompt — to remove a constraint for a specific profile,
the user retags the entry's `applies_to` to exclude that profile.
This makes identity the source of truth and profiles its consumers,
not the other way around.

## What this is NOT

- **Not memory.** Episodic facts auto-learned from conversation
  (`memory_store`/`memory_recall`) live in `athen-memory`. Identity
  is hand-maintained and always-on; memory is auto-collected and
  recalled per-query.
- **Not the agent profile.** The agent profile defines *what*
  (which tools, which system prompt, which model bundle); identity
  defines *who* (voice, rules, who the user is). A profile reads
  identity; identity does not read profiles.
- **Not per-arc state.** An identity entry applies to every arc on
  every profile that matches. Arc-specific behavior lives in the
  arc itself (open actions, decisions made there) or — eventually
  — in standing instructions
  (see `MULTI_INTENT_ROUTING.md#adjacent-idea-coordinator-as-agent-with-standing-instructions`).
- **Not agent-editable or agent-deletable.** The agent CAN add entries
  via the `identity_add` tool (see "Agent-write" below), but never
  edits or deletes existing ones. Agent-added entries land with
  `proposed_by_agent: true` and the chat tool-call card IS the
  notification — there is no approval flow. The user reviews and
  dismisses anything wrong from Settings → Identity with one click.

## v1 scope explicitly excludes

- **Per-arc identity overrides.** Could be useful ("for this work
  arc, prefer formal English"); v2.
- **Agent edit / delete.** The agent can ADD via `identity_add`
  (shipped); editing or deleting existing entries stays user-only.
- **Sharing/import.** A community library of identity templates
  ("startup CTO", "indie hacker", "PhD researcher") would be neat
  but is post-v1.
- **Embedding-based recall of identity.** Treat identity as
  always-on, not RAG. If it grows past the budget, the user prunes;
  Athen does not silently drop entries.
- **Markdown rendering richness.** v1 stores markdown verbatim and
  injects it as plain text into the prompt. The UI may render it,
  but the LLM sees the source.

## Sequencing

This is independent of every other in-flight feature. It can ship
once Athen has at least two distinct profiles (otherwise `applies_to`
is decorative). Today the agent has one effective profile, so the
v0 of this feature could ship with `applies_to` as `[Always]` only,
and grow tagging once profiles land.

Reasonable bite-size order:

1. SQLite tables + migration.
2. Tauri commands (`list_categories`, `upsert_category`,
   `delete_category`, `list_entries`, `upsert_entry`, `delete_entry`).
3. Prompt builder integration: read all entries with
   `applies_to` matching active profile, group by category, inject
   between system prompt and reference docs.
4. Settings UI (the layout above).
5. Token estimator + disclaimer.
6. Profile-filtered token estimate (only meaningful with >1 profile).

Step 3 is the unlock — even a minimal store + manual seed entries
would deliver value.
