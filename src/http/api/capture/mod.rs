//! Внешний capture: `POST /api/v1/capture`.
//!
//! Текст приходит от Home Assistant, ярлыка или интеграции, разбирается тем же
//! промптом и валидацией, что Telegram-путь, и синхронно возвращает результат.
//! Telegram здесь не нужен: запись в `outbox` не создаётся.

mod idempotency;
mod rate_limit;

use std::sync::Arc;
use std::time::Duration as StdDuration;

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{FromRequest, FromRequestParts, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode};
use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sqlx::Connection;
use uuid::Uuid;

use idempotency::{ReservationGuard, header_key, request_hash, resolve_idempotency_key};
pub use rate_limit::{RateDecision, RateLimiter};

use crate::auth::tokens::AuthContext;
use crate::capture::breaker::CircuitBreaker;
use crate::capture::draft::{self, Applied, NewCapture, Responder};
use crate::capture::llm::{self, Failure};
use crate::capture::prompt::PROMPT_VERSION;
use crate::capture::schema::{ParseError, ParsedResponse, RawItem};
use crate::capture::validate::{DraftItem, ValidationMode, validate_items_in};
use crate::config::CaptureConfig;
use crate::domain::error::DomainError;
use crate::domain::models::{CaptureMode, CaptureSource, Item, List, ListKind};
use crate::domain::validation::{MAX_TITLE, sanitize};
use crate::home_assistant::{AiTask, MAX_GENERATE_TIMEOUT};
use crate::storage::repositories::capture_requests;
use crate::storage::repositories::capture_requests::Reserved;
use crate::storage::repositories::drafts::Draft;
use crate::storage::repositories::lists;
use crate::storage::repositories::llm_runs::{self, FAILURE, NewRun, SUCCESS};

use super::super::error::ApiError;
use super::super::router::AppState;
use super::{connection, database_error, integration_actor, require_capture};

/// Предельный размер тела запроса.
pub const BODY_LIMIT_BYTES: usize = 16 * 1024;

/// Всё, что нужно маршруту capture из конфигурации и окружения.
#[derive(Clone)]
pub struct CaptureState {
    /// Параметры внешнего capture.
    pub config: CaptureConfig,
    /// Клиент LLM; `None`, если разбор выключен или настроен неверно.
    pub ai: Option<Arc<dyn AiTask>>,
    /// Общий с Telegram предохранитель вызовов LLM.
    pub breaker: Arc<CircuitBreaker>,
    /// Суточная квота разборов на household.
    pub quota: u32,
    /// Лимит частоты на токен.
    pub rate: Arc<RateLimiter>,
}

impl CaptureState {
    /// Создать состояние маршрута.
    pub fn new(
        config: CaptureConfig,
        ai: Option<Arc<dyn AiTask>>,
        breaker: Arc<CircuitBreaker>,
        quota: u32,
    ) -> Self {
        Self {
            config,
            ai,
            breaker,
            quota,
            rate: Arc::new(RateLimiter::new(config.rate_limit_per_min)),
        }
    }
}

/// Авторизация внешнего capture: токен и scope проверяются до разбора тела.
pub struct CaptureAuth(pub AuthContext);

impl FromRequestParts<AppState> for CaptureAuth {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let auth =
            <AuthContext as FromRequestParts<AppState>>::from_request_parts(parts, state).await?;
        require_capture(&auth)?;
        Ok(Self(auth))
    }
}

/// JSON-тело с ошибкой в общем конверте API.
pub struct ApiJson<T>(pub T);

impl<T, S> FromRequest<S> for ApiJson<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        Json::<T>::from_request(request, state)
            .await
            .map(|Json(value)| Self(value))
            .map_err(json_error)
    }
}

fn json_error(rejection: JsonRejection) -> ApiError {
    match rejection.status() {
        StatusCode::PAYLOAD_TOO_LARGE => ApiError::payload_too_large("Request body is too large"),
        StatusCode::UNSUPPORTED_MEDIA_TYPE => {
            ApiError::unsupported_media_type("Content-Type must be application/json")
        }
        _ => ApiError::validation("Invalid request body"),
    }
}

/// Источник внешнего capture из запроса.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestSource {
    /// Голосовой Assist.
    HaVoice,
    /// Автоматизация Home Assistant.
    HaAutomation,
    /// HTTP API.
    Api,
    /// Ярлык на телефоне.
    Shortcut,
}

impl From<RequestSource> for CaptureSource {
    fn from(source: RequestSource) -> Self {
        match source {
            RequestSource::HaVoice => Self::HaVoice,
            RequestSource::HaAutomation => Self::HaAutomation,
            RequestSource::Api => Self::Api,
            RequestSource::Shortcut => Self::Shortcut,
        }
    }
}

/// Подсказка списка по умолчанию.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestHint {
    /// Задачи.
    Tasks,
    /// Покупки.
    Shopping,
    /// Чек-лист.
    Checklist,
}

impl From<RequestHint> for ListKind {
    fn from(hint: RequestHint) -> Self {
        match hint {
            RequestHint::Tasks => Self::Tasks,
            RequestHint::Shopping => Self::Shopping,
            RequestHint::Checklist => Self::Checklist,
        }
    }
}

