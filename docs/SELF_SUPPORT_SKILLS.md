# Self-Support: Athen as Its Own IT Help Desk (Design Doc)

**Status:** Design, not yet implemented. Builds on shipped [Skills](SKILLS.md) and shipped onboarding wizard (`frontend/app.js` ~10759).

The thesis: Athen is for non-technical users, but Athen has a Settings surface that already covers ~20 panels (providers, calendars, email, MCP servers, Cloud APIs, GitHub identity, profiles, vault, runtimes, …). Onboarding only walks through the LLM step. After that the user is on their own, staring at fields like "CalDAV principal URL" or "MCP server stdio command" with no agent help — because the help they'd ask the agent for IS the help to configure the agent.

This doc covers the layered path to closing that gap: cheap docs surface for the very first step, system Skills for everything after, and a narrowly-scoped agent-write surface gated behind those Skills.

## The chicken-and-egg

Step 1 of onboarding is configuring the LLM provider. The agent helping a user with step 1 needs an LLM. Three workable shapes:

1. **Bundle a zero-config free-tier preset.** One-click "use Athen's default" → routes through a free-tier provider (OpenRouter free models, Groq free tier, Gemini free tier). Lowest friction; needs picking the most permissive of the three on ToS, refresh cadence, and rate-limit behavior. The user upgrades to their own key whenever.
2. **Keep step 1 deterministic.** No agent help for the very first step — just better static UX (info modals, copy-paste install snippets for local). Skills take over from step 2 onward.
3. **Hosted onboarding assistant.** Athen ships with a short-lived Anthropic/OpenAI proxy URL whose only job is the onboarding playbook. Operational tail; rejected unless we ever need a real-time conversation during step 1.

