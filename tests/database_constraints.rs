//! Интеграционные тесты ограничений схемы.

mod support;

use chrono::Utc;
use sqlx::PgConnection;
use uuid::Uuid;

use tasklerq::domain::models::{ItemSource, ListKind};
use tasklerq::storage::repositories::lists;

fn error_code(error: &sqlx::Error) -> Option<String> {
    match error {
        sqlx::Error::Database(database) => database.code().map(|code| code.into_owned()),
        _ => None,
    }
}

async fn insert_minimal_item(
    conn: &mut PgConnection,
    household_id: Uuid,
    list_id: Uuid,
    suffix: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO items (
             id, household_id, list_id, title, status, priority, source, position, version
         )
         VALUES ($1, $2, $3, $4, 'open', 'normal', 'api', 0, 1)",
    )
    .bind(Uuid::now_v7())
    .bind(household_id)
    .bind(list_id)
    .bind(format!("item {suffix}"))
    .execute(conn)
    .await
    .map(|_| ())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn item_checks_reject_invalid_rows() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let mut conn = db.pool.acquire().await.expect("connection");

    let both_due = sqlx::query(
        "INSERT INTO items (
             id, household_id, list_id, title, status, priority, source,
             due_at, due_date, position, version
         )
         VALUES ($1, $2, $3, 'both due', 'open', 'normal', 'api',
                 '2026-11-01T10:00:00Z', '2026-11-01', 0, 1)",
    )
    .bind(Uuid::now_v7())
    .bind(household.household_id)
    .bind(household.tasks_list_id)
    .execute(&mut *conn)
    .await
    .expect_err("due_at and due_date must be exclusive");
    assert_eq!(error_code(&both_due).as_deref(), Some("23514"));

    let zero_quantity = sqlx::query(
        "INSERT INTO items (
             id, household_id, list_id, title, status, priority, source,
             quantity, position, version
         )
         VALUES ($1, $2, $3, 'zero', 'open', 'normal', 'api', 0, 0, 1)",
    )
    .bind(Uuid::now_v7())
    .bind(household.household_id)
    .bind(household.shopping_list_id)
    .execute(&mut *conn)
    .await
    .expect_err("quantity must be positive");
    assert_eq!(error_code(&zero_quantity).as_deref(), Some("23514"));

    let weekly_without_days = sqlx::query(
        "INSERT INTO items (
             id, household_id, list_id, title, status, priority, source, position, version,
             recurrence_freq, recurrence_interval, recurrence_timezone, recurrence_anchor_date
         )
         VALUES ($1, $2, $3, 'weekly', 'open', 'normal', 'api', 0, 1,
                 'weekly', 1, 'Europe/Moscow', '2026-01-01')",
    )
    .bind(Uuid::now_v7())
    .bind(household.household_id)
    .bind(household.tasks_list_id)
    .execute(&mut *conn)
    .await
    .expect_err("weekly recurrence needs weekdays");
    assert_eq!(error_code(&weekly_without_days).as_deref(), Some("23514"));

    let monthly_without_rule = sqlx::query(
        "INSERT INTO items (
             id, household_id, list_id, title, status, priority, source, position, version,
             recurrence_freq, recurrence_interval, recurrence_timezone, recurrence_anchor_date
         )
         VALUES ($1, $2, $3, 'monthly', 'open', 'normal', 'api', 0, 1,
                 'monthly', 1, 'Europe/Moscow', '2026-01-31')",
    )
    .bind(Uuid::now_v7())
    .bind(household.household_id)
    .bind(household.tasks_list_id)
    .execute(&mut *conn)
    .await
    .expect_err("monthly recurrence needs month_rule");
    assert_eq!(error_code(&monthly_without_rule).as_deref(), Some("23514"));

    insert_minimal_item(
        &mut conn,
        household.household_id,
        household.tasks_list_id,
        "valid",
    )
    .await
    .expect("valid item should insert");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tenant_foreign_key_and_unique_indexes_are_enforced() {
    let db = support::db().await;
    let first = support::create_household(&db.pool).await;
    let second = support::create_household(&db.pool).await;
    let mut conn = db.pool.acquire().await.expect("connection");

    let foreign_list = sqlx::query(
        "INSERT INTO items (
             id, household_id, list_id, title, status, priority, source, position, version
         )
         VALUES ($1, $2, $3, 'foreign list', 'open', 'normal', 'api', 0, 1)",
    )
    .bind(Uuid::now_v7())
    .bind(first.household_id)
    .bind(second.tasks_list_id)
    .execute(&mut *conn)
    .await
    .expect_err("item list must belong to the same household");
    assert_eq!(error_code(&foreign_list).as_deref(), Some("23503"));

    let second_default = sqlx::query(
        "INSERT INTO lists (
             id, household_id, kind, name, is_default, is_archived, position
         )
         VALUES ($1, $2, 'tasks', 'Другое', true, false, 2)",
    )
    .bind(Uuid::now_v7())
    .bind(first.household_id)
    .execute(&mut *conn)
    .await
    .expect_err("only one default list per kind");
    assert_eq!(error_code(&second_default).as_deref(), Some("23505"));

    let duplicate_name = sqlx::query(
        "INSERT INTO lists (
             id, household_id, kind, name, is_default, is_archived, position
         )
         VALUES ($1, $2, 'checklist', 'зАдачи', false, false, 2)",
    )
    .bind(Uuid::now_v7())
    .bind(first.household_id)
    .execute(&mut *conn)
    .await
    .expect_err("list names are unique case-insensitively");
    assert_eq!(error_code(&duplicate_name).as_deref(), Some("23505"));

    let tag = sqlx::query("INSERT INTO tags (id, household_id, name) VALUES ($1, $2, 'Дом')")
        .bind(Uuid::now_v7())
        .bind(first.household_id)
        .execute(&mut *conn)
        .await
        .expect("tag");
    let _ = tag;
    let duplicate_tag =
        sqlx::query("INSERT INTO tags (id, household_id, name) VALUES ($1, $2, 'дом')")
            .bind(Uuid::now_v7())
            .bind(first.household_id)
            .execute(&mut *conn)
            .await
            .expect_err("tag names are unique case-insensitively");
    assert_eq!(error_code(&duplicate_tag).as_deref(), Some("23505"));

    let extra_list = lists::create(
        &mut conn,
        first.household_id,
        ListKind::Checklist,
        "Дача",
        Utc::now(),
    )
    .await
    .expect("additional list");
    assert_eq!(extra_list.kind, ListKind::Checklist);
    let _ = ItemSource::Api;
}
