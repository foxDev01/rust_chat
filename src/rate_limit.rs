//! Ограничение частоты запросов к чувствительным эндпоинтам.
//!
//! Проверка построена на скользящем окне: для каждого ключа хранятся моменты
//! последних попыток. Ключ выбирает вызывающая сторона: для входа и
//! регистрации это IP клиента, для сообщений и загрузок — аккаунт. Как только
//! попыток становится больше лимита, сервер отвечает `429 Too Many Requests`
//! (или ошибкой в WebSocket) и просит подождать.
//!
//! Ограничение защищает не только от перебора паролей, но и от лишней
//! нагрузки: Argon2 намеренно медленный, а поток сообщений или файлов от
//! одного аккаунта способен занять диск и рабочие потоки.

use std::{
    collections::{HashMap, VecDeque},
    hash::Hash,
    net::IpAddr,
    time::{Duration, Instant},
};

use tokio::sync::Mutex;

/// Лимит попыток за промежуток времени.
#[derive(Debug, Clone, Copy)]
pub struct Quota {
    /// Сколько попыток разрешено в пределах окна.
    pub attempts: usize,
    /// Длина окна.
    pub window: Duration,
}

impl Quota {
    /// Создаёт лимит; нулевое число попыток запрещено.
    pub const fn new(attempts: usize, window: Duration) -> Self {
        assert!(attempts > 0, "лимит попыток должен быть больше нуля");
        Self { attempts, window }
    }
}

/// Вход в аккаунт: 10 попыток в минуту с одного адреса.
pub const LOGIN_QUOTA: Quota = Quota::new(10, Duration::from_secs(60));
/// Регистрация: 5 новых аккаунтов за 10 минут с одного адреса.
pub const REGISTER_QUOTA: Quota = Quota::new(5, Duration::from_secs(10 * 60));
/// Сообщения (публичные и личные): 10 за 10 секунд с одного аккаунта.
pub const MESSAGE_QUOTA: Quota = Quota::new(10, Duration::from_secs(10));
/// Загрузки файлов: 10 за минуту с одного аккаунта.
pub const UPLOAD_QUOTA: Quota = Quota::new(10, Duration::from_secs(60));
/// Неудачные входы: 20 за 15 минут на аккаунт.
///
/// Считаются только неудачи, поэтому успешный вход сбрасывает счётчик.
/// Лимит по IP не спасает от распределённого перебора, а этот — спасает
/// ценой того, что после исчерпания окна вход временно недоступен даже
/// с верным паролем.
pub const FAILED_LOGIN_QUOTA: Quota = Quota::new(20, Duration::from_secs(15 * 60));

/// Сколько ключей хранится до принудительной очистки устаревших записей.
const SWEEP_THRESHOLD: usize = 512;

/// Скользящее окно попыток по ключу.
///
/// По умолчанию ключ — IP-адрес: так устроены лимиты входа и регистрации.
/// Для лимитов аккаунта используется `RateLimiter<Uuid>`.
pub struct RateLimiter<K = IpAddr> {
    quota: Quota,
    buckets: Mutex<HashMap<K, VecDeque<Instant>>>,
}

