//! Reverse proxy `/i/{instance}/api/*` → the instance's HTTP API, and
//! `/i/{instance}/` → the instance's embedded web UI (static shell +
//! SPA routes), so admins get the full client — Settings modal included —
//! without ever holding the instance token.
//!
//! The browser/mobile client authenticates to the panel with its session
//! cookie; the proxy swaps that for the instance's bearer token, which
//! never leaves the panel. This is also what makes `EventSource` work for
//! React clients: it can't set an `Authorization` header, but same-origin
//! cookies ride along automatically.
//!
//! Streaming is end-to-end: request and response bodies are forwarded as
//! byte streams with no buffering, so SSE (`/api/events`, log tails) and
//! the long-poll `POST /api/messages` behave exactly as they do when
//! talking to the instance directly. The proxy's reqwest client is built
//! without timeouts for the same reason.
//!
//! The instance target is resolved per request (container IP on the panel
//! network, via the Docker API): IPs change across container restarts and
//! Docker DNS names don't resolve from the host. One inspect per request
//! is ~1ms on the unix socket — fine at panel scale.

use std::sync::Arc;

use axum::extract::{Path, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{body::Body, Extension, Json};
use serde_json::json;

use crate::auth::{self, CurrentUser};
use crate::{instances, PanelState};

/// Hop-by-hop + auth headers that must not cross the proxy in either
/// direction. `cookie`/`authorization` carry panel credentials inbound;
/// the rest confuse hyper's framing on the way out.
const STRIP: &[header::HeaderName] = &[
    header::HOST,
    header::COOKIE,
    header::AUTHORIZATION,
    header::CONNECTION,
    header::TRANSFER_ENCODING,
    header::CONTENT_LENGTH,
    header::UPGRADE,
];

/// `/i/{instance}/api/{*path}` — the instance HTTP API.
pub async fn handle(
    State(state): State<Arc<PanelState>>,
    Extension(CurrentUser(user)): Extension<CurrentUser>,
    Path((instance_id, path)): Path<(String, String)>,
    req: Request,
) -> Response {
    forward(state, user, instance_id, format!("api/{path}"), req).await
}

/// `/i/{instance}/{*path}` — the instance's embedded web UI (static
/// assets + SPA routes). The instance serves these without a token, but
/// the bearer is injected anyway so the path split stays in one place.
pub async fn handle_ui(
    State(state): State<Arc<PanelState>>,
    Extension(CurrentUser(user)): Extension<CurrentUser>,
    Path((instance_id, path)): Path<(String, String)>,
    req: Request,
) -> Response {
    forward(state, user, instance_id, path, req).await
}

/// `/i/{instance}/` — the web UI shell (index.html).
pub async fn handle_ui_root(
    State(state): State<Arc<PanelState>>,
    Extension(CurrentUser(user)): Extension<CurrentUser>,
    Path(instance_id): Path<String>,
    req: Request,
) -> Response {
    forward(state, user, instance_id, String::new(), req).await
}

async fn forward(
    state: Arc<PanelState>,
    user: crate::db::User,
    instance_id: String,
    upstream_path: String,
    req: Request,
) -> Response {
    // Access: admins reach every instance, users need a grant. 404 (not
    // 403) for unknown ids so users can't enumerate instance ids.
    let allowed = match auth::user_can_access(&state.db, &user, &instance_id).await {
        Ok(a) => a,
        Err(e) => return internal(e),
    };
    if !allowed {
        return error_response(StatusCode::FORBIDDEN, "no access to this instance");
    }
    let instance = match instances::get(&state.db, &instance_id).await {
        Ok(Some(i)) => i,
        Ok(None) => return error_response(StatusCode::NOT_FOUND, "no such instance"),
        Err(e) => return internal(e),
    };

    let ip = match state
        .docker
        .instance_ip(&instance.container_name, &state.cfg.network)
        .await
    {
        Ok(ip) => ip,
        Err(e) => {
            tracing::debug!(error = %e, instance = %instance.name, "proxy target unreachable");
            return error_response(
                StatusCode::BAD_GATEWAY,
                "instance is not running (no address on the panel network)",
            );
        }
    };

    let query = req
        .uri()
        .query()
        .map(|q| format!("?{q}"))
        .unwrap_or_default();
    let url = format!(
        "http://{ip}:{}/{upstream_path}{query}",
        instances::INSTANCE_PORT
    );

    let method = match reqwest::Method::from_bytes(req.method().as_str().as_bytes()) {
        Ok(m) => m,
        Err(_) => return error_response(StatusCode::METHOD_NOT_ALLOWED, "unsupported method"),
    };

    // Forward headers minus the strip list, then inject the instance token.
    let mut headers = reqwest::header::HeaderMap::new();
    for (name, value) in req.headers() {
        if STRIP.contains(name) {
            continue;
        }
        if let (Ok(n), Ok(v)) = (
            reqwest::header::HeaderName::from_bytes(name.as_str().as_bytes()),
            reqwest::header::HeaderValue::from_bytes(value.as_bytes()),
        ) {
            headers.insert(n, v);
        }
    }
    let bearer = format!("Bearer {}", instance.http_token);
    match reqwest::header::HeaderValue::from_str(&bearer) {
        Ok(mut v) => {
            v.set_sensitive(true);
            headers.insert(reqwest::header::AUTHORIZATION, v);
        }
        Err(e) => return internal(e.into()),
    }

    let body_stream = req.into_body().into_data_stream();
    let upstream = state
        .http
        .request(method, &url)
        .headers(headers)
        .body(reqwest::Body::wrap_stream(body_stream))
        .send()
        .await;

    let upstream = match upstream {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, instance = %instance.name, "proxy upstream error");
            return error_response(StatusCode::BAD_GATEWAY, "instance did not respond");
        }
    };

    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut out_headers = HeaderMap::new();
    for (name, value) in upstream.headers() {
        let Ok(n) = header::HeaderName::from_bytes(name.as_str().as_bytes()) else {
            continue;
        };
        if STRIP.contains(&n) {
            continue;
        }
        if let Ok(v) = HeaderValue::from_bytes(value.as_bytes()) {
            out_headers.insert(n, v);
        }
    }

    let mut resp = Response::builder().status(status);
    if let Some(h) = resp.headers_mut() {
        *h = out_headers;
    }
    resp.body(Body::from_stream(upstream.bytes_stream()))
        .unwrap_or_else(|e| internal(e.into()))
}

fn error_response(code: StatusCode, msg: &str) -> Response {
    (code, Json(json!({ "error": msg }))).into_response()
}

fn internal(e: anyhow::Error) -> Response {
    tracing::error!(error = %e, "proxy internal error");
    error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
}
