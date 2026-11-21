//! Загруженные файлы: хранение на диске и безопасные имена.
//!
//! На диске файл называется UUID сообщения, к которому он прикреплён. Это
//! исключает подстановку пути через имя, которое прислал браузер: исходное
//! имя хранится только в базе и показывается пользователю.

use std::{
    fmt::Write as _,
    io,
    path::PathBuf,
    time::{Duration, SystemTime},
};

use tracing::warn;
use uuid::Uuid;

/// Сколько символов исходного имени файла сохраняем.
const MAX_FILENAME_CHARS: usize = 120;
/// Тип, который используется, если браузер не прислал корректный Content-Type.
pub const DEFAULT_CONTENT_TYPE: &str = "application/octet-stream";

/// Минимальный возраст файла, после которого его можно считать сиротой.
///
/// Файл записывается на диск раньше, чем появляется строка сообщения в базе.
/// Без этого запаса фоновая чистка могла бы удалить только что загруженный
/// файл, который ещё не успел попасть в базу.
pub const ORPHAN_GRACE: Duration = Duration::from_secs(60 * 60);

/// Папка с вложениями и ограничение размера.
#[derive(Debug, Clone)]
pub struct Uploads {
    dir: PathBuf,
    max_bytes: u64,
    /// Предел суммарного размера папки; 0 — без ограничения.
    quota_bytes: u64,
}

impl Default for Uploads {
    /// Значения по умолчанию без `CHAT_UPLOAD_DIR`: папка `uploads`,
    /// максимум 10 МиБ; используется тестами и состоянием без конфигурации.
    fn default() -> Self {
        Self::new(
            PathBuf::from(crate::config::DEFAULT_UPLOAD_DIR),
            crate::config::DEFAULT_MAX_UPLOAD_MB * 1024 * 1024,
        )
        .with_quota(crate::config::DEFAULT_UPLOAD_QUOTA_MB * 1024 * 1024)
    }
}

impl Uploads {
    /// Создаёт описание хранилища без обращения к диску.
    pub fn new(dir: PathBuf, max_bytes: u64) -> Self {
        Self {
            dir,
            max_bytes,
            quota_bytes: 0,
        }
    }

