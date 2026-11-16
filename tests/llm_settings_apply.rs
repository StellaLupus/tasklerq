//! Применение настроек LLM без перезапуска: снимок, перенос предохранителей,
//! квота и откат невалидной группы.
//!
//! Каждый тест получает отдельную базу: ключи настроек фиксированы, и общая
//! таблица `settings` иначе смешивала бы строки параллельных тестов. Слот
//! подменяется HTTP-сервером `wiremock`, как в `tests/settings.rs`.

mod support;

use std::sync::Arc;
use std::time::Duration;

use axum::http::{Method, StatusCode};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use chrono::Utc;
use serde_json::json;
use tasklerq::app::{Application, build_application};
use tasklerq::config::{Config, TenancyMode};
use tasklerq::llm::{LlmChange, LlmSettings, LlmSnapshot};
use tasklerq::security_audit::Audit;
use tasklerq::telegram::bot::Bot;
use tasklerq::telegram::capture_worker::{CaptureWorker, Pass};
use tokio::sync::{Notify, watch};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use support::telegram::{FakeTelegram, Harness};

/// Служба без слотов: LLM_* не заданы, таблица `settings` пуста.
fn config() -> Config {
    Config::from_map([
        ("ENV", "development".to_owned()),
        ("PUBLIC_BASE_URL", "http://127.0.0.1:8080".to_owned()),
        (
            "DATABASE_URL",
            "postgres://tasklerq:secret@127.0.0.1:5432/tasklerq".to_owned(),
        ),
        ("TELEGRAM_MODE", "disabled".to_owned()),
        ("CAPTURE_VOICE_FALLBACK_RAW", "false".to_owned()),
        ("TASKLERQ_SECRET_KEY", BASE64.encode([7u8; 32])),
    ])
    .expect("test config should be valid")
}

async fn start(db: &support::TestDb) -> (Application, watch::Sender<bool>) {
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let application = build_application(&config(), db.pool.clone(), shutdown_rx)
        .await
        .expect("application");
    (application, shutdown_tx)
}

async fn stop(application: Application, shutdown: watch::Sender<bool>) {
    shutdown.send(true).expect("shutdown signal");
    for worker in application.workers {
        tokio::time::timeout(Duration::from_secs(5), worker)
            .await
            .expect("worker must stop on shutdown")
            .expect("worker task must join");
    }
}

/// Подменный LLM: всегда отдаёт валидную покупку с заданным названием.
async fn mount_answer(server: &MockServer, title: &str, delay: Duration) {
    let answer = support::llm::v4::response(vec![support::llm::v4::shopping(title)]);
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({
                    "model": "mock/model",
                    "choices": [{"message": {"content": serde_json::to_string(&answer).expect("json")}}],
                }))
                .set_delay(delay),
        )
        .mount(server)
        .await;
}

/// Применить основной слот и включить разбор.
async fn apply_slot(settings: &LlmSettings, server: &MockServer, model: &str) -> Arc<LlmSnapshot> {
    settings
        .apply(&[
            LlmChange::set("llm.primary.url", json!(format!("{}/v1", server.uri()))),
            LlmChange::set("llm.primary.model", json!(model)),
            LlmChange::set("llm.enabled", json!(true)),
        ])
        .await
        .expect("slot is applied")
}

