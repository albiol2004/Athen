//! CalDAV adapter for Athen's [`CalendarSource`] port.
//!
//! One generic adapter that talks RFC 4791 (CalDAV) over HTTPS with HTTP
//! Basic auth. The same code handles **Apple iCloud**
//! (`https://caldav.icloud.com/`), **Google Calendar via CalDAV**
//! (`https://apidata.googleusercontent.com/caldav/v2/<email>/events/`),
//! **Fastmail**, **Nextcloud**, **Yandex**, and any RFC-compliant server,
//! because they all use the same PROPFIND/REPORT verbs and iCalendar
//! payloads.
//!
//! Authentication is always an **app-specific password** (or an
//! account password where the provider allows it). The credential is
//! taken at construction time; the adapter does not touch the vault
//! directly so it stays free of storage dependencies.
//!
//! This crate currently exposes the trait-conformant [`CalDavSource`]
//! struct but stubs the wire-protocol methods so the rest of Athen can
//! be wired up against the trait. The PROPFIND/REPORT/PUT/DELETE bodies
//! land in a follow-up commit (task #25).

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use reqwest::Client;
use url::Url;

use athen_core::error::{AthenError, Result};
use athen_core::traits::calendar_source::{
    CalendarSource, CalendarSourceCapabilities, RemoteCalendar, RemoteEvent,
};

/// One configured CalDAV account.
///
/// Construct via [`CalDavSource::new`]. The `base_url` is the CalDAV root
/// for the provider — see [`presets`] for the URL each major provider
/// expects.
pub struct CalDavSource {
    source_id: String,
    display_name: String,
    base_url: Url,
    username: String,
    app_password: String,
    http: Client,
}

impl CalDavSource {
    pub fn new(
        source_id: impl Into<String>,
        display_name: impl Into<String>,
        base_url: Url,
        username: impl Into<String>,
        app_password: impl Into<String>,
    ) -> Result<Self> {
        let http = Client::builder()
            .user_agent(concat!("Athen/", env!("CARGO_PKG_VERSION"), " (CalDAV)"))
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| AthenError::Other(format!("CalDAV HTTP client build: {e}")))?;
        Ok(Self {
            source_id: source_id.into(),
            display_name: display_name.into(),
            base_url,
            username: username.into(),
            app_password: app_password.into(),
            http,
        })
    }
}

#[async_trait]
impl CalendarSource for CalDavSource {
    fn source_id(&self) -> &str {
        &self.source_id
    }

    fn display_name(&self) -> &str {
        &self.display_name
    }

    fn capabilities(&self) -> CalendarSourceCapabilities {
        CalendarSourceCapabilities {
            read: true,
            create: true,
            update: true,
            delete: true,
            find_meeting_times: false,
        }
    }

    async fn test_connection(&self) -> Result<()> {
        Err(not_implemented("test_connection"))
    }

    async fn list_calendars(&self) -> Result<Vec<RemoteCalendar>> {
        Err(not_implemented("list_calendars"))
    }

    async fn list_events(
        &self,
        _calendar_id: &str,
        _start: DateTime<Utc>,
        _end: DateTime<Utc>,
    ) -> Result<Vec<RemoteEvent>> {
        Err(not_implemented("list_events"))
    }

    async fn create_event(
        &self,
        _calendar_id: &str,
        _event: &RemoteEvent,
    ) -> Result<(String, Option<String>)> {
        Err(not_implemented("create_event"))
    }

    async fn update_event(
        &self,
        _calendar_id: &str,
        _remote_id: &str,
        _if_match_etag: Option<&str>,
        _event: &RemoteEvent,
    ) -> Result<Option<String>> {
        Err(not_implemented("update_event"))
    }

    async fn delete_event(
        &self,
        _calendar_id: &str,
        _remote_id: &str,
        _if_match_etag: Option<&str>,
    ) -> Result<()> {
        Err(not_implemented("delete_event"))
    }
}

fn not_implemented(method: &str) -> AthenError {
    AthenError::Other(format!(
        "athen-caldav: {method} not yet wired (see task #25 — PROPFIND/REPORT/PUT/DELETE bodies + iCalendar parse)"
    ))
}

/// Suppress unused-field warnings until the wire protocol lands.
#[allow(dead_code)]
fn _silence_unused(s: &CalDavSource) {
    let _ = (&s.base_url, &s.username, &s.app_password, &s.http);
}

/// Provider presets so the Settings UI can pre-fill `base_url` once the
/// user picks "iCloud" vs "Google" vs "Fastmail" vs "Custom".
///
/// These URLs are stable per provider but kept here (not in
/// `athen-core`) because they are CalDAV-specific.
pub mod presets {
    /// Apple iCloud Calendar.
    /// Username: full Apple ID email. Password: app-specific password
    /// from appleid.apple.com → Sign-In and Security → App-Specific Passwords.
    pub const ICLOUD: &str = "https://caldav.icloud.com/";

    /// Google Calendar via CalDAV. The path includes the user's email.
    /// Athen substitutes `{email}` at config time.
    /// Username: full Gmail address. Password: 16-character app password
    /// from myaccount.google.com → Security → 2-Step Verification → App Passwords.
    pub const GOOGLE_TEMPLATE: &str =
        "https://apidata.googleusercontent.com/caldav/v2/{email}/events/";

    /// Fastmail. Username: full Fastmail address. Password: app password
    /// from fastmail.com → Settings → Privacy & Security → Integrations.
    pub const FASTMAIL: &str = "https://caldav.fastmail.com/";

    /// Yandex. Username: full Yandex address. Password: app password.
    pub const YANDEX: &str = "https://caldav.yandex.com/";

    /// Nextcloud — server-specific. User must paste their server URL;
    /// the canonical path is `/remote.php/dav/calendars/<user>/`.
    /// Stored as a hint string, not a complete URL.
    pub const NEXTCLOUD_HINT: &str =
        "https://your-nextcloud.example.com/remote.php/dav/calendars/<user>/";
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make() -> CalDavSource {
        CalDavSource::new(
            "test-id",
            "Test (alex@example.com)",
            Url::parse("https://caldav.icloud.com/").unwrap(),
            "alex@example.com",
            "abcd-efgh-ijkl-mnop",
        )
        .unwrap()
    }

    #[test]
    fn identity_fields() {
        let s = make();
        assert_eq!(s.source_id(), "test-id");
        assert_eq!(s.display_name(), "Test (alex@example.com)");
    }

    #[test]
    fn capabilities_full_rw() {
        let caps = make().capabilities();
        assert!(caps.read && caps.create && caps.update && caps.delete);
        assert!(!caps.find_meeting_times);
    }

    #[tokio::test]
    async fn stubbed_methods_return_descriptive_error() {
        let s = make();
        let err = s.test_connection().await.unwrap_err().to_string();
        assert!(err.contains("not yet wired"));
    }
}
