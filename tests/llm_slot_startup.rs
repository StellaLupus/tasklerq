//! Метрика статуса слота, когда база не читается при старте.
//!
//! Метрики — глобальные серии процесса. Тесты этого файла выставляют
//! `llm_slot_verified{slot="primary"}` в 1 под общим мьютексом, поэтому 0 после
//! действия может появиться только от проверяемого кода.

mod support;

use std::sync::Arc;
use std::time::Duration as StdDuration;

use tasklerq::app::build_application;
use tasklerq::capture::slot_status::{LlmSlotHealth, SlotStatus};
use tasklerq::config::{Config, LlmConfig, SecretString, SlotConfig};
use tokio::sync::watch;

/// Сериализует тесты, которые читают или пишут `llm_slot_verified`: реестр
/// метрик общий на процесс.
static SLOT_METRIC: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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
    let _gauge = SLOT_METRIC.lock().await;
    // Исходное значение 1: 0 ниже может выставить только проверяемый код.
    tasklerq::metrics::llm_slot_verified("primary", true);

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

#[tokio::test]
async fn an_unreadable_database_without_a_cache_marks_slots_unverified() {
    let _gauge = SLOT_METRIC.lock().await;
    // Исходное значение 1: 0 ниже может выставить только проверяемый код.
    tasklerq::metrics::llm_slot_verified("primary", true);

    let primary = SlotConfig {
        url: "https://api.example.test/v1".parse().expect("url"),
        model: "vendor/primary".to_owned(),
        token: Some(SecretString::new("SECRET-slot-token")),
        timeout: StdDuration::from_secs(10),
        extra: Default::default(),
    };
    let chain = support::llm::chain(
        support::llm::BackendDouble::always(Ok(support::llm::v4::response(vec![]))),
        None,
    );
    let llm = LlmConfig {
        enabled: true,
        primary: Some(primary),
        fallback: None,
        connect_timeout: StdDuration::from_millis(1_000),
        daily_quota: 200,
    };
    let health = LlmSlotHealth::new(Arc::new(chain), &llm).expect("llm health");

    let broken = sqlx::postgres::PgPoolOptions::new()
        .acquire_timeout(StdDuration::from_millis(500))
        .connect_lazy("postgres://tasklerq:secret@127.0.0.1:1/tasklerq")
        .expect("lazy pool");

    // Кеша ещё нет: ошибка чтения не должна оставить статус без метрики.
    let statuses = health.statuses(&broken, chrono::Utc::now()).await;

    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0].status, SlotStatus::Unverified);
    assert_eq!(
        metric_value(
            &tasklerq::metrics::render(),
            "llm_slot_verified{slot=\"primary\"}"
        ),
        Some(0.0),
        "a failed read without a cache must publish the alertable zero"
    );
}
