//! Журнал внешнего capture не должен содержать текст и заголовки элементов.
//!
//! Отдельный тестовый бинарник, как `llm_logs.rs`: `tracing` кеширует интерес к
//! callsite, и параллельные тесты общего бинарника могли бы отключить его до
//! появления подписчика.

mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration as StdDuration;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tasklerq::capture::breaker::CircuitBreaker;
use tasklerq::config::CaptureConfig;
use tasklerq::home_assistant::{AiTask, GenerateData, HaError};
use tasklerq::http::api::capture::CaptureState;
use tasklerq::http::router::{AppState, build_router};
use tower::ServiceExt;

use support::{db, issue_token};

struct StubAi;

#[async_trait]
impl AiTask for StubAi {
    async fn check_entity(&self) -> Result<(), HaError> {
        Ok(())
    }

    async fn generate_data(
        &self,
        _request: &GenerateData<'_>,
        _timeout: StdDuration,
    ) -> Result<Value, HaError> {
        Ok(json!({"items": [{"target_kind": "shopping", "title": "секретный заголовок"}]}))
    }
}

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

#[test]
fn capture_logs_have_metadata_but_not_the_text() {
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
            let db = db().await;
            let household = support::create_household(&db.pool).await;
            let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
            let state = AppState::new(db.pool.clone()).with_capture(CaptureState::new(
                CaptureConfig {
                    enabled: true,
                    voice_timeout_ms: 8_000,
                    voice_fallback_raw: false,
                    rate_limit_per_min: 10,
                    max_text_chars: 2_000,
                },
                Some(Arc::new(StubAi)),
                Arc::new(CircuitBreaker::default()),
                200,
            ));
            let app = build_router(state);
            let body = serde_json::to_vec(&json!({
                "text": "секретная заметка про подарок",
                "source": "ha_voice",
                "mode": "auto"
            }))
            .expect("json");
            let request = Request::builder()
                .method(Method::POST)
                .uri("/api/v1/capture")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(body))
                .expect("request");
            let response = app.oneshot(request).await.expect("response");
            assert_eq!(response.status(), StatusCode::OK);
            let _ = response.into_body().collect().await.expect("body");
        });
    });

    let captured = logs.0.lock().expect("lock").clone();
    let captured = String::from_utf8(captured).expect("utf8");
    assert!(
        captured.contains("capture finished"),
        "the capture must be logged:\n{captured}"
    );
    assert!(
        captured.contains("\"source\":\"ha_voice\"")
            && captured.contains("\"status\":\"applied\"")
            && captured.contains("\"capture_id\""),
        "the log line must contain source, status and capture_id:\n{captured}"
    );
    assert!(
        !captured.contains("секретная")
            && !captured.contains("подарок")
            && !captured.contains("секретный заголовок"),
        "the text, speech and item titles must not reach the logs:\n{captured}"
    );
}
