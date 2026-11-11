//! Contract-тесты fallback внешнего capture.

mod support;

use std::sync::Arc;
use std::time::{Duration as StdDuration, Instant};

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use chrono::Utc;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::PgPool;
use tasklerq::config::CaptureConfig;
use tasklerq::domain::validation::MAX_TITLE;
use tasklerq::http::api::capture::CaptureState;
use tasklerq::http::router::{AppState, build_router};
use tasklerq::llm::{Chain, LlmError};
use tasklerq::security_audit::Audit;
use tower::ServiceExt;
use uuid::Uuid;

use support::{db, issue_token};

/// Двойник слота: отдаёт заготовленные ответы по очереди и запоминает таймаут.
type StubAi = support::llm::BackendDouble;

fn config() -> CaptureConfig {
    CaptureConfig {
        enabled: true,
        voice_timeout_ms: 8_000,
        voice_fallback_raw: true,
        rate_limit_per_min: 10,
        max_text_chars: 2_000,
    }
}

fn app(pool: &PgPool, ai: Arc<StubAi>, config: CaptureConfig) -> Router {
    app_with(pool, Some(ai), config, 200)
}

fn app_with(pool: &PgPool, ai: Option<Arc<StubAi>>, config: CaptureConfig, quota: u32) -> Router {
    let llm = ai.map(|backend| Arc::new(support::llm::chain(backend, None)));
    app_chain(pool, llm, config, quota)
}

fn app_chain(pool: &PgPool, llm: Option<Arc<Chain>>, config: CaptureConfig, quota: u32) -> Router {
    build_router(
        AppState::new(pool.clone(), Audit::new(pool.clone()))
            .with_capture(CaptureState::new(config, llm, quota)),
    )
}

fn two_purchases() -> Value {
    support::llm::v4::response(vec![
        support::llm::v4::shopping("молоко"),
        support::llm::v4::shopping("хлеб"),
    ])
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

fn validation_failures() -> f64 {
    tasklerq::metrics::render()
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(' ')?;
            (name == "llm_validation_failures_total")
                .then(|| value.parse::<f64>().ok())
                .flatten()
        })
        .unwrap_or(0.0)
}

