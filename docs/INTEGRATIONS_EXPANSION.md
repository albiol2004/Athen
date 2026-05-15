# Integrations & Capabilities Expansion — Picking Menu

Synthesized 2026-05-15 from 4 parallel research streams covering (A) productivity/workspace
APIs, (B) cloud deployment platforms, (C) whitespace proactive integrations, and (D)
agent capability gaps vs Claude Code / Cursor / Devin / ChatGPT.

Builds on:
- [INTEGRATIONS_PUSH.md](INTEGRATIONS_PUSH.md) — original 5-move integrations menu (MCP-byo, IMAP/SMTP autodetect, OAuth wave, CalDAV/CardDAV, LLM-assisted credential UX)
- [CLOUD_APIS.md](CLOUD_APIS.md) — 15 `http_request` presets already shipped
- [EMAIL_SETUP.md](EMAIL_SETUP.md) — Move #2 (shipped)

Default verdict rule (from feedback memory `feedback_research_first_pick_later`): **free > paid**, **user-supplied keys > our keys**, **generic `http_request` preset > bespoke wrap** unless streaming / OAuth-refresh / file-upload / state-carrying protocol demands it.

---

## Top-10 to ship next (cross-stream consolidation)

Ranked by **(user leverage × Athen-already-halfway × low cost) − (competitor-strong)**.
Stream tag in parens: (A) workspace, (B) deploy, (C) whitespace, (D) capability gap.

