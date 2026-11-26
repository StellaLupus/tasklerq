//! Планировщик напоминаний: время, дедупликация, отмена и получатель.
//!
//! Время передаётся в тик явно, поэтому тесты не ждут 30-секундный интервал.

mod support;

use std::sync::Arc;
use std::time::{Duration as StdDuration, Instant};

use axum::http::{Method, StatusCode};
use chrono::{DateTime, Datelike, Duration, NaiveDate, TimeZone, Timelike, Utc};
use chrono_tz::Tz;
use serde_json::json;
use sqlx::{Acquire, PgPool};
use support::notifications::{
    button_of, digest_delivery_id, household_for, local_today, onboarded, quiet_off,
    tick_and_deliver,
};
use support::telegram::{Call, Harness, buttons};
use support::{TestDb, TestHousehold, new_member, next_telegram_id};
use tasklerq::domain::models::{
    Actor, ActorType, Field, Item, ItemPatch, ItemPriority, ItemSource, ListKind, RecurrenceFreq,
    RecurrenceInput, UserRole,
};
use tasklerq::domain::recurrence::Due;
use tasklerq::domain::settings::{DigestScope, NotificationSettingsUpdate, ProfileUpdate};
use tasklerq::jobs::digest::{DigestSection, DigestSnapshotItem, request_manual};
use tasklerq::jobs::domain::DomainTicker;
use tasklerq::jobs::scheduler::{
    Scheduler, TickStats, all_day_occurrence_key, occurrence_key, snooze_origin,
};
use tasklerq::storage::repositories::notifications::{self, NewDelivery, NotificationKind};
use tasklerq::storage::repositories::{items, lists, members, users};
use tasklerq::telegram::api::TelegramError;
use tasklerq::telegram::outbox::OutboxWorker;
use tokio::sync::Notify;
use uuid::Uuid;

fn scheduler(pool: &PgPool) -> Scheduler {
    Scheduler::new(pool.clone(), Arc::new(Notify::new()))
}

