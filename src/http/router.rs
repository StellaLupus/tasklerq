//! Сборка HTTP-роутера.

use axum::Router;
use axum::routing::get;
use sqlx::PgPool;

use super::api;
use super::error::ApiError;
use super::health;
use super::middleware::request_id;

/// Состояние приложения, доступное обработчикам.
#[derive(Clone)]
pub struct AppState {
    /// Пул соединений PostgreSQL.
    pub pool: PgPool,
}

impl AppState {
    /// Создать состояние из пула.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

/// Собрать роутер со всеми маршрутами и общими middleware.
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/health/live", get(health::live))
        .route("/health/ready", get(health::ready))
        .nest("/api/v1", api::router())
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
