//! Аутентификация: регистрация, вход, проверка сессий и выход.
//!
//! Пароли хранятся только в виде Argon2id-хешей, а клиент получает случайный
//! токен сессии. В базе лежит SHA-256 от токена, поэтому утечка таблицы
//! `sessions` не даёт готовых токенов для входа.

use std::{
    net::{IpAddr, SocketAddr},
    sync::OnceLock,
    time::Duration,
};

use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier, password_hash::SaltString};
use axum::{
    Json,
    extract::{ConnectInfo, State},
    http::{
        HeaderMap, StatusCode,
        header::{AUTHORIZATION, RETRY_AFTER},
    },
    response::{IntoResponse, Response},
};
use chrono::{DateTime, Utc};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use tracing::{error, warn};
use uuid::Uuid;

use crate::{protocol::UserName, state::ChatState};

/// Минимальная длина пароля.
pub const MIN_PASSWORD_LEN: usize = 8;
/// Максимальная длина пароля.
pub const MAX_PASSWORD_LEN: usize = 128;
/// Сколько живёт сессия входа.
pub const SESSION_TTL: Duration = Duration::from_secs(60 * 60 * 24 * 30);

/// Аккаунт без секретных данных.
#[derive(Debug, Clone, Serialize)]
pub struct Account {
    pub id: Uuid,
    pub username: String,
}

/// Созданная сессия: сам токен отдаётся клиенту только один раз.
#[derive(Debug, Clone)]
pub struct AuthSession {
    pub token: String,
    pub account: Account,
}

/// Ошибки аутентификации с готовым текстом для клиента.
#[derive(Debug)]
pub enum AuthError {
    /// Такое имя уже занято.
    UsernameTaken,
    /// Неверная пара имя/пароль (без подсказки, что именно не так).
    InvalidCredentials,
    /// Токен сессии отсутствует в базе или уже истёк.
    Unauthorized,
    /// Текущий пароль при смене пароля не совпал.
    WrongPassword,
    /// Аккаунты недоступны, потому что сервер запущен без базы.
    Unavailable,
    /// Непредвиденная ошибка хранилища.
    Internal(String),
}

impl AuthError {
    /// Безопасный для клиента текст ошибки.
    pub fn message(&self) -> &str {
        match self {
            Self::UsernameTaken => "Это имя уже занято",
            Self::InvalidCredentials => "Неверное имя или пароль",
            Self::Unauthorized => "Сессия истекла, войдите снова",
            Self::WrongPassword => "Неверный текущий пароль",
            Self::Unavailable => "Хранилище аккаунтов недоступно",
            Self::Internal(_) => "Внутренняя ошибка сервера",
        }
    }
}

/// Тело запросов `/api/register` и `/api/login`.
#[derive(Debug, Deserialize)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

/// Тело запроса `/api/password`.
#[derive(Debug, Deserialize)]
pub struct PasswordChange {
    pub current_password: String,
    pub new_password: String,
}

/// Публичные данные пользователя.
#[derive(Debug, Serialize)]
pub struct UserView {
    pub id: Uuid,
    pub username: String,
}

impl From<Account> for UserView {
    fn from(account: Account) -> Self {
        Self {
            id: account.id,
            username: account.username,
        }
    }
}

/// Ответ на успешный вход или регистрацию.
#[derive(Debug, Serialize)]
pub struct AuthResponse {
    pub token: String,
    pub user: UserView,
}

/// Ответ с ошибкой API в едином формате `{ "error": "..." }`.
///
/// Для ответов `429` дополнительно выставляется заголовок `Retry-After`,
/// чтобы клиент знал, через сколько секунд можно повторить запрос.
/// Поля скрыты: другие модули (например, `routes` для загрузки файлов)
/// строят ответы через `api_error` и возвращают уже готовый тип.
pub(crate) struct ApiError {
    status: StatusCode,
    message: String,
    retry_after: Option<u64>,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut response = (self.status, Json(json!({ "error": self.message }))).into_response();

        if let Some(seconds) = self.retry_after {
            response.headers_mut().insert(RETRY_AFTER, seconds.into());
        }

        response
    }
}

/// `POST /api/register` — создаёт аккаунт и сразу открывает сессию.
pub async fn register(
    State(state): State<ChatState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(credentials): Json<Credentials>,
) -> Result<Json<AuthResponse>, ApiError> {
    state
        .check_register_rate(client_ip(addr, &headers))
        .await
        .map_err(|wait| rate_limited(wait, "регистрации"))?;

    let username = UserName::parse(&credentials.username)
        .map_err(|reason| api_error(StatusCode::BAD_REQUEST, reason))?;
    validate_password(&credentials.password)
        .map_err(|reason| api_error(StatusCode::BAD_REQUEST, reason))?;

    let session = state
        .register(&username, &credentials.password)
        .await
        .map_err(auth_api_error)?;

    Ok(Json(AuthResponse {
        token: session.token,
        user: session.account.into(),
    }))
}

