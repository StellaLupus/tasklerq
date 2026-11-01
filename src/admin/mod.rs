//! Административный CLI: токены, household, пользователи, очереди и экспорт.
//!
//! Функции этого модуля работают поверх пула и открывают собственные
//! транзакции; разбор аргументов и вывод находятся в `main.rs`. Webhook
//! вызывается из `main.rs` напрямую через клиент Bot API.

use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::auth::tokens::{self, IssuedToken};
use crate::domain::error::DomainError;
use crate::domain::identity::NewMember;
use crate::domain::models::{Actor, ActorType, Household, Item, ListKind, User, UserRole};
use crate::storage::repositories::items;
use crate::storage::repositories::tokens::ApiToken;
use crate::storage::repositories::{households, lists, users};
use crate::telegram::queue;

/// Имя участника, если администратор его не указал.
pub const DEFAULT_MEMBER_NAME: &str = "Участник";

/// Максимальный срок токена, дней: десять лет.
pub const MAX_TOKEN_EXPIRY_DAYS: i64 = 3650;

/// Актор административных действий: не пользователь и не интеграция.
pub fn admin_actor() -> Actor {
    Actor::system(ActorType::System)
}

/// Выпустить токен household. Открытое значение возвращается один раз.
pub async fn create_token(
    pool: &PgPool,
    household_id: Option<Uuid>,
    name: &str,
    scopes: &[String],
    expires_in_days: Option<i64>,
    now: DateTime<Utc>,
) -> Result<IssuedToken, DomainError> {
    let expires_at = match expires_in_days {
        Some(days) if (1..=MAX_TOKEN_EXPIRY_DAYS).contains(&days) => Some(
            Duration::try_days(days)
                .and_then(|span| now.checked_add_signed(span))
                .ok_or_else(|| DomainError::Validation("expiry is out of range".into()))?,
        ),
        Some(_) => {
            return Err(DomainError::Validation(format!(
                "expiry must be between 1 and {MAX_TOKEN_EXPIRY_DAYS} days"
            )));
        }
        None => None,
    };

    let mut tx = pool.begin().await?;
    let household = resolve_household(&mut tx, household_id).await?;
    let issued = tokens::issue(
        &mut tx,
        household.id,
        name,
        scopes,
        expires_at,
        &admin_actor(),
        now,
    )
    .await?;
    tx.commit().await?;

    Ok(issued)
}

/// Отозвать токен. Второй элемент — был ли токен отозван сейчас.
pub async fn revoke_token(
    pool: &PgPool,
    token_id: Uuid,
    now: DateTime<Utc>,
) -> Result<(ApiToken, bool), DomainError> {
    let mut tx = pool.begin().await?;
    let result = tokens::revoke(&mut tx, token_id, &admin_actor(), now).await?;
    tx.commit().await?;

    Ok(result)
}

/// Токены household без секретов.
pub async fn list_tokens(
    pool: &PgPool,
    household_id: Option<Uuid>,
) -> Result<Vec<ApiToken>, DomainError> {
    let mut conn = pool.acquire().await?;
    let household = resolve_household(&mut conn, household_id).await?;
    crate::storage::repositories::tokens::list(&mut conn, household.id).await
}

/// Создать household без владельца.
pub async fn create_household(
    pool: &PgPool,
    name: &str,
    timezone: &str,
    now: DateTime<Utc>,
) -> Result<Household, DomainError> {
    let mut tx = pool.begin().await?;
    let household = households::create_empty(&mut tx, name, timezone, &admin_actor(), now).await?;
    tx.commit().await?;

    Ok(household)
}

