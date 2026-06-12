# Headless Mode

Athen can run as a GUI-less daemon — the full autonomous stack (sense
monitors, coordinator, risk gates, dispatch loop, wake-up scheduler, CalDAV
sync) on a plain tokio runtime, with **Telegram as the user surface**:
owner messages drive the agent, notifications arrive as bot messages, and
approval prompts come with inline keyboards. This is the deployment shape
for servers, containers, and (later) the hosted/cloud offering.

A second user surface is the **HTTP API** (`ATHEN_HTTP_ADDR`): REST +
Server-Sent Events for remote clients — a React web dashboard or a React
Native companion app. See [HTTP API](#http-api-remote-clients) below.
The same listener also serves the **embedded web UI** (`web/`, a React
chat client compiled into the binary): point a browser at the instance,
paste the token, chat. See [Web UI](#web-ui-embedded) below.

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

## Environment variable reference

Everything an instance owns lives under one directory, so containers /
multi-tenant hosts isolate by pointing each instance at its own tree.
Two read models: **boot** vars are read once at process start; **overlay**
vars are re-applied after vault hydration on every config (re)read
(including per-arc router rebuilds) and win over the vault. Every overlay
var — plus `ATHEN_HTTP_TOKEN` — also accepts a `NAME_FILE` variant whose
value is a path to a mounted secret file (trailing newline trimmed, so
Docker secrets work out of the box); the direct var wins when both are
set, and empty values are ignored.

### Identity & paths (boot)

| Env var | Meaning | Default |
|---|---|---|
| `ATHEN_DATA_DIR` | Data tree root: `config.toml`, `models.toml`, vault, SQLite, workspace, skills, snapshots. Also honored by `athen-cli`. | `~/.athen` (Windows: `%APPDATA%\Athen`) |
| `ATHEN_WORKSPACE_DIR` | Where relative paths in file/shell tools resolve (benchmark harnesses point this at the task dir) | `<data_dir>/workspace` |
| `ATHEN_VAULT_BACKEND` | `auto` \| `file` \| `keyring`, case-insensitive (unknown values warn and mean `auto`) | `auto` |
| `ATHEN_HEADLESS` | `1`/`true` = same as `--headless`, checked before any GTK/WebKit touchpoint | unset |

Set `ATHEN_VAULT_BACKEND=file` in containers: there is no secret-service
daemon, and forcing the encrypted-file backend skips the D-Bus probe
entirely (in `auto` mode the fallback still engages, just noisier and, with
a present-but-locked keyring daemon, potentially slow).

### HTTP API (boot; desktop honors these too)

| Env var | Meaning | Default |
|---|---|---|
| `ATHEN_HTTP_ADDR` | Enable the HTTP API + embedded web UI on this socket (e.g. `0.0.0.0:8787`). Unset, empty, or unparseable (warns) = disabled. | unset (disabled) |
| `ATHEN_HTTP_TOKEN` | API bearer token (`_FILE` accepted). Precedence: env → persisted `<data_dir>/http_token` → freshly generated (mode 0600). | auto-generated |

### Credential seeding (env overlay; all accept `_FILE`)

| Variable | Target |
|---|---|
| `ATHEN_PROVIDER_<ID>_API_KEY` | LLM provider api_key — `<ID>` = provider id uppercased, non-alphanumerics → `_` (`opencode_go` → `OPENCODE_GO`, `my-relay.v2` → `MY_RELAY_V2`). Only patches providers that already exist in `models.toml`. |
| `ATHEN_EMBEDDING_API_KEY` | cloud embedding provider key |

### Sense credentials (env overlay; all accept `_FILE`)

| Variable | Target |
|---|---|
| `ATHEN_TELEGRAM_BOT_TOKEN` | Telegram bot token |
| `ATHEN_IMAP_PASSWORD` | email IMAP password |
| `ATHEN_SMTP_PASSWORD` | email SMTP password |

### Web search (env overlay; all accept `_FILE`)

| Variable | Target |
|---|---|
| `ATHEN_WEBSEARCH_BRAVE_API_KEY` | Brave web-search key |
| `ATHEN_WEBSEARCH_TAVILY_API_KEY` | Tavily web-search key |

### Tuning & debug

| Env var | Meaning | Default |
|---|---|---|
| `ATHEN_DISABLE_RISK_GATE` | Any value except empty/`0`/`false` skips the rule-engine risk gate in `shell_execute`/`shell_spawn` (checked per command). Sandboxed benchmark harnesses only — never set in production. | unset (gate on) |
| `ATHEN_LLM_VERBOSE_ROUTING` | `1`/`true` makes the LLM profile-routing classifier also return a reasoning field (costs tokens + latency) | off |
| `RUST_LOG` | standard tracing `EnvFilter` | `info` |

### Voice (desktop only)

| Env var | Meaning | Default |
|---|---|---|
| `ATHEN_PIPECAT_RUNNER` | Path override for the Pipecat voice runner (dev fallback when the Tauri resource dir is missing; used only if the path exists). Headless has no voice. | bundled resource |

### athen-cli (REPL / one-shot) — not read by the daemon

These configure `athen-cli --prompt` (the stateless benchmark/CI path)
only; `athen-app` ignores them. `ATHEN_DATA_DIR`/`ATHEN_VAULT_BACKEND`
above additionally apply to `athen-cli vault` and `--profile` resolution.

| Env var | Meaning | Default |
|---|---|---|
| `ATHEN_BASE_URL` | OpenAI-compatible endpoint (required) | — |
| `ATHEN_MODEL` | model slug (required) | — |
| `ATHEN_API_KEY` | bearer token, if the backend needs one | unset |
| `ATHEN_FAMILY` | model-family wire id for per-model quirks; `--family` wins; unknown ids exit 2 | `Default` |
| `ATHEN_TEMPERATURE` | sampling temperature, unclamped float; `--temperature` wins | executor default (0.7) |
| `ATHEN_TASK_TIMEOUT_SECS` | per-task overall timeout (seconds) | `1800` |

(`ATHEN_MAX_STEPS` appears in `athen-cli --help` but is not read by any
code today.)

> Build-time only, not operator-facing: `ATHEN_TARGET_TRIPLE` (stamped by
> `athen-shell`'s build script to locate the bundled `nu` binary).
> `ATHEN_CALDAV_URL/USER/PASSWORD` exist only in the `athen-caldav` smoke
> example — the app takes CalDAV credentials via Settings / the vault.

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
   they also survive per-arc router rebuilds. The full variable list lives
   in the [reference above](#environment-variable-reference); every one
   accepts the `_FILE` suffix form
   (`ATHEN_TELEGRAM_BOT_TOKEN_FILE=/run/secrets/bot_token`).

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

## HTTP API (remote clients)

Set `ATHEN_HTTP_ADDR` (the Docker image defaults it to `0.0.0.0:8787`;
publish the port to reach it) and the daemon serves a token-gated REST +
SSE API designed for React / React Native clients. The desktop app honors
the same env var, so a phone companion to a running desktop instance works
identically. Implementation: `crates/athen-app/src/http_api.rs` — handlers
call the same `*_core` functions as the Tauri commands, so semantics match
the WebView exactly.

**Auth.** Every endpoint except `GET /api/health` requires the token:
`Authorization: Bearer <token>`, `X-Athen-Token: <token>`, or `?token=`
(for `EventSource`, which can't set headers). Token precedence:
`ATHEN_HTTP_TOKEN` / `ATHEN_HTTP_TOKEN_FILE` env → `<data_dir>/http_token`
(auto-generated 0600 on first start; read it out with
`docker exec <c> cat /data/http_token`). The token gates access, it does
not encrypt: bind to localhost/VPN or front with a TLS reverse proxy for
anything internet-reachable. CORS is permissive by design — origin checks
add nothing when auth is a bearer token.

**Endpoints** (all JSON; errors are `{"error": "..."}` with 4xx):

| Method + path | Body | Returns |
|---|---|---|
| `GET /api/health` | — | `{status, name, version}` (no auth) |
| `GET /api/events` | — | SSE stream (see below) |
| `GET /api/arcs` | — | `ArcMeta[]` (sidebar list) |
| `POST /api/arcs` | — | `{arc_id}` (new arc, becomes active) |
| `GET /api/arcs/current` | — | `{arc_id}` |
| `GET /api/arcs/{id}/entries` | — | `ArcEntryResponse[]` (render history) |
| `POST /api/arcs/{id}/select` | — | `ArcEntryResponse[]` (switch + load) |
| `POST /api/messages` | `{message, arc_id?, images?, attachments?}` | `ChatResponse` — **long-poll**: resolves when the turn finishes or parks on `pending_approval` |
| `POST /api/messages/queue` | `{arc_id, text}` | steer a *running* task mid-flight |
| `POST /api/approvals/task` | `{task_id, approved}` | `ChatResponse` (risk-gate card answer) |
| `POST /api/approvals/question` | `{question_id, choice_key}` | `{resolved}` (`approval-question` answer) |
| `GET /api/grants/pending` | — | parked file-permission prompts (`grant-requested` payloads) |
| `POST /api/grants/{id}` | `{decision}` — `"Allow"` \| `"AllowAlways"` \| `"Deny"` \| `{"AllowProjectRoot": "/path"}` | `{resolved}` (unparks the agent) |
| `POST /api/cancel` | — | cancel all running agents |
| `GET /api/agents` | — | `ActiveAgent[]` (watch-agents panel) |
| `POST /api/agents/{task_id}/cancel` | — | `{cancelled}` |
| `GET /api/notifications` | — | `NotificationInfo[]` |
| `POST /api/notifications/{id}/read` · `/read-all` | — | `{ok}` |

…plus the **full command surface** (~110 more routes, added 2026-06-10
for web-UI parity): arcs rename/delete/compact/branch, goal + plan,
agent profiles + per-arc pickers, checkpoint snapshots + rewind,
calendar events + sources, memory/entities/relations, MCP catalog +
custom servers, directory grants, registered HTTP endpoints, identity,
skills, contacts, wake-ups, and every Settings save/test. Each handler
is a thin shim over the same `*_core` function the matching Tauri
command delegates to — see `full_surface_router()` in
`crates/athen-app/src/http_api.rs` for the route map and the
`#[derive(Deserialize)]` body structs above each handler for exact
request shapes. NOT exposed (admin/desktop-only): updater + runtime
installs, bundled-model download/delete, Voice/Pipecat setup.

**Events.** `GET /api/events` streams every UI event with the SSE `event:`
field set to the Tauri event name and `data:` carrying the exact payload
the WebView gets — `agent-stream` (token deltas: `{delta, is_final,
arc_id, is_thinking}`), `agent-progress` (tool cards), `approval-question`
/ `approval-resolved` / `approval-cancel`, `arc-updated`, `notification`,
`sense-event`, `wakeup-fired`, `agents-changed`, `grant-requested`. A
synthetic `lagged` event means the client fell behind the 1024-event
buffer — refetch state via REST. Browsers: native `EventSource`
(`/api/events?token=...`). React Native: use an SSE polyfill (e.g.
`react-native-sse`); it allows headers, so prefer `Authorization` over
the query param there.

**Typical chat client loop:** subscribe to `/api/events` → `POST
/api/messages` (don't await it for rendering; paint from `agent-stream` /
`agent-progress`) → if the response carries `pending_approval`, render a
card and answer via `/api/approvals/task` → on `approval-question` events
(mid-task risk prompts), answer via `/api/approvals/question` → on
`grant-requested` events (file-permission prompts; the agent is parked),
answer via `/api/grants/{id}`. If nothing can deliver a grant prompt
(HTTP API disabled *and* no Telegram), the FileGate fails closed with an
error to the agent instead of parking forever.

With Telegram *and* HTTP configured, approval prompts race both channels
(same as desktop + Telegram today): first answer wins.

## Web UI (embedded)

The reference chat client for the HTTP API ships **inside the binary**:
`web/` is a React + TypeScript app (Vite), built to `web/dist` and
embedded via rust-embed; `http_api.rs` serves it as the fallback for
every non-`/api/*` path on the same listener. A single-instance user
gets a remote browser UI with zero extra moving parts:

```
http://<host>:8787/  →  login screen  →  paste the http_token  →  chat
```

- **Auth:** the login screen asks for the API token (stored in
  `localStorage`; REST uses the `Authorization` header, `EventSource`
  uses `?token=`). The app shell itself is public by design — every
  byte of user data stays behind the token-gated `/api/*` routes.
- **Gateway mode (admin panel):** served through `athen-admin` at
  `/i/{instance}/`, the client detects the path prefix, probes
  `/api/arcs/current`, and skips the login screen entirely — the panel
  session cookie is the auth and the proxy injects the instance bearer.
  The Vite build uses `base: './'` (relative asset paths) so the same
  committed `dist` works at `/` and under the prefix. See
  ADMIN_PANEL.md § route map.
- **Scope (parity round, 2026-06-10):** the full desktop chat surface —
  arcs sidebar (switch / new / unread dots / rename / compact / delete
  via per-row menu), streaming chat with collapsible thinking blocks,
  expandable tool cards (args/result bodies, edit diffs, shell output)
  grouped into collapsible tool groups, delegation sub-arc inline
  expansion, goal banner + plan card, markdown rendering
  (react-markdown — React elements, no innerHTML), approval-question /
  risk-gate / file-grant cards, image+file attachments
  (paste/drag/picker), per-arc pickers (profile / reasoning effort /
  model tier / security mode), active-agents drawer, Changes rail with
  point-in-time revert, wake-ups drawer (list/create/toggle/delete),
  queue-while-busy composer, cancel, notifications bell — **plus a full
  Settings modal** (Models: connections/bundles/embeddings; Agents &
  Tools: profiles/skills/identity/MCP; Connections: owner contact/
  email/telegram/GitHub/calendar sources/web search/Cloud APIs;
  Security: mode/attachment policy/notification prefs/global grants;
  Contacts; Memory). Deliberately absent (server-admin / desktop-
  native): updater + runtime installs, bundled-model download/delete,
  Voice/Pipecat setup, onboarding wizard, theme toggle, timeline view.
- **Build workflow:** `cd web && npm run build`, then `cargo build`.
  `web/dist` is **committed**, so cargo (and the Dockerfile's
  `COPY . .`) never needs Node. Debug builds read `web/dist` from disk
  at runtime (edit → `npm run build` → refresh, no recompile); release
  builds embed the bytes. Hashed `assets/*` get immutable cache
  headers; `index.html` is `no-cache`.
- **UI development:** `npm run dev` (Vite on :5173) against any running
  instance — open the login screen's *Server* field and point it at
  e.g. `http://127.0.0.1:8787` (instance CORS is permissive; auth is
  the token, not the origin).
- **React Native path:** `web/src/api/` (typed client + SSE handler
  interface) is deliberately DOM-free — lift it into the RN app and
  swap `EventSource` for an SSE polyfill behind the same interface.
- **Wire-shape footnote:** the long-poll `POST /api/messages` response
  carries the final text in `content` (the `ChatResponse` shape), not
  `reply`. The SSE `agent-stream` final event can be a bare
  `{is_final: true}` with no delta — clients must not treat that as
  "streamed content was rendered".

## What's intentionally absent headless

- **InApp notification channel** — not constructed; the **InApp approval
  sink** *is* constructed when the HTTP API is enabled (SSE clients count
  as an in-app surface). With neither Telegram nor HTTP configured the
  daemon boots but warns loudly: anything needing human confirmation
  fails closed (the approval ask errors; the task sits unactioned).
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
