//! Контракт `HttpAiTask` с Home Assistant REST на mock-сервере.

use std::time::{Duration, Instant};

use serde_json::json;
use tasklerq::config::SecretString;
use tasklerq::home_assistant::{AiTask, GenerateData, HaError, HttpAiTask};
use wiremock::matchers::{body_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TOKEN: &str = "ha-long-lived-SECRET";
const ENTITY: &str = "ai_task.google_ai_task_2";

fn client(server: &MockServer) -> HttpAiTask {
    HttpAiTask::new(&server.uri(), SecretString::new(TOKEN), ENTITY).expect("client")
}

fn structure() -> serde_json::Value {
    json!({"items": {"selector": {"object": {"multiple": true}}}})
}

fn request(structure: &serde_json::Value) -> GenerateData<'_> {
    GenerateData {
        task_name: "Разбор заметки Tasklerq",
        instructions: "инструкции",
        structure,
    }
}

#[tokio::test]
async fn generate_data_posts_bearer_request_and_returns_data() {
    let server = MockServer::start().await;
    let structure = structure();
    Mock::given(method("POST"))
        .and(path("/api/services/ai_task/generate_data"))
        .and(query_param("return_response", ""))
        .and(header("authorization", format!("Bearer {TOKEN}").as_str()))
        .and(body_json(json!({
            "entity_id": ENTITY,
            "task_name": "Разбор заметки Tasklerq",
            "instructions": "инструкции",
            "structure": structure,
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "changed_states": [],
            "service_response": {
                "conversation_id": "c1",
                "data": {"items": [{"title": "Молоко"}]}
            }
        })))
        .expect(1)
        .mount(&server)
        .await;

    let data = client(&server)
        .generate_data(&request(&structure), Duration::from_secs(5))
        .await
        .expect("data");

    assert_eq!(data, json!({"items": [{"title": "Молоко"}]}));
}

#[tokio::test]
async fn base_url_with_path_prefix_and_trailing_slash_is_supported() {
    let server = MockServer::start().await;
    let structure = structure();
    Mock::given(method("POST"))
        .and(path("/ha/api/services/ai_task/generate_data"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "service_response": {"data": {"items": []}}
        })))
        .expect(1)
        .mount(&server)
        .await;

    let base = format!("{}/ha/", server.uri());
    let client = HttpAiTask::new(&base, SecretString::new(TOKEN), ENTITY).expect("client");
    client
        .generate_data(&request(&structure), Duration::from_secs(5))
        .await
        .expect("data");
}

#[tokio::test]
async fn http_statuses_map_to_errors() {
    let cases = [
        (401, HaError::Unauthorized),
        (403, HaError::Unauthorized),
        (404, HaError::EntityUnavailable),
        (400, HaError::Rejected(400)),
        (503, HaError::Server(503)),
    ];
    for (status, expected) in cases {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(status).set_body_string("тело с текстом пользователя"),
            )
            .mount(&server)
            .await;
        let structure = structure();

        let error = client(&server)
            .generate_data(&request(&structure), Duration::from_secs(5))
            .await
            .expect_err("error");

        assert_eq!(error, expected, "status {status}");
    }
}

#[tokio::test]
async fn malformed_success_body_is_bad_response() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"changed_states": []})))
        .mount(&server)
        .await;
    let structure = structure();

    let error = client(&server)
        .generate_data(&request(&structure), Duration::from_secs(5))
        .await
        .expect_err("error");

    assert_eq!(error, HaError::BadResponse);
}

#[tokio::test]
async fn slow_response_times_out_at_requested_limit() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"service_response": {"data": {}}}))
                .set_delay(Duration::from_secs(3)),
        )
        .mount(&server)
        .await;
    let structure = structure();

    let started = Instant::now();
    let error = client(&server)
        .generate_data(&request(&structure), Duration::from_millis(200))
        .await
        .expect_err("error");

    assert_eq!(error, HaError::Timeout);
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn redirects_are_not_followed() {
    let server = MockServer::start().await;
    let elsewhere = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(302).insert_header("location", elsewhere.uri().as_str()),
        )
        .mount(&server)
        .await;
    let structure = structure();

    let error = client(&server)
        .generate_data(&request(&structure), Duration::from_secs(5))
        .await
        .expect_err("error");

    assert_eq!(error, HaError::Rejected(302));
    assert!(elsewhere.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn unreachable_home_assistant_is_reported_without_url_or_token() {
    // Порт, на котором никто не слушает: занимаем и сразу освобождаем.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("addr");
    drop(listener);
    let client = HttpAiTask::new(
        &format!("http://{address}"),
        SecretString::new(TOKEN),
        ENTITY,
    )
    .expect("client");
    let structure = structure();

    let error = client
        .generate_data(&request(&structure), Duration::from_secs(2))
        .await
        .expect_err("error");

    assert_eq!(error, HaError::Unreachable);
    assert!(error.is_retryable());
    let rendered = format!("{error} {error:?}");
    assert!(!rendered.contains(TOKEN));
    assert!(!rendered.contains("127.0.0.1"));
    assert_eq!(client.check_entity().await, Err(HaError::Unreachable));
}

#[tokio::test]
async fn check_entity_accepts_known_and_unknown_state() {
    for state in ["2026-10-30T10:00:00+00:00", "unknown"] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/api/states/{ENTITY}")))
            .and(header("authorization", format!("Bearer {TOKEN}").as_str()))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "entity_id": ENTITY,
                "state": state,
                "attributes": {}
            })))
            .expect(1)
            .mount(&server)
            .await;

        client(&server).check_entity().await.expect("available");
    }
}

#[tokio::test]
async fn check_entity_rejects_missing_unavailable_and_unauthorized() {
    let cases = [
        (
            404,
            json!({"message": "Entity not found."}),
            HaError::EntityUnavailable,
        ),
        (
            200,
            json!({"state": "unavailable"}),
            HaError::EntityUnavailable,
        ),
        (
            401,
            json!({"message": "Unauthorized"}),
            HaError::Unauthorized,
        ),
    ];
    for (status, body, expected) in cases {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(status).set_body_json(body))
            .mount(&server)
            .await;

        assert_eq!(client(&server).check_entity().await, Err(expected));
    }
}

#[test]
fn foreign_entity_is_rejected_at_construction() {
    let result = HttpAiTask::new(
        "http://ha.local:8123",
        SecretString::new(TOKEN),
        "light.kitchen",
    );
    assert!(matches!(result, Err(HaError::Config(_))));
}
