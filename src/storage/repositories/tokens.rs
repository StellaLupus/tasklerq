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
