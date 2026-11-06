//! События безопасности в `audit_events`: отказы аутентификации, лимиты,
//! ошибки LLM-валидации, порог отказов webhook и отсутствие секретов.

mod support;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration as StdDuration;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Method, Request, StatusCode};
use chrono::{Duration, Utc};
use serde_json::{Value, json};
use sqlx::PgPool;
use tasklerq::admin;
use tasklerq::auth::tokens::hash_token;
use tasklerq::capture::breaker::CircuitBreaker;
use tasklerq::capture::prompt::PROMPT_VERSION;
use tasklerq::config::{CaptureConfig, SecretString, TenancyMode};
use tasklerq::home_assistant::{AiTask, GenerateData, HaError};
use tasklerq::http::api::capture::CaptureState;
use tasklerq::http::router::{AppState, build_router};
use tasklerq::security_audit::Audit;
use tasklerq::storage::repositories::capture_jobs::{self, NewJob};
use tasklerq::storage::repositories::members;
use tasklerq::telegram::bot::{Bot, Outcome};
use tasklerq::telegram::capture_worker::{CaptureWorker, Pass as CapturePass};
use tasklerq::telegram::webhook::{Ingress, SECRET_HEADER};
use tasklerq::telegram::worker::{DEFERRED_QUEUE_LIMIT, Pass, UpdateHandler, UpdateWorker};
use tokio::sync::Notify;
use tower::ServiceExt;
use uuid::Uuid;

use support::telegram::FakeTelegram;
use support::{TestHousehold, create_household, isolated_db, issue_token, next_telegram_id};

const SECRET: &str = "configured-webhook-SECRET-value";

fn peer() -> SocketAddr {
    SocketAddr::from(([172, 18, 0, 1], 4040))
}

fn app_with_webhook(pool: &PgPool, audit: Audit, per_second: u32) -> Router {
    // Длинное окно, чтобы последовательные запросы теста не зависели от
    // реального времени; скользящее окно проверяет unit-тест лимитера.
    let ingress = Ingress::with_rate_limit(
        &SecretString::new(SECRET),
        Arc::new(Notify::new()),
        per_second,
        3_600,
    );
    build_router(AppState::new(pool.clone(), audit).with_telegram(ingress))
        .layer(MockConnectInfo(peer()))
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

async fn post_webhook(app: &Router, secret: Option<&str>, body: &[u8]) -> StatusCode {
    post_webhook_from(app, secret, None, body).await
}

async fn post_webhook_from(
    app: &Router,
    secret: Option<&str>,
    forwarded: Option<&str>,
    body: &[u8],
) -> StatusCode {
    let mut request = Request::builder().method("POST").uri("/webhooks/telegram");
    if let Some(secret) = secret {
        request = request.header(SECRET_HEADER, secret);
    }
    if let Some(forwarded) = forwarded {
        request = request.header("x-forwarded-for", forwarded);
    }
    app.clone()
        .oneshot(request.body(Body::from(body.to_vec())).expect("request"))
        .await
        .expect("response")
        .status()
}

async fn update_payload(id: i64) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "update_id": id,
        "message": {
            "message_id": 1,
            "date": 1_700_000_000,
            "chat": {"id": 42, "type": "private", "first_name": "Ann"},
            "from": {"id": 42, "is_bot": false, "first_name": "Ann"},
            "text": "Купить лекарство"
        }
    }))
    .expect("json")
}

