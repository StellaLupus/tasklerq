//! Создание учётной записи администратора по setup-коду.
//!
//! Одна функция на весь сценарий [`create_account`]: ограничение частоты,
//! argon2, гашение кода, создание учётки и события аудита живут здесь, поэтому
//! обработчик страницы только различает итог — так же, как
//! [`crate::admin_console::login::authenticate`] у входа.
//!
//! Код гасится и учётка создаётся в одной транзакции; любая ошибка откатывает
//! транзакцию и возвращает код в неиспользованное состояние. Транзакция берёт
//! `pg_advisory_xact_lock` назначения `setup`: параллельный выпуск setup-кода
//! (`tasklerq admin setup-code` или старт сервиса) ждёт её и после коммита
//! видит учётку, поэтому код не выпускается.
//!
//! Ошибка кода не различает причину: повторный, погашенный, чужой по назначению
//! и неизвестный код дают [`SetupOutcome::InvalidCode`]. Если учётка уже есть (в
//! том числе появилась параллельно), [`SetupOutcome::AlreadyConfigured`], а код
//! остаётся действующим.

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
use crate::storage::repositories::admin_accounts::{self, AdminAccount};

/// Отказ создания учётной записи.
#[derive(Debug)]
pub enum SetupError {
    /// Ошибка базы.
    Database(DomainError),
    /// Сломался argon2.
    Password(PasswordError),
    /// Задача проверки упала.
    Task(String),
}

/// Итог попытки настройки.
#[derive(Debug)]
pub enum SetupOutcome {
    /// Учётная запись создана; сессию выдаёт обработчик.
    Created(AdminAccount),
    /// Учётная запись уже существует: второй строки быть не может.
    AlreadyConfigured,
    /// Код недействителен; причина клиенту не раскрывается.
    InvalidCode,
    /// Лимит неудач исчерпан: код и пароль не проверялись.
    RateLimited(RateDecision),
}

/// Итог транзакции настройки без различения ошибки клиентом.
enum AccountCheck {
    Created(AdminAccount),
    AlreadyConfigured,
    InvalidCode,
}

/// Создать учётную запись по setup-коду с лимитом частоты и аудитом.
///
/// Слот лимита резервируется до argon2 и до кода. Проверка и аудит идут в
/// отдельной задаче, владеющей слотом: отмена HTTP-обработчика не освобождает
/// слот, не теряет неудачу и не теряет событие создания учётки.
pub async fn create_account(
    state: &AdminState,
    client: &Client,
    code: &str,
    username: &str,
    password: Password,
    now: DateTime<Utc>,
) -> Result<SetupOutcome, SetupError> {
    let key = ip_key(client.ip);
    let pool = state.pool.clone();
    let code = code.to_owned();
    let username = username.to_owned();
    let audit = state.audit.clone();
    let ip = client.ip;
    let outcome = attempt::run_with_after(
        state.setup_limiter(),
        key,
        now,
        move || async move {
            match setup_transaction(&pool, &code, &username, password, now).await? {
                AccountCheck::Created(account) => Ok((AccountCheck::Created(account), false)),
                AccountCheck::AlreadyConfigured => Ok((AccountCheck::AlreadyConfigured, false)),
                AccountCheck::InvalidCode => Ok((AccountCheck::InvalidCode, true)),
            }
        },
        move |value: AccountCheck, _failed| async move {
            match &value {
                AccountCheck::Created(account) => {
                    audit
                        .admin_account_created(ip, &account.username, now)
                        .await;
                }
                AccountCheck::InvalidCode => {
                    audit.admin_setup_code_failed(ip, "invalid_code", now).await;
                }
                AccountCheck::AlreadyConfigured => {}
            }
            value
        },
        |error| SetupError::Task(error.to_string()),
    )
    .await;

    match outcome {
        Attempt::Denied(decision) => {
            state
                .audit_rate_limited(
                    security_audit::ADMIN_SETUP_CODE_LIMIT,
                    client.ip,
                    decision,
                    now,
                )
                .await;
            Ok(SetupOutcome::RateLimited(decision))
        }
        Attempt::Error(error) => Err(error),
        // События аудита уже записаны задачей проверки.
        Attempt::Checked {
            value: AccountCheck::Created(account),
            ..
        } => Ok(SetupOutcome::Created(account)),
        Attempt::Checked {
            value: AccountCheck::InvalidCode,
            ..
        } => Ok(SetupOutcome::InvalidCode),
        Attempt::Checked {
            value: AccountCheck::AlreadyConfigured,
            ..
        } => Ok(SetupOutcome::AlreadyConfigured),
    }
}

/// Погасить setup-код и создать учётную запись одной транзакцией.
async fn setup_transaction(
    pool: &PgPool,
    code: &str,
    username: &str,
    password: Password,
    now: DateTime<Utc>,
) -> Result<AccountCheck, SetupError> {
    // Хеш считается до транзакции, чтобы не держать блокировку кода и
    // назначения на время argon2.
    let password_hash = password::hash(password)
        .await
        .map_err(SetupError::Password)?;

    let mut tx = pool.begin().await.map_err(database_error)?;

    // Та же блокировка, что у выпуска setup-кода: выпуск, начатый до коммита,
    // после него увидит учётку и кода не создаст.
    if let Err(error) = codes::lock_purpose(&mut tx, Purpose::Setup, None).await {
        let _ = tx.rollback().await;
        return Err(SetupError::Database(error));
    }

    match codes::use_code(&mut tx, Purpose::Setup, code, now).await {
        Ok(_) => {}
        // Откат возвращает код в неиспользованное состояние: неверный код не
        // должен сжигаться.
        Err(DomainError::NotFound) => {
            let _ = tx.rollback().await;
            return Ok(AccountCheck::InvalidCode);
        }
        Err(error) => {
            let _ = tx.rollback().await;
            return Err(SetupError::Database(error));
        }
    }

    let account = match admin_accounts::create(&mut tx, username, &password_hash, now).await {
        Ok(account) => account,
        // Откат возвращает код в неиспользованное состояние: нарушение
        // уникальности учётки не должно его сжигать.
        Err(DomainError::Database(sqlx::Error::Database(database)))
            if database.is_unique_violation() =>
        {
            let _ = tx.rollback().await;
            return Ok(AccountCheck::AlreadyConfigured);
        }
        Err(error) => {
            let _ = tx.rollback().await;
            return Err(SetupError::Database(error));
        }
    };

    tx.commit().await.map_err(database_error)?;
    Ok(AccountCheck::Created(account))
}

fn database_error(error: sqlx::Error) -> SetupError {
    SetupError::Database(DomainError::Database(error))
}
