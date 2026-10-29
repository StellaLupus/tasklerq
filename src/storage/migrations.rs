//! Применение миграций базы данных.

use sqlx::PgPool;
use sqlx::migrate::{MigrateError, Migrator};

/// Миграции, встроенные в бинарник.
pub static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

/// Применить все ожидающие миграции.
pub async fn run(pool: &PgPool) -> Result<(), MigrateError> {
    MIGRATOR.run(pool).await
}
