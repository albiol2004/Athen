# Configuration & LLM Providers

## Configuration

TOML-based configuration with split files and sensible defaults.

### Config discovery (CLI and Tauri app)
1. `~/.athen/config.toml` -- user-level config (checked first)
2. `./config/config.toml` -- project-local config (fallback)
3. Built-in defaults if no file found

Both `athen-cli` and `athen-app` use the same discovery logic. The Tauri app also creates `~/.athen/` if it does not exist and opens SQLite at `~/.athen/athen.db`.

### Config files
- `config/config.toml` -- operation mode, security settings, persistence paths, email settings, telegram settings, notification settings (`[notifications]`: preferred_channels, escalation_timeout_secs, quiet_hours), embedding settings (`[embeddings]`: mode, provider, model, base_url, api_key)
- `config/models.toml` -- LLM providers (API keys, models), profiles (powerful/fast/code/cheap), domain-to-profile assignments
- `config/domains.toml` -- per-domain settings (model profile, max steps, timeout)

### API key resolution order
1. Saved config key (`~/.athen/models.toml`) -- takes priority (user explicitly saved via Settings UI)
2. Environment variable (e.g. `DEEPSEEK_API_KEY`, `OPENAI_API_KEY`) -- fallback
3. Config values like `${DEEPSEEK_API_KEY}` are treated as unresolved placeholders and skipped

This order ensures that keys explicitly saved through the Settings UI always take precedence over environment variables.

### Config loading API (`athen-core::config_loader`)
```rust
load_config(path) -> Result<AthenConfig>       // Load single TOML file
load_config_dir(dir) -> Result<AthenConfig>     // Load config.toml + optional models.toml
save_default_config(path) -> Result<()>         // Write defaults to file
```

---

## Domains

Tasks are classified into domains with optimized flows:

| Domain | Model Profile | Max Steps | Timeout | Notes |
|--------|--------------|-----------|---------|-------|
| base | fast | 50 | 5min | Fallback for uncategorized tasks |
| communication | fast | 20 | 3min | Group by thread, wait 30s for related messages |
| code | powerful | 100 | 15min | Require tests, sandbox execution |
| agenda | fast | 15 | 2min | Check conflicts, notify before events |
| files | fast | 30 | 5min | Document management |
| research | powerful | 50 | 10min | Web search, synthesis |

---

## LLM Configuration

### Providers
Anthropic, OpenAI, Google, DeepSeek, Ollama (local), llama.cpp (local). Each configurable with API key or OAuth. Any OpenAI-compatible endpoint supported via `OpenAiCompatibleProvider`.

### Model Profiles
- **Powerful**: Claude Opus -> Gemini Ultra -> o3 (fallback: DeepSeek)
- **Fast**: DeepSeek -> Gemini Pro -> Claude Sonnet
- **Code**: Claude Opus -> DeepSeek
- **Cheap**: DeepSeek -> Local models
- **Local**: Ollama models only (max privacy)

### Failover
If a model fails: try next in priority list. If rate limited: wait and retry same model. Circuit breaker if persistent failures.

### Budget
Optional daily USD limit with warning threshold. Per-provider rate limits. Token tracking.

---

## Embedding Configuration

Embeddings are configured separately from LLM providers and controlled entirely via the Tauri UI. No config files.

### Embedding Modes (`EmbeddingConfig`)
- **Automatic** (default): Auto-detect best available provider (NPU > GPU > Ollama > CPU > keyword fallback)
- **Cloud**: Use a cloud provider (OpenAI-compatible endpoint; requires API key)
- **LocalOnly**: Force local-only embeddings (no network calls; Ollama or keyword)
- **Specific**: Use a specific provider by ID (ollama, openai, etc.)
- **Off**: Disable memory/embeddings entirely

### Tauri Commands for Embedding Settings
- `save_embedding_settings(mode, provider?, model?, base_url?, api_key?)` — Save mode and optional provider details. API key `None` preserves existing; `Some("")` removes; `Some("sk-...")` updates.
- `test_embedding_provider(provider, model?, base_url?, api_key?)` — Test connectivity (Ollama `/api/embed`, cloud `/v1/embeddings`, keyword always succeeds)

### Supported Providers
- **Ollama** (local): `POST {base_url}/api/embed` with `model` and `input`
- **OpenAI-compatible** (cloud): `POST {base_url}/v1/embeddings` with auth header and `model`
- **Keyword** (fallback): TF-IDF-based hashing; no external calls needed

See: `athen-core/config.rs:EmbeddingConfig` (lines 315–353), `athen-app/src/settings.rs:save_embedding_settings` (1154–1191) and `test_embedding_provider` (1195–1252).

---

## MCP (Model Context Protocol) Configuration

