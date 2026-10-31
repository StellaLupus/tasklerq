//! Клиент Home Assistant REST и вызов `ai_task.generate_data`.
//!
//! `AiTask` — граница, за которой скрыт транспорт. Оркестрация capture
//! работает только с этим trait, поэтому в тестах HA заменяется mock-сервером
//! или заглушкой. Клиент ничего не знает о промпте и схеме ответа: он
//! отправляет инструкции и структуру, возвращает объект `data`.

use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use thiserror::Error;

use crate::config::SecretString;

/// Верхняя граница ожидания ответа LLM; короче можно, длиннее нельзя.
pub const MAX_GENERATE_TIMEOUT: Duration = Duration::from_secs(20);

/// Таймаут проверки сущности: это обычное чтение состояния, не LLM.
const CHECK_TIMEOUT: Duration = Duration::from_secs(5);

/// Ошибка обращения к Home Assistant.
///
/// Тексты не содержат URL, токен и тело ответа: журнал не должен получить ни
/// секреты, ни пользовательский текст, который HA мог вернуть эхом.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum HaError {
    /// Некорректные параметры клиента.
    #[error("invalid home assistant config: {0}")]
    Config(&'static str),
    /// Ответ не пришёл за отведённое время.
    #[error("home assistant request timed out")]
    Timeout,
    /// Соединение не установлено или оборвано.
    #[error("home assistant is unreachable")]
    Unreachable,
    /// `401`/`403`: токен отклонён.
    #[error("home assistant rejected the token")]
    Unauthorized,
    /// Сущности нет, она отключена или недоступна.
    #[error("ai_task entity is not available")]
    EntityUnavailable,
    /// `5xx`: сбой на стороне Home Assistant.
    #[error("home assistant failed with http {0}")]
    Server(u16),
    /// Другой `4xx`: запрос отклонён, повтор бессмысленен.
    #[error("home assistant rejected the request with http {0}")]
    Rejected(u16),
    /// Ответ не похож на ответ `ai_task.generate_data`.
    #[error("home assistant returned an unexpected response")]
    BadResponse,
}

impl HaError {
    /// Повтор может завершиться успехом. Таймаут намеренно не входит сюда:
    /// повтор удвоил бы ожидание и стоимость вызова.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Unreachable | Self::Server(_))
    }

    /// Короткий код без пользовательских данных для `llm_runs.error_code`.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Config(_) => "config",
            Self::Timeout => "timeout",
            Self::Unreachable => "unreachable",
            Self::Unauthorized => "unauthorized",
            Self::EntityUnavailable => "entity_unavailable",
            Self::Server(_) => "server_error",
            Self::Rejected(_) => "rejected",
            Self::BadResponse => "bad_response",
        }
    }
}

/// Параметры одного вызова `ai_task.generate_data`.
#[derive(Debug, Clone, Copy)]
pub struct GenerateData<'a> {
    /// Короткое имя задачи для журнала Home Assistant.
    pub task_name: &'a str,
    /// Промпт вместе с данными пользователя.
    pub instructions: &'a str,
    /// Структура ответа в виде селекторов Home Assistant.
    pub structure: &'a Value,
}

/// Операции Home Assistant, которые нужны capture.
#[async_trait]
pub trait AiTask: Send + Sync {
    /// Убедиться, что сущность `ai_task` существует и доступна.
    async fn check_entity(&self) -> Result<(), HaError>;

    /// Выполнить разбор и вернуть объект `data` из ответа. Реальное ожидание —
    /// `timeout`, но не больше [`MAX_GENERATE_TIMEOUT`].
    async fn generate_data(
        &self,
        request: &GenerateData<'_>,
        timeout: Duration,
    ) -> Result<Value, HaError>;
}

/// Клиент Home Assistant REST поверх `reqwest`.
pub struct HttpAiTask {
    client: reqwest::Client,
    base_url: String,
    token: SecretString,
    entity_id: String,
}

impl HttpAiTask {
    /// Создать клиент. `base_url` — адрес HA из контейнера Tasklerq, без
    /// завершающего `/`; допускается префикс пути за обратным прокси.
    pub fn new(base_url: &str, token: SecretString, entity_id: &str) -> Result<Self, HaError> {
        if !is_ai_task_entity(entity_id) {
            return Err(HaError::Config("entity id must look like ai_task.<name>"));
        }
        // Токен уходит в Bearer-заголовке: на редирект не идём, чтобы он не
        // попал на другой адрес. HA отвечает на REST без перенаправлений.
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(MAX_GENERATE_TIMEOUT)
            .build()
            .map_err(|_| HaError::Config("http client"))?;
        Ok(Self {
            client,
            base_url: base_url.trim_end_matches('/').to_owned(),
            token,
            entity_id: entity_id.to_owned(),
        })
    }

    /// Отправить запрос с токеном; транспортная ошибка и ошибочный статус
    /// становятся [`HaError`].
    async fn send(&self, request: reqwest::RequestBuilder) -> Result<reqwest::Response, HaError> {
        let response = request
            .bearer_auth(self.token.expose())
            .send()
            .await
            .map_err(classify_transport)?;
        match classify_status(response.status().as_u16()) {
            Some(error) => Err(error),
            None => Ok(response),
        }
    }
}

