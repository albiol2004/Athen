//! Email sense monitor.
//!
//! Polls an IMAP server for new unseen messages, parses them with `mailparse`,
//! and converts each into a [`SenseEvent`] with [`RiskLevel::Caution`] since
//! email is an external input channel.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use uuid::Uuid;

use athen_core::config::{AthenConfig, EmailConfig};
use athen_core::error::{AthenError, Result};
use athen_core::event::{
    Attachment, AttachmentSource, EventKind, EventSource, NormalizedContent, SenderInfo,
    SenseEvent,
};
use athen_core::risk::RiskLevel;
use athen_core::traits::sense::SenseMonitor;

/// Hard cap below which we save email attachment bytes to disk.
/// Anything above is recorded as metadata-only (size + name + source
/// pointer) so the agent can still see "an attachment was here" and
/// optionally re-download via the source coordinates. Set roomy enough
/// for typical invoices/receipts but tight enough to survive spam.
const MAX_PERSIST_BYTES: u64 = 25 * 1024 * 1024;

/// MIME prefixes we never persist bytes for, regardless of size. The
/// orchestrator can still receive the metadata record. Defence-in-depth
/// against malware-laden attachments — full policy still lives upstream.
const MIME_BLOCKLIST_PREFIXES: &[&str] = &[
    "application/x-msdownload",
    "application/x-msi",
    "application/x-executable",
    "application/x-sh",
    "application/x-bat",
];

const ACCOUNT_ID_PRIMARY: &str = "primary";

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

/// Bytes + IMAP part path captured during MIME extraction. Internal to
/// the email crate — the public `extract_email_body` discards the
/// bytes for tests that only care about metadata, while the persist
/// path consumes them in `extract_email_body_internal`.
struct RawEmailAttachment {
    name: String,
    mime_type: String,
    bytes: Vec<u8>,
    /// Dotted IMAP part path (`"2.1"`) so we can `BODY[<path>]` re-fetch
    /// just this attachment after the bytes are TTL-purged.
    part_path: String,
}

/// Extract text + html bodies and attachment **metadata** from raw bytes.
/// Public wrapper over [`extract_email_body_internal`] for tests that
/// don't need the per-part bytes.
#[cfg(test)]
pub(crate) fn extract_email_body(raw_bytes: &[u8]) -> (String, Option<String>, Vec<Attachment>) {
    let (text, html, raws) = extract_email_body_internal(raw_bytes);
    let attachments = raws
        .into_iter()
        .map(|r| Attachment::new(r.name, r.mime_type, r.bytes.len() as u64, None, None))
        .collect();
    (text, html, attachments)
}

/// Internal version that yields the raw bytes + IMAP part path for each
/// attachment. The persist step consumes these to save bytes to disk
/// and build final `Attachment` records with `AttachmentSource::Email`
/// populated for refetch-after-TTL.
fn extract_email_body_internal(
    raw_bytes: &[u8],
) -> (String, Option<String>, Vec<RawEmailAttachment>) {
    let mut text_body = String::new();
    let mut html_body: Option<String> = None;
    let mut attachments: Vec<RawEmailAttachment> = Vec::new();

    let parsed = match mailparse::parse_mail(raw_bytes) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("Failed to parse email body: {e}");
            return (text_body, html_body, attachments);
        }
    };

    collect_parts(
        &parsed,
        &mut Vec::new(),
        &mut text_body,
        &mut html_body,
        &mut attachments,
    );

    (text_body, html_body, attachments)
}

/// Recursively walk MIME parts extracting text, HTML, and attachment
/// bytes. `path` is the dotted IMAP part-path stack accumulated as we
/// recurse — so the first sub-part of the second top-level part lands
/// at `"2.1"`, matching what `BODY[2.1]` would re-fetch.
fn collect_parts(
    mail: &mailparse::ParsedMail<'_>,
    path: &mut Vec<usize>,
    text_body: &mut String,
    html_body: &mut Option<String>,
    attachments: &mut Vec<RawEmailAttachment>,
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

        let bytes = mail.get_body_raw().unwrap_or_default();

        attachments.push(RawEmailAttachment {
            name: filename,
            mime_type: content_type.clone(),
            bytes,
            part_path: format_imap_part_path(path),
        });
        return;
    }

    // If this part has sub-parts, recurse into them. IMAP numbers
    // sub-parts from 1.
    if !mail.subparts.is_empty() {
        for (idx, part) in mail.subparts.iter().enumerate() {
            path.push(idx + 1);
            collect_parts(part, path, text_body, html_body, attachments);
            path.pop();
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

        let bytes = mail.get_body_raw().unwrap_or_default();

        attachments.push(RawEmailAttachment {
            name: filename,
            mime_type: content_type,
            bytes,
            part_path: format_imap_part_path(path),
        });
    }
}

