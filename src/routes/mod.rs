//! HTTP-маршруты сервера: WebSocket, служебный API, загрузка и раздача
//! файлов, а также раздача собранного SPA.
//!
//! Сам роутер остаётся точкой сборки, а обработчики вложений живут в
//! [`attachments`], чтобы файл с загрузкой не разрастался вместе с API.

use std::path::PathBuf;

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, Request, StatusCode},
    routing::{get, post},
};
use serde_json::{Value, json};
use tower_http::{
    services::{ServeDir, ServeFile},
    trace::{MakeSpan, TraceLayer},
};
use tracing::Span;

use crate::{auth, state::ChatState};

mod attachments;

use self::attachments::{MULTIPART_OVERHEAD, download_attachment, upload_attachment};

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
        .nest("/api", api_router(&state))
        .route("/ws", get(crate::ws::upgrade))
        .fallback_service(static_files)
        .layer(
            TraceLayer::new_for_http()
                // В `/ws?ticket=...` лежит одноразовый билет, но query-строку
                // не пишем даже на уровне debug: билет ещё может быть живым.
                .make_span_with(RequestPathSpan),
        )
        .with_state(state)
}

/// Создаёт span запроса из одного пути, без query-строки.
#[derive(Clone, Copy, Debug)]
struct RequestPathSpan;

impl<B> MakeSpan<B> for RequestPathSpan {
    fn make_span(&mut self, request: &Request<B>) -> Span {
        tracing::debug_span!(
            "request",
            method = %request.method(),
            path = %request.uri().path(),
        )
    }
}

/// Служебные JSON-тела входа и регистрации не бывают больше пары килобайт.
const AUTH_BODY_LIMIT: usize = 8 * 1024;

/// Роутер служебного API и аутентификации.
///
/// Лимит тела запроса берётся из состояния: широко разрешать нельзя,
/// иначе в память попадут произвольно большие файлы.
fn api_router(state: &ChatState) -> Router<ChatState> {
    // Ограничение тела ставится ровно на маршрут загрузки: rest API больше
    // пары килобайт не требует.
    let upload_limit = DefaultBodyLimit::max(
        usize::try_from(state.max_upload_bytes().saturating_add(MULTIPART_OVERHEAD))
            .unwrap_or(usize::MAX),
    );

    Router::new()
        .route("/health", get(health))
        .route(
            "/register",
            post(crate::auth::register).layer(DefaultBodyLimit::max(AUTH_BODY_LIMIT)),
        )
        .route(
            "/login",
            post(crate::auth::login).layer(DefaultBodyLimit::max(AUTH_BODY_LIMIT)),
        )
        .route("/me", get(crate::auth::me))
        .route("/ws-ticket", post(crate::auth::ws_ticket))
        .route("/logout", post(crate::auth::logout))
        .route(
            "/password",
            post(crate::auth::change_password).layer(DefaultBodyLimit::max(AUTH_BODY_LIMIT)),
        )
        .route("/upload", post(upload_attachment).layer(upload_limit))
        .route("/attachments/{id}", get(download_attachment))
        .fallback(api_not_found)
}

/// `GET /api/health` — состояние сервера, счётчики и занятость ресурсов.
///
/// Без действующей сессии отдаётся только `status`: число участников, размер
/// вложений и статистика пула БД — служебные данные, которым не место в
/// публичном ответе. Подробности доступны с `Authorization: Bearer <token>`.
async fn health(State(state): State<ChatState>, headers: HeaderMap) -> Json<Value> {
    if auth::account_from_headers(&state, &headers).await.is_err() {
        return Json(json!({ "status": "ok" }));
    }

    // Показываем, используется ли PostgreSQL или состояние живёт только в памяти.
    let storage = if state.has_storage() {
        "postgres"
    } else {
        "memory"
    };

    let mut body = json!({
        "status": "ok",
        "users": state.users().await.len(),
        "windows": state.window_count().await,
        "rooms": state.rooms().await.len(),
        "uptime_seconds": state.uptime_seconds(),
        "storage": storage,
        "attachments_bytes": state.uploads_bytes().await,
        "attachments_quota_bytes": state.uploads().quota_bytes(),
    });

    // Счётчики строк есть только при подключённой базе.
    if let Some((public, direct)) = state.storage_counts().await {
        body["messages"] = json!(public);
        body["direct_messages"] = json!(direct);
    }

    if let Some((size, idle)) = state.pool_stats() {
        body["db_pool"] = json!({ "size": size, "idle": idle });
    }

    Json(body)
}

/// Ответ на неизвестный маршрут внутри `/api`.
async fn api_not_found() -> (StatusCode, Json<Value>) {
    (StatusCode::NOT_FOUND, Json(json!({ "error": "not found" })))
}

