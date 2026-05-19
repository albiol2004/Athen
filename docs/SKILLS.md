# Skills (Design Doc)

**Status:** Shipped (2026-05-15, as of 2026-05-19). v0 build order steps 1-7 all landed; per-arc idempotency is design not yet implemented.

Live today:
- Core types + `SkillStore` trait at `crates/athen-core/src/skill.rs` and `crates/athen-core/src/traits/skill.rs`.
- Filesystem + SQLite index store at `crates/athen-persistence/src/skills.rs`.
- `load_skill` agent tool at `crates/athen-app/src/app_tools.rs` (~line 1585), refuses cleanly when the store isn't wired. Per-arc idempotency (skipping body re-fetch on repeated calls in same arc) is a design pattern shown below but not yet implemented.
- Tauri commands in `crates/athen-app/src/commands.rs`.
- Settings → Skills panel at `frontend/index.html` + `frontend/app.js`.
- Static-prefix listing of skill names + descriptions in executor (gated on `load_skill` tool presence).

The "v0 scope explicitly excludes" section below is still authoritative — `write_skill` (agent-authored skills), vector-recall discovery, sibling-file auto-bundling, bundled-skills shipping, and the marketplace are all deferred. Use the design doc as the reference for what's intentionally out of scope.

Athen needs a place to drop **procedural orientation** that the agent
can pull in on demand: how to write a good cold email, how to file an
expense report, how to phrase a refusal, how to format a release note
for this repo. These aren't personality, aren't rules, aren't
auto-recalled facts — they're *playbooks the model consults when the
task fits the shape*.

Claude Code's Skills are the shape we copy: a folder per skill with a
`SKILL.md` (frontmatter `name` + `description` + body) and optional
sibling files (templates, examples, scripts). The model sees the
name+description in every turn, calls a `load_skill` tool when one
fits, and the body lands in context as a tool result. Lightweight,
file-based, no runtime, naturally shareable as a zip or a git repo.

## Motivating examples

- **Cold email playbook.** Alex writes `cold-email-outreach/SKILL.md`
  covering subject-line patterns, opening hooks, CTA shapes, and
  anti-spam rules. The outreach profile lists it in its prefix; when
  the agent gets "draft a cold email to the CTO of Stripe", it calls
  `load_skill("cold-email-outreach")` and the body lands inline. The
  coder profile never sees it.
- **Release-note formatter.** A `release-notes/SKILL.md` with the
  team's house style (Conventional Commits → human prose, breaking
  changes section, contributor list shape). The coder profile loads
  it before drafting a release blurb.
- **Phone-call summary.** A `call-summary/SKILL.md` with the
  user's preferred structure (TL;DR → decisions → action items →
  open questions). Loaded whenever the agent's input looks like a
  meeting transcript.
- **Shareable.** A second user downloads `cold-email-outreach/`
  as a folder (or zip, or `athen skills install <url>`), drops it
  in their skills dir, and their agent gets the same playbook.

The unifying property: each skill is a *small unit of
domain-specific procedural knowledge* that's too long to bake
into every system prompt and too task-specific to ride in identity.

## Distinction vs sibling stores

| Store | What | When loaded | Filled by |
|---|---|---|---|
| **Identity** | Who Athen / the user is — personality, rules, team, knowledge | Always-on in static prefix | User edits + `identity_add` tool |
| **Memory** | Auto-recalled episodic facts | Top-3 hits per user turn | Agent's `memory_store` + post-turn auto-judge |
| **Skills** (this doc) | Procedural playbooks — "how to do X" | On-demand via `load_skill(name)` | User edits + (later) `write_skill` tool |
| **AgentProfile** | What the agent *is* — tools, model bundle, base prompt | Boot-time per profile | Settings UI |

Skills are the only one of these where the **listing** is cheap (just
name + description) and the **body** is loaded lazily. Identity pays
full freight up front; memory recalls full bodies; skills tease.

## The shape

A skill is a folder. Inside:

```
<data_dir>/skills/<slug>/
  SKILL.md           # frontmatter + body (required)
  examples.md        # optional, agent reads on demand
  template.md        # optional, can be referenced by SKILL.md body
  ...                # any other sibling files the SKILL.md references
```

`SKILL.md` frontmatter mirrors Claude Code's:

```markdown
---
name: cold-email-outreach
description: Use when drafting a cold email to a stranger — covers subject lines, opening hooks, CTA shapes, and anti-spam rules.
applies_to: [outreach, personal_assistant]   # optional; default is "all profiles"
---

# Cold Email Outreach

(body — free-form markdown. The agent sees this verbatim as a tool result.
Reference sibling files explicitly, e.g. `see examples.md for full templates`.)
```

