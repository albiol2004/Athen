//! Voice subsystem types — VoiceConfig, providers, vault scope helpers.
//!
//! Pure types + helpers. Subprocess orchestration (pipecat_runtime) and
//! the place_call tool live in separate batches; this crate is the contract.

use serde::{Deserialize, Serialize};

/// Persisted voice configuration. Stored as a single JSON row in the
/// settings table — voice_config.
///
/// Endpoint references point at rows in the Cloud APIs registered_endpoints
/// table (the user manages credentials there; we never duplicate them).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceConfig {
    /// Registered Cloud APIs endpoint id for the STT provider. None until configured.
    pub stt_endpoint_id: Option<String>,
    /// Registered Cloud APIs endpoint id for the TTS provider.
    pub tts_endpoint_id: Option<String>,
    /// Registered Cloud APIs endpoint id for the phone-service provider (Twilio).
    pub phone_endpoint_id: Option<String>,

    /// TTS-specific voice ID (e.g. ElevenLabs voice ID, Cartesia UUID).
    pub voice_id: Option<String>,
    /// E.164 number the call is placed FROM (must be a number purchased on phone provider).
    pub from_number: Option<String>,
    /// E.164 number for the user themselves — used by reminder calls (called_party="user").
    pub user_number: Option<String>,

    /// Override the Fast-tier LLM for voice calls. None = use the resolved Fast tier.
    /// (connection_id, slug) tuple split into two fields for serde simplicity.
    pub llm_override_connection_id: Option<String>,
    pub llm_override_slug: Option<String>,

    /// Hard cap on call length, seconds. Default 600; enforced cap 1800.
    pub max_call_duration_s: u32,
}

impl VoiceConfig {
    pub const DEFAULT_MAX_DURATION_S: u32 = 600;
    pub const HARD_MAX_DURATION_S: u32 = 1800;

    pub fn new() -> Self {
        Self {
            max_call_duration_s: Self::DEFAULT_MAX_DURATION_S,
            ..Default::default()
        }
    }

    /// Clamp duration to the hard cap, return whether it was clamped.
    pub fn clamp_duration(seconds: u32) -> (u32, bool) {
        if seconds == 0 {
            (Self::DEFAULT_MAX_DURATION_S, true)
        } else if seconds > Self::HARD_MAX_DURATION_S {
            (Self::HARD_MAX_DURATION_S, true)
        } else {
            (seconds, false)
        }
    }

    /// Returns true if the three required wiring fields are set.
    /// (voice_id, from_number, user_number are optional at config-save time.)
    pub fn is_minimally_configured(&self) -> bool {
        self.stt_endpoint_id.is_some()
            && self.tts_endpoint_id.is_some()
            && self.phone_endpoint_id.is_some()
    }

    /// List which of the three required endpoints are still missing.
    /// Used by the Voice panel to render "Set up X first" hints.
    pub fn missing_endpoints(&self) -> Vec<VoiceProviderKind> {
        let mut missing = Vec::new();
        if self.stt_endpoint_id.is_none() {
            missing.push(VoiceProviderKind::Stt);
        }
        if self.tts_endpoint_id.is_none() {
            missing.push(VoiceProviderKind::Tts);
        }
        if self.phone_endpoint_id.is_none() {
            missing.push(VoiceProviderKind::Phone);
        }
        missing
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VoiceProviderKind {
    Stt,
    Tts,
    Phone,
}

impl VoiceProviderKind {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Stt => "Speech-to-Text",
            Self::Tts => "Text-to-Speech",
            Self::Phone => "Phone service",
        }
    }
}

/// Errors specific to the voice subsystem.
#[derive(Debug, thiserror::Error)]
pub enum VoiceError {
    #[error("voice config not set up — missing: {0:?}")]
    NotConfigured(Vec<VoiceProviderKind>),
    #[error("invalid phone number (must be E.164, e.g. +14155551234): {0}")]
    InvalidPhoneNumber(String),
    #[error("max_duration_s out of range (1..={max}): {got}", max = VoiceConfig::HARD_MAX_DURATION_S)]
    DurationOutOfRange { got: u32 },
}

