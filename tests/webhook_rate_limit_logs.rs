//! Журнал и метрика отклонённых по лимиту webhook-запросов.
//!
//! Отдельный тестовый бинарник: метрики — глобальные счётчики процесса, а
//! `tracing` кеширует интерес к callsite; один тест не искажает дельту и не
//! делит подписчика с другими сценариями.

mod support;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tasklerq::config::SecretString;
use tasklerq::http::router::{AppState, build_router};
use tasklerq::metrics;
use tasklerq::security_audit::Audit;
use tasklerq::telegram::webhook::Ingress;
use tokio::sync::Notify;
use tower::ServiceExt;

use support::isolated_db;

const SECRET: &str = "configured-webhook-SECRET-value";
const WARNING: &str = "webhook requests from one IP are rate limited";

fn peer() -> SocketAddr {
    SocketAddr::from(([172, 18, 0, 1], 4040))
}

fn app_with_limit(pool: &sqlx::PgPool, limit: u32, window_seconds: i64) -> Router {
    let ingress = Ingress::with_rate_limit(
        &SecretString::new(SECRET),
        Arc::new(Notify::new()),
        limit,
        window_seconds,
    );
    build_router(AppState::new(pool.clone(), Audit::new(pool.clone())).with_telegram(ingress))
        .layer(MockConnectInfo(peer()))
}

async fn post_webhook_from(app: &Router, forwarded: &str) -> StatusCode {
    app.clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/webhooks/telegram")
                .header("x-forwarded-for", forwarded)
                .body(Body::from(b"{}".to_vec()))
                .expect("request"),
        )
        .await
        .expect("response")
        .status()
}

/// Писатель журнала в память.
#[derive(Clone, Default)]
struct LogBuffer(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LogBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("lock").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuffer {
    type Writer = LogBuffer;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

fn warnings(captured: &str) -> usize {
    captured
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
        .filter(|record| record["level"] == "WARN" && record["fields"]["message"] == *WARNING)
        .count()
}

fn counter(text: &str, name: &str) -> u64 {
    let prefix = format!("{name} ");
    text.lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(0)
}

#[tokio::test]
async fn a_rejected_webhook_flood_logs_one_warning_and_counts_every_request() {
    let db = isolated_db().await;
    let logs = LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(logs.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let app = app_with_limit(&db.pool, 1, 3_600);
    let before = counter(&metrics::render(), "webhook_rate_limited_total");

    // Первый запрос расходует лимит, остальные 500 отклоняются.
    assert_eq!(
        post_webhook_from(&app, "203.0.113.7").await,
        StatusCode::UNAUTHORIZED
    );
    for _ in 0..500 {
        assert_eq!(
            post_webhook_from(&app, "203.0.113.7").await,
            StatusCode::TOO_MANY_REQUESTS
        );
    }

    let captured = String::from_utf8(logs.0.lock().expect("lock").clone()).expect("utf8");
    assert_eq!(
        warnings(&captured),
        1,
        "only the recorded audit event may log a warning:\n{captured}"
    );
    assert_eq!(
        counter(&metrics::render(), "webhook_rate_limited_total") - before,
        500,
        "the metric counts every rejected request, including suppressed ones"
    );
}
