//! Календарь: развёртка сроков и повторений.

use axum::Json;
use axum::extract::{Query, State};
use chrono::NaiveDate;
use serde::Deserialize;

use crate::auth::tokens::AuthContext;
use crate::domain::calendar;
use crate::domain::recurrence::{MAX_CALENDAR_OCCURRENCES, parse_timezone};
use crate::storage::repositories::{households, items};

use super::super::error::ApiError;
use super::super::router::AppState;
use super::dto::CalendarResponse;
use super::{connection, require_read};

/// Параметры интервала.
#[derive(Debug, Deserialize)]
pub struct CalendarQuery {
    /// Начало интервала, `YYYY-MM-DD`.
    pub start: Option<String>,
    /// Конец интервала, `YYYY-MM-DD` (исключающий).
    pub end: Option<String>,
}

/// `GET /api/v1/calendar`.
pub async fn calendar(
    State(state): State<AppState>,
    auth: AuthContext,
    Query(query): Query<CalendarQuery>,
) -> Result<Json<CalendarResponse>, ApiError> {
    require_read(&auth)?;

    let start = parse_date(query.start.as_deref(), "start")?;
    let end = parse_date(query.end.as_deref(), "end")?;
    calendar::validate_range(start, end)?;

    let mut conn = connection(&state).await?;
    let household = households::get(&mut conn, auth.household_id).await?;
    let timezone = parse_timezone(&household.timezone)?;
    let items = items::calendar_items(&mut conn, auth.household_id).await?;

    let mut events = Vec::new();
    for item in &items {
        events.extend(calendar::expand_item(item, start, end, timezone)?);
    }
    if events.len() > MAX_CALENDAR_OCCURRENCES {
        return Err(ApiError::validation(format!(
            "calendar interval produces more than {MAX_CALENDAR_OCCURRENCES} occurrences"
        )));
    }

    events.sort_by_key(|event| event.sort_key(timezone));
    Ok(Json(CalendarResponse { events }))
}

fn parse_date(value: Option<&str>, field: &str) -> Result<NaiveDate, ApiError> {
    let value = value.ok_or_else(|| ApiError::validation(format!("{field} is required")))?;
    NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .map_err(|_| ApiError::validation(format!("{field} must be YYYY-MM-DD")))
}
