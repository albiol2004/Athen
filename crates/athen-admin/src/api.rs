//! Panel HTTP surface: UI assets, auth endpoints, panel REST, proxy.
//!
//! Route map:
//! - `GET /` + `/panel.css` + `/panel.js` — embedded UI (client decides
//!   login vs dashboard via `GET /panel/me`)
//! - `GET /healthz` — liveness, no auth
//! - `POST /panel/login` / `POST /panel/logout` / `GET /panel/me`
//! - everything else under `/panel/*` — session-gated panel REST
//! - `/i/{instance}/api/*` — session-gated reverse proxy to the instance

use std::sync::Arc;

use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{any, get, post};
use axum::{Extension, Json, Router};
use serde_json::json;

use crate::auth::{self, CurrentUser};
use crate::{proxy, ui, PanelState};

pub fn router(state: Arc<PanelState>) -> Router {
    // Session-gated panel API (admin checks happen per-handler).
    let panel = Router::new()
        .route("/panel/me", get(me))
        .route("/panel/logout", post(logout))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::require_session,
        ));

    // Session-gated proxy to instances (access check inside).
    let instances = Router::new()
        .route("/i/{instance}/api/{*path}", any(proxy::not_yet))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::require_session,
        ));

    Router::new()
        .route("/", get(ui::index))
        .route("/panel.css", get(ui::styles))
        .route("/panel.js", get(ui::script))
        .route("/healthz", get(health))
        .route("/panel/login", post(login))
        .merge(panel)
        .merge(instances)
        .layer(axum::extract::DefaultBodyLimit::max(32 * 1024 * 1024))
        .with_state(state)
}

async fn health() -> Json<serde_json::Value> {
    Json(json!({
        "status": "ok",
        "name": "athen-admin",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

#[derive(serde::Deserialize)]
struct LoginBody {
    username: String,
    password: String,
}

async fn login(
    State(state): State<Arc<PanelState>>,
    Json(body): Json<LoginBody>,
) -> impl IntoResponse {
    let user = match auth::user_by_name(&state.db, &body.username).await {
        Ok(Some(u)) => u,
        Ok(None) => {
            // Burn the same time as a real verify so login timing doesn't
            // reveal which usernames exist.
            let _ = auth::verify_password(body.password, DUMMY_HASH.to_string()).await;
            return err(StatusCode::UNAUTHORIZED, "invalid credentials");
        }
        Err(e) => return internal(e),
    };
    if !auth::verify_password(body.password, user.password_hash.clone()).await {
        return err(StatusCode::UNAUTHORIZED, "invalid credentials");
    }
    let session = match auth::new_session(&state.db, &user.id).await {
        Ok(s) => s,
        Err(e) => return internal(e),
    };
    (
        StatusCode::OK,
        [(header::SET_COOKIE, auth::set_session_cookie(&session))],
        Json(json!({ "username": user.username, "role": user.role })),
    )
        .into_response()
}

async fn logout(
    State(state): State<Arc<PanelState>>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    if let Some(sid) = auth::session_cookie_value(&headers) {
        let _ = auth::delete_session(&state.db, &sid).await;
    }
    (
        StatusCode::OK,
        [(header::SET_COOKIE, auth::clear_session_cookie())],
        Json(json!({ "ok": true })),
    )
        .into_response()
}

async fn me(Extension(CurrentUser(user)): Extension<CurrentUser>) -> Json<serde_json::Value> {
    Json(json!({ "id": user.id, "username": user.username, "role": user.role }))
}

/// A valid argon2 PHC string for a throwaway password; only used to
/// equalize login timing for unknown usernames.
const DUMMY_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$bm90LWEtcmVhbC1zYWx0$2x1zd8nB1WrLWLLpDLZ48qBLgzuTzzNS3ZpdEEsBuRI";

fn err(code: StatusCode, msg: &str) -> axum::response::Response {
    (code, Json(json!({ "error": msg }))).into_response()
}

fn internal(e: anyhow::Error) -> axum::response::Response {
    tracing::error!(error = %e, "panel internal error");
    err(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
}
