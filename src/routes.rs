//! HTTP-маршруты сервера: WebSocket, служебный API и раздача собранного SPA.

use std::path::PathBuf;

use axum::{Json, Router, extract::State, http::StatusCode, routing::get};
use serde_json::{Value, json};
use tower_http::{
    services::{ServeDir, ServeFile},
    trace::TraceLayer,
};

use crate::{state::ChatState, ws};

/// Собирает корневой роутер приложения.
///
/// - `/ws` — WebSocket для чата;
/// - `/api/*` — служебные HTTP-эндпоинты;
/// - всё остальное — статика из `frontend/dist` (если она собрана).
pub fn router(state: ChatState) -> Router {
    let dist = PathBuf::from("frontend/dist");
    let index = dist.join("index.html");

    // В продакшене сервер сам раздаёт собранный SPA. Если папки `frontend/dist`
    // нет (обычная ситуация при разработке), запросы просто получат 404.
    let static_files = ServeDir::new(dist).not_found_service(ServeFile::new(index));

    Router::new()
        .nest("/api", api_router())
        .route("/ws", get(ws::upgrade))
        .fallback_service(static_files)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// Роутер служебного API.
fn api_router() -> Router<ChatState> {
    Router::new()
        .route("/health", get(health))
        .fallback(api_not_found)
}

/// `GET /api/health` — состояние сервера, число пользователей и счётчики сообщений.
async fn health(State(state): State<ChatState>) -> Json<Value> {
    // Показываем, используется ли PostgreSQL или состояние живёт только в памяти.
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

    // Счётчики строк есть только при подключённой базе.
    if let Some((public, direct)) = state.storage_counts().await {
        body["messages"] = json!(public);
        body["direct_messages"] = json!(direct);
    }

    Json(body)
}

/// Ответ на неизвестный маршрут внутри `/api`.
async fn api_not_found() -> (StatusCode, Json<Value>) {
    (StatusCode::NOT_FOUND, Json(json!({ "error": "not found" })))
}