    /// Задаёт общий предел папки в байтах; 0 — без ограничения.
    pub fn with_quota(mut self, quota_bytes: u64) -> Self {
        self.quota_bytes = quota_bytes;
        self
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

    /// Предел суммарного размера папки в байтах; 0 — без ограничения.
    pub fn quota_bytes(&self) -> u64 {
        self.quota_bytes
    }

    /// Поместится ли ещё один файл такого размера.
    ///
    /// Размер папки считается обходом каталога: загрузки ограничены по частоте,
    /// поэтому держать отдельный счётчик ради этого не нужно. Ошибку чтения
    /// каталога считаем «место есть»: квота не должна ломать загрузку из-за
    /// временного сбоя файловой системы.
    pub async fn has_room_for(&self, incoming: u64) -> bool {
        if self.quota_bytes == 0 {
            return true;
        }

        match self.usage_bytes().await {
            Ok(used) => used.saturating_add(incoming) <= self.quota_bytes,
            Err(error) => {
                warn!(%error, "failed to measure the uploads directory");
                true
            }
        }
    }

    /// Путь к файлу сообщения.
    fn path(&self, id: Uuid) -> PathBuf {
        self.dir.join(id.to_string())
    }

    /// Записывает файл на диск.
    pub async fn save(&self, id: Uuid, bytes: &[u8]) -> io::Result<()> {
        tokio::fs::write(self.path(id), bytes).await
    }

    /// Открывает файл для потоковой отдачи; `None`, если его уже нет.
    ///
    /// Скачивание не читает файл целиком в память: `ReaderStream` отдаёт его
    /// частями, поэтому размер вложения не влияет на потребление RAM.
    pub async fn open(&self, id: Uuid) -> io::Result<Option<tokio::fs::File>> {
        match tokio::fs::File::open(self.path(id)).await {
            Ok(file) => Ok(Some(file)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Файлы, лежащие в папке: идентификатор сообщения и время изменения.
    ///
    /// Имена, которые не являются UUID, пропускаются: сервер сам создаёт только
    /// такие файлы, а посторонние в папке вложений не должны ломать чистку.
    pub async fn files(&self) -> io::Result<Vec<(Uuid, SystemTime)>> {
        let mut dir = tokio::fs::read_dir(&self.dir).await?;
        let mut files = Vec::new();

        while let Some(entry) = dir.next_entry().await? {
            let metadata = entry.metadata().await?;
            if !metadata.is_file() {
                continue;
            }

            let name = entry.file_name();
            let Some(id) = name.to_str().and_then(|name| Uuid::parse_str(name).ok()) else {
                warn!(
                    name = %name.to_string_lossy(),
                    "skipping a file with an unexpected name in the uploads directory"
                );
                continue;
            };

            // Если время изменения недоступно, файл выглядит свежим: лучше
            // оставить его до следующей чистки, чем удалить используемый.
            files.push((
                id,
                metadata.modified().unwrap_or_else(|_| SystemTime::now()),
            ));
        }

        Ok(files)
    }

    /// Удаляет файл; отсутствие файла ошибкой не считается.
    pub async fn remove(&self, id: Uuid) -> io::Result<()> {
        match tokio::fs::remove_file(self.path(id)).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Суммарный размер всех вложений на диске.
    ///
    /// Используется в `/api/health` и при проверке квоты: запросы редкие,
    /// поэтому обход папки дешевле, чем отдельный счётчик, который нужно
    /// синхронизировать с удалением файлов.
    pub async fn usage_bytes(&self) -> io::Result<u64> {
        let mut dir = tokio::fs::read_dir(&self.dir).await?;
        let mut total = 0u64;

        while let Some(entry) = dir.next_entry().await? {
            let metadata = entry.metadata().await?;
            if metadata.is_file() {
                total = total.saturating_add(metadata.len());
            }
        }

        Ok(total)
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

    #[tokio::test]
    async fn lists_and_removes_stored_files() {
        let dir = std::env::temp_dir().join(format!("rust-chat-uploads-{}", Uuid::new_v4()));
        let uploads = Uploads::prepare(dir.clone(), 1024)
            .await
            .expect("prepare uploads");
        let stored = Uuid::new_v4();

        uploads.save(stored, b"one").await.expect("save");
        // Посторонний файл не должен притворяться вложением.
        tokio::fs::write(dir.join("readme.txt"), b"x")
            .await
            .expect("write a foreign file");

        let files = uploads.files().await.expect("list files");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].0, stored);

        // Потоковое открытие видит те же байты, что были записаны.
        let mut file = uploads
            .open(stored)
            .await
            .expect("open")
            .expect("file exists");
        let mut bytes = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut file, &mut bytes)
            .await
            .expect("read");
        assert_eq!(bytes, b"one");

        uploads.remove(stored).await.expect("remove");
        // Повторное удаление безопасно: файла уже нет.
        uploads.remove(stored).await.expect("remove twice");
        assert!(uploads.files().await.expect("list").is_empty());
        assert!(uploads.open(stored).await.expect("open").is_none());

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn enforces_the_storage_quota() {
        let dir = std::env::temp_dir().join(format!("rust-chat-quota-{}", Uuid::new_v4()));
        let uploads = Uploads::prepare(dir.clone(), 1024)
            .await
            .expect("prepare uploads")
            .with_quota(10);

        // Пустая папка принимает файл в пределах квоты, но не больше неё.
        assert!(uploads.has_room_for(10).await);
        assert!(!uploads.has_room_for(11).await);

        uploads.save(Uuid::new_v4(), b"123456").await.expect("save");
        assert!(uploads.has_room_for(4).await);
        assert!(!uploads.has_room_for(5).await);

        // Ноль означает «без ограничения»: квота не мешает даже огромным файлам.
        let unlimited = Uploads::prepare(dir.clone(), 1024)
            .await
            .expect("prepare uploads")
            .with_quota(0);
        assert!(unlimited.has_room_for(u64::MAX).await);

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
