//! Страницы консоли: setup, вход, восстановление, смена пароля, главная и выход.
//!
//! До 5.8 разметка минимальна: строки HTML без шаблонизатора, общий макет,
//! CSS и htmx появятся в 5.8. Формы — обычные POST без JavaScript; вводимое имя
//! и код экранируются перед вставкой в ответ, пароль обратно не показывается.
//!
//! Сверка `Origin` живёт отдельным слоем роутера
//! ([`super::router::require_trusted_origin`]): сюда запрос с чужим origin не
//! доходит. Лимит частоты, argon2 и аудит — в функциях сценариев
//! [`setup::create_account`], [`login::authenticate`], [`recovery::check`],
//! [`recovery::complete`] и [`change::change`], поэтому обработчики только
//! различают итог и собирают ответ.

use std::net::SocketAddr;

use axum::extract::{ConnectInfo, Form, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use chrono::Utc;
use serde::Deserialize;

use super::change::{self, ChangeError, ChangeOutcome};
use super::login::{self, LoginError, LoginOutcome};
use super::password::{self, Password};
use super::recovery::{self, RecoveryCheck, RecoveryError, RecoveryOutcome};
use super::router::AdminState;
use super::session::{self, AdminSession, LOGIN_PATH};
use super::setup::{self, SetupError, SetupOutcome};
use crate::domain::error::DomainError;
use crate::rate_limit::RateDecision;
use crate::storage::repositories::admin_accounts;

/// Путь страницы первого входа.
const SETUP_PATH: &str = "/admin/setup";
/// Путь главной страницы.
const HOME_PATH: &str = "/admin";
/// Путь страницы смены пароля; `?changed=1` показывает подтверждение.
const PASSWORD_PATH: &str = "/admin/password";
/// Путь формы восстановления; сам код подставляется из адреса.
const RECOVER_PATH: &str = "/admin/recover";

const SETUP_FORM_ERROR: &str = "Код недействителен.";
const LOGIN_FORM_ERROR: &str = "Неверное имя или пароль.";
const EMPTY_USERNAME_ERROR: &str = "Имя не должно быть пустым.";
const WRONG_PASSWORD_ERROR: &str = "Неверный текущий пароль.";
const INVALID_LINK_TITLE: &str = "Ссылка недействительна";

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

/// Поля формы восстановления.
#[derive(Deserialize)]
pub(crate) struct RecoveryForm {
    /// Новый пароль; `Debug` не печатает значение.
    password: Password,
}

/// Поля формы смены пароля.
#[derive(Deserialize)]
pub(crate) struct ChangePasswordForm {
    /// Текущий пароль.
    current_password: Password,
    /// Новый пароль.
    new_password: Password,
}

/// Метка успешной смены пароля в адресе страницы.
#[derive(Deserialize)]
pub(crate) struct PasswordQuery {
    /// Присутствие любого значения означает «пароль изменён».
    changed: Option<String>,
}

/// `GET /admin`: главная страница за сессией.
pub(crate) async fn index(_session: AdminSession) -> Html<&'static str> {
    Html(
        "<!doctype html><html lang=\"ru\"><head><meta charset=\"utf-8\">\
         <title>Tasklerq</title></head><body><h1>Tasklerq</h1>\
         <p><a href=\"/admin/password\">Сменить пароль</a></p>\
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
    // Неверная длина — ошибка формы до лимита, argon2 и кода.
    if let Err(error) = password::validate(form.password.expose()) {
        return Html(setup_html(Some(&error.to_string()))).into_response();
    }

    let client = state.client(peer, &headers);
    match setup::create_account(
        &state,
        &client,
        &form.code,
        username,
        form.password,
        Utc::now(),
    )
    .await
    {
        Ok(SetupOutcome::Created(_)) => match session::start(&state, &client, Utc::now()).await {
            Ok((_, cookie)) => redirect_with_cookie(HOME_PATH, cookie),
            Err(error) => internal(error),
        },
        Ok(SetupOutcome::AlreadyConfigured) => redirect(LOGIN_PATH),
        Ok(SetupOutcome::InvalidCode) => Html(setup_html(Some(SETUP_FORM_ERROR))).into_response(),
        Ok(SetupOutcome::RateLimited(decision)) => too_many_requests(decision),
        Err(SetupError::Database(error)) => internal(error),
        Err(SetupError::Password(error)) => internal(error),
        Err(SetupError::Task(error)) => internal(error),
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
        Err(LoginError::Task(error)) => internal(error),
    }
}

/// `GET /admin/recover/{code}`: форма нового пароля по действующей ссылке.
///
/// Код не расходуется: проверка только читает строку. Недействительная ссылка
/// даёт один ответ без причины.
pub(crate) async fn recover_page(
    State(state): State<AdminState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(code): Path<String>,
) -> Response {
    let client = state.client(peer, &headers);
    match recovery::check(&state, &client, &code, Utc::now()).await {
        Ok(RecoveryCheck::Valid) => Html(recovery_html(None, &code)).into_response(),
        Ok(RecoveryCheck::Invalid) => invalid_link(),
        Ok(RecoveryCheck::RateLimited(decision)) => too_many_requests(decision),
        Err(RecoveryError::Database(error)) => internal(error),
        Err(RecoveryError::Password(error)) => internal(error),
        Err(RecoveryError::Task(error)) => internal(error),
    }
}

/// `POST /admin/recover/{code}`: сохранить новый пароль и погасить ссылку.
pub(crate) async fn recover_submit(
    State(state): State<AdminState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(code): Path<String>,
    Form(form): Form<RecoveryForm>,
) -> Response {
    // Длина проверяется до лимита, argon2 и кода: ошибка формы код не сжигает.
    if let Err(error) = password::validate(form.password.expose()) {
        return Html(recovery_html(Some(&error.to_string()), &code)).into_response();
    }

    let client = state.client(peer, &headers);
    match recovery::complete(&state, &client, &code, form.password, Utc::now()).await {
        Ok(RecoveryOutcome::Changed) => redirect(LOGIN_PATH),
        Ok(RecoveryOutcome::Invalid) => invalid_link(),
        Ok(RecoveryOutcome::RateLimited(decision)) => too_many_requests(decision),
        Err(RecoveryError::Database(error)) => internal(error),
        Err(RecoveryError::Password(error)) => internal(error),
        Err(RecoveryError::Task(error)) => internal(error),
    }
}

/// `GET /admin/password`: форма смены пароля за сессией.
pub(crate) async fn password_page(
    _session: AdminSession,
    Query(query): Query<PasswordQuery>,
) -> Response {
    Html(password_html(None, query.changed.is_some())).into_response()
}

/// `POST /admin/password`: проверить текущий пароль и сменить его.
pub(crate) async fn password_submit(
    State(state): State<AdminState>,
    session: AdminSession,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Form(form): Form<ChangePasswordForm>,
) -> Response {
    // Неверная длина нового пароля — ошибка формы без argon2 и без лимита.
    if let Err(error) = password::validate(form.new_password.expose()) {
        return Html(password_html(Some(&error.to_string()), false)).into_response();
    }

    let client = state.client(peer, &headers);
    match change::change(
        &state,
        session,
        &client,
        form.current_password,
        form.new_password,
        Utc::now(),
    )
    .await
    {
        Ok(ChangeOutcome::Changed) => redirect(&format!("{PASSWORD_PATH}?changed=1")),
        Ok(ChangeOutcome::WrongPassword) => {
            Html(password_html(Some(WRONG_PASSWORD_ERROR), false)).into_response()
        }
        Ok(ChangeOutcome::RateLimited(decision)) => too_many_requests(decision),
        Err(ChangeError::Database(error)) => internal(error),
        Err(ChangeError::Password(error)) => internal(error),
        Err(ChangeError::Task(error)) => internal(error),
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

/// Единый ответ на неизвестную, погашенную или истёкшую ссылку.
///
/// `410 Gone` отличает её от отсутствующей страницы, но не раскрывает причину:
/// клиент не должен различать «нет такого кода», «уже использован» и «истёк».
fn invalid_link() -> Response {
    (
        StatusCode::GONE,
        Html(page(
            INVALID_LINK_TITLE,
            "<h1>Ссылка недействительна</h1>\
             <p>Ссылка восстановления недействительна или истекла. \
             Выпустите новую командой <code>tasklerq admin reset-password</code>.</p>\
             <p><a href=\"/admin/login\">Перейти ко входу</a></p>",
        )),
    )
        .into_response()
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
    let error = error_block(error);
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
    let error = error_block(error);
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

/// Форма нового пароля по ссылке восстановления.
fn recovery_html(error: Option<&str>, code: &str) -> String {
    let error = error_block(error);
    page(
        "Восстановление доступа",
        &format!(
            "<h1>Восстановление доступа</h1>{error}\
             <form method=\"post\" action=\"{RECOVER_PATH}/{code}\">\
             <p><label>Новый пароль <input name=\"password\" type=\"password\" autocomplete=\"new-password\" minlength=\"12\" maxlength=\"1024\" required></label></p>\
             <p><button type=\"submit\">Сохранить пароль</button></p>\
             </form>",
            code = escape(code),
        ),
    )
}

/// Форма смены пароля за сессией.
fn password_html(error: Option<&str>, changed: bool) -> String {
    let error = error_block(error);
    let changed = if changed {
        "<p role=\"status\">Пароль изменён.</p>"
    } else {
        ""
    };
    page(
        "Смена пароля",
        &format!(
            "<h1>Смена пароля</h1>{changed}{error}\
             <form method=\"post\" action=\"{PASSWORD_PATH}\">\
             <p><label>Текущий пароль <input name=\"current_password\" type=\"password\" autocomplete=\"current-password\" required></label></p>\
             <p><label>Новый пароль <input name=\"new_password\" type=\"password\" autocomplete=\"new-password\" minlength=\"12\" maxlength=\"1024\" required></label></p>\
             <p><button type=\"submit\">Сменить пароль</button></p>\
             </form>",
        ),
    )
}

/// Блок сообщения об ошибке формы.
fn error_block(error: Option<&str>) -> String {
    error.map_or_else(String::new, |message| {
        format!("<p role=\"alert\">{}</p>", escape(message))
    })
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
