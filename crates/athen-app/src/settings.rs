//! Settings management commands for the Tauri frontend.
//!
//! Provides Tauri IPC commands for managing LLM providers, API keys,
//! and general application settings through the UI.

use std::path::PathBuf;

use serde::Serialize;
use tauri::State;
use tracing::{info, warn};

use athen_core::config::{
    AthenConfig, AuthType, ModelsConfig, ProviderConfig, SecurityMode,
};
use athen_core::config_loader;

use crate::state::{AppState, build_router_for_provider};

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
}

/// Full settings response for the frontend.
#[derive(Debug, Clone, Serialize)]
pub struct SettingsResponse {
    pub providers: Vec<ProviderInfo>,
    pub active_provider: String,
    pub security_mode: String,
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

/// Resolve the `~/.athen/` directory, creating it if needed.
fn ensure_athen_dir() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME")
        .ok_or_else(|| "HOME environment variable not set".to_string())?;
    let dir = PathBuf::from(home).join(".athen");
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("Failed to create ~/.athen/: {e}"))?;
    Ok(dir)
}

/// Load the current models config from `~/.athen/models.toml`, or return
/// an empty `ModelsConfig` if the file does not exist.
fn load_models_config() -> ModelsConfig {
    if let Ok(dir) = ensure_athen_dir() {
        let path = dir.join("models.toml");
        if path.exists() {
            match std::fs::read_to_string(&path) {
                Ok(content) => match toml::from_str::<ModelsConfig>(&content) {
                    Ok(cfg) => return cfg,
                    Err(e) => warn!("Failed to parse models.toml: {e}"),
                },
                Err(e) => warn!("Failed to read models.toml: {e}"),
            }
        }
    }
    ModelsConfig::default()
}

