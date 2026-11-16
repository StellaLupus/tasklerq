//! Страницы консоли: setup, вход, восстановление, смена пароля, мастер и главная.
//!
//! Макет (`Layout`) общий: шапка, навигация вошедшего администратора, сообщение
//! и место для содержимого. Содержимое рисует небольшой askama-шаблон своей
//! страницы, а [`Page`] хранит данные страницы (`Recovery { code }`,
//! `Password { changed }`, шаг мастера) и решает, показывать ли навигацию.
//! Новая страница добавляется вариантом [`Page`] и своим шаблоном, без правки
//! чужих страниц.
//!
//! Шаблоны заданы строками в этом файле: askama 0.16 ищет файловые шаблоны
//! только в `<корень>/templates` (черты `config` и `code-in-doc` выключены), а
//! каталог вне `src/` не попадает в образ сборки — Dockerfile копирует только
//! `src/`. Значения пользователя подставляет askama и экранирует их сам; `|safe`
//! стоит только на уже отрисованном фрагменте askama, не на данных страницы.
//!
//! Формы — обычные POST без JavaScript; htmx подключён макетом для будущего
//! хода проверки слота, но ни одна форма этого пункта от него не зависит.
//!
//! Сверка `Origin` живёт отдельным слоем роутера
//! ([`super::router::require_trusted_origin`]): сюда запрос с чужим origin не
//! доходит. Лимит частоты, argon2 и аудит — в функциях сценариев
//! [`setup::create_account`], [`login::authenticate`], [`recovery::check`],
//! [`recovery::complete`] и [`change::change`], поэтому обработчики только
//! различают итог и собирают ответ.

use std::net::SocketAddr;

use askama::Template;
use axum::extract::{ConnectInfo, Form, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use chrono::Utc;
use serde::Deserialize;

use super::assets;
use super::change::{self, ChangeError, ChangeOutcome};
use super::login::{self, LoginError, LoginOutcome};
use super::password::{self, Password};
use super::recovery::{self, RecoveryCheck, RecoveryError, RecoveryOutcome};
use super::router::AdminState;
use super::session::{self, AdminSession, LOGIN_PATH};
use super::setup::{self, SetupError, SetupOutcome};
use super::wizard::{self, WizardStep};
use crate::domain::error::DomainError;
use crate::rate_limit::RateDecision;
use crate::storage::repositories::admin_accounts;

/// Путь страницы первого входа.
const SETUP_PATH: &str = "/admin/setup";
/// Путь главной страницы.
const HOME_PATH: &str = "/admin";
/// Путь страницы смены пароля; `?changed=1` показывает подтверждение.
const PASSWORD_PATH: &str = "/admin/password";

const SETUP_FORM_ERROR: &str = "Код недействителен.";
const LOGIN_FORM_ERROR: &str = "Неверное имя или пароль.";
const EMPTY_USERNAME_ERROR: &str = "Имя не должно быть пустым.";
const WRONG_PASSWORD_ERROR: &str = "Неверный текущий пароль.";
const WIZARD_SKIP_ERROR: &str = "Этот шаг нельзя пропустить.";
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

/// Поля формы пропуска шага мастера.
#[derive(Deserialize)]
pub(crate) struct SkipForm {
    /// Значение шага: `household`, `llm` или `token`.
    step: String,
}

/// `GET /admin`: главная страница за сессией.
pub(crate) async fn index(State(state): State<AdminState>, _session: AdminSession) -> Response {
    match wizard::read_state(&state.pool, state.wizard(), Utc::now()).await {
        Ok(snapshot) => render(ConsolePage::new(Page::Home {
            setup_incomplete: wizard::wizard_step(&snapshot) != WizardStep::Done,
        })),
        Err(error) => internal(error),
    }
}

/// `GET /admin/setup`: форма первого входа, пока учётной записи нет.
pub(crate) async fn setup_page(State(state): State<AdminState>) -> Response {
    match account_exists(&state).await {
        Ok(false) => render(ConsolePage::new(Page::Setup)),
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
        return render(ConsolePage::new(Page::Setup).error(EMPTY_USERNAME_ERROR));
    }
    // Неверная длина — ошибка формы до лимита, argon2 и кода.
    if let Err(error) = password::validate(form.password.expose()) {
        let message = error.to_string();
        return render(ConsolePage::new(Page::Setup).error(&message));
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
        Ok(SetupOutcome::Created(_)) => match wizard_target(&state).await {
            Ok(path) => match session::start(&state, &client, Utc::now()).await {
                Ok((_, cookie)) => redirect_with_cookie(path, cookie),
                Err(error) => internal(error),
            },
            Err(error) => internal(error),
        },
        Ok(SetupOutcome::AlreadyConfigured) => redirect(LOGIN_PATH),
        Ok(SetupOutcome::InvalidCode) => {
            render(ConsolePage::new(Page::Setup).error(SETUP_FORM_ERROR))
        }
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
        Ok(None) => render(ConsolePage::new(Page::Login)),
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
        Ok(LoginOutcome::Accepted(_)) => match wizard_target(&state).await {
            Ok(path) => match session::start(&state, &client, Utc::now()).await {
                Ok((_, cookie)) => redirect_with_cookie(path, cookie),
                Err(error) => internal(error),
            },
            Err(error) => internal(error),
        },
        // Неверное имя и неверный пароль дают один и тот же ответ.
        Ok(LoginOutcome::Rejected) => render(ConsolePage::new(Page::Login).error(LOGIN_FORM_ERROR)),
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
        Ok(RecoveryCheck::Valid) => render(ConsolePage::new(Page::Recovery {
            code: encode_path_segment(&code),
        })),
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
        let message = error.to_string();
        return render(
            ConsolePage::new(Page::Recovery {
                code: encode_path_segment(&code),
            })
            .error(&message),
        );
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
    render(ConsolePage::new(Page::Password {
        changed: query.changed.is_some(),
    }))
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
        let message = error.to_string();
        return render(ConsolePage::new(Page::Password { changed: false }).error(&message));
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
            render(ConsolePage::new(Page::Password { changed: false }).error(WRONG_PASSWORD_ERROR))
        }
        // Пароль уже изменён параллельным восстановлением, сессия удалена:
        // отправляем войти заново.
        Ok(ChangeOutcome::AlreadyChanged) => redirect(LOGIN_PATH),
        Ok(ChangeOutcome::RateLimited(decision)) => too_many_requests(decision),
        Err(ChangeError::Database(error)) => internal(error),
        Err(ChangeError::Password(error)) => internal(error),
        Err(ChangeError::Task(error)) => internal(error),
    }
}

