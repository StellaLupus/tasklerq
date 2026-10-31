//! Планировщик напоминаний: время, дедупликация, отмена и получатель.
//!
//! Время передаётся в тик явно, поэтому тесты не ждут 30-секундный интервал.

mod support;

use std::sync::Arc;

use chrono::{DateTime, Duration, Timelike, Utc};
use serde_json::json;
use sqlx::PgPool;
use support::{TestDb, TestHousehold};
use tasklerq::domain::models::{Item, RecurrenceFreq, RecurrenceInput};
use tasklerq::jobs::scheduler::{Scheduler, occurrence_key};
use tasklerq::storage::repositories::items;
use tokio::sync::Notify;
use uuid::Uuid;

fn scheduler(pool: &PgPool) -> Scheduler {
    Scheduler::new(pool.clone(), Arc::new(Notify::new()))
}

fn rounded_now() -> DateTime<Utc> {
    Utc::now()
        .with_second(0)
        .and_then(|at| at.with_nanosecond(0))
        .expect("rounded time")
}

async fn create_task(
    db: &TestDb,
    household: &TestHousehold,
    title: &str,
    due_at: Option<DateTime<Utc>>,
    assignee: Option<Uuid>,
    recurrence: Option<RecurrenceInput>,
) -> Item {
    let mut item = support::new_item(household.tasks_list_id, title);
    item.due_at = due_at;
    item.assignee_user_id = assignee;
    item.recurrence = recurrence;
    support::create_item(
        &db.pool,
        household,
        household.tasks_list_id,
        &household.owner(),
        item,
    )
    .await
}

