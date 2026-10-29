//! Репозиторий API-токенов.

use chrono::{DateTime, Utc};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::domain::error::DomainError;

/// API-токен без хеша.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ApiToken {
    /// Идентификатор.
    pub id: Uuid,
    /// Household.
    pub household_id: Uuid,
    /// Название.
    pub name: String,
    /// Видимый префикс.
    pub prefix: String,
    /// Scopes.
    pub scopes: Vec<String>,
    /// Срок действия.
    pub expires_at: Option<DateTime<Utc>>,
    /// Время отзыва.
    pub revoked_at: Option<DateTime<Utc>>,
    /// Последнее использование.
    pub last_used_at: Option<DateTime<Utc>>,
    /// Время создания.
    pub created_at: DateTime<Utc>,
}

/// Создать токен по готовому хешу.
#[allow(clippy::too_many_arguments)]
pub async fn create(
    conn: &mut PgConnection,
    household_id: Uuid,
    name: &str,
    token_hash: &str,
    prefix: &str,
    scopes: &[String],
    expires_at: Option<DateTime<Utc>>,
    created_by_user_id: Option<Uuid>,
    now: DateTime<Utc>,
) -> Result<ApiToken, DomainError> {
    let token = sqlx::query_as::<_, ApiToken>(
        "INSERT INTO api_tokens (
             id, household_id, name, token_hash, prefix, scopes, created_by_user_id,
             expires_at, created_at
         )
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
         RETURNING id, household_id, name, prefix, scopes, expires_at, revoked_at,
                   last_used_at, created_at",
    )
    .bind(Uuid::now_v7())
    .bind(household_id)
    .bind(name)
    .bind(token_hash)
    .bind(prefix)
    .bind(scopes)
    .bind(created_by_user_id)
    .bind(expires_at)
    .bind(now)
    .fetch_one(conn)
    .await?;

    Ok(token)
}

/// Найти действующий токен по хешу.
pub async fn find_valid(
    conn: &mut PgConnection,
    token_hash: &str,
    now: DateTime<Utc>,
) -> Result<Option<ApiToken>, DomainError> {
    let token = sqlx::query_as::<_, ApiToken>(
        "SELECT id, household_id, name, prefix, scopes, expires_at, revoked_at,
                last_used_at, created_at
         FROM api_tokens
         WHERE token_hash = $1
           AND revoked_at IS NULL
           AND (expires_at IS NULL OR expires_at > $2)",
    )
    .bind(token_hash)
    .bind(now)
    .fetch_optional(conn)
    .await?;

    Ok(token)
}

/// Обновить `last_used_at`.
pub async fn touch(
    conn: &mut PgConnection,
    token_id: Uuid,
    now: DateTime<Utc>,
) -> Result<(), DomainError> {
    sqlx::query("UPDATE api_tokens SET last_used_at = $2 WHERE id = $1")
        .bind(token_id)
        .bind(now)
        .execute(conn)
        .await?;

    Ok(())
}

const COLUMNS: &str = "id, household_id, name, prefix, scopes, expires_at, revoked_at, \
    last_used_at, created_at";

/// Токен по идентификатору.
pub async fn find_by_id(
    conn: &mut PgConnection,
    token_id: Uuid,
) -> Result<Option<ApiToken>, DomainError> {
    let token =
        sqlx::query_as::<_, ApiToken>(&format!("SELECT {COLUMNS} FROM api_tokens WHERE id = $1"))
            .bind(token_id)
            .fetch_optional(conn)
            .await?;

    Ok(token)
}

/// Токены household, новые первыми. Хеши не возвращаются.
pub async fn list(
    conn: &mut PgConnection,
    household_id: Uuid,
) -> Result<Vec<ApiToken>, DomainError> {
    let tokens = sqlx::query_as::<_, ApiToken>(&format!(
        "SELECT {COLUMNS} FROM api_tokens
         WHERE household_id = $1
         ORDER BY created_at DESC, id DESC"
    ))
    .bind(household_id)
    .fetch_all(conn)
    .await?;

    Ok(tokens)
}

/// Отозвать токен. Возвращает запись, если токен был отозван сейчас, и `None`,
/// если он уже отозван или не существует.
pub async fn revoke(
    conn: &mut PgConnection,
    token_id: Uuid,
    now: DateTime<Utc>,
) -> Result<Option<ApiToken>, DomainError> {
    let token = sqlx::query_as::<_, ApiToken>(&format!(
        "UPDATE api_tokens SET revoked_at = $2
         WHERE id = $1 AND revoked_at IS NULL
         RETURNING {COLUMNS}"
    ))
    .bind(token_id)
    .bind(now)
    .fetch_optional(conn)
    .await?;

    Ok(token)
}

/// Отозвать все токены household.
pub(crate) async fn revoke_all(
    conn: &mut PgConnection,
    household_id: Uuid,
    now: DateTime<Utc>,
) -> Result<u64, DomainError> {
    let result = sqlx::query(
        "UPDATE api_tokens SET revoked_at = $2
         WHERE household_id = $1 AND revoked_at IS NULL",
    )
    .bind(household_id)
    .bind(now)
    .execute(conn)
    .await?;

    Ok(result.rows_affected())
}

/// Отозвать токены, выпущенные пользователем.
pub(crate) async fn revoke_created_by(
    conn: &mut PgConnection,
    household_id: Uuid,
    user_id: Uuid,
    now: DateTime<Utc>,
) -> Result<u64, DomainError> {
    let result = sqlx::query(
        "UPDATE api_tokens SET revoked_at = $3
         WHERE household_id = $1 AND created_by_user_id = $2 AND revoked_at IS NULL",
    )
    .bind(household_id)
    .bind(user_id)
    .bind(now)
    .execute(conn)
    .await?;

    Ok(result.rows_affected())
}
