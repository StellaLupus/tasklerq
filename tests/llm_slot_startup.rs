//! Метрика статуса слота, когда база не читается при старте.
//!
//! Отдельный тестовый бинарник: метрики — глобальные серии процесса, и соседние
//! сценарии этого файла не могут перезаписать `llm_slot_verified{slot="primary"}`.

mod support;

use std::time::Duration as StdDuration;

use tasklerq::app::build_application;
use tasklerq::config::Config;
use tokio::sync::watch;

fn metric_value(text: &str, series: &str) -> Option<f64> {
    text.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        if parts.next()? != series {
            return None;
        }
        parts.next()?.parse().ok()
    })
}

#[tokio::test]
async fn a_database_error_at_startup_publishes_an_unverified_metric() {
    let db = support::isolated_db().await;
    let database_url = support::database_url().await;
    sqlx::query("DROP TABLE llm_slot_checks")
        .execute(&db.pool)
        .await
        .expect("the table should be dropped");

    let config = Config::from_map([
        ("ENV", "development"),
        ("PUBLIC_BASE_URL", "http://127.0.0.1:18080"),
        ("DATABASE_URL", database_url.as_str()),
        ("TELEGRAM_MODE", "disabled"),
        ("TENANCY_MODE", "single"),
        ("LLM_CAPTURE_ENABLED", "true"),
        ("LLM_PRIMARY_URL", "https://api.example.test/v1"),
        ("LLM_PRIMARY_MODEL", "vendor/primary"),
        ("LLM_PRIMARY_TOKEN", "SECRET-slot-token"),
    ])
    .expect("test config should be valid");
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let application = build_application(&config, db.pool.clone(), shutdown_rx)
        .await
        .expect("a database error must not stop startup");

    assert_eq!(
        metric_value(
            &tasklerq::metrics::render(),
            "llm_slot_verified{slot=\"primary\"}"
        ),
        Some(0.0),
        "an unreadable check must still publish the alertable zero"
    );

    shutdown_tx.send(true).expect("shutdown signal");
    for worker in application.workers {
        tokio::time::timeout(StdDuration::from_secs(5), worker)
            .await
            .expect("worker must stop on shutdown")
            .expect("worker task must join");
    }
}
