//! Проверка имени и пароля администратора.
//!
//! Одна функция на весь вход: сюда 5.6 добавит ограничение частоты и события
//! аудита. Сейчас она всегда прогоняет argon2, даже если имя не совпало, чтобы
//! время ответа не выдавало существующее имя. При отсутствии учётки
//! проверяется фиктивный хеш с теми же параметрами.

use crate::admin_console::password::{self, Password, PasswordError};
use crate::admin_console::router::AdminState;
use crate::domain::error::DomainError;
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
}

/// Проверить имя и пароль.
///
/// `Ok(Some(account))` — данные верны; `Ok(None)` — имя или пароль не подходят
/// (клиент не должен различать эти случаи).
pub async fn verify_credentials(
    state: &AdminState,
    username: &str,
    password: Password,
) -> Result<Option<AdminAccount>, LoginError> {
    // Соединение нужно только для чтения учётки. argon2 в `spawn_blocking`
    // считается медленно, поэтому отпускаем соединение в пул до проверки
    // пароля: иначе параллельные входы занимали бы весь пул, общий с публичным
    // API и capture.
    let account = {
        let mut conn = state
            .pool
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
