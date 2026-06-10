//! Reverse proxy `/i/{instance}/api/*` → instance HTTP API.
//!
//! Placeholder — implemented in the proxy task.

use axum::http::StatusCode;

pub async fn not_yet() -> StatusCode {
    StatusCode::NOT_IMPLEMENTED
}
