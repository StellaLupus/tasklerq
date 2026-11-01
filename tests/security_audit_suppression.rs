//! Потолок карты подавления и общий лимит записей на тип события.
//!
//! Отдельный тестовый бинарник: метрики — глобальные счётчики процесса, и
//! параллельные тесты одного файла исказили бы дельту.

mod support;

use std::net::{IpAddr, SocketAddr};
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
use tasklerq::domain::models::{Actor, ActorType};
use tasklerq::http::router::{AppState, build_router};
use tasklerq::metrics;
use tasklerq::rate_limit::ip_key;
use tasklerq::security_audit::{
    Audit, CAPTURE_LIMIT, LimitKey, MAX_RECORDS_PER_WINDOW, RateLimitEvent,
    SUPPRESSION_WINDOW_SECONDS, WEBHOOK_LIMIT,
};
use tasklerq::telegram::webhook::{Ingress, SECRET_HEADER};
use tokio::sync::Notify;
use tower::ServiceExt;
use uuid::Uuid;

use support::isolated_db;

const SECRET: &str = "configured-webhook-SECRET-value";

/// Метрики — глобальные счётчики процесса, поэтому тесты этого файла нельзя
/// запускать параллельно: один исказит дельту другого.
static AUDIT_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn peer() -> SocketAddr {
    SocketAddr::from(([172, 18, 0, 1], 4040))
}

fn app_with_webhook(pool: &PgPool, audit: Audit) -> Router {
    app_with_webhook_limit(pool, audit, 100_000, 1)
}

