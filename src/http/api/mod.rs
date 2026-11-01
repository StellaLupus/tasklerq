//! REST API v1.

pub mod auth;
pub mod calendar;
pub mod capture;
pub mod captures;
pub mod dto;
pub mod household;
pub mod items;
pub mod lists;
pub mod search;
pub mod snapshot;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::routing::{get, patch, post};
use chrono::Utc;
use sqlx::Transaction;
use sqlx::postgres::Postgres;

use crate::auth::tokens::AuthContext;
use crate::domain::models::{Actor, ActorType, ItemSource, UserRole};

use super::error::ApiError;
use super::router::AppState;

/// Собрать маршруты `/api/v1`.
pub fn router(state: &AppState) -> Router<AppState> {
    let mut router = Router::new()
        .route("/household", get(household::get))
        .route("/lists", get(lists::list).post(lists::create))
        .route("/lists/{id}", patch(lists::update).delete(lists::delete))
        .route("/lists/{id}/items", get(items::list).post(items::create))
        .route(
            "/items/{id}",
            get(items::get).patch(items::update).delete(items::delete),
        )
        .route("/items/{id}/complete", post(items::complete))
        .route("/items/{id}/reopen", post(items::reopen))
        .route("/items/{id}/undo", post(items::undo))
        .route("/search", get(search::search))
        .route("/calendar", get(calendar::calendar))
        .route("/snapshot", get(snapshot::snapshot));
    // Выключенный capture выглядит как отсутствующий маршрут: 404 в общем
    // конверте, без вызова LLM и без разбора тела.
    if state
        .capture
        .as_ref()
        .is_some_and(|capture| capture.config.enabled)
    {
        router = router.route(
            "/capture",
            post(capture::capture).layer(DefaultBodyLimit::max(capture::BODY_LIMIT_BYTES)),
        );
        router = router
            .route("/captures/undo-last", post(captures::undo_last))
            .route("/captures/{id}/confirm", post(captures::confirm))
            .route("/captures/{id}/cancel", post(captures::cancel))
            .route("/captures/{id}/undo", post(captures::undo));
    }
    router
}

/// Проверить scope и записать отказ в журнал безопасности.
///
/// Отказ не меняет ответ: клиент получает тот же `403`, запись идёт отдельной
/// короткой операцией и её ошибка не влияет на запрос.
pub async fn require_scope(
    state: &AppState,
    auth: &AuthContext,
    scope: &'static str,
) -> Result<(), ApiError> {
    if auth.require_scope(scope).is_ok() {
        return Ok(());
    }

    state
        .audit
        .api_auth_failed(
            "missing_scope",
            auth.route.as_deref().unwrap_or("unmatched"),
            Some((auth.token_id, auth.household_id)),
            Utc::now(),
        )
        .await;
    Err(ApiError::forbidden(format!(
        "Token is missing scope {scope}"
    )))
}

/// Проверить scope на чтение.
pub async fn require_read(state: &AppState, auth: &AuthContext) -> Result<(), ApiError> {
    require_scope(state, auth, "ha:read").await
}

/// Проверить scope на запись.
pub async fn require_write(state: &AppState, auth: &AuthContext) -> Result<(), ApiError> {
    require_scope(state, auth, "ha:write").await
}

/// Проверить scope на внешний capture.
pub async fn require_capture(state: &AppState, auth: &AuthContext) -> Result<(), ApiError> {
    require_scope(state, auth, "capture:write").await
}

/// Получить соединение из пула.
pub async fn connection(
    state: &AppState,
) -> Result<sqlx::pool::PoolConnection<Postgres>, ApiError> {
    state.pool.acquire().await.map_err(database_error)
}

/// Начать транзакцию.
pub async fn transaction(state: &AppState) -> Result<Transaction<'static, Postgres>, ApiError> {
    state.pool.begin().await.map_err(database_error)
}

/// Зафиксировать транзакцию.
pub async fn commit(transaction: Transaction<'static, Postgres>) -> Result<(), ApiError> {
    transaction.commit().await.map_err(database_error)
}

/// Актор внешнего API. Доверенный токен действует от имени интеграции, но
/// владелец нужен для операций со списками.
pub fn integration_actor(source: ItemSource) -> Actor {
    let actor_type = match source {
        ItemSource::HaVoice => ActorType::Voice,
        _ => ActorType::HomeAssistant,
    };
    Actor {
        user_id: None,
        role: Some(UserRole::Owner),
        actor_type,
    }
}

fn database_error(error: sqlx::Error) -> ApiError {
    tracing::error!(error = %error, "database operation failed");
    ApiError::internal("Database error")
}
