//! Планировщик напоминаний: время, дедупликация, отмена и получатель.
//!
//! Время передаётся в тик явно, поэтому тесты не ждут 30-секундный интервал.

mod support;

use std::sync::Arc;

use chrono::{DateTime, Datelike, Duration, NaiveDate, TimeZone, Timelike, Utc};
use chrono_tz::Tz;
use serde_json::json;
use sqlx::PgPool;
use support::{TestDb, TestHousehold};
use tasklerq::domain::models::{Item, RecurrenceFreq, RecurrenceInput};
use tasklerq::jobs::scheduler::{Scheduler, all_day_occurrence_key, occurrence_key};
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

async fn create_all_day_task(
    db: &TestDb,
    household: &TestHousehold,
    title: &str,
    due_date: NaiveDate,
    assignee: Option<Uuid>,
) -> Item {
    let mut item = support::new_item(household.tasks_list_id, title);
    item.due_date = Some(due_date);
    item.assignee_user_id = assignee;
    support::create_item(
        &db.pool,
        household,
        household.tasks_list_id,
        &household.owner(),
        item,
    )
    .await
}

async fn clear_author(pool: &PgPool, item_id: Uuid) {
    sqlx::query(
        "UPDATE items
         SET created_by_user_id = NULL, source = 'ha_voice'
         WHERE id = $1",
    )
    .bind(item_id)
    .execute(pool)
    .await
    .expect("clear author");
}

async fn set_user_timezone(pool: &PgPool, user_id: Uuid, timezone: &str) {
    sqlx::query("UPDATE users SET timezone = $2 WHERE id = $1")
        .bind(user_id)
        .bind(timezone)
        .execute(pool)
        .await
        .expect("user timezone");
}

async fn set_household_timezone(pool: &PgPool, household_id: Uuid, timezone: &str) {
    sqlx::query("UPDATE households SET timezone = $2 WHERE id = $1")
        .bind(household_id)
        .bind(timezone)
        .execute(pool)
        .await
        .expect("household timezone");
}

async fn add_member(pool: &PgPool, household_id: Uuid, name: &str) -> Uuid {
    let id = Uuid::now_v7();
    let telegram_id = support::next_telegram_id();
    sqlx::query(
        "INSERT INTO users (
             id, household_id, telegram_user_id, telegram_chat_id, display_name,
             role, settings, created_at, updated_at
         )
         VALUES (
             $1, $2, $3, $3, $4, 'member',
             jsonb_build_object('notifications', jsonb_build_object('digest_enabled', false)),
             now(), now()
         )",
    )
    .bind(id)
    .bind(household_id)
    .bind(telegram_id)
    .bind(name)
    .execute(pool)
    .await
    .expect("extra member");
    id
}

fn local_utc(timezone: Tz, date: NaiveDate, hour: u32, minute: u32) -> DateTime<Utc> {
    timezone
        .with_ymd_and_hms(date.year(), date.month(), date.day(), hour, minute, 0)
        .single()
        .expect("local time")
        .with_timezone(&Utc)
}

async fn delivery_status(pool: &PgPool, item_id: Uuid, user_id: Uuid) -> Option<String> {
    sqlx::query_scalar(
        "SELECT status::text FROM notification_deliveries
         WHERE item_id = $1 AND user_id = $2",
    )
    .bind(item_id)
    .bind(user_id)
    .fetch_optional(pool)
    .await
    .expect("delivery status")
}

async fn insert_outbox(pool: &PgPool, chat: i64, dedupe_key: &str) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO outbox (chat_id, kind, payload, dedupe_key, next_attempt_at, created_at)
         VALUES ($1, 'message', jsonb_build_object('html', 'already', 'rich', false), $2, now(), now())
         RETURNING seq",
    )
    .bind(chat)
    .bind(dedupe_key)
    .fetch_one(pool)
    .await
    .expect("outbox row")
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

async fn create_shopping(db: &TestDb, household: &TestHousehold, title: &str) -> Item {
    let item = support::new_item(household.shopping_list_id, title);
    support::create_item(
        &db.pool,
        household,
        household.shopping_list_id,
        &household.owner(),
        item,
    )
    .await
}

async fn digest_rows(
    pool: &PgPool,
    user_id: Uuid,
) -> Vec<(String, String, Option<Uuid>, Option<i64>)> {
    sqlx::query_as(
        "SELECT occurrence_key, status::text, item_id, outbox_seq
         FROM notification_deliveries
         WHERE user_id = $1 AND kind = 'digest'
         ORDER BY created_at, id",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
    .expect("digest deliveries")
}

async fn insert_digest_delivery(
    pool: &PgPool,
    household_id: Uuid,
    user_id: Uuid,
    key: &str,
    scheduled_at: DateTime<Utc>,
) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO notification_deliveries (
             id, household_id, user_id, item_id, occurrence_key, kind,
             status, scheduled_at, created_at, updated_at
         )
         VALUES ($1, $2, $3, NULL, $4, 'digest', 'pending', $5, $5, $5)",
    )
    .bind(id)
    .bind(household_id)
    .bind(user_id)
    .bind(key)
    .bind(scheduled_at)
    .execute(pool)
    .await
    .expect("digest delivery");
    id
}

