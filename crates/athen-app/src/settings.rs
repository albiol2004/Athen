//! Settings management commands for the Tauri frontend.
//!
//! Provides Tauri IPC commands for managing LLM providers, API keys,
//! and general application settings through the UI.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde::Serialize;
use tauri::State;
use tracing::{info, warn};

use athen_core::config::{
    AthenConfig, AuthType, EmbeddingMode, ModelsConfig, NotificationChannelKind,
    NotificationConfig, ProviderConfig, QuietHours, SecurityMode,
};
use athen_core::config_loader;

use crate::state::{build_router_for_provider, AppState};

// ---------------------------------------------------------------------------
// Owner-identifier disjointness helpers
// ---------------------------------------------------------------------------

/// Snapshot the owner contact's identifiers and check that none of the
/// `candidates` (scheme, value) pairs overlap. Returns a human-friendly
/// error string the frontend renders verbatim under the form when a
/// conflict is found.
///
/// Used by the email + Telegram settings-save commands to prevent the
/// user from assigning an identifier that the owner contact already
/// owns — which would let an unauthenticated sender masquerade as the
/// owner (Phase 3 of the owner-identity unification).
///
/// When the owner contact has no identifiers (no owner configured yet
/// in a fresh install), this is a trivial `Ok(())` — there's nothing
/// to conflict with.
async fn validate_disjoint_from_owner(
    owner_lookup: &athen_contacts::OwnerLookup,
    candidates: &[(String, String)],
) -> std::result::Result<(), String> {
    let owner_idents = owner_lookup.owner_identifiers().await;
    if owner_idents.is_empty() || candidates.is_empty() {
        return Ok(());
    }
    match athen_contacts::assert_disjoint_from_owner(&owner_idents, candidates) {
        Ok(()) => Ok(()),
        Err(conflicts) => {
            // Render every conflict so the user fixes them all at once
            // rather than discovering them one at a time on resubmit.
            let parts: Vec<String> = conflicts
                .into_iter()
                .map(|(scheme, value)| format!("{scheme}={value}"))
                .collect();
            Err(format!(
                "Conflicts with owner contact: {} (cannot be both you and Athen's identity)",
                parts.join(", ")
            ))
        }
    }
}

/// Extract the bot's own numeric Telegram user id from a bot token of
/// the form `<digits>:<base64ish>`. Used to catch the rare misconfig
/// where the user pastes a bot token whose prefix matches the owner's
/// own Telegram id. Returns `None` for malformed tokens — the caller
/// then just skips the bot-id leg of the validation.
pub(crate) fn bot_user_id_from_token(token: &str) -> Option<String> {
    let (prefix, _) = token.split_once(':')?;
    let n: i64 = prefix.parse().ok()?;
    Some(n.to_string())
}

// ---------------------------------------------------------------------------
// Response types
// ---------------------------------------------------------------------------

/// Information about a configured LLM provider.
#[derive(Debug, Clone, Serialize)]
pub struct ProviderInfo {
    pub id: String,
    pub name: String,
    pub provider_type: String,
    pub base_url: String,
    pub model: String,
    pub has_api_key: bool,
    pub api_key_hint: String,
    pub is_active: bool,
    /// Whether the configured `default_model` accepts image input.
    pub supports_vision: bool,
    /// Whether the configured `default_model` accepts native PDF/document
    /// input. Independent of `supports_vision`.
    pub supports_documents: bool,
    /// User-selected model family (e.g. "Qwen35Local"). Drives the
    /// per-model quirks system. `"Default"` for unprofiled providers.
    pub family: String,
    /// Authoritative context-window ceiling used by the arc compactor.
    pub context_window_tokens: u32,
    /// Compact when arc tokens exceed this percentage of `context_window_tokens`.
    pub compaction_trigger_pct: u8,
    /// Compaction target as a percentage of `context_window_tokens`.
    pub compaction_target_pct: u8,
    /// Sampling temperature override. `None` lets the provider adapter pick
    /// its baked-in default.
    pub temperature: Option<f32>,
    /// Per-tier model slugs. Wire-string keys ("Cheap" | "Fast" | "Code" |
    /// "Powerful"), values are model slugs. Missing keys or empty strings
    /// mean "use `model` (the default)". The frontend renders one input
    /// per tier so the user can edit them.
    pub tier_models: HashMap<String, String>,
}

/// Email configuration info for the frontend.
#[derive(Debug, Clone, Serialize)]
pub struct EmailSettingsInfo {
    pub enabled: bool,
    pub imap_server: String,
    pub imap_port: u16,
    pub username: String,
    pub has_password: bool,
    pub use_tls: bool,
    pub folders: String,
    pub poll_interval_secs: u64,
    pub lookback_hours: u32,
    pub smtp_server: String,
    pub smtp_port: u16,
    pub smtp_username: String,
    pub has_smtp_password: bool,
    pub smtp_use_tls: bool,
    pub from_address: String,
}

/// Telegram bot configuration info for the frontend.
#[derive(Debug, Clone, Serialize)]
pub struct TelegramSettingsInfo {
    pub enabled: bool,
    pub has_bot_token: bool,
    pub bot_token_hint: String,
    /// The actual bot token so the frontend can re-populate the field.
    pub bot_token: String,
    pub allowed_chat_ids: Vec<i64>,
    pub poll_interval_secs: u64,
}

/// Notification delivery configuration info for the frontend.
#[derive(Debug, Clone, Serialize)]
pub struct NotificationSettingsInfo {
    pub preferred_channels: Vec<String>,
    pub escalation_timeout_secs: u64,
    pub quiet_hours_enabled: bool,
    pub quiet_start_hour: u32,
    pub quiet_start_minute: u32,
    pub quiet_end_hour: u32,
    pub quiet_end_minute: u32,
    pub quiet_allow_critical: bool,
}

/// Web search provider configuration info for the frontend. Keys are not
/// exposed verbatim — the frontend gets a boolean + masked hint so a
/// settings refresh on a shared screen doesn't leak the credential.
#[derive(Debug, Clone, Serialize)]
pub struct WebSearchSettingsInfo {
    pub brave_configured: bool,
    pub brave_hint: String,
    pub tavily_configured: bool,
    pub tavily_hint: String,
}

/// Embedding provider configuration info for the frontend.
#[derive(Debug, Clone, Serialize)]
pub struct EmbeddingSettingsInfo {
    pub mode: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub has_api_key: bool,
    pub api_key_hint: Option<String>,
}

/// Full settings response for the frontend.
#[derive(Debug, Clone, Serialize)]
pub struct SettingsResponse {
    pub providers: Vec<ProviderInfo>,
    /// Legacy field — points at the Connection used by the active
    /// Bundle's Fast tier (or Cheap if Fast is empty). Vision-check and
    /// other single-provider callers still read this. Real activation
    /// truth lives in `active_bundle_id`.
    pub active_provider: String,
    /// Stringified UUID of the currently active Bundle, or empty when
    /// no Bundle is set (first-run before migration / wizard write).
    pub active_bundle_id: String,
    /// Every Bundle the user has, with `is_active` flagging the live
    /// one. Ordered alphabetically by name.
    pub bundles: Vec<crate::bundle_settings::BundleView>,
    pub security_mode: String,
    pub email: EmailSettingsInfo,
    pub telegram: TelegramSettingsInfo,
    pub notifications: NotificationSettingsInfo,
    pub embeddings: EmbeddingSettingsInfo,
    pub web_search: WebSearchSettingsInfo,
}

/// Result of a provider connection test.
#[derive(Debug, Clone, Serialize)]
pub struct TestResult {
    pub success: bool,
    pub message: String,
}

// ---------------------------------------------------------------------------
// Helper: config directory
// ---------------------------------------------------------------------------

/// Resolve Athen's per-user data directory, creating it if needed.
///
/// Path is platform-aware via [`athen_core::paths::athen_data_dir`]:
/// - Unix: `~/.athen`
/// - Windows: `%APPDATA%\Athen`
fn ensure_athen_dir() -> Result<PathBuf, String> {
    let dir = athen_core::paths::athen_data_dir()
        .ok_or_else(|| "Cannot resolve Athen data directory (no home).".to_string())?;
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("Failed to create {}: {e}", dir.display()))?;
    Ok(dir)
}

/// Load the current models config from `~/.athen/models.toml`, or return
/// an empty `ModelsConfig` if the file does not exist.
pub(crate) fn load_models_config() -> ModelsConfig {
    if let Ok(dir) = ensure_athen_dir() {
        let path = dir.join("models.toml");
        if path.exists() {
            match std::fs::read_to_string(&path) {
                Ok(content) => match toml::from_str::<ModelsConfig>(&content) {
                    Ok(mut cfg) => {
                        // First-load Bundles migration: if the user has a
                        // legacy active_provider + tier_models shape but
                        // no bundles yet, synthesise the Default Bundle
                        // and persist it back so the resolver and UI both
                        // see the same state on the next read.
                        if let Some(id) = cfg.synthesize_default_bundle_if_empty() {
                            info!(
                                "Synthesised Default Bundle {} from legacy active_provider",
                                id
                            );
                            if let Err(e) = save_models_config(&cfg) {
                                warn!("Failed to persist synthesised Default Bundle: {e}");
                            }
                        }
                        return cfg;
                    }
                    Err(e) => warn!("Failed to parse models.toml: {e}"),
                },
                Err(e) => warn!("Failed to read models.toml: {e}"),
            }
        }
    }
    ModelsConfig::default()
}

/// Save the models config to `~/.athen/models.toml`.
pub(crate) fn save_models_config(config: &ModelsConfig) -> Result<(), String> {
    let dir = ensure_athen_dir()?;
    let path = dir.join("models.toml");
    let content = toml::to_string_pretty(config)
        .map_err(|e| format!("Failed to serialize models config: {e}"))?;
    std::fs::write(&path, content).map_err(|e| format!("Failed to write models.toml: {e}"))?;
    info!("Saved models config to {}", path.display());
    Ok(())
}

/// Module-friendly wrapper so other crates inside `athen-app` (notably
/// `state::start_attachment_purger`) can read persisted settings without
/// duplicating the load path.
pub(crate) fn load_main_config_public() -> AthenConfig {
    load_main_config()
}

/// Load the main config from `~/.athen/config.toml`, or return defaults.
fn load_main_config() -> AthenConfig {
    if let Ok(dir) = ensure_athen_dir() {
        let path = dir.join("config.toml");
        if path.exists() {
            match config_loader::load_config(&path) {
                Ok(cfg) => return cfg,
                Err(e) => warn!("Failed to load config.toml: {e}"),
            }
        }
    }
    AthenConfig::default()
}

/// Load the main config and hydrate every credential field from the vault.
///
/// Use this in Tauri commands that consume credentials AFTER startup —
/// `load_main_config()` returns the raw on-disk view, in which migrated
/// secrets sit as empty strings. The startup-time hydrate in
/// `AppState::new` only repopulates the in-memory config used to build
/// the background senders; later disk reloads need this helper to see
/// the real values.
pub(crate) async fn load_main_config_hydrated(
    vault: Option<&std::sync::Arc<dyn athen_core::traits::vault::Vault>>,
) -> AthenConfig {
    let mut config = load_main_config();
    crate::vault_creds::hydrate_secrets_from_vault(vault, &mut config).await;
    config
}

/// Load `models.toml` and hydrate each provider's api_key from the vault.
///
/// Same rationale as [`load_main_config_hydrated`], scoped to the
/// per-provider api_keys that `save_provider` blanks on disk.
pub(crate) async fn load_models_config_hydrated(
    vault: Option<&std::sync::Arc<dyn athen_core::traits::vault::Vault>>,
) -> ModelsConfig {
    let mut models = load_models_config();
    crate::vault_creds::hydrate_models_from_vault(vault, &mut models).await;
    models
}

/// Save the main config to `~/.athen/config.toml`.
fn save_main_config(config: &AthenConfig) -> Result<(), String> {
    let dir = ensure_athen_dir()?;
    let path = dir.join("config.toml");
    let content =
        toml::to_string_pretty(config).map_err(|e| format!("Failed to serialize config: {e}"))?;
    std::fs::write(&path, content).map_err(|e| format!("Failed to write config.toml: {e}"))?;
    info!("Saved config to {}", path.display());
    Ok(())
}

// ---------------------------------------------------------------------------
// Onboarding (first-launch detection)
// ---------------------------------------------------------------------------
//
// We treat onboarding-completion conservatively: any sign of an existing
// installation suppresses the wizard. The canonical "done" signal is the
// `.onboarded` sentinel file in `~/.athen/`. Once it exists we never show
// onboarding again. Before it exists, we still suppress the wizard if the
// user has a `models.toml` with at least one provider configured, or a
// `config.toml` from any prior install — this protects users who upgraded
// from a version that predates the sentinel.

/// Sentinel file name marking onboarding as completed.
const ONBOARDED_SENTINEL: &str = ".onboarded";

/// Pure predicate against an explicit Athen directory. Returns `true` only
/// when we are confident this is a fresh install. All ambiguous states (I/O
/// errors, malformed config, partial install) resolve to `false` so we
/// never accidentally re-run onboarding for a returning user.
fn is_first_launch_in(athen_dir: &std::path::Path) -> bool {
    // Sentinel takes priority — if onboarding was ever completed, never again.
    if athen_dir.join(ONBOARDED_SENTINEL).exists() {
        return false;
    }

    // Pre-sentinel returning users: any models.toml with at least one
    // provider means they're already set up.
    let models_path = athen_dir.join("models.toml");
    if models_path.exists() {
        match std::fs::read_to_string(&models_path) {
            Ok(content) => match toml::from_str::<ModelsConfig>(&content) {
                Ok(cfg) if !cfg.providers.is_empty() => return false,
                Ok(_) => {
                    // Empty providers map — could be a stub from a wizard
                    // that bailed midway. Be conservative: don't onboard.
                    return false;
                }
                Err(_) => {
                    // Parse error — file exists but unreadable. Assume
                    // returning user; better to make them visit Settings
                    // than to overwrite their broken config.
                    return false;
                }
            },
            Err(_) => return false,
        }
    }

    // Any pre-existing main config also signals a returning user. Their
    // models config might just live elsewhere (env vars, custom path).
    if athen_dir.join("config.toml").exists() {
        return false;
    }

    true
}

/// Write the onboarding sentinel. Idempotent: re-calling on an existing
/// sentinel is a no-op (not an error).
fn mark_onboarded_in(athen_dir: &std::path::Path) -> Result<(), String> {
    std::fs::create_dir_all(athen_dir)
        .map_err(|e| format!("Failed to create {}: {e}", athen_dir.display()))?;
    let sentinel = athen_dir.join(ONBOARDED_SENTINEL);
    if sentinel.exists() {
        return Ok(());
    }
    std::fs::write(&sentinel, b"")
        .map_err(|e| format!("Failed to write onboarding sentinel: {e}"))?;
    info!("Marked onboarding complete: {}", sentinel.display());
    Ok(())
}

/// Returns `true` when the desktop app should show the onboarding wizard.
/// Conservative: any ambiguity resolves to `false`.
#[tauri::command]
pub async fn is_first_launch() -> std::result::Result<bool, String> {
    // If we can't even resolve `~/.athen/`, assume we're not on a fresh
    // install — better to silently fall through to the main UI than to
    // pop a wizard from a broken state.
    let dir = match ensure_athen_dir() {
        Ok(d) => d,
        Err(e) => {
            warn!("is_first_launch: cannot resolve athen dir, treating as returning: {e}");
            return Ok(false);
        }
    };
    Ok(is_first_launch_in(&dir))
}

/// Mark onboarding as complete. Should be called by the frontend wizard's
/// "Done" handler after the user has either configured a provider or
/// explicitly chosen to skip. Idempotent.
#[tauri::command]
pub async fn complete_onboarding() -> std::result::Result<(), String> {
    let dir = ensure_athen_dir()?;
    mark_onboarded_in(&dir)
}

// ---------------------------------------------------------------------------
// Device capability detection
// ---------------------------------------------------------------------------
//
// Used by the onboarding wizard to recommend a sensible embedding tier
// without requiring the user to know what their machine can handle. We
// inspect total RAM and logical core count and bucket the result into one
// of three tiers. Conservative on purpose — when unsure, recommend the
// lighter option so the user isn't fighting OOM kills on first run.

/// Capability snapshot returned to the frontend.
#[derive(Serialize)]
pub struct DeviceCapabilities {
    /// Total system RAM, in gigabytes (rounded down).
    pub total_ram_gb: u64,
    /// Logical CPU cores.
    pub cpu_cores: usize,
    /// OS family: "linux", "macos", "windows", or "other".
    pub os: &'static str,
    /// Recommended embedding tier: "standard" | "small" | "skip".
    /// - `standard`: 16 GB+ RAM, 4+ cores. Local 80–250 MB models are fine.
    /// - `small`:    8–16 GB RAM. Stick to ~80 MB MiniLM-class models.
    /// - `skip`:     <8 GB RAM. Recommend cloud or keyword fallback.
    pub recommended_tier: &'static str,
    /// Short human-readable explanation for the chosen tier.
    pub tier_reason: String,
}

