use axum::extract::State;
use axum::response::{Html, IntoResponse};

use crate::AppState;

pub async fn ui_handler(State(state): State<AppState>) -> impl IntoResponse {
    let prefix = if state.context_path == "/" {
        ""
    } else {
        state.context_path.as_str()
    };
    Html(HTML_CONTENT.replace("__CONTEXT_PATH__", prefix))
}

const HTML_CONTENT: &str = include_str!("index.html");
