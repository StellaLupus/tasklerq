//! Журнал отказа авторизации слота: имя слота есть, токена и адреса нет.
//!
//! Отдельный тестовый бинарник, как `llm_logs.rs`: `tracing` кеширует интерес к
//! callsite, и параллельные тесты общего бинарника могли бы отключить его до
//! появления подписчика.

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Map, json};
use tasklerq::config::SecretString;
use tasklerq::llm::{
    Chain, ChatMessage, ChatRequest, OpenAiBackend, ResponseFailure, Slot, SystemClock,
};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

const TOKEN: &str = "llm-SECRET-token";
const ANSWER: &str = r#"{"items":[]}"#;

#[derive(Clone, Default)]
struct LogBuffer(Arc<Mutex<Vec<u8>>>);

impl LogBuffer {
    fn contents(&self) -> String {
        let bytes = self.0.lock().expect("log buffer lock").clone();
        String::from_utf8(bytes).expect("logs are UTF-8")
    }
}

impl Write for LogBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("log buffer lock")
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'writer> tracing_subscriber::fmt::MakeWriter<'writer> for LogBuffer {
    type Writer = Self;

    fn make_writer(&'writer self) -> Self::Writer {
        self.clone()
    }
}

#[derive(Debug, PartialEq, Eq)]
struct UserError;

fn accept(text: &str) -> Result<String, ResponseFailure<UserError>> {
    Ok(text.to_owned())
}

fn backend(server: &MockServer, model: &str, token: Option<&str>) -> Arc<OpenAiBackend> {
    Arc::new(
        OpenAiBackend::new(
            &format!("{}/v1", server.uri()),
            model,
            token.map(SecretString::new),
            Duration::from_millis(500),
            Map::new(),
        )
        .expect("backend"),
    )
}

#[test]
fn unauthorized_slot_is_logged_with_its_name_but_not_the_token_or_address() {
    let logs = LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_max_level(tracing::Level::ERROR)
        .with_writer(logs.clone())
        .finish();

    tracing::subscriber::with_default(subscriber, || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let primary_server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(401).set_body_string("rejected"))
                .mount(&primary_server)
                .await;

            let fallback_server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "model": "fallback/model",
                    "choices": [{"message": {"role": "assistant", "content": ANSWER}}],
                })))
                .mount(&fallback_server)
                .await;

            let primary = Slot::new(
                "primary",
                backend(&primary_server, "primary/model", Some(TOKEN)),
                Duration::from_secs(10),
            );
            let fallback = Slot::new(
                "fallback",
                backend(&fallback_server, "fallback/model", None),
                Duration::from_secs(10),
            );
            let chain = Chain::new(primary, Some(fallback));
            let schema = json!({"type": "object"});
            let messages = vec![ChatMessage::user("купить молоко")];
            let request = ChatRequest {
                messages: &messages,
                schema_name: "capture_v4",
                schema: &schema,
            };
            let clock = SystemClock::new();

            let outcome = chain
                .run(
                    &request,
                    Duration::from_secs(20),
                    chrono::Utc::now(),
                    &clock,
                    accept,
                )
                .await;

            assert_eq!(outcome.result, Ok(ANSWER.to_owned()));
            assert_eq!(outcome.attempts[0].error_code, Some("unauthorized"));
            assert!(outcome.attempts[1].is_success());
        });
    });

    let logs = logs.contents();
    assert!(logs.contains("\"level\":\"ERROR\""), "{logs}");
    assert!(logs.contains("primary"), "{logs}");
    assert!(!logs.contains(TOKEN), "{logs}");
    assert!(!logs.contains("127.0.0.1"), "{logs}");
}