/// `GET /admin/wizard`: текущий шаг мастера за сессией.
pub(crate) async fn wizard_page(
    State(state): State<AdminState>,
    _session: AdminSession,
) -> Response {
    match wizard::read_state(&state.pool, state.wizard(), Utc::now()).await {
        Ok(snapshot) => {
            let step = wizard::wizard_step(&snapshot);
            match step {
                WizardStep::Done => redirect(HOME_PATH),
                WizardStep::Setup => redirect(SETUP_PATH),
                WizardStep::Household | WizardStep::Llm | WizardStep::Token => {
                    render(ConsolePage::new(Page::Wizard {
                        step,
                        household_skip_allowed: state.wizard().telegram_enabled,
                    }))
                }
            }
        }
        Err(error) => internal(error),
    }
}

/// `POST /admin/wizard/skip`: пропустить текущий шаг и перейти к следующему.
///
/// Пропуск пишется, только если шаг сейчас действительно текущий и допустим в
/// режиме: household — лишь при включённом Telegram. Иначе ответ `400` без
/// записи ключа.
pub(crate) async fn wizard_skip(
    State(state): State<AdminState>,
    _session: AdminSession,
    Form(form): Form<SkipForm>,
) -> Response {
    let now = Utc::now();
    let snapshot = match wizard::read_state(&state.pool, state.wizard(), now).await {
        Ok(snapshot) => snapshot,
        Err(error) => return internal(error),
    };
    let current = wizard::wizard_step(&snapshot);
    let telegram_enabled = state.wizard().telegram_enabled;
    let Some(step) = WizardStep::from_slug(&form.step)
        .filter(|step| *step == current && step.can_skip(telegram_enabled))
    else {
        return wizard_rejection(current, telegram_enabled);
    };
    let key = match wizard::skip(&state.pool, step).await {
        Ok(key) => key,
        Err(error) => return internal(error),
    };
    state.audit.admin_setting_changed(key, "true", now).await;

    match wizard::read_state(&state.pool, state.wizard(), now).await {
        Ok(snapshot) => redirect(wizard::wizard_step(&snapshot).target_path()),
        Err(error) => internal(error),
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

/// Куда вести после входа или setup: текущий шаг мастера или главная.
async fn wizard_target(state: &AdminState) -> Result<&'static str, DomainError> {
    let snapshot = wizard::read_state(&state.pool, state.wizard(), Utc::now()).await?;
    Ok(wizard::wizard_step(&snapshot).target_path())
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
    let mut response = render(ConsolePage::new(Page::InvalidLink));
    // Сбой отрисовки уже `500`: подменять его на `410` нельзя, иначе ошибка
    // шаблона выглядит как недействительная ссылка.
    if response.status() == StatusCode::OK {
        *response.status_mut() = StatusCode::GONE;
    }
    response
}

/// Отказ пропуска: повтор текущего шага с сообщением и статусом `400`.
///
/// Если пропускать уже нечего, повторный POST уводит на главную (или на setup),
/// а не показывает сырой текст ошибки.
fn wizard_rejection(current: WizardStep, telegram_enabled: bool) -> Response {
    match current {
        WizardStep::Household | WizardStep::Llm | WizardStep::Token => render_with_status(
            ConsolePage::new(Page::Wizard {
                step: current,
                household_skip_allowed: telegram_enabled,
            })
            .error(WIZARD_SKIP_ERROR),
            StatusCode::BAD_REQUEST,
        ),
        WizardStep::Setup => redirect(SETUP_PATH),
        WizardStep::Done => redirect(HOME_PATH),
    }
}

/// Percent-encode значение как один сегмент пути.
///
/// Код восстановления приходит из адреса и подставляется в `action` формы.
/// `?`, `#` и другие разделители без кодирования ломают адрес, поэтому
/// оставляем только unreserved-символы RFC 3986; base64url кода не меняется.
fn encode_path_segment(raw: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";

    let mut encoded = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
    }
    encoded
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

/// Отрисовать страницу; сбой отрисовки — `500` без подробностей.
fn render(page: ConsolePage<'_>) -> Response {
    render_with_status(page, StatusCode::OK)
}

/// Отрисовать страницу с явным статусом; сбой отрисовки — `500`.
fn render_with_status(page: ConsolePage<'_>, status: StatusCode) -> Response {
    match page.render() {
        Ok(body) => (status, Html(body)).into_response(),
        Err(error) => internal(error),
    }
}

/// Данные страницы; тип страницы задаёт и заголовок, и навигацию.
enum Page {
    /// Главная за сессией.
    Home {
        /// Мастер ещё не завершён: показать ссылку «Продолжить настройку».
        setup_incomplete: bool,
    },
    /// Первый вход по setup-коду.
    Setup,
    /// Вход.
    Login,
    /// Новый пароль по ссылке восстановления.
    Recovery {
        /// Код восстановления, уже percent-encoded для адреса формы.
        code: String,
    },
    /// Смена пароля за сессией.
    Password {
        /// Пароль только что изменён.
        changed: bool,
    },
    /// Недействительная ссылка восстановления.
    InvalidLink,
    /// Текущий шаг мастера.
    Wizard {
        /// Шаг, который показывается.
        step: WizardStep,
        /// Telegram включён: household можно пропустить.
        household_skip_allowed: bool,
    },
}

impl Page {
    /// Заголовок страницы.
    fn title(&self) -> &'static str {
        match self {
            Self::Home { .. } => "Tasklerq",
            Self::Setup => "Первый вход",
            Self::Login => "Вход",
            Self::Recovery { .. } => "Восстановление доступа",
            Self::Password { .. } => "Смена пароля",
            Self::InvalidLink => INVALID_LINK_TITLE,
            Self::Wizard { step, .. } => step.title(),
        }
    }

    /// Навигация вошедшего администратора.
    ///
    /// Определяется типом страницы: страницы за сессией показывают её всегда,
    /// страницы входа и восстановления — никогда.
    fn nav(&self) -> bool {
        matches!(
            self,
            Self::Home { .. } | Self::Password { .. } | Self::Wizard { .. }
        )
    }

    /// Отрисовать содержимое страницы без макета.
    fn render_body(&self) -> Result<String, askama::Error> {
        match self {
            Self::Home { setup_incomplete } => HomeBody {
                setup_incomplete: *setup_incomplete,
            }
            .render(),
            Self::Setup => SetupBody.render(),
            Self::Login => LoginBody.render(),
            Self::Recovery { code } => RecoveryBody { code }.render(),
            Self::Password { changed } => PasswordBody { changed: *changed }.render(),
            Self::InvalidLink => InvalidLinkBody.render(),
            Self::Wizard {
                step,
                household_skip_allowed,
            } => WizardBody {
                title: step.title(),
                description: step.description(),
                skip: if step.can_skip(*household_skip_allowed) {
                    step.slug()
                } else {
                    None
                },
            }
            .render(),
        }
    }
}

