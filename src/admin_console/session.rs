//! Сессия администратора: единственное место проверки cookie консоли.
//!
//! В 5.1 сессий ещё нет: учётные записи и таблицы появятся в 5.2–5.4, поэтому
//! проверка сейчас всегда отказывает. Обработчики страниц получают
//! [`AdminSession`] и не знают, откуда берётся сессия; 5.4 добавит в экстрактор
//! чтение cookie `tlq_admin` и проверку токена в базе.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Redirect, Response};

use super::router::AdminState;

/// Путь страницы входа.
pub const LOGIN_PATH: &str = "/admin/login";

/// Администратор с действующей сессией.
pub struct AdminSession;

/// Отказ страницы, требующей сессии: перенаправление на страницу входа.
#[derive(Debug)]
pub struct RedirectToLogin;

impl IntoResponse for RedirectToLogin {
    fn into_response(self) -> Response {
        Redirect::to(LOGIN_PATH).into_response()
    }
}

impl FromRequestParts<AdminState> for AdminSession {
    type Rejection = RedirectToLogin;

    async fn from_request_parts(
        _parts: &mut Parts,
        _state: &AdminState,
    ) -> Result<Self, Self::Rejection> {
        // 5.4: прочитать cookie и проверить сессию в базе.
        Err(RedirectToLogin)
    }
}