/// Probe the host machine and return a capability snapshot. Cheap — only
/// reads RAM/CPU info via `sysinfo`, no network or disk scans.
#[tauri::command]
pub async fn detect_device_capabilities() -> std::result::Result<DeviceCapabilities, String> {
    use sysinfo::System;

    let mut sys = System::new();
    sys.refresh_memory();

    let total_ram_gb = sys.total_memory() / (1024 * 1024 * 1024);
    let cpu_cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);

    let os = if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        "other"
    };

    let (recommended_tier, tier_reason) = if total_ram_gb >= 16 && cpu_cores >= 4 {
        (
            "standard",
            format!("{total_ram_gb} GB RAM and {cpu_cores} cores — comfortable for a local embedding model."),
        )
    } else if total_ram_gb >= 8 {
        (
            "small",
            format!("{total_ram_gb} GB RAM — a small embedding model (~80 MB) will run fine."),
        )
    } else {
        (
            "skip",
            format!("Only {total_ram_gb} GB RAM detected — recommend cloud embeddings or the keyword fallback."),
        )
    };

    Ok(DeviceCapabilities {
        total_ram_gb,
        cpu_cores,
        os,
        recommended_tier,
        tier_reason,
    })
}

// ---------------------------------------------------------------------------
// Helpers for provider info
// ---------------------------------------------------------------------------

/// Default base URL for a provider ID.
pub(crate) fn default_base_url(id: &str) -> &str {
    match id {
        "deepseek" => "https://api.deepseek.com",
        "openai" => "https://api.openai.com",
        "anthropic" => "https://api.anthropic.com",
        "google" => "https://generativelanguage.googleapis.com",
        "mistral" => "https://api.mistral.ai",
        "openrouter" => "https://openrouter.ai/api",
        // Single logical entry for the OpenCode Go relay. Both the OpenAI-
        // compat `/v1/chat/completions` wire and the Anthropic `/v1/messages`
        // wire live on the same host; the in-process router dispatches
        // per-slug, so users no longer have to pick a wire format.
        "opencode_go" => "https://opencode.ai/zen/go",
        "minimax" => "https://api.minimax.io",
        "minimax_anthropic" => "https://api.minimax.io/anthropic",
        "ollama" => "http://localhost:11434",
        "llamacpp" => "http://localhost:8080",
        _ => "",
    }
}

/// Default model for a provider ID.
pub(crate) fn default_model(id: &str) -> &str {
    match id {
        // Refreshed 2026-05-13: DeepSeek slugs migrated from `deepseek-chat`
        // / `deepseek-reasoner` to the V4 Flash/Pro split. Anthropic Sonnet
        // dropped the date suffix in the 4.6 generation. OpenAI rolled the
        // GPT-5.x line. Gemini moved from `3-flash-preview` to
        // `3.1-flash-lite-preview` / `3.1-pro-preview` as the live slugs.
        "deepseek" => "deepseek-v4-flash",
        "openai" => "gpt-5.4-mini",
        "anthropic" => "claude-sonnet-4-6",
        "google" => "gemini-3.1-flash-lite-preview",
        "mistral" => "mistral-large-latest",
        "openrouter" => "openai/gpt-5.4-mini",
        // OpenCode Go: V4 Flash has the most generous 5h quota (31.6K req)
        // on the $10 tier — the right default for an agent loop. Pro/Kimi/
        // Qwen are selectable from the model picker.
        "opencode_go" => "deepseek-v4-flash",
        // MiniMax Token Plan: M2.7 is the newest flagship; Starter tier
        // exposes only this model, higher tiers add the full lineup.
        "minimax" | "minimax_anthropic" => "minimax-m2.7",
        "ollama" => "llama3",
        "llamacpp" => "default",
        _ => "",
    }
}

/// Display name for a provider ID.
fn display_name(id: &str) -> &str {
    match id {
        "deepseek" => "DeepSeek",
        "openai" => "OpenAI",
        "anthropic" => "Anthropic",
        "google" => "Google (Gemini)",
        "mistral" => "Mistral",
        "openrouter" => "OpenRouter",
        "opencode_go" => "OpenCode Go (DeepSeek / Kimi / Qwen / MiniMax / GLM / MiMo)",
        "minimax" => "MiniMax Token Plan (OpenAI-compat)",
        "minimax_anthropic" => "MiniMax Token Plan (Anthropic-compat + prompt cache)",
        "ollama" => "Ollama",
        "llamacpp" => "llama.cpp",
        _ => id,
    }
}

/// Whether a provider is cloud-based (needs API key) or local.
fn provider_type(id: &str) -> &str {
    match id {
        "ollama" | "llamacpp" => "local",
        _ => "cloud",
    }
}

/// Default ModelFamily wire id for a provider. The frontend uses this to
/// pre-select the family dropdown when the user adds a fresh provider via
/// the "+ Add Provider" chip — and the family's change handler then auto-
/// fills the model slug, giving a one-click "matches the default model"
/// UX. Empty string falls through to `Default` (the safety-net family).
///
/// Local providers (Ollama / llama.cpp) and OpenRouter intentionally
/// stay on `Default` — the user picks the actual model post-add.
fn default_family(id: &str) -> &str {
    match id {
        // Wire-id values from ModelFamily. Keep aligned with the refreshed
        // default_model slugs above so a "+ Add Provider" autofill yields a
        // consistent model+family pair.
        "deepseek" => "DeepSeekV4Chat",
        "openai" => "Gpt5",
        "anthropic" => "ClaudeSonnet46",
        "google" => "Gemini3Flash",
        "mistral" => "MistralLarge3",
        // OpenCode Go's default bundle model is DeepSeek V4 Flash — same
        // quirks profile as DeepSeek direct. Note: the persisted `family`
        // is no longer authoritative for opencode_go — `build_provider_
        // instance` ignores it and picks DeepSeekV4Chat / MiniMaxM25Cloud
        // per-slug. This default still drives the "+ Add Provider"
        // autofill UX.
        "opencode_go" => "DeepSeekV4Chat",
        // MiniMax M2.7 — closest existing family is MiniMaxM25Cloud (same
        // tool-call extraction shape). User can switch in the family
        // dropdown if a more specific profile lands later.
        "minimax" | "minimax_anthropic" => "MiniMaxM25Cloud",
        _ => "Default",
    }
}

/// Parse a wire-string ModelProfile from the frontend ("Cheap" / "Fast" /
/// "Code" / "Powerful"). Returns `None` for anything else so a stale or
/// typo'd payload silently falls through to "use default_model" rather
/// than poisoning the config.
fn parse_model_profile(s: &str) -> Option<athen_core::llm::ModelProfile> {
    use athen_core::llm::ModelProfile;
    match s {
        "Cheap" => Some(ModelProfile::Cheap),
        "Fast" => Some(ModelProfile::Fast),
        "Code" => Some(ModelProfile::Code),
        "Powerful" => Some(ModelProfile::Powerful),
        _ => None,
    }
}

/// Seeded per-tier model slug presets surfaced in the Settings UI when
/// the user first adds a provider. Returns (Cheap, Fast, Code, Powerful)
/// — empty strings mean "fall back to default_model". The user edits any
/// of these from the per-provider config card; the values land in
/// `ProviderConfig.tier_models`. Pure metadata — no live calls happen
/// here. Refresh whenever a provider rolls out new model generations.
fn default_tier_slugs(id: &str) -> (&'static str, &'static str, &'static str, &'static str) {
    match id {
        // (Cheap, Fast, Code, Powerful)
        "deepseek" => (
            "deepseek-v4-flash",
            "deepseek-v4-flash",
            "deepseek-v4-pro",
            "deepseek-v4-pro",
        ),
        "anthropic" => (
            "claude-haiku-4-5-20251001",
            "claude-sonnet-4-6",
            "claude-sonnet-4-6",
            "claude-opus-4-7",
        ),
        "google" => (
            "gemini-3.1-flash-lite-preview",
            "gemini-3.1-flash-lite-preview",
            "gemini-3.1-pro-preview",
            "gemini-3.1-pro-preview",
        ),
        "openai" => ("gpt-5.4-nano", "gpt-5.4-mini", "gpt-5.5", "gpt-5.5-pro"),
        "mistral" => (
            "ministral-3b-latest",
            "mistral-small-latest",
            "codestral-latest",
            "mistral-large-latest",
        ),
        "openrouter" => (
            "openai/gpt-5.4-mini",
            "openai/gpt-5.5",
            "anthropic/claude-sonnet-4-6",
            "anthropic/claude-opus-4-7",
        ),
        // OpenCode Go covers both OpenAI-compat (DeepSeek/Qwen/Kimi/GLM/
        // MiMo) and Anthropic-compat (MiniMax M2.x) slugs — dispatch is
        // per-slug. Default seed leans on the DeepSeek V4 line for its
        // generous 5h quota; users can swap any tier to e.g.
        // `minimax-m2.7` and the router automatically routes that tier
        // through the Anthropic adapter.
        "opencode_go" => (
            "deepseek-v4-flash",
            "deepseek-v4-flash",
            "deepseek-v4-pro",
            "deepseek-v4-pro",
        ),
        "minimax" | "minimax_anthropic" => (
            "minimax-m2.7",
            "minimax-m2.7",
            "minimax-m2.7",
            "minimax-m2.7",
        ),
        // Local providers leave tiers empty by default — the user only
        // has one model loaded most of the time.
        _ => ("", "", "", ""),
    }
}

/// Curated model catalog per provider — `(slug, display_name)` pairs the
/// Bundles UI offers in a dropdown alongside a "Custom..." escape hatch.
///
/// Goal is breadth of "what users actually pick", not exhaustive coverage.
/// Live `/models` enumeration is Phase 3; this static list ships now so
/// users stop guessing slug spellings. Empty slice = "no curated picks,
/// user types a custom slug" (used for local providers).
pub(crate) fn curated_models(id: &str) -> &'static [(&'static str, &'static str)] {
    match id {
        "deepseek" => &[
            ("deepseek-v4-flash", "DeepSeek V4 Flash"),
            ("deepseek-v4-pro", "DeepSeek V4 Pro"),
            ("deepseek-chat", "DeepSeek Chat (legacy)"),
            ("deepseek-reasoner", "DeepSeek Reasoner (legacy)"),
        ],
        "anthropic" => &[
            ("claude-haiku-4-5-20251001", "Claude Haiku 4.5"),
            ("claude-sonnet-4-6", "Claude Sonnet 4.6"),
            ("claude-opus-4-7", "Claude Opus 4.7"),
        ],
        "google" => &[
            ("gemini-3.1-flash-lite-preview", "Gemini 3.1 Flash Lite"),
            ("gemini-3.1-flash-preview", "Gemini 3.1 Flash"),
            ("gemini-3.1-pro-preview", "Gemini 3.1 Pro"),
            ("gemini-3-flash-preview", "Gemini 3 Flash (legacy)"),
            ("gemini-2.5-pro", "Gemini 2.5 Pro (legacy)"),
        ],
        "openai" => &[
            ("gpt-5.4-nano", "GPT-5.4 Nano"),
            ("gpt-5.4-mini", "GPT-5.4 Mini"),
            ("gpt-5.5", "GPT-5.5"),
            ("gpt-5.5-pro", "GPT-5.5 Pro"),
            ("o4-mini", "o4-mini (reasoning)"),
            ("gpt-4o", "GPT-4o (legacy)"),
        ],
        "mistral" => &[
            ("ministral-3b-latest", "Ministral 3B"),
            ("ministral-8b-latest", "Ministral 8B"),
            ("mistral-small-latest", "Mistral Small"),
            ("mistral-large-latest", "Mistral Large"),
            ("codestral-latest", "Codestral"),
        ],
        "openrouter" => &[
            ("openai/gpt-5.4-mini", "OpenAI GPT-5.4 Mini"),
            ("openai/gpt-5.5", "OpenAI GPT-5.5"),
            ("openai/gpt-5.5-pro", "OpenAI GPT-5.5 Pro"),
            ("anthropic/claude-haiku-4-5", "Anthropic Claude Haiku 4.5"),
            ("anthropic/claude-sonnet-4-6", "Anthropic Claude Sonnet 4.6"),
            ("anthropic/claude-opus-4-7", "Anthropic Claude Opus 4.7"),
            (
                "google/gemini-3.1-flash-lite",
                "Google Gemini 3.1 Flash Lite",
            ),
            ("google/gemini-3.1-pro", "Google Gemini 3.1 Pro"),
            ("deepseek/deepseek-v4", "DeepSeek V4"),
            ("meta-llama/llama-4-70b-instruct", "Meta Llama 4 70B"),
            ("qwen/qwen-3-72b-instruct", "Qwen 3 72B"),
        ],
        "opencode_go" => &[
            ("deepseek-v4-flash", "DeepSeek V4 Flash"),
            ("deepseek-v4-pro", "DeepSeek V4 Pro"),
            ("kimi-k2", "Kimi K2"),
            ("qwen3-coder-plus", "Qwen 3 Coder Plus"),
            ("glm-4.6", "GLM 4.6"),
            ("mimo-7b", "MiMo 7B"),
            ("minimax-m2.7", "MiniMax M2.7 (Anthropic wire)"),
        ],
        "minimax" | "minimax_anthropic" => &[
            ("minimax-m2.7", "MiniMax M2.7"),
            ("minimax-m2.5", "MiniMax M2.5 (legacy)"),
        ],
        // Local providers: user picks whatever they pulled. The text input
        // remains the only path.
        _ => &[],
    }
}

/// One catalog row returned to the frontend's Bundle dropdown.
#[derive(Serialize)]
pub struct ModelCatalogEntry {
    pub slug: &'static str,
    pub display_name: &'static str,
}

/// Return the curated `(slug, display_name)` list for a given Connection's
/// provider id. The frontend uses this to populate the per-tier model
/// dropdown in each Bundle card. Unknown ids return an empty list, which
/// the UI treats as "show the text input only".
#[tauri::command]
pub async fn list_curated_models(
    provider_id: String,
) -> std::result::Result<Vec<ModelCatalogEntry>, String> {
    Ok(curated_models(&provider_id)
        .iter()
        .map(|(slug, name)| ModelCatalogEntry {
            slug,
            display_name: name,
        })
        .collect())
}

/// Placeholder text for the API key field in the UI. Empty for local
/// providers (no key needed).
fn api_key_hint(id: &str) -> &str {
    match id {
        "anthropic" => "sk-ant-...",
        "openrouter" => "sk-or-...",
        "google" => "AIza...",
        // MiniMax Token Plan issues coding-plan keys with the `sk-cp-` prefix
        // (distinct from their standard API keys, which are JWT-shaped).
        "minimax" | "minimax_anthropic" => "sk-cp-...",
        "deepseek" | "openai" | "mistral" | "opencode_go" => "sk-...",
        _ => "",
    }
}

fn dashboard_url(id: &str) -> &'static str {
    match id {
        "deepseek" => "https://platform.deepseek.com/api_keys",
        "anthropic" => "https://console.anthropic.com/settings/keys",
        "google" => "https://aistudio.google.com/apikey",
        "openai" => "https://platform.openai.com/api-keys",
        "mistral" => "https://console.mistral.ai/api-keys/",
        "openrouter" => "https://openrouter.ai/keys",
        "opencode_go" => "https://opencode.ai/",
        "minimax" | "minimax_anthropic" => "https://platform.minimaxi.com/",
        "ollama" => "https://ollama.com/",
        "llamacpp" => "https://github.com/ggml-org/llama.cpp",
        _ => "",
    }
}

fn cost_note(id: &str) -> &'static str {
    match id {
        "deepseek" => "Pay-as-you-go. V4 Flash: ~$0.07/1M input, $0.28/1M output.",
        "anthropic" => "Pay-as-you-go. Sonnet 4.6: $3/1M input, $15/1M output.",
        "google" => "Free tier: 15 RPM, 1M TPM for Flash models. Paid plans available.",
        "openai" => "Pay-as-you-go. GPT-5.4-mini: $0.15/1M input, $0.60/1M output.",
        "mistral" => "Free tier for some models. Large: $2/1M input, $6/1M output.",
        "openrouter" => "Aggregator — prices vary by model. Some models are free.",
        "opencode_go" => "Relay service with own pricing — check their dashboard.",
        "minimax" | "minimax_anthropic" => "Token plan — check MiniMax pricing page.",
        "ollama" | "llamacpp" => "Free — runs on your hardware.",
        _ => "",
    }
}

