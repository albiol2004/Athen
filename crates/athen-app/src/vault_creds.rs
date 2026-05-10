//! Centralised scope/key conventions for credentials stored in the vault.
//!
//! Every credential migrated out of `config.toml` lives here so the
//! convention stays consistent across save commands and build paths.
//! Pattern: each credential pairs a `pub const SCOPE_…` with a `pub const
//! KEY_…`, plus a `read_or_legacy` helper that reads vault first, falls
//! back to the legacy `config.toml` value, so installs that haven't
//! re-saved their creds yet keep working without intervention.
//!
//! Migration semantics: `save_*_settings` writes to the vault and BLANKS
//! the corresponding `config.toml` field on disk — that way a config
//! file ever exfiltrated post-migration carries no secret material.

use std::sync::Arc;

use athen_core::traits::vault::Vault;

pub const SCOPE_PROVIDER: &str = "provider";
pub const SCOPE_EMAIL_IMAP: &str = "email:imap";
pub const SCOPE_EMAIL_SMTP: &str = "email:smtp";
pub const SCOPE_TELEGRAM: &str = "telegram";
pub const SCOPE_WEBSEARCH_BRAVE: &str = "websearch:brave";
pub const SCOPE_WEBSEARCH_TAVILY: &str = "websearch:tavily";
pub const SCOPE_EMBEDDING: &str = "embedding";

pub const KEY_API_KEY: &str = "api_key";
pub const KEY_PASSWORD: &str = "password";
pub const KEY_BOT_TOKEN: &str = "bot_token";

/// Per-provider scope: each LLM provider id (`deepseek`, `openai`, …)
/// gets its own scope under `provider:<id>` so a future provider rename
/// is a one-line change.
pub fn provider_scope(provider_id: &str) -> String {
    format!("{SCOPE_PROVIDER}:{provider_id}")
}

/// Per-registered-HTTP-endpoint scope used by the `http_request` tool:
/// `endpoint:<uuid>`. The UUID (not the human name) is the stable key so
/// renaming an endpoint in the UI doesn't strand its credential.
pub fn endpoint_scope(endpoint_id: uuid::Uuid) -> String {
    format!("endpoint:{endpoint_id}")
}

/// Patch every credential field in `config` from the vault.
///
/// For each known secret, if the vault holds a non-empty value it
/// overrides whatever was in the config. Empty vault entries (or no
/// vault) leave the field alone, so legacy installs whose secrets
/// still live in `config.toml` keep working until the user re-saves.
///
/// Provider api_keys are hydrated for every entry in
/// `models.providers` — the vault scope is per-provider, so DeepSeek
/// and OpenAI keys never collide. Web-search keys handle Brave and
/// Tavily individually.
///
/// Call once, right after `open_vault` returns, before anything that
/// builds clients off `config` (router, email_sender, web_search,
/// telegram launcher, …).
pub async fn hydrate_secrets_from_vault(
    vault: Option<&Arc<dyn Vault>>,
    config: &mut athen_core::config::AthenConfig,
) {
    let Some(v) = vault else {
        return;
    };
    // IMAP password.
    if let Ok(Some(pw)) = v.get(SCOPE_EMAIL_IMAP, KEY_PASSWORD).await {
        if !pw.is_empty() {
            config.email.password = pw;
        }
    }
    // SMTP password.
    if let Ok(Some(pw)) = v.get(SCOPE_EMAIL_SMTP, KEY_PASSWORD).await {
        if !pw.is_empty() {
            config.email.smtp_password = pw;
        }
    }
    // Telegram bot token.
    if let Ok(Some(token)) = v.get(SCOPE_TELEGRAM, KEY_BOT_TOKEN).await {
        if !token.is_empty() {
            config.telegram.bot_token = token;
        }
    }
    // Web search keys.
    if let Ok(Some(key)) = v.get(SCOPE_WEBSEARCH_BRAVE, KEY_API_KEY).await {
        if !key.is_empty() {
            config.web_search.brave_api_key = key;
        }
    }
    if let Ok(Some(key)) = v.get(SCOPE_WEBSEARCH_TAVILY, KEY_API_KEY).await {
        if !key.is_empty() {
            config.web_search.tavily_api_key = key;
        }
    }
    // Embedding api_key (cloud-mode OpenAI-compatible provider).
    if let Ok(Some(key)) = v.get(SCOPE_EMBEDDING, KEY_API_KEY).await {
        if !key.is_empty() {
            config.embeddings.api_key = Some(key);
        }
    }
    // Per-provider api_keys. Walk every provider in models.providers and
    // promote the vault value into AuthType::ApiKey when present.
    let provider_ids: Vec<String> = config.models.providers.keys().cloned().collect();
    for id in provider_ids {
        let scope = provider_scope(&id);
        if let Ok(Some(key)) = v.get(&scope, KEY_API_KEY).await {
            if !key.is_empty() {
                if let Some(p) = config.models.providers.get_mut(&id) {
                    p.auth = athen_core::config::AuthType::ApiKey(key);
                }
            }
        }
    }
}
