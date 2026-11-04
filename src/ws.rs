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

/// A browser message is at most 2 000 chars, plus JSON overhead and multibyte text.
const MAX_FRAME_SIZE: usize = 16 * 1024;
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

type SocketSink = SplitSink<WebSocket, Message>;

pub async fn upgrade(ws: WebSocketUpgrade, State(state): State<ChatState>) -> impl IntoResponse {
    ws.max_message_size(MAX_FRAME_SIZE)
        .max_frame_size(MAX_FRAME_SIZE)
        .on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: ChatState) {
    let id = Uuid::new_v4();
    let (mut sink, mut stream) = socket.split();

    // Subscribe before announcing the user, so no event is missed.
    let mut events = state.subscribe();
    // Private messages arrive through a dedicated queue owned by this connection.
    let (inbox, mut personal) = mpsc::channel::<ServerMessage>(PERSONAL_QUEUE);
    let mut me: Option<UserName> = None;

    let mut heartbeat = time::interval(HEARTBEAT_INTERVAL);
    heartbeat.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
    heartbeat.tick().await; // the first tick fires immediately

    loop {
        tokio::select! {
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
                    Message::Ping(_) | Message::Pong(_) => {}
                }
            }
            event = events.recv() => {
                match event {
                    Ok(event) if me.is_some() => {
                        if send_json(&mut sink, &event).await.is_err() {
                            break;
                        }
                    }
                    Ok(_) => {} // not joined yet, nothing to show
                    Err(RecvError::Lagged(skipped)) => {
                        warn!(%id, skipped, "client is too slow, events were skipped");
                    }
                    Err(RecvError::Closed) => break,
                }
            }
            personal_message = personal.recv() => {
                match personal_message {
                    Some(event) if me.is_some() => {
                        if send_json(&mut sink, &event).await.is_err() {
                            break;
                        }
                    }
                    Some(_) => {} // not joined yet, nothing to show
                    None => break,
                }
            }
            _ = heartbeat.tick() => {
                if sink.send(Message::Ping(Vec::new().into())).await.is_err() {
                    break;
                }
            }
        }
    }

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

/// Handles one client payload. Returns `false` when the connection should close.
async fn handle_text(
    raw: &str,
    id: Uuid,
    me: &mut Option<UserName>,
    state: &ChatState,
    inbox: &mpsc::Sender<ServerMessage>,
    sink: &mut SocketSink,
) -> bool {
    let message = match serde_json::from_str::<ClientMessage>(raw) {
        Ok(message) => message,
        Err(error) => {
            debug!(%id, %error, "invalid client payload");
            return send_error(sink, "Некорректный формат сообщения").await;
        }
    };

    match message {
        ClientMessage::Join { name } => {
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

            // Private conversations are not part of the public history: the client
            // needs them per peer, with unread counters.
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

            // Persist first: a restored thread must never miss a message that
            // some client has already received.
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

            // Echo to the sender through its own socket; the recipient already
            // got a copy through its private queue.
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

async fn send_json(sink: &mut SocketSink, message: &ServerMessage) -> Result<(), axum::Error> {
    let payload =
        serde_json::to_string(message).expect("ServerMessage serialization is infallible");

    sink.send(Message::Text(payload.into())).await
}
