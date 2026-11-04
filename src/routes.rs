use std::path::PathBuf;

use axum::{Json, Router, extract::State, http::StatusCode, routing::get};
use serde_json::{Value, json};
use tower_http::{
    services::{ServeDir, ServeFile},
    trace::TraceLayer,
};

use crate::{state::ChatState, ws};

pub fn router(state: ChatState) -> Router {
    let dist = PathBuf::from("frontend/dist");
    let index = dist.join("index.html");

    // In production the server also serves the built SPA. If `frontend/dist`
    // does not exist (typical during development), requests simply get a 404.
    let static_files = ServeDir::new(dist).not_found_service(ServeFile::new(index));

    Router::new()
        .nest("/api", api_router())
        .route("/ws", get(ws::upgrade))
        .fallback_service(static_files)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

fn api_router() -> Router<ChatState> {
    Router::new()
        .route("/health", get(health))
        .fallback(api_not_found)
}

async fn health(State(state): State<ChatState>) -> Json<Value> {
    let storage = if state.has_storage() {
        "postgres"
    } else {
        "memory"
    };

    let mut body = json!({
        "status": "ok",
        "users": state.users().await.len(),
        "storage": storage,
    });

    if let Some((public, direct)) = state.storage_counts().await {
        body["messages"] = json!(public);
        body["direct_messages"] = json!(direct);
    }

    Json(body)
}

async fn api_not_found() -> (StatusCode, Json<Value>) {
    (StatusCode::NOT_FOUND, Json(json!({ "error": "not found" })))
}
