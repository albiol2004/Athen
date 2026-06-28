# Remote Access — UI-exposed HTTP API + Cloudflare quick-tunnel

> Status: **DESIGN + BUILD IN FLIGHT** (2026-06-26, `feat/remote-access`).
> Read this when working on: the Settings → Remote Access panel, the
> user+password auth on the HTTP API, the `cloudflared` quick-tunnel
> manager, or the start/stop lifecycle of the desktop HTTP listener.
> Code is authoritative once shipped.

## 1. Why

Athen already ships a complete, token-gated HTTP API + embedded React web
UI (`http_api.rs`, see [HEADLESS.md](HEADLESS.md)). On desktop it only
turns on via the `ATHEN_HTTP_ADDR` env var, resolved **once at boot** —
which violates Athen's "config via UI, never env/files for non-technical
users" rule. So the data plane exists; what's missing is:

1. A **Settings → Remote Access** panel that starts/stops the listener at
   runtime on a user-chosen port.
2. **Simple user+password auth** the user sets in that panel (the chosen
   posture — *not* a bearer token the user has to copy out of a file).
3. A **Cloudflare quick-tunnel** (`cloudflared tunnel --url …`) we
   detect/install on demand and whose `*.trycloudflare.com` URL we print
   in the panel — so the user gets a shareable browser link with zero
   account, zero port-forwarding.

The payoff: the tunnel URL opens straight into the full React chat
surface (the web UI rides the same listener via the SPA fallback), so
"use Athen for coding from my phone/another machine" is one toggle away.

## 2. Decisions (taken with the user)

- **Auth = user + password (HTTP Basic).** No bearer-token UX for the
  human. The existing token path STAYS for headless/env clients
  (backward compat) — the middleware accepts *either*.
- **Quick-tunnel only.** `cloudflared tunnel --url` needs no Cloudflare
  account; the URL is random per run and ephemeral. Named/stable tunnels
  (which need an account + DNS) are out of scope.
- **`cloudflared` is a managed binary.** Detect on `PATH` first; else
  install on demand into `<data_dir>/toolbox/bin/` — same
  detect-at-startup / install-on-demand / never-bundle / never-copy
  policy as the portable Python/Node runtimes
  (`athen-agent/src/runtimes.rs`).
- **Off by default.** Binds to `127.0.0.1` (cloudflared reaches
  localhost; the raw port never touches the LAN). The panel warns that
  enabling exposes a shell-capable agent.
- **Desktop is the control surface.** You configure Remote Access from
  the local desktop app. The web/HTTP surface can *read* status and
  toggle the tunnel for parity, but mutating its own lifeline is a
  documented footgun, not the primary path.

## 3. Auth model (`http_api.rs`)

`ApiState` gains optional Basic credentials beside the existing token:

```rust
struct ApiState {
    ui: UiBridge,
    token: Arc<String>,                 // unchanged — headless/env path
    basic: Option<Arc<BasicCreds>>,     // Some when user+password set
}
struct BasicCreds { username: String, password: String }
```

`require_token` becomes `require_auth`, accepting (first match wins):

