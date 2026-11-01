//! Метрика dead-letter outbox: код ошибки без текста сообщения и токенов.
//!
//! Отдельный тестовый бинарник, как `llm_logs.rs`: `tracing` кеширует интерес к
//! callsite, и параллельные тесты общего бинарника могли бы отключить его до
//! появления подписчика.

mod support;

use std::sync::{Arc, Mutex};
use std::time::Instant;

use chrono::{DateTime, Duration, Timelike, Utc};
use serde_json::json;
use support::telegram::FakeTelegram;
use tasklerq::jobs::scheduler::Scheduler;
use tasklerq::telegram::api::TelegramError;
use tasklerq::telegram::outbox::OutboxWorker;
use tokio::sync::Notify;

/// Писатель журнала в память.
#[derive(Clone, Default)]
struct LogBuffer(Arc<Mutex<Vec<u8>>>);

struct LogWriter(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LogWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("lock").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuffer {
    type Writer = LogWriter;

    fn make_writer(&'a self) -> Self::Writer {
        LogWriter(self.0.clone())
    }
}

fn rounded_now() -> DateTime<Utc> {
    Utc::now()
        .with_second(0)
        .and_then(|at| at.with_nanosecond(0))
        .expect("rounded time")
}

#[test]
fn dead_notification_logs_the_metric_and_code_without_its_text() {
    let logs = LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(logs.clone())
        .finish();

    tracing::subscriber::with_default(subscriber, || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let db = support::db().await;
            let household = support::create_household(&db.pool).await;
            sqlx::query(
                "UPDATE users
                 SET settings = jsonb_build_object(
                     'notifications',
                     jsonb_build_object(
                         'digest_enabled', false,
                         'quiet_hours_enabled', false
                     )
                 )
                 WHERE id = $1",
            )
            .bind(household.owner_id)
            .execute(&db.pool)
            .await
            .expect("settings");

            let due = rounded_now() + Duration::hours(3);
            let now = due - Duration::minutes(15);
            let mut item = support::new_item(household.tasks_list_id, "СЕКРЕТНОЕ_НАЗВАНИЕ");
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

            Scheduler::new(db.pool.clone(), Arc::new(Notify::new()))
                .tick(now)
                .await
                .expect("scheduler tick");

            let chat: i64 = sqlx::query_scalar("SELECT telegram_chat_id FROM users WHERE id = $1")
                .bind(household.owner_id)
                .fetch_one(&db.pool)
                .await
                .expect("chat id");
            let fake = FakeTelegram::new();
            fake.fail_next(TelegramError::Forbidden("blocked".into()));
            let mut worker = OutboxWorker::new(db.pool.clone(), fake).only_chat(chat);
            assert_eq!(
                worker
                    .run_once(now, Instant::now())
                    .await
                    .expect("outbox pass"),
                0
            );
        });
    });

    let captured = logs.0.lock().expect("lock").clone();
    let captured = String::from_utf8(captured).expect("utf8");
    assert!(
        captured.contains("\"metric\":\"outbox_dead\"")
            && captured.contains("\"error\":\"forbidden\""),
        "the dead-letter metric must carry the error code:\n{captured}"
    );
    assert!(
        !captured.contains("СЕКРЕТНОЕ_НАЗВАНИЕ") && !captured.contains("blocked"),
        "the message text must not reach the logs:\n{captured}"
    );
}

#[test]
fn invalid_payload_is_dead_letter_with_time_and_metric() {
    let logs = LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(logs.clone())
        .finish();

    let now = rounded_now();
    let (status, last_error, dead_at, seq) = tracing::subscriber::with_default(subscriber, || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async move {
            let db = support::db().await;
            let chat = support::next_telegram_id();
            let seq: i64 = sqlx::query_scalar(
                "INSERT INTO outbox (chat_id, kind, payload, next_attempt_at, created_at)
                 VALUES ($1, 'message', $2, $3, $3)
                 RETURNING seq",
            )
            .bind(chat)
            .bind(json!({"html": 42}))
            .bind(now)
            .fetch_one(&db.pool)
            .await
            .expect("outbox row");

            let fake = FakeTelegram::new();
            let mut worker = OutboxWorker::new(db.pool.clone(), fake).only_chat(chat);
            assert_eq!(
                worker
                    .run_once(now, Instant::now())
                    .await
                    .expect("outbox pass"),
                0
            );

            let row: (String, Option<String>, Option<DateTime<Utc>>) = sqlx::query_as(
                "SELECT status::text, last_error, dead_at FROM outbox WHERE seq = $1",
            )
            .bind(seq)
            .fetch_one(&db.pool)
            .await
            .expect("outbox row");
            (row.0, row.1, row.2, seq)
        })
    });

    assert_eq!(status, "dead");
    assert_eq!(last_error.as_deref(), Some("invalid_payload"));
    assert_eq!(dead_at, Some(now), "dead-letter time must be stored");

    let captured = logs.0.lock().expect("lock").clone();
    let captured = String::from_utf8(captured).expect("utf8");
    assert!(
        captured.contains("\"metric\":\"outbox_dead\"")
            && captured.contains("\"error\":\"invalid_payload\"")
            && captured.contains(&format!("\"seq\":{seq}")),
        "invalid payload must log the dead-letter metric and code:\n{captured}"
    );
}
