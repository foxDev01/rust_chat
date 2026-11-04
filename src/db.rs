//! PostgreSQL storage: public history, private threads and read marks.
//!
//! The schema lives in `migrations/001_init.sql` and is applied on every start;
//! all statements are idempotent, so the server can boot against an existing
//! database.

use std::collections::BTreeMap;
use std::error::Error;
use std::time::Duration;

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{PgPool, Row};
use tracing::info;

use crate::protocol::{ChatMessage, DirectMessage, DirectThread, name_key};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_CONNECTIONS: u32 = 5;

/// How many recent messages of one conversation are replayed when a user joins.
pub const THREAD_REPLAY: i64 = 100;

pub struct ChatDb {
    pool: PgPool,
}

impl ChatDb {
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

        sqlx::raw_sql(include_str!("../migrations/001_init.sql"))
            .execute(&pool)
            .await
            .map_err(|error| format!("Не применить схему чата в PostgreSQL: {error}"))?;

        info!("connected to PostgreSQL");
        Ok(Self { pool })
    }

    pub async fn close(&self) {
        self.pool.close().await;
    }

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

    /// The last `limit` public messages, oldest first.
    pub async fn load_public_history(&self, limit: i64) -> Result<Vec<ChatMessage>, sqlx::Error> {
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

    /// Private conversations of `name`, newest message last, most recent peer first.
    pub async fn load_direct_threads(
        &self,
        name: &str,
        per_thread: i64,
    ) -> Result<Vec<DirectThread>, sqlx::Error> {
        let key = name_key(name);

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

        #[derive(Default)]
        struct Thread {
            peer: String,
            last_seq: i64,
            messages: Vec<DirectMessage>,
            unread: i64,
        }

        let mut threads: BTreeMap<String, Thread> = BTreeMap::new();

        for row in messages {
            let peer_key: String = row.get("peer_key");
            let sender: String = row.get("sender");
            let recipient: String = row.get("recipient");

            let entry = threads.entry(peer_key.clone()).or_default();
            // Prefer the spelling the peer itself used when joining.
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

    /// Remembers that `user` has read everything sent by `peer` so far.
    pub async fn mark_read(&self, user: &str, peer: &str) -> Result<(), sqlx::Error> {
        let user_key = name_key(user);
        let peer_key = name_key(peer);

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

    /// Row counts for `GET /api/health`.
    pub async fn counts(&self) -> Result<(i64, i64), sqlx::Error> {
        let row = sqlx::query(
            "SELECT (SELECT COUNT(*) FROM chat_messages) AS public,
                    (SELECT COUNT(*) FROM direct_messages) AS direct",
        )
        .fetch_one(&self.pool)
        .await?;

        Ok((row.get("public"), row.get("direct")))
    }

    /// Removes all stored messages. Used by the PostgreSQL integration test.
    #[cfg(test)]
    async fn truncate(&self) -> Result<(), sqlx::Error> {
        sqlx::query("TRUNCATE chat_messages, direct_messages, direct_reads RESTART IDENTITY")
            .execute(&self.pool)
            .await?;

        Ok(())
    }
}

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

    // Requires a running PostgreSQL, e.g.
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

        let history = db.load_public_history(1).await.unwrap();
        assert_eq!(history.len(), 1, "limit is respected");
        assert_eq!(history[0].text, "второе", "newest message wins");

        db.save_direct(&direct("Алиса", "Боб", "привет"))
            .await
            .unwrap();
        db.save_direct(&direct("БОБ", "алиса", "здарова"))
            .await
            .unwrap();

        let threads = db
            .load_direct_threads("алиса", THREAD_REPLAY)
            .await
            .unwrap();
        assert_eq!(threads.len(), 1, "one conversation, any spelling");
        assert_eq!(threads[0].messages.len(), 2);
        assert_eq!(threads[0].unread, 1, "the reply from Боб is unread");

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
