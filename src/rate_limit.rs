//! Лимит частоты со скользящим окном.

use std::collections::{HashMap, VecDeque};
use std::hash::Hash;
use std::net::{IpAddr, Ipv6Addr};
use std::sync::Mutex;

use chrono::{DateTime, Duration, Utc};
use uuid::Uuid;

/// Окно лимита по умолчанию, секунды.
pub const DEFAULT_WINDOW_SECONDS: i64 = 60;

/// Решение лимита частоты.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateDecision {
    /// Операцию можно выполнять.
    pub allowed: bool,
    /// Через сколько секунд повторить; `None`, если операция разрешена.
    pub retry_after: Option<u64>,
}

/// Лимит частоты на ключ.
///
/// Состояние живёт в памяти процесса: сервис — один экземпляр, как и
/// [`CircuitBreaker`](crate::capture::breaker::CircuitBreaker). Окно скользящее,
/// поэтому переход через границу окна не сбрасывает счётчик.
///
/// Тип ключа задаётся вызывающей стороной: токен capture — `Uuid`, пользователь
/// Telegram — `i64`, адрес webhook — `IpAddr`.
#[derive(Debug)]
pub struct RateLimiter<K = Uuid> {
    limit: u32,
    window: Duration,
    state: Mutex<State<K>>,
}

#[derive(Debug)]
struct State<K> {
    hits: HashMap<K, VecDeque<DateTime<Utc>>>,
    /// Когда последний раз чистили всю карту. Полный проход по карте не
    /// должен повторяться на каждый запрос с новым ключом.
    last_cleanup: Option<DateTime<Utc>>,
}

impl<K> RateLimiter<K> {
    /// Создать лимитер с окном по умолчанию; ноль трактуется как один запрос.
    pub fn new(limit: u32) -> Self {
        Self::with_window(limit, DEFAULT_WINDOW_SECONDS)
    }

    /// Создать лимитер с явным окном в секундах.
    pub fn with_window(limit: u32, window_seconds: i64) -> Self {
        Self {
            limit: limit.max(1),
            window: Duration::seconds(window_seconds.max(1)),
            state: Mutex::new(State {
                hits: HashMap::new(),
                last_cleanup: None,
            }),
        }
    }

    /// Настроенный лимит операций в окне.
    pub fn limit(&self) -> u32 {
        self.limit
    }

    /// Длина окна в секундах.
    pub fn window_seconds(&self) -> i64 {
        self.window.num_seconds()
    }
}

impl<K: Eq + Hash + Clone> RateLimiter<K> {
    /// Хранится ли ключ `key` в карте; только для тестов.
    #[cfg(test)]
    fn tracks(&self, key: &K) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .hits
            .contains_key(key)
    }

    /// Сколько ключей сейчас в карте; только для тестов.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .hits
            .len()
    }

    /// Учесть операцию ключа `key` в момент `now`.
    ///
    /// Устаревшие ключи удаляются из карты не чаще раза в окно: полный проход
    /// по карте на каждый запрос с новым ключом был бы квадратичным. Отметки
    /// текущего ключа при этом обрезаются по окну всегда.
    pub fn check(&self, key: K, now: DateTime<Utc>) -> RateDecision {
        let cutoff = now - self.window;
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        if state
            .last_cleanup
            .is_none_or(|last| now - last >= self.window)
        {
            state.hits.retain(|_, queue| {
                while queue.front().is_some_and(|at| *at <= cutoff) {
                    queue.pop_front();
                }
                !queue.is_empty()
            });
            state.last_cleanup = Some(now);
        }

        let queue = state.hits.entry(key).or_default();
        while queue.front().is_some_and(|at| *at <= cutoff) {
            queue.pop_front();
        }
        if queue.len() >= self.limit as usize {
            let wait = queue
                .front()
                .map(|at| *at + self.window - now)
                .unwrap_or(self.window);
            return RateDecision {
                allowed: false,
                retry_after: Some(retry_after_seconds(wait)),
            };
        }
        queue.push_back(now);
        RateDecision {
            allowed: true,
            retry_after: None,
        }
    }
}

/// Округлить оставшееся время ожидания вверх до целых секунд.
///
/// Одна функция на все лимиты: `Retry-After`, ответ бота и поле
/// `retry_after` в аудите должны считать одинаково.
pub fn retry_after_seconds(wait: Duration) -> u64 {
    let mut seconds = wait.num_seconds();
    if wait.subsec_nanos() > 0 {
        seconds += 1;
    }
    u64::try_from(seconds.max(1)).unwrap_or(1)
}

