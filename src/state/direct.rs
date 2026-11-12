//! Личные переписки: сохранение, доставка во все окна участников и отметки
//! о прочтении.
//!
//! Личные сообщения не попадают в broadcast-каналы комнат: они идут только
//! в персональные очереди тех аккаунтов, которые участвуют в диалоге.

use tracing::warn;
use uuid::Uuid;

use super::ChatState;
use crate::db::THREAD_REPLAY;
use crate::protocol::{DirectMessage, DirectThread, ServerMessage, name_key};

impl ChatState {
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
}
