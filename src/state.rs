//! Общее состояние чата: участники онлайн, история и рассылка событий.
//!
//! Состояние дешево клонируется: внутри лежит `Arc<Inner>`, поэтому один и тот
//! же экземпляр используют все WebSocket-соединения и HTTP-обработчики.

use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    net::IpAddr,
    sync::Arc,
    time::Duration,
};

use tokio::sync::{RwLock, broadcast, mpsc};
use tracing::warn;
use uuid::Uuid;

use crate::auth::{self, Account, AuthError, AuthSession};
use crate::db::{ChatDb, StoredAttachment, THREAD_REPLAY};
use crate::protocol::{
    ChatMessage, DEFAULT_ROOMS, DirectMessage, DirectThread, Room, ServerMessage, UserName,
    name_key,
};
use crate::rate_limit::{LOGIN_QUOTA, REGISTER_QUOTA, RateLimiter};
use crate::uploads::Uploads;

/// Сколько последних сообщений комнаты получает новый клиент при подключении.
pub const HISTORY_LIMIT: usize = 100;

/// Сколько сообщений отдаётся за один запрос более старой истории.
pub const PAGE_LIMIT: i64 = 50;

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
    /// Доступные комнаты в порядке отображения.
    rooms: RwLock<Vec<Room>>,
    /// Ограниченная история каждой комнаты (не более [`HISTORY_LIMIT`] сообщений).
    history: RwLock<HashMap<String, VecDeque<ChatMessage>>>,
    /// Лимиты попыток входа и регистрации по IP-адресу.
    login_limiter: RateLimiter,
    register_limiter: RateLimiter,
    /// Хранилище файлов, прикрепляемых к сообщениям.
    uploads: Uploads,
    /// `None` — сервер работает только в памяти, на этом построены тесты.
    db: Option<ChatDb>,
}

