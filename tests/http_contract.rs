//! Contract-тесты единого формата ошибок и request ID.

use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use chrono::{DateTime, TimeZone, Utc};
use http_body_util::BodyExt;
use serde_json::Value;
use sqlx::postgres::PgPoolOptions;
use tasklerq::domain::models::{Item, ItemPriority, ItemSource, ItemStatus};
use tasklerq::http::api::dto::ItemResponse;
use tasklerq::http::router::{AppState, build_router};
use tasklerq::security_audit::Audit;
use tower::ServiceExt;
use uuid::Uuid;

fn test_app() -> Router {
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_millis(100))
        .connect_lazy("postgres://tasklerq:password@127.0.0.1:1/tasklerq")
        .expect("lazy pool should be valid");
    build_router(AppState::new(pool.clone(), Audit::new(pool)))
}

async fn send(request: Request<Body>) -> (StatusCode, HeaderMap, Value) {
    let response = test_app()
        .oneshot(request)
        .await
        .expect("router should respond");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body should be readable")
        .to_bytes();
    let json = serde_json::from_slice(&bytes).expect("response should be JSON");
    (status, headers, json)
}

#[tokio::test]
async fn unknown_route_uses_error_envelope_and_echoes_request_id() {
    let request = Request::builder()
        .uri("/missing")
        .header("x-request-id", "req-123")
        .body(Body::empty())
        .expect("request should build");

    let (status, headers, json) = send(request).await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        headers
            .get("x-request-id")
            .and_then(|value| value.to_str().ok()),
        Some("req-123")
    );
    assert_eq!(json["error"]["code"], "not_found");
    assert!(json["error"]["message"].is_string());
    assert!(json["error"]["details"].is_object());
    assert_eq!(json["error"]["request_id"], "req-123");
}

#[tokio::test]
async fn request_id_is_generated_when_missing() {
    let request = Request::builder()
        .uri("/missing")
        .body(Body::empty())
        .expect("request should build");

    let (status, headers, json) = send(request).await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    let header = headers
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .expect("request id header should be present");
    assert!(Uuid::parse_str(header).is_ok(), "generated id must be UUID");
    assert_eq!(json["error"]["request_id"], header);
}

#[tokio::test]
async fn unsafe_request_id_is_replaced() {
    let request = Request::builder()
        .uri("/missing")
        .header("x-request-id", "bad id with spaces")
        .body(Body::empty())
        .expect("request should build");

    let (_, headers, json) = send(request).await;

    let header = headers
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .expect("request id header should be present");
    assert_ne!(header, "bad id with spaces");
    assert!(Uuid::parse_str(header).is_ok());
    assert_eq!(json["error"]["request_id"], header);
}

#[tokio::test]
async fn method_not_allowed_uses_error_envelope() {
    let request = Request::builder()
        .method("POST")
        .uri("/health/live")
        .body(Body::empty())
        .expect("request should build");

    let (status, _, json) = send(request).await;

    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(json["error"]["code"], "method_not_allowed");
    assert!(json["error"]["request_id"].is_string());
}

#[tokio::test]
async fn health_live_returns_version_and_time() {
    let request = Request::builder()
        .uri("/health/live")
        .body(Body::empty())
        .expect("request should build");

    let (status, headers, json) = send(request).await;

    assert_eq!(status, StatusCode::OK);
    assert!(headers.get("x-request-id").is_some());
    assert_eq!(json["status"], "ok");
    assert_eq!(json["version"], env!("CARGO_PKG_VERSION"));
    let time = json["time"].as_str().expect("time should be a string");
    DateTime::parse_from_rfc3339(time).expect("time should be RFC 3339");
}

#[tokio::test]
async fn health_ready_reports_unavailable_database() {
    let request = Request::builder()
        .uri("/health/ready")
        .body(Body::empty())
        .expect("request should build");

    let (status, _, json) = send(request).await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(json["error"]["code"], "service_unavailable");
    assert!(json["error"]["request_id"].is_string());
    assert!(
        !json.to_string().contains("password"),
        "error must not leak the database URL"
    );
}

#[test]
fn item_object_exposes_event_end_fields() {
    let moment = |hour: u32, minute: u32| {
        Utc.with_ymd_and_hms(2026, 11, 11, hour, minute, 0)
            .single()
            .expect("fixed moment")
    };
    let now = moment(10, 0);
    let item = Item {
        id: Uuid::nil(),
        household_id: Uuid::nil(),
        list_id: Uuid::nil(),
        title: "Маникюр".into(),
        notes: None,
        status: ItemStatus::Open,
        priority: ItemPriority::Normal,
        due_at: Some(moment(14, 0)),
        due_date: None,
        end_at: Some(moment(15, 30)),
        end_date: None,
        assignee_user_id: None,
        created_by_user_id: None,
        source: ItemSource::Api,
        capture_id: None,
        completed_at: None,
        completed_by_user_id: None,
        quantity: None,
        unit: None,
        position: 0,
        version: 1,
        recurrence: None,
        created_at: now,
        updated_at: now,
        deleted_at: None,
        tags: Vec::new(),
        subtasks: Vec::new(),
    };

    let json = serde_json::to_value(ItemResponse::from(item)).expect("item response serializes");

    assert_eq!(json["due_at"], "2026-11-11T14:00:00Z");
    assert_eq!(json["end_at"], "2026-11-11T15:30:00Z");
    assert_eq!(json["end_date"], Value::Null);
    assert!(
        json.as_object()
            .expect("item object")
            .contains_key("end_date"),
        "the Item object must always expose end_date"
    );
}
