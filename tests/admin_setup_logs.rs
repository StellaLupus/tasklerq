//! Стартовый setup-код консоли: единственная строка `warn` с кодом и портом.
//!
//! Отдельный тестовый бинарник, как `degradation_logs.rs`: подписчик ставится
//! на время теста через `with_default`, а `tracing` кеширует интерес к
//! callsite, поэтому параллельные тесты общего бинарника могли бы отключить
//! его до появления подписчика.

mod support;

use serde_json::Value;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use support::logs::LogBuffer;
use tasklerq::admin_console::codes::{self, Purpose};
use tasklerq::config::Config;
use tasklerq::storage::migrations;
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
