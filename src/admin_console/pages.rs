//! Страницы консоли: setup, вход, главная и выход.
//!
//! До 5.8 разметка минимальна: строки HTML без шаблонизатора, общий макет,
//! CSS и htmx появятся в 5.8. Формы — обычные POST без JavaScript; вводимое имя
//! экранируется перед вставкой в ответ, пароль и код обратно не показываются.
//!
//! Проверка `Origin` (5.5), ограничение частоты и аудит (5.6) живут в
//! вызываемых отсюда функциях `login::verify_credentials` и
//! `setup::create_account`, поэтому обработчики не придётся переписывать.

use std::net::SocketAddr;

use axum::extract::{ConnectInfo, Form, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use chrono::Utc;
use serde::Deserialize;

use super::login::{self, LoginError, LoginOutcome};
use super::password::{self, Password};
use super::router::AdminState;
use super::session::{self, AdminSession, LOGIN_PATH};
use super::setup::{self, SetupError};
use crate::domain::error::DomainError;
use crate::rate_limit::{RateDecision, Reservation, ip_key};
use crate::security_audit;
use crate::storage::repositories::admin_accounts;

/// Путь страницы первого входа.
const SETUP_PATH: &str = "/admin/setup";
/// Путь главной страницы.
const HOME_PATH: &str = "/admin";

const SETUP_FORM_ERROR: &str = "Код недействителен.";
const LOGIN_FORM_ERROR: &str = "Неверное имя или пароль.";
const EMPTY_USERNAME_ERROR: &str = "Имя не должно быть пустым.";

/// Поля формы setup.
#[derive(Deserialize)]
pub(crate) struct SetupForm {
    /// Setup-код из лога; пробелы по краям допускаются.
    code: String,
    /// Имя администратора.
    username: String,
    /// Пароль; `Debug` не печатает значение.
    password: Password,
}

/// Поля формы входа.
#[derive(Deserialize)]
pub(crate) struct LoginForm {
    /// Имя администратора.
    username: String,
    /// Пароль; `Debug` не печатает значение.
    password: Password,
}

/// `GET /admin`: главная страница за сессией.
pub(crate) async fn index(_session: AdminSession) -> Html<&'static str> {
    Html(
        "<!doctype html><html lang=\"ru\"><head><meta charset=\"utf-8\">\
         <title>Tasklerq</title></head><body><h1>Tasklerq</h1>\
         <form method=\"post\" action=\"/admin/logout\">\
         <button type=\"submit\">Выйти</button></form></body></html>",
    )
}

/// `GET /admin/setup`: форма первого входа, пока учётной записи нет.
pub(crate) async fn setup_page(State(state): State<AdminState>) -> Response {
    match account_exists(&state).await {
        Ok(false) => Html(setup_html(None)).into_response(),
        Ok(true) => redirect(LOGIN_PATH),
        Err(error) => internal(error),
    }
}

