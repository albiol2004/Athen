//! Approval routing across reply channels.
//!
//! Approvals (e.g. risk-system "do you really want to run this?" prompts)
//! need bidirectional delivery: send a question, wait for the answer.
//! This module implements three pieces:
//!
//! * [`InAppApprovalSink`] — emits a Tauri event with the question and
//!   parks a oneshot keyed by question id; the existing approve/deny UI
//!   resolves it via [`InAppApprovalSink::resolve`].
//! * [`TelegramApprovalSink`] — sends an inline keyboard via the Bot
//!   API and resolves when the corresponding `callback_query` arrives
//!   (forwarded from the Telegram poll loop via
//!   [`TelegramApprovalSink::resolve_callback`]).
//! * [`ApprovalRouter`] — picks a sink based on the arc's preferred
//!   reply channel + user presence + config, with a basic escalation
//!   ladder when the primary doesn't answer in time.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tokio::sync::{oneshot, Mutex};
use tracing::{info, warn};
use uuid::Uuid;

use athen_core::approval::{
    ApprovalAnswer, ApprovalChoice, ApprovalChoiceKind, ApprovalQuestion, ReplyChannelKind,
};
use athen_core::error::{AthenError, Result};
use athen_core::traits::approval::ApprovalSink;
use athen_persistence::arcs::{ArcSource, ArcStore};

