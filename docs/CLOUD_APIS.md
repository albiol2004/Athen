# Cloud API Expansion — `http_request` + Registered Endpoints

Companion to `TOOL_EXPANSION.md` (which covers OS-level CLI wrapping). This doc covers the **cloud-API expansion** — a single generic `http_request` tool backed by a "Registered HTTP Endpoints" store, mirroring the Contacts / Identity / proposed-Registered-Databases pattern.

The bet: **one ship unlocks ~15 APIs.** Instead of wrapping each cloud service in bespoke Rust, we ship a generic GET/POST tool whose credentials come from an encrypted SQLite store, plus a preset library so onboarding is "click provider → paste key → enable."

Surveyed 2026-05-10. APIs verified live; pricing snapshots will drift — re-verify before recommending paid tiers.

---

## Architecture

### Data model — `crates/athen-core/src/http_endpoint.rs` (new)

```rust
pub struct RegisteredEndpoint {
    pub id: Uuid,
    pub name: String,                              // PK display, e.g. "Jina"
    pub provider: String,                          // "Jina Reader"
    pub base_url: Url,
    pub enabled: bool,
    pub auth_method: AuthMethod,                   // see below
    pub default_headers: Vec<(String, String)>,
    pub default_query_params: Vec<(String, String)>,
    pub rate_limit: Option<RateLimit>,             // requests/min
    pub risk_override: Option<EndpointRisk>,
    pub notes: Option<String>,
    pub last_used: Option<DateTime<Utc>>,
    pub call_count_30d: u32,
    pub created_at: DateTime<Utc>,
}

pub enum AuthMethod {
    None,
    BearerToken { token: String },
    Header { name: String, value: String },        // X-API-Key, etc
    QueryParam { name: String, value: String },    // ?api_key=...
    BasicAuth { user: String, pass: String },
}

pub enum EndpointRisk { Low, Medium, High }
```

Credentials encrypted at rest (reuse existing `athen-core::crypto`); decrypted only inside the `http_request` executor and scrubbed from any error logs. The store implementation lives in `crates/athen-persistence/src/http_endpoints.rs`, parallels `identity.rs` / `contacts.rs`.

### Tool — `http_request`

```json
{
  "name": "http_request",
  "description": "Call a registered cloud HTTP API by name. Prefer bespoke tools (web_fetch, email_send, calendar_*) when one exists — they have richer schemas. Use this for one-off / less-common APIs the user has registered.",
  "input_schema": {
    "type": "object",
    "required": ["endpoint", "path"],
    "properties": {
      "endpoint": { "type": "string", "description": "Registered endpoint name (case-insensitive)" },
      "method":   { "type": "string", "enum": ["GET", "POST", "PUT", "DELETE", "PATCH"], "default": "GET" },
      "path":     { "type": "string", "description": "URL path joined to base_url" },
      "query":    { "type": "object", "additionalProperties": { "type": "string" } },
      "body":     { "description": "Request body (JSON). POST/PUT/PATCH only." },
      "headers":  { "type": "object", "additionalProperties": { "type": "string" } }
    }
  }
}
```

Returns `{ status, headers, body, latency_ms }`. Body is parsed JSON when `Content-Type: application/json`, else `{ raw_text: "…" }`.

### Risk model

Default tiers (overridable per endpoint at registration):

| Method | Default risk | Notes |
|---|---|---|
| `GET` no auth | Low | Auto-approve |
| `GET` with auth | Medium | Reads private data |
| `POST/PUT/DELETE/PATCH` | High | Mutates external system; per-action approval by default |

The UI's add-endpoint modal asks "what does this endpoint mainly do?" → maps to a risk override. Power users can flag a specific endpoint as fully trusted.

### Pre-call safety

- **Bespoke-tool nudge:** if the agent calls `http_request` for an endpoint that already has a bespoke wrapper (e.g. Jina via `web_fetch`), the executor emits a one-line reasoning warning ("`web_fetch` already wraps Jina with fallback chain — prefer it"). Doesn't block; informs.
- **Rate limiting:** in-memory sliding window per endpoint. If `rate_limit.requests_per_minute` is exceeded, return `{ error: "Rate limit 50/min exceeded (47 calls in past 60s). Try again in 1 min." }`.
- **Credential scrubbing:** any error message that would echo back the auth header / query param has the value replaced with `[REDACTED]`.

### UI — Settings → Cloud APIs

Mirrors the Identity panel layout: list of registered endpoints with provider, last-used, 30-day call count, enable toggle, edit/delete. "+ Add Endpoint" opens a modal with a **provider preset dropdown** that pre-fills base_url + auth_method + default headers; user just enters the API key. "Test connection" button issues a HEAD or known-safe GET and shows status.

