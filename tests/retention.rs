//! Интеграционные тесты retention и purge.

mod support;

use chrono::{Duration, Utc};
use tasklerq::config::RetentionConfig;
use tasklerq::domain::models::ListKind;
use tasklerq::jobs::retention;
use tasklerq::storage::repositories::{items, lists};
use uuid::Uuid;

fn config() -> RetentionConfig {
    RetentionConfig {
        updates_days: 30,
        drafts_days: 7,
        llm_runs_days: 30,
        events_days: 90,
        deleted_days: 30,
        outbox_days: 30,
        audit_days: 180,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retention_expires_drafts_and_purges_old_data() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let doomed = support::create_household(&db.pool).await;
    let now = Utc::now();
    let old = now - Duration::days(31);

    let active = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        support::new_item(household.tasks_list_id, "Активная"),
    )
    .await;

    let old_deleted = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        support::new_item(household.tasks_list_id, "Давно удалённая"),
    )
    .await;
    let recent_deleted = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        support::new_item(household.tasks_list_id, "Недавно удалённая"),
    )
    .await;

    let mut conn = db.pool.acquire().await.expect("connection");
    let old_list = lists::create(
        &mut conn,
        household.household_id,
        ListKind::Checklist,
        "Старый список",
        now,
    )
    .await
    .expect("list");
    let list_item = support::create_item(
        &db.pool,
        &household,
        old_list.id,
        &household.owner(),
        support::new_item(old_list.id, "Элемент старого списка"),
    )
    .await;

    items::soft_delete(
        &mut conn,
        household.household_id,
        &household.owner(),
        old_deleted.id,
        old,
    )
    .await
    .expect("old delete");
    items::soft_delete(
        &mut conn,
        household.household_id,
        &household.owner(),
        recent_deleted.id,
        now - Duration::days(5),
    )
    .await
    .expect("recent delete");
    lists::delete(&mut conn, household.household_id, old_list.id, true, old)
        .await
        .expect("delete old list");

    sqlx::query("UPDATE households SET deleted_at = $2 WHERE id = $1")
        .bind(doomed.household_id)
        .bind(old)
        .execute(&mut *conn)
        .await
        .expect("delete doomed household");

    sqlx::query(
        "UPDATE item_events SET created_at = $2
         WHERE item_id = $1 AND event_type = 'created'",
    )
    .bind(active.id)
    .bind(now - Duration::days(100))
    .execute(&mut *conn)
    .await
    .expect("backdate event");

    let expired_recent = Uuid::now_v7();
    let expired_old = Uuid::now_v7();
    let pending = Uuid::now_v7();
    for (id, created_at, expires_at) in [
        (
            expired_recent,
            now - Duration::days(1),
            now - Duration::hours(1),
        ),
        (
            expired_old,
            now - Duration::days(10),
            now - Duration::days(9),
        ),
        (pending, now - Duration::days(1), now + Duration::days(1)),
    ] {
        sqlx::query(
            "INSERT INTO capture_drafts (
                 id, household_id, source, mode, raw_text, status, created_at, expires_at
             )
             VALUES ($1, $2, 'api', 'auto', 'черновик', 'pending', $3, $4)",
        )
        .bind(id)
        .bind(household.household_id)
        .bind(created_at)
        .bind(expires_at)
        .execute(&mut *conn)
        .await
        .expect("draft");
    }

    let stats = retention::run(&db.pool, &config(), now)
        .await
        .expect("retention run");

    assert_eq!(stats.expired_drafts, 2);
    assert_eq!(stats.purged_items, 2);
    assert_eq!(stats.purged_lists, 1);
    assert_eq!(stats.purged_households, 1);
    assert_eq!(stats.purged_events, 1);
    assert_eq!(stats.purged_drafts, 1);

    let remaining_items: i64 = sqlx::query_scalar("SELECT count(*) FROM items WHERE id = ANY($1)")
        .bind(vec![
            active.id,
            old_deleted.id,
            recent_deleted.id,
            list_item.id,
        ])
        .fetch_one(&mut *conn)
        .await
        .expect("count items");
    assert_eq!(
        remaining_items, 2,
        "only active and recently deleted remain"
    );

    let recent_deleted_at: Option<chrono::DateTime<Utc>> =
        sqlx::query_scalar("SELECT deleted_at FROM items WHERE id = $1")
            .bind(recent_deleted.id)
            .fetch_one(&mut *conn)
            .await
            .expect("recent deleted item");
    assert!(recent_deleted_at.is_some());

    let list_exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM lists WHERE id = $1)")
        .bind(old_list.id)
        .fetch_one(&mut *conn)
        .await
        .expect("list exists");
    assert!(!list_exists);

    let household_exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM households WHERE id = $1)")
            .bind(doomed.household_id)
            .fetch_one(&mut *conn)
            .await
            .expect("household exists");
    assert!(!household_exists);

    let active_events: i64 =
        sqlx::query_scalar("SELECT count(*) FROM item_events WHERE item_id = $1")
            .bind(active.id)
            .fetch_one(&mut *conn)
            .await
            .expect("events");
    assert_eq!(active_events, 0);

    let statuses: Vec<(Uuid, String)> =
        sqlx::query_as("SELECT id, status::text FROM capture_drafts WHERE id = ANY($1)")
            .bind(vec![expired_recent, expired_old, pending])
            .fetch_all(&mut *conn)
            .await
            .expect("draft statuses");
    assert_eq!(statuses.len(), 2, "old draft is purged");
    assert!(
        statuses
            .iter()
            .any(|(id, status)| *id == expired_recent && status == "expired")
    );
    assert!(
        statuses
            .iter()
            .any(|(id, status)| *id == pending && status == "pending")
    );

    let repeated = retention::run(&db.pool, &config(), now)
        .await
        .expect("second run");
    assert_eq!(repeated.expired_drafts, 0);
    assert_eq!(repeated.purged_items, 0);
    assert_eq!(repeated.purged_lists, 0);
    assert_eq!(repeated.purged_households, 0);
    assert_eq!(repeated.purged_events, 0);
    assert_eq!(repeated.purged_drafts, 0);
}