fn format_imap_part_path(path: &[usize]) -> String {
    if path.is_empty() {
        // Top-level (single-part email) — IMAP convention: BODY[1].
        return "1".into();
    }
    path.iter()
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join(".")
}

/// Sanitize a filename for on-disk storage: drop directory separators
/// and any character that's risky on either Linux or Windows. Falls
/// back to `"file"` if the result is empty.
fn sanitize_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | '<' | '>' | ':' | '"' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim_matches(|c: char| c == '.' || c.is_whitespace());
    if trimmed.is_empty() {
        "file".into()
    } else {
        trimmed.to_string()
    }
}

/// Save raw attachment bytes under `<save_root>/<event_id>/` and build
/// final [`Attachment`] records with `AttachmentSource::Email`
/// populated. Skips bytes for blocklisted MIMEs and oversize parts but
/// still records metadata so the agent can see "an attachment exists"
/// — orchestrator decides downstream whether to re-download.
///
/// `save_root: None` is the test-friendly path: builds metadata-only
/// records without touching the filesystem.
fn persist_attachments(
    event_id: Uuid,
    folder: &str,
    uid: u32,
    uid_validity: u32,
    raws: Vec<RawEmailAttachment>,
    save_root: Option<&Path>,
) -> Vec<Attachment> {
    let mut out = Vec::with_capacity(raws.len());
    for raw in raws {
        let size = raw.bytes.len() as u64;
        let mime_lower = raw.mime_type.to_ascii_lowercase();
        let is_blocked = MIME_BLOCKLIST_PREFIXES
            .iter()
            .any(|p| mime_lower.starts_with(p));
        let is_oversize = size > MAX_PERSIST_BYTES;

        let source = AttachmentSource::Email {
            account_id: ACCOUNT_ID_PRIMARY.into(),
            mailbox: folder.into(),
            uid_validity,
            uid,
            part_path: raw.part_path.clone(),
        };

        let local_path = if is_blocked || is_oversize {
            None
        } else {
            save_root.and_then(|root| save_bytes(root, event_id, &raw.name, &raw.bytes))
        };

        // For PDFs that landed on disk, eagerly extract a `.txt`
        // sidecar so the executor can inline truncated text without
        // doing any IO at turn-build time, and so the agent can still
        // recall what the file said after the bytes are TTL-purged.
        let extracted_text_path = local_path.as_ref().and_then(|p| {
            if mime_lower.starts_with("application/pdf") {
                match crate::pdf_extract::extract_to_sidecar(p) {
                    Ok(side) => Some(side),
                    Err(e) => {
                        tracing::warn!("pdf-extract sidecar failed for {p:?}: {e}");
                        None
                    }
                }
            } else {
                None
            }
        });

        let mut att = Attachment::new(raw.name, raw.mime_type, size, local_path, Some(source));
        att.extracted_text_path = extracted_text_path;
        out.push(att);
    }
    out
}

