//! Проверка имени и пароля администратора.
//!
//! Одна функция на весь вход [`authenticate`]: ограничение частоты и события
//! аудита живут здесь, поэтому обработчик страницы только различает итог.
//! Проверка пароля всегда прогоняет argon2, даже если имя не совпало, чтобы
//! время ответа не выдавало существующее имя. При отсутствии учётки
//! проверяется фиктивный хеш с теми же параметрами.

use chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::admin_console::attempt::{self, Attempt};
use crate::admin_console::password::{self, Password, PasswordError};
use crate::admin_console::router::AdminState;
use crate::domain::error::DomainError;
use crate::http::client::Client;
use crate::rate_limit::{RateDecision, ip_key};
use crate::security_audit;
use crate::storage::repositories::admin_accounts::{self, AdminAccount};

/// Фиктивный хеш для проверки пароля, когда учётной записи нет.
///
/// Параметры совпадают с боевыми: проверка занимает то же время, что и с
/// настоящим хешем. Это PHC-строка пароля, который никто не вводит.
const FAKE_PASSWORD_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$CQkJCQkJCQkJCQkJCQkJCQ$\
    3CgpDU3jKGYNfNcg02A6OziMygGIcYAHAQaq/ZTB9GI";

/// Ошибка проверки входа: неверные данные — `Ok(None)`, а не ошибка.
#[derive(Debug)]
pub enum LoginError {
    /// Ошибка базы.
    Database(DomainError),
    /// Сломался argon2 или повреждён хеш в базе.
    Password(PasswordError),
    /// Задача проверки упала.
    Task(String),
}

/// Итог попытки входа.
#[derive(Debug)]
pub enum LoginOutcome {
    /// Данные верны; сессию выдаёт обработчик.
    Accepted(AdminAccount),
    /// Имя или пароль не подошли; неудача уже учтена в лимите и журнале.
    Rejected,
    /// Лимит неудач исчерпан: пароль не проверялся.
    RateLimited(RateDecision),
}

/// Проверить имя и пароль с ограничением частоты и аудитом.
///
/// Слот резервируется до argon2: после пятой неудачи шестая попытка не
/// расходует хеширование, а параллельные попытки видят уже занятые слоты.
/// Успех слот освобождает и лимит не расходует. Неверные данные дают
/// [`LoginOutcome::Rejected`], а не ошибку: клиент не должен различать
/// неверное имя и неверный пароль.
///
/// Проверка и аудит выполняются в отдельной задаче, владеющей слотом: отмена
/// обработчика не освобождает слот, не теряет неудачу и не теряет событие
/// входа.
pub async fn authenticate(
    state: &AdminState,
    client: &Client,
    username: &str,
    password: Password,
    now: DateTime<Utc>,
) -> Result<LoginOutcome, LoginError> {
    let key = ip_key(client.ip);
    let pool = state.pool.clone();
    let username = username.to_owned();
    let audit = state.audit.clone();
    let ip = client.ip;
    let outcome = attempt::run_with_after(
        state.login_limiter(),
        key,
        now,
        move || async move {
            match verify_credentials(&pool, &username, password).await? {
                Some(account) => Ok((Some(account), false)),
                None => Ok((None, true)),
            }
        },
        move |value: Option<AdminAccount>, failed| async move {
            if failed {
                audit.admin_login_failed(ip, "bad_credentials", now).await;
            } else {
                audit.admin_login_succeeded(ip, now).await;
            }
            value
        },
        |error| LoginError::Task(error.to_string()),
    )
    .await;

    match outcome {
        Attempt::Denied(decision) => {
            state
                .audit_rate_limited(security_audit::ADMIN_LOGIN_LIMIT, client.ip, decision, now)
                .await;
            Ok(LoginOutcome::RateLimited(decision))
        }
        Attempt::Error(error) => Err(error),
        Attempt::Checked {
            value: Some(account),
            ..
        } => Ok(LoginOutcome::Accepted(account)),
        // Неверное имя и неверный пароль дают один и тот же итог; аудит уже
        // записан задачей проверки.
        Attempt::Checked { value: None, .. } => Ok(LoginOutcome::Rejected),
    }
}

/// Проверить имя и пароль без лимита и аудита.
///
/// `Ok(Some(account))` — данные верны; `Ok(None)` — имя или пароль не подходят
/// (клиент не должен различать эти случаи).
async fn verify_credentials(
    pool: &PgPool,
    username: &str,
    password: Password,
) -> Result<Option<AdminAccount>, LoginError> {
    // Соединение нужно только для чтения учётки. argon2 в `spawn_blocking`
    // считается медленно, поэтому отпускаем соединение в пул до проверки
    // пароля: иначе параллельные входы занимали бы весь пул, общий с публичным
    // API и capture.
    let account = {
        let mut conn = pool
            .acquire()
            .await
            .map_err(|error| LoginError::Database(DomainError::Database(error)))?;
        admin_accounts::get(&mut conn)
            .await
            .map_err(LoginError::Database)?
    };

    let hash = account.as_ref().map_or_else(
        || FAKE_PASSWORD_HASH.to_owned(),
        |account| account.password_hash.clone(),
    );
    let password_matches = password::verify(password, hash)
        .await
        .map_err(LoginError::Password)?;

    match account {
        Some(account) if password_matches && account.username == username.trim() => {
            Ok(Some(account))
        }
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::FAKE_PASSWORD_HASH;
    use crate::admin_console::password::{Password, verify};

    #[tokio::test]
    async fn the_fake_hash_is_valid_and_never_matches() {
        assert_eq!(
            verify(
                Password::new("not-the-fake-password"),
                FAKE_PASSWORD_HASH.to_owned()
            )
            .await,
            Ok(false),
            "the fake hash must parse and the check must return a non-match"
        );
    }
}
