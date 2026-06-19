# Athen -- Universal AI Agent

Athen is a **universal, proactive AI agent** built as a native desktop application (Tauri 2 + Rust). It monitors emails, calendar, messages, and direct input ("senses"), evaluates what needs doing, and executes tasks autonomously -- with a dynamic risk system that decides when to act silently vs. ask for permission. Designed for non-technical users: single binary, native GUI, zero runtime dependencies.

## Tech Stack

| Component | Technology | Why |
|-----------|-----------|-----|
| Core | Rust | Speed, memory safety, native cross-platform |
| UI | Tauri 2 | Native app with web frontend, tiny binaries |
| MCPs | Rust binaries | Standalone tools, no runtime deps |
| Database | SQLite | Embedded, serverless, portable |
| Shell | Nushell (embedded) | Cross-platform consistent shell + native fallback |
| Sandbox | OS-native + Podman/Docker | Tiered isolation, zero user setup for OS-native |

## Architecture

Multi-process architecture communicating over IPC (Unix sockets / Named pipes).

```
SENTIDOS (Monitors) --IPC--> SENSE ROUTER (Tauri) --events--> COORDINADOR
                                                                   |
                                                          IPC (TaskAssignments)
                                                         /    |    \
                                                    Agent1  Agent2  AgentN
                                                         \    |    /
                                                      EXECUTION LAYER
                                              (MCPs + Shell + Scripts + HTTP)
```

## Workspace Structure

```
athen/
├── Cargo.toml                    # Workspace root
├── frontend/                     # Desktop WebView frontend (HTML/CSS/JS, Tauri)
│   ├── index.html, styles.css, app.js
├── web/                          # Remote web UI (React+TS+Vite) — embedded via rust-embed, served by http_api.rs; dist/ committed so cargo never needs Node
├── crates/
│   ├── athen-core/               # Shared types + trait contracts (THE CONTRACTS)
│   ├── athen-ipc/                # IPC transport layer
│   ├── athen-sentidos/           # Sense monitors (email, calendar, messaging, telegram, user)
│   ├── athen-coordinador/        # Coordinator (router, risk eval, queue, dispatch)
│   ├── athen-agent/              # Agent worker (LLM executor, auditor, timeout)
│   ├── athen-llm/                # LLM provider adapters + router + failover + embeddings
│   ├── athen-web/                # Web search + page-reader providers (DDG, Tavily, Local, Jina, Wayback, Cloudflare)
│   ├── athen-mcp/                # MCP runtime catalog + registry (enable/config/spawn BYO + bundled MCPs)
│   ├── athen-memory/             # Vector index + knowledge graph + SQLite
│   ├── athen-risk/               # Risk scorer + regex rules + LLM fallback
│   ├── athen-persistence/        # SQLite persistence, arcs, calendar, contacts, http endpoints, hint dismissals
│   ├── athen-contacts/           # Contact trust model + risk multipliers
│   ├── athen-sandbox/            # OS-native + container sandboxing
│   ├── athen-shell/              # Nushell embedding + native shell fallback
│   ├── athen-vault/              # Encrypted credential vault (OS keychain + encrypted-file fallback)
│   ├── athen-caldav/             # CalDAV adapter (iCloud / Google-via-CalDAV / Fastmail / Nextcloud)
│   ├── athen-checkpoint/         # Gix-backed snapshot store (write/edit pre-state, revert UI)
│   ├── athen-scheduler/          # Wake-up scheduler (compute_next_fire + WakeupScheduler driver)
│   ├── athen-cli/                # CLI runner (REPL)
│   ├── athen-admin/              # Admin panel + gateway for hosted multi-instance deployments (standalone, zero internal deps)
│   └── athen-app/                # Tauri desktop app (composition root)
```

## Design Principles (CRITICAL)

### 1. Hexagonal Architecture (Ports & Adapters)

`athen-core` defines ALL traits (ports). Every other crate implements adapters. No crate depends on a sibling -- only on `athen-core`. `athen-app` is the composition root that wires implementations together.

### 2. Dependency Rules

