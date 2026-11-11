//! Журнал разборов LLM: только метаданные, без исходного текста.

use chrono::{DateTime, Utc};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::domain::models::{CaptureMode, CaptureSource};
use crate::llm::Attempt;

/// Успешный разбор.
pub const SUCCESS: &str = "success";
/// Разбор завершился ошибкой.
pub const FAILURE: &str = "failure";

/// Одна запись журнала.
#[derive(Debug, Clone)]
pub struct NewRun {
    /// Household.
    pub household_id: Uuid,
    /// Автор; у внешнего capture его может не быть.
    pub user_id: Option<Uuid>,
    /// Источник.
    pub source: CaptureSource,
    /// Режим.
    pub mode: CaptureMode,
    /// Версия промпта.
    pub prompt_version: Option<&'static str>,
    /// Имя слота (`primary`/`fallback`); `None` у разбора без вызова слота.
    pub slot: Option<&'static str>,
    /// Номер попытки в разборе, с единицы; `None` у разбора без вызова слота.
    pub attempt: Option<i16>,
    /// `SUCCESS` или `FAILURE`.
    pub status: &'static str,
    /// Короткий код ошибки без пользовательских данных.
    pub error_code: Option<&'static str>,
    /// Длительность вызова, миллисекунды.
    pub latency_ms: i64,
    /// Входные токены, если провайдер их вернул.
    pub input_tokens: Option<i32>,
    /// Выходные токены, если провайдер их вернул.
    pub output_tokens: Option<i32>,
}

/// Общие поля разбора, одинаковые у всех его попыток.
#[derive(Debug, Clone, Copy)]
pub struct RunFields {
    /// Household.
    pub household_id: Uuid,
    /// Автор; у внешнего capture его может не быть.
    pub user_id: Option<Uuid>,
    /// Источник.
    pub source: CaptureSource,
    /// Режим.
    pub mode: CaptureMode,
    /// Версия промпта.
    pub prompt_version: Option<&'static str>,
}

/// Построить записи журнала по попыткам цепочки слотов: одна строка на вызов.
///
/// Попытка хранит только метаданные ответа, поэтому текст пользователя и текст
/// модели в журнал не попадают.
pub fn runs_from_attempts(fields: RunFields, attempts: &[Attempt]) -> Vec<NewRun> {
    attempts
        .iter()
        .map(|attempt| NewRun {
            household_id: fields.household_id,
            user_id: fields.user_id,
            source: fields.source,
            mode: fields.mode,
            prompt_version: fields.prompt_version,
            slot: Some(attempt.slot),
            attempt: Some(i16::try_from(attempt.number).unwrap_or(i16::MAX)),
            status: if attempt.is_success() {
                SUCCESS
            } else {
                FAILURE
            },
            error_code: attempt.error_code,
            latency_ms: i64::try_from(attempt.duration.as_millis()).unwrap_or(i64::MAX),
            input_tokens: attempt
                .input_tokens
                .and_then(|tokens| i32::try_from(tokens).ok()),
            output_tokens: attempt
                .output_tokens
                .and_then(|tokens| i32::try_from(tokens).ok()),
        })
        .collect()
}

/// Записать разбор.
pub async fn record(
    conn: &mut PgConnection,
    run: NewRun,
    now: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO llm_runs (
             id, household_id, user_id, source, mode, prompt_version, status,
             error_code, latency_ms, input_tokens, output_tokens, slot, attempt,
             created_at
         )
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)",
    )
    .bind(Uuid::now_v7())
    .bind(run.household_id)
    .bind(run.user_id)
    .bind(run.source)
    .bind(run.mode)
    .bind(run.prompt_version)
    .bind(run.status)
    .bind(run.error_code)
    .bind(run.latency_ms)
    .bind(run.input_tokens)
    .bind(run.output_tokens)
    .bind(run.slot)
    .bind(run.attempt)
    .bind(now)
    .execute(conn)
    .await?;
    Ok(())
}

/// Сколько разборов household начато после `from`.
///
/// Квота считает разборы, а не вызовы слотов: разбор, в котором вызваны оба
/// слота, пишет строки с попытками 1 и 2, и учитывается только первая. Строки
/// без номера попытки (до 0018 и разборы без вызова слота) считаются как
/// раньше.
pub async fn count_since(
    conn: &mut PgConnection,
    household_id: Uuid,
    from: DateTime<Utc>,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT count(*) FROM llm_runs
         WHERE household_id = $1 AND created_at >= $2
           AND (attempt = 1 OR attempt IS NULL)",
    )
    .bind(household_id)
    .bind(from)
    .fetch_one(conn)
    .await
}

/// Удалить записи старше `cutoff`.
pub async fn purge(conn: &mut PgConnection, cutoff: DateTime<Utc>) -> Result<u64, sqlx::Error> {
    Ok(sqlx::query("DELETE FROM llm_runs WHERE created_at < $1")
        .bind(cutoff)
        .execute(conn)
        .await?
        .rows_affected())
}
