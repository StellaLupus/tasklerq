//! Журнал событий элемента.

use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::models::{Actor, EventType, ItemEvent};

/// Записать событие.
#[allow(clippy::too_many_arguments)]
pub async fn insert(
    conn: &mut PgConnection,
    household_id: Uuid,
    item_id: Uuid,
    actor: &Actor,
    event_type: EventType,
    payload: Value,
    undo_of: Option<Uuid>,
    now: DateTime<Utc>,
) -> Result<ItemEvent, DomainError> {
    let event = sqlx::query_as::<_, ItemEvent>(
        "INSERT INTO item_events (
             id, household_id, item_id, actor_user_id, actor_type, event_type,
             payload, undo_of, created_at
         )
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
         RETURNING id, household_id, item_id, actor_user_id, actor_type, event_type,
                   payload, undo_of, created_at",
    )
    .bind(Uuid::now_v7())
    .bind(household_id)
    .bind(item_id)
    .bind(actor.user_id)
    .bind(actor.actor_type)
    .bind(event_type)
    .bind(payload)
    .bind(undo_of)
    .bind(now)
    .fetch_one(conn)
    .await?;

    Ok(event)
}

/// Последнее событие, которое ещё можно отменить.
pub async fn latest_undoable(
    conn: &mut PgConnection,
    household_id: Uuid,
    item_id: Uuid,
) -> Result<Option<ItemEvent>, DomainError> {
    let event = sqlx::query_as::<_, ItemEvent>(
        "SELECT e.id, e.household_id, e.item_id, e.actor_user_id, e.actor_type,
                e.event_type, e.payload, e.undo_of, e.created_at
         FROM item_events e
         WHERE e.household_id = $1
           AND e.item_id = $2
           AND e.undo_of IS NULL
           AND NOT EXISTS (
               SELECT 1 FROM item_events undo WHERE undo.undo_of = e.id
           )
         ORDER BY e.created_at DESC, e.id DESC
         LIMIT 1",
    )
    .bind(household_id)
    .bind(item_id)
    .fetch_optional(conn)
    .await?;

    Ok(event)
}

/// События элемента по убыванию времени (для аудита и тестов).
pub async fn for_item(
    conn: &mut PgConnection,
    household_id: Uuid,
    item_id: Uuid,
) -> Result<Vec<ItemEvent>, DomainError> {
    let events = sqlx::query_as::<_, ItemEvent>(
        "SELECT id, household_id, item_id, actor_user_id, actor_type, event_type,
                payload, undo_of, created_at
         FROM item_events
         WHERE household_id = $1 AND item_id = $2
         ORDER BY created_at DESC, id DESC",
    )
    .bind(household_id)
    .bind(item_id)
    .fetch_all(conn)
    .await?;

    Ok(events)
}
