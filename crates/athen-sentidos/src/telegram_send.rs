//! Outbound Telegram adapter implementing [`TelegramSender`] over the
//! Bot HTTP API via reqwest.
//!
//! The inbound monitor (`telegram::TelegramMonitor`) already polls
//! `getUpdates` and ships free-standing helpers (`send_message`,
//! `edit_message_text`, …) for the approval router. This file adds the
//! agent-facing outbound surface — text + photo + document — behind a
//! single trait so the tool layer doesn't have to know about reqwest or
//! multipart upload mechanics.
//!
//! Endpoints used:
//! - `sendMessage` (JSON body, 4096-char text cap → chunked)
//! - `sendPhoto` (multipart, image/* re-compressed by Telegram)
//! - `sendDocument` (multipart, any file, preserves bytes + filename)
//!
//! Caption affinity: when the agent passes a single attachment and a
//! short text (≤ 1024 chars and no explicit caption), the adapter
//! attaches the text as the photo/document caption instead of sending a
//! separate bubble. Multiple attachments → text is sent as its own
//! bubble first, then each file follows.

use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::multipart;

use athen_core::error::{AthenError, Result};
use athen_core::traits::telegram_sender::{
    OutboundTelegramMessage, SentTelegramMessage, TelegramAttachment, TelegramAttachmentKind,
    TelegramSender,
};

/// Telegram caption cap, in unicode characters. Above this length the
/// adapter sends the text as a separate `sendMessage` bubble before the
/// attachments.
const TELEGRAM_CAPTION_LIMIT: usize = 1024;

/// HTTP timeout per Bot API call. Attachments can be slow; keep generous.
const TELEGRAM_HTTP_TIMEOUT: Duration = Duration::from_secs(60);

pub struct BotApiTelegramSender {
    bot_token: String,
    default_chat_id: Option<i64>,
    client: reqwest::Client,
}

impl BotApiTelegramSender {
    pub fn new(bot_token: impl Into<String>, default_chat_id: Option<i64>) -> Result<Self> {
        let bot_token = bot_token.into();
        if bot_token.trim().is_empty() {
            return Err(AthenError::Config(
                "Telegram bot_token is empty".to_string(),
            ));
        }
        let client = reqwest::Client::builder()
            .timeout(TELEGRAM_HTTP_TIMEOUT)
            .build()
            .map_err(|e| AthenError::Other(format!("Telegram HTTP client: {e}")))?;
        Ok(Self {
            bot_token,
            default_chat_id,
            client,
        })
    }

    fn endpoint(&self, method: &str) -> String {
        format!("https://api.telegram.org/bot{}/{}", self.bot_token, method)
    }

    /// Resolve `Auto` to a concrete kind based on file extension. Image
    /// extensions → `Photo`, anything else → `Document`.
    fn resolve_kind(kind: TelegramAttachmentKind, path: &Path) -> TelegramAttachmentKind {
        match kind {
            TelegramAttachmentKind::Photo | TelegramAttachmentKind::Document => kind,
            TelegramAttachmentKind::Auto => {
                let ext = path
                    .extension()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_ascii_lowercase();
                match ext.as_str() {
                    "png" | "jpg" | "jpeg" | "webp" | "gif" | "bmp" => {
                        TelegramAttachmentKind::Photo
                    }
                    _ => TelegramAttachmentKind::Document,
                }
            }
        }
    }

    async fn send_text_chunked(
        &self,
        chat_id: i64,
        text: &str,
        reply_to: Option<i64>,
    ) -> Result<Vec<i64>> {
        let mut ids = Vec::new();
        let chunks: Vec<&str> = if text.chars().count() <= 4096 {
            vec![text]
        } else {
            split_text(text, 4096)
        };
        let url = self.endpoint("sendMessage");
        for (i, chunk) in chunks.iter().enumerate() {
            if chunk.is_empty() {
                continue;
            }
            let mut body = serde_json::json!({
                "chat_id": chat_id,
                "text": chunk,
            });
            // Thread only the first chunk against the original message;
            // subsequent chunks chain naturally.
            if i == 0 {
                if let Some(rid) = reply_to {
                    body["reply_to_message_id"] = serde_json::Value::from(rid);
                }
            }
            let resp = self
                .client
                .post(&url)
                .json(&body)
                .send()
                .await
                .map_err(|e| AthenError::Other(format!("Telegram sendMessage: {e}")))?;
            let mid = parse_message_id(resp, "sendMessage").await?;
            ids.push(mid);
        }
        Ok(ids)
    }

