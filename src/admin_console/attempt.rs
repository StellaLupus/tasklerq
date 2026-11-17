//! Проверка ввода под лимитом неудач.
//!
//! Слот [`Reservation`](crate::rate_limit::Reservation) держится не в
//! HTTP-обработчике, а в отдельной `tokio::spawn`-задаче, которая владеет
//! разрешением и сама решает `succeeded`/`failed`. Иначе отмена обработчика
//! (клиент закрыл соединение) освобождала бы слот через `Drop`, а `argon2` в
//! `spawn_blocking` дорабатывал бы уже без учёта: лимит «идущих проверок»
//! обходился бы, а задачи на 19 MiB накапливались бы.
//!
//! Задача проверки отвязана от обработчика: [`run`] ждёт её `JoinHandle`, но
//! отмена ожидания задачу не останавливает. Неудача попадёт в окно лимита
//! даже если ответ клиенту уже не нужен. Хук [`run_with_after`] выполняет
//! аудит в той же задаче после освобождения слота: событие о состоявшейся
//! смене пароля или создании учётки тоже переживает отмену обработчика.

use std::future::Future;
use std::hash::Hash;
use std::sync::Arc;

use chrono::{DateTime, Utc};

use crate::rate_limit::{RateDecision, RateLimiter, Reservation};

/// Итог проверки ввода.
pub enum Attempt<T, E> {
    /// Слот занят, проверка выполнена: `value` — её результат, `failed` —
    /// считать ли попытку неудачей.
    Checked {
        /// Результат проверки.
        value: T,
        /// Попытка неудачна; слот уже записан в окно.
        failed: bool,
    },
    /// Лимит исчерпан: проверка не выполнялась.
    Denied(RateDecision),
    /// Внутренняя ошибка (база, argon2) или падение задачи проверки.
    Error(E),
}

/// Выполнить проверку `check`, заняв слот лимита `limiter` для ключа `key`.
///
/// `check` возвращает `Ok((value, failed))`: `failed` определяет, превратится
/// ли слот в отметку неудачи. Ошибка `check` освобождает слот без отметки —
/// это не попытка ввода. `on_join_error` переводит падение задачи в ошибку
/// вызывающей стороны.
pub async fn run<K, T, E, F, Fut, P>(
    limiter: Arc<RateLimiter<K>>,
    key: K,
    now: DateTime<Utc>,
    check: F,
    on_join_error: P,
) -> Attempt<T, E>
where
    K: Eq + Hash + Clone + Send + 'static,
    T: Send + 'static,
    E: Send + 'static,
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = Result<(T, bool), E>> + Send,
    P: FnOnce(tokio::task::JoinError) -> E,
{
    run_with_after(
        limiter,
        key,
        now,
        check,
        |value, _| async move { value },
        on_join_error,
    )
    .await
}

/// [`run`] с хуком `after`, который выполняется внутри задачи проверки.
///
/// Хук получает результат и признак неудачи уже после `succeeded`/`failed`:
/// слот освобождён, но задача ещё жива, поэтому аудит дописывается даже при
/// отменённом HTTP-обработчике. Результат хук возвращает обратно. Ошибка самой
/// проверки хук не вызывает.
pub async fn run_with_after<K, T, E, F, Fut, A, Afut, P>(
    limiter: Arc<RateLimiter<K>>,
    key: K,
    now: DateTime<Utc>,
    check: F,
    after: A,
    on_join_error: P,
) -> Attempt<T, E>
where
    K: Eq + Hash + Clone + Send + 'static,
    T: Send + 'static,
    E: Send + 'static,
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = Result<(T, bool), E>> + Send,
    A: FnOnce(T, bool) -> Afut + Send + 'static,
    Afut: Future<Output = T> + Send,
    P: FnOnce(tokio::task::JoinError) -> E,
{
    let guard = match limiter.reserve(&key, now) {
        Reservation::Allowed(guard) => guard,
        Reservation::Denied(decision) => return Attempt::Denied(decision),
    };

    let task = tokio::spawn(async move {
        match check().await {
            Ok((value, failed)) => {
                if failed {
                    guard.failed();
                } else {
                    guard.succeeded();
                }
                let value = after(value, failed).await;
                Ok((value, failed))
            }
            Err(error) => {
                drop(guard);
                Err(error)
            }
        }
    });

    match task.await {
        Ok(Ok((value, failed))) => Attempt::Checked { value, failed },
        Ok(Err(error)) => Attempt::Error(error),
        Err(error) => Attempt::Error(on_join_error(error)),
    }
}