/// Тело запроса.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureRequest {
    /// Текст для разбора.
    pub text: String,
    /// Источник.
    pub source: RequestSource,
    /// Режим.
    pub mode: CaptureMode,
    /// Подсказка списка по умолчанию.
    #[serde(default)]
    pub hint: Option<RequestHint>,
    /// Конкретный список household для всех элементов.
    #[serde(default)]
    pub list_id: Option<Uuid>,
    /// Ключ повтора; заголовок `Idempotency-Key` имеет приоритет над полем.
    #[serde(default)]
    pub idempotency_key: Option<String>,
}

/// Статус capture в ответе.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Элементы созданы.
    Applied,
    /// Сохранён черновик.
    Draft,
    /// Жёсткая ошибка, ничего не создано.
    Rejected,
}

/// Элемент в ответе.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureItem {
    /// Идентификатор созданного элемента; у черновика — `null`.
    pub id: Option<Uuid>,
    /// Название.
    pub title: String,
    /// Список.
    pub list_id: Option<Uuid>,
    /// Тип списка.
    pub kind: ListKind,
    /// Вседневный срок.
    pub due_date: Option<NaiveDate>,
    /// Срок со временем.
    pub due_at: Option<DateTime<Utc>>,
}

/// Ответ `POST /api/v1/capture`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureResponse {
    /// Идентификатор capture; для черновика совпадает с `draft_id`.
    pub capture_id: Uuid,
    /// Итог.
    pub status: Status,
    /// Создана позиция из исходного текста без LLM.
    pub fallback: bool,
    /// Короткая фраза для озвучивания.
    pub speech: String,
    /// Элементы, созданные этим запросом, либо элементы черновика.
    pub items: Vec<CaptureItem>,
    /// Черновик, если `status=draft`.
    pub draft_id: Option<Uuid>,
    /// Срок жизни черновика.
    pub expires_at: Option<DateTime<Utc>>,
    /// Вопросы модели.
    pub clarifications: Vec<String>,
}

/// Через сколько секунд повторять запрос, пока первый обрабатывается.
const IN_PROGRESS_RETRY_AFTER: u64 = 2;
/// Через сколько секунд предлагать повторить `draft` при исчерпанной квоте.
/// Точный момент смены суток зависит от часового пояса household, поэтому
/// используется консервативный час: раньше квота не восстановится.
const QUOTA_RETRY_AFTER: u64 = 3_600;
/// Минимальный остаток бюджета, при котором повтор разбора имеет смысл:
/// второй вызов с меньшим бюджетом почти наверняка упадёт таймаутом.
const MIN_RETRY_BUDGET: StdDuration = StdDuration::from_secs(1);

/// Обработать внешний capture.
pub async fn capture(
    State(state): State<AppState>,
    auth: CaptureAuth,
    headers: HeaderMap,
    ApiJson(request): ApiJson<CaptureRequest>,
) -> Result<Json<CaptureResponse>, ApiError> {
    let mut guard = ReservationGuard::new(state.pool.clone());
    let result = capture_inner(&state, auth.0, &headers, request, guard.slot()).await;

    // Единственное синхронное место освобождения резерва: любая ветка ошибки
    // после `reserve`, не записавшая ответ, возвращает ключ. `Drop` выше
    // освобождает его, если запрос был отменён. `Reservation::release` не
    // трогает уже завершённый или перехваченный резерв.
    if result.is_err() {
        guard.release().await;
    }
    result
}

