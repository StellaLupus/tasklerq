//! Сквозные сценарии напоминаний: доставка планировщиком и действия из
//! уведомления.

mod support;

use chrono::{DateTime, Datelike, Duration, NaiveDate, TimeZone, Timelike, Utc};
use chrono_tz::Tz;
use serde_json::json;
use sqlx::PgPool;
use support::notifications::{button_of, scheduler, tick_and_deliver};
use support::telegram::{Call, Harness, buttons};
use support::{TestHousehold, create_item, isolated_db, new_item, next_telegram_id};
use tasklerq::domain::models::{Actor, Item, RecurrenceFreq, RecurrenceInput, UserRole};
use tasklerq::storage::repositories::items;
use tasklerq::storage::repositories::notifications::{self, NewDelivery, NotificationKind};
use tasklerq::telegram::bot::model::{Action, SnoozeUntil};
use tasklerq::telegram::callbacks::{self, NewToken, Scope};
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
        replaces_message_id: None,
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

/// Статус и отметка уборки доставок задачи данного вида.
async fn delivery_states(
    pool: &PgPool,
    item_id: Uuid,
    kind: &str,
) -> Vec<(String, Option<DateTime<Utc>>)> {
    sqlx::query_as(
        "SELECT status::text, dismissed_at FROM notification_deliveries
         WHERE item_id = $1 AND kind = $2::notification_kind
         ORDER BY created_at, id",
    )
    .bind(item_id)
    .bind(kind)
    .fetch_all(pool)
    .await
    .expect("delivery states")
}

/// Строки отложенного удаления «✅ Выполнено» сообщения: статус, время
/// следующей попытки и payload (после доставки и отмены скрыт).
async fn notice_expire_rows(
    pool: &PgPool,
    chat: i64,
    message_id: i64,
) -> Vec<(String, DateTime<Utc>, serde_json::Value)> {
    sqlx::query_as(
        "SELECT status::text, next_attempt_at, payload FROM outbox
         WHERE chat_id = $1 AND kind = 'delete' AND dedupe_key LIKE $2
         ORDER BY seq",
    )
    .bind(chat)
    .bind(format!("notice-expire:{chat}:{message_id}:%"))
    .fetch_all(pool)
    .await
    .expect("notice expire rows")
}

/// Поля закрытия доставок сообщения (сопоставление через outbox, как у
/// `mark_dismissed`): `closed_at`, `closed_reason`, `closed_by_event`.
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

/// Id последнего действующего выполнения задачи по `item_events`: событие
/// `completed`, не компенсирующее и не отменённое.
async fn latest_completion_event(pool: &PgPool, item_id: Uuid) -> Option<Uuid> {
    sqlx::query_scalar(
        "SELECT e.id FROM item_events e
         WHERE e.item_id = $1 AND e.event_type = 'completed' AND e.undo_of IS NULL
           AND NOT EXISTS (SELECT 1 FROM item_events u WHERE u.undo_of = e.id)
         ORDER BY e.created_at DESC, e.id DESC
         LIMIT 1",
    )
    .bind(item_id)
    .fetch_optional(pool)
    .await
    .expect("latest completion event")
}

/// Есть ли среди вызовов фейка удаление данного сообщения.
fn deleted(bot: &Harness, message_id: i64) -> bool {
    bot.fake.calls().iter().any(
        |call| matches!(call, Call::Delete { chat, id } if *chat == bot.chat && *id == message_id),
    )
}

fn local_utc(timezone: Tz, date: NaiveDate, hour: u32, minute: u32) -> DateTime<Utc> {
    timezone
        .with_ymd_and_hms(date.year(), date.month(), date.day(), hour, minute, 0)
        .single()
        .expect("local time")
        .with_timezone(&Utc)
}

/// Ближайший будущий момент `hour:minute` по Москве. Заморозка часов не должна
/// уводить время назад относительно реального: событие создания задачи
/// пишется с реальным `created_at`, и в прошлом оно оказалось бы «новее»
/// завершения — `undo_last` выбрал бы не то событие.
fn next_moscow_time(bot: &Harness, hour: u32, minute: u32) -> DateTime<Utc> {
    let moscow = chrono_tz::Europe::Moscow;
    let mut day = bot.now.with_timezone(&moscow).date_naive();
    let mut at = local_utc(moscow, day, hour, minute);
    if at <= bot.now {
        day += Duration::days(1);
        at = local_utc(moscow, day, hour, minute);
    }
    at
}

