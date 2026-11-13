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
use serde_json::{Map, Value, json};
use sqlx::PgConnection;
use tasklerq::capture::eval::TitleMatch;
use tasklerq::capture::prompt;
use tasklerq::capture::slot_check;
use tasklerq::capture::slot_status::LlmSlotHealth;
use tasklerq::config::{CaptureConfig, LlmConfig, SecretString, SlotConfig};
use tasklerq::http::api::capture::CaptureState;
use tasklerq::http::router::{AppState, build_router};
use tasklerq::llm::{
    Chain, ChatBackend, ChatCompletion, ChatMessage, ChatRequest, LlmError, ResponseFailure, Slot,
    SystemClock,
};
use tasklerq::security_audit::Audit;
use tasklerq::storage::repositories::llm_slot_checks::{self, NewCheck};
use tasklerq::telegram::api::TelegramError;
use tasklerq::telegram::bot::{Bot, Outcome};
use tasklerq::telegram::capture_worker::{CaptureWorker, Pass as CapturePass};
use tasklerq::telegram::outbox::{OutboxWorker, Outgoing};
use tasklerq::telegram::queue;
use tasklerq::telegram::webhook::{Ingress, SECRET_HEADER};
use tasklerq::telegram::worker::{Pass, UpdateHandler, UpdateWorker};
use tokio::sync::Notify;
use tower::ServiceExt;

use support::telegram::{FakeTelegram, Harness};
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

fn fallback_capture_config() -> CaptureConfig {
    CaptureConfig {
        voice_fallback_raw: true,
        ..capture_config()
    }
}

