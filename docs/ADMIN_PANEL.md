# Admin Panel & Gateway (`athen-admin`)

Control plane for hosting **multiple Athen instances** — for one person
running their own server, or a provider hosting Athen for many users.
SHIPPED 2026-06-10 (branch `feat/admin-panel`). Builds directly on the
headless daemon + HTTP API (see HEADLESS.md): one Docker container per
user, the panel provisions/supervises them and gateways client traffic in.

```
React / RN / browser            ┌────────────────────── host ──────────────────────┐
        │  session cookie       │  athen-admin (axum)        Docker bridge network │
        ▼                       │  ┌──────────────┐          (no published ports)  │
   TLS terminator ───────────▶  │  │ panel UI     │   bearer  ┌─────────────┐      │
   (Caddy / cloudflared /       │  │ users+grants │──token───▶│ athen-alice │      │
    nginx — ONE endpoint)       │  │ Docker ctl   │           ├─────────────┤      │
                                │  │ proxy /i/…   │──────────▶│ athen-bob   │      │
                                │  └──────────────┘           └─────────────┘      │
                                └───────────────────────────────────────────────────┘
```

## Design decisions (why it looks like this)

- **Gateway, not token hand-off.** Human users authenticate to the panel
  (argon2 + opaque server-side session cookie); the panel swaps the session
  for the per-instance bearer token server-side. Instance tokens never
  reach a client, revocation is a session-row delete, and browser
  `EventSource` works because cookies ride along where headers can't.
  The instance HTTP API needed **zero changes** for multi-tenancy.
