//! Лог ошибки шага тика планировщика: сбой сводки не отменяет доставку
//! напоминания и попадает в журнал с названием шага.
//!
//! Отдельный тестовый бинарник, как `outbox_dead_logs.rs`: `tracing` кеширует
//! интерес к callsite, и параллельные тесты общего бинарника могли бы отключить
//! его до появления подписчика.

mod support;

use chrono::Duration;
use serde_json::json;
use support::logs::LogBuffer;

#[test]
fn a_failed_digest_step_is_logged_with_its_name_and_still_delivers_reminders() {
    let buffer = LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(buffer.clone())
        .finish();

    let result = tracing::subscriber::with_default(subscriber, || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let db = support::isolated_db().await;
            let household = support::create_household(&db.pool).await;
            // Сводка должна планироваться в этом тике, поэтому её время уже
            // наступило. Тихие часы выключены: иначе сводка переносится на утро.
            for user_id in [household.owner_id, household.member_id] {
                sqlx::query(
                    "UPDATE users
                     SET settings = jsonb_set(settings, '{notifications}', $2::jsonb, true)
                     WHERE id = $1",
                )
                .bind(user_id)
                .bind(json!({
                    "digest_enabled": true,
                    "digest_time": "00:00",
                    "quiet_hours_enabled": false
                }))
                .execute(&db.pool)
                .await
                .expect("settings");
            }

            let due = chrono::Utc::now() + Duration::hours(3);
            let mut item = support::new_item(household.tasks_list_id, "Позвонить в поликлинику");
            item.due_at = Some(due);
            item.assignee_user_id = Some(household.owner_id);
            support::create_item(
                &db.pool,
                &household,
                household.tasks_list_id,
                &household.owner(),
                item,
            )
            .await;

            let result = support::notifications::tick_with_a_failing_digest(
                &db.pool,
                due - Duration::minutes(15),
            )
            .await;

            let delivered: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM notification_deliveries
                 WHERE item_id IS NOT NULL AND kind = 'reminder' AND status = 'sent'",
            )
            .fetch_one(&db.pool)
            .await
            .expect("delivered reminders");
            (result.is_err(), delivered)
        })
    });

    let logs = buffer.contents();
    assert!(
        result.0,
        "a failed digest step must be reported by the tick"
    );
    assert_eq!(result.1, 1, "the reminder must still be delivered:\n{logs}");
    assert!(logs.contains("\"level\":\"ERROR\""), "{logs}");
    assert!(
        logs.contains("\"step\":\"plan digest\""),
        "the error must carry the step name:\n{logs}"
    );
    assert!(
        logs.contains("\"error\":"),
        "the step error must be logged:\n{logs}"
    );
}

#[test]
fn a_failed_event_advance_step_is_logged_with_its_name_and_still_delivers_reminders() {
    let buffer = LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(buffer.clone())
        .finish();

    let result = tracing::subscriber::with_default(subscriber, || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let db = support::isolated_db().await;
            let household = support::create_household(&db.pool).await;
            for user_id in [household.owner_id, household.member_id] {
                sqlx::query(
                    "UPDATE users
                     SET settings = jsonb_set(settings, '{notifications}', $2::jsonb, true)
                     WHERE id = $1",
                )
                .bind(user_id)
                .bind(json!({
                    "digest_enabled": false,
                    "quiet_hours_enabled": false
                }))
                .execute(&db.pool)
                .await
                .expect("settings");
            }

            let due = chrono::Utc::now() + Duration::hours(3);
            let mut task = support::new_item(household.tasks_list_id, "Позвонить");
            task.due_at = Some(due);
            task.assignee_user_id = Some(household.owner_id);
            support::create_item(
                &db.pool,
                &household,
                household.tasks_list_id,
                &household.owner(),
                task,
            )
            .await;

            let mut event = support::new_item(household.events_list_id, "Тренировка");
            event.due_at = Some(due - Duration::hours(4));
            event.end_at = Some(due - Duration::hours(3));
            event.recurrence = Some(support::recurrence(
                tasklerq::domain::models::RecurrenceFreq::Weekly,
                vec![2],
            ));
            support::create_item(
                &db.pool,
                &household,
                household.events_list_id,
                &household.owner(),
                event,
            )
            .await;

            sqlx::query(
                "CREATE FUNCTION fail_advance() RETURNS trigger AS $$
                 BEGIN
                     RAISE EXCEPTION 'forced advance failure';
                 END $$ LANGUAGE plpgsql",
            )
            .execute(&db.pool)
            .await
            .expect("function");
            sqlx::query(
                "CREATE TRIGGER fail_advance BEFORE UPDATE ON items
                 FOR EACH ROW WHEN (OLD.recurrence_freq IS NOT NULL)
                 EXECUTE FUNCTION fail_advance()",
            )
            .execute(&db.pool)
            .await
            .expect("trigger");

            let result = support::notifications::scheduler(&db.pool)
                .tick(due - Duration::minutes(15))
                .await;

            sqlx::query("DROP TRIGGER fail_advance ON items")
                .execute(&db.pool)
                .await
                .expect("drop trigger");
            sqlx::query("DROP FUNCTION fail_advance()")
                .execute(&db.pool)
                .await
                .expect("drop function");

            let delivered: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM notification_deliveries
                 WHERE item_id IS NOT NULL AND kind = 'reminder' AND status = 'sent'",
            )
            .fetch_one(&db.pool)
            .await
            .expect("delivered reminders");
            (result.is_err(), delivered)
        })
    });

    let logs = buffer.contents();
    assert!(
        result.0,
        "a failed event advance step must be reported by the tick"
    );
    assert_eq!(
        result.1, 1,
        "the reminder must still be delivered after the failed step:\n{logs}"
    );
    assert!(logs.contains("\"level\":\"ERROR\""), "{logs}");
    assert!(
        logs.contains("\"step\":\"advance events\""),
        "the error must carry the step name:\n{logs}"
    );
    assert!(
        logs.contains("\"error\":"),
        "the step error must be logged:\n{logs}"
    );
}
