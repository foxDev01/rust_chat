//! Общее состояние чата: участники онлайн, история и рассылка событий.
//!
//! Модуль собран из нескольких частей, каждая из которых отвечает за своё:
//!
//! - [`presence`] — кто онлайн, окна аккаунтов и личные очереди;
//! - [`history`] — публичная история комнат и рассылка сообщений;
//! - [`direct`] — личные переписки и отметки о прочтении;
//! - [`tickets`] — одноразовые билеты для входа в WebSocket.
//!
//! Сам `ChatState` остаётся тонким фасадом: состояние дешево клонируется,
//! внутри лежит `Arc<Inner>`, поэтому один и тот же экземпляр используют все
//! WebSocket-соединения и HTTP-обработчики.

use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    net::IpAddr,
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

use tokio::sync::{Mutex, RwLock, broadcast};
use tracing::warn;
use uuid::Uuid;

use crate::auth::{self, Account, AuthError, AuthSession};
use crate::db::{ChatDb, StoredAttachment};
use crate::protocol::{DEFAULT_ROOMS, Room, ServerMessage};
use crate::rate_limit::{
    FAILED_LOGIN_QUOTA, LOGIN_QUOTA, MESSAGE_QUOTA, REGISTER_QUOTA, RateLimiter, UPLOAD_QUOTA,
    WS_TICKET_QUOTA,
};
use crate::uploads::{ORPHAN_GRACE, Uploads};

mod direct;
mod history;
mod presence;
mod tickets;

pub use self::history::HISTORY_LIMIT;
#[cfg(test)]
pub use self::history::PAGE_LIMIT;
pub use self::presence::PERSONAL_QUEUE;
pub use self::tickets::TICKET_TTL;

use self::presence::User;
use self::tickets::WsTicket;

/// Сколько событий может накопиться у медленного клиента, прежде чем они начнут пропускаться.
const BROADCAST_CAPACITY: usize = 256;

/// Общее состояние приложения, которое дешево клонируется.
#[derive(Clone)]
pub struct ChatState {
    inner: Arc<Inner>,
}

struct Inner {
    /// Общие события: системные уведомления и список участников.
    events: broadcast::Sender<ServerMessage>,
    /// Отдельный канал на каждую комнату: сообщения и «печатает…» получают
    /// только соединения, открывшие эту комнату.
    room_events: RwLock<HashMap<String, broadcast::Sender<ServerMessage>>>,
    /// Подключённые пользователи: id соединения → имя и личная очередь.
    users: RwLock<BTreeMap<Uuid, User>>,
    /// Доступные комнаты в порядке отображения.
    rooms: RwLock<Vec<Room>>,
    /// Ограниченная история каждой комнаты (не более [`HISTORY_LIMIT`] сообщений).
    history: RwLock<HashMap<String, VecDeque<crate::protocol::ChatMessage>>>,
    /// Лимиты попыток входа и регистрации по IP-адресу.
    login_limiter: RateLimiter,
    register_limiter: RateLimiter,
    /// Лимиты сообщений и загрузок по аккаунту: смена IP не обходит их.
    message_limiter: RateLimiter<Uuid>,
    upload_limiter: RateLimiter<Uuid>,
    /// Неудачные входы по ключу имени: распределённый перебор одного аккаунта.
    failed_logins: RateLimiter<String>,
    /// Одноразовые билеты для WebSocket: хеш билета → сессия и срок.
    ws_tickets: Mutex<HashMap<String, WsTicket>>,
    /// Лимит выдач билетов на аккаунт.
    ws_ticket_limiter: RateLimiter<Uuid>,
    /// Хранилище файлов, прикрепляемых к сообщениям.
    uploads: Uploads,
    /// `None` — сервер работает только в памяти, на этом построены тесты.
    db: Option<ChatDb>,
    /// Момент запуска: отсчёт uptime для `/api/health`.
    started_at: Instant,
}