async fn capture_inner(
    state: &AppState,
    auth: AuthContext,
    headers: &HeaderMap,
    request: CaptureRequest,
    reservation: &mut Option<capture_requests::Reservation>,
) -> Result<Json<CaptureResponse>, ApiError> {
    let capture = state
        .capture
        .as_ref()
        .ok_or_else(|| ApiError::not_found("Route not found"))?;

    let text = request.text.trim().to_owned();
    let length = text.chars().count();
    if length == 0 || length > usize::try_from(capture.config.max_text_chars).unwrap_or(0) {
        return Err(ApiError::validation(format!(
            "text must be between 1 and {} characters",
            capture.config.max_text_chars
        )));
    }

    let now = Utc::now();
    let source: CaptureSource = request.source.into();
    // Идентификатор попытки для журнала и ответа `rejected`; у применённого
    // capture в ответе и журнале будет идентификатор черновика. При повторе
    // этот же идентификатор закрепляется за резервом ключа.
    let capture_id = Uuid::now_v7();

    // Лимит частоты проверяется до разбора ключа: запрос с невалидным ключом —
    // такой же запрос и тоже расходует лимит.
    let decision = capture.rate.check(auth.token_id, now);
    if !decision.allowed {
        return Err(ApiError::rate_limited("Too many capture requests")
            .with_retry_after(decision.retry_after.unwrap_or(60)));
    }

    let header = header_key(headers)?;
    let idempotency_key = resolve_idempotency_key(header, request.idempotency_key.as_deref())?;
    let request_hash = idempotency_key
        .as_ref()
        .map(|_| request_hash(&text, source, request.mode, request.hint, request.list_id));

    // Соединение нужно только для коротких чтений до вызова LLM: сам вызов
    // занимает до 20 секунд и не должен держать соединение пула.
    let mut conn = connection(state).await?;

    // Ключ резервируется отдельной короткой транзакцией до вызова LLM:
    // параллельный повтор должен увидеть резерв, а не запустить второй разбор.
    // Коммит делает `reserve` перед возвратом. Проверки списка, квоты,
    // предохранителя и доступности LLM идут после резерва, чтобы завершённый
    // повтор получал сохранённый ответ, даже если состояние household с тех пор
    // изменилось.
    if let (Some(key), Some(hash)) = (&idempotency_key, request_hash.as_deref()) {
        match capture_requests::reserve(
            &mut conn,
            auth.household_id,
            source,
            key,
            hash,
            capture_id,
            now,
        )
        .await
        .map_err(database_error)?
        {
            Reserved::Acquired => {
                *reservation = Some(capture_requests::Reservation::new(
                    auth.household_id,
                    source,
                    key,
                    capture_id,
                ));
            }
            Reserved::Completed(response) => {
                let response: CaptureResponse =
                    serde_json::from_value(response).map_err(|error| {
                        tracing::error!(%error, "stored idempotency response is corrupted");
                        ApiError::internal("Database error")
                    })?;
                log_capture(source, "replayed", response.capture_id);
                return Ok(Json(response));
            }
            Reserved::InProgress => {
                return Err(ApiError::conflict(
                    "Capture with this Idempotency-Key is still in progress",
                )
                .with_retry_after(IN_PROGRESS_RETRY_AFTER));
            }
            Reserved::Mismatch => {
                return Err(ApiError::validation(
                    "Idempotency-Key was already used with a different request",
                ));
            }
        }
    }

    // Конкретный список проверяется после резерва: чужой, отсутствующий и
    // архивный список дают один и тот же 422, чтобы не раскрывать чужие данные.
    let target_list = match request.list_id {
        Some(list_id) => match lists::get_visible(&mut conn, auth.household_id, list_id).await {
            Ok(list) => Some(list),
            Err(DomainError::NotFound) => {
                return Err(ApiError::validation("Unknown list"));
            }
            Err(error) => return Err(error.into()),
        },
        None => None,
    };

    let context = draft::load_context_for(&mut conn, auth.household_id, None, now).await?;

    // Один помощник на все ветки fallback: поля запроса и контекст одинаковы,
    // различаются только причина и момент записи.
    let fallback = Fallback {
        household_id: auth.household_id,
        capture_id,
        source,
        idempotency_key: idempotency_key.as_deref(),
        request_hash: request_hash.as_deref(),
        text: &text,
        target_list: target_list.as_ref(),
        hint: request.hint,
        context: &context,
    };

    let fallback_allowed = request.mode == CaptureMode::Auto && capture.config.voice_fallback_raw;

    // Выключенная или не настроенная LLM — это недоступность LLM: `auto` с
    // включённым fallback создаёт позицию из исходного текста, `draft` — 503.
    let Some(ai) = capture.ai.as_deref() else {
        if fallback_allowed {
            return apply_fallback(&mut conn, fallback, now, "llm_disabled", reservation).await;
        }
        log_capture(source, "unavailable", capture_id);
        return Err(ApiError::service_unavailable(
            "Automatic capture is not configured",
        ));
    };

    let used = llm_runs::count_since(&mut conn, auth.household_id, now - Duration::hours(24))
        .await
        .map_err(database_error)?;
    if used >= i64::from(capture.quota) {
        tracing::warn!(
            metric = "capture_quota_exhausted",
            "llm capture quota is exhausted"
        );
        if fallback_allowed {
            return apply_fallback(&mut conn, fallback, now, "quota_exhausted", reservation).await;
        }
        log_capture(source, "unavailable", capture_id);
        if request.mode == CaptureMode::Draft {
            return Err(
                ApiError::rate_limited("Automatic capture quota is exhausted")
                    .with_retry_after(QUOTA_RETRY_AFTER),
            );
        }
        return Err(ApiError::service_unavailable(
            "Automatic capture quota is exhausted",
        ));
    }
    if !capture.breaker.allow(now) {
        tracing::warn!(
            metric = "capture_circuit_open",
            "llm circuit breaker is open"
        );
        if fallback_allowed {
            return apply_fallback(&mut conn, fallback, now, "circuit_open", reservation).await;
        }
        log_capture(source, "unavailable", capture_id);
        return Err(ApiError::service_unavailable(
            "Automatic capture is temporarily unavailable",
        ));
    }

    // Вызов LLM не держит соединение пула.
    drop(conn);

    // Некорректный ответ можно повторить один раз, но только внутри того же
    // общего таймаута: голосовой pipeline не должен ждать больше бюджета
    // источника. Ошибки HA (сеть, таймаут, 401, 5xx) не повторяются.
    let timeout = llm_timeout(&capture.config, source);
    let deadline = tokio::time::Instant::now() + timeout;
    let mut attempt = 0u8;
    let mut runs: Vec<PendingRun> = Vec::new();

    let outcome = loop {
        attempt += 1;
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let attempt_timeout = if attempt == 1 { timeout } else { remaining };
        let result = llm::generate(
            ai,
            capture.breaker.as_ref(),
            &context,
            &text,
            Utc::now(),
            attempt_timeout,
        )
        .await;
        match result {
            Ok(generated) => {
                runs.push(pending_run(
                    auth.household_id,
                    source,
                    request.mode,
                    RunOutcome::Success,
                    None,
                    generated.latency_ms,
                    generated.finished_at,
                ));
                break Ok(generated);
            }
            Err(failed) => {
                let retry = attempt == 1 && failed.failure.is_parse_retryable();
                let time_left = deadline.saturating_duration_since(tokio::time::Instant::now());
                let code = failed.failure.code();
                runs.push(pending_run(
                    auth.household_id,
                    source,
                    request.mode,
                    RunOutcome::Failure,
                    Some(code),
                    failed.latency_ms,
                    failed.finished_at,
                ));
                if retry && time_left >= MIN_RETRY_BUDGET {
                    continue;
                }
                break Err((failed.failure, failed.finished_at));
            }
        }
    };

    let mut conn = connection(state).await?;
    for pending in runs {
        match pending.outcome {
            RunOutcome::Success => {
                llm_runs::record(&mut conn, pending.run, pending.at)
                    .await
                    .map_err(database_error)?;
            }
            RunOutcome::Failure => record_failure(&mut conn, pending.run, pending.at).await,
        }
    }

    let generated = match outcome {
        Ok(generated) => generated,
        Err((failure, last_finished_at)) => {
            match &failure {
                Failure::Ai(error) => {
                    tracing::warn!(error = error.code(), "llm capture request failed");
                }
                Failure::Parse(error) => {
                    tracing::warn!(error = error.code(), "llm capture response rejected");
                }
            }

            if matches!(failure, Failure::Parse(ParseError::TooManyItems { .. })) {
                return reject(
                    &mut conn,
                    reservation,
                    capture_id,
                    source,
                    TOO_MANY_ITEMS_SPEECH.to_owned(),
                    Vec::new(),
                    last_finished_at,
                )
                .await;
            }

            if fallback_allowed {
                return apply_fallback(
                    &mut conn,
                    fallback,
                    last_finished_at,
                    failure.code(),
                    reservation,
                )
                .await;
            }

            log_capture(source, "unavailable", capture_id);
            return Err(ApiError::service_unavailable(
                "Automatic capture is temporarily unavailable",
            ));
        }
    };
    let (mut parsed, finished_at) = (generated.parsed, generated.finished_at);

    apply_target(&mut parsed, target_list.as_ref(), request.hint);

    let item_source = draft::item_source(source);
    let validation = match request.mode {
        CaptureMode::Auto => ValidationMode::Auto,
        CaptureMode::Draft => ValidationMode::Strict,
    };
    let validated = validate_items_in(&context, item_source, parsed.clone(), validation);

    // Пустой разбор отклоняется в обоих режимах, невалидный — только в `auto`:
    // черновик покажет ошибки пользователю.
    if validated.items.is_empty() || (request.mode == CaptureMode::Auto && !validated.is_valid()) {
        return reject(
            &mut conn,
            reservation,
            capture_id,
            source,
            rejected_speech(&validated.items),
            validated.clarifications,
            finished_at,
        )
        .await;
    }

    match request.mode {
        CaptureMode::Auto => {
            let item_kinds: Vec<ListKind> = validated.items.iter().map(|item| item.kind).collect();
            let expected_items = item_kinds.len();
            let actor = integration_actor(item_source);
            let kinds_for_response = item_kinds.clone();
            let clarifications = validated.clarifications.clone();
            let responder: Responder<'_, Applied> = Box::new(move |applied| {
                serde_json::to_value(applied_response(
                    applied,
                    &kinds_for_response,
                    clarifications,
                ))
            });
            let applied = draft::apply_auto(
                &mut conn,
                NewCapture {
                    id: idempotency_key.as_ref().map(|_| capture_id),
                    household_id: auth.household_id,
                    user_id: None,
                    chat_id: None,
                    source,
                    mode: CaptureMode::Auto,
                    idempotency_key,
                    request_hash,
                    raw_text: text,
                    parsed,
                    fallback: false,
                    prompt_version: Some(PROMPT_VERSION.to_owned()),
                },
                &actor,
                &context,
                validated.items,
                finished_at,
                Some(responder),
                reservation.as_mut(),
            )
            .await
            .map_err(store_error)?;
            if applied.items.len() != expected_items {
                // Ответ и элементы уже закоммичены: резерв завершён и не
                // освобождается, иначе повтор создал бы дубликаты.
                tracing::error!("applied capture item count does not match the parse");
                return Err(ApiError::internal("Database error"));
            }
            log_capture(source, "applied", applied.capture_id);
            Ok(Json(applied_response(
                &applied,
                &item_kinds,
                validated.clarifications,
            )))
        }
        CaptureMode::Draft => {
            let items: Vec<CaptureItem> = validated.items.iter().map(draft_item_response).collect();
            let draft_items = items.clone();
            let clarifications = validated.clarifications.clone();
            let responder: Responder<'_, Draft> = Box::new(move |draft| {
                serde_json::to_value(draft_response(draft, draft_items, clarifications))
            });
            // Ответ и черновик пишутся одной транзакцией: падение между ними не
            // должно оставить ключ в состоянии «обрабатывается» после коммита.
            let mut tx = conn.begin().await.map_err(database_error)?;
            let created = draft::create_with_responder(
                &mut tx,
                NewCapture {
                    id: idempotency_key.as_ref().map(|_| capture_id),
                    household_id: auth.household_id,
                    user_id: None,
                    chat_id: None,
                    source,
                    mode: CaptureMode::Draft,
                    idempotency_key,
                    request_hash,
                    raw_text: text,
                    parsed,
                    fallback: false,
                    prompt_version: Some(PROMPT_VERSION.to_owned()),
                },
                finished_at,
                Some(responder),
                reservation.as_mut(),
            )
            .await;
            let draft = match created {
                Ok(draft) => {
                    // Резерв освободит общий обработчик ошибки.
                    tx.commit().await.map_err(database_error)?;
                    if let Some(reservation) = reservation.as_mut() {
                        reservation.settle();
                    }
                    draft
                }
                Err(error) => {
                    let _ = tx.rollback().await;
                    return Err(store_error(error));
                }
            };
            log_capture(source, "draft", draft.id);
            Ok(Json(draft_response(
                &draft,
                items,
                validated.clarifications,
            )))
        }
    }
}