/// Добавить участника. Без `household_id` используется единственный
/// существующий household.
pub async fn add_user(
    pool: &PgPool,
    household_id: Option<Uuid>,
    role: UserRole,
    telegram_user_id: i64,
    display_name: Option<&str>,
    now: DateTime<Utc>,
) -> Result<(Household, User), DomainError> {
    let mut tx = pool.begin().await?;
    let household = resolve_household(&mut tx, household_id).await?;

    // В приватном чате chat_id равен Telegram ID, поэтому CLI достаточно ID.
    let user = users::add(
        &mut tx,
        household.id,
        role,
        NewMember {
            telegram_user_id,
            telegram_chat_id: telegram_user_id,
            display_name: display_name.unwrap_or(DEFAULT_MEMBER_NAME).to_owned(),
            username: None,
        },
        &admin_actor(),
        now,
    )
    .await?;
    tx.commit().await?;

    Ok((household, user))
}

/// Сводка household для `admin household list`.
#[derive(Debug, Clone, Serialize)]
pub struct HouseholdSummary {
    /// Идентификатор.
    pub id: Uuid,
    /// Название.
    pub name: String,
    /// Часовой пояс IANA.
    pub timezone: String,
    /// Число активных участников.
    pub members: i64,
    /// Время создания.
    pub created_at: DateTime<Utc>,
}

/// Все неудалённые household с числом участников.
pub async fn list_households(pool: &PgPool) -> Result<Vec<HouseholdSummary>, DomainError> {
    let rows: Vec<(Uuid, String, String, DateTime<Utc>, i64)> = sqlx::query_as(
        "SELECT h.id, h.name, h.timezone, h.created_at, count(u.id)
         FROM households h
         LEFT JOIN users u ON u.household_id = h.id AND u.deleted_at IS NULL
         WHERE h.deleted_at IS NULL
         GROUP BY h.id, h.name, h.timezone, h.created_at
         ORDER BY h.created_at, h.id",
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(
            |(id, name, timezone, created_at, members)| HouseholdSummary {
                id,
                name,
                timezone,
                members,
                created_at,
            },
        )
        .collect())
}

/// Количество заданий каждой очереди по статусам.
///
/// Содержимое сообщений и payload не читается: только `status` и `count`.
#[derive(Debug, Clone, Default)]
pub struct QueueStats {
    /// `telegram_updates` по статусам.
    pub telegram_updates: Vec<(String, i64)>,
    /// `outbox` по статусам.
    pub outbox: Vec<(String, i64)>,
    /// `capture_jobs` по статусам.
    pub capture_jobs: Vec<(String, i64)>,
    /// `notification_deliveries` по статусам.
    pub notification_deliveries: Vec<(String, i64)>,
    /// Время самого старого готового обновления.
    pub oldest_ready: Option<DateTime<Utc>>,
    /// Момент расчёта; нужен для возраста oldest_ready.
    pub now: DateTime<Utc>,
}

/// Собрать статистику очередей.
pub async fn queue_stats(pool: &PgPool, now: DateTime<Utc>) -> Result<QueueStats, DomainError> {
    let mut conn = pool.acquire().await?;

    let telegram_updates = status_counts(&mut conn, "telegram_updates").await?;
    let outbox = status_counts(&mut conn, "outbox").await?;
    let capture_jobs = status_counts(&mut conn, "capture_jobs").await?;
    let notification_deliveries = status_counts(&mut conn, "notification_deliveries").await?;
    let oldest_ready = queue::oldest_ready(&mut conn, now).await?;

    Ok(QueueStats {
        telegram_updates,
        outbox,
        capture_jobs,
        notification_deliveries,
        oldest_ready,
        now,
    })
}

/// Счётчики по статусам одной таблицы. Имя таблицы задаётся только
/// константой в этом модуле.
async fn status_counts(
    conn: &mut PgConnection,
    table: &'static str,
) -> Result<Vec<(String, i64)>, sqlx::Error> {
    sqlx::query_as(&format!(
        "SELECT status::text, count(*)::bigint
         FROM {table}
         GROUP BY status
         ORDER BY status"
    ))
    .fetch_all(conn)
    .await
}

