//! Репозиторий элементов, жизненный цикл, журнал и поиск.

use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde_json::{Value, json};
use sqlx::{FromRow, PgConnection};
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::models::{
    Actor, EventType, Field, Item, ItemPatch, ItemPriority, ItemSource, ItemStatus, ListKind,
    MonthRule, Recurrence, RecurrenceFreq, Subtask, SubtaskInput,
};
use crate::domain::recurrence::{Due, parse_timezone};
use crate::domain::search::{SearchCursor, SearchFilters};
use crate::domain::validation::{
    MAX_NOTES, MAX_TITLE, ValidatedItem, normalize_due_for_recurrence, normalize_recurrence,
    normalize_subtasks, normalize_tags, normalize_text, normalize_text_allow_empty, normalize_unit,
    validate_quantity,
};

use super::{active_member, events, lists, subtasks, tags};

/// Окно отмены последнего действия.
pub const UNDO_WINDOW_MINUTES: i64 = 10;
/// Срок хранения мягко удалённых элементов.
pub const DELETED_RETENTION_DAYS: i64 = 30;

const ITEM_COLUMNS: &str = "i.id, i.household_id, i.list_id, i.title, i.notes, \
    i.status, i.priority, i.due_at, i.due_date, i.assignee_user_id, \
    i.created_by_user_id, i.source, i.capture_id, i.completed_at, \
    i.completed_by_user_id, i.quantity::float8 AS quantity, i.unit, i.position, \
    i.version, i.recurrence_freq, i.recurrence_interval, i.recurrence_byweekday, \
    i.recurrence_until, i.recurrence_timezone, i.recurrence_anchor_date, \
    i.recurrence_month_rule, i.created_at, i.updated_at, i.deleted_at";

#[derive(Debug, Clone, FromRow)]
struct ItemRow {
    id: Uuid,
    household_id: Uuid,
    list_id: Uuid,
    title: String,
    notes: Option<String>,
    status: ItemStatus,
    priority: ItemPriority,
    due_at: Option<DateTime<Utc>>,
    due_date: Option<NaiveDate>,
    assignee_user_id: Option<Uuid>,
    created_by_user_id: Option<Uuid>,
    source: ItemSource,
    capture_id: Option<Uuid>,
    completed_at: Option<DateTime<Utc>>,
    completed_by_user_id: Option<Uuid>,
    quantity: Option<f64>,
    unit: Option<String>,
    position: i32,
    version: i32,
    recurrence_freq: Option<RecurrenceFreq>,
    recurrence_interval: Option<i16>,
    recurrence_byweekday: Option<Vec<i16>>,
    recurrence_until: Option<NaiveDate>,
    recurrence_timezone: Option<String>,
    recurrence_anchor_date: Option<NaiveDate>,
    recurrence_month_rule: Option<MonthRule>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    deleted_at: Option<DateTime<Utc>>,
}

impl ItemRow {
    fn recurrence(&self) -> Option<Recurrence> {
        let freq = self.recurrence_freq?;
        let anchor_date = self.recurrence_anchor_date?;
        Some(Recurrence {
            freq,
            interval: self.recurrence_interval.unwrap_or(1),
            byweekday: self.recurrence_byweekday.clone().unwrap_or_default(),
            until: self.recurrence_until,
            timezone: self.recurrence_timezone.clone().unwrap_or_default(),
            anchor_date,
            month_rule: self.recurrence_month_rule,
        })
    }

    fn due(&self) -> Option<Due> {
        match (self.due_at, self.due_date) {
            (Some(at), None) => Some(Due::Timed(at)),
            (None, Some(date)) => Some(Due::AllDay(date)),
            _ => None,
        }
    }
}

/// Результат поиска.
#[derive(Debug, Clone)]
pub struct SearchHit {
    /// Элемент.
    pub item: Item,
    /// Название списка.
    pub list_name: String,
    /// Тип списка.
    pub list_kind: ListKind,
    /// Релевантность.
    pub rank: f64,
}

