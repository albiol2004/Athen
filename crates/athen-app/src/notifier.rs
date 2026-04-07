//! Notification orchestrator and channel implementations.
//!
//! Delivers notifications to the user through the best available channel,
//! with quiet-hours support and escalation for high-urgency items.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chrono::{Local, NaiveTime};
use tauri::{AppHandle, Emitter};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use athen_core::config::{NotificationChannelKind, NotificationConfig};
use athen_core::error::Result;
use athen_core::llm::{ChatMessage, LlmRequest, MessageContent, ModelProfile, Role};
use athen_core::notification::{
    DeliveryResult, DeliveryStatus, Notification, NotificationOrigin, NotificationUrgency,
};
use athen_core::traits::llm::LlmRouter;
use athen_core::traits::notification::NotificationChannel;
use athen_persistence::notifications::NotificationStore;

/// A notification with its read status, serializable for the frontend.
#[derive(Debug, Clone, serde::Serialize)]
pub struct NotificationInfo {
    pub id: String,
    pub urgency: NotificationUrgency,
    pub title: String,
    pub body: String,
    pub origin: NotificationOrigin,
    pub arc_id: Option<String>,
    pub created_at: String,
    pub is_read: bool,
}

// ---------------------------------------------------------------------------
// InAppChannel
// ---------------------------------------------------------------------------

/// In-app notification channel -- emits Tauri events to the frontend.
pub struct InAppChannel {
    app_handle: AppHandle,
}

impl InAppChannel {
    pub fn new(app_handle: AppHandle) -> Self {
        Self { app_handle }
    }
}

#[async_trait::async_trait]
impl NotificationChannel for InAppChannel {
    fn channel_kind(&self) -> NotificationChannelKind {
        NotificationChannelKind::InApp
    }

    async fn send(&self, notification: &Notification) -> Result<DeliveryResult> {
        let payload = serde_json::json!({
            "id": notification.id.to_string(),
            "urgency": notification.urgency,
            "title": notification.title,
            "body": notification.body,
            "arc_id": notification.arc_id,
            "requires_response": notification.requires_response,
        });

        match self.app_handle.emit("notification", &payload) {
            Ok(_) => Ok(DeliveryResult::Delivered),
            Err(e) => Ok(DeliveryResult::Failed(format!("Failed to emit event: {e}"))),
        }
    }
}

// ---------------------------------------------------------------------------
// TelegramChannel
// ---------------------------------------------------------------------------

/// Telegram notification channel -- sends messages via Bot API.
pub struct TelegramChannel {
    bot_token: String,
    owner_chat_id: i64,
}

impl TelegramChannel {
    pub fn new(bot_token: String, owner_chat_id: i64) -> Self {
        Self {
            bot_token,
            owner_chat_id,
        }
    }
}

#[async_trait::async_trait]
impl NotificationChannel for TelegramChannel {
    fn channel_kind(&self) -> NotificationChannelKind {
        NotificationChannelKind::Telegram
    }

    async fn send(&self, notification: &Notification) -> Result<DeliveryResult> {
        let text = if notification.title.is_empty() {
            // Already humanized — send body as-is.
            notification.body.clone()
        } else {
            format!("{}\n\n{}", notification.title, notification.body)
        };

        match athen_sentidos::telegram::send_message(&self.bot_token, self.owner_chat_id, &text)
            .await
        {
            Ok(_) => Ok(DeliveryResult::Delivered),
            Err(e) => Ok(DeliveryResult::Failed(e)),
        }
    }
}

// ---------------------------------------------------------------------------
// NotificationOrchestrator
// ---------------------------------------------------------------------------

struct PendingNotification {
    notification: Notification,
    status: DeliveryStatus,
    channel_index: usize,
}

/// Orchestrates notification delivery across channels with escalation.
///
/// Channels are tried in order. If the user does not acknowledge a
/// high/critical notification within `escalation_timeout_secs`, the
/// orchestrator escalates to the next channel. Quiet-hours suppress
/// non-critical notifications until the window ends.
pub struct NotificationOrchestrator {
    channels: Vec<Box<dyn NotificationChannel>>,
    config: NotificationConfig,
    user_present: AtomicBool,
    pending: RwLock<HashMap<Uuid, PendingNotification>>,
    cancellation_tokens: RwLock<HashMap<Uuid, CancellationToken>>,
    llm_router: Option<Box<dyn LlmRouter>>,
    store: Option<NotificationStore>,
}

impl NotificationOrchestrator {
    pub fn new(
        config: NotificationConfig,
        channels: Vec<Box<dyn NotificationChannel>>,
    ) -> Self {
        Self {
            channels,
            config,
            user_present: AtomicBool::new(true), // assume present at startup
            pending: RwLock::new(HashMap::new()),
            cancellation_tokens: RwLock::new(HashMap::new()),
            llm_router: None,
            store: None,
        }
    }

    pub fn with_llm_router(mut self, router: Box<dyn LlmRouter>) -> Self {
        self.llm_router = Some(router);
        self
    }

    pub fn with_store(mut self, store: NotificationStore) -> Self {
        self.store = Some(store);
        self
    }

