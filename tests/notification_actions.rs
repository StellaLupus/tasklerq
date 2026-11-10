//! Сквозные сценарии напоминаний: доставка планировщиком и действия из
//! уведомления.

mod support;

use chrono::{DateTime, Datelike, Duration, NaiveDate, TimeZone, Timelike, Utc};
use chrono_tz::Tz;
use serde_json::json;
use sqlx::PgPool;
use support::notifications::{button_of, tick_and_deliver};
use support::telegram::{Call, Harness, buttons};
use support::{TestHousehold, create_item, isolated_db, new_item, next_telegram_id};
use tasklerq::domain::models::{Actor, Item, RecurrenceFreq, RecurrenceInput, UserRole};
use tasklerq::storage::repositories::items;
use tasklerq::storage::repositories::notifications::{self, NewDelivery, NotificationKind};
use uuid::Uuid;

async fn onboarded(pool: &PgPool) -> Harness {
    let mut bot = Harness::new(pool, next_telegram_id()).named("Alice");
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
        events_list_id: list("events").await,
    }
}

async fn set_notifications(pool: &PgPool, telegram_id: i64, value: serde_json::Value) {
    sqlx::query(
        "UPDATE users
         SET settings = jsonb_set(settings, '{notifications}', $2::jsonb, true)
         WHERE telegram_user_id = $1",
    )
    .bind(telegram_id)
    .bind(value)
    .execute(pool)
    .await
    .expect("notification settings");
}

async fn set_timezone(pool: &PgPool, telegram_id: i64, timezone: &str) {
    sqlx::query("UPDATE users SET timezone = $2 WHERE telegram_user_id = $1")
        .bind(telegram_id)
        .bind(timezone)
        .execute(pool)
        .await
        .expect("user timezone");
}

async fn quiet_off(pool: &PgPool, telegram_id: i64) {
    set_notifications(
        pool,
        telegram_id,
        json!({"digest_enabled": false, "quiet_hours_enabled": false}),
    )
    .await;
}

async fn create_task(
    pool: &PgPool,
    household: &TestHousehold,
    title: &str,
    due_at: DateTime<Utc>,
    recurrence: Option<tasklerq::domain::models::RecurrenceInput>,
) -> Item {
    let mut item = new_item(household.tasks_list_id, title);
    item.due_at = Some(due_at);
    item.assignee_user_id = Some(household.owner_id);
    item.recurrence = recurrence;
    create_item(
        pool,
        household,
        household.tasks_list_id,
        &Actor::user(household.owner_id, UserRole::Owner),
        item,
    )
    .await
}

async fn create_all_day_task(
    pool: &PgPool,
    household: &TestHousehold,
    title: &str,
    due_date: NaiveDate,
) -> Item {
    let mut item = new_item(household.tasks_list_id, title);
    item.due_date = Some(due_date);
    item.assignee_user_id = Some(household.owner_id);
    create_item(
        pool,
        household,
        household.tasks_list_id,
        &Actor::user(household.owner_id, UserRole::Owner),
        item,
    )
    .await
}

async fn create_task_without_due(pool: &PgPool, household: &TestHousehold, title: &str) -> Item {
    let mut item = new_item(household.tasks_list_id, title);
    item.assignee_user_id = Some(household.owner_id);
    create_item(
        pool,
        household,
        household.tasks_list_id,
        &Actor::user(household.owner_id, UserRole::Owner),
        item,
    )
    .await
}

async fn create_event(
    pool: &PgPool,
    household: &TestHousehold,
    title: &str,
    due_at: DateTime<Utc>,
    end_at: DateTime<Utc>,
    recurrence: Option<RecurrenceInput>,
) -> Item {
    let mut item = new_item(household.events_list_id, title);
    item.due_at = Some(due_at);
    item.end_at = Some(end_at);
    item.assignee_user_id = Some(household.owner_id);
    item.recurrence = recurrence;
    create_item(
        pool,
        household,
        household.events_list_id,
        &Actor::user(household.owner_id, UserRole::Owner),
        item,
    )
    .await
}

fn message_count(bot: &Harness, needle: &str) -> usize {
    bot.fake
        .messages(bot.chat)
        .into_iter()
        .filter(|(_, html)| html.contains(needle))
        .count()
}

fn last_toast(bot: &Harness) -> Option<String> {
    bot.fake
        .answers()
        .into_iter()
        .next_back()
        .and_then(|(_, text, _)| text)
}

async fn snooze_rows(
    pool: &PgPool,
    item_id: Uuid,
) -> Vec<(String, DateTime<Utc>, String, Option<String>)> {
    sqlx::query_as(
        "SELECT status::text, scheduled_at, occurrence_key, origin_key
         FROM notification_deliveries
         WHERE item_id = $1 AND kind = 'snooze'
         ORDER BY created_at, id",
    )
    .bind(item_id)
    .fetch_all(pool)
    .await
    .expect("snooze deliveries")
}

async fn item_status(pool: &PgPool, item_id: Uuid) -> (String, Option<DateTime<Utc>>, i32) {
    sqlx::query_as("SELECT status::text, deleted_at, version FROM items WHERE id = $1")
        .bind(item_id)
        .fetch_one(pool)
        .await
        .expect("item state")
}

async fn reminder_occurrence_key(pool: &PgPool, item_id: Uuid) -> String {
    sqlx::query_scalar(
        "SELECT occurrence_key FROM notification_deliveries
         WHERE item_id = $1 AND kind = 'reminder'
         ORDER BY created_at, id LIMIT 1",
    )
    .bind(item_id)
    .fetch_one(pool)
    .await
    .expect("reminder key")
}