MCPs provide tool integrations (filesystem access, web search, shell execution, etc.). Fully managed through the Tauri UI.

### Tauri Commands for MCP Management
- `list_mcp_catalog()` — Return all available MCPs with enable/disable state, current config, display name, description, and JSON schema for config validation.
- `enable_mcp(mcp_id, config)` — Enable an MCP with optional configuration and persist to database.
- `disable_mcp(mcp_id)` — Disable an MCP and persist the change.

MCPs are stored in the database (athen-mcp-store) and automatically loaded on startup. The `tools.md` document is refreshed after enable/disable.

See: `athen-app/src/commands.rs:list_mcp_catalog` (1853–1883), `enable_mcp` (1886–1906), `disable_mcp` (1909–1921); `athen-mcp` crate for builtin catalog.

---

## Directory Grants for MCPs

MCPs requesting filesystem access are mediated by a permission model with two scopes:

### Permission Scopes
- **Arc-scoped**: Grant applies only to a specific conversation/Arc. Requires one-time human approval via pending queue.
- **Global**: Grant applies to all future tasks. Persisted and reusable.

### Tauri Commands for Grant Management
- `list_pending_grants()` — Return pending filesystem access requests awaiting human approval (shows path, access type, requesting MCP).
- `resolve_pending_grant(id, decision)` — Approve or deny a pending grant (GrantDecision::Allow | Deny).
- `list_arc_grants(arc_id)` — List all grants for a specific Arc.
- `list_global_grants()` — List all global grants.
- `add_global_grant(path, access)` — Manually grant global filesystem access (read/write).
- `revoke_arc_grant(id)` — Revoke an Arc-scoped grant.
- `revoke_global_grant(id)` — Revoke a global grant.

Grants are persisted in SQLite and enforced by the FileGate before MCPs access the filesystem.

See: `athen-app/src/commands.rs:list_pending_grants` (1954–1959), `resolve_pending_grant` (1962–1976), `list_arc_grants` (1979–1993), `list_global_grants` (1996–2008), `add_global_grant` (2011–2030), `revoke_arc_grant` (2033–2042), `revoke_global_grant` (2045+); `athen-app/src/file_gate.rs` for FileGate implementation.

---

## Operation Modes

1. **Always-On**: PC stays awake 24/7. Immediate reactivity. ~15-30W idle.
2. **Wake Timer**: System suspends, wakes every N minutes for polling. ~2-5W average. Max delay = wake interval.
3. **Cloud Relay** (paid): Monitors run on cloud server, push to local PC. PC can be off. Immediate reactivity.

---

## External Dependencies

| Crate | Version | Used by | Purpose |
|-------|---------|---------|---------|
| `tokio` | 1.x (full) | All | Async runtime |
| `serde` / `serde_json` | 1.x | All | Serialization |
| `uuid` | 1.x (v4, serde) | All | Unique identifiers |
| `chrono` | 0.4 (serde) | All | Timestamps |
| `thiserror` | 2.x | athen-core | Error derive macro |
| `async-trait` | 0.1 | All traits | Async trait support |
| `tracing` | 0.1 | All | Structured logging |
| `tracing-subscriber` | 0.3 | athen-cli, athen-app | Structured log output |
| `url` | 2.x (serde) | athen-core | URL type for HttpApi backend |
| `tokio-stream` | 0.1 | athen-core, athen-llm | Stream trait for LLM streaming |
| `reqwest` | 0.12 (rustls-tls) | athen-llm, athen-app, athen-sentidos | HTTP client (pure Rust TLS) |
| `futures` | 0.3 | athen-llm | Stream utilities |
| `regex` | 1.x | athen-risk | Pattern matching for rules engine |
| `rusqlite` | 0.32 (bundled) | athen-persistence, athen-memory, athen-sentidos | Embedded SQLite |
| `sha2` | 0.10 | athen-persistence | Checkpoint integrity checksums |
| `tempfile` | 3.x | athen-ipc (dev) | Test socket paths |
| `toml` | 0.8 | athen-core, athen-app | TOML config parsing (core: loading, app: settings save/load) |
| `tauri` | 2.x | athen-app | Desktop app framework |
| `tauri-build` | 2.x | athen-app (build) | Tauri build system |
| `imap` | 2.4 (default-features = false) | athen-sentidos | IMAP client for email monitoring |
| `rustls-connector` | 0.22 | athen-sentidos | Sync TLS via rustls for IMAP (no OpenSSL) |
| `mailparse` | 0.16 | athen-sentidos | MIME email body parsing |
| `rustls` | 0.23 (aws_lc_rs) | athen-app | Crypto provider installation |
| `tokio-util` | 0.7 | athen-app | CancellationToken for notification escalation |

All HTTP uses `rustls-tls` (pure Rust) -- no OpenSSL system dependency needed.
