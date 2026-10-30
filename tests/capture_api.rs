//! Contract-тесты внешнего `POST /api/v1/capture`.

mod support;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration as StdDuration;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use chrono::{DateTime, Duration, Utc};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::PgPool;
use tasklerq::capture::breaker::CircuitBreaker;
use tasklerq::config::CaptureConfig;
use tasklerq::home_assistant::{AiTask, GenerateData, HaError};
use tasklerq::http::api::capture::CaptureState;
use tasklerq::http::router::{AppState, build_router};
use tower::ServiceExt;
use uuid::Uuid;

use support::{db, issue_token};

/// Подмена Home Assistant: отдаёт заготовленные ответы по очереди.
struct StubAi {
    answers: Mutex<VecDeque<Result<Value, HaError>>>,
    calls: AtomicUsize,
}

impl StubAi {
    fn new(answers: Vec<Result<Value, HaError>>) -> Arc<Self> {
        Arc::new(Self {
            answers: Mutex::new(answers.into()),
            calls: AtomicUsize::new(0),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

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
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.answers
            .lock()
            .expect("lock")
            .pop_front()
            .unwrap_or(Err(HaError::Unreachable))
    }
}

fn config() -> CaptureConfig {
    CaptureConfig {
        enabled: true,
        voice_timeout_ms: 8_000,
        voice_fallback_raw: false,
        rate_limit_per_min: 10,
        max_text_chars: 2_000,
    }
}

fn app(pool: &PgPool, ai: Arc<StubAi>, config: CaptureConfig) -> Router {
    app_with(
        pool,
        Some(ai),
        config,
        Arc::new(CircuitBreaker::default()),
        200,
    )
}

fn app_with(
    pool: &PgPool,
    ai: Option<Arc<StubAi>>,
    config: CaptureConfig,
    breaker: Arc<CircuitBreaker>,
    quota: u32,
) -> Router {
    let ai: Option<Arc<dyn AiTask>> = ai.map(|ai| ai as Arc<dyn AiTask>);
    build_router(
        AppState::new(pool.clone()).with_capture(CaptureState::new(config, ai, breaker, quota)),
    )
}

/// Один ответ LLM: валидная покупка.
fn shopping(title: &str) -> Value {
    json!({"items": [{"target_kind": "shopping", "title": title}]})
}

/// Ответ LLM: две валидные покупки.
fn two_purchases() -> Value {
    json!({"items": [
        {"target_kind": "shopping", "title": "молоко"},
        {"target_kind": "shopping", "title": "хлеб"}
    ]})
}

fn body(text: &str, source: &str, mode: &str) -> Value {
    json!({"text": text, "source": source, "mode": mode})
}

async fn call(
    app: &Router,
    method: Method,
    uri: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, HeaderMap, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
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
        .expect("router should respond");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body should be readable")
        .to_bytes();
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, headers, json)
}

async fn raw(app: &Router, token: &str, payload: &str) -> (StatusCode, Value) {
    raw_with_content_type(app, token, Some("application/json"), payload).await
}

async fn raw_with_content_type(
    app: &Router,
    token: &str,
    content_type: Option<&str>,
    payload: &str,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(Method::POST)
        .uri("/api/v1/capture")
        .header("authorization", format!("Bearer {token}"));
    if let Some(content_type) = content_type {
        builder = builder.header("content-type", content_type);
    }
    let response = app
        .clone()
        .oneshot(
            builder
                .body(Body::from(payload.to_owned()))
                .expect("request"),
        )
        .await
        .expect("router should respond");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body should be readable")
        .to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

async fn item_count(pool: &PgPool, household_id: Uuid) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM items WHERE household_id = $1 AND deleted_at IS NULL")
        .bind(household_id)
        .fetch_one(pool)
        .await
        .expect("count items")
}

async fn draft_count(pool: &PgPool, household_id: Uuid) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM capture_drafts WHERE household_id = $1")
        .bind(household_id)
        .fetch_one(pool)
        .await
        .expect("count drafts")
}