    async fn send_attachment(
        &self,
        chat_id: i64,
        att: &TelegramAttachment,
        caption: Option<&str>,
        reply_to: Option<i64>,
    ) -> Result<i64> {
        let resolved = Self::resolve_kind(att.kind, &att.path);
        let (method, field) = match resolved {
            TelegramAttachmentKind::Photo => ("sendPhoto", "photo"),
            TelegramAttachmentKind::Document => ("sendDocument", "document"),
            // resolve_kind never returns Auto
            TelegramAttachmentKind::Auto => unreachable!(),
        };

        let bytes = tokio::fs::read(&att.path).await.map_err(|e| {
            AthenError::Other(format!(
                "Cannot read attachment '{}': {}",
                att.path.display(),
                e
            ))
        })?;
        let file_name = att
            .path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("file")
            .to_string();
        let mime = guess_mime(&att.path);

        let part = multipart::Part::bytes(bytes)
            .file_name(file_name)
            .mime_str(&mime)
            .map_err(|e| AthenError::Other(format!("Telegram multipart mime '{mime}': {e}")))?;

        let mut form = multipart::Form::new()
            .text("chat_id", chat_id.to_string())
            .part(field.to_string(), part);

        if let Some(c) = caption {
            if !c.is_empty() {
                form = form.text("caption", c.to_string());
            }
        }
        if let Some(rid) = reply_to {
            form = form.text("reply_to_message_id", rid.to_string());
        }

        let url = self.endpoint(method);
        let resp = self
            .client
            .post(&url)
            .multipart(form)
            .send()
            .await
            .map_err(|e| AthenError::Other(format!("Telegram {method}: {e}")))?;
        parse_message_id(resp, method).await
    }
}

#[async_trait]
impl TelegramSender for BotApiTelegramSender {
    async fn send(&self, msg: &OutboundTelegramMessage) -> Result<SentTelegramMessage> {
        let chat_id = msg.chat_id.or(self.default_chat_id).ok_or_else(|| {
            AthenError::Config(
                "Telegram destination chat_id missing and no owner default is configured"
                    .to_string(),
            )
        })?;

        let text = msg
            .text
            .as_deref()
            .map(|t| t.trim())
            .filter(|t| !t.is_empty());
        if text.is_none() && msg.attachments.is_empty() {
            return Err(AthenError::Config(
                "Telegram message has neither text nor attachments".to_string(),
            ));
        }

        let mut message_ids: Vec<i64> = Vec::new();

        // Single attachment + short text + no explicit caption →
        // attach text as the file's caption (one bubble instead of two).
        let single_with_short_text = match (&text, msg.attachments.as_slice()) {
            (Some(t), [only]) => {
                only.caption.is_none() && t.chars().count() <= TELEGRAM_CAPTION_LIMIT
            }
            _ => false,
        };

        if single_with_short_text {
            let only = &msg.attachments[0];
            let mid = self
                .send_attachment(chat_id, only, text, msg.reply_to_message_id)
                .await?;
            message_ids.push(mid);
            return Ok(SentTelegramMessage {
                message_ids,
                chat_id,
            });
        }

        // Otherwise: text first (if any), then each attachment with its
        // own caption (when provided).
        if let Some(t) = text {
            let ids = self
                .send_text_chunked(chat_id, t, msg.reply_to_message_id)
                .await?;
            message_ids.extend(ids);
        }

        for att in &msg.attachments {
            let mid = self
                .send_attachment(
                    chat_id,
                    att,
                    att.caption.as_deref(),
                    // Only thread the first message; attachments after
                    // the initial text are siblings in the same thread.
                    if message_ids.is_empty() {
                        msg.reply_to_message_id
                    } else {
                        None
                    },
                )
                .await?;
            message_ids.push(mid);
        }

        Ok(SentTelegramMessage {
            message_ids,
            chat_id,
        })
    }

    async fn test_connection(&self) -> Result<()> {
        let url = self.endpoint("getMe");
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| AthenError::Other(format!("Telegram getMe: {e}")))?;
        let status = resp.status();
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| AthenError::Other(format!("Telegram getMe parse: {e}")))?;
        if !status.is_success() || !body.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            return Err(AthenError::Other(format!(
                "Telegram getMe error {status}: {body}"
            )));
        }
        Ok(())
    }

    fn default_chat_id(&self) -> Option<i64> {
        self.default_chat_id
    }

    fn name(&self) -> &'static str {
        "telegram-bot-api"
    }
}

async fn parse_message_id(resp: reqwest::Response, method: &str) -> Result<i64> {
    let status = resp.status();
    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| AthenError::Other(format!("Telegram {method} parse: {e}")))?;
    if !status.is_success() || !body.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
        let desc = body
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        return Err(AthenError::Other(format!(
            "Telegram {method} error {status}: {desc}"
        )));
    }
    body.get("result")
        .and_then(|r| r.get("message_id"))
        .and_then(|v| v.as_i64())
        .ok_or_else(|| AthenError::Other(format!("Telegram {method}: response missing message_id")))
}

