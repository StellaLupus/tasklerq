//! Чтение элементов: карточка, список, календарь и снапшот.

use chrono::{DateTime, Utc};
use sqlx::PgConnection;
use uuid::Uuid;

use super::row::{ITEM_COLUMNS, ItemRow, hydrate_many, load_item};
use crate::domain::error::DomainError;
use crate::domain::models::Item;
use crate::storage::repositories::lists;

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

    let mut live_rows = Vec::new();
    let mut deleted_item_ids = Vec::new();
    for row in rows {
        if row.deleted_at.is_some() {
            if since.is_some() {
                deleted_item_ids.push(row.id);
            }
        } else {
            live_rows.push(row);
        }
    }
    let items = hydrate_many(conn, live_rows).await?;

    Ok((items, deleted_item_ids))
}
