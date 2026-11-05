//! Хранилище PostgreSQL: публичная история, личные переписки и отметки о прочтении.
//!
//! Схема описана в `migrations/` и применяется при каждом старте; все команды
//! идемпотентны, поэтому сервер можно запускать на готовой базе. Сообщения
//! ссылаются на аккаунты по UUID: имя пользователя используется только для
//! показа и подтягивается из `accounts` при чтении.

use std::collections::BTreeMap;
use std::error::Error;
use std::time::Duration;

use chrono::{DateTime, Utc};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{PgPool, Row};
use tracing::info;
use uuid::Uuid;

use crate::auth::Account;
use crate::protocol::{ChatMessage, DirectMessage, DirectThread, UserName, name_key};

/// Тайм-аут установки соединения с базой.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Максимальное число соединений в пуле.
const MAX_CONNECTIONS: u32 = 5;

/// Сколько последних сообщений одного диалога восстанавливается при входе пользователя.
pub const THREAD_REPLAY: i64 = 100;

/// Пул подключений к PostgreSQL.
pub struct ChatDb {
    pool: PgPool,
}

impl ChatDb {
    /// Подключается к базе и применяет схему чата.
    ///
    /// Ошибки оборачиваются в понятные сообщения, чтобы при старте было видно,
    /// что именно пошло не так: строка подключения, доступность сервера или схема.
    pub async fn connect(url: &str) -> Result<Self, Box<dyn Error + Send + Sync>> {
        let options: PgConnectOptions = url
            .parse()
            .map_err(|error| format!("CHAT_DB_URL разобран с ошибкой: {error}"))?;

        let pool = PgPoolOptions::new()
            .max_connections(MAX_CONNECTIONS)
            .acquire_timeout(CONNECT_TIMEOUT)
            .connect_with(options.application_name("rust-chat"))
            .await
            .map_err(|error| {
                format!(
                    "PostgreSQL недоступен по CHAT_DB_URL ({url}): {error}. \
                     Проверьте, что сервер запущен и база создана"
                )
            })?;

        // Схема применяется по порядку номеров; повторный запуск ничего не ломает.
        // 002 переводит базы, созданные до появления account_id, на UUID.
        for migration in [
            include_str!("../migrations/001_init.sql"),
            include_str!("../migrations/002_account_ids.sql"),
        ] {
            sqlx::raw_sql(migration)
                .execute(&pool)
                .await
                .map_err(|error| {
                    format!("Не удалось применить схему чата в PostgreSQL: {error}")
                })?;
        }

        info!("connected to PostgreSQL");
        Ok(Self { pool })
    }

    /// Закрывает все соединения пула.
    pub async fn close(&self) {
        self.pool.close().await;
    }

