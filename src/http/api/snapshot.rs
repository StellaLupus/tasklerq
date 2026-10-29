//! Инкрементальный снапшот для Home Assistant.

use axum::Json;
use axum::extract::{Query, State};
use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde::Deserialize;

use crate::auth::tokens::AuthContext;
use crate::domain::calendar::{self, CalendarEvent};
use crate::domain::recurrence::{MAX_CALENDAR_OCCURRENCES, parse_timezone};
use crate::storage::repositories::{households, items, lists};

use super::super::error::ApiError;
use super::super::router::AppState;
use super::dto::{ItemResponse, SnapshotResponse};
use super::{connection, require_read};

/// Параметры снапшота.
#[derive(Debug, Deserialize)]
pub struct SnapshotQuery {
    /// Курсор предыдущего ответа.
    pub since: Option<String>,
    /// Конец календарного горизонта, `YYYY-MM-DD`.
    pub calendar_end: Option<String>,
}

/// `GET /api/v1/snapshot`.
pub async fn snapshot(
    State(state): State<AppState>,
    auth: AuthContext,
    Query(query): Query<SnapshotQuery>,
) -> Result<Json<SnapshotResponse>, ApiError> {
    require_read(&auth)?;

    let since = parse_since(query.since.as_deref())?;
    let server_time = Utc::now();

    let mut conn = connection(&state).await?;
    let household = households::get(&mut conn, auth.household_id).await?;
    let lists = lists::list(&mut conn, auth.household_id).await?;
    let (items, deleted_item_ids) = items::snapshot(&mut conn, auth.household_id, since).await?;

    let mut calendar_events = Vec::new();
    let mut calendar_truncated = false;
    if let Some(value) = query.calendar_end.as_deref() {
        let end = NaiveDate::parse_from_str(value, "%Y-%m-%d")
            .map_err(|_| ApiError::validation("calendar_end must be YYYY-MM-DD"))?;
        let timezone = parse_timezone(&household.timezone)?;
        let start = server_time.with_timezone(&timezone).date_naive();
        if end > start {
            calendar::validate_range(start, end)?;
            for item in items::calendar_items(&mut conn, auth.household_id).await? {
                calendar_events.extend(calendar::expand_item(&item, start, end, timezone)?);
            }
            calendar_events.sort_by_key(CalendarEvent::sort_key);
            // Календарь в снапшоте вспомогательный: усекаем до лимита, а не
            // роняем весь ответ, иначе HA теряет и todo-синхронизацию. Полный
            // горизонт HA берёт через GET /calendar с его лимитом.
            if calendar_events.len() > MAX_CALENDAR_OCCURRENCES {
                calendar_events.truncate(MAX_CALENDAR_OCCURRENCES);
                calendar_truncated = true;
            }
        }
    }

    Ok(Json(SnapshotResponse {
        server_time,
        next_since: server_time - Duration::seconds(5),
        lists,
        items: items.into_iter().map(ItemResponse::from).collect(),
        deleted_item_ids,
        calendar_events,
        calendar_truncated,
    }))
}

fn parse_since(value: Option<&str>) -> Result<Option<DateTime<Utc>>, ApiError> {
    value
        .map(|value| {
            DateTime::parse_from_rfc3339(value)
                .map(|at| at.with_timezone(&Utc))
                .map_err(|_| ApiError::validation("since must be RFC 3339"))
        })
        .transpose()
}
