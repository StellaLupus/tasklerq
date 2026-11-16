//! Страницы консоли: setup, вход, восстановление, смена пароля, мастер,
//! настройки LLM и главная.
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
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;

use super::assets;
use super::change::{self, ChangeError, ChangeOutcome};
use super::login::{self, LoginError, LoginOutcome};
use super::password::{self, Password};
use super::recovery::{self, RecoveryCheck, RecoveryError, RecoveryOutcome};
use super::router::AdminState;
use super::session::{self, AdminSession, LOGIN_PATH};
use super::setup::{self, SetupError, SetupOutcome};
use super::wizard::{self, WizardStep};
use crate::admin;
use crate::domain::error::DomainError;
use crate::domain::settings::{DEFAULT_TIMEZONE, validate_timezone};
use crate::domain::validation::normalize_text;
use crate::llm::{LlmApplyError, LlmChange};
use crate::rate_limit::RateDecision;
use crate::settings::{ResolvedSetting, SecretState, SettingError, SettingSource};
use crate::storage::repositories::admin_accounts;

/// Путь страницы первого входа.
const SETUP_PATH: &str = "/admin/setup";
/// Путь главной страницы.
const HOME_PATH: &str = "/admin";
/// Путь страницы смены пароля; `?changed=1` показывает подтверждение.
const PASSWORD_PATH: &str = "/admin/password";
/// Путь страницы настроек LLM.
pub(crate) const LLM_PATH: &str = "/admin/llm";
/// Успешное сохранение настроек LLM.
const LLM_SAVED_PATH: &str = "/admin/llm?saved=1";
/// Настройки сохранены, но токен отклонён: нет ключа шифрования.
const LLM_SECRET_REJECTED_PATH: &str = "/admin/llm?saved=1&secret=key_missing";
/// Токен отклонён, других изменений не было.
const LLM_SECRET_ONLY_REJECTED_PATH: &str = "/admin/llm?secret=key_missing";
/// Имя токена Home Assistant по умолчанию.
const DEFAULT_TOKEN_NAME: &str = "Home Assistant";
/// Scopes токена, который выпускает мастер.
const HOME_ASSISTANT_SCOPES: [&str; 3] = ["ha:read", "ha:write", "capture:write"];

const SETUP_FORM_ERROR: &str = "Код недействителен.";
const LOGIN_FORM_ERROR: &str = "Неверное имя или пароль.";
const EMPTY_USERNAME_ERROR: &str = "Имя не должно быть пустым.";
const WRONG_PASSWORD_ERROR: &str = "Неверный текущий пароль.";
const WIZARD_SKIP_ERROR: &str = "Этот шаг нельзя пропустить.";
const WIZARD_STALE_STEP_ERROR: &str = "Этот шаг мастера уже завершён или недоступен.";
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

/// Поля формы шага мастера.
#[derive(Deserialize)]
pub(crate) struct SkipForm {
    /// Значение шага: `household`, `llm` или `token`.
    step: String,
}

/// Поля формы household.
#[derive(Deserialize)]
pub(crate) struct HouseholdForm {
    /// Название household.
    name: String,
    /// Часовой пояс IANA.
    timezone: String,
}

/// Поля формы токена Home Assistant: имя по желанию.
#[derive(Deserialize)]
pub(crate) struct TokenForm {
    /// Название токена; пустое значение заменяется именем по умолчанию.
    #[serde(default)]
    name: Option<String>,
}

/// Поля формы настроек LLM.
///
/// Отсутствие поля означает «не менять»: так неполный POST не стирает
/// настройки. Пустая строка у обычного поля стирает ключ (действует умолчание
/// или слот остаётся неполным и сохранение отклоняется).
#[derive(Deserialize)]
pub(crate) struct LlmForm {
    /// Флажок включения разбора.
    #[serde(default)]
    enabled: Option<String>,
    #[serde(default)]
    primary_url: Option<String>,
    #[serde(default)]
    primary_model: Option<String>,
    #[serde(default)]
    primary_token: Option<String>,
    #[serde(default)]
    primary_token_delete: Option<String>,
    #[serde(default)]
    primary_timeout_ms: Option<String>,
    #[serde(default)]
    primary_extra_json: Option<String>,
    #[serde(default)]
    fallback_url: Option<String>,
    #[serde(default)]
    fallback_model: Option<String>,
    #[serde(default)]
    fallback_token: Option<String>,
    #[serde(default)]
    fallback_token_delete: Option<String>,
    #[serde(default)]
    fallback_timeout_ms: Option<String>,
    #[serde(default)]
    fallback_extra_json: Option<String>,
    #[serde(default)]
    connect_timeout_ms: Option<String>,
    #[serde(default)]
    daily_quota: Option<String>,
}

impl LlmForm {
    /// Поля одного слота; `primary` выбирает основной слот.
    fn slot(&self, primary: bool) -> LlmSlotFields<'_> {
        if primary {
            LlmSlotFields {
                url: self.primary_url.as_deref(),
                model: self.primary_model.as_deref(),
                token: self.primary_token.as_deref(),
                delete_token: self.primary_token_delete.is_some(),
                timeout: self.primary_timeout_ms.as_deref(),
                extra_json: self.primary_extra_json.as_deref(),
            }
        } else {
            LlmSlotFields {
                url: self.fallback_url.as_deref(),
                model: self.fallback_model.as_deref(),
                token: self.fallback_token.as_deref(),
                delete_token: self.fallback_token_delete.is_some(),
                timeout: self.fallback_timeout_ms.as_deref(),
                extra_json: self.fallback_extra_json.as_deref(),
            }
        }
    }
}

