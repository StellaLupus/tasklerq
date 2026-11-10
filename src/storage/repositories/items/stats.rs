//! Счётчики для главного меню и списков.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, NaiveDate, Utc};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::events::effective_end;
use crate::domain::models::Item;
use crate::domain::recurrence::parse_timezone;

use super::completions;
use super::row::{ITEM_COLUMNS, ItemRow, hydrate_many};

/// Условие просроченной задачи: срок со временем прошёл или вседневный срок
/// раньше сегодняшнего дня. Параметры — `$2` (сейчас) и `$3` (сегодня в поясе
/// пользователя).
const OVERDUE_TASKS_CONDITION: &str = "i.due_at < $2 OR i.due_date < $3";

/// Счётчики главного меню.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MenuCounts {
    /// Открытые задачи во всех списках `tasks`.
    pub open_tasks: i64,
    /// Из них просроченные.
    pub overdue_tasks: i64,
    /// Открытые задачи и события на сегодня: задачи со сроком на текущий день
    /// и ещё не просроченные, события, которые начинаются сегодня или ещё идут.
    pub due_today_tasks: i64,
    /// Невыполненные позиции в `shopping` и `checklist`.
    pub open_shopping: i64,
    /// Выполнения задач сегодня по правилу раздела «Выполнено сегодня».
    pub done_today: i64,
}

/// Начало и конец события для счётчика «на сегодня».
#[derive(sqlx::FromRow)]
struct EventTimes {
    due_at: Option<DateTime<Utc>>,
    due_date: Option<NaiveDate>,
    end_at: Option<DateTime<Utc>>,
    end_date: Option<NaiveDate>,
}

/// Счётчики меню на момент `now` в часовом поясе `timezone`.
pub async fn menu_counts(
    conn: &mut PgConnection,
    household_id: Uuid,
    timezone: &str,
    now: DateTime<Utc>,
) -> Result<MenuCounts, DomainError> {
    let household_timezone: String =
        sqlx::query_scalar("SELECT timezone FROM households WHERE id = $1")
            .bind(household_id)
            .fetch_one(&mut *conn)
            .await?;
    menu_counts_with_timezone(conn, household_id, timezone, &household_timezone, now).await
}

/// Счётчики меню с уже известным поясом household.
///
/// Вызывающий экран уже держит household, поэтому не делает лишний `SELECT`.
pub async fn menu_counts_with_timezone(
    conn: &mut PgConnection,
    household_id: Uuid,
    timezone: &str,
    household_timezone: &str,
    now: DateTime<Utc>,
) -> Result<MenuCounts, DomainError> {
    let tz = parse_timezone(timezone)?;
    let today = now.with_timezone(&tz).date_naive();
    // Вседневный конец события считается в поясе household: так событие
    // проходит для всех участников одновременно (design, решение 3).
    let household_tz = parse_timezone(household_timezone)?;

    let (open_tasks, overdue_tasks, due_today_tasks, open_shopping): (i64, i64, i64, i64) =
        sqlx::query_as(&format!(
            "SELECT
                 count(*) FILTER (WHERE l.kind = 'tasks' AND i.status = 'open'),
                 count(*) FILTER (
                     WHERE l.kind = 'tasks' AND i.status = 'open'
                       AND ({OVERDUE_TASKS_CONDITION})
                 ),
                 count(*) FILTER (
                     WHERE l.kind = 'tasks' AND i.status = 'open'
                       AND (i.due_date = $3
                            OR (i.due_at >= $2
                                AND (i.due_at AT TIME ZONE $4)::date = $3))
                 ),
                 -- 0009-events 8.x/9.x: открытые события не считаются покупками.
                 count(*) FILTER (
                     WHERE l.kind IN ('shopping', 'checklist') AND i.status = 'open'
                 )
             FROM items i
             JOIN lists l ON l.id = i.list_id
             WHERE i.household_id = $1
               AND i.deleted_at IS NULL
               AND l.deleted_at IS NULL
               AND NOT l.is_archived",
        ))
        .bind(household_id)
        .bind(now)
        .bind(today)
        .bind(timezone)
        .fetch_one(&mut *conn)
        .await?;

    // 0009-events 8.1: событие на сегодня начинается сегодня или началось
    // раньше и ещё не прошло. Конец считает домен: у вседневных событий это
    // начало следующего дня в поясе household, включая переходы DST, которые
    // SQL повторил бы неточно.
    let events: Vec<EventTimes> = sqlx::query_as(
        "SELECT i.due_at, i.due_date, i.end_at, i.end_date
         FROM items i
         JOIN lists l ON l.id = i.list_id
         WHERE i.household_id = $1
           AND i.deleted_at IS NULL
           AND l.deleted_at IS NULL
           AND NOT l.is_archived
           AND l.kind = 'events'
           AND i.status = 'open'",
    )
    .bind(household_id)
    .fetch_all(&mut *conn)
    .await?;
    let due_today_events = events
        .iter()
        .filter(|event| {
            let starts_today = event.due_date.is_some_and(|date| date == today)
                || event
                    .due_at
                    .is_some_and(|at| at.with_timezone(&tz).date_naive() == today);
            let started = starts_today
                || event.due_at.is_some_and(|at| at < now)
                || event.due_date.is_some_and(|date| date < today);
            started
                && effective_end(
                    event.due_at,
                    event.due_date,
                    event.end_at,
                    event.end_date,
                    household_tz,
                )
                .is_some_and(|end| end > now)
        })
        .count() as i64;
    let due_today_tasks = due_today_tasks + due_today_events;

    // «Выполнено» — те же события, что попадают в разделы «Выполнено сегодня»,
    // и только по спискам задач.
    let since = completions::day_start(tz, now);
    let done = completions::for_household_since(&mut *conn, household_id, since).await?;
    let task_lists: HashSet<Uuid> = sqlx::query_scalar(
        "SELECT id FROM lists
         WHERE household_id = $1
           AND kind = 'tasks'
           AND deleted_at IS NULL
           AND NOT is_archived",
    )
    .bind(household_id)
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .collect();
    let done_today = done
        .iter()
        .filter(|completion| task_lists.contains(&completion.list_id))
        .count() as i64;

    Ok(MenuCounts {
        open_tasks,
        overdue_tasks,
        due_today_tasks,
        open_shopping,
        done_today,
    })
}

