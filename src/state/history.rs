//! Публичная история комнат: ограниченный кэш в памяти, постраничная
//! подгрузка из PostgreSQL и рассылка новых сообщений.
//!
//! Каждая комната живёт в отдельном `broadcast`-канале, поэтому соединение
//! получает только события своей комнаты и не тратит время на фильтрацию
//! чужих сообщений.

use std::collections::VecDeque;

use tracing::warn;
use uuid::Uuid;

use super::ChatState;
use crate::protocol::{ChatMessage, ServerMessage};

/// Сколько последних сообщений комнаты получает новый клиент при подключении.
pub const HISTORY_LIMIT: usize = 100;

/// Сколько сообщений отдаётся за один запрос более старой истории.
pub const PAGE_LIMIT: i64 = 50;

impl ChatState {
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

            // Рассылаем только подписчикам этой комнаты.
            self.broadcast_room(
                &message.room,
                ServerMessage::Message {
                    message: message.clone(),
                },
            )
            .await;
        }

        // В базу пишем вне блокировки: порядок после перезапуска задаёт `seq`,
        // а в текущей сессии порядок уже определён выше.
        if let Some(db) = &self.inner.db
            && let Err(error) = db.save_public(&message, author_id).await
        {
            warn!(%error, "failed to store a public message");
        }
    }
}
