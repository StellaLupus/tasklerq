//! Скрейп `/metrics`: метрики очередей, LLM, capture, HTTP и защита scope.

mod support;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration as StdDuration, Instant};

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use chrono::{DateTime, Duration, Utc};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::PgConnection;
use tasklerq::capture::breaker::CircuitBreaker;
use tasklerq::config::{CaptureConfig, SecretString};
use tasklerq::home_assistant::{AiTask, GenerateData, HaError};
use tasklerq::http::api::capture::CaptureState;
use tasklerq::http::router::{AppState, build_router};
use tasklerq::telegram::api::TelegramError;
use tasklerq::telegram::bot::Outcome;
use tasklerq::telegram::outbox::{OutboxWorker, Outgoing};
use tasklerq::telegram::queue;
use tasklerq::telegram::webhook::{Ingress, SECRET_HEADER};
use tasklerq::telegram::worker::{Pass, UpdateHandler, UpdateWorker};
use tokio::sync::Notify;
use tower::ServiceExt;

use support::telegram::FakeTelegram;
use support::{db, issue_token, next_telegram_id};

const SECRET: &str = "webhook-secret";

fn peer() -> SocketAddr {
    SocketAddr::from(([172, 18, 0, 1], 4040))
}

fn capture_config() -> CaptureConfig {
    CaptureConfig {
        enabled: true,
        voice_timeout_ms: 8_000,
        voice_fallback_raw: false,
        rate_limit_per_min: 10,
        max_text_chars: 2_000,
    }
}

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
        Ok(json!({"items": [{"target_kind": "shopping", "title": "Молоко"}]}))
    }
}

struct BadAi;

#[async_trait]
impl AiTask for BadAi {
    async fn check_entity(&self) -> Result<(), HaError> {
        Ok(())
    }

    async fn generate_data(
        &self,
        _request: &GenerateData<'_>,
        _timeout: StdDuration,
    ) -> Result<Value, HaError> {
        Ok(json!({}))
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

async fn send(
    app: &Router,
    method: Method,
    uri: &str,
    token: Option<&str>,
    body: Option<Value>,
    extra: Option<(&str, &str)>,
) -> (StatusCode, HeaderMap, String) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    if let Some((name, value)) = extra {
        builder = builder.header(name, value);
    }
    let body = match body {
        Some(body) => {
            builder = builder.header("content-type", "application/json");
            Body::from(serde_json::to_vec(&body).expect("json body"))
        }
        None => Body::empty(),
    };
    let response = app
        .clone()
        .oneshot(builder.body(body).expect("request"))
        .await
        .expect("response");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    (
        status,
        headers,
        String::from_utf8(bytes.to_vec()).expect("UTF-8"),
    )
}

fn metric_value(text: &str, series: &str) -> Option<f64> {
    text.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        if parts.next()? != series {
            return None;
        }
        parts.next()?.parse().ok()
    })
}

fn assert_metric_at_least(text: &str, series: &str, minimum: f64) {
    let value =
        metric_value(text, series).unwrap_or_else(|| panic!("metric {series} is missing:\n{text}"));
    assert!(
        value >= minimum,
        "metric {series} = {value}, expected at least {minimum}:\n{text}"
    );
}