// Проверяем HTTP-слой целиком: маршруты, коды ответов и формат ошибок.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        db::{ChatDb, DB_TEST_LOCK},
        protocol::UserName,
        uploads::Uploads,
    };
    use axum::{
        body::{Body, to_bytes},
        extract::ConnectInfo,
        http::{
            Request,
            header::{AUTHORIZATION, CONTENT_DISPOSITION, CONTENT_TYPE, RETRY_AFTER},
        },
    };
    use serde_json::{Value, json};
    use std::net::SocketAddr;
    use std::time::{Duration, SystemTime};
    use tower::ServiceExt;
    use uuid::Uuid;

    /// Отправляет запрос в собранный роутер и возвращает ответ.
    async fn call(app: Router, request: Request<Body>) -> axum::response::Response {
        app.oneshot(request).await.expect("router всегда отвечает")
    }

    #[tokio::test]
    async fn health_hides_details_without_a_session() {
        let app = router(ChatState::new());

        let response = call(
            app,
            Request::builder()
                .uri("/api/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 16 * 1024).await.unwrap();
        let payload: Value = serde_json::from_slice(&body).unwrap();

        // Публичный ответ ограничен признаком «сервер жив».
        assert_eq!(payload["status"], "ok");
        assert!(payload.get("storage").is_none());
        assert!(payload.get("users").is_none());
        assert!(payload.get("windows").is_none());
        assert!(payload.get("uptime_seconds").is_none());
        assert!(payload.get("attachments_bytes").is_none());
        assert!(payload.get("db_pool").is_none());
    }

    #[tokio::test]
    async fn upload_requires_a_session() {
        let app = router(ChatState::new());

        let response = call(
            app,
            Request::builder()
                .method("POST")
                .uri("/api/upload")
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        // Проверка токена идёт раньше разбора формы.
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn ws_ticket_requires_a_session() {
        let app = router(ChatState::new());

        let response = call(
            app,
            Request::builder()
                .method("POST")
                .uri("/api/ws-ticket")
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        // Билет выдаётся только владельцу действующей сессии.
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn download_requires_a_session() {
        let app = router(ChatState::new());
        let id = Uuid::new_v4();

        let response = call(
            app,
            Request::builder()
                .uri(format!("/api/attachments/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    /// Собирает POST-запрос с JSON-телом и адресом клиента для `ConnectInfo`.
    fn json_post(uri: &str, body: String) -> Request<Body> {
        let mut request = Request::builder()
            .method("POST")
            .uri(uri)
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap();

        request
            .extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 5000))));
        request
    }

    #[tokio::test]
    async fn login_rejects_malformed_username_without_storage() {
        let app = router(ChatState::new());
        let body = json!({
            "username": "bad\nname",
            "password": "пароль-123",
        })
        .to_string();

        let response = call(app, json_post("/api/login", body)).await;

        // Некорректная форма не доходит ни до базы, ни до общего лимитера.
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn login_rejects_oversized_payloads() {
        let app = router(ChatState::new());
        let body = json!({
            "username": "я".repeat(1024 * 1024),
            "password": "пароль-123",
        })
        .to_string();

        let response = call(app, json_post("/api/login", body)).await;

        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn unknown_api_route_returns_json_404() {
        let app = router(ChatState::new());

        let response = call(
            app,
            Request::builder()
                .uri("/api/does-not-exist")
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = to_bytes(response.into_body(), 4096).await.unwrap();
        let payload: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(payload["error"], "not found");
    }

    /// Тело multipart-запроса для проверки загрузки.
    fn multipart_body(
        boundary: &str,
        room: &str,
        caption: &str,
        filename: &str,
        bytes: &[u8],
    ) -> Vec<u8> {
        let mut body = Vec::new();

        for (name, value) in [("room", room), ("text", caption)] {
            body.extend_from_slice(
                format!(
                    "--{boundary}\r\n\
                     Content-Disposition: form-data; name=\"{name}\"\r\n\r\n\
                     {value}\r\n"
                )
                .as_bytes(),
            );
        }

        body.extend_from_slice(
            format!(
                "--{boundary}\r\n\
                 Content-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\n\
                 Content-Type: text/plain\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(bytes);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        body
    }

    // Полный сценарий требует PostgreSQL:
    //   $env:CHAT_DB_URL = "postgres://<user>:<password>@localhost:5432/rust_chat_test"
    //   cargo test -- --ignored
    #[tokio::test]
    #[ignore]
    async fn uploads_and_downloads_a_file_over_http() {
        let _guard = DB_TEST_LOCK.lock().await;
        let url = std::env::var("CHAT_DB_URL").expect("CHAT_DB_URL must be set");
        let db = ChatDb::connect(&url).await.expect("connect to PostgreSQL");
        db.truncate().await.expect("truncate");

        // Отдельная папка, чтобы тест не трогал настоящие загрузки.
        let dir = std::env::temp_dir().join(format!("rust-chat-http-{}", Uuid::new_v4()));
        let uploads = Uploads::prepare(dir.clone(), 1024 * 1024)
            .await
            .expect("prepare uploads");
        let state = ChatState::persisted(db, uploads)
            .await
            .expect("restore state");

        let alice = state
            .register(&UserName::parse("ФайловТест").unwrap(), "пароль-123")
            .await
            .expect("register alice");

        let app = router(state.clone());
        let boundary = "----rust-chat-test";
        let contents = b"hello from the http test";

        let response = call(
            app.clone(),
            Request::builder()
                .method("POST")
                .uri("/api/upload")
                .header(AUTHORIZATION, format!("Bearer {}", alice.token))
                .header(
                    CONTENT_TYPE,
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .body(Body::from(multipart_body(
                    boundary,
                    "general",
                    "файл из теста",
                    "тест.txt",
                    contents,
                )))
                .unwrap(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 16 * 1024).await.unwrap();
        let payload: Value = serde_json::from_slice(&body).unwrap();
        let id = Uuid::parse_str(payload["id"].as_str().unwrap()).unwrap();

        // Байты легли на диск под UUID сообщения.
        assert_eq!(
            tokio::fs::read(dir.join(id.to_string())).await.unwrap(),
            contents
        );

        // Участник комнаты скачивает файл и получает безопасные заголовки.
        let response = call(
            app.clone(),
            Request::builder()
                .uri(format!("/api/attachments/{id}"))
                .header(AUTHORIZATION, format!("Bearer {}", alice.token))
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let disposition = response.headers()[CONTENT_DISPOSITION]
            .to_str()
            .unwrap()
            .to_owned();
        assert!(disposition.starts_with("attachment;"));
        assert!(disposition.contains("filename*=UTF-8''"));
        assert_eq!(
            to_bytes(response.into_body(), 64 * 1024).await.unwrap(),
            contents.as_slice()
        );

        // Несуществующий (или чужой) идентификатор неотличим от запрещённого.
        let response = call(
            app.clone(),
            Request::builder()
                .uri(format!("/api/attachments/{}", Uuid::new_v4()))
                .header(AUTHORIZATION, format!("Bearer {}", alice.token))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        // Лимит загрузок: одна загрузка уже прошла, ещё девять укладываются
        // в десятку за минуту, а одиннадцатая получает `429` и `Retry-After`.
        for index in 0..9 {
            let response = call(
                app.clone(),
                Request::builder()
                    .method("POST")
                    .uri("/api/upload")
                    .header(AUTHORIZATION, format!("Bearer {}", alice.token))
                    .header(
                        CONTENT_TYPE,
                        format!("multipart/form-data; boundary={boundary}"),
                    )
                    .body(Body::from(multipart_body(
                        boundary,
                        "general",
                        &format!("лимит {index}"),
                        "limit.txt",
                        contents,
                    )))
                    .unwrap(),
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);
        }

        let response = call(
            app,
            Request::builder()
                .method("POST")
                .uri("/api/upload")
                .header(AUTHORIZATION, format!("Bearer {}", alice.token))
                .header(
                    CONTENT_TYPE,
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .body(Body::from(multipart_body(
                    boundary,
                    "general",
                    "лимит",
                    "limit.txt",
                    contents,
                )))
                .unwrap(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(response.headers().contains_key(RETRY_AFTER));

        // Файл без сообщения — сирота: чистка удаляет его, но только если он
        // пролежал дольше запаса, отведённого незавершённой загрузке.
        let orphan = Uuid::new_v4();
        let orphan_path = dir.join(orphan.to_string());
        tokio::fs::write(&orphan_path, b"orphan")
            .await
            .expect("write an orphan");
        std::fs::OpenOptions::new()
            .write(true)
            .open(&orphan_path)
            .expect("open an orphan")
            .set_modified(SystemTime::now() - Duration::from_secs(2 * 60 * 60))
            .expect("age the orphan");

        let fresh = Uuid::new_v4();
        let fresh_path = dir.join(fresh.to_string());
        tokio::fs::write(&fresh_path, b"fresh")
            .await
            .expect("write a fresh file");

        assert_eq!(state.cleanup_orphans().await, 1);
        assert!(!orphan_path.exists());
        // Свежий файл мог ещё не попасть в базу, а файл существующего
        // сообщения нужен пользователю: оба остаются на месте.
        assert!(fresh_path.exists());
        assert!(dir.join(id.to_string()).exists());

        let _ = tokio::fs::remove_file(&fresh_path).await;
        state.close_storage().await;
        let db = ChatDb::connect(&url).await.expect("reconnect for cleanup");
        db.truncate().await.expect("truncate");
        db.close().await;
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
