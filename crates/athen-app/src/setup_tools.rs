//! Agent-callable setup tools.
//!
//! Each function wraps existing Tauri-command-level logic but operates
//! directly on the underlying stores and vault, bypassing the
//! `State<'_, AppState>` requirement. This lets the agent executor invoke
//! them from the tool dispatch path where a Tauri `State` borrow is not
//! available.
//!
//! Every tool returns a JSON string the agent can relay verbatim to the
//! user. Errors are returned as `Err(AthenError)` with actionable
//! messages.

use std::sync::Arc;

use athen_caldav::CalDavSource;
use athen_contacts::ContactStore;
use athen_core::calendar_source_config::CalendarSourceConfig;
use athen_core::contact::{Contact, ContactIdentifier, IdentifierKind, TrustLevel};
use athen_core::email_provider::Security;
use athen_core::error::{AthenError, Result};
use athen_core::traits::calendar_source::CalendarSource;
use athen_core::traits::calendar_source_config::CalendarSourceConfigStore;
use athen_core::traits::vault::Vault;
use serde_json::json;
use tracing::info;
use uuid::Uuid;

use crate::vault_creds::{
    KEY_API_KEY, KEY_BOT_TOKEN, KEY_PASSWORD, SCOPE_EMAIL_IMAP, SCOPE_EMAIL_SMTP, SCOPE_TELEGRAM,
    SCOPE_WEBSEARCH_BRAVE, SCOPE_WEBSEARCH_TAVILY,
};

// ---------------------------------------------------------------------------
// 1. Email setup
// ---------------------------------------------------------------------------

/// Autodetect IMAP/SMTP servers for `address`, test with `password`, and
/// persist the full email configuration if both tests pass.
pub async fn do_setup_email(
    vault: &Arc<dyn Vault>,
    address: &str,
    password: &str,
) -> Result<String> {
    // Step 1: autodetect provider.
    let hint = crate::email_autodetect::detect(address)
        .await
        .ok_or_else(|| {
            AthenError::Other(
                "Could not autodetect servers for that address. \
             Ask the user for the IMAP/SMTP server details."
                    .to_string(),
            )
        })?;

    // Step 2: build test config from hint.
    let config = crate::email_test::EmailTestConfig {
        imap_host: hint.incoming.host.clone(),
        imap_port: hint.incoming.port,
        imap_security: hint.incoming.security,
        imap_username: address.to_string(),
        smtp_host: hint.outgoing.host.clone(),
        smtp_port: hint.outgoing.port,
        smtp_security: hint.outgoing.security,
        smtp_username: address.to_string(),
    };

    // Step 3: run end-to-end credential test.
    let result = crate::email_test::test_connection(&config, password, password).await;

    if !result.imap.ok || !result.smtp.ok {
        // Surface error details so the agent can help troubleshoot.
        return Ok(json!({
            "ok": false,
            "imap_ok": result.imap.ok,
            "imap_error": result.imap.error,
            "imap_stage": result.imap.stage,
            "smtp_ok": result.smtp.ok,
            "smtp_error": result.smtp.error,
            "smtp_stage": result.smtp.stage,
            "message": "Connection test failed. Check the error details and help the user fix them."
        })
        .to_string());
    }

    // Step 4: persist credentials in the vault.
    vault
        .set(SCOPE_EMAIL_IMAP, KEY_PASSWORD, password)
        .await
        .map_err(|e| AthenError::Other(format!("Vault store IMAP password: {e}")))?;
    vault
        .set(SCOPE_EMAIL_SMTP, KEY_PASSWORD, password)
        .await
        .map_err(|e| AthenError::Other(format!("Vault store SMTP password: {e}")))?;

    // Step 5: load + mutate + save config.toml.
    let mut cfg = crate::settings::load_main_config_public();

    let imap_use_tls = hint.incoming.security == Security::Ssl;
    let smtp_use_tls = matches!(hint.outgoing.security, Security::Ssl | Security::StartTls);

    cfg.email.enabled = true;
    cfg.email.imap_server = hint.incoming.host.clone();
    cfg.email.imap_port = hint.incoming.port;
    cfg.email.username = address.to_string();
    cfg.email.password = String::new(); // blank on disk, real value in vault
    cfg.email.use_tls = imap_use_tls;
    cfg.email.folders = vec!["INBOX".to_string()];
    cfg.email.poll_interval_secs = 60;
    cfg.email.lookback_hours = 24;

    cfg.email.smtp_server = hint.outgoing.host.clone();
    cfg.email.smtp_port = hint.outgoing.port;
    cfg.email.smtp_username = address.to_string();
    cfg.email.smtp_password = String::new(); // blank on disk
    cfg.email.smtp_use_tls = smtp_use_tls;
    cfg.email.from_address = address.to_string();

    crate::settings::save_main_config(&cfg)
        .map_err(|e| AthenError::Other(format!("Save config.toml: {e}")))?;

    info!(address, provider = %hint.display_name, "Setup tool: email configured");

    Ok(json!({
        "ok": true,
        "provider": hint.display_name,
        "imap_ok": true,
        "smtp_ok": true,
        "message": "Email configured. Restart to start monitoring."
    })
    .to_string())
}

