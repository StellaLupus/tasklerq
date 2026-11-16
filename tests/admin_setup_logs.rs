//! Стартовый setup-код консоли: единственная строка `warn` с кодом и портом.
//!
//! Отдельный тестовый бинарник, как `degradation_logs.rs`: подписчик ставится
//! на время теста через `with_default`, а `tracing` кеширует интерес к
//! callsite, поэтому параллельные тесты общего бинарника могли бы отключить
//! его до появления подписчика.

mod support;

use std::net::SocketAddr;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use serde_json::Value;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use support::logs::LogBuffer;
use tasklerq::admin_console::codes::{self, Purpose};
use tasklerq::admin_console::router::{AdminState, build_router as build_admin_router};
use tasklerq::config::Config;
use tasklerq::security_audit::Audit;
use tasklerq::storage::migrations;
use tower::ServiceExt;
use uuid::Uuid;

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
}

fn capture<T>(run: impl FnOnce() -> T) -> (String, T) {
    let buffer = LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(buffer.clone())
        .finish();
    let result = tracing::subscriber::with_default(subscriber, run);
    (buffer.contents(), result)
}

async fn setup_db() -> (PgPool, String) {
    let name = format!("admin_setup_{}", Uuid::now_v7().simple());
    support::create_database(&name).await;
    let url = support::database_url_for(&name).await;
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&url)
        .await
        .expect("isolated database should accept connections");
    migrations::run(&pool)
        .await
        .expect("migrations should apply");
    (pool, url)
}

fn config(database_url: &str, admin_bind_addr: &str) -> Config {
    Config::from_map([
        ("ENV", "development"),
        ("PUBLIC_BASE_URL", "http://127.0.0.1:18080"),
        ("DATABASE_URL", database_url),
        ("TELEGRAM_MODE", "disabled"),
        ("LLM_CAPTURE_ENABLED", "false"),
        ("BIND_ADDR", "127.0.0.1:0"),
        ("ADMIN_BIND_ADDR", admin_bind_addr),
    ])
    .expect("test config should be valid")
}

async fn insert_account(pool: &PgPool) {
    sqlx::query(
        "INSERT INTO admin_accounts (id, username, password_hash)
         VALUES ($1, 'admin', '$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$aGFzaA')",
    )
    .bind(Uuid::now_v7())
    .execute(pool)
    .await
    .expect("administrator account");
}

/// События `warn` о выпущенном setup-коде.
fn setup_code_events(logs: &str) -> Vec<Value> {
    logs.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|event| {
            event["fields"]["message"]
                .as_str()
                .is_some_and(|message| message.contains("setup code issued"))
        })
        .collect()
}

async fn code_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM admin_one_time_codes")
        .fetch_one(pool)
        .await
        .expect("code count")
}

#[test]
fn startup_without_account_logs_one_warning_with_the_code_and_port() {
    let (logs, url) = capture(|| {
        runtime().block_on(async {
            let (pool, url) = setup_db().await;
            let config = config(&url, "127.0.0.1:0");
            codes::announce_setup_code(&pool, &config, Some(18081), chrono::Utc::now()).await;
            url
        })
    });

    let events = setup_code_events(&logs);
    assert_eq!(
        events.len(),
        1,
        "exactly one warning must announce the setup code:\n{logs}"
    );
    let event = &events[0];
    assert_eq!(event["level"], "WARN", "{logs}");
    assert_eq!(event["fields"]["port"], 18081, "the bound port is logged");
    assert!(
        event["fields"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("/admin/setup")),
        "the warning must point to the setup page:\n{logs}"
    );
    let code = event["fields"]["code"]
        .as_str()
        .expect("the warning carries the open code")
        .to_owned();
    assert!(code.len() >= 22, "at least 128 bits: {code}");
    assert_eq!(
        logs.matches(&code).count(),
        1,
        "the code must be printed exactly once:\n{logs}"
    );

    runtime().block_on(async {
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .expect("database connection");
        let mut conn = pool.acquire().await.expect("connection");
        codes::use_code(&mut conn, Purpose::Setup, &code, chrono::Utc::now())
            .await
            .expect("the logged code must be accepted as a setup code");
    });
}

#[test]
fn startup_with_an_account_logs_no_code_and_creates_none() {
    let (logs, url) = capture(|| {
        runtime().block_on(async {
            let (pool, url) = setup_db().await;
            insert_account(&pool).await;
            let config = config(&url, "127.0.0.1:0");
            codes::announce_setup_code(&pool, &config, Some(18081), chrono::Utc::now()).await;
            url
        })
    });

    assert!(
        setup_code_events(&logs).is_empty(),
        "an existing account must not produce a setup code:\n{logs}"
    );
    assert!(!logs.contains("setup code"), "{logs}");

    runtime().block_on(async {
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .expect("database connection");
        assert_eq!(code_count(&pool).await, 0, "no code rows are added");
    });
}

