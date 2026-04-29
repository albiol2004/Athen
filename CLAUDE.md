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
│   ├── athen-cli/                # CLI runner (REPL)
│   ├── athen-app/                # Tauri desktop app (composition root)
│   └── mcps/
│       └── mcp-filesystem/       # Standalone MCP filesystem tool
```

## Design Principles (CRITICAL)

### 1. Hexagonal Architecture (Ports & Adapters)

`athen-core` defines ALL traits (ports). Every other crate implements adapters. No crate depends on a sibling -- only on `athen-core`. `athen-app` is the composition root that wires implementations together.

### 2. Dependency Rules

- `athen-core` depends on NOTHING internal (only serde, chrono, uuid, thiserror, async-trait, url, tokio-stream)
- All other crates depend on `athen-core` for trait definitions
- MCPs (`crates/mcps/*`) do NOT depend on `athen-core` -- standalone JSON-RPC servers
- Crates NEVER depend on sibling crates (except through `athen-core` traits)
- `athen-app` is the ONLY crate that depends on multiple siblings

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

## CI/CD

- `.github/workflows/ci.yml` -- clippy + tests on push to main + PRs
- `.github/workflows/release.yml` -- cross-platform Tauri builds (Linux/macOS/Windows)

## Detailed Documentation

Read the relevant doc BEFORE working on a feature area:

- [Architecture, Core Types & Security](docs/ARCHITECTURE.md) — Read when: adding/modifying traits, types, risk system, IPC, error handling, or security model
- [Implementation Status by Crate](docs/IMPLEMENTATION.md) — Read when: you need to understand what a crate does, its current state, test counts, or what files exist. Update this file after implementing changes.
- [Configuration & LLM Providers](docs/CONFIGURATION.md) — Read when: working on config loading, LLM providers, model profiles, failover, or domain settings
- [Tools, Senses & Notifications](docs/TOOLS_AND_SENSES.md) — Read when: working on agent tools, sense monitors, sandbox execution, or notification delivery
