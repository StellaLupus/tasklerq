//! Репозиторий household и участников.

use chrono::{DateTime, Utc};
use sqlx::PgConnection;
use uuid::Uuid;

use serde_json::json;

use crate::config::TenancyMode;
use crate::domain::error::DomainError;
use crate::domain::identity::{AuditEventType, NewMember, Onboarded};
use crate::domain::models::{Actor, ActorType, Household, List, ListKind, User, UserRole};
use crate::domain::settings::validate_timezone;
use crate::domain::validation::normalize_text;

use super::{audit, users};

/// Класс блокировки инсталляции в двухкомпонентном пространстве ключей.
///
/// Двухкомпонентная форма `pg_advisory_xact_lock(int4, int4)` не пересекается
/// с однокомпонентными bigint-ключами: `lock_telegram_user` берёт ключ, равный
/// Telegram ID, а доставка — свой ключ получателя.
const INSTALLATION_LOCK_CLASS: i32 = 0x544C_5251; // "TLRQ"
/// Объект блокировки: создание household на инсталляции.
const INSTALLATION_LOCK_OBJECT: i32 = 0x494E_5354; // "INST"

/// Данные владельца при создании household.
#[derive(Debug, Clone)]
pub struct NewOwner {
    /// Telegram-профиль владельца.
    pub member: NewMember,
    /// Личный часовой пояс.
    pub timezone: Option<String>,
}

/// Данные нового household.
#[derive(Debug, Clone)]
pub struct NewHousehold {
    /// Название.
    pub name: String,
    /// Часовой пояс IANA.
    pub timezone: String,
    /// Владелец; без него household создаёт администратор и добавляет владельца
    /// отдельной командой.
    pub owner: Option<NewOwner>,
}

/// Создать household, два списка по умолчанию и (если указан) владельца.
///
/// Единственная публичная точка создания вместе с [`onboard`]. Порядок
/// блокировок: сначала Telegram ID владельца, затем installation. В режиме
/// [`TenancyMode::Single`] активный household может быть только один; проверка
/// под advisory-блокировкой не даёт двум одновременным запросам создать два.
pub async fn create(
    conn: &mut PgConnection,
    mode: TenancyMode,
    new: NewHousehold,
    actor: &Actor,
    now: DateTime<Utc>,
) -> Result<(Household, Option<User>), DomainError> {
    if let Some(owner) = &new.owner {
        users::lock_telegram_user(conn, owner.member.telegram_user_id).await?;
    }
    sqlx::query("SELECT pg_advisory_xact_lock($1, $2)")
        .bind(INSTALLATION_LOCK_CLASS)
        .bind(INSTALLATION_LOCK_OBJECT)
        .execute(&mut *conn)
        .await?;

    if mode == TenancyMode::Single {
        let active: i64 =
            sqlx::query_scalar("SELECT count(*) FROM households WHERE deleted_at IS NULL")
                .fetch_one(&mut *conn)
                .await?;
        if active >= 1 {
            return Err(DomainError::conflict_with(
                "a household already exists; only one is allowed in single mode",
                json!({"reason": "household_exists"}),
            ));
        }
    }

    if let Some(owner) = &new.owner
        && let Some(user_timezone) = owner.timezone.as_deref()
    {
        validate_timezone(user_timezone)?;
    }

    let household = insert_household(conn, &new.name, &new.timezone, now).await?;

    let Some(owner) = new.owner else {
        audit::record(
            conn,
            Some(household.id),
            actor,
            AuditEventType::HouseholdCreated,
            json!({"household_name": household.name, "timezone": household.timezone}),
            now,
        )
        .await?;
        return Ok((household, None));
    };

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

    Ok((household, Some(user)))
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
///
/// Идемпотентность по Telegram ID важнее режима: повторный шаг возвращает
/// существующий household даже в [`TenancyMode::Single`], когда household уже
/// есть. Для нового ID создание идёт через [`create`].
pub async fn onboard(
    conn: &mut PgConnection,
    mode: TenancyMode,
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

    // `create` повторно берёт блокировку того же Telegram ID. Повторный
    // `pg_advisory_xact_lock` в той же транзакции безопасен: блокировка
    // реентерабельна и освобождается только с концом транзакции. Порядок
    // «Telegram ID → installation» при этом сохраняется, взаимоблокировки с
    // `user add` нет. Актор для аудита здесь не важен: при наличии владельца
    // `create` пишет событие от его имени.
    let (household, user) = create(
        conn,
        mode,
        NewHousehold {
            name: name.to_owned(),
            timezone: timezone.to_owned(),
            owner: Some(owner),
        },
        &Actor::system(ActorType::System),
        now,
    )
    .await?;

    Ok(Onboarded {
        household,
        user: user.expect("a household created with an owner contains the owner"),
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