impl<K: Eq + Hash> RateLimiter<K> {
    /// Создаёт новое независимое ограничение.
    pub fn new(quota: Quota) -> Self {
        Self {
            quota,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Регистрирует попытку для `key`.
    ///
    /// `Ok(())` — попытку можно выполнять. `Err(ожидание)` — лимит исчерпан,
    /// столько времени осталось до освобождения окна.
    pub async fn check(&self, key: K) -> Result<(), Duration> {
        let now = Instant::now();
        let mut buckets = self.buckets.lock().await;

        // Периодически выбрасываем ключи, которые давно не обращались,
        // иначе карта росла бы от разовых посетителей.
        if buckets.len() > SWEEP_THRESHOLD {
            sweep(&mut buckets, self.quota.window, now);
        }

        let hits = buckets.entry(key).or_default();
        prune(hits, self.quota.window, now);

        if hits.len() >= self.quota.attempts {
            // Самая старая попытка выйдет из окна раньше всех.
            let oldest = hits.front().copied().unwrap_or(now);
            let wait = self.quota.window.saturating_sub(now.duration_since(oldest));
            return Err(wait.max(Duration::from_secs(1)));
        }

        hits.push_back(now);
        Ok(())
    }

    /// Сколько осталось ждать, если ключ уже исчерпал лимит.
    ///
    /// В отличие от [`RateLimiter::check`] попытку не записывает: метод нужен,
    /// чтобы отклонить запрос до дорогой проверки пароля.
    pub async fn retry_after(&self, key: &K) -> Option<Duration> {
        let now = Instant::now();
        let mut buckets = self.buckets.lock().await;
        let hits = buckets.get_mut(key)?;

        prune(hits, self.quota.window, now);
        if hits.len() < self.quota.attempts {
            return None;
        }

        let oldest = hits.front().copied().unwrap_or(now);
        Some(
            self.quota
                .window
                .saturating_sub(now.duration_since(oldest))
                .max(Duration::from_secs(1)),
        )
    }

    /// Сбрасывает историю попыток ключа.
    ///
    /// Успешный вход очищает счётчик неудач, чтобы редкие опечатки не
    /// накапливались у активного пользователя.
    pub async fn reset(&self, key: &K) {
        self.buckets.lock().await.remove(key);
    }
}

/// Удаляет из очереди попытки, которые уже вышли из окна.
fn prune(hits: &mut VecDeque<Instant>, window: Duration, now: Instant) {
    while hits
        .front()
        .is_some_and(|at| now.duration_since(*at) >= window)
    {
        hits.pop_front();
    }
}

/// Удаляет пустые и устаревшие записи из всей карты.
fn sweep<K: Eq + Hash>(
    buckets: &mut HashMap<K, VecDeque<Instant>>,
    window: Duration,
    now: Instant,
) {
    buckets.retain(|_, hits| {
        prune(hits, window, now);
        !hits.is_empty()
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn ip(last: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(127, 0, 0, last))
    }

    #[tokio::test]
    async fn blocks_after_the_limit_and_recovers_after_the_window() {
        let limiter = RateLimiter::new(Quota::new(2, Duration::from_millis(40)));

        assert!(limiter.check(ip(1)).await.is_ok());
        assert!(limiter.check(ip(1)).await.is_ok());
        assert!(limiter.check(ip(1)).await.is_err());

        // Окно короткое: после паузы попытки снова разрешены.
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(limiter.check(ip(1)).await.is_ok());
    }

    #[tokio::test]
    async fn limits_each_address_separately() {
        let limiter = RateLimiter::new(Quota::new(1, Duration::from_secs(60)));

        assert!(limiter.check(ip(1)).await.is_ok());
        assert!(limiter.check(ip(1)).await.is_err());
        // Другой адрес не должен страдать из-за первого.
        assert!(limiter.check(ip(2)).await.is_ok());
    }

    #[tokio::test]
    async fn limits_each_account_separately() {
        let limiter: RateLimiter<uuid::Uuid> =
            RateLimiter::new(Quota::new(1, Duration::from_secs(60)));
        let first = uuid::Uuid::new_v4();
        let second = uuid::Uuid::new_v4();

        assert!(limiter.check(first).await.is_ok());
        assert!(limiter.check(first).await.is_err());
        // Второй аккаунт не исчерпал свой лимит чужими сообщениями.
        assert!(limiter.check(second).await.is_ok());
    }

    #[tokio::test]
    async fn reports_and_resets_a_blocked_key() {
        let limiter: RateLimiter<uuid::Uuid> =
            RateLimiter::new(Quota::new(1, Duration::from_secs(60)));
        let key = uuid::Uuid::new_v4();

        // Проверка без записи не расходует попытку.
        assert!(limiter.retry_after(&key).await.is_none());
        assert!(limiter.check(key).await.is_ok());
        assert!(limiter.retry_after(&key).await.is_some());

        limiter.reset(&key).await;
        assert!(limiter.retry_after(&key).await.is_none());
        assert!(limiter.check(key).await.is_ok());
    }
}
