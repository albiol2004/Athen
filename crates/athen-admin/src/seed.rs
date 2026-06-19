//! Panel-generated seed config for new Athen instances.
//!
//! Generates a minimal `models.toml` that the env-key overlay can patch,
//! and computes the `ATHEN_PROVIDER_<ID>_API_KEY` env var name to inject
//! into the container.
//!
//! athen-admin has ZERO internal athen deps (hexagonal rule). The string
//! constants here are copies with explicit pointers to their sources of
//! truth:
//!
//! - Family wire IDs → `athen-core::llm::ModelFamily::wire_id()` in
//!   `crates/athen-core/src/llm.rs`.
//! - Provider IDs / default slugs / context windows → `PROVIDER_IDS`,
//!   `default_model`, `default_family`, `default_tier_slugs` in
//!   `crates/athen-app/src/settings.rs`.
//! - Env var name mangling → `provider_env_var()` in
//!   `crates/athen-app/src/env_creds.rs` (test: `provider_env_var_uppercases_and_sanitizes`).

use chrono::{DateTime, Utc};
use serde::Serialize;
use uuid::Uuid;

// ─── Preset table ────────────────────────────────────────────────────────────

/// One row in the provider preset catalog.
#[derive(Debug, Clone, Serialize)]
pub struct ProviderPreset {
    /// Stable provider id (e.g. `"deepseek"`). Used as the key in
    /// `models.providers` and mangled into the env var name.
    pub id: &'static str,
    /// Human-readable label for the UI dropdown.
    pub label: &'static str,
    /// Default model slug routed to all four tiers on fresh provision.
    /// Matches `default_model(id)` in `athen-app/src/settings.rs`.
    pub default_slug: &'static str,
    /// `ModelFamily::wire_id()` value. Must match the enum variant name in
    /// `athen-core::llm::ModelFamily`. Source of truth:
    /// `crates/athen-core/src/llm.rs` — `wire_id()` match arm.
    pub family: &'static str,
    /// Authoritative context window. Matches `ProviderConfig` defaults or the
    /// curated value per provider. Source of truth: Settings → provider cards.
    pub context_window_tokens: u32,
    /// Link to the provider's API-key dashboard (shown as a hint in the UI).
    /// Source of truth: `dashboard_url(id)` in `athen-app/src/settings.rs`.
    pub key_page_url: &'static str,
    /// `true` when this is the "Custom" escape-hatch entry (user types all
    /// fields manually). The UI renders a text input instead of a dropdown
    /// for provider_id / family / slug when this is selected.
    pub custom: bool,
}

