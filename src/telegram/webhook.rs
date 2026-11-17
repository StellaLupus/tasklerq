//! Приём обновлений: `POST /webhooks/telegram`.
//!
//! Обработчик только проверяет секрет, разбирает тело и ставит обновление в
//! очередь. Всё остальное делает воркер, поэтому ответ `200` не зависит от
//! LLM и Telegram API.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use arc_swap::ArcSwapOption;
use axum::body::{Body, to_bytes};
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode};
use chrono::Utc;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::sync::Notify;

use crate::config::SecretString;
use crate::domain::models::{Actor, ActorType};
use crate::http::client::{Client, TrustedProxies};
use crate::http::error::ApiError;
use crate::http::router::AppState;
use crate::metrics;
use crate::rate_limit::RateLimiter;
use crate::security_audit::{self, LimitKey, RateLimitEvent};

use super::queue;
use super::update::{self, Classified};

/// Заголовок с секретом webhook.
pub const SECRET_HEADER: &str = "x-telegram-bot-api-secret-token";

/// Максимальный размер тела обновления.
pub const MAX_BODY_BYTES: usize = 1024 * 1024;

/// Сколько секунд Telegram должен подождать перед повтором, пока идёт смена
/// поколения.
pub const SWITCH_RETRY_AFTER: u64 = 5;

/// Не больше 120 webhook-запросов в секунду с одного IP.
pub const WEBHOOKS_PER_SECOND: u32 = 120;
/// Окно лимита webhook, секунды.
const WEBHOOK_WINDOW_SECONDS: i64 = 1;

/// Настройки приёма webhook.
#[derive(Clone)]
pub struct Ingress {
    secret_digest: [u8; 32],
    rate: Arc<RateLimiter<IpAddr>>,
    wake: Arc<Notify>,
}

impl Ingress {
    /// Создать приёмник. `wake` будит воркер обновлений после вставки.
    pub fn new(secret: &SecretString, wake: Arc<Notify>) -> Self {
        Self::with_rate_limit(secret, wake, WEBHOOKS_PER_SECOND, WEBHOOK_WINDOW_SECONDS)
    }

    /// Создать приёмник с явным лимитом и окном в секундах.
    ///
    /// Окно вынесено в параметр, чтобы тест мог не зависеть от реального
    /// времени; рабочий режим использует [`Self::new`].
    pub fn with_rate_limit(
        secret: &SecretString,
        wake: Arc<Notify>,
        limit: u32,
        window_seconds: i64,
    ) -> Self {
        Self {
            secret_digest: Sha256::digest(secret.expose().as_bytes()).into(),
            rate: Arc::new(RateLimiter::with_window(limit, window_seconds)),
            wake,
        }
    }

    /// Сравнить секрет за постоянное время: оба значения приводятся к хешу
    /// одинаковой длины, поэтому длина секрета не раскрывается.
    fn accepts(&self, candidate: &str) -> bool {
        let digest: [u8; 32] = Sha256::digest(candidate.as_bytes()).into();
        digest.ct_eq(&self.secret_digest).into()
    }
}

/// Общий слот приёма webhook: текущее поколение и признак его смены.
///
/// Супервизор Telegram кладёт сюда приём поколения и снимает его на время
/// переключения. Слот разделяется между супервизором и [`crate::http::router::AppState`],
/// поэтому обработчик видит смену поколения без пересборки роутера.
#[derive(Clone, Default)]
pub struct IngressSlot {
    ingress: Arc<ArcSwapOption<Ingress>>,
    switching: Arc<AtomicBool>,
    /// Есть живое поколение Telegram; приём webhook есть только у webhook-режима,
    /// поэтому для polling и readiness состояние хранится отдельно.
    active: Arc<AtomicBool>,
}

impl IngressSlot {
    /// Поставить приём текущего поколения; `None` — режим без webhook.
    pub fn store(&self, ingress: Option<Ingress>) {
        self.ingress.store(ingress.map(Arc::new));
    }

    /// Текущий приём; `None` — webhook выключен или идёт переключение.
    pub fn load(&self) -> Option<Arc<Ingress>> {
        self.ingress.load_full()
    }

    /// Отметить наличие живого поколения.
    ///
    /// Признак нужен readiness: лаг очереди проверяется, только пока есть кому
    /// её разбирать. Значение ставит супервизор при запуске и остановке
    /// поколения, а не при чтении настроек.
    pub fn set_active(&self, active: bool) {
        self.active.store(active, Ordering::SeqCst);
    }

