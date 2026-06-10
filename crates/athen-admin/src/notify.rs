//! Panel-side push notifications.
//!
//! Instances can't push to a backgrounded phone — but the panel is a
//! long-lived server, so it subscribes to each running instance's
//! `/api/events` SSE stream (with the instance bearer, like the proxy)
//! and forwards the events a human must not miss to each granted user's
//! **notify webhook** (`users.notify_url`).
//!
//! The webhook contract is deliberately the simplest thing that reaches a
//! phone with zero accounts: a plain-text POST with `Title` / `Priority`
//! headers — exactly what [ntfy](https://ntfy.sh) topics accept, and easy
//! for anything else (a Discord/Slack shim, a home-automation hook) to
//! consume. FCM/APNs for a future React Native app slots in behind the
//! same `deliver()` seam.
//!
//! What gets forwarded:
//! - every `approval-question` (the agent is blocked on the user), and
//! - `notification` events that demand attention (`requires_response`,
//!   or urgency High/Critical) — routine notifications stay in-app.
//!
//! Supervision: one sweep loop re-lists instances every few seconds and
//! keeps exactly one watcher task per *running* container. A watcher
//! exits on any stream error and the next sweep restarts it, so instance
//! stop/start/restart needs no special handling. SSE bytes are buffered
//! across chunks and split on the blank-line frame boundary — per-chunk
//! parsing silently drops oversized events.

use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;

use crate::db::{Db, Instance, User};
use crate::{instances, PanelState};

const SWEEP_INTERVAL: Duration = Duration::from_secs(10);
/// Per-watcher memory of recently forwarded event ids (re-emits happen
/// on approval escalation paths; users should get one push, not three).
const SEEN_CAP: usize = 256;

/// Spawn the notification supervisor. Returns immediately.
pub fn spawn(state: Arc<PanelState>) {
    tokio::spawn(supervisor(state));
}

async fn supervisor(state: Arc<PanelState>) {
    let watching: Arc<Mutex<HashSet<String>>> = Arc::default();
    loop {
        if let Err(e) = sweep(&state, &watching).await {
            tracing::debug!(error = %e, "notify sweep failed (docker down?)");
        }
        tokio::time::sleep(SWEEP_INTERVAL).await;
    }
}

async fn sweep(
    state: &Arc<PanelState>,
    watching: &Arc<Mutex<HashSet<String>>>,
) -> anyhow::Result<()> {
    let all = instances::list_all(&state.db).await?;
    let status = state.docker.status_by_container().await?;
    for instance in all {
        let running = status
            .get(&instance.container_name)
            .map(|(s, _)| s == "running")
            .unwrap_or(false);
        if !running {
            continue;
        }
        {
            let mut w = watching.lock().expect("notify watching poisoned");
            if !w.insert(instance.id.clone()) {
                continue; // already watched
            }
        }
        let state = state.clone();
        let watching = watching.clone();
        tokio::spawn(async move {
            let id = instance.id.clone();
            if let Err(e) = watch_instance(&state, &instance).await {
                tracing::debug!(instance = %instance.name, error = %e, "event watch ended");
            }
            watching.lock().expect("notify watching poisoned").remove(&id);
        });
    }
    Ok(())
}

/// Hold one SSE connection to the instance and forward pushworthy events
/// until the stream ends (instance stopped, network error, …).
async fn watch_instance(state: &Arc<PanelState>, instance: &Instance) -> anyhow::Result<()> {
    let ip = state
        .docker
        .instance_ip(&instance.container_name, &state.cfg.network)
        .await?;
    let url = format!("http://{ip}:{}/api/events", instances::INSTANCE_PORT);
    let resp = state
        .http
        .get(&url)
        .bearer_auth(&instance.http_token)
        .send()
        .await?
        .error_for_status()?;
    tracing::info!(instance = %instance.name, "notification watcher connected");

    let mut stream = resp.bytes_stream();
    let mut buf = String::new();
    let mut seen: VecDeque<String> = VecDeque::new();
    while let Some(chunk) = stream.next().await {
        buf.push_str(&String::from_utf8_lossy(&chunk?));
        for frame in drain_sse_frames(&mut buf) {
            let Some(push) = pushworthy(&frame.event, &frame.data) else {
                continue;
            };
            // Dedup on event id when present.
            if let Some(id) = &push.dedup_id {
                if seen.contains(id) {
                    continue;
                }
                seen.push_back(id.clone());
                if seen.len() > SEEN_CAP {
                    seen.pop_front();
                }
            }
            deliver(state, instance, &push).await;
        }
    }
    Ok(())
}

