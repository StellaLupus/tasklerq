//! Действия в сводном сообщении: готово, завершить все, snooze и открытие.

mod support;

use chrono::{DateTime, Duration, Utc};
use support::notifications::*;
use support::telegram::buttons;
use tasklerq::domain::models::{Actor, Item, RecurrenceFreq, UserRole};
use tasklerq::jobs::scheduler::occurrence_key;
use tasklerq::storage::repositories::items;
use uuid::Uuid;

async fn create_event(
    pool: &sqlx::PgPool,
    household: &support::TestHousehold,
    title: &str,
    due_at: DateTime<Utc>,
    end_at: DateTime<Utc>,
) -> Item {
    let mut item = support::new_item(household.events_list_id, title);
    item.due_at = Some(due_at);
    item.end_at = Some(end_at);
    item.assignee_user_id = Some(household.owner_id);
    support::create_item(
        pool,
        household,
        household.events_list_id,
        &Actor::user(household.owner_id, UserRole::Owner),
        item,
    )
    .await
}

fn labels(html: &str) -> Vec<String> {
    buttons(html).into_iter().map(|(label, _)| label).collect()
}

async fn snooze_origin_rows(
    pool: &sqlx::PgPool,
    item_id: Uuid,
) -> Vec<(String, Option<String>, DateTime<Utc>, bool)> {
    sqlx::query_as(
        "SELECT status::text, origin_key, scheduled_at, batched
         FROM notification_deliveries
         WHERE item_id = $1 AND kind = 'snooze'
         ORDER BY created_at, id",
    )
    .bind(item_id)
    .fetch_all(pool)
    .await
    .expect("snooze rows")
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
    let done = button_of(&html, "Выполнено");
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
    let complete_all = button_of(&html, "Выполнить все");
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
    let again = button_of(&edited, "Выполнить все");
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
async fn snooze_all_waits_for_its_due_time_after_timezone_change() {
    let db = support::isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    let household = household_for(&db.pool, bot.chat).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = local_today(&bot, moscow);
    let at = local_utc(moscow, day, 9, 0);
    bot.advance(at - bot.now);
    quiet_settings(&db.pool, household.owner_id, false).await;

    let due = local_utc(moscow, day, 11, 0);
    for title in ["Первая", "Вторая"] {
        let item = create_task(&db.pool, &household, title, due, None).await;
        deferred_reminder(&db.pool, &household, item.id, due, at).await;
    }

    tick_and_deliver(&mut bot, &db.pool, at).await;
    let (message_id, html) = bot.latest();
    let snooze_all = button_of(&html, "Отложить все на час");
    bot.press_data(&snooze_all, message_id).await;
    let snoozed_at: DateTime<Utc> = sqlx::query_scalar(
        "SELECT min(scheduled_at) FROM notification_deliveries
         WHERE kind = 'snooze' AND status = 'pending'",
    )
    .fetch_one(&db.pool)
    .await
    .expect("snooze time");

    // 09:20 в Москве — 16:20 во Владивостоке: по новым настройкам не тихо.
    bot.advance(at + Duration::minutes(20) - bot.now);
    bot.press("Настройки").await;
    bot.press("Профиль").await;
    bot.press("Часовой пояс").await;
    let sent_before = bot.fake.sent().len();
    bot.press("Asia/Vladivostok").await;

    assert!(!bot.screen().contains("Накопилось"), "{}", bot.screen());
    assert_eq!(
        bot.fake.sent().len(),
        sent_before,
        "grouped snooze must not be released by a timezone change"
    );
    let pending: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM notification_deliveries
         WHERE kind = 'snooze' AND status = 'pending'",
    )
    .fetch_one(&db.pool)
    .await
    .expect("pending snooze");
    assert_eq!(pending, 2);

    assert_eq!(
        tick_and_deliver(&mut bot, &db.pool, snoozed_at)
            .await
            .delivered,
        1
    );
    let (_, next) = bot.latest();
    assert!(next.contains("Пока были тихие часы"), "{next}");
    assert_eq!(block_count(&next, "😴 отложенное"), 2, "{next}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grouped_snooze_that_falls_into_quiet_hours_becomes_quiet_deferred() {
    let db = support::isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    let household = household_for(&db.pool, bot.chat).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = local_today(&bot, moscow);
    let at = local_utc(moscow, day, 21, 30);
    bot.advance(at - bot.now);
    quiet_settings(&db.pool, household.owner_id, false).await;

    let due = local_utc(moscow, day, 23, 0);
    for title in ["Первая", "Вторая"] {
        let item = create_task(&db.pool, &household, title, due, None).await;
        deferred_reminder(&db.pool, &household, item.id, due, at).await;
    }

    tick_and_deliver(&mut bot, &db.pool, at).await;
    let (message_id, html) = bot.latest();
    let snooze_all = button_of(&html, "Отложить все на час");
    bot.press_data(&snooze_all, message_id).await;

    // 22:30 попали в тихие часы: общее правило переносит snooze на 08:00 и
    // помечает его отложенным тихими часами.
    tick_and_deliver(&mut bot, &db.pool, local_utc(moscow, day, 22, 0)).await;
    let rows: Vec<(DateTime<Utc>, bool)> = sqlx::query_as(
        "SELECT scheduled_at, deferred FROM notification_deliveries
         WHERE kind = 'snooze' AND status = 'pending' ORDER BY item_id",
    )
    .fetch_all(&db.pool)
    .await
    .expect("snooze rows");
    assert_eq!(rows.len(), 2, "{rows:?}");
    let morning = local_utc(moscow, day + Duration::days(1), 8, 0);
    assert!(
        rows.iter()
            .all(|(scheduled_at, deferred)| { *scheduled_at == morning && *deferred }),
        "{rows:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn complete_all_finishes_a_snoozed_block_and_cancels_its_snooze() {
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
    bot.press_data(&snooze, message_id).await;
    let (_, edited) = bot.latest();
    assert!(edited.contains("😴 Напомню в"), "{edited}");
    // Отложена одна из двух задач; «Выполнить все» должен взять и её блок.
    let (snoozed_item, snoozed_at): (Uuid, DateTime<Utc>) = sqlx::query_as(
        "SELECT item_id, scheduled_at FROM notification_deliveries
         WHERE kind = 'snooze' AND status = 'pending'",
    )
    .fetch_one(&db.pool)
    .await
    .expect("snooze delivery");

    let complete_all = button_of(&edited, "Выполнить все");
    bot.press_data(&complete_all, message_id).await;

    assert_eq!(item_status(&db.pool, first.id).await.0, "done");
    assert_eq!(item_status(&db.pool, second.id).await.0, "done");
    assert_eq!(item_status(&db.pool, snoozed_item).await.0, "done");
    let (_, result) = bot.latest();
    assert!(result.contains("Выполнено 2"), "{result}");

    // Отложенная доставка выполнена вместе с задачей: тик отменяет её как
    // устаревшую и ничего не присылает через час.
    let sent_before = bot.fake.sent().len();
    let stats = tick_and_deliver(&mut bot, &db.pool, snoozed_at).await;
    assert_eq!(stats.delivered, 0, "{stats:?}");
    assert_eq!(stats.cancelled, 1, "{stats:?}");
    assert_eq!(
        bot.fake.sent().len(),
        sent_before,
        "a completed task must not arrive in an hour"
    );
    assert_eq!(snooze_rows(&db.pool, snoozed_item).await[0].0, "cancelled");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_block_completed_from_the_card_stops_promising_a_cancelled_snooze() {
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
    bot.press_data(&button_of(&html, "На час"), message_id)
        .await;
    let (snoozed_item, snoozed_at): (Uuid, DateTime<Utc>) = sqlx::query_as(
        "SELECT item_id, scheduled_at FROM notification_deliveries
         WHERE kind = 'snooze' AND status = 'pending'",
    )
    .fetch_one(&db.pool)
    .await
    .expect("snooze delivery");
    let snoozed = batch_message(&bot, message_id);
    assert!(snoozed.contains("😴 Напомню в"), "{snoozed}");

    // Задачу без повторения выполнили с карточки, а не из сводного.
    let mut conn = db.pool.acquire().await.expect("conn");
    items::complete(
        &mut conn,
        household.household_id,
        &Actor::user(household.owner_id, UserRole::Owner),
        snoozed_item,
        None,
        "Europe/Moscow",
        bot.now,
    )
    .await
    .expect("complete from the card");

    // Тик отменяет snooze выполненной задачи ещё до его срока, но само
    // сообщение до перерисовки всё ещё обещает напоминание.
    let stats = tick_and_deliver(&mut bot, &db.pool, morning + Duration::minutes(5)).await;
    assert!(stats.cancelled >= 1, "{stats:?}");
    assert_eq!(snooze_rows(&db.pool, snoozed_item).await[0].0, "cancelled");
    let before = batch_message(&bot, message_id);
    assert!(before.contains("😴 Напомню в"), "{before}");

    // Перерисовка другим действием — «Выполнено» у второй задачи — не трогает
    // `batch_state` отложенного блока: его `Done` берётся только из задачи.
    let done_other = button_of(&before, "Выполнено");
    bot.press_data(&done_other, message_id).await;
    let edited = batch_message(&bot, message_id);
    assert_eq!(block_count(&edited, "✅ Выполнено"), 2, "{edited}");
    assert!(!edited.contains("😴 Напомню в"), "{edited}");

    // Через час отменённый snooze не приходит.
    let sent_before = bot.fake.sent().len();
    let stats = tick_and_deliver(&mut bot, &db.pool, snoozed_at).await;
    assert_eq!(stats.delivered, 0, "{stats:?}");
    assert_eq!(
        bot.fake.sent().len(),
        sent_before,
        "a task completed from the card must not remind in an hour"
    );
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
async fn snooze_all_waits_for_its_due_time_when_quiet_hours_change() {
    let db = support::isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    let household = household_for(&db.pool, bot.chat).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = local_today(&bot, moscow);
    let at = local_utc(moscow, day, 9, 0);
    bot.advance(at - bot.now);
    quiet_settings(&db.pool, household.owner_id, false).await;

    let due = local_utc(moscow, day, 11, 0);
    for title in ["Первая", "Вторая"] {
        let item = create_task(&db.pool, &household, title, due, None).await;
        deferred_reminder(&db.pool, &household, item.id, due, at).await;
    }

    tick_and_deliver(&mut bot, &db.pool, at).await;
    let (message_id, html) = bot.latest();
    let snooze_all = button_of(&html, "Отложить все на час");
    bot.press_data(&snooze_all, message_id).await;

    let snoozed_at: DateTime<Utc> = sqlx::query_scalar(
        "SELECT min(scheduled_at) FROM notification_deliveries
         WHERE kind = 'snooze' AND status = 'pending'",
    )
    .fetch_one(&db.pool)
    .await
    .expect("snooze time");
    assert_eq!(snoozed_at, at + Duration::hours(1));

    // 09:20: тихие часы выключены, но групповой snooze ещё не наступил.
    let later = at + Duration::minutes(20);
    bot.advance(later - bot.now);
    open_notifications(&mut bot).await;
    let sent_before = bot.fake.sent().len();
    bot.press_after("🌙 Тихие часы", "Выкл").await;

    assert!(!bot.screen().contains("Накопилось"), "{}", bot.screen());
    assert_eq!(
        bot.fake.sent().len(),
        sent_before,
        "grouped snooze must not be released by a quiet hours change"
    );
    let pending: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM notification_deliveries
         WHERE kind = 'snooze' AND status = 'pending'",
    )
    .fetch_one(&db.pool)
    .await
    .expect("pending snooze");
    assert_eq!(pending, 2);

    // 10:00: наступил срок группового snooze — приходит одно сводное.
    assert_eq!(
        tick_and_deliver(&mut bot, &db.pool, snoozed_at)
            .await
            .delivered,
        1
    );
    let (_, next) = bot.latest();
    assert!(next.contains("Пока были тихие часы"), "{next}");
    assert_eq!(block_count(&next, "😴 отложенное"), 2, "{next}");
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
    let done = button_of(&html, "Выполнено");
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
    let after_press = bot.now;
    let (_, edited) = bot.latest();

    let rows = snooze_rows(&db.pool, first.id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "pending");
    let snoozed_at = rows[0].1;

    // `Harness::tick` двигает часы стенда при каждом `enqueue`/`pump`, поэтому
    // обработчик видит `now` где-то между `pressed_at` и `after_press`, а не
    // ровно время чтения `bot.now` до нажатия. Snooze — это `now + 1 ч`; время
    // в ответе берём из самой записи, а запись проверяем интервалом.
    let lower = pressed_at.timestamp() + Duration::hours(1).num_seconds();
    let upper = after_press.timestamp() + Duration::hours(1).num_seconds();
    assert!(
        (lower..=upper).contains(&snoozed_at.timestamp()),
        "snooze at {snoozed_at} must be within [{lower}, {upper}]"
    );
    let expected = snoozed_at
        .with_timezone(&moscow)
        .format("%H:%M")
        .to_string();
    assert!(
        edited.contains(&format!("😴 Напомню в {expected}")),
        "{edited}"
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
    let complete_all = button_of(&html, "Выполнить все");
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
    let again = button_of(&edited, "Выполнить все");
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
    let done = button_of(&html, "Выполнено");
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

    // «Выполнено» по устаревшей версии не завершает, а перерисовывает блок.
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
        .filter(|(label, _)| label.contains("Выполнено"))
        .map(|(_, data)| data)
        .collect();
    assert_eq!(done_buttons.len(), 2, "{edited}");

    // «Выполнено» по новой версии завершает задачу.
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
    let done = button_of(&html, "Выполнено");
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

/// После «Выполнить все» блоки, по которым действие уже невозможно, не меняются.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn complete_all_keeps_done_missing_and_archived_blocks_unchanged() {
    let db = support::isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = bot.now.with_timezone(&moscow).date_naive();
    let morning = local_utc(moscow, day, 8, 0);
    let due = local_utc(moscow, day, 9, 0);

    let done = create_task(&db.pool, &household, "Выполнят заранее", due, None).await;
    let deleted = create_task(&db.pool, &household, "Удалят", due, None).await;
    let open = create_task(&db.pool, &household, "Останется", due, None).await;
    // Отдельный список задач, который после выпуска отправят в архив.
    let archived_list = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO lists (id, household_id, kind, name) VALUES ($1, $2, 'tasks', 'Архивный')",
    )
    .bind(archived_list)
    .bind(household.household_id)
    .execute(&db.pool)
    .await
    .expect("archived list");
    let mut archived_item = support::new_item(archived_list, "В архиве");
    archived_item.due_at = Some(due);
    archived_item.assignee_user_id = Some(household.owner_id);
    let archived = support::create_item(
        &db.pool,
        &household,
        archived_list,
        &household.owner(),
        archived_item,
    )
    .await;

    for item in [&done, &deleted, &open, &archived] {
        deferred_reminder(&db.pool, &household, item.id, due, morning).await;
    }
    tick_and_deliver(&mut bot, &db.pool, morning).await;
    let (message_id, html) = bot.latest();
    assert_eq!(block_count(&html, "⏰ напоминание"), 4, "{html}");

    let mut conn = db.pool.acquire().await.expect("conn");
    items::soft_delete(
        &mut conn,
        household.household_id,
        &Actor::user(household.owner_id, UserRole::Owner),
        deleted.id,
        bot.now,
    )
    .await
    .expect("delete");
    sqlx::query("UPDATE lists SET is_archived = true WHERE id = $1")
        .bind(archived_list)
        .execute(&db.pool)
        .await
        .expect("archive list");

    let done_button = button_of(&html, "Выполнено");
    bot.press_data(&done_button, message_id).await;
    let before = batch_message(&bot, message_id);
    assert!(before.contains("✅ Выполнено"), "{before}");
    assert!(before.contains("Задача удалена"), "{before}");
    assert!(before.contains("Список в архиве"), "{before}");
    let done_version = item_status(&db.pool, done.id).await.1;
    let archived_version = item_status(&db.pool, archived.id).await.1;

    let complete_all = button_of(&before, "Выполнить все");
    bot.press_data(&complete_all, message_id).await;

    assert_eq!(item_status(&db.pool, open.id).await.0, "done");
    assert_eq!(item_status(&db.pool, done.id).await.1, done_version);
    assert_eq!(item_status(&db.pool, archived.id).await.1, archived_version);
    assert_eq!(item_status(&db.pool, archived.id).await.0, "open");
    let deleted_at: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT deleted_at FROM items WHERE id = $1")
            .bind(deleted.id)
            .fetch_one(&db.pool)
            .await
            .expect("deleted item");
    assert!(deleted_at.is_some());

    let after = batch_message(&bot, message_id);
    assert!(after.contains("Выполнено 1"), "{after}");
    assert!(after.contains("✅ Выполнено"), "{after}");
    assert!(after.contains("Задача удалена"), "{after}");
    assert!(after.contains("Список в архиве"), "{after}");

    let again = button_of(&after, "Выполнить все");
    bot.press_data(&again, message_id).await;
    // Кнопки получают новые callback-токены на каждую отрисовку: сравниваем
    // видимую часть сообщения.
    let visible = |html: &str| {
        html.split("<tg-button-row>")
            .next()
            .unwrap_or(html)
            .to_owned()
    };
    assert_eq!(
        visible(&batch_message(&bot, message_id)),
        visible(&after),
        "a repeated «Выполнить все» must not change the message"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_block_snooze_only_redraws_the_batch() {
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
    let stale = button_of(&html, "На час");
    sqlx::query("UPDATE items SET title = 'Первая (новая)', version = version + 1 WHERE id = $1")
        .bind(first.id)
        .execute(&db.pool)
        .await
        .expect("external change");

    bot.press_data(&stale, message_id).await;
    let toast = bot
        .fake
        .answers()
        .last()
        .expect("callback answer")
        .1
        .clone();
    assert_eq!(toast.as_deref(), Some("Задача уже изменилась"));
    assert!(
        snooze_rows(&db.pool, first.id).await.is_empty(),
        "a stale block must not create a snooze"
    );
    let (_, edited) = bot.latest();
    assert!(edited.contains("Первая (новая)"), "{edited}");
    assert!(!edited.contains("😴 Напомню в"), "{edited}");
    assert_eq!(block_count(&edited, "😴 На час"), 2, "{edited}");
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

    let done = button_of(&html, "Выполнено");
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snooze_all_of_an_assignment_without_due_records_the_no_due_origin() {
    let db = support::isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = bot.now.with_timezone(&moscow).date_naive();
    let morning = local_utc(moscow, day, 8, 0);
    let due = local_utc(moscow, day, 9, 0);

    let mut new = support::new_item(household.tasks_list_id, "Без срока");
    new.assignee_user_id = Some(household.owner_id);
    let no_due = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        new,
    )
    .await;
    let timed = create_task(&db.pool, &household, "Со сроком", due, None).await;
    insert_delivery(
        &db.pool,
        household.household_id,
        household.owner_id,
        no_due.id,
        "assignment",
        "assignment:no-due:0",
        morning,
        true,
    )
    .await;
    insert_delivery(
        &db.pool,
        household.household_id,
        household.owner_id,
        timed.id,
        "assignment",
        "assignment:no-due:1",
        morning,
        true,
    )
    .await;

    tick_and_deliver(&mut bot, &db.pool, morning).await;
    let (message_id, html) = bot.latest();
    let snooze_all = button_of(&html, "Отложить все на час");
    bot.press_data(&snooze_all, message_id).await;

    let origin: Option<String> = sqlx::query_scalar(
        "SELECT origin_key FROM notification_deliveries
         WHERE item_id = $1 AND kind = 'snooze'",
    )
    .bind(no_due.id)
    .fetch_one(&db.pool)
    .await
    .expect("snooze origin");
    assert_eq!(origin.as_deref(), Some("no-due"));

    // Через час отложенное напоминание о задаче без срока доставляется.
    let snoozed_at: DateTime<Utc> = sqlx::query_scalar(
        "SELECT scheduled_at FROM notification_deliveries
         WHERE item_id = $1 AND kind = 'snooze'",
    )
    .bind(no_due.id)
    .fetch_one(&db.pool)
    .await
    .expect("snooze time");
    assert_eq!(
        tick_and_deliver(&mut bot, &db.pool, snoozed_at)
            .await
            .delivered,
        1
    );
    let rows = snooze_rows(&db.pool, no_due.id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "sent", "{rows:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_snooze_block_opens_with_the_current_version_and_snoozes_again() {
    let db = support::isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = bot.now.with_timezone(&moscow).date_naive();
    let morning = local_utc(moscow, day, 8, 0);
    // Нажатия кнопок обрабатываются по часам стенда (`bot.now`), а не по
    // времени тика. Ставим часы на утро сценария: днём утро и новый срок уже
    // в прошлом, и оба snooze получают одинаковое время «сейчас + час»,
    // сливаясь в одну запись. Так сценарий одинаков в любое время суток.
    bot.set_now(morning);
    let due = local_utc(moscow, day, 9, 0);
    let first = create_task(&db.pool, &household, "Первая", due, None).await;
    let second = create_task(&db.pool, &household, "Вторая", due, None).await;
    deferred_reminder(&db.pool, &household, first.id, due, morning).await;
    deferred_reminder(&db.pool, &household, second.id, due, morning).await;

    tick_and_deliver(&mut bot, &db.pool, morning).await;
    let (message_id, html) = bot.latest();
    bot.press_data(&button_of(&html, "На час"), message_id)
        .await;
    let snoozed_once = batch_message(&bot, message_id);
    assert!(snoozed_once.contains("😴 Напомню в"), "{snoozed_once}");

    let new_due = local_utc(moscow, day, 12, 0);
    sqlx::query("UPDATE items SET due_at = $2, title = 'Первая (новая)', version = version + 1 WHERE id = $1")
        .bind(first.id)
        .bind(new_due)
        .execute(&db.pool)
        .await
        .expect("move due");
    tick_and_deliver(&mut bot, &db.pool, morning + Duration::minutes(5)).await;
    let rows = snooze_origin_rows(&db.pool, first.id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "cancelled", "{rows:?}");

    // Действие по второй задаче перерисовывает весь блок: отменённый snooze
    // больше не показывается как «Напомню в …».
    bot.press_data(&button_of(&snoozed_once, "На час"), message_id)
        .await;
    let edited = batch_message(&bot, message_id);
    assert!(edited.contains("Первая (новая)"), "{edited}");
    let snooze_buttons: Vec<String> = buttons(&edited)
        .into_iter()
        .filter(|(label, _)| label.contains("На час"))
        .map(|(_, data)| data)
        .collect();
    assert_eq!(snooze_buttons.len(), 1, "{edited}");

    let version = item_status(&db.pool, first.id).await.1.to_string();
    let issued: Option<String> = sqlx::query_scalar(
        "SELECT payload->>'version' FROM callback_tokens
         WHERE action = 'batch_snooze' AND payload->>'item_id' = $1
           AND consumed_at IS NULL
         ORDER BY created_at DESC, token_hash DESC LIMIT 1",
    )
    .bind(first.id.to_string())
    .fetch_optional(&db.pool)
    .await
    .expect("issued version");
    assert_eq!(
        issued.as_deref(),
        Some(version.as_str()),
        "the redrawn block must carry the current task version"
    );

    let pressed_at = bot.now;
    bot.press_data(&snooze_buttons[0], message_id).await;
    let rows = snooze_origin_rows(&db.pool, first.id).await;
    assert_eq!(rows.len(), 2, "{rows:?}");
    let pending = rows
        .iter()
        .find(|row| row.0 == "pending")
        .expect("new snooze");
    assert_eq!(pending.1.as_deref(), Some(occurrence_key(new_due).as_str()));
    assert_eq!(
        pending.2.timestamp(),
        (pressed_at + Duration::hours(1)).timestamp()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snooze_all_takes_a_block_whose_snooze_was_cancelled() {
    let db = support::isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = bot.now.with_timezone(&moscow).date_naive();
    let morning = local_utc(moscow, day, 8, 0);
    // Нажатия кнопок обрабатываются по часам стенда (`bot.now`), а не по
    // времени тика. Ставим часы на утро сценария: днём утро и новый срок уже
    // в прошлом, и оба snooze получают одинаковое время «сейчас + час»,
    // сливаясь в одну запись. Так сценарий одинаков в любое время суток.
    bot.set_now(morning);
    let due = local_utc(moscow, day, 9, 0);
    let first = create_task(&db.pool, &household, "Первая", due, None).await;
    let second = create_task(&db.pool, &household, "Вторая", due, None).await;
    deferred_reminder(&db.pool, &household, first.id, due, morning).await;
    deferred_reminder(&db.pool, &household, second.id, due, morning).await;

    tick_and_deliver(&mut bot, &db.pool, morning).await;
    let (message_id, html) = bot.latest();
    bot.press_data(&button_of(&html, "На час"), message_id)
        .await;

    let new_due = local_utc(moscow, day, 12, 0);
    sqlx::query("UPDATE items SET due_at = $2, version = version + 1 WHERE id = $1")
        .bind(first.id)
        .bind(new_due)
        .execute(&db.pool)
        .await
        .expect("move due");
    tick_and_deliver(&mut bot, &db.pool, morning + Duration::minutes(5)).await;

    let snooze_all = button_of(&batch_message(&bot, message_id), "Отложить все на час");
    bot.press_data(&snooze_all, message_id).await;

    let rows = snooze_origin_rows(&db.pool, first.id).await;
    assert_eq!(rows.len(), 2, "{rows:?}");
    let pending = rows
        .iter()
        .find(|row| row.0 == "pending")
        .expect("new grouped snooze");
    assert_eq!(pending.1.as_deref(), Some(occurrence_key(new_due).as_str()));
    assert!(pending.3, "«Отложить все на час» keeps the grouped flag");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn complete_all_with_a_snoozed_recurring_block_cancels_the_snooze() {
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
    bot.press_data(&button_of(&html, "На час"), message_id)
        .await;
    let snoozed_at = snooze_rows(&db.pool, recurring.id).await[0].1;

    let complete_all = button_of(&batch_message(&bot, message_id), "Выполнить все");
    bot.press_data(&complete_all, message_id).await;

    let (status, version, next_due) = item_status(&db.pool, recurring.id).await;
    assert_eq!(
        status, "open",
        "recurring task moves to its next occurrence"
    );
    assert_eq!(version, recurring.version + 1);
    assert_eq!(next_due, Some(due + Duration::days(1)));

    let sent_before = bot.fake.sent().len();
    let stats = tick_and_deliver(&mut bot, &db.pool, snoozed_at).await;
    assert_eq!(stats.delivered, 0, "{stats:?}");
    assert_eq!(stats.cancelled, 1, "{stats:?}");
    assert_eq!(
        bot.fake.sent().len(),
        sent_before,
        "the cancelled snooze must not arrive in an hour"
    );
    assert_eq!(snooze_rows(&db.pool, recurring.id).await[0].0, "cancelled");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn complete_all_with_mixed_blocks_leaves_the_event_untouched() {
    let db = support::isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = bot.now.with_timezone(&moscow).date_naive();
    let morning = local_utc(moscow, day, 8, 0);
    let due = local_utc(moscow, day, 9, 0);
    let event_start = local_utc(moscow, day, 10, 0);
    let first = create_task(&db.pool, &household, "Первая", due, None).await;
    let second = create_task(&db.pool, &household, "Вторая", due, None).await;
    let event = create_event(
        &db.pool,
        &household,
        "Маникюр",
        event_start,
        event_start + Duration::hours(1),
    )
    .await;
    deferred_reminder(&db.pool, &household, first.id, due, morning).await;
    deferred_reminder(&db.pool, &household, second.id, due, morning).await;
    deferred_reminder(&db.pool, &household, event.id, event_start, morning).await;

    tick_and_deliver(&mut bot, &db.pool, morning).await;
    let (message_id, html) = bot.latest();
    assert!(html.contains("Маникюр"), "{html}");
    let labels = labels(&html);
    assert_eq!(
        labels
            .iter()
            .filter(|label| label.contains("Перенести"))
            .count(),
        1,
        "the event block has its own move button: {labels:?}"
    );
    assert_eq!(
        labels
            .iter()
            .filter(|label| label.contains("Открыть"))
            .count(),
        3,
        "{labels:?}"
    );
    assert_eq!(
        labels
            .iter()
            .filter(|label| label.contains("Выполнено"))
            .count(),
        2,
        "only task blocks offer «Выполнено»: {labels:?}"
    );
    assert_eq!(
        labels
            .iter()
            .filter(|label| label.contains("На час"))
            .count(),
        2,
        "only task blocks offer «На час»: {labels:?}"
    );

    let event_before = item_status(&db.pool, event.id).await;
    bot.press_data(&button_of(&html, "Выполнить все"), message_id)
        .await;

    assert_eq!(item_status(&db.pool, first.id).await.0, "done");
    assert_eq!(item_status(&db.pool, second.id).await.0, "done");
    assert_eq!(
        item_status(&db.pool, event.id).await,
        event_before,
        "«Выполнить все» must not touch an event"
    );
    assert_eq!(completed_events(&db.pool, event.id).await, 0);

    let (_, edited) = bot.latest();
    assert!(edited.contains("Выполнено 2"), "{edited}");
    assert_eq!(block_count(&edited, "✅ Выполнено"), 2, "{edited}");
    assert_eq!(block_count(&edited, "📅 Перенести"), 1, "{edited}");
    assert!(edited.contains("Маникюр"), "{edited}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snooze_all_with_mixed_blocks_creates_no_event_snooze() {
    let db = support::isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    quiet_off(&db.pool, bot.chat).await;
    let household = household_for(&db.pool, bot.chat).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = bot.now.with_timezone(&moscow).date_naive();
    let morning = local_utc(moscow, day, 8, 0);
    let due = local_utc(moscow, day, 9, 0);
    let event_start = local_utc(moscow, day, 10, 0);
    let task = create_task(&db.pool, &household, "Позвонить маме", due, None).await;
    let event = create_event(
        &db.pool,
        &household,
        "Маникюр",
        event_start,
        event_start + Duration::hours(1),
    )
    .await;
    deferred_reminder(&db.pool, &household, task.id, due, morning).await;
    deferred_reminder(&db.pool, &household, event.id, event_start, morning).await;

    tick_and_deliver(&mut bot, &db.pool, morning).await;
    let (message_id, html) = bot.latest();
    assert!(html.contains("Маникюр"), "{html}");
    let event_before = item_status(&db.pool, event.id).await;

    bot.press_data(&button_of(&html, "Отложить все на час"), message_id)
        .await;

    let task_rows = snooze_rows(&db.pool, task.id).await;
    assert_eq!(task_rows.len(), 1, "{task_rows:?}");
    assert_eq!(task_rows[0].0, "pending", "{task_rows:?}");
    assert!(
        snooze_rows(&db.pool, event.id).await.is_empty(),
        "«Отложить все на час» must not snooze an event"
    );
    assert_eq!(item_status(&db.pool, event.id).await, event_before);

    let (_, edited) = bot.latest();
    assert_eq!(block_count(&edited, "😴 Напомню в"), 1, "{edited}");
    assert_eq!(block_count(&edited, "📅 Перенести"), 1, "{edited}");
    assert!(edited.contains("Маникюр"), "{edited}");

    // Через час приходит обычное напоминание только о задаче.
    let snoozed_at = task_rows[0].1;
    assert_eq!(
        tick_and_deliver(&mut bot, &db.pool, snoozed_at)
            .await
            .delivered,
        1
    );
    let (_, next) = bot.latest();
    assert!(next.contains("Позвонить маме"), "{next}");
    assert!(
        !next.contains("Маникюр"),
        "a snoozed event reminder must not be created: {next}"
    );
    assert!(snooze_rows(&db.pool, event.id).await.is_empty());
}
