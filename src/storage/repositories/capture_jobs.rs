//! Очередь разбора текста через LLM.
//!
//! Задание захватывается арендой, а не удержанием транзакции: вызов LLM идёт
//! вне транзакции и может занимать десятки секунд. Счётчик попыток растёт при
//! захвате, поэтому «ядовитое» задание, которое роняет воркер, всё равно
//! исчерпает попытки. Текст пользователя хранится только пока задание ждёт
//! обработки: у завершённого и мёртвого заданий payload затирается.

use chrono::{DateTime, Duration, Utc};
use serde_json::{Value, json};
use sqlx::{FromRow, PgConnection};
use uuid::Uuid;

use crate::domain::models::CaptureSource;

/// Время аренды захваченного задания. Больше таймаута LLM (20 с) с запасом на
/// чтение контекста и запись результата.
pub const LEASE: Duration = Duration::seconds(60);

/// Верхняя граница задержки между попытками, секунды.
pub const MAX_BACKOFF_SECONDS: i64 = 60;

/// Вид входа, который разбирает воркер.
pub const INPUT_TEXT: &str = "text";

/// Новое задание.
#[derive(Debug, Clone)]
pub struct NewJob {
    /// Household.
    pub household_id: Uuid,
    /// Автор.
    pub user_id: Uuid,
    /// Чат, куда вернуть результат.
    pub chat_id: i64,
    /// Источник.
    pub source: CaptureSource,
    /// Вид входа.
    pub input_kind: &'static str,
    /// Данные входа.
    pub payload: Value,
}

impl NewJob {
    /// Задание на разбор текста из Telegram.
    pub fn telegram_text(household_id: Uuid, user_id: Uuid, chat_id: i64, text: &str) -> Self {
        Self {
            household_id,
            user_id,
            chat_id,
            source: CaptureSource::Telegram,
            input_kind: INPUT_TEXT,
            payload: json!({"text": text}),
        }
    }
}

/// Захваченное задание.
#[derive(Debug, Clone, FromRow)]
pub struct Job {
    /// Идентификатор.
    pub id: Uuid,
    /// Household.
    pub household_id: Uuid,
    /// Автор; пуст, если участника удалили.
    pub user_id: Option<Uuid>,
    /// Чат.
    pub chat_id: i64,
    /// Источник.
    pub source: CaptureSource,
    /// Вид входа.
    pub input_kind: String,
    /// Данные входа.
    pub payload: Value,
    /// Номер текущей попытки, начиная с 1.
    pub attempts: i32,
}

impl Job {
    /// Текст задания, если это текстовый вход.
    pub fn text(&self) -> Option<&str> {
        if self.input_kind == INPUT_TEXT {
            self.payload.get("text").and_then(Value::as_str)
        } else {
            None
        }
    }
}

/// Исход неудачной попытки.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// Повторить не раньше указанного времени.
    Retry(DateTime<Utc>),
    /// Попытки исчерпаны или повтор бессмысленен.
    Dead,
}

/// Состояние очереди.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    /// Ждут обработки.
    pub pending: i64,
    /// В работе.
    pub running: i64,
    /// Обработаны.
    pub done: i64,
    /// Закончили ошибкой.
    pub dead: i64,
}

/// Задержка перед повтором: 2, 4, 8, … секунд, не больше минуты.
pub fn backoff(attempts: i32) -> Duration {
    let exponent = u32::try_from(attempts.clamp(1, 16)).unwrap_or(16);
    Duration::seconds(2_i64.saturating_pow(exponent).min(MAX_BACKOFF_SECONDS))
}

/// Поставить задание в очередь.
pub async fn enqueue(
    conn: &mut PgConnection,
    job: NewJob,
    now: DateTime<Utc>,
) -> Result<Uuid, sqlx::Error> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO capture_jobs (
             id, household_id, user_id, chat_id, source, input_kind, payload,
             available_at, created_at
         )
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $8)",
    )
    .bind(id)
    .bind(job.household_id)
    .bind(job.user_id)
    .bind(job.chat_id)
    .bind(job.source)
    .bind(job.input_kind)
    .bind(job.payload)
    .bind(now)
    .execute(conn)
    .await?;
    Ok(id)
}