fn applied_response(
    applied: &Applied,
    kinds: &[ListKind],
    clarifications: Vec<String>,
) -> CaptureResponse {
    let items: Vec<CaptureItem> = applied
        .items
        .iter()
        .zip(kinds)
        .map(|(item, kind)| item_response(item, *kind))
        .collect();
    CaptureResponse {
        capture_id: applied.capture_id,
        status: Status::Applied,
        fallback: false,
        speech: fit_speech(applied_speech(&items)),
        items,
        draft_id: None,
        expires_at: None,
        clarifications,
    }
}

fn draft_response(
    draft: &Draft,
    items: Vec<CaptureItem>,
    clarifications: Vec<String>,
) -> CaptureResponse {
    CaptureResponse {
        capture_id: draft.id,
        status: Status::Draft,
        fallback: false,
        speech: fit_speech(draft_speech(&items)),
        items,
        draft_id: Some(draft.id),
        expires_at: Some(draft.expires_at),
        clarifications,
    }
}

/// Таймаут LLM для источника.
///
/// `ha_voice` укладывается в восьмисекундный бюджет Home Assistant; остальные
/// источники получают общий предел вызова. Telegram-путь сюда не попадает: он
/// всегда использует [`MAX_GENERATE_TIMEOUT`].
fn llm_timeout(config: &CaptureConfig, source: CaptureSource) -> StdDuration {
    match source {
        CaptureSource::HaVoice => StdDuration::from_millis(u64::from(config.voice_timeout_ms)),
        _ => MAX_GENERATE_TIMEOUT,
    }
}