    pub fn set_user_present(&self, present: bool) {
        self.user_present.store(present, Ordering::Relaxed);
    }

    pub fn is_user_present(&self) -> bool {
        self.user_present.load(Ordering::Relaxed)
    }

    /// Main entry point: deliver a notification through the best available channel.
    ///
    /// If an LLM router is configured, the title and body are rephrased into
    /// natural, human-like language before delivery.
    pub async fn notify(self: &Arc<Self>, notification: Notification) {
        let notification = self.humanize(notification).await;

        // Check quiet hours -- critical notifications always go through.
        if self.is_quiet_hours() && notification.urgency != NotificationUrgency::Critical {
            tracing::info!(
                id = %notification.id,
                "Notification queued during quiet hours"
            );
            let mut pending = self.pending.write().await;
            pending.insert(
                notification.id,
                PendingNotification {
                    notification: notification.clone(),
                    status: DeliveryStatus::Pending,
                    channel_index: 0,
                },
            );

            // Persist queued notification.
            if let Some(ref store) = self.store {
                if let Err(e) = store.save(&notification, false).await {
                    tracing::warn!(error = %e, "Failed to persist queued notification");
                }
            }
            return;
        }

        let channel_index = self.select_first_channel();
        self.deliver(notification, channel_index).await;
    }

    /// Mark a notification as seen, cancelling any pending escalation.
    pub async fn mark_seen(&self, notification_id: Uuid) {
        // Cancel escalation if running.
        if let Some(token) = self
            .cancellation_tokens
            .write()
            .await
            .remove(&notification_id)
        {
            token.cancel();
        }

        // Update status.
        if let Some(pending) = self.pending.write().await.get_mut(&notification_id) {
            pending.status = DeliveryStatus::Seen;
        }

        // Persist read status.
        if let Some(ref store) = self.store {
            if let Err(e) = store.mark_read(notification_id).await {
                tracing::warn!(error = %e, "Failed to persist notification read status");
            }
        }
    }

    /// Return all notifications, newest first.
    ///
    /// Prefers the SQLite store as source of truth when available,
    /// falling back to the in-memory map otherwise.
    pub async fn list_notifications(&self) -> Vec<NotificationInfo> {
        // Prefer DB as source of truth.
        if let Some(ref store) = self.store {
            match store.list_all().await {
                Ok(notifications) => {
                    return notifications
                        .iter()
                        .map(|(n, is_read)| NotificationInfo {
                            id: n.id.to_string(),
                            urgency: n.urgency.clone(),
                            title: n.title.clone(),
                            body: n.body.clone(),
                            origin: n.origin.clone(),
                            arc_id: n.arc_id.clone(),
                            created_at: n.created_at.to_rfc3339(),
                            is_read: *is_read,
                        })
                        .collect();
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Failed to load notifications from DB, using in-memory");
                }
            }
        }

        // Fallback to in-memory.
        let pending = self.pending.read().await;
        let mut notifs: Vec<NotificationInfo> = pending
            .values()
            .map(|pn| NotificationInfo {
                id: pn.notification.id.to_string(),
                urgency: pn.notification.urgency.clone(),
                title: pn.notification.title.clone(),
                body: pn.notification.body.clone(),
                origin: pn.notification.origin.clone(),
                arc_id: pn.notification.arc_id.clone(),
                created_at: pn.notification.created_at.to_rfc3339(),
                is_read: matches!(pn.status, DeliveryStatus::Seen),
            })
            .collect();
        notifs.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        notifs
    }

    /// Mark a single notification as read.
    pub async fn mark_read(&self, notification_id: Uuid) {
        self.mark_seen(notification_id).await;
    }

    /// Mark all notifications as read.
    pub async fn mark_all_read(&self) {
        // Cancel all escalation tokens.
        let mut tokens = self.cancellation_tokens.write().await;
        for (_, token) in tokens.drain() {
            token.cancel();
        }

        // Mark all as Seen.
        let mut pending = self.pending.write().await;
        for pn in pending.values_mut() {
            pn.status = DeliveryStatus::Seen;
        }
        drop(pending);

        // Persist mark-all-read.
        if let Some(ref store) = self.store {
            if let Err(e) = store.mark_all_read().await {
                tracing::warn!(error = %e, "Failed to persist mark-all-read");
            }
        }
    }

    /// Mark all notifications related to a specific arc as read.
    ///
    /// Called when the user switches to an arc or actively engages with it
    /// (e.g. via Telegram owner message).
    pub async fn mark_arc_read(&self, arc_id: &str) {
        let ids_to_mark: Vec<Uuid> = {
            let pending = self.pending.read().await;
            pending
                .values()
                .filter(|pn| pn.notification.arc_id.as_deref() == Some(arc_id))
                .filter(|pn| !matches!(pn.status, DeliveryStatus::Seen))
                .map(|pn| pn.notification.id)
                .collect()
        };

        for id in ids_to_mark {
            self.mark_seen(id).await;
        }
    }

    /// Count of unread notifications.
    pub async fn unread_count(&self) -> usize {
        if let Some(ref store) = self.store {
            match store.unread_count().await {
                Ok(count) => return count,
                Err(e) => tracing::warn!(error = %e, "Failed to get unread count from DB"),
            }
        }

        // Fallback to in-memory.
        let pending = self.pending.read().await;
        pending
            .values()
            .filter(|pn| !matches!(pn.status, DeliveryStatus::Seen))
            .count()
    }

