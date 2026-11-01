//! Извлечение и проверка Bearer-токена.

use axum::extract::{FromRequestParts, MatchedPath};
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use chrono::Utc;

use crate::auth::tokens::{self, AuthContext, AuthError};

use super::super::error::ApiError;
use super::super::router::AppState;

impl FromRequestParts<AppState> for AuthContext {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // Шаблон маршрута, а не сырой путь: идентификаторы не попадают в журнал.
        let route = parts
            .extensions
            .get::<MatchedPath>()
            .map_or_else(|| "unmatched".to_owned(), |path| path.as_str().to_owned());

        let token = parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let Some(token) = token else {
            return Err(reject(state, "missing_token", &route, None, "Missing bearer token").await);
        };

        match tokens::authenticate(&state.pool, token).await {
            Ok(mut context) => {
                context.route = Some(route);
                Ok(context)
            }
            Err(AuthError::Unknown) => {
                Err(reject(state, "unknown_token", &route, None, INVALID).await)
            }
            // `authenticate` не проверяет scopes, поэтому эта ветка
            // недостижима; `Forbidden` возвращает только `require_scope`
            // после успешной аутентификации.
            Err(AuthError::Forbidden(_)) => {
                unreachable!("authenticate never checks scopes")
            }
            Err(AuthError::Revoked {
                token_id,
                household_id,
            }) => Err(reject(
                state,
                "revoked_token",
                &route,
                Some((token_id, household_id)),
                INVALID,
            )
            .await),
            Err(AuthError::Expired {
                token_id,
                household_id,
            }) => Err(reject(
                state,
                "expired_token",
                &route,
                Some((token_id, household_id)),
                INVALID,
            )
            .await),
            Err(AuthError::Database(error)) => {
                tracing::error!(error = %error, "token authentication failed");
                Err(ApiError::internal("Database error"))
            }
        }
    }
}

const INVALID: &str = "Invalid or expired token";

/// Записать отказ аутентификации отдельной короткой операцией и вернуть ответ.
/// В журнал попадают только причина, шаблон маршрута и опознанный token id.
async fn reject(
    state: &AppState,
    reason: &'static str,
    route: &str,
    token: Option<(uuid::Uuid, uuid::Uuid)>,
    message: &'static str,
) -> ApiError {
    state
        .audit
        .api_auth_failed(reason, route, token, Utc::now())
        .await;
    ApiError::unauthorized(message)
}
