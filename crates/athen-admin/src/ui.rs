//! Embedded panel UI — plain HTML/CSS/JS (same convention as the Athen
//! desktop frontend), compiled into the binary so the panel ships as a
//! single file.

use axum::http::header;
use axum::response::{Html, IntoResponse};

pub async fn index() -> Html<&'static str> {
    Html(include_str!("../ui/index.html"))
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
