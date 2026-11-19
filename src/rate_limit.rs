//! Лимит частоты со скользящим окном.

use std::collections::{HashMap, VecDeque};
use std::hash::Hash;
use std::net::{IpAddr, Ipv6Addr};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Duration, Utc};
use uuid::Uuid;

/// Окно лимита по умолчанию, секунды.
pub const DEFAULT_WINDOW_SECONDS: i64 = 60;
/// `Retry-After`, когда отказ вызван только идущими проверками.
///
/// Окно в этом случае ещё не исчерпано: как только параллельная проверка
/// завершится, слот освободится. Называть всё окно (до 15 минут) клиенту,
/// который просто попал в параллельный запрос, незачем.
pub const IN_FLIGHT_RETRY_SECONDS: u64 = 1;

/// Решение лимита частоты.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateDecision {
    /// Операцию можно выполнять.
    pub allowed: bool,
    /// Через сколько секунд повторить; `None`, если операция разрешена.
    pub retry_after: Option<u64>,
    /// Точный момент освобождения слота; `None`, если операция разрешена.
    pub retry_at: Option<DateTime<Utc>>,
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
    /// Сколько проверок ключа уже выполняется. Они тоже занимают слоты:
    /// иначе параллельные запросы проходили бы лимит все сразу, пока ни одна
    /// неудача ещё не записана.
    in_flight: HashMap<K, u32>,
    /// Когда последний раз чистили всю карту. Полный проход по карте не
    /// должен повторяться на каждый запрос с новым ключом.
    last_cleanup: Option<DateTime<Utc>>,
}

/// Итог резервирования слота проверки.
///
/// [`Reservation::Denied`] содержит решение с `Retry-After`: проверку ввода
/// выполнять нельзя.
pub enum Reservation<K: Eq + Hash + Clone> {
    /// Слот занят до [`RateGuard::succeeded`] или [`RateGuard::failed`]; если
    /// разрешение просто уничтожить, слот освобождается без отметки неудачи.
    Allowed(RateGuard<K>),
    /// Лимит уже исчерпан записанными неудачами или идущими проверками.
    Denied(RateDecision),
}

/// Занятый слот проверки ввода.
///
/// Пока разрешение живо, параллельные проверки того же ключа его видят и не
/// проходят лимит все сразу. Успех освобождает слот без отметки, неудача
/// превращает его в запись окна. Внутренняя ошибка (база, argon2) разрешение
/// просто роняет: это не попытка ввода и лимит она не расходует.
///
/// Лимитер хранится в [`Arc`], а не заимствуется: разрешение можно передать в
/// `tokio::spawn`, чтобы проверка и её отметка пережили отмену HTTP-обработчика.
pub struct RateGuard<K: Eq + Hash + Clone> {
    limiter: Arc<RateLimiter<K>>,
    key: K,
    now: DateTime<Utc>,
    armed: bool,
}

impl<K: Eq + Hash + Clone> RateGuard<K> {
    /// Ввод удался: слот освобождается, неудача не записывается.
    pub fn succeeded(mut self) {
        self.armed = false;
        self.limiter.release(&self.key);
    }

    /// Ввод не удался: слот превращается в отметку неудачи.
    pub fn failed(mut self) {
        self.armed = false;
        self.limiter.record_hit(&self.key, self.now);
    }

    /// Операция состоялась и должна считаться в окне.
    ///
    /// Нужен лимитам, которые ограничивают успешные операции (например, выпуск
    /// ссылок восстановления), а не неудачные попытки ввода: в отличие от
    /// [`RateGuard::succeeded`], отметка попадает в окно.
    pub fn recorded(mut self) {
        self.armed = false;
        self.limiter.record_hit(&self.key, self.now);
    }
}

impl<K: Eq + Hash + Clone> Drop for RateGuard<K> {
    fn drop(&mut self) {
        if self.armed {
            self.limiter.release(&self.key);
        }
    }
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
                in_flight: HashMap::new(),
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