fn rounded_now() -> DateTime<Utc> {
    Utc::now()
        .with_second(0)
        .and_then(|at| at.with_nanosecond(0))
        .expect("rounded time")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scheduler_reminder_has_five_action_buttons() {
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
    assert_eq!(labels.len(), 5, "{labels:?}");
    for expected in [
        "Выполнено",
        "Отложить на час",
        "Завтра в 09:00",
        "Открыть",
        "Убрать",
    ] {
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

    // В момент snooze приходит новое напоминание с кнопками, а сообщение
    // «😴 Напомню …» удаляется: в чате одно сообщение по задаче.
    tick_and_deliver(&mut bot, &db.pool, expected).await;
    assert_eq!(message_count(&bot, "Позвонить маме"), 1);
    assert!(deleted(&bot, message_id));
    let (_, reminder) = bot.latest();
    assert_eq!(buttons(&reminder).len(), 5, "{reminder}");
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
    assert_eq!(message_count(&bot, "Выключить свет"), 1);
    assert!(deleted(&bot, message_id));
    assert_eq!(buttons(&bot.latest().1).len(), 5);
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
    // Отмена snooze удаляет сообщение «😴 Напомню …».
    assert_eq!(message_count(&bot, "Позвонить маме"), 0);
    assert!(deleted(&bot, message_id));
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
    // Отмена snooze удаляет сообщение «😴 Напомню …».
    assert_eq!(message_count(&bot, "Купить билеты"), 0);
    assert!(deleted(&bot, message_id));
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
    // Отмена snooze удаляет сообщение «😴 Напомню …».
    assert_eq!(message_count(&bot, "Оплатить интернет"), 0);
    assert!(deleted(&bot, message_id));

    // Напоминание о новом сроке приходит за 15 минут до него.
    tick_and_deliver(&mut bot, &db.pool, new_due - Duration::minutes(15)).await;
    assert_eq!(message_count(&bot, "Оплатить интернет"), 1);
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
    // Отмена snooze удаляет сообщение «😴 Напомню …».
    assert_eq!(message_count(&bot, "Позвонить маме"), 0);
    assert!(deleted(&bot, message_id));
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
    // Отмена snooze удаляет сообщение «😴 Напомню …».
    assert_eq!(message_count(&bot, "Ежедневная зарядка"), 0);
    assert!(deleted(&bot, message_id));

    tick_and_deliver(&mut bot, &db.pool, next_due - Duration::minutes(15)).await;
    assert_eq!(message_count(&bot, "Ежедневная зарядка"), 1);
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
    // Новое напоминание заменило сообщение «😴 Напомню …».
    assert_eq!(message_count(&bot, "Выключить свет"), 1);
    assert!(deleted(&bot, message_id));
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
    // Новое напоминание заменило сообщение «😴 Напомню …».
    assert_eq!(message_count(&bot, "Вынести мусор"), 1);
    assert!(deleted(&bot, message_id));
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
    // Новое напоминание приходит, а сообщение «😴 Напомню …» удаляется.
    // Если отмена уже удалила его раньше, перерисовки по нажатиям создали
    // запасные сообщения и счёт растёт на одно новое напоминание; иначе
    // новое напоминание заменяет прежнее сообщение, и счёт не меняется.
    let expected = if cancel_before_second {
        before + 1
    } else {
        before
    };
    assert_eq!(message_count(&bot, "Позвонить маме"), expected);
    assert!(deleted(&bot, message_id));
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
    for expected in ["🗑 Убрать", "📅 Перенести", "Открыть"] {
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
    let close = button_of(&html, "🗑 Убрать");
    let before = event_times(&db.pool, event.id).await;

    bot.press_data(&close, message_id).await;

    assert!(
        bot.fake.calls().iter().any(|call| matches!(
            call,
            Call::Delete { chat, id } if *chat == bot.chat && *id == message_id
        )),
        "«🗑 Убрать» must delete the reminder through the outbox"
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
        "«🗑 Убрать» must not change the event"
    );
    let rows = reminder_rows(&db.pool, event.id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "sent", "{rows:?}");
    let states = delivery_states(&db.pool, event.id, "reminder").await;
    assert!(
        states[0].1.is_some(),
        "«🗑 Убрать» must mark the delivery dismissed: {states:?}"
    );

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
    let states = delivery_states(&db.pool, event.id, "reminder").await;
    assert!(
        states[1].1.is_none(),
        "the next occurrence must not be dismissed: {states:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dismiss_in_a_task_reminder_deletes_with_cleanup_and_marks_the_delivery() {
    let db = isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let due = rounded_now() + Duration::hours(3);
    let item = create_task(&db.pool, &household, "Оплатить интернет", due, None).await;

    tick_and_deliver(&mut bot, &db.pool, due - Duration::minutes(15)).await;
    let (message_id, html) = bot.latest();
    let dismiss = button_of(&html, "🗑 Убрать");
    let before = item_status(&db.pool, item.id).await;

    // Обработчик прошёл, исходящие ещё не доставлены: payload удаления цел.
    bot.press_data_pending(&dismiss, message_id).await;
    let payload: serde_json::Value = sqlx::query_scalar(
        "SELECT payload FROM outbox
         WHERE chat_id = $1 AND kind = 'delete' AND dedupe_key = $2",
    )
    .bind(bot.chat)
    .bind(format!("delete:{}:{message_id}", bot.chat))
    .fetch_one(&db.pool)
    .await
    .expect("cleanup delete row");
    assert_eq!(payload["message_id"], json!(message_id), "{payload}");
    assert_eq!(payload["cleanup"], json!(true), "{payload}");

    bot.deliver().await;
    assert!(
        !bot.fake
            .messages(bot.chat)
            .into_iter()
            .any(|(id, _)| id == message_id),
        "the reminder must disappear from the chat"
    );
    assert_eq!(
        item_status(&db.pool, item.id).await,
        before,
        "«🗑 Убрать» must not change the task"
    );
    let stored_due: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT due_at FROM items WHERE id = $1")
            .bind(item.id)
            .fetch_one(&db.pool)
            .await
            .expect("due");
    assert_eq!(
        stored_due,
        Some(due),
        "«🗑 Убрать» must not move the due date"
    );
    let states = delivery_states(&db.pool, item.id, "reminder").await;
    assert_eq!(states.len(), 1, "{states:?}");
    assert_eq!(states[0].0, "sent", "{states:?}");
    assert!(
        states[0].1.is_some(),
        "«🗑 Убрать» must mark the delivery dismissed: {states:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dismiss_in_an_assignment_notice_deletes_with_cleanup_and_marks_the_delivery() {
    let db = isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let due = rounded_now() + Duration::hours(3);
    let item = create_task(&db.pool, &household, "Купить торт", due, None).await;
    let now = rounded_now();
    support::notifications::insert_delivery(
        &db.pool,
        household.household_id,
        household.owner_id,
        item.id,
        "assignment",
        "assignment:dismiss:0",
        now,
        false,
    )
    .await;

    tick_and_deliver(&mut bot, &db.pool, now).await;
    let (message_id, html) = bot.latest();
    assert!(html.contains("Вам назначили задачу"), "{html}");
    let dismiss = button_of(&html, "🗑 Убрать");
    let before = item_status(&db.pool, item.id).await;

    bot.press_data_pending(&dismiss, message_id).await;
    let payload: serde_json::Value = sqlx::query_scalar(
        "SELECT payload FROM outbox
         WHERE chat_id = $1 AND kind = 'delete' AND dedupe_key = $2",
    )
    .bind(bot.chat)
    .bind(format!("delete:{}:{message_id}", bot.chat))
    .fetch_one(&db.pool)
    .await
    .expect("cleanup delete row");
    assert_eq!(payload["message_id"], json!(message_id), "{payload}");
    assert_eq!(payload["cleanup"], json!(true), "{payload}");

    bot.deliver().await;
    assert!(
        !bot.fake
            .messages(bot.chat)
            .into_iter()
            .any(|(id, _)| id == message_id),
        "the assignment notice must disappear from the chat"
    );
    assert_eq!(
        item_status(&db.pool, item.id).await,
        before,
        "«🗑 Убрать» must not change the task"
    );
    let states = delivery_states(&db.pool, item.id, "assignment").await;
    assert_eq!(states.len(), 1, "{states:?}");
    assert_eq!(states[0].0, "sent", "{states:?}");
    assert!(
        states[0].1.is_some(),
        "«🗑 Убрать» must mark the delivery dismissed: {states:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dismiss_after_snooze_deletes_the_message_and_keeps_the_snooze_pending() {
    let db = isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let due = rounded_now() + Duration::hours(3);
    let item = create_task(&db.pool, &household, "Позвонить маме", due, None).await;

    tick_and_deliver(&mut bot, &db.pool, due - Duration::minutes(15)).await;
    let (message_id, html) = bot.latest();
    bot.press_data(&button_of(&html, "на час"), message_id)
        .await;
    let rows = snooze_rows(&db.pool, item.id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "pending", "{rows:?}");
    let snoozed_at = rows[0].1;
    let (_, snoozed_html) = bot.latest();
    assert!(snoozed_html.contains("😴 Напомню"), "{snoozed_html}");
    let dismiss = button_of(&snoozed_html, "🗑 Убрать");

    bot.press_data(&dismiss, message_id).await;

    assert!(
        bot.fake.calls().iter().any(|call| matches!(
            call,
            Call::Delete { chat, id } if *chat == bot.chat && *id == message_id
        )),
        "«🗑 Убрать» must delete the snoozed message through the outbox"
    );
    assert!(
        !bot.fake
            .messages(bot.chat)
            .into_iter()
            .any(|(id, _)| id == message_id),
        "the snoozed message must disappear from the chat"
    );
    // Отложенная доставка не тронута: прежний срок, без отметки уборки.
    let rows = snooze_rows(&db.pool, item.id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "pending", "{rows:?}");
    assert_eq!(rows[0].1, snoozed_at, "{rows:?}");
    let snooze_dismissed: Option<DateTime<Utc>> = sqlx::query_scalar(
        "SELECT dismissed_at FROM notification_deliveries
         WHERE item_id = $1 AND kind = 'snooze'",
    )
    .bind(item.id)
    .fetch_one(&db.pool)
    .await
    .expect("snooze dismissed_at");
    assert!(snooze_dismissed.is_none(), "{snooze_dismissed:?}");
    // Запись исходного напоминания отмечена убранной, статус прежний.
    let states = delivery_states(&db.pool, item.id, "reminder").await;
    assert_eq!(states.len(), 1, "{states:?}");
    assert_eq!(states[0].0, "sent", "{states:?}");
    assert!(states[0].1.is_some(), "{states:?}");
    assert_eq!(
        item_status(&db.pool, item.id).await.2,
        item.version,
        "«🗑 Убрать» must not change the task"
    );

    // Отложенное напоминание всё равно приходит в свой срок.
    tick_and_deliver(&mut bot, &db.pool, snoozed_at).await;
    assert_eq!(message_count(&bot, "Позвонить маме"), 1);
    assert_eq!(snooze_rows(&db.pool, item.id).await[0].0, "sent");
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn done_in_a_reminder_is_deleted_ten_minutes_later() {
    let db = isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let at = next_moscow_time(&bot, 9, 0);
    bot.freeze_at(at);
    let item = create_task(
        &db.pool,
        &household,
        "Оплатить интернет",
        at + Duration::minutes(15),
        None,
    )
    .await;

    tick_and_deliver(&mut bot, &db.pool, at).await;
    let (message_id, html) = bot.latest();
    let done = button_of(&html, "Выполнено");

    // Обработчик прошёл, исходящие ещё не доставлены: payload удаления цел.
    bot.press_data_pending(&done, message_id).await;
    let rows = notice_expire_rows(&db.pool, bot.chat, message_id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "pending", "{rows:?}");
    assert_eq!(rows[0].1, at + Duration::minutes(10), "{rows:?}");
    assert_eq!(rows[0].2["message_id"], json!(message_id), "{rows:?}");
    assert_eq!(rows[0].2["cleanup"], json!(true), "{rows:?}");

    bot.deliver().await;
    let (_, done_html) = bot.latest();
    assert!(done_html.contains("✅ Выполнено"), "{done_html}");
    assert!(
        buttons(&done_html)
            .iter()
            .any(|(label, _)| label.contains("Отменить")),
        "{done_html}"
    );
    assert_eq!(item_status(&db.pool, item.id).await.0, "done");

    // До истечения окна сообщение живо.
    tick_and_deliver(&mut bot, &db.pool, at + Duration::minutes(9)).await;
    assert!(
        !deleted(&bot, message_id),
        "the notice must wait for the window"
    );

    tick_and_deliver(&mut bot, &db.pool, at + Duration::minutes(11)).await;
    assert!(
        deleted(&bot, message_id),
        "the notice must be deleted at 09:10"
    );
    assert!(
        !bot.fake
            .messages(bot.chat)
            .into_iter()
            .any(|(id, _)| id == message_id),
        "the done notice must disappear from the chat"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undo_in_the_window_cancels_the_scheduled_delete() {
    let db = isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let at = next_moscow_time(&bot, 9, 0);
    bot.freeze_at(at);
    let item = create_task(
        &db.pool,
        &household,
        "Оплатить интернет",
        at + Duration::minutes(15),
        None,
    )
    .await;

    tick_and_deliver(&mut bot, &db.pool, at).await;
    let (message_id, html) = bot.latest();
    bot.press_data(&button_of(&html, "Выполнено"), message_id)
        .await;
    let undo = button_of(&bot.latest().1, "Отменить");

    bot.advance(Duration::minutes(5));
    bot.press_data(&undo, message_id).await;

    let rows = notice_expire_rows(&db.pool, bot.chat, message_id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "skipped", "{rows:?}");
    assert_eq!(item_status(&db.pool, item.id).await.0, "open");
    let (_, restored) = bot.latest();
    assert!(!restored.contains("✅"), "{restored}");
    assert!(restored.contains("Отложить на час"), "{restored}");

    tick_and_deliver(&mut bot, &db.pool, at + Duration::minutes(11)).await;
    assert!(
        !deleted(&bot, message_id),
        "the cancelled delete must not run at 09:10"
    );
    assert!(
        bot.fake
            .messages(bot.chat)
            .into_iter()
            .any(|(id, _)| id == message_id),
        "the restored reminder must stay in the chat"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn done_again_after_undo_schedules_a_new_delete() {
    let db = isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let at = next_moscow_time(&bot, 9, 0);
    bot.freeze_at(at);
    let item = create_task(
        &db.pool,
        &household,
        "Оплатить интернет",
        at + Duration::minutes(15),
        None,
    )
    .await;

    tick_and_deliver(&mut bot, &db.pool, at).await;
    let (message_id, html) = bot.latest();
    bot.press_data(&button_of(&html, "Выполнено"), message_id)
        .await;
    bot.advance(Duration::minutes(5));
    bot.press_data(&button_of(&bot.latest().1, "Отменить"), message_id)
        .await;

    // 09:06: напоминание возвращено, кнопка несёт актуальную версию.
    bot.advance(Duration::minutes(1));
    let done_again = button_of(&bot.latest().1, "Выполнено");
    bot.press_data_pending(&done_again, message_id).await;

    let rows = notice_expire_rows(&db.pool, bot.chat, message_id).await;
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[0].0, "skipped", "{rows:?}");
    assert_eq!(rows[0].1, at + Duration::minutes(10), "{rows:?}");
    assert_eq!(rows[1].0, "pending", "{rows:?}");
    assert_eq!(rows[1].1, at + Duration::minutes(16), "{rows:?}");
    assert_eq!(rows[1].2["message_id"], json!(message_id), "{rows:?}");
    assert_eq!(rows[1].2["cleanup"], json!(true), "{rows:?}");
    assert_eq!(item_status(&db.pool, item.id).await.0, "done");

    bot.deliver().await;
    tick_and_deliver(&mut bot, &db.pool, at + Duration::minutes(11)).await;
    assert!(
        !deleted(&bot, message_id),
        "the first delete is skipped, the second is not due yet"
    );

    tick_and_deliver(&mut bot, &db.pool, at + Duration::minutes(17)).await;
    assert!(
        deleted(&bot, message_id),
        "the notice must be deleted at 09:16"
    );
    assert!(
        !bot.fake
            .messages(bot.chat)
            .into_iter()
            .any(|(id, _)| id == message_id),
        "the done notice must disappear from the chat"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn done_in_an_assignment_notice_schedules_the_delete_and_undo_cancels_it() {
    let db = isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let at = next_moscow_time(&bot, 9, 0);
    bot.freeze_at(at);
    let item = create_task(
        &db.pool,
        &household,
        "Купить торт",
        at + Duration::hours(3),
        None,
    )
    .await;
    support::notifications::insert_delivery(
        &db.pool,
        household.household_id,
        household.owner_id,
        item.id,
        "assignment",
        "assignment:done:0",
        at,
        false,
    )
    .await;

    tick_and_deliver(&mut bot, &db.pool, at).await;
    let (message_id, html) = bot.latest();
    assert!(html.contains("Вам назначили задачу"), "{html}");
    let done = button_of(&html, "Выполнено");

    bot.press_data_pending(&done, message_id).await;
    let rows = notice_expire_rows(&db.pool, bot.chat, message_id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "pending", "{rows:?}");
    assert_eq!(rows[0].1, at + Duration::minutes(10), "{rows:?}");
    assert_eq!(rows[0].2["message_id"], json!(message_id), "{rows:?}");
    assert_eq!(rows[0].2["cleanup"], json!(true), "{rows:?}");
    bot.deliver().await;
    assert!(
        bot.latest().1.contains("✅ Выполнено"),
        "{}",
        bot.latest().1
    );

    bot.advance(Duration::minutes(5));
    bot.press_data(&button_of(&bot.latest().1, "Отменить"), message_id)
        .await;
    let rows = notice_expire_rows(&db.pool, bot.chat, message_id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "skipped", "{rows:?}");
    assert_eq!(item_status(&db.pool, item.id).await.0, "open");

    tick_and_deliver(&mut bot, &db.pool, at + Duration::minutes(11)).await;
    assert!(
        !deleted(&bot, message_id),
        "the cancelled delete must not run at 09:10"
    );
    assert!(
        bot.fake
            .messages(bot.chat)
            .into_iter()
            .any(|(id, _)| id == message_id),
        "the restored notice must stay in the chat"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn done_on_a_task_completed_from_the_card_schedules_the_delete() {
    let db = isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let at = next_moscow_time(&bot, 12, 0);
    bot.freeze_at(at);
    let item = create_task(
        &db.pool,
        &household,
        "Оплатить интернет",
        at + Duration::minutes(15),
        None,
    )
    .await;

    tick_and_deliver(&mut bot, &db.pool, at).await;
    let (message_id, html) = bot.latest();
    let done = button_of(&html, "Выполнено");

    // Задачу выполнили из карточки: кнопка в напоминании устарела по версии.
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
    .expect("complete from the card");
    drop(conn);

    bot.press_data_pending(&done, message_id).await;
    assert_eq!(last_toast(&bot).as_deref(), Some("Задача уже выполнена"));
    let rows = notice_expire_rows(&db.pool, bot.chat, message_id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "pending", "{rows:?}");
    assert_eq!(rows[0].1, at + Duration::minutes(10), "{rows:?}");
    assert_eq!(rows[0].2["cleanup"], json!(true), "{rows:?}");

    bot.deliver().await;
    assert!(
        bot.latest().1.contains("✅ Выполнено"),
        "{}",
        bot.latest().1
    );

    tick_and_deliver(&mut bot, &db.pool, at + Duration::minutes(11)).await;
    assert!(
        deleted(&bot, message_id),
        "the notice must be deleted at 12:10"
    );
    assert!(
        !bot.fake
            .messages(bot.chat)
            .into_iter()
            .any(|(id, _)| id == message_id),
        "the done notice must disappear from the chat"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_undo_does_not_cancel_the_scheduled_delete() {
    let db = isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let at = next_moscow_time(&bot, 9, 0);
    bot.freeze_at(at);
    let item = create_task(
        &db.pool,
        &household,
        "Оплатить интернет",
        at + Duration::minutes(15),
        None,
    )
    .await;

    tick_and_deliver(&mut bot, &db.pool, at).await;
    let (message_id, html) = bot.latest();
    bot.press_data(&button_of(&html, "Выполнено"), message_id)
        .await;
    let undo = button_of(&bot.latest().1, "Отменить");

    // 09:11: окно отмены истекло, срок удаления уже наступил.
    bot.advance(Duration::minutes(11));
    bot.press_data(&undo, message_id).await;

    assert_eq!(last_toast(&bot).as_deref(), Some("Время отмены истекло"));
    assert_eq!(item_status(&db.pool, item.id).await.0, "done");
    let rows = notice_expire_rows(&db.pool, bot.chat, message_id).await;
    assert_eq!(
        rows[0].0, "sent",
        "the expired undo must not skip the delete: {rows:?}"
    );
    assert_eq!(rows[0].1, at + Duration::minutes(10), "{rows:?}");
    assert!(
        deleted(&bot, message_id),
        "the scheduled delete must still run"
    );
    assert!(
        !bot.fake
            .messages(bot.chat)
            .into_iter()
            .any(|(id, _)| id == message_id),
        "the done notice must disappear from the chat"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn done_from_reminder_marks_its_delivery_closed() {
    let db = isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let at = next_moscow_time(&bot, 9, 0);
    bot.freeze_at(at);
    let item = create_task(
        &db.pool,
        &household,
        "Оплатить интернет",
        at + Duration::minutes(15),
        None,
    )
    .await;

    tick_and_deliver(&mut bot, &db.pool, at).await;
    let (message_id, html) = bot.latest();
    bot.press_data(&button_of(&html, "Выполнено"), message_id)
        .await;

    let event_id = latest_completion_event(&db.pool, item.id)
        .await
        .expect("completion event");
    let rows = notice_closed_rows(&db.pool, bot.chat, message_id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    let closed_at = rows[0].0.expect("closed_at must be set");
    assert!(
        (closed_at - at).num_seconds().abs() <= 5,
        "closed_at {closed_at}, expected about {at}"
    );
    assert_eq!(rows[0].1.as_deref(), Some("completed"), "{rows:?}");
    assert_eq!(rows[0].2, Some(event_id), "{rows:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undo_clears_the_close_and_done_again_marks_the_new_event() {
    let db = isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let at = next_moscow_time(&bot, 9, 0);
    bot.freeze_at(at);
    let item = create_task(
        &db.pool,
        &household,
        "Оплатить интернет",
        at + Duration::minutes(15),
        None,
    )
    .await;

    tick_and_deliver(&mut bot, &db.pool, at).await;
    let (message_id, html) = bot.latest();
    bot.press_data(&button_of(&html, "Выполнено"), message_id)
        .await;
    let first_event = latest_completion_event(&db.pool, item.id)
        .await
        .expect("first completion event");
    let rows = notice_closed_rows(&db.pool, bot.chat, message_id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].2, Some(first_event), "{rows:?}");

    bot.advance(Duration::minutes(5));
    bot.press_data(&button_of(&bot.latest().1, "Отменить"), message_id)
        .await;
    let rows = notice_closed_rows(&db.pool, bot.chat, message_id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0], (None, None, None), "{rows:?}");

    bot.advance(Duration::minutes(1));
    bot.press_data(&button_of(&bot.latest().1, "Выполнено"), message_id)
        .await;
    let second_event = latest_completion_event(&db.pool, item.id)
        .await
        .expect("second completion event");
    assert_ne!(second_event, first_event);
    let rows = notice_closed_rows(&db.pool, bot.chat, message_id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    let closed_at = rows[0].0.expect("closed_at must be set again");
    assert!(
        (closed_at - (at + Duration::minutes(6)))
            .num_seconds()
            .abs()
            <= 5,
        "closed_at {closed_at}, expected about {}",
        at + Duration::minutes(6)
    );
    assert_eq!(rows[0].1.as_deref(), Some("completed"), "{rows:?}");
    assert_eq!(rows[0].2, Some(second_event), "{rows:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn done_in_an_assignment_notice_marks_its_delivery_closed_and_undo_clears() {
    let db = isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let at = next_moscow_time(&bot, 9, 0);
    bot.freeze_at(at);
    let item = create_task(
        &db.pool,
        &household,
        "Купить торт",
        at + Duration::hours(3),
        None,
    )
    .await;
    support::notifications::insert_delivery(
        &db.pool,
        household.household_id,
        household.owner_id,
        item.id,
        "assignment",
        "assignment:closed:0",
        at,
        false,
    )
    .await;

    tick_and_deliver(&mut bot, &db.pool, at).await;
    let (message_id, html) = bot.latest();
    assert!(html.contains("Вам назначили задачу"), "{html}");
    bot.press_data(&button_of(&html, "Выполнено"), message_id)
        .await;

    let event_id = latest_completion_event(&db.pool, item.id)
        .await
        .expect("completion event");
    let rows = notice_closed_rows(&db.pool, bot.chat, message_id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert!(rows[0].0.is_some(), "{rows:?}");
    assert_eq!(rows[0].1.as_deref(), Some("completed"), "{rows:?}");
    assert_eq!(rows[0].2, Some(event_id), "{rows:?}");

    bot.advance(Duration::minutes(5));
    bot.press_data(&button_of(&bot.latest().1, "Отменить"), message_id)
        .await;
    let rows = notice_closed_rows(&db.pool, bot.chat, message_id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0], (None, None, None), "{rows:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn done_from_reminder_leaves_other_recipients_deliveries_open() {
    let db = isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let at = next_moscow_time(&bot, 9, 0);
    bot.freeze_at(at);

    // Второй участник household со своим чатом и включёнными напоминаниями.
    let member_telegram = next_telegram_id();
    let member_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO users (
             id, household_id, telegram_user_id, telegram_chat_id, display_name,
             role, settings, created_at, updated_at
         )
         VALUES (
             $1, $2, $3, $3, 'Masha', 'member',
             jsonb_build_object(
                 'notifications',
                 jsonb_build_object('digest_enabled', false, 'quiet_hours_enabled', false)
             ),
             $4, $4
         )",
    )
    .bind(member_id)
    .bind(household.household_id)
    .bind(member_telegram)
    .bind(at)
    .execute(&db.pool)
    .await
    .expect("member");
    let mut member = Harness::sharing(&db.pool, member_telegram, bot.fake.clone()).named("Masha");
    member.freeze_at(at);

    // Общая задача без исполнителя: напоминание приходит обоим участникам.
    let mut new = new_item(household.tasks_list_id, "Общая задача");
    new.due_at = Some(at + Duration::minutes(15));
    let item = create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &Actor::user(household.owner_id, UserRole::Owner),
        new,
    )
    .await;

    tick_and_deliver(&mut bot, &db.pool, at).await;
    member.deliver().await;
    let (message_id, html) = bot.latest();
    let (member_message_id, member_html) = member.latest();
    assert!(member_html.contains("Общая задача"), "{member_html}");

    bot.press_data(&button_of(&html, "Выполнено"), message_id)
        .await;

    let event_id = latest_completion_event(&db.pool, item.id)
        .await
        .expect("completion event");
    let rows = notice_closed_rows(&db.pool, bot.chat, message_id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert!(rows[0].0.is_some(), "{rows:?}");
    assert_eq!(rows[0].1.as_deref(), Some("completed"), "{rows:?}");
    assert_eq!(rows[0].2, Some(event_id), "{rows:?}");
    let member_rows = notice_closed_rows(&db.pool, member.chat, member_message_id).await;
    assert_eq!(member_rows.len(), 1, "{member_rows:?}");
    assert_eq!(member_rows[0], (None, None, None), "{member_rows:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn done_on_a_task_completed_from_the_card_marks_its_delivery_closed() {
    let db = isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let at = next_moscow_time(&bot, 12, 0);
    bot.freeze_at(at);
    let item = create_task(
        &db.pool,
        &household,
        "Оплатить интернет",
        at + Duration::minutes(15),
        None,
    )
    .await;

    tick_and_deliver(&mut bot, &db.pool, at).await;
    let (message_id, html) = bot.latest();
    let done = button_of(&html, "Выполнено");

    // Задачу выполнили из карточки: кнопка в напоминании устарела по версии.
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
    .expect("complete from the card");
    drop(conn);
    let event_id = latest_completion_event(&db.pool, item.id)
        .await
        .expect("completion event");

    bot.press_data(&done, message_id).await;
    assert_eq!(last_toast(&bot).as_deref(), Some("Задача уже выполнена"));
    let rows = notice_closed_rows(&db.pool, bot.chat, message_id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    let closed_at = rows[0].0.expect("closed_at must be set");
    assert!(
        (closed_at - at).num_seconds().abs() <= 5,
        "closed_at {closed_at}, expected about {at}"
    );
    assert_eq!(rows[0].1.as_deref(), Some("completed"), "{rows:?}");
    assert_eq!(rows[0].2, Some(event_id), "{rows:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_undo_keeps_the_delivery_closed() {
    let db = isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let at = next_moscow_time(&bot, 9, 0);
    bot.freeze_at(at);
    let item = create_task(
        &db.pool,
        &household,
        "Оплатить интернет",
        at + Duration::minutes(15),
        None,
    )
    .await;

    tick_and_deliver(&mut bot, &db.pool, at).await;
    let (message_id, html) = bot.latest();
    bot.press_data(&button_of(&html, "Выполнено"), message_id)
        .await;
    let event_id = latest_completion_event(&db.pool, item.id)
        .await
        .expect("completion event");
    let undo = button_of(&bot.latest().1, "Отменить");

    // 09:11: окно отмены истекло.
    bot.advance(Duration::minutes(11));
    bot.press_data(&undo, message_id).await;

    assert_eq!(last_toast(&bot).as_deref(), Some("Время отмены истекло"));
    let rows = notice_closed_rows(&db.pool, bot.chat, message_id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    let closed_at = rows[0].0.expect("closed_at must stay set");
    assert!(
        (closed_at - at).num_seconds().abs() <= 5,
        "closed_at {closed_at}, expected about {at}"
    );
    assert_eq!(rows[0].1.as_deref(), Some("completed"), "{rows:?}");
    assert_eq!(rows[0].2, Some(event_id), "{rows:?}");
}

/// `replaces_message_id` snooze-доставок задачи в порядке создания.
async fn snooze_replaces(pool: &PgPool, item_id: Uuid) -> Vec<Option<i64>> {
    sqlx::query_scalar(
        "SELECT replaces_message_id FROM notification_deliveries
         WHERE item_id = $1 AND kind = 'snooze'
         ORDER BY created_at, id",
    )
    .bind(item_id)
    .fetch_all(pool)
    .await
    .expect("snooze replaces")
}

/// Строки удаления с ключом замены: статус и payload.
async fn replace_delete_rows(pool: &PgPool, chat: i64) -> Vec<(String, serde_json::Value)> {
    sqlx::query_as(
        "SELECT status::text, payload FROM outbox
         WHERE chat_id = $1 AND kind = 'delete' AND dedupe_key LIKE 'replace:%'
         ORDER BY seq",
    )
    .bind(chat)
    .fetch_all(pool)
    .await
    .expect("replace delete rows")
}

/// Новый токен snooze, как у кнопки после перерисовки сообщения.
async fn fresh_snooze_token(
    pool: &PgPool,
    bot: &Harness,
    household: &TestHousehold,
    item_id: Uuid,
    version: i32,
    until: SnoozeUntil,
) -> String {
    let action = Action::Snooze {
        item_id,
        until,
        version: Some(version),
    };
    let specs = [NewToken {
        telegram_user_id: bot.chat,
        household_id: Some(household.household_id),
        scope: Scope::Notice,
        action: action.name(),
        payload: serde_json::to_value(&action).expect("payload"),
        version: action.version(),
    }];
    let mut conn = pool.acquire().await.expect("conn");
    callbacks::issue_many(&mut conn, &specs, bot.now)
        .await
        .expect("snooze token")
        .remove(0)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snooze_records_the_message_it_will_replace() {
    let db = isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let at = next_moscow_time(&bot, 9, 0);
    bot.freeze_at(at);
    let item = create_task(
        &db.pool,
        &household,
        "Позвонить маме",
        at + Duration::minutes(15),
        None,
    )
    .await;

    tick_and_deliver(&mut bot, &db.pool, at).await;
    let (message_id, html) = bot.latest();
    bot.press_data(&button_of(&html, "на час"), message_id)
        .await;

    assert_eq!(
        snooze_replaces(&db.pool, item.id).await,
        vec![Some(message_id)],
        "the snooze must replace the message where the button was pressed"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeating_the_same_snooze_from_the_same_message_queues_no_delete() {
    let db = isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let at = next_moscow_time(&bot, 9, 0);
    bot.freeze_at(at);
    let item = create_task(
        &db.pool,
        &household,
        "Позвонить маме",
        at + Duration::minutes(15),
        None,
    )
    .await;

    tick_and_deliver(&mut bot, &db.pool, at).await;
    let (message_id, html) = bot.latest();
    bot.press_data(&button_of(&html, "на час"), message_id)
        .await;

    // Повтор того же snooze из того же сообщения: новый токен, тот же id.
    let version = item_status(&db.pool, item.id).await.2;
    let again = fresh_snooze_token(
        &db.pool,
        &bot,
        &household,
        item.id,
        version,
        SnoozeUntil::OneHour,
    )
    .await;
    bot.press_data(&again, message_id).await;
    assert_eq!(
        snooze_replaces(&db.pool, item.id).await,
        vec![Some(message_id)]
    );
    assert!(
        replace_delete_rows(&db.pool, bot.chat).await.is_empty(),
        "the same message must not be deleted"
    );

    // Отмена прежнего и повтор из того же сообщения: равный id не удаляется.
    sqlx::query("UPDATE items SET due_at = $2, version = version + 1 WHERE id = $1")
        .bind(item.id)
        .bind(at + Duration::minutes(10))
        .execute(&db.pool)
        .await
        .expect("move due");
    // Отмена snooze сама ставит удаление сообщения «😴 Напомню …»; payload
    // проверяется до доставки, пока он не скрыт.
    let stats = scheduler(&db.pool).tick(at).await.expect("cancel tick");
    assert!(stats.cancelled >= 1, "{stats:?}");
    let deletes = replace_delete_rows(&db.pool, bot.chat).await;
    assert_eq!(deletes.len(), 1, "{deletes:?}");
    assert_eq!(deletes[0].1["message_id"], json!(message_id), "{deletes:?}");
    bot.deliver().await;
    let version = item_status(&db.pool, item.id).await.2;
    let again = fresh_snooze_token(
        &db.pool,
        &bot,
        &household,
        item.id,
        version,
        SnoozeUntil::OneHour,
    )
    .await;
    bot.press_data(&again, message_id).await;
    let rows = snooze_rows(&db.pool, item.id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "pending", "{rows:?}");
    assert_eq!(
        snooze_replaces(&db.pool, item.id).await,
        vec![Some(message_id)]
    );
    // Повтор из того же сообщения не добавляет второе удаление.
    let deletes = replace_delete_rows(&db.pool, bot.chat).await;
    assert_eq!(deletes.len(), 1, "{deletes:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snooze_after_cancel_from_another_message_deletes_the_overwritten_one() {
    let db = isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let at = next_moscow_time(&bot, 9, 0);
    bot.freeze_at(at);
    let item = create_task(
        &db.pool,
        &household,
        "Позвонить маме",
        at + Duration::minutes(15),
        None,
    )
    .await;

    tick_and_deliver(&mut bot, &db.pool, at).await;
    let (first_id, html) = bot.latest();
    bot.press_data(&button_of(&html, "на час"), first_id).await;
    let snoozed_at = snooze_rows(&db.pool, item.id).await[0].1;
    assert_eq!(
        snooze_replaces(&db.pool, item.id).await,
        vec![Some(first_id)]
    );

    // Срок перенесён: тик отменяет прежний snooze и выпускает напоминание
    // о новом сроке — второе сообщение по той же задаче. Отмена сразу ставит
    // удаление сообщения «😴 Напомню …»; payload проверяется до доставки.
    sqlx::query("UPDATE items SET due_at = $2, version = version + 1 WHERE id = $1")
        .bind(item.id)
        .bind(at + Duration::minutes(10))
        .execute(&db.pool)
        .await
        .expect("move due");
    let stats = scheduler(&db.pool).tick(at).await.expect("cancel tick");
    assert!(stats.cancelled >= 1, "{stats:?}");
    assert_eq!(snooze_rows(&db.pool, item.id).await[0].0, "cancelled");
    let deletes = replace_delete_rows(&db.pool, bot.chat).await;
    assert_eq!(deletes.len(), 1, "{deletes:?}");
    assert_eq!(deletes[0].1["message_id"], json!(first_id), "{deletes:?}");
    assert_eq!(deletes[0].1["cleanup"], json!(true), "{deletes:?}");
    let key: String = sqlx::query_scalar(
        "SELECT dedupe_key FROM outbox
         WHERE chat_id = $1 AND kind = 'delete' AND dedupe_key LIKE 'replace:%'",
    )
    .bind(bot.chat)
    .fetch_one(&db.pool)
    .await
    .expect("replace key");
    assert_eq!(key, format!("replace:{}:{first_id}", bot.chat));

    bot.deliver().await;
    let (second_id, html) = bot.latest();
    assert_ne!(second_id, first_id);
    assert!(
        deleted(&bot, first_id),
        "the cancelled snooze message must be deleted"
    );

    // Тот же час snooze из другого сообщения: поле обновлено. Удаление прежнего
    // сообщения тем же ключом уже поставлено отменой — вторая строка не создаётся.
    bot.press_data_pending(&button_of(&html, "на час"), second_id)
        .await;
    let rows = snooze_rows(&db.pool, item.id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "pending", "{rows:?}");
    assert_eq!(rows[0].1, snoozed_at, "{rows:?}");
    assert_eq!(
        snooze_replaces(&db.pool, item.id).await,
        vec![Some(second_id)]
    );
    let deletes = replace_delete_rows(&db.pool, bot.chat).await;
    assert_eq!(deletes.len(), 1, "{deletes:?}");

    // Доставки прежнего сообщения отмечены убранными; новое напоминание и
    // сама snooze-доставка — нет.
    let states = delivery_states(&db.pool, item.id, "reminder").await;
    assert_eq!(states.len(), 2, "{states:?}");
    assert!(
        states[0].1.is_some(),
        "the first reminder must be dismissed: {states:?}"
    );
    assert!(
        states[1].1.is_none(),
        "the second reminder must stay: {states:?}"
    );
    let snooze_dismissed: Option<DateTime<Utc>> = sqlx::query_scalar(
        "SELECT dismissed_at FROM notification_deliveries
         WHERE item_id = $1 AND kind = 'snooze'",
    )
    .bind(item.id)
    .fetch_one(&db.pool)
    .await
    .expect("snooze dismissed_at");
    assert!(snooze_dismissed.is_none(), "{snooze_dismissed:?}");

    bot.deliver().await;
    assert!(
        deleted(&bot, first_id),
        "the overwritten message must be deleted"
    );
    assert!(
        bot.fake
            .messages(bot.chat)
            .into_iter()
            .any(|(id, _)| id == second_id),
        "the new reminder must stay in the chat"
    );
}

/// Два живых уведомления по одной задаче: напоминание о прежнем сроке (A) и
/// напоминание о новом сроке (B). Snooze на то же время из B при ожидающем
/// snooze из A записывает B, а A удаляется сразу (0017, решение 5, гейт).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snooze_from_another_live_message_replaces_the_pending_one() {
    let db = isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let at = next_moscow_time(&bot, 9, 0);
    bot.freeze_at(at);
    let item = create_task(
        &db.pool,
        &household,
        "Позвонить маме",
        at + Duration::minutes(15),
        None,
    )
    .await;

    // A — напоминание о первоначальном сроке.
    tick_and_deliver(&mut bot, &db.pool, at).await;
    let (first_id, _) = bot.latest();

    // Срок перенесён: тик выпускает B о новом сроке, A остаётся в чате
    // (отправленные напоминания не отменяются).
    sqlx::query("UPDATE items SET due_at = $2, version = version + 1 WHERE id = $1")
        .bind(item.id)
        .bind(at + Duration::minutes(10))
        .execute(&db.pool)
        .await
        .expect("move due");
    tick_and_deliver(&mut bot, &db.pool, at).await;
    let (second_id, html) = bot.latest();
    assert_ne!(second_id, first_id);
    assert!(
        bot.fake
            .messages(bot.chat)
            .into_iter()
            .any(|(id, _)| id == first_id),
        "the first reminder must stay in the chat"
    );

    // Snooze «Утром» из A: токен кнопки устарел, выпускаем новый.
    let version = item_status(&db.pool, item.id).await.2;
    let from_first = fresh_snooze_token(
        &db.pool,
        &bot,
        &household,
        item.id,
        version,
        SnoozeUntil::TomorrowMorning,
    )
    .await;
    bot.press_data(&from_first, first_id).await;
    let snoozed_at = snooze_rows(&db.pool, item.id).await[0].1;
    assert_eq!(
        snooze_replaces(&db.pool, item.id).await,
        vec![Some(first_id)]
    );

    // Тот же snooze из B: актуальным становится последнее сообщение.
    bot.press_data_pending(&button_of(&html, "Завтра в 09:00"), second_id)
        .await;
    let rows = snooze_rows(&db.pool, item.id).await;
    assert_eq!(rows.len(), 1, "one snooze record: {rows:?}");
    assert_eq!(rows[0].0, "pending", "{rows:?}");
    assert_eq!(
        rows[0].1, snoozed_at,
        "the snooze time must not move: {rows:?}"
    );
    assert_eq!(
        snooze_replaces(&db.pool, item.id).await,
        vec![Some(second_id)],
        "the pending snooze must replace the latest message"
    );

    // A удаляется сразу ключом замены; payload цел до доставки.
    let deletes = replace_delete_rows(&db.pool, bot.chat).await;
    assert_eq!(deletes.len(), 1, "{deletes:?}");
    assert_eq!(deletes[0].0, "pending", "{deletes:?}");
    assert_eq!(deletes[0].1["message_id"], json!(first_id), "{deletes:?}");
    assert_eq!(deletes[0].1["cleanup"], json!(true), "{deletes:?}");
    let key: String = sqlx::query_scalar(
        "SELECT dedupe_key FROM outbox
         WHERE chat_id = $1 AND kind = 'delete' AND dedupe_key LIKE 'replace:%'",
    )
    .bind(bot.chat)
    .fetch_one(&db.pool)
    .await
    .expect("replace key");
    assert_eq!(key, format!("replace:{}:{first_id}", bot.chat));

    // Доставки A отмечены убранными; B и сама snooze-доставка — ещё нет.
    let states = delivery_states(&db.pool, item.id, "reminder").await;
    assert_eq!(states.len(), 2, "{states:?}");
    assert!(
        states[0].1.is_some(),
        "the first reminder must be dismissed: {states:?}"
    );
    assert!(
        states[1].1.is_none(),
        "the second reminder must stay: {states:?}"
    );
    let snooze_dismissed: Option<DateTime<Utc>> = sqlx::query_scalar(
        "SELECT dismissed_at FROM notification_deliveries
         WHERE item_id = $1 AND kind = 'snooze'",
    )
    .bind(item.id)
    .fetch_one(&db.pool)
    .await
    .expect("snooze dismissed_at");
    assert!(snooze_dismissed.is_none(), "{snooze_dismissed:?}");

    bot.deliver().await;
    assert!(
        deleted(&bot, first_id),
        "the overwritten message must be deleted"
    );

    // Доставка snooze: новое напоминание заменяет B, а не A.
    let stats = scheduler(&db.pool)
        .tick(snoozed_at)
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
    assert_eq!(payload["replaces"], json!([second_id]), "{payload}");

    bot.advance(snoozed_at - bot.now);
    bot.deliver().await;
    assert!(
        deleted(&bot, second_id),
        "the latest message must be replaced"
    );
    assert_eq!(
        message_count(&bot, "Позвонить маме"),
        1,
        "one message per task must remain in the chat"
    );
    let rows = snooze_rows(&db.pool, item.id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "sent", "{rows:?}");
}

/// Snooze из сводного сообщения (пустой `replaces_message_id`) на то же время
/// не трогает ожидающую строку: записанное сообщение остаётся (0017, решение 5).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn insert_snooze_with_an_empty_new_message_keeps_the_recorded_one() {
    let db = isolated_db().await;
    let bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let due = rounded_now() + Duration::hours(3);
    let item = create_task(&db.pool, &household, "Позвонить маме", due, None).await;
    let deliver_at = rounded_now() + Duration::hours(1);
    let key = notifications::snooze_key(deliver_at);
    let origin = tasklerq::jobs::scheduler::occurrence_key(due);

    let mut conn = db.pool.acquire().await.expect("conn");
    let mut delivery = NewDelivery {
        household_id: household.household_id,
        user_id: household.owner_id,
        item_id: Some(item.id),
        kind: NotificationKind::Snooze,
        occurrence_key: &key,
        origin_key: Some(&origin),
        replaces_message_id: Some(101),
        scheduled_at: deliver_at,
    };
    assert_eq!(
        notifications::insert_snooze(&mut conn, &delivery, false, deliver_at)
            .await
            .expect("first snooze"),
        None,
        "a fresh snooze has nothing to overwrite"
    );

    // Snooze из блока сводного на то же время: поле пустое, строка не меняется.
    delivery.replaces_message_id = None;
    assert_eq!(
        notifications::insert_snooze(&mut conn, &delivery, true, deliver_at)
            .await
            .expect("digest snooze"),
        None,
        "an empty new message must not overwrite the recorded one"
    );
    let (status, scheduled, batched, replaces): (String, DateTime<Utc>, bool, Option<i64>) =
        sqlx::query_as(
            "SELECT status::text, scheduled_at, batched, replaces_message_id
             FROM notification_deliveries
             WHERE item_id = $1 AND kind = 'snooze'",
        )
        .bind(item.id)
        .fetch_one(&db.pool)
        .await
        .expect("snooze row");
    assert_eq!(status, "pending");
    assert_eq!(scheduled, deliver_at);
    assert_eq!(replaces, Some(101), "the recorded message must stay");
    assert!(!batched, "the pending row must not be touched");
}

/// Записанное сообщение пустое, новое непустое: snooze из отдельного
/// уведомления заполняет поле, удалять нечего (0017, решение 5).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn insert_snooze_fills_an_empty_recorded_message_without_overwrite() {
    let db = isolated_db().await;
    let bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let due = rounded_now() + Duration::hours(3);
    let item = create_task(&db.pool, &household, "Позвонить маме", due, None).await;
    let deliver_at = rounded_now() + Duration::hours(1);
    let key = notifications::snooze_key(deliver_at);
    let origin = tasklerq::jobs::scheduler::occurrence_key(due);

    let mut conn = db.pool.acquire().await.expect("conn");
    let mut delivery = NewDelivery {
        household_id: household.household_id,
        user_id: household.owner_id,
        item_id: Some(item.id),
        kind: NotificationKind::Snooze,
        occurrence_key: &key,
        origin_key: Some(&origin),
        replaces_message_id: None,
        scheduled_at: deliver_at,
    };
    notifications::insert_snooze(&mut conn, &delivery, true, deliver_at)
        .await
        .expect("digest snooze");

    delivery.replaces_message_id = Some(202);
    assert_eq!(
        notifications::insert_snooze(&mut conn, &delivery, false, deliver_at)
            .await
            .expect("single snooze"),
        None,
        "an empty recorded message leaves nothing to delete"
    );
    let (scheduled, replaces): (DateTime<Utc>, Option<i64>) = sqlx::query_as(
        "SELECT scheduled_at, replaces_message_id FROM notification_deliveries
         WHERE item_id = $1 AND kind = 'snooze'",
    )
    .bind(item.id)
    .fetch_one(&db.pool)
    .await
    .expect("snooze row");
    assert_eq!(scheduled, deliver_at);
    assert_eq!(replaces, Some(202), "the new message must be recorded");
}
