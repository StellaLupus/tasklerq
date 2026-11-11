//! Журнал внешнего capture не должен содержать текст и заголовки элементов.
//!
//! Отдельный тестовый бинарник, как `llm_logs.rs`: `tracing` кеширует интерес к
//! callsite, и параллельные тесты общего бинарника могли бы отключить его до
//! появления подписчика.

mod support;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tasklerq::config::CaptureConfig;
use tasklerq::http::api::capture::CaptureState;
use tasklerq::http::router::{AppState, build_router};
use tasklerq::llm::{Chain, LlmError};
use tasklerq::security_audit::Audit;
use tower::ServiceExt;

use support::logs::LogBuffer;
use support::{db, issue_token};

fn chain(answer: Result<Value, LlmError>) -> Arc<Chain> {
    Arc::new(support::llm::chain(
        support::llm::BackendDouble::always(answer),
        None,
    ))
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
            let state = AppState::new(db.pool.clone(), Audit::new(db.pool.clone())).with_capture(
                CaptureState::new(
                    CaptureConfig {
                        enabled: true,
                        voice_timeout_ms: 8_000,
                        voice_fallback_raw: false,
                        rate_limit_per_min: 10,
                        max_text_chars: 2_000,
                    },
                    Some(chain(Ok(support::llm::v4::response(vec![
                        support::llm::v4::shopping("секретный заголовок"),
                    ])))),
                    200,
                ),
            );
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

    let captured = logs.contents();
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

#[test]
fn a_disabled_llm_logs_the_fallback_reason_without_the_text() {
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
            let state = AppState::new(db.pool.clone(), Audit::new(db.pool.clone())).with_capture(
                CaptureState::new(
                    CaptureConfig {
                        enabled: true,
                        voice_timeout_ms: 8_000,
                        voice_fallback_raw: true,
                        rate_limit_per_min: 10,
                        max_text_chars: 2_000,
                    },
                    None,
                    200,
                ),
            );
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

    let captured = logs.contents();
    assert!(
        captured.contains("\"metric\":\"capture_fallback_total\"")
            && captured.contains("\"reason\":\"llm_disabled\""),
        "the fallback metrics must name the disabled llm:\n{captured}"
    );
    assert!(
        !captured.contains("секретная") && !captured.contains("подарок"),
        "the text must not reach the logs:\n{captured}"
    );
}

/// Выполнить capture и вернуть его статус и журнал, собранный за вызов.
fn capture_logs(ai: Option<Arc<Chain>>, text: &str) -> (StatusCode, String) {
    capture_logs_for(
        ai,
        vec![(
            json!({
                "text": text,
                "source": "ha_voice",
                "mode": "auto"
            }),
            Vec::new(),
        )],
        10,
    )
}

/// Выполнить несколько capture подряд на одном стенде и вернуть статус
/// последнего запроса и весь собранный журнал.
fn capture_logs_for(
    ai: Option<Arc<Chain>>,
    requests: Vec<(Value, Vec<(String, String)>)>,
    rate_limit_per_min: u32,
) -> (StatusCode, String) {
    let logs = LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(logs.clone())
        .finish();

    let status = tracing::subscriber::with_default(subscriber, || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let db = db().await;
            let household = support::create_household(&db.pool).await;
            let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
            let state = AppState::new(db.pool.clone(), Audit::new(db.pool.clone())).with_capture(
                CaptureState::new(
                    CaptureConfig {
                        enabled: true,
                        voice_timeout_ms: 8_000,
                        voice_fallback_raw: false,
                        rate_limit_per_min,
                        max_text_chars: 2_000,
                    },
                    ai,
                    200,
                ),
            );
            let app = build_router(state);
            let mut status = StatusCode::OK;
            for (payload, extra_headers) in requests {
                let body = serde_json::to_vec(&payload).expect("json");
                let mut builder = Request::builder()
                    .method(Method::POST)
                    .uri("/api/v1/capture")
                    .header("authorization", format!("Bearer {token}"))
                    .header("content-type", "application/json");
                for (name, value) in extra_headers {
                    builder = builder.header(name, value);
                }
                let request = builder.body(Body::from(body)).expect("request");
                let response = app.clone().oneshot(request).await.expect("response");
                status = response.status();
                let _ = response.into_body().collect().await.expect("body");
            }
            status
        })
    });

    (status, logs.contents())
}

