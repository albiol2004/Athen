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
| `ATHEN_ADMIN_AUDIT_RETENTION_DAYS` | prune audit rows older than this, daily (`0` = keep forever) | `90` |
| `DOCKER_HOST` | honored by bollard | unix socket |

First start with no users creates `admin` (password from env or printed
once).

### The Docker socket (and how to de-privilege it)

The panel needs a Docker-compatible API socket, and on a rootful daemon
that socket is **root-equivalent on the host** (anyone holding it can
bind-mount `/` into a privileged container). Mitigations, in order of
preference:

1. **Rootless Docker or Podman** — point `DOCKER_HOST` at a rootless
   socket and the panel's blast radius shrinks from "host root" to "that
   unprivileged account":
   ```bash
   # Podman (docker-compatible API, verified with the panel):
   systemctl --user enable --now podman.socket
   DOCKER_HOST=unix:///run/user/$(id -u)/podman/podman.sock athen-admin
   # Rootless Docker: DOCKER_HOST=unix:///run/user/$(id -u)/docker.sock
   ```
   Caveats: rootless networking is slirp4netns/pasta (slower NAT), and
   memory/CPU quotas need cgroups v2 delegation (default on Fedora,
   `systemd.unified_cgroup_hierarchy` distros).
2. **Dedicated user in the `docker` group** (rootful daemon) — still
   root-equivalent, but at least the panel isn't running as root itself
   and nothing else on the box shares its uid.

Either way: keep the panel the only internet-facing thing holding the
socket, and front it with TLS.

## Surfaces

- **`GET /`** — embedded panel UI (plain HTML/CSS/JS, warm-dark glass).
  Admin: instance cards (state badge, quota + disk-usage line,
  start/stop/logs/access/delete), provision modal (env vars + memory/CPU
  quotas + soft disk limit + optional `config.toml`/`models.toml` seeds +
  user grants), users table (role chip + promote/demote + delete), audit
  log tab. Non-admin users: their granted instances with an *Open chat*
  button. Everyone: a bell modal to set their push webhook.
- **`GET /i/{instance}/chat`** — minimal built-in chat client running the
  exact contract a React/RN app will use: history via proxied
  `/arcs/.../entries`, live `EventSource` on `/events` (stream deltas,
  tool chips, approval cards, file-permission grant cards), long-poll
  `POST /messages`, `pending_approval` risk card with Approve/Deny.
- **`/i/{instance}/api/{*}`** — the session-gated reverse proxy (below).
- **Panel REST** (session; admin-only where noted): `POST /panel/login`,
  `/panel/logout`, `GET /panel/me`, `POST /panel/password`,
  `POST /panel/notify` (own push webhook), `GET /panel/audit` (admin),
  `GET|POST /panel/instances` (admin for POST),
  `POST /panel/instances/{id}/start|stop|delete|grants` (admin),
  `GET /panel/instances/{id}/logs?tail&follow` (admin, SSE),
  `GET|POST /panel/users`, `POST /panel/users/{id}/delete` (admin),
  `POST /panel/users/{id}/role` (admin; promote/demote).
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
Two seed footguns found live: (1) sanitize BOTH key shapes — inline
`api_key = "…"` *and* the table form `[providers.X.auth]\nApiKey = "…"`;
(2) inject the key for the provider the **active bundle** routes to, not
the one you assume — `ATHEN_PROVIDER_<ID>_API_KEY` for the wrong `<ID>`
yields "Authentication failed" at the first real LLM call while
regex-only risk triage still appears to work.

## Security model & hardening notes

- Sessions: opaque 64-hex ids in SQLite, 30-day expiry, `HttpOnly;
  SameSite=Strict` (also the CSRF story). `Secure` is not set — TLS is
  the fronting proxy's job and localhost/VPN deployments are first-class.
- Login burns an argon2 verify on unknown usernames (no timing oracle);
  generated bootstrap password goes to stdout once, never the log file.
- **Brute-force throttle**: per-username consecutive-failure lockout
  (5 free tries, then 30s doubling per failure, capped at 1h; cleared on
  success) plus a global 30 attempts/min cap. 429 + `Retry-After`,
  rejected before any argon2 work. Keyed by username, not client IP —
  the panel sits behind a proxy and unconfigured `X-Forwarded-For` trust
  is worse than no IPs.
