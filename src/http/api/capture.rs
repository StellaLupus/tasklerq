//! Внешний capture: `POST /api/v1/capture`.
//!
//! Текст приходит от Home Assistant, ярлыка или интеграции, разбирается тем же
//! промптом и валидацией, что Telegram-путь, и синхронно возвращает результат.
//! Telegram здесь не нужен: запись в `outbox` не создаётся.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{FromRequest, FromRequestParts, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::Connection;
use uuid::Uuid;

use crate::auth::tokens::AuthContext;
use crate::capture::breaker::CircuitBreaker;
use crate::capture::draft::{self, Applied, NewCapture, Responder};
use crate::capture::llm::{self, Failure};
use crate::capture::prompt::PROMPT_VERSION;
use crate::capture::schema::{ParseError, ParsedResponse};
use crate::capture::validate::{DraftItem, ValidationMode, validate_items_in};
use crate::config::CaptureConfig;
use crate::domain::error::DomainError;
use crate::domain::models::{CaptureMode, CaptureSource, Item, List, ListKind};
use crate::home_assistant::AiTask;
use crate::storage::repositories::capture_requests;
use crate::storage::repositories::capture_requests::Reserved;
use crate::storage::repositories::drafts::Draft;
use crate::storage::repositories::lists;
use crate::storage::repositories::llm_runs::{self, FAILURE, NewRun, SUCCESS};

use super::super::error::ApiError;
use super::super::router::AppState;
use super::{connection, integration_actor, require_capture};

/// Предельный размер тела запроса.
pub const BODY_LIMIT_BYTES: usize = 16 * 1024;
/// Окно лимита частоты.
const RATE_WINDOW_SECONDS: i64 = 60;

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

/// Решение лимита частоты.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateDecision {
    /// Запрос можно обрабатывать.
    pub allowed: bool,
    /// Через сколько секунд повторить; `None`, если запрос разрешён.
    pub retry_after: Option<u64>,
}

/// Лимит capture в минуту на токен.
///
/// Состояние живёт в памяти процесса: сервис — один экземпляр, как и
/// [`CircuitBreaker`]. Окно скользящее, поэтому переход через границу минуты не
/// сбрасывает счётчик.
#[derive(Debug)]
pub struct RateLimiter {
    limit: u32,
    hits: Mutex<HashMap<Uuid, VecDeque<DateTime<Utc>>>>,
}

impl RateLimiter {
    /// Создать лимитер; ноль трактуется как один запрос.
    pub fn new(limit: u32) -> Self {
        Self {
            limit: limit.max(1),
            hits: Mutex::new(HashMap::new()),
        }
    }

    /// Хранится ли ключ `key` в карте; только для тестов.
    #[cfg(test)]
    fn tracks(&self, key: Uuid) -> bool {
        self.hits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(&key)
    }

