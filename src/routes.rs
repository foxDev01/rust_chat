//! HTTP-маршруты сервера: WebSocket, служебный API, загрузка и раздача
//! файлов, а также раздача собранного SPA.

use std::path::PathBuf;

use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, Multipart, Path, State, multipart::MultipartRejection},
    http::{
        HeaderMap, HeaderValue, StatusCode,
        header::{CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_TYPE},
    },
    response::Response,
    routing::{get, post},
};
use serde_json::{Value, json};
use tower_http::{
    services::{ServeDir, ServeFile},
    trace::TraceLayer,
};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::{
    auth, protocol,
    state::ChatState,
    uploads::{content_disposition, normalize_content_type, sanitize_filename},
    ws,
};

/// Запас сверх лимита файла на обвязку multipart-запроса.
const MULTIPART_OVERHEAD: u64 = 256 * 1024;

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
        .route("/ws", get(ws::upgrade))
        .fallback_service(static_files)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

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
        .route("/register", post(auth::register))
        .route("/login", post(auth::login))
        .route("/me", get(auth::me))
        .route("/logout", post(auth::logout))
        .route("/password", post(auth::change_password))
        .route("/upload", post(upload_attachment).layer(upload_limit))
        .route("/attachments/{id}", get(download_attachment))
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

/// `POST /api/upload` — сохраняет файл и сразу отправляет сообщение.
///
/// Поля формы:
/// - `file` — сам файл (обязательно);
/// - `room` — комната для публичного сообщения;
/// - `to` — имя получателя для личного сообщения;
/// - `text` — комментарий к файлу (необязательно).
///
/// Должно быть задано ровно одно из `room` и `to`. Сообщение создаётся тем же
/// способом, что и текстовые: сервер рассылает его по WebSocket, поэтому
/// клиенту достаточно дождаться обычного события.
async fn upload_attachment(
    State(state): State<ChatState>,
    headers: HeaderMap,
    multipart: Result<Multipart, MultipartRejection>,
) -> Result<Json<Value>, ApiError> {
    let account = auth::account_from_headers(&state, &headers).await?;

    let mut multipart = multipart.map_err(|rejection| {
        api_error(
            rejection.status(),
            if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
                "Файл слишком большой"
            } else {
                "Некорректная форма загрузки"
            },
        )
    })?;

    let max_bytes = state.max_upload_bytes();
    let mut file: Option<(String, String, Vec<u8>)> = None;
    let mut room: Option<String> = None;
    let mut to: Option<String> = None;
    let mut caption = String::new();

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|_| api_error(StatusCode::BAD_REQUEST, "Не удалось прочитать файл"))?
    {
        match field.name().unwrap_or_default() {
            "file" => {
                if file.is_some() {
                    return Err(api_error(StatusCode::BAD_REQUEST, "Файл указан дважды"));
                }

                let filename = field.file_name().map(str::to_owned);
                let content_type = field.content_type().map(str::to_owned);
                let bytes = field
                    .bytes()
                    .await
                    .map_err(|_| api_error(StatusCode::BAD_REQUEST, "Не удалось прочитать файл"))?;

                if bytes.is_empty() {
                    return Err(api_error(StatusCode::BAD_REQUEST, "Файл пустой"));
                }

                // Проверка размера ещё до записи на диск: лимит тела запроса
                // чуть выше, поэтому до этой строки доходят даже большие файлы.
                if bytes.len() as u64 > max_bytes {
                    return Err(api_error(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        "Файл слишком большой",
                    ));
                }

                file = Some((
                    sanitize_filename(filename.as_deref().unwrap_or("")),
                    normalize_content_type(content_type.as_deref()),
                    bytes.to_vec(),
                ));
            }
            "room" => room = Some(read_text_field(field).await?),
            "to" => to = Some(read_text_field(field).await?),
            "text" => caption = read_text_field(field).await?,
            // Неизвестные поля нужно вычитать, чтобы соединение не портилось.
            _ => {
                let _ = field.bytes().await;
            }
        }
    }

    let Some((name, mime, bytes)) = file else {
        return Err(api_error(StatusCode::BAD_REQUEST, "Файл не выбран"));
    };

    // Сообщение может быть и без комментария: файл сам по себе — это сообщение.
    let caption = if caption.trim().is_empty() {
        String::new()
    } else {
        protocol_text(&caption)?
    };

    let id = Uuid::new_v4();
    let sent_at = chrono::Utc::now();

    match (room, to) {
        (Some(room), None) => {
            if !state.is_room(&room).await {
                return Err(api_error(StatusCode::NOT_FOUND, "Такой комнаты нет"));
            }

            let message = protocol::ChatMessage {
                id,
                room,
                author: account.username.clone(),
                text: caption,
                sent_at,
                attachment: Some(protocol::Attachment {
                    id,
                    name: name.clone(),
                    mime: mime.clone(),
                    size: bytes.len() as i64,
                }),
            };

            // Файл появляется раньше сообщения: случай ACID-сбоя между ними
            // означает обычный потерянный файл на диске, а не битую ссылку.
            if state.uploads().save(id, &bytes).await.is_err() {
                error!(message_id = %id, "failed to save an attachment file");
                return Err(api_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Не удалось сохранить файл",
                ));
            }

            info!(message_id = %id, room = %message.room, size = bytes.len(), "attachment uploaded");
            state.publish(message, account.id).await;
        }
        (None, Some(to)) => {
            let recipient = protocol::UserName::parse(&to)
                .map_err(|reason| api_error(StatusCode::BAD_REQUEST, reason))?;

            // Правила те же, что и у текстовых личных сообщений: получатель
            // должен быть в сети, а отправлять себе нельзя.
            let connections = state.connections(recipient.as_str()).await;
            let Some(recipient_connection) = connections.first() else {
                return Err(api_error(StatusCode::BAD_REQUEST, "Пользователь не в сети"));
            };
            if recipient_connection.account_id == account.id {
                return Err(api_error(
                    StatusCode::BAD_REQUEST,
                    "Нельзя отправить личное сообщение самому себе",
                ));
            }

            let message = protocol::DirectMessage {
                id,
                from: account.username.clone(),
                to: recipient_connection.name.clone(),
                text: caption,
                sent_at,
                attachment: Some(protocol::Attachment {
                    id,
                    name: name.clone(),
                    mime: mime.clone(),
                    size: bytes.len() as i64,
                }),
            };

            if state.uploads().save(id, &bytes).await.is_err() {
                error!(message_id = %id, "failed to save an attachment file");
                return Err(api_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Не удалось сохранить файл",
                ));
            }

            // Доставка через общую логику `deliver_direct`: она и пишет в базу,
            // и раскладывает событие по окнам обоих участников.
            if state
                .deliver_direct(&message, account.id, recipient_connection.account_id)
                .await
                == 0
            {
                // Получатель исчез между проверкой и доставкой: файл остаётся
                // на диске, но сообщение в базу уже записано, как у текстовых.
                warn!(message_id = %id, "recipient disappeared while uploading");
                return Err(api_error(
                    StatusCode::BAD_REQUEST,
                    "Не удалось доставить сообщение",
                ));
            }

            info!(message_id = %id, size = bytes.len(), "direct attachment uploaded");
        }
        _ => {
            return Err(api_error(
                StatusCode::BAD_REQUEST,
                "Укажите комнату или получателя",
            ));
        }
    }

    // Клиенту достаточно знать, что загрузка удалась: само сообщение
    // придёт по WebSocket вместе с остальными.
    Ok(Json(json!({ "id": id, "name": name, "size": bytes.len() })))
}

