//! Configuration row for a calendar source registered in the Settings UI.
//!
//! Distinct from [`crate::traits::calendar_source::CalendarSource`] —
//! the trait is the runtime adapter, this struct is the persisted config
//! that the composition root reads on startup and feeds to a per-kind
//! factory that builds the adapter.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Discriminator for which adapter to instantiate. New providers
/// (Microsoft Graph, native Google API) add a variant here and a match
/// arm in the factory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CalendarSourceKind {
    /// Generic CalDAV server — handles iCloud, Google-via-CalDAV,
    /// Fastmail, Nextcloud, Yandex, custom servers.
    Caldav,
}

impl CalendarSourceKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Caldav => "caldav",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "caldav" => Some(Self::Caldav),
            _ => None,
        }
    }
}

/// One persisted calendar-source configuration. Credentials live in the
/// vault under `(vault_scope, vault_key)` — this struct only carries
/// the pointer, never the secret.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalendarSourceConfig {
    pub id: Uuid,
    pub kind: CalendarSourceKind,
    /// User-facing label, e.g. "iCloud (alex@me.com)".
    pub display_name: String,
    /// Provider-specific base URL. For CalDAV this is the discovery
    /// root (e.g. `https://caldav.icloud.com/`).
    pub base_url: String,
    /// User identifier — typically the email address.
    pub username: String,
    /// Vault scope holding the password. Convention: `calendar:<uuid>`.
    pub vault_scope: String,
    /// Vault key under that scope. Convention: `password`.
    pub vault_key: String,
    /// When false the sync loop skips this source. Adapter is not
    /// constructed at all — saves background work.
    pub enabled: bool,
    /// Remote calendar IDs the user picked in Settings → "which calendars
    /// to sync?". Empty list ⇒ pull every calendar the source exposes.
    pub selected_calendars: Vec<String>,
    /// Seconds between sync passes. 300 (5 min) is a sensible default
    /// for poll-only servers — iCloud, Google, Fastmail handle it
    /// without rate-limit complaints.
    pub sync_interval_secs: u64,
    /// Last successful sync. `None` until the first pass completes.
    pub last_sync_at: Option<DateTime<Utc>>,
    /// Last error message from the sync loop, cleared on success.
    /// Surfaces in the Settings panel so users can see "iCloud auth
    /// expired" without needing to dig in logs.
    pub last_sync_error: Option<String>,
    pub created_at: DateTime<Utc>,
}

impl CalendarSourceConfig {
    /// Build a fresh row for a newly-configured source. The caller is
    /// expected to write the password to the vault under
    /// `(vault_scope, vault_key)` immediately after.
    pub fn new_caldav(
        display_name: impl Into<String>,
        base_url: impl Into<String>,
        username: impl Into<String>,
    ) -> Self {
        let id = Uuid::new_v4();
        Self {
            id,
            kind: CalendarSourceKind::Caldav,
            display_name: display_name.into(),
            base_url: base_url.into(),
            username: username.into(),
            vault_scope: format!("calendar:{id}"),
            vault_key: "password".to_string(),
            enabled: true,
            selected_calendars: Vec::new(),
            sync_interval_secs: 300,
            last_sync_at: None,
            last_sync_error: None,
            created_at: Utc::now(),
        }
    }
}