/// Save the models config to `~/.athen/models.toml`.
fn save_models_config(config: &ModelsConfig) -> Result<(), String> {
    let dir = ensure_athen_dir()?;
    let path = dir.join("models.toml");
    let content = toml::to_string_pretty(config)
        .map_err(|e| format!("Failed to serialize models config: {e}"))?;
    std::fs::write(&path, content)
        .map_err(|e| format!("Failed to write models.toml: {e}"))?;
    info!("Saved models config to {}", path.display());
    Ok(())
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

/// Save the main config to `~/.athen/config.toml`.
fn save_main_config(config: &AthenConfig) -> Result<(), String> {
    let dir = ensure_athen_dir()?;
    let path = dir.join("config.toml");
    let content = toml::to_string_pretty(config)
        .map_err(|e| format!("Failed to serialize config: {e}"))?;
    std::fs::write(&path, content)
        .map_err(|e| format!("Failed to write config.toml: {e}"))?;
    info!("Saved config to {}", path.display());
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers for provider info
// ---------------------------------------------------------------------------

/// Default base URL for a provider ID.
fn default_base_url(id: &str) -> &str {
    match id {
        "deepseek" => "https://api.deepseek.com",
        "openai" => "https://api.openai.com",
        "anthropic" => "https://api.anthropic.com",
        "ollama" => "http://localhost:11434",
        "llamacpp" => "http://localhost:8080",
        _ => "",
    }
}

/// Default model for a provider ID.
fn default_model(id: &str) -> &str {
    match id {
        "deepseek" => "deepseek-chat",
        "openai" => "gpt-4o",
        "anthropic" => "claude-sonnet-4-20250514",
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

/// Mask an API key, showing only the last 4 characters.
fn mask_api_key(key: &str) -> String {
    if key.len() <= 4 {
        return "*".repeat(key.len());
    }
    let visible = &key[key.len() - 4..];
    format!("{}...{}", &key[..3], visible)
}

/// Convert a `ProviderConfig` entry into `ProviderInfo` for the frontend.
fn provider_config_to_info(
    id: &str,
    config: &ProviderConfig,
    active_id: &str,
) -> ProviderInfo {
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
    }
}

// ---------------------------------------------------------------------------
// Tauri commands
// ---------------------------------------------------------------------------

/// Return the current settings to populate the settings page.
#[tauri::command]
pub async fn get_settings(
    state: State<'_, AppState>,
) -> std::result::Result<SettingsResponse, String> {
    let models = load_models_config();
    let main_config = load_main_config();

    // Read the active provider from runtime state.
    let active = state.active_provider_id.lock().await.clone();

    let mut providers: Vec<ProviderInfo> = models
        .providers
        .iter()
        .map(|(id, cfg)| provider_config_to_info(id, cfg, &active))
        .collect();

    // If no providers configured at all, show the active provider as a template.
    if providers.is_empty() {
        let has_env_key = std::env::var("DEEPSEEK_API_KEY")
            .ok()
            .filter(|k| !k.is_empty())
            .is_some();
        let hint = if has_env_key {
            let key = std::env::var("DEEPSEEK_API_KEY").unwrap_or_default();
            format!("{}  (env)", mask_api_key(&key))
        } else {
            String::new()
        };
        let model = state.model_name.lock().await.clone();
        providers.push(ProviderInfo {
            id: active.clone(),
            name: display_name(&active).to_string(),
            provider_type: provider_type(&active).to_string(),
            base_url: default_base_url(&active).to_string(),
            model,
            has_api_key: has_env_key,
            api_key_hint: hint,
            is_active: true,
        });
    }

    // Sort: active first, then alphabetical.
    providers.sort_by(|a, b| b.is_active.cmp(&a.is_active).then(a.id.cmp(&b.id)));

    let security_mode = format!("{:?}", main_config.security.mode).to_lowercase();

    Ok(SettingsResponse {
        providers,
        active_provider: active,
        security_mode,
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
pub async fn save_provider(
    id: String,
    base_url: String,
    model: String,
    api_key: Option<String>,
    state: State<'_, AppState>,
) -> std::result::Result<String, String> {
    let mut models = load_models_config();

    let existing = models.providers.get(&id);
    let auth = match api_key {
        Some(key) if key.is_empty() => AuthType::None,
        Some(key) => AuthType::ApiKey(key),
        None => existing
            .map(|p| p.auth.clone())
            .unwrap_or(AuthType::None),
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

    let provider = ProviderConfig {
        auth: auth.clone(),
        default_model: model,
        endpoint,
    };

    models.providers.insert(id.clone(), provider);
    save_models_config(&models)?;

    // Hot-reload if saving the currently active provider.
    let active_id = state.active_provider_id.lock().await.clone();
    if id == active_id {
        // Resolve the API key: saved key takes priority over env var.
        let router_api_key = match &auth {
            AuthType::ApiKey(key) if !key.is_empty() && !key.starts_with("${") => {
                Some(key.clone())
            }
            _ => {
                let env_var = format!("{}_API_KEY", id.to_uppercase());
                std::env::var(&env_var).ok().filter(|k| !k.is_empty())
            }
        };

        let new_router = build_router_for_provider(
            &id,
            &resolved_base_url,
            &resolved_model,
            router_api_key.as_deref(),
        );

        {
            let mut router_guard = state.router.write().await;
            *router_guard = new_router;
        }
        *state.model_name.lock().await = resolved_model.clone();

        let name = display_name(&id);
        info!("Hot-reloaded active provider {} ({})", name, resolved_model);
        Ok(format!("Provider saved and activated ({} / {}).", name, resolved_model))
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

    if models.providers.remove(&id).is_none() {
        return Err(format!("Provider '{}' not found.", id));
    }

    save_models_config(&models)?;

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

        let new_router =
            build_router_for_provider(&fallback_id, &base_url, &model, api_key.as_deref());

        {
            let mut router_guard = state.router.write().await;
            *router_guard = new_router;
        }
        *state.active_provider_id.lock().await = fallback_id.clone();
        *state.model_name.lock().await = model;

        if let Err(e) = persist_active_provider(&fallback_id) {
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

    // Resolve the API key: use provided, or fall back to env var, or config.
    let key = api_key.unwrap_or_else(|| {
        if id == "deepseek" {
            std::env::var("DEEPSEEK_API_KEY").unwrap_or_default()
        } else {
            let models = load_models_config();
            models
                .providers
                .get(&id)
                .and_then(|p| match &p.auth {
                    AuthType::ApiKey(k) if !k.is_empty() && !k.starts_with("${") => {
                        Some(k.clone())
                    }
                    _ => None,
                })
                .unwrap_or_default()
        }
    });

    // Build a minimal test request based on the provider type.
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {e}"))?;

    let result = match id.as_str() {
        "ollama" => test_ollama(&client, &url).await,
        "llamacpp" => test_llamacpp(&client, &url).await,
        "anthropic" => test_anthropic(&client, &url, &key, &model).await,
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

/// Switch the active LLM provider at runtime without restarting the app.
///
/// Builds a new `DefaultLlmRouter` for the requested provider and swaps it
/// into the shared `RwLock`, so all subsequent LLM calls use the new provider.
/// Also persists the choice to `~/.athen/models.toml` so it survives restarts.
#[tauri::command]
pub async fn set_active_provider(
    id: String,
    state: State<'_, AppState>,
) -> std::result::Result<String, String> {
    let models = load_models_config();
    let provider_cfg = models.providers.get(&id);

    let base_url = provider_cfg
        .and_then(|c| c.endpoint.as_deref())
        .unwrap_or_else(|| default_base_url(&id))
        .to_string();

    let model = provider_cfg
        .map(|c| c.default_model.as_str())
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| default_model(&id))
        .to_string();

    // Resolve API key: saved config takes priority over env var.
    let api_key = provider_cfg
        .and_then(|c| match &c.auth {
            AuthType::ApiKey(key) if !key.is_empty() && !key.starts_with("${") => {
                Some(key.clone())
            }
            _ => None,
        })
        .or_else(|| {
            let env_var = format!("{}_API_KEY", id.to_uppercase());
            std::env::var(&env_var).ok().filter(|k| !k.is_empty())
        });

    // Cloud providers require an API key.
    let is_local = matches!(id.as_str(), "ollama" | "llamacpp");
    if !is_local && api_key.is_none() {
        let env_var = format!("{}_API_KEY", id.to_uppercase());
        return Err(format!(
            "No API key found for '{}'. Set {} env var or configure a key in settings first.",
            id, env_var,
        ));
    }

    // Build the new router.
    let new_router = build_router_for_provider(&id, &base_url, &model, api_key.as_deref());

    // Swap the router atomically.
    {
        let mut router_guard = state.router.write().await;
        *router_guard = new_router;
    }

    // Update runtime state.
    *state.active_provider_id.lock().await = id.clone();
    *state.model_name.lock().await = model.clone();

    // Persist the active provider choice.
    if let Err(e) = persist_active_provider(&id) {
        warn!("Failed to persist active provider: {e}");
    }

    let name = display_name(&id);
    info!("Switched active provider to {} ({})", name, model);
    Ok(format!("Switched to {} ({})", name, model))
}

/// Persist the active provider choice to `~/.athen/models.toml`.
fn persist_active_provider(provider_id: &str) -> Result<(), String> {
    let mut models = load_models_config();
    models
        .assignments
        .insert("active_provider".to_string(), provider_id.to_string());
    save_models_config(&models)
}

/// Save general settings (security mode, etc.).
#[tauri::command]
pub async fn save_settings(
    security_mode: String,
) -> std::result::Result<String, String> {
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
// Provider-specific test functions
// ---------------------------------------------------------------------------

async fn test_ollama(
    client: &reqwest::Client,
    base_url: &str,
) -> Result<String, String> {
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

async fn test_llamacpp(
    client: &reqwest::Client,
    base_url: &str,
) -> Result<String, String> {
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

    let url = format!(
        "{}/v1/chat/completions",
        base_url.trim_end_matches('/')
    );

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

async fn test_anthropic(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    model: &str,
) -> Result<String, String> {
    if api_key.is_empty() {
        return Err("API key is required.".to_string());
    }

    let url = format!(
        "{}/v1/messages",
        base_url.trim_end_matches('/')
    );

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
