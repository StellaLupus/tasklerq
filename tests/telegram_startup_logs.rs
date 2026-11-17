//! Стартовый лог ненастроенной группы Telegram из базы: код причины и ключ.
//!
//! Отдельный тестовый бинарник, как `admin_setup_logs.rs`: подписчик ставится
//! на время теста, а `tracing` кеширует интерес к callsite, поэтому
//! параллельные тесты общего бинарника могли бы отключить его до появления
//! подписчика.

mod support;

use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde_json::json;
use tasklerq::app::build_application;
use tasklerq::config::{Config, SecretKey, SecretString};
use tasklerq::settings::{self, crypto::SecretCipher};
use tokio::sync::watch;

use support::logs::LogBuffer;

const DATABASE_URL: &str = "postgres://tasklerq:secret@127.0.0.1:5432/tasklerq";
const BOT_TOKEN: &str = "123:SECRET-db-bot-token-log";

async fn stop(application: tasklerq::app::Application, shutdown: watch::Sender<bool>) {
    shutdown.send(true).expect("shutdown signal");
    for worker in application.workers {
        tokio::time::timeout(Duration::from_secs(5), worker)
            .await
            .expect("worker must stop on shutdown")
            .expect("worker task must join");
    }
}

#[tokio::test]
async fn an_incomplete_stored_group_warns_with_code_and_key_without_the_token() {
    let db = support::isolated_db().await;
    let secret_key = BASE64.encode([3_u8; 32]);
    let key = SecretKey::parse(&secret_key).expect("32 bytes are a key");
    let cipher = SecretCipher::new(Some(key), None);

    // Режим webhook и токен есть, секрета и адреса webhook нет: группа
    // ненастроена, а бот молча выключен — старт обязан назвать причину.
    let mut conn = db.pool.acquire().await.expect("connection");
    settings::write_value(&mut conn, "telegram.mode", json!("webhook"))
        .await
        .expect("mode");
    settings::write_secret(
        &mut conn,
        "telegram.bot_token",
        &SecretString::new(BOT_TOKEN),
        &cipher,
    )
    .await
    .expect("bot token");
    drop(conn);

    let config = Config::from_map([
        ("ENV", "development"),
        ("DATABASE_URL", DATABASE_URL),
        ("TASKLERQ_SECRET_KEY", secret_key.as_str()),
    ])
    .expect("test config is valid");
    assert!(
        config.telegram.is_none(),
        "the environment must not describe a bot"
    );

    let buffer = LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(buffer.clone())
        .finish();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let application = {
        let _guard = tracing::subscriber::set_default(subscriber);
        build_application(&config, db.pool.clone(), shutdown_rx)
            .await
            .expect("application")
    };

    let captured = buffer.contents();
    assert!(
        captured.contains("telegram bot is not configured"),
        "the startup warning is missing:\n{captured}"
    );
    assert!(
        captured.contains("\"code\":\"incomplete\""),
        "the warning must name the reason code:\n{captured}"
    );
    assert!(
        captured.contains("\"setting\":\"telegram.webhook_secret\""),
        "the warning must name the setting key:\n{captured}"
    );
    assert!(
        !captured.contains(BOT_TOKEN),
        "the warning must not leak the bot token:\n{captured}"
    );

    stop(application, shutdown_tx).await;
}
