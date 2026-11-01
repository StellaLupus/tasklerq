//! `GET /metrics`: метрики в формате Prometheus exposition 0.0.4.
//!
//! Эндпоинт не публичный: нужен Bearer-токен со scope `metrics:read`.
//! Токен в query, как и во всём API, не принимается.

use axum::extract::State;
use axum::http::header::CONTENT_TYPE;
use axum::response::{IntoResponse, Response};

use crate::auth::tokens::AuthContext;
use crate::metrics;

use super::error::ApiError;
use super::router::AppState;

/// Scope для чтения метрик.
pub const SCOPE: &str = "metrics:read";

/// `GET /metrics`.
pub async fn scrape(
    State(state): State<AppState>,
    auth: AuthContext,
) -> Result<Response, ApiError> {
    auth.require_scope(SCOPE)
        .map_err(|_| ApiError::forbidden("Token is missing scope metrics:read"))?;

    let text = metrics::render_with_database_gauges(&state.pool)
        .await
        .map_err(|error| {
            tracing::warn!(error = %error, "metrics database check failed");
            ApiError::service_unavailable("Database is unavailable")
        })?;

    Ok((
        [(CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")],
        text,
    )
        .into_response())
}
