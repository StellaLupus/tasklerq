//! Readiness: база, применённые миграции и задержка очереди.

mod support;

use std::time::Duration as StdDuration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{DateTime, Utc};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::PgPool;
use tasklerq::app::build_application;
use tasklerq::config::{Config, TenancyMode};
use tasklerq::http::router::{AppState, build_router};
use tasklerq::security_audit::Audit;
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
