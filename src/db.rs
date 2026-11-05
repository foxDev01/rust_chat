//! Хранилище PostgreSQL: публичная история, личные переписки и отметки о прочтении.
//!
//! Схема описана в `migrations/001_init.sql` и применяется при каждом старте;
//! все команды идемпотентны, поэтому сервер можно запускать на готовой базе.

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

        // Схема применяется целиком; повторный запуск ничего не ломает.
        sqlx::raw_sql(include_str!("../migrations/001_init.sql"))
            .execute(&pool)
            .await
            .map_err(|error| format!("Не применить схему чата в PostgreSQL: {error}"))?;

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

    /// Сохраняет сообщение общего канала.
    pub async fn save_public(&self, message: &ChatMessage) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO chat_messages (id, author, body, sent_at) VALUES ($1, $2, $3, $4)",
        )
        .bind(message.id)
        .bind(&message.author)
        .bind(&message.text)
        .bind(message.sent_at)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Последние `limit` публичных сообщений, самые старые первыми.
    pub async fn load_public_history(&self, limit: i64) -> Result<Vec<ChatMessage>, sqlx::Error> {
        // Берём хвост истории по убыванию seq, а затем разворачиваем его.
        let rows = sqlx::query(
            "SELECT id, author, body, sent_at FROM chat_messages ORDER BY seq DESC LIMIT $1",
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

    /// Сохраняет личное сообщение вместе с ключами имён без учёта регистра.
    pub async fn save_direct(&self, message: &DirectMessage) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO direct_messages (id, sender, recipient, sender_key, recipient_key, body, sent_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(message.id)
        .bind(&message.from)
        .bind(&message.to)
        .bind(name_key(&message.from))
        .bind(name_key(&message.to))
        .bind(&message.text)
        .bind(message.sent_at)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Личные переписки пользователя: в каждой — последние сообщения,
    /// самые старые первыми, а список диалогов отсортирован по свежести.
    pub async fn load_direct_threads(
        &self,
        name: &str,
        per_thread: i64,
    ) -> Result<Vec<DirectThread>, sqlx::Error> {
        let key = name_key(name);

        // Для каждого сообщения определяем собеседника (`peer_key`), затем
        // нумеруем сообщения внутри диалога от новых к старым и оставляем
        // только `per_thread` последних.
        let messages = sqlx::query(
            "WITH involved AS (
                 SELECT seq, id, sender, recipient, body, sent_at,
                        CASE WHEN sender_key = $1 THEN recipient_key ELSE sender_key END AS peer_key
                 FROM direct_messages
                 WHERE sender_key = $1 OR recipient_key = $1
             ),
             ranked AS (
                 SELECT *, ROW_NUMBER() OVER (PARTITION BY peer_key ORDER BY seq DESC) AS newest_first
                 FROM involved
             )
             SELECT seq, id, sender, recipient, body, sent_at, peer_key
             FROM ranked
             WHERE newest_first <= $2
             ORDER BY seq",
        )
        .bind(&key)
        .bind(per_thread)
        .fetch_all(&self.pool)
        .await?;

        // Непрочитанные считаем как сообщения, пришедшие после отметки `direct_reads`.
        let unread_rows = sqlx::query(
            "SELECT m.sender_key AS peer_key, COUNT(*) AS unread
             FROM direct_messages m
             WHERE m.recipient_key = $1
               AND m.seq > COALESCE(
                   (SELECT r.last_seq FROM direct_reads r WHERE r.user_key = $1 AND r.peer_key = m.sender_key),
                   0)
             GROUP BY m.sender_key",
        )
        .bind(&key)
        .fetch_all(&self.pool)
        .await?;

        /// Промежуточная запись диалога при сборке результата.
        #[derive(Default)]
        struct Thread {
            peer: String,
            last_seq: i64,
            messages: Vec<DirectMessage>,
            unread: i64,
        }

        let mut threads: BTreeMap<String, Thread> = BTreeMap::new();

        // Складываем сообщения в диалоги по ключу собеседника.
        for row in messages {
            let peer_key: String = row.get("peer_key");
            let sender: String = row.get("sender");
            let recipient: String = row.get("recipient");

            let entry = threads.entry(peer_key.clone()).or_default();
            // Предпочитаем написание, которое собеседник использовал при входе.
            entry.peer = if name_key(&sender) == key {
                recipient.clone()
            } else {
                sender.clone()
            };

            entry.last_seq = row.get::<i64, _>("seq");
            entry.messages.push(DirectMessage {
                id: row.get("id"),
                from: sender,
                to: recipient,
                text: row.get("body"),
                sent_at: row.get("sent_at"),
            });
        }

        // Добавляем счётчики непрочитанных; диалог мог не попасть в выборку сообщений.
        for row in unread_rows {
            let peer_key: String = row.get("peer_key");
            let unread: i64 = row.get("unread");

            let entry = threads.entry(peer_key.clone()).or_default();
            if entry.peer.is_empty() {
                entry.peer = peer_key;
            }
            entry.unread = unread;
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

    /// Запоминает, что `user` прочитал всё, что `peer` отправил к этому моменту.
    pub async fn mark_read(&self, user: &str, peer: &str) -> Result<(), sqlx::Error> {
        let user_key = name_key(user);
        let peer_key = name_key(peer);

        // Отметка ставится на максимальный seq в диалоге в обе стороны.
        let last_seq: i64 = sqlx::query(
            "SELECT COALESCE(MAX(seq), 0) AS last_seq
             FROM direct_messages
             WHERE (sender_key = $1 AND recipient_key = $2)
                OR (sender_key = $2 AND recipient_key = $1)",
        )
        .bind(&user_key)
        .bind(&peer_key)
        .fetch_one(&self.pool)
        .await?
        .get("last_seq");

        // Счётчик прочитанного может только расти.
        sqlx::query(
            "INSERT INTO direct_reads (user_key, peer_key, last_seq, read_at)
             VALUES ($1, $2, $3, now())
             ON CONFLICT (user_key, peer_key)
             DO UPDATE SET last_seq = GREATEST(direct_reads.last_seq, EXCLUDED.last_seq),
                           read_at = now()",
        )
        .bind(&user_key)
        .bind(&peer_key)
        .bind(last_seq)
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

    /// Удаляет все сохранённые сообщения. Используется интеграционным тестом PostgreSQL.
    #[cfg(test)]
    async fn truncate(&self) -> Result<(), sqlx::Error> {
        sqlx::query("TRUNCATE chat_messages, direct_messages, direct_reads RESTART IDENTITY")
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

    // Требует запущенного PostgreSQL, например:
    //   $env:CHAT_DB_URL = "postgres://<user>:<password>@localhost:5432/rust_chat"
    //   cargo test -- --ignored
    #[tokio::test]
    #[ignore]
    async fn round_trips_messages_and_read_marks() {
        let url = std::env::var("CHAT_DB_URL").expect("CHAT_DB_URL must be set");
        let db = ChatDb::connect(&url).await.expect("connect to PostgreSQL");
        db.truncate().await.expect("truncate");

        db.save_public(&public("первое")).await.unwrap();
        db.save_public(&public("второе")).await.unwrap();

        // Лимит соблюдается, а последнее сообщение считается самым новым.
        let history = db.load_public_history(1).await.unwrap();
        assert_eq!(history.len(), 1, "limit is respected");
        assert_eq!(history[0].text, "второе", "newest message wins");

        db.save_direct(&direct("Алиса", "Боб", "привет"))
            .await
            .unwrap();
        db.save_direct(&direct("БОБ", "алиса", "здарова"))
            .await
            .unwrap();

        // Один диалог независимо от регистра имён, ответ Боба не прочитан.
        let threads = db
            .load_direct_threads("алиса", THREAD_REPLAY)
            .await
            .unwrap();
        assert_eq!(threads.len(), 1, "one conversation, any spelling");
        assert_eq!(threads[0].messages.len(), 2);
        assert_eq!(threads[0].unread, 1, "the reply from Боб is unread");

        // Отметка о прочтении обнуляет счётчик.
        db.mark_read("Алиса", "боб").await.unwrap();
        let after = db
            .load_direct_threads("Алиса", THREAD_REPLAY)
            .await
            .unwrap();
        assert_eq!(after[0].unread, 0, "read mark clears the counter");

        db.truncate().await.unwrap();
        db.close().await;
    }
}
