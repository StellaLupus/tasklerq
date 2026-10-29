//! Жизненный цикл элемента: завершение, повторное открытие, удаление,
//! восстановление и отмена последнего действия.

use chrono::{DateTime, Duration, Utc};
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use super::row::{hydrate_one, load_item, load_item_any, row_by_id};
use super::{
    DELETED_RETENTION_DAYS, UNDO_WINDOW_MINUTES, local_today, recurrence_values, version_conflict,
};
use crate::domain::error::DomainError;
use crate::domain::models::{Actor, EventType, Item, ItemStatus};
use crate::domain::recurrence::Due;
use crate::storage::repositories::{events, lists, subtasks, tags};

/// Завершить элемент.
pub async fn complete(
    conn: &mut PgConnection,
    household_id: Uuid,
    actor: &Actor,
    item_id: Uuid,
    version: Option<i32>,
    household_timezone: &str,
    now: DateTime<Utc>,
) -> Result<Item, DomainError> {
    let row = row_by_id(conn, household_id, item_id, false, true).await?;
    let current = hydrate_one(conn, row.clone()).await?;
    if let Some(version) = version
        && version != row.version
    {
        return Err(version_conflict(&current));
    }

    if row.status == ItemStatus::Done {
        return Ok(current);
    }

    let today = local_today(household_timezone, now)?;
    let recurrence = row.recurrence();

    if let Some(rule) = recurrence.as_ref() {
        let current_due = match row.due() {
            Some(due) => due,
            None => rule.first_due(today)?,
        };
        let future = match current_due {
            Due::Timed(at) => at > now,
            Due::AllDay(date) => date > today,
        };
        if row.completed_at.is_some() && future {
            // Повторный вызов после переноса срока — идемпотентен.
            return Ok(current);
        }

        match rule.next_after(current_due, now)? {
            Some(next) => {
                let (due_at, due_date) = match next {
                    Due::Timed(at) => (Some(at), None),
                    Due::AllDay(date) => (None, Some(date)),
                };
                sqlx::query(
                    "UPDATE items
                     SET status = 'open', completed_at = $3, completed_by_user_id = $4,
                         due_at = $5, due_date = $6, version = version + 1, updated_at = $3
                     WHERE id = $1 AND household_id = $2",
                )
                .bind(item_id)
                .bind(household_id)
                .bind(now)
                .bind(actor.user_id)
                .bind(due_at)
                .bind(due_date)
                .execute(&mut *conn)
                .await?;
            }
            None => {
                sqlx::query(
                    "UPDATE items
                     SET status = 'done', completed_at = $3, completed_by_user_id = $4,
                         version = version + 1, updated_at = $3
                     WHERE id = $1 AND household_id = $2",
                )
                .bind(item_id)
                .bind(household_id)
                .bind(now)
                .bind(actor.user_id)
                .execute(&mut *conn)
                .await?;
            }
        }
    } else {
        sqlx::query(
            "UPDATE items
             SET status = 'done', completed_at = $3, completed_by_user_id = $4,
                 version = version + 1, updated_at = $3
             WHERE id = $1 AND household_id = $2",
        )
        .bind(item_id)
        .bind(household_id)
        .bind(now)
        .bind(actor.user_id)
        .execute(&mut *conn)
        .await?;
    }

    let updated = load_item(conn, household_id, item_id).await?;
    events::insert(
        conn,
        household_id,
        item_id,
        actor,
        EventType::Completed,
        json!({"before": current, "after": updated}),
        None,
        now,
    )
    .await?;

    Ok(updated)
}

/// Открыть завершённый элемент и восстановить прежний срок.
pub async fn reopen(
    conn: &mut PgConnection,
    household_id: Uuid,
    actor: &Actor,
    item_id: Uuid,
    version: Option<i32>,
    now: DateTime<Utc>,
) -> Result<Item, DomainError> {
    let row = row_by_id(conn, household_id, item_id, false, true).await?;
    let current = hydrate_one(conn, row.clone()).await?;
    if let Some(version) = version
        && version != row.version
    {
        return Err(version_conflict(&current));
    }
    if row.status == ItemStatus::Open {
        return Ok(current);
    }

    let events = events::for_item(conn, household_id, item_id).await?;
    let previous = events
        .iter()
        .find(|event| event.event_type == EventType::Completed && event.undo_of.is_none())
        .and_then(|event| {
            serde_json::from_value::<Item>(event.payload.get("before")?.clone()).ok()
        });

    let (due_at, due_date, completed_at, completed_by_user_id) = match previous {
        Some(previous) => (
            previous.due_at,
            previous.due_date,
            previous.completed_at,
            previous.completed_by_user_id,
        ),
        None => (row.due_at, row.due_date, None, None),
    };

    sqlx::query(
        "UPDATE items
         SET status = 'open', due_at = $3, due_date = $4, completed_at = $5,
             completed_by_user_id = $6, version = version + 1, updated_at = $7
         WHERE id = $1 AND household_id = $2",
    )
    .bind(item_id)
    .bind(household_id)
    .bind(due_at)
    .bind(due_date)
    .bind(completed_at)
    .bind(completed_by_user_id)
    .bind(now)
    .execute(&mut *conn)
    .await?;

    let updated = load_item(conn, household_id, item_id).await?;
    events::insert(
        conn,
        household_id,
        item_id,
        actor,
        EventType::Reopened,
        json!({"before": current, "after": updated}),
        None,
        now,
    )
    .await?;

    Ok(updated)
}

