//! Действия в сводном сообщении: готово, завершить все, snooze и открытие.

mod support;

use chrono::{DateTime, Duration, Utc};
use support::notifications::*;
use support::telegram::buttons;
use tasklerq::domain::models::{Actor, RecurrenceFreq, UserRole};
use tasklerq::storage::repositories::items;
use uuid::Uuid;

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
    bot.press("Время").await;
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
    // Отложена одна из двух задач; «Завершить все» должен взять и её блок.
    let (snoozed_item, snoozed_at): (Uuid, DateTime<Utc>) = sqlx::query_as(
        "SELECT item_id, scheduled_at FROM notification_deliveries
         WHERE kind = 'snooze' AND status = 'pending'",
    )
    .fetch_one(&db.pool)
    .await
    .expect("snooze delivery");

    let complete_all = button_of(&edited, "Завершить все");
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
    bot.press("Тихие часы: вкл").await;

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
