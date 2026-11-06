//! Загруженные файлы: хранение на диске и безопасные имена.
//!
//! На диске файл называется UUID сообщения, к которому он прикреплён. Это
//! исключает подстановку пути через имя, которое прислал браузер: исходное
//! имя хранится только в базе и показывается пользователю.

use std::{fmt::Write as _, io, path::PathBuf};

use uuid::Uuid;

/// Сколько символов исходного имени файла сохраняем.
const MAX_FILENAME_CHARS: usize = 120;
/// Тип, который используется, если браузер не прислал корректный Content-Type.
pub const DEFAULT_CONTENT_TYPE: &str = "application/octet-stream";

/// Папка с вложениями и ограничение размера.
#[derive(Debug, Clone)]
pub struct Uploads {
    dir: PathBuf,
    max_bytes: u64,
}

impl Default for Uploads {
    /// Значения по умолчанию без `CHAT_UPLOAD_DIR`: папка `uploads`,
    /// максимум 10 МиБ; используется тестами и состоянием без конфигурации.
    fn default() -> Self {
        Self::new(
            PathBuf::from(crate::config::DEFAULT_UPLOAD_DIR),
            crate::config::DEFAULT_MAX_UPLOAD_MB * 1024 * 1024,
        )
    }
}

impl Uploads {
    /// Создаёт описание хранилища без обращения к диску.
    pub fn new(dir: PathBuf, max_bytes: u64) -> Self {
        Self { dir, max_bytes }
    }

    /// Создаёт папку вложений при старте сервера.
    pub async fn prepare(dir: PathBuf, max_bytes: u64) -> io::Result<Self> {
        tokio::fs::create_dir_all(&dir).await?;
        Ok(Self::new(dir, max_bytes))
    }

    /// Максимальный размер одного файла в байтах.
    pub fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    /// Путь к файлу сообщения.
    fn path(&self, id: Uuid) -> PathBuf {
        self.dir.join(id.to_string())
    }

    /// Записывает файл на диск.
    pub async fn save(&self, id: Uuid, bytes: &[u8]) -> io::Result<()> {
        tokio::fs::write(self.path(id), bytes).await
    }

    /// Читает файл; `None`, если его уже нет.
    pub async fn read(&self, id: Uuid) -> io::Result<Option<Vec<u8>>> {
        match tokio::fs::read(self.path(id)).await {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }
}

/// Оставляет от имени файла только безопасную часть.
///
/// Браузер может прислать путь (`C:\...\фото.png` или `../../etc/passwd`),
/// управляющие символы и слишком длинное имя. На диск это имя не попадает,
/// но в интерфейсе и заголовке скачивания оно должно быть предсказуемым.
pub fn sanitize_filename(raw: &str) -> String {
    // Берём только последний компонент пути: и `/`, и `\` считаются разделителями.
    let base = raw.rsplit(['/', '\\']).next().unwrap_or(raw);
    let cleaned: String = base.chars().filter(|c| !c.is_control()).collect();
    let trimmed = cleaned.trim();

    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        return "файл".to_owned();
    }

    trimmed.chars().take(MAX_FILENAME_CHARS).collect()
}

/// Проверяет Content-Type, который прислал браузер.
pub fn normalize_content_type(raw: Option<&str>) -> String {
    let Some(value) = raw else {
        return DEFAULT_CONTENT_TYPE.to_owned();
    };

    let value = value.trim();

    // Заголовок не должен содержать переводов строк и других управляющих
    // символов: иначе можно подставить лишние заголовки в ответ.
    if value.is_empty()
        || value.len() > 120
        || !value.chars().all(|c| c.is_ascii_graphic() || c == ' ')
    {
        return DEFAULT_CONTENT_TYPE.to_owned();
    }

    value.to_owned()
}

/// Значение заголовка `Content-Disposition` с исходным именем файла.
///
/// `filename*` передаёт UTF-8 имя по RFC 5987, а `filename` — ASCII-запасной
/// вариант для старых клиентов.
pub fn content_disposition(name: &str) -> String {
    let fallback: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ' ') {
                c
            } else {
                '_'
            }
        })
        .collect();

    format!(
        "attachment; filename=\"{fallback}\"; filename*=UTF-8''{}",
        percent_encode(name)
    )
}

/// Процентное кодирование UTF-8 строки для `filename*`.
fn percent_encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());

    for byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(*byte as char);
        } else {
            // Запись в String не может завершиться ошибкой.
            let _ = write!(encoded, "%{byte:02X}");
        }
    }

    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_paths_and_control_characters() {
        assert_eq!(sanitize_filename(r"C:\Users\me\фото.png"), "фото.png");
        assert_eq!(sanitize_filename("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_filename("отчёт\n2026.pdf"), "отчёт2026.pdf");
    }

    #[test]
    fn replaces_empty_and_dot_names() {
        assert_eq!(sanitize_filename(""), "файл");
        assert_eq!(sanitize_filename("   "), "файл");
        assert_eq!(sanitize_filename("."), "файл");
        assert_eq!(sanitize_filename(".."), "файл");
    }

    #[test]
    fn limits_filename_length() {
        let long = "я".repeat(500);
        assert_eq!(sanitize_filename(&long).chars().count(), MAX_FILENAME_CHARS);
    }

    #[test]
    fn rejects_broken_content_types() {
        assert_eq!(normalize_content_type(None), DEFAULT_CONTENT_TYPE);
        assert_eq!(normalize_content_type(Some("")), DEFAULT_CONTENT_TYPE);
        assert_eq!(
            normalize_content_type(Some("text/plain\r\nX-Evil: 1")),
            DEFAULT_CONTENT_TYPE
        );
        assert_eq!(normalize_content_type(Some("image/png")), "image/png");
    }

    #[test]
    fn encodes_unicode_filename() {
        let value = content_disposition("фото 1.png");
        assert!(value.starts_with("attachment; filename=\"____ 1.png\"; filename*=UTF-8''"));
        assert!(value.contains("%D1%84%D0%BE%D1%82%D0%BE"));
    }
}
