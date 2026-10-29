//! Репозиторий household и участников.

use chrono::{DateTime, Utc};
use sqlx::PgConnection;
use uuid::Uuid;

use serde_json::json;

use crate::domain::error::DomainError;
use crate::domain::identity::{AuditEventType, NewMember, Onboarded};
use crate::domain::models::{Actor, Household, List, ListKind, User, UserRole};
use crate::domain::settings::validate_timezone;
use crate::domain::validation::normalize_text;

use super::{audit, users};

/// Данные владельца при создании household.
#[derive(Debug, Clone)]
pub struct NewOwner {
    /// Telegram-профиль владельца.
    pub member: NewMember,
    /// Личный часовой пояс.
    pub timezone: Option<String>,
}

/// Создать household без владельца: два списка по умолчанию.
///
/// Владельца добавляет администратор (`tasklerq admin user add --role owner`).
pub async fn create_empty(
    conn: &mut PgConnection,
    name: &str,
    timezone: &str,
    actor: &Actor,
    now: DateTime<Utc>,
) -> Result<Household, DomainError> {
    let household = insert_household(conn, name, timezone, now).await?;

    audit::record(
        conn,
        Some(household.id),
        actor,
        AuditEventType::HouseholdCreated,
        json!({"household_name": household.name, "timezone": household.timezone}),
        now,
    )
    .await?;

    Ok(household)
}

/// Создать household, владельца и два списка по умолчанию.
///
/// Единственная точка входа снаружи — [`onboard`]: она держит блокировку
/// Telegram ID и не создаёт второй household для того же человека.
async fn create_with_owner(
    conn: &mut PgConnection,
    name: &str,
    timezone: &str,
    owner: NewOwner,
    now: DateTime<Utc>,
) -> Result<(Household, User), DomainError> {
    if let Some(user_timezone) = owner.timezone.as_deref() {
        validate_timezone(user_timezone)?;
    }

    let household = insert_household(conn, name, timezone, now).await?;
    let user = users::insert(
        conn,
        household.id,
        UserRole::Owner,
        &owner.member,
        owner.timezone.as_deref(),
        now,
    )
    .await?;

    audit::record(
        conn,
        Some(household.id),
        &Actor::user(user.id, UserRole::Owner),
        AuditEventType::HouseholdCreated,
        json!({
            "household_name": household.name,
            "timezone": household.timezone,
            "actor_name": user.display_name,
        }),
        now,
    )
    .await?;

    Ok((household, user))
}

/// Вставить household и два списка по умолчанию.
async fn insert_household(
    conn: &mut PgConnection,
    name: &str,
    timezone: &str,
    now: DateTime<Utc>,
) -> Result<Household, DomainError> {
    let name = normalize_text(name, 100, "household name")?;
    let timezone = validate_timezone(timezone)?;

    let household = sqlx::query_as::<_, Household>(
        "INSERT INTO households (id, name, timezone, created_at, updated_at)
         VALUES ($1, $2, $3, $4, $4)
         RETURNING id, name, timezone, created_at, updated_at, deleted_at",
    )
    .bind(Uuid::now_v7())
    .bind(&name)
    .bind(&timezone)
    .bind(now)
    .fetch_one(&mut *conn)
    .await?;

    insert_default_list(conn, household.id, ListKind::Tasks, "Задачи", 0, now).await?;
    insert_default_list(conn, household.id, ListKind::Shopping, "Покупки", 1, now).await?;

    Ok(household)
}

/// Онбординг: пользователь без приглашения создаёт household.
///
/// Шаг идемпотентен: если у Telegram ID уже есть активная запись (повтор
/// подтверждения или владелец, добавленный администратором), возвращается
/// существующий household и второй не создаётся. Вызывать внутри транзакции.
pub async fn onboard(
    conn: &mut PgConnection,
    name: &str,
    timezone: &str,
    owner: NewOwner,
    now: DateTime<Utc>,
) -> Result<Onboarded, DomainError> {
    users::lock_telegram_user(conn, owner.member.telegram_user_id).await?;

    if let Some(user) = users::find_active_by_telegram(conn, owner.member.telegram_user_id).await? {
        let household = get(conn, user.household_id).await?;
        return Ok(Onboarded {
            household,
            user,
            created: false,
        });
    }

    let (household, user) = create_with_owner(conn, name, timezone, owner, now).await?;
    Ok(Onboarded {
        household,
        user,
        created: true,
    })
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

/// Все неудалённые household (для административных команд).
pub async fn list_active(conn: &mut PgConnection) -> Result<Vec<Household>, DomainError> {
    let households = sqlx::query_as::<_, Household>(
        "SELECT id, name, timezone, created_at, updated_at, deleted_at
         FROM households
         WHERE deleted_at IS NULL
         ORDER BY created_at, id",
    )
    .fetch_all(conn)
    .await?;

    Ok(households)
}