/// Страница с общим макетом.
struct ConsolePage<'a> {
    /// Данные и тип страницы.
    page: Page,
    /// Сообщение формы или причина отказа.
    error: Option<&'a str>,
}

impl<'a> ConsolePage<'a> {
    /// Страница с общим макетом и подключёнными ресурсами.
    fn new(page: Page) -> Self {
        Self { page, error: None }
    }

    /// Показать сообщение формы.
    fn error(mut self, message: &'a str) -> Self {
        self.error = Some(message);
        self
    }

    /// Отрисовать содержимое страницы и вставить его в макет.
    fn render(&self) -> Result<String, askama::Error> {
        Layout {
            css: assets::console_css_path(),
            htmx: assets::htmx_path(),
            title: self.page.title(),
            nav: self.page.nav(),
            error: self.error,
            content: self.page.render_body()?,
        }
        .render()
    }
}

/// Общий макет консоли.
#[derive(Template)]
#[template(
    source = r#"<!doctype html>
<html lang="ru">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{{ title }}</title>
<link rel="stylesheet" href="{{ css }}">
<script src="{{ htmx }}" defer></script>
</head>
<body>
<header class="console-header">
<span class="console-brand">Tasklerq</span>
{% if nav %}
<nav class="console-nav">
<a href="/admin">Главная</a>
<a href="/admin/password">Сменить пароль</a>
<form method="post" action="/admin/logout">
<button type="submit">Выйти</button>
</form>
</nav>
{% endif %}
</header>
<main class="console-main">
{% match error %}
{% when Some with (message) %}
<p role="alert" class="console-alert">{{ message }}</p>
{% when None %}
{% endmatch %}
{{ content|safe }}
</main>
</body>
</html>
"#,
    ext = "html"
)]
struct Layout<'a> {
    /// Путь CSS с хешем содержимого.
    css: &'static str,
    /// Путь htmx с хешем содержимого.
    htmx: &'static str,
    /// Заголовок страницы.
    title: &'static str,
    /// Навигация вошедшего администратора.
    nav: bool,
    /// Сообщение формы или причина отказа.
    error: Option<&'a str>,
    /// Уже отрисованный askama фрагмент страницы; экранирован шаблоном
    /// страницы, поэтому в макет вставляется без повторного экранирования.
    content: String,
}

