//! Метрики отказов аутентификации webhook.
//!
//! Отдельный тестовый бинарник: метрики — глобальные счётчики процесса, и
//! параллельные тесты одного файла исказили бы дельту. В этом файле отказы
//! аутентификации webhook создаёт только один тест.

mod support;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Request, StatusCode};
use chrono::{Duration, Utc};
use serde_json::json;
use sqlx::PgPool;
use tasklerq::config::SecretString;
use tasklerq::http::router::{AppState, build_router};
use tasklerq::metrics;
use tasklerq::security_audit::{Audit, SUPPRESSION_WINDOW_SECONDS, WEBHOOK_FAILURE_THRESHOLD};
use tasklerq::telegram::webhook::{Ingress, SECRET_HEADER};
use tokio::sync::Notify;
use tower::ServiceExt;

use support::isolated_db;

const SECRET: &str = "configured-webhook-SECRET-value";

fn peer() -> SocketAddr {
    SocketAddr::from(([172, 18, 0, 1], 4040))
}

fn app_with_webhook(pool: &PgPool, audit: Audit, per_second: u32) -> Router {
    let ingress = Ingress::with_rate_limit(
        &SecretString::new(SECRET),
        Arc::new(Notify::new()),
        per_second,
        1,
    );
    build_router(
        AppState::new(pool.clone())
            .with_audit(audit)
            .with_telegram(ingress),
    )
    .layer(MockConnectInfo(peer()))
}

async fn post_webhook(app: &Router, secret: Option<&str>, body: &[u8]) -> StatusCode {
    post_webhook_from(app, secret, None, body).await
}

async fn post_webhook_from(
    app: &Router,
    secret: Option<&str>,
    forwarded: Option<&str>,
    body: &[u8],
) -> StatusCode {
    let mut request = Request::builder().method("POST").uri("/webhooks/telegram");
    if let Some(secret) = secret {
        request = request.header(SECRET_HEADER, secret);
    }
    if let Some(forwarded) = forwarded {
        request = request.header("x-forwarded-for", forwarded);
    }
    app.clone()
        .oneshot(request.body(Body::from(body.to_vec())).expect("request"))
        .await
        .expect("response")
        .status()
}

async fn update_payload(id: i64) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "update_id": id,
        "message": {
            "message_id": 1,
            "date": 1_700_000_000,
            "chat": {"id": 42, "type": "private", "first_name": "Ann"},
            "from": {"id": 42, "is_bot": false, "first_name": "Ann"},
            "text": "Купить лекарство"
        }
    }))
    .expect("json")
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
async fn webhook_auth_failures_are_counted_once_per_window_with_a_threshold() {
    let db = isolated_db().await;
    // Пир стенда — шлюз Docker bridge; X-Forwarded-For в тесте не задан.
    let ip = "172.18.0.1";

    // Отдельная причина отказа: неверный секрет.
    let audit = Audit::new(db.pool.clone());
    let app = app_with_webhook(&db.pool, audit, 10_000);
    assert_eq!(
        post_webhook_from(
            &app,
            Some("wrong-Bearer-SECRET-value"),
            Some("203.0.113.10"),
            &update_payload(1).await,
        )
        .await,
        StatusCode::UNAUTHORIZED
    );
    let events = payloads(&db.pool, "auth_failed").await;
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["source"], "webhook");
    assert_eq!(events[0]["reason"], "bad_secret");
    assert_eq!(events[0]["ip"], "203.0.113.10");
    assert_eq!(events[0]["suppressed"], 0);

    // 50 отказов с одного IP — порог ещё не превышен.
    let audit = Audit::new(db.pool.clone());
    let app = app_with_webhook(&db.pool, audit.clone(), 10_000);
    let before = metrics::render();
    for id in 0..WEBHOOK_FAILURE_THRESHOLD {
        assert_eq!(
            post_webhook(
                &app,
                None,
                &update_payload(10 + i64::try_from(id).expect("id")).await
            )
            .await,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        payloads(&db.pool, "auth_failure_threshold_exceeded")
            .await
            .len(),
        0,
        "50 failures do not exceed the threshold"
    );
    assert_eq!(
        counter(&metrics::render(), "webhook_auth_failures_total")
            - counter(&before, "webhook_auth_failures_total"),
        WEBHOOK_FAILURE_THRESHOLD
    );
    assert_eq!(
        counter(
            &metrics::render(),
            "webhook_auth_failure_threshold_exceeded_total"
        ) - counter(&before, "webhook_auth_failure_threshold_exceeded_total"),
        0
    );

    // 51-й отказ создаёт одно событие порога и увеличивает метрику на 1.
    let before_threshold = metrics::render();
    assert_eq!(
        post_webhook(&app, None, &update_payload(100).await).await,
        StatusCode::UNAUTHORIZED
    );
    let threshold = payloads(&db.pool, "auth_failure_threshold_exceeded").await;
    assert_eq!(threshold.len(), 1, "{threshold:?}");
    assert_eq!(threshold[0]["window_seconds"], 300);
    assert_eq!(threshold[0]["threshold"], WEBHOOK_FAILURE_THRESHOLD);
    assert_eq!(threshold[0]["count"], WEBHOOK_FAILURE_THRESHOLD + 1);
    assert_eq!(
        counter(
            &metrics::render(),
            "webhook_auth_failure_threshold_exceeded_total"
        ) - counter(
            &before_threshold,
            "webhook_auth_failure_threshold_exceeded_total"
        ),
        1
    );

    // Остальные 149 отказов не пишут ни строки в журнал и не повторяют порог,
    // но метрика считает каждый отказ.
    for id in 0..(200 - WEBHOOK_FAILURE_THRESHOLD - 1) {
        assert_eq!(
            post_webhook(
                &app,
                None,
                &update_payload(200 + i64::try_from(id).expect("id")).await
            )
            .await,
            StatusCode::UNAUTHORIZED
        );
    }
    let auth_failed = payloads(&db.pool, "auth_failed").await;
    assert_eq!(
        auth_failed
            .iter()
            .filter(|payload| payload["ip"] == ip)
            .count(),
        1,
        "one row per IP per suppression window: {auth_failed:?}"
    );
    assert_eq!(
        payloads(&db.pool, "auth_failure_threshold_exceeded")
            .await
            .len(),
        1,
        "the threshold event is not repeated while the excess lasts"
    );
    assert_eq!(
        counter(&metrics::render(), "webhook_auth_failures_total")
            - counter(&before, "webhook_auth_failures_total"),
        200,
        "the metric counts every rejection"
    );

    // Следующее событие после окна сообщает, сколько случаев подавлено.
    audit
        .webhook_auth_failed(
            "missing_secret",
            ip.parse().expect("ip"),
            Utc::now() + Duration::seconds(SUPPRESSION_WINDOW_SECONDS + 1),
        )
        .await;
    let auth_failed = payloads(&db.pool, "auth_failed").await;
    let after_window = auth_failed
        .iter()
        .rfind(|payload| payload["ip"] == ip)
        .expect("a new event after the window");
    assert_eq!(after_window["suppressed"], 199, "{after_window}");
}