fn key_format_hint(id: &str) -> &'static str {
    match id {
        "deepseek" => "Starts with \"sk-\", about 35 characters.",
        "anthropic" => "Starts with \"sk-ant-\", about 100 characters.",
        "google" => "Starts with \"AIza\", about 39 characters.",
        "openai" => "Starts with \"sk-\", about 50 characters.",
        "mistral" => "About 32 characters, alphanumeric.",
        "openrouter" => "Starts with \"sk-or-\", about 60 characters.",
        "opencode_go" => "Check OpenCode dashboard for format.",
        "minimax" | "minimax_anthropic" => "Starts with \"sk-cp-\" (Token Plan coding key).",
        _ => "",
    }
}

fn setup_steps(id: &str) -> &'static [&'static str] {
    match id {
        "deepseek" => &[
            "Go to platform.deepseek.com and sign up or log in.",
            "Navigate to API Keys in the left sidebar.",
            "Click \"Create new API key\" and copy it.",
            "Paste the key into Athen's API Key field.",
        ],
        "anthropic" => &[
            "Go to console.anthropic.com and sign up or log in.",
            "Open Settings → API Keys.",
            "Click \"Create Key\", name it, and copy the value.",
            "Paste the key into Athen's API Key field.",
        ],
        "google" => &[
            "Go to aistudio.google.com/apikey.",
            "Sign in with your Google account.",
            "Click \"Create API key\" and copy it.",
            "Paste the key into Athen's API Key field.",
        ],
        "openai" => &[
            "Go to platform.openai.com and sign up or log in.",
            "Navigate to API Keys in the sidebar.",
            "Click \"Create new secret key\" and copy it.",
            "Paste the key into Athen's API Key field.",
        ],
        "mistral" => &[
            "Go to console.mistral.ai and sign up or log in.",
            "Open API Keys from the sidebar.",
            "Click \"Create new key\" and copy it.",
            "Paste the key into Athen's API Key field.",
        ],
        "openrouter" => &[
            "Go to openrouter.ai and sign up or log in.",
            "Navigate to Keys from your dashboard.",
            "Click \"Create Key\" and copy it.",
            "Paste the key into Athen's API Key field.",
        ],
        "ollama" => &[
            "Install Ollama (see install commands below).",
            "Run: ollama pull <model-name> (e.g. ollama pull qwen3:8b).",
            "Ollama runs on port 11434 by default — no API key needed.",
            "In Athen, set the model slug to the name you pulled.",
        ],
        "llamacpp" => &[
            "Download llama.cpp from GitHub or install via package manager.",
            "Download a GGUF model file from HuggingFace.",
            "Run: llama-server -m <model-file.gguf> --port 8080",
            "In Athen, leave base URL as http://localhost:8080.",
        ],
        _ => &[],
    }
}

const OLLAMA_SNIPPETS: &[InstallSnippet] = &[
    InstallSnippet { os: "linux", label: "Install Ollama", cmd: "curl -fsSL https://ollama.com/install.sh | sh" },
    InstallSnippet { os: "macos", label: "Install Ollama", cmd: "brew install ollama" },
    InstallSnippet { os: "windows", label: "Install Ollama", cmd: "Download from https://ollama.com/download/windows" },
];

const LLAMACPP_SNIPPETS: &[InstallSnippet] = &[
    InstallSnippet { os: "linux", label: "Install llama.cpp", cmd: "git clone https://github.com/ggml-org/llama.cpp && cd llama.cpp && make -j" },
    InstallSnippet { os: "macos", label: "Install llama.cpp", cmd: "brew install llama.cpp" },
    InstallSnippet { os: "windows", label: "Install llama.cpp", cmd: "Download pre-built from https://github.com/ggml-org/llama.cpp/releases" },
];

fn install_snippets(id: &str) -> &'static [InstallSnippet] {
    match id {
        "ollama" => OLLAMA_SNIPPETS,
        "llamacpp" => LLAMACPP_SNIPPETS,
        _ => &[],
    }
}

/// Canonical list of provider IDs the backend knows how to talk to.
/// Adding a new provider only requires adding it here plus implementing
/// the matching `match` arms in `default_*`/`display_name`/`provider_type`/
/// `api_key_hint` and `build_router_for_provider`. The frontend renders
/// onboarding pickers and settings templates entirely from this list.
const PROVIDER_IDS: &[&str] = &[
    "deepseek",
    "anthropic",
    "google",
    "openai",
    "mistral",
    "openrouter",
    "opencode_go",
    "minimax",
    "minimax_anthropic",
    "ollama",
    "llamacpp",
];

/// One entry in the provider catalog returned to the frontend.
#[derive(Serialize)]
pub struct ProviderCatalogEntry {
    pub id: &'static str,
    pub name: &'static str,
    /// "cloud" or "local".
    pub provider_type: &'static str,
    pub default_base_url: &'static str,
    pub default_model: &'static str,
    /// `ModelFamily::wire_id()` value to pre-select in the family dropdown
    /// when this provider is added via the "+ Add Provider" chip. Empty
    /// string means "leave on Default" — used for OpenRouter and local
    /// providers where the user picks the actual model afterwards.
    pub default_family: &'static str,
    /// Placeholder text for the API key input. Empty for local providers.
    pub api_key_hint: &'static str,
    /// Seeded slug presets for each tier. Frontend uses these to autofill
    /// the four per-tier inputs in the provider config card and to power
    /// a "Reset to defaults" button. Empty strings mean "leave the input
    /// empty — fall through to default_model at request time".
    pub default_tier_cheap: &'static str,
    pub default_tier_fast: &'static str,
    pub default_tier_code: &'static str,
    pub default_tier_powerful: &'static str,
    // ── L1 help fields ─────────────────────────────────────────────
    /// Direct link to the provider's API key dashboard.
    pub dashboard_url: &'static str,
    /// Free-tier / pricing one-liner.
    pub cost_note: &'static str,
    /// Key format hint (e.g. "Starts with sk-...").
    pub key_format_hint: &'static str,
    /// 2-4 step quick-start instructions.
    pub setup_steps: &'static [&'static str],
    /// Install snippets for local providers (empty for cloud).
    pub install_snippets: &'static [InstallSnippet],
}

#[derive(Debug, Clone, Serialize)]
pub struct InstallSnippet {
    pub os: &'static str,
    pub label: &'static str,
    pub cmd: &'static str,
}

/// Return the canonical list of providers the app supports. Single source
/// of truth — onboarding and settings UI both render from this.
#[tauri::command]
pub async fn list_provider_catalog() -> std::result::Result<Vec<ProviderCatalogEntry>, String> {
    Ok(PROVIDER_IDS
        .iter()
        .map(|id| {
            let (cheap, fast, code, powerful) = default_tier_slugs(id);
            ProviderCatalogEntry {
                id,
                name: display_name(id),
                provider_type: provider_type(id),
                default_base_url: default_base_url(id),
                default_model: default_model(id),
                default_family: default_family(id),
                api_key_hint: api_key_hint(id),
                default_tier_cheap: cheap,
                default_tier_fast: fast,
                default_tier_code: code,
                default_tier_powerful: powerful,
                dashboard_url: dashboard_url(id),
                cost_note: cost_note(id),
                key_format_hint: key_format_hint(id),
                setup_steps: setup_steps(id),
                install_snippets: install_snippets(id),
            }
        })
        .collect())
}

/// Mask an API key, showing only the last 4 characters.
fn mask_api_key(key: &str) -> String {
    if key.len() <= 4 {
        return "*".repeat(key.len());
    }
    let visible = &key[key.len() - 4..];
    format!("{}...{}", &key[..3], visible)
}

/// Convert a `ProviderConfig` entry into `ProviderInfo` for the frontend.
fn provider_config_to_info(id: &str, config: &ProviderConfig, active_id: &str) -> ProviderInfo {
    let base_url = config
        .endpoint
        .as_deref()
        .unwrap_or_else(|| default_base_url(id))
        .to_string();

    // Config file key takes priority over env var.
    let (has_key, hint) = match &config.auth {
        AuthType::ApiKey(key) if !key.is_empty() && !key.starts_with("${") => {
            (true, mask_api_key(key))
        }
        _ => {
            // Fall back to env var (e.g. DEEPSEEK_API_KEY, OPENAI_API_KEY).
            let env_var = format!("{}_API_KEY", id.to_uppercase());
            if let Ok(env_key) = std::env::var(&env_var) {
                if !env_key.is_empty() {
                    (true, format!("{}  (env)", mask_api_key(&env_key)))
                } else {
                    (false, String::new())
                }
            } else {
                (false, String::new())
            }
        }
    };

    // Project the typed `tier_models` map onto a wire-string-keyed map for
    // the frontend. Empty map round-trips as an empty object so the JS
    // side can detect "no per-tier overrides set" and fall back to the
    // catalog's presets when rendering the inputs.
    let mut tier_models_wire: HashMap<String, String> = HashMap::new();
    for (profile, slug) in &config.tier_models {
        let key = match profile {
            athen_core::llm::ModelProfile::Cheap => "Cheap",
            athen_core::llm::ModelProfile::Fast => "Fast",
            athen_core::llm::ModelProfile::Code => "Code",
            athen_core::llm::ModelProfile::Powerful => "Powerful",
            // Local profile isn't user-editable from this UI.
            athen_core::llm::ModelProfile::Local => continue,
        };
        tier_models_wire.insert(key.to_string(), slug.clone());
    }

    ProviderInfo {
        id: id.to_string(),
        name: display_name(id).to_string(),
        provider_type: provider_type(id).to_string(),
        base_url,
        model: if config.default_model.is_empty() {
            default_model(id).to_string()
        } else {
            config.default_model.clone()
        },
        has_api_key: has_key,
        api_key_hint: hint,
        is_active: id == active_id,
        supports_vision: config.supports_vision,
        supports_documents: config.supports_documents,
        family: config.family.wire_id().to_string(),
        context_window_tokens: config.context_window_tokens,
        compaction_trigger_pct: config.compaction_trigger_pct,
        compaction_target_pct: config.compaction_target_pct,
        temperature: config.temperature,
        tier_models: tier_models_wire,
    }
}

/// One row in the family-dropdown catalog returned to the frontend.
#[derive(Serialize)]
pub struct ModelFamilyEntry {
    /// Stable wire identifier (e.g. `"Qwen35Local"`).
    pub id: &'static str,
    /// Human-readable label for the dropdown (e.g. `"Qwen 3.5 (local)"`).
    pub label: &'static str,
    /// Default model slug to pre-fill when this family is selected.
    pub default_slug: &'static str,
}

/// Return the catalog of `ModelFamily` presets the per-model quirks system
/// knows about. Frontend renders this as the family dropdown next to the
/// model-slug field on each provider card. Selecting a family pre-fills the
/// slug field with `default_slug`; the user can edit the slug freely.
#[tauri::command]
pub async fn list_model_families() -> std::result::Result<Vec<ModelFamilyEntry>, String> {
    use athen_core::llm::ModelFamily;
    Ok(ModelFamily::all()
        .iter()
        .map(|f| ModelFamilyEntry {
            id: f.wire_id(),
            label: f.display_label(),
            default_slug: athen_llm::quirks::seed::default_slug_for_family(*f),
        })
        .collect())
}

// ---------------------------------------------------------------------------
// Tauri commands
// ---------------------------------------------------------------------------

/// Return the current settings to populate the settings page.
#[tauri::command]
pub async fn get_settings(
    state: State<'_, AppState>,
) -> std::result::Result<SettingsResponse, String> {
    let models = load_models_config_hydrated(state.vault.as_ref()).await;
    let main_config = load_main_config_hydrated(state.vault.as_ref()).await;

    // Read the active provider from runtime state.
    let active = state.active_provider_id.lock().await.clone();

    let mut providers: Vec<ProviderInfo> = models
        .providers
        .iter()
        .map(|(id, cfg)| provider_config_to_info(id, cfg, &active))
        .collect();

    // Note: we deliberately don't synthesize a placeholder card when the
    // provider map is empty. Doing so caused a UI bug where deleting the
    // last provider made it look like the delete failed (the synthesizer
    // re-created a card for the now-orphan active id). The frontend's
    // "Add provider" button already covers the empty-list state.

    // Sort: active first, then alphabetical.
    providers.sort_by(|a, b| b.is_active.cmp(&a.is_active).then(a.id.cmp(&b.id)));

    let security_mode = format!("{:?}", main_config.security.mode).to_lowercase();

    let email = EmailSettingsInfo {
        enabled: main_config.email.enabled,
        imap_server: main_config.email.imap_server.clone(),
        imap_port: main_config.email.imap_port,
        username: main_config.email.username.clone(),
        has_password: !main_config.email.password.is_empty(),
        use_tls: main_config.email.use_tls,
        folders: main_config.email.folders.join(", "),
        poll_interval_secs: main_config.email.poll_interval_secs,
        lookback_hours: main_config.email.lookback_hours,
        smtp_server: main_config.email.smtp_server.clone(),
        smtp_port: main_config.email.smtp_port,
        smtp_username: main_config.email.smtp_username.clone(),
        has_smtp_password: !main_config.email.smtp_password.is_empty(),
        smtp_use_tls: main_config.email.smtp_use_tls,
        from_address: main_config.email.from_address.clone(),
    };

    let telegram = TelegramSettingsInfo {
        enabled: main_config.telegram.enabled,
        has_bot_token: !main_config.telegram.bot_token.is_empty(),
        bot_token_hint: if main_config.telegram.bot_token.is_empty() {
            String::new()
        } else {
            mask_api_key(&main_config.telegram.bot_token)
        },
        bot_token: main_config.telegram.bot_token.clone(),
        allowed_chat_ids: main_config.telegram.allowed_chat_ids.clone(),
        poll_interval_secs: main_config.telegram.poll_interval_secs,
    };

    let notifications = {
        let nc = &main_config.notifications;
        let (qh_enabled, qh_start_h, qh_start_m, qh_end_h, qh_end_m, qh_critical) =
            match &nc.quiet_hours {
                Some(qh) => (
                    true,
                    qh.start_hour,
                    qh.start_minute,
                    qh.end_hour,
                    qh.end_minute,
                    qh.allow_critical,
                ),
                None => (false, 22, 0, 8, 0, true),
            };

        NotificationSettingsInfo {
            preferred_channels: nc
                .preferred_channels
                .iter()
                .map(|k| format!("{k:?}"))
                .collect(),
            escalation_timeout_secs: nc.escalation_timeout_secs,
            quiet_hours_enabled: qh_enabled,
            quiet_start_hour: qh_start_h,
            quiet_start_minute: qh_start_m,
            quiet_end_hour: qh_end_h,
            quiet_end_minute: qh_end_m,
            quiet_allow_critical: qh_critical,
        }
    };

    let embeddings = {
        let ec = &main_config.embeddings;
        let mode_str = format!("{:?}", ec.mode);
        let (has_key, hint) = match &ec.api_key {
            Some(key) if !key.is_empty() => (true, Some(mask_api_key(key))),
            _ => (false, None),
        };
        EmbeddingSettingsInfo {
            mode: mode_str,
            provider: ec.provider.clone(),
            model: ec.model.clone(),
            base_url: ec.base_url.clone(),
            has_api_key: has_key,
            api_key_hint: hint,
        }
    };

    let web_search = {
        let ws = &main_config.web_search;
        let brave_configured = !ws.brave_api_key.is_empty();
        let tavily_configured = !ws.tavily_api_key.is_empty();
        WebSearchSettingsInfo {
            brave_configured,
            brave_hint: if brave_configured {
                mask_api_key(&ws.brave_api_key)
            } else {
                String::new()
            },
            tavily_configured,
            tavily_hint: if tavily_configured {
                mask_api_key(&ws.tavily_api_key)
            } else {
                String::new()
            },
        }
    };

    // Project Bundles for the new Bundles panel. Reuses the bundle
    // command's projection so list_bundles and get_settings agree.
    let active_bundle_id = models
        .assignments
        .get(athen_core::config::ACTIVE_BUNDLE_KEY)
        .cloned()
        .unwrap_or_default();
    let mut bundles: Vec<crate::bundle_settings::BundleView> = models
        .bundles
        .values()
        .map(|b| {
            // Tiny inline projection — kept here so settings.rs doesn't
            // re-import the projection helper. Matches the shape
            // returned by `bundle_settings::list_bundles`.
            let id = b.id.to_string();
            let tiers = crate::bundle_settings::BundleTiersView {
                cheap: b.tiers.get(&athen_core::llm::ModelProfile::Cheap).map(|t| {
                    crate::bundle_settings::BundleTierView {
                        connection_id: t.connection_id.clone(),
                        slug: t.slug.clone(),
                    }
                }),
                fast: b.tiers.get(&athen_core::llm::ModelProfile::Fast).map(|t| {
                    crate::bundle_settings::BundleTierView {
                        connection_id: t.connection_id.clone(),
                        slug: t.slug.clone(),
                    }
                }),
                code: b.tiers.get(&athen_core::llm::ModelProfile::Code).map(|t| {
                    crate::bundle_settings::BundleTierView {
                        connection_id: t.connection_id.clone(),
                        slug: t.slug.clone(),
                    }
                }),
                powerful: b
                    .tiers
                    .get(&athen_core::llm::ModelProfile::Powerful)
                    .map(|t| crate::bundle_settings::BundleTierView {
                        connection_id: t.connection_id.clone(),
                        slug: t.slug.clone(),
                    }),
            };
            crate::bundle_settings::BundleView {
                is_active: id == active_bundle_id,
                id,
                name: b.name.clone(),
                tiers,
                created_at: b.created_at.to_rfc3339(),
                updated_at: b.updated_at.to_rfc3339(),
            }
        })
        .collect();
    bundles.sort_by(|a, b| a.name.cmp(&b.name));

    Ok(SettingsResponse {
        providers,
        active_provider: active,
        active_bundle_id,
        bundles,
        security_mode,
        email,
        telegram,
        notifications,
        embeddings,
        web_search,
    })
}