**Recommendation:** ship (2) first (it's already 80% there — see Layer 1 below), ship (1) as a follow-up if friction data justifies it. Skip (3).

## Three layers, ordered by cost

### Layer 1 — Per-field help modals (no Skills, no LLM)

A static info button (`?` icon) on every provider card in onboarding's cloud/local picker AND in Settings → Providers. Click → modal containing:

- **Where to get the key.** Direct dashboard URL (anthropic.com/settings/keys, platform.openai.com/api-keys, …).
- **Cost note.** "Free tier: X requests/day", "$0.27/1M cached input tokens", "Subscription: $20/mo Plus tier", etc.
- **Key format hint.** Already on `ProviderCatalogEntry::api_key_hint`.
- **For local providers:** copy-paste install snippet per OS — `curl -fsSL ollama.ai/install.sh | sh` (Linux/macOS), winget / choco line (Windows), llama.cpp `brew install llama.cpp` / `make` from source. Plus a "what model can my machine run?" link out to a Skill (Layer 2).

**Data model.** Extend `ProviderCatalogEntry` in `crates/athen-app/src/settings.rs:711`:

```rust
pub struct ProviderCatalogEntry {
    // ... existing fields ...
    pub dashboard_url: &'static str,      // where to get the key
    pub cost_note: &'static str,          // human-readable pricing summary
    pub install_snippets: &'static [InstallSnippet], // local only
}
pub struct InstallSnippet { pub os: &'static str, pub label: &'static str, pub cmd: &'static str }
```

Same shape for `RegisteredEndpointPreset` in `http_presets.rs` (most already have a `dashboard_url` field morally; just formalize).

**Scope:** ~12 LLM providers + 15 Cloud APIs presets + 8 email providers + 5 calendar presets. ~40 entries hand-curated, none AI-generated. Half a day, no new infrastructure.

**Why this is the right first move.** The onboarding LLM step IS the moment the user can't yet ask the agent for help, so this is the *only* layer that needs to work without an LLM running. Everything else can lean on Skills.

### Layer 2 — System Skills (knowledge-only, walk-the-user-through-clicks)

One Skill per Settings area. Each is a folder under a new `skills/system/` tree, shipped with the binary, listed in the Skills picker alongside user skills.

**Catalog (initial):**

- `setup-calendar-source` — "Help me connect my iCloud/Google/Fastmail calendar"
- `setup-email` — "Help me connect my email" (delegates to the autodetect flow + LLM error translator)
- `setup-mcp-server` — "Add an MCP server"
- `setup-cloud-api-endpoint` — "Register a new HTTP API"
- `setup-github-identity` — "Connect my GitHub account / make a bot identity"
- `setup-skill` — meta: "How do I write my own Skill?"
- `setup-wakeup` — "How do scheduled tasks work?"
- `pick-local-model` — hardware estimator + opinionated recommendations (8GB RAM → llama 3.2-3B, 16GB → Qwen 3.5-7B, 24GB+ GPU → Qwen 3.5-14B, …)
- `understand-risk-system` — what Auto / NotifyAndProceed / HumanConfirm / HardBlock mean and how to change defaults
- `understand-profiles` — how to make a specialized agent (outreach, personal assistant, coder)
- `troubleshoot-no-llm-response` — provider key wrong? quota hit? network? walks through diagnostic checks

**Storage model.** Re-use the existing `SkillStore` machinery (`athen-persistence/src/skills.rs`). Two flags on the row:

- `system: bool` — read-only, can't be deleted via UI, can be hidden per-user (`hidden: bool` already in design).
- `version: String` — bumps with the binary; on upgrade we replace the on-disk skill body and bump version, but never blow away user edits to user skills.

**Discovery surface.** System Skills appear in the static prefix listing like any other Skill, with a small `[system]` tag in the picker. The agent calls `load_skill("setup-calendar-source")` just like a user Skill.

**What the body looks like.** Plain markdown, walk-through style. Example sketch for `setup-calendar-source`:

```
You're helping the user connect a remote calendar. The flow:
1. Ask which provider (iCloud / Google / Fastmail / Yandex / Nextcloud / Custom).
2. For iCloud: they need an app-specific password from appleid.apple.com → Security → App-Specific Passwords. Generate one, label it "Athen".
3. Tell them to open Settings → Connections → Calendar Sources → Add Source → pick iCloud preset.
4. Paste username (Apple ID email) + the app-specific password.
5. Athen will test, then ask which calendars to sync.

Common failures: ...
```

No new tools needed; agent reads the playbook + already-callable `calendar_*` info tools.

**Cost:** ~12 system Skills, ~150-300 lines of prose each. Generated via the pipeline below, then human-reviewed.

### Layer 3 — Skill-gated settings tools (the agent does it for you)

The next ambition: the agent doesn't just *walk* the user through Settings → Calendar → Add Source, it actually presses the buttons. But free-text fields (CalDAV custom URL, MCP stdio command, API key) stay user-driven — too risky to delegate, too much tool bloat to wrap each individually.

**Carve.** Three generic typed Tauri commands, callable only from inside a Skill execution context:

- `apply_setting_toggle(panel: SettingPanel, key: &str, value: bool)`
- `apply_setting_choice(panel: SettingPanel, key: &str, value: &str)` — from a known enum
- `apply_setting_numeric(panel: SettingPanel, key: &str, value: f64)`

NOT included: `apply_setting_text(...)`. Free-text always goes through the UI by the user's hand.

**Gating mechanic.** These tools aren't in `AppToolRegistry` for normal agent dispatch. They're registered on a separate **SkillScopedRegistry** that the executor only consults when the current arc has an active `load_skill` frame from a `system: true` Skill with `apply_setting: true` in its frontmatter. When the Skill frame closes, the capability drops.

**Risk story.** Per-call risk stays `WritePersist`. The pre-approval at the playbook level (the user invoked the system Skill knowing what it does) covers the *plan*; the per-call gate still fires on each write. For destructive panel actions (delete a calendar source, revoke a key) the band stays `HumanConfirm`.

**Why this is Layer 3 and not Layer 2.** It introduces a new tool surface, a new capability-token mechanism in the executor, and a new risk model (Skill-author trust). All three are non-trivial. Layer 2 ships value (walk-throughs work) without any of that.

## Skill generation pipeline

The schema/scaffold should be generated; the prose should be human-reviewed. Trigger should be **local**, not CI-LLM.

### `cargo xtask gen-skills`

A new xtask binary that introspects:

- The Tauri command registry (`crates/athen-app/src/lib.rs` invoke list).
- A new declarative `settings_registry!` macro you'd add (one line per setting: panel, key, type, free-text-or-structured, default).
- The current `ProviderCatalogEntry` / `RegisteredEndpointPreset` / `EmailProviderPreset` / `CalendarPreset` lists.

Output: under `skills/system/`, one skeleton per area with:

- Frontmatter filled (`name`, `description`, `system: true`, `applies_to`, `apply_setting` flag where appropriate).
- A schema block listing the relevant settings keys + types.
- Body sections with `TODO:` placeholders for prose (intro, step-by-step, common-failures).

### `cargo xtask gen-skills --fill-prose`

Optional. Hits the user's currently-active LLM provider (no separate key needed — Athen's own router). For each `TODO:` block, prompts: "Write a non-technical-user walkthrough for setting X. Here's the panel definition, here are the field types, here are the validation rules. Keep it < N words. Output markdown."

Output goes to disk as plain `.md` for git diff + human review. **Nothing auto-commits.** The user accepts or edits, then commits the file like any source change.

### CI staleness check

A workflow `gen-skills-check` that:

1. Runs `cargo xtask gen-skills` in a temp dir (no `--fill-prose`).
2. Diffs the *schemas* (not prose) against committed `skills/system/`.
3. Fails if drift detected, with a message: "Settings changed but Skill schemas weren't regenerated — run `cargo xtask gen-skills` locally."

Same shape as `cargo fmt --check`. No LLM in CI hot path. Works on forks. Deterministic.

**Why this beats CI-trigger.** A GitHub Actions workflow that calls an LLM on every settings PR (a) needs a repo secret with billing attached, (b) fails on forks, (c) introduces non-determinism into CI, (d) puts LLM-confidently-vague prose into the source tree without review. The local pipeline keeps the LLM out of CI and forces a human review step.

