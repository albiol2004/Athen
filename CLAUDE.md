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
├── frontend/                     # Web frontend (HTML/CSS/JS)
│   ├── index.html, styles.css, app.js
├── crates/
│   ├── athen-core/               # Shared types + trait contracts (THE CONTRACTS)
│   ├── athen-ipc/                # IPC transport layer
│   ├── athen-sentidos/           # Sense monitors (email, calendar, messaging, telegram, user)
│   ├── athen-coordinador/        # Coordinator (router, risk eval, queue, dispatch)
│   ├── athen-agent/              # Agent worker (LLM executor, auditor, timeout)
│   ├── athen-llm/                # LLM provider adapters + router + failover + embeddings
│   ├── athen-web/                # Web search + page-reader providers (DDG, Tavily, Local, Jina, Wayback, Cloudflare)
│   ├── athen-mcp/                # MCP runtime catalog + registry (enable/config/spawn MCPs)
│   ├── athen-memory/             # Vector index + knowledge graph + SQLite
│   ├── athen-risk/               # Risk scorer + regex rules + LLM fallback
│   ├── athen-persistence/        # SQLite persistence, checkpoints, arcs, calendar, contacts
│   ├── athen-contacts/           # Contact trust model + risk multipliers
│   ├── athen-sandbox/            # OS-native + container sandboxing
│   ├── athen-shell/              # Nushell embedding + native shell fallback
│   ├── athen-vault/              # Encrypted credential vault (OS keychain + encrypted-file fallback)
│   ├── athen-caldav/             # CalDAV adapter (iCloud / Google-via-CalDAV / Fastmail / Nextcloud)
│   ├── athen-cli/                # CLI runner (REPL)
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
- [Arc Compaction](docs/ARC_COMPACTION.md) — Read when: working on context-window management, arc summarization, or anything that touches `ArcStore::load_entries` in the executor path. Design doc; not yet implemented.
- [Multi-Intent Routing](docs/MULTI_INTENT_ROUTING.md) — Read when: extending the Telegram owner-message path to split a single message into N per-arc intents (task #152), OR adding standing instructions / coordinator-as-agent memory ("for the next 4h, reply on Telegram"). Design doc; not yet implemented. Builds on #149's single-intent heuristic.
- [Identity](docs/IDENTITY.md) — Read when: building the user-maintained personality/rules/knowledge/team store that feeds every agent's static prefix. Categories are user-editable, entries are tagged with `applies_to` so each agent profile only pays for the sections it needs. Distinct from `athen-memory` (auto-learned, recalled per-query) and from agent profiles (define what an agent does, not who Athen is). Design doc; not yet implemented.
- [Wake-ups](docs/WAKEUPS.md) — Read when: working on scheduled / recurring / one-shot proactive triggers (reminders, daily digests, deferred follow-ups), including the agent-authored `create_wakeup` tool. Wake-ups are synthetic sense events with a clock as their trigger; they reuse coordinator/risk/dispatch. Risk model is "pre-approve capability, not content" via per-wake-up `AutonomyBand` + tool/contact allowlists, with the existing per-action risk gate still firing at fire time. Implemented; doc remains the conceptual reference.
- [Packaging & Distribution](docs/PACKAGING.md) — Read when: cutting a release, debugging the auto-updater, or adding a distribution channel. Covers AUR, COPR, GitHub Releases, the `installer_kind` self-update vs system-package split, and the per-release checklist.
- [Per-Model Quirks](docs/PER_MODEL_QUIRKS.md) — Read when: adding a new model/family to the provider stack, debugging tool-call extraction failures, reasoning-content surface mismatches, or strict-template HTTP 500s on local inference (Qwen/Gemma/DeepSeek). Design doc; ToolExtraction/Reasoning/TemplateStrictness axes are partially implemented (TemplateStrictness via `external_system_suffix`); ToolExtraction + Reasoning still pending. User-driven family selection (UI dropdown + editable slug), no auto-detection.
- [Prompt Caching](docs/PROMPT_CACHING.md) — Read when: touching any LLM provider request/response path, the `TokenUsage` struct, or cost estimation. Per-provider audit (Anthropic/OpenAI/DeepSeek/Google) of how prompt caching works on the wire and where Athen currently leaves money on the table. Design doc; not yet implemented. Anthropic gap is severe (zero `cache_control` markers + missing `tools` field), DeepSeek gap is observability-only (cache fires but cost UI lies).
- [Memory](docs/MEMORY.md) — Read when: touching auto-recall injection, the post-turn `judge_worth_remembering` flow, the `memory_store` / `memory_recall` agent tools, or the relevance threshold. Memory is the episodic auto-recall store; distinct from Identity (always-on, in the static prefix). Covers dedup at all three layers (auto-judge sees existing memories, `memory_store` skips duplicates, recall threshold raised to 0.6).
- [Tool Expansion Menu](docs/TOOL_EXPANSION.md) — Read when: picking the next agent tool to wrap, or when an existing tool category needs a refresh. 10-category survey (browser, docs, OCR, GitHub, audio/video, db, social, outreach, scraping, jobs) with top pick + footprint + wrap shape + risk + fit per category. Picking menu, not a build plan; per-category implementation docs land when each category is shipped.
- [Integrations Push](docs/INTEGRATIONS_PUSH.md) — Read when: planning the "breadth of integrations" roadmap (custom MCP servers, personal-OAuth wraps, IMAP/SMTP autodetect, CalDAV/CardDAV sync, LLM-assisted credential setup). 5-move picking menu synthesized 2026-05-12 from parallel research. Sets the default answer to "should we wrap X?" — MCP-byo first, `http_request` preset second, bespoke wrap only when streaming/OAuth/state demand it.
- [Integrations & Capabilities Expansion](docs/INTEGRATIONS_EXPANSION.md) — Read when: deciding what to ship next across (A) productivity APIs, (B) deploy platforms, (C) whitespace proactive integrations, (D) competitor capability gaps. Picking menu synthesized 2026-05-15 from 4 parallel Haiku research streams. Top-10 ordered cross-stream; phase 1/2/3 ship order; explicit "out of scope" list. Extends INTEGRATIONS_PUSH — Move #3 OAuth wave reorders to MS Graph + Notion + Linear first. Authoritative answer to "should we wrap X next?".
- [Email Setup Wizard](docs/EMAIL_SETUP.md) — Read when: working on the Settings → Email panel, the IMAP/SMTP autodetect chain, the credential-test flow, the provider/app-password deep-link table, or the LLM error translator. Move #2 of the integrations push. Design doc synthesized 2026-05-12 from 5 parallel research streams; reuses the existing `athen-sentidos/src/email.rs` polling monitor + `email_send.rs` SMTP sender as the data plane (no async-imap migration in scope).
- [Subscription-Relay Providers](docs/SUBSCRIPTION_RELAY_PROVIDERS.md) — Read when: a user asks "can Athen use my Cursor/Windsurf/Copilot/Poe subscription as an API?". Picking menu synthesized 2026-05-13 from parallel research. Verdict: no new bespoke provider in `athen-llm`; ship at most two Cloud APIs presets (Poe official API + self-hosted Copilot relay) with disclaimers. Cursor/Windsurf reverse-engineered wrappers are off-limits (explicit ToS + active enforcement).
- [Provider Pinning](docs/PROVIDER_PINNING.md) — Read when: working on arc rehydration, the active-provider switcher, executor entry/exit (task-boundary detection), or any failure mode where "I switched providers and my running task broke." Design doc; not yet implemented. Mechanism: `ArcRow.pinned_provider_id` set on first LLM call of a task, cleared when arc returns to idle. Distinct from a user-set durable per-arc override.
- [Reasoning Effort](docs/REASONING_EFFORT.md) — Read when: adding or tuning a model's reasoning/thinking knob, debugging "why is my Anthropic call burning 64k thinking tokens", or wiring per-arc / `delegate_to_agent` effort overrides. Picking-menu doc synthesized 2026-05-13 from 5 parallel Haiku research streams (OpenAI / Anthropic / Google / DeepSeek+xAI+Mistral / local-llamacpp). Single `ReasoningEffort` enum (Default/Off/Minimal/Low/Medium/High/Max) maps to 7 distinct wire shapes. Design doc; not yet implemented.
- [Skills](docs/SKILLS.md) — Read when: working on user-authored procedural playbooks the agent loads on demand (Claude-Code-style `SKILL.md` folders, `load_skill` tool, Settings → Skills panel). Distinct from Identity (always-on persona) and Memory (auto-recalled episodic facts). Design doc; not yet implemented.
- [Cloud APIs Expansion](docs/CLOUD_APIS.md) — Read when: working on the `http_request` agent tool or the Registered HTTP Endpoints store. v0 SHIPPED 2026-05-10: generic `http_request` tool wired into `AppToolRegistry`, `RegisteredEndpoint` SQLite store (`athen-persistence/src/http_endpoints.rs`), vault-backed credentials under scope `endpoint:<uuid>`, in-process per-endpoint per-minute rate limiter (`athen-app/src/http_rate_limiter.rs`), Settings → Cloud APIs panel with 15 presets (`athen-app/src/http_presets.rs`: Jina, Firecrawl, Brave, SerpAPI, Hunter, Apollo, PDL, DeepL, NewsAPI, Open-Meteo, Frankfurter, OpenCage, ElevenLabs, OpenRouter, Groq). Per-call risk currently sits at the registry-level WritePersist default; per-method/per-endpoint dynamic risk is a follow-up. Doc still describes the design — code is authoritative for current behaviour.

### Crate-level docs (read source, not a separate file)

- **Calendar source registry & sync loop** (in `athen-app/src/calendar_sources.rs`) — pulls events from configured remote sources into the local `CalendarStore`. Config rows live in `calendar_sources` (`athen-persistence`), credentials in the vault under `calendar:<uuid>:password`. `start_calendar_sync()` on AppState reads enabled rows, builds a `Box<dyn CalendarSource>` per row (currently routes only to `CalDavSource`), spawns one tokio loop per source. Default interval 5 min; pull window now-1d → now+30d. Reconciliation key: `(source_id, remote_id)`. INSERTS new, UPDATES on etag mismatch, DELETES rows whose start is inside the window but not in the pull. Local-only events (`source_id IS NULL`) are never deleted. Write direction (agent edits → remote) is deferred — agent tools still hit local store only; the next remote pull observes the divergence. Last-sync timestamp + last error stamped on the config row for the Settings panel.

- `athen-caldav` — CalDAV adapter implementing `athen_core::traits::calendar_source::CalendarSource`. One generic `CalDavSource` struct speaks RFC 4791 over HTTPS with HTTP Basic auth + an app-specific password. Same code handles iCloud, Google Calendar via its CalDAV endpoint (sidesteps Google OAuth verification entirely), Fastmail, Nextcloud, Yandex. Producer side of the pipeline: a sync loop in `athen-app` (task #26) pulls `RemoteEvent`s from each configured source and reconciles them with the existing `CalendarStore` (`athen-persistence/src/calendar.rs`) on the `(source_id, remote_id)` key. Internal modules: `client.rs` (PROPFIND/REPORT/PUT/DELETE helpers + Basic auth), `multistatus.rs` (quick-xml parser for `<D:multistatus>` envelopes), `discovery.rs` (current-user-principal → calendar-home-set → calendar collection enumeration, with the home-set URL cached on the struct), `ical_codec.rs` (minimal RFC 5545 VEVENT parse/emit — UTC datetimes, `VALUE=DATE` all-day, `RRULE` round-trip, `VALARM` triggers as reminder minutes; TZID-with-IANA support is a follow-up). `presets` module holds per-provider base URLs + credential-page hints used by the Settings UI (task #27).

- `athen-vault` — Encrypted credential vault. Two backends sharing the `Vault` trait from `athen-core::traits::vault`: `KeyringVault` (OS keychain via the `keyring` crate, with a SQLite index for `list`) and `EncryptedFileVault` (chacha20poly1305, random 32-byte master key at `<data_dir>/vault.key` mode 0600, AAD bound to `(scope, key)` so row-swap attacks fail). Use `athen_vault::open_vault(data_dir, "athen")` — it tries the keychain, self-checks with a sentinel round-trip, falls back to encrypted file on failure. Wired into `AppState::vault` in `athen-app`. Backs `http_request` registered endpoints (#184), future IMAP/SMTP credentials, and OAuth tokens. Single source of truth for at-rest secrets — never roll your own.
