//! Элементы: создание, карточка, изменение и жизненный цикл.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use uuid::Uuid;

use crate::auth::tokens::AuthContext;
use crate::domain::models::{ItemSource, NewItem};
use crate::domain::recurrence::parse_timezone;
use crate::domain::validation::validate_new_item;
use crate::storage::repositories::{households, items, lists};

use super::super::error::ApiError;
use super::super::router::AppState;
use super::dto::{
    CreateItemRequest, ItemResponse, ItemsResponse, UpdateItemRequest, VersionRequest,
};
use super::{commit, connection, integration_actor, require_read, require_write, transaction};

/// Элементы списка.
pub async fn list(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(list_id): Path<Uuid>,
) -> Result<Json<ItemsResponse>, ApiError> {
    require_read(&auth)?;
    let mut conn = connection(&state).await?;
    let items = items::list_by_list(&mut conn, auth.household_id, list_id).await?;
    Ok(Json(ItemsResponse {
        items: items.into_iter().map(ItemResponse::from).collect(),
    }))
}

/// Создать задачу или позицию списка.
pub async fn create(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(list_id): Path<Uuid>,
    Json(request): Json<CreateItemRequest>,
) -> Result<(StatusCode, Json<ItemResponse>), ApiError> {
    require_write(&auth)?;

    let mut tx = transaction(&state).await?;
    let list = lists::get_visible(&mut tx, auth.household_id, list_id).await?;
    let household = households::get(&mut tx, auth.household_id).await?;
    let timezone = parse_timezone(&household.timezone)?;
    let today = chrono::Utc::now().with_timezone(&timezone).date_naive();

    let source = request.source.unwrap_or(ItemSource::Api);
    let new_item = NewItem {
        list_id,
        title: request.title,
        notes: request.notes,
        priority: request.priority.unwrap_or_default(),
        due_at: request.due_at,
        due_date: request.due_date,
        assignee_user_id: request.assignee_user_id,
        source,
        quantity: request.quantity,
        unit: request.unit,
        tags: request.tags,
        subtasks: request.subtasks,
        recurrence: request.recurrence,
    };
    let validated = validate_new_item(list.kind, &household.timezone, new_item, today)?;
    let actor = integration_actor(source);

    let item = items::create(
        &mut tx,
        auth.household_id,
        &actor,
        list_id,
        validated,
        request.assignee_user_id,
        chrono::Utc::now(),
    )
    .await?;
    commit(tx).await?;

    Ok((StatusCode::CREATED, Json(ItemResponse::from(item))))
}

/// Карточка элемента.
pub async fn get(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(id): Path<Uuid>,
) -> Result<Json<ItemResponse>, ApiError> {
    require_read(&auth)?;
    let mut conn = connection(&state).await?;
    let item = items::get(&mut conn, auth.household_id, id).await?;
    Ok(Json(ItemResponse::from(item)))
}

/// Изменить элемент.
pub async fn update(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(id): Path<Uuid>,
    Json(request): Json<UpdateItemRequest>,
) -> Result<Json<ItemResponse>, ApiError> {
    require_write(&auth)?;

    let mut tx = transaction(&state).await?;
    let household = households::get(&mut tx, auth.household_id).await?;
    let item = items::update(
        &mut tx,
        auth.household_id,
        &auth.actor,
        id,
        request.version,
        request.patch,
        &household.timezone,
        chrono::Utc::now(),
    )
    .await?;
    commit(tx).await?;

    Ok(Json(ItemResponse::from(item)))
}

/// Мягко удалить элемент.
pub async fn delete(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    require_write(&auth)?;

    let mut tx = transaction(&state).await?;
    items::soft_delete(
        &mut tx,
        auth.household_id,
        &auth.actor,
        id,
        chrono::Utc::now(),
    )
    .await?;
    commit(tx).await?;

    Ok(StatusCode::NO_CONTENT)
}

/// Завершить элемент.
pub async fn complete(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(id): Path<Uuid>,
    body: Option<Json<VersionRequest>>,
) -> Result<Json<ItemResponse>, ApiError> {
    require_write(&auth)?;

    let mut tx = transaction(&state).await?;
    let household = households::get(&mut tx, auth.household_id).await?;
    let version = body.and_then(|Json(body)| body.version);
    let item = items::complete(
        &mut tx,
        auth.household_id,
        &auth.actor,
        id,
        version,
        &household.timezone,
        chrono::Utc::now(),
    )
    .await?;
    commit(tx).await?;

    Ok(Json(ItemResponse::from(item)))
}

/// Открыть завершённый элемент.
pub async fn reopen(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(id): Path<Uuid>,
    body: Option<Json<VersionRequest>>,
) -> Result<Json<ItemResponse>, ApiError> {
    require_write(&auth)?;

    let mut tx = transaction(&state).await?;
    let version = body.and_then(|Json(body)| body.version);
    let item = items::reopen(
        &mut tx,
        auth.household_id,
        &auth.actor,
        id,
        version,
        chrono::Utc::now(),
    )
    .await?;
    commit(tx).await?;

    Ok(Json(ItemResponse::from(item)))
}

/// Отменить последнее действие над элементом.
pub async fn undo(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(id): Path<Uuid>,
) -> Result<Json<ItemResponse>, ApiError> {
    require_write(&auth)?;

    let mut tx = transaction(&state).await?;
    let item = items::undo_last(
        &mut tx,
        auth.household_id,
        &auth.actor,
        id,
        chrono::Utc::now(),
    )
    .await?;
    commit(tx).await?;

    Ok(Json(ItemResponse::from(item)))
}