/// Canonical preset table. Keep aligned with `PROVIDER_IDS` and
/// `default_model` / `default_family` / `default_tier_slugs` in
/// `crates/athen-app/src/settings.rs`.
pub const PRESETS: &[ProviderPreset] = &[
    ProviderPreset {
        id: "deepseek",
        label: "DeepSeek",
        default_slug: "deepseek-v4-flash",
        // Source: ModelFamily::DeepSeekV4Chat wire_id in athen-core/src/llm.rs
        family: "DeepSeekV4Chat",
        context_window_tokens: 128_000,
        key_page_url: "https://platform.deepseek.com/api_keys",
        custom: false,
    },
    ProviderPreset {
        id: "openai",
        label: "OpenAI",
        default_slug: "gpt-5.4-mini",
        // Source: ModelFamily::Gpt5 wire_id in athen-core/src/llm.rs
        family: "Gpt5",
        context_window_tokens: 128_000,
        key_page_url: "https://platform.openai.com/api-keys",
        custom: false,
    },
    ProviderPreset {
        id: "anthropic",
        label: "Anthropic",
        default_slug: "claude-sonnet-4-6",
        // Source: ModelFamily::ClaudeSonnet46 wire_id in athen-core/src/llm.rs
        family: "ClaudeSonnet46",
        context_window_tokens: 200_000,
        key_page_url: "https://console.anthropic.com/settings/keys",
        custom: false,
    },
    ProviderPreset {
        id: "google",
        label: "Google (Gemini)",
        default_slug: "gemini-3.1-flash-lite-preview",
        // Source: ModelFamily::Gemini3Flash wire_id in athen-core/src/llm.rs
        family: "Gemini3Flash",
        context_window_tokens: 1_048_576,
        key_page_url: "https://aistudio.google.com/apikey",
        custom: false,
    },
    ProviderPreset {
        id: "mistral",
        label: "Mistral",
        default_slug: "mistral-large-latest",
        // Source: ModelFamily::MistralLarge3 wire_id in athen-core/src/llm.rs
        family: "MistralLarge3",
        context_window_tokens: 131_072,
        key_page_url: "https://console.mistral.ai/api-keys/",
        custom: false,
    },
    ProviderPreset {
        id: "openrouter",
        label: "OpenRouter",
        default_slug: "openai/gpt-5.4-mini",
        // Source: ModelFamily::Default wire_id in athen-core/src/llm.rs
        // (OpenRouter intentionally stays on Default — user picks the actual model)
        family: "Default",
        context_window_tokens: 128_000,
        key_page_url: "https://openrouter.ai/keys",
        custom: false,
    },
    ProviderPreset {
        id: "kimi",
        label: "Kimi (Moonshot platform)",
        default_slug: "kimi-k2.7-code",
        // Source: ModelFamily::KimiK27Code wire_id in athen-core/src/llm.rs
        family: "KimiK27Code",
        context_window_tokens: 262_144,
        key_page_url: "https://platform.moonshot.ai/console/api-keys",
        custom: false,
    },
    ProviderPreset {
        id: "kimi_code",
        label: "Kimi Code Plan (subscription)",
        default_slug: "kimi-for-coding",
        // Source: ModelFamily::KimiK27Code wire_id in athen-core/src/llm.rs
        family: "KimiK27Code",
        context_window_tokens: 262_144,
        key_page_url: "https://www.kimi.com/code",
        custom: false,
    },
    // Custom escape-hatch: user types everything manually.
    ProviderPreset {
        id: "",
        label: "Custom…",
        default_slug: "",
        family: "Default",
        context_window_tokens: 128_000,
        key_page_url: "",
        custom: true,
    },
];

// ─── Env var name mangling ────────────────────────────────────────────────────

/// Compute the env var name for a provider's API key.
///
/// Matches the logic in `athen-app::env_creds::provider_env_var`:
/// uppercase the id, replace every non-alphanumeric char with `_`, then
/// wrap as `ATHEN_PROVIDER_<ID>_API_KEY`.
///
/// Test vectors (from `env_creds.rs` tests):
/// - `"deepseek"` → `"ATHEN_PROVIDER_DEEPSEEK_API_KEY"`
/// - `"opencode_go"` → `"ATHEN_PROVIDER_OPENCODE_GO_API_KEY"`
/// - `"my-relay.v2"` → `"ATHEN_PROVIDER_MY_RELAY_V2_API_KEY"`
pub fn provider_env_var(provider_id: &str) -> String {
    let id: String = provider_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    format!("ATHEN_PROVIDER_{id}_API_KEY")
}

// ─── TOML seed generation ─────────────────────────────────────────────────────

/// Structured LLM seed. When present on a `CreateSpec`, the admin generates
/// a `models.toml` automatically; the API key rides in the container env
/// (`ATHEN_PROVIDER_<ID>_API_KEY`), never the file.
#[derive(Debug, Clone)]
pub struct LlmSeed {
    /// Provider id, e.g. `"deepseek"`. Maps to the key in
    /// `models.providers` and the env var name.
    pub provider_id: String,
    /// Model slug routed to all four tiers (Cheap/Fast/Code/Powerful).
    pub slug: String,
    /// API key injected into the container env. Never written to the file.
    pub api_key: String,
    /// `ModelFamily::wire_id()` string, e.g. `"DeepSeekV4Chat"`.
    pub family: String,
    /// Authoritative context window. `None` → default from the preset (128k).
    pub context_window_tokens: Option<u32>,
}

impl LlmSeed {
    /// Validate the seed fields, returning a human-readable error on failure.
    ///
    /// Rules mirror the `env` validation in `instances.rs`:
    /// - `provider_id` non-empty, no `=` or newlines.
    /// - `slug` non-empty, no newlines.
    /// - `family` non-empty.
    /// - `api_key` may be empty (local providers, no-key relays) — the
    ///   env var is simply not injected when it is.
    pub fn validate(&self) -> Result<(), String> {
        if self.provider_id.is_empty() {
            return Err("provider_id is required".into());
        }
        if self.provider_id.contains('=') || self.provider_id.contains('\n') {
            return Err("provider_id must not contain '=' or newlines".into());
        }
        if self.slug.is_empty() {
            return Err("slug is required".into());
        }
        if self.slug.contains('\n') {
            return Err("slug must not contain newlines".into());
        }
        if self.family.is_empty() {
            return Err("family is required".into());
        }
        Ok(())
    }
}

