//! Переключение поколений Telegram без перезапуска: подключение бота,
//! перевыпуск секрета, выключение, `503` во время смены, `409` в polling и
//! аварийный `abort`.

mod support;

use std::time::Duration;

use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Request, StatusCode};
use chrono::{Duration as ChronoDuration, Utc};
use serde_json::{Value, json};
use sqlx::PgPool;
use tasklerq::app::{Application, build_application};
use tasklerq::config::{Config, SecretString};
use tasklerq::domain::models::RecurrenceFreq;
use tasklerq::jobs::domain::DomainTicker;
use tasklerq::settings::{self, crypto::SecretCipher};
use tasklerq::storage::repositories::items;
use tasklerq::telegram::outbox::{self, Outgoing};
use tokio::sync::watch;
use tower::ServiceExt;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

const BOT_TOKEN: &str = "123:SECRET-db-bot-token";
const DATABASE_URL: &str = "postgres://tasklerq:secret@127.0.0.1:5432/tasklerq";
const SECRET_KEY: &str = "AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=";

fn cipher(config: &Config) -> SecretCipher {
    SecretCipher::from_config(config)
}

fn config(server: &MockServer, extra: &[(&str, &str)]) -> Config {
    let mut vars = vec![
        ("ENV", "development".to_owned()),
        ("DATABASE_URL", DATABASE_URL.to_owned()),
        ("TELEGRAM_API_BASE_URL", server.uri()),
        ("TASKLERQ_SECRET_KEY", SECRET_KEY.to_owned()),
        ("LLM_CAPTURE_ENABLED", "false".to_owned()),
    ];
    vars.extend(extra.iter().map(|(key, value)| (*key, (*value).to_owned())));
    Config::from_map(vars).expect("test config is valid")
}

async fn write_value(pool: &PgPool, key: &str, value: Value) {
    let mut conn = pool.acquire().await.expect("connection");
    settings::write_value(&mut conn, key, value)
        .await
        .expect("setting");
}

async fn write_secret(pool: &PgPool, cipher: &SecretCipher, key: &str, value: &str) {
    let mut conn = pool.acquire().await.expect("connection");
    settings::write_secret(&mut conn, key, &SecretString::new(value), cipher)
        .await
        .expect("secret setting");
}

async fn write_polling(pool: &PgPool, cipher: &SecretCipher) {
    write_value(pool, "telegram.mode", json!("polling")).await;
    write_value(pool, "telegram.bot_username", json!("switch_bot")).await;
    write_secret(pool, cipher, "telegram.bot_token", BOT_TOKEN).await;
}

async fn write_webhook(pool: &PgPool, cipher: &SecretCipher, server: &MockServer, secret: &str) {
    write_value(pool, "telegram.mode", json!("webhook")).await;
    write_value(pool, "telegram.bot_username", json!("switch_bot")).await;
    write_value(
        pool,
        "telegram.webhook_url_override",
        json!(format!("{}/webhooks/telegram", server.uri())),
    )
    .await;
    write_secret(pool, cipher, "telegram.bot_token", BOT_TOKEN).await;
    write_secret(pool, cipher, "telegram.webhook_secret", secret).await;
}

fn text_update(update_id: i64, chat_id: i64, text: &str) -> Value {
    json!({
        "update_id": update_id,
        "message": {
            "message_id": update_id,
            "date": 1_700_000_000,
            "chat": {"id": chat_id, "type": "private", "first_name": "Alice"},
            "from": {"id": chat_id, "is_bot": false, "first_name": "Alice"},
            "text": text
        }
    })
}

fn bot_path(method_name: &str) -> String {
    format!(r"^/bot[^/]+/{method_name}$")
}

async fn mount_bot_answers(server: &MockServer) {
    for method_name in [
        "sendMessage",
        "sendRichMessage",
        "editMessageText",
        "deleteMessage",
        "answerCallbackQuery",
        "setWebhook",
        "deleteWebhook",
        "getMe",
    ] {
        Mock::given(method("POST"))
            .and(path_regex(bot_path(method_name)))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true,
                "result": {"message_id": 1}
            })))
            .mount(server)
            .await;
    }
}