/// One parsed SSE frame (`event:` name + concatenated `data:` lines).
#[derive(Debug, PartialEq)]
pub struct SseFrame {
    pub event: String,
    pub data: String,
}

/// Pull every complete (blank-line-terminated) SSE frame out of `buf`,
/// leaving any trailing partial frame in place for the next chunk.
pub fn drain_sse_frames(buf: &mut String) -> Vec<SseFrame> {
    let mut frames = Vec::new();
    while let Some(pos) = buf.find("\n\n") {
        let raw: String = buf.drain(..pos + 2).collect();
        let mut event = String::from("message");
        let mut data_lines: Vec<&str> = Vec::new();
        for line in raw.lines() {
            if let Some(v) = line.strip_prefix("event:") {
                event = v.trim().to_string();
            } else if let Some(v) = line.strip_prefix("data:") {
                data_lines.push(v.strip_prefix(' ').unwrap_or(v));
            }
            // comments (`:keep-alive`) and `id:`/`retry:` fields ignored
        }
        if !data_lines.is_empty() {
            frames.push(SseFrame {
                event,
                data: data_lines.join("\n"),
            });
        }
    }
    frames
}

/// A notification ready for webhook delivery.
#[derive(Debug, PartialEq)]
pub struct Push {
    pub title: String,
    pub body: String,
    /// ntfy priority header value.
    pub priority: &'static str,
    pub dedup_id: Option<String>,
}

/// Decide whether an instance event warrants a phone push, and shape it.
pub fn pushworthy(event: &str, data: &str) -> Option<Push> {
    let v: serde_json::Value = serde_json::from_str(data).ok()?;
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
    match event {
        "approval-question" => Some(Push {
            title: "Athen needs your approval".into(),
            body: {
                let (p, d) = (s("prompt"), s("description"));
                if d.is_empty() {
                    p
                } else {
                    format!("{p}\n{d}")
                }
            },
            priority: "high",
            dedup_id: v.get("id").and_then(|x| x.as_str()).map(String::from),
        }),
        "notification" => {
            let urgency = s("urgency");
            let requires_response = v
                .get("requires_response")
                .and_then(|x| x.as_bool())
                .unwrap_or(false);
            if !requires_response && urgency != "High" && urgency != "Critical" {
                return None; // routine — stays in-app
            }
            Some(Push {
                title: if s("title").is_empty() {
                    "Athen".into()
                } else {
                    s("title")
                },
                body: s("body"),
                priority: if urgency == "Critical" { "max" } else { "high" },
                dedup_id: v.get("id").and_then(|x| x.as_str()).map(String::from),
            })
        }
        _ => None,
    }
}