### Crate paths

| Component | Crate | File |
|---|---|---|
| Types + trait | `athen-core` | `src/http_endpoint.rs` + `src/traits/http_endpoint.rs` |
| SQLite store | `athen-persistence` | `src/http_endpoints.rs` |
| Tool dispatch | `athen-app` | `src/app_tools.rs` (new `do_http_request`) |
| Tauri commands | `athen-app` | `src/commands.rs` (CRUD + test) |
| Preset library | `athen-app` | `src/http_presets.rs` |
| Frontend | `frontend/` | extend `index.html` + `app.js` + `styles.css` |

---

## Preset library — ship 15 enabled-by-default-disabled out of the box

| Preset | Base URL | Auth | Default risk | Free tier (verify before recommending) |
|---|---|---|---|---|
| Jina Reader | `https://r.jina.ai/` | Bearer | Low | 10M tokens/mo |
| Firecrawl | `https://api.firecrawl.dev/v2/` | Bearer | Low | 1k credits/mo |
| Crawlbase | `https://api.crawlbase.com/` | QueryParam(token) | Low | 1k req/mo |
| Brave Search | `https://api.search.brave.com/res/v1/` | Header(X-Subscription-Token) | Low | $5 credits/mo (~1k queries) |
| SerpAPI | `https://serpapi.com/` | QueryParam(api_key) | Low | 250 searches/mo |
| Hunter.io | `https://api.hunter.io/v2/` | QueryParam(api_key) | Medium | 50/mo |
| Snov.io | `https://api.snov.io/v1/` | OAuth Bearer | Medium | 50/mo |
| Apollo.io | `https://api.apollo.io/api/v1/` | Header(X-Api-Key) | Medium | 100 credits/mo (gated) |
| People Data Labs | `https://api.peopledatalabs.com/v5/` | Header(X-Api-Key) | Medium | 100 lookups/mo |
| DeepL | `https://api-free.deepl.com/v2/` | Header(Authorization: DeepL-Auth-Key) | Low | 500k chars/mo |
| NewsAPI | `https://newsapi.org/v2/` | QueryParam(apiKey) | Low | 100 req/day |
| Adzuna | `https://api.adzuna.com/v1/api/` | QueryParam(app_id, app_key) | Low | Free dev tier |
| Open-Meteo | `https://api.open-meteo.com/v1/` | None | Low | 10k req/day |
| Frankfurter (FX) | `https://api.frankfurter.app/` | None | Low | Unlimited (ECB rates) |
| OpenCage Geocoding | `https://api.opencagedata.com/geocode/v1/` | QueryParam(key) | Low | 2.5k req/day |
| ElevenLabs TTS | `https://api.elevenlabs.io/v1/` | Header(xi-api-key) | Medium | 10k chars/mo (non-commercial) |
| Cal.com | `https://api.cal.com/v1/` | Bearer | High | 25 bookings/mo |
| OpenRouter (LLM fallback) | `https://openrouter.ai/api/v1/` | Bearer | Medium | Some free models |
| Groq (LLM + Whisper) | `https://api.groq.com/openai/v1/` | Bearer | Medium | 30 req/min, 7.2k audio-sec/hr |
| Wikipedia | `https://en.wikipedia.org/w/api.php` | None | Low | Unlimited (polite) |

User picks from the list, pastes key (or leaves blank for keyless ones), enables. Never bigger than ~5 minutes of setup per endpoint.

---

## Per-category notes (highlights)

### Scraping APIs (deep dive — user's stated priority)

- **Jina Reader** is already wired in `web_fetch` (free 10M tokens/mo with key, EU mirror at `eu.r.jina.ai`). The earlier survey claimed an `x-json-schema` extract header; the live docs don't currently show it — verify before relying on it.
- **Firecrawl** is the best schema-driven extractor — `POST /v2/scrape` with `formats: ["json"], jsonOptions: { schema }` returns matching JSON. Free 1k/mo, $49/mo for 50k. **Top recommended add.**
- **Crawlbase** is the cheapest anti-bot bypass at low volume (free 1k/mo, then $3–6/1k). LinkedIn at $15/1k.
- **ScrapingBee / ScraperAPI / ZenRows** all sit around 1k free / $49–249/mo paid. ScrapingBee has the cleanest free tier (no card required).
- **Brave Search API** is the cleanest SERP option — $5/mo free credits = ~1k queries. SerpAPI is more featureful (250/mo free) but pricier. **Recommend both as presets.**

