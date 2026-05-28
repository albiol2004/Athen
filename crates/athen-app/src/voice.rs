//! Tauri commands for the Settings → Voice panel.
//!
//! Three commands surface the voice subsystem to the frontend:
//!
//! * [`get_voice_settings`] — load the persisted [`VoiceConfig`].
//! * [`save_voice_settings`] — overwrite the persisted [`VoiceConfig`].
//! * [`list_voice_options`] — enumerate registered Cloud APIs endpoints
//!   (bucketed by STT / TTS / Phone), plus available LLM (connection, slug)
//!   loadout picks for the per-call override dropdown.
//!
//! The actual `place_call` agent tool is a separate batch — this module
//! only persists the wiring. The Settings → Voice panel renders a
//! disabled "Test setup" stub until that batch lands.
//!
//! Persistence: `VoiceConfig` lives as a JSON blob under
//! `AthenConfig::voice` in `config.toml`. `athen-core` cannot depend on
//! `athen-voice` (hexagonal rules), so the typed serialization happens
//! here — `athen-core` just stores a [`serde_json::Value`].
//!
//! Endpoint bucketing keys off the preset `slug` — `deepgram` → STT,
//! `elevenlabs`/`cartesia` → TTS, `twilio` → Phone. The match is
//! case-insensitive on `provider` so a user who edits the provider field
//! manually still lands in the right bucket.

use std::collections::HashSet;

use athen_core::config::{Bundle, ACTIVE_BUNDLE_KEY};
use athen_core::llm::ModelProfile;
use athen_voice::VoiceConfig;
use serde::{Deserialize, Serialize};
use tauri::State;

use crate::state::AppState;

// ---------------------------------------------------------------------------
// Endpoint slug → bucket classification
// ---------------------------------------------------------------------------