/// Содержимое главной.
#[derive(Template)]
#[template(
    source = r#"<h1>Tasklerq</h1>
<p>Консоль администратора.</p>
{% if setup_incomplete %}
<p class="console-notice">Настройка ещё не завершена. <a href="/admin/wizard">Продолжить настройку</a></p>
{% endif %}
"#,
    ext = "html"
)]
struct HomeBody {
    /// Мастер ещё не завершён.
    setup_incomplete: bool,
}

/// Содержимое первого входа.
#[derive(Template)]
#[template(
    source = r#"<h1>Первый вход</h1>
<form method="post" action="/admin/setup">
<p><label for="setup-code">Код</label>
<input id="setup-code" name="code" type="text" autocomplete="one-time-code" required></p>
<p><label for="setup-username">Имя</label>
<input id="setup-username" name="username" type="text" autocomplete="username" required></p>
<p><label for="setup-password">Пароль</label>
<input id="setup-password" name="password" type="password" autocomplete="new-password" minlength="12" maxlength="1024" required></p>
<p><button type="submit">Создать учётную запись</button></p>
</form>
"#,
    ext = "html"
)]
struct SetupBody;

/// Содержимое входа.
#[derive(Template)]
#[template(
    source = r#"<h1>Вход</h1>
<form method="post" action="/admin/login">
<p><label for="login-username">Имя</label>
<input id="login-username" name="username" type="text" autocomplete="username" required></p>
<p><label for="login-password">Пароль</label>
<input id="login-password" name="password" type="password" autocomplete="current-password" required></p>
<p><button type="submit">Войти</button></p>
</form>
"#,
    ext = "html"
)]
struct LoginBody;

/// Содержимое восстановления доступа.
#[derive(Template)]
#[template(
    source = r#"<h1>Восстановление доступа</h1>
<form method="post" action="/admin/recover/{{ code }}">
<p><label for="recovery-password">Новый пароль</label>
<input id="recovery-password" name="password" type="password" autocomplete="new-password" minlength="12" maxlength="1024" required></p>
<p><button type="submit">Сохранить пароль</button></p>
</form>
"#,
    ext = "html"
)]
struct RecoveryBody<'a> {
    /// Код восстановления в адресе формы.
    code: &'a str,
}