fn models(requests: &[Request]) -> Vec<String> {
    requests
        .iter()
        .map(|request| {
            serde_json::from_slice::<serde_json::Value>(&request.body)
                .expect("request body is json")["model"]
                .as_str()
                .expect("model in request")
                .to_owned()
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_applied_slot_serves_the_next_http_capture_without_a_restart() {
    let db = support::isolated_db().await;
    let server = MockServer::start().await;
    mount_answer(&server, "Молоко", Duration::ZERO).await;
    let (application, shutdown) = start(&db).await;
    assert!(
        application.llm_settings.handle().load().chain().is_none(),
        "the service starts without slots"
    );

    let snapshot = apply_slot(&application.llm_settings, &server, "settings/model").await;
    assert!(snapshot.chain().is_some());
    assert!(snapshot.version() > 1);

    let household = support::create_household(&db.pool).await;
    let token = support::issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let (status, body) = support::request(
        &application.router,
        Method::POST,
        "/api/v1/capture",
        Some(&token),
        Some(json!({"text": "купить молоко", "source": "api", "mode": "auto"})),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "applied", "{body}");
    assert_eq!(
        body["fallback"], false,
        "the answer must come from the applied slot, not from the raw-text fallback: {body}"
    );
    assert_eq!(body["items"][0]["title"], "Молоко", "{body}");
    assert_eq!(
        server.received_requests().await.expect("requests").len(),
        1,
        "the slot applied from settings must receive the capture request"
    );

    stop(application, shutdown).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_applied_slot_serves_the_next_bot_message_without_a_restart() {
    let db = support::isolated_db().await;
    let server = MockServer::start().await;
    mount_answer(&server, "Хлеб", Duration::ZERO).await;
    let (application, shutdown) = start(&db).await;
    let llm = application.llm_settings.handle().clone();
    assert!(
        llm.load().chain().is_none(),
        "the service starts without slots"
    );

    apply_slot(&application.llm_settings, &server, "settings/model").await;

    let chat = support::next_telegram_id();
    let handler = Bot::new("tasklerqbot")
        .with_llm(llm.clone())
        .with_tenancy(TenancyMode::Multi);
    let mut bot = Harness::with_handler(
        &db.pool,
        chat,
        Arc::new(handler.clone()),
        FakeTelegram::new(),
    );
    bot.text("/start").await;
    bot.text("Семья").await;
    bot.press("Москва").await;
    bot.press("меню").await;

    let worker = CaptureWorker::new(
        db.pool.clone(),
        llm.clone(),
        handler,
        Arc::new(Notify::new()),
        Audit::new(db.pool.clone()),
    )
    .only_chat(chat);

    bot.text("купить хлеб").await;
    let jobs: i64 = sqlx::query_scalar("SELECT count(*) FROM capture_jobs WHERE chat_id = $1")
        .bind(chat)
        .fetch_one(&db.pool)
        .await
        .expect("count jobs");
    assert_eq!(
        jobs, 1,
        "the bot must queue a parse job after the slot appears"
    );

    assert_eq!(
        worker.run_once(bot.now).await.expect("worker pass"),
        Pass::Drafted
    );
    bot.deliver().await;
    bot.assert_screen_contains("Хлеб");
    assert_eq!(
        server.received_requests().await.expect("requests").len(),
        1,
        "the applied slot must parse the queued bot message"
    );

    stop(application, shutdown).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_parse_started_before_a_model_change_finishes_with_the_old_model() {
    let db = support::isolated_db().await;
    let server = MockServer::start().await;
    mount_answer(&server, "Молоко", Duration::from_millis(700)).await;
    let (application, shutdown) = start(&db).await;
    apply_slot(&application.llm_settings, &server, "old/model").await;

    let household = support::create_household(&db.pool).await;
    let token = support::issue_token(&db.pool, household.household_id, &["capture:write"]).await;

    let router = application.router.clone();
    let first_token = token.clone();
    let first = tokio::spawn(async move {
        support::request(
            &router,
            Method::POST,
            "/api/v1/capture",
            Some(&first_token),
            Some(json!({"text": "купить молоко", "source": "api", "mode": "auto"})),
        )
        .await
    });

    // Ждём, пока начатый разбор действительно уйдёт в подменный сервер: только
    // тогда смена модели проверяет уже начатый разбор, а не гонку постановки.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while server
        .received_requests()
        .await
        .expect("requests")
        .is_empty()
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the first capture request never reached the slot"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    application
        .llm_settings
        .apply(&[LlmChange::set("llm.primary.model", json!("new/model"))])
        .await
        .expect("model is changed");

    let (status, body) = first.await.expect("first capture joins");
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"][0]["title"], "Молоко", "{body}");

    let (status, body) = support::request(
        &application.router,
        Method::POST,
        "/api/v1/capture",
        Some(&token),
        Some(json!({"text": "купить молоко", "source": "api", "mode": "auto"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"][0]["title"], "Молоко", "{body}");

    assert_eq!(
        models(&server.received_requests().await.expect("requests")),
        vec!["old/model".to_owned(), "new/model".to_owned()],
        "the started parse keeps the old chain, the next one takes the new model"
    );

    stop(application, shutdown).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_group_applies_a_secret_and_a_delete_for_the_next_parse() {
    let db = support::isolated_db().await;
    let server = MockServer::start().await;
    mount_answer(&server, "Молоко", Duration::ZERO).await;
    let (application, shutdown) = start(&db).await;

    application
        .llm_settings
        .apply(&[
            LlmChange::set("llm.primary.url", json!(format!("{}/v1", server.uri()))),
            LlmChange::set("llm.primary.model", json!("settings/model")),
            LlmChange::secret("llm.primary.token", "settings-token"),
            LlmChange::set("llm.enabled", json!(true)),
        ])
        .await
        .expect("a group with a secret is applied");

    let household = support::create_household(&db.pool).await;
    let token = support::issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let capture = json!({"text": "купить молоко", "source": "api", "mode": "auto"});
    let (status, body) = support::request(
        &application.router,
        Method::POST,
        "/api/v1/capture",
        Some(&token),
        Some(capture.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let requests = server.received_requests().await.expect("requests");
    assert_eq!(
        requests[0]
            .headers
            .get("authorization")
            .map(|value| value.to_str().expect("header")),
        Some("Bearer settings-token"),
        "the saved secret must be used by the next parse"
    );

    application
        .llm_settings
        .apply(&[LlmChange::delete("llm.primary.token")])
        .await
        .expect("the token is deleted");

    let (status, body) = support::request(
        &application.router,
        Method::POST,
        "/api/v1/capture",
        Some(&token),
        Some(capture),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let requests = server.received_requests().await.expect("requests");
    assert!(
        requests[1].headers.get("authorization").is_none(),
        "a deleted token must not be sent by the next parse"
    );

    stop(application, shutdown).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_invalid_slot_rolls_back_and_keeps_the_snapshot() {
    let db = support::isolated_db().await;
    let server = MockServer::start().await;
    mount_answer(&server, "Молоко", Duration::ZERO).await;
    let (application, shutdown) = start(&db).await;
    apply_slot(&application.llm_settings, &server, "settings/model").await;

    let before = application.llm_settings.handle().load();
    let chain_before = before.chain().cloned().expect("configured chain");

    let error = application
        .llm_settings
        .apply(&[LlmChange::set(
            "llm.fallback.url",
            json!("http://127.0.0.1:9/v1"),
        )])
        .await
        .expect_err("a fallback address without a model is rejected");
    let text = error.to_string();
    assert!(
        text.contains("llm.fallback.model"),
        "the error must name the missing field: {text}"
    );

    let after = application.llm_settings.handle().load();
    assert_eq!(
        after.version(),
        before.version(),
        "a rejected save must not replace the snapshot"
    );
    let chain_after = after.chain().cloned().expect("chain is kept");
    assert!(
        Arc::ptr_eq(&chain_before, &chain_after),
        "the active chain must not be rebuilt on a rejected save"
    );

    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM settings WHERE key = $1")
        .bind("llm.fallback.url")
        .fetch_one(&db.pool)
        .await
        .expect("count");
    assert_eq!(rows, 0, "the transaction must roll back the saved address");

    stop(application, shutdown).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_quota_change_allows_a_parse_on_the_same_day() {
    let db = support::isolated_db().await;
    let server = MockServer::start().await;
    mount_answer(&server, "Молоко", Duration::ZERO).await;
    let (application, shutdown) = start(&db).await;
    apply_slot(&application.llm_settings, &server, "settings/model").await;

    let household = support::create_household(&db.pool).await;
    let token = support::issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    // Квота 200 уже израсходована в тот же день.
    sqlx::query(
        "INSERT INTO llm_runs (
             id, household_id, source, mode, prompt_version, status, latency_ms, created_at
         )
         SELECT gen_random_uuid(), $1, 'api', 'auto', 'capture-v4', 'success', 1,
                now() - interval '1 minute'
         FROM generate_series(1, 200)",
    )
    .bind(household.household_id)
    .execute(&db.pool)
    .await
    .expect("quota usage");

    let (status, body) = support::request(
        &application.router,
        Method::POST,
        "/api/v1/capture",
        Some(&token),
        Some(json!({"text": "купить молоко", "source": "api", "mode": "auto"})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "an exhausted quota rejects the parse: {body}"
    );
    assert_eq!(
        server.received_requests().await.expect("requests").len(),
        0,
        "the slot must not be called when the quota is exhausted"
    );

    application
        .llm_settings
        .apply(&[LlmChange::set("llm.daily_quota", json!(300))])
        .await
        .expect("quota is raised");

    let (status, body) = support::request(
        &application.router,
        Method::POST,
        "/api/v1/capture",
        Some(&token),
        Some(json!({"text": "купить молоко", "source": "api", "mode": "auto"})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a raised quota allows the parse on the same day: {body}"
    );
    assert_eq!(body["fallback"], false, "{body}");
    assert_eq!(body["items"][0]["title"], "Молоко", "{body}");
    assert_eq!(server.received_requests().await.expect("requests").len(), 1);

    stop(application, shutdown).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_quota_change_keeps_an_open_breaker_but_a_model_change_closes_it() {
    let db = support::isolated_db().await;
    let server = MockServer::start().await;
    mount_answer(&server, "Молоко", Duration::ZERO).await;
    let (application, shutdown) = start(&db).await;
    apply_slot(&application.llm_settings, &server, "old/model").await;

    let llm = application.llm_settings.handle().clone();
    let chain = llm.load().chain().cloned().expect("configured chain");
    support::llm::open_primary(&chain, Utc::now());

    application
        .llm_settings
        .apply(&[LlmChange::set("llm.daily_quota", json!(300))])
        .await
        .expect("quota is raised");

    let after_quota = llm.load();
    let chain = after_quota.chain().cloned().expect("chain");
    assert!(
        chain.primary().breaker().retry_after(Utc::now()).is_some(),
        "a quota change must keep the open breaker of the same slot"
    );

    let household = support::create_household(&db.pool).await;
    let token = support::issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let (status, body) = support::request(
        &application.router,
        Method::POST,
        "/api/v1/capture",
        Some(&token),
        Some(json!({"text": "купить молоко", "source": "api", "mode": "auto"})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "the open breaker must skip the slot: {body}"
    );
    assert_eq!(
        server.received_requests().await.expect("requests").len(),
        0,
        "an open breaker must not call the slot"
    );

    application
        .llm_settings
        .apply(&[LlmChange::set("llm.primary.model", json!("new/model"))])
        .await
        .expect("model is changed");

    let after_model = llm.load();
    let chain = after_model.chain().cloned().expect("chain");
    assert!(
        chain.primary().breaker().retry_after(Utc::now()).is_none(),
        "a changed slot must start with a closed breaker"
    );

    let (status, body) = support::request(
        &application.router,
        Method::POST,
        "/api/v1/capture",
        Some(&token),
        Some(json!({"text": "купить молоко", "source": "api", "mode": "auto"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"][0]["title"], "Молоко", "{body}");
    let requests = server.received_requests().await.expect("requests");
    assert_eq!(models(&requests), vec!["new/model".to_owned()]);

    stop(application, shutdown).await;
}