/// Открытые просроченные задачи всех списков household.
///
/// Отдельный запрос для раздела «Просрочено» повестки: календарная развёртка
/// не возвращает прошлое. Условие то же, что у счётчика просроченных в меню.
pub async fn overdue_tasks(
    conn: &mut PgConnection,
    household_id: Uuid,
    today: NaiveDate,
    now: DateTime<Utc>,
) -> Result<Vec<Item>, DomainError> {
    let rows = sqlx::query_as::<_, ItemRow>(&format!(
        "SELECT {ITEM_COLUMNS}
         FROM items i
         JOIN lists l ON l.id = i.list_id
         WHERE i.household_id = $1
           AND i.deleted_at IS NULL
           AND l.deleted_at IS NULL
           AND NOT l.is_archived
           AND l.kind = 'tasks'
           AND i.status = 'open'
           AND ({OVERDUE_TASKS_CONDITION})
         ORDER BY i.due_at NULLS LAST, i.due_date NULLS LAST, i.id",
    ))
    .bind(household_id)
    .bind(now)
    .bind(today)
    .fetch_all(&mut *conn)
    .await?;
    hydrate_many(conn, rows).await
}

/// Открытые задачи, назначенные участнику.
pub async fn open_assigned_tasks(
    conn: &mut PgConnection,
    household_id: Uuid,
    user_id: Uuid,
) -> Result<i64, DomainError> {
    let count = sqlx::query_scalar(
        "SELECT count(*)
         FROM items i
         JOIN lists l ON l.id = i.list_id
         WHERE i.household_id = $1
           AND i.assignee_user_id = $2
           AND i.status = 'open'
           AND i.deleted_at IS NULL
           AND l.kind = 'tasks'
           AND l.deleted_at IS NULL
           AND NOT l.is_archived",
    )
    .bind(household_id)
    .bind(user_id)
    .fetch_one(conn)
    .await?;

    Ok(count)
}

/// Число открытых элементов по спискам household.
pub async fn open_counts_by_list(
    conn: &mut PgConnection,
    household_id: Uuid,
) -> Result<HashMap<Uuid, i64>, DomainError> {
    let rows: Vec<(Uuid, i64)> = sqlx::query_as(
        "SELECT list_id, count(*) FROM items
         WHERE household_id = $1 AND deleted_at IS NULL AND status = 'open'
         GROUP BY list_id",
    )
    .bind(household_id)
    .fetch_all(conn)
    .await?;

    Ok(rows.into_iter().collect())
}