/// `ai_task.<object_id>`: только строчные латинские буквы, цифры и `_`.
fn is_ai_task_entity(entity_id: &str) -> bool {
    let Some(object_id) = entity_id.strip_prefix("ai_task.") else {
        return false;
    };
    !object_id.is_empty()
        && object_id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

/// Реальное ожидание `generate_data`: запрошенное, но не больше
/// [`MAX_GENERATE_TIMEOUT`].
fn effective_timeout(requested: Duration) -> Duration {
    requested.min(MAX_GENERATE_TIMEOUT)
}

fn classify_transport(error: reqwest::Error) -> HaError {
    if error.is_timeout() {
        HaError::Timeout
    } else {
        // Отказ соединения, DNS или обрыв при чтении тела: ответа нет.
        HaError::Unreachable
    }
}

fn classify_status(status: u16) -> Option<HaError> {
    match status {
        200..=299 => None,
        401 | 403 => Some(HaError::Unauthorized),
        404 => Some(HaError::EntityUnavailable),
        500..=599 => Some(HaError::Server(status)),
        _ => Some(HaError::Rejected(status)),
    }
}

#[derive(Deserialize)]
struct StateResponse {
    state: String,
}

#[derive(Deserialize)]
struct ServiceResponse {
    service_response: Option<ServiceData>,
}

#[derive(Deserialize)]
struct ServiceData {
    data: Option<Value>,
}

/// Достать `service_response.data` из ответа `?return_response`.
fn extract_data(body: &str) -> Result<Value, HaError> {
    let response: ServiceResponse = serde_json::from_str(body).map_err(|_| HaError::BadResponse)?;
    match response.service_response.and_then(|response| response.data) {
        Some(data @ Value::Object(_)) => Ok(data),
        _ => Err(HaError::BadResponse),
    }
}

#[async_trait]
impl AiTask for HttpAiTask {
    async fn check_entity(&self) -> Result<(), HaError> {
        let url = format!("{}/api/states/{}", self.base_url, self.entity_id);
        let response = self
            .send(self.client.get(url).timeout(CHECK_TIMEOUT))
            .await?;
        let state: StateResponse = response.json().await.map_err(|_| HaError::BadResponse)?;
        // У ai_task состояние — время последнего использования или `unknown`;
        // недоступной сущность делает только `unavailable`.
        if state.state == "unavailable" {
            return Err(HaError::EntityUnavailable);
        }
        Ok(())
    }

    async fn generate_data(
        &self,
        request: &GenerateData<'_>,
        timeout: Duration,
    ) -> Result<Value, HaError> {
        let url = format!(
            "{}/api/services/ai_task/generate_data?return_response",
            self.base_url
        );
        let body = json!({
            "entity_id": self.entity_id,
            "task_name": request.task_name,
            "instructions": request.instructions,
            "structure": request.structure,
        });
        let response = self
            .send(
                self.client
                    .post(url)
                    .timeout(effective_timeout(timeout))
                    .json(&body),
            )
            .await?;
        let text = response.text().await.map_err(classify_transport)?;
        extract_data(&text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entity_id_must_be_ai_task() {
        assert!(is_ai_task_entity("ai_task.google_ai_task_2"));
        assert!(!is_ai_task_entity("conversation.home"));
        assert!(!is_ai_task_entity("ai_task."));
        assert!(!is_ai_task_entity("ai_task.a/../b"));
        assert!(!is_ai_task_entity("ai_task.Upper"));
    }

    #[test]
    fn generate_timeout_is_capped_at_twenty_seconds() {
        assert_eq!(
            effective_timeout(Duration::from_secs(60)),
            Duration::from_secs(20)
        );
        assert_eq!(
            effective_timeout(Duration::from_secs(20)),
            Duration::from_secs(20)
        );
        assert_eq!(
            effective_timeout(Duration::from_secs(5)),
            Duration::from_secs(5)
        );
    }

    #[test]
    fn statuses_are_classified() {
        assert_eq!(classify_status(200), None);
        assert_eq!(classify_status(401), Some(HaError::Unauthorized));
        assert_eq!(classify_status(404), Some(HaError::EntityUnavailable));
        assert_eq!(classify_status(502), Some(HaError::Server(502)));
        assert_eq!(classify_status(400), Some(HaError::Rejected(400)));
    }

    #[test]
    fn only_transient_errors_are_retryable() {
        assert!(HaError::Unreachable.is_retryable());
        assert!(HaError::Server(503).is_retryable());
        assert!(!HaError::Timeout.is_retryable());
        assert!(!HaError::Unauthorized.is_retryable());
        assert!(!HaError::BadResponse.is_retryable());
    }

    #[test]
    fn data_is_taken_from_service_response() {
        let body = r#"{"changed_states":[],"service_response":{"conversation_id":"c","data":{"items":[]}}}"#;
        assert_eq!(extract_data(body), Ok(json!({"items": []})));
    }

    #[test]
    fn response_without_data_is_bad() {
        assert_eq!(extract_data("{}"), Err(HaError::BadResponse));
        assert_eq!(
            extract_data(r#"{"service_response":{"data":"text"}}"#),
            Err(HaError::BadResponse)
        );
        assert_eq!(extract_data("not json"), Err(HaError::BadResponse));
    }
}