/// `POST /api/login` — проверяет пароль и открывает новую сессию.
pub async fn login(
    State(state): State<ChatState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(credentials): Json<Credentials>,
) -> Result<Json<AuthResponse>, ApiError> {
    state
        .check_login_rate(client_ip(addr, &headers))
        .await
        .map_err(|wait| rate_limited(wait, "входа"))?;

    let session = state
        .login(&credentials.username, &credentials.password)
        .await
        .map_err(auth_api_error)?;

    Ok(Json(AuthResponse {
        token: session.token,
        user: session.account.into(),
    }))
}

/// `GET /api/me` — возвращает владельца токена.
pub async fn me(
    State(state): State<ChatState>,
    headers: HeaderMap,
) -> Result<Json<UserView>, ApiError> {
    let token = bearer_token(&headers)
        .ok_or_else(|| api_error(StatusCode::UNAUTHORIZED, "Требуется вход"))?;

    let account = state
        .authenticate(&token)
        .await
        .ok_or_else(|| api_error(StatusCode::UNAUTHORIZED, "Сессия истекла, войдите снова"))?;

    Ok(Json(account.into()))
}

/// `POST /api/logout` — отзывает текущую сессию, если токен передан.
pub async fn logout(State(state): State<ChatState>, headers: HeaderMap) -> StatusCode {
    if let Some(token) = bearer_token(&headers) {
        state.logout(&token).await;
    }

    StatusCode::NO_CONTENT
}

/// `POST /api/password` — меняет пароль и отзывает остальные сессии аккаунта.
pub async fn change_password(
    State(state): State<ChatState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<PasswordChange>,
) -> Result<StatusCode, ApiError> {
    let token = bearer_token(&headers)
        .ok_or_else(|| api_error(StatusCode::UNAUTHORIZED, "Требуется вход"))?;

    validate_password(&body.new_password)
        .map_err(|reason| api_error(StatusCode::BAD_REQUEST, reason))?;

    // Проверка текущего пароля тоже считается попыткой входа: без лимита
    // его можно было бы подбирать с украденным токеном.
    state
        .check_login_rate(client_ip(addr, &headers))
        .await
        .map_err(|wait| rate_limited(wait, "смены пароля"))?;

    state
        .change_password(&token, &body.current_password, &body.new_password)
        .await
        .map_err(auth_api_error)?;

    Ok(StatusCode::NO_CONTENT)
}

/// Возвращает владельца токена из `Authorization: Bearer ...`.
///
/// Общие HTTP-обработчики файлов пользуются ею, чтобы не дублировать проверку
/// сессии из `me`.
pub async fn account_from_headers(
    state: &ChatState,
    headers: &HeaderMap,
) -> Result<Account, ApiError> {
    let token = bearer_token(headers)
        .ok_or_else(|| api_error(StatusCode::UNAUTHORIZED, "Требуется вход"))?;

    state
        .authenticate(&token)
        .await
        .ok_or_else(|| api_error(StatusCode::UNAUTHORIZED, "Сессия истекла, войдите снова"))
}

/// Проверяет пароль перед хешированием.
pub fn validate_password(password: &str) -> Result<(), &'static str> {
    let length = password.chars().count();

    if !(MIN_PASSWORD_LEN..=MAX_PASSWORD_LEN).contains(&length) {
        return Err("Пароль должен быть от 8 до 128 символов");
    }

    if password.chars().all(char::is_whitespace) {
        return Err("Пароль не может состоять только из пробелов");
    }

    Ok(())
}

/// Считает Argon2id-хеш пароля. Операция намеренно медленная.
pub fn hash_password(password: &str) -> Result<String, String> {
    let mut salt_bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut salt_bytes);
    let salt = SaltString::encode_b64(&salt_bytes).map_err(|error| error.to_string())?;

    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|error| error.to_string())
}