/// Payload emitted to the frontend so the UI can render an approval
/// prompt. Mirrors [`ApprovalQuestion`] with field names the JS side
/// already knows how to render.
#[derive(Debug, Clone, Serialize)]
pub struct ApprovalQuestionEvent {
    pub id: String,
    pub prompt: String,
    pub description: Option<String>,
    pub choices: Vec<ApprovalChoiceView>,
    pub arc_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApprovalChoiceView {
    pub key: String,
    pub label: String,
    pub kind: String,
}

impl From<&ApprovalChoice> for ApprovalChoiceView {
    fn from(c: &ApprovalChoice) -> Self {
        Self {
            key: c.key.clone(),
            label: c.label.clone(),
            kind: match c.kind {
                ApprovalChoiceKind::Approve => "approve",
                ApprovalChoiceKind::Deny => "deny",
                ApprovalChoiceKind::AllowOnce => "allow_once",
                ApprovalChoiceKind::AllowAlways => "allow_always",
                ApprovalChoiceKind::Cancel => "cancel",
                ApprovalChoiceKind::Custom => "custom",
            }
            .to_string(),
        }
    }
}

/// Shared map: question_id → oneshot sender to deliver the answer.
type Pending = Arc<Mutex<HashMap<Uuid, oneshot::Sender<ApprovalAnswer>>>>;

// ---------------------------------------------------------------------------
// InApp sink
// ---------------------------------------------------------------------------

/// In-app approval sink: emits a Tauri event so the frontend renders a
/// prompt, and resolves the parked oneshot when the frontend calls
/// [`InAppApprovalSink::resolve`].
pub struct InAppApprovalSink {
    app_handle: Option<AppHandle>,
    pending: Pending,
}

impl InAppApprovalSink {
    pub fn new(app_handle: AppHandle) -> Self {
        Self {
            app_handle: Some(app_handle),
            pending: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Construct without an AppHandle, for unit tests that exercise the
    /// pending-map flow without going through Tauri.
    #[cfg(test)]
    pub fn new_for_test() -> Self {
        Self {
            app_handle: None,
            pending: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Resolve a pending question with the user's choice. Returns
    /// `false` if the question was unknown (already resolved or never
    /// asked here).
    pub async fn resolve(&self, answer: ApprovalAnswer) -> bool {
        let mut map = self.pending.lock().await;
        if let Some(tx) = map.remove(&answer.question_id) {
            let _ = tx.send(answer);
            true
        } else {
            false
        }
    }
}

#[async_trait]
impl ApprovalSink for InAppApprovalSink {
    fn channel_kind(&self) -> ReplyChannelKind {
        ReplyChannelKind::InApp
    }

    async fn ask(&self, question: ApprovalQuestion) -> Result<ApprovalAnswer> {
        let (tx, rx) = oneshot::channel();
        {
            let mut map = self.pending.lock().await;
            map.insert(question.id, tx);
        }

        if let Some(app) = &self.app_handle {
            let event = ApprovalQuestionEvent {
                id: question.id.to_string(),
                prompt: question.prompt.clone(),
                description: question.description.clone(),
                choices: question.choices.iter().map(Into::into).collect(),
                arc_id: question.arc_id.clone(),
            };
            if let Err(e) = app.emit("approval-question", event) {
                warn!("Failed to emit approval-question event: {e}");
            }
        }

        rx.await
            .map_err(|_| AthenError::Other("InApp approval cancelled".into()))
    }

    async fn cancel(&self, question_id: Uuid) -> Result<()> {
        let mut map = self.pending.lock().await;
        map.remove(&question_id);
        if let Some(app) = &self.app_handle {
            let _ = app.emit("approval-cancel", question_id.to_string());
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Telegram sink
// ---------------------------------------------------------------------------

/// Telegram approval sink: sends a message with an inline keyboard
/// (Approve / Deny) and resolves when the corresponding
/// `callback_query` arrives.
///
/// The host (the Telegram poll loop in `state.rs`) is responsible for
/// calling [`TelegramApprovalSink::resolve_callback`] every time the
/// monitor reports a callback event.
pub struct TelegramApprovalSink {
    bot_token: String,
    chat_id: i64,
    pending: Pending,
    /// Tracks `(message_id, choices)` for each pending question so the
    /// callback handler can edit the message after the user answers.
    posted: Arc<Mutex<HashMap<Uuid, PostedQuestion>>>,
}

#[derive(Debug, Clone)]
struct PostedQuestion {
    message_id: i64,
    chat_id: i64,
    choices: Vec<ApprovalChoice>,
}

impl TelegramApprovalSink {
    pub fn new(bot_token: String, chat_id: i64) -> Self {
        Self {
            bot_token,
            chat_id,
            pending: Arc::new(Mutex::new(HashMap::new())),
            posted: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Forward a callback_query event from the Telegram poll loop. If
    /// the `data` payload encodes a known question, deliver the answer
    /// and ack the callback. Returns `true` if the callback was for a
    /// question this sink was waiting on.
    ///
    /// **Always** acks the callback (`answerCallbackQuery`), even when
    /// the question is unknown — otherwise the user's button would
    /// stay in a loading state forever. The most common unknown-id case
    /// is "another channel won the race", which is healthy.
    pub async fn resolve_callback(
        &self,
        callback_id: &str,
        data: &str,
    ) -> bool {
        // Always ack the button so it stops spinning, no matter what
        // we decide below.
        ack_callback(self.bot_token.clone(), callback_id.to_string(), "");

        let (q_id, choice_key) = match parse_callback_data(data) {
            Some(v) => v,
            None => return false,
        };

        // Pull the parked oneshot.
        let sender = {
            let mut map = self.pending.lock().await;
            map.remove(&q_id)
        };
        let Some(sender) = sender else {
            // Already answered through another channel, or expired.
            // The cancel path edited the message; nothing more to do.
            return false;
        };

        let posted = {
            let mut map = self.posted.lock().await;
            map.remove(&q_id)
        };

        let _ = sender.send(ApprovalAnswer {
            question_id: q_id,
            choice_key: choice_key.clone(),
        });

        // Edit the message to show the user's choice instead of buttons.
        if let Some(p) = posted {
            let label = p
                .choices
                .iter()
                .find(|c| c.key == choice_key)
                .map(|c| c.label.clone())
                .unwrap_or_else(|| choice_key.clone());
            let token = self.bot_token.clone();
            let chat_id = p.chat_id;
            let msg_id = p.message_id;
            let confirmation = format!("{label} ✓");
            tokio::spawn(async move {
                if let Err(e) = athen_sentidos::telegram::edit_message_text(
                    &token, chat_id, msg_id, &confirmation,
                )
                .await
                {
                    warn!("Failed to edit Telegram approval message: {e}");
                }
            });
        }

        info!(
            question_id = %q_id,
            choice = %choice_key,
            "Telegram approval resolved"
        );
        true
    }
}

#[async_trait]
impl ApprovalSink for TelegramApprovalSink {
    fn channel_kind(&self) -> ReplyChannelKind {
        ReplyChannelKind::Telegram
    }

    async fn ask(&self, question: ApprovalQuestion) -> Result<ApprovalAnswer> {
        // Build the inline keyboard. Each button's callback_data is
        // `<question_id>|<choice_key>` so the callback handler can
        // dispatch back to the right pending question.
        let q_id = question.id;
        let body = match &question.description {
            Some(d) if !d.is_empty() => format!("{}\n\n{}", question.prompt, d),
            _ => question.prompt.clone(),
        };
        let datas: Vec<String> = question
            .choices
            .iter()
            .map(|c| format!("{}|{}", q_id, c.key))
            .collect();
        let buttons: Vec<(&str, &str)> = question
            .choices
            .iter()
            .zip(datas.iter())
            .map(|(c, d)| (c.label.as_str(), d.as_str()))
            .collect();

        // Park the oneshot before posting, so a fast callback can find it.
        let (tx, rx) = oneshot::channel();
        {
            let mut map = self.pending.lock().await;
            map.insert(q_id, tx);
        }

        let message_id = match athen_sentidos::telegram::send_message_with_keyboard(
            &self.bot_token,
            self.chat_id,
            &body,
            &buttons,
        )
        .await
        {
            Ok(id) => id,
            Err(e) => {
                // Drop the parked oneshot so we don't leak.
                let mut map = self.pending.lock().await;
                map.remove(&q_id);
                return Err(AthenError::Other(format!(
                    "Failed to send Telegram approval: {e}"
                )));
            }
        };

        {
            let mut posted = self.posted.lock().await;
            posted.insert(
                q_id,
                PostedQuestion {
                    message_id,
                    chat_id: self.chat_id,
                    choices: question.choices.clone(),
                },
            );
        }

        rx.await
            .map_err(|_| AthenError::Other("Telegram approval cancelled".into()))
    }

    async fn cancel(&self, question_id: Uuid) -> Result<()> {
        let mut map = self.pending.lock().await;
        map.remove(&question_id);
        let posted = {
            let mut p = self.posted.lock().await;
            p.remove(&question_id)
        };
        if let Some(p) = posted {
            let token = self.bot_token.clone();
            tokio::spawn(async move {
                if let Err(e) = athen_sentidos::telegram::edit_message_text(
                    &token,
                    p.chat_id,
                    p.message_id,
                    "(Approval handled elsewhere.)",
                )
                .await
                {
                    warn!("Failed to edit cancelled Telegram approval: {e}");
                }
            });
        }
        Ok(())
    }
}

/// Parse the `callback_data` we set on inline-keyboard buttons. Returns
/// `(question_id, choice_key)` on success.
/// Spawn a fire-and-forget `answerCallbackQuery` so the inline-keyboard
/// button stops showing the loading spinner. Safe to call even when we
/// don't know the question — Telegram silently no-ops on already-acked
/// callback ids.
fn ack_callback(token: String, callback_id: String, text: &str) {
    let text_owned = text.to_string();
    tokio::spawn(async move {
        if let Err(e) =
            athen_sentidos::telegram::answer_callback_query(&token, &callback_id, &text_owned).await
        {
            warn!("Failed to answer Telegram callback: {e}");
        }
    });
}

pub fn parse_callback_data(data: &str) -> Option<(Uuid, String)> {
    let (id_part, choice_part) = data.split_once('|')?;
    let q_id = Uuid::parse_str(id_part).ok()?;
    if choice_part.is_empty() {
        return None;
    }
    Some((q_id, choice_part.to_string()))
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

/// Routes an [`ApprovalQuestion`] to the right sink based on the arc's
/// preferred reply channel + user presence, with a simple escalation
/// ladder when the primary doesn't respond in time.
pub struct ApprovalRouter {
    sinks: Vec<Arc<dyn ApprovalSink>>,
    arc_store: Option<ArcStore>,
    /// How long to wait on the primary channel before also asking the
    /// escalation channel. The first answer wins; the other is cancelled.
    escalation_after: Duration,
}

impl ApprovalRouter {
    pub fn new(sinks: Vec<Arc<dyn ApprovalSink>>) -> Self {
        Self {
            sinks,
            arc_store: None,
            escalation_after: Duration::from_secs(120),
        }
    }

    pub fn with_arc_store(mut self, store: ArcStore) -> Self {
        self.arc_store = Some(store);
        self
    }

    pub fn with_escalation_after(mut self, d: Duration) -> Self {
        self.escalation_after = d;
        self
    }

    fn find(&self, kind: ReplyChannelKind) -> Option<Arc<dyn ApprovalSink>> {
        self.sinks
            .iter()
            .find(|s| s.channel_kind() == kind)
            .cloned()
    }

    /// Pick the channel to ask first, given the arc this question
    /// belongs to. Honours an explicit `primary_reply_channel` set on
    /// the arc; otherwise falls back to a default derived from the arc
    /// source (Messaging → Telegram, everything else → InApp).
    pub async fn pick_primary(&self, arc_id: Option<&str>) -> ReplyChannelKind {
        let Some(arc_id) = arc_id else {
            return ReplyChannelKind::InApp;
        };
        let Some(store) = &self.arc_store else {
            return ReplyChannelKind::InApp;
        };
        let Ok(Some(meta)) = store.get_arc(arc_id).await else {
            return ReplyChannelKind::InApp;
        };
        if let Some(s) = meta.primary_reply_channel.as_deref() {
            if let Some(kind) = ReplyChannelKind::from_str(s) {
                return kind;
            }
        }
        match meta.source {
            ArcSource::Messaging => ReplyChannelKind::Telegram,
            _ => ReplyChannelKind::InApp,
        }
    }

    /// Ask the question on the primary channel, escalating to the
    /// secondary after `escalation_after` if the primary hasn't
    /// answered. Returns the first answer that comes back; the other
    /// sink (if any) is cancelled so it cleans up its waiter.
    pub async fn ask_with_escalation(
        &self,
        question: ApprovalQuestion,
        primary: ReplyChannelKind,
    ) -> Result<ApprovalAnswer> {
        let primary_sink = self.find(primary).ok_or_else(|| {
            AthenError::Other(format!(
                "No approval sink available for primary channel {primary:?}"
            ))
        })?;
        let secondary_sink = self
            .sinks
            .iter()
            .find(|s| s.channel_kind() != primary)
            .cloned();

        let q_id = question.id;
        let escalation_after = self.escalation_after;

        let q1 = question.clone();
        let p_sink = primary_sink.clone();
        let mut primary_handle =
            tokio::spawn(async move { p_sink.ask(q1).await });

        let timer = tokio::time::sleep(escalation_after);
        tokio::pin!(timer);

        // Phase 1: wait for primary OR escalation timer.
        tokio::select! {
            biased;
            result = &mut primary_handle => {
                return result
                    .map_err(|e| AthenError::Other(format!("Approval primary task join: {e}")))
                    .and_then(|r| r);
            }
            _ = &mut timer => {
                // Fall through to escalation.
            }
        }

        // Phase 2: primary timed out for the initial wait.
        let Some(secondary) = secondary_sink else {
            // No escalation possible; just await primary indefinitely.
            return primary_handle
                .await
                .map_err(|e| AthenError::Other(format!("Approval primary task join: {e}")))
                .and_then(|r| r);
        };

        info!(
            question_id = %q_id,
            primary = ?primary,
            secondary = ?secondary.channel_kind(),
            "Approval primary timed out, escalating",
        );

        let q2 = question.clone();
        let s_sink = secondary.clone();
        let mut secondary_handle =
            tokio::spawn(async move { s_sink.ask(q2).await });

        // Race primary vs secondary. First to answer wins; cancel the
        // other so it cleans up its waiter.
        tokio::select! {
            result = &mut primary_handle => {
                let _ = secondary.cancel(q_id).await;
                secondary_handle.abort();
                result
                    .map_err(|e| AthenError::Other(format!("Approval primary task join: {e}")))
                    .and_then(|r| r)
            }
            result = &mut secondary_handle => {
                let _ = primary_sink.cancel(q_id).await;
                primary_handle.abort();
                result
                    .map_err(|e| AthenError::Other(format!("Approval secondary task join: {e}")))
                    .and_then(|r| r)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Stub sink that resolves with a configured answer after a delay.
    struct StubSink {
        kind: ReplyChannelKind,
        delay: Duration,
        answer_key: String,
    }

    #[async_trait]
    impl ApprovalSink for StubSink {
        fn channel_kind(&self) -> ReplyChannelKind {
            self.kind
        }
        async fn ask(&self, question: ApprovalQuestion) -> Result<ApprovalAnswer> {
            tokio::time::sleep(self.delay).await;
            Ok(ApprovalAnswer {
                question_id: question.id,
                choice_key: self.answer_key.clone(),
            })
        }
    }

    #[test]
    fn parse_callback_data_round_trips() {
        let q = Uuid::new_v4();
        let data = format!("{q}|approve");
        let (parsed_q, key) = parse_callback_data(&data).unwrap();
        assert_eq!(parsed_q, q);
        assert_eq!(key, "approve");
    }

    #[test]
    fn parse_callback_data_rejects_malformed() {
        assert!(parse_callback_data("nope").is_none());
        assert!(parse_callback_data("not-a-uuid|approve").is_none());
        let q = Uuid::new_v4();
        assert!(parse_callback_data(&format!("{q}|")).is_none());
    }

    #[tokio::test]
    async fn inapp_sink_resolves_when_frontend_replies() {
        let sink = Arc::new(InAppApprovalSink::new_for_test());
        let q = ApprovalQuestion::approve_or_deny("Send?");
        let q_id = q.id;

        let sink2 = sink.clone();
        let ask_handle = tokio::spawn(async move { sink2.ask(q).await });

        // Give the task a moment to register the oneshot.
        tokio::time::sleep(Duration::from_millis(20)).await;

        let resolved = sink
            .resolve(ApprovalAnswer {
                question_id: q_id,
                choice_key: "approve".into(),
            })
            .await;
        assert!(resolved);

        let answer = ask_handle.await.unwrap().unwrap();
        assert_eq!(answer.choice_key, "approve");
        assert_eq!(answer.question_id, q_id);
    }

    #[tokio::test]
    async fn telegram_resolve_callback_returns_false_for_unknown_question() {
        // Don't actually hit the Telegram API: ack_callback's spawned
        // task will fail to reach api.telegram.org and log a warning,
        // but that's fine — we only assert the resolve_callback return.
        let sink = TelegramApprovalSink::new("fake-token".to_string(), 0);
        let unknown_data = format!("{}|approve", Uuid::new_v4());
        let resolved = sink.resolve_callback("cb-fake", &unknown_data).await;
        assert!(!resolved);
    }

    #[tokio::test]
    async fn telegram_resolve_callback_returns_false_for_malformed_data() {
        let sink = TelegramApprovalSink::new("fake-token".to_string(), 0);
        let resolved = sink.resolve_callback("cb-fake", "not-a-valid-payload").await;
        assert!(!resolved);
    }

    #[tokio::test]
    async fn inapp_sink_resolve_returns_false_for_unknown_id() {
        let sink = InAppApprovalSink::new_for_test();
        let resolved = sink
            .resolve(ApprovalAnswer {
                question_id: Uuid::new_v4(),
                choice_key: "approve".into(),
            })
            .await;
        assert!(!resolved);
    }

    #[tokio::test]
    async fn inapp_sink_cancel_drops_pending_waiter() {
        let sink = Arc::new(InAppApprovalSink::new_for_test());
        let q = ApprovalQuestion::approve_or_deny("Send?");
        let q_id = q.id;

        let sink2 = sink.clone();
        let ask_handle = tokio::spawn(async move { sink2.ask(q).await });

        tokio::time::sleep(Duration::from_millis(20)).await;
        sink.cancel(q_id).await.unwrap();

        let result = ask_handle.await.unwrap();
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn router_uses_primary_when_it_answers_in_time() {
        let primary = Arc::new(StubSink {
            kind: ReplyChannelKind::InApp,
            delay: Duration::from_millis(20),
            answer_key: "approve".into(),
        }) as Arc<dyn ApprovalSink>;
        let secondary = Arc::new(StubSink {
            kind: ReplyChannelKind::Telegram,
            delay: Duration::from_millis(500),
            answer_key: "deny".into(),
        }) as Arc<dyn ApprovalSink>;

        let router = ApprovalRouter::new(vec![primary, secondary])
            .with_escalation_after(Duration::from_millis(200));

        let q = ApprovalQuestion::approve_or_deny("Test?");
        let answer = router
            .ask_with_escalation(q, ReplyChannelKind::InApp)
            .await
            .unwrap();
        assert_eq!(answer.choice_key, "approve");
    }

    #[tokio::test]
    async fn router_escalates_to_secondary_when_primary_too_slow() {
        let primary = Arc::new(StubSink {
            kind: ReplyChannelKind::InApp,
            delay: Duration::from_millis(2000),
            answer_key: "approve".into(),
        }) as Arc<dyn ApprovalSink>;
        let secondary = Arc::new(StubSink {
            kind: ReplyChannelKind::Telegram,
            delay: Duration::from_millis(50),
            answer_key: "deny".into(),
        }) as Arc<dyn ApprovalSink>;

        let router = ApprovalRouter::new(vec![primary, secondary])
            .with_escalation_after(Duration::from_millis(50));

        let q = ApprovalQuestion::approve_or_deny("Test?");
        let answer = router
            .ask_with_escalation(q, ReplyChannelKind::InApp)
            .await
            .unwrap();
        // Secondary answered first after escalation.
        assert_eq!(answer.choice_key, "deny");
    }

    #[tokio::test]
    async fn router_handles_missing_secondary_gracefully() {
        let primary = Arc::new(StubSink {
            kind: ReplyChannelKind::InApp,
            delay: Duration::from_millis(50),
            answer_key: "approve".into(),
        }) as Arc<dyn ApprovalSink>;

        let router = ApprovalRouter::new(vec![primary])
            .with_escalation_after(Duration::from_millis(10));

        let q = ApprovalQuestion::approve_or_deny("Test?");
        let answer = router
            .ask_with_escalation(q, ReplyChannelKind::InApp)
            .await
            .unwrap();
        assert_eq!(answer.choice_key, "approve");
    }

    #[tokio::test]
    async fn router_pick_primary_defaults_to_inapp_without_arc_store() {
        let sink = Arc::new(InAppApprovalSink::new_for_test()) as Arc<dyn ApprovalSink>;
        let router = ApprovalRouter::new(vec![sink]);
        assert_eq!(router.pick_primary(None).await, ReplyChannelKind::InApp);
        assert_eq!(
            router.pick_primary(Some("missing")).await,
            ReplyChannelKind::InApp
        );
    }
}