Rust types in `athen-core`:

```rust
pub struct Skill {
    pub slug: String,                 // folder name, kebab-case, unique
    pub name: String,                 // frontmatter `name` (display)
    pub description: String,          // frontmatter `description` — what the model sees in the listing
    pub applies_to: Vec<ProfileTag>,  // reuse the enum from identity
    pub source: SkillSource,          // Bundled | User | Imported
    pub body_path: PathBuf,           // disk path of SKILL.md
    pub hash: String,                 // body+frontmatter content hash for cache invalidation
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub enum SkillSource {
    Bundled,        // compiled into the binary; user can shadow with a same-slug User skill
    User,           // hand-authored or imported via UI
    Imported,       // shipped via zip/URL — same as User in v0, distinct flag for provenance UI
}

#[async_trait]
pub trait SkillStore: Send + Sync {
    async fn list(&self, profile: Option<&str>) -> Result<Vec<Skill>>;
    async fn get(&self, slug: &str) -> Result<Skill>;
    async fn load_body(&self, slug: &str) -> Result<String>;   // reads SKILL.md body from disk
    async fn upsert(&self, slug: &str, frontmatter: SkillFrontmatter, body: &str) -> Result<()>;
    async fn delete(&self, slug: &str) -> Result<()>;
}
```

`load_body` returns just the body (frontmatter stripped) — that's what
the agent consumes. Listing returns descriptions only — keeps the
static-prefix injection cheap.

## Storage — hybrid (filesystem + SQLite index)

Bodies live on disk as plain `SKILL.md` files; SQLite holds an index
for queryability and so listing doesn't have to scan the filesystem
every turn.

```
<data_dir>/skills/
  <slug>/SKILL.md      ← user-editable, git-friendly, the source of truth
  <slug>/...           ← sibling files
```

```sql
CREATE TABLE skills_index (
    slug TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    description TEXT NOT NULL,
    applies_to JSON NOT NULL,
    source TEXT NOT NULL,           -- "Bundled" | "User" | "Imported"
    body_path TEXT NOT NULL,
    hash TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE INDEX idx_skills_source ON skills_index(source);
```

A boot-time **sync pass** walks `<data_dir>/skills/`, reads each
`SKILL.md`'s frontmatter, computes its hash, and reconciles the SQLite
index (insert new, update changed, delete missing). This makes the
filesystem authoritative: a user can `git clone` a skills repo into
the directory and Athen picks it up on next start. Same pattern for
the bundled skills baked into the binary — they're written to disk
on first boot if absent.

The SQLite index is **derived state**, never authoritative. If a user
deletes it, the next boot rebuilds it from disk.

## Discovery — static-prefix listing

Every executor turn lists all skills whose `applies_to` matches the
active profile in the static system prefix:

```
[system]
  <agent system prompt>
  --- IDENTITY ---
  ...
  --- /IDENTITY ---
  --- SKILLS ---
  Available skills (call `load_skill(slug)` to read full body):
  - cold-email-outreach: Use when drafting a cold email to a stranger — covers subject lines, opening hooks, CTA shapes, and anti-spam rules.
  - release-notes: Format release notes from Conventional Commits in this team's house style.
  - call-summary: Structure a meeting transcript into TL;DR / decisions / action items / open questions.
  --- /SKILLS ---
```

Budget: ~30 tokens per skill (name + one-sentence description). 20
skills costs ~600 tokens always-on, which is on the cheap side. We
do **not** auto-truncate the listing; the user is the budget
authority. Settings shows a live tokens-by-profile estimate (same
shape as identity).

Vector-recall as an *alternative* discovery path (mirror
`memory_recall`) is deferred — it hides skills from the model unless
the cosine threshold trips, which defeats the "browse and consult"
intent. Revisit if a user hits >50 skills and the static listing gets
heavy.

## Invocation — the `load_skill` tool

New agent tool in `crates/athen-app/src/app_tools.rs` (~line 1585):

```rust
async fn do_load_skill(store: &SkillStore, slug: &str) -> Result<String> {
    let body = store.load_body(slug).await?;
    Ok(body)
}
```

**v0 note**: Per-arc idempotency (the pattern below) is a future optimization not yet in code. Today `load_skill` always returns the full body. Once implemented, it will cache `(arc_id, slug)` pairs and return a stub "already loaded" notice on repeated calls.

```rust
// Future per-arc idempotency pattern (design, not yet shipped):
// Idempotent: if already loaded in this arc, return a short
// "already loaded" notice — don't refire the body and double the cost.
if state.arc_skill_cache.contains(arc_id, slug).await {
    return Ok(format!("Skill `{slug}` is already loaded in this arc."));
}
let body = state.skills.load_body(slug).await?;
state.arc_skill_cache.mark_loaded(arc_id, slug).await;
Ok(body)
```

