//! Общее состояние чата: участники онлайн, история и рассылка событий.
//!
//! Состояние дешево клонируется: внутри лежит `Arc<Inner>`, поэтому один и тот
//! же экземпляр используют все WebSocket-соединения и HTTP-обработчики.

use std::{
    collections::{BTreeMap, VecDeque},
    net::IpAddr,
    sync::Arc,
    time::Duration,
};

use tokio::sync::{RwLock, broadcast, mpsc};
use tracing::warn;
use uuid::Uuid;

use crate::auth::{self, Account, AuthError, AuthSession};
use crate::db::{ChatDb, THREAD_REPLAY};
use crate::protocol::{
    ChatMessage, DirectMessage, DirectThread, ServerMessage, UserName, name_key,
};
use crate::rate_limit::{LOGIN_QUOTA, REGISTER_QUOTA, RateLimiter};

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
    /// Лимиты попыток входа и регистрации по IP-адресу.
    login_limiter: RateLimiter,
    register_limiter: RateLimiter,
    /// `None` — сервер работает только в памяти, на этом построены тесты.
    db: Option<ChatDb>,
}

struct User {
    name: String,
    /// Хеш токена сессии, по которому открыто это окно.
    ///
    /// Нужен, чтобы при смене пароля отключить чужие устройства, но оставить
    /// окна, работающие с тем же токеном.
    token_hash: String,
    /// Личные сообщения кладутся сюда, а не в общий broadcast-канал,
    /// чтобы их содержимое не уходило другим соединениям.
    inbox: mpsc::Sender<ServerMessage>,
}

impl ChatState {
    /// Состояние только в памяти: используется тестами и внутри [`ChatState::persisted`].
    pub fn new() -> Self {
        Self::with_db(None)
    }

