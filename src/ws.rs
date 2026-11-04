//! Обработка WebSocket-соединений: вход по токену сессии, сообщения,
//! личные переписки и heartbeat.

use std::sync::OnceLock;

use axum::{
    Json,
    extract::{
        Query, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{
        HeaderMap, StatusCode,
        header::{HOST, ORIGIN},
    },
    response::Response,
};
use chrono::Utc;
use futures_util::{SinkExt, StreamExt, stream::SplitSink};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{
    sync::{broadcast::error::RecvError, mpsc},
    time::{self, Duration},
};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::{
    auth::Account,
    protocol::{ChatMessage, ClientMessage, DirectMessage, MessageText, ServerMessage, UserName},
    state::{ChatState, PERSONAL_QUEUE},
};

/// Сообщение браузера — не больше 2 000 символов плюс JSON-обвязка и многобайтовый текст.
const MAX_FRAME_SIZE: usize = 16 * 1024;
/// Как часто отправляем ping, чтобы заметить мёртвое соединение.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

type SocketSink = SplitSink<WebSocket, Message>;

/// Параметры подключения: браузер не умеет задавать заголовки для WebSocket,
/// поэтому токен сессии передаётся в строке запроса.
#[derive(Debug, Deserialize)]
pub struct WsQuery {
    token: Option<String>,
}

/// Точка входа для `GET /ws?token=...`.
///
/// Соединение создаётся только для владельца действующей сессии; иначе
/// браузер получает обычный HTTP 401 ещё до апгрейда. Запрос с чужого
/// `Origin` отклоняется с HTTP 403 — так чужой сайт не сможет
/// подключиться, даже если украдёт токен из `localStorage`.
pub async fn upgrade(
    ws: WebSocketUpgrade,
    State(state): State<ChatState>,
    Query(query): Query<WsQuery>,
    headers: HeaderMap,
) -> Result<Response, (StatusCode, Json<Value>)> {
    let origin = headers.get(ORIGIN).and_then(|value| value.to_str().ok());
    let host = headers.get(HOST).and_then(|value| value.to_str().ok());

    if !origin_allowed(origin, host) {
        warn!(
            ?origin,
            ?host,
            "rejected websocket upgrade from a foreign origin"
        );
        return Err((
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "Запрос с недопустимого Origin" })),
        ));
    }

    let token = query
        .token
        .ok_or_else(|| unauthorized("Требуется вход в аккаунт"))?;

    let account = state
        .authenticate(&token)
        .await
        .ok_or_else(|| unauthorized("Сессия истекла, войдите снова"))?;

    Ok(ws
        .max_message_size(MAX_FRAME_SIZE)
        .max_frame_size(MAX_FRAME_SIZE)
        .on_upgrade(move |socket| handle_socket(socket, state, account)))
}

/// Дополнительные источники WebSocket из `CHAT_ALLOWED_ORIGINS`
/// (через запятую), кэшируются при первом подключении.
fn extra_allowed_origins() -> &'static [String] {
    static ORIGINS: OnceLock<Vec<String>> = OnceLock::new();

    ORIGINS
        .get_or_init(|| {
            std::env::var("CHAT_ALLOWED_ORIGINS")
                .ok()
                .map(|raw| {
                    raw.split(',')
                        .map(str::trim)
                        .filter(|item| !item.is_empty())
                        .map(ToOwned::to_owned)
                        .collect()
                })
                .unwrap_or_default()
        })
        .as_slice()
}

/// Проверяет, что апгрейд пришёл из доверенного источника.
///
/// `Origin` присылают только браузеры: запрос без этого заголовка проверять
/// нечего (на него защита от кросс-сайтовых атак и не рассчитана).
fn origin_allowed(origin: Option<&str>, host: Option<&str>) -> bool {
    origin_allowed_in(origin, host, extra_allowed_origins())
}

