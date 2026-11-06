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
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgRow};
use sqlx::{PgPool, Row};
use tracing::info;
use uuid::Uuid;

use crate::auth::Account;
use crate::protocol::{
    Attachment, ChatMessage, DEFAULT_ROOMS, DirectMessage, DirectThread, Room, UserName, name_key,
};

/// Тайм-аут установки соединения с базой.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Максимальное число соединений в пуле.
const MAX_CONNECTIONS: u32 = 5;

/// Сколько последних сообщений одного диалога восстанавливается при входе пользователя.
pub const THREAD_REPLAY: i64 = 100;

/// Метаданные файла, загруженного вместе с сообщением; используются при скачивании.
#[derive(Debug, Clone)]
pub struct StoredAttachment {
    /// Исходное имя файла.
    pub name: String,
    /// MIME-тип, полученный от браузера при загрузке.
    pub mime: String,
    /// Размер файла в байтах.
    pub size: i64,
}

/// Собирает вложение сообщения из колонок строки результата.
///
/// Имя задаётся вместе с вложением, а тип и размер могли появиться на базах
/// старой схемы, поэтому при их отсутствии подставляются разумные значения.
fn attachment_from_row(row: &PgRow) -> Option<Attachment> {
    let name = row.get::<Option<String>, _>("attachment_name")?;

    // У сообщения с вложением имя обязательно есть; остальное — запасной путь.
    let id: Uuid = row.get("id");
    Some(Attachment {
        id,
        name,
        mime: row
            .try_get::<Option<String>, _>("attachment_mime")
            .ok()
            .flatten()
            .unwrap_or_else(|| crate::uploads::DEFAULT_CONTENT_TYPE.to_owned()),
        size: row
            .try_get::<Option<i64>, _>("attachment_size")
            .ok()
            .flatten()
            .unwrap_or(0),
    })
}

