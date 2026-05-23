use std::path::Path;

use crate::config::AthenConfig;
use crate::error::{AthenError, Result};

/// Load config from a TOML file, falling back to defaults for missing fields.
///
/// Applies in-process legacy-id migrations on the deserialised config —
/// see [`AthenConfig::migrate_legacy_provider_ids`]. Migrations are pure
/// (no I/O, no logging) so the caller decides whether/how to surface
/// them.
pub fn load_config(path: &Path) -> Result<AthenConfig> {
    if path.exists() {
        let content = std::fs::read_to_string(path)
            .map_err(|e| AthenError::Config(format!("Failed to read {}: {e}", path.display())))?;
        let mut config: AthenConfig = toml::from_str(&content)
            .map_err(|e| AthenError::Config(format!("Failed to parse {}: {e}", path.display())))?;
        let _ = config.migrate_legacy_provider_ids();
        Ok(config)
    } else {
        Ok(AthenConfig::default())
    }
}

/// Load config from a directory containing multiple TOML files.
///
/// Looks for:
/// - `config.toml` — main configuration (operation, security, persistence)
/// - `models.toml` — LLM provider configuration (overrides `models` section)
pub fn load_config_dir(dir: &Path) -> Result<AthenConfig> {
    let mut config = if dir.join("config.toml").exists() {
        load_config(&dir.join("config.toml"))?
    } else {
        AthenConfig::default()
    };

    // Override models if models.toml exists
    if dir.join("models.toml").exists() {
        let content = std::fs::read_to_string(dir.join("models.toml"))
            .map_err(|e| AthenError::Config(format!("Failed to read models.toml: {e}")))?;
        config.models = toml::from_str(&content)
            .map_err(|e| AthenError::Config(format!("Failed to parse models.toml: {e}")))?;
    }

    // Apply legacy-id migrations after both files have been merged so the
    // assignments + providers maps see their final shape (e.g. an active-
    // provider set in config.toml + a providers map loaded from
    // models.toml).
    let _ = config.migrate_legacy_provider_ids();

    Ok(config)
}

/// Save default config to a file (for first-run setup).
pub fn save_default_config(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.exists() {
            std::fs::create_dir_all(parent).map_err(|e| {
                AthenError::Config(format!(
                    "Failed to create directory {}: {e}",
                    parent.display()
                ))
            })?;
        }
    }

    let config = AthenConfig::default();
    let content = toml::to_string_pretty(&config)
        .map_err(|e| AthenError::Config(format!("Failed to serialize config: {e}")))?;
    std::fs::write(path, content)
        .map_err(|e| AthenError::Config(format!("Failed to write {}: {e}", path.display())))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AuthType, OperationMode, ProfileConfig, ProviderConfig, SecurityMode};
    use std::collections::HashMap;
    use tempfile::TempDir;

    #[test]
    fn test_load_nonexistent_returns_defaults() {
        let path = Path::new("/tmp/this_file_does_not_exist_athen_test.toml");
        let config = load_config(path).unwrap();
        assert_eq!(config.operation.mode, OperationMode::AlwaysOn);
        assert_eq!(config.security.mode, SecurityMode::Assistant);
        assert_eq!(config.security.auto_approve_below, 20);
        assert_eq!(config.persistence.checkpoint_interval_secs, 30);
    }

    #[test]
    fn test_load_valid_config() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        let content = r#"
workspace_path = "/home/test/.athen"

[operation]
mode = "WakeTimer"
wake_interval_minutes = 15

[models]
[models.providers]
[models.profiles]
[models.assignments]

[security]
mode = "Bunker"
auto_approve_below = 10
max_steps_per_task = 30
max_task_duration_minutes = 3

