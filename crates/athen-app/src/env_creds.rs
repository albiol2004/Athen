//! Env-var credential overlay for headless / containerized deployments.
//!
//! Docker, Podman, Kubernetes and friends inject secrets either as plain
//! env vars or as files mounted under `/run/secrets` with the path passed
//! in an env var. Both shapes are supported: for every secret `NAME`,
//! `NAME` itself is checked first, then `NAME_FILE` (whose value is a path
//! whose trimmed contents become the secret).
//!
//! The overlay runs AFTER vault hydration and wins over it — the
//! orchestrator-injected value is the operator's explicit intent for this
//! instance, while the vault holds whatever was saved historically. On
//! desktop installs none of these vars are set and this module is a no-op.
//!
//! Recognized variables:
//!
//! | Variable | Target |
//! |---|---|
//! | `ATHEN_PROVIDER_<ID>_API_KEY` | LLM provider api_key (`<ID>` = provider id uppercased, non-alphanumerics → `_`) |
//! | `ATHEN_TELEGRAM_BOT_TOKEN` | Telegram bot token |
//! | `ATHEN_IMAP_PASSWORD` | Email IMAP password |
//! | `ATHEN_SMTP_PASSWORD` | Email SMTP password |
//! | `ATHEN_WEBSEARCH_BRAVE_API_KEY` | Brave web-search key |
//! | `ATHEN_WEBSEARCH_TAVILY_API_KEY` | Tavily web-search key |
//! | `ATHEN_EMBEDDING_API_KEY` | Cloud embedding provider key |
//!
//! Each also accepts the `_FILE` suffix form.

use athen_core::config::{AthenConfig, AuthType, ModelsConfig};

/// Inner form taking the env reader explicitly so tests never mutate
/// process env (parallel test runs race on `set_var`).
fn lookup_env_secret(name: &str, env: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    if let Some(v) = env(name) {
        if !v.is_empty() {
            return Some(v);
        }
    }
    let path = env(&format!("{name}_FILE")).filter(|p| !p.is_empty())?;
    match std::fs::read_to_string(&path) {
        Ok(s) => {
            let s = s.trim_end_matches(['\r', '\n']);
            if s.is_empty() {
                None
            } else {
                Some(s.to_string())
            }
        }
        Err(e) => {
            tracing::warn!(var = name, path, error = %e, "secret file unreadable; ignoring");
            None
        }
    }
}

/// Env var name carrying the api_key for `provider_id`
/// (`opencode_go` → `ATHEN_PROVIDER_OPENCODE_GO_API_KEY`).
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

/// Overlay every recognized secret env var onto `config`. Call after
/// `hydrate_secrets_from_vault` — env wins over vault.
pub fn overlay_secrets_from_env(config: &mut AthenConfig) {
    overlay_secrets_with(config, &|n| std::env::var(n).ok());
}

fn overlay_secrets_with(config: &mut AthenConfig, env: &dyn Fn(&str) -> Option<String>) {
    if let Some(pw) = lookup_env_secret("ATHEN_IMAP_PASSWORD", env) {
        config.email.password = pw;
    }
    if let Some(pw) = lookup_env_secret("ATHEN_SMTP_PASSWORD", env) {
        config.email.smtp_password = pw;
    }
    if let Some(token) = lookup_env_secret("ATHEN_TELEGRAM_BOT_TOKEN", env) {
        config.telegram.bot_token = token;
    }
    if let Some(key) = lookup_env_secret("ATHEN_WEBSEARCH_BRAVE_API_KEY", env) {
        config.web_search.brave_api_key = key;
    }
    if let Some(key) = lookup_env_secret("ATHEN_WEBSEARCH_TAVILY_API_KEY", env) {
        config.web_search.tavily_api_key = key;
    }
    if let Some(key) = lookup_env_secret("ATHEN_EMBEDDING_API_KEY", env) {
        config.embeddings.api_key = Some(key);
    }
    overlay_models_with(&mut config.models, env);
}

/// Overlay per-provider api_keys onto `models`. Call after
/// `hydrate_models_from_vault` on any path that re-reads `models.toml`
/// from disk — otherwise a rebuilt router loses the env-injected key.
pub fn overlay_models_from_env(models: &mut ModelsConfig) {
    overlay_models_with(models, &|n| std::env::var(n).ok());
}

