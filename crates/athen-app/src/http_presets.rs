//! Preset library for the "+ Add Endpoint" modal.
//!
//! Each preset prefills `base_url + auth_method + default_headers + notes`
//! so the user only enters an API key and clicks Save. Selection in the UI
//! is by `slug`; the human label is displayed but never round-trips.
//!
//! Presets are static — re-shipping the binary is the migration path. If a
//! provider URL changes, bump the preset and any saved endpoints stay
//! pointing at the (now-broken) old URL until the user edits them. That
//! preserves user intent ("you registered THIS specific URL"), but the UI
//! should surface a "preset URL changed" hint when it can detect a match.
//!
//! Sources for the 15 presets are catalogued in `docs/CLOUD_APIS.md`. A
//! new preset is one entry here + one re-build away — no DB migration.

use serde::Serialize;

use athen_core::http_endpoint::AuthMethod;

#[derive(Debug, Clone, Serialize)]
pub struct EndpointPreset {
    /// Stable identifier for UI selection. Lowercase, snake-ish.
    pub slug: &'static str,
    /// Display label shown in the dropdown.
    pub label: &'static str,
    /// Human-readable provider — copied into `RegisteredEndpoint.provider`
    /// when the user picks the preset.
    pub provider: &'static str,
    pub base_url: &'static str,
    pub auth_method: AuthMethod,
    pub default_headers: Vec<(String, String)>,
    /// Suggested risk override for the dropdown ("low" / "medium" / "high").
    /// `None` falls back to the per-method default when the user saves.
    pub suggested_risk: Option<&'static str>,
    pub default_rate_limit_per_minute: u32,
    /// One-line free-tier blurb shown under the preset name. Drift hazard
    /// — the value is informational, not enforced; check the upstream
    /// page before paying.
    pub free_tier_blurb: &'static str,
    /// Where to register / get a key, and a sample path the test button
    /// can hit. Both shown as helper text in the modal.
    pub signup_url: &'static str,
    pub test_path: &'static str,
    /// Operational hints shown to the agent in the per-endpoint detail
    /// markdown file (tier-3 doc). Distilled from real API research,
    /// not marketing copy: endpoints, key params, response shape, and
    /// 1–2 critical gotchas. Only loaded when the agent reads the
    /// per-endpoint detail file — never in the always-present prompt.
    pub usage_hints: &'static str,
}

fn h(name: &str, value: &str) -> (String, String) {
    (name.to_string(), value.to_string())
}

