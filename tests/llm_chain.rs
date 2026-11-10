//! Цепочка слотов: бюджет времени, переход на резервный слот и предохранитель.
//!
//! Часы подменены: двойники бэкендов двигают [`FakeClock`], поэтому тесты не
//! ждут реальные секунды бюджета.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::{Map, Value, json};
use tasklerq::capture::breaker::DEFAULT_THRESHOLD;
use tasklerq::config::{LlmConfig, SecretString, SlotConfig};
use tasklerq::llm::{
    Chain, ChainError, ChatBackend, ChatCompletion, ChatMessage, ChatRequest, Clock, LlmError,
    ResponseFailure, Slot,
};

const ANSWER: &str = r#"{"items":[]}"#;

/// Часы, которые двигают тесты, а не реальное время.
#[derive(Default)]
struct FakeClock(Mutex<Duration>);

impl FakeClock {
    fn advance(&self, by: Duration) {
        let mut elapsed = self.0.lock().expect("clock lock");
        *elapsed += by;
    }
}

impl Clock for FakeClock {
    fn elapsed(&self) -> Duration {
        *self.0.lock().expect("clock lock")
    }
}

enum Reply {
    Content(&'static str),
    Error(LlmError),
}

/// Двойник слота: запоминает переданный таймаут, двигает часы и отвечает.
struct StubBackend {
    clock: Arc<FakeClock>,
    delay: Duration,
    reply: Reply,
    calls: Mutex<Vec<Duration>>,
}

impl StubBackend {
    fn new(clock: &Arc<FakeClock>, delay: Duration, reply: Reply) -> Self {
        Self {
            clock: clock.clone(),
            delay,
            reply,
            calls: Mutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> Vec<Duration> {
        self.calls.lock().expect("calls lock").clone()
    }
}

#[async_trait]
impl ChatBackend for StubBackend {
    async fn complete(
        &self,
        _request: &ChatRequest<'_>,
        timeout: Duration,
    ) -> Result<ChatCompletion, LlmError> {
        self.calls.lock().expect("calls lock").push(timeout);
        self.clock.advance(self.delay);
        match &self.reply {
            Reply::Content(content) => Ok(ChatCompletion {
                content: (*content).to_owned(),
                input_tokens: Some(10),
                output_tokens: Some(4),
                model: "stub/model".to_owned(),
            }),
            Reply::Error(error) => Err(error.clone()),
        }
    }
}

/// Двойник, который действительно спит: нужен только внешнему таймауту.
struct SlowBackend(Duration);

#[async_trait]
impl ChatBackend for SlowBackend {
    async fn complete(
        &self,
        _request: &ChatRequest<'_>,
        _timeout: Duration,
    ) -> Result<ChatCompletion, LlmError> {
        tokio::time::sleep(self.0).await;
        Ok(ChatCompletion {
            content: ANSWER.to_owned(),
            input_tokens: None,
            output_tokens: None,
            model: String::new(),
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
struct UserError(&'static str);

fn accept(text: &str) -> Result<String, ResponseFailure<UserError>> {
    Ok(text.to_owned())
}

fn shape(text: &str) -> Result<String, ResponseFailure<UserError>> {
    if text.trim_start().starts_with('{') {
        return Ok(text.to_owned());
    }
    Err(ResponseFailure::Shape {
        code: "invalid_field",
    })
}

fn too_many(_text: &str) -> Result<String, ResponseFailure<UserError>> {
    Err(ResponseFailure::User {
        code: "too_many_items",
        error: UserError("too many items"),
    })
}

fn slot(
    name: &'static str,
    clock: &Arc<FakeClock>,
    timeout: Duration,
    delay: Duration,
    reply: Reply,
) -> (Slot, Arc<StubBackend>) {
    let backend = Arc::new(StubBackend::new(clock, delay, reply));
    (Slot::new(name, backend.clone(), timeout), backend)
}

fn now() -> DateTime<Utc> {
    DateTime::from_timestamp(1_800_000_000, 0).expect("timestamp")
}

fn schema() -> Value {
    json!({"type": "object"})
}

fn messages() -> Vec<ChatMessage> {
    vec![ChatMessage::user("купить молоко")]
}

fn request<'a>(messages: &'a [ChatMessage], schema: &'a Value) -> ChatRequest<'a> {
    ChatRequest {
        messages,
        schema_name: "capture_v4",
        schema,
    }
}

#[test]
fn chain_is_built_from_slot_configs() {
    let slot_config = |url: &str, model: &str, timeout: Duration| SlotConfig {
        url: url.parse().expect("url"),
        model: model.to_owned(),
        token: Some(SecretString::new("slot-token")),
        timeout,
        extra: Map::new(),
    };
    let config = LlmConfig {
        enabled: true,
        primary: Some(slot_config(
            "https://primary.example/v1",
            "primary/model",
            Duration::from_secs(3),
        )),
        fallback: Some(slot_config(
            "http://fallback.example/v1",
            "fallback/model",
            Duration::from_secs(4),
        )),
        connect_timeout: Duration::from_millis(500),
        daily_quota: 200,
    };

    let chain = Chain::from_config(&config)
        .expect("slots should build")
        .expect("an enabled config with a primary slot builds a chain");
    assert_eq!(chain.primary().name(), "primary");
    assert_eq!(chain.primary().timeout(), Duration::from_secs(3));
    assert_eq!(
        chain.fallback().map(Slot::timeout),
        Some(Duration::from_secs(4))
    );

    let disabled = LlmConfig {
        enabled: false,
        ..config
    };
    assert!(
        Chain::from_config(&disabled)
            .expect("a disabled config builds no chain")
            .is_none()
    );
}

#[tokio::test]
async fn voice_budget_gives_the_fallback_the_rest() {
    let clock = Arc::new(FakeClock::default());
    // Бюджет 8 с: основной получает 4 с и не отвечает, резервному остаётся 4 с.
    let (primary, primary_backend) = slot(
        "primary",
        &clock,
        Duration::from_secs(10),
        Duration::from_secs(4),
        Reply::Error(LlmError::Timeout),
    );
    let (fallback, fallback_backend) = slot(
        "fallback",
        &clock,
        Duration::from_secs(10),
        Duration::from_secs(1),
        Reply::Content(ANSWER),
    );
    let chain = Chain::new(primary, Some(fallback));
    let schema = schema();
    let messages = messages();

    let outcome = chain
        .run(
            &request(&messages, &schema),
            Duration::from_secs(8),
            now(),
            clock.as_ref(),
            accept,
        )
        .await;

    assert_eq!(outcome.result, Ok(ANSWER.to_owned()));
    assert_eq!(primary_backend.calls(), vec![Duration::from_secs(4)]);
    assert_eq!(fallback_backend.calls(), vec![Duration::from_secs(4)]);
    assert_eq!(outcome.attempts.len(), 2);

    assert_eq!(outcome.attempts[0].slot, "primary");
    assert_eq!(outcome.attempts[0].number, 1);
    assert_eq!(outcome.attempts[0].error_code, Some("timeout"));
    assert_eq!(outcome.attempts[0].duration, Duration::from_secs(4));

    assert_eq!(outcome.attempts[1].slot, "fallback");
    assert_eq!(outcome.attempts[1].number, 2);
    assert!(outcome.attempts[1].is_success());
    assert_eq!(outcome.attempts[1].input_tokens, Some(10));
    assert_eq!(outcome.attempts[1].output_tokens, Some(4));
    assert_eq!(outcome.attempts[1].model.as_deref(), Some("stub/model"));
}

#[tokio::test]
async fn fast_primary_error_leaves_the_rest_of_the_budget() {
    let clock = Arc::new(FakeClock::default());
    let (primary, _) = slot(
        "primary",
        &clock,
        Duration::from_secs(10),
        Duration::from_secs(1),
        Reply::Error(LlmError::Server(503)),
    );
    // Таймаут резервного больше остатка: действует остаток бюджета 19 с.
    let (fallback, fallback_backend) = slot(
        "fallback",
        &clock,
        Duration::from_secs(30),
        Duration::from_secs(1),
        Reply::Content(ANSWER),
    );
    let chain = Chain::new(primary, Some(fallback));
    let schema = schema();
    let messages = messages();

    let outcome = chain
        .run(
            &request(&messages, &schema),
            Duration::from_secs(20),
            now(),
            clock.as_ref(),
            accept,
        )
        .await;

    assert_eq!(outcome.result, Ok(ANSWER.to_owned()));
    assert_eq!(fallback_backend.calls(), vec![Duration::from_secs(19)]);
}

#[tokio::test]
async fn unreachable_primary_gives_the_remaining_budget_to_the_fallback() {
    let clock = Arc::new(FakeClock::default());
    // Выключенный хост: попытка прекращается по таймауту соединения (1 с).
    let (primary, _) = slot(
        "primary",
        &clock,
        Duration::from_secs(10),
        Duration::from_secs(1),
        Reply::Error(LlmError::Transport),
    );
    let (fallback, fallback_backend) = slot(
        "fallback",
        &clock,
        Duration::from_secs(30),
        Duration::from_secs(1),
        Reply::Content(ANSWER),
    );
    let chain = Chain::new(primary, Some(fallback));
    let schema = schema();
    let messages = messages();

    let outcome = chain
        .run(
            &request(&messages, &schema),
            Duration::from_secs(20),
            now(),
            clock.as_ref(),
            accept,
        )
        .await;

    assert_eq!(outcome.result, Ok(ANSWER.to_owned()));
    assert_eq!(fallback_backend.calls(), vec![Duration::from_secs(19)]);
}

#[tokio::test]
async fn invalid_primary_response_calls_the_fallback_once() {
    let clock = Arc::new(FakeClock::default());
    let (primary, primary_backend) = slot(
        "primary",
        &clock,
        Duration::from_secs(10),
        Duration::from_secs(2),
        Reply::Content("не JSON по схеме"),
    );
    let (fallback, fallback_backend) = slot(
        "fallback",
        &clock,
        Duration::from_secs(10),
        Duration::from_secs(1),
        Reply::Content(ANSWER),
    );
    let chain = Chain::new(primary, Some(fallback));
    let schema = schema();
    let messages = messages();

    let outcome = chain
        .run(
            &request(&messages, &schema),
            Duration::from_secs(20),
            now(),
            clock.as_ref(),
            shape,
        )
        .await;

    assert_eq!(outcome.result, Ok(ANSWER.to_owned()));
    assert_eq!(
        primary_backend.calls().len(),
        1,
        "the same slot must not be called twice"
    );
    assert_eq!(fallback_backend.calls().len(), 1);
    assert_eq!(outcome.attempts[0].error_code, Some("invalid_field"));
    assert!(outcome.attempts[1].is_success());
}

#[tokio::test]
async fn unauthorized_primary_falls_back() {
    let clock = Arc::new(FakeClock::default());
    let (primary, _) = slot(
        "primary",
        &clock,
        Duration::from_secs(10),
        Duration::from_millis(200),
        Reply::Error(LlmError::Unauthorized),
    );
    let (fallback, fallback_backend) = slot(
        "fallback",
        &clock,
        Duration::from_secs(10),
        Duration::from_secs(1),
        Reply::Content(ANSWER),
    );
    let chain = Chain::new(primary, Some(fallback));
    let schema = schema();
    let messages = messages();

    let outcome = chain
        .run(
            &request(&messages, &schema),
            Duration::from_secs(20),
            now(),
            clock.as_ref(),
            accept,
        )
        .await;

    assert_eq!(outcome.result, Ok(ANSWER.to_owned()));
    assert_eq!(outcome.attempts[0].error_code, Some("unauthorized"));
    assert_eq!(fallback_backend.calls().len(), 1);
}

#[tokio::test]
async fn too_many_items_does_not_fall_back_and_does_not_open_the_breaker() {
    // Разборщик сообщает, что слот вернул 25 элементов при лимите 20: это
    // ошибка пользователя, а не сбой слота.
    let clock = Arc::new(FakeClock::default());
    let (primary, primary_backend) = slot(
        "primary",
        &clock,
        Duration::from_secs(10),
        Duration::from_secs(1),
        Reply::Content(ANSWER),
    );
    let (fallback, fallback_backend) = slot(
        "fallback",
        &clock,
        Duration::from_secs(10),
        Duration::from_secs(1),
        Reply::Content(ANSWER),
    );
    let chain = Chain::new(primary, Some(fallback));
    let schema = schema();
    let messages = messages();
    let request = request(&messages, &schema);

    for _ in 0..5 {
        let outcome = chain
            .run(
                &request,
                Duration::from_secs(20),
                now(),
                clock.as_ref(),
                too_many,
            )
            .await;

        assert_eq!(
            outcome.result,
            Err(ChainError::User(UserError("too many items")))
        );
        assert_eq!(outcome.attempts.len(), 1);
        assert_eq!(outcome.attempts[0].error_code, Some("too_many_items"));
    }

    assert!(
        fallback_backend.calls().is_empty(),
        "a user error must not switch to the fallback slot"
    );
    assert_eq!(primary_backend.calls().len(), 5);
    assert!(
        chain.primary().breaker().retry_after(now()).is_none(),
        "a user error counts as a successful call for the breaker"
    );
}

#[tokio::test]
async fn both_slots_failing_degrades_with_the_last_reason() {
    let clock = Arc::new(FakeClock::default());
    let (primary, _) = slot(
        "primary",
        &clock,
        Duration::from_secs(10),
        Duration::from_secs(4),
        Reply::Error(LlmError::Timeout),
    );
    let (fallback, _) = slot(
        "fallback",
        &clock,
        Duration::from_secs(10),
        Duration::from_secs(1),
        Reply::Error(LlmError::Server(503)),
    );
    let chain = Chain::new(primary, Some(fallback));
    let schema = schema();
    let messages = messages();

    let outcome = chain
        .run(
            &request(&messages, &schema),
            Duration::from_secs(20),
            now(),
            clock.as_ref(),
            accept,
        )
        .await;

    assert_eq!(
        outcome.result,
        Err(ChainError::Degraded {
            reason: "server_error"
        })
    );
    assert_eq!(outcome.attempts.len(), 2);
    assert_eq!(outcome.attempts[0].error_code, Some("timeout"));
    assert_eq!(outcome.attempts[1].error_code, Some("server_error"));
}

#[tokio::test]
async fn open_primary_breaker_skips_the_primary_and_gives_the_fallback_the_whole_budget() {
    let clock = Arc::new(FakeClock::default());
    let (primary, primary_backend) = slot(
        "primary",
        &clock,
        Duration::from_secs(10),
        Duration::from_secs(1),
        Reply::Content(ANSWER),
    );
    for _ in 0..5 {
        primary.breaker().record_failure(now());
    }
    let (fallback, fallback_backend) = slot(
        "fallback",
        &clock,
        Duration::from_secs(30),
        Duration::from_secs(1),
        Reply::Content(ANSWER),
    );
    let chain = Chain::new(primary, Some(fallback));
    let schema = schema();
    let messages = messages();

    let outcome = chain
        .run(
            &request(&messages, &schema),
            Duration::from_secs(20),
            now(),
            clock.as_ref(),
            accept,
        )
        .await;

    assert_eq!(outcome.result, Ok(ANSWER.to_owned()));
    assert!(
        primary_backend.calls().is_empty(),
        "an open breaker must not call the slot"
    );
    assert_eq!(fallback_backend.calls(), vec![Duration::from_secs(20)]);
    assert_eq!(outcome.attempts.len(), 1);
    assert_eq!(outcome.attempts[0].slot, "fallback");
    assert_eq!(outcome.attempts[0].number, 1);
}

#[tokio::test]
async fn both_open_breakers_make_no_calls_and_degrade() {
    let clock = Arc::new(FakeClock::default());
    let (primary, primary_backend) = slot(
        "primary",
        &clock,
        Duration::from_secs(10),
        Duration::from_secs(1),
        Reply::Content(ANSWER),
    );
    let (fallback, fallback_backend) = slot(
        "fallback",
        &clock,
        Duration::from_secs(10),
        Duration::from_secs(1),
        Reply::Content(ANSWER),
    );
    for _ in 0..5 {
        primary.breaker().record_failure(now());
        fallback.breaker().record_failure(now());
    }
    let chain = Chain::new(primary, Some(fallback));
    let schema = schema();
    let messages = messages();

    let outcome = chain
        .run(
            &request(&messages, &schema),
            Duration::from_secs(20),
            now(),
            clock.as_ref(),
            accept,
        )
        .await;

    assert_eq!(
        outcome.result,
        Err(ChainError::Degraded {
            reason: "circuit_open"
        })
    );
    assert!(outcome.attempts.is_empty());
    assert!(primary_backend.calls().is_empty());
    assert!(fallback_backend.calls().is_empty());
}

#[tokio::test]
async fn without_a_fallback_the_primary_gets_the_whole_budget_within_its_timeout() {
    let clock = Arc::new(FakeClock::default());
    let (primary, primary_backend) = slot(
        "primary",
        &clock,
        Duration::from_secs(30),
        Duration::from_secs(1),
        Reply::Content(ANSWER),
    );
    let chain = Chain::new(primary, None);
    let schema = schema();
    let messages = messages();

    let outcome = chain
        .run(
            &request(&messages, &schema),
            Duration::from_secs(20),
            now(),
            clock.as_ref(),
            accept,
        )
        .await;

    assert_eq!(outcome.result, Ok(ANSWER.to_owned()));
    assert_eq!(primary_backend.calls(), vec![Duration::from_secs(20)]);
    assert_eq!(outcome.attempts.len(), 1);
    assert_eq!(outcome.attempts[0].number, 1);
}

#[tokio::test]
async fn a_smaller_primary_timeout_is_respected() {
    let clock = Arc::new(FakeClock::default());
    let (primary, primary_backend) = slot(
        "primary",
        &clock,
        Duration::from_secs(2),
        Duration::from_secs(1),
        Reply::Content(ANSWER),
    );
    let (fallback, _) = slot(
        "fallback",
        &clock,
        Duration::from_secs(10),
        Duration::from_secs(1),
        Reply::Content(ANSWER),
    );
    let chain = Chain::new(primary, Some(fallback));
    let schema = schema();
    let messages = messages();

    let outcome = chain
        .run(
            &request(&messages, &schema),
            Duration::from_secs(8),
            now(),
            clock.as_ref(),
            accept,
        )
        .await;

    assert_eq!(outcome.result, Ok(ANSWER.to_owned()));
    assert_eq!(primary_backend.calls(), vec![Duration::from_secs(2)]);
}

#[tokio::test]
async fn the_outer_timeout_is_reported_as_a_timeout() {
    // Настоящая задержка — миллисекунды: проверяется внешний таймаут, а не
    // бюджет в секундах.
    let clock = Arc::new(FakeClock::default());
    let primary = Slot::new(
        "primary",
        Arc::new(SlowBackend(Duration::from_millis(200))),
        Duration::from_millis(10),
    );
    let chain = Chain::new(primary, None);
    let schema = schema();
    let messages = messages();

    let outcome = chain
        .run(
            &request(&messages, &schema),
            Duration::from_millis(20),
            now(),
            clock.as_ref(),
            accept,
        )
        .await;

    assert_eq!(
        outcome.result,
        Err(ChainError::Degraded { reason: "timeout" })
    );
    assert_eq!(outcome.attempts.len(), 1);
    assert_eq!(outcome.attempts[0].error_code, Some("timeout"));
    assert_eq!(outcome.attempts[0].duration, Duration::from_millis(10));
}

#[tokio::test]
async fn chain_opens_the_primary_breaker_after_repeated_transport_failures() {
    let clock = Arc::new(FakeClock::default());
    let (primary, primary_backend) = slot(
        "primary",
        &clock,
        Duration::from_secs(10),
        Duration::from_secs(1),
        Reply::Error(LlmError::Server(503)),
    );
    let (fallback, fallback_backend) = slot(
        "fallback",
        &clock,
        Duration::from_secs(10),
        Duration::from_secs(1),
        Reply::Content(ANSWER),
    );
    let chain = Chain::new(primary, Some(fallback));
    let schema = schema();
    let messages = messages();
    let request = request(&messages, &schema);

    // Каждый разбор: основной падает, резервный отвечает. После порога
    // предохранитель основного должен разомкнуться сам.
    for _ in 0..DEFAULT_THRESHOLD {
        let outcome = chain
            .run(
                &request,
                Duration::from_secs(20),
                now(),
                clock.as_ref(),
                accept,
            )
            .await;
        assert_eq!(outcome.result, Ok(ANSWER.to_owned()));
        assert_eq!(outcome.attempts.len(), 2);
    }
    assert_eq!(primary_backend.calls().len(), DEFAULT_THRESHOLD);
    assert!(
        chain.primary().breaker().retry_after(now()).is_some(),
        "repeated primary failures must open its breaker"
    );

    let outcome = chain
        .run(
            &request,
            Duration::from_secs(20),
            now(),
            clock.as_ref(),
            accept,
        )
        .await;

    assert_eq!(outcome.result, Ok(ANSWER.to_owned()));
    assert_eq!(outcome.attempts.len(), 1);
    assert_eq!(outcome.attempts[0].slot, "fallback");
    assert_eq!(outcome.attempts[0].number, 1);
    assert_eq!(
        primary_backend.calls().len(),
        DEFAULT_THRESHOLD,
        "an open breaker must not call the primary slot"
    );
    assert_eq!(fallback_backend.calls().len(), DEFAULT_THRESHOLD + 1);
}

#[tokio::test]
async fn chain_opens_the_primary_breaker_after_repeated_invalid_responses() {
    let clock = Arc::new(FakeClock::default());
    let (primary, primary_backend) = slot(
        "primary",
        &clock,
        Duration::from_secs(10),
        Duration::from_secs(1),
        Reply::Content("не JSON по схеме"),
    );
    let (fallback, fallback_backend) = slot(
        "fallback",
        &clock,
        Duration::from_secs(10),
        Duration::from_secs(1),
        Reply::Content(ANSWER),
    );
    let chain = Chain::new(primary, Some(fallback));
    let schema = schema();
    let messages = messages();
    let request = request(&messages, &schema);

    for _ in 0..DEFAULT_THRESHOLD {
        let outcome = chain
            .run(
                &request,
                Duration::from_secs(20),
                now(),
                clock.as_ref(),
                shape,
            )
            .await;
        assert_eq!(outcome.result, Ok(ANSWER.to_owned()));
        assert_eq!(outcome.attempts[0].error_code, Some("invalid_field"));
        assert_eq!(outcome.attempts.len(), 2);
    }
    assert!(
        chain.primary().breaker().retry_after(now()).is_some(),
        "responses rejected by the schema must open the breaker"
    );

    let outcome = chain
        .run(
            &request,
            Duration::from_secs(20),
            now(),
            clock.as_ref(),
            shape,
        )
        .await;

    assert_eq!(outcome.result, Ok(ANSWER.to_owned()));
    assert_eq!(outcome.attempts.len(), 1);
    assert_eq!(outcome.attempts[0].slot, "fallback");
    assert_eq!(primary_backend.calls().len(), DEFAULT_THRESHOLD);
    assert_eq!(fallback_backend.calls().len(), DEFAULT_THRESHOLD + 1);
}