/// Поля формы одного слота.
#[derive(Clone, Copy)]
struct LlmSlotFields<'a> {
    url: Option<&'a str>,
    model: Option<&'a str>,
    token: Option<&'a str>,
    delete_token: bool,
    timeout: Option<&'a str>,
    extra_json: Option<&'a str>,
}

/// Метка успешного сохранения в адресе страницы.
#[derive(Deserialize)]
pub(crate) struct LlmQuery {
    #[serde(default)]
    saved: Option<String>,
    #[serde(default)]
    secret: Option<String>,
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
        // Сессия выдаётся до вычисления адреса: сбой снимка мастера не должен
        // оставить созданную учётку без cookie.
        Ok(SetupOutcome::Created(_)) => match session::start(&state, &client, Utc::now()).await {
            Ok((_, cookie)) => redirect_with_cookie(post_auth_target(&state).await, cookie),
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
        // Живая сессия ведёт туда же, куда после входа, а не всегда на главную:
        // незавершённый мастер должен открыться с текущего шага.
        Ok(Some(_)) => redirect(post_auth_target(&state).await),
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
        // Пароль уже проверен: сессия выдаётся даже если снимок мастера не
        // прочитался, иначе успешный вход выглядит как `500` без cookie.
        Ok(LoginOutcome::Accepted(_)) => match session::start(&state, &client, Utc::now()).await {
            Ok((_, cookie)) => redirect_with_cookie(post_auth_target(&state).await, cookie),
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

/// `GET /admin/llm`: настройки разбора за сессией.
pub(crate) async fn llm_page(
    State(state): State<AdminState>,
    _session: AdminSession,
    Query(query): Query<LlmQuery>,
) -> Response {
    match state.llm_settings().read_settings().await {
        Ok(settings) => render(ConsolePage::new(Page::Llm(Box::new(llm_view(
            &settings,
            None,
            query.saved.is_some(),
            query.secret.as_deref() == Some("key_missing"),
        ))))),
        Err(error) => internal(error),
    }
}

/// `POST /admin/llm`: применить изменённые поля одной группой.
///
/// Поля из окружения не попадают в набор изменений, поэтому POST их не
/// меняет. Ошибка валидации возвращает форму с введёнными несекретными
/// значениями; токен в форму не возвращается никогда. Без ключа шифрования
/// токен отклоняется, а остальные поля сохраняются отдельной группой.
pub(crate) async fn llm_submit(
    State(state): State<AdminState>,
    _session: AdminSession,
    Form(form): Form<LlmForm>,
) -> Response {
    let settings = match state.llm_settings().read_settings().await {
        Ok(settings) => settings,
        Err(error) => return internal(error),
    };
    let pending = collect_changes(&form, &settings);
    if pending.is_empty() {
        return redirect(LLM_PATH);
    }

    let now = Utc::now();
    let all: Vec<LlmChange> = pending.iter().map(|change| change.change.clone()).collect();
    let has_secret = pending.iter().any(PendingChange::is_secret);

    if has_secret {
        match state.llm_settings().apply(&all).await {
            Ok(_) => {
                audit_changes(&state, &pending, now).await;
                return redirect(LLM_SAVED_PATH);
            }
            Err(LlmApplyError::Setting(SettingError::SecretKeyMissing { .. })) => {
                let plain: Vec<LlmChange> = pending
                    .iter()
                    .filter(|change: &&PendingChange| !change.is_secret())
                    .map(|change| change.change.clone())
                    .collect();
                if plain.is_empty() {
                    return redirect(LLM_SECRET_ONLY_REJECTED_PATH);
                }
                match state.llm_settings().apply(&plain).await {
                    Ok(_) => {
                        audit_plain_changes(&state, &pending, now).await;
                        return redirect(LLM_SECRET_REJECTED_PATH);
                    }
                    Err(error) => return llm_error_page(&state, &form, &error).await,
                }
            }
            Err(error) => return llm_error_page(&state, &form, &error).await,
        }
    }

    match state.llm_settings().apply(&all).await {
        Ok(_) => {
            audit_changes(&state, &pending, now).await;
            redirect(LLM_SAVED_PATH)
        }
        Err(error) => llm_error_page(&state, &form, &error).await,
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
                WizardStep::Household | WizardStep::Llm | WizardStep::Token => render(
                    ConsolePage::new(wizard_step_page(step, state.wizard().telegram_enabled)),
                ),
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

    match current_step(&state, now).await {
        Ok(step) => redirect(step.target_path()),
        Err(error) => internal(error),
    }
}

/// `POST /admin/wizard/household`: создать household без владельца.
///
/// Шаг выполняется, только пока он текущий: повторный POST после создания
/// household отклоняется и второй дом не появляется. Ошибка валидации
/// возвращает форму с введёнными значениями; значения экранирует askama.
pub(crate) async fn wizard_household(
    State(state): State<AdminState>,
    _session: AdminSession,
    Form(form): Form<HouseholdForm>,
) -> Response {
    let now = Utc::now();
    let telegram_enabled = state.wizard().telegram_enabled;
    let step = match current_step(&state, now).await {
        Ok(step) => step,
        Err(error) => return internal(error),
    };
    if step != WizardStep::Household {
        return wizard_stale(step, telegram_enabled);
    }

    let (name, timezone) = match normalize_text(&form.name, 100, "household name")
        .and_then(|name| validate_timezone(&form.timezone).map(|timezone| (name, timezone)))
    {
        Ok(values) => values,
        Err(error) => {
            return household_form_error(step, telegram_enabled, &form, &error.to_string());
        }
    };

    match admin::create_household(&state.pool, state.wizard().tenancy, &name, &timezone, now).await
    {
        Ok(_) => redirect(wizard::WIZARD_PATH),
        // Household успел появиться параллельно: второй не создаётся, ведём на
        // актуальный шаг.
        Err(DomainError::Conflict { .. }) => match current_step(&state, now).await {
            Ok(step) => redirect(step.target_path()),
            Err(error) => internal(error),
        },
        // Валидация повторилась уже в репозитории: это ошибка формы, а не сбой
        // сервера, и введённые значения возвращаются.
        Err(DomainError::Validation(message)) => {
            household_form_error(step, telegram_enabled, &form, &message)
        }
        Err(error) => internal(error),
    }
}

/// Форма household с сообщением об ошибке валидации.
fn household_form_error(
    step: WizardStep,
    telegram_enabled: bool,
    form: &HouseholdForm,
    message: &str,
) -> Response {
    render(
        ConsolePage::new(Page::Wizard {
            step,
            household_skip_allowed: telegram_enabled,
            form: WizardForm::Household {
                name: form.name.clone(),
                timezone: form.timezone.clone(),
            },
        })
        .error(message),
    )
}

/// `POST /admin/wizard/token`: выпустить токен Home Assistant и показать его
/// один раз.
///
/// Открытое значение живёт только в теле ответа: в базу и аудит попадает хеш и
/// метаданные, cookie и адрес токен не получают. Повторный POST, когда шаг уже
/// завершён, отклоняется и второй токен не выпускает.
pub(crate) async fn wizard_token(
    State(state): State<AdminState>,
    _session: AdminSession,
    Form(form): Form<TokenForm>,
) -> Response {
    let now = Utc::now();
    let telegram_enabled = state.wizard().telegram_enabled;
    let step = match current_step(&state, now).await {
        Ok(step) => step,
        Err(error) => return internal(error),
    };
    if step != WizardStep::Token {
        return wizard_stale(step, telegram_enabled);
    }

    let submitted_name = form.name.unwrap_or_default();
    let chosen_name = submitted_name.trim();
    let chosen_name = if chosen_name.is_empty() {
        DEFAULT_TOKEN_NAME
    } else {
        chosen_name
    };
    let name = match normalize_text(chosen_name, 64, "token name") {
        Ok(name) => name,
        Err(error) => {
            let message = error.to_string();
            return render(
                ConsolePage::new(Page::Wizard {
                    step,
                    household_skip_allowed: telegram_enabled,
                    form: WizardForm::Token {
                        name: submitted_name,
                    },
                })
                .error(&message),
            );
        }
    };

    let scopes: Vec<String> = HOME_ASSISTANT_SCOPES
        .iter()
        .map(|scope| (*scope).to_owned())
        .collect();
    match admin::create_token(
        &state.pool,
        state.wizard().tenancy,
        None,
        &name,
        &scopes,
        None,
        now,
    )
    .await
    {
        Ok(issued) => render(ConsolePage::new(Page::TokenIssued {
            token: issued.token,
        })),
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

/// Текущий шаг мастера по снимку состояния.
async fn current_step(
    state: &AdminState,
    now: chrono::DateTime<Utc>,
) -> Result<WizardStep, DomainError> {
    let snapshot = wizard::read_state(&state.pool, state.wizard(), now).await?;
    Ok(wizard::wizard_step(&snapshot))
}

/// Куда вести после входа или setup: текущий шаг мастера или главная.
async fn wizard_target(state: &AdminState) -> Result<&'static str, DomainError> {
    Ok(current_step(state, Utc::now()).await?.target_path())
}

/// Адрес после состоявшегося входа: сбой снимка мастера ведёт на главную.
///
/// Учётка и сессия к этому моменту уже есть, поэтому ошибка не должна
/// превращаться в `500` без cookie; главная покажет «Продолжить настройку»,
/// когда снимок снова прочитается.
async fn post_auth_target(state: &AdminState) -> &'static str {
    target_or_home(wizard_target(state).await)
}

/// Выбор адреса по результату [`wizard_target`]; ошибка уходит в журнал.
fn target_or_home(target: Result<&'static str, DomainError>) -> &'static str {
    match target {
        Ok(path) => path,
        Err(error) => {
            tracing::warn!(
                error = %error,
                "admin console wizard target failed; leading to the main page"
            );
            HOME_PATH
        }
    }
}

/// Одно изменение формы вместе с именем настройки для аудита.
struct PendingChange {
    /// Изменение для [`LlmSettings::apply`].
    change: LlmChange,
    /// Имя настройки; в аудит попадает без значения.
    key: &'static str,
    /// Признак для аудита: `changed`, `set` или `deleted`.
    audit_value: &'static str,
}

impl PendingChange {
    /// Изменение записывает секрет: значение в аудит не попадает.
    fn is_secret(&self) -> bool {
        matches!(&self.change, LlmChange::SetSecret { .. })
    }
}

/// Набор изменений по полям формы.
///
/// Поле из окружения не попадает в набор: POST не может его изменить.
/// Отсутствующее поле тоже пропускается, пустое обычное значение удаляет ключ.
fn collect_changes(form: &LlmForm, settings: &[ResolvedSetting]) -> Vec<PendingChange> {
    let mut changes = Vec::new();

    if !is_readonly(settings, "llm.enabled") {
        let current = bool_of(settings, "llm.enabled").unwrap_or(false);
        let submitted = form.enabled.is_some();
        if submitted != current {
            changes.push(PendingChange {
                change: LlmChange::set("llm.enabled", Value::Bool(submitted)),
                key: "llm.enabled",
                audit_value: "changed",
            });
        }
    }

    collect_slot_changes(&mut changes, settings, PRIMARY_KEYS, &form.slot(true));
    collect_slot_changes(&mut changes, settings, FALLBACK_KEYS, &form.slot(false));

    push_text_change(
        &mut changes,
        settings,
        "llm.connect_timeout_ms",
        form.connect_timeout_ms.as_deref(),
    );
    push_text_change(
        &mut changes,
        settings,
        "llm.daily_quota",
        form.daily_quota.as_deref(),
    );

    changes
}

/// Ключи строк одного слота для формы.
#[derive(Clone, Copy)]
struct SlotKeys {
    url: &'static str,
    model: &'static str,
    token: &'static str,
    timeout: &'static str,
    extra_json: &'static str,
}

const PRIMARY_KEYS: SlotKeys = SlotKeys {
    url: "llm.primary.url",
    model: "llm.primary.model",
    token: "llm.primary.token",
    timeout: "llm.primary.timeout_ms",
    extra_json: "llm.primary.extra_json",
};

const FALLBACK_KEYS: SlotKeys = SlotKeys {
    url: "llm.fallback.url",
    model: "llm.fallback.model",
    token: "llm.fallback.token",
    timeout: "llm.fallback.timeout_ms",
    extra_json: "llm.fallback.extra_json",
};

fn collect_slot_changes(
    changes: &mut Vec<PendingChange>,
    settings: &[ResolvedSetting],
    keys: SlotKeys,
    form: &LlmSlotFields<'_>,
) {
    push_text_change(changes, settings, keys.url, form.url);
    push_text_change(changes, settings, keys.model, form.model);
    push_text_change(changes, settings, keys.timeout, form.timeout);
    push_text_change(changes, settings, keys.extra_json, form.extra_json);

    if is_readonly(settings, keys.token) {
        return;
    }
    if form.delete_token {
        changes.push(PendingChange {
            change: LlmChange::delete(keys.token),
            key: keys.token,
            audit_value: "deleted",
        });
    } else if let Some(token) = form.token.filter(|token| !token.trim().is_empty()) {
        changes.push(PendingChange {
            change: LlmChange::secret(keys.token, token),
            key: keys.token,
            audit_value: "set",
        });
    }
}

fn push_text_change(
    changes: &mut Vec<PendingChange>,
    settings: &[ResolvedSetting],
    key: &'static str,
    submitted: Option<&str>,
) {
    if is_readonly(settings, key) {
        return;
    }
    let Some(submitted) = submitted else {
        return;
    };
    let submitted = submitted.trim();
    if submitted == text_of(settings, key).trim() {
        return;
    }
    let change = if submitted.is_empty() {
        LlmChange::delete(key)
    } else {
        LlmChange::set(key, Value::String(submitted.to_owned()))
    };
    changes.push(PendingChange {
        change,
        key,
        audit_value: "changed",
    });
}

/// Записать в аудит имена изменённых настроек без значений.
async fn audit_changes(state: &AdminState, changes: &[PendingChange], now: DateTime<Utc>) {
    for change in changes {
        state
            .audit
            .admin_setting_changed(change.key, change.audit_value, now)
            .await;
    }
}

/// Записать в аудит только несекретные изменения: токен отклонён.
async fn audit_plain_changes(state: &AdminState, changes: &[PendingChange], now: DateTime<Utc>) {
    for change in changes.iter().filter(|change| !change.is_secret()) {
        state
            .audit
            .admin_setting_changed(change.key, change.audit_value, now)
            .await;
    }
}

/// Страница с ошибкой применения; неизвестные сбои — `500` без подробностей.
async fn llm_error_page(state: &AdminState, form: &LlmForm, error: &LlmApplyError) -> Response {
    let Some(message) = llm_error_message(error) else {
        return internal(error);
    };
    match state.llm_settings().read_settings().await {
        Ok(settings) => render(
            ConsolePage::new(Page::Llm(Box::new(llm_view(
                &settings,
                Some(form),
                false,
                false,
            ))))
            .error(&message),
        ),
        Err(error) => internal(error),
    }
}

/// Понятное сообщение формы с именем поля; `None` — внутренняя ошибка.
fn llm_error_message(error: &LlmApplyError) -> Option<String> {
    match error {
        LlmApplyError::MissingField { key } => Some(format!(
            "Не хватает поля `{key}`: у слота должны быть и адрес, и модель."
        )),
        LlmApplyError::InvalidField { key } => {
            Some(format!("Поле `{key}` содержит неверное значение."))
        }
        LlmApplyError::Chain { .. } => {
            Some("Не удалось собрать цепочку LLM; ничего не сохранено.".to_owned())
        }
        LlmApplyError::Setting(error) => setting_error_message(error),
        LlmApplyError::Database(_) | LlmApplyError::Task(_) => None,
    }
}

fn setting_error_message(error: &SettingError) -> Option<String> {
    match error {
        SettingError::UnknownKey { key } => Some(format!("Неизвестная настройка `{key}`.")),
        SettingError::PlainValueForSecret { key } => {
            Some(format!("Поле `{key}` можно сохранить только как секрет."))
        }
        SettingError::SecretValueForPlain { key } => {
            Some(format!("Поле `{key}` не является секретом."))
        }
        SettingError::InvalidValue { key, reason } => {
            Some(format!("Неверное значение поля `{key}`: {reason}."))
        }
        SettingError::SecretKeyMissing { key } => Some(format!(
            "Не удалось сохранить секрет `{key}`: нужен ключ шифрования TASKLERQ_SECRET_KEY."
        )),
        SettingError::Cipher { key, .. } => Some(format!(
            "Не удалось зашифровать секрет `{key}`; ничего не сохранено."
        )),
        SettingError::Database(_) => None,
    }
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
    wizard_step_rejection(current, telegram_enabled, WIZARD_SKIP_ERROR)
}

/// Отказ шага, который сейчас не текущий: без записи и без второго действия.
fn wizard_stale(current: WizardStep, telegram_enabled: bool) -> Response {
    wizard_step_rejection(current, telegram_enabled, WIZARD_STALE_STEP_ERROR)
}

/// Повтор текущего шага с сообщением и статусом `400`.
fn wizard_step_rejection(
    current: WizardStep,
    telegram_enabled: bool,
    message: &'static str,
) -> Response {
    match current {
        WizardStep::Household | WizardStep::Llm | WizardStep::Token => render_with_status(
            ConsolePage::new(wizard_step_page(current, telegram_enabled)).error(message),
            StatusCode::BAD_REQUEST,
        ),
        WizardStep::Setup => redirect(SETUP_PATH),
        WizardStep::Done => redirect(HOME_PATH),
    }
}

/// Страница шага мастера с пустой формой и значениями по умолчанию.
fn wizard_step_page(step: WizardStep, telegram_enabled: bool) -> Page {
    let form = match step {
        WizardStep::Household => WizardForm::Household {
            name: String::new(),
            timezone: DEFAULT_TIMEZONE.to_owned(),
        },
        WizardStep::Token => WizardForm::Token {
            name: DEFAULT_TOKEN_NAME.to_owned(),
        },
        WizardStep::Setup | WizardStep::Llm | WizardStep::Done => WizardForm::None,
    };

    Page::Wizard {
        step,
        household_skip_allowed: telegram_enabled,
        form,
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

/// Вид страницы настроек LLM.
struct LlmView {
    /// Настройки сохранены.
    saved: bool,
    /// Токен отклонён: нет ключа шифрования.
    secret_rejected: bool,
    /// Флажок включения разбора.
    enabled: LlmToggle,
    /// Основной слот.
    primary: LlmSlotView,
    /// Резервный слот.
    fallback: LlmSlotView,
    /// Таймаут установки соединения.
    connect_timeout: LlmTextField,
    /// Суточная квота.
    daily_quota: LlmTextField,
}

/// Логическое поле формы.
struct LlmToggle {
    /// Флажок установлен.
    checked: bool,
    /// Подпись источника.
    source: &'static str,
    /// Значение задано окружением.
    readonly: bool,
}

/// Текстовое поле формы.
#[derive(Clone)]
struct LlmTextField {
    /// Значение; секретов здесь не бывает.
    value: String,
    /// Подпись источника.
    source: &'static str,
    /// Значение задано окружением.
    readonly: bool,
}

/// Состояние секрета без значения.
#[derive(Clone)]
struct LlmTokenView {
    /// «задан», «не задан» или «введите заново».
    label: &'static str,
    /// Подпись источника.
    source: &'static str,
    /// Секрет задан окружением: заменить и удалить нельзя.
    readonly: bool,
}

/// Поля одного слота на странице.
struct LlmSlotView {
    /// Адрес.
    url: LlmTextField,
    /// Модель.
    model: LlmTextField,
    /// Таймаут запроса.
    timeout: LlmTextField,
    /// Дополнительные параметры JSON.
    extra_json: LlmTextField,
    /// Токен: только состояние.
    token: LlmTokenView,
}

/// Собрать вид страницы из действующих значений.
///
/// `form` задаёт несекретные значения после отказа формы; `None` — значения из
/// настроек. Секрет из формы не берётся никогда.
fn llm_view(
    settings: &[ResolvedSetting],
    form: Option<&LlmForm>,
    saved: bool,
    secret_rejected: bool,
) -> LlmView {
    LlmView {
        saved,
        secret_rejected,
        enabled: toggle_view(
            settings,
            "llm.enabled",
            form.map(|form| form.enabled.is_some()),
        ),
        primary: slot_view(settings, PRIMARY_KEYS, form, true),
        fallback: slot_view(settings, FALLBACK_KEYS, form, false),
        connect_timeout: text_view(
            settings,
            "llm.connect_timeout_ms",
            form.and_then(|form| form.connect_timeout_ms.as_deref()),
        ),
        daily_quota: text_view(
            settings,
            "llm.daily_quota",
            form.and_then(|form| form.daily_quota.as_deref()),
        ),
    }
}

fn slot_view(
    settings: &[ResolvedSetting],
    keys: SlotKeys,
    form: Option<&LlmForm>,
    primary: bool,
) -> LlmSlotView {
    let fields = form.map(|form| form.slot(primary));
    LlmSlotView {
        url: text_view(settings, keys.url, fields.and_then(|fields| fields.url)),
        model: text_view(settings, keys.model, fields.and_then(|fields| fields.model)),
        timeout: text_view(
            settings,
            keys.timeout,
            fields.and_then(|fields| fields.timeout),
        ),
        extra_json: text_view(
            settings,
            keys.extra_json,
            fields.and_then(|fields| fields.extra_json),
        ),
        token: token_view(settings, keys.token),
    }
}

fn toggle_view(settings: &[ResolvedSetting], key: &str, submitted: Option<bool>) -> LlmToggle {
    let source = source_of(settings, key);
    let readonly = source == SettingSource::Env;
    let current = bool_of(settings, key).unwrap_or(false);
    LlmToggle {
        checked: if readonly {
            current
        } else {
            submitted.unwrap_or(current)
        },
        source: source_label(source),
        readonly,
    }
}

fn text_view(settings: &[ResolvedSetting], key: &str, submitted: Option<&str>) -> LlmTextField {
    let source = source_of(settings, key);
    let readonly = source == SettingSource::Env;
    let value = if readonly {
        text_of(settings, key)
    } else {
        submitted.map_or_else(|| text_of(settings, key), str::to_owned)
    };
    LlmTextField {
        value,
        source: source_label(source),
        readonly,
    }
}

fn token_view(settings: &[ResolvedSetting], key: &str) -> LlmTokenView {
    let source = source_of(settings, key);
    let state = resolved(settings, key)
        .and_then(|setting| setting.secret)
        .unwrap_or(SecretState::NotSet);
    LlmTokenView {
        label: secret_label(state),
        source: source_label(source),
        readonly: source == SettingSource::Env,
    }
}

fn secret_label(state: SecretState) -> &'static str {
    match state {
        SecretState::NotSet => "не задан",
        SecretState::Set => "задан",
        SecretState::KeyMissing => "ключ шифрования не задан — введите токен заново",
        SecretState::Undecryptable => "не расшифровывается — введите токен заново",
    }
}

fn source_label(source: SettingSource) -> &'static str {
    match source {
        SettingSource::Env => "окружение",
        SettingSource::Db => "база",
        SettingSource::Default => "по умолчанию",
    }
}

fn resolved<'a>(settings: &'a [ResolvedSetting], key: &str) -> Option<&'a ResolvedSetting> {
    settings.iter().find(|setting| setting.key == key)
}

fn source_of(settings: &[ResolvedSetting], key: &str) -> SettingSource {
    resolved(settings, key).map_or(SettingSource::Default, |setting| setting.source)
}

fn is_readonly(settings: &[ResolvedSetting], key: &str) -> bool {
    source_of(settings, key) == SettingSource::Env
}

fn text_of(settings: &[ResolvedSetting], key: &str) -> String {
    resolved(settings, key)
        .and_then(|setting| setting.value.as_ref())
        .map(value_text)
        .unwrap_or_default()
}

fn value_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Object(_) => serde_json::to_string(value).unwrap_or_default(),
        other => other.to_string(),
    }
}

fn bool_of(settings: &[ResolvedSetting], key: &str) -> Option<bool> {
    match resolved(settings, key)?.value.as_ref()? {
        Value::Bool(value) => Some(*value),
        Value::String(text) => match text.as_str() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        },
        _ => None,
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
        /// Данные формы шага.
        form: WizardForm,
    },
    /// Токен только что выпущен: открытое значение показывается один раз.
    TokenIssued {
        /// Открытый токен; в базу и журнал не попадает.
        token: String,
    },
    /// Настройки разбора.
    Llm(Box<LlmView>),
}

