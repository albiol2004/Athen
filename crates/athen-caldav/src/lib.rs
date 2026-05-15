//! CalDAV adapter for Athen's [`CalendarSource`] port.
//!
//! One generic adapter that talks RFC 4791 (CalDAV) over HTTPS with HTTP
//! Basic auth + an app-specific password. The same code handles **Apple
//! iCloud** (`https://caldav.icloud.com/`), **Google Calendar via CalDAV**
//! (`https://apidata.googleusercontent.com/caldav/v2/<email>/events/`),
//! **Fastmail**, **Nextcloud**, **Yandex**, and any RFC-compliant server,
//! because they all use the same PROPFIND/REPORT verbs and iCalendar
//! payloads.
//!
//! Discovery flow on first contact:
//!
//! 1. PROPFIND `current-user-principal` against the user-supplied base URL.
//! 2. PROPFIND `calendar-home-set` against the resolved principal.
//! 3. Depth=1 PROPFIND against the home set to enumerate calendar collections.
//!
//! Steps 1–2 run lazily on first need and the resulting home-set URL is
//! cached on the struct so subsequent operations skip the round-trips.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use reqwest::header::HeaderValue;
use reqwest::Client;
use tokio::sync::Mutex;
use url::Url;

use athen_core::error::{AthenError, Result};
use athen_core::traits::calendar_source::{
    CalendarSource, CalendarSourceCapabilities, RemoteCalendar, RemoteEvent,
};

mod client;
mod discovery;
mod ical_codec;
mod multistatus;

pub use ical_codec::{emit_vcalendar, parse_vcalendar};

/// One configured CalDAV account.
pub struct CalDavSource {
    source_id: String,
    display_name: String,
    base_url: Url,
    auth: HeaderValue,
    http: Client,
    /// Cached calendar-home-set URL discovered on first use.
    home_set: Arc<Mutex<Option<Url>>>,
}

impl CalDavSource {
    pub fn new(
        source_id: impl Into<String>,
        display_name: impl Into<String>,
        base_url: Url,
        username: impl AsRef<str>,
        app_password: impl AsRef<str>,
    ) -> Result<Self> {
        let http = Client::builder()
            .user_agent(concat!("Athen/", env!("CARGO_PKG_VERSION"), " (CalDAV)"))
            .timeout(std::time::Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()
            .map_err(|e| AthenError::Other(format!("CalDAV HTTP client build: {e}")))?;
        let auth = client::basic_auth_header(username.as_ref(), app_password.as_ref());
        Ok(Self {
            source_id: source_id.into(),
            display_name: display_name.into(),
            base_url,
            auth,
            http,
            home_set: Arc::new(Mutex::new(None)),
        })
    }

    async fn calendar_home(&self) -> Result<Url> {
        {
            let guard = self.home_set.lock().await;
            if let Some(u) = guard.as_ref() {
                return Ok(u.clone());
            }
        }
        let principal =
            discovery::discover_principal(&self.http, &self.auth, &self.base_url).await?;
        let home = discovery::discover_calendar_home(&self.http, &self.auth, &principal).await?;
        let mut guard = self.home_set.lock().await;
        *guard = Some(home.clone());
        Ok(home)
    }

    fn parse_calendar_id(&self, calendar_id: &str) -> Result<Url> {
        Url::parse(calendar_id).map_err(|e| {
            AthenError::Other(format!(
                "Invalid calendar_id `{calendar_id}` for source {}: {e}",
                self.source_id
            ))
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
        // Discovery itself is the auth probe — a 401 surfaces from
        // `propfind` as a descriptive error.
        let _ = self.calendar_home().await?;
        Ok(())
    }

    async fn list_calendars(&self) -> Result<Vec<RemoteCalendar>> {
        let home = self.calendar_home().await?;
        discovery::list_calendar_collections(&self.http, &self.auth, &home).await
    }

    async fn list_events(
        &self,
        calendar_id: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<Vec<RemoteEvent>> {
        let cal_url = self.parse_calendar_id(calendar_id)?;
        let body = client::build_calendar_query(
            &start.format("%Y%m%dT%H%M%SZ").to_string(),
            &end.format("%Y%m%dT%H%M%SZ").to_string(),
        );
        let xml = client::report(&self.http, &cal_url, &self.auth, "1", &body).await?;
        let entries = multistatus::parse_multistatus(&xml)?;
        let mut out = Vec::new();
        for e in entries {
            let Some(data) = e.calendar_data else {
                continue;
            };
            let Some(href) = e.href.clone() else { continue };
            // The remote_id is the object href (relative to the home),
            // resolved to an absolute URL so the sync loop can PUT/DELETE
            // against it later without re-resolving.
            let object_url = discovery::resolve_href(&cal_url, &href)?.to_string();
            match parse_vcalendar(&data, calendar_id, &object_url, e.etag) {
                Ok(events) => out.extend(events),
                Err(err) => {
                    tracing::warn!(href, ?err, "CalDAV: skipping unparseable VEVENT");
                }
            }
        }
        Ok(out)
    }

    async fn create_event(
        &self,
        calendar_id: &str,
        event: &RemoteEvent,
    ) -> Result<(String, Option<String>)> {
        let cal_url = self.parse_calendar_id(calendar_id)?;
        // Allocate a fresh object URL under the calendar collection.
        let uid = event
            .ical_uid
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let object_url = cal_url
            .join(&format!("{uid}.ics"))
            .map_err(|e| AthenError::Other(format!("Build event URL: {e}")))?;
        let ics = emit_vcalendar(&RemoteEvent {
            ical_uid: Some(uid.clone()),
            ..event.clone()
        });
        let etag =
            client::put_ical(&self.http, &object_url, &self.auth, &ics, None, Some("*")).await?;
        Ok((object_url.to_string(), etag))
    }

    async fn update_event(
        &self,
        _calendar_id: &str,
        remote_id: &str,
        if_match_etag: Option<&str>,
        event: &RemoteEvent,
    ) -> Result<Option<String>> {
        let url = Url::parse(remote_id)
            .map_err(|e| AthenError::Other(format!("Invalid remote_id `{remote_id}`: {e}")))?;
        let ics = emit_vcalendar(event);
        client::put_ical(&self.http, &url, &self.auth, &ics, if_match_etag, None).await
    }

    async fn delete_event(
        &self,
        _calendar_id: &str,
        remote_id: &str,
        if_match_etag: Option<&str>,
    ) -> Result<()> {
        let url = Url::parse(remote_id)
            .map_err(|e| AthenError::Other(format!("Invalid remote_id `{remote_id}`: {e}")))?;
        client::delete(&self.http, &url, &self.auth, if_match_etag).await
    }
}

/// Provider presets so the Settings UI can pre-fill `base_url` once the
/// user picks "iCloud" vs "Google" vs "Fastmail" vs "Custom".
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
}
