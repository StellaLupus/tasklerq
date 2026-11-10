//! Создание и изменение элементов.

use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use super::row::{hydrate_one, load_item, row_by_id};
use super::{local_today, recurrence_values, version_conflict};
use crate::domain::error::DomainError;
use crate::domain::models::{
    Actor, EventType, Field, Item, ItemPatch, ItemStatus, ListKind, SubtaskInput,
};
use crate::domain::recurrence::{Due, parse_timezone, resolve_local, rule_changed};
use crate::domain::validation::{
    MAX_NOTES, MAX_TITLE, ValidatedItem, normalize_due_for_recurrence, normalize_recurrence,
    normalize_subtasks, normalize_tags, normalize_text, normalize_text_allow_empty, normalize_unit,
    validate_fields_for_kind, validate_quantity,
};
use crate::storage::repositories::{active_member, events, lists, notifications, subtasks, tags};

/// Как внутренний `update` относится к серии при смене срока.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DueChange {
    /// Срок сдвигает якорь серии (обычный `update`).
    Reanchor,
    /// Срок меняется без переякорения (перенос текущего вхождения).
    KeepSeries,
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
        end_at,
        end_date,
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
        recurrence_occurrence_date,
        recurrence_occurrence_time,
    ) = recurrence_values(recurrence.as_ref());

    let item_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO items (
             id, household_id, list_id, title, notes, status, priority, due_at, due_date,
             end_at, end_date, assignee_user_id, created_by_user_id, source, quantity, unit,
             position, recurrence_freq, recurrence_interval, recurrence_byweekday,
             recurrence_until, recurrence_timezone, recurrence_anchor_date,
             recurrence_month_rule, recurrence_occurrence_date, recurrence_occurrence_time,
             created_at, updated_at
         )
         VALUES (
             $1, $2, $3, $4, $5, 'open', $6, $7, $8, $9, $10, $11, $12, $13,
             $14::float8::numeric, $15, $16, $17, $18, $19, $20, $21, $22, $23, $24,
             $25, $26, $26
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
    .bind(end_at)
    .bind(end_date)
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
    .bind(recurrence_occurrence_date)
    .bind(recurrence_occurrence_time)
    .bind(now)
    .execute(&mut *conn)
    .await?;

    tags::set_item_tags(conn, household_id, item_id, &tag_names).await?;
    subtasks::replace(conn, item_id, &subtask_inputs, now).await?;

    let item = load_item(conn, household_id, item_id).await?;
    let event = events::insert(
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
    notifications::notify_assignment(
        conn,
        &notifications::AssignmentNotice {
            household_id,
            item_id,
            actor,
            previous_assignee: None,
            assignee: item.assignee_user_id,
            event_id: event.id,
        },
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
    update_inner(
        conn,
        household_id,
        actor,
        item_id,
        version,
        patch,
        household_timezone,
        now,
        DueChange::Reanchor,
    )
    .await
}

/// Перенести срок текущего вхождения, не меняя якорь серии.
#[allow(clippy::too_many_arguments)]
pub async fn reschedule_occurrence(
    conn: &mut PgConnection,
    household_id: Uuid,
    actor: &Actor,
    item_id: Uuid,
    version: i32,
    due: Due,
    household_timezone: &str,
    now: DateTime<Utc>,
) -> Result<Item, DomainError> {
    let patch = match due {
        Due::Timed(at) => ItemPatch {
            due_at: Field::Value(at),
            ..ItemPatch::default()
        },
        Due::AllDay(date) => ItemPatch {
            due_date: Field::Value(date),
            ..ItemPatch::default()
        },
    };
    update_inner(
        conn,
        household_id,
        actor,
        item_id,
        version,
        patch,
        household_timezone,
        now,
        DueChange::KeepSeries,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn update_inner(
    conn: &mut PgConnection,
    household_id: Uuid,
    actor: &Actor,
    item_id: Uuid,
    version: i32,
    patch: ItemPatch,
    household_timezone: &str,
    now: DateTime<Utc>,
    due_change: DueChange,
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
    match patch.due_at {
        Field::Missing => {}
        Field::Null => {
            due_at = None;
        }
        Field::Value(value) => {
            due_at = Some(value);
            due_date = None;
        }
    }
    match patch.due_date {
        Field::Missing => {}
        Field::Null => {
            due_date = None;
        }
        Field::Value(value) => {
            due_date = Some(value);
            due_at = None;
        }
    }
    // Срок изменён, только если новые значения отличаются от текущих. Повтор
    // того же срока не должен переякоривать серию и терять слот вхождения.
    let due_changed = due_at != row.due_at || due_date != row.due_date;

    let mut end_at = row.end_at;
    let mut end_date = row.end_date;
    if patch.end_at.is_set() && patch.end_date.is_set() {
        return Err(DomainError::Validation(
            "end_at and end_date are mutually exclusive".into(),
        ));
    }
    match patch.end_at {
        Field::Missing => {}
        Field::Null => {
            end_at = None;
        }
        Field::Value(value) => {
            end_at = Some(value);
            end_date = None;
        }
    }
    match patch.end_date {
        Field::Missing => {}
        Field::Null => {
            end_date = None;
        }
        Field::Value(value) => {
            end_date = Some(value);
            end_at = None;
        }
    }
    // 0009-events 3.3: перенос начала события без конца в том же патче
    // сохраняет длительность — конец сдвигается на ту же величину. Явно
    // переданный конец (значение или очистка) применяется как есть. Смена вида
    // начала не сдвигает конец другого вида: её отклонит проверка полей.
    if target_list.kind == ListKind::Events
        && due_changed
        && patch.end_at.is_missing()
        && patch.end_date.is_missing()
    {
        if let (Some(old_start), Some(new_start)) = (row.due_at, due_at) {
            end_at = row.end_at.map(|old_end| old_end + (new_start - old_start));
        }
        if let (Some(old_start), Some(new_start)) = (row.due_date, due_date) {
            end_date = row
                .end_date
                .map(|old_end| old_end + (new_start - old_start));
        }
    }

    let old_recurrence = row.recurrence();
    let mut recurrence = old_recurrence.clone();
    let today = local_today(household_timezone, now)?;

    match patch.recurrence {
        Field::Missing => {
            if due_change == DueChange::Reanchor
                && due_changed
                && let Some(rule) = recurrence.as_mut()
            {
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
            // 0009-events 5.1: повторение станет доступно и спискам событий.
            if target_list.kind != ListKind::Tasks {
                return Err(DomainError::Validation(
                    "recurrence is only allowed for task lists".into(),
                ));
            }
            let mut normalized =
                normalize_recurrence(input, household_timezone, due_at, due_date, today)?;
            // Повтор правила из ответа API отличается только слотом. Сохраняем
            // его, чтобы такой `PATCH` оставался no-op.
            if let Some(old) = &old_recurrence
                && normalized.same_rule(old)
            {
                normalized.occurrence_date = old.occurrence_date;
                normalized.occurrence_time = old.occurrence_time;
            }
            recurrence = Some(normalized);
        }
    }

    let recurrence_changed = rule_changed(recurrence.as_ref(), old_recurrence.as_ref());

    match due_change {
        DueChange::Reanchor => {
            // Нормализуем срок по правилу, только если он реально менялся или
            // изменилось само правило. Иначе правка другого поля (например,
            // заметки) откатывала бы перенос вхождения `last_day`-серии к
            // последнему дню месяца. Срок без даты по-прежнему заполняется
            // первым вхождением.
            let due_missing = due_at.is_none() && due_date.is_none();
            if due_changed || recurrence_changed || due_missing {
                (due_at, due_date) = normalize_due_for_recurrence(
                    due_at,
                    due_date,
                    recurrence.as_ref(),
                    household_timezone,
                    today,
                )?;
            }
            if (due_changed || recurrence_changed)
                && let Some(rule) = recurrence.as_mut()
            {
                rule.occurrence_date = None;
                rule.occurrence_time = None;
            }
        }
        DueChange::KeepSeries => {
            if let Some(rule) = recurrence.as_mut() {
                let timezone = parse_timezone(&rule.timezone)?;
                let new_due = match (due_at, due_date) {
                    (Some(at), None) => Some(Due::Timed(at)),
                    (None, Some(date)) => Some(Due::AllDay(date)),
                    _ => None,
                };
                if let Some(new_due) = new_due {
                    // Слот хранит исходный срок вхождения: дату правила и время
                    // суток серии. При первом переносе он берётся из текущего
                    // срока, при повторном — не меняется.
                    let current_due = row.due();
                    let slot_date = rule
                        .occurrence_date
                        .or_else(|| current_due.map(|due| due.local_date(timezone)));
                    let slot_time = if rule.occurrence_date.is_some() {
                        rule.occurrence_time
                    } else {
                        current_due.and_then(|due| due.local_time(timezone))
                    };
                    let slot_due = match slot_date {
                        Some(date) => match slot_time {
                            Some(time) => Due::Timed(resolve_local(timezone, date, time)?),
                            None => Due::AllDay(date),
                        },
                        None => new_due,
                    };
                    let same = match (new_due, slot_due) {
                        (Due::AllDay(new), Due::AllDay(slot)) => new == slot,
                        (Due::Timed(new), Due::Timed(slot)) => {
                            let new = new.with_timezone(&timezone);
                            let slot = slot.with_timezone(&timezone);
                            new.date_naive() == slot.date_naive() && new.time() == slot.time()
                        }
                        _ => false,
                    };
                    if same {
                        rule.occurrence_date = None;
                        rule.occurrence_time = None;
                    } else {
                        rule.occurrence_date = Some(slot_due.local_date(timezone));
                        rule.occurrence_time = slot_due.local_time(timezone);
                    }
                }
            }
        }
    }

    // У события нет полей покупки: количество и единица не поддерживаются.
    let quantity_supplied = matches!(&patch.quantity, Field::Value(_));
    let unit_supplied = matches!(&patch.unit, Field::Value(_));
    let (quantity, unit) = match target_list.kind {
        ListKind::Tasks | ListKind::Events => (None, None),
        _ => {
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
        }
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

    validate_fields_for_kind(
        target_list.kind,
        due_at,
        due_date,
        end_at,
        end_date,
        priority,
        quantity_supplied,
        unit_supplied,
        !subtask_inputs.is_empty(),
    )?;

    let (
        recurrence_freq,
        recurrence_interval,
        recurrence_byweekday,
        recurrence_until,
        recurrence_timezone,
        recurrence_anchor_date,
        recurrence_month_rule,
        recurrence_occurrence_date,
        recurrence_occurrence_time,
    ) = recurrence_values(recurrence.as_ref());

    let tags_unchanged = tag_names
        == current
            .tags
            .iter()
            .map(|tag| tag.name.clone())
            .collect::<Vec<_>>();
    let subtasks_unchanged = subtask_inputs.len() == current.subtasks.len()
        && subtask_inputs
            .iter()
            .zip(&current.subtasks)
            .all(|(input, subtask)| {
                input.title == subtask.title
                    && input.status.unwrap_or(ItemStatus::Open) == subtask.status
            });
    let unchanged = target_list.id == current.list_id
        && title == current.title
        && notes == current.notes
        && priority == current.priority
        && due_at == current.due_at
        && due_date == current.due_date
        && end_at == current.end_at
        && end_date == current.end_date
        && assignee_user_id == current.assignee_user_id
        && quantity == current.quantity
        && unit == current.unit
        && recurrence == current.recurrence
        && tags_unchanged
        && subtasks_unchanged;
    if unchanged {
        // Повторный PATCH без изменений не двигает версию и не пишет событие:
        // «состояние задачи не меняется». Уведомление о назначении при этом
        // тоже не создаётся.
        return Ok(current);
    }

    sqlx::query(
        "UPDATE items
         SET list_id = $3, title = $4, notes = $5, priority = $6,
             due_at = $7, due_date = $8, end_at = $9, end_date = $10,
             assignee_user_id = $11, quantity = $12::float8::numeric, unit = $13,
             recurrence_freq = $14, recurrence_interval = $15,
             recurrence_byweekday = $16, recurrence_until = $17,
             recurrence_timezone = $18, recurrence_anchor_date = $19,
             recurrence_month_rule = $20, recurrence_occurrence_date = $21,
             recurrence_occurrence_time = $22,
             version = version + 1, updated_at = $23
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
    .bind(end_at)
    .bind(end_date)
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
    .bind(recurrence_occurrence_date)
    .bind(recurrence_occurrence_time)
    .bind(now)
    .execute(&mut *conn)
    .await?;

    tags::set_item_tags(conn, household_id, item_id, &tag_names).await?;
    subtasks::replace(conn, item_id, &subtask_inputs, now).await?;

    let updated = load_item(conn, household_id, item_id).await?;
    let event_type = if recurrence_changed {
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

    let event = events::insert(
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

    notifications::notify_assignment(
        conn,
        &notifications::AssignmentNotice {
            household_id,
            item_id,
            actor,
            previous_assignee: row.assignee_user_id,
            assignee: updated.assignee_user_id,
            event_id: event.id,
        },
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
