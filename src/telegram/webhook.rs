//! Приём обновлений: `POST /webhooks/telegram`.
//!
//! Обработчик только проверяет секрет, разбирает тело и ставит обновление в
//! очередь. Всё остальное делает воркер, поэтому ответ `200` не зависит от
//! LLM и Telegram API.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode};
use chrono::Utc;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::sync::Notify;

use crate::config::SecretString;
use crate::domain::models::{Actor, ActorType};
use crate::http::error::ApiError;
use crate::http::router::AppState;
use crate::rate_limit::RateLimiter;
use crate::security_audit::{self, LimitKey, RateLimitEvent};

use super::queue;
use super::update::{self, Classified};

/// Заголовок с секретом webhook.
pub const SECRET_HEADER: &str = "x-telegram-bot-api-secret-token";

/// Максимальный размер тела обновления.
pub const MAX_BODY_BYTES: usize = 1024 * 1024;

/// Не больше 120 webhook-запросов в секунду с одного IP.
pub const WEBHOOKS_PER_SECOND: u32 = 120;
/// Окно лимита webhook, секунды.
const WEBHOOK_WINDOW_SECONDS: i64 = 1;
/// Заголовок, который дописывает доверенный reverse proxy.
const FORWARDED_FOR_HEADER: &str = "x-forwarded-for";

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

/// `POST /webhooks/telegram`.
pub async fn receive(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Body,
) -> Result<StatusCode, ApiError> {
    let Some(ingress) = &state.telegram else {
        return Err(ApiError::not_found("Route not found"));
    };

    // Проверка лимита — до разбора тела и секрета: всплеск запросов с одного
    // IP не должен доходить до парсинга и базы.
    let ip = client_ip(peer, forwarded_for(&headers).as_deref());
    let now = Utc::now();
    let decision = ingress.rate.check(ip, now);
    if !decision.allowed {
        tracing::warn!(
            ip = %ip,
            limit = ingress.rate.limit(),
            "webhook requests from one IP are rate limited"
        );
        let retry_after = decision.retry_after.unwrap_or(1);
        state
            .audit
            .rate_limited(
                RateLimitEvent {
                    limit: security_audit::WEBHOOK_LIMIT,
                    key: LimitKey::Ip(ip),
                    retry_after,
                    household_id: None,
                    actor: Actor::system(ActorType::System),
                },
                now,
            )
            .await;
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

/// Последнее значение `X-Forwarded-For` из всех заголовков запроса.
fn forwarded_for(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(FORWARDED_FOR_HEADER)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .rfind(|value| !value.is_empty())
        .map(str::to_owned)
}

/// IP клиента по адресу пира и `X-Forwarded-For`.
///
/// Заголовок принимается только от доверенного прокси: loopback или приватной
/// сети. В Docker reverse proxy на хосте подключается к контейнеру через
/// bridge-шлюз (`172.x.0.1`), поэтому одного loopback недостаточно. Для
/// остальных пиров заголовок игнорируется: клиент мог подделать его сам.
fn client_ip(peer: SocketAddr, forwarded: Option<&str>) -> IpAddr {
    if is_trusted_proxy(peer.ip())
        && let Some(forwarded) = forwarded
        && let Ok(ip) = forwarded.trim().parse::<IpAddr>()
    {
        return ip;
    }
    peer.ip()
}

/// Адрес доверенного reverse proxy: loopback, RFC1918 или IPv6 ULA.
fn is_trusted_proxy(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => ip.is_loopback() || ip.is_private(),
        IpAddr::V6(ip) => ip.is_loopback() || ip.is_unique_local(),
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    use axum::http::{HeaderMap, HeaderValue};

    use super::{client_ip, forwarded_for};

    fn peer(octets: [u8; 4]) -> SocketAddr {
        SocketAddr::from((octets, 4_040))
    }

    #[test]
    fn the_last_forwarded_address_is_used() {
        let mut headers = HeaderMap::new();
        headers.append(
            "x-forwarded-for",
            HeaderValue::from_static("203.0.113.1, 198.51.100.7"),
        );
        assert_eq!(forwarded_for(&headers).as_deref(), Some("198.51.100.7"));
    }

    #[test]
    fn a_trusted_peer_uses_the_forwarded_address() {
        let ip: IpAddr = "198.51.100.7".parse().expect("ip");
        assert_eq!(client_ip(peer([127, 0, 0, 1]), Some("198.51.100.7")), ip);
        // Docker bridge: reverse proxy на хосте виден как шлюз 172.x.0.1.
        assert_eq!(client_ip(peer([172, 18, 0, 1]), Some("198.51.100.7")), ip);
        assert_eq!(
            client_ip(peer([127, 0, 0, 1]), None),
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        );
        assert_eq!(
            client_ip(peer([127, 0, 0, 1]), Some("not-an-ip")),
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        );
    }

    #[test]
    fn a_public_peer_ignores_the_forwarded_header() {
        assert_eq!(
            client_ip(peer([203, 0, 113, 9]), Some("198.51.100.7")),
            IpAddr::V4(Ipv4Addr::new(203, 0, 113, 9))
        );
    }
}