impl ChatState {
    /// Состояние только в памяти: используется тестами и внутри [`ChatState::persisted`].
    pub fn new() -> Self {
        Self::with_db(None, Uploads::default())
    }

    /// Восстанавливает публичную историю из PostgreSQL, чтобы перезапуск её не терял.
    pub async fn persisted(db: ChatDb, uploads: Uploads) -> Result<Self, sqlx::Error> {
        // Попутно чистим сессии, срок которых уже истёк.
        if let Err(error) = db.purge_expired_sessions().await {
            warn!(%error, "failed to purge expired sessions");
        }

        // Комнаты уже созданы при подключении к базе (`ensure_rooms`).
        let rooms = db.load_rooms().await?;
        let mut stored = HashMap::with_capacity(rooms.len());

        for room in &rooms {
            let (messages, _) = db
                .load_public_history(&room.id, HISTORY_LIMIT as i64)
                .await?;
            stored.insert(room.id.clone(), VecDeque::from(messages));
        }

        let state = Self::with_db(Some(db), uploads);

        *state.inner.rooms.write().await = rooms;
        *state.inner.history.write().await = stored;

        Ok(state)
    }

    /// Создаёт состояние с указанным (или отсутствующим) хранилищем.
    fn with_db(db: Option<ChatDb>, uploads: Uploads) -> Self {
        let (events, _) = broadcast::channel(BROADCAST_CAPACITY);

        // Без базы комнаты берутся из кода: тесты работают с тем же набором,
        // что и настоящий сервер.
        let rooms: Vec<Room> = DEFAULT_ROOMS
            .iter()
            .map(|(id, title)| Room {
                id: (*id).to_owned(),
                title: (*title).to_owned(),
            })
            .collect();
        let history = rooms
            .iter()
            .map(|room| (room.id.clone(), VecDeque::with_capacity(HISTORY_LIMIT)))
            .collect();

        Self {
            inner: Arc::new(Inner {
                events,
                room_events: RwLock::new(HashMap::with_capacity(DEFAULT_ROOMS.len())),
                users: RwLock::new(BTreeMap::new()),
                rooms: RwLock::new(rooms),
                history: RwLock::new(history),
                login_limiter: RateLimiter::new(LOGIN_QUOTA),
                register_limiter: RateLimiter::new(REGISTER_QUOTA),
                message_limiter: RateLimiter::new(MESSAGE_QUOTA),
                upload_limiter: RateLimiter::new(UPLOAD_QUOTA),
                failed_logins: RateLimiter::new(FAILED_LOGIN_QUOTA),
                ws_tickets: Mutex::new(HashMap::new()),
                ws_ticket_limiter: RateLimiter::new(WS_TICKET_QUOTA),
                uploads,
                db,
                started_at: Instant::now(),
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

    /// Сколько секунд сервер работает с момента запуска.
    pub fn uptime_seconds(&self) -> u64 {
        self.inner.started_at.elapsed().as_secs()
    }

    /// Занятые и свободные соединения пула; `None`, если базы нет.
    pub fn pool_stats(&self) -> Option<(u32, u32)> {
        self.inner.db.as_ref().map(ChatDb::pool_stats)
    }

    /// Регистрирует аккаунт и сразу открывает для него сессию.
    pub async fn register(
        &self,
        username: &crate::protocol::UserName,
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
            .find_account_by_key(&crate::protocol::name_key(username))
            .await
            .map_err(storage_error)?
        else {
            self.record_failed_login(username).await;
            return Err(AuthError::InvalidCredentials);
        };

        if !auth::verify_password_async(password.to_owned(), hash).await {
            self.record_failed_login(username).await;
            return Err(AuthError::InvalidCredentials);
        }

        // Успешный вход обнуляет счётчик: опечатки активного пользователя
        // не должны копиться до блокировки.
        self.clear_failed_logins(username).await;
        self.open_session(account).await
    }

    /// Возвращает аккаунт по токену действующей сессии.
    pub async fn authenticate(&self, token: &str) -> Option<Account> {
        self.authenticate_hash(&auth::token_hash(token)).await
    }

    /// То же, но по уже посчитанному SHA-256 токена.
    ///
    /// Нужно WebSocket-билету: он хранит хеш сессии, а не сам долгоживущий
    /// токен, поэтому повторно хешировать нечего.
    pub async fn authenticate_hash(&self, token_hash: &str) -> Option<Account> {
        let db = self.inner.db.as_ref()?;
        db.find_account_by_token(token_hash).await.ok().flatten()
    }

    /// Проверяет лимит попыток входа с указанного IP-адреса.
    pub async fn check_login_rate(&self, ip: IpAddr) -> Result<(), Duration> {
        self.inner.login_limiter.check(ip).await
    }

    /// Проверяет лимит попыток регистрации с указанного IP-адреса.
    pub async fn check_register_rate(&self, ip: IpAddr) -> Result<(), Duration> {
        self.inner.register_limiter.check(ip).await
    }

    /// Проверяет лимит сообщений аккаунта.
    ///
    /// Считается по `account_id`, а не по IP: иначе лимит обходится сменой
    /// адреса, а несколько окон одного аккаунта должны делить общий бюджет.
    pub async fn check_message_rate(&self, account_id: Uuid) -> Result<(), Duration> {
        self.inner.message_limiter.check(account_id).await
    }

    /// Проверяет лимит загрузок файлов аккаунта.
    pub async fn check_upload_rate(&self, account_id: Uuid) -> Result<(), Duration> {
        self.inner.upload_limiter.check(account_id).await
    }

    /// Сколько ждать, если по имени уже исчерпан лимит неудачных входов.
    ///
    /// Попытку не записывает и вызывается до проверки пароля: заблокированный
    /// аккаунт не должен тратить время на Argon2.
    pub async fn login_retry_after(&self, username: &str) -> Option<Duration> {
        let key = crate::protocol::name_key(username);
        self.inner.failed_logins.retry_after(&key).await
    }

    /// Запоминает неудачную попытку входа.
    async fn record_failed_login(&self, username: &str) {
        let key = crate::protocol::name_key(username);
        if self.inner.failed_logins.check(key).await.is_err() {
            warn!(%username, "too many failed logins for an account");
        }
    }

    /// Сбрасывает историю неудачных входов после успешного входа.
    async fn clear_failed_logins(&self, username: &str) {
        let key = crate::protocol::name_key(username);
        self.inner.failed_logins.reset(&key).await;
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
            .find_account_by_key(&crate::protocol::name_key(&account.username))
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
        self.end_other_sessions(account.id, &token_hash).await;

        Ok(())
    }

    /// Новый получатель общих событий: у каждого соединения он свой.
    pub fn subscribe(&self) -> broadcast::Receiver<ServerMessage> {
        self.inner.events.subscribe()
    }

    /// Отправляет общее событие всем соединениям.
    ///
    /// Для сообщений комнат используйте [`ChatState::broadcast_room`]: они не
    /// должны будить соединения других комнат.
    pub fn broadcast(&self, message: ServerMessage) {
        // Ошибка означает лишь то, что слушателей нет, — это нормально.
        let _ = self.inner.events.send(message);
    }

    /// Подписка на события одной комнаты.
    ///
    /// Канал создаётся при первом обращении, поэтому список комнат и карта
    /// каналов не могут разойтись.
    pub async fn subscribe_room(&self, room: &str) -> broadcast::Receiver<ServerMessage> {
        {
            let rooms = self.inner.room_events.read().await;
            if let Some(sender) = rooms.get(room) {
                return sender.subscribe();
            }
        }

        let mut rooms = self.inner.room_events.write().await;
        rooms
            .entry(room.to_owned())
            .or_insert_with(|| broadcast::channel(BROADCAST_CAPACITY).0)
            .subscribe()
    }

    /// Отправляет событие только подписчикам комнаты.
    pub async fn broadcast_room(&self, room: &str, message: ServerMessage) {
        let sender = self.inner.room_events.read().await.get(room).cloned();
        if let Some(sender) = sender {
            // Ошибка означает лишь то, что комнату никто не слушает.
            let _ = sender.send(message);
        }
    }

    /// Комнаты в порядке отображения.
    pub async fn rooms(&self) -> Vec<Room> {
        self.inner.rooms.read().await.clone()
    }

    /// Комната, в которой соединение начинает работу.
    pub async fn initial_room(&self) -> String {
        self.inner
            .rooms
            .read()
            .await
            .first()
            .map(|room| room.id.clone())
            .unwrap_or_else(|| "general".to_owned())
    }

    /// Существует ли такая комната.
    pub async fn is_room(&self, room: &str) -> bool {
        self.inner
            .rooms
            .read()
            .await
            .iter()
            .any(|candidate| candidate.id == room)
    }

    /// Хранилище файлов, прикрепляемых к сообщениям.
    pub fn uploads(&self) -> &Uploads {
        &self.inner.uploads
    }

    /// Максимальный размер одного вложения в байтах.
    pub fn max_upload_bytes(&self) -> u64 {
        self.inner.uploads.max_bytes()
    }

    /// Суммарный размер файлов в папке вложений.
    pub async fn uploads_bytes(&self) -> u64 {
        self.inner.uploads.usage_bytes().await.unwrap_or(0)
    }

    /// Удаляет файлы, для которых больше нет сообщения в базе.
    ///
    /// Возвращает число удалённых файлов. Без базы чистка не делается: иначе
    /// все файлы выглядели бы сиротами, а тесты работают с состоянием в памяти.
    /// Свежие файлы не трогаются: загрузка пишет файл раньше сообщения, и
    /// фоновая чистка не должна отбирать его у незавершённого запроса.
    pub async fn cleanup_orphans(&self) -> usize {
        let Some(db) = &self.inner.db else {
            return 0;
        };

        let known: HashSet<Uuid> = match db.attachment_ids().await {
            Ok(ids) => ids.into_iter().collect(),
            Err(error) => {
                warn!(%error, "failed to load attachment ids: cleanup skipped");
                return 0;
            }
        };

        let files = match self.inner.uploads.files().await {
            Ok(files) => files,
            Err(error) => {
                warn!(%error, "failed to list attachment files: cleanup skipped");
                return 0;
            }
        };

        let now = SystemTime::now();
        let mut removed = 0;

        for (id, modified) in files {
            if known.contains(&id) {
                continue;
            }

            if now.duration_since(modified).unwrap_or_default() < ORPHAN_GRACE {
                continue;
            }

            match self.inner.uploads.remove(id).await {
                Ok(()) => {
                    removed += 1;
                    tracing::debug!(message_id = %id, "removed an orphaned attachment");
                }
                Err(error) => {
                    warn!(%error, message_id = %id, "failed to remove an orphaned attachment");
                }
            }
        }

        removed
    }

    /// Метаданные файла сообщения с проверкой доступа аккаунта.
    ///
    /// `None` также приходит при ошибке базы: сообщаем клиенту «файл не
    /// найден» вместо утечки деталей хранилища.
    pub async fn attachment(&self, message_id: Uuid, account_id: Uuid) -> Option<StoredAttachment> {
        let db = self.inner.db.as_ref()?;
        db.find_attachment(message_id, account_id)
            .await
            .ok()
            .flatten()
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

// Тесты работают только с состоянием в памяти и не требуют PostgreSQL.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{ChatMessage, DirectMessage};
    use chrono::Utc;
    use tokio::sync::{broadcast::error::TryRecvError, mpsc};

    fn account(name: &str) -> Account {
        Account {
            id: Uuid::new_v4(),
            username: name.to_owned(),
        }
    }

    fn inbox() -> mpsc::Sender<ServerMessage> {
        mpsc::channel(PERSONAL_QUEUE).0
    }

    fn message(author: &str, value: &str) -> ChatMessage {
        ChatMessage {
            id: Uuid::new_v4(),
            room: "general".to_owned(),
            author: author.to_owned(),
            text: value.to_owned(),
            sent_at: Utc::now(),
            attachment: None,
        }
    }

    fn direct(from: &str, to: &str, value: &str) -> DirectMessage {
        DirectMessage {
            id: Uuid::new_v4(),
            from: from.to_owned(),
            to: to.to_owned(),
            text: value.to_owned(),
            sent_at: Utc::now(),
            attachment: None,
        }
    }

    #[tokio::test]
    async fn allows_multiple_windows_for_one_account() {
        let state = ChatState::new();
        let anna = account("Анна");
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();

        let (users, is_first) = state.join(first, &anna, "token-a", inbox()).await;
        assert!(is_first);
        assert_eq!(users, ["Анна"]);

        // Второе окно того же аккаунта не создаёт вторую строку в списке.
        let (users, is_first) = state.join(second, &anna, "token-a", inbox()).await;
        assert!(!is_first);
        assert_eq!(users, ["Анна"]);

        // Закрытие одного окна не считается выходом из чата.
        assert_eq!(state.leave(first).await, Some(("Анна".to_owned(), true)));
        assert_eq!(state.leave(second).await, Some(("Анна".to_owned(), false)));
    }

    #[tokio::test]
    async fn ends_only_sessions_with_another_token() {
        let state = ChatState::new();
        let anna = account("Анна");

        let (own_inbox, mut own_messages) = mpsc::channel(PERSONAL_QUEUE);
        state
            .join(Uuid::new_v4(), &anna, "token-a", own_inbox)
            .await;

        let (other_inbox, mut other_messages) = mpsc::channel(PERSONAL_QUEUE);
        state
            .join(Uuid::new_v4(), &anna, "token-b", other_inbox)
            .await;

        state.end_other_sessions(anna.id, "token-a").await;

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
        let apple = account("яблоко");
        let banana = account("Банан");
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();

        state.join(first, &apple, "token-a", inbox()).await;
        state.join(second, &banana, "token-b", inbox()).await;
        assert_eq!(state.users().await, ["Банан", "яблоко"]);

        assert_eq!(state.leave(first).await, Some(("яблоко".to_owned(), false)));
        assert_eq!(state.users().await, ["Банан"]);
        assert_eq!(state.leave(first).await, None);
    }

    #[tokio::test]
    async fn delivers_direct_messages_only_to_the_recipient() {
        let state = ChatState::new();
        let alice = account("Алиса");
        let bob = account("Боб");

        state.join(Uuid::new_v4(), &alice, "token-a", inbox()).await;
        let (bob_inbox, mut bob_messages) = mpsc::channel(PERSONAL_QUEUE);
        state.join(Uuid::new_v4(), &bob, "token-b", bob_inbox).await;

        let connections = state.connections("боб").await;
        assert_eq!(connections.len(), 1);
        assert_eq!(connections[0].name, "Боб");
        assert_eq!(connections[0].account_id, bob.id);

        let message = direct("Алиса", "Боб", "привет");
        assert!(
            state
                .send_to(connections[0].id, ServerMessage::Direct { message })
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
    async fn starts_with_default_rooms() {
        let state = ChatState::new();
        let rooms = state.rooms().await;

        assert_eq!(
            rooms
                .iter()
                .map(|room| room.id.as_str())
                .collect::<Vec<_>>(),
            ["general", "rust", "random"]
        );
        assert_eq!(state.initial_room().await, "general");
        assert!(state.is_room("rust").await);
        assert!(!state.is_room("unknown").await);
    }

    #[tokio::test]
    async fn history_is_bounded() {
        let state = ChatState::new();

        // Публикуем больше сообщений, чем помещается в историю.
        for index in 0..(HISTORY_LIMIT + 5) {
            state
                .publish(message("bot", &index.to_string()), Uuid::new_v4())
                .await;
        }

        let history = state.history_snapshot("general").await;
        assert_eq!(history.len(), HISTORY_LIMIT);
        assert_eq!(history.first().unwrap().text, "5");
        assert_eq!(
            history.last().unwrap().text,
            (HISTORY_LIMIT + 4).to_string()
        );
    }

    #[tokio::test]
    async fn keeps_rooms_separate() {
        let state = ChatState::new();

        state
            .publish(message("bot", "в общей"), Uuid::new_v4())
            .await;

        let mut rust = message("bot", "в Rust");
        rust.room = "rust".to_owned();
        state.publish(rust, Uuid::new_v4()).await;

        assert_eq!(state.history_snapshot("general").await[0].text, "в общей");
        assert_eq!(state.history_snapshot("rust").await[0].text, "в Rust");
        assert!(state.history_snapshot("random").await.is_empty());
    }

    #[tokio::test]
    async fn room_broadcasts_reach_only_that_room() {
        let state = ChatState::new();
        let mut general = state.subscribe_room("general").await;
        let mut rust = state.subscribe_room("rust").await;

        state
            .publish(message("bot", "только в общую"), Uuid::new_v4())
            .await;

        // Подписчик общей комнаты получил сообщение.
        match general.recv().await {
            Ok(ServerMessage::Message { message }) => {
                assert_eq!(message.text, "только в общую");
            }
            other => panic!("unexpected event: {other:?}"),
        }

        // Подписчик Rust не должен просыпаться на чужих сообщениях.
        assert!(matches!(rust.try_recv(), Err(TryRecvError::Empty)));
    }

    #[tokio::test]
    async fn pages_memory_history_from_the_oldest_message() {
        let state = ChatState::new();

        for index in 0..60 {
            state
                .publish(message("bot", &index.to_string()), Uuid::new_v4())
                .await;
        }

        let snapshot = state.history_snapshot("general").await;
        let newest = snapshot.last().unwrap();

        // Перед последним сообщением лежит 59 штук: в страницу входят 50
        // ближайших, а более старые ещё остаются.
        let (page, has_more) = state.history_page("general", newest.id).await;
        assert_eq!(page.len(), PAGE_LIMIT as usize);
        assert!(has_more);
        assert_eq!(page.first().unwrap().text, "9");
        assert_eq!(page.last().unwrap().text, "58");

        // Следующая страница забирает остаток.
        let (older, has_more) = state
            .history_page("general", page.first().unwrap().id)
            .await;
        assert_eq!(older.len(), 9);
        assert!(!has_more);
        assert_eq!(older.first().unwrap().text, "0");

        // Неизвестное сообщение не открывает бесконечную подгрузку.
        let (empty, has_more) = state.history_page("general", Uuid::new_v4()).await;
        assert!(empty.is_empty());
        assert!(!has_more);
    }

    #[tokio::test]
    async fn enforces_message_rate_per_account() {
        let state = ChatState::new();
        let alice = Uuid::new_v4();
        let bob = Uuid::new_v4();

        // Щедрый на вид лимит исчерпывается ровно на одиннадцатом сообщении.
        for _ in 0..10 {
            assert!(state.check_message_rate(alice).await.is_ok());
        }

        let wait = state
            .check_message_rate(alice)
            .await
            .expect_err("лимит должен сработать");
        assert!(wait.as_secs() <= 10);

        // Второй аккаунт не страдает от сообщений первого.
        assert!(state.check_message_rate(bob).await.is_ok());
    }

    #[tokio::test]
    async fn blocks_an_account_after_failed_logins() {
        let state = ChatState::new();

        for _ in 0..FAILED_LOGIN_QUOTA.attempts {
            state.record_failed_login("Анна").await;
        }

        // Имя сравнивается без учёта регистра, как и при входе.
        assert!(state.login_retry_after("анна").await.is_some());
        assert!(state.login_retry_after("АННА").await.is_some());

        state.clear_failed_logins("анна").await;
        assert!(state.login_retry_after("Анна").await.is_none());
    }
}
