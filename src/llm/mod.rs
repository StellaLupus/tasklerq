//! Клиент OpenAI-совместимого API для разбора текста.
//!
//! [`ChatBackend`] — граница, за которой скрыт транспорт слота. Оркестрация
//! разбора работает только с этим trait, поэтому в тестах слот заменяется
//! двойником, а [`OpenAiBackend`] остаётся единственной рабочей реализацией.
//! Клиент ничего не знает о промпте: он отправляет сообщения и схему ответа,
//! возвращает текст, число токенов и модель.
//!
//! Тексты ошибок не содержат адрес слота, токен и тело ответа: журнал не
//! должен получить ни секреты, ни пользовательский текст, который провайдер
//! мог вернуть эхом.

use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use thiserror::Error;

use crate::config::SecretString;

/// Верхняя граница ответа модели: черновик из двадцати элементов и уточнений
/// укладывается с запасом.
pub const MAX_COMPLETION_TOKENS: u32 = 4096;

/// Ключи тела запроса, которые дополнительные параметры слота заменить не
/// могут.
const RESERVED_KEYS: [&str; 4] = ["model", "messages", "response_format", "temperature"];

/// Роль сообщения в чате.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ChatRole {
    /// Системные правила и контекст.
    System,
    /// Текст пользователя.
    User,
    /// Ответ модели; нужен для истории диалога.
    Assistant,
}

/// Одно сообщение запроса.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChatMessage {
    /// Роль отправителя.
    pub role: ChatRole,
    /// Текст сообщения.
    pub content: String,
}

impl ChatMessage {
    /// Системное сообщение с правилами и контекстом.
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: ChatRole::System,
            content: content.into(),
        }
    }

    /// Сообщение с текстом пользователя.
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: ChatRole::User,
            content: content.into(),
        }
    }
}

/// Параметры одного вызова модели.
#[derive(Debug, Clone, Copy)]
pub struct ChatRequest<'a> {
    /// Сообщения чата в порядке отправки.
    pub messages: &'a [ChatMessage],
    /// Имя схемы ответа для `response_format`.
    pub schema_name: &'a str,
    /// JSON Schema ответа.
    pub schema: &'a Value,
}

/// Успешный ответ модели.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatCompletion {
    /// Текст ответа из `choices[0].message.content`.
    pub content: String,
    /// Входные токены из `usage.prompt_tokens`, если провайдер их вернул.
    pub input_tokens: Option<u32>,
    /// Выходные токены из `usage.completion_tokens`, если провайдер их вернул.
    pub output_tokens: Option<u32>,
    /// Имя модели из ответа; пустая строка, если провайдер его не прислал.
    pub model: String,
}

/// Ошибка обращения к слоту LLM.
///
/// Каждый случай даёт отдельный стабильный [`LlmError::code`] для `llm_runs` и
/// метрик. Тексты не содержат URL, токен и тело ответа.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum LlmError {
    /// Некорректные параметры клиента.
    #[error("invalid llm client config: {0}")]
    Config(&'static str),
    /// Соединение не установлено или оборвано.
    #[error("llm endpoint is unreachable")]
    Transport,
    /// Ответ не пришёл за отведённое время.
    #[error("llm request timed out")]
    Timeout,
    /// `401`/`403`: токен отклонён.
    #[error("llm rejected the token")]
    Unauthorized,
    /// `429`: превышен лимит запросов.
    #[error("llm rate limited the request")]
    RateLimited,
    /// Другой `4xx`, включая `400`: запрос или схема отклонены.
    #[error("llm rejected the request with http {0}")]
    Rejected(u16),
    /// `5xx`: сбой на стороне провайдера.
    #[error("llm failed with http {0}")]
    Server(u16),
    /// Ответ пуст или не разбирается как ответ чата.
    #[error("llm returned an empty or malformed response")]
    EmptyResponse,
}

impl LlmError {
    /// Короткий код без пользовательских данных для `llm_runs.error_code`.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Config(_) => "config",
            Self::Transport => "transport",
            Self::Timeout => "timeout",
            Self::Unauthorized => "unauthorized",
            Self::RateLimited => "rate_limited",
            Self::Rejected(_) => "rejected",
            Self::Server(_) => "server_error",
            Self::EmptyResponse => "empty_response",
        }
    }
}

/// Операции слота, которые нужны разбору.
#[async_trait]
pub trait ChatBackend: Send + Sync {
    /// Отправить сообщения со схемой ответа и вернуть ответ модели. Реальное
    /// ожидание — `timeout`, установка соединения ограничена таймаутом,
    /// заданным в конструкторе реализации.
    async fn complete(
        &self,
        request: &ChatRequest<'_>,
        timeout: Duration,
    ) -> Result<ChatCompletion, LlmError>;
}

/// Клиент OpenAI-совместимого API поверх `reqwest`.
pub struct OpenAiBackend {
    client: reqwest::Client,
    endpoint: String,
    model: String,
    token: Option<SecretString>,
    extra: Map<String, Value>,
}

impl OpenAiBackend {
    /// Создать клиент слота. `url` — базовый адрес API, включая `/v1`,
    /// завершающий `/` допускается. `token` необязателен: локальный слот
    /// работает без авторизации. `extra` — дополнительные параметры запроса,
    /// уже разобранные из `LLM_*_EXTRA_JSON`.
    pub fn new(
        url: &str,
        model: impl Into<String>,
        token: Option<SecretString>,
        connect_timeout: Duration,
        extra: Map<String, Value>,
    ) -> Result<Self, LlmError> {
        let url = url.trim();
        if url.is_empty() {
            return Err(LlmError::Config("url must not be empty"));
        }
        // Токен уходит в Bearer-заголовке: на редирект не идём, чтобы он не
        // попал на другой адрес.
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(connect_timeout)
            .build()
            .map_err(|_| LlmError::Config("http client"))?;
        Ok(Self {
            client,
            endpoint: format!("{}/chat/completions", url.trim_end_matches('/')),
            model: model.into(),
            token,
            extra,
        })
    }

