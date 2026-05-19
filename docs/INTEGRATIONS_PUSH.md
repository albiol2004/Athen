# Integrations & Tools Push

Strategic picking menu for the "become the gods of integrations" pillar. Synthesized 2026-05-12 from four parallel Haiku research streams: MCP ecosystem audit, personal-OAuth landscape, credential-setup UX, and bespoke-vs-`http_request` criteria.

**Status:** Move #2 (IMAP/SMTP autodetect wizard) shipped 2026-05-12 — see [EMAIL_SETUP.md](EMAIL_SETUP.md). Move #4 (CalDAV sync) shipped 2026-05-15 — `crates/athen-caldav/` RFC 4791 adapter + sync loop in `crates/athen-app/src/calendar_sources.rs` + agent calendar_create/update now push to remote (2026-05-17 commits). CardDAV deferred. Moves #1, #3 still design-only; Move #5's LLM error translator landed 2026-05-12.

This is a **picking menu**, not a build plan. Per-feature implementation docs land when each item is picked up. Related: [TOOL_EXPANSION.md](TOOL_EXPANSION.md) (the 10-category CLI/API menu, complementary), [CLOUD_APIS.md](CLOUD_APIS.md) (the `http_request` substrate this builds on).

## Operating constraints

- **No enterprise data-source licenses.** No paying $thousands/month to be a "Gmail provider" the way ChatGPT Apps and Google Workspace AI do. Every user grants OAuth to their own account, the dev app stays under free-tier ceilings.
- **Ugly-but-easy beats slick-and-blocked.** LLM-assisted credential setup is fine. CLI commands during onboarding are NOT fine.
- **`http_request` is the substrate.** The 15-preset Cloud APIs panel already covers stateless JSON-over-HTTP. Bespoke wraps must earn their footprint with streaming, OAuth refresh, pagination loops, file streams, or state-carrying protocols.
- **Free tier ≫ paid.** Microsoft Graph (zero approval, metered fractions of a cent) beats Google Gmail edit (CASA audit ~$10k/year). Pick the easy ramps first.

## The five-move push (ship order)

### 1. Custom MCP servers (BYO) — **highest leverage** (status: design)


Single biggest move. Every Claude Desktop / Cursor / Zed user has stdio MCPs configured for Slack, Notion, Linear, GitHub, filesystem, Postgres, etc. Athen reads the same config schema and gets the whole ecosystem for free.

**What's already there:** stub `athen-mcp` crate. No UI to add custom MCPs.

**Key findings from research:**
- Claude Desktop's `claude_desktop_config.json` and Cursor's `~/.cursor/mcp.json` are **schema-identical** — `mcpServers: { name: { command, args, env } }`. Athen can import either verbatim.
- **100% of popular community servers use stdio.** Streamable HTTP is the deprecation-replacement-for-SSE story, no major server has migrated yet.
- ~95% are `npx -y @scope/server` or `uvx server-name`. Athen's portable Node/Python wizard already covers ~90% of bootstrap (gaps: Playwright browsers, `kubectl` binary — handle per-server).
- Tool discovery is one JSON-RPC `tools/list` round-trip after `initialize`. Schemas are JSON Schema subset.

**Build shape:**
- Settings → MCP Servers panel: import from `claude_desktop_config.json`, paste-config form, or pick-from-registry (cached gallery of top ~30 servers).
- Spawn = stdio subprocess + `initialize` ping with timeout. Tools register into `AppToolRegistry` with name prefix `<server>__<tool>` (the convention `tool_grouping.rs:18` already handles).
- **Risk gating:** treat all third-party MCP tools as `WritePersist` by default (the spec has no scope model — Athen must add). User can per-server or per-tool downgrade after first call.
- **Credential pass-through via vault:** never let MCP env vars hold raw API keys. Resolve from `athen-vault` at spawn time. Doppler-style: 48% of servers leak via `process.env`; vault solves it.
- **Subprocess hygiene (critical):** track child PIDs in `AppState`, `SIGTERM → 180s wait → SIGKILL` on shutdown. The "Claude Code zombie ate 14 GB of RAM" precedent is real. Reuse the PID reconciliation from #205.

**Effort:** 1–2 weeks for "import + spawn + tools/list + vault env". Subagent risk gating + per-server allowlist is an additional ~3 days.

