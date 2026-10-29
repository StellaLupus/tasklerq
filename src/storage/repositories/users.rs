//! Репозиторий пользователей: поиск, добавление, профиль и настройки.

use chrono::{DateTime, Utc};
use serde_json::json;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::auth::access::{require_owner, require_user};
use crate::domain::error::DomainError;
use crate::domain::identity::{AuditEventType, NewMember};
use crate::domain::models::{Actor, User, UserRole};
use crate::domain::settings::{
    NOTIFICATIONS_KEY, NotificationSettings, ProfileUpdate, normalize_display_name,
    validate_timezone,
};

use super::audit;

pub(crate) const COLUMNS: &str = "id, household_id, telegram_user_id, telegram_chat_id, \
    display_name, username, role, timezone, settings, created_at, updated_at, deleted_at";

/// Максимальная длина Telegram username.
const MAX_USERNAME: usize = 64;

/// Получить активного участника household.
pub async fn get(
    conn: &mut PgConnection,
    household_id: Uuid,
    user_id: Uuid,
) -> Result<User, DomainError> {
    sqlx::query_as::<_, User>(&format!(
        "SELECT {COLUMNS} FROM users
         WHERE id = $1 AND household_id = $2 AND deleted_at IS NULL"
    ))
    .bind(user_id)
    .bind(household_id)
    .fetch_optional(conn)
    .await?
    .ok_or(DomainError::NotFound)
}

/// Активный пользователь по Telegram ID.
pub async fn find_active_by_telegram(
    conn: &mut PgConnection,
    telegram_user_id: i64,
) -> Result<Option<User>, DomainError> {
    let user = sqlx::query_as::<_, User>(&format!(
        "SELECT {COLUMNS} FROM users
         WHERE telegram_user_id = $1 AND deleted_at IS NULL"
    ))
    .bind(telegram_user_id)
    .fetch_optional(conn)
    .await?;

    Ok(user)
}

/// Активные владельцы household.
pub async fn owners(conn: &mut PgConnection, household_id: Uuid) -> Result<Vec<User>, DomainError> {
    let owners = sqlx::query_as::<_, User>(&format!(
        "SELECT {COLUMNS} FROM users
         WHERE household_id = $1 AND role = 'owner' AND deleted_at IS NULL
         ORDER BY created_at, id"
    ))
    .bind(household_id)
    .fetch_all(conn)
    .await?;

    Ok(owners)
}

/// Сериализовать операции с одним Telegram ID в пределах транзакции.
///
/// Онбординг, добавление администратором и приглашение проверяют «нет
/// активного пользователя» и затем вставляют строку; без блокировки две
/// параллельные операции упрутся в уникальный индекс вместо понятного ответа.
pub(crate) async fn lock_telegram_user(
    conn: &mut PgConnection,
    telegram_user_id: i64,
) -> Result<(), DomainError> {
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(telegram_user_id)
        .execute(conn)
        .await?;
    Ok(())
}

/// Вставить пользователя. Вызывающая сторона обязана держать
/// [`lock_telegram_user`] и проверить отсутствие активной записи.
pub(crate) async fn insert(
    conn: &mut PgConnection,
    household_id: Uuid,
    role: UserRole,
    member: &NewMember,
    timezone: Option<&str>,
    now: DateTime<Utc>,
) -> Result<User, DomainError> {
    let display_name = normalize_display_name(&member.display_name)?;
    if let Some(username) = member.username.as_deref()
        && username.chars().count() > MAX_USERNAME
    {
        return Err(DomainError::Validation(format!(
            "username must be at most {MAX_USERNAME} characters"
        )));
    }
    if let Some(timezone) = timezone {
        validate_timezone(timezone)?;
    }

    let user = sqlx::query_as::<_, User>(&format!(
        "INSERT INTO users (
             id, household_id, telegram_user_id, telegram_chat_id, display_name,
             username, role, timezone, settings, created_at, updated_at
         )
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, '{{}}'::jsonb, $9, $9)
         RETURNING {COLUMNS}"
    ))
    .bind(Uuid::now_v7())
    .bind(household_id)
    .bind(member.telegram_user_id)
    .bind(member.telegram_chat_id)
    .bind(&display_name)
    .bind(member.username.as_deref())
    .bind(role)
    .bind(timezone)
    .bind(now)
    .fetch_one(conn)
    .await?;

    Ok(user)
}

/// Добавить участника администратором (`tasklerq admin user add`).
///
/// Второй владелец не создаётся: владение передаётся штатным способом.
/// Добавление попадает в журнал от имени `actor`.
pub async fn add(
    conn: &mut PgConnection,
    household_id: Uuid,
    role: UserRole,
    member: NewMember,
    actor: &Actor,
    now: DateTime<Utc>,
) -> Result<User, DomainError> {
    lock_telegram_user(conn, member.telegram_user_id).await?;
    lock_household(conn, household_id).await?;

    if find_active_by_telegram(conn, member.telegram_user_id)
        .await?
        .is_some()
    {
        return Err(DomainError::conflict_with(
            "user already belongs to a household",
            json!({"reason": "already_member"}),
        ));
    }
    if role == UserRole::Owner && !owners(conn, household_id).await?.is_empty() {
        return Err(DomainError::conflict_with(
            "household already has an owner; transfer ownership instead",
            json!({"reason": "owner_exists"}),
        ));
    }

    let user = insert(conn, household_id, role, &member, None, now).await?;
    audit::record(
        conn,
        Some(household_id),
        actor,
        AuditEventType::MemberJoined,
        json!({
            "user_id": user.id,
            "display_name": user.display_name,
            "role": role,
        }),
        now,
    )
    .await?;

    Ok(user)
}

