//! Webhook `POST /webhooks/telegram`: секрет, лимиты, дубликаты и группы.

mod support;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::PgPool;
use support::db;
use tasklerq::config::SecretString;
use tasklerq::http::router::{AppState, build_router};
use tasklerq::security_audit::Audit;
use tasklerq::telegram::webhook::{Ingress, MAX_BODY_BYTES, SECRET_HEADER, WEBHOOKS_PER_SECOND};
use tokio::sync::Notify;
use tower::ServiceExt;

const SECRET: &str = "webhook-secret";

/// Пир внутри контейнера — шлюз Docker bridge, как в `docker-compose.yml`.
fn peer() -> SocketAddr {
    SocketAddr::from(([172, 18, 0, 1], 4040))
}

fn app_for(pool: &PgPool, peer: SocketAddr) -> Router {
    let ingress = Ingress::new(&SecretString::new(SECRET), Arc::new(Notify::new()));
    build_router(AppState::new(pool.clone(), Audit::new(pool.clone())).with_telegram(ingress))
        .layer(MockConnectInfo(peer))
}

fn app_with_window(pool: &PgPool, peer: SocketAddr, window_seconds: i64) -> Router {
    let ingress = Ingress::with_rate_limit(
        &SecretString::new(SECRET),
        Arc::new(Notify::new()),
        WEBHOOKS_PER_SECOND,
        window_seconds,
    );
    build_router(AppState::new(pool.clone(), Audit::new(pool.clone())).with_telegram(ingress))
        .layer(MockConnectInfo(peer))
}

fn app(pool: &PgPool) -> Router {
    app_for(pool, peer())
}

async fn post(app: &Router, secret: Option<&str>, body: Vec<u8>) -> (StatusCode, Value) {
    let mut request = Request::builder().method("POST").uri("/webhooks/telegram");
    if let Some(secret) = secret {
        request = request.header(SECRET_HEADER, secret);
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::from(body)).expect("request"))
        .await
        .expect("response");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn post_forwarded(
    app: &Router,
    secret: Option<&str>,
    forwarded: &str,
    body: Vec<u8>,
) -> (StatusCode, Option<String>) {
    let mut request = Request::builder()
        .method("POST")
        .uri("/webhooks/telegram")
        .header("x-forwarded-for", forwarded);
    if let Some(secret) = secret {
        request = request.header(SECRET_HEADER, secret);
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::from(body)).expect("request"))
        .await
        .expect("response");
    let status = response.status();
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let _ = response.into_body().collect().await;
    (status, retry_after)
}

fn update(id: i64, chat_kind: &str, chat_id: i64) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "update_id": id,
        "message": {
            "message_id": 1,
            "date": 1_700_000_000,
            "chat": {"id": chat_id, "type": chat_kind, "first_name": "Ann", "title": "Группа"},
            "from": {"id": 42, "is_bot": false, "first_name": "Ann"},
            "text": "секретный текст группы"
        }
    }))
    .expect("json")
}

