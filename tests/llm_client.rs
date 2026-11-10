//! Контракт `OpenAiBackend` с OpenAI-совместимым API на mock-сервере.

use std::net::TcpListener;
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};
use tasklerq::config::SecretString;
use tasklerq::llm::{
    ChatBackend, ChatMessage, ChatRequest, LlmError, MAX_COMPLETION_TOKENS, OpenAiBackend,
};
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TOKEN: &str = "llm-SECRET-token";
const MODEL: &str = "anthropic/claude-haiku-5.5";
const ANSWER: &str = r#"{"items":[]}"#;

fn backend(server: &MockServer) -> OpenAiBackend {
    OpenAiBackend::new(
        &format!("{}/v1", server.uri()),
        MODEL,
        Some(SecretString::new(TOKEN)),
        Duration::from_millis(500),
        Map::new(),
    )
    .expect("backend")
}

fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {"items": {"type": "array"}},
        "required": ["items"],
        "additionalProperties": false,
    })
}

fn messages() -> Vec<ChatMessage> {
    vec![
        ChatMessage::system("правила разбора"),
        ChatMessage::user("купить молоко"),
    ]
}

fn request<'a>(messages: &'a [ChatMessage], schema: &'a Value) -> ChatRequest<'a> {
    ChatRequest {
        messages,
        schema_name: "capture_v4",
        schema,
    }
}

fn answered(content: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "id": "chatcmpl-1",
        "model": "anthropic/claude-haiku-5.5-2026",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": content},
            "finish_reason": "stop",
        }],
        "usage": {"prompt_tokens": 123, "completion_tokens": 45, "total_tokens": 168},
    }))
}

fn response_format(schema: &Value) -> Value {
    json!({
        "type": "json_schema",
        "json_schema": {"name": "capture_v4", "strict": true, "schema": schema},
    })
}

#[tokio::test]
async fn request_carries_schema_temperature_and_extra_parameters() {
    let server = MockServer::start().await;
    let schema = schema();
    let messages = messages();
    let mut extra = Map::new();
    extra.insert("reasoning".to_owned(), json!({"effort": "minimal"}));

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", format!("Bearer {TOKEN}").as_str()))
        .and(body_partial_json(json!({
            "model": MODEL,
            "messages": [
                {"role": "system", "content": "правила разбора"},
                {"role": "user", "content": "купить молоко"},
            ],
            "response_format": response_format(&schema),
            "temperature": 0,
            "max_tokens": MAX_COMPLETION_TOKENS,
            "reasoning": {"effort": "minimal"},
        })))
        .respond_with(answered(ANSWER))
        .expect(1)
        .mount(&server)
        .await;

    let backend = OpenAiBackend::new(
        &format!("{}/v1", server.uri()),
        MODEL,
        Some(SecretString::new(TOKEN)),
        Duration::from_millis(500),
        extra,
    )
    .expect("backend");

    let completion = backend
        .complete(&request(&messages, &schema), Duration::from_secs(5))
        .await
        .expect("completion");

    assert_eq!(completion.content, ANSWER);
    assert_eq!(completion.input_tokens, Some(123));
    assert_eq!(completion.output_tokens, Some(45));
    assert_eq!(completion.model, "anthropic/claude-haiku-5.5-2026");
}

#[tokio::test]
async fn extra_parameters_cannot_replace_reserved_keys() {
    let server = MockServer::start().await;
    let schema = schema();
    let messages = messages();
    let mut extra = Map::new();
    extra.insert("model".to_owned(), json!("hijacked/model"));
    extra.insert(
        "messages".to_owned(),
        json!([{"role": "user", "content": "чужой текст"}]),
    );
    extra.insert("response_format".to_owned(), json!({"type": "text"}));
    extra.insert("temperature".to_owned(), json!(1));
    // `max_tokens` намеренно не зарезервирован: reasoning-моделям нужен запас,
    // и слот переопределяет верхнюю границу через `extra`.
    extra.insert("max_tokens".to_owned(), json!(2048));
    extra.insert("reasoning".to_owned(), json!({"effort": "minimal"}));

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_partial_json(json!({
            "model": MODEL,
            "messages": [
                {"role": "system", "content": "правила разбора"},
                {"role": "user", "content": "купить молоко"},
            ],
            "response_format": response_format(&schema),
            "temperature": 0,
            "max_tokens": 2048,
            "reasoning": {"effort": "minimal"},
        })))
        .respond_with(answered(ANSWER))
        .mount(&server)
        .await;

    let backend = OpenAiBackend::new(
        &format!("{}/v1", server.uri()),
        MODEL,
        Some(SecretString::new(TOKEN)),
        Duration::from_millis(500),
        extra,
    )
    .expect("backend");

    backend
        .complete(&request(&messages, &schema), Duration::from_secs(5))
        .await
        .expect("completion");
}