/// Проверка `Origin` с явно заданным списком дополнительно разрешённых источников.
fn origin_allowed_in(origin: Option<&str>, host: Option<&str>, extra: &[String]) -> bool {
    let Some(origin) = origin else {
        // Не-браузерные клиенты заголовок не шлют — их Origin защита не касается.
        return true;
    };

    if origin.trim().eq_ignore_ascii_case("null") {
        return false;
    }

    // Хост из `Origin` вида `scheme://хост[:порт]/путь`.
    let Some(origin_host) = host_of(origin) else {
        return false;
    };
    let origin_host = normalize_host(origin_host);

    // Same-origin: браузер отдаёт страницу с того же хоста, что и сокет.
    if let Some(host) = host
        && normalize_host(host) == origin_host
    {
        return true;
    }

    // Либо источник явно разрешён конфигурацией (например, за прокси).
    extra
        .iter()
        .any(|allowed| host_of(allowed).is_some_and(|value| normalize_host(value) == origin_host))
}

/// Извлекает `хост[:порт]` из `Origin`, URL или обычного адреса.
fn host_of(value: &str) -> Option<&str> {
    let value = value.trim();
    let rest = match value.split_once("://") {
        Some((_, rest)) => rest,
        None => value,
    };

    let hostport = rest.split('/').next().unwrap_or(rest);
    (!hostport.is_empty()).then_some(hostport)
}

/// Сравнивает хосты без учёта регистра и без портов по умолчанию.
fn normalize_host(host: &str) -> String {
    let host = host.trim().to_ascii_lowercase();

    for default_port in [":443", ":80"] {
        if let Some(base) = host.strip_suffix(default_port) {
            return base.to_owned();
        }
    }

    host
}

