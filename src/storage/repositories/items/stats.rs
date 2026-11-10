//! Счётчики для главного меню и списков.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::recurrence::parse_timezone;

use super::completions;

/// Счётчики главного меню.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MenuCounts {
    /// Открытые задачи во всех списках `tasks`.
    pub open_tasks: i64,
    /// Из них просроченные.
    pub overdue_tasks: i64,
    /// Открытые задачи со сроком на текущий день, ещё не просроченные.
    pub due_today_tasks: i64,
    /// Невыполненные позиции в `shopping` и `checklist`.
    pub open_shopping: i64,
    /// Выполнения задач сегодня по правилу раздела «Выполнено сегодня».
    pub done_today: i64,
}

/// Счётчики меню на момент `now` в часовом поясе `timezone`.
pub async fn menu_counts(
    conn: &mut PgConnection,
    household_id: Uuid,
    timezone: &str,
    now: DateTime<Utc>,
) -> Result<MenuCounts, DomainError> {
    let tz = parse_timezone(timezone)?;
    let today = now.with_timezone(&tz).date_naive();

    let (open_tasks, overdue_tasks, due_today_tasks, open_shopping): (i64, i64, i64, i64) =
        sqlx::query_as(
            "SELECT
                 count(*) FILTER (WHERE l.kind = 'tasks' AND i.status = 'open'),
                 count(*) FILTER (
                     WHERE l.kind = 'tasks' AND i.status = 'open'
                       AND (i.due_at < $2 OR i.due_date < $3)
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
        )
        .bind(household_id)
        .bind(now)
        .bind(today)
        .bind(timezone)
        .fetch_one(&mut *conn)
        .await?;

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