#[tokio::test]
async fn request_without_token_has_no_authorization_header() {
    let server = MockServer::start().await;
    let schema = schema();
    let messages = messages();

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(answered(ANSWER))
        .mount(&server)
        .await;

    let backend = OpenAiBackend::new(
        &format!("{}/v1", server.uri()),
        MODEL,
        None,
        Duration::from_millis(500),
        Map::new(),
    )
    .expect("backend");

    backend
        .complete(&request(&messages, &schema), Duration::from_secs(5))
        .await
        .expect("completion");

    let requests = server.received_requests().await.expect("requests");
    assert_eq!(requests.len(), 1);
    assert!(requests[0].headers.get("authorization").is_none());
}

#[tokio::test]
async fn request_with_token_uses_bearer_authorization() {
    let server = MockServer::start().await;
    let schema = schema();
    let messages = messages();

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(answered(ANSWER))
        .mount(&server)
        .await;

    backend(&server)
        .complete(&request(&messages, &schema), Duration::from_secs(5))
        .await
        .expect("completion");

    let requests = server.received_requests().await.expect("requests");
    assert_eq!(
        requests[0].headers.get("authorization").expect("header"),
        format!("Bearer {TOKEN}").as_str()
    );
}

#[tokio::test]
async fn base_url_with_trailing_slash_is_accepted() {
    let server = MockServer::start().await;
    let schema = schema();
    let messages = messages();

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(answered(ANSWER))
        .expect(1)
        .mount(&server)
        .await;

    let backend = OpenAiBackend::new(
        &format!("{}/v1/", server.uri()),
        MODEL,
        None,
        Duration::from_millis(500),
        Map::new(),
    )
    .expect("backend");

    backend
        .complete(&request(&messages, &schema), Duration::from_secs(5))
        .await
        .expect("completion");
}

#[tokio::test]
async fn http_statuses_map_to_distinct_error_codes() {
    let cases = [
        (401, "unauthorized"),
        (403, "unauthorized"),
        (429, "rate_limited"),
        (400, "rejected"),
        (422, "rejected"),
        (503, "server_error"),
    ];
    let schema = schema();
    let messages = messages();

    for (status, code) in cases {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(status).set_body_string("тело с текстом пользователя"),
            )
            .mount(&server)
            .await;

        let error = backend(&server)
            .complete(&request(&messages, &schema), Duration::from_secs(5))
            .await
            .expect_err("error");

        assert_eq!(error.code(), code, "status {status}");
    }
}

#[tokio::test]
async fn connection_refused_is_reported_as_transport() {
    // Порт, на котором никто не слушает: занимаем и сразу освобождаем.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("addr");
    drop(listener);

    let schema = schema();
    let messages = messages();
    let backend = OpenAiBackend::new(
        &format!("http://{address}/v1"),
        MODEL,
        Some(SecretString::new(TOKEN)),
        Duration::from_millis(500),
        Map::new(),
    )
    .expect("backend");

    let error = backend
        .complete(&request(&messages, &schema), Duration::from_secs(2))
        .await
        .expect_err("error");

    assert_eq!(error.code(), "transport");
    let rendered = format!("{error} {error:?}");
    assert!(!rendered.contains(TOKEN));
    assert!(!rendered.contains("127.0.0.1"));
}

