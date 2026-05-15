//! CalDAV server discovery: principal URL → calendar home set → list of
//! calendar collections.
//!
//! RFC 6764 well-known URL bootstrap (`/.well-known/caldav`) is handled
//! transparently — reqwest follows the redirect that servers like iCloud
//! return. We just always start with whatever base URL the user gave us
//! and let `current-user-principal` resolve to the canonical principal.

use reqwest::header::HeaderValue;
use reqwest::Client;
use url::Url;

use athen_core::error::{AthenError, Result};
use athen_core::traits::calendar_source::RemoteCalendar;

use crate::client::{
    propfind, PROPFIND_CALENDAR_HOME_SET, PROPFIND_CALENDAR_LIST, PROPFIND_CURRENT_USER_PRINCIPAL,
};
use crate::multistatus::parse_multistatus;

/// Resolve the principal URL for the authenticated user.
pub async fn discover_principal(http: &Client, auth: &HeaderValue, base: &Url) -> Result<Url> {
    let body = propfind(http, base, auth, "0", PROPFIND_CURRENT_USER_PRINCIPAL).await?;
    let entries = parse_multistatus(&body)?;
    let href = entries
        .into_iter()
        .find_map(|e| e.current_user_principal_href)
        .ok_or_else(|| {
            AthenError::Other(format!(
                "CalDAV discovery: no current-user-principal at {base}"
            ))
        })?;
    resolve_href(base, &href)
}

/// Resolve the calendar-home-set URL from a principal URL.
pub async fn discover_calendar_home(
    http: &Client,
    auth: &HeaderValue,
    principal: &Url,
) -> Result<Url> {
    let body = propfind(http, principal, auth, "0", PROPFIND_CALENDAR_HOME_SET).await?;
    let entries = parse_multistatus(&body)?;
    let href = entries
        .into_iter()
        .find_map(|e| e.calendar_home_set_href)
        .ok_or_else(|| {
            AthenError::Other(format!(
                "CalDAV discovery: no calendar-home-set at {principal}"
            ))
        })?;
    resolve_href(principal, &href)
}

/// List calendar collections under a home set. Filters by `<C:calendar/>`
/// in `<D:resourcetype>` so subscribed-but-non-calendar collections
/// (notebooks, address books) don't slip through.
pub async fn list_calendar_collections(
    http: &Client,
    auth: &HeaderValue,
    home: &Url,
) -> Result<Vec<RemoteCalendar>> {
    let body = propfind(http, home, auth, "1", PROPFIND_CALENDAR_LIST).await?;
    let entries = parse_multistatus(&body)?;
    let mut out = Vec::new();
    for e in entries {
        if !e.resource_types.iter().any(|n| n == "calendar") {
            continue;
        }
        let Some(href) = e.href else { continue };
        // Use the absolute URL as the stable id — opaque to the rest of Athen,
        // round-tripped on subsequent calls.
        let id = resolve_href(home, &href)?.to_string();
        let name = e.displayname.unwrap_or_else(|| {
            href.trim_end_matches('/')
                .rsplit('/')
                .next()
                .unwrap_or("Calendar")
                .to_string()
        });
        out.push(RemoteCalendar {
            id,
            name,
            color: e.calendar_color,
            // CalDAV does not advertise read-only at the collection level via
            // the props we ask for. Pessimistic v1: assume writable. A future
            // pass can PROPFIND `current-user-privilege-set` for accuracy.
            read_only: false,
        });
    }
    Ok(out)
}

/// Join an `href` (which may be absolute or path-relative) against a base URL.
pub fn resolve_href(base: &Url, href: &str) -> Result<Url> {
    base.join(href.trim())
        .map_err(|e| AthenError::Other(format!("Resolve href `{href}` against {base}: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_relative_path() {
        let base = Url::parse("https://caldav.icloud.com/123/principal/").unwrap();
        let resolved = resolve_href(&base, "/123/calendars/").unwrap();
        assert_eq!(
            resolved.as_str(),
            "https://caldav.icloud.com/123/calendars/"
        );
    }

    #[test]
    fn resolve_absolute_url() {
        let base = Url::parse("https://caldav.icloud.com/").unwrap();
        let resolved = resolve_href(&base, "https://p01-caldav.icloud.com/abc/principal/").unwrap();
        assert_eq!(
            resolved.as_str(),
            "https://p01-caldav.icloud.com/abc/principal/"
        );
    }
}
