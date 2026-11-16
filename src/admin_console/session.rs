//! Сессия администратора: cookie `tlq_admin` и проверка токена в базе.
//!
//! Токен — 32 случайных байта (256 бит) в base64url; в базе лежит только
//! SHA-256 хеш (`auth::tokens::hash_token`). Cookie `HttpOnly`,
//! `SameSite=Strict`, `Path=/`; `Secure` ставится, только если клиент пришёл по
//! HTTPS, в том числе через доверенный прокси (`AdminState::client`).
//!
//! Срок cookie — `Max-Age` на абсолютные 30 дней, а не сессионная cookie:
//! браузер не должен хранить её дольше серверного срока, а серверный срок всё
//! равно считается от входа, поэтому скользящего продления у cookie нет.
//! Неактивность 7 дней проверяется на сервере по `last_seen_at`.
//!
//! [`AdminSession`] — единственное место проверки cookie: обработчики страниц
//! получают готовую сессию и не знают, откуда она берётся. Сбой базы в
//! экстракторе — `500`, а не вход без сессии.

use axum::extract::{FromRequestParts, OptionalFromRequestParts};
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use chrono::{DateTime, Duration, Utc};
use uuid::Uuid;

use crate::auth::secrets::random_base64url;
use crate::auth::tokens::hash_token;
use crate::domain::error::DomainError;
use crate::http::client::{Client, Scheme};
use crate::storage::repositories::admin_sessions;

use super::router::AdminState;

/// Имя cookie сессии.
pub const COOKIE_NAME: &str = "tlq_admin";
/// Путь страницы входа.
pub const LOGIN_PATH: &str = "/admin/login";
/// Сессия без активности истекает через 7 дней.
pub const IDLE_DAYS: i64 = 7;
/// Абсолютный срок сессии — 30 дней после входа.
pub const ABSOLUTE_DAYS: i64 = 30;
/// Длина случайной части токена: 256 бит.
pub const TOKEN_BYTES: usize = 32;

/// Действующая сессия администратора.
#[derive(Debug, Clone, Copy)]
pub struct AdminSession {
    /// Идентификатор строки `admin_sessions`.
    pub id: Uuid,
}

/// Отказ экстрактора: нет сессии или сломалась база.
#[derive(Debug)]
pub enum SessionRejection {
    /// Cookie нет, она неизвестна или сессия истекла.
    Anonymous,
    /// База недоступна: вход без сессии недопустим.
    Internal,
}

impl IntoResponse for SessionRejection {
    fn into_response(self) -> Response {
        match self {
            Self::Anonymous => Redirect::to(LOGIN_PATH).into_response(),
            Self::Internal => {
                (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response()
            }
        }
    }
}

impl FromRequestParts<AdminState> for AdminSession {
    type Rejection = SessionRejection;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AdminState,
    ) -> Result<Self, Self::Rejection> {
        <Self as OptionalFromRequestParts<AdminState>>::from_request_parts(parts, state)
            .await?
            .ok_or(SessionRejection::Anonymous)
    }
}

/// `Option<AdminSession>` для выхода: без действующей сессии cookie всё равно
/// сбрасывается, а сбой базы остаётся ошибкой.
impl OptionalFromRequestParts<AdminState> for AdminSession {
    type Rejection = SessionRejection;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AdminState,
    ) -> Result<Option<Self>, Self::Rejection> {
        match lookup(state, &parts.headers, Utc::now()).await {
            Ok(session) => Ok(session),
            Err(error) => {
                tracing::error!(error = %error, "admin session lookup failed");
                Err(SessionRejection::Internal)
            }
        }
    }
}

/// Найти действующую сессию по cookie.
///
/// Истёкшая строка удаляется, `last_seen_at` продлевается не чаще раза в
/// минуту. `Ok(None)` — cookie нет или сессия недействительна.
pub async fn lookup(
    state: &AdminState,
    headers: &HeaderMap,
    now: DateTime<Utc>,
) -> Result<Option<AdminSession>, DomainError> {
    let Some(token) = cookie_token(headers) else {
        return Ok(None);
    };

    let mut conn = state.pool.acquire().await?;
    let Some(record) = admin_sessions::find_by_hash(&mut conn, &hash_token(&token)).await? else {
        return Ok(None);
    };

    if is_expired(record.created_at, record.last_seen_at, now) {
        admin_sessions::delete(&mut conn, record.id).await?;
        return Ok(None);
    }

    admin_sessions::touch(&mut conn, record.id, now).await?;
    Ok(Some(AdminSession { id: record.id }))
}