/// Содержимое смены пароля.
#[derive(Template)]
#[template(
    source = r#"<h1>Смена пароля</h1>
{% if changed %}
<p role="status" class="console-notice">Пароль изменён.</p>
{% endif %}
<form method="post" action="/admin/password">
<p><label for="current-password">Текущий пароль</label>
<input id="current-password" name="current_password" type="password" autocomplete="current-password" required></p>
<p><label for="new-password">Новый пароль</label>
<input id="new-password" name="new_password" type="password" autocomplete="new-password" minlength="12" maxlength="1024" required></p>
<p><button type="submit">Сменить пароль</button></p>
</form>
"#,
    ext = "html"
)]
struct PasswordBody {
    /// Пароль только что изменён.
    changed: bool,
}

/// Содержимое недействительной ссылки восстановления.
#[derive(Template)]
#[template(
    source = r#"<h1>Ссылка недействительна</h1>
<p>Ссылка восстановления недействительна или истекла. Выпустите новую командой <code>tasklerq admin reset-password</code>.</p>
<p><a href="/admin/login">Перейти ко входу</a></p>
"#,
    ext = "html"
)]
struct InvalidLinkBody;

/// Содержимое шага мастера.
#[derive(Template)]
#[template(
    source = r#"<h1>{{ title }}</h1>
<p>{{ description }}</p>
<p class="console-notice">Действие на этом шаге появится позже.</p>
{% match skip %}
{% when Some with (step) %}
<form method="post" action="/admin/wizard/skip">
<input type="hidden" name="step" value="{{ step }}">
<button type="submit">Пропустить</button>
</form>
{% when None %}
<p>Этот шаг нельзя пропустить: Telegram выключен, поэтому household и владельца нужно создать командой администратора.</p>
{% endmatch %}
"#,
    ext = "html"
)]
struct WizardBody {
    /// Название шага.
    title: &'static str,
    /// Короткое пояснение шага.
    description: &'static str,
    /// Значение формы пропуска; `None` — пропуск недоступен.
    skip: Option<&'static str>,
}

#[cfg(test)]
mod tests {
    use super::{ConsolePage, Page};
    use crate::admin_console::wizard::WizardStep;

    #[test]
    fn user_text_is_escaped() {
        let page = ConsolePage::new(Page::Setup).error("<script>alert(\"x\")</script>");
        let html = page.render().expect("the page renders");

        assert!(!html.contains("<script>alert"), "{html}");
        assert!(
            html.contains("&#60;script&#62;alert(&#34;x&#34;)&#60;/script&#62;"),
            "{html}"
        );
    }

    #[test]
    fn every_page_has_the_shared_layout() {
        for page in [
            Page::Home {
                setup_incomplete: false,
            },
            Page::Setup,
            Page::Login,
            Page::Recovery {
                code: "code".to_owned(),
            },
            Page::Password { changed: false },
            Page::InvalidLink,
            Page::Wizard {
                step: WizardStep::Llm,
                household_skip_allowed: false,
            },
        ] {
            let html = ConsolePage::new(page).render().expect("the page renders");
            assert!(html.contains("/admin/assets/console-"), "{html}");
            assert!(html.contains("/admin/assets/htmx-"), "{html}");
        }
    }

    #[test]
    fn the_navigation_follows_the_page_kind() {
        let home = ConsolePage::new(Page::Home {
            setup_incomplete: false,
        })
        .render()
        .expect("home renders");
        assert!(home.contains("action=\"/admin/logout\""), "{home}");

        let login = ConsolePage::new(Page::Login)
            .render()
            .expect("login renders");
        assert!(!login.contains("action=\"/admin/logout\""), "{login}");
    }

    #[test]
    fn the_household_skip_button_depends_on_telegram() {
        let without = ConsolePage::new(Page::Wizard {
            step: WizardStep::Household,
            household_skip_allowed: false,
        })
        .render()
        .expect("the page renders");
        assert!(!without.contains("Пропустить"), "{without}");

        let with = ConsolePage::new(Page::Wizard {
            step: WizardStep::Household,
            household_skip_allowed: true,
        })
        .render()
        .expect("the page renders");
        assert!(with.contains("Пропустить"), "{with}");
        assert!(with.contains("value=\"household\""), "{with}");
    }
}