fn text_update(update_id: i64, chat_id: i64, text: &str) -> Value {
    json!({
        "update_id": update_id,
        "message": {
            "message_id": 1,
            "date": 1_700_000_000,
            "chat": {"id": chat_id, "type": "private", "first_name": "Ann"},
            "from": {"id": chat_id, "is_bot": false, "first_name": "Ann"},
            "text": text
        }
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scrape_reports_queue_llm_capture_and_http_metrics() {
    let db = support::isolated_db().await;
    let household = support::create_household(&db.pool).await;
    let metrics_token = issue_token(&db.pool, household.household_id, &["metrics:read"]).await;
    let capture_token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let read_token = issue_token(&db.pool, household.household_id, &["ha:read"]).await;

    let ingress = Ingress::new(&SecretString::new(SECRET), Arc::new(Notify::new()));
    let state = AppState::new(db.pool.clone())
        .with_capture(CaptureState::new(
            capture_config(),
            Some(Arc::new(StubAi)),
            Arc::new(CircuitBreaker::default()),
            200,
        ))
        .with_telegram(ingress)
        .with_queue_lag_limit(60);
    let app = build_router(state).layer(MockConnectInfo(peer()));
    let now = Utc::now();

    // Обработанное обновление Telegram.
    let chat = next_telegram_id();
    let update = text_update(6_000_001, chat, "Купить лекарство");
    let mut conn = db.pool.acquire().await.expect("connection");
    queue::enqueue(&mut conn, 6_000_001, &update, now)
        .await
        .expect("enqueue update");
    drop(conn);
    let worker = UpdateWorker::new(
        db.pool.clone(),
        Arc::new(Noop),
        FakeTelegram::new(),
        10,
        Arc::new(Notify::new()),
    )
    .only_chat(chat);
    assert_eq!(worker.run_once(now).await.expect("worker"), Pass::Processed);

    // Успешный capture через LLM.
    let (status, _, _) = send(
        &app,
        Method::POST,
        "/api/v1/capture",
        Some(&capture_token),
        Some(json!({"text": "Молоко", "source": "api", "mode": "auto"})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Сбой валидации ответа LLM считается отдельной метрикой.
    let bad_app = build_router(
        AppState::new(db.pool.clone()).with_capture(CaptureState::new(
            capture_config(),
            Some(Arc::new(BadAi)),
            Arc::new(CircuitBreaker::default()),
            200,
        )),
    );
    let (status, _, _) = send(
        &bad_app,
        Method::POST,
        "/api/v1/capture",
        Some(&capture_token),
        Some(json!({"text": "Молоко", "source": "api", "mode": "auto"})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

    // HTTP-метрика использует шаблон маршрута, а не сырой путь.
    let item_id = uuid::Uuid::now_v7();
    let (status, _, _) = send(
        &app,
        Method::GET,
        &format!("/api/v1/items/{item_id}"),
        Some(&read_token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Отказ аутентификации webhook.
    let (status, _, _) = send(
        &app,
        Method::POST,
        "/webhooks/telegram",
        None,
        Some(json!({"update_id": 1})),
        Some((SECRET_HEADER, "wrong")),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Обычный HTTP-запрос.
    let (status, _, _) = send(&app, Method::GET, "/health/live", None, None, None).await;
    assert_eq!(status, StatusCode::OK);

    // Dead-letter outbox и отдельная pending-запись для gauge.
    let dead_chat = next_telegram_id();
    let mut conn = db.pool.acquire().await.expect("connection");
    let dead_payload = Outgoing::Message {
        html: "dead".into(),
        rich: false,
    };
    tasklerq::telegram::outbox::enqueue(&mut conn, dead_chat, &dead_payload, None, now)
        .await
        .expect("enqueue dead");
    drop(conn);
    let fake = FakeTelegram::new();
    fake.fail_next(TelegramError::Forbidden("blocked".into()));
    let mut outbox = OutboxWorker::new(db.pool.clone(), fake).only_chat(dead_chat);
    assert_eq!(
        outbox
            .run_once(now, Instant::now())
            .await
            .expect("outbox pass"),
        0
    );

    let pending_chat = next_telegram_id();
    let mut conn = db.pool.acquire().await.expect("connection");
    let pending = Outgoing::Message {
        html: "pending".into(),
        rich: false,
    };
    tasklerq::telegram::outbox::enqueue(&mut conn, pending_chat, &pending, None, now)
        .await
        .expect("enqueue pending");
    drop(conn);

    // Задержка очереди: готовое обновление ждёт 10 минут.
    let lagging_chat = next_telegram_id();
    let lagging = text_update(6_000_002, lagging_chat, "старое");
    let mut conn = db.pool.acquire().await.expect("connection");
    queue::enqueue(&mut conn, 6_000_002, &lagging, now - Duration::minutes(10))
        .await
        .expect("enqueue lagging");
    drop(conn);

    let (status, _, body) = send(&app, Method::GET, "/health/ready", None, None, None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");

    let (status, headers, text) = send(
        &app,
        Method::GET,
        "/metrics",
        Some(&metrics_token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert!(
        headers
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.contains("version=0.0.4")),
        "Prometheus exposition 0.0.4 is required"
    );

    assert_metric_at_least(&text, "telegram_updates_total{status=\"processed\"}", 1.0);
    assert_metric_at_least(&text, "telegram_queue_lag_seconds", 590.0);
    assert_metric_at_least(&text, "outbox_pending", 1.0);
    assert_metric_at_least(&text, "outbox_dead_total", 1.0);
    assert_metric_at_least(
        &text,
        "capture_total{source=\"api\",status=\"applied\"}",
        1.0,
    );
    assert_metric_at_least(&text, "llm_requests_total{status=\"success\"}", 1.0);
    assert_metric_at_least(&text, "llm_requests_total{status=\"failure\"}", 1.0);
    assert_metric_at_least(&text, "llm_validation_failures_total", 1.0);
    assert_metric_at_least(&text, "llm_latency_seconds_count", 2.0);
    assert_metric_at_least(
        &text,
        "http_requests_total{route=\"/webhooks/telegram\",status=\"401\"}",
        1.0,
    );
    assert_metric_at_least(
        &text,
        "http_requests_total{route=\"/health/live\",status=\"200\"}",
        1.0,
    );
    assert_metric_at_least(
        &text,
        "http_requests_total{route=\"/api/v1/items/{id}\",status=\"404\"}",
        1.0,
    );
    assert_metric_at_least(&text, "db_pool_in_use", 0.0);
    assert_metric_at_least(&text, "webhook_auth_failures_total", 1.0);
    assert!(
        !text.contains(&item_id.to_string()),
        "the raw path must not become a metric label:\n{text}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metrics_endpoint_requires_bearer_token_and_metrics_scope() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let app = support::app(&db.pool);
    let read_token = issue_token(&db.pool, household.household_id, &["ha:read"]).await;
    let metrics_token = issue_token(&db.pool, household.household_id, &["metrics:read"]).await;

    let (status, _, body) = send(&app, Method::GET, "/metrics", None, None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");

    let (status, _, body) =
        send(&app, Method::GET, "/metrics", Some(&read_token), None, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    let (status, _, body) = send(
        &app,
        Method::GET,
        &format!("/metrics?token={metrics_token}"),
        None,
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");

    let (status, _, body) = send(
        &app,
        Method::GET,
        "/metrics",
        Some(&metrics_token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body.contains("# TYPE http_requests_total counter"),
        "{body}"
    );
}