/// Save or update a provider configuration.
///
/// If `api_key` is `None`, the existing key is preserved.
/// If `api_key` is `Some("")`, the key is removed.
/// If `api_key` is `Some("sk-...")`, the key is updated.
///
/// Saves to `~/.athen/models.toml`. Changes require an app restart.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn save_provider(
    id: String,
    base_url: String,
    model: String,
    api_key: Option<String>,
    supports_vision: Option<bool>,
    supports_documents: Option<bool>,
    family: Option<String>,
    context_window_tokens: Option<u32>,
    compaction_trigger_pct: Option<u8>,
    compaction_target_pct: Option<u8>,
    temperature: Option<f32>,
    // Wire-string keys: "Cheap" | "Fast" | "Code" | "Powerful". Frontend
    // posts a flat object; we parse each key into the typed enum. Missing
    // keys are treated as "fall back to default_model"; empty strings are
    // also skipped so a cleared input behaves identically.
    tier_models: Option<HashMap<String, String>>,
    state: State<'_, AppState>,
) -> std::result::Result<String, String> {
    let mut models = load_models_config();

    let existing = models.providers.get(&id);
    // Decide what to do with the api_key first, *then* derive the
    // AuthType to write into models.toml. The vault-backed path stores
    // the key out of band and writes `AuthType::None` on disk, so a
    // leaked models.toml carries no secret. The legacy path (no vault
    // wired — test/CLI builds) keeps the old behaviour of storing the
    // plaintext key under `AuthType::ApiKey`.
    //
    // `effective_key` is the value we'll use to (re)build the live
    // router below — vault-stored or in-flight — so a hot-reload after
    // save still has the credential available.
    let (auth, effective_key): (AuthType, Option<String>) = match (api_key, state.vault.as_ref()) {
        (Some(key), _) if key.is_empty() => {
            // User cleared the key: drop it from both the vault and the
            // existing AuthType. Failure to delete from the vault is
            // non-fatal (it might never have been written there).
            if let Some(vault) = state.vault.as_ref() {
                let _ = vault
                    .delete(
                        &crate::vault_creds::provider_scope(&id),
                        crate::vault_creds::KEY_API_KEY,
                    )
                    .await;
            }
            (AuthType::None, None)
        }
        (Some(key), Some(vault)) => {
            vault
                .set(
                    &crate::vault_creds::provider_scope(&id),
                    crate::vault_creds::KEY_API_KEY,
                    &key,
                )
                .await
                .map_err(|e| format!("Vault store provider api_key: {e}"))?;
            (AuthType::None, Some(key))
        }
        (Some(key), None) => (AuthType::ApiKey(key.clone()), Some(key)),
        (None, _) => {
            // No new key from the caller — preserve whatever was already
            // set. ALSO opportunistically migrate: if the existing
            // AuthType holds a plaintext ApiKey AND the vault doesn't
            // yet have one for this provider, move it across now and
            // write `AuthType::None` to disk. Lazy migration kicks in
            // on the very first Save the user performs through the
            // new code — they don't have to manually retype the key.
            let existing_auth = existing.map(|p| p.auth.clone()).unwrap_or(AuthType::None);
            let plaintext_legacy = match &existing_auth {
                AuthType::ApiKey(k) if !k.is_empty() && !k.starts_with("${") => Some(k.clone()),
                _ => None,
            };
            let vault_held = if let Some(vault) = state.vault.as_ref() {
                vault
                    .get(
                        &crate::vault_creds::provider_scope(&id),
                        crate::vault_creds::KEY_API_KEY,
                    )
                    .await
                    .ok()
                    .flatten()
                    .filter(|s| !s.is_empty())
            } else {
                None
            };
            match (state.vault.as_ref(), &plaintext_legacy, &vault_held) {
                (Some(vault), Some(legacy), None) => {
                    // Migrate plaintext → vault; flip the on-disk auth
                    // to None so models.toml stops carrying the secret.
                    vault
                        .set(
                            &crate::vault_creds::provider_scope(&id),
                            crate::vault_creds::KEY_API_KEY,
                            legacy,
                        )
                        .await
                        .map_err(|e| format!("Vault migrate provider api_key: {e}"))?;
                    (AuthType::None, Some(legacy.clone()))
                }
                (Some(_), _, Some(v)) => {
                    // Vault already has a value — make sure on-disk auth
                    // doesn't lie about a stale plaintext.
                    let on_disk = if matches!(&existing_auth, AuthType::ApiKey(_)) {
                        AuthType::None
                    } else {
                        existing_auth
                    };
                    (on_disk, Some(v.clone()))
                }
                _ => (existing_auth, plaintext_legacy),
            }
        }
    };

    let resolved_base_url = if base_url.is_empty() {
        default_base_url(&id).to_string()
    } else {
        base_url.clone()
    };

    let endpoint = if base_url.is_empty() || base_url == default_base_url(&id) {
        None
    } else {
        Some(base_url)
    };

    let resolved_model = if model.is_empty() {
        default_model(&id).to_string()
    } else {
        model.clone()
    };

    // Advanced fields preserve existing values when the caller omits
    // them (e.g. an older frontend that doesn't yet send them). When
    // present, percentages are clamped to [1, 100] so a fat-fingered 0
    // or 200 can't poison `resolve_compaction_budget`. The temperature
    // field is `Option<f32>` end-to-end: the frontend sends `Some(f32)`
    // when the user typed a number and `None` when they cleared the
    // field — mirroring the provider-config schema (None = adapter
    // default). There is no "preserve" sentinel for temperature
    // because the Save flow always renders the whole card; an empty
    // box is an explicit reset to the adapter default.
    let context_window_tokens = context_window_tokens
        .filter(|w| *w > 0)
        .or_else(|| existing.map(|p| p.context_window_tokens))
        .unwrap_or(128_000);
    let compaction_trigger_pct = compaction_trigger_pct
        .map(|p| p.clamp(1, 100))
        .or_else(|| existing.map(|p| p.compaction_trigger_pct))
        .unwrap_or(65);
    let compaction_target_pct = compaction_target_pct
        .map(|p| p.clamp(1, 100))
        .or_else(|| existing.map(|p| p.compaction_target_pct))
        .unwrap_or(30);
    // Clamp trigger above target so the user can't configure the
    // hysteresis backwards (a 30% trigger / 50% target would ping-pong).
    // Bumps trigger to target+1 if the user supplied an inverted pair.
    let compaction_trigger_pct =
        compaction_trigger_pct.max(compaction_target_pct.saturating_add(1).min(100));

    // If the caller didn't pass a flag, preserve the existing value
    // (so editing other fields doesn't accidentally clear vision/documents).
    let supports_vision_resolved =
        supports_vision.unwrap_or_else(|| existing.is_some_and(|p| p.supports_vision));
    let supports_documents_resolved =
        supports_documents.unwrap_or_else(|| existing.is_some_and(|p| p.supports_documents));

    // Family: parse the wire string (e.g. "Qwen35Local") into the typed enum,
    // preserving the existing value if absent or unrecognised. Unrecognised
    // strings fall back to existing → Default rather than erroring so the
    // settings save can't break for a stale frontend.
    let family_resolved = match family.as_deref() {
        Some(s) if !s.is_empty() => athen_core::llm::ModelFamily::from_wire_id(s)
            .unwrap_or_else(|| existing.map(|p| p.family).unwrap_or_default()),
        _ => existing.map(|p| p.family).unwrap_or_default(),
    };

    // Tier models: parse the wire-string map ("Cheap" → slug, …) into the
    // typed enum. Empty strings are dropped so a cleared input falls back
    // to `default_model`. `None` from the caller preserves the existing
    // map verbatim so editing other fields can't accidentally wipe per-
    // tier slugs.
    let tier_models_resolved = match tier_models {
        Some(raw) => {
            let mut parsed: HashMap<athen_core::llm::ModelProfile, String> = HashMap::new();
            for (k, v) in raw {
                if v.trim().is_empty() {
                    continue;
                }
                if let Some(profile) = parse_model_profile(&k) {
                    parsed.insert(profile, v);
                }
            }
            parsed
        }
        None => existing.map(|p| p.tier_models.clone()).unwrap_or_default(),
    };

    let provider = ProviderConfig {
        auth: auth.clone(),
        default_model: model,
        endpoint,
        context_window_tokens,
        compaction_trigger_pct,
        compaction_target_pct,
        supports_vision: supports_vision_resolved,
        supports_documents: supports_documents_resolved,
        family: family_resolved,
        temperature,
        tier_models: tier_models_resolved,
    };

    models.providers.insert(id.clone(), provider);
    save_models_config(&models)?;

    // Hot-reload if saving the currently active provider.
    let active_id = state.active_provider_id.lock().await.clone();
    if id == active_id {
        // Resolve the API key for the live router rebuild. `effective_key`
        // already reflects vault + in-flight + existing AuthType; only
        // fall back to the env var when none of those held a value.
        let router_api_key = effective_key.clone().or_else(|| {
            let env_var = format!("{}_API_KEY", id.to_uppercase());
            std::env::var(&env_var).ok().filter(|k| !k.is_empty())
        });

        let supports_vision = models.providers.get(&id).is_some_and(|c| c.supports_vision);
        let supports_documents = models
            .providers
            .get(&id)
            .is_some_and(|c| c.supports_documents);
        let family_for_router = models
            .providers
            .get(&id)
            .map(|c| c.family)
            .unwrap_or_default();
        let empty_tiers = std::collections::HashMap::new();
        let tier_models_for_router = models
            .providers
            .get(&id)
            .map(|c| &c.tier_models)
            .unwrap_or(&empty_tiers);
        let new_router = build_router_for_provider(
            &id,
            &resolved_base_url,
            &resolved_model,
            router_api_key.as_deref(),
            supports_vision,
            supports_documents,
            family_for_router,
            tier_models_for_router,
            // Global router rebuild on Settings save — no arc context,
            // so no slug pin applies. Per-arc pinning rebuilds its own
            // router at execution time when a pin is in force.
            None,
        );

        {
            let mut router_guard = state.router.write().await;
            *router_guard = new_router;
        }
        *state.model_name.lock().await = resolved_model.clone();

        let name = display_name(&id);
        info!("Hot-reloaded active provider {} ({})", name, resolved_model);
        Ok(format!(
            "Provider saved and activated ({} / {}).",
            name, resolved_model
        ))
    } else {
        Ok("Provider saved.".to_string())
    }
}

/// Delete a provider configuration.
///
/// If the deleted provider is the currently active one, automatically
/// switches to the first remaining provider (or "deepseek" as fallback)
/// and hot-reloads the router.
#[tauri::command]
pub async fn delete_provider(
    id: String,
    state: State<'_, AppState>,
) -> std::result::Result<String, String> {
    let mut models = load_models_config();

    if !models.providers.contains_key(&id) {
        return Err(format!("Provider '{}' not found.", id));
    }

    // Block deletion when any Bundle references this Connection. The
    // FE must move the bundle's pick off this connection (or delete
    // the bundle) before this connection can disappear — otherwise the
    // active Bundle's tier slot would silently fall back to the legacy
    // path or become undispatchable. Spelled out in `docs/BUNDLES.md`
    // §"Connection deleted while in-flight arcs are pinned to it".
    let referencing: Vec<String> = models
        .bundles
        .values()
        .filter(|b| b.tiers.values().any(|t| t.connection_id == id))
        .map(|b| b.name.clone())
        .collect();
    if !referencing.is_empty() {
        return Err(format!(
            "Cannot delete '{id}' — referenced by Bundle(s): {}. \
             Edit those Bundles to point at a different Connection first.",
            referencing.join(", ")
        ));
    }

    models.providers.remove(&id);
    save_models_config(&models)?;

    // Drop the deleted provider's vault entry so we don't leak it for the
    // lifetime of the OS keychain. Best-effort: a NoEntry is fine.
    if let Some(vault) = state.vault.as_ref() {
        let _ = vault
            .delete(
                &crate::vault_creds::provider_scope(&id),
                crate::vault_creds::KEY_API_KEY,
            )
            .await;
    }

    // Hydrate AFTER the save so disk stays clean (AuthType::None) but the
    // fallback router below sees the real api_key from the vault.
    crate::vault_creds::hydrate_models_from_vault(state.vault.as_ref(), &mut models).await;

    // If deleting the active provider, switch to a fallback.
    let active_id = state.active_provider_id.lock().await.clone();
    if id == active_id {
        let fallback_id = models
            .providers
            .keys()
            .next()
            .cloned()
            .unwrap_or_else(|| "deepseek".to_string());

        let fallback_cfg = models.providers.get(&fallback_id);
        let base_url = fallback_cfg
            .and_then(|c| c.endpoint.as_deref())
            .unwrap_or_else(|| default_base_url(&fallback_id))
            .to_string();
        let model = fallback_cfg
            .map(|c| c.default_model.as_str())
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| default_model(&fallback_id))
            .to_string();

        let api_key = fallback_cfg
            .and_then(|c| match &c.auth {
                AuthType::ApiKey(key) if !key.is_empty() && !key.starts_with("${") => {
                    Some(key.clone())
                }
                _ => None,
            })
            .or_else(|| {
                let env_var = format!("{}_API_KEY", fallback_id.to_uppercase());
                std::env::var(&env_var).ok().filter(|k| !k.is_empty())
            });

        let supports_vision = fallback_cfg.is_some_and(|c| c.supports_vision);
        let supports_documents = fallback_cfg.is_some_and(|c| c.supports_documents);
        let family_for_router = fallback_cfg.map(|c| c.family).unwrap_or_default();
        let empty_tiers = std::collections::HashMap::new();
        let tier_models_for_router = fallback_cfg.map(|c| &c.tier_models).unwrap_or(&empty_tiers);
        let new_router = build_router_for_provider(
            &fallback_id,
            &base_url,
            &model,
            api_key.as_deref(),
            supports_vision,
            supports_documents,
            family_for_router,
            tier_models_for_router,
            // Global router rebuild on provider delete — no arc context.
            None,
        );

        {
            let mut router_guard = state.router.write().await;
            *router_guard = new_router;
        }
        *state.active_provider_id.lock().await = fallback_id.clone();
        *state.model_name.lock().await = model;

        // Persist the legacy `active_provider` assignment for any
        // resolver fallback path that still reads it. The active Bundle
        // assignment is unaffected (delete_provider blocks earlier when
        // a Bundle references the deleted Connection, so the active
        // Bundle is guaranteed not to point at it).
        let mut persisted = load_models_config();
        persisted
            .assignments
            .insert("active_provider".to_string(), fallback_id.clone());
        if let Err(e) = save_models_config(&persisted) {
            warn!("Failed to persist active provider after delete: {e}");
        }

        let fallback_name = display_name(&fallback_id);
        info!("Deleted provider '{}', switched to {}", id, fallback_name);
        Ok(format!(
            "Provider '{}' deleted. Switched to {}.",
            id, fallback_name
        ))
    } else {
        info!("Deleted provider '{}'", id);
        Ok(format!("Provider '{}' deleted.", id))
    }
}