#[tokio::test]
async fn connection_closed_without_a_response_is_reported_as_transport() {
    // Сервер принимает соединение, читает начало запроса и закрывает сокет
    // без ответа: это обрыв, а не отказ в соединении.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("addr");
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.expect("accept");
        socket.readable().await.expect("request bytes");
        let mut buffer = [0u8; 1024];
        let _ = socket.try_read(&mut buffer);
        drop(socket);
    });

    let schema = schema();
    let messages = messages();
    let backend = OpenAiBackend::new(
        &format!("http://{address}/v1"),
        MODEL,
        Some(SecretString::new(TOKEN)),
        Duration::from_millis(500),
        Map::new(),
    )
    .expect("backend");

    let error = backend
        .complete(&request(&messages, &schema), Duration::from_secs(2))
        .await
        .expect_err("error");

    server.await.expect("server task");
    assert_eq!(error.code(), "transport");
}

#[tokio::test]
async fn slow_response_times_out_at_requested_limit() {
    let server = MockServer::start().await;
    let schema = schema();
    let messages = messages();

    Mock::given(method("POST"))
        .respond_with(answered(ANSWER).set_delay(Duration::from_secs(3)))
        .mount(&server)
        .await;

    let started = Instant::now();
    let error = backend(&server)
        .complete(&request(&messages, &schema), Duration::from_millis(200))
        .await
        .expect_err("error");

    assert_eq!(error.code(), "timeout");
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn empty_or_malformed_response_is_reported_as_empty_response() {
    let bodies = [
        json!({"choices": [{"message": {"role": "assistant", "content": ""}}]}),
        json!({"choices": [{"message": {"role": "assistant", "content": "   "}}]}),
        json!({"choices": [{"message": {"role": "assistant"}}]}),
        json!({"choices": []}),
        json!({"model": "x"}),
    ];
    let schema = schema();
    let messages = messages();

    for body in bodies {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body.clone()))
            .mount(&server)
            .await;

        let error = backend(&server)
            .complete(&request(&messages, &schema), Duration::from_secs(5))
            .await
            .expect_err("error");

        assert_eq!(error.code(), "empty_response", "body {body}");
    }
}

#[tokio::test]
async fn response_without_usage_has_no_token_counts() {
    let server = MockServer::start().await;
    let schema = schema();
    let messages = messages();

    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "model": "local/qwen",
            "choices": [{"message": {"role": "assistant", "content": ANSWER}}],
        })))
        .mount(&server)
        .await;

    let completion = backend(&server)
        .complete(&request(&messages, &schema), Duration::from_secs(5))
        .await
        .expect("completion");

    assert_eq!(completion.content, ANSWER);
    assert_eq!(completion.input_tokens, None);
    assert_eq!(completion.output_tokens, None);
    assert_eq!(completion.model, "local/qwen");
}

#[tokio::test]
async fn wrong_usage_types_do_not_discard_a_valid_answer() {
    let usages = [
        json!({"prompt_tokens": "12", "completion_tokens": -1}),
        json!({"prompt_tokens": -1, "completion_tokens": 12.5}),
    ];
    let schema = schema();
    let messages = messages();

    for usage in usages {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "model": "local/qwen",
                "choices": [{"message": {"role": "assistant", "content": ANSWER}}],
                "usage": usage,
            })))
            .mount(&server)
            .await;

        let completion = backend(&server)
            .complete(&request(&messages, &schema), Duration::from_secs(5))
            .await
            .expect("valid answer with bad usage is accepted");

        assert_eq!(completion.content, ANSWER);
        assert_eq!(completion.input_tokens, None, "usage {usage}");
        assert_eq!(completion.output_tokens, None, "usage {usage}");
    }
}

#[test]
fn error_codes_are_stable() {
    assert_eq!(LlmError::Transport.code(), "transport");
    assert_eq!(LlmError::Timeout.code(), "timeout");
    assert_eq!(LlmError::Unauthorized.code(), "unauthorized");
    assert_eq!(LlmError::RateLimited.code(), "rate_limited");
    assert_eq!(LlmError::Rejected(400).code(), "rejected");
    assert_eq!(LlmError::Server(503).code(), "server_error");
    assert_eq!(LlmError::EmptyResponse.code(), "empty_response");
}