fn chain_always(answer: Result<Value, LlmError>) -> Arc<tasklerq::llm::Chain> {
    Arc::new(support::llm::chain(
        support::llm::BackendDouble::always(answer),
        None,
    ))
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

struct Failing;

#[async_trait]
impl UpdateHandler for Failing {
    async fn handle(
        &self,
        _conn: &mut PgConnection,
        _payload: &Value,
        _now: DateTime<Utc>,
    ) -> Result<Outcome, sqlx::Error> {
        Err(sqlx::Error::PoolTimedOut)
    }
}

/// Слот, который всегда отвечает таймаутом.
struct TimeoutBackend;

#[async_trait]
impl ChatBackend for TimeoutBackend {
    async fn complete(
        &self,
        _request: &ChatRequest<'_>,
        _timeout: StdDuration,
    ) -> Result<ChatCompletion, LlmError> {
        Err(LlmError::Timeout)
    }
}

/// Слот, который всегда отвечает корректным текстом.
struct AnswerBackend;

#[async_trait]
impl ChatBackend for AnswerBackend {
    async fn complete(
        &self,
        _request: &ChatRequest<'_>,
        _timeout: StdDuration,
    ) -> Result<ChatCompletion, LlmError> {
        Ok(ChatCompletion {
            content: r#"{"items":[]}"#.to_owned(),
            input_tokens: Some(7),
            output_tokens: Some(3),
            model: "stub/model".to_owned(),
        })
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
    let state = AppState::new(db.pool.clone(), Audit::new(db.pool.clone()))
        .with_capture(CaptureState::new(
            capture_config(),
            Some(chain_always(Ok(support::llm::v4::response(vec![
                support::llm::v4::shopping("Молоко"),
            ])))),
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
        Audit::new(db.pool.clone()),
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
        AppState::new(db.pool.clone(), Audit::new(db.pool.clone())).with_capture(
            CaptureState::new(capture_config(), Some(chain_always(Ok(json!({})))), 200),
        ),
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

    // Недоступная LLM с разрешённым fallback создаёт позицию из исходного текста.
    let fallback_app = build_router(
        AppState::new(db.pool.clone(), Audit::new(db.pool.clone())).with_capture(
            CaptureState::new(
                fallback_capture_config(),
                Some(chain_always(Err(LlmError::Transport))),
                200,
            ),
        ),
    );
    let (status, _, body) = send(
        &fallback_app,
        Method::POST,
        "/api/v1/capture",
        Some(&capture_token),
        Some(json!({"text": "Купить хлеб", "source": "api", "mode": "auto"})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Обновления, исчерпавшие попытки: failed и dead.
    let failed_chat = next_telegram_id();
    let failed_update = text_update(6_000_010, failed_chat, "сбой");
    let mut conn = db.pool.acquire().await.expect("connection");
    queue::enqueue(&mut conn, 6_000_010, &failed_update, now)
        .await
        .expect("enqueue failed");
    drop(conn);
    let failed_worker = UpdateWorker::new(
        db.pool.clone(),
        Arc::new(Failing),
        FakeTelegram::new(),
        5,
        Arc::new(Notify::new()),
        Audit::new(db.pool.clone()),
    )
    .only_chat(failed_chat);
    assert!(matches!(
        failed_worker.run_once(now).await.expect("failed pass"),
        Pass::Failed(_)
    ));

    let dead_chat = next_telegram_id();
    let dead_update = text_update(6_000_011, dead_chat, "исчерпано");
    let mut conn = db.pool.acquire().await.expect("connection");
    queue::enqueue(&mut conn, 6_000_011, &dead_update, now)
        .await
        .expect("enqueue dead update");
    drop(conn);
    let dead_worker = UpdateWorker::new(
        db.pool.clone(),
        Arc::new(Failing),
        FakeTelegram::new(),
        1,
        Arc::new(Notify::new()),
        Audit::new(db.pool.clone()),
    )
    .only_chat(dead_chat);
    assert!(matches!(
        dead_worker.run_once(now).await.expect("dead pass"),
        Pass::Failed(_)
    ));

    // Второе обновление сверх лимита откладывается до освобождения слота.
    let limited_chat = next_telegram_id();
    for update_id in [6_000_020, 6_000_021] {
        let update = text_update(update_id, limited_chat, "часто");
        let mut conn = db.pool.acquire().await.expect("connection");
        queue::enqueue(&mut conn, update_id, &update, now)
            .await
            .expect("enqueue limited");
    }
    let limited_worker = UpdateWorker::new(
        db.pool.clone(),
        Arc::new(Noop),
        FakeTelegram::new(),
        5,
        Arc::new(Notify::new()),
        Audit::new(db.pool.clone()),
    )
    .only_chat(limited_chat)
    .with_rate_limit(1);
    assert_eq!(
        limited_worker.run_once(now).await.expect("first limited"),
        Pass::Processed
    );
    assert_eq!(
        limited_worker.run_once(now).await.expect("second limited"),
        Pass::Deferred
    );

    // Переполнение очереди отложенных: обновление отбрасывается.
    let dropped_chat = next_telegram_id();
    for offset in 0..20 {
        let update_id = 6_000_030 + offset;
        let update = text_update(update_id, dropped_chat, "в очереди");
        let mut conn = db.pool.acquire().await.expect("connection");
        queue::enqueue(&mut conn, update_id, &update, now + Duration::seconds(60))
            .await
            .expect("enqueue deferred");
    }
    let dropping_worker = UpdateWorker::new(
        db.pool.clone(),
        Arc::new(Noop),
        FakeTelegram::new(),
        5,
        Arc::new(Notify::new()),
        Audit::new(db.pool.clone()),
    )
    .only_chat(dropped_chat)
    .with_rate_limit(1);
    for (update_id, expected) in [(6_000_050, Pass::Processed), (6_000_051, Pass::Dropped)] {
        let update = text_update(update_id, dropped_chat, "сверх очереди");
        let mut conn = db.pool.acquire().await.expect("connection");
        queue::enqueue(&mut conn, update_id, &update, now)
            .await
            .expect("enqueue ready");
        drop(conn);
        assert_eq!(
            dropping_worker.run_once(now).await.expect("dropping"),
            expected
        );
    }

    // Capture из Telegram: отдельный household с реальным ботом.
    let capture_chat = next_telegram_id();
    let handler = Bot::new("tasklerqbot")
        .with_llm_capture(true)
        .with_tenancy(tasklerq::config::TenancyMode::Multi);
    let mut telegram = Harness::with_handler(
        &db.pool,
        capture_chat,
        Arc::new(handler.clone()),
        FakeTelegram::new(),
    );
    telegram.text("/start").await;
    telegram.text("Семья").await;
    telegram.press("Москва").await;
    telegram.press("меню").await;
    let capture_worker = CaptureWorker::new(
        db.pool.clone(),
        chain_always(Ok(support::llm::v4::response(vec![
            support::llm::v4::shopping("Молоко"),
        ]))),
        handler,
        Arc::new(Notify::new()),
        Audit::new(db.pool.clone()),
    )
    .only_chat(capture_chat);
    telegram.text("Молоко").await;
    assert_eq!(
        capture_worker
            .run_once(telegram.now)
            .await
            .expect("telegram capture"),
        CapturePass::Drafted
    );

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
        delete_after: None,
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
        delete_after: None,
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
    assert_metric_at_least(&text, "telegram_updates_total{status=\"failed\"}", 1.0);
    assert_metric_at_least(&text, "telegram_updates_total{status=\"dead\"}", 1.0);
    assert_metric_at_least(&text, "telegram_updates_total{status=\"deferred\"}", 1.0);
    assert_metric_at_least(&text, "telegram_updates_total{status=\"dropped\"}", 1.0);
    assert_metric_at_least(&text, "telegram_queue_lag_seconds", 590.0);
    assert_metric_at_least(&text, "outbox_pending", 1.0);
    assert_metric_at_least(&text, "outbox_dead_total", 1.0);
    assert_metric_at_least(
        &text,
        "capture_total{source=\"api\",status=\"applied\"}",
        1.0,
    );
    assert_metric_at_least(&text, "capture_fallback_total{source=\"api\"}", 1.0);
    assert_metric_at_least(
        &text,
        "capture_total{source=\"telegram\",status=\"drafted\"}",
        1.0,
    );
    assert_metric_at_least(
        &text,
        "llm_requests_total{slot=\"primary\",status=\"success\"}",
        1.0,
    );
    assert_metric_at_least(
        &text,
        "llm_requests_total{slot=\"primary\",status=\"failure\"}",
        1.0,
    );
    assert_metric_at_least(&text, "llm_validation_failures_total", 1.0);
    assert_metric_at_least(&text, "llm_latency_seconds_count{slot=\"primary\"}", 2.0);
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

/// Значение серии из рендера `/metrics`; 0, если серии ещё нет.
fn series_value(text: &str, series: &str) -> u64 {
    text.lines()
        .find_map(|line| {
            let (name, value) = line.split_once(' ')?;
            if name != series {
                return None;
            }
            value.parse::<u64>().ok()
        })
        .unwrap_or(0)
}

#[tokio::test]
async fn chain_reports_llm_metrics_with_slot_labels() {
    // Уникальные имена слотов: соседние тесты в общем реестре пишут те же
    // серии, и разницу значений можно считать только для своих слотов.
    const PRIMARY: &str = "metrics-test-primary";
    const FALLBACK: &str = "metrics-test-fallback";
    let primary = Slot::new(PRIMARY, Arc::new(TimeoutBackend), StdDuration::from_secs(1));
    let fallback = Slot::new(FALLBACK, Arc::new(AnswerBackend), StdDuration::from_secs(1));
    let chain = Chain::new(primary, Some(fallback));
    let schema = json!({"type": "object"});
    let messages = vec![ChatMessage::user("купить молоко")];
    let request = ChatRequest {
        messages: &messages,
        schema_name: "capture_v4",
        schema: &schema,
    };

    let before = tasklerq::metrics::render();
    let outcome = chain
        .run(
            &request,
            StdDuration::from_secs(2),
            Utc::now(),
            &SystemClock::new(),
            |text| Ok::<_, ResponseFailure<()>>(text.to_owned()),
        )
        .await;
    assert!(outcome.result.is_ok());

    let after = tasklerq::metrics::render();
    for (slot, status) in [(PRIMARY, "failure"), (FALLBACK, "success")] {
        let series = format!("llm_requests_total{{slot=\"{slot}\",status=\"{status}\"}}");
        assert_eq!(
            series_value(&after, &series),
            series_value(&before, &series) + 1,
            "Chain must record the request:\n{after}"
        );
        let count = format!("llm_latency_seconds_count{{slot=\"{slot}\"}}");
        assert_eq!(
            series_value(&after, &count),
            series_value(&before, &count) + 1,
            "Chain must record the latency:\n{after}"
        );
    }
    assert!(
        after.contains("llm_latency_seconds_bucket{slot=\"metrics-test-fallback\",le=\""),
        "{after}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn llm_slot_verified_is_one_only_for_a_passed_slot() {
    let db = support::isolated_db().await;
    let primary = SlotConfig {
        url: "https://api.example.test/v1".parse().expect("url"),
        model: "vendor/primary".to_owned(),
        token: Some(SecretString::new("SECRET-slot-token")),
        timeout: StdDuration::from_secs(10),
        extra: Map::new(),
    };
    let chain = support::llm::chain(
        support::llm::BackendDouble::always(Ok(support::llm::v4::response(vec![]))),
        None,
    );
    let llm = LlmConfig {
        enabled: true,
        primary: Some(primary.clone()),
        fallback: None,
        connect_timeout: StdDuration::from_millis(1_000),
        daily_quota: 200,
    };
    let health = LlmSlotHealth::new(Arc::new(chain), &llm).expect("llm health");
    let now = Utc::now();

    // Нет строки проверки — слот не прошёл.
    health.refresh(&db.pool, now).await.expect("refresh");
    assert_eq!(
        metric_value(
            &tasklerq::metrics::render(),
            "llm_slot_verified{slot=\"primary\"}"
        ),
        Some(0.0),
        "an unchecked slot must be 0"
    );

    let fingerprint = slot_check::fingerprint(
        &primary,
        &prompt::contract_hash(),
        &slot_check::set_hash(slot_check::BUILTIN_SET),
        TitleMatch::Strict,
    );
    let parts = fingerprint.parts.to_json();
    let report = json!({"total": 22, "passed": 22});
    let mut conn = db.pool.acquire().await.expect("connection");
    llm_slot_checks::upsert(
        &mut conn,
        NewCheck {
            slot: "primary",
            fingerprint: &fingerprint.value,
            fingerprint_parts: &parts,
            passed: 22,
            total: 22,
            p95_ms: 1_000,
            report: &report,
        },
        now,
    )
    .await
    .expect("slot check should be stored");
    drop(conn);

    health.invalidate();
    health.refresh(&db.pool, now).await.expect("refresh");
    assert_eq!(
        metric_value(
            &tasklerq::metrics::render(),
            "llm_slot_verified{slot=\"primary\"}"
        ),
        Some(1.0),
        "a passed slot must be 1"
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
