//! Последняя проверка слота LLM smoke-набором.
//!
//! Одна строка на слот: повторный прогон `capture-eval` обновляет её, а не
//! добавляет новую. Отчёт хранится без текста фраз и ответов модели: только
//! результаты сравнения, причины провала и время.

use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::PgConnection;

/// Результат проверки слота.
#[derive(Debug, Clone, Copy)]
pub struct NewCheck<'a> {
    /// Имя слота: `primary` или `fallback`.
    pub slot: &'a str,
    /// Отпечаток конфигурации, контракта и набора.
    pub fingerprint: &'a str,
    /// Части отпечатка для статуса «устарел» в 9.4.
    pub fingerprint_parts: &'a Value,
    /// Сколько фраз пройдено.
    pub passed: usize,
    /// Сколько фраз всего.
    pub total: usize,
    /// 95-й перцентиль задержки в миллисекундах.
    pub p95_ms: u64,
    /// Отчёт проверки в JSON.
    pub report: &'a Value,
}

/// Записать результат проверки, заменив предыдущий для этого слота.
pub async fn upsert(
    conn: &mut PgConnection,
    check: NewCheck<'_>,
    now: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO llm_slot_checks (
             slot, fingerprint, fingerprint_parts, passed, total, p95_ms, report,
             checked_at
         )
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         ON CONFLICT (slot) DO UPDATE SET
             fingerprint = EXCLUDED.fingerprint,
             fingerprint_parts = EXCLUDED.fingerprint_parts,
             passed = EXCLUDED.passed,
             total = EXCLUDED.total,
             p95_ms = EXCLUDED.p95_ms,
             report = EXCLUDED.report,
             checked_at = EXCLUDED.checked_at",
    )
    .bind(check.slot)
    .bind(check.fingerprint)
    .bind(check.fingerprint_parts)
    .bind(i32::try_from(check.passed).unwrap_or(i32::MAX))
    .bind(i32::try_from(check.total).unwrap_or(i32::MAX))
    .bind(i32::try_from(check.p95_ms).unwrap_or(i32::MAX))
    .bind(check.report)
    .bind(now)
    .execute(conn)
    .await?;
    Ok(())
}
