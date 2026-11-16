//! Роутер консоли администратора.
//!
//! Консоль живёт на отдельном listener: публичный [`crate::http::router`] не
//! содержит её маршрутов. Все ответы консоли, включая редиректы и ошибки,
//! получают запрет кеширования и встраивания во фреймы одним слоем на роутере.

use std::net::SocketAddr;

use axum::Router;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, header};
use axum::middleware::Next;
use axum::response::Response;
use axum::routing::{get, post};
use sqlx::PgPool;

use crate::config::IpNet;
use crate::http::client::{Client, TrustedProxies};

use super::{origin, pages};

/// Состояние консоли: база и доверенные прокси.
#[derive(Clone)]
pub struct AdminState {
    /// Пул соединений PostgreSQL.
    pub pool: PgPool,
    /// Сети, от которых принимаются `X-Forwarded-*`.
    trusted_proxies: Vec<IpNet>,
}

impl AdminState {
    /// Создать состояние консоли.
    pub fn new(pool: PgPool, trusted_proxies: Vec<IpNet>) -> Self {
        Self {
            pool,
            trusted_proxies,
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
