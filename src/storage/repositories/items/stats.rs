//! Счётчики для главного меню и списков.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::recurrence::parse_timezone;

/// Счётчики главного меню.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MenuCounts {
    /// Открытые задачи во всех списках `tasks`.
    pub open_tasks: i64,
    /// Из них просроченные.
    pub overdue_tasks: i64,
    /// Невыполненные позиции в `shopping` и `checklist`.
    pub open_shopping: i64,
    /// Завершённые сегодня (по локальной дате) задачи и позиции.
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

    let (open_tasks, overdue_tasks, open_shopping, done_today): (i64, i64, i64, i64) =
        sqlx::query_as(
            "SELECT
                 count(*) FILTER (WHERE l.kind = 'tasks' AND i.status = 'open'),
                 count(*) FILTER (
                     WHERE l.kind = 'tasks' AND i.status = 'open'
                       AND (i.due_at < $2 OR i.due_date < $3)
                 ),
                 count(*) FILTER (WHERE l.kind <> 'tasks' AND i.status = 'open'),
                 count(*) FILTER (
                     WHERE i.status = 'done'
                       AND (i.completed_at AT TIME ZONE $4)::date = $3
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
        .fetch_one(conn)
        .await?;

    Ok(MenuCounts {
        open_tasks,
        overdue_tasks,
        open_shopping,
        done_today,
    })
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
