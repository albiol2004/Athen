//! Low-level CalDAV HTTP helpers — PROPFIND, REPORT, PUT, DELETE with
//! HTTP Basic auth.
//!
//! Lives one level below the [`CalendarSource`] trait impl. Knows about
//! HTTP and request bodies but nothing about iCalendar or the
//! application's calendar event types.

use base64::Engine;
use reqwest::header::{HeaderMap, HeaderValue, CONTENT_TYPE};
use reqwest::{Client, Method, Response, StatusCode};
use url::Url;

use athen_core::error::{AthenError, Result};

/// XML media type used in CalDAV request bodies.
const CONTENT_XML: &str = "application/xml; charset=utf-8";

/// iCalendar media type used in PUT bodies.
const CONTENT_ICAL: &str = "text/calendar; charset=utf-8";

/// HTTP Basic credentials header value (`Basic base64(user:pass)`).
pub fn basic_auth_header(username: &str, password: &str) -> HeaderValue {
    let raw = format!("{username}:{password}");
    let b64 = base64::engine::general_purpose::STANDARD.encode(raw.as_bytes());
    HeaderValue::from_str(&format!("Basic {b64}")).expect("ASCII base64 fits in HeaderValue")
}

/// Run a PROPFIND with the given XML body, returning the response body
/// on a 207 Multi-Status. Anything else is mapped to an error containing
/// the status code and a body excerpt.
pub async fn propfind(
    http: &Client,
    url: &Url,
    auth: &HeaderValue,
    depth: &str,
    body: &str,
) -> Result<String> {
    let method = Method::from_bytes(b"PROPFIND")
        .map_err(|e| AthenError::Other(format!("PROPFIND method: {e}")))?;
    request_xml(http, method, url, auth, Some(depth), body).await
}

/// Run a REPORT with the given XML body.
pub async fn report(
    http: &Client,
    url: &Url,
    auth: &HeaderValue,
    depth: &str,
    body: &str,
) -> Result<String> {
    let method = Method::from_bytes(b"REPORT")
        .map_err(|e| AthenError::Other(format!("REPORT method: {e}")))?;
    request_xml(http, method, url, auth, Some(depth), body).await
}

async fn request_xml(
    http: &Client,
    method: Method,
    url: &Url,
    auth: &HeaderValue,
    depth: Option<&str>,
    body: &str,
) -> Result<String> {
    let mut headers = HeaderMap::new();
    headers.insert(reqwest::header::AUTHORIZATION, auth.clone());
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(CONTENT_XML));
    if let Some(d) = depth {
        headers.insert(
            "Depth",
            HeaderValue::from_str(d)
                .map_err(|e| AthenError::Other(format!("Depth header: {e}")))?,
        );
    }
    let resp = http
        .request(method.clone(), url.clone())
        .headers(headers)
        .body(body.to_string())
        .send()
        .await
        .map_err(|e| AthenError::Other(format!("CalDAV {method} {url} send: {e}")))?;
    handle_response(resp, &format!("{method} {url}")).await
}

/// PUT an iCalendar object at `url`. Set `if_match` to an existing ETag
/// for safe updates; `if_none_match` to `"*"` for creates that must not
/// overwrite. Returns the new ETag if the server returned one.
pub async fn put_ical(
    http: &Client,
    url: &Url,
    auth: &HeaderValue,
    ical: &str,
    if_match: Option<&str>,
    if_none_match: Option<&str>,
) -> Result<Option<String>> {
    let mut headers = HeaderMap::new();
    headers.insert(reqwest::header::AUTHORIZATION, auth.clone());
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(CONTENT_ICAL));
    if let Some(etag) = if_match {
        headers.insert(
            "If-Match",
            HeaderValue::from_str(etag).map_err(|e| AthenError::Other(format!("If-Match: {e}")))?,
        );
    }
    if let Some(v) = if_none_match {
        headers.insert(
            "If-None-Match",
            HeaderValue::from_str(v)
                .map_err(|e| AthenError::Other(format!("If-None-Match: {e}")))?,
        );
    }
    let resp = http
        .put(url.clone())
        .headers(headers)
        .body(ical.to_string())
        .send()
        .await
        .map_err(|e| AthenError::Other(format!("CalDAV PUT {url} send: {e}")))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(AthenError::Other(format!(
            "CalDAV PUT {url} -> {status}: {}",
            body.chars().take(400).collect::<String>()
        )));
    }
    let etag = resp
        .headers()
        .get("ETag")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    Ok(etag)
}

