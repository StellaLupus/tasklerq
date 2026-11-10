//! Redaction секретов в конфигурации, ошибках и журналах.
//!
//! Отдельный тестовый бинарник, как `capture_api_logs.rs`: тесты сами ставят
//! подписчика `tracing` и проверяют захваченный вывод по подстроке секрета.

mod support;

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::json;
use support::logs::LogBuffer;
use tasklerq::config::{Config, SecretString};
use tasklerq::home_assistant::{AiTask, GenerateData, HttpAiTask};
use tasklerq::http::router::{AppState, build_router};
use tasklerq::security_audit::Audit;
use tasklerq::telegram::api::{HttpTelegramApi, TelegramApi};
use tasklerq::telegram::webhook::{Ingress, SECRET_HEADER};
use tokio::sync::Notify;
use tower::ServiceExt;

const BOT_TOKEN: &str = "123456:SECRET-bot-token-redaction";
const WEBHOOK_SECRET: &str = "SECRET-webhook-redaction";
const PROVIDED_WEBHOOK_SECRET: &str = "SECRET-provided-webhook-redaction";
const HA_TOKEN: &str = "SECRET-ha-token-redaction";
const PRIMARY_TOKEN: &str = "SECRET-primary-slot-redaction";
const FALLBACK_TOKEN: &str = "SECRET-fallback-slot-redaction";
const DB_PASSWORD: &str = "S3cretPassw0rd";
const API_TOKEN: &str = "tlq_SECRET-bearer-redaction";

/// Все значения, которые не должны появляться в выводе.
const SECRETS: [&str; 8] = [
    BOT_TOKEN,
    WEBHOOK_SECRET,
    PROVIDED_WEBHOOK_SECRET,
    HA_TOKEN,
    PRIMARY_TOKEN,
    FALLBACK_TOKEN,
    DB_PASSWORD,
    API_TOKEN,
];

fn assert_no_secret(rendered: &str, context: &str) {
    for secret in SECRETS {
        assert!(
            !rendered.contains(secret),
            "{context} leaked {secret}:\n{rendered}"
        );
    }
}

fn config_env() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("ENV".to_owned(), "development".to_owned()),
        (
            "PUBLIC_BASE_URL".to_owned(),
            "http://127.0.0.1:8080".to_owned(),
        ),
        (
            "DATABASE_URL".to_owned(),
            format!("postgres://tasklerq:{DB_PASSWORD}@127.0.0.1:5432/tasklerq?sslmode=disable"),
        ),
        ("TELEGRAM_MODE".to_owned(), "webhook".to_owned()),
        ("TELEGRAM_BOT_TOKEN".to_owned(), BOT_TOKEN.to_owned()),
        (
            "TELEGRAM_WEBHOOK_SECRET".to_owned(),
            WEBHOOK_SECRET.to_owned(),
        ),
        (
            "TELEGRAM_WEBHOOK_URL".to_owned(),
            "http://127.0.0.1:8080/webhooks/telegram".to_owned(),
        ),
        ("LLM_CAPTURE_ENABLED".to_owned(), "true".to_owned()),
        (
            "LLM_PRIMARY_URL".to_owned(),
            "https://llm.example.com/v1".to_owned(),
        ),
        (
            "LLM_PRIMARY_MODEL".to_owned(),
            "anthropic/claude-haiku-5.5".to_owned(),
        ),
        ("LLM_PRIMARY_TOKEN".to_owned(), PRIMARY_TOKEN.to_owned()),
        (
            "LLM_FALLBACK_URL".to_owned(),
            "https://llm-backup.example.com/v1".to_owned(),
        ),
        (
            "LLM_FALLBACK_MODEL".to_owned(),
            "google/gemini-3.5-flash".to_owned(),
        ),
        ("LLM_FALLBACK_TOKEN".to_owned(), FALLBACK_TOKEN.to_owned()),
        ("HA_BASE_URL".to_owned(), "http://127.0.0.1:8123".to_owned()),
        ("HA_TOKEN".to_owned(), HA_TOKEN.to_owned()),
        (
            "HA_AI_TASK_ENTITY_ID".to_owned(),
            "ai_task.google_ai_task_2".to_owned(),
        ),
    ])
}

fn config_from(pairs: BTreeMap<String, String>) -> Result<Config, tasklerq::config::ConfigError> {
    Config::from_map(pairs)
}