    /// Есть живое поколение Telegram.
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }

    /// Начать смену поколения: приём снимается, webhook отвечает `503`.
    pub fn begin_switch(&self) {
        self.switching.store(true, Ordering::SeqCst);
        self.active.store(false, Ordering::SeqCst);
        self.ingress.store(None);
    }

    /// Смена поколения закончена: приём уже в слоте (или его нет по режиму).
    pub fn end_switch(&self) {
        self.switching.store(false, Ordering::SeqCst);
    }

    /// Идёт смена поколения.
    pub fn is_switching(&self) -> bool {
        self.switching.load(Ordering::SeqCst)
    }
}

/// `POST /webhooks/telegram`.
pub async fn receive(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Body,
) -> Result<StatusCode, ApiError> {
    let Some(ingress) = state.telegram.load() else {
        // Поколение меняется: Telegram повторит доставку, а отказ не считается
        // ни ошибкой секрета, ни выключенным режимом.
        if state.telegram.is_switching() {
            return Err(ApiError::service_unavailable("Telegram is switching")
                .with_retry_after(SWITCH_RETRY_AFTER));
        }
        return Err(ApiError::not_found("Route not found"));
    };

    // Проверка лимита — до разбора тела и секрета: всплеск запросов с одного
    // IP не должен доходить до парсинга и базы.
    let ip = Client::resolve(peer, &headers, TrustedProxies::PrivateNetworks).ip;
    let now = Utc::now();
    // IPv6-клиент почти всегда распоряжается целой /64: лимит должен считать
    // префикс, иначе смена младших 64 бит обходит 120/с. Тот же префикс идёт в
    // событие аудита, чтобы ротация адреса внутри /64 не давала новый ключ
    // подавления на каждый запрос.
    let key = crate::rate_limit::ip_key(ip);
    let decision = ingress.rate.check(key, now);
    if !decision.allowed {
        // Метрика считает каждый отказ, включая подавленные в аудите.
        metrics::webhook_rate_limited();
        let retry_after = decision.retry_after.unwrap_or(1);
        let recorded = state
            .audit
            .rate_limited(
                RateLimitEvent {
                    limit: security_audit::WEBHOOK_LIMIT,
                    key: LimitKey::Ip(key),
                    retry_after,
                    household_id: None,
                    actor: Actor::system(ActorType::System),
                },
                now,
            )
            .await;
        // Предупреждение только на записанное событие: иначе флуд сверх
        // подавления засорял бы журнал строкой на каждый отклонённый запрос.
        if recorded {
            tracing::warn!(
                ip = %ip,
                limit = ingress.rate.limit(),
                "webhook requests from one IP are rate limited"
            );
        } else {
            tracing::debug!(
                ip = %ip,
                limit = ingress.rate.limit(),
                "webhook requests from one IP are rate limited"
            );
        }
        return Err(
            ApiError::rate_limited("Too many webhook requests").with_retry_after(retry_after)
        );
    }

    let provided = headers
        .get(SECRET_HEADER)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if !ingress.accepts(provided) {
        let reason = if provided.is_empty() {
            "missing_secret"
        } else {
            "bad_secret"
        };
        state.audit.webhook_auth_failed(reason, ip, now).await;
        return Err(ApiError::unauthorized("Invalid webhook secret"));
    }

    let bytes = to_bytes(body, MAX_BODY_BYTES)
        .await
        .map_err(|_| ApiError::payload_too_large("Update is too large"))?;
    let payload: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            crate::http::error::ErrorCode::ValidationError,
            "Malformed update",
        )
    })?;
    let parsed = update::parse_bytes(&bytes).map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            crate::http::error::ErrorCode::ValidationError,
            "Malformed update",
        )
    })?;

    // Группы, каналы и неподдерживаемые типы подтверждаются без сохранения.
    if matches!(update::classify(&parsed), Classified::Ignore) {
        return Ok(StatusCode::OK);
    }

    let mut conn = state.pool.acquire().await.map_err(|error| {
        tracing::error!(%error, "webhook: database unavailable");
        ApiError::internal("Database error")
    })?;
    // При ошибке записи отвечаем 500: Telegram повторит доставку.
    let inserted = queue::enqueue(&mut conn, i64::from(parsed.id.0), &payload, Utc::now())
        .await
        .map_err(|error| {
            tracing::error!(%error, "webhook: failed to enqueue update");
            ApiError::internal("Database error")
        })?;
    if inserted {
        ingress.wake.notify_one();
    }

    Ok(StatusCode::OK)
}
