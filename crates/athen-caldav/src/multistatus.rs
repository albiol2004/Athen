//! Tiny parser for the WebDAV `<D:multistatus>` XML envelope that
//! PROPFIND and REPORT responses use.
//!
//! We do not try to be a full WebDAV XML parser — just enough to pull
//! out `href`, `getetag`, and `calendar-data` for each response. Namespace
//! prefixes are normalised by taking the **local name** only, since
//! servers vary (`D:`, `d:`, `dav:`, `xmlns="DAV:"`).

use quick_xml::events::Event;
use quick_xml::Reader;

use athen_core::error::{AthenError, Result};

/// One row of a multistatus response.
#[derive(Debug, Clone, Default)]
pub struct MultistatusEntry {
    pub href: Option<String>,
    pub etag: Option<String>,
    pub calendar_data: Option<String>,
    /// Raw `<resourcetype>` child element names (lowercased local names).
    /// Lets the calendar enumerator filter by `calendar`.
    pub resource_types: Vec<String>,
    /// Display name for a calendar collection (`<displayname>`).
    pub displayname: Option<String>,
    /// Calendar color (`<calendar-color>` — Apple extension, but Google
    /// and Nextcloud also emit it).
    pub calendar_color: Option<String>,
    /// Where the current-user-principal points (one-shot discovery).
    pub current_user_principal_href: Option<String>,
    /// Where the calendar-home-set points (one-shot discovery).
    pub calendar_home_set_href: Option<String>,
}

/// Parse a `<D:multistatus>` payload into one entry per `<D:response>`.
pub fn parse_multistatus(xml: &str) -> Result<Vec<MultistatusEntry>> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut entries: Vec<MultistatusEntry> = Vec::new();
    let mut current: MultistatusEntry = MultistatusEntry::default();
    let mut path: Vec<String> = Vec::new();
    let mut text_buf = String::new();
    let mut in_response = false;

    loop {
        let event = reader
            .read_event()
            .map_err(|e| AthenError::Other(format!("Multistatus XML error: {e}")))?;
        match event {
            Event::Eof => break,
            Event::Start(e) => {
                let name = local_name(e.name().as_ref());
                path.push(name.clone());
                if name == "response" {
                    in_response = true;
                    current = MultistatusEntry::default();
                }
                if in_response
                    && path.contains(&"resourcetype".to_string())
                    && name != "resourcetype"
                {
                    current.resource_types.push(name);
                }
                text_buf.clear();
            }
            Event::Empty(e) => {
                let name = local_name(e.name().as_ref());
                if in_response && path.last().map(String::as_str) == Some("resourcetype") {
                    current.resource_types.push(name);
                }
            }
            Event::Text(t) => {
                let s = t
                    .unescape()
                    .map_err(|e| AthenError::Other(format!("Multistatus text unescape: {e}")))?;
                text_buf.push_str(&s);
            }
            Event::CData(c) => {
                text_buf.push_str(&String::from_utf8_lossy(c.as_ref()));
            }
            Event::End(e) => {
                let name = local_name(e.name().as_ref());
                if in_response {
                    match name.as_str() {
                        "href" => {
                            let parent = path.iter().rev().nth(1).map(String::as_str).unwrap_or("");
                            let trimmed = text_buf.trim().to_string();
                            match parent {
                                "current-user-principal" => {
                                    current.current_user_principal_href = Some(trimmed);
                                }
                                "calendar-home-set" => {
                                    current.calendar_home_set_href = Some(trimmed);
                                }
                                _ => {
                                    if current.href.is_none() {
                                        current.href = Some(trimmed);
                                    }
                                }
                            }
                        }
                        "getetag" => {
                            current.etag = Some(text_buf.trim().to_string());
                        }
                        "calendar-data" => {
                            current.calendar_data = Some(text_buf.clone());
                        }
                        "displayname" if current.displayname.is_none() => {
                            current.displayname = Some(text_buf.trim().to_string());
                        }
                        "calendar-color" => {
                            current.calendar_color = Some(text_buf.trim().to_string());
                        }
                        "response" => {
                            entries.push(std::mem::take(&mut current));
                            in_response = false;
                        }
                        _ => {}
                    }
                }
                path.pop();
                text_buf.clear();
            }
            _ => {}
        }
    }
    Ok(entries)
}

fn local_name(qname: &[u8]) -> String {
    let s = std::str::from_utf8(qname).unwrap_or("");
    match s.rfind(':') {
        Some(i) => s[i + 1..].to_ascii_lowercase(),
        None => s.to_ascii_lowercase(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_calendar_collection_list() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<D:multistatus xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:response>
    <D:href>/calendars/user/personal/</D:href>
    <D:propstat>
      <D:prop>
        <D:displayname>Personal</D:displayname>
        <D:resourcetype>
          <D:collection/>
          <C:calendar/>
        </D:resourcetype>
      </D:prop>
    </D:propstat>
  </D:response>
  <D:response>
    <D:href>/calendars/user/work/</D:href>
    <D:propstat>
      <D:prop>
        <D:displayname>Work</D:displayname>
        <D:resourcetype>
          <D:collection/>
          <C:calendar/>
        </D:resourcetype>
      </D:prop>
    </D:propstat>
  </D:response>
</D:multistatus>"#;
        let entries = parse_multistatus(xml).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries[0].href.as_deref(),
            Some("/calendars/user/personal/")
        );
        assert_eq!(entries[0].displayname.as_deref(), Some("Personal"));
        assert!(entries[0].resource_types.contains(&"calendar".to_string()));
        assert_eq!(entries[1].displayname.as_deref(), Some("Work"));
    }

    #[test]
    fn parse_principal_discovery() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<D:multistatus xmlns:D="DAV:">
  <D:response>
    <D:href>/</D:href>
    <D:propstat>
      <D:prop>
        <D:current-user-principal>
          <D:href>/principals/users/alex/</D:href>
        </D:current-user-principal>
      </D:prop>
    </D:propstat>
  </D:response>
</D:multistatus>"#;
        let entries = parse_multistatus(xml).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].current_user_principal_href.as_deref(),
            Some("/principals/users/alex/")
        );
    }

    #[test]
    fn parse_calendar_query_with_etag_and_data() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<D:multistatus xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:response>
    <D:href>/calendars/user/personal/evt-1.ics</D:href>
    <D:propstat>
      <D:prop>
        <D:getetag>"abc123"</D:getetag>
        <C:calendar-data>BEGIN:VCALENDAR
VERSION:2.0
BEGIN:VEVENT
UID:evt-1
END:VEVENT
END:VCALENDAR</C:calendar-data>
      </D:prop>
    </D:propstat>
  </D:response>
</D:multistatus>"#;
        let entries = parse_multistatus(xml).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].etag.as_deref(), Some("\"abc123\""));
        let cd = entries[0].calendar_data.as_ref().unwrap();
        assert!(cd.contains("UID:evt-1"));
        assert!(cd.contains("BEGIN:VCALENDAR"));
    }
}