    /// Flush any notifications queued during quiet hours.
    /// Call this periodically or when quiet hours end.
    pub async fn flush_pending(self: &Arc<Self>) {
        if self.is_quiet_hours() {
            return;
        }

        let to_flush: Vec<(Uuid, Notification)> = {
            let mut pending = self.pending.write().await;
            let mut flush = Vec::new();
            let mut to_remove = Vec::new();
            for (id, pn) in pending.iter() {
                if matches!(pn.status, DeliveryStatus::Pending) {
                    flush.push((*id, pn.notification.clone()));
                    to_remove.push(*id);
                }
            }
            for id in to_remove {
                pending.remove(&id);
            }
            flush
        };

        for (_, notification) in to_flush {
            let channel_index = self.select_first_channel();
            self.deliver(notification, channel_index).await;
        }
    }

    /// Load persisted notifications from SQLite into the in-memory map.
    ///
    /// Call once at startup, after attaching the store.
    pub async fn load_persisted(&self) {
        let store = match &self.store {
            Some(s) => s,
            None => return,
        };

        match store.list_all().await {
            Ok(notifications) => {
                let mut pending = self.pending.write().await;
                for (notif, is_read) in notifications {
                    let status = if is_read {
                        DeliveryStatus::Seen
                    } else {
                        DeliveryStatus::Pending
                    };
                    pending.insert(
                        notif.id,
                        PendingNotification {
                            notification: notif,
                            status,
                            channel_index: 0,
                        },
                    );
                }
                tracing::info!(count = pending.len(), "Loaded persisted notifications");
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to load persisted notifications");
            }
        }
    }

    /// Delete a single notification.
    pub async fn delete_notification(&self, notification_id: Uuid) {
        // Remove from in-memory.
        self.pending.write().await.remove(&notification_id);

        // Cancel escalation if any.
        if let Some(token) = self
            .cancellation_tokens
            .write()
            .await
            .remove(&notification_id)
        {
            token.cancel();
        }

        // Remove from DB.
        if let Some(ref store) = self.store {
            if let Err(e) = store.delete(notification_id).await {
                tracing::warn!(error = %e, "Failed to delete notification from DB");
            }
        }
    }

    /// Delete all read notifications. Returns count deleted.
    pub async fn delete_read_notifications(&self) -> usize {
        // Remove read from in-memory.
        let mut pending = self.pending.write().await;
        let before = pending.len();
        pending.retain(|_, pn| !matches!(pn.status, DeliveryStatus::Seen));
        let in_memory_deleted = before - pending.len();
        drop(pending);

        // Remove from DB.
        if let Some(ref store) = self.store {
            match store.delete_read().await {
                Ok(count) => return count as usize,
                Err(e) => tracing::warn!(error = %e, "Failed to delete read notifications from DB"),
            }
        }

        in_memory_deleted
    }

    // --- Private helpers ---

    /// Rephrase a notification's title and body into natural, human-like
    /// language using a fast LLM call.  Falls back to the original text on
    /// failure or when no router is configured.
    async fn humanize(&self, mut notification: Notification) -> Notification {
        let router = match &self.llm_router {
            Some(r) => r,
            None => return notification,
        };

        let prompt = format!(
            "You are a personal assistant notifying your user about something.\n\
             Rewrite the notification below into a short, friendly, natural message \
             as if you were a human assistant talking to them casually.\n\
             - Be concise (1-2 sentences max).\n\
             - Don't use emojis.\n\
             - Keep all important details (names, times, numbers).\n\
             - Don't add information that isn't there.\n\
             - Answer with ONLY the rewritten message, nothing else.\n\n\
             Title: {}\n\
             Body: {}",
            notification.title, notification.body
        );

        let request = LlmRequest {
            profile: ModelProfile::Cheap,
            messages: vec![ChatMessage {
                role: Role::User,
                content: MessageContent::Text(prompt),
            }],
            max_tokens: Some(100),
            temperature: Some(0.7),
            tools: None,
            system_prompt: None,
        };

        match tokio::time::timeout(Duration::from_secs(5), router.route(&request)).await {
            Ok(Ok(response)) => {
                let text = response.content.trim().to_string();
                if !text.is_empty() {
                    notification.title = String::new();
                    notification.body = text;
                }
            }
            Ok(Err(e)) => {
                tracing::debug!(error = %e, "LLM humanization failed, using original text");
            }
            Err(_) => {
                tracing::debug!("LLM humanization timed out, using original text");
            }
        }

        notification
    }

    fn is_quiet_hours(&self) -> bool {
        let qh = match &self.config.quiet_hours {
            Some(qh) => qh,
            None => return false,
        };

        let now = Local::now().time();
        let start = NaiveTime::from_hms_opt(qh.start_hour, qh.start_minute, 0)
            .unwrap_or_else(|| NaiveTime::from_hms_opt(22, 0, 0).unwrap());
        let end = NaiveTime::from_hms_opt(qh.end_hour, qh.end_minute, 0)
            .unwrap_or_else(|| NaiveTime::from_hms_opt(8, 0, 0).unwrap());

        if start <= end {
            // Same-day range: e.g. 09:00 - 17:00
            now >= start && now < end
        } else {
            // Overnight range: e.g. 22:00 - 08:00
            now >= start || now < end
        }
    }