/// DELETE a calendar object. `if_match` provides optimistic concurrency.
pub async fn delete(
    http: &Client,
    url: &Url,
    auth: &HeaderValue,
    if_match: Option<&str>,
) -> Result<()> {
    let mut headers = HeaderMap::new();
    headers.insert(reqwest::header::AUTHORIZATION, auth.clone());
    if let Some(etag) = if_match {
        headers.insert(
            "If-Match",
            HeaderValue::from_str(etag).map_err(|e| AthenError::Other(format!("If-Match: {e}")))?,
        );
    }
    let resp = http
        .delete(url.clone())
        .headers(headers)
        .send()
        .await
        .map_err(|e| AthenError::Other(format!("CalDAV DELETE {url} send: {e}")))?;
    let status = resp.status();
    if !status.is_success() && status != StatusCode::NOT_FOUND {
        let body = resp.text().await.unwrap_or_default();
        return Err(AthenError::Other(format!(
            "CalDAV DELETE {url} -> {status}: {}",
            body.chars().take(400).collect::<String>()
        )));
    }
    Ok(())
}

async fn handle_response(resp: Response, context: &str) -> Result<String> {
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| AthenError::Other(format!("Read {context} body: {e}")))?;
    if status == StatusCode::MULTI_STATUS || status.is_success() {
        return Ok(body);
    }
    if status == StatusCode::UNAUTHORIZED {
        return Err(AthenError::Other(format!(
            "{context}: 401 Unauthorized — check username and app-specific password"
        )));
    }
    Err(AthenError::Other(format!(
        "{context} -> {status}: {}",
        body.chars().take(400).collect::<String>()
    )))
}

/// Body of the `current-user-principal` PROPFIND used for discovery.
pub const PROPFIND_CURRENT_USER_PRINCIPAL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<D:propfind xmlns:D="DAV:">
  <D:prop>
    <D:current-user-principal/>
  </D:prop>
</D:propfind>"#;

/// Body of the `calendar-home-set` PROPFIND on a principal URL.
pub const PROPFIND_CALENDAR_HOME_SET: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<D:propfind xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:prop>
    <C:calendar-home-set/>
  </D:prop>
</D:propfind>"#;

/// Body of the depth=1 PROPFIND that enumerates calendars under a home set.
pub const PROPFIND_CALENDAR_LIST: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<D:propfind xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav" xmlns:A="http://apple.com/ns/ical/">
  <D:prop>
    <D:resourcetype/>
    <D:displayname/>
    <A:calendar-color/>
  </D:prop>
</D:propfind>"#;

/// Build a `calendar-query` REPORT body filtering VEVENTs by a time range.
/// Times are UTC `YYYYMMDDTHHMMSSZ`.
pub fn build_calendar_query(start_utc: &str, end_utc: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<C:calendar-query xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:prop>
    <D:getetag/>
    <C:calendar-data/>
  </D:prop>
  <C:filter>
    <C:comp-filter name="VCALENDAR">
      <C:comp-filter name="VEVENT">
        <C:time-range start="{start_utc}" end="{end_utc}"/>
      </C:comp-filter>
    </C:comp-filter>
  </C:filter>
</C:calendar-query>"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_auth_header_format() {
        let h = basic_auth_header("alex@example.com", "abcd-efgh-ijkl-mnop");
        let s = h.to_str().unwrap();
        assert!(s.starts_with("Basic "));
        let raw = base64::engine::general_purpose::STANDARD
            .decode(s.strip_prefix("Basic ").unwrap())
            .unwrap();
        assert_eq!(
            String::from_utf8(raw).unwrap(),
            "alex@example.com:abcd-efgh-ijkl-mnop"
        );
    }

    #[test]
    fn calendar_query_includes_time_range() {
        let body = build_calendar_query("20260515T000000Z", "20260522T000000Z");
        assert!(body.contains("20260515T000000Z"));
        assert!(body.contains("20260522T000000Z"));
        assert!(body.contains("VEVENT"));
    }
}
