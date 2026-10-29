//! Репозиторий подзадач.

use chrono::{DateTime, Utc};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::models::{ItemStatus, Subtask, SubtaskInput};

/// Заменить набор подзадач элемента.
pub async fn replace(
    conn: &mut PgConnection,
    item_id: Uuid,
    subtasks: &[SubtaskInput],
    now: DateTime<Utc>,
) -> Result<Vec<Subtask>, DomainError> {
    sqlx::query("DELETE FROM subtasks WHERE item_id = $1")
        .bind(item_id)
        .execute(&mut *conn)
        .await?;

    for (index, subtask) in subtasks.iter().enumerate() {
        let status = subtask.status.unwrap_or(ItemStatus::Open);
        let completed_at = (status == ItemStatus::Done).then_some(now);
        sqlx::query(
            "INSERT INTO subtasks (id, item_id, title, status, position, created_at, completed_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(Uuid::now_v7())
        .bind(item_id)
        .bind(&subtask.title)
        .bind(status)
        .bind(i32::try_from(index).unwrap_or(i32::MAX))
        .bind(now)
        .bind(completed_at)
        .execute(&mut *conn)
        .await?;
    }

    list(conn, item_id).await
}

/// Восстановить подзадачи из снимка события (для undo).
pub async fn replace_snapshot(
    conn: &mut PgConnection,
    item_id: Uuid,
    subtasks: &[Subtask],
) -> Result<Vec<Subtask>, DomainError> {
    sqlx::query("DELETE FROM subtasks WHERE item_id = $1")
        .bind(item_id)
        .execute(&mut *conn)
        .await?;

    for subtask in subtasks {
        sqlx::query(
            "INSERT INTO subtasks (id, item_id, title, status, position, created_at, completed_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(subtask.id)
        .bind(item_id)
        .bind(&subtask.title)
        .bind(subtask.status)
        .bind(subtask.position)
        .bind(subtask.created_at)
        .bind(subtask.completed_at)
        .execute(&mut *conn)
        .await?;
    }

    list(conn, item_id).await
}

/// Подзадачи элемента по порядку.
pub async fn list(conn: &mut PgConnection, item_id: Uuid) -> Result<Vec<Subtask>, DomainError> {
    let subtasks = sqlx::query_as::<_, Subtask>(
        "SELECT id, item_id, title, status, position, created_at, completed_at
         FROM subtasks
         WHERE item_id = $1
         ORDER BY position, created_at, id",
    )
    .bind(item_id)
    .fetch_all(conn)
    .await?;

    Ok(subtasks)
}
