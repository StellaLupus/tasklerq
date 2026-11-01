//! Readiness: база, применённые миграции и задержка очереди.

mod support;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::DateTime;
use http_body_util::BodyExt;
use serde_json::Value;
use sqlx::PgPool;
use tasklerq::http::router::{AppState, build_router};
use tasklerq::security_audit::Audit;
use tower::ServiceExt;

async fn ready(pool: &PgPool) -> (StatusCode, Value) {
    let app = build_router(AppState::new(pool.clone(), Audit::new(pool.clone())));
    let response = app
        .oneshot(
            Request::builder()
                .uri("/health/ready")
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