fn assert_capture_log(captured: &str, status: &str, text: &str) {
    assert!(
        captured.contains("capture finished"),
        "the capture must be logged:\n{captured}"
    );
    assert!(
        captured.contains("\"source\":\"ha_voice\"")
            && captured.contains(&format!("\"status\":\"{status}\""))
            && captured.contains("\"capture_id\""),
        "the log line must contain source, status and capture_id:\n{captured}"
    );
    assert!(
        !captured.contains(text),
        "the text must not reach the logs:\n{captured}"
    );
}

#[test]
fn an_llm_failure_logs_the_status_without_the_text() {
    let (status, captured) = capture_logs(
        Some(chain(Err(LlmError::Transport))),
        "секретная заметка про подарок",
    );
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_capture_log(&captured, "unavailable", "секретная заметка про подарок");
}

#[test]
fn a_validation_failure_logs_the_status_without_the_text() {
    let text = "секретная заметка про подарок ".repeat(100);
    let (status, captured) = capture_logs(None, &text);
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(text.chars().count() > 2_000);
    assert_capture_log(&captured, "invalid", "секретная заметка про подарок");
}

#[test]
fn an_invalid_idempotency_key_is_logged_without_the_text() {
    let (status, captured) = capture_logs_for(
        None,
        vec![(
            json!({
                "text": "секретная заметка про подарок",
                "source": "ha_voice",
                "mode": "auto"
            }),
            vec![("idempotency-key".to_owned(), "bad key".to_owned())],
        )],
        10,
    );
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_capture_log(&captured, "invalid", "секретная заметка про подарок");
}

#[test]
fn an_idempotency_mismatch_is_logged_as_a_conflict_without_the_text() {
    let key = "019-test-key".to_owned();
    let headers = vec![("idempotency-key".to_owned(), key)];
    let (status, captured) = capture_logs_for(
        Some(chain(Ok(support::llm::v4::response(vec![
            support::llm::v4::shopping("секретный заголовок"),
        ])))),
        vec![
            (
                json!({
                    "text": "секретная заметка про подарок",
                    "source": "ha_voice",
                    "mode": "auto"
                }),
                headers.clone(),
            ),
            (
                json!({
                    "text": "секретная заметка про отпуск",
                    "source": "ha_voice",
                    "mode": "auto"
                }),
                headers,
            ),
        ],
        10,
    );
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_capture_log(&captured, "conflict", "секретная заметка про отпуск");
}

#[test]
fn an_unknown_list_is_logged_as_invalid_without_the_text() {
    let (status, captured) = capture_logs_for(
        None,
        vec![(
            json!({
                "text": "секретная заметка про подарок",
                "source": "ha_voice",
                "mode": "auto",
                "list_id": "018f0000-0000-7000-8000-000000000000"
            }),
            Vec::new(),
        )],
        10,
    );
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_capture_log(&captured, "invalid", "секретная заметка про подарок");
}

#[test]
fn a_rate_limited_capture_is_logged_without_the_text() {
    let payload = || {
        json!({
            "text": "секретная заметка про подарок",
            "source": "ha_voice",
            "mode": "auto"
        })
    };
    let (status, captured) = capture_logs_for(
        Some(chain(Ok(support::llm::v4::response(vec![
            support::llm::v4::shopping("секретный заголовок"),
        ])))),
        vec![(payload(), Vec::new()), (payload(), Vec::new())],
        1,
    );
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_capture_log(&captured, "rate_limited", "секретная заметка про подарок");
}