/// Household, у которого сводка выключена: тесты напоминаний не должны
/// получать дополнительные сообщения от планировщика.
async fn household_without_digest(db: &TestDb) -> TestHousehold {
    let household = support::create_household(&db.pool).await;
    for user_id in [household.owner_id, household.member_id] {
        set_notifications(&db.pool, user_id, json!({"digest_enabled": false})).await;
    }
    household
}

async fn enable_digest(pool: &PgPool, user_id: Uuid) {
    set_notifications(pool, user_id, json!({"digest_enabled": true})).await;
}

async fn set_notifications(pool: &PgPool, user_id: Uuid, mut value: serde_json::Value) {
    // Настройки заменяются целиком, поэтому без явного значения сводка
    // остаётся выключенной и не влияет на тесты напоминаний.
    if let Some(settings) = value.as_object_mut() {
        settings
            .entry("digest_enabled")
            .or_insert(serde_json::Value::Bool(false));
    }
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
) -> Result<Uuid, sqlx::Error> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO notification_deliveries (
             id, household_id, user_id, item_id, occurrence_key, kind,
             status, scheduled_at, created_at, updated_at
         )
         VALUES ($1, $2, $3, $4, $5, 'reminder', $6::notification_status, $7, $7, $7)",
    )
    .bind(id)
    .bind(household_id)
    .bind(user_id)
    .bind(item_id)
    .bind(key)
    .bind(status)
    .bind(scheduled_at)
    .execute(pool)
    .await
    .map(|_| id)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timed_reminder_fires_exactly_at_lead_time() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
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
    let household = household_without_digest(&db).await;
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
    let household = household_without_digest(&db).await;
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
    let household = household_without_digest(&db).await;
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
    let household = household_without_digest(&db).await;
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
    let household = household_without_digest(&db).await;
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
    let household = household_without_digest(&db).await;
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
    let household = household_without_digest(&db).await;
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn all_day_reminder_fires_at_user_local_nine_and_only_on_due_day() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let vladivostok = chrono_tz::Asia::Vladivostok;
    set_household_timezone(&db.pool, household.household_id, "Europe/Moscow").await;
    set_user_timezone(&db.pool, household.owner_id, "Asia/Vladivostok").await;
    set_notifications(
        &db.pool,
        household.owner_id,
        json!({"all_day_time": "09:00"}),
    )
    .await;

    let due_date = Utc::now().with_timezone(&vladivostok).date_naive() + Duration::days(2);
    let item = create_all_day_task(
        &db,
        &household,
        "Сдать отчёт",
        due_date,
        Some(household.owner_id),
    )
    .await;
    let chat = chat_id(&db.pool, household.owner_id).await;
    let scheduler = scheduler(&db.pool);

    let night = local_utc(vladivostok, due_date, 2, 0);
    assert_eq!(
        scheduler.tick(night).await.expect("night tick").delivered,
        0
    );
    assert!(messages(&db.pool, chat).await.is_empty());

    let early = local_utc(vladivostok, due_date, 8, 59);
    assert_eq!(
        scheduler.tick(early).await.expect("early tick").delivered,
        0
    );
    assert!(messages(&db.pool, chat).await.is_empty());

    let on_time = local_utc(vladivostok, due_date, 9, 0);
    let stats = scheduler.tick(on_time).await.expect("on-time tick");
    assert_eq!(stats.planned, 1);
    assert_eq!(stats.delivered, 1);
    let sent = messages(&db.pool, chat).await;
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert!(sent[0].contains("Сдать отчёт"), "{sent:?}");
    assert!(sent[0].contains("Сегодня"), "{sent:?}");

    let rows = delivery_rows(&db.pool, item.id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "sent");
    assert_eq!(rows[0].1, all_day_occurrence_key(due_date));

    let next_day = local_utc(vladivostok, due_date + Duration::days(1), 9, 0);
    assert_eq!(
        scheduler
            .tick(next_day)
            .await
            .expect("next-day tick")
            .delivered,
        0
    );
    assert_eq!(messages(&db.pool, chat).await.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn all_day_reminder_is_not_created_on_the_next_day_if_the_service_missed_it() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let moscow = chrono_tz::Europe::Moscow;
    let due_date = Utc::now().with_timezone(&moscow).date_naive();
    set_notifications(
        &db.pool,
        household.owner_id,
        json!({"all_day_time": "09:00"}),
    )
    .await;

    let item = create_all_day_task(
        &db,
        &household,
        "Пропущенный день",
        due_date,
        Some(household.owner_id),
    )
    .await;
    let chat = chat_id(&db.pool, household.owner_id).await;

    let next_day = local_utc(moscow, due_date + Duration::days(1), 9, 0);
    let stats = scheduler(&db.pool)
        .tick(next_day)
        .await
        .expect("missed tick");
    assert_eq!(stats.planned, 0);
    assert_eq!(stats.delivered, 0);
    assert!(messages(&db.pool, chat).await.is_empty());
    assert!(delivery_rows(&db.pool, item.id).await.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pending_all_day_delivery_expires_at_the_end_of_the_due_day() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let moscow = chrono_tz::Europe::Moscow;
    let due_date = Utc::now().with_timezone(&moscow).date_naive();
    let item = create_all_day_task(
        &db,
        &household,
        "Просроченное напоминание",
        due_date,
        Some(household.owner_id),
    )
    .await;
    insert_delivery(
        &db.pool,
        household.household_id,
        household.owner_id,
        Some(item.id),
        &all_day_occurrence_key(due_date),
        "pending",
        local_utc(moscow, due_date, 9, 0),
    )
    .await
    .expect("pending all-day delivery");
    let chat = chat_id(&db.pool, household.owner_id).await;

    let next_day = local_utc(moscow, due_date + Duration::days(1), 9, 0);
    let stats = scheduler(&db.pool)
        .tick(next_day)
        .await
        .expect("expired tick");
    assert_eq!(stats.cancelled, 1);
    assert_eq!(stats.delivered, 0);
    assert!(messages(&db.pool, chat).await.is_empty());
    let rows = delivery_rows(&db.pool, item.id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "cancelled");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn all_day_task_created_after_reminder_time_still_fires_same_day() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let moscow = chrono_tz::Europe::Moscow;
    let due_date = Utc::now().with_timezone(&moscow).date_naive();
    set_notifications(
        &db.pool,
        household.owner_id,
        json!({"all_day_time": "09:00"}),
    )
    .await;

    let item = create_all_day_task(
        &db,
        &household,
        "Позвонить врачу",
        due_date,
        Some(household.owner_id),
    )
    .await;
    let chat = chat_id(&db.pool, household.owner_id).await;

    let now = local_utc(moscow, due_date, 15, 0);
    let stats = scheduler(&db.pool).tick(now).await.expect("afternoon tick");
    assert_eq!(stats.planned, 1);
    assert_eq!(stats.delivered, 1);
    let sent = messages(&db.pool, chat).await;
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert!(sent[0].contains("Позвонить врачу"), "{sent:?}");
    assert!(sent[0].contains("Сегодня"), "{sent:?}");
    assert_eq!(delivery_rows(&db.pool, item.id).await[0].0, "sent");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_without_assignee_or_author_notifies_every_enabled_member_once() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let extra_id = add_member(&db.pool, household.household_id, "Extra member").await;
    set_notifications(
        &db.pool,
        household.member_id,
        json!({"reminders_enabled": false}),
    )
    .await;

    let due = rounded_now() + Duration::hours(3);
    let item = create_task(&db, &household, "Голосовая задача", Some(due), None, None).await;
    clear_author(&db.pool, item.id).await;

    let owner_chat = chat_id(&db.pool, household.owner_id).await;
    let member_chat = chat_id(&db.pool, household.member_id).await;
    let extra_chat = chat_id(&db.pool, extra_id).await;
    let scheduler = scheduler(&db.pool);
    let now = due - Duration::minutes(15);

    let stats = scheduler.tick(now).await.expect("fallback tick");
    assert_eq!(stats.planned, 2);
    assert_eq!(stats.delivered, 2);
    assert_eq!(messages(&db.pool, owner_chat).await.len(), 1);
    assert_eq!(messages(&db.pool, extra_chat).await.len(), 1);
    assert!(messages(&db.pool, member_chat).await.is_empty());

    assert_eq!(scheduler.tick(now).await.expect("repeat tick").delivered, 0);
    assert_eq!(messages(&db.pool, owner_chat).await.len(), 1);
    assert_eq!(messages(&db.pool, extra_chat).await.len(), 1);
    assert!(messages(&db.pool, member_chat).await.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn changing_assignee_cancels_old_delivery_and_notifies_new_assignee() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let due = rounded_now() + Duration::hours(3);
    let item = create_task(
        &db,
        &household,
        "Передать задачу",
        Some(due),
        Some(household.owner_id),
        None,
    )
    .await;
    let now = due - Duration::minutes(15);
    insert_delivery(
        &db.pool,
        household.household_id,
        household.owner_id,
        Some(item.id),
        &occurrence_key(due),
        "pending",
        now,
    )
    .await
    .expect("old assignee delivery");
    sqlx::query("UPDATE items SET assignee_user_id = $2 WHERE id = $1")
        .bind(item.id)
        .bind(household.member_id)
        .execute(&db.pool)
        .await
        .expect("reassign item");

    let owner_chat = chat_id(&db.pool, household.owner_id).await;
    let member_chat = chat_id(&db.pool, household.member_id).await;
    let stats = scheduler(&db.pool).tick(now).await.expect("reassign tick");
    assert_eq!(stats.cancelled, 1);
    assert_eq!(stats.planned, 1);
    assert_eq!(stats.delivered, 1);
    assert_eq!(
        delivery_status(&db.pool, item.id, household.owner_id)
            .await
            .as_deref(),
        Some("cancelled")
    );
    assert_eq!(
        delivery_status(&db.pool, item.id, household.member_id)
            .await
            .as_deref(),
        Some("sent")
    );
    assert!(messages(&db.pool, owner_chat).await.is_empty());
    assert_eq!(messages(&db.pool, member_chat).await.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn changing_due_at_cancels_pending_delivery_for_the_old_due() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let due = rounded_now() + Duration::hours(3);
    let item = create_task(
        &db,
        &household,
        "Перенести встречу",
        Some(due),
        Some(household.owner_id),
        None,
    )
    .await;
    let now = due - Duration::minutes(15);
    insert_delivery(
        &db.pool,
        household.household_id,
        household.owner_id,
        Some(item.id),
        &occurrence_key(due),
        "pending",
        now,
    )
    .await
    .expect("old due delivery");
    sqlx::query("UPDATE items SET due_at = $2 WHERE id = $1")
        .bind(item.id)
        .bind(due + Duration::hours(2))
        .execute(&db.pool)
        .await
        .expect("move due");

    let chat = chat_id(&db.pool, household.owner_id).await;
    let stats = scheduler(&db.pool).tick(now).await.expect("moved tick");
    assert_eq!(stats.cancelled, 1);
    assert_eq!(stats.delivered, 0);
    assert!(messages(&db.pool, chat).await.is_empty());
    let rows = delivery_rows(&db.pool, item.id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "cancelled");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lead_time_of_one_hour_shifts_the_delivery_moment() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    set_notifications(&db.pool, household.owner_id, json!({"lead_minutes": 60})).await;
    let due = rounded_now() + Duration::hours(3);
    create_task(
        &db,
        &household,
        "За час",
        Some(due),
        Some(household.owner_id),
        None,
    )
    .await;
    let chat = chat_id(&db.pool, household.owner_id).await;
    let scheduler = scheduler(&db.pool);

    assert_eq!(
        scheduler
            .tick(due - Duration::minutes(61))
            .await
            .expect("early")
            .delivered,
        0
    );
    assert!(messages(&db.pool, chat).await.is_empty());
    assert_eq!(
        scheduler
            .tick(due - Duration::minutes(60))
            .await
            .expect("on time")
            .delivered,
        1
    );
    assert_eq!(messages(&db.pool, chat).await.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reminder_text_uses_user_timezone_and_escapes_the_title() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    set_household_timezone(&db.pool, household.household_id, "Europe/Moscow").await;
    set_user_timezone(&db.pool, household.owner_id, "Asia/Vladivostok").await;
    let date = (Utc::now() + Duration::days(2)).date_naive();
    let due = Utc
        .with_ymd_and_hms(date.year(), date.month(), date.day(), 10, 0, 0)
        .single()
        .expect("due");
    create_task(
        &db,
        &household,
        "<b>Важно</b>",
        Some(due),
        Some(household.owner_id),
        None,
    )
    .await;
    let chat = chat_id(&db.pool, household.owner_id).await;

    assert_eq!(
        scheduler(&db.pool)
            .tick(due - Duration::minutes(15))
            .await
            .expect("tick")
            .delivered,
        1
    );
    let sent = messages(&db.pool, chat).await;
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert!(sent[0].contains("20:00"), "{sent:?}");
    assert!(!sent[0].contains("13:00"), "{sent:?}");
    assert!(sent[0].contains("&lt;b&gt;Важно&lt;/b&gt;"), "{sent:?}");
    assert!(!sent[0].contains("<b>Важно</b>"), "{sent:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn existing_outbox_row_marks_delivery_sent_without_a_second_message() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let due = rounded_now() + Duration::hours(3);
    let item = create_task(
        &db,
        &household,
        "Уже в очереди",
        Some(due),
        Some(household.owner_id),
        None,
    )
    .await;
    let now = due - Duration::minutes(15);
    let delivery_id = insert_delivery(
        &db.pool,
        household.household_id,
        household.owner_id,
        Some(item.id),
        &occurrence_key(due),
        "pending",
        now,
    )
    .await
    .expect("delivery");
    let chat = chat_id(&db.pool, household.owner_id).await;
    let existing_seq = insert_outbox(
        &db.pool,
        chat,
        &format!("notification:reminder:{delivery_id}"),
    )
    .await;

    let stats = scheduler(&db.pool).tick(now).await.expect("tick");
    assert_eq!(stats.delivered, 1);
    assert_eq!(messages(&db.pool, chat).await.len(), 1);
    let rows = delivery_rows(&db.pool, item.id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "sent");
    assert_eq!(rows[0].2, Some(existing_seq));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_delivery_is_revived_when_the_same_due_becomes_relevant_again() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let due = rounded_now() + Duration::hours(3);
    let item = create_task(
        &db,
        &household,
        "Вернуть задачу",
        Some(due),
        Some(household.owner_id),
        None,
    )
    .await;
    insert_delivery(
        &db.pool,
        household.household_id,
        household.owner_id,
        Some(item.id),
        &occurrence_key(due),
        "pending",
        due - Duration::minutes(15),
    )
    .await
    .expect("pending delivery");
    let chat = chat_id(&db.pool, household.owner_id).await;
    let scheduler = scheduler(&db.pool);
    let mut conn = db.pool.acquire().await.expect("conn");

    items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        item.id,
        None,
        "Europe/Moscow",
        due - Duration::minutes(30),
    )
    .await
    .expect("complete");
    let stats = scheduler
        .tick(due - Duration::minutes(15))
        .await
        .expect("cancel tick");
    assert_eq!(stats.cancelled, 1);
    assert_eq!(stats.delivered, 0);
    let rows = delivery_rows(&db.pool, item.id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "cancelled");
    assert!(messages(&db.pool, chat).await.is_empty());

    items::reopen(
        &mut conn,
        household.household_id,
        &household.owner(),
        item.id,
        None,
        due - Duration::minutes(10),
    )
    .await
    .expect("reopen");
    let stats = scheduler
        .tick(due - Duration::minutes(9))
        .await
        .expect("revive tick");
    assert_eq!(stats.planned, 1);
    assert_eq!(stats.delivered, 1);
    let rows = delivery_rows(&db.pool, item.id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "sent");
    assert_eq!(messages(&db.pool, chat).await.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn selected_assignee_with_disabled_reminders_notifies_nobody() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    set_notifications(
        &db.pool,
        household.member_id,
        json!({"reminders_enabled": false}),
    )
    .await;
    let due = rounded_now() + Duration::hours(3);
    let item = create_task(
        &db,
        &household,
        "Исполнитель без напоминаний",
        Some(due),
        Some(household.member_id),
        None,
    )
    .await;
    let owner_chat = chat_id(&db.pool, household.owner_id).await;
    let member_chat = chat_id(&db.pool, household.member_id).await;

    let stats = scheduler(&db.pool)
        .tick(due - Duration::minutes(15))
        .await
        .expect("tick");
    assert_eq!(stats.planned, 0);
    assert_eq!(stats.delivered, 0);
    assert!(messages(&db.pool, owner_chat).await.is_empty());
    assert!(messages(&db.pool, member_chat).await.is_empty());
    assert!(delivery_rows(&db.pool, item.id).await.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deleted_list_cancels_pending_delivery() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let due = rounded_now() + Duration::hours(3);
    let item = create_task(
        &db,
        &household,
        "Задача в удалённом списке",
        Some(due),
        Some(household.owner_id),
        None,
    )
    .await;
    insert_delivery(
        &db.pool,
        household.household_id,
        household.owner_id,
        Some(item.id),
        &occurrence_key(due),
        "pending",
        due - Duration::minutes(15),
    )
    .await
    .expect("pending delivery");
    sqlx::query("UPDATE lists SET deleted_at = now() WHERE id = $1")
        .bind(household.tasks_list_id)
        .execute(&db.pool)
        .await
        .expect("delete list");
    let chat = chat_id(&db.pool, household.owner_id).await;

    let stats = scheduler(&db.pool)
        .tick(due - Duration::minutes(15))
        .await
        .expect("tick");
    assert_eq!(stats.cancelled, 1);
    assert_eq!(stats.delivered, 0);
    assert!(messages(&db.pool, chat).await.is_empty());
    let rows = delivery_rows(&db.pool, item.id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "cancelled");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deleted_household_cancels_pending_delivery() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let due = rounded_now() + Duration::hours(3);
    let item = create_task(
        &db,
        &household,
        "Задача в удалённом household",
        Some(due),
        Some(household.owner_id),
        None,
    )
    .await;
    insert_delivery(
        &db.pool,
        household.household_id,
        household.owner_id,
        Some(item.id),
        &occurrence_key(due),
        "pending",
        due - Duration::minutes(15),
    )
    .await
    .expect("pending delivery");
    sqlx::query("UPDATE households SET deleted_at = now() WHERE id = $1")
        .bind(household.household_id)
        .execute(&db.pool)
        .await
        .expect("delete household");
    let chat = chat_id(&db.pool, household.owner_id).await;

    let stats = scheduler(&db.pool)
        .tick(due - Duration::minutes(15))
        .await
        .expect("tick");
    assert_eq!(stats.cancelled, 1);
    assert_eq!(stats.delivered, 0);
    assert!(messages(&db.pool, chat).await.is_empty());
    let rows = delivery_rows(&db.pool, item.id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "cancelled");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clearing_assignee_cancels_old_delivery_and_notifies_the_author() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let due = rounded_now() + Duration::hours(3);
    let item = create_task(
        &db,
        &household,
        "Вернуть автору",
        Some(due),
        Some(household.member_id),
        None,
    )
    .await;
    insert_delivery(
        &db.pool,
        household.household_id,
        household.member_id,
        Some(item.id),
        &occurrence_key(due),
        "pending",
        due - Duration::minutes(15),
    )
    .await
    .expect("assignee delivery");
    sqlx::query("UPDATE items SET assignee_user_id = NULL WHERE id = $1")
        .bind(item.id)
        .execute(&db.pool)
        .await
        .expect("clear assignee");
    let owner_chat = chat_id(&db.pool, household.owner_id).await;
    let member_chat = chat_id(&db.pool, household.member_id).await;

    let stats = scheduler(&db.pool)
        .tick(due - Duration::minutes(15))
        .await
        .expect("tick");
    assert_eq!(stats.cancelled, 1);
    assert_eq!(stats.delivered, 1);
    assert_eq!(
        delivery_status(&db.pool, item.id, household.member_id)
            .await
            .as_deref(),
        Some("cancelled")
    );
    assert_eq!(
        delivery_status(&db.pool, item.id, household.owner_id)
            .await
            .as_deref(),
        Some("sent")
    );
    assert_eq!(messages(&db.pool, owner_chat).await.len(), 1);
    assert!(messages(&db.pool, member_chat).await.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn soft_deleted_assignee_is_not_replaced_by_the_author() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let due = rounded_now() + Duration::hours(3);
    let item = create_task(
        &db,
        &household,
        "Удалённый исполнитель",
        Some(due),
        Some(household.member_id),
        None,
    )
    .await;
    sqlx::query("UPDATE users SET deleted_at = now() WHERE id = $1")
        .bind(household.member_id)
        .execute(&db.pool)
        .await
        .expect("delete assignee");
    let owner_chat = chat_id(&db.pool, household.owner_id).await;
    let member_chat = chat_id(&db.pool, household.member_id).await;

    let stats = scheduler(&db.pool)
        .tick(due - Duration::minutes(15))
        .await
        .expect("tick");
    assert_eq!(stats.planned, 0);
    assert_eq!(stats.delivered, 0);
    assert!(messages(&db.pool, owner_chat).await.is_empty());
    assert!(messages(&db.pool, member_chat).await.is_empty());
    assert!(delivery_rows(&db.pool, item.id).await.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delivery_for_a_deleted_user_is_cancelled() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let due = rounded_now() + Duration::hours(3);
    let item = create_task(
        &db,
        &household,
        "Удалённый получатель",
        Some(due),
        Some(household.owner_id),
        None,
    )
    .await;
    insert_delivery(
        &db.pool,
        household.household_id,
        household.owner_id,
        Some(item.id),
        &occurrence_key(due),
        "pending",
        due - Duration::minutes(15),
    )
    .await
    .expect("pending delivery");
    sqlx::query("UPDATE users SET deleted_at = now() WHERE id = $1")
        .bind(household.owner_id)
        .execute(&db.pool)
        .await
        .expect("delete user");
    let chat = chat_id(&db.pool, household.owner_id).await;

    let stats = scheduler(&db.pool)
        .tick(due - Duration::minutes(15))
        .await
        .expect("tick");
    assert_eq!(stats.cancelled, 1);
    assert_eq!(stats.delivered, 0);
    assert!(messages(&db.pool, chat).await.is_empty());
    let rows = delivery_rows(&db.pool, item.id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "cancelled");
}

fn digest_day() -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 10, 30).expect("digest day")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn digest_fires_at_local_digest_time_once_and_next_day_gets_a_new_key() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = digest_day();
    set_household_timezone(&db.pool, household.household_id, "Europe/Moscow").await;
    enable_digest(&db.pool, household.owner_id).await;
    set_notifications(
        &db.pool,
        household.member_id,
        json!({"digest_enabled": false}),
    )
    .await;

    create_task(
        &db,
        &household,
        "Оплатить интернет",
        Some(local_utc(moscow, day, 18, 0)),
        Some(household.owner_id),
        None,
    )
    .await;
    create_all_day_task(
        &db,
        &household,
        "Сдать отчёт",
        day,
        Some(household.owner_id),
    )
    .await;
    create_task(
        &db,
        &household,
        "Просроченная задача",
        Some(local_utc(moscow, day - Duration::days(1), 12, 0)),
        Some(household.owner_id),
        None,
    )
    .await;
    create_task(
        &db,
        &household,
        "Утренняя задача",
        Some(local_utc(moscow, day, 7, 0)),
        Some(household.owner_id),
        None,
    )
    .await;
    create_shopping(&db, &household, "Молоко").await;
    create_shopping(&db, &household, "Хлеб").await;
    create_shopping(&db, &household, "Сыр").await;
    let done = create_shopping(&db, &household, "Куплено").await;
    let mut conn = db.pool.acquire().await.expect("conn");
    items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        done.id,
        None,
        "Europe/Moscow",
        local_utc(moscow, day, 7, 0),
    )
    .await
    .expect("complete shopping item");

    let chat = chat_id(&db.pool, household.owner_id).await;
    let scheduler = scheduler(&db.pool);

    let early = scheduler
        .tick(local_utc(moscow, day, 8, 29))
        .await
        .expect("early tick");
    assert_eq!(early.delivered, 0);
    assert!(messages(&db.pool, chat).await.is_empty());

    let on_time = scheduler
        .tick(local_utc(moscow, day, 8, 30))
        .await
        .expect("digest tick");
    assert_eq!(on_time.delivered, 1);

    let sent = messages(&db.pool, chat).await;
    assert_eq!(sent.len(), 1, "{sent:?}");
    let html = &sent[0];
    assert!(html.contains("Сегодня"), "{html}");
    assert!(html.contains("Просрочено"), "{html}");
    assert!(html.contains("Оплатить интернет"), "{html}");
    assert!(html.contains("Сдать отчёт"), "{html}");
    assert!(html.contains("Просроченная задача"), "{html}");
    assert!(html.contains("Утренняя задача"), "{html}");
    let overdue_at = html.find("<b>Просрочено</b>").expect("overdue heading");
    let morning_at = html.find("Утренняя задача").expect("morning task");
    assert!(morning_at > overdue_at, "{html}");
    assert!(html.contains("Покупки: 3 позиции"), "{html}");
    assert!(!html.contains("Куплено"), "{html}");

    let rows = digest_rows(&db.pool, household.owner_id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, format!("digest:{day}"));
    assert_eq!(rows[0].1, "sent");
    assert_eq!(rows[0].2, None);
    assert!(rows[0].3.is_some());

    assert_eq!(
        scheduler
            .tick(local_utc(moscow, day, 8, 30))
            .await
            .expect("repeat tick")
            .delivered,
        0
    );
    assert_eq!(messages(&db.pool, chat).await.len(), 1);

    let next_day = day + Duration::days(1);
    let next = scheduler
        .tick(local_utc(moscow, next_day, 8, 30))
        .await
        .expect("next-day tick");
    assert_eq!(next.delivered, 1);
    assert_eq!(messages(&db.pool, chat).await.len(), 2);

    let rows = digest_rows(&db.pool, household.owner_id).await;
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[0].0, format!("digest:{day}"));
    assert_eq!(rows[1].0, format!("digest:{next_day}"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn digest_uses_the_user_digest_time_and_timezone() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let vladivostok = chrono_tz::Asia::Vladivostok;
    let day = digest_day();
    set_household_timezone(&db.pool, household.household_id, "Europe/Moscow").await;
    set_user_timezone(&db.pool, household.owner_id, "Asia/Vladivostok").await;
    set_notifications(
        &db.pool,
        household.owner_id,
        json!({"digest_enabled": true, "digest_time": "07:15"}),
    )
    .await;

    let chat = chat_id(&db.pool, household.owner_id).await;
    let scheduler = scheduler(&db.pool);

    let early = scheduler
        .tick(local_utc(vladivostok, day, 7, 14))
        .await
        .expect("early tick");
    assert_eq!(early.delivered, 0);
    assert!(messages(&db.pool, chat).await.is_empty());

    let on_time = scheduler
        .tick(local_utc(vladivostok, day, 7, 15))
        .await
        .expect("digest tick");
    assert_eq!(on_time.delivered, 1);
    assert_eq!(messages(&db.pool, chat).await.len(), 1);

    let rows = digest_rows(&db.pool, household.owner_id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, format!("digest:{day}"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_digest_says_the_day_is_free() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = digest_day();
    enable_digest(&db.pool, household.owner_id).await;
    let chat = chat_id(&db.pool, household.owner_id).await;

    let stats = scheduler(&db.pool)
        .tick(local_utc(moscow, day, 8, 30))
        .await
        .expect("digest tick");
    assert_eq!(stats.delivered, 1);

    let sent = messages(&db.pool, chat).await;
    assert_eq!(sent.len(), 1, "{sent:?}");
    let html = &sent[0];
    assert!(html.contains("День свободен."), "{html}");
    assert!(!html.contains("Сегодня"), "{html}");
    assert!(!html.contains("Просрочено"), "{html}");
    assert!(!html.contains("Покупки:"), "{html}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disabled_digest_is_not_sent_and_cancels_a_pending_delivery() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = digest_day();
    let key = format!("digest:{day}");
    insert_digest_delivery(
        &db.pool,
        household.household_id,
        household.member_id,
        &key,
        local_utc(moscow, day, 8, 30),
    )
    .await;
    set_notifications(
        &db.pool,
        household.member_id,
        json!({"digest_enabled": false}),
    )
    .await;
    enable_digest(&db.pool, household.owner_id).await;

    let owner_chat = chat_id(&db.pool, household.owner_id).await;
    let member_chat = chat_id(&db.pool, household.member_id).await;
    let stats = scheduler(&db.pool)
        .tick(local_utc(moscow, day, 8, 30))
        .await
        .expect("digest tick");
    assert_eq!(stats.cancelled, 1);
    assert_eq!(stats.delivered, 1);
    assert_eq!(messages(&db.pool, owner_chat).await.len(), 1);
    assert!(messages(&db.pool, member_chat).await.is_empty());

    let rows = digest_rows(&db.pool, household.member_id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, key);
    assert_eq!(rows[0].1, "cancelled");
    assert_eq!(rows[0].2, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn digest_shows_twenty_task_lines_and_a_show_more_button() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = digest_day();
    enable_digest(&db.pool, household.owner_id).await;
    for n in 0..25 {
        create_all_day_task(
            &db,
            &household,
            &format!("Задача {n}"),
            day,
            Some(household.owner_id),
        )
        .await;
    }
    let chat = chat_id(&db.pool, household.owner_id).await;

    let stats = scheduler(&db.pool)
        .tick(local_utc(moscow, day, 8, 30))
        .await
        .expect("digest tick");
    assert_eq!(stats.delivered, 1);

    let sent = messages(&db.pool, chat).await;
    assert_eq!(sent.len(), 1, "{sent:?}");
    let html = &sent[0];
    assert_eq!(html.matches("☐ ").count(), 20, "{html}");
    assert!(html.contains("…и ещё 5 задач"), "{html}");
    assert!(html.contains("Показать ещё"), "{html}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn voice_task_without_author_is_included_exactly_once() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = digest_day();
    enable_digest(&db.pool, household.owner_id).await;
    let item = create_task(
        &db,
        &household,
        "Голосовая задача",
        Some(local_utc(moscow, day, 18, 0)),
        None,
        None,
    )
    .await;
    clear_author(&db.pool, item.id).await;
    let chat = chat_id(&db.pool, household.owner_id).await;

    let stats = scheduler(&db.pool)
        .tick(local_utc(moscow, day, 8, 30))
        .await
        .expect("digest tick");
    assert_eq!(stats.delivered, 1);

    let sent = messages(&db.pool, chat).await;
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(
        sent[0].matches("Голосовая задача").count(),
        1,
        "{}",
        sent[0]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn digest_skips_completed_deleted_and_other_household_tasks() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let other = household_without_digest(&db).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = digest_day();
    for user_id in [household.member_id, other.owner_id, other.member_id] {
        set_notifications(&db.pool, user_id, json!({"digest_enabled": false})).await;
    }
    enable_digest(&db.pool, household.owner_id).await;
    create_task(
        &db,
        &household,
        "Видимая задача",
        Some(local_utc(moscow, day, 18, 0)),
        Some(household.owner_id),
        None,
    )
    .await;
    let completed = create_task(
        &db,
        &household,
        "Выполненная задача",
        Some(local_utc(moscow, day, 19, 0)),
        Some(household.owner_id),
        None,
    )
    .await;
    let deleted = create_task(
        &db,
        &household,
        "Удалённая задача",
        Some(local_utc(moscow, day, 20, 0)),
        Some(household.owner_id),
        None,
    )
    .await;
    create_task(
        &db,
        &other,
        "Чужая задача",
        Some(local_utc(moscow, day, 18, 0)),
        Some(other.owner_id),
        None,
    )
    .await;

    let mut conn = db.pool.acquire().await.expect("conn");
    items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        completed.id,
        None,
        "Europe/Moscow",
        local_utc(moscow, day, 7, 0),
    )
    .await
    .expect("complete task");
    items::soft_delete(
        &mut conn,
        household.household_id,
        &household.owner(),
        deleted.id,
        local_utc(moscow, day, 7, 30),
    )
    .await
    .expect("delete task");

    let chat = chat_id(&db.pool, household.owner_id).await;
    let stats = scheduler(&db.pool)
        .tick(local_utc(moscow, day, 8, 30))
        .await
        .expect("digest tick");
    assert_eq!(stats.delivered, 1);

    let sent = messages(&db.pool, chat).await;
    assert_eq!(sent.len(), 1, "{sent:?}");
    let html = &sent[0];
    assert!(html.contains("Видимая задача"), "{html}");
    assert!(!html.contains("Выполненная задача"), "{html}");
    assert!(!html.contains("Удалённая задача"), "{html}");
    assert!(!html.contains("Чужая задача"), "{html}");
}
