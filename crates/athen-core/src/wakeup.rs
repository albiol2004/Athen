//! Wake-up types: scheduled / recurring / one-shot proactive triggers.
//!
//! Wake-ups are synthetic sense events with a clock as their trigger. The
//! data layer in this module describes *what* a wake-up is; the scheduler
//! (Phase 2) decides *when* to fire it; the coordinator (Phase 3) consumes
//! the fire as a sense event.
//!
//! Risk model: pre-approve **capability** (autonomy band + tool/contact
//! allowlists set at creation), let the existing per-action risk gate run
//! at fire time on the actual call. See `docs/WAKEUPS.md`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::config::NotificationChannelKind;
use crate::contact::ContactId;

/// One scheduled trigger. The schedule decides *when* it fires; the
/// instruction decides *what* the agent does on fire.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Wakeup {
    pub id: Uuid,
    pub schedule: Schedule,
    /// Free-text instruction handed to the agent on fire, exactly like a
    /// sense event payload. Output destination is *part of the instruction*
    /// (the agent picks the right tool); `preferred_channel` is only the
    /// completion-ping channel.
    pub instruction: String,
    pub autonomy: AutonomyBand,
    /// Channel for the "this wake-up is done" notification. `None` means
    /// fall back to the user's configured default.
    pub preferred_channel: Option<NotificationChannelKind>,
    /// `None` = the agent profile's default tool surface. `Some(list)` =
    /// strict allowlist; the dispatcher MUST hide every other tool from the
    /// agent at fire time. The first injection defense.
    pub tool_allowlist: Option<Vec<String>>,
    /// `None` = profile defaults; `Some(list)` = outbound contact strict
    /// allowlist (Telegram chat ids, email recipients, etc.).
    pub contact_allowlist: Option<Vec<ContactId>>,
    /// Which agent profile runs the work.
    pub profile: String,
    /// If set, the wake-up appends to that arc. If `None`, a fresh arc is
    /// spawned per fire.
    pub arc_id: Option<Uuid>,
    pub origin: WakeupOrigin,
    pub created_at: DateTime<Utc>,
    pub last_fired_at: Option<DateTime<Utc>>,
    /// Computed by the scheduler after every fire and on process startup.
    /// `None` when the wake-up is disabled or its one-shot has already run.
    pub next_fire_at: Option<DateTime<Utc>>,
    pub enabled: bool,
}

/// When a wake-up fires.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Schedule {
    /// Fire exactly once at `at`. After firing, `next_fire_at` becomes
    /// `None` and the wake-up is effectively complete.
    OneShot { at: DateTime<Utc> },
    /// Recurring schedule. `expr` is a 5-field cron expression evaluated in
    /// `tz`. Stored as opaque string at this layer; the scheduler parses.
    Cron { expr: String, tz: String },
    /// Fire every `every_seconds`, anchored to `anchor` so the offsets are
    /// stable across restarts. Stored in seconds (not `Duration`) to keep
    /// the JSON form compact and tz-free.
    Interval {
        every_seconds: u64,
        anchor: DateTime<Utc>,
    },
}

/// How much autonomy a wake-up's fire has when the per-action risk gate
/// returns a non-zero score. The gate itself is unchanged; this band only
/// decides what to do when it trips.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AutonomyBand {
    /// Auto-execute everything except `Critical`. `Critical` always pauses.
    /// Reserved for explicitly user-trusted wake-ups.
    Auto,
    /// Auto-execute below the per-user safe threshold; pause anything at or
    /// above. Default for agent-authored wake-ups.
    SafeOnly,
    /// Outbound tools are stripped from the agent surface entirely. The
    /// wake-up can read and summarize but cannot act on the world.
    NotifyOnly,
}

impl AutonomyBand {
    pub fn as_str(&self) -> &str {
        match self {
            AutonomyBand::Auto => "auto",
            AutonomyBand::SafeOnly => "safe_only",
            AutonomyBand::NotifyOnly => "notify_only",
        }
    }

    pub fn from_str_lossy(s: &str) -> Self {
        match s {
            "auto" => AutonomyBand::Auto,
            "notify_only" => AutonomyBand::NotifyOnly,
            // Default to the safest band on unknown — never silently widen.
            _ => AutonomyBand::SafeOnly,
        }
    }
}

/// Who created the wake-up. `Agent` carries the originating arc id so the
/// UI can link back to "where did this come from?"; user-created wake-ups
/// don't need that pointer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WakeupOrigin {
    User,
    Agent { authoring_arc_id: Uuid },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedule_round_trips_through_json() {
        let cases = vec![
            Schedule::OneShot {
                at: chrono::DateTime::parse_from_rfc3339("2026-06-01T08:00:00Z")
                    .unwrap()
                    .with_timezone(&Utc),
            },
            Schedule::Cron {
                expr: "0 8 * * *".into(),
                tz: "UTC".into(),
            },
            Schedule::Interval {
                every_seconds: 3600,
                anchor: Utc::now(),
            },
        ];
        for s in cases {
            let j = serde_json::to_string(&s).unwrap();
            let back: Schedule = serde_json::from_str(&j).unwrap();
            assert_eq!(s, back);
        }
    }

    #[test]
    fn autonomy_band_round_trips_via_str() {
        for b in [
            AutonomyBand::Auto,
            AutonomyBand::SafeOnly,
            AutonomyBand::NotifyOnly,
        ] {
            assert_eq!(AutonomyBand::from_str_lossy(b.as_str()), b);
        }
    }

    #[test]
    fn autonomy_band_unknown_falls_back_to_safest() {
        // Defense in depth — a future enum variant we forgot to handle
        // must not silently widen autonomy.
        assert_eq!(
            AutonomyBand::from_str_lossy("never_seen_before"),
            AutonomyBand::SafeOnly
        );
    }

    #[test]
    fn origin_user_serializes_without_extra_fields() {
        let j = serde_json::to_string(&WakeupOrigin::User).unwrap();
        // Tag-only; no authoring_arc_id key on User.
        assert!(j.contains("\"kind\":\"user\""));
        assert!(!j.contains("authoring_arc_id"));
    }

    #[test]
    fn origin_agent_carries_arc_id() {
        let arc_id = Uuid::new_v4();
        let o = WakeupOrigin::Agent {
            authoring_arc_id: arc_id,
        };
        let j = serde_json::to_string(&o).unwrap();
        let back: WakeupOrigin = serde_json::from_str(&j).unwrap();
        assert_eq!(o, back);
    }
}
