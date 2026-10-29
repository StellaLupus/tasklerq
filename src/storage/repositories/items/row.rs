//! Строка `items` и сборка `Item` с тегами и подзадачами.

use chrono::{DateTime, NaiveDate, Utc};
use sqlx::{FromRow, PgConnection};
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::models::{
    Item, ItemPriority, ItemSource, ItemStatus, MonthRule, Recurrence, RecurrenceFreq, Subtask,
};
use crate::domain::recurrence::Due;
use crate::storage::repositories::{subtasks, tags};

pub(super) const ITEM_COLUMNS: &str = "i.id, i.household_id, i.list_id, i.title, i.notes, \
    i.status, i.priority, i.due_at, i.due_date, i.assignee_user_id, \
    i.created_by_user_id, i.source, i.capture_id, i.completed_at, \
    i.completed_by_user_id, i.quantity::float8 AS quantity, i.unit, i.position, \
    i.version, i.recurrence_freq, i.recurrence_interval, i.recurrence_byweekday, \
    i.recurrence_until, i.recurrence_timezone, i.recurrence_anchor_date, \
    i.recurrence_month_rule, i.created_at, i.updated_at, i.deleted_at";

#[derive(Debug, Clone, FromRow)]
pub(super) struct ItemRow {
    pub(super) id: Uuid,
    pub(super) household_id: Uuid,
    pub(super) list_id: Uuid,
    pub(super) title: String,
    pub(super) notes: Option<String>,
    pub(super) status: ItemStatus,
    pub(super) priority: ItemPriority,
    pub(super) due_at: Option<DateTime<Utc>>,
    pub(super) due_date: Option<NaiveDate>,
    pub(super) assignee_user_id: Option<Uuid>,
    pub(super) created_by_user_id: Option<Uuid>,
    pub(super) source: ItemSource,
    pub(super) capture_id: Option<Uuid>,
    pub(super) completed_at: Option<DateTime<Utc>>,
    pub(super) completed_by_user_id: Option<Uuid>,
    pub(super) quantity: Option<f64>,
    pub(super) unit: Option<String>,
    pub(super) position: i32,
    pub(super) version: i32,
    pub(super) recurrence_freq: Option<RecurrenceFreq>,
    pub(super) recurrence_interval: Option<i16>,
    pub(super) recurrence_byweekday: Option<Vec<i16>>,
    pub(super) recurrence_until: Option<NaiveDate>,
    pub(super) recurrence_timezone: Option<String>,
    pub(super) recurrence_anchor_date: Option<NaiveDate>,
    pub(super) recurrence_month_rule: Option<MonthRule>,
    pub(super) created_at: DateTime<Utc>,
    pub(super) updated_at: DateTime<Utc>,
    pub(super) deleted_at: Option<DateTime<Utc>>,
}

impl ItemRow {
    pub(super) fn recurrence(&self) -> Option<Recurrence> {
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

    pub(super) fn due(&self) -> Option<Due> {
        match (self.due_at, self.due_date) {
            (Some(at), None) => Some(Due::Timed(at)),
            (None, Some(date)) => Some(Due::AllDay(date)),
            _ => None,
        }
    }
}

pub(super) async fn row_by_id(
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

pub(super) async fn load_item(
    conn: &mut PgConnection,
    household_id: Uuid,
    item_id: Uuid,
) -> Result<Item, DomainError> {
    let row = row_by_id(conn, household_id, item_id, false, false).await?;
    hydrate_one(conn, row).await
}

pub(super) async fn load_item_any(
    conn: &mut PgConnection,
    household_id: Uuid,
    item_id: Uuid,
) -> Result<Item, DomainError> {
    let row = row_by_id(conn, household_id, item_id, true, false).await?;
    hydrate_one(conn, row).await
}

pub(super) async fn hydrate_one(
    conn: &mut PgConnection,
    row: ItemRow,
) -> Result<Item, DomainError> {
    let mut items = hydrate_many(conn, vec![row]).await?;
    Ok(items.remove(0))
}

/// Теги и подзадачи всех строк загружаются двумя запросами, а не по два на
/// элемент.
pub(super) async fn hydrate_many(
    conn: &mut PgConnection,
    rows: Vec<ItemRow>,
) -> Result<Vec<Item>, DomainError> {
    if rows.is_empty() {
        return Ok(Vec::new());
    }

    let ids: Vec<Uuid> = rows.iter().map(|row| row.id).collect();
    let mut tags = tags::for_items(conn, &ids).await?;
    let mut subtasks = subtasks::for_items(conn, &ids).await?;

    Ok(rows
        .into_iter()
        .map(|row| {
            let tags = tags.remove(&row.id).unwrap_or_default();
            let subtasks = subtasks.remove(&row.id).unwrap_or_default();
            row_to_item(row, tags, subtasks)
        })
        .collect())
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