        self.cleanup(&mut state, now, cutoff);

        let queue = state.hits.entry(key).or_default();
        prune(queue, cutoff);
        if let Some(denied) = self.denied(queue, now) {
            return denied;
        }
        queue.push_back(now);
        allowed()
    }

    /// Занять слот проверки ввода, учитывая уже идущие проверки.
    ///
    /// Без этого параллельные запросы все прошли бы лимит, пока ни одна
    /// неудача не записана, и лимит обходился бы всплеском. Занятый слот
    /// освобождает [`RateGuard::succeeded`] (успех) или занимает
    /// [`RateGuard::failed`] (неудача); простое уничтожение разрешения
    /// освобождает слот без отметки. Проверка без учёта неудачи — тот же путь
    /// с завершением `succeeded` или `failed`.
    ///
    /// Лимитер принимается через [`Arc`], чтобы разрешение можно было передать
    /// в `tokio::spawn` и слот держался до конца проверки, даже если
    /// HTTP-обработчик отменён.
    pub fn reserve(self: &Arc<Self>, key: &K, now: DateTime<Utc>) -> Reservation<K> {
        let cutoff = now - self.window;
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        self.cleanup(&mut state, now, cutoff);

        let occupied = {
            let hits = state.hits.get_mut(key).map_or(0, |queue| {
                prune(queue, cutoff);
                queue.len()
            });
            hits + usize::try_from(state.in_flight.get(key).copied().unwrap_or(0)).unwrap_or(0)
        };
        if occupied >= self.limit as usize {
            let empty = VecDeque::new();
            let queue = state.hits.get(key).unwrap_or(&empty);
            return Reservation::Denied(self.denied(queue, now).unwrap_or(RateDecision {
                allowed: false,
                retry_after: Some(IN_FLIGHT_RETRY_SECONDS),
                retry_at: Some(now + Duration::seconds(IN_FLIGHT_RETRY_SECONDS as i64)),
            }));
        }

        *state.in_flight.entry(key.clone()).or_default() += 1;
        Reservation::Allowed(RateGuard {
            limiter: Arc::clone(self),
            key: key.clone(),
            now,
            armed: true,
        })
    }

    /// Освободить занятый слот без отметки неудачи.
    fn release(&self, key: &K) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        release_slot(&mut state.in_flight, key);
    }

    /// Освободить занятый слот и записать отметку в окно.
    fn record_hit(&self, key: &K, now: DateTime<Utc>) {
        let cutoff = now - self.window;
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        release_slot(&mut state.in_flight, key);
        self.cleanup(&mut state, now, cutoff);

        let queue = state.hits.entry(key.clone()).or_default();
        prune(queue, cutoff);
        queue.push_back(now);
    }

    /// Удалить устаревшие ключи, но не чаще раза в окно.
    fn cleanup(&self, state: &mut State<K>, now: DateTime<Utc>, cutoff: DateTime<Utc>) {
        if state
            .last_cleanup
            .is_none_or(|last| now - last >= self.window)
        {
            state.hits.retain(|_, queue| {
                prune(queue, cutoff);
                !queue.is_empty()
            });
            state.last_cleanup = Some(now);
        }
    }

    /// Отказ, если в окне уже лимит отметок.
    fn denied(&self, queue: &VecDeque<DateTime<Utc>>, now: DateTime<Utc>) -> Option<RateDecision> {
        if queue.len() < self.limit as usize {
            return None;
        }
        let retry_at = queue.front().map(|at| *at + self.window);
        let wait = retry_at.map_or(self.window, |at| at - now);
        Some(RateDecision {
            allowed: false,
            retry_after: Some(retry_after_seconds(wait)),
            retry_at,
        })
    }
}

/// Удалить отметки старше окна.
fn prune(queue: &mut VecDeque<DateTime<Utc>>, cutoff: DateTime<Utc>) {
    while queue.front().is_some_and(|at| *at <= cutoff) {
        queue.pop_front();
    }
}

