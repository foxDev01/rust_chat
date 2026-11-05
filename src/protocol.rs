//! Протокол обмена между сервером и браузерным клиентом.
//!
//! Входящие значения сразу разбираются в типы, которые могут содержать только
//! корректные данные ([`UserName`], [`MessageText`]), поэтому остальному коду
//! сервера не нужно повторно проверять их.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Минимальная длина имени пользователя в символах.
pub const MIN_NAME_LEN: usize = 2;
/// Максимальная длина имени пользователя в символах.
pub const MAX_NAME_LEN: usize = 24;
/// Максимальная длина текста сообщения в символах.
pub const MAX_MESSAGE_LEN: usize = 2_000;

/// Имя пользователя, прошедшее проверку.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserName(String);

impl UserName {
    /// Проверяет имя и возвращает его в нормализованном виде (без пробелов по краям).
    ///
    /// Допустимы буквы, цифры, пробел, `_` и `-`; длина — от 2 до 24 символов.
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

    /// Возвращает имя в том виде, в котором оно было зарегистрировано.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for UserName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Приводит имя к ключу сравнения.
///
/// Имена сравниваются без учёта регистра в списке онлайн-участников и при
/// поиске собеседника. В PostgreSQL сообщения ссылаются на `accounts.id`,
/// поэтому ключи имён там больше не хранятся.
pub fn name_key(name: &str) -> String {
    name.trim().to_lowercase()
}

/// Текст сообщения, прошедший проверку.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageText(String);

impl MessageText {
    /// Проверяет текст сообщения и обрезает пробелы по краям.
    ///
    /// Пустые сообщения и текст длиннее [`MAX_MESSAGE_LEN`] отклоняются;
    /// управляющие символы запрещены, кроме перевода строки и табуляции.
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

    /// Забирает проверенный текст, чтобы сохранить его или отправить дальше.
    pub fn into_string(self) -> String {
        self.0
    }
}

/// Сообщение, которое браузер отправляет серверу.
///
/// Поле `type` в JSON задаёт вариант: `join`, `message`, `direct` или `read`.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    /// Публичное сообщение в общий канал.
    Message { text: String },
    /// Личное сообщение конкретному пользователю.
    Direct { to: String, text: String },
    /// Отмечает все личные сообщения от `peer` прочитанными текущим пользователем.
    Read { peer: String },
}

/// Сообщение общего канала: хранится в истории и рассылается всем клиентам.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChatMessage {
    pub id: Uuid,
    pub author: String,
    pub text: String,
    pub sent_at: DateTime<Utc>,
}

/// Личное сообщение: доставляется только отправителю и получателю.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DirectMessage {
    pub id: Uuid,
    pub from: String,
    pub to: String,
    pub text: String,
    pub sent_at: DateTime<Utc>,
}

/// Личная переписка, восстановленная из PostgreSQL при входе пользователя.
#[derive(Debug, Clone, Serialize)]
pub struct DirectThread {
    /// Имя собеседника из таблицы `accounts`.
    pub peer: String,
    /// Последние сообщения диалога в хронологическом порядке.
    pub messages: Vec<DirectMessage>,
    /// Сколько сообщений от собеседника пользователь ещё не прочитал.
    pub unread: i64,
}

/// Сообщение, которое сервер отправляет браузеру.
///
/// Поле `type` в JSON задаёт вариант; клиент разбирает его в `frontend/src/types.ts`.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    /// Успешный вход: имя пользователя и список участников онлайн.
    Welcome { you: String, users: Vec<String> },
    /// Публичная история на момент подключения.
    History { messages: Vec<ChatMessage> },
    /// Новое сообщение общего канала.
    Message { message: ChatMessage },
    /// Новое личное сообщение.
    Direct { message: DirectMessage },
    /// Личные переписки пользователя, восстановленные из базы.
    DirectHistory { threads: Vec<DirectThread> },
    /// Системное уведомление (вход или выход участника).
    System { text: String, at: DateTime<Utc> },
    /// Актуальный список участников онлайн.
    Users { users: Vec<String> },
    /// Ошибка, которую нужно показать пользователю.
    Error { message: String },
    /// Сервер завершил сессию: например, пароль сменили на другом устройстве.
    SessionEnded { reason: String },
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