/// Test connectivity to an LLM provider by sending a simple request.
#[tauri::command]
pub async fn test_provider(
    id: String,
    base_url: String,
    model: String,
    api_key: Option<String>,
    state: State<'_, AppState>,
) -> std::result::Result<TestResult, String> {
    // For local providers, just check if the endpoint responds.
    let url = if base_url.is_empty() {
        default_base_url(&id).to_string()
    } else {
        base_url
    };

    let model = if model.is_empty() {
        default_model(&id).to_string()
    } else {
        model
    };

    // Resolve the API key: caller-provided first, then the hydrated
    // models.toml entry (vault-backed), then the per-provider env var.
    let key = if let Some(k) = api_key.filter(|k| !k.is_empty()) {
        k
    } else {
        let models = load_models_config_hydrated(state.vault.as_ref()).await;
        let from_config = models.providers.get(&id).and_then(|p| match &p.auth {
            AuthType::ApiKey(k) if !k.is_empty() && !k.starts_with("${") => Some(k.clone()),
            _ => None,
        });
        from_config
            .or_else(|| {
                let env_var = format!("{}_API_KEY", id.to_uppercase());
                std::env::var(&env_var).ok().filter(|k| !k.is_empty())
            })
            .unwrap_or_default()
    };

    // Build a minimal test request based on the provider type.
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {e}"))?;

    let result = match id.as_str() {
        "ollama" => test_ollama(&client, &url).await,
        "llamacpp" => test_llamacpp(&client, &url).await,
        "anthropic" => test_anthropic(&client, &url, &key, &model).await,
        "google" => test_google(&client, &url, &key, &model).await,
        _ => test_openai_compatible(&client, &url, &key, &model).await,
    };

    match result {
        Ok(msg) => Ok(TestResult {
            success: true,
            message: msg,
        }),
        Err(msg) => Ok(TestResult {
            success: false,
            message: msg,
        }),
    }
}

/// Switch the active LLM provider at runtime — compat shim retained so
/// the Connections panel's per-card "Set Active" button and the
/// onboarding wizard keep working under the Bundles model.
///
/// Under Bundles the source of truth for "what model do I use" is the
/// active Bundle, not a single active provider. This command does the
/// natural one-click thing for a user thinking in terms of providers:
/// **synthesise (or refresh) a Bundle named after this Connection with
/// every tier pointing at it, then activate that Bundle**. The result
/// is identical to clicking "+ New Bundle", filling each tier with
/// `(this connection, its default slug)`, and activating — collapsed
/// into one call.
///
/// Idempotent: re-running on the same provider id reuses the existing
/// auto-generated Bundle (matched by name) rather than spawning
/// duplicates on every click.
#[tauri::command]
pub async fn set_active_provider(
    id: String,
    state: State<'_, AppState>,
) -> std::result::Result<String, String> {
    let mut models = load_models_config_hydrated(state.vault.as_ref()).await;
    let provider_cfg = models
        .providers
        .get(&id)
        .ok_or_else(|| format!("Provider '{id}' not found in config."))?
        .clone();

    // Reject up-front if a cloud Connection has no key — the bundle
    // would activate but every request would 401. Matches today's UX
    // ("can't activate without a key").
    let has_key = matches!(&provider_cfg.auth, AuthType::ApiKey(k) if !k.is_empty() && !k.starts_with("${"))
        || std::env::var(format!("{}_API_KEY", id.to_uppercase()))
            .ok()
            .filter(|k| !k.is_empty())
            .is_some();
    let is_local = matches!(id.as_str(), "ollama" | "llamacpp");
    if !is_local && !has_key {
        let env_var = format!("{}_API_KEY", id.to_uppercase());
        return Err(format!(
            "No API key found for '{id}'. Set {env_var} env var or configure a key first."
        ));
    }

    // Build the per-tier picks. Prefer this Connection's `tier_models`
    // entries when set (preserves any per-tier slug a power user wrote
    // pre-Bundles), falling back to `default_model`.
    let default_slug = if provider_cfg.default_model.is_empty() {
        default_model(&id).to_string()
    } else {
        provider_cfg.default_model.clone()
    };
    let mut tiers: std::collections::HashMap<
        athen_core::llm::ModelProfile,
        athen_core::config::BundleTier,
    > = std::collections::HashMap::new();
    for tier in [
        athen_core::llm::ModelProfile::Cheap,
        athen_core::llm::ModelProfile::Fast,
        athen_core::llm::ModelProfile::Code,
        athen_core::llm::ModelProfile::Powerful,
    ] {
        let slug = provider_cfg
            .tier_models
            .get(&tier)
            .filter(|s| !s.is_empty())
            .cloned()
            .unwrap_or_else(|| default_slug.clone());
        tiers.insert(
            tier,
            athen_core::config::BundleTier {
                connection_id: id.clone(),
                slug,
            },
        );
    }

    // Idempotency: if there's already a Bundle with the auto-generated
    // name, refresh its tiers in place. Otherwise create a new one.
    let auto_name = display_name(&id);
    let now = chrono::Utc::now();
    let bundle = if let Some(existing) = models
        .bundles
        .values()
        .find(|b| b.name == auto_name)
        .cloned()
    {
        let mut updated = existing;
        updated.tiers = tiers;
        updated.updated_at = now;
        models
            .bundles
            .insert(updated.id.to_string(), updated.clone());
        updated
    } else {
        let bundle = athen_core::config::Bundle {
            id: uuid::Uuid::new_v4(),
            name: auto_name.to_string(),
            created_at: now,
            updated_at: now,
            tiers,
        };
        models.bundles.insert(bundle.id.to_string(), bundle.clone());
        bundle
    };

    // Activate it + persist.
    models.assignments.insert(
        athen_core::config::ACTIVE_BUNDLE_KEY.to_string(),
        bundle.id.to_string(),
    );
    // Also stamp the legacy `active_provider` key so resolver paths
    // that still read it as a fallback hint stay consistent.
    models
        .assignments
        .insert("active_provider".to_string(), id.clone());
    save_models_config(&models)?;

    // Rebuild the global router from the Bundle. Hydrate again to
    // resolve any vault-backed credential we just persisted.
    let hydrated = load_models_config_hydrated(state.vault.as_ref()).await;
    let new_router = crate::state::build_router_for_bundle(&bundle, &hydrated.providers);
    *state.router.write().await = new_router;

    // Keep the legacy `active_provider_id` / `model_name` snapshots in
    // sync — they back vision-check and a few other single-provider
    // call sites that still ask "what's the active one?"
    if let Some((cid, slug)) = crate::bundle_settings::derive_primary_connection(&bundle) {
        *state.active_provider_id.lock().await = cid;
        *state.model_name.lock().await = slug;
    }

    let name = display_name(&id);
    info!(
        bundle_id = %bundle.id,
        provider = %id,
        "Activated Bundle '{}' (one-Connection compat shim)",
        bundle.name
    );
    Ok(format!("Switched to {name}"))
}

/// Save general settings (security mode, etc.).
#[tauri::command]
pub async fn save_settings(security_mode: String) -> std::result::Result<String, String> {
    let mut config = load_main_config();

    config.security.mode = match security_mode.to_lowercase().as_str() {
        "bunker" => SecurityMode::Bunker,
        "yolo" => SecurityMode::Yolo,
        _ => SecurityMode::Assistant,
    };

    save_main_config(&config)?;
    Ok("Settings saved. Restart the app to apply changes.".to_string())
}

// ---------------------------------------------------------------------------
// Email settings commands
// ---------------------------------------------------------------------------

/// Save email monitor settings.
#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub async fn save_email_settings(
    enabled: bool,
    imap_server: String,
    imap_port: u16,
    username: String,
    password: Option<String>,
    use_tls: bool,
    folders: String,
    poll_interval_secs: u64,
    lookback_hours: u32,
    state: State<'_, AppState>,
) -> std::result::Result<String, String> {
    let mut config = load_main_config();

    config.email.enabled = enabled;
    config.email.imap_server = imap_server;
    config.email.imap_port = imap_port;
    config.email.username = username;
    if let Some(pw) = password {
        if !pw.is_empty() {
            // Vault path: store the password and blank it on disk so a
            // leaked config.toml carries no secret. Falls back to the
            // legacy plaintext write when no vault is wired (test/CLI).
            if let Some(vault) = state.vault.as_ref() {
                vault
                    .set(
                        crate::vault_creds::SCOPE_EMAIL_IMAP,
                        crate::vault_creds::KEY_PASSWORD,
                        &pw,
                    )
                    .await
                    .map_err(|e| format!("Vault store IMAP password: {e}"))?;
                config.email.password = String::new();
            } else {
                config.email.password = pw;
            }
        }
    } else if let Some(vault) = state.vault.as_ref() {
        // Caller didn't supply a new password. Opportunistic migration:
        // if a plaintext value still lives in config.toml AND the vault
        // doesn't yet have one, move it across now and blank the disk.
        if !config.email.password.is_empty() {
            let already = vault
                .get(
                    crate::vault_creds::SCOPE_EMAIL_IMAP,
                    crate::vault_creds::KEY_PASSWORD,
                )
                .await
                .ok()
                .flatten()
                .is_some_and(|s| !s.is_empty());
            if !already {
                vault
                    .set(
                        crate::vault_creds::SCOPE_EMAIL_IMAP,
                        crate::vault_creds::KEY_PASSWORD,
                        &config.email.password,
                    )
                    .await
                    .map_err(|e| format!("Vault migrate IMAP password: {e}"))?;
            }
            config.email.password = String::new();
        }
    }
    config.email.use_tls = use_tls;
    config.email.folders = folders
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    config.email.poll_interval_secs = poll_interval_secs;
    config.email.lookback_hours = lookback_hours;

    // Disjointness: refuse to save if the IMAP `username` is the
    // owner's own email — that would let an unauthenticated sender
    // masquerade as the owner over inbound mail.
    if let Some(lookup) = state.owner_lookup() {
        let mut candidates: Vec<(String, String)> = Vec::new();
        let uname = config.email.username.trim();
        if uname.contains('@') {
            candidates.push(("email".into(), uname.to_ascii_lowercase()));
        }
        validate_disjoint_from_owner(&lookup, &candidates).await?;
    }

    save_main_config(&config)?;
    Ok("Email settings saved. Restart to apply.".to_string())
}

/// Test email connection with the provided IMAP settings.
#[tauri::command]
pub async fn test_email_connection(
    imap_server: String,
    imap_port: u16,
    username: String,
    password: String,
    use_tls: bool,
) -> std::result::Result<TestResult, String> {
    // Run the blocking IMAP test in spawn_blocking
    let result = tokio::task::spawn_blocking(move || {
        test_imap_connection(&imap_server, imap_port, &username, &password, use_tls)
    })
    .await
    .map_err(|e| format!("Test task failed: {e}"))?;

    match result {
        Ok(msg) => Ok(TestResult {
            success: true,
            message: msg,
        }),
        Err(msg) => Ok(TestResult {
            success: false,
            message: msg,
        }),
    }
}

fn test_imap_connection(
    server: &str,
    port: u16,
    username: &str,
    password: &str,
    use_tls: bool,
) -> Result<String, String> {
    use std::net::TcpStream;
    use std::time::Duration;

    let addr = (server, port);
    let tcp = TcpStream::connect(addr).map_err(|e| format!("Connection failed: {e}"))?;
    tcp.set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| format!("Failed to set timeout: {e}"))?;
    tcp.set_write_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| format!("Failed to set timeout: {e}"))?;

    if use_tls {
        let connector = rustls_connector::RustlsConnector::new_with_native_certs()
            .map_err(|e| format!("TLS setup failed: {e}"))?;
        let tls_stream = connector
            .connect(server, tcp)
            .map_err(|e| format!("TLS handshake failed: {e}"))?;
        let client = imap::Client::new(tls_stream);
        let mut session = client
            .login(username, password)
            .map_err(|(e, _)| format!("Login failed: {e}"))?;
        let mailbox = session
            .select("INBOX")
            .map_err(|e| format!("Failed to select INBOX: {e}"))?;
        let count = mailbox.exists;
        session
            .logout()
            .map_err(|e| format!("Logout failed: {e}"))?;
        Ok(format!(
            "Connected successfully. INBOX has {} messages.",
            count
        ))
    } else {
        let client = imap::Client::new(tcp);
        let mut session = client
            .login(username, password)
            .map_err(|(e, _)| format!("Login failed: {e}"))?;
        let mailbox = session
            .select("INBOX")
            .map_err(|e| format!("Failed to select INBOX: {e}"))?;
        let count = mailbox.exists;
        session
            .logout()
            .map_err(|e| format!("Logout failed: {e}"))?;
        Ok(format!(
            "Connected successfully. INBOX has {} messages.",
            count
        ))
    }
}

/// Save SMTP outbound settings.
#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub async fn save_smtp_settings(
    smtp_server: String,
    smtp_port: u16,
    smtp_username: String,
    smtp_password: Option<String>,
    smtp_use_tls: bool,
    from_address: String,
    state: State<'_, AppState>,
) -> std::result::Result<String, String> {
    let mut config = load_main_config();
    config.email.smtp_server = smtp_server;
    config.email.smtp_port = smtp_port;
    config.email.smtp_username = smtp_username;
    if let Some(pw) = smtp_password {
        if !pw.is_empty() {
            if let Some(vault) = state.vault.as_ref() {
                vault
                    .set(
                        crate::vault_creds::SCOPE_EMAIL_SMTP,
                        crate::vault_creds::KEY_PASSWORD,
                        &pw,
                    )
                    .await
                    .map_err(|e| format!("Vault store SMTP password: {e}"))?;
                config.email.smtp_password = String::new();
            } else {
                config.email.smtp_password = pw;
            }
        }
    } else if let Some(vault) = state.vault.as_ref() {
        if !config.email.smtp_password.is_empty() {
            let already = vault
                .get(
                    crate::vault_creds::SCOPE_EMAIL_SMTP,
                    crate::vault_creds::KEY_PASSWORD,
                )
                .await
                .ok()
                .flatten()
                .is_some_and(|s| !s.is_empty());
            if !already {
                vault
                    .set(
                        crate::vault_creds::SCOPE_EMAIL_SMTP,
                        crate::vault_creds::KEY_PASSWORD,
                        &config.email.smtp_password,
                    )
                    .await
                    .map_err(|e| format!("Vault migrate SMTP password: {e}"))?;
            }
            config.email.smtp_password = String::new();
        }
    }
    config.email.smtp_use_tls = smtp_use_tls;
    config.email.from_address = from_address;

    // Disjointness: refuse to save when SMTP `from_address` or an
    // email-shaped `smtp_username` matches one of the owner contact's
    // identifiers. Same rationale as the IMAP path above.
    if let Some(lookup) = state.owner_lookup() {
        let mut candidates: Vec<(String, String)> = Vec::new();
        let from = config.email.from_address.trim();
        if !from.is_empty() {
            candidates.push(("email".into(), from.to_ascii_lowercase()));
        }
        let sun = config.email.smtp_username.trim();
        if sun.contains('@') {
            candidates.push(("email".into(), sun.to_ascii_lowercase()));
        }
        validate_disjoint_from_owner(&lookup, &candidates).await?;
    }

    save_main_config(&config)?;
    Ok("SMTP settings saved. Restart to apply.".to_string())
}

/// Test SMTP connection with the provided settings.
#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub async fn test_smtp_connection(
    smtp_server: String,
    smtp_port: u16,
    smtp_username: String,
    smtp_password: String,
    smtp_use_tls: bool,
    from_address: String,
) -> std::result::Result<TestResult, String> {
    use athen_core::traits::email_sender::EmailSender;
    use athen_sentidos::email_send::{LettreSmtpSender, SmtpSettings};

    if smtp_server.trim().is_empty() {
        return Ok(TestResult {
            success: false,
            message: "SMTP server required".into(),
        });
    }
    if from_address.trim().is_empty() {
        return Ok(TestResult {
            success: false,
            message: "From address required".into(),
        });
    }

    let settings = SmtpSettings {
        server: smtp_server,
        port: smtp_port,
        username: smtp_username,
        password: smtp_password,
        use_implicit_tls: smtp_use_tls,
        from_address,
    };
    let sender = match LettreSmtpSender::new(settings) {
        Ok(s) => s,
        Err(e) => {
            return Ok(TestResult {
                success: false,
                message: format!("Setup failed: {e}"),
            })
        }
    };
    match sender.test_connection().await {
        Ok(()) => Ok(TestResult {
            success: true,
            message: "SMTP authenticated successfully.".into(),
        }),
        Err(e) => Ok(TestResult {
            success: false,
            message: format!("Connection failed: {e}"),
        }),
    }
}

// ---------------------------------------------------------------------------
// Telegram settings commands
// ---------------------------------------------------------------------------

