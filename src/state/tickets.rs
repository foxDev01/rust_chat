//! Одноразовые билеты для входа в WebSocket.
//!
//! Долгоживущий токен сессии в query-строке оседает в access-логах прокси и
//! истории браузера, поэтому клиент сначала меняет его на короткоживущий билет
//! авторизованным HTTP-запросом, а сокет открывает уже с билетом. Сервер хранит
//! только SHA-256 билета: дамп памяти процесса не даёт готовых значений.

use std::time::{Duration, Instant};

use uuid::Uuid;

use super::ChatState;
use crate::auth;

/// Сколько живёт билет: хватает на один запрос апгрейда сразу после выдачи.
pub const TICKET_TTL: Duration = Duration::from_secs(30);

/// Выданный и ещё не использованный билет.
pub(super) struct WsTicket {
    /// Хеш токена сессии, по которому выдан билет.
    token_hash: String,
    /// Момент, после которого билет не принимается.
    expires_at: Instant,
}

impl ChatState {
    /// Выдаёт одноразовый билет для WebSocket.
    ///
    /// `Err(ожидание)` означает, что аккаунт исчерпал лимит выдач. Хранится
    /// только хеш билета, а сам билет возвращается клиенту один раз.
    pub async fn issue_ws_ticket(
        &self,
        account_id: Uuid,
        session_token: &str,
    ) -> Result<String, Duration> {
        self.inner.ws_ticket_limiter.check(account_id).await?;

        let ticket = auth::new_token();
        let now = Instant::now();
        let mut tickets = self.inner.ws_tickets.lock().await;

        // Карта маленькая (билет живёт секунды), но просроченные записи лучше
        // убирать при каждой выдаче, а не копить до рестарта.
        tickets.retain(|_, stored| stored.expires_at > now);
        tickets.insert(
            auth::token_hash(&ticket),
            WsTicket {
                token_hash: auth::token_hash(session_token),
                expires_at: now + TICKET_TTL,
            },
        );

        Ok(ticket)
    }

    /// Забирает билет и возвращает хеш сессии, по которой он выдан.
    ///
    /// Билет удаляется даже если уже истёк: повторно им воспользоваться нельзя.
    pub async fn consume_ws_ticket(&self, ticket: &str) -> Option<String> {
        let stored = self
            .inner
            .ws_tickets
            .lock()
            .await
            .remove(&auth::token_hash(ticket))?;

        (stored.expires_at > Instant::now()).then_some(stored.token_hash)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn ticket_works_once_and_is_bound_to_the_session() {
        let state = ChatState::new();
        let ticket = state
            .issue_ws_ticket(Uuid::new_v4(), "session-token")
            .await
            .expect("первая выдача укладывается в лимит");

        assert_eq!(
            state.consume_ws_ticket(&ticket).await,
            Some(auth::token_hash("session-token"))
        );
        // Второй апгрейд с тем же билетом уже не пройдёт.
        assert!(state.consume_ws_ticket(&ticket).await.is_none());
    }

    #[tokio::test]
    async fn expired_ticket_is_rejected() {
        let state = ChatState::new();
        let ticket = auth::new_token();

        // Просроченный билет кладём напрямую: ждать TICKET_TTL в тесте незачем.
        state.inner.ws_tickets.lock().await.insert(
            auth::token_hash(&ticket),
            WsTicket {
                token_hash: "session-hash".to_owned(),
                expires_at: Instant::now(),
            },
        );

        assert!(state.consume_ws_ticket(&ticket).await.is_none());
    }
}