struct User {
    /// Аккаунт, которому принадлежит окно. Именно он, а не имя, связывает
    /// соединение с сохранёнными сообщениями.
    account_id: Uuid,
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

/// Одно открытое окно пользователя.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connection {
    /// Идентификатор WebSocket-соединения.
    pub id: Uuid,
    /// Имя аккаунта в написании при регистрации.
    pub name: String,
    /// Идентификатор аккаунта: по нему сохраняются сообщения.
    pub account_id: Uuid,
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
                users: RwLock::new(BTreeMap::new()),
                rooms: RwLock::new(rooms),
                history: RwLock::new(history),
                login_limiter: RateLimiter::new(LOGIN_QUOTA),
                register_limiter: RateLimiter::new(REGISTER_QUOTA),
                uploads,
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
        self.end_other_sessions(account.id, &token_hash).await;

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
        account: &Account,
        token_hash: &str,
        inbox: mpsc::Sender<ServerMessage>,
    ) -> (Vec<String>, bool) {
        let mut users = self.inner.users.write().await;

        // Первым считается окно, которое открыло аккаунт впервые: только оно
        // объявляет вход остальным.
        let first = !users.values().any(|user| user.account_id == account.id);

        users.insert(
            id,
            User {
                account_id: account.id,
                name: account.username.clone(),
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

        let still_online = users
            .values()
            .any(|other| other.account_id == user.account_id);

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
    pub async fn connections(&self, name: &str) -> Vec<Connection> {
        let key = name_key(name);
        let users = self.inner.users.read().await;

        users
            .iter()
            .filter(|(_, user)| name_key(&user.name) == key)
            .map(|(id, user)| Connection {
                id: *id,
                name: user.name.clone(),
                account_id: user.account_id,
            })
            .collect()
    }

    /// Завершает все соединения аккаунта, открытые с других токенов.
    ///
    /// Вызывается после смены пароля: окна с тем же токеном остаются в чате,
    /// а чужие устройства получают `session_ended` и закрывают соединение.
    pub async fn end_other_sessions(&self, account_id: Uuid, keep_token_hash: &str) {
        // Собираем отправители под блокировкой, а отправляем уже без неё.
        let targets: Vec<mpsc::Sender<ServerMessage>> = self
            .inner
            .users
            .read()
            .await
            .values()
            .filter(|user| user.account_id == account_id && user.token_hash != keep_token_hash)
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

    /// Снимок последних сообщений комнаты из памяти.
    pub async fn history_snapshot(&self, room: &str) -> Vec<ChatMessage> {
        self.inner
            .history
            .read()
            .await
            .get(room)
            .map(|messages| messages.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Последние сообщения комнаты и признак наличия более старых.
    pub async fn initial_history(&self, room: &str) -> (Vec<ChatMessage>, bool) {
        if let Some(db) = &self.inner.db {
            match db.load_public_history(room, HISTORY_LIMIT as i64).await {
                Ok(page) => return page,
                Err(error) => {
                    warn!(%error, %room, "failed to load room history from PostgreSQL");
                }
            }
        }

        // Без базы или при её ошибке отдаём то, что уже есть в памяти.
        // Признак `has_more` в этом случае неизвестен, поэтому `false`.
        (self.history_snapshot(room).await, false)
    }

    /// Порция истории комнаты, которая старше сообщения `before`.
    ///
    /// Если сообщение не найдено, возвращается пустая страница: клиент
    /// перестанет показывать кнопку подгрузки.
    pub async fn history_page(&self, room: &str, before: Uuid) -> (Vec<ChatMessage>, bool) {
        if let Some(db) = &self.inner.db {
            match db.load_public_page(room, Some(before), PAGE_LIMIT).await {
                Ok(page) => return page,
                Err(error) => {
                    warn!(%error, %room, "failed to load a history page from PostgreSQL");
                }
            }
        }

        // Запасной путь для состояния в памяти: ищем сообщение в очереди
        // и берём до PAGE_LIMIT элементов перед ним.
        let history = self.inner.history.read().await;
        let Some(messages) = history.get(room) else {
            return (Vec::new(), false);
        };
        let Some(index) = messages.iter().position(|message| message.id == before) else {
            return (Vec::new(), false);
        };

        let start = index.saturating_sub(PAGE_LIMIT as usize);
        let page = messages.range(start..index).cloned().collect();
        (page, start > 0)
    }

    /// Сохраняет сообщение в истории комнаты, пишет его в базу и рассылает.
    ///
    /// `author_id` — аккаунт автора: в PostgreSQL сообщение ссылается на него,
    /// а не на имя.
    ///
    /// Блокировка истории удерживается и во время рассылки, чтобы порядок в
    /// истории совпадал с порядком доставки, даже если пишут несколько клиентов сразу.
    pub async fn publish(&self, message: ChatMessage, author_id: Uuid) {
        {
            let mut history = self.inner.history.write().await;
            let room = history
                .entry(message.room.clone())
                .or_insert_with(|| VecDeque::with_capacity(HISTORY_LIMIT));

            // История ограничена: самое старое сообщение вытесняется.
            if room.len() == HISTORY_LIMIT {
                room.pop_front();
            }
            room.push_back(message.clone());

            self.broadcast(ServerMessage::Message {
                message: message.clone(),
            });
        }

        // В базу пишем вне блокировки: порядок после перезапуска задаёт `seq`,
        // а в текущей сессии порядок уже определён выше.
        if let Some(db) = &self.inner.db
            && let Err(error) = db.save_public(&message, author_id).await
        {
            warn!(%error, "failed to store a public message");
        }
    }

    /// Сохраняет личное сообщение до его доставки, чтобы клиент никогда не получил
    /// то, чего ещё нет в базе.
    pub async fn save_direct(&self, message: &DirectMessage, sender_id: Uuid, recipient_id: Uuid) {
        if let Some(db) = &self.inner.db
            && let Err(error) = db.save_direct(message, sender_id, recipient_id).await
        {
            warn!(%error, "failed to store a private message");
        }
    }

    /// Сохраняет личное сообщение и кладёт его в очереди всех окон обоих участников.
    ///
    /// Соединения ищутся по `account_id`, поэтому имени не нужно совпадать в
    /// регистре, а копия автора доставляется в каждое его окно. Возвращает,
    /// сколько окон получателя приняли сообщение: если ноль, получатель уже
    /// отключился и сообщение стоит считать недоставленным.
    pub async fn deliver_direct(
        &self,
        message: &DirectMessage,
        sender_id: Uuid,
        recipient_id: Uuid,
    ) -> usize {
        // Сначала запись: восстановленный диалог не должен потерять сообщение,
        // которое клиент уже получил.
        self.save_direct(message, sender_id, recipient_id).await;

        let event = ServerMessage::Direct {
            message: message.clone(),
        };

        let mut delivered_recipient = 0;
        for connection in [
            self.connections_by_account(sender_id).await,
            self.connections_by_account(recipient_id).await,
        ]
        .concat()
        {
            if self.send_to(connection.id, event.clone()).await
                && connection.account_id == recipient_id
            {
                delivered_recipient += 1;
            }
        }

        delivered_recipient
    }

    /// Все соединения аккаунта (в отличие от [`ChatState::connections`] — по имени).
    async fn connections_by_account(&self, account_id: Uuid) -> Vec<Connection> {
        let users = self.inner.users.read().await;

        users
            .iter()
            .filter(|(_, user)| user.account_id == account_id)
            .map(|(id, user)| Connection {
                id: *id,
                name: user.name.clone(),
                account_id: user.account_id,
            })
            .collect()
    }

    /// Личные переписки аккаунта из базы.
    ///
    /// Если собеседник сейчас онлайн, имя приводится к написанию, под которым он вошёл.
    pub async fn direct_threads(&self, account_id: Uuid) -> Vec<DirectThread> {
        let Some(db) = &self.inner.db else {
            return Vec::new();
        };

        let threads = match db.load_direct_threads(account_id, THREAD_REPLAY).await {
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

    /// Запоминает, что аккаунт прочитал всё, что прислал собеседник.
    pub async fn mark_read(&self, account_id: Uuid, peer: &str) {
        let peer_key = name_key(peer);

        if let Some(db) = &self.inner.db
            && let Err(error) = db.mark_read(account_id, &peer_key).await
        {
            warn!(%error, "failed to store a read mark");
        }
    }

    /// Хранилище файлов, прикрепляемых к сообщениям.
    pub fn uploads(&self) -> &Uploads {
        &self.inner.uploads
    }

    /// Максимальный размер одного вложения в байтах.
    pub fn max_upload_bytes(&self) -> u64 {
        self.inner.uploads.max_bytes()
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
}
