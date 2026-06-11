//! HTTP API for remote clients (React / React Native companions, cloud
//! deployments).
//!
//! Exposes the same chat-critical surface the Tauri WebView uses — the
//! handlers call the exact `*_core` functions behind the Tauri commands,
//! so a remote client and the desktop frontend see identical semantics.
//! Live events (`agent-stream` deltas, `agent-progress` tool cards,
//! `approval-question`, `arc-updated`, …) reach remote clients through
//! `GET /api/events` as Server-Sent Events fed by the `UiBridge` event
//! bus; the event names and payloads are byte-identical to what the
//! WebView receives via Tauri emit.
//!
//! Enabled by setting `ATHEN_HTTP_ADDR` (e.g. `127.0.0.1:8787` or
//! `0.0.0.0:8787`). Every endpoint except `/api/health` requires the
//! bearer token: `Authorization: Bearer <token>`, `X-Athen-Token`
//! header, or `?token=` query parameter (EventSource can't set
//! headers). The token comes from `ATHEN_HTTP_TOKEN` /
//! `ATHEN_HTTP_TOKEN_FILE`, or is auto-generated and persisted at
//! `<data_dir>/http_token` (mode 0600) on first start.
//!
//! Transport security is the operator's job: bind to localhost, a VPN
//! interface, or put a TLS-terminating reverse proxy in front for
//! anything reachable from the internet. The token gates access; it
//! does not encrypt the wire.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{DefaultBodyLimit, Path as UrlPath, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use tokio_stream::StreamExt;

use crate::commands;
use crate::ui_bridge::UiBridge;

/// Maximum request body: composer images/attachments ride base64 in
/// JSON, so the Tauri-equivalent payloads can run tens of megabytes.
const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone)]
pub struct HttpApiConfig {
    pub addr: SocketAddr,
    pub token: String,
}

impl HttpApiConfig {
    /// Resolve the HTTP API config from the environment. `None` (API
    /// disabled) unless `ATHEN_HTTP_ADDR` is set to a valid socket
    /// address. Token precedence: `ATHEN_HTTP_TOKEN` (+`_FILE` variant)
    /// → persisted `<data_dir>/http_token` → freshly generated.
    pub fn from_env(data_dir: &Path) -> Option<Self> {
        let raw = std::env::var("ATHEN_HTTP_ADDR").ok()?;
        if raw.trim().is_empty() {
            return None;
        }
        let addr: SocketAddr = match raw.parse() {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!(addr = %raw, "ATHEN_HTTP_ADDR unparseable ({e}); HTTP API disabled");
                return None;
            }
        };
        let env_reader = |name: &str| std::env::var(name).ok();
        let token = crate::env_creds::lookup_env_secret("ATHEN_HTTP_TOKEN", &env_reader)
            .unwrap_or_else(|| load_or_create_token(data_dir));
        Some(Self { addr, token })
    }
}

/// Read the persisted API token, or mint one (two UUIDv4s, 244 bits of
/// OS randomness) and persist it at `<data_dir>/http_token`, mode 0600.
fn load_or_create_token(data_dir: &Path) -> String {
    let path = data_dir.join("http_token");
    if let Ok(s) = std::fs::read_to_string(&path) {
        let s = s.trim();
        if !s.is_empty() {
            return s.to_string();
        }
    }
    let token = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    if let Err(e) = std::fs::create_dir_all(data_dir) {
        tracing::warn!(error = %e, "could not create data dir for http_token");
    }
    if let Err(e) = std::fs::write(&path, &token) {
        tracing::warn!(error = %e, path = %path.display(), "could not persist http_token (token is ephemeral this run)");
    } else {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        tracing::info!(path = %path.display(), "HTTP API token generated (clients authenticate with its contents)");
    }
    token
}

#[derive(Clone)]
struct ApiState {
    ui: UiBridge,
    token: Arc<String>,
}

