//! Wire protocol shared by the server and the browser client.
//!
//! Incoming values are parsed into types that can only hold valid data
//! ([`UserName`], [`MessageText`]), so the rest of the server never has to
//! re-check them.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const MIN_NAME_LEN: usize = 2;
pub const MAX_NAME_LEN: usize = 24;
pub const MAX_MESSAGE_LEN: usize = 2_000;

/// A name that has passed validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserName(String);

impl UserName {
    pub fn parse(raw: &str) -> Result<Self, &'static str> {
        let name = raw.trim();
        let len = name.chars().count();

        if !(MIN_NAME_LEN..=MAX_NAME_LEN).contains(&len) {
            return Err("Имя должно быть от 2 до 24 символов");
        }

        if !name
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, ' ' | '_' | '-'))
        {
            return Err("В имени допустимы только буквы, цифры, пробел, «_» и «-»");
        }

        Ok(Self(name.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for UserName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Names are compared case-insensitively everywhere: online users, lookups and
/// the `*_key` columns in PostgreSQL. Keep this the single source of truth.
pub fn name_key(name: &str) -> String {
    name.trim().to_lowercase()
}

/// Message text that has passed validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageText(String);

impl MessageText {
    pub fn parse(raw: &str) -> Result<Self, &'static str> {
        let text = raw.trim();

        if text.is_empty() {
            return Err("Сообщение не может быть пустым");
        }

        if text.chars().count() > MAX_MESSAGE_LEN {
            return Err("Сообщение слишком длинное (максимум 2000 символов)");
        }

        if text
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\n' | '\t'))
        {
            return Err("Сообщение содержит недопустимые символы");
        }

        Ok(Self(text.to_owned()))
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

/// What the browser sends to the server.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    Join {
        name: String,
    },
    Message {
        text: String,
    },
    Direct {
        to: String,
        text: String,
    },
    /// Marks every private message from `peer` as read by the current user.
    Read {
        peer: String,
    },
}

/// A chat message as stored in history and broadcast to clients.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChatMessage {
    pub id: Uuid,
    pub author: String,
    pub text: String,
    pub sent_at: DateTime<Utc>,
}

/// A private message delivered only to its sender and recipient.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DirectMessage {
    pub id: Uuid,
    pub from: String,
    pub to: String,
    pub text: String,
    pub sent_at: DateTime<Utc>,
}

/// A private conversation restored from PostgreSQL when a user joins.
#[derive(Debug, Clone, Serialize)]
pub struct DirectThread {
    pub peer: String,
    pub messages: Vec<DirectMessage>,
    pub unread: i64,
}

/// What the server sends to the browser.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Welcome { you: String, users: Vec<String> },
    History { messages: Vec<ChatMessage> },
    Message { message: ChatMessage },
    Direct { message: DirectMessage },
    DirectHistory { threads: Vec<DirectThread> },
    System { text: String, at: DateTime<Utc> },
    Users { users: Vec<String> },
    Error { message: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_reasonable_names() {
        assert_eq!(
            UserName::parse("  Анна-Мария_1 ").unwrap().as_str(),
            "Анна-Мария_1"
        );
        assert_eq!(UserName::parse("Михаил").unwrap().as_str(), "Михаил");
    }

    #[test]
    fn rejects_bad_names() {
        assert_eq!(
            UserName::parse(""),
            Err("Имя должно быть от 2 до 24 символов")
        );
        assert_eq!(
            UserName::parse("a"),
            Err("Имя должно быть от 2 до 24 символов")
        );
        assert!(UserName::parse(&"я".repeat(MAX_NAME_LEN + 1)).is_err());
        assert!(UserName::parse("bad<script>").is_err());
        assert!(UserName::parse("лишняя\nстрока").is_err());
    }

    #[test]
    fn trims_message_text() {
        assert_eq!(
            MessageText::parse("  привет  ").unwrap().into_string(),
            "привет"
        );
        assert!(MessageText::parse(" \n\t ").is_err());
    }

    #[test]
    fn enforces_message_limits() {
        assert!(MessageText::parse(&"я".repeat(MAX_MESSAGE_LEN)).is_ok());
        assert!(MessageText::parse(&"я".repeat(MAX_MESSAGE_LEN + 1)).is_err());
        assert!(MessageText::parse("битый\0символ").is_err());
    }

    #[test]
    fn parses_direct_client_messages() {
        let parsed: ClientMessage =
            serde_json::from_str(r#"{"type":"direct","to":"Боб","text":"привет"}"#).unwrap();

        match parsed {
            ClientMessage::Direct { to, text } => {
                assert_eq!(to, "Боб");
                assert_eq!(text, "привет");
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }

    #[test]
    fn serializes_direct_server_messages() {
        let message = ServerMessage::Direct {
            message: DirectMessage {
                id: Uuid::nil(),
                from: "Алиса".to_owned(),
                to: "Боб".to_owned(),
                text: "привет".to_owned(),
                sent_at: Utc::now(),
            },
        };

        let json = serde_json::to_value(&message).unwrap();
        assert_eq!(json["type"], "direct");
        assert_eq!(json["message"]["from"], "Алиса");
        assert_eq!(json["message"]["to"], "Боб");
        assert_eq!(json["message"]["text"], "привет");
    }
}