    /// Учесть запрос токена `key` в момент `now`.
    ///
    /// Перед учётом из карты удаляются ключи без отметок внутри окна: токены
    /// отзываются и пересоздаются, и их записи иначе остались бы навсегда.
    pub fn check(&self, key: Uuid, now: DateTime<Utc>) -> RateDecision {
        let cutoff = now - Duration::seconds(RATE_WINDOW_SECONDS);
        let mut hits = self
            .hits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        hits.retain(|_, queue| {
            while queue.front().is_some_and(|at| *at <= cutoff) {
                queue.pop_front();
            }
            !queue.is_empty()
        });
        let queue = hits.entry(key).or_default();
        if queue.len() >= self.limit as usize {
            let wait = queue
                .front()
                .map(|at| *at + Duration::seconds(RATE_WINDOW_SECONDS) - now)
                .unwrap_or_else(|| Duration::seconds(RATE_WINDOW_SECONDS));
            let mut seconds = wait.num_seconds();
            if wait.subsec_nanos() > 0 {
                seconds += 1;
            }
            return RateDecision {
                allowed: false,
                retry_after: Some(u64::try_from(seconds.max(1)).unwrap_or(1)),
            };
        }
        queue.push_back(now);
        RateDecision {
            allowed: true,
            retry_after: None,
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
        match Json::<T>::from_request(request, state).await {
            Ok(Json(value)) => Ok(Self(value)),
            Err(rejection) => Err(json_error(rejection)),
        }
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
    /// Ключ повтора. В 5.11 принимается, но не сохраняется: семантика — 5.13.
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

/// Заголовок с ключом повтора.
const IDEMPOTENCY_HEADER: &str = "idempotency-key";
/// Предельная длина ключа повтора.
const IDEMPOTENCY_KEY_MAX: usize = 128;
/// Через сколько секунд повторять запрос, пока первый обрабатывается.
const IN_PROGRESS_RETRY_AFTER: u64 = 2;

/// Обработать внешний capture.
pub async fn capture(
    State(state): State<AppState>,
    auth: CaptureAuth,
    headers: HeaderMap,
    ApiJson(request): ApiJson<CaptureRequest>,
) -> Result<Json<CaptureResponse>, ApiError> {
    let auth = auth.0;
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

    let idempotency_key = resolve_idempotency_key(
        headers.get(IDEMPOTENCY_HEADER),
        request.idempotency_key.as_deref(),
    )?;
    let request_hash = idempotency_key
        .as_ref()
        .map(|_| request_hash(&text, source, request.mode, request.hint, request.list_id));

    let decision = capture.rate.check(auth.token_id, now);
    if !decision.allowed {
        return Err(ApiError::rate_limited("Too many capture requests")
            .with_retry_after(decision.retry_after.unwrap_or(60)));
    }

    let Some(ai) = capture.ai.as_deref() else {
        log_capture(source, "unavailable", capture_id);
        return Err(ApiError::service_unavailable(
            "Automatic capture is not configured",
        ));
    };

    // Соединение нужно только для коротких чтений до вызова LLM: сам вызов
    // занимает до 20 секунд и не должен держать соединение пула.
    let (target_list, context) = {
        let mut conn = connection(&state).await?;
        // Конкретный список проверяется до LLM: чужой, отсутствующий и
        // архивный список дают один и тот же 422, чтобы не раскрывать чужие
        // данные.
        let target_list = match request.list_id {
            Some(list_id) => {
                match lists::get_visible(&mut conn, auth.household_id, list_id).await {
                    Ok(list) => Some(list),
                    Err(DomainError::NotFound) => {
                        return Err(ApiError::validation("Unknown list"));
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            None => None,
        };

        // Ключ резервируется отдельной короткой транзакцией до вызова LLM:
        // параллельный повтор должен увидеть резерв, а не запустить второй
        // разбор. Коммит делает `reserve` перед возвратом. Проверки квоты и
        // предохранителя идут после резерва, чтобы завершённый повтор получал
        // сохранённый ответ, даже если квота уже исчерпана.
        if let Some(key) = &idempotency_key {
            let hash = request_hash.as_deref().unwrap_or_default();
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
                Reserved::Acquired => {}
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

        let used =
            match llm_runs::count_since(&mut conn, auth.household_id, now - Duration::hours(24))
                .await
            {
                Ok(used) => used,
                Err(error) => {
                    release_key(
                        &mut conn,
                        auth.household_id,
                        source,
                        idempotency_key.as_deref(),
                    )
                    .await;
                    return Err(database_error(error));
                }
            };
        if used >= i64::from(capture.quota) {
            release_key(
                &mut conn,
                auth.household_id,
                source,
                idempotency_key.as_deref(),
            )
            .await;
            tracing::warn!(
                metric = "capture_quota_exhausted",
                "llm capture quota is exhausted"
            );
            log_capture(source, "unavailable", capture_id);
            return Err(ApiError::service_unavailable(
                "Automatic capture quota is exhausted",
            ));
        }
        if !capture.breaker.allow(now) {
            release_key(
                &mut conn,
                auth.household_id,
                source,
                idempotency_key.as_deref(),
            )
            .await;
            tracing::warn!(
                metric = "capture_circuit_open",
                "llm circuit breaker is open"
            );
            log_capture(source, "unavailable", capture_id);
            return Err(ApiError::service_unavailable(
                "Automatic capture is temporarily unavailable",
            ));
        }

        let context = match draft::load_context_for(&mut conn, auth.household_id, None, now).await {
            Ok(context) => context,
            Err(error) => {
                release_key(
                    &mut conn,
                    auth.household_id,
                    source,
                    idempotency_key.as_deref(),
                )
                .await;
                return Err(error.into());
            }
        };

        (target_list, context)
    };

    let outcome = llm::generate(ai, capture.breaker.as_ref(), &context, &text, now).await;
    let mut conn = connection(&state).await?;
    let (mut parsed, latency_ms, finished_at) = match outcome {
        Ok(generated) => (
            generated.parsed,
            generated.latency_ms,
            generated.finished_at,
        ),
        Err(failed) => {
            let code = failed.failure.code();
            match &failed.failure {
                Failure::Ai(error) => {
                    tracing::warn!(error = error.code(), "llm capture request failed");
                }
                Failure::Parse(error) => {
                    tracing::warn!(error = error.code(), "llm capture response rejected");
                }
            }
            let run = NewRun {
                household_id: auth.household_id,
                user_id: None,
                source,
                mode: request.mode,
                prompt_version: Some(PROMPT_VERSION),
                status: FAILURE,
                error_code: Some(code),
                latency_ms: failed.latency_ms,
                input_tokens: None,
                output_tokens: None,
            };
            record_failure(&mut conn, run, failed.finished_at).await;

            if matches!(
                failed.failure,
                Failure::Parse(ParseError::TooManyItems { .. })
            ) {
                let response =
                    rejected_response(capture_id, TOO_MANY_ITEMS_SPEECH.to_owned(), Vec::new());
                store_response(
                    &mut conn,
                    auth.household_id,
                    source,
                    idempotency_key.as_deref(),
                    &response,
                    failed.finished_at,
                )
                .await?;
                log_capture(source, "rejected", capture_id);
                return Ok(Json(response));
            }

            release_key(
                &mut conn,
                auth.household_id,
                source,
                idempotency_key.as_deref(),
            )
            .await;
            log_capture(source, "unavailable", capture_id);
            return Err(ApiError::service_unavailable(
                "Automatic capture is temporarily unavailable",
            ));
        }
    };

    apply_target(&mut parsed, target_list.as_ref(), request.hint);

    let item_source = draft::item_source(source);
    let validation = match request.mode {
        CaptureMode::Auto => ValidationMode::Auto,
        CaptureMode::Draft => ValidationMode::Strict,
    };
    let validated = validate_items_in(&context, item_source, parsed.clone(), validation);

    let run = NewRun {
        household_id: auth.household_id,
        user_id: None,
        source,
        mode: request.mode,
        prompt_version: Some(PROMPT_VERSION),
        status: SUCCESS,
        error_code: None,
        latency_ms,
        input_tokens: None,
        output_tokens: None,
    };
    if let Err(error) = llm_runs::record(&mut conn, run, finished_at).await {
        release_key(
            &mut conn,
            auth.household_id,
            source,
            idempotency_key.as_deref(),
        )
        .await;
        return Err(database_error(error));
    }

    if validated.items.is_empty() {
        let response = rejected_response(
            capture_id,
            rejected_speech(&validated.items),
            validated.clarifications.clone(),
        );
        store_response(
            &mut conn,
            auth.household_id,
            source,
            idempotency_key.as_deref(),
            &response,
            finished_at,
        )
        .await?;
        log_capture(source, "rejected", capture_id);
        return Ok(Json(response));
    }

    match request.mode {
        CaptureMode::Auto => {
            if !validated.is_valid() {
                let response = rejected_response(
                    capture_id,
                    rejected_speech(&validated.items),
                    validated.clarifications.clone(),
                );
                store_response(
                    &mut conn,
                    auth.household_id,
                    source,
                    idempotency_key.as_deref(),
                    &response,
                    finished_at,
                )
                .await?;
                log_capture(source, "rejected", capture_id);
                return Ok(Json(response));
            }
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
                .unwrap_or(Value::Null)
            });
            let applied = match draft::apply_auto(
                &mut conn,
                NewCapture {
                    id: idempotency_key.as_ref().map(|_| capture_id),
                    household_id: auth.household_id,
                    user_id: None,
                    chat_id: None,
                    source,
                    mode: CaptureMode::Auto,
                    idempotency_key: idempotency_key.clone(),
                    request_hash: request_hash.clone(),
                    raw_text: text.clone(),
                    parsed: parsed.clone(),
                    fallback: false,
                    prompt_version: Some(PROMPT_VERSION.to_owned()),
                },
                &actor,
                &context,
                validated.items,
                finished_at,
                Some(responder),
            )
            .await
            {
                Ok(applied) => applied,
                Err(error) => {
                    release_key(
                        &mut conn,
                        auth.household_id,
                        source,
                        idempotency_key.as_deref(),
                    )
                    .await;
                    return Err(draft_error(error));
                }
            };
            if applied.items.len() != expected_items {
                tracing::error!("applied capture item count does not match the parse");
                release_key(
                    &mut conn,
                    auth.household_id,
                    source,
                    idempotency_key.as_deref(),
                )
                .await;
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
                    .unwrap_or(Value::Null)
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
                    idempotency_key: idempotency_key.clone(),
                    request_hash: request_hash.clone(),
                    raw_text: text,
                    parsed,
                    fallback: false,
                    prompt_version: Some(PROMPT_VERSION.to_owned()),
                },
                finished_at,
                Some(responder),
            )
            .await;
            let draft = match created {
                Ok(draft) => {
                    if let Err(error) = tx.commit().await {
                        let mut fresh = connection(&state).await?;
                        release_key(
                            &mut fresh,
                            auth.household_id,
                            source,
                            idempotency_key.as_deref(),
                        )
                        .await;
                        return Err(database_error(error));
                    }
                    draft
                }
                Err(error) => {
                    let _ = tx.rollback().await;
                    release_key(
                        &mut conn,
                        auth.household_id,
                        source,
                        idempotency_key.as_deref(),
                    )
                    .await;
                    return Err(error.into());
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

/// Ключ повтора: заголовок сильнее поля, но различающиеся значения — ошибка.
fn resolve_idempotency_key(
    header: Option<&HeaderValue>,
    field: Option<&str>,
) -> Result<Option<String>, ApiError> {
    let header = match header {
        Some(value) => match value.to_str() {
            Ok(value) => validate_idempotency_key(value)?,
            Err(_) => return Err(invalid_idempotency_key()),
        },
        None => None,
    };
    let field = field.map(validate_idempotency_key).transpose()?.flatten();
    match (header, field) {
        (Some(header), Some(field)) if header != field => Err(ApiError::validation(
            "Idempotency-Key header and idempotency_key field must match",
        )),
        (Some(key), _) | (None, Some(key)) => Ok(Some(key)),
        (None, None) => Ok(None),
    }
}

/// Пустой ключ означает «без ключа»; иначе допустимы 1..128 видимых ASCII.
fn validate_idempotency_key(raw: &str) -> Result<Option<String>, ApiError> {
    if raw.is_empty() {
        return Ok(None);
    }
    if raw.len() > IDEMPOTENCY_KEY_MAX || !raw.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(invalid_idempotency_key());
    }
    Ok(Some(raw.to_owned()))
}

fn invalid_idempotency_key() -> ApiError {
    ApiError::validation("Idempotency-Key must be 1..128 visible ASCII characters without spaces")
}

/// sha256 канонического представления значимых полей. Ключ сюда не входит.
fn request_hash(
    text: &str,
    source: CaptureSource,
    mode: CaptureMode,
    hint: Option<RequestHint>,
    list_id: Option<Uuid>,
) -> String {
    let canonical = json!({
        "text": text,
        "source": source_name(source),
        "mode": mode_name(mode),
        "hint": hint.map(hint_name),
        "list_id": list_id,
    });
    sha256_hex(canonical.to_string().as_bytes())
}

fn sha256_hex(value: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;

    let digest = Sha256::digest(value);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

fn mode_name(mode: CaptureMode) -> &'static str {
    match mode {
        CaptureMode::Auto => "auto",
        CaptureMode::Draft => "draft",
    }
}

fn hint_name(hint: RequestHint) -> &'static str {
    match hint {
        RequestHint::Tasks => "tasks",
        RequestHint::Shopping => "shopping",
        RequestHint::Checklist => "checklist",
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

/// Сохранить ответ в резерве ключа. После ошибки записи резерв освобождается,
/// чтобы повтор не ждал истечения.
async fn store_response(
    conn: &mut sqlx::PgConnection,
    household_id: Uuid,
    source: CaptureSource,
    key: Option<&str>,
    response: &CaptureResponse,
    now: DateTime<Utc>,
) -> Result<(), ApiError> {
    let Some(key) = key else {
        return Ok(());
    };
    let value = serde_json::to_value(response).map_err(|error| {
        tracing::error!(%error, "capture response could not be serialized");
        ApiError::internal("Database error")
    })?;
    if let Err(error) =
        capture_requests::complete(conn, household_id, source, key, &value, now).await
    {
        tracing::error!(%error, "could not store the idempotency response");
        release_key(conn, household_id, source, Some(key)).await;
        return Err(database_error(error));
    }
    Ok(())
}

/// Освободить ключ после ошибки, чтобы повтор обработался заново.
async fn release_key(
    conn: &mut sqlx::PgConnection,
    household_id: Uuid,
    source: CaptureSource,
    key: Option<&str>,
) {
    let Some(key) = key else {
        return;
    };
    if let Err(error) = capture_requests::release(conn, household_id, source, key).await {
        tracing::error!(%error, "could not release the idempotency key");
    }
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

fn item_response(item: &Item, kind: ListKind) -> CaptureItem {
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

fn applied_speech(items: &[CaptureItem]) -> String {
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
            match kind {
                ListKind::Shopping => format!(
                    "Добавил {}: {titles}.",
                    crate::telegram::ru::purchases(count as i64)
                ),
                ListKind::Tasks => format!(
                    "Добавил {}: {titles}.",
                    crate::telegram::ru::tasks(count as i64)
                ),
                ListKind::Checklist => format!(
                    "Добавил {}: {titles}.",
                    crate::telegram::ru::positions(count as i64)
                ),
            }
        }
        _ => format!("Добавил {}.", elements(count)),
    }
}

fn draft_speech(items: &[CaptureItem]) -> String {
    let count = items.len();
    match common_kind(items) {
        Some(kind) if count <= 3 => format!(
            "Сохранил черновик: {}.",
            match kind {
                ListKind::Shopping => crate::telegram::ru::purchases(count as i64),
                ListKind::Tasks => crate::telegram::ru::tasks(count as i64),
                ListKind::Checklist => crate::telegram::ru::positions(count as i64),
            }
        ),
        _ => format!("Сохранил черновик: {}.", elements(count)),
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
        .flat_map(|item| item.errors.first())
        .next()
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

fn elements(count: usize) -> String {
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
fn fit_speech(text: String) -> String {
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

fn draft_error(error: draft::DraftError) -> ApiError {
    match error {
        draft::DraftError::Domain(error) => error.into(),
        draft::DraftError::Invalid(_) => {
            tracing::warn!("capture items became invalid before they could be applied");
            ApiError::validation("Capture items are no longer valid")
        }
        other => {
            tracing::error!(%other, "capture could not be applied");
            ApiError::internal("Database error")
        }
    }
}

fn database_error(error: sqlx::Error) -> ApiError {
    tracing::error!(%error, "database operation failed");
    ApiError::internal("Database error")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(seconds, 0).expect("timestamp")
    }

    #[test]
    fn the_rate_limit_is_per_key_and_slides() {
        let limiter = RateLimiter::new(10);
        let key = Uuid::now_v7();
        let other = Uuid::now_v7();
        let start = at(1_000);

        for offset in 0..10 {
            assert!(
                limiter
                    .check(key, start + Duration::seconds(offset))
                    .allowed
            );
        }
        let denied = limiter.check(key, start + Duration::seconds(10));
        assert!(!denied.allowed);
        assert!(denied.retry_after.unwrap_or(0) >= 50);

        // Другой токен считается отдельно.
        assert!(limiter.check(other, start + Duration::seconds(10)).allowed);

        // Через минуту окно освобождается.
        assert!(limiter.check(key, start + Duration::seconds(61)).allowed);
    }

    #[test]
    fn a_key_outside_the_window_is_removed_from_the_rate_limiter() {
        let limiter = RateLimiter::new(10);
        let stale = Uuid::now_v7();
        let fresh = Uuid::now_v7();

        assert!(limiter.check(stale, at(1_000)).allowed);
        assert!(limiter.check(fresh, at(1_061)).allowed);

        assert!(
            !limiter.tracks(stale),
            "a stale key must not stay in the map"
        );
        assert!(limiter.tracks(fresh));
    }

    #[test]
    fn a_key_still_inside_the_window_keeps_its_hits() {
        let limiter = RateLimiter::new(10);
        let first = Uuid::now_v7();
        let second = Uuid::now_v7();

        assert!(limiter.check(first, at(1_000)).allowed);
        assert!(limiter.check(second, at(1_030)).allowed);

        assert!(limiter.tracks(first));
        assert!(limiter.tracks(second));
    }

    #[test]
    fn a_removed_key_counts_its_limit_from_zero() {
        let limiter = RateLimiter::new(2);
        let stale = Uuid::now_v7();
        let fresh = Uuid::now_v7();
        let start = at(1_000);

        assert!(limiter.check(stale, start).allowed);
        assert!(limiter.check(stale, start + Duration::seconds(1)).allowed);
        assert!(!limiter.check(stale, start + Duration::seconds(2)).allowed);

        assert!(limiter.check(fresh, start + Duration::seconds(61)).allowed);
        assert!(!limiter.tracks(stale));

        assert!(limiter.check(stale, start + Duration::seconds(61)).allowed);
        assert!(limiter.check(stale, start + Duration::seconds(62)).allowed);
        assert!(!limiter.check(stale, start + Duration::seconds(63)).allowed);
    }

    #[test]
    fn a_stale_draft_invalid_error_is_a_validation_failure() {
        let error = draft_error(draft::DraftError::Invalid(vec![0]));
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
    fn speech_is_cut_at_a_word_boundary_and_stays_plain() {
        let text = format!("Добавил {}: {}", elements(50), "молоко ".repeat(60));
        let speech = fit_speech(text);
        assert!(speech.chars().count() <= 200, "{}", speech.chars().count());
        assert!(speech.ends_with('…'));
        assert!(!speech.ends_with(" …"), "{speech}");
    }
}
