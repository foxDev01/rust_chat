//! HTTP-обработчики вложений: загрузка файла вместе с сообщением и
//! авторизованное скачивание.
//!
//! Загрузка идёт отдельным HTTP-запросом, потому что WebSocket-кадры
//! ограничены 16 КиБ. После сохранения файла сообщение создаётся и
//! рассылается обычным путём (`state.publish` / `state.deliver_direct`),
//! поэтому клиент получает его как любое другое.

use axum::{
    Json,
    body::Body,
    extract::{Multipart, Path, State, multipart::MultipartRejection},
    http::{
        HeaderMap, HeaderValue, StatusCode,
        header::{CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_TYPE},
    },
    response::Response,
};
use serde_json::{Value, json};
use tokio_util::io::ReaderStream;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::{
    auth::{self, ApiError},
    protocol,
    state::ChatState,
    uploads::{content_disposition, normalize_content_type, sanitize_filename},
};

/// Запас сверх лимита файла на обвязку multipart-запроса.
pub(super) const MULTIPART_OVERHEAD: u64 = 256 * 1024;

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
pub(super) async fn upload_attachment(
    State(state): State<ChatState>,
    headers: HeaderMap,
    multipart: Result<Multipart, MultipartRejection>,
) -> Result<Json<Value>, ApiError> {
    let account = auth::account_from_headers(&state, &headers).await?;

    // Лимит проверяется до разбора формы: иначе поток загрузок от одного
    // аккаунта занимал бы память ещё до проверки размера.
    state
        .check_upload_rate(account.id)
        .await
        .map_err(|wait| auth::rate_limited(wait, "загрузки файлов"))?;

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

    // Квота папки общая для всех: файлы удаляются только вместе с чисткой
    // сирот, поэтому без предела каталог однажды займёт весь раздел.
    if !state.uploads().has_room_for(bytes.len() as u64).await {
        warn!(
            size = bytes.len(),
            "upload rejected: storage quota exceeded"
        );
        return Err(api_error(
            StatusCode::INSUFFICIENT_STORAGE,
            "На сервере закончилось место для файлов",
        ));
    }

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
pub(super) async fn download_attachment(
    State(state): State<ChatState>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let account = auth::account_from_headers(&state, &headers).await?;

    let Some(stored) = state.attachment(id, account.id).await else {
        return Err(api_error(StatusCode::NOT_FOUND, "Файл не найден"));
    };

    let file = match state.uploads().open(id).await {
        Ok(Some(file)) => file,
        Ok(None) => {
            warn!(message_id = %id, "attachment metadata without a file on disk");
            return Err(api_error(StatusCode::NOT_FOUND, "Файл не найден"));
        }
        Err(error) => {
            error!(%error, message_id = %id, "failed to open an attachment");
            return Err(api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Не удалось прочитать файл",
            ));
        }
    };

    // Файл отдаётся потоком: в памяти держится только текущий буфер, а не
    // всё вложение целиком, поэтому размер файла не влияет на память сервера.
    let mut response = Response::new(Body::from_stream(ReaderStream::new(file)));
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
    protocol::MessageText::parse(raw)
        .map(|text| text.into_string())
        .map_err(|reason| api_error(StatusCode::BAD_REQUEST, reason))
}

/// Короткий конструктор ошибки в общем формате API.
fn api_error(status: StatusCode, message: impl Into<String>) -> ApiError {
    auth::api_error(status, message)
}
