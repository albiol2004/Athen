//! Live progress reporting for owner Telegram tasks.
//!
//! Without this, the bot is mute from the moment the user sends a
//! message until the final reply arrives, which on a multi-tool task
//! can be 30+ seconds of dead silence. This reporter:
//!
//! - Loops `sendChatAction=typing` every 4s so the client shows
//!   "Athen is typing…" continuously.
//! - Posts a single placeholder message at the start and edits it in
//!   place as new tools fire — one message per turn instead of N.
//! - On finalize, replaces the placeholder with the real response,
//!   splitting across additional messages when it exceeds Telegram's
//!   4096-char per-message ceiling.
//!
//! All Telegram API failures are logged and swallowed: progress is a
//! UX nicety, never the source of truth, so a network blip mid-turn
//! must not poison the actual reply.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio::time::interval;
use tokio_util::sync::CancellationToken;

const TYPING_INTERVAL: Duration = Duration::from_secs(4);
const TELEGRAM_MAX: usize = 4096;
const STATUS_HEADER: &str = "🤔 Working on it…";

/// Mirror of `frontend/app.js::BUILTIN_TOOL_LABELS`. Kept in sync by hand
/// because Telegram users see the same friendly labels the in-app UI does
/// — "Run" instead of `shell_execute`, "List" instead of `files__list_dir`.
/// Anything not in the table falls through to a humanized form of the raw
/// tool name in `pretty_tool_label`.
const BUILTIN_TOOL_LABELS: &[(&str, &str)] = &[
    ("read", "Read"),
    ("list_directory", "List"),
    ("grep", "Search files"),
    ("write", "Write"),
    ("edit", "Edit"),
    ("shell_execute", "Run"),
    ("shell_spawn", "Spawn"),
    ("shell_kill", "Stop"),
    ("shell_logs", "Logs"),
    ("web_search", "Search web"),
    ("web_fetch", "Fetch"),
    ("memory_store", "Save"),
    ("memory_recall", "Recall"),
    ("calendar_list", "Events"),
    ("calendar_create", "Create event"),
    ("calendar_update", "Update event"),
    ("calendar_delete", "Delete event"),
    ("contacts_list", "Contacts"),
    ("contacts_search", "Find contact"),
    ("contacts_create", "Add contact"),
    ("contacts_update", "Update contact"),
    ("contacts_delete", "Delete contact"),
    ("delete_path", "Delete"),
    ("append_file", "Append"),
    ("create_dir", "Create folder"),
    ("move_path", "Move"),
    ("exists", "Check"),
    ("stat", "Info"),
    ("delegate_to_agent", "Consult specialist"),
    ("install_package", "Install package"),
    ("uninstall_package", "Uninstall package"),
    ("list_installed_packages", "List packages"),
];

/// MCP suffixes that don't match a built-in tool name directly but should
/// alias to one — same set the frontend uses.
const MCP_SUFFIX_ALIASES: &[(&str, &str)] = &[
    ("read_file", "read"),
    ("write_file", "write"),
    ("list_dir", "list_directory"),
    ("list_files", "list_directory"),
    ("search_files", "grep"),
];

/// Convert a raw tool name (e.g. `files__list_dir`, `shell_execute`) into
/// the friendly label the user sees in the in-app UI. Falls back to a
/// humanized form of the raw name (`some_tool` → `Some tool`) when no
/// mapping exists, so unknown / new MCP tools still read decently.
pub(crate) fn pretty_tool_label(raw: &str) -> String {
    if let Some(label) = lookup_builtin(raw) {
        return label.to_string();
    }
    // MCP tools: `prefix__suffix`. Try the suffix as a built-in, then via
    // the alias table.
    if let Some(idx) = raw.find("__") {
        let suffix = &raw[idx + 2..];
        if let Some(label) = lookup_builtin(suffix) {
            return label.to_string();
        }
        if let Some(aliased) = MCP_SUFFIX_ALIASES
            .iter()
            .find_map(|(s, target)| (s == &suffix).then_some(*target))
        {
            if let Some(label) = lookup_builtin(aliased) {
                return label.to_string();
            }
        }
        // Unknown MCP tool — show the suffix humanized (the prefix is the
        // MCP server name and rarely meaningful to a Telegram user).
        return humanize(suffix);
    }
    humanize(raw)
}