/// Выгрузка household: список, задачи, участники и их настройки.
///
/// Хеши токенов, callback-токены, outbox, очереди и `llm_runs` не читаются.
#[derive(Debug, Serialize)]
pub struct Export {
    /// Момент формирования выгрузки.
    pub exported_at: DateTime<Utc>,
    /// Household.
    pub household: ExportHousehold,
    /// Активные участники.
    pub members: Vec<ExportMember>,
    /// Неудалённые списки, включая архивные.
    pub lists: Vec<ExportList>,
    /// Неудалённые задачи, включая выполненные.
    pub items: Vec<ExportItem>,
}

/// Household в выгрузке.
#[derive(Debug, Serialize)]
pub struct ExportHousehold {
    /// Идентификатор.
    pub id: Uuid,
    /// Название.
    pub name: String,
    /// Часовой пояс IANA.
    pub timezone: String,
    /// Время создания.
    pub created_at: DateTime<Utc>,
    /// Настройки household. В схеме отдельной таблицы нет, поэтому поле
    /// остаётся пустым объектом; настройки уведомлений — у участников.
    pub settings: serde_json::Value,
}

/// Участник в выгрузке: без Telegram chat state.
#[derive(Debug, Serialize)]
pub struct ExportMember {
    /// Идентификатор.
    pub id: Uuid,
    /// Отображаемое имя.
    pub display_name: String,
    /// Telegram username, если известен.
    pub username: Option<String>,
    /// Роль.
    pub role: UserRole,
    /// Личный часовой пояс.
    pub timezone: Option<String>,
    /// Настройки участника.
    pub settings: serde_json::Value,
    /// Время создания.
    pub created_at: DateTime<Utc>,
}

/// Список в выгрузке.
#[derive(Debug, Serialize)]
pub struct ExportList {
    /// Идентификатор.
    pub id: Uuid,
    /// Тип.
    pub kind: ListKind,
    /// Название.
    pub name: String,
    /// Список по умолчанию.
    pub is_default: bool,
    /// Архивирован.
    pub is_archived: bool,
    /// Позиция.
    pub position: i32,
    /// Время создания.
    pub created_at: DateTime<Utc>,
    /// Время изменения.
    pub updated_at: DateTime<Utc>,
}

/// Задача в выгрузке: все поля `items` плюс напоминания.
#[derive(Debug, Serialize)]
pub struct ExportItem {
    /// Поля элемента.
    #[serde(flatten)]
    pub item: Item,
    /// Записи напоминаний по этому элементу.
    pub reminders: Vec<Reminder>,
}

/// Напоминание, связанное с задачей.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Reminder {
    /// Тип уведомления.
    pub kind: String,
    /// Ключ вхождения.
    pub occurrence_key: String,
    /// Статус доставки.
    pub status: String,
    /// Время, начиная с которого напоминание можно доставлять.
    pub scheduled_at: DateTime<Utc>,
    /// Запись перенесена тихими часами.
    pub deferred: bool,
}

#[derive(sqlx::FromRow)]
struct ReminderRow {
    item_id: Uuid,
    kind: String,
    occurrence_key: String,
    status: String,
    scheduled_at: DateTime<Utc>,
    deferred: bool,
}

