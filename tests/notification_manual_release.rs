//! Ручной выпуск отложенных тихими часами уведомлений из настроек.

mod support;

use std::time::{Duration as StdDuration, Instant};

use chrono::{DateTime, Duration, Utc};
use serde_json::json;
use support::notifications::*;
use support::telegram::{Call, Harness, buttons};
use tasklerq::jobs::digest::digest_occurrence_key;
use tasklerq::telegram::outbox::OutboxWorker;
use uuid::Uuid;

/// Последняя строка outbox чата — отметка перед действием.
async fn outbox_seq_before(bot: &Harness) -> i64 {
    sqlx::query_scalar("SELECT coalesce(max(seq), 0) FROM outbox WHERE chat_id = $1")
        .bind(bot.chat)
        .fetch_one(&bot.pool)
        .await
        .expect("outbox seq")
}

/// После `seq_before` не должно появиться ни одной строки вида `message`:
/// уведомление выпускается именно этим видом, а экран — строкой `screen`.
///
/// Экран настроек после кнопки на засыпанном якоре правится на месте
/// (`in_place`) и остаётся засыпанным, поэтому ответ формы приходит новым
/// экраном. Это не выпуск отложенного snooze.
async fn assert_no_notification_sent(bot: &Harness, seq_before: i64, message: &str) {
    let kinds: Vec<String> = sqlx::query_scalar(
        "SELECT kind::text FROM outbox
         WHERE chat_id = $1 AND seq > $2 AND kind = 'message'
         ORDER BY seq",
    )
    .bind(bot.chat)
    .bind(seq_before)
    .fetch_all(&bot.pool)
    .await
    .expect("outbox kinds");
    assert!(kinds.is_empty(), "{message}: {kinds:?}");
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
    bot.press_after("🌙 Тихие часы", "Выкл").await;

    let screen = bot.screen();
    assert!(screen.contains("Тихие часы выключены"), "{screen}");
    assert!(screen.contains("Накопилось 3 уведомления"), "{screen}");

    let messages = bot.fake.messages(bot.chat);
    let (batch_id, batch) = messages
        .iter()
        .find(|(_, html)| html.contains("Пока были тихие часы"))
        .expect("batch message");
    assert_eq!(block_count(batch, "⏰ напоминание"), 3, "{batch}");
    let (digest_id, _) = messages
        .iter()
        .find(|(_, html)| html.contains("Доброе утро"))
        .expect("digest message");
    let (screen_id, _) = bot.active();

    // Порядок в outbox, а не по id сообщений: экран должен попасть в очередь
    // раньше сводного, даже если он правит уже существующее сообщение.
    let seq = |kind: &'static str, message_id: i64| {
        let pool = db.pool.clone();
        let chat = bot.chat;
        async move {
            sqlx::query_scalar::<_, i64>(
                "SELECT max(seq) FROM outbox
                 WHERE chat_id = $1 AND kind = $2::outbox_kind AND message_id = $3",
            )
            .bind(chat)
            .bind(kind)
            .bind(message_id)
            .fetch_one(&pool)
            .await
            .expect("outbox seq")
        }
    };
    let screen_seq = seq("screen", screen_id).await;
    let batch_seq = seq("message", *batch_id).await;
    let digest_seq = seq("message", *digest_id).await;
    assert!(
        screen_seq < batch_seq,
        "screen before batch: {screen_seq} < {batch_seq}"
    );
    assert!(
        batch_seq < digest_seq,
        "batch before digest: {batch_seq} < {digest_seq}"
    );

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
    bot.press_after("🌙 Тихие часы", "Выкл").await;

    let screen = bot.screen();
    assert!(screen.contains("Накопилось 1 уведомление"), "{screen}");
    let (_, notice) = bot.latest();
    assert!(notice.contains("Одно напоминание"), "{notice}");
    assert!(!notice.contains("Пока были тихие часы"), "{notice}");
    let labels: Vec<String> = buttons(&notice)
        .into_iter()
        .map(|(label, _)| label)
        .collect();
    for label in ["Выполнено", "Отложить на час", "Открыть"] {
        assert!(
            labels.iter().any(|item| item.contains(label)),
            "{label}: {labels:?}"
        );
    }

    // Повторное включение и выключение не выпускает уведомление снова. Кнопки
    // нажаты на самом якоре, поэтому после 2.3 экран правится на месте
    // (`in_place`) и новых сообщений нет даже при `buried = true`.
    let sent_after_release = bot.fake.sent().len();
    bot.press_after("🌙 Тихие часы", "Вкл").await;
    bot.press_after("🌙 Тихие часы", "Выкл").await;
    assert_eq!(
        bot.fake.sent().len(),
        sent_after_release,
        "repeated toggling must not send anything, including another notification"
    );
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
    bot.press_after("🌙 Тихие часы", "Выкл").await;

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
    // Групповой snooze ждёт своего срока и не переносится вместе с тихими часами.
    let grouped = create_task(&db.pool, &household, "Групповой", due, None).await;
    let grouped_at = local_utc(moscow, day, 10, 0);
    sqlx::query(
        "INSERT INTO notification_deliveries (
             id, household_id, user_id, item_id, occurrence_key, kind,
             status, scheduled_at, deferred, batched, created_at, updated_at
         )
         VALUES ($1, $2, $3, $4, $5, 'snooze', 'pending', $6, false, true, $6, $6)",
    )
    .bind(Uuid::now_v7())
    .bind(household.household_id)
    .bind(household.owner_id)
    .bind(grouped.id)
    .bind(format!("snooze:{}", grouped_at.timestamp()))
    .bind(grouped_at)
    .execute(&db.pool)
    .await
    .expect("grouped snooze");

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
    assert_eq!(scheduled.len(), 2, "{scheduled:?}");
    assert!(
        scheduled
            .iter()
            .all(|at| *at == local_utc(moscow, day, 9, 0)),
        "{scheduled:?}"
    );
    let grouped_scheduled: DateTime<Utc> = sqlx::query_scalar(
        "SELECT scheduled_at FROM notification_deliveries
         WHERE item_id = $1 AND kind = 'snooze' AND status = 'pending'",
    )
    .bind(grouped.id)
    .fetch_one(&db.pool)
    .await
    .expect("grouped snooze time");
    assert_eq!(grouped_scheduled, grouped_at);

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
    bot.press("Профиль").await;
    bot.press("Часовой пояс").await;
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
    bot.press_after("🌙 Тихие часы", "Выкл").await;

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
async fn manual_release_waits_for_the_scheduler_recipient_lock() {
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
    let chat = bot.chat;
    let fake = bot.fake.clone();
    let sent_before = fake.sent().len();
    let mut press = Box::pin(bot.press_after("🌙 Тихие часы", "Выкл"));
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(300), &mut press)
            .await
            .is_err(),
        "manual release must wait for the recipient lock instead of skipping"
    );
    assert_eq!(fake.sent().len(), sent_before);
    assert!(
        fake.messages(chat)
            .iter()
            .all(|(_, html)| !html.contains("Накопилось"))
    );

    sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(key)
        .execute(&mut *holder)
        .await
        .expect("unlock");
    press.await;

    assert!(
        bot.screen().contains("Накопилось 2 уведомления"),
        "{}",
        bot.screen()
    );
    let messages = bot.fake.messages(bot.chat);
    assert_eq!(
        messages
            .iter()
            .filter(|(_, html)| html.contains("Пока были тихие часы"))
            .count(),
        1,
        "the manual release sends exactly one batch"
    );

    // Планировщик уже не находит отложенных записей: дубля нет.
    assert_eq!(
        tick_and_deliver(&mut bot, &db.pool, release_at)
            .await
            .delivered,
        0
    );
    let messages = bot.fake.messages(bot.chat);
    assert_eq!(
        messages
            .iter()
            .filter(|(_, html)| html.contains("Пока были тихие часы"))
            .count(),
        1,
        "the scheduler must not send the batch a second time"
    );
}