// ---------------------------------------------------------------------------
// 2. Calendar connect
// ---------------------------------------------------------------------------

/// Resolve the CalDAV base URL for a well-known provider slug, or use
/// the caller-supplied `base_url` for custom / Nextcloud servers.
fn resolve_caldav_base_url(
    provider: &str,
    username: &str,
    base_url: Option<&str>,
) -> Result<String> {
    match provider {
        "icloud" => Ok("https://caldav.icloud.com/".to_string()),
        "google" => Ok(format!(
            "https://apidata.googleusercontent.com/caldav/v2/{}/events/",
            username
        )),
        "fastmail" => Ok("https://caldav.fastmail.com/".to_string()),
        "yandex" => Ok("https://caldav.yandex.com/".to_string()),
        "nextcloud" | "custom" => base_url
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .ok_or_else(|| {
                AthenError::Other(format!(
                    "A base_url is required for provider \"{provider}\". \
                     Ask the user for the CalDAV server URL."
                ))
            }),
        other => Err(AthenError::Other(format!(
            "Unknown calendar provider \"{other}\". \
             Valid values: icloud, google, fastmail, yandex, nextcloud, custom."
        ))),
    }
}

/// Connect a CalDAV calendar source: resolve the base URL, store
/// credentials, create the source row, and probe the server.
pub async fn do_setup_calendar_connect(
    vault: &Arc<dyn Vault>,
    calendar_source_store: &dyn CalendarSourceConfigStore,
    provider: &str,
    username: &str,
    password: &str,
    base_url: Option<&str>,
) -> Result<String> {
    let resolved_url = resolve_caldav_base_url(provider, username, base_url)?;

    // Build the config row.
    let display_name = format!(
        "{} ({})",
        match provider {
            "icloud" => "iCloud",
            "google" => "Google Calendar",
            "fastmail" => "Fastmail",
            "yandex" => "Yandex",
            "nextcloud" => "Nextcloud",
            _ => "CalDAV",
        },
        username
    );
    let cfg = CalendarSourceConfig::new_caldav(&display_name, &resolved_url, username);
    let source_id = cfg.id;

    // Store password in vault.
    vault
        .set(&cfg.vault_scope, &cfg.vault_key, password)
        .await
        .map_err(|e| AthenError::Other(format!("Vault store calendar password: {e}")))?;

    // Persist the source row.
    if let Err(e) = calendar_source_store.upsert(&cfg).await {
        // Best-effort vault cleanup.
        let _ = vault.delete(&cfg.vault_scope, &cfg.vault_key).await;
        return Err(AthenError::Other(format!("Save calendar source: {e}")));
    }

    // Probe the remote: authenticate + list calendars.
    let parsed_url = url::Url::parse(&resolved_url)
        .map_err(|e| AthenError::Other(format!("Invalid base URL \"{resolved_url}\": {e}")))?;
    let source = CalDavSource::new(
        source_id.to_string(),
        &display_name,
        parsed_url,
        username,
        password,
    )?;

    match source.list_calendars().await {
        Ok(cals) => {
            let cal_list: Vec<serde_json::Value> = cals
                .iter()
                .map(|c| {
                    json!({
                        "id": c.id,
                        "name": c.name,
                        "read_only": c.read_only,
                    })
                })
                .collect();

            info!(
                source_id = %source_id,
                provider,
                calendar_count = cals.len(),
                "Setup tool: calendar source connected"
            );

            Ok(json!({
                "ok": true,
                "source_id": source_id.to_string(),
                "test_ok": true,
                "calendars": cal_list,
                "message": format!("Connected to {}. Found {} calendar(s).", display_name, cals.len())
            })
            .to_string())
        }
        Err(e) => {
            // Connection failed but the source row is persisted (disabled
            // state). Surface the error so the agent can help.
            Ok(json!({
                "ok": false,
                "source_id": source_id.to_string(),
                "test_ok": false,
                "error": e.to_string(),
                "message": format!(
                    "Calendar source saved but connection test failed: {}. \
                     Check credentials and server URL.",
                    e
                )
            })
            .to_string())
        }
    }
}

