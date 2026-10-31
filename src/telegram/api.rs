//! Транспорт Telegram Bot API.
//!
//! `TelegramApi` — граница, за которой скрыта точная схема запросов. Экраны и
//! воркеры работают только с этим trait, поэтому обновление схемы RichMessage
//! не затрагивает остальной код. Типы входящих обновлений берутся из
//! `teloxide-core`, а структуры RichMessage описаны локально.

use std::time::Duration;

use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;

use crate::config::SecretString;

/// Базовый адрес Bot API.
pub const DEFAULT_BASE_URL: &str = "https://api.telegram.org";

/// Таймаут обычного запроса.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// Исходящее RichMessage: HTML с кнопками `<tg-button>` внутри содержимого.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RichMessage {
    /// HTML-содержимое вместе с кнопками.
    pub html: String,
}

/// Ошибка вызова Bot API.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TelegramError {
    /// `429 Too Many Requests`: повторить не раньше чем через N секунд.
    #[error("rate limited, retry after {0}s")]
    RetryAfter(u64),
    /// Новое содержимое совпадает с текущим.
    #[error("message is not modified")]
    NotModified,
    /// Сообщение удалено, слишком старое или не принадлежит боту.
    #[error("message cannot be edited or deleted")]
    MessageGone,
    /// Бот заблокирован пользователем или чат недоступен.
    #[error("forbidden: {0}")]
    Forbidden(String),
    /// Запрос отклонён Telegram; повтор бессмысленен.
    #[error("bad request: {0}")]
    BadRequest(String),
    /// Сеть или `5xx`: повтор имеет смысл.
    #[error("transient failure: {0}")]
    Transient(String),
}

impl TelegramError {
    /// Повтор той же операции может завершиться успехом.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::RetryAfter(_) | Self::Transient(_))
    }

    /// Короткий код без пользовательских данных для `last_error`.
    pub fn code(&self) -> &'static str {
        match self {
            Self::RetryAfter(_) => "retry_after",
            Self::NotModified => "not_modified",
            Self::MessageGone => "message_gone",
            Self::Forbidden(_) => "forbidden",
            Self::BadRequest(_) => "bad_request",
            Self::Transient(_) => "transient",
        }
    }
}

/// Состояние webhook по данным `getWebhookInfo`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct WebhookInfo {
    /// Текущий URL; пустой, если webhook не задан.
    #[serde(default)]
    pub url: String,
    /// Число необработанных обновлений на стороне Telegram.
    #[serde(default)]
    pub pending_update_count: u32,
    /// Последняя ошибка доставки.
    #[serde(default)]
    pub last_error_message: Option<String>,
}

/// Операции Bot API, которые нужны сервису.
#[async_trait]
pub trait TelegramApi: Send + Sync {
    /// Отправить RichMessage; возвращает `message_id`.
    async fn send_rich_message(
        &self,
        chat_id: i64,
        message: &RichMessage,
    ) -> Result<i64, TelegramError>;

    /// Заменить содержимое RichMessage.
    async fn edit_rich_message(
        &self,
        chat_id: i64,
        message_id: i64,
        message: &RichMessage,
    ) -> Result<(), TelegramError>;

    /// Отправить обычное HTML-сообщение; возвращает `message_id`.
    async fn send_message(&self, chat_id: i64, html: &str) -> Result<i64, TelegramError>;

    /// Заменить текст обычного сообщения.
    async fn edit_message_text(
        &self,
        chat_id: i64,
        message_id: i64,
        html: &str,
    ) -> Result<(), TelegramError>;

    /// Ответить на callback-запрос всплывающим уведомлением.
    async fn answer_callback_query(
        &self,
        callback_query_id: &str,
        text: Option<&str>,
        show_alert: bool,
    ) -> Result<(), TelegramError>;

    /// Удалить сообщение.
    async fn delete_message(&self, chat_id: i64, message_id: i64) -> Result<(), TelegramError>;

    /// Зарегистрировать webhook с секретом.
    async fn set_webhook(&self, url: &str, secret_token: &str) -> Result<(), TelegramError>;

    /// Прочитать состояние webhook.
    async fn get_webhook_info(&self) -> Result<WebhookInfo, TelegramError>;

    /// Long polling для локальной разработки; возвращает сырые обновления.
    async fn get_updates(
        &self,
        offset: i64,
        timeout_secs: u32,
    ) -> Result<Vec<Value>, TelegramError>;
}

/// Клиент Bot API поверх `reqwest`.
pub struct HttpTelegramApi {
    client: reqwest::Client,
    base_url: String,
    token: SecretString,
}

impl HttpTelegramApi {
    /// Создать клиент. `base_url` задаёт адрес Bot API (в тестах — mock).
    pub fn new(token: SecretString, base_url: &str) -> Result<Self, reqwest::Error> {
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()?;
        Ok(Self {
            client,
            base_url: base_url.trim_end_matches('/').to_owned(),
            token,
        })
    }

    async fn call<T: DeserializeOwned>(
        &self,
        method: &str,
        body: &Value,
        timeout: Option<Duration>,
    ) -> Result<T, TelegramError> {
        // Токен бота входит в URL, поэтому URL не должен попасть в ошибку.
        let url = format!("{}/bot{}/{method}", self.base_url, self.token.expose());
        let mut request = self.client.post(url).json(body);
        if let Some(timeout) = timeout {
            request = request.timeout(timeout);
        }
        let response = request
            .send()
            .await
            .map_err(|error| TelegramError::Transient(describe(error)))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|error| TelegramError::Transient(describe(error)))?;

        parse_response(status.as_u16(), &text)
    }
}

