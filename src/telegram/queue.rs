//! Очередь входящих обновлений в PostgreSQL.
//!
//! Обновление обрабатывается в той же транзакции, где его строка заблокирована
//! `FOR UPDATE SKIP LOCKED`: эффекты и пометка `processed` фиксируются вместе,
//! а упавший воркер освобождает строку сам. Счётчик попыток и `dead`
//! сохраняются отдельным запросом после отката. Сырое обновление нужно только
//! пока оно ждёт обработки: в `processed` и `dead` payload затирается.

use chrono::{DateTime, Duration, Utc};
use serde_json::Value;
use sqlx::PgConnection;

/// Верхняя граница задержки между попытками, секунды.
pub const MAX_BACKOFF_SECONDS: i64 = 300;

/// Задание из очереди.
#[derive(Debug, Clone)]
pub struct Claimed {
    /// Идентификатор обновления Telegram.
    pub update_id: i64,
    /// Сырое обновление.
    pub payload: Value,
    /// Сколько раз обработка уже завершалась ошибкой.
    pub attempts: i32,
}

/// Исход неудачной попытки.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// Повторить не раньше указанного времени.
    Retry(DateTime<Utc>),
    /// Попытки исчерпаны.
    Dead,
}

/// Состояние очереди.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    /// Ждут обработки.
    pub pending: i64,
    /// Обработаны.
    pub processed: i64,
    /// В очереди ошибок.
    pub dead: i64,
}

/// Задержка перед следующей попыткой: 1, 2, 4, … секунд, не больше 5 минут.
pub fn backoff(failed_attempts: i32) -> Duration {
    let exponent = u32::try_from(failed_attempts.saturating_sub(1).clamp(0, 16)).unwrap_or(16);
    Duration::seconds(2_i64.saturating_pow(exponent).min(MAX_BACKOFF_SECONDS))
}

/// Поставить обновление в очередь. `false` — дубликат `update_id`.
pub async fn enqueue(
    conn: &mut PgConnection,
    update_id: i64,
    payload: &Value,
    now: DateTime<Utc>,
) -> Result<bool, sqlx::Error> {
    let inserted = sqlx::query(
        "INSERT INTO telegram_updates (update_id, payload, available_at, received_at)
         VALUES ($1, $2, $3, $3)
         ON CONFLICT (update_id) DO NOTHING",
    )
    .bind(update_id)
    .bind(payload)
    .bind(now)
    .execute(conn)
    .await?
    .rows_affected();

    Ok(inserted == 1)
}

/// Заблокировать следующее готовое обновление. Вызывать в транзакции.
///
/// `only_chat` ограничивает выборку одним чатом: так тесты, делящие одну
/// базу, не подбирают обновления друг друга. В работе сервиса — `None`.
pub async fn claim_next(
    conn: &mut PgConnection,
    now: DateTime<Utc>,
    only_chat: Option<i64>,
) -> Result<Option<Claimed>, sqlx::Error> {
    let row: Option<(i64, Value, i32)> = sqlx::query_as(
        "SELECT update_id, payload, attempts
         FROM telegram_updates
         WHERE status = 'pending' AND available_at <= $1
           AND ($2::bigint IS NULL OR COALESCE(
                   payload #>> '{message,chat,id}',
                   payload #>> '{callback_query,message,chat,id}'
               )::bigint = $2)
         ORDER BY available_at, update_id
         LIMIT 1
         FOR UPDATE SKIP LOCKED",
    )
    .bind(now)
    .bind(only_chat)
    .fetch_optional(conn)
    .await?;

    Ok(row.map(|(update_id, payload, attempts)| Claimed {
        update_id,
        payload,
        attempts,
    }))
}

/// Пометить обновление обработанным. Сырое обновление (текст пользователя, код
/// приглашения из `/start`) после обработки не хранится.
pub async fn mark_processed(
    conn: &mut PgConnection,
    update_id: i64,
    now: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE telegram_updates
         SET status = 'processed', processed_at = $2, last_error = NULL,
             payload = jsonb_build_object('redacted', true)
         WHERE update_id = $1",
    )
    .bind(update_id)
    .bind(now)
    .execute(conn)
    .await?;

    Ok(())
}

/// Записать неудачную попытку: отложить повтор или перенести в `dead`.
///
/// Одним запросом: счётчик, статус и время повтора не расходятся. Задержка
/// считается тем же правилом, что и [`backoff`]. Из `dead` payload затирается.
///
/// `error_code` — короткий код без пользовательских данных.
pub async fn record_failure(
    conn: &mut PgConnection,
    update_id: i64,
    error_code: &str,
    max_attempts: u32,
    now: DateTime<Utc>,
) -> Result<Failure, sqlx::Error> {
    let (status, available_at): (String, DateTime<Utc>) = sqlx::query_as(
        "UPDATE telegram_updates
         SET attempts = attempts + 1,
             last_error = $2,
             status = CASE WHEN attempts + 1 >= $3 THEN 'dead'::update_status ELSE status END,
             payload = CASE WHEN attempts + 1 >= $3
                            THEN jsonb_build_object('redacted', true) ELSE payload END,
             available_at = $4 + make_interval(
                 secs => LEAST(power(2, LEAST(attempts, 16)), $5::bigint))
         WHERE update_id = $1
         RETURNING status::text, available_at",
    )
    .bind(update_id)
    .bind(error_code)
    .bind(i64::from(max_attempts))
    .bind(now)
    .bind(MAX_BACKOFF_SECONDS)
    .fetch_one(conn)
    .await?;

    Ok(if status == "dead" {
        Failure::Dead
    } else {
        Failure::Retry(available_at)
    })
}

/// Количество обновлений по статусам.
pub async fn counts(conn: &mut PgConnection) -> Result<Counts, sqlx::Error> {
    let rows: Vec<(String, i64)> =
        sqlx::query_as("SELECT status::text, count(*) FROM telegram_updates GROUP BY status")
            .fetch_all(conn)
            .await?;

    let mut counts = Counts::default();
    for (status, count) in rows {
        match status.as_str() {
            "pending" => counts.pending = count,
            "processed" => counts.processed = count,
            "dead" => counts.dead = count,
            _ => {}
        }
    }
    Ok(counts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_and_is_capped() {
        let seconds: Vec<i64> = (1..=11).map(|n| backoff(n).num_seconds()).collect();
        assert_eq!(seconds, [1, 2, 4, 8, 16, 32, 64, 128, 256, 300, 300]);
    }
}
