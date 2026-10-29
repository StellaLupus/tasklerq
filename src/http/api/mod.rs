//! REST API v1.

pub mod auth;
pub mod calendar;
pub mod dto;
pub mod household;
pub mod items;
pub mod lists;
pub mod search;
pub mod snapshot;

use axum::Router;
use axum::routing::{get, patch, post};
use sqlx::Transaction;
use sqlx::postgres::Postgres;

use crate::auth::tokens::AuthContext;
use crate::domain::models::{Actor, ActorType, ItemSource, UserRole};

use super::error::ApiError;
use super::router::AppState;

/// Собрать маршруты `/api/v1`.
pub fn router() -> Router<AppState> {
    Router::new()
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
        .route("/snapshot", get(snapshot::snapshot))
}

/// Проверить scope на чтение.
pub fn require_read(auth: &AuthContext) -> Result<(), ApiError> {
    auth.require_scope("ha:read")
        .map_err(|_| ApiError::forbidden("Token is missing scope ha:read"))
}

/// Проверить scope на запись.
pub fn require_write(auth: &AuthContext) -> Result<(), ApiError> {
    auth.require_scope("ha:write")
        .map_err(|_| ApiError::forbidden("Token is missing scope ha:write"))
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
