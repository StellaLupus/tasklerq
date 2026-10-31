//! Сборка HTTP-роутера.

use axum::Router;
use axum::routing::{get, post};
use sqlx::PgPool;

use super::api;
use super::api::capture::CaptureState;
use super::error::ApiError;
use super::health;
use super::middleware::request_id;
use crate::telegram::webhook::{self, Ingress};

/// Состояние приложения, доступное обработчикам.
#[derive(Clone)]
pub struct AppState {
    /// Пул соединений PostgreSQL.
    pub pool: PgPool,
    /// Приём Telegram webhook; `None`, если webhook выключен.
    pub telegram: Option<Ingress>,
    /// Допустимый возраст необработанного обновления Telegram; `None`, если
    /// воркер обновлений не запущен и очередь не проверяется.
    pub queue_lag_limit: Option<chrono::Duration>,
    /// Внешний capture; `None`, если маршрут выключен.
    pub capture: Option<CaptureState>,
}

impl AppState {
    /// Создать состояние из пула.
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            telegram: None,
            queue_lag_limit: None,
            capture: None,
        }
    }

    /// Включить `POST /api/v1/capture`.
    pub fn with_capture(mut self, capture: CaptureState) -> Self {
        self.capture = Some(capture);
        self
    }

    /// Считать сервис неготовым, если обновление ждёт обработки дольше `seconds`.
    pub fn with_queue_lag_limit(mut self, seconds: u32) -> Self {
        self.queue_lag_limit = Some(chrono::Duration::seconds(i64::from(seconds)));
        self
    }

    /// Включить приём Telegram webhook.
    pub fn with_telegram(mut self, ingress: Ingress) -> Self {
        self.telegram = Some(ingress);
        self
    }
}

/// Собрать роутер со всеми маршрутами и общими middleware.
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/health/live", get(health::live))
        .route("/health/ready", get(health::ready))
        .nest("/api/v1", api::router(&state))
        .route("/webhooks/telegram", post(webhook::receive))
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .layer(axum::middleware::from_fn(request_id))
        .with_state(state)
}

async fn not_found() -> ApiError {
    ApiError::not_found("Route not found")
}

async fn method_not_allowed() -> ApiError {
    ApiError::method_not_allowed("Method not allowed")
}
