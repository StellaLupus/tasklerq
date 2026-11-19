//! Учётная запись администратора консоли.
//!
//! Строка ровно одна: уникальный индекс по константному выражению
//! (`migrations/0021_admin_console.sql`). Чтение возвращает эту строку или
//! `None`, пока setup-код не создал учётку; создание выполняется в транзакции
//! вызывающего вместе с гашением setup-кода, поэтому откат возвращает код в
//! неиспользованное состояние.
//!
//! Хеш пароля — PHC-строка argon2id; в журнал и ответы он не попадает, `Debug`
//! показывает `<redacted>`.

use std::fmt;

use chrono::{DateTime, Utc};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::redaction::REDACTED;

const COLUMNS: &str = "id, username, password_hash, created_at, password_changed_at";

/// Учётная запись администратора.
#[derive(Clone, sqlx::FromRow)]
pub struct AdminAccount {
    /// Идентификатор.
    pub id: Uuid,
    /// Имя для входа.
    pub username: String,
    /// PHC-строка argon2id.
    pub password_hash: String,
    /// Время создания.
    pub created_at: DateTime<Utc>,
    /// Время последней смены пароля.
    pub password_changed_at: DateTime<Utc>,
}

impl fmt::Debug for AdminAccount {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AdminAccount")
            .field("id", &self.id)
            .field("username", &self.username)
            .field("password_hash", &REDACTED)
            .field("created_at", &self.created_at)
            .field("password_changed_at", &self.password_changed_at)
            .finish()
    }
}

/// Прочитать единственную учётную запись; `None`, пока её нет.
pub async fn get(conn: &mut PgConnection) -> Result<Option<AdminAccount>, DomainError> {
    let account =
        sqlx::query_as::<_, AdminAccount>(&format!("SELECT {COLUMNS} FROM admin_accounts LIMIT 1"))
            .fetch_optional(conn)
            .await?;

    Ok(account)
}

/// Есть ли учётная запись администратора.
///
/// Дешевле [`get`]: не читает хеш пароля и остальные поля, когда нужен только
/// факт существования (первый контакт).
pub async fn exists(conn: &mut PgConnection) -> Result<bool, DomainError> {
    let exists = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM admin_accounts)")
        .fetch_one(conn)
        .await?;

    Ok(exists)
}

/// Создать учётную запись по готовому хешу пароля.
///
/// Вызывать внутри транзакции вместе с [`crate::admin_console::codes::use_code`]:
/// второй `INSERT` получает нарушение уникальности синглтона, и вызывающая
/// сторона по нему понимает, что учётка уже создана параллельно.
pub async fn create(
    conn: &mut PgConnection,
    username: &str,
    password_hash: &str,
    now: DateTime<Utc>,
) -> Result<AdminAccount, DomainError> {
    let account = sqlx::query_as::<_, AdminAccount>(&format!(
        "INSERT INTO admin_accounts
             (id, username, password_hash, created_at, password_changed_at)
         VALUES ($1, $2, $3, $4, $4)
         RETURNING {COLUMNS}"
    ))
    .bind(Uuid::now_v7())
    .bind(username)
    .bind(password_hash)
    .bind(now)
    .fetch_one(conn)
    .await?;

    Ok(account)
}

/// Заменить хеш пароля и время смены.
///
/// Вызывать внутри транзакции вместе с удалением сессий: смена пароля и отзыв
/// сессий должны стать видимыми вместе. `NotFound` — учётной записи нет.
pub async fn update_password(
    conn: &mut PgConnection,
    password_hash: &str,
    now: DateTime<Utc>,
) -> Result<AdminAccount, DomainError> {
    let account = sqlx::query_as::<_, AdminAccount>(&format!(
        "UPDATE admin_accounts
            SET password_hash = $1, password_changed_at = $2
          RETURNING {COLUMNS}"
    ))
    .bind(password_hash)
    .bind(now)
    .fetch_optional(conn)
    .await?
    .ok_or(DomainError::NotFound)?;

    Ok(account)
}

/// Заменить хеш пароля и время смены, если действующий хеш не изменился.
///
/// Смена пароля проверяет текущий пароль по прочитанному хешу; между проверкой
/// и записью параллельное восстановление могло сменить пароль. Условие
/// `password_hash = $3` не даёт затёреть более новый пароль. `None` — хеш уже
/// другой (или учётной записи нет); транзакция вызывающего откатывается.
pub async fn update_password_if_unchanged(
    conn: &mut PgConnection,
    expected_hash: &str,
    password_hash: &str,
    now: DateTime<Utc>,
) -> Result<Option<AdminAccount>, DomainError> {
    let account = sqlx::query_as::<_, AdminAccount>(&format!(
        "UPDATE admin_accounts
            SET password_hash = $1, password_changed_at = $2
          WHERE password_hash = $3
          RETURNING {COLUMNS}"
    ))
    .bind(password_hash)
    .bind(now)
    .bind(expected_hash)
    .fetch_optional(conn)
    .await?;

    Ok(account)
}