/// Snooze из блока сводного, срок которого ещё не наступил, не выпускается
/// раньше этого срока, даже если выключение тихих часов освобождает отложенные.
async fn snooze_not_released_before_its_own_time(move_quiet_start: bool) {
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
    let snooze = button_of(&html, "На час");
    bot.press_data(&snooze, message_id).await;
    let snoozed_at = local_utc(moscow, day, 22, 30);
    let delivery_id: Uuid = sqlx::query_scalar(
        "SELECT id FROM notification_deliveries
         WHERE kind = 'snooze' AND status = 'pending'",
    )
    .fetch_one(&db.pool)
    .await
    .expect("snooze delivery");

    // 21:40: срок snooze (22:30) попал в тихие часы — общее правило переносит
    // его на 08:00 и помечает отложенным тихими часами.
    let morning = local_utc(moscow, day + Duration::days(1), 8, 0);
    tick_and_deliver(&mut bot, &db.pool, at + Duration::minutes(10)).await;
    let (scheduled_at, deferred): (DateTime<Utc>, bool) =
        sqlx::query_as("SELECT scheduled_at, deferred FROM notification_deliveries WHERE id = $1")
            .bind(delivery_id)
            .fetch_one(&db.pool)
            .await
            .expect("deferred snooze");
    assert_eq!(scheduled_at, morning);
    assert!(deferred);

    // 21:50: тихие часы больше не действуют, но собственный срок snooze ещё
    // впереди — ручной выпуск не должен его ускорять.
    bot.advance(local_utc(moscow, day, 21, 50) - bot.now);
    open_notifications(&mut bot).await;
    let seq_before = outbox_seq_before(&bot).await;
    if move_quiet_start {
        bot.press("Начало тихих").await;
        bot.text("23:00").await;
    } else {
        bot.press_after("🌙 Тихие часы", "Выкл").await;
    }

    assert!(!bot.screen().contains("Накопилось"), "{}", bot.screen());
    assert_no_notification_sent(
        &bot,
        seq_before,
        "snooze must not be released before its own time",
    )
    .await;
    let (scheduled_at, deferred): (DateTime<Utc>, bool) =
        sqlx::query_as("SELECT scheduled_at, deferred FROM notification_deliveries WHERE id = $1")
            .bind(delivery_id)
            .fetch_one(&db.pool)
            .await
            .expect("restored snooze");
    assert_eq!(scheduled_at, snoozed_at);
    assert!(!deferred);

    // 22:30: срок наступил — snooze приходит обычным сообщением.
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
async fn disabling_quiet_hours_does_not_release_a_snooze_early() {
    snooze_not_released_before_its_own_time(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn moving_quiet_start_later_does_not_release_a_snooze_early() {
    snooze_not_released_before_its_own_time(true).await;
}

/// Сокращение тихих часов не выпускает snooze раньше его собственного срока:
/// запись возвращается к своему сроку и снимает признак отложенности.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shortening_quiet_hours_does_not_release_a_snooze_early() {
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
    let snooze = button_of(&html, "На час");
    bot.press_data(&snooze, message_id).await;
    let snoozed_at = local_utc(moscow, day, 22, 30);
    let delivery_id: Uuid = sqlx::query_scalar(
        "SELECT id FROM notification_deliveries
         WHERE kind = 'snooze' AND status = 'pending'",
    )
    .fetch_one(&db.pool)
    .await
    .expect("snooze delivery");

    // 21:40: срок snooze (22:30) попал в тихие часы — общее правило переносит
    // его на 08:00 и помечает отложенным тихими часами.
    let morning = local_utc(moscow, day + Duration::days(1), 8, 0);
    tick_and_deliver(&mut bot, &db.pool, at + Duration::minutes(10)).await;
    let (scheduled_at, deferred): (DateTime<Utc>, bool) =
        sqlx::query_as("SELECT scheduled_at, deferred FROM notification_deliveries WHERE id = $1")
            .bind(delivery_id)
            .fetch_one(&db.pool)
            .await
            .expect("deferred snooze");
    assert_eq!(scheduled_at, morning);
    assert!(deferred);

    // 21:50: тихие часы меняют на 21:00–22:00. Всё ещё тихо, ручного выпуска
    // нет, но snooze со сроком 22:30 не должен переехать на конец интервала.
    bot.advance(local_utc(moscow, day, 21, 50) - bot.now);
    open_notifications(&mut bot).await;
    let seq_before = outbox_seq_before(&bot).await;
    bot.press("Начало тихих").await;
    bot.text("21:00").await;
    bot.press("Конец тихих").await;
    bot.text("22:00").await;

    assert!(!bot.screen().contains("Накопилось"), "{}", bot.screen());
    assert_no_notification_sent(
        &bot,
        seq_before,
        "snooze must not be sent before its own time",
    )
    .await;
    let (scheduled_at, deferred): (DateTime<Utc>, bool) =
        sqlx::query_as("SELECT scheduled_at, deferred FROM notification_deliveries WHERE id = $1")
            .bind(delivery_id)
            .fetch_one(&db.pool)
            .await
            .expect("restored snooze");
    assert_eq!(scheduled_at, snoozed_at);
    assert!(!deferred);

    // 22:00: конец тихого интервала ничего не выпускает — snooze ещё ждёт.
    assert_eq!(
        tick_and_deliver(&mut bot, &db.pool, local_utc(moscow, day, 22, 0))
            .await
            .delivered,
        0
    );
    // 22:30: срок наступил — snooze приходит обычным сообщением.
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

/// Snooze, собственный срок которого попадает в новый тихий интервал, ждёт
/// конца интервала, а не уходит раньше своего срока.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snooze_inside_the_new_quiet_interval_waits_for_its_end() {
    let db = support::isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    let household = household_for(&db.pool, bot.chat).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = local_today(&bot, moscow);
    let at = local_utc(moscow, day, 21, 50);
    bot.advance(at - bot.now);
    quiet_settings(&db.pool, household.owner_id, false).await;

    let due = local_utc(moscow, day, 23, 0);
    let item = create_task(&db.pool, &household, "Отложенная", due, None).await;
    let snoozed_at = local_utc(moscow, day, 21, 55);
    let morning = local_utc(moscow, day + Duration::days(1), 8, 0);
    let delivery_id = insert_delivery(
        &db.pool,
        household.household_id,
        household.owner_id,
        item.id,
        "snooze",
        &format!("snooze:{}", snoozed_at.timestamp()),
        morning,
        true,
    )
    .await;

    // 21:50–22:00: тихие часы сокращаются, срок snooze (21:55) внутри нового
    // интервала — он ждёт конца, но не раньше своего срока.
    open_notifications(&mut bot).await;
    bot.press("Начало тихих").await;
    bot.text("21:00").await;
    bot.press("Конец тихих").await;
    bot.text("22:00").await;

    let (scheduled_at, deferred): (DateTime<Utc>, bool) =
        sqlx::query_as("SELECT scheduled_at, deferred FROM notification_deliveries WHERE id = $1")
            .bind(delivery_id)
            .fetch_one(&db.pool)
            .await
            .expect("postponed snooze");
    assert_eq!(scheduled_at, local_utc(moscow, day, 22, 0));
    assert!(deferred);

    // 22:00: конец интервала наступил — snooze приходит.
    assert_eq!(
        tick_and_deliver(&mut bot, &db.pool, local_utc(moscow, day, 21, 59))
            .await
            .delivered,
        0
    );
    assert_eq!(
        tick_and_deliver(&mut bot, &db.pool, local_utc(moscow, day, 22, 0))
            .await
            .delivered,
        1
    );
}

/// Ручной выпуск смешанного набора: два обычных отложенных уходят одним
/// сводным, а snooze с будущим сроком возвращается к своему сроку и приходит
/// тогда, когда должен.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manual_release_sends_deferred_as_one_batch_and_restores_a_future_snooze() {
    let db = support::isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    let household = household_for(&db.pool, bot.chat).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = local_today(&bot, moscow);
    let at = local_utc(moscow, day, 21, 50);
    bot.advance(at - bot.now);
    quiet_settings(&db.pool, household.owner_id, false).await;

    let due = local_utc(moscow, day, 23, 0);
    let morning = local_utc(moscow, day + Duration::days(1), 8, 0);
    let mut items = Vec::new();
    for title in ["Первая", "Вторая"] {
        let item = create_task(&db.pool, &household, title, due, None).await;
        deferred_reminder(&db.pool, &household, item.id, due, morning).await;
        items.push(item);
    }
    let snoozed_at = local_utc(moscow, day, 22, 30);
    let snooze_id = insert_delivery(
        &db.pool,
        household.household_id,
        household.owner_id,
        items[0].id,
        "snooze",
        &format!("snooze:{}", snoozed_at.timestamp()),
        morning,
        true,
    )
    .await;

    open_notifications(&mut bot).await;
    let sent_before = bot.fake.sent().len();
    bot.press_after("🌙 Тихие часы", "Выкл").await;

    let screen = bot.screen();
    assert!(screen.contains("Накопилось 2 уведомления"), "{screen}");
    let messages = bot.fake.messages(bot.chat);
    let batches: Vec<&(i64, String)> = messages
        .iter()
        .filter(|(_, html)| html.contains("Пока были тихие часы"))
        .collect();
    assert_eq!(batches.len(), 1, "manual release sends exactly one batch");
    let (_, batch) = batches[0];
    assert_eq!(block_count(batch, "⏰ напоминание"), 2, "{batch}");
    assert_eq!(block_count(batch, "😴 отложенное"), 0, "{batch}");
    assert!(
        bot.fake.sent().len() > sent_before,
        "the batch and the screen must be queued"
    );

    let (scheduled_at, deferred): (DateTime<Utc>, bool) =
        sqlx::query_as("SELECT scheduled_at, deferred FROM notification_deliveries WHERE id = $1")
            .bind(snooze_id)
            .fetch_one(&db.pool)
            .await
            .expect("restored snooze");
    assert_eq!(scheduled_at, snoozed_at);
    assert!(!deferred);

    // В 22:30 snooze приходит обычным сообщением, сводное не повторяется.
    assert_eq!(
        tick_and_deliver(&mut bot, &db.pool, snoozed_at)
            .await
            .delivered,
        1
    );
    let (_, next) = bot.latest();
    assert!(!next.contains("Пока были тихие часы"), "{next}");
    assert!(next.contains("Пора"), "{next}");
    let messages = bot.fake.messages(bot.chat);
    assert_eq!(
        messages
            .iter()
            .filter(|(_, html)| html.contains("Пока были тихие часы"))
            .count(),
        1,
        "the batch must not be sent again"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manual_release_sends_nothing_when_the_scheduler_already_delivered() {
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
    let chat = bot.chat;
    let fake = bot.fake.clone();
    let sent_before = fake.sent().len();
    let mut press = Box::pin(bot.press_after("🌙 Тихие часы", "Выкл"));
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(300), &mut press)
            .await
            .is_err(),
        "manual release must wait for the recipient lock"
    );

    // Пока ручной выпуск ждёт, планировщик успел доставить записи.
    sqlx::query(
        "UPDATE notification_deliveries
         SET status = 'sent', updated_at = now()
         WHERE user_id = $1 AND status = 'pending' AND deferred",
    )
    .bind(household.owner_id)
    .execute(&db.pool)
    .await
    .expect("scheduler delivery");

    sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(key)
        .execute(&mut *holder)
        .await
        .expect("unlock");
    press.await;

    assert!(!bot.screen().contains("Накопилось"), "{}", bot.screen());
    assert_eq!(fake.sent().len(), sent_before, "nothing should be sent");
    assert!(
        fake.messages(chat)
            .iter()
            .all(|(_, html)| !html.contains("Пока были тихие часы")),
        "manual release must not send a duplicate batch"
    );
}

/// Payload последней сводки чата в outbox.
async fn latest_digest_payload(bot: &Harness) -> serde_json::Value {
    sqlx::query_scalar(
        "SELECT payload FROM outbox
         WHERE chat_id = $1 AND kind = 'message' AND dedupe_key LIKE 'notification:digest:%'
         ORDER BY seq DESC LIMIT 1",
    )
    .bind(bot.chat)
    .fetch_one(&bot.pool)
    .await
    .expect("digest payload")
}

/// Строки удаления с ключом замены: статус, ключ и payload.
async fn replace_delete_rows(bot: &Harness) -> Vec<(String, String, serde_json::Value)> {
    sqlx::query_as(
        "SELECT status::text, dedupe_key, payload FROM outbox
         WHERE chat_id = $1 AND kind = 'delete' AND dedupe_key LIKE 'replace:%'
         ORDER BY seq",
    )
    .bind(bot.chat)
    .fetch_all(&bot.pool)
    .await
    .expect("replace delete rows")
}

/// Есть ли среди вызовов фейка удаление данного сообщения.
fn deleted(bot: &Harness, message_id: i64) -> bool {
    bot.fake.calls().iter().any(
        |call| matches!(call, Call::Delete { chat, id } if *chat == bot.chat && *id == message_id),
    )
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
async fn manually_released_digest_replaces_the_previous_one() {
    let db = support::isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    let household = household_for(&db.pool, bot.chat).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = local_today(&bot, moscow);
    let yesterday = day - Duration::days(1);
    quiet_settings(&db.pool, household.owner_id, true).await;

    // Вчерашняя сводка доставлена после конца тихих часов.
    let d1_at = local_utc(moscow, yesterday, 9, 0);
    bot.freeze_at(d1_at);
    let stats = scheduler(&db.pool)
        .tick(d1_at)
        .await
        .expect("yesterday tick");
    assert_eq!(stats.delivered, 1, "{stats:?}");
    bot.deliver().await;
    let (d1, html) = bot.latest();
    assert!(html.contains("Доброе утро"), "{html}");

    // Сегодняшняя сводка отложена тихими часами на 08:00.
    let release_at = local_utc(moscow, day, 8, 0);
    insert_digest_delivery(
        &db.pool,
        household.household_id,
        household.owner_id,
        &digest_occurrence_key(day),
        release_at,
        true,
    )
    .await;

    // Ручной выпуск в 06:00, внутри тихих часов: обновление обработано,
    // исходящие ещё не доставлены.
    bot.advance(local_utc(moscow, day, 6, 0) - bot.now);
    open_notifications(&mut bot).await;
    let screen = bot.screen();
    let start = screen.find("🌙 Тихие часы").expect("quiet hours block");
    let toggle = button_of(&screen[start..], "Выкл");
    let (screen_id, _) = bot.active();
    bot.press_data_pending(&toggle, screen_id).await;

    let payload = latest_digest_payload(&bot).await;
    assert_eq!(payload["replaces"], json!([d1]), "{payload}");
    // До доставки выпущенной сводки удаления прежней нет.
    assert!(replace_delete_rows(&bot).await.is_empty());

    // Проходы воркера: сначала экран, затем выпущенная сводка; удаление
    // ставится только после доставки сводки.
    let mut worker = OutboxWorker::new(db.pool.clone(), bot.fake.clone()).only_chat(bot.chat);
    let mut clock = Instant::now();
    clock += StdDuration::from_secs(2);
    assert_eq!(
        worker.run_once(bot.now, clock).await.expect("screen pass"),
        1
    );
    assert!(replace_delete_rows(&bot).await.is_empty());
    clock += StdDuration::from_secs(2);
    assert_eq!(
        worker.run_once(bot.now, clock).await.expect("digest pass"),
        1
    );
    let (d2, html) = bot.latest();
    assert!(html.contains("Доброе утро"), "{html}");
    assert_ne!(d2, d1);
    let deletes = replace_delete_rows(&bot).await;
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
    worker.run_once(bot.now, clock).await.expect("delete pass");
    assert!(deleted(&bot, d1));
    let alive = alive_digests(&bot);
    assert_eq!(alive.len(), 1, "{alive:?}");
    assert_ne!(alive[0].0, d1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn digest_deferred_by_quiet_hours_replaces_the_previous_one_when_released_by_tick() {
    let db = support::isolated_db().await;
    let mut bot = onboarded(&db.pool).await;
    let household = household_for(&db.pool, bot.chat).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = local_today(&bot, moscow);
    let yesterday = day - Duration::days(1);
    // Сводка в 07:00 попадает внутрь тихих часов 22:00–08:00.
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

    // Вчерашняя сводка доставлена после конца тихих часов.
    let d1_at = local_utc(moscow, yesterday, 9, 0);
    bot.freeze_at(d1_at);
    let stats = scheduler(&db.pool)
        .tick(d1_at)
        .await
        .expect("yesterday tick");
    assert_eq!(stats.delivered, 1, "{stats:?}");
    bot.deliver().await;
    let (d1, html) = bot.latest();
    assert!(html.contains("Доброе утро"), "{html}");

    // 07:30: время сводки (07:00) прошло, но ещё тихо — запись планируется
    // и переносится тихими часами на их конец.
    let morning = local_utc(moscow, day, 7, 30);
    bot.advance(morning - bot.now);
    let stats = scheduler(&db.pool)
        .tick(morning)
        .await
        .expect("morning tick");
    assert_eq!(stats.delivered, 0, "{stats:?}");
    let (scheduled_at, deferred): (DateTime<Utc>, bool) = sqlx::query_as(
        "SELECT scheduled_at, deferred FROM notification_deliveries
         WHERE user_id = $1 AND kind = 'digest' AND status = 'pending'",
    )
    .bind(household.owner_id)
    .fetch_one(&db.pool)
    .await
    .expect("deferred digest");
    assert_eq!(scheduled_at, local_utc(moscow, day, 8, 0));
    assert!(deferred);

    // 08:00: тихие часы закончились, тик выпускает сводку, и она убирает прежнюю.
    let release_at = local_utc(moscow, day, 8, 0);
    let stats = scheduler(&db.pool)
        .tick(release_at)
        .await
        .expect("release tick");
    assert_eq!(stats.delivered, 1, "{stats:?}");
    let payload = latest_digest_payload(&bot).await;
    assert_eq!(payload["replaces"], json!([d1]), "{payload}");
    assert!(replace_delete_rows(&bot).await.is_empty());

    bot.advance(release_at - bot.now);
    // Первый проход воркера доставляет сводку и ставит удаление прежней.
    let mut worker = OutboxWorker::new(db.pool.clone(), bot.fake.clone()).only_chat(bot.chat);
    let mut clock = Instant::now();
    clock += StdDuration::from_secs(2);
    assert_eq!(
        worker.run_once(bot.now, clock).await.expect("digest pass"),
        1
    );
    let deletes = replace_delete_rows(&bot).await;
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
    worker.run_once(bot.now, clock).await.expect("delete pass");
    assert!(deleted(&bot, d1));
    let alive = alive_digests(&bot);
    assert_eq!(alive.len(), 1, "{alive:?}");
    assert_ne!(alive[0].0, d1);
}
