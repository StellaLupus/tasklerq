//! Readiness: база, применённые миграции и задержка очереди.

mod support;

use std::sync::Arc;
use std::time::Duration as StdDuration;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use chrono::{DateTime, Utc};
use http_body_util::BodyExt;
use serde_json::{Map, Value, json};
use sqlx::PgPool;
use tasklerq::app::build_application;
use tasklerq::capture::prompt;
use tasklerq::capture::slot_check;
use tasklerq::capture::slot_status::LlmSlotHealth;
use tasklerq::config::{CaptureConfig, Config, LlmConfig, SecretString, SlotConfig, TenancyMode};
use tasklerq::http::api::capture::CaptureState;
use tasklerq::http::router::{AppState, build_router};
use tasklerq::llm::Chain;
use tasklerq::security_audit::Audit;
use tasklerq::storage::repositories::llm_slot_checks::{self, NewCheck};
use tokio::sync::watch;
use tower::ServiceExt;

async fn ready(pool: &PgPool) -> (StatusCode, Value) {
    ready_with_mode(pool, TenancyMode::Single).await
}

async fn ready_with_mode(pool: &PgPool, mode: TenancyMode) -> (StatusCode, Value) {
    let state = AppState::new(pool.clone(), Audit::new(pool.clone())).with_tenancy_mode(mode);
    get(&build_router(state), "/health/ready").await
}

async fn get(app: &Router, uri: &str) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(uri)
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let body = serde_json::from_slice(&bytes).expect("JSON");
    (status, body)
}

fn slot_config(model: &str) -> SlotConfig {
    SlotConfig {
        url: "https://api.example.test/v1".parse().expect("url"),
        model: model.to_owned(),
        token: Some(SecretString::new("SECRET-slot-token")),
        timeout: StdDuration::from_secs(10),
        extra: Map::new(),
    }
}

/// Цепочка из двойников и состояние проверки слотов для readiness.
fn llm_health(
    primary: &SlotConfig,
    fallback: Option<&SlotConfig>,
) -> (Arc<Chain>, Arc<LlmSlotHealth>) {
    let answer = || {
        support::llm::BackendDouble::always(Ok(support::llm::v4::response(vec![
            support::llm::v4::shopping("Молоко"),
        ])))
    };
    let chain = Arc::new(support::llm::chain(answer(), fallback.map(|_| answer())));
    let llm = LlmConfig {
        enabled: true,
        primary: Some(primary.clone()),
        fallback: fallback.cloned(),
        connect_timeout: StdDuration::from_millis(1_000),
        daily_quota: 200,
    };
    let health = Arc::new(LlmSlotHealth::new(chain.clone(), &llm).expect("llm health"));
    (chain, health)
}

async fn store_check(pool: &PgPool, slot: &str, config: &SlotConfig, passed: usize, total: usize) {
    let fingerprint = slot_check::fingerprint(
        config,
        &prompt::contract_hash(),
        &slot_check::set_hash(slot_check::BUILTIN_SET),
    );
    let parts = fingerprint.parts.to_json();
    let report = json!({"total": total, "passed": passed});
    let mut conn = pool.acquire().await.expect("connection");
    llm_slot_checks::upsert(
        &mut conn,
        NewCheck {
            slot,
            fingerprint: &fingerprint.value,
            fingerprint_parts: &parts,
            passed,
            total,
            p95_ms: 1_000,
            report: &report,
        },
        Utc::now(),
    )
    .await
    .expect("slot check should be stored");
}

async fn ready_with_llm(pool: &PgPool, health: Arc<LlmSlotHealth>) -> (StatusCode, Value) {
    let state = AppState::new(pool.clone(), Audit::new(pool.clone()))
        .with_tenancy_mode(TenancyMode::Single)
        .with_llm_slots(health);
    get(&build_router(state), "/health/ready").await
}

