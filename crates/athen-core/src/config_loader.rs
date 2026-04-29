use std::path::Path;

use crate::config::AthenConfig;
use crate::error::{AthenError, Result};

/// Load config from a TOML file, falling back to defaults for missing fields.
pub fn load_config(path: &Path) -> Result<AthenConfig> {
    if path.exists() {
        let content = std::fs::read_to_string(path)
            .map_err(|e| AthenError::Config(format!("Failed to read {}: {e}", path.display())))?;
        let config: AthenConfig = toml::from_str(&content)
            .map_err(|e| AthenError::Config(format!("Failed to parse {}: {e}", path.display())))?;
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
}
