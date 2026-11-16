//! Роутер консоли администратора.
//!
//! Консоль живёт на отдельном listener: публичный [`crate::http::router`] не
//! содержит её маршрутов. Все ответы консоли, включая редиректы и ошибки,
//! получают запрет кеширования и встраивания во фреймы одним слоем на роутере.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::Router;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, header};
use axum::middleware::Next;
use axum::response::Response;
use axum::routing::{get, post};
use chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::config::IpNet;
use crate::domain::models::{Actor, ActorType};
use crate::http::client::{Client, TrustedProxies};
use crate::rate_limit::{RateDecision, RateLimiter};
use crate::security_audit::{Audit, LimitKey, RateLimitEvent};

use super::{origin, pages};

/// Сколько неудач допускается в окне лимита консоли.
pub const FAILURE_LIMIT: u32 = 5;
/// Окно лимита консоли, секунды.
pub const FAILURE_WINDOW_SECONDS: i64 = 900;

/// Состояние консоли: база, доверенные прокси, журнал и лимиты.
#[derive(Clone)]
pub struct AdminState {
    /// Пул соединений PostgreSQL.
    pub pool: PgPool,
    /// Сети, от которых принимаются `X-Forwarded-*`.
    trusted_proxies: Vec<IpNet>,
    /// Тот же журнал событий, что у публичного роутера: порог отказов
    /// аутентификации общий.
    pub audit: Audit,
    /// Бакеты неудач по IP клиента. Ключ нормализуется
    /// [`ip_key`](crate::rate_limit::ip_key): IPv6 сводится к /64, как у лимита
    /// webhook, чтобы смена младших 64 бит не обходила лимит. Журнал при этом
    /// хранит полный адрес клиента. `Arc`, чтобы клоны состояния делили
    /// счётчики: новые лимитеры при копировании не создаются.
    login_limiter: Arc<RateLimiter<IpAddr>>,
    setup_limiter: Arc<RateLimiter<IpAddr>>,
    current_password_limiter: Arc<RateLimiter<IpAddr>>,
    recovery_limiter: Arc<RateLimiter<IpAddr>>,
}

impl AdminState {
    /// Создать состояние консоли.
    ///
    /// Журнал передаётся снаружи: неудачные входы в консоль должны попадать в
    /// общий порог отказов аутентификации публичного роутера.
    pub fn new(pool: PgPool, trusted_proxies: Vec<IpNet>, audit: Audit) -> Self {
        Self {
            pool,
            trusted_proxies,
            audit,
            login_limiter: failure_limiter(),
            setup_limiter: failure_limiter(),
            current_password_limiter: failure_limiter(),
            recovery_limiter: failure_limiter(),
        }
    }

    /// Клиент по адресу соединения и заголовкам запроса.
    ///
    /// Доверие задаётся только явным списком `ADMIN_TRUSTED_PROXIES`; пустой
    /// список не доверяет никому. Схема клиента решает флаг `Secure` у cookie.
    pub fn client(&self, peer: SocketAddr, headers: &HeaderMap) -> Client {
        Client::resolve(
            peer,
            headers,
            TrustedProxies::Explicit(&self.trusted_proxies),
        )
    }

    /// Бакет неудачных входов.
    pub fn login_limiter(&self) -> &RateLimiter<IpAddr> {
        &self.login_limiter
    }

    /// Бакет неудачных setup-кодов.
    pub fn setup_limiter(&self) -> &RateLimiter<IpAddr> {
        &self.setup_limiter
    }

    /// Бакет неверного текущего пароля (подключается в 5.7).
    pub fn current_password_limiter(&self) -> &RateLimiter<IpAddr> {
        &self.current_password_limiter
    }

    /// Бакет восстановления доступа (подключается в 5.7).
    pub fn recovery_limiter(&self) -> &RateLimiter<IpAddr> {
        &self.recovery_limiter
    }

    /// Записать в журнал превышение лимита консоли.
    ///
    /// Ключ — IP клиента без секретов. Повторы подавляет сам [`Audit`].
    pub async fn audit_rate_limited(
        &self,
        limit: &'static str,
        ip: IpAddr,
        decision: RateDecision,
        now: DateTime<Utc>,
    ) {
        self.audit
            .rate_limited(
                RateLimitEvent {
                    limit,
                    key: LimitKey::Ip(ip),
                    retry_after: decision.retry_after.unwrap_or(0),
                    household_id: None,
                    actor: Actor::system(ActorType::System),
                },
                now,
            )
            .await;
    }
}

/// Бакет неудач консоли: 5 за 15 минут на клиента.
fn failure_limiter() -> Arc<RateLimiter<IpAddr>> {
    Arc::new(RateLimiter::with_window(
        FAILURE_LIMIT,
        FAILURE_WINDOW_SECONDS,
    ))
}

/// Собрать роутер консоли.
///
/// Сверка `Origin` стоит отдельным слоем, а не вызовом в каждом обработчике:
/// новые формы получают защиту автоматически. `security_headers` добавлен
/// последним, поэтому покрывает и `403` слоя сверки.
pub fn build_router(state: AdminState) -> Router {
    Router::new()
        .route("/admin", get(pages::index))
        .route(
            "/admin/setup",
            get(pages::setup_page).post(pages::setup_submit),
        )
        .route(
            "/admin/login",
            get(pages::login_page).post(pages::login_submit),
        )
        .route("/admin/logout", post(pages::logout))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_trusted_origin,
        ))
        .layer(axum::middleware::map_response(security_headers))
        .with_state(state)
}

/// Отклонить не-GET-запрос без `Origin`/`Referer` своего же origin.
///
/// Проверка стоит до обработчика: ни сессия, ни код не используются, ни одна
/// строка не пишется. Причина уходит в журнал без значений cookie и заголовков.
async fn require_trusted_origin(
    State(state): State<AdminState>,
    request: Request,
    next: Next,
) -> Response {
    let method = request.method();
    if method == Method::GET || method == Method::HEAD {
        return next.run(request).await;
    }

    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(peer)| *peer);
    let allowed = peer.is_some_and(|peer| {
        let client = state.client(peer, request.headers());
        origin::allows(&client, request.headers())
    });

    if !allowed {
        tracing::info!(
            method = %request.method(),
            path = %request.uri().path(),
            "admin console request rejected: origin does not match"
        );
        return origin::forbidden();
    }

    next.run(request).await
}

/// Заголовки всех ответов консоли.
///
/// `no-store` не даёт прокси и браузеру сохранить страницу с формой или
/// cookie; запрет фреймов закрывает clickjacking. Один слой на роутере
/// покрывает и HTML, и `303`, и ошибки обработчиков.
async fn security_headers(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("frame-ancestors 'none'"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    response
}
