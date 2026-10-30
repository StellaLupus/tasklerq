//! Подтверждение, отмена и undo внешнего capture.
//!
//! Эндпоинты работают только с capture своего household и только с внешними
//! источниками: Telegram-черновик живёт в боте и через API не виден. Чужой,
//! несуществующий и не внешний capture не отличаются в ответе — все дают `404`.

use std::collections::HashMap;

use axum::Json;
use axum::extract::{Path, State};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::capture::draft::{self, DraftError};
use crate::domain::error::DomainError;
use crate::domain::models::{CaptureSource, ListKind};
use crate::storage::repositories::drafts::{self, Draft};
use crate::storage::repositories::lists;

use super::super::error::ApiError;
use super::super::router::AppState;
use super::capture::{
    CaptureAuth, CaptureResponse, Status, applied_speech, elements, fit_speech, item_response,
};
use super::{connection, integration_actor};

/// Итог отмены черновика или undo.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ActionStatus {
    /// Черновик отменён.
    Cancelled,
    /// Применённый capture отменён.
    Undone,
}

/// Ответ `POST /api/v1/captures/{id}/cancel`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancelResponse {
    /// Идентификатор capture.
    pub capture_id: Uuid,
    /// Итог.
    pub status: ActionStatus,
    /// Короткая фраза для озвучивания.
    pub speech: String,
}

/// Ответ `POST /api/v1/captures/{id}/undo`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UndoResponse {
    /// Идентификатор capture.
    pub capture_id: Uuid,
    /// Итог.
    pub status: ActionStatus,
    /// Сколько элементов мягко удалено этим запросом.
    pub removed: usize,
    /// Короткая фраза для озвучивания.
    pub speech: String,
}

/// Подтвердить внешний черновик.
pub async fn confirm(
    State(state): State<AppState>,
    auth: CaptureAuth,
    Path(capture_id): Path<Uuid>,
) -> Result<Json<CaptureResponse>, ApiError> {
    let auth = auth.0;
    let mut conn = connection(&state).await?;
    let draft = external_capture(&mut conn, auth.household_id, capture_id).await?;
    let actor = integration_actor(draft::item_source(draft.source));

    let confirmed = draft::confirm(&mut conn, auth.household_id, &actor, capture_id, Utc::now())
        .await
        .map_err(action_error)?;

    let kinds = list_kinds(&mut conn, auth.household_id).await?;
    let mut items = Vec::with_capacity(confirmed.items.len());
    for item in &confirmed.items {
        let kind = kinds.get(&item.list_id).copied().ok_or_else(|| {
            tracing::error!(item_id = %item.id, "capture item has no list");
            ApiError::internal("Database error")
        })?;
        items.push(item_response(item, kind));
    }

    Ok(Json(CaptureResponse {
        capture_id,
        status: Status::Applied,
        fallback: false,
        speech: fit_speech(applied_speech(&items)),
        items,
        draft_id: None,
        expires_at: None,
        clarifications: Vec::new(),
    }))
}

/// Отменить внешний черновик. Повторная отмена и отмена истёкшего — успех.
pub async fn cancel(
    State(state): State<AppState>,
    auth: CaptureAuth,
    Path(capture_id): Path<Uuid>,
) -> Result<Json<CancelResponse>, ApiError> {
    let auth = auth.0;
    let mut conn = connection(&state).await?;
    external_capture(&mut conn, auth.household_id, capture_id).await?;

    draft::cancel(&mut conn, auth.household_id, capture_id, Utc::now())
        .await
        .map_err(action_error)?;

    Ok(Json(CancelResponse {
        capture_id,
        status: ActionStatus::Cancelled,
        speech: "Отменил черновик.".to_owned(),
    }))
}

/// Отменить применённый capture: мягко удалить все его элементы.
pub async fn undo(
    State(state): State<AppState>,
    auth: CaptureAuth,
    Path(capture_id): Path<Uuid>,
) -> Result<Json<UndoResponse>, ApiError> {
    let auth = auth.0;
    let mut conn = connection(&state).await?;
    let draft = external_capture(&mut conn, auth.household_id, capture_id).await?;
    let actor = integration_actor(draft::item_source(draft.source));

    let removed = draft::undo(&mut conn, auth.household_id, &actor, capture_id, Utc::now())
        .await
        .map_err(action_error)?;

    let speech = if removed.is_empty() {
        "Отменять нечего: элементы уже удалены.".to_owned()
    } else {
        fit_speech(format!("Отменил добавление: {}.", elements(removed.len())))
    };

    Ok(Json(UndoResponse {
        capture_id,
        status: ActionStatus::Undone,
        removed: removed.len(),
        speech,
    }))
}

/// Capture своего household и внешнего источника; иначе — `404`.
async fn external_capture(
    conn: &mut PgConnection,
    household_id: Uuid,
    capture_id: Uuid,
) -> Result<Draft, ApiError> {
    let draft = match drafts::get(conn, household_id, capture_id).await {
        Ok(draft) => draft,
        Err(DomainError::NotFound) => return Err(capture_not_found()),
        Err(error) => return Err(error.into()),
    };
    if !is_external(draft.source) {
        return Err(capture_not_found());
    }
    Ok(draft)
}

fn is_external(source: CaptureSource) -> bool {
    matches!(
        source,
        CaptureSource::HaVoice
            | CaptureSource::HaAutomation
            | CaptureSource::Api
            | CaptureSource::Shortcut
    )
}

fn capture_not_found() -> ApiError {
    ApiError::not_found("Capture not found")
}

async fn list_kinds(
    conn: &mut PgConnection,
    household_id: Uuid,
) -> Result<HashMap<Uuid, ListKind>, ApiError> {
    let kinds = lists::list(conn, household_id)
        .await?
        .into_iter()
        .map(|list| (list.id, list.kind))
        .collect();
    Ok(kinds)
}

fn action_error(error: DraftError) -> ApiError {
    match error {
        DraftError::Expired => ApiError::gone("Draft has expired"),
        DraftError::Cancelled => ApiError::conflict("Capture was cancelled"),
        DraftError::Applied => {
            ApiError::conflict("Capture is already applied; use undo to cancel it")
        }
        DraftError::NotApplied => ApiError::conflict("Capture is not applied"),
        DraftError::UndoExpired => ApiError::conflict("The undo window is closed"),
        DraftError::Changed => ApiError::conflict("Capture items were changed"),
        DraftError::Invalid(_) => {
            tracing::warn!("capture items became invalid before they could be applied");
            ApiError::validation("Capture items are no longer valid")
        }
        DraftError::Domain(error) => error.into(),
        other => {
            tracing::error!(%other, "capture action failed");
            ApiError::internal("Database error")
        }
    }
}