    /// Восстанавливает публичную историю из PostgreSQL, чтобы перезапуск её не терял.
    pub async fn persisted(db: ChatDb) -> Result<Self, sqlx::Error> {
        // Попутно чистим сессии, срок которых уже истёк.
        if let Err(error) = db.purge_expired_sessions().await {
            warn!(%error, "failed to purge expired sessions");
        }

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
                login_limiter: RateLimiter::new(LOGIN_QUOTA),
                register_limiter: RateLimiter::new(REGISTER_QUOTA),
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

    /// Регистрирует аккаунт и сразу открывает для него сессию.
    pub async fn register(
        &self,
        username: &UserName,
        password: &str,
    ) -> Result<AuthSession, AuthError> {
        let db = self.inner.db.as_ref().ok_or(AuthError::Unavailable)?;

        // Argon2 намеренно медленный, поэтому считаем хеш вне async-рантайма.
        let hash = auth::hash_password_async(password.to_owned())
            .await
            .map_err(AuthError::Internal)?;

        match db
            .create_account(username, &hash)
            .await
            .map_err(storage_error)?
        {
            Some(account) => self.open_session(account).await,
            None => Err(AuthError::UsernameTaken),
        }
    }

    /// Проверяет пароль и открывает новую сессию.
    pub async fn login(&self, username: &str, password: &str) -> Result<AuthSession, AuthError> {
        let db = self.inner.db.as_ref().ok_or(AuthError::Unavailable)?;

        // Одинаковый ответ для несуществующего имени и неверного пароля,
        // чтобы нельзя было перебором узнать, какие аккаунты есть.
        let Some((account, hash)) = db
            .find_account_by_key(&name_key(username))
            .await
            .map_err(storage_error)?
        else {
            return Err(AuthError::InvalidCredentials);
        };

        if !auth::verify_password_async(password.to_owned(), hash).await {
            return Err(AuthError::InvalidCredentials);
        }

        self.open_session(account).await
    }

    /// Возвращает аккаунт по токену действующей сессии.
    pub async fn authenticate(&self, token: &str) -> Option<Account> {
        let db = self.inner.db.as_ref()?;
        db.find_account_by_token(&auth::token_hash(token))
            .await
            .ok()
            .flatten()
    }

    /// Проверяет лимит попыток входа с указанного IP-адреса.
    pub async fn check_login_rate(&self, ip: IpAddr) -> Result<(), Duration> {
        self.inner.login_limiter.check(ip).await
    }

    /// Проверяет лимит попыток регистрации с указанного IP-адреса.
    pub async fn check_register_rate(&self, ip: IpAddr) -> Result<(), Duration> {
        self.inner.register_limiter.check(ip).await
    }

    /// Отзывает сессию (выход из аккаунта).
    pub async fn logout(&self, token: &str) {
        if let Some(db) = &self.inner.db
            && let Err(error) = db.delete_session(&auth::token_hash(token)).await
        {
            warn!(%error, "failed to delete a session");
        }
    }

    /// Создаёт токен сессии и сохраняет его хеш в базе.
    async fn open_session(&self, account: Account) -> Result<AuthSession, AuthError> {
        let db = self.inner.db.as_ref().ok_or(AuthError::Unavailable)?;
        let token = auth::new_token();

        db.create_session(
            account.id,
            &auth::token_hash(&token),
            auth::session_expiry(),
        )
        .await
        .map_err(storage_error)?;

        Ok(AuthSession { token, account })
    }

    /// Меняет пароль владельца сессии.
    ///
    /// После успешной смены все остальные сессии аккаунта отзываются:
    /// сохранённый где-то ещё токен перестаёт работать, а текущее устройство
    /// остаётся в чате и не требует повторного входа.
    pub async fn change_password(
        &self,
        token: &str,
        current_password: &str,
        new_password: &str,
    ) -> Result<(), AuthError> {
        let db = self.inner.db.as_ref().ok_or(AuthError::Unavailable)?;
        let token_hash = auth::token_hash(token);

        let account = db
            .find_account_by_token(&token_hash)
            .await
            .map_err(storage_error)?
            .ok_or(AuthError::Unauthorized)?;

        let Some((_, hash)) = db
            .find_account_by_key(&name_key(&account.username))
            .await
            .map_err(storage_error)?
        else {
            return Err(AuthError::Internal(
                "аккаунт пропал из таблицы accounts".to_owned(),
            ));
        };

        // Текущий пароль обязателен: одного украденного токена недостаточно,
        // чтобы полностью завладеть аккаунтом.
        if !auth::verify_password_async(current_password.to_owned(), hash).await {
            return Err(AuthError::WrongPassword);
        }

        let hash = auth::hash_password_async(new_password.to_owned())
            .await
            .map_err(AuthError::Internal)?;

        db.update_password(account.id, &hash)
            .await
            .map_err(storage_error)?;
        // Остальные сессии отзываем уже после смены хеша.
        db.delete_other_sessions(account.id, &token_hash)
            .await
            .map_err(storage_error)?;

        // И сразу закрываем их открытые WebSocket-соединения: недостаточно
        // просто удалить токен, если украденное соединение уже установлено.
        self.end_other_sessions(&account.username, &token_hash)
            .await;

        Ok(())
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

    /// Регистрирует соединение и возвращает список участников.
    ///
    /// Один аккаунт может быть открыт в нескольких окнах: каждое соединение
    /// получает свой id, а в списке участников имя показывается один раз.
    /// Второй элемент результата — `true`, если это первое соединение имени.
    pub async fn join(
        &self,
        id: Uuid,
        name: &UserName,
        token_hash: &str,
        inbox: mpsc::Sender<ServerMessage>,
    ) -> (Vec<String>, bool) {
        let mut users = self.inner.users.write().await;

        let key = name_key(name.as_str());
        let first = !users.values().any(|user| name_key(&user.name) == key);

        users.insert(
            id,
            User {
                name: name.as_str().to_owned(),
                token_hash: token_hash.to_owned(),
                inbox,
            },
        );

        (sorted_names(&users), first)
    }

    /// Удаляет соединение.
    ///
    /// Возвращает имя и признак того, что пользователь остался онлайн
    /// в другом окне.
    pub async fn leave(&self, id: Uuid) -> Option<(String, bool)> {
        let mut users = self.inner.users.write().await;
        let user = users.remove(&id)?;

        let key = name_key(&user.name);
        let still_online = users.values().any(|other| name_key(&other.name) == key);

        Some((user.name, still_online))
    }

    /// Имена участников онлайн, отсортированные без учёта регистра.
    ///
    /// Несколько окон одного аккаунта дают одно имя в списке.
    pub async fn users(&self) -> Vec<String> {
        let users = self.inner.users.read().await;
        sorted_names(&users)
    }

    /// Все соединения пользователя с указанным именем (без учёта регистра).
    ///
    /// Личное сообщение доставляется в каждое открытое окно получателя.
    pub async fn connections(&self, name: &str) -> Vec<(Uuid, String)> {
        let key = name_key(name);
        let users = self.inner.users.read().await;

        users
            .iter()
            .filter(|(_, user)| name_key(&user.name) == key)
            .map(|(id, user)| (*id, user.name.clone()))
            .collect()
    }

    /// Завершает все соединения пользователя, открытые с других токенов.
    ///
    /// Вызывается после смены пароля: окна с тем же токеном остаются в чате,
    /// а чужие устройства получают `session_ended` и закрывают соединение.
    pub async fn end_other_sessions(&self, name: &str, keep_token_hash: &str) {
        let key = name_key(name);

        // Собираем отправители под блокировкой, а отправляем уже без неё.
        let targets: Vec<mpsc::Sender<ServerMessage>> = self
            .inner
            .users
            .read()
            .await
            .values()
            .filter(|user| name_key(&user.name) == key && user.token_hash != keep_token_hash)
            .map(|user| user.inbox.clone())
            .collect();

        for inbox in targets {
            let message = ServerMessage::SessionEnded {
                reason: "Пароль изменён на другом устройстве. Войдите снова.".to_owned(),
            };

            // Не ждём медленного клиента: смена пароля не должна зависать
            // из-за переполненной очереди чужого соединения.
            if inbox.try_send(message).is_err() {
                warn!("failed to end a stale session: client queue is full");
            }
        }
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

/// Преобразует ошибку базы в доменную ошибку аутентификации.
fn storage_error(error: sqlx::Error) -> AuthError {
    AuthError::Internal(error.to_string())
}

/// Собирает уникальные имена участников и сортирует их без учёта регистра.
fn sorted_names(users: &BTreeMap<Uuid, User>) -> Vec<String> {
    let mut names: Vec<String> = users.values().map(|user| user.name.clone()).collect();
    names.sort_by_cached_key(|name| name_key(name));
    names.dedup_by(|first, second| name_key(first) == name_key(second));
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
    async fn allows_multiple_windows_for_one_account() {
        let state = ChatState::new();
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();

        let (users, is_first) = state.join(first, &text("Анна"), "token-a", inbox()).await;
        assert!(is_first);
        assert_eq!(users, ["Анна"]);

        // Второе окно того же аккаунта не создаёт вторую строку в списке.
        let (users, is_first) = state.join(second, &text("Анна"), "token-a", inbox()).await;
        assert!(!is_first);
        assert_eq!(users, ["Анна"]);

        // Закрытие одного окна не считается выходом из чата.
        assert_eq!(state.leave(first).await, Some(("Анна".to_owned(), true)));
        assert_eq!(state.leave(second).await, Some(("Анна".to_owned(), false)));
    }

    #[tokio::test]
    async fn ends_only_sessions_with_another_token() {
        let state = ChatState::new();

        let (own_inbox, mut own_messages) = mpsc::channel(PERSONAL_QUEUE);
        state
            .join(Uuid::new_v4(), &text("Анна"), "token-a", own_inbox)
            .await;

        let (other_inbox, mut other_messages) = mpsc::channel(PERSONAL_QUEUE);
        state
            .join(Uuid::new_v4(), &text("Анна"), "token-b", other_inbox)
            .await;

        state.end_other_sessions("анна", "token-a").await;

        // Чужое устройство получило команду закрыть соединение.
        assert!(matches!(
            other_messages.try_recv(),
            Ok(ServerMessage::SessionEnded { .. })
        ));
        // Окно, с которого меняли пароль, продолжает работать.
        assert!(own_messages.try_recv().is_err());
    }

    #[tokio::test]
    async fn keeps_users_sorted_and_removes_them_on_leave() {
        let state = ChatState::new();
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();

        state.join(first, &text("яблоко"), "token-a", inbox()).await;
        state.join(second, &text("Банан"), "token-b", inbox()).await;
        assert_eq!(state.users().await, ["Банан", "яблоко"]);

        assert_eq!(state.leave(first).await, Some(("яблоко".to_owned(), false)));
        assert_eq!(state.users().await, ["Банан"]);
        assert_eq!(state.leave(first).await, None);
    }

    #[tokio::test]
    async fn delivers_direct_messages_only_to_the_recipient() {
        let state = ChatState::new();
        let alice = Uuid::new_v4();
        let bob = Uuid::new_v4();

        state.join(alice, &text("Алиса"), "token-a", inbox()).await;
        let (bob_inbox, mut bob_messages) = mpsc::channel(PERSONAL_QUEUE);
        state.join(bob, &text("Боб"), "token-b", bob_inbox).await;

        let connections = state.connections("боб").await;
        assert_eq!(connections, [(bob, "Боб".to_owned())]);

        let message = direct("Алиса", "Боб", "привет");
        assert!(
            state
                .send_to(connections[0].0, ServerMessage::Direct { message })
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

        assert!(state.connections("Никого").await.is_empty());
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
