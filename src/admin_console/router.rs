//! Роутер консоли администратора.
//!
//! Консоль живёт на отдельном listener: публичный [`crate::http::router`] не
//! содержит её маршрутов. Все ответы консоли, включая редиректы и ошибки,
//! получают запрет кеширования и встраивания во фреймы одним слоем на роутере.

use std::net::SocketAddr;

use axum::Router;
use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::Response;
use axum::routing::{get, post};
use sqlx::PgPool;

use crate::config::IpNet;
use crate::http::client::{Client, TrustedProxies};

use super::pages;

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
        .layer(axum::middleware::map_response(security_headers))
        .with_state(state)
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
