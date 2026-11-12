//! Присутствие: кто сейчас онлайн, в каких окнах и куда доставлять личные
//! сообщения.
//!
//! Один аккаунт может быть открыт в нескольких окнах. Наружу имя показывается
//! один раз, но личное сообщение уходит в каждое окно получателя.

use std::collections::BTreeMap;

use tokio::sync::mpsc;
use tracing::warn;
use uuid::Uuid;

use super::ChatState;
use crate::auth::Account;
use crate::protocol::{ServerMessage, name_key};

/// Сколько личных сообщений может ждать получателя, прежде чем доставка не удастся.
pub const PERSONAL_QUEUE: usize = 64;

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

/// Данные окна, которое держит WebSocket-соединение.
pub(super) struct User {
    /// Аккаунт, которому принадлежит окно. Именно он, а не имя, связывает
    /// соединение с сохранёнными сообщениями.
    pub(super) account_id: Uuid,
    pub(super) name: String,
    /// Хеш токена сессии, по которому открыто это окно.
    ///
    /// Нужен, чтобы при смене пароля отключить чужие устройства, но оставить
    /// окна, работающие с тем же токеном.
    pub(super) token_hash: String,
    /// Личные сообщения кладутся сюда, а не в broadcast-канал комнаты,
    /// чтобы их содержимое не уходило другим соединениям.
    pub(super) inbox: mpsc::Sender<ServerMessage>,
}

impl ChatState {
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

    /// Сколько WebSocket-окон открыто прямо сейчас (не уникальных имён).
    pub async fn window_count(&self) -> usize {
        self.inner.users.read().await.len()
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

    /// Все соединения аккаунта (в отличие от [`ChatState::connections`] — по имени).
    pub(super) async fn connections_by_account(&self, account_id: Uuid) -> Vec<Connection> {
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
}

/// Собирает уникальные имена участников и сортирует их без учёта регистра.
fn sorted_names(users: &BTreeMap<Uuid, User>) -> Vec<String> {
    let mut names: Vec<String> = users.values().map(|user| user.name.clone()).collect();
    names.sort_by_cached_key(|name| name_key(name));
    names.dedup_by(|first, second| name_key(first) == name_key(second));
    names
}