fn overlay_models_with(models: &mut ModelsConfig, env: &dyn Fn(&str) -> Option<String>) {
    let ids: Vec<String> = models.providers.keys().cloned().collect();
    for id in ids {
        overlay_provider_with(models, &id, env);
    }
}

/// Single-provider form for the per-arc router rebuild path.
pub fn overlay_one_provider_from_env(models: &mut ModelsConfig, provider_id: &str) {
    overlay_provider_with(models, provider_id, &|n| std::env::var(n).ok());
}

fn overlay_provider_with(
    models: &mut ModelsConfig,
    provider_id: &str,
    env: &dyn Fn(&str) -> Option<String>,
) {
    if let Some(key) = lookup_env_secret(&provider_env_var(provider_id), env) {
        if let Some(p) = models.providers.get_mut(provider_id) {
            p.auth = AuthType::ApiKey(key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |n: &str| map.get(n).cloned()
    }

    #[test]
    fn provider_env_var_uppercases_and_sanitizes() {
        assert_eq!(
            provider_env_var("deepseek"),
            "ATHEN_PROVIDER_DEEPSEEK_API_KEY"
        );
        assert_eq!(
            provider_env_var("opencode_go"),
            "ATHEN_PROVIDER_OPENCODE_GO_API_KEY"
        );
        assert_eq!(
            provider_env_var("my-relay.v2"),
            "ATHEN_PROVIDER_MY_RELAY_V2_API_KEY"
        );
    }

    #[test]
    fn direct_var_wins_over_file_variant() {
        let td = tempfile::tempdir().unwrap();
        let f = td.path().join("secret");
        std::fs::write(&f, "from-file\n").unwrap();
        let env = env_of(&[
            ("ATHEN_X", "direct"),
            ("ATHEN_X_FILE", f.to_str().unwrap()),
        ]);
        assert_eq!(lookup_env_secret("ATHEN_X", &env).as_deref(), Some("direct"));
    }

    #[test]
    fn file_variant_reads_and_trims_trailing_newline() {
        let td = tempfile::tempdir().unwrap();
        let f = td.path().join("secret");
        std::fs::write(&f, "sk-secret\n").unwrap();
        let env = env_of(&[("ATHEN_X_FILE", f.to_str().unwrap())]);
        assert_eq!(
            lookup_env_secret("ATHEN_X", &env).as_deref(),
            Some("sk-secret")
        );
    }

    #[test]
    fn empty_and_missing_yield_none() {
        let env = env_of(&[("ATHEN_X", "")]);
        assert_eq!(lookup_env_secret("ATHEN_X", &env), None);
        assert_eq!(lookup_env_secret("ATHEN_Y", &env), None);
    }

    #[test]
    fn unreadable_secret_file_is_ignored() {
        let env = env_of(&[("ATHEN_X_FILE", "/nonexistent/athen-secret")]);
        assert_eq!(lookup_env_secret("ATHEN_X", &env), None);
    }

    #[test]
    fn overlay_patches_config_and_providers() {
        let mut config = AthenConfig::default();
        let provider: athen_core::config::ProviderConfig = serde_json::from_value(
            serde_json::json!({ "auth": "None", "default_model": "deepseek-chat" }),
        )
        .unwrap();
        config.models.providers.insert("deepseek".into(), provider);
        let env = env_of(&[
            ("ATHEN_TELEGRAM_BOT_TOKEN", "123:abc"),
            ("ATHEN_IMAP_PASSWORD", "imap-pw"),
            ("ATHEN_PROVIDER_DEEPSEEK_API_KEY", "sk-ds"),
        ]);
        overlay_secrets_with(&mut config, &env);
        assert_eq!(config.telegram.bot_token, "123:abc");
        assert_eq!(config.email.password, "imap-pw");
        assert!(matches!(
            &config.models.providers["deepseek"].auth,
            AuthType::ApiKey(k) if k == "sk-ds"
        ));
    }

    #[test]
    fn overlay_leaves_untouched_fields_alone() {
        let mut config = AthenConfig::default();
        config.telegram.bot_token = "existing".into();
        let env = env_of(&[]);
        overlay_secrets_with(&mut config, &env);
        assert_eq!(config.telegram.bot_token, "existing");
    }
}
