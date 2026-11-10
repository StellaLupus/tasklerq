//! Общие помощники тестов уведомлений и сводных сообщений.

use std::sync::Arc;

use chrono::{DateTime, Datelike, NaiveDate, TimeZone, Utc};
use chrono_tz::Tz;
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

use super::TestHousehold;
use super::telegram::{Harness, buttons};
use tasklerq::domain::models::{Actor, Item, RecurrenceInput, UserRole};
use tasklerq::jobs::scheduler::{Scheduler, TickStats};
use tasklerq::storage::repositories::notifications;
use tokio::sync::Notify;

pub fn scheduler(pool: &PgPool) -> Scheduler {
    Scheduler::new(pool.clone(), Arc::new(Notify::new()))
}

pub fn local_utc(timezone: Tz, date: NaiveDate, hour: u32, minute: u32) -> DateTime<Utc> {
    timezone
        .with_ymd_and_hms(date.year(), date.month(), date.day(), hour, minute, 0)
        .single()
        .expect("local time")
        .with_timezone(&Utc)
}

pub fn day() -> NaiveDate {
    Utc::now()
        .with_timezone(&chrono_tz::Europe::Moscow)
        .date_naive()
}

pub async fn set_notifications(pool: &PgPool, user_id: Uuid, value: serde_json::Value) {
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

pub async fn quiet_off(pool: &PgPool, telegram_id: i64) {
    sqlx::query(
        "UPDATE users
         SET settings = jsonb_set(
             settings, '{notifications}',
             jsonb_build_object('digest_enabled', false, 'quiet_hours_enabled', false),
             true
         )
         WHERE telegram_user_id = $1",
    )
    .bind(telegram_id)
    .execute(pool)
    .await
    .expect("quiet off");
}

pub async fn quiet_settings(pool: &PgPool, user_id: Uuid, digest: bool) {
    set_notifications(
        pool,
        user_id,
        json!({
            "digest_enabled": digest,
            "digest_time": "08:00",
            "quiet_hours_enabled": true,
            "quiet_start": "22:00",
            "quiet_end": "08:00"
        }),
    )
    .await;
}

pub async fn onboarded(pool: &PgPool) -> Harness {
    let mut bot = Harness::new(pool, super::next_telegram_id()).named("Alice");
    bot.text("/start").await;
    bot.text("Семья").await;
    bot.press("Москва").await;
    bot.press("меню").await;
    bot
}

pub async fn household_for(pool: &PgPool, telegram_id: i64) -> TestHousehold {
    let (household_id, owner_id): (Uuid, Uuid) = sqlx::query_as(
        "SELECT household_id, id FROM users WHERE telegram_user_id = $1 AND deleted_at IS NULL",
    )
    .bind(telegram_id)
    .fetch_one(pool)
    .await
    .expect("user");
    let list = |kind: &'static str| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, Uuid>(
                "SELECT id FROM lists WHERE household_id = $1 AND kind = $2::list_kind AND is_default",
            )
            .bind(household_id)
            .bind(kind)
            .fetch_one(&pool)
            .await
            .expect("list")
        }
    };
    TestHousehold {
        household_id,
        owner_id,
        member_id: owner_id,
        tasks_list_id: list("tasks").await,
        shopping_list_id: list("shopping").await,
        events_list_id: list("events").await,
    }
}

pub async fn create_task(
    pool: &PgPool,
    household: &TestHousehold,
    title: &str,
    due_at: DateTime<Utc>,
    recurrence: Option<RecurrenceInput>,
) -> Item {
    let mut item = super::new_item(household.tasks_list_id, title);
    item.due_at = Some(due_at);
    item.assignee_user_id = Some(household.owner_id);
    item.recurrence = recurrence;
    super::create_item(
        pool,
        household,
        household.tasks_list_id,
        &Actor::user(household.owner_id, UserRole::Owner),
        item,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn insert_delivery(
    pool: &PgPool,
    household_id: Uuid,
    user_id: Uuid,
    item_id: Uuid,
    kind: &str,
    key: &str,
    scheduled_at: DateTime<Utc>,
    deferred: bool,
) -> Uuid {
    // Snooze привязан к вхождению срока: без привязки он считается устаревшим.
    let origin_key = if kind == "snooze" {
        let (due_at, due_date): (Option<DateTime<Utc>>, Option<NaiveDate>) =
            sqlx::query_as("SELECT due_at, due_date FROM items WHERE id = $1")
                .bind(item_id)
                .fetch_one(pool)
                .await
                .expect("item due");
        Some(tasklerq::jobs::scheduler::snooze_origin(due_at, due_date))
    } else {
        None
    };
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO notification_deliveries (
             id, household_id, user_id, item_id, occurrence_key, origin_key, kind,
             status, scheduled_at, deferred, created_at, updated_at
         )
         VALUES ($1, $2, $3, $4, $5, $6, $7::notification_kind, 'pending', $8, $9, $8, $8)",
    )
    .bind(id)
    .bind(household_id)
    .bind(user_id)
    .bind(item_id)
    .bind(key)
    .bind(origin_key)
    .bind(kind)
    .bind(scheduled_at)
    .bind(deferred)
    .execute(pool)
    .await
    .expect("delivery");
    id
}

#[allow(clippy::too_many_arguments)]
pub async fn insert_digest_delivery(
    pool: &PgPool,
    household_id: Uuid,
    user_id: Uuid,
    key: &str,
    scheduled_at: DateTime<Utc>,
    deferred: bool,
) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO notification_deliveries (
             id, household_id, user_id, item_id, occurrence_key, kind,
             status, scheduled_at, deferred, created_at, updated_at
         )
         VALUES ($1, $2, $3, NULL, $4, 'digest', 'pending', $5, $6, $5, $5)",
    )
    .bind(id)
    .bind(household_id)
    .bind(user_id)
    .bind(key)
    .bind(scheduled_at)
    .bind(deferred)
    .execute(pool)
    .await
    .expect("digest delivery");
    id
}