**Why first:** unlocks ~50 ecosystem integrations in one shot. Anything Athen would otherwise wrap bespoke (Slack search, Notion query, Linear issues, GitHub PRs) is already an off-the-shelf MCP.

---

### 2. IMAP/SMTP autodetect wizard — **SHIPPED (post 2026-05-12)**

Settings → Email panel landed. `email_detect` runs the hardcoded provider table + Thunderbird autoconfig chain, `email_test_connection` validates IMAP+SMTP credentials, `email_translate_error` pipes raw failures through the LLM error translator. Backend lives at `crates/athen-app/src/email_autodetect.rs` + `email_errors.rs`; commands at `crates/athen-app/src/commands.rs` (~line 7307+); frontend wired in the Settings → Email panel. The remaining email work (e.g. `email_search` IMAP IDLE tool, OAuth-tier upgrade) tracks under separate items, not under this move.

Email is the bedrock proactive sense. Today's `email_send` is SMTP-only; inbound + search needs IMAP. **Avoid the Google OAuth verification trap by shipping app-password flow first.**

**Key findings:**
- Thunderbird's three-tier ISPDB (provider autoconfig XML → centralized database → hostname guessing) covers >50% of users globally. Athen ships a **hardcoded provider list** for the first ten:
  - Gmail (`imap.gmail.com:993` / `smtp.gmail.com:587`, app-password)
  - Outlook (`imap-mail.outlook.com:993` / `smtp-mail.outlook.com:587`, app-password)
  - Fastmail (`imap.fastmail.com:993` / `smtp.send.fastmail.com:465`)
  - iCloud (`imap.mail.me.com:993`, app-specific password)
  - Proton (Bridge required — flag and link)
  - Yandex, GMX, Yahoo, ProtonBridge, generic fallback.
- **Google killed basic auth for IMAP March 2025** — must use either app-password (works today) or OAuth (CASA-audited later). Ship app-password path now; layer OAuth in Move #3.

**Build shape:**
- Onboarding wizard: ask email address → match domain → pre-fill server settings + inline "How to make a Gmail app password" link → ask for password → one-click test.
- `email_search(query, folder?, since?, until?, limit)` agent tool over IMAP IDLE for inbox monitoring (already a sense slot in `athen-sentidos`).
- LLM-translated error messages: if test fails with "AUTHENTICATIONFAILED", reply "Your password looks wrong. Did you use your account password? Gmail / Outlook / iCloud need an app-specific password — see [link]."

**Effort:** 4–6 days.

---

### 3. Personal OAuth wraps: GitHub + Microsoft Graph + Notion + Slack (status: design)


The "no friction" tier. All four offer free developer apps with zero verification, scope grants at consent time, standard refresh-token rotation. **Skip Google Workspace edit for now** (CASA audit $10k/yr, 3–6 weeks).

**Per provider:**
| Provider | Scopes ship-ready | Why now |
| --- | --- | --- |
| GitHub | repo, issue, PR, Actions | Zero verification. 5k req/h. Stateless tokens (revoke + re-auth). |
| Microsoft Graph | Outlook mail+cal, OneDrive | Zero verification. Metered ~$0.375/1k objects. Huge user base. |
| Notion | search, query DB, page CRUD | Zero verification. 3 req/s is plenty. Webhooks new in 2026 → no polling. |
| Slack | chat.history, search, files | Zero verification. Per-workspace token (multi-org users need to install per workspace — UX caveat). |

**Build shape:**
- **Device-code flow + QR** for OAuth (RFC 8628). Avoids the brittle localhost-listener path; works on any desktop without firewall fuss. Fallback to paste-token on timeout.
- Tokens live in `athen-vault` under scope `oauth:<provider>:<account>`. Proactive refresh 60s before expiry, family-revoke on logout (RFC 9700).
- One Rust crate per provider — handles pagination loops, scope-aware error translation, and refresh. Tools land in `AppToolRegistry` as a single group per provider (`github_*`, `outlook_*`, `notion_*`, `slack_*`).
- Defer Google Gmail/Calendar edit until traction justifies CASA. Gmail _read_ basic scope is reachable without CASA — could ship as a later "Google Light" move.

**Effort:** 1 week per provider (device-code flow + token refresh is the reusable plumbing, ~3 days; per-provider tool surface ~4 days). Recommend GitHub + Microsoft Graph first.