- `athen-core` depends on NOTHING internal (only serde, chrono, uuid, thiserror, async-trait, url, tokio-stream)
- All other crates depend on `athen-core` for trait definitions
- Crates NEVER depend on sibling crates (except through `athen-core` traits)
- `athen-app` is the ONLY crate that depends on multiple siblings
- Future bundled MCPs (Slack, Notion, ...) will live under `crates/mcps/` as standalone JSON-RPC servers that do NOT depend on `athen-core`

### 3. Independent Testability

Every crate can be tested in isolation by mocking trait dependencies.

## Coding Guidelines

- Async: `tokio` runtime, `#[async_trait]` for trait definitions
- Errors: `thiserror` with `AthenError` enum and `Result<T>` from `athen-core::error`
- Serialization: `serde` with `Serialize`/`Deserialize` derives
- IDs: `uuid::Uuid` v4 | Timestamps: `chrono::DateTime<Utc>`
- Platform-specific: `#[cfg(target_os = "...")]`
- Logging: `tracing` crate
- HTTP: `reqwest` with `rustls-tls` (no OpenSSL)
- Tests: mock trait dependencies, not real services
- `cargo clippy --workspace` must produce zero warnings
- All config via UI, never config files -- Athen is for non-technical users
- NEVER commit or push to git unless explicitly asked by the user

## Key Commands

```bash
# Build & test
cargo build --workspace
cargo test --workspace
cargo clippy --workspace

# Run CLI
DEEPSEEK_API_KEY=sk-... cargo run -p athen-cli --release

# Run desktop app
cargo tauri dev    # (from crates/athen-app/)

# System libs needed (Fedora)
# webkit2gtk4.1-devel gtk3-devel libsoup3-devel libappindicator-gtk3-devel
```

## Platform Workarounds

- **Linux WebKitGTK + AMD/RADV stutter**: `crates/athen-app/src/main.rs` forces `WEBKIT_DISABLE_DMABUF_RENDERER=1` at startup. The DMABUF renderer in WebKitGTK 2.44+ stalls the compositor on AMD/Mesa, causing system-wide stutter. Remove once upstream ships a fix and the older GLX path is no longer needed.

## CI/CD

- `.github/workflows/ci.yml` -- clippy + tests on push to main + PRs
- `.github/workflows/release.yml` -- cross-platform Tauri builds (Linux/macOS/Windows)

## Detailed Documentation

Read the relevant doc BEFORE working on a feature area:

