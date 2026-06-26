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
//!
//! The same listener also serves the embedded web UI (the React app in
//! `web/`, built to `web/dist` and compiled into the binary via
//! rust-embed): any non-`/api/*` path falls back to the SPA. The app
//! shell is public by design — every byte of user data still sits
//! behind the token-gated `/api/*` routes; the login screen just asks
//! for that token.

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

use std::collections::HashMap;

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
        .route("/api/grants/pending", get(grants_pending))
        .route("/api/grants/{id}", post(grant_resolve))
        .route("/api/cancel", post(cancel_all))
        .route("/api/agents", get(agents_list))
        .route("/api/agents/{task_id}/cancel", post(agent_cancel))
        .route("/api/notifications", get(notifications_list))
        .route("/api/notifications/read-all", post(notifications_read_all))
        .route("/api/notifications/{id}/read", post(notification_read))
        .merge(full_surface_router())
        // The embedded web UI rides every non-/api path (SPA fallback);
        // require_token skips it — only /api/* carries user data.
        .fallback(static_asset)
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
    let path = req.uri().path();
    // Health is the readiness probe; non-/api paths are the embedded
    // web UI shell (static assets, no user data).
    if path == "/api/health" || !path.starts_with("/api/") {
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
// Embedded web UI
// ---------------------------------------------------------------------------

/// The React app from `web/`, built to `web/dist` (`npm run build`) and
/// committed so `cargo build` never needs Node. Debug builds read the
/// folder from disk (edit → rebuild dist → refresh); release builds
/// embed the bytes.
#[derive(rust_embed::Embed)]
#[folder = "../../web/dist"]
struct WebAssets;

fn content_type_for(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("json") => "application/json",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        Some("map") => "application/json",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// Serve the embedded SPA: exact asset match first, otherwise
/// `index.html` (client-side routing). Vite emits content-hashed
/// filenames under `assets/`, so those get an immutable cache; the
/// HTML shell must revalidate so a new binary's hashes are picked up.
async fn static_asset(uri: axum::http::Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    let (file, name) = match WebAssets::get(path) {
        Some(f) => (f, path),
        None => match WebAssets::get("index.html") {
            Some(f) => (f, "index.html"),
            None => {
                return (
                    StatusCode::NOT_FOUND,
                    "web UI not bundled in this build (web/dist was empty)",
                )
                    .into_response();
            }
        },
    };
    let cache = if name.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    (
        [
            (axum::http::header::CONTENT_TYPE, content_type_for(name)),
            (axum::http::header::CACHE_CONTROL, cache),
        ],
        file.data,
    )
        .into_response()
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

/// File-permission prompts currently parked on this instance. Each entry
/// mirrors the `grant-requested` SSE event payload; answer with
/// `POST /api/grants/{id}`.
async fn grants_pending(State(api): State<ApiState>) -> Response {
    json_result(commands::list_pending_grants_core(api.ui.app_state()).await)
}

#[derive(Deserialize)]
struct GrantResolveBody {
    /// `"Allow"`, `"AllowAlways"`, `"Deny"`, or
    /// `{"AllowProjectRoot": "/abs/path"}` (serde externally-tagged —
    /// same wire shape the desktop frontend sends).
    decision: crate::file_gate::GrantDecision,
}

/// Answer a parked file-permission prompt — the remote-client analogue
/// of the desktop `resolve_pending_grant` command, and the missing piece
/// that used to leave headless agents hung on out-of-workspace writes.
async fn grant_resolve(
    State(api): State<ApiState>,
    UrlPath(id): UrlPath<String>,
    Json(body): Json<GrantResolveBody>,
) -> Response {
    json_result(
        commands::resolve_pending_grant_core(api.ui.app_state(), id, body.decision)
            .await
            .map(|()| json!({"resolved": true})),
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

// ---------------------------------------------------------------------------
// Full command surface (settings / arcs / memory / contacts / wake-ups / …)
//
// Same pattern as the chat routes above: every handler is a thin shim over
// the `*_core` function the matching Tauri command delegates to, so the web
// client and the desktop WebView get identical semantics. Deliberately NOT
// exposed (admin-only / desktop-native): updater + runtime installs,
// bundled-model download/delete, Voice/Pipecat setup, onboarding wizard.
// ---------------------------------------------------------------------------

// ---- handler shims (one per core fn; bodies mirror the Tauri arg names) ----

#[derive(Deserialize)]
struct NameBody {
    name: String,
}
#[derive(Deserialize)]
struct EnabledBody {
    enabled: bool,
}

async fn h_arc_rename(
    State(api): State<ApiState>,
    UrlPath(arc_id): UrlPath<String>,
    Json(b): Json<NameBody>,
) -> Response {
    json_result(commands::rename_arc_core(arc_id, b.name, api.ui.app_state()).await)
}
async fn h_arc_delete(State(api): State<ApiState>, UrlPath(arc_id): UrlPath<String>) -> Response {
    json_result(
        commands::delete_arc_core(arc_id, api.ui.app_state())
            .await
            .map(|id| json!({"active_arc_id": id})),
    )
}
async fn h_arc_compact(State(api): State<ApiState>, UrlPath(arc_id): UrlPath<String>) -> Response {
    json_result(commands::compact_arc_core(arc_id, api.ui.app_state()).await)
}
#[derive(Deserialize)]
struct BranchBody {
    parent_arc_id: String,
    name: String,
    up_to_entry_id: i64,
}
async fn h_arc_branch(State(api): State<ApiState>, Json(b): Json<BranchBody>) -> Response {
    json_result(
        commands::branch_arc_core(
            b.parent_arc_id,
            b.name,
            b.up_to_entry_id,
            api.ui.app_state(),
        )
        .await
        .map(|id| json!({"arc_id": id})),
    )
}
async fn h_status(State(api): State<ApiState>) -> Response {
    json_result(commands::get_status_core(api.ui.app_state()).await)
}
#[derive(Deserialize)]
struct LimitQuery {
    limit: Option<u32>,
}
async fn h_agent_runs(State(api): State<ApiState>, Query(q): Query<LimitQuery>) -> Response {
    json_result(commands::list_recent_agent_runs_core(api.ui.app_state(), q.limit).await)
}
async fn h_attachments_for_event(
    State(api): State<ApiState>,
    UrlPath(event_id): UrlPath<String>,
) -> Response {
    json_result(commands::list_attachments_for_event_core(event_id, api.ui.app_state()).await)
}

// goal + plan
#[derive(Deserialize)]
struct GoalBody {
    goal: String,
    criteria: Option<String>,
}
async fn h_goal_get(State(api): State<ApiState>) -> Response {
    json_result(commands::get_arc_goal_core(api.ui.app_state()).await)
}
async fn h_goal_set(State(api): State<ApiState>, Json(b): Json<GoalBody>) -> Response {
    json_result(commands::set_arc_goal_core(b.goal, b.criteria, api.ui.app_state()).await)
}
async fn h_goal_clear(State(api): State<ApiState>) -> Response {
    json_result(commands::clear_arc_goal_core(api.ui.app_state()).await)
}
async fn h_plan_get(State(api): State<ApiState>) -> Response {
    json_result(commands::get_plan_core(api.ui.app_state()).await)
}
#[derive(Deserialize)]
struct DescriptionBody {
    description: String,
}
async fn h_plan_start(State(api): State<ApiState>, Json(b): Json<DescriptionBody>) -> Response {
    json_result(commands::start_plan_core(b.description, api.ui.app_state(), &api.ui).await)
}
async fn h_plan_approve(State(api): State<ApiState>) -> Response {
    json_result(commands::approve_plan_core(api.ui.app_state()).await)
}
async fn h_plan_clear(State(api): State<ApiState>) -> Response {
    json_result(commands::clear_plan_core(api.ui.app_state()).await)
}
async fn h_plan_update(
    State(api): State<ApiState>,
    Json(update): Json<commands::PlanDraftUpdate>,
) -> Response {
    json_result(commands::update_plan_draft_core(update, api.ui.app_state()).await)
}

// profiles + per-arc pickers
async fn h_profiles_list(State(api): State<ApiState>) -> Response {
    json_result(commands::list_agent_profiles_core(api.ui.app_state()).await)
}
async fn h_profile_create(
    State(api): State<ApiState>,
    Json(input): Json<commands::AgentProfileInput>,
) -> Response {
    json_result(commands::create_agent_profile_core(input, api.ui.app_state()).await)
}
async fn h_profile_update(
    State(api): State<ApiState>,
    Json(input): Json<commands::AgentProfileInput>,
) -> Response {
    json_result(commands::update_agent_profile_core(input, api.ui.app_state()).await)
}
async fn h_profile_delete(State(api): State<ApiState>, UrlPath(id): UrlPath<String>) -> Response {
    json_result(commands::delete_agent_profile_core(id, api.ui.app_state()).await)
}
async fn h_profile_restore(State(api): State<ApiState>, UrlPath(id): UrlPath<String>) -> Response {
    json_result(commands::restore_agent_profile_core(id, api.ui.app_state()).await)
}
async fn h_profile_tokens(State(api): State<ApiState>, UrlPath(id): UrlPath<String>) -> Response {
    json_result(commands::estimate_profile_tokens_core(api.ui.app_state(), &api.ui, id).await)
}
#[derive(Deserialize)]
struct OptStringBody {
    value: Option<String>,
}
async fn h_arc_set_profile(
    State(api): State<ApiState>,
    UrlPath(arc_id): UrlPath<String>,
    Json(b): Json<OptStringBody>,
) -> Response {
    json_result(commands::set_arc_profile_core(arc_id, b.value, api.ui.app_state()).await)
}
async fn h_arc_set_effort(
    State(api): State<ApiState>,
    UrlPath(arc_id): UrlPath<String>,
    Json(b): Json<OptStringBody>,
) -> Response {
    json_result(commands::set_arc_reasoning_effort_core(arc_id, b.value, api.ui.app_state()).await)
}
async fn h_arc_set_tier(
    State(api): State<ApiState>,
    UrlPath(arc_id): UrlPath<String>,
    Json(b): Json<OptStringBody>,
) -> Response {
    json_result(commands::set_arc_tier_core(arc_id, b.value, api.ui.app_state()).await)
}
async fn h_arc_set_security(
    State(api): State<ApiState>,
    UrlPath(arc_id): UrlPath<String>,
    Json(b): Json<OptStringBody>,
) -> Response {
    json_result(commands::set_arc_security_mode_core(arc_id, b.value, api.ui.app_state()).await)
}

// checkpoints / changes rail
async fn h_arc_snapshots(
    State(api): State<ApiState>,
    UrlPath(arc_id): UrlPath<String>,
) -> Response {
    json_result(commands::list_arc_snapshots_core(api.ui.app_state(), arc_id).await)
}
async fn h_snapshot_revert(
    State(api): State<ApiState>,
    UrlPath(action_id): UrlPath<String>,
) -> Response {
    json_result(commands::revert_snapshot_core(api.ui.app_state(), action_id).await)
}
#[derive(Deserialize)]
struct RewindBody {
    action_id: String,
}
async fn h_arc_rewind(
    State(api): State<ApiState>,
    UrlPath(arc_id): UrlPath<String>,
    Json(b): Json<RewindBody>,
) -> Response {
    json_result(commands::rewind_changes_core(api.ui.app_state(), arc_id, b.action_id).await)
}

// calendar events
#[derive(Deserialize)]
struct CalRangeQuery {
    start: String,
    end: String,
}
async fn h_cal_events(State(api): State<ApiState>, Query(q): Query<CalRangeQuery>) -> Response {
    json_result(commands::list_calendar_events_core(q.start, q.end, api.ui.app_state()).await)
}
#[derive(Deserialize)]
struct CalCreateBody {
    event: athen_persistence::calendar::CalendarEvent,
    target_source_id: Option<String>,
    target_calendar_id: Option<String>,
}
async fn h_cal_event_create(State(api): State<ApiState>, Json(b): Json<CalCreateBody>) -> Response {
    json_result(
        commands::create_calendar_event_core(
            b.event,
            b.target_source_id,
            b.target_calendar_id,
            api.ui.app_state(),
        )
        .await,
    )
}
#[derive(Deserialize)]
struct CalUpdateBody {
    event: athen_persistence::calendar::CalendarEvent,
}
async fn h_cal_event_update(State(api): State<ApiState>, Json(b): Json<CalUpdateBody>) -> Response {
    json_result(commands::update_calendar_event_core(b.event, api.ui.app_state()).await)
}
async fn h_cal_event_delete(State(api): State<ApiState>, UrlPath(id): UrlPath<String>) -> Response {
    json_result(commands::delete_calendar_event_core(id, api.ui.app_state()).await)
}

// notifications extra
async fn h_notif_seen(State(api): State<ApiState>, UrlPath(id): UrlPath<String>) -> Response {
    json_result(commands::mark_notification_seen_core(api.ui.app_state(), id).await)
}
async fn h_notif_delete(State(api): State<ApiState>, UrlPath(id): UrlPath<String>) -> Response {
    json_result(commands::delete_notification_core(api.ui.app_state(), id).await)
}
async fn h_notif_delete_read(State(api): State<ApiState>) -> Response {
    json_result(commands::delete_read_notifications_core(api.ui.app_state()).await)
}

// memory + knowledge graph
async fn h_memory_list(State(api): State<ApiState>) -> Response {
    json_result(commands::list_memories_core(api.ui.app_state()).await)
}
#[derive(Deserialize)]
struct ContentBody {
    content: String,
}
async fn h_memory_update(
    State(api): State<ApiState>,
    UrlPath(id): UrlPath<String>,
    Json(b): Json<ContentBody>,
) -> Response {
    json_result(commands::update_memory_core(api.ui.app_state(), id, b.content).await)
}
async fn h_memory_delete(State(api): State<ApiState>, UrlPath(id): UrlPath<String>) -> Response {
    json_result(commands::delete_memory_core(api.ui.app_state(), id).await)
}
async fn h_entities_list(State(api): State<ApiState>) -> Response {
    json_result(commands::list_entities_core(api.ui.app_state()).await)
}
#[derive(Deserialize)]
struct EntityUpdateBody {
    name: Option<String>,
    entity_type: Option<String>,
}
async fn h_entity_update(
    State(api): State<ApiState>,
    UrlPath(id): UrlPath<String>,
    Json(b): Json<EntityUpdateBody>,
) -> Response {
    json_result(commands::update_entity_core(api.ui.app_state(), id, b.name, b.entity_type).await)
}
async fn h_entity_delete(State(api): State<ApiState>, UrlPath(id): UrlPath<String>) -> Response {
    json_result(commands::delete_entity_core(api.ui.app_state(), id).await)
}
async fn h_relations_list(State(api): State<ApiState>) -> Response {
    json_result(commands::list_relations_core(api.ui.app_state()).await)
}
#[derive(Deserialize)]
struct RelationDeleteBody {
    from_id: String,
    to_id: String,
    relation: String,
}
async fn h_relation_delete(
    State(api): State<ApiState>,
    Json(b): Json<RelationDeleteBody>,
) -> Response {
    json_result(
        commands::delete_relation_core(api.ui.app_state(), b.from_id, b.to_id, b.relation).await,
    )
}

// MCP
async fn h_mcp_catalog(State(api): State<ApiState>) -> Response {
    json_result(commands::list_mcp_catalog_core(api.ui.app_state()).await)
}
#[derive(Deserialize)]
struct McpEnableBody {
    config: serde_json::Value,
}
async fn h_mcp_enable(
    State(api): State<ApiState>,
    UrlPath(id): UrlPath<String>,
    Json(b): Json<McpEnableBody>,
) -> Response {
    json_result(commands::enable_mcp_core(api.ui.app_state(), id, b.config).await)
}
async fn h_mcp_disable(State(api): State<ApiState>, UrlPath(id): UrlPath<String>) -> Response {
    json_result(commands::disable_mcp_core(api.ui.app_state(), id).await)
}
async fn h_mcp_custom(State(api): State<ApiState>) -> Response {
    json_result(commands::mcp_list_custom_core(api.ui.app_state()).await)
}
async fn h_mcp_enabled(State(api): State<ApiState>) -> Response {
    json_result(commands::mcp_list_enabled_core(api.ui.app_state()).await)
}
#[derive(Deserialize)]
struct McpAddBody {
    entry: athen_core::traits::mcp::McpCatalogEntry,
    #[serde(default)]
    env_secrets: HashMap<String, String>,
    #[serde(default)]
    enable_now: bool,
}
async fn h_mcp_add_custom(State(api): State<ApiState>, Json(b): Json<McpAddBody>) -> Response {
    json_result(
        commands::mcp_add_custom_core(b.entry, b.env_secrets, b.enable_now, api.ui.app_state())
            .await,
    )
}
async fn h_mcp_remove_custom(
    State(api): State<ApiState>,
    UrlPath(id): UrlPath<String>,
) -> Response {
    json_result(commands::mcp_remove_custom_core(id, api.ui.app_state()).await)
}
#[derive(Deserialize)]
struct EnableBody {
    enable: bool,
}
async fn h_mcp_set_enabled(
    State(api): State<ApiState>,
    UrlPath(id): UrlPath<String>,
    Json(b): Json<EnableBody>,
) -> Response {
    json_result(commands::mcp_set_enabled_core(id, b.enable, api.ui.app_state()).await)
}
#[derive(Deserialize)]
struct McpTestBody {
    entry: athen_core::traits::mcp::McpCatalogEntry,
    #[serde(default)]
    env_secrets: HashMap<String, String>,
}
async fn h_mcp_test_spawn(State(api): State<ApiState>, Json(b): Json<McpTestBody>) -> Response {
    json_result(commands::mcp_test_spawn_core(b.entry, b.env_secrets, api.ui.app_state()).await)
}
async fn h_mcp_tools_for(State(api): State<ApiState>, UrlPath(id): UrlPath<String>) -> Response {
    json_result(commands::mcp_list_tools_for_core(id, api.ui.app_state()).await)
}
#[derive(Deserialize)]
struct McpRisksBody {
    default_risk: athen_core::risk::BaseImpact,
    #[serde(default)]
    tool_overrides: HashMap<String, athen_core::risk::BaseImpact>,
}
async fn h_mcp_set_risks(
    State(api): State<ApiState>,
    UrlPath(id): UrlPath<String>,
    Json(b): Json<McpRisksBody>,
) -> Response {
    json_result(
        commands::mcp_set_risks_core(id, b.default_risk, b.tool_overrides, api.ui.app_state())
            .await,
    )
}

// directory grants
async fn h_grants_arc(State(api): State<ApiState>, UrlPath(arc_id): UrlPath<String>) -> Response {
    json_result(commands::list_arc_grants_core(api.ui.app_state(), arc_id).await)
}
async fn h_grants_global(State(api): State<ApiState>) -> Response {
    json_result(commands::list_global_grants_core(api.ui.app_state()).await)
}
#[derive(Deserialize)]
struct GrantAddBody {
    path: String,
    access: String,
}
async fn h_grant_global_add(State(api): State<ApiState>, Json(b): Json<GrantAddBody>) -> Response {
    json_result(commands::add_global_grant_core(api.ui.app_state(), b.path, b.access).await)
}
async fn h_grant_arc_revoke(State(api): State<ApiState>, UrlPath(id): UrlPath<i64>) -> Response {
    json_result(commands::revoke_arc_grant_core(api.ui.app_state(), id).await)
}
async fn h_grant_global_revoke(State(api): State<ApiState>, UrlPath(id): UrlPath<i64>) -> Response {
    json_result(commands::revoke_global_grant_core(api.ui.app_state(), id).await)
}

// registered HTTP endpoints (Cloud APIs)
async fn h_endpoints_list(State(api): State<ApiState>) -> Response {
    json_result(commands::list_http_endpoints_core(api.ui.app_state()).await)
}
async fn h_endpoint_upsert(
    State(api): State<ApiState>,
    Json(input): Json<commands::EndpointInput>,
) -> Response {
    json_result(commands::upsert_http_endpoint_core(input, api.ui.app_state()).await)
}
async fn h_endpoint_delete(State(api): State<ApiState>, UrlPath(id): UrlPath<String>) -> Response {
    json_result(commands::delete_http_endpoint_core(id, api.ui.app_state()).await)
}
async fn h_endpoint_enabled(
    State(api): State<ApiState>,
    UrlPath(id): UrlPath<String>,
    Json(b): Json<EnabledBody>,
) -> Response {
    json_result(commands::set_http_endpoint_enabled_core(id, b.enabled, api.ui.app_state()).await)
}
#[derive(Deserialize)]
struct EndpointTestBody {
    path: Option<String>,
}
async fn h_endpoint_test(
    State(api): State<ApiState>,
    UrlPath(id): UrlPath<String>,
    Json(b): Json<EndpointTestBody>,
) -> Response {
    json_result(commands::test_http_endpoint_core(id, b.path, api.ui.app_state()).await)
}
async fn h_endpoint_presets(State(_api): State<ApiState>) -> Response {
    json_result(commands::list_http_endpoint_presets().await)
}
async fn h_vault_smoke(State(api): State<ApiState>) -> Response {
    json_result(commands::vault_smoke_test_core(api.ui.app_state()).await)
}

// identity
async fn h_id_cats(State(api): State<ApiState>) -> Response {
    json_result(commands::list_identity_categories_core(api.ui.app_state()).await)
}
async fn h_id_cat_upsert(
    State(api): State<ApiState>,
    Json(input): Json<commands::IdentityCategoryInput>,
) -> Response {
    json_result(commands::upsert_identity_category_core(input, api.ui.app_state()).await)
}
async fn h_id_cat_delete(State(api): State<ApiState>, UrlPath(name): UrlPath<String>) -> Response {
    json_result(commands::delete_identity_category_core(name, api.ui.app_state()).await)
}
#[derive(Deserialize)]
struct CategoryQuery {
    category: Option<String>,
}
async fn h_id_entries(State(api): State<ApiState>, Query(q): Query<CategoryQuery>) -> Response {
    json_result(commands::list_identity_entries_core(q.category, api.ui.app_state()).await)
}
async fn h_id_entry_upsert(
    State(api): State<ApiState>,
    Json(input): Json<commands::IdentityEntryInput>,
) -> Response {
    json_result(commands::upsert_identity_entry_core(input, api.ui.app_state()).await)
}
async fn h_id_entry_delete(State(api): State<ApiState>, UrlPath(id): UrlPath<String>) -> Response {
    json_result(commands::delete_identity_entry_core(id, api.ui.app_state()).await)
}
async fn h_id_entry_dismiss(State(api): State<ApiState>, UrlPath(id): UrlPath<String>) -> Response {
    json_result(commands::dismiss_identity_entry_core(id, api.ui.app_state()).await)
}
async fn h_id_estimate(State(api): State<ApiState>) -> Response {
    json_result(commands::estimate_identity_total_core(api.ui.app_state()).await)
}

// projects
async fn h_projects_list(State(api): State<ApiState>) -> Response {
    json_result(commands::list_projects_core(api.ui.app_state()).await)
}
#[derive(Deserialize)]
struct ProjectCreateBody {
    name: String,
    instructions: Option<String>,
}
async fn h_project_create(
    State(api): State<ApiState>,
    Json(body): Json<ProjectCreateBody>,
) -> Response {
    json_result(
        commands::create_project_core(body.name, body.instructions, api.ui.app_state()).await,
    )
}
#[derive(Deserialize)]
struct ProjectUpdateBody {
    name: Option<String>,
    instructions: Option<String>,
    // When true, clears instructions to NULL. When false/absent and
    // `instructions` is absent, instructions are left untouched. When
    // `instructions` is present, it is set to that value.
    clear_instructions: Option<bool>,
}
async fn h_project_update(
    State(api): State<ApiState>,
    UrlPath(id): UrlPath<String>,
    Json(body): Json<ProjectUpdateBody>,
) -> Response {
    // Translate the flat body into the `Option<Option<String>>` the core fn wants:
    //   None              -> leave instructions untouched
    //   Some(None)        -> clear instructions
    //   Some(Some(value)) -> set instructions to value
    let instructions = if let Some(value) = body.instructions {
        Some(Some(value))
    } else if body.clear_instructions.unwrap_or(false) {
        Some(None)
    } else {
        None
    };
    json_result(
        commands::update_project_core(id, body.name, instructions, api.ui.app_state()).await,
    )
}
async fn h_project_delete(State(api): State<ApiState>, UrlPath(id): UrlPath<String>) -> Response {
    json_result(commands::delete_project_core(id, api.ui.app_state()).await)
}
async fn h_project_summary_update(
    State(api): State<ApiState>,
    UrlPath(id): UrlPath<String>,
) -> Response {
    json_result(commands::update_project_summary_core(id, api.ui.app_state()).await)
}
async fn h_arc_assign_project(
    State(api): State<ApiState>,
    UrlPath(id): UrlPath<String>,
    Json(body): Json<OptStringBody>,
) -> Response {
    json_result(commands::assign_arc_to_project_core(id, body.value, api.ui.app_state()).await)
}
async fn h_set_active_project(
    State(api): State<ApiState>,
    Json(body): Json<OptStringBody>,
) -> Response {
    json_result(commands::set_active_project_core(body.value, api.ui.app_state()).await)
}
#[derive(Deserialize)]
struct DeepResearchBody {
    question: String,
    depth: Option<String>,
    mode: Option<String>,
}
async fn h_deep_research(
    State(api): State<ApiState>,
    UrlPath(arc_id): UrlPath<String>,
    Json(body): Json<DeepResearchBody>,
) -> Response {
    let state = api.ui.app_state();
    json_result(
        commands::deep_research_core(
            arc_id,
            body.question,
            body.depth,
            body.mode,
            state,
            api.ui.clone(),
        )
        .await,
    )
}

async fn h_research_paper(
    State(api): State<ApiState>,
    UrlPath(arc_id): UrlPath<String>,
) -> Response {
    json_result(commands::get_research_paper_core(arc_id, api.ui.app_state()).await)
}
#[derive(Deserialize)]
struct ArcCodeModeBody {
    enabled: bool,
    root: Option<String>,
}
async fn h_arc_code_mode(
    State(api): State<ApiState>,
    UrlPath(arc_id): UrlPath<String>,
    Json(body): Json<ArcCodeModeBody>,
) -> Response {
    json_result(
        commands::set_arc_code_mode_core(arc_id, body.enabled, body.root, api.ui.app_state()).await,
    )
}
async fn h_code_mode_git(
    State(api): State<ApiState>,
    UrlPath(arc_id): UrlPath<String>,
) -> Response {
    json_result(commands::code_mode_git_state_core(&arc_id, api.ui.app_state()).await)
}
#[derive(Deserialize)]
struct CodeModeDiscardBody {
    path: Option<String>,
}
async fn h_code_mode_discard(
    State(api): State<ApiState>,
    UrlPath(arc_id): UrlPath<String>,
    Json(body): Json<CodeModeDiscardBody>,
) -> Response {
    json_result(commands::code_mode_discard_core(&arc_id, body.path, api.ui.app_state()).await)
}
async fn h_project_summary_mode_get(State(api): State<ApiState>) -> Response {
    json_result(commands::get_project_summary_mode_core(api.ui.app_state()).await)
}
#[derive(Deserialize)]
struct SummaryModeBody {
    mode: String,
}
async fn h_project_summary_mode_set(
    State(api): State<ApiState>,
    Json(body): Json<SummaryModeBody>,
) -> Response {
    json_result(commands::set_project_summary_mode_core(body.mode, api.ui.app_state()).await)
}
async fn h_project_files(State(api): State<ApiState>, UrlPath(id): UrlPath<String>) -> Response {
    json_result(commands::list_project_files_core(api.ui.app_state(), id).await)
}
async fn h_project_memories(State(api): State<ApiState>, UrlPath(id): UrlPath<String>) -> Response {
    json_result(commands::list_project_memories_core(api.ui.app_state(), id).await)
}

// skills
async fn h_skills_list(State(api): State<ApiState>) -> Response {
    json_result(commands::list_skills_core(api.ui.app_state()).await)
}
async fn h_skill_get(State(api): State<ApiState>, UrlPath(slug): UrlPath<String>) -> Response {
    json_result(commands::get_skill_core(slug, api.ui.app_state()).await)
}
async fn h_skill_upsert(
    State(api): State<ApiState>,
    Json(input): Json<commands::SkillInput>,
) -> Response {
    json_result(commands::upsert_skill_core(input, api.ui.app_state()).await)
}
async fn h_skill_delete(State(api): State<ApiState>, UrlPath(slug): UrlPath<String>) -> Response {
    json_result(commands::delete_skill_core(slug, api.ui.app_state()).await)
}
async fn h_skills_sync(State(api): State<ApiState>) -> Response {
    json_result(commands::sync_skills_core(api.ui.app_state()).await)
}
async fn h_skill_inject(State(api): State<ApiState>, UrlPath(slug): UrlPath<String>) -> Response {
    json_result(commands::inject_skill_core(slug, api.ui.app_state()).await)
}

// contacts
async fn h_contacts_list(State(api): State<ApiState>) -> Response {
    json_result(crate::contacts::list_contacts_core(api.ui.app_state()).await)
}
#[derive(Deserialize)]
struct ContactCreateBody {
    name: String,
    trust_level: Option<String>,
    #[serde(default)]
    identifiers: Vec<crate::contacts::IdentifierInput>,
    notes: Option<String>,
}
async fn h_contact_create(
    State(api): State<ApiState>,
    Json(b): Json<ContactCreateBody>,
) -> Response {
    json_result(
        crate::contacts::create_contact_core(
            api.ui.app_state(),
            b.name,
            b.trust_level,
            b.identifiers,
            b.notes,
        )
        .await,
    )
}
async fn h_contact_get(State(api): State<ApiState>, UrlPath(id): UrlPath<String>) -> Response {
    json_result(crate::contacts::get_contact_core(api.ui.app_state(), id).await)
}
#[derive(Deserialize)]
struct ContactUpdateBody {
    name: Option<String>,
    trust_level: Option<String>,
    identifiers: Option<Vec<crate::contacts::IdentifierInput>>,
    notes: Option<String>,
}
async fn h_contact_update(
    State(api): State<ApiState>,
    UrlPath(id): UrlPath<String>,
    Json(b): Json<ContactUpdateBody>,
) -> Response {
    json_result(
        crate::contacts::update_contact_core(
            api.ui.app_state(),
            id,
            b.name,
            b.trust_level,
            b.identifiers,
            b.notes,
        )
        .await,
    )
}
async fn h_contact_delete(State(api): State<ApiState>, UrlPath(id): UrlPath<String>) -> Response {
    json_result(crate::contacts::delete_contact_core(api.ui.app_state(), id).await)
}
#[derive(Deserialize)]
struct TrustBody {
    trust_level: String,
}
async fn h_contact_trust(
    State(api): State<ApiState>,
    UrlPath(id): UrlPath<String>,
    Json(b): Json<TrustBody>,
) -> Response {
    json_result(
        crate::contacts::set_contact_trust_core(api.ui.app_state(), id, b.trust_level).await,
    )
}
async fn h_contact_block(State(api): State<ApiState>, UrlPath(id): UrlPath<String>) -> Response {
    json_result(crate::contacts::block_contact_core(api.ui.app_state(), id).await)
}
async fn h_contact_unblock(State(api): State<ApiState>, UrlPath(id): UrlPath<String>) -> Response {
    json_result(crate::contacts::unblock_contact_core(api.ui.app_state(), id).await)
}
async fn h_owner_get(State(api): State<ApiState>) -> Response {
    json_result(crate::contacts::get_owner_contact_core(api.ui.app_state()).await)
}
#[derive(Deserialize)]
struct OwnerBody {
    name: String,
    #[serde(default)]
    identifiers: Vec<crate::contacts::IdentifierInput>,
}
async fn h_owner_save(State(api): State<ApiState>, Json(b): Json<OwnerBody>) -> Response {
    json_result(
        crate::contacts::save_owner_contact_core(api.ui.app_state(), b.name, b.identifiers).await,
    )
}
async fn h_owner_clear(State(api): State<ApiState>) -> Response {
    json_result(crate::contacts::clear_owner_contact_core(api.ui.app_state()).await)
}

// wake-ups
async fn h_wakeups_list(State(api): State<ApiState>) -> Response {
    json_result(crate::wakeup_commands::list_wakeups_core(api.ui.app_state()).await)
}
async fn h_wakeup_create(
    State(api): State<ApiState>,
    Json(req): Json<crate::wakeup_commands::CreateWakeupReq>,
) -> Response {
    json_result(crate::wakeup_commands::create_wakeup_core(req, api.ui.app_state()).await)
}
async fn h_wakeup_update(
    State(api): State<ApiState>,
    UrlPath(id): UrlPath<String>,
    Json(req): Json<crate::wakeup_commands::CreateWakeupReq>,
) -> Response {
    json_result(crate::wakeup_commands::update_wakeup_core(id, req, api.ui.app_state()).await)
}
async fn h_wakeup_delete(State(api): State<ApiState>, UrlPath(id): UrlPath<String>) -> Response {
    json_result(crate::wakeup_commands::delete_wakeup_core(id, api.ui.app_state()).await)
}
async fn h_wakeup_enabled(
    State(api): State<ApiState>,
    UrlPath(id): UrlPath<String>,
    Json(b): Json<EnabledBody>,
) -> Response {
    json_result(
        crate::wakeup_commands::set_wakeup_enabled_core(id, b.enabled, api.ui.app_state()).await,
    )
}
async fn h_wakeup_tools(State(api): State<ApiState>) -> Response {
    json_result(crate::wakeup_commands::list_available_tools_core(api.ui.app_state()).await)
}

// settings
async fn h_settings_get(State(api): State<ApiState>) -> Response {
    json_result(crate::settings::get_settings_core(api.ui.app_state()).await)
}
#[derive(Deserialize)]
struct SecurityBody {
    security_mode: String,
}
async fn h_settings_security(State(api): State<ApiState>, Json(b): Json<SecurityBody>) -> Response {
    json_result(crate::settings::save_settings_core(b.security_mode, api.ui.app_state()).await)
}
async fn h_provider_catalog(State(_api): State<ApiState>) -> Response {
    json_result(crate::settings::list_provider_catalog().await)
}
#[derive(Deserialize)]
struct ProviderIdQuery {
    provider_id: String,
}
async fn h_curated_models(
    State(_api): State<ApiState>,
    Query(q): Query<ProviderIdQuery>,
) -> Response {
    json_result(crate::settings::list_curated_models(q.provider_id).await)
}
async fn h_model_families(State(_api): State<ApiState>) -> Response {
    json_result(crate::settings::list_model_families().await)
}
#[derive(Deserialize)]
struct ProviderSaveBody {
    id: String,
    base_url: String,
    model: String,
    api_key: Option<String>,
    supports_vision: Option<bool>,
    supports_documents: Option<bool>,
    family: Option<String>,
    context_window_tokens: Option<u32>,
    compaction_trigger_pct: Option<u8>,
    compaction_target_pct: Option<u8>,
    temperature: Option<f32>,
    tier_models: Option<HashMap<String, String>>,
}
async fn h_provider_save(State(api): State<ApiState>, Json(b): Json<ProviderSaveBody>) -> Response {
    json_result(
        crate::settings::save_provider_core(
            b.id,
            b.base_url,
            b.model,
            b.api_key,
            b.supports_vision,
            b.supports_documents,
            b.family,
            b.context_window_tokens,
            b.compaction_trigger_pct,
            b.compaction_target_pct,
            b.temperature,
            b.tier_models,
            api.ui.app_state(),
        )
        .await,
    )
}
#[derive(Deserialize)]
struct ProviderTestBody {
    id: String,
    base_url: String,
    model: String,
    api_key: Option<String>,
}
async fn h_provider_test(State(api): State<ApiState>, Json(b): Json<ProviderTestBody>) -> Response {
    json_result(
        crate::settings::test_provider_core(
            b.id,
            b.base_url,
            b.model,
            b.api_key,
            api.ui.app_state(),
        )
        .await,
    )
}
async fn h_provider_delete(State(api): State<ApiState>, UrlPath(id): UrlPath<String>) -> Response {
    json_result(crate::settings::delete_provider_core(id, api.ui.app_state()).await)
}
async fn h_provider_activate(
    State(api): State<ApiState>,
    UrlPath(id): UrlPath<String>,
) -> Response {
    json_result(crate::settings::set_active_provider_core(id, api.ui.app_state()).await)
}

// bundles
async fn h_bundles_list(State(_api): State<ApiState>) -> Response {
    json_result(crate::bundle_settings::list_bundles().await)
}
async fn h_bundle_create(State(_api): State<ApiState>, Json(b): Json<NameBody>) -> Response {
    json_result(crate::bundle_settings::create_bundle(b.name).await)
}
#[derive(Deserialize)]
struct BundleUpdateBody {
    name: Option<String>,
    tiers: Option<crate::bundle_settings::BundleTiersView>,
}
async fn h_bundle_update(
    State(api): State<ApiState>,
    UrlPath(id): UrlPath<String>,
    Json(b): Json<BundleUpdateBody>,
) -> Response {
    json_result(
        crate::bundle_settings::update_bundle_core(id, b.name, b.tiers, api.ui.app_state()).await,
    )
}
async fn h_bundle_delete(State(_api): State<ApiState>, UrlPath(id): UrlPath<String>) -> Response {
    json_result(crate::bundle_settings::delete_bundle(id).await)
}
async fn h_bundle_activate(State(api): State<ApiState>, UrlPath(id): UrlPath<String>) -> Response {
    json_result(crate::bundle_settings::set_active_bundle_core(id, api.ui.app_state()).await)
}
#[derive(Deserialize)]
struct DuplicateBody {
    new_name: String,
}
async fn h_bundle_duplicate(
    State(_api): State<ApiState>,
    UrlPath(id): UrlPath<String>,
    Json(b): Json<DuplicateBody>,
) -> Response {
    json_result(crate::bundle_settings::duplicate_bundle(id, b.new_name).await)
}

// email / smtp / telegram
#[derive(Deserialize)]
struct EmailSaveBody {
    enabled: bool,
    imap_server: String,
    imap_port: u16,
    username: String,
    password: Option<String>,
    use_tls: bool,
    folders: String,
    poll_interval_secs: u64,
    lookback_hours: u32,
}
async fn h_email_save(State(api): State<ApiState>, Json(b): Json<EmailSaveBody>) -> Response {
    json_result(
        crate::settings::save_email_settings_core(
            b.enabled,
            b.imap_server,
            b.imap_port,
            b.username,
            b.password,
            b.use_tls,
            b.folders,
            b.poll_interval_secs,
            b.lookback_hours,
            api.ui.app_state(),
            &api.ui,
        )
        .await,
    )
}
#[derive(Deserialize)]
struct EmailTestBody {
    imap_server: String,
    imap_port: u16,
    username: String,
    password: String,
    use_tls: bool,
}
async fn h_email_test(State(_api): State<ApiState>, Json(b): Json<EmailTestBody>) -> Response {
    json_result(
        crate::settings::test_email_connection(
            b.imap_server,
            b.imap_port,
            b.username,
            b.password,
            b.use_tls,
        )
        .await,
    )
}
#[derive(Deserialize)]
struct EmailDetectBody {
    email: String,
}
async fn h_email_detect(State(_api): State<ApiState>, Json(b): Json<EmailDetectBody>) -> Response {
    json_result(commands::email_detect(b.email).await)
}
#[derive(Deserialize)]
struct SmtpSaveBody {
    smtp_server: String,
    smtp_port: u16,
    smtp_username: String,
    smtp_password: Option<String>,
    smtp_use_tls: bool,
    from_address: String,
}
async fn h_smtp_save(State(api): State<ApiState>, Json(b): Json<SmtpSaveBody>) -> Response {
    json_result(
        crate::settings::save_smtp_settings_core(
            b.smtp_server,
            b.smtp_port,
            b.smtp_username,
            b.smtp_password,
            b.smtp_use_tls,
            b.from_address,
            api.ui.app_state(),
        )
        .await,
    )
}
#[derive(Deserialize)]
struct SmtpTestBody {
    smtp_server: String,
    smtp_port: u16,
    smtp_username: String,
    smtp_password: String,
    smtp_use_tls: bool,
    from_address: String,
}
async fn h_smtp_test(State(_api): State<ApiState>, Json(b): Json<SmtpTestBody>) -> Response {
    json_result(
        crate::settings::test_smtp_connection(
            b.smtp_server,
            b.smtp_port,
            b.smtp_username,
            b.smtp_password,
            b.smtp_use_tls,
            b.from_address,
        )
        .await,
    )
}
#[derive(Deserialize)]
struct TelegramSaveBody {
    enabled: bool,
    bot_token: Option<String>,
    #[serde(default)]
    allowed_chat_ids: Vec<i64>,
    poll_interval_secs: Option<u64>,
}
async fn h_telegram_save(State(api): State<ApiState>, Json(b): Json<TelegramSaveBody>) -> Response {
    json_result(
        crate::settings::save_telegram_settings_core(
            b.enabled,
            b.bot_token,
            b.allowed_chat_ids,
            b.poll_interval_secs,
            api.ui.app_state(),
            &api.ui,
        )
        .await,
    )
}
#[derive(Deserialize)]
struct TelegramTestBody {
    bot_token: String,
}
async fn h_telegram_test(
    State(_api): State<ApiState>,
    Json(b): Json<TelegramTestBody>,
) -> Response {
    json_result(crate::settings::test_telegram_connection(b.bot_token).await)
}

// web search / attachments policy / notification prefs
#[derive(Deserialize)]
struct WebSearchSaveBody {
    brave_api_key: Option<String>,
    tavily_api_key: Option<String>,
}
async fn h_websearch_save(
    State(api): State<ApiState>,
    Json(b): Json<WebSearchSaveBody>,
) -> Response {
    json_result(
        crate::settings::save_web_search_settings_core(
            b.brave_api_key,
            b.tavily_api_key,
            api.ui.app_state(),
        )
        .await,
    )
}
#[derive(Deserialize)]
struct WebSearchTestBody {
    provider: String,
    api_key: String,
}
async fn h_websearch_test(
    State(_api): State<ApiState>,
    Json(b): Json<WebSearchTestBody>,
) -> Response {
    json_result(crate::settings::test_web_search_provider(b.provider, b.api_key).await)
}
async fn h_attach_policy_get(State(_api): State<ApiState>) -> Response {
    json_result(crate::settings::get_attachment_policy_settings().await)
}
#[derive(Deserialize)]
struct AttachPolicyBody {
    mime_bundles: Vec<String>,
    max_attachment_mb: u64,
    max_event_mb: u64,
    min_inline_trust: String,
    min_download_trust: String,
    byte_ttl_days: u32,
}
async fn h_attach_policy_save(
    State(_api): State<ApiState>,
    Json(b): Json<AttachPolicyBody>,
) -> Response {
    json_result(
        crate::settings::save_attachment_policy_settings(
            b.mime_bundles,
            b.max_attachment_mb,
            b.max_event_mb,
            b.min_inline_trust,
            b.min_download_trust,
            b.byte_ttl_days,
        )
        .await,
    )
}
async fn h_notif_settings_get(State(api): State<ApiState>) -> Response {
    json_result(crate::settings::get_notification_settings_core(api.ui.app_state()).await)
}
#[derive(Deserialize)]
struct NotifSettingsBody {
    preferred_channels: Vec<String>,
    escalation_timeout_secs: u64,
    quiet_hours_enabled: bool,
    quiet_start_hour: Option<u32>,
    quiet_start_minute: Option<u32>,
    quiet_end_hour: Option<u32>,
    quiet_end_minute: Option<u32>,
    quiet_allow_critical: Option<bool>,
}
async fn h_notif_settings_save(
    State(api): State<ApiState>,
    Json(b): Json<NotifSettingsBody>,
) -> Response {
    json_result(
        crate::settings::save_notification_settings_core(
            api.ui.app_state(),
            &api.ui,
            b.preferred_channels,
            b.escalation_timeout_secs,
            b.quiet_hours_enabled,
            b.quiet_start_hour,
            b.quiet_start_minute,
            b.quiet_end_hour,
            b.quiet_end_minute,
            b.quiet_allow_critical,
        )
        .await,
    )
}

// embeddings
#[derive(Deserialize)]
struct EmbeddingsSaveBody {
    mode: String,
    provider: Option<String>,
    model: Option<String>,
    base_url: Option<String>,
    api_key: Option<String>,
}
async fn h_embeddings_save(
    State(api): State<ApiState>,
    Json(b): Json<EmbeddingsSaveBody>,
) -> Response {
    json_result(
        crate::settings::save_embedding_settings_core(
            api.ui.app_state(),
            b.mode,
            b.provider,
            b.model,
            b.base_url,
            b.api_key,
        )
        .await,
    )
}
#[derive(Deserialize)]
struct EmbeddingsTestBody {
    provider: String,
    model: Option<String>,
    base_url: Option<String>,
    api_key: Option<String>,
}
async fn h_embeddings_test(
    State(api): State<ApiState>,
    Json(b): Json<EmbeddingsTestBody>,
) -> Response {
    json_result(
        crate::settings::test_embedding_provider_core(
            api.ui.app_state(),
            b.provider,
            b.model,
            b.base_url,
            b.api_key,
        )
        .await,
    )
}
async fn h_embeddings_bundled_status(State(api): State<ApiState>) -> Response {
    json_result(
        crate::bundled_embeddings::get_bundled_embedding_status_core(api.ui.app_state()).await,
    )
}
#[derive(Deserialize)]
struct BundledModeBody {
    tier: athen_core::config::BundledTier,
}
async fn h_embeddings_bundled_mode(
    State(api): State<ApiState>,
    Json(b): Json<BundledModeBody>,
) -> Response {
    json_result(
        crate::bundled_embeddings::set_embedding_mode_bundled_core(b.tier, api.ui.app_state())
            .await,
    )
}
async fn h_embeddings_recommend(State(_api): State<ApiState>) -> Response {
    json_result(crate::bundled_embeddings::recommend_embedding_tier().await)
}

// github identity
async fn h_github_get(State(api): State<ApiState>) -> Response {
    json_result(crate::settings::get_github_identities_core(api.ui.app_state()).await)
}
#[derive(Deserialize)]
struct GithubSaveBody {
    identity: String,
    token: Option<String>,
    user_name: String,
    user_email: String,
}
async fn h_github_save(State(api): State<ApiState>, Json(b): Json<GithubSaveBody>) -> Response {
    json_result(
        crate::settings::save_github_identity_core(
            b.identity,
            b.token,
            b.user_name,
            b.user_email,
            api.ui.app_state(),
        )
        .await,
    )
}
#[derive(Deserialize)]
struct TokenBody {
    token: String,
}
async fn h_github_test(State(_api): State<ApiState>, Json(b): Json<TokenBody>) -> Response {
    json_result(crate::settings::test_github_identity(b.token).await)
}

// calendar sources + prompt + default
async fn h_cal_sources(State(api): State<ApiState>) -> Response {
    json_result(crate::settings_calendar::list_calendar_sources_core(api.ui.app_state()).await)
}
#[derive(Deserialize)]
struct CalDavAddBody {
    display_name: String,
    base_url: String,
    username: String,
    password: String,
}
async fn h_cal_source_add(State(api): State<ApiState>, Json(b): Json<CalDavAddBody>) -> Response {
    json_result(
        crate::settings_calendar::add_caldav_source_core(
            b.display_name,
            b.base_url,
            b.username,
            b.password,
            api.ui.app_state(),
        )
        .await,
    )
}
async fn h_cal_source_delete(
    State(api): State<ApiState>,
    UrlPath(id): UrlPath<String>,
) -> Response {
    json_result(crate::settings_calendar::delete_calendar_source_core(id, api.ui.app_state()).await)
}
async fn h_cal_source_enabled(
    State(api): State<ApiState>,
    UrlPath(id): UrlPath<String>,
    Json(b): Json<EnabledBody>,
) -> Response {
    json_result(
        crate::settings_calendar::set_calendar_source_enabled_core(
            id,
            b.enabled,
            api.ui.app_state(),
        )
        .await,
    )
}
#[derive(Deserialize)]
struct CalendarIdsBody {
    calendar_ids: Vec<String>,
}
async fn h_cal_source_calendars(
    State(api): State<ApiState>,
    UrlPath(id): UrlPath<String>,
    Json(b): Json<CalendarIdsBody>,
) -> Response {
    json_result(
        crate::settings_calendar::set_calendar_source_selected_calendars_core(
            id,
            b.calendar_ids,
            api.ui.app_state(),
        )
        .await,
    )
}
async fn h_cal_source_test(State(api): State<ApiState>, UrlPath(id): UrlPath<String>) -> Response {
    json_result(
        crate::settings_calendar::test_calendar_source_connection_core(id, api.ui.app_state())
            .await,
    )
}
async fn h_cal_source_remote(
    State(api): State<ApiState>,
    UrlPath(id): UrlPath<String>,
) -> Response {
    json_result(crate::settings_calendar::list_remote_calendars_core(id, api.ui.app_state()).await)
}
async fn h_cal_source_sync(State(api): State<ApiState>, UrlPath(id): UrlPath<String>) -> Response {
    json_result(
        crate::settings_calendar::sync_calendar_source_now_core(id, api.ui.app_state(), &api.ui)
            .await,
    )
}
async fn h_cal_sync_all(State(api): State<ApiState>) -> Response {
    json_result(
        crate::settings_calendar::sync_all_calendar_sources_now_core(api.ui.app_state(), &api.ui)
            .await,
    )
}
async fn h_cal_writable(State(api): State<ApiState>) -> Response {
    json_result(crate::settings_calendar::list_writable_calendars_core(api.ui.app_state()).await)
}
async fn h_cal_prompt_get(State(api): State<ApiState>) -> Response {
    json_result(crate::settings::get_calendar_prompt_core(api.ui.app_state()).await)
}
#[derive(Deserialize)]
struct PromptBody {
    prompt: String,
}
async fn h_cal_prompt_save(State(api): State<ApiState>, Json(b): Json<PromptBody>) -> Response {
    json_result(crate::settings::save_calendar_prompt_core(api.ui.app_state(), b.prompt).await)
}
async fn h_cal_default_get(State(api): State<ApiState>) -> Response {
    json_result(crate::settings::get_agent_default_calendar_core(api.ui.app_state()).await)
}
#[derive(Deserialize)]
struct CalDefaultBody {
    source_id: Option<String>,
    calendar_id: Option<String>,
    calendar_name: Option<String>,
}
async fn h_cal_default_save(
    State(api): State<ApiState>,
    Json(b): Json<CalDefaultBody>,
) -> Response {
    json_result(
        crate::settings::save_agent_default_calendar_core(
            api.ui.app_state(),
            b.source_id,
            b.calendar_id,
            b.calendar_name,
        )
        .await,
    )
}

fn full_surface_router() -> Router<ApiState> {
    Router::new()
        // arcs extra
        .route("/api/arcs/{arc_id}/rename", post(h_arc_rename))
        .route("/api/arcs/{arc_id}/delete", post(h_arc_delete))
        .route("/api/arcs/{arc_id}/compact", post(h_arc_compact))
        .route("/api/arcs/branch", post(h_arc_branch))
        .route("/api/status", get(h_status))
        .route("/api/agent-runs", get(h_agent_runs))
        .route("/api/attachments/{event_id}", get(h_attachments_for_event))
        // goal + plan (active-arc scoped, like the desktop)
        .route(
            "/api/goal",
            get(h_goal_get).post(h_goal_set).delete(h_goal_clear),
        )
        .route("/api/plan", get(h_plan_get))
        .route("/api/plan/start", post(h_plan_start))
        .route("/api/plan/approve", post(h_plan_approve))
        .route("/api/plan/clear", post(h_plan_clear))
        .route("/api/plan/update", post(h_plan_update))
        // profiles + per-arc pickers
        .route("/api/profiles", get(h_profiles_list).post(h_profile_create))
        .route("/api/profiles/update", post(h_profile_update))
        .route("/api/profiles/{id}/delete", post(h_profile_delete))
        .route("/api/profiles/{id}/restore", post(h_profile_restore))
        .route("/api/profiles/{id}/tokens", get(h_profile_tokens))
        .route("/api/arcs/{arc_id}/profile", post(h_arc_set_profile))
        .route("/api/arcs/{arc_id}/effort", post(h_arc_set_effort))
        .route("/api/arcs/{arc_id}/tier", post(h_arc_set_tier))
        .route("/api/arcs/{arc_id}/security", post(h_arc_set_security))
        // checkpoints / changes rail
        .route("/api/arcs/{arc_id}/snapshots", get(h_arc_snapshots))
        .route("/api/snapshots/{action_id}/revert", post(h_snapshot_revert))
        .route("/api/arcs/{arc_id}/rewind", post(h_arc_rewind))
        // calendar events
        .route(
            "/api/calendar/events",
            get(h_cal_events).post(h_cal_event_create),
        )
        .route("/api/calendar/events/update", post(h_cal_event_update))
        .route("/api/calendar/events/{id}/delete", post(h_cal_event_delete))
        // notifications extra
        .route("/api/notifications/{id}/seen", post(h_notif_seen))
        .route("/api/notifications/{id}/delete", post(h_notif_delete))
        .route("/api/notifications/delete-read", post(h_notif_delete_read))
        // memory + knowledge graph
        .route("/api/memory", get(h_memory_list))
        .route("/api/memory/{id}", post(h_memory_update))
        .route("/api/memory/{id}/delete", post(h_memory_delete))
        .route("/api/entities", get(h_entities_list))
        .route("/api/entities/{id}", post(h_entity_update))
        .route("/api/entities/{id}/delete", post(h_entity_delete))
        .route("/api/relations", get(h_relations_list))
        .route("/api/relations/delete", post(h_relation_delete))
        // MCP
        .route("/api/mcp/catalog", get(h_mcp_catalog))
        .route("/api/mcp/custom", get(h_mcp_custom).post(h_mcp_add_custom))
        .route("/api/mcp/custom/{id}/remove", post(h_mcp_remove_custom))
        .route("/api/mcp/enabled", get(h_mcp_enabled))
        .route("/api/mcp/test-spawn", post(h_mcp_test_spawn))
        .route("/api/mcp/{id}/enable", post(h_mcp_enable))
        .route("/api/mcp/{id}/disable", post(h_mcp_disable))
        .route("/api/mcp/{id}/set-enabled", post(h_mcp_set_enabled))
        .route("/api/mcp/{id}/tools", get(h_mcp_tools_for))
        .route("/api/mcp/{id}/risks", post(h_mcp_set_risks))
        // directory grants
        .route("/api/grants/arc/{arc_id}", get(h_grants_arc))
        .route("/api/grants/arc/{id}/revoke", post(h_grant_arc_revoke))
        .route(
            "/api/grants/global",
            get(h_grants_global).post(h_grant_global_add),
        )
        .route(
            "/api/grants/global/{id}/revoke",
            post(h_grant_global_revoke),
        )
        // registered HTTP endpoints (Cloud APIs)
        .route(
            "/api/endpoints",
            get(h_endpoints_list).post(h_endpoint_upsert),
        )
        .route("/api/endpoints/presets", get(h_endpoint_presets))
        .route("/api/endpoints/{id}/delete", post(h_endpoint_delete))
        .route("/api/endpoints/{id}/enabled", post(h_endpoint_enabled))
        .route("/api/endpoints/{id}/test", post(h_endpoint_test))
        .route("/api/vault/smoke", get(h_vault_smoke))
        // identity
        .route(
            "/api/identity/categories",
            get(h_id_cats).post(h_id_cat_upsert),
        )
        .route(
            "/api/identity/categories/{name}/delete",
            post(h_id_cat_delete),
        )
        .route(
            "/api/identity/entries",
            get(h_id_entries).post(h_id_entry_upsert),
        )
        .route("/api/identity/entries/{id}/delete", post(h_id_entry_delete))
        .route(
            "/api/identity/entries/{id}/dismiss",
            post(h_id_entry_dismiss),
        )
        .route("/api/identity/estimate", get(h_id_estimate))
        // projects
        .route("/api/projects", get(h_projects_list).post(h_project_create))
        .route(
            "/api/projects/summary-mode",
            get(h_project_summary_mode_get).post(h_project_summary_mode_set),
        )
        .route("/api/projects/{id}", post(h_project_update))
        .route("/api/projects/{id}/delete", post(h_project_delete))
        .route("/api/projects/{id}/summary", post(h_project_summary_update))
        .route("/api/projects/{id}/files", get(h_project_files))
        .route("/api/projects/{id}/memories", get(h_project_memories))
        .route("/api/arcs/{id}/project", post(h_arc_assign_project))
        .route("/api/arcs/{id}/deep-research", post(h_deep_research))
        .route("/api/arcs/{id}/research-paper", get(h_research_paper))
        .route("/api/arcs/{id}/code-mode", post(h_arc_code_mode))
        .route("/api/arcs/{id}/code-mode/git", get(h_code_mode_git))
        .route(
            "/api/arcs/{id}/code-mode/discard",
            post(h_code_mode_discard),
        )
        .route("/api/active-project", post(h_set_active_project))
        // skills
        .route("/api/skills", get(h_skills_list).post(h_skill_upsert))
        .route("/api/skills/sync", post(h_skills_sync))
        .route("/api/skills/{slug}", get(h_skill_get))
        .route("/api/skills/{slug}/delete", post(h_skill_delete))
        .route("/api/skills/{slug}/inject", post(h_skill_inject))
        // contacts
        .route("/api/contacts", get(h_contacts_list).post(h_contact_create))
        .route("/api/contacts/owner", get(h_owner_get).post(h_owner_save))
        .route("/api/contacts/owner/clear", post(h_owner_clear))
        .route(
            "/api/contacts/{id}",
            get(h_contact_get).post(h_contact_update),
        )
        .route("/api/contacts/{id}/delete", post(h_contact_delete))
        .route("/api/contacts/{id}/trust", post(h_contact_trust))
        .route("/api/contacts/{id}/block", post(h_contact_block))
        .route("/api/contacts/{id}/unblock", post(h_contact_unblock))
        // wake-ups
        .route("/api/wakeups", get(h_wakeups_list).post(h_wakeup_create))
        .route("/api/wakeups/tools", get(h_wakeup_tools))
        .route("/api/wakeups/{id}", post(h_wakeup_update))
        .route("/api/wakeups/{id}/delete", post(h_wakeup_delete))
        .route("/api/wakeups/{id}/enabled", post(h_wakeup_enabled))
        // settings
        .route("/api/settings", get(h_settings_get))
        .route("/api/settings/security", post(h_settings_security))
        .route("/api/settings/provider-catalog", get(h_provider_catalog))
        .route("/api/settings/curated-models", get(h_curated_models))
        .route("/api/settings/model-families", get(h_model_families))
        .route("/api/settings/providers", post(h_provider_save))
        .route("/api/settings/providers/test", post(h_provider_test))
        .route(
            "/api/settings/providers/{id}/delete",
            post(h_provider_delete),
        )
        .route(
            "/api/settings/providers/{id}/activate",
            post(h_provider_activate),
        )
        .route(
            "/api/settings/bundles",
            get(h_bundles_list).post(h_bundle_create),
        )
        .route("/api/settings/bundles/{id}", post(h_bundle_update))
        .route("/api/settings/bundles/{id}/delete", post(h_bundle_delete))
        .route(
            "/api/settings/bundles/{id}/activate",
            post(h_bundle_activate),
        )
        .route(
            "/api/settings/bundles/{id}/duplicate",
            post(h_bundle_duplicate),
        )
        .route("/api/settings/email", post(h_email_save))
        .route("/api/settings/email/test", post(h_email_test))
        .route("/api/settings/email/detect", post(h_email_detect))
        .route("/api/settings/smtp", post(h_smtp_save))
        .route("/api/settings/smtp/test", post(h_smtp_test))
        .route("/api/settings/telegram", post(h_telegram_save))
        .route("/api/settings/telegram/test", post(h_telegram_test))
        .route("/api/settings/websearch", post(h_websearch_save))
        .route("/api/settings/websearch/test", post(h_websearch_test))
        .route(
            "/api/settings/attachments",
            get(h_attach_policy_get).post(h_attach_policy_save),
        )
        .route(
            "/api/settings/notifications",
            get(h_notif_settings_get).post(h_notif_settings_save),
        )
        .route("/api/settings/embeddings", post(h_embeddings_save))
        .route("/api/settings/embeddings/test", post(h_embeddings_test))
        .route(
            "/api/settings/embeddings/bundled-status",
            get(h_embeddings_bundled_status),
        )
        .route(
            "/api/settings/embeddings/bundled-mode",
            post(h_embeddings_bundled_mode),
        )
        .route(
            "/api/settings/embeddings/recommend",
            get(h_embeddings_recommend),
        )
        .route(
            "/api/settings/github",
            get(h_github_get).post(h_github_save),
        )
        .route("/api/settings/github/test", post(h_github_test))
        .route(
            "/api/settings/calendar-sources",
            get(h_cal_sources).post(h_cal_source_add),
        )
        .route(
            "/api/settings/calendar-sources/sync-all",
            post(h_cal_sync_all),
        )
        .route(
            "/api/settings/calendar-sources/writable",
            get(h_cal_writable),
        )
        .route(
            "/api/settings/calendar-sources/{id}/delete",
            post(h_cal_source_delete),
        )
        .route(
            "/api/settings/calendar-sources/{id}/enabled",
            post(h_cal_source_enabled),
        )
        .route(
            "/api/settings/calendar-sources/{id}/calendars",
            post(h_cal_source_calendars),
        )
        .route(
            "/api/settings/calendar-sources/{id}/test",
            post(h_cal_source_test),
        )
        .route(
            "/api/settings/calendar-sources/{id}/remote",
            get(h_cal_source_remote),
        )
        .route(
            "/api/settings/calendar-sources/{id}/sync",
            post(h_cal_source_sync),
        )
        .route(
            "/api/settings/calendar-prompt",
            get(h_cal_prompt_get).post(h_cal_prompt_save),
        )
        .route(
            "/api/settings/calendar-default",
            get(h_cal_default_get).post(h_cal_default_save),
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