    /// Тело запроса: обязательные поля и дополнительные параметры слота.
    fn request_body(&self, request: &ChatRequest<'_>) -> Value {
        let mut body = Map::new();
        body.insert("model".to_owned(), Value::String(self.model.clone()));
        body.insert(
            "messages".to_owned(),
            serde_json::to_value(request.messages).expect("chat messages serialize"),
        );
        body.insert(
            "response_format".to_owned(),
            json!({
                "type": "json_schema",
                "json_schema": {
                    "name": request.schema_name,
                    "strict": true,
                    "schema": request.schema,
                },
            }),
        );
        body.insert("temperature".to_owned(), json!(0));
        body.insert("max_tokens".to_owned(), Value::from(MAX_COMPLETION_TOKENS));
        for (key, value) in &self.extra {
            if !RESERVED_KEYS.contains(&key.as_str()) {
                body.insert(key.clone(), value.clone());
            }
        }
        Value::Object(body)
    }
}

impl fmt::Debug for OpenAiBackend {
    /// Печатает адрес и модель, но не токен.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAiBackend")
            .field("endpoint", &self.endpoint)
            .field("model", &self.model)
            .field("has_token", &self.token.is_some())
            .field("extra", &self.extra)
            .finish()
    }
}

#[async_trait]
impl ChatBackend for OpenAiBackend {
    async fn complete(
        &self,
        request: &ChatRequest<'_>,
        timeout: Duration,
    ) -> Result<ChatCompletion, LlmError> {
        let mut http_request = self
            .client
            .post(&self.endpoint)
            .timeout(timeout)
            .json(&self.request_body(request));
        if let Some(token) = &self.token {
            http_request = http_request.bearer_auth(token.expose());
        }
        let response = http_request.send().await.map_err(classify_transport)?;
        if let Some(error) = classify_status(response.status().as_u16()) {
            return Err(error);
        }
        let text = response.text().await.map_err(classify_transport)?;
        extract_completion(&text)
    }
}

fn classify_transport(error: reqwest::Error) -> LlmError {
    if error.is_timeout() {
        LlmError::Timeout
    } else {
        // Отказ соединения, DNS или обрыв при чтении тела: ответа нет.
        LlmError::Transport
    }
}

fn classify_status(status: u16) -> Option<LlmError> {
    match status {
        200..=299 => None,
        401 | 403 => Some(LlmError::Unauthorized),
        429 => Some(LlmError::RateLimited),
        500..=599 => Some(LlmError::Server(status)),
        _ => Some(LlmError::Rejected(status)),
    }
}

#[derive(Deserialize)]
struct CompletionBody {
    #[serde(default)]
    choices: Vec<Choice>,
    usage: Option<Usage>,
    model: Option<String>,
}

#[derive(Deserialize)]
struct Choice {
    message: Option<ChoiceMessage>,
}

#[derive(Deserialize)]
struct ChoiceMessage {
    content: Option<String>,
}

#[derive(Deserialize)]
struct Usage {
    prompt_tokens: Option<u32>,
    completion_tokens: Option<u32>,
}

/// Достать текст, токены и модель из ответа `chat/completions`.
fn extract_completion(body: &str) -> Result<ChatCompletion, LlmError> {
    let response: CompletionBody =
        serde_json::from_str(body).map_err(|_| LlmError::EmptyResponse)?;
    let content = response
        .choices
        .into_iter()
        .next()
        .and_then(|choice| choice.message)
        .and_then(|message| message.content)
        .ok_or(LlmError::EmptyResponse)?;
    if content.trim().is_empty() {
        return Err(LlmError::EmptyResponse);
    }
    Ok(ChatCompletion {
        content,
        input_tokens: response
            .usage
            .as_ref()
            .and_then(|usage| usage.prompt_tokens),
        output_tokens: response
            .usage
            .as_ref()
            .and_then(|usage| usage.completion_tokens),
        model: response.model.unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend(token: Option<&str>) -> OpenAiBackend {
        OpenAiBackend::new(
            "http://llm.local:8080/v1",
            "local/model",
            token.map(SecretString::new),
            Duration::from_millis(1000),
            Map::new(),
        )
        .expect("backend")
    }

    #[test]
    fn debug_does_not_contain_token() {
        let backend = backend(Some("llm-SECRET-token"));
        let rendered = format!("{backend:?}");
        assert!(!rendered.contains("llm-SECRET-token"));
        assert!(rendered.contains("has_token: true"));
        assert!(rendered.contains("local/model"));
    }

    #[test]
    fn empty_url_is_rejected_at_construction() {
        let result = OpenAiBackend::new(
            "  ",
            "local/model",
            None,
            Duration::from_millis(1000),
            Map::new(),
        );
        assert!(matches!(result, Err(LlmError::Config(_))));
    }

    #[test]
    fn completion_is_taken_from_first_choice() {
        let completion = extract_completion(
            r#"{"model":"m","choices":[{"message":{"content":"{\"items\":[]}"}}],
                "usage":{"prompt_tokens":10,"completion_tokens":4}}"#,
        )
        .expect("completion");
        assert_eq!(completion.content, r#"{"items":[]}"#);
        assert_eq!(completion.input_tokens, Some(10));
        assert_eq!(completion.output_tokens, Some(4));
        assert_eq!(completion.model, "m");
    }
}
