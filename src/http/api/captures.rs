//! Подтверждение, отмена и undo внешнего capture.
//!
//! Эндпоинты работают только с capture своего household и только с внешними
//! источниками: Telegram-черновик живёт в боте и через API не виден. Чужой,
//! несуществующий и не внешний capture не отличаются в ответе — все дают `404`.

use std::collections::{HashMap, HashSet};

use axum::Json;
use axum::extract::{Path, State};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::capture::draft::{self, DraftError};
use crate::domain::error::DomainError;
use crate::domain::models::{CaptureSource, Item, ListKind};
use crate::storage::repositories::drafts::{self, Draft};
use crate::storage::repositories::items;
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

/// Ответ `POST /api/v1/captures/undo-last`.
///
/// `capture_id` пуст, если за окно не нашлось подходящего голосового capture:
/// это не ошибка, отменять просто нечего.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UndoLastResponse {
    /// Идентификатор отменённого capture; `None`, если отменять нечего.
    pub capture_id: Option<Uuid>,
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

    // Применённый capture без живых элементов уже отменён: повторный confirm
    // не должен отвечать «Добавил 0 элементов».
    if confirmed.replayed && confirmed.items.is_empty() {
        return Err(ApiError::conflict("Capture was undone"));
    }

    let kinds = list_kinds(&mut conn, auth.household_id, &confirmed.items).await?;
    let items: Vec<_> = confirmed
        .items
        .iter()
        .zip(kinds)
        .map(|(item, kind)| item_response(item, kind))
        .collect();

    Ok(Json(CaptureResponse {
        capture_id,
        status: Status::Applied,
        fallback: draft.fallback,
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

    Ok(Json(UndoResponse {
        capture_id,
        status: ActionStatus::Undone,
        removed: removed.len(),
        speech: undo_speech(&removed),
    }))
}

/// Отменить последний голосовой capture household за окно undo.
///
/// Учитываются только `ha_voice` в статусе `applied`: `auto` (включая
/// fallback) и подтверждённый `draft`. Применённый capture остаётся
/// `applied` после отмены, поэтому повторная команда находит его же и
/// возвращает `removed=0`, не отменяя более ранние добавления.
pub async fn undo_last(
    State(state): State<AppState>,
    auth: CaptureAuth,
) -> Result<Json<UndoLastResponse>, ApiError> {
    let auth = auth.0;
    let mut conn = connection(&state).await?;
    let now = Utc::now();
    let since = now - chrono::Duration::minutes(items::UNDO_WINDOW_MINUTES);
    let Some(draft) = drafts::last_voice_applied(&mut conn, auth.household_id, since).await? else {
        return Ok(Json(UndoLastResponse {
            capture_id: None,
            status: ActionStatus::Undone,
            removed: 0,
            speech: format!(
                "Нечего отменять: за последние {} минут голосовых добавлений не было.",
                items::UNDO_WINDOW_MINUTES
            ),
        }));
    };
    let actor = integration_actor(draft::item_source(draft.source));
    let removed = draft::undo(&mut conn, auth.household_id, &actor, draft.id, now)
        .await
        .map_err(action_error)?;

    Ok(Json(UndoLastResponse {
        capture_id: Some(draft.id),
        status: ActionStatus::Undone,
        removed: removed.len(),
        speech: undo_speech(&removed),
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

/// Вид списка для каждого элемента, в том же порядке.
///
/// Список ищется по `item.list_id`, включая мягко удалённые: элемент capture
/// переживает удаление своего списка, и повторный `confirm` должен вернуть его
/// исходный вид, а не `500`.
async fn list_kinds(
    conn: &mut PgConnection,
    household_id: Uuid,
    items: &[Item],
) -> Result<Vec<ListKind>, ApiError> {
    let list_ids: Vec<Uuid> = items
        .iter()
        .map(|item| item.list_id)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let found: HashMap<Uuid, ListKind> = lists::kinds(conn, household_id, &list_ids)
        .await?
        .into_iter()
        .collect();
    items
        .iter()
        .map(|item| {
            found.get(&item.list_id).copied().ok_or_else(|| {
                tracing::error!(item_id = %item.id, "capture item has no list");
                ApiError::internal("Database error")
            })
        })
        .collect()
}

/// Фраза ответа об отмене capture.
fn undo_speech(removed: &[Item]) -> String {
    if removed.is_empty() {
        "Отменять нечего: элементы уже удалены.".to_owned()
    } else {
        fit_speech(format!("Отменил добавление: {}.", elements(removed.len())))
    }
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
