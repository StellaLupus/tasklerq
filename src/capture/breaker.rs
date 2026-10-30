//! Предохранитель вызовов LLM.
//!
//! Когда Home Assistant или LLM отвечает ошибкой, повторять вызов сразу
//! бессмысленно: это удваивает ожидание и стоимость. После нескольких ошибок
//! подряд предохранитель размыкается на короткое время, и capture переходит в
//! ручной режим, не обращаясь к LLM.
//!
//! Состояние живёт в памяти процесса: сервис — один экземпляр, а после
//! перезапуска счётчик ошибок начинать заново безопасно.

use std::collections::VecDeque;
use std::sync::Mutex;

use chrono::{DateTime, Duration, Utc};

/// Сколько ошибок за окно размыкают предохранитель.
pub const DEFAULT_THRESHOLD: usize = 5;
/// Окно, в котором считаются ошибки.
pub const DEFAULT_WINDOW: Duration = Duration::seconds(5 * 60);
/// Сколько предохранитель остаётся разомкнутым.
pub const DEFAULT_OPEN_FOR: Duration = Duration::seconds(2 * 60);

#[derive(Debug, Default)]
struct State {
    failures: VecDeque<DateTime<Utc>>,
    open_until: Option<DateTime<Utc>>,
}

/// Предохранитель вызовов LLM.
#[derive(Debug)]
pub struct CircuitBreaker {
    threshold: usize,
    window: Duration,
    open_for: Duration,
    state: Mutex<State>,
}

impl Default for CircuitBreaker {
    fn default() -> Self {
        Self::new(DEFAULT_THRESHOLD, DEFAULT_WINDOW, DEFAULT_OPEN_FOR)
    }
}

impl CircuitBreaker {
    /// Создать предохранитель с явными параметрами.
    pub fn new(threshold: usize, window: Duration, open_for: Duration) -> Self {
        Self {
            threshold: threshold.max(1),
            window: window.max(Duration::seconds(1)),
            open_for: open_for.max(Duration::seconds(1)),
            state: Mutex::new(State::default()),
        }
    }

    /// Можно ли сейчас вызывать LLM.
    pub fn allow(&self, now: DateTime<Utc>) -> bool {
        let mut state = self.lock();
        match state.open_until {
            Some(until) if now < until => false,
            Some(_) => {
                // Время размыкания вышло: пробуем один вызов заново.
                state.open_until = None;
                state.failures.clear();
                true
            }
            None => true,
        }
    }

    /// Вызов завершился успешно: счётчик ошибок сбрасывается.
    pub fn record_success(&self, _now: DateTime<Utc>) {
        let mut state = self.lock();
        state.failures.clear();
        state.open_until = None;
    }

    /// Вызов завершился ошибкой. При достижении порога предохранитель
    /// размыкается, а счётчик очищается: после паузы отсчёт начинается заново.
    pub fn record_failure(&self, now: DateTime<Utc>) {
        let mut state = self.lock();
        prune(&mut state, now, self.window);
        state.failures.push_back(now);
        if state.failures.len() >= self.threshold {
            state.failures.clear();
            state.open_until = Some(now + self.open_for);
        }
    }

    /// Сколько осталось до новой попытки; `None`, если вызов разрешён.
    pub fn retry_after(&self, now: DateTime<Utc>) -> Option<Duration> {
        let state = self.lock();
        state
            .open_until
            .filter(|until| *until > now)
            .map(|until| until - now)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn prune(state: &mut State, now: DateTime<Utc>, window: Duration) {
    let cutoff = now - window;
    while state
        .failures
        .front()
        .is_some_and(|failure| *failure < cutoff)
    {
        state.failures.pop_front();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(seconds, 0).expect("timestamp")
    }

    #[test]
    fn five_failures_open_the_breaker_for_two_minutes() {
        let breaker = CircuitBreaker::default();
        let now = at(1_000);

        for offset in 0..4 {
            breaker.record_failure(now + Duration::seconds(offset));
        }
        assert!(breaker.allow(now + Duration::seconds(4)));
        breaker.record_failure(now + Duration::seconds(5));

        assert!(!breaker.allow(now + Duration::seconds(6)));
        assert_eq!(
            breaker.retry_after(now + Duration::seconds(6)),
            Some(Duration::seconds(119))
        );
        // Через две минуты вызов снова разрешён.
        assert!(breaker.allow(now + Duration::seconds(125)));
    }

    #[test]
    fn failures_outside_the_window_do_not_count() {
        let breaker = CircuitBreaker::default();
        let now = at(1_000);

        for offset in 0..4 {
            breaker.record_failure(now + Duration::seconds(offset));
        }
        // Пятая ошибка приходит уже после окна: старые ошибки не в счёт.
        breaker.record_failure(now + Duration::seconds(301));
        assert!(breaker.allow(now + Duration::seconds(302)));
    }

    #[test]
    fn success_resets_the_failure_count() {
        let breaker = CircuitBreaker::default();
        let now = at(1_000);

        for offset in 0..4 {
            breaker.record_failure(now + Duration::seconds(offset));
        }
        breaker.record_success(now + Duration::seconds(4));
        breaker.record_failure(now + Duration::seconds(5));

        assert!(breaker.allow(now + Duration::seconds(6)));
    }
}
