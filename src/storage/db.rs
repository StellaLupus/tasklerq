//! Пул соединений PostgreSQL.

use std::str::FromStr;
use std::time::Duration;

use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use thiserror::Error;

use crate::config::DatabaseConfig;

/// `statement_timeout` для всех соединений пула.
const STATEMENT_TIMEOUT: &str = "30s";
/// Таймаут получения соединения из пула.
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(5);

/// Ошибка подключения к базе.
#[derive(Debug, Error)]
pub enum DbError {
    /// Строка подключения не является допустимым postgres URL.
    #[error("DATABASE_URL must be a valid postgres:// connection string")]
    InvalidUrl,
    /// Не удалось подключиться к базе.
    #[error("could not connect to PostgreSQL")]
    Connect(#[source] sqlx::Error),
}

/// Создать пул и проверить соединение с базой.
///
/// Верхнюю границу пула проверяет конфигурация (5..=20), а здесь пул держит
/// минимум одно соединение. `statement_timeout` задаётся параметром соединения,
/// чтобы не собирать SQL из строки.
pub async fn connect(config: &DatabaseConfig) -> Result<PgPool, DbError> {
    let options = PgConnectOptions::from_str(config.url.expose())
        .map_err(|_| DbError::InvalidUrl)?
        .options([("statement_timeout", STATEMENT_TIMEOUT)]);

    PgPoolOptions::new()
        .max_connections(config.max_connections)
        .min_connections(1)
        .acquire_timeout(ACQUIRE_TIMEOUT)
        .connect_with(options)
        .await
        .map_err(DbError::Connect)
}