/// Уменьшить счётчик идущих проверок ключа, удалив нулевую запись.
fn release_slot<K: Eq + Hash + Clone>(in_flight: &mut HashMap<K, u32>, key: &K) {
    if let std::collections::hash_map::Entry::Occupied(mut entry) = in_flight.entry(key.clone()) {
        let count = entry.get_mut();
        *count = count.saturating_sub(1);
        if *count == 0 {
            entry.remove();
        }
    }
}

const fn allowed() -> RateDecision {
    RateDecision {
        allowed: true,
        retry_after: None,
        retry_at: None,
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
        let limiter = Arc::new(RateLimiter::new(10));
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
        let limiter = Arc::new(RateLimiter::new(10));
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
        let limiter = Arc::new(RateLimiter::new(10));
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
        let limiter = Arc::new(RateLimiter::with_window(2, 10));
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
        let limiter = Arc::new(RateLimiter::new(10));
        let first = Uuid::now_v7();
        let second = Uuid::now_v7();

        assert!(limiter.check(first, at(1_000)).allowed);
        assert!(limiter.check(second, at(1_030)).allowed);

        assert!(limiter.tracks(&first));
        assert!(limiter.tracks(&second));
    }

    #[test]
    fn a_removed_key_counts_its_limit_from_zero() {
        let limiter = Arc::new(RateLimiter::new(2));
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
    fn the_slot_release_moment_is_exact_and_two_denials_share_it() {
        let limiter = Arc::new(RateLimiter::with_window(2, 60));
        let key = 7_i64;
        let start = at(1_000) + Duration::milliseconds(250);
        assert!(limiter.check(key, start).allowed);
        assert!(
            limiter
                .check(key, start + Duration::milliseconds(500))
                .allowed
        );

        // Слот освобождает самая старая отметка в окне — без округления до
        // целых секунд: иначе ответы формы могли бы переставиться.
        let first = limiter.check(key, start + Duration::milliseconds(700));
        assert!(!first.allowed);
        assert_eq!(first.retry_at, Some(start + Duration::seconds(60)));
        assert_eq!(first.retry_after, Some(60));

        let second = limiter.check(key, start + Duration::seconds(5));
        assert!(!second.allowed);
        assert_eq!(second.retry_at, first.retry_at);

        // В объявленный момент слот уже свободен.
        assert!(limiter.check(key, start + Duration::seconds(60)).allowed);
    }

    #[test]
    fn a_denied_check_does_not_consume_a_slot() {
        let limiter = Arc::new(RateLimiter::with_window(1, 60));
        let key = 7_i64;
        let start = at(2_000) + Duration::milliseconds(100);
        assert!(limiter.check(key, start).allowed);

        let denied = limiter.check(key, start + Duration::seconds(30));
        assert!(!denied.allowed);
        assert_eq!(denied.retry_at, Some(start + Duration::seconds(60)));

        // Если бы отказ записывался отметкой, в этот момент он ещё был бы в
        // окне и снова запретил бы операцию.
        assert!(limiter.check(key, start + Duration::seconds(60)).allowed);
    }

    /// Учесть неудачу через боевой путь: слот → `failed`.
    fn fail(limiter: &Arc<RateLimiter<i64>>, key: &i64, now: DateTime<Utc>) {
        let Reservation::Allowed(guard) = limiter.reserve(key, now) else {
            panic!("a failure inside the limit must reserve a slot");
        };
        guard.failed();
    }

    #[test]
    fn a_check_without_a_recorded_outcome_does_not_consume_the_limit() {
        let limiter = Arc::new(RateLimiter::with_window(2, 60));
        let key = 7_i64;
        let start = at(3_000);

        for _ in 0..10 {
            let Reservation::Allowed(guard) = limiter.reserve(&key, start) else {
                panic!("a successful check must not spend a failure slot");
            };
            guard.succeeded();
        }

        fail(&limiter, &key, start);
        fail(&limiter, &key, start + Duration::seconds(1));
        let Reservation::Denied(denied) = limiter.reserve(&key, start + Duration::seconds(2))
        else {
            panic!("the third failure must be refused");
        };
        assert_eq!(denied.retry_after, Some(58));
        assert_eq!(denied.retry_at, Some(start + Duration::seconds(60)));
    }

    #[test]
    fn five_failures_deny_the_sixth_attempt_and_the_window_slides() {
        let limiter = Arc::new(RateLimiter::with_window(5, 900));
        let key = 7_i64;
        let start = at(4_000);

        for offset in 0..5 {
            fail(&limiter, &key, start + Duration::seconds(offset));
        }
        let Reservation::Denied(denied) = limiter.reserve(&key, start + Duration::seconds(5))
        else {
            panic!("the sixth attempt must be refused");
        };
        assert!(denied.retry_after.unwrap_or(0) >= 895);

        // Окно скользит от самой старой неудачи, а не от начала отсчёта.
        assert!(matches!(
            limiter.reserve(&key, start + Duration::seconds(899)),
            Reservation::Denied(_)
        ));
        assert!(matches!(
            limiter.reserve(&key, start + Duration::seconds(900)),
            Reservation::Allowed(_)
        ));
    }

    #[test]
    fn the_check_hits_are_seen_by_reserve() {
        let limiter = Arc::new(RateLimiter::with_window(1, 60));
        let key = 7_i64;
        let start = at(5_000);

        assert!(limiter.check(key, start).allowed);
        assert!(
            matches!(
                limiter.reserve(&key, start + Duration::seconds(1)),
                Reservation::Denied(_)
            ),
            "check and reserve share the same counters"
        );
    }

    #[test]
    fn an_in_flight_check_occupies_a_slot_until_it_finishes() {
        let limiter = Arc::new(RateLimiter::with_window(1, 60));
        let key = 7_i64;
        let start = at(6_000);

        let Reservation::Allowed(guard) = limiter.reserve(&key, start) else {
            panic!("the first check must be allowed");
        };
        let Reservation::Denied(denied) = limiter.reserve(&key, start) else {
            panic!("a parallel check must not pass while the slot is busy");
        };
        assert!(!denied.allowed);
        assert_eq!(
            denied.retry_after,
            Some(1),
            "a denial caused only by a running check must not name the whole window"
        );
        assert_eq!(denied.retry_at, Some(start + Duration::seconds(1)));

        guard.succeeded();
        assert!(
            matches!(
                limiter.reserve(&key, start + Duration::seconds(1)),
                Reservation::Allowed(_)
            ),
            "a successful check returns the slot"
        );
    }

    #[test]
    fn a_failed_reservation_becomes_a_hit_and_a_dropped_one_does_not() {
        let limiter = Arc::new(RateLimiter::with_window(1, 60));
        let key = 7_i64;
        let start = at(7_000);

        {
            let Reservation::Allowed(_guard) = limiter.reserve(&key, start) else {
                panic!("the first check must be allowed");
            };
        }
        assert!(
            matches!(
                limiter.reserve(&key, start + Duration::seconds(1)),
                Reservation::Allowed(_)
            ),
            "dropping a permit means an internal error, not a failed input"
        );

        let Reservation::Allowed(guard) = limiter.reserve(&key, start + Duration::seconds(2))
        else {
            panic!("the slot is free after the dropped permit");
        };
        guard.failed();
        assert!(
            matches!(
                limiter.reserve(&key, start + Duration::seconds(2)),
                Reservation::Denied(_)
            ),
            "a failed check consumes the slot"
        );
        assert!(matches!(
            limiter.reserve(&key, start + Duration::seconds(3)),
            Reservation::Denied(_)
        ));
    }

    #[test]
    fn the_window_is_configurable() {
        let limiter = Arc::new(RateLimiter::with_window(2, 1));
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