/// Save Telegram bot monitor settings.
#[tauri::command]
pub async fn save_telegram_settings(
    enabled: bool,
    bot_token: Option<String>,
    allowed_chat_ids: Vec<i64>,
    poll_interval_secs: Option<u64>,
    state: State<'_, AppState>,
) -> std::result::Result<String, String> {
    let mut config = load_main_config();

    config.telegram.enabled = enabled;
    if let Some(token) = bot_token {
        if !token.is_empty() {
            if let Some(vault) = state.vault.as_ref() {
                vault
                    .set(
                        crate::vault_creds::SCOPE_TELEGRAM,
                        crate::vault_creds::KEY_BOT_TOKEN,
                        &token,
                    )
                    .await
                    .map_err(|e| format!("Vault store Telegram bot token: {e}"))?;
                config.telegram.bot_token = String::new();
            } else {
                config.telegram.bot_token = token;
            }
        }
    } else if let Some(vault) = state.vault.as_ref() {
        if !config.telegram.bot_token.is_empty() {
            let already = vault
                .get(
                    crate::vault_creds::SCOPE_TELEGRAM,
                    crate::vault_creds::KEY_BOT_TOKEN,
                )
                .await
                .ok()
                .flatten()
                .is_some_and(|s| !s.is_empty());
            if !already {
                vault
                    .set(
                        crate::vault_creds::SCOPE_TELEGRAM,
                        crate::vault_creds::KEY_BOT_TOKEN,
                        &config.telegram.bot_token,
                    )
                    .await
                    .map_err(|e| format!("Vault migrate Telegram bot token: {e}"))?;
            }
            config.telegram.bot_token = String::new();
        }
    }
    config.telegram.allowed_chat_ids = allowed_chat_ids;
    if let Some(interval) = poll_interval_secs {
        config.telegram.poll_interval_secs = interval;
    }

    // Disjointness: catch the rare misconfig where the bot token's
    // numeric prefix (== bot's own user id) collides with the owner
    // contact's Telegram identifier. We pull the token from config,
    // which may have just been emptied above when the value was
    // routed into the vault — try the vault-backed value first.
    if let Some(lookup) = state.owner_lookup() {
        let mut maybe_token = config.telegram.bot_token.clone();
        if maybe_token.is_empty() {
            if let Some(vault) = state.vault.as_ref() {
                if let Ok(Some(t)) = vault
                    .get(
                        crate::vault_creds::SCOPE_TELEGRAM,
                        crate::vault_creds::KEY_BOT_TOKEN,
                    )
                    .await
                {
                    maybe_token = t;
                }
            }
        }
        let mut candidates: Vec<(String, String)> = Vec::new();
        if let Some(bot_id) = bot_user_id_from_token(&maybe_token) {
            candidates.push(("telegram_user".into(), bot_id));
        }
        validate_disjoint_from_owner(&lookup, &candidates).await?;
    }

    save_main_config(&config)?;
    Ok("Telegram settings saved. Restart to apply.".to_string())
}

/// Test Telegram bot connectivity by calling the `getMe` API endpoint.
#[tauri::command]
pub async fn test_telegram_connection(
    bot_token: String,
) -> std::result::Result<TestResult, String> {
    if bot_token.is_empty() {
        return Ok(TestResult {
            success: false,
            message: "Bot token is required.".to_string(),
        });
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {e}"))?;

    let url = format!("https://api.telegram.org/bot{}/getMe", bot_token);

    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("Connection failed: {e}"));

    match resp {
        Ok(r) if r.status().is_success() => {
            let body: serde_json::Value = r
                .json()
                .await
                .map_err(|e| format!("Invalid response: {e}"))?;
            let username = body
                .get("result")
                .and_then(|r| r.get("username"))
                .and_then(|u| u.as_str())
                .unwrap_or("unknown");
            Ok(TestResult {
                success: true,
                message: format!("Connected! Bot: @{}", username),
            })
        }
        Ok(r) => {
            let status = r.status();
            let text = r.text().await.unwrap_or_default();
            let detail = serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|v| {
                    v.get("description")
                        .and_then(|d| d.as_str())
                        .map(|s| s.to_string())
                })
                .unwrap_or_else(|| text.chars().take(200).collect());
            Ok(TestResult {
                success: false,
                message: format!("HTTP {}: {}", status, detail),
            })
        }
        Err(msg) => Ok(TestResult {
            success: false,
            message: msg,
        }),
    }
}

// ---------------------------------------------------------------------------
// Web search settings commands
// ---------------------------------------------------------------------------

/// Save web search provider keys.
///
/// Each key follows the same convention as the LLM provider commands:
/// - `None` keeps the existing value untouched.
/// - `Some("")` clears the key.
/// - `Some("key")` updates it.
///
/// Changes take effect after a restart — the runtime builds the
/// MultiSearchProvider chain from this config in `AppState::new`.
#[tauri::command]
pub async fn save_web_search_settings(
    brave_api_key: Option<String>,
    tavily_api_key: Option<String>,
    state: State<'_, AppState>,
) -> std::result::Result<String, String> {
    let mut config = load_main_config();

    migrate_websearch_key(
        state.vault.as_ref(),
        crate::vault_creds::SCOPE_WEBSEARCH_BRAVE,
        brave_api_key,
        &mut config.web_search.brave_api_key,
        "Brave",
    )
    .await?;
    migrate_websearch_key(
        state.vault.as_ref(),
        crate::vault_creds::SCOPE_WEBSEARCH_TAVILY,
        tavily_api_key,
        &mut config.web_search.tavily_api_key,
        "Tavily",
    )
    .await?;

    save_main_config(&config)?;
    Ok("Web search settings saved. Restart to apply.".to_string())
}

/// Shared logic for `save_web_search_settings`. Treats:
/// - `Some(non-empty)` → write to vault (or to `cfg_field` if no vault),
///   blank `cfg_field` on disk.
/// - `Some("")` → delete from vault, blank `cfg_field`.
/// - `None` with a non-empty plaintext still in `cfg_field` → migrate
///   it to the vault if the vault doesn't already hold one. Lazy
///   migration that doesn't require the user to retype.
/// - `None` with empty `cfg_field` → no-op.
async fn migrate_websearch_key(
    vault: Option<&Arc<dyn athen_core::traits::vault::Vault>>,
    scope: &str,
    incoming: Option<String>,
    cfg_field: &mut String,
    label: &str,
) -> std::result::Result<(), String> {
    use crate::vault_creds::KEY_API_KEY;
    match incoming {
        Some(key) if key.is_empty() => {
            if let Some(v) = vault {
                let _ = v.delete(scope, KEY_API_KEY).await;
            }
            *cfg_field = String::new();
        }
        Some(key) => {
            if let Some(v) = vault {
                v.set(scope, KEY_API_KEY, &key)
                    .await
                    .map_err(|e| format!("Vault store {label} API key: {e}"))?;
                *cfg_field = String::new();
            } else {
                *cfg_field = key;
            }
        }
        None => {
            if let Some(v) = vault {
                if !cfg_field.is_empty() {
                    let already = v
                        .get(scope, KEY_API_KEY)
                        .await
                        .ok()
                        .flatten()
                        .is_some_and(|s| !s.is_empty());
                    if !already {
                        v.set(scope, KEY_API_KEY, cfg_field)
                            .await
                            .map_err(|e| format!("Vault migrate {label} API key: {e}"))?;
                    }
                    *cfg_field = String::new();
                }
            }
        }
    }
    Ok(())
}

/// Frontend-shaped view of the attachment policy. Sizes go to/from the
/// UI in MB so users don't have to count zeros; the backend round-trips
/// to bytes. MIME types are grouped into named bundles (images, pdfs,
/// office, …) instead of raw prefixes — the UI shows checkboxes with
/// friendly labels and the backend expands those into the underlying
/// `mime_allowlist` prefix list.
#[derive(Debug, Clone, Serialize)]
pub struct AttachmentPolicySettings {
    pub mime_bundles: Vec<String>,
    pub max_attachment_mb: u64,
    pub max_event_mb: u64,
    pub min_inline_trust: String,
    pub min_download_trust: String,
    pub byte_ttl_days: u32,
}

/// Bundle id -> the set of `mime_allowlist` prefixes it expands to.
///
/// Treated as the single source of truth: a bundle is "checked" iff any
/// of its prefixes is present in the persisted policy; saving a checked
/// bundle re-emits all of its prefixes. Bundles are all-or-nothing on
/// purpose — non-technical users shouldn't have to pick "Word but not
/// Excel". Power users editing `config.toml` directly are not the
/// audience here.
const MIME_BUNDLES: &[(&str, &[&str])] = &[
    ("images", &["image/"]),
    ("pdfs", &["application/pdf"]),
    ("text", &["text/"]),
    (
        "office",
        &[
            "application/vnd.openxmlformats-officedocument",
            "application/msword",
            "application/vnd.ms-excel",
            "application/vnd.ms-powerpoint",
        ],
    ),
    ("data", &["application/json", "application/xml"]),
];

fn prefixes_to_bundles(prefixes: &[String]) -> Vec<String> {
    let lower: Vec<String> = prefixes.iter().map(|p| p.to_ascii_lowercase()).collect();
    MIME_BUNDLES
        .iter()
        .filter_map(|(id, members)| {
            let any_present = members
                .iter()
                .any(|m| lower.iter().any(|p| p == &m.to_ascii_lowercase()));
            if any_present {
                Some((*id).to_string())
            } else {
                None
            }
        })
        .collect()
}

fn bundles_to_prefixes(bundles: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (id, members) in MIME_BUNDLES {
        if bundles.iter().any(|b| b == id) {
            for m in members.iter() {
                let s = (*m).to_string();
                if !out.contains(&s) {
                    out.push(s);
                }
            }
        }
    }
    out
}

fn trust_level_to_string(t: athen_core::contact::TrustLevel) -> &'static str {
    use athen_core::contact::TrustLevel;
    match t {
        TrustLevel::Unknown => "Unknown",
        TrustLevel::Neutral => "Neutral",
        TrustLevel::Known => "Known",
        TrustLevel::Trusted => "Trusted",
        TrustLevel::AuthUser => "AuthUser",
    }
}

fn trust_level_from_string(s: &str) -> Option<athen_core::contact::TrustLevel> {
    use athen_core::contact::TrustLevel;
    match s {
        "Unknown" => Some(TrustLevel::Unknown),
        "Neutral" => Some(TrustLevel::Neutral),
        "Known" => Some(TrustLevel::Known),
        "Trusted" => Some(TrustLevel::Trusted),
        "AuthUser" => Some(TrustLevel::AuthUser),
        _ => None,
    }
}

#[tauri::command]
pub async fn get_attachment_policy_settings(
) -> std::result::Result<AttachmentPolicySettings, String> {
    let cfg = load_main_config();
    let p = cfg.attachment_policy;
    Ok(AttachmentPolicySettings {
        mime_bundles: prefixes_to_bundles(&p.mime_allowlist),
        max_attachment_mb: p.max_attachment_bytes / (1024 * 1024),
        max_event_mb: p.max_event_bytes / (1024 * 1024),
        min_inline_trust: trust_level_to_string(p.min_inline_trust).to_string(),
        min_download_trust: trust_level_to_string(p.min_download_trust).to_string(),
        byte_ttl_days: p.byte_ttl_days,
    })
}

#[tauri::command]
pub async fn save_attachment_policy_settings(
    mime_bundles: Vec<String>,
    max_attachment_mb: u64,
    max_event_mb: u64,
    min_inline_trust: String,
    min_download_trust: String,
    byte_ttl_days: u32,
) -> std::result::Result<String, String> {
    let mut cfg = load_main_config();

    let allowlist = bundles_to_prefixes(&mime_bundles);
    if allowlist.is_empty() {
        return Err(
            "Pick at least one file category — leaving everything off would \
             drop every attachment Athen ever sees."
                .into(),
        );
    }

    let inline = trust_level_from_string(&min_inline_trust)
        .ok_or_else(|| format!("Invalid min_inline_trust: {min_inline_trust}"))?;
    let download = trust_level_from_string(&min_download_trust)
        .ok_or_else(|| format!("Invalid min_download_trust: {min_download_trust}"))?;
    if inline < download {
        return Err(
            "Inline trust must be at or above download trust — auto-inlining \
             a sender we wouldn't even download from is incoherent."
                .into(),
        );
    }

    if max_event_mb < max_attachment_mb {
        return Err("Per-message total must be at least as large as the per-file cap.".into());
    }
    if byte_ttl_days == 0 {
        return Err("TTL must be at least 1 day. Use a long value to effectively disable.".into());
    }

    cfg.attachment_policy.mime_allowlist = allowlist;
    cfg.attachment_policy.max_attachment_bytes = max_attachment_mb.saturating_mul(1024 * 1024);
    cfg.attachment_policy.max_event_bytes = max_event_mb.saturating_mul(1024 * 1024);
    cfg.attachment_policy.min_inline_trust = inline;
    cfg.attachment_policy.min_download_trust = download;
    cfg.attachment_policy.byte_ttl_days = byte_ttl_days;

    save_main_config(&cfg)?;
    Ok("Attachment policy saved. Restart to apply.".to_string())
}

/// Test a web search provider key with a tiny smoke query. The provider
/// `id` must be one of `"brave"` or `"tavily"`; DDG isn't keyed and
/// doesn't need a test path.
#[tauri::command]
pub async fn test_web_search_provider(
    provider: String,
    api_key: String,
) -> std::result::Result<TestResult, String> {
    use athen_web::WebSearchProvider;

    if api_key.trim().is_empty() {
        return Ok(TestResult {
            success: false,
            message: "API key is required.".to_string(),
        });
    }

    let backend: Box<dyn WebSearchProvider> = match provider.as_str() {
        "brave" => Box::new(athen_web::BraveSearch::new(api_key)),
        "tavily" => Box::new(athen_web::TavilySearch::new(api_key)),
        other => {
            return Ok(TestResult {
                success: false,
                message: format!("Unknown provider '{other}'. Use 'brave' or 'tavily'."),
            });
        }
    };

    match backend.search("athen ai agent", 1).await {
        Ok(hits) if hits.is_empty() => Ok(TestResult {
            success: true,
            message: "Connected — provider returned no hits for the smoke query, but auth works."
                .to_string(),
        }),
        Ok(hits) => Ok(TestResult {
            success: true,
            message: format!("Connected. Top result: {}", hits[0].title),
        }),
        Err(e) => Ok(TestResult {
            success: false,
            message: e.to_string(),
        }),
    }
}

// ---------------------------------------------------------------------------
// Provider-specific test functions
// ---------------------------------------------------------------------------

async fn test_ollama(client: &reqwest::Client, base_url: &str) -> Result<String, String> {
    let url = format!("{}/api/tags", base_url.trim_end_matches('/'));
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("Connection failed: {e}"))?;

    if resp.status().is_success() {
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("Invalid response: {e}"))?;
        let count = body
            .get("models")
            .and_then(|m| m.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        Ok(format!("Connected. {} model(s) available.", count))
    } else {
        Err(format!("Server returned HTTP {}", resp.status()))
    }
}

async fn test_llamacpp(client: &reqwest::Client, base_url: &str) -> Result<String, String> {
    let url = format!("{}/health", base_url.trim_end_matches('/'));
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("Connection failed: {e}"))?;

    if resp.status().is_success() {
        Ok("Connected. Server is healthy.".to_string())
    } else {
        Err(format!("Server returned HTTP {}", resp.status()))
    }
}

async fn test_openai_compatible(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    model: &str,
) -> Result<String, String> {
    if api_key.is_empty() {
        return Err("API key is required.".to_string());
    }

    let url = format!("{}/v1/chat/completions", base_url.trim_end_matches('/'));

    let body = serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": "Say hello in one word."}],
        "max_tokens": 10,
    });

    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("Connection failed: {e}"))?;

    if resp.status().is_success() {
        Ok(format!("Connected to {} successfully.", model))
    } else {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        // Try to extract an error message from JSON response.
        let detail = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| {
                v.get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .map(|s| s.to_string())
            })
            .unwrap_or_else(|| text.chars().take(200).collect());
        Err(format!("HTTP {}: {}", status, detail))
    }
}

// ---------------------------------------------------------------------------
// Calendar settings commands
// ---------------------------------------------------------------------------

/// Return the free-form calendar prompt the user wrote in Settings.
/// Empty string when unset.
#[tauri::command]
pub async fn get_calendar_prompt(
    _state: State<'_, AppState>,
) -> std::result::Result<String, String> {
    Ok(load_main_config().calendar.agent_prompt)
}

/// Save the free-form calendar prompt. Persisted to the main TOML config
/// — picked up immediately by the next sense event since
/// `build_context_message` reads it via `load_main_config_public()`.
#[tauri::command]
pub async fn save_calendar_prompt(
    _state: State<'_, AppState>,
    prompt: String,
) -> std::result::Result<(), String> {
    let mut config = load_main_config();
    config.calendar.agent_prompt = prompt;
    save_main_config(&config)?;
    Ok(())
}

/// Default-calendar info returned to the Settings UI.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CalendarAgentDefault {
    pub source_id: Option<String>,
    pub calendar_id: Option<String>,
    pub calendar_name: Option<String>,
}

/// Return the agent's default write-target calendar (set via Settings →
/// Calendar). When all three are `None`, the agent's `calendar_create`
/// falls back to `auto_pick_write_target` and ultimately local-only.
#[tauri::command]
pub async fn get_agent_default_calendar(
    _state: State<'_, AppState>,
) -> std::result::Result<CalendarAgentDefault, String> {
    let c = load_main_config().calendar;
    Ok(CalendarAgentDefault {
        source_id: c.agent_default_source_id,
        calendar_id: c.agent_default_calendar_id,
        calendar_name: c.agent_default_calendar_name,
    })
}

