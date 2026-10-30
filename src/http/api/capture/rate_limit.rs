//! Лимит частоты внешнего capture.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use chrono::{DateTime, Duration, Utc};
use uuid::Uuid;

/// Окно лимита частоты.
const RATE_WINDOW_SECONDS: i64 = 60;

/// Решение лимита частоты.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateDecision {
    /// Запрос можно обрабатывать.
    pub allowed: bool,
    /// Через сколько секунд повторить; `None`, если запрос разрешён.
    pub retry_after: Option<u64>,
}

/// Лимит capture в минуту на токен.
///
/// Состояние живёт в памяти процесса: сервис — один экземпляр, как и
/// [`CircuitBreaker`](crate::capture::breaker::CircuitBreaker). Окно скользящее,
/// поэтому переход через границу минуты не сбрасывает счётчик.
#[derive(Debug)]
pub struct RateLimiter {
    limit: u32,
    hits: Mutex<HashMap<Uuid, VecDeque<DateTime<Utc>>>>,
}

impl RateLimiter {
    /// Создать лимитер; ноль трактуется как один запрос.
    pub fn new(limit: u32) -> Self {
        Self {
            limit: limit.max(1),
            hits: Mutex::new(HashMap::new()),
        }
    }

    /// Хранится ли ключ `key` в карте; только для тестов.
    #[cfg(test)]
    fn tracks(&self, key: Uuid) -> bool {
        self.hits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(&key)
    }

    /// Учесть запрос токена `key` в момент `now`.
    ///
    /// Перед учётом из карты удаляются ключи без отметок внутри окна: токены
    /// отзываются и пересоздаются, и их записи иначе остались бы навсегда.
    pub fn check(&self, key: Uuid, now: DateTime<Utc>) -> RateDecision {
        let cutoff = now - Duration::seconds(RATE_WINDOW_SECONDS);
        let mut hits = self
            .hits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        hits.retain(|_, queue| {
            while queue.front().is_some_and(|at| *at <= cutoff) {
                queue.pop_front();
            }
            !queue.is_empty()
        });
        let queue = hits.entry(key).or_default();
        if queue.len() >= self.limit as usize {
            let wait = queue
                .front()
                .map(|at| *at + Duration::seconds(RATE_WINDOW_SECONDS) - now)
                .unwrap_or_else(|| Duration::seconds(RATE_WINDOW_SECONDS));
            let mut seconds = wait.num_seconds();
            if wait.subsec_nanos() > 0 {
                seconds += 1;
            }
            return RateDecision {
                allowed: false,
                retry_after: Some(u64::try_from(seconds.max(1)).unwrap_or(1)),
            };
        }
        queue.push_back(now);
        RateDecision {
            allowed: true,
            retry_after: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(seconds, 0).expect("timestamp")
    }

    #[test]
    fn the_rate_limit_is_per_key_and_slides() {
        let limiter = RateLimiter::new(10);
        let key = Uuid::now_v7();
        let other = Uuid::now_v7();
        let start = at(1_000);

        for offset in 0..10 {
            assert!(
                limiter
                    .check(key, start + Duration::seconds(offset))
                    .allowed
            );
        }
        let denied = limiter.check(key, start + Duration::seconds(10));
        assert!(!denied.allowed);
        assert!(denied.retry_after.unwrap_or(0) >= 50);

        // Другой токен считается отдельно.
        assert!(limiter.check(other, start + Duration::seconds(10)).allowed);

        // Через минуту окно освобождается.
        assert!(limiter.check(key, start + Duration::seconds(61)).allowed);
    }

    #[test]
    fn a_key_outside_the_window_is_removed_from_the_rate_limiter() {
        let limiter = RateLimiter::new(10);
        let stale = Uuid::now_v7();
        let fresh = Uuid::now_v7();

        assert!(limiter.check(stale, at(1_000)).allowed);
        assert!(limiter.check(fresh, at(1_061)).allowed);

        assert!(
            !limiter.tracks(stale),
            "a stale key must not stay in the map"
        );
        assert!(limiter.tracks(fresh));
    }

    #[test]
    fn a_key_still_inside_the_window_keeps_its_hits() {
        let limiter = RateLimiter::new(10);
        let first = Uuid::now_v7();
        let second = Uuid::now_v7();

        assert!(limiter.check(first, at(1_000)).allowed);
        assert!(limiter.check(second, at(1_030)).allowed);

        assert!(limiter.tracks(first));
        assert!(limiter.tracks(second));
    }

    #[test]
    fn a_removed_key_counts_its_limit_from_zero() {
        let limiter = RateLimiter::new(2);
        let stale = Uuid::now_v7();
        let fresh = Uuid::now_v7();
        let start = at(1_000);

        assert!(limiter.check(stale, start).allowed);
        assert!(limiter.check(stale, start + Duration::seconds(1)).allowed);
        assert!(!limiter.check(stale, start + Duration::seconds(2)).allowed);

        assert!(limiter.check(fresh, start + Duration::seconds(61)).allowed);
        assert!(!limiter.tracks(stale));

        assert!(limiter.check(stale, start + Duration::seconds(61)).allowed);
        assert!(limiter.check(stale, start + Duration::seconds(62)).allowed);
        assert!(!limiter.check(stale, start + Duration::seconds(63)).allowed);
    }
}