/// Very loose E.164 sanity check — does NOT validate the number is reachable,
/// just rejects obvious garbage. Real validation happens at the Twilio call site.
pub fn validate_e164(number: &str) -> Result<(), VoiceError> {
    let trimmed = number.trim();
    if !trimmed.starts_with('+') {
        return Err(VoiceError::InvalidPhoneNumber(format!(
            "{number} (must start with +)"
        )));
    }
    let digits: String = trimmed.chars().filter(|c| c.is_ascii_digit()).collect();
    if !(7..=15).contains(&digits.len()) {
        return Err(VoiceError::InvalidPhoneNumber(format!(
            "{number} (need 7–15 digits, got {})",
            digits.len()
        )));
    }
    Ok(())
}

// --------------------------------------------------------------------------
// Tests
// --------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_max_duration_set() {
        let cfg = VoiceConfig::new();
        assert_eq!(cfg.max_call_duration_s, VoiceConfig::DEFAULT_MAX_DURATION_S);
    }

    #[test]
    fn missing_endpoints_lists_all_three_when_empty() {
        let cfg = VoiceConfig::new();
        let missing = cfg.missing_endpoints();
        assert_eq!(missing.len(), 3);
        assert!(missing.contains(&VoiceProviderKind::Stt));
        assert!(missing.contains(&VoiceProviderKind::Tts));
        assert!(missing.contains(&VoiceProviderKind::Phone));
    }

    #[test]
    fn is_minimally_configured_only_when_all_three_set() {
        let mut cfg = VoiceConfig::new();
        assert!(!cfg.is_minimally_configured());
        cfg.stt_endpoint_id = Some("a".into());
        cfg.tts_endpoint_id = Some("b".into());
        assert!(!cfg.is_minimally_configured());
        cfg.phone_endpoint_id = Some("c".into());
        assert!(cfg.is_minimally_configured());
    }

    #[test]
    fn clamp_duration_replaces_zero_with_default() {
        let (d, clamped) = VoiceConfig::clamp_duration(0);
        assert_eq!(d, VoiceConfig::DEFAULT_MAX_DURATION_S);
        assert!(clamped);
    }

    #[test]
    fn clamp_duration_clamps_to_hard_cap() {
        let (d, clamped) = VoiceConfig::clamp_duration(9999);
        assert_eq!(d, VoiceConfig::HARD_MAX_DURATION_S);
        assert!(clamped);
    }

    #[test]
    fn clamp_duration_passes_valid_through() {
        let (d, clamped) = VoiceConfig::clamp_duration(120);
        assert_eq!(d, 120);
        assert!(!clamped);
    }

    #[test]
    fn validate_e164_accepts_valid() {
        assert!(validate_e164("+14155551234").is_ok());
        assert!(validate_e164("+447911123456").is_ok());
        assert!(validate_e164("+34 612 345 678").is_ok()); // spaces tolerated
    }

    #[test]
    fn validate_e164_rejects_no_plus() {
        assert!(validate_e164("14155551234").is_err());
    }

    #[test]
    fn validate_e164_rejects_too_short() {
        assert!(validate_e164("+123").is_err());
    }

    #[test]
    fn voice_config_serde_camelcase() {
        let mut cfg = VoiceConfig::new();
        cfg.stt_endpoint_id = Some("abc".into());
        cfg.user_number = Some("+14155551234".into());
        let json = serde_json::to_value(&cfg).unwrap();
        assert!(json.get("sttEndpointId").is_some());
        assert!(json.get("userNumber").is_some());
        // ensure NOT snake_case
        assert!(json.get("stt_endpoint_id").is_none());
    }
}