#[test]
fn config_debug_display_and_startup_log_hide_every_secret() {
    let config = config_from(config_env()).expect("config should be valid");

    let debug = format!("{config:?}");
    assert_no_secret(&debug, "Config Debug");
    assert!(debug.contains("<redacted>"));
    assert_eq!(config.database.url.to_string(), "<redacted>");
    assert_eq!(
        config.telegram.bot_token.as_ref().map(ToString::to_string),
        Some("<redacted>".to_owned())
    );
    assert_eq!(
        config
            .telegram
            .webhook_secret
            .as_ref()
            .map(ToString::to_string),
        Some("<redacted>".to_owned())
    );
    assert_eq!(
        config
            .llm
            .primary
            .as_ref()
            .and_then(|slot| slot.token.as_ref())
            .map(ToString::to_string),
        Some("<redacted>".to_owned())
    );
    assert_eq!(
        config
            .llm
            .fallback
            .as_ref()
            .and_then(|slot| slot.token.as_ref())
            .map(ToString::to_string),
        Some("<redacted>".to_owned())
    );

    let logs = LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_writer(logs.clone())
        .finish();
    tracing::subscriber::with_default(subscriber, || config.log_startup());

    assert_no_secret(&logs.contents(), "startup log");
}

#[test]
fn legacy_home_assistant_variables_are_reported_without_values() {
    let config = config_from(config_env()).expect("config should be valid");
    assert_eq!(
        config.deprecated_env,
        vec!["HA_BASE_URL", "HA_TOKEN", "HA_AI_TASK_ENTITY_ID"]
    );

    let logs = LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_writer(logs.clone())
        .finish();
    tracing::subscriber::with_default(subscriber, || config.log_startup());

    let logs = logs.contents();
    for name in ["HA_BASE_URL", "HA_TOKEN", "HA_AI_TASK_ENTITY_ID"] {
        assert!(
            logs.contains(name),
            "startup log must name the unused variable {name}:\n{logs}"
        );
    }
    assert!(logs.contains("no longer used"));
    assert_no_secret(&logs, "startup log");
}

#[test]
fn config_validation_errors_do_not_echo_secret_values() {
    let cases = [
        (
            "DATABASE_URL",
            format!("postgres://tasklerq:{DB_PASSWORD}@[::1"),
        ),
        (
            "PUBLIC_BASE_URL",
            format!("https://tasklerq:{DB_PASSWORD}@[::1"),
        ),
        (
            "TELEGRAM_WEBHOOK_URL",
            format!("http://tasklerq:{DB_PASSWORD}@[::1"),
        ),
        (
            "LLM_PRIMARY_URL",
            format!("http://tasklerq:{DB_PASSWORD}@[::1"),
        ),
    ];

    for (name, value) in cases {
        let mut vars = config_env();
        vars.insert(name.to_owned(), value);
        let error = config_from(vars).expect_err("invalid URL should be rejected");
        assert_no_secret(&error.to_string(), &format!("{name} validation error"));
    }
}

#[test]
fn telegram_network_error_and_log_hide_the_bot_token() {
    let logs = LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(logs.clone())
        .finish();

    let error = tracing::subscriber::with_default(subscriber, || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let api = HttpTelegramApi::new(SecretString::new(BOT_TOKEN), "http://127.0.0.1:1")
                .expect("client");
            let error = api.send_message(1, "x").await.expect_err("network error");
            tracing::error!(%error, "telegram request failed");
            error
        })
    });

    let rendered = format!("{error} {error:?}");
    assert_no_secret(&rendered, "Telegram error");
    assert_no_secret(&logs.contents(), "Telegram log");
}

#[test]
fn home_assistant_error_and_log_hide_the_token() {
    let logs = LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(logs.clone())
        .finish();
    let structure = json!({});

    let error = tracing::subscriber::with_default(subscriber, || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let ai = HttpAiTask::new(
                "http://127.0.0.1:1",
                SecretString::new(HA_TOKEN),
                "ai_task.google_ai_task_2",
            )
            .expect("client");
            let request = GenerateData {
                task_name: "test",
                instructions: "текст пользователя",
                structure: &structure,
            };
            let error = ai
                .generate_data(&request, Duration::from_secs(2))
                .await
                .expect_err("network error");
            tracing::error!(%error, "home assistant request failed");
            error
        })
    });

    let rendered = format!("{error} {error:?}");
    assert_no_secret(&rendered, "Home Assistant error");
    assert_no_secret(&logs.contents(), "Home Assistant log");
}

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_tasklerq"))
}