/// Итог одного вызова LLM, ожидающий записи в журнал.
struct PendingRun {
    run: NewRun,
    at: DateTime<Utc>,
    outcome: RunOutcome,
}

/// Чем закончился вызов LLM.
#[derive(Debug, Clone, Copy)]
enum RunOutcome {
    Success,
    Failure,
}

/// Собрать запись журнала для одного вызова LLM.
fn pending_run(
    household_id: Uuid,
    source: CaptureSource,
    mode: CaptureMode,
    outcome: RunOutcome,
    error_code: Option<&'static str>,
    latency_ms: i64,
    at: DateTime<Utc>,
) -> PendingRun {
    let status = match outcome {
        RunOutcome::Success => SUCCESS,
        RunOutcome::Failure => FAILURE,
    };
    PendingRun {
        run: NewRun {
            household_id,
            user_id: None,
            source,
            mode,
            prompt_version: Some(PROMPT_VERSION),
            status,
            error_code,
            latency_ms,
            input_tokens: None,
            output_tokens: None,
        },
        at,
        outcome,
    }
}

/// Общие поля fallback-веток одного запроса.
#[derive(Clone, Copy)]
struct Fallback<'a> {
    household_id: Uuid,
    capture_id: Uuid,
    source: CaptureSource,
    idempotency_key: Option<&'a str>,
    request_hash: Option<&'a str>,
    text: &'a str,
    target_list: Option<&'a List>,
    hint: Option<RequestHint>,
    context: &'a crate::capture::context::CaptureContext,
}

