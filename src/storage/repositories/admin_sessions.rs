//! Сессии консоли администратора.
//!
//! В базе хранится только SHA-256 хеш токена (`auth::tokens::hash_token`), как у
//! API-токенов и одноразовых кодов. Сроки отдельными колонками не дублируются:
//! 7 дней без активности считаются от `last_seen_at`, 30 дней абсолютного
//! срока — от `created_at` (design, решение 4).
//!
//! `last_seen_at` обновляет [`touch`] одним `UPDATE … WHERE`: условие «не чаще
//! раза в минуту» проверяет PostgreSQL, отдельное чтение перед записью не
//! нужно. Поэтому CHECK `last_seen_at >= created_at` держится.

use chrono::{DateTime, Utc};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::domain::error::DomainError;

const COLUMNS: &str = "id, created_at, last_seen_at";

/// Сессия без хеша токена.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct AdminSessionRecord {
    /// Идентификатор.
    pub id: Uuid,
    /// Время входа.
    pub created_at: DateTime<Utc>,
    /// Время последней активности.
    pub last_seen_at: DateTime<Utc>,
}

/// Создать сессию по готовому хешу токена.
pub async fn create(
    conn: &mut PgConnection,
    token_hash: &str,
    now: DateTime<Utc>,
) -> Result<AdminSessionRecord, DomainError> {
    let session = sqlx::query_as::<_, AdminSessionRecord>(&format!(
        "INSERT INTO admin_sessions (id, token_hash, created_at, last_seen_at)
         VALUES ($1, $2, $3, $3)
         RETURNING {COLUMNS}"
    ))
    .bind(Uuid::now_v7())
    .bind(token_hash)
    .bind(now)
    .fetch_one(conn)
    .await?;

    Ok(session)
}

/// Найти сессию по хешу токена.
pub async fn find_by_hash(
    conn: &mut PgConnection,
    token_hash: &str,
) -> Result<Option<AdminSessionRecord>, DomainError> {
    let session = sqlx::query_as::<_, AdminSessionRecord>(&format!(
        "SELECT {COLUMNS} FROM admin_sessions WHERE token_hash = $1"
    ))
    .bind(token_hash)
    .fetch_optional(conn)
    .await?;

    Ok(session)
}

/// Обновить `last_seen_at`, если с прошлого обновления прошло не меньше минуты.
///
/// Возвращает `true`, если строка обновлена. Условие живёт в `WHERE`, поэтому
/// два одновременных запроса не перезаписывают время чаще разрешённого.
pub async fn touch(
    conn: &mut PgConnection,
    session_id: Uuid,
    now: DateTime<Utc>,
) -> Result<bool, DomainError> {
    let result = sqlx::query(
        "UPDATE admin_sessions SET last_seen_at = $2
         WHERE id = $1 AND last_seen_at <= $2 - interval '1 minute'",
    )
    .bind(session_id)
    .bind(now)
    .execute(conn)
    .await?;

    Ok(result.rows_affected() > 0)
}

/// Удалить сессию.
pub async fn delete(conn: &mut PgConnection, session_id: Uuid) -> Result<(), DomainError> {
    sqlx::query("DELETE FROM admin_sessions WHERE id = $1")
        .bind(session_id)
        .execute(conn)
        .await?;

    Ok(())
}

/// Удалить все сессии, кроме `keep`.
///
/// `None` удаляет все: восстановление пароля завершает доступ со всех
/// устройств. Смена пароля передаёт текущую сессию, чтобы администратор не
/// вылетел из консоли на том же устройстве.
pub async fn delete_all_except(
    conn: &mut PgConnection,
    keep: Option<Uuid>,
) -> Result<u64, DomainError> {
    let result = match keep {
        Some(session_id) => {
            sqlx::query("DELETE FROM admin_sessions WHERE id <> $1")
                .bind(session_id)
                .execute(conn)
                .await?
        }
        None => {
            sqlx::query("DELETE FROM admin_sessions")
                .execute(conn)
                .await?
        }
    };

    Ok(result.rows_affected())
}
