//! Журнал разборов LLM: только метаданные, без исходного текста.

use chrono::{DateTime, Utc};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::domain::models::{CaptureMode, CaptureSource};

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
    /// `SUCCESS` или `FAILURE`.
    pub status: &'static str,
    /// Короткий код ошибки без пользовательских данных.
    pub error_code: Option<&'static str>,
    /// Длительность вызова, миллисекунды.
    pub latency_ms: i64,
    /// Входные токены, если Home Assistant их вернул.
    pub input_tokens: Option<i32>,
    /// Выходные токены, если Home Assistant их вернул.
    pub output_tokens: Option<i32>,
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
             error_code, latency_ms, input_tokens, output_tokens, created_at
         )
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
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
    .bind(now)
    .execute(conn)
    .await?;
    Ok(())
}

/// Сколько разборов household начато после `from`.
pub async fn count_since(
    conn: &mut PgConnection,
    household_id: Uuid,
    from: DateTime<Utc>,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT count(*) FROM llm_runs WHERE household_id = $1 AND created_at >= $2")
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