async fn mount_empty_updates(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path_regex(bot_path("getUpdates")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(200))
                .set_body_json(json!({"ok": true, "result": []})),
        )
        .mount(server)
        .await;
}

async fn received(server: &MockServer, method_name: &str) -> Vec<wiremock::Request> {
    server
        .received_requests()
        .await
        .expect("requests")
        .into_iter()
        .filter(|request| request.url.path().ends_with(&format!("/{method_name}")))
        .collect()
}

async fn wait_for_requests(server: &MockServer, method_name: &str, count: usize) {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if received(server, method_name).await.len() >= count {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{method_name} was not called {count} times"));
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

fn webhook_router(application: &Application) -> axum::Router {
    application
        .router
        .clone()
        .layer(MockConnectInfo(std::net::SocketAddr::from((
            [172, 18, 0, 1],
            4040,
        ))))
}

async fn post_webhook(
    app: &axum::Router,
    secret: &str,
    update: Value,
) -> (StatusCode, Option<String>) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/webhooks/telegram")
                .header("x-telegram-bot-api-secret-token", secret)
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&update).expect("json")))
                .expect("request"),
        )
        .await
        .expect("response");
    let status = response.status();
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    (status, retry_after)
}

async fn ready_status(app: &axum::Router) -> StatusCode {
    app.clone()
        .oneshot(
            Request::builder()
                .uri("/health/ready")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response")
        .status()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bot_enabled_by_reload_handles_start_without_restart() {
    let db = support::isolated_db().await;
    let server = MockServer::start().await;
    mount_bot_answers(&server).await;
    Mock::given(method("POST"))
        .and(path_regex(bot_path("getUpdates")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "result": [text_update(70_001, 555_001, "/start")]
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    mount_empty_updates(&server).await;

    let config = config(&server, &[]);
    let cipher = cipher(&config);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let application = build_application(&config, db.pool.clone(), shutdown_rx)
        .await
        .expect("application");
    assert_eq!(
        application.telegram_mode,
        tasklerq::config::TelegramMode::Disabled
    );

    write_polling(&db.pool, &cipher).await;
    let started = application
        .telegram_supervisor
        .reload()
        .await
        .expect("reload");
    assert!(started, "polling must start a generation");

    wait_for_requests(&server, "getUpdates", 1).await;
    wait_for_requests(&server, "sendRichMessage", 1).await;
    assert!(
        !received(&server, "sendRichMessage").await.is_empty(),
        "the /start reply must reach the mock Bot API without a restart"
    );

    stop(application, shutdown_tx).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn readiness_checks_queue_lag_only_while_a_generation_is_active() {
    let db = support::isolated_db().await;
    let server = MockServer::start().await;
    mount_bot_answers(&server).await;

    let config = config(&server, &[]);
    let cipher = cipher(&config);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let application = build_application(&config, db.pool.clone(), shutdown_rx)
        .await
        .expect("application");

    // Старое готовое обновление не должно разбираться воркером: блокируем
    // строку, чтобы `claim_next` пропустил её через `SKIP LOCKED`, а
    // `oldest_ready` продолжал видеть.
    let update_id = 70_100_i64;
    sqlx::query(
        "INSERT INTO telegram_updates (update_id, payload, available_at, received_at)
         VALUES ($1, '{}'::jsonb, now() - interval '10 minutes',
                 now() - interval '10 minutes')",
    )
    .bind(update_id)
    .execute(&db.pool)
    .await
    .expect("stuck update");
    let mut lock = db.pool.begin().await.expect("lock transaction");
    let locked: Option<i64> = sqlx::query_scalar(
        "SELECT update_id FROM telegram_updates WHERE update_id = $1 FOR UPDATE",
    )
    .bind(update_id)
    .fetch_optional(&mut *lock)
    .await
    .expect("lock stuck update");
    assert_eq!(locked, Some(update_id));

    assert_eq!(
        ready_status(&application.router).await,
        StatusCode::OK,
        "a disabled bot must not check the update queue"
    );

    write_webhook(&db.pool, &cipher, &server, "secret-lag").await;
    assert!(
        application
            .telegram_supervisor
            .reload()
            .await
            .expect("webhook reload")
    );
    assert_eq!(
        ready_status(&application.router).await,
        StatusCode::SERVICE_UNAVAILABLE,
        "an active generation with a stuck update is not ready"
    );

    write_value(&db.pool, "telegram.mode", json!("disabled")).await;
    assert!(
        !application
            .telegram_supervisor
            .reload()
            .await
            .expect("disable reload"),
        "a disabled configuration has no generation"
    );
    assert_eq!(
        ready_status(&application.router).await,
        StatusCode::OK,
        "disabling the bot must stop the queue lag check even with a pending update"
    );

    lock.rollback().await.expect("release stuck update");
    stop(application, shutdown_tx).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn switching_from_webhook_to_polling_removes_the_webhook_first() {
    let db = support::isolated_db().await;
    let server = MockServer::start().await;
    mount_bot_answers(&server).await;
    mount_empty_updates(&server).await;

    let config = config(&server, &[]);
    let cipher = cipher(&config);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let application = build_application(&config, db.pool.clone(), shutdown_rx)
        .await
        .expect("application");

    write_webhook(&db.pool, &cipher, &server, "secret-webhook").await;
    assert!(
        application
            .telegram_supervisor
            .reload()
            .await
            .expect("webhook reload")
    );
    wait_for_requests(&server, "setWebhook", 1).await;

    write_value(&db.pool, "telegram.mode", json!("polling")).await;
    assert!(
        application
            .telegram_supervisor
            .reload()
            .await
            .expect("polling reload")
    );
    wait_for_requests(&server, "deleteWebhook", 1).await;
    wait_for_requests(&server, "getUpdates", 1).await;

    let requests = server.received_requests().await.expect("requests");
    let delete = requests
        .iter()
        .position(|request| request.url.path().ends_with("/deleteWebhook"))
        .expect("deleteWebhook must be called");
    let updates = requests
        .iter()
        .position(|request| request.url.path().ends_with("/getUpdates"))
        .expect("getUpdates must be called");
    assert!(
        delete < updates,
        "the webhook must be removed before long polling starts"
    );

    stop(application, shutdown_tx).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_secret_reissue_does_not_deliver_a_started_message_twice() {
    let db = support::isolated_db().await;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path_regex(bot_path("sendMessage")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(2))
                .set_body_json(json!({"ok": true, "result": {"message_id": 4242}})),
        )
        .mount(&server)
        .await;
    mount_bot_answers(&server).await;

    let config = config(&server, &[]);
    let cipher = cipher(&config);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let application = build_application(&config, db.pool.clone(), shutdown_rx)
        .await
        .expect("application");

    write_webhook(&db.pool, &cipher, &server, "secret-old").await;
    let mut conn = db.pool.acquire().await.expect("connection");
    let marker = "slow-delivery-marker";
    outbox::enqueue(
        &mut conn,
        555_002,
        &Outgoing::Message {
            html: marker.to_owned(),
            rich: false,
            delete_after: None,
        },
        None,
        Utc::now(),
    )
    .await
    .expect("outbox row");
    drop(conn);

    assert!(
        application
            .telegram_supervisor
            .reload()
            .await
            .expect("first reload")
    );
    wait_for_requests(&server, "sendMessage", 1).await;

    // Секрет меняется, пока отправка ещё ждёт ответа медленного API.
    write_secret(&db.pool, &cipher, "telegram.webhook_secret", "secret-new").await;
    assert!(
        application
            .telegram_supervisor
            .reload()
            .await
            .expect("second reload")
    );

    let sends: Vec<_> = received(&server, "sendMessage")
        .await
        .into_iter()
        .filter(|request| String::from_utf8_lossy(&request.body).contains(marker))
        .collect();
    assert_eq!(
        sends.len(),
        1,
        "a delivery started before the switch must finish exactly once"
    );
    let (status, message_id): (String, Option<i64>) =
        sqlx::query_as("SELECT status::text, message_id FROM outbox WHERE chat_id = $1")
            .bind(555_002)
            .fetch_one(&db.pool)
            .await
            .expect("outbox row");
    assert_eq!(status, "sent");
    assert_eq!(message_id, Some(4242));

    let app = webhook_router(&application);
    let (status, _) =
        post_webhook(&app, "secret-new", text_update(70_002, 555_002, "/start")).await;
    assert_eq!(status, StatusCode::OK, "the new secret must be accepted");
    let (status, _) =
        post_webhook(&app, "secret-old", text_update(70_003, 555_002, "/start")).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "the old secret must be rejected after the switch"
    );

    stop(application, shutdown_tx).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disabling_telegram_keeps_capture_and_recurring_events_working() {
    let db = support::isolated_db().await;
    let household = support::create_household(&db.pool).await;
    let server = MockServer::start().await;
    mount_bot_answers(&server).await;
    let completion = json!({
        "model": "test/model",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": support::llm::v4::response(vec![
                    support::llm::v4::shopping("молоко")
                ]).to_string()
            },
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 10, "completion_tokens": 5}
    });
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(completion))
        .mount(&server)
        .await;

    let llm_url = format!("{}/v1", server.uri());
    let config = config(
        &server,
        &[
            ("LLM_CAPTURE_ENABLED", "true"),
            ("LLM_PRIMARY_URL", &llm_url),
            ("LLM_PRIMARY_MODEL", "test/model"),
        ],
    );
    let cipher = cipher(&config);
    write_webhook(&db.pool, &cipher, &server, "secret-enabled").await;

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let application = build_application(&config, db.pool.clone(), shutdown_rx)
        .await
        .expect("application");
    assert!(application.telegram_mode.is_enabled());

    write_value(&db.pool, "telegram.mode", json!("disabled")).await;
    assert!(
        !application
            .telegram_supervisor
            .reload()
            .await
            .expect("disable reload"),
        "a disabled configuration has no generation"
    );

    let app = webhook_router(&application);
    let (status, _) = post_webhook(
        &app,
        "secret-enabled",
        text_update(70_010, 555_003, "/start"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "a disabled bot must not accept webhook updates"
    );

    let token = support::issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let (status, body) = support::request(
        &application.router,
        axum::http::Method::POST,
        "/api/v1/capture",
        Some(&token),
        Some(json!({"text": "молоко", "source": "api", "mode": "draft"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Доменный тикер не входит в поколение Telegram: с выключенным ботом
    // повторяющееся событие всё равно переходит.
    let start = Utc::now() - ChronoDuration::days(2);
    let mut item = support::new_item(household.events_list_id, "Ежедневная зарядка");
    item.due_at = Some(start);
    item.end_at = Some(start + ChronoDuration::hours(1));
    item.recurrence = Some(support::recurrence(RecurrenceFreq::Daily, Vec::new()));
    let event = support::create_item(
        &db.pool,
        &household,
        household.events_list_id,
        &household.owner(),
        item,
    )
    .await;
    DomainTicker::new(db.pool.clone())
        .tick(Utc::now())
        .await
        .expect("domain tick");
    let mut conn = db.pool.acquire().await.expect("connection");
    let current = items::get(&mut conn, household.household_id, event.id)
        .await
        .expect("event");
    assert!(
        current.due_at > Some(start),
        "a recurring event must still advance without a Telegram generation"
    );

    stop(application, shutdown_tx).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn webhook_answers_503_while_setwebhook_of_the_new_generation_is_pending() {
    let db = support::isolated_db().await;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path_regex(bot_path("setWebhook")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(1))
                .set_body_json(json!({"ok": true, "result": true})),
        )
        .mount(&server)
        .await;
    mount_bot_answers(&server).await;

    let config = config(&server, &[]);
    let cipher = cipher(&config);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let application = build_application(&config, db.pool.clone(), shutdown_rx)
        .await
        .expect("application");
    write_webhook(&db.pool, &cipher, &server, "secret-switch").await;

    let supervisor = application.telegram_supervisor.clone();
    let switching = tokio::spawn(async move { supervisor.reload().await });
    wait_for_requests(&server, "setWebhook", 1).await;

    let app = webhook_router(&application);
    let (status, retry_after) = post_webhook(
        &app,
        "secret-switch",
        text_update(70_020, 555_004, "/start"),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(retry_after.as_deref(), Some("5"));

    assert!(
        switching.await.expect("reload task").expect("reload"),
        "the new webhook generation must start"
    );
    let (status, _) = post_webhook(
        &app,
        "secret-switch",
        text_update(70_021, 555_004, "/start"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    stop(application, shutdown_tx).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn polling_survives_conflict_and_still_handles_the_update() {
    let db = support::isolated_db().await;
    let server = MockServer::start().await;
    mount_bot_answers(&server).await;
    Mock::given(method("POST"))
        .and(path_regex(bot_path("getUpdates")))
        .respond_with(ResponseTemplate::new(409).set_body_json(json!({
            "ok": false,
            "error_code": 409,
            "description": "Conflict: terminated by other getUpdates request"
        })))
        .up_to_n_times(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path_regex(bot_path("getUpdates")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "result": [text_update(70_030, 555_005, "/start")]
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    mount_empty_updates(&server).await;

    let config = config(&server, &[]);
    let cipher = cipher(&config);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let application = build_application(&config, db.pool.clone(), shutdown_rx)
        .await
        .expect("application");

    write_polling(&db.pool, &cipher).await;
    assert!(
        application
            .telegram_supervisor
            .reload()
            .await
            .expect("reload")
    );

    wait_for_requests(&server, "getUpdates", 3).await;
    wait_for_requests(&server, "sendRichMessage", 1).await;
    assert!(
        !received(&server, "sendMessage").await.is_empty()
            || !received(&server, "sendRichMessage").await.is_empty(),
        "polling must not stop after 409 and must process the next update"
    );

    stop(application, shutdown_tx).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_generation_stuck_in_delivery_is_aborted_and_replaced() {
    let db = support::isolated_db().await;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path_regex(bot_path("sendMessage")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(2))
                .set_body_json(json!({"ok": true, "result": {"message_id": 9_001}})),
        )
        .mount(&server)
        .await;
    mount_bot_answers(&server).await;

    let config = config(&server, &[]);
    let cipher = cipher(&config);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let application = build_application(&config, db.pool.clone(), shutdown_rx)
        .await
        .expect("application");

    write_webhook(&db.pool, &cipher, &server, "secret-abort").await;
    let mut conn = db.pool.acquire().await.expect("connection");
    outbox::enqueue(
        &mut conn,
        555_006,
        &Outgoing::Message {
            html: "stuck-message".to_owned(),
            rich: false,
            delete_after: None,
        },
        None,
        Utc::now(),
    )
    .await
    .expect("outbox row");
    drop(conn);

    assert!(
        application
            .telegram_supervisor
            .reload()
            .await
            .expect("first reload")
    );
    wait_for_requests(&server, "sendMessage", 1).await;
    application
        .telegram_supervisor
        .set_stop_timeout(Duration::from_millis(100));

    write_secret(
        &db.pool,
        &cipher,
        "telegram.webhook_secret",
        "secret-after-abort",
    )
    .await;
    let started = tokio::time::timeout(
        Duration::from_secs(5),
        application.telegram_supervisor.reload(),
    )
    .await
    .expect("a stuck generation must be aborted, not awaited forever")
    .expect("reload");
    assert!(started, "the new generation must start after the abort");

    let app = webhook_router(&application);
    let (status, _) = post_webhook(
        &app,
        "secret-after-abort",
        text_update(70_040, 555_006, "/start"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the replacement generation must accept the new secret"
    );

    stop(application, shutdown_tx).await;
}