/// Собирает метаданные файла для скачивания из колонок строки результата.
fn stored_attachment_from_row(row: &PgRow) -> StoredAttachment {
    StoredAttachment {
        name: row.get("attachment_name"),
        mime: row
            .try_get::<Option<String>, _>("attachment_mime")
            .ok()
            .flatten()
            .unwrap_or_else(|| crate::uploads::DEFAULT_CONTENT_TYPE.to_owned()),
        size: row
            .try_get::<Option<i64>, _>("attachment_size")
            .ok()
            .flatten()
            .unwrap_or(0),
    }
}

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
            include_str!("../migrations/003_rooms_pagination.sql"),
            include_str!("../migrations/004_attachments.sql"),
        ] {
            sqlx::raw_sql(migration)
                .execute(&pool)
                .await
                .map_err(|error| {
                    format!("Не удалось применить схему чата в PostgreSQL: {error}")
                })?;
        }

        let db = Self { pool };
        // Названия и порядок комнат задаёт код, а не миграция: так список
        // комнат остаётся в одном месте (DEFAULT_ROOMS в protocol.rs).
        db.ensure_rooms()
            .await
            .map_err(|error| format!("Не удалось подготовить комнаты: {error}"))?;

        info!("connected to PostgreSQL");
        Ok(db)
    }

    /// Закрывает все соединения пула.
    pub async fn close(&self) {
        self.pool.close().await;
    }

    /// Создаёт стартовые комнаты и обновляет их названия и порядок.
    ///
    /// `DEFAULT_ROOMS` — единственный источник истины: если название комнаты
    /// поменяли в коде, при следующем запуске оно обновится и в базе.
    /// Возвращает актуальный список комнат.
    pub async fn ensure_rooms(&self) -> Result<Vec<Room>, sqlx::Error> {
        for (position, (id, title)) in DEFAULT_ROOMS.iter().enumerate() {
            sqlx::query(
                "INSERT INTO rooms (id, title, position)
                 VALUES ($1, $2, $3)
                 ON CONFLICT (id) DO UPDATE
                 SET title = EXCLUDED.title,
                     position = EXCLUDED.position",
            )
            .bind(*id)
            .bind(*title)
            .bind(position as i32)
            .execute(&self.pool)
            .await?;
        }

        self.load_rooms().await
    }

    /// Комнаты в порядке отображения.
    pub async fn load_rooms(&self) -> Result<Vec<Room>, sqlx::Error> {
        let rows = sqlx::query("SELECT id, title FROM rooms ORDER BY position, id")
            .fetch_all(&self.pool)
            .await?;

        Ok(rows
            .into_iter()
            .map(|row| Room {
                id: row.get("id"),
                title: row.get("title"),
            })
            .collect())
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

    /// Сохраняет сообщение комнаты от имени аккаунта `author_id`.
    pub async fn save_public(
        &self,
        message: &ChatMessage,
        author_id: Uuid,
    ) -> Result<(), sqlx::Error> {
        let attachment = message.attachment.as_ref();

        sqlx::query(
            "INSERT INTO chat_messages
                 (id, room_id, author_id, body, sent_at,
                  attachment_name, attachment_mime, attachment_size)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(message.id)
        .bind(&message.room)
        .bind(author_id)
        .bind(&message.text)
        .bind(message.sent_at)
        .bind(attachment.map(|file| file.name.as_str()))
        .bind(attachment.map(|file| file.mime.as_str()))
        .bind(attachment.map(|file| file.size))
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Последние `limit` сообщений комнаты, самые старые первыми.
    ///
    /// Второй элемент — есть ли в базе сообщения старше присланных.
    pub async fn load_public_history(
        &self,
        room: &str,
        limit: i64,
    ) -> Result<(Vec<ChatMessage>, bool), sqlx::Error> {
        self.load_public_page(room, None, limit).await
    }

    /// Порция сообщений комнаты, которые старше сообщения `before`.
    ///
    /// `None` в качестве `before` означает «самые новые сообщения».
    /// Второй элемент — есть ли ещё более старые сообщения.
    pub async fn load_public_page(
        &self,
        room: &str,
        before: Option<Uuid>,
        limit: i64,
    ) -> Result<(Vec<ChatMessage>, bool), sqlx::Error> {
        // В истории хранится только UUID автора, имя подтягиваем из аккаунта.
        // Запрашиваем на одно сообщение больше, чтобы узнать, есть ли ещё.
        let rows = sqlx::query(
            "SELECT m.id, m.room_id, a.username AS author, m.body, m.sent_at,
                    m.attachment_name, m.attachment_mime, m.attachment_size
             FROM chat_messages m
             JOIN accounts a ON a.id = m.author_id
             WHERE m.room_id = $1
               AND ($2::uuid IS NULL OR m.seq < (
                   SELECT anchor.seq
                   FROM chat_messages anchor
                   WHERE anchor.id = $2 AND anchor.room_id = $1
               ))
             ORDER BY m.seq DESC
             LIMIT $3",
        )
        .bind(room)
        .bind(before)
        .bind(limit + 1)
        .fetch_all(&self.pool)
        .await?;

        let has_more = rows.len() as i64 > limit;
        let messages = rows
            .into_iter()
            .take(limit.max(0) as usize)
            .rev()
            .map(|row| ChatMessage {
                id: row.get("id"),
                room: row.get("room_id"),
                author: row.get("author"),
                text: row.get("body"),
                sent_at: row.get("sent_at"),
                attachment: attachment_from_row(&row),
            })
            .collect();

        Ok((messages, has_more))
    }

    /// Сохраняет личное сообщение с идентификаторами отправителя и получателя.
    pub async fn save_direct(
        &self,
        message: &DirectMessage,
        sender_id: Uuid,
        recipient_id: Uuid,
    ) -> Result<(), sqlx::Error> {
        let attachment = message.attachment.as_ref();

        sqlx::query(
            "INSERT INTO direct_messages
                 (id, sender_id, recipient_id, body, sent_at,
                  attachment_name, attachment_mime, attachment_size)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(message.id)
        .bind(sender_id)
        .bind(recipient_id)
        .bind(&message.text)
        .bind(message.sent_at)
        .bind(attachment.map(|file| file.name.as_str()))
        .bind(attachment.map(|file| file.mime.as_str()))
        .bind(attachment.map(|file| file.size))
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
                        attachment_name, attachment_mime, attachment_size,
                        CASE WHEN sender_id = $1 THEN recipient_id ELSE sender_id END AS peer_id
                 FROM direct_messages
                 WHERE sender_id = $1 OR recipient_id = $1
             ),
             ranked AS (
                 SELECT *, ROW_NUMBER() OVER (PARTITION BY peer_id ORDER BY seq DESC) AS newest_first
                 FROM involved
             )
             SELECT r.seq, r.id, s.username AS sender, rc.username AS recipient,
                    r.body, r.sent_at, r.peer_id, p.username AS peer,
                    r.attachment_name, r.attachment_mime, r.attachment_size
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
                attachment: attachment_from_row(&row),
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

    /// Ищет файл по идентификатору сообщения и проверяет право доступа.
    ///
    /// Файлы публичных комнат может скачать любой владелец сессии, файлы
    /// личных сообщений — только участники переписки. Сообщению без вложения
    /// и чужому личному файлу соответствует `None`, чтобы нельзя было
    /// отличить «нет файла» от «нет доступа».
    pub async fn find_attachment(
        &self,
        message_id: Uuid,
        account_id: Uuid,
    ) -> Result<Option<StoredAttachment>, sqlx::Error> {
        let public = sqlx::query(
            "SELECT attachment_name, attachment_mime, attachment_size
             FROM chat_messages
             WHERE id = $1 AND attachment_name IS NOT NULL",
        )
        .bind(message_id)
        .fetch_optional(&self.pool)
        .await?;

        if let Some(row) = public {
            return Ok(Some(stored_attachment_from_row(&row)));
        }

        let direct = sqlx::query(
            "SELECT attachment_name, attachment_mime, attachment_size
             FROM direct_messages
             WHERE id = $1
               AND attachment_name IS NOT NULL
               AND (sender_id = $2 OR recipient_id = $2)",
        )
        .bind(message_id)
        .bind(account_id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(direct.map(|row| stored_attachment_from_row(&row)))
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

    fn public(room: &str, text: &str) -> ChatMessage {
        ChatMessage {
            id: Uuid::new_v4(),
            room: room.to_owned(),
            author: "Алиса".to_owned(),
            text: text.to_owned(),
            sent_at: Utc::now(),
            attachment: None,
        }
    }

    fn direct(from: &str, to: &str, text: &str) -> DirectMessage {
        DirectMessage {
            id: Uuid::new_v4(),
            from: from.to_owned(),
            to: to.to_owned(),
            text: text.to_owned(),
            sent_at: Utc::now(),
            attachment: None,
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

        // Комнаты создаются при подключении, даже если база была пустой.
        let rooms = db.load_rooms().await.unwrap();
        assert_eq!(
            rooms
                .iter()
                .map(|room| room.id.as_str())
                .collect::<Vec<_>>(),
            ["general", "rust", "random"]
        );

        let first = public("general", "первое");
        let second = public("general", "второе");
        db.save_public(&first, alice.id).await.unwrap();
        db.save_public(&second, alice.id).await.unwrap();
        db.save_public(&public("rust", "про Rust"), alice.id)
            .await
            .unwrap();

        // Лимит соблюдается, а последнее сообщение считается самым новым.
        let (history, has_more) = db.load_public_history("general", 1).await.unwrap();
        assert_eq!(history.len(), 1, "limit is respected");
        assert_eq!(history[0].text, "второе", "newest message wins");
        assert_eq!(history[0].author, "Алиса", "author is joined from accounts");
        assert_eq!(history[0].room, "general");
        assert!(has_more, "older messages are reported");

        // Страница перед `второе` возвращает `первое` и больше ничего.
        let (older, more) = db
            .load_public_page("general", Some(second.id), 1)
            .await
            .unwrap();
        assert_eq!(older.len(), 1);
        assert_eq!(older[0].text, "первое");
        assert!(!more);

        // Комнаты не смешиваются.
        let (rust, more) = db.load_public_history("rust", 10).await.unwrap();
        assert_eq!(rust.len(), 1);
        assert_eq!(rust[0].text, "про Rust");
        assert!(!more);

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

        // Вложение: метаданные переживают запись и чтение, а доступ к файлу
        // личного сообщения есть только у участников переписки.
        let eva = db
            .create_account(&name("Ева"), "hash")
            .await
            .unwrap()
            .expect("create Ева");

        let mut with_file = public("general", "снимок экрана");
        with_file.attachment = Some(Attachment {
            id: with_file.id,
            name: "снимок.png".to_owned(),
            mime: "image/png".to_owned(),
            size: 12,
        });
        db.save_public(&with_file, alice.id).await.unwrap();

        let restored = db
            .load_public_history("general", 100)
            .await
            .unwrap()
            .0
            .into_iter()
            .find(|message| message.id == with_file.id)
            .expect("attachment message is in history");
        let restored_file = restored.attachment.expect("attachment is restored");
        assert_eq!(restored_file.name, "снимок.png");
        assert_eq!(restored_file.mime, "image/png");
        assert_eq!(restored_file.size, 12);

        let downloaded = db
            .find_attachment(with_file.id, eva.id)
            .await
            .unwrap()
            .expect("public attachment is open to any account");
        assert_eq!(downloaded.name, "снимок.png");

        let mut private = direct("Алиса", "Боб", "смотри сюда");
        private.attachment = Some(Attachment {
            id: private.id,
            name: "архив.zip".to_owned(),
            mime: "application/zip".to_owned(),
            size: 34,
        });
        db.save_direct(&private, alice.id, bob.id).await.unwrap();

        assert!(
            db.find_attachment(private.id, bob.id)
                .await
                .unwrap()
                .is_some(),
            "the recipient can download the file"
        );
        assert!(
            db.find_attachment(private.id, eva.id)
                .await
                .unwrap()
                .is_none(),
            "a stranger cannot resolve a private attachment"
        );

        db.truncate().await.unwrap();
        db.close().await;
    }
}