/// Поля шага мастера, которые возвращаются в форму после отказа.
enum WizardForm {
    /// У шага нет формы (LLM).
    None,
    /// Форма household.
    Household {
        /// Название.
        name: String,
        /// Часовой пояс IANA.
        timezone: String,
    },
    /// Форма токена.
    Token {
        /// Название токена.
        name: String,
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
            Self::TokenIssued { .. } => "Токен Home Assistant",
            Self::Llm(_) => "Разбор записей",
        }
    }

    /// Навигация вошедшего администратора.
    ///
    /// Определяется типом страницы: страницы за сессией показывают её всегда,
    /// страницы входа и восстановления — никогда.
    fn nav(&self) -> bool {
        matches!(
            self,
            Self::Home { .. }
                | Self::Password { .. }
                | Self::Wizard { .. }
                | Self::TokenIssued { .. }
                | Self::Llm(_)
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
                form,
            } => {
                let skip = if step.can_skip(*household_skip_allowed) {
                    step.slug()
                } else {
                    None
                };
                let skip = SkipBody { skip }.render()?;
                match form {
                    WizardForm::Household { name, timezone } => HouseholdBody {
                        title: step.title(),
                        description: step.description(),
                        name,
                        timezone,
                        skip,
                    }
                    .render(),
                    WizardForm::Token { name } => TokenBody {
                        title: step.title(),
                        description: step.description(),
                        name,
                        skip,
                    }
                    .render(),
                    WizardForm::None => WizardBody {
                        title: step.title(),
                        description: step.description(),
                        skip,
                    }
                    .render(),
                }
            }
            Self::TokenIssued { token } => TokenIssuedBody { token }.render(),
            Self::Llm(view) => {
                let primary = SlotBody {
                    legend: "Основной слот",
                    prefix: "primary",
                    slot: &view.primary,
                }
                .render()?;
                let fallback = SlotBody {
                    legend: "Резервный слот",
                    prefix: "fallback",
                    slot: &view.fallback,
                }
                .render()?;
                LlmBody {
                    saved: view.saved,
                    secret_rejected: view.secret_rejected,
                    enabled: &view.enabled,
                    connect_timeout: &view.connect_timeout,
                    daily_quota: &view.daily_quota,
                    primary,
                    fallback,
                }
                .render()
            }
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
<a href="/admin/llm">Разбор</a>
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

/// Форма пропуска шага мастера.
#[derive(Template)]
#[template(
    source = r#"{% match skip %}
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
struct SkipBody {
    /// Значение формы пропуска; `None` — пропуск недоступен.
    skip: Option<&'static str>,
}

/// Содержимое шага мастера без формы (LLM).
#[derive(Template)]
#[template(
    source = r#"<h1>{{ title }}</h1>
<p>{{ description }}</p>
<p><a href="/admin/llm">Настроить разбор</a></p>
{{ skip|safe }}
"#,
    ext = "html"
)]
struct WizardBody {
    /// Название шага.
    title: &'static str,
    /// Короткое пояснение шага.
    description: &'static str,
    /// Уже отрисованная форма пропуска.
    skip: String,
}

