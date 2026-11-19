//! Запуск Telegram из базы: бот, настроенный только в консоли, работает после
//! перезапуска; без настроек Telegram выключен, а API и консоль работают.

mod support;

use std::time::Duration;

use axum::http::{Method, StatusCode};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde_json::json;
use tasklerq::app::build_application;
use tasklerq::config::{Config, SecretKey, SecretString, TelegramMode};
use tasklerq::settings::{self, crypto::SecretCipher};
use tokio::sync::watch;
use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

const DATABASE_URL: &str = "postgres://tasklerq:secret@127.0.0.1:5432/tasklerq";
const BOT_TOKEN: &str = "123:SECRET-db-bot-token";

fn cipher() -> (SecretCipher, String) {
    let text = BASE64.encode([1_u8; 32]);
    let key = SecretKey::parse(&text).expect("32 bytes are a key");
    (SecretCipher::new(Some(key), None), text)
}

async fn stop(application: tasklerq::app::Application, shutdown: watch::Sender<bool>) {
    shutdown.send(true).expect("shutdown signal");
    for worker in application.workers {
        tokio::time::timeout(Duration::from_secs(5), worker)
            .await
            .expect("worker must stop on shutdown")
            .expect("worker task must join");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bot_stored_only_in_settings_starts_polling_after_restart() {
    let db = support::isolated_db().await;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path_regex(r"^/bot[^/]+/getUpdates$"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(100))
                .set_body_json(json!({"ok": true, "result": []})),
        )
        .mount(&server)
        .await;

    let (cipher, secret_key) = cipher();
    let mut conn = db.pool.acquire().await.expect("connection");
    settings::write_value(&mut conn, "telegram.mode", json!("polling"))
        .await
        .expect("mode");
    settings::write_value(&mut conn, "telegram.bot_username", json!("db_bot"))
        .await
        .expect("username");
    settings::write_secret(
        &mut conn,
        "telegram.bot_token",
        &SecretString::new(BOT_TOKEN),
        &cipher,
    )
    .await
    .expect("bot token");
    drop(conn);

    // Окружение Telegram не задаёт: бот приходит только из базы. Адрес Bot API
    // — служебный, он применяется и без токена в окружении.
    let api_base = server.uri();
    let config = Config::from_map([
        ("ENV", "development"),
        ("DATABASE_URL", DATABASE_URL),
        ("TELEGRAM_API_BASE_URL", api_base.as_str()),
        ("TASKLERQ_SECRET_KEY", secret_key.as_str()),
    ])
    .expect("test config is valid");
    assert!(
        config.telegram.is_none(),
        "the environment must not describe a bot"
    );

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let application = build_application(&config, db.pool.clone(), shutdown_rx)
        .await
        .expect("application");

    assert_eq!(application.telegram_mode, TelegramMode::Polling);

    let mut polled = false;
    for _ in 0..50 {
        if !server
            .received_requests()
            .await
            .expect("requests")
            .is_empty()
        {
            polled = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        polled,
        "the polling worker must call the Bot API configured in the environment"
    );

    stop(application, shutdown_tx).await;
}

#[tokio::test]
async fn a_database_url_alone_starts_with_telegram_disabled() {
    let db = support::isolated_db().await;
    let config =
        Config::from_map([("DATABASE_URL", DATABASE_URL)]).expect("strict minimal config is valid");
    assert!(config.telegram.is_none());

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let application = build_application(&config, db.pool.clone(), shutdown_rx)
        .await
        .expect("application");

    assert_eq!(application.telegram_mode, TelegramMode::Disabled);
    assert!(
        application.admin_router.is_some(),
        "the console stays available without a bot"
    );
    let (status, body) =
        support::request(&application.router, Method::GET, "/health/live", None, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    stop(application, shutdown_tx).await;
}