/// Split a long string into chunks of at most `max_chars` Unicode chars
/// each, preferring line breaks then word boundaries.
fn split_text(text: &str, max_chars: usize) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let bytes = text.as_bytes();
    while start < bytes.len() {
        // Walk forward up to max_chars chars.
        let mut end = start;
        let mut count = 0usize;
        while end < bytes.len() && count < max_chars {
            // Step one Unicode scalar — find next char boundary.
            let mut next = end + 1;
            while next < bytes.len() && (bytes[next] & 0b1100_0000) == 0b1000_0000 {
                next += 1;
            }
            end = next;
            count += 1;
        }
        if end >= bytes.len() {
            out.push(&text[start..]);
            break;
        }
        // Prefer a newline boundary; failing that, a space boundary.
        let slice = &text[start..end];
        let split_at = slice
            .rfind('\n')
            .or_else(|| slice.rfind(' '))
            .map(|i| start + i)
            .filter(|i| *i > start)
            .unwrap_or(end);
        out.push(&text[start..split_at]);
        start = split_at;
        // Eat a leading separator so the next chunk doesn't start with it.
        while start < bytes.len() && matches!(bytes[start], b'\n' | b' ') {
            start += 1;
        }
    }
    out
}

fn guess_mime(path: &Path) -> String {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "pdf" => "application/pdf",
        "json" => "application/json",
        "txt" | "log" => "text/plain",
        "csv" => "text/csv",
        "html" | "htm" => "text/html",
        "md" => "text/markdown",
        "mp3" => "audio/mpeg",
        "ogg" | "oga" | "opus" => "audio/ogg",
        "wav" => "audio/wav",
        "m4a" | "aac" => "audio/mp4",
        "flac" => "audio/flac",
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "zip" => "application/zip",
        "tar" => "application/x-tar",
        "gz" => "application/gzip",
        _ => "application/octet-stream",
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_kind_explicit_pass_through() {
        let p = Path::new("/tmp/x.pdf");
        assert_eq!(
            BotApiTelegramSender::resolve_kind(TelegramAttachmentKind::Photo, p),
            TelegramAttachmentKind::Photo
        );
        assert_eq!(
            BotApiTelegramSender::resolve_kind(TelegramAttachmentKind::Document, p),
            TelegramAttachmentKind::Document
        );
    }

    #[test]
    fn resolve_kind_auto_maps_by_extension() {
        for (name, want) in &[
            ("a.png", TelegramAttachmentKind::Photo),
            ("a.JPG", TelegramAttachmentKind::Photo),
            ("a.webp", TelegramAttachmentKind::Photo),
            ("a.gif", TelegramAttachmentKind::Photo),
            ("a.pdf", TelegramAttachmentKind::Document),
            ("a.txt", TelegramAttachmentKind::Document),
            ("a.zip", TelegramAttachmentKind::Document),
            ("no_ext", TelegramAttachmentKind::Document),
        ] {
            let got =
                BotApiTelegramSender::resolve_kind(TelegramAttachmentKind::Auto, Path::new(name));
            assert_eq!(got, *want, "auto mapping for {name}");
        }
    }

    #[test]
    fn split_text_under_limit_is_single_chunk() {
        let chunks = split_text("hello world", 4096);
        assert_eq!(chunks, vec!["hello world"]);
    }

    #[test]
    fn split_text_prefers_newlines() {
        let s = "aaaa\nbbbb\ncccc";
        let chunks = split_text(s, 5);
        assert!(chunks.iter().all(|c| c.chars().count() <= 5));
        assert_eq!(chunks.join("\n"), s);
    }

    #[test]
    fn split_text_falls_back_to_word_boundary() {
        let s = "aaa bbb ccc ddd eee";
        let chunks = split_text(s, 8);
        for c in &chunks {
            assert!(c.chars().count() <= 8, "chunk too long: {c:?}");
        }
        // Reassembly with single-space separators reproduces the input
        // (the splitter eats the boundary char).
        assert_eq!(chunks.join(" "), s);
    }

    #[test]
    fn split_text_handles_unicode_scalars() {
        // Each emoji is multi-byte; we measure in scalars not bytes.
        let s = "😀😀😀😀😀😀";
        let chunks = split_text(s, 2);
        assert!(chunks.iter().all(|c| c.chars().count() <= 2));
        assert_eq!(chunks.concat(), s);
    }

    #[test]
    fn guess_mime_common_kinds() {
        assert_eq!(guess_mime(Path::new("x.png")), "image/png");
        assert_eq!(guess_mime(Path::new("x.JPG")), "image/jpeg");
        assert_eq!(guess_mime(Path::new("x.pdf")), "application/pdf");
        assert_eq!(guess_mime(Path::new("x.zip")), "application/zip");
        assert_eq!(guess_mime(Path::new("noext")), "application/octet-stream");
    }

    #[test]
    fn new_rejects_empty_token() {
        let err = BotApiTelegramSender::new("", None);
        assert!(err.is_err());
    }
}
