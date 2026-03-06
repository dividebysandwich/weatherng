use axum::response::{Html, IntoResponse};

pub async fn ui_handler() -> impl IntoResponse {
    Html(HTML_CONTENT)
}

const HTML_CONTENT: &str = include_str!("index.html");
