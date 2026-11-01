//! Применение миграций базы данных.

use std::collections::HashSet;

use sqlx::PgPool;
use sqlx::migrate::{MigrateError, Migrator};

/// Миграции, встроенные в бинарник.
///
/// `sqlx::migrate!` строит [`Migrator`] с `locking = true`, поэтому
/// [`run`] выполняет `pg_advisory_lock` на всё время применения (см.
/// `sqlx-core::migrate::migrator`, `sqlx-postgres::migrate::lock`). Не
/// отключаем блокировку и не добавляем свою: параллельные `serve` и
/// `tasklerq migrate` сериализуются на одной базе.
pub static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

/// Применить все ожидающие миграции под advisory lock.
pub async fn run(pool: &PgPool) -> Result<(), MigrateError> {
    MIGRATOR.run(pool).await
}

/// Все встроенные миграции применены успешно.
///
/// Проверяются обе стороны: нет записи с `success = false` и в
/// `_sqlx_migrations` есть каждая версия из бинарника. Отсутствующая таблица
/// (миграции ещё не запускались) тоже означает «не готов».
pub async fn all_applied(pool: &PgPool) -> Result<bool, sqlx::Error> {
    let rows: Vec<(i64, bool)> = sqlx::query_as("SELECT version, success FROM _sqlx_migrations")
        .fetch_all(pool)
        .await?;

    if rows.iter().any(|(_, success)| !success) {
        return Ok(false);
    }

    let applied: HashSet<i64> = rows.into_iter().map(|(version, _)| version).collect();
    Ok(MIGRATOR
        .iter()
        .all(|migration| applied.contains(&migration.version)))
}
