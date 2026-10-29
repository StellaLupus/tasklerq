//! Приём обновлений: `POST /webhooks/telegram`.
//!
//! Обработчик только проверяет секрет, разбирает тело и ставит обновление в
//! очередь. Всё остальное делает воркер, поэтому ответ `200` не зависит от
//! LLM и Telegram API.

use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use chrono::Utc;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::sync::Notify;

use crate::config::SecretString;
use crate::http::error::ApiError;
use crate::http::router::AppState;

use super::queue;
use super::update::{self, Classified};

/// Заголовок с секретом webhook.
pub const SECRET_HEADER: &str = "x-telegram-bot-api-secret-token";

/// Максимальный размер тела обновления.
pub const MAX_BODY_BYTES: usize = 1024 * 1024;

/// Настройки приёма webhook.
#[derive(Clone)]
pub struct Ingress {
    secret_digest: [u8; 32],
    wake: Arc<Notify>,
}

impl Ingress {
    /// Создать приёмник. `wake` будит воркер обновлений после вставки.
    pub fn new(secret: &SecretString, wake: Arc<Notify>) -> Self {
        Self {
            secret_digest: Sha256::digest(secret.expose().as_bytes()).into(),
            wake,
        }
    }

    /// Сравнить секрет за постоянное время: оба значения приводятся к хешу
    /// одинаковой длины, поэтому длина секрета не раскрывается.
    fn accepts(&self, candidate: &str) -> bool {
        let digest: [u8; 32] = Sha256::digest(candidate.as_bytes()).into();
        digest.ct_eq(&self.secret_digest).into()
    }
}

/// `POST /webhooks/telegram`.
pub async fn receive(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Body,
) -> Result<StatusCode, ApiError> {
    let Some(ingress) = &state.telegram else {
        return Err(ApiError::not_found("Route not found"));
    };

    let provided = headers
        .get(SECRET_HEADER)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if !ingress.accepts(provided) {
        return Err(ApiError::unauthorized("Invalid webhook secret"));
    }

    let bytes = to_bytes(body, MAX_BODY_BYTES)
        .await
        .map_err(|_| ApiError::payload_too_large("Update is too large"))?;
    let payload: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            crate::http::error::ErrorCode::ValidationError,
            "Malformed update",
        )
    })?;
    let parsed = update::parse_bytes(&bytes).map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            crate::http::error::ErrorCode::ValidationError,
            "Malformed update",
        )
    })?;

    // Группы, каналы и неподдерживаемые типы подтверждаются без сохранения.
    if matches!(update::classify(&parsed), Classified::Ignore) {
        return Ok(StatusCode::OK);
    }

    let mut conn = state.pool.acquire().await.map_err(|error| {
        tracing::error!(%error, "webhook: database unavailable");
        ApiError::internal("Database error")
    })?;
    // При ошибке записи отвечаем 500: Telegram повторит доставку.
    let inserted = queue::enqueue(&mut conn, i64::from(parsed.id.0), &payload, Utc::now())
        .await
        .map_err(|error| {
            tracing::error!(%error, "webhook: failed to enqueue update");
            ApiError::internal("Database error")
        })?;
    if inserted {
        ingress.wake.notify_one();
    }

    Ok(StatusCode::OK)
}
