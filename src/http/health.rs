//! Проверки живости и готовности.

use axum::Json;
use axum::extract::State;
use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::VERSION;
use crate::config::TenancyMode;
use crate::storage::repositories::households;
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
    /// Необязательные предупреждения о деградации; поле отсутствует, когда их
    /// нет. Имена предупреждений фиксированы: `multiple_households`,
    /// `llm_unavailable`, `llm_slot_unverified:<slot>` и
    /// `llm_slot_unverified:<slot>:<режим>` для сохранённой проверки.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warnings: Option<Vec<String>>,
}

/// `GET /health/live`: процесс отвечает.
pub async fn live() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        version: VERSION,
        time: Utc::now(),
        warnings: None,
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
    let mut warnings: Vec<String> = Vec::new();
    // Очередь и счёт household делят одно соединение. Счёт идёт после проверки
    // очереди: при 503 из-за лага он уже не нужен.
    if state.queue_lag_limit.is_some() || state.tenancy_mode == TenancyMode::Single {
        let mut conn = state.pool.acquire().await.map_err(|error| {
            tracing::warn!(error = %error, "readiness check failed");
            ApiError::service_unavailable("Database is unavailable")
        })?;

        if let Some(limit) = state.queue_lag_limit {
            // Упавший или остановленный воркер заметен по самому старому
            // обновлению, которое давно могло быть обработано.
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

        if state.tenancy_mode == TenancyMode::Single {
            // Счёт читается при каждом запросе: после `household delete`
            // предупреждение исчезает без перезапуска.
            let active = households::count_active(&mut conn).await.map_err(|error| {
                tracing::warn!(error = %error, "readiness household count failed");
                ApiError::service_unavailable("Database is unavailable")
            })?;
            if active > 1 {
                warnings.push("multiple_households".to_owned());
            }
        }
    }

    // Статус слотов LLM — предупреждение, а не отказ: без LLM сервис работает с
    // деградацией, и падение readiness вывело бы из работы то, что от него не
    // зависит.
    if let Some(llm_slots) = &state.llm_slots {
        if llm_slots.all_breakers_open(now) {
            warnings.push("llm_unavailable".to_owned());
        }
        for report in llm_slots.statuses(&state.pool, now).await {
            if !report.status.is_passed() {
                match report.titles {
                    Some(titles) => warnings.push(format!(
                        "llm_slot_unverified:{}:{}",
                        report.slot,
                        titles.as_str()
                    )),
                    None => warnings.push(format!("llm_slot_unverified:{}", report.slot)),
                }
            }
        }
    }

    Ok(Json(HealthResponse {
        status: "ok",
        version: VERSION,
        time: now,
        warnings: (!warnings.is_empty()).then_some(warnings),
    }))
}