/// Основной цикл одного соединения.
///
/// Параллельно обрабатываются четыре источника событий: входящие кадры,
/// общая рассылка, личная очередь и heartbeat.
async fn handle_socket(socket: WebSocket, state: ChatState, account: Account) {
    let id = Uuid::new_v4();
    let (mut sink, mut stream) = socket.split();

    // Подписываемся до объявления пользователя, чтобы не пропустить ни одного события.
    let mut events = state.subscribe();
    // Личные сообщения приходят через отдельную очередь, принадлежащую этому соединению.
    let (inbox, mut personal) = mpsc::channel::<ServerMessage>(PERSONAL_QUEUE);

    // Имя аккаунта уже прошло проверку при регистрации, но тип нужен для состояния.
    let Ok(name) = UserName::parse(&account.username) else {
        let _ = send_error(&mut sink, "Некорректное имя аккаунта").await;
        let _ = sink.close().await;
        return;
    };

    // Один аккаунт — одно активное соединение: второе окно получит ошибку.
    let users = match state.join(id, &name, inbox).await {
        Ok(users) => users,
        Err(error) => {
            let _ = send_error(&mut sink, error.message()).await;
            let _ = sink.close().await;
            return;
        }
    };

    // Приветствие, публичная история и личные переписки.
    if !send_initial(&mut sink, &state, &name, users).await {
        state.leave(id).await;
        let _ = sink.close().await;
        return;
    }

    info!(%id, %name, "user joined");
    state.broadcast(ServerMessage::System {
        text: format!("{name} теперь в чате"),
        at: Utc::now(),
    });
    state.broadcast(ServerMessage::Users {
        users: state.users().await,
    });

    let mut heartbeat = time::interval(HEARTBEAT_INTERVAL);
    heartbeat.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
    heartbeat.tick().await; // первый тик срабатывает сразу

    loop {
        tokio::select! {
            // Входящий кадр от браузера.
            incoming = stream.next() => {
                let Some(Ok(message)) = incoming else { break };

                match message {
                    Message::Text(text) => {
                        if !handle_text(text.as_str(), id, &name, &state, &mut sink).await {
                            break;
                        }
                    }
                    Message::Close(_) => break,
                    Message::Binary(_) => {
                        // Бинарные кадры протоколом не предусмотрены.
                        if send_json(
                            &mut sink,
                            &ServerMessage::Error {
                                message: "Бинарные сообщения не поддерживаются".to_owned(),
                            },
                        )
                        .await
                        .is_err()
                        {
                            break;
                        }
                    }
                    // Ping/Pong обслуживает сам axum, здесь реакции не нужны.
                    Message::Ping(_) | Message::Pong(_) => {}
                }
            }
            // Событие общей рассылки (сообщения, системные уведомления, список участников).
            event = events.recv() => {
                match event {
                    Ok(event) => {
                        if send_json(&mut sink, &event).await.is_err() {
                            break;
                        }
                    }
                    Err(RecvError::Lagged(skipped)) => {
                        warn!(%id, skipped, "client is too slow, events were skipped");
                    }
                    Err(RecvError::Closed) => break,
                }
            }
            // Личное сообщение для этого пользователя.
            personal_message = personal.recv() => {
                match personal_message {
                    Some(event) => {
                        if send_json(&mut sink, &event).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                }
            }
            // Периодический ping: если отправить не удалось, соединение мертво.
            _ = heartbeat.tick() => {
                if sink.send(Message::Ping(Vec::new().into())).await.is_err() {
                    break;
                }
            }
        }
    }

    // Сообщаем остальным, что участник вышел.
    if let Some(name) = state.leave(id).await {
        info!(%id, %name, "user left");
        state.broadcast(ServerMessage::System {
            text: format!("{name} больше не в чате"),
            at: Utc::now(),
        });
        state.broadcast(ServerMessage::Users {
            users: state.users().await,
        });
    }

    let _ = sink.close().await;
    debug!(%id, "connection closed");
}

/// Отправляет приветствие, историю и личные переписки. `false` — если сокет закрылся.
async fn send_initial(
    sink: &mut SocketSink,
    state: &ChatState,
    name: &UserName,
    users: Vec<String>,
) -> bool {
    let welcome = ServerMessage::Welcome {
        you: name.to_string(),
        users,
    };
    if send_json(sink, &welcome).await.is_err() {
        return false;
    }

    let history = ServerMessage::History {
        messages: state.history().await,
    };
    if send_json(sink, &history).await.is_err() {
        return false;
    }

    // Личные переписки не входят в публичную историю: клиенту нужны
    // они отдельно по собеседникам, со счётчиками непрочитанных.
    let threads = state.direct_threads(name.as_str()).await;
    if !threads.is_empty()
        && send_json(sink, &ServerMessage::DirectHistory { threads })
            .await
            .is_err()
    {
        return false;
    }

    true
}

/// Обрабатывает один текстовый кадр клиента.
///
/// Возвращает `false`, если соединение нужно закрыть (например, не удалось отправить ответ).
async fn handle_text(
    raw: &str,
    id: Uuid,
    author: &UserName,
    state: &ChatState,
    sink: &mut SocketSink,
) -> bool {
    // Неизвестный или битый JSON — просто ошибка протокола, соединение не рвём.
    let message = match serde_json::from_str::<ClientMessage>(raw) {
        Ok(message) => message,
        Err(error) => {
            debug!(%id, %error, "invalid client payload");
            return send_error(sink, "Некорректный формат сообщения").await;
        }
    };

    match message {
        ClientMessage::Message { text } => {
            let text = match MessageText::parse(&text) {
                Ok(text) => text,
                Err(reason) => return send_error(sink, reason).await,
            };

            let message = ChatMessage {
                id: Uuid::new_v4(),
                author: author.to_string(),
                text: text.into_string(),
                sent_at: Utc::now(),
            };

            // Публикация сама сохранит сообщение и разошлёт его всем.
            state.publish(message).await;
            true
        }
        ClientMessage::Direct { to, text } => {
            let recipient = match UserName::parse(&to) {
                Ok(recipient) => recipient,
                Err(reason) => return send_error(sink, reason).await,
            };

            let text = match MessageText::parse(&text) {
                Ok(text) => text,
                Err(reason) => return send_error(sink, reason).await,
            };

            // Личные сообщения доставляются только тем, кто сейчас онлайн.
            let Some((recipient_id, recipient_name)) = state.find_user(recipient.as_str()).await
            else {
                return send_error(sink, "Пользователь не в сети").await;
            };

            if recipient_id == id {
                return send_error(sink, "Нельзя отправить личное сообщение самому себе").await;
            }

            let message = DirectMessage {
                id: Uuid::new_v4(),
                from: author.to_string(),
                to: recipient_name,
                text: text.into_string(),
                sent_at: Utc::now(),
            };

            // Сначала сохраняем: восстановленный диалог не должен потерять
            // сообщение, которое клиент уже получил.
            state.save_direct(&message).await;

            if !state
                .send_to(
                    recipient_id,
                    ServerMessage::Direct {
                        message: message.clone(),
                    },
                )
                .await
            {
                return send_error(sink, "Не удалось доставить сообщение").await;
            }

            // Копию отправителю шлём в его же сокет; получатель уже получил
            // свою копию через личную очередь.
            send_json(sink, &ServerMessage::Direct { message })
                .await
                .is_ok()
        }
        ClientMessage::Read { peer } => {
            let peer = match UserName::parse(&peer) {
                Ok(peer) => peer,
                Err(reason) => return send_error(sink, reason).await,
            };

            state.mark_read(author.as_str(), peer.as_str()).await;
            true
        }
    }
}

/// Ответ HTTP 401 в едином для API формате.
fn unauthorized(message: &str) -> (StatusCode, Json<Value>) {
    (StatusCode::UNAUTHORIZED, Json(json!({ "error": message })))
}

/// Отправляет клиенту сообщение об ошибке; `false`, если отправка не удалась.
async fn send_error(sink: &mut SocketSink, message: &str) -> bool {
    send_json(
        sink,
        &ServerMessage::Error {
            message: message.to_owned(),
        },
    )
    .await
    .is_ok()
}

/// Сериализует сообщение сервера в JSON и отправляет его в сокет.
async fn send_json(sink: &mut SocketSink, message: &ServerMessage) -> Result<(), axum::Error> {
    let payload =
        serde_json::to_string(message).expect("ServerMessage serialization is infallible");

    sink.send(Message::Text(payload.into())).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_requests_without_origin() {
        // Так подключаются тесты и не-браузерные клиенты.
        assert!(origin_allowed_in(None, Some("localhost:3000"), &[]));
        assert!(origin_allowed_in(None, None, &[]));
    }

    #[test]
    fn allows_same_origin() {
        assert!(origin_allowed_in(
            Some("http://localhost:3000"),
            Some("localhost:3000"),
            &[]
        ));
        // Порт по умолчанию не мешает сравнению.
        assert!(origin_allowed_in(
            Some("https://chat.example.com"),
            Some("chat.example.com"),
            &[]
        ));
        assert!(origin_allowed_in(
            Some("HTTPS://Chat.Example.Com"),
            Some("chat.example.com:443"),
            &[]
        ));
    }

    #[test]
    fn denies_foreign_origin() {
        assert!(!origin_allowed_in(
            Some("http://evil.example"),
            Some("chat.example.com"),
            &[]
        ));
        assert!(!origin_allowed_in(
            Some("null"),
            Some("chat.example.com"),
            &[]
        ));
        assert!(!origin_allowed_in(
            Some("http://chat.example.com"),
            None,
            &[]
        ));
        assert!(!origin_allowed_in(
            Some("не-адрес"),
            Some("chat.example.com"),
            &[]
        ));
    }

    #[test]
    fn allows_configured_origin() {
        let extra = ["https://chat.example.com".to_owned()];

        // Клиент с другого хоста, но из разрешённого источника.
        assert!(origin_allowed_in(
            Some("https://chat.example.com"),
            Some("api.internal:8080"),
            &extra
        ));
        assert!(!origin_allowed_in(
            Some("https://evil.example"),
            Some("api.internal:8080"),
            &extra
        ));
    }
}