/// Serve the HTTP API forever. Call once from a composition root after
/// the `AppState` is reachable through `ui` (Tauri managed state or the
/// published headless singleton).
pub async fn serve(cfg: HttpApiConfig, ui: UiBridge) -> std::io::Result<()> {
    // Bus must be live before the first subscriber AND before loops
    // emit; composition roots also init it early — this is a no-op then.
    UiBridge::init_event_bus();

    let api = ApiState {
        ui,
        token: Arc::new(cfg.token),
    };

    let app = Router::new()
        .route("/api/health", get(health))
        .route("/api/events", get(events))
        .route("/api/arcs", get(arcs_list).post(arcs_create))
        .route("/api/arcs/current", get(arc_current))
        .route("/api/arcs/{arc_id}/entries", get(arc_entries))
        .route("/api/arcs/{arc_id}/select", post(arc_select))
        .route("/api/messages", post(messages_send))
        .route("/api/messages/queue", post(messages_queue))
        .route("/api/approvals/task", post(approvals_task))
        .route("/api/approvals/question", post(approvals_question))
        .route("/api/cancel", post(cancel_all))
        .route("/api/agents", get(agents_list))
        .route("/api/agents/{task_id}/cancel", post(agent_cancel))
        .route("/api/notifications", get(notifications_list))
        .route("/api/notifications/read-all", post(notifications_read_all))
        .route("/api/notifications/{id}/read", post(notification_read))
        .layer(axum::middleware::from_fn_with_state(
            api.clone(),
            require_token,
        ))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        // Remote web clients (React dev server, hosted dashboard) need
        // CORS; auth is the token, not the origin.
        .layer(tower_http::cors::CorsLayer::permissive())
        .with_state(api);

    let listener = tokio::net::TcpListener::bind(cfg.addr).await?;
    tracing::info!(addr = %cfg.addr, "HTTP API listening");
    axum::serve(listener, app).await
}

// ---------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct TokenQuery {
    token: Option<String>,
}

/// Constant-time string compare — token checks shouldn't leak prefix
/// length through response timing.
fn ct_eq(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

async fn require_token(
    State(api): State<ApiState>,
    Query(q): Query<TokenQuery>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if req.uri().path() == "/api/health" {
        return next.run(req).await;
    }
    let headers = req.headers();
    let presented = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_string)
        .or_else(|| {
            headers
                .get("x-athen-token")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        })
        .or(q.token);
    match presented {
        Some(t) if ct_eq(&t, &api.token) => next.run(req).await,
        _ => (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "missing or invalid token"})),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// Map the `Result<T, String>` shape every `*_core` returns onto HTTP.
fn json_result<T: serde::Serialize>(r: Result<T, String>) -> Response {
    match r {
        Ok(v) => Json(v).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response(),
    }
}

async fn health() -> Response {
    Json(json!({
        "status": "ok",
        "name": "athen",
        "version": env!("CARGO_PKG_VERSION"),
    }))
    .into_response()
}

/// SSE stream of every UI event. Event names/payloads mirror the Tauri
/// events the WebView consumes (`agent-stream`, `agent-progress`,
/// `approval-question`, `arc-updated`, `notification`, `sense-event`,
/// …). A slow consumer that falls behind the bus's buffer gets a
/// synthetic `lagged` event with the dropped count — refetch state via
/// REST when you see one.
async fn events(State(_api): State<ApiState>) -> Response {
    let Some(rx) = UiBridge::subscribe_events() else {
        // serve() initializes the bus, so this is unreachable in
        // practice — but degrade with a clear error rather than panic.
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "event bus not initialized"})),
        )
            .into_response();
    };
    let stream = tokio_stream::wrappers::BroadcastStream::new(rx).map(|item| match item {
        Ok(ev) => Ok::<Event, std::convert::Infallible>(
            Event::default()
                .event(ev.event)
                .data(ev.payload.to_string()),
        ),
        Err(tokio_stream::wrappers::errors::BroadcastStreamRecvError::Lagged(n)) => {
            Ok(Event::default().event("lagged").data(n.to_string()))
        }
    });
    Sse::new(stream)
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(15))
                .text("keep-alive"),
        )
        .into_response()
}

async fn arcs_list(State(api): State<ApiState>) -> Response {
    json_result(commands::list_arcs_core(api.ui.app_state()).await)
}

async fn arcs_create(State(api): State<ApiState>) -> Response {
    json_result(
        commands::new_arc_core(api.ui.app_state())
            .await
            .map(|id| json!({"arc_id": id})),
    )
}

async fn arc_current(State(api): State<ApiState>) -> Response {
    json_result(
        commands::get_current_arc_core(api.ui.app_state())
            .await
            .map(|id| json!({"arc_id": id})),
    )
}

async fn arc_entries(State(api): State<ApiState>, UrlPath(arc_id): UrlPath<String>) -> Response {
    json_result(commands::get_arc_entries_core(arc_id, api.ui.app_state()).await)
}

async fn arc_select(State(api): State<ApiState>, UrlPath(arc_id): UrlPath<String>) -> Response {
    json_result(commands::switch_arc_core(arc_id, api.ui.app_state()).await)
}

#[derive(Deserialize)]
struct SendMessageBody {
    message: String,
    /// Target arc. When set and different from the active arc, the
    /// server switches first (same as the desktop sidebar click).
    /// Omitted → the message lands on the currently active arc.
    arc_id: Option<String>,
    images: Option<Vec<athen_core::llm::ImageInput>>,
    attachments: Option<Vec<commands::UploadedAttachment>>,
}

