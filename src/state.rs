//! Общее состояние чата: участники онлайн, история и рассылка событий.
//!
//! Состояние дешево клонируется: внутри лежит `Arc<Inner>`, поэтому один и тот
//! же экземпляр используют все WebSocket-соединения и HTTP-обработчики.

use std::{
    collections::{BTreeMap, VecDeque},
    sync::Arc,
};

use tokio::sync::{RwLock, broadcast, mpsc};
use tracing::warn;
use uuid::Uuid;

use crate::db::{ChatDb, THREAD_REPLAY};
use crate::protocol::{
    ChatMessage, DirectMessage, DirectThread, ServerMessage, UserName, name_key,
};

/// Сколько последних сообщений получает новый клиент при подключении.
pub const HISTORY_LIMIT: usize = 100;

/// Сколько событий может накопиться у медленного клиента, прежде чем они начнут пропускаться.
const BROADCAST_CAPACITY: usize = 256;

/// Сколько личных сообщений может ждать получателя, прежде чем доставка не удастся.
pub const PERSONAL_QUEUE: usize = 64;

/// Общее состояние приложения, которое дешево клонируется.
#[derive(Clone)]
pub struct ChatState {
    inner: Arc<Inner>,
}

struct Inner {
    /// Канал широковещательной рассылки: у каждого соединения свой получатель.
    events: broadcast::Sender<ServerMessage>,
    /// Подключённые пользователи: id соединения → имя и личная очередь.
    users: RwLock<BTreeMap<Uuid, User>>,
    /// Ограниченная история общего канала (не более [`HISTORY_LIMIT`] сообщений).
    history: RwLock<VecDeque<ChatMessage>>,
    /// `None` — сервер работает только в памяти, на этом построены тесты.
    db: Option<ChatDb>,
}

struct User {
    name: String,
    /// Личные сообщения кладутся сюда, а не в общий broadcast-канал,
    /// чтобы их содержимое не уходило другим соединениям.
    inbox: mpsc::Sender<ServerMessage>,
}

/// Ошибки, которые могут возникнуть при входе в чат.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinError {
    /// Имя уже занято другим участником (сравнение без учёта регистра).
    NameTaken,
}

impl JoinError {
    /// Текст ошибки для отправки клиенту.
    pub fn message(self) -> &'static str {
        match self {
            Self::NameTaken => "Это имя уже занято",
        }
    }
}

impl ChatState {
    /// Состояние только в памяти: используется тестами и внутри [`ChatState::persisted`].
    pub fn new() -> Self {
        Self::with_db(None)
    }

    /// Восстанавливает публичную историю из PostgreSQL, чтобы перезапуск её не терял.
    pub async fn persisted(db: ChatDb) -> Result<Self, sqlx::Error> {
        let stored = db.load_public_history(HISTORY_LIMIT as i64).await?;
        let state = Self::with_db(Some(db));

        let mut history = state.inner.history.write().await;
        *history = VecDeque::from(stored);
        drop(history);

        Ok(state)
    }

    /// Создаёт состояние с указанным (или отсутствующим) хранилищем.
    fn with_db(db: Option<ChatDb>) -> Self {
        let (events, _) = broadcast::channel(BROADCAST_CAPACITY);

        Self {
            inner: Arc::new(Inner {
                events,
                users: RwLock::new(BTreeMap::new()),
                history: RwLock::new(VecDeque::with_capacity(HISTORY_LIMIT)),
                db,
            }),
        }
    }

    /// Есть ли подключённая база данных.
    pub fn has_storage(&self) -> bool {
        self.inner.db.is_some()
    }

    /// Закрывает пул соединений с базой при остановке сервера.
    pub async fn close_storage(&self) {
        if let Some(db) = &self.inner.db {
            db.close().await;
        }
    }

    /// Новый получатель широковещательных событий: у каждого соединения он свой.
    pub fn subscribe(&self) -> broadcast::Receiver<ServerMessage> {
        self.inner.events.subscribe()
    }

    /// Отправляет событие всем подписчикам.
    pub fn broadcast(&self, message: ServerMessage) {
        // Ошибка означает лишь то, что слушателей нет, — это нормально.
        let _ = self.inner.events.send(message);
    }

