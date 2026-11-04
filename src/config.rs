//! Конфигурация сервера, читаемая из переменных окружения.
//!
//! - `CHAT_BIND` — адрес и порт прослушивания (по умолчанию `0.0.0.0:3000`);
//! - `CHAT_DB_URL` — обязательная строка подключения к PostgreSQL.

use std::{env, error::Error, net::SocketAddr};

/// Адрес, который используется, если `CHAT_BIND` не задан.
pub const DEFAULT_BIND: &str = "0.0.0.0:3000";

/// Настройки, необходимые серверу для запуска.
#[derive(Debug, Clone)]
pub struct Config {
    /// Адрес, на котором axum принимает HTTP- и WebSocket-соединения.
    pub bind_addr: SocketAddr,
    /// Строка подключения PostgreSQL: история и личные сообщения хранятся в базе.
    pub db_url: String,
}

impl Config {
    /// Собирает конфигурацию из переменных окружения.
    ///
    /// Возвращает ошибку с понятным текстом, если адрес или строка подключения
    /// заданы неверно — так проблему видно сразу при старте.
    pub fn from_env() -> Result<Self, Box<dyn Error + Send + Sync>> {
        // Адрес необязателен: без него слушаем все интерфейсы на порту 3000.
        let raw = env::var("CHAT_BIND").unwrap_or_else(|_| DEFAULT_BIND.to_owned());

        let bind_addr = raw.parse::<SocketAddr>().map_err(|err| {
            format!("CHAT_BIND должен быть адресом вида 0.0.0.0:3000 (получено `{raw}`): {err}")
        })?;

        // Без базы сервер не сможет хранить историю, поэтому переменная обязательна.
        let db_url = env::var("CHAT_DB_URL").map_err(
            |_| "Нужно задать CHAT_DB_URL, например postgres://postgres@localhost:5432/rust_chat",
        )?;

        Ok(Self { bind_addr, db_url })
    }
}