fn app_with_webhook_limit(pool: &PgPool, audit: Audit, limit: u32, window_seconds: i64) -> Router {
    let ingress = Ingress::with_rate_limit(
        &SecretString::new(SECRET),
        Arc::new(Notify::new()),
        limit,
        window_seconds,
    );
    build_router(AppState::new(pool.clone(), audit).with_telegram(ingress))
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
    let _guard = AUDIT_TEST_LOCK.lock().await;
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_webhook_flood_does_not_hide_api_auth_failures() {
    let _guard = AUDIT_TEST_LOCK.lock().await;
    let db = isolated_db().await;
    let household = support::create_household(&db.pool).await;
    let audit = Audit::new(db.pool.clone());
    let app = app_with_webhook(&db.pool, audit.clone());

    // Потолок выбран так, чтобы флуд webhook исчерпал свой бюджет записей,
    // но не бюджет API в том же окне подавления.
    for index in 0..60 {
        let ip = format!("198.51.100.{}", index % 256);
        let status = post_webhook_from(&app, "wrong-Bearer-SECRET-value", &ip, b"{}").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    audit
        .api_auth_failed(
            "revoked_token",
            "/api/v1/lists",
            Some((Uuid::now_v7(), household.household_id)),
            Utc::now(),
        )
        .await;

    let events = payloads(&db.pool, "auth_failed").await;
    let api_event = events
        .iter()
        .find(|payload| payload["source"] == "api")
        .expect("an API auth failure must be recorded despite the webhook flood");
    assert_eq!(api_event["reason"], "revoked_token");
    let webhook_events = events
        .iter()
        .filter(|payload| payload["source"] == "webhook")
        .count();
    assert_eq!(
        webhook_events, MAX_RECORDS_PER_WINDOW,
        "the webhook flood must exhaust its own cap without touching the API budget"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_webhook_rate_limit_flood_does_not_hide_capture_rate_limits() {
    let _guard = AUDIT_TEST_LOCK.lock().await;
    let db = isolated_db().await;
    let audit = Audit::new(db.pool.clone());
    // Один запрос на /64, окно на весь тест: второй адрес той же /64 уже
    // отклоняется по лимиту.
    let app = app_with_webhook_limit(&db.pool, audit.clone(), 1, 3_600);

    // 60 разных /64 дают 60 отказов по лимиту — больше потолка записей.
    for index in 0..60 {
        let first = format!("2001:db8:{index:x}:1::1");
        let second = format!("2001:db8:{index:x}:1::2");
        assert_eq!(
            post_webhook_from(&app, "wrong-secret", &first, b"{}").await,
            StatusCode::UNAUTHORIZED,
            "the first request of a /64 is allowed by the rate limit"
        );
        assert_eq!(
            post_webhook_from(&app, "wrong-secret", &second, b"{}").await,
            StatusCode::TOO_MANY_REQUESTS
        );
    }

    let webhook_events = payloads(&db.pool, "rate_limited")
        .await
        .into_iter()
        .filter(|payload| payload["limit"] == WEBHOOK_LIMIT)
        .count();
    assert_eq!(
        webhook_events, MAX_RECORDS_PER_WINDOW,
        "the webhook flood must exhaust its own rate-limit record budget"
    );

    // Превышение лимита capture в той же минуте должно попасть в журнал,
    // несмотря на исчерпанный потолок webhook.
    let recorded = audit
        .rate_limited(
            RateLimitEvent {
                limit: CAPTURE_LIMIT,
                key: LimitKey::Token(Uuid::now_v7()),
                retry_after: 60,
                household_id: None,
                actor: Actor::system(ActorType::System),
            },
            Utc::now(),
        )
        .await;
    assert!(
        recorded,
        "the capture rate-limit event must be recorded during a webhook flood"
    );

    let events = payloads(&db.pool, "rate_limited").await;
    assert!(
        events
            .iter()
            .any(|payload| payload["limit"] == CAPTURE_LIMIT),
        "the capture rate-limit event must stay visible in the audit log: {events:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn addresses_from_one_ipv6_prefix_share_one_rate_limit_event() {
    let _guard = AUDIT_TEST_LOCK.lock().await;
    let db = isolated_db().await;
    let audit = Audit::new(db.pool.clone());
    let app = app_with_webhook_limit(&db.pool, audit.clone(), 1, 3_600);

    // Первый запрос /64 разрешён, остальные отклоняются. Ротация младших бит
    // не должна давать отдельное событие на каждый адрес.
    for index in 0..5 {
        let forwarded = format!("2001:db8:1:2::{index:x}");
        assert_eq!(
            post_webhook_from(&app, "wrong-secret", &forwarded, b"{}").await,
            if index == 0 {
                StatusCode::UNAUTHORIZED
            } else {
                StatusCode::TOO_MANY_REQUESTS
            }
        );
    }

    let events = payloads(&db.pool, "rate_limited").await;
    assert_eq!(
        events.len(),
        1,
        "one /64 must produce one event: {events:?}"
    );
    assert_eq!(events[0]["limit"], WEBHOOK_LIMIT);
    assert_eq!(events[0]["ip"], "2001:db8:1:2::");
    assert_eq!(events[0]["suppressed"], 0);

    // Следующая запись после окна подавления сообщает, сколько повторов
    // подавлено, и остаётся в том же ключе /64.
    let next: IpAddr = "2001:db8:1:2:ffff::1".parse().expect("ip");
    let recorded = audit
        .rate_limited(
            RateLimitEvent {
                limit: WEBHOOK_LIMIT,
                key: LimitKey::Ip(ip_key(next)),
                retry_after: 1,
                household_id: None,
                actor: Actor::system(ActorType::System),
            },
            Utc::now() + Duration::seconds(SUPPRESSION_WINDOW_SECONDS + 1),
        )
        .await;
    assert!(recorded, "the first event after the window is written");

    let events = payloads(&db.pool, "rate_limited").await;
    assert_eq!(events.len(), 2, "{events:?}");
    assert_eq!(events[1]["ip"], "2001:db8:1:2::");
    assert_eq!(
        events[1]["suppressed"], 3,
        "repeats from the same /64 are aggregated, not written per address"
    );
}