#[derive(FromRow)]
struct SearchRow {
    id: Uuid,
    rank: f64,
    sort_due: DateTime<Utc>,
    list_name: String,
    list_kind: ListKind,
}

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

/// Получить активный элемент.
pub async fn get(
    conn: &mut PgConnection,
    household_id: Uuid,
    item_id: Uuid,
) -> Result<Item, DomainError> {
    load_item(conn, household_id, item_id).await
}

/// Элементы списка: открытые сверху, затем выполненные.
pub async fn list_by_list(
    conn: &mut PgConnection,
    household_id: Uuid,
    list_id: Uuid,
) -> Result<Vec<Item>, DomainError> {
    lists::get_visible(conn, household_id, list_id).await?;

    let rows = sqlx::query_as::<_, ItemRow>(&format!(
        "SELECT {ITEM_COLUMNS}
         FROM items i
         WHERE i.household_id = $1 AND i.list_id = $2 AND i.deleted_at IS NULL
         ORDER BY CASE WHEN i.status = 'open' THEN 0 ELSE 1 END,
                  i.position, i.created_at, i.id"
    ))
    .bind(household_id)
    .bind(list_id)
    .fetch_all(&mut *conn)
    .await?;

    hydrate_many(conn, rows).await
}

/// Задачи со сроком или повторением для календаря.
pub async fn calendar_items(
    conn: &mut PgConnection,
    household_id: Uuid,
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
           AND (i.due_at IS NOT NULL OR i.due_date IS NOT NULL OR i.recurrence_freq IS NOT NULL)
         ORDER BY i.due_date NULLS LAST, i.due_at NULLS LAST, i.id"
    ))
    .bind(household_id)
    .fetch_all(&mut *conn)
    .await?;

    hydrate_many(conn, rows).await
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

    let events = events::for_item(conn, household_id, item_id).await?;
    if events.first().is_some_and(|event| event.undo_of.is_some()) {
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

/// Элементы для инкрементального снапшота и идентификаторы удалённых.
pub async fn snapshot(
    conn: &mut PgConnection,
    household_id: Uuid,
    since: Option<DateTime<Utc>>,
) -> Result<(Vec<Item>, Vec<Uuid>), DomainError> {
    let mut sql = format!("SELECT {ITEM_COLUMNS} FROM items i WHERE i.household_id = $1");
    match since {
        Some(_) => sql.push_str(" AND i.updated_at > $2"),
        None => sql.push_str(" AND i.deleted_at IS NULL"),
    }
    sql.push_str(" ORDER BY i.updated_at, i.id");

    let mut query = sqlx::query_as::<_, ItemRow>(&sql).bind(household_id);
    if let Some(since) = since {
        query = query.bind(since);
    }
    let rows = query.fetch_all(&mut *conn).await?;

    let mut items = Vec::new();
    let mut deleted_item_ids = Vec::new();
    for row in rows {
        if row.deleted_at.is_some() {
            if since.is_some() {
                deleted_item_ids.push(row.id);
            }
        } else {
            items.push(hydrate_one(conn, row).await?);
        }
    }

    Ok((items, deleted_item_ids))
}

/// Поиск с курсорной пагинацией.
pub async fn search(
    conn: &mut PgConnection,
    household_id: Uuid,
    household_timezone: &str,
    filters: &SearchFilters,
) -> Result<(Vec<SearchHit>, Option<String>), DomainError> {
    filters.validate()?;

    let (cursor_rank, cursor_due, cursor_id) = match &filters.cursor {
        Some(cursor) => (Some(cursor.rank), Some(cursor.due), Some(cursor.id)),
        None => (None, None, None),
    };
    let query = filters
        .q
        .as_deref()
        .map(str::trim)
        .filter(|q| !q.is_empty());
    let tag = filters
        .tag
        .as_deref()
        .map(str::trim)
        .filter(|tag| !tag.is_empty());

    let sql = "
        SELECT id, rank, sort_due, list_name, list_kind
        FROM (
            SELECT i.id AS id,
                   (
                       COALESCE(ts_rank(i.search_vector, plainto_tsquery('simple', $9)), 0)
                       + CASE WHEN $9::text IS NOT NULL
                                   AND strpos(lower(i.title), lower($9)) > 0
                              THEN 1.0 ELSE 0.0 END
                   )::float8 AS rank,
                   COALESCE(
                       i.due_at,
                       (i.due_date::timestamp AT TIME ZONE $13),
                       '9999-12-31 00:00:00+00'::timestamptz
                   ) AS sort_due,
                   l.name AS list_name,
                   l.kind AS list_kind
            FROM items i
            JOIN lists l ON l.id = i.list_id
            WHERE i.household_id = $1
              AND i.deleted_at IS NULL
              AND l.deleted_at IS NULL
              AND NOT l.is_archived
              AND ($2::bool OR i.status = $3)
              AND ($4::uuid IS NULL OR i.list_id = $4)
              AND ($5::uuid IS NULL OR i.assignee_user_id = $5)
              AND ($6::text IS NULL OR EXISTS (
                      SELECT 1 FROM item_tags it
                      JOIN tags t ON t.id = it.tag_id
                      WHERE it.item_id = i.id AND lower(t.name) = lower($6)
                  ))
              AND ($7::timestamptz IS NULL OR
                   COALESCE(i.due_at, (i.due_date::timestamp AT TIME ZONE $13)) >= $7)
              AND ($8::timestamptz IS NULL OR
                   COALESCE(i.due_at, (i.due_date::timestamp AT TIME ZONE $13)) <= $8)
              AND ($9::text IS NULL OR
                   i.search_vector @@ plainto_tsquery('simple', $9)
                   OR strpos(lower(i.title), lower($9)) > 0
                   OR strpos(lower(COALESCE(i.notes, '')), lower($9)) > 0
                   OR EXISTS (
                       SELECT 1 FROM item_tags it
                       JOIN tags t ON t.id = it.tag_id
                       WHERE it.item_id = i.id AND strpos(lower(t.name), lower($9)) > 0
                   )
                   OR EXISTS (
                       SELECT 1 FROM subtasks s
                       WHERE s.item_id = i.id AND strpos(lower(s.title), lower($9)) > 0
                   ))
        ) matched
        WHERE ($10::float8 IS NULL OR rank < $10 OR (rank = $10 AND (sort_due > $11 OR (sort_due = $11 AND id > $12))))
        ORDER BY rank DESC, sort_due ASC, id ASC
        LIMIT $14
    ";

    let rows = sqlx::query_as::<_, SearchRow>(sql)
        .bind(household_id)
        .bind(filters.all_status)
        .bind(filters.status.unwrap_or(ItemStatus::Open))
        .bind(filters.list_id)
        .bind(filters.assignee_user_id)
        .bind(tag)
        .bind(filters.due_from)
        .bind(filters.due_to)
        .bind(query)
        .bind(cursor_rank)
        .bind(cursor_due)
        .bind(cursor_id)
        .bind(household_timezone)
        .bind(filters.limit)
        .fetch_all(&mut *conn)
        .await?;

    let mut hits = Vec::with_capacity(rows.len());
    for row in &rows {
        let item = load_item(conn, household_id, row.id).await?;
        hits.push(SearchHit {
            item,
            list_name: row.list_name.clone(),
            list_kind: row.list_kind,
            rank: row.rank,
        });
    }

    let next_cursor = if i64::try_from(rows.len()).unwrap_or(0) >= filters.limit {
        rows.last()
            .map(|row| {
                crate::domain::search::encode_cursor(&SearchCursor {
                    rank: row.rank,
                    due: row.sort_due,
                    id: row.id,
                })
            })
            .transpose()?
    } else {
        None
    };

    Ok((hits, next_cursor))
}

type RecurrenceColumns = (
    Option<RecurrenceFreq>,
    Option<i16>,
    Option<Vec<i16>>,
    Option<NaiveDate>,
    Option<String>,
    Option<NaiveDate>,
    Option<MonthRule>,
);

fn recurrence_values(recurrence: Option<&Recurrence>) -> RecurrenceColumns {
    match recurrence {
        Some(rule) => (
            Some(rule.freq),
            Some(rule.interval),
            (!rule.byweekday.is_empty()).then(|| rule.byweekday.clone()),
            rule.until,
            Some(rule.timezone.clone()),
            Some(rule.anchor_date),
            rule.month_rule,
        ),
        None => (None, None, None, None, None, None, None),
    }
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

fn version_conflict(current: &Item) -> DomainError {
    DomainError::conflict_with(
        "Task was modified",
        json!({"current_version": current.version, "item": current}),
    )
}

fn local_today(timezone: &str, now: DateTime<Utc>) -> Result<NaiveDate, DomainError> {
    let timezone = parse_timezone(timezone)?;
    Ok(now.with_timezone(&timezone).date_naive())
}

async fn row_by_id(
    conn: &mut PgConnection,
    household_id: Uuid,
    item_id: Uuid,
    include_deleted: bool,
    for_update: bool,
) -> Result<ItemRow, DomainError> {
    let mut sql =
        format!("SELECT {ITEM_COLUMNS} FROM items i WHERE i.id = $1 AND i.household_id = $2");
    if !include_deleted {
        sql.push_str(" AND i.deleted_at IS NULL");
    }
    if for_update {
        sql.push_str(" FOR UPDATE");
    }

    sqlx::query_as::<_, ItemRow>(&sql)
        .bind(item_id)
        .bind(household_id)
        .fetch_optional(&mut *conn)
        .await?
        .ok_or(DomainError::NotFound)
}

async fn load_item(
    conn: &mut PgConnection,
    household_id: Uuid,
    item_id: Uuid,
) -> Result<Item, DomainError> {
    let row = row_by_id(conn, household_id, item_id, false, false).await?;
    hydrate_one(conn, row).await
}

async fn load_item_any(
    conn: &mut PgConnection,
    household_id: Uuid,
    item_id: Uuid,
) -> Result<Item, DomainError> {
    let row = row_by_id(conn, household_id, item_id, true, false).await?;
    hydrate_one(conn, row).await
}

async fn hydrate_one(conn: &mut PgConnection, row: ItemRow) -> Result<Item, DomainError> {
    let tags = tags::item_tags(conn, row.id).await?;
    let subtasks = subtasks::list(conn, row.id).await?;
    Ok(row_to_item(row, tags, subtasks))
}

async fn hydrate_many(
    conn: &mut PgConnection,
    rows: Vec<ItemRow>,
) -> Result<Vec<Item>, DomainError> {
    let mut items = Vec::with_capacity(rows.len());
    for row in rows {
        items.push(hydrate_one(conn, row).await?);
    }
    Ok(items)
}

fn row_to_item(
    row: ItemRow,
    tags: Vec<crate::domain::models::Tag>,
    subtasks: Vec<Subtask>,
) -> Item {
    let recurrence = row.recurrence();
    Item {
        id: row.id,
        household_id: row.household_id,
        list_id: row.list_id,
        title: row.title,
        notes: row.notes,
        status: row.status,
        priority: row.priority,
        due_at: row.due_at,
        due_date: row.due_date,
        assignee_user_id: row.assignee_user_id,
        created_by_user_id: row.created_by_user_id,
        source: row.source,
        capture_id: row.capture_id,
        completed_at: row.completed_at,
        completed_by_user_id: row.completed_by_user_id,
        quantity: row.quantity,
        unit: row.unit,
        position: row.position,
        version: row.version,
        recurrence,
        created_at: row.created_at,
        updated_at: row.updated_at,
        deleted_at: row.deleted_at,
        tags,
        subtasks,
    }
}