/// Создать одну позицию из исходного текста без LLM.
///
/// Список выбирается так же, как для обычного разбора: явный `list_id`,
/// затем `hint` (список по умолчанию своего типа), затем список задач по
/// умолчанию. Срок, исполнитель и теги не задаются. Черновик и ответ
/// коммитятся той же транзакцией через [`draft::apply_auto`].
async fn apply_fallback(
    conn: &mut sqlx::PgConnection,
    input: Fallback<'_>,
    now: DateTime<Utc>,
    reason: &'static str,
    reservation: &mut Option<capture_requests::Reservation>,
) -> Result<Json<CaptureResponse>, ApiError> {
    let requested_kind = input
        .target_list
        .map(|list| list.kind)
        .or_else(|| input.hint.map(ListKind::from))
        .unwrap_or(ListKind::Tasks);
    // Подсказка могла указать тип, для которого в household нет списка
    // (например, чек-лист): тогда используется список задач по умолчанию.
    let kind = if input.context.default_list(requested_kind).is_some() {
        requested_kind
    } else {
        ListKind::Tasks
    };
    let (title, notes) = fallback_text(input.text);
    let parsed = ParsedResponse {
        items: vec![RawItem {
            target_kind: kind,
            target_list: input.target_list.map(|list| list.name.clone()),
            title,
            notes,
            due_date: None,
            due_time: None,
            priority: None,
            assignee: None,
            assignee_id: None,
            tags: Vec::new(),
            quantity: None,
            unit: None,
            subtasks: Vec::new(),
            recurrence: None,
        }],
        clarifications: Vec::new(),
    };
    let item_source = draft::item_source(input.source);
    let validated = validate_items_in(
        input.context,
        item_source,
        parsed.clone(),
        ValidationMode::Auto,
    );
    if !validated.is_valid() {
        // Текст из одних управляющих символов нормализуется в пустое название.
        tracing::warn!("fallback text cannot be stored as is");
        return Err(ApiError::validation("Capture text cannot be stored as is"));
    }

    let actor = integration_actor(item_source);
    let responder: Responder<'_, Applied> =
        Box::new(move |applied| serde_json::to_value(fallback_response(applied, kind)));
    let applied = draft::apply_auto(
        conn,
        NewCapture {
            id: input.idempotency_key.map(|_| input.capture_id),
            household_id: input.household_id,
            user_id: None,
            chat_id: None,
            source: input.source,
            mode: CaptureMode::Auto,
            idempotency_key: input.idempotency_key.map(str::to_owned),
            request_hash: input.request_hash.map(str::to_owned),
            raw_text: input.text.to_owned(),
            parsed,
            fallback: true,
            prompt_version: Some(PROMPT_VERSION.to_owned()),
        },
        &actor,
        input.context,
        validated.items,
        now,
        Some(responder),
        reservation.as_mut(),
    )
    .await
    .map_err(store_error)?;

    if applied.items.len() != 1 {
        // Ответ и элементы уже закоммичены: резерв не освобождается.
        tracing::error!("fallback applied an unexpected number of items");
        return Err(ApiError::internal("Internal error"));
    }

    tracing::warn!(
        metric = "capture_fallback_total",
        reason,
        source = source_name(input.source),
        "capture fell back to the raw text"
    );
    log_capture(input.source, "fallback", applied.capture_id);
    Ok(Json(fallback_response(&applied, kind)))
}

/// Название fallback-позиции и заметка с полным текстом.
///
/// Управляющие символы удаляются, пробелы нормализуются. Длинный текст
/// обрезается по границе слова до [`MAX_TITLE`], а полный текст кладётся в
/// заметку: текст запроса не больше [`crate::domain::validation::MAX_NOTES`].
fn fallback_text(text: &str) -> (String, Option<String>) {
    let full = sanitize(text).trim().to_owned();
    let normalized = full.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() <= MAX_TITLE {
        return (normalized, None);
    }
    let mut chars = normalized.chars();
    let mut title: String = chars.by_ref().take(MAX_TITLE).collect();
    // Если следующий символ — пробел, слово закончилось ровно на пределе:
    // обрезать его не нужно.
    if chars.next().is_some_and(|next| !next.is_whitespace())
        && let Some(position) = title.rfind(char::is_whitespace)
    {
        title.truncate(position);
    }
    (title.trim_end().to_owned(), Some(full))
}

fn fallback_response(applied: &Applied, kind: ListKind) -> CaptureResponse {
    let items: Vec<CaptureItem> = applied
        .items
        .iter()
        .map(|item| item_response(item, kind))
        .collect();
    let speech = match items.first() {
        Some(item) => format!("Записал как есть: {}", item.title),
        None => "Записал как есть.".to_owned(),
    };
    CaptureResponse {
        capture_id: applied.capture_id,
        status: Status::Applied,
        fallback: true,
        speech: fit_speech(speech),
        items,
        draft_id: None,
        expires_at: None,
        clarifications: Vec::new(),
    }
}