fn assert_error_envelope(json: &Value, code: &str) {
    assert_eq!(json["error"]["code"], code, "{json}");
    assert!(json["error"]["message"].is_string());
    assert!(json["error"]["details"].is_object());
    assert!(json["error"]["request_id"].is_string());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn llm_unavailability_in_auto_creates_one_fallback_item() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let ai = StubAi::new(vec![Err(LlmError::Transport)]);
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
    assert!(items[0]["date"].is_null());
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
    let ai = StubAi::new(vec![Err(LlmError::Transport)]);
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
    let ai = StubAi::new(vec![Err(LlmError::Transport)]);
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
    let ai = StubAi::hanging();
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
async fn the_voice_budget_is_split_between_two_slots() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let mut config = config();
    // Бюджет голоса уменьшен в десять раз: та же доля основного слота и тот же
    // остаток резервному, но тест не ждёт секунды. Таймауты слотов намеренно
    // больше бюджета: иначе деление пополам не отличить от таймаута слота.
    config.voice_timeout_ms = 800;
    let primary = StubAi::hanging();
    let fallback = StubAi::hanging();
    let chain = support::llm::chain_with_timeouts(
        primary.clone(),
        StdDuration::from_secs(20),
        Some(fallback.clone()),
        StdDuration::from_secs(20),
    );
    let app = app_chain(&db.pool, Some(Arc::new(chain)), config, 200);

    let started = Instant::now();
    let (status, _, json) = call(&app, &token, body("купить молоко", "ha_voice", "auto")).await;
    let elapsed = started.elapsed();

    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["fallback"], true);
    assert_eq!(
        primary.timeouts(),
        vec![StdDuration::from_millis(400)],
        "the primary gets half of the voice budget, not its own timeout"
    );
    let fallback_timeout = fallback
        .timeouts()
        .first()
        .copied()
        .expect("the fallback slot must be called");
    assert!(
        fallback_timeout <= StdDuration::from_millis(400),
        "the fallback gets no more than the rest of the voice budget: {fallback_timeout:?}"
    );
    assert!(
        fallback_timeout > StdDuration::ZERO,
        "the fallback must get the remaining budget: {fallback_timeout:?}"
    );
    assert!(
        elapsed < StdDuration::from_secs(3),
        "the request must respect the budget: {elapsed:?}"
    );
    assert_eq!(item_count(&db.pool, household.household_id).await, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bad_json_answer_without_a_fallback_degrades_after_one_call() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let ai = StubAi::new(vec![Ok(json!({"unexpected": true}))]);
    let app = app(&db.pool, ai.clone(), config());

    let (status, _, json) = call(&app, &token, body("купить молоко", "ha_voice", "auto")).await;

    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["status"], "applied");
    assert_eq!(json["fallback"], true, "{json}");
    assert_eq!(json["items"].as_array().expect("items").len(), 1);
    assert_eq!(ai.calls(), 1, "the same slot must not be called twice");
    let runs: i64 = sqlx::query_scalar("SELECT count(*) FROM llm_runs WHERE household_id = $1")
        .bind(household.household_id)
        .fetch_one(&db.pool)
        .await
        .expect("llm runs");
    assert_eq!(runs, 1, "the rejected response must be logged");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn both_slots_bad_json_degrade_after_one_call_each() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let primary = StubAi::new(vec![Ok(json!({"unexpected": true}))]);
    let fallback = StubAi::new(vec![Ok(json!({"nope": []}))]);
    let chain = support::llm::chain(primary.clone(), Some(fallback.clone()));
    let app = app_chain(&db.pool, Some(Arc::new(chain)), config(), 200);
    let failures_before = validation_failures();

    let (status, _, json) = call(&app, &token, body("купить молоко", "ha_voice", "auto")).await;

    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["status"], "applied");
    assert_eq!(json["fallback"], true);
    assert_eq!(json["items"].as_array().expect("items").len(), 1);
    assert_eq!(primary.calls(), 1, "each slot is called at most once");
    assert_eq!(fallback.calls(), 1);
    let runs: Vec<(Option<String>, Option<i16>, Option<String>)> = sqlx::query_as(
        "SELECT slot, attempt, error_code FROM llm_runs
         WHERE household_id = $1 ORDER BY attempt NULLS FIRST",
    )
    .bind(household.household_id)
    .fetch_all(&db.pool)
    .await
    .expect("llm runs");
    assert_eq!(runs.len(), 2, "each answer is logged: {runs:?}");
    assert_eq!(runs[0].0.as_deref(), Some("primary"));
    assert_eq!(runs[1].0.as_deref(), Some("fallback"));
    assert!(
        validation_failures() >= failures_before + 2.0,
        "both rejected answers must count as validation failures"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_fallback_slot_rescues_the_parse_and_spends_one_quota_unit() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let primary = StubAi::new(vec![Err(LlmError::Transport)]);
    let fallback = StubAi::new(vec![Ok(two_purchases())]);
    let chain = support::llm::chain(primary.clone(), Some(fallback.clone()));
    let app = app_chain(&db.pool, Some(Arc::new(chain)), config(), 200);

    let (status, _, json) = call(&app, &token, body("купить молоко", "ha_voice", "auto")).await;

    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["status"], "applied");
    assert_eq!(json["fallback"], false, "a rescued parse is a normal draft");
    assert_eq!(json["items"].as_array().expect("items").len(), 2);
    assert_eq!(primary.calls(), 1);
    assert_eq!(fallback.calls(), 1);

    let runs: Vec<(Option<String>, Option<i16>)> = sqlx::query_as(
        "SELECT slot, attempt FROM llm_runs WHERE household_id = $1 ORDER BY attempt",
    )
    .bind(household.household_id)
    .fetch_all(&db.pool)
    .await
    .expect("llm runs");
    assert_eq!(
        runs,
        vec![
            (Some("primary".to_owned()), Some(1)),
            (Some("fallback".to_owned()), Some(2)),
        ]
    );
    let since = Utc::now() - chrono::Duration::hours(24);
    let mut conn = db.pool.acquire().await.expect("connection");
    assert_eq!(
        tasklerq::storage::repositories::llm_runs::count_since(
            &mut conn,
            household.household_id,
            since,
        )
        .await
        .expect("quota"),
        1,
        "a run through two slots spends one quota unit"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn llm_unavailability_is_not_retried() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let ai = StubAi::new(vec![Err(LlmError::Transport)]);
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
    let app = app_with(&db.pool, Some(ai.clone()), config(), 0);

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
    let ai = StubAi::new(vec![Ok(two_purchases())]);
    let chain = support::llm::chain(ai.clone(), None);
    support::llm::open_primary(&chain, Utc::now());
    let app = app_chain(&db.pool, Some(Arc::new(chain)), config(), 200);

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
    let runs: i64 = sqlx::query_scalar("SELECT count(*) FROM llm_runs WHERE household_id = $1")
        .bind(household.household_id)
        .fetch_one(&db.pool)
        .await
        .expect("llm runs");
    assert_eq!(runs, 0, "an open circuit must not write an llm run");
    let mut conn = db.pool.acquire().await.expect("connection");
    assert_eq!(
        tasklerq::storage::repositories::llm_runs::count_since(
            &mut conn,
            household.household_id,
            Utc::now() - chrono::Duration::hours(24),
        )
        .await
        .expect("quota"),
        0,
        "a skipped LLM call must not spend quota"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_disabled_fallback_returns_503_in_auto() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let mut config = config();
    config.voice_fallback_raw = false;
    let ai = StubAi::new(vec![Err(LlmError::Transport)]);
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
    let ai = StubAi::new(vec![Err(LlmError::Transport)]);
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
    let ai = StubAi::new(vec![Err(LlmError::Transport)]);
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
        .map(|n| json!(support::llm::v4::task(&format!("задача {n}"))))
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
        Ok(json!({"items": [support::llm::v4::task(&marker)]})),
        Ok(json!({"items": [support::llm::v4::task("позвонить маме")]})),
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
    let app = app_with(&db.pool, None, config(), 200);
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
    let app = app_with(&db.pool, None, config, 200);

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
    let app = app_with(&db.pool, None, config(), 200);

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
    let app = app_with(&db.pool, None, config(), 200);
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
async fn a_slow_invalid_answer_is_not_retried() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let mut config = config();
    config.voice_timeout_ms = 1_000;
    let ai = StubAi::delayed(
        vec![Ok(json!({"unexpected": true}))],
        StdDuration::from_millis(50),
    );
    let app = app(&db.pool, ai.clone(), config);

    let (status, _, json) = call(&app, &token, body("купить молоко", "ha_voice", "auto")).await;

    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["fallback"], true);
    assert_eq!(ai.calls(), 1, "a rejected answer must not be retried");
    let runs: i64 = sqlx::query_scalar("SELECT count(*) FROM llm_runs WHERE household_id = $1")
        .bind(household.household_id)
        .fetch_one(&db.pool)
        .await
        .expect("count llm runs");
    assert_eq!(runs, 1, "only the first call must be logged");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn too_many_items_are_rejected_without_calling_the_fallback() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let items: Vec<Value> = (0..21)
        .map(|n| support::llm::v4::task(&format!("задача {n}")))
        .collect();
    let primary = StubAi::new(vec![Ok(support::llm::v4::response(items))]);
    let fallback = StubAi::new(vec![Ok(two_purchases())]);
    let chain = support::llm::chain(primary.clone(), Some(fallback.clone()));
    let app = app_chain(&db.pool, Some(Arc::new(chain)), config(), 200);

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
    assert_eq!(primary.calls(), 1);
    assert_eq!(
        fallback.calls(),
        0,
        "a user error must not switch to the fallback slot"
    );
    assert_eq!(item_count(&db.pool, household.household_id).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn control_characters_only_are_rejected_before_the_key_is_reserved() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let ai = StubAi::new(Vec::new());
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
    assert_eq!(
        requests, 0,
        "the key must not be reserved for rejected text"
    );

    let (status, _, json) = call_with_key(&app, &token, Some(&key), payload).await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "the repeat must not find a reservation and return 409: {json}"
    );
    assert_eq!(
        ai.calls(),
        0,
        "control-only text must be rejected before the LLM"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_draft_llm_failure_returns_503_without_a_draft() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let ai = StubAi::new(vec![Err(LlmError::Transport)]);
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
    config.voice_timeout_ms = 30;
    let ai = StubAi::hanging();
    let chain = Arc::new(support::llm::chain(ai.clone(), None));
    let app = app_chain(&db.pool, Some(chain.clone()), config, 200);

    for _ in 0..tasklerq::capture::breaker::DEFAULT_THRESHOLD {
        let started = Instant::now();
        let (status, _, json) = call(&app, &token, body("купить молоко", "ha_voice", "auto")).await;
        let elapsed = started.elapsed();
        assert_eq!(status, StatusCode::OK, "{json}");
        assert_eq!(json["fallback"], true);
        assert!(
            elapsed >= StdDuration::from_millis(30),
            "the request must wait for the timeout, not return earlier: {elapsed:?}"
        );
    }

    assert!(
        chain.primary().breaker().retry_after(Utc::now()).is_some(),
        "repeated timeouts must open the slot breaker"
    );

    let (run_status, error_code): (String, Option<String>) = sqlx::query_as(
        "SELECT status, error_code FROM llm_runs
         WHERE household_id = $1 ORDER BY created_at DESC LIMIT 1",
    )
    .bind(household.household_id)
    .fetch_one(&db.pool)
    .await
    .expect("llm run");
    assert_eq!(run_status, "failure");
    assert_eq!(error_code.as_deref(), Some("timeout"));
}

/// Telegram: текст на экране покупок без разбора делится на позиции.
mod telegram_split {
    use super::*;

    use chrono::Duration;
    use tasklerq::telegram::bot::Bot;
    use tasklerq::telegram::capture_worker::{
        CaptureWorker, DEFAULT_DAILY_QUOTA, MAX_ATTEMPTS, Pass,
    };
    use tokio::sync::Notify;

    use support::telegram::{FakeTelegram, Harness};

    struct Stand {
        bot: Harness,
        worker: CaptureWorker,
        ai: Arc<StubAi>,
    }

    async fn stand(
        pool: &PgPool,
        answers: Vec<Result<Value, LlmError>>,
        llm_capture: bool,
    ) -> Stand {
        let id = support::next_telegram_id();
        let handler = Bot::new("tasklerqbot")
            .with_llm_capture(llm_capture)
            .with_tenancy(tasklerq::config::TenancyMode::Multi);
        let ai = StubAi::new(answers);
        let llm = Arc::new(support::llm::chain(ai.clone(), None));
        let mut bot =
            Harness::with_handler(pool, id, Arc::new(handler.clone()), FakeTelegram::new());
        bot.text("/start").await;
        bot.text("Семья").await;
        bot.press("Москва").await;
        bot.press("меню").await;

        let worker = CaptureWorker::new(
            pool.clone(),
            llm,
            handler,
            Arc::new(Notify::new()),
            Audit::new(pool.clone()),
        )
        .only_chat(id)
        .with_quota(DEFAULT_DAILY_QUOTA);
        Stand { bot, worker, ai }
    }

    impl Stand {
        async fn work(&mut self) -> Pass {
            let pass = self
                .worker
                .run_once(self.bot.now)
                .await
                .expect("worker pass");
            self.bot.deliver().await;
            pass
        }

        async fn rows(&self) -> Vec<(String, f64)> {
            sqlx::query_as(
                "SELECT i.title, i.quantity::float8
                 FROM items i
                 JOIN users u ON u.household_id = i.household_id
                 WHERE u.telegram_user_id = $1 AND i.deleted_at IS NULL
                 ORDER BY i.title",
            )
            .bind(self.bot.chat)
            .fetch_all(&self.bot.pool)
            .await
            .expect("items")
        }

        async fn jobs(&self) -> i64 {
            sqlx::query_scalar("SELECT count(*) FROM capture_jobs WHERE chat_id = $1")
                .bind(self.bot.chat)
                .fetch_one(&self.bot.pool)
                .await
                .expect("jobs")
        }
    }

    async fn shopping_list_id(pool: &PgPool, telegram_id: i64) -> Uuid {
        sqlx::query_scalar(
            "SELECT l.id FROM lists l
             JOIN users u ON u.household_id = l.household_id
             WHERE u.telegram_user_id = $1 AND l.kind = 'shopping' AND l.is_default
             LIMIT 1",
        )
        .bind(telegram_id)
        .fetch_one(pool)
        .await
        .expect("shopping list")
    }

    async fn household_id(pool: &PgPool, telegram_id: i64) -> Uuid {
        sqlx::query_scalar("SELECT household_id FROM users WHERE telegram_user_id = $1")
            .bind(telegram_id)
            .fetch_one(pool)
            .await
            .expect("household")
    }

    async fn seed_shopping_item(
        pool: &PgPool,
        telegram_id: i64,
        title: &str,
        quantity: f64,
        unit: &str,
    ) {
        let household = household_id(pool, telegram_id).await;
        let list = shopping_list_id(pool, telegram_id).await;
        sqlx::query(
            "INSERT INTO items (
                 id, household_id, list_id, title, status, priority, source,
                 quantity, unit, position, created_at, updated_at
             )
             VALUES ($1, $2, $3, $4, 'open', 'normal', 'telegram', $5, $6, 1, now(), now())",
        )
        .bind(Uuid::now_v7())
        .bind(household)
        .bind(list)
        .bind(title)
        .bind(quantity)
        .bind(unit)
        .execute(pool)
        .await
        .expect("seeded item");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_disabled_llm_splits_the_text_into_unit_positions() {
        let db = db().await;
        let mut stand = stand(&db.pool, vec![], false).await;

        stand.bot.press("Покупки").await;
        stand.bot.text("молоко, хлеб").await;

        stand.bot.assert_screen_contains(
            "Добавлено без разбора (количество не распознаётся): молоко, хлеб",
        );
        stand.bot.assert_has_button("Отменить");
        assert_eq!(
            stand.rows().await,
            vec![("молоко".to_owned(), 1.0), ("хлеб".to_owned(), 1.0)]
        );
        assert_eq!(stand.jobs().await, 0, "no parse job is queued");
        assert_eq!(stand.ai.calls(), 0, "the LLM must not be called");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unavailable_llm_falls_back_to_splitting() {
        let db = db().await;
        let answers = (0..MAX_ATTEMPTS)
            .map(|_| Err(LlmError::Transport))
            .collect();
        let mut stand = stand(&db.pool, answers, true).await;

        stand.bot.press("Покупки").await;
        stand.bot.text("молоко, хлеб").await;

        let mut last = Pass::Idle;
        for _ in 0..MAX_ATTEMPTS {
            stand.bot.advance(Duration::minutes(5));
            last = stand.work().await;
        }
        assert_eq!(last, Pass::Dead, "all attempts are exhausted");

        stand
            .bot
            .assert_screen_contains("Добавлено без разбора (количество не распознаётся)");
        stand.bot.assert_has_button("Отменить");
        assert_eq!(
            stand.rows().await,
            vec![("молоко".to_owned(), 1.0), ("хлеб".to_owned(), 1.0)],
            "a failed parse adds the positions without the LLM"
        );
        assert_eq!(stand.ai.calls(), MAX_ATTEMPTS as usize);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn newlines_empty_parts_and_repeated_commas_are_ignored() {
        let db = db().await;
        let mut stand = stand(&db.pool, vec![], false).await;

        stand.bot.press("Покупки").await;
        stand.bot.text("молоко,\n\n, хлеб\n,,").await;

        assert_eq!(
            stand.rows().await,
            vec![("молоко".to_owned(), 1.0), ("хлеб".to_owned(), 1.0)],
            "empty parts must not create positions"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_duplicate_rule_applies_to_split_positions() {
        let db = db().await;
        let mut stand = stand(&db.pool, vec![], false).await;
        seed_shopping_item(&db.pool, stand.bot.chat, "Молоко", 2.0, "л").await;

        stand.bot.press("Покупки").await;
        stand.bot.text("молоко, хлеб").await;

        stand.bot.assert_screen_contains("Добавлено без разбора");
        stand.bot.assert_screen_contains("Уже в списке: «молоко»");
        stand.bot.assert_has_button("Увеличить");
        stand.bot.assert_has_button("Добавить отдельно");
        stand.bot.assert_has_button("Отменить");
        assert_eq!(
            stand.rows().await,
            vec![("Молоко".to_owned(), 2.0), ("хлеб".to_owned(), 1.0)],
            "the duplicate must not be saved without a choice"
        );
    }
}
