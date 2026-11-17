//! Восстановление доступа по одноразовой ссылке.
//!
//! `GET` только читает код (`find_active`): предпросмотр и повторное открытие
//! не должны его сжигать. `POST` с новым паролем гасит код, меняет хеш и
//! удаляет все сессии одной транзакцией. Лимит неудач восстановления общий для
//! `GET` и `POST`: недействительная ссылка — неудача, действительная лимит не
//! расходует. Проверка идёт в отдельной задаче, владеющей слотом, поэтому
//! отмена обработчика не теряет неудачу.

use chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::admin_console::attempt::{self, Attempt};
use crate::admin_console::codes::{self, Purpose};
use crate::admin_console::password::{self, Password, PasswordError};
use crate::admin_console::router::AdminState;
use crate::domain::error::DomainError;
use crate::http::client::Client;
use crate::rate_limit::{RateDecision, ip_key};
use crate::security_audit;
use crate::storage::repositories::admin_accounts;
use crate::storage::repositories::admin_one_time_codes;
use crate::storage::repositories::admin_sessions;

/// Ошибка восстановления доступа.
#[derive(Debug)]
pub enum RecoveryError {
    /// Ошибка базы.
    Database(DomainError),
    /// Сломался argon2.
    Password(PasswordError),
    /// Задача проверки упала.
    Task(String),
}

/// Итог открытия ссылки восстановления.
#[derive(Debug)]
pub enum RecoveryCheck {
    /// Код действителен: форму можно показывать.
    Valid,
    /// Код неизвестен, погашен или истёк.
    Invalid,
    /// Лимит неудач исчерпан: код не проверялся.
    RateLimited(RateDecision),
}

/// Итог сохранения нового пароля по ссылке.
#[derive(Debug)]
pub enum RecoveryOutcome {
    /// Пароль сменён, все сессии удалены.
    Changed,
    /// Код неизвестен, погашен или истёк; пароль не менялся.
    Invalid,
    /// Лимит неудач исчерпан: код не проверялся.
    RateLimited(RateDecision),
}

/// Проверить ссылку без её расхода.
pub async fn check(
    state: &AdminState,
    client: &Client,
    code: &str,
    now: DateTime<Utc>,
) -> Result<RecoveryCheck, RecoveryError> {
    let key = ip_key(client.ip);
    let pool = state.pool.clone();
    let code = code.to_owned();
    let outcome = attempt::run(
        state.recovery_limiter(),
        key,
        now,
        move || async move {
            let mut conn = pool
                .acquire()
                .await
                .map_err(|error| RecoveryError::Database(DomainError::Database(error)))?;
            let active = admin_one_time_codes::find_active(
                &mut conn,
                Purpose::Recovery.as_str(),
                &code,
                now,
            )
            .await
            .map_err(RecoveryError::Database)?
            .is_some();
            Ok((active, !active))
        },
        |error| RecoveryError::Task(error.to_string()),
    )
    .await;

    match outcome {
        Attempt::Denied(decision) => {
            audit_rate_limited(state, client, decision, now).await;
            Ok(RecoveryCheck::RateLimited(decision))
        }
        Attempt::Error(error) => Err(error),
        Attempt::Checked { value: true, .. } => Ok(RecoveryCheck::Valid),
        Attempt::Checked { value: false, .. } => Ok(RecoveryCheck::Invalid),
    }
}

/// Погасить ссылку, сменить пароль и удалить все сессии.
///
/// Длину пароля проверяет обработчик до вызова: ошибка длины не расходует код
/// и не занимает лимит.
pub async fn complete(
    state: &AdminState,
    client: &Client,
    code: &str,
    password: Password,
    now: DateTime<Utc>,
) -> Result<RecoveryOutcome, RecoveryError> {
    let key = ip_key(client.ip);
    let pool = state.pool.clone();
    let code = code.to_owned();
    let audit = state.audit.clone();
    let ip = client.ip;
    let outcome = attempt::run_with_after(
        state.recovery_limiter(),
        key,
        now,
        move || async move {
            let outcome = recovery_transaction(&pool, &code, password, now).await?;
            let failed = matches!(outcome, RecoveryOutcome::Invalid);
            Ok((outcome, failed))
        },
        move |value: RecoveryOutcome, _failed| async move {
            if matches!(&value, RecoveryOutcome::Changed) {
                audit.admin_password_changed(ip, "recovery", now).await;
            }
            value
        },
        |error| RecoveryError::Task(error.to_string()),
    )
    .await;

    match outcome {
        Attempt::Denied(decision) => {
            audit_rate_limited(state, client, decision, now).await;
            Ok(RecoveryOutcome::RateLimited(decision))
        }
        Attempt::Error(error) => Err(error),
        Attempt::Checked { value, failed } => {
            if failed {
                tracing::info!(
                    ip = %client.ip,
                    "admin recovery link rejected: invalid or expired"
                );
            }
            Ok(value)
        }
    }
}

async fn recovery_transaction(
    pool: &PgPool,
    code: &str,
    password: Password,
    now: DateTime<Utc>,
) -> Result<RecoveryOutcome, RecoveryError> {
    // Хеш считается до транзакции: argon2 не должен держать блокировку кода.
    let password_hash = password::hash(password)
        .await
        .map_err(RecoveryError::Password)?;

    let mut tx = pool.begin().await.map_err(database_error)?;

    match codes::use_code(&mut tx, Purpose::Recovery, code, now).await {
        Ok(_) => {}
        Err(DomainError::NotFound) => {
            let _ = tx.rollback().await;
            return Ok(RecoveryOutcome::Invalid);
        }
        Err(error) => {
            let _ = tx.rollback().await;
            return Err(RecoveryError::Database(error));
        }
    }

    match admin_accounts::update_password(&mut tx, &password_hash, now).await {
        Ok(_) => {}
        // Учётки нет — ссылка выпускалась при ней, но проверить нечего:
        // откат оставляет код действующим, клиент видит ту же ошибку.
        Err(DomainError::NotFound) => {
            let _ = tx.rollback().await;
            return Ok(RecoveryOutcome::Invalid);
        }
        Err(error) => {
            let _ = tx.rollback().await;
            return Err(RecoveryError::Database(error));
        }
    }

    if let Err(error) = admin_sessions::delete_all_except(&mut tx, None).await {
        let _ = tx.rollback().await;
        return Err(RecoveryError::Database(error));
    }

    tx.commit().await.map_err(database_error)?;
    Ok(RecoveryOutcome::Changed)
}

async fn audit_rate_limited(
    state: &AdminState,
    client: &Client,
    decision: RateDecision,
    now: DateTime<Utc>,
) {
    state
        .audit_rate_limited(
            security_audit::ADMIN_RECOVERY_LIMIT,
            client.ip,
            decision,
            now,
        )
        .await;
}

fn database_error(error: sqlx::Error) -> RecoveryError {
    RecoveryError::Database(DomainError::Database(error))
}