/// Send a chat message. Long-poll semantics, exactly like the Tauri
/// `send_message` command: the response resolves when the agent turn
/// finishes (or parks on `pending_approval`). Consume `/api/events`
/// for live streaming while this is in flight.
async fn messages_send(State(api): State<ApiState>, Json(body): Json<SendMessageBody>) -> Response {
    let state = api.ui.app_state();
    if let Some(ref target) = body.arc_id {
        let active = state.active_arc_id.lock().await.clone();
        if *target != active {
            match state.arc_store.as_ref() {
                Some(store) => match store.get_arc(target).await {
                    Ok(Some(_)) => {
                        if let Err(e) = commands::switch_arc_core(target.clone(), state).await {
                            return json_result::<()>(Err(e));
                        }
                    }
                    Ok(None) => {
                        return (
                            StatusCode::NOT_FOUND,
                            Json(json!({"error": format!("unknown arc {target}")})),
                        )
                            .into_response();
                    }
                    Err(e) => return json_result::<()>(Err(e.to_string())),
                },
                None => {
                    return json_result::<()>(Err("arc store not initialized".into()));
                }
            }
        }
    }
    json_result(
        commands::send_message_core(body.message, body.images, body.attachments, &api.ui, state)
            .await,
    )
}

#[derive(Deserialize)]
struct QueueBody {
    arc_id: String,
    text: String,
}

async fn messages_queue(State(api): State<ApiState>, Json(body): Json<QueueBody>) -> Response {
    json_result(
        commands::queue_user_input_core(body.arc_id, body.text, api.ui.app_state())
            .await
            .map(|()| json!({"queued": true})),
    )
}

#[derive(Deserialize)]
struct ApproveTaskBody {
    task_id: String,
    approved: bool,
}

async fn approvals_task(
    State(api): State<ApiState>,
    Json(body): Json<ApproveTaskBody>,
) -> Response {
    json_result(
        commands::approve_task_core(body.task_id, body.approved, &api.ui, api.ui.app_state()).await,
    )
}

#[derive(Deserialize)]
struct ApprovalQuestionBody {
    question_id: String,
    choice_key: String,
}

async fn approvals_question(
    State(api): State<ApiState>,
    Json(body): Json<ApprovalQuestionBody>,
) -> Response {
    json_result(
        commands::submit_approval_core(body.question_id, body.choice_key, api.ui.app_state())
            .await
            .map(|resolved| json!({"resolved": resolved})),
    )
}

async fn cancel_all(State(api): State<ApiState>) -> Response {
    json_result(
        commands::cancel_task_core(api.ui.app_state())
            .await
            .map(|()| json!({"cancelled": true})),
    )
}

async fn agents_list(State(api): State<ApiState>) -> Response {
    json_result(commands::list_active_agents_core(api.ui.app_state()).await)
}

async fn agent_cancel(State(api): State<ApiState>, UrlPath(task_id): UrlPath<String>) -> Response {
    json_result(
        commands::cancel_agent_core(api.ui.app_state(), task_id)
            .await
            .map(|found| json!({"cancelled": found})),
    )
}

async fn notifications_list(State(api): State<ApiState>) -> Response {
    json_result(commands::list_notifications_core(api.ui.app_state()).await)
}

async fn notifications_read_all(State(api): State<ApiState>) -> Response {
    json_result(
        commands::mark_all_notifications_read_core(api.ui.app_state())
            .await
            .map(|()| json!({"ok": true})),
    )
}

async fn notification_read(State(api): State<ApiState>, UrlPath(id): UrlPath<String>) -> Response {
    json_result(
        commands::mark_notification_read_core(api.ui.app_state(), id)
            .await
            .map(|()| json!({"ok": true})),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ct_eq_basic() {
        assert!(ct_eq("abc", "abc"));
        assert!(!ct_eq("abc", "abd"));
        assert!(!ct_eq("abc", "ab"));
        assert!(!ct_eq("", "a"));
        assert!(ct_eq("", ""));
    }

    #[test]
    fn token_persists_and_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let t1 = load_or_create_token(dir.path());
        assert_eq!(t1.len(), 64);
        let t2 = load_or_create_token(dir.path());
        assert_eq!(t1, t2, "second call must reload, not regenerate");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join("http_token"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn from_env_requires_addr() {
        // ATHEN_HTTP_ADDR unset in the test environment → disabled.
        // (Read-only env access; safe under parallel tests.)
        if std::env::var("ATHEN_HTTP_ADDR").is_err() {
            let dir = tempfile::tempdir().unwrap();
            assert!(HttpApiConfig::from_env(dir.path()).is_none());
        }
    }
}