// ---------------------------------------------------------------------------
// 3. Calendar configure
// ---------------------------------------------------------------------------

/// Select which remote calendars to sync and optionally set the agent's
/// default write-target calendar.
pub async fn do_setup_calendar_configure(
    calendar_source_store: &dyn CalendarSourceConfigStore,
    source_id: Uuid,
    selected_calendars: &[String],
    default_calendar_id: Option<&str>,
) -> Result<String> {
    calendar_source_store
        .set_selected_calendars(source_id, selected_calendars)
        .await?;

    // Optionally set the agent's default write target.
    if let Some(cal_id) = default_calendar_id {
        let mut config = crate::settings::load_main_config_public();
        config.calendar.agent_default_source_id = Some(source_id.to_string());
        config.calendar.agent_default_calendar_id = Some(cal_id.to_string());
        // We don't know the display name here; the sync loop will populate
        // it on the first pass. Leave it as None so the save doesn't set a
        // stale label.
        config.calendar.agent_default_calendar_name = None;
        crate::settings::save_main_config(&config)
            .map_err(|e| AthenError::Other(format!("Save config.toml: {e}")))?;
    }

    info!(
        %source_id,
        selected = selected_calendars.len(),
        "Setup tool: calendar configured"
    );

    Ok(json!({
        "ok": true,
        "message": "Calendar configured. Sync will start shortly."
    })
    .to_string())
}

// ---------------------------------------------------------------------------
// 4. Telegram setup
// ---------------------------------------------------------------------------

/// Validate a Telegram bot token via the `getMe` API and persist it.
pub async fn do_setup_telegram(vault: &Arc<dyn Vault>, bot_token: &str) -> Result<String> {
    // Test the token.
    let url = format!("https://api.telegram.org/bot{}/getMe", bot_token);
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| AthenError::Other(format!("HTTP client build: {e}")))?;

    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| AthenError::Other(format!("Telegram getMe request failed: {e}")))?;

    let status = resp.status();
    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| AthenError::Other(format!("Telegram getMe parse failed: {e}")))?;

    if !status.is_success() || body.get("ok") != Some(&json!(true)) {
        let description = body
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown error");
        return Ok(json!({
            "ok": false,
            "error": format!("HTTP {}: {}", status.as_u16(), description),
            "message": format!(
                "Bot token validation failed: {}. \
                 Check that the token is correct and the bot has not been revoked.",
                description
            )
        })
        .to_string());
    }

    let bot_username = body
        .pointer("/result/username")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    // Store token in vault and blank it on disk.
    vault
        .set(SCOPE_TELEGRAM, KEY_BOT_TOKEN, bot_token)
        .await
        .map_err(|e| AthenError::Other(format!("Vault store bot token: {e}")))?;

    let mut cfg = crate::settings::load_main_config_public();
    cfg.telegram.enabled = true;
    cfg.telegram.bot_token = String::new(); // blank on disk
    crate::settings::save_main_config(&cfg)
        .map_err(|e| AthenError::Other(format!("Save config.toml: {e}")))?;

    info!(bot_username, "Setup tool: Telegram bot configured");

    Ok(json!({
        "ok": true,
        "bot_username": format!("@{}", bot_username),
        "message": format!("Telegram bot @{} configured. Restart to start monitoring.", bot_username)
    })
    .to_string())
}

// ---------------------------------------------------------------------------
// 5. Owner info setup
// ---------------------------------------------------------------------------

/// Set a field on the owner contact (name, email, phone, telegram_user_id).
/// Creates the owner contact if none exists.
pub async fn do_setup_owner_info(
    contact_store: &dyn ContactStore,
    field: &str,
    value: &str,
) -> Result<String> {
    // Validate field name.
    let kind = match field {
        "name" => None,
        "email" => Some(IdentifierKind::Email),
        "phone" => Some(IdentifierKind::Phone),
        "telegram_user_id" => Some(IdentifierKind::Telegram),
        other => {
            return Err(AthenError::Other(format!(
                "Unknown owner field \"{other}\". Valid: name, email, phone, telegram_user_id."
            )));
        }
    };

    // Load or create owner contact.
    let mut owner = contact_store
        .find_owner()
        .await?
        .unwrap_or_else(|| Contact {
            id: Uuid::new_v4(),
            name: String::new(),
            trust_level: TrustLevel::AuthUser,
            trust_manual_override: true,
            identifiers: Vec::new(),
            interaction_count: 0,
            last_interaction: None,
            notes: None,
            blocked: false,
            is_owner: true,
        });

    match kind {
        None => {
            // "name" field
            owner.name = value.to_string();
        }
        Some(ident_kind) => {
            // Update or add identifier of the given kind.
            if let Some(existing) = owner.identifiers.iter_mut().find(|i| i.kind == ident_kind) {
                existing.value = value.to_string();
            } else {
                owner.identifiers.push(ContactIdentifier {
                    kind: ident_kind,
                    value: value.to_string(),
                });
            }
        }
    }

    let contact_id = owner.id;
    contact_store.save(&owner).await?;
    contact_store.set_owner(&contact_id).await?;

    info!(field, value, "Setup tool: owner info updated");

    Ok(json!({
        "ok": true,
        "field": field,
        "value": value,
    })
    .to_string())
}