/// Сформировать выгрузку household.
///
/// Мягко удалённые участники, списки и задачи не включаются.
pub async fn export(
    pool: &PgPool,
    household_id: Option<Uuid>,
    now: DateTime<Utc>,
) -> Result<Export, DomainError> {
    let mut conn = pool.acquire().await?;
    let household = resolve_household(&mut conn, household_id).await?;
    let members = users::list_active(&mut conn, household.id).await?;
    let lists = lists::list(&mut conn, household.id).await?;
    let items = items::all_for_export(&mut conn, household.id).await?;
    let reminders = reminders_for(&mut conn, household.id).await?;

    let mut by_item: HashMap<Uuid, Vec<Reminder>> = HashMap::new();
    for row in reminders {
        by_item.entry(row.item_id).or_default().push(Reminder {
            kind: row.kind,
            occurrence_key: row.occurrence_key,
            status: row.status,
            scheduled_at: row.scheduled_at,
            deferred: row.deferred,
        });
    }

    Ok(Export {
        exported_at: now,
        household: ExportHousehold {
            id: household.id,
            name: household.name,
            timezone: household.timezone,
            created_at: household.created_at,
            settings: serde_json::Value::Object(serde_json::Map::new()),
        },
        members: members
            .into_iter()
            .map(|user| ExportMember {
                id: user.id,
                display_name: user.display_name,
                username: user.username,
                role: user.role,
                timezone: user.timezone,
                settings: user.settings,
                created_at: user.created_at,
            })
            .collect(),
        lists: lists
            .into_iter()
            .map(|list| ExportList {
                id: list.id,
                kind: list.kind,
                name: list.name,
                is_default: list.is_default,
                is_archived: list.is_archived,
                position: list.position,
                created_at: list.created_at,
                updated_at: list.updated_at,
            })
            .collect(),
        items: items
            .into_iter()
            .map(|item| ExportItem {
                reminders: by_item.remove(&item.id).unwrap_or_default(),
                item,
            })
            .collect(),
    })
}

async fn reminders_for(
    conn: &mut PgConnection,
    household_id: Uuid,
) -> Result<Vec<ReminderRow>, DomainError> {
    let rows = sqlx::query_as::<_, ReminderRow>(
        "SELECT item_id, kind::text AS kind, occurrence_key,
                status::text AS status, scheduled_at, deferred
         FROM notification_deliveries
         WHERE household_id = $1
         ORDER BY scheduled_at, id",
    )
    .bind(household_id)
    .fetch_all(conn)
    .await?;
    Ok(rows)
}

/// Household из аргумента или, если его не указали, единственный существующий.
pub async fn resolve_household(
    conn: &mut sqlx::PgConnection,
    household_id: Option<Uuid>,
) -> Result<Household, DomainError> {
    match household_id {
        Some(id) => households::get(conn, id).await,
        None => select_household(households::list_active(conn).await?),
    }
}

/// Выбрать household, когда администратор его не указал: допустим только
/// единственный.
pub fn select_household(mut all: Vec<Household>) -> Result<Household, DomainError> {
    match all.len() {
        0 => Err(DomainError::Validation(
            "no household exists; create one with `admin household create`".into(),
        )),
        1 => Ok(all.remove(0)),
        count => Err(DomainError::Validation(format!(
            "{count} households exist; pass --household"
        ))),
    }
}

/// Человекочитаемое описание ошибки для CLI с HTTP-подобным кодом.
pub fn describe(error: &DomainError) -> String {
    match error {
        DomainError::Validation(message) => format!("invalid input (422): {message}"),
        DomainError::NotFound => "not found (404)".into(),
        DomainError::Forbidden(message) => format!("forbidden (403): {message}"),
        DomainError::Conflict { message, .. } => format!("conflict (409): {message}"),
        DomainError::Database(error) => format!("database error: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn household(name: &str) -> Household {
        Household {
            id: Uuid::now_v7(),
            name: name.into(),
            timezone: "Europe/Moscow".into(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            deleted_at: None,
        }
    }

    #[test]
    fn implicit_household_must_be_unique() {
        assert!(select_household(Vec::new()).is_err());
        let single = household("A");
        assert_eq!(
            select_household(vec![single.clone()]).expect("single").id,
            single.id
        );
        assert!(select_household(vec![household("A"), household("B")]).is_err());
    }

    #[test]
    fn errors_carry_http_like_codes() {
        assert!(describe(&DomainError::conflict("x")).contains("409"));
        assert!(describe(&DomainError::NotFound).contains("404"));
    }
}
