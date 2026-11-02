//! Сборка HTTP-роутера.

use axum::Router;
use axum::routing::{get, post};
use sqlx::PgPool;

use super::api;
use super::api::capture::CaptureState;
use super::error::ApiError;
use super::health;
use super::metrics;
use super::middleware::request_id;
use crate::config::TenancyMode;
use crate::security_audit::Audit;
use crate::telegram::webhook::{self, Ingress};

/// Состояние приложения, доступное обработчикам.
#[derive(Clone)]
pub struct AppState {
    /// Пул соединений PostgreSQL.
    pub pool: PgPool,
    /// Журнал событий безопасности.
    pub audit: Audit,
    /// Приём Telegram webhook; `None`, если webhook выключен.
    pub telegram: Option<Ingress>,
    /// Допустимый возраст необработанного обновления Telegram; `None`, если
    /// воркер обновлений не запущен и очередь не проверяется.
    pub queue_lag_limit: Option<chrono::Duration>,
    /// Режим размещения: `/health/ready` считает предупреждения только в
    /// `single`. Боевой путь выставляет значение из `config.tenancy.mode`.
    pub tenancy_mode: TenancyMode,
    /// Внешний capture; `None`, если маршрут выключен.
    pub capture: Option<CaptureState>,
}

impl AppState {
    /// Создать состояние из пула и общего журнала событий безопасности.
    ///
    /// Журнал передаётся аргументом, чтобы production-путь не мог случайно
    /// создать отдельное состояние подавления: [`crate::app::build_application`]
    /// собирает один `Audit` на процесс и передаёт его сюда, в воркеры и бота.
    pub fn new(pool: PgPool, audit: Audit) -> Self {
        Self {
            audit,
            pool,
            telegram: None,
            queue_lag_limit: None,
            tenancy_mode: TenancyMode::Single,
            capture: None,
        }
    }

    /// Включить `POST /api/v1/capture`.
    pub fn with_capture(mut self, capture: CaptureState) -> Self {
        self.capture = Some(capture);
        self
    }

    /// Задать режим размещения для проверки деградации в `/health/ready`.
    pub fn with_tenancy_mode(mut self, mode: TenancyMode) -> Self {
        self.tenancy_mode = mode;
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
        .route("/metrics", get(metrics::scrape))
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