/// Содержимое шага household.
#[derive(Template)]
#[template(
    source = r#"<h1>{{ title }}</h1>
<p>{{ description }}</p>
<form method="post" action="/admin/wizard/household">
<p><label for="household-name">Название</label>
<input id="household-name" name="name" type="text" value="{{ name }}" maxlength="100" required></p>
<p><label for="household-timezone">Часовой пояс</label>
<input id="household-timezone" name="timezone" type="text" value="{{ timezone }}" maxlength="64" required></p>
<p><button type="submit">Создать дом</button></p>
</form>
{{ skip|safe }}
"#,
    ext = "html"
)]
struct HouseholdBody<'a> {
    /// Название шага.
    title: &'static str,
    /// Короткое пояснение шага.
    description: &'static str,
    /// Название household из формы.
    name: &'a str,
    /// Часовой пояс из формы.
    timezone: &'a str,
    /// Уже отрисованная форма пропуска.
    skip: String,
}

/// Содержимое шага токена Home Assistant.
#[derive(Template)]
#[template(
    source = r#"<h1>{{ title }}</h1>
<p>{{ description }}</p>
<form method="post" action="/admin/wizard/token">
<p><label for="token-name">Название токена</label>
<input id="token-name" name="name" type="text" value="{{ name }}" maxlength="64"></p>
<p><button type="submit">Выпустить токен</button></p>
</form>
{{ skip|safe }}
"#,
    ext = "html"
)]
struct TokenBody<'a> {
    /// Название шага.
    title: &'static str,
    /// Короткое пояснение шага.
    description: &'static str,
    /// Название токена из формы.
    name: &'a str,
    /// Уже отрисованная форма пропуска.
    skip: String,
}

