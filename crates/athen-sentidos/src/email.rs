//! Email sense monitor.
//!
//! Polls an IMAP server for new unseen messages, parses them with `mailparse`,
//! and converts each into a [`SenseEvent`] with [`RiskLevel::Caution`] since
//! email is an external input channel.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use uuid::Uuid;

use athen_core::config::{AthenConfig, EmailConfig};
use athen_core::error::{AthenError, Result};
use athen_core::event::{
    Attachment, EventKind, EventSource, NormalizedContent, SenderInfo, SenseEvent,
};
use athen_core::risk::RiskLevel;
use athen_core::traits::sense::SenseMonitor;

/// Email sense monitor backed by IMAP.
///
/// Connects to the configured IMAP server on each poll, fetches unseen
/// messages with UIDs greater than the last one processed, and returns
/// them as [`SenseEvent`]s.
pub struct EmailMonitor {
    config: Arc<Mutex<Option<EmailConfig>>>,
    last_seen_uid: Arc<Mutex<Option<u32>>>,
    poll_interval: Duration,
}

impl EmailMonitor {
    /// Create a new `EmailMonitor` with the default poll interval of 60 seconds.
    pub fn new() -> Self {
        Self {
            config: Arc::new(Mutex::new(None)),
            last_seen_uid: Arc::new(Mutex::new(None)),
            poll_interval: Duration::from_secs(60),
        }
    }

    /// Create an `EmailMonitor` with a custom poll interval.
    pub fn with_interval(poll_interval: Duration) -> Self {
        Self {
            config: Arc::new(Mutex::new(None)),
            last_seen_uid: Arc::new(Mutex::new(None)),
            poll_interval,
        }
    }

    /// The risk level assigned to events from this source.
    pub fn source_risk() -> RiskLevel {
        RiskLevel::Caution
    }
}

impl Default for EmailMonitor {
    fn default() -> Self {
        Self::new()
    }
}

/// Extract the text body, optional HTML body, and attachments from raw email bytes.
///
/// Returns `(text_body, html_body, attachments)`. If no text part is found the
/// text body will be an empty string.
pub(crate) fn extract_email_body(raw_bytes: &[u8]) -> (String, Option<String>, Vec<Attachment>) {
    let mut text_body = String::new();
    let mut html_body: Option<String> = None;
    let mut attachments = Vec::new();

    let parsed = match mailparse::parse_mail(raw_bytes) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("Failed to parse email body: {e}");
            return (text_body, html_body, attachments);
        }
    };

    collect_parts(&parsed, &mut text_body, &mut html_body, &mut attachments);

    (text_body, html_body, attachments)
}

/// Recursively walk MIME parts extracting text, HTML, and attachments.
fn collect_parts(
    mail: &mailparse::ParsedMail<'_>,
    text_body: &mut String,
    html_body: &mut Option<String>,
    attachments: &mut Vec<Attachment>,
) {
    let content_type = mail.ctype.mimetype.to_lowercase();

    // Check if this part is an attachment via Content-Disposition.
    let disposition: String = mail
        .headers
        .iter()
        .find(|h| h.get_key().eq_ignore_ascii_case("Content-Disposition"))
        .map(|h| h.get_value())
        .unwrap_or_default();

    let is_attachment = disposition.starts_with("attachment");

    if is_attachment {
        let filename = mail
            .ctype
            .params
            .get("name")
            .cloned()
            .unwrap_or_else(|| "unnamed".to_string());

        let size = mail
            .get_body_raw()
            .map(|b| b.len() as u64)
            .unwrap_or(0);

        attachments.push(Attachment {
            name: filename,
            mime_type: content_type.clone(),
            size_bytes: size,
            path: None,
        });
        return;
    }

    // If this part has sub-parts, recurse into them.
    if !mail.subparts.is_empty() {
        for part in &mail.subparts {
            collect_parts(part, text_body, html_body, attachments);
        }
        return;
    }

    // Leaf part — extract content.
    if content_type == "text/plain" {
        if let Ok(body) = mail.get_body() {
            if text_body.is_empty() {
                *text_body = body;
            } else {
                text_body.push('\n');
                text_body.push_str(&body);
            }
        }
    } else if content_type == "text/html" {
        if let Ok(body) = mail.get_body() {
            *html_body = Some(body);
        }
    } else {
        // Non-text leaf part — treat as inline attachment.
        let filename = mail
            .ctype
            .params
            .get("name")
            .cloned()
            .unwrap_or_else(|| "unnamed".to_string());

        let size = mail
            .get_body_raw()
            .map(|b| b.len() as u64)
            .unwrap_or(0);

        attachments.push(Attachment {
            name: filename,
            mime_type: content_type,
            size_bytes: size,
            path: None,
        });
    }
}