async fn stored(pool: &PgPool, id: i64) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM telegram_updates WHERE update_id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("count")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn valid_update_is_stored_before_ok() {
    let db = db().await;
    let app = app(&db.pool);

    let (status, _) = post(&app, Some(SECRET), update(5_000_001, "private", 42)).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(stored(&db.pool, 5_000_001).await, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrong_or_missing_secret_is_rejected_without_storing() {
    let db = db().await;
    let app = app(&db.pool);

    for secret in [None, Some("wrong"), Some("")] {
        let (status, body) = post(&app, secret, update(5_000_002, "private", 42)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"]["code"], "unauthorized");
    }
    assert_eq!(stored(&db.pool, 5_000_002).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_update_id_is_stored_once() {
    let db = db().await;
    let app = app(&db.pool);

    for _ in 0..3 {
        let (status, _) = post(&app, Some(SECRET), update(5_000_003, "private", 42)).await;
        assert_eq!(status, StatusCode::OK);
    }

    assert_eq!(stored(&db.pool, 5_000_003).await, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_121st_request_from_one_ip_is_rejected_and_not_queued() {
    let db = db().await;
    // Окно расширено, чтобы 121 последовательный запрос гарантированно попал
    // в одно окно; скользящее окно в секунду проверяется unit-тестом лимитера.
    let app = app_with_window(&db.pool, peer(), 3_600);

    // За прокси (шлюз Docker bridge) лимит считается по последнему
    // X-Forwarded-For.
    for _ in 0..120 {
        let (status, retry_after) = post_forwarded(&app, None, "203.0.113.7", b"{}".to_vec()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(retry_after.is_none());
    }

    let (status, retry_after) = post_forwarded(
        &app,
        Some(SECRET),
        "203.0.113.7",
        update(5_100_001, "private", 42),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert!(retry_after.is_some(), "Retry-After is required");
    assert_eq!(stored(&db.pool, 5_100_001).await, 0);

    // Другой IP в ту же секунду обрабатывается как обычно.
    let (status, _) = post_forwarded(
        &app,
        Some(SECRET),
        "203.0.113.8",
        update(5_100_002, "private", 42),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(stored(&db.pool, 5_100_002).await, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_121st_request_from_one_ipv6_prefix_is_rejected_and_another_prefix_is_allowed() {
    let db = db().await;
    let app = app_with_window(&db.pool, peer(), 3_600);

    // Все адреса принадлежат одной /64; смена младших 64 бит не должна
    // обходить лимит.
    for index in 0..120 {
        let forwarded = format!("2001:db8:1:2::{index:x}");
        let (status, retry_after) = post_forwarded(&app, None, &forwarded, b"{}".to_vec()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(retry_after.is_none());
    }

    let (status, retry_after) = post_forwarded(
        &app,
        Some(SECRET),
        "2001:db8:1:2::78",
        update(5_200_001, "private", 42),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert!(retry_after.is_some(), "Retry-After is required");
    assert_eq!(stored(&db.pool, 5_200_001).await, 0);

    // Другая /64 считается отдельным клиентом.
    let (status, _) = post_forwarded(
        &app,
        Some(SECRET),
        "2001:db8:1:3::1",
        update(5_200_002, "private", 42),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(stored(&db.pool, 5_200_002).await, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn group_updates_are_acknowledged_but_not_stored() {
    let db = db().await;
    let app = app(&db.pool);

    for kind in ["group", "supergroup", "channel"] {
        let id = 5_000_010 + i64::try_from(kind.len()).expect("len");
        let (status, _) = post(&app, Some(SECRET), update(id, kind, -100)).await;
        assert_eq!(status, StatusCode::OK, "{kind}");
        assert_eq!(stored(&db.pool, id).await, 0, "{kind}");
    }
    let leaked: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM telegram_updates WHERE payload #>> '{message,chat,type}' <> 'private'",
    )
    .fetch_one(&db.pool)
    .await
    .expect("count");
    assert_eq!(leaked, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_and_oversized_bodies_are_rejected() {
    let db = db().await;
    let app = app(&db.pool);

    let (status, body) = post(&app, Some(SECRET), b"not json".to_vec()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "validation_error");

    let (status, _) = post(&app, Some(SECRET), vec![b' '; MAX_BODY_BYTES + 1]).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deeply_nested_json_is_rejected_by_the_parser_limit() {
    let db = db().await;
    let app = app(&db.pool);

    // serde_json по умолчанию ограничивает вложенность 128 уровнями. Тело
    // валидно по размеру, но не должно разбираться глубже безопасного предела.
    let body = format!(
        "{{\"update_id\":5000030,\"nested\":{}{}}}",
        "[".repeat(200),
        "]".repeat(200)
    );
    let (status, body) = post(&app, Some(SECRET), body.into_bytes()).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "validation_error");
    assert_eq!(stored(&db.pool, 5_000_030).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn route_is_absent_when_webhook_mode_is_off() {
    let db = db().await;
    let app = build_router(AppState::new(db.pool.clone(), Audit::new(db.pool.clone())))
        .layer(MockConnectInfo(peer()));

    let (status, _) = post(&app, Some(SECRET), update(5_000_020, "private", 42)).await;

    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_post_is_allowed() {
    let db = db().await;
    let app = app(&db.pool);

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/webhooks/telegram")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn readiness_fails_while_an_update_waits_longer_than_the_limit() {
    let db = db().await;
    let app = build_router(
        AppState::new(db.pool.clone(), Audit::new(db.pool.clone())).with_queue_lag_limit(60),
    );
    let ready = |app: Router| async move {
        app.oneshot(
            Request::builder()
                .uri("/health/ready")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response")
        .status()
    };

    assert_eq!(ready(app.clone()).await, StatusCode::OK);

    let update_id = 5_000_900_i64;
    sqlx::query(
        "INSERT INTO telegram_updates (update_id, payload, available_at)
         VALUES ($1, '{}'::jsonb, now() - interval '10 minutes')
         ON CONFLICT (update_id) DO NOTHING",
    )
    .bind(update_id)
    .execute(&db.pool)
    .await
    .expect("stuck update");
    assert_eq!(ready(app.clone()).await, StatusCode::SERVICE_UNAVAILABLE);

    // Обработанное обновление очередь не задерживает.
    sqlx::query("UPDATE telegram_updates SET status = 'processed' WHERE update_id = $1")
        .bind(update_id)
        .execute(&db.pool)
        .await
        .expect("processed");
    assert_eq!(ready(app).await, StatusCode::OK);
}