---

### 4. CalDAV / CardDAV calendar + contacts sync — **CalDAV SHIPPED (2026-05-15), CardDAV deferred**


[SHIPPED] **CalDAV calendar sync:** RFC 4791 adapter in `crates/athen-caldav/` handles iCloud, Google-via-CalDAV, Fastmail, Nextcloud, Yandex. Bidirectional sync loop at `crates/athen-app/src/calendar_sources.rs` polls every 5 min and reconciles on `(source_id, remote_id)` keys via ETag. Agent tools `calendar_create`/`update`/`delete` now push back to remote (committed 2026-05-17, `0b8e818`). Settings → Connections → Calendar Sources panel surfaces add/remove/pick-calendars UX with per-provider credential capture and automatic test-sync.

**CardDAV contacts sync:** deferred. Same architectural shape (provider autodetect, ETags, bidirectional). Lower urgency than calendar.

**Why behind OAuth wraps:** Microsoft / Google calendars are reachable via Graph and (eventually) Google APIs. CalDAV is the long-tail fallback. But it's the only path to iCloud, which matters for Mac-heavy users.

---

### 5. LLM-assisted credential setup panel (cross-cutting, status: partial)

Not a single integration — a UX layer that makes all of the above feel painless. **Partial today:** the LLM error translator landed alongside Move #2 (`email_translate_error` command) and is reusable for any future credential form. The rest of the cross-cutting tier (universal Test button state, device-code + QR flow, proactive token refresh, three-tier setup) is still design-only and lights up as Moves #1/#3/#4 land.

**The pattern (Thunderbird-ish but with LLM hindsight):**
1. **Test button on every credential form.** Spinner → ✓ green / ✗ red.
2. **LLM error translator:** when the test fails, pipe the raw error + the integration kind into a tiny structured-output call ("explain in one sentence + give the user a next step"). Cache responses so common errors don't re-roundtrip.
3. **Inline provider docs links** generated at form-render time (no chatbot needed for the happy path).
4. **Proactive token refresh** (60s pre-expiry). Re-auth flow surfaces through the existing approval router (Telegram + InApp) — never silently fail.
5. **Three-tier setup flow** across all integrations:
   - Tier A: autodetect + paste-password (IMAP/SMTP).
   - Tier B: paste-token (API keys, app passwords).
   - Tier C: device-code + QR (OAuth providers).

**Anti-patterns to ban (research-confirmed):**
- Opening a 404 in the browser. Sanity-check redirect URLs at first launch.
- "Run this CLI command to generate a key." Hard no.
- Test button with no visible state. Always spinner + green/red.
- Raw `invalid_grant` shown to the user without translation.

**Effort:** Cross-cutting; budget ~3 days of UX hardening once the first MCP / OAuth / IMAP wires are in. The error-translator LLM call is ~50 LOC.

---

## Defer / skip

- **Google Workspace edit scopes** (Docs/Sheets write). CASA audit $10k + annual re-verification. Revisit when there are paying users asking for it.
- **X / Twitter API.** No free tier post-2024. Skip.
- **Okta / Auth0.** Not a data source. Skip.
- **Apple OAuth.** Doesn't exist for personal accounts — use CalDAV/CardDAV (Move #4).
- **Playwright browser_login as a built-in.** Useful but slower payoff than MCP-byo (a community Playwright MCP already exists — [playwright-mcp](https://github.com/microsoft/playwright-mcp), 32k stars). Punt to "install this MCP" once Move #1 ships.
- **Bespoke wraps for stateless REST APIs** (Notion search, GitHub search, Stripe webhooks, Cloudflare DNS, Linear, etc. when there's no pagination/streaming pain). Stay as `http_request` presets or MCP servers.

## What this changes about future tool wraps

After Move #1 ships, the default answer to "should we wrap X?" flips:

1. Is there a community MCP for it? → install via MCP-byo gallery.
2. Is it a stateless REST API? → `http_request` preset.
3. Does it need OAuth refresh / streaming / pagination loops / state? → bespoke wrap.

That collapses the wrap-everything urge into a narrow path. The five moves above are exactly the things that don't fit (1) or (2) cleanly: MCP infra itself, email's protocol baggage, OAuth refresh plumbing, CalDAV's stateful sync, and the credential UX glue.
