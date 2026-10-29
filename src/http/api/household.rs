//! `GET /api/v1/household`.

use axum::Json;
use axum::extract::State;

use crate::auth::tokens::AuthContext;
use crate::storage::repositories::households;

use super::super::error::ApiError;
use super::super::router::AppState;
use super::dto::HouseholdResponse;
use super::{connection, require_read};

/// Household, участники и настройки.
pub async fn get(
    State(state): State<AppState>,
    auth: AuthContext,
) -> Result<Json<HouseholdResponse>, ApiError> {
    require_read(&auth)?;
    let mut conn = connection(&state).await?;

    let household = households::get(&mut conn, auth.household_id).await?;
    let members = households::members(&mut conn, auth.household_id).await?;

    Ok(Json(HouseholdResponse { household, members }))
}