fn ticker(pool: &PgPool) -> DomainTicker {
    DomainTicker::new(pool.clone())
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

async fn create_event(
    db: &TestDb,
    household: &TestHousehold,
    title: &str,
    due_at: DateTime<Utc>,
    end_at: DateTime<Utc>,
) -> Item {
    let mut item = support::new_item(household.events_list_id, title);
    item.due_at = Some(due_at);
    item.end_at = Some(end_at);
    support::create_item(
        &db.pool,
        household,
        household.events_list_id,
        &household.owner(),
        item,
    )
    .await
}

async fn create_recurring_event(
    db: &TestDb,
    household: &TestHousehold,
    title: &str,
    due_at: DateTime<Utc>,
    end_at: DateTime<Utc>,
    recurrence: RecurrenceInput,
) -> Item {
    let mut item = support::new_item(household.events_list_id, title);
    item.due_at = Some(due_at);
    item.end_at = Some(end_at);
    item.recurrence = Some(recurrence);
    support::create_item(
        &db.pool,
        household,
        household.events_list_id,
        &household.owner(),
        item,
    )
    .await
}

async fn create_all_day_event(
    db: &TestDb,
    household: &TestHousehold,
    title: &str,
    due_date: NaiveDate,
    end_date: Option<NaiveDate>,
) -> Item {
    let mut item = support::new_item(household.events_list_id, title);
    item.due_date = Some(due_date);
    item.end_date = end_date;
    support::create_item(
        &db.pool,
        household,
        household.events_list_id,
        &household.owner(),
        item,
    )
    .await
}

/// Назначить событие в обход уведомления о назначении: тесты напоминаний
/// проверяют выбор получателя, а не отдельную доставку назначения.
async fn assign_without_notice(pool: &PgPool, item_id: Uuid, user_id: Uuid) {
    sqlx::query("UPDATE items SET assignee_user_id = $2 WHERE id = $1")
        .bind(item_id)
        .bind(user_id)
        .execute(pool)
        .await
        .expect("raw assignee");
}

/// Создать задачу с исполнителем в обход уведомления о назначении.
///
/// Тесты напоминаний проверяют выбор получателя, а не доставку назначения:
/// прямая запись `assignee_user_id` не создаёт запись `assignment`.
async fn create_task_without_assignment_notice(
    db: &TestDb,
    household: &TestHousehold,
    title: &str,
    due_at: Option<DateTime<Utc>>,
    assignee: Uuid,
    recurrence: Option<RecurrenceInput>,
) -> Item {
    let item = create_task(db, household, title, due_at, None, recurrence).await;
    sqlx::query("UPDATE items SET assignee_user_id = $2 WHERE id = $1")
        .bind(item.id)
        .bind(assignee)
        .execute(&db.pool)
        .await
        .expect("raw assignee");
    let mut conn = db.pool.acquire().await.expect("conn");
    items::get(&mut conn, household.household_id, item.id)
        .await
        .expect("assigned item")
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

async fn add_member(pool: &PgPool, household_id: Uuid, name: &str, digest_enabled: bool) -> Uuid {
    let id = Uuid::now_v7();
    let telegram_id = support::next_telegram_id();
    sqlx::query(
        "INSERT INTO users (
             id, household_id, telegram_user_id, telegram_chat_id, display_name,
             role, settings, created_at, updated_at
         )
         VALUES (
             $1, $2, $3, $3, $4, 'member',
             jsonb_build_object(
                 'notifications',
                 jsonb_build_object('digest_enabled', $5, 'quiet_hours_enabled', false)
             ),
             now(), now()
         )",
    )
    .bind(id)
    .bind(household_id)
    .bind(telegram_id)
    .bind(name)
    .bind(digest_enabled)
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
         WHERE item_id = $1 AND user_id = $2 AND kind = 'reminder'",
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
         FROM notification_deliveries WHERE item_id = $1 AND kind = 'reminder'
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

/// Снимок и отпечаток последней доставленной сводки получателя.
async fn digest_snapshot_and_hash(
    pool: &PgPool,
    user_id: Uuid,
) -> (Vec<DigestSnapshotItem>, String) {
    let (items, hash): (Option<serde_json::Value>, Option<String>) = sqlx::query_as(
        "SELECT digest_items, digest_hash
         FROM notification_deliveries
         WHERE user_id = $1 AND kind = 'digest'
         ORDER BY created_at DESC, id DESC LIMIT 1",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await
    .expect("digest delivery");
    let snapshot =
        serde_json::from_value(items.expect("digest_items written")).expect("snapshot reads");
    (snapshot, hash.expect("digest_hash written"))
}

async fn assignment_rows(
    pool: &PgPool,
    item_id: Uuid,
    user_id: Uuid,
) -> Vec<(String, String, Option<i64>)> {
    sqlx::query_as(
        "SELECT status::text, occurrence_key, outbox_seq
         FROM notification_deliveries
         WHERE item_id = $1 AND user_id = $2 AND kind = 'assignment'
         ORDER BY created_at, id",
    )
    .bind(item_id)
    .bind(user_id)
    .fetch_all(pool)
    .await
    .expect("assignment deliveries")
}

async fn update_assignee(
    db: &TestDb,
    household: &TestHousehold,
    actor: &Actor,
    item: &Item,
    assignee: Option<Uuid>,
    now: DateTime<Utc>,
) -> Item {
    let patch = ItemPatch {
        assignee_user_id: match assignee {
            Some(user_id) => Field::Value(user_id),
            None => Field::Null,
        },
        ..ItemPatch::default()
    };
    let mut conn = db.pool.acquire().await.expect("conn");
    items::update(
        &mut conn,
        household.household_id,
        actor,
        item.id,
        item.version,
        patch,
        "Europe/Moscow",
        now,
    )
    .await
    .expect("assignee update")
}

async fn move_due(
    db: &TestDb,
    household: &TestHousehold,
    actor: &Actor,
    item: &Item,
    due_at: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Item {
    let patch = ItemPatch {
        due_at: Field::Value(due_at),
        ..ItemPatch::default()
    };
    let mut conn = db.pool.acquire().await.expect("conn");
    items::update(
        &mut conn,
        household.household_id,
        actor,
        item.id,
        item.version,
        patch,
        "Europe/Moscow",
        now,
    )
    .await
    .expect("due update")
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
        // Тихие часы выключены: большинство тестов использует реальное время
        // запуска, а сценарии тихих часов задают границы и `now` явно.
        set_notifications(
            &db.pool,
            user_id,
            json!({"digest_enabled": false, "quiet_hours_enabled": false}),
        )
        .await;
    }
    household
}

async fn enable_digest(pool: &PgPool, user_id: Uuid) {
    set_notifications(pool, user_id, json!({"digest_enabled": true})).await;
}

async fn set_notifications(pool: &PgPool, user_id: Uuid, value: serde_json::Value) {
    // Настройки заменяются ровно переданным значением: если тесту нужна
    // выключенная сводка, это должно быть видно в самом вызове.
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

/// Состав сводки участника через patch настроек, как на экране уведомлений.
async fn set_digest_scope(pool: &PgPool, household_id: Uuid, user_id: Uuid, scope: DigestScope) {
    let mut tx = pool.begin().await.expect("tx");
    users::update_profile(
        &mut tx,
        household_id,
        &Actor::user(user_id, UserRole::Member),
        ProfileUpdate {
            notifications: Some(NotificationSettingsUpdate {
                digest_scope: Some(scope),
                ..Default::default()
            }),
            ..Default::default()
        },
        Utc::now(),
    )
    .await
    .expect("digest scope");
    tx.commit().await.expect("commit");
}

/// Настройки напоминаний при заведомо выключенной сводке: тесты, которые
/// проверяют только напоминания, не должны получать лишние сообщения.
async fn set_notifications_without_digest(
    pool: &PgPool,
    user_id: Uuid,
    mut value: serde_json::Value,
) {
    if let Some(settings) = value.as_object_mut() {
        settings
            .entry("digest_enabled")
            .or_insert(serde_json::Value::Bool(false));
        // Тесты без явного сценария тихих часов не должны зависеть от времени
        // суток, в которое запущен набор.
        settings
            .entry("quiet_hours_enabled")
            .or_insert(serde_json::Value::Bool(false));
    }
    set_notifications(pool, user_id, value).await;
}

async fn enable_quiet(pool: &PgPool, user_id: Uuid, start: &str, end: &str) {
    set_notifications_without_digest(
        pool,
        user_id,
        json!({
            "quiet_hours_enabled": true,
            "quiet_start": start,
            "quiet_end": end
        }),
    )
    .await;
}

async fn scheduled_at(pool: &PgPool, item_id: Uuid, user_id: Uuid) -> Option<DateTime<Utc>> {
    sqlx::query_scalar(
        "SELECT scheduled_at FROM notification_deliveries
         WHERE item_id = $1 AND user_id = $2 AND kind = 'reminder'
         ORDER BY created_at DESC, id DESC LIMIT 1",
    )
    .bind(item_id)
    .bind(user_id)
    .fetch_optional(pool)
    .await
    .expect("scheduled at")
}

async fn create_task_with_priority(
    db: &TestDb,
    household: &TestHousehold,
    title: &str,
    due_at: DateTime<Utc>,
    priority: ItemPriority,
) -> Item {
    let mut item = support::new_item(household.tasks_list_id, title);
    item.due_at = Some(due_at);
    item.priority = priority;
    item.assignee_user_id = Some(household.owner_id);
    support::create_item(
        &db.pool,
        household,
        household.tasks_list_id,
        &household.owner(),
        item,
    )
    .await
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

#[allow(clippy::too_many_arguments)]
async fn insert_snooze_with_origin(
    pool: &PgPool,
    household_id: Uuid,
    user_id: Uuid,
    item_id: Uuid,
    key: &str,
    origin_key: Option<&str>,
    scheduled_at: DateTime<Utc>,
) -> Result<u64, sqlx::Error> {
    let mut conn = pool.acquire().await?;
    let delivery = NewDelivery {
        household_id,
        user_id,
        item_id: Some(item_id),
        kind: NotificationKind::Snooze,
        occurrence_key: key,
        origin_key,
        replaces_message_id: None,
        scheduled_at,
    };
    notifications::insert(&mut conn, &delivery, scheduled_at).await
}

async fn snooze_delivery(
    pool: &PgPool,
    item_id: Uuid,
    key: &str,
) -> (
    String,
    Option<String>,
    DateTime<Utc>,
    bool,
    bool,
    Option<i64>,
) {
    sqlx::query_as(
        "SELECT status::text, origin_key, scheduled_at, deferred, batched, outbox_seq
         FROM notification_deliveries
         WHERE item_id = $1 AND occurrence_key = $2 AND kind = 'snooze'",
    )
    .bind(item_id)
    .bind(key)
    .fetch_one(pool)
    .await
    .expect("snooze delivery")
}

/// Snooze-доставка с сообщением «😴 Напомню …», которое она заменит.
#[allow(clippy::too_many_arguments)]
async fn insert_snooze_replacing(
    pool: &PgPool,
    household_id: Uuid,
    user_id: Uuid,
    item_id: Uuid,
    key: &str,
    origin_key: Option<&str>,
    scheduled_at: DateTime<Utc>,
    replaces_message_id: Option<i64>,
) -> Result<u64, sqlx::Error> {
    let mut conn = pool.acquire().await?;
    let delivery = NewDelivery {
        household_id,
        user_id,
        item_id: Some(item_id),
        kind: NotificationKind::Snooze,
        occurrence_key: key,
        origin_key,
        replaces_message_id,
        scheduled_at,
    };
    notifications::insert(&mut conn, &delivery, scheduled_at).await
}

/// Доставленное напоминание без бота: тик отдаёт его в outbox, а строка
/// отмечается отправленной с сообщением `101`, как это сделал бы воркер.
async fn delivered_reminder_row(
    db: &TestDb,
    household: &TestHousehold,
    item: &Item,
    at: DateTime<Utc>,
) -> i64 {
    insert_delivery(
        &db.pool,
        household.household_id,
        household.owner_id,
        Some(item.id),
        &occurrence_key(item.due_at.expect("timed task")),
        "pending",
        at,
    )
    .await
    .expect("reminder delivery");
    scheduler(&db.pool).tick(at).await.expect("reminder tick");
    let seq: i64 = sqlx::query_scalar(
        "SELECT outbox_seq FROM notification_deliveries
         WHERE item_id = $1 AND kind = 'reminder'",
    )
    .bind(item.id)
    .fetch_one(&db.pool)
    .await
    .expect("reminder outbox seq");
    sqlx::query("UPDATE outbox SET status = 'sent', message_id = 101, sent_at = $2 WHERE seq = $1")
        .bind(seq)
        .bind(at)
        .execute(&db.pool)
        .await
        .expect("mark outbox sent");
    101
}

/// Строки удаления с ключом замены: статус, ключ и payload.
async fn replace_delete_rows(pool: &PgPool, chat: i64) -> Vec<(String, String, serde_json::Value)> {
    sqlx::query_as(
        "SELECT status::text, dedupe_key, payload FROM outbox
         WHERE chat_id = $1 AND kind = 'delete' AND dedupe_key LIKE 'replace:%'
         ORDER BY seq",
    )
    .bind(chat)
    .fetch_all(pool)
    .await
    .expect("replace delete rows")
}

/// Есть ли среди вызовов фейка удаление данного сообщения.
fn deleted(bot: &Harness, message_id: i64) -> bool {
    bot.fake.calls().iter().any(
        |call| matches!(call, Call::Delete { chat, id } if *chat == bot.chat && *id == message_id),
    )
}

/// Напоминание, доставленное боту: задача со сроком через три часа, тик за
/// 15 минут до срока. Возвращает стенд, household, задачу и id сообщения.
async fn delivered_reminder(db: &TestDb, title: &str) -> (Harness, TestHousehold, Item, i64) {
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let due = rounded_now() + Duration::hours(3);
    let item = create_task(
        db,
        &household,
        title,
        Some(due),
        Some(household.owner_id),
        None,
    )
    .await;
    tick_and_deliver(&mut bot, &db.pool, due - Duration::minutes(15)).await;
    let (message_id, _) = bot.latest();
    (bot, household, item, message_id)
}

/// Нажать «Отложить на час» в напоминании и вернуть срок snooze-доставки.
async fn snooze_reminder(bot: &mut Harness, pool: &PgPool, item_id: Uuid) -> DateTime<Utc> {
    let (message_id, html) = bot.latest();
    bot.press_data(&button_of(&html, "на час"), message_id)
        .await;
    sqlx::query_scalar(
        "SELECT scheduled_at FROM notification_deliveries
         WHERE item_id = $1 AND kind = 'snooze'",
    )
    .bind(item_id)
    .fetch_one(pool)
    .await
    .expect("snooze scheduled_at")
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
async fn timed_event_reminder_goes_to_its_participant_at_lead_time() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let due = rounded_now() + Duration::hours(3);
    let event = create_event(&db, &household, "Маникюр", due, due + Duration::minutes(90)).await;
    assign_without_notice(&db.pool, event.id, household.member_id).await;
    let owner_chat = chat_id(&db.pool, household.owner_id).await;
    let member_chat = chat_id(&db.pool, household.member_id).await;
    let scheduler = scheduler(&db.pool);

    let early = scheduler
        .tick(due - Duration::minutes(16))
        .await
        .expect("early tick");
    assert_eq!(early.planned, 0);
    assert_eq!(early.delivered, 0);
    assert!(messages(&db.pool, member_chat).await.is_empty());

    let on_time = scheduler
        .tick(due - Duration::minutes(15))
        .await
        .expect("on-time tick");
    assert_eq!(on_time.planned, 1);
    assert_eq!(on_time.delivered, 1);

    let sent = messages(&db.pool, member_chat).await;
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert!(sent[0].contains("Маникюр"), "{sent:?}");
    assert!(
        messages(&db.pool, owner_chat).await.is_empty(),
        "the author must not get a reminder for another participant's event"
    );
    let rows = delivery_rows(&db.pool, event.id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "sent");
    assert_eq!(rows[0].1, occurrence_key(due));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_multiday_all_day_event_reminds_only_on_its_first_day() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let moscow = chrono_tz::Europe::Moscow;
    let first = NaiveDate::from_ymd_opt(2026, 3, 5).expect("first day");
    let last = NaiveDate::from_ymd_opt(2026, 3, 10).expect("last day");
    let event = create_all_day_event(&db, &household, "Отпуск", first, Some(last)).await;
    assign_without_notice(&db.pool, event.id, household.owner_id).await;
    let chat = chat_id(&db.pool, household.owner_id).await;
    let scheduler = scheduler(&db.pool);

    let early = scheduler
        .tick(local_utc(moscow, first, 8, 59))
        .await
        .expect("early tick");
    assert_eq!(early.planned, 0);
    assert!(messages(&db.pool, chat).await.is_empty());

    let on_time = scheduler
        .tick(local_utc(moscow, first, 9, 0))
        .await
        .expect("first-day tick");
    assert_eq!(on_time.delivered, 1);
    assert_eq!(messages(&db.pool, chat).await.len(), 1);

    let next_day = scheduler
        .tick(local_utc(moscow, first + Duration::days(1), 9, 0))
        .await
        .expect("second-day tick");
    assert_eq!(next_day.planned, 0);
    assert_eq!(next_day.delivered, 0);
    assert_eq!(messages(&db.pool, chat).await.len(), 1);
    let rows = delivery_rows(&db.pool, event.id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].1, all_day_occurrence_key(first));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_recurring_event_occurrence_gets_its_own_reminder() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let moscow = chrono_tz::Europe::Moscow;
    // Вторник 10 ноября 2026, 18:00–19:00 по Москве.
    let start = local_utc(
        moscow,
        NaiveDate::from_ymd_opt(2026, 11, 10).expect("day"),
        18,
        0,
    );
    let event = create_recurring_event(
        &db,
        &household,
        "Тренировка",
        start,
        start + Duration::hours(1),
        support::recurrence(RecurrenceFreq::Weekly, vec![2]),
    )
    .await;
    assign_without_notice(&db.pool, event.id, household.owner_id).await;
    let chat = chat_id(&db.pool, household.owner_id).await;
    let scheduler = scheduler(&db.pool);

    assert_eq!(
        scheduler
            .tick(start - Duration::minutes(15))
            .await
            .expect("first occurrence")
            .delivered,
        1
    );
    assert_eq!(
        scheduler
            .tick(start - Duration::minutes(15))
            .await
            .expect("same occurrence")
            .delivered,
        0
    );

    ticker(&db.pool)
        .tick(start + Duration::hours(1))
        .await
        .expect("the event moves to the next occurrence");

    let next_start = start + Duration::days(7);
    assert_eq!(
        scheduler
            .tick(next_start - Duration::minutes(15))
            .await
            .expect("next occurrence")
            .delivered,
        1
    );
    let rows = delivery_rows(&db.pool, event.id).await;
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[0].1, occurrence_key(start));
    assert_eq!(rows[1].1, occurrence_key(next_start));
    assert_eq!(messages(&db.pool, chat).await.len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_past_event_reminder_is_cancelled_instead_of_delivered() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let moscow = chrono_tz::Europe::Moscow;
    // Событие 07:00–07:30, а тихие часы откладывают напоминание до 08:00.
    let day = NaiveDate::from_ymd_opt(2026, 11, 10).expect("day");
    let start = local_utc(moscow, day, 7, 0);
    let end = local_utc(moscow, day, 7, 30);
    let event = create_event(&db, &household, "Зарядка", start, end).await;
    assign_without_notice(&db.pool, event.id, household.owner_id).await;
    enable_quiet(&db.pool, household.owner_id, "22:00", "08:00").await;
    let chat = chat_id(&db.pool, household.owner_id).await;
    let scheduler = scheduler(&db.pool);

    let planned = scheduler
        .tick(start - Duration::minutes(15))
        .await
        .expect("quiet-hours tick");
    assert_eq!(planned.planned, 1);
    assert_eq!(planned.delivered, 0);
    assert!(messages(&db.pool, chat).await.is_empty());

    let morning = scheduler
        .tick(local_utc(moscow, day, 8, 0))
        .await
        .expect("morning tick");
    assert_eq!(morning.delivered, 0);
    assert_eq!(morning.cancelled, 1);
    assert!(messages(&db.pool, chat).await.is_empty());

    let rows = delivery_rows(&db.pool, event.id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "cancelled");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_changed_occurrence_cancels_the_reminder_of_the_old_one() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let moscow = chrono_tz::Europe::Moscow;
    // Еженедельное событие 07:00–07:30; тихие часы откладывают напоминание до
    // 08:00, когда первое вхождение уже закончилось и событие перешло на
    // следующую неделю.
    let day = NaiveDate::from_ymd_opt(2026, 11, 10).expect("day");
    let start = local_utc(moscow, day, 7, 0);
    let end = local_utc(moscow, day, 7, 30);
    let event = create_recurring_event(
        &db,
        &household,
        "Зарядка",
        start,
        end,
        support::recurrence(RecurrenceFreq::Weekly, vec![2]),
    )
    .await;
    assign_without_notice(&db.pool, event.id, household.owner_id).await;
    enable_quiet(&db.pool, household.owner_id, "22:00", "08:00").await;
    let chat = chat_id(&db.pool, household.owner_id).await;
    let scheduler = scheduler(&db.pool);

    let planned = scheduler
        .tick(start - Duration::minutes(15))
        .await
        .expect("quiet-hours tick");
    assert_eq!(planned.planned, 1);
    assert_eq!(planned.delivered, 0);
    assert!(messages(&db.pool, chat).await.is_empty());

    ticker(&db.pool)
        .tick(local_utc(moscow, day, 8, 0))
        .await
        .expect("morning domain tick");
    let morning = scheduler
        .tick(local_utc(moscow, day, 8, 0))
        .await
        .expect("morning tick");
    assert_eq!(morning.delivered, 0);
    assert_eq!(morning.cancelled, 1);
    assert!(messages(&db.pool, chat).await.is_empty());

    let rows = delivery_rows(&db.pool, event.id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "cancelled");
    assert_eq!(rows[0].1, occurrence_key(start));

    let mut conn = db.pool.acquire().await.expect("conn");
    let current = items::get(&mut conn, household.household_id, event.id)
        .await
        .expect("advanced event");
    assert_eq!(
        current.due_at,
        Some(start + Duration::days(7)),
        "the event must have moved to the next occurrence"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failing_digest_step_does_not_stop_reminder_delivery() {
    let db = support::isolated_db().await;
    let household = support::create_household(&db.pool).await;
    // Сводка должна планироваться именно в этом тике, поэтому её время уже
    // наступило. Тихие часы выключены: иначе сводка переносится на утро.
    for user_id in [household.owner_id, household.member_id] {
        set_notifications(
            &db.pool,
            user_id,
            json!({
                "digest_enabled": true,
                "digest_time": "00:00",
                "quiet_hours_enabled": false
            }),
        )
        .await;
    }
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

    let result =
        support::notifications::tick_with_a_failing_digest(&db.pool, due - Duration::minutes(15))
            .await;

    assert!(
        result.is_err(),
        "a failed digest step must be reported by the tick"
    );
    let sent = messages(&db.pool, chat).await;
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert!(sent[0].contains("Позвонить в поликлинику"), "{sent:?}");
    let rows = delivery_rows(&db.pool, item.id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "sent");
    assert!(rows[0].2.is_some());
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
    set_notifications_without_digest(
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

    // Задачу создал владелец, но назначил участнику: напоминание получает
    // только исполнитель, автор — нет.
    let assigned = create_task_without_assignment_notice(
        &db,
        &household,
        "Задача участника",
        Some(due),
        household.member_id,
        None,
    )
    .await;

    let stats = scheduler(&db.pool)
        .tick(due - Duration::minutes(15))
        .await
        .expect("tick");
    assert_eq!(stats.delivered, 1);

    let member_messages = messages(&db.pool, member_chat).await;
    assert_eq!(member_messages.len(), 1, "{member_messages:?}");
    assert!(member_messages[0].contains("Задача участника"));
    assert!(
        messages(&db.pool, owner_chat).await.is_empty(),
        "the author must not get a reminder for a task assigned to someone else"
    );

    let assigned_user: Uuid =
        sqlx::query_scalar("SELECT user_id FROM notification_deliveries WHERE item_id = $1")
            .bind(assigned.id)
            .fetch_one(&db.pool)
            .await
            .expect("assignee delivery");
    assert_eq!(assigned_user, household.member_id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_task_reminds_every_enabled_member_including_author_once() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let due = rounded_now() + Duration::hours(3);
    let item = create_task(&db, &household, "Общая задача", Some(due), None, None).await;
    let owner_chat = chat_id(&db.pool, household.owner_id).await;
    let member_chat = chat_id(&db.pool, household.member_id).await;
    let scheduler = scheduler(&db.pool);
    let now = due - Duration::minutes(15);

    let stats = scheduler.tick(now).await.expect("shared tick");
    assert_eq!(stats.planned, 2);
    assert_eq!(stats.delivered, 2);
    let owner_messages = messages(&db.pool, owner_chat).await;
    assert_eq!(owner_messages.len(), 1, "{owner_messages:?}");
    assert!(owner_messages[0].contains("Общая задача"));
    let member_messages = messages(&db.pool, member_chat).await;
    assert_eq!(member_messages.len(), 1, "{member_messages:?}");
    assert!(member_messages[0].contains("Общая задача"));

    // Повторный тик не дублирует: каждому ровно одно сообщение.
    assert_eq!(scheduler.tick(now).await.expect("repeat tick").delivered, 0);
    assert_eq!(messages(&db.pool, owner_chat).await.len(), 1);
    assert_eq!(messages(&db.pool, member_chat).await.len(), 1);
    assert_eq!(delivery_rows(&db.pool, item.id).await.len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_task_skips_member_with_disabled_reminders() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let extra_id = add_member(&db.pool, household.household_id, "Extra member", false).await;
    set_notifications_without_digest(
        &db.pool,
        household.member_id,
        json!({"reminders_enabled": false}),
    )
    .await;
    let due = rounded_now() + Duration::hours(3);
    create_task(&db, &household, "Общая задача", Some(due), None, None).await;
    let owner_chat = chat_id(&db.pool, household.owner_id).await;
    let member_chat = chat_id(&db.pool, household.member_id).await;
    let extra_chat = chat_id(&db.pool, extra_id).await;

    let stats = scheduler(&db.pool)
        .tick(due - Duration::minutes(15))
        .await
        .expect("tick");
    assert_eq!(stats.delivered, 2);
    assert_eq!(messages(&db.pool, owner_chat).await.len(), 1);
    assert_eq!(messages(&db.pool, extra_chat).await.len(), 1);
    assert!(
        messages(&db.pool, member_chat).await.is_empty(),
        "a member with disabled reminders must not get the shared task"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn assigning_shared_task_cancels_pending_reminders_of_other_members() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let extra_id = add_member(&db.pool, household.household_id, "Extra member", false).await;
    let due = rounded_now() + Duration::hours(3);
    let item = create_task(&db, &household, "Общая задача", Some(due), None, None).await;
    let now = due - Duration::minutes(15);
    let key = occurrence_key(due);
    // Запланированные, но ещё не доставленные напоминания остальным участникам.
    for user_id in [household.owner_id, extra_id] {
        insert_delivery(
            &db.pool,
            household.household_id,
            user_id,
            Some(item.id),
            &key,
            "pending",
            now,
        )
        .await
        .expect("pending reminder");
    }
    assign_without_notice(&db.pool, item.id, household.member_id).await;

    let owner_chat = chat_id(&db.pool, household.owner_id).await;
    let member_chat = chat_id(&db.pool, household.member_id).await;
    let extra_chat = chat_id(&db.pool, extra_id).await;
    let stats = scheduler(&db.pool).tick(now).await.expect("assign tick");
    assert_eq!(stats.cancelled, 2, "{stats:?}");
    assert_eq!(stats.delivered, 1, "{stats:?}");
    assert_eq!(
        delivery_status(&db.pool, item.id, household.owner_id)
            .await
            .as_deref(),
        Some("cancelled")
    );
    assert_eq!(
        delivery_status(&db.pool, item.id, extra_id)
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
    assert!(messages(&db.pool, extra_chat).await.is_empty());
    assert_eq!(messages(&db.pool, member_chat).await.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unassigning_adds_reminders_for_other_members_on_next_tick() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let due = rounded_now() + Duration::hours(3);
    let item = create_task(
        &db,
        &household,
        "Снять исполнителя",
        Some(due),
        Some(household.owner_id),
        None,
    )
    .await;
    let owner_chat = chat_id(&db.pool, household.owner_id).await;
    let member_chat = chat_id(&db.pool, household.member_id).await;
    let scheduler = scheduler(&db.pool);
    let now = due - Duration::minutes(15);

    // Сначала напоминание получает только исполнитель (владелец).
    assert_eq!(
        scheduler.tick(now).await.expect("assigned tick").delivered,
        1
    );
    assert_eq!(messages(&db.pool, owner_chat).await.len(), 1);
    assert!(messages(&db.pool, member_chat).await.is_empty());

    // Снятие исполнителя делает задачу общей: на следующем тике напоминание
    // получает оставшийся участник, а у владельца не дублируется.
    sqlx::query("UPDATE items SET assignee_user_id = NULL WHERE id = $1")
        .bind(item.id)
        .execute(&db.pool)
        .await
        .expect("unassign");
    let stats = scheduler.tick(now).await.expect("shared tick");
    assert_eq!(stats.delivered, 1, "{stats:?}");
    assert_eq!(messages(&db.pool, member_chat).await.len(), 1);
    assert_eq!(messages(&db.pool, owner_chat).await.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn removing_assignee_from_household_makes_task_shared() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let extra_id = add_member(&db.pool, household.household_id, "Extra member", false).await;
    let due = rounded_now() + Duration::hours(3);
    create_task(
        &db,
        &household,
        "Задача исключённого",
        Some(due),
        Some(household.member_id),
        None,
    )
    .await;
    let owner_chat = chat_id(&db.pool, household.owner_id).await;
    let member_chat = chat_id(&db.pool, household.member_id).await;
    let extra_chat = chat_id(&db.pool, extra_id).await;

    // Исключение исполнителя реальным путём снимает его с задачи (detach).
    let mut conn = db.pool.acquire().await.expect("conn");
    members::remove(
        &mut conn,
        household.household_id,
        &household.owner(),
        household.member_id,
        Utc::now(),
    )
    .await
    .expect("remove member");
    drop(conn);

    let stats = scheduler(&db.pool)
        .tick(due - Duration::minutes(15))
        .await
        .expect("tick");
    assert_eq!(stats.delivered, 2, "{stats:?}");
    assert_eq!(messages(&db.pool, owner_chat).await.len(), 1);
    assert_eq!(messages(&db.pool, extra_chat).await.len(), 1);
    assert!(messages(&db.pool, member_chat).await.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn event_without_participant_reminds_every_enabled_member() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let due = rounded_now() + Duration::hours(3);
    let event = create_event(
        &db,
        &household,
        "Общее событие",
        due,
        due + Duration::minutes(90),
    )
    .await;
    let owner_chat = chat_id(&db.pool, household.owner_id).await;
    let member_chat = chat_id(&db.pool, household.member_id).await;

    let stats = scheduler(&db.pool)
        .tick(due - Duration::minutes(15))
        .await
        .expect("event tick");
    assert_eq!(stats.delivered, 2, "{stats:?}");
    let owner_messages = messages(&db.pool, owner_chat).await;
    assert_eq!(owner_messages.len(), 1, "{owner_messages:?}");
    assert!(owner_messages[0].contains("Общее событие"));
    let member_messages = messages(&db.pool, member_chat).await;
    assert_eq!(member_messages.len(), 1, "{member_messages:?}");
    assert!(member_messages[0].contains("Общее событие"));
    assert_eq!(delivery_rows(&db.pool, event.id).await.len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snooze_on_shared_task_is_cancelled_when_assigned_to_another_member() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let due = rounded_now() + Duration::hours(3);
    let item = create_task(&db, &household, "Общая задача", Some(due), None, None).await;
    let owner_chat = chat_id(&db.pool, household.owner_id).await;
    let at = due - Duration::minutes(15);
    // Петя (владелец) получает напоминание об общей задаче и откладывает его.
    scheduler(&db.pool).tick(at).await.expect("reminder tick");
    let snooze_at = at + Duration::hours(1);
    insert_snooze_with_origin(
        &db.pool,
        household.household_id,
        household.owner_id,
        item.id,
        &notifications::snooze_key(snooze_at),
        Some(&snooze_origin(item.due_at, None)),
        snooze_at,
    )
    .await
    .expect("snooze");
    // Задачу назначили Маше: отложенное напоминание Пете больше не нужно.
    assign_without_notice(&db.pool, item.id, household.member_id).await;

    let stats = scheduler(&db.pool)
        .tick(snooze_at)
        .await
        .expect("snooze tick");
    assert_eq!(stats.delivered, 0, "{stats:?}");
    assert!(stats.cancelled >= 1, "{stats:?}");
    let snooze_messages: Vec<String> = sqlx::query_scalar(
        "SELECT payload->>'html' FROM outbox
         WHERE chat_id = $1 AND dedupe_key LIKE 'notification:snooze:%'",
    )
    .bind(owner_chat)
    .fetch_all(&db.pool)
    .await
    .expect("snooze messages");
    assert!(snooze_messages.is_empty(), "{snooze_messages:?}");
    let status: String = sqlx::query_scalar(
        "SELECT status::text FROM notification_deliveries
         WHERE item_id = $1 AND kind = 'snooze'",
    )
    .bind(item.id)
    .fetch_one(&db.pool)
    .await
    .expect("snooze status");
    assert_eq!(status, "cancelled");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snooze_on_shared_task_is_delivered_when_assigned_to_the_same_member() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let due = rounded_now() + Duration::hours(3);
    let item = create_task(&db, &household, "Общая задача", Some(due), None, None).await;
    let owner_chat = chat_id(&db.pool, household.owner_id).await;
    let at = due - Duration::minutes(15);
    scheduler(&db.pool).tick(at).await.expect("reminder tick");
    let snooze_at = at + Duration::hours(1);
    insert_snooze_with_origin(
        &db.pool,
        household.household_id,
        household.owner_id,
        item.id,
        &notifications::snooze_key(snooze_at),
        Some(&snooze_origin(item.due_at, None)),
        snooze_at,
    )
    .await
    .expect("snooze");
    // Задачу назначили самому Пете: отложенное напоминание по-прежнему его.
    assign_without_notice(&db.pool, item.id, household.owner_id).await;

    let stats = scheduler(&db.pool)
        .tick(snooze_at)
        .await
        .expect("snooze tick");
    assert_eq!(stats.delivered, 1, "{stats:?}");
    let snooze_messages: Vec<String> = sqlx::query_scalar(
        "SELECT payload->>'html' FROM outbox
         WHERE chat_id = $1 AND dedupe_key LIKE 'notification:snooze:%'",
    )
    .bind(owner_chat)
    .fetch_all(&db.pool)
    .await
    .expect("snooze messages");
    assert_eq!(snooze_messages.len(), 1, "{snooze_messages:?}");
    assert!(snooze_messages[0].contains("Общая задача"));
    let status: String = sqlx::query_scalar(
        "SELECT status::text FROM notification_deliveries
         WHERE item_id = $1 AND kind = 'snooze'",
    )
    .bind(item.id)
    .fetch_one(&db.pool)
    .await
    .expect("snooze status");
    assert_eq!(status, "sent");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn all_day_reminder_fires_at_user_local_nine_and_only_on_due_day() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let vladivostok = chrono_tz::Asia::Vladivostok;
    set_household_timezone(&db.pool, household.household_id, "Europe/Moscow").await;
    set_user_timezone(&db.pool, household.owner_id, "Asia/Vladivostok").await;
    set_notifications_without_digest(
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
    set_notifications_without_digest(
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
    set_notifications_without_digest(
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
    let extra_id = add_member(&db.pool, household.household_id, "Extra member", false).await;
    set_notifications_without_digest(
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
async fn cancel_stale_skips_a_locked_delivery_until_the_lock_is_released() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let due = rounded_now() + Duration::hours(3);
    let now = due - Duration::minutes(15);
    let unlocked = create_task(
        &db,
        &household,
        "Отменится сразу",
        Some(due),
        Some(household.owner_id),
        None,
    )
    .await;
    let locked = create_task(
        &db,
        &household,
        "Ждёт блокировку",
        Some(due),
        Some(household.owner_id),
        None,
    )
    .await;
    let unlocked_delivery = insert_delivery(
        &db.pool,
        household.household_id,
        household.owner_id,
        Some(unlocked.id),
        &occurrence_key(due),
        "pending",
        now,
    )
    .await
    .expect("unlocked delivery");
    let locked_delivery = insert_delivery(
        &db.pool,
        household.household_id,
        household.owner_id,
        Some(locked.id),
        &occurrence_key(due),
        "pending",
        now,
    )
    .await
    .expect("locked delivery");

    // Оба срока сдвинуты: записи устарели.
    sqlx::query("UPDATE items SET due_at = due_at + interval '2 hours' WHERE id = $1 OR id = $2")
        .bind(unlocked.id)
        .bind(locked.id)
        .execute(&db.pool)
        .await
        .expect("move due");

    let mut lock_conn = db.pool.acquire().await.expect("lock connection");
    let mut lock_tx = lock_conn.begin().await.expect("lock transaction");
    sqlx::query("SELECT id FROM notification_deliveries WHERE id = $1 FOR UPDATE")
        .bind(locked_delivery)
        .fetch_one(&mut *lock_tx)
        .await
        .expect("row lock");

    let stats = scheduler(&db.pool).tick(now).await.expect("locked tick");
    assert_eq!(stats.cancelled, 1, "{stats:?}");
    let unlocked_status: String =
        sqlx::query_scalar("SELECT status::text FROM notification_deliveries WHERE id = $1")
            .bind(unlocked_delivery)
            .fetch_one(&db.pool)
            .await
            .expect("unlocked status");
    let locked_status: String =
        sqlx::query_scalar("SELECT status::text FROM notification_deliveries WHERE id = $1")
            .bind(locked_delivery)
            .fetch_one(&db.pool)
            .await
            .expect("locked status");
    assert_eq!(unlocked_status, "cancelled");
    assert_eq!(
        locked_status, "pending",
        "a row held by another transaction must be skipped"
    );

    lock_tx.rollback().await.expect("release lock");
    drop(lock_conn);

    let stats = scheduler(&db.pool).tick(now).await.expect("release tick");
    assert_eq!(stats.cancelled, 1, "{stats:?}");
    let locked_status: String =
        sqlx::query_scalar("SELECT status::text FROM notification_deliveries WHERE id = $1")
            .bind(locked_delivery)
            .fetch_one(&db.pool)
            .await
            .expect("locked status after release");
    assert_eq!(locked_status, "cancelled");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rescheduling_an_occurrence_moves_the_pending_reminder() {
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

    let new_due = due + Duration::hours(2);
    let mut conn = db.pool.acquire().await.expect("connection");
    items::reschedule_occurrence(
        &mut conn,
        household.household_id,
        &household.owner(),
        item.id,
        item.version,
        Due::Timed(new_due),
        "Europe/Moscow",
        now,
    )
    .await
    .expect("reschedule occurrence");
    drop(conn);

    let chat = chat_id(&db.pool, household.owner_id).await;
    let stats = scheduler(&db.pool).tick(now).await.expect("moved tick");
    assert_eq!(stats.cancelled, 1);
    assert_eq!(stats.delivered, 0);
    assert!(messages(&db.pool, chat).await.is_empty());
    let rows = delivery_rows(&db.pool, item.id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "cancelled");
    assert_eq!(rows[0].1, occurrence_key(due));

    let moved_at = new_due - Duration::minutes(15);
    let stats = scheduler(&db.pool)
        .tick(moved_at)
        .await
        .expect("new due tick");
    assert_eq!(stats.planned, 1);
    assert_eq!(stats.delivered, 1);
    let sent = messages(&db.pool, chat).await;
    assert_eq!(sent.len(), 1, "{sent:?}");
    let rows = delivery_rows(&db.pool, item.id).await;
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert!(
        rows.iter()
            .any(|row| { row.0 == "cancelled" && row.1 == occurrence_key(due) })
    );
    assert!(
        rows.iter()
            .any(|row| { row.0 == "sent" && row.1 == occurrence_key(new_due) })
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lead_time_of_one_hour_shifts_the_delivery_moment() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    set_notifications_without_digest(&db.pool, household.owner_id, json!({"lead_minutes": 60}))
        .await;
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
    set_notifications_without_digest(
        &db.pool,
        household.member_id,
        json!({"reminders_enabled": false}),
    )
    .await;
    let due = rounded_now() + Duration::hours(3);
    let item = create_task_without_assignment_notice(
        &db,
        &household,
        "Исполнитель без напоминаний",
        Some(due),
        household.member_id,
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
async fn clearing_assignee_makes_the_task_shared_and_reminds_every_member() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let due = rounded_now() + Duration::hours(3);
    let item = create_task_without_assignment_notice(
        &db,
        &household,
        "Общая задача",
        Some(due),
        household.member_id,
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

    // Без исполнителя задача общая: прежнее напоминание исполнителя остаётся
    // актуальным, а остальные участники получают своё на этом же тике.
    let stats = scheduler(&db.pool)
        .tick(due - Duration::minutes(15))
        .await
        .expect("tick");
    assert_eq!(stats.cancelled, 0);
    assert_eq!(stats.delivered, 2);
    assert_eq!(
        delivery_status(&db.pool, item.id, household.member_id)
            .await
            .as_deref(),
        Some("sent")
    );
    assert_eq!(
        delivery_status(&db.pool, item.id, household.owner_id)
            .await
            .as_deref(),
        Some("sent")
    );
    assert_eq!(messages(&db.pool, owner_chat).await.len(), 1);
    assert_eq!(messages(&db.pool, member_chat).await.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn soft_deleted_assignee_is_not_replaced_by_the_author() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let due = rounded_now() + Duration::hours(3);
    let item = create_task_without_assignment_notice(
        &db,
        &household,
        "Удалённый исполнитель",
        Some(due),
        household.member_id,
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
    assert!(html.contains("🛒 Покупки · 3"), "{html}");
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
        json!({
            "digest_enabled": true,
            "digest_time": "07:15",
            "quiet_hours_enabled": false
        }),
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
    // Событие завтра не делает сегодняшний день занятым.
    create_all_day_event(
        &db,
        &household,
        "Завтрашнее событие",
        day + Duration::days(1),
        None,
    )
    .await;
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
    assert!(!html.contains("Завтрашнее событие"), "{html}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn digest_shows_today_events_and_keeps_them_out_of_overdue() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = digest_day();
    set_household_timezone(&db.pool, household.household_id, "Europe/Moscow").await;
    enable_digest(&db.pool, household.owner_id).await;

    create_event(
        &db,
        &household,
        "Маникюр",
        local_utc(moscow, day, 14, 0),
        local_utc(moscow, day, 15, 30),
    )
    .await;
    // Многодневное событие началось раньше и ещё идёт.
    create_all_day_event(
        &db,
        &household,
        "Отпуск",
        day - Duration::days(2),
        Some(day + Duration::days(1)),
    )
    .await;
    // Прошедшее событие не попадает ни в «Сегодня», ни в «Просрочено».
    create_all_day_event(
        &db,
        &household,
        "Вчерашняя встреча",
        day - Duration::days(1),
        Some(day - Duration::days(1)),
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
    let chat = chat_id(&db.pool, household.owner_id).await;

    let stats = scheduler(&db.pool)
        .tick(local_utc(moscow, day, 8, 30))
        .await
        .expect("digest tick");
    assert_eq!(stats.delivered, 1);

    let sent = messages(&db.pool, chat).await;
    assert_eq!(sent.len(), 1, "{sent:?}");
    let html = &sent[0];
    assert!(html.contains("Маникюр"), "{html}");
    assert!(html.contains("14:00–15:30"), "{html}");
    assert!(html.contains("Отпуск"), "{html}");
    assert!(html.contains("Просроченная задача"), "{html}");
    assert!(!html.contains("Вчерашняя встреча"), "{html}");

    let overdue_at = html.find("<b>Просрочено</b>").expect("overdue heading");
    assert!(
        html.find("Маникюр").expect("manicure") < overdue_at,
        "a today event must not fall into the overdue section: {html}"
    );
    assert!(
        html.find("Отпуск").expect("vacation") < overdue_at,
        "an ongoing multi-day event must not fall into the overdue section: {html}"
    );
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
    let mut tasks = Vec::new();
    for n in 0..25 {
        tasks.push(
            create_all_day_task(
                &db,
                &household,
                &format!("Задача {n}"),
                day,
                Some(household.owner_id),
            )
            .await,
        );
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
    assert!(html.contains("Задача 19"), "{html}");
    assert!(!html.contains("Задача 20"), "{html}");

    // Снимок хранит все 25 строк в порядке отрисовки, хотя в сообщении 20.
    let (snapshot, _hash) = digest_snapshot_and_hash(&db.pool, household.owner_id).await;
    let expected: Vec<DigestSnapshotItem> = tasks
        .iter()
        .map(|task| DigestSnapshotItem {
            item_id: task.id,
            section: DigestSection::Today,
            occurrence_key: all_day_occurrence_key(day),
        })
        .collect();
    assert_eq!(snapshot, expected);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn digest_delivery_writes_snapshot_and_render_hash() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = digest_day();
    set_household_timezone(&db.pool, household.household_id, "Europe/Moscow").await;
    enable_digest(&db.pool, household.owner_id).await;
    let today_task = create_task(
        &db,
        &household,
        "Задача сегодня",
        Some(local_utc(moscow, day, 18, 0)),
        Some(household.owner_id),
        None,
    )
    .await;
    let overdue_task = create_task(
        &db,
        &household,
        "Просроченная задача",
        Some(local_utc(moscow, day - Duration::days(1), 18, 0)),
        Some(household.owner_id),
        None,
    )
    .await;
    let event = create_event(
        &db,
        &household,
        "Событие сегодня",
        local_utc(moscow, day, 14, 0),
        local_utc(moscow, day, 15, 0),
    )
    .await;
    let chat = chat_id(&db.pool, household.owner_id).await;

    let stats = scheduler(&db.pool)
        .tick(local_utc(moscow, day, 8, 30))
        .await
        .expect("digest tick");
    assert_eq!(stats.delivered, 1);

    // Снимок — все строки в порядке отрисовки: «Сегодня», затем «Просрочено»;
    // ключ вхождения — как у напоминаний, у события — ключ начала.
    let (snapshot, hash) = digest_snapshot_and_hash(&db.pool, household.owner_id).await;
    assert_eq!(
        snapshot,
        vec![
            DigestSnapshotItem {
                item_id: event.id,
                section: DigestSection::Event,
                occurrence_key: occurrence_key(event.due_at.expect("event start")),
            },
            DigestSnapshotItem {
                item_id: today_task.id,
                section: DigestSection::Today,
                occurrence_key: occurrence_key(today_task.due_at.expect("task due")),
            },
            DigestSnapshotItem {
                item_id: overdue_task.id,
                section: DigestSection::Overdue,
                occurrence_key: occurrence_key(overdue_task.due_at.expect("task due")),
            },
        ]
    );
    assert_eq!(hash.len(), 64, "{hash}");
    assert!(hash.chars().all(|ch| ch.is_ascii_hexdigit()), "{hash}");

    // Порядок строк сообщения совпадает с порядком снимка.
    let sent = messages(&db.pool, chat).await;
    assert_eq!(sent.len(), 1, "{sent:?}");
    let html = &sent[0];
    let event_at = html.find("Событие сегодня").expect("event line");
    let today_at = html.find("Задача сегодня").expect("today line");
    let overdue_at = html.find("Просроченная задача").expect("overdue line");
    assert!(event_at < today_at && today_at < overdue_at, "{html}");
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

/// Элементы household для тестов состава сводки.
struct DigestScopeItems {
    petya_task: Uuid,
    shared_task: Uuid,
    masha_task: Uuid,
    petya_event: Uuid,
    shared_event: Uuid,
    masha_event: Uuid,
}

/// Household для тестов состава сводки: Петя получает сводку, Маша — нет.
/// На сегодня задача Пети, общая задача, задача Маши, событие Пети, событие
/// Маши, общее событие и одна открытая покупка.
async fn digest_scope_household(db: &TestDb) -> (TestHousehold, Uuid, Uuid, DigestScopeItems) {
    let household = household_without_digest(db).await;
    set_household_timezone(&db.pool, household.household_id, "Europe/Moscow").await;
    let petya = add_member(&db.pool, household.household_id, "Петя", true).await;
    let masha = add_member(&db.pool, household.household_id, "Маша", false).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = digest_day();
    let petya_task = create_task_without_assignment_notice(
        db,
        &household,
        "Задача Пети",
        Some(local_utc(moscow, day, 18, 0)),
        petya,
        None,
    )
    .await;
    let shared_task = create_task(
        db,
        &household,
        "Общая задача",
        Some(local_utc(moscow, day, 19, 0)),
        None,
        None,
    )
    .await;
    let masha_task = create_task_without_assignment_notice(
        db,
        &household,
        "Задача Маши",
        Some(local_utc(moscow, day, 20, 0)),
        masha,
        None,
    )
    .await;
    let masha_event = create_event(
        db,
        &household,
        "Событие Маши",
        local_utc(moscow, day, 14, 0),
        local_utc(moscow, day, 15, 30),
    )
    .await;
    assign_without_notice(&db.pool, masha_event.id, masha).await;
    let shared_event = create_event(
        db,
        &household,
        "Общее событие",
        local_utc(moscow, day, 16, 0),
        local_utc(moscow, day, 17, 0),
    )
    .await;
    let petya_event = create_event(
        db,
        &household,
        "Событие Пети",
        local_utc(moscow, day, 12, 0),
        local_utc(moscow, day, 13, 0),
    )
    .await;
    assign_without_notice(&db.pool, petya_event.id, petya).await;
    create_shopping(db, &household, "Молоко").await;
    let items = DigestScopeItems {
        petya_task: petya_task.id,
        shared_task: shared_task.id,
        masha_task: masha_task.id,
        petya_event: petya_event.id,
        shared_event: shared_event.id,
        masha_event: masha_event.id,
    };
    (household, petya, masha, items)
}

/// HTML отрисовки представления сводки с фиктивными токенами.
fn digest_view_html(view: &tasklerq::telegram::bot::notify::DigestView) -> String {
    let ui = tasklerq::telegram::bot::notify::digest_ui(view);
    let tokens = vec!["t:AAAAAAAAAAAAAAAA".to_owned(); ui.button_count()];
    tasklerq::telegram::rich::render(&ui, &tokens).html
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn digest_mine_scope_shows_own_and_shared_items_only() {
    let db = support::isolated_db().await;
    let (household, petya, _masha, scope_items) = digest_scope_household(&db).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = digest_day();
    set_digest_scope(&db.pool, household.household_id, petya, DigestScope::Mine).await;
    let chat = chat_id(&db.pool, petya).await;

    let stats = scheduler(&db.pool)
        .tick(local_utc(moscow, day, 8, 30))
        .await
        .expect("mine digest tick");
    assert_eq!(stats.delivered, 1);

    let sent = messages(&db.pool, chat).await;
    assert_eq!(sent.len(), 1, "{sent:?}");
    let html = &sent[0];
    assert!(html.contains("Задача Пети"), "{html}");
    assert!(html.contains("Общая задача"), "{html}");
    assert!(html.contains("Общее событие"), "{html}");
    assert!(html.contains("Событие Пети"), "{html}");
    assert!(!html.contains("Задача Маши"), "{html}");
    assert!(!html.contains("Событие Маши"), "{html}");
    // Число покупок от состава не зависит.
    assert!(html.contains("🛒 Покупки · 1"), "{html}");

    // Снимок «только мои» содержит ровно задачу Пети, общую задачу, событие
    // Пети и общее событие — и ничего Машиного.
    let (snapshot, _hash) = digest_snapshot_and_hash(&db.pool, petya).await;
    let ids: Vec<Uuid> = snapshot.iter().map(|line| line.item_id).collect();
    assert_eq!(ids.len(), 4, "{snapshot:?}");
    for id in [
        scope_items.petya_task,
        scope_items.shared_task,
        scope_items.petya_event,
        scope_items.shared_event,
    ] {
        assert!(ids.contains(&id), "{snapshot:?}");
    }
    assert!(!ids.contains(&scope_items.masha_task), "{snapshot:?}");
    assert!(!ids.contains(&scope_items.masha_event), "{snapshot:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn digest_all_scope_shows_the_whole_family() {
    let db = support::isolated_db().await;
    let (household, petya, _masha, _items) = digest_scope_household(&db).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = digest_day();
    set_digest_scope(&db.pool, household.household_id, petya, DigestScope::All).await;
    let chat = chat_id(&db.pool, petya).await;

    let stats = scheduler(&db.pool)
        .tick(local_utc(moscow, day, 8, 30))
        .await
        .expect("all digest tick");
    assert_eq!(stats.delivered, 1);

    let sent = messages(&db.pool, chat).await;
    assert_eq!(sent.len(), 1, "{sent:?}");
    let html = &sent[0];
    assert!(html.contains("Задача Пети"), "{html}");
    assert!(html.contains("Общая задача"), "{html}");
    assert!(html.contains("Задача Маши"), "{html}");
    assert!(html.contains("Общее событие"), "{html}");
    assert!(html.contains("Событие Маши"), "{html}");
    // Столько же покупок, сколько в составе «только мои».
    assert!(html.contains("🛒 Покупки · 1"), "{html}");
}

/// Перерисовка сводки из снимка после назначения общей задачи Маше:
/// в «только мои» строка скрыта и итоги пересчитаны, после снятия
/// исполнителя — снова видна с «👥»; в «все в семье» — с «👤 Маша».
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn digest_view_hides_a_task_assigned_to_another_member_in_mine_scope() {
    let db = support::isolated_db().await;
    let (household, petya, masha, scope_items) = digest_scope_household(&db).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = digest_day();
    set_digest_scope(&db.pool, household.household_id, petya, DigestScope::Mine).await;

    let stats = scheduler(&db.pool)
        .tick(local_utc(moscow, day, 8, 30))
        .await
        .expect("mine digest tick");
    assert_eq!(stats.delivered, 1);
    let delivery_id = digest_delivery_id(&db.pool, petya).await;
    let now = local_utc(moscow, day, 9, 0);

    let mut conn = db.pool.acquire().await.expect("conn");
    let shared = items::get(&mut conn, household.household_id, scope_items.shared_task)
        .await
        .expect("shared task");
    drop(conn);

    // Общую задачу назначили Маше после доставки.
    let shared = update_assignee(
        &db,
        &household,
        &household.owner(),
        &shared,
        Some(masha),
        now,
    )
    .await;
    let mut conn = db.pool.acquire().await.expect("conn");
    let view = tasklerq::jobs::digest::digest_view(&mut conn, delivery_id)
        .await
        .expect("digest view");
    drop(conn);
    let html = digest_view_html(&view);
    assert!(!html.contains("Общая задача"), "{html}");
    assert!(html.contains("Задача Пети"), "{html}");
    // Итоги пересчитаны без исчезнувшей задачи: 1 задача · 2 события.
    assert!(html.contains("1 задача · 2 события"), "{html}");
    assert!(!html.contains("2 задачи"), "{html}");
    // Снимок при этом не меняется.
    let (snapshot, _hash) = digest_snapshot_and_hash(&db.pool, petya).await;
    assert!(
        snapshot
            .iter()
            .any(|line| line.item_id == scope_items.shared_task)
    );

    // Исполнителя сняли — задача снова видна с «👥».
    update_assignee(&db, &household, &household.owner(), &shared, None, now).await;
    let mut conn = db.pool.acquire().await.expect("conn");
    let view = tasklerq::jobs::digest::digest_view(&mut conn, delivery_id)
        .await
        .expect("digest view");
    drop(conn);
    let html = digest_view_html(&view);
    assert!(html.contains("☐ Общая задача"), "{html}");
    assert!(html.contains("👥"), "{html}");
    assert!(html.contains("2 задачи · 2 события"), "{html}");
}

/// В составе «все в семье» назначенная после доставки задача остаётся
/// в сводке с меткой «👤 Маша».
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn digest_view_labels_a_task_assigned_to_another_member_in_all_scope() {
    let db = support::isolated_db().await;
    let (household, petya, masha, scope_items) = digest_scope_household(&db).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = digest_day();
    set_digest_scope(&db.pool, household.household_id, petya, DigestScope::All).await;

    let stats = scheduler(&db.pool)
        .tick(local_utc(moscow, day, 8, 30))
        .await
        .expect("all digest tick");
    assert_eq!(stats.delivered, 1);
    let delivery_id = digest_delivery_id(&db.pool, petya).await;
    let now = local_utc(moscow, day, 9, 0);

    let mut conn = db.pool.acquire().await.expect("conn");
    let shared = items::get(&mut conn, household.household_id, scope_items.shared_task)
        .await
        .expect("shared task");
    drop(conn);
    update_assignee(
        &db,
        &household,
        &household.owner(),
        &shared,
        Some(masha),
        now,
    )
    .await;

    let mut conn = db.pool.acquire().await.expect("conn");
    let view = tasklerq::jobs::digest::digest_view(&mut conn, delivery_id)
        .await
        .expect("digest view");
    drop(conn);
    let html = digest_view_html(&view);
    assert!(html.contains("Общая задача"), "{html}");
    assert!(html.contains("👤 Маша"), "{html}");
    assert!(html.contains("3 задачи · 3 события"), "{html}");
}

/// После доставки `digest_hash` равен отпечатку отрисовки
/// `digest_ui(digest_view(...))` этой доставки.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn digest_hash_matches_the_fingerprint_of_the_delivery_render() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = digest_day();
    set_household_timezone(&db.pool, household.household_id, "Europe/Moscow").await;
    enable_digest(&db.pool, household.owner_id).await;
    create_task(
        &db,
        &household,
        "Задача сегодня",
        Some(local_utc(moscow, day, 18, 0)),
        Some(household.owner_id),
        None,
    )
    .await;
    create_event(
        &db,
        &household,
        "Событие сегодня",
        local_utc(moscow, day, 14, 0),
        local_utc(moscow, day, 15, 0),
    )
    .await;

    let stats = scheduler(&db.pool)
        .tick(local_utc(moscow, day, 8, 30))
        .await
        .expect("digest tick");
    assert_eq!(stats.delivered, 1);

    let (_snapshot, hash) = digest_snapshot_and_hash(&db.pool, household.owner_id).await;
    let delivery_id = digest_delivery_id(&db.pool, household.owner_id).await;
    let mut conn = db.pool.acquire().await.expect("conn");
    let view = tasklerq::jobs::digest::digest_view(&mut conn, delivery_id)
        .await
        .expect("digest view");
    let ui = tasklerq::telegram::bot::notify::digest_ui(&view);
    assert_eq!(hash, tasklerq::telegram::bot::notify::ui_fingerprint(&ui));
}

/// Повторяющееся событие, сдвинутое DomainTicker на следующее вхождение,
/// остаётся в сводке со своим временем из снимка.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn digest_view_keeps_the_event_time_from_the_snapshot() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = digest_day();
    set_household_timezone(&db.pool, household.household_id, "Europe/Moscow").await;
    set_notifications(
        &db.pool,
        household.owner_id,
        json!({"digest_enabled": true, "reminders_enabled": false, "quiet_hours_enabled": false}),
    )
    .await;
    let start = local_utc(moscow, day, 7, 0);
    create_recurring_event(
        &db,
        &household,
        "Бассейн",
        start,
        start + Duration::hours(1),
        // 30 октября 2026 — пятница.
        support::recurrence(RecurrenceFreq::Weekly, vec![5]),
    )
    .await;
    let task = create_task(
        &db,
        &household,
        "Задача",
        Some(local_utc(moscow, day, 18, 0)),
        Some(household.owner_id),
        None,
    )
    .await;

    let stats = scheduler(&db.pool)
        .tick(local_utc(moscow, day, 8, 30))
        .await
        .expect("digest tick");
    assert_eq!(stats.delivered, 1);
    let delivery_id = digest_delivery_id(&db.pool, household.owner_id).await;

    // Событие прошло, и DomainTicker сдвинул его на следующую неделю.
    ticker(&db.pool)
        .tick(local_utc(moscow, day, 9, 0))
        .await
        .expect("event moves to the next occurrence");
    let mut conn = db.pool.acquire().await.expect("conn");
    items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        task.id,
        None,
        "Europe/Moscow",
        local_utc(moscow, day, 9, 0),
    )
    .await
    .expect("complete task");
    let view = tasklerq::jobs::digest::digest_view(&mut conn, delivery_id)
        .await
        .expect("digest view");
    drop(conn);

    let html = digest_view_html(&view);
    assert!(html.contains("🗓 Бассейн · 07:00–08:00"), "{html}");
    assert!(html.contains("✅ Задача"), "{html}");
    assert!(html.contains("Выполнено 1 из 1"), "{html}");
}

/// Выполненная из сводки ежедневная задача показывает «следующее завтра» —
/// текущий срок повторяющейся задачи относительно дня сводки.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn digest_view_shows_the_next_due_of_a_completed_daily_task() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = digest_day();
    set_household_timezone(&db.pool, household.household_id, "Europe/Moscow").await;
    set_notifications(
        &db.pool,
        household.owner_id,
        json!({"digest_enabled": true, "reminders_enabled": false, "quiet_hours_enabled": false}),
    )
    .await;
    let mut item = support::new_item(household.tasks_list_id, "Вынести мусор");
    item.due_date = Some(day);
    item.assignee_user_id = Some(household.owner_id);
    item.recurrence = Some(support::recurrence(RecurrenceFreq::Daily, vec![]));
    let task = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        item,
    )
    .await;

    let stats = scheduler(&db.pool)
        .tick(local_utc(moscow, day, 8, 30))
        .await
        .expect("digest tick");
    assert_eq!(stats.delivered, 1);
    let delivery_id = digest_delivery_id(&db.pool, household.owner_id).await;

    let mut conn = db.pool.acquire().await.expect("conn");
    let task = items::get(&mut conn, household.household_id, task.id)
        .await
        .expect("task");
    items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        task.id,
        Some(task.version),
        "Europe/Moscow",
        local_utc(moscow, day, 9, 0),
    )
    .await
    .expect("complete daily task");
    let view = tasklerq::jobs::digest::digest_view(&mut conn, delivery_id)
        .await
        .expect("digest view");
    drop(conn);

    let html = digest_view_html(&view);
    assert!(
        html.contains("✅ Вынести мусор · следующее завтра"),
        "{html}"
    );
    assert!(html.contains("Выполнено 1 из 1"), "{html}");
}

/// Выполненная неповторяющаяся задача без действующего события выполнения
/// (событие удалено хранением): строка «✅ <название>» без имени и без кнопки,
/// а не открытая строка с кнопкой (design, решение 2).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn digest_view_shows_a_done_task_without_a_completion_event() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = digest_day();
    set_household_timezone(&db.pool, household.household_id, "Europe/Moscow").await;
    enable_digest(&db.pool, household.owner_id).await;
    let task = create_all_day_task(
        &db,
        &household,
        "Вынести мусор",
        day,
        Some(household.owner_id),
    )
    .await;

    let stats = scheduler(&db.pool)
        .tick(local_utc(moscow, day, 8, 30))
        .await
        .expect("digest tick");
    assert_eq!(stats.delivered, 1);
    let delivery_id = digest_delivery_id(&db.pool, household.owner_id).await;

    let mut conn = db.pool.acquire().await.expect("conn");
    items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        task.id,
        None,
        "Europe/Moscow",
        local_utc(moscow, day, 9, 0),
    )
    .await
    .expect("complete task");
    // Действующего события выполнения больше нет (например, удалено хранением),
    // но задача осталась выполненной.
    sqlx::query("DELETE FROM item_events WHERE item_id = $1 AND event_type = 'completed'")
        .bind(task.id)
        .execute(&mut *conn)
        .await
        .expect("purge completion event");
    let view = tasklerq::jobs::digest::digest_view(&mut conn, delivery_id)
        .await
        .expect("digest view");
    drop(conn);

    assert!(matches!(
        view.lines[0].state,
        tasklerq::telegram::bot::notify::DigestLineState::Completed {
            can_undo: false,
            actor_name: None,
            completion_event: None,
            ..
        }
    ));
    let html = digest_view_html(&view);
    assert!(html.contains("✅ Вынести мусор"), "{html}");
    assert!(!html.contains("☐ Вынести мусор"), "{html}");
    assert!(!html.contains("✅ Вынести мусор ·"), "{html}");
}

/// Подписи сводки относительны к дню сводки, а не к моменту отрисовки:
/// вседневная задача дня сводки — «весь день», вчерашняя — «вчера, 12:00»,
/// следующая у выполненной ежедневной — «следующее завтра».
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn digest_view_labels_are_relative_to_the_digest_day() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = digest_day();
    set_household_timezone(&db.pool, household.household_id, "Europe/Moscow").await;
    set_notifications(
        &db.pool,
        household.owner_id,
        json!({"digest_enabled": true, "reminders_enabled": false, "quiet_hours_enabled": false}),
    )
    .await;
    create_all_day_task(
        &db,
        &household,
        "Сегодняшняя",
        day,
        Some(household.owner_id),
    )
    .await;
    create_task(
        &db,
        &household,
        "Просроченная",
        Some(local_utc(moscow, day - Duration::days(1), 12, 0)),
        Some(household.owner_id),
        None,
    )
    .await;
    let mut daily = support::new_item(household.tasks_list_id, "Ежедневная");
    daily.due_date = Some(day);
    daily.assignee_user_id = Some(household.owner_id);
    daily.recurrence = Some(support::recurrence(RecurrenceFreq::Daily, vec![]));
    let daily = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        daily,
    )
    .await;

    let stats = scheduler(&db.pool)
        .tick(local_utc(moscow, day, 8, 30))
        .await
        .expect("digest tick");
    assert_eq!(stats.delivered, 1);
    let delivery_id = digest_delivery_id(&db.pool, household.owner_id).await;

    let mut conn = db.pool.acquire().await.expect("conn");
    let daily = items::get(&mut conn, household.household_id, daily.id)
        .await
        .expect("daily task");
    items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        daily.id,
        Some(daily.version),
        "Europe/Moscow",
        local_utc(moscow, day, 9, 0),
    )
    .await
    .expect("complete daily task");

    let view = tasklerq::jobs::digest::digest_view(&mut conn, delivery_id)
        .await
        .expect("digest view");
    let html = digest_view_html(&view);
    assert!(html.contains("☐ Сегодняшняя · весь день"), "{html}");
    assert!(html.contains("☐ Просроченная · вчера, 12:00"), "{html}");
    assert!(html.contains("✅ Ежедневная · следующее завтра"), "{html}");
}

/// Удалённая задача, задача из архивного списка и удалённое событие — текст
/// без кнопки.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn digest_view_shows_deleted_archived_and_deleted_event_lines() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = digest_day();
    set_household_timezone(&db.pool, household.household_id, "Europe/Moscow").await;
    enable_digest(&db.pool, household.owner_id).await;
    let deleted =
        create_all_day_task(&db, &household, "Удалённая", day, Some(household.owner_id)).await;
    // Архивный список — не по умолчанию (список по умолчанию архивировать нельзя).
    let mut conn = db.pool.acquire().await.expect("conn");
    let archive_list = lists::create(
        &mut conn,
        household.household_id,
        ListKind::Tasks,
        "Архив",
        Utc::now(),
    )
    .await
    .expect("archive list");
    drop(conn);
    let mut archived_item = support::new_item(archive_list.id, "Архивная");
    archived_item.due_date = Some(day);
    archived_item.assignee_user_id = Some(household.owner_id);
    let _archived = support::create_item(
        &db.pool,
        &household,
        archive_list.id,
        &household.owner(),
        archived_item,
    )
    .await;
    let event = create_event(
        &db,
        &household,
        "Удалённое событие",
        local_utc(moscow, day, 14, 0),
        local_utc(moscow, day, 15, 0),
    )
    .await;

    let stats = scheduler(&db.pool)
        .tick(local_utc(moscow, day, 8, 30))
        .await
        .expect("digest tick");
    assert_eq!(stats.delivered, 1);
    let delivery_id = digest_delivery_id(&db.pool, household.owner_id).await;

    let mut conn = db.pool.acquire().await.expect("conn");
    items::soft_delete(
        &mut conn,
        household.household_id,
        &household.owner(),
        deleted.id,
        local_utc(moscow, day, 9, 0),
    )
    .await
    .expect("delete task");
    items::soft_delete(
        &mut conn,
        household.household_id,
        &household.owner(),
        event.id,
        local_utc(moscow, day, 9, 0),
    )
    .await
    .expect("delete event");
    sqlx::query("UPDATE lists SET is_archived = true WHERE id = $1")
        .bind(archive_list.id)
        .execute(&mut *conn)
        .await
        .expect("archive list");
    let view = tasklerq::jobs::digest::digest_view(&mut conn, delivery_id)
        .await
        .expect("digest view");
    drop(conn);

    let html = digest_view_html(&view);
    assert!(html.contains("Удалённая — удалена"), "{html}");
    assert!(html.contains("Архивная — в архиве"), "{html}");
    assert!(html.contains("Удалённое событие — удалено"), "{html}");
    // Ни одна из этих строк не кнопка.
    let labels: Vec<String> = buttons(&html).into_iter().map(|(label, _)| label).collect();
    assert!(
        !labels.iter().any(|label| label.contains("Удалённая")
            || label.contains("Архивная")
            || label.contains("Удалённое событие")),
        "{labels:?}"
    );
}

/// Выполнение другим участником: «✅ … · Маша» без кнопки у участника-не
/// владельца и с кнопкой у владельца.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn digest_view_completed_by_another_member_button_follows_the_role() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = digest_day();
    set_household_timezone(&db.pool, household.household_id, "Europe/Moscow").await;
    enable_digest(&db.pool, household.owner_id).await;
    let petya = add_member(&db.pool, household.household_id, "Петя", true).await;
    let masha = add_member(&db.pool, household.household_id, "Маша", false).await;
    let task = create_task(
        &db,
        &household,
        "Вынести мусор",
        Some(local_utc(moscow, day, 18, 0)),
        None,
        None,
    )
    .await;

    let stats = scheduler(&db.pool)
        .tick(local_utc(moscow, day, 8, 30))
        .await
        .expect("digest tick");
    assert_eq!(stats.delivered, 2, "owner and Petya digests");
    let owner_delivery = digest_delivery_id(&db.pool, household.owner_id).await;
    let petya_delivery = digest_delivery_id(&db.pool, petya).await;

    let mut conn = db.pool.acquire().await.expect("conn");
    items::complete(
        &mut conn,
        household.household_id,
        &Actor::user(masha, UserRole::Member),
        task.id,
        None,
        "Europe/Moscow",
        local_utc(moscow, day, 9, 0),
    )
    .await
    .expect("masha completes");

    let petya_view = tasklerq::jobs::digest::digest_view(&mut conn, petya_delivery)
        .await
        .expect("petya view");
    let owner_view = tasklerq::jobs::digest::digest_view(&mut conn, owner_delivery)
        .await
        .expect("owner view");
    drop(conn);

    // Петя-не владелец: «✅ … · Маша» без кнопки.
    let petya_html = digest_view_html(&petya_view);
    assert!(
        petya_html.contains("✅ Вынести мусор · Маша"),
        "{petya_html}"
    );
    assert!(
        !buttons(&petya_html)
            .iter()
            .any(|(label, _)| label.contains("Вынести мусор")),
        "{petya_html}"
    );
    // Владелец: та же строка с кнопкой (касание — отмена).
    let owner_html = digest_view_html(&owner_view);
    assert!(
        owner_html.contains("✅ Вынести мусор · Маша"),
        "{owner_html}"
    );
    assert!(
        buttons(&owner_html)
            .iter()
            .any(|(label, _)| label.contains("✅ Вынести мусор · Маша")),
        "{owner_html}"
    );
}

/// Выполнение самим получателем, без участника (HA) и исключённым участником —
/// без имени.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn digest_view_completed_line_hides_the_name_for_recipient_system_and_excluded() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = digest_day();
    set_household_timezone(&db.pool, household.household_id, "Europe/Moscow").await;
    enable_digest(&db.pool, household.owner_id).await;
    let masha = add_member(&db.pool, household.household_id, "Маша", false).await;
    let by_recipient =
        create_all_day_task(&db, &household, "Своя", day, Some(household.owner_id)).await;
    let by_system = create_all_day_task(
        &db,
        &household,
        "Без участника",
        day,
        Some(household.owner_id),
    )
    .await;
    let by_excluded = create_all_day_task(
        &db,
        &household,
        "Исключённым",
        day,
        Some(household.owner_id),
    )
    .await;

    let stats = scheduler(&db.pool)
        .tick(local_utc(moscow, day, 8, 30))
        .await
        .expect("digest tick");
    assert_eq!(stats.delivered, 1);
    let delivery_id = digest_delivery_id(&db.pool, household.owner_id).await;

    let mut conn = db.pool.acquire().await.expect("conn");
    let now = local_utc(moscow, day, 9, 0);
    items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        by_recipient.id,
        None,
        "Europe/Moscow",
        now,
    )
    .await
    .expect("recipient completes");
    items::complete(
        &mut conn,
        household.household_id,
        &Actor::system(ActorType::HomeAssistant),
        by_system.id,
        None,
        "Europe/Moscow",
        now,
    )
    .await
    .expect("system completes");
    items::complete(
        &mut conn,
        household.household_id,
        &Actor::user(masha, UserRole::Member),
        by_excluded.id,
        None,
        "Europe/Moscow",
        now,
    )
    .await
    .expect("masha completes");
    // Машу исключили из household после выполнения.
    sqlx::query("UPDATE users SET deleted_at = now() WHERE id = $1")
        .bind(masha)
        .execute(&mut *conn)
        .await
        .expect("exclude masha");
    let view = tasklerq::jobs::digest::digest_view(&mut conn, delivery_id)
        .await
        .expect("digest view");
    drop(conn);

    let html = digest_view_html(&view);
    assert!(html.contains("✅ Своя"), "{html}");
    assert!(html.contains("✅ Без участника"), "{html}");
    assert!(html.contains("✅ Исключённым"), "{html}");
    assert!(!html.contains("✅ Своя ·"), "{html}");
    assert!(!html.contains("✅ Без участника ·"), "{html}");
    assert!(!html.contains("✅ Исключённым ·"), "{html}");
    assert!(!html.contains("Маша"), "{html}");
}

/// В составе «только мои» событие, ставшее чужим после доставки, скрыто
/// (фильтр состава по текущему участнику — как для задач).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn digest_view_hides_an_event_assigned_to_another_member_in_mine_scope() {
    let db = support::isolated_db().await;
    let (household, petya, masha, scope_items) = digest_scope_household(&db).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = digest_day();
    set_digest_scope(&db.pool, household.household_id, petya, DigestScope::Mine).await;

    let stats = scheduler(&db.pool)
        .tick(local_utc(moscow, day, 8, 30))
        .await
        .expect("mine digest tick");
    assert_eq!(stats.delivered, 1);
    let delivery_id = digest_delivery_id(&db.pool, petya).await;

    // Общее событие из снимка Пети назначили Маше после доставки.
    assign_without_notice(&db.pool, scope_items.shared_event, masha).await;
    let mut conn = db.pool.acquire().await.expect("conn");
    let view = tasklerq::jobs::digest::digest_view(&mut conn, delivery_id)
        .await
        .expect("digest view");
    drop(conn);

    let html = digest_view_html(&view);
    assert!(!html.contains("Общее событие"), "{html}");
    assert!(html.contains("Событие Пети"), "{html}");
    // Итоги пересчитаны без исчезнувшего события.
    assert!(html.contains("2 задачи · 1 событие"), "{html}");
}

/// Отпечаток не зависит от токенов: два `render_notice` одного `Ui` дают
/// разные токены в HTML, а `ui_fingerprint` — одинаковый.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn digest_fingerprint_does_not_depend_on_notice_tokens() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = digest_day();
    enable_digest(&db.pool, household.owner_id).await;
    create_task(
        &db,
        &household,
        "Задача сегодня",
        Some(local_utc(moscow, day, 18, 0)),
        Some(household.owner_id),
        None,
    )
    .await;

    let stats = scheduler(&db.pool)
        .tick(local_utc(moscow, day, 8, 30))
        .await
        .expect("digest tick");
    assert_eq!(stats.delivered, 1);

    let delivery_id = digest_delivery_id(&db.pool, household.owner_id).await;
    // В фикстурах telegram_user_id совпадает с telegram_chat_id.
    let telegram_id = chat_id(&db.pool, household.owner_id).await;
    let mut conn = db.pool.acquire().await.expect("conn");
    let view = tasklerq::jobs::digest::digest_view(&mut conn, delivery_id)
        .await
        .expect("digest view");
    let ui = tasklerq::telegram::bot::notify::digest_ui(&view);
    let before = tasklerq::telegram::bot::notify::ui_fingerprint(&ui);
    let now = local_utc(moscow, day, 9, 0);
    let first = tasklerq::telegram::bot::notify::render_notice(
        &mut conn,
        telegram_id,
        household.household_id,
        &ui,
        now,
    )
    .await
    .expect("first render");
    let second = tasklerq::telegram::bot::notify::render_notice(
        &mut conn,
        telegram_id,
        household.household_id,
        &ui,
        now,
    )
    .await
    .expect("second render");
    assert_ne!(first, second, "tokens must differ between renders");
    assert_eq!(before, tasklerq::telegram::bot::notify::ui_fingerprint(&ui));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn digest_missed_at_its_time_still_fires_once_later_the_same_day() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = digest_day();
    set_household_timezone(&db.pool, household.household_id, "Europe/Moscow").await;
    enable_digest(&db.pool, household.owner_id).await;
    let chat = chat_id(&db.pool, household.owner_id).await;

    let stats = scheduler(&db.pool)
        .tick(local_utc(moscow, day, 15, 0))
        .await
        .expect("afternoon tick");
    assert_eq!(stats.delivered, 1);

    assert_eq!(messages(&db.pool, chat).await.len(), 1);
    let rows = digest_rows(&db.pool, household.owner_id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, format!("digest:{day}"));
    assert_eq!(rows[0].1, "sent");
    assert_eq!(rows[0].2, None);
    assert!(rows[0].3.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pending_digest_from_yesterday_is_cancelled_and_not_sent() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = digest_day();
    let yesterday = day - Duration::days(1);
    set_household_timezone(&db.pool, household.household_id, "Europe/Moscow").await;
    enable_digest(&db.pool, household.owner_id).await;
    insert_digest_delivery(
        &db.pool,
        household.household_id,
        household.owner_id,
        &format!("digest:{yesterday}"),
        local_utc(moscow, yesterday, 8, 30),
    )
    .await;
    let chat = chat_id(&db.pool, household.owner_id).await;

    let stats = scheduler(&db.pool)
        .tick(local_utc(moscow, day, 8, 30))
        .await
        .expect("digest tick");
    assert_eq!(stats.cancelled, 1);
    assert_eq!(stats.delivered, 1);

    // Отправлена только сегодняшняя сводка; вчерашняя запись отменена.
    assert_eq!(messages(&db.pool, chat).await.len(), 1);
    let rows = digest_rows(&db.pool, household.owner_id).await;
    assert_eq!(rows.len(), 2, "{rows:?}");
    let yesterday_row = rows
        .iter()
        .find(|row| row.0 == format!("digest:{yesterday}"))
        .expect("yesterday row");
    assert_eq!(yesterday_row.1, "cancelled");
    let today_row = rows
        .iter()
        .find(|row| row.0 == format!("digest:{day}"))
        .expect("today row");
    assert_eq!(today_row.1, "sent");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn digest_for_a_deleted_user_is_cancelled() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = digest_day();
    set_household_timezone(&db.pool, household.household_id, "Europe/Moscow").await;
    set_notifications(
        &db.pool,
        household.member_id,
        json!({"digest_enabled": true}),
    )
    .await;
    insert_digest_delivery(
        &db.pool,
        household.household_id,
        household.member_id,
        &format!("digest:{day}"),
        local_utc(moscow, day, 8, 30),
    )
    .await;
    sqlx::query("UPDATE users SET deleted_at = now() WHERE id = $1")
        .bind(household.member_id)
        .execute(&db.pool)
        .await
        .expect("delete user");
    let member_chat = chat_id(&db.pool, household.member_id).await;

    let stats = scheduler(&db.pool)
        .tick(local_utc(moscow, day, 8, 30))
        .await
        .expect("digest tick");
    assert_eq!(stats.cancelled, 1);
    assert_eq!(stats.delivered, 0);
    assert!(messages(&db.pool, member_chat).await.is_empty());
    let rows = digest_rows(&db.pool, household.member_id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].1, "cancelled");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn archived_list_tasks_get_no_reminders_and_no_digest() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = digest_day();
    set_household_timezone(&db.pool, household.household_id, "Europe/Moscow").await;
    let due = local_utc(moscow, day, 12, 0);
    let item = create_task(
        &db,
        &household,
        "Задача в архиве",
        Some(due),
        Some(household.owner_id),
        None,
    )
    .await;
    sqlx::query("UPDATE lists SET is_default = false, is_archived = true WHERE id = $1")
        .bind(household.tasks_list_id)
        .execute(&db.pool)
        .await
        .expect("archive list");
    let chat = chat_id(&db.pool, household.owner_id).await;

    let reminder = scheduler(&db.pool)
        .tick(due - Duration::minutes(15))
        .await
        .expect("reminder tick");
    assert_eq!(reminder.planned, 0);
    assert_eq!(reminder.delivered, 0);
    assert!(delivery_rows(&db.pool, item.id).await.is_empty());

    enable_digest(&db.pool, household.owner_id).await;
    let digest = scheduler(&db.pool)
        .tick(local_utc(moscow, day, 8, 30))
        .await
        .expect("digest tick");
    assert_eq!(digest.delivered, 1);
    let sent = messages(&db.pool, chat).await;
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert!(!sent[0].contains("Задача в архиве"), "{sent:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn editor_assignment_with_other_fields_notifies_once() {
    let db = support::db().await;
    let mut bot = Harness::new(&db.pool, next_telegram_id()).named("Alice");
    bot.text("/start").await;
    bot.text("Семья").await;
    bot.press("Москва").await;
    bot.press("меню").await;
    bot.text("Одно уведомление").await;
    bot.press("Сохранить").await;

    let (household_id, item_id): (Uuid, Uuid) = sqlx::query_as(
        "SELECT i.household_id, i.id FROM items i
         JOIN users u ON u.household_id = i.household_id
         WHERE u.telegram_user_id = $1 AND i.deleted_at IS NULL",
    )
    .bind(bot.chat)
    .fetch_one(&db.pool)
    .await
    .expect("item");

    let member = new_member("Борис");
    let member = {
        let mut conn = db.pool.acquire().await.expect("conn");
        users::add(
            &mut conn,
            household_id,
            UserRole::Member,
            member,
            &Actor::system(ActorType::System),
            Utc::now(),
        )
        .await
        .expect("member")
    };

    bot.press("Одно уведомление").await;
    bot.press("Править").await;
    bot.press("Срок").await;
    bot.press("Завтра").await;
    bot.press("Исполнитель").await;
    bot.press("Борис").await;
    bot.press("Сохранить").await;

    let rows = assignment_rows(&db.pool, item_id, member.id).await;
    assert_eq!(rows.len(), 1, "one save must notify once: {rows:?}");
    assert_eq!(rows[0].0, "pending");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn assignment_notifies_the_assignee_but_not_the_actor() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let due = rounded_now() + Duration::hours(3);
    let item = create_task(&db, &household, "Помыть посуду", Some(due), None, None).await;
    let now = Utc::now();
    update_assignee(
        &db,
        &household,
        &household.owner(),
        &item,
        Some(household.member_id),
        now,
    )
    .await;

    let member_chat = chat_id(&db.pool, household.member_id).await;
    let owner_chat = chat_id(&db.pool, household.owner_id).await;
    let stats = scheduler(&db.pool)
        .tick(now + Duration::seconds(1))
        .await
        .expect("assignment tick");
    assert_eq!(stats.delivered, 1);

    let member_messages = messages(&db.pool, member_chat).await;
    assert_eq!(member_messages.len(), 1, "{member_messages:?}");
    assert!(member_messages[0].contains("Вам назначили задачу"));
    assert!(member_messages[0].contains("Помыть посуду"));
    assert!(member_messages[0].contains("Задачи"));
    assert!(member_messages[0].contains("Открыть"));
    assert!(messages(&db.pool, owner_chat).await.is_empty());

    let rows = assignment_rows(&db.pool, item.id, household.member_id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "sent");
    assert!(rows[0].2.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn assigning_a_task_to_self_sends_nothing() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let item = create_task(&db, &household, "Своя задача", None, None, None).await;
    let now = Utc::now();
    update_assignee(
        &db,
        &household,
        &household.owner(),
        &item,
        Some(household.owner_id),
        now,
    )
    .await;

    let owner_chat = chat_id(&db.pool, household.owner_id).await;
    let stats = scheduler(&db.pool)
        .tick(now + Duration::seconds(1))
        .await
        .expect("self-assign tick");
    assert_eq!(stats.delivered, 0);
    assert!(messages(&db.pool, owner_chat).await.is_empty());
    assert!(
        assignment_rows(&db.pool, item.id, household.owner_id)
            .await
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeated_assignment_to_the_same_assignee_does_not_notify_again() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let item = create_task(&db, &household, "Повторное назначение", None, None, None).await;
    let now = Utc::now();
    let first = update_assignee(
        &db,
        &household,
        &household.owner(),
        &item,
        Some(household.member_id),
        now,
    )
    .await;
    let chat = chat_id(&db.pool, household.member_id).await;

    assert_eq!(
        scheduler(&db.pool)
            .tick(now + Duration::seconds(1))
            .await
            .expect("first tick")
            .delivered,
        1
    );

    let second = update_assignee(
        &db,
        &household,
        &household.owner(),
        &first,
        Some(household.member_id),
        now + Duration::seconds(2),
    )
    .await;
    assert_eq!(second.assignee_user_id, Some(household.member_id));
    assert_eq!(
        second.version, first.version,
        "unchanged PATCH must not bump version"
    );

    assert_eq!(
        scheduler(&db.pool)
            .tick(now + Duration::seconds(3))
            .await
            .expect("repeat tick")
            .delivered,
        0
    );
    assert_eq!(messages(&db.pool, chat).await.len(), 1);
    let rows = assignment_rows(&db.pool, item.id, household.member_id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "sent");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unassigning_a_task_sends_nothing() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let item = create_task(
        &db,
        &household,
        "Снять исполнителя",
        None,
        Some(household.member_id),
        None,
    )
    .await;
    let now = Utc::now();
    assert_eq!(
        scheduler(&db.pool)
            .tick(now + Duration::seconds(1))
            .await
            .expect("assignment tick")
            .delivered,
        1
    );
    let chat = chat_id(&db.pool, household.member_id).await;

    update_assignee(
        &db,
        &household,
        &household.owner(),
        &item,
        None,
        now + Duration::seconds(2),
    )
    .await;
    assert_eq!(
        scheduler(&db.pool)
            .tick(now + Duration::seconds(3))
            .await
            .expect("unassign tick")
            .delivered,
        0
    );
    assert_eq!(messages(&db.pool, chat).await.len(), 1);
    let rows = assignment_rows(&db.pool, item.id, household.member_id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_to_b_to_a_notifies_a_twice() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let extra = add_member(&db.pool, household.household_id, "Extra member", false).await;
    let item = create_task(&db, &household, "Переходящая задача", None, None, None).await;
    let now = Utc::now();
    let actor = household.owner();

    let first = update_assignee(
        &db,
        &household,
        &actor,
        &item,
        Some(household.member_id),
        now,
    )
    .await;
    assert_eq!(
        scheduler(&db.pool)
            .tick(now + Duration::seconds(1))
            .await
            .expect("first assignment tick")
            .delivered,
        1
    );
    let second = update_assignee(
        &db,
        &household,
        &actor,
        &first,
        Some(extra),
        now + Duration::seconds(2),
    )
    .await;
    assert_eq!(
        scheduler(&db.pool)
            .tick(now + Duration::seconds(3))
            .await
            .expect("second assignment tick")
            .delivered,
        1
    );
    update_assignee(
        &db,
        &household,
        &actor,
        &second,
        Some(household.member_id),
        now + Duration::seconds(4),
    )
    .await;

    let member_chat = chat_id(&db.pool, household.member_id).await;
    let extra_chat = chat_id(&db.pool, extra).await;
    let stats = scheduler(&db.pool)
        .tick(now + Duration::seconds(5))
        .await
        .expect("third assignment tick");
    assert_eq!(stats.delivered, 1);

    assert_eq!(messages(&db.pool, member_chat).await.len(), 2);
    assert_eq!(messages(&db.pool, extra_chat).await.len(), 1);
    assert_eq!(
        assignment_rows(&db.pool, item.id, household.member_id)
            .await
            .len(),
        2
    );
    assert_eq!(assignment_rows(&db.pool, item.id, extra).await.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn voice_capture_with_an_assignee_notifies_the_assignee_only() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let mut item = support::new_item(household.tasks_list_id, "Голосовая задача");
    item.assignee_user_id = Some(household.member_id);
    item.source = ItemSource::HaVoice;
    let created = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &Actor::system(ActorType::Voice),
        item,
    )
    .await;
    assert!(created.created_by_user_id.is_none());

    let now = Utc::now();
    let member_chat = chat_id(&db.pool, household.member_id).await;
    let owner_chat = chat_id(&db.pool, household.owner_id).await;
    let stats = scheduler(&db.pool)
        .tick(now + Duration::seconds(1))
        .await
        .expect("assignment tick");
    assert_eq!(stats.delivered, 1);

    let member_messages = messages(&db.pool, member_chat).await;
    assert_eq!(member_messages.len(), 1, "{member_messages:?}");
    assert!(member_messages[0].contains("Вам назначили задачу"));
    assert!(member_messages[0].contains("Голосовая задача"));
    assert!(messages(&db.pool, owner_chat).await.is_empty());
    assert_eq!(
        assignment_rows(&db.pool, created.id, household.member_id)
            .await
            .len(),
        1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn changing_recurrence_and_assignee_together_still_notifies() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let due = rounded_now() + Duration::hours(3);
    let item = create_task(
        &db,
        &household,
        "Повторяющаяся задача",
        Some(due),
        None,
        None,
    )
    .await;
    let now = Utc::now();
    let mut conn = db.pool.acquire().await.expect("conn");
    let updated = items::update(
        &mut conn,
        household.household_id,
        &household.owner(),
        item.id,
        item.version,
        ItemPatch {
            assignee_user_id: Field::Value(household.member_id),
            recurrence: Field::Value(support::recurrence(RecurrenceFreq::Daily, Vec::new())),
            ..ItemPatch::default()
        },
        "Europe/Moscow",
        now,
    )
    .await
    .expect("update recurrence and assignee");

    let chat = chat_id(&db.pool, household.member_id).await;
    let stats = scheduler(&db.pool)
        .tick(now + Duration::seconds(1))
        .await
        .expect("assignment tick");
    assert_eq!(stats.delivered, 1);
    let sent = messages(&db.pool, chat).await;
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert!(sent[0].contains("Вам назначили задачу"));
    assert_eq!(
        assignment_rows(&db.pool, updated.id, household.member_id)
            .await
            .len(),
        1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disabled_assignment_notifications_cancel_the_pending_delivery() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let item = create_task(
        &db,
        &household,
        "Без уведомлений о назначении",
        None,
        Some(household.member_id),
        None,
    )
    .await;
    set_notifications_without_digest(
        &db.pool,
        household.member_id,
        json!({"assignment_enabled": false}),
    )
    .await;

    let chat = chat_id(&db.pool, household.member_id).await;
    let now = Utc::now();
    let stats = scheduler(&db.pool)
        .tick(now + Duration::seconds(1))
        .await
        .expect("assignment tick");
    assert_eq!(stats.delivered, 0);
    assert!(messages(&db.pool, chat).await.is_empty());
    let rows = assignment_rows(&db.pool, item.id, household.member_id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "cancelled");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reassigning_before_delivery_cancels_the_first_notification() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let extra = add_member(&db.pool, household.household_id, "Extra member", false).await;
    let item = create_task(&db, &household, "Переназначенная", None, None, None).await;
    let now = Utc::now();
    let actor = household.owner();
    let first = update_assignee(
        &db,
        &household,
        &actor,
        &item,
        Some(household.member_id),
        now,
    )
    .await;
    update_assignee(
        &db,
        &household,
        &actor,
        &first,
        Some(extra),
        now + Duration::seconds(1),
    )
    .await;

    let member_chat = chat_id(&db.pool, household.member_id).await;
    let extra_chat = chat_id(&db.pool, extra).await;
    let stats = scheduler(&db.pool)
        .tick(now + Duration::seconds(2))
        .await
        .expect("reassign tick");
    // Прежняя pending-запись отменена ещё при постановке нового назначения,
    // поэтому тик её уже не считает.
    assert_eq!(stats.cancelled, 0);
    assert_eq!(stats.delivered, 1);

    let member_rows = assignment_rows(&db.pool, item.id, household.member_id).await;
    assert_eq!(member_rows.len(), 1, "{member_rows:?}");
    assert_eq!(member_rows[0].0, "cancelled");
    assert!(messages(&db.pool, member_chat).await.is_empty());
    let extra_rows = assignment_rows(&db.pool, item.id, extra).await;
    assert_eq!(extra_rows.len(), 1, "{extra_rows:?}");
    assert_eq!(extra_rows[0].0, "sent");
    assert_eq!(messages(&db.pool, extra_chat).await.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn assignment_is_cancelled_when_the_item_is_completed_or_deleted() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let completed = create_task(
        &db,
        &household,
        "Выполненная до доставки",
        None,
        Some(household.member_id),
        None,
    )
    .await;
    let deleted = create_task(
        &db,
        &household,
        "Удалённая до доставки",
        None,
        Some(household.member_id),
        None,
    )
    .await;
    let now = Utc::now();
    let mut conn = db.pool.acquire().await.expect("conn");
    items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        completed.id,
        None,
        "Europe/Moscow",
        now,
    )
    .await
    .expect("complete item");
    items::soft_delete(
        &mut conn,
        household.household_id,
        &household.owner(),
        deleted.id,
        now,
    )
    .await
    .expect("delete item");

    let chat = chat_id(&db.pool, household.member_id).await;
    let stats = scheduler(&db.pool)
        .tick(now + Duration::seconds(1))
        .await
        .expect("cancel tick");
    assert_eq!(stats.cancelled, 2);
    assert_eq!(stats.delivered, 0);
    assert!(messages(&db.pool, chat).await.is_empty());
    for item_id in [completed.id, deleted.id] {
        let rows = assignment_rows(&db.pool, item_id, household.member_id).await;
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].0, "cancelled");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeated_ticks_and_restart_do_not_duplicate_an_assignment() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let item = create_task(
        &db,
        &household,
        "Однократное уведомление",
        None,
        Some(household.member_id),
        None,
    )
    .await;
    let chat = chat_id(&db.pool, household.member_id).await;
    let now = Utc::now();

    assert_eq!(
        scheduler(&db.pool)
            .tick(now + Duration::seconds(1))
            .await
            .expect("first tick")
            .delivered,
        1
    );
    assert_eq!(
        scheduler(&db.pool)
            .tick(now + Duration::seconds(2))
            .await
            .expect("restarted tick")
            .delivered,
        0
    );

    assert_eq!(messages(&db.pool, chat).await.len(), 1);
    let rows = assignment_rows(&db.pool, item.id, household.member_id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "sent");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undo_that_restores_the_assignee_notifies_them_again() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let item = create_task(
        &db,
        &household,
        "Возвращённый исполнитель",
        None,
        Some(household.member_id),
        None,
    )
    .await;
    let chat = chat_id(&db.pool, household.member_id).await;
    let now = Utc::now();
    assert_eq!(
        scheduler(&db.pool)
            .tick(now + Duration::seconds(1))
            .await
            .expect("creation assignment tick")
            .delivered,
        1
    );

    let unassigned = update_assignee(
        &db,
        &household,
        &household.owner(),
        &item,
        None,
        now + Duration::seconds(2),
    )
    .await;
    let mut conn = db.pool.acquire().await.expect("conn");
    let restored = items::undo_last(
        &mut conn,
        household.household_id,
        &household.owner(),
        unassigned.id,
        now + Duration::seconds(3),
    )
    .await
    .expect("undo unassignment");
    assert_eq!(restored.assignee_user_id, Some(household.member_id));

    assert_eq!(
        scheduler(&db.pool)
            .tick(now + Duration::seconds(4))
            .await
            .expect("undo assignment tick")
            .delivered,
        1
    );
    assert_eq!(messages(&db.pool, chat).await.len(), 2);
    let rows = assignment_rows(&db.pool, item.id, household.member_id).await;
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert!(rows.iter().all(|row| row.0 == "sent"), "{rows:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn assignment_for_a_deleted_recipient_is_cancelled() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let item = create_task(
        &db,
        &household,
        "Удалённый получатель назначения",
        None,
        Some(household.member_id),
        None,
    )
    .await;
    sqlx::query("UPDATE users SET deleted_at = now() WHERE id = $1")
        .bind(household.member_id)
        .execute(&db.pool)
        .await
        .expect("delete recipient");
    let chat = chat_id(&db.pool, household.member_id).await;
    let now = Utc::now();

    let stats = scheduler(&db.pool)
        .tick(now + Duration::seconds(1))
        .await
        .expect("assignment tick");
    assert_eq!(stats.cancelled, 1);
    assert_eq!(stats.delivered, 0);
    assert!(messages(&db.pool, chat).await.is_empty());
    let rows = assignment_rows(&db.pool, item.id, household.member_id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "cancelled");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn assignment_of_a_shopping_item_notifies_the_assignee() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let item = create_shopping(&db, &household, "Купить молоко").await;
    let now = Utc::now();
    update_assignee(
        &db,
        &household,
        &household.owner(),
        &item,
        Some(household.member_id),
        now,
    )
    .await;
    let chat = chat_id(&db.pool, household.member_id).await;

    let stats = scheduler(&db.pool)
        .tick(now + Duration::seconds(1))
        .await
        .expect("assignment tick");
    assert_eq!(stats.delivered, 1);
    let sent = messages(&db.pool, chat).await;
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert!(sent[0].contains("Вам назначили задачу"));
    assert!(sent[0].contains("Купить молоко"));
    assert_eq!(
        assignment_rows(&db.pool, item.id, household.member_id)
            .await
            .len(),
        1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn creating_a_task_assigned_to_self_sends_nothing() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let item = create_task(
        &db,
        &household,
        "Создана и сразу своя",
        None,
        Some(household.owner_id),
        None,
    )
    .await;
    let chat = chat_id(&db.pool, household.owner_id).await;

    let stats = scheduler(&db.pool)
        .tick(Utc::now() + Duration::seconds(1))
        .await
        .expect("self-assign creation tick");
    assert_eq!(stats.delivered, 0);
    assert!(messages(&db.pool, chat).await.is_empty());
    assert!(
        assignment_rows(&db.pool, item.id, household.owner_id)
            .await
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quiet_hours_defer_a_reminder_to_morning_and_send_it_once() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = digest_day();
    enable_quiet(&db.pool, household.owner_id, "22:00", "08:00").await;
    let due = local_utc(moscow, day, 23, 45);
    let item = create_task(
        &db,
        &household,
        "Ночное напоминание",
        Some(due),
        Some(household.owner_id),
        None,
    )
    .await;
    let chat = chat_id(&db.pool, household.owner_id).await;
    let scheduler = scheduler(&db.pool);

    let night = scheduler
        .tick(local_utc(moscow, day, 23, 30))
        .await
        .expect("night tick");
    assert_eq!(night.planned, 1);
    assert_eq!(night.delivered, 0);
    assert!(messages(&db.pool, chat).await.is_empty());
    assert_eq!(
        scheduled_at(&db.pool, item.id, household.owner_id).await,
        Some(local_utc(moscow, day + Duration::days(1), 8, 0))
    );

    assert_eq!(
        scheduler
            .tick(local_utc(moscow, day + Duration::days(1), 7, 59))
            .await
            .expect("before end")
            .delivered,
        0
    );
    assert!(messages(&db.pool, chat).await.is_empty());

    assert_eq!(
        scheduler
            .tick(local_utc(moscow, day + Duration::days(1), 8, 0))
            .await
            .expect("morning tick")
            .delivered,
        1
    );
    assert_eq!(
        scheduler
            .tick(local_utc(moscow, day + Duration::days(1), 8, 0))
            .await
            .expect("repeat tick")
            .delivered,
        0
    );
    assert_eq!(messages(&db.pool, chat).await.len(), 1);
    assert_eq!(
        delivery_status(&db.pool, item.id, household.owner_id)
            .await
            .as_deref(),
        Some("sent")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disabling_reminders_cancels_a_deferred_reminder() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = digest_day();
    enable_quiet(&db.pool, household.owner_id, "22:00", "08:00").await;
    let item = create_task(
        &db,
        &household,
        "Ночное напоминание",
        Some(local_utc(moscow, day, 23, 45)),
        Some(household.owner_id),
        None,
    )
    .await;
    let chat = chat_id(&db.pool, household.owner_id).await;
    let scheduler = scheduler(&db.pool);

    let night = scheduler
        .tick(local_utc(moscow, day, 23, 30))
        .await
        .expect("night tick");
    assert_eq!(night.planned, 1);
    assert_eq!(night.delivered, 0);
    assert_eq!(
        scheduled_at(&db.pool, item.id, household.owner_id).await,
        Some(local_utc(moscow, day + Duration::days(1), 8, 0))
    );

    // Пользователь выключил напоминания уже после того, как тихие часы
    // перенесли доставку на утро.
    set_notifications_without_digest(
        &db.pool,
        household.owner_id,
        json!({"reminders_enabled": false}),
    )
    .await;
    let cancelled = scheduler
        .tick(local_utc(moscow, day, 23, 30))
        .await
        .expect("disable tick");
    assert_eq!(cancelled.cancelled, 1);
    assert_eq!(cancelled.delivered, 0);
    assert_eq!(
        delivery_status(&db.pool, item.id, household.owner_id)
            .await
            .as_deref(),
        Some("cancelled")
    );
    assert!(messages(&db.pool, chat).await.is_empty());

    assert_eq!(
        scheduler
            .tick(local_utc(moscow, day + Duration::days(1), 8, 0))
            .await
            .expect("morning tick")
            .delivered,
        0
    );
    assert!(messages(&db.pool, chat).await.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quiet_hours_boundaries_are_start_inclusive_and_end_exclusive() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = digest_day();
    enable_quiet(&db.pool, household.owner_id, "22:00", "08:00").await;

    let before = create_task(
        &db,
        &household,
        "До тихих часов",
        Some(local_utc(moscow, day, 22, 14)),
        Some(household.owner_id),
        None,
    )
    .await;
    let exactly_start = create_task(
        &db,
        &household,
        "Ровно начало",
        Some(local_utc(moscow, day, 22, 15)),
        Some(household.owner_id),
        None,
    )
    .await;
    let exactly_end = create_task(
        &db,
        &household,
        "Ровно конец",
        Some(local_utc(moscow, day + Duration::days(1), 8, 15)),
        Some(household.owner_id),
        None,
    )
    .await;
    let chat = chat_id(&db.pool, household.owner_id).await;
    let scheduler = scheduler(&db.pool);

    let early = scheduler
        .tick(local_utc(moscow, day, 21, 59))
        .await
        .expect("21:59 tick");
    assert_eq!(early.delivered, 1);
    assert_eq!(messages(&db.pool, chat).await.len(), 1);
    assert_eq!(
        delivery_status(&db.pool, before.id, household.owner_id)
            .await
            .as_deref(),
        Some("sent")
    );

    let start = scheduler
        .tick(local_utc(moscow, day, 22, 0))
        .await
        .expect("22:00 tick");
    assert_eq!(start.delivered, 0);
    assert_eq!(messages(&db.pool, chat).await.len(), 1);
    assert_eq!(
        delivery_status(&db.pool, exactly_start.id, household.owner_id)
            .await
            .as_deref(),
        Some("pending")
    );

    let end = scheduler
        .tick(local_utc(moscow, day + Duration::days(1), 8, 0))
        .await
        .expect("08:00 tick");
    assert_eq!(end.delivered, 2);
    let sent = messages(&db.pool, chat).await;
    assert_eq!(sent.len(), 3, "{sent:?}");
    assert_eq!(
        delivery_status(&db.pool, exactly_end.id, household.owner_id)
            .await
            .as_deref(),
        Some("sent")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quiet_hours_work_for_a_user_interval_that_does_not_cross_midnight() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = digest_day();
    enable_quiet(&db.pool, household.owner_id, "13:00", "15:00").await;
    let item = create_task(
        &db,
        &household,
        "Дневные тихие часы",
        Some(local_utc(moscow, day, 13, 15)),
        Some(household.owner_id),
        None,
    )
    .await;
    let chat = chat_id(&db.pool, household.owner_id).await;
    let scheduler = scheduler(&db.pool);

    assert_eq!(
        scheduler
            .tick(local_utc(moscow, day, 13, 0))
            .await
            .expect("13:00 tick")
            .delivered,
        0
    );
    assert!(messages(&db.pool, chat).await.is_empty());
    assert_eq!(
        scheduled_at(&db.pool, item.id, household.owner_id).await,
        Some(local_utc(moscow, day, 15, 0))
    );

    assert_eq!(
        scheduler
            .tick(local_utc(moscow, day, 14, 59))
            .await
            .expect("14:59 tick")
            .delivered,
        0
    );
    assert_eq!(
        scheduler
            .tick(local_utc(moscow, day, 15, 0))
            .await
            .expect("15:00 tick")
            .delivered,
        1
    );
    assert_eq!(messages(&db.pool, chat).await.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quiet_hours_use_the_recipient_timezone_not_the_household() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let vladivostok = chrono_tz::Asia::Vladivostok;
    let day = digest_day();
    set_household_timezone(&db.pool, household.household_id, "Europe/Moscow").await;
    set_user_timezone(&db.pool, household.owner_id, "Asia/Vladivostok").await;
    enable_quiet(&db.pool, household.owner_id, "22:00", "08:00").await;
    let item = create_task(
        &db,
        &household,
        "Пояс получателя",
        Some(local_utc(vladivostok, day, 23, 45)),
        Some(household.owner_id),
        None,
    )
    .await;
    let chat = chat_id(&db.pool, household.owner_id).await;
    let scheduler = scheduler(&db.pool);

    assert_eq!(
        scheduler
            .tick(local_utc(vladivostok, day, 23, 30))
            .await
            .expect("night tick")
            .delivered,
        0
    );
    assert!(messages(&db.pool, chat).await.is_empty());
    assert_eq!(
        scheduled_at(&db.pool, item.id, household.owner_id).await,
        Some(local_utc(vladivostok, day + Duration::days(1), 8, 0))
    );

    assert_eq!(
        scheduler
            .tick(local_utc(vladivostok, day + Duration::days(1), 7, 59))
            .await
            .expect("before end")
            .delivered,
        0
    );
    assert_eq!(
        scheduler
            .tick(local_utc(vladivostok, day + Duration::days(1), 8, 0))
            .await
            .expect("morning tick")
            .delivered,
        1
    );
    assert_eq!(messages(&db.pool, chat).await.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disabled_quiet_hours_do_not_release_a_deferred_reminder_early() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = digest_day();
    let first = create_task(
        &db,
        &household,
        "Без тихих часов",
        Some(local_utc(moscow, day, 23, 45)),
        Some(household.owner_id),
        None,
    )
    .await;
    let chat = chat_id(&db.pool, household.owner_id).await;
    let scheduler = scheduler(&db.pool);

    assert_eq!(
        scheduler
            .tick(local_utc(moscow, day, 23, 30))
            .await
            .expect("on time")
            .delivered,
        1
    );
    assert_eq!(
        delivery_status(&db.pool, first.id, household.owner_id)
            .await
            .as_deref(),
        Some("sent")
    );

    let second = create_task(
        &db,
        &household,
        "Отложено до утра",
        Some(local_utc(moscow, day + Duration::days(1), 23, 45)),
        Some(household.owner_id),
        None,
    )
    .await;
    enable_quiet(&db.pool, household.owner_id, "22:00", "08:00").await;
    assert_eq!(
        scheduler
            .tick(local_utc(moscow, day + Duration::days(1), 23, 30))
            .await
            .expect("deferring tick")
            .delivered,
        0
    );
    assert_eq!(
        scheduled_at(&db.pool, second.id, household.owner_id).await,
        Some(local_utc(moscow, day + Duration::days(2), 8, 0))
    );

    // Отключение тихих часов не ускоряет уже отложенную доставку.
    set_notifications_without_digest(
        &db.pool,
        household.owner_id,
        json!({"quiet_hours_enabled": false}),
    )
    .await;
    assert_eq!(
        scheduler
            .tick(local_utc(moscow, day + Duration::days(1), 23, 31))
            .await
            .expect("disabled tick")
            .delivered,
        0
    );
    assert_eq!(messages(&db.pool, chat).await.len(), 1);
    assert_eq!(
        delivery_status(&db.pool, second.id, household.owner_id)
            .await
            .as_deref(),
        Some("pending")
    );

    assert_eq!(
        scheduler
            .tick(local_utc(moscow, day + Duration::days(2), 8, 0))
            .await
            .expect("fixed release tick")
            .delivered,
        1
    );
    assert_eq!(messages(&db.pool, chat).await.len(), 2);
    assert_eq!(
        delivery_status(&db.pool, second.id, household.owner_id)
            .await
            .as_deref(),
        Some("sent")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn digest_at_seven_is_deferred_to_eight_and_sent_once() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = digest_day();
    set_notifications(
        &db.pool,
        household.owner_id,
        json!({
            "digest_enabled": true,
            "digest_time": "07:00",
            "quiet_hours_enabled": true,
            "quiet_start": "22:00",
            "quiet_end": "08:00"
        }),
    )
    .await;
    let chat = chat_id(&db.pool, household.owner_id).await;
    let scheduler = scheduler(&db.pool);

    assert_eq!(
        scheduler
            .tick(local_utc(moscow, day, 7, 0))
            .await
            .expect("07:00 tick")
            .delivered,
        0
    );
    assert!(messages(&db.pool, chat).await.is_empty());
    assert_eq!(
        scheduler
            .tick(local_utc(moscow, day, 7, 59))
            .await
            .expect("07:59 tick")
            .delivered,
        0
    );
    assert_eq!(
        scheduler
            .tick(local_utc(moscow, day, 8, 0))
            .await
            .expect("08:00 tick")
            .delivered,
        1
    );
    assert_eq!(
        scheduler
            .tick(local_utc(moscow, day, 8, 0))
            .await
            .expect("repeat tick")
            .delivered,
        0
    );

    assert_eq!(messages(&db.pool, chat).await.len(), 1);
    let rows = digest_rows(&db.pool, household.owner_id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, format!("digest:{day}"));
    assert_eq!(rows[0].1, "sent");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn digest_deferred_across_midnight_sends_once_next_morning_and_is_not_lost() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = digest_day();
    let next_day = day + Duration::days(1);
    set_notifications(
        &db.pool,
        household.owner_id,
        json!({
            "digest_enabled": true,
            "digest_time": "23:00",
            "quiet_hours_enabled": true,
            "quiet_start": "22:00",
            "quiet_end": "08:00"
        }),
    )
    .await;
    let chat = chat_id(&db.pool, household.owner_id).await;
    let scheduler = scheduler(&db.pool);

    assert_eq!(
        scheduler
            .tick(local_utc(moscow, day, 23, 0))
            .await
            .expect("night digest tick")
            .delivered,
        0
    );
    assert!(messages(&db.pool, chat).await.is_empty());

    // Полночь не отменяет отложенную сводку: она всё ещё ждёт 08:00.
    let midnight = scheduler
        .tick(local_utc(moscow, next_day, 0, 0))
        .await
        .expect("midnight tick");
    assert_eq!(midnight.cancelled, 0);
    assert_eq!(midnight.delivered, 0);
    assert!(messages(&db.pool, chat).await.is_empty());

    assert_eq!(
        scheduler
            .tick(local_utc(moscow, next_day, 8, 0))
            .await
            .expect("morning tick")
            .delivered,
        1
    );
    assert_eq!(messages(&db.pool, chat).await.len(), 1);

    // Вечером следующего дня планируется уже новая сводка, и утром она
    // приходит одна — двух сводок за одно утро нет.
    assert_eq!(
        scheduler
            .tick(local_utc(moscow, next_day, 23, 0))
            .await
            .expect("next night tick")
            .delivered,
        0
    );
    assert_eq!(messages(&db.pool, chat).await.len(), 1);
    assert_eq!(
        scheduler
            .tick(local_utc(moscow, next_day + Duration::days(1), 8, 0))
            .await
            .expect("next morning tick")
            .delivered,
        1
    );
    assert_eq!(messages(&db.pool, chat).await.len(), 2);

    let rows = digest_rows(&db.pool, household.owner_id).await;
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert!(rows.iter().all(|row| row.1 == "sent"), "{rows:?}");
    assert_eq!(rows[0].0, format!("digest:{day}"));
    assert_eq!(rows[1].0, format!("digest:{next_day}"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quiet_hours_defer_night_assignments_and_collapse_a_to_b_to_a() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let extra = add_member(&db.pool, household.household_id, "Extra member", false).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = digest_day();
    for user_id in [household.member_id, extra] {
        enable_quiet(&db.pool, user_id, "22:00", "08:00").await;
    }
    let item = create_task(&db, &household, "Ночное назначение", None, None, None).await;
    let actor = household.owner();
    let night = local_utc(moscow, day, 23, 30);

    let first = update_assignee(
        &db,
        &household,
        &actor,
        &item,
        Some(household.member_id),
        night,
    )
    .await;
    let second = update_assignee(
        &db,
        &household,
        &actor,
        &first,
        Some(extra),
        night + Duration::seconds(1),
    )
    .await;
    update_assignee(
        &db,
        &household,
        &actor,
        &second,
        Some(household.member_id),
        night + Duration::seconds(2),
    )
    .await;

    let member_chat = chat_id(&db.pool, household.member_id).await;
    let extra_chat = chat_id(&db.pool, extra).await;
    let scheduler = scheduler(&db.pool);

    assert_eq!(
        scheduler
            .tick(night + Duration::seconds(3))
            .await
            .expect("night tick")
            .delivered,
        0
    );
    assert!(messages(&db.pool, member_chat).await.is_empty());
    assert!(messages(&db.pool, extra_chat).await.is_empty());

    assert_eq!(
        scheduler
            .tick(local_utc(moscow, day + Duration::days(1), 7, 59))
            .await
            .expect("before end")
            .delivered,
        0
    );
    assert_eq!(
        scheduler
            .tick(local_utc(moscow, day + Duration::days(1), 8, 0))
            .await
            .expect("morning tick")
            .delivered,
        1
    );

    let member_messages = messages(&db.pool, member_chat).await;
    assert_eq!(member_messages.len(), 1, "{member_messages:?}");
    assert!(messages(&db.pool, extra_chat).await.is_empty());
    let member_rows = assignment_rows(&db.pool, item.id, household.member_id).await;
    assert_eq!(member_rows.len(), 2, "{member_rows:?}");
    assert_eq!(member_rows[0].0, "cancelled");
    assert_eq!(member_rows[1].0, "sent");
    let extra_rows = assignment_rows(&db.pool, item.id, extra).await;
    assert_eq!(extra_rows.len(), 1, "{extra_rows:?}");
    assert_eq!(extra_rows[0].0, "cancelled");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn urgent_tasks_do_not_bypass_quiet_hours() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = digest_day();
    enable_quiet(&db.pool, household.owner_id, "22:00", "08:00").await;
    let item = create_task_with_priority(
        &db,
        &household,
        "Срочная ночью",
        local_utc(moscow, day, 23, 45),
        ItemPriority::Urgent,
    )
    .await;
    let chat = chat_id(&db.pool, household.owner_id).await;
    let scheduler = scheduler(&db.pool);

    assert_eq!(
        scheduler
            .tick(local_utc(moscow, day, 23, 30))
            .await
            .expect("night tick")
            .delivered,
        0
    );
    assert!(messages(&db.pool, chat).await.is_empty());
    assert_eq!(
        scheduler
            .tick(local_utc(moscow, day + Duration::days(1), 8, 0))
            .await
            .expect("morning tick")
            .delivered,
        1
    );
    assert_eq!(messages(&db.pool, chat).await.len(), 1);
    assert_eq!(
        delivery_status(&db.pool, item.id, household.owner_id)
            .await
            .as_deref(),
        Some("sent")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pending_reminder_is_recalculated_after_a_lead_time_change() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = digest_day();
    let due = local_utc(moscow, day, 12, 0);
    let item = create_task(
        &db,
        &household,
        "Пересчитать напоминание",
        Some(due),
        Some(household.owner_id),
        None,
    )
    .await;
    // Будущая pending-запись, какие остаются после отложенных тихих часов.
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

    set_notifications_without_digest(&db.pool, household.owner_id, json!({"lead_minutes": 60}))
        .await;

    assert_eq!(
        scheduler
            .tick(local_utc(moscow, day, 10, 0))
            .await
            .expect("recalculate tick")
            .delivered,
        0
    );
    assert_eq!(
        scheduled_at(&db.pool, item.id, household.owner_id).await,
        Some(due - Duration::minutes(60))
    );
    assert_eq!(
        scheduler
            .tick(local_utc(moscow, day, 10, 59))
            .await
            .expect("before new lead")
            .delivered,
        0
    );
    assert_eq!(
        scheduler
            .tick(local_utc(moscow, day, 11, 0))
            .await
            .expect("new lead tick")
            .delivered,
        1
    );
    assert_eq!(messages(&db.pool, chat).await.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn all_day_reminder_deferred_across_midnight_is_not_cancelled() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = digest_day();
    set_notifications_without_digest(
        &db.pool,
        household.owner_id,
        json!({
            "all_day_time": "23:00",
            "quiet_hours_enabled": true,
            "quiet_start": "22:00",
            "quiet_end": "08:00"
        }),
    )
    .await;
    let item = create_all_day_task(
        &db,
        &household,
        "Позднее вседневное",
        day,
        Some(household.owner_id),
    )
    .await;
    let chat = chat_id(&db.pool, household.owner_id).await;
    let scheduler = scheduler(&db.pool);

    assert_eq!(
        scheduler
            .tick(local_utc(moscow, day, 23, 0))
            .await
            .expect("night tick")
            .delivered,
        0
    );
    assert!(messages(&db.pool, chat).await.is_empty());

    let midnight = scheduler
        .tick(local_utc(moscow, day + Duration::days(1), 0, 0))
        .await
        .expect("midnight tick");
    assert_eq!(midnight.cancelled, 0);
    assert_eq!(midnight.delivered, 0);
    assert_eq!(
        delivery_status(&db.pool, item.id, household.owner_id)
            .await
            .as_deref(),
        Some("pending")
    );

    assert_eq!(
        scheduler
            .tick(local_utc(moscow, day + Duration::days(1), 8, 0))
            .await
            .expect("morning tick")
            .delivered,
        1
    );
    assert_eq!(messages(&db.pool, chat).await.len(), 1);
    assert_eq!(
        delivery_status(&db.pool, item.id, household.owner_id)
            .await
            .as_deref(),
        Some("sent")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn digest_time_change_after_midnight_does_not_send_two_digests_in_one_morning() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = digest_day();
    let next_day = day + Duration::days(1);
    set_notifications(
        &db.pool,
        household.owner_id,
        json!({
            "digest_enabled": true,
            "digest_time": "23:00",
            "quiet_hours_enabled": true,
            "quiet_start": "22:00",
            "quiet_end": "08:00"
        }),
    )
    .await;
    let chat = chat_id(&db.pool, household.owner_id).await;
    let scheduler = scheduler(&db.pool);

    assert_eq!(
        scheduler
            .tick(local_utc(moscow, day, 23, 0))
            .await
            .expect("night tick")
            .delivered,
        0
    );

    // После полуночи меняем время сводки на 06:00: новая сводка тоже попадёт
    // на 08:00, поэтому вчерашняя отменяется, а не приходит второй.
    set_notifications(
        &db.pool,
        household.owner_id,
        json!({
            "digest_enabled": true,
            "digest_time": "06:00",
            "quiet_hours_enabled": true,
            "quiet_start": "22:00",
            "quiet_end": "08:00"
        }),
    )
    .await;
    assert_eq!(
        scheduler
            .tick(local_utc(moscow, next_day, 0, 30))
            .await
            .expect("after midnight tick")
            .delivered,
        0
    );
    let rows = digest_rows(&db.pool, household.owner_id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, format!("digest:{day}"));
    assert_eq!(rows[0].1, "cancelled");

    assert_eq!(
        scheduler
            .tick(local_utc(moscow, next_day, 6, 0))
            .await
            .expect("new digest time tick")
            .delivered,
        0
    );
    assert_eq!(
        scheduler
            .tick(local_utc(moscow, next_day, 8, 0))
            .await
            .expect("morning tick")
            .delivered,
        1
    );
    assert_eq!(messages(&db.pool, chat).await.len(), 1);

    let rows = digest_rows(&db.pool, household.owner_id).await;
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[1].0, format!("digest:{next_day}"));
    assert_eq!(rows[1].1, "sent");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disabled_quiet_hours_do_not_release_a_deferred_digest_early() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = digest_day();
    let next_day = day + Duration::days(1);
    set_notifications(
        &db.pool,
        household.owner_id,
        json!({
            "digest_enabled": true,
            "digest_time": "23:00",
            "quiet_hours_enabled": true,
            "quiet_start": "22:00",
            "quiet_end": "08:00"
        }),
    )
    .await;
    let chat = chat_id(&db.pool, household.owner_id).await;
    let scheduler = scheduler(&db.pool);

    assert_eq!(
        scheduler
            .tick(local_utc(moscow, day, 23, 0))
            .await
            .expect("night tick")
            .delivered,
        0
    );
    assert_eq!(
        digest_rows(&db.pool, household.owner_id).await[0].1,
        "pending"
    );
    set_notifications(
        &db.pool,
        household.owner_id,
        json!({
            "digest_enabled": true,
            "digest_time": "23:00",
            "quiet_hours_enabled": false
        }),
    )
    .await;
    // Выключение тихих часов не ускоряет отложенную сводку: её время
    // зафиксировано на конце тихих часов.
    assert_eq!(
        scheduler
            .tick(local_utc(moscow, next_day, 0, 30))
            .await
            .expect("disabled tick")
            .delivered,
        0
    );
    assert!(messages(&db.pool, chat).await.is_empty());

    assert_eq!(
        scheduler
            .tick(local_utc(moscow, next_day, 8, 0))
            .await
            .expect("fixed release tick")
            .delivered,
        1
    );
    assert_eq!(messages(&db.pool, chat).await.len(), 1);
    let rows = digest_rows(&db.pool, household.owner_id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].1, "sent");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disabled_quiet_hours_do_not_release_a_deferred_assignment_early() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = digest_day();
    enable_quiet(&db.pool, household.member_id, "22:00", "08:00").await;
    let item = create_task(&db, &household, "Ночное назначение", None, None, None).await;
    let night = local_utc(moscow, day, 23, 30);
    update_assignee(
        &db,
        &household,
        &household.owner(),
        &item,
        Some(household.member_id),
        night,
    )
    .await;
    let chat = chat_id(&db.pool, household.member_id).await;
    let scheduler = scheduler(&db.pool);

    assert_eq!(
        scheduler
            .tick(night + Duration::seconds(1))
            .await
            .expect("deferring tick")
            .delivered,
        0
    );
    set_notifications_without_digest(
        &db.pool,
        household.member_id,
        json!({"quiet_hours_enabled": false}),
    )
    .await;
    assert_eq!(
        scheduler
            .tick(night + Duration::seconds(2))
            .await
            .expect("disabled tick")
            .delivered,
        0
    );
    assert!(messages(&db.pool, chat).await.is_empty());

    assert_eq!(
        scheduler
            .tick(local_utc(moscow, day + Duration::days(1), 8, 0))
            .await
            .expect("fixed release tick")
            .delivered,
        1
    );
    assert_eq!(messages(&db.pool, chat).await.len(), 1);
    let rows = assignment_rows(&db.pool, item.id, household.member_id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "sent");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn digest_time_is_independent_between_members() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = digest_day();
    set_notifications(
        &db.pool,
        household.owner_id,
        json!({
            "digest_enabled": true,
            "digest_time": "07:00",
            "quiet_hours_enabled": false
        }),
    )
    .await;
    set_notifications(
        &db.pool,
        household.member_id,
        json!({
            "digest_enabled": true,
            "digest_time": "09:00",
            "quiet_hours_enabled": false
        }),
    )
    .await;
    let owner_chat = chat_id(&db.pool, household.owner_id).await;
    let member_chat = chat_id(&db.pool, household.member_id).await;
    let scheduler = scheduler(&db.pool);

    assert_eq!(
        scheduler
            .tick(local_utc(moscow, day, 7, 0))
            .await
            .expect("owner digest tick")
            .delivered,
        1
    );
    assert_eq!(messages(&db.pool, owner_chat).await.len(), 1);
    assert!(messages(&db.pool, member_chat).await.is_empty());

    assert_eq!(
        scheduler
            .tick(local_utc(moscow, day, 9, 0))
            .await
            .expect("member digest tick")
            .delivered,
        1
    );
    assert_eq!(messages(&db.pool, owner_chat).await.len(), 1);
    assert_eq!(messages(&db.pool, member_chat).await.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reminder_and_digest_escape_a_script_title() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = digest_day();
    set_household_timezone(&db.pool, household.household_id, "Europe/Moscow").await;
    enable_digest(&db.pool, household.owner_id).await;
    let title = "<script>alert(1)</script> & <b>\"x\"</b>";
    create_task(
        &db,
        &household,
        title,
        Some(local_utc(moscow, day, 10, 0)),
        Some(household.owner_id),
        None,
    )
    .await;
    let chat = chat_id(&db.pool, household.owner_id).await;
    let scheduler = scheduler(&db.pool);

    assert_eq!(
        scheduler
            .tick(local_utc(moscow, day, 8, 30))
            .await
            .expect("digest tick")
            .delivered,
        1
    );
    assert_eq!(
        scheduler
            .tick(local_utc(moscow, day, 9, 45))
            .await
            .expect("reminder tick")
            .delivered,
        1
    );

    let sent = messages(&db.pool, chat).await;
    assert_eq!(sent.len(), 2, "{sent:?}");
    for html in &sent {
        assert!(!html.contains("<script>"), "{html}");
        assert!(
            html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"),
            "{html}"
        );
        assert!(html.contains("&amp;"), "{html}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snooze_with_a_changed_origin_rebinds_pending_and_cancelled_deliveries() {
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
    let first_at = rounded_now() + Duration::hours(1);
    let new_at = rounded_now() + Duration::hours(2);

    for (index, status) in ["pending", "cancelled"].into_iter().enumerate() {
        let key = notifications::snooze_key(first_at + Duration::hours(index as i64));
        let inserted = insert_snooze_with_origin(
            &db.pool,
            household.household_id,
            household.owner_id,
            item.id,
            &key,
            Some("date:2026-11-09"),
            first_at,
        )
        .await
        .expect("initial snooze");
        assert_eq!(inserted, 1);

        let seq = insert_outbox(&db.pool, chat, &format!("snooze-origin-{index}")).await;
        sqlx::query(
            "UPDATE notification_deliveries
             SET status = $2::notification_status,
                 deferred = true, batched = true, outbox_seq = $3
             WHERE item_id = $1 AND occurrence_key = $4",
        )
        .bind(item.id)
        .bind(status)
        .bind(seq)
        .bind(&key)
        .execute(&db.pool)
        .await
        .expect("prepare delivery");

        let rebound = insert_snooze_with_origin(
            &db.pool,
            household.household_id,
            household.owner_id,
            item.id,
            &key,
            Some("date:2026-11-10"),
            new_at,
        )
        .await
        .expect("re-snooze with a new origin");
        assert_eq!(rebound, 1, "{status} delivery must be rebound");

        let (status, origin, scheduled, deferred, batched, outbox_seq) =
            snooze_delivery(&db.pool, item.id, &key).await;
        assert_eq!(status, "pending");
        assert_eq!(origin.as_deref(), Some("date:2026-11-10"));
        assert_eq!(scheduled, new_at);
        assert!(!deferred, "rebinding must reset the quiet-hours flag");
        assert!(!batched, "rebinding must reset the batched flag");
        assert_eq!(outbox_seq, None, "rebinding must reset the outbox link");

        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM notification_deliveries
             WHERE item_id = $1 AND occurrence_key = $2 AND kind = 'snooze'",
        )
        .bind(item.id)
        .bind(&key)
        .fetch_one(&db.pool)
        .await
        .expect("snooze count");
        assert_eq!(count, 1, "rebinding must update the same row");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snooze_with_the_same_origin_changes_nothing() {
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
    let first_at = rounded_now() + Duration::hours(1);
    let key = notifications::snooze_key(first_at);
    let origin = "date:2026-11-09";

    assert_eq!(
        insert_snooze_with_origin(
            &db.pool,
            household.household_id,
            household.owner_id,
            item.id,
            &key,
            Some(origin),
            first_at,
        )
        .await
        .expect("initial snooze"),
        1
    );

    assert_eq!(
        insert_snooze_with_origin(
            &db.pool,
            household.household_id,
            household.owner_id,
            item.id,
            &key,
            Some(origin),
            first_at + Duration::hours(1),
        )
        .await
        .expect("same snooze"),
        0,
        "the same pending delivery must not be touched"
    );

    let (status, stored_origin, scheduled, _, _, _) =
        snooze_delivery(&db.pool, item.id, &key).await;
    assert_eq!(status, "pending");
    assert_eq!(stored_origin.as_deref(), Some(origin));
    assert_eq!(scheduled, first_at);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snooze_with_a_changed_origin_does_not_touch_a_sent_delivery() {
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
    let first_at = rounded_now() + Duration::hours(1);
    let key = notifications::snooze_key(first_at);
    let origin = "date:2026-11-09";

    assert_eq!(
        insert_snooze_with_origin(
            &db.pool,
            household.household_id,
            household.owner_id,
            item.id,
            &key,
            Some(origin),
            first_at,
        )
        .await
        .expect("initial snooze"),
        1
    );
    sqlx::query(
        "UPDATE notification_deliveries
         SET status = 'sent' WHERE item_id = $1 AND occurrence_key = $2",
    )
    .bind(item.id)
    .bind(&key)
    .execute(&db.pool)
    .await
    .expect("mark sent");

    assert_eq!(
        insert_snooze_with_origin(
            &db.pool,
            household.household_id,
            household.owner_id,
            item.id,
            &key,
            Some("date:2026-11-10"),
            first_at + Duration::hours(1),
        )
        .await
        .expect("re-snooze after delivery"),
        0,
        "a delivered snooze must not be rebound"
    );

    let (status, stored_origin, scheduled, _, _, _) =
        snooze_delivery(&db.pool, item.id, &key).await;
    assert_eq!(status, "sent");
    assert_eq!(stored_origin.as_deref(), Some(origin));
    assert_eq!(scheduled, first_at);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snooze_with_an_origin_rebinds_a_legacy_pending_delivery() {
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
    let first_at = rounded_now() + Duration::hours(1);
    let key = notifications::snooze_key(first_at);

    // Запись до миграции: привязки ещё нет.
    assert_eq!(
        insert_snooze_with_origin(
            &db.pool,
            household.household_id,
            household.owner_id,
            item.id,
            &key,
            None,
            first_at,
        )
        .await
        .expect("legacy snooze"),
        1
    );

    let new_at = first_at + Duration::hours(1);
    let origin = "date:2026-11-10";
    assert_eq!(
        insert_snooze_with_origin(
            &db.pool,
            household.household_id,
            household.owner_id,
            item.id,
            &key,
            Some(origin),
            new_at,
        )
        .await
        .expect("snooze after migration"),
        1,
        "a legacy pending delivery must be rebound to the new origin"
    );

    let (status, stored_origin, scheduled, deferred, batched, outbox_seq) =
        snooze_delivery(&db.pool, item.id, &key).await;
    assert_eq!(status, "pending");
    assert_eq!(stored_origin.as_deref(), Some(origin));
    assert_eq!(scheduled, new_at);
    assert!(!deferred);
    assert!(!batched);
    assert_eq!(outbox_seq, None);

    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM notification_deliveries
         WHERE item_id = $1 AND occurrence_key = $2 AND kind = 'snooze'",
    )
    .bind(item.id)
    .bind(&key)
    .fetch_one(&db.pool)
    .await
    .expect("snooze count");
    assert_eq!(count, 1, "rebinding must update the same row");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snooze_without_an_origin_rebinds_a_delivery_with_an_origin() {
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
    let first_at = rounded_now() + Duration::hours(1);
    let key = notifications::snooze_key(first_at);
    let origin = "date:2026-11-09";

    assert_eq!(
        insert_snooze_with_origin(
            &db.pool,
            household.household_id,
            household.owner_id,
            item.id,
            &key,
            Some(origin),
            first_at,
        )
        .await
        .expect("initial snooze"),
        1
    );

    let new_at = first_at + Duration::hours(1);
    assert_eq!(
        insert_snooze_with_origin(
            &db.pool,
            household.household_id,
            household.owner_id,
            item.id,
            &key,
            None,
            new_at,
        )
        .await
        .expect("re-snooze without an origin"),
        1,
        "the origin must be cleared when the new delivery has none"
    );

    let (status, stored_origin, scheduled, _, _, _) =
        snooze_delivery(&db.pool, item.id, &key).await;
    assert_eq!(status, "pending");
    assert_eq!(stored_origin, None);
    assert_eq!(scheduled, new_at);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delivered_snooze_replaces_the_message_it_was_created_from() {
    let db = support::isolated_db().await;
    let (mut bot, _household, item, message_id) = delivered_reminder(&db, "Позвонить маме").await;
    let snooze_at = snooze_reminder(&mut bot, &db.pool, item.id).await;

    // Тик доставляет snooze в outbox, но воркер ещё не прошёл.
    let stats = scheduler(&db.pool)
        .tick(snooze_at)
        .await
        .expect("snooze tick");
    assert_eq!(stats.delivered, 1, "{stats:?}");
    let payload: serde_json::Value = sqlx::query_scalar(
        "SELECT payload FROM outbox
         WHERE chat_id = $1 AND kind = 'message' AND dedupe_key LIKE 'notification:snooze:%'",
    )
    .bind(bot.chat)
    .fetch_one(&db.pool)
    .await
    .expect("snooze payload");
    assert_eq!(payload["replaces"], json!([message_id]), "{payload}");
    // До прохода воркера удаления прежнего сообщения нет.
    assert!(replace_delete_rows(&db.pool, bot.chat).await.is_empty());
    assert!(!deleted(&bot, message_id));
    // Доставка исходного напоминания уже отмечена убранным.
    let dismissed: Option<DateTime<Utc>> = sqlx::query_scalar(
        "SELECT dismissed_at FROM notification_deliveries
         WHERE item_id = $1 AND kind = 'reminder'",
    )
    .bind(item.id)
    .fetch_one(&db.pool)
    .await
    .expect("reminder dismissed_at");
    assert!(dismissed.is_some(), "{dismissed:?}");

    // Первый проход воркера доставляет новое напоминание и ставит удаление.
    if snooze_at > bot.now {
        bot.advance(snooze_at - bot.now);
    }
    let mut worker = OutboxWorker::new(db.pool.clone(), bot.fake.clone()).only_chat(bot.chat);
    let mut clock = Instant::now();
    clock += StdDuration::from_secs(2);
    assert_eq!(
        worker.run_once(bot.now, clock).await.expect("first pass"),
        1
    );
    let deletes = replace_delete_rows(&db.pool, bot.chat).await;
    assert_eq!(deletes.len(), 1, "{deletes:?}");
    assert_eq!(deletes[0].0, "pending", "{deletes:?}");
    assert_eq!(
        deletes[0].1,
        format!("replace:{}:{message_id}", bot.chat),
        "{deletes:?}"
    );
    assert_eq!(deletes[0].2["message_id"], json!(message_id), "{deletes:?}");
    assert_eq!(deletes[0].2["cleanup"], json!(true), "{deletes:?}");
    assert!(
        !deleted(&bot, message_id),
        "the delete must not run before its own pass"
    );

    // Следующий проход выполняет удаление: в чате одно сообщение по задаче.
    clock += StdDuration::from_secs(2);
    worker.run_once(bot.now, clock).await.expect("second pass");
    assert!(deleted(&bot, message_id));
    let alive: Vec<(i64, String)> = bot
        .fake
        .messages(bot.chat)
        .into_iter()
        .filter(|(_, html)| html.contains("Позвонить маме"))
        .collect();
    assert_eq!(alive.len(), 1, "{alive:?}");
    assert_ne!(alive[0].0, message_id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_a_snooze_after_due_change_deletes_its_message_immediately() {
    let db = support::isolated_db().await;
    let (mut bot, _household, item, message_id) =
        delivered_reminder(&db, "Перенести встречу").await;
    let snooze_at = snooze_reminder(&mut bot, &db.pool, item.id).await;
    let new_due = item.due_at.expect("timed task") + Duration::hours(2);
    sqlx::query("UPDATE items SET due_at = $2, version = version + 1 WHERE id = $1")
        .bind(item.id)
        .bind(new_due)
        .execute(&db.pool)
        .await
        .expect("move due");

    let stats = scheduler(&db.pool)
        .tick(snooze_at)
        .await
        .expect("cancel tick");
    assert_eq!(stats.cancelled, 1, "{stats:?}");
    assert_eq!(stats.delivered, 0, "{stats:?}");
    // Удаление поставлено сразу, до прохода воркера.
    let deletes = replace_delete_rows(&db.pool, bot.chat).await;
    assert_eq!(deletes.len(), 1, "{deletes:?}");
    assert_eq!(deletes[0].0, "pending", "{deletes:?}");
    assert_eq!(
        deletes[0].1,
        format!("replace:{}:{message_id}", bot.chat),
        "{deletes:?}"
    );
    assert_eq!(deletes[0].2["message_id"], json!(message_id), "{deletes:?}");
    assert_eq!(deletes[0].2["cleanup"], json!(true), "{deletes:?}");
    if snooze_at > bot.now {
        bot.advance(snooze_at - bot.now);
    }
    bot.deliver().await;
    assert!(deleted(&bot, message_id));

    // Напоминание о новом сроке приходит по расписанию.
    let stats = tick_and_deliver(&mut bot, &db.pool, new_due - Duration::minutes(15)).await;
    assert_eq!(stats.delivered, 1, "{stats:?}");
    assert!(
        bot.latest().1.contains("Перенести встречу"),
        "{}",
        bot.latest().1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_a_snooze_after_completion_deletes_its_message() {
    let db = support::isolated_db().await;
    let (mut bot, household, item, message_id) = delivered_reminder(&db, "Купить билеты").await;
    let snooze_at = snooze_reminder(&mut bot, &db.pool, item.id).await;

    let mut conn = db.pool.acquire().await.expect("conn");
    items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        item.id,
        None,
        "Europe/Moscow",
        bot.now,
    )
    .await
    .expect("complete task");

    let stats = scheduler(&db.pool)
        .tick(snooze_at)
        .await
        .expect("cancel tick");
    assert_eq!(stats.cancelled, 1, "{stats:?}");
    assert_eq!(stats.delivered, 0, "{stats:?}");
    let deletes = replace_delete_rows(&db.pool, bot.chat).await;
    assert_eq!(deletes.len(), 1, "{deletes:?}");
    assert_eq!(
        deletes[0].1,
        format!("replace:{}:{message_id}", bot.chat),
        "{deletes:?}"
    );
    if snooze_at > bot.now {
        bot.advance(snooze_at - bot.now);
    }
    bot.deliver().await;
    assert!(deleted(&bot, message_id));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_snooze_without_a_recipient_chat_queues_no_delete() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let due = rounded_now() + Duration::hours(3);
    let item = create_task(
        &db,
        &household,
        "Полить цветы",
        Some(due),
        Some(household.owner_id),
        None,
    )
    .await;
    let chat = chat_id(&db.pool, household.owner_id).await;
    let at = due - Duration::minutes(15);
    let message_id = delivered_reminder_row(&db, &household, &item, at).await;
    let snooze_at = at + Duration::hours(1);
    assert_eq!(
        insert_snooze_replacing(
            &db.pool,
            household.household_id,
            household.owner_id,
            item.id,
            &notifications::snooze_key(snooze_at),
            Some(&snooze_origin(item.due_at, None)),
            snooze_at,
            Some(message_id),
        )
        .await
        .expect("snooze"),
        1
    );
    // Получатель выбыл: `is_stale` отменяет доставку, чата нет — удалять нечего.
    sqlx::query("UPDATE users SET deleted_at = now() WHERE id = $1")
        .bind(household.owner_id)
        .execute(&db.pool)
        .await
        .expect("delete user");

    let stats = scheduler(&db.pool)
        .tick(snooze_at)
        .await
        .expect("cancel tick");
    assert_eq!(stats.cancelled, 1, "{stats:?}");
    assert_eq!(stats.delivered, 0, "{stats:?}");
    assert!(replace_delete_rows(&db.pool, chat).await.is_empty());
    let dismissed: Option<DateTime<Utc>> = sqlx::query_scalar(
        "SELECT dismissed_at FROM notification_deliveries
         WHERE item_id = $1 AND kind = 'reminder'",
    )
    .bind(item.id)
    .fetch_one(&db.pool)
    .await
    .expect("reminder dismissed_at");
    assert!(dismissed.is_none(), "{dismissed:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snooze_without_replaces_delivers_without_deleting_anything() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let due = rounded_now() + Duration::hours(3);
    let item = create_task(
        &db,
        &household,
        "Полить цветы",
        Some(due),
        Some(household.owner_id),
        None,
    )
    .await;
    let chat = chat_id(&db.pool, household.owner_id).await;
    let at = due - Duration::minutes(15);
    delivered_reminder_row(&db, &household, &item, at).await;
    // Snooze из сводного сообщения: поля замены нет.
    let snooze_at = at + Duration::hours(1);
    assert_eq!(
        insert_snooze_replacing(
            &db.pool,
            household.household_id,
            household.owner_id,
            item.id,
            &notifications::snooze_key(snooze_at),
            Some(&snooze_origin(item.due_at, None)),
            snooze_at,
            None,
        )
        .await
        .expect("snooze"),
        1
    );

    let stats = scheduler(&db.pool)
        .tick(snooze_at)
        .await
        .expect("snooze tick");
    assert_eq!(stats.delivered, 1, "{stats:?}");
    let payload: serde_json::Value = sqlx::query_scalar(
        "SELECT payload FROM outbox
         WHERE chat_id = $1 AND kind = 'message' AND dedupe_key LIKE 'notification:snooze:%'",
    )
    .bind(chat)
    .fetch_one(&db.pool)
    .await
    .expect("snooze payload");
    assert!(payload.get("replaces").is_none(), "{payload}");
    assert!(replace_delete_rows(&db.pool, chat).await.is_empty());
    let dismissed: Option<DateTime<Utc>> = sqlx::query_scalar(
        "SELECT dismissed_at FROM notification_deliveries
         WHERE item_id = $1 AND kind = 'reminder'",
    )
    .bind(item.id)
    .fetch_one(&db.pool)
    .await
    .expect("reminder dismissed_at");
    assert!(dismissed.is_none(), "{dismissed:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_snooze_without_replaces_queues_no_delete() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let due = rounded_now() + Duration::hours(3);
    let item = create_task(
        &db,
        &household,
        "Полить цветы",
        Some(due),
        Some(household.owner_id),
        None,
    )
    .await;
    let chat = chat_id(&db.pool, household.owner_id).await;
    let at = due - Duration::minutes(15);
    delivered_reminder_row(&db, &household, &item, at).await;
    let snooze_at = at + Duration::hours(1);
    assert_eq!(
        insert_snooze_replacing(
            &db.pool,
            household.household_id,
            household.owner_id,
            item.id,
            &notifications::snooze_key(snooze_at),
            Some(&snooze_origin(item.due_at, None)),
            snooze_at,
            None,
        )
        .await
        .expect("snooze"),
        1
    );
    sqlx::query("UPDATE items SET due_at = $2, version = version + 1 WHERE id = $1")
        .bind(item.id)
        .bind(due + Duration::hours(2))
        .execute(&db.pool)
        .await
        .expect("move due");

    let stats = scheduler(&db.pool)
        .tick(snooze_at)
        .await
        .expect("cancel tick");
    assert_eq!(stats.cancelled, 1, "{stats:?}");
    assert_eq!(stats.delivered, 0, "{stats:?}");
    assert!(replace_delete_rows(&db.pool, chat).await.is_empty());
}

/// Настройки получателя: сводка включена на 08:30, тихие часы выключены.
async fn digest_on(pool: &PgPool, user_id: Uuid) {
    set_notifications(
        pool,
        user_id,
        json!({"digest_enabled": true, "digest_time": "08:30", "quiet_hours_enabled": false}),
    )
    .await;
}

/// Тик планировщика и доставка outbox в заданный момент; возвращает id
/// доставленного сообщения сводки.
async fn deliver_digest_at(bot: &mut Harness, pool: &PgPool, at: DateTime<Utc>) -> i64 {
    let stats = scheduler(pool).tick(at).await.expect("digest tick");
    assert_eq!(stats.delivered, 1, "{stats:?}");
    if at > bot.now {
        bot.advance(at - bot.now);
    }
    bot.deliver().await;
    let (message_id, html) = bot.latest();
    assert!(html.contains("Доброе утро"), "{html}");
    message_id
}

/// Payload последней сводки чата в outbox.
async fn latest_digest_payload(pool: &PgPool, chat: i64) -> serde_json::Value {
    sqlx::query_scalar(
        "SELECT payload FROM outbox
         WHERE chat_id = $1 AND kind = 'message' AND dedupe_key LIKE 'notification:digest:%'
         ORDER BY seq DESC LIMIT 1",
    )
    .bind(chat)
    .fetch_one(pool)
    .await
    .expect("digest payload")
}

/// `dismissed_at` сводки получателя по ключу вхождения.
async fn digest_dismissed_at(pool: &PgPool, user_id: Uuid, key: &str) -> Option<DateTime<Utc>> {
    sqlx::query_scalar(
        "SELECT dismissed_at FROM notification_deliveries
         WHERE user_id = $1 AND kind = 'digest' AND occurrence_key = $2",
    )
    .bind(user_id)
    .bind(key)
    .fetch_one(pool)
    .await
    .expect("digest dismissed_at")
}

/// Живые сообщения сводки чата по версии фейка.
fn alive_digests(bot: &Harness) -> Vec<(i64, String)> {
    bot.fake
        .messages(bot.chat)
        .into_iter()
        .filter(|(_, html)| html.contains("Доброе утро"))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn new_digest_replaces_yesterdays_digest_after_delivery() {
    let db = support::isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    let household = household_for(&db.pool, bot.chat).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = local_today(&bot, moscow);
    let yesterday = day - Duration::days(1);
    digest_on(&db.pool, household.owner_id).await;

    let d1_at = local_utc(moscow, yesterday, 8, 30);
    bot.freeze_at(d1_at);
    let d1 = deliver_digest_at(&mut bot, &db.pool, d1_at).await;

    // Новая сводка поставлена в outbox, но воркер ещё не прошёл.
    let d2_at = local_utc(moscow, day, 8, 30);
    let stats = scheduler(&db.pool).tick(d2_at).await.expect("digest tick");
    assert_eq!(stats.delivered, 1, "{stats:?}");
    let payload = latest_digest_payload(&db.pool, bot.chat).await;
    assert_eq!(payload["replaces"], json!([d1]), "{payload}");
    // Пока новая сводка не доставлена, удаления прежней нет.
    assert!(replace_delete_rows(&db.pool, bot.chat).await.is_empty());
    // Отметка уборки ставится в транзакции доставки.
    assert!(
        digest_dismissed_at(&db.pool, household.owner_id, &format!("digest:{yesterday}"))
            .await
            .is_some()
    );

    bot.advance(d2_at - bot.now);
    // Первый проход воркера доставляет новую сводку и ставит удаление.
    let mut worker = OutboxWorker::new(db.pool.clone(), bot.fake.clone()).only_chat(bot.chat);
    let mut clock = Instant::now();
    clock += StdDuration::from_secs(2);
    assert_eq!(
        worker.run_once(bot.now, clock).await.expect("first pass"),
        1
    );
    let deletes = replace_delete_rows(&db.pool, bot.chat).await;
    assert_eq!(deletes.len(), 1, "{deletes:?}");
    assert_eq!(deletes[0].0, "pending", "{deletes:?}");
    assert_eq!(
        deletes[0].1,
        format!("replace:{}:{d1}", bot.chat),
        "{deletes:?}"
    );
    assert_eq!(deletes[0].2["message_id"], json!(d1), "{deletes:?}");
    assert_eq!(deletes[0].2["cleanup"], json!(true), "{deletes:?}");
    assert!(
        !deleted(&bot, d1),
        "the delete must not run before its own pass"
    );

    // Следующий проход выполняет удаление: в чате одна сводка.
    clock += StdDuration::from_secs(2);
    worker.run_once(bot.now, clock).await.expect("second pass");
    assert!(deleted(&bot, d1));
    let alive = alive_digests(&bot);
    assert_eq!(alive.len(), 1, "{alive:?}");
    assert_ne!(alive[0].0, d1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn digest_older_than_the_replacement_window_is_left_alone() {
    let db = support::isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    let household = household_for(&db.pool, bot.chat).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = local_today(&bot, moscow);
    let old_day = day - Duration::days(4);
    digest_on(&db.pool, household.owner_id).await;

    let d1_at = local_utc(moscow, old_day, 8, 30);
    bot.freeze_at(d1_at);
    let d1 = deliver_digest_at(&mut bot, &db.pool, d1_at).await;

    let d2_at = local_utc(moscow, day, 8, 30);
    let stats = scheduler(&db.pool).tick(d2_at).await.expect("digest tick");
    assert_eq!(stats.delivered, 1, "{stats:?}");
    let payload = latest_digest_payload(&db.pool, bot.chat).await;
    assert!(payload.get("replaces").is_none(), "{payload}");
    assert!(
        digest_dismissed_at(&db.pool, household.owner_id, &format!("digest:{old_day}"))
            .await
            .is_none()
    );

    bot.advance(d2_at - bot.now);
    bot.deliver().await;
    // Сводка старше окна замены не удаляется: Telegram уже не даст.
    assert!(replace_delete_rows(&db.pool, bot.chat).await.is_empty());
    assert!(!deleted(&bot, d1));
    assert_eq!(alive_digests(&bot).len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn digest_sent_46_hours_ago_is_still_replaced() {
    let db = support::isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    let household = household_for(&db.pool, bot.chat).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = local_today(&bot, moscow);
    digest_on(&db.pool, household.owner_id).await;

    let d1_at = local_utc(moscow, day, 8, 30);
    bot.freeze_at(d1_at);
    let d1 = deliver_digest_at(&mut bot, &db.pool, d1_at).await;

    // Новая сводка через 46 часов — внутри окна замены в 47 часов.
    set_notifications(
        &db.pool,
        household.owner_id,
        json!({"digest_enabled": true, "digest_time": "06:30", "quiet_hours_enabled": false}),
    )
    .await;
    let d2_at = local_utc(moscow, day + Duration::days(2), 6, 30);
    assert_eq!(d2_at - d1_at, Duration::hours(46));
    let stats = scheduler(&db.pool).tick(d2_at).await.expect("digest tick");
    assert_eq!(stats.delivered, 1, "{stats:?}");
    let payload = latest_digest_payload(&db.pool, bot.chat).await;
    assert_eq!(payload["replaces"], json!([d1]), "{payload}");
    assert!(
        digest_dismissed_at(&db.pool, household.owner_id, &format!("digest:{day}"))
            .await
            .is_some()
    );

    bot.advance(d2_at - bot.now);
    bot.deliver().await;
    let deletes = replace_delete_rows(&db.pool, bot.chat).await;
    assert_eq!(deletes.len(), 1, "{deletes:?}");
    assert_eq!(
        deletes[0].1,
        format!("replace:{}:{d1}", bot.chat),
        "{deletes:?}"
    );
    assert!(deleted(&bot, d1));
    assert_eq!(alive_digests(&bot).len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn digest_sent_48_hours_ago_is_not_replaced() {
    let db = support::isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    let household = household_for(&db.pool, bot.chat).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = local_today(&bot, moscow);
    digest_on(&db.pool, household.owner_id).await;

    let d1_at = local_utc(moscow, day, 8, 30);
    bot.freeze_at(d1_at);
    let d1 = deliver_digest_at(&mut bot, &db.pool, d1_at).await;

    // Новая сводка через 48 часов — за окном замены в 47 часов.
    let d2_at = local_utc(moscow, day + Duration::days(2), 8, 30);
    assert_eq!(d2_at - d1_at, Duration::hours(48));
    let stats = scheduler(&db.pool).tick(d2_at).await.expect("digest tick");
    assert_eq!(stats.delivered, 1, "{stats:?}");
    let payload = latest_digest_payload(&db.pool, bot.chat).await;
    assert!(payload.get("replaces").is_none(), "{payload}");
    assert!(
        digest_dismissed_at(&db.pool, household.owner_id, &format!("digest:{day}"))
            .await
            .is_none()
    );

    bot.advance(d2_at - bot.now);
    bot.deliver().await;
    assert!(replace_delete_rows(&db.pool, bot.chat).await.is_empty());
    assert!(!deleted(&bot, d1));
    assert_eq!(alive_digests(&bot).len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn already_dismissed_digest_is_not_replaced_again() {
    let db = support::isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    let household = household_for(&db.pool, bot.chat).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = local_today(&bot, moscow);
    let yesterday = day - Duration::days(1);
    digest_on(&db.pool, household.owner_id).await;

    let d1_at = local_utc(moscow, yesterday, 8, 30);
    bot.freeze_at(d1_at);
    let d1 = deliver_digest_at(&mut bot, &db.pool, d1_at).await;
    // Прежняя сводка уже убрана, например кнопкой «🗑 Убрать».
    sqlx::query(
        "UPDATE notification_deliveries SET dismissed_at = $2
         WHERE user_id = $1 AND kind = 'digest'",
    )
    .bind(household.owner_id)
    .bind(d1_at)
    .execute(&db.pool)
    .await
    .expect("dismiss digest");

    let d2_at = local_utc(moscow, day, 8, 30);
    let stats = scheduler(&db.pool).tick(d2_at).await.expect("digest tick");
    assert_eq!(stats.delivered, 1, "{stats:?}");
    let payload = latest_digest_payload(&db.pool, bot.chat).await;
    assert!(payload.get("replaces").is_none(), "{payload}");

    bot.advance(d2_at - bot.now);
    bot.deliver().await;
    assert!(replace_delete_rows(&db.pool, bot.chat).await.is_empty());
    assert!(!deleted(&bot, d1));
    assert_eq!(alive_digests(&bot).len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn third_digest_replaces_only_the_second_one() {
    let db = support::isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    let household = household_for(&db.pool, bot.chat).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = local_today(&bot, moscow);
    digest_on(&db.pool, household.owner_id).await;

    let first_day = day - Duration::days(2);
    let d1_at = local_utc(moscow, first_day, 8, 30);
    bot.freeze_at(d1_at);
    let d1 = deliver_digest_at(&mut bot, &db.pool, d1_at).await;
    let d2_at = local_utc(moscow, day - Duration::days(1), 8, 30);
    let d2 = deliver_digest_at(&mut bot, &db.pool, d2_at).await;
    let first_dismissed =
        digest_dismissed_at(&db.pool, household.owner_id, &format!("digest:{first_day}")).await;
    assert!(first_dismissed.is_some(), "{first_dismissed:?}");

    let d3_at = local_utc(moscow, day, 8, 30);
    let stats = scheduler(&db.pool).tick(d3_at).await.expect("digest tick");
    assert_eq!(stats.delivered, 1, "{stats:?}");
    let payload = latest_digest_payload(&db.pool, bot.chat).await;
    assert_eq!(payload["replaces"], json!([d2]), "{payload}");

    bot.advance(d3_at - bot.now);
    bot.deliver().await;
    let deletes = replace_delete_rows(&db.pool, bot.chat).await;
    assert_eq!(deletes.len(), 2, "{deletes:?}");
    let first_key = format!("replace:{}:{d1}", bot.chat);
    assert_eq!(
        deletes
            .iter()
            .filter(|(_, key, _)| *key == first_key)
            .count(),
        1,
        "the first digest must get exactly one delete row: {deletes:?}"
    );
    assert!(
        deletes
            .iter()
            .any(|(_, key, _)| *key == format!("replace:{}:{d2}", bot.chat)),
        "{deletes:?}"
    );
    // Первая сводка уже отмечена: отметка не перезаписывается.
    assert_eq!(
        digest_dismissed_at(&db.pool, household.owner_id, &format!("digest:{first_day}")).await,
        first_dismissed
    );
    assert!(deleted(&bot, d1));
    assert!(deleted(&bot, d2));
    let alive = alive_digests(&bot);
    assert_eq!(alive.len(), 1, "{alive:?}");
    assert_ne!(alive[0].0, d1);
    assert_ne!(alive[0].0, d2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dead_digest_does_not_delete_the_previous_one() {
    let db = support::isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    let household = household_for(&db.pool, bot.chat).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = local_today(&bot, moscow);
    let yesterday = day - Duration::days(1);
    digest_on(&db.pool, household.owner_id).await;

    let d1_at = local_utc(moscow, yesterday, 8, 30);
    bot.freeze_at(d1_at);
    let d1 = deliver_digest_at(&mut bot, &db.pool, d1_at).await;

    let d2_at = local_utc(moscow, day, 8, 30);
    let stats = scheduler(&db.pool).tick(d2_at).await.expect("digest tick");
    assert_eq!(stats.delivered, 1, "{stats:?}");
    let payload = latest_digest_payload(&db.pool, bot.chat).await;
    assert_eq!(payload["replaces"], json!([d1]), "{payload}");

    // Новая сводка не доставляется: получатель заблокировал бота.
    bot.fake
        .fail_next(TelegramError::Forbidden("blocked".into()));
    bot.advance(d2_at - bot.now);
    bot.deliver().await;
    let status: String = sqlx::query_scalar(
        "SELECT status::text FROM outbox
         WHERE chat_id = $1 AND kind = 'message' AND dedupe_key LIKE 'notification:digest:%'
         ORDER BY seq DESC LIMIT 1",
    )
    .bind(bot.chat)
    .fetch_one(&db.pool)
    .await
    .expect("digest status");
    assert_eq!(status, "dead");
    assert!(replace_delete_rows(&db.pool, bot.chat).await.is_empty());
    assert!(!deleted(&bot, d1));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn digest_replacement_does_not_touch_another_recipient() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = digest_day();
    set_household_timezone(&db.pool, household.household_id, "Europe/Moscow").await;
    digest_on(&db.pool, household.owner_id).await;
    digest_on(&db.pool, household.member_id).await;
    let owner_chat = chat_id(&db.pool, household.owner_id).await;
    let member_chat = chat_id(&db.pool, household.member_id).await;
    let mut owner_bot = Harness::new(&db.pool, owner_chat);
    let mut member_bot = Harness::new(&db.pool, member_chat);

    let d1_at = local_utc(moscow, day, 8, 30);
    owner_bot.freeze_at(d1_at);
    member_bot.freeze_at(d1_at);
    let stats = scheduler(&db.pool).tick(d1_at).await.expect("digest tick");
    assert_eq!(stats.delivered, 2, "{stats:?}");
    owner_bot.deliver().await;
    member_bot.deliver().await;
    let (owner_d1, html) = owner_bot.latest();
    assert!(html.contains("Доброе утро"), "{html}");
    let (member_d1, html) = member_bot.latest();
    assert!(html.contains("Доброе утро"), "{html}");

    // На следующий день сводка приходит только владельцу.
    set_notifications(
        &db.pool,
        household.member_id,
        json!({"digest_enabled": false}),
    )
    .await;
    let d2_at = local_utc(moscow, day + Duration::days(1), 8, 30);
    let stats = scheduler(&db.pool).tick(d2_at).await.expect("digest tick");
    assert_eq!(stats.delivered, 1, "{stats:?}");
    let payload = latest_digest_payload(&db.pool, owner_chat).await;
    assert_eq!(payload["replaces"], json!([owner_d1]), "{payload}");
    owner_bot.advance(d2_at - owner_bot.now);
    owner_bot.deliver().await;
    assert!(deleted(&owner_bot, owner_d1));

    // Сводка другого получателя не отмечена и не удалена.
    assert!(
        digest_dismissed_at(&db.pool, household.member_id, &format!("digest:{day}"))
            .await
            .is_none()
    );
    assert!(replace_delete_rows(&db.pool, member_chat).await.is_empty());
    assert!(!deleted(&member_bot, member_d1));
    assert!(
        member_bot
            .fake
            .messages(member_chat)
            .iter()
            .any(|(id, _)| *id == member_d1)
    );
}

// ---------------------------------------------------------------------------
// Сводка по запросу (0019, design решение 7, пункт 6.1).
// ---------------------------------------------------------------------------

/// Ключ сводки по запросу для дня сводки и момента запроса.
fn manual_key(day: NaiveDate, at: DateTime<Utc>) -> String {
    format!("digest:{day}#manual:{}", at.timestamp_millis())
}

/// Запросить сводку и вернуть её id; `None` от функции означает занятого
/// получателя и в тестах не ожидается.
async fn manual_digest(pool: &PgPool, user_id: Uuid, at: DateTime<Utc>) -> Uuid {
    request_manual(pool, user_id, at)
        .await
        .expect("manual digest request")
        .expect("manual digest scheduled")
}

/// Живые сообщения сводки по запросу в чате: заголовок «☀️ На сегодня · …»,
/// а не строка «На сегодня» экрана меню.
fn alive_manual_digests(bot: &Harness) -> Vec<(i64, String)> {
    bot.fake
        .messages(bot.chat)
        .into_iter()
        .filter(|(_, html)| html.contains("На сегодня ·"))
        .collect()
}

/// Строки outbox сообщений сводок чата: статус, ключ и id сообщения.
async fn digest_outbox_rows(pool: &PgPool, chat: i64) -> Vec<(String, String, Option<i64>)> {
    sqlx::query_as(
        "SELECT status::text, dedupe_key, message_id FROM outbox
         WHERE chat_id = $1 AND dedupe_key LIKE 'notification:digest:%'
         ORDER BY seq",
    )
    .bind(chat)
    .fetch_all(pool)
    .await
    .expect("digest outbox rows")
}

/// Ключ транзакционной блокировки получателя, как в `jobs::delivery`.
fn recipient_lock_key(user_id: Uuid) -> i64 {
    i64::from_be_bytes(
        user_id.as_bytes()[8..]
            .try_into()
            .expect("uuid has at least sixteen bytes"),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manual_digest_midday_includes_a_task_added_after_morning_and_replaces_it() {
    let db = support::isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    let household = household_for(&db.pool, bot.chat).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = local_today(&bot, moscow);
    digest_on(&db.pool, household.owner_id).await;

    let morning_at = local_utc(moscow, day, 8, 30);
    bot.freeze_at(morning_at);
    let morning = deliver_digest_at(&mut bot, &db.pool, morning_at).await;

    create_task(
        &db,
        &household,
        "Позвонить в банк",
        Some(local_utc(moscow, day, 18, 0)),
        Some(household.owner_id),
        None,
    )
    .await;

    let manual_at = local_utc(moscow, day, 15, 0);
    let manual_id = manual_digest(&db.pool, household.owner_id, manual_at).await;

    let payload = latest_digest_payload(&db.pool, bot.chat).await;
    assert_eq!(payload["replaces"], json!([morning]), "{payload}");
    let (status, hash): (String, Option<String>) = sqlx::query_as(
        "SELECT status::text, digest_hash
         FROM notification_deliveries WHERE id = $1",
    )
    .bind(manual_id)
    .fetch_one(&db.pool)
    .await
    .expect("manual digest row");
    assert_eq!(status, "sent");
    assert!(hash.is_some(), "digest_hash written");
    assert!(
        digest_dismissed_at(&db.pool, household.owner_id, &format!("digest:{day}"))
            .await
            .is_some()
    );

    bot.advance(manual_at - bot.now);
    bot.deliver().await;
    assert!(deleted(&bot, morning));
    let alive = alive_manual_digests(&bot);
    assert_eq!(alive.len(), 1, "{alive:?}");
    assert!(alive[0].1.contains("Позвонить в банк"), "{}", alive[0].1);
    assert!(alive[0].1.contains("На сегодня"), "{}", alive[0].1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manual_digest_arrives_during_quiet_hours_with_morning_digest_disabled() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = digest_day();
    set_household_timezone(&db.pool, household.household_id, "Europe/Moscow").await;
    set_notifications(
        &db.pool,
        household.owner_id,
        json!({
            "digest_enabled": false,
            "digest_time": "08:30",
            "quiet_hours_enabled": true,
            "quiet_start": "22:00",
            "quiet_end": "08:00"
        }),
    )
    .await;
    create_all_day_task(
        &db,
        &household,
        "Полить цветы",
        day,
        Some(household.owner_id),
    )
    .await;

    let at = local_utc(moscow, day, 23, 0);
    manual_digest(&db.pool, household.owner_id, at).await;

    let html = messages(&db.pool, chat_id(&db.pool, household.owner_id).await)
        .await
        .pop()
        .expect("manual digest message");
    assert!(html.contains("На сегодня"), "{html}");
    assert!(html.contains("Полить цветы"), "{html}");

    let rows = digest_rows(&db.pool, household.owner_id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, manual_key(day, at));
    assert_eq!(rows[0].1, "sent");
    assert_eq!(rows[0].2, None);
    assert!(rows[0].3.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_manual_digests_leave_a_single_message_in_the_chat() {
    let db = support::isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    let household = household_for(&db.pool, bot.chat).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = local_today(&bot, moscow);
    digest_on(&db.pool, household.owner_id).await;
    create_all_day_task(
        &db,
        &household,
        "Разобрать почту",
        day,
        Some(household.owner_id),
    )
    .await;

    let first_at = local_utc(moscow, day, 15, 0);
    bot.freeze_at(first_at);
    manual_digest(&db.pool, household.owner_id, first_at).await;
    bot.deliver().await;
    let (first_msg, html) = bot.latest();
    assert!(html.contains("На сегодня"), "{html}");

    let second_at = first_at + Duration::minutes(5);
    manual_digest(&db.pool, household.owner_id, second_at).await;
    let payload = latest_digest_payload(&db.pool, bot.chat).await;
    assert_eq!(payload["replaces"], json!([first_msg]), "{payload}");
    assert!(
        digest_dismissed_at(&db.pool, household.owner_id, &manual_key(day, first_at))
            .await
            .is_some()
    );

    bot.advance(second_at - bot.now);
    bot.deliver().await;
    assert!(deleted(&bot, first_msg));
    let alive = alive_manual_digests(&bot);
    assert_eq!(alive.len(), 1, "{alive:?}");
    assert_ne!(alive[0].0, first_msg);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manual_digests_queued_back_to_back_leave_a_single_message() {
    let db = support::isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    let household = household_for(&db.pool, bot.chat).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = local_today(&bot, moscow);
    digest_on(&db.pool, household.owner_id).await;
    create_all_day_task(
        &db,
        &household,
        "Проверить почту",
        day,
        Some(household.owner_id),
    )
    .await;

    // Воркер outbox между нажатиями не проходит: первое сообщение ещё не
    // доставлено, у его строки нет `message_id`.
    let first_at = local_utc(moscow, day, 15, 0);
    bot.freeze_at(first_at);
    manual_digest(&db.pool, household.owner_id, first_at).await;
    let second_at = first_at + Duration::minutes(1);
    manual_digest(&db.pool, household.owner_id, second_at).await;

    let rows = digest_outbox_rows(&db.pool, bot.chat).await;
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[0].0, "skipped");
    assert_eq!(rows[0].2, None);
    assert_eq!(rows[1].0, "pending");
    assert_eq!(rows[1].2, None);
    assert!(
        digest_dismissed_at(&db.pool, household.owner_id, &manual_key(day, first_at))
            .await
            .is_some()
    );

    bot.advance(second_at - bot.now);
    bot.deliver().await;
    let rows = digest_outbox_rows(&db.pool, bot.chat).await;
    assert_eq!(rows[0].0, "skipped");
    assert_eq!(rows[1].0, "sent");
    assert!(rows[1].2.is_some());
    let alive = alive_manual_digests(&bot);
    assert_eq!(alive.len(), 1, "{alive:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn morning_digest_supersedes_a_manual_that_is_still_in_the_outbox() {
    let db = support::isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    let household = household_for(&db.pool, bot.chat).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = local_today(&bot, moscow);
    digest_on(&db.pool, household.owner_id).await;
    create_all_day_task(
        &db,
        &household,
        "Забрать заказ",
        day,
        Some(household.owner_id),
    )
    .await;

    let manual_at = local_utc(moscow, day, 8, 29);
    bot.freeze_at(manual_at);
    manual_digest(&db.pool, household.owner_id, manual_at).await;

    // Утренняя поставлена и передана в outbox до того, как воркер отправил
    // ручную: ручное сообщение вытесняется, утренняя остаётся.
    let morning_at = local_utc(moscow, day, 8, 30);
    let stats = scheduler(&db.pool)
        .tick(morning_at)
        .await
        .expect("morning tick");
    assert_eq!(stats.delivered, 1, "{stats:?}");
    let rows = digest_outbox_rows(&db.pool, bot.chat).await;
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[0].0, "skipped");
    assert_eq!(rows[1].0, "pending");
    assert!(
        digest_dismissed_at(&db.pool, household.owner_id, &manual_key(day, manual_at))
            .await
            .is_some()
    );

    bot.advance(morning_at - bot.now);
    bot.deliver().await;
    let rows = digest_outbox_rows(&db.pool, bot.chat).await;
    assert_eq!(rows[1].0, "sent");
    let alive = alive_digests(&bot);
    assert_eq!(alive.len(), 1, "{alive:?}");
    assert!(alive[0].1.contains("Забрать заказ"), "{}", alive[0].1);
    assert!(alive_manual_digests(&bot).is_empty());
}

/// Вытеснение ожидающей ручной сводки не теряет удаление прежней
/// доставленной: новая сводка наследует `replaces` вытесненной.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_manual_digests_inherit_the_replacement_of_a_delivered_one() {
    let db = support::isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    let household = household_for(&db.pool, bot.chat).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = local_today(&bot, moscow);
    digest_on(&db.pool, household.owner_id).await;
    create_all_day_task(
        &db,
        &household,
        "Проверить почту",
        day,
        Some(household.owner_id),
    )
    .await;

    // A уже доставлена и лежит в чате.
    let morning_at = local_utc(moscow, day, 8, 30);
    bot.freeze_at(morning_at);
    let delivered = deliver_digest_at(&mut bot, &db.pool, morning_at).await;
    assert_eq!(bot.latest().0, delivered);

    // B и C запрошены подряд: воркер между ними не проходил.
    let first_at = local_utc(moscow, day, 9, 0);
    let second_at = first_at + Duration::minutes(1);
    manual_digest(&db.pool, household.owner_id, first_at).await;
    manual_digest(&db.pool, household.owner_id, second_at).await;

    let payload = latest_digest_payload(&db.pool, bot.chat).await;
    assert_eq!(payload["replaces"], json!([delivered]), "{payload}");
    let rows = digest_outbox_rows(&db.pool, bot.chat).await;
    assert_eq!(rows.len(), 3, "{rows:?}");
    assert_eq!(rows[0].0, "sent", "{rows:?}");
    assert_eq!(rows[1].0, "skipped", "{rows:?}");
    assert_eq!(rows[2].0, "pending", "{rows:?}");

    bot.advance(second_at - bot.now);
    bot.deliver().await;

    let deletes = replace_delete_rows(&db.pool, bot.chat).await;
    assert_eq!(deletes.len(), 1, "{deletes:?}");
    assert_eq!(
        deletes[0].1,
        format!("replace:{}:{delivered}", bot.chat),
        "{deletes:?}"
    );
    assert!(deleted(&bot, delivered));
    let alive = alive_manual_digests(&bot);
    assert_eq!(alive.len(), 1, "{alive:?}");
    assert!(alive_digests(&bot).is_empty());
}

/// Сдвинуть `sent_at` строки outbox доставленной сводки по ключу вхождения.
async fn age_digest_sent_at_by_key(
    pool: &PgPool,
    user_id: Uuid,
    occurrence_key: &str,
    sent_at: DateTime<Utc>,
) {
    sqlx::query(
        "UPDATE outbox SET sent_at = $3
         WHERE seq = (SELECT outbox_seq FROM notification_deliveries
                      WHERE user_id = $1 AND kind = 'digest' AND occurrence_key = $2)",
    )
    .bind(user_id)
    .bind(occurrence_key)
    .bind(sent_at)
    .execute(pool)
    .await
    .expect("age digest message");
}

/// Перенесённый id старше окна замены отбрасывается: B получила удаление A,
/// пока та была свежей, но к моменту C A уже вне окна, и наследовать его
/// нельзя. Проверяется годность перенесённого id в `supersede_pending_manual`
/// (`o.sent_at >= $3`), а не выборка `replace_previous`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_manual_digests_drop_an_inherited_replacement_outside_the_window() {
    let db = support::isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    let household = household_for(&db.pool, bot.chat).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = local_today(&bot, moscow);
    digest_on(&db.pool, household.owner_id).await;
    create_all_day_task(
        &db,
        &household,
        "Проверить почту",
        day,
        Some(household.owner_id),
    )
    .await;

    // A уже доставлена и лежит в чате.
    let morning_at = local_utc(moscow, day, 8, 30);
    bot.freeze_at(morning_at);
    let delivered = deliver_digest_at(&mut bot, &db.pool, morning_at).await;

    // B запрошена, пока A свежая: B несёт удаление A.
    let first_at = local_utc(moscow, day, 9, 0);
    manual_digest(&db.pool, household.owner_id, first_at).await;
    let payload = latest_digest_payload(&db.pool, bot.chat).await;
    assert_eq!(payload["replaces"], json!([delivered]), "{payload}");

    // A состаривается после B и до C: `replace_previous` для B уже пропустил
    // её по своему окну, поэтому проверяется именно фильтр переноса.
    let second_at = first_at + Duration::minutes(1);
    age_digest_sent_at_by_key(
        &db.pool,
        household.owner_id,
        &format!("digest:{day}"),
        second_at - Duration::hours(48),
    )
    .await;

    // C вытесняет ожидающую B: перенесённый id A старше окна и отбрасывается.
    manual_digest(&db.pool, household.owner_id, second_at).await;
    let payload = latest_digest_payload(&db.pool, bot.chat).await;
    assert!(payload.get("replaces").is_none(), "{payload}");

    bot.advance(second_at - bot.now);
    bot.deliver().await;
    assert!(replace_delete_rows(&db.pool, bot.chat).await.is_empty());
}

/// Граница окна включительна: перенесённый id A ровно на 47 часов старше
/// момента C остаётся в `replaces` (`>=`, как у `replace_previous`; мутация
/// на `>` ломает тест).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_manual_digests_inherit_a_replacement_on_the_exact_window_boundary() {
    let db = support::isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    let household = household_for(&db.pool, bot.chat).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = local_today(&bot, moscow);
    digest_on(&db.pool, household.owner_id).await;
    create_all_day_task(
        &db,
        &household,
        "Проверить почту",
        day,
        Some(household.owner_id),
    )
    .await;

    let morning_at = local_utc(moscow, day, 8, 30);
    bot.freeze_at(morning_at);
    let delivered = deliver_digest_at(&mut bot, &db.pool, morning_at).await;

    let first_at = local_utc(moscow, day, 9, 0);
    manual_digest(&db.pool, household.owner_id, first_at).await;
    let payload = latest_digest_payload(&db.pool, bot.chat).await;
    assert_eq!(payload["replaces"], json!([delivered]), "{payload}");

    let second_at = first_at + Duration::minutes(1);
    age_digest_sent_at_by_key(
        &db.pool,
        household.owner_id,
        &format!("digest:{day}"),
        second_at - Duration::hours(47),
    )
    .await;

    manual_digest(&db.pool, household.owner_id, second_at).await;
    let payload = latest_digest_payload(&db.pool, bot.chat).await;
    assert_eq!(payload["replaces"], json!([delivered]), "{payload}");
}

/// Утренняя сводка, вытеснившая ожидающую ручную, наследует её удаление.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn morning_digest_inherits_the_replacement_of_a_superseded_manual() {
    let db = support::isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    let household = household_for(&db.pool, bot.chat).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = local_today(&bot, moscow);
    let yesterday = day - Duration::days(1);
    digest_on(&db.pool, household.owner_id).await;
    create_all_day_task(
        &db,
        &household,
        "Забрать заказ",
        day,
        Some(household.owner_id),
    )
    .await;

    // A — вчерашняя утренняя, уже в чате.
    let yesterday_at = local_utc(moscow, yesterday, 8, 30);
    bot.freeze_at(yesterday_at);
    let delivered = deliver_digest_at(&mut bot, &db.pool, yesterday_at).await;

    // B запрошена и ждёт воркер: она уже несёт удаление A.
    let manual_at = local_utc(moscow, day, 8, 29);
    manual_digest(&db.pool, household.owner_id, manual_at).await;
    let payload = latest_digest_payload(&db.pool, bot.chat).await;
    assert_eq!(payload["replaces"], json!([delivered]), "{payload}");

    // Утренняя C вытесняет B до доставки.
    let morning_at = local_utc(moscow, day, 8, 30);
    let stats = scheduler(&db.pool)
        .tick(morning_at)
        .await
        .expect("morning tick");
    assert_eq!(stats.delivered, 1, "{stats:?}");
    let payload = latest_digest_payload(&db.pool, bot.chat).await;
    assert_eq!(payload["replaces"], json!([delivered]), "{payload}");

    bot.advance(morning_at - bot.now);
    bot.deliver().await;

    assert!(deleted(&bot, delivered));
    let deletes = replace_delete_rows(&db.pool, bot.chat).await;
    assert_eq!(deletes.len(), 1, "{deletes:?}");
    let alive = alive_digests(&bot);
    assert_eq!(alive.len(), 1, "{alive:?}");
    assert!(alive_manual_digests(&bot).is_empty());
}

/// Перенос транзитивен: цепочка вытесненных ручных сводок передаёт удаление
/// прежней доставленной дальше.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replacement_is_inherited_transitively_through_queued_manuals() {
    let db = support::isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    let household = household_for(&db.pool, bot.chat).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = local_today(&bot, moscow);
    digest_on(&db.pool, household.owner_id).await;
    create_all_day_task(
        &db,
        &household,
        "Проверить почту",
        day,
        Some(household.owner_id),
    )
    .await;

    let morning_at = local_utc(moscow, day, 8, 30);
    bot.freeze_at(morning_at);
    let delivered = deliver_digest_at(&mut bot, &db.pool, morning_at).await;

    let first_at = local_utc(moscow, day, 9, 0);
    for offset in 0..3 {
        manual_digest(
            &db.pool,
            household.owner_id,
            first_at + Duration::minutes(offset),
        )
        .await;
    }

    let payload = latest_digest_payload(&db.pool, bot.chat).await;
    assert_eq!(payload["replaces"], json!([delivered]), "{payload}");
    let rows = digest_outbox_rows(&db.pool, bot.chat).await;
    assert_eq!(rows.len(), 4, "{rows:?}");
    for row in &rows[1..3] {
        assert_eq!(row.0, "skipped", "{rows:?}");
    }
    assert_eq!(rows[3].0, "pending", "{rows:?}");

    bot.advance(first_at + Duration::minutes(2) - bot.now);
    bot.deliver().await;

    assert!(deleted(&bot, delivered));
    let alive = alive_manual_digests(&bot);
    assert_eq!(alive.len(), 1, "{alive:?}");
}

/// Перенесённый id принадлежит прежнему чату получателя: удаление в новый чат
/// не ставится.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replacement_of_a_message_from_the_previous_chat_is_dropped() {
    let db = support::isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    let old_chat = bot.chat;
    let household = household_for(&db.pool, old_chat).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = local_today(&bot, moscow);
    digest_on(&db.pool, household.owner_id).await;
    create_all_day_task(
        &db,
        &household,
        "Проверить почту",
        day,
        Some(household.owner_id),
    )
    .await;

    let morning_at = local_utc(moscow, day, 8, 30);
    bot.freeze_at(morning_at);
    let delivered = deliver_digest_at(&mut bot, &db.pool, morning_at).await;

    // B в старом чате несёт удаление A.
    let first_at = local_utc(moscow, day, 9, 0);
    manual_digest(&db.pool, household.owner_id, first_at).await;
    let payload = latest_digest_payload(&db.pool, old_chat).await;
    assert_eq!(payload["replaces"], json!([delivered]), "{payload}");

    // Получатель сменил чат: C вытесняет B, но удаление A в новый чат не идёт.
    let new_chat = support::next_telegram_id();
    sqlx::query("UPDATE users SET telegram_chat_id = $2 WHERE id = $1")
        .bind(household.owner_id)
        .bind(new_chat)
        .execute(&db.pool)
        .await
        .expect("change chat");
    let second_at = first_at + Duration::minutes(1);
    manual_digest(&db.pool, household.owner_id, second_at).await;
    let payload = latest_digest_payload(&db.pool, new_chat).await;
    assert!(payload.get("replaces").is_none(), "{payload}");

    let mut new_bot = Harness::sharing(&db.pool, new_chat, bot.fake.clone());
    new_bot.freeze_at(second_at);
    new_bot.deliver().await;

    assert!(replace_delete_rows(&db.pool, new_chat).await.is_empty());
    assert!(!deleted(&new_bot, delivered));
    assert!(
        bot.fake
            .messages(old_chat)
            .iter()
            .any(|(id, _)| *id == delivered)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manual_after_a_queued_morning_is_coalesced_on_the_next_tick() {
    let db = support::isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    let household = household_for(&db.pool, bot.chat).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = local_today(&bot, moscow);
    digest_on(&db.pool, household.owner_id).await;
    create_all_day_task(
        &db,
        &household,
        "Забрать заказ",
        day,
        Some(household.owner_id),
    )
    .await;

    // Утренняя уже в outbox, но воркер её ещё не отправил.
    let morning_at = local_utc(moscow, day, 8, 30);
    bot.freeze_at(morning_at);
    let stats = scheduler(&db.pool)
        .tick(morning_at)
        .await
        .expect("morning tick");
    assert_eq!(stats.delivered, 1, "{stats:?}");

    // Ручная не отменяет и не сдвигает утреннюю: обе строки ждут отправки.
    let manual_at = morning_at + Duration::milliseconds(300);
    manual_digest(&db.pool, household.owner_id, manual_at).await;
    let rows = digest_outbox_rows(&db.pool, bot.chat).await;
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[0].0, "pending");
    assert_eq!(rows[1].0, "pending");
    assert!(
        digest_dismissed_at(&db.pool, household.owner_id, &format!("digest:{day}"))
            .await
            .is_none()
    );

    // Обе доставлены: окно гонки видно в чате до следующего тика.
    bot.advance(manual_at - bot.now);
    bot.deliver().await;
    let alive = |bot: &Harness| -> Vec<(i64, String)> {
        bot.fake
            .messages(bot.chat)
            .into_iter()
            .filter(|(_, html)| html.contains("Доброе утро") || html.contains("На сегодня ·"))
            .collect()
    };
    let both = alive(&bot);
    assert_eq!(both.len(), 2, "{both:?}");
    let morning_msg = both
        .iter()
        .find(|(_, html)| html.contains("Доброе утро"))
        .map(|(id, _)| *id)
        .expect("morning message");

    // Тик доставки оставляет последнюю живую сводку — ручную.
    let later = manual_at + Duration::minutes(1);
    scheduler(&db.pool)
        .tick(later)
        .await
        .expect("coalesce tick");
    bot.advance(later - bot.now);
    bot.deliver().await;
    assert!(deleted(&bot, morning_msg));
    let remaining = alive(&bot);
    assert_eq!(remaining.len(), 1, "{remaining:?}");
    assert!(
        remaining[0].1.contains("На сегодня ·"),
        "{}",
        remaining[0].1
    );
    assert!(
        digest_dismissed_at(&db.pool, household.owner_id, &format!("digest:{day}"))
            .await
            .is_some()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn morning_digest_after_a_manual_one_is_planned_and_replaces_it() {
    let db = support::isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    let household = household_for(&db.pool, bot.chat).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = local_today(&bot, moscow);
    digest_on(&db.pool, household.owner_id).await;
    create_all_day_task(
        &db,
        &household,
        "Забрать заказ",
        day,
        Some(household.owner_id),
    )
    .await;

    let manual_at = local_utc(moscow, day, 7, 0);
    bot.freeze_at(manual_at);
    manual_digest(&db.pool, household.owner_id, manual_at).await;
    bot.deliver().await;
    let (manual_msg, html) = bot.latest();
    assert!(html.contains("На сегодня"), "{html}");

    let morning_at = local_utc(moscow, day, 8, 30);
    let morning = deliver_digest_at(&mut bot, &db.pool, morning_at).await;
    assert!(
        digest_dismissed_at(&db.pool, household.owner_id, &manual_key(day, manual_at))
            .await
            .is_some()
    );
    assert!(deleted(&bot, manual_msg));
    let alive = alive_digests(&bot);
    assert_eq!(alive.len(), 1, "{alive:?}");
    assert_eq!(alive[0].0, morning);
    assert!(alive[0].1.contains("Забрать заказ"), "{}", alive[0].1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_digests_updates_the_manual_digest_as_the_latest() {
    let db = support::isolated_db().await;
    let mut stand = digest_sync_stand(&db).await;
    let manual_at = stand.at + Duration::minutes(30);
    let manual_id = manual_digest(&db.pool, stand.petya_id, manual_at).await;
    stand.petya.freeze_at(manual_at);
    stand.petya.deliver().await;
    let (manual_msg, html) = stand.petya.latest();
    assert!(html.contains("На сегодня"), "{html}");
    assert!(html.contains("Вынести мусор"), "{html}");

    stand.masha.advance(Duration::minutes(1));
    let masha_html = message_html(&stand.masha, stand.masha_msg);
    stand
        .masha
        .press_data(&button_of(&masha_html, "Вынести мусор"), stand.masha_msg)
        .await;

    let done_at = manual_at + Duration::minutes(1);
    let stats = digest_sync_tick(&mut stand, done_at).await;
    assert_eq!(stats.synced, 1, "{stats:?}");
    let edits = pending_edit_payloads(&db.pool, stand.petya.chat).await;
    assert_eq!(edits.len(), 1, "{edits:?}");
    assert_eq!(edits[0]["message_id"], json!(manual_msg));

    stand.petya.deliver().await;
    let html = message_html(&stand.petya, manual_msg);
    assert!(html.contains("На сегодня"), "{html}");
    assert!(html.contains("✅ Вынести мусор · Маша"), "{html}");
    assert!(html.contains("Выполнено 1 из 2"), "{html}");
    assert_digest_hash_matches_render(&db.pool, stand.petya_id).await;

    let mut conn = db.pool.acquire().await.expect("conn");
    let view = tasklerq::jobs::digest::digest_view(&mut conn, manual_id)
        .await
        .expect("manual digest view");
    assert_eq!(
        view.kind,
        tasklerq::telegram::bot::notify::DigestKind::Manual
    );
    assert_eq!(view.day, stand.day);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn digest_view_takes_the_manual_day_from_the_key_not_created_at() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = digest_day();
    set_household_timezone(&db.pool, household.household_id, "Europe/Moscow").await;
    let created_at = local_utc(moscow, day - Duration::days(1), 12, 0);
    let key = manual_key(day, created_at);
    let id = insert_digest_delivery(
        &db.pool,
        household.household_id,
        household.owner_id,
        &key,
        created_at,
    )
    .await;

    let chat = chat_id(&db.pool, household.owner_id).await;
    let mut conn = db.pool.acquire().await.expect("conn");
    let view = tasklerq::jobs::digest::digest_view(&mut conn, id)
        .await
        .expect("manual digest view");
    assert_eq!(
        view.kind,
        tasklerq::telegram::bot::notify::DigestKind::Manual
    );
    assert_eq!(view.day, day);

    let ui = tasklerq::telegram::bot::notify::digest_ui(&view);
    let html = tasklerq::telegram::bot::notify::render_notice(
        &mut conn,
        chat,
        household.household_id,
        &ui,
        created_at,
    )
    .await
    .expect("manual digest render");
    assert!(html.contains("На сегодня"), "{html}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manual_digest_respects_the_mine_scope() {
    let db = support::isolated_db().await;
    let stand = digest_sync_stand_scoped(&db, DigestScope::Mine).await;
    let moscow = chrono_tz::Europe::Moscow;
    create_task_without_assignment_notice(
        &db,
        &stand.household,
        "Задача Маши",
        Some(local_utc(moscow, stand.day, 19, 0)),
        stand.masha_id,
        None,
    )
    .await;

    let manual_at = stand.at + Duration::minutes(30);
    manual_digest(&db.pool, stand.petya_id, manual_at).await;
    let payload = latest_digest_payload(&db.pool, stand.petya.chat).await;
    let html = payload["html"]
        .as_str()
        .expect("manual digest payload")
        .to_owned();
    assert!(html.contains("Вынести мусор"), "{html}");
    assert!(html.contains("Забрать посылку"), "{html}");
    assert!(!html.contains("Задача Маши"), "{html}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn digest_delivery_and_manual_request_share_the_recipient_lock() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = digest_day();
    set_household_timezone(&db.pool, household.household_id, "Europe/Moscow").await;
    digest_on(&db.pool, household.owner_id).await;
    create_all_day_task(
        &db,
        &household,
        "Забрать заказ",
        day,
        Some(household.owner_id),
    )
    .await;
    let chat = chat_id(&db.pool, household.owner_id).await;
    let at = local_utc(moscow, day, 8, 30);

    // Держим блокировку получателя на другой транзакции: доставка сводки и
    // ручной запрос обязаны её заметить.
    let mut lock_conn = db.pool.acquire().await.expect("lock conn");
    let mut lock_tx = lock_conn.begin().await.expect("lock tx");
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(recipient_lock_key(household.owner_id))
        .execute(&mut *lock_tx)
        .await
        .expect("hold recipient lock");

    let stats = scheduler(&db.pool).tick(at).await.expect("locked tick");
    assert_eq!(stats.delivered, 0, "{stats:?}");
    let rows = digest_rows(&db.pool, household.owner_id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].1, "pending");

    let manual_at = at + Duration::minutes(1);
    assert!(
        request_manual(&db.pool, household.owner_id, manual_at)
            .await
            .expect("manual request")
            .is_none()
    );

    lock_tx.rollback().await.expect("release recipient lock");
    let later = at + Duration::minutes(2);
    let stats = scheduler(&db.pool)
        .tick(later)
        .await
        .expect("unlocked tick");
    assert_eq!(stats.delivered, 1, "{stats:?}");
    let rows = digest_rows(&db.pool, household.owner_id).await;
    assert_eq!(rows[0].1, "sent");
    assert!(rows[0].3.is_some());
    let sent = messages(&db.pool, chat).await;
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert!(sent[0].contains("Доброе утро"), "{}", sent[0]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manual_digest_is_not_queued_when_the_recipient_disappears_mid_transaction() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = digest_day();
    set_household_timezone(&db.pool, household.household_id, "Europe/Moscow").await;

    // Мягкое удаление между первичной проверкой и чтением записи: Telegram-поля
    // при удалении не очищаются.
    sqlx::query(
        "CREATE FUNCTION delete_digest_recipient() RETURNS trigger AS $$
         BEGIN
             UPDATE users SET deleted_at = now() WHERE id = NEW.user_id;
             RETURN NEW;
         END $$ LANGUAGE plpgsql",
    )
    .execute(&db.pool)
    .await
    .expect("function");
    sqlx::query(
        "CREATE TRIGGER delete_digest_recipient BEFORE INSERT ON notification_deliveries
         FOR EACH ROW WHEN (NEW.kind = 'digest') EXECUTE FUNCTION delete_digest_recipient()",
    )
    .execute(&db.pool)
    .await
    .expect("trigger");

    let at = local_utc(moscow, day, 15, 0);
    let result = request_manual(&db.pool, household.owner_id, at).await;

    sqlx::query("DROP TRIGGER delete_digest_recipient ON notification_deliveries")
        .execute(&db.pool)
        .await
        .expect("drop trigger");
    sqlx::query("DROP FUNCTION delete_digest_recipient()")
        .execute(&db.pool)
        .await
        .expect("drop function");

    assert!(result.expect("manual request").is_none());
    assert!(digest_rows(&db.pool, household.owner_id).await.is_empty());
    let deleted_at: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT deleted_at FROM users WHERE id = $1")
            .bind(household.owner_id)
            .fetch_one(&db.pool)
            .await
            .expect("recipient");
    assert!(deleted_at.is_none(), "request transaction rolled back");
}

// ---------------------------------------------------------------------------
// Шаг синхронизации отправленных уведомлений `sync_completed_notices`
// (0018, design решение 4, пункт 3.5).
// ---------------------------------------------------------------------------

/// Момент сценария синхронизации: всегда в будущем относительно реальных
/// отметок создания, чтобы отмена выбирала событие выполнения, а не создания.
fn sync_now() -> DateTime<Utc> {
    rounded_now() + Duration::minutes(1)
}

/// Стенд: общая задача со сроком и напоминания, доставленные обоим участникам.
struct SyncStand {
    bot: Harness,
    member: Harness,
    household: TestHousehold,
    item: Item,
    member_id: Uuid,
    bot_msg: i64,
    member_msg: i64,
    at: DateTime<Utc>,
}

async fn shared_task_stand(db: &TestDb, title: &str) -> SyncStand {
    let at = sync_now();
    shared_task_stand_at(db, title, at + Duration::minutes(15), at).await
}

async fn shared_task_stand_at(
    db: &TestDb,
    title: &str,
    due: DateTime<Utc>,
    at: DateTime<Utc>,
) -> SyncStand {
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let member_id = add_member(&db.pool, household.household_id, "Маша", false).await;
    let member_chat = chat_id(&db.pool, member_id).await;
    let mut member = Harness::sharing(&db.pool, member_chat, bot.fake.clone()).named("Маша");
    bot.freeze_at(at);
    member.freeze_at(at);
    let item = create_task(db, &household, title, Some(due), None, None).await;
    tick_and_deliver(&mut bot, &db.pool, at).await;
    member.deliver().await;
    let (bot_msg, bot_html) = bot.latest();
    let (member_msg, member_html) = member.latest();
    assert!(bot_html.contains(title), "{bot_html}");
    assert!(member_html.contains(title), "{member_html}");
    SyncStand {
        bot,
        member,
        household,
        item,
        member_id,
        bot_msg,
        member_msg,
        at,
    }
}

/// Тик синхронизации с доставкой исходящих обоим участникам стенда.
async fn sync_tick(stand: &mut SyncStand, at: DateTime<Utc>) -> TickStats {
    let stats = scheduler(&stand.bot.pool)
        .tick(at)
        .await
        .expect("sync tick");
    if at > stand.bot.now {
        stand.bot.advance(at - stand.bot.now);
    }
    if at > stand.member.now {
        stand.member.advance(at - stand.member.now);
    }
    stand.bot.deliver().await;
    stand.member.deliver().await;
    stats
}

/// Выполнить задачу от имени актора на момент `at`.
async fn complete_as(
    db: &TestDb,
    household: &TestHousehold,
    item_id: Uuid,
    actor: &Actor,
    at: DateTime<Utc>,
) {
    let mut conn = db.pool.acquire().await.expect("conn");
    items::complete(
        &mut conn,
        household.household_id,
        actor,
        item_id,
        None,
        "Europe/Moscow",
        at,
    )
    .await
    .expect("complete");
}

/// Id последнего действующего выполнения задачи.
async fn latest_completion_event(pool: &PgPool, item_id: Uuid) -> Option<Uuid> {
    sqlx::query_scalar(
        "SELECT e.id FROM item_events e
         WHERE e.item_id = $1 AND e.event_type = 'completed' AND e.undo_of IS NULL
           AND NOT EXISTS (SELECT 1 FROM item_events u WHERE u.undo_of = e.id)
         ORDER BY e.created_at DESC, e.id DESC LIMIT 1",
    )
    .bind(item_id)
    .fetch_optional(pool)
    .await
    .expect("latest completion event")
}

/// Строки отложенного удаления сообщения: статус, ключ, время и payload.
async fn notice_expire_rows(
    pool: &PgPool,
    chat: i64,
    message_id: i64,
) -> Vec<(String, String, DateTime<Utc>, serde_json::Value)> {
    sqlx::query_as(
        "SELECT status::text, dedupe_key, next_attempt_at, payload FROM outbox
         WHERE chat_id = $1 AND kind = 'delete' AND dedupe_key LIKE $2
         ORDER BY seq",
    )
    .bind(chat)
    .bind(format!("notice-expire:{chat}:{message_id}:%"))
    .fetch_all(pool)
    .await
    .expect("notice expire rows")
}

/// Поля закрытия доставок сообщения (сопоставление через outbox).
async fn notice_closed_rows(
    pool: &PgPool,
    chat: i64,
    message_id: i64,
) -> Vec<(Option<DateTime<Utc>>, Option<String>, Option<Uuid>)> {
    sqlx::query_as(
        "SELECT d.closed_at, d.closed_reason, d.closed_by_event
         FROM notification_deliveries d
         JOIN outbox o ON o.seq = d.outbox_seq
         WHERE o.chat_id = $1 AND o.message_id = $2
         ORDER BY d.created_at, d.id",
    )
    .bind(chat)
    .bind(message_id)
    .fetch_all(pool)
    .await
    .expect("notice closed rows")
}

/// Живое сообщение чата по id.
fn message_html(bot: &Harness, message_id: i64) -> String {
    bot.fake
        .messages(bot.chat)
        .into_iter()
        .find(|(id, _)| *id == message_id)
        .map(|(_, html)| html)
        .unwrap_or_else(|| panic!("message {message_id} is not alive"))
}

/// Число строк правки в outbox чата.
async fn edit_count(pool: &PgPool, chat: i64) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM outbox WHERE chat_id = $1 AND kind = 'edit'")
        .bind(chat)
        .fetch_one(pool)
        .await
        .expect("edit count")
}

/// В сообщении единственная кнопка «🗑 Убрать».
fn assert_only_dismiss_button(html: &str) {
    let labels: Vec<String> = buttons(html).into_iter().map(|(label, _)| label).collect();
    assert_eq!(labels, ["🗑 Убрать"], "{html}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_closes_other_member_copy_when_done_from_their_notice() {
    let db = support::isolated_db().await;
    let mut stand = shared_task_stand(&db, "Вынести мусор").await;
    let at = stand.at;
    let done_at = at + Duration::minutes(1);
    stand.member.advance(Duration::minutes(1));
    let member_html = message_html(&stand.member, stand.member_msg);
    stand
        .member
        .press_data(&button_of(&member_html, "Выполнено"), stand.member_msg)
        .await;
    let event_id = latest_completion_event(&db.pool, stand.item.id)
        .await
        .expect("completion event");

    sync_tick(&mut stand, done_at).await;

    // Копия бота перерисована с именем выполнившей Маши.
    let html = message_html(&stand.bot, stand.bot_msg);
    assert!(
        html.contains("✅ Выполнено: Вынести мусор · Маша"),
        "{html}"
    );
    assert_only_dismiss_button(&html);
    let rows = notice_closed_rows(&db.pool, stand.bot.chat, stand.bot_msg).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert!(rows[0].0.is_some(), "{rows:?}");
    assert_eq!(rows[0].1.as_deref(), Some("completed"), "{rows:?}");
    assert_eq!(rows[0].2, Some(event_id), "{rows:?}");
    // Удаление на completed_at + 10 минут с ключом по событию выполнения.
    let deletes = notice_expire_rows(&db.pool, stand.bot.chat, stand.bot_msg).await;
    assert_eq!(deletes.len(), 1, "{deletes:?}");
    assert_eq!(deletes[0].0, "pending", "{deletes:?}");
    assert_eq!(
        deletes[0].1,
        format!(
            "notice-expire:{}:{}:{event_id}",
            stand.bot.chat, stand.bot_msg
        ),
        "{deletes:?}"
    );
    assert_eq!(deletes[0].2, done_at + Duration::minutes(10), "{deletes:?}");
    // Своё сообщение Маши шаг не трогает: его ведёт show_done с «Отменить».
    let own = message_html(&stand.member, stand.member_msg);
    assert!(own.contains("↩️ Отменить"), "{own}");
    assert!(
        !notice_expire_rows(&db.pool, stand.member.chat, stand.member_msg)
            .await
            .iter()
            .any(|row| row.1.ends_with(&event_id.to_string())),
        "step must not touch the message completed from itself"
    );
    // Через 10 минут копия удаляется из чата бота.
    stand.bot.advance(Duration::minutes(10));
    stand.bot.deliver().await;
    assert!(deleted(&stand.bot, stand.bot_msg));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_closes_copy_without_name_when_done_from_the_card() {
    let db = support::isolated_db().await;
    let mut stand = shared_task_stand(&db, "Вынести мусор").await;
    let at = stand.at;
    // Владелец выполняет из карточки: actor совпадает с получателем его копии.
    complete_as(
        &db,
        &stand.household,
        stand.item.id,
        &stand.household.owner(),
        at + Duration::minutes(1),
    )
    .await;

    sync_tick(&mut stand, at + Duration::minutes(1)).await;

    let html = message_html(&stand.bot, stand.bot_msg);
    assert!(html.contains("✅ Выполнено: Вынести мусор"), "{html}");
    assert!(
        !html.contains(" · "),
        "no name for the recipient themselves: {html}"
    );
    assert_only_dismiss_button(&html);
    let rows = notice_closed_rows(&db.pool, stand.bot.chat, stand.bot_msg).await;
    assert_eq!(rows[0].1.as_deref(), Some("completed"), "{rows:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_closes_copy_completed_via_http_api() {
    let db = support::isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    // HTTP-обработчик выполняет с реальным Utc::now(), поэтому доставка тоже
    // в реальном времени: выполнение должно быть после outbox.created_at.
    let at = Utc::now();
    let due = at + Duration::minutes(15);
    let item = create_task(&db, &household, "Оплатить интернет", Some(due), None, None).await;
    tick_and_deliver(&mut bot, &db.pool, at).await;
    let (msg, html) = bot.latest();
    assert!(html.contains("Оплатить интернет"), "{html}");
    let token =
        support::issue_token(&db.pool, household.household_id, &["ha:read", "ha:write"]).await;
    let app = support::app(&db.pool);
    let (status, body) = support::request(
        &app,
        Method::POST,
        &format!("/api/v1/items/{}/complete", item.id),
        Some(&token),
        Some(json!({"version": item.version})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    scheduler(&db.pool)
        .tick(Utc::now())
        .await
        .expect("sync tick");
    bot.deliver().await;

    let html = message_html(&bot, msg);
    assert!(html.contains("✅ Выполнено: Оплатить интернет"), "{html}");
    assert!(
        !html.contains(" · "),
        "API completion has no actor name: {html}"
    );
    assert_only_dismiss_button(&html);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_closes_copy_completed_via_home_assistant() {
    let db = support::isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let at = Utc::now();
    let token =
        support::issue_token(&db.pool, household.household_id, &["ha:read", "ha:write"]).await;
    let app = support::app(&db.pool);
    let due = at + Duration::minutes(15);
    // Задача создаётся и выполняется через HTTP-обработчик интеграции HA.
    let (status, item) = support::request(
        &app,
        Method::POST,
        &format!("/api/v1/lists/{}/items", household.tasks_list_id),
        Some(&token),
        Some(json!({
            "title": "Проверить HA",
            "source": "ha_todo",
            "due_at": due.to_rfc3339(),
            "assignee_user_id": household.owner_id,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{item}");
    let item_id = item["id"].as_str().expect("item id").to_owned();
    let version = item["version"].as_i64().expect("version");

    tick_and_deliver(&mut bot, &db.pool, at).await;
    let (msg, html) = bot.latest();
    assert!(html.contains("Проверить HA"), "{html}");

    let (status, completed) = support::request(
        &app,
        Method::POST,
        &format!("/api/v1/items/{item_id}/complete"),
        Some(&token),
        Some(json!({"version": version})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{completed}");

    scheduler(&db.pool)
        .tick(Utc::now())
        .await
        .expect("sync tick");
    bot.deliver().await;
    let html = message_html(&bot, msg);
    assert!(html.contains("✅ Выполнено: Проверить HA"), "{html}");
    assert!(
        !html.contains(" · "),
        "HA completion has no actor name: {html}"
    );
    assert_only_dismiss_button(&html);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_closes_copy_when_completion_happened_while_queued() {
    let db = support::isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let at = sync_now();
    bot.freeze_at(at);
    let due = at + Duration::minutes(15);
    let item = create_task(
        &db,
        &household,
        "В очереди",
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
        at,
    )
    .await
    .expect("reminder delivery");
    // Тик ставит сообщение в outbox, но воркер ещё не отправил его.
    scheduler(&db.pool).tick(at).await.expect("queue tick");
    // Выполнение, пока сообщение стояло в очереди (после outbox.created_at).
    complete_as(
        &db,
        &household,
        item.id,
        &household.owner(),
        at + Duration::minutes(1),
    )
    .await;
    bot.advance(Duration::minutes(1));
    bot.deliver().await;
    let (msg, html) = bot.latest();
    assert!(html.contains("В очереди"), "{html}");

    scheduler(&db.pool)
        .tick(at + Duration::minutes(2))
        .await
        .expect("sync tick");
    bot.advance(Duration::minutes(1));
    bot.deliver().await;
    let html = message_html(&bot, msg);
    assert!(html.contains("✅ Выполнено: В очереди"), "{html}");
    assert_only_dismiss_button(&html);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_returns_copy_when_completion_undone_before_delete() {
    let db = support::isolated_db().await;
    let mut stand = shared_task_stand(&db, "Вынести мусор").await;
    let at = stand.at;
    complete_as(
        &db,
        &stand.household,
        stand.item.id,
        &stand.household.owner(),
        at + Duration::minutes(1),
    )
    .await;
    sync_tick(&mut stand, at + Duration::minutes(1)).await;
    assert!(message_html(&stand.bot, stand.bot_msg).contains("✅ Выполнено"));

    // Отмена в 09:04.
    let mut conn = db.pool.acquire().await.expect("conn");
    items::undo_last(
        &mut conn,
        stand.household.household_id,
        &stand.household.owner(),
        stand.item.id,
        at + Duration::minutes(5),
    )
    .await
    .expect("undo");
    drop(conn);
    sync_tick(&mut stand, at + Duration::minutes(5)).await;

    let html = message_html(&stand.bot, stand.bot_msg);
    assert!(html.contains("⏰ Пора"), "{html}");
    assert!(html.contains("✓ Выполнено"), "{html}");
    assert!(
        buttons(&html).len() > 1,
        "action buttons must return: {html}"
    );
    let deletes = notice_expire_rows(&db.pool, stand.bot.chat, stand.bot_msg).await;
    assert_eq!(deletes.len(), 1, "{deletes:?}");
    assert_eq!(deletes[0].0, "skipped", "{deletes:?}");
    let rows = notice_closed_rows(&db.pool, stand.bot.chat, stand.bot_msg).await;
    assert_eq!(rows[0], (None, None, None), "{rows:?}");
    // В 09:10 сообщение не удаляется.
    stand.bot.advance(Duration::minutes(6));
    stand.bot.deliver().await;
    assert!(!deleted(&stand.bot, stand.bot_msg));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_returns_copy_when_completion_removed_by_reopen() {
    let db = support::isolated_db().await;
    let mut stand = shared_task_stand(&db, "Вынести мусор").await;
    let at = stand.at;
    complete_as(
        &db,
        &stand.household,
        stand.item.id,
        &stand.household.owner(),
        at + Duration::minutes(1),
    )
    .await;
    sync_tick(&mut stand, at + Duration::minutes(1)).await;
    assert!(message_html(&stand.bot, stand.bot_msg).contains("✅ Выполнено"));

    let mut conn = db.pool.acquire().await.expect("conn");
    items::reopen(
        &mut conn,
        stand.household.household_id,
        &stand.household.owner(),
        stand.item.id,
        None,
        at + Duration::minutes(5),
    )
    .await
    .expect("reopen");
    drop(conn);
    sync_tick(&mut stand, at + Duration::minutes(5)).await;

    let html = message_html(&stand.bot, stand.bot_msg);
    assert!(html.contains("⏰ Пора"), "{html}");
    assert!(
        buttons(&html).len() > 1,
        "action buttons must return: {html}"
    );
    let deletes = notice_expire_rows(&db.pool, stand.bot.chat, stand.bot_msg).await;
    assert_eq!(deletes[0].0, "skipped", "{deletes:?}");
    let rows = notice_closed_rows(&db.pool, stand.bot.chat, stand.bot_msg).await;
    assert_eq!(rows[0], (None, None, None), "{rows:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_closes_today_copies_of_daily_task_and_tomorrow_reminder_arrives() {
    let db = support::isolated_db().await;
    let at = sync_now();
    let due = at + Duration::minutes(15);
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let member_id = add_member(&db.pool, household.household_id, "Маша", false).await;
    let member_chat = chat_id(&db.pool, member_id).await;
    let mut member = Harness::sharing(&db.pool, member_chat, bot.fake.clone()).named("Маша");
    bot.freeze_at(at);
    member.freeze_at(at);
    let item = create_task(
        &db,
        &household,
        "Ежедневная",
        Some(due),
        None,
        Some(support::recurrence(RecurrenceFreq::Daily, Vec::new())),
    )
    .await;
    tick_and_deliver(&mut bot, &db.pool, at).await;
    member.deliver().await;
    let (bot_msg, _) = bot.latest();
    let (member_msg, _) = member.latest();

    // Маша выполняет сегодняшнее вхождение: срок переходит на завтра.
    complete_as(
        &db,
        &household,
        item.id,
        &Actor::user(member_id, UserRole::Member),
        at + Duration::minutes(1),
    )
    .await;
    scheduler(&db.pool)
        .tick(at + Duration::minutes(1))
        .await
        .expect("sync tick");
    bot.advance(Duration::minutes(1));
    member.advance(Duration::minutes(1));
    bot.deliver().await;
    member.deliver().await;
    let html = message_html(&bot, bot_msg);
    assert!(html.contains("✅ Выполнено: Ежедневная · Маша"), "{html}");
    assert!(message_html(&member, member_msg).contains("✅ Выполнено"));

    // Напоминание о завтрашнем вхождении приходит по расписанию обоим.
    let tomorrow_at = due + Duration::days(1) - Duration::minutes(15);
    bot.advance(tomorrow_at - bot.now);
    member.advance(tomorrow_at - member.now);
    let stats = scheduler(&db.pool)
        .tick(tomorrow_at)
        .await
        .expect("tomorrow tick");
    assert_eq!(stats.delivered, 2, "{stats:?}");
    bot.deliver().await;
    member.deliver().await;
    assert!(bot.latest().1.contains("Ежедневная"), "{}", bot.latest().1);
    assert!(bot.latest().1.contains("✓ Выполнено"), "{}", bot.latest().1);
    assert_eq!(delivery_rows(&db.pool, item.id).await.len(), 4);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_closes_copy_of_task_with_subsecond_due() {
    let db = support::isolated_db().await;
    let at = sync_now();
    let due = at + Duration::minutes(15) - Duration::seconds(1) + Duration::microseconds(123_456);
    let mut stand = shared_task_stand_at(&db, "Доли секунды", due, at).await;
    complete_as(
        &db,
        &stand.household,
        stand.item.id,
        &stand.household.owner(),
        at + Duration::minutes(1),
    )
    .await;

    sync_tick(&mut stand, at + Duration::minutes(1)).await;

    let html = message_html(&stand.bot, stand.bot_msg);
    assert!(html.contains("✅ Выполнено: Доли секунды"), "{html}");
    assert_only_dismiss_button(&html);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_closes_delivered_snooze_copy() {
    let db = support::isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let at = sync_now();
    bot.freeze_at(at);
    let due = at + Duration::minutes(15);
    let item = create_task(
        &db,
        &household,
        "Отложенная",
        Some(due),
        Some(household.owner_id),
        None,
    )
    .await;
    tick_and_deliver(&mut bot, &db.pool, at).await;
    let (msg, html) = bot.latest();
    bot.press_data(&button_of(&html, "на час"), msg).await;
    let snooze_at = at + Duration::hours(1);
    bot.advance(Duration::hours(1));
    scheduler(&db.pool)
        .tick(snooze_at)
        .await
        .expect("snooze tick");
    bot.deliver().await;
    let (snooze_msg, snooze_html) = bot.latest();
    assert!(snooze_html.contains("⏰ Пора"), "{snooze_html}");

    complete_as(
        &db,
        &household,
        item.id,
        &household.owner(),
        snooze_at + Duration::minutes(1),
    )
    .await;
    scheduler(&db.pool)
        .tick(snooze_at + Duration::minutes(1))
        .await
        .expect("sync tick");
    bot.advance(Duration::minutes(1));
    bot.deliver().await;

    let html = message_html(&bot, snooze_msg);
    assert!(html.contains("✅ Выполнено: Отложенная"), "{html}");
    assert_only_dismiss_button(&html);
    let rows = notice_closed_rows(&db.pool, bot.chat, snooze_msg).await;
    assert_eq!(rows[0].1.as_deref(), Some("completed"), "{rows:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_closes_assignment_notice_copy_on_completion() {
    let db = support::isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let member_id = add_member(&db.pool, household.household_id, "Маша", false).await;
    let at = sync_now();
    bot.freeze_at(at);
    let due = at + Duration::hours(3);
    let item = create_task(&db, &household, "Купить торт", Some(due), None, None).await;
    let item = update_assignee(
        &db,
        &household,
        &Actor::user(member_id, UserRole::Member),
        &item,
        Some(household.owner_id),
        at,
    )
    .await;
    tick_and_deliver(&mut bot, &db.pool, at).await;
    let (msg, html) = bot.latest();
    assert!(html.contains("Вам назначили задачу"), "{html}");

    complete_as(
        &db,
        &household,
        item.id,
        &Actor::user(member_id, UserRole::Member),
        at + Duration::minutes(1),
    )
    .await;
    scheduler(&db.pool)
        .tick(at + Duration::minutes(1))
        .await
        .expect("sync tick");
    bot.advance(Duration::minutes(1));
    bot.deliver().await;

    let html = message_html(&bot, msg);
    assert!(html.contains("✅ Выполнено: Купить торт · Маша"), "{html}");
    assert_only_dismiss_button(&html);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_redraws_snoozed_message_before_snooze_delivery() {
    let db = support::isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let at = sync_now();
    bot.freeze_at(at);
    let due = at + Duration::minutes(15);
    let item = create_task(
        &db,
        &household,
        "Напомню позже",
        Some(due),
        Some(household.owner_id),
        None,
    )
    .await;
    tick_and_deliver(&mut bot, &db.pool, at).await;
    let (msg, html) = bot.latest();
    // «😴 Напомню …»: snooze-доставка ещё не доставлена.
    bot.press_data(&button_of(&html, "на час"), msg).await;
    assert!(
        message_html(&bot, msg).contains("😴 Напомню"),
        "{}",
        message_html(&bot, msg)
    );

    complete_as(
        &db,
        &household,
        item.id,
        &household.owner(),
        at + Duration::minutes(1),
    )
    .await;
    scheduler(&db.pool)
        .tick(at + Duration::minutes(1))
        .await
        .expect("sync tick");
    bot.advance(Duration::minutes(1));
    bot.deliver().await;

    // «😴 Напомню …» перерисовано в «✅ Выполнено»: правка доставлена. Отмена
    // pending-snooze из 0017 затем снимает это сообщение своим удалением.
    assert!(
        bot.fake.calls().iter().any(|call| matches!(
            call,
            Call::EditRich { id, html, .. }
                if *id == msg && html.contains("✅ Выполнено: Напомню позже")
        )),
        "the snoozed message must be redrawn as completed: {:?}",
        bot.fake.calls()
    );
    let rows = notice_closed_rows(&db.pool, bot.chat, msg).await;
    assert_eq!(rows[0].1.as_deref(), Some("completed"), "{rows:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_leaves_dismissed_copy_alone() {
    let db = support::isolated_db().await;
    let mut stand = shared_task_stand(&db, "Убранная").await;
    let at = stand.at;
    let member_html = message_html(&stand.member, stand.member_msg);
    stand
        .member
        .press_data(&button_of(&member_html, "Убрать"), stand.member_msg)
        .await;
    complete_as(
        &db,
        &stand.household,
        stand.item.id,
        &stand.household.owner(),
        at,
    )
    .await;

    sync_tick(&mut stand, at).await;

    let rows = notice_closed_rows(&db.pool, stand.member.chat, stand.member_msg).await;
    assert_eq!(rows[0], (None, None, None), "{rows:?}");
    assert_eq!(edit_count(&db.pool, stand.member.chat).await, 0);
    assert!(
        notice_expire_rows(&db.pool, stand.member.chat, stand.member_msg)
            .await
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_leaves_copy_sent_more_than_47_hours_ago_alone() {
    let db = support::isolated_db().await;
    let mut stand = shared_task_stand(&db, "Старая").await;
    let at = stand.at;
    complete_as(
        &db,
        &stand.household,
        stand.item.id,
        &stand.household.owner(),
        at + Duration::minutes(1),
    )
    .await;
    let now = at + Duration::hours(48);
    // Отметка доставки свежая, но сообщение отправлено 48 часов назад.
    sqlx::query("UPDATE notification_deliveries SET updated_at = $2 WHERE item_id = $1")
        .bind(stand.item.id)
        .bind(now)
        .execute(&db.pool)
        .await
        .expect("refresh updated_at");
    stand.bot.advance(now - stand.bot.now);
    stand.member.advance(now - stand.member.now);

    scheduler(&db.pool).tick(now).await.expect("sync tick");
    stand.bot.deliver().await;
    stand.member.deliver().await;

    assert_eq!(edit_count(&db.pool, stand.bot.chat).await, 0);
    let rows = notice_closed_rows(&db.pool, stand.bot.chat, stand.bot_msg).await;
    assert_eq!(rows[0], (None, None, None), "{rows:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_leaves_copy_when_household_deleted_alone() {
    let db = support::isolated_db().await;
    let mut stand = shared_task_stand(&db, "Удалённый household").await;
    let at = stand.at;
    complete_as(
        &db,
        &stand.household,
        stand.item.id,
        &stand.household.owner(),
        at + Duration::minutes(1),
    )
    .await;
    // Household удалён: условие живого получателя не выполняется.
    sqlx::query("UPDATE households SET deleted_at = $2 WHERE id = $1")
        .bind(stand.household.household_id)
        .bind(at + Duration::minutes(1))
        .execute(&db.pool)
        .await
        .expect("delete household");

    sync_tick(&mut stand, at + Duration::minutes(1)).await;

    let rows = notice_closed_rows(&db.pool, stand.member.chat, stand.member_msg).await;
    assert_eq!(rows[0], (None, None, None), "{rows:?}");
    assert_eq!(edit_count(&db.pool, stand.member.chat).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_leaves_copy_of_excluded_recipient_alone() {
    let db = support::isolated_db().await;
    let mut stand = shared_task_stand(&db, "Исключённый").await;
    let at = stand.at;
    sqlx::query("UPDATE users SET deleted_at = $2 WHERE id = $1")
        .bind(stand.member_id)
        .bind(at)
        .execute(&db.pool)
        .await
        .expect("exclude member");
    complete_as(
        &db,
        &stand.household,
        stand.item.id,
        &stand.household.owner(),
        at,
    )
    .await;

    sync_tick(&mut stand, at).await;

    let rows = notice_closed_rows(&db.pool, stand.member.chat, stand.member_msg).await;
    assert_eq!(rows[0], (None, None, None), "{rows:?}");
    assert_eq!(edit_count(&db.pool, stand.member.chat).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_does_not_touch_batch_deliveries() {
    let db = support::isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let at = sync_now();
    bot.freeze_at(at);
    let due = at + Duration::minutes(15);
    let item = create_task(
        &db,
        &household,
        "В сводном",
        Some(due),
        Some(household.owner_id),
        None,
    )
    .await;
    let batch_id = support::notifications::create_empty_batch(&db.pool, &household, at).await;
    let seq = insert_outbox(&db.pool, bot.chat, "batch-block").await;
    sqlx::query("UPDATE outbox SET status = 'sent', message_id = 501, sent_at = $2 WHERE seq = $1")
        .bind(seq)
        .bind(at)
        .execute(&db.pool)
        .await
        .expect("mark batch sent");
    sqlx::query(
        "INSERT INTO notification_deliveries (
             id, household_id, user_id, item_id, occurrence_key, kind,
             status, scheduled_at, batch_id, outbox_seq, created_at, updated_at
         )
         VALUES ($1, $2, $3, $4, $5, 'reminder', 'sent', $6, $7, $8, $6, $6)",
    )
    .bind(Uuid::now_v7())
    .bind(household.household_id)
    .bind(household.owner_id)
    .bind(item.id)
    .bind(occurrence_key(due))
    .bind(at)
    .bind(batch_id)
    .bind(seq)
    .execute(&db.pool)
    .await
    .expect("batch delivery");

    complete_as(&db, &household, item.id, &household.owner(), at).await;
    scheduler(&db.pool).tick(at).await.expect("sync tick");
    bot.deliver().await;

    assert_eq!(edit_count(&db.pool, bot.chat).await, 0);
    let closed: Option<String> =
        sqlx::query_scalar("SELECT closed_reason FROM notification_deliveries WHERE batch_id = $1")
            .bind(batch_id)
            .fetch_one(&db.pool)
            .await
            .expect("batch closed_reason");
    assert_eq!(closed, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_repeat_tick_does_not_duplicate() {
    let db = support::isolated_db().await;
    let mut stand = shared_task_stand(&db, "Повтор").await;
    let at = stand.at + Duration::minutes(1);
    complete_as(
        &db,
        &stand.household,
        stand.item.id,
        &stand.household.owner(),
        at,
    )
    .await;

    sync_tick(&mut stand, at).await;
    assert_eq!(edit_count(&db.pool, stand.bot.chat).await, 1);
    assert_eq!(
        notice_expire_rows(&db.pool, stand.bot.chat, stand.bot_msg)
            .await
            .len(),
        1
    );

    // Повторный тик ничего не дублирует.
    sync_tick(&mut stand, at).await;
    assert_eq!(edit_count(&db.pool, stand.bot.chat).await, 1);
    assert_eq!(
        notice_expire_rows(&db.pool, stand.bot.chat, stand.bot_msg)
            .await
            .len(),
        1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_closes_petya_copy_when_shared_task_assigned_to_masha() {
    let db = support::isolated_db().await;
    let mut stand = shared_task_stand(&db, "Вынести мусор").await;
    let at = stand.at;
    update_assignee(
        &db,
        &stand.household,
        &stand.household.owner(),
        &stand.item,
        Some(stand.member_id),
        at,
    )
    .await;

    sync_tick(&mut stand, at).await;

    let html = message_html(&stand.bot, stand.bot_msg);
    assert!(
        html.contains("👤 Назначено: Вынести мусор · Маша"),
        "{html}"
    );
    assert_only_dismiss_button(&html);
    let rows = notice_closed_rows(&db.pool, stand.bot.chat, stand.bot_msg).await;
    assert_eq!(rows[0].1.as_deref(), Some("assigned"), "{rows:?}");
    assert!(rows[0].2.is_none(), "{rows:?}");
    let deletes = notice_expire_rows(&db.pool, stand.bot.chat, stand.bot_msg).await;
    assert_eq!(deletes.len(), 1, "{deletes:?}");
    assert_eq!(deletes[0].0, "pending", "{deletes:?}");
    assert_eq!(
        deletes[0].1,
        format!(
            "notice-expire:{}:{}:assigned:{}",
            stand.bot.chat,
            stand.bot_msg,
            at.timestamp_millis()
        ),
        "{deletes:?}"
    );
    assert_eq!(deletes[0].2, at + Duration::minutes(10), "{deletes:?}");
    // Копия Маши (исполнителя) остаётся с кнопками действий.
    let masha = message_html(&stand.member, stand.member_msg);
    assert!(masha.contains("✓ Выполнено"), "{masha}");
    assert!(!masha.contains("Назначено"), "{masha}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_returns_copy_when_assignee_removed_within_window() {
    let db = support::isolated_db().await;
    let mut stand = shared_task_stand(&db, "Вынести мусор").await;
    let at = stand.at;
    let item = update_assignee(
        &db,
        &stand.household,
        &stand.household.owner(),
        &stand.item,
        Some(stand.member_id),
        at,
    )
    .await;
    sync_tick(&mut stand, at).await;
    assert!(message_html(&stand.bot, stand.bot_msg).contains("👤 Назначено"));

    // Исполнителя сняли через 4 минуты.
    update_assignee(
        &db,
        &stand.household,
        &stand.household.owner(),
        &item,
        None,
        at + Duration::minutes(4),
    )
    .await;
    sync_tick(&mut stand, at + Duration::minutes(4)).await;

    let html = message_html(&stand.bot, stand.bot_msg);
    assert!(html.contains("⏰ Пора"), "{html}");
    assert!(
        buttons(&html).len() > 1,
        "action buttons must return: {html}"
    );
    let deletes = notice_expire_rows(&db.pool, stand.bot.chat, stand.bot_msg).await;
    assert_eq!(deletes[0].0, "skipped", "{deletes:?}");
    let rows = notice_closed_rows(&db.pool, stand.bot.chat, stand.bot_msg).await;
    assert_eq!(rows[0], (None, None, None), "{rows:?}");
    stand.bot.advance(Duration::minutes(6));
    stand.bot.deliver().await;
    assert!(!deleted(&stand.bot, stand.bot_msg));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_returns_petya_copy_when_reassigned_to_petya() {
    let db = support::isolated_db().await;
    let mut stand = shared_task_stand(&db, "Вынести мусор").await;
    let at = stand.at;
    let item = update_assignee(
        &db,
        &stand.household,
        &stand.household.owner(),
        &stand.item,
        Some(stand.member_id),
        at,
    )
    .await;
    sync_tick(&mut stand, at).await;
    assert!(message_html(&stand.bot, stand.bot_msg).contains("👤 Назначено"));

    // Переназначили на Петю (владельца, получателя копии).
    update_assignee(
        &db,
        &stand.household,
        &stand.household.owner(),
        &item,
        Some(stand.household.owner_id),
        at + Duration::minutes(4),
    )
    .await;
    sync_tick(&mut stand, at + Duration::minutes(4)).await;

    let html = message_html(&stand.bot, stand.bot_msg);
    assert!(html.contains("⏰ Пора"), "{html}");
    assert!(
        buttons(&html).len() > 1,
        "action buttons must return: {html}"
    );
    let deletes = notice_expire_rows(&db.pool, stand.bot.chat, stand.bot_msg).await;
    assert_eq!(deletes[0].0, "skipped", "{deletes:?}");
    let rows = notice_closed_rows(&db.pool, stand.bot.chat, stand.bot_msg).await;
    assert_eq!(rows[0], (None, None, None), "{rows:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_closes_petya_assignment_notice_after_reassign_to_masha() {
    let db = support::isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let member_id = add_member(&db.pool, household.household_id, "Маша", false).await;
    let at = sync_now();
    bot.freeze_at(at);
    let due = at + Duration::hours(3);
    let item = create_task(&db, &household, "Купить торт", Some(due), None, None).await;
    // Уведомление о назначении Пете (владельцу): назначает другой участник.
    let item = update_assignee(
        &db,
        &household,
        &Actor::user(member_id, UserRole::Member),
        &item,
        Some(household.owner_id),
        at,
    )
    .await;
    tick_and_deliver(&mut bot, &db.pool, at).await;
    let (msg, html) = bot.latest();
    assert!(html.contains("Вам назначили задачу"), "{html}");

    // Переназначили Маше.
    update_assignee(
        &db,
        &household,
        &household.owner(),
        &item,
        Some(member_id),
        at + Duration::minutes(1),
    )
    .await;
    scheduler(&db.pool)
        .tick(at + Duration::minutes(1))
        .await
        .expect("sync tick");
    bot.advance(Duration::minutes(1));
    bot.deliver().await;

    let html = message_html(&bot, msg);
    assert!(html.contains("👤 Назначено: Купить торт · Маша"), "{html}");
    assert_only_dismiss_button(&html);
    let rows = notice_closed_rows(&db.pool, bot.chat, msg).await;
    assert_eq!(rows[0].1.as_deref(), Some("assigned"), "{rows:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_keeps_assigned_closed_copy_after_completion() {
    let db = support::isolated_db().await;
    let mut stand = shared_task_stand(&db, "Вынести мусор").await;
    let at = stand.at;
    update_assignee(
        &db,
        &stand.household,
        &stand.household.owner(),
        &stand.item,
        Some(stand.member_id),
        at,
    )
    .await;
    sync_tick(&mut stand, at).await;
    assert!(message_html(&stand.bot, stand.bot_msg).contains("👤 Назначено"));

    // Маша выполняет задачу: копия Пети, закрытая по назначению, остаётся закрытой.
    complete_as(
        &db,
        &stand.household,
        stand.item.id,
        &Actor::user(stand.member_id, UserRole::Member),
        at + Duration::minutes(2),
    )
    .await;
    sync_tick(&mut stand, at + Duration::minutes(2)).await;

    let html = message_html(&stand.bot, stand.bot_msg);
    assert!(html.contains("👤 Назначено"), "{html}");
    assert!(!html.contains("✅ Выполнено"), "{html}");
    let rows = notice_closed_rows(&db.pool, stand.bot.chat, stand.bot_msg).await;
    assert_eq!(rows[0].1.as_deref(), Some("assigned"), "{rows:?}");
    assert_eq!(edit_count(&db.pool, stand.bot.chat).await, 1);
    let deletes = notice_expire_rows(&db.pool, stand.bot.chat, stand.bot_msg).await;
    assert_eq!(deletes.len(), 1, "{deletes:?}");
    assert_eq!(deletes[0].2, at + Duration::minutes(10), "{deletes:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_prefers_completion_over_assignment_in_one_tick() {
    let db = support::isolated_db().await;
    let mut stand = shared_task_stand(&db, "Вынести мусор").await;
    let at = stand.at;
    // Назначение и выполнение до одного тика: побеждает выполнение.
    update_assignee(
        &db,
        &stand.household,
        &stand.household.owner(),
        &stand.item,
        Some(stand.member_id),
        at,
    )
    .await;
    complete_as(
        &db,
        &stand.household,
        stand.item.id,
        &Actor::user(stand.member_id, UserRole::Member),
        at + Duration::minutes(1),
    )
    .await;

    sync_tick(&mut stand, at + Duration::minutes(1)).await;

    let html = message_html(&stand.bot, stand.bot_msg);
    assert!(
        html.contains("✅ Выполнено: Вынести мусор · Маша"),
        "{html}"
    );
    assert_only_dismiss_button(&html);
    let rows = notice_closed_rows(&db.pool, stand.bot.chat, stand.bot_msg).await;
    assert_eq!(rows[0].1.as_deref(), Some("completed"), "{rows:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_closes_assigned_copy_without_name_when_assignee_deleted() {
    let db = support::isolated_db().await;
    let mut stand = shared_task_stand(&db, "Вынести мусор").await;
    let at = stand.at;
    update_assignee(
        &db,
        &stand.household,
        &stand.household.owner(),
        &stand.item,
        Some(stand.member_id),
        at,
    )
    .await;
    // Исполнитель удалён: имя показать нельзя, но копия всё равно закрывается.
    sqlx::query("UPDATE users SET deleted_at = $2 WHERE id = $1")
        .bind(stand.member_id)
        .bind(at)
        .execute(&db.pool)
        .await
        .expect("delete assignee");

    sync_tick(&mut stand, at).await;

    let html = message_html(&stand.bot, stand.bot_msg);
    assert!(html.contains("👤 Назначено: Вынести мусор"), "{html}");
    assert!(!html.contains("· Маша"), "{html}");
    assert_only_dismiss_button(&html);
    let rows = notice_closed_rows(&db.pool, stand.bot.chat, stand.bot_msg).await;
    assert_eq!(rows[0].1.as_deref(), Some("assigned"), "{rows:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_delete_deadline_is_completion_time_plus_window_not_tick_time() {
    let db = support::isolated_db().await;
    let mut stand = shared_task_stand(&db, "Вынести мусор").await;
    let at = stand.at;
    let done_at = at + Duration::minutes(1);
    complete_as(
        &db,
        &stand.household,
        stand.item.id,
        &stand.household.owner(),
        done_at,
    )
    .await;

    // Тик шага через 3 минуты после выполнения: срок удаления всё равно
    // отсчитывается от момента выполнения, а не от тика.
    sync_tick(&mut stand, done_at + Duration::minutes(3)).await;

    let deletes = notice_expire_rows(&db.pool, stand.bot.chat, stand.bot_msg).await;
    assert_eq!(deletes.len(), 1, "{deletes:?}");
    assert_eq!(
        deletes[0].2,
        done_at + Duration::minutes(10),
        "delete must be scheduled from completion time, not tick time: {deletes:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_does_not_return_completed_copy_after_return_window() {
    let db = support::isolated_db().await;
    let mut stand = shared_task_stand(&db, "Вынести мусор").await;
    let at = stand.at;
    let done_at = at + Duration::minutes(1);
    complete_as(
        &db,
        &stand.household,
        stand.item.id,
        &stand.household.owner(),
        done_at,
    )
    .await;
    sync_tick(&mut stand, done_at).await;
    assert!(message_html(&stand.bot, stand.bot_msg).contains("✅ Выполнено"));
    let event_id = latest_completion_event(&db.pool, stand.item.id)
        .await
        .expect("completion event");

    // Выполнение снято позже окна возврата: тик через 11 минут после выполнения.
    let late = done_at + Duration::minutes(11);
    let mut conn = db.pool.acquire().await.expect("conn");
    items::reopen(
        &mut conn,
        stand.household.household_id,
        &stand.household.owner(),
        stand.item.id,
        None,
        late,
    )
    .await
    .expect("reopen");
    drop(conn);
    scheduler(&db.pool).tick(late).await.expect("late tick");

    // Копия не возвращается: новой правки нет, отметка закрытия на месте,
    // отложенное удаление не отменено.
    assert_eq!(edit_count(&db.pool, stand.bot.chat).await, 1);
    let rows = notice_closed_rows(&db.pool, stand.bot.chat, stand.bot_msg).await;
    assert!(rows[0].0.is_some(), "{rows:?}");
    assert_eq!(rows[0].1.as_deref(), Some("completed"), "{rows:?}");
    assert_eq!(rows[0].2, Some(event_id), "{rows:?}");
    let deletes = notice_expire_rows(&db.pool, stand.bot.chat, stand.bot_msg).await;
    assert_eq!(deletes.len(), 1, "{deletes:?}");
    assert_ne!(deletes[0].0, "skipped", "{deletes:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_does_not_return_assigned_copy_after_return_window() {
    let db = support::isolated_db().await;
    let mut stand = shared_task_stand(&db, "Вынести мусор").await;
    let at = stand.at;
    let item = update_assignee(
        &db,
        &stand.household,
        &stand.household.owner(),
        &stand.item,
        Some(stand.member_id),
        at,
    )
    .await;
    sync_tick(&mut stand, at).await;
    assert!(message_html(&stand.bot, stand.bot_msg).contains("👤 Назначено"));

    // Исполнитель снят позже окна возврата: тик через 11 минут после закрытия.
    let late = at + Duration::minutes(11);
    update_assignee(
        &db,
        &stand.household,
        &stand.household.owner(),
        &item,
        None,
        late,
    )
    .await;
    scheduler(&db.pool).tick(late).await.expect("late tick");

    assert_eq!(edit_count(&db.pool, stand.bot.chat).await, 1);
    let rows = notice_closed_rows(&db.pool, stand.bot.chat, stand.bot_msg).await;
    assert!(rows[0].0.is_some(), "{rows:?}");
    assert_eq!(rows[0].1.as_deref(), Some("assigned"), "{rows:?}");
    let deletes = notice_expire_rows(&db.pool, stand.bot.chat, stand.bot_msg).await;
    assert_eq!(deletes.len(), 1, "{deletes:?}");
    assert_ne!(deletes[0].0, "skipped", "{deletes:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_does_not_close_reminder_copy_of_a_different_occurrence() {
    let db = support::isolated_db().await;
    let at = sync_now();
    let due_a = at + Duration::minutes(15);
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    bot.freeze_at(at);
    let item = create_task(
        &db,
        &household,
        "Перенесённая",
        Some(due_a),
        Some(household.owner_id),
        None,
    )
    .await;
    tick_and_deliver(&mut bot, &db.pool, at).await;
    let (msg, html) = bot.latest();
    assert!(html.contains("Перенесённая"), "{html}");

    // Срок перенесли на B и задачу выполнили: выполнение относится к вхождению B.
    let due_b = at + Duration::hours(2);
    let item = move_due(
        &db,
        &household,
        &household.owner(),
        &item,
        due_b,
        at + Duration::minutes(1),
    )
    .await;
    complete_as(
        &db,
        &household,
        item.id,
        &household.owner(),
        at + Duration::minutes(2),
    )
    .await;
    scheduler(&db.pool)
        .tick(at + Duration::minutes(2))
        .await
        .expect("sync tick");
    bot.advance(Duration::minutes(2));
    bot.deliver().await;

    // Копия о сроке A не закрыта выполнением вхождения B.
    let rows = notice_closed_rows(&db.pool, bot.chat, msg).await;
    assert_ne!(rows[0].1.as_deref(), Some("completed"), "{rows:?}");
    let html = message_html(&bot, msg);
    assert!(!html.contains("✅ Выполнено"), "{html}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_does_not_close_snooze_copy_of_a_different_occurrence() {
    let db = support::isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let at = sync_now();
    bot.freeze_at(at);
    let due_a = at + Duration::minutes(15);
    let item = create_task(
        &db,
        &household,
        "Отложенная",
        Some(due_a),
        Some(household.owner_id),
        None,
    )
    .await;
    tick_and_deliver(&mut bot, &db.pool, at).await;
    let (msg, html) = bot.latest();
    bot.press_data(&button_of(&html, "на час"), msg).await;
    let snooze_at = at + Duration::hours(1);
    bot.advance(Duration::hours(1));
    scheduler(&db.pool)
        .tick(snooze_at)
        .await
        .expect("snooze tick");
    bot.deliver().await;
    let (snooze_msg, snooze_html) = bot.latest();
    assert!(snooze_html.contains("⏰ Пора"), "{snooze_html}");

    // Срок перенесли на B и задачу выполнили: выполнение относится к вхождению B,
    // а origin_key доставки привязан к сроку A.
    let due_b = snooze_at + Duration::hours(2);
    let item = move_due(
        &db,
        &household,
        &household.owner(),
        &item,
        due_b,
        snooze_at + Duration::minutes(1),
    )
    .await;
    complete_as(
        &db,
        &household,
        item.id,
        &household.owner(),
        snooze_at + Duration::minutes(2),
    )
    .await;
    scheduler(&db.pool)
        .tick(snooze_at + Duration::minutes(2))
        .await
        .expect("sync tick");
    bot.advance(Duration::minutes(2));
    bot.deliver().await;

    let rows = notice_closed_rows(&db.pool, bot.chat, snooze_msg).await;
    assert_ne!(rows[0].1.as_deref(), Some("completed"), "{rows:?}");
    let html = message_html(&bot, snooze_msg);
    assert!(!html.contains("✅ Выполнено"), "{html}");
}

// ---------------------------------------------------------------------------
// Шаг синхронизации сводок `sync_digests` (0019, design решение 6, пункт 5.1).
// ---------------------------------------------------------------------------

/// Стенд: Петя (не владелец) и Маша с включённой сводкой, общая вседневная
/// задача и задача Пети на день сводки; сводки доставлены обоим в 08:30.
struct DigestSyncStand {
    petya: Harness,
    masha: Harness,
    household: TestHousehold,
    petya_id: Uuid,
    masha_id: Uuid,
    shared: Item,
    petya_msg: i64,
    masha_msg: i64,
    day: NaiveDate,
    at: DateTime<Utc>,
}

async fn digest_sync_stand(db: &TestDb) -> DigestSyncStand {
    digest_sync_stand_scoped(db, DigestScope::All).await
}

async fn digest_sync_stand_scoped(db: &TestDb, petya_scope: DigestScope) -> DigestSyncStand {
    let at = local_utc(chrono_tz::Europe::Moscow, digest_day(), 8, 30);
    digest_sync_stand_at(db, petya_scope, at).await
}

/// Стенд на заданный момент `at` (локальные 08:30 Москвы): день сводки —
/// локальная дата этого момента. Момент в будущем относительно реальных
/// отметок создания нужен сценариям отмены: событие выполнения должно быть
/// новее события создания задачи.
async fn digest_sync_stand_at(
    db: &TestDb,
    petya_scope: DigestScope,
    at: DateTime<Utc>,
) -> DigestSyncStand {
    let household = household_without_digest(db).await;
    set_household_timezone(&db.pool, household.household_id, "Europe/Moscow").await;
    let petya_id = add_member(&db.pool, household.household_id, "Петя", true).await;
    let masha_id = add_member(&db.pool, household.household_id, "Маша", true).await;
    set_digest_scope(&db.pool, household.household_id, petya_id, petya_scope).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = at.with_timezone(&moscow).date_naive();
    let petya_chat = chat_id(&db.pool, petya_id).await;
    let masha_chat = chat_id(&db.pool, masha_id).await;
    let mut petya = Harness::new(&db.pool, petya_chat).named("Петя");
    let mut masha = Harness::sharing(&db.pool, masha_chat, petya.fake.clone()).named("Маша");
    petya.freeze_at(at);
    masha.freeze_at(at);
    let shared = create_all_day_task(db, &household, "Вынести мусор", day, None).await;
    create_task_without_assignment_notice(
        db,
        &household,
        "Забрать посылку",
        Some(local_utc(moscow, day, 18, 0)),
        petya_id,
        None,
    )
    .await;
    let stats = scheduler(&db.pool).tick(at).await.expect("digest tick");
    assert_eq!(stats.delivered, 2, "{stats:?}");
    petya.deliver().await;
    masha.deliver().await;
    let (petya_msg, petya_html) = petya.latest();
    let (masha_msg, masha_html) = masha.latest();
    assert!(petya_html.contains("Вынести мусор"), "{petya_html}");
    assert!(masha_html.contains("Вынести мусор"), "{masha_html}");
    DigestSyncStand {
        petya,
        masha,
        household,
        petya_id,
        masha_id,
        shared,
        petya_msg,
        masha_msg,
        day,
        at,
    }
}

/// Тик шага синхронизации сводок; исходящие ещё не доставлены, чтобы можно
/// было проверить payload правок до доставки.
async fn digest_sync_tick(stand: &mut DigestSyncStand, at: DateTime<Utc>) -> TickStats {
    let stats = scheduler(&stand.petya.pool)
        .tick(at)
        .await
        .expect("sync digest tick");
    if at > stand.petya.now {
        stand.petya.advance(at - stand.petya.now);
    }
    if at > stand.masha.now {
        stand.masha.advance(at - stand.masha.now);
    }
    stats
}

/// Payload недоставленных строк правки чата.
async fn pending_edit_payloads(pool: &PgPool, chat: i64) -> Vec<serde_json::Value> {
    sqlx::query_scalar(
        "SELECT payload FROM outbox
         WHERE chat_id = $1 AND kind = 'edit' AND status = 'pending' ORDER BY seq",
    )
    .bind(chat)
    .fetch_all(pool)
    .await
    .expect("pending edit payloads")
}

/// Число строк outbox чата (все виды).
async fn outbox_count(pool: &PgPool, chat: i64) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM outbox WHERE chat_id = $1")
        .bind(chat)
        .fetch_one(pool)
        .await
        .expect("outbox count")
}

/// `digest_hash` последней сводки получателя равен отпечатку её текущей отрисовки.
async fn assert_digest_hash_matches_render(pool: &PgPool, user_id: Uuid) {
    let delivery_id = digest_delivery_id(pool, user_id).await;
    let mut conn = pool.acquire().await.expect("conn");
    let view = tasklerq::jobs::digest::digest_view(&mut conn, delivery_id)
        .await
        .expect("digest view");
    drop(conn);
    let ui = tasklerq::telegram::bot::notify::digest_ui(&view);
    let hash: String =
        sqlx::query_scalar("SELECT digest_hash FROM notification_deliveries WHERE id = $1")
            .bind(delivery_id)
            .fetch_one(pool)
            .await
            .expect("digest hash");
    assert_eq!(hash, tasklerq::telegram::bot::notify::ui_fingerprint(&ui));
}

/// Поставленная, но ещё не доставленная сводка получателя: запись `sent`,
/// строка outbox без `message_id` (воркер ещё не отправил сообщение).
async fn insert_queued_digest(
    pool: &PgPool,
    household_id: Uuid,
    user_id: Uuid,
    chat: i64,
    key: &str,
    at: DateTime<Utc>,
) -> Uuid {
    let seq = insert_outbox(pool, chat, &format!("queued-digest:{user_id}")).await;
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO notification_deliveries (
             id, household_id, user_id, item_id, occurrence_key, kind,
             status, scheduled_at, outbox_seq, created_at, updated_at
         )
         VALUES ($1, $2, $3, NULL, $4, 'digest', 'sent', $5, $6, $5, $5)",
    )
    .bind(id)
    .bind(household_id)
    .bind(user_id)
    .bind(key)
    .bind(at)
    .bind(seq)
    .execute(pool)
    .await
    .expect("queued digest");
    id
}

/// Маша выполнила задачу из своей сводки: у Пети правка «✅ … · Маша» без
/// кнопки и «Выполнено 1 из N», у Маши повторной правки нет; правка — Edit с
/// `resend_if_gone: false`, `digest_hash` равен отпечатку новой отрисовки.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_digests_edits_petya_when_masha_completes_from_her_digest() {
    let db = support::isolated_db().await;
    let mut stand = digest_sync_stand(&db).await;
    let done_at = stand.at + Duration::minutes(1);

    // Маша выполняет общую задачу касанием строки своей сводки.
    stand.masha.advance(Duration::minutes(1));
    let masha_html = message_html(&stand.masha, stand.masha_msg);
    stand
        .masha
        .press_data(&button_of(&masha_html, "Вынести мусор"), stand.masha_msg)
        .await;
    assert!(message_html(&stand.masha, stand.masha_msg).contains("✅ Вынести мусор"));

    let stats = digest_sync_tick(&mut stand, done_at).await;
    assert_eq!(stats.synced, 1, "{stats:?}");

    // Правка Пети — Edit с resend_if_gone: false, адресованная его сводке.
    let edits = pending_edit_payloads(&db.pool, stand.petya.chat).await;
    assert_eq!(edits.len(), 1, "{edits:?}");
    assert_eq!(edits[0]["message_id"], json!(stand.petya_msg));
    assert_eq!(edits[0]["resend_if_gone"], json!(false));
    // У Маши повторной правки нет: её сводку уже перерисовало касание.
    assert!(
        pending_edit_payloads(&db.pool, stand.masha.chat)
            .await
            .is_empty()
    );

    stand.petya.deliver().await;
    stand.masha.deliver().await;

    let html = message_html(&stand.petya, stand.petya_msg);
    assert!(html.contains("✅ Вынести мусор · Маша"), "{html}");
    assert!(html.contains("Выполнено 1 из 2"), "{html}");
    // Строка выполненной чужой задачи — без кнопки.
    let labels: Vec<String> = buttons(&html).into_iter().map(|(label, _)| label).collect();
    assert!(
        !labels.iter().any(|label| label.contains("Вынести мусор")),
        "{labels:?}"
    );
    assert_digest_hash_matches_render(&db.pool, stand.petya_id).await;
    assert_eq!(edit_count(&db.pool, stand.masha.chat).await, 1);
}

/// Выполнение через HTTP API (без участника) — правка сводки получателя.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_digests_edits_after_completion_via_http_api() {
    let db = support::isolated_db().await;
    let mut stand = digest_sync_stand(&db).await;
    let token = support::issue_token(
        &db.pool,
        stand.household.household_id,
        &["ha:read", "ha:write"],
    )
    .await;
    let app = support::app(&db.pool);
    let (status, body) = support::request(
        &app,
        Method::POST,
        &format!("/api/v1/items/{}/complete", stand.shared.id),
        Some(&token),
        Some(json!({"version": stand.shared.version})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let at = stand.at + Duration::minutes(1);
    digest_sync_tick(&mut stand, at).await;
    stand.petya.deliver().await;
    stand.masha.deliver().await;

    let html = message_html(&stand.petya, stand.petya_msg);
    assert!(html.contains("✅ Вынести мусор"), "{html}");
    assert!(
        !html.contains("· Маша"),
        "API completion has no actor name: {html}"
    );
    assert_digest_hash_matches_render(&db.pool, stand.petya_id).await;
}

/// Выполнение через Home Assistant — правка сводки получателя.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_digests_edits_after_completion_via_home_assistant() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    set_household_timezone(&db.pool, household.household_id, "Europe/Moscow").await;
    let petya_id = add_member(&db.pool, household.household_id, "Петя", true).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = digest_day();
    let at = local_utc(moscow, day, 8, 30);
    let petya_chat = chat_id(&db.pool, petya_id).await;
    let mut petya = Harness::new(&db.pool, petya_chat).named("Петя");
    petya.freeze_at(at);
    // Задача создаётся интеграцией HA: без участника-автора.
    let token =
        support::issue_token(&db.pool, household.household_id, &["ha:read", "ha:write"]).await;
    let app = support::app(&db.pool);
    let (status, item) = support::request(
        &app,
        Method::POST,
        &format!("/api/v1/lists/{}/items", household.tasks_list_id),
        Some(&token),
        Some(json!({
            "title": "Проверить HA",
            "source": "ha_todo",
            "due_date": day.format("%Y-%m-%d").to_string(),
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{item}");
    let item_id = item["id"].as_str().expect("item id").to_owned();
    let version = item["version"].as_i64().expect("version");

    let stats = scheduler(&db.pool).tick(at).await.expect("digest tick");
    assert_eq!(stats.delivered, 1, "{stats:?}");
    petya.deliver().await;
    let (msg, html) = petya.latest();
    assert!(html.contains("Проверить HA"), "{html}");

    let (status, completed) = support::request(
        &app,
        Method::POST,
        &format!("/api/v1/items/{item_id}/complete"),
        Some(&token),
        Some(json!({"version": version})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{completed}");

    scheduler(&db.pool)
        .tick(at + Duration::minutes(1))
        .await
        .expect("sync tick");
    petya.advance(Duration::minutes(1));
    petya.deliver().await;

    let html = message_html(&petya, msg);
    assert!(html.contains("✅ Проверить HA"), "{html}");
    assert!(
        !html.contains("Проверить HA ·"),
        "HA completion has no actor name: {html}"
    );
}

/// Отмена Машей в окне отмены: строка у Пети снова кнопкой.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_digests_returns_petya_button_when_masha_undoes() {
    let db = support::isolated_db().await;
    // Момент в будущем: выполнение должно быть новее создания задачи, иначе
    // проверка «последнее отменяемое событие» выберет событие создания.
    let moscow = chrono_tz::Europe::Moscow;
    let mut day = Utc::now().with_timezone(&moscow).date_naive();
    let mut at = local_utc(moscow, day, 8, 30);
    if at <= Utc::now() {
        day += Duration::days(1);
        at = local_utc(moscow, day, 8, 30);
    }
    let mut stand = digest_sync_stand_at(&db, DigestScope::All, at).await;

    // Маша выполняет касанием, шаг правит сводку Пети.
    stand.masha.advance(Duration::minutes(1));
    let masha_html = message_html(&stand.masha, stand.masha_msg);
    stand
        .masha
        .press_data(&button_of(&masha_html, "Вынести мусор"), stand.masha_msg)
        .await;
    let at = stand.at + Duration::minutes(1);
    digest_sync_tick(&mut stand, at).await;
    stand.petya.deliver().await;
    stand.masha.deliver().await;
    assert!(message_html(&stand.petya, stand.petya_msg).contains("✅ Вынести мусор · Маша"));

    // Маша отменяет касанием выполненной строки в окне отмены.
    stand.masha.advance(Duration::minutes(1));
    let done_html = message_html(&stand.masha, stand.masha_msg);
    stand
        .masha
        .press_data(&button_of(&done_html, "Вынести мусор"), stand.masha_msg)
        .await;
    let at = stand.at + Duration::minutes(2);
    digest_sync_tick(&mut stand, at).await;
    stand.petya.deliver().await;
    stand.masha.deliver().await;

    let html = message_html(&stand.petya, stand.petya_msg);
    assert!(html.contains("☐ Вынести мусор · весь день"), "{html}");
    let labels: Vec<String> = buttons(&html).into_iter().map(|(label, _)| label).collect();
    assert!(
        labels.iter().any(|label| label.contains("Вынести мусор")),
        "open task button must return: {labels:?}"
    );
}

/// Переименование задачи из снимка — правка сводки.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_digests_edits_after_rename() {
    let db = support::isolated_db().await;
    let mut stand = digest_sync_stand(&db).await;
    let now = stand.at + Duration::minutes(1);
    let mut conn = db.pool.acquire().await.expect("conn");
    items::update(
        &mut conn,
        stand.household.household_id,
        &stand.household.owner(),
        stand.shared.id,
        stand.shared.version,
        ItemPatch {
            title: Some("Вынести мусор срочно".to_owned()),
            ..ItemPatch::default()
        },
        "Europe/Moscow",
        now,
    )
    .await
    .expect("rename");
    drop(conn);

    digest_sync_tick(&mut stand, now).await;
    stand.petya.deliver().await;
    stand.masha.deliver().await;

    let html = message_html(&stand.petya, stand.petya_msg);
    assert!(html.contains("Вынести мусор срочно"), "{html}");
    assert_digest_hash_matches_render(&db.pool, stand.petya_id).await;
}

/// Назначение общей задачи Маше в составе «все в семье» — правка с «👤 Маша».
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_digests_edits_after_assignment_in_all_scope() {
    let db = support::isolated_db().await;
    let mut stand = digest_sync_stand(&db).await;
    let now = stand.at + Duration::minutes(1);
    update_assignee(
        &db,
        &stand.household,
        &stand.household.owner(),
        &stand.shared,
        Some(stand.masha_id),
        now,
    )
    .await;

    digest_sync_tick(&mut stand, now).await;
    stand.petya.deliver().await;
    stand.masha.deliver().await;

    let html = message_html(&stand.petya, stand.petya_msg);
    assert!(html.contains("Вынести мусор"), "{html}");
    assert!(html.contains("👤 Маша"), "{html}");
    assert_digest_hash_matches_render(&db.pool, stand.petya_id).await;
}

/// Удаление задачи из снимка — правка с отметкой «удалена», без кнопки.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_digests_edits_after_deletion() {
    let db = support::isolated_db().await;
    let mut stand = digest_sync_stand(&db).await;
    let now = stand.at + Duration::minutes(1);
    let mut conn = db.pool.acquire().await.expect("conn");
    items::soft_delete(
        &mut conn,
        stand.household.household_id,
        &stand.household.owner(),
        stand.shared.id,
        now,
    )
    .await
    .expect("delete");
    drop(conn);

    digest_sync_tick(&mut stand, now).await;
    stand.petya.deliver().await;
    stand.masha.deliver().await;

    let html = message_html(&stand.petya, stand.petya_msg);
    assert!(html.contains("Вынести мусор — удалена"), "{html}");
    let labels: Vec<String> = buttons(&html).into_iter().map(|(label, _)| label).collect();
    assert!(
        !labels.iter().any(|label| label.contains("Вынести мусор")),
        "{labels:?}"
    );
    assert_digest_hash_matches_render(&db.pool, stand.petya_id).await;
}

/// Архивирование списка задачи из снимка — правка со строкой «— в архиве».
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_digests_edits_after_list_archive() {
    let db = support::isolated_db().await;
    let mut stand = digest_sync_stand(&db).await;
    let now = stand.at + Duration::minutes(1);
    // Список по умолчанию архивировать нельзя (check-ограничение): снимаем
    // признак, как другие тесты архивирования.
    sqlx::query("UPDATE lists SET is_default = false, is_archived = true WHERE id = $1")
        .bind(stand.household.tasks_list_id)
        .execute(&db.pool)
        .await
        .expect("archive list");

    digest_sync_tick(&mut stand, now).await;
    stand.petya.deliver().await;
    stand.masha.deliver().await;

    let html = message_html(&stand.petya, stand.petya_msg);
    assert!(html.contains("Вынести мусор — в архиве"), "{html}");
    let labels: Vec<String> = buttons(&html).into_iter().map(|(label, _)| label).collect();
    assert!(
        !labels.iter().any(|label| label.contains("Вынести мусор")),
        "{labels:?}"
    );
    assert_digest_hash_matches_render(&db.pool, stand.petya_id).await;
}

/// В «только мои» назначенная Маше общая задача исчезает из сводки Пети,
/// после снятия исполнителя — возвращается с «👥».
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_digests_hides_and_returns_shared_task_in_mine_scope() {
    let db = support::isolated_db().await;
    let mut stand = digest_sync_stand_scoped(&db, DigestScope::Mine).await;
    let now = stand.at + Duration::minutes(1);
    assert!(message_html(&stand.petya, stand.petya_msg).contains("👥"));

    // Общую задачу назначили Маше: строка исчезает, итоги пересчитаны.
    let shared = update_assignee(
        &db,
        &stand.household,
        &stand.household.owner(),
        &stand.shared,
        Some(stand.masha_id),
        now,
    )
    .await;
    digest_sync_tick(&mut stand, now).await;
    stand.petya.deliver().await;
    stand.masha.deliver().await;
    let html = message_html(&stand.petya, stand.petya_msg);
    assert!(!html.contains("Вынести мусор"), "{html}");
    assert!(html.contains("Забрать посылку"), "{html}");
    assert!(html.contains("1 задача"), "{html}");
    assert_digest_hash_matches_render(&db.pool, stand.petya_id).await;

    // Исполнителя сняли: строка возвращается с «👥».
    let later = now + Duration::minutes(1);
    update_assignee(
        &db,
        &stand.household,
        &stand.household.owner(),
        &shared,
        None,
        later,
    )
    .await;
    digest_sync_tick(&mut stand, later).await;
    stand.petya.deliver().await;
    stand.masha.deliver().await;
    let html = message_html(&stand.petya, stand.petya_msg);
    assert!(html.contains("☐ Вынести мусор"), "{html}");
    assert!(html.contains("👥"), "{html}");
    assert!(html.contains("2 задачи"), "{html}");
    assert_digest_hash_matches_render(&db.pool, stand.petya_id).await;
}

/// Повторный тик без изменений и тик после полуночи не добавляют строк outbox.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_digests_repeat_and_midnight_ticks_add_no_outbox_rows() {
    let db = support::isolated_db().await;
    let mut stand = digest_sync_stand(&db).await;
    let before = outbox_count(&db.pool, stand.petya.chat).await;

    // Повторный тик без изменений.
    let at = stand.at + Duration::minutes(1);
    digest_sync_tick(&mut stand, at).await;
    stand.petya.deliver().await;
    stand.masha.deliver().await;
    assert_eq!(outbox_count(&db.pool, stand.petya.chat).await, before);
    assert_eq!(edit_count(&db.pool, stand.petya.chat).await, 0);

    // Тик после полуночи: отрисовка не зависит от «сейчас», правок нет.
    let midnight = local_utc(
        chrono_tz::Europe::Moscow,
        stand.day + Duration::days(1),
        0,
        0,
    );
    digest_sync_tick(&mut stand, midnight).await;
    stand.petya.deliver().await;
    stand.masha.deliver().await;
    assert_eq!(outbox_count(&db.pool, stand.petya.chat).await, before);
    assert_eq!(edit_count(&db.pool, stand.petya.chat).await, 0);
}

/// Прежняя сводка при поставленной, но не доставленной новой не трогается.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_digests_leaves_previous_alone_when_newer_is_queued() {
    let db = support::isolated_db().await;
    let mut stand = digest_sync_stand(&db).await;
    let done_at = stand.at + Duration::minutes(1);
    // Новая сводка Пети поставлена в outbox, но воркер ещё не доставил её.
    insert_queued_digest(
        &db.pool,
        stand.household.household_id,
        stand.petya_id,
        stand.petya.chat,
        &format!("digest:{}#manual:{}", stand.day, done_at.timestamp_millis()),
        done_at,
    )
    .await;
    complete_as(
        &db,
        &stand.household,
        stand.shared.id,
        &Actor::user(stand.masha_id, UserRole::Member),
        done_at,
    )
    .await;

    digest_sync_tick(&mut stand, done_at).await;
    stand.petya.deliver().await;
    stand.masha.deliver().await;

    assert_eq!(edit_count(&db.pool, stand.petya.chat).await, 0);
    assert!(message_html(&stand.petya, stand.petya_msg).contains("☐ Вынести мусор"));
}

/// Убранная сводка (`dismissed_at`) не правится.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_digests_leaves_dismissed_digest_alone() {
    let db = support::isolated_db().await;
    let mut stand = digest_sync_stand(&db).await;
    let petya_html = message_html(&stand.petya, stand.petya_msg);
    stand
        .petya
        .press_data(&button_of(&petya_html, "Убрать"), stand.petya_msg)
        .await;
    complete_as(
        &db,
        &stand.household,
        stand.shared.id,
        &Actor::user(stand.masha_id, UserRole::Member),
        stand.at,
    )
    .await;

    let at = stand.at;
    digest_sync_tick(&mut stand, at).await;
    stand.petya.deliver().await;
    stand.masha.deliver().await;

    assert_eq!(edit_count(&db.pool, stand.petya.chat).await, 0);
}

/// Сводка старше 47 часов не правится.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_digests_leaves_digest_older_than_47_hours_alone() {
    let db = support::isolated_db().await;
    let mut stand = digest_sync_stand(&db).await;
    let done_at = stand.at + Duration::minutes(1);
    // Сообщение сводки Пети отправлено 48 часов назад.
    sqlx::query(
        "UPDATE outbox SET sent_at = $2
         WHERE seq = (SELECT outbox_seq FROM notification_deliveries
                      WHERE user_id = $1 AND kind = 'digest')",
    )
    .bind(stand.petya_id)
    .bind(stand.at - Duration::hours(48))
    .execute(&db.pool)
    .await
    .expect("age digest message");
    complete_as(
        &db,
        &stand.household,
        stand.shared.id,
        &Actor::user(stand.masha_id, UserRole::Member),
        done_at,
    )
    .await;

    digest_sync_tick(&mut stand, done_at).await;
    stand.petya.deliver().await;
    stand.masha.deliver().await;

    assert_eq!(edit_count(&db.pool, stand.petya.chat).await, 0);
}

/// Сдвинуть отправку сводки получателя в прошлое: `sent_at` строки outbox и
/// `updated_at` записи доставки (запись получает его при постановке в outbox).
///
/// `updated_at` можно задать отдельно от `sent_at`: задержка outbox между
/// постановкой и фактической отправкой ничем не ограничена, поэтому `sent_at`
/// внутри окна может сочетаться со старым `updated_at`. По умолчанию они равны.
async fn age_digest_sent_at(
    pool: &PgPool,
    user_id: Uuid,
    sent_at: DateTime<Utc>,
    updated_at: Option<DateTime<Utc>>,
) {
    sqlx::query(
        "UPDATE outbox SET sent_at = $2
         WHERE seq = (SELECT outbox_seq FROM notification_deliveries
                      WHERE user_id = $1 AND kind = 'digest')",
    )
    .bind(user_id)
    .bind(sent_at)
    .execute(pool)
    .await
    .expect("age digest message");
    sqlx::query(
        "UPDATE notification_deliveries SET updated_at = $2
         WHERE user_id = $1 AND kind = 'digest'",
    )
    .bind(user_id)
    .bind(updated_at.unwrap_or(sent_at))
    .execute(pool)
    .await
    .expect("age digest delivery");
}

/// Граница 47 часов: сводка, доставленная 46 ч 59 мин назад, ещё правится.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_digests_edits_digest_delivered_just_under_47_hours_ago() {
    let db = support::isolated_db().await;
    let mut stand = digest_sync_stand(&db).await;
    let done_at = stand.at + Duration::minutes(1);
    age_digest_sent_at(
        &db.pool,
        stand.petya_id,
        done_at - Duration::hours(47) + Duration::minutes(1),
        None,
    )
    .await;
    complete_as(
        &db,
        &stand.household,
        stand.shared.id,
        &Actor::user(stand.masha_id, UserRole::Member),
        done_at,
    )
    .await;

    digest_sync_tick(&mut stand, done_at).await;
    stand.petya.deliver().await;
    stand.masha.deliver().await;

    assert_eq!(edit_count(&db.pool, stand.petya.chat).await, 1);
    let html = message_html(&stand.petya, stand.petya_msg);
    assert!(html.contains("✅ Вынести мусор · Маша"), "{html}");
    assert_digest_hash_matches_render(&db.pool, stand.petya_id).await;
}

/// Граница 47 часов: сводка, доставленная 47 ч 01 мин назад, уже не правится.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_digests_leaves_digest_delivered_just_over_47_hours_ago() {
    let db = support::isolated_db().await;
    let mut stand = digest_sync_stand(&db).await;
    let done_at = stand.at + Duration::minutes(1);
    age_digest_sent_at(
        &db.pool,
        stand.petya_id,
        done_at - Duration::hours(47) - Duration::minutes(1),
        None,
    )
    .await;
    complete_as(
        &db,
        &stand.household,
        stand.shared.id,
        &Actor::user(stand.masha_id, UserRole::Member),
        done_at,
    )
    .await;

    digest_sync_tick(&mut stand, done_at).await;
    stand.petya.deliver().await;
    stand.masha.deliver().await;

    assert_eq!(edit_count(&db.pool, stand.petya.chat).await, 0);
    assert!(message_html(&stand.petya, stand.petya_msg).contains("☐ Вынести мусор"));
}

/// Граница 47 часов включительна: сводка, доставленная ровно 47 часов назад,
/// ещё правится (`o.sent_at >= $1`, мутация на `>` ломает тест).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_digests_edits_digest_delivered_exactly_47_hours_ago() {
    let db = support::isolated_db().await;
    let mut stand = digest_sync_stand(&db).await;
    let done_at = stand.at + Duration::minutes(1);
    age_digest_sent_at(
        &db.pool,
        stand.petya_id,
        done_at - Duration::hours(47),
        None,
    )
    .await;
    complete_as(
        &db,
        &stand.household,
        stand.shared.id,
        &Actor::user(stand.masha_id, UserRole::Member),
        done_at,
    )
    .await;

    digest_sync_tick(&mut stand, done_at).await;
    stand.petya.deliver().await;
    stand.masha.deliver().await;

    assert_eq!(edit_count(&db.pool, stand.petya.chat).await, 1);
    let html = message_html(&stand.petya, stand.petya_msg);
    assert!(html.contains("✅ Вынести мусор · Маша"), "{html}");
    assert_digest_hash_matches_render(&db.pool, stand.petya_id).await;
}

/// Задержка outbox не ограничена: сводка, поставленная 76 часов назад, но
/// фактически отправленная 46 часов назад, ещё правится. Прежний предфильтр
/// по `updated_at` (`done_at - 71 ч`) исключил бы её до проверки `sent_at`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_digests_edits_a_digest_delivered_late_from_a_stale_queue() {
    let db = support::isolated_db().await;
    let mut stand = digest_sync_stand(&db).await;
    let done_at = stand.at + Duration::minutes(1);
    let sent_at = done_at - Duration::hours(46);
    age_digest_sent_at(
        &db.pool,
        stand.petya_id,
        sent_at,
        Some(sent_at - Duration::hours(30)),
    )
    .await;
    complete_as(
        &db,
        &stand.household,
        stand.shared.id,
        &Actor::user(stand.masha_id, UserRole::Member),
        done_at,
    )
    .await;

    digest_sync_tick(&mut stand, done_at).await;
    stand.petya.deliver().await;
    stand.masha.deliver().await;

    assert_eq!(edit_count(&db.pool, stand.petya.chat).await, 1);
    let html = message_html(&stand.petya, stand.petya_msg);
    assert!(html.contains("✅ Вынести мусор · Маша"), "{html}");
    assert_digest_hash_matches_render(&db.pool, stand.petya_id).await;
}

/// Сводка без снимка (как до выката) не правится.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_digests_leaves_digest_without_snapshot_alone() {
    let db = support::isolated_db().await;
    let mut stand = digest_sync_stand(&db).await;
    sqlx::query(
        "UPDATE notification_deliveries SET digest_items = NULL
         WHERE user_id = $1 AND kind = 'digest'",
    )
    .bind(stand.petya_id)
    .execute(&db.pool)
    .await
    .expect("drop snapshot");
    complete_as(
        &db,
        &stand.household,
        stand.shared.id,
        &Actor::user(stand.masha_id, UserRole::Member),
        stand.at,
    )
    .await;

    let at = stand.at;
    digest_sync_tick(&mut stand, at).await;
    stand.petya.deliver().await;
    stand.masha.deliver().await;

    assert_eq!(edit_count(&db.pool, stand.petya.chat).await, 0);
}

/// Получатель без чата не правится. Колонка `telegram_chat_id` NOT NULL,
/// поэтому ограничение снимается в изолированной базе; получатель один, чтобы
/// шаг не строил представление по другому получателю с живым чатом.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_digests_leaves_recipient_without_chat_alone() {
    let db = support::isolated_db().await;
    let household = household_without_digest(&db).await;
    set_household_timezone(&db.pool, household.household_id, "Europe/Moscow").await;
    let petya_id = add_member(&db.pool, household.household_id, "Петя", true).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = digest_day();
    let at = local_utc(moscow, day, 8, 30);
    let petya_chat = chat_id(&db.pool, petya_id).await;
    let mut petya = Harness::new(&db.pool, petya_chat).named("Петя");
    petya.freeze_at(at);
    let shared = create_all_day_task(&db, &household, "Вынести мусор", day, None).await;
    let stats = scheduler(&db.pool).tick(at).await.expect("digest tick");
    assert_eq!(stats.delivered, 1, "{stats:?}");
    petya.deliver().await;

    sqlx::query("ALTER TABLE users ALTER COLUMN telegram_chat_id DROP NOT NULL")
        .execute(&db.pool)
        .await
        .expect("drop not null");
    sqlx::query("UPDATE users SET telegram_chat_id = NULL WHERE id = $1")
        .bind(petya_id)
        .execute(&db.pool)
        .await
        .expect("drop chat");
    complete_as(&db, &household, shared.id, &household.owner(), at).await;

    scheduler(&db.pool)
        .tick(at + Duration::minutes(1))
        .await
        .expect("sync tick");
    petya.advance(Duration::minutes(1));
    petya.deliver().await;

    assert_eq!(edit_count(&db.pool, petya_chat).await, 0);
}

/// Получатель сменил чат: прежняя сводка не правится — её `message_id`
/// принадлежит старому чату, а правка ушла бы в новый.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_digests_leaves_digest_alone_after_recipient_changed_chat() {
    let db = support::isolated_db().await;
    let mut stand = digest_sync_stand(&db).await;
    let new_chat = support::next_telegram_id();
    sqlx::query("UPDATE users SET telegram_chat_id = $2 WHERE id = $1")
        .bind(stand.petya_id)
        .bind(new_chat)
        .execute(&db.pool)
        .await
        .expect("change chat");
    let done_at = stand.at + Duration::minutes(1);
    complete_as(
        &db,
        &stand.household,
        stand.shared.id,
        &Actor::user(stand.masha_id, UserRole::Member),
        done_at,
    )
    .await;

    digest_sync_tick(&mut stand, done_at).await;
    stand.petya.deliver().await;
    stand.masha.deliver().await;

    // Ни в старом, ни в новом чате Пети правки нет; сводка осталась прежней.
    assert_eq!(edit_count(&db.pool, stand.petya.chat).await, 0);
    assert_eq!(edit_count(&db.pool, new_chat).await, 0);
    assert!(message_html(&stand.petya, stand.petya_msg).contains("☐ Вынести мусор"));
}
