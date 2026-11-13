//! Стандарт сравнения названий при проверке слота smoke-набором.
//!
//! Одна строка на слот: `strict` или `normalized`. Строки нет — `strict`,
//! поэтому миграция не заполняет таблицу, а прежнее поведение сохраняется.
//! Значение входит в отпечаток проверки: смена стандарта делает сохранённую
//! проверку устаревшей.

use sqlx::PgConnection;

/// Стандарт слота, когда строки в таблице нет.
pub const DEFAULT_TITLES: &str = "strict";

/// Стандарт сравнения названий для слота; нет строки — `strict`.
pub async fn titles(conn: &mut PgConnection, slot: &str) -> Result<String, sqlx::Error> {
    let titles: Option<String> =
        sqlx::query_scalar("SELECT titles FROM llm_slot_settings WHERE slot = $1")
            .bind(slot)
            .fetch_optional(conn)
            .await?;
    Ok(titles.unwrap_or_else(|| DEFAULT_TITLES.to_owned()))
}

/// Записать стандарт слота, заменив прежний.
pub async fn set_titles(
    conn: &mut PgConnection,
    slot: &str,
    titles: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO llm_slot_settings (slot, titles, updated_at)
         VALUES ($1, $2, now())
         ON CONFLICT (slot) DO UPDATE SET
             titles = EXCLUDED.titles,
             updated_at = EXCLUDED.updated_at",
    )
    .bind(slot)
    .bind(titles)
    .execute(conn)
    .await?;
    Ok(())
}
