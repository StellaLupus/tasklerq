//! Репозиторий household и участников.

use chrono::{DateTime, Utc};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::models::{Household, List, ListKind, User, UserRole};
use crate::domain::recurrence::parse_timezone;
use crate::domain::validation::normalize_text;

/// Данные владельца при создании household.
#[derive(Debug, Clone)]
pub struct NewOwner {
    /// Telegram user id.
    pub telegram_user_id: i64,
    /// Telegram chat id.
    pub telegram_chat_id: i64,
    /// Отображаемое имя.
    pub display_name: String,
    /// Telegram username.
    pub username: Option<String>,
    /// Личный часовой пояс.
    pub timezone: Option<String>,
}

/// Создать household, владельца и два списка по умолчанию.
pub async fn create_with_owner(
    conn: &mut PgConnection,
    name: &str,
    timezone: &str,
    owner: NewOwner,
    now: DateTime<Utc>,
) -> Result<(Household, User), DomainError> {
    let name = normalize_text(name, 100, "household name")?;
    parse_timezone(timezone)?;

    let display_name = normalize_text(&owner.display_name, 64, "display name")?;
    if let Some(username) = owner.username.as_deref()
        && username.chars().count() > 64
    {
        return Err(DomainError::Validation(
            "username must be at most 64 characters".into(),
        ));
    }
    if let Some(user_timezone) = owner.timezone.as_deref() {
        parse_timezone(user_timezone)?;
    }

    let household_id = Uuid::now_v7();
    let household = sqlx::query_as::<_, Household>(
        "INSERT INTO households (id, name, timezone, created_at, updated_at)
         VALUES ($1, $2, $3, $4, $4)
         RETURNING id, name, timezone, created_at, updated_at, deleted_at",
    )
    .bind(household_id)
    .bind(&name)
    .bind(timezone)
    .bind(now)
    .fetch_one(&mut *conn)
    .await?;

    let user_id = Uuid::now_v7();
    let user = sqlx::query_as::<_, User>(
        "INSERT INTO users (
             id, household_id, telegram_user_id, telegram_chat_id, display_name,
             username, role, timezone, settings, created_at, updated_at
         )
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, '{}'::jsonb, $9, $9)
         RETURNING id, household_id, telegram_user_id, telegram_chat_id, display_name,
                   username, role, timezone, settings, created_at, updated_at, deleted_at",
    )
    .bind(user_id)
    .bind(household_id)
    .bind(owner.telegram_user_id)
    .bind(owner.telegram_chat_id)
    .bind(&display_name)
    .bind(owner.username)
    .bind(UserRole::Owner)
    .bind(owner.timezone)
    .bind(now)
    .fetch_one(&mut *conn)
    .await?;

    insert_default_list(conn, household_id, ListKind::Tasks, "Задачи", 0, now).await?;
    insert_default_list(conn, household_id, ListKind::Shopping, "Покупки", 1, now).await?;

    Ok((household, user))
}

async fn insert_default_list(
    conn: &mut PgConnection,
    household_id: Uuid,
    kind: ListKind,
    name: &str,
    position: i32,
    now: DateTime<Utc>,
) -> Result<List, DomainError> {
    let list = sqlx::query_as::<_, List>(
        "INSERT INTO lists (
             id, household_id, kind, name, is_default, is_archived, position,
             created_at, updated_at
         )
         VALUES ($1, $2, $3, $4, true, false, $5, $6, $6)
         RETURNING id, household_id, kind, name, is_default, is_archived, position,
                   created_at, updated_at, deleted_at",
    )
    .bind(Uuid::now_v7())
    .bind(household_id)
    .bind(kind)
    .bind(name)
    .bind(position)
    .bind(now)
    .fetch_one(conn)
    .await?;

    Ok(list)
}

/// Получить активный household.
pub async fn get(conn: &mut PgConnection, household_id: Uuid) -> Result<Household, DomainError> {
    sqlx::query_as::<_, Household>(
        "SELECT id, name, timezone, created_at, updated_at, deleted_at
         FROM households
         WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(household_id)
    .fetch_optional(conn)
    .await?
    .ok_or(DomainError::NotFound)
}

/// Активные участники household.
pub async fn members(
    conn: &mut PgConnection,
    household_id: Uuid,
) -> Result<Vec<User>, DomainError> {
    let members = sqlx::query_as::<_, User>(
        "SELECT id, household_id, telegram_user_id, telegram_chat_id, display_name,
                username, role, timezone, settings, created_at, updated_at, deleted_at
         FROM users
         WHERE household_id = $1 AND deleted_at IS NULL
         ORDER BY role, display_name, id",
    )
    .bind(household_id)
    .fetch_all(conn)
    .await?;

    Ok(members)
}