/// Сохранить ответ в резерве ключа. Ошибка записи возвращается вызывающей
/// стороне; резерв освобождает общий обработчик ошибки.
async fn store_response(
    conn: &mut sqlx::PgConnection,
    reservation: &mut Option<capture_requests::Reservation>,
    response: &CaptureResponse,
    now: DateTime<Utc>,
) -> Result<(), ApiError> {
    let Some(reservation) = reservation.as_mut() else {
        return Ok(());
    };
    let value = serde_json::to_value(response).map_err(|error| {
        tracing::error!(%error, "capture response could not be serialized");
        ApiError::internal("Internal error")
    })?;
    match reservation.complete(conn, &value, now).await {
        Ok(()) => {
            reservation.settle();
            Ok(())
        }
        Err(capture_requests::CompleteError::Lost) => Err(reservation_lost()),
        Err(capture_requests::CompleteError::Database(error)) => {
            tracing::error!(%error, "could not store the idempotency response");
            Err(database_error(error))
        }
    }
}

/// Ответить `rejected` и сохранить ответ в резерве ключа.
async fn reject(
    conn: &mut sqlx::PgConnection,
    reservation: &mut Option<capture_requests::Reservation>,
    capture_id: Uuid,
    source: CaptureSource,
    speech: String,
    clarifications: Vec<String>,
    now: DateTime<Utc>,
) -> Result<Json<CaptureResponse>, ApiError> {
    let response = rejected_response(capture_id, speech, clarifications);
    store_response(conn, reservation, &response, now).await?;
    log_capture(source, "rejected", capture_id);
    Ok(Json(response))
}

/// Конкретный список или подсказка меняют цель элементов до валидации.
fn apply_target(parsed: &mut ParsedResponse, list: Option<&List>, hint: Option<RequestHint>) {
    if let Some(list) = list {
        for item in &mut parsed.items {
            item.target_kind = list.kind;
            item.target_list = Some(list.name.clone());
        }
        return;
    }
    let Some(hint) = hint else {
        return;
    };
    let kind: ListKind = hint.into();
    // `hint` задаёт тип списка по умолчанию для всего capture: список в рамках
    // типа выбирает уже настоящий `default_list` household.
    for item in &mut parsed.items {
        item.target_kind = kind;
        item.target_list = None;
    }
}

pub(super) fn item_response(item: &Item, kind: ListKind) -> CaptureItem {
    CaptureItem {
        id: Some(item.id),
        title: item.title.clone(),
        list_id: Some(item.list_id),
        kind,
        due_date: item.due_date,
        due_at: item.due_at,
    }
}

fn draft_item_response(item: &DraftItem) -> CaptureItem {
    CaptureItem {
        // Элемент черновика ещё не создан: идентификатора нет.
        id: None,
        title: item.title.clone(),
        list_id: item.list_id,
        kind: item.kind,
        due_date: item.effective_due_date,
        due_at: item.effective_due_at,
    }
}

pub(super) fn applied_speech(items: &[CaptureItem]) -> String {
    let count = items.len();
    match common_kind(items) {
        Some(kind) if count == 1 => {
            let titles = titles(items);
            match kind {
                ListKind::Shopping => format!("Добавил покупку: {titles}."),
                ListKind::Tasks => format!("Добавил задачу: {titles}."),
                ListKind::Checklist => format!("Добавил позицию: {titles}."),
            }
        }
        Some(kind) if count <= 3 => {
            let titles = titles(items);
            format!("Добавил {}: {titles}.", kind_noun(kind, count))
        }
        _ => format!("Добавил {}.", elements(count)),
    }
}

fn draft_speech(items: &[CaptureItem]) -> String {
    let count = items.len();
    match common_kind(items) {
        Some(kind) if count <= 3 => format!("Сохранил черновик: {}.", kind_noun(kind, count)),
        _ => format!("Сохранил черновик: {}.", elements(count)),
    }
}

/// «2 покупки», «3 задачи», «2 позиции» — число с существительным по типу списка.
fn kind_noun(kind: ListKind, count: usize) -> String {
    match kind {
        ListKind::Shopping => crate::telegram::ru::purchases(count as i64),
        ListKind::Tasks => crate::telegram::ru::tasks(count as i64),
        ListKind::Checklist => crate::telegram::ru::positions(count as i64),
    }
}

/// Фраза для случая, когда LLM вернула больше элементов, чем допускает схема.
const TOO_MANY_ITEMS_SPEECH: &str =
    "Слишком много элементов. Разделите текст на части и отправьте их по отдельности.";

fn rejected_response(
    capture_id: Uuid,
    speech: String,
    clarifications: Vec<String>,
) -> CaptureResponse {
    CaptureResponse {
        capture_id,
        status: Status::Rejected,
        fallback: false,
        speech,
        items: Vec::new(),
        draft_id: None,
        expires_at: None,
        clarifications,
    }
}

fn rejected_speech(items: &[DraftItem]) -> String {
    let reason = items
        .iter()
        .find_map(|item| item.errors.first())
        .map(|error| error.message)
        .unwrap_or("не нашёл задач или покупок");
    fit_speech(format!("Не смог добавить: {}.", lowercase_first(reason)))
}

fn common_kind(items: &[CaptureItem]) -> Option<ListKind> {
    let first = items.first()?.kind;
    items.iter().all(|item| item.kind == first).then_some(first)
}