/// Extract the sender (From header) from a parsed email.
fn extract_sender(parsed: &mailparse::ParsedMail<'_>) -> Option<SenderInfo> {
    let from_header = parsed
        .headers
        .iter()
        .find(|h| h.get_key().eq_ignore_ascii_case("From"))?;
    let from = from_header.get_value();

    if from.is_empty() {
        return None;
    }

    // Try to split "Display Name <email@example.com>" format.
    let (display_name, identifier) = if let Some(start) = from.find('<') {
        let name = from[..start].trim().trim_matches('"').to_string();
        let email = from[start + 1..]
            .trim_end_matches('>')
            .trim()
            .to_string();
        let display = if name.is_empty() { None } else { Some(name) };
        (display, email)
    } else {
        (None, from.trim().to_string())
    };

    Some(SenderInfo {
        identifier,
        contact_id: None,
        display_name,
    })
}

/// Extract the Subject header from a parsed email.
fn extract_subject(parsed: &mailparse::ParsedMail<'_>) -> Option<String> {
    parsed
        .headers
        .iter()
        .find(|h| h.get_key().eq_ignore_ascii_case("Subject"))
        .map(|h| h.get_value())
        .filter(|s| !s.is_empty())
}

/// Build a `SenseEvent` from a single fetched IMAP message.
fn message_to_event(uid: u32, raw_body: &[u8]) -> Result<SenseEvent> {
    let parsed = mailparse::parse_mail(raw_body)
        .map_err(|e| AthenError::Other(format!("Failed to parse email UID {uid}: {e}")))?;

    let subject = extract_subject(&parsed);
    let sender = extract_sender(&parsed);
    let (text, html, attachments) = extract_email_body(raw_body);

    let from_str = sender
        .as_ref()
        .map(|s| s.identifier.clone())
        .unwrap_or_default();

    let body = serde_json::json!({
        "subject": subject.as_deref().unwrap_or(""),
        "from": from_str,
        "text": text,
        "html": html,
    });

    Ok(SenseEvent {
        id: Uuid::new_v4(),
        timestamp: Utc::now(),
        source: EventSource::Email,
        kind: EventKind::NewMessage,
        sender,
        content: NormalizedContent {
            summary: subject,
            body,
            attachments,
        },
        source_risk: RiskLevel::Caution,
        raw_id: Some(uid.to_string()),
    })
}

/// Poll all configured folders and return collected events with the global max UID.
fn poll_all_folders<S: std::io::Read + std::io::Write>(
    session: &mut imap::Session<S>,
    folders: &[String],
    current_last: Option<u32>,
) -> Result<(Vec<SenseEvent>, Option<u32>)> {
    let mut all_events = Vec::new();
    let mut global_max_uid = current_last;

    for folder in folders {
        match fetch_folder(session, folder, current_last) {
            Ok((events, max_uid)) => {
                all_events.extend(events);
                if let Some(mu) = max_uid {
                    global_max_uid = Some(global_max_uid.map_or(mu, |cur: u32| cur.max(mu)));
                }
            }
            Err(e) => {
                tracing::warn!("Error polling folder '{folder}': {e}");
            }
        }
    }

    Ok((all_events, global_max_uid))
}