/// Мягко удалить элемент.
pub async fn soft_delete(
    conn: &mut PgConnection,
    household_id: Uuid,
    actor: &Actor,
    item_id: Uuid,
    now: DateTime<Utc>,
) -> Result<Item, DomainError> {
    let row = row_by_id(conn, household_id, item_id, true, true).await?;
    let current = hydrate_one(conn, row.clone()).await?;

    if row.deleted_at.is_some() {
        return Ok(current);
    }
    ensure_can_modify(actor, row.created_by_user_id)?;

    sqlx::query(
        "UPDATE items
         SET deleted_at = $3, version = version + 1, updated_at = $3
         WHERE id = $1 AND household_id = $2",
    )
    .bind(item_id)
    .bind(household_id)
    .bind(now)
    .execute(&mut *conn)
    .await?;

    let updated = load_item_any(conn, household_id, item_id).await?;
    events::insert(
        conn,
        household_id,
        item_id,
        actor,
        EventType::Deleted,
        json!({"before": current, "after": updated}),
        None,
        now,
    )
    .await?;

    Ok(updated)
}

/// Восстановить мягко удалённый элемент.
pub async fn restore(
    conn: &mut PgConnection,
    household_id: Uuid,
    actor: &Actor,
    item_id: Uuid,
    now: DateTime<Utc>,
) -> Result<Item, DomainError> {
    let row = row_by_id(conn, household_id, item_id, true, true).await?;
    let current = hydrate_one(conn, row.clone()).await?;

    let Some(deleted_at) = row.deleted_at else {
        return Ok(current);
    };
    ensure_can_modify(actor, row.created_by_user_id)?;

    if now - deleted_at > Duration::days(DELETED_RETENTION_DAYS) {
        return Err(DomainError::conflict(
            "item can no longer be restored after the retention period",
        ));
    }

    if lists::get(conn, household_id, row.list_id).await.is_err() {
        return Err(DomainError::conflict(
            "item cannot be restored because its list was deleted",
        ));
    }

    sqlx::query(
        "UPDATE items
         SET deleted_at = NULL, version = version + 1, updated_at = $3
         WHERE id = $1 AND household_id = $2",
    )
    .bind(item_id)
    .bind(household_id)
    .bind(now)
    .execute(&mut *conn)
    .await?;

    let updated = load_item(conn, household_id, item_id).await?;
    events::insert(
        conn,
        household_id,
        item_id,
        actor,
        EventType::Restored,
        json!({"before": current, "after": updated}),
        None,
        now,
    )
    .await?;

    Ok(updated)
}