#[test]
fn disabled_console_logs_no_setup_code() {
    let (logs, url) = capture(|| {
        runtime().block_on(async {
            let (pool, url) = setup_db().await;
            let config = config(&url, "disabled");
            codes::announce_setup_code(&pool, &config, Some(18081), chrono::Utc::now()).await;
            url
        })
    });

    assert!(
        setup_code_events(&logs).is_empty(),
        "ADMIN_BIND_ADDR=disabled must not issue a setup code:\n{logs}"
    );

    runtime().block_on(async {
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .expect("database connection");
        assert_eq!(code_count(&pool).await, 0, "no code rows are added");
    });
}

#[test]
fn restart_revokes_the_previous_setup_code() {
    let (logs, url) = capture(|| {
        runtime().block_on(async {
            let (pool, url) = setup_db().await;
            let config = config(&url, "127.0.0.1:0");
            codes::announce_setup_code(&pool, &config, Some(18081), chrono::Utc::now()).await;
            codes::announce_setup_code(&pool, &config, Some(18082), chrono::Utc::now()).await;
            url
        })
    });

    let events = setup_code_events(&logs);
    assert_eq!(events.len(), 2, "every start issues a code:\n{logs}");
    let first = events[0]["fields"]["code"]
        .as_str()
        .expect("first code")
        .to_owned();
    let second = events[1]["fields"]["code"]
        .as_str()
        .expect("second code")
        .to_owned();
    assert_ne!(first, second, "a restart must mint a new code");

    runtime().block_on(async {
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .expect("database connection");
        let mut conn = pool.acquire().await.expect("connection");

        let previous = codes::use_code(&mut conn, Purpose::Setup, &first, chrono::Utc::now()).await;
        assert!(
            previous.is_err(),
            "the previous code must be revoked by the restart"
        );
        codes::use_code(&mut conn, Purpose::Setup, &second, chrono::Utc::now())
            .await
            .expect("the code from the last start must work");

        assert_eq!(
            code_count(&pool).await,
            2,
            "both issues are kept as history"
        );
    });
}

/// Пароль не попадает в журнал ни при настройке, ни при входе.
#[test]
fn the_password_never_appears_in_the_logs() {
    const PASSWORD: &str = "log-secret-password-123";

    let (logs, (setup_status, login_status)) = capture(|| {
        runtime().block_on(async {
            let (pool, _url) = setup_db().await;
            let app = build_admin_router(AdminState::new(
                pool.clone(),
                Vec::new(),
                Audit::new(pool.clone()),
            ));
            let issued = codes::issue(&pool, Purpose::Setup, None, chrono::Utc::now())
                .await
                .expect("a setup code");

            let setup_status = post_form(
                &app,
                "/admin/setup",
                &[
                    ("code", &issued.code),
                    ("username", "admin"),
                    ("password", PASSWORD),
                ],
            )
            .await;
            let login_status = post_form(
                &app,
                "/admin/login",
                &[("username", "admin"), ("password", PASSWORD)],
            )
            .await;
            (setup_status, login_status)
        })
    });

    assert_eq!(setup_status, StatusCode::SEE_OTHER);
    assert_eq!(login_status, StatusCode::SEE_OTHER);
    assert!(
        !logs.contains(PASSWORD),
        "the password must not be logged:\n{logs}"
    );
}

/// `POST` формы с пиром, как у боевого listener через `ConnectInfo`.
async fn post_form(app: &Router, path: &str, fields: &[(&str, &str)]) -> StatusCode {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(fields)
        .finish();
    post_form_with_origin(app, path, "http://nas:18081", &body).await
}

/// `POST` формы с явным `Origin`: нужен для проверки отказа до обработчика.
async fn post_form_with_origin(app: &Router, path: &str, origin: &str, body: &str) -> StatusCode {
    let mut request = Request::builder()
        .method(Method::POST)
        .uri(path)
        .header(header::HOST, "nas:18081")
        .header(header::ORIGIN, origin)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body.to_owned()))
        .expect("request should build");
    request
        .extensions_mut()
        .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 4_040))));

    let response = app
        .clone()
        .oneshot(request)
        .await
        .expect("router should respond");
    let status = response.status();
    let _ = response.into_body().collect().await;
    status
}

/// Отказ сверки origin не выносит код восстановления в журнал.
#[test]
fn a_rejected_recovery_post_does_not_log_the_code() {
    let (logs, (status, code)) = capture(|| {
        runtime().block_on(async {
            let (pool, _url) = setup_db().await;
            insert_account(&pool).await;
            let issued = codes::issue(&pool, Purpose::Recovery, None, chrono::Utc::now())
                .await
                .expect("a recovery code");
            let app = build_admin_router(AdminState::new(
                pool.clone(),
                Vec::new(),
                Audit::new(pool.clone()),
            ));
            let path = format!("/admin/recover/{}", issued.code);
            let status = post_form_with_origin(
                &app,
                &path,
                "http://evil.example",
                "password=unused-password-12",
            )
            .await;
            (status, issued.code)
        })
    });

    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(
        !logs.contains(&code),
        "the recovery code must not be logged:\n{logs}"
    );
    assert!(
        logs.contains("/admin/recover/{code}"),
        "the route template is logged instead of the raw path:\n{logs}"
    );
}
