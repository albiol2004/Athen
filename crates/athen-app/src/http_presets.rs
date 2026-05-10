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
            free_tier_blurb: "10M tokens/mo with key",
            signup_url: "https://jina.ai/?sui=apikey",
            test_path: "https://example.com",
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
            free_tier_blurb: "$5/mo credits (~1k queries)",
            signup_url: "https://api.search.brave.com/app/keys",
            test_path: "web/search?q=athen",
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
            free_tier_blurb: "250 searches/mo",
            signup_url: "https://serpapi.com/users/sign_up",
            test_path: "search.json?q=athen",
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
            free_tier_blurb: "50 lookups/mo",
            signup_url: "https://hunter.io/api-keys",
            test_path: "account",
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
            free_tier_blurb: "100 credits/mo (gated)",
            signup_url: "https://apolloiosettings.com/integrations",
            test_path: "auth/health",
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
            free_tier_blurb: "100 lookups/mo",
            signup_url: "https://www.peopledatalabs.com/",
            test_path: "person/enrich",
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
            free_tier_blurb: "100 req/day (developer)",
            signup_url: "https://newsapi.org/register",
            test_path: "top-headlines?country=us",
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
            free_tier_blurb: "No key, 10k req/day",
            signup_url: "https://open-meteo.com/",
            test_path: "forecast?latitude=47.55&longitude=7.59&hourly=temperature_2m",
        },
        EndpointPreset {
            slug: "frankfurter",
            label: "Frankfurter (FX)",
            provider: "Frankfurter",
            base_url: "https://api.frankfurter.app/",
            auth_method: AuthMethod::None,
            default_headers: vec![],
            suggested_risk: Some("low"),
            default_rate_limit_per_minute: 60,
            free_tier_blurb: "No key, ECB rates, unlimited",
            signup_url: "https://www.frankfurter.app/",
            test_path: "latest?from=EUR&to=USD",
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
            free_tier_blurb: "2.5k req/day",
            signup_url: "https://opencagedata.com/users/sign_up",
            test_path: "json?q=Basel,CH",
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
            free_tier_blurb: "10k chars/mo (non-commercial)",
            signup_url: "https://elevenlabs.io/app/settings/api-keys",
            test_path: "user/subscription",
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
            free_tier_blurb: "Several free models (DeepSeek, Llama)",
            signup_url: "https://openrouter.ai/keys",
            test_path: "models",
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
            free_tier_blurb: "30 req/min, 7.2k audio-sec/hr free",
            signup_url: "https://console.groq.com/keys",
            test_path: "models",
        },
    ]
}