/// Отменить последнее действие над элементом в течение 10 минут.
pub async fn undo_last(
    conn: &mut PgConnection,
    household_id: Uuid,
    actor: &Actor,
    item_id: Uuid,
    now: DateTime<Utc>,
) -> Result<Item, DomainError> {
    let row = row_by_id(conn, household_id, item_id, true, true).await?;
    let current = hydrate_one(conn, row).await?;

    if events::latest(conn, household_id, item_id)
        .await?
        .is_some_and(|event| event.undo_of.is_some())
    {
        // Компенсирующее событие уже применено: повторная отмена не должна
        // откатывать более раннее действие.
        return Err(DomainError::conflict("there is no action to undo"));
    }

    let event = events::latest_undoable(conn, household_id, item_id)
        .await?
        .ok_or_else(|| DomainError::conflict("there is no action to undo"))?;

    if now - event.created_at > Duration::minutes(UNDO_WINDOW_MINUTES) {
        return Err(DomainError::conflict_with(
            "the undo window has expired",
            json!({"event_id": event.id, "created_at": event.created_at}),
        ));
    }
    ensure_can_undo(actor, &event)?;

    // Откатывать можно только актуальное событие. Если элемент менялся после
    // него — в том числе удалением списка, которое поднимает version, не
    // пишя событий, — отмена воскресила бы устаревшее состояние.
    let event_version = event
        .payload
        .get("after")
        .and_then(|after| after.get("version"))
        .and_then(Value::as_i64);
    if event_version != Some(i64::from(current.version)) {
        return Err(DomainError::conflict(
            "the item was modified after the action; there is nothing to undo",
        ));
    }

    let before = event.payload.get("before").cloned();
    if !matches!(&before, Some(Value::Object(_))) {
        // Отмена создания — мягкое удаление.
        sqlx::query(
            "UPDATE items
             SET deleted_at = $3, version = version + 1, updated_at = $3
             WHERE id = $1 AND household_id = $2",
        )
        .bind(item_id)
        .bind(household_id)
        .bind(now)
        .execute(&mut *conn)
        .await?;

        let updated = load_item_any(conn, household_id, item_id).await?;
        events::insert(
            conn,
            household_id,
            item_id,
            actor,
            EventType::Deleted,
            json!({"before": current, "after": updated}),
            Some(event.id),
            now,
        )
        .await?;
        return Ok(updated);
    }

    let before: Item = serde_json::from_value(before.expect("checked object"))
        .map_err(|_| DomainError::conflict("stored undo snapshot is invalid"))?;

    let (
        recurrence_freq,
        recurrence_interval,
        recurrence_byweekday,
        recurrence_until,
        recurrence_timezone,
        recurrence_anchor_date,
        recurrence_month_rule,
    ) = recurrence_values(before.recurrence.as_ref());

    sqlx::query(
        "UPDATE items
         SET list_id = $3, title = $4, notes = $5, status = $6, priority = $7,
             due_at = $8, due_date = $9, assignee_user_id = $10,
             created_by_user_id = $11, source = $12, capture_id = $13,
             completed_at = $14, completed_by_user_id = $15,
             quantity = $16::float8::numeric, unit = $17, position = $18,
             recurrence_freq = $19, recurrence_interval = $20,
             recurrence_byweekday = $21, recurrence_until = $22,
             recurrence_timezone = $23, recurrence_anchor_date = $24,
             recurrence_month_rule = $25, deleted_at = $26,
             version = $27, updated_at = $28
         WHERE id = $1 AND household_id = $2",
    )
    .bind(item_id)
    .bind(household_id)
    .bind(before.list_id)
    .bind(&before.title)
    .bind(&before.notes)
    .bind(before.status)
    .bind(before.priority)
    .bind(before.due_at)
    .bind(before.due_date)
    .bind(before.assignee_user_id)
    .bind(before.created_by_user_id)
    .bind(before.source)
    .bind(before.capture_id)
    .bind(before.completed_at)
    .bind(before.completed_by_user_id)
    .bind(before.quantity)
    .bind(&before.unit)
    .bind(before.position)
    .bind(recurrence_freq)
    .bind(recurrence_interval)
    .bind(recurrence_byweekday)
    .bind(recurrence_until)
    .bind(recurrence_timezone)
    .bind(recurrence_anchor_date)
    .bind(recurrence_month_rule)
    .bind(before.deleted_at)
    .bind(current.version + 1)
    .bind(now)
    .execute(&mut *conn)
    .await?;

    let tag_names: Vec<String> = before.tags.iter().map(|tag| tag.name.clone()).collect();
    tags::set_item_tags(conn, household_id, item_id, &tag_names).await?;
    subtasks::replace_snapshot(conn, item_id, &before.subtasks).await?;

    let updated = load_item_any(conn, household_id, item_id).await?;
    let event_type = undo_event_type(&current, &updated);
    events::insert(
        conn,
        household_id,
        item_id,
        actor,
        event_type,
        json!({"before": current, "after": updated}),
        Some(event.id),
        now,
    )
    .await?;

    Ok(updated)
}

fn ensure_can_modify(actor: &Actor, created_by_user_id: Option<Uuid>) -> Result<(), DomainError> {
    if actor.user_id.is_none() || actor.is_owner() {
        return Ok(());
    }
    match created_by_user_id {
        Some(author) if actor.user_id != Some(author) => Err(DomainError::Forbidden(
            "only the author or the household owner can perform this action".into(),
        )),
        _ => Ok(()),
    }
}

fn ensure_can_undo(
    actor: &Actor,
    event: &crate::domain::models::ItemEvent,
) -> Result<(), DomainError> {
    if actor.user_id.is_none() || actor.is_owner() || event.actor_user_id.is_none() {
        return Ok(());
    }
    if actor.user_id == event.actor_user_id {
        Ok(())
    } else {
        Err(DomainError::Forbidden(
            "only the actor or the household owner can undo this action".into(),
        ))
    }
}

fn undo_event_type(current: &Item, restored: &Item) -> EventType {
    if restored.deleted_at.is_some() {
        EventType::Deleted
    } else if restored.status == ItemStatus::Done {
        EventType::Completed
    } else if current.status == ItemStatus::Done {
        EventType::Reopened
    } else {
        EventType::Updated
    }
}