    /// Регистрирует пользователя и возвращает отсортированный список участников.
    ///
    /// Имя сравнивается без учёта регистра, поэтому «Анна» и «анна» — одно и то же.
    pub async fn join(
        &self,
        id: Uuid,
        name: &UserName,
        inbox: mpsc::Sender<ServerMessage>,
    ) -> Result<Vec<String>, JoinError> {
        let mut users = self.inner.users.write().await;

        let key = name_key(name.as_str());
        if users.values().any(|user| name_key(&user.name) == key) {
            return Err(JoinError::NameTaken);
        }

        users.insert(
            id,
            User {
                name: name.as_str().to_owned(),
                inbox,
            },
        );
        Ok(sorted_names(&users))
    }

    /// Удаляет пользователя при отключении и возвращает его имя, если он был в чате.
    pub async fn leave(&self, id: Uuid) -> Option<String> {
        self.inner
            .users
            .write()
            .await
            .remove(&id)
            .map(|user| user.name)
    }

    /// Имена участников онлайн, отсортированные без учёта регистра.
    pub async fn users(&self) -> Vec<String> {
        let users = self.inner.users.read().await;
        sorted_names(&users)
    }

    /// Находит участника по имени без учёта регистра.
    ///
    /// Возвращает id соединения и имя в том написании, в котором пользователь вошёл.
    pub async fn find_user(&self, name: &str) -> Option<(Uuid, String)> {
        let key = name_key(name);
        let users = self.inner.users.read().await;

        users
            .iter()
            .find(|(_, user)| name_key(&user.name) == key)
            .map(|(id, user)| (*id, user.name.clone()))
    }

    /// Доставляет сообщение одному соединению.
    ///
    /// Возвращает `false`, если получатель уже отключился или его очередь переполнена.
    pub async fn send_to(&self, id: Uuid, message: ServerMessage) -> bool {
        let inbox = {
            let users = self.inner.users.read().await;
            match users.get(&id) {
                Some(user) => user.inbox.clone(),
                None => return false,
            }
        };

        inbox.try_send(message).is_ok()
    }

    /// Снимок текущей публичной истории.
    pub async fn history(&self) -> Vec<ChatMessage> {
        self.inner.history.read().await.iter().cloned().collect()
    }

    /// Сохраняет сообщение в ограниченной истории, пишет его в базу и рассылает.
    ///
    /// Блокировка истории удерживается и во время рассылки, чтобы порядок в
    /// истории совпадал с порядком доставки, даже если пишут несколько клиентов сразу.
    pub async fn publish(&self, message: ChatMessage) {
        {
            let mut history = self.inner.history.write().await;

            // История ограничена: самое старое сообщение вытесняется.
            if history.len() == HISTORY_LIMIT {
                history.pop_front();
            }
            history.push_back(message.clone());

            self.broadcast(ServerMessage::Message {
                message: message.clone(),
            });
        }

        // В базу пишем вне блокировки: порядок после перезапуска задаёт `seq`,
        // а в текущей сессии порядок уже определён выше.
        if let Some(db) = &self.inner.db
            && let Err(error) = db.save_public(&message).await
        {
            warn!(%error, "failed to store a public message");
        }
    }

    /// Сохраняет личное сообщение до его доставки, чтобы клиент никогда не получил
    /// то, чего ещё нет в базе.
    pub async fn save_direct(&self, message: &DirectMessage) {
        if let Some(db) = &self.inner.db
            && let Err(error) = db.save_direct(message).await
        {
            warn!(%error, "failed to store a private message");
        }
    }

    /// Личные переписки пользователя из базы.
    ///
    /// Если собеседник сейчас онлайн, имя приводится к написанию, под которым он вошёл.
    pub async fn direct_threads(&self, name: &str) -> Vec<DirectThread> {
        let Some(db) = &self.inner.db else {
            return Vec::new();
        };

        let threads = match db.load_direct_threads(name, THREAD_REPLAY).await {
            Ok(threads) => threads,
            Err(error) => {
                warn!(%error, "failed to load private threads");
                return Vec::new();
            }
        };

        let online = self.users().await;

        threads
            .into_iter()
            .map(|thread| {
                let key = name_key(&thread.peer);
                match online.iter().find(|user| name_key(user) == key) {
                    Some(canonical) => DirectThread {
                        peer: canonical.clone(),
                        ..thread
                    },
                    None => thread,
                }
            })
            .collect()
    }

