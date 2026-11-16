//! Смена пароля администратора после ввода текущего.
//!
//! Текущий пароль проверяется argon2; неверный ввод — неудача в бакете
//! текущего пароля, после пяти неудач шестая попытка не доходит до хеширования
//! ([`crate::admin_console::attempt`]). Верный ввод меняет хеш и
//! `password_changed_at` и удаляет все сессии, кроме текущей, одной
//! транзакцией: администратор остаётся в консоли на этом устройстве, а другие
//! устройства теряют доступ.

use chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::admin_console::attempt::{self, Attempt};
use crate::admin_console::password::{self, Password, PasswordError};
use crate::admin_console::router::AdminState;
use crate::admin_console::session::AdminSession;
use crate::domain::error::DomainError;
use crate::http::client::Client;
use crate::rate_limit::{RateDecision, ip_key};
use crate::security_audit;
use crate::storage::repositories::{admin_accounts, admin_sessions};

/// Ошибка смены пароля.
#[derive(Debug)]
pub enum ChangeError {
    /// Ошибка базы.
    Database(DomainError),
    /// Сломался argon2 или повреждён хеш в базе.
    Password(PasswordError),
    /// Задача проверки упала.
    Task(String),
}

/// Итог попытки смены пароля.
#[derive(Debug)]
pub enum ChangeOutcome {
    /// Пароль изменён; прочие сессии удалены.
    Changed,
    /// Текущий пароль не подошёл; пароль не изменён.
    WrongPassword,
    /// Лимит неудач исчерпан: текущий пароль не проверялся.
    RateLimited(RateDecision),
}

/// Сменить пароль, проверив текущий, с лимитом частоты и аудитом.
///
/// Длину нового пароля проверяет обработчик до вызова: ошибка длины не
/// занимает лимит и не проверяет текущий пароль.
pub async fn change(
    state: &AdminState,
    session: AdminSession,
    client: &Client,
    current: Password,
    new_password: Password,
    now: DateTime<Utc>,
) -> Result<ChangeOutcome, ChangeError> {
    let key = ip_key(client.ip);
    let pool = state.pool.clone();
    let audit = state.audit.clone();
    let ip = client.ip;
    let outcome = attempt::run_with_after(
        state.current_password_limiter(),
        key,
        now,
        move || async move {
            let outcome = change_transaction(&pool, session.id, current, new_password, now).await?;
            let failed = matches!(outcome, ChangeOutcome::WrongPassword);
            Ok((outcome, failed))
        },
        move |value: ChangeOutcome, _failed| async move {
            if matches!(&value, ChangeOutcome::Changed) {
                audit.admin_password_changed(ip, "change", now).await;
            }
            value
        },
        |error| ChangeError::Task(error.to_string()),
    )
    .await;

    match outcome {
        Attempt::Denied(decision) => {
            state
                .audit_rate_limited(
                    security_audit::ADMIN_CURRENT_PASSWORD_LIMIT,
                    client.ip,
                    decision,
                    now,
                )
                .await;
            Ok(ChangeOutcome::RateLimited(decision))
        }
        Attempt::Error(error) => Err(error),
        // Аудит уже записан задачей проверки.
        Attempt::Checked { value, .. } => Ok(value),
    }
}

async fn change_transaction(
    pool: &PgPool,
    session_id: uuid::Uuid,
    current: Password,
    new_password: Password,
    now: DateTime<Utc>,
) -> Result<ChangeOutcome, ChangeError> {
    // Учётку читаем отдельным соединением и отпускаем до argon2: проверка
    // пароля не должна держать соединение пула, общий с публичным API.
    let account = {
        let mut conn = pool
            .acquire()
            .await
            .map_err(|error| ChangeError::Database(DomainError::Database(error)))?;
        admin_accounts::get(&mut conn)
            .await
            .map_err(ChangeError::Database)?
    };
    let Some(account) = account else {
        return Err(ChangeError::Database(DomainError::NotFound));
    };

    // Хеш, против которого проверен текущий пароль. Запись ниже идёт только
    // если он всё ещё действует: параллельное восстановление не должно быть
    // затёрто сменой, проверенной по прежнему паролю.
    let current_hash = account.password_hash.clone();
    let matches = password::verify(current, account.password_hash)
        .await
        .map_err(ChangeError::Password)?;
    if !matches {
        return Ok(ChangeOutcome::WrongPassword);
    }

    let password_hash = password::hash(new_password)
        .await
        .map_err(ChangeError::Password)?;

    let mut tx = pool.begin().await.map_err(database_error)?;
    match admin_accounts::update_password_if_unchanged(&mut tx, &current_hash, &password_hash, now)
        .await
    {
        Ok(Some(_)) => {}
        // Хеш сменился между проверкой и записью: текущий пароль больше не
        // действует, отвечаем как на неверный.
        Ok(None) => {
            let _ = tx.rollback().await;
            return Ok(ChangeOutcome::WrongPassword);
        }
        Err(error) => {
            let _ = tx.rollback().await;
            return Err(ChangeError::Database(error));
        }
    }
    if let Err(error) = admin_sessions::delete_all_except(&mut tx, Some(session_id)).await {
        let _ = tx.rollback().await;
        return Err(ChangeError::Database(error));
    }
    tx.commit().await.map_err(database_error)?;

    Ok(ChangeOutcome::Changed)
}

fn database_error(error: sqlx::Error) -> ChangeError {
    ChangeError::Database(DomainError::Database(error))
}
