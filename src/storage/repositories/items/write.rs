//! Создание и изменение элементов.

use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use super::row::{hydrate_one, load_item, row_by_id};
use super::{local_today, recurrence_values, version_conflict};
use crate::domain::error::DomainError;
use crate::domain::models::{Actor, EventType, Field, Item, ItemPatch, ListKind, SubtaskInput};
use crate::domain::recurrence::parse_timezone;
use crate::domain::validation::{
    MAX_NOTES, MAX_TITLE, ValidatedItem, normalize_due_for_recurrence, normalize_recurrence,
    normalize_subtasks, normalize_tags, normalize_text, normalize_text_allow_empty, normalize_unit,
    validate_quantity,
};
use crate::storage::repositories::{active_member, events, lists, subtasks, tags};

/// Создать элемент.
pub async fn create(
    conn: &mut PgConnection,
    household_id: Uuid,
    actor: &Actor,
    list_id: Uuid,
    item: ValidatedItem,
    assignee_user_id: Option<Uuid>,
    now: DateTime<Utc>,
) -> Result<Item, DomainError> {
    let list = lists::get_visible(conn, household_id, list_id).await?;
    ensure_assignee(conn, household_id, assignee_user_id).await?;

    let ValidatedItem {
        title,
        notes,
        priority,
        due_at,
        due_date,
        quantity,
        unit,
        tags: tag_names,
        subtasks: subtask_inputs,
        recurrence,
        source,
    } = item;

    let position: i32 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(position), 0) + 1
         FROM items WHERE list_id = $1 AND household_id = $2",
    )
    .bind(list.id)
    .bind(household_id)
    .fetch_one(&mut *conn)
    .await?;

    let (
        recurrence_freq,
        recurrence_interval,
        recurrence_byweekday,
        recurrence_until,
        recurrence_timezone,
        recurrence_anchor_date,
        recurrence_month_rule,
    ) = recurrence_values(recurrence.as_ref());

    let item_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO items (
             id, household_id, list_id, title, notes, status, priority, due_at, due_date,
             assignee_user_id, created_by_user_id, source, quantity, unit, position,
             recurrence_freq, recurrence_interval, recurrence_byweekday, recurrence_until,
             recurrence_timezone, recurrence_anchor_date, recurrence_month_rule,
             created_at, updated_at
         )
         VALUES (
             $1, $2, $3, $4, $5, 'open', $6, $7, $8, $9, $10, $11,
             $12::float8::numeric, $13, $14, $15, $16, $17, $18, $19, $20, $21, $22, $22
         )",
    )
    .bind(item_id)
    .bind(household_id)
    .bind(list.id)
    .bind(&title)
    .bind(notes)
    .bind(priority)
    .bind(due_at)
    .bind(due_date)
    .bind(assignee_user_id)
    .bind(actor.user_id)
    .bind(source)
    .bind(quantity)
    .bind(unit)
    .bind(position)
    .bind(recurrence_freq)
    .bind(recurrence_interval)
    .bind(recurrence_byweekday)
    .bind(recurrence_until)
    .bind(recurrence_timezone)
    .bind(recurrence_anchor_date)
    .bind(recurrence_month_rule)
    .bind(now)
    .execute(&mut *conn)
    .await?;

    tags::set_item_tags(conn, household_id, item_id, &tag_names).await?;
    subtasks::replace(conn, item_id, &subtask_inputs, now).await?;

    let item = load_item(conn, household_id, item_id).await?;
    events::insert(
        conn,
        household_id,
        item_id,
        actor,
        EventType::Created,
        json!({"before": Value::Null, "after": item}),
        None,
        now,
    )
    .await?;

    Ok(item)
}