- **Per-user request buckets** on every session-gated route (burst 300,
  refill 5/s → 429). Generous by design: it stops runaway scripts, not
  normal chat (SSE/long-poll connections cost one token each).
- **Audit log**: append-only `audit_log` table recording login (+failed/
  throttled), logout, password/notify changes, instance create/start/
  stop/delete, grants, user create/delete, role changes, disk-quota
  crossings. `GET /panel/audit` + Audit tab (admin). Retention: a daily
  prune deletes rows older than `ATHEN_ADMIN_AUDIT_RETENTION_DAYS`
  (default 90; `0` keeps forever); each prune that removes rows leaves
  its own `audit_prune` entry so the trail records its truncation.
- **Multi-admin**: `POST /panel/users/{id}/role` promotes/demotes
  (audited as `user_role` with the old → new transition). Invariants:
  the **last admin can never be demoted**, and no-op changes are
  rejected. Self-demotion is allowed when another admin exists — roles
  are re-read from the DB per request, so it bites on the very next
  call. Self-deletion stays refused, which also keeps admin count ≥ 1
  through deletes. All admins remain fully trusted peers (no scoped/
  read-only admin tier).
- **Tenant isolation on the bridge**: the network is created with
  `com.docker.network.bridge.enable_icc=false`, so instances cannot
  reach each other's :8787 (verified live: container→container blocked
  both directions; host→container and internet egress unaffected).
  Network options are immutable — a pre-ICC `athen-net` keeps working
  but logs a warning; recreate it once (`docker network rm athen-net`
  with no instances running) to get isolation.
- **Resource quotas**: optional per-instance memory (hard cgroup limit,
  swap disabled — a runaway instance is OOM-killed and auto-restarted by
  the `unless-stopped` policy, and the proxy's per-request IP resolve
  reconnects transparently) and CPU (`nano_cpus`). Give a real instance
  **≥ 2 GB**: the daemon + bundled embedder + an agent turn peak past
  1 GB (the 1024 MB e2e instance was memcg-OOM-killed mid-turn).
- **Disk quotas (soft)**: optional per-instance `disk_limit_mb`. A
  5-minute `docker system df` sweep (volumes only) measures each data
  volume; the dashboard shows `disk used / limit` (red when over), and
  crossing the limit writes a `disk_quota_exceeded` audit row + pushes a
  webhook warning to granted users — once per crossing, re-armed after
  usage falls back under 90% of the limit. Soft by necessity: Docker's
  hard quota (`storage_opt: size=`) only works on xfs-with-pquota
  backing filesystems and fails container creation everywhere else.
  Nothing is blocked at the limit — the operator decides.
- Operator holds user secrets — inherent to hosting an agent that acts on
  the user's behalf (vault encrypts at rest; host root can read keys).
  Be explicit about this with hosted users.
- Remaining gaps, deliberate: admins are fully trusted peers (no scoped
  admin tier), disk quotas warn but don't block, and the Docker socket
  stays root-equivalent on rootful daemons (see *The Docker socket*
  above for the rootless mitigation).

## Push notifications (panel-side)

Instances can't reach a backgrounded phone; the panel can. A supervisor
(`notify.rs`) keeps one SSE connection per *running* instance
(`/api/events`, instance bearer, container IP re-resolved per connect;
re-sweeps every 10s so stop/start/restart self-heals — verified across a
live OOM restart). Forwarded to every **granted** user with a webhook
configured (admins are not implicitly included — grant them explicitly):

- every `approval-question` (agent blocked on a human),
- every `grant-requested` (file-permission prompt — agent parked until
  answered in the chat page or via `POST /api/grants/{id}`), and
- `notification` events with `requires_response` or urgency
  High/Critical. Routine notifications stay in-app.

