//! Создание учётной записи администратора по setup-коду.
//!
//! Одна функция на весь сценарий: здесь же будут учитываться ограничение
//! частоты и аудит неудачного кода (5.6). Сейчас она атомарно гасит код и
//! создаёт учётку в одной транзакции; любая ошибка откатывает транзакцию и
//! возвращает код в неиспользованное состояние.
//!
//! Ошибка кода не различает причину: повторный, погашенный, чужой по назначению
//! и неизвестный код дают [`SetupError::InvalidCode`]. Если учётка уже есть (в
//! том числе появилась параллельно), [`SetupError::AlreadyConfigured`], а код
//! остаётся действующим.

use chrono::{DateTime, Utc};

use crate::admin_console::codes::{self, Purpose};
use crate::admin_console::router::AdminState;
use crate::domain::error::DomainError;
use crate::storage::repositories::admin_accounts::{self, AdminAccount};

/// Отказ создания учётной записи.
#[derive(Debug)]
pub enum SetupError {
    /// Учётная запись уже существует: второй строки быть не может.
    AlreadyConfigured,
    /// Код недействителен; причина клиенту не раскрывается.
    InvalidCode,
    /// Ошибка базы.
    Database(DomainError),
}

/// Погасить setup-код и создать учётную запись одной транзакцией.
pub async fn create_account(
    state: &AdminState,
    code: &str,
    username: &str,
    password_hash: &str,
    now: DateTime<Utc>,
) -> Result<AdminAccount, SetupError> {
    let mut tx = state.pool.begin().await.map_err(database_error)?;

    match codes::use_code(&mut tx, Purpose::Setup, code, now).await {
        Ok(_) => {}
        Err(error) => {
            let _ = tx.rollback().await;
            return Err(setup_error(error));
        }
    }

    let account = match admin_accounts::create(&mut tx, username, password_hash, now).await {
        Ok(account) => account,
        // Откат возвращает код в неиспользованное состояние: нарушение
        // уникальности учётки не должно его сжигать.
        Err(error) => {
            let mapped = setup_error(error);
            let _ = tx.rollback().await;
            return Err(mapped);
        }
    };

    tx.commit().await.map_err(database_error)?;
    Ok(account)
}

fn setup_error(error: DomainError) -> SetupError {
    match error {
        DomainError::NotFound => SetupError::InvalidCode,
        DomainError::Database(sqlx::Error::Database(database))
            if database.is_unique_violation() =>
        {
            SetupError::AlreadyConfigured
        }
        other => SetupError::Database(other),
    }
}

fn database_error(error: sqlx::Error) -> SetupError {
    SetupError::Database(DomainError::Database(error))
}