/// Set the agent's default write-target calendar. Pass all three `None`
/// to clear (reverts to auto-pick).
#[tauri::command]
pub async fn save_agent_default_calendar(
    _state: State<'_, AppState>,
    source_id: Option<String>,
    calendar_id: Option<String>,
    calendar_name: Option<String>,
) -> std::result::Result<(), String> {
    let mut config = load_main_config();
    config.calendar.agent_default_source_id = source_id;
    config.calendar.agent_default_calendar_id = calendar_id;
    config.calendar.agent_default_calendar_name = calendar_name;
    save_main_config(&config)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Notification settings commands
// ---------------------------------------------------------------------------

/// Return the current notification settings.
#[tauri::command]
pub async fn get_notification_settings(
    _state: State<'_, AppState>,
) -> std::result::Result<NotificationSettingsInfo, String> {
    let main_config = load_main_config();
    let nc = &main_config.notifications;

    let (qh_enabled, qh_start_h, qh_start_m, qh_end_h, qh_end_m, qh_critical) =
        match &nc.quiet_hours {
            Some(qh) => (
                true,
                qh.start_hour,
                qh.start_minute,
                qh.end_hour,
                qh.end_minute,
                qh.allow_critical,
            ),
            None => (false, 22, 0, 8, 0, true),
        };

    Ok(NotificationSettingsInfo {
        preferred_channels: nc
            .preferred_channels
            .iter()
            .map(|k| format!("{k:?}"))
            .collect(),
        escalation_timeout_secs: nc.escalation_timeout_secs,
        quiet_hours_enabled: qh_enabled,
        quiet_start_hour: qh_start_h,
        quiet_start_minute: qh_start_m,
        quiet_end_hour: qh_end_h,
        quiet_end_minute: qh_end_m,
        quiet_allow_critical: qh_critical,
    })
}

/// Save notification delivery settings.
#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub async fn save_notification_settings(
    _state: State<'_, AppState>,
    preferred_channels: Vec<String>,
    escalation_timeout_secs: u64,
    quiet_hours_enabled: bool,
    quiet_start_hour: Option<u32>,
    quiet_start_minute: Option<u32>,
    quiet_end_hour: Option<u32>,
    quiet_end_minute: Option<u32>,
    quiet_allow_critical: Option<bool>,
) -> std::result::Result<String, String> {
    let mut config = load_main_config();

    let channels: Vec<NotificationChannelKind> = preferred_channels
        .iter()
        .filter_map(|s| match s.to_lowercase().as_str() {
            "inapp" | "in_app" => Some(NotificationChannelKind::InApp),
            "telegram" => Some(NotificationChannelKind::Telegram),
            _ => None,
        })
        .collect();

    config.notifications = NotificationConfig {
        preferred_channels: if channels.is_empty() {
            vec![
                NotificationChannelKind::InApp,
                NotificationChannelKind::Telegram,
            ]
        } else {
            channels
        },
        escalation_timeout_secs,
        quiet_hours: if quiet_hours_enabled {
            Some(QuietHours {
                start_hour: quiet_start_hour.unwrap_or(22),
                start_minute: quiet_start_minute.unwrap_or(0),
                end_hour: quiet_end_hour.unwrap_or(8),
                end_minute: quiet_end_minute.unwrap_or(0),
                allow_critical: quiet_allow_critical.unwrap_or(true),
            })
        } else {
            None
        },
    };

    save_main_config(&config)?;
    Ok("Notification settings saved. Restart to apply.".to_string())
}

// ---------------------------------------------------------------------------
// Embedding settings commands
// ---------------------------------------------------------------------------

/// Save embedding / memory provider settings.
#[tauri::command]
pub async fn save_embedding_settings(
    state: State<'_, AppState>,
    mode: String,
    provider: Option<String>,
    model: Option<String>,
    base_url: Option<String>,
    api_key: Option<String>,
) -> std::result::Result<String, String> {
    use crate::vault_creds::{KEY_API_KEY, SCOPE_EMBEDDING};
    let mut config = load_main_config();

    config.embeddings.mode = match mode.as_str() {
        "Cloud" => EmbeddingMode::Cloud,
        "LocalOnly" => EmbeddingMode::LocalOnly,
        "Specific" => EmbeddingMode::Specific,
        "Off" => EmbeddingMode::Off,
        _ => EmbeddingMode::Automatic,
    };

    config.embeddings.provider = provider.filter(|s| !s.is_empty());
    config.embeddings.model = model.filter(|s| !s.is_empty());
    config.embeddings.base_url = base_url.filter(|s| !s.is_empty());

    // API key handling: vault-backed when wired, plaintext fallback
    // otherwise. Same migrate-on-Save discipline as the other secrets:
    // an explicit value writes to vault and blanks the disk; an empty
    // string deletes; absent + lingering plaintext triggers an
    // opportunistic migration so the user doesn't have to retype.
    match (api_key, state.vault.as_ref()) {
        (Some(key), _) if key.is_empty() => {
            if let Some(v) = state.vault.as_ref() {
                let _ = v.delete(SCOPE_EMBEDDING, KEY_API_KEY).await;
            }
            config.embeddings.api_key = None;
        }
        (Some(key), Some(vault)) => {
            vault
                .set(SCOPE_EMBEDDING, KEY_API_KEY, &key)
                .await
                .map_err(|e| format!("Vault store embedding api_key: {e}"))?;
            config.embeddings.api_key = None;
        }
        (Some(key), None) => {
            config.embeddings.api_key = Some(key);
        }
        (None, Some(vault)) => {
            if let Some(plaintext) = config.embeddings.api_key.clone().filter(|s| !s.is_empty()) {
                let already = vault
                    .get(SCOPE_EMBEDDING, KEY_API_KEY)
                    .await
                    .ok()
                    .flatten()
                    .is_some_and(|s| !s.is_empty());
                if !already {
                    vault
                        .set(SCOPE_EMBEDDING, KEY_API_KEY, &plaintext)
                        .await
                        .map_err(|e| format!("Vault migrate embedding api_key: {e}"))?;
                }
                config.embeddings.api_key = None;
            }
        }
        (None, None) => {
            // No vault, no caller value — preserve existing.
        }
    }

    save_main_config(&config)?;
    Ok("Embedding settings saved. Restart to apply.".to_string())
}

/// Test connectivity to an embedding provider.
#[tauri::command]
pub async fn test_embedding_provider(
    _state: State<'_, AppState>,
    provider: String,
    model: Option<String>,
    base_url: Option<String>,
    api_key: Option<String>,
) -> std::result::Result<TestResult, String> {
    // Keyword provider always works — no connectivity needed.
    if provider == "keyword" {
        return Ok(TestResult {
            success: true,
            message: "Keyword fallback is always available.".to_string(),
        });
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {e}"))?;

    let result = match provider.as_str() {
        "ollama" => {
            let url = base_url.as_deref().unwrap_or("http://localhost:11434");
            let model_name = model.as_deref().unwrap_or("nomic-embed-text");
            test_ollama_embedding(&client, url, model_name).await
        }
        _ => {
            // OpenAI-compatible embedding endpoint.
            let url = base_url.as_deref().unwrap_or("https://api.openai.com");
            let model_name = model.as_deref().unwrap_or("text-embedding-3-small");
            let key = api_key
                .or_else(|| {
                    // Fall back to saved config key.
                    let cfg = load_main_config();
                    cfg.embeddings.api_key.clone()
                })
                .unwrap_or_default();
            test_openai_embedding(&client, url, model_name, &key).await
        }
    };

    match result {
        Ok(msg) => Ok(TestResult {
            success: true,
            message: msg,
        }),
        Err(msg) => Ok(TestResult {
            success: false,
            message: msg,
        }),
    }
}

async fn test_ollama_embedding(
    client: &reqwest::Client,
    base_url: &str,
    model: &str,
) -> Result<String, String> {
    let url = format!("{}/api/embed", base_url.trim_end_matches('/'));

    let body = serde_json::json!({
        "model": model,
        "input": "test embedding connection",
    });

    let resp = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("Connection failed: {e}"))?;

    if resp.status().is_success() {
        Ok(format!(
            "Connected. Model '{}' is available for embeddings.",
            model
        ))
    } else {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        Err(format!(
            "HTTP {}: {}",
            status,
            text.chars().take(200).collect::<String>()
        ))
    }
}

async fn test_openai_embedding(
    client: &reqwest::Client,
    base_url: &str,
    model: &str,
    api_key: &str,
) -> Result<String, String> {
    if api_key.is_empty() {
        return Err("API key is required for cloud embedding providers.".to_string());
    }

    let url = format!("{}/v1/embeddings", base_url.trim_end_matches('/'));

    let body = serde_json::json!({
        "model": model,
        "input": "test embedding connection",
    });

    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("Connection failed: {e}"))?;

    if resp.status().is_success() {
        Ok(format!(
            "Connected. Model '{}' returned embeddings successfully.",
            model
        ))
    } else {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        let detail = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| {
                v.get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .map(|s| s.to_string())
            })
            .unwrap_or_else(|| text.chars().take(200).collect());
        Err(format!("HTTP {}: {}", status, detail))
    }
}

// ---------------------------------------------------------------------------
// Provider-specific test functions (continued)
// ---------------------------------------------------------------------------

async fn test_anthropic(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    model: &str,
) -> Result<String, String> {
    if api_key.is_empty() {
        return Err("API key is required.".to_string());
    }

    let url = format!("{}/v1/messages", base_url.trim_end_matches('/'));

    let body = serde_json::json!({
        "model": model,
        "max_tokens": 10,
        "messages": [{"role": "user", "content": "Say hello in one word."}],
    });

    let resp = client
        .post(&url)
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("Connection failed: {e}"))?;

    if resp.status().is_success() {
        Ok(format!("Connected to {} successfully.", model))
    } else {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        let detail = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| {
                v.get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .map(|s| s.to_string())
            })
            .unwrap_or_else(|| text.chars().take(200).collect());
        Err(format!("HTTP {}: {}", status, detail))
    }
}

/// Test a Google (Gemini) endpoint with a minimal `generateContent` call.
async fn test_google(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    model: &str,
) -> Result<String, String> {
    if api_key.is_empty() {
        return Err("API key is required.".to_string());
    }

    let url = format!(
        "{}/v1beta/models/{}:generateContent",
        base_url.trim_end_matches('/'),
        model
    );

    let body = serde_json::json!({
        "contents": [{
            "role": "user",
            "parts": [{ "text": "hi" }]
        }],
        "generationConfig": { "maxOutputTokens": 10 }
    });

    let resp = client
        .post(&url)
        .header("x-goog-api-key", api_key)
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("Connection failed: {e}"))?;

    if resp.status().is_success() {
        Ok(format!("Connected to {} successfully.", model))
    } else {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        let detail = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| {
                v.get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .map(|s| s.to_string())
            })
            .unwrap_or_else(|| text.chars().take(200).collect());
        Err(format!("HTTP {}: {}", status, detail))
    }
}

// ---------------------------------------------------------------------------
// GitHub identity settings commands
// ---------------------------------------------------------------------------
//
// Two vault scopes — `github:bot` and `github:user` — each hold:
//   - `token`       (the PAT)
//   - `user_name`   (commit author/committer name)
//   - `user_email`  (commit author/committer email)
//
// Agents that opt into a github identity on their profile get these env
// vars injected into every `shell_execute` (see
// `athen-app/src/github_identity.rs`). Identity is selected per-profile;
// the Settings UI just maintains the two credential rows.

const GH_SCOPE_BOT: &str = "github:bot";
const GH_SCOPE_USER: &str = "github:user";
const GH_KEY_TOKEN: &str = "token";
const GH_KEY_USER_NAME: &str = "user_name";
const GH_KEY_USER_EMAIL: &str = "user_email";

/// Light snapshot of one identity — credentials are NEVER returned;
/// only "is each field set?" + the non-secret name/email so the UI can
/// echo what's configured.
#[derive(serde::Serialize, Debug)]
pub struct GithubIdentitySettings {
    pub has_token: bool,
    pub user_name: String,
    pub user_email: String,
}

#[derive(serde::Serialize, Debug)]
pub struct GithubIdentitiesSnapshot {
    pub bot: GithubIdentitySettings,
    pub user: GithubIdentitySettings,
}

fn which_scope(identity: &str) -> std::result::Result<&'static str, String> {
    match identity {
        "bot" => Ok(GH_SCOPE_BOT),
        "user" => Ok(GH_SCOPE_USER),
        other => Err(format!(
            "Unknown GitHub identity '{other}' — expected 'bot' or 'user'"
        )),
    }
}

async fn load_one_identity(
    vault: &Arc<dyn athen_core::traits::vault::Vault>,
    scope: &str,
) -> GithubIdentitySettings {
    let has_token = matches!(
        vault.get(scope, GH_KEY_TOKEN).await,
        Ok(Some(t)) if !t.is_empty()
    );
    let user_name = vault
        .get(scope, GH_KEY_USER_NAME)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
    let user_email = vault
        .get(scope, GH_KEY_USER_EMAIL)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
    GithubIdentitySettings {
        has_token,
        user_name,
        user_email,
    }
}

/// Return what's configured for both identities. Credentials never leave
/// the vault — only the public commit name/email + a `has_token` flag.
#[tauri::command]
pub async fn get_github_identities(
    state: State<'_, AppState>,
) -> std::result::Result<GithubIdentitiesSnapshot, String> {
    let Some(vault) = state.vault.as_ref() else {
        return Ok(GithubIdentitiesSnapshot {
            bot: GithubIdentitySettings {
                has_token: false,
                user_name: String::new(),
                user_email: String::new(),
            },
            user: GithubIdentitySettings {
                has_token: false,
                user_name: String::new(),
                user_email: String::new(),
            },
        });
    };
    Ok(GithubIdentitiesSnapshot {
        bot: load_one_identity(vault, GH_SCOPE_BOT).await,
        user: load_one_identity(vault, GH_SCOPE_USER).await,
    })
}

/// Save credentials for one identity (`bot` or `user`).
///
/// `token`: `None` keeps the existing value, `Some("")` clears, `Some(x)` sets.
/// `user_name`/`user_email`: always overwrite (these aren't secret).
#[tauri::command]
pub async fn save_github_identity(
    identity: String,
    token: Option<String>,
    user_name: String,
    user_email: String,
    state: State<'_, AppState>,
) -> std::result::Result<String, String> {
    let scope = which_scope(&identity)?;
    let Some(vault) = state.vault.as_ref() else {
        return Err("Vault not available — credentials cannot be stored.".to_string());
    };

    match token {
        Some(t) if t.is_empty() => {
            vault
                .delete(scope, GH_KEY_TOKEN)
                .await
                .map_err(|e| format!("Clear token: {e}"))?;
        }
        Some(t) => {
            vault
                .set(scope, GH_KEY_TOKEN, &t)
                .await
                .map_err(|e| format!("Store token: {e}"))?;
        }
        None => {}
    }

    vault
        .set(scope, GH_KEY_USER_NAME, &user_name)
        .await
        .map_err(|e| format!("Store user_name: {e}"))?;
    vault
        .set(scope, GH_KEY_USER_EMAIL, &user_email)
        .await
        .map_err(|e| format!("Store user_email: {e}"))?;

    Ok(format!("{identity} identity saved."))
}

/// Test a GitHub PAT by hitting `GET /user` on api.github.com. Returns
/// the resolved login + `name` for the UI to confirm "yes, this is the
/// account we'll commit as." Token isn't persisted by this call —
/// callers pass it explicitly so they can validate before saving.
#[tauri::command]
pub async fn test_github_identity(token: String) -> std::result::Result<TestResult, String> {
    if token.is_empty() {
        return Ok(TestResult {
            success: false,
            message: "Token is required.".to_string(),
        });
    }
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {e}"))?;
    let resp = client
        .get("https://api.github.com/user")
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", "Athen")
        .send()
        .await;
    match resp {
        Ok(r) if r.status().is_success() => {
            let body: serde_json::Value = r
                .json()
                .await
                .map_err(|e| format!("Invalid response: {e}"))?;
            let login = body
                .get("login")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let name = body
                .get("name")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty());
            let msg = if let Some(n) = name {
                format!("Authenticated as @{login} ({n})")
            } else {
                format!("Authenticated as @{login}")
            };
            Ok(TestResult {
                success: true,
                message: msg,
            })
        }
        Ok(r) => {
            let status = r.status();
            let text = r.text().await.unwrap_or_default();
            let detail = serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|v| {
                    v.get("message")
                        .and_then(|m| m.as_str())
                        .map(|s| s.to_string())
                })
                .unwrap_or_else(|| text.chars().take(200).collect());
            Ok(TestResult {
                success: false,
                message: format!("HTTP {status}: {detail}"),
            })
        }
        Err(e) => Ok(TestResult {
            success: false,
            message: format!("Connection failed: {e}"),
        }),
    }
}