/// Perform the blocking IMAP fetch for a single folder.
///
/// Returns the events and the maximum UID seen.
fn fetch_folder<S: std::io::Read + std::io::Write>(
    session: &mut imap::Session<S>,
    folder: &str,
    min_uid: Option<u32>,
) -> Result<(Vec<SenseEvent>, Option<u32>)> {
    session
        .select(folder)
        .map_err(|e| AthenError::Other(format!("IMAP select '{folder}': {e}")))?;

    let uids = session
        .uid_search("UNSEEN")
        .map_err(|e| AthenError::Other(format!("IMAP uid_search UNSEEN in '{folder}': {e}")))?;

    // Filter to only UIDs we haven't seen yet.
    let new_uids: Vec<u32> = uids
        .into_iter()
        .filter(|&uid| match min_uid {
            Some(last) => uid > last,
            None => true,
        })
        .collect();

    if new_uids.is_empty() {
        return Ok((Vec::new(), None));
    }

    // Build a comma-separated UID set for the fetch command.
    let uid_set: String = new_uids
        .iter()
        .map(|u| u.to_string())
        .collect::<Vec<_>>()
        .join(",");

    let fetches = session
        .uid_fetch(&uid_set, "(UID ENVELOPE BODY.PEEK[] FLAGS)")
        .map_err(|e| AthenError::Other(format!("IMAP uid_fetch in '{folder}': {e}")))?;

    let mut events = Vec::new();
    let mut max_uid: Option<u32> = None;

    for fetch in fetches.iter() {
        let uid = match fetch.uid {
            Some(u) => u,
            None => continue,
        };

        // Track maximum UID.
        max_uid = Some(max_uid.map_or(uid, |m: u32| m.max(uid)));

        let body = match fetch.body() {
            Some(b) => b,
            None => {
                tracing::warn!("Email UID {uid} in '{folder}' has no body, skipping");
                continue;
            }
        };

        match message_to_event(uid, body) {
            Ok(event) => events.push(event),
            Err(e) => {
                tracing::warn!("Failed to parse email UID {uid} in '{folder}': {e}");
            }
        }
    }

    Ok((events, max_uid))
}

#[async_trait]
impl SenseMonitor for EmailMonitor {
    fn sense_id(&self) -> &str {
        "email"
    }

    async fn init(&mut self, config: &AthenConfig) -> Result<()> {
        let email_config = config.email.clone();
        if email_config.poll_interval_secs > 0 {
            self.poll_interval = Duration::from_secs(email_config.poll_interval_secs);
        }
        *self.config.lock().unwrap() = Some(email_config);
        tracing::info!("EmailMonitor initialized");
        Ok(())
    }