- [Architecture, Core Types & Security](docs/ARCHITECTURE.md) — Read when: adding/modifying traits, types, risk system, IPC, error handling, or security model
- [Implementation Status by Crate](docs/IMPLEMENTATION.md) — Read when: you need to understand what a crate does, its current state, test counts, or what files exist. Update this file after implementing changes.
- [Configuration & LLM Providers](docs/CONFIGURATION.md) — Read when: working on config loading, LLM providers, model profiles, failover, domain settings, embeddings, or web search keys/chain
- [Tools, Senses & Notifications](docs/TOOLS_AND_SENSES.md) — Read when: working on agent tools, sense monitors, sandbox execution, notification delivery, or the web search/page-reader providers
- [Arc Compaction](docs/ARC_COMPACTION.md) — Read when: working on context-window management, arc summarization, or anything that touches `ArcStore::load_entries` in the executor path. SHIPPED (Phase 1): `ArcCompactor` trait + `LlmArcCompactor` + summary persistence + per-provider budgets + settings UI + executor integration are live; Phase 2/3 (entropy pre-pass, embedding salience, hierarchical re-compaction, post-compaction verification) remain open. See doc §12 for the per-section implementation map.
- [Multi-Intent Routing](docs/MULTI_INTENT_ROUTING.md) — Read when: extending the Telegram owner-message path to split a single message into N per-arc intents (task #152), OR adding standing instructions / coordinator-as-agent memory ("for the next 4h, reply on Telegram"). Design doc; not yet implemented. Builds on #149's single-intent heuristic.
- [Projects](docs/PROJECTS.md) — Read when: working on Projects (the ChatGPT/Claude-style container that groups many arcs around common work), the opinionated workspace folder layout (`UserInfo/`/`Downloads/`/`Projects/<name>/`/...), category-driven save/write tools, or project-wide compaction. Design only (2026-06-19). A Project is a context-scope above arcs — an id that arcs (`project_id`), Identity (`applies_to`), Memory (scope), a workspace folder, and a maintained project summary all hang off. Context sharing is layered cheapest-first (instructions → file listing → scoped recall → optional LLM summary). Project summary = incremental hierarchical compaction (fold just-left arc's delta on arc-switch, per-arc watermark + dirty-gate, cheap tier, degradable for local/token-averse users). Recommended to ship the folder layout early and standalone with a forward-compatible `arcs.project_id`. Sense auto-routing into projects is future work building on Multi-Intent Routing (#152).
- [Identity](docs/IDENTITY.md) — Read when: building the user-maintained personality/rules/knowledge/team store that feeds every agent's static prefix. Categories are user-editable, entries are tagged with `applies_to` so each agent profile only pays for the sections it needs. Distinct from `athen-memory` (auto-learned, recalled per-query) and from agent profiles (define what an agent does, not who Athen is). SHIPPED (storage, UI, prompt injection, agent-write tool, 2026-05-19).
- [Wake-ups](docs/WAKEUPS.md) — Read when: working on scheduled / recurring / one-shot proactive triggers (reminders, daily digests, deferred follow-ups), including the agent-authored `create_wakeup` tool. Wake-ups are synthetic sense events with a clock as their trigger; they reuse coordinator/risk/dispatch. Risk model is "pre-approve capability, not content" via per-wake-up `AutonomyBand` + tool/contact allowlists, with the existing per-action risk gate still firing at fire time. Implemented; doc remains the conceptual reference.
- [Admin Panel & Gateway](docs/ADMIN_PANEL.md) — Read when: working on `crates/athen-admin` (multi-instance hosting: panel auth/users/grants, Docker provisioning via bollard, the session→bearer reverse proxy `/i/{id}/api/*`, the embedded panel UI or `/i/{id}/chat` client), or planning the hosted/provider offering. SHIPPED 2026-06-10: gateway model (instance tokens never reach clients; instances on a no-published-ports ICC-disabled bridge network; TLS only at whatever fronts the panel) + same-day hardening (login throttle, per-user request buckets, audit log + daily retention prune, memory/CPU quotas, ENFORCED disk quotas via docker-df sweep (warn → stop escalation with restart grace + `disk_limit` edit endpoint; `ATHEN_ADMIN_DISK_ENFORCE=warn` for warn-only), multi-admin with last-admin protection, push notifications to ntfy-style webhooks via per-instance SSE watchers, rootless Docker/Podman socket via `DOCKER_HOST` with rootful-socket detection (startup warn + audit row + dashboard banner) and per-container privilege hardening (`cap_drop ALL`, `no-new-privileges`, pids limit)). The headless `grant-requested` gap found during e2e is FIXED: FileGate speaks UiBridge, instances expose `GET /api/grants/pending` + `POST /api/grants/{id}`, the panel forwards grant prompts to webhooks and renders cards in the chat client.
- [Headless Mode](docs/HEADLESS.md) — Read when: working on `athen --headless` (the GUI-less daemon), the `UiBridge` seam (`ui_bridge.rs`), env-var credential seeding (`env_creds.rs`), `ATHEN_DATA_DIR`/`ATHEN_VAULT_BACKEND`, `athen-cli vault` subcommands, the Dockerfile, the HTTP API (`http_api.rs`: token-gated axum REST + SSE for remote React/React Native clients, `ATHEN_HTTP_ADDR`, works on desktop too), or the embedded web UI (`web/`: React+TS client compiled in via rust-embed, served on the same listener for non-`/api/*` paths — token login, full desktop-parity chat surface (expandable tool cards/groups, delegation expansion, goal/plan, attachments, per-arc pickers, agents/changes/wake-ups drawers) + full Settings modal; backed by ~110 `full_surface_router()` routes shimming the same `*_core` fns the Tauri commands use; `web/src/api/` is the DOM-free layer a future React Native app reuses; rebuild with `cd web && npm run build`, dist committed; NOT exposed: updater/runtime installs, bundled-model download, Voice setup). SHIPPED 2026-06-10 (daemon + HTTP API + web UI + parity round): full autonomous stack on plain tokio, Telegram + HTTP + browser as user surfaces, per-instance isolation for containers. Desktop keeps "config via UI"; headless is operator-facing (files + env).
- [Packaging & Distribution](docs/PACKAGING.md) — Read when: cutting a release, debugging the auto-updater, or adding a distribution channel. Covers AUR, COPR, GitHub Releases, the `installer_kind` self-update vs system-package split, and the per-release checklist.
- [Per-Model Quirks](docs/PER_MODEL_QUIRKS.md) — Read when: adding a new model/family to the provider stack, debugging tool-call extraction failures, reasoning-content surface mismatches, or strict-template HTTP 500s on local inference (Qwen/Gemma/DeepSeek). Design doc; ToolExtraction/Reasoning/TemplateStrictness axes are partially implemented (TemplateStrictness via `external_system_suffix`); ToolExtraction + Reasoning still pending. User-driven family selection (UI dropdown + editable slug), no auto-detection.
- [Prompt Caching](docs/PROMPT_CACHING.md) — Read when: touching any LLM provider request/response path, the `TokenUsage` struct, or cost estimation. Per-provider audit (Anthropic/OpenAI/DeepSeek/Google) of how prompt caching works on the wire and where Athen currently leaves money on the table. Design doc; not yet implemented. Anthropic gap is severe (zero `cache_control` markers + missing `tools` field), DeepSeek gap is observability-only (cache fires but cost UI lies).
- [Memory](docs/MEMORY.md) — Read when: touching auto-recall injection, the post-turn `judge_worth_remembering` flow, the `memory_store` / `memory_recall` agent tools, or the relevance threshold. Memory is the episodic auto-recall store; distinct from Identity (always-on, in the static prefix). Covers dedup at all three layers (auto-judge sees existing memories, `memory_store` skips duplicates, recall threshold raised to 0.6).
- [Tool Expansion Menu](docs/TOOL_EXPANSION.md) — Read when: picking the next agent tool to wrap, or when an existing tool category needs a refresh. 10-category survey (browser, docs, OCR, GitHub, audio/video, db, social, outreach, scraping, jobs) with top pick + footprint + wrap shape + risk + fit per category. Picking menu, not a build plan; per-category implementation docs land when each category is shipped.
- [Integrations Push](docs/INTEGRATIONS_PUSH.md) — Read when: planning the "breadth of integrations" roadmap (custom MCP servers, personal-OAuth wraps, IMAP/SMTP autodetect, CalDAV/CardDAV sync, LLM-assisted credential setup). 5-move picking menu synthesized 2026-05-12 from parallel research. Shipped: Move #1 (MCP-BYO: process source, persistence, registry, Settings UI, risk overrides), Move #2 (email autodetect wizard), Move #4 (CalDAV sync), Move #5 (LLM error translator). Move #3 (OAuth: MS Graph, Notion, Linear) still design-only. Default answer to "should we wrap X?" — MCP-byo first, `http_request` preset second, bespoke wrap only when streaming/OAuth/state demand it.
- [Integrations & Capabilities Expansion](docs/INTEGRATIONS_EXPANSION.md) — Read when: deciding what to ship next across (A) productivity APIs, (B) deploy platforms, (C) whitespace proactive integrations, (D) competitor capability gaps. Picking menu synthesized 2026-05-15 from 4 parallel Haiku research streams. Top-10 ordered cross-stream; phase 1/2/3 ship order; explicit "out of scope" list. Extends INTEGRATIONS_PUSH — Move #3 OAuth wave reorders to MS Graph + Notion + Linear first. Authoritative answer to "should we wrap X next?".
- [Email Setup Wizard](docs/EMAIL_SETUP.md) — Read when: working on the Settings → Email panel, the IMAP/SMTP autodetect chain, the credential-test flow, the provider/app-password deep-link table, or the LLM error translator. SHIPPED 2026-05-22: `email_autodetect.rs`, `email_errors.rs`, `email_test.rs` landed in `athen-app`; sync imap 2.4.1 retained — async-imap migration deferred. Reuses `athen-sentidos/src/email.rs` polling monitor + `email_send.rs` SMTP sender as the data plane.
- [IMAP IDLE & Crate Migration](docs/IMAP_IDLE.md) — Read when: (a) Rust promotes the `SEMICOLON_IN_EXPRESSIONS_FROM_MACROS` lint to a hard error and our build breaks on transitive `imap-proto 0.10.2`, or (b) we want sub-second new-mail latency (IMAP IDLE / push). Deferred design; not implemented. Recommends `async-imap 0.11.x` over `imap 3.0.0-alpha.X` regardless of trigger; IDLE state machine (`Polling | Idling | ReIdling | Backoff`) runs alongside polling, doesn't replace it.
- [Subscription-Relay Providers](docs/SUBSCRIPTION_RELAY_PROVIDERS.md) — Read when: a user asks "can Athen use my Cursor/Windsurf/Copilot/Poe subscription as an API?". Picking menu synthesized 2026-05-13 from parallel research. Verdict: no new bespoke provider in `athen-llm`; ship at most two Cloud APIs presets (Poe official API + self-hosted Copilot relay) with disclaimers. Cursor/Windsurf reverse-engineered wrappers are off-limits (explicit ToS + active enforcement).
- [Provider Pinning](docs/PROVIDER_PINNING.md) — Read when: working on arc rehydration, the active-provider switcher, executor entry/exit (task-boundary detection), or any failure mode where "I switched providers and my running task broke." SHIPPED end-to-end (2026-05-23): pin store + resolver + per-arc router. `EffectiveProviderTarget {provider_id, pinned_slug}` returned by resolver; `arc_router_for` builds per-arc router with `override_slug` when pin in force; `build_router_for_provider` collapses every tier to pinned slug. Earlier landing only plumbed pin into compaction/temperature lookups — actual routing wasn't load-bearing until 2026-05-23 fix.
- [Bundles](docs/BUNDLES.md) — Read when: working on the Settings → Connections / Settings → Bundles UI rework, the `ProviderConfig` → `Connection` rename, the per-slug `SlugQuirksRegistry`, the model catalog (live `/models` + hardcoded curated), Bundle migration from today's `active_provider + tier_models`, or any cross-vendor mixing concern. Builds on shipped Provider Pinning (the 2026-05-23 load-bearing fix is the substrate). Splits today's coupled "active provider + family + slug + tier_models" into two orthogonal surfaces (Connections = credentials; Bundles = per-tier (connection, slug) loadouts, one active). SHIPPED (Phases 1-3): core types + migration, resolver reads active Bundle, Settings UI (Connections + Bundles panels), per-slug `SlugQuirks` + `BUILTIN_SLUG_QUIRKS` registry wired into `build_provider_instance`. One remaining gap: the `ProviderConfig` → `Connection` type rename (field is still `providers: HashMap<String, ProviderConfig>` in `config.rs`; the UI already says "Connections").
- [Reasoning Effort](docs/REASONING_EFFORT.md) — Read when: adding or tuning a model's reasoning/thinking knob, debugging "why is my Anthropic call burning 64k thinking tokens", or wiring per-arc / `delegate_to_agent` effort overrides. Picking-menu doc synthesized 2026-05-13 from 5 parallel Haiku research streams (OpenAI / Anthropic / Google / DeepSeek+xAI+Mistral / local-llamacpp). Single `ReasoningEffort` enum (Default/Off/Minimal/Low/Medium/High/Max) maps to 7 distinct wire shapes. SHIPPED (verified 2026-05-19): enum + `ChatRequest` field + `ArcMeta.reasoning_effort_override` + lifecycle methods + per-provider mappers + migration all live; per-arc setting and `delegate_to_agent` override both wired.
- [Skills](docs/SKILLS.md) — Read when: working on user-authored procedural playbooks the agent loads on demand (Claude-Code-style `SKILL.md` folders, `load_skill` tool, Settings → Skills panel). Distinct from Identity (always-on persona) and Memory (auto-recalled episodic facts). SHIPPED (SkillStore, `load_skill` tool, Settings panel, boot seeding, 2026-05-15/19). Per-arc idempotency (skip body re-fetch on repeated calls in same arc) is the one remaining design item.
- [Self-Support Skills](docs/SELF_SUPPORT_SKILLS.md) — Read when: working on Athen-as-IT-support: per-provider help modals (L1), `athen_docs` agent tool + `skills/system/` guides (L2), or proactive post-onboarding hints. L1 help modals + L2 `athen_docs` tool (11 system guides) + proactive hints sense SHIPPED. L3 (skill-gated structured-settings tools), `cargo xtask gen-skills`, and CI staleness check deferred. Builds on shipped Skills.
- [Benchmarks](docs/BENCHMARKS.md) — Read when: running a public benchmark against Athen, debugging a harness gotcha (SELinux relabel, glibc mismatch, shell-timeout caps, target/incremental bloat), or recording a new result. Headline: Athen + V4 Flash non-thinking scored 53.9% on TerminalBench 2.0 vs DeepSeek's 49.1% baseline (2026-05-22). Includes full reproduction steps for the TB2 run. Update this doc each time a new benchmark is run.
- [Cloud APIs Expansion](docs/CLOUD_APIS.md) — Read when: working on the `http_request` agent tool or the Registered HTTP Endpoints store. v0 SHIPPED 2026-05-10: generic `http_request` tool wired into `AppToolRegistry`, `RegisteredEndpoint` SQLite store (`athen-persistence/src/http_endpoints.rs`), vault-backed credentials under scope `endpoint:<uuid>`, in-process per-endpoint per-minute rate limiter (`athen-app/src/http_rate_limiter.rs`), Settings → Cloud APIs panel with 15 presets (`athen-app/src/http_presets.rs`: Jina, Firecrawl, Brave, SerpAPI, Hunter, Apollo, PDL, DeepL, NewsAPI, Open-Meteo, Frankfurter, OpenCage, ElevenLabs, OpenRouter, Groq). Per-call risk currently sits at the registry-level WritePersist default; per-method/per-endpoint dynamic risk is a follow-up. Doc still describes the design — code is authoritative for current behaviour.
- [Checkpointing & Undo](docs/CHECKPOINTING.md) — Read when: working on the agent action snapshot layer, the Changes side panel, "Revert this action" UI, or the `athen-checkpoint` crate. SHIPPED (phase 1): `GixCheckpointStore` in `athen-checkpoint` (pure-Rust `gix` backend, no `git` dep), bare repo at `<data_dir>/athen-snapshots`, branch-per-arc, one tag per destructive action, lazy sandbox-fenced snapshots of `write`/`edit` pre-state, `revert_action` restores file content. Shell snapshotting and the full Changes rail UI are follow-on phases. Design doc is the conceptual reference; doc header status says "phase 1 in flight" but the crate is live.
- [Interactive Onboarding](docs/TOOLS_AND_SENSES.md) — Read when: working on the `athen_setup` agent profile, the 6 setup tools (`setup_tools.rs`), the `create_setup_arc` Tauri command, or the proactive hints sense (`proactive_hints.rs`). SHIPPED: dedicated `athen_setup` profile drives a wizard arc at first launch; setup tools wrap IMAP/SMTP, CalDAV, Telegram, API keys, and contacts without requiring Tauri `State`; `proactive_hints.rs` fires rate-limited one-liner nudges (1/hr, permanently dismissable) via in-app notifications; `athen_docs` tool exposes 11 system guide topics. See also SELF_SUPPORT_SKILLS.md for L1/L2/L3 layering.

### Crate-level docs (read source, not a separate file)

- **Calendar source registry & sync loop** (in `athen-app/src/calendar_sources.rs`) — pulls events from configured remote sources into the local `CalendarStore`. Config rows live in `calendar_sources` (`athen-persistence`), credentials in the vault under `calendar:<uuid>:password`. `start_calendar_sync()` on AppState reads enabled rows, builds a `Box<dyn CalendarSource>` per row (currently routes only to `CalDavSource`), spawns one tokio loop per source. Default interval 5 min; pull window now-1d → now+30d. Reconciliation key: `(source_id, remote_id)`. INSERTS new, UPDATES on etag mismatch, DELETES rows whose start is inside the window but not in the pull. Local-only events (`source_id IS NULL`) are never deleted. Write direction (agent edits → remote) is deferred — agent tools still hit local store only; the next remote pull observes the divergence. Last-sync timestamp + last error stamped on the config row for the Settings panel. Tauri command surface in `settings_calendar.rs` (list/add/delete/enable/selected_calendars/test/sync_now/list_remote_calendars). Settings → Connections → Calendar Sources panel handles the full lifecycle: preset picker (iCloud / Google-via-CalDAV / Fastmail / Yandex / Nextcloud / Custom), credential capture, automatic test+first-sync after add, per-row actions to pick which calendars to sync.

- `athen-caldav` — CalDAV adapter implementing `athen_core::traits::calendar_source::CalendarSource`. One generic `CalDavSource` struct speaks RFC 4791 over HTTPS with HTTP Basic auth + an app-specific password. Same code handles iCloud, Google Calendar via its CalDAV endpoint (sidesteps Google OAuth verification entirely), Fastmail, Nextcloud, Yandex. Producer side of the pipeline: a sync loop in `athen-app` (task #26) pulls `RemoteEvent`s from each configured source and reconciles them with the existing `CalendarStore` (`athen-persistence/src/calendar.rs`) on the `(source_id, remote_id)` key. Internal modules: `client.rs` (PROPFIND/REPORT/PUT/DELETE helpers + Basic auth), `multistatus.rs` (quick-xml parser for `<D:multistatus>` envelopes), `discovery.rs` (current-user-principal → calendar-home-set → calendar collection enumeration, with the home-set URL cached on the struct), `ical_codec.rs` (minimal RFC 5545 VEVENT parse/emit — UTC datetimes, `VALUE=DATE` all-day, `RRULE` round-trip, `VALARM` triggers as reminder minutes; TZID-with-IANA support is a follow-up). `presets` module holds per-provider base URLs + credential-page hints used by the Settings UI (task #27).

- `athen-vault` — Encrypted credential vault. Two backends sharing the `Vault` trait from `athen-core::traits::vault`: `KeyringVault` (OS keychain via the `keyring` crate, with a SQLite index for `list`) and `EncryptedFileVault` (chacha20poly1305, random 32-byte master key at `<data_dir>/vault.key` mode 0600, AAD bound to `(scope, key)` so row-swap attacks fail). Use `athen_vault::open_vault(data_dir, "athen")` — it tries the keychain, self-checks with a sentinel round-trip, falls back to encrypted file on failure. Wired into `AppState::vault` in `athen-app`. Backs `http_request` registered endpoints (#184), future IMAP/SMTP credentials, and OAuth tokens. Single source of truth for at-rest secrets — never roll your own.

- `athen-checkpoint` — Gix-backed action snapshot store implementing `CheckpointStore` from `athen-core::traits::checkpoint`. `GixCheckpointStore` opens/initializes a bare git repo at `<data_dir>/athen-snapshots` using pure-Rust `gix` (no `git` subprocess). One branch per arc (`refs/heads/arc/<uuid>`); one tag per destructive action (`refs/tags/action/<entry_id>`). `snapshot_paths` captures file pre-state (or records "absent" for new-file creates) before each `write`/`edit` tool call; `revert_action` restores content from the tag. Snapshot is lazy + sandbox-fenced + size-capped (~50 MB/file). Operations are dispatched via `tokio::task::spawn_blocking` to keep async callers unblocked. Shell snapshotting is a follow-on phase.

- `athen-scheduler` — Wake-up scheduler. Two pieces: `compute_next_fire` (pure function, deterministic — computes next fire time after a reference timestamp for any `Schedule` variant) and `WakeupScheduler` (driver that polls `WakeupStore` for due wake-ups and hands them to a `WakeupFireSink`). `tick(now)` is the testable unit; `run(...)` wraps it in a tokio loop with a shutdown signal. Coalescing policy: `mark_fired` advances `next_fire_at` to the next slot strictly after now, so missed periodic fires collapse naturally. Wired into `AppState` alongside the `WakeupStore` in `athen-persistence`.
