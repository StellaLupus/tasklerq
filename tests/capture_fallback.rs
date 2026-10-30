//! Contract-тесты fallback внешнего capture.

mod support;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration as StdDuration, Instant};

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use chrono::Utc;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::PgPool;
use tasklerq::capture::breaker::CircuitBreaker;
use tasklerq::config::CaptureConfig;
use tasklerq::domain::validation::MAX_TITLE;
use tasklerq::home_assistant::{AiTask, GenerateData, HaError};
use tasklerq::http::api::capture::CaptureState;
use tasklerq::http::router::{AppState, build_router};
use tower::ServiceExt;
use uuid::Uuid;

use support::{db, issue_token};

/// Подмена Home Assistant: отдаёт заготовленные ответы по очереди и запоминает
/// запрошенный таймаут.
struct StubAi {
    answers: Mutex<VecDeque<Result<Value, HaError>>>,
    timeouts: Mutex<Vec<StdDuration>>,
    calls: AtomicUsize,
}

impl StubAi {
    fn new(answers: Vec<Result<Value, HaError>>) -> Arc<Self> {
        Arc::new(Self {
            answers: Mutex::new(answers.into()),
            timeouts: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn timeouts(&self) -> Vec<StdDuration> {
        self.timeouts.lock().expect("lock").clone()
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
        timeout: StdDuration,
    ) -> Result<Value, HaError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.timeouts.lock().expect("lock").push(timeout);
        self.answers
            .lock()
            .expect("lock")
            .pop_front()
            .unwrap_or(Err(HaError::Unreachable))
    }
}

/// Заглушка, которая не отвечает дольше любого разумного таймаута и при этом
/// игнорирует переданный ей `timeout`: сервер обязан оборвать ожидание сам.
struct HangingAi {
    calls: AtomicUsize,
}

impl HangingAi {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl AiTask for HangingAi {
    async fn check_entity(&self) -> Result<(), HaError> {
        Ok(())
    }

    async fn generate_data(
        &self,
        _request: &GenerateData<'_>,
        _timeout: StdDuration,
    ) -> Result<Value, HaError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(StdDuration::from_secs(30)).await;
        Err(HaError::Timeout)
    }
}

/// Заглушка, которая отвечает мусором после задержки: нужна для проверки
/// повтора при почти исчерпанном бюджете.
struct SlowAi {
    delay: StdDuration,
    answer: Value,
    calls: AtomicUsize,
}

impl SlowAi {
    fn new(delay: StdDuration, answer: Value) -> Arc<Self> {
        Arc::new(Self {
            delay,
            answer,
            calls: AtomicUsize::new(0),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl AiTask for SlowAi {
    async fn check_entity(&self) -> Result<(), HaError> {
        Ok(())
    }

    async fn generate_data(
        &self,
        _request: &GenerateData<'_>,
        _timeout: StdDuration,
    ) -> Result<Value, HaError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(self.delay).await;
        Ok(self.answer.clone())
    }
}

fn config() -> CaptureConfig {
    CaptureConfig {
        enabled: true,
        voice_timeout_ms: 8_000,
        voice_fallback_raw: true,
        rate_limit_per_min: 10,
        max_text_chars: 2_000,
    }
}

fn app(pool: &PgPool, ai: Arc<dyn AiTask>, config: CaptureConfig) -> Router {
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
    ai: Option<Arc<dyn AiTask>>,
    config: CaptureConfig,
    breaker: Arc<CircuitBreaker>,
    quota: u32,
) -> Router {
    build_router(
        AppState::new(pool.clone()).with_capture(CaptureState::new(config, ai, breaker, quota)),
    )
}

fn two_purchases() -> Value {
    json!({"items": [
        {"target_kind": "shopping", "title": "молоко"},
        {"target_kind": "shopping", "title": "хлеб"}
    ]})
}

fn body(text: &str, source: &str, mode: &str) -> Value {
    json!({"text": text, "source": source, "mode": mode})
}

async fn call(app: &Router, token: &str, body: Value) -> (StatusCode, HeaderMap, Value) {
    call_with_key(app, token, None, body).await
}

async fn call_with_key(
    app: &Router,
    token: &str,
    key: Option<&str>,
    body: Value,
) -> (StatusCode, HeaderMap, Value) {
    let mut builder = Request::builder()
        .method(Method::POST)
        .uri("/api/v1/capture")
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json");
    if let Some(key) = key {
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

async fn item_count(pool: &PgPool, household_id: Uuid) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM items WHERE household_id = $1 AND deleted_at IS NULL")
        .bind(household_id)
        .fetch_one(pool)
        .await
        .expect("count items")
}

async fn draft_count(pool: &PgPool, household_id: Uuid, mode: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM capture_drafts WHERE household_id = $1 AND mode::text = $2",
    )
    .bind(household_id)
    .bind(mode)
    .fetch_one(pool)
    .await
    .expect("count drafts")
}

fn assert_error_envelope(json: &Value, code: &str) {
    assert_eq!(json["error"]["code"], code, "{json}");
    assert!(json["error"]["message"].is_string());
    assert!(json["error"]["details"].is_object());
    assert!(json["error"]["request_id"].is_string());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ha_unavailability_in_auto_creates_one_fallback_item() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let ai = StubAi::new(vec![Err(HaError::Unreachable)]);
    let app = app(&db.pool, ai.clone(), config());
    let text = "купить молоко и позвонить маме";

    let (status, _, json) = call(&app, &token, body(text, "ha_voice", "auto")).await;

    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["status"], "applied");
    assert_eq!(json["fallback"], true);
    assert_eq!(json["clarifications"], json!([]));
    assert!(json["draft_id"].is_null());
    assert!(json["expires_at"].is_null());
    let items = json["items"].as_array().expect("items");
    assert_eq!(items.len(), 1, "{json}");
    assert!(items[0]["id"].is_string());
    assert_eq!(items[0]["title"], text);
    assert_eq!(items[0]["kind"], "tasks");
    assert_eq!(items[0]["list_id"], household.tasks_list_id.to_string());
    assert!(items[0]["due_date"].is_null());
    assert!(items[0]["due_at"].is_null());
    let speech = json["speech"].as_str().expect("speech");
    assert!(speech.starts_with("Записал как есть:"), "{speech}");
    assert!(speech.contains(text), "{speech}");
    assert!(speech.chars().count() <= 200, "{speech}");

    let capture_id =
        Uuid::parse_str(json["capture_id"].as_str().expect("capture_id")).expect("uuid");
    let (fallback, raw_text, status): (bool, Option<String>, String) = sqlx::query_as(
        "SELECT fallback, raw_text, status::text FROM capture_drafts
         WHERE id = $1 AND household_id = $2",
    )
    .bind(capture_id)
    .bind(household.household_id)
    .fetch_one(&db.pool)
    .await
    .expect("capture row");
    assert!(fallback, "the capture row must be marked as fallback");
    assert_eq!(raw_text.as_deref(), Some(text));
    assert_eq!(status, "applied");

    let (item_source, item_capture_id): (String, Option<Uuid>) =
        sqlx::query_as("SELECT source::text, capture_id FROM items WHERE id = $1")
            .bind(Uuid::parse_str(items[0]["id"].as_str().expect("item id")).expect("uuid"))
            .fetch_one(&db.pool)
            .await
            .expect("item row");
    assert_eq!(item_source, "ha_voice");
    assert_eq!(item_capture_id, Some(capture_id));
    assert_eq!(ai.calls(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_shopping_hint_puts_the_fallback_item_into_the_shopping_list() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let ai = StubAi::new(vec![Err(HaError::Unreachable)]);
    let app = app(&db.pool, ai.clone(), config());

    let (status, _, json) = call(
        &app,
        &token,
        json!({"text": "купить молоко", "source": "api", "mode": "auto",
               "hint": "shopping"}),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["fallback"], true);
    assert_eq!(json["items"].as_array().expect("items").len(), 1);
    assert_eq!(json["items"][0]["kind"], "shopping");
    assert_eq!(
        json["items"][0]["list_id"],
        household.shopping_list_id.to_string()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_explicit_list_receives_the_fallback_item() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let second = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO lists (id, household_id, kind, name, is_default, position, created_at, updated_at)
         VALUES ($1, $2, 'tasks', 'Работа', false, 10, now(), now())",
    )
    .bind(second)
    .bind(household.household_id)
    .execute(&db.pool)
    .await
    .expect("second list");
    let ai = StubAi::new(vec![Err(HaError::Unreachable)]);
    let app = app(&db.pool, ai.clone(), config());

    let (status, _, json) = call(
        &app,
        &token,
        json!({"text": "позвонить маме", "source": "ha_automation", "mode": "auto",
               "list_id": second}),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["fallback"], true);
    assert_eq!(json["items"][0]["list_id"], second.to_string());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_llm_timeout_depends_on_the_source() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let ai = StubAi::new(vec![
        Ok(two_purchases()),
        Ok(two_purchases()),
        Ok(two_purchases()),
    ]);
    let app = app(&db.pool, ai.clone(), config());

    for source in ["ha_voice", "ha_automation", "api"] {
        let (status, _, json) = call(&app, &token, body("купить молоко", source, "auto")).await;
        assert_eq!(status, StatusCode::OK, "{json}");
    }

    assert_eq!(
        ai.timeouts(),
        vec![
            StdDuration::from_millis(8_000),
            StdDuration::from_millis(20_000),
            StdDuration::from_millis(20_000),
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hanging_llm_is_cut_at_the_voice_timeout_and_falls_back() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let mut config = config();
    config.voice_timeout_ms = 1_000;
    let ai = HangingAi::new();
    let app = app(&db.pool, ai.clone(), config);

    let started = Instant::now();
    let (status, _, json) = call(&app, &token, body("купить молоко", "ha_voice", "auto")).await;
    let elapsed = started.elapsed();

    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["fallback"], true);
    assert_eq!(json["items"].as_array().expect("items").len(), 1);
    assert!(
        elapsed < StdDuration::from_secs(4),
        "the request must not wait for the hanging client: {elapsed:?}"
    );
    assert_eq!(ai.calls(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bad_json_answer_is_retried_once_and_the_second_is_applied() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let ai = StubAi::new(vec![Ok(json!({"unexpected": true})), Ok(two_purchases())]);
    let app = app(&db.pool, ai.clone(), config());

    let (status, _, json) = call(&app, &token, body("купить молоко", "ha_voice", "auto")).await;

    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["status"], "applied");
    assert_eq!(json["fallback"], false);
    assert_eq!(json["items"].as_array().expect("items").len(), 2);
    assert_eq!(ai.calls(), 2);
    let runs: i64 = sqlx::query_scalar("SELECT count(*) FROM llm_runs WHERE household_id = $1")
        .bind(household.household_id)
        .fetch_one(&db.pool)
        .await
        .expect("llm runs");
    assert_eq!(runs, 2, "each call must be logged");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_bad_json_answers_fall_back_after_two_calls() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let ai = StubAi::new(vec![
        Ok(json!({"unexpected": true})),
        Ok(json!({"nope": []})),
    ]);
    let app = app(&db.pool, ai.clone(), config());

    let (status, _, json) = call(&app, &token, body("купить молоко", "ha_voice", "auto")).await;

    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["status"], "applied");
    assert_eq!(json["fallback"], true);
    assert_eq!(json["items"].as_array().expect("items").len(), 1);
    assert_eq!(ai.calls(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_home_assistant_error_is_not_retried() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let ai = StubAi::new(vec![Err(HaError::Unreachable)]);
    let app = app(&db.pool, ai.clone(), config());

    let (status, _, json) = call(&app, &token, body("купить молоко", "api", "auto")).await;

    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["fallback"], true);
    assert_eq!(ai.calls(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_exhausted_quota_falls_back_in_auto_and_is_429_in_draft() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let ai = StubAi::new(vec![Ok(two_purchases())]);
    let app = app_with(
        &db.pool,
        Some(ai.clone() as Arc<dyn AiTask>),
        config(),
        Arc::new(CircuitBreaker::default()),
        0,
    );

    let (status, _, json) = call(&app, &token, body("купить молоко", "ha_voice", "auto")).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["fallback"], true);
    assert_eq!(json["items"].as_array().expect("items").len(), 1);
    assert_eq!(ai.calls(), 0, "quota must be checked before the LLM");

    let (status, headers, json) = call(&app, &token, body("купить хлеб", "api", "draft")).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{json}");
    assert_error_envelope(&json, "rate_limited");
    assert!(headers.get("retry-after").is_some(), "{headers:?}");
    assert_eq!(
        draft_count(&db.pool, household.household_id, "draft").await,
        0
    );
    assert_eq!(ai.calls(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_open_breaker_falls_back_in_auto_and_is_503_in_draft() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let breaker = Arc::new(CircuitBreaker::default());
    for _ in 0..5 {
        breaker.record_failure(Utc::now());
    }
    let ai = StubAi::new(vec![Ok(two_purchases())]);
    let app = app_with(
        &db.pool,
        Some(ai.clone() as Arc<dyn AiTask>),
        config(),
        breaker,
        200,
    );

    let (status, _, json) = call(&app, &token, body("купить молоко", "ha_voice", "auto")).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["fallback"], true);
    assert_eq!(ai.calls(), 0);

    let (status, _, json) = call(&app, &token, body("купить хлеб", "api", "draft")).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{json}");
    assert_error_envelope(&json, "service_unavailable");
    assert_eq!(
        draft_count(&db.pool, household.household_id, "draft").await,
        0
    );
    assert_eq!(ai.calls(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_disabled_fallback_returns_503_in_auto() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let mut config = config();
    config.voice_fallback_raw = false;
    let ai = StubAi::new(vec![Err(HaError::Unreachable)]);
    let app = app(&db.pool, ai.clone(), config);

    let (status, _, json) = call(&app, &token, body("купить молоко", "ha_voice", "auto")).await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{json}");
    assert_error_envelope(&json, "service_unavailable");
    assert_eq!(item_count(&db.pool, household.household_id).await, 0);
    assert_eq!(
        draft_count(&db.pool, household.household_id, "auto").await,
        0
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_long_text_is_cut_at_a_word_boundary_and_kept_in_notes() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let text = format!("купить {}молоко", "очень ".repeat(50));
    assert!(text.chars().count() > MAX_TITLE);
    let ai = StubAi::new(vec![Err(HaError::Unreachable)]);
    let app = app(&db.pool, ai.clone(), config());

    let (status, _, json) = call(&app, &token, body(&text, "ha_voice", "auto")).await;

    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["fallback"], true);
    let item = &json["items"][0];
    let title = item["title"].as_str().expect("title");
    assert!(
        title.chars().count() <= MAX_TITLE,
        "{}",
        title.chars().count()
    );
    assert!(title.chars().count() > MAX_TITLE - 10);
    assert!(text.starts_with(title), "{title}");
    assert!(
        text[title.len()..].starts_with(' '),
        "the title must end at a word boundary: {title}"
    );

    let (stored_title, notes): (String, Option<String>) =
        sqlx::query_as("SELECT title, notes FROM items WHERE id = $1")
            .bind(Uuid::parse_str(item["id"].as_str().expect("id")).expect("uuid"))
            .fetch_one(&db.pool)
            .await
            .expect("item row");
    assert_eq!(stored_title, title);
    assert_eq!(notes.as_deref(), Some(text.as_str()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_repeat_after_fallback_returns_the_same_response() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let ai = StubAi::new(vec![Err(HaError::Unreachable)]);
    let app = app(&db.pool, ai.clone(), config());
    let key = Uuid::now_v7().to_string();
    let payload = body("купить молоко", "ha_voice", "auto");

    let (status, _, first) = call_with_key(&app, &token, Some(&key), payload.clone()).await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(first["fallback"], true);

    let (status, _, repeat) = call_with_key(&app, &token, Some(&key), payload).await;
    assert_eq!(status, StatusCode::OK, "{repeat}");
    assert_eq!(repeat, first);
    assert_eq!(ai.calls(), 1);
    assert_eq!(item_count(&db.pool, household.household_id).await, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn too_many_items_is_still_rejected_with_fallback_enabled() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let items: Vec<Value> = (0..21)
        .map(|n| json!({"target_kind": "tasks", "title": format!("задача {n}")}))
        .collect();
    let ai = StubAi::new(vec![Ok(json!({"items": items}))]);
    let app = app(&db.pool, ai.clone(), config());

    let (status, _, json) = call(
        &app,
        &token,
        body("двадцать одна задача", "ha_voice", "auto"),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["status"], "rejected");
    assert_eq!(json["fallback"], false);
    assert_eq!(json["items"], json!([]));
    assert_eq!(ai.calls(), 1, "too many items must not be retried");
    assert_eq!(item_count(&db.pool, household.household_id).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_error_after_the_reservation_releases_the_key() {
    let db = support::isolated_db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;

    let marker = format!("boom-{}", Uuid::now_v7());
    let suffix = marker.replace('-', "_");
    sqlx::query(&format!(
        "CREATE FUNCTION fail_{suffix}() RETURNS trigger AS $$
         BEGIN
             IF NEW.title = '{marker}' THEN
                 RAISE EXCEPTION 'forced failure';
             END IF;
             RETURN NEW;
         END $$ LANGUAGE plpgsql"
    ))
    .execute(&db.pool)
    .await
    .expect("function");
    sqlx::query(&format!(
        "CREATE TRIGGER fail_{suffix} BEFORE INSERT ON items
         FOR EACH ROW EXECUTE FUNCTION fail_{suffix}()"
    ))
    .execute(&db.pool)
    .await
    .expect("trigger");

    let ai = StubAi::new(vec![
        Ok(json!({"items": [{"target_kind": "tasks", "title": marker}]})),
        Ok(json!({"items": [{"target_kind": "tasks", "title": "позвонить маме"}]})),
    ]);
    let app = app(&db.pool, ai.clone(), config());
    let key = Uuid::now_v7().to_string();
    let payload = body("позвонить маме", "api", "auto");

    let (status, _, json) = call_with_key(&app, &token, Some(&key), payload.clone()).await;
    sqlx::query(&format!("DROP TRIGGER fail_{suffix} ON items"))
        .execute(&db.pool)
        .await
        .expect("drop trigger");
    sqlx::query(&format!("DROP FUNCTION fail_{suffix}()"))
        .execute(&db.pool)
        .await
        .expect("drop function");
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{json}");

    let (status, _, json) = call_with_key(&app, &token, Some(&key), payload).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the reservation must be released, not replayed as 409: {json}"
    );
    assert_eq!(json["status"], "applied");
    assert_eq!(ai.calls(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_taken_over_reservation_rejects_the_old_owner_and_release_ignores_it() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let mut conn = db.pool.acquire().await.expect("connection");
    let key = Uuid::now_v7().to_string();
    let first = Uuid::now_v7();
    let second = Uuid::now_v7();

    let reserved = tasklerq::storage::repositories::capture_requests::reserve(
        &mut conn,
        household.household_id,
        tasklerq::domain::models::CaptureSource::Api,
        &key,
        "hash",
        first,
        Utc::now(),
    )
    .await
    .expect("reserve");
    assert!(matches!(
        reserved,
        tasklerq::storage::repositories::capture_requests::Reserved::Acquired
    ));

    // Второй запрос перехватил просроченный резерв.
    sqlx::query(
        "UPDATE capture_requests SET capture_id = $3, created_at = now() - interval '61 seconds'
         WHERE household_id = $1 AND source = 'api' AND idempotency_key = $2",
    )
    .bind(household.household_id)
    .bind(&key)
    .bind(second)
    .execute(&mut *conn)
    .await
    .expect("take over");

    let error = tasklerq::storage::repositories::capture_requests::complete(
        &mut conn,
        household.household_id,
        tasklerq::domain::models::CaptureSource::Api,
        &key,
        first,
        &json!({"stale": true}),
        Utc::now(),
    )
    .await
    .expect_err("the old owner must not complete a taken over reservation");
    assert!(matches!(
        error,
        tasklerq::storage::repositories::capture_requests::CompleteError::Lost
    ));

    let (capture_id, response): (Uuid, Option<Value>) = sqlx::query_as(
        "SELECT capture_id, response FROM capture_requests
         WHERE household_id = $1 AND source = 'api' AND idempotency_key = $2",
    )
    .bind(household.household_id)
    .bind(&key)
    .fetch_one(&mut *conn)
    .await
    .expect("row");
    assert_eq!(capture_id, second, "the old owner must not rewrite the row");
    assert_eq!(response, None);

    tasklerq::storage::repositories::capture_requests::release(
        &mut conn,
        household.household_id,
        tasklerq::domain::models::CaptureSource::Api,
        &key,
        first,
    )
    .await
    .expect("release the old owner");

    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM capture_requests
         WHERE household_id = $1 AND source = 'api' AND idempotency_key = $2",
    )
    .bind(household.household_id)
    .bind(&key)
    .fetch_one(&mut *conn)
    .await
    .expect("count");
    assert_eq!(count, 1, "the new owner's reservation must survive");

    tasklerq::storage::repositories::capture_requests::complete(
        &mut conn,
        household.household_id,
        tasklerq::domain::models::CaptureSource::Api,
        &key,
        second,
        &json!({"fresh": true}),
        Utc::now(),
    )
    .await
    .expect("complete the new owner");
    let response: Option<Value> = sqlx::query_scalar(
        "SELECT response FROM capture_requests
         WHERE household_id = $1 AND source = 'api' AND idempotency_key = $2",
    )
    .bind(household.household_id)
    .bind(&key)
    .fetch_one(&mut *conn)
    .await
    .expect("row");
    assert_eq!(response, Some(json!({"fresh": true})));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_disabled_llm_falls_back_in_auto_when_raw_fallback_is_enabled() {
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
    let text = "купить молоко";

    let (status, _, json) = call(&app, &token, body(text, "ha_voice", "auto")).await;

    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["status"], "applied");
    assert_eq!(json["fallback"], true);
    assert_eq!(json["items"].as_array().expect("items").len(), 1);
    assert_eq!(json["items"][0]["title"], text);
    let speech = json["speech"].as_str().expect("speech");
    assert!(speech.starts_with("Записал как есть:"), "{speech}");

    let runs: i64 = sqlx::query_scalar("SELECT count(*) FROM llm_runs WHERE household_id = $1")
        .bind(household.household_id)
        .fetch_one(&db.pool)
        .await
        .expect("count llm runs");
    assert_eq!(runs, 0, "a disabled LLM must not create an llm run");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_disabled_llm_without_raw_fallback_returns_503_in_auto() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let mut config = config();
    config.voice_fallback_raw = false;
    let app = app_with(
        &db.pool,
        None,
        config,
        Arc::new(CircuitBreaker::default()),
        200,
    );

    let (status, _, json) = call(&app, &token, body("купить молоко", "ha_voice", "auto")).await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{json}");
    assert_error_envelope(&json, "service_unavailable");
    assert_eq!(item_count(&db.pool, household.household_id).await, 0);
    assert_eq!(
        draft_count(&db.pool, household.household_id, "auto").await,
        0
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_disabled_llm_in_draft_returns_503_and_creates_nothing() {
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

    let (status, _, json) = call(&app, &token, body("купить молоко", "api", "draft")).await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{json}");
    assert_error_envelope(&json, "service_unavailable");
    assert_eq!(
        draft_count(&db.pool, household.household_id, "draft").await,
        0
    );
    assert_eq!(item_count(&db.pool, household.household_id).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_repeat_after_a_disabled_llm_fallback_returns_the_stored_response() {
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
    let key = Uuid::now_v7().to_string();
    let payload = body("купить молоко", "ha_voice", "auto");

    let (status, _, first) = call_with_key(&app, &token, Some(&key), payload.clone()).await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(first["fallback"], true);

    let (status, _, repeat) = call_with_key(&app, &token, Some(&key), payload).await;
    assert_eq!(status, StatusCode::OK, "{repeat}");
    assert_eq!(repeat, first, "the saved fallback must be replayed");

    let requests: i64 =
        sqlx::query_scalar("SELECT count(*) FROM capture_requests WHERE household_id = $1")
            .bind(household.household_id)
            .fetch_one(&db.pool)
            .await
            .expect("count requests");
    assert_eq!(requests, 1);
    assert_eq!(item_count(&db.pool, household.household_id).await, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_parse_retry_is_skipped_when_less_than_a_second_is_left() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let mut config = config();
    config.voice_timeout_ms = 1_000;
    let ai = SlowAi::new(StdDuration::from_millis(900), json!({"unexpected": true}));
    let app = app(&db.pool, ai.clone(), config);

    let (status, _, json) = call(&app, &token, body("купить молоко", "ha_voice", "auto")).await;

    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["fallback"], true);
    assert_eq!(
        ai.calls(),
        1,
        "a retry with almost no budget must not start"
    );
    let runs: i64 = sqlx::query_scalar("SELECT count(*) FROM llm_runs WHERE household_id = $1")
        .bind(household.household_id)
        .fetch_one(&db.pool)
        .await
        .expect("count llm runs");
    assert_eq!(runs, 1, "only the first call must be logged");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn too_many_items_in_the_retry_is_rejected_not_fallen_back() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let items: Vec<Value> = (0..21)
        .map(|n| json!({"target_kind": "tasks", "title": format!("задача {n}")}))
        .collect();
    let ai = StubAi::new(vec![
        Ok(json!({"unexpected": true})),
        Ok(json!({"items": items})),
    ]);
    let app = app(&db.pool, ai.clone(), config());

    let (status, _, json) = call(
        &app,
        &token,
        body("двадцать одна задача", "ha_voice", "auto"),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["status"], "rejected");
    assert_eq!(json["fallback"], false);
    assert_eq!(json["items"], json!([]));
    assert_eq!(ai.calls(), 2);
    assert_eq!(item_count(&db.pool, household.household_id).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn control_characters_only_cannot_be_stored_and_release_the_key() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let ai = StubAi::new(vec![Err(HaError::Unreachable), Err(HaError::Unreachable)]);
    let app = app(&db.pool, ai.clone(), config());
    let key = Uuid::now_v7().to_string();
    let payload = body("\u{1}\u{2}", "ha_voice", "auto");

    let (status, _, json) = call_with_key(&app, &token, Some(&key), payload.clone()).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{json}");
    assert_error_envelope(&json, "validation_error");

    let requests: i64 =
        sqlx::query_scalar("SELECT count(*) FROM capture_requests WHERE household_id = $1")
            .bind(household.household_id)
            .fetch_one(&db.pool)
            .await
            .expect("count requests");
    assert_eq!(requests, 0, "the reservation must be released");

    let (status, _, json) = call_with_key(&app, &token, Some(&key), payload).await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "the repeat must be processed again, not rejected as 409: {json}"
    );
    assert_eq!(ai.calls(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_draft_llm_failure_returns_503_without_a_draft() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let ai = StubAi::new(vec![Err(HaError::Unreachable)]);
    let app = app(&db.pool, ai.clone(), config());

    let (status, _, json) = call(&app, &token, body("купить молоко", "api", "draft")).await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{json}");
    assert_error_envelope(&json, "service_unavailable");
    assert_eq!(
        draft_count(&db.pool, household.household_id, "draft").await,
        0
    );
    assert_eq!(item_count(&db.pool, household.household_id).await, 0);
    assert_eq!(ai.calls(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hanging_llm_is_recorded_as_a_breaker_failure() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let mut config = config();
    config.voice_timeout_ms = 300;
    let breaker = Arc::new(CircuitBreaker::new(
        1,
        chrono::Duration::seconds(300),
        chrono::Duration::seconds(60),
    ));
    let ai = HangingAi::new();
    let app = app_with(&db.pool, Some(ai.clone()), config, breaker.clone(), 200);

    let started = Instant::now();
    let (status, _, json) = call(&app, &token, body("купить молоко", "ha_voice", "auto")).await;
    let elapsed = started.elapsed();

    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["fallback"], true);
    assert!(
        elapsed >= StdDuration::from_millis(300),
        "the request must wait for the timeout, not return earlier: {elapsed:?}"
    );
    assert!(
        breaker.retry_after(Utc::now()).is_some(),
        "the timeout must be recorded as a breaker failure"
    );

    let (run_status, error_code): (String, Option<String>) =
        sqlx::query_as("SELECT status, error_code FROM llm_runs WHERE household_id = $1")
            .bind(household.household_id)
            .fetch_one(&db.pool)
            .await
            .expect("llm run");
    assert_eq!(run_status, "failure");
    assert_eq!(error_code.as_deref(), Some("timeout"));
}