**Best free combo for 50–500 pages/month:** Jina (markdown baseline, already wired) + Firecrawl (structured JSON) + Crawlbase (anti-bot fallback) + Brave Search (SERP). Total: $0.

### Lead enrichment

- **Hunter.io** sweet spot at ≤2k/mo (50 free, $49/mo for 2k credits covering both find + verify).
- **People Data Labs** for deeper enrichment (100 free, $98/mo for 350) — returns email/phone/job/funding/tech-stack.
- **Apollo.io** has a misleading free tier (100 credits/mo gated to corporate emails); useful sample but the real API is on paid tiers.
- **EU/DACH:** Kaspr (€49/mo, France-based, GDPR-native, LinkedIn waterfall) is the right tool when targeting Switzerland / DACH outreach. Cognism is enterprise-only.
- **Dead/changed:** Clearbit was acquired by HubSpot (Logo API shut Dec 2024), Crunchbase killed its free API tier in 2025, Lusha doubled phone-reveal cost.

### General utility

- **DeepL** translation — 500k chars/mo free, best EU-language quality.
- **Open-Meteo** — no auth, 10k req/day, perfect for "what's the weather in Basel tomorrow".
- **Frankfurter** — ECB rates, no auth, unlimited.
- **Groq** — 30 req/min free tier on Llama + Whisper (7.2k audio-sec/hr free transcription). Excellent LLM fallback when DeepSeek throttles.
- **OpenRouter** — unified API for 100+ models with some free tiers (DeepSeek V3/R1, Llama).
- **Cal.com** — open-source self-hostable; cloud has 25 free bookings/mo.

---

## Where this lands in ship priority

This **becomes the new #1** in the tool-expansion ship order from `TOOL_EXPANSION.md`. Rationale:

- One feature ship unlocks ~15 APIs simultaneously.
- The user's stated bottleneck ("agent is pretty capable now to use with shell") is solved at the lowest-cost wrap.
- The "registered endpoint" pattern is the right abstraction the user already asked for ("registered databases" came up in the database survey too — same shape works for both, factor common code).
- Doesn't conflict with bespoke tools — `web_fetch`, `email_send`, etc stay as they are; this is additive.

Updated ship order (replaces §"Recommended ship order" in `TOOL_EXPANSION.md`):

1. **`http_request` + Registered Endpoints + 15 presets** (4–6 days). Unlocks ~15 cloud APIs in one diff.
2. **`web_fetch_json` schema extension** (1 day) — extends the existing `web_fetch` chain with Jina/Firecrawl structured extraction. May actually be subsumed by #1 once the Firecrawl preset is in.
3. **`email_search` / `email_inbox`** (2–3 days) — IMAP wrapper; closes the inbound side of email; required for the outreach loop.
4. **Outreach primitives + sequences on wakeups** (1 week) — flagship for the consulting practice.
5. **Job search primitives** (3–4 days) — composes with #4 for monitor → research → personalised outreach end-to-end.
6. **Browser automation** (1 week) — `playwright-rs`. LinkedIn, Glassdoor, Malt unlock.
7. **Social via Postiz** (3–4 days).

---

## What `http_request` is **not**

- Not a replacement for `web_fetch` — that has a fallback chain (DDG → Tavily → Local → Jina → Wayback → Cloudflare) that's bespoke logic worth preserving.
- Not a replacement for any tool that already exists (`email_send`, `calendar_*`, `contacts_*`). Tool description nudges the agent toward bespoke when one is available.
- Not a generic web crawler — for paginated/depth crawling, see the future `web_crawl_paginated` primitive in `TOOL_EXPANSION.md` §9.
- Not a permission-bypass — it goes through the same risk gating as every other tool. POST defaults to High risk; user-trusted endpoints can downgrade per registration.

---

## Cross-cutting prerequisite still to land

**Encryption at rest for credentials.** Today Athen stores config in plain SQLite. Before shipping `http_request`, we need a `crypto` module (`athen-core::crypto` or new `athen-crypto` crate) that gives us:
- Master key derivation (Argon2id from a user passphrase OR system-keychain-managed key)
- AEAD encrypt/decrypt (chacha20poly1305 or AES-256-GCM)

Same module will be reused for: HTTP endpoint credentials, future Registered Databases connection strings, OAuth tokens for X / LinkedIn / Gmail. So this is **infrastructure work**, not just for `http_request`.

If we want to ship `http_request` ASAP without the crypto module, the v0 cut can store credentials base64-encoded with a clear "v0 — credentials are NOT encrypted at rest yet" warning in the UI. Trades security for ship speed; would only be acceptable in dev builds.