    /// Select the first channel index, preferring InApp if user is present.
    fn select_first_channel(&self) -> usize {
        if self.is_user_present() {
            for (i, ch) in self.channels.iter().enumerate() {
                if ch.channel_kind() == NotificationChannelKind::InApp {
                    return i;
                }
            }
        } else {
            // User not present -- skip InApp, find first external channel.
            for (i, ch) in self.channels.iter().enumerate() {
                if ch.channel_kind() != NotificationChannelKind::InApp {
                    return i;
                }
            }
        }
        0
    }

    fn deliver(
        self: &Arc<Self>,
        notification: Notification,
        channel_index: usize,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        Box::pin(self.deliver_inner(notification, channel_index))
    }

    async fn deliver_inner(self: &Arc<Self>, notification: Notification, channel_index: usize) {
        let notif_id = notification.id;
        let requires_response = notification.requires_response;
        let urgency = notification.urgency.clone();

        if channel_index >= self.channels.len() {
            tracing::warn!(id = %notif_id, "All notification channels exhausted");
            let mut pending = self.pending.write().await;
            pending
                .entry(notif_id)
                .and_modify(|pn| pn.status = DeliveryStatus::Expired)
                .or_insert(PendingNotification {
                    notification: notification.clone(),
                    status: DeliveryStatus::Expired,
                    channel_index: 0,
                });

            // Persist expired notification.
            if let Some(ref store) = self.store {
                if let Err(e) = store.save(&notification, false).await {
                    tracing::warn!(error = %e, "Failed to persist expired notification");
                }
            }
            return;
        }

        let channel = &self.channels[channel_index];
        let kind = channel.channel_kind();

        match channel.send(&notification).await {
            Ok(DeliveryResult::Delivered) => {
                tracing::info!(id = %notif_id, channel = ?kind, "Notification delivered");

                let mut pending = self.pending.write().await;
                pending.insert(
                    notif_id,
                    PendingNotification {
                        notification: notification.clone(),
                        status: DeliveryStatus::Delivered(kind.clone()),
                        channel_index,
                    },
                );

                // Persist to SQLite.
                if let Some(ref store) = self.store {
                    if let Err(e) = store.save(&notification, false).await {
                        tracing::warn!(error = %e, "Failed to persist notification");
                    }
                }

                // Spawn escalation for high/critical notifications that need a response.
                if requires_response
                    && matches!(
                        urgency,
                        NotificationUrgency::High | NotificationUrgency::Critical
                    )
                {
                    drop(pending); // release lock before spawning
                    self.spawn_escalation(notif_id, channel_index).await;
                }
            }
            Ok(DeliveryResult::Failed(reason)) => {
                tracing::warn!(
                    id = %notif_id,
                    channel = ?kind,
                    reason,
                    "Notification delivery failed, trying next"
                );
                self.deliver(notification, channel_index + 1).await;
            }
            Err(e) => {
                tracing::error!(
                    id = %notif_id,
                    channel = ?kind,
                    error = %e,
                    "Notification channel error"
                );
                self.deliver(notification, channel_index + 1).await;
            }
        }
    }

