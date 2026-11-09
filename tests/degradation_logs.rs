//! Стартовые сообщения `app.rs`: деградация при нескольких household и
//! предупреждение о незаданном `BOOTSTRAP_OWNER_TELEGRAM_ID`.
//!
//! Отдельный тестовый бинарник, как `outbox_dead_logs.rs`: `tracing` кеширует
//! интерес к callsite, и параллельные тесты общего бинарника могли бы отключить
//! его до появления подписчика.

mod support;

use std::time::Duration as StdDuration;

use sqlx::PgPool;
use support::logs::LogBuffer;
use tasklerq::app::build_application;
use tasklerq::config::Config;
use tokio::sync::watch;

const BOT_TOKEN: &str = "123456:SECRET-bot-token";
const WEBHOOK_SECRET: &str = "SECRET-webhook-secret";

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

fn config(database_url: &str, tenancy: &str, telegram: &str) -> Config {
    let mut vars = vec![
        ("ENV", "development"),
        ("PUBLIC_BASE_URL", "http://127.0.0.1:18080"),
        ("DATABASE_URL", database_url),
        ("TELEGRAM_MODE", telegram),
        ("TENANCY_MODE", tenancy),
        ("LLM_CAPTURE_ENABLED", "false"),
    ];
    if telegram != "disabled" {
        vars.extend([
            ("TELEGRAM_BOT_TOKEN", BOT_TOKEN),
            ("TELEGRAM_WEBHOOK_SECRET", WEBHOOK_SECRET),
            (
                "TELEGRAM_WEBHOOK_URL",
                "http://127.0.0.1:18080/webhooks/telegram",
            ),
            ("TELEGRAM_API_BASE_URL", "http://127.0.0.1:1"),
        ]);
    }
    Config::from_map(vars).expect("test config should be valid")
}

async fn build_and_stop(config: &Config, pool: &PgPool) {
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let application = build_application(config, pool.clone(), shutdown_rx)
        .await
        .expect("application should start");
    shutdown_tx.send(true).expect("shutdown signal");
    for worker in application.workers {
        tokio::time::timeout(StdDuration::from_secs(5), worker)
            .await
            .expect("worker must stop on shutdown")
            .expect("worker task must join");
    }
}

#[test]
fn single_mode_with_two_households_logs_an_error_with_the_count_only() {
    let (logs, (first, second)) = capture(|| {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let db = support::isolated_db().await;
            let first = support::create_household(&db.pool).await;
            let second = support::create_household(&db.pool).await;
            let database_url = support::database_url().await;
            let config = config(&database_url, "single", "disabled");
            build_and_stop(&config, &db.pool).await;
            (first, second)
        })
    });

    assert!(logs.contains("\"level\":\"ERROR\""), "{logs}");
    assert!(logs.contains("multiple active households"), "{logs}");
    assert!(
        logs.contains("\"households\":2"),
        "the error must carry the household count:\n{logs}"
    );
    for household in [first, second] {
        assert!(
            !logs.contains(&household.household_id.to_string()),
            "the error must not contain household ids:\n{logs}"
        );
    }
    assert!(
        !logs.contains("Test household"),
        "the error must not contain household names:\n{logs}"
    );
}

#[test]
fn single_mode_with_one_household_logs_no_degradation_error() {
    let (logs, ()) = capture(|| {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let db = support::isolated_db().await;
            support::create_household(&db.pool).await;
            let database_url = support::database_url().await;
            let config = config(&database_url, "single", "disabled");
            build_and_stop(&config, &db.pool).await;
        })
    });

    assert!(
        !logs.contains("multiple active households"),
        "one household must not degrade:\n{logs}"
    );
}

#[test]
fn multi_mode_with_two_households_logs_no_degradation_error() {
    let (logs, ()) = capture(|| {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let db = support::isolated_db().await;
            support::create_household(&db.pool).await;
            support::create_household(&db.pool).await;
            let database_url = support::database_url().await;
            let config = config(&database_url, "multi", "disabled");
            build_and_stop(&config, &db.pool).await;
        })
    });

    assert!(
        !logs.contains("multiple active households"),
        "multi mode must not degrade:\n{logs}"
    );
}

#[test]
fn enabled_telegram_without_bootstrap_owner_warns_through_build_application() {
    let (logs, ()) = capture(|| {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let db = support::isolated_db().await;
            let database_url = support::database_url().await;
            let config = config(&database_url, "single", "webhook");
            build_and_stop(&config, &db.pool).await;
        })
    });

    assert!(
        logs.contains("BOOTSTRAP_OWNER_TELEGRAM_ID"),
        "the production startup path must warn about the missing bootstrap owner:\n{logs}"
    );
    assert!(
        logs.contains("first person"),
        "the warning must explain who becomes the owner:\n{logs}"
    );
    assert!(!logs.contains(BOT_TOKEN), "the warning leaked the token");
    assert!(
        !logs.contains(WEBHOOK_SECRET),
        "the warning leaked the webhook secret"
    );
}
