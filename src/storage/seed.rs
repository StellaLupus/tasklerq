//! Идемпотентные демонстрационные данные.

use chrono::{Datelike, Duration, Utc};
use sqlx::PgPool;
use uuid::Uuid;

const HOUSEHOLD: &str = "0190a000-0000-7000-8000-000000000001";
const OWNER: &str = "0190a000-0000-7000-8000-000000000002";
const MEMBER: &str = "0190a000-0000-7000-8000-000000000003";
const TASKS_LIST: &str = "0190a000-0000-7000-8000-000000000010";
const SHOPPING_LIST: &str = "0190a000-0000-7000-8000-000000000011";
const TAG_HOME: &str = "0190a000-0000-7000-8000-000000000020";
const TAG_URGENT: &str = "0190a000-0000-7000-8000-000000000021";
const ITEM_CALL: &str = "0190a000-0000-7000-8000-000000000030";
const ITEM_GARBAGE: &str = "0190a000-0000-7000-8000-000000000031";
const ITEM_MILK: &str = "0190a000-0000-7000-8000-000000000032";
const ITEM_BREAD: &str = "0190a000-0000-7000-8000-000000000033";
const ITEM_EGGS: &str = "0190a000-0000-7000-8000-000000000034";
const SUBTASK_FIND: &str = "0190a000-0000-7000-8000-000000000040";
const SUBTASK_CALL: &str = "0190a000-0000-7000-8000-000000000041";
const EVENT_CALL: &str = "0190a000-0000-7000-8000-000000000050";
const EVENT_GARBAGE: &str = "0190a000-0000-7000-8000-000000000051";
const EVENT_MILK: &str = "0190a000-0000-7000-8000-000000000052";
const EVENT_BREAD: &str = "0190a000-0000-7000-8000-000000000053";
const EVENT_EGGS: &str = "0190a000-0000-7000-8000-000000000054";

fn uuid(value: &str) -> Uuid {
    Uuid::parse_str(value).expect("seed UUID is valid")
}

