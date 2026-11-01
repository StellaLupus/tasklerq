//! Логи Telegram и HTTP: идентификаторы и результат есть, текста сообщения нет.
//!
//! Отдельный тестовый бинарник, как `llm_logs.rs`: `tracing` кеширует интерес к
//! callsite, и параллельные тесты общего бинарника могли бы отключить его до
//! появления подписчика.

mod support;

use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::Request;
use chrono::{DateTime, Utc};
use http_body_util::BodyExt;
use serde_json::Value;
use sqlx::PgConnection;
use tasklerq::http::router::{AppState, build_router};
use tasklerq::telegram::bot::Outcome;
use tasklerq::telegram::queue;
use tasklerq::telegram::worker::{Pass, UpdateHandler, UpdateWorker};
use tokio::sync::Notify;
use tower::ServiceExt;

use support::next_telegram_id;
use support::telegram::FakeTelegram;

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

struct Noop;

#[async_trait]
impl UpdateHandler for Noop {
    async fn handle(
        &self,
        _conn: &mut PgConnection,
        _payload: &Value,
        _now: DateTime<Utc>,
    ) -> Result<Outcome, sqlx::Error> {
        Ok(Outcome::default())
    }
}

#[test]
fn logs_keep_identifiers_and_results_but_not_the_message_text() {
    let logs = LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(logs.clone())
        .finish();
    let update_id: i64 = 6_000_999;

    tracing::subscriber::with_default(subscriber, || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let db = support::db().await;
            let chat = next_telegram_id();
            let update = serde_json::json!({
                "update_id": update_id,
                "message": {
                    "message_id": 1,
                    "date": 1_700_000_000,
                    "chat": {"id": chat, "type": "private", "first_name": "Ann"},
                    "from": {"id": chat, "is_bot": false, "first_name": "Ann"},
                    "text": "Купить лекарство"
                }
            });
            let mut conn = db.pool.acquire().await.expect("connection");
            queue::enqueue(&mut conn, update_id, &update, Utc::now())
                .await
                .expect("enqueue");
            drop(conn);

            let worker = UpdateWorker::new(
                db.pool.clone(),
                Arc::new(Noop),
                FakeTelegram::new(),
                10,
                Arc::new(Notify::new()),
            )
            .only_chat(chat);
            assert_eq!(
                worker.run_once(Utc::now()).await.expect("worker"),
                Pass::Processed
            );

            let app = build_router(AppState::new(db.pool.clone()));
            let response = app
                .oneshot(
                    Request::builder()
                        .uri("/health/live")
                        .header("x-request-id", "req-observability")
                        .body(Body::empty())
                        .expect("request"),
                )
                .await
                .expect("response");
            let _ = response.into_body().collect().await.expect("body");
        });
    });

    let captured = String::from_utf8(logs.0.lock().expect("lock").clone()).expect("utf8");
    assert!(
        captured.contains("telegram update finished"),
        "the update must be logged:\n{captured}"
    );
    assert!(
        captured.contains(&format!("\"update_id\":{update_id}")),
        "the log must contain update_id:\n{captured}"
    );
    assert!(
        captured.contains("\"status\":\"processed\"") && captured.contains("\"duration_ms\":"),
        "the log must contain the result and duration:\n{captured}"
    );
    assert!(
        captured.contains("request completed")
            && captured.contains("\"route\":\"/health/live\"")
            && captured.contains("\"request_id\":")
            && captured.contains("\"status\":200")
            && captured.contains("\"duration_ms\":"),
        "the HTTP log must contain route, request_id, status and duration:\n{captured}"
    );
    assert!(
        !captured.contains("Купить лекарство"),
        "the message text must not reach the logs:\n{captured}"
    );
}