## Proactive help (post-onboarding nudges)

Once Layer 2 is shipped, Athen has a unique capability no other app has: **the agent knows the same thing about Settings the user does.** That unlocks a new UX surface — Athen can volunteer help.

**Triggers (concrete signals, not vibes):**

- User skipped a step in onboarding (memory step, search step, identity step) → on next idle pass, surface a one-line notification: *"You skipped the memory step. Want me to walk you through it? Takes ~2 min."* Click → invokes `setup-memory` Skill.
- User has no calendar source configured but agent receives a calendar-shaped request (sense email mentions a meeting, user-typed "schedule X") → surface: *"I can't push this to a real calendar yet — only local. Want to connect one?"* → `setup-calendar-source`.
- User has no email sender configured but agent drafts an email reply (sense path) → *"Reply ready, but I can't send it from Athen until you connect SMTP. ~3 min."* → `setup-email`.
- User configured Cloud API endpoint X but hasn't used it in 30 days → *"You set up Hunter.io but haven't used it. Want me to suggest workflows that'd use it?"* — low-priority "show me how" surface.
- User on `llamacpp` + a known-quirky model (Qwen-class) without `model_family` set → *"Your local model has known quirks. Want to set the family for better tool calls?"* → `setup-local-model-family`.

**Implementation shape.** A new `proactive_hint` sense that runs a small rules engine on AppState changes (config writes, sense events) and emits hints to the in-app notification surface. Hints are *suggestions*, not actions — they always require a user click to invoke the corresponding Skill. Never auto-dispatches without consent.

**Rate limit.** At most one proactive hint per hour, dismissable, "don't show this again" sticks. Otherwise it becomes Clippy.

**Why this isn't v1.** Needs Layer 2 shipped first (otherwise the Skills it'd invoke don't exist), needs notification UX work, and needs careful rate-limit tuning. Mention in roadmap, don't build until L2 is in user hands and we have telemetry on what users actually get stuck on.

## Build order

1. **L1 — Per-field help modals.** ~1 day. Extend `ProviderCatalogEntry` + sibling preset structs with `dashboard_url`, `cost_note`, `install_snippets`. Wire `?` icon + modal into onboarding and Settings. Hand-curate ~40 entries.
2. **`cargo xtask gen-skills` scaffold + `settings_registry!` macro.** ~1 day. Skeleton generation only, no `--fill-prose` yet.
3. **L2 — ship 3 highest-value system Skills first.** `setup-calendar-source`, `setup-email`, `setup-mcp-server`. Hand-write the prose; validate the shape works.
4. **`--fill-prose` flag + run on remaining ~9 system Skills.** Human-review every output before merge.
5. **CI staleness check.** ~half day. Drop-in.
6. **(Defer) L3 — Skill-gated settings tools.** Comes back when L2 telemetry shows users want the agent to *do* the click, not just describe it. Likely 1-2 weeks: capability token + SkillScopedRegistry + risk wiring + write-side Tauri commands.
7. **(Defer) Proactive help sense.** After L2 has been in users' hands for ~a release cycle.

## Out of scope (explicit)

- **Free-tier bundled LLM preset.** Separate decision — answered by [SUBSCRIPTION_RELAY_PROVIDERS.md](SUBSCRIPTION_RELAY_PROVIDERS.md)-style research on which free-tier provider has the most permissive ToS for our use. Not blocked by this doc.
- **Auto-applying Skills.** L3 is gated by user invoking a Skill; Skills never run on their own without user consent.
- **Free-text Settings writes by agent.** Stays user-driven forever. Frees us from prompt-injection-via-settings attacks.
- **Per-field tool wrappers.** Explicitly rejected — bloats the tool surface and creates a maintenance burden. Three generic structured-write tools cover the surface.
- **A Skills marketplace for system Skills.** System Skills ship with the binary; user marketplace is the existing Skills design's follow-up.

## Open questions

- **Multi-language.** Skills today are English-only. If we localize, do system Skills follow the OS locale, the user's identity-stated language, or stay English? Probably identity-stated language with English fallback; the LLM is already doing the translation work either way.
- **System Skill versioning across upgrades.** If a user hides a system Skill, then we release a new version with significantly different content, do we re-show it? Probably yes with a "this Skill was updated" toast, dismissable.
- **Telemetry on which Skills get loaded.** Useful for tuning the catalog, but opt-in only — never silently. Adjacent to the existing privacy stance.

## See also

- [SKILLS.md](SKILLS.md) — Shipped user-Skills design. This doc extends it with the `system: true` flag and the Skill-gated capability mechanic.
- [IDENTITY.md](IDENTITY.md) — Separate store for *who* Athen is, vs Skills' *how to do X*.
- [INTEGRATIONS_PUSH.md](INTEGRATIONS_PUSH.md) Move #5 (LLM-assisted credential UX) — partial overlap with Layer 1 here.
- [EMAIL_SETUP.md](EMAIL_SETUP.md) — Already has an LLM error translator pattern, which is the kind of just-in-time help Layer 2 generalizes.