    async fn poll(&self) -> Result<Vec<SenseEvent>> {
        let config = {
            let guard = self.config.lock().unwrap();
            match guard.as_ref() {
                Some(c) if c.enabled => c.clone(),
                Some(_) => return Ok(Vec::new()), // disabled
                None => return Ok(Vec::new()),     // not initialized
            }
        };

        let last_seen = Arc::clone(&self.last_seen_uid);

        let result = tokio::task::spawn_blocking(move || -> Result<Vec<SenseEvent>> {
            let current_last = *last_seen.lock().unwrap();

            let server = config.imap_server.as_str();
            let port = config.imap_port;

            let (all_events, global_max_uid) = if config.use_tls {
                let tcp = std::net::TcpStream::connect((server, port)).map_err(|e| {
                    AthenError::Other(format!("TCP connect to {server}:{port}: {e}"))
                })?;

                let connector =
                    rustls_connector::RustlsConnector::new_with_native_certs().map_err(|e| {
                        AthenError::Other(format!("TLS connector setup: {e}"))
                    })?;

                let tls_stream = connector.connect(server, tcp).map_err(|e| {
                    AthenError::Other(format!("TLS handshake with {server}: {e}"))
                })?;

                let client = imap::Client::new(tls_stream);
                let mut session = client
                    .login(&config.username, &config.password)
                    .map_err(|(e, _)| AthenError::Other(format!("IMAP login: {e}")))?;

                let result = poll_all_folders(&mut session, &config.folders, current_last);
                if let Err(e) = session.logout() {
                    tracing::warn!("IMAP logout error: {e}");
                }
                result?
            } else {
                let tcp = std::net::TcpStream::connect((server, port)).map_err(|e| {
                    AthenError::Other(format!("TCP connect to {server}:{port}: {e}"))
                })?;

                let client = imap::Client::new(tcp);
                let mut session = client
                    .login(&config.username, &config.password)
                    .map_err(|(e, _)| AthenError::Other(format!("IMAP login: {e}")))?;

                let result = poll_all_folders(&mut session, &config.folders, current_last);
                if let Err(e) = session.logout() {
                    tracing::warn!("IMAP logout error: {e}");
                }
                result?
            };

            // Update last seen UID.
            if let Some(new_max) = global_max_uid {
                let mut guard = last_seen.lock().unwrap();
                *guard = Some(match *guard {
                    Some(old) => old.max(new_max),
                    None => new_max,
                });
            }

            Ok(all_events)
        })
        .await
        .map_err(|e| AthenError::Other(format!("Email poll task panicked: {e}")))?;

        result
    }

    fn poll_interval(&self) -> Duration {
        self.poll_interval
    }