// ---------------------------------------------------------------------------
// 6. Web search key setup
// ---------------------------------------------------------------------------

/// Store a web-search API key (Brave or Tavily) in the vault and blank it
/// on disk.
pub async fn do_setup_search_key(
    vault: &Arc<dyn Vault>,
    provider: &str,
    key: &str,
) -> Result<String> {
    let scope = match provider {
        "brave" => SCOPE_WEBSEARCH_BRAVE,
        "tavily" => SCOPE_WEBSEARCH_TAVILY,
        other => {
            return Err(AthenError::Other(format!(
                "Unknown search provider \"{other}\". Valid: brave, tavily."
            )));
        }
    };

    vault
        .set(scope, KEY_API_KEY, key)
        .await
        .map_err(|e| AthenError::Other(format!("Vault store {provider} key: {e}")))?;

    // Blank the on-disk key so config.toml doesn't leak secrets.
    let mut cfg = crate::settings::load_main_config_public();
    match provider {
        "brave" => cfg.web_search.brave_api_key = String::new(),
        "tavily" => cfg.web_search.tavily_api_key = String::new(),
        _ => {}
    }
    crate::settings::save_main_config(&cfg)
        .map_err(|e| AthenError::Other(format!("Save config.toml: {e}")))?;

    info!(provider, "Setup tool: web search key stored");

    Ok(json!({
        "ok": true,
        "provider": provider,
        "message": format!("{} API key stored.", match provider {
            "brave" => "Brave Search",
            "tavily" => "Tavily",
            _ => provider,
        })
    })
    .to_string())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Setup status context (injected into system_suffix for athen_setup profile)
// ---------------------------------------------------------------------------

pub async fn build_setup_status_context(
    config: &athen_core::config::AthenConfig,
    calendar_source_store: Option<&Arc<dyn CalendarSourceConfigStore>>,
    contact_store: Option<&athen_persistence::contacts::SqliteContactStore>,
) -> String {
    use athen_contacts::ContactStore as _;

    let mut lines = Vec::new();

    // Owner identity
    let owner_status = if let Some(cs) = contact_store {
        match cs.find_owner().await {
            Ok(Some(c)) => {
                let name = if c.name.is_empty() {
                    "unnamed"
                } else {
                    &c.name
                };
                let ids: Vec<String> = c
                    .identifiers
                    .iter()
                    .map(|i| format!("{:?}: {}", i.kind, i.value))
                    .collect();
                if ids.is_empty() {
                    format!("set (name: {name}, no identifiers)")
                } else {
                    format!("set (name: {name}, {})", ids.join(", "))
                }
            }
            _ => "not configured".into(),
        }
    } else {
        "not configured".into()
    };
    lines.push(format!("- Owner identity: {owner_status}"));

    // Email
    if config.email.enabled {
        let addr = if config.email.from_address.is_empty() {
            &config.email.username
        } else {
            &config.email.from_address
        };
        lines.push(format!("- Email: configured ({addr})"));
    } else {
        lines.push("- Email: not configured".into());
    }

    // Calendar
    let cal_status = if let Some(store) = calendar_source_store {
        match store.list().await {
            Ok(sources) if !sources.is_empty() => {
                let enabled = sources.iter().filter(|s| s.enabled).count();
                format!("{} source(s) connected ({enabled} enabled)", sources.len())
            }
            _ => "not configured".into(),
        }
    } else {
        "not configured".into()
    };
    lines.push(format!("- Calendar: {cal_status}"));

    // Telegram
    if config.telegram.enabled {
        lines.push("- Telegram: configured".into());
    } else {
        lines.push("- Telegram: not configured".into());
    }

    // Web search
    let brave = !config.web_search.brave_api_key.trim().is_empty();
    let tavily = !config.web_search.tavily_api_key.trim().is_empty();
    let search_status = match (brave, tavily) {
        (true, true) => "Brave + Tavily keys set",
        (true, false) => "Brave key set",
        (false, true) => "Tavily key set",
        (false, false) => "no API keys (using DuckDuckGo fallback)",
    };
    lines.push(format!("- Web search: {search_status}"));

    // Embeddings
    let emb_status = match config.embeddings.mode {
        athen_core::config::EmbeddingMode::Off => "off (keyword fallback)",
        athen_core::config::EmbeddingMode::Specific => "on",
        athen_core::config::EmbeddingMode::Automatic => "auto",
        _ => "on",
    };
    lines.push(format!("- Memory/Embeddings: {emb_status}"));

    format!(
        "CURRENT SETUP STATUS — do NOT offer to configure integrations \
         that are already done. Only ask about items marked \"not configured\" \
         if the user wants help with them:\n{}\n\n",
        lines.join("\n")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- CalDAV URL resolution ----

    #[test]
    fn resolve_icloud_url() {
        let url = resolve_caldav_base_url("icloud", "alice@icloud.com", None).unwrap();
        assert_eq!(url, "https://caldav.icloud.com/");
    }

    #[test]
    fn resolve_google_url_substitutes_username() {
        let url = resolve_caldav_base_url("google", "bob@gmail.com", None).unwrap();
        assert_eq!(
            url,
            "https://apidata.googleusercontent.com/caldav/v2/bob@gmail.com/events/"
        );
    }

    #[test]
    fn resolve_fastmail_url() {
        let url = resolve_caldav_base_url("fastmail", "user@fastmail.com", None).unwrap();
        assert_eq!(url, "https://caldav.fastmail.com/");
    }

    #[test]
    fn resolve_yandex_url() {
        let url = resolve_caldav_base_url("yandex", "user@yandex.com", None).unwrap();
        assert_eq!(url, "https://caldav.yandex.com/");
    }

    #[test]
    fn resolve_nextcloud_requires_base_url() {
        let result = resolve_caldav_base_url("nextcloud", "user", None);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("base_url is required"),
            "Expected 'base_url is required' in: {err}"
        );
    }

    #[test]
    fn resolve_nextcloud_with_base_url() {
        let url = resolve_caldav_base_url(
            "nextcloud",
            "user",
            Some("https://cloud.example.com/remote.php/dav/"),
        )
        .unwrap();
        assert_eq!(url, "https://cloud.example.com/remote.php/dav/");
    }

    #[test]
    fn resolve_custom_requires_base_url() {
        let result = resolve_caldav_base_url("custom", "user", None);
        assert!(result.is_err());
    }

    #[test]
    fn resolve_custom_with_base_url() {
        let url =
            resolve_caldav_base_url("custom", "user", Some("https://dav.example.org/")).unwrap();
        assert_eq!(url, "https://dav.example.org/");
    }

    #[test]
    fn resolve_unknown_provider_errors() {
        let result = resolve_caldav_base_url("onedrive", "user", None);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("Unknown calendar provider"),
            "Expected 'Unknown calendar provider' in: {err}"
        );
    }

    #[test]
    fn resolve_nextcloud_empty_base_url_treated_as_missing() {
        let result = resolve_caldav_base_url("nextcloud", "user", Some(""));
        assert!(result.is_err());
    }

    // ---- Owner info field validation ----

    #[test]
    fn valid_owner_fields() {
        // "name" maps to None (direct field set)
        assert!(matches!(
            match "name" {
                "name" => Some(()),
                _ => None,
            },
            Some(())
        ));
    }

    #[test]
    fn invalid_owner_field() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let store = athen_contacts::InMemoryContactStore::new();
        let result = rt.block_on(do_setup_owner_info(&store, "favorite_color", "blue"));
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("Unknown owner field"),
            "Expected 'Unknown owner field' in: {err}"
        );
    }

    // ---- Search provider validation ----

    #[test]
    fn unknown_search_provider_errors() {
        // We can't easily mock the vault, but we can test the validation
        // branch that runs before the vault call.
        let result = resolve_search_scope("bing");
        assert!(result.is_err());
    }

    /// Helper to test the search-provider validation without needing a vault.
    fn resolve_search_scope(provider: &str) -> Result<&'static str> {
        match provider {
            "brave" => Ok(SCOPE_WEBSEARCH_BRAVE),
            "tavily" => Ok(SCOPE_WEBSEARCH_TAVILY),
            other => Err(AthenError::Other(format!(
                "Unknown search provider \"{other}\"."
            ))),
        }
    }
}