/// POST the push to every granted user's webhook. Failures are logged per
/// user and never abort the watcher.
async fn deliver(state: &Arc<PanelState>, instance: &Instance, push: &Push) {
    let users = match users_with_webhooks(&state.db, &instance.id).await {
        Ok(u) => u,
        Err(e) => {
            tracing::error!(error = %e, "notify: grant lookup failed");
            return;
        }
    };
    for user in users {
        // Headers must be ASCII-safe (the separator too — a unicode dash
        // here mojibakes on ntfy); full UTF-8 rides in the body.
        let title = format!(
            "{} - {}",
            sanitize_header(&push.title),
            sanitize_header(&instance.name)
        );
        let res = state
            .http
            .post(&user.notify_url)
            .timeout(Duration::from_secs(10))
            .header("Title", title)
            .header("Priority", push.priority)
            .header("X-Athen-Instance", sanitize_header(&instance.name))
            .body(push.body.clone())
            .send()
            .await
            .and_then(|r| r.error_for_status());
        match res {
            Ok(_) => tracing::info!(user = %user.username, instance = %instance.name, "push delivered"),
            Err(e) => {
                tracing::warn!(user = %user.username, error = %e, "push delivery failed")
            }
        }
    }
}

/// Granted users with a webhook configured. Admins are NOT implicitly
/// included — access is implicit for them, notifications are opt-in via
/// an explicit grant on the instance.
async fn users_with_webhooks(db: &Db, instance_id: &str) -> anyhow::Result<Vec<User>> {
    let iid = instance_id.to_string();
    db.call(move |c| {
        let mut stmt = c.prepare(
            "SELECT u.* FROM users u JOIN user_instances ui ON ui.user_id = u.id \
             WHERE ui.instance_id = ?1 AND u.notify_url <> ''",
        )?;
        let rows = stmt.query_map([iid], User::from_row)?;
        rows.collect()
    })
    .await
}

fn sanitize_header(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_graphic() || c == ' ' { c } else { '?' })
        .take(120)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_frames_buffer_across_chunks() {
        let mut buf = String::new();
        buf.push_str("event: approval-question\ndata: {\"id\":");
        assert!(drain_sse_frames(&mut buf).is_empty(), "partial frame held");
        buf.push_str("\"q1\"}\n\n:keep-alive\n\nevent: agent-stream\ndata: {}\n\nevent: x\ndata: par");
        let frames = drain_sse_frames(&mut buf);
        assert_eq!(
            frames,
            vec![
                SseFrame {
                    event: "approval-question".into(),
                    data: "{\"id\":\"q1\"}".into()
                },
                SseFrame {
                    event: "agent-stream".into(),
                    data: "{}".into()
                },
            ]
        );
        assert_eq!(buf, "event: x\ndata: par", "tail kept for next chunk");
    }

    #[test]
    fn multiline_data_joined() {
        let mut buf = String::from("data: line1\ndata: line2\n\n");
        let frames = drain_sse_frames(&mut buf);
        assert_eq!(frames[0].event, "message");
        assert_eq!(frames[0].data, "line1\nline2");
    }

    #[test]
    fn approval_questions_always_push() {
        let p = pushworthy(
            "approval-question",
            r#"{"id":"q1","prompt":"Send this email?","description":"to bob@x.com","choices":[]}"#,
        )
        .expect("approval pushes");
        assert_eq!(p.priority, "high");
        assert!(p.body.contains("Send this email?"));
        assert!(p.body.contains("bob@x.com"));
        assert_eq!(p.dedup_id.as_deref(), Some("q1"));
    }

    #[test]
    fn notifications_filtered_by_urgency_and_response() {
        let routine = r#"{"id":"n1","urgency":"Medium","title":"FYI","body":"b","requires_response":false}"#;
        assert!(pushworthy("notification", routine).is_none());
        let needs_reply = r#"{"id":"n2","urgency":"Low","title":"Q","body":"b","requires_response":true}"#;
        assert!(pushworthy("notification", needs_reply).is_some());
        let critical = r#"{"id":"n3","urgency":"Critical","title":"!","body":"b","requires_response":false}"#;
        assert_eq!(pushworthy("notification", critical).unwrap().priority, "max");
        // Other event types never push.
        assert!(pushworthy("agent-stream", "{}").is_none());
    }

    #[test]
    fn header_sanitizer_strips_non_ascii() {
        assert_eq!(sanitize_header("ok — café\n"), "ok ? caf??");
        assert!(sanitize_header(&"x".repeat(500)).len() <= 120);
    }
}