fn titles(items: &[CaptureItem]) -> String {
    items
        .iter()
        .map(|item| item.title.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect::<Vec<_>>()
        .join(", ")
}

pub(super) fn elements(count: usize) -> String {
    format!(
        "{count} {}",
        crate::telegram::ru::plural(count as i64, "элемент", "элемента", "элементов")
    )
}

fn lowercase_first(value: &str) -> String {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) => first.to_lowercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Ограничить фразу 200 символами по границе слова.
pub(super) fn fit_speech(text: String) -> String {
    const LIMIT: usize = 200;
    if text.chars().count() <= LIMIT {
        return text;
    }
    let mut shortened: String = text.chars().take(LIMIT - 1).collect();
    if let Some(position) = shortened.rfind(char::is_whitespace) {
        shortened.truncate(position);
    }
    let shortened = shortened.trim_end_matches([',', '.', ':', ';', ' ']);
    format!("{shortened}…")
}

fn source_name(source: CaptureSource) -> &'static str {
    match source {
        CaptureSource::Telegram => "telegram",
        CaptureSource::HaVoice => "ha_voice",
        CaptureSource::HaAutomation => "ha_automation",
        CaptureSource::Api => "api",
        CaptureSource::Shortcut => "shortcut",
    }
}

/// Одна строка на capture; текст, `speech` и заголовки сюда не попадают.
fn log_capture(source: CaptureSource, status: &str, capture_id: Uuid) {
    tracing::info!(
        source = source_name(source),
        status,
        capture_id = %capture_id,
        "capture finished"
    );
}

async fn record_failure(conn: &mut sqlx::PgConnection, run: NewRun, now: DateTime<Utc>) {
    if let Err(error) = llm_runs::record(conn, run, now).await {
        tracing::error!(%error, "could not record the failed llm run");
    }
}

fn store_error(error: draft::StoreError) -> ApiError {
    match error {
        draft::StoreError::Draft(draft::DraftError::Domain(error)) => error.into(),
        draft::StoreError::Draft(draft::DraftError::Invalid(_)) => {
            tracing::warn!("capture items became invalid before they could be applied");
            ApiError::validation("Capture items are no longer valid")
        }
        draft::StoreError::Serialize(error) => {
            tracing::error!(%error, "capture response could not be serialized");
            ApiError::internal("Internal error")
        }
        draft::StoreError::ReservationLost => reservation_lost(),
        draft::StoreError::Draft(other) => {
            tracing::error!(%other, "capture could not be applied");
            ApiError::internal("Database error")
        }
    }
}

/// Опоздавший запрос потерял резерв: capture не создан, повтор возможен после
/// того, как новый владелец закончит.
fn reservation_lost() -> ApiError {
    ApiError::conflict("Capture with this Idempotency-Key is being processed by another request")
        .with_retry_after(IN_PROGRESS_RETRY_AFTER)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stale_draft_invalid_error_is_a_validation_failure() {
        let error = store_error(draft::StoreError::Draft(draft::DraftError::Invalid(vec![
            0,
        ])));
        assert_eq!(error.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[test]
    fn applied_speech_uses_natural_counts() {
        let item = |kind, title: &str| CaptureItem {
            id: None,
            title: title.to_owned(),
            list_id: None,
            kind,
            due_date: None,
            due_at: None,
        };
        assert_eq!(
            applied_speech(&[item(ListKind::Shopping, "молоко")]),
            "Добавил покупку: молоко."
        );
        assert_eq!(
            applied_speech(&[
                item(ListKind::Shopping, "молоко"),
                item(ListKind::Shopping, "хлеб")
            ]),
            "Добавил 2 покупки: молоко, хлеб."
        );
        assert_eq!(
            applied_speech(
                &(0..5)
                    .map(|n| item(ListKind::Shopping, &format!("покупка {n}")))
                    .collect::<Vec<_>>()
            ),
            "Добавил 5 элементов."
        );
        assert_eq!(
            applied_speech(&[
                item(ListKind::Shopping, "молоко"),
                item(ListKind::Tasks, "позвонить")
            ]),
            "Добавил 2 элемента."
        );
    }

    #[test]
    fn a_word_that_ends_at_the_title_limit_is_not_cut() {
        let prefix = "купить ";
        let word = "а".repeat(MAX_TITLE - prefix.chars().count());
        let text = format!("{prefix}{word} молоко");

        let (title, notes) = fallback_text(&text);

        assert_eq!(title, format!("{prefix}{word}"));
        assert_eq!(title.chars().count(), MAX_TITLE);
        assert_eq!(notes.as_deref(), Some(text.as_str()));
    }

    #[test]
    fn speech_is_cut_at_a_word_boundary_and_stays_plain() {
        let text = format!("Добавил {}: {}", elements(50), "молоко ".repeat(60));
        let speech = fit_speech(text);
        assert!(speech.chars().count() <= 200, "{}", speech.chars().count());
        assert!(speech.ends_with('…'));
        assert!(!speech.ends_with(" …"), "{speech}");
    }
}
