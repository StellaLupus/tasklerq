//! Ручной выпуск отложенных тихими часами уведомлений из настроек.

mod support;

use chrono::{DateTime, Utc};
use support::notifications::*;
use support::telegram::buttons;
use tasklerq::jobs::digest::digest_occurrence_key;
use uuid::Uuid;

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
    let mut press = Box::pin(bot.press("Тихие часы: вкл"));
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
    let mut press = Box::pin(bot.press("Тихие часы: вкл"));
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
