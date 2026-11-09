//! Применение миграций базы данных.

use std::collections::HashSet;
use std::sync::LazyLock;

use sqlx::PgPool;
use sqlx::migrate::{MigrateError, Migrator};

/// Миграции, встроенные в бинарник.
///
/// `sqlx::migrate!` строит [`Migrator`] с `locking = true`, поэтому
/// [`run`] выполняет `pg_advisory_lock` на всё время применения (см.
/// `sqlx-core::migrate::migrator`, `sqlx-postgres::migrate::lock`). Не
/// отключаем блокировку и не добавляем свою: параллельные `serve` и
/// `tasklerq migrate` сериализуются на одной базе.
///
/// `ignore_missing` включён, чтобы образ стартовал на базе, где применена
/// миграция, которой в этом образе нет: откат тегом на более старый образ
/// после аддитивной миграции.
pub static MIGRATOR: LazyLock<Migrator> = LazyLock::new(|| {
    let mut migrator = sqlx::migrate!("./migrations");
    migrator.set_ignore_missing(true);
    migrator
});

/// Применить все ожидающие миграции под advisory lock.
///
/// Версии из `_sqlx_migrations`, которых нет в бинарнике, не считаются
/// ошибкой: они перечисляются в одном предупреждении, чтобы оператор видел,
/// что база новее образа.
pub async fn run(pool: &PgPool) -> Result<(), MigrateError> {
    MIGRATOR.run(pool).await?;

    let applied: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations ORDER BY version")
            .fetch_all(pool)
            .await
            .map_err(MigrateError::Execute)?;
    let unknown: Vec<i64> = applied
        .into_iter()
        .filter(|version| !MIGRATOR.version_exists(*version))
        .collect();
    if !unknown.is_empty() {
        tracing::warn!(
            unknown_versions = ?unknown,
            "database has migrations unknown to this image"
        );
    }

    Ok(())
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