- **Instances are never exposed.** All instance containers join one shared
  bridge network with no published ports — inbound only via the panel.
  The network is deliberately NOT `internal: true`: instances need
  outbound internet (LLM APIs, IMAP/SMTP, Telegram). Panel→instance
  targets are resolved per request as container IP via the Docker API
  (IPs change across restarts; Docker DNS doesn't resolve from the host).
- **TLS exists at exactly one point** — whatever fronts the panel. VPS:
  Caddy (auto Let's Encrypt). Home/NAT: one cloudflared tunnel. LAN/VPN:
  plain HTTP or Tailscale. Instance count never changes this.
- **Athen stays single-user.** Identity lives at the edge (panel); never
  put two users on one instance — arcs/memory/identity/contacts all
  assume one owner. Isolation = one container + one volume per user.
- **Panel generates each instance's `ATHEN_HTTP_TOKEN`** and injects it at
  create time (no exec-into-container to read tokens back). Tokens are
  stored in the panel DB and serialization-skipped everywhere
  (`#[serde(skip_serializing)]`).

## Running the panel

```bash
cargo build --release -p athen-admin
ATHEN_ADMIN_ADDR=127.0.0.1:8800 ./target/release/athen-admin
```

| Env var | Meaning | Default |
|---|---|---|
| `ATHEN_ADMIN_ADDR` | listen address | `127.0.0.1:8800` |
| `ATHEN_ADMIN_DATA_DIR` | panel DB (`panel.db`) | `~/.athen-admin` |
| `ATHEN_ADMIN_PASSWORD` | bootstrap admin password | generated, printed once to stdout |
| `ATHEN_ADMIN_IMAGE` | image for new instances | `athen` |
| `ATHEN_ADMIN_NETWORK` | shared bridge network | `athen-net` |
| `DOCKER_HOST` | honored by bollard | unix socket |

First start with no users creates `admin` (password from env or printed
once). The panel needs the Docker socket — run it as a user in the
`docker` group or point `DOCKER_HOST` at a rootless Podman socket.
Treat socket access as root-equivalent on the host: keep the panel the
only internet-facing thing with it.

## Surfaces

- **`GET /`** — embedded panel UI (plain HTML/CSS/JS, warm-dark glass).
  Admin: instance cards (state badge, start/stop/logs/access/delete),
  provision modal (env vars + optional `config.toml`/`models.toml` seeds +
  user grants), users table. Non-admin users: their granted instances
  with an *Open chat* button.
- **`GET /i/{instance}/chat`** — minimal built-in chat client running the
  exact contract a React/RN app will use: history via proxied
  `/arcs/.../entries`, live `EventSource` on `/events` (stream deltas,
  tool chips, approval cards), long-poll `POST /messages`,
  `pending_approval` risk card with Approve/Deny.
- **`/i/{instance}/api/{*}`** — the session-gated reverse proxy (below).
- **Panel REST** (session; admin-only where noted): `POST /panel/login`,
  `/panel/logout`, `GET /panel/me`, `POST /panel/password`,
  `GET|POST /panel/instances` (admin for POST),
  `POST /panel/instances/{id}/start|stop|delete|grants` (admin),
  `GET /panel/instances/{id}/logs?tail&follow` (admin, SSE),
  `GET|POST /panel/users`, `POST /panel/users/{id}/delete` (admin).
  `GET /healthz` is the only unauthenticated route.

## The proxy contract (what a React/RN client sees)

Base URL: `https://panel.example.com/i/<instance-id>/api` — then the
endpoint table in HEADLESS.md § HTTP API applies verbatim. Credential:
the panel session cookie (web) or the `athen_admin_session` cookie value
sent as a Cookie header (React Native). Inbound `Cookie`/`Authorization`
headers are stripped at the proxy; the instance bearer token is injected
server-side. Bodies stream both ways unbuffered — SSE `/events` and the
long-poll `POST /messages` behave identically to direct instance access.
403 = no grant; 404 = unknown instance id; 502 = instance not running.

## Provisioning details

`POST /panel/instances` → volume `athen-<short>-data` + container
`athen-<short>` (labels `athen.panel.instance=<id>`), restart policy
`unless-stopped`, `/data` bind, env = `ATHEN_HTTP_ADDR` +
panel-generated `ATHEN_HTTP_TOKEN` + operator-provided extras
(`ATHEN_PROVIDER_*_API_KEY`, `ATHEN_TELEGRAM_BOT_TOKEN`, … — the
env-overlay table in HEADLESS.md). Optional `config_toml`/`models_toml`
strings are tar-uploaded into `/data` (uid/gid 1000 to match the image's
`athen` user — root-owned 0600 files are unreadable to the daemon and
fail silently into config defaults) before first start. Provisioning
failures roll back container, volume, and DB row. Instance delete keeps
the data volume unless `delete_data: true`.

Note: the env-overlay only patches **providers that exist in
`models.toml`** — seeding a `models.toml` (keys blanked, `auth = "None"`)
is the normal way to give a new instance its provider/bundle layout.

## Security model & hardening notes

- Sessions: opaque 64-hex ids in SQLite, 30-day expiry, `HttpOnly;
  SameSite=Strict` (also the CSRF story). `Secure` is not set — TLS is
  the fronting proxy's job and localhost/VPN deployments are first-class.
- Login burns an argon2 verify on unknown usernames (no timing oracle);
  generated bootstrap password goes to stdout once, never the log file.
- Operator holds user secrets — inherent to hosting an agent that acts on
  the user's behalf (vault encrypts at rest; host root can read keys).
  Be explicit about this with hosted users.
- Instances on the shared bridge can reach *each other's* :8787 (each is
  token-gated). Acceptable v1; per-instance networks would be stricter.
- v1 gaps, deliberate: no per-user rate limits, no audit log, no
  instance resource quotas (`HostConfig` memory/cpu fields are the hook),
  no panel-side push notifications (subscribe to instance
  `approval-question` events server-side + FCM/APNs is the natural
  follow-up), single-admin trust model.

## E2E verification (2026-06-10)

Live run against real containers (`athen:headless-test` image, panel on
host): admin login → user create → instance provision (volume + container
+ token + config seeds + start, ~0.4s) → role gates (user hits admin
endpoints → 403, proxy without session → 401) → granted user opens chat,
message through panel→proxy→instance→LLM→tool→reply with SSE deltas
captured → risk-gate card denied through the proxy → logs SSE → stop →
delete with volume. Findings fixed during the run: seed-file uid (above)
and the image needing a rebuild to include the HTTP API.