/// Ship 15 presets out of the box. Order is roughly the value-per-API
/// ranking from `docs/CLOUD_APIS.md` so the dropdown's first entries
/// are the ones most users actually want.
pub fn presets() -> Vec<EndpointPreset> {
    vec![
        EndpointPreset {
            slug: "jina_reader",
            label: "Jina Reader",
            provider: "Jina AI",
            base_url: "https://r.jina.ai/",
            auth_method: AuthMethod::BearerToken,
            default_headers: vec![h("Accept", "application/json")],
            suggested_risk: Some("low"),
            default_rate_limit_per_minute: 60,
            free_tier_blurb: "10M tokens one-time with key (~free w/o key at 20 RPM)",
            signup_url: "https://jina.ai/api-dashboard/",
            test_path: "https://example.com",
            usage_hints: "GET `https://r.jina.ai/{full_url}` converts a page to clean markdown — pass the full target URL as `path` (no encoding needed). Auth is optional (20 RPM unauthed, 500+ RPM with key).\n\n**Headers that change behaviour:**\n- `X-Respond-With: markdown|html|text|screenshot|pageshot` controls output format (default: markdown).\n- `X-With-Generated-Alt: true` adds AI-generated alt-text for images.\n- `Accept: application/json` wraps the response in a JSON envelope; otherwise body is raw markdown.\n\n**Quirks:**\n- The 10M-token allowance is one-time per key (shared across Jina services), not monthly.\n- Some sites actively block extraction — expect occasional empty bodies even on 200s.",
        },
        EndpointPreset {
            slug: "firecrawl",
            label: "Firecrawl",
            provider: "Firecrawl",
            base_url: "https://api.firecrawl.dev/v2/",
            auth_method: AuthMethod::BearerToken,
            default_headers: vec![h("Content-Type", "application/json")],
            suggested_risk: Some("low"),
            default_rate_limit_per_minute: 30,
            free_tier_blurb: "1k credits/mo",
            signup_url: "https://www.firecrawl.dev/",
            test_path: "scrape",
            usage_hints: "All endpoints are POST + JSON body.\n\n- `scrape` — synchronous, returns content. Body: `url` (required) + optional `formats:[\"markdown\",\"html\",\"screenshot\",\"links\"]`, `onlyMainContent` (default true — strips sidebars), `maxAge` (default 2 days; cache hits ~5x faster).\n- `crawl` — async spider. Returns a job ID; poll `/v2/crawl/{id}`. Set `limit`, `maxDiscoveryDepth`, `scrapeOptions.formats`.\n- `map` — flat URL list for a domain.\n- `search` — search results + full page content.\n- `agent` — replaces deprecated `extract`; structured data via natural-language prompt.\n\n**Costs:** most formats 1 credit; `Interact` (click/scroll/JS actions) 5 credits. JS render is OFF by default — enable via `actions`. v1 still works at `/v1/` if v2 response shapes surprise.",
        },
        EndpointPreset {
            slug: "brave_search",
            label: "Brave Search",
            provider: "Brave Search",
            base_url: "https://api.search.brave.com/res/v1/",
            auth_method: AuthMethod::Header {
                name: "X-Subscription-Token".to_string(),
            },
            default_headers: vec![h("Accept", "application/json")],
            suggested_risk: Some("low"),
            default_rate_limit_per_minute: 60,
            free_tier_blurb: "$5/mo credits (~1k queries) for new accounts; legacy 2k/mo for existing free tier",
            signup_url: "https://api.search.brave.com/app/keys",
            test_path: "web/search?q=athen",
            usage_hints: "**Endpoints:** `web/search`, `news/search`, `images/search`, `videos/search`, `summarizer/search` (unbilled), `local/pois`.\n\n**Params:** `q` (required), `count`, `country`, `search_lang`, `freshness` — `pd`/`pw`/`pm`/`py` for past day/week/month/year, or `YYYY-MM-DDtoYYYY-MM-DD` range.\n\n**Response:** `web.results[].{title, url, description}`, plus `extra_snippets[]` when enabled. `summarizer/search` returns AI-generated summary text.\n\n**Rate limits by plan:** legacy free tier 1 qps / 2k mo (existing accounts only — new sign-ups don't get this), new accounts $5/mo metered credits at 1 qps, Search Plan 50 qps. Summarizer requests don't bill tokens.",
        },
        EndpointPreset {
            slug: "serpapi",
            label: "SerpAPI",
            provider: "SerpAPI",
            base_url: "https://serpapi.com/",
            auth_method: AuthMethod::QueryParam {
                name: "api_key".to_string(),
            },
            default_headers: vec![],
            suggested_risk: Some("low"),
            default_rate_limit_per_minute: 30,
            free_tier_blurb: "100 searches/mo",
            signup_url: "https://serpapi.com/users/sign_up",
            test_path: "search.json?q=athen",
            usage_hints: "Set `engine=google|google_images|google_news|google_scholar|bing|duckduckgo|youtube|google_maps|amazon|walmart` (100+ engines available). Path is `search` (canonical) or `search.json` (legacy alias) — both return JSON.\n\n**Params:** `q` (required), `location`, `hl` (lang), `gl` (country), `start` (pagination offset). Do NOT use `num` — it's not in the current API; use `start` instead.\n\n**Response:** `organic_results[]`, `knowledge_graph`, `answer_box`, `shopping_results`, `related_searches`, plus engine-specific arrays.\n\n**Free tier:** 100 searches/month (only successful counted; cached/errored are free), non-commercial use only. Async via `async=true` then poll `search/archive/{search_id}` later.",
        },
        EndpointPreset {
            slug: "hunter_io",
            label: "Hunter.io",
            provider: "Hunter",
            base_url: "https://api.hunter.io/v2/",
            auth_method: AuthMethod::QueryParam {
                name: "api_key".to_string(),
            },
            default_headers: vec![],
            suggested_risk: Some("medium"),
            default_rate_limit_per_minute: 15,
            free_tier_blurb: "50 credits/mo (~25 finds + 50 verifies)",
            signup_url: "https://hunter.io/api-keys",
            test_path: "account",
            usage_hints: "**Endpoints + cost (in credits):**\n- `domain-search` — 1 per call (returns 1–10 emails for a domain)\n- `email-finder` — 1 (find email by name + domain/company)\n- `email-verifier` — 0.5 (deliverability check)\n- `email-count` — free (count without revealing)\n- `account` — free (health check)\n- `email-enrichment`, `combined-enrichment` — 0.2 each\n\n**Params:** `domain`, `company`, `first_name` + `last_name`, `email`. **Response:** `data.emails[]` (domain-search), `data.email` + `data.score` (finder/verifier — score 0–100, <50 = high-risk delivery), `meta.results` for pagination.\n\n**Limits:** ~15 req/sec for most endpoints; verifier stricter at ~10 req/sec. **GDPR:** don't cold-outreach EU contacts without lawful basis.",
        },
        EndpointPreset {
            slug: "apollo_io",
            label: "Apollo.io",
            provider: "Apollo",
            base_url: "https://api.apollo.io/api/v1/",
            auth_method: AuthMethod::Header {
                name: "X-Api-Key".to_string(),
            },
            default_headers: vec![h("Content-Type", "application/json")],
            suggested_risk: Some("medium"),
            default_rate_limit_per_minute: 60,
            free_tier_blurb: "100 email credits/mo (10k for corporate-domain accounts)",
            signup_url: "https://app.apollo.io/#/settings/integrations",
            test_path: "auth/health",
            usage_hints: "B2B prospect & company search + enrichment. Most endpoints are POST + JSON body.\n\n**Endpoints:**\n- `mixed_people/search` — filters: `q_keywords`, `person_titles[]`, `organization_locations[]`, `person_seniorities[]`, `page`, `per_page`.\n- `mixed_companies/search` — same filter shape for orgs.\n- `people/match` — identify a contact; pass `reveal_personal_emails: true` to spend a credit on the email.\n- `organizations/enrich` — single-org enrich.\n- `auth/health` — GET, returns `{healthy: true, is_logged_in: true}` (no credit).\n\n**Path inconsistency:** some endpoints live under `/api/v1/`, others under `/v1/` — check per-endpoint docs if a 404 surprises you. **Response:** `people[]` / `organizations[]` + `pagination.total_entries`.\n\n**Critical:** scoped (non-master) keys 403 on most endpoints. Email/phone reveals require master key + spend credits per reveal.",
        },
        EndpointPreset {
            slug: "people_data_labs",
            label: "People Data Labs",
            provider: "People Data Labs",
            base_url: "https://api.peopledatalabs.com/v5/",
            auth_method: AuthMethod::Header {
                name: "X-Api-Key".to_string(),
            },
            default_headers: vec![],
            suggested_risk: Some("medium"),
            default_rate_limit_per_minute: 30,
            free_tier_blurb: "100 lookups/mo, 100 req/min",
            signup_url: "https://www.peopledatalabs.com/",
            test_path: "person/enrich",
            usage_hints: "v5 endpoints: `person/enrich` (GET or POST), `person/search` (POST, ES or SQL syntax), `company/enrich`, `company/search`, `person/identify`, `person/bulk`, `autocomplete`.\n\n**Enrich params:** `email`, `phone`, `profile` (LinkedIn URL), or `name` + `company`/`location`. `min_likelihood` (1–10, default 2) raises the match-confidence floor.\n\n**Response:** top-level `status` (200 = match found, 404 = no match), `likelihood` (1–10), `data.{full_name, emails[], experience[], …}`.\n\n**Key economic gotcha:** credits are charged ONLY on 200 responses — 404s are free, so set `min_likelihood` high if you'd rather miss than pay for low-confidence matches. Free tier masks contact fields as booleans (you see *that* an email exists, not the value).",
        },
        EndpointPreset {
            slug: "deepl",
            label: "DeepL",
            provider: "DeepL",
            base_url: "https://api-free.deepl.com/v2/",
            auth_method: AuthMethod::Header {
                name: "Authorization".to_string(),
            },
            default_headers: vec![],
            suggested_risk: Some("low"),
            default_rate_limit_per_minute: 60,
            free_tier_blurb: "500k chars/mo (free key prefix 'DeepL-Auth-Key ')",
            signup_url: "https://www.deepl.com/pro-api",
            test_path: "usage",
            usage_hints: "**Auth value MUST be `DeepL-Auth-Key <key>`** — bare key fails with 403. Free keys end `:fx` and ONLY work at `api-free.deepl.com`; pro keys (no suffix) at `api.deepl.com`.\n\n**Endpoints (POST unless noted):**\n- `translate` — JSON or form-encoded; body: `text[]`, `target_lang`, `source_lang`, `formality`, `preserve_formatting`, `tag_handling`.\n- `usage` (GET) — returns `character_count`, `character_limit`.\n- `languages` (GET), `glossaries` (CRUD), `document` (upload).\n\n**Response:** `translations[].text`, `translations[].detected_source_language`. Each text item in one call translates independently — no cross-text context. JSON body is preferred over form-encoded.",
        },
        EndpointPreset {
            slug: "newsapi",
            label: "NewsAPI",
            provider: "NewsAPI.org",
            base_url: "https://newsapi.org/v2/",
            auth_method: AuthMethod::QueryParam {
                name: "apiKey".to_string(),
            },
            default_headers: vec![],
            suggested_risk: Some("low"),
            default_rate_limit_per_minute: 60,
            free_tier_blurb: "100 req/day (developer — dev/localhost only, no commercial use)",
            signup_url: "https://newsapi.org/register",
            test_path: "top-headlines?country=us",
            usage_hints: "**Endpoints:**\n- `top-headlines` — params: `country` (ISO 3166 lowercase, e.g. `us`, `gb`, `de`), `category`, `sources`, `q`, `pageSize` (≤100), `page`. **Cannot mix `country` + `sources`.**\n- `everything` — full-text search across articles. Only this endpoint accepts `from`/`to` (ISO 8601) and `sortBy=relevancy|popularity|publishedAt`.\n- `top-headlines/sources` — list available sources.\n\n**Response:** `{status, totalResults, articles[].{source.{id, name}, title, description, url, urlToImage, publishedAt, content}}`.\n\n**Developer-key restrictions to plan around:** dev / localhost only (no production / CORS-locked), articles <24h old not searchable, 1-month history cap, `content` truncated to ~200 chars. Free: 100 req/day total.",
        },
        EndpointPreset {
            slug: "open_meteo",
            label: "Open-Meteo",
            provider: "Open-Meteo",
            base_url: "https://api.open-meteo.com/v1/",
            auth_method: AuthMethod::None,
            default_headers: vec![],
            suggested_risk: Some("low"),
            default_rate_limit_per_minute: 100,
            free_tier_blurb: "No key — 10k req/day, 5k/hr, 600/min (non-commercial)",
            signup_url: "https://open-meteo.com/",
            test_path: "forecast?latitude=47.55&longitude=7.59&hourly=temperature_2m",
            usage_hints: "This preset hits the **forecast** API only (`/v1/forecast`).\n\n**Params:** `latitude`, `longitude`, CSV `hourly` (e.g. `temperature_2m,precipitation,weathercode,relative_humidity_2m,wind_speed_10m,cloudcover`), optional `daily`, `current`, `timezone=auto`, `forecast_days`, `temperature_unit`, `wind_speed_unit`.\n\n**Response shape:** parallel arrays — `hourly.time[]` aligns 1:1 with `hourly.temperature_2m[]`, etc.\n\n**Other Open-Meteo APIs are SEPARATE hosts** — register each as its own endpoint if needed:\n- archive: `archive-api.open-meteo.com/v1/archive` (history from 1940)\n- marine: `marine-api.open-meteo.com/v1/marine`\n- air-quality: `air-quality-api.open-meteo.com/v1/air-quality`\n- geocoding: `geocoding-api.open-meteo.com/v1/search?name=...`\n\nCommercial use needs a paid key at `customer-api.open-meteo.com`.",
        },
        EndpointPreset {
            slug: "frankfurter",
            label: "Frankfurter (FX)",
            provider: "Frankfurter",
            base_url: "https://api.frankfurter.dev/v1/",
            auth_method: AuthMethod::None,
            default_headers: vec![],
            suggested_risk: Some("low"),
            default_rate_limit_per_minute: 60,
            free_tier_blurb: "No key, ECB rates, unlimited (daily updates only)",
            signup_url: "https://frankfurter.dev/",
            test_path: "latest?from=EUR&to=USD",
            usage_hints: "ECB reference exchange rates (~30 currencies, daily updates from 2016+, weekend/holiday gaps).\n\n**Paths under `/v1/`:**\n- `latest` — current rates\n- `<YYYY-MM-DD>` — single date\n- `<YYYY-MM-DD>..<YYYY-MM-DD>` — range\n- `<YYYY-MM-DD>..` — since date\n- `currencies` — list supported codes\n\n**Params:** `base` (default EUR; `from` is also accepted as alias), `symbols` (CSV target list; `to` is alias), `amount` (multiplier).\n\n**Response:** single → `{amount, base, date, rates: {USD: ...}}`. Range → adds `start_date`, `end_date`, `rates: {<date>: {USD: ...}}`.\n\nNot real-time forex — daily ECB reference rates only. The legacy `api.frankfurter.app` host still 301-redirects to `.dev` if the new host ever 5xx's.",
        },
        EndpointPreset {
            slug: "opencage",
            label: "OpenCage Geocoding",
            provider: "OpenCage",
            base_url: "https://api.opencagedata.com/geocode/v1/",
            auth_method: AuthMethod::QueryParam {
                name: "key".to_string(),
            },
            default_headers: vec![],
            suggested_risk: Some("low"),
            default_rate_limit_per_minute: 60,
            free_tier_blurb: "2.5k req/day, 1 req/sec",
            signup_url: "https://opencagedata.com/users/sign_up",
            test_path: "json?q=Basel,CH",
            usage_hints: "Single endpoint `json` handles BOTH forward and reverse geocoding — distinguished by query format.\n\n**Forward** (address → coords): `q=<address or place name>`, plus optional `countrycode` (ISO 3166-1), `bounds`, `proximity`, `language`, `limit` (≤100), `min_confidence`, `no_annotations=1` (saves bytes by dropping timezone/currency/sun/what3words).\n\n**Reverse** (coords → address): `q=<lat>,<lng>` — same endpoint. Returns ≤1 result.\n\n**Response:** `results[].geometry.{lat, lng}`, `results[].formatted`, `results[].components.{country, city, postcode, road, ...}`, `results[].confidence` (1–10).\n\n**Always check `status.code`:** 200 ok, 401 invalid key, 402 quota exceeded, 403 disabled. `total_results: 0` means success but empty match (don't treat as error).",
        },
        EndpointPreset {
            slug: "elevenlabs",
            label: "ElevenLabs TTS",
            provider: "ElevenLabs",
            base_url: "https://api.elevenlabs.io/v1/",
            auth_method: AuthMethod::Header {
                name: "xi-api-key".to_string(),
            },
            default_headers: vec![],
            suggested_risk: Some("medium"),
            default_rate_limit_per_minute: 30,
            free_tier_blurb: "10k credits/mo (~10 min Multilingual v2; attribution required, no voice cloning)",
            signup_url: "https://elevenlabs.io/app/settings/api-keys",
            test_path: "user/subscription",
            usage_hints: "**TTS endpoint:** `POST text-to-speech/{voice_id}` — **returns binary audio** (default `audio/mpeg`), not JSON. The agent must save bytes, not parse.\n\nBody: `text` (required), `model_id` (`eleven_multilingual_v2` default; `eleven_turbo_v2_5` and `eleven_flash_v2_5` for speed), `voice_settings.{stability, similarity_boost, style, use_speaker_boost}`. Query `output_format` (`mp3_44100_128`, `pcm_16000`, etc.) controls codec.\n\n**Other endpoints:** `voices` (GET list), `models`, `user`, `user/subscription` (quota — `character_count`/`character_limit`), `text-to-speech/{voice_id}/stream` (chunks), `speech-to-text` (multipart upload), `sound-generation` (text → SFX).\n\n**Free-tier limits:** 10k credits/mo, attribution required in published content, voice cloning blocked (paid plan only).",
        },
        EndpointPreset {
            slug: "openrouter",
            label: "OpenRouter (LLM fallback)",
            provider: "OpenRouter",
            base_url: "https://openrouter.ai/api/v1/",
            auth_method: AuthMethod::BearerToken,
            default_headers: vec![h("Content-Type", "application/json")],
            suggested_risk: Some("medium"),
            default_rate_limit_per_minute: 30,
            free_tier_blurb: "Several free models (DeepSeek R1, Llama 3.3 70B, Qwen3, Gemma3, …)",
            signup_url: "https://openrouter.ai/keys",
            test_path: "models",
            usage_hints: "**OpenAI-compatible** — drop-in replacement: same `chat/completions`, `completions`, `models`, `embeddings`, `generation/{id}` shapes. Existing OpenAI clients work by swapping base URL + key.\n\n**Free models append `:free`** to the slug — e.g. `deepseek/deepseek-r1:free`, `meta-llama/llama-3.3-70b-instruct:free`, `qwen/qwen3-coder-480b-a35b-instruct:free`, `google/gemma-3-27b-it:free`.\n\n**Free-tier rate limits:** 20 req/min per model, 200 req/day (1000/day if ≥$10 has ever been loaded onto the account).\n\n**Optional headers:** `HTTP-Referer` and `X-Title` set app attribution / leaderboard rank.\n\n**Useful body extras:** `provider.order[]` (provider preference), `provider.allow_fallbacks`, `models[]` (auto-fallback list), `transforms: [\"middle-out\"]` (compress oversize prompts). Response `id` → call `generation/{id}` for cost + token breakdown.",
        },
        EndpointPreset {
            slug: "groq",
            label: "Groq (LLM + Whisper)",
            provider: "Groq",
            base_url: "https://api.groq.com/openai/v1/",
            auth_method: AuthMethod::BearerToken,
            default_headers: vec![h("Content-Type", "application/json")],
            suggested_risk: Some("medium"),
            default_rate_limit_per_minute: 30,
            free_tier_blurb: "30 RPM, ~6k TPM, 7,200 audio-sec/hr (cascading caps — 429 hits whichever first)",
            signup_url: "https://console.groq.com/keys",
            test_path: "models",
            usage_hints: "**OpenAI-compatible** at `/openai/v1/`: `chat/completions`, `models`, `audio/transcriptions`, `audio/translations`, `audio/speech` (TTS), limited `embeddings`.\n\n**Flagship models (Q2 2026):** `llama-3.3-70b-versatile`, `llama-3.1-8b-instant`, Qwen3 32B, DeepSeek R1 distill (70B/32B), GPT-OSS 20B/120B, `whisper-large-v3-turbo`. Tool use + JSON mode supported on most chat models.\n\n**Whisper transcription:** multipart `file` + `model=whisper-large-v3-turbo` + optional `language`, `prompt`, `response_format=verbose_json`.\n\n**Headline feature:** raw throughput — 300–1000+ tok/s on chat.\n\n**Free tier has THREE cascading limits:** 30 RPM, ~6k TPM (chat), 7,200 audio-sec/hr — 429 hits whichever arrives first. Response includes `x-ratelimit-reset-*` headers telling you exactly when to retry.",
        },
    ]
}
