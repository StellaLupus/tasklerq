//! Списки: чтение, создание, переименование, архивирование и удаление.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use uuid::Uuid;

use crate::auth::tokens::AuthContext;
use crate::domain::error::DomainError;
use crate::domain::models::{List, ListKind};
use crate::storage::repositories::lists;

use super::super::error::ApiError;
use super::super::router::AppState;
use super::dto::{CreateListRequest, UpdateListRequest};
use super::{commit, connection, require_read, require_write, transaction};

/// Все неудалённые списки household.
pub async fn list(
    State(state): State<AppState>,
    auth: AuthContext,
) -> Result<Json<Vec<List>>, ApiError> {
    require_read(&auth)?;
    let mut conn = connection(&state).await?;
    Ok(Json(lists::list(&mut conn, auth.household_id).await?))
}

/// Создать дополнительный список.
pub async fn create(
    State(state): State<AppState>,
    auth: AuthContext,
    Json(request): Json<CreateListRequest>,
) -> Result<(StatusCode, Json<List>), ApiError> {
    require_write(&auth)?;
    if request.kind == ListKind::Tasks {
        return Err(ApiError::validation(
            "additional lists must have kind shopping or checklist",
        ));
    }

    let mut tx = transaction(&state).await?;
    let list = lists::create(
        &mut tx,
        auth.household_id,
        request.kind,
        &request.name,
        chrono::Utc::now(),
    )
    .await?;
    commit(tx).await?;

    Ok((StatusCode::CREATED, Json(list)))
}

/// Переименовать или архивировать список.
pub async fn update(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(id): Path<Uuid>,
    Json(request): Json<UpdateListRequest>,
) -> Result<Json<List>, ApiError> {
    require_write(&auth)?;

    let mut tx = transaction(&state).await?;
    let list = lists::update(
        &mut tx,
        auth.household_id,
        id,
        request.name.as_deref(),
        request.is_archived,
        auth.actor.is_owner(),
        chrono::Utc::now(),
    )
    .await?;
    commit(tx).await?;

    Ok(Json(list))
}

/// Мягко удалить список.
pub async fn delete(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    require_write(&auth)?;

    let mut tx = transaction(&state).await?;
    lists::delete(
        &mut tx,
        auth.household_id,
        id,
        auth.actor.is_owner(),
        chrono::Utc::now(),
    )
    .await?;
    commit(tx).await?;

    Ok(StatusCode::NO_CONTENT)
}

/// Проверить, что список принадлежит household (для вложенных маршрутов).
pub async fn ensure_list(
    state: &AppState,
    household_id: Uuid,
    list_id: Uuid,
) -> Result<List, ApiError> {
    let mut conn = connection(state).await?;
    lists::get_visible(&mut conn, household_id, list_id)
        .await
        .map_err(|error| match error {
            DomainError::NotFound => ApiError::not_found("List not found"),
            other => other.into(),
        })
}