    async fn spawn_escalation(self: &Arc<Self>, notif_id: Uuid, current_channel_index: usize) {
        let token = CancellationToken::new();
        self.cancellation_tokens
            .write()
            .await
            .insert(notif_id, token.clone());

        let this = Arc::clone(self);
        let timeout = Duration::from_secs(this.config.escalation_timeout_secs);

        tokio::spawn(async move {
            let mut channel_idx = current_channel_index;

            loop {
                tokio::select! {
                    _ = tokio::time::sleep(timeout) => {
                        channel_idx += 1;

                        if channel_idx >= this.channels.len() {
                            tracing::warn!(id = %notif_id, "Escalation exhausted all channels");
                            let mut pending = this.pending.write().await;
                            if let Some(pn) = pending.get_mut(&notif_id) {
                                pn.status = DeliveryStatus::Expired;
                            }
                            break;
                        }

                        // Check if already seen.
                        {
                            let pending = this.pending.read().await;
                            if let Some(pn) = pending.get(&notif_id) {
                                if matches!(pn.status, DeliveryStatus::Seen) {
                                    break;
                                }
                            }
                        }

                        let channel = &this.channels[channel_idx];
                        let kind = channel.channel_kind();

                        // Retrieve the notification from pending.
                        let notification = {
                            let pending = this.pending.read().await;
                            match pending.get(&notif_id) {
                                Some(pn) => pn.notification.clone(),
                                None => break,
                            }
                        };

                        tracing::info!(id = %notif_id, channel = ?kind, "Escalating notification");

                        match channel.send(&notification).await {
                            Ok(DeliveryResult::Delivered) => {
                                let mut pending = this.pending.write().await;
                                if let Some(pn) = pending.get_mut(&notif_id) {
                                    pn.status = DeliveryStatus::Escalated(kind);
                                    pn.channel_index = channel_idx;
                                }
                            }
                            _ => {
                                // Failed -- next iteration will try the next channel.
                                continue;
                            }
                        }
                    }
                    _ = token.cancelled() => {
                        tracing::debug!(id = %notif_id, "Escalation cancelled (notification seen)");
                        break;
                    }
                }
            }

            // Cleanup the cancellation token.
            this.cancellation_tokens.write().await.remove(&notif_id);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    use athen_core::config::QuietHours;
    use athen_core::notification::NotificationOrigin;
    use chrono::Utc;

    /// Mock channel that records deliveries for testing.
    struct MockChannel {
        kind: NotificationChannelKind,
        send_count: Arc<AtomicUsize>,
        should_fail: AtomicBool,
    }

    impl MockChannel {
        fn new(kind: NotificationChannelKind) -> Self {
            Self {
                kind,
                send_count: Arc::new(AtomicUsize::new(0)),
                should_fail: AtomicBool::new(false),
            }
        }

        fn send_count(&self) -> usize {
            self.send_count.load(Ordering::Relaxed)
        }

        fn set_should_fail(&self, fail: bool) {
            self.should_fail.store(fail, Ordering::Relaxed);
        }

        fn counter(&self) -> Arc<AtomicUsize> {
            self.send_count.clone()
        }
    }

    #[async_trait::async_trait]
    impl NotificationChannel for MockChannel {
        fn channel_kind(&self) -> NotificationChannelKind {
            self.kind.clone()
        }

        async fn send(&self, _notification: &Notification) -> Result<DeliveryResult> {
            if self.should_fail.load(Ordering::Relaxed) {
                Ok(DeliveryResult::Failed("mock failure".to_string()))
            } else {
                self.send_count.fetch_add(1, Ordering::Relaxed);
                Ok(DeliveryResult::Delivered)
            }
        }
    }

    fn make_notification(urgency: NotificationUrgency, requires_response: bool) -> Notification {
        Notification {
            id: Uuid::new_v4(),
            urgency,
            title: "Test".to_string(),
            body: "Test body".to_string(),
            origin: NotificationOrigin::System,
            arc_id: None,
            task_id: None,
            created_at: Utc::now(),
            requires_response,
        }
    }

    fn make_config() -> NotificationConfig {
        NotificationConfig {
            preferred_channels: vec![
                NotificationChannelKind::InApp,
                NotificationChannelKind::Telegram,
            ],
            escalation_timeout_secs: 300,
            quiet_hours: None,
        }
    }

    /// Build an orchestrator with two mock channels (InApp + Telegram).
    /// Returns (orchestrator, inapp_counter, telegram_counter).
    fn make_orchestrator(
        config: NotificationConfig,
    ) -> (
        Arc<NotificationOrchestrator>,
        Arc<AtomicUsize>,
        Arc<AtomicUsize>,
    ) {
        let inapp = MockChannel::new(NotificationChannelKind::InApp);
        let telegram = MockChannel::new(NotificationChannelKind::Telegram);
        let inapp_counter = inapp.counter();
        let telegram_counter = telegram.counter();

        let orch = Arc::new(NotificationOrchestrator::new(
            config,
            vec![Box::new(inapp), Box::new(telegram)],
        ));

        (orch, inapp_counter, telegram_counter)
    }

    /// Build an orchestrator where both channels can be set to fail.
    /// Returns (orchestrator, inapp_counter, telegram_counter).
    /// Channels are accessible via the orchestrator's channels field (private),
    /// so we set failure state before construction.
    fn make_orchestrator_with_failing_inapp(
        config: NotificationConfig,
    ) -> (
        Arc<NotificationOrchestrator>,
        Arc<AtomicUsize>,
        Arc<AtomicUsize>,
    ) {
        let inapp = MockChannel::new(NotificationChannelKind::InApp);
        inapp.set_should_fail(true);
        let telegram = MockChannel::new(NotificationChannelKind::Telegram);
        let inapp_counter = inapp.counter();
        let telegram_counter = telegram.counter();

        let orch = Arc::new(NotificationOrchestrator::new(
            config,
            vec![Box::new(inapp), Box::new(telegram)],
        ));

        (orch, inapp_counter, telegram_counter)
    }

    #[tokio::test]
    async fn test_user_present_selects_inapp() {
        let (orch, inapp_counter, telegram_counter) = make_orchestrator(make_config());
        orch.set_user_present(true);

        let notif = make_notification(NotificationUrgency::Medium, false);
        orch.notify(notif).await;

        assert_eq!(inapp_counter.load(Ordering::Relaxed), 1);
        assert_eq!(telegram_counter.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn test_user_away_selects_telegram() {
        let (orch, inapp_counter, telegram_counter) = make_orchestrator(make_config());
        orch.set_user_present(false);

        let notif = make_notification(NotificationUrgency::Medium, false);
        orch.notify(notif).await;

        assert_eq!(inapp_counter.load(Ordering::Relaxed), 0);
        assert_eq!(telegram_counter.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn test_fallback_on_channel_failure() {
        let (orch, _inapp_counter, telegram_counter) =
            make_orchestrator_with_failing_inapp(make_config());
        orch.set_user_present(true);

        let notif = make_notification(NotificationUrgency::Medium, false);
        orch.notify(notif).await;

        assert_eq!(telegram_counter.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn test_all_channels_exhausted() {
        let inapp = MockChannel::new(NotificationChannelKind::InApp);
        let telegram = MockChannel::new(NotificationChannelKind::Telegram);
        inapp.set_should_fail(true);
        telegram.set_should_fail(true);
        let inapp_counter = inapp.counter();
        let telegram_counter = telegram.counter();

        let orch = Arc::new(NotificationOrchestrator::new(
            make_config(),
            vec![Box::new(inapp), Box::new(telegram)],
        ));
        orch.set_user_present(true);

        let notif = make_notification(NotificationUrgency::Medium, false);
        orch.notify(notif).await;

        // Neither channel delivered successfully.
        assert_eq!(inapp_counter.load(Ordering::Relaxed), 0);
        assert_eq!(telegram_counter.load(Ordering::Relaxed), 0);

        // Notification is tracked as Expired.
        let pending = orch.pending.read().await;
        assert_eq!(pending.len(), 1);
        let pn = pending.values().next().unwrap();
        assert!(matches!(pn.status, DeliveryStatus::Expired));
    }

    #[tokio::test]
    async fn test_mark_seen_cancels_escalation() {
        let config = NotificationConfig {
            escalation_timeout_secs: 1,
            ..make_config()
        };
        let (orch, _inapp_counter, telegram_counter) = make_orchestrator(config);
        orch.set_user_present(true);

        let notif = make_notification(NotificationUrgency::High, true);
        let notif_id = notif.id;
        orch.notify(notif).await;

        // Mark seen immediately, before escalation fires.
        orch.mark_seen(notif_id).await;

        // Wait longer than escalation timeout.
        tokio::time::sleep(Duration::from_secs(2)).await;

        // Telegram should NOT have received the escalation.
        assert_eq!(telegram_counter.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn test_escalation_tries_next_channel() {
        let config = NotificationConfig {
            escalation_timeout_secs: 1,
            ..make_config()
        };
        let (orch, inapp_counter, telegram_counter) = make_orchestrator(config);
        orch.set_user_present(true);

        let notif = make_notification(NotificationUrgency::High, true);
        orch.notify(notif).await;

        // InApp should have been called first.
        assert_eq!(inapp_counter.load(Ordering::Relaxed), 1);

        // Wait for escalation to fire.
        tokio::time::sleep(Duration::from_millis(1500)).await;

        // Telegram should have received the escalation.
        assert_eq!(telegram_counter.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn test_quiet_hours_queues_notification() {
        let config = NotificationConfig {
            quiet_hours: Some(QuietHours {
                start_hour: 0,
                start_minute: 0,
                end_hour: 23,
                end_minute: 59,
                allow_critical: true,
            }),
            ..make_config()
        };
        let (orch, inapp_counter, telegram_counter) = make_orchestrator(config);
        orch.set_user_present(true);

        let notif = make_notification(NotificationUrgency::Medium, false);
        let notif_id = notif.id;
        orch.notify(notif).await;

        // Neither channel should have been called (queued during quiet hours).
        assert_eq!(inapp_counter.load(Ordering::Relaxed), 0);
        assert_eq!(telegram_counter.load(Ordering::Relaxed), 0);

        // Should be in pending with Pending status.
        let pending = orch.pending.read().await;
        let pn = pending.get(&notif_id).expect("should be in pending");
        assert!(matches!(pn.status, DeliveryStatus::Pending));
    }

    #[tokio::test]
    async fn test_quiet_hours_allows_critical() {
        let config = NotificationConfig {
            quiet_hours: Some(QuietHours {
                start_hour: 0,
                start_minute: 0,
                end_hour: 23,
                end_minute: 59,
                allow_critical: true,
            }),
            ..make_config()
        };
        let (orch, inapp_counter, _telegram_counter) = make_orchestrator(config);
        orch.set_user_present(true);

        // Critical notifications bypass quiet hours.
        let notif = make_notification(NotificationUrgency::Critical, false);
        orch.notify(notif).await;

        assert_eq!(inapp_counter.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn test_flush_pending_delivers_queued() {
        // Create orchestrator with NO quiet hours so flush_pending will deliver.
        let config = make_config();
        let (orch, inapp_counter, _telegram_counter) = make_orchestrator(config);
        orch.set_user_present(true);

        // Manually insert a pending notification (simulating one queued during quiet hours).
        let notif = make_notification(NotificationUrgency::Medium, false);
        let notif_id = notif.id;
        {
            let mut pending = orch.pending.write().await;
            pending.insert(
                notif_id,
                PendingNotification {
                    notification: notif,
                    status: DeliveryStatus::Pending,
                    channel_index: 0,
                },
            );
        }

        orch.flush_pending().await;

        // The notification should have been delivered.
        assert_eq!(inapp_counter.load(Ordering::Relaxed), 1);

        // It should no longer be Pending (removed from pending map by flush_pending,
        // then re-inserted by deliver_inner with Delivered status).
        let pending = orch.pending.read().await;
        let pn = pending.get(&notif_id).expect("should still be tracked");
        assert!(matches!(pn.status, DeliveryStatus::Delivered(_)));
    }

    #[tokio::test]
    async fn test_select_channel_with_only_telegram() {
        let config = NotificationConfig {
            preferred_channels: vec![NotificationChannelKind::Telegram],
            ..make_config()
        };

        let telegram = MockChannel::new(NotificationChannelKind::Telegram);
        let telegram_counter = telegram.counter();

        let orch = Arc::new(NotificationOrchestrator::new(
            config,
            vec![Box::new(telegram)],
        ));
        orch.set_user_present(true);

        let notif = make_notification(NotificationUrgency::Medium, false);
        orch.notify(notif).await;

        // Should use the only available channel (Telegram at index 0).
        assert_eq!(telegram_counter.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn test_notification_config_default() {
        let config = NotificationConfig::default();
        assert_eq!(
            config.preferred_channels,
            vec![
                NotificationChannelKind::InApp,
                NotificationChannelKind::Telegram,
            ]
        );
        assert_eq!(config.escalation_timeout_secs, 300);
        assert!(config.quiet_hours.is_none());
    }

    #[tokio::test]
    async fn test_is_quiet_hours_with_all_day_range() {
        let config = NotificationConfig {
            quiet_hours: Some(QuietHours {
                start_hour: 0,
                start_minute: 0,
                end_hour: 23,
                end_minute: 59,
                allow_critical: true,
            }),
            ..make_config()
        };
        let orch = NotificationOrchestrator::new(config, vec![]);
        assert!(orch.is_quiet_hours());
    }

    #[tokio::test]
    async fn test_is_quiet_hours_without_config() {
        let config = NotificationConfig {
            quiet_hours: None,
            ..make_config()
        };
        let orch = NotificationOrchestrator::new(config, vec![]);
        assert!(!orch.is_quiet_hours());
    }

    fn make_notification_with_arc(
        urgency: NotificationUrgency,
        arc_id: Option<&str>,
    ) -> Notification {
        Notification {
            id: Uuid::new_v4(),
            urgency,
            title: "Test".to_string(),
            body: "Test body".to_string(),
            origin: NotificationOrigin::System,
            arc_id: arc_id.map(|s| s.to_string()),
            task_id: None,
            created_at: Utc::now(),
            requires_response: false,
        }
    }

    #[tokio::test]
    async fn test_list_notifications() {
        let (orch, _inapp, _tg) = make_orchestrator(make_config());
        orch.set_user_present(true);

        let n1 = make_notification(NotificationUrgency::Low, false);
        let n2 = make_notification(NotificationUrgency::High, false);
        orch.notify(n1).await;

        // Small sleep to ensure different timestamps.
        tokio::time::sleep(Duration::from_millis(10)).await;
        orch.notify(n2).await;

        let list = orch.list_notifications().await;
        assert_eq!(list.len(), 2);
        // Newest first.
        assert!(list[0].created_at >= list[1].created_at);
    }

    #[tokio::test]
    async fn test_mark_arc_read() {
        let (orch, _inapp, _tg) = make_orchestrator(make_config());
        orch.set_user_present(true);

        let n1 = make_notification_with_arc(NotificationUrgency::Medium, Some("arc1"));
        let n2 = make_notification_with_arc(NotificationUrgency::Medium, Some("arc2"));
        let n1_id = n1.id.to_string();
        let n2_id = n2.id.to_string();

        orch.notify(n1).await;
        orch.notify(n2).await;

        // Mark arc1 as read.
        orch.mark_arc_read("arc1").await;

        let list = orch.list_notifications().await;
        let arc1_notif = list.iter().find(|n| n.id == n1_id).unwrap();
        let arc2_notif = list.iter().find(|n| n.id == n2_id).unwrap();
        assert!(arc1_notif.is_read);
        assert!(!arc2_notif.is_read);
    }

    #[tokio::test]
    async fn test_mark_all_read() {
        let (orch, _inapp, _tg) = make_orchestrator(make_config());
        orch.set_user_present(true);

        orch.notify(make_notification(NotificationUrgency::Low, false))
            .await;
        orch.notify(make_notification(NotificationUrgency::Medium, false))
            .await;
        orch.notify(make_notification(NotificationUrgency::High, false))
            .await;

        orch.mark_all_read().await;

        let list = orch.list_notifications().await;
        assert_eq!(list.len(), 3);
        assert!(list.iter().all(|n| n.is_read));
    }

    #[tokio::test]
    async fn test_unread_count() {
        let (orch, _inapp, _tg) = make_orchestrator(make_config());
        orch.set_user_present(true);

        let n1 = make_notification(NotificationUrgency::Low, false);
        let n1_id = n1.id;
        orch.notify(n1).await;
        orch.notify(make_notification(NotificationUrgency::Medium, false))
            .await;
        orch.notify(make_notification(NotificationUrgency::High, false))
            .await;

        assert_eq!(orch.unread_count().await, 3);

        orch.mark_read(n1_id).await;
        assert_eq!(orch.unread_count().await, 2);
    }

    // --- Persistence tests ---

    /// Build an orchestrator backed by an in-memory SQLite database.
    async fn make_orchestrator_with_store(
        config: NotificationConfig,
    ) -> (Arc<NotificationOrchestrator>, Arc<AtomicUsize>) {
        let db = athen_persistence::Database::in_memory()
            .await
            .expect("in-memory DB");
        let store = db.notification_store();

        let inapp = MockChannel::new(NotificationChannelKind::InApp);
        let counter = inapp.counter();

        let orch = Arc::new(
            NotificationOrchestrator::new(config, vec![Box::new(inapp)]).with_store(store),
        );

        (orch, counter)
    }

    #[tokio::test]
    async fn test_notify_persists_to_db() {
        let (orch, _counter) = make_orchestrator_with_store(make_config()).await;
        orch.set_user_present(true);

        let notif = make_notification(NotificationUrgency::Medium, false);
        let notif_id = notif.id;
        orch.notify(notif).await;

        // Verify it's in the DB via list_notifications (which reads from DB).
        let list = orch.list_notifications().await;
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, notif_id.to_string());
        assert!(!list[0].is_read);
    }

    #[tokio::test]
    async fn test_mark_read_persists_to_db() {
        let (orch, _counter) = make_orchestrator_with_store(make_config()).await;
        orch.set_user_present(true);

        let notif = make_notification(NotificationUrgency::Medium, false);
        let notif_id = notif.id;
        orch.notify(notif).await;

        orch.mark_read(notif_id).await;

        let list = orch.list_notifications().await;
        assert_eq!(list.len(), 1);
        assert!(list[0].is_read);
    }

    #[tokio::test]
    async fn test_delete_notification() {
        let (orch, _counter) = make_orchestrator_with_store(make_config()).await;
        orch.set_user_present(true);

        let n1 = make_notification(NotificationUrgency::Low, false);
        let n2 = make_notification(NotificationUrgency::High, false);
        let n1_id = n1.id;
        orch.notify(n1).await;
        orch.notify(n2).await;

        assert_eq!(orch.list_notifications().await.len(), 2);

        orch.delete_notification(n1_id).await;

        let list = orch.list_notifications().await;
        assert_eq!(list.len(), 1);
        assert_ne!(list[0].id, n1_id.to_string());
    }

    #[tokio::test]
    async fn test_delete_read_notifications() {
        let (orch, _counter) = make_orchestrator_with_store(make_config()).await;
        orch.set_user_present(true);

        let n1 = make_notification(NotificationUrgency::Low, false);
        let n2 = make_notification(NotificationUrgency::Medium, false);
        let n3 = make_notification(NotificationUrgency::High, false);
        let n1_id = n1.id;
        let n2_id = n2.id;
        orch.notify(n1).await;
        orch.notify(n2).await;
        orch.notify(n3).await;

        // Mark two as read.
        orch.mark_read(n1_id).await;
        orch.mark_read(n2_id).await;

        let deleted = orch.delete_read_notifications().await;
        assert_eq!(deleted, 2);

        let list = orch.list_notifications().await;
        assert_eq!(list.len(), 1);
        assert!(!list[0].is_read);
    }

    #[tokio::test]
    async fn test_load_persisted_restores_notifications() {
        // Use a shared DB so two orchestrators see the same data.
        let db = athen_persistence::Database::in_memory()
            .await
            .expect("in-memory DB");
        let store = db.notification_store();

        // First orchestrator: send notifications.
        let inapp1 = MockChannel::new(NotificationChannelKind::InApp);
        let orch1 = Arc::new(
            NotificationOrchestrator::new(make_config(), vec![Box::new(inapp1)])
                .with_store(store.clone()),
        );
        orch1.set_user_present(true);

        let n1 = make_notification(NotificationUrgency::Low, false);
        let n2 = make_notification(NotificationUrgency::High, false);
        let n1_id = n1.id;
        orch1.notify(n1).await;
        orch1.notify(n2).await;
        orch1.mark_read(n1_id).await;

        // Second orchestrator (simulating restart): load from same DB.
        let inapp2 = MockChannel::new(NotificationChannelKind::InApp);
        let orch2 = Arc::new(
            NotificationOrchestrator::new(make_config(), vec![Box::new(inapp2)])
                .with_store(store),
        );
        orch2.load_persisted().await;

        let list = orch2.list_notifications().await;
        assert_eq!(list.len(), 2);

        let read_count = list.iter().filter(|n| n.is_read).count();
        let unread_count = list.iter().filter(|n| !n.is_read).count();
        assert_eq!(read_count, 1);
        assert_eq!(unread_count, 1);
    }

    #[tokio::test]
    async fn test_mark_all_read_persists_to_db() {
        let (orch, _counter) = make_orchestrator_with_store(make_config()).await;
        orch.set_user_present(true);

        orch.notify(make_notification(NotificationUrgency::Low, false)).await;
        orch.notify(make_notification(NotificationUrgency::Medium, false)).await;

        orch.mark_all_read().await;

        let list = orch.list_notifications().await;
        assert!(list.iter().all(|n| n.is_read));
        assert_eq!(orch.unread_count().await, 0);
    }
}
