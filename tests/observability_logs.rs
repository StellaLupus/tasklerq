//! Логи Telegram и HTTP: идентификаторы и результат есть, текста сообщения нет.
//!
//! Отдельный тестовый бинарник, как `llm_logs.rs`: `tracing` кеширует интерес к
//! callsite, и параллельные тесты общего бинарника могли бы отключить его до
//! появления подписчика.

mod support;

use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt;
use serde_json::Value;
use tasklerq::http::router::{AppState, build_router};
use tasklerq::security_audit::Audit;
use tower::ServiceExt;

use support::logs::LogBuffer;
use support::next_telegram_id;
use support::telegram::Harness;

fn records(captured: &str) -> Vec<Value> {
    captured
        .lines()
        .filter_map(|line| serde_json::from_str(line.trim()).ok())
        .collect()
}

fn record<'a>(records: &'a [Value], message: &str) -> &'a Value {
    records
        .iter()
        .find(|record| record["fields"]["message"].as_str() == Some(message))
        .unwrap_or_else(|| panic!("log record '{message}' is missing:\n{records:#?}"))
}

#[test]
fn logs_keep_identifiers_and_results_but_not_the_message_text() {
    let logs = LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(logs.clone())
        .finish();

    let mut update_id = 0;
    tracing::subscriber::with_default(subscriber, || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let db = support::db().await;
            let chat = next_telegram_id();
            let mut bot = Harness::new(&db.pool, chat);
            let update = bot.text_update("Ann", "Купить лекарство");
            update_id = update["update_id"]
                .as_i64()
                .expect("the fake update has an id");
            assert!(update_id > 0);
            bot.enqueue(update).await;
            bot.pump().await;

            let app = build_router(AppState::new(db.pool.clone(), Audit::new(db.pool.clone())));
            let response = app
                .oneshot(
                    Request::builder()
                        .uri("/health/live")
                        .header("x-request-id", "req-observability")
                        .body(Body::empty())
                        .expect("request"),
                )
                .await
                .expect("response");
            let _ = response.into_body().collect().await.expect("body");
        });
    });

    let captured = logs.contents();
    let records = records(&captured);

    let update = record(&records, "telegram update finished");
    assert_eq!(
        update["fields"]["update_id"].as_i64(),
        Some(update_id),
        "the update log must contain update_id:\n{captured}"
    );
    assert_eq!(
        update["fields"]["status"].as_str(),
        Some("processed"),
        "the update log must contain the result:\n{captured}"
    );
    assert!(
        update["fields"]["duration_ms"].as_u64().is_some(),
        "the update log must contain duration_ms:\n{captured}"
    );

    let http = record(&records, "request completed");
    assert_eq!(
        http["fields"]["request_id"].as_str(),
        Some("req-observability"),
        "the HTTP log must contain request_id:\n{captured}"
    );
    assert_eq!(
        http["fields"]["route"].as_str(),
        Some("/health/live"),
        "the HTTP log must contain the matched route:\n{captured}"
    );
    assert_eq!(
        http["fields"]["status"].as_u64(),
        Some(200),
        "the HTTP log must contain the status:\n{captured}"
    );
    assert!(
        http["fields"]["duration_ms"].as_u64().is_some(),
        "the HTTP log must contain duration_ms:\n{captured}"
    );

    assert!(
        !captured.contains("Купить лекарство"),
        "the message text must not reach any log line:\n{captured}"
    );
}
