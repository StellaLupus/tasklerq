//! Contract-тесты `Idempotency-Key` внешнего `POST /api/v1/capture`.

mod support;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration as StdDuration;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::PgPool;
use tasklerq::capture::breaker::CircuitBreaker;
use tasklerq::config::CaptureConfig;
use tasklerq::home_assistant::{AiTask, GenerateData, HaError};
use tasklerq::http::api::capture::CaptureState;
use tasklerq::http::router::{AppState, build_router};
use tokio::sync::Notify;
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

/// Заглушка, которая держит первый вызов LLM до сигнала: нужна для проверки
/// повтора, пока запрос ещё обрабатывается.
struct GatedAi {
    answer: Value,
    started: Notify,
    release: Notify,
    calls: AtomicUsize,
}

impl GatedAi {
    fn new(answer: Value) -> Arc<Self> {
        Arc::new(Self {
            answer,
            started: Notify::new(),
            release: Notify::new(),
            calls: AtomicUsize::new(0),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl AiTask for GatedAi {
    async fn check_entity(&self) -> Result<(), HaError> {
        Ok(())
    }

    async fn generate_data(
        &self,
        _request: &GenerateData<'_>,
        _timeout: StdDuration,
    ) -> Result<Value, HaError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.started.notify_one();
        self.release.notified().await;
        Ok(self.answer.clone())
    }
}

/// Заглушка, которая держит только первый вызов LLM: тест успевает перехватить
/// резерв, а второй (новый владелец) завершается сразу.
struct TakeoverAi {
    answer: Value,
    started: Notify,
    release: Notify,
    calls: AtomicUsize,
}

impl TakeoverAi {
    fn new(answer: Value) -> Arc<Self> {
        Arc::new(Self {
            answer,
            started: Notify::new(),
            release: Notify::new(),
            calls: AtomicUsize::new(0),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl AiTask for TakeoverAi {
    async fn check_entity(&self) -> Result<(), HaError> {
        Ok(())
    }

    async fn generate_data(
        &self,
        _request: &GenerateData<'_>,
        _timeout: StdDuration,
    ) -> Result<Value, HaError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if call == 1 {
            self.started.notify_one();
            self.release.notified().await;
        }
        Ok(self.answer.clone())
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

fn app(pool: &PgPool, ai: Arc<dyn AiTask>) -> Router {
    build_router(AppState::new(pool.clone()).with_capture(CaptureState::new(
        config(),
        Some(ai),
        Arc::new(CircuitBreaker::default()),
        200,
    )))
}

fn app_with(
    pool: &PgPool,
    ai: Option<Arc<dyn AiTask>>,
    config: CaptureConfig,
    breaker: Arc<CircuitBreaker>,
    quota: u32,
) -> Router {
    build_router(
        AppState::new(pool.clone()).with_capture(CaptureState::new(config, ai, breaker, quota)),
    )
}

/// Дождаться, пока фоновое освобождение удалит резерв.
async fn wait_for_release(pool: &PgPool, household_id: Uuid) {
    tokio::time::timeout(StdDuration::from_secs(10), async {
        loop {
            let count: i64 =
                sqlx::query_scalar("SELECT count(*) FROM capture_requests WHERE household_id = $1")
                    .bind(household_id)
                    .fetch_one(pool)
                    .await
                    .expect("count requests");
            if count == 0 {
                break;
            }
            tokio::time::sleep(StdDuration::from_millis(10)).await;
        }
    })
    .await
    .expect("the reservation should be released");
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

fn keyed(body: Value, key: &str) -> Value {
    let mut body = body;
    body["idempotency_key"] = json!(key);
    body
}

async fn call_inner(
    app: &Router,
    token: &str,
    key_header: Option<&str>,
    body: Value,
) -> (StatusCode, HeaderMap, Value) {
    let mut builder = Request::builder()
        .method(Method::POST)
        .uri("/api/v1/capture")
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json");
    if let Some(key) = key_header {
        builder = builder.header("idempotency-key", key);
    }
    let response = app
        .clone()
        .oneshot(
            builder
                .body(Body::from(serde_json::to_vec(&body).expect("json body")))
                .expect("request"),
        )
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
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, headers, json)
}

async fn call_with_key(
    app: &Router,
    token: &str,
    key: &str,
    body: Value,
) -> (StatusCode, HeaderMap, Value) {
    call_inner(app, token, Some(key), body).await
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

async fn run_count(pool: &PgPool, household_id: Uuid) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM llm_runs WHERE household_id = $1")
        .bind(household_id)
        .fetch_one(pool)
        .await
        .expect("count llm runs")
}

async fn request_count(pool: &PgPool, household_id: Uuid) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM capture_requests WHERE household_id = $1")
        .bind(household_id)
        .fetch_one(pool)
        .await
        .expect("count capture requests")
}

fn assert_validation(json: &Value) {
    assert_eq!(json["error"]["code"], "validation_error", "{json}");
    assert!(json["error"]["message"].is_string(), "{json}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_auto_repeat_returns_the_first_result_without_duplicates() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let ai = StubAi::new(vec![Ok(two_purchases())]);
    let app = app(&db.pool, ai.clone());
    let key = Uuid::now_v7().to_string();
    let payload = keyed(body("купить молоко и хлеб", "ha_voice", "auto"), &key);

    let (status, _, first) = call_with_key(&app, &token, &key, payload.clone()).await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(first["status"], "applied");

    let (status, _, second) = call_with_key(&app, &token, &key, payload).await;
    assert_eq!(status, StatusCode::OK, "{second}");
    assert_eq!(second, first, "a repeat must return the first response");
    assert_eq!(second["items"], first["items"]);

    assert_eq!(item_count(&db.pool, household.household_id).await, 2);
    assert_eq!(ai.calls(), 1, "the LLM must be called once");
    assert_eq!(run_count(&db.pool, household.household_id).await, 1);
    assert_eq!(request_count(&db.pool, household.household_id).await, 1);

    let capture_id =
        Uuid::parse_str(first["capture_id"].as_str().expect("capture_id")).expect("capture uuid");
    let (stored_key, stored_hash): (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT idempotency_key, request_hash FROM capture_drafts
         WHERE id = $1 AND household_id = $2",
    )
    .bind(capture_id)
    .bind(household.household_id)
    .fetch_one(&db.pool)
    .await
    .expect("capture draft row");
    assert_eq!(stored_key.as_deref(), Some(key.as_str()));
    assert!(stored_hash.is_some(), "the request hash must be stored");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_draft_repeat_returns_the_first_draft_and_keeps_one_row() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let ai = StubAi::new(vec![Ok(two_purchases())]);
    let app = app(&db.pool, ai.clone());
    let key = Uuid::now_v7().to_string();
    let payload = keyed(body("купить молоко и хлеб", "api", "draft"), &key);

    let (status, _, first) = call_with_key(&app, &token, &key, payload.clone()).await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(first["status"], "draft");

    let (status, _, second) = call_with_key(&app, &token, &key, payload).await;
    assert_eq!(status, StatusCode::OK, "{second}");
    assert_eq!(second, first, "a repeat must return the first response");

    assert_eq!(draft_count(&db.pool, household.household_id).await, 1);
    assert_eq!(item_count(&db.pool, household.household_id).await, 0);
    assert_eq!(ai.calls(), 1);
    assert_eq!(run_count(&db.pool, household.household_id).await, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rejected_repeat_returns_the_first_rejection() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let ai = StubAi::new(vec![Ok(json!({"items": [
        {"target_kind": "shopping", "title": "хлеб", "due_date": "2000-01-01"}
    ]}))]);
    let app = app(&db.pool, ai.clone());
    let key = Uuid::now_v7().to_string();
    let payload = keyed(body("купить хлеб", "ha_voice", "auto"), &key);

    let (status, _, first) = call_with_key(&app, &token, &key, payload.clone()).await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(first["status"], "rejected");

    let (status, _, second) = call_with_key(&app, &token, &key, payload).await;
    assert_eq!(status, StatusCode::OK, "{second}");
    assert_eq!(second, first);
    assert_eq!(second["status"], "rejected");

    assert_eq!(item_count(&db.pool, household.household_id).await, 0);
    assert_eq!(draft_count(&db.pool, household.household_id).await, 0);
    assert_eq!(ai.calls(), 1);
    assert_eq!(run_count(&db.pool, household.household_id).await, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_same_text_with_different_keys_creates_two_captures() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let ai = StubAi::new(vec![Ok(shopping("молоко")), Ok(shopping("молоко"))]);
    let app = app(&db.pool, ai.clone());
    let payload = body("купить молоко", "ha_voice", "auto");

    let (status, _, first) =
        call_with_key(&app, &token, &Uuid::now_v7().to_string(), payload.clone()).await;
    assert_eq!(status, StatusCode::OK, "{first}");
    let (status, _, second) =
        call_with_key(&app, &token, &Uuid::now_v7().to_string(), payload).await;
    assert_eq!(status, StatusCode::OK, "{second}");

    assert_ne!(first["capture_id"], second["capture_id"]);
    assert_eq!(item_count(&db.pool, household.household_id).await, 2);
    assert_eq!(ai.calls(), 2);
    assert_eq!(request_count(&db.pool, household.household_id).await, 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_same_key_with_another_text_or_mode_is_rejected() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let ai = StubAi::new(vec![Ok(shopping("молоко"))]);
    let app = app(&db.pool, ai.clone());
    let key = Uuid::now_v7().to_string();

    let (status, _, first) = call_with_key(
        &app,
        &token,
        &key,
        keyed(body("купить молоко", "ha_voice", "auto"), &key),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{first}");

    let (status, _, other_text) = call_with_key(
        &app,
        &token,
        &key,
        keyed(body("купить хлеб", "ha_voice", "auto"), &key),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_validation(&other_text);

    let (status, _, other_mode) = call_with_key(
        &app,
        &token,
        &key,
        keyed(body("купить молоко", "ha_voice", "draft"), &key),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_validation(&other_mode);

    assert_eq!(item_count(&db.pool, household.household_id).await, 1);
    let open_drafts: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM capture_drafts WHERE household_id = $1 AND mode = 'draft'",
    )
    .bind(household.household_id)
    .fetch_one(&db.pool)
    .await
    .expect("count draft-mode rows");
    assert_eq!(open_drafts, 0);
    assert_eq!(ai.calls(), 1, "a mismatched repeat must not call the LLM");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_header_and_a_field_key_that_differ_are_rejected() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let ai = StubAi::new(vec![Ok(shopping("молоко")), Ok(shopping("молоко"))]);
    let app = app(&db.pool, ai.clone());

    let (status, _, json) = call_with_key(
        &app,
        &token,
        "header-key",
        keyed(body("купить молоко", "ha_voice", "auto"), "field-key"),
    )
    .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_validation(&json);
    assert_eq!(ai.calls(), 0);
    assert_eq!(request_count(&db.pool, household.household_id).await, 0);

    // Одинаковый ключ в заголовке и поле допустим.
    let key = Uuid::now_v7().to_string();
    let (status, _, json) = call_with_key(
        &app,
        &token,
        &key,
        keyed(body("купить молоко", "ha_voice", "auto"), &key),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["status"], "applied");

    // Ключ можно передать только заголовком.
    let (status, _, json) = call_with_key(
        &app,
        &token,
        &Uuid::now_v7().to_string(),
        body("купить молоко", "ha_voice", "auto"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["status"], "applied");
    assert_eq!(ai.calls(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_invalid_key_is_rejected_before_the_llm() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let ai = StubAi::new(vec![Ok(shopping("молоко"))]);
    let app = app(&db.pool, ai.clone());
    let payload = body("купить молоко", "ha_voice", "auto");

    let long = "a".repeat(129);
    for key in [long.as_str(), "bad key", "ключ", "tab\tkey"] {
        let (status, _, json) = call_with_key(&app, &token, key, payload.clone()).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "key={key:?}");
        assert_validation(&json);
    }

    let long_header = "b".repeat(129);
    let (status, _, json) = call_with_key(
        &app,
        &token,
        &long_header,
        keyed(payload.clone(), &long_header),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_validation(&json);

    assert_eq!(ai.calls(), 0);
    assert_eq!(request_count(&db.pool, household.household_id).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_same_key_with_another_source_creates_two_captures() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let ai = StubAi::new(vec![Ok(shopping("молоко")), Ok(shopping("молоко"))]);
    let app = app(&db.pool, ai.clone());
    let key = Uuid::now_v7().to_string();

    let (status, _, first) = call_with_key(
        &app,
        &token,
        &key,
        keyed(body("купить молоко", "ha_voice", "auto"), &key),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{first}");

    let (status, _, second) = call_with_key(
        &app,
        &token,
        &key,
        keyed(body("купить молоко", "api", "auto"), &key),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{second}");

    assert_ne!(first["capture_id"], second["capture_id"]);
    assert_eq!(item_count(&db.pool, household.household_id).await, 2);
    assert_eq!(ai.calls(), 2);
    assert_eq!(request_count(&db.pool, household.household_id).await, 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_concurrent_repeat_gets_409_and_runs_the_llm_once() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let ai = GatedAi::new(shopping("молоко"));
    let app = app(&db.pool, ai.clone());
    let key = Uuid::now_v7().to_string();
    let payload = keyed(body("купить молоко", "ha_voice", "auto"), &key);

    let first_app = app.clone();
    let first_token = token.clone();
    let first_key = key.clone();
    let first_payload = payload.clone();
    let first = tokio::spawn(async move {
        call_with_key(&first_app, &first_token, &first_key, first_payload).await
    });

    tokio::time::timeout(StdDuration::from_secs(10), ai.started.notified())
        .await
        .expect("the llm call should start");

    let (status, headers, json) = call_with_key(&app, &token, &key, payload.clone()).await;
    assert_eq!(status, StatusCode::CONFLICT, "{json}");
    assert_eq!(json["error"]["code"], "conflict");
    assert!(
        headers.get("retry-after").is_some(),
        "Retry-After is required"
    );
    assert_eq!(ai.calls(), 1, "a concurrent repeat must not call the LLM");

    ai.release.notify_one();
    let (status, _, first_json) = tokio::time::timeout(StdDuration::from_secs(10), first)
        .await
        .expect("the first request should finish")
        .expect("the first request should not panic");
    assert_eq!(status, StatusCode::OK, "{first_json}");
    assert_eq!(first_json["status"], "applied");

    let (status, _, repeat) = call_with_key(&app, &token, &key, payload).await;
    assert_eq!(status, StatusCode::OK, "{repeat}");
    assert_eq!(
        repeat, first_json,
        "after completion the answer is replayed"
    );
    assert_eq!(ai.calls(), 1);
    assert_eq!(item_count(&db.pool, household.household_id).await, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_first_request_releases_the_key_for_a_retry() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let ai = StubAi::new(vec![Err(HaError::BadResponse), Ok(shopping("молоко"))]);
    let app = app(&db.pool, ai.clone());
    let key = Uuid::now_v7().to_string();
    let payload = keyed(body("купить молоко", "ha_voice", "auto"), &key);

    let (status, _, json) = call_with_key(&app, &token, &key, payload.clone()).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{json}");
    assert_eq!(request_count(&db.pool, household.household_id).await, 0);

    let (status, _, json) = call_with_key(&app, &token, &key, payload).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["status"], "applied");
    assert_eq!(ai.calls(), 2, "the retry must call the LLM again");
    assert_eq!(item_count(&db.pool, household.household_id).await, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stale_reservation_is_taken_over_and_processed_again() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let ai = StubAi::new(vec![Ok(shopping("молоко"))]);
    let app = app(&db.pool, ai.clone());
    let key = Uuid::now_v7().to_string();
    let payload = keyed(body("купить молоко", "ha_voice", "auto"), &key);

    // Первый запрос умер после резерва, но до ответа: строка есть, `response`
    // пуст, резерв старше таймаута LLM. Ни `capture_drafts`, ни элементов у
    // такого запроса не осталось (они коммитятся вместе с ответом).
    sqlx::query(
        "INSERT INTO capture_requests (
             household_id, source, idempotency_key, request_hash, capture_id,
             response, created_at, completed_at
         )
         VALUES ($1, 'ha_voice', $2, $3, $4, NULL,
                 now() - interval '61 seconds', NULL)",
    )
    .bind(household.household_id)
    .bind(&key)
    .bind(capture_request_hash("купить молоко", "ha_voice", "auto"))
    .bind(Uuid::now_v7())
    .execute(&db.pool)
    .await
    .expect("stale reservation");

    let (status, _, json) = call_with_key(&app, &token, &key, payload).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["status"], "applied");
    assert_eq!(
        ai.calls(),
        1,
        "the stale reservation must allow a new parse"
    );
    assert_eq!(item_count(&db.pool, household.household_id).await, 1);
    assert_eq!(request_count(&db.pool, household.household_id).await, 1);
}

/// Канонический хеш тела повторяет формат сервера: тест изображает резерв,
/// оставшийся от умершего процесса.
fn capture_request_hash(text: &str, source: &str, mode: &str) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;

    let canonical = json!({
        "text": text,
        "source": source,
        "mode": mode,
        "hint": Value::Null,
        "list_id": Value::Null,
    });
    let digest = Sha256::digest(canonical.to_string().as_bytes());
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_invalid_key_still_uses_the_rate_limit() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let mut config = config();
    config.rate_limit_per_min = 2;
    let ai = StubAi::new(vec![Ok(shopping("молоко"))]);
    let app = app_with(
        &db.pool,
        Some(ai as Arc<dyn AiTask>),
        config,
        Arc::new(CircuitBreaker::default()),
        200,
    );
    let payload = body("купить молоко", "ha_voice", "auto");

    for _ in 0..2 {
        let (status, _, json) = call_with_key(&app, &token, "bad key", payload.clone()).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{json}");
    }

    let (status, headers, json) = call_with_key(&app, &token, "", payload).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{json}");
    assert!(headers.get("retry-after").is_some(), "{headers:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_completed_repeat_ignores_a_list_archived_since_the_first_request() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let ai = StubAi::new(vec![Ok(shopping("молоко"))]);
    let app = app(&db.pool, ai.clone());
    let list_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO lists (id, household_id, kind, name, is_default, position, created_at, updated_at)
         VALUES ($1, $2, 'tasks', 'Работа', false, 10, now(), now())",
    )
    .bind(list_id)
    .bind(household.household_id)
    .execute(&db.pool)
    .await
    .expect("second list");
    let key = Uuid::now_v7().to_string();
    let mut payload = keyed(body("купить молоко", "api", "auto"), &key);
    payload["list_id"] = json!(list_id);

    let (status, _, first) = call_with_key(&app, &token, &key, payload.clone()).await;
    assert_eq!(status, StatusCode::OK, "{first}");

    sqlx::query("UPDATE lists SET is_archived = true WHERE id = $1")
        .bind(list_id)
        .execute(&db.pool)
        .await
        .expect("archive list");

    let (status, _, repeat) = call_with_key(&app, &token, &key, payload.clone()).await;
    assert_eq!(status, StatusCode::OK, "{repeat}");
    assert_eq!(repeat, first, "the saved response must be replayed");

    let mut fresh = body("купить молоко", "api", "auto");
    fresh["list_id"] = json!(list_id);
    let (status, _, json) = call_inner(&app, &token, None, fresh).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{json}");
    assert_validation(&json);
    assert_eq!(ai.calls(), 1, "a replay must not call the LLM");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multiple_idempotency_headers_are_rejected() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let ai = StubAi::new(vec![Ok(shopping("молоко"))]);
    let app = app(&db.pool, ai.clone());
    let payload = body("купить молоко", "ha_voice", "auto");

    let request = Request::builder()
        .method(Method::POST)
        .uri("/api/v1/capture")
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .header("idempotency-key", "first")
        .header("idempotency-key", "second")
        .body(Body::from(serde_json::to_vec(&payload).expect("json")))
        .expect("request");
    let response = app.oneshot(request).await.expect("response");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let json: Value = serde_json::from_slice(&bytes).expect("json");

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{json}");
    assert_validation(&json);
    assert_eq!(ai.calls(), 0);
    assert_eq!(request_count(&db.pool, household.household_id).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_key_that_is_only_in_the_header_is_replayed() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let ai = StubAi::new(vec![Ok(shopping("молоко"))]);
    let app = app(&db.pool, ai.clone());
    let key = Uuid::now_v7().to_string();
    let payload = body("купить молоко", "ha_voice", "auto");

    let (status, _, first) = call_with_key(&app, &token, &key, payload.clone()).await;
    assert_eq!(status, StatusCode::OK, "{first}");

    let (status, _, repeat) = call_with_key(&app, &token, &key, payload).await;
    assert_eq!(status, StatusCode::OK, "{repeat}");
    assert_eq!(repeat, first);
    assert_eq!(ai.calls(), 1);
    assert_eq!(item_count(&db.pool, household.household_id).await, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hint_and_a_list_id_are_part_of_the_request_hash() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let ai = StubAi::new(vec![Ok(shopping("молоко"))]);
    let app = app(&db.pool, ai.clone());
    let key = Uuid::now_v7().to_string();

    let mut first = body("купить молоко", "api", "auto");
    first["hint"] = json!("shopping");
    let (status, _, json) = call_with_key(&app, &token, &key, first).await;
    assert_eq!(status, StatusCode::OK, "{json}");

    let mut other_hint = body("купить молоко", "api", "auto");
    other_hint["hint"] = json!("tasks");
    let (status, _, json) = call_with_key(&app, &token, &key, other_hint).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{json}");
    assert_validation(&json);

    let mut other_list = body("купить молоко", "api", "auto");
    other_list["hint"] = json!("shopping");
    other_list["list_id"] = json!(household.tasks_list_id);
    let (status, _, json) = call_with_key(&app, &token, &key, other_list).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{json}");
    assert_validation(&json);
    assert_eq!(ai.calls(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_quota_503_releases_the_key() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let ai = StubAi::new(vec![Ok(shopping("молоко")), Ok(shopping("молоко"))]);
    let app = app_with(
        &db.pool,
        Some(ai as Arc<dyn AiTask>),
        config(),
        Arc::new(CircuitBreaker::default()),
        0,
    );
    let key = Uuid::now_v7().to_string();
    let payload = keyed(body("купить молоко", "api", "auto"), &key);

    for _ in 0..2 {
        let (status, _, json) = call_with_key(&app, &token, &key, payload.clone()).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{json}");
    }

    assert_eq!(request_count(&db.pool, household.household_id).await, 0);
    assert_eq!(item_count(&db.pool, household.household_id).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_open_breaker_503_releases_the_key() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let breaker = Arc::new(CircuitBreaker::default());
    for _ in 0..5 {
        breaker.record_failure(chrono::Utc::now());
    }
    let ai = StubAi::new(vec![Ok(shopping("молоко")), Ok(shopping("молоко"))]);
    let app = app_with(
        &db.pool,
        Some(ai as Arc<dyn AiTask>),
        config(),
        breaker,
        200,
    );
    let key = Uuid::now_v7().to_string();
    let payload = keyed(body("купить молоко", "api", "auto"), &key);

    for _ in 0..2 {
        let (status, _, json) = call_with_key(&app, &token, &key, payload.clone()).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{json}");
    }

    assert_eq!(request_count(&db.pool, household.household_id).await, 0);
    assert_eq!(item_count(&db.pool, household.household_id).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_concurrent_draft_repeat_gets_409() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let ai = GatedAi::new(two_purchases());
    let app = app(&db.pool, ai.clone());
    let key = Uuid::now_v7().to_string();
    let payload = keyed(body("купить молоко и хлеб", "api", "draft"), &key);

    let first_app = app.clone();
    let first_token = token.clone();
    let first_key = key.clone();
    let first_payload = payload.clone();
    let first = tokio::spawn(async move {
        call_with_key(&first_app, &first_token, &first_key, first_payload).await
    });

    tokio::time::timeout(StdDuration::from_secs(10), ai.started.notified())
        .await
        .expect("the llm call should start");

    let (status, _, json) = call_with_key(&app, &token, &key, payload).await;
    assert_eq!(status, StatusCode::CONFLICT, "{json}");
    assert_eq!(ai.calls(), 1);

    ai.release.notify_one();
    let (status, _, first_json) = tokio::time::timeout(StdDuration::from_secs(10), first)
        .await
        .expect("the first request should finish")
        .expect("the first request should not panic");
    assert_eq!(status, StatusCode::OK, "{first_json}");
    assert_eq!(first_json["status"], "draft");
    assert_eq!(draft_count(&db.pool, household.household_id).await, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_taken_over_reservation_does_not_commit_the_capture() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let ai = TakeoverAi::new(shopping("молоко"));
    let app = app(&db.pool, ai.clone());
    let key = Uuid::now_v7().to_string();
    let payload = keyed(body("купить молоко", "ha_voice", "auto"), &key);

    let first_app = app.clone();
    let first_token = token.clone();
    let first_key = key.clone();
    let first_payload = payload.clone();
    let first = tokio::spawn(async move {
        call_with_key(&first_app, &first_token, &first_key, first_payload).await
    });

    tokio::time::timeout(StdDuration::from_secs(10), ai.started.notified())
        .await
        .expect("the llm call should start");

    // Другой запрос успел перехватить резерв: у строки чужой `capture_id`, и
    // она уже считается зависшей.
    sqlx::query(
        "UPDATE capture_requests
         SET capture_id = $1, created_at = now() - interval '61 seconds'
         WHERE household_id = $2 AND source = 'ha_voice' AND idempotency_key = $3",
    )
    .bind(Uuid::now_v7())
    .bind(household.household_id)
    .bind(&key)
    .execute(&db.pool)
    .await
    .expect("take over reservation");

    ai.release.notify_one();
    let (status, headers, json) = tokio::time::timeout(StdDuration::from_secs(10), first)
        .await
        .expect("the first request should finish")
        .expect("the first request should not panic");

    assert_eq!(status, StatusCode::CONFLICT, "{json}");
    assert_eq!(json["error"]["code"], "conflict");
    assert!(
        headers.get("retry-after").is_some(),
        "Retry-After is required"
    );
    assert_eq!(
        item_count(&db.pool, household.household_id).await,
        0,
        "the late request must not create items"
    );
    assert_eq!(
        draft_count(&db.pool, household.household_id).await,
        0,
        "the late request must not commit capture_drafts"
    );

    // Новый владелец резерва завершает запрос успешно, а повтор возвращает
    // именно его ответ.
    let (status, _, second) = call_with_key(&app, &token, &key, payload.clone()).await;
    assert_eq!(status, StatusCode::OK, "{second}");
    assert_eq!(second["status"], "applied");
    assert_eq!(item_count(&db.pool, household.household_id).await, 1);
    assert_eq!(draft_count(&db.pool, household.household_id).await, 1);

    let (status, _, repeat) = call_with_key(&app, &token, &key, payload).await;
    assert_eq!(status, StatusCode::OK, "{repeat}");
    assert_eq!(
        repeat, second,
        "the repeat must return the new owner's answer"
    );
    assert_eq!(ai.calls(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_taken_over_reservation_does_not_overwrite_the_new_response() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    // Дата в прошлом делает ответ `rejected`, поэтому проверяется запись
    // ответа вне транзакции элементов (`store_response`), а не `apply_auto`.
    let ai = TakeoverAi::new(json!({"items": [
        {"target_kind": "shopping", "title": "хлеб", "due_date": "2000-01-01"}
    ]}));
    let app = app(&db.pool, ai.clone());
    let key = Uuid::now_v7().to_string();
    let payload = keyed(body("купить хлеб", "ha_voice", "auto"), &key);

    let first_app = app.clone();
    let first_token = token.clone();
    let first_key = key.clone();
    let first_payload = payload.clone();
    let first = tokio::spawn(async move {
        call_with_key(&first_app, &first_token, &first_key, first_payload).await
    });

    tokio::time::timeout(StdDuration::from_secs(10), ai.started.notified())
        .await
        .expect("the llm call should start");

    // Новый владелец уже записал свой ответ: опоздавший не должен его затереть
    // и не должен ответить успехом.
    let new_response = json!({
        "capture_id": Uuid::now_v7(),
        "status": "rejected",
        "fallback": false,
        "speech": "new owner",
        "items": [],
        "draft_id": null,
        "expires_at": null,
        "clarifications": [],
    });
    sqlx::query(
        "UPDATE capture_requests
         SET capture_id = $1, response = $2, completed_at = now()
         WHERE household_id = $3 AND source = 'ha_voice' AND idempotency_key = $4",
    )
    .bind(Uuid::now_v7())
    .bind(&new_response)
    .bind(household.household_id)
    .bind(&key)
    .execute(&db.pool)
    .await
    .expect("take over reservation");

    ai.release.notify_one();
    let (status, _, json) = tokio::time::timeout(StdDuration::from_secs(10), first)
        .await
        .expect("the first request should finish")
        .expect("the first request should not panic");

    assert_eq!(status, StatusCode::CONFLICT, "{json}");
    assert_eq!(
        item_count(&db.pool, household.household_id).await,
        0,
        "a rejected late request must not create items"
    );
    assert_eq!(
        draft_count(&db.pool, household.household_id).await,
        0,
        "a rejected late request must not commit capture_drafts"
    );
    assert_eq!(ai.calls(), 1, "the rejected path runs the LLM once");

    let stored: Value = sqlx::query_scalar(
        "SELECT response FROM capture_requests
         WHERE household_id = $1 AND source = 'ha_voice' AND idempotency_key = $2",
    )
    .bind(household.household_id)
    .bind(&key)
    .fetch_one(&db.pool)
    .await
    .expect("stored response");
    assert_eq!(stored, new_response, "the new owner's response is kept");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_late_request_gets_409_after_the_new_owner_has_committed() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let ai = GatedAi::new(shopping("молоко"));
    let gated_app = app(&db.pool, ai.clone());
    let key = Uuid::now_v7().to_string();
    let payload = keyed(body("купить молоко", "ha_voice", "auto"), &key);

    let first_app = gated_app.clone();
    let first_token = token.clone();
    let first_key = key.clone();
    let first_payload = payload.clone();
    let first = tokio::spawn(async move {
        call_with_key(&first_app, &first_token, &first_key, first_payload).await
    });

    tokio::time::timeout(StdDuration::from_secs(10), ai.started.notified())
        .await
        .expect("the llm call should start");

    // Новый владелец перехватывает зависший резерв...
    sqlx::query(
        "UPDATE capture_requests
         SET capture_id = $1, created_at = now() - interval '61 seconds'
         WHERE household_id = $2 AND source = 'ha_voice' AND idempotency_key = $3",
    )
    .bind(Uuid::now_v7())
    .bind(household.household_id)
    .bind(&key)
    .execute(&db.pool)
    .await
    .expect("take over reservation");

    // ...и полностью завершает запрос: `capture_drafts`, элементы и ответ уже
    // закоммичены, когда опоздавший запрос возобновляется.
    let owner_ai = StubAi::new(vec![Ok(shopping("молоко"))]);
    let owner_app = app(&db.pool, owner_ai.clone());
    let (status, _, owner) = call_with_key(&owner_app, &token, &key, payload.clone()).await;
    assert_eq!(status, StatusCode::OK, "{owner}");
    assert_eq!(owner["status"], "applied");
    assert_eq!(item_count(&db.pool, household.household_id).await, 1);
    assert_eq!(draft_count(&db.pool, household.household_id).await, 1);

    ai.release.notify_one();
    let (status, headers, json) = tokio::time::timeout(StdDuration::from_secs(10), first)
        .await
        .expect("the first request should finish")
        .expect("the first request should not panic");

    assert_eq!(status, StatusCode::CONFLICT, "{json}");
    assert_eq!(json["error"]["code"], "conflict");
    assert!(
        headers.get("retry-after").is_some(),
        "Retry-After is required"
    );
    assert_eq!(
        item_count(&db.pool, household.household_id).await,
        1,
        "the late request must not create duplicate items"
    );
    assert_eq!(
        draft_count(&db.pool, household.household_id).await,
        1,
        "the late request must not create another capture row"
    );
    assert_eq!(owner_ai.calls(), 1);

    let (status, _, repeat) = call_with_key(&owner_app, &token, &key, payload).await;
    assert_eq!(status, StatusCode::OK, "{repeat}");
    assert_eq!(repeat, owner, "the repeat returns the new owner's answer");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dropped_request_releases_the_key() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let ai = GatedAi::new(shopping("молоко"));
    let app = app(&db.pool, ai.clone());
    let key = Uuid::now_v7().to_string();
    let payload = keyed(body("купить молоко", "ha_voice", "auto"), &key);

    let first_app = app.clone();
    let first_token = token.clone();
    let first_key = key.clone();
    let first_payload = payload.clone();
    let first = tokio::spawn(async move {
        call_with_key(&first_app, &first_token, &first_key, first_payload).await
    });

    tokio::time::timeout(StdDuration::from_secs(10), ai.started.notified())
        .await
        .expect("the llm call should start");
    first.abort();
    let _ = first.await;

    wait_for_release(&db.pool, household.household_id).await;

    let second_app = app.clone();
    let second_token = token.clone();
    let second_key = key.clone();
    let second_payload = payload.clone();
    let second = tokio::spawn(async move {
        call_with_key(&second_app, &second_token, &second_key, second_payload).await
    });

    tokio::time::timeout(StdDuration::from_secs(10), ai.started.notified())
        .await
        .expect("the repeat should call the llm again");
    ai.release.notify_one();

    let (status, _, json) = tokio::time::timeout(StdDuration::from_secs(10), second)
        .await
        .expect("the repeat should finish")
        .expect("the repeat should not panic");
    assert_eq!(
        status,
        StatusCode::OK,
        "the dropped request must release the key, not leave a 409: {json}"
    );
    assert_eq!(json["status"], "applied");
    assert_eq!(ai.calls(), 2);
    assert_eq!(item_count(&db.pool, household.household_id).await, 1);
}