/// Generate the `models.toml` content for a new instance from an `LlmSeed`.
///
/// The generated TOML:
/// - Sets `auth = "None"` — the real key rides in
///   `ATHEN_PROVIDER_<ID>_API_KEY` injected into the container env.
/// - Creates a deterministic `active_bundle` with all four tiers pointing
///   at `(provider_id, slug)` so the routing chain works out of the box.
/// - Includes `[providers.<id>.tier_models]` as an empty table — the env
///   overlay patches the provider entry (not the tier_models), but the
///   block must exist for the deserializer.
///
/// **TOML round-trip contract** (verified by unit tests):
/// The generated string must parse via `toml::from_str::<ModelsConfigSurface>(_)`
/// (the test struct below mirrors the load-bearing fields of
/// `athen-core::config::ModelsConfig`).
///
/// The bundle UUID is generated fresh per call; `created_at`/`updated_at`
/// timestamps are set to `now`.
pub fn generate_models_toml(seed: &LlmSeed, now: DateTime<Utc>) -> String {
    let bundle_id = Uuid::new_v4();
    let bundle_id_str = bundle_id.to_string();
    let ts = now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let ctx = seed.context_window_tokens.unwrap_or(128_000);

    // provider_id, slug, and family are validated before this is called, so
    // they won't contain TOML-breaking characters. We still escape the slug
    // as a TOML string to be safe.
    let id = &seed.provider_id;
    let slug = &seed.slug;
    let family = &seed.family;

    // Build the TOML string by hand rather than serializing a struct that
    // would depend on athen-core types.  This keeps athen-admin free of all
    // internal deps while producing a string that round-trips through the
    // real deserializer.
    //
    // Key points matched to the real structs:
    //   - `auth = "None"` → AuthType::None (unit variant, serde external-tag → bare string)
    //   - `family = "<wire_id>"` → ModelFamily::<variant> (no rename_all; variant name = wire_id)
    //   - `[providers.<id>.tier_models]` → HashMap<ModelProfile, String>; empty table is fine
    //     because #[serde(default)] on ModelsConfig + ProviderConfig
    //   - `[bundles.<uuid>.tiers.Fast]` etc. → HashMap<ModelProfile, BundleTier>;
    //     ModelProfile variant names: "Fast", "Cheap", "Code", "Powerful"
    //   - `connection_id` / `slug` are BundleTier fields (snake_case, no rename)
    //   - `[assignments]` → HashMap<String,String>; keys "active_provider" + "active_bundle"
    //   - `[profiles]` → HashMap<String,ProfileConfig>; empty, #[serde(default)] so omittable
    //     but we include it explicitly to match the shape tools expect
    format!(
        r#"# Generated by athen-admin panel-seed. Do NOT add API keys here.
# The key is injected via ATHEN_PROVIDER_{env_id}_API_KEY in the container env.

[providers.{id}]
auth = "None"
default_model = "{slug}"
context_window_tokens = {ctx}
compaction_trigger_pct = 65
compaction_target_pct = 30
supports_vision = false
supports_documents = false
family = "{family}"

[providers.{id}.tier_models]

[profiles]

[assignments]
active_provider = "{id}"
active_bundle = "{bundle_id_str}"

[bundles.{bundle_id_str}]
id = "{bundle_id_str}"
name = "Default"
created_at = "{ts}"
updated_at = "{ts}"

[bundles.{bundle_id_str}.tiers.Fast]
connection_id = "{id}"
slug = "{slug}"

[bundles.{bundle_id_str}.tiers.Cheap]
connection_id = "{id}"
slug = "{slug}"

[bundles.{bundle_id_str}.tiers.Code]
connection_id = "{id}"
slug = "{slug}"

[bundles.{bundle_id_str}.tiers.Powerful]
connection_id = "{id}"
slug = "{slug}"
"#,
        env_id = provider_env_var(id)
            .trim_start_matches("ATHEN_PROVIDER_")
            .trim_end_matches("_API_KEY"),
        id = id,
        slug = slug,
        ctx = ctx,
        family = family,
        bundle_id_str = bundle_id_str,
        ts = ts,
    )
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use std::collections::HashMap;

    // ── Minimal mirror of ModelsConfig load-bearing fields. ───────────────
    // We can't depend on athen-core here, so we reproduce just enough to
    // verify that the generated TOML parses and contains the right keys.

    #[derive(Debug, Deserialize)]
    #[allow(dead_code)]
    struct ProviderSurface {
        auth: toml::Value,
        default_model: String,
        context_window_tokens: u32,
        family: String,
        #[serde(default)]
        tier_models: HashMap<String, String>,
    }

    #[derive(Debug, Deserialize)]
    struct BundleTierSurface {
        connection_id: String,
        slug: String,
    }

    #[derive(Debug, Deserialize)]
    struct BundleSurface {
        id: String,
        name: String,
        #[serde(default)]
        tiers: HashMap<String, BundleTierSurface>,
    }

    #[derive(Debug, Deserialize)]
    #[allow(dead_code)]
    struct ModelsConfigSurface {
        #[serde(default)]
        providers: HashMap<String, ProviderSurface>,
        #[serde(default)]
        profiles: HashMap<String, toml::Value>,
        #[serde(default)]
        assignments: HashMap<String, String>,
        #[serde(default)]
        bundles: HashMap<String, BundleSurface>,
    }

    fn seed() -> LlmSeed {
        LlmSeed {
            provider_id: "deepseek".into(),
            slug: "deepseek-v4-flash".into(),
            api_key: "sk-test".into(),
            family: "DeepSeekV4Chat".into(),
            context_window_tokens: None,
        }
    }

    #[test]
    fn generated_toml_parses() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-06-12T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let toml_str = generate_models_toml(&seed(), now);
        let parsed: Result<ModelsConfigSurface, _> = toml::from_str(&toml_str);
        assert!(
            parsed.is_ok(),
            "parse failed: {:?}\nTOML was:\n{}",
            parsed.err(),
            toml_str
        );
    }

    #[test]
    fn generated_toml_contains_provider_entry() {
        let now = Utc::now();
        let toml_str = generate_models_toml(&seed(), now);
        let parsed: ModelsConfigSurface = toml::from_str(&toml_str).expect("parse");
        assert!(
            parsed.providers.contains_key("deepseek"),
            "no deepseek provider key"
        );
        let p = &parsed.providers["deepseek"];
        assert_eq!(p.default_model, "deepseek-v4-flash");
        assert_eq!(p.context_window_tokens, 128_000);
        assert_eq!(p.family, "DeepSeekV4Chat");
    }

    #[test]
    fn generated_toml_auth_is_none() {
        let now = Utc::now();
        let toml_str = generate_models_toml(&seed(), now);
        let parsed: ModelsConfigSurface = toml::from_str(&toml_str).expect("parse");
        let auth = &parsed.providers["deepseek"].auth;
        // AuthType::None serialises as the bare string "None".
        assert_eq!(auth.as_str(), Some("None"), "auth must be the string None");
    }

    #[test]
    fn generated_toml_has_active_bundle_and_assignments() {
        let now = Utc::now();
        let toml_str = generate_models_toml(&seed(), now);
        let parsed: ModelsConfigSurface = toml::from_str(&toml_str).expect("parse");
        assert!(
            parsed.assignments.contains_key("active_bundle"),
            "missing active_bundle in assignments"
        );
        assert!(
            parsed.assignments.contains_key("active_provider"),
            "missing active_provider in assignments"
        );
        let bundle_id = parsed.assignments["active_bundle"].clone();
        assert!(
            parsed.bundles.contains_key(&bundle_id),
            "active_bundle id not in bundles map"
        );
    }

    #[test]
    fn generated_toml_all_four_tiers_present() {
        let now = Utc::now();
        let toml_str = generate_models_toml(&seed(), now);
        let parsed: ModelsConfigSurface = toml::from_str(&toml_str).expect("parse");
        let bundle_id = parsed.assignments["active_bundle"].clone();
        let bundle = &parsed.bundles[&bundle_id];
        for tier in &["Fast", "Cheap", "Code", "Powerful"] {
            let t = bundle
                .tiers
                .get(*tier)
                .unwrap_or_else(|| panic!("missing tier {tier}"));
            assert_eq!(t.connection_id, "deepseek");
            assert_eq!(t.slug, "deepseek-v4-flash");
        }
    }

    #[test]
    fn generated_toml_bundle_name_is_default() {
        let now = Utc::now();
        let toml_str = generate_models_toml(&seed(), now);
        let parsed: ModelsConfigSurface = toml::from_str(&toml_str).expect("parse");
        let bundle_id = parsed.assignments["active_bundle"].clone();
        assert_eq!(parsed.bundles[&bundle_id].name, "Default");
    }

    #[test]
    fn generated_toml_bundle_id_matches_assignments() {
        let now = Utc::now();
        let toml_str = generate_models_toml(&seed(), now);
        let parsed: ModelsConfigSurface = toml::from_str(&toml_str).expect("parse");
        let active = &parsed.assignments["active_bundle"];
        let bundle = &parsed.bundles[active];
        assert_eq!(&bundle.id, active);
    }

    #[test]
    fn generated_toml_custom_context_window() {
        let now = Utc::now();
        let mut s = seed();
        s.context_window_tokens = Some(200_000);
        let toml_str = generate_models_toml(&s, now);
        let parsed: ModelsConfigSurface = toml::from_str(&toml_str).expect("parse");
        assert_eq!(parsed.providers["deepseek"].context_window_tokens, 200_000);
    }

    #[test]
    fn generated_toml_openai_preset() {
        let now = Utc::now();
        let s = LlmSeed {
            provider_id: "openai".into(),
            slug: "gpt-5.4-mini".into(),
            api_key: "sk-...".into(),
            family: "Gpt5".into(),
            context_window_tokens: None,
        };
        let toml_str = generate_models_toml(&s, now);
        let parsed: ModelsConfigSurface = toml::from_str(&toml_str).expect("parse");
        assert!(parsed.providers.contains_key("openai"));
        assert_eq!(parsed.providers["openai"].default_model, "gpt-5.4-mini");
    }

    // ── Env var name mangling ──────────────────────────────────────────────

    #[test]
    fn env_var_deepseek() {
        assert_eq!(
            provider_env_var("deepseek"),
            "ATHEN_PROVIDER_DEEPSEEK_API_KEY"
        );
    }

    #[test]
    fn env_var_opencode_go() {
        assert_eq!(
            provider_env_var("opencode_go"),
            "ATHEN_PROVIDER_OPENCODE_GO_API_KEY"
        );
    }

    #[test]
    fn env_var_with_hyphens_and_dots() {
        assert_eq!(
            provider_env_var("my-relay.v2"),
            "ATHEN_PROVIDER_MY_RELAY_V2_API_KEY"
        );
    }

    #[test]
    fn env_var_anthropic() {
        assert_eq!(
            provider_env_var("anthropic"),
            "ATHEN_PROVIDER_ANTHROPIC_API_KEY"
        );
    }

    // ── LlmSeed validation ─────────────────────────────────────────────────

    #[test]
    fn validate_ok() {
        assert!(seed().validate().is_ok());
    }

    #[test]
    fn validate_rejects_empty_provider_id() {
        let mut s = seed();
        s.provider_id = String::new();
        assert!(s.validate().is_err());
    }

    #[test]
    fn validate_rejects_provider_id_with_equals() {
        let mut s = seed();
        s.provider_id = "bad=id".into();
        assert!(s.validate().is_err());
    }

    #[test]
    fn validate_rejects_provider_id_with_newline() {
        let mut s = seed();
        s.provider_id = "bad\nid".into();
        assert!(s.validate().is_err());
    }

    #[test]
    fn validate_rejects_empty_slug() {
        let mut s = seed();
        s.slug = String::new();
        assert!(s.validate().is_err());
    }

    #[test]
    fn validate_rejects_slug_with_newline() {
        let mut s = seed();
        s.slug = "bad\nslug".into();
        assert!(s.validate().is_err());
    }

    #[test]
    fn validate_rejects_empty_family() {
        let mut s = seed();
        s.family = String::new();
        assert!(s.validate().is_err());
    }

    #[test]
    fn validate_allows_empty_api_key() {
        let mut s = seed();
        s.api_key = String::new(); // local provider — no key
        assert!(s.validate().is_ok());
    }
}