fn lookup_builtin(name: &str) -> Option<&'static str> {
    BUILTIN_TOOL_LABELS
        .iter()
        .find_map(|(k, v)| (k == &name).then_some(*v))
}

/// `some_tool_name` → `Some tool name`. Underscores become spaces and the
/// first character is uppercased. Avoids touching anything else so tool
/// names with numbers or unusual casing aren't mangled.
fn humanize(raw: &str) -> String {
    if raw.is_empty() {
        return String::new();
    }
    let spaced = raw.replace('_', " ");
    let mut chars = spaced.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

pub(crate) struct TelegramProgressReporter {
    bot_token: String,
    chat_id: i64,
    state: Mutex<ReporterState>,
    typing_cancel: CancellationToken,
    typing_handle: Mutex<Option<JoinHandle<()>>>,
}

struct ReporterState {
    /// `message_id` of the live status message, or `None` if the
    /// initial post failed (we degrade to sending the final reply
    /// fresh in that case).
    message_id: Option<i64>,
    /// Ordered, deduplicated list of tools the auditor has reported.
    tools_seen: Vec<String>,
    /// Set once finalize runs; subsequent calls are no-ops so a late
    /// `report_tool` from a racing auditor can't overwrite the reply.
    finalized: bool,
}

impl TelegramProgressReporter {
    pub fn new(bot_token: String, chat_id: i64) -> Self {
        Self {
            bot_token,
            chat_id,
            state: Mutex::new(ReporterState {
                message_id: None,
                tools_seen: Vec::new(),
                finalized: false,
            }),
            typing_cancel: CancellationToken::new(),
            typing_handle: Mutex::new(None),
        }
    }

    /// Fire the first typing action, post the placeholder status
    /// message, and start the periodic typing loop. Best-effort: if
    /// the placeholder post fails we still start the typing loop so
    /// the user sees *something*, and finalize will fall back to a
    /// fresh send.
    pub async fn start(self: &Arc<Self>) {
        let _ = athen_sentidos::telegram::send_chat_action(
            &self.bot_token,
            self.chat_id,
            "typing",
        )
        .await;

        match athen_sentidos::telegram::send_message_returning_id(
            &self.bot_token,
            self.chat_id,
            STATUS_HEADER,
        )
        .await
        {
            Ok(mid) => {
                self.state.lock().await.message_id = Some(mid);
            }
            Err(e) => {
                tracing::warn!("Telegram progress: failed to post status message: {e}");
            }
        }

        // Periodic typing loop. The first chat action is already sent
        // above, so the interval's immediate first tick is wasted — we
        // consume and discard it to align cadence to the 4s delay.
        let me = Arc::clone(self);
        let token = me.typing_cancel.clone();
        let handle = tokio::spawn(async move {
            let mut tick = interval(TYPING_INTERVAL);
            tick.tick().await; // immediate-fire tick; we already typed once
            loop {
                tokio::select! {
                    _ = token.cancelled() => break,
                    _ = tick.tick() => {
                        let _ = athen_sentidos::telegram::send_chat_action(
                            &me.bot_token,
                            me.chat_id,
                            "typing",
                        ).await;
                    }
                }
            }
        });
        *self.typing_handle.lock().await = Some(handle);
    }

    /// Append `tool_name` to the running list (if new) and re-render
    /// the status message. The raw tool name (e.g. `files__list_dir`)
    /// is mapped to its UI label (`List`) before storage so the user
    /// sees the same friendly names they see in-app, and so two MCP
    /// tools that round-trip to the same UI label don't show up twice.
    pub async fn report_tool(&self, tool_name: &str) {
        let label = pretty_tool_label(tool_name);
        if label.is_empty() {
            return;
        }
        let (mid, render) = {
            let mut state = self.state.lock().await;
            if state.finalized {
                return;
            }
            if !state.tools_seen.iter().any(|n| n == &label) {
                state.tools_seen.push(label);
            }
            (state.message_id, Self::render_status(&state.tools_seen))
        };

        let Some(message_id) = mid else { return };
        if let Err(e) = athen_sentidos::telegram::edit_message_text(
            &self.bot_token,
            self.chat_id,
            message_id,
            &render,
        )
        .await
        {
            // Most common cause: Telegram returns 400 "message is not
            // modified" when we re-edit with identical content. Not an
            // error worth surfacing.
            tracing::debug!("Telegram progress edit failed: {e}");
        }
    }

    /// Replace the status message with the final response. If the
    /// reply exceeds Telegram's per-message ceiling, the first chunk
    /// edits the placeholder and subsequent chunks are sent as new
    /// messages. Idempotent — only the first call has effect.
    pub async fn finalize_with_text(&self, text: &str) {
        // Stop typing first so the loop can't race with the final edit.
        self.shutdown_typing().await;

        let (mid, already) = {
            let mut state = self.state.lock().await;
            let was_done = state.finalized;
            state.finalized = true;
            (state.message_id, was_done)
        };
        if already || text.is_empty() {
            return;
        }

        let chunks = chunk_text(text, TELEGRAM_MAX);
        let mut iter = chunks.into_iter();
        let first = iter.next().unwrap_or_default();

        match mid {
            Some(message_id) => {
                if let Err(e) = athen_sentidos::telegram::edit_message_text(
                    &self.bot_token,
                    self.chat_id,
                    message_id,
                    &first,
                )
                .await
                {
                    // Edit can fail (e.g. message older than 48h, or
                    // deleted). Fall back so the user still gets the
                    // reply even if the placeholder is unreachable.
                    tracing::warn!(
                        "Telegram progress: final edit failed, falling back to send: {e}"
                    );
                    let _ = athen_sentidos::telegram::send_message(
                        &self.bot_token,
                        self.chat_id,
                        &first,
                    )
                    .await;
                }
            }
            None => {
                let _ = athen_sentidos::telegram::send_message(
                    &self.bot_token,
                    self.chat_id,
                    &first,
                )
                .await;
            }
        }

        for chunk in iter {
            if chunk.is_empty() {
                continue;
            }
            let _ = athen_sentidos::telegram::send_message(
                &self.bot_token,
                self.chat_id,
                &chunk,
            )
            .await;
        }
    }

    fn render_status(tools: &[String]) -> String {
        if tools.is_empty() {
            STATUS_HEADER.to_string()
        } else {
            let body = tools
                .iter()
                .map(|t| format!("• {t}"))
                .collect::<Vec<_>>()
                .join("\n");
            format!("{STATUS_HEADER}\n\n{body}")
        }
    }

    async fn shutdown_typing(&self) {
        self.typing_cancel.cancel();
        if let Some(handle) = self.typing_handle.lock().await.take() {
            let _ = handle.await;
        }
    }
}

/// Split `text` into UTF-8-safe chunks no larger than `max` bytes.
/// Prefers to break on a newline, then on whitespace, within the
/// last 200 bytes of the budget so paragraphs and words aren't
/// bisected. Falls back to a hard char-boundary cut when no friendly
/// break point exists.
fn chunk_text(text: &str, max: usize) -> Vec<String> {
    if text.len() <= max {
        return vec![text.to_string()];
    }
    let mut out = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let budget_end = (start + max).min(text.len());
        let mut end = budget_end;
        while end > start && !text.is_char_boundary(end) {
            end -= 1;
        }
        if end < text.len() {
            let lookback_floor = end.saturating_sub(200).max(start);
            if let Some(idx) = text[start..end].rfind('\n') {
                let candidate = start + idx + 1;
                if candidate > lookback_floor {
                    end = candidate;
                }
            } else if let Some(idx) = text[start..end].rfind(char::is_whitespace) {
                let candidate = start + idx + 1;
                if candidate > lookback_floor {
                    end = candidate;
                }
            }
        }
        if end == start {
            // Shouldn't happen — `max` is well above any single char's
            // UTF-8 width — but guard against an infinite loop.
            break;
        }
        out.push(text[start..end].to_string());
        start = end;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_short_text_unchanged() {
        let chunks = chunk_text("hello world", 4096);
        assert_eq!(chunks, vec!["hello world".to_string()]);
    }

    #[test]
    fn chunk_breaks_on_newline_when_within_lookback() {
        // Build a string where a newline sits ~50 bytes before the budget
        // end, well within the 200-byte lookback window.
        let mut s = String::new();
        s.push_str(&"a".repeat(4040));
        s.push('\n');
        s.push_str(&"b".repeat(100));
        let chunks = chunk_text(&s, 4096);
        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].ends_with('\n'));
        assert!(chunks[1].starts_with('b'));
    }

    #[test]
    fn chunk_handles_multibyte_at_boundary() {
        // 'é' is 2 bytes; pad so the byte budget lands mid-character.
        let mut s = String::new();
        s.push_str(&"a".repeat(4095));
        s.push('é'); // bytes 4095..4097
        s.push_str(&"b".repeat(50));
        let chunks = chunk_text(&s, 4096);
        // First chunk must end on a char boundary, never mid-é.
        assert!(chunks.len() >= 2);
        assert!(chunks[0].is_char_boundary(chunks[0].len()));
    }

    #[test]
    fn render_status_with_no_tools_is_just_header() {
        let r = TelegramProgressReporter::render_status(&[]);
        assert_eq!(r, STATUS_HEADER);
    }

    #[test]
    fn pretty_label_built_in_tools() {
        assert_eq!(pretty_tool_label("shell_execute"), "Run");
        assert_eq!(pretty_tool_label("read"), "Read");
        assert_eq!(pretty_tool_label("list_directory"), "List");
        assert_eq!(pretty_tool_label("delegate_to_agent"), "Consult specialist");
    }

    #[test]
    fn pretty_label_mcp_tools_resolve_via_suffix() {
        // Direct suffix match.
        assert_eq!(pretty_tool_label("files__read"), "Read");
        // Alias map (the suffix isn't a built-in but maps to one).
        assert_eq!(pretty_tool_label("files__list_dir"), "List");
        assert_eq!(pretty_tool_label("files__read_file"), "Read");
        assert_eq!(pretty_tool_label("files__search_files"), "Search files");
    }

    #[test]
    fn pretty_label_unknown_mcp_tool_falls_back_to_humanized_suffix() {
        // No built-in or alias for `weird_tool` — show the suffix humanized,
        // dropping the MCP server prefix which the user doesn't care about.
        assert_eq!(pretty_tool_label("custom__weird_tool"), "Weird tool");
    }

    #[test]
    fn pretty_label_unknown_plain_tool_humanizes() {
        assert_eq!(pretty_tool_label("brand_new_tool"), "Brand new tool");
    }

    #[test]
    fn render_status_lists_tools_in_order() {
        let r = TelegramProgressReporter::render_status(&[
            "shell_execute".to_string(),
            "read".to_string(),
        ]);
        assert!(r.starts_with(STATUS_HEADER));
        assert!(r.contains("• shell_execute"));
        assert!(r.contains("• read"));
        let i_shell = r.find("shell_execute").unwrap();
        let i_read = r.find("read").unwrap();
        assert!(i_shell < i_read);
    }
}