/// Начать сессию: записать хеш токена и вернуть cookie.
///
/// Открытый токен возвращается один раз в `Set-Cookie`; в базе остаётся
/// только хеш.
pub async fn start(
    state: &AdminState,
    client: &Client,
    now: DateTime<Utc>,
) -> Result<(AdminSession, String), DomainError> {
    let token = random_base64url(TOKEN_BYTES);
    let mut conn = state.pool.acquire().await?;
    let record = admin_sessions::create(&mut conn, &hash_token(&token), now).await?;

    Ok((
        AdminSession { id: record.id },
        session_cookie(&token, client),
    ))
}

/// Завершить сессию на сервере.
pub async fn end(state: &AdminState, session_id: Uuid) -> Result<(), DomainError> {
    let mut conn = state.pool.acquire().await?;
    admin_sessions::delete(&mut conn, session_id).await
}

/// Cookie, сбрасывающая сессию: `Max-Age=0`.
pub fn clear_cookie(client: &Client) -> String {
    cookie_value(None, client, 0)
}

/// Единственная cookie сессии из заголовка `Cookie`.
///
/// Разбор не падает на мусоре: чужие и повреждённые пары пропускаются.
pub fn cookie_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.split_once('='))
        .find(|(name, _)| name.trim() == COOKIE_NAME)
        .map(|(_, value)| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// Cookie новой сессии на абсолютный срок жизни.
fn session_cookie(token: &str, client: &Client) -> String {
    cookie_value(Some(token), client, ABSOLUTE_DAYS * 24 * 60 * 60)
}

/// Собрать `Set-Cookie` без внешнего крейта: состав полей фиксирован.
fn cookie_value(token: Option<&str>, client: &Client, max_age_seconds: i64) -> String {
    let mut cookie = format!(
        "{COOKIE_NAME}={}; HttpOnly; SameSite=Strict; Path=/; Max-Age={max_age_seconds}",
        token.unwrap_or_default()
    );
    if client.scheme == Scheme::Https {
        cookie.push_str("; Secure");
    }
    cookie
}

/// Истекла ли сессия: 7 дней без активности или 30 дней после входа.
fn is_expired(created_at: DateTime<Utc>, last_seen_at: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    now.signed_duration_since(last_seen_at) > Duration::days(IDLE_DAYS)
        || now.signed_duration_since(created_at) > Duration::days(ABSOLUTE_DAYS)
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderMap, HeaderValue, header};
    use chrono::{Duration, Utc};

    use crate::http::client::{Client, Scheme};

    use super::{COOKIE_NAME, cookie_token, cookie_value, is_expired, session_cookie};

    fn client(scheme: Scheme) -> Client {
        Client {
            ip: "127.0.0.1".parse().expect("ip"),
            scheme,
            host: Some("console.example".into()),
        }
    }

    fn headers(values: &[&str]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for value in values {
            headers.append(
                header::COOKIE,
                HeaderValue::from_str(value).expect("cookie"),
            );
        }
        headers
    }

    #[test]
    fn the_cookie_has_the_required_flags() {
        let cookie = session_cookie("token-value", &client(Scheme::Http));
        assert!(
            cookie.starts_with(&format!("{COOKIE_NAME}=token-value;")),
            "{cookie}"
        );
        assert!(cookie.contains("HttpOnly"), "{cookie}");
        assert!(cookie.contains("SameSite=Strict"), "{cookie}");
        assert!(cookie.contains("Path=/"), "{cookie}");
        assert!(cookie.contains("Max-Age=2592000"), "{cookie}");
        assert!(!cookie.contains("Secure"), "{cookie}");
    }

    #[test]
    fn only_https_adds_secure() {
        let cookie = session_cookie("token-value", &client(Scheme::Https));
        assert!(cookie.ends_with("; Secure"), "{cookie}");
    }

    #[test]
    fn clearing_the_cookie_sets_zero_max_age() {
        let cookie = cookie_value(None, &client(Scheme::Http), 0);
        assert_eq!(
            cookie,
            "tlq_admin=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0"
        );
    }

    #[test]
    fn the_session_cookie_is_found_among_others() {
        let token = cookie_token(&headers(&[
            "theme=dark",
            "tlq_admin=abc; another=1",
            "tlq_admin=second",
        ]));
        assert_eq!(token.as_deref(), Some("abc"));
        assert_eq!(cookie_token(&headers(&["other=1"])), None);
    }

    #[test]
    fn the_idle_and_absolute_limits_are_strict() {
        let now = Utc::now();
        assert!(is_expired(
            now - Duration::days(1),
            now - Duration::days(8),
            now
        ));
        assert!(!is_expired(
            now - Duration::days(1),
            now - Duration::days(7),
            now
        ));
        assert!(is_expired(
            now - Duration::days(31),
            now - Duration::minutes(1),
            now
        ));
        assert!(!is_expired(
            now - Duration::days(30),
            now - Duration::minutes(1),
            now
        ));
    }
}
