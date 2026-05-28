//! Voice subsystem types — VoiceConfig, providers, vault scope helpers.
//!
//! Pure types + helpers. The `place_call` tool lives in a separate batch;
//! the `pipecat_runtime` submodule implements the install pipeline for
//! the Python-side runner.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

pub mod pipecat_runtime;

// ---------------------------------------------------------------------------
// place_call approval gate
// ---------------------------------------------------------------------------

/// One outbound call about to be placed. Handed to the
/// [`TelephonyApprovalGate`] before the runner is spawned. Carries the
/// full set of fields the user needs to decide: who's being called, the
/// objective, the call-cost estimate, the voice/LLM/stack picks. Mirrors
/// [`athen_agent::tools::EmailSendSummary`] in spirit — same fail-closed
/// approval surface, same arc-aware routing through the cross-channel
/// ApprovalRouter (InApp + Telegram with escalation).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallRequest {
    pub arc_id: uuid::Uuid,
    /// E.164 destination — the resolved number we will actually dial.
    /// For `called_party = User` callers, this is the user's own
    /// `user_number` from `VoiceConfig`, not the original `number` arg.
    pub to_number: String,
    pub objective: String,
    pub called_party: CalledParty,
    pub voice_id: Option<String>,
    pub max_duration_s: u32,
    /// Pre-call dollar estimate at the requested `max_duration_s` cap.
    /// Computed via [`estimate_call_cost_usd`].
    pub est_cost_usd: f64,
    /// Human-readable LLM picker label, e.g.
    /// `"DeepSeek :: deepseek-chat"`. Shown verbatim in the approval
    /// dialog so the user sees which model will hold the conversation.
    pub llm_label: String,
    pub voice_provider: String,
    pub stt_provider: String,
    pub phone_provider: String,
}

/// Who is being called.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CalledParty {
    /// Calling the user themselves — typically a reminder. Destination
    /// number is auto-substituted with [`VoiceConfig::user_number`].
    User,
    /// Calling an external party (restaurant, hotel, service desk…).
    Other,
}

/// Approval gate consulted before the `place_call` tool spawns the
/// Pipecat runner. Mirrors `EmailSendApprovalGate`: implementations
/// surface a confirmation prompt and return `true` only on Approve.
/// Every other outcome (deny, timeout, transport error) returns `false`
/// so calls fail closed.
#[async_trait]
pub trait TelephonyApprovalGate: Send + Sync {
    /// Returns `true` only when the user approves the call.
    async fn confirm_call(&self, request: &CallRequest) -> bool;
}

// ---------------------------------------------------------------------------
// Cost estimation
// ---------------------------------------------------------------------------

/// Per-minute aggregate cost in USD for a typical Twilio + Deepgram +
/// ElevenLabs/Cartesia stack, with an LLM surcharge applied. Mirrors
/// `_cost_estimate` in `pipecat_runner.py` so the pre-call estimate the
/// approval dialog shows matches the actual after-the-fact figure the
/// runner emits in its final `result` event.
///
/// Numbers are conservative published rates (USD):
/// * Twilio outbound:  $0.014 / min
/// * Deepgram STT:     $0.0058 / min
/// * ElevenLabs / Cartesia TTS: ~$0.05 / min
/// * LLM surcharge:    +30 % on top of the above three lines
pub const TWILIO_USD_PER_MIN: f64 = 0.014;
pub const DEEPGRAM_USD_PER_MIN: f64 = 0.0058;
pub const TTS_USD_PER_MIN: f64 = 0.05;
pub const LLM_SURCHARGE_FACTOR: f64 = 1.30;

/// Returns the pre-call cost estimate (USD) for a call capped at
/// `max_duration_s` seconds. Pure function — kept testable.
pub fn estimate_call_cost_usd(max_duration_s: u32) -> f64 {
    let per_minute = TWILIO_USD_PER_MIN + DEEPGRAM_USD_PER_MIN + TTS_USD_PER_MIN;
    let voice_cost = (max_duration_s as f64 / 60.0) * per_minute;
    let total = voice_cost * LLM_SURCHARGE_FACTOR;
    (total * 100.0).round() / 100.0
}

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
    #[error("pipecat install failed: {0}")]
    PipecatInstallFailed(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
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
    fn cost_estimate_math_is_sensible() {
        // 10-minute call: per-minute total = .014 + .0058 + .05 = .0698
        // voice cost = 10 * .0698 = .698
        // total with 30% LLM surcharge = .698 * 1.3 = .9074 → rounds to .91
        let cost = estimate_call_cost_usd(600);
        assert!(
            (cost - 0.91).abs() < 0.005,
            "expected ~$0.91 for 10min, got ${cost}"
        );

        // 1-minute call should round to ~$0.09 (.0698 * 1.3 = .09074).
        let one_min = estimate_call_cost_usd(60);
        assert!(
            (one_min - 0.09).abs() < 0.005,
            "expected ~$0.09 for 1min, got ${one_min}"
        );

        // Zero duration costs nothing.
        assert!((estimate_call_cost_usd(0) - 0.0).abs() < 1e-9);

        // Hard cap (1800s = 30min) should be reasonable, never NaN/inf.
        let cap = estimate_call_cost_usd(VoiceConfig::HARD_MAX_DURATION_S);
        assert!(cap.is_finite() && cap > 0.0);
    }

    #[test]
    fn called_party_serde_lowercase() {
        let user = serde_json::to_value(CalledParty::User).unwrap();
        assert_eq!(user, serde_json::json!("user"));
        let other = serde_json::to_value(CalledParty::Other).unwrap();
        assert_eq!(other, serde_json::json!("other"));
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