/// Изменить элемент с проверкой версии.
#[allow(clippy::too_many_arguments)]
pub async fn update(
    conn: &mut PgConnection,
    household_id: Uuid,
    actor: &Actor,
    item_id: Uuid,
    version: i32,
    patch: ItemPatch,
    household_timezone: &str,
    now: DateTime<Utc>,
) -> Result<Item, DomainError> {
    let row = row_by_id(conn, household_id, item_id, false, true).await?;
    let current = hydrate_one(conn, row.clone()).await?;
    if row.version != version {
        return Err(version_conflict(&current));
    }

    let current_list = lists::get(conn, household_id, row.list_id).await?;
    if current_list.is_archived {
        return Err(DomainError::NotFound);
    }

    let target_list = match patch.list_id {
        Some(target_id) if target_id != row.list_id => {
            let target = lists::get_visible(conn, household_id, target_id).await?;
            if target.kind != current_list.kind {
                return Err(DomainError::Validation(
                    "cannot move an item to a list of another kind".into(),
                ));
            }
            target
        }
        _ => current_list.clone(),
    };

    let title = match patch.title.as_deref() {
        Some(title) => normalize_text(title, MAX_TITLE, "title")?,
        None => row.title.clone(),
    };

    let notes = match patch.notes {
        Field::Missing => row.notes.clone(),
        Field::Null => None,
        Field::Value(notes) => Some(normalize_text_allow_empty(&notes, MAX_NOTES, "notes")?),
    };

    let priority = patch.priority.unwrap_or(row.priority);

    let mut due_at = row.due_at;
    let mut due_date = row.due_date;
    if patch.due_at.is_set() && patch.due_date.is_set() {
        return Err(DomainError::Validation(
            "due_at and due_date are mutually exclusive".into(),
        ));
    }
    let mut due_changed = false;
    match patch.due_at {
        Field::Missing => {}
        Field::Null => {
            due_at = None;
            due_changed = true;
        }
        Field::Value(value) => {
            due_at = Some(value);
            due_date = None;
            due_changed = true;
        }
    }
    match patch.due_date {
        Field::Missing => {}
        Field::Null => {
            due_date = None;
            due_changed = true;
        }
        Field::Value(value) => {
            due_date = Some(value);
            due_at = None;
            due_changed = true;
        }
    }

    let old_recurrence = row.recurrence();
    let mut recurrence = old_recurrence.clone();
    let today = local_today(household_timezone, now)?;

    match patch.recurrence {
        Field::Missing => {
            if due_changed && let Some(rule) = recurrence.as_mut() {
                if let Some(at) = due_at {
                    let timezone = parse_timezone(&rule.timezone)?;
                    rule.anchor_date = at.with_timezone(&timezone).date_naive();
                } else if let Some(date) = due_date {
                    rule.anchor_date = date;
                }
            }
        }
        Field::Null => recurrence = None,
        Field::Value(input) => {
            if target_list.kind != ListKind::Tasks {
                return Err(DomainError::Validation(
                    "recurrence is only allowed for task lists".into(),
                ));
            }
            recurrence = Some(normalize_recurrence(
                input,
                household_timezone,
                due_at,
                due_date,
                today,
            )?);
        }
    }

    (due_at, due_date) = normalize_due_for_recurrence(
        due_at,
        due_date,
        recurrence.as_ref(),
        household_timezone,
        today,
    )?;

    let (quantity, unit) = if target_list.kind == ListKind::Tasks {
        (None, None)
    } else {
        let quantity = match patch.quantity {
            Field::Missing => row.quantity.unwrap_or(1.0),
            Field::Null => 1.0,
            Field::Value(value) => value,
        };
        let quantity = validate_quantity(Some(quantity))?;
        let unit = match patch.unit {
            Field::Missing => row.unit.clone().unwrap_or_else(|| "шт".into()),
            Field::Null => "шт".into(),
            Field::Value(value) => normalize_unit(Some(value)),
        };
        (Some(quantity), Some(unit))
    };

    let assignee_user_id = match patch.assignee_user_id {
        Field::Missing => row.assignee_user_id,
        Field::Null => None,
        Field::Value(value) => Some(value),
    };
    ensure_assignee(conn, household_id, assignee_user_id).await?;

    let tag_names = match patch.tags {
        Some(names) => normalize_tags(names)?,
        None => current.tags.iter().map(|tag| tag.name.clone()).collect(),
    };
    let subtask_inputs = match patch.subtasks {
        Some(inputs) => normalize_subtasks(inputs)?,
        None => current
            .subtasks
            .iter()
            .map(|subtask| SubtaskInput {
                title: subtask.title.clone(),
                status: Some(subtask.status),
            })
            .collect(),
    };

    let (
        recurrence_freq,
        recurrence_interval,
        recurrence_byweekday,
        recurrence_until,
        recurrence_timezone,
        recurrence_anchor_date,
        recurrence_month_rule,
    ) = recurrence_values(recurrence.as_ref());

    sqlx::query(
        "UPDATE items
         SET list_id = $3, title = $4, notes = $5, priority = $6,
             due_at = $7, due_date = $8, assignee_user_id = $9,
             quantity = $10::float8::numeric, unit = $11,
             recurrence_freq = $12, recurrence_interval = $13,
             recurrence_byweekday = $14, recurrence_until = $15,
             recurrence_timezone = $16, recurrence_anchor_date = $17,
             recurrence_month_rule = $18,
             version = version + 1, updated_at = $19
         WHERE id = $1 AND household_id = $2 AND deleted_at IS NULL",
    )
    .bind(item_id)
    .bind(household_id)
    .bind(target_list.id)
    .bind(&title)
    .bind(&notes)
    .bind(priority)
    .bind(due_at)
    .bind(due_date)
    .bind(assignee_user_id)
    .bind(quantity)
    .bind(&unit)
    .bind(recurrence_freq)
    .bind(recurrence_interval)
    .bind(recurrence_byweekday)
    .bind(recurrence_until)
    .bind(recurrence_timezone)
    .bind(recurrence_anchor_date)
    .bind(recurrence_month_rule)
    .bind(now)
    .execute(&mut *conn)
    .await?;

    tags::set_item_tags(conn, household_id, item_id, &tag_names).await?;
    subtasks::replace(conn, item_id, &subtask_inputs, now).await?;

    let updated = load_item(conn, household_id, item_id).await?;
    let event_type = if recurrence != old_recurrence {
        EventType::RecurrenceChanged
    } else if assignee_user_id != row.assignee_user_id {
        if assignee_user_id.is_some() {
            EventType::Assigned
        } else {
            EventType::Unassigned
        }
    } else {
        EventType::Updated
    };

    events::insert(
        conn,
        household_id,
        item_id,
        actor,
        event_type,
        json!({"before": current, "after": updated}),
        None,
        now,
    )
    .await?;

    Ok(updated)
}

async fn ensure_assignee(
    conn: &mut PgConnection,
    household_id: Uuid,
    assignee_user_id: Option<Uuid>,
) -> Result<(), DomainError> {
    if let Some(user_id) = assignee_user_id
        && !active_member(conn, household_id, user_id).await?
    {
        return Err(DomainError::Validation(
            "assignee must be an active member of the household".into(),
        ));
    }
    Ok(())
}