/// `GET /api/attachments/{id}` — отдаёт файл сообщения его авторизованному читателю.
///
/// Ответ всегда помечается `Content-Disposition: attachment` и `nosniff`,
/// чтобы браузер не открыл HTML или скрипт напрямую из адресной строки.
async fn download_attachment(
    State(state): State<ChatState>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let account = auth::account_from_headers(&state, &headers).await?;

    let Some(stored) = state.attachment(id, account.id).await else {
        return Err(api_error(StatusCode::NOT_FOUND, "Файл не найден"));
    };

    let bytes = match state.uploads().read(id).await {
        Ok(Some(bytes)) => bytes,
        Ok(None) => {
            warn!(message_id = %id, "attachment metadata without a file on disk");
            return Err(api_error(StatusCode::NOT_FOUND, "Файл не найден"));
        }
        Err(error) => {
            error!(%error, message_id = %id, "failed to read an attachment");
            return Err(api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Не удалось прочитать файл",
            ));
        }
    };

    let mut response = Response::new(Body::from(bytes));
    let headers = response.headers_mut();

    // Значения уже проверены при загрузке (см. `normalize_content_type`),
    // поэтому аксессоры не дают неожиданностей.
    if let Ok(value) = HeaderValue::from_str(&stored.mime) {
        headers.insert(CONTENT_TYPE, value);
    }
    if let Ok(value) = HeaderValue::from_str(&content_disposition(&stored.name)) {
        headers.insert(CONTENT_DISPOSITION, value);
    }
    if let Ok(value) = HeaderValue::from_str(&stored.size.to_string()) {
        headers.insert(CONTENT_LENGTH, value);
    }

    // Файлы приватные: кэш браузера может их хранить, но не общий прокси.
    headers.insert(
        CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=3600"),
    );
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );

    debug!(message_id = %id, "attachment served");
    Ok(response)
}

/// Читает простое текстовое поле multipart.
async fn read_text_field(field: MultipartField<'_>) -> Result<String, ApiError> {
    field
        .text()
        .await
        .map_err(|_| api_error(StatusCode::BAD_REQUEST, "Не удалось прочитать форму"))
}

/// Тип поля multipart: заимствован из axum.
type MultipartField<'a> = axum::extract::multipart::Field<'a>;

/// Проверяет комментарий к файлу теми же правилами, что и текст сообщений.
fn protocol_text(raw: &str) -> Result<String, ApiError> {
    crate::protocol::MessageText::parse(raw)
        .map(|text| text.into_string())
        .map_err(|reason| api_error(StatusCode::BAD_REQUEST, reason))
}

/// Ошибка API — общий формат `{ "error": "..." }` из модуля `auth`.
type ApiError = auth::ApiError;

/// Короткий конструктор ошибки.
fn api_error(status: StatusCode, message: impl Into<String>) -> ApiError {
    auth::api_error(status, message)
}