/// Write `bytes` to `<root>/<event_id>/<sanitized_name>` and return the
/// final path. Returns `None` on any I/O error so callers degrade
/// gracefully to metadata-only.
fn save_bytes(root: &Path, event_id: Uuid, name: &str, bytes: &[u8]) -> Option<PathBuf> {
    let dir = root.join(event_id.to_string());
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!("Failed to create attachment dir {dir:?}: {e}");
        return None;
    }
    let path = dir.join(sanitize_filename(name));
    match std::fs::write(&path, bytes) {
        Ok(()) => Some(path),
        Err(e) => {
            tracing::warn!("Failed to save attachment {path:?}: {e}");
            None
        }
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
        let email = from[start + 1..].trim_end_matches('>').trim().to_string();
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
///
/// `folder` is preserved in the event body so downstream consumers (e.g. the
/// sense router) know where to mark the message `\Seen` after a successful
/// agent run.
fn message_to_event(
    uid: u32,
    uid_validity: u32,
    folder: &str,
    raw_body: &[u8],
    save_root: Option<&Path>,
) -> Result<SenseEvent> {
    let parsed = mailparse::parse_mail(raw_body)
        .map_err(|e| AthenError::Other(format!("Failed to parse email UID {uid}: {e}")))?;

    let subject = extract_subject(&parsed);
    let sender = extract_sender(&parsed);
    let (text, html, raws) = extract_email_body_internal(raw_body);

    let event_id = Uuid::new_v4();
    let attachments = persist_attachments(event_id, folder, uid, uid_validity, raws, save_root);

    let from_str = sender
        .as_ref()
        .map(|s| s.identifier.clone())
        .unwrap_or_default();

    let body = serde_json::json!({
        "subject": subject.as_deref().unwrap_or(""),
        "from": from_str,
        "text": text,
        "html": html,
        "folder": folder,
        "uid_validity": uid_validity,
    });

    Ok(SenseEvent {
        id: event_id,
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
    save_root: Option<&Path>,
) -> Result<(Vec<SenseEvent>, Option<u32>)> {
    let mut all_events = Vec::new();
    let mut global_max_uid = current_last;

    for folder in folders {
        match fetch_folder(session, folder, current_last, save_root) {
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
    save_root: Option<&Path>,
) -> Result<(Vec<SenseEvent>, Option<u32>)> {
    let mailbox = session
        .select(folder)
        .map_err(|e| AthenError::Other(format!("IMAP select '{folder}': {e}")))?;
    let uid_validity = mailbox.uid_validity.unwrap_or(0);

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

        match message_to_event(uid, uid_validity, folder, body, save_root) {
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
                None => return Ok(Vec::new()),    // not initialized
            }
        };

        let last_seen = Arc::clone(&self.last_seen_uid);
        // Resolve the on-disk root once per poll. None means "host
        // hasn't been initialised with a writable data dir" — fine for
        // tests, just degrades to metadata-only attachments.
        let save_root = athen_core::paths::athen_attachments_dir();

        let result = tokio::task::spawn_blocking(move || -> Result<Vec<SenseEvent>> {
            let current_last = *last_seen.lock().unwrap();
            let save_root = save_root.as_deref();

            let server = config.imap_server.as_str();
            let port = config.imap_port;

            let (all_events, global_max_uid) = if config.use_tls {
                let tcp = std::net::TcpStream::connect((server, port)).map_err(|e| {
                    AthenError::Other(format!("TCP connect to {server}:{port}: {e}"))
                })?;

                let connector = rustls_connector::RustlsConnector::new_with_native_certs()
                    .map_err(|e| AthenError::Other(format!("TLS connector setup: {e}")))?;

                let tls_stream = connector
                    .connect(server, tcp)
                    .map_err(|e| AthenError::Other(format!("TLS handshake with {server}: {e}")))?;

                let client = imap::Client::new(tls_stream);
                let mut session = client
                    .login(&config.username, &config.password)
                    .map_err(|(e, _)| AthenError::Other(format!("IMAP login: {e}")))?;

                let result =
                    poll_all_folders(&mut session, &config.folders, current_last, save_root);
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

                let result =
                    poll_all_folders(&mut session, &config.folders, current_last, save_root);
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

/// Mark an IMAP message as `\Seen` on the server.
///
/// Opens a fresh, single-purpose IMAP session, selects `folder`, runs
/// `UID STORE <uid> +FLAGS (\Seen)`, and logs out cleanly. Used by the
/// autonomous-agent dispatch path to flag emails the agent has already
/// successfully acted on, so they don't re-trigger on the next
/// `UID SEARCH UNSEEN` poll.
///
/// Mirrors the connection setup in `EmailMonitor::poll` (TLS / non-TLS
/// branches via `spawn_blocking`) so it shares the same TLS / TCP
/// behavior. Doesn't touch the polling session — concurrent IMAP calls on
/// one session are not safe.
///
/// Errors are returned as-is for the caller to log; this function never
/// retries. Callers that pass a wrong/disabled config get a clear
/// error back rather than a silent no-op.
pub async fn mark_uid_seen(config: &EmailConfig, folder: &str, uid: u32) -> Result<()> {
    if !config.enabled {
        return Err(AthenError::Other(
            "mark_uid_seen called with disabled email config".into(),
        ));
    }
    if config.imap_server.is_empty() {
        return Err(AthenError::Other(
            "mark_uid_seen called with empty imap_server".into(),
        ));
    }

    let config = config.clone();
    let folder = folder.to_string();

    tokio::task::spawn_blocking(move || -> Result<()> {
        let server = config.imap_server.as_str();
        let port = config.imap_port;

        if config.use_tls {
            let tcp = std::net::TcpStream::connect((server, port))
                .map_err(|e| AthenError::Other(format!("TCP connect to {server}:{port}: {e}")))?;

            let connector = rustls_connector::RustlsConnector::new_with_native_certs()
                .map_err(|e| AthenError::Other(format!("TLS connector setup: {e}")))?;

            let tls_stream = connector
                .connect(server, tcp)
                .map_err(|e| AthenError::Other(format!("TLS handshake with {server}: {e}")))?;

            let client = imap::Client::new(tls_stream);
            let mut session = client
                .login(&config.username, &config.password)
                .map_err(|(e, _)| AthenError::Other(format!("IMAP login: {e}")))?;

            let store_result = session
                .select(&folder)
                .map_err(|e| AthenError::Other(format!("IMAP select '{folder}': {e}")))
                .and_then(|_| {
                    session
                        .uid_store(uid.to_string(), "+FLAGS (\\Seen)")
                        .map(|_| ())
                        .map_err(|e| {
                            AthenError::Other(format!(
                                "IMAP uid_store \\Seen for UID {uid} in '{folder}': {e}"
                            ))
                        })
                });

            if let Err(e) = session.logout() {
                tracing::warn!("IMAP logout error after mark_uid_seen: {e}");
            }

            store_result
        } else {
            let tcp = std::net::TcpStream::connect((server, port))
                .map_err(|e| AthenError::Other(format!("TCP connect to {server}:{port}: {e}")))?;

            let client = imap::Client::new(tcp);
            let mut session = client
                .login(&config.username, &config.password)
                .map_err(|(e, _)| AthenError::Other(format!("IMAP login: {e}")))?;

            let store_result = session
                .select(&folder)
                .map_err(|e| AthenError::Other(format!("IMAP select '{folder}': {e}")))
                .and_then(|_| {
                    session
                        .uid_store(uid.to_string(), "+FLAGS (\\Seen)")
                        .map(|_| ())
                        .map_err(|e| {
                            AthenError::Other(format!(
                                "IMAP uid_store \\Seen for UID {uid} in '{folder}': {e}"
                            ))
                        })
                });

            if let Err(e) = session.logout() {
                tracing::warn!("IMAP logout error after mark_uid_seen: {e}");
            }

            store_result
        }
    })
    .await
    .map_err(|e| AthenError::Other(format!("mark_uid_seen task panicked: {e}")))?
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

        let event = message_to_event(42, 1, "INBOX", raw, None).unwrap();
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
        assert_eq!(body["folder"], "INBOX");
    }

    #[test]
    fn extract_sender_with_quoted_name_and_angle_brackets() {
        let raw = b"From: \"John O'Brien\" <john.obrien@example.com>\r\n\
Subject: Test\r\n\
Content-Type: text/plain\r\n\
\r\n\
Body";
        let parsed = mailparse::parse_mail(raw).unwrap();
        let sender = extract_sender(&parsed).unwrap();
        assert_eq!(sender.identifier, "john.obrien@example.com");
        assert_eq!(sender.display_name.as_deref(), Some("John O'Brien"));
    }

    #[test]
    fn extract_sender_no_from_header() {
        let raw = b"Subject: No From\r\n\
Content-Type: text/plain\r\n\
\r\n\
Body";
        let parsed = mailparse::parse_mail(raw).unwrap();
        let sender = extract_sender(&parsed);
        assert!(sender.is_none());
    }

    #[test]
    fn extract_subject_missing() {
        let raw = b"From: x@y.com\r\n\
Content-Type: text/plain\r\n\
\r\n\
Body";
        let parsed = mailparse::parse_mail(raw).unwrap();
        let subject = extract_subject(&parsed);
        assert!(subject.is_none());
    }

    #[test]
    fn extract_subject_empty() {
        let raw = b"From: x@y.com\r\n\
Subject: \r\n\
Content-Type: text/plain\r\n\
\r\n\
Body";
        let parsed = mailparse::parse_mail(raw).unwrap();
        let subject = extract_subject(&parsed);
        // Empty subject should return None
        assert!(subject.is_none());
    }

    #[test]
    fn parse_nested_multipart() {
        let raw = b"From: nested@example.com\r\n\
Subject: Nested\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=\"outer\"\r\n\
\r\n\
--outer\r\n\
Content-Type: multipart/alternative; boundary=\"inner\"\r\n\
\r\n\
--inner\r\n\
Content-Type: text/plain\r\n\
\r\n\
Plain text\r\n\
--inner\r\n\
Content-Type: text/html\r\n\
\r\n\
<p>HTML</p>\r\n\
--inner--\r\n\
--outer\r\n\
Content-Type: image/png; name=\"photo.png\"\r\n\
Content-Disposition: attachment; filename=\"photo.png\"\r\n\
Content-Transfer-Encoding: base64\r\n\
\r\n\
iVBOR\r\n\
--outer--\r\n";

        let (text, html, attachments) = extract_email_body(raw);
        assert!(text.contains("Plain text"));
        assert!(html.is_some());
        assert!(html.unwrap().contains("<p>HTML</p>"));
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].name, "photo.png");
    }

    #[test]
    fn parse_email_with_non_utf8_tolerant() {
        // Email with Latin-1 content that's valid ASCII subset
        let raw = b"From: latin@example.com\r\n\
Subject: Latin chars\r\n\
Content-Type: text/plain; charset=iso-8859-1\r\n\
\r\n\
Hello world";
        let (text, _, _) = extract_email_body(raw);
        assert!(text.contains("Hello world"));
    }

    #[test]
    fn message_to_event_no_subject() {
        let raw = b"From: nosub@example.com\r\n\
Content-Type: text/plain\r\n\
\r\n\
Just a body, no subject";

        let event = message_to_event(99, 1, "INBOX", raw, None).unwrap();
        assert!(event.content.summary.is_none());
        assert_eq!(event.raw_id.as_deref(), Some("99"));
        assert!(event.content.body["text"]
            .as_str()
            .unwrap()
            .contains("Just a body"));
    }

    #[test]
    fn message_to_event_no_sender() {
        let raw = b"Subject: No From\r\n\
Content-Type: text/plain\r\n\
\r\n\
Body without sender";

        let event = message_to_event(100, 1, "Archive", raw, None).unwrap();
        assert!(event.sender.is_none());
        assert_eq!(event.content.summary.as_deref(), Some("No From"));
        assert_eq!(event.content.body["folder"], "Archive");
    }

    #[test]
    fn multiple_text_parts_concatenated() {
        let raw = b"From: multi@example.com\r\n\
Subject: Multi text\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=\"bound\"\r\n\
\r\n\
--bound\r\n\
Content-Type: text/plain\r\n\
\r\n\
Part one\r\n\
--bound\r\n\
Content-Type: text/plain\r\n\
\r\n\
Part two\r\n\
--bound--\r\n";

        let (text, _, _) = extract_email_body(raw);
        assert!(text.contains("Part one"));
        assert!(text.contains("Part two"));
    }

    #[tokio::test]
    async fn mark_uid_seen_errors_on_disabled_config() {
        // Disabled config should fail fast with a clear error rather than
        // silently no-op or attempt a TCP connect.
        let config = EmailConfig::default(); // enabled = false
        let result = mark_uid_seen(&config, "INBOX", 1).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn mark_uid_seen_errors_on_missing_server() {
        // Enabled but no imap_server configured.
        let config = EmailConfig {
            enabled: true,
            imap_server: String::new(),
            ..Default::default()
        };
        let result = mark_uid_seen(&config, "INBOX", 1).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn mark_uid_seen_errors_on_unreachable_host() {
        // Enabled config pointing at an unroutable host: must return an
        // error gracefully rather than panic. We use TEST-NET-1 (RFC 5737)
        // with a short-lived expectation that TCP connect will fail.
        let config = EmailConfig {
            enabled: true,
            imap_server: "192.0.2.1".to_string(),
            imap_port: 1, // closed port; connect should refuse/timeout
            use_tls: false,
            username: "u".to_string(),
            password: "p".to_string(),
            ..Default::default()
        };

        // This may take a moment to fail (TCP connect timeout); cap it so the
        // test stays fast even when the OS is slow to error out.
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            mark_uid_seen(&config, "INBOX", 1),
        )
        .await;
        match result {
            Ok(Err(_)) => {} // expected: connect/login error
            Ok(Ok(())) => panic!("mark_uid_seen unexpectedly succeeded against TEST-NET-1"),
            Err(_) => {} // also acceptable: TCP connect just hangs in CI
        }
    }

    #[test]
    fn inline_image_treated_as_attachment() {
        let raw = b"From: inline@example.com\r\n\
Subject: Inline\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=\"bound\"\r\n\
\r\n\
--bound\r\n\
Content-Type: text/plain\r\n\
\r\n\
See image below\r\n\
--bound\r\n\
Content-Type: image/jpeg; name=\"photo.jpg\"\r\n\
Content-Disposition: inline\r\n\
Content-Transfer-Encoding: base64\r\n\
\r\n\
/9j/4AAQ\r\n\
--bound--\r\n";

        let (text, _, attachments) = extract_email_body(raw);
        assert!(text.contains("See image below"));
        // Inline non-text parts should be treated as attachments
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].mime_type, "image/jpeg");
    }

    #[test]
    fn part_path_is_dotted_for_nested_multipart() {
        // multipart/mixed → [text/plain, multipart/alternative → [text/plain, text/html]],
        // followed by an attachment at top level. We expect part_path = "2"
        // for the attachment (second top-level sub-part).
        let raw = b"From: sender@example.com\r\n\
Subject: With attachment\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=\"outer\"\r\n\
\r\n\
--outer\r\n\
Content-Type: multipart/alternative; boundary=\"inner\"\r\n\
\r\n\
--inner\r\n\
Content-Type: text/plain\r\n\
\r\n\
plain body\r\n\
--inner\r\n\
Content-Type: text/html\r\n\
\r\n\
<p>html body</p>\r\n\
--inner--\r\n\
--outer\r\n\
Content-Type: application/pdf; name=\"invoice.pdf\"\r\n\
Content-Disposition: attachment; filename=\"invoice.pdf\"\r\n\
\r\n\
PDF-BYTES\r\n\
--outer--\r\n";

        let (_text, _html, raws) = extract_email_body_internal(raw);
        assert_eq!(raws.len(), 1);
        assert_eq!(raws[0].part_path, "2");
        assert_eq!(raws[0].name, "invoice.pdf");
        assert!(!raws[0].bytes.is_empty());
    }

    #[test]
    fn persist_attachments_writes_bytes_when_save_root_set() {
        let tmp = tempfile::tempdir().unwrap();
        let event_id = Uuid::new_v4();
        let raws = vec![RawEmailAttachment {
            name: "doc.pdf".into(),
            mime_type: "application/pdf".into(),
            bytes: b"hello".to_vec(),
            part_path: "2".into(),
        }];
        let attachments =
            persist_attachments(event_id, "INBOX", 42, 1, raws, Some(tmp.path()));
        assert_eq!(attachments.len(), 1);
        let a = &attachments[0];
        assert!(a.local_path.is_some());
        assert!(matches!(a.source, Some(AttachmentSource::Email { .. })));
        let on_disk = std::fs::read(a.local_path.as_ref().unwrap()).unwrap();
        assert_eq!(on_disk, b"hello");
    }

    #[test]
    fn persist_attachments_pdf_with_garbage_bytes_does_not_poison_record() {
        // A PDF mime with garbage bytes — pdf-extract will reject it,
        // and we should still produce a clean Attachment with the raw
        // bytes saved and `extracted_text_path` left None. Earlier
        // versions could have panicked or emitted an Err that bubbled
        // out of the persist loop.
        let tmp = tempfile::tempdir().unwrap();
        let event_id = Uuid::new_v4();
        let raws = vec![RawEmailAttachment {
            name: "broken.pdf".into(),
            mime_type: "application/pdf".into(),
            bytes: b"not a real pdf".to_vec(),
            part_path: "2".into(),
        }];
        let attachments =
            persist_attachments(event_id, "INBOX", 42, 1, raws, Some(tmp.path()));
        assert_eq!(attachments.len(), 1);
        assert!(attachments[0].local_path.is_some());
        // Extraction failed → no sidecar → metadata stays clean.
        assert!(attachments[0].extracted_text_path.is_none());
    }

    #[test]
    fn persist_attachments_non_pdf_does_not_attempt_extraction() {
        let tmp = tempfile::tempdir().unwrap();
        let event_id = Uuid::new_v4();
        let raws = vec![RawEmailAttachment {
            name: "photo.jpg".into(),
            mime_type: "image/jpeg".into(),
            bytes: b"\xff\xd8\xff\xe0fakebody".to_vec(),
            part_path: "2".into(),
        }];
        let attachments =
            persist_attachments(event_id, "INBOX", 42, 1, raws, Some(tmp.path()));
        // Non-PDFs never get extracted_text_path set.
        assert!(attachments[0].local_path.is_some());
        assert!(attachments[0].extracted_text_path.is_none());
    }

    #[test]
    fn persist_attachments_skips_blocklisted_mime() {
        let tmp = tempfile::tempdir().unwrap();
        let event_id = Uuid::new_v4();
        let raws = vec![RawEmailAttachment {
            name: "evil.exe".into(),
            mime_type: "application/x-msdownload".into(),
            bytes: b"MZ".to_vec(),
            part_path: "2".into(),
        }];
        let attachments =
            persist_attachments(event_id, "INBOX", 42, 1, raws, Some(tmp.path()));
        assert_eq!(attachments.len(), 1);
        // Metadata recorded, bytes NOT saved.
        assert!(attachments[0].local_path.is_none());
        assert!(matches!(
            attachments[0].source,
            Some(AttachmentSource::Email { .. })
        ));
    }

    #[test]
    fn persist_attachments_skips_oversize() {
        let tmp = tempfile::tempdir().unwrap();
        let event_id = Uuid::new_v4();
        // Spoofing size by making a vec that's "officially" oversize.
        // We just need vec.len() > MAX_PERSIST_BYTES — keeping the vec
        // a few bytes over the limit avoids huge allocs in the test.
        let bytes = vec![0u8; (MAX_PERSIST_BYTES + 16) as usize];
        let raws = vec![RawEmailAttachment {
            name: "big.bin".into(),
            mime_type: "application/octet-stream".into(),
            bytes,
            part_path: "2".into(),
        }];
        let attachments =
            persist_attachments(event_id, "INBOX", 42, 1, raws, Some(tmp.path()));
        assert!(attachments[0].local_path.is_none());
        assert_eq!(attachments[0].size_bytes, MAX_PERSIST_BYTES + 16);
    }

    #[test]
    fn persist_attachments_no_save_root_yields_metadata_only() {
        let event_id = Uuid::new_v4();
        let raws = vec![RawEmailAttachment {
            name: "doc.pdf".into(),
            mime_type: "application/pdf".into(),
            bytes: b"hello".to_vec(),
            part_path: "2".into(),
        }];
        let attachments = persist_attachments(event_id, "INBOX", 42, 1, raws, None);
        assert_eq!(attachments.len(), 1);
        assert!(attachments[0].local_path.is_none());
        // Source is still populated so we can refetch.
        match attachments[0].source.as_ref().unwrap() {
            AttachmentSource::Email {
                mailbox,
                uid,
                uid_validity,
                part_path,
                account_id,
            } => {
                assert_eq!(mailbox, "INBOX");
                assert_eq!(*uid, 42);
                assert_eq!(*uid_validity, 1);
                assert_eq!(part_path, "2");
                assert_eq!(account_id, ACCOUNT_ID_PRIMARY);
            }
            _ => panic!("expected Email source"),
        }
    }

    #[test]
    fn sanitize_filename_strips_dangerous_chars() {
        // Slashes become underscores; leading dots are stripped so a
        // crafted "../foo" can't resolve out of the event dir or create
        // a dotfile.
        assert_eq!(sanitize_filename("../etc/passwd"), "_etc_passwd");
        assert_eq!(sanitize_filename("invoice<>.pdf"), "invoice__.pdf");
        assert_eq!(sanitize_filename(""), "file");
        assert_eq!(sanitize_filename("..."), "file");
    }
}