Risk tier: read-only, no side effects on external systems. Goes in
the `tier-2 / read` group — primary_groups for outreach / coder
profiles include it; mainstream profiles call it ad-hoc when relevant.

The body returns as a tool result, which lands in the message stream
verbatim. The agent treats it like any other context — same recency
weighting, same cache behavior (skill bodies are stable; the tool
result chunk caches normally).

Sibling-file access: the body can say "see `examples.md` for the
filled templates" — the agent then calls `read_file` (or the
existing file-read tool) with the path emitted by `load_skill`'s
result preamble. v0 keeps it explicit; we don't auto-bundle siblings
into the `load_skill` response.

## Where it plugs into the prompt

The listing rides in the static system prefix, after identity, before
reference docs:

```
[tools]
  <stable tools, breakpoint, variable tools>

[system]
  <agent system prompt>
  --- IDENTITY ---
  ...
  --- /IDENTITY ---
  --- SKILLS ---
  <name + description list, profile-filtered, deterministic order>
  --- /SKILLS ---
  --- REFERENCE DOCS ---
  ...
  --- /REFERENCE DOCS ---

[messages]
  ...
  (load_skill tool calls land here as normal tool results)
```

Ordering: by `slug ASC` for stability — keeps the prefix cache valid
across requests. Editing a skill bumps `updated_at` and `hash` but
not position, so cache invalidation is one-shot.

## Cache friendliness

Listing is part of the static prefix → same cache model as identity.
Editing a description invalidates once, then cache-hits again.

Bodies arrive as **tool results** — cached on the *message* side, not
the prefix side. A skill loaded twice in the same arc returns the
"already loaded" stub (saves the body tokens on repeated calls; the
agent already has the first load earlier in the message stream).

Anti-patterns to avoid:

- **No timestamps in the listing.** The description should describe
  *the skill*, not *when it was written*.
- **Deterministic order** — `ORDER BY slug ASC`, no `HashMap`.
- **No nested loading.** `load_skill` returns one skill's body. If
  skill A's body says "also load skill B", the agent makes a second
  tool call — keeps each load auditable.

## Authoring

### User (v0)

Settings → **Skills** (top-level, sibling of Identity):

```
┌─────────────────────────────┬─────────────────────────────────────────┐
│ Skills                      │  cold-email-outreach                    │
│                             │  ─────────────────────────────────────  │
│ ▸ cold-email-outreach       │  Description: Use when drafting a cold  │
│   release-notes             │  email to a stranger — covers subject…  │
│   call-summary              │                                         │
│ ─── bundled ───             │  applies_to: [outreach, personal_assi.] │
│   summarize-pdf             │                                         │
│   format-bash-script        │  Body (markdown):                       │
│                             │  ┌─────────────────────────────────┐   │
│ + New skill                 │  │ # Cold Email Outreach            │  │
│ ⬆ Import from zip/URL       │  │                                  │  │
│                             │  │ ## Subject lines                 │  │
│ ───                         │  │ Keep them <40 chars. ...         │  │
│ Tokens (outreach): 412      │  └─────────────────────────────────┘   │
│ Skills loaded this arc: 0   │                                         │
└─────────────────────────────┴─────────────────────────────────────────┘
```

- **Left panel:** skill list grouped by source (User first, Bundled
  second, Imported third). `+ New skill` opens an editor with empty
  frontmatter + body. `⬆ Import` accepts a zip or URL.
- **Right panel:** frontmatter form (name, description, applies_to
  chip) + markdown editor for the body. Save writes the `SKILL.md`
  file to disk and refreshes the SQLite index.
- **Footer:** per-profile token estimate; counter of how many skills
  the current arc has loaded so far (debug aid).

Editing a Bundled skill creates a User shadow — same slug, source =
User, takes precedence in the listing. The bundled original is
restorable from a "Reset to bundled" button.

### Agent (deferred to v1+)