    /// Запоминает, что пользователь прочитал всё, что прислал собеседник.
    pub async fn mark_read(&self, user: &str, peer: &str) {
        if let Some(db) = &self.inner.db
            && let Err(error) = db.mark_read(user, peer).await
        {
            warn!(%error, "failed to store a read mark");
        }
    }

    /// Количество строк в хранилище; `None`, если базы нет.
    pub async fn storage_counts(&self) -> Option<(i64, i64)> {
        let db = self.inner.db.as_ref()?;
        db.counts().await.ok()
    }
}

impl Default for ChatState {
    fn default() -> Self {
        Self::new()
    }
}

/// Собирает имена участников и сортирует их без учёта регистра.
fn sorted_names(users: &BTreeMap<Uuid, User>) -> Vec<String> {
    let mut names: Vec<String> = users.values().map(|user| user.name.clone()).collect();
    names.sort_by_cached_key(|name| name_key(name));
    names
}

// Тесты работают только с состоянием в памяти и не требуют PostgreSQL.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::DirectMessage;
    use chrono::Utc;

    fn text(value: &str) -> UserName {
        UserName::parse(value).expect("valid test name")
    }

    fn inbox() -> mpsc::Sender<ServerMessage> {
        mpsc::channel(PERSONAL_QUEUE).0
    }

    fn message(author: &str, value: &str) -> ChatMessage {
        ChatMessage {
            id: Uuid::new_v4(),
            author: author.to_owned(),
            text: value.to_owned(),
            sent_at: Utc::now(),
        }
    }

    fn direct(from: &str, to: &str, value: &str) -> DirectMessage {
        DirectMessage {
            id: Uuid::new_v4(),
            from: from.to_owned(),
            to: to.to_owned(),
            text: value.to_owned(),
            sent_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn rejects_duplicate_names_ignoring_case() {
        let state = ChatState::new();

        assert!(
            state
                .join(Uuid::new_v4(), &text("Анна"), inbox())
                .await
                .is_ok()
        );
        assert_eq!(
            state.join(Uuid::new_v4(), &text("анна"), inbox()).await,
            Err(JoinError::NameTaken)
        );
    }

    #[tokio::test]
    async fn keeps_users_sorted_and_removes_them_on_leave() {
        let state = ChatState::new();
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();

        state.join(first, &text("яблоко"), inbox()).await.unwrap();
        state.join(second, &text("Банан"), inbox()).await.unwrap();
        assert_eq!(state.users().await, ["Банан", "яблоко"]);

        assert_eq!(state.leave(first).await.as_deref(), Some("яблоко"));
        assert_eq!(state.users().await, ["Банан"]);
        assert_eq!(state.leave(first).await, None);
    }

    #[tokio::test]
    async fn delivers_direct_messages_only_to_the_recipient() {
        let state = ChatState::new();
        let alice = Uuid::new_v4();
        let bob = Uuid::new_v4();

        state.join(alice, &text("Алиса"), inbox()).await.unwrap();
        let (bob_inbox, mut bob_messages) = mpsc::channel(PERSONAL_QUEUE);
        state.join(bob, &text("Боб"), bob_inbox).await.unwrap();

        let (found_id, found_name) = state.find_user("боб").await.expect("user is online");
        assert_eq!(found_id, bob);
        assert_eq!(found_name, "Боб");

        let message = direct("Алиса", "Боб", "привет");
        assert!(
            state
                .send_to(found_id, ServerMessage::Direct { message })
                .await
        );

        match bob_messages.recv().await {
            Some(ServerMessage::Direct { message }) => {
                assert_eq!(message.from, "Алиса");
                assert_eq!(message.text, "привет");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn rejects_delivery_to_unknown_users() {
        let state = ChatState::new();

        assert!(state.find_user("Никого").await.is_none());
        assert!(
            !state
                .send_to(
                    Uuid::new_v4(),
                    ServerMessage::Error {
                        message: String::new()
                    }
                )
                .await
        );
    }

    #[tokio::test]
    async fn history_is_bounded() {
        let state = ChatState::new();

        // Публикуем больше сообщений, чем помещается в историю.
        for index in 0..(HISTORY_LIMIT + 5) {
            state.publish(message("bot", &index.to_string())).await;
        }

        let history = state.history().await;
        assert_eq!(history.len(), HISTORY_LIMIT);
        assert_eq!(history.first().unwrap().text, "5");
        assert_eq!(
            history.last().unwrap().text,
            (HISTORY_LIMIT + 4).to_string()
        );
    }
}
