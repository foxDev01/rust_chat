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

/// How many messages a newly connected client receives.
pub const HISTORY_LIMIT: usize = 100;

/// How many in-flight events a slow client may lag behind before skipping them.
const BROADCAST_CAPACITY: usize = 256;

/// How many private messages may wait for a recipient before delivery fails.
pub const PERSONAL_QUEUE: usize = 64;

/// Shared, cheaply cloneable application state.
#[derive(Clone)]
pub struct ChatState {
    inner: Arc<Inner>,
}

struct Inner {
    events: broadcast::Sender<ServerMessage>,
    users: RwLock<BTreeMap<Uuid, User>>,
    history: RwLock<VecDeque<ChatMessage>>,
    /// `None` keeps the server memory-only, which tests rely on.
    db: Option<ChatDb>,
}

struct User {
    name: String,
    /// Private messages are pushed here instead of the shared broadcast channel,
    /// so their contents never reach other connections.
    inbox: mpsc::Sender<ServerMessage>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinError {
    NameTaken,
}

impl JoinError {
    pub fn message(self) -> &'static str {
        match self {
            Self::NameTaken => "Это имя уже занято",
        }
    }
}

impl ChatState {
    /// Memory-only state: used by tests and by `ChatState::persisted`.
    pub fn new() -> Self {
        Self::with_db(None)
    }

    /// Restores the public history from PostgreSQL so a restart does not lose it.
    pub async fn persisted(db: ChatDb) -> Result<Self, sqlx::Error> {
        let stored = db.load_public_history(HISTORY_LIMIT as i64).await?;
        let state = Self::with_db(Some(db));

        let mut history = state.inner.history.write().await;
        *history = VecDeque::from(stored);
        drop(history);

        Ok(state)
    }

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

    pub fn has_storage(&self) -> bool {
        self.inner.db.is_some()
    }

    pub async fn close_storage(&self) {
        if let Some(db) = &self.inner.db {
            db.close().await;
        }
    }

    /// Every connection gets its own receiver; `broadcast` fans events out to all of them.
    pub fn subscribe(&self) -> broadcast::Receiver<ServerMessage> {
        self.inner.events.subscribe()
    }

    pub fn broadcast(&self, message: ServerMessage) {
        // An error only means nobody is listening, which is fine.
        let _ = self.inner.events.send(message);
    }

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

    pub async fn leave(&self, id: Uuid) -> Option<String> {
        self.inner
            .users
            .write()
            .await
            .remove(&id)
            .map(|user| user.name)
    }

    pub async fn users(&self) -> Vec<String> {
        let users = self.inner.users.read().await;
        sorted_names(&users)
    }

    /// Finds a joined user by name, ignoring case. Returns the connection id
    /// and the name as it was registered.
    pub async fn find_user(&self, name: &str) -> Option<(Uuid, String)> {
        let key = name_key(name);
        let users = self.inner.users.read().await;

        users
            .iter()
            .find(|(_, user)| name_key(&user.name) == key)
            .map(|(id, user)| (*id, user.name.clone()))
    }

    /// Delivers a message to one connection. Returns `false` when the
    /// recipient has gone away or its queue is full.
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

    pub async fn history(&self) -> Vec<ChatMessage> {
        self.inner.history.read().await.iter().cloned().collect()
    }

    /// Stores a message in the bounded history, persists it and broadcasts it.
    ///
    /// Holding the history lock while broadcasting keeps history order and
    /// delivery order the same, even when several clients send at once.
    pub async fn publish(&self, message: ChatMessage) {
        {
            let mut history = self.inner.history.write().await;

            if history.len() == HISTORY_LIMIT {
                history.pop_front();
            }
            history.push_back(message.clone());

            self.broadcast(ServerMessage::Message {
                message: message.clone(),
            });
        }

        // The database is written outside the lock: `seq` decides the order after
        // a restart, and the live session already agreed on the order above.
        if let Some(db) = &self.inner.db
            && let Err(error) = db.save_public(&message).await
        {
            warn!(%error, "failed to store a public message");
        }
    }

    /// Persists a private message before it is delivered, so a client can never
    /// hold something the database does not have.
    pub async fn save_direct(&self, message: &DirectMessage) {
        if let Some(db) = &self.inner.db
            && let Err(error) = db.save_direct(message).await
        {
            warn!(%error, "failed to store a private message");
        }
    }

    /// Private conversations of `name` restored from the database. Peers who are
    /// online right now are renamed to the spelling they registered with.
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

    pub async fn mark_read(&self, user: &str, peer: &str) {
        if let Some(db) = &self.inner.db
            && let Err(error) = db.mark_read(user, peer).await
        {
            warn!(%error, "failed to store a read mark");
        }
    }

    /// Stored row counts, `None` when running without a database.
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

fn sorted_names(users: &BTreeMap<Uuid, User>) -> Vec<String> {
    let mut names: Vec<String> = users.values().map(|user| user.name.clone()).collect();
    names.sort_by_cached_key(|name| name_key(name));
    names
}

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