A `write_skill` tool — same shape as `identity_add`: agent emits a
new skill from conversation ("user asks me to summarise meetings
this way every time → write a skill, mark `proposed_by_agent`,
notify the user via chat tool-call card"). Not in v0.

Editing or deleting existing skills stays user-only forever, same
rule as identity.

## Sharing — export/import

v0: a skill is a folder; users can copy it manually or share a git
repo. The Settings UI exposes `⬆ Import from zip/URL` and a
per-skill `⬇ Export as zip`.

v1+:
- `athen skills install <github-url>` CLI command — clones the repo
  into `<data_dir>/skills/<slug>/`.
- A curated registry (similar to Homebrew taps) — `athen skills
  install awesome/cold-email` resolves to a known repo. Out of
  scope until v0 lands and we have something to share.

Imports land with `source: Imported`. We **do not** auto-execute any
scripts in imported skills; bodies are read by the LLM, not by Athen.
A future v1 may allow imported skills to bundle a sandbox-safe
script (e.g. a deterministic formatter) that the agent can invoke —
that's where the risk story gets real and warrants its own design.

## What this is NOT

- **Not memory.** A skill is stable procedural knowledge; memory is
  per-conversation episodic facts.
- **Not identity.** Identity is always-on and short; skills are
  on-demand and can be long.
- **Not a profile.** A profile selects tools and a base prompt;
  a skill is consulted *within* a profile.
- **Not an MCP.** An MCP is a tool surface; a skill is text. They
  compose — a skill body can say "this task is best done with the
  GitHub MCP's `create_pr` tool", but the skill itself executes
  nothing.
- **Not auto-recalled.** The agent decides when to `load_skill`; we
  don't push bodies into the prompt based on similarity. (Listing is
  pushed; bodies are pulled.)

## v0 scope explicitly excludes

- **Vector-recall discovery** of skills (always-listed in prefix
  instead).
- **Agent-authored skills** via `write_skill`. User-authored only in
  v0.
- **Skill sibling-file auto-bundling** in the `load_skill` response.
  Bodies reference sibling files; agent fetches them separately.
- **Skill-bundled scripts** (executable templates). Bodies are
  prose only.
- **Marketplace / curated registry.** Manual zip/URL import only.
- **Skill chaining** (skill A auto-loads skill B). One load = one
  tool call.
- **Per-arc skill overrides.** Skills are global per-profile.
- **Token-budget enforcement.** Settings shows the estimate; user
  is the budget authority (same stance as identity).

## Sequencing — v0 build order

1. **Core types & trait** — `Skill`, `SkillStore`, `SkillSource`,
   `SkillFrontmatter` in `athen-core`. `applies_to` reuses
   identity's `ProfileTag`.
2. **Filesystem + SQLite store** in `athen-persistence` — boot-time
   sync pass, `list / get / load_body / upsert / delete`, hash-based
   change detection.
3. **`load_skill` agent tool** in `athen-app/src/app_tools.rs` —
   idempotent per arc (cache `(arc_id, slug)` loaded set in
   `AppState`).
4. **Static-prefix listing** — extend the prompt builder to inject
   the `--- SKILLS ---` block after identity. Profile-filtered.
5. **Tauri commands** — `list_skills`, `get_skill`, `upsert_skill`,
   `delete_skill`, `import_skill_zip`, `export_skill_zip`.
6. **Settings → Skills panel** — the layout above.
7. **Token estimator** in the panel footer.

Step 4 is the unlock — once skills are visible to the model and
`load_skill` works, even a hand-seeded `<data_dir>/skills/` directory
delivers value. Bundled skills (step before this whole list, if we
choose to ship some) can be added later by writing them to disk on
first boot.

## Sequencing — what to do BEFORE step 1

Two upstream decisions worth nailing first:

- **Frontmatter parser.** YAML (matches Claude Code, but adds a YAML
  dep) vs. a tiny hand-rolled `key: value` parser (no dep,
  Athen-flavored). Recommendation: hand-rolled — the frontmatter
  schema is fixed and small (`name`, `description`, `applies_to`).
  Avoids dragging `serde_yaml` into `athen-core`.
- **Bundled defaults vs. empty start.** Ship 3-5 bundled skills
  (cold-email-outreach, release-notes, call-summary, summarize-pdf,
  format-bash-script) so the feature has something to show on first
  boot, or ship empty and let users discover the import flow.
  Recommendation: ship empty in v0, ship 3-5 bundled in v0.1 once
  the format has stabilised — otherwise we'll regret writing them
  before the format is locked.

## Adjacent ideas — explicitly deferred

- **Skill versioning.** Body hash changes mean "the skill changed";
  we don't track semver. If a community library grows, this comes
  back.
- **Skill dependencies on MCPs.** A `requires_mcp:
  [github]` frontmatter field that the agent surfaces if the MCP
  isn't enabled. Useful once skills become first-class shareable
  artifacts.
- **Per-skill telemetry.** "Skill X loaded N times in the last
  month" — interesting for pruning, but feels like a v2 polish.
- **Skill-aware routing.** Coordinator picks a profile based on
  which skill matches best. Premature today; revisit when there
  are >5 profiles.