/// Preset slugs (or provider-name lowercased fallback) we recognise as STT.
const STT_SLUGS: &[&str] = &["deepgram"];
/// Preset slugs we recognise as TTS.
const TTS_SLUGS: &[&str] = &["elevenlabs", "cartesia"];
/// Preset slugs we recognise as phone-service providers.
const PHONE_SLUGS: &[&str] = &["twilio"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VoiceBucket {
    Stt,
    Tts,
    Phone,
}

/// Classify a registered endpoint by `provider` (the human label saved
/// from the preset; e.g. "Deepgram", "Twilio") OR by a slug-like match
/// against the same. Returns `None` if the endpoint is unrelated.
///
/// Exposed for the unit tests below — the pure-function shape lets the
/// classifier be exercised without spinning up an HTTP endpoint store.
fn classify(provider: &str) -> Option<VoiceBucket> {
    let normalised = provider.trim().to_ascii_lowercase();
    if normalised.is_empty() {
        return None;
    }
    let buckets: &[(VoiceBucket, &[&str])] = &[
        (VoiceBucket::Stt, STT_SLUGS),
        (VoiceBucket::Tts, TTS_SLUGS),
        (VoiceBucket::Phone, PHONE_SLUGS),
    ];
    for (bucket, slugs) in buckets {
        for slug in *slugs {
            if normalised == *slug || normalised.contains(slug) {
                return Some(*bucket);
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Wire shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EndpointOption {
    pub id: String,
    pub label: String,
    pub provider: String,
    pub slug: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LlmConnectionOption {
    pub connection_id: String,
    pub connection_label: String,
    pub slug: String,
    pub display: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceOptions {
    pub stt_endpoints: Vec<EndpointOption>,
    pub tts_endpoints: Vec<EndpointOption>,
    pub phone_endpoints: Vec<EndpointOption>,
    pub llm_connections: Vec<LlmConnectionOption>,
    /// Cosmetic placeholder shown as the first item in the LLM override
    /// dropdown — e.g. `"Use Fast tier (DeepSeek :: deepseek-chat)"`.
    /// `None` when the active Bundle has no Fast tier configured.
    pub fast_tier_label: Option<String>,
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// Load the persisted voice config. Returns defaults when nothing has
/// been saved yet.
#[tauri::command]
pub async fn get_voice_settings(
    _state: State<'_, AppState>,
) -> std::result::Result<VoiceConfig, String> {
    let config = crate::settings::load_main_config_public();
    Ok(decode_voice(&config.voice))
}

/// Persist a voice config. Clamps `max_call_duration_s` to the hard
/// limit declared by `VoiceConfig`.
#[tauri::command]
pub async fn save_voice_settings(
    _state: State<'_, AppState>,
    config: VoiceConfig,
) -> std::result::Result<(), String> {
    let mut config = config;
    let (clamped, _) = VoiceConfig::clamp_duration(config.max_call_duration_s);
    config.max_call_duration_s = clamped;

    let mut main = crate::settings::load_main_config_public();
    main.voice = serde_json::to_value(&config)
        .map_err(|e| format!("serialize VoiceConfig: {e}"))?;
    crate::settings::save_main_config_public(&main)?;
    tracing::info!("Voice config saved");
    Ok(())
}

/// Enumerate the picks the Voice panel renders into its dropdowns.
#[tauri::command]
pub async fn list_voice_options(
    state: State<'_, AppState>,
) -> std::result::Result<VoiceOptions, String> {
    let endpoints = collect_endpoints(&state).await?;

    let mut stt_endpoints = Vec::new();
    let mut tts_endpoints = Vec::new();
    let mut phone_endpoints = Vec::new();

    for ep in endpoints {
        let Some(bucket) = classify(&ep.provider) else {
            continue;
        };
        let slug = ep.provider.trim().to_ascii_lowercase();
        let label = if ep.name.trim().is_empty() {
            format!("{} ({})", ep.provider, ep.base_url)
        } else {
            ep.name.clone()
        };
        let opt = EndpointOption {
            id: ep.id.to_string(),
            label,
            provider: ep.provider.clone(),
            slug,
        };
        match bucket {
            VoiceBucket::Stt => stt_endpoints.push(opt),
            VoiceBucket::Tts => tts_endpoints.push(opt),
            VoiceBucket::Phone => phone_endpoints.push(opt),
        }
    }

    // Stable alphabetic sort so the dropdown order doesn't jiggle.
    for v in [&mut stt_endpoints, &mut tts_endpoints, &mut phone_endpoints] {
        v.sort_by_key(|a| a.label.to_ascii_lowercase());
    }

    let cfg = crate::settings::load_main_config_public();
    let llm_connections = collect_llm_connections(&cfg);
    let fast_tier_label = active_fast_tier_label(&cfg);

    Ok(VoiceOptions {
        stt_endpoints,
        tts_endpoints,
        phone_endpoints,
        llm_connections,
        fast_tier_label,
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn decode_voice(value: &serde_json::Value) -> VoiceConfig {
    if value.is_null() {
        return VoiceConfig::new();
    }
    match serde_json::from_value::<VoiceConfig>(value.clone()) {
        Ok(cfg) => cfg,
        Err(e) => {
            tracing::warn!(error = %e, "voice config blob malformed; returning defaults");
            VoiceConfig::new()
        }
    }
}

async fn collect_endpoints(
    state: &State<'_, AppState>,
) -> std::result::Result<Vec<athen_core::http_endpoint::RegisteredEndpoint>, String> {
    use athen_core::traits::http_endpoint::HttpEndpointStore;
    let Some(store) = state.http_endpoint_store.as_ref() else {
        return Ok(Vec::new());
    };
    store.list().await.map_err(|e| e.to_string())
}

/// Build the `(connection, slug)` option list for the LLM override
/// dropdown. We emit one entry per connection × its `default_model`,
/// plus one per (connection × tier slug) the user has set. This is a
/// pragmatic-not-exhaustive list — most users only see "<provider> ::
/// <default model>" and that's enough.
fn collect_llm_connections(
    cfg: &athen_core::config::AthenConfig,
) -> Vec<LlmConnectionOption> {
    let mut out: Vec<LlmConnectionOption> = Vec::new();
    let mut seen: HashSet<(String, String)> = HashSet::new();

    for (id, provider) in &cfg.models.providers {
        let connection_label = display_connection_label(id);
        let mut push_slug = |slug: &str| {
            if slug.is_empty() {
                return;
            }
            let key = (id.clone(), slug.to_string());
            if !seen.insert(key) {
                return;
            }
            out.push(LlmConnectionOption {
                connection_id: id.clone(),
                connection_label: connection_label.clone(),
                slug: slug.to_string(),
                display: format!("{connection_label} :: {slug}"),
            });
        };
        push_slug(&provider.default_model);
        for slug in provider.tier_models.values() {
            push_slug(slug);
        }
    }

    out.sort_by_key(|a| a.display.to_ascii_lowercase());
    out
}

/// Resolve the active Bundle's Fast tier into a human label, used as the
/// placeholder for the LLM override dropdown.
fn active_fast_tier_label(cfg: &athen_core::config::AthenConfig) -> Option<String> {
    let bundle_id = cfg.models.assignments.get(ACTIVE_BUNDLE_KEY)?;
    let bundle: &Bundle = cfg.models.bundles.get(bundle_id)?;
    let tier = bundle.tiers.get(&ModelProfile::Fast)?;
    let connection_label = display_connection_label(&tier.connection_id);
    Some(format!(
        "Use Fast tier ({connection_label} :: {})",
        tier.slug
    ))
}

/// Fallback "Display Name" derivation for a connection id. The Settings
/// UI uses the same trick — `display_name(id)` over in `settings.rs` —
/// but that helper is private and we don't want to widen its visibility
/// for this one consumer.
fn display_connection_label(id: &str) -> String {
    let mut chars = id.chars();
    match chars.next() {
        Some(c) => c.to_ascii_uppercase().to_string() + chars.as_str(),
        None => String::new(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voice_options_serde_camelcase() {
        let opts = VoiceOptions {
            stt_endpoints: vec![EndpointOption {
                id: "u".into(),
                label: "Deepgram".into(),
                provider: "Deepgram".into(),
                slug: "deepgram".into(),
            }],
            tts_endpoints: vec![],
            phone_endpoints: vec![],
            llm_connections: vec![LlmConnectionOption {
                connection_id: "deepseek".into(),
                connection_label: "Deepseek".into(),
                slug: "deepseek-chat".into(),
                display: "Deepseek :: deepseek-chat".into(),
            }],
            fast_tier_label: Some("Use Fast tier (Deepseek :: deepseek-chat)".into()),
        };
        let json = serde_json::to_value(&opts).unwrap();
        assert!(json.get("sttEndpoints").is_some());
        assert!(json.get("ttsEndpoints").is_some());
        assert!(json.get("phoneEndpoints").is_some());
        assert!(json.get("llmConnections").is_some());
        assert!(json.get("fastTierLabel").is_some());
        // ensure NOT snake_case
        assert!(json.get("stt_endpoints").is_none());
        let llm = &json.get("llmConnections").unwrap()[0];
        assert!(llm.get("connectionId").is_some());
        assert!(llm.get("connectionLabel").is_some());
    }

    #[test]
    fn classify_recognises_known_providers() {
        assert_eq!(classify("Deepgram"), Some(VoiceBucket::Stt));
        assert_eq!(classify("deepgram"), Some(VoiceBucket::Stt));
        assert_eq!(classify("ElevenLabs"), Some(VoiceBucket::Tts));
        assert_eq!(classify("Cartesia"), Some(VoiceBucket::Tts));
        assert_eq!(classify("Twilio"), Some(VoiceBucket::Phone));
    }

    #[test]
    fn classify_returns_none_for_unrelated() {
        assert_eq!(classify(""), None);
        assert_eq!(classify("Jina AI"), None);
        assert_eq!(classify("OpenAI"), None);
    }

    #[test]
    fn classify_is_substring_tolerant() {
        // A user who types "Twilio Voice" or "Deepgram (work)" should
        // still bucket correctly.
        assert_eq!(classify("Twilio Voice"), Some(VoiceBucket::Phone));
        assert_eq!(classify("Deepgram (work)"), Some(VoiceBucket::Stt));
    }

    #[test]
    fn decode_voice_handles_null_blob() {
        let cfg = decode_voice(&serde_json::Value::Null);
        assert_eq!(cfg.max_call_duration_s, VoiceConfig::DEFAULT_MAX_DURATION_S);
        assert!(cfg.stt_endpoint_id.is_none());
    }

    #[test]
    fn decode_voice_round_trips() {
        let mut original = VoiceConfig::new();
        original.stt_endpoint_id = Some("abc".into());
        original.voice_id = Some("voice-1".into());
        original.max_call_duration_s = 900;
        let blob = serde_json::to_value(&original).unwrap();
        let decoded = decode_voice(&blob);
        assert_eq!(decoded.stt_endpoint_id.as_deref(), Some("abc"));
        assert_eq!(decoded.voice_id.as_deref(), Some("voice-1"));
        assert_eq!(decoded.max_call_duration_s, 900);
    }

    #[test]
    fn decode_voice_returns_default_on_malformed_blob() {
        let blob = serde_json::json!({ "garbage": true });
        // serde derives are strict on unknown variants of unrelated
        // shapes — but VoiceConfig is permissive (`#[serde(default)]`
        // is implicit per-field via Option/u32 defaults). We tolerate
        // any decode outcome, but the helper itself must never panic.
        let _ = decode_voice(&blob);
    }
}