    async fn shutdown(&self) -> Result<()> {
        tracing::info!("EmailMonitor shutting down");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sense_id_is_email() {
        let monitor = EmailMonitor::new();
        assert_eq!(monitor.sense_id(), "email");
    }

    #[tokio::test]
    async fn poll_returns_empty_when_not_initialized() {
        let monitor = EmailMonitor::new();
        let events = monitor.poll().await.unwrap();
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn poll_returns_empty_when_disabled() {
        let mut monitor = EmailMonitor::new();
        let config = AthenConfig::default(); // email.enabled = false by default
        monitor.init(&config).await.unwrap();
        let events = monitor.poll().await.unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn default_poll_interval_is_60s() {
        let monitor = EmailMonitor::new();
        assert_eq!(monitor.poll_interval(), Duration::from_secs(60));
    }

    #[test]
    fn custom_poll_interval() {
        let monitor = EmailMonitor::with_interval(Duration::from_secs(120));
        assert_eq!(monitor.poll_interval(), Duration::from_secs(120));
    }

    #[test]
    fn source_risk_is_caution() {
        assert_eq!(EmailMonitor::source_risk(), RiskLevel::Caution);
    }

    #[tokio::test]
    async fn shutdown_succeeds() {
        let monitor = EmailMonitor::new();
        monitor.shutdown().await.unwrap();
    }

    #[test]
    fn parse_email_body_helper_plain_text() {
        let raw = b"From: alice@example.com\r\n\
                     Subject: Hello\r\n\
                     Content-Type: text/plain; charset=utf-8\r\n\
                     \r\n\
                     Hello, world!";

        let (text, html, attachments) = extract_email_body(raw);
        assert_eq!(text.trim(), "Hello, world!");
        assert!(html.is_none());
        assert!(attachments.is_empty());
    }

    #[test]
    fn parse_email_body_helper_multipart() {
        let raw = b"From: bob@example.com\r\n\
Subject: Multipart\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/alternative; boundary=\"boundary123\"\r\n\
\r\n\
--boundary123\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
\r\n\
Plain text part\r\n\
--boundary123\r\n\
Content-Type: text/html; charset=utf-8\r\n\
\r\n\
<p>HTML part</p>\r\n\
--boundary123--\r\n";

        let (text, html, attachments) = extract_email_body(raw);
        assert!(text.contains("Plain text part"));
        assert!(html.is_some());
        assert!(html.unwrap().contains("<p>HTML part</p>"));
        assert!(attachments.is_empty());
    }

    #[test]
    fn parse_email_body_helper_with_attachment() {
        let raw = b"From: carol@example.com\r\n\
Subject: With attachment\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=\"mixbound\"\r\n\
\r\n\
--mixbound\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
\r\n\
See attached.\r\n\
--mixbound\r\n\
Content-Type: application/pdf; name=\"report.pdf\"\r\n\
Content-Disposition: attachment; filename=\"report.pdf\"\r\n\
Content-Transfer-Encoding: base64\r\n\
\r\n\
SGVsbG8=\r\n\
--mixbound--\r\n";

        let (text, _html, attachments) = extract_email_body(raw);
        assert!(text.contains("See attached"));
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].name, "report.pdf");
        assert_eq!(attachments[0].mime_type, "application/pdf");
        assert!(attachments[0].size_bytes > 0);
    }

    #[test]
    fn parse_email_body_helper_empty_body() {
        let raw = b"From: nobody@example.com\r\n\
Subject: Empty\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
\r\n";

        let (text, html, attachments) = extract_email_body(raw);
        assert!(text.is_empty() || text.trim().is_empty());
        assert!(html.is_none());
        assert!(attachments.is_empty());
    }

    #[test]
    fn extract_sender_parses_name_and_email() {
        let raw = b"From: \"Alice Smith\" <alice@example.com>\r\n\
Subject: Test\r\n\
Content-Type: text/plain\r\n\
\r\n\
Body";

        let parsed = mailparse::parse_mail(raw).unwrap();
        let sender = extract_sender(&parsed).unwrap();
        assert_eq!(sender.identifier, "alice@example.com");
        assert_eq!(sender.display_name.as_deref(), Some("Alice Smith"));
        assert!(sender.contact_id.is_none());
    }

    #[test]
    fn extract_sender_email_only() {
        let raw = b"From: bob@example.com\r\n\
Subject: Test\r\n\
Content-Type: text/plain\r\n\
\r\n\
Body";

        let parsed = mailparse::parse_mail(raw).unwrap();
        let sender = extract_sender(&parsed).unwrap();
        assert_eq!(sender.identifier, "bob@example.com");
        assert!(sender.display_name.is_none());
    }

    #[test]
    fn extract_subject_works() {
        let raw = b"From: x@y.com\r\n\
Subject: Important meeting\r\n\
Content-Type: text/plain\r\n\
\r\n\
Body";

        let parsed = mailparse::parse_mail(raw).unwrap();
        let subject = extract_subject(&parsed);
        assert_eq!(subject.as_deref(), Some("Important meeting"));
    }

    #[test]
    fn message_to_event_produces_correct_event() {
        let raw = b"From: \"Test User\" <test@example.com>\r\n\
Subject: Hello Athen\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
\r\n\
This is the body.";

        let event = message_to_event(42, raw).unwrap();
        assert_eq!(event.source, EventSource::Email);
        assert!(matches!(event.kind, EventKind::NewMessage));
        assert_eq!(event.source_risk, RiskLevel::Caution);
        assert_eq!(event.raw_id.as_deref(), Some("42"));
        assert_eq!(event.content.summary.as_deref(), Some("Hello Athen"));

        let sender = event.sender.unwrap();
        assert_eq!(sender.identifier, "test@example.com");
        assert_eq!(sender.display_name.as_deref(), Some("Test User"));

        let body = &event.content.body;
        assert_eq!(body["subject"], "Hello Athen");
        assert_eq!(body["from"], "test@example.com");
        assert!(body["text"].as_str().unwrap().contains("This is the body"));
        assert!(body["html"].is_null());
    }
}
