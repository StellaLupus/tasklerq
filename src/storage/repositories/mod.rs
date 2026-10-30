//! Репозитории домена.
//!
//! Функции принимают `&mut PgConnection`, чтобы вызывающая сторона могла
//! объединять несколько операций в одну транзакцию.

pub mod audit;
pub mod capture_jobs;
pub mod drafts;
pub mod events;
pub mod households;
pub mod invites;
pub mod items;
pub mod lists;
pub mod llm_runs;
pub mod members;
pub mod subtasks;
pub mod tags;
pub mod tokens;
pub mod users;

use sqlx::PgConnection;

/// Проверить, что пользователь — активный участник household.
pub async fn active_member(
    conn: &mut PgConnection,
    household_id: uuid::Uuid,
    user_id: uuid::Uuid,
) -> Result<bool, sqlx::Error> {
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1 FROM users
             WHERE id = $1 AND household_id = $2 AND deleted_at IS NULL
         )",
    )
    .bind(user_id)
    .bind(household_id)
    .fetch_one(conn)
    .await?;

    Ok(exists)
}
