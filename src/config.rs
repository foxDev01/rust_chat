//! Конфигурация сервера, читаемая из переменных окружения.
//!
//! - `CHAT_BIND` — адрес и порт прослушивания (по умолчанию `0.0.0.0:3000`);
//! - `CHAT_DB_URL` — обязательная строка подключения к PostgreSQL;
//! - `CHAT_DB_POOL` — максимальное число соединений с базой (по умолчанию 5);
//! - `CHAT_UPLOAD_DIR` — папка для файлов, прикреплённых к сообщениям;
//! - `CHAT_MAX_UPLOAD_MB` — максимальный размер одного файла в мегабайтах;
//! - `CHAT_UPLOAD_QUOTA_MB` — общий предел папки вложений (0 — без ограничения);
//! - `CHAT_ALLOWED_ORIGINS`, `CHAT_TRUST_PROXY` читаются модулями `ws` и `auth`.

use std::{env, error::Error, net::SocketAddr, path::PathBuf};

/// Адрес, который используется, если `CHAT_BIND` не задан.
pub const DEFAULT_BIND: &str = "0.0.0.0:3000";
/// Папка для вложений, если `CHAT_UPLOAD_DIR` не задан.
pub const DEFAULT_UPLOAD_DIR: &str = "uploads";
/// Максимальный размер файла по умолчанию — 10 МиБ.
pub const DEFAULT_MAX_UPLOAD_MB: u64 = 10;
/// Верхняя граница настройки `CHAT_MAX_UPLOAD_MB`: защищает диск от случайной опечатки.
pub const MAX_UPLOAD_MB_LIMIT: u64 = 100;
/// Общий предел папки вложений по умолчанию — 1 ГиБ.
pub const DEFAULT_UPLOAD_QUOTA_MB: u64 = 1024;
/// Верхняя граница `CHAT_UPLOAD_QUOTA_MB` — 1 ТиБ.
pub const MAX_UPLOAD_QUOTA_MB: u64 = 1024 * 1024;
/// Сколько соединений с базой открывается, если `CHAT_DB_POOL` не задан.
pub const DEFAULT_DB_POOL: u32 = 5;
/// Верхняя граница пула: PostgreSQL не любит сотни соединений от одного процесса.
pub const MAX_DB_POOL_LIMIT: u32 = 32;

/// Настройки, необходимые серверу для запуска.
#[derive(Debug, Clone)]
pub struct Config {
    /// Адрес, на котором axum принимает HTTP- и WebSocket-соединения.
    pub bind_addr: SocketAddr,
    /// Строка подключения PostgreSQL: история и личные сообщения хранятся в базе.
    pub db_url: String,
    /// Максимальное число соединений в пуле.
    pub db_pool: u32,
    /// Папка, в которой лежат загруженные файлы.
    pub upload_dir: PathBuf,
    /// Максимальный размер одного вложения в байтах.
    pub max_upload_bytes: u64,
    /// Предел суммарного размера вложений в байтах; 0 — без ограничения.
    pub upload_quota_bytes: u64,
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

        // Вложения лежат рядом с сервером; папка создаётся при запуске.
        let upload_dir = env::var("CHAT_UPLOAD_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(DEFAULT_UPLOAD_DIR));

        // Пул ограничен, чтобы при всплеске активности сервер не открыл
        // соединений больше, чем разрешает PostgreSQL.
        let db_pool = match env::var("CHAT_DB_POOL") {
            Ok(raw) => raw
                .trim()
                .parse::<u32>()
                .map_err(|_| format!("CHAT_DB_POOL должен быть целым числом (получено `{raw}`)"))?,
            Err(_) => DEFAULT_DB_POOL,
        };

        if !(1..=MAX_DB_POOL_LIMIT).contains(&db_pool) {
            return Err(format!("CHAT_DB_POOL должен быть от 1 до {MAX_DB_POOL_LIMIT}").into());
        }

        let max_upload_mb = match env::var("CHAT_MAX_UPLOAD_MB") {
            Ok(raw) => raw.trim().parse::<u64>().map_err(|_| {
                format!("CHAT_MAX_UPLOAD_MB должен быть целым числом (получено `{raw}`)")
            })?,
            Err(_) => DEFAULT_MAX_UPLOAD_MB,
        };

        if !(1..=MAX_UPLOAD_MB_LIMIT).contains(&max_upload_mb) {
            return Err(
                format!("CHAT_MAX_UPLOAD_MB должен быть от 1 до {MAX_UPLOAD_MB_LIMIT}").into(),
            );
        }

        // Общая квота защищает диск: без неё папка вложений растёт, пока не
        // закончится место. Ноль означает «без ограничения».
        let upload_quota_mb = match env::var("CHAT_UPLOAD_QUOTA_MB") {
            Ok(raw) => raw.trim().parse::<u64>().map_err(|_| {
                format!("CHAT_UPLOAD_QUOTA_MB должен быть целым числом (получено `{raw}`)")
            })?,
            Err(_) => DEFAULT_UPLOAD_QUOTA_MB,
        };

        if upload_quota_mb > MAX_UPLOAD_QUOTA_MB {
            return Err(format!(
                "CHAT_UPLOAD_QUOTA_MB должен быть от 0 до {MAX_UPLOAD_QUOTA_MB} (0 — без ограничения)"
            )
            .into());
        }

        Ok(Self {
            bind_addr,
            db_url,
            db_pool,
            upload_dir,
            max_upload_bytes: max_upload_mb * 1024 * 1024,
            upload_quota_bytes: upload_quota_mb * 1024 * 1024,
        })
    }
}
