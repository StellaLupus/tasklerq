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

/// `GET /health/ready`: база доступна и принимает запросы, а очередь обновлений
/// Telegram не застряла (если воркер запущен).
pub async fn ready(State(state): State<AppState>) -> Result<Json<HealthResponse>, ApiError> {
    sqlx::query("SELECT 1")
        .execute(&state.pool)
        .await
        .map_err(|error| {
            tracing::warn!(error = %error, "readiness check failed");
            ApiError::service_unavailable("Database is unavailable")
        })?;

    let now = Utc::now();
    if let Some(limit) = state.queue_lag_limit {
        // Упавший или остановленный воркер заметен по самому старому обновлению,
        // которое давно могло быть обработано.
        let oldest: Option<DateTime<Utc>> = sqlx::query_scalar(
            "SELECT min(available_at) FROM telegram_updates
             WHERE status = 'pending' AND available_at <= $1",
        )
        .bind(now)
        .fetch_one(&state.pool)
        .await
        .map_err(|error| {
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
