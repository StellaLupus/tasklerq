//! Сводные сообщения после тихих часов: фиксированное время выпуска,
//! блоки, пагинация и действия.

mod support;

use std::sync::Arc;

use chrono::{DateTime, Datelike, Duration, NaiveDate, TimeZone, Utc};
use chrono_tz::Tz;
use serde_json::json;
use sqlx::PgPool;
use support::TestHousehold;
use support::telegram::{Harness, buttons};
use tasklerq::config::RetentionConfig;
use tasklerq::domain::models::{Actor, Item, RecurrenceFreq, RecurrenceInput, UserRole};
use tasklerq::jobs::digest::digest_occurrence_key;
use tasklerq::jobs::retention;
use tasklerq::jobs::scheduler::{Scheduler, TickStats};
use tasklerq::storage::repositories::items;
use tasklerq::storage::repositories::notifications;
use tasklerq::telegram::bot::notify::BATCH_PAGE_SIZE;
use tokio::sync::Notify;
use uuid::Uuid;

fn scheduler(pool: &PgPool) -> Scheduler {
    Scheduler::new(pool.clone(), Arc::new(Notify::new()))
}

fn local_utc(timezone: Tz, date: NaiveDate, hour: u32, minute: u32) -> DateTime<Utc> {
    timezone
        .with_ymd_and_hms(date.year(), date.month(), date.day(), hour, minute, 0)
        .single()
        .expect("local time")
        .with_timezone(&Utc)
}