#[test]
fn startup_error_from_an_unreachable_database_hides_the_password() {
    let url = format!("postgres://tasklerq:{DB_PASSWORD}@127.0.0.1:1/tasklerq?sslmode=disable");
    let output = bin()
        .env("DATABASE_URL", url)
        .arg("migrate")
        .output()
        .expect("cli should run");

    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).expect("utf8");
    assert!(
        stderr.contains("database connection failed"),
        "the startup error must keep useful context:\n{stderr}"
    );
    assert_no_secret(&stderr, "startup error");
}

#[test]
fn healthcheck_error_hides_credentials_in_the_url() {
    let url = format!("http://tasklerq:{DB_PASSWORD}@127.0.0.1:1/health/ready");
    let output = bin()
        .args(["healthcheck", "--url", &url])
        .output()
        .expect("cli should run");

    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).expect("utf8");
    assert_no_secret(&stderr, "healthcheck error");
}

#[test]
fn healthcheck_error_hides_credentials_when_the_url_cannot_be_parsed() {
    let url = format!("http://tasklerq:{DB_PASSWORD}@[::1");
    let output = bin()
        .args(["healthcheck", "--url", &url])
        .output()
        .expect("cli should run");

    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).expect("utf8");
    assert_no_secret(&stderr, "healthcheck parse error");
}

#[test]
fn admin_cli_errors_hide_the_database_password_and_bot_token() {
    let url = format!("postgres://tasklerq:{DB_PASSWORD}@127.0.0.1:1/tasklerq?sslmode=disable");
    let cases: [&[&str]; 4] = [
        &["admin", "queue", "stats"],
        &["admin", "token", "list"],
        &["admin", "export"],
        &["admin", "webhook", "info"],
    ];

    for args in cases {
        let output = bin()
            .env("DATABASE_URL", &url)
            .env("ENV", "development")
            .env("PUBLIC_BASE_URL", "http://127.0.0.1:8080")
            .env("TELEGRAM_MODE", "polling")
            .env("TELEGRAM_BOT_TOKEN", BOT_TOKEN)
            .env("TELEGRAM_API_BASE_URL", "http://127.0.0.1:1")
            .env("LLM_CAPTURE_ENABLED", "false")
            .args(args)
            .output()
            .expect("cli should run");

        assert!(!output.status.success(), "{args:?}: {output:?}");
        let mut combined = String::from_utf8(output.stdout).expect("utf8");
        combined.push_str(&String::from_utf8(output.stderr).expect("utf8"));
        assert_no_secret(&combined, &format!("admin CLI {args:?}"));
    }
}

fn peer() -> SocketAddr {
    SocketAddr::from(([172, 18, 0, 1], 4040))
}

#[tokio::test]
async fn an_invalid_bearer_token_is_not_reflected_in_the_body_or_log() {
    let db = support::db().await;
    let app = build_router(AppState::new(db.pool.clone(), Audit::new(db.pool.clone())));
    let logs = LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(logs.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let request = Request::builder()
        .method("GET")
        .uri("/api/v1/household")
        .header("authorization", format!("Bearer {API_TOKEN}"))
        .body(Body::empty())
        .expect("request");
    let response = app.oneshot(request).await.expect("response");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let body = String::from_utf8(bytes.to_vec()).expect("utf8");

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_no_secret(&body, "authentication response");
    assert_no_secret(&logs.contents(), "authentication log");
}

#[tokio::test]
async fn a_wrong_webhook_secret_is_not_logged() {
    let db = support::db().await;
    let ingress = Ingress::new(&SecretString::new(WEBHOOK_SECRET), Arc::new(Notify::new()));
    let app = build_router(
        AppState::new(db.pool.clone(), Audit::new(db.pool.clone())).with_telegram(ingress),
    )
    .layer(MockConnectInfo(peer()));
    let logs = LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(logs.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let request = Request::builder()
        .method("POST")
        .uri("/webhooks/telegram")
        .header(SECRET_HEADER, PROVIDED_WEBHOOK_SECRET)
        .body(Body::from(
            serde_json::to_vec(&json!({"update_id": 1})).expect("json"),
        ))
        .expect("request");
    let response = app.oneshot(request).await.expect("response");
    let status = response.status();
    let _ = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_no_secret(&logs.contents(), "webhook log");
}