async fn chat_id(pool: &PgPool, user_id: Uuid) -> i64 {
    sqlx::query_scalar("SELECT telegram_chat_id FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_one(pool)
        .await
        .expect("chat id")
}

async fn messages(pool: &PgPool, chat: i64) -> Vec<String> {
    sqlx::query_scalar("SELECT payload->>'html' FROM outbox WHERE chat_id = $1 ORDER BY seq")
        .bind(chat)
        .fetch_all(pool)
        .await
        .expect("outbox messages")
}

async fn delivery_rows(pool: &PgPool, item_id: Uuid) -> Vec<(String, String, Option<i64>)> {
    sqlx::query_as(
        "SELECT status::text, occurrence_key, outbox_seq
         FROM notification_deliveries WHERE item_id = $1
         ORDER BY created_at, id",
    )
    .bind(item_id)
    .fetch_all(pool)
    .await
    .expect("deliveries")
}

async fn set_notifications(pool: &PgPool, user_id: Uuid, value: serde_json::Value) {
    sqlx::query(
        "UPDATE users
         SET settings = jsonb_set(settings, '{notifications}', $2::jsonb, true)
         WHERE id = $1",
    )
    .bind(user_id)
    .bind(value)
    .execute(pool)
    .await
    .expect("settings");
}

#[allow(clippy::too_many_arguments)]
async fn insert_delivery(
    pool: &PgPool,
    household_id: Uuid,
    user_id: Uuid,
    item_id: Option<Uuid>,
    key: &str,
    status: &str,
    scheduled_at: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO notification_deliveries (
             id, household_id, user_id, item_id, occurrence_key, kind,
             status, scheduled_at, created_at, updated_at
         )
         VALUES ($1, $2, $3, $4, $5, 'reminder', $6::notification_status, $7, $7, $7)",
    )
    .bind(Uuid::now_v7())
    .bind(household_id)
    .bind(user_id)
    .bind(item_id)
    .bind(key)
    .bind(status)
    .bind(scheduled_at)
    .execute(pool)
    .await
    .map(|_| ())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timed_reminder_fires_exactly_at_lead_time() {
    let db = support::isolated_db().await;
    let household = support::create_household(&db.pool).await;
    let due = rounded_now() + Duration::hours(3);
    let item = create_task(
        &db,
        &household,
        "Позвонить в поликлинику",
        Some(due),
        Some(household.owner_id),
        None,
    )
    .await;
    let chat = chat_id(&db.pool, household.owner_id).await;
    let scheduler = scheduler(&db.pool);

    let early = scheduler
        .tick(due - Duration::minutes(16))
        .await
        .expect("early tick");
    assert_eq!(early.delivered, 0);
    assert_eq!(early.planned, 0);
    assert!(messages(&db.pool, chat).await.is_empty());

    let on_time = scheduler
        .tick(due - Duration::minutes(15))
        .await
        .expect("on-time tick");
    assert_eq!(on_time.planned, 1);
    assert_eq!(on_time.delivered, 1);

    let sent = messages(&db.pool, chat).await;
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert!(sent[0].contains("Позвонить в поликлинику"), "{sent:?}");
    let rows = delivery_rows(&db.pool, item.id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "sent");
    assert_eq!(rows[0].1, occurrence_key(due));
    assert!(rows[0].2.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeated_ticks_do_not_send_the_same_reminder_twice() {
    let db = support::isolated_db().await;
    let household = support::create_household(&db.pool).await;
    let due = rounded_now() + Duration::hours(3);
    let item = create_task(
        &db,
        &household,
        "Вынести мусор",
        Some(due),
        Some(household.owner_id),
        None,
    )
    .await;
    let chat = chat_id(&db.pool, household.owner_id).await;
    let scheduler = scheduler(&db.pool);

    assert_eq!(
        scheduler
            .tick(due - Duration::minutes(15))
            .await
            .expect("first tick")
            .delivered,
        1
    );
    assert_eq!(
        scheduler
            .tick(due - Duration::minutes(15))
            .await
            .expect("same tick")
            .delivered,
        0
    );
    assert_eq!(
        scheduler
            .tick(due - Duration::minutes(5))
            .await
            .expect("later tick")
            .delivered,
        0
    );

    assert_eq!(messages(&db.pool, chat).await.len(), 1);
    let rows = delivery_rows(&db.pool, item.id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "sent");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_does_not_duplicate_and_next_occurrence_gets_its_own_delivery() {
    let db = support::isolated_db().await;
    let household = support::create_household(&db.pool).await;
    let due = rounded_now() + Duration::hours(3);
    let item = create_task(
        &db,
        &household,
        "Еженедельная планёрка",
        Some(due),
        Some(household.owner_id),
        Some(support::recurrence(RecurrenceFreq::Daily, Vec::new())),
    )
    .await;
    let chat = chat_id(&db.pool, household.owner_id).await;

    assert_eq!(
        scheduler(&db.pool)
            .tick(due - Duration::minutes(15))
            .await
            .expect("first instance")
            .delivered,
        1
    );
    // «Перезапуск»: новый экземпляр с тем же состоянием базы.
    assert_eq!(
        scheduler(&db.pool)
            .tick(due - Duration::minutes(14))
            .await
            .expect("restarted instance")
            .delivered,
        0
    );
    assert_eq!(messages(&db.pool, chat).await.len(), 1);

    let completed_at = due - Duration::minutes(10);
    let mut conn = db.pool.acquire().await.expect("conn");
    let next = items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        item.id,
        None,
        "Europe/Moscow",
        completed_at,
    )
    .await
    .expect("complete recurring task");
    let next_due = next.due_at.expect("next occurrence has a time");
    assert!(next_due > due);

    assert_eq!(
        scheduler(&db.pool)
            .tick(next_due - Duration::minutes(15))
            .await
            .expect("next occurrence tick")
            .delivered,
        1
    );

    let rows = delivery_rows(&db.pool, item.id).await;
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_ne!(rows[0].1, rows[1].1);
    assert_eq!(rows[1].1, occurrence_key(next_due));
    assert_eq!(messages(&db.pool, chat).await.len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completed_task_is_skipped_and_pending_delivery_is_cancelled() {
    let db = support::isolated_db().await;
    let household = support::create_household(&db.pool).await;
    let due = rounded_now() + Duration::hours(3);
    let chat = chat_id(&db.pool, household.owner_id).await;
    let scheduler = scheduler(&db.pool);

    // Завершена до времени напоминания — сообщения нет.
    let early = create_task(
        &db,
        &household,
        "Купить билеты",
        Some(due),
        Some(household.owner_id),
        None,
    )
    .await;
    let mut conn = db.pool.acquire().await.expect("conn");
    items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        early.id,
        None,
        "Europe/Moscow",
        due - Duration::minutes(30),
    )
    .await
    .expect("complete early");
    let stats = scheduler
        .tick(due - Duration::minutes(15))
        .await
        .expect("tick");
    assert_eq!(stats.delivered, 0);
    assert_eq!(stats.planned, 0);
    assert!(messages(&db.pool, chat).await.is_empty());

    // Запись уже была запланирована, но задачу завершили — она отменяется.
    let pending = create_task(
        &db,
        &household,
        "Оплатить счёт",
        Some(due + Duration::hours(1)),
        Some(household.owner_id),
        None,
    )
    .await;
    let pending_due = due + Duration::hours(1);
    insert_delivery(
        &db.pool,
        household.household_id,
        household.owner_id,
        Some(pending.id),
        &occurrence_key(pending_due),
        "pending",
        pending_due - Duration::minutes(15),
    )
    .await
    .expect("pending delivery");
    items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        pending.id,
        None,
        "Europe/Moscow",
        pending_due - Duration::minutes(30),
    )
    .await
    .expect("complete pending");

    let stats = scheduler
        .tick(pending_due - Duration::minutes(16))
        .await
        .expect("cancel tick");
    assert_eq!(stats.cancelled, 1);
    let rows = delivery_rows(&db.pool, pending.id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "cancelled");
    assert!(messages(&db.pool, chat).await.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deleted_and_dateless_tasks_get_no_reminders() {
    let db = support::isolated_db().await;
    let household = support::create_household(&db.pool).await;
    let due = rounded_now() + Duration::hours(3);
    let chat = chat_id(&db.pool, household.owner_id).await;

    let dateless = create_task(
        &db,
        &household,
        "Когда-нибудь",
        None,
        Some(household.owner_id),
        None,
    )
    .await;
    let deleted = create_task(
        &db,
        &household,
        "Удалённая",
        Some(due),
        Some(household.owner_id),
        None,
    )
    .await;
    let mut conn = db.pool.acquire().await.expect("conn");
    items::soft_delete(
        &mut conn,
        household.household_id,
        &household.owner(),
        deleted.id,
        Utc::now(),
    )
    .await
    .expect("delete task");

    let stats = scheduler(&db.pool)
        .tick(due - Duration::minutes(15))
        .await
        .expect("tick");
    assert_eq!(stats.delivered, 0);
    assert_eq!(stats.planned, 0);
    assert!(messages(&db.pool, chat).await.is_empty());
    assert!(delivery_rows(&db.pool, dateless.id).await.is_empty());
    assert!(delivery_rows(&db.pool, deleted.id).await.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delivery_key_is_unique_including_null_item() {
    let db = support::isolated_db().await;
    let household = support::create_household(&db.pool).await;
    let item = create_task(
        &db,
        &household,
        "Задача",
        None,
        Some(household.owner_id),
        None,
    )
    .await;
    let key = occurrence_key(rounded_now());
    let scheduled = rounded_now();

    insert_delivery(
        &db.pool,
        household.household_id,
        household.owner_id,
        Some(item.id),
        &key,
        "pending",
        scheduled,
    )
    .await
    .expect("first delivery");
    assert!(
        insert_delivery(
            &db.pool,
            household.household_id,
            household.owner_id,
            Some(item.id),
            &key,
            "pending",
            scheduled,
        )
        .await
        .is_err(),
        "duplicate (user, item, occurrence, kind) must be rejected"
    );

    insert_delivery(
        &db.pool,
        household.household_id,
        household.owner_id,
        None,
        &key,
        "pending",
        scheduled,
    )
    .await
    .expect("first delivery without item");
    assert!(
        insert_delivery(
            &db.pool,
            household.household_id,
            household.owner_id,
            None,
            &key,
            "pending",
            scheduled,
        )
        .await
        .is_err(),
        "NULL item must still be deduplicated"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disabled_reminders_produce_no_delivery() {
    let db = support::isolated_db().await;
    let household = support::create_household(&db.pool).await;
    let due = rounded_now() + Duration::hours(3);
    let item = create_task(
        &db,
        &household,
        "Без напоминания",
        Some(due),
        Some(household.owner_id),
        None,
    )
    .await;
    set_notifications(
        &db.pool,
        household.owner_id,
        json!({"reminders_enabled": false}),
    )
    .await;
    let chat = chat_id(&db.pool, household.owner_id).await;

    let stats = scheduler(&db.pool)
        .tick(due - Duration::minutes(15))
        .await
        .expect("tick");
    assert_eq!(stats.delivered, 0);
    assert!(messages(&db.pool, chat).await.is_empty());
    assert!(delivery_rows(&db.pool, item.id).await.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn assignee_gets_the_reminder_and_author_does_not() {
    let db = support::isolated_db().await;
    let household = support::create_household(&db.pool).await;
    let due = rounded_now() + Duration::hours(3);
    let owner_chat = chat_id(&db.pool, household.owner_id).await;
    let member_chat = chat_id(&db.pool, household.member_id).await;

    let assigned = create_task(
        &db,
        &household,
        "Задача участника",
        Some(due),
        Some(household.member_id),
        None,
    )
    .await;
    // Без исполнителя напоминание получает автор.
    let authored = create_task(&db, &household, "Задача автора", Some(due), None, None).await;

    let stats = scheduler(&db.pool)
        .tick(due - Duration::minutes(15))
        .await
        .expect("tick");
    assert_eq!(stats.delivered, 2);

    let member_messages = messages(&db.pool, member_chat).await;
    assert_eq!(member_messages.len(), 1, "{member_messages:?}");
    assert!(member_messages[0].contains("Задача участника"));
    let owner_messages = messages(&db.pool, owner_chat).await;
    assert_eq!(owner_messages.len(), 1, "{owner_messages:?}");
    assert!(owner_messages[0].contains("Задача автора"));

    let assigned_user: Uuid =
        sqlx::query_scalar("SELECT user_id FROM notification_deliveries WHERE item_id = $1")
            .bind(assigned.id)
            .fetch_one(&db.pool)
            .await
            .expect("assignee delivery");
    assert_eq!(assigned_user, household.member_id);
    let authored_user: Uuid =
        sqlx::query_scalar("SELECT user_id FROM notification_deliveries WHERE item_id = $1")
            .bind(authored.id)
            .fetch_one(&db.pool)
            .await
            .expect("author delivery");
    assert_eq!(authored_user, household.owner_id);
}