fn assert_error_envelope(json: &Value, code: &str) {
    assert_eq!(json["error"]["code"], code);
    assert!(json["error"]["message"].is_string());
    assert!(json["error"]["details"].is_object());
    assert!(json["error"]["request_id"].is_string());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_request_without_a_token_is_rejected_before_the_llm() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let ai = StubAi::new(vec![Ok(two_purchases())]);
    let app = app(&db.pool, ai.clone(), config());

    let (status, _, json) = call(
        &app,
        Method::POST,
        "/api/v1/capture",
        None,
        Some(body("купить молоко", "ha_voice", "auto")),
    )
    .await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_error_envelope(&json, "unauthorized");
    assert_eq!(ai.calls(), 0);
    assert_eq!(item_count(&db.pool, household.household_id).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_token_without_capture_scope_is_forbidden_and_creates_nothing() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["ha:read", "ha:write"]).await;
    let ai = StubAi::new(vec![Ok(two_purchases())]);
    let app = app(&db.pool, ai.clone(), config());

    let (status, _, json) = call(
        &app,
        Method::POST,
        "/api/v1/capture",
        Some(&token),
        Some(body("купить молоко", "ha_voice", "auto")),
    )
    .await;

    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_error_envelope(&json, "forbidden");
    assert_eq!(ai.calls(), 0);
    assert_eq!(item_count(&db.pool, household.household_id).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_capture_token_cannot_read_lists() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let ai = StubAi::new(vec![Ok(two_purchases())]);
    let app = app(&db.pool, ai.clone(), config());

    let (status, _, json) = call(&app, Method::GET, "/api/v1/lists", Some(&token), None).await;

    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_error_envelope(&json, "forbidden");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_token_in_a_query_parameter_is_not_accepted() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let ai = StubAi::new(vec![Ok(two_purchases())]);
    let app = app(&db.pool, ai.clone(), config());

    let (status, _, json) = call(
        &app,
        Method::POST,
        &format!("/api/v1/capture?token={token}"),
        None,
        Some(body("купить молоко", "ha_voice", "auto")),
    )
    .await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_error_envelope(&json, "unauthorized");
    assert_eq!(ai.calls(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_body_over_sixteen_kilobytes_is_rejected_without_parsing() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let ai = StubAi::new(vec![Ok(two_purchases())]);
    let app = app(&db.pool, ai.clone(), config());

    let (status, _, json) = call(
        &app,
        Method::POST,
        "/api/v1/capture",
        Some(&token),
        Some(body(&"a".repeat(17_000), "ha_voice", "auto")),
    )
    .await;

    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_error_envelope(&json, "payload_too_large");
    assert_eq!(ai.calls(), 0);
    assert_eq!(item_count(&db.pool, household.household_id).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn text_over_the_configured_limit_is_rejected_before_the_llm() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let ai = StubAi::new(vec![Ok(two_purchases())]);
    let app = app(&db.pool, ai.clone(), config());

    let (status, _, json) = call(
        &app,
        Method::POST,
        "/api/v1/capture",
        Some(&token),
        Some(body(&"a".repeat(2_500), "ha_voice", "auto")),
    )
    .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_error_envelope(&json, "validation_error");
    assert_eq!(ai.calls(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_request_fields_are_rejected_before_the_llm() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let foreign = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let ai = StubAi::new(vec![Ok(two_purchases())]);
    let app = app(&db.pool, ai.clone(), config());

    let cases = [
        body("   ", "ha_voice", "auto"),
        json!({"text": "купить молоко", "source": "telegram", "mode": "auto"}),
        json!({"text": "купить молоко", "source": "ha_voice", "mode": "later"}),
        json!({"text": "купить молоко", "source": "ha_voice", "mode": "auto", "hint": "books"}),
        json!({"text": "купить молоко", "source": "ha_voice", "mode": "auto",
               "list_id": foreign.tasks_list_id}),
        json!({"text": "купить молоко", "source": "ha_voice", "mode": "auto",
               "list_id": Uuid::now_v7()}),
        json!({"text": "купить молоко", "source": "ha_voice", "mode": "auto",
               "unknown_field": true}),
    ];

    for body in cases {
        let (status, _, json) = call(
            &app,
            Method::POST,
            "/api/v1/capture",
            Some(&token),
            Some(body.clone()),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "body must be rejected: {body}"
        );
        assert_error_envelope(&json, "validation_error");
    }

    let (status, json) = raw(&app, &token, "{not json").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_error_envelope(&json, "validation_error");
    assert_eq!(ai.calls(), 0);
    assert_eq!(item_count(&db.pool, household.household_id).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_or_wrong_content_type_is_rejected_before_the_llm() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let ai = StubAi::new(vec![Ok(two_purchases())]);
    let app = app(&db.pool, ai.clone(), config());
    let payload = serde_json::to_string(&body("купить молоко", "ha_voice", "auto")).expect("json");

    for content_type in [None, Some("text/plain")] {
        let (status, json) = raw_with_content_type(&app, &token, content_type, &payload).await;

        assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE, "{json}");
        assert_error_envelope(&json, "unsupported_media_type");
    }
    assert_eq!(ai.calls(), 0);
    assert_eq!(item_count(&db.pool, household.household_id).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_eleventh_capture_in_a_minute_is_rate_limited_per_token() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let first = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let second = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let mut answers: Vec<Result<Value, HaError>> =
        (0..11).map(|_| Ok(shopping("молоко"))).collect();
    answers.push(Ok(shopping("хлеб")));
    let ai = StubAi::new(answers);
    let app = app(&db.pool, ai.clone(), config());

    for _ in 0..10 {
        let (status, _, json) = call(
            &app,
            Method::POST,
            "/api/v1/capture",
            Some(&first),
            Some(body("молоко", "api", "auto")),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{json}");
    }
    assert_eq!(ai.calls(), 10);

    let (status, headers, json) = call(
        &app,
        Method::POST,
        "/api/v1/capture",
        Some(&first),
        Some(body("молоко", "api", "auto")),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_error_envelope(&json, "rate_limited");
    assert!(
        headers.get("retry-after").is_some(),
        "Retry-After is required"
    );
    assert_eq!(ai.calls(), 10, "the LLM must not be called when limited");

    // У второго токена свой счётчик.
    let (status, _, json) = call(
        &app,
        Method::POST,
        "/api/v1/capture",
        Some(&second),
        Some(body("хлеб", "api", "auto")),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(ai.calls(), 11);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_applies_two_purchases_and_returns_the_full_contract() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let ai = StubAi::new(vec![Ok(two_purchases())]);
    let app = app(&db.pool, ai.clone(), config());

    let (status, _, json) = call(
        &app,
        Method::POST,
        "/api/v1/capture",
        Some(&token),
        Some(json!({"text": "купить молоко и хлеб", "source": "ha_voice",
                    "mode": "auto", "hint": "shopping",
                    "idempotency_key": "019-test"})),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{json}");
    let capture_id = json["capture_id"].as_str().expect("capture_id");
    assert_eq!(json["status"], "applied");
    assert_eq!(json["fallback"], false);
    assert!(json["draft_id"].is_null());
    assert!(json["expires_at"].is_null());
    assert_eq!(json["clarifications"], json!([]));
    assert_eq!(json["items"].as_array().expect("items").len(), 2);

    for item in json["items"].as_array().expect("items") {
        for key in ["id", "title", "list_id", "kind", "due_date", "due_at"] {
            assert!(item.get(key).is_some(), "missing {key}: {item}");
        }
        assert!(item["id"].is_string());
        assert_eq!(item["kind"], "shopping");
        assert!(item["due_date"].is_null());
        assert!(item["due_at"].is_null());
    }
    assert_eq!(json["items"][0]["title"], "молоко");
    assert_eq!(json["items"][1]["title"], "хлеб");

    let speech = json["speech"].as_str().expect("speech");
    assert!(speech.chars().count() <= 200, "{speech}");
    assert!(!speech.contains('\n'));
    for markdown in ['*', '#', '`', '[', ']'] {
        assert!(!speech.contains(markdown), "speech must be plain: {speech}");
    }

    let linked: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM items
         WHERE household_id = $1 AND capture_id = $2 AND source = 'ha_voice'
           AND deleted_at IS NULL",
    )
    .bind(household.household_id)
    .bind(Uuid::parse_str(capture_id).expect("uuid"))
    .fetch_one(&db.pool)
    .await
    .expect("count linked items");
    assert_eq!(linked, 2);

    let leaked_into_outbox: i64 =
        sqlx::query_scalar("SELECT count(*) FROM outbox WHERE payload::text LIKE '%' || $1 || '%'")
            .bind(capture_id)
            .fetch_one(&db.pool)
            .await
            .expect("count outbox rows");
    assert_eq!(leaked_into_outbox, 0, "capture must not write to outbox");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_rejects_the_whole_capture_when_one_item_is_invalid() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let ai = StubAi::new(vec![Ok(json!({"items": [
        {"target_kind": "shopping", "title": "молоко"},
        {"target_kind": "shopping", "title": "хлеб", "due_date": "2000-01-01"}
    ]}))]);
    let app = app(&db.pool, ai.clone(), config());

    let (status, _, json) = call(
        &app,
        Method::POST,
        "/api/v1/capture",
        Some(&token),
        Some(body("купить молоко и хлеб", "ha_voice", "auto")),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["status"], "rejected");
    assert_eq!(json["fallback"], false);
    assert_eq!(json["items"], json!([]));
    assert!(json["capture_id"].is_string());
    assert!(json["draft_id"].is_null());
    assert!(json["expires_at"].is_null());
    let speech = json["speech"].as_str().expect("speech");
    assert!(speech.starts_with("Не смог добавить:"), "{speech}");
    assert!(speech.chars().count() <= 200);

    assert_eq!(item_count(&db.pool, household.household_id).await, 0);
    assert_eq!(draft_count(&db.pool, household.household_id).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn too_many_items_is_a_rejected_input_and_does_not_open_the_breaker() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let items: Vec<Value> = (0..21)
        .map(|n| json!({"target_kind": "tasks", "title": format!("задача {n}")}))
        .collect();
    let ai = StubAi::new(vec![Ok(json!({"items": items}))]);
    let breaker = Arc::new(CircuitBreaker::default());
    // Один сбой до порога: если ошибка ввода считалась бы сбоем LLM,
    // предохранитель разомкнулся бы этим вызовом.
    for _ in 0..4 {
        breaker.record_failure(Utc::now());
    }
    let app = app_with(&db.pool, Some(ai.clone()), config(), breaker.clone(), 200);

    let (status, _, json) = call(
        &app,
        Method::POST,
        "/api/v1/capture",
        Some(&token),
        Some(body("двадцать одна задача", "ha_voice", "auto")),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["status"], "rejected");
    assert_eq!(json["fallback"], false);
    assert_eq!(json["items"], json!([]));
    assert!(json["capture_id"].is_string());
    assert!(json["draft_id"].is_null());
    assert!(json["expires_at"].is_null());
    let speech = json["speech"].as_str().expect("speech");
    assert!(speech.chars().count() <= 200, "{speech}");
    assert!(speech.contains("Слишком много"), "{speech}");
    assert!(!speech.contains("too_many_items"), "{speech}");

    assert_eq!(item_count(&db.pool, household.household_id).await, 0);
    assert_eq!(draft_count(&db.pool, household.household_id).await, 0);
    assert!(
        breaker.allow(Utc::now()),
        "an input error must not open the breaker"
    );

    let (run_status, error_code): (String, Option<String>) =
        sqlx::query_as("SELECT status::text, error_code FROM llm_runs WHERE household_id = $1")
            .bind(household.household_id)
            .fetch_one(&db.pool)
            .await
            .expect("llm run row");
    assert_eq!(run_status, "failure");
    assert_eq!(error_code.as_deref(), Some("too_many_items"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn draft_saves_a_pending_capture_and_creates_no_items() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let ai = StubAi::new(vec![Ok(two_purchases())]);
    let app = app(&db.pool, ai.clone(), config());
    let started = Utc::now();

    let (status, _, json) = call(
        &app,
        Method::POST,
        "/api/v1/capture",
        Some(&token),
        Some(body("купить молоко и хлеб", "api", "draft")),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["status"], "draft");
    assert_eq!(json["fallback"], false);
    assert_eq!(json["items"].as_array().expect("items").len(), 2);
    for item in json["items"].as_array().expect("items") {
        assert!(item["id"].is_null(), "draft items have no id yet: {item}");
        assert_eq!(item["kind"], "shopping");
    }

    let capture_id = json["capture_id"].as_str().expect("capture_id");
    assert_eq!(json["draft_id"], capture_id);

    let expires_at = json["expires_at"]
        .as_str()
        .expect("expires_at")
        .parse::<DateTime<Utc>>()
        .expect("expires_at is RFC 3339");
    let lifetime = expires_at - started;
    assert!(
        lifetime >= Duration::minutes(14) && lifetime <= Duration::minutes(16),
        "draft lifetime is {lifetime}"
    );

    assert_eq!(item_count(&db.pool, household.household_id).await, 0);
    let (status, source, mode): (String, String, String) = sqlx::query_as(
        "SELECT status::text, source::text, mode::text FROM capture_drafts
         WHERE id = $1 AND household_id = $2",
    )
    .bind(Uuid::parse_str(capture_id).expect("uuid"))
    .bind(household.household_id)
    .fetch_one(&db.pool)
    .await
    .expect("draft row");
    assert_eq!(status, "pending");
    assert_eq!(source, "api");
    assert_eq!(mode, "draft");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_llm_failure_returns_service_unavailable_without_leaking_text() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let ai = StubAi::new(vec![Err(HaError::BadResponse)]);
    let app = app(&db.pool, ai.clone(), config());

    let (status, _, json) = call(
        &app,
        Method::POST,
        "/api/v1/capture",
        Some(&token),
        Some(body("секретная заметка про подарок", "ha_voice", "auto")),
    )
    .await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_error_envelope(&json, "service_unavailable");
    let rendered = json.to_string();
    assert!(!rendered.contains("секретная"), "{rendered}");
    assert!(!rendered.contains("подарок"), "{rendered}");
    assert_eq!(ai.calls(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_disabled_capture_route_looks_missing() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let ai = StubAi::new(vec![Ok(two_purchases())]);
    let mut disabled = config();
    disabled.enabled = false;
    let app = app(&db.pool, ai.clone(), disabled);

    let (status, _, json) = call(
        &app,
        Method::POST,
        "/api/v1/capture",
        Some(&token),
        Some(body("купить молоко", "ha_voice", "auto")),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_error_envelope(&json, "not_found");
    assert_eq!(ai.calls(), 0);
    assert_eq!(item_count(&db.pool, household.household_id).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn draft_keeps_invalid_items_for_later_correction() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let ai = StubAi::new(vec![Ok(json!({"items": [
        {"target_kind": "shopping", "title": "молоко"},
        {"target_kind": "shopping", "title": "хлеб", "due_date": "2000-01-01"}
    ]}))]);
    let app = app(&db.pool, ai.clone(), config());

    let (status, _, json) = call(
        &app,
        Method::POST,
        "/api/v1/capture",
        Some(&token),
        Some(body("купить молоко и хлеб", "api", "draft")),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["status"], "draft");
    assert_eq!(json["items"].as_array().expect("items").len(), 2);
    assert_eq!(item_count(&db.pool, household.household_id).await, 0);
    assert_eq!(draft_count(&db.pool, household.household_id).await, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_long_applied_speech_is_still_cut_to_two_hundred_characters() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let long = "очень длинное название покупки ".repeat(5);
    let ai = StubAi::new(vec![Ok(json!({"items": [
        {"target_kind": "shopping", "title": format!("{long}1")},
        {"target_kind": "shopping", "title": format!("{long}2")},
        {"target_kind": "shopping", "title": format!("{long}3")}
    ]}))]);
    let app = app(&db.pool, ai.clone(), config());

    let (status, _, json) = call(
        &app,
        Method::POST,
        "/api/v1/capture",
        Some(&token),
        Some(body("купить три вещи", "ha_voice", "auto")),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["status"], "applied");
    let speech = json["speech"].as_str().expect("speech");
    assert!(speech.chars().count() <= 200, "{}", speech.chars().count());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_id_targets_the_exact_list() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let second: Uuid = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO lists (id, household_id, kind, name, is_default, position, created_at, updated_at)
         VALUES ($1, $2, 'shopping', 'Аптека', false, 10, now(), now())",
    )
    .bind(second)
    .bind(household.household_id)
    .execute(&db.pool)
    .await
    .expect("second list");
    let ai = StubAi::new(vec![Ok(shopping("молоко"))]);
    let app = app(&db.pool, ai.clone(), config());

    let (status, _, json) = call(
        &app,
        Method::POST,
        "/api/v1/capture",
        Some(&token),
        Some(json!({"text": "молоко", "source": "api", "mode": "auto",
                    "list_id": second})),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["status"], "applied");
    assert_eq!(json["items"][0]["list_id"], second.to_string());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hint_uses_the_real_default_list_of_the_kind() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    // «Аптека» стоит первой, но списком по умолчанию остаются «Покупки».
    sqlx::query("UPDATE lists SET is_default = true WHERE id = $1")
        .bind(household.shopping_list_id)
        .execute(&db.pool)
        .await
        .expect("default shopping list");
    sqlx::query(
        "INSERT INTO lists (id, household_id, kind, name, is_default, position, created_at, updated_at)
         VALUES ($1, $2, 'shopping', 'Аптека', false, -1, now(), now())",
    )
    .bind(Uuid::now_v7())
    .bind(household.household_id)
    .execute(&db.pool)
    .await
    .expect("pharmacy list");

    let ai = StubAi::new(vec![Ok(shopping("молоко")), Ok(shopping("хлеб"))]);
    let app = app(&db.pool, ai.clone(), config());

    let (status, _, json) = call(
        &app,
        Method::POST,
        "/api/v1/capture",
        Some(&token),
        Some(json!({"text": "купить молоко", "source": "ha_voice",
                    "mode": "auto", "hint": "shopping"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(
        json["items"][0]["list_id"],
        household.shopping_list_id.to_string()
    );
    let capture_id =
        Uuid::parse_str(json["capture_id"].as_str().expect("capture_id")).expect("uuid");
    let stored_list: Uuid = sqlx::query_scalar("SELECT list_id FROM items WHERE capture_id = $1")
        .bind(capture_id)
        .fetch_one(&db.pool)
        .await
        .expect("item list");
    assert_eq!(stored_list, household.shopping_list_id);

    let (status, _, json) = call(
        &app,
        Method::POST,
        "/api/v1/capture",
        Some(&token),
        Some(json!({"text": "купить хлеб", "source": "api",
                    "mode": "draft", "hint": "shopping"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["status"], "draft");
    assert_eq!(
        json["items"][0]["list_id"],
        household.shopping_list_id.to_string()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hint_sets_the_type_of_every_item() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    // Модель ошиблась с типом: подсказка вызывающей стороны сильнее.
    let ai = StubAi::new(vec![Ok(json!({"items": [
        {"target_kind": "tasks", "title": "позвонить маме"}
    ]}))]);
    let app = app(&db.pool, ai.clone(), config());

    let (status, _, json) = call(
        &app,
        Method::POST,
        "/api/v1/capture",
        Some(&token),
        Some(json!({"text": "позвонить маме", "source": "ha_voice",
                    "mode": "auto", "hint": "shopping"})),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["status"], "applied");
    assert_eq!(json["items"][0]["kind"], "shopping");
    assert_eq!(
        json["items"][0]["list_id"],
        household.shopping_list_id.to_string()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn capture_scope_is_checked_before_the_body_is_parsed() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["ha:read"]).await;
    let ai = StubAi::new(vec![Ok(two_purchases())]);
    let app = app(&db.pool, ai.clone(), config());

    let (status, json) = raw(&app, &token, "{not json").await;

    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_error_envelope(&json, "forbidden");
    assert_eq!(ai.calls(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quota_exhaustion_and_an_open_breaker_return_503_without_calling_the_llm() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;

    let quota_ai = StubAi::new(vec![Ok(two_purchases())]);
    let app = app_with(
        &db.pool,
        Some(quota_ai.clone()),
        config(),
        Arc::new(CircuitBreaker::default()),
        0,
    );
    let (status, _, json) = call(
        &app,
        Method::POST,
        "/api/v1/capture",
        Some(&token),
        Some(body("купить молоко", "ha_voice", "auto")),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_error_envelope(&json, "service_unavailable");
    assert_eq!(quota_ai.calls(), 0);

    let breaker = Arc::new(CircuitBreaker::default());
    for _ in 0..5 {
        breaker.record_failure(Utc::now());
    }
    let breaker_ai = StubAi::new(vec![Ok(two_purchases())]);
    let app = app_with(&db.pool, Some(breaker_ai.clone()), config(), breaker, 200);
    let (status, _, json) = call(
        &app,
        Method::POST,
        "/api/v1/capture",
        Some(&token),
        Some(body("купить молоко", "ha_voice", "auto")),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_error_envelope(&json, "service_unavailable");
    assert_eq!(breaker_ai.calls(), 0);
    assert_eq!(item_count(&db.pool, household.household_id).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_llm_client_returns_503() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let app = app_with(
        &db.pool,
        None,
        config(),
        Arc::new(CircuitBreaker::default()),
        200,
    );

    let (status, _, json) = call(
        &app,
        Method::POST,
        "/api/v1/capture",
        Some(&token),
        Some(body("купить молоко", "ha_voice", "auto")),
    )
    .await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_error_envelope(&json, "service_unavailable");
    assert_eq!(item_count(&db.pool, household.household_id).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_capture_token_cannot_read_items() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let ai = StubAi::new(vec![Ok(two_purchases())]);
    let app = app(&db.pool, ai.clone(), config());

    let (status, _, json) = call(
        &app,
        Method::GET,
        &format!("/api/v1/lists/{}/items", household.tasks_list_id),
        Some(&token),
        None,
    )
    .await;

    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_error_envelope(&json, "forbidden");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_shortcut_capture_creates_items_with_the_api_source() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let ai = StubAi::new(vec![Ok(shopping("молоко"))]);
    let app = app(&db.pool, ai.clone(), config());

    let (status, _, json) = call(
        &app,
        Method::POST,
        "/api/v1/capture",
        Some(&token),
        Some(body("молоко", "shortcut", "auto")),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["status"], "applied");
    let capture_id =
        Uuid::parse_str(json["capture_id"].as_str().expect("capture_id")).expect("uuid");
    let source: String = sqlx::query_scalar(
        "SELECT source::text FROM items WHERE household_id = $1 AND capture_id = $2",
    )
    .bind(household.household_id)
    .bind(capture_id)
    .fetch_one(&db.pool)
    .await
    .expect("item source");
    assert_eq!(source, "api");
}
