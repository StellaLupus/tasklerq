//! Потолок карты подавления и общий лимит записей на тип события.
//!
//! Отдельный тестовый бинарник: метрики — глобальные счётчики процесса, и
//! параллельные тесты одного файла исказили бы дельту.

mod support;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Request, StatusCode};
use chrono::{Duration, Utc};
use futures_util::future::join_all;
use serde_json::json;
use sqlx::PgPool;
use tasklerq::config::SecretString;
use tasklerq::http::router::{AppState, build_router};
use tasklerq::metrics;
use tasklerq::security_audit::{Audit, MAX_RECORDS_PER_WINDOW, SUPPRESSION_WINDOW_SECONDS};
use tasklerq::telegram::webhook::{Ingress, SECRET_HEADER};
use tokio::sync::Notify;
use tower::ServiceExt;

use support::isolated_db;

const SECRET: &str = "configured-webhook-SECRET-value";

fn peer() -> SocketAddr {
    SocketAddr::from(([172, 18, 0, 1], 4040))
}

fn app_with_webhook(pool: &PgPool, audit: Audit) -> Router {
    let ingress = Ingress::with_rate_limit(
        &SecretString::new(SECRET),
        Arc::new(Notify::new()),
        100_000,
        1,
    );
    build_router(
        AppState::new(pool.clone())
            .with_audit(audit)
            .with_telegram(ingress),
    )
    .layer(MockConnectInfo(peer()))
}

async fn post_webhook_from(app: &Router, secret: &str, forwarded: &str, body: &[u8]) -> StatusCode {
    let request = Request::builder()
        .method("POST")
        .uri("/webhooks/telegram")
        .header(SECRET_HEADER, secret)
        .header("x-forwarded-for", forwarded);
    app.clone()
        .oneshot(request.body(Body::from(body.to_vec())).expect("request"))
        .await
        .expect("response")
        .status()
}

async fn payloads(pool: &PgPool, event_type: &str) -> Vec<serde_json::Value> {
    sqlx::query_scalar(
        "SELECT payload FROM audit_events
         WHERE event_type::text = $1
         ORDER BY created_at, id",
    )
    .bind(event_type)
    .fetch_all(pool)
    .await
    .expect("audit payloads")
}

fn counter(text: &str, name: &str) -> u64 {
    let prefix = format!("{name} ");
    text.lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(0)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_burst_of_distinct_ips_is_capped_and_counted_by_the_metric() {
    let db = isolated_db().await;
    let audit = Audit::new(db.pool.clone());
    let app = app_with_webhook(&db.pool, audit.clone());
    let body = serde_json::to_vec(&json!({
        "update_id": 1,
        "message": {
            "message_id": 1,
            "date": 1_700_000_000,
            "chat": {"id": 42, "type": "private", "first_name": "Ann"},
            "from": {"id": 42, "is_bot": false, "first_name": "Ann"},
            "text": "Купить лекарство"
        }
    }))
    .expect("json");

    let before = counter(&metrics::render(), "webhook_auth_failures_total");
    let requests = (0..1_000).map(|index| {
        let app = app.clone();
        let body = body.clone();
        // 1000 разных адресов: карта подавления не должна расти без предела.
        let ip = format!(
            "10.{}.{}.{}",
            (index / 65_536) % 256,
            (index / 256) % 256,
            index % 256
        );
        async move { post_webhook_from(&app, "wrong-Bearer-SECRET-value", &ip, &body).await }
    });
    for status in join_all(requests).await {
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    let events = payloads(&db.pool, "auth_failed").await;
    assert!(
        !events.is_empty(),
        "the first failures must still be written"
    );
    assert!(
        events.len() <= MAX_RECORDS_PER_WINDOW,
        "auth_failed rows must not exceed the per-type limit: {}",
        events.len()
    );
    assert_eq!(
        counter(&metrics::render(), "webhook_auth_failures_total") - before,
        1_000,
        "the metric counts every rejection, including suppressed ones"
    );

    // Следующая разрешённая запись сообщает, сколько событий подавлено
    // агрегатно из-за общего лимита.
    audit
        .webhook_auth_failed(
            "missing_secret",
            "203.0.113.99".parse().expect("ip"),
            Utc::now() + Duration::seconds(SUPPRESSION_WINDOW_SECONDS + 1),
        )
        .await;
    let events = payloads(&db.pool, "auth_failed").await;
    let last = events.last().expect("an event after the window");
    assert!(
        last["suppressed"].as_u64().unwrap_or(0) >= 950,
        "over-limit events must be reported as suppressed: {last}"
    );
}
