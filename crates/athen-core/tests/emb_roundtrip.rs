//! Regression guard for the 2026-05-28 embedding-settings persistence
//! bug. `AthenConfig.voice` defaults to `serde_json::Value::Null`, which
//! TOML rejects ("unsupported unit type"). Before the fix, every Save
//! call from any settings panel (Embedding, Calendar, Email, …) was
//! silently failing on fresh installs because TOML serialization of the
//! whole config tree blew up — only voice settings' typed shape was
//! known to be vulnerable, the broader poisoning was missed.
//!
//! These tests pin both halves: AthenConfig::default() must serialize,
//! and EmbeddingMode::Bundled must round-trip.

use athen_core::config::{AthenConfig, BundledTier, EmbeddingMode};

#[test]
fn default_athen_config_round_trips_through_toml() {
    let cfg = AthenConfig::default();
    let toml_str = toml::to_string_pretty(&cfg).expect("default must serialize");
    let _: AthenConfig = toml::from_str(&toml_str).expect("default must deserialize");
}

#[test]
fn embedding_mode_bundled_round_trips_through_toml() {
    let mut cfg = AthenConfig::default();
    cfg.embeddings.mode = EmbeddingMode::Bundled {
        tier: BundledTier::Light,
    };
    let toml_str = toml::to_string_pretty(&cfg).expect("serialize");
    let back: AthenConfig = toml::from_str(&toml_str).expect("deserialize");
    assert_eq!(
        back.embeddings.mode,
        EmbeddingMode::Bundled {
            tier: BundledTier::Light,
        }
    );
}