// ---------------------------------------------------------------------------
// Tests — onboarding non-destructive behavior
// ---------------------------------------------------------------------------
//
// These tests target the path-explicit helpers (`is_first_launch_in`,
// `mark_onboarded_in`) so they can run with isolated `TempDir` state
// without mutating the global `HOME` env var. The Tauri commands wrapping
// these helpers just resolve `~/.athen/` from `HOME` and delegate.

#[cfg(test)]
mod onboarding_tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn fresh_dir() -> TempDir {
        TempDir::new().expect("tempdir creation should never fail in tests")
    }

    fn write_models_with_provider(dir: &std::path::Path, provider_id: &str) {
        let toml = format!(
            r#"
[providers.{provider_id}]
auth = {{ ApiKey = "sk-test-not-real" }}
default_model = "test-model"
"#
        );
        fs::write(dir.join("models.toml"), toml).unwrap();
    }

    // ── is_first_launch_in: positive cases ─────────────────────────────────

    #[test]
    fn empty_dir_is_first_launch() {
        let dir = fresh_dir();
        assert!(
            is_first_launch_in(dir.path()),
            "fresh empty directory should be detected as first launch"
        );
    }

    #[test]
    fn nonexistent_dir_is_first_launch() {
        // Predicate must not panic on a path that doesn't exist yet —
        // `ensure_athen_dir` would have created it before this fires, but
        // defense in depth: confirm the predicate itself is robust.
        let parent = fresh_dir();
        let ghost = parent.path().join("nonexistent-subdir");
        assert!(
            is_first_launch_in(&ghost),
            "missing directory should be treated as first launch"
        );
    }

    // ── is_first_launch_in: sentinel takes priority ────────────────────────

    #[test]
    fn sentinel_suppresses_first_launch() {
        let dir = fresh_dir();
        fs::write(dir.path().join(ONBOARDED_SENTINEL), b"").unwrap();
        assert!(
            !is_first_launch_in(dir.path()),
            "sentinel must always suppress onboarding"
        );
    }

    #[test]
    fn sentinel_suppresses_even_when_models_toml_is_empty() {
        // Belt and suspenders: sentinel + empty models. Sentinel wins.
        let dir = fresh_dir();
        fs::write(dir.path().join(ONBOARDED_SENTINEL), b"").unwrap();
        fs::write(dir.path().join("models.toml"), b"").unwrap();
        assert!(!is_first_launch_in(dir.path()));
    }

    // ── is_first_launch_in: returning users without sentinel ───────────────

    #[test]
    fn existing_provider_in_models_suppresses_onboarding() {
        let dir = fresh_dir();
        write_models_with_provider(dir.path(), "deepseek");
        assert!(
            !is_first_launch_in(dir.path()),
            "a configured provider must suppress onboarding even without sentinel"
        );
    }

    #[test]
    fn empty_models_toml_suppresses_onboarding() {
        // A models.toml exists but has no providers. Conservative: assume
        // the user got partway through some prior setup; don't re-prompt.
        let dir = fresh_dir();
        fs::write(dir.path().join("models.toml"), b"").unwrap();
        assert!(!is_first_launch_in(dir.path()));
    }

    #[test]
    fn malformed_models_toml_suppresses_onboarding() {
        // Better to leave a corrupt config alone than to walk a user
        // through onboarding that might overwrite it.
        let dir = fresh_dir();
        fs::write(
            dir.path().join("models.toml"),
            b"this is not valid {[ toml ]]",
        )
        .unwrap();
        assert!(
            !is_first_launch_in(dir.path()),
            "malformed config must NOT trigger onboarding (don't overwrite user data)"
        );
    }

    #[test]
    fn config_toml_alone_suppresses_onboarding() {
        // User has main config but never configured an LLM via UI (e.g.
        // they set a key via env var). They're a returning user.
        let dir = fresh_dir();
        fs::write(dir.path().join("config.toml"), b"# real config goes here").unwrap();
        assert!(!is_first_launch_in(dir.path()));
    }

    // ── mark_onboarded_in ──────────────────────────────────────────────────

    #[test]
    fn mark_onboarded_writes_sentinel() {
        let dir = fresh_dir();
        mark_onboarded_in(dir.path()).unwrap();
        assert!(dir.path().join(ONBOARDED_SENTINEL).exists());
    }

    #[test]
    fn mark_onboarded_creates_missing_dir() {
        let parent = fresh_dir();
        let nested = parent.path().join("does-not-exist-yet");
        mark_onboarded_in(&nested).unwrap();
        assert!(nested.join(ONBOARDED_SENTINEL).exists());
    }

    #[test]
    fn mark_onboarded_is_idempotent() {
        let dir = fresh_dir();
        mark_onboarded_in(dir.path()).unwrap();
        mark_onboarded_in(dir.path()).unwrap();
        mark_onboarded_in(dir.path()).unwrap();
        assert!(dir.path().join(ONBOARDED_SENTINEL).exists());
    }

    #[test]
    fn mark_onboarded_does_not_touch_other_files() {
        // CRITICAL: completing onboarding must never overwrite or delete
        // an existing models.toml, config.toml, or any other state.
        let dir = fresh_dir();
        write_models_with_provider(dir.path(), "anthropic");
        let original_models = fs::read(dir.path().join("models.toml")).unwrap();

        fs::write(dir.path().join("config.toml"), b"# user main config").unwrap();
        let original_config = fs::read(dir.path().join("config.toml")).unwrap();

        let user_db_path = dir.path().join("athen.db");
        fs::write(&user_db_path, b"pretend-sqlite-bytes").unwrap();
        let original_db = fs::read(&user_db_path).unwrap();

        mark_onboarded_in(dir.path()).unwrap();

        assert_eq!(
            fs::read(dir.path().join("models.toml")).unwrap(),
            original_models,
            "models.toml must not be modified by onboarding completion"
        );
        assert_eq!(
            fs::read(dir.path().join("config.toml")).unwrap(),
            original_config,
            "config.toml must not be modified by onboarding completion"
        );
        assert_eq!(
            fs::read(&user_db_path).unwrap(),
            original_db,
            "user database must not be modified by onboarding completion"
        );
    }

    // ── End-to-end flow: completing onboarding suppresses re-prompts ───────

    #[test]
    fn completing_onboarding_suppresses_future_prompts() {
        let dir = fresh_dir();
        assert!(is_first_launch_in(dir.path()), "starts as first launch");

        mark_onboarded_in(dir.path()).unwrap();
        assert!(
            !is_first_launch_in(dir.path()),
            "after completion, must never prompt again"
        );

        // And again on a "subsequent boot"
        assert!(!is_first_launch_in(dir.path()));
    }

    #[test]
    fn returning_user_with_provider_then_marked_onboarded_stays_marked() {
        // Migration scenario: pre-sentinel returning user with a configured
        // provider. is_first_launch_in returns false (provider present).
        // The frontend may then call complete_onboarding to upgrade them
        // to the sentinel. After that, the sentinel keeps things stable
        // even if their models.toml is later edited or briefly empty.
        let dir = fresh_dir();
        write_models_with_provider(dir.path(), "openai");
        assert!(!is_first_launch_in(dir.path()));

        mark_onboarded_in(dir.path()).unwrap();

        // Simulate the user clearing their provider config later.
        fs::write(dir.path().join("models.toml"), b"").unwrap();
        assert!(
            !is_first_launch_in(dir.path()),
            "sentinel must override an emptied models.toml — \
             a user clearing their config should NOT re-trigger onboarding"
        );
    }

    #[test]
    fn save_then_complete_then_verify_provider_intact() {
        // Realistic flow: wizard saves a provider, then immediately calls
        // complete_onboarding. The provider must survive completion.
        let dir = fresh_dir();
        write_models_with_provider(dir.path(), "deepseek");
        let snapshot = fs::read_to_string(dir.path().join("models.toml")).unwrap();

        mark_onboarded_in(dir.path()).unwrap();

        let after = fs::read_to_string(dir.path().join("models.toml")).unwrap();
        assert_eq!(snapshot, after);
        let parsed: ModelsConfig = toml::from_str(&after).unwrap();
        assert!(parsed.providers.contains_key("deepseek"));
    }
}

#[cfg(test)]
mod attachment_policy_settings_tests {
    use super::*;
    use athen_core::attachment_policy::AttachmentPolicy;
    use athen_core::contact::TrustLevel;

    #[test]
    fn default_policy_maps_to_all_bundles() {
        // The shipped default allowlist covers every bundle. If someone
        // adds/removes a bundle without updating the default, this test
        // forces them to think about it.
        let p = AttachmentPolicy::default();
        let bundles = prefixes_to_bundles(&p.mime_allowlist);
        for (id, _) in MIME_BUNDLES {
            assert!(bundles.iter().any(|b| b == id), "bundle {id} missing");
        }
    }

    #[test]
    fn bundles_to_prefixes_expands_full_office_set() {
        let prefixes = bundles_to_prefixes(&["office".to_string()]);
        // Office expands to 4 distinct legacy/modern prefixes.
        assert!(prefixes.contains(&"application/msword".to_string()));
        assert!(prefixes
            .iter()
            .any(|p| p.contains("openxmlformats-officedocument")));
        assert_eq!(prefixes.len(), 4, "office bundle expands to 4 prefixes");
    }

    #[test]
    fn bundle_round_trip_preserves_identity() {
        let original = vec!["images".to_string(), "pdfs".to_string()];
        let prefixes = bundles_to_prefixes(&original);
        let back = prefixes_to_bundles(&prefixes);
        assert_eq!(back, original);
    }

    #[test]
    fn unknown_bundle_id_silently_dropped() {
        // Adversarial frontend: send a junk bundle id. Save shouldn't
        // panic; the unknown id just doesn't expand to anything.
        let prefixes = bundles_to_prefixes(&["images".into(), "banana".into()]);
        assert_eq!(prefixes, vec!["image/".to_string()]);
    }

    #[test]
    fn case_insensitive_prefix_match_for_bundle_detection() {
        // Some legacy configs may have mixed-case prefixes saved by hand.
        // The bundle detector lowercases on both sides.
        let prefixes = vec!["IMAGE/".to_string(), "Application/PDF".to_string()];
        let bundles = prefixes_to_bundles(&prefixes);
        assert!(bundles.iter().any(|b| b == "images"));
        assert!(bundles.iter().any(|b| b == "pdfs"));
    }

    #[test]
    fn trust_level_round_trip() {
        for tl in [
            TrustLevel::Unknown,
            TrustLevel::Neutral,
            TrustLevel::Known,
            TrustLevel::Trusted,
            TrustLevel::AuthUser,
        ] {
            let s = trust_level_to_string(tl);
            assert_eq!(trust_level_from_string(s), Some(tl), "round-trip for {s}");
        }
    }

    #[test]
    fn trust_level_unknown_string_is_none() {
        assert!(trust_level_from_string("Banana").is_none());
        assert!(trust_level_from_string("").is_none());
    }

    #[test]
    fn config_round_trip_keeps_policy() {
        // The whole point of #148 is that the persisted TOML survives.
        // Mutate every field, serialize, reload, assert no drift.
        let mut cfg = AthenConfig::default();
        cfg.attachment_policy.mime_allowlist = vec!["image/".into(), "text/csv".into()];
        cfg.attachment_policy.max_attachment_bytes = 5 * 1024 * 1024;
        cfg.attachment_policy.max_event_bytes = 50 * 1024 * 1024;
        cfg.attachment_policy.min_inline_trust = TrustLevel::Trusted;
        cfg.attachment_policy.min_download_trust = TrustLevel::Neutral;
        cfg.attachment_policy.byte_ttl_days = 7;

        let s = toml::to_string(&cfg).expect("serialize");
        let back: AthenConfig = toml::from_str(&s).expect("parse");
        assert_eq!(
            back.attachment_policy.mime_allowlist,
            vec!["image/".to_string(), "text/csv".to_string()]
        );
        assert_eq!(back.attachment_policy.max_attachment_bytes, 5 * 1024 * 1024);
        assert_eq!(back.attachment_policy.max_event_bytes, 50 * 1024 * 1024);
        assert_eq!(back.attachment_policy.min_inline_trust, TrustLevel::Trusted);
        assert_eq!(
            back.attachment_policy.min_download_trust,
            TrustLevel::Neutral
        );
        assert_eq!(back.attachment_policy.byte_ttl_days, 7);
    }

    #[test]
    fn legacy_config_without_policy_loads_with_defaults() {
        // A pre-#148 config.toml has no [attachment_policy] table.
        // serde(default) on the field must keep parsing those.
        let toml = "workspace_path = \".athen\"\n";
        let cfg: AthenConfig = toml::from_str(toml).expect("parse legacy");
        let default = AttachmentPolicy::default();
        assert_eq!(cfg.attachment_policy.byte_ttl_days, default.byte_ttl_days);
        assert_eq!(
            cfg.attachment_policy.max_attachment_bytes,
            default.max_attachment_bytes
        );
    }
}

#[cfg(test)]
mod owner_disjointness_tests {
    use super::*;
    use athen_contacts::{ContactStore, InMemoryContactStore, OwnerLookup};
    use athen_core::contact::{Contact, ContactIdentifier, IdentifierKind, TrustLevel};
    use uuid::Uuid;

    async fn make_owner_store_with(idents: Vec<(&str, IdentifierKind)>) -> Arc<dyn ContactStore> {
        let store: Arc<dyn ContactStore> = Arc::new(InMemoryContactStore::new());
        let mut owner = Contact {
            id: Uuid::new_v4(),
            name: "Alex".into(),
            trust_level: TrustLevel::AuthUser,
            trust_manual_override: true,
            identifiers: idents
                .into_iter()
                .map(|(v, k)| ContactIdentifier {
                    value: v.to_string(),
                    kind: k,
                })
                .collect(),
            interaction_count: 0,
            last_interaction: None,
            notes: None,
            blocked: false,
            is_owner: true,
        };
        let id = owner.id;
        owner.is_owner = true;
        store.save(&owner).await.unwrap();
        store.set_owner(&id).await.unwrap();
        store
    }

    #[tokio::test]
    async fn validate_disjoint_returns_human_error_on_email_conflict() {
        let store = make_owner_store_with(vec![("alex@example.com", IdentifierKind::Email)]).await;
        let lookup = OwnerLookup::new(store);
        let candidates = vec![("email".to_string(), "ALEX@example.com".to_string())];
        let err = validate_disjoint_from_owner(&lookup, &candidates)
            .await
            .unwrap_err();
        assert!(err.contains("Conflicts with owner contact"), "got: {err}");
        assert!(err.contains("alex@example.com"), "got: {err}");
    }

    #[tokio::test]
    async fn validate_disjoint_passes_when_no_overlap() {
        let store = make_owner_store_with(vec![("alex@example.com", IdentifierKind::Email)]).await;
        let lookup = OwnerLookup::new(store);
        let candidates = vec![("email".to_string(), "athen-bot@example.com".to_string())];
        assert!(validate_disjoint_from_owner(&lookup, &candidates)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn validate_disjoint_passes_when_no_owner_set() {
        // Empty store → no owner identifiers → no possible conflict,
        // even when candidates would otherwise overlap if the owner
        // existed. This is the "first-run" defensive case.
        let store: Arc<dyn ContactStore> = Arc::new(InMemoryContactStore::new());
        let lookup = OwnerLookup::new(store);
        let candidates = vec![("email".to_string(), "anybody@example.com".to_string())];
        assert!(validate_disjoint_from_owner(&lookup, &candidates)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn validate_disjoint_telegram_user_conflict() {
        let store = make_owner_store_with(vec![("987654321", IdentifierKind::Telegram)]).await;
        let lookup = OwnerLookup::new(store);
        let candidates = vec![("telegram_user".to_string(), "987654321".to_string())];
        let err = validate_disjoint_from_owner(&lookup, &candidates)
            .await
            .unwrap_err();
        assert!(err.contains("telegram_user=987654321"), "got: {err}");
    }

    #[test]
    fn bot_user_id_parses_valid_token() {
        let id = bot_user_id_from_token("123456789:ABCDEF-some-base64ish_payload");
        assert_eq!(id.as_deref(), Some("123456789"));
    }

    #[test]
    fn bot_user_id_rejects_malformed_token() {
        // No colon at all.
        assert!(bot_user_id_from_token("not-a-token").is_none());
        // Non-numeric prefix.
        assert!(bot_user_id_from_token("abc:def").is_none());
    }

    #[test]
    fn bot_user_id_rejects_empty_string() {
        assert!(bot_user_id_from_token("").is_none());
    }
}