fn day() -> NaiveDate {
    Utc::now()
        .with_timezone(&chrono_tz::Europe::Moscow)
        .date_naive()
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

async fn quiet_off(pool: &PgPool, telegram_id: i64) {
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

async fn onboarded(pool: &PgPool) -> Harness {
    let mut bot = Harness::new(pool, support::next_telegram_id()).named("Alice");
    bot.text("/start").await;
    bot.text("Семья").await;
    bot.press("Москва").await;
    bot.press("меню").await;
    bot
}

async fn household_for(pool: &PgPool, telegram_id: i64) -> TestHousehold {
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
    }
}

async fn create_task(
    pool: &PgPool,
    household: &TestHousehold,
    title: &str,
    due_at: DateTime<Utc>,
    recurrence: Option<RecurrenceInput>,
) -> Item {
    let mut item = support::new_item(household.tasks_list_id, title);
    item.due_at = Some(due_at);
    item.assignee_user_id = Some(household.owner_id);
    item.recurrence = recurrence;
    support::create_item(
        pool,
        household,
        household.tasks_list_id,
        &Actor::user(household.owner_id, UserRole::Owner),
        item,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn insert_delivery(
    pool: &PgPool,
    household_id: Uuid,
    user_id: Uuid,
    item_id: Uuid,
    kind: &str,
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
         VALUES ($1, $2, $3, $4, $5, $6::notification_kind, 'pending', $7, $8, $7, $7)",
    )
    .bind(id)
    .bind(household_id)
    .bind(user_id)
    .bind(item_id)
    .bind(key)
    .bind(kind)
    .bind(scheduled_at)
    .bind(deferred)
    .execute(pool)
    .await
    .expect("delivery");
    id
}

#[allow(clippy::too_many_arguments)]
async fn insert_digest_delivery(
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

async fn deferred_reminder(
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

async fn outbox_messages(pool: &PgPool, chat: i64) -> Vec<String> {
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

async fn tick_and_deliver(bot: &mut Harness, pool: &PgPool, at: DateTime<Utc>) -> TickStats {
    let stats = scheduler(pool).tick(at).await.expect("tick");
    if at > bot.now {
        bot.advance(at - bot.now);
    }
    bot.deliver().await;
    stats
}

fn button_of(html: &str, label: &str) -> String {
    buttons(html)
        .into_iter()
        .find(|(text, _)| text.contains(label))
        .unwrap_or_else(|| panic!("button `{label}` not found in:\n{html}"))
        .1
}

fn block_count(html: &str, label: &str) -> usize {
    html.matches(label).count()
}

async fn item_status(pool: &PgPool, item_id: Uuid) -> (String, i32, Option<DateTime<Utc>>) {
    sqlx::query_as(
        "SELECT status::text, version, due_at FROM items WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(item_id)
    .fetch_one(pool)
    .await
    .expect("item")
}

async fn snooze_rows(pool: &PgPool, item_id: Uuid) -> Vec<(String, DateTime<Utc>, bool)> {
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

async fn completed_events(pool: &PgPool, item_id: Uuid) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM item_events WHERE item_id = $1 AND event_type = 'completed'",
    )
    .bind(item_id)
    .fetch_one(pool)
    .await
    .expect("completed events")
}

fn batch_message(bot: &Harness, message_id: i64) -> String {
    bot.fake
        .messages(bot.chat)
        .into_iter()
        .find(|(id, _)| *id == message_id)
        .map(|(_, html)| html)
        .expect("batch message should still be alive")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn moved_quiet_hours_boundary_defers_release_to_the_new_end() {
    let db = support::isolated_db().await;
    let household = support::create_household(&db.pool).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = day();
    set_notifications(
        &db.pool,
        household.owner_id,
        json!({
            "digest_enabled": false,
            "quiet_hours_enabled": true,
            "quiet_start": "22:00",
            "quiet_end": "08:00"
        }),
    )
    .await;
    set_notifications(
        &db.pool,
        household.member_id,
        json!({"digest_enabled": false, "quiet_hours_enabled": false}),
    )
    .await;
    let item = create_task(
        &db.pool,
        &household,
        "Ночное напоминание",
        local_utc(moscow, day, 23, 45),
        None,
    )
    .await;
    let chat = sqlx::query_scalar("SELECT telegram_chat_id FROM users WHERE id = $1")
        .bind(household.owner_id)
        .fetch_one(&db.pool)
        .await
        .expect("chat");
    let scheduler = scheduler(&db.pool);

    assert_eq!(
        scheduler
            .tick(local_utc(moscow, day, 23, 30))
            .await
            .expect("night tick")
            .delivered,
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, DateTime<Utc>>(
            "SELECT scheduled_at FROM notification_deliveries WHERE item_id = $1",
        )
        .bind(item.id)
        .fetch_one(&db.pool)
        .await
        .expect("scheduled"),
        local_utc(moscow, day + Duration::days(1), 8, 0)
    );

    // Границы сдвинули: в 08:00 ещё тихо, доставка переносится на 09:00.
    set_notifications(
        &db.pool,
        household.owner_id,
        json!({
            "digest_enabled": false,
            "quiet_hours_enabled": true,
            "quiet_start": "22:00",
            "quiet_end": "09:00"
        }),
    )
    .await;
    assert_eq!(
        scheduler
            .tick(local_utc(moscow, day + Duration::days(1), 8, 0))
            .await
            .expect("08:00 tick")
            .delivered,
        0
    );
    assert!(outbox_messages(&db.pool, chat).await.is_empty());
    assert_eq!(
        scheduler
            .tick(local_utc(moscow, day + Duration::days(1), 9, 0))
            .await
            .expect("09:00 tick")
            .delivered,
        1
    );
    assert_eq!(outbox_messages(&db.pool, chat).await.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn five_deferred_notifications_become_one_message_and_digest_stays_separate() {
    let db = support::isolated_db().await;
    let household = support::create_household(&db.pool).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = day();
    set_notifications(
        &db.pool,
        household.owner_id,
        json!({
            "digest_enabled": true,
            "digest_time": "08:00",
            "quiet_hours_enabled": false
        }),
    )
    .await;
    let morning = local_utc(moscow, day, 8, 0);
    let due = local_utc(moscow, day, 9, 0);
    let mut reminders = Vec::new();
    for title in ["Интернет", "Мама", "Отчёт"] {
        let item = create_task(&db.pool, &household, title, due, None).await;
        deferred_reminder(&db.pool, &household, item.id, due, morning).await;
        reminders.push(item);
    }
    let assignment = create_task(&db.pool, &household, "Купить торт", due, None).await;
    insert_delivery(
        &db.pool,
        household.household_id,
        household.owner_id,
        assignment.id,
        "assignment",
        "assignment:batch",
        morning,
        true,
    )
    .await;
    let snoozed = create_task(&db.pool, &household, "Выключить свет", due, None).await;
    let key = notifications::snooze_key(morning);
    insert_delivery(
        &db.pool,
        household.household_id,
        household.owner_id,
        snoozed.id,
        "snooze",
        &key,
        morning,
        true,
    )
    .await;
    let chat = sqlx::query_scalar("SELECT telegram_chat_id FROM users WHERE id = $1")
        .bind(household.owner_id)
        .fetch_one(&db.pool)
        .await
        .expect("chat");
    let scheduler = scheduler(&db.pool);

    let stats = scheduler.tick(morning).await.expect("morning tick");
    // Одно сводное сообщение плюс отдельная утренняя сводка.
    assert_eq!(stats.delivered, 2, "{stats:?}");
    let sent = outbox_messages(&db.pool, chat).await;
    assert_eq!(sent.len(), 2, "{sent:?}");
    let batch = sent
        .iter()
        .find(|html| html.contains("Пока были тихие часы"))
        .expect("batch message");
    assert_eq!(block_count(batch, "⏰ напоминание"), 3, "{batch}");
    assert_eq!(block_count(batch, "😴 отложенное"), 1, "{batch}");
    assert_eq!(block_count(batch, "👤 назначение"), 1, "{batch}");
    let labels: Vec<String> = buttons(batch).into_iter().map(|(label, _)| label).collect();
    assert!(labels.iter().any(|label| label.contains("Завершить все")));
    for label in [
        "Готово",
        "На час",
        "Открыть",
        "Завершить все",
        "Отложить все на час",
    ] {
        assert!(
            labels.iter().any(|item| item.contains(label)),
            "{label}: {labels:?}"
        );
    }
    assert!(sent.iter().any(|html| html.contains("Доброе утро")));

    // Повторный тик не создаёт второе сводное сообщение.
    assert_eq!(
        scheduler
            .tick(morning)
            .await
            .expect("repeat tick")
            .delivered,
        0
    );
    assert_eq!(outbox_messages(&db.pool, chat).await.len(), 2);
    assert_eq!(reminders.len(), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_deferred_notification_is_an_ordinary_message() {
    let db = support::isolated_db().await;
    let household = support::create_household(&db.pool).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = day();
    set_notifications(
        &db.pool,
        household.owner_id,
        json!({"digest_enabled": false, "quiet_hours_enabled": false}),
    )
    .await;
    let morning = local_utc(moscow, day, 8, 0);
    let due = local_utc(moscow, day, 9, 0);
    let item = create_task(&db.pool, &household, "Одно напоминание", due, None).await;
    deferred_reminder(&db.pool, &household, item.id, due, morning).await;
    let chat = sqlx::query_scalar("SELECT telegram_chat_id FROM users WHERE id = $1")
        .bind(household.owner_id)
        .fetch_one(&db.pool)
        .await
        .expect("chat");

    assert_eq!(
        scheduler(&db.pool)
            .tick(morning)
            .await
            .expect("tick")
            .delivered,
        1
    );
    let sent = outbox_messages(&db.pool, chat).await;
    assert_eq!(sent.len(), 1);
    assert!(!sent[0].contains("Пока были тихие часы"), "{sent:?}");
    let labels: Vec<String> = buttons(&sent[0])
        .into_iter()
        .map(|(label, _)| label)
        .collect();
    for label in ["Готово", "Отложить на час", "Завтра в 09:00", "Открыть"]
    {
        assert!(
            labels.iter().any(|item| item.contains(label)),
            "{label}: {labels:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn seven_deferred_notifications_are_paginated_and_arrow_edits_the_same_message() {
    let db = support::isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = bot.now.with_timezone(&moscow).date_naive();
    let morning = local_utc(moscow, day, 8, 0);
    let due = local_utc(moscow, day, 9, 0);
    for index in 0..7 {
        let item = create_task(&db.pool, &household, &format!("Задача {index}"), due, None).await;
        deferred_reminder(&db.pool, &household, item.id, due, morning).await;
    }

    tick_and_deliver(&mut bot, &db.pool, morning).await;
    let (message_id, html) = bot.latest();
    assert!(html.contains("1/2"), "{html}");
    assert_eq!(
        block_count(&html, "⏰ напоминание"),
        BATCH_PAGE_SIZE,
        "{html}"
    );
    let forward = button_of(&html, "→");
    let sent_before = bot.fake.sent().len();

    bot.press_data(&forward, message_id).await;
    let (same_id, page_two) = bot.latest();
    assert_eq!(same_id, message_id);
    assert!(page_two.contains("2/2"), "{page_two}");
    assert_eq!(block_count(&page_two, "⏰ напоминание"), 2, "{page_two}");
    assert_eq!(bot.fake.sent().len(), sent_before, "pagination must edit");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn done_in_a_block_completes_once_and_repeat_does_not_change_it() {
    let db = support::isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = bot.now.with_timezone(&moscow).date_naive();
    let morning = local_utc(moscow, day, 8, 0);
    let due = local_utc(moscow, day, 9, 0);
    let first = create_task(&db.pool, &household, "Первая", due, None).await;
    let second = create_task(&db.pool, &household, "Вторая", due, None).await;
    deferred_reminder(&db.pool, &household, first.id, due, morning).await;
    deferred_reminder(&db.pool, &household, second.id, due, morning).await;

    tick_and_deliver(&mut bot, &db.pool, morning).await;
    let (message_id, html) = bot.latest();
    let done = button_of(&html, "Готово");
    bot.press_data(&done, message_id).await;
    assert_eq!(item_status(&db.pool, first.id).await.0, "done");
    let (_, edited) = bot.latest();
    assert!(edited.contains("✅ Выполнено"), "{edited}");

    let version = item_status(&db.pool, first.id).await.1;
    bot.press_data(&done, message_id).await;
    assert_eq!(item_status(&db.pool, first.id).await.1, version);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM item_events WHERE item_id = $1 AND event_type = 'completed'",
        )
        .bind(first.id)
        .fetch_one(&db.pool)
        .await
        .expect("events"),
        1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn complete_all_skips_a_task_changed_after_release_and_repeat_changes_nothing() {
    let db = support::isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = bot.now.with_timezone(&moscow).date_naive();
    let morning = local_utc(moscow, day, 8, 0);
    let due = local_utc(moscow, day, 9, 0);
    let mut items = Vec::new();
    for title in ["Раз", "Два", "Три"] {
        let item = create_task(&db.pool, &household, title, due, None).await;
        deferred_reminder(&db.pool, &household, item.id, due, morning).await;
        items.push(item);
    }

    tick_and_deliver(&mut bot, &db.pool, morning).await;
    // Вторую задачу изменили после выпуска сводного.
    sqlx::query(
        "UPDATE items SET title = title || ' (изменено)', version = version + 1 WHERE id = $1",
    )
    .bind(items[1].id)
    .execute(&db.pool)
    .await
    .expect("external change");
    let (message_id, html) = bot.latest();
    let complete_all = button_of(&html, "Завершить все");
    bot.press_data(&complete_all, message_id).await;

    assert_eq!(item_status(&db.pool, items[0].id).await.0, "done");
    assert_eq!(item_status(&db.pool, items[1].id).await.0, "open");
    assert_eq!(item_status(&db.pool, items[2].id).await.0, "done");
    let (_, edited) = bot.latest();
    assert!(edited.contains("Выполнено 2"), "{edited}");
    assert!(edited.contains("1 уже изменились"), "{edited}");

    let versions = [
        item_status(&db.pool, items[0].id).await.1,
        item_status(&db.pool, items[1].id).await.1,
        item_status(&db.pool, items[2].id).await.1,
    ];
    let again = button_of(&edited, "Завершить все");
    bot.press_data(&again, message_id).await;
    assert_eq!(
        versions,
        [
            item_status(&db.pool, items[0].id).await.1,
            item_status(&db.pool, items[1].id).await.1,
            item_status(&db.pool, items[2].id).await.1,
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snooze_all_creates_one_more_batch_and_repeat_adds_no_deliveries() {
    let db = support::isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = bot.now.with_timezone(&moscow).date_naive();
    let morning = local_utc(moscow, day, 8, 0);
    let due = local_utc(moscow, day, 9, 0);
    let first = create_task(&db.pool, &household, "Первая", due, None).await;
    let second = create_task(&db.pool, &household, "Вторая", due, None).await;
    deferred_reminder(&db.pool, &household, first.id, due, morning).await;
    deferred_reminder(&db.pool, &household, second.id, due, morning).await;

    tick_and_deliver(&mut bot, &db.pool, morning).await;
    let (message_id, html) = bot.latest();
    let snooze_all = button_of(&html, "Отложить все на час");
    bot.press_data(&snooze_all, message_id).await;
    let snooze_rows = |item_id: Uuid| {
        let pool = db.pool.clone();
        async move {
            sqlx::query_as::<_, (String, DateTime<Utc>, bool)>(
                "SELECT status::text, scheduled_at, deferred
                 FROM notification_deliveries WHERE item_id = $1 AND kind = 'snooze'",
            )
            .bind(item_id)
            .fetch_all(&pool)
            .await
            .expect("snooze")
        }
    };
    assert_eq!(snooze_rows(first.id).await.len(), 1);
    assert_eq!(snooze_rows(second.id).await.len(), 1);

    let (_, edited) = bot.latest();
    let again = button_of(&edited, "Отложить все на час");
    bot.press_data(&again, message_id).await;
    assert_eq!(snooze_rows(first.id).await.len(), 1);
    assert_eq!(snooze_rows(second.id).await.len(), 1);

    let snoozed_at = snooze_rows(first.id).await[0].1;
    let stats = tick_and_deliver(&mut bot, &db.pool, snoozed_at).await;
    assert_eq!(stats.delivered, 1, "{stats:?}");
    let (_, next) = bot.latest();
    assert!(next.contains("Пока были тихие часы"), "{next}");
    assert_eq!(block_count(&next, "😴 отложенное"), 2, "{next}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snooze_all_with_one_remaining_task_sends_an_ordinary_message() {
    let db = support::isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = bot.now.with_timezone(&moscow).date_naive();
    let morning = local_utc(moscow, day, 8, 0);
    let due = local_utc(moscow, day, 9, 0);
    let first = create_task(&db.pool, &household, "Останется", due, None).await;
    let second = create_task(&db.pool, &household, "Выполнят", due, None).await;
    deferred_reminder(&db.pool, &household, first.id, due, morning).await;
    deferred_reminder(&db.pool, &household, second.id, due, morning).await;

    tick_and_deliver(&mut bot, &db.pool, morning).await;
    let (message_id, html) = bot.latest();
    let snooze_all = button_of(&html, "Отложить все на час");
    bot.press_data(&snooze_all, message_id).await;
    let mut conn = db.pool.acquire().await.expect("conn");
    items::complete(
        &mut conn,
        household.household_id,
        &Actor::user(household.owner_id, UserRole::Owner),
        second.id,
        None,
        "Europe/Moscow",
        bot.now,
    )
    .await
    .expect("complete second");

    let deliver_at: DateTime<Utc> = sqlx::query_scalar(
        "SELECT scheduled_at FROM notification_deliveries
         WHERE item_id = $1 AND kind = 'snooze' AND status = 'pending'",
    )
    .bind(first.id)
    .fetch_one(&db.pool)
    .await
    .expect("snooze at");
    assert_eq!(
        tick_and_deliver(&mut bot, &db.pool, deliver_at)
            .await
            .delivered,
        1
    );
    let (_, next) = bot.latest();
    assert!(!next.contains("Пока были тихие часы"), "{next}");
    assert!(next.contains("Пора"), "{next}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_completed_at_night_is_not_in_the_batch() {
    let db = support::isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = bot.now.with_timezone(&moscow).date_naive();
    let morning = local_utc(moscow, day, 8, 0);
    let due = local_utc(moscow, day, 9, 0);
    let mut items = Vec::new();
    for title in ["Ночью выполнена", "Осталась раз", "Осталась два"]
    {
        let item = create_task(&db.pool, &household, title, due, None).await;
        deferred_reminder(&db.pool, &household, item.id, due, morning).await;
        items.push(item);
    }
    let mut conn = db.pool.acquire().await.expect("conn");
    items::complete(
        &mut conn,
        household.household_id,
        &Actor::user(household.owner_id, UserRole::Owner),
        items[0].id,
        None,
        "Europe/Moscow",
        bot.now,
    )
    .await
    .expect("night complete");

    tick_and_deliver(&mut bot, &db.pool, morning).await;
    let (_, html) = bot.latest();
    assert!(!html.contains("Ночью выполнена"), "{html}");
    assert_eq!(block_count(&html, "⏰ напоминание"), 2, "{html}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deleted_task_in_a_block_shows_that_it_is_gone_and_does_nothing() {
    let db = support::isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = bot.now.with_timezone(&moscow).date_naive();
    let morning = local_utc(moscow, day, 8, 0);
    let due = local_utc(moscow, day, 9, 0);
    let first = create_task(&db.pool, &household, "Удалят", due, None).await;
    let second = create_task(&db.pool, &household, "Останется", due, None).await;
    deferred_reminder(&db.pool, &household, first.id, due, morning).await;
    deferred_reminder(&db.pool, &household, second.id, due, morning).await;

    tick_and_deliver(&mut bot, &db.pool, morning).await;
    let (message_id, html) = bot.latest();
    let done = button_of(&html, "Готово");
    let mut conn = db.pool.acquire().await.expect("conn");
    items::soft_delete(
        &mut conn,
        household.household_id,
        &Actor::user(household.owner_id, UserRole::Owner),
        first.id,
        bot.now,
    )
    .await
    .expect("delete");

    bot.press_data(&done, message_id).await;
    let (_, edited) = bot.latest();
    assert!(edited.contains("Задача удалена"), "{edited}");
    assert_eq!(item_status(&db.pool, second.id).await.0, "open");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snooze_in_a_block_creates_one_delivery_and_an_ordinary_reminder_later() {
    let db = support::isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = bot.now.with_timezone(&moscow).date_naive();
    let morning = local_utc(moscow, day, 8, 0);
    let due = local_utc(moscow, day, 9, 0);
    let first = create_task(&db.pool, &household, "Первая", due, None).await;
    let second = create_task(&db.pool, &household, "Вторая", due, None).await;
    deferred_reminder(&db.pool, &household, first.id, due, morning).await;
    deferred_reminder(&db.pool, &household, second.id, due, morning).await;

    tick_and_deliver(&mut bot, &db.pool, morning).await;
    let (message_id, html) = bot.latest();
    let snooze = button_of(&html, "На час");
    let pressed_at = bot.now;

    bot.press_data(&snooze, message_id).await;
    let (_, edited) = bot.latest();
    let expected = (pressed_at + Duration::hours(1))
        .with_timezone(&moscow)
        .format("%H:%M")
        .to_string();
    assert!(
        edited.contains(&format!("😴 Напомню в {expected}")),
        "{edited}"
    );
    let rows = snooze_rows(&db.pool, first.id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "pending");
    let snoozed_at = rows[0].1;
    assert_eq!(
        snoozed_at.timestamp(),
        (pressed_at + Duration::hours(1)).timestamp()
    );

    // Повторное нажатие той же кнопки не создаёт вторую доставку.
    bot.press_data(&snooze, message_id).await;
    assert_eq!(snooze_rows(&db.pool, first.id).await.len(), 1);

    // Отложена одна задача — через час приходит обычное сообщение.
    assert_eq!(
        tick_and_deliver(&mut bot, &db.pool, snoozed_at)
            .await
            .delivered,
        1
    );
    let (_, next) = bot.latest();
    assert!(!next.contains("Пока были тихие часы"), "{next}");
    assert!(next.contains("Пора"), "{next}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn open_in_a_block_shows_the_card_and_a_deleted_task_becomes_missing() {
    let db = support::isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = bot.now.with_timezone(&moscow).date_naive();
    let morning = local_utc(moscow, day, 8, 0);
    let due = local_utc(moscow, day, 9, 0);
    let first = create_task(&db.pool, &household, "Первая", due, None).await;
    let second = create_task(&db.pool, &household, "Вторая", due, None).await;
    deferred_reminder(&db.pool, &household, first.id, due, morning).await;
    deferred_reminder(&db.pool, &household, second.id, due, morning).await;

    tick_and_deliver(&mut bot, &db.pool, morning).await;
    let (message_id, html) = bot.latest();
    let opens: Vec<String> = buttons(&html)
        .into_iter()
        .filter(|(label, _)| label.contains("Открыть"))
        .map(|(_, data)| data)
        .collect();
    assert_eq!(opens.len(), 2, "{html}");

    bot.press_data(&opens[0], message_id).await;
    bot.assert_screen_contains("Первая");
    bot.assert_screen_contains("Список: Задачи");

    // Удалённая задача: блок становится «Задача удалена», карточка не открывается.
    let mut conn = db.pool.acquire().await.expect("conn");
    items::soft_delete(
        &mut conn,
        household.household_id,
        &Actor::user(household.owner_id, UserRole::Owner),
        second.id,
        bot.now,
    )
    .await
    .expect("delete");
    bot.press_data(&opens[1], message_id).await;

    let batch = batch_message(&bot, message_id);
    assert!(batch.contains("Задача удалена"), "{batch}");
    assert!(!batch.contains("Вторая"), "{batch}");
    bot.assert_screen_contains("Первая");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn complete_all_moves_a_recurring_task_by_one_occurrence_and_repeat_changes_nothing() {
    let db = support::isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = bot.now.with_timezone(&moscow).date_naive();
    let morning = local_utc(moscow, day, 8, 0);
    let due = local_utc(moscow, day, 9, 0);
    let recurring = create_task(
        &db.pool,
        &household,
        "Ежедневная зарядка",
        due,
        Some(support::recurrence(RecurrenceFreq::Daily, Vec::new())),
    )
    .await;
    let plain = create_task(&db.pool, &household, "Обычная", due, None).await;
    deferred_reminder(&db.pool, &household, recurring.id, due, morning).await;
    deferred_reminder(&db.pool, &household, plain.id, due, morning).await;

    tick_and_deliver(&mut bot, &db.pool, morning).await;
    let (message_id, html) = bot.latest();
    let complete_all = button_of(&html, "Завершить все");
    bot.press_data(&complete_all, message_id).await;

    let (status, version, next_due) = item_status(&db.pool, recurring.id).await;
    assert_eq!(
        status, "open",
        "recurring task moves to its next occurrence"
    );
    assert_eq!(version, recurring.version + 1);
    assert_eq!(next_due, Some(due + Duration::days(1)));
    assert_eq!(completed_events(&db.pool, recurring.id).await, 1);
    assert_eq!(item_status(&db.pool, plain.id).await.0, "done");

    let (_, edited) = bot.latest();
    let again = button_of(&edited, "Завершить все");
    bot.press_data(&again, message_id).await;
    assert_eq!(item_status(&db.pool, recurring.id).await.1, version);
    assert_eq!(item_status(&db.pool, recurring.id).await.2, next_due);
    assert_eq!(completed_events(&db.pool, recurring.id).await, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn changed_task_block_is_redrawn_and_the_old_version_only_refreshes_it() {
    let db = support::isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = bot.now.with_timezone(&moscow).date_naive();
    let morning = local_utc(moscow, day, 8, 0);
    let due = local_utc(moscow, day, 9, 0);
    let first = create_task(&db.pool, &household, "Первая", due, None).await;
    let second = create_task(&db.pool, &household, "Вторая", due, None).await;
    deferred_reminder(&db.pool, &household, first.id, due, morning).await;
    deferred_reminder(&db.pool, &household, second.id, due, morning).await;

    tick_and_deliver(&mut bot, &db.pool, morning).await;
    let (message_id, html) = bot.latest();
    let done = button_of(&html, "Готово");
    let changed_due = due + Duration::hours(2);
    sqlx::query(
        "UPDATE items
         SET title = 'Первая (новая)', due_at = $2, version = version + 1
         WHERE id = $1",
    )
    .bind(first.id)
    .bind(changed_due)
    .execute(&db.pool)
    .await
    .expect("external change");

    // «Готово» по устаревшей версии не завершает, а перерисовывает блок.
    bot.press_data(&done, message_id).await;
    let (status, version, due_at) = item_status(&db.pool, first.id).await;
    assert_eq!(status, "open");
    assert_eq!(version, first.version + 1);
    assert_eq!(due_at, Some(changed_due));
    let (_, edited) = bot.latest();
    assert!(edited.contains("Первая (новая)"), "{edited}");
    let toast = bot
        .fake
        .answers()
        .last()
        .expect("callback answer")
        .1
        .clone();
    assert_eq!(toast.as_deref(), Some("Задача уже изменилась"));
    let done_buttons: Vec<String> = buttons(&edited)
        .into_iter()
        .filter(|(label, _)| label.contains("Готово"))
        .map(|(_, data)| data)
        .collect();
    assert_eq!(done_buttons.len(), 2, "{edited}");

    // «Готово» по новой версии завершает задачу.
    bot.press_data(&done_buttons[0], message_id).await;
    assert_eq!(item_status(&db.pool, first.id).await.0, "done");
    assert_eq!(completed_events(&db.pool, first.id).await, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn archived_list_block_has_its_own_label_and_does_nothing() {
    let db = support::isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = bot.now.with_timezone(&moscow).date_naive();
    let morning = local_utc(moscow, day, 8, 0);
    let due = local_utc(moscow, day, 9, 0);
    let first = create_task(&db.pool, &household, "Первая", due, None).await;
    let second = create_task(&db.pool, &household, "Вторая", due, None).await;
    deferred_reminder(&db.pool, &household, first.id, due, morning).await;
    deferred_reminder(&db.pool, &household, second.id, due, morning).await;

    tick_and_deliver(&mut bot, &db.pool, morning).await;
    let (message_id, html) = bot.latest();
    let done = button_of(&html, "Готово");
    sqlx::query("UPDATE lists SET is_default = false, is_archived = true WHERE id = $1")
        .bind(household.tasks_list_id)
        .execute(&db.pool)
        .await
        .expect("archive list");

    bot.press_data(&done, message_id).await;
    let batch = batch_message(&bot, message_id);
    assert!(batch.contains("Список в архиве"), "{batch}");
    assert!(!batch.contains("Первая"), "{batch}");
    assert_eq!(item_status(&db.pool, first.id).await.0, "open");
    assert_eq!(completed_events(&db.pool, first.id).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn archived_list_deferred_assignments_behave_the_same_for_one_and_two() {
    async fn outcome(count: usize) -> (u64, Vec<String>, Vec<String>) {
        let db = support::isolated_db().await;
        let household = support::create_household(&db.pool).await;
        let moscow = chrono_tz::Europe::Moscow;
        let day = day();
        set_notifications(
            &db.pool,
            household.owner_id,
            json!({"digest_enabled": false, "quiet_hours_enabled": false}),
        )
        .await;
        let due = local_utc(moscow, day, 9, 0);
        let released_at = local_utc(moscow, day, 8, 0);
        for index in 0..count {
            let item =
                create_task(&db.pool, &household, &format!("Задача {index}"), due, None).await;
            insert_delivery(
                &db.pool,
                household.household_id,
                household.owner_id,
                item.id,
                "assignment",
                &format!("assignment:archived:{index}"),
                released_at,
                true,
            )
            .await;
        }
        sqlx::query("UPDATE lists SET is_default = false, is_archived = true WHERE id = $1")
            .bind(household.tasks_list_id)
            .execute(&db.pool)
            .await
            .expect("archive list");

        let stats = scheduler(&db.pool).tick(released_at).await.expect("tick");
        let statuses: Vec<String> = sqlx::query_scalar(
            "SELECT status::text FROM notification_deliveries
             WHERE kind = 'assignment' ORDER BY created_at, id",
        )
        .fetch_all(&db.pool)
        .await
        .expect("assignment deliveries");
        let chat: i64 = sqlx::query_scalar("SELECT telegram_chat_id FROM users WHERE id = $1")
            .bind(household.owner_id)
            .fetch_one(&db.pool)
            .await
            .expect("chat");
        (
            stats.delivered,
            statuses,
            outbox_messages(&db.pool, chat).await,
        )
    }

    let (one_delivered, one_statuses, one_messages) = outcome(1).await;
    assert_eq!(one_delivered, 1);
    assert_eq!(one_statuses, vec!["sent"]);
    assert_eq!(one_messages.len(), 1);
    assert!(!one_messages[0].contains("Пока были тихие часы"));

    let (two_delivered, two_statuses, two_messages) = outcome(2).await;
    assert_eq!(two_delivered, 1, "two assignments come in one batch");
    assert_eq!(two_statuses, vec!["sent", "sent"]);
    assert_eq!(two_messages.len(), 1);
    assert!(two_messages[0].contains("Пока были тихие часы"));
}

async fn create_empty_batch(pool: &PgPool, household: &TestHousehold, now: DateTime<Utc>) -> Uuid {
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retention_purges_old_batches_without_pending_deliveries() {
    let db = support::isolated_db().await;
    let household = support::create_household(&db.pool).await;
    let now = Utc::now();
    let old = now - Duration::days(60);
    let mut conn = db.pool.acquire().await.expect("conn");
    let old_sent = create_empty_batch(&db.pool, &household, now).await;
    let old_pending = create_empty_batch(&db.pool, &household, now).await;
    let recent_sent = create_empty_batch(&db.pool, &household, now).await;
    sqlx::query("UPDATE notification_batches SET updated_at = $2 WHERE id = $1")
        .bind(old_sent)
        .bind(old)
        .execute(&mut *conn)
        .await
        .expect("old sent time");
    sqlx::query("UPDATE notification_batches SET updated_at = $2 WHERE id = $1")
        .bind(old_pending)
        .bind(old)
        .execute(&mut *conn)
        .await
        .expect("old pending time");
    for (id, status, batch_id) in [
        ("old-sent", "sent", old_sent),
        ("old-pending", "pending", old_pending),
        ("recent-sent", "sent", recent_sent),
    ] {
        let at = if id.starts_with("recent") { now } else { old };
        sqlx::query(
            "INSERT INTO notification_deliveries (
                 id, household_id, user_id, item_id, occurrence_key, kind,
                 status, scheduled_at, batch_id, created_at, updated_at
             )
             VALUES ($1, $2, $3, NULL, $4, 'reminder', $5::notification_status,
                     $6, $7, $6, $6)",
        )
        .bind(Uuid::now_v7())
        .bind(household.household_id)
        .bind(household.owner_id)
        .bind(id)
        .bind(status)
        .bind(at)
        .bind(batch_id)
        .execute(&mut *conn)
        .await
        .expect("delivery");
    }
    drop(conn);

    let config = RetentionConfig {
        updates_days: 30,
        drafts_days: 7,
        llm_runs_days: 30,
        events_days: 90,
        deleted_days: 30,
        outbox_days: 30,
        audit_days: 180,
    };
    let stats = retention::run(&db.pool, &config, now)
        .await
        .expect("retention");
    assert_eq!(stats.purged_notification_batches, 1);
    let left: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM notification_batches ORDER BY id")
        .fetch_all(&db.pool)
        .await
        .expect("batches");
    assert_eq!(left.len(), 2);
    assert!(!left.contains(&old_sent));
    assert!(left.contains(&old_pending));
    assert!(left.contains(&recent_sent));
}

async fn open_notifications(bot: &mut Harness) {
    bot.press("Настройки").await;
    bot.press("Уведомления").await;
}

async fn quiet_settings(pool: &PgPool, user_id: Uuid, digest: bool) {
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

fn local_today(bot: &Harness, timezone: Tz) -> NaiveDate {
    bot.now.with_timezone(&timezone).date_naive()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recipient_advisory_lock_blocks_the_tick_until_released() {
    let db = support::isolated_db().await;
    let household = support::create_household(&db.pool).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = day();
    set_notifications(
        &db.pool,
        household.owner_id,
        json!({"digest_enabled": false, "quiet_hours_enabled": false}),
    )
    .await;
    let due = local_utc(moscow, day, 9, 0);
    let release_at = local_utc(moscow, day, 8, 0);
    for title in ["Первая", "Вторая"] {
        let item = create_task(&db.pool, &household, title, due, None).await;
        deferred_reminder(&db.pool, &household, item.id, due, release_at).await;
    }
    let chat = sqlx::query_scalar("SELECT telegram_chat_id FROM users WHERE id = $1")
        .bind(household.owner_id)
        .fetch_one(&db.pool)
        .await
        .expect("chat");

    // Ключ совпадает с `scheduler::try_recipient_lock`: последние 8 байт UUID.
    let key = i64::from_be_bytes(
        household.owner_id.as_bytes()[8..]
            .try_into()
            .expect("eight bytes"),
    );
    let mut holder = db.pool.acquire().await.expect("holder connection");
    let locked: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
        .bind(key)
        .fetch_one(&mut *holder)
        .await
        .expect("lock");
    assert!(locked);

    assert_eq!(
        scheduler(&db.pool)
            .tick(release_at)
            .await
            .expect("locked tick")
            .delivered,
        0
    );
    let pending: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM notification_deliveries WHERE status = 'pending' AND deferred",
    )
    .fetch_one(&db.pool)
    .await
    .expect("pending");
    assert_eq!(pending, 2);
    assert!(outbox_messages(&db.pool, chat).await.is_empty());

    sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(key)
        .execute(&mut *holder)
        .await
        .expect("unlock");
    assert_eq!(
        scheduler(&db.pool)
            .tick(release_at)
            .await
            .expect("released tick")
            .delivered,
        1
    );
    let sent = outbox_messages(&db.pool, chat).await;
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert!(sent[0].contains("Пока были тихие часы"), "{sent:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disabling_quiet_hours_releases_three_notifications_and_the_digest() {
    let db = support::isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    let household = household_for(&db.pool, bot.chat).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = local_today(&bot, moscow);
    let at = local_utc(moscow, day, 6, 0);
    bot.advance(at - bot.now);
    quiet_settings(&db.pool, household.owner_id, true).await;

    let due = local_utc(moscow, day, 9, 0);
    let release_at = local_utc(moscow, day, 8, 0);
    for title in ["Первая", "Вторая", "Третья"] {
        let item = create_task(&db.pool, &household, title, due, None).await;
        deferred_reminder(&db.pool, &household, item.id, due, release_at).await;
    }
    insert_digest_delivery(
        &db.pool,
        household.household_id,
        household.owner_id,
        &digest_occurrence_key(day),
        release_at,
        true,
    )
    .await;

    open_notifications(&mut bot).await;
    bot.press("Тихие часы: вкл").await;

    let screen = bot.screen();
    assert!(screen.contains("Тихие часы выключены"), "{screen}");
    assert!(screen.contains("Накопилось 3 уведомления"), "{screen}");

    let messages = bot.fake.messages(bot.chat);
    let (batch_id, batch) = messages
        .iter()
        .find(|(_, html)| html.contains("Пока были тихие часы"))
        .expect("batch message");
    assert_eq!(block_count(batch, "⏰ напоминание"), 3, "{batch}");
    let (digest_id, _digest) = messages
        .iter()
        .find(|(_, html)| html.contains("Доброе утро"))
        .expect("digest message");
    assert!(*batch_id < *digest_id, "digest comes after the batch");

    let sent_before = bot.fake.sent().len();
    let stats = tick_and_deliver(&mut bot, &db.pool, release_at).await;
    assert_eq!(stats.delivered, 0, "{stats:?}");
    assert_eq!(
        bot.fake.sent().len(),
        sent_before,
        "nothing else should be sent at the fixed release time"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_deferred_notification_is_released_as_an_ordinary_message() {
    let db = support::isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    let household = household_for(&db.pool, bot.chat).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = local_today(&bot, moscow);
    let at = local_utc(moscow, day, 6, 0);
    bot.advance(at - bot.now);
    quiet_settings(&db.pool, household.owner_id, false).await;

    let due = local_utc(moscow, day, 9, 0);
    let release_at = local_utc(moscow, day, 8, 0);
    let item = create_task(&db.pool, &household, "Одно напоминание", due, None).await;
    deferred_reminder(&db.pool, &household, item.id, due, release_at).await;

    open_notifications(&mut bot).await;
    bot.press("Тихие часы: вкл").await;

    let screen = bot.screen();
    assert!(screen.contains("Накопилось 1 уведомление"), "{screen}");
    let (_, notice) = bot.latest();
    assert!(notice.contains("Одно напоминание"), "{notice}");
    assert!(!notice.contains("Пока были тихие часы"), "{notice}");
    let labels: Vec<String> = buttons(&notice)
        .into_iter()
        .map(|(label, _)| label)
        .collect();
    for label in ["Готово", "Отложить на час", "Открыть"] {
        assert!(
            labels.iter().any(|item| item.contains(label)),
            "{label}: {labels:?}"
        );
    }

    // Повторное включение и выключение не присылает ничего.
    let sent_after_release = bot.fake.sent().len();
    bot.press("Тихие часы: выкл").await;
    bot.press("Тихие часы: вкл").await;
    assert_eq!(bot.fake.sent().len(), sent_after_release);
    assert!(!bot.screen().contains("Накопилось"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disabling_quiet_hours_with_nothing_deferred_sends_no_notice() {
    let db = support::isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    let household = household_for(&db.pool, bot.chat).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = local_today(&bot, moscow);
    let at = local_utc(moscow, day, 6, 0);
    bot.advance(at - bot.now);
    quiet_settings(&db.pool, household.owner_id, false).await;

    open_notifications(&mut bot).await;
    let sent_before = bot.fake.sent().len();
    bot.press("Тихие часы: вкл").await;

    assert!(!bot.screen().contains("Накопилось"));
    assert_eq!(bot.fake.sent().len(), sent_before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn moving_quiet_end_earlier_releases_deferred_immediately() {
    let db = support::isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    let household = household_for(&db.pool, bot.chat).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = local_today(&bot, moscow);
    let at = local_utc(moscow, day, 6, 0);
    bot.advance(at - bot.now);
    quiet_settings(&db.pool, household.owner_id, false).await;

    let due = local_utc(moscow, day, 9, 0);
    let release_at = local_utc(moscow, day, 8, 0);
    for title in ["Первая", "Вторая"] {
        let item = create_task(&db.pool, &household, title, due, None).await;
        deferred_reminder(&db.pool, &household, item.id, due, release_at).await;
    }

    open_notifications(&mut bot).await;
    bot.press("Конец тихих").await;
    bot.text("05:00").await;

    let screen = bot.screen();
    assert!(screen.contains("Тихие часы: 22:00–05:00"), "{screen}");
    assert!(screen.contains("Накопилось 2 уведомления"), "{screen}");
    let messages = bot.fake.messages(bot.chat);
    let batch = messages
        .iter()
        .find(|(_, html)| html.contains("Пока были тихие часы"))
        .expect("batch message");
    assert_eq!(block_count(&batch.1, "⏰ напоминание"), 2, "{}", batch.1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn moving_quiet_end_later_keeps_deferred_until_the_new_end() {
    let db = support::isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    let household = household_for(&db.pool, bot.chat).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = local_today(&bot, moscow);
    let at = local_utc(moscow, day, 6, 0);
    bot.advance(at - bot.now);
    quiet_settings(&db.pool, household.owner_id, false).await;

    let due = local_utc(moscow, day, 10, 0);
    let release_at = local_utc(moscow, day, 8, 0);
    for title in ["Первая", "Вторая"] {
        let item = create_task(&db.pool, &household, title, due, None).await;
        deferred_reminder(&db.pool, &household, item.id, due, release_at).await;
    }

    open_notifications(&mut bot).await;
    let sent_before = bot.fake.sent().len();
    bot.press("Конец тихих").await;
    bot.text("09:00").await;

    assert!(!bot.screen().contains("Накопилось"));
    assert_eq!(bot.fake.sent().len(), sent_before);
    let scheduled: Vec<DateTime<Utc>> = sqlx::query_scalar(
        "SELECT scheduled_at FROM notification_deliveries
         WHERE status = 'pending' AND deferred ORDER BY scheduled_at",
    )
    .fetch_all(&db.pool)
    .await
    .expect("scheduled");
    assert!(
        scheduled
            .iter()
            .all(|at| *at == local_utc(moscow, day, 9, 0)),
        "{scheduled:?}"
    );

    assert_eq!(
        tick_and_deliver(&mut bot, &db.pool, local_utc(moscow, day, 8, 0))
            .await
            .delivered,
        0
    );
    assert_eq!(
        tick_and_deliver(&mut bot, &db.pool, local_utc(moscow, day, 9, 0))
            .await
            .delivered,
        1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timezone_change_that_leaves_quiet_hours_releases_deferred() {
    let db = support::isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    let household = household_for(&db.pool, bot.chat).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = local_today(&bot, moscow);
    let at = local_utc(moscow, day, 6, 0);
    bot.advance(at - bot.now);
    quiet_settings(&db.pool, household.owner_id, false).await;

    let due = local_utc(moscow, day, 9, 0);
    let release_at = local_utc(moscow, day, 8, 0);
    for title in ["Первая", "Вторая"] {
        let item = create_task(&db.pool, &household, title, due, None).await;
        deferred_reminder(&db.pool, &household, item.id, due, release_at).await;
    }

    bot.press("Настройки").await;
    bot.press("Время").await;
    // 06:00 в Москве — 13:00 во Владивостоке: по новым настройкам уже не тихо.
    bot.press("Asia/Vladivostok").await;

    let screen = bot.screen();
    assert!(screen.contains("Часовой пояс обновлён"), "{screen}");
    assert!(screen.contains("Накопилось 2 уведомления"), "{screen}");
    let messages = bot.fake.messages(bot.chat);
    let batch = messages
        .iter()
        .find(|(_, html)| html.contains("Пока были тихие часы"))
        .expect("batch message");
    assert_eq!(block_count(&batch.1, "⏰ напоминание"), 2, "{}", batch.1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manual_release_does_not_touch_another_participant() {
    let db = support::isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    let household = household_for(&db.pool, bot.chat).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = local_today(&bot, moscow);
    let at = local_utc(moscow, day, 6, 0);
    bot.advance(at - bot.now);
    quiet_settings(&db.pool, household.owner_id, false).await;

    let extra_id = Uuid::now_v7();
    let extra_chat = support::next_telegram_id();
    sqlx::query(
        "INSERT INTO users (
             id, household_id, telegram_user_id, telegram_chat_id, display_name,
             role, settings, created_at, updated_at
         )
         VALUES ($1, $2, $3, $3, 'Второй', 'member', '{}'::jsonb, now(), now())",
    )
    .bind(extra_id)
    .bind(household.household_id)
    .bind(extra_chat)
    .execute(&db.pool)
    .await
    .expect("extra member");

    let due = local_utc(moscow, day, 9, 0);
    let release_at = local_utc(moscow, day, 8, 0);
    let owner_item = create_task(&db.pool, &household, "Владельца", due, None).await;
    let extra_item = create_task(&db.pool, &household, "Второго", due, None).await;
    deferred_reminder(&db.pool, &household, owner_item.id, due, release_at).await;
    insert_delivery(
        &db.pool,
        household.household_id,
        extra_id,
        extra_item.id,
        "reminder",
        &tasklerq::jobs::scheduler::occurrence_key(due),
        release_at,
        true,
    )
    .await;

    open_notifications(&mut bot).await;
    bot.press("Тихие часы: вкл").await;

    assert!(bot.screen().contains("Накопилось 1 уведомление"));
    assert!(
        outbox_messages(&db.pool, extra_chat).await.is_empty(),
        "another participant must keep their deferred notification"
    );
    let extra_pending: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM notification_deliveries
         WHERE user_id = $1 AND status = 'pending' AND deferred",
    )
    .bind(extra_id)
    .fetch_one(&db.pool)
    .await
    .expect("extra pending");
    assert_eq!(extra_pending, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manual_release_skips_while_the_scheduler_holds_the_recipient_lock() {
    let db = support::isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    let household = household_for(&db.pool, bot.chat).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = local_today(&bot, moscow);
    let at = local_utc(moscow, day, 6, 0);
    bot.advance(at - bot.now);
    quiet_settings(&db.pool, household.owner_id, false).await;

    let due = local_utc(moscow, day, 9, 0);
    let release_at = local_utc(moscow, day, 8, 0);
    for title in ["Первая", "Вторая"] {
        let item = create_task(&db.pool, &household, title, due, None).await;
        deferred_reminder(&db.pool, &household, item.id, due, release_at).await;
    }

    let key = i64::from_be_bytes(
        household.owner_id.as_bytes()[8..]
            .try_into()
            .expect("eight bytes"),
    );
    let mut holder = db.pool.acquire().await.expect("holder connection");
    let locked: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
        .bind(key)
        .fetch_one(&mut *holder)
        .await
        .expect("lock");
    assert!(locked);

    open_notifications(&mut bot).await;
    let sent_before = bot.fake.sent().len();
    bot.press("Тихие часы: вкл").await;
    assert!(!bot.screen().contains("Накопилось"));
    assert_eq!(bot.fake.sent().len(), sent_before);

    sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(key)
        .execute(&mut *holder)
        .await
        .expect("unlock");
    assert_eq!(
        tick_and_deliver(&mut bot, &db.pool, release_at)
            .await
            .delivered,
        1
    );
    let messages = bot.fake.messages(bot.chat);
    assert_eq!(
        messages
            .iter()
            .filter(|(_, html)| html.contains("Пока были тихие часы"))
            .count(),
        1,
        "the scheduler sends exactly one batch"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn assignment_from_an_archived_list_keeps_its_actions_in_a_block() {
    let db = support::isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = bot.now.with_timezone(&moscow).date_naive();
    let morning = local_utc(moscow, day, 8, 0);
    let due = local_utc(moscow, day, 9, 0);
    let first = create_task(&db.pool, &household, "Первая", due, None).await;
    let second = create_task(&db.pool, &household, "Вторая", due, None).await;
    for (index, item) in [&first, &second].into_iter().enumerate() {
        insert_delivery(
            &db.pool,
            household.household_id,
            household.owner_id,
            item.id,
            "assignment",
            &format!("assignment:archived:{index}"),
            morning,
            true,
        )
        .await;
    }
    sqlx::query("UPDATE lists SET is_default = false, is_archived = true WHERE id = $1")
        .bind(household.tasks_list_id)
        .execute(&db.pool)
        .await
        .expect("archive list");

    tick_and_deliver(&mut bot, &db.pool, morning).await;
    let (message_id, html) = bot.latest();
    assert!(html.contains("Пока были тихие часы"), "{html}");
    assert!(!html.contains("Список в архиве"), "{html}");
    assert_eq!(block_count(&html, "👤 назначение"), 2, "{html}");

    let done = button_of(&html, "Готово");
    bot.press_data(&done, message_id).await;
    assert_eq!(item_status(&db.pool, first.id).await.0, "done");

    let (_, edited) = bot.latest();
    let opens: Vec<String> = buttons(&edited)
        .into_iter()
        .filter(|(label, _)| label.contains("Открыть"))
        .map(|(_, data)| data)
        .collect();
    assert_eq!(opens.len(), 1, "{edited}");
    bot.press_data(&opens[0], message_id).await;
    bot.assert_screen_contains("Вторая");
}