| Form | Where | Credential |
|------|-------|------------|
| `Authorization: Bearer <tok>` / `X-Athen-Token` / `?token=` | header/query | the token (unchanged) |
| `Authorization: Basic <base64(user:pass)>` | header | configured user+password |
| `?auth=<base64(user:pass)>` | query | configured user+password (EventSource can't set headers) |

Password compare is constant-time (`ct_eq`); username compare too. When
`basic` is `None`, only the token forms are accepted (exactly today's
behaviour). `/api/health` and non-`/api/*` (the web shell) stay open.

`HttpApiConfig` gains `basic: Option<BasicCreds>`. A new constructor
`HttpApiConfig::from_settings(addr, token, basic)` builds it from the UI;
`from_env` is unchanged (`basic: None`).

**Graceful shutdown.** `serve()` today is `axum::serve(...).await`
(forever). Add `serve_with_shutdown(cfg, ui, shutdown: oneshot::Receiver<()>)`
using `.with_graceful_shutdown(async { let _ = shutdown.await; })`. The
old `serve` delegates with a never-firing receiver so headless is
unchanged.

## 4. The web client (`web/`)

- `client.ts` `ClientConfig` gains optional `username`/`password`. When
  both present → `Authorization: Basic …` header + `?auth=` on the SSE
  URL. Else → the existing `Bearer` + `?token=` path. One client, both
  modes.
- `Login.tsx` adds a **Username** field above the password. Empty
  username ⇒ token mode (the field is now labelled "Password or token").
  Backward compatible: a headless user pastes their token, leaves
  username blank, and it still works.

## 5. Tunnel manager (`athen-app/src/tunnel.rs`, new)

Standalone module, no `AppState` coupling:

- `cloudflared_path() -> Option<PathBuf>` — `which`-style PATH probe, then
  `<data_dir>/toolbox/bin/cloudflared[.exe]`.
- `ensure_cloudflared(progress) -> Result<PathBuf>` — return the detected
  path, else download the per-platform static binary from the pinned
  cloudflared GitHub release into `<data_dir>/toolbox/bin/`, `chmod +x`
  on unix. Linux/Windows ship a raw binary/`.exe`; macOS ships a `.tgz`
  (extract the single `cloudflared`). Reuses the reqwest download shape
  from `runtimes.rs`.
- `start_quick_tunnel(port) -> Result<TunnelHandle>` — spawn
  `cloudflared tunnel --url http://127.0.0.1:<port> --no-autoupdate
  --protocol http2`, merge stdout+stderr into a bounded channel (one
  reader task per pipe), and wait until cloudflared has BOTH printed
  `https://<sub>.trycloudflare.com` AND logged a `Registered tunnel
  connection` line before returning ready (timeout `TUNNEL_READY_TIMEOUT`
  ~30s; on timeout-with-URL it returns the URL anyway, else kill+error).
  Three deliberate 1033-avoidance choices: (a) wait for an edge
  connection, not just the URL — returning on the URL alone handed the
  user a hostname whose connections weren't up yet → immediate 1033;
  (b) a background **drain task** keeps reading both pipes for the
  child's whole lifetime, so cloudflared's logging never blocks on a full
  ~64 KiB pipe buffer (a full buffer stalls its connection manager →
  sustained 1033); (c) `--protocol http2` forces the outbound-TCP-443
  edge protocol because the default QUIC (outbound UDP 7844) is silently
  dropped on many home/ISP networks. `TunnelHandle { child, url, drain }`;
  drop/`stop()` aborts the drain and kills the child. Windows:
  `CREATE_NO_WINDOW`.
- `parse_tunnel_url(line) -> Option<String>` / `is_connection_registered(line)`
  — pure, unit-tested against real cloudflared stderr fixtures.

## 6. Runtime + persistence (`athen-core` + `state.rs`)

`RemoteAccessConfig` on `AthenConfig` (`#[serde(default)]`):

```rust
struct RemoteAccessConfig {
    enabled: bool,
    port: u16,           // default 8787
    username: String,
    tunnel_enabled: bool,
    // password NOT here — secrets live in the vault, never config.toml
}
```

Password → vault scope `remote_access` key `password` (mirrors the
existing credential-in-vault convention). `config.toml` carries only
non-secret fields.

`AppState` (mirrors the `email_shutdown` monitor pattern):

```rust
remote_access_shutdown: Mutex<Option<oneshot::Sender<()>>>,  // stops the listener
tunnel: Mutex<Option<tunnel::TunnelHandle>>,                  // kills cloudflared on stop
remote_access_status: Mutex<RemoteAccessStatus>,             // bound addr, tunnel url, errors
```

- `start_remote_access(port, basic, tunnel_enabled)` — bind
  `127.0.0.1:port`, spawn `serve_with_shutdown`, store the sender; if
  `tunnel_enabled`, `ensure_cloudflared` + `start_quick_tunnel` and stash
  the handle + URL. Stamp status.
- `stop_remote_access()` — fire the shutdown sender, kill the tunnel,
  clear status.

**Boot.** In `lib.rs` setup, after `app.manage(state)`: if
`config.remote_access.enabled`, call `start_remote_access(...)`. The
`from_env` path still works for headless and for power users who set
`ATHEN_HTTP_ADDR` (the two are independent; env wins its own listener).

## 7. Commands + routes (house style: Tauri cmd + HTTP route share `*_core`)

- `get_remote_access` → `{enabled, port, username, tunnel_enabled,
  has_password}` (never returns the password).
- `set_remote_access(enabled, port, username, password?, tunnel_enabled)`
  — persist config, store password in vault if provided, then
  stop+restart the listener to apply live (no app restart). Returns fresh
  status.
- `remote_access_status` → `{listening, local_url, tunnel_url,
  cloudflared_installed, last_error}`.

HTTP parity in `full_surface_router()`: `GET /api/remote-access`,
`POST /api/remote-access`, `GET /api/remote-access/status`. (Mutating
remote access from the remote surface is allowed but warned.)

## 8. UI (`/frontend` desktop + `/web` parity)

New **Remote Access** settings tab, both surfaces:

- Enable toggle + port input + **Username** + **Password** fields.
- "Create public link (Cloudflare)" toggle → triggers install-on-demand
  (progress line) then shows the live `*.trycloudflare.com` URL with a
  Copy button and an "Open" link.
- Status block: listening addr, tunnel URL, `cloudflared` install state,
  last error.
- A clear warning that enabling exposes a shell-capable agent; keep it
  off by default.

Desktop uses the existing absolute-text-input convention (no native
pickers). All tunnel/git output escaped. Web panel mirrors
`PanelAgents`/`PanelProjects` shape; `web/dist` rebuilt and committed.

## 9. Build phasing (file-disjoint waves, orchestrator owns git+builds)

- **A** — `athen-core/config.rs` (RemoteAccessConfig) + `http_api.rs`
  (Basic auth + `serve_with_shutdown`). Self-contained; unit-tested.
- **B** — `athen-app/src/tunnel.rs` (new). Self-contained; pure-parse
  unit-tested. Parallel with A.
- **C** — `state.rs` start/stop + `commands.rs` + `http_api` route
  registration + `lib.rs` (register cmds + boot). Depends on A+B.
- **D** — desktop frontend panel + web `PanelRemoteAccess.tsx` +
  `Login.tsx`/`client.ts` Basic auth + `web/dist` rebuild. Depends on C
  (commands) + A (auth).
- **E** — this doc → SHIPPED, `docs/IMPLEMENTATION.md`, `CLAUDE.md`
  index, memory.

CI gate every wave: `cargo clippy --workspace --all-targets -- -D
warnings` && `cargo test --workspace` && `cd web && npx tsc --noEmit`.

## 10. Security notes

- **Login throttle (SHIPPED).** `AuthThrottle` in `http_api.rs` is a
  process-local GLOBAL failed-auth counter with exponential backoff
  (threshold 8, lockout 15s doubling to a 15-min cap, reset on first
  success). It's *global* by design: behind a quick-tunnel every request
  comes from localhost, so a per-IP limiter is useless. The lockout is
  checked BEFORE the credential compare, so during a cooldown even a
  correct token/password gets `429 Too Many Requests` (with `Retry-After`).
  The 244-bit token is unbruteforceable on its own; this caps the guess
  rate against a weaker user-chosen Basic password. `/api/health` and the
  static web shell are never throttled.
- Bind `127.0.0.1` only; the tunnel is the sole public path.
- Password at rest in the vault, never `config.toml`; never logged
  (log lengths only).
- Off by default; explicit warning in the panel.
