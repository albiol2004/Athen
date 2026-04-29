# Athen

> **AI agents for everyday people, on every platform.**

[![CI](https://github.com/albiol2004/Athen/actions/workflows/ci.yml/badge.svg)](https://github.com/albiol2004/Athen/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![Built with Rust](https://img.shields.io/badge/built_with-Rust-orange.svg)](https://www.rust-lang.org)
[![Tauri 2](https://img.shields.io/badge/Tauri-2-24C8DB.svg)](https://tauri.app)

Athen is a native desktop AI agent that watches your inbox, calendar, and
messages, decides what needs doing, and does it — autonomously, with a risk
system that knows when to act silently and when to ask first.

It's built for people who don't want to learn what an LLM is. **Single
binary, zero runtime dependencies, runs on Linux, macOS, and Windows.** Bring
your own API key, or run it fully offline against a local model.

> ⚠️ **Status: alpha.** The core agent loop, tool surface, and infrastructure
> are working and well-tested. The desktop UI is intentionally rough — it's
> the next thing we're polishing before a public launch. If you're here from
> a launch post, you're early.

---

## Table of contents

- [Why Athen?](#why-athen)
- [What works today](#what-works-today)
- [Architecture](#architecture)
- [Tech stack](#tech-stack)
- [Getting started](#getting-started)
- [LLM providers](#llm-providers)
- [Tools the agent has](#tools-the-agent-has)
- [Privacy & safety](#privacy--safety)
- [Roadmap](#roadmap)
- [Contributing](#contributing)
- [License](#license)

---

## Why Athen?

Today's AI assistants fall into two camps:

- **Developer-first agents** like Claude Code, Cursor, Aider — incredible for
  code, but they assume you live in a terminal and know what an LLM is.
- **Cloud-native SaaS agents** — convenient, but you pay per seat, the agent
  runs on someone else's machine, and your inbox/calendar/contacts get mailed
  off to a third party to be processed.

There's nothing in the middle for the **everyday person** who just wants
something that handles their email, schedules their calendar, reminds them
about birthdays, drafts replies in their voice, and books their flights —
running on **their own laptop**, with **their own keys** (or no keys at
all), respecting **their data**.

Athen is that. A single native app, written in Rust for speed and a tiny
footprint. Native UI via Tauri 2 — not a Chromium fork shipping with every
release. Tools, sandboxing, risk evaluation, and persistence all live on
your machine. The only thing that leaves is what you explicitly route to a
remote LLM provider, and you can avoid even that by running locally.

---

## What works today

| Capability | Status | Notes |
|---|---|---|
| **Core agent loop** | ✅ Working | LLM-driven tool calling, streaming, cancellation, completion judge |
| **Shell + filesystem tools** | ✅ Working | `shell_execute`/`shell_spawn`/`shell_kill`/`shell_logs`, `read`/`edit`/`write`/`grep`, `list_directory` — sandboxed when bubblewrap is available |
| **In-session memory** | ✅ Working | `memory_store` / `memory_recall` — survives across the conversation |
| **Persistent semantic memory** | ✅ Working | Vector index + knowledge graph in SQLite, Ollama / OpenAI / TF-IDF embeddings |
| **Web search & fetch** | ✅ Working | DuckDuckGo (no key) + Tavily (optional). `web_fetch` chains a static reader → Jina Reader (no-key JS rendering) → Wayback Machine for paywalls/dead pages |
| **Calendar & contacts** | ✅ Working | Local SQLite, trust-level model that grows risk multipliers for unknown senders |
| **MCP runtime** | ✅ Working | Spawn and route tools through Model Context Protocol servers (stdio JSON-RPC) |
| **Senses** | 🟡 Working | Email (IMAP), Calendar, Telegram, generic User input — solid pipeline, more sources coming |
| **Risk system** | ✅ Working | Regex rule engine + LLM fallback, per-action base impact + contact trust multipliers |
| **Sandbox** | ✅ Working | OS-native (bwrap/Landlock on Linux, macOS sandbox-exec, Windows job objects), Podman/Docker tier for higher risk |
| **LLM provider routing** | ✅ Working | Failover, circuit breakers, budget tracker, supports Anthropic / DeepSeek / OpenAI-compatible / Ollama / llama.cpp |
| **Desktop UI** | 🟠 Functional but ugly | Tauri 2 app loads, agent runs, basic chat works — visual design is the next focus |
| **Onboarding flow** | ❌ Not yet | First-launch wizard for picking a provider and pasting keys is the next big UX piece |
| **Vision (screenshots/images)** | ❌ Not yet | On the roadmap |
| **Voice (STT/TTS)** | ❌ Not yet | On the roadmap |

---

## Architecture

Athen is built around **hexagonal architecture (ports and adapters)**. The
`athen-core` crate defines every trait; every other crate is a swappable
adapter that implements one. No internal crate depends on its siblings —
they all depend on `athen-core`. The `athen-app` crate is the composition
root that wires concrete implementations together.

```
┌─────────────────────────────────────────────────────────────┐
│                   SENTIDOS (Monitors)                       │
│   Email IMAP, Calendar polls, Telegram, user input, ...     │
│   Each runs as its own process, normalizes to SenseEvents   │
└──────────────────────┬──────────────────────────────────────┘
                       │ IPC (Unix sockets / Named pipes)
                       ▼
┌─────────────────────────────────────────────────────────────┐
│                  SENSE ROUTER (Tauri app)                   │
│   Triage → Arc creation → routes high-relevance events on   │
└──────────────────────┬──────────────────────────────────────┘
                       │
                       ▼
┌─────────────────────────────────────────────────────────────┐
│                       COORDINADOR                           │
│   Risk evaluation, priority queue, dispatch to workers      │
└──────────────────────┬──────────────────────────────────────┘
                       │ TaskAssignments
        ┌──────────────┼──────────────┐
        ▼              ▼              ▼
   ┌────────┐    ┌────────┐    ┌────────┐
   │ Agent  │    │ Agent  │    │ Agent  │
   │ worker │    │ worker │    │ worker │
   └───┬────┘    └───┬────┘    └───┬────┘
       └─────────────┼─────────────┘
                     ▼
┌─────────────────────────────────────────────────────────────┐
│                    EXECUTION LAYER                          │
│   Tools: shell + files + web + memory + MCP + ...           │
│   Sandboxed by risk tier (OS-native or container)           │
└─────────────────────────────────────────────────────────────┘
```

Each crate is independently testable: mock the trait, not the service.

```
crates/
├── athen-core/         # Trait definitions, shared types — depends on nothing internal
├── athen-ipc/          # Multi-process IPC transport
├── athen-sentidos/     # Sense monitors (email, calendar, telegram, ...)
├── athen-coordinador/  # Coordinator: router, risk eval, queue, dispatch
├── athen-agent/        # Agent worker: LLM executor, tools, auditor, timeout
├── athen-llm/          # LLM provider adapters + router + failover + embeddings
├── athen-web/          # Web search + page reader providers
├── athen-mcp/          # MCP runtime catalog + registry
├── athen-memory/       # Vector index + knowledge graph + SQLite
├── athen-risk/         # Risk scoring: regex rules + LLM fallback
├── athen-persistence/  # SQLite persistence, checkpoints, arcs, calendar, contacts
├── athen-contacts/     # Contact trust model + risk multipliers
├── athen-sandbox/      # OS-native + container sandboxing
├── athen-shell/        # Nushell embedding + native shell fallback
├── athen-cli/          # CLI runner (REPL)
├── athen-app/          # Tauri 2 desktop app — the composition root
└── mcps/
    └── mcp-filesystem/ # Standalone MCP filesystem server
```

For a deeper dive see [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

---

## Tech stack

| Layer | Choice | Why |
|---|---|---|
| Language | **Rust** | Speed, memory safety, single static binary |
| UI shell | **Tauri 2** | Native window per OS, much smaller than Electron |
| Frontend | HTML/CSS/JS | Plain — no framework lock-in for a UI that will keep changing |
| Database | **SQLite** | Embedded, serverless, portable. No external db to install |
| Shell embed | **Nushell** + native fallback | Cross-platform consistent shell on every OS |
| Sandbox | bwrap / Landlock / sandbox-exec / Job Objects, Podman/Docker | Tiered isolation. Zero user setup for the OS-native tier |
| HTTP | reqwest + rustls | No OpenSSL dependency, builds clean everywhere |
| LLM | Anthropic / DeepSeek / OpenAI-compat / Ollama / llama.cpp | Bring-your-own or run local |

---

## Getting started

> Pre-built binaries land with the v0.1.0 tag. Until then, build from source.

### System dependencies

**Linux (Fedora):**
```bash
sudo dnf install webkit2gtk4.1-devel gtk3-devel libsoup3-devel \
                 libappindicator-gtk3-devel
```

**Linux (Ubuntu/Debian):**
```bash
sudo apt-get install libwebkit2gtk-4.1-dev libgtk-3-dev libsoup-3.0-dev \
                     libappindicator3-dev librsvg2-dev cmake nasm
```

**macOS:** Xcode Command Line Tools (`xcode-select --install`) is enough.

**Windows:** the [Microsoft Visual C++ Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/) and [WebView2](https://developer.microsoft.com/en-us/microsoft-edge/webview2/) (preinstalled on Windows 10+).

### Build & run

```bash
# Clone
git clone https://github.com/albiol2004/Athen.git
cd Athen

# Build the workspace
cargo build --workspace --release

# Run the desktop app
cargo run -p athen-app --release

# Or run the CLI (REPL)
cargo run -p athen-cli --release
```

### Configuration

Athen reads its config from `~/.athen/`. The plan is for everything to be
configurable via the desktop UI (target: zero config files for end users).
Today, while the onboarding UI is being built, you'll need to drop a config
or set environment variables — see [`docs/CONFIGURATION.md`](docs/CONFIGURATION.md)
for the layout.

For the local-only path, install [Ollama](https://ollama.com/) or
[llama.cpp](https://github.com/ggerganov/llama.cpp), point Athen at the
local server, and you're running with no API keys at all.

---

## LLM providers

Mix and match — Athen routes by **profile** (Powerful / Fast / Code /
Cheap) so you can put Claude on the heavy work and a local model on the
cheap work.

| Provider | Type | Notes |
|---|---|---|
| **Anthropic** | Cloud | Claude Opus / Sonnet / Haiku via `/v1/messages` |
| **DeepSeek** | Cloud | OpenAI-compatible at `api.deepseek.com`. Cheap and capable |
| **OpenAI-compatible** | Cloud or local | Generic adapter — works with OpenAI proper, Together, Groq, OpenRouter, etc. |
| **Ollama** | Local | Wraps the OpenAI-compatible adapter against `localhost:11434` |
| **llama.cpp** | Local | Wraps the OpenAI-compatible adapter against `localhost:8080`. Works great with Qwen, Llama, Mistral GGUFs |

All providers support streaming. The router has per-provider circuit
breakers and a failover chain — if your primary times out, it transparently
falls through to the next one in the priority list.

---

## Tools the agent has

The agent has 13 built-in tools, plus everything exposed through the MCP
registry. See [`docs/TOOLS_AND_SENSES.md`](docs/TOOLS_AND_SENSES.md) for
the full reference.

**Shell & files:** `shell_execute`, `shell_spawn` / `shell_kill` /
`shell_logs` (long-running processes), `read`, `edit` (exact-string
replace, requires prior read), `write`, `grep` (ripgrep), `list_directory`.

**Memory:** `memory_store` / `memory_recall` (in-session HashMap;
overridden to persistent semantic memory when wired through
`AppToolRegistry`).

**Web:** `web_search` (DuckDuckGo by default, Tavily on opt-in), `web_fetch`
(static → Jina Reader → Wayback Machine fallback chain — handles SPAs and
paywalled pages without bundling a headless browser).

**Calendar:** `calendar_list` / `calendar_create` / `calendar_update` /
`calendar_delete` — local SQLite, ISO-8601 with the user's local timezone.

**Contacts:** `contacts_list` / `contacts_search` / `contacts_create` /
`contacts_update` / `contacts_delete` — trust levels learned implicitly
from approval/rejection patterns.

**MCP servers:** any tool exposed by an enabled MCP server, namespaced
`<mcp_id>__<tool_name>`. The bundled `mcp-filesystem` server is one example.

---

## Privacy & safety

Athen is built around the assumption that **your data is yours**. A few
concrete things that fall out of that:

- **No telemetry.** Period. There's no analytics SDK, no crash reporter
  that phones home, nothing. Your usage is between you and the LLM
  provider you chose.
- **All data stays local.** Calendar, contacts, memory, conversation
  history — all in SQLite under `~/.athen/`. Nothing syncs to a cloud
  unless you explicitly enable a sync feature (none are planned for v0.1).
- **Risk system before action.** Every tool call carries a base impact
  (Read / WritePersist) which is multiplied by contact trust level. High-risk
  actions either prompt you or require explicit pre-grant. See
  [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for the threat model.
- **Sandboxed shell.** `shell_execute` runs through `RuleEngine` (regex
  rules + LLM fallback) which blocks Danger/Critical commands before
  dispatch, then executes inside a `RestrictedWrite` sandbox: writable in
  `/tmp`, your home dir, the agent workspace, and any per-arc grants;
  read-only everywhere else. Falls back to unsandboxed only if no
  sandboxing tier is available.
- **Bring your own keys (or no keys).** The local-only path with Ollama or
  llama.cpp doesn't ever talk to a third party. Cloud LLM keys, when used,
  live in your config — they aren't bundled with the app or shared.

---

## Roadmap

**Pre-launch (in flight):**
- Onboarding flow + settings UI for provider keys
- Visual polish on the desktop UI
- Cross-platform CI matrix (Linux / macOS / Windows)
- v0.1.0 tagged release with pre-built binaries

**Near-term after launch:**
- Vision (screenshot tool, image-input LLM routing)
- Voice (STT/TTS) for hands-free interaction
- Multi-step planning tool for complex tasks
- More senses: WhatsApp, Slack, Discord, RSS

**Bigger picture:**
- Mobile companion (React Native or native — undecided)
- Plugin marketplace beyond MCP
- Federated multi-device sync (CRDT-based, end-to-end encrypted)

---

## Contributing

Issues, PRs, and discussions are welcome. See [`CONTRIBUTING.md`](CONTRIBUTING.md)
for the dev loop and house style. Short version: clippy must be clean
(`-D warnings`), tests must pass, and `athen-core` does not depend on its
siblings. Each crate stays independently testable.

For a deeper architectural orientation read these in order:

1. [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) — types, traits, risk model
2. [`docs/IMPLEMENTATION.md`](docs/IMPLEMENTATION.md) — what every crate does
3. [`docs/CONFIGURATION.md`](docs/CONFIGURATION.md) — config and LLM providers
4. [`docs/TOOLS_AND_SENSES.md`](docs/TOOLS_AND_SENSES.md) — the tool surface

---

## Security

Found a security issue? Please **don't** open a public issue. See
[`SECURITY.md`](SECURITY.md) for the disclosure process.

---

## License

MIT — see [`LICENSE`](LICENSE). Use it however you like.

---

## Acknowledgements

Athen stands on a lot of giants' shoulders:

- [**Tauri**](https://tauri.app) for making native desktop apps in Rust actually pleasant
- The [**Rust async ecosystem**](https://tokio.rs) — `tokio`, `reqwest`, `serde`, `tracing`
- [**SQLite**](https://sqlite.org) — the unsung hero of every desktop app
- The [**Model Context Protocol**](https://modelcontextprotocol.io) team for a clean tool-server standard
- [**Claude Code**](https://claude.com/claude-code) for showing what a great agent harness feels like

Built by [Alejandro Garcia](https://alejandrogarcia.blog).
