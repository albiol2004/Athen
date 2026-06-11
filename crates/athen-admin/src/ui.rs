//! Embedded panel UI — plain HTML/CSS/JS (same convention as the Athen
//! desktop frontend), compiled into the binary so the panel ships as a
//! single file.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::Extension;

use crate::auth::CurrentUser;
use crate::{auth, instances, PanelState};

pub async fn index() -> Html<&'static str> {
    Html(include_str!("../ui/index.html"))
}

/// Minimal chat client for one instance — the same session→proxy→SSE path
/// a future React / React Native app uses. Session-gated by the router;
/// access to the specific instance checked here.
pub async fn chat_page(
    State(state): State<Arc<PanelState>>,
    Extension(CurrentUser(user)): Extension<CurrentUser>,
    Path(instance_id): Path<String>,
) -> Response {
    match auth::user_can_access(&state.db, &user, &instance_id).await {
        Ok(true) => {}
        Ok(false) => return StatusCode::FORBIDDEN.into_response(),
        Err(e) => {
            tracing::error!(error = %e, "chat page access check failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    }
    match instances::get(&state.db, &instance_id).await {
        Ok(Some(_)) => Html(include_str!("../ui/chat.html")).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::error!(error = %e, "chat page instance lookup failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

pub async fn styles() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        include_str!("../ui/panel.css"),
    )
}

pub async fn script() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("../ui/panel.js"),
    )
}