Delivery is a plain-text POST to `users.notify_url` with `Title` /
`Priority` / `X-Athen-Instance` headers — an [ntfy](https://ntfy.sh)
topic works out of the box (install the app, pick an unguessable topic,
paste `https://ntfy.sh/<topic>` into the panel's bell modal), and any
endpoint accepting plain POSTs (Gotify shim, home-automation hook) works
too. FCM/APNs for a future React Native app slots in behind the same
`deliver()` seam. Recently forwarded event ids are deduped per watcher.

Note: the *coordinator* risk gate (`pending_approval`) rides the
long-poll `POST /messages` response, not the event bus — clients holding
that request already have the card. The push path covers the events a
disconnected user would otherwise miss.

## E2E verification (2026-06-10)

Live run against real containers (`athen:headless-test` image, panel on
host): admin login → user create → instance provision (volume + container
+ token + config seeds + start, ~0.4s) → role gates (user hits admin
endpoints → 403, proxy without session → 401) → granted user opens chat,
message through panel→proxy→instance→LLM→tool→reply with SSE deltas
captured → risk-gate card denied through the proxy → logs SSE → stop →
delete with volume. Findings fixed during the run: seed-file uid (above)
and the image needing a rebuild to include the HTTP API.

Hardening round, same day, two live instances: login throttle (5 wrong
passwords → 429 + `Retry-After`, correct password also throttled during
lockout, success clears state); request bucket (400 parallel `/panel/me`
→ ~300×200 then 306×429); audit rows for every step, admin-only read;
quotas exact in `docker inspect` AND enforced for real — the 1024 MB
instance was memcg-OOM-killed mid-turn, `unless-stopped` restarted it,
the watcher + proxy reconnected without intervention; ICC: container→
container :8787 blocked both directions while host→container and
internet egress kept working; push: agent `install_package` →
`approval-question` SSE → panel watcher → ntfy-style POST on the test
webhook (Title/Priority/X-Athen-Instance + body), answered with "deny"
via the proxied `/approvals/question`.

Bug found in the *instance* during the run (headless gap, not panel):
file-path grant prompts (`grant-requested`) bypassed the ApprovalRouter
and were emitted only through the FileGate's `app_handle` — which
headless doesn't wire — so an out-of-workspace `write` parked the agent
silently with no event and no HTTP endpoint to answer. **Fixed
2026-06-10** (follow-up round, below).

## Follow-up round (2026-06-10): the five remaining gaps

Shipped in one pass: the headless grant fix, multi-admin, audit
retention, soft disk quotas, and the rootless-socket mitigation (each
described in its section above). Implementation notes:

- **Headless grant fix** (instance-side, in `athen-app`): the `FileGate`
  now holds a `UiBridge` instead of a raw `Option<tauri::AppHandle>`, so
  `grant-requested` / `grant-resolved-elsewhere` flow through the same
  `emit` chokepoint every other UI event uses — WebView on desktop, HTTP
  event bus (SSE) in both modes. Two new instance API routes:
  `GET /api/grants/pending` (parked prompts, same payload as the event)
  and `POST /api/grants/{id}` with
  `{"decision": "Allow" | "AllowAlways" | "Deny" | {"AllowProjectRoot": "/path"}}`.
  The embedded chat client renders a grant card with those choices, and
  the panel watcher forwards `grant-requested` to webhooks. Fail-closed
  backstop: when *no* surface could deliver the prompt (headless with
  the HTTP API disabled and no Telegram sink), the gate now returns a
  clear error to the agent instead of parking forever.
- **bollard `df` footgun**: the `type=volume` filter
  (`DataUsageOptionsBuilder::_type`) fails on every daemon with "Unable
  to URLEncode" (serde_urlencoded can't serialize sequence params) —
  the disk sweep calls `df(None)` and reads only the volumes section.

E2E (same day, live): role matrix — demoting the only admin → 400; with
two admins, self-demotion works and bites on the very next request
(403); the second admin re-promotes; no-op role change → 400; non-admin
role change → 403. Retention — a 2025-dated row pruned at startup under
a 30-day policy while a 9-day-old row survived; `audit_prune` row
written. Grants — out-of-workspace `write` on a live instance produced
`grant-requested` on the proxied SSE stream *and* an ntfy-style webhook
push ("Athen needs file access"), `GET /api/grants/pending` listed it,
`POST /api/grants/{id} {"decision":"Allow"}` unparked the agent, the
file appeared in the container and the long-poll returned a normal
reply. Disk — an 80 MB `dd` into `/data` pushed usage to 545 MB against
a 64 MB soft limit: warn log + `disk_quota_exceeded` audit row + webhook
push + red `disk 545 / 64 MB` on the dashboard card. Rootless — the
panel ran against `podman.socket` via `DOCKER_HOST` (health, login,
status sweep, df sweep all clean).