/// Страница с токеном, который показывается один раз.
#[derive(Template)]
#[template(
    source = r#"<h1>Токен Home Assistant</h1>
<p>Скопируйте токен и вставьте его в настройки Home Assistant. Токен показывается один раз: после перехода он больше не будет доступен.</p>
<p><input id="issued-token" name="token" type="text" value="{{ token }}" readonly></p>
<p><a href="/admin/wizard">Продолжить</a></p>
"#,
    ext = "html"
)]
struct TokenIssuedBody<'a> {
    /// Открытый токен.
    token: &'a str,
}

/// Содержимое одного слота формы LLM.
#[derive(Template)]
#[template(
    source = r#"<fieldset>
<legend>{{ legend }}</legend>
<p><label for="llm-{{ prefix }}-url">Адрес</label>
<input id="llm-{{ prefix }}-url" name="{{ prefix }}_url" type="text" value="{{ slot.url.value }}"{% if slot.url.readonly %} readonly{% endif %}>
<span class="console-source">источник: {{ slot.url.source }}</span></p>
<p><label for="llm-{{ prefix }}-model">Модель</label>
<input id="llm-{{ prefix }}-model" name="{{ prefix }}_model" type="text" value="{{ slot.model.value }}"{% if slot.model.readonly %} readonly{% endif %}>
<span class="console-source">источник: {{ slot.model.source }}</span></p>
<p>Токен: <strong>{{ slot.token.label }}</strong>
<span class="console-source">источник: {{ slot.token.source }}</span></p>
{% if slot.token.readonly %}
{% else %}
<p><label for="llm-{{ prefix }}-token">Заменить токен (пустое поле — не менять)</label>
<input id="llm-{{ prefix }}-token" name="{{ prefix }}_token" type="password" autocomplete="new-password"></p>
<p><label><input name="{{ prefix }}_token_delete" type="checkbox" value="1"> Удалить токен</label></p>
{% endif %}
<p><label for="llm-{{ prefix }}-timeout">Таймаут запроса, мс</label>
<input id="llm-{{ prefix }}-timeout" name="{{ prefix }}_timeout_ms" type="text" inputmode="numeric" value="{{ slot.timeout.value }}"{% if slot.timeout.readonly %} readonly{% endif %}>
<span class="console-source">источник: {{ slot.timeout.source }}</span></p>
<p><label for="llm-{{ prefix }}-extra">Дополнительные параметры JSON</label>
<input id="llm-{{ prefix }}-extra" name="{{ prefix }}_extra_json" type="text" value="{{ slot.extra_json.value }}"{% if slot.extra_json.readonly %} readonly{% endif %}>
<span class="console-source">источник: {{ slot.extra_json.source }}</span></p>
</fieldset>
"#,
    ext = "html"
)]
struct SlotBody<'a> {
    /// Название слота.
    legend: &'static str,
    /// Префикс имён полей.
    prefix: &'static str,
    /// Поля слота.
    slot: &'a LlmSlotView,
}

