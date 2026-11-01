//! Проверки живости и готовности.

use axum::Json;
use axum::extract::State;
use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::VERSION;
use crate::telegram::queue;

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

/// `GET /health/ready`: база доступна и принимает запросы, миграции применены,
/// а очередь обновлений Telegram не застряла (если воркер запущен).
pub async fn ready(State(state): State<AppState>) -> Result<Json<HealthResponse>, ApiError> {
    sqlx::query("SELECT 1")
        .execute(&state.pool)
        .await
        .map_err(|error| {
            tracing::warn!(error = %error, "readiness check failed");
            ApiError::service_unavailable("Database is unavailable")
        })?;

    let migrations = crate::storage::migrations::all_applied(&state.pool)
        .await
        .map_err(|error| {
            tracing::warn!(error = %error, "readiness migration check failed");
            ApiError::service_unavailable("Database migrations are not applied")
        })?;
    if !migrations {
        tracing::warn!("not all embedded migrations are applied");
        return Err(ApiError::service_unavailable(
            "Database migrations are not applied",
        ));
    }

    let now = Utc::now();
    if let Some(limit) = state.queue_lag_limit {
        // Упавший или остановленный воркер заметен по самому старому обновлению,
        // которое давно могло быть обработано.
        let mut conn = state.pool.acquire().await.map_err(|error| {
            tracing::warn!(error = %error, "readiness check failed");
            ApiError::service_unavailable("Database is unavailable")
        })?;
        let oldest = queue::oldest_ready(&mut conn, now).await.map_err(|error| {
            tracing::warn!(error = %error, "readiness check failed");
            ApiError::service_unavailable("Database is unavailable")
        })?;
        if oldest.is_some_and(|oldest| now - oldest > limit) {
            tracing::warn!("telegram update queue is lagging");
            return Err(ApiError::service_unavailable(
                "Telegram update queue is lagging",
            ));
        }
    }

    Ok(Json(HealthResponse {
        status: "ok",
        version: VERSION,
        time: now,
    }))
}