/// `POST /admin/setup`: проверить ввод, погасить код и создать учётку.
pub(crate) async fn setup_submit(
    State(state): State<AdminState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Form(form): Form<SetupForm>,
) -> Response {
    match account_exists(&state).await {
        Ok(false) => {}
        Ok(true) => return redirect(LOGIN_PATH),
        Err(error) => return internal(error),
    }

    let username = form.username.trim();
    if username.is_empty() {
        return Html(setup_html(Some(EMPTY_USERNAME_ERROR))).into_response();
    }
    if let Err(error) = password::validate(form.password.expose()) {
        return Html(setup_html(Some(&error.to_string()))).into_response();
    }

    // Лимит проверяется до argon2 и до кода: неверная длина или пустое имя
    // выше попыткой кода не считаются, а расход хеширования ограничен. Слот
    // резервируется, поэтому параллельный всплеск не проходит лимит целиком.
    let client = state.client(peer, &headers);
    let now = Utc::now();
    let key = ip_key(client.ip);
    let guard = match state.setup_limiter().reserve(&key, now) {
        Reservation::Allowed(guard) => guard,
        Reservation::Denied(decision) => {
            state
                .audit_rate_limited(
                    security_audit::ADMIN_SETUP_CODE_LIMIT,
                    client.ip,
                    decision,
                    now,
                )
                .await;
            return too_many_requests(decision);
        }
    };

    // Длина проверена до хеширования: неверный ввод код не расходует.
    // Хеш считается до транзакции, чтобы не держать блокировку строки кода на
    // время argon2; неверный код поэтому стоит одного хеша, а частоту
    // setup-попыток ограничивает бакет выше.
    let password_hash = match password::hash(form.password).await {
        Ok(hash) => hash,
        Err(error) => return internal(error),
    };

    match setup::create_account(&state, &form.code, username, &password_hash, now).await {
        Ok(account) => {
            guard.succeeded();
            state
                .audit
                .admin_account_created(client.ip, &account.username, Utc::now())
                .await;
            match session::start(&state, &client, Utc::now()).await {
                Ok((_, cookie)) => redirect_with_cookie(HOME_PATH, cookie),
                Err(error) => internal(error),
            }
        }
        // Учётка появилась параллельно: код откатился и остался действующим.
        Err(SetupError::AlreadyConfigured) => {
            guard.succeeded();
            redirect(LOGIN_PATH)
        }
        Err(SetupError::InvalidCode) => {
            guard.failed();
            state
                .audit
                .admin_setup_code_failed(client.ip, "invalid_code", Utc::now())
                .await;
            Html(setup_html(Some(SETUP_FORM_ERROR))).into_response()
        }
        Err(SetupError::Database(error)) => internal(error),
    }
}

/// `GET /admin/login`: форма входа или перенаправления.
pub(crate) async fn login_page(State(state): State<AdminState>, headers: HeaderMap) -> Response {
    match account_exists(&state).await {
        Ok(true) => {}
        Ok(false) => return redirect(SETUP_PATH),
        Err(error) => return internal(error),
    }

    match session::lookup(&state, &headers, Utc::now()).await {
        Ok(Some(_)) => redirect(HOME_PATH),
        Ok(None) => Html(login_html(None)).into_response(),
        Err(error) => internal(error),
    }
}

/// `POST /admin/login`: проверить имя и пароль и выдать сессию.
pub(crate) async fn login_submit(
    State(state): State<AdminState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Form(form): Form<LoginForm>,
) -> Response {
    match account_exists(&state).await {
        Ok(true) => {}
        Ok(false) => return redirect(SETUP_PATH),
        Err(error) => return internal(error),
    }

    let client = state.client(peer, &headers);
    match login::authenticate(&state, &client, &form.username, form.password, Utc::now()).await {
        Ok(LoginOutcome::Accepted(_)) => match session::start(&state, &client, Utc::now()).await {
            Ok((_, cookie)) => redirect_with_cookie(HOME_PATH, cookie),
            Err(error) => internal(error),
        },
        // Неверное имя и неверный пароль дают один и тот же ответ.
        Ok(LoginOutcome::Rejected) => Html(login_html(Some(LOGIN_FORM_ERROR))).into_response(),
        // Пароль не проверялся: лимит исчерпан записанными неудачами.
        Ok(LoginOutcome::RateLimited(decision)) => too_many_requests(decision),
        Err(LoginError::Database(error)) => internal(error),
        Err(LoginError::Password(error)) => internal(error),
    }
}

/// `POST /admin/logout`: удалить сессию на сервере и сбросить cookie.
///
/// Сессия необязательна: с протухшей или неизвестной cookie серверу удалять
/// нечего, но браузер не должен продолжать хранить мёртвую cookie, поэтому
/// `Set-Cookie` с `Max-Age=0` отдаётся в любом случае.
pub(crate) async fn logout(
    State(state): State<AdminState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    session: Option<AdminSession>,
) -> Response {
    if let Some(session) = session
        && let Err(error) = session::end(&state, session.id).await
    {
        return internal(error);
    }

    redirect_with_cookie(
        LOGIN_PATH,
        session::clear_cookie(&state.client(peer, &headers)),
    )
}

