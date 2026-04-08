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
