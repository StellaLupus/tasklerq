//! Пароль администратора: длина и argon2id.
//!
//! Параметры — m=19 MiB, t=2, p=1 (design, решение 2). Соль случайная, в базе
//! лежит PHC-строка с параметрами, поэтому смена параметров не требует
//! миграции: проверка берёт их из строки.
//!
//! Длина считается в символах (`chars().count()`), а не в байтах: кириллица в
//! UTF-8 занимает два байта, и порог в байтах отклонил бы пароль, который
//! человек считает достаточно длинным. Верхняя граница не даёт отправить
//! мегабайт в argon2.
//!
//! Хеширование и проверка выполняются в [`tokio::task::spawn_blocking`]: argon2
//! намеренно медленный и не должен занимать поток runtime. Пароль не пишется в
//! журнал и не входит в ответы; [`Password`] печатается как `<redacted>`.

use std::fmt;

use argon2::{Algorithm, Argon2, Params, PasswordHash, PasswordHasher, PasswordVerifier, Version};

use crate::redaction::REDACTED;

/// Минимальная длина пароля в символах.
pub const MIN_CHARS: usize = 12;
/// Максимальная длина пароля в символах.
pub const MAX_CHARS: usize = 1024;

/// Память argon2id, КиБ: 19 MiB.
const MEMORY_KIB: u32 = 19 * 1024;
/// Число проходов argon2id.
const TIME_COST: u32 = 2;
/// Степень параллелизма argon2id.
const PARALLELISM: u32 = 1;
/// Длина случайной соли.
const SALT_BYTES: usize = 16;

/// Пароль из формы. `Debug` не печатает значение.
#[derive(Clone, serde::Deserialize)]
#[serde(transparent)]
pub struct Password(String);

impl Password {
    /// Обернуть строку формы.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Значение для хеширования и проверки.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Password {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("Password").field(&REDACTED).finish()
    }
}

/// Нарушение длины пароля. Сообщение называет границу.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PasswordLengthError {
    /// Слишком короткий пароль.
    #[error("пароль должен содержать не менее {MIN_CHARS} символов")]
    TooShort,
    /// Слишком длинный пароль.
    #[error("пароль должен содержать не более {MAX_CHARS} символов")]
    TooLong,
}

/// Проверить длину пароля до хеширования.
pub fn validate(password: &str) -> Result<(), PasswordLengthError> {
    match password.chars().count() {
        length if length < MIN_CHARS => Err(PasswordLengthError::TooShort),
        length if length > MAX_CHARS => Err(PasswordLengthError::TooLong),
        _ => Ok(()),
    }
}

/// Сбой хеширования или проверки: подробности не раскрываются.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("password hashing failed")]
pub struct PasswordError;

/// Захешировать пароль в PHC-строку argon2id.
///
/// Длину проверяет вызывающая сторона ([`validate`]) до вызова: здесь нет
/// второй проверки, чтобы форма могла показать сообщение без хеширования.
pub async fn hash(password: Password) -> Result<String, PasswordError> {
    tokio::task::spawn_blocking(move || {
        let mut salt = [0_u8; SALT_BYTES];
        getrandom::fill(&mut salt).map_err(|_| PasswordError)?;
        argon2()
            .hash_password_with_salt(password.expose().as_bytes(), &salt)
            .map(|hash| hash.to_string())
            .map_err(|_| PasswordError)
    })
    .await
    .map_err(|_| PasswordError)?
}

/// Проверить пароль против PHC-строки.
///
/// `Ok(false)` — пароль не подходит; `Err` — хеш в базе повреждён или сломался
/// сам argon2, это ошибка сервера, а не отказ во входе.
pub async fn verify(password: Password, password_hash: String) -> Result<bool, PasswordError> {
    tokio::task::spawn_blocking(move || {
        let hash = PasswordHash::new(&password_hash).map_err(|_| PasswordError)?;
        match argon2().verify_password(password.expose().as_bytes(), &hash) {
            Ok(()) => Ok(true),
            Err(argon2::password_hash::Error::PasswordInvalid) => Ok(false),
            Err(_) => Err(PasswordError),
        }
    })
    .await
    .map_err(|_| PasswordError)?
}

/// argon2id с боевыми параметрами.
fn argon2() -> Argon2<'static> {
    let params = Params::new(MEMORY_KIB, TIME_COST, PARALLELISM, None)
        .expect("argon2id parameters are valid constants");
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
}

#[cfg(test)]
mod tests {
    use super::{Password, PasswordLengthError, validate};

    #[test]
    fn the_length_is_counted_in_characters() {
        assert!(validate("123456789012").is_ok());
        assert_eq!(validate("12345678901"), Err(PasswordLengthError::TooShort));
        // 11 кириллических букв — 22 байта, но всё ещё 11 символов.
        assert_eq!(validate("парольпарол"), Err(PasswordLengthError::TooShort));
        assert!(validate("парольпароль").is_ok());
    }

    #[test]
    fn the_bounds_name_the_limit() {
        let short = PasswordLengthError::TooShort.to_string();
        assert!(short.contains("12"), "{short}");
        let long = PasswordLengthError::TooLong.to_string();
        assert!(long.contains("1024"), "{long}");
        assert!(
            validate(&"a".repeat(1024)).is_ok(),
            "1024 characters are allowed"
        );
        assert_eq!(
            validate(&"a".repeat(1025)),
            Err(PasswordLengthError::TooLong)
        );
    }

    #[test]
    fn debug_does_not_print_the_value() {
        let rendered = format!("{:?}", Password::new("s3cret-password-value"));
        assert!(!rendered.contains("s3cret-password-value"), "{rendered}");
        assert!(rendered.contains("<redacted>"), "{rendered}");
    }
}