/// Есть ли учётная запись администратора.
async fn account_exists(state: &AdminState) -> Result<bool, DomainError> {
    let mut conn = state.pool.acquire().await?;
    Ok(admin_accounts::get(&mut conn).await?.is_some())
}

/// `303` без cookie.
fn redirect(path: &str) -> Response {
    Redirect::to(path).into_response()
}

/// `429` без проверки ввода: `Retry-After` из решения лимита.
///
/// Заголовки безопасности добавляет слой роутера, как и к остальным ответам.
fn too_many_requests(decision: RateDecision) -> Response {
    let mut response = (StatusCode::TOO_MANY_REQUESTS, "Too Many Requests").into_response();
    if let Some(retry_after) = decision.retry_after
        && let Ok(value) = header::HeaderValue::from_str(&retry_after.to_string())
    {
        response.headers_mut().insert(header::RETRY_AFTER, value);
    }
    response
}

/// `303` с одной `Set-Cookie`.
fn redirect_with_cookie(path: &str, cookie: String) -> Response {
    let mut response = Redirect::to(path).into_response();
    match header::HeaderValue::from_str(&cookie) {
        Ok(value) => {
            response.headers_mut().append(header::SET_COOKIE, value);
            response
        }
        // Cookie собирается из base64url и констант, поэтому сюда попасть
        // нельзя; отказ лучше ошибки без заголовка.
        Err(error) => internal(error),
    }
}

/// `500` без подробностей: причина уходит в журнал вызывающей стороны.
fn internal(error: impl std::fmt::Display) -> Response {
    tracing::error!(error = %error, "admin console request failed");
    (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response()
}

/// Общий каркас страницы до макета 5.8.
fn page(title: &str, body: &str) -> String {
    format!(
        "<!doctype html><html lang=\"ru\"><head><meta charset=\"utf-8\">\
         <title>{title}</title></head><body>{body}</body></html>",
        title = escape(title),
    )
}

/// Форма первого входа. Код обратно не показывается.
fn setup_html(error: Option<&str>) -> String {
    let error = error.map_or_else(String::new, |message| {
        format!("<p role=\"alert\">{}</p>", escape(message))
    });
    page(
        "Первый вход",
        &format!(
            "<h1>Первый вход</h1>{error}\
             <form method=\"post\" action=\"/admin/setup\">\
             <p><label>Код <input name=\"code\" type=\"text\" autocomplete=\"one-time-code\" required></label></p>\
             <p><label>Имя <input name=\"username\" type=\"text\" autocomplete=\"username\" required></label></p>\
             <p><label>Пароль <input name=\"password\" type=\"password\" autocomplete=\"new-password\" minlength=\"12\" maxlength=\"1024\" required></label></p>\
             <p><button type=\"submit\">Создать учётную запись</button></p>\
             </form>",
        ),
    )
}

/// Форма входа.
fn login_html(error: Option<&str>) -> String {
    let error = error.map_or_else(String::new, |message| {
        format!("<p role=\"alert\">{}</p>", escape(message))
    });
    page(
        "Вход",
        &format!(
            "<h1>Вход</h1>{error}\
             <form method=\"post\" action=\"/admin/login\">\
             <p><label>Имя <input name=\"username\" type=\"text\" autocomplete=\"username\" required></label></p>\
             <p><label>Пароль <input name=\"password\" type=\"password\" autocomplete=\"current-password\" required></label></p>\
             <p><button type=\"submit\">Войти</button></p>\
             </form>",
        ),
    )
}

/// Экранирование текста для HTML.
fn escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::escape;

    #[test]
    fn user_text_is_escaped() {
        assert_eq!(
            escape("<b>\"Имя\" & 'x'</b>"),
            "&lt;b&gt;&quot;Имя&quot; &amp; &#39;x&#39;&lt;/b&gt;"
        );
    }
}