fn capture_config() -> CaptureConfig {
    CaptureConfig {
        enabled: true,
        voice_timeout_ms: 8_000,
        voice_fallback_raw: false,
        rate_limit_per_min: 10,
        max_text_chars: 2_000,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn readiness_requires_every_embedded_migration_to_be_successful() {
    let db = support::isolated_db().await;

    // Полностью мигрированная база готова и отдаёт версию и время.
    let (status, body) = ready(&db.pool).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "ok");
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
    let time = body["time"].as_str().expect("time");
    DateTime::parse_from_rfc3339(time).expect("RFC 3339");

    // Неуспешная запись в журнале миграций — не готов.
    sqlx::query(
        "UPDATE _sqlx_migrations SET success = false
         WHERE version = (SELECT max(version) FROM _sqlx_migrations)",
    )
    .execute(&db.pool)
    .await
    .expect("mark failed");
    let (status, body) = ready(&db.pool).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"]["code"], "service_unavailable");

    // Возврат успешного флага снова делает базу готовой.
    sqlx::query(
        "UPDATE _sqlx_migrations SET success = true
         WHERE version = (SELECT max(version) FROM _sqlx_migrations)",
    )
    .execute(&db.pool)
    .await
    .expect("mark successful");
    assert_eq!(ready(&db.pool).await.0, StatusCode::OK);

    // Отсутствующая миграция (журнал не содержит последнюю версию) — не готов.
    sqlx::query(
        "DELETE FROM _sqlx_migrations
         WHERE version = (SELECT max(version) FROM _sqlx_migrations)",
    )
    .execute(&db.pool)
    .await
    .expect("drop migration record");
    let (status, body) = ready(&db.pool).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"]["code"], "service_unavailable");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn single_mode_with_one_household_has_no_warnings() {
    let db = support::isolated_db().await;
    let household = support::create_household(&db.pool).await;

    let (status, body) = ready_with_mode(&db.pool, TenancyMode::Single).await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "ok");
    assert!(
        body.get("warnings").is_none(),
        "one household must not warn:\n{body}"
    );
    assert!(
        !body
            .to_string()
            .contains(&household.household_id.to_string())
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn single_mode_with_two_households_warns_without_names_or_ids() {
    let db = support::isolated_db().await;
    let first = support::create_household(&db.pool).await;
    let second = support::create_household(&db.pool).await;

    let (status, body) = ready_with_mode(&db.pool, TenancyMode::Single).await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "ok");
    assert_eq!(body["warnings"], json!(["multiple_households"]));
    let response = body.to_string();
    for household in [first, second] {
        assert!(!response.contains(&household.household_id.to_string()));
    }
    assert!(!response.contains("Test household"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multi_mode_with_two_households_has_no_warnings() {
    let db = support::isolated_db().await;
    support::create_household(&db.pool).await;
    support::create_household(&db.pool).await;

    let (status, body) = ready_with_mode(&db.pool, TenancyMode::Multi).await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body.get("warnings").is_none(),
        "multi mode must not warn:\n{body}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn warning_disappears_after_a_soft_delete_without_rebuilding_the_app() {
    let db = support::isolated_db().await;
    let first = support::create_household(&db.pool).await;
    let second = support::create_household(&db.pool).await;
    let state = AppState::new(db.pool.clone(), Audit::new(db.pool.clone()))
        .with_tenancy_mode(TenancyMode::Single);
    let app = build_router(state);

    let (status, body) = get(&app, "/health/ready").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["warnings"], json!(["multiple_households"]));

    tasklerq::admin::delete_household(&db.pool, second.household_id, Utc::now())
        .await
        .expect("second household should be deleted");

    let (status, body) = get(&app, "/health/ready").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body.get("warnings").is_none(),
        "the warning must disappear without a restart:\n{body}"
    );
    assert!(!body.to_string().contains(&first.household_id.to_string()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn liveness_never_reports_warnings() {
    let db = support::isolated_db().await;
    support::create_household(&db.pool).await;
    support::create_household(&db.pool).await;
    let state = AppState::new(db.pool.clone(), Audit::new(db.pool.clone()))
        .with_tenancy_mode(TenancyMode::Single);
    let app = build_router(state);

    let (status, body) = get(&app, "/health/live").await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "ok");
    assert!(
        body.get("warnings").is_none(),
        "liveness must never compute degradation:\n{body}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn production_build_wires_the_tenancy_mode_into_readiness() {
    let db = support::isolated_db().await;
    support::create_household(&db.pool).await;
    support::create_household(&db.pool).await;
    let database_url = support::database_url().await;

    // Проверяем именно боевую сборку: дефолт `AppState::new` — `Single`, поэтому
    // забытое `with_tenancy_mode` проявится в `multi` ложным предупреждением.
    for (mode, expected_warning) in [("single", true), ("multi", false)] {
        let config = Config::from_map([
            ("ENV", "development"),
            ("PUBLIC_BASE_URL", "http://127.0.0.1:18080"),
            ("DATABASE_URL", database_url.as_str()),
            ("TELEGRAM_MODE", "disabled"),
            ("TENANCY_MODE", mode),
            ("LLM_CAPTURE_ENABLED", "false"),
        ])
        .expect("test config should be valid");
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let application = build_application(&config, db.pool.clone(), shutdown_rx)
            .await
            .expect("application should build");

        let (status, body) = get(&application.router, "/health/ready").await;
        assert_eq!(status, StatusCode::OK, "{mode}: {body}");
        if expected_warning {
            assert_eq!(
                body["warnings"],
                json!(["multiple_households"]),
                "{mode}: {body}"
            );
        } else {
            assert!(
                body.get("warnings").is_none(),
                "{mode}: the configured mode must reach readiness:\n{body}"
            );
        }

        shutdown_tx.send(true).expect("shutdown signal");
        for worker in application.workers {
            tokio::time::timeout(StdDuration::from_secs(5), worker)
                .await
                .expect("worker must stop on shutdown")
                .expect("worker task must join");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn all_llm_slots_unavailable_is_a_warning_not_a_failure() {
    let db = support::isolated_db().await;
    let primary = slot_config("vendor/primary");
    let fallback = slot_config("vendor/fallback");
    let (chain, health) = llm_health(&primary, Some(&fallback));
    store_check(&db.pool, "primary", &primary, 22, 22).await;
    store_check(&db.pool, "fallback", &fallback, 22, 22).await;
    let now = Utc::now();
    support::llm::open_primary(&chain, now);
    support::llm::open_fallback(&chain, now);

    let (status, body) = ready_with_llm(&db.pool, health).await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["warnings"], json!(["llm_unavailable"]), "{body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unchecked_primary_slot_is_reported_in_readiness() {
    let db = support::isolated_db().await;
    let primary = slot_config("vendor/primary");
    let (_chain, health) = llm_health(&primary, None);

    let (status, body) = ready_with_llm(&db.pool, health).await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["warnings"],
        json!(["llm_slot_unverified:primary"]),
        "{body}"
    );
    let response = body.to_string();
    assert!(!response.contains("vendor/primary"), "{response}");
    assert!(!response.contains("api.example.test"), "{response}");
    assert!(!response.contains("SECRET-slot-token"), "{response}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unchecked_slot_does_not_block_capture() {
    let db = support::isolated_db().await;
    let household = support::create_household(&db.pool).await;
    let token = support::issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let primary = slot_config("vendor/primary");
    let (chain, health) = llm_health(&primary, None);
    let state = AppState::new(db.pool.clone(), Audit::new(db.pool.clone()))
        .with_tenancy_mode(TenancyMode::Single)
        .with_llm_slots(health)
        .with_capture(CaptureState::new(capture_config(), Some(chain), 200));
    let app = build_router(state);

    let (status, body) = support::request(
        &app,
        Method::POST,
        "/api/v1/capture",
        Some(&token),
        Some(json!({"text": "Молоко", "source": "api", "mode": "auto"})),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "applied", "{body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_passed_slot_has_no_readiness_warning() {
    let db = support::isolated_db().await;
    let primary = slot_config("vendor/primary");
    let (_chain, health) = llm_health(&primary, None);
    store_check(&db.pool, "primary", &primary, 22, 22).await;

    let (status, body) = ready_with_llm(&db.pool, health).await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body.get("warnings").is_none(),
        "a passed slot must not warn:\n{body}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_readiness_status_is_cached_for_a_minute() {
    let db = support::isolated_db().await;
    let primary = slot_config("vendor/primary");
    let (_chain, health) = llm_health(&primary, None);
    store_check(&db.pool, "primary", &primary, 22, 22).await;

    let (status, body) = ready_with_llm(&db.pool, health.clone()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.get("warnings").is_none(), "{body}");

    // Проверка изменилась, но readiness ещё смотрит в кеш.
    store_check(&db.pool, "primary", &primary, 17, 22).await;
    let (status, body) = ready_with_llm(&db.pool, health.clone()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body.get("warnings").is_none(),
        "the cached status must hide the new row:\n{body}"
    );

    // Явный сброс кеша заменяет ожидание минуты.
    health.invalidate();
    let (status, body) = ready_with_llm(&db.pool, health).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["warnings"],
        json!(["llm_slot_unverified:primary"]),
        "{body}"
    );
}

#[tokio::test]
async fn startup_warns_about_an_unchecked_primary_slot_without_secrets() {
    let db = support::isolated_db().await;
    let database_url = support::database_url().await;
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

    // current_thread: подписчик `set_default` живёт на потоке теста, и все
    // записи старта попадают в буфер.
    let logs = support::logs::LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(logs.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let application = build_application(&config, db.pool.clone(), shutdown_rx)
        .await
        .expect("an unchecked slot must not stop startup");

    let (status, body) = get(&application.router, "/health/ready").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["warnings"],
        json!(["llm_slot_unverified:primary"]),
        "{body}"
    );

    let text = logs.contents();
    assert!(
        text.contains("primary"),
        "the slot name is missing:\n{text}"
    );
    assert!(
        text.contains("unverified"),
        "the status is missing:\n{text}"
    );
    assert!(!text.contains("SECRET-slot-token"), "token leaked:\n{text}");
    assert!(
        !text.contains("api.example.test"),
        "slot address leaked:\n{text}"
    );

    shutdown_tx.send(true).expect("shutdown signal");
    for worker in application.workers {
        tokio::time::timeout(StdDuration::from_secs(5), worker)
            .await
            .expect("worker must stop on shutdown")
            .expect("worker task must join");
    }
}