async fn insert_snooze_delivery(
    pool: &PgPool,
    household: &TestHousehold,
    item_id: Uuid,
    origin_key: Option<&str>,
    deliver_at: DateTime<Utc>,
) {
    let key = notifications::snooze_key(deliver_at);
    let delivery = NewDelivery {
        household_id: household.household_id,
        user_id: household.owner_id,
        item_id: Some(item_id),
        kind: NotificationKind::Snooze,
        occurrence_key: &key,
        origin_key,
        scheduled_at: deliver_at,
    };
    let mut conn = pool.acquire().await.expect("conn");
    notifications::insert(&mut conn, &delivery, deliver_at)
        .await
        .expect("snooze delivery");
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

async fn event_times(
    pool: &PgPool,
    item_id: Uuid,
) -> (Option<DateTime<Utc>>, Option<DateTime<Utc>>, i32) {
    sqlx::query_as("SELECT due_at, end_at, version FROM items WHERE id = $1")
        .bind(item_id)
        .fetch_one(pool)
        .await
        .expect("event state")
}

async fn reminder_rows(pool: &PgPool, item_id: Uuid) -> Vec<(String, String)> {
    sqlx::query_as(
        "SELECT status::text, occurrence_key FROM notification_deliveries
         WHERE item_id = $1 AND kind = 'reminder'
         ORDER BY created_at, id",
    )
    .bind(item_id)
    .fetch_all(pool)
    .await
    .expect("reminder deliveries")
}

fn local_utc(timezone: Tz, date: NaiveDate, hour: u32, minute: u32) -> DateTime<Utc> {
    timezone
        .with_ymd_and_hms(date.year(), date.month(), date.day(), hour, minute, 0)
        .single()
        .expect("local time")
        .with_timezone(&Utc)
}

fn rounded_now() -> DateTime<Utc> {
    Utc::now()
        .with_second(0)
        .and_then(|at| at.with_nanosecond(0))
        .expect("rounded time")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scheduler_reminder_has_four_action_buttons() {
    let db = isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let due = rounded_now() + Duration::hours(3);
    let item = create_task(&db.pool, &household, "Оплатить интернет", due, None).await;

    tick_and_deliver(&mut bot, &db.pool, due - Duration::minutes(15)).await;

    let (_, html) = bot.latest();
    assert!(html.contains("⏰ Через 15 минут"), "{html}");
    assert!(html.contains("Оплатить интернет"), "{html}");
    let labels: Vec<String> = buttons(&html).into_iter().map(|(label, _)| label).collect();
    assert_eq!(labels.len(), 4, "{labels:?}");
    for expected in ["Выполнено", "Отложить на час", "Завтра в 09:00", "Открыть"]
    {
        assert!(
            labels.iter().any(|label| label.contains(expected)),
            "button `{expected}` missing: {labels:?}"
        );
    }

    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT status::text, occurrence_key FROM notification_deliveries
         WHERE item_id = $1 AND kind = 'reminder'",
    )
    .bind(item.id)
    .fetch_all(&db.pool)
    .await
    .expect("reminder deliveries");
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "sent");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn done_from_reminder_completes_once_and_repeat_does_not_change_version() {
    let db = isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let due = rounded_now() + Duration::hours(3);
    let item = create_task(&db.pool, &household, "Оплатить интернет", due, None).await;

    tick_and_deliver(&mut bot, &db.pool, due - Duration::minutes(15)).await;
    let (message_id, html) = bot.latest();
    let done = button_of(&html, "Выполнено");

    bot.press_data(&done, message_id).await;
    let (status, _, version) = item_status(&db.pool, item.id).await;
    assert_eq!(status, "done");
    assert_eq!(version, item.version + 1);
    assert_eq!(completed_events(&db.pool, item.id).await, 1);
    let (_, done_html) = bot.latest();
    assert!(done_html.contains("✅ Выполнено"), "{done_html}");
    assert!(
        buttons(&done_html)
            .iter()
            .any(|(label, _)| label.contains("Отменить"))
    );

    let calls_before = bot.fake.calls().len();
    bot.press_data(&done, message_id).await;
    assert_eq!(bot.fake.calls().len(), calls_before);
    assert_eq!(item_status(&db.pool, item.id).await.2, version);
    assert_eq!(completed_events(&db.pool, item.id).await, 1);
    assert_eq!(
        last_toast(&bot).as_deref(),
        Some("Это действие уже выполнено.")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn done_on_recurring_task_does_not_advance_a_second_occurrence() {
    let db = isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let due = rounded_now() + Duration::hours(3);
    let item = create_task(
        &db.pool,
        &household,
        "Ежедневная зарядка",
        due,
        Some(support::recurrence(RecurrenceFreq::Daily, Vec::new())),
    )
    .await;

    tick_and_deliver(&mut bot, &db.pool, due - Duration::minutes(15)).await;
    let (message_id, html) = bot.latest();
    let done = button_of(&html, "Выполнено");

    bot.press_data(&done, message_id).await;
    let (status, _, version) = item_status(&db.pool, item.id).await;
    assert_eq!(
        status, "open",
        "recurring task moves to the next occurrence"
    );
    assert_eq!(version, item.version + 1);
    assert_eq!(completed_events(&db.pool, item.id).await, 1);

    bot.press_data(&done, message_id).await;
    assert_eq!(item_status(&db.pool, item.id).await.2, version);
    assert_eq!(completed_events(&db.pool, item.id).await, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snooze_one_hour_creates_one_delivery_and_keeps_the_due_date() {
    let db = isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let due = rounded_now() + Duration::hours(3);
    let item = create_task(&db.pool, &household, "Позвонить маме", due, None).await;

    tick_and_deliver(&mut bot, &db.pool, due - Duration::minutes(15)).await;
    let (message_id, html) = bot.latest();
    let snooze = button_of(&html, "на час");
    let pressed_at = bot.now;

    bot.press_data(&snooze, message_id).await;
    let rows = snooze_rows(&db.pool, item.id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "pending");
    let expected = pressed_at + Duration::hours(1);
    assert!(
        (rows[0].1 - expected).num_seconds().abs() <= 5,
        "snooze at {}, expected {}",
        rows[0].1,
        expected
    );
    assert!(rows[0].2.contains("snooze:"), "{rows:?}");
    let stored_due: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT due_at FROM items WHERE id = $1")
            .bind(item.id)
            .fetch_one(&db.pool)
            .await
            .expect("due");
    assert_eq!(stored_due, Some(due), "snooze must not move the due date");
    assert_eq!(message_count(&bot, "Позвонить маме"), 1);
    assert!(bot.latest().1.contains("😴 Напомню"), "{}", bot.latest().1);

    // Повторное нажатие той же кнопки не создаёт вторую доставку.
    bot.press_data(&snooze, message_id).await;
    assert_eq!(snooze_rows(&db.pool, item.id).await.len(), 1);

    // За минуту до срока — тишина.
    tick_and_deliver(&mut bot, &db.pool, expected - Duration::minutes(1)).await;
    assert_eq!(message_count(&bot, "Позвонить маме"), 1);

    // В момент snooze приходит новое напоминание с кнопками.
    tick_and_deliver(&mut bot, &db.pool, expected).await;
    assert_eq!(message_count(&bot, "Позвонить маме"), 2);
    let (_, reminder) = bot.latest();
    assert_eq!(buttons(&reminder).len(), 4, "{reminder}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deferred_reminder_shows_the_actual_time_left() {
    let db = isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    set_timezone(&db.pool, bot.chat, "Europe/Moscow").await;
    set_notifications(
        &db.pool,
        bot.chat,
        json!({
            "digest_enabled": false,
            "lead_minutes": 60,
            "quiet_hours_enabled": true,
            "quiet_start": "22:00",
            "quiet_end": "08:00"
        }),
    )
    .await;
    let household = household_for(&db.pool, bot.chat).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = bot.now.with_timezone(&moscow).date_naive() + Duration::days(1);
    let due = local_utc(moscow, day, 8, 10);
    create_task(&db.pool, &household, "Позвонить врачу", due, None).await;

    // Напоминание должно было уйти в 07:10, но тихие часы перенесли его на 08:00.
    tick_and_deliver(&mut bot, &db.pool, local_utc(moscow, day, 8, 0)).await;

    let (_, html) = bot.latest();
    assert!(html.contains("⏰ Через 10 минут"), "{html}");
    assert!(!html.contains("Через 60 минут"), "{html}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snooze_tomorrow_uses_the_recipient_local_nine() {
    let db = isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    set_timezone(&db.pool, bot.chat, "Asia/Vladivostok").await;
    let household = household_for(&db.pool, bot.chat).await;
    let vladivostok = chrono_tz::Asia::Vladivostok;
    let due = rounded_now() + Duration::hours(3);
    let item = create_task(&db.pool, &household, "Сдать отчёт", due, None).await;

    tick_and_deliver(&mut bot, &db.pool, due - Duration::minutes(15)).await;
    let (message_id, html) = bot.latest();
    let tomorrow = button_of(&html, "Завтра в 09:00");
    let pressed_at = bot.now;
    bot.press_data(&tomorrow, message_id).await;

    let local_date = pressed_at.with_timezone(&vladivostok).date_naive() + Duration::days(1);
    let expected = local_utc(vladivostok, local_date, 9, 0);
    let rows = snooze_rows(&db.pool, item.id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].1, expected, "{rows:?}");
    assert_ne!(
        expected.with_timezone(&Utc).hour(),
        9,
        "timezone must differ from UTC"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snooze_at_night_is_delivered_at_the_end_of_quiet_hours() {
    let db = isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    set_timezone(&db.pool, bot.chat, "Europe/Moscow").await;
    set_notifications(
        &db.pool,
        bot.chat,
        json!({
            "digest_enabled": false,
            "quiet_hours_enabled": true,
            "quiet_start": "22:00",
            "quiet_end": "08:00"
        }),
    )
    .await;
    let household = household_for(&db.pool, bot.chat).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = bot.now.with_timezone(&moscow).date_naive() + Duration::days(1);
    let due = local_utc(moscow, day, 21, 45);
    let item = create_task(&db.pool, &household, "Выключить свет", due, None).await;

    tick_and_deliver(&mut bot, &db.pool, local_utc(moscow, day, 21, 30)).await;
    let (message_id, html) = bot.latest();
    let snooze = button_of(&html, "на час");
    bot.press_data(&snooze, message_id).await;

    // 22:30 попадает в тихие часы: доставка переносится на 08:00.
    tick_and_deliver(&mut bot, &db.pool, local_utc(moscow, day, 22, 30)).await;
    assert_eq!(message_count(&bot, "Выключить свет"), 1);
    let rows = snooze_rows(&db.pool, item.id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "pending");
    assert_eq!(rows[0].1, local_utc(moscow, day + Duration::days(1), 8, 0));

    tick_and_deliver(
        &mut bot,
        &db.pool,
        local_utc(moscow, day + Duration::days(1), 8, 0),
    )
    .await;
    assert_eq!(message_count(&bot, "Выключить свет"), 2);
    assert_eq!(buttons(&bot.latest().1).len(), 4);
    assert_eq!(snooze_rows(&db.pool, item.id).await[0].0, "sent");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disabling_reminders_cancels_a_snoozed_delivery() {
    let db = isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let due = rounded_now() + Duration::hours(3);
    let item = create_task(&db.pool, &household, "Позвонить маме", due, None).await;

    tick_and_deliver(&mut bot, &db.pool, due - Duration::minutes(15)).await;
    let (message_id, html) = bot.latest();
    let snooze = button_of(&html, "на час");
    bot.press_data(&snooze, message_id).await;
    let deliver_at = snooze_rows(&db.pool, item.id).await[0].1;

    // Snooze — явное действие пользователя, но выключение напоминаний
    // отменяет и его: новых напоминаний быть не должно.
    set_notifications(
        &db.pool,
        bot.chat,
        json!({
            "digest_enabled": false,
            "quiet_hours_enabled": false,
            "reminders_enabled": false
        }),
    )
    .await;

    let stats = tick_and_deliver(&mut bot, &db.pool, deliver_at).await;
    assert!(stats.cancelled >= 1, "{stats:?}");
    let rows = snooze_rows(&db.pool, item.id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "cancelled");
    assert_eq!(message_count(&bot, "Позвонить маме"), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completing_the_task_cancels_the_snoozed_delivery() {
    let db = isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let due = rounded_now() + Duration::hours(3);
    let item = create_task(&db.pool, &household, "Купить билеты", due, None).await;

    tick_and_deliver(&mut bot, &db.pool, due - Duration::minutes(15)).await;
    let (message_id, html) = bot.latest();
    let snooze = button_of(&html, "на час");
    bot.press_data(&snooze, message_id).await;
    let deliver_at = snooze_rows(&db.pool, item.id).await[0].1;

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

    let stats = tick_and_deliver(&mut bot, &db.pool, deliver_at).await;
    assert!(stats.cancelled >= 1, "{stats:?}");
    let rows = snooze_rows(&db.pool, item.id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "cancelled");
    assert_eq!(message_count(&bot, "Купить билеты"), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actions_on_a_deleted_task_report_unavailable_and_do_nothing() {
    let db = isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let due = rounded_now() + Duration::hours(3);

    for label in ["Выполнено", "на час", "Завтра в 09:00", "Открыть"] {
        let item = create_task(
            &db.pool,
            &household,
            &format!("Удалённая задача {label}"),
            due,
            None,
        )
        .await;
        tick_and_deliver(&mut bot, &db.pool, due - Duration::minutes(15)).await;
        let (message_id, html) = bot.latest();
        let data = button_of(&html, label);

        let mut conn = db.pool.acquire().await.expect("conn");
        items::soft_delete(
            &mut conn,
            household.household_id,
            &household.owner(),
            item.id,
            bot.now,
        )
        .await
        .expect("delete task");

        let before = bot.fake.calls().len();
        bot.press_data(&data, message_id).await;
        let toast = last_toast(&bot).unwrap_or_default();
        assert!(
            toast.contains("больше недоступна"),
            "button `{label}`: {toast:?}"
        );
        assert!(
            bot.latest().1.contains("Задача больше недоступна"),
            "button `{label}`: {}",
            bot.latest().1
        );
        assert_eq!(
            item_status(&db.pool, item.id).await.0,
            "open",
            "action must not complete a deleted task"
        );
        assert!(
            snooze_rows(&db.pool, item.id).await.is_empty(),
            "button `{label}` must not create a snooze"
        );
        assert!(
            !bot.fake.calls()[before..]
                .iter()
                .any(|call| matches!(call, Call::SendRich { .. })),
            "button `{label}` must not open a new screen"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn done_and_snooze_on_a_completed_task_change_nothing() {
    let db = isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let due = rounded_now() + Duration::hours(3);
    let mut conn = db.pool.acquire().await.expect("conn");

    // «Отложить» по уже выполненной задаче не создаёт доставку.
    let snoozed = create_task(&db.pool, &household, "Выполненная задача", due, None).await;
    tick_and_deliver(&mut bot, &db.pool, due - Duration::minutes(15)).await;
    let (message_id, html) = bot.latest();
    let snooze = button_of(&html, "на час");
    items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        snoozed.id,
        None,
        "Europe/Moscow",
        bot.now,
    )
    .await
    .expect("complete snoozed task");
    let version = item_status(&db.pool, snoozed.id).await.2;
    bot.press_data(&snooze, message_id).await;
    assert_eq!(last_toast(&bot).as_deref(), Some("Задача уже выполнена"));
    assert!(snooze_rows(&db.pool, snoozed.id).await.is_empty());
    assert!(bot.latest().1.contains("✅ Выполнено"));
    assert_eq!(item_status(&db.pool, snoozed.id).await.2, version);

    // «Выполнено» по уже выполненной задаче не завершает её второй раз.
    let completed = create_task(&db.pool, &household, "Ещё одна задача", due, None).await;
    tick_and_deliver(&mut bot, &db.pool, due - Duration::minutes(15)).await;
    let (message_id, html) = bot.latest();
    let done = button_of(&html, "Выполнено");
    items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        completed.id,
        None,
        "Europe/Moscow",
        bot.now,
    )
    .await
    .expect("complete task directly");
    let version = item_status(&db.pool, completed.id).await.2;
    bot.press_data(&done, message_id).await;
    assert_eq!(item_status(&db.pool, completed.id).await.2, version);
    assert_eq!(completed_events(&db.pool, completed.id).await, 1);
    assert!(bot.latest().1.contains("✅ Выполнено"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn open_from_reminder_shows_the_task_card() {
    let db = isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let due = rounded_now() + Duration::hours(3);
    create_task(&db.pool, &household, "Позвонить маме", due, None).await;

    tick_and_deliver(&mut bot, &db.pool, due - Duration::minutes(15)).await;
    let (message_id, html) = bot.latest();
    let open = button_of(&html, "Открыть");
    bot.press_data(&open, message_id).await;

    bot.assert_screen_contains("Позвонить маме");
    bot.assert_screen_contains("Список: Задачи");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snooze_records_the_current_occurrence_as_origin() {
    let db = isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let due = rounded_now() + Duration::hours(3);
    let timed = create_task(&db.pool, &household, "Позвонить маме", due, None).await;

    tick_and_deliver(&mut bot, &db.pool, due - Duration::minutes(15)).await;
    let (message_id, html) = bot.latest();
    let snooze = button_of(&html, "на час");
    bot.press_data(&snooze, message_id).await;

    let reminder_key = reminder_occurrence_key(&db.pool, timed.id).await;
    assert_eq!(
        snooze_rows(&db.pool, timed.id).await[0].3.as_deref(),
        Some(reminder_key.as_str()),
        "a timed snooze must be bound to the reminder occurrence"
    );

    let moscow = chrono_tz::Europe::Moscow;
    let day = bot.now.with_timezone(&moscow).date_naive();
    bot.set_now(local_utc(moscow, day, 8, 0));
    let all_day = create_all_day_task(&db.pool, &household, "Вынести мусор", day).await;
    tick_and_deliver(&mut bot, &db.pool, local_utc(moscow, day, 9, 0)).await;
    let (message_id, html) = bot.latest();
    let snooze = button_of(&html, "на час");
    bot.press_data(&snooze, message_id).await;

    let reminder_key = reminder_occurrence_key(&db.pool, all_day.id).await;
    assert_eq!(reminder_key, format!("date:{day}"));
    assert_eq!(
        snooze_rows(&db.pool, all_day.id).await[0].3.as_deref(),
        Some(reminder_key.as_str()),
        "an all-day snooze must be bound to the reminder occurrence"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn changed_due_cancels_the_snooze_and_the_new_reminder_fires() {
    let db = isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = bot.now.with_timezone(&moscow).date_naive();
    bot.set_now(local_utc(moscow, day, 8, 0));
    let item = create_all_day_task(&db.pool, &household, "Оплатить интернет", day).await;

    tick_and_deliver(&mut bot, &db.pool, local_utc(moscow, day, 9, 0)).await;
    let (message_id, html) = bot.latest();
    assert!(html.contains("⏰ Пора"), "{html}");
    let snooze = button_of(&html, "на час");
    bot.press_data(&snooze, message_id).await;
    let snooze_at = snooze_rows(&db.pool, item.id).await[0].1;

    let new_due = local_utc(moscow, day, 15, 0);
    sqlx::query(
        "UPDATE items SET due_at = $2, due_date = NULL, version = version + 1 WHERE id = $1",
    )
    .bind(item.id)
    .bind(new_due)
    .execute(&db.pool)
    .await
    .expect("move due");

    let stats = tick_and_deliver(&mut bot, &db.pool, snooze_at + Duration::minutes(5)).await;
    assert!(stats.cancelled >= 1, "{stats:?}");
    let rows = snooze_rows(&db.pool, item.id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "cancelled", "{rows:?}");
    assert_eq!(message_count(&bot, "Оплатить интернет"), 1);

    // Напоминание о новом сроке приходит за 15 минут до него.
    tick_and_deliver(&mut bot, &db.pool, new_due - Duration::minutes(15)).await;
    assert_eq!(message_count(&bot, "Оплатить интернет"), 2);
    assert!(
        bot.latest().1.contains("⏰ Через 15 минут"),
        "{}",
        bot.latest().1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn removed_due_cancels_the_snooze() {
    let db = isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let due = rounded_now() + Duration::hours(3);
    let item = create_task(&db.pool, &household, "Позвонить маме", due, None).await;

    tick_and_deliver(&mut bot, &db.pool, due - Duration::minutes(15)).await;
    let (message_id, html) = bot.latest();
    let snooze = button_of(&html, "на час");
    bot.press_data(&snooze, message_id).await;
    let snooze_at = snooze_rows(&db.pool, item.id).await[0].1;

    sqlx::query(
        "UPDATE items SET due_at = NULL, due_date = NULL, version = version + 1 WHERE id = $1",
    )
    .bind(item.id)
    .execute(&db.pool)
    .await
    .expect("remove due");

    let stats = tick_and_deliver(&mut bot, &db.pool, snooze_at).await;
    assert!(stats.cancelled >= 1, "{stats:?}");
    let rows = snooze_rows(&db.pool, item.id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "cancelled", "{rows:?}");
    assert_eq!(message_count(&bot, "Позвонить маме"), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completing_a_recurring_task_cancels_the_snooze_of_the_previous_occurrence() {
    let db = isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let due = rounded_now() + Duration::hours(3);
    let item = create_task(
        &db.pool,
        &household,
        "Ежедневная зарядка",
        due,
        Some(support::recurrence(RecurrenceFreq::Daily, Vec::new())),
    )
    .await;

    tick_and_deliver(&mut bot, &db.pool, due - Duration::minutes(15)).await;
    let (message_id, html) = bot.latest();
    let snooze = button_of(&html, "на час");
    bot.press_data(&snooze, message_id).await;
    let snooze_at = snooze_rows(&db.pool, item.id).await[0].1;

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
    .expect("complete recurring task");
    let next_due: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT due_at FROM items WHERE id = $1")
            .bind(item.id)
            .fetch_one(&db.pool)
            .await
            .expect("next due");
    let next_due = next_due.expect("recurring task keeps a due");
    assert!(next_due > due);

    let stats = tick_and_deliver(&mut bot, &db.pool, snooze_at).await;
    assert!(stats.cancelled >= 1, "{stats:?}");
    let rows = snooze_rows(&db.pool, item.id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "cancelled", "{rows:?}");
    assert_eq!(message_count(&bot, "Ежедневная зарядка"), 1);

    tick_and_deliver(&mut bot, &db.pool, next_due - Duration::minutes(15)).await;
    assert_eq!(message_count(&bot, "Ежедневная зарядка"), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_snooze_of_a_task_without_due_is_delivered() {
    let db = isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let item = create_task_without_due(&db.pool, &household, "Купить билеты").await;
    let deliver_at = rounded_now() + Duration::hours(1);
    insert_snooze_delivery(&db.pool, &household, item.id, Some("no-due"), deliver_at).await;

    tick_and_deliver(&mut bot, &db.pool, deliver_at).await;

    let rows = snooze_rows(&db.pool, item.id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "sent", "{rows:?}");
    assert_eq!(message_count(&bot, "Купить билеты"), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_snooze_without_due_is_cancelled_when_the_due_appears() {
    let db = isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let item = create_task_without_due(&db.pool, &household, "Купить билеты").await;
    let deliver_at = rounded_now() + Duration::hours(1);
    let due = deliver_at + Duration::hours(2);
    insert_snooze_delivery(&db.pool, &household, item.id, Some("no-due"), deliver_at).await;
    sqlx::query("UPDATE items SET due_at = $2, version = version + 1 WHERE id = $1")
        .bind(item.id)
        .bind(due)
        .execute(&db.pool)
        .await
        .expect("set due");

    let stats = tick_and_deliver(&mut bot, &db.pool, deliver_at).await;
    assert!(stats.cancelled >= 1, "{stats:?}");
    let rows = snooze_rows(&db.pool, item.id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "cancelled", "{rows:?}");
    assert_eq!(message_count(&bot, "Купить билеты"), 0);

    tick_and_deliver(&mut bot, &db.pool, due - Duration::minutes(15)).await;
    assert_eq!(message_count(&bot, "Купить билеты"), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snooze_until_morning_survives_midnight_at_the_same_due() {
    let db = isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = bot.now.with_timezone(&moscow).date_naive();
    bot.set_now(local_utc(moscow, day, 21, 30));
    let due = local_utc(moscow, day, 22, 0);
    let item = create_task(&db.pool, &household, "Выключить свет", due, None).await;

    tick_and_deliver(&mut bot, &db.pool, local_utc(moscow, day, 21, 45)).await;
    let (message_id, html) = bot.latest();
    let tomorrow = button_of(&html, "Завтра в 09:00");
    bot.press_data(&tomorrow, message_id).await;

    let next_morning = local_utc(moscow, day + Duration::days(1), 9, 0);
    let rows = snooze_rows(&db.pool, item.id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].1, next_morning);

    // Полночь меняет локальную дату, но не вхождение срока.
    tick_and_deliver(
        &mut bot,
        &db.pool,
        local_utc(moscow, day + Duration::days(1), 0, 1),
    )
    .await;
    assert_eq!(snooze_rows(&db.pool, item.id).await[0].0, "pending");
    assert_eq!(message_count(&bot, "Выключить свет"), 1);

    tick_and_deliver(&mut bot, &db.pool, next_morning).await;
    let rows = snooze_rows(&db.pool, item.id).await;
    assert_eq!(rows[0].0, "sent", "{rows:?}");
    assert_eq!(message_count(&bot, "Выключить свет"), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snooze_until_morning_survives_midnight_for_an_all_day_task() {
    let db = isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    set_timezone(&db.pool, bot.chat, "Europe/Moscow").await;
    set_notifications(
        &db.pool,
        bot.chat,
        json!({
            "digest_enabled": false,
            "quiet_hours_enabled": false,
            "all_day_time": "21:00"
        }),
    )
    .await;
    let household = household_for(&db.pool, bot.chat).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = bot.now.with_timezone(&moscow).date_naive();
    bot.set_now(local_utc(moscow, day, 21, 0));
    let item = create_all_day_task(&db.pool, &household, "Вынести мусор", day).await;

    tick_and_deliver(&mut bot, &db.pool, local_utc(moscow, day, 21, 0)).await;
    let (message_id, html) = bot.latest();
    let tomorrow = button_of(&html, "Завтра в 09:00");
    bot.press_data(&tomorrow, message_id).await;

    let next_morning = local_utc(moscow, day + Duration::days(1), 9, 0);
    let rows = snooze_rows(&db.pool, item.id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].1, next_morning);

    // Полночь меняет локальную дату, но не вхождение вседневного срока.
    tick_and_deliver(
        &mut bot,
        &db.pool,
        local_utc(moscow, day + Duration::days(1), 0, 1),
    )
    .await;
    assert_eq!(snooze_rows(&db.pool, item.id).await[0].0, "pending");
    assert_eq!(message_count(&bot, "Вынести мусор"), 1);

    tick_and_deliver(&mut bot, &db.pool, next_morning).await;
    let rows = snooze_rows(&db.pool, item.id).await;
    assert_eq!(rows[0].0, "sent", "{rows:?}");
    assert_eq!(message_count(&bot, "Вынести мусор"), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snooze_in_a_stale_reminder_only_redraws_and_then_works() {
    let db = isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let due = rounded_now() + Duration::hours(3);
    let item = create_task(&db.pool, &household, "Позвонить маме", due, None).await;

    tick_and_deliver(&mut bot, &db.pool, due - Duration::minutes(15)).await;
    let (message_id, html) = bot.latest();
    let stale = button_of(&html, "на час");
    sqlx::query(
        "UPDATE items SET title = 'Позвонить маме (новая)', version = version + 1 WHERE id = $1",
    )
    .bind(item.id)
    .execute(&db.pool)
    .await
    .expect("external change");

    bot.press_data(&stale, message_id).await;
    assert_eq!(last_toast(&bot).as_deref(), Some("Задача уже изменилась"));
    assert!(
        snooze_rows(&db.pool, item.id).await.is_empty(),
        "a stale reminder must not create a snooze"
    );
    let (_, edited) = bot.latest();
    assert!(edited.contains("Позвонить маме (новая)"), "{edited}");
    assert!(!edited.contains("😴 Напомню"), "{edited}");

    // Новая версия кнопки откладывает напоминание на час.
    let next = button_of(&edited, "на час");
    let pressed_at = bot.now;
    bot.press_data(&next, message_id).await;
    let rows = snooze_rows(&db.pool, item.id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "pending");
    assert!(
        (rows[0].1 - (pressed_at + Duration::hours(1)))
            .num_seconds()
            .abs()
            <= 5,
        "snooze at {}, expected {}",
        rows[0].1,
        pressed_at + Duration::hours(1)
    );
    let stored_due: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT due_at FROM items WHERE id = $1")
            .bind(item.id)
            .fetch_one(&db.pool)
            .await
            .expect("due");
    assert_eq!(stored_due, Some(due), "snooze must not move the due date");
    assert!(bot.latest().1.contains("😴 Напомню"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_snooze_without_origin_is_cancelled_after_the_due_changed() {
    let db = isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let due = rounded_now() + Duration::hours(3);
    let item = create_task(&db.pool, &household, "Позвонить маме", due, None).await;
    let deliver_at = rounded_now() + Duration::hours(1);
    // Snooze без привязки не совпадает ни с одним вхождением срока, поэтому
    // устаревает на ближайшем тике.
    insert_snooze_delivery(&db.pool, &household, item.id, None, deliver_at).await;
    sqlx::query("UPDATE items SET due_at = $2, version = version + 1 WHERE id = $1")
        .bind(item.id)
        .bind(due + Duration::hours(1))
        .execute(&db.pool)
        .await
        .expect("move due");

    tick_and_deliver(&mut bot, &db.pool, deliver_at).await;

    let rows = snooze_rows(&db.pool, item.id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "cancelled", "{rows:?}");
    assert_eq!(
        message_count(&bot, "Позвонить маме"),
        0,
        "an unbound snooze must not be delivered"
    );
}

/// Пользователь дважды нажимает «Завтра в 09:00» после переноса срока.
///
/// Второе нажатие идёт через соседнюю кнопку старой версии: она только
/// перерисовывает напоминание. `cancel_before_second` — тик уже отменил первую
/// запись snooze до второго нажатия.
async fn repeat_morning_snooze_after_due_change(cancel_before_second: bool) {
    let db = isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = bot.now.with_timezone(&moscow).date_naive();
    bot.set_now(local_utc(moscow, day, 20, 0));
    let due = local_utc(moscow, day, 22, 0);
    let item = create_task(&db.pool, &household, "Позвонить маме", due, None).await;

    tick_and_deliver(&mut bot, &db.pool, local_utc(moscow, day, 21, 45)).await;
    let (message_id, html) = bot.latest();
    let tomorrow = button_of(&html, "Завтра в 09:00");
    let other_snooze = button_of(&html, "на час");
    bot.press_data(&tomorrow, message_id).await;

    let next_morning = local_utc(moscow, day + Duration::days(1), 9, 0);
    let first = snooze_rows(&db.pool, item.id).await;
    assert_eq!(first.len(), 1, "{first:?}");
    assert_eq!(first[0].1, next_morning);

    let new_due = local_utc(moscow, day + Duration::days(1), 12, 0);
    sqlx::query("UPDATE items SET due_at = $2, version = version + 1 WHERE id = $1")
        .bind(item.id)
        .bind(new_due)
        .execute(&db.pool)
        .await
        .expect("move due");
    let new_origin = tasklerq::jobs::scheduler::occurrence_key(new_due);

    if cancel_before_second {
        tick_and_deliver(&mut bot, &db.pool, local_utc(moscow, day, 22, 30)).await;
        let rows = snooze_rows(&db.pool, item.id).await;
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].0, "cancelled", "{rows:?}");
    }

    bot.press_data(&other_snooze, message_id).await;
    assert_eq!(last_toast(&bot).as_deref(), Some("Задача уже изменилась"));
    let (_, redrawn) = bot.latest();
    assert!(redrawn.contains("Позвонить маме"), "{redrawn}");
    assert!(!redrawn.contains("😴 Напомню"), "{redrawn}");

    let second_morning = button_of(&redrawn, "Завтра в 09:00");
    bot.press_data(&second_morning, message_id).await;

    let rows = snooze_rows(&db.pool, item.id).await;
    assert_eq!(rows.len(), 1, "one snooze record: {rows:?}");
    assert_eq!(rows[0].0, "pending", "{rows:?}");
    assert_eq!(
        rows[0].3.as_deref(),
        Some(new_origin.as_str()),
        "the repeated snooze must bind to the new due"
    );
    assert_eq!(rows[0].1, next_morning, "{rows:?}");

    let before = message_count(&bot, "Позвонить маме");
    tick_and_deliver(&mut bot, &db.pool, next_morning).await;
    let rows = snooze_rows(&db.pool, item.id).await;
    assert_eq!(
        rows.len(),
        1,
        "the old binding must not add a delivery: {rows:?}"
    );
    assert_eq!(rows[0].0, "sent", "{rows:?}");
    assert_eq!(rows[0].3.as_deref(), Some(new_origin.as_str()));
    assert_eq!(message_count(&bot, "Позвонить маме"), before + 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeated_morning_snooze_after_due_change_before_the_cancel_tick() {
    repeat_morning_snooze_after_due_change(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeated_morning_snooze_after_due_change_after_the_cancel_tick() {
    repeat_morning_snooze_after_due_change(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_snooze_token_without_a_version_still_snoozes() {
    let db = isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let due = rounded_now() + Duration::hours(3);
    let item = create_task(&db.pool, &household, "Позвонить маме", due, None).await;

    tick_and_deliver(&mut bot, &db.pool, due - Duration::minutes(15)).await;
    let (message_id, html) = bot.latest();
    let snooze = button_of(&html, "на час");

    sqlx::query(
        "UPDATE items SET title = 'Позвонить маме (новая)', version = version + 1 WHERE id = $1",
    )
    .bind(item.id)
    .execute(&db.pool)
    .await
    .expect("external change");
    // Токен, выпущенный до деплоя: в payload нет версии, как и в старой схеме.
    sqlx::query(
        "UPDATE callback_tokens
         SET payload = payload - 'version', version = NULL
         WHERE action = 'snooze' AND payload->>'item_id' = $1 AND consumed_at IS NULL",
    )
    .bind(item.id.to_string())
    .execute(&db.pool)
    .await
    .expect("legacy token");

    bot.press_data(&snooze, message_id).await;
    assert_eq!(last_toast(&bot).as_deref(), Some("Отложено"));
    let rows = snooze_rows(&db.pool, item.id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "pending", "{rows:?}");
    assert!(bot.latest().1.contains("😴 Напомню"), "{}", bot.latest().1);

    let reminder_key = reminder_occurrence_key(&db.pool, item.id).await;
    assert_eq!(
        snooze_rows(&db.pool, item.id).await[0].3.as_deref(),
        Some(reminder_key.as_str())
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn event_reminder_shows_start_end_and_event_actions() {
    let db = isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let moscow = chrono_tz::Europe::Moscow;
    let start = rounded_now() + Duration::hours(3);
    let end = start + Duration::minutes(90);
    create_event(&db.pool, &household, "Маникюр", start, end, None).await;

    tick_and_deliver(&mut bot, &db.pool, start - Duration::minutes(15)).await;

    let (_, html) = bot.latest();
    assert!(html.contains("⏰ Через 15 минут"), "{html}");
    assert!(html.contains("Маникюр"), "{html}");
    let local_start = start.with_timezone(&moscow).format("%H:%M").to_string();
    let local_end = end.with_timezone(&moscow).format("%H:%M").to_string();
    assert!(html.contains("📅 Начало:"), "{html}");
    assert!(html.contains(&local_start), "{html}");
    assert!(html.contains("🏁 Конец:"), "{html}");
    assert!(html.contains(&local_end), "{html}");

    let labels: Vec<String> = buttons(&html).into_iter().map(|(label, _)| label).collect();
    assert_eq!(labels.len(), 3, "{labels:?}");
    for expected in ["Закрыть", "📅 Перенести", "Открыть"] {
        assert!(
            labels.iter().any(|label| label.contains(expected)),
            "button `{expected}` missing: {labels:?}"
        );
    }
    for forbidden in ["Выполнено", "Отложить на час", "Завтра в 09:00"]
    {
        assert!(
            !labels.iter().any(|label| label.contains(forbidden)),
            "button `{forbidden}` must be absent: {labels:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closed_event_reminder_is_deleted_and_does_not_come_back_for_the_same_occurrence() {
    let db = isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let moscow = chrono_tz::Europe::Moscow;
    let start = rounded_now() + Duration::hours(3);
    let end = start + Duration::hours(1);
    let weekday = start.with_timezone(&moscow).weekday().number_from_monday() as i16;
    let event = create_event(
        &db.pool,
        &household,
        "Тренировка",
        start,
        end,
        Some(support::recurrence(RecurrenceFreq::Weekly, vec![weekday])),
    )
    .await;

    tick_and_deliver(&mut bot, &db.pool, start - Duration::minutes(15)).await;
    let (message_id, html) = bot.latest();
    let close = button_of(&html, "Закрыть");
    let before = event_times(&db.pool, event.id).await;

    bot.press_data(&close, message_id).await;

    assert!(
        bot.fake.calls().iter().any(|call| matches!(
            call,
            Call::Delete { chat, id } if *chat == bot.chat && *id == message_id
        )),
        "«Закрыть» must delete the reminder through the outbox"
    );
    assert!(
        !bot.fake
            .messages(bot.chat)
            .into_iter()
            .any(|(id, _)| id == message_id),
        "the reminder must disappear from the chat"
    );
    assert_eq!(
        event_times(&db.pool, event.id).await,
        before,
        "«Закрыть» must not change the event"
    );
    let rows = reminder_rows(&db.pool, event.id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "sent", "{rows:?}");

    // Тик того же вхождения ничего не планирует повторно.
    tick_and_deliver(&mut bot, &db.pool, start - Duration::minutes(5)).await;
    let rows = reminder_rows(&db.pool, event.id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(message_count(&bot, "Тренировка"), 0, "{rows:?}");

    // Следующее вхождение идёт по расписанию серии.
    tick_and_deliver(&mut bot, &db.pool, end).await;
    let (due_at, end_at, _) = event_times(&db.pool, event.id).await;
    let next_start = start + Duration::days(7);
    assert_eq!(due_at, Some(next_start));
    assert_eq!(end_at, Some(next_start + Duration::hours(1)));

    tick_and_deliver(&mut bot, &db.pool, next_start - Duration::minutes(15)).await;
    assert_eq!(
        message_count(&bot, "Тренировка"),
        1,
        "the next occurrence must get its own reminder"
    );
    let rows = reminder_rows(&db.pool, event.id).await;
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[1].0, "sent", "{rows:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn moving_an_event_from_the_reminder_keeps_the_duration_and_replans_the_reminder() {
    let db = isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let moscow = chrono_tz::Europe::Moscow;
    let start = rounded_now() + Duration::hours(3);
    let end = start + Duration::minutes(90);
    let event = create_event(&db.pool, &household, "Маникюр", start, end, None).await;

    tick_and_deliver(&mut bot, &db.pool, start - Duration::minutes(15)).await;
    let (message_id, html) = bot.latest();
    let move_button = button_of(&html, "Перенести");
    bot.press_data(&move_button, message_id).await;

    bot.assert_screen_contains("Срок");
    for label in ["Сегодня", "Завтра", "📅 Дата и время"] {
        bot.assert_has_button(label);
    }
    assert!(
        bot.find_button("Без срока").is_none(),
        "an event must have no «Без срока» in the quick due screen:\n{}",
        bot.screen()
    );

    bot.press("Завтра").await;

    let local = start.with_timezone(&moscow);
    let tomorrow = local.date_naive() + Duration::days(1);
    let new_start = local_utc(moscow, tomorrow, local.hour(), local.minute());
    let (due_at, end_at, version) = event_times(&db.pool, event.id).await;
    assert_eq!(due_at, Some(new_start));
    assert_eq!(end_at, Some(new_start + Duration::minutes(90)));
    assert_eq!(version, event.version + 1);

    tick_and_deliver(&mut bot, &db.pool, new_start - Duration::minutes(15)).await;
    let (_, reminder) = bot.latest();
    assert!(
        reminder.contains("Маникюр") && reminder.contains("⏰ Через 15 минут"),
        "a reminder for the new start must arrive at lead time: {reminder}"
    );
    let rows = reminder_rows(&db.pool, event.id).await;
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[1].0, "sent", "{rows:?}");
    assert_eq!(
        rows[1].1,
        tasklerq::jobs::scheduler::occurrence_key(new_start)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn moving_a_recurring_event_from_the_reminder_changes_only_the_nearest_occurrence() {
    let db = isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let moscow = chrono_tz::Europe::Moscow;
    let start = rounded_now() + Duration::hours(3);
    let end = start + Duration::hours(1);
    let weekday = start.with_timezone(&moscow).weekday().number_from_monday() as i16;
    let event = create_event(
        &db.pool,
        &household,
        "Тренировка",
        start,
        end,
        Some(support::recurrence(RecurrenceFreq::Weekly, vec![weekday])),
    )
    .await;

    tick_and_deliver(&mut bot, &db.pool, start - Duration::minutes(15)).await;
    let (message_id, html) = bot.latest();
    bot.press_data(&button_of(&html, "Перенести"), message_id)
        .await;
    bot.press("Завтра").await;

    let local = start.with_timezone(&moscow);
    let tomorrow = local.date_naive() + Duration::days(1);
    let moved_start = local_utc(moscow, tomorrow, local.hour(), local.minute());
    assert_eq!(event_times(&db.pool, event.id).await.0, Some(moved_start));

    // После конца перенесённого вхождения серия возвращается к своему дню.
    tick_and_deliver(&mut bot, &db.pool, moved_start + Duration::hours(1)).await;
    let (due_at, end_at, _) = event_times(&db.pool, event.id).await;
    let next_start = start + Duration::days(7);
    assert_eq!(
        due_at,
        Some(next_start),
        "the following occurrence must follow the series"
    );
    assert_eq!(end_at, Some(next_start + Duration::hours(1)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn moving_a_deleted_event_reports_unavailable_and_opens_nothing() {
    let db = isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let start = rounded_now() + Duration::hours(3);
    let event = create_event(
        &db.pool,
        &household,
        "Маникюр",
        start,
        start + Duration::minutes(90),
        None,
    )
    .await;

    tick_and_deliver(&mut bot, &db.pool, start - Duration::minutes(15)).await;
    let (message_id, html) = bot.latest();
    let move_button = button_of(&html, "Перенести");

    let mut conn = db.pool.acquire().await.expect("conn");
    items::soft_delete(
        &mut conn,
        household.household_id,
        &household.owner(),
        event.id,
        bot.now,
    )
    .await
    .expect("delete event");

    let before = bot.fake.calls().len();
    bot.press_data(&move_button, message_id).await;

    assert_eq!(
        last_toast(&bot).as_deref(),
        Some("Событие больше недоступно")
    );
    assert!(
        !bot.fake.calls()[before..]
            .iter()
            .any(|call| matches!(call, Call::SendRich { .. })),
        "a deleted event must not open the quick due screen"
    );
    assert!(snooze_rows(&db.pool, event.id).await.is_empty());
}