fn describe(error: reqwest::Error) -> String {
    let error = error.without_url();
    if error.is_timeout() {
        "timeout".to_owned()
    } else if error.is_connect() {
        "connect".to_owned()
    } else {
        "request".to_owned()
    }
}

#[derive(Deserialize)]
struct Envelope<T> {
    ok: bool,
    result: Option<T>,
    description: Option<String>,
    error_code: Option<u16>,
    parameters: Option<EnvelopeParameters>,
}

#[derive(Deserialize)]
struct EnvelopeParameters {
    retry_after: Option<u64>,
}

/// Разобрать ответ Bot API и классифицировать ошибку.
pub fn parse_response<T: DeserializeOwned>(status: u16, body: &str) -> Result<T, TelegramError> {
    let envelope: Envelope<T> = match serde_json::from_str(body) {
        Ok(envelope) => envelope,
        Err(_) if status >= 500 => {
            return Err(TelegramError::Transient(format!("http {status}")));
        }
        Err(_) => return Err(TelegramError::BadRequest(format!("http {status}"))),
    };
    if envelope.ok {
        return envelope
            .result
            .ok_or_else(|| TelegramError::BadRequest("empty result".to_owned()));
    }

    let code = envelope.error_code.unwrap_or(status);
    let description = envelope.description.unwrap_or_default();
    let lowered = description.to_lowercase();

    if code == 429 {
        let retry_after = envelope
            .parameters
            .and_then(|parameters| parameters.retry_after)
            .unwrap_or(1);
        return Err(TelegramError::RetryAfter(retry_after));
    }
    if lowered.contains("message is not modified") {
        return Err(TelegramError::NotModified);
    }
    if lowered.contains("message to edit not found")
        || lowered.contains("message to delete not found")
        || lowered.contains("message can't be edited")
        || lowered.contains("message can't be deleted")
        || lowered.contains("message_id_invalid")
    {
        return Err(TelegramError::MessageGone);
    }
    if code == 403 {
        return Err(TelegramError::Forbidden(description));
    }
    if code >= 500 {
        return Err(TelegramError::Transient(format!("http {code}")));
    }
    Err(TelegramError::BadRequest(description))
}

#[derive(Deserialize)]
struct SentMessage {
    message_id: i64,
}

#[async_trait]
impl TelegramApi for HttpTelegramApi {
    async fn send_rich_message(
        &self,
        chat_id: i64,
        message: &RichMessage,
    ) -> Result<i64, TelegramError> {
        let sent: SentMessage = self
            .call(
                "sendRichMessage",
                &json!({"chat_id": chat_id, "rich_message": {"html": message.html}}),
                None,
            )
            .await?;
        Ok(sent.message_id)
    }

    async fn edit_rich_message(
        &self,
        chat_id: i64,
        message_id: i64,
        message: &RichMessage,
    ) -> Result<(), TelegramError> {
        // Богатое сообщение редактируется тем же методом, что и текст:
        // отдельного `editRichMessage` в Bot API нет.
        self.call::<Value>(
            "editMessageText",
            &json!({
                "chat_id": chat_id,
                "message_id": message_id,
                "rich_message": {"html": message.html}
            }),
            None,
        )
        .await
        .map(|_| ())
    }

    async fn send_message(&self, chat_id: i64, html: &str) -> Result<i64, TelegramError> {
        let sent: SentMessage = self
            .call(
                "sendMessage",
                &json!({"chat_id": chat_id, "text": html, "parse_mode": "HTML"}),
                None,
            )
            .await?;
        Ok(sent.message_id)
    }

    async fn edit_message_text(
        &self,
        chat_id: i64,
        message_id: i64,
        html: &str,
    ) -> Result<(), TelegramError> {
        self.call::<Value>(
            "editMessageText",
            &json!({
                "chat_id": chat_id,
                "message_id": message_id,
                "text": html,
                "parse_mode": "HTML",
            }),
            None,
        )
        .await
        .map(|_| ())
    }

    async fn answer_callback_query(
        &self,
        callback_query_id: &str,
        text: Option<&str>,
        show_alert: bool,
    ) -> Result<(), TelegramError> {
        self.call::<Value>(
            "answerCallbackQuery",
            &json!({
                "callback_query_id": callback_query_id,
                "text": text,
                "show_alert": show_alert,
            }),
            None,
        )
        .await
        .map(|_| ())
    }

    async fn delete_message(&self, chat_id: i64, message_id: i64) -> Result<(), TelegramError> {
        self.call::<Value>(
            "deleteMessage",
            &json!({"chat_id": chat_id, "message_id": message_id}),
            None,
        )
        .await
        .map(|_| ())
    }

    async fn set_webhook(&self, url: &str, secret_token: &str) -> Result<(), TelegramError> {
        self.call::<Value>(
            "setWebhook",
            &json!({
                "url": url,
                "secret_token": secret_token,
                "allowed_updates": ["message", "callback_query"],
                "drop_pending_updates": false,
            }),
            None,
        )
        .await
        .map(|_| ())
    }

    async fn get_webhook_info(&self) -> Result<WebhookInfo, TelegramError> {
        self.call("getWebhookInfo", &json!({}), None).await
    }

    async fn get_updates(
        &self,
        offset: i64,
        timeout_secs: u32,
    ) -> Result<Vec<Value>, TelegramError> {
        self.call(
            "getUpdates",
            &json!({
                "offset": offset,
                "timeout": timeout_secs,
                "allowed_updates": ["message", "callback_query"],
            }),
            Some(Duration::from_secs(u64::from(timeout_secs) + 10)),
        )
        .await
    }
}