/// Содержимое страницы настроек LLM.
#[derive(Template)]
#[template(
    source = r#"<h1>Разбор записей</h1>
{% if saved %}
<p role="status" class="console-notice">Настройки LLM сохранены.</p>
{% endif %}
{% if secret_rejected %}
<p role="alert" class="console-alert">Токен не сохранён: не задан ключ шифрования TASKLERQ_SECRET_KEY.{% if saved %} Остальные поля сохранены.{% endif %}</p>
{% endif %}
<form method="post" action="/admin/llm">
<fieldset>
<legend>Разбор</legend>
<p><label><input id="llm-enabled" name="enabled" type="checkbox" value="1"{% if enabled.checked %} checked{% endif %}{% if enabled.readonly %} disabled{% endif %}> Включить разбор</label>
<span class="console-source">источник: {{ enabled.source }}</span></p>
</fieldset>
{{ primary|safe }}
{{ fallback|safe }}
<fieldset>
<legend>Общие</legend>
<p><label for="llm-connect-timeout">Таймаут соединения, мс</label>
<input id="llm-connect-timeout" name="connect_timeout_ms" type="text" inputmode="numeric" value="{{ connect_timeout.value }}"{% if connect_timeout.readonly %} readonly{% endif %}>
<span class="console-source">источник: {{ connect_timeout.source }}</span></p>
<p><label for="llm-daily-quota">Суточная квота</label>
<input id="llm-daily-quota" name="daily_quota" type="text" inputmode="numeric" value="{{ daily_quota.value }}"{% if daily_quota.readonly %} readonly{% endif %}>
<span class="console-source">источник: {{ daily_quota.source }}</span></p>
</fieldset>
<p><button type="submit">Сохранить</button></p>
</form>
"#,
    ext = "html"
)]
struct LlmBody<'a> {
    /// Настройки сохранены.
    saved: bool,
    /// Токен отклонён: нет ключа шифрования.
    secret_rejected: bool,
    /// Флажок разбора.
    enabled: &'a LlmToggle,
    /// Таймаут соединения.
    connect_timeout: &'a LlmTextField,
    /// Суточная квота.
    daily_quota: &'a LlmTextField,
    /// Уже отрисованный основной слот.
    primary: String,
    /// Уже отрисованный резервный слот.
    fallback: String,
}

