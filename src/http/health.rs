//! Проверки живости и готовности.

use axum::Json;
use axum::extract::State;
use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::VERSION;

use super::error::ApiError;
use super::router::AppState;

/// Ответ health-эндпоинта.
#[derive(Debug, Serialize)]
pub struct HealthResponse {
    /// `ok`, если проверка пройдена.
    pub status: &'static str,
    /// Версия сервиса.
    pub version: &'static str,
    /// Время проверки в UTC.
    pub time: DateTime<Utc>,
}

/// `GET /health/live`: процесс отвечает.
pub async fn live() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        version: VERSION,
        time: Utc::now(),
    })
}

/// `GET /health/ready`: база доступна и принимает запросы.
pub async fn ready(State(state): State<AppState>) -> Result<Json<HealthResponse>, ApiError> {
    sqlx::query("SELECT 1")
        .execute(&state.pool)
        .await
        .map_err(|error| {
            tracing::warn!(error = %error, "readiness check failed");
            ApiError::service_unavailable("Database is unavailable")
        })?;

    Ok(Json(HealthResponse {
        status: "ok",
        version: VERSION,
        time: Utc::now(),
    }))
}