/// Заблокировать активный household до конца транзакции.
pub(crate) async fn lock_household(
    conn: &mut PgConnection,
    household_id: Uuid,
) -> Result<(), DomainError> {
    sqlx::query("SELECT id FROM households WHERE id = $1 AND deleted_at IS NULL FOR NO KEY UPDATE")
        .bind(household_id)
        .fetch_optional(conn)
        .await?
        .ok_or(DomainError::NotFound)?;
    Ok(())
}

/// Изменить профиль и настройки пользователя.
///
/// Пользователь меняет только собственный профиль. Telegram ID не
/// редактируется. Изменения затрагивают одну строку `users`, поэтому
/// настройки других участников не меняются.
pub async fn update_profile(
    conn: &mut PgConnection,
    household_id: Uuid,
    actor: &Actor,
    update: ProfileUpdate,
    now: DateTime<Utc>,
) -> Result<User, DomainError> {
    let user_id = require_user(actor)?;

    let current = sqlx::query_as::<_, User>(&format!(
        "SELECT {COLUMNS} FROM users
         WHERE id = $1 AND household_id = $2 AND deleted_at IS NULL
         FOR UPDATE"
    ))
    .bind(user_id)
    .bind(household_id)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(DomainError::NotFound)?;

    let display_name = match update.display_name {
        Some(name) => normalize_display_name(&name)?,
        None => current.display_name.clone(),
    };
    let timezone = match update.timezone {
        Some(Some(value)) => Some(validate_timezone(&value)?),
        Some(None) => None,
        None => current.timezone.clone(),
    };
    let mut settings = current.settings.clone();
    if let Some(notifications) = update.notifications {
        let next = notifications.apply(NotificationSettings::from_settings(&settings))?;
        if !settings.is_object() {
            settings = json!({});
        }
        settings[NOTIFICATIONS_KEY] = serde_json::to_value(next)
            .map_err(|error| DomainError::Validation(error.to_string()))?;
    }

    let updated = sqlx::query_as::<_, User>(&format!(
        "UPDATE users
         SET display_name = $3, timezone = $4, settings = $5, updated_at = $6
         WHERE id = $1 AND household_id = $2
         RETURNING {COLUMNS}"
    ))
    .bind(user_id)
    .bind(household_id)
    .bind(&display_name)
    .bind(timezone.as_deref())
    .bind(&settings)
    .bind(now)
    .fetch_one(conn)
    .await?;

    Ok(updated)
}

/// Активные участники household.
pub async fn list_active(
    conn: &mut PgConnection,
    household_id: Uuid,
) -> Result<Vec<User>, DomainError> {
    let members = sqlx::query_as::<_, User>(&format!(
        "SELECT {COLUMNS} FROM users
         WHERE household_id = $1 AND deleted_at IS NULL
         ORDER BY role, display_name, id"
    ))
    .bind(household_id)
    .fetch_all(conn)
    .await?;

    Ok(members)
}

/// Число активных участников.
pub async fn count_active(conn: &mut PgConnection, household_id: Uuid) -> Result<i64, DomainError> {
    let count = sqlx::query_scalar(
        "SELECT count(*) FROM users WHERE household_id = $1 AND deleted_at IS NULL",
    )
    .bind(household_id)
    .fetch_one(conn)
    .await?;

    Ok(count)
}

/// Изменить роль активного участника.
pub(crate) async fn set_role(
    conn: &mut PgConnection,
    household_id: Uuid,
    user_id: Uuid,
    role: UserRole,
    now: DateTime<Utc>,
) -> Result<(), DomainError> {
    sqlx::query(
        "UPDATE users SET role = $3, updated_at = $4
         WHERE id = $1 AND household_id = $2 AND deleted_at IS NULL",
    )
    .bind(user_id)
    .bind(household_id)
    .bind(role)
    .bind(now)
    .execute(conn)
    .await?;

    Ok(())
}

/// Заблокировать household и проверить владельца под блокировкой.
///
/// Общее начало операций управления household. Актор без роли владельца
/// отклоняется до блокировки, чтобы не держать строку household впустую.
pub(crate) async fn lock_and_require_owner(
    conn: &mut PgConnection,
    household_id: Uuid,
    actor: &Actor,
) -> Result<User, DomainError> {
    require_owner(actor)?;
    lock_household(conn, household_id).await?;
    require_current_owner(conn, household_id, actor).await
}

/// Проверить владельца по текущим данным базы, а не только по роли в `Actor`.
///
/// `Actor` собирается вызывающей стороной и может устареть (например, после
/// передачи владения), поэтому операции управления household сверяют роль с
/// базой. Пользователь не из этого household получает `NotFound`.
pub(crate) async fn require_current_owner(
    conn: &mut PgConnection,
    household_id: Uuid,
    actor: &Actor,
) -> Result<User, DomainError> {
    let user_id = require_owner(actor)?;
    let user = get(conn, household_id, user_id).await?;
    if user.role != UserRole::Owner {
        return Err(DomainError::Forbidden(
            "only the household owner can perform this action".into(),
        ));
    }
    Ok(user)
}