[persistence]
db_path = "custom/path.db"
checkpoint_interval_secs = 60
completed_retention_days = 14
"#;
        std::fs::write(&path, content).unwrap();

        let config = load_config(&path).unwrap();
        assert_eq!(config.operation.mode, OperationMode::WakeTimer);
        assert_eq!(config.operation.wake_interval_minutes, Some(15));
        assert_eq!(config.security.mode, SecurityMode::Bunker);
        assert_eq!(config.security.auto_approve_below, 10);
        assert_eq!(config.persistence.checkpoint_interval_secs, 60);
        assert_eq!(config.persistence.completed_retention_days, 14);
    }

    #[test]
    fn test_load_invalid_toml_returns_error() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("bad.toml");
        std::fs::write(&path, "this is not valid { toml [[").unwrap();

        let result = load_config(&path);
        assert!(result.is_err());
        let err = format!("{}", result.unwrap_err());
        assert!(err.contains("Failed to parse"));
    }

    #[test]
    fn test_save_and_reload_roundtrip() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("roundtrip.toml");

        save_default_config(&path).unwrap();
        assert!(path.exists());

        let config = load_config(&path).unwrap();
        assert_eq!(config.operation.mode, OperationMode::AlwaysOn);
        assert_eq!(config.security.mode, SecurityMode::Assistant);
        assert_eq!(config.security.max_steps_per_task, 50);
        assert_eq!(config.persistence.completed_retention_days, 7);
        // Default domains should be present
        assert!(config.domains.contains_key("base"));
        assert!(config.domains.contains_key("code"));
        assert!(config.domains.contains_key("communication"));
    }

    #[test]
    fn test_load_config_dir_main_only() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        save_default_config(&path).unwrap();

        let config = load_config_dir(dir.path()).unwrap();
        assert_eq!(config.operation.mode, OperationMode::AlwaysOn);
        assert!(config.models.providers.is_empty());
    }

    #[test]
    fn test_load_config_dir_with_models() {
        let dir = TempDir::new().unwrap();

        // Write main config
        let main_path = dir.path().join("config.toml");
        save_default_config(&main_path).unwrap();

        // Write models config
        let models_toml = r#"
[providers.deepseek]
auth = { ApiKey = "sk-test-key" }
default_model = "deepseek-chat"

[profiles.fast]
description = "Quick responses"
priority = ["deepseek"]

[assignments]
base = "fast"
"#;
        std::fs::write(dir.path().join("models.toml"), models_toml).unwrap();

        let config = load_config_dir(dir.path()).unwrap();
        assert!(config.models.providers.contains_key("deepseek"));
        assert!(config.models.profiles.contains_key("fast"));
        assert_eq!(
            config.models.assignments.get("base"),
            Some(&"fast".to_string())
        );
    }

    #[test]
    fn test_load_config_dir_empty_uses_defaults() {
        let dir = TempDir::new().unwrap();
        let config = load_config_dir(dir.path()).unwrap();
        assert_eq!(config.operation.mode, OperationMode::AlwaysOn);
        assert_eq!(config.security.mode, SecurityMode::Assistant);
    }

    /// A user who has only configured an LLM provider via Settings writes
    /// `models.toml` but not `config.toml`. Loading must still surface the
    /// provider keys — otherwise the router starts up with an empty
    /// providers map and authenticated requests fail.
    #[test]
    fn test_load_config_dir_models_only_no_config_toml() {
        let dir = TempDir::new().unwrap();
        let models_toml = r#"
[providers.deepseek]
auth = { ApiKey = "sk-test-key" }
default_model = "deepseek-chat"
"#;
        std::fs::write(dir.path().join("models.toml"), models_toml).unwrap();

        let config = load_config_dir(dir.path()).unwrap();
        assert!(config.models.providers.contains_key("deepseek"));
        match &config.models.providers["deepseek"].auth {
            crate::config::AuthType::ApiKey(key) => assert_eq!(key, "sk-test-key"),
            _ => panic!("expected ApiKey auth"),
        }
        // Other config defaults to baseline values.
        assert_eq!(config.security.mode, SecurityMode::Assistant);
    }

    #[test]
    fn test_save_creates_parent_dirs() {
        let dir = TempDir::new().unwrap();
        let nested = dir.path().join("a").join("b").join("config.toml");

        save_default_config(&nested).unwrap();
        assert!(nested.exists());
    }

    #[test]
    fn test_models_config_with_providers() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("full.toml");

        let mut config = AthenConfig::default();
        config.models.providers.insert(
            "anthropic".into(),
            ProviderConfig {
                auth: AuthType::ApiKey("sk-ant-test".into()),
                default_model: "claude-sonnet-4".into(),
                endpoint: None,
                context_window_tokens: 200_000,
                compaction_trigger_pct: 65,
                compaction_target_pct: 30,
                supports_vision: false,
                supports_documents: false,
                family: crate::llm::ModelFamily::Default,
                temperature: None,
                tier_models: HashMap::new(),
            },
        );
        config.models.profiles.insert(
            "powerful".into(),
            ProfileConfig {
                description: "Complex reasoning".into(),
                priority: vec!["anthropic".into()],
                fallback: None,
            },
        );
        config
            .models
            .assignments
            .insert("code".into(), "powerful".into());

        let content = toml::to_string_pretty(&config).unwrap();
        std::fs::write(&path, &content).unwrap();

        let loaded = load_config(&path).unwrap();
        assert!(loaded.models.providers.contains_key("anthropic"));
        match &loaded.models.providers["anthropic"].auth {
            AuthType::ApiKey(key) => assert_eq!(key, "sk-ant-test"),
            _ => panic!("Expected ApiKey auth"),
        }
    }

    /// An existing user has the legacy `opencode_go_anthropic` provider
    /// id pinned as the active provider, AND both legacy + unified
    /// entries in their providers map with mixed tier slugs across the
    /// two wire formats. After load, the active id rewrites to
    /// `opencode_go`, the legacy entry vanishes, and the tier_models map
    /// merges with Anthropic-entry slugs winning on collision.
    #[test]
    fn test_load_migrates_opencode_go_anthropic() {
        use crate::config::ProviderConfig;
        use crate::llm::{ModelFamily, ModelProfile};

        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.toml");

        let mut config = AthenConfig::default();

        // Both legacy and unified provider rows present. Collision on
        // Cheap (different slugs) — Anthropic-entry slug should win.
        let mut legacy_tiers: HashMap<ModelProfile, String> = HashMap::new();
        legacy_tiers.insert(ModelProfile::Cheap, "minimax-m2.5".into());
        legacy_tiers.insert(ModelProfile::Powerful, "minimax-m2.7".into());

        let mut unified_tiers: HashMap<ModelProfile, String> = HashMap::new();
        unified_tiers.insert(ModelProfile::Cheap, "deepseek-v4-flash".into());
        unified_tiers.insert(ModelProfile::Code, "deepseek-v4-pro".into());

        config.models.providers.insert(
            "opencode_go_anthropic".into(),
            ProviderConfig {
                auth: AuthType::ApiKey("sk-test".into()),
                default_model: "minimax-m2.7".into(),
                endpoint: None,
                context_window_tokens: 200_000,
                compaction_trigger_pct: 65,
                compaction_target_pct: 30,
                supports_vision: false,
                supports_documents: false,
                family: ModelFamily::MiniMaxM25Cloud,
                temperature: None,
                tier_models: legacy_tiers,
            },
        );
        config.models.providers.insert(
            "opencode_go".into(),
            ProviderConfig {
                auth: AuthType::ApiKey("sk-test".into()),
                default_model: "deepseek-v4-flash".into(),
                endpoint: None,
                context_window_tokens: 200_000,
                compaction_trigger_pct: 65,
                compaction_target_pct: 30,
                supports_vision: false,
                supports_documents: false,
                family: ModelFamily::DeepSeekV4Chat,
                temperature: None,
                tier_models: unified_tiers,
            },
        );
        config
            .models
            .assignments
            .insert("active_provider".into(), "opencode_go_anthropic".into());

        let content = toml::to_string_pretty(&config).unwrap();
        std::fs::write(&path, &content).unwrap();

        let loaded = load_config(&path).unwrap();

        // active_provider rewritten.
        assert_eq!(
            loaded.models.assignments.get("active_provider"),
            Some(&"opencode_go".to_string())
        );
        // Legacy entry gone.
        assert!(!loaded
            .models
            .providers
            .contains_key("opencode_go_anthropic"));
        // Unified entry survives.
        let unified = loaded
            .models
            .providers
            .get("opencode_go")
            .expect("opencode_go must survive merge");
        // Anthropic-entry Cheap slug wins on collision.
        assert_eq!(
            unified.tier_models.get(&ModelProfile::Cheap),
            Some(&"minimax-m2.5".to_string())
        );
        // Anthropic-entry Powerful slug copied across.
        assert_eq!(
            unified.tier_models.get(&ModelProfile::Powerful),
            Some(&"minimax-m2.7".to_string())
        );
        // Unified-entry Code slug preserved (no collision).
        assert_eq!(
            unified.tier_models.get(&ModelProfile::Code),
            Some(&"deepseek-v4-pro".to_string())
        );
        // Family normalised to DeepSeekV4Chat regardless of inbound value.
        assert!(matches!(unified.family, ModelFamily::DeepSeekV4Chat));
    }

    /// Migration is idempotent — running load on an already-migrated
    /// config is a no-op.
    #[test]
    fn test_load_migration_idempotent_when_no_legacy() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.toml");

        let mut config = AthenConfig::default();
        config
            .models
            .assignments
            .insert("active_provider".into(), "deepseek".into());
        let content = toml::to_string_pretty(&config).unwrap();
        std::fs::write(&path, &content).unwrap();

        let loaded = load_config(&path).unwrap();
        assert_eq!(
            loaded.models.assignments.get("active_provider"),
            Some(&"deepseek".to_string())
        );
    }
}
