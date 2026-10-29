//! Поиск и фильтры.

use axum::Json;
use axum::extract::{Query, State};
use serde::Deserialize;
use uuid::Uuid;

use crate::auth::tokens::AuthContext;
use crate::domain::models::ItemStatus;
use crate::domain::search::{DEFAULT_LIMIT, SearchFilters, decode_cursor, parse_due_bound};
use crate::storage::repositories::{households, items};

use super::super::error::ApiError;
use super::super::router::AppState;
use super::dto::{ItemResponse, SearchItemResponse, SearchResponse};
use super::{connection, require_read};

/// Параметры строки запроса.
#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    /// Текст.
    pub q: Option<String>,
    /// `open`, `done` или `all`.
    pub status: Option<String>,
    /// Список.
    pub list_id: Option<String>,
    /// Исполнитель.
    pub assignee_user_id: Option<String>,
    /// Тег.
    pub tag: Option<String>,
    /// Начало диапазона сроков.
    pub due_from: Option<String>,
    /// Конец диапазона сроков.
    pub due_to: Option<String>,
    /// Размер страницы.
    pub limit: Option<String>,
    /// Курсор.
    pub cursor: Option<String>,
}

/// `GET /api/v1/search`.
pub async fn search(
    State(state): State<AppState>,
    auth: AuthContext,
    Query(query): Query<SearchQuery>,
) -> Result<Json<SearchResponse>, ApiError> {
    require_read(&auth)?;

    let (status, all_status) = match query.status.as_deref() {
        None | Some("open") => (Some(ItemStatus::Open), false),
        Some("done") => (Some(ItemStatus::Done), false),
        Some("all") => (Some(ItemStatus::Open), true),
        Some(_) => {
            return Err(ApiError::validation(
                "status must be one of open, done or all",
            ));
        }
    };

    let limit = match query.limit.as_deref() {
        Some(value) => value
            .parse::<i64>()
            .map_err(|_| ApiError::validation("limit must be an integer"))?,
        None => DEFAULT_LIMIT,
    };

    let filters = SearchFilters {
        q: query.q,
        status,
        all_status,
        list_id: parse_uuid(query.list_id.as_deref(), "list_id")?,
        assignee_user_id: parse_uuid(query.assignee_user_id.as_deref(), "assignee_user_id")?,
        tag: query.tag,
        due_from: parse_due_bound(query.due_from.as_deref(), false)?,
        due_to: parse_due_bound(query.due_to.as_deref(), true)?,
        limit,
        cursor: query.cursor.as_deref().map(decode_cursor).transpose()?,
    };
    filters.validate()?;

    let mut conn = connection(&state).await?;
    let household = households::get(&mut conn, auth.household_id).await?;
    let (hits, next_cursor) =
        items::search(&mut conn, auth.household_id, &household.timezone, &filters).await?;

    Ok(Json(SearchResponse {
        items: hits
            .into_iter()
            .map(|hit| SearchItemResponse {
                item: ItemResponse::from(hit.item),
                list_name: hit.list_name,
                list_kind: hit.list_kind,
                rank: hit.rank,
            })
            .collect(),
        next_cursor,
    }))
}

fn parse_uuid(value: Option<&str>, field: &str) -> Result<Option<Uuid>, ApiError> {
    value
        .map(|value| {
            Uuid::parse_str(value)
                .map_err(|_| ApiError::validation(format!("{field} must be a UUID")))
        })
        .transpose()
}