/// Захватить следующее готовое задание на время [`LEASE`].
///
/// Готово то, что ждёт и чьё время подошло, а также то, чья аренда истекла
/// (воркер упал). Задания, исчерпавшие попытки, сначала переводятся в `dead`;
/// их id возвращаются, чтобы вызывающий сообщил пользователю.
///
/// `only_chat` ограничивает выборку одним чатом: тесты делят одну базу.
pub async fn claim_next(
    conn: &mut PgConnection,
    now: DateTime<Utc>,
    max_attempts: u32,
    only_chat: Option<i64>,
) -> Result<Option<Job>, sqlx::Error> {
    sqlx::query(
        "UPDATE capture_jobs
         SET status = 'dead', last_error = 'lease_expired', finished_at = $1,
             payload = jsonb_build_object('redacted', true)
         WHERE status = 'running' AND locked_until <= $1 AND attempts >= $2
           AND ($3::bigint IS NULL OR chat_id = $3)",
    )
    .bind(now)
    .bind(i64::from(max_attempts))
    .bind(only_chat)
    .execute(&mut *conn)
    .await?;

    sqlx::query_as::<_, Job>(
        "UPDATE capture_jobs
         SET status = 'running', attempts = attempts + 1, locked_until = $1
         WHERE id = (
             SELECT id FROM capture_jobs
             WHERE (   (status = 'pending' AND available_at <= $2)
                    OR (status = 'running' AND locked_until <= $2))
               AND ($3::bigint IS NULL OR chat_id = $3)
             ORDER BY available_at, id
             LIMIT 1
             FOR UPDATE SKIP LOCKED
         )
         RETURNING id, household_id, user_id, chat_id, source, input_kind, payload, attempts",
    )
    .bind(now + LEASE)
    .bind(now)
    .bind(only_chat)
    .fetch_optional(conn)
    .await
}

/// Завершить задание успешно. Текст пользователя затирается.
pub async fn complete(
    conn: &mut PgConnection,
    job_id: Uuid,
    draft_id: Option<Uuid>,
    now: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE capture_jobs
         SET status = 'done', draft_id = $2, finished_at = $3, locked_until = NULL,
             last_error = NULL, payload = jsonb_build_object('redacted', true)
         WHERE id = $1",
    )
    .bind(job_id)
    .bind(draft_id)
    .bind(now)
    .execute(conn)
    .await?;
    Ok(())
}

/// Записать неудачу. `retryable` — повтор может помочь (сбой на стороне HA);
/// иначе, или когда попытки исчерпаны, задание уходит в `dead` вместе с
/// затиранием текста.
///
/// `error_code` — короткий код без пользовательских данных.
pub async fn record_failure(
    conn: &mut PgConnection,
    job: &Job,
    error_code: &str,
    retryable: bool,
    max_attempts: u32,
    now: DateTime<Utc>,
) -> Result<Failure, sqlx::Error> {
    let exhausted = job.attempts >= i32::try_from(max_attempts).unwrap_or(i32::MAX);
    if retryable && !exhausted {
        let retry_at = now + backoff(job.attempts);
        sqlx::query(
            "UPDATE capture_jobs
             SET status = 'pending', available_at = $2, locked_until = NULL, last_error = $3
             WHERE id = $1",
        )
        .bind(job.id)
        .bind(retry_at)
        .bind(error_code)
        .execute(conn)
        .await?;
        return Ok(Failure::Retry(retry_at));
    }

    sqlx::query(
        "UPDATE capture_jobs
         SET status = 'dead', last_error = $2, finished_at = $3, locked_until = NULL,
             payload = jsonb_build_object('redacted', true)
         WHERE id = $1",
    )
    .bind(job.id)
    .bind(error_code)
    .bind(now)
    .execute(conn)
    .await?;
    Ok(Failure::Dead)
}

/// Количество заданий по статусам.
pub async fn counts(conn: &mut PgConnection) -> Result<Counts, sqlx::Error> {
    let rows: Vec<(String, i64)> =
        sqlx::query_as("SELECT status::text, count(*) FROM capture_jobs GROUP BY status")
            .fetch_all(conn)
            .await?;
    let mut counts = Counts::default();
    for (status, count) in rows {
        match status.as_str() {
            "pending" => counts.pending = count,
            "running" => counts.running = count,
            "done" => counts.done = count,
            "dead" => counts.dead = count,
            _ => {}
        }
    }
    Ok(counts)
}

/// Удалить завершённые задания старше `cutoff`.
pub async fn purge(conn: &mut PgConnection, cutoff: DateTime<Utc>) -> Result<u64, sqlx::Error> {
    let deleted = sqlx::query("DELETE FROM capture_jobs WHERE finished_at < $1")
        .bind(cutoff)
        .execute(conn)
        .await?
        .rows_affected();
    Ok(deleted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_and_is_capped() {
        let seconds: Vec<i64> = (1..=8).map(|n| backoff(n).num_seconds()).collect();
        assert_eq!(seconds, [2, 4, 8, 16, 32, 60, 60, 60]);
    }
}