| # | Pick | Stream | Wrap shape | Effort | Why it wins |
|---|---|---|---|---|---|
| 1 | **MS Graph — Outlook Mail + Calendar reads** | A | OAuth + `http_request` preset | 3–4 d | Zero CASA verification (unlike Google), large user base, unlocks 30 % of daily context. Replaces GitHub as Move #3 lead. |
| 2 | **Cloudflare Workers + Cloudflare DNS bundle** | B | `http_request` preset | 2–3 d | Only platform with true single-call deploy (script → live, 200 OK). 100 k req/day free. Pairs with DNS for full "ship to a domain from a prompt." |
| 3 | **Persistent arc continuation UI** | D | FE + executor hook | Medium | Arc replay already 90 % there ([project_tool_calls_dropped_on_rehydration]). No competitor does cross-session "continue from here." Highest differentiation score in stream D. |
| 4 | **Notion — database query + page create** | A | OAuth + `http_request` preset | 2–3 d | Zero verification, 180 req/min ceiling fine for single-user, covers note capture + task routing. |
| 5 | **Linear — GraphQL issues + create** | A | API-key + `http_request` preset | 2–3 d | Agent-native GraphQL, zero friction, dev-focused user base. |
| 6 | **Home Assistant — smart-home command** | C | Dedicated tool (LAN reach) | 1–2 wk | Uniquely possible: cloud agents can't reach 192.168.x.x. Zero competitor coverage. Risk-tier per entity (lock = HIGH, light = MED). |
| 7 | **CI/CD wake-ups (GitHub Actions / GitLab CI)** | D | `http_request` + `create_wakeup` | 1–2 wk | Closes "babysit the deploy" pain. Athen's wake-ups are uniquely cheap (sleeping arc, 0 cost while waiting). Claude Code Loop has 72 h cap + token burn. |
| 8 | **CalDAV / CardDAV unified sync** (Move #4) | A | Bespoke (stateful protocol) | 5–7 d | One wrap unlocks Apple iCloud + Fastmail + Nextcloud + Yandex + on-prem Exchange. The only viable Apple path (no OAuth, no REST). |
| 9 | **DigitalOcean App Platform + GCP Cloud Run** | B | MCP import (Move #1) | 0 d after Move #1 | Both ship official MCP servers GA 2026 — subsumed by MCP-byo, no bespoke code. Skip unless Move #1 slips. |
| 10 | **Spotlight / Windows-Search / browser-history local search** | C+D | Dedicated tool (native OS) | 1 wk per OS | Desktop-exclusive. Cloud agents can't read SQLite history files or query Spotlight. Powers "you've been reading X, pre-load that context." |

Three honorable mentions just below the cut: **GitHub OAuth wave** (originally Move #3 lead, demoted: dev users tolerate gh-cli + `http_request` token), **Approval batching** (UX polish, not differentiation), **Audit-log export** (compliance gap, ship later when an enterprise user asks).

---

## Stream A — Workspace / productivity APIs

Survey of 25 services across Google Workspace, Microsoft 365, Apple iCloud, Notion, Linear/Asana/Jira/Trello/ClickUp/Monday, Slack/Teams/Discord, Obsidian/Logseq/Notes/Keep.

**Wrap soon (Move #3 OAuth wave, reordered):**
1. **Microsoft Graph** (Mail + Calendar reads) — replaces GitHub as Move #3 lead. Zero verification, metered but free for personal accounts, scope explosion is the only pain.
2. **Notion** — long-lived bearer token, paginated reads, no audit cost.
3. **Linear** — GraphQL is unusually agent-friendly, 250-issue free tier covers single-user.
4. **GitHub** — keep but demote; users can already paste a PAT into the existing `http_request` registry.

**Wrap eventually:**
- **Slack + Teams** via Move #1 (MCP-byo) — official MCP servers exist; bespoke wrap is wasted code.
- **CalDAV / CardDAV** (Move #4) — one wrap, 5 + providers, only path to Apple Calendar / Contacts.
- **Google "Light"** (Calendar + Tasks read via `http_request`, no CASA audit) — nice-to-have post-Move #3.

**Skip / defer:**
- **Google Docs / Sheets write** — CASA audit gate ≥ $10 k / yr, 3–6 wk turnaround. Re-evaluate when paying users justify it.
- **Asana** — 60-day refresh-token expiry breaks silently; bespoke OAuth refresh not worth it vs simpler tools.
- **Jira / ClickUp / Monday / Trello** — medium effort, low single-user demand.
- **Apple Notes / Reminders, Google Keep** — no public API (Apple) or low demand (Keep). Obsidian / Logseq are filesystem-shell, not API.
- **Discord bespoke** — community MCP suffices on request.

---

## Stream B — Cloud deployment platforms

Survey of 19 platforms across edge/serverless, BaaS, container PaaS, big cloud, DNS.

**One-call deploy bar (cleared by only one platform):**
| Platform | Single-API-call deploy? |
|---|---|
| Cloudflare Workers | **Yes** (upload script → live, 200 OK) |
| Vercel | No (file upload → create → poll) |
| Netlify | No (zip upload + async polling) |
| DigitalOcean App Platform | No (but official MCP smooths it) |
| GCP Cloud Run | No (but official MCP smooths it) |
| AWS Lambda | No (IAM role + S3 zip + create-function + API Gateway) |
| Azure Functions | No (resource-group → app-plan → function) |
| Firebase Functions | No (async build) |

**Wrap soon:**
1. **Cloudflare Workers + DNS** — pick #2 overall. Paste a token, ship a Worker, attach a record, done.
2. **DigitalOcean App Platform + GCP Cloud Run** — via Move #1 MCP-byo (both ship official MCPs GA 2026). Skip bespoke wraps.

**Wrap eventually:**
- **Vercel** — strong market presence, REST API designed for code agents, but file-upload + create + poll loop earns a bespoke tool.
- **Supabase Management API** — distinct "spin up a DB + auth backend" use case; needs schema templating to be useful.
- **Fly.io** — Machines API for long-running services; needs `fly.toml` template library.

**Skip:**
- **AWS Lambda** — generous free tier but IAM role boilerplate kills agent ergonomics; revisit if paying users ask.
- **Heroku, Azure Functions, Firebase Cloud Functions, Deno Deploy v1, Fastly Compute** — pricing, complexity, or instability puts them below the bar.
- **Railway / Render** — Git-dependency makes "deploy a fresh thing from a prompt" awkward.

---

## Stream C — Whitespace (proactive / local-only)

Categories with high agent leverage and near-zero coverage in Claude Desktop / Cursor / Windsurf / ChatGPT-with-MCP / Zapier.

**Desktop-exclusive (cloud agents physically can't):**
- **Home Assistant** + Hue + Sonos + Plex + Jellyfin + ESPHome / Tasmota — local-network only. Tauri + Rust binds to LAN; cloud agents need port-forwarding or Tailscale Funnel.
- **Spotlight / Windows Search / browser-history** — SQLite files on disk, no API.
- **Apple Health (HealthKit)** — device-local export only, Apple forbids third-party cloud backends.
- **Clipboard / screenshot + OCR** — native OS APIs, no remote equivalent.

**Single-user privacy advantage:**
- **Plaid / Open Banking** — bank credentials in `athen-vault`, never touch our cloud. Read-only first (balance + transactions); transfers stay HumanConfirm.
- **Garmin / Withings / Oura / Whoop / Strava** — health REST APIs exist; nobody proactively monitors. Always-on advantage.

**Always-on advantage (vs reactive SaaS agents):**
- **Miniflux (self-hosted RSS) + Readwise** — proactive daily digest beats "check your reader."
- **Skyscanner / Eventbrite / Songkick** — price-watch and ticket-watch wake-ups.
- **Sentry / PostHog / Grafana** — alert-to-Linear-issue loops.

**Pick for Phase 2:** **Home Assistant** (#6 overall). Highest delight, lowest cost, zero competitor coverage, desktop-unique.

**Pick for Phase 3:** **Spotlight/browser-history search**, **Garmin Connect**, **Miniflux** — bundle as "proactive personal-context" wave.

**Skip:**
- **WhatsApp Business** — Meta business approval gate.
- **Signal** — no maintained client lib (`signal-cli` rotten).
- **Midjourney / Suno** — no public API.
- **YouTube Music** — unofficial-scraper-only.
- **Resy / OpenTable** — scraping risky.

---

## Stream D — Capability gaps vs competitors

Where competitor agents structurally fail. Athen's existing primitives close most of these — gaps are wiring, not new infrastructure.

| Gap | Competitor failure | Athen primitive that already exists | Missing piece |
|---|---|---|---|
| Cross-session continuity | Claude Code / Cursor / ChatGPT: zero memory. Devin: file-based but agent must explicitly recall. | Arc persistence + auto-recall memory + Identity store | UI button "continue from last related arc"; arc-recap on rehydrate |
| Long-running operations | Claude Code Loop 72 h cap, polling burns tokens. Zapier polls dumbly. | Wake-ups (sleeping arc, 0 cost while waiting) | CI/GitLab/Actions wake-up senses; webhook relay for NAT users |
| Proactive monitoring | All competitors reactive (Zapier polls but no language-native thresholds) | Sense streams + wake-ups + always-on coordinator | System-metrics sense; conditional wake-ups with hysteresis |
| Local resource access | Cloud agents can't reach local files; Claude Code is sandboxed per-dir | Shell + sandbox + native desktop | Clipboard / screenshot / OCR / browser-history tools |
| Audit trail | 78 % enterprises require, < 30 % products ship | Arc + tool-call rows in SQLite | Structured export (JSON/CSV) + redaction rules |
| Multi-action approval | One approval per action, 36 clicks for 12 emails + 12 tickets + 12 messages | Risk model + ApprovalRouter | Batch-by-action-type approval UI; rollback on partial-execute |
| Stale data | Cursor / Claude re-fetch on every query; context fills with old responses | Live sense streams | Freshness timestamps injected into context; conflict detection |
| Cost awareness | Claude Code post-hoc tokens only; Cursor opaque | Per-task tiering + arc-level usage tracking | Real-time spend meter UI; cheaper-model hint mid-task |
| Personal context | All competitors use generic tone | Identity store (shipped) | Multi-user profiles; auto-tone-adjust on mismatch |
| Credential refresh across workflows | Each agent re-asks for auth | Vault (shipped) | Auto-refresh loop before tool call (not after failure) |

**Top pick from stream:** **Persistent arc continuation UI** (#3 overall) — biggest visible UX win, mostly FE wiring on top of shipped backend.

**Honest competitor strengths (don't try to out-build):**
- Cursor's 8-agent parallel git-worktree execution — Athen is serial arc-based; adding lanes is expensive.
- Zapier's 7000-app breadth — Move #1 (MCP-byo) is the catch-up play, not a bespoke rewrite.
- Claude Code's code-quality generations on Opus — orthogonal to integrations; addressed by per-task tiering + provider failover.

---

## Recommended ship order

### Phase 1 — v0.3 (4–6 weeks)
1. **Cloudflare Workers + DNS bundle** (pick #2) — 2–3 d. Ships as two `http_request` presets + Cloud APIs row.
2. **Microsoft Graph OAuth wave** (pick #1) — 3–4 d. Mail-search + Calendar-list tools. Sets the OAuth-refresh pattern that #4 + #5 reuse.
3. **Notion + Linear OAuth wave** (picks #4, #5) — 2–3 d each, ride the Graph OAuth scaffold.
4. **Persistent arc continuation UI** (pick #3) — 1 wk. FE button + executor "recap on rehydrate" hook.

### Phase 2 — v0.4 (6–8 weeks)
5. **CI/CD wake-ups** (pick #7) — GitHub Actions + GitLab CI webhook ingest, wired to `create_wakeup`.
6. **Home Assistant smart-home tool** (pick #6) — REST API + per-entity risk tier + LAN discovery.
7. **CalDAV / CardDAV unified sync** (pick #8) — Move #4 from INTEGRATIONS_PUSH.
8. **Approval batching** — risk-UI redesign for multi-action approval.

### Phase 3 — v0.5+ (rolling)
9. **Move #1 MCP-byo** — unlocks DO App Platform / GCP Cloud Run / Slack / Teams without bespoke code.
10. **Proactive personal-context wave** — Spotlight/Windows-Search + Garmin + Miniflux (pick #10 + stream-C runner-ups).
11. **Audit log export, cost meter, multi-user profiles** — UX polish for power users + enterprise readiness.
12. **Move #5 LLM-assisted credential UX** — translates provider errors to plain English; helps every prior wave.

---

## Out of scope / explicit no

Captured here so future "should we wrap X?" questions have a fast answer:

| Service / capability | Reason |
|---|---|
| Google Docs / Sheets write | CASA audit ≥ $10 k / yr |
| Apple Notes, Apple Reminders | No public API, no CalDAV, no IMAP fallback |
| Asana | 60-day refresh-token expiry; bespoke OAuth not worth it |
| Jira / Trello / ClickUp / Monday (bespoke) | Low single-user demand; revisit if paying users ask |
| Discord (bespoke) | Community MCP via Move #1 covers it |
| Slack / Teams (bespoke) | Official MCPs exist; Move #1 imports them |
| AWS Lambda (bespoke) | IAM-role boilerplate kills agent ergonomics |
| Azure Functions, Firebase Functions | Free tier weak + multi-step orchestration |
| Heroku | No free tier |
| Railway, Render (full deploys) | Git-push dependency |
| Cursor / Windsurf reverse-engineered providers | ToS forbids, see [SUBSCRIPTION_RELAY_PROVIDERS.md](SUBSCRIPTION_RELAY_PROVIDERS.md) |
| WhatsApp Business | Meta business approval gate |
| Signal | `signal-cli` unmaintained |
| Midjourney, Suno, YouTube Music | No public API |
| Cursor's 8-lane parallel worktree | Architectural rewrite, not an integration |

---

## Cross-stream takeaways

1. **MCP-byo (Move #1) is more valuable than every individual wrap.** It subsumes Slack, Teams, DO App Platform, GCP Cloud Run, and Discord. Ship it before the second OAuth wave.
2. **Desktop-unique advantages cluster in Phase 2–3.** Home Assistant, browser-history, Spotlight, Garmin, Plaid — none of these are even possible in Claude Desktop / Cursor / ChatGPT. They're not the fastest wins, but they're the most defensible.
3. **The OAuth wave reorders.** MS Graph + Notion + Linear come before GitHub. GitHub already works via `http_request` PAT for the dev audience.
4. **Capability-gap wins (stream D) are mostly wiring, not new code.** Arc continuation, CI/CD wake-ups, approval batching, cost meter — every one builds on already-shipped primitives.
5. **Two free-tier ceilings to respect:** Notion 3 req/s and Trello 30 req/s. Everything else is generous for single-user agents.

---

## Sources

Stream A — Google Workspace MCP, Microsoft Graph MCP, Softeria/ms-365-mcp, Notion API rate limits, Slack MCP guide, Discord MCP guide, iCloud CalDAV/CardDAV docs, Linear API (GraphQL), Asana refresh-token expiry, Obsidian CLI early-access, gkeepapi.

Stream B — Cloudflare Workers API + 2026 Wrangler v4 sync deploys, Vercel REST API, Netlify "APIs for code agents," Deno Deploy v2 (v1 sunset 2026-07-20), Supabase Management API, Convex Deployment API, Fly.io Machines API, Render Deployments, DO App Platform MCP (GA 2026), GCP Cloud Run MCP (GA 2026), AWS Lambda CreateFunction, Azure Functions deploy, Cloudflare Zones API, Porkbun API.

Stream C — Home Assistant + go-hass-agent, Apple HealthKit, Spotify Web API 2026, Miniflux REST, Plaid API, Garmin Connect, Sentry + Grafana, Linear-for-agents, Skyscanner / Kiwi / Travelpayouts, WhatsApp Business, Replicate vs Fal, Tailscale API + Aperture 2026, macOS Spotlight DB structure, Tauri local-network capabilities.

Stream D — Claude Code 72 h Loop cap, Cursor 3 agent-first interface, Windsurf 2.0 + Devin / Agent Command Center, ChatGPT MCP (Plus/Pro write restrictions), Copilot agent mode, Aider CI/CD gap, AI Agent memory architecture (Cloudflare blog), Zapier Agents guide, AI reliability decade-old problem (Temporal), audit-log compliance gap surveys (78 % require / < 30 % ship).

(URLs preserved in agent-stream transcripts; not relisted here to keep this doc skimmable. If a source needs verification before shipping a pick, open the matching transcript or re-query.)
