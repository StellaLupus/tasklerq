//! Роутер консоли администратора.
//!
//! Консоль живёт на отдельном listener: публичный [`crate::http::router`] не
//! содержит её маршрутов.

use std::net::SocketAddr;

use axum::Router;
use axum::http::HeaderMap;
use axum::response::Html;
use axum::routing::get;
use sqlx::PgPool;

use crate::config::IpNet;
use crate::http::client::{Client, TrustedProxies};

use super::session::AdminSession;

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
    /// список не доверяет никому.
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
    Router::new().route("/admin", get(index)).with_state(state)
}

/// Главная страница консоли. До 5.8 отдаётся простой HTML без шаблонов.
async fn index(_session: AdminSession) -> Html<&'static str> {
    Html(
        "<!doctype html><html lang=\"ru\"><head><meta charset=\"utf-8\">\
         <title>Tasklerq</title></head><body><h1>Tasklerq</h1></body></html>",
    )
}
