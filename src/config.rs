use std::{env, error::Error, net::SocketAddr};

pub const DEFAULT_BIND: &str = "0.0.0.0:3000";

#[derive(Debug, Clone)]
pub struct Config {
    pub bind_addr: SocketAddr,
    /// Строка подключения PostgreSQL: история и личные сообщения хранятся в базе.
    pub db_url: String,
}

impl Config {
    pub fn from_env() -> Result<Self, Box<dyn Error + Send + Sync>> {
        let raw = env::var("CHAT_BIND").unwrap_or_else(|_| DEFAULT_BIND.to_owned());

        let bind_addr = raw.parse::<SocketAddr>().map_err(|err| {
            format!("CHAT_BIND должен быть адресом вида 0.0.0.0:3000 (получено `{raw}`): {err}")
        })?;

        let db_url = env::var("CHAT_DB_URL").map_err(
            |_| "Нужно задать CHAT_DB_URL, например postgres://postgres@localhost:5432/rust_chat",
        )?;

        Ok(Self { bind_addr, db_url })
    }
}