pub async fn deferred_reminder(
    pool: &PgPool,
    household: &TestHousehold,
    item_id: Uuid,
    due_at: DateTime<Utc>,
    scheduled_at: DateTime<Utc>,
) {
    let key = tasklerq::jobs::scheduler::occurrence_key(due_at);
    insert_delivery(
        pool,
        household.household_id,
        household.owner_id,
        item_id,
        "reminder",
        &key,
        scheduled_at,
        true,
    )
    .await;
}

pub async fn outbox_messages(pool: &PgPool, chat: i64) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT payload->>'html' FROM outbox
         WHERE chat_id = $1 AND payload ? 'html'
         ORDER BY seq",
    )
    .bind(chat)
    .fetch_all(pool)
    .await
    .expect("outbox")
}

/// Тик с триггером, ломающим вставку сводки: шаг сводки сообщает ошибку, а
/// триггер снимается и в этом случае, поэтому база остаётся пригодной.
///
/// База должна быть отдельной: DDL на `notification_deliveries` задел бы
/// параллельные тики общей базы.
pub async fn tick_with_a_failing_digest(
    pool: &PgPool,
    at: DateTime<Utc>,
) -> Result<TickStats, sqlx::Error> {
    sqlx::query(
        "CREATE FUNCTION fail_digest() RETURNS trigger AS $$
         BEGIN
             RAISE EXCEPTION 'forced digest failure';
         END $$ LANGUAGE plpgsql",
    )
    .execute(pool)
    .await
    .expect("function");
    sqlx::query(
        "CREATE TRIGGER fail_digest BEFORE INSERT ON notification_deliveries
         FOR EACH ROW WHEN (NEW.kind = 'digest') EXECUTE FUNCTION fail_digest()",
    )
    .execute(pool)
    .await
    .expect("trigger");

    let result = scheduler(pool).tick(at).await;

    sqlx::query("DROP TRIGGER fail_digest ON notification_deliveries")
        .execute(pool)
        .await
        .expect("drop trigger");
    sqlx::query("DROP FUNCTION fail_digest()")
        .execute(pool)
        .await
        .expect("drop function");

    result
}

pub async fn tick_and_deliver(bot: &mut Harness, pool: &PgPool, at: DateTime<Utc>) -> TickStats {
    let stats = scheduler(pool).tick(at).await.expect("tick");
    if at > bot.now {
        bot.advance(at - bot.now);
    }
    bot.deliver().await;
    stats
}

pub fn button_of(html: &str, label: &str) -> String {
    buttons(html)
        .into_iter()
        .find(|(text, _)| text.contains(label))
        .unwrap_or_else(|| panic!("button `{label}` not found in:\n{html}"))
        .1
}

pub fn block_count(html: &str, label: &str) -> usize {
    html.matches(label).count()
}

pub async fn item_status(pool: &PgPool, item_id: Uuid) -> (String, i32, Option<DateTime<Utc>>) {
    sqlx::query_as(
        "SELECT status::text, version, due_at FROM items WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(item_id)
    .fetch_one(pool)
    .await
    .expect("item")
}

pub async fn snooze_rows(pool: &PgPool, item_id: Uuid) -> Vec<(String, DateTime<Utc>, bool)> {
    sqlx::query_as(
        "SELECT status::text, scheduled_at, deferred
         FROM notification_deliveries
         WHERE item_id = $1 AND kind = 'snooze'
         ORDER BY created_at, id",
    )
    .bind(item_id)
    .fetch_all(pool)
    .await
    .expect("snooze deliveries")
}

pub async fn completed_events(pool: &PgPool, item_id: Uuid) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM item_events WHERE item_id = $1 AND event_type = 'completed'",
    )
    .bind(item_id)
    .fetch_one(pool)
    .await
    .expect("completed events")
}

pub fn batch_message(bot: &Harness, message_id: i64) -> String {
    bot.fake
        .messages(bot.chat)
        .into_iter()
        .find(|(id, _)| *id == message_id)
        .map(|(_, html)| html)
        .expect("batch message should still be alive")
}

pub async fn create_empty_batch(
    pool: &PgPool,
    household: &TestHousehold,
    now: DateTime<Utc>,
) -> Uuid {
    let id = Uuid::now_v7();
    let mut conn = pool.acquire().await.expect("conn");
    notifications::create_batch(
        &mut conn,
        id,
        household.household_id,
        household.owner_id,
        1,
        now,
    )
    .await
    .expect("batch");
    id
}

pub async fn open_notifications(bot: &mut Harness) {
    bot.press("Настройки").await;
    bot.press("Уведомления").await;
}

pub fn local_today(bot: &Harness, timezone: Tz) -> NaiveDate {
    bot.now.with_timezone(&timezone).date_naive()
}