/// Заполнить базу демонстрационными данными. Повторный запуск не создаёт дублей.
pub async fn run(pool: &PgPool) -> Result<(), sqlx::Error> {
    let now = Utc::now();
    let today = now.date_naive();
    let tomorrow = today + Duration::days(1);
    let in_three_days = today + Duration::days(3);
    let weekday = i16::try_from(today.weekday().number_from_monday()).unwrap_or(1);

    let mut tx = pool.begin().await?;

    sqlx::query(
        "INSERT INTO households (id, name, timezone, created_at, updated_at)
         VALUES ($1, 'Демо-семья', 'Europe/Moscow', $2, $2)
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(uuid(HOUSEHOLD))
    .bind(now)
    .execute(&mut *tx)
    .await?;

    sqlx::query(
        "INSERT INTO users (
             id, household_id, telegram_user_id, telegram_chat_id, display_name,
             username, role, timezone, settings, created_at, updated_at
         )
         VALUES
             ($1, $3, 900000001, 900000001, 'Алексей', 'alexey', 'owner',
              'Europe/Moscow', '{}'::jsonb, $4, $4),
             ($2, $3, 900000002, 900000002, 'Мария', 'maria', 'member',
              'Europe/Moscow', '{}'::jsonb, $4, $4)
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(uuid(OWNER))
    .bind(uuid(MEMBER))
    .bind(uuid(HOUSEHOLD))
    .bind(now)
    .execute(&mut *tx)
    .await?;

    sqlx::query(
        "INSERT INTO lists (
             id, household_id, kind, name, is_default, is_archived, position,
             created_at, updated_at
         )
         VALUES
             ($1, $3, 'tasks', 'Задачи', true, false, 0, $4, $4),
             ($2, $3, 'shopping', 'Покупки', true, false, 1, $4, $4)
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(uuid(TASKS_LIST))
    .bind(uuid(SHOPPING_LIST))
    .bind(uuid(HOUSEHOLD))
    .bind(now)
    .execute(&mut *tx)
    .await?;

    sqlx::query(
        "INSERT INTO tags (id, household_id, name, created_at)
         VALUES ($1, $3, 'дом', $4), ($2, $3, 'срочно', $4)
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(uuid(TAG_HOME))
    .bind(uuid(TAG_URGENT))
    .bind(uuid(HOUSEHOLD))
    .bind(now)
    .execute(&mut *tx)
    .await?;

    insert_task(
        &mut tx,
        uuid(ITEM_CALL),
        uuid(HOUSEHOLD),
        uuid(TASKS_LIST),
        "Позвонить маме",
        Some("Обсудить выходные"),
        "high",
        Some(tomorrow),
        None,
        None,
        uuid(OWNER),
        now,
    )
    .await?;
    insert_task(
        &mut tx,
        uuid(ITEM_GARBAGE),
        uuid(HOUSEHOLD),
        uuid(TASKS_LIST),
        "Вынести мусор",
        None,
        "normal",
        Some(in_three_days),
        Some(weekday),
        Some("Europe/Moscow"),
        uuid(OWNER),
        now,
    )
    .await?;

    insert_shopping_item(
        &mut tx,
        uuid(ITEM_MILK),
        uuid(HOUSEHOLD),
        uuid(SHOPPING_LIST),
        "Молоко",
        2.0,
        "л",
        0,
        uuid(OWNER),
        now,
    )
    .await?;
    insert_shopping_item(
        &mut tx,
        uuid(ITEM_BREAD),
        uuid(HOUSEHOLD),
        uuid(SHOPPING_LIST),
        "Хлеб",
        1.0,
        "шт",
        1,
        uuid(MEMBER),
        now,
    )
    .await?;
    insert_shopping_item(
        &mut tx,
        uuid(ITEM_EGGS),
        uuid(HOUSEHOLD),
        uuid(SHOPPING_LIST),
        "Яйца",
        10.0,
        "шт",
        2,
        uuid(OWNER),
        now,
    )
    .await?;
    sqlx::query(
        "UPDATE items
         SET status = 'done', completed_at = $2, completed_by_user_id = $3
         WHERE id = $1 AND status = 'open'",
    )
    .bind(uuid(ITEM_EGGS))
    .bind(now)
    .bind(uuid(OWNER))
    .execute(&mut *tx)
    .await?;

    sqlx::query(
        "INSERT INTO subtasks (id, item_id, title, status, position, created_at)
         VALUES
             ($1, $3, 'Найти номер', 'open', 0, $4),
             ($2, $3, 'Позвонить', 'open', 1, $4)
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(uuid(SUBTASK_FIND))
    .bind(uuid(SUBTASK_CALL))
    .bind(uuid(ITEM_CALL))
    .bind(now)
    .execute(&mut *tx)
    .await?;

    sqlx::query(
        "INSERT INTO item_tags (item_id, tag_id)
         VALUES ($1, $2), ($1, $3)
         ON CONFLICT DO NOTHING",
    )
    .bind(uuid(ITEM_CALL))
    .bind(uuid(TAG_HOME))
    .bind(uuid(TAG_URGENT))
    .execute(&mut *tx)
    .await?;

    for (event, item) in [
        (EVENT_CALL, ITEM_CALL),
        (EVENT_GARBAGE, ITEM_GARBAGE),
        (EVENT_MILK, ITEM_MILK),
        (EVENT_BREAD, ITEM_BREAD),
        (EVENT_EGGS, ITEM_EGGS),
    ] {
        sqlx::query(
            "INSERT INTO item_events (
                 id, household_id, item_id, actor_user_id, actor_type, event_type,
                 payload, created_at
             )
             VALUES ($1, $2, $3, $4, 'user', 'created', '{\"seed\": true}'::jsonb, $5)
             ON CONFLICT (id) DO NOTHING",
        )
        .bind(uuid(event))
        .bind(uuid(HOUSEHOLD))
        .bind(uuid(item))
        .bind(uuid(OWNER))
        .bind(now)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    tracing::info!("seed: demo household is ready");
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn insert_task(
    tx: &mut sqlx::PgConnection,
    id: Uuid,
    household_id: Uuid,
    list_id: Uuid,
    title: &str,
    notes: Option<&str>,
    priority: &str,
    due_date: Option<chrono::NaiveDate>,
    recurrence_weekday: Option<i16>,
    recurrence_timezone: Option<&str>,
    created_by: Uuid,
    now: chrono::DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO items (
             id, household_id, list_id, title, notes, status, priority, due_date,
             created_by_user_id, source, position, recurrence_freq, recurrence_interval,
             recurrence_byweekday, recurrence_timezone, recurrence_anchor_date,
             created_at, updated_at
         )
         VALUES (
             $1, $2, $3, $4, $5, 'open', $6::item_priority, $7, $8, 'import', 0,
             CASE WHEN $9::smallint IS NULL THEN NULL ELSE 'weekly'::recurrence_freq END,
             CASE WHEN $9::smallint IS NULL THEN NULL ELSE 1::smallint END,
             CASE WHEN $9::smallint IS NULL THEN NULL ELSE ARRAY[$9::smallint] END,
             $10,
             CASE WHEN $9::smallint IS NULL THEN NULL ELSE $7::date END,
             $11, $11
         )
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(id)
    .bind(household_id)
    .bind(list_id)
    .bind(title)
    .bind(notes)
    .bind(priority)
    .bind(due_date)
    .bind(created_by)
    .bind(recurrence_weekday)
    .bind(recurrence_timezone)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn insert_shopping_item(
    tx: &mut sqlx::PgConnection,
    id: Uuid,
    household_id: Uuid,
    list_id: Uuid,
    title: &str,
    quantity: f64,
    unit: &str,
    position: i32,
    created_by: Uuid,
    now: chrono::DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO items (
             id, household_id, list_id, title, status, priority, created_by_user_id,
             source, quantity, unit, position, created_at, updated_at
         )
         VALUES ($1, $2, $3, $4, 'open', 'normal', $5, 'import',
                 $6::float8::numeric, $7, $8, $9, $9)
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(id)
    .bind(household_id)
    .bind(list_id)
    .bind(title)
    .bind(created_by)
    .bind(quantity)
    .bind(unit)
    .bind(position)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    Ok(())
}
