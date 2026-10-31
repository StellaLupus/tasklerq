//! Интеграционные тесты retention и purge.

mod support;

// Каждый тест работает в своей базе: `retention::run` чистит всю базу целиком,
// и в общей базе параллельный тест успевал удалить строки соседа раньше, чем
// тот читал статистику.

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
    let db = support::isolated_db().await;
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retention_purges_old_audit_events_only() {
    use tasklerq::domain::identity::AuditEventType;
    use tasklerq::domain::models::{Actor, ActorType};
    use tasklerq::storage::repositories::audit;

    let db = support::isolated_db().await;
    let household = support::create_household(&db.pool).await;
    let now = Utc::now();
    let mut conn = db.pool.acquire().await.expect("connection");
    let actor = Actor::system(ActorType::System);

    for (age_days, label) in [(200, "old"), (10, "recent")] {
        audit::record(
            &mut conn,
            Some(household.household_id),
            &actor,
            AuditEventType::MemberLeft,
            serde_json::json!({"label": label}),
            now - Duration::days(age_days),
        )
        .await
        .expect("event");
    }

    let stats = retention::run(&db.pool, &config(), now)
        .await
        .expect("retention");
    assert!(stats.purged_audit_events >= 1);

    let labels: Vec<String> = sqlx::query_scalar(
        "SELECT payload->>'label' FROM audit_events
         WHERE household_id = $1 AND payload ? 'label'",
    )
    .bind(household.household_id)
    .fetch_all(&db.pool)
    .await
    .expect("labels");
    assert_eq!(labels, vec!["recent".to_owned()]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retention_purges_finished_invites_only() {
    use tasklerq::storage::repositories::invites;

    let db = support::isolated_db().await;
    let household = support::create_household(&db.pool).await;
    let now = Utc::now();
    let long_ago = now - Duration::days(400);

    // Истёкшее давно, использованное давно, отозванное давно и свежее активное.
    let mut conn = db.pool.acquire().await.expect("connection");
    let mut created = Vec::new();
    for (age, name) in [
        (long_ago, "expired"),
        (long_ago, "used"),
        (long_ago, "revoked"),
    ] {
        let invite = invites::create(&mut conn, household.household_id, &household.owner(), age)
            .await
            .expect(name);
        created.push(invite.invite.id);
    }
    sqlx::query("UPDATE invites SET used_by_user_id = $2, used_at = $3 WHERE id = $1")
        .bind(created[1])
        .bind(household.member_id)
        .bind(long_ago)
        .execute(&mut *conn)
        .await
        .expect("mark used");
    sqlx::query("UPDATE invites SET revoked_at = $2 WHERE id = $1")
        .bind(created[2])
        .bind(long_ago)
        .execute(&mut *conn)
        .await
        .expect("mark revoked");
    let fresh = invites::create(&mut conn, household.household_id, &household.owner(), now)
        .await
        .expect("fresh");
    drop(conn);

    // Тесты файла идут параллельно на одной базе, и чужой запуск retention мог
    // удалить строки раньше, поэтому счётчик не проверяется, только состояние.
    retention::run(&db.pool, &config(), now)
        .await
        .expect("retention");

    let left: Vec<uuid::Uuid> =
        sqlx::query_scalar("SELECT id FROM invites WHERE household_id = $1")
            .bind(household.household_id)
            .fetch_all(&db.pool)
            .await
            .expect("invites");
    assert_eq!(left, vec![fresh.invite.id]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retention_purges_llm_runs_older_than_the_retention_window() {
    let db = support::isolated_db().await;
    let household = support::create_household(&db.pool).await;
    let now = Utc::now();
    let old = Uuid::now_v7();
    let recent = Uuid::now_v7();

    for (id, created) in [
        (old, now - Duration::days(31)),
        (recent, now - Duration::days(5)),
    ] {
        sqlx::query(
            "INSERT INTO llm_runs (
                 id, household_id, source, mode, prompt_version, status,
                 latency_ms, created_at
             )
             VALUES ($1, $2, 'telegram', 'draft', 'capture-v2', 'success', 1200, $3)",
        )
        .bind(id)
        .bind(household.household_id)
        .bind(created)
        .execute(&db.pool)
        .await
        .expect("llm run");
    }

    let stats = retention::run(&db.pool, &config(), now)
        .await
        .expect("retention");
    assert!(stats.purged_llm_runs >= 1);

    let left: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM llm_runs WHERE id = ANY($1)")
        .bind(vec![old, recent])
        .fetch_all(&db.pool)
        .await
        .expect("runs");
    assert_eq!(left, vec![recent]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retention_purges_finished_telegram_data_but_keeps_pending() {
    let db = support::isolated_db().await;
    let now = Utc::now();
    let old = now - Duration::days(40);
    let tag = Uuid::now_v7().to_string();

    // Обновления: старые обработанные и мёртвые удаляются, ожидающие остаются.
    for (id, status, received) in [
        (7_100_001_i64, "processed", old),
        (7_100_002, "dead", old),
        (7_100_003, "pending", old),
        (7_100_004, "processed", now),
    ] {
        sqlx::query(
            "INSERT INTO telegram_updates (update_id, payload, status, received_at)
             VALUES ($1, '{}'::jsonb, $2::update_status, $3)",
        )
        .bind(id)
        .bind(status)
        .bind(received)
        .execute(&db.pool)
        .await
        .expect("update");
    }
    // Outbox: старые отправленные, пропущенные и мёртвые удаляются, ожидающие остаются.
    for (status, created) in [
        ("sent", old),
        ("skipped", old),
        ("dead", old),
        ("pending", old),
        ("sent", now),
    ] {
        sqlx::query(
            "INSERT INTO outbox (chat_id, kind, payload, status, created_at, dedupe_key)
             VALUES (1, 'message', '{}'::jsonb, $1::outbox_status, $2, $3)",
        )
        .bind(status)
        .bind(created)
        .bind(format!("{tag}:{status}:{}", created.timestamp()))
        .execute(&db.pool)
        .await
        .expect("outbox");
    }
    sqlx::query(
        "INSERT INTO conversation_states (chat_id, telegram_user_id, kind, expires_at)
         VALUES (7100001, 1, 'search', $1), (7100002, 1, 'search', $2)",
    )
    .bind(now - Duration::minutes(1))
    .bind(now + Duration::minutes(10))
    .execute(&db.pool)
    .await
    .expect("forms");

    let stats = retention::run(&db.pool, &config(), now)
        .await
        .expect("retention");

    assert!(stats.purged_updates >= 1);
    assert!(stats.purged_outbox >= 1);
    assert!(stats.purged_conversations >= 1);
    let remaining: Vec<(i64,)> = sqlx::query_as(
        "SELECT update_id FROM telegram_updates WHERE update_id BETWEEN 7100001 AND 7100004 ORDER BY 1",
    )
    .fetch_all(&db.pool)
    .await
    .expect("remaining");
    assert_eq!(remaining, [(7_100_003,), (7_100_004,)]);
    let outbox: Vec<(String,)> = sqlx::query_as(
        "SELECT status::text FROM outbox WHERE dedupe_key LIKE $1 ORDER BY status::text",
    )
    .bind(format!("{tag}%"))
    .fetch_all(&db.pool)
    .await
    .expect("outbox");
    assert_eq!(outbox, [("pending".to_owned(),), ("sent".to_owned(),)]);
    let forms: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM conversation_states WHERE chat_id IN (7100001, 7100002)",
    )
    .fetch_one(&db.pool)
    .await
    .expect("forms");
    assert_eq!(forms, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retention_purges_capture_request_reservations_with_the_drafts() {
    let db = support::isolated_db().await;
    let household = support::create_household(&db.pool).await;
    let now = Utc::now();
    let old = now - Duration::days(8);
    let recent = now - Duration::days(1);

    for (key, created_at) in [("old", old), ("recent", recent)] {
        sqlx::query(
            "INSERT INTO capture_requests (
                 household_id, source, idempotency_key, request_hash, capture_id,
                 response, created_at, completed_at
             )
             VALUES ($1, 'api', $2, 'hash', $3, NULL, $4, NULL)",
        )
        .bind(household.household_id)
        .bind(key)
        .bind(Uuid::now_v7())
        .bind(created_at)
        .execute(&db.pool)
        .await
        .expect("capture request");
    }

    let stats = retention::run(&db.pool, &config(), now)
        .await
        .expect("retention");

    assert_eq!(stats.purged_capture_requests, 1);
    let remaining: Vec<(String,)> =
        sqlx::query_as("SELECT idempotency_key FROM capture_requests WHERE household_id = $1")
            .bind(household.household_id)
            .fetch_all(&db.pool)
            .await
            .expect("remaining reservations");
    assert_eq!(remaining, [("recent".to_owned(),)]);
}
