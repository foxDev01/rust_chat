//! Обработка WebSocket-соединений: вход, сообщения, личные переписки и heartbeat.

use axum::{
    extract::{
        State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    response::IntoResponse,
};
use chrono::Utc;
use futures_util::{SinkExt, StreamExt, stream::SplitSink};
use tokio::{
    sync::{broadcast::error::RecvError, mpsc},
    time::{self, Duration},
};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::{
    protocol::{ChatMessage, ClientMessage, DirectMessage, MessageText, ServerMessage, UserName},
    state::{ChatState, PERSONAL_QUEUE},
};

/// Сообщение браузера — не больше 2 000 символов плюс JSON-обвязка и многобайтовый текст.
const MAX_FRAME_SIZE: usize = 16 * 1024;
/// Как часто отправляем ping, чтобы заметить мёртвое соединение.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

type SocketSink = SplitSink<WebSocket, Message>;

/// Точка входа для `GET /ws`: проверяет параметры апгрейда и передаёт сокет обработчику.
pub async fn upgrade(ws: WebSocketUpgrade, State(state): State<ChatState>) -> impl IntoResponse {
    ws.max_message_size(MAX_FRAME_SIZE)
        .max_frame_size(MAX_FRAME_SIZE)
        .on_upgrade(move |socket| handle_socket(socket, state))
}

/// Основной цикл одного соединения.
///
/// Параллельно обрабатываются четыре источника событий: входящие кадры,
/// общая рассылка, личная очередь и heartbeat.
async fn handle_socket(socket: WebSocket, state: ChatState) {
    let id = Uuid::new_v4();
    let (mut sink, mut stream) = socket.split();

    // Подписываемся до объявления пользователя, чтобы не пропустить ни одного события.
    let mut events = state.subscribe();
    // Личные сообщения приходят через отдельную очередь, принадлежащую этому соединению.
    let (inbox, mut personal) = mpsc::channel::<ServerMessage>(PERSONAL_QUEUE);
    let mut me: Option<UserName> = None;

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
                        if !handle_text(text.as_str(), id, &mut me, &state, &inbox, &mut sink).await {
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
                    Ok(event) if me.is_some() => {
                        if send_json(&mut sink, &event).await.is_err() {
                            break;
                        }
                    }
                    Ok(_) => {} // пользователь ещё не вошёл — показывать нечего
                    Err(RecvError::Lagged(skipped)) => {
                        warn!(%id, skipped, "client is too slow, events were skipped");
                    }
                    Err(RecvError::Closed) => break,
                }
            }
            // Личное сообщение для этого пользователя.
            personal_message = personal.recv() => {
                match personal_message {
                    Some(event) if me.is_some() => {
                        if send_json(&mut sink, &event).await.is_err() {
                            break;
                        }
                    }
                    Some(_) => {} // пользователь ещё не вошёл — показывать нечего
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

    // Сообщаем остальным, что участник вышел (если он вообще успел войти).
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

/// Обрабатывает один текстовый кадр клиента.
///
/// Возвращает `false`, если соединение нужно закрыть (например, не удалось отправить ответ).
async fn handle_text(
    raw: &str,
    id: Uuid,
    me: &mut Option<UserName>,
    state: &ChatState,
    inbox: &mpsc::Sender<ServerMessage>,
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
        ClientMessage::Join { name } => {
            // Повторный вход в одном соединении запрещён.
            if me.is_some() {
                return send_error(sink, "Вы уже вошли в чат").await;
            }

            let name = match UserName::parse(&name) {
                Ok(name) => name,
                Err(reason) => return send_error(sink, reason).await,
            };

            let users = match state.join(id, &name, inbox.clone()).await {
                Ok(users) => users,
                Err(error) => return send_error(sink, error.message()).await,
            };

            *me = Some(name.clone());

            // Приветствие: кто вы и кто сейчас онлайн.
            let welcome = ServerMessage::Welcome {
                you: name.to_string(),
                users,
            };
            if send_json(sink, &welcome).await.is_err() {
                return false;
            }

            // Публичная история общего канала.
            let history = ServerMessage::History {
                messages: state.history().await,
            };
            if send_json(sink, &history).await.is_err() {
                return false;
            }

            // Личные переписки не входят в публичную историю: клиенту нужны
            // они отдельно по собеседникам, со счётчиками непрочитанных.
            let threads = state.direct_threads(name.as_str()).await;
            if !threads.is_empty() {
                let restored = ServerMessage::DirectHistory { threads };
                if send_json(sink, &restored).await.is_err() {
                    return false;
                }
            }

            info!(%id, %name, "user joined");
            state.broadcast(ServerMessage::System {
                text: format!("{name} теперь в чате"),
                at: Utc::now(),
            });
            state.broadcast(ServerMessage::Users {
                users: state.users().await,
            });

            true
        }
        ClientMessage::Message { text } => {
            // Писать можно только после входа.
            let Some(author) = me.as_ref() else {
                return send_error(sink, "Сначала войдите в чат").await;
            };

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
            let Some(author) = me.as_ref() else {
                return send_error(sink, "Сначала войдите в чат").await;
            };

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
            let Some(author) = me.as_ref() else {
                return send_error(sink, "Сначала войдите в чат").await;
            };

            let peer = match UserName::parse(&peer) {
                Ok(peer) => peer,
                Err(reason) => return send_error(sink, reason).await,
            };

            state.mark_read(author.as_str(), peer.as_str()).await;
            true
        }
    }
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
