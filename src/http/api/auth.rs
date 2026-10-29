//! Извлечение и проверка Bearer-токена.

use axum::extract::FromRequestParts;
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;

use crate::auth::tokens::{self, AuthContext, AuthError};

use super::super::error::ApiError;
use super::super::router::AppState;

impl FromRequestParts<AppState> for AuthContext {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let token = parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| ApiError::unauthorized("Missing bearer token"))?;

        match tokens::authenticate(&state.pool, token).await {
            Ok(Some(context)) => Ok(context),
            Ok(None) => Err(ApiError::unauthorized("Invalid or expired token")),
            Err(AuthError::Database(error)) => {
                tracing::error!(error = %error, "token authentication failed");
                Err(ApiError::internal("Database error"))
            }
            Err(_) => Err(ApiError::unauthorized("Invalid or expired token")),
        }
    }
}
