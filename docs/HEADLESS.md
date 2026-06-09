# Headless Mode

Athen can run as a GUI-less daemon — the full autonomous stack (sense
monitors, coordinator, risk gates, dispatch loop, wake-up scheduler, CalDAV
sync) on a plain tokio runtime, with **Telegram as the user surface**:
owner messages drive the agent, notifications arrive as bot messages, and
approval prompts come with inline keyboards. This is the deployment shape
for servers, containers, and (later) the hosted/cloud offering.

> Audience note: desktop Athen keeps its "all config via UI, never config
> files" rule — that rule exists for non-technical users. Headless mode is
> operator-facing by definition, so files + env vars are the interface.

## Quick start

```bash
# one isolated instance
ATHEN_DATA_DIR=/srv/athen/alice \
ATHEN_VAULT_BACKEND=file \
ATHEN_TELEGRAM_BOT_TOKEN=123456:ABC... \
ATHEN_PROVIDER_DEEPSEEK_API_KEY=sk-... \
athen --headless
```

`--headless` (or `ATHEN_HEADLESS=1`) is checked before any GTK/WebKit
touchpoint; no display server is needed. SIGINT/SIGTERM run the same
graceful shutdown coordinator as the desktop Quit path (monitors drained,
live agent runs finalized as cancelled, spawned processes killed, WAL
checkpointed), so `docker stop` is clean.

## Per-instance isolation

Everything an instance owns lives under one directory, so containers /
multi-tenant hosts isolate by pointing each instance at its own tree:

| Env var | Meaning | Default |
|---|---|---|
| `ATHEN_DATA_DIR` | Data tree root: `config.toml`, `models.toml`, vault, SQLite, workspace, skills, snapshots | `~/.athen` |
| `ATHEN_WORKSPACE_DIR` | Where relative paths in file/shell tools resolve | `<data_dir>/workspace` |
| `ATHEN_VAULT_BACKEND` | `file` \| `keyring` \| `auto` | `auto` |
| `ATHEN_HEADLESS` | `1`/`true` = same as `--headless` | unset |

Set `ATHEN_VAULT_BACKEND=file` in containers: there is no secret-service
daemon, and forcing the encrypted-file backend skips the D-Bus probe
entirely (in `auto` mode the fallback still engages, just noisier and, with
a present-but-locked keyring daemon, potentially slow).

## Credential separation

Three layers, later wins:

1. **Config files** (`config.toml` / `models.toml` in `ATHEN_DATA_DIR`) —
   non-secret config: which providers/bundles exist, IMAP/SMTP hosts,
   telegram enabled + owner id, intervals. Mount these read-only from your
   orchestration layer. (They *can* carry plaintext keys for dev setups,
   but don't ship secrets in images/volumes.)
2. **Encrypted file vault** (`vault.db` + `vault.key`, chacha20poly1305) —
   persistent secrets for standalone headless boxes. Seed without a GUI:

   ```bash
   echo "$DEEPSEEK_KEY" | athen-cli vault set provider:deepseek api_key
   echo "$BOT_TOKEN"    | athen-cli vault set telegram bot_token
   athen-cli vault list provider:deepseek
   ```

   (`athen-cli vault --help` documents the scope/key conventions. Seed and
   serve with the same `ATHEN_DATA_DIR` + `ATHEN_VAULT_BACKEND`.)
3. **Env-var overlay** — orchestrator-injected secrets, the Docker/K8s
   native path. Applied after vault hydration on every config (re)read, so
   they also survive per-arc router rebuilds:

   | Variable | Target |
   |---|---|
   | `ATHEN_PROVIDER_<ID>_API_KEY` | provider api_key (`<ID>` = provider id uppercased, non-alphanumerics → `_`; e.g. `opencode_go` → `OPENCODE_GO`) |
   | `ATHEN_TELEGRAM_BOT_TOKEN` | Telegram bot token |
   | `ATHEN_IMAP_PASSWORD` / `ATHEN_SMTP_PASSWORD` | email credentials |
   | `ATHEN_WEBSEARCH_BRAVE_API_KEY` / `ATHEN_WEBSEARCH_TAVILY_API_KEY` | web search |
   | `ATHEN_EMBEDDING_API_KEY` | cloud embeddings |

   Every variable also accepts a `_FILE` suffix form whose value is a path
   to a mounted secret file (`ATHEN_TELEGRAM_BOT_TOKEN_FILE=/run/secrets/bot_token`),
   trailing newline trimmed — i.e. Docker secrets work out of the box.

Secrets never need to be baked into images or written into the mounted
config files; the on-disk `config.toml`/`models.toml` can keep blanked
(`auth = "None"`) credential fields.

## Docker

A reference `Dockerfile` lives at the repo root (Debian-based, two-stage).
The binary still *links* WebKitGTK/GTK (Tauri's types are compiled in even
though headless never initializes them), so the runtime image carries those
libs — functional today, image-size optimization (a Tauri-free runtime
crate) is future work.

```bash
docker build -t athen .

docker run -d --name athen-alice \
  -v athen-alice-data:/data \
  -e ATHEN_TELEGRAM_BOT_TOKEN_FILE=/run/secrets/alice_bot \
  -e ATHEN_PROVIDER_DEEPSEEK_API_KEY_FILE=/run/secrets/alice_deepseek \
  athen
```

The image sets `ATHEN_DATA_DIR=/data`, `ATHEN_VAULT_BACKEND=file`,
`ATHEN_HEADLESS=1`. Run N users as N containers with N volumes; the
single-instance lock is deliberately not engaged in headless mode.

Minimal `config.toml` to drop into the volume (or build a sidecar init
that writes it) for a Telegram-driven instance:

```toml
[telegram]
enabled = true
owner_user_id = 123456789   # your Telegram user id
bot_token = ""               # comes from the env overlay
```

and a `models.toml` with your provider/bundle layout (copy one from a
desktop install, keys blanked — see docs/CONFIGURATION.md).

## What's intentionally absent headless

- **InApp notification channel + InApp approval sink** — not constructed;
  with no Telegram bot configured the daemon boots but warns loudly:
  anything needing human confirmation fails closed (the approval ask
  errors; the task sits unactioned).
- **`place_call` telephony** — the tool refuses at call time (resource-dir
  + progress UI are Tauri-bound today).
- **Proactive hints** — they're GUI cards pointing at Settings.
- **Tray / updater / single-instance plugins.**

Everything else — delegation (`spawn_subagent`), wake-up authoring,
checkpointing, memory, MCPs, skills, identity, HTTP endpoints — works
identically to desktop; the registry assembly resolves state through the
`UiBridge` seam (`crates/athen-app/src/ui_bridge.rs`) instead of a Tauri
handle.

## Architecture notes (for maintainers)

- `headless::run_headless()` (`crates/athen-app/src/headless.rs`) mirrors
  the Tauri `setup()` hook step for step. If you add a background loop to
  `lib.rs`, add it there too or document the skip.
- `tauri::async_runtime::set(...)` is called with the daemon's tokio
  runtime, so shared code using `tauri::async_runtime::spawn` keeps
  working unchanged.
- `UiBridge::Headless` resolves `AppState` through a `OnceLock` published
  right after init; `emit()` drops frontend events at DEBUG.
- `athen-cli --prompt` remains the *stateless one-shot* headless path
  (benchmarks, CI); the daemon is the *stateful* one. They share nothing
  above `athen-core`/`athen-agent`, by design.