/// Проверяет пароль по сохранённому хешу.
pub fn verify_password(password: &str, hash: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(hash) else {
        return false;
    };

    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

/// Хеширование в отдельном потоке, чтобы не блокировать async-рантайм.
pub async fn hash_password_async(password: String) -> Result<String, String> {
    tokio::task::spawn_blocking(move || hash_password(&password))
        .await
        .map_err(|error| error.to_string())?
}

/// Проверка пароля в отдельном потоке, чтобы не блокировать async-рантайм.
pub async fn verify_password_async(password: String, hash: String) -> bool {
    tokio::task::spawn_blocking(move || verify_password(&password, &hash))
        .await
        .unwrap_or(false)
}

/// Генерирует случайный токен сессии (32 байта в hex).
pub fn new_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// Хеш токена для хранения в базе.
pub fn token_hash(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

/// Момент истечения новой сессии.
pub fn session_expiry() -> DateTime<Utc> {
    Utc::now()
        + chrono::Duration::from_std(SESSION_TTL)
            .expect("SESSION_TTL всегда помещается в chrono::Duration")
}

/// Достаёт токен из заголовка `Authorization: Bearer ...`.
fn bearer_token(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(AUTHORIZATION)?.to_str().ok()?;
    let token = value.strip_prefix("Bearer ")?.trim();

    if token.is_empty() {
        None
    } else {
        Some(token.to_owned())
    }
}

/// IP клиента для лимитов.
///
/// За reverse proxy реальный адрес приходит в `X-Forwarded-For`, но доверять
/// этому заголовку можно только когда прокси действительно есть: иначе любой
/// клиент подделает адрес и обойдёт лимит. Включается через `CHAT_TRUST_PROXY`.
fn client_ip(addr: SocketAddr, headers: &HeaderMap) -> IpAddr {
    client_ip_in(trust_proxy(), addr, headers)
}

/// Проверка IP с явно заданным доверием к прокси (удобно тестировать).
fn client_ip_in(trust_proxy: bool, addr: SocketAddr, headers: &HeaderMap) -> IpAddr {
    if !trust_proxy {
        return addr.ip();
    }

    forwarded_ip(headers, "x-forwarded-for")
        .or_else(|| forwarded_ip(headers, "x-real-ip"))
        .unwrap_or_else(|| addr.ip())
}

/// Первый адрес из заголовка прокси.
fn forwarded_ip(headers: &HeaderMap, name: &str) -> Option<IpAddr> {
    headers
        .get(name)?
        .to_str()
        .ok()?
        .split(',')
        .next()?
        .trim()
        .parse()
        .ok()
}

/// Читает `CHAT_TRUST_PROXY` один раз за запуск.
fn trust_proxy() -> bool {
    static TRUST: OnceLock<bool> = OnceLock::new();

    *TRUST.get_or_init(|| {
        std::env::var("CHAT_TRUST_PROXY")
            .map(|value| {
                matches!(
                    value.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            })
            .unwrap_or(false)
    })
}

/// Ответ с ошибкой в едином для API формате; открыт другим модулям.
pub(crate) fn api_error(status: StatusCode, message: impl Into<String>) -> ApiError {
    ApiError {
        status,
        message: message.into(),
        retry_after: None,
    }
}

/// Ответ `429 Too Many Requests` с подсказкой, сколько ждать.
fn rate_limited(wait: Duration, action: &str) -> ApiError {
    let seconds = wait.as_secs().max(1);

    if seconds > 1 {
        warn!(seconds, %action, "too many authentication attempts");
    }

    ApiError {
        status: StatusCode::TOO_MANY_REQUESTS,
        message: format!("Слишком много попыток {action}. Повторите через {seconds} с."),
        retry_after: Some(seconds),
    }
}

/// Преобразует доменную ошибку в HTTP-ответ.
fn auth_api_error(error: AuthError) -> ApiError {
    let status = match &error {
        AuthError::UsernameTaken => StatusCode::CONFLICT,
        AuthError::InvalidCredentials => StatusCode::UNAUTHORIZED,
        AuthError::Unauthorized => StatusCode::UNAUTHORIZED,
        AuthError::WrongPassword => StatusCode::BAD_REQUEST,
        AuthError::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
        AuthError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };

    // Подробности внутренней ошибки пишем в лог, а не отдаём клиенту.
    if let AuthError::Internal(details) = &error {
        error!(%details, "authentication failed");
    }

    api_error(status, error.message())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_password_length_and_content() {
        assert!(validate_password("коротко").is_err());
        assert!(validate_password("        ").is_err());
        assert!(validate_password("надёжный пароль").is_ok());
    }

    #[test]
    fn hashes_and_verifies_passwords() {
        let hash = hash_password("секрет-123").unwrap();

        assert!(verify_password("секрет-123", &hash));
        assert!(!verify_password("другой-пароль", &hash));
        assert!(!verify_password("секрет-123", "не-хеш"));
    }

    #[test]
    fn tokens_are_unique_and_hashed() {
        let first = new_token();
        let second = new_token();

        assert_ne!(first, second);
        assert_eq!(first.len(), 64);
        assert_eq!(token_hash(&first).len(), 64);
        assert_ne!(token_hash(&first), token_hash(&second));
    }

    #[test]
    fn honours_forwarded_ip_only_behind_a_trusted_proxy() {
        let peer: SocketAddr = "127.0.0.1:5000".parse().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "203.0.113.7, 10.0.0.1".parse().unwrap());

        // Без доверия к прокси заголовок игнорируется: его легко подделать.
        assert_eq!(client_ip_in(false, peer, &headers), peer.ip());
        // С доверием берётся первый адрес в цепочке.
        assert_eq!(
            client_ip_in(true, peer, &headers),
            "203.0.113.7".parse::<IpAddr>().unwrap()
        );

        // Если заголовка нет, остаётся адрес TCP-соединения.
        assert_eq!(client_ip_in(true, peer, &HeaderMap::new()), peer.ip());
    }
}
