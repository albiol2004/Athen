//! Telegram Bot sense monitor.
//!
//! Polls the Telegram Bot API via raw HTTP (`reqwest`) for new messages
//! and converts each into a [`SenseEvent`] with [`EventSource::Messaging`].
//! Uses the `getUpdates` long-polling endpoint with offset tracking to
//! avoid processing the same message twice.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use serde::Deserialize;
use uuid::Uuid;

use athen_contacts::OwnerLookup;
use athen_core::config::{AthenConfig, TelegramConfig};
use athen_core::error::{AthenError, Result};
use athen_core::event::{
    Attachment, AttachmentSource, EventKind, EventSource, NormalizedContent, SenderInfo, SenseEvent,
};
use athen_core::risk::RiskLevel;
use athen_core::traits::sense::SenseMonitor;

/// Telegram caps `getFile` downloads at 20 MiB for bots. Anything
/// bigger we record as metadata-only so the agent still sees it
/// existed and can choose to ignore.
const TELEGRAM_MAX_DOWNLOAD_BYTES: u64 = 20 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Telegram Bot API response types (minimal)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct TelegramResponse<T> {
    pub ok: bool,
    pub result: Option<T>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TelegramUpdate {
    pub update_id: i64,
    pub message: Option<TelegramMessage>,
    /// Inline-keyboard button taps. Used by the approval router to
    /// resolve approve/deny questions delivered via Telegram.
    pub callback_query: Option<TelegramCallbackQuery>,
}

/// A button tap on an inline keyboard, mapped from Telegram's
/// `callback_query` update payload.
#[derive(Debug, Clone, Deserialize)]
pub struct TelegramCallbackQuery {
    pub id: String,
    pub from: TelegramUser,
    pub message: Option<TelegramMessage>,
    /// The `callback_data` string we set when sending the keyboard.
    pub data: Option<String>,
}

/// A drained callback_query, surfaced from the poll loop so the
/// approval router can resolve the corresponding pending question.
#[derive(Debug, Clone)]
pub struct TelegramCallbackEvent {
    pub callback_id: String,
    pub data: String,
    pub from_user_id: i64,
    /// The chat the original keyboard message lives in. Needed to edit
    /// the message after the user has answered.
    pub chat_id: Option<i64>,
    pub message_id: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TelegramMessage {
    pub message_id: i64,
    pub from: Option<TelegramUser>,
    pub chat: TelegramChat,
    pub date: i64,
    pub text: Option<String>,
    pub caption: Option<String>,
    /// Photo sizes (Telegram delivers the same image at multiple
    /// resolutions). We pick the largest one that fits the download cap.
    #[serde(default)]
    pub photo: Option<Vec<TelegramPhotoSize>>,
    /// Generic file attachment (PDFs, archives, etc.).
    #[serde(default)]
    pub document: Option<TelegramDocument>,
    /// Voice notes (Opus-encoded audio).
    #[serde(default)]
    pub voice: Option<TelegramVoice>,
    /// Audio file with metadata (music, podcasts).
    #[serde(default)]
    pub audio: Option<TelegramAudio>,
    /// Video file (.mp4 etc.).
    #[serde(default)]
    pub video: Option<TelegramVideo>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TelegramUser {
    pub id: i64,
    pub first_name: String,
    pub username: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TelegramChat {
    pub id: i64,
    #[serde(rename = "type")]
    pub chat_type: String,
}

/// One resolution of a photo. Telegram returns several per message.
#[derive(Debug, Clone, Deserialize)]
pub struct TelegramPhotoSize {
    pub file_id: String,
    pub file_size: Option<u64>,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TelegramDocument {
    pub file_id: String,
    pub file_name: Option<String>,
    pub mime_type: Option<String>,
    pub file_size: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TelegramVoice {
    pub file_id: String,
    pub duration: u32,
    pub mime_type: Option<String>,
    pub file_size: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TelegramAudio {
    pub file_id: String,
    pub duration: u32,
    pub file_name: Option<String>,
    pub mime_type: Option<String>,
    pub file_size: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TelegramVideo {
    pub file_id: String,
    pub duration: u32,
    pub file_name: Option<String>,
    pub mime_type: Option<String>,
    pub file_size: Option<u64>,
}

/// `getFile` response payload. Combined with the bot token, the
/// `file_path` becomes a download URL of
/// `https://api.telegram.org/file/bot<TOKEN>/<file_path>`.
#[derive(Debug, Clone, Deserialize)]
struct TelegramFileMeta {
    #[allow(dead_code)]
    file_id: String,
    file_path: Option<String>,
    file_size: Option<u64>,
}

// ---------------------------------------------------------------------------
// TelegramMonitor
// ---------------------------------------------------------------------------

/// Telegram Bot API sense monitor.
///
/// Polls `getUpdates` for new messages, converts them to [`SenseEvent`]s,
/// and tracks the last processed `update_id` to avoid duplicates.
pub struct TelegramMonitor {
    config: TelegramConfig,
    client: reqwest::Client,
    last_update_id: Mutex<Option<i64>>,
    /// Callback-query events collected during `process_updates` and
    /// drained by callers via [`TelegramMonitor::take_callbacks`].
    callbacks: Mutex<Vec<TelegramCallbackEvent>>,
    /// Optional cross-channel owner resolver. When wired, the per-poll
    /// flow fetches the owner's identifier set via async lookup and
    /// passes it into the sync `process_updates`. Falls back to the
    /// legacy `TelegramConfig::owner_user_id` for first-boot / when no
    /// store is available.
    owner_lookup: Option<Arc<OwnerLookup>>,
}

impl TelegramMonitor {
    /// Create a new `TelegramMonitor` from the given config.
    pub fn new(config: TelegramConfig) -> Self {
        Self {
            config,
            client: reqwest::Client::new(),
            last_update_id: Mutex::new(None),
            callbacks: Mutex::new(Vec::new()),
            owner_lookup: None,
        }
    }

    /// Attach an `OwnerLookup` so inbound messages can be cross-checked
    /// against the unified owner contact instead of (or in addition to)
    /// the legacy `TelegramConfig::owner_user_id`.
    pub fn with_owner_lookup(mut self, lookup: Arc<OwnerLookup>) -> Self {
        self.owner_lookup = Some(lookup);
        self
    }

    /// Resolve the owner's Telegram user ids (as strings) for the
    /// current poll tick. Returns the cached snapshot from
    /// `OwnerLookup`, augmented with `config.owner_user_id` if present —
    /// the legacy fallback is intentional so first-boot before the
    /// migration runs still treats the user as owner.
    async fn current_owner_telegram_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = Vec::new();
        if let Some(ref lookup) = self.owner_lookup {
            for (scheme, value) in lookup.owner_identifiers().await {
                if scheme == "telegram_user" {
                    ids.push(value);
                }
            }
        }
        if let Some(legacy) = self.config.owner_user_id {
            let s = legacy.to_string();
            if !ids.contains(&s) {
                ids.push(s);
            }
        }
        ids
    }

    /// Drain accumulated callback-query events. Called by the host after
    /// each poll tick to forward inline-keyboard taps to the approval
    /// router.
    pub fn take_callbacks(&self) -> Vec<TelegramCallbackEvent> {
        let mut guard = self.callbacks.lock().unwrap();
        std::mem::take(&mut *guard)
    }

    /// Base URL for the Telegram Bot API.
    fn api_url(&self, method: &str) -> String {
        format!(
            "https://api.telegram.org/bot{}/{}",
            self.config.bot_token, method
        )
    }

    /// Convert a list of Telegram updates into [`SenseEvent`]s.
    ///
    /// This method is public so it can be tested in isolation without
    /// making HTTP calls. `owner_telegram_ids` is the snapshot of
    /// Telegram user ids that should be treated as the owner for this
    /// batch — `poll` builds it via [`OwnerLookup`] + the legacy config
    /// fallback before calling in; tests pass the set directly.
    pub fn process_updates_with_owner(
        &self,
        updates: Vec<TelegramUpdate>,
        owner_telegram_ids: &[String],
    ) -> Vec<SenseEvent> {
        let mut events = Vec::new();
        let mut max_id: Option<i64> = None;

        for update in updates {
            // Track the highest update_id we have seen.
            max_id = Some(max_id.map_or(update.update_id, |m| m.max(update.update_id)));

            // Capture callback_query updates (inline-keyboard taps) into
            // the callbacks queue so the host can route them to the
            // approval router after the poll tick.
            if let Some(cb) = update.callback_query.as_ref() {
                if let Some(data) = cb.data.clone() {
                    tracing::info!(
                        callback_id = %cb.id,
                        data = %data,
                        from_user_id = cb.from.id,
                        "Telegram callback_query buffered"
                    );
                    let event = TelegramCallbackEvent {
                        callback_id: cb.id.clone(),
                        data,
                        from_user_id: cb.from.id,
                        chat_id: cb.message.as_ref().map(|m| m.chat.id),
                        message_id: cb.message.as_ref().map(|m| m.message_id),
                    };
                    self.callbacks.lock().unwrap().push(event);
                } else {
                    tracing::warn!(
                        callback_id = %cb.id,
                        "Telegram callback_query received without data field"
                    );
                }
            }

            let message = match update.message {
                Some(m) => m,
                None => continue, // skip non-message updates (edited, channel_post, etc.)
            };

            // Filter by allowed chat IDs if configured.
            if !self.config.allowed_chat_ids.is_empty()
                && !self.config.allowed_chat_ids.contains(&message.chat.id)
            {
                tracing::debug!(
                    chat_id = message.chat.id,
                    "Skipping message from non-allowed chat"
                );
                continue;
            }

            // Extract text content: prefer `text`, fall back to `caption`.
            let text_opt = message
                .text
                .as_deref()
                .or(message.caption.as_deref())
                .filter(|t| !t.is_empty())
                .map(|t| t.to_string());

            // Extract media into Attachment records. We only set
            // metadata + AttachmentSource here; bytes get fetched in a
            // follow-up async pass (see [`fetch_pending_attachments`]).
            let attachments = extract_attachments(&message);

            // Skip updates with no textual content AND no media. Pure
            // system events (chat-action, etc.) don't deserve a sense
            // event — they'd just be noise to the agent.
            if text_opt.is_none() && attachments.is_empty() {
                continue;
            }

            // Determine risk based on sender vs owner. Cross-channel
            // owner identity lives in the contact store: we match by
            // numeric user_id (the canonical Telegram identifier), with
            // a legacy fallback to `TelegramConfig::owner_user_id` so
            // first-boot before the migration runs still resolves the
            // owner correctly.
            let is_owner_msg = message
                .from
                .as_ref()
                .map(|user| {
                    let uid = user.id.to_string();
                    owner_telegram_ids.iter().any(|o| o == &uid)
                })
                .unwrap_or(false);

            let source_risk = if is_owner_msg {
                RiskLevel::Safe // L1
            } else {
                RiskLevel::Caution // L2
            };

            // Build sender info. We always use the numeric user_id as
            // the canonical identifier so the contact-store lookup in
            // the coordinator picks up the unified owner contact (whose
            // attached identifier is the user_id). The display name
            // still carries the friendlier name/@username.
            let sender = message.from.as_ref().map(|user| {
                let display = if let Some(ref uname) = user.username {
                    format!("{} (@{})", user.first_name, uname)
                } else {
                    user.first_name.clone()
                };
                SenderInfo {
                    identifier: user.id.to_string(),
                    contact_id: None,
                    display_name: Some(display),
                }
            });

            let timestamp: DateTime<Utc> = Utc
                .timestamp_opt(message.date, 0)
                .single()
                .unwrap_or_else(Utc::now);

            // Summary: use the text/caption if present; otherwise
            // synthesise from the media kinds so a media-only message
            // still triggers a useful sense event ("[photo]",
            // "[document: invoice.pdf]") instead of being dropped.
            let text_for_body = text_opt.clone().unwrap_or_default();
            let summary = match text_opt.as_deref() {
                Some(t) if !t.is_empty() => {
                    if t.len() > 100 {
                        let cap = t.floor_char_boundary(97);
                        format!("{}...", &t[..cap])
                    } else {
                        t.to_string()
                    }
                }
                _ => synthesise_media_summary(&attachments),
            };

            let body = serde_json::json!({
                "text": text_for_body,
                "chat_id": message.chat.id,
                "chat_type": message.chat.chat_type,
                "message_id": message.message_id,
                "sender_user_id": message.from.as_ref().map(|u| u.id),
                "sender_username": message.from.as_ref().and_then(|u| u.username.as_deref()),
                "sender_first_name": message.from.as_ref().map(|u| u.first_name.as_str()),
                "has_media": !attachments.is_empty(),
            });

            events.push(SenseEvent {
                id: Uuid::new_v4(),
                timestamp,
                source: EventSource::Messaging,
                kind: EventKind::NewMessage,
                sender,
                content: NormalizedContent {
                    summary: Some(summary),
                    body,
                    attachments,
                },
                source_risk,
                raw_id: Some(format!("telegram-{}", message.message_id)),
            });
        }

        // Persist max update_id for offset tracking.
        if let Some(max) = max_id {
            let mut guard = self.last_update_id.lock().unwrap();
            *guard = Some(max);
        }

        events
    }

    /// Back-compat wrapper that derives the owner id list from the
    /// legacy `TelegramConfig::owner_user_id` only. New code should
    /// build a snapshot via [`OwnerLookup`] and call
    /// [`process_updates_with_owner`] directly — kept here so existing
    /// tests and call sites compile without churn.
    pub fn process_updates(&self, updates: Vec<TelegramUpdate>) -> Vec<SenseEvent> {
        let owner_ids: Vec<String> = self
            .config
            .owner_user_id
            .map(|id| vec![id.to_string()])
            .unwrap_or_default();
        self.process_updates_with_owner(updates, &owner_ids)
    }

    /// Walk every attachment on `events` whose source is Telegram and
    /// `local_path` is `None`, pull the bytes via `getFile` + the file
    /// download endpoint, and save them under `<save_root>/<event_id>/`.
    /// On failure for a single file the others still proceed; the
    /// failed attachment stays metadata-only and the agent can still
    /// see it existed.
    pub async fn fetch_pending_attachments(
        &self,
        events: &mut [SenseEvent],
        save_root: Option<&Path>,
    ) {
        for event in events.iter_mut() {
            for att in event.content.attachments.iter_mut() {
                if att.local_path.is_some() {
                    continue;
                }
                let file_id = match &att.source {
                    Some(AttachmentSource::Telegram { file_id, .. }) => file_id.clone(),
                    _ => continue,
                };
                if att.size_bytes > TELEGRAM_MAX_DOWNLOAD_BYTES {
                    tracing::info!(
                        name = %att.name,
                        size = att.size_bytes,
                        "Telegram attachment exceeds 20 MiB cap; metadata-only"
                    );
                    continue;
                }
                match self
                    .download_telegram_file(&file_id, event.id, &att.name, save_root)
                    .await
                {
                    Ok(Some(path)) => {
                        // Eager PDF text extraction on a blocking pool —
                        // pdf-extract is CPU-bound and sync, so we keep
                        // it off the runtime worker thread.
                        if att.mime_type.starts_with("application/pdf") {
                            let pdf_path = path.clone();
                            match tokio::task::spawn_blocking(move || {
                                crate::pdf_extract::extract_to_sidecar(&pdf_path)
                            })
                            .await
                            {
                                Ok(Ok(sidecar)) => att.extracted_text_path = Some(sidecar),
                                Ok(Err(e)) => {
                                    tracing::warn!("pdf-extract sidecar failed for {path:?}: {e}")
                                }
                                Err(e) => tracing::warn!(
                                    "pdf-extract spawn_blocking panicked for {path:?}: {e}"
                                ),
                            }
                        }
                        att.local_path = Some(path);
                    }
                    Ok(None) => {} // no save_root configured — degrade silently
                    Err(e) => tracing::warn!(
                        file_id = %file_id,
                        "Telegram getFile/download failed: {e}"
                    ),
                }
            }
        }
    }

    async fn download_telegram_file(
        &self,
        file_id: &str,
        event_id: Uuid,
        name: &str,
        save_root: Option<&Path>,
    ) -> Result<Option<PathBuf>> {
        let Some(root) = save_root else {
            return Ok(None);
        };

        // Step 1: getFile to learn the file_path.
        let meta_url = self.api_url("getFile");
        let meta: TelegramResponse<TelegramFileMeta> = self
            .client
            .get(&meta_url)
            .query(&[("file_id", file_id)])
            .timeout(Duration::from_secs(15))
            .send()
            .await
            .map_err(|e| AthenError::Other(format!("getFile request: {e}")))?
            .json()
            .await
            .map_err(|e| AthenError::Other(format!("getFile parse: {e}")))?;

        if !meta.ok {
            return Err(AthenError::Other(format!(
                "Telegram getFile not ok: {}",
                meta.description.unwrap_or_default()
            )));
        }
        let result = meta
            .result
            .ok_or_else(|| AthenError::Other("getFile missing result".into()))?;
        let file_path = result
            .file_path
            .ok_or_else(|| AthenError::Other("getFile missing file_path".into()))?;

        // Defence-in-depth: re-check the size returned by getFile
        // against our cap. The TelegramMessage size hint can lie or be
        // missing; this is the authoritative number.
        if let Some(sz) = result.file_size {
            if sz > TELEGRAM_MAX_DOWNLOAD_BYTES {
                return Err(AthenError::Other(format!(
                    "telegram file too large: {sz} bytes"
                )));
            }
        }

        // Step 2: download the bytes from the file endpoint.
        let download_url = format!(
            "https://api.telegram.org/file/bot{}/{}",
            self.config.bot_token, file_path
        );
        let bytes = self
            .client
            .get(&download_url)
            .timeout(Duration::from_secs(60))
            .send()
            .await
            .map_err(|e| AthenError::Other(format!("download request: {e}")))?
            .bytes()
            .await
            .map_err(|e| AthenError::Other(format!("download body: {e}")))?;

        // Sanitise + save under <root>/<event_id>/<sanitized_name>.
        let dir = root.join(event_id.to_string());
        if let Err(e) = std::fs::create_dir_all(&dir) {
            return Err(AthenError::Other(format!(
                "create attachment dir {dir:?}: {e}"
            )));
        }
        let path = dir.join(sanitize_filename(name));
        std::fs::write(&path, &bytes)
            .map_err(|e| AthenError::Other(format!("write {path:?}: {e}")))?;
        Ok(Some(path))
    }
}

/// Pull every supported media kind out of a Telegram message into
/// `Attachment` records. Metadata only — bytes get filled in later by
/// [`TelegramMonitor::fetch_pending_attachments`].
fn extract_attachments(message: &TelegramMessage) -> Vec<Attachment> {
    let mut out = Vec::new();
    let chat_id = message.chat.id;
    let message_id = message.message_id;

    // photo[] — pick the largest size that fits the download cap.
    if let Some(photos) = message.photo.as_ref() {
        if let Some(best) = pick_best_photo(photos) {
            out.push(make_attachment(
                format!("photo_{message_id}.jpg"),
                "image/jpeg".into(),
                best.file_size.unwrap_or(0),
                &best.file_id,
                chat_id,
                message_id,
            ));
        }
    }

    if let Some(doc) = message.document.as_ref() {
        out.push(make_attachment(
            doc.file_name
                .clone()
                .unwrap_or_else(|| format!("document_{message_id}")),
            doc.mime_type
                .clone()
                .unwrap_or_else(|| "application/octet-stream".into()),
            doc.file_size.unwrap_or(0),
            &doc.file_id,
            chat_id,
            message_id,
        ));
    }

    if let Some(voice) = message.voice.as_ref() {
        out.push(make_attachment(
            format!("voice_{message_id}.ogg"),
            voice
                .mime_type
                .clone()
                .unwrap_or_else(|| "audio/ogg".into()),
            voice.file_size.unwrap_or(0),
            &voice.file_id,
            chat_id,
            message_id,
        ));
    }

    if let Some(audio) = message.audio.as_ref() {
        out.push(make_attachment(
            audio
                .file_name
                .clone()
                .unwrap_or_else(|| format!("audio_{message_id}.mp3")),
            audio
                .mime_type
                .clone()
                .unwrap_or_else(|| "audio/mpeg".into()),
            audio.file_size.unwrap_or(0),
            &audio.file_id,
            chat_id,
            message_id,
        ));
    }

    if let Some(video) = message.video.as_ref() {
        out.push(make_attachment(
            video
                .file_name
                .clone()
                .unwrap_or_else(|| format!("video_{message_id}.mp4")),
            video
                .mime_type
                .clone()
                .unwrap_or_else(|| "video/mp4".into()),
            video.file_size.unwrap_or(0),
            &video.file_id,
            chat_id,
            message_id,
        ));
    }

    out
}

fn make_attachment(
    name: String,
    mime_type: String,
    size_bytes: u64,
    file_id: &str,
    chat_id: i64,
    message_id: i64,
) -> Attachment {
    Attachment::new(
        name,
        mime_type,
        size_bytes,
        None,
        Some(AttachmentSource::Telegram {
            chat_id,
            message_id,
            file_id: file_id.to_string(),
        }),
    )
}

/// Telegram returns photos at multiple resolutions — pick the largest
/// one that's still under our download cap.
fn pick_best_photo(photos: &[TelegramPhotoSize]) -> Option<&TelegramPhotoSize> {
    photos
        .iter()
        .filter(|p| {
            p.file_size
                .map(|s| s <= TELEGRAM_MAX_DOWNLOAD_BYTES)
                .unwrap_or(true)
        })
        .max_by_key(|p| (p.width as u64) * (p.height as u64))
}

/// Build a synthetic summary like `"[photo]"` or `"[document: foo.pdf]"`
/// for media-only messages so they still produce a useful sense event.
fn synthesise_media_summary(attachments: &[Attachment]) -> String {
    if attachments.is_empty() {
        return "[empty message]".into();
    }
    let parts: Vec<String> = attachments
        .iter()
        .map(|a| {
            let (kind, has_user_name) = classify_attachment(a);
            if has_user_name {
                format!("[{kind}: {}]", a.name)
            } else {
                format!("[{kind}]")
            }
        })
        .collect();
    parts.join(" ")
}

/// Returns the human label for an attachment + whether the filename
/// looks user-supplied (`true`) or auto-synthesised by us
/// (`"voice_NN.ogg"` etc., `false`). Used to decide whether to put the
/// name in the synthesised summary — auto-names are noise.
fn classify_attachment(a: &Attachment) -> (&'static str, bool) {
    let synthetic_prefixes = ["photo_", "voice_", "audio_", "video_", "document_"];
    let auto_synthesised =
        synthetic_prefixes.iter().any(|p| a.name.starts_with(p)) || a.name.is_empty();

    let kind = if a.mime_type.starts_with("image/") {
        "photo"
    } else if a.mime_type.starts_with("audio/") && a.name.starts_with("voice_") {
        "voice note"
    } else if a.mime_type.starts_with("audio/") {
        "audio"
    } else if a.mime_type.starts_with("video/") {
        "video"
    } else {
        "document"
    };

    (kind, !auto_synthesised)
}

/// Same sanitiser as the email sense — drop directory separators and
/// risky characters, fall back to `"file"` if the result is empty.
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

#[async_trait]
impl SenseMonitor for TelegramMonitor {
    fn sense_id(&self) -> &str {
        "telegram"
    }

    async fn init(&mut self, _config: &AthenConfig) -> Result<()> {
        if !self.config.enabled {
            tracing::info!("TelegramMonitor disabled");
            return Ok(());
        }

        if self.config.bot_token.is_empty() {
            return Err(AthenError::Config(
                "Telegram bot_token is empty".to_string(),
            ));
        }

        // Validate the token by calling getMe.
        let url = self.api_url("getMe");
        let resp = self
            .client
            .get(&url)
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(|e| AthenError::Other(format!("Telegram getMe request failed: {e}")))?;

        let body: TelegramResponse<serde_json::Value> = resp
            .json()
            .await
            .map_err(|e| AthenError::Other(format!("Telegram getMe parse failed: {e}")))?;

        if !body.ok {
            return Err(AthenError::Config(format!(
                "Telegram getMe failed: {}",
                body.description.unwrap_or_default()
            )));
        }

        if let Some(result) = body.result {
            let username = result
                .get("username")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            tracing::info!(bot_username = %username, "TelegramMonitor initialized");
        }

        // Best-effort: clear any stale webhook so getUpdates is the
        // active delivery mechanism. Without this, `getUpdates` would
        // return an error like "Conflict: can't use getUpdates while
        // webhook is set" and we'd never see any updates at all,
        // including callback_query.
        let delete_webhook_url = self.api_url("deleteWebhook");
        match self
            .client
            .post(&delete_webhook_url)
            .timeout(Duration::from_secs(5))
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => {
                tracing::debug!("Cleared any stale Telegram webhook");
            }
            Ok(r) => {
                tracing::warn!("Telegram deleteWebhook returned {}", r.status());
            }
            Err(e) => {
                tracing::warn!("Telegram deleteWebhook failed (non-fatal): {e}");
            }
        }

        Ok(())
    }

    async fn poll(&self) -> Result<Vec<SenseEvent>> {
        if !self.config.enabled || self.config.bot_token.is_empty() {
            return Ok(Vec::new());
        }

        let offset = {
            let guard = self.last_update_id.lock().unwrap();
            guard.map(|id| id + 1)
        };

        // Explicitly opt in to `callback_query` updates — by default
        // Telegram remembers the previous `allowed_updates` setting,
        // so a bot that was ever started with `["message"]` (or had a
        // webhook configured) would silently never receive button
        // taps. Sending an explicit list each call resets that.
        //
        // Build the URL by hand so it's easy to log and verify in
        // production without dumping every reqwest builder field.
        let base = self.api_url("getUpdates");
        let allowed_updates_param = urlencode_param("[\"message\",\"callback_query\"]");
        let mut url = format!("{base}?timeout=0&allowed_updates={allowed_updates_param}");
        if let Some(off) = offset {
            url.push_str(&format!("&offset={off}"));
        }
        tracing::debug!(url = %url, "Telegram getUpdates URL");

        let resp = self
            .client
            .get(&url)
            .timeout(Duration::from_secs(15))
            .send()
            .await
            .map_err(|e| AthenError::Other(format!("Telegram getUpdates failed: {e}")))?;

        // Read body as text first so we can log it on parse error AND
        // count update kinds (message vs callback_query) without a
        // second deserialization pass.
        let body_text = resp
            .text()
            .await
            .map_err(|e| AthenError::Other(format!("Telegram getUpdates body read: {e}")))?;

        let body: TelegramResponse<Vec<TelegramUpdate>> = serde_json::from_str(&body_text)
            .map_err(|e| {
                tracing::error!(
                    body = %body_text.chars().take(500).collect::<String>(),
                    "Telegram getUpdates parse failed: {e}"
                );
                AthenError::Other(format!("Telegram getUpdates parse failed: {e}"))
            })?;

        if !body.ok {
            return Err(AthenError::Other(format!(
                "Telegram getUpdates error: {}",
                body.description.unwrap_or_default()
            )));
        }

        let updates = body.result.unwrap_or_default();

        if !updates.is_empty() {
            // Count kinds to make it obvious in logs whether
            // callback_query updates are arriving at all.
            let mut msg_count = 0;
            let mut cb_count = 0;
            for u in &updates {
                if u.message.is_some() {
                    msg_count += 1;
                }
                if u.callback_query.is_some() {
                    cb_count += 1;
                }
            }
            tracing::info!(
                total = updates.len(),
                messages = msg_count,
                callbacks = cb_count,
                "Telegram getUpdates returned updates"
            );
            // If a callback was returned, also log the raw JSON keys of
            // the first update so we can see what Telegram is actually
            // sending vs. what our struct expects.
            if cb_count == 0 && msg_count == 0 {
                tracing::warn!(
                    body = %body_text.chars().take(500).collect::<String>(),
                    "Telegram returned updates but none parsed as message or callback_query"
                );
            }
        }

        // Snapshot the owner's Telegram identifiers once per poll so we
        // don't hit the store per message. Falls back to the legacy
        // config when no lookup is wired (CLI tests, first-boot before
        // migration).
        let owner_telegram_ids = self.current_owner_telegram_ids().await;
        let mut events = self.process_updates_with_owner(updates, &owner_telegram_ids);
        // Second pass: pull the bytes for every Telegram-sourced
        // attachment. Failures are logged but don't fail the poll —
        // the agent still gets the metadata.
        let save_root = athen_core::paths::athen_attachments_dir();
        self.fetch_pending_attachments(&mut events, save_root.as_deref())
            .await;
        Ok(events)
    }

    fn poll_interval(&self) -> Duration {
        Duration::from_secs(self.config.poll_interval_secs)
    }

    async fn shutdown(&self) -> Result<()> {
        tracing::info!("TelegramMonitor shutting down");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Public utility: send a message via the Telegram Bot API
// ---------------------------------------------------------------------------

/// Send a text message to a Telegram chat via the Bot API.
///
/// Handles the 4096-character limit by splitting into multiple messages.
/// Tiny URL-encoder for query parameter values. Escapes the characters
/// that Telegram's getUpdates is sensitive about (`[`, `]`, `"`, `,`)
/// without pulling in a full url-encoding crate just for this.
fn urlencode_param(value: &str) -> String {
    let mut out = String::with_capacity(value.len() * 2);
    for b in value.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                out.push_str(&format!("%{b:02X}"));
            }
        }
    }
    out
}

pub async fn send_message(
    bot_token: &str,
    chat_id: i64,
    text: &str,
) -> std::result::Result<(), String> {
    let client = reqwest::Client::new();
    let url = format!("https://api.telegram.org/bot{}/sendMessage", bot_token);

    // Telegram has a 4096 character limit per message.  Split if needed.
    let chunks: Vec<&str> = if text.len() <= 4096 {
        vec![text]
    } else {
        text.as_bytes()
            .chunks(4096)
            .map(|c| std::str::from_utf8(c).unwrap_or(""))
            .collect()
    };

    for chunk in chunks {
        if chunk.is_empty() {
            continue;
        }
        let resp = client
            .post(&url)
            .json(&serde_json::json!({
                "chat_id": chat_id,
                "text": chunk,
            }))
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await
            .map_err(|e| format!("Failed to send Telegram message: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("Telegram API error {status}: {body}"));
        }
    }

    Ok(())
}

/// Send a single text message and return its `message_id`, so the
/// caller can later edit it via [`edit_message_text`]. Unlike
/// [`send_message`] this does not chunk — pass text under the 4096-char
/// Bot API limit. Used by the live-progress reporter, which posts one
/// status message and then mutates it as the agent works.
pub async fn send_message_returning_id(
    bot_token: &str,
    chat_id: i64,
    text: &str,
) -> std::result::Result<i64, String> {
    let client = reqwest::Client::new();
    let url = format!("https://api.telegram.org/bot{bot_token}/sendMessage");
    let resp = client
        .post(&url)
        .json(&serde_json::json!({
            "chat_id": chat_id,
            "text": text,
        }))
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| format!("Failed to send Telegram message: {e}"))?;
    let status = resp.status();
    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse Telegram response: {e}"))?;
    if !status.is_success() || !body.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
        return Err(format!("Telegram sendMessage error {status}: {body}"));
    }
    body.get("result")
        .and_then(|r| r.get("message_id"))
        .and_then(|v| v.as_i64())
        .ok_or_else(|| "Telegram response missing message_id".to_string())
}

/// Send a chat action (e.g. "typing") so Telegram clients show the
/// activity indicator. The indicator only persists ~5s, so callers
/// that want a sustained "typing…" must call this on a loop.
pub async fn send_chat_action(
    bot_token: &str,
    chat_id: i64,
    action: &str,
) -> std::result::Result<(), String> {
    let client = reqwest::Client::new();
    let url = format!("https://api.telegram.org/bot{bot_token}/sendChatAction");
    let resp = client
        .post(&url)
        .json(&serde_json::json!({
            "chat_id": chat_id,
            "action": action,
        }))
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| format!("Failed to send chat action: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("Telegram sendChatAction {status}: {body}"));
    }
    Ok(())
}

/// Send a text message with an inline keyboard via the Bot API.
///
/// `buttons` is a single horizontal row of `(label, callback_data)`
/// pairs. Returns the `message_id` of the sent message so the caller
/// can later edit it (e.g. to confirm the user's choice).
pub async fn send_message_with_keyboard(
    bot_token: &str,
    chat_id: i64,
    text: &str,
    buttons: &[(&str, &str)],
) -> std::result::Result<i64, String> {
    let client = reqwest::Client::new();
    let url = format!("https://api.telegram.org/bot{bot_token}/sendMessage");

    let row: Vec<serde_json::Value> = buttons
        .iter()
        .map(|(label, data)| {
            serde_json::json!({
                "text": label,
                "callback_data": data,
            })
        })
        .collect();

    let resp = client
        .post(&url)
        .json(&serde_json::json!({
            "chat_id": chat_id,
            "text": text,
            "reply_markup": { "inline_keyboard": [row] },
        }))
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| format!("Failed to send keyboard message: {e}"))?;

    let status = resp.status();
    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse Telegram response: {e}"))?;
    if !status.is_success() || !body.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
        return Err(format!("Telegram sendMessage error {status}: {body}"));
    }
    let message_id = body
        .get("result")
        .and_then(|r| r.get("message_id"))
        .and_then(|v| v.as_i64())
        .ok_or_else(|| "Telegram response missing message_id".to_string())?;
    Ok(message_id)
}

/// Acknowledge a callback_query so the user's button stops showing the
/// loading spinner. `text`, if non-empty, is shown as a tooltip.
pub async fn answer_callback_query(
    bot_token: &str,
    callback_id: &str,
    text: &str,
) -> std::result::Result<(), String> {
    let client = reqwest::Client::new();
    let url = format!("https://api.telegram.org/bot{bot_token}/answerCallbackQuery");
    let resp = client
        .post(&url)
        .json(&serde_json::json!({
            "callback_query_id": callback_id,
            "text": text,
        }))
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| format!("Failed to answer callback: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("Telegram answerCallbackQuery {status}: {body}"));
    }
    Ok(())
}

/// Edit a message's text (e.g. to remove the keyboard and confirm the
/// user's choice after they answered an approval question).
pub async fn edit_message_text(
    bot_token: &str,
    chat_id: i64,
    message_id: i64,
    text: &str,
) -> std::result::Result<(), String> {
    let client = reqwest::Client::new();
    let url = format!("https://api.telegram.org/bot{bot_token}/editMessageText");
    let resp = client
        .post(&url)
        .json(&serde_json::json!({
            "chat_id": chat_id,
            "message_id": message_id,
            "text": text,
        }))
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| format!("Failed to edit message: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("Telegram editMessageText {status}: {body}"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: build a default disabled config for testing.
    fn test_config() -> TelegramConfig {
        TelegramConfig {
            enabled: true,
            bot_token: "123456:ABC-DEF".to_string(),
            owner_user_id: Some(42),
            allowed_chat_ids: vec![],
            poll_interval_secs: 5,
        }
    }

    /// Helper: build a TelegramUpdate with a text message.
    fn make_text_update(
        update_id: i64,
        message_id: i64,
        user_id: i64,
        first_name: &str,
        username: Option<&str>,
        chat_id: i64,
        text: &str,
    ) -> TelegramUpdate {
        TelegramUpdate {
            update_id,
            message: Some(TelegramMessage {
                message_id,
                from: Some(TelegramUser {
                    id: user_id,
                    first_name: first_name.to_string(),
                    username: username.map(|s| s.to_string()),
                }),
                chat: TelegramChat {
                    id: chat_id,
                    chat_type: "private".to_string(),
                },
                date: 1700000000,
                text: Some(text.to_string()),
                caption: None,
                photo: None,
                document: None,
                voice: None,
                audio: None,
                video: None,
            }),
            callback_query: None,
        }
    }

    /// Build a TelegramUpdate carrying a callback_query (inline-button tap).
    fn make_callback_update(
        update_id: i64,
        callback_id: &str,
        from_user_id: i64,
        data: &str,
        chat_id: i64,
        message_id: i64,
    ) -> TelegramUpdate {
        TelegramUpdate {
            update_id,
            message: None,
            callback_query: Some(TelegramCallbackQuery {
                id: callback_id.to_string(),
                from: TelegramUser {
                    id: from_user_id,
                    first_name: "Owner".into(),
                    username: None,
                },
                message: Some(TelegramMessage {
                    message_id,
                    from: None,
                    chat: TelegramChat {
                        id: chat_id,
                        chat_type: "private".to_string(),
                    },
                    date: 1700000000,
                    text: Some("Approve?".into()),
                    caption: None,
                    photo: None,
                    document: None,
                    voice: None,
                    audio: None,
                    video: None,
                }),
                data: Some(data.to_string()),
            }),
        }
    }

    // ---------------------------------------------------------------
    // Basic properties
    // ---------------------------------------------------------------

    #[test]
    fn construction_with_config() {
        let config = test_config();
        let monitor = TelegramMonitor::new(config.clone());
        assert_eq!(monitor.config.bot_token, "123456:ABC-DEF");
        assert_eq!(monitor.config.owner_user_id, Some(42));
        assert!(monitor.last_update_id.lock().unwrap().is_none());
    }

    #[test]
    fn sense_id_is_telegram() {
        let monitor = TelegramMonitor::new(test_config());
        assert_eq!(monitor.sense_id(), "telegram");
    }

    #[test]
    fn poll_interval_from_config() {
        let mut config = test_config();
        config.poll_interval_secs = 10;
        let monitor = TelegramMonitor::new(config);
        assert_eq!(monitor.poll_interval(), Duration::from_secs(10));
    }

    #[test]
    fn default_poll_interval_is_5s() {
        let config = TelegramConfig::default();
        assert_eq!(config.poll_interval_secs, 5);
    }

    #[test]
    fn callback_queries_are_collected_and_drainable() {
        let monitor = TelegramMonitor::new(test_config());
        let updates = vec![
            make_callback_update(1, "cb-1", 42, "qid-1|approve", 999, 7),
            make_text_update(2, 8, 42, "Alex", None, 999, "hi"),
            make_callback_update(3, "cb-2", 42, "qid-2|deny", 999, 9),
        ];

        let events = monitor.process_updates(updates);
        // The text message yielded a SenseEvent; the callback queries did not.
        assert_eq!(events.len(), 1);

        let drained = monitor.take_callbacks();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].callback_id, "cb-1");
        assert_eq!(drained[0].data, "qid-1|approve");
        assert_eq!(drained[0].from_user_id, 42);
        assert_eq!(drained[0].chat_id, Some(999));
        assert_eq!(drained[0].message_id, Some(7));
        assert_eq!(drained[1].data, "qid-2|deny");

        // A second drain returns empty (drained on read).
        assert!(monitor.take_callbacks().is_empty());
    }

    #[test]
    fn callback_query_without_data_is_skipped() {
        let monitor = TelegramMonitor::new(test_config());
        let mut update = make_callback_update(1, "cb-1", 42, "x", 999, 7);
        if let Some(ref mut cq) = update.callback_query {
            cq.data = None;
        }
        let _ = monitor.process_updates(vec![update]);
        assert!(monitor.take_callbacks().is_empty());
    }

    // ---------------------------------------------------------------
    // JSON deserialization of Telegram API responses
    // ---------------------------------------------------------------

    #[test]
    fn parse_valid_get_updates_response() {
        let json = r#"{
            "ok": true,
            "result": [
                {
                    "update_id": 100,
                    "message": {
                        "message_id": 1,
                        "from": { "id": 42, "first_name": "Alex", "username": "alexdev" },
                        "chat": { "id": 42, "type": "private" },
                        "date": 1700000000,
                        "text": "Hello bot!"
                    }
                }
            ]
        }"#;

        let resp: TelegramResponse<Vec<TelegramUpdate>> =
            serde_json::from_str(json).expect("parse failed");
        assert!(resp.ok);
        let updates = resp.result.unwrap();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].update_id, 100);
        let msg = updates[0].message.as_ref().unwrap();
        assert_eq!(msg.text.as_deref(), Some("Hello bot!"));
        assert_eq!(msg.from.as_ref().unwrap().first_name, "Alex");
        assert_eq!(
            msg.from.as_ref().unwrap().username.as_deref(),
            Some("alexdev")
        );
    }

    #[test]
    fn parse_response_with_no_messages() {
        let json = r#"{ "ok": true, "result": [] }"#;
        let resp: TelegramResponse<Vec<TelegramUpdate>> =
            serde_json::from_str(json).expect("parse failed");
        assert!(resp.ok);
        assert!(resp.result.unwrap().is_empty());
    }

    #[test]
    fn parse_response_with_photo_caption() {
        let json = r#"{
            "ok": true,
            "result": [
                {
                    "update_id": 200,
                    "message": {
                        "message_id": 5,
                        "from": { "id": 99, "first_name": "Bob" },
                        "chat": { "id": 99, "type": "private" },
                        "date": 1700000000,
                        "caption": "Check out this photo!"
                    }
                }
            ]
        }"#;

        let resp: TelegramResponse<Vec<TelegramUpdate>> =
            serde_json::from_str(json).expect("parse failed");
        let updates = resp.result.unwrap();
        let msg = updates[0].message.as_ref().unwrap();
        assert!(msg.text.is_none());
        assert_eq!(msg.caption.as_deref(), Some("Check out this photo!"));
    }

    // ---------------------------------------------------------------
    // process_updates logic
    // ---------------------------------------------------------------

    #[test]
    fn process_updates_converts_text_message() {
        let monitor = TelegramMonitor::new(test_config());
        let updates = vec![make_text_update(
            100,
            1,
            99,
            "Bob",
            Some("bob123"),
            99,
            "Hello!",
        )];

        let events = monitor.process_updates(updates);
        assert_eq!(events.len(), 1);

        let event = &events[0];
        assert_eq!(event.source, EventSource::Messaging);
        assert!(matches!(event.kind, EventKind::NewMessage));
        assert_eq!(event.raw_id.as_deref(), Some("telegram-1"));
        assert_eq!(event.content.summary.as_deref(), Some("Hello!"));
        assert_eq!(event.content.body["text"], "Hello!");
        assert_eq!(event.content.body["chat_id"], 99);

        let sender = event.sender.as_ref().unwrap();
        // Identifier is the canonical numeric user_id (99) so the
        // contact-store owner lookup matches across Telegram clients
        // that may or may not have a username set.
        assert_eq!(sender.identifier, "99");
        assert_eq!(sender.display_name.as_deref(), Some("Bob (@bob123)"));
    }

    #[test]
    fn process_updates_caption_fallback() {
        let monitor = TelegramMonitor::new(test_config());
        let update = TelegramUpdate {
            update_id: 200,
            message: Some(TelegramMessage {
                message_id: 5,
                from: Some(TelegramUser {
                    id: 99,
                    first_name: "Carol".to_string(),
                    username: None,
                }),
                chat: TelegramChat {
                    id: 99,
                    chat_type: "private".to_string(),
                },
                date: 1700000000,
                text: None,
                caption: Some("Photo caption".to_string()),
                photo: None,
                document: None,
                voice: None,
                audio: None,
                video: None,
            }),
            callback_query: None,
        };

        let events = monitor.process_updates(vec![update]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].content.summary.as_deref(), Some("Photo caption"));
        // Sender without username uses user ID as identifier.
        let sender = events[0].sender.as_ref().unwrap();
        assert_eq!(sender.identifier, "99");
        assert_eq!(sender.display_name.as_deref(), Some("Carol"));
    }

    #[test]
    fn process_updates_owner_gets_l1_risk() {
        let monitor = TelegramMonitor::new(test_config());
        // owner_user_id is 42
        let updates = vec![make_text_update(
            100,
            1,
            42,
            "Alex",
            Some("alexdev"),
            42,
            "Owner message",
        )];

        let events = monitor.process_updates(updates);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].source_risk, RiskLevel::Safe); // L1
    }

    #[tokio::test]
    async fn process_updates_with_owner_via_lookup_marks_safe() {
        use athen_contacts::{ContactStore, InMemoryContactStore};
        use athen_core::contact::{Contact, ContactIdentifier, IdentifierKind, TrustLevel};

        let store: std::sync::Arc<dyn ContactStore> =
            std::sync::Arc::new(InMemoryContactStore::new());
        let owner = Contact {
            id: Uuid::new_v4(),
            name: "Owner".into(),
            trust_level: TrustLevel::AuthUser,
            trust_manual_override: true,
            identifiers: vec![ContactIdentifier {
                kind: IdentifierKind::Telegram,
                value: "777".into(),
            }],
            interaction_count: 0,
            last_interaction: None,
            notes: None,
            blocked: false,
            is_owner: true,
        };
        let id = owner.id;
        store.save(&owner).await.unwrap();
        store.set_owner(&id).await.unwrap();

        // No legacy fallback in this config — owner must resolve via
        // the lookup alone.
        let mut config = test_config();
        config.owner_user_id = None;
        let monitor = TelegramMonitor::new(config)
            .with_owner_lookup(std::sync::Arc::new(OwnerLookup::new(store)));

        let owner_ids = monitor.current_owner_telegram_ids().await;
        assert_eq!(owner_ids, vec!["777".to_string()]);

        let updates = vec![make_text_update(
            100,
            1,
            777,
            "Owner",
            Some("ownerdev"),
            777,
            "hi me",
        )];
        let events = monitor.process_updates_with_owner(updates, &owner_ids);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].source_risk, RiskLevel::Safe);
    }

    #[tokio::test]
    async fn process_updates_with_owner_lookup_negative_case() {
        use athen_contacts::{ContactStore, InMemoryContactStore};
        use athen_core::contact::{Contact, ContactIdentifier, IdentifierKind, TrustLevel};

        let store: std::sync::Arc<dyn ContactStore> =
            std::sync::Arc::new(InMemoryContactStore::new());
        let owner = Contact {
            id: Uuid::new_v4(),
            name: "Owner".into(),
            trust_level: TrustLevel::AuthUser,
            trust_manual_override: true,
            identifiers: vec![ContactIdentifier {
                kind: IdentifierKind::Telegram,
                value: "777".into(),
            }],
            interaction_count: 0,
            last_interaction: None,
            notes: None,
            blocked: false,
            is_owner: true,
        };
        let id = owner.id;
        store.save(&owner).await.unwrap();
        store.set_owner(&id).await.unwrap();

        let mut config = test_config();
        config.owner_user_id = None;
        let monitor = TelegramMonitor::new(config)
            .with_owner_lookup(std::sync::Arc::new(OwnerLookup::new(store)));

        let owner_ids = monitor.current_owner_telegram_ids().await;
        let updates = vec![make_text_update(100, 1, 999, "Stranger", None, 999, "spam")];
        let events = monitor.process_updates_with_owner(updates, &owner_ids);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].source_risk, RiskLevel::Caution);
    }

    #[tokio::test]
    async fn current_owner_telegram_ids_falls_back_to_legacy_config() {
        // No lookup wired; only the legacy `owner_user_id` provides the
        // owner. Confirms first-boot path still works before the
        // app-level migration runs.
        let mut config = test_config();
        config.owner_user_id = Some(123);
        let monitor = TelegramMonitor::new(config);
        let ids = monitor.current_owner_telegram_ids().await;
        assert_eq!(ids, vec!["123".to_string()]);
    }

    #[test]
    fn process_updates_non_owner_gets_l2_risk() {
        let monitor = TelegramMonitor::new(test_config());
        let updates = vec![make_text_update(
            100, 1, 999, "Stranger", None, 999, "Hi there",
        )];

        let events = monitor.process_updates(updates);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].source_risk, RiskLevel::Caution); // L2
    }

    #[test]
    fn process_updates_filters_by_allowed_chat_ids() {
        let mut config = test_config();
        config.allowed_chat_ids = vec![100, 200];
        let monitor = TelegramMonitor::new(config);

        let updates = vec![
            make_text_update(1, 1, 42, "Alex", None, 100, "Allowed chat"),
            make_text_update(2, 2, 42, "Alex", None, 300, "Blocked chat"),
            make_text_update(3, 3, 42, "Alex", None, 200, "Another allowed"),
        ];

        let events = monitor.process_updates(updates);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].content.body["chat_id"], 100);
        assert_eq!(events[1].content.body["chat_id"], 200);
    }

    #[test]
    fn process_updates_skips_updates_without_message() {
        let monitor = TelegramMonitor::new(test_config());
        let updates = vec![
            TelegramUpdate {
                update_id: 1,
                message: None,
                callback_query: None,
            },
            make_text_update(2, 10, 42, "Alex", None, 42, "Real message"),
        ];

        let events = monitor.process_updates(updates);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].content.summary.as_deref(), Some("Real message"));
    }

    #[test]
    fn process_updates_skips_empty_text_and_caption() {
        let monitor = TelegramMonitor::new(test_config());
        let update = TelegramUpdate {
            update_id: 1,
            message: Some(TelegramMessage {
                message_id: 1,
                from: Some(TelegramUser {
                    id: 42,
                    first_name: "Alex".to_string(),
                    username: None,
                }),
                chat: TelegramChat {
                    id: 42,
                    chat_type: "private".to_string(),
                },
                date: 1700000000,
                text: None,
                caption: None,
                photo: None,
                document: None,
                voice: None,
                audio: None,
                video: None,
            }),
            callback_query: None,
        };

        let events = monitor.process_updates(vec![update]);
        assert!(events.is_empty());
    }

    fn make_media_update(
        update_id: i64,
        message_id: i64,
        text: Option<&str>,
        caption: Option<&str>,
        photo: Option<Vec<TelegramPhotoSize>>,
        document: Option<TelegramDocument>,
        voice: Option<TelegramVoice>,
    ) -> TelegramUpdate {
        TelegramUpdate {
            update_id,
            message: Some(TelegramMessage {
                message_id,
                from: Some(TelegramUser {
                    id: 42,
                    first_name: "Alex".into(),
                    username: None,
                }),
                chat: TelegramChat {
                    id: 42,
                    chat_type: "private".into(),
                },
                date: 1700000000,
                text: text.map(|s| s.into()),
                caption: caption.map(|s| s.into()),
                photo,
                document,
                voice,
                audio: None,
                video: None,
            }),
            callback_query: None,
        }
    }

    #[test]
    fn pure_photo_message_emits_event_with_attachment() {
        let monitor = TelegramMonitor::new(test_config());
        let update = make_media_update(
            300,
            42,
            None,
            None,
            Some(vec![
                TelegramPhotoSize {
                    file_id: "low".into(),
                    file_size: Some(1_000),
                    width: 100,
                    height: 100,
                },
                TelegramPhotoSize {
                    file_id: "high".into(),
                    file_size: Some(50_000),
                    width: 1024,
                    height: 768,
                },
            ]),
            None,
            None,
        );
        let events = monitor.process_updates(vec![update]);
        assert_eq!(events.len(), 1, "media-only message should emit an event");
        assert_eq!(events[0].content.summary.as_deref(), Some("[photo]"));
        assert_eq!(events[0].content.attachments.len(), 1);
        let att = &events[0].content.attachments[0];
        assert_eq!(att.mime_type, "image/jpeg");
        match att.source.as_ref().unwrap() {
            AttachmentSource::Telegram {
                file_id,
                chat_id,
                message_id,
            } => {
                assert_eq!(file_id, "high"); // larger of the two
                assert_eq!(*chat_id, 42);
                assert_eq!(*message_id, 42);
            }
            _ => panic!("expected Telegram source"),
        }
    }

    #[test]
    fn document_with_no_caption_synthesises_summary() {
        let monitor = TelegramMonitor::new(test_config());
        let update = make_media_update(
            301,
            10,
            None,
            None,
            None,
            Some(TelegramDocument {
                file_id: "doc1".into(),
                file_name: Some("invoice.pdf".into()),
                mime_type: Some("application/pdf".into()),
                file_size: Some(123_456),
            }),
            None,
        );
        let events = monitor.process_updates(vec![update]);
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].content.summary.as_deref(),
            Some("[document: invoice.pdf]")
        );
        assert_eq!(events[0].content.attachments.len(), 1);
        assert_eq!(
            events[0].content.attachments[0].mime_type,
            "application/pdf"
        );
    }

    #[test]
    fn voice_note_synthesises_voice_summary() {
        let monitor = TelegramMonitor::new(test_config());
        let update = make_media_update(
            302,
            11,
            None,
            None,
            None,
            None,
            Some(TelegramVoice {
                file_id: "voice1".into(),
                duration: 5,
                mime_type: Some("audio/ogg".into()),
                file_size: Some(8_000),
            }),
        );
        let events = monitor.process_updates(vec![update]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].content.summary.as_deref(), Some("[voice note]"));
    }

    #[test]
    fn caption_with_photo_uses_caption_as_summary_and_keeps_photo_as_attachment() {
        let monitor = TelegramMonitor::new(test_config());
        let update = make_media_update(
            303,
            12,
            None,
            Some("Look at this!"),
            Some(vec![TelegramPhotoSize {
                file_id: "p".into(),
                file_size: Some(5_000),
                width: 800,
                height: 600,
            }]),
            None,
            None,
        );
        let events = monitor.process_updates(vec![update]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].content.summary.as_deref(), Some("Look at this!"));
        assert_eq!(events[0].content.attachments.len(), 1);
    }

    #[test]
    fn pick_best_photo_skips_oversize() {
        // Largest is over the 20 MiB cap → should fall back to the smaller.
        let big = TelegramPhotoSize {
            file_id: "big".into(),
            file_size: Some(TELEGRAM_MAX_DOWNLOAD_BYTES + 1),
            width: 4000,
            height: 3000,
        };
        let small = TelegramPhotoSize {
            file_id: "small".into(),
            file_size: Some(1_000_000),
            width: 800,
            height: 600,
        };
        let photos = [big, small];
        let picked = pick_best_photo(&photos).unwrap();
        assert_eq!(picked.file_id, "small");
    }

    #[test]
    fn process_updates_tracks_last_update_id() {
        let monitor = TelegramMonitor::new(test_config());
        assert!(monitor.last_update_id.lock().unwrap().is_none());

        let updates = vec![
            make_text_update(10, 1, 42, "A", None, 42, "msg1"),
            make_text_update(15, 2, 42, "A", None, 42, "msg2"),
            make_text_update(12, 3, 42, "A", None, 42, "msg3"),
        ];

        monitor.process_updates(updates);
        assert_eq!(*monitor.last_update_id.lock().unwrap(), Some(15));
    }

    #[test]
    fn process_updates_long_text_truncated_in_summary() {
        let monitor = TelegramMonitor::new(test_config());
        let long_text = "a".repeat(200);
        let updates = vec![make_text_update(1, 1, 42, "A", None, 42, &long_text)];

        let events = monitor.process_updates(updates);
        assert_eq!(events.len(), 1);
        let summary = events[0].content.summary.as_ref().unwrap();
        assert_eq!(summary.len(), 100); // 97 chars + "..."
        assert!(summary.ends_with("..."));
        // Full text is in body.
        assert_eq!(events[0].content.body["text"].as_str().unwrap().len(), 200);
    }

    // ---------------------------------------------------------------
    // SenseMonitor trait: poll returns empty when disabled
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn poll_returns_empty_when_disabled() {
        let mut config = test_config();
        config.enabled = false;
        let monitor = TelegramMonitor::new(config);
        let events = monitor.poll().await.unwrap();
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn poll_returns_empty_when_token_empty() {
        let mut config = test_config();
        config.bot_token = String::new();
        let monitor = TelegramMonitor::new(config);
        let events = monitor.poll().await.unwrap();
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn shutdown_succeeds() {
        let monitor = TelegramMonitor::new(test_config());
        monitor.shutdown().await.unwrap();
    }

    // ---------------------------------------------------------------
    // Config defaults
    // ---------------------------------------------------------------

    #[test]
    fn telegram_config_default_is_disabled() {
        let config = TelegramConfig::default();
        assert!(!config.enabled);
        assert!(config.bot_token.is_empty());
        assert!(config.owner_user_id.is_none());
        assert!(config.allowed_chat_ids.is_empty());
        assert_eq!(config.poll_interval_secs, 5);
    }

    #[test]
    fn telegram_config_deserializes_from_empty_toml() {
        let toml_str = "";
        let config: TelegramConfig = toml::from_str(toml_str).unwrap();
        assert!(!config.enabled);
        assert_eq!(config.poll_interval_secs, 5);
    }

    #[test]
    fn telegram_config_deserializes_partial() {
        let toml_str = r#"
            enabled = true
            bot_token = "123:ABC"
            owner_user_id = 42
        "#;
        let config: TelegramConfig = toml::from_str(toml_str).unwrap();
        assert!(config.enabled);
        assert_eq!(config.bot_token, "123:ABC");
        assert_eq!(config.owner_user_id, Some(42));
        assert!(config.allowed_chat_ids.is_empty());
        assert_eq!(config.poll_interval_secs, 5);
    }
}