    /// Создаёт аккаунт.
    ///
    /// Возвращает `None`, если имя уже занято (проверка без учёта регистра
    /// выполняется уникальным индексом `username_key`).
    pub async fn create_account(
        &self,
        username: &UserName,
        password_hash: &str,
    ) -> Result<Option<Account>, sqlx::Error> {
        let row = sqlx::query(
            "INSERT INTO accounts (id, username, username_key, password_hash)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (username_key) DO NOTHING
             RETURNING id, username",
        )
        .bind(Uuid::new_v4())
        .bind(username.as_str())
        .bind(name_key(username.as_str()))
        .bind(password_hash)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|row| Account {
            id: row.get("id"),
            username: row.get("username"),
        }))
    }

    /// Ищет аккаунт по ключу имени; вместе с аккаунтом возвращается хеш пароля.
    pub async fn find_account_by_key(
        &self,
        username_key: &str,
    ) -> Result<Option<(Account, String)>, sqlx::Error> {
        let row =
            sqlx::query("SELECT id, username, password_hash FROM accounts WHERE username_key = $1")
                .bind(username_key)
                .fetch_optional(&self.pool)
                .await?;

        Ok(row.map(|row| {
            (
                Account {
                    id: row.get("id"),
                    username: row.get("username"),
                },
                row.get("password_hash"),
            )
        }))
    }

    /// Сохраняет новую сессию (в базе хранится только хеш токена).
    pub async fn create_session(
        &self,
        account_id: Uuid,
        token_hash: &str,
        expires_at: DateTime<Utc>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO sessions (token_hash, account_id, expires_at)
             VALUES ($1, $2, $3)",
        )
        .bind(token_hash)
        .bind(account_id)
        .bind(expires_at)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Находит владельца действующей сессии по хешу токена.
    pub async fn find_account_by_token(
        &self,
        token_hash: &str,
    ) -> Result<Option<Account>, sqlx::Error> {
        let row = sqlx::query(
            "SELECT a.id, a.username
             FROM sessions s
             JOIN accounts a ON a.id = s.account_id
             WHERE s.token_hash = $1 AND s.expires_at > now()",
        )
        .bind(token_hash)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|row| Account {
            id: row.get("id"),
            username: row.get("username"),
        }))
    }

    /// Удаляет сессию (выход из аккаунта).
    pub async fn delete_session(&self, token_hash: &str) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM sessions WHERE token_hash = $1")
            .bind(token_hash)
            .execute(&self.pool)
            .await?;

        Ok(())
    }

    /// Заменяет хеш пароля аккаунта.
    pub async fn update_password(
        &self,
        account_id: Uuid,
        password_hash: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE accounts SET password_hash = $2 WHERE id = $1")
            .bind(account_id)
            .bind(password_hash)
            .execute(&self.pool)
            .await?;

        Ok(())
    }

    /// Отзывает все сессии аккаунта, кроме текущей.
    ///
    /// Нужно после смены пароля: если токен успели украсть, он перестанет
    /// работать, а устройство, на котором пароль меняли, останется в чате.
    pub async fn delete_other_sessions(
        &self,
        account_id: Uuid,
        keep_token_hash: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM sessions WHERE account_id = $1 AND token_hash <> $2")
            .bind(account_id)
            .bind(keep_token_hash)
            .execute(&self.pool)
            .await?;

        Ok(())
    }

    /// Удаляет истёкшие сессии, чтобы таблица не росла бесконечно.
    pub async fn purge_expired_sessions(&self) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM sessions WHERE expires_at <= now()")
            .execute(&self.pool)
            .await?;

        Ok(())
    }

    /// Сохраняет сообщение общего канала от имени аккаунта `author_id`.
    pub async fn save_public(
        &self,
        message: &ChatMessage,
        author_id: Uuid,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO chat_messages (id, author_id, body, sent_at)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(message.id)
        .bind(author_id)
        .bind(&message.text)
        .bind(message.sent_at)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Последние `limit` публичных сообщений, самые старые первыми.
    pub async fn load_public_history(&self, limit: i64) -> Result<Vec<ChatMessage>, sqlx::Error> {
        // В истории хранится только UUID автора, имя подтягиваем из аккаунта.
        // Берём хвост по убыванию seq, а затем разворачиваем его.
        let rows = sqlx::query(
            "SELECT m.id, a.username AS author, m.body, m.sent_at
             FROM chat_messages m
             JOIN accounts a ON a.id = m.author_id
             ORDER BY m.seq DESC
             LIMIT $1",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .rev()
            .map(|row| ChatMessage {
                id: row.get("id"),
                author: row.get("author"),
                text: row.get("body"),
                sent_at: row.get("sent_at"),
            })
            .collect())
    }

    /// Сохраняет личное сообщение с идентификаторами отправителя и получателя.
    pub async fn save_direct(
        &self,
        message: &DirectMessage,
        sender_id: Uuid,
        recipient_id: Uuid,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO direct_messages (id, sender_id, recipient_id, body, sent_at)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(message.id)
        .bind(sender_id)
        .bind(recipient_id)
        .bind(&message.text)
        .bind(message.sent_at)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Личные переписки аккаунта: в каждой — последние сообщения,
    /// самые старые первыми, а список диалогов отсортирован по свежести.
    pub async fn load_direct_threads(
        &self,
        account_id: Uuid,
        per_thread: i64,
    ) -> Result<Vec<DirectThread>, sqlx::Error> {
        // Для каждого сообщения определяем собеседника (`peer_id`), затем
        // нумеруем сообщения внутри диалога от новых к старым и оставляем
        // только `per_thread` последних. Имена участников берём из accounts.
        let messages = sqlx::query(
            "WITH involved AS (
                 SELECT seq, id, sender_id, recipient_id, body, sent_at,
                        CASE WHEN sender_id = $1 THEN recipient_id ELSE sender_id END AS peer_id
                 FROM direct_messages
                 WHERE sender_id = $1 OR recipient_id = $1
             ),
             ranked AS (
                 SELECT *, ROW_NUMBER() OVER (PARTITION BY peer_id ORDER BY seq DESC) AS newest_first
                 FROM involved
             )
             SELECT r.seq, r.id, s.username AS sender, rc.username AS recipient,
                    r.body, r.sent_at, r.peer_id, p.username AS peer
             FROM ranked r
             JOIN accounts s ON s.id = r.sender_id
             JOIN accounts rc ON rc.id = r.recipient_id
             JOIN accounts p ON p.id = r.peer_id
             WHERE r.newest_first <= $2
             ORDER BY r.seq",
        )
        .bind(account_id)
        .bind(per_thread)
        .fetch_all(&self.pool)
        .await?;

        // Непрочитанные считаем как сообщения, пришедшие после отметки `direct_reads`.
        let unread_rows = sqlx::query(
            "SELECT m.sender_id AS peer_id, COUNT(*) AS unread
             FROM direct_messages m
             WHERE m.recipient_id = $1
               AND m.seq > COALESCE(
                   (SELECT r.last_seq FROM direct_reads r
                    WHERE r.user_id = $1 AND r.peer_id = m.sender_id),
                   0)
             GROUP BY m.sender_id",
        )
        .bind(account_id)
        .fetch_all(&self.pool)
        .await?;

        /// Промежуточная запись диалога при сборке результата.
        #[derive(Default)]
        struct Thread {
            peer: String,
            messages: Vec<DirectMessage>,
            unread: i64,
        }

        let mut threads: BTreeMap<Uuid, Thread> = BTreeMap::new();

        // Складываем сообщения в диалоги по идентификатору собеседника.
        for row in messages {
            let peer_id: Uuid = row.get("peer_id");
            let peer: String = row.get("peer");

            let entry = threads.entry(peer_id).or_default();
            entry.peer = peer;
            entry.messages.push(DirectMessage {
                id: row.get("id"),
                from: row.get("sender"),
                to: row.get("recipient"),
                text: row.get("body"),
                sent_at: row.get("sent_at"),
            });
        }

        // Добавляем счётчики непрочитанных; диалог мог не попасть в выборку сообщений.
        for row in unread_rows {
            let peer_id: Uuid = row.get("peer_id");
            let unread: i64 = row.get("unread");

            threads.entry(peer_id).or_default().unread = unread;
        }

        let mut result: Vec<DirectThread> = threads
            .into_values()
            .filter(|thread| !thread.messages.is_empty())
            .map(|thread| DirectThread {
                peer: thread.peer,
                messages: thread.messages,
                unread: thread.unread,
            })
            .collect();

        // Свежие диалоги — первыми.
        result.sort_by_key(|thread| {
            thread
                .messages
                .last()
                .map(|message| message.sent_at)
                .unwrap_or_else(chrono::Utc::now)
        });
        result.reverse();

        Ok(result)
    }

    /// Запоминает, что `account_id` прочитал всё, что `peer` отправил к этому моменту.
    ///
    /// Собеседник ищется по ключу имени: клиент знает только имя из списка
    /// участников, а отметка хранится уже по UUID.
    pub async fn mark_read(&self, account_id: Uuid, peer_key: &str) -> Result<(), sqlx::Error> {
        // Отметка ставится на максимальный seq в диалоге в обе стороны.
        // Счётчик прочитанного может только расти.
        sqlx::query(
            "INSERT INTO direct_reads (user_id, peer_id, last_seq, read_at)
             SELECT $1, p.id,
                    COALESCE((
                        SELECT MAX(m.seq)
                        FROM direct_messages m
                        WHERE (m.sender_id = $1 AND m.recipient_id = p.id)
                           OR (m.recipient_id = $1 AND m.sender_id = p.id)
                    ), 0),
                    now()
             FROM accounts p
             WHERE p.username_key = $2
             ON CONFLICT (user_id, peer_id)
             DO UPDATE SET last_seq = GREATEST(direct_reads.last_seq, EXCLUDED.last_seq),
                           read_at = now()",
        )
        .bind(account_id)
        .bind(peer_key)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Количество строк в таблицах для `GET /api/health`.
    pub async fn counts(&self) -> Result<(i64, i64), sqlx::Error> {
        let row = sqlx::query(
            "SELECT (SELECT COUNT(*) FROM chat_messages) AS public,
                    (SELECT COUNT(*) FROM direct_messages) AS direct",
        )
        .fetch_one(&self.pool)
        .await?;

        Ok((row.get("public"), row.get("direct")))
    }

    /// Удаляет все сохранённые сообщения и аккаунты. Используется интеграционным
    /// тестом PostgreSQL.
    #[cfg(test)]
    async fn truncate(&self) -> Result<(), sqlx::Error> {
        sqlx::query(
            "TRUNCATE chat_messages, direct_messages, direct_reads, sessions, accounts
             RESTART IDENTITY",
        )
        .execute(&self.pool)
        .await?;

        Ok(())
    }
}

// Интеграционные тесты требуют запущенного PostgreSQL и по умолчанию пропускаются.
#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use uuid::Uuid;

    fn public(text: &str) -> ChatMessage {
        ChatMessage {
            id: Uuid::new_v4(),
            author: "Алиса".to_owned(),
            text: text.to_owned(),
            sent_at: Utc::now(),
        }
    }

    fn direct(from: &str, to: &str, text: &str) -> DirectMessage {
        DirectMessage {
            id: Uuid::new_v4(),
            from: from.to_owned(),
            to: to.to_owned(),
            text: text.to_owned(),
            sent_at: Utc::now(),
        }
    }

    fn name(value: &str) -> UserName {
        UserName::parse(value).expect("valid test name")
    }

    // Требует запущенного PostgreSQL, например:
    //   $env:CHAT_DB_URL = "postgres://<user>:<password>@localhost:5432/rust_chat"
    //   cargo test -- --ignored
    #[tokio::test]
    #[ignore]
    async fn round_trips_messages_and_read_marks() {
        let url = std::env::var("CHAT_DB_URL").expect("CHAT_DB_URL must be set");
        let db = ChatDb::connect(&url).await.expect("connect to PostgreSQL");
        db.truncate().await.expect("truncate");

        // Сообщения ссылаются на аккаунты, поэтому сначала создаём участников.
        let alice = db
            .create_account(&name("Алиса"), "hash")
            .await
            .unwrap()
            .expect("create Алиса");
        let bob = db
            .create_account(&name("Боб"), "hash")
            .await
            .unwrap()
            .expect("create Боб");

        db.save_public(&public("первое"), alice.id).await.unwrap();
        db.save_public(&public("второе"), alice.id).await.unwrap();

        // Лимит соблюдается, а последнее сообщение считается самым новым.
        let history = db.load_public_history(1).await.unwrap();
        assert_eq!(history.len(), 1, "limit is respected");
        assert_eq!(history[0].text, "второе", "newest message wins");
        assert_eq!(history[0].author, "Алиса", "author is joined from accounts");

        db.save_direct(&direct("Алиса", "Боб", "привет"), alice.id, bob.id)
            .await
            .unwrap();
        db.save_direct(&direct("Боб", "Алиса", "здарова"), bob.id, alice.id)
            .await
            .unwrap();

        // Один диалог независимо от написания имён, ответ Боба не прочитан.
        let threads = db
            .load_direct_threads(alice.id, THREAD_REPLAY)
            .await
            .unwrap();
        assert_eq!(threads.len(), 1, "one conversation for the account pair");
        assert_eq!(threads[0].messages.len(), 2);
        assert_eq!(threads[0].peer, "Боб");
        assert_eq!(threads[0].unread, 1, "the reply from Боб is unread");

        // Отметка о прочтении обнуляет счётчик.
        db.mark_read(alice.id, &name_key("боб")).await.unwrap();
        let after = db
            .load_direct_threads(alice.id, THREAD_REPLAY)
            .await
            .unwrap();
        assert_eq!(after[0].unread, 0, "read mark clears the counter");

        db.truncate().await.unwrap();
        db.close().await;
    }
}