#[cfg(test)]
mod tests {
    use super::{
        ConsolePage, HouseholdForm, LlmSlotView, LlmTextField, LlmToggle, LlmTokenView, LlmView,
        Page, WizardForm, household_form_error, target_or_home,
    };
    use crate::admin_console::wizard::WizardStep;
    use crate::domain::error::DomainError;

    fn llm_view_for_tests(token_label: &'static str) -> LlmView {
        let text = |value: &str| LlmTextField {
            value: value.to_owned(),
            source: "база",
            readonly: false,
        };
        let slot = || LlmSlotView {
            url: text("https://llm.example.test/v1"),
            model: text("vendor/model"),
            timeout: text("10000"),
            extra_json: text("{}"),
            token: LlmTokenView {
                label: token_label,
                source: "база",
                readonly: false,
            },
        };
        LlmView {
            saved: true,
            secret_rejected: false,
            enabled: LlmToggle {
                checked: true,
                source: "база",
                readonly: false,
            },
            primary: slot(),
            fallback: slot(),
            connect_timeout: text("1000"),
            daily_quota: text("200"),
        }
    }

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
                form: WizardForm::None,
            },
            Page::Wizard {
                step: WizardStep::Household,
                household_skip_allowed: false,
                form: WizardForm::Household {
                    name: "Дом".to_owned(),
                    timezone: "Europe/Moscow".to_owned(),
                },
            },
            Page::TokenIssued {
                token: "tlq_token".to_owned(),
            },
            Page::Llm(Box::new(llm_view_for_tests("задан"))),
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

    fn household(name: &str, timezone: &str, telegram: bool) -> Page {
        Page::Wizard {
            step: WizardStep::Household,
            household_skip_allowed: telegram,
            form: WizardForm::Household {
                name: name.to_owned(),
                timezone: timezone.to_owned(),
            },
        }
    }

    #[test]
    fn the_household_skip_button_depends_on_telegram() {
        let without = ConsolePage::new(household("", "Europe/Moscow", false))
            .render()
            .expect("the page renders");
        assert!(!without.contains("Пропустить"), "{without}");

        let with = ConsolePage::new(household("", "Europe/Moscow", true))
            .render()
            .expect("the page renders");
        assert!(with.contains("Пропустить"), "{with}");
        assert!(with.contains("value=\"household\""), "{with}");
    }

    #[test]
    fn the_household_form_keeps_values_and_escapes_them() {
        let html = ConsolePage::new(household("Дом <b>", "\"x\"", false))
            .render()
            .expect("the page renders");

        assert!(
            html.contains("action=\"/admin/wizard/household\""),
            "{html}"
        );
        assert!(html.contains("value=\"Дом &#60;b&#62;\""), "{html}");
        assert!(html.contains("value=\"&#34;x&#34;\""), "{html}");
        assert!(!html.contains("Дом <b>"), "{html}");
    }

    #[test]
    fn the_token_page_shows_the_value_once() {
        let page = Page::TokenIssued {
            token: "tlq_open_token".to_owned(),
        };
        let html = ConsolePage::new(page).render().expect("the page renders");

        assert_eq!(html.matches("tlq_open_token").count(), 1, "{html}");
        assert!(html.contains("readonly"), "{html}");
        assert!(html.contains("показывается один раз"), "{html}");
        assert!(html.contains("href=\"/admin/wizard\""), "{html}");
    }

    #[test]
    fn a_failed_wizard_target_leads_home() {
        assert_eq!(target_or_home(Ok("/admin/wizard")), "/admin/wizard");
        assert_eq!(target_or_home(Err(DomainError::NotFound)), "/admin");
    }

    #[test]
    fn the_llm_page_shows_only_the_secret_state() {
        let html = ConsolePage::new(Page::Llm(Box::new(llm_view_for_tests("задан"))))
            .render()
            .expect("the page renders");

        assert!(html.contains("action=\"/admin/llm\""), "{html}");
        assert!(html.contains("источник: база"), "{html}");
        assert!(html.contains("Токен: <strong>задан</strong>"), "{html}");
        assert!(html.contains("Заменить токен"), "{html}");
        assert!(
            html.contains("name=\"primary_token\" type=\"password\""),
            "the replacement token field never carries a value: {html}"
        );
        assert!(!html.contains("value=\"tlq_"), "{html}");
    }

    #[test]
    fn a_validation_error_keeps_the_household_form() {
        let form = HouseholdForm {
            name: "Дом".to_owned(),
            timezone: "Europe/Moscow".to_owned(),
        };
        let response = household_form_error(
            WizardStep::Household,
            false,
            &form,
            "household name is invalid",
        );

        assert_eq!(response.status(), axum::http::StatusCode::OK);
    }
}
