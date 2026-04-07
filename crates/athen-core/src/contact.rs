use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub type ContactId = Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Contact {
    pub id: ContactId,
    pub name: String,
    pub trust_level: TrustLevel,
    pub trust_manual_override: bool,
    pub identifiers: Vec<ContactIdentifier>,
    pub interaction_count: u32,
    pub last_interaction: Option<DateTime<Utc>>,
    pub notes: Option<String>,
    pub blocked: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContactIdentifier {
    pub kind: IdentifierKind,
    pub value: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum IdentifierKind {
    Email,
    Phone,
    Telegram,
    WhatsApp,
    IMessage,
    Signal,
    Discord,
    Slack,
    Twitter,
    Username,
    Other,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub enum TrustLevel {
    /// T0: Unknown sender, 5.0x risk multiplier
    Unknown = 0,
    /// T1: In contacts but no history, 2.0x
    Neutral = 1,
    /// T2: Known with positive history, 1.5x
    Known = 2,
    /// T3: Explicitly trusted, 1.0x
    Trusted = 3,
    /// T4: Authenticated user, 0.5x
    AuthUser = 4,
}

impl TrustLevel {
    /// Returns the risk origin multiplier (M_origen) for this trust level.
    pub fn risk_multiplier(&self) -> f64 {
        match self {
            TrustLevel::Unknown => 5.0,
            TrustLevel::Neutral => 2.0,
            TrustLevel::Known => 1.5,
            TrustLevel::Trusted => 1.0,
            TrustLevel::AuthUser => 0.5,
        }
    }
}
