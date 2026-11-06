//! Сводные сообщения после тихих часов: выпуск по тику, пагинация и retention.

mod support;

use chrono::{DateTime, Duration, Utc};
use serde_json::json;
use support::notifications::*;
use support::telegram::buttons;
use tasklerq::config::RetentionConfig;
use tasklerq::domain::models::{Actor, UserRole};
use tasklerq::jobs::retention;
use tasklerq::storage::repositories::{items, notifications};
use tasklerq::telegram::bot::notify::BATCH_PAGE_SIZE;
use uuid::Uuid;

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
    assert!(labels.iter().any(|label| label.contains("Выполнить все")));
    for label in [
        "Выполнено",
        "На час",
        "Открыть",
        "Выполнить все",
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
    for label in ["Выполнено", "Отложить на час", "Завтра в 09:00", "Открыть"]
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retention_purges_old_batches_without_references() {
    let db = support::isolated_db().await;
    let household = support::create_household(&db.pool).await;
    let now = Utc::now();
    let old = now - Duration::days(60);
    let mut conn = db.pool.acquire().await.expect("conn");
    let old_sent = create_empty_batch(&db.pool, &household, now).await;
    let old_pending = create_empty_batch(&db.pool, &household, now).await;
    let recent_sent = create_empty_batch(&db.pool, &household, now).await;
    let old_batch_with_recent_delivery = create_empty_batch(&db.pool, &household, now).await;
    let recent_batch_with_old_delivery = create_empty_batch(&db.pool, &household, now).await;
    for batch in [old_sent, old_pending, old_batch_with_recent_delivery] {
        sqlx::query("UPDATE notification_batches SET updated_at = $2 WHERE id = $1")
            .bind(batch)
            .bind(old)
            .execute(&mut *conn)
            .await
            .expect("old batch time");
    }
    for (id, status, batch_id) in [
        ("old-sent", "sent", old_sent),
        ("old-pending", "pending", old_pending),
        ("recent-sent", "sent", recent_sent),
        (
            "old-batch-recent-delivery",
            "sent",
            old_batch_with_recent_delivery,
        ),
        (
            "recent-batch-old-delivery",
            "sent",
            recent_batch_with_old_delivery,
        ),
    ] {
        let at = if id == "recent-sent" || id == "old-batch-recent-delivery" {
            now
        } else {
            old
        };
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
    assert_eq!(left.len(), 4);
    assert!(!left.contains(&old_sent));
    assert!(left.contains(&old_pending));
    assert!(left.contains(&recent_sent));
    assert!(left.contains(&old_batch_with_recent_delivery));
    assert!(left.contains(&recent_batch_with_old_delivery));

    // Живой батч не теряет блоки: его доставка старше срока, но пока батч
    // обновлялся листанием, страница должна оставаться непустой.
    let kept: i64 =
        sqlx::query_scalar("SELECT count(*) FROM notification_deliveries WHERE batch_id = $1")
            .bind(recent_batch_with_old_delivery)
            .fetch_one(&db.pool)
            .await
            .expect("kept delivery");
    assert_eq!(kept, 1);
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