async fn payloads(pool: &PgPool, event_type: &str) -> Vec<Value> {
    sqlx::query_scalar(
        "SELECT payload FROM audit_events
         WHERE event_type::text = $1
         ORDER BY created_at, id",
    )
    .bind(event_type)
    .fetch_all(pool)
    .await
    .expect("audit payloads")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_authentication_failures_are_audited_without_secrets() {
    let db = isolated_db().await;
    let household = create_household(&db.pool).await;
    let now = Utc::now();

    // Нет токена.
    let app = support::app(&db.pool);
    let (status, _) = support::request(&app, Method::GET, "/api/v1/household", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let events = payloads(&db.pool, "auth_failed").await;
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["source"], "api");
    assert_eq!(events[0]["reason"], "missing_token");
    assert_eq!(events[0]["route"], "/api/v1/household");
    assert!(events[0].get("token_id").is_none());

    // Неизвестный токен: причина входит в ключ подавления, поэтому событие
    // записывается тем же журналом, что и `missing_token`.
    let (status, _) = support::request(
        &app,
        Method::GET,
        "/api/v1/household",
        Some("tlq_Bearer-SECRET-unknown"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let events = payloads(&db.pool, "auth_failed").await;
    let unknown = events
        .iter()
        .find(|payload| payload["reason"] == "unknown_token")
        .expect("unknown token event");
    assert_eq!(unknown["route"], "/api/v1/household");
    assert!(unknown.get("token_id").is_none(), "{unknown}");

    // Отозванный токен: идентификатор опознан, значение — нет.
    let revoked = admin::create_token(
        &db.pool,
        TenancyMode::Single,
        Some(household.household_id),
        "revoked",
        &["ha:read".to_owned()],
        None,
        now,
    )
    .await
    .expect("token");
    admin::revoke_token(&db.pool, revoked.record.id, now)
        .await
        .expect("revoke");
    let (status, _) = support::request(
        &app,
        Method::GET,
        "/api/v1/household",
        Some(&revoked.token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let events = payloads(&db.pool, "auth_failed").await;
    let revoked_event = events
        .iter()
        .find(|payload| payload["reason"] == "revoked_token")
        .expect("revoked token event");
    assert_eq!(revoked_event["token_id"], revoked.record.id.to_string());
    let stored_household: Option<Uuid> = sqlx::query_scalar(
        "SELECT household_id FROM audit_events
         WHERE event_type = 'auth_failed' AND payload->>'reason' = 'revoked_token'",
    )
    .fetch_one(&db.pool)
    .await
    .expect("household of the event");
    assert_eq!(stored_household, Some(household.household_id));

    // Просроченный токен.
    let expiring = admin::create_token(
        &db.pool,
        TenancyMode::Single,
        Some(household.household_id),
        "expiring",
        &["ha:read".to_owned()],
        Some(1),
        now,
    )
    .await
    .expect("token");
    sqlx::query("UPDATE api_tokens SET expires_at = $2 WHERE id = $1")
        .bind(expiring.record.id)
        .bind(now - Duration::minutes(1))
        .execute(&db.pool)
        .await
        .expect("expire");
    let (status, _) = support::request(
        &app,
        Method::GET,
        "/api/v1/household",
        Some(&expiring.token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let events = payloads(&db.pool, "auth_failed").await;
    let expired = events
        .iter()
        .find(|payload| payload["reason"] == "expired_token")
        .expect("expired token event");
    assert_eq!(expired["token_id"], expiring.record.id.to_string());

    // Не хватает scope.
    let read_only = admin::create_token(
        &db.pool,
        TenancyMode::Single,
        Some(household.household_id),
        "read only",
        &["ha:read".to_owned()],
        None,
        now,
    )
    .await
    .expect("token");
    let (status, _) = support::request(
        &app,
        Method::POST,
        "/api/v1/lists",
        Some(&read_only.token),
        Some(json!({"kind": "checklist", "name": "Чужое право"})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let events = payloads(&db.pool, "auth_failed").await;
    let scope = events
        .iter()
        .find(|payload| payload["reason"] == "missing_scope")
        .expect("missing scope event");
    assert_eq!(scope["route"], "/api/v1/lists");
    assert_eq!(scope["token_id"], read_only.record.id.to_string());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rate_limits_are_audited_for_webhook_and_capture() {
    let db = isolated_db().await;
    let household = create_household(&db.pool).await;

    // Webhook: один запрос в окно, второй отклоняется.
    let app = app_with_webhook(&db.pool, Audit::new(db.pool.clone()), 1);
    assert_eq!(
        post_webhook(&app, Some(SECRET), &update_payload(500).await).await,
        StatusCode::OK
    );
    assert_eq!(
        post_webhook(&app, Some(SECRET), &update_payload(501).await).await,
        StatusCode::TOO_MANY_REQUESTS
    );
    let limits = payloads(&db.pool, "rate_limited").await;
    let webhook = limits
        .iter()
        .find(|payload| payload["limit"] == "webhook_per_second")
        .expect("webhook limit event");
    assert_eq!(webhook["ip"], "172.18.0.1");
    assert!(webhook["retry_after"].as_u64().unwrap_or(0) >= 1);
    assert_eq!(webhook["suppressed"], 0);

    // Capture: один разбор в минуту на токен.
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let mut config = capture_config();
    config.rate_limit_per_min = 1;
    let app = build_router(
        AppState::new(db.pool.clone(), Audit::new(db.pool.clone())).with_capture(
            CaptureState::new(config, None, Arc::new(CircuitBreaker::default()), 200),
        ),
    );
    let body = json!({"text": "Купить лекарство", "source": "api", "mode": "draft"});
    let (first, _) = support::request(
        &app,
        Method::POST,
        "/api/v1/capture",
        Some(&token),
        Some(body.clone()),
    )
    .await;
    assert_eq!(first, StatusCode::SERVICE_UNAVAILABLE);
    let (second, _) = support::request(
        &app,
        Method::POST,
        "/api/v1/capture",
        Some(&token),
        Some(body),
    )
    .await;
    assert_eq!(second, StatusCode::TOO_MANY_REQUESTS);

    let limits = payloads(&db.pool, "rate_limited").await;
    let capture = limits
        .iter()
        .find(|payload| payload["limit"] == "capture_per_minute")
        .expect("capture limit event");
    assert!(capture["token_id"].is_string(), "{capture}");
    assert!(capture["retry_after"].as_u64().unwrap_or(0) >= 1);
    let stored_household: Option<Uuid> = sqlx::query_scalar(
        "SELECT household_id FROM audit_events
         WHERE event_type = 'rate_limited' AND payload->>'limit' = 'capture_per_minute'",
    )
    .fetch_one(&db.pool)
    .await
    .expect("household of the event");
    assert_eq!(stored_household, Some(household.household_id));
}

struct NoopHandler;

#[async_trait]
impl UpdateHandler for NoopHandler {
    async fn handle(
        &self,
        _conn: &mut sqlx::PgConnection,
        _payload: &Value,
        _now: chrono::DateTime<Utc>,
    ) -> Result<Outcome, sqlx::Error> {
        Ok(Outcome::default())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_telegram_update_limit_is_audited() {
    let db = isolated_db().await;
    let chat = next_telegram_id();
    let fake = FakeTelegram::new();
    let worker = UpdateWorker::new(
        db.pool.clone(),
        Arc::new(NoopHandler),
        fake,
        10,
        Arc::new(Notify::new()),
        Audit::new(db.pool.clone()),
    )
    .only_chat(chat)
    .with_rate_limit(1);

    let mut conn = db.pool.acquire().await.expect("connection");
    for id in 0..2 {
        let payload = json!({
            "update_id": 900 + id,
            "message": {
                "message_id": 900 + id,
                "date": 1_700_000_000,
                "chat": {"id": chat, "type": "private", "first_name": "Ann"},
                "from": {"id": chat, "is_bot": false, "first_name": "Ann"},
                "text": "привет"
            }
        });
        tasklerq::telegram::queue::enqueue(&mut conn, 900 + id, &payload, Utc::now())
            .await
            .expect("enqueue");
    }
    drop(conn);

    for expected in [Pass::Processed, Pass::Deferred] {
        assert_eq!(worker.run_once(Utc::now()).await.expect("pass"), expected);
    }

    let limits = payloads(&db.pool, "rate_limited").await;
    assert_eq!(limits.len(), 1, "{limits:?}");
    assert_eq!(limits[0]["limit"], "telegram_updates_per_minute");
    assert_eq!(limits[0]["user_id"], chat);
    assert!(limits[0]["retry_after"].as_u64().unwrap_or(0) >= 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dropped_telegram_update_is_audited() {
    let db = isolated_db().await;
    let chat = next_telegram_id();
    let worker = UpdateWorker::new(
        db.pool.clone(),
        Arc::new(NoopHandler),
        FakeTelegram::new(),
        10,
        Arc::new(Notify::new()),
        Audit::new(db.pool.clone()),
    )
    .only_chat(chat)
    .with_rate_limit(1);

    let now = Utc::now();
    let mut conn = db.pool.acquire().await.expect("connection");
    // Очередь отложенных уже заполнена до предела. Событие `rate_limited`
    // пишется впервые именно на отбрасывании: откладывания не было, поэтому
    // подавление повторов его не скроет.
    for id in 0..DEFERRED_QUEUE_LIMIT {
        let payload = json!({
            "update_id": 800 + id,
            "message": {
                "message_id": 800 + id,
                "date": 1_700_000_000,
                "chat": {"id": chat, "type": "private", "first_name": "Ann"},
                "from": {"id": chat, "is_bot": false, "first_name": "Ann"},
                "text": "привет"
            }
        });
        sqlx::query(
            "INSERT INTO telegram_updates (update_id, payload, available_at, received_at)
             VALUES ($1, $2, $3, $3)",
        )
        .bind(800 + id)
        .bind(&payload)
        .bind(now + Duration::hours(1))
        .execute(&mut *conn)
        .await
        .expect("deferred row");
    }

    // Первое обновление расходует единственный слот лимита.
    let first = json!({
        "update_id": 900,
        "message": {
            "message_id": 900,
            "date": 1_700_000_000,
            "chat": {"id": chat, "type": "private", "first_name": "Ann"},
            "from": {"id": chat, "is_bot": false, "first_name": "Ann"},
            "text": "привет"
        }
    });
    tasklerq::telegram::queue::enqueue(&mut conn, 900, &first, now)
        .await
        .expect("first update");
    drop(conn);
    assert_eq!(
        worker.run_once(now).await.expect("first pass"),
        Pass::Processed
    );
    assert!(
        payloads(&db.pool, "rate_limited").await.is_empty(),
        "an allowed update must not be audited as rate limited"
    );

    // Второе обновление сверх слота, а очередь отложенных полна: отбрасывание.
    let mut conn = db.pool.acquire().await.expect("connection");
    let second = json!({
        "update_id": 901,
        "message": {
            "message_id": 901,
            "date": 1_700_000_000,
            "chat": {"id": chat, "type": "private", "first_name": "Ann"},
            "from": {"id": chat, "is_bot": false, "first_name": "Ann"},
            "text": "привет"
        }
    });
    tasklerq::telegram::queue::enqueue(&mut conn, 901, &second, now)
        .await
        .expect("second update");
    drop(conn);
    assert_eq!(
        worker.run_once(now).await.expect("second pass"),
        Pass::Dropped
    );

    let limits = payloads(&db.pool, "rate_limited").await;
    assert_eq!(limits.len(), 1, "{limits:?}");
    assert_eq!(limits[0]["limit"], tasklerq::security_audit::TELEGRAM_LIMIT);
    assert_eq!(limits[0]["user_id"], chat);
    assert!(limits[0]["retry_after"].as_u64().unwrap_or(0) >= 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_invite_limit_is_audited() {
    use support::telegram::Harness;

    let db = isolated_db().await;
    let mut owner = Harness::new(&db.pool, next_telegram_id()).named("Алексей");
    owner.text("/start").await;
    owner.text("Семья").await;
    owner.press("Москва").await;
    owner.press("меню").await;
    owner.press("Настройки").await;
    owner.press("Семья").await;
    owner.press("Пригласить").await;

    for _ in 0..5 {
        owner.press("Создать приглашение").await;
        owner.press("Назад").await;
    }
    owner.press("Создать приглашение").await;

    let household_id: Uuid =
        sqlx::query_scalar("SELECT household_id FROM users WHERE telegram_user_id = $1")
            .bind(owner.chat)
            .fetch_one(&db.pool)
            .await
            .expect("owner household");
    let limits = payloads(&db.pool, "rate_limited").await;
    assert_eq!(limits.len(), 1, "{limits:?}");
    assert_eq!(limits[0]["limit"], "invites_per_hour");
    assert_eq!(limits[0]["household_id"], household_id.to_string());
    let stored_household: Option<Uuid> = sqlx::query_scalar(
        "SELECT household_id FROM audit_events WHERE event_type = 'rate_limited'",
    )
    .fetch_one(&db.pool)
    .await
    .expect("household of the event");
    assert_eq!(stored_household, Some(household_id));
}

/// Home Assistant с заготовленным ответом.
struct StubAi {
    answer: Value,
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
        Ok(self.answer.clone())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn llm_validation_failures_are_audited_for_api_and_telegram() {
    const LLM_RESPONSE: &str = "LLM-RESPONSE-SECRET-NAME";
    const TEXT: &str = "Купить лекарство";

    let db = isolated_db().await;
    let household = create_household(&db.pool).await;

    // HTTP API: ответ модели не прошёл разбор схемы.
    let token = issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let ai: Arc<dyn AiTask> = Arc::new(StubAi {
        answer: json!({"items": LLM_RESPONSE}),
    });
    let app = build_router(
        AppState::new(db.pool.clone(), Audit::new(db.pool.clone())).with_capture(
            CaptureState::new(
                capture_config(),
                Some(ai),
                Arc::new(CircuitBreaker::default()),
                200,
            ),
        ),
    );
    let (status, _) = support::request(
        &app,
        Method::POST,
        "/api/v1/capture",
        Some(&token),
        Some(json!({"text": TEXT, "source": "api", "mode": "auto"})),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

    let events = payloads(&db.pool, "llm_validation_failed").await;
    assert_eq!(events.len(), 1, "{events:?}");
    let api = &events[0];
    assert_eq!(api["source"], "api");
    assert_eq!(api["stage"], "parse");
    assert_eq!(api["code"], "invalid_field");
    assert_eq!(api["prompt_version"], PROMPT_VERSION);
    assert!(api["capture_id"].is_string(), "{api}");
    assert!(!api.to_string().contains(TEXT), "{api}");
    assert!(!api.to_string().contains(LLM_RESPONSE), "{api}");

    // Telegram: тот же путь разбора в воркере.
    let owner_chat: i64 = sqlx::query_scalar("SELECT telegram_chat_id FROM users WHERE id = $1")
        .bind(household.owner_id)
        .fetch_one(&db.pool)
        .await
        .expect("owner chat");
    let now = Utc::now();
    let mut conn = db.pool.acquire().await.expect("connection");
    let job_id = capture_jobs::enqueue(
        &mut conn,
        NewJob::telegram_text(household.household_id, household.owner_id, owner_chat, TEXT),
        now,
    )
    .await
    .expect("job");
    drop(conn);

    let worker = CaptureWorker::new(
        db.pool.clone(),
        Arc::new(StubAi {
            answer: json!({"items": LLM_RESPONSE}),
        }),
        Bot::new("tasklerqbot"),
        Arc::new(Notify::new()),
        Audit::new(db.pool.clone()),
    )
    .only_chat(owner_chat);
    assert_eq!(
        worker.run_once(now).await.expect("capture pass"),
        CapturePass::Dead
    );

    let events = payloads(&db.pool, "llm_validation_failed").await;
    let telegram = events
        .iter()
        .find(|payload| payload["source"] == "telegram")
        .expect("telegram validation event");
    assert_eq!(telegram["job_id"], job_id.to_string());
    assert_eq!(telegram["code"], "invalid_field");
    assert!(!telegram.to_string().contains(TEXT), "{telegram}");
    let stored_household: Option<Uuid> = sqlx::query_scalar(
        "SELECT household_id FROM audit_events
         WHERE event_type = 'llm_validation_failed' AND payload->>'source' = 'telegram'",
    )
    .fetch_one(&db.pool)
    .await
    .expect("household of the event");
    assert_eq!(stored_household, Some(household.household_id));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn token_creation_revocation_and_role_changes_are_audited() {
    let db = isolated_db().await;
    let household = create_household(&db.pool).await;
    let now = Utc::now();

    let issued = admin::create_token(
        &db.pool,
        TenancyMode::Single,
        Some(household.household_id),
        "Home Assistant",
        &["ha:read".to_owned(), "ha:write".to_owned()],
        None,
        now,
    )
    .await
    .expect("token");
    admin::revoke_token(&db.pool, issued.record.id, now)
        .await
        .expect("revoke");

    let created = payloads(&db.pool, "token_created").await;
    assert_eq!(created.len(), 1, "{created:?}");
    let rendered = created[0].to_string();
    assert_eq!(created[0]["name"], "Home Assistant");
    assert_eq!(created[0]["scopes"][0], "ha:read");
    assert!(!rendered.contains(&issued.token));
    assert!(!rendered.contains(&hash_token(&issued.token)));

    let revoked = payloads(&db.pool, "token_revoked").await;
    assert_eq!(revoked.len(), 1, "{revoked:?}");
    assert_eq!(revoked[0]["token_id"], issued.record.id.to_string());
    assert!(!revoked[0].to_string().contains(&issued.token));

    // Передача владения: две записи о роли с прежней и новой ролью.
    let mut conn = db.pool.acquire().await.expect("connection");
    members::transfer_ownership(
        &mut conn,
        household.household_id,
        &household.owner(),
        household.member_id,
        now,
    )
    .await
    .expect("transfer");
    drop(conn);

    let changes = payloads(&db.pool, "role_changed").await;
    assert_eq!(changes.len(), 2, "{changes:?}");
    let pairs: Vec<(String, String)> = changes
        .iter()
        .map(|payload| {
            (
                payload["from"].as_str().expect("from").to_owned(),
                payload["to"].as_str().expect("to").to_owned(),
            )
        })
        .collect();
    assert!(pairs.contains(&("owner".to_owned(), "member".to_owned())));
    assert!(pairs.contains(&("member".to_owned(), "owner".to_owned())));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_secret_or_user_content_reaches_audit_payloads() {
    const BAD_SECRET: &str = "wrong-Bearer-SECRET-value";
    const UNKNOWN_TOKEN: &str = "tlq_Bearer-SECRET-unknown";
    const TEXT: &str = "Купить лекарство";
    const LLM_RESPONSE: &str = "LLM-RESPONSE-SECRET-NAME";

    let db = isolated_db().await;
    let household: TestHousehold = create_household(&db.pool).await;
    let issued = admin::create_token(
        &db.pool,
        TenancyMode::Single,
        Some(household.household_id),
        "No secrets",
        &["ha:read".to_owned(), "capture:write".to_owned()],
        None,
        Utc::now(),
    )
    .await
    .expect("token");

    // Webhook: неверный секрет.
    let app = app_with_webhook(&db.pool, Audit::new(db.pool.clone()), 10_000);
    let _ = post_webhook(&app, Some(BAD_SECRET), &update_payload(900).await).await;
    let _ = post_webhook(&app, None, &update_payload(901).await).await;

    // API: неизвестный Bearer.
    let app = support::app(&db.pool);
    let _ = support::request(
        &app,
        Method::GET,
        "/api/v1/household",
        Some(UNKNOWN_TOKEN),
        None,
    )
    .await;

    // Capture с ответом модели, который не прошёл валидацию.
    let ai: Arc<dyn AiTask> = Arc::new(StubAi {
        answer: json!({"items": LLM_RESPONSE}),
    });
    let app = build_router(
        AppState::new(db.pool.clone(), Audit::new(db.pool.clone())).with_capture(
            CaptureState::new(
                capture_config(),
                Some(ai),
                Arc::new(CircuitBreaker::default()),
                200,
            ),
        ),
    );
    let _ = support::request(
        &app,
        Method::POST,
        "/api/v1/capture",
        Some(&issued.token),
        Some(json!({"text": TEXT, "source": "api", "mode": "auto"})),
    )
    .await;

    let payloads: Vec<String> = sqlx::query_scalar("SELECT payload::text FROM audit_events")
        .fetch_all(&db.pool)
        .await
        .expect("payloads");
    assert!(!payloads.is_empty(), "the scenario must write audit events");
    let joined = payloads.join("\n");
    let database_url = support::database_url().await;
    let database_password = database_url
        .split("//")
        .nth(1)
        .and_then(|rest| rest.split('@').next())
        .and_then(|userinfo| userinfo.split_once(':'))
        .map(|(_, password)| password.to_owned())
        .expect("database password");

    for secret in [
        SECRET,
        BAD_SECRET,
        UNKNOWN_TOKEN,
        issued.token.as_str(),
        hash_token(&issued.token).as_str(),
        database_url.as_str(),
        database_password.as_str(),
        TEXT,
        LLM_RESPONSE,
    ] {
        assert!(
            !joined.contains(secret),
            "audit payloads leaked {secret}:\n{joined}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_broken_audit_does_not_change_the_response() {
    use sqlx::postgres::PgPoolOptions;

    let db = isolated_db().await;
    // Пул, который не может открыть соединение: запись аудита завершится
    // ошибкой, но отказ запроса должен остаться прежним.
    let dead = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(StdDuration::from_millis(200))
        .connect_lazy("postgres://tasklerq:tasklerq@127.0.0.1:1/tasklerq")
        .expect("lazy pool");

    let app = build_router(
        AppState::new(db.pool.clone(), Audit::new(dead.clone())).with_telegram(
            Ingress::with_rate_limit(&SecretString::new(SECRET), Arc::new(Notify::new()), 120, 1),
        ),
    )
    .layer(MockConnectInfo(peer()));

    assert_eq!(
        post_webhook(
            &app,
            Some("wrong-Bearer-SECRET-value"),
            &update_payload(990).await
        )
        .await,
        StatusCode::UNAUTHORIZED
    );

    let app = support::app(&db.pool);
    let (status, body) = support::request(&app, Method::GET, "/api/v1/household", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["code"], "unauthorized");

    // API с отказавшим журналом.
    let app = build_router(AppState::new(db.pool.clone(), Audit::new(dead)));
    let (status, body) = support::request(&app, Method::GET, "/api/v1/household", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["code"], "unauthorized");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hung_audit_database_does_not_delay_auth_rejections() {
    let db = isolated_db().await;
    // Держим таблицу аудита под блокировкой: запись не сможет завершиться,
    // пока тест не отпустит её. Ответ должен прийти по таймауту аудита.
    let mut lock = db.pool.begin().await.expect("lock transaction");
    sqlx::query("LOCK TABLE audit_events IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *lock)
        .await
        .expect("lock audit_events");

    let app = app_with_webhook(&db.pool, Audit::new(db.pool.clone()), 10_000);

    let started = std::time::Instant::now();
    assert_eq!(
        post_webhook(
            &app,
            Some("wrong-Bearer-SECRET-value"),
            &update_payload(991).await
        )
        .await,
        StatusCode::UNAUTHORIZED
    );
    let webhook_elapsed = started.elapsed();
    assert!(
        webhook_elapsed < StdDuration::from_secs(1),
        "webhook 401 waited for audit: {webhook_elapsed:?}"
    );

    let started = std::time::Instant::now();
    let (status, _) = support::request(&app, Method::GET, "/api/v1/household", None, None).await;
    let api_elapsed = started.elapsed();
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(
        api_elapsed < StdDuration::from_secs(1),
        "API 401 waited for audit: {api_elapsed:?}"
    );

    lock.rollback().await.expect("release lock");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_webhook_without_the_secret_header_is_audited_as_missing_secret() {
    let db = isolated_db().await;
    let app = app_with_webhook(&db.pool, Audit::new(db.pool.clone()), 10_000);

    assert_eq!(
        post_webhook(&app, None, &update_payload(992).await).await,
        StatusCode::UNAUTHORIZED
    );

    let events = payloads(&db.pool, "auth_failed").await;
    let event = events
        .iter()
        .find(|payload| payload["source"] == "webhook")
        .expect("webhook auth failure event");
    assert_eq!(event["reason"], "missing_secret", "{event}");
}