/// Ключ лимита webhook для адреса клиента.
///
/// IPv4 считается целиком, IPv6 — префиксом /64: у одного клиента почти
/// всегда целая /64, и смена младших 64 бит не должна обходить лимит.
pub fn ip_key(ip: IpAddr) -> IpAddr {
    // IPv4-mapped адрес (`::ffff:203.0.113.7`) — это IPv4-клиент на
    // dual-stack-сокете: без приведения обнуление младших бит свело бы всех
    // таких клиентов в общий префикс `::`.
    let ip = ip.to_canonical();
    match ip {
        IpAddr::V4(_) => ip,
        IpAddr::V6(ip) => {
            let mut octets = ip.octets();
            octets[8..].fill(0);
            IpAddr::V6(Ipv6Addr::from(octets))
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Duration};
    use uuid::Uuid;

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

        // Другой ключ считается отдельно.
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
            !limiter.tracks(&stale),
            "a stale key must not stay in the map"
        );
        assert!(limiter.tracks(&fresh));
    }

    #[test]
    fn stale_keys_are_cleaned_up_once_per_window_not_on_every_call() {
        let limiter = RateLimiter::new(10);
        for _ in 0..50 {
            assert!(limiter.check(Uuid::now_v7(), at(1_000)).allowed);
        }
        assert_eq!(limiter.len(), 50);

        // Внутри окна новый ключ не запускает полный проход по карте.
        assert!(limiter.check(Uuid::now_v7(), at(1_001)).allowed);
        assert_eq!(
            limiter.len(),
            51,
            "cleanup must not run on every call inside the window"
        );

        // После окна устаревшие ключи удаляются, остаётся только текущий.
        assert!(limiter.check(Uuid::now_v7(), at(1_061)).allowed);
        assert_eq!(
            limiter.len(),
            1,
            "keys outside the window must be removed after the window"
        );
    }

    #[test]
    fn the_current_key_is_pruned_even_between_global_cleanups() {
        let limiter = RateLimiter::with_window(2, 10);
        let first = Uuid::now_v7();
        let other = Uuid::now_v7();

        // Первая чистка задаёт начало окна.
        assert!(limiter.check(other, at(95)).allowed);
        assert!(limiter.check(first, at(100)).allowed);
        assert!(limiter.check(first, at(101)).allowed);
        // Чистка через окно оставляет `first`: его отметки ещё внутри окна.
        assert!(limiter.check(other, at(105)).allowed);
        // Глобальная чистка ещё не наступила, но отметки `first` уже устарели.
        assert!(
            limiter.check(first, at(111)).allowed,
            "the current key must be pruned on every call"
        );
    }

    #[test]
    fn ipv4_is_used_whole_and_ipv6_by_the_64_bit_prefix() {
        let ipv4: IpAddr = "203.0.113.7".parse().expect("ipv4");
        assert_eq!(ip_key(ipv4), ipv4);

        let same_prefix: IpAddr = "2001:db8:1:2::1".parse().expect("ipv6");
        let other_host: IpAddr = "2001:db8:1:2:ffff:ffff:ffff:ffff".parse().expect("ipv6");
        assert_eq!(ip_key(same_prefix), ip_key(other_host));
        assert_eq!(
            ip_key(same_prefix),
            "2001:db8:1:2::".parse::<IpAddr>().expect("network")
        );

        let other_prefix: IpAddr = "2001:db8:1:3::1".parse().expect("ipv6");
        assert_ne!(ip_key(same_prefix), ip_key(other_prefix));

        // IPv4-mapped адрес — это IPv4-клиент, а не общий префикс `::`.
        let mapped: IpAddr = "::ffff:203.0.113.7".parse().expect("mapped");
        assert_eq!(ip_key(mapped), ipv4);
    }

    #[test]
    fn a_key_still_inside_the_window_keeps_its_hits() {
        let limiter = RateLimiter::new(10);
        let first = Uuid::now_v7();
        let second = Uuid::now_v7();

        assert!(limiter.check(first, at(1_000)).allowed);
        assert!(limiter.check(second, at(1_030)).allowed);

        assert!(limiter.tracks(&first));
        assert!(limiter.tracks(&second));
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
        assert!(!limiter.tracks(&stale));

        assert!(limiter.check(stale, start + Duration::seconds(61)).allowed);
        assert!(limiter.check(stale, start + Duration::seconds(62)).allowed);
        assert!(!limiter.check(stale, start + Duration::seconds(63)).allowed);
    }

    #[test]
    fn the_window_is_configurable() {
        let limiter = RateLimiter::with_window(2, 1);
        let key = 7_i64;

        assert!(limiter.check(key, at(1_000)).allowed);
        assert!(
            limiter
                .check(key, at(1_000) + Duration::milliseconds(500))
                .allowed
        );
        let denied = limiter.check(key, at(1_000) + Duration::milliseconds(700));
        assert!(!denied.allowed);
        assert_eq!(denied.retry_after, Some(1));

        // Через секунду от первой отметки окно освобождается.
        assert!(limiter.check(key, at(1_001)).allowed);
    }
}
