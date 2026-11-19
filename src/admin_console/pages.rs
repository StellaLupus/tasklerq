//! Страницы консоли: setup, вход, восстановление, смена пароля, мастер,
//! настройки LLM и главная. Страницы household — в [`super::households`].
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
//! Формы — обычные POST без JavaScript; htmx подключён макетом для опроса хода
//! проверки слота. Без JavaScript страница хода не перезагружается сама: ссылку
//! «Обновить» показывает `<noscript>`, поэтому браузер с htmx не делает второй
//! запрос параллельно с опросом.
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
use super::checks::{CheckOutcome, CheckSlot, CheckSnapshot};
use super::codes::{OWNER_CLAIM_TTL_HOURS, TELEGRAM_LINK_TTL_MINUTES};
use super::login::{self, LoginError, LoginOutcome};
use super::owner::{self, OwnerIssueOutcome, OwnerLinkForm};
use super::password::{self, Password};
use super::recovery::{self, RecoveryCheck, RecoveryError, RecoveryOutcome};
use super::router::AdminState;
use super::session::{self, AdminSession, LOGIN_PATH};
use super::setup::{self, SetupError, SetupOutcome};
use super::telegram::{self, BotChange, TelegramForm, TelegramSubmitOutcome, TelegramView};
use super::telegram_link::{self, BindingOutcome, IssueOutcome};
use super::wizard::{self, WizardStep};
use crate::VERSION;
use crate::admin;
use crate::capture::eval::TitleMatch;
use crate::capture::slot_check::{self, CheckError, PreparedCheck};
use crate::capture::slot_status::SlotStatus;
use crate::domain::error::DomainError;
use crate::domain::settings::{DEFAULT_TIMEZONE, validate_timezone};
use crate::domain::validation::normalize_text;
use crate::http::health::{self, ReadinessInput};
use crate::llm::{LlmApplyError, LlmChange, OpenAiBackend};
use crate::rate_limit::RateDecision;
use crate::settings::{
    self, ResolvedSetting, SecretProblem, SecretState, SettingError, SettingSource,
};
use crate::storage::repositories::admin_accounts;
use crate::storage::repositories::households;
use crate::storage::repositories::llm_slot_settings;

/// Путь страницы первого входа.
const SETUP_PATH: &str = "/admin/setup";
/// Путь главной страницы.
const HOME_PATH: &str = "/admin";
/// Путь страницы смены пароля; `?changed=1` показывает подтверждение.
const PASSWORD_PATH: &str = "/admin/password";
/// Путь страницы настроек LLM.
pub(crate) const LLM_PATH: &str = "/admin/llm";
/// Путь страницы Telegram.
pub(crate) const TELEGRAM_PATH: &str = telegram::TELEGRAM_PATH;
/// Путь перевыпуска секрета webhook.
pub(crate) const TELEGRAM_SECRET_PATH: &str = telegram::TELEGRAM_SECRET_PATH;
/// Путь выпуска ссылки-claim владельца.
pub(crate) const OWNER_PATH: &str = owner::OWNER_PATH;
/// Путь выпуска ссылки привязки Telegram.
pub(crate) const TELEGRAM_LINK_PATH: &str = telegram_link::TELEGRAM_LINK_PATH;
/// Путь подтверждения кандидата.
pub(crate) const TELEGRAM_LINK_CONFIRM_PATH: &str = telegram_link::TELEGRAM_LINK_CONFIRM_PATH;
/// Путь отклонения кандидата.
pub(crate) const TELEGRAM_LINK_REJECT_PATH: &str = telegram_link::TELEGRAM_LINK_REJECT_PATH;
/// Путь отвязки подтверждённого аккаунта.
pub(crate) const TELEGRAM_LINK_UNLINK_PATH: &str = telegram_link::TELEGRAM_LINK_UNLINK_PATH;
/// Путь страницы хода проверки слота.
pub(crate) const LLM_CHECK_PATH: &str = "/admin/llm/check";
/// Путь фрагмента хода для htmx; без JavaScript страница обновляется ссылкой.
pub(crate) const LLM_CHECK_FRAGMENT_PATH: &str = "/admin/llm/check-fragment";
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
const WIZARD_TOKEN_AMBIGUOUS_ERROR: &str = "В режиме single больше одного household: выпустите токен командой \
     `tasklerq admin token create --household <id>`.";
const WIZARD_TOKEN_NO_HOUSEHOLD_ERROR: &str = "В инсталляции нет активного household: создайте дом и \
     повторите выпуск токена.";
const WIZARD_TOKEN_CHANGED_ERROR: &str =
    "Не удалось выпустить токен: состояние инсталляции изменилось. Повторите попытку.";
const INVALID_LINK_TITLE: &str = "Ссылка недействительна";
const STALE_CANDIDATE_ERROR: &str = "Кандидат уже изменился. Показываю актуальное состояние.";

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

/// Поля формы кандидата на привязку Telegram.
#[derive(Deserialize)]
pub(crate) struct CandidateForm {
    /// Telegram id кандидата, показанный при отрисовке формы.
    candidate: String,
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
    pub(crate) name: String,
    /// Часовой пояс IANA.
    pub(crate) timezone: String,
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
    /// Служебное поле: браузерная форма присылает его всегда, поэтому
    /// отсутствие флажка `enabled` означает «снят», а не «поле не прислали».
    #[serde(default)]
    enabled_present: Option<String>,
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

/// Метки страницы Telegram: сохранение и режим шага мастера.
#[derive(Deserialize)]
pub(crate) struct TelegramQuery {
    #[serde(default)]
    saved: Option<String>,
    #[serde(default)]
    wizard: Option<String>,
}

/// Поля формы проверки слота.
#[derive(Deserialize)]
pub(crate) struct CheckForm {
    /// Выбранный стандарт названий; нет поля — действующий стандарт слота.
    #[serde(default)]
    titles: Option<String>,
}

/// `GET /admin`: главная страница за сессией.
///
/// Показывает состояние инсталляции без секретов и пользовательского
/// содержимого: версию, готовность с предупреждениями, режим размещения, число
/// household, Telegram и статусы слотов. Готовность считает та же сборка, что
/// `/health/ready`.
pub(crate) async fn index(State(state): State<AdminState>, _session: AdminSession) -> Response {
    home_response(&state, Utc::now(), None).await
}

/// Главная с необязательным сообщением формы и его статусом.
async fn home_response(
    state: &AdminState,
    now: DateTime<Utc>,
    error: Option<(&str, StatusCode)>,
) -> Response {
    match home_view(state, now).await {
        Ok(view) => {
            let page = ConsolePage::new(Page::Home(Box::new(view)));
            match error {
                Some((message, status)) => render_with_status(page.error(message), status),
                None => render(page),
            }
        }
        Err(error) => internal(error),
    }
}

/// Выпустить ссылку-claim владельца для выбранной цели.
///
/// Открытый код показывается только в этом ответе; при повторном открытии
/// страницы он недоступен, потому что в базе хранится хеш.
pub(crate) async fn owner_submit(
    State(state): State<AdminState>,
    _session: AdminSession,
    Form(form): Form<OwnerLinkForm>,
) -> Response {
    let now = Utc::now();
    match owner::issue(&state, &form.target, now).await {
        Ok(OwnerIssueOutcome::Issued(link)) => {
            let view = OwnerLinkView {
                link: link.link,
                expires_at: link.expires_at.to_rfc3339(),
                ttl_hours: OWNER_CLAIM_TTL_HOURS,
                svg: link.svg,
            };
            render(ConsolePage::new(Page::OwnerLinkIssued(Box::new(view))))
        }
        Ok(OwnerIssueOutcome::Rejected(message)) => {
            home_response(&state, now, Some((&message, StatusCode::BAD_REQUEST))).await
        }
        Err(error) => internal(error),
    }
}

/// Выпустить ссылку привязки Telegram.
///
/// Открытый код показывается только в этом ответе: в базе хранится хеш.
pub(crate) async fn telegram_link_issue(
    State(state): State<AdminState>,
    _session: AdminSession,
) -> Response {
    let now = Utc::now();
    match telegram_link::issue(&state, now).await {
        Ok(IssueOutcome::Issued(link)) => {
            let view = TelegramLinkView {
                link: link.link,
                expires_at: link.expires_at.to_rfc3339(),
                ttl_minutes: TELEGRAM_LINK_TTL_MINUTES,
            };
            render(ConsolePage::new(Page::TelegramLinkIssued(Box::new(view))))
        }
        Ok(IssueOutcome::Rejected(message)) => {
            home_response(&state, now, Some((&message, StatusCode::BAD_REQUEST))).await
        }
        Err(error) => internal(error),
    }
}

/// Подтвердить кандидата на привязку Telegram.
pub(crate) async fn telegram_link_confirm(
    State(state): State<AdminState>,
    _session: AdminSession,
    Form(form): Form<CandidateForm>,
) -> Response {
    telegram_link_candidate_action(&state, &form.candidate, true).await
}

/// Отклонить кандидата на привязку Telegram.
pub(crate) async fn telegram_link_reject(
    State(state): State<AdminState>,
    _session: AdminSession,
    Form(form): Form<CandidateForm>,
) -> Response {
    telegram_link_candidate_action(&state, &form.candidate, false).await
}

/// Подтвердить или отклонить кандидата; форма несёт его id.
async fn telegram_link_candidate_action(
    state: &AdminState,
    candidate: &str,
    confirm: bool,
) -> Response {
    let now = Utc::now();
    let Some(candidate) = candidate.trim().parse::<i64>().ok() else {
        return home_response(
            state,
            now,
            Some((STALE_CANDIDATE_ERROR, StatusCode::BAD_REQUEST)),
        )
        .await;
    };

    let outcome = if confirm {
        telegram_link::confirm(state, candidate, now).await
    } else {
        telegram_link::reject(state, candidate).await
    };
    match outcome {
        Ok(BindingOutcome::Applied) => home_response(state, now, None).await,
        Ok(BindingOutcome::Stale) => {
            home_response(
                state,
                now,
                Some((STALE_CANDIDATE_ERROR, StatusCode::BAD_REQUEST)),
            )
            .await
        }
        Ok(BindingOutcome::NotLinked) => home_response(state, now, None).await,
        Err(error) => internal(error),
    }
}

/// Снять подтверждённую привязку Telegram.
pub(crate) async fn telegram_link_unlink(
    State(state): State<AdminState>,
    _session: AdminSession,
) -> Response {
    let now = Utc::now();
    match telegram_link::unlink(&state, now).await {
        Ok(_) => home_response(&state, now, None).await,
        Err(error) => internal(error),
    }
}

/// Данные главной страницы.
struct HomeView {
    /// Версия сервиса.
    version: &'static str,
    /// Мастер ещё не завершён: показать ссылку «Продолжить настройку».
    setup_incomplete: bool,
    /// Сервис готов.
    ready: bool,
    /// Причина неготовности без технических подробностей.
    readiness_problem: Option<&'static str>,
    /// Предупреждения readiness с подписями.
    warnings: Vec<HomeWarning>,
    /// Есть ли предупреждения.
    has_warnings: bool,
    /// Режим размещения: `single` или `multi`.
    tenancy: &'static str,
    /// Число активных household.
    households: i64,
    /// Telegram включён.
    telegram_enabled: bool,
    /// Цели ссылки-claim владельца.
    owner_targets: Vec<owner::OwnerTarget>,
    /// Бот включён и его имя известно: можно выпускать ссылку.
    owner_telegram_ready: bool,
    /// Привязка Telegram к учётной записи администратора.
    telegram: HomeTelegram,
    /// Статусы основного и резервного слотов.
    slots: Vec<HomeSlot>,
}

/// Состояние блока «Telegram администратора» на главной.
struct HomeTelegram {
    /// Бот включён и его имя известно: можно выпускать ссылку.
    ready: bool,
    /// Имя подтверждённого аккаунта.
    linked: Option<String>,
    /// Кандидат, открывший ссылку привязки.
    candidate: Option<HomeTelegramCandidate>,
}

/// Кандидат на привязку Telegram.
struct HomeTelegramCandidate {
    /// Telegram id кандидата для сверки формы.
    id: i64,
    /// Имя из Telegram.
    name: String,
}

/// Предупреждение readiness на главной.
struct HomeWarning {
    /// Код предупреждения; справочно рядом с подписью.
    code: String,
    /// Понятная подпись.
    label: String,
}

/// Выпущенная ссылка-claim: открытый код показывается один раз.
struct OwnerLinkView {
    /// Ссылка `https://t.me/<бот>?start=own_<код>`.
    link: String,
    /// Срок действия в RFC 3339.
    expires_at: String,
    /// Срок в часах для подписи.
    ttl_hours: i64,
    /// SVG QR-кода, сгенерированный сервером.
    svg: String,
}

/// Выпущенная ссылка привязки Telegram: открытый код показывается один раз.
struct TelegramLinkView {
    /// Ссылка `https://t.me/<бот>?start=adm_<код>`.
    link: String,
    /// Срок действия в RFC 3339.
    expires_at: String,
    /// Срок в минутах для подписи.
    ttl_minutes: i64,
}

/// Статус одного слота LLM на главной.
struct HomeSlot {
    /// Название слота.
    label: &'static str,
    /// Статус без адреса, модели и токена.
    status: String,
    /// Показать ссылку на проверку.
    check_link: bool,
}

/// Собрать данные главной: готовность, счёт household и статусы слотов.
async fn home_view(state: &AdminState, now: DateTime<Utc>) -> Result<HomeView, DomainError> {
    let telegram = telegram_wizard_state(state).await?;
    let snapshot = wizard::read_state(
        &state.pool,
        state.wizard(),
        state.llm_settings(),
        telegram,
        now,
    )
    .await?;

    let readiness = health::readiness(ReadinessInput {
        pool: &state.pool,
        tenancy_mode: state.wizard().tenancy,
        queue_lag_limit: state.queue_lag_limit(),
        telegram_active: state.telegram_active(),
        secret_cipher: state.llm_settings().cipher(),
        llm_slots: state.llm_slots(),
        now,
    })
    .await;
    let (ready, readiness_problem, codes) = match readiness {
        Ok(codes) => (true, None, codes),
        Err(failure) => (false, Some(failure.label()), Vec::new()),
    };
    let warnings: Vec<HomeWarning> = codes
        .into_iter()
        .map(|code| HomeWarning {
            label: warning_label(&code),
            code,
        })
        .collect();

    let mut conn = state.pool.acquire().await?;
    let active_households = households::count_active(&mut conn).await?;
    let account = admin_accounts::get(&mut conn).await?;
    drop(conn);

    let owner_targets = owner::targets(state).await?;
    let owner_telegram_ready = owner_targets.telegram_ready;
    let telegram = HomeTelegram {
        ready: owner_telegram_ready,
        linked: account
            .as_ref()
            .and_then(|account| account.telegram_name.clone()),
        candidate: account.as_ref().and_then(|account| {
            Some(HomeTelegramCandidate {
                id: account.pending_telegram_user_id?,
                name: account.pending_telegram_name.clone()?,
            })
        }),
    };

    let slots = home_slots(state, now).await;

    Ok(HomeView {
        version: VERSION,
        setup_incomplete: wizard::wizard_step(&snapshot) != WizardStep::Done,
        ready,
        readiness_problem,
        has_warnings: !warnings.is_empty(),
        warnings,
        tenancy: match state.wizard().tenancy {
            crate::config::TenancyMode::Single => "single",
            crate::config::TenancyMode::Multi => "multi",
        },
        households: active_households,
        telegram_enabled: snapshot.telegram_enabled,
        owner_targets: owner_targets.targets,
        owner_telegram_ready,
        telegram,
        slots,
    })
}

/// Статусы слотов из снимка и сохранённых проверок.
///
/// Ненастроенный слот — «не настроен»; настроенный без проверки — «не проверен».
/// Ссылка на проверку есть у настроенного слота, который проверку не прошёл.
async fn home_slots(state: &AdminState, now: DateTime<Utc>) -> Vec<HomeSlot> {
    let snapshot = state.llm_settings().handle().load();
    let effective = snapshot.effective();
    let statuses = state.llm_slots().statuses(&state.pool, now).await;

    CheckSlot::ALL
        .into_iter()
        .map(|slot| {
            let configured = slot.config(effective).is_some();
            let report = statuses.iter().find(|report| report.slot == slot.name());
            let status = match (configured, report) {
                (false, _) => "не настроен",
                (true, None) => "не проверен",
                (true, Some(report)) => slot_status_label(report.status),
            };
            HomeSlot {
                label: match slot {
                    CheckSlot::Primary => "Основной слот",
                    CheckSlot::Fallback => "Резервный слот",
                },
                status: status.to_owned(),
                check_link: configured && report.is_none_or(|report| !report.status.is_passed()),
            }
        })
        .collect()
}

/// Подпись статуса слота без адреса и модели.
fn slot_status_label(status: SlotStatus) -> &'static str {
    match status {
        SlotStatus::Unverified => "не проверен",
        SlotStatus::Stale => "устарел",
        SlotStatus::Passed => "проверен",
        SlotStatus::Failed { .. } => "не прошёл проверку",
    }
}

/// Понятная подпись предупреждения readiness; код показывается рядом.
///
/// Коды сопоставляются с константами [`health`], а не с литералами:
/// `readiness()` строит предупреждения только из них. Новый код в
/// `WARNING_CODES` без подписи ловит тест
/// `every_readiness_warning_has_a_console_label`, а изменение строки кода —
/// тесты `/readyz`, которые сверяют ответ байт-в-байт.
fn warning_label(code: &str) -> String {
    match code {
        health::WARNING_MULTIPLE_HOUSEHOLDS => "в режиме single больше одного household".to_owned(),
        health::WARNING_LLM_UNAVAILABLE => "все настроенные слоты LLM недоступны".to_owned(),
        _ if code == SecretProblem::KeyMissing.code() => {
            "нет ключа шифрования сохранённых секретов".to_owned()
        }
        _ if code == SecretProblem::Undecryptable.code() => {
            "сохранённые секреты не расшифровываются".to_owned()
        }
        _ => match code
            .strip_prefix(health::WARNING_LLM_SLOT_UNVERIFIED)
            .and_then(|rest| rest.strip_prefix(':'))
        {
            Some(rest) => {
                let mut parts = rest.splitn(2, ':');
                let slot = parts.next().unwrap_or_default();
                match parts.next() {
                    Some(mode) => {
                        format!("слот LLM {slot}: проверка ({mode}) не пройдена")
                    }
                    None => format!("слот LLM {slot} не проверен"),
                }
            }
            None => "неизвестное предупреждение".to_owned(),
        },
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
    let settings = match state.llm_settings().read_settings().await {
        Ok(settings) => settings,
        Err(error) => return internal(error),
    };
    let checks = match llm_checks(&state).await {
        Ok(checks) => checks,
        Err(error) => return internal(error),
    };
    render(ConsolePage::new(Page::Llm(Box::new(llm_view(
        &settings,
        None,
        query.saved.is_some(),
        query.secret.as_deref() == Some("key_missing"),
        checks,
    )))))
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

    // Применение и аудит — одна отсоединённая задача: обрыв HTTP-запроса не
    // оставляет изменённые настройки без события журнала. Ожидание результата
    // отменяемо, сама запись и аудит — нет.
    let task = tokio::spawn(save_llm(state.clone(), pending, all, now));
    match task.await {
        Ok(LlmSaveOutcome::Saved) => redirect(LLM_SAVED_PATH),
        Ok(LlmSaveOutcome::SecretRejected { others_saved: true }) => {
            redirect(LLM_SECRET_REJECTED_PATH)
        }
        Ok(LlmSaveOutcome::SecretRejected {
            others_saved: false,
        }) => redirect(LLM_SECRET_ONLY_REJECTED_PATH),
        Ok(LlmSaveOutcome::Failed(error)) => llm_error_page(&state, &form, &error).await,
        Err(error) => internal(error),
    }
}

/// Итог отсоединённого сохранения настроек LLM.
enum LlmSaveOutcome {
    /// Всё сохранено и записано в аудит.
    Saved,
    /// Токен отклонён: нет ключа шифрования.
    SecretRejected {
        /// Остальные поля сохранены отдельной группой.
        others_saved: bool,
    },
    /// Применение не удалось; ничего не изменено.
    Failed(LlmApplyError),
}

/// Применить группу изменений и записать её в аудит.
///
/// Группа с секретом при отсутствии ключа шифрования сохраняется без секрета:
/// остальные поля формы не должны теряться. Аудит пишется в той же задаче, что
/// и применение, поэтому отмена запроса не оставляет сохранение без события.
async fn save_llm(
    state: AdminState,
    pending: Vec<PendingChange>,
    all: Vec<LlmChange>,
    now: DateTime<Utc>,
) -> LlmSaveOutcome {
    let has_secret = pending.iter().any(PendingChange::is_secret);

    if has_secret {
        match state.llm_settings().apply(&all).await {
            Ok(_) => {
                audit_changes(&state, &pending, now).await;
                return LlmSaveOutcome::Saved;
            }
            Err(LlmApplyError::Setting(SettingError::SecretKeyMissing { .. })) => {
                let plain: Vec<LlmChange> = pending
                    .iter()
                    .filter(|change: &&PendingChange| !change.is_secret())
                    .map(|change| change.change.clone())
                    .collect();
                if plain.is_empty() {
                    return LlmSaveOutcome::SecretRejected {
                        others_saved: false,
                    };
                }
                return match state.llm_settings().apply(&plain).await {
                    Ok(_) => {
                        audit_plain_changes(&state, &pending, now).await;
                        LlmSaveOutcome::SecretRejected { others_saved: true }
                    }
                    Err(error) => LlmSaveOutcome::Failed(error),
                };
            }
            Err(error) => return LlmSaveOutcome::Failed(error),
        }
    }

    match state.llm_settings().apply(&all).await {
        Ok(_) => {
            audit_changes(&state, &pending, now).await;
            LlmSaveOutcome::Saved
        }
        Err(error) => LlmSaveOutcome::Failed(error),
    }
}

/// `GET /admin/llm/check`: страница хода проверки слота.
///
/// Пока прогон идёт, ход обновляет htmx-опрос фрагмента. Без JavaScript
/// автоперезагрузки нет: `<noscript>` показывает ссылку «Обновить», и браузер с
/// JavaScript не делает второй запрос параллельно с опросом.
pub(crate) async fn llm_check_page(
    State(state): State<AdminState>,
    _session: AdminSession,
) -> Response {
    render(ConsolePage::new(Page::Check(state.checks().current())))
}

/// `GET /admin/llm/check-fragment`: ход проверки для htmx.
///
/// Фрагмент опрашивает себя раз в 2 секунды, пока прогон идёт; после отчёта
/// атрибуты опроса пропадают, и страница перестаёт ходить в сеть.
pub(crate) async fn llm_check_fragment(
    State(state): State<AdminState>,
    _session: AdminSession,
) -> Response {
    let view = check_view(state.checks().current().as_ref());
    match (CheckBody { view: &view }).render() {
        Ok(body) => (StatusCode::OK, Html(body)).into_response(),
        Err(error) => internal(error),
    }
}

/// `POST /admin/llm/check/{slot}`: запустить проверку слота в фоне.
///
/// Проверка берёт слот из действующего снимка — того же, что строит
/// `capture-eval` из окружения, базы и ключа. Прогон идёт отдельной задачей:
/// обрыв HTTP-запроса его не прерывает. Пока проверка идёт, повторный POST
/// ведёт на её страницу и не запускает второй прогон.
pub(crate) async fn llm_check_start(
    State(state): State<AdminState>,
    _session: AdminSession,
    Path(slot): Path<String>,
    Form(form): Form<CheckForm>,
) -> Response {
    let Some(slot) = CheckSlot::from_slug(&slot) else {
        return (StatusCode::BAD_REQUEST, "Unknown slot").into_response();
    };

    let snapshot = state.llm_settings().handle().load();
    let effective = snapshot.effective();
    let Some(config) = slot.config(effective).cloned() else {
        let message = format!("Слот {} не настроен: проверять нечего.", slot.slug());
        return llm_page_with_message(&state, &message, StatusCode::BAD_REQUEST).await;
    };

    let set_text = slot_check::BUILTIN_SET.to_owned();
    let set = match slot_check::load_set(&set_text) {
        Ok(set) => set,
        Err(error) => return internal(error),
    };
    let backend = match OpenAiBackend::from_slot(&config, effective.connect_timeout()) {
        Ok(backend) => backend,
        Err(error) => return internal(error),
    };
    let prepared = PreparedCheck {
        slot_name: slot.name(),
        slot: config,
        backend: Box::new(backend),
        set,
        set_text,
    };
    let titles = form.titles.as_deref().and_then(TitleMatch::from_code);

    if !state
        .checks()
        .try_start(slot.name(), slot_check::builtin_phrase_count())
    {
        return redirect(LLM_CHECK_PATH);
    }

    let check_state = state.checks().clone();
    let checks = check_state.clone();
    let task_state = state.clone();
    check_state.spawn(async move {
        let progress = |done, total| checks.advance(done, total);
        let outcome =
            slot_check::run_check_with_progress(&task_state.pool, prepared, titles, progress).await;
        // Успешный прогон записал строку проверки; кеш статусов привязан к
        // версии снимка, поэтому без сброса главная и `/health/ready` до
        // минуты показывали бы прежний статус.
        task_state.llm_slots().invalidate();
        match outcome {
            Ok(result) => checks.finish_result(result),
            Err(CheckError::Busy) => checks.fail("Слот не проверен: проверка уже идёт."),
            Err(error) => checks.fail(format!("Проверка не выполнена: {error}")),
        }
    });

    redirect(LLM_CHECK_PATH)
}

/// `GET /admin/telegram`: страница подключения бота за сессией.
pub(crate) async fn telegram_page(
    State(state): State<AdminState>,
    _session: AdminSession,
    Query(query): Query<TelegramQuery>,
) -> Response {
    let wizard = query.wizard.is_some();
    match telegram::view(&state, None, query.saved.is_some(), wizard).await {
        Ok(view) => render(ConsolePage::new(Page::Telegram(Box::new(view)))),
        Err(error) => internal(error),
    }
}

/// `POST /admin/telegram`: проверить токен, сохранить группу и применить.
pub(crate) async fn telegram_submit(
    State(state): State<AdminState>,
    _session: AdminSession,
    Form(form): Form<TelegramForm>,
) -> Response {
    let wizard = form.is_wizard();
    match telegram::submit(&state, &form, Utc::now()).await {
        Ok(TelegramSubmitOutcome::Saved { apply_error: None }) => redirect(if wizard {
            wizard::WIZARD_PATH
        } else {
            telegram::TELEGRAM_SAVED_PATH
        }),
        Ok(TelegramSubmitOutcome::Saved {
            apply_error: Some(message),
        }) => telegram_message(&state, Some(&form), true, wizard, &message, StatusCode::OK).await,
        Ok(TelegramSubmitOutcome::ConfirmBotChange(change)) => {
            telegram_confirmation(&state, &form, wizard, change).await
        }
        Ok(TelegramSubmitOutcome::Rejected { status, message }) => {
            telegram_message(&state, Some(&form), false, wizard, &message, status).await
        }
        Err(error) => internal(error),
    }
}

/// `POST /admin/telegram/secret`: новый секрет webhook и переключение.
pub(crate) async fn telegram_reissue_secret(
    State(state): State<AdminState>,
    _session: AdminSession,
) -> Response {
    match telegram::reissue_secret(&state, Utc::now()).await {
        Ok(TelegramSubmitOutcome::Saved { apply_error: None }) => {
            redirect(telegram::TELEGRAM_SAVED_PATH)
        }
        Ok(TelegramSubmitOutcome::Saved {
            apply_error: Some(message),
        }) => telegram_message(&state, None, true, false, &message, StatusCode::OK).await,
        Ok(TelegramSubmitOutcome::ConfirmBotChange(_)) => {
            internal("bot change confirmation is not expected for a secret reissue")
        }
        Ok(TelegramSubmitOutcome::Rejected { status, message }) => {
            telegram_message(&state, None, false, false, &message, status).await
        }
        Err(error) => internal(error),
    }
}

/// Показать форму подтверждения смены бота; введённый токен не возвращается.
async fn telegram_confirmation(
    state: &AdminState,
    form: &TelegramForm,
    wizard: bool,
    change: BotChange,
) -> Response {
    match telegram::view(state, Some(form), false, wizard).await {
        Ok(mut view) => {
            view.bot_change = Some(change);
            render(ConsolePage::new(Page::Telegram(Box::new(view))))
        }
        Err(error) => internal(error),
    }
}

async fn telegram_message(
    state: &AdminState,
    form: Option<&TelegramForm>,
    saved: bool,
    wizard: bool,
    message: &str,
    status: StatusCode,
) -> Response {
    match telegram::view(state, form, saved, wizard).await {
        Ok(view) => render_with_status(
            ConsolePage::new(Page::Telegram(Box::new(view))).error(message),
            status,
        ),
        Err(error) => internal(error),
    }
}

/// `GET /admin/wizard`: текущий шаг мастера за сессией.
pub(crate) async fn wizard_page(
    State(state): State<AdminState>,
    _session: AdminSession,
) -> Response {
    match wizard::read_state(
        &state.pool,
        state.wizard(),
        state.llm_settings(),
        match telegram_wizard_state(&state).await {
            Ok(telegram) => telegram,
            Err(error) => return internal(error),
        },
        Utc::now(),
    )
    .await
    {
        Ok(snapshot) => {
            let step = wizard::wizard_step(&snapshot);
            match step {
                WizardStep::Done => redirect(HOME_PATH),
                WizardStep::Setup => redirect(SETUP_PATH),
                WizardStep::Telegram => match telegram::view(&state, None, false, true).await {
                    Ok(view) => render(ConsolePage::new(Page::Telegram(Box::new(view)))),
                    Err(error) => internal(error),
                },
                WizardStep::Household | WizardStep::Llm | WizardStep::Token => render(
                    ConsolePage::new(wizard_step_page(step, snapshot.telegram_enabled)),
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
    let snapshot = match wizard::read_state(
        &state.pool,
        state.wizard(),
        state.llm_settings(),
        match telegram_wizard_state(&state).await {
            Ok(telegram) => telegram,
            Err(error) => return internal(error),
        },
        now,
    )
    .await
    {
        Ok(snapshot) => snapshot,
        Err(error) => return internal(error),
    };
    let current = wizard::wizard_step(&snapshot);
    let telegram_enabled = snapshot.telegram_enabled;
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
    let telegram_enabled = match telegram_wizard_state(&state).await {
        Ok(telegram) => telegram.enabled,
        Err(error) => return internal(error),
    };
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
    let telegram_enabled = match telegram_wizard_state(&state).await {
        Ok(telegram) => telegram.enabled,
        Err(error) => return internal(error),
    };
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
    match admin::create_wizard_token(&state.pool, state.wizard().tenancy, &name, &scopes, now).await
    {
        Ok(Some(issued)) => render(ConsolePage::new(Page::TokenIssued {
            token: issued.token,
        })),
        // Параллельный POST уже выпустил токен: второй не выпускается.
        Ok(None) => render_with_status(
            ConsolePage::new(Page::Wizard {
                step,
                household_skip_allowed: telegram_enabled,
                form: WizardForm::Token {
                    name: submitted_name,
                },
            })
            .error(WIZARD_STALE_STEP_ERROR),
            StatusCode::BAD_REQUEST,
        ),
        // `Validation` приходит и при нескольких active household (выбрать дом
        // за мастера нельзя), и при нуле — дом удалили между шагом и POST.
        // Причину различает число active household; введённое имя сохраняется,
        // как в соседних ветках.
        Err(DomainError::Validation(_)) => {
            let message = match active_household_count(&state).await {
                Ok(0) => WIZARD_TOKEN_NO_HOUSEHOLD_ERROR,
                Ok(1) => WIZARD_TOKEN_CHANGED_ERROR,
                Ok(_) => WIZARD_TOKEN_AMBIGUOUS_ERROR,
                Err(error) => return internal(error),
            };
            render_with_status(
                ConsolePage::new(Page::Wizard {
                    step,
                    household_skip_allowed: telegram_enabled,
                    form: WizardForm::Token {
                        name: submitted_name,
                    },
                })
                .error(message),
                StatusCode::BAD_REQUEST,
            )
        }
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

/// Число активных household; по нему различается причина `Validation` на шаге
/// токена.
async fn active_household_count(state: &AdminState) -> Result<i64, DomainError> {
    let mut conn = state.pool.acquire().await?;
    households::count_active(&mut conn).await
}

/// Текущий шаг мастера по снимку состояния.
async fn current_step(
    state: &AdminState,
    now: chrono::DateTime<Utc>,
) -> Result<WizardStep, DomainError> {
    let telegram = telegram_wizard_state(state).await?;
    let snapshot = wizard::read_state(
        &state.pool,
        state.wizard(),
        state.llm_settings(),
        telegram,
        now,
    )
    .await?;
    Ok(wizard::wizard_step(&snapshot))
}

/// Действующее состояние Telegram для шагов мастера.
///
/// Читается из окружения и базы при каждом запросе: подключение бота в консоли
/// сразу меняет шаги, без перезапуска. Консоль без рантайма (тесты страниц)
/// сохраняет прежнее поведение — шаг Telegram не показывается.
async fn telegram_wizard_state(
    state: &AdminState,
) -> Result<wizard::TelegramWizardState, DomainError> {
    let Some(config) = state.telegram_config() else {
        return Ok(wizard::TelegramWizardState::from_static(
            state.wizard().telegram_enabled,
        ));
    };
    let effective =
        settings::read_effective_telegram(&state.pool, config, state.llm_settings().cipher())
            .await?;
    Ok(wizard::TelegramWizardState {
        configured: effective.is_configured(),
        enabled: effective.is_enabled(),
    })
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
        // Отсутствие `enabled_present` — неполный POST, а не снятый флажок:
        // браузерная форма присылает служебное поле всегда.
        if form.enabled_present.is_some() {
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
    } else if let Some(token) = form.token.map(str::trim).filter(|token| !token.is_empty()) {
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
    let settings = match state.llm_settings().read_settings().await {
        Ok(settings) => settings,
        Err(error) => return internal(error),
    };
    let checks = match llm_checks(state).await {
        Ok(checks) => checks,
        Err(error) => return internal(error),
    };
    render(
        ConsolePage::new(Page::Llm(Box::new(llm_view(
            &settings,
            Some(form),
            false,
            false,
            checks,
        ))))
        .error(&message),
    )
}

/// Страница настроек с сообщением об отказе проверки.
async fn llm_page_with_message(state: &AdminState, message: &str, status: StatusCode) -> Response {
    let settings = match state.llm_settings().read_settings().await {
        Ok(settings) => settings,
        Err(error) => return internal(error),
    };
    let checks = match llm_checks(state).await {
        Ok(checks) => checks,
        Err(error) => return internal(error),
    };
    render_with_status(
        ConsolePage::new(Page::Llm(Box::new(llm_view(
            &settings, None, false, false, checks,
        ))))
        .error(message),
        status,
    )
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
        // У шага Telegram своя страница с формой: повторный отказ открывает её
        // без записи, а не подставляет шаблон без полей.
        WizardStep::Telegram => redirect(wizard::WIZARD_PATH),
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
        WizardStep::Setup | WizardStep::Telegram | WizardStep::Llm | WizardStep::Done => {
            WizardForm::None
        }
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
pub(crate) fn internal(error: impl std::fmt::Display) -> Response {
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

/// Отрисовать список household в общем макете.
pub(crate) fn render_household_list(view: super::households::HouseholdList) -> Response {
    render(ConsolePage::new(Page::Households(Box::new(view))))
}

/// Отрисовать карточку household в общем макете.
pub(crate) fn render_household_detail(view: super::households::HouseholdView) -> Response {
    render(ConsolePage::new(Page::Household(Box::new(view))))
}

/// Отрисовать список household с сообщением отказа формы.
pub(crate) fn render_household_list_error(
    view: super::households::HouseholdList,
    message: &str,
    status: StatusCode,
) -> Response {
    render_with_status(
        ConsolePage::new(Page::Households(Box::new(view))).error(message),
        status,
    )
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
    /// Кнопки проверки настроенных слотов.
    checks: LlmChecks,
}

/// Кнопки проверки по слотам.
struct LlmChecks {
    /// Основной слот; `None` — слот не настроен.
    primary: Option<LlmCheck>,
    /// Резервный слот; `None` — слот не настроен.
    fallback: Option<LlmCheck>,
}

impl LlmChecks {
    /// Есть ли что проверять.
    fn any(&self) -> bool {
        self.primary.is_some() || self.fallback.is_some()
    }

    /// Кнопки в порядке слотов.
    fn iter(&self) -> impl Iterator<Item = &LlmCheck> {
        [self.primary.as_ref(), self.fallback.as_ref()]
            .into_iter()
            .flatten()
    }
}

/// Кнопка проверки одного слота.
struct LlmCheck {
    /// Слот.
    slot: CheckSlot,
    /// Название слота для подписи.
    legend: &'static str,
    /// Число фраз встроенного набора.
    phrases: usize,
    /// Действующий стандарт слота — он выбран в форме по умолчанию.
    titles: TitleMatch,
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
    checks: LlmChecks,
) -> LlmView {
    LlmView {
        saved,
        secret_rejected,
        enabled: toggle_view(
            settings,
            "llm.enabled",
            form.and_then(|form| {
                form.enabled_present
                    .as_ref()
                    .map(|_| form.enabled.is_some())
            }),
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
        checks,
    }
}

/// Кнопки проверки для настроенных слотов.
///
/// Слот берётся из действующего снимка: проверка пойдёт с той же
/// конфигурацией, что у разбора. Стандарт названий читается из `settings`:
/// как и у `capture-eval` без флага, он служит значением по умолчанию.
async fn llm_checks(state: &AdminState) -> Result<LlmChecks, sqlx::Error> {
    let snapshot = state.llm_settings().handle().load();
    let effective = snapshot.effective();
    let phrases = slot_check::builtin_phrase_count();
    let mut conn = state.pool.acquire().await?;

    let primary = llm_check(&mut conn, effective, CheckSlot::Primary, phrases).await?;
    let fallback = llm_check(&mut conn, effective, CheckSlot::Fallback, phrases).await?;
    Ok(LlmChecks { primary, fallback })
}

async fn llm_check(
    conn: &mut sqlx::PgConnection,
    effective: &crate::settings::EffectiveLlm,
    slot: CheckSlot,
    phrases: usize,
) -> Result<Option<LlmCheck>, sqlx::Error> {
    if slot.config(effective).is_none() {
        return Ok(None);
    }
    let stored = llm_slot_settings::titles(conn, slot.name()).await?;
    let titles = TitleMatch::from_code(&stored).unwrap_or(TitleMatch::Strict);
    Ok(Some(LlmCheck {
        slot,
        legend: match slot {
            CheckSlot::Primary => "Основной слот",
            CheckSlot::Fallback => "Резервный слот",
        },
        phrases,
        titles,
    }))
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

/// Состояние хода проверки для шаблона.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CheckPhase {
    /// Проверка ещё не запускалась.
    Idle,
    /// Проверка идёт.
    Running,
    /// Отчёт готов.
    Report,
    /// Проверка не выполнена.
    Failed,
}

/// Вид хода проверки для шаблона.
struct CheckView {
    /// Состояние хода: отчёт и ошибка не различаются по пустоте строки.
    phase: CheckPhase,
    /// Имя слота.
    slot: &'static str,
    /// Сколько фраз пройдено.
    done: usize,
    /// Сколько фраз в наборе.
    total: usize,
    /// Сколько фраз пройдено в готовом отчёте.
    passed: usize,
    /// Отчёт с причинами и оценкой задержки.
    report: String,
    /// Результат сохранён.
    saved: bool,
    /// Причина, по которой проверка не выполнена.
    error: String,
}

/// Привести снимок проверки к плоскому виду для шаблона.
fn check_view(snapshot: Option<&CheckSnapshot>) -> CheckView {
    let Some(snapshot) = snapshot else {
        return CheckView {
            phase: CheckPhase::Idle,
            slot: "",
            done: 0,
            total: 0,
            passed: 0,
            report: String::new(),
            saved: false,
            error: String::new(),
        };
    };
    let mut view = CheckView {
        phase: CheckPhase::Running,
        slot: snapshot.slot,
        done: snapshot.done,
        total: snapshot.total,
        passed: 0,
        report: String::new(),
        saved: false,
        error: String::new(),
    };
    match &snapshot.outcome {
        None => {}
        Some(CheckOutcome::Report {
            report,
            passed,
            total,
            saved,
        }) => {
            view.phase = CheckPhase::Report;
            view.passed = *passed;
            view.total = *total;
            view.report = report.clone();
            view.saved = *saved;
        }
        Some(CheckOutcome::Failed { message }) => {
            view.phase = CheckPhase::Failed;
            view.error = message.clone();
        }
    }
    view
}

/// Слово «фраза» в нужной форме: 22 фразы, 21 фраза, 25 фраз.
fn phrase_word(count: usize) -> &'static str {
    let last_two = count % 100;
    let last = count % 10;
    if (11..=14).contains(&last_two) {
        "фраз"
    } else {
        match last {
            1 => "фраза",
            2..=4 => "фразы",
            _ => "фраз",
        }
    }
}

/// Данные страницы; тип страницы задаёт и заголовок, и навигацию.
enum Page {
    /// Главная за сессией.
    Home(Box<HomeView>),
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
    /// Подключение Telegram.
    Telegram(Box<TelegramView>),
    /// Выпущенная ссылка-claim владельца.
    OwnerLinkIssued(Box<OwnerLinkView>),
    /// Выпущенная ссылка привязки Telegram.
    TelegramLinkIssued(Box<TelegramLinkView>),
    /// Ход проверки слота.
    Check(Option<CheckSnapshot>),
    /// Список household.
    Households(Box<super::households::HouseholdList>),
    /// Карточка household.
    Household(Box<super::households::HouseholdView>),
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
            Self::Home(_) => "Tasklerq",
            Self::Setup => "Первый вход",
            Self::Login => "Вход",
            Self::Recovery { .. } => "Восстановление доступа",
            Self::Password { .. } => "Смена пароля",
            Self::InvalidLink => INVALID_LINK_TITLE,
            Self::Wizard { step, .. } => step.title(),
            Self::TokenIssued { .. } => "Токен Home Assistant",
            Self::Llm(_) => "Разбор записей",
            Self::Telegram(_) => "Telegram",
            Self::OwnerLinkIssued(_) => "Ссылка-claim владельца",
            Self::TelegramLinkIssued(_) => "Ссылка привязки Telegram",
            Self::Check(_) => "Проверка слота",
            Self::Households(_) | Self::Household(_) => "Household",
        }
    }

    /// Навигация вошедшего администратора.
    ///
    /// Определяется типом страницы: страницы за сессией показывают её всегда,
    /// страницы входа и восстановления — никогда.
    fn nav(&self) -> bool {
        matches!(
            self,
            Self::Home(_)
                | Self::Password { .. }
                | Self::Wizard { .. }
                | Self::TokenIssued { .. }
                | Self::Llm(_)
                | Self::Telegram(_)
                | Self::OwnerLinkIssued(_)
                | Self::TelegramLinkIssued(_)
                | Self::Check(_)
                | Self::Households(_)
                | Self::Household(_)
        )
    }

    /// Раздел навигации, который подсвечивается на странице; пусто — ни один.
    fn section(&self) -> &'static str {
        match self {
            Self::Home(_) | Self::OwnerLinkIssued(_) | Self::TelegramLinkIssued(_) => "home",
            Self::Households(_) | Self::Household(_) => "household",
            Self::Llm(_) | Self::Check(_) => "llm",
            Self::Telegram(view) if !view.wizard => "telegram",
            Self::Password { .. } => "password",
            _ => "",
        }
    }

    /// Отрисовать содержимое страницы без макета.
    fn render_body(&self) -> Result<String, askama::Error> {
        match self {
            Self::Home(view) => HomeBody { view }.render(),
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
            Self::Telegram(view) => {
                let skip = if view.wizard {
                    SkipBody {
                        skip: Some(WizardStep::Telegram.slug().unwrap_or_default()),
                    }
                    .render()?
                } else {
                    String::new()
                };
                TelegramBody { view, skip }.render()
            }
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
                let mut checks = String::new();
                for check in view.checks.iter() {
                    checks.push_str(
                        &CheckButtonBody {
                            legend: check.legend,
                            slot: check.slot.slug(),
                            phrases: check.phrases,
                            phrase_word: phrase_word(check.phrases),
                            strict: check.titles == TitleMatch::Strict,
                            normalized: check.titles == TitleMatch::Normalized,
                        }
                        .render()?,
                    );
                }
                if !view.checks.any() {
                    checks.push_str(
                        "<div class=\"panel-body\"><p class=\"muted\">Настройте слот, чтобы проверить его.</p></div>",
                    );
                }
                LlmBody {
                    saved: view.saved,
                    secret_rejected: view.secret_rejected,
                    enabled: &view.enabled,
                    connect_timeout: &view.connect_timeout,
                    daily_quota: &view.daily_quota,
                    primary,
                    fallback,
                    checks,
                }
                .render()
            }
            Self::OwnerLinkIssued(view) => OwnerLinkBody { view }.render(),
            Self::TelegramLinkIssued(view) => TelegramLinkBody { view }.render(),
            Self::Check(snapshot) => {
                let view = check_view(snapshot.as_ref());
                let check = CheckBody { view: &view }.render()?;
                CheckPageBody { check }.render()
            }
            Self::Households(view) => super::households::render_list_body(view),
            Self::Household(view) => view.render_body(),
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
            section: self.page.section(),
            error: self.error,
            content: self.page.render_body()?,
        }
        .render()
    }
}

/// Общий макет консоли.
///
/// Страницы за сессией — шапка с навигацией и колонка содержимого; страницы
/// входа и восстановления — одна панель по центру без навигации.
#[derive(Template)]
#[template(
    source = r#"<!doctype html>
<html lang="ru">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="color-scheme" content="dark">
<title>{{ title }}</title>
<link rel="stylesheet" href="{{ css }}">
<script src="{{ htmx }}" defer></script>
</head>
{% if nav %}
<body class="console-app">
<header class="console-header">
<div class="console-header-inner">
<a class="console-brand" href="/admin"><span class="console-mark" aria-hidden="true">T</span>Tasklerq</a>
<nav class="console-nav" aria-label="Разделы">
<a href="/admin"{% if section == "home" %} aria-current="page"{% endif %}>Главная</a>
<a href="/admin/households"{% if section == "household" %} aria-current="page"{% endif %}>Household</a>
<a href="/admin/llm"{% if section == "llm" %} aria-current="page"{% endif %}>Разбор</a>
<a href="/admin/telegram"{% if section == "telegram" %} aria-current="page"{% endif %}>Telegram</a>
</nav>
<div class="console-account">
<a href="/admin/password"{% if section == "password" %} aria-current="page"{% endif %}>Сменить пароль</a>
<form method="post" action="/admin/logout">
<button type="submit" class="btn-ghost">Выйти</button>
</form>
</div>
</div>
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
{% else %}
<body class="console-auth">
<main class="console-auth-main">
<div class="console-auth-brand"><span class="console-brand"><span class="console-mark" aria-hidden="true">T</span>Tasklerq</span></div>
{% match error %}
{% when Some with (message) %}
<p role="alert" class="console-alert">{{ message }}</p>
{% when None %}
{% endmatch %}
<div class="auth-panel">
{{ content|safe }}
</div>
</main>
</body>
{% endif %}
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
    /// Раздел навигации, к которому относится страница; пусто — ни к какому.
    section: &'static str,
    /// Сообщение формы или причина отказа.
    error: Option<&'a str>,
    /// Уже отрисованный askama фрагмент страницы; экранирован шаблоном
    /// страницы, поэтому в макет вставляется без повторного экранирования.
    content: String,
}

/// Содержимое главной.
#[derive(Template)]
#[template(
    source = r#"<header class="page-head">
<h1>Tasklerq</h1>
<p class="page-desc">Консоль администратора.</p>
</header>
{% if view.setup_incomplete %}
<div class="console-notice" role="status"><span>Настройка ещё не завершена.</span> <a class="btn btn-primary" href="/admin/wizard">Продолжить настройку</a></div>
{% endif %}
<section class="panel">
<header class="panel-head"><h2>Состояние инсталляции</h2></header>
<dl class="console-state">
<dt>Версия</dt><dd class="mono">{{ view.version }}</dd>
<dt>Готовность</dt><dd>{% if view.ready %}<span class="status status-ok">готов</span>{% else %}<span class="status status-bad">не готов</span>{% endif %}{% match view.readiness_problem %}{% when Some with (reason) %} <span class="muted">— {{ reason }}</span>{% when None %}{% endmatch %}</dd>
<dt>Режим размещения</dt><dd>{{ view.tenancy }}</dd>
<dt>Household</dt><dd>{{ view.households }}</dd>
<dt>Telegram</dt><dd>{% if view.telegram_enabled %}<span class="status status-ok">включён</span>{% else %}<span class="status">выключен</span>{% endif %}</dd>
</dl>
</section>
{% if view.has_warnings %}
<section class="panel">
<header class="panel-head"><h2>Предупреждения readiness</h2></header>
<ul class="list list-warnings">
{% for warning in view.warnings %}
<li><code>{{ warning.code }}</code> — {{ warning.label }}</li>
{% endfor %}
</ul>
</section>
{% endif %}
{% if !view.owner_targets.is_empty() %}
<section class="panel">
<header class="panel-head">
<h2>Владелец</h2>
{% if view.owner_telegram_ready %}<p>Выпустите одноразовую ссылку для того, кто станет владельцем.</p>{% endif %}
</header>
{% if view.owner_telegram_ready %}
<ul class="list">
{% for target in view.owner_targets %}
<li><span class="row-label">{{ target.label }}</span>
<form method="post" action="/admin/owner">
<input type="hidden" name="target" value="{{ target.value }}">
<button type="submit">Выпустить ссылку</button>
</form>
</li>
{% endfor %}
</ul>
{% else %}
<div class="panel-body"><p class="console-notice">Сначала подключите бота на странице <a href="/admin/telegram">Telegram</a>.</p></div>
{% endif %}
</section>
{% endif %}
<section class="panel">
<header class="panel-head"><h2>Telegram администратора</h2></header>
{% match view.telegram.linked %}
{% when Some with (name) %}
<div class="row">
<div class="row-text"><p class="row-desc">Привязанный аккаунт: <strong>{{ name }}</strong></p></div>
</div>
{% when None %}
{% match view.telegram.candidate %}
{% when Some with (candidate) %}
<div class="row">
<div class="row-text"><p class="row-desc">Кандидат на привязку: <strong>{{ candidate.name }}</strong></p></div>
<div class="row-actions">
<form method="post" action="/admin/telegram-link/reject">
<input type="hidden" name="candidate" value="{{ candidate.id }}">
<button type="submit" class="btn-ghost">Отклонить</button>
</form>
<form method="post" action="/admin/telegram-link/confirm">
<input type="hidden" name="candidate" value="{{ candidate.id }}">
<button type="submit" class="btn-primary">Подтвердить</button>
</form>
</div>
</div>
{% when None %}
{% if view.telegram.ready %}
<div class="row">
<div class="row-text"><p class="row-desc">Выпустите одноразовую ссылку и откройте её в Telegram.</p></div>
<form method="post" action="/admin/telegram-link">
<button type="submit">Создать ссылку привязки</button>
</form>
</div>
{% else %}
<div class="panel-body"><p class="console-notice">Сначала подключите бота на странице <a href="/admin/telegram">Telegram</a>.</p></div>
{% endif %}
{% endmatch %}
{% endmatch %}
</section>
<section class="panel">
<header class="panel-head"><h2>Слоты LLM</h2></header>
<ul class="list">
{% for slot in view.slots %}
<li><span>{{ slot.label }}: {{ slot.status }}{% if slot.check_link %} — <a href="/admin/llm">Проверить</a>{% endif %}</span></li>
{% endfor %}
</ul>
</section>
{% match view.telegram.linked %}
{% when Some with (name) %}
<section class="panel danger-zone">
<header class="panel-head"><h2>Опасная зона</h2></header>
<div class="row">
<div class="row-text">
<span class="row-label">Telegram администратора</span>
<span class="row-desc">{{ name }}</span>
</div>
<form method="post" action="/admin/telegram-link/unlink">
<button type="submit" class="btn-danger">Отвязать</button>
</form>
</div>
</section>
{% when None %}
{% endmatch %}
"#,
    ext = "html"
)]
struct HomeBody<'a> {
    /// Данные состояния инсталляции.
    view: &'a HomeView,
}

/// Содержимое выпущенной ссылки-claim.
#[derive(Template)]
#[template(
    source = r#"<header class="page-head">
<h1>Ссылка-claim владельца</h1>
<p class="page-desc">Передайте ссылку или QR-код тому, кто станет владельцем.</p>
</header>
<section class="panel">
<div class="panel-body">
<a class="link-box" href="{{ view.link }}" rel="noreferrer">{{ view.link }}</a>
<p class="field-hint num">Срок действия: {{ view.ttl_hours }} ч, до {{ view.expires_at }}.</p>
<div class="console-qr">{{ view.svg|safe }}</div>
</div>
</section>
<p class="console-notice">Код сохранён только хешем: при повторном открытии страницы ссылка недоступна. Ссылку можно выпустить заново.</p>
<div class="actions"><a class="btn" href="/admin">На главную</a></div>
"#,
    ext = "html"
)]
struct OwnerLinkBody<'a> {
    /// Ссылка, срок и SVG QR-кода.
    view: &'a OwnerLinkView,
}

/// Содержимое выпущенной ссылки привязки Telegram.
#[derive(Template)]
#[template(
    source = r#"<header class="page-head">
<h1>Ссылка привязки Telegram</h1>
<p class="page-desc">Откройте ссылку в Telegram с того аккаунта, который привязываете, и подтвердите его на главной странице.</p>
</header>
<section class="panel">
<div class="panel-body">
<a class="link-box" href="{{ view.link }}" rel="noreferrer">{{ view.link }}</a>
<p class="field-hint num">Срок действия: {{ view.ttl_minutes }} мин, до {{ view.expires_at }}.</p>
</div>
</section>
<p class="console-notice">Код сохранён только хешем: при повторном открытии страницы ссылка недоступна. Ссылку можно выпустить заново.</p>
<div class="actions"><a class="btn" href="/admin">На главную</a></div>
"#,
    ext = "html"
)]
struct TelegramLinkBody<'a> {
    /// Ссылка и срок.
    view: &'a TelegramLinkView,
}

/// Содержимое первого входа.
#[derive(Template)]
#[template(
    source = r#"<h1>Первый вход</h1>
<form method="post" action="/admin/setup">
<div class="field"><label for="setup-code">Код</label>
<input id="setup-code" class="mono" name="code" type="text" autocomplete="one-time-code" required></div>
<div class="field"><label for="setup-username">Имя</label>
<input id="setup-username" name="username" type="text" autocomplete="username" required></div>
<div class="field"><label for="setup-password">Пароль</label>
<input id="setup-password" name="password" type="password" autocomplete="new-password" minlength="12" maxlength="1024" required></div>
<button type="submit" class="btn-primary btn-block">Создать учётную запись</button>
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
<div class="field"><label for="login-username">Имя</label>
<input id="login-username" name="username" type="text" autocomplete="username" required></div>
<div class="field"><label for="login-password">Пароль</label>
<input id="login-password" name="password" type="password" autocomplete="current-password" required></div>
<button type="submit" class="btn-primary btn-block">Войти</button>
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
<div class="field"><label for="recovery-password">Новый пароль</label>
<input id="recovery-password" name="password" type="password" autocomplete="new-password" minlength="12" maxlength="1024" required></div>
<button type="submit" class="btn-primary btn-block">Сохранить пароль</button>
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
    source = r#"<header class="page-head"><h1>Смена пароля</h1></header>
{% if changed %}
<p role="status" class="console-notice">Пароль изменён.</p>
{% endif %}
<form method="post" action="/admin/password">
<section class="panel">
<div class="panel-body">
<div class="field"><label for="current-password">Текущий пароль</label>
<input id="current-password" name="current_password" type="password" autocomplete="current-password" required></div>
<div class="field"><label for="new-password">Новый пароль</label>
<input id="new-password" name="new_password" type="password" autocomplete="new-password" minlength="12" maxlength="1024" required></div>
</div>
<div class="panel-foot"><button type="submit" class="btn-primary">Сменить пароль</button></div>
</section>
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
<a class="btn btn-block" href="/admin/login">Перейти ко входу</a>
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
<button type="submit" class="btn-ghost">Пропустить</button>
</form>
{% when None %}
<p class="field-hint">Этот шаг нельзя пропустить: Telegram выключен, поэтому household и владельца нужно создать командой администратора.</p>
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
    source = r#"<header class="page-head">
<h1>{{ title }}</h1>
<p class="page-desc">{{ description }}</p>
</header>
<div class="actions">
<a class="btn btn-primary" href="/admin/llm">Настроить разбор</a>
{{ skip|safe }}
</div>
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
    source = r#"<header class="page-head">
<h1>{{ title }}</h1>
<p class="page-desc">{{ description }}</p>
</header>
<form method="post" action="/admin/wizard/household">
<section class="panel">
<div class="panel-body grid-2">
<div class="field"><label for="household-name">Название</label>
<input id="household-name" name="name" type="text" value="{{ name }}" maxlength="100" required></div>
<div class="field"><label for="household-timezone">Часовой пояс</label>
<input id="household-timezone" class="mono" name="timezone" type="text" value="{{ timezone }}" maxlength="64" required></div>
</div>
<div class="panel-foot"><button type="submit" class="btn-primary">Создать дом</button></div>
</section>
</form>
<div class="actions">{{ skip|safe }}</div>
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
    source = r#"<header class="page-head">
<h1>{{ title }}</h1>
<p class="page-desc">{{ description }}</p>
</header>
<form method="post" action="/admin/wizard/token">
<section class="panel">
<div class="panel-body">
<div class="field"><label for="token-name">Название токена</label>
<input id="token-name" name="name" type="text" value="{{ name }}" maxlength="64"></div>
</div>
<div class="panel-foot"><button type="submit" class="btn-primary">Выпустить токен</button></div>
</section>
</form>
<div class="actions">{{ skip|safe }}</div>
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
    source = r#"<header class="page-head"><h1>Токен Home Assistant</h1></header>
<div class="console-warn" role="status"><p>Скопируйте токен и вставьте его в настройки Home Assistant. Токен показывается один раз: после перехода он больше не будет доступен.</p></div>
<section class="panel">
<div class="panel-body">
<input id="issued-token" class="token-value" name="token" type="text" value="{{ token }}" readonly aria-label="Токен Home Assistant">
</div>
<div class="panel-foot"><a class="btn btn-primary" href="/admin/wizard">Продолжить</a></div>
</section>
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
    source = r#"<section class="panel">
<header class="panel-head"><h2>{{ legend }}</h2></header>
<div class="panel-body">
<div class="field">
<div class="field-head"><label for="llm-{{ prefix }}-url">Адрес</label><span class="console-source">источник: {{ slot.url.source }}</span></div>
<input id="llm-{{ prefix }}-url" class="mono" name="{{ prefix }}_url" type="text" value="{{ slot.url.value }}"{% if slot.url.readonly %} readonly{% endif %}>
</div>
<div class="grid-2">
<div class="field">
<div class="field-head"><label for="llm-{{ prefix }}-model">Модель</label><span class="console-source">источник: {{ slot.model.source }}</span></div>
<input id="llm-{{ prefix }}-model" class="mono" name="{{ prefix }}_model" type="text" value="{{ slot.model.value }}"{% if slot.model.readonly %} readonly{% endif %}>
</div>
<div class="field">
<div class="field-head"><label for="llm-{{ prefix }}-timeout">Таймаут запроса, мс</label><span class="console-source">источник: {{ slot.timeout.source }}</span></div>
<input id="llm-{{ prefix }}-timeout" name="{{ prefix }}_timeout_ms" type="text" inputmode="numeric" value="{{ slot.timeout.value }}"{% if slot.timeout.readonly %} readonly{% endif %}>
</div>
</div>
<div class="field">
<div class="field-head"><label for="llm-{{ prefix }}-extra">Дополнительные параметры JSON</label><span class="console-source">источник: {{ slot.extra_json.source }}</span></div>
<input id="llm-{{ prefix }}-extra" class="mono" name="{{ prefix }}_extra_json" type="text" value="{{ slot.extra_json.value }}"{% if slot.extra_json.readonly %} readonly{% endif %}>
</div>
</div>
<div class="row">
<div class="row-text"><span class="row-label">Токен: <strong>{{ slot.token.label }}</strong></span></div>
<span class="console-source">источник: {{ slot.token.source }}</span>
</div>
{% if slot.token.readonly %}
{% else %}
<div class="panel-body panel-body-tight">
<div class="field"><label for="llm-{{ prefix }}-token">Заменить токен (пустое поле — не менять)</label>
<input id="llm-{{ prefix }}-token" name="{{ prefix }}_token" type="password" autocomplete="new-password"></div>
<label class="check check-danger"><input name="{{ prefix }}_token_delete" type="checkbox" value="1"> Удалить токен</label>
</div>
{% endif %}
</section>
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
    source = r#"<header class="page-head"><h1>Разбор записей</h1></header>
{% if saved %}
<p role="status" class="console-notice">Настройки LLM сохранены.</p>
{% endif %}
{% if secret_rejected %}
<p role="alert" class="console-alert">Токен не сохранён: не задан ключ шифрования TASKLERQ_SECRET_KEY.{% if saved %} Остальные поля сохранены.{% endif %}</p>
{% endif %}
<form method="post" action="/admin/llm">
<input type="hidden" name="enabled_present" value="1">
<section class="panel">
<header class="panel-head"><h2>Разбор</h2></header>
<div class="row">
<div class="row-text">
<label for="llm-enabled" class="row-label">Включить разбор</label>
<span><span class="console-source">источник: {{ enabled.source }}</span></span>
</div>
<input id="llm-enabled" class="switch" name="enabled" type="checkbox" value="1"{% if enabled.checked %} checked{% endif %}{% if enabled.readonly %} disabled{% endif %}>
</div>
</section>
{{ primary|safe }}
{{ fallback|safe }}
<section class="panel">
<header class="panel-head"><h2>Общие</h2></header>
<div class="panel-body grid-2">
<div class="field">
<div class="field-head"><label for="llm-connect-timeout">Таймаут соединения, мс</label><span class="console-source">источник: {{ connect_timeout.source }}</span></div>
<input id="llm-connect-timeout" name="connect_timeout_ms" type="text" inputmode="numeric" value="{{ connect_timeout.value }}"{% if connect_timeout.readonly %} readonly{% endif %}>
</div>
<div class="field">
<div class="field-head"><label for="llm-daily-quota">Суточная квота</label><span class="console-source">источник: {{ daily_quota.source }}</span></div>
<input id="llm-daily-quota" name="daily_quota" type="text" inputmode="numeric" value="{{ daily_quota.value }}"{% if daily_quota.readonly %} readonly{% endif %}>
</div>
</div>
</section>
<div class="form-actions"><button type="submit" class="btn-primary">Сохранить</button></div>
</form>
<section class="panel">
<header class="panel-head"><h2>Проверка слотов</h2></header>
{{ checks|safe }}
</section>
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
    /// Уже отрисованные кнопки проверки.
    checks: String,
}

/// Содержимое страницы Telegram.
#[derive(Template)]
#[template(
    source = r#"<header class="page-head"><h1>Telegram</h1></header>
{% if view.saved %}
<p role="status" class="console-notice">Настройки Telegram сохранены.</p>
{% endif %}
<section class="panel">
<header class="panel-head"><h2>Состояние</h2></header>
<dl class="console-state">
<dt>Бот</dt><dd>{% if view.bot_name.is_empty() %}<span class="muted">не подключён</span>{% else %}{{ view.bot_name }}{% endif %}{% match view.bot_id %}{% when Some with (id) %} <span class="muted num">({{ id }})</span>{% when None %}{% endmatch %}</dd>
<dt>Источник группы</dt><dd>{{ view.source }}</dd>
<dt>Режим</dt><dd>{{ view.mode }}</dd>
<dt>Адрес webhook</dt><dd>{% if view.webhook_address.is_empty() %}<span class="muted">не задан</span>{% else %}<span class="mono">{{ view.webhook_address }}</span>{% endif %}</dd>
<dt>Токен</dt><dd>{{ view.token_label }} <span class="console-source">источник: {{ view.token_source }}</span></dd>
<dt>Секрет webhook</dt><dd>{% if view.webhook_secret_set %}задан{% else %}<span class="muted">не задан</span>{% endif %}</dd>
{% if view.webhook_info_failed %}
<dt>Состояние webhook</dt><dd><span class="status status-bad">не удалось получить</span></dd>
{% else %}
<dt>Ожидают обработки</dt><dd>{% match view.pending_update_count %}{% when Some with (count) %}{{ count }}{% when None %}{% endmatch %}</dd>
{% match view.webhook_error_message %}
{% when Some with (message) %}
<dt>Последняя ошибка</dt><dd><span class="text-danger">{{ message }}</span>{% match view.webhook_error_date %}{% when Some with (date) %} <span class="muted num">({{ date }})</span>{% when None %}{% endmatch %}</dd>
{% when None %}
{% endmatch %}
{% endif %}
</dl>
</section>
{% if view.readonly %}
<p class="console-notice">Токен задан окружением TELEGRAM_BOT_TOKEN: страница доступна только для чтения.</p>
{% else %}
{% match view.bot_change %}
{% when Some with (change) %}
<div class="console-warn" role="alert">
<p><strong>Подтвердить смену бота</strong></p>
<p>Сейчас подключён бот {{ change.previous_name }}, вводится бот {{ change.new_name }}.</p>
<p>Участников в инсталляции: {{ change.participants }}. Новый бот не сможет писать участникам, пока они не отправят ему /start.</p>
</div>
<form method="post" action="/admin/telegram">
{% if view.wizard %}<input type="hidden" name="next" value="wizard">{% endif %}
<input type="hidden" name="confirm_bot_change" value="1">
<input type="hidden" name="confirm_bot_id" value="{{ change.new_id }}">
<section class="panel">
<div class="row">
<div class="row-text"><span class="row-label" id="telegram-confirm-mode">Режим</span></div>
<div class="segmented" role="radiogroup" aria-labelledby="telegram-confirm-mode">
<label><input name="mode" type="radio" value="webhook"{% if view.webhook_selected %} checked{% endif %}><span>webhook</span></label>
{% if view.polling_allowed %}
<label><input name="mode" type="radio" value="polling"{% if view.polling_selected %} checked{% endif %}><span>polling</span></label>
{% endif %}
<label><input name="mode" type="radio" value="disabled"{% if view.disabled_selected %} checked{% endif %}><span>выключен</span></label>
</div>
</div>
</section>
<section class="panel">
<header class="panel-head"><h2>Токен</h2><p>Введите токен нового бота ещё раз: страница не сохраняет его в форме.</p></header>
<div class="panel-body">
<div class="field"><label for="telegram-confirm-token">Токен нового бота</label>
<input id="telegram-confirm-token" class="mono" name="token" type="password" autocomplete="new-password"></div>
</div>
</section>
<section class="panel">
<header class="panel-head"><h2>Адрес</h2></header>
<div class="panel-body">
<div class="field">
<div class="field-head"><label for="telegram-confirm-public-base-url">Публичный адрес</label><span class="console-source">источник: {{ view.public_base_url.source }}</span></div>
<input id="telegram-confirm-public-base-url" class="mono" name="public_base_url" type="text" value="{{ view.public_base_url.value }}"{% if view.public_base_url.readonly %} readonly{% endif %}>
</div>
<div class="field">
<div class="field-head"><label for="telegram-confirm-webhook-override">Переопределение адреса webhook (пусто — публичный адрес)</label><span class="console-source">источник: {{ view.webhook_url_override.source }}</span></div>
<input id="telegram-confirm-webhook-override" class="mono" name="webhook_url_override" type="text" value="{{ view.webhook_url_override.value }}"{% if view.webhook_url_override.readonly %} readonly{% endif %}>
</div>
</div>
</section>
<div class="form-actions"><button type="submit" class="btn-primary">Подтвердить смену бота</button></div>
</form>
{% when None %}
<form method="post" action="/admin/telegram">
{% if view.wizard %}<input type="hidden" name="next" value="wizard">{% endif %}
<section class="panel">
<div class="row">
<div class="row-text"><span class="row-label" id="telegram-mode">Режим</span></div>
<div class="segmented" role="radiogroup" aria-labelledby="telegram-mode">
<label><input name="mode" type="radio" value="webhook"{% if view.webhook_selected %} checked{% endif %}><span>webhook</span></label>
{% if view.polling_allowed %}
<label><input name="mode" type="radio" value="polling"{% if view.polling_selected %} checked{% endif %}><span>polling</span></label>
{% endif %}
<label><input name="mode" type="radio" value="disabled"{% if view.disabled_selected %} checked{% endif %}><span>выключен</span></label>
</div>
</div>
</section>
<section class="panel">
<header class="panel-head"><h2>Токен</h2></header>
<div class="row">
<div class="row-text"><span class="row-label">Токен: <strong>{{ view.token_label }}</strong></span></div>
</div>
<div class="panel-body panel-body-tight">
<div class="field"><label for="telegram-token">Новый токен (пустое поле — не менять)</label>
<input id="telegram-token" class="mono" name="token" type="password" autocomplete="new-password"></div>
</div>
</section>
<section class="panel">
<header class="panel-head"><h2>Адрес</h2></header>
<div class="panel-body">
<div class="field">
<div class="field-head"><label for="telegram-public-base-url">Публичный адрес</label><span class="console-source">источник: {{ view.public_base_url.source }}</span></div>
<input id="telegram-public-base-url" class="mono" name="public_base_url" type="text" value="{{ view.public_base_url.value }}"{% if view.public_base_url.readonly %} readonly{% endif %}>
</div>
<div class="field">
<div class="field-head"><label for="telegram-webhook-override">Переопределение адреса webhook (пусто — публичный адрес)</label><span class="console-source">источник: {{ view.webhook_url_override.source }}</span></div>
<input id="telegram-webhook-override" class="mono" name="webhook_url_override" type="text" value="{{ view.webhook_url_override.value }}"{% if view.webhook_url_override.readonly %} readonly{% endif %}>
</div>
</div>
</section>
<div class="form-actions"><button type="submit" class="btn-primary">Сохранить</button></div>
</form>
{% if view.reissue_allowed %}
<section class="panel danger-zone">
<header class="panel-head"><h2>Опасная зона</h2></header>
<div class="row">
<div class="row-text"><span class="row-label">Секрет webhook</span></div>
<form method="post" action="/admin/telegram/secret">
<button type="submit" class="btn-danger">Перевыпустить секрет webhook</button>
</form>
</div>
</section>
{% endif %}
{% endmatch %}
{% endif %}
{% if !skip.is_empty() %}<div class="actions">{{ skip|safe }}</div>{% endif %}
"#,
    ext = "html"
)]
struct TelegramBody<'a> {
    /// Данные страницы.
    view: &'a TelegramView,
    /// Уже отрисованная форма пропуска шага мастера.
    skip: String,
}

/// Форма проверки одного слота.
#[derive(Template)]
#[template(
    source = r#"<div class="row console-check">
<div class="row-text"><span class="row-label">{{ legend }}</span></div>
<form method="post" action="/admin/llm/check/{{ slot }}" class="row-actions">
<label for="llm-check-{{ slot }}-titles" class="field-hint">Стандарт названий</label>
<select id="llm-check-{{ slot }}-titles" name="titles">
<option value="strict"{% if strict %} selected{% endif %}>strict</option>
<option value="normalized"{% if normalized %} selected{% endif %}>normalized</option>
</select>
<button type="submit">Проверить — {{ phrases }} {{ phrase_word }}</button>
</form>
</div>
"#,
    ext = "html"
)]
struct CheckButtonBody {
    /// Название слота.
    legend: &'static str,
    /// Слот в адресе формы.
    slot: &'static str,
    /// Число фраз набора.
    phrases: usize,
    /// Слово «фраза» в нужной форме.
    phrase_word: &'static str,
    /// Выбран строгий стандарт.
    strict: bool,
    /// Выбран нормализованный стандарт.
    normalized: bool,
}

/// Содержимое страницы хода проверки.
///
/// Пока прогон идёт, ход опрашивает себя через htmx; без JavaScript видна
/// `<noscript>`-ссылка «Обновить». Заголовок `Refresh` не ставится: он
/// перезагружал бы страницу у браузера с JavaScript параллельно с опросом.
#[derive(Template)]
#[template(
    source = r#"<header class="page-head"><h1>Проверка слота</h1></header>
{{ check|safe }}
"#,
    ext = "html"
)]
struct CheckPageBody {
    /// Уже отрисованный ход.
    check: String,
}

/// Ход проверки; он же содержимое страницы и фрагмент для htmx.
#[derive(Template)]
#[template(
    source = r#"{% match view.phase %}
{% when CheckPhase::Idle %}
<div id="llm-check" class="panel">
<div class="panel-body">
<p class="muted">Проверка ещё не запускалась.</p>
<div class="actions"><a class="btn" href="/admin/llm">Вернуться к настройкам</a></div>
</div>
</div>
{% when CheckPhase::Running %}
<div id="llm-check" class="panel" hx-get="/admin/llm/check-fragment" hx-trigger="every 2s" hx-swap="outerHTML">
<div class="panel-body">
<p role="status" class="num">Проверка слота {{ view.slot }} идёт: пройдено {{ view.done }} из {{ view.total }} фраз.</p>
<progress value="{{ view.done }}" max="{{ view.total }}"></progress>
<noscript><p><a href="/admin/llm/check">Обновить</a></p></noscript>
</div>
</div>
{% when CheckPhase::Report %}
<div id="llm-check" class="panel">
<div class="panel-body">
<p role="status" class="console-notice num">Проверка слота {{ view.slot }} завершена: пройдено {{ view.passed }} из {{ view.total }}.</p>
{% if !view.saved %}<p role="alert" class="console-alert">Результат проверки не сохранён.</p>{% endif %}
<pre>{{ view.report }}</pre>
</div>
<div class="panel-foot"><a class="btn btn-ghost" href="/admin/llm/check">Обновить</a><a class="btn" href="/admin/llm">Вернуться к настройкам</a></div>
</div>
{% when CheckPhase::Failed %}
<div id="llm-check" class="panel">
<div class="panel-body">
<p role="alert" class="console-alert">{{ view.error }}</p>
<div class="actions"><a class="btn" href="/admin/llm">Вернуться к настройкам</a></div>
</div>
</div>
{% endmatch %}
"#,
    ext = "html"
)]
struct CheckBody<'a> {
    /// Ход проверки.
    view: &'a CheckView,
}

#[cfg(test)]
mod tests {
    use super::{
        ConsolePage, HomeSlot, HomeTelegram, HomeView, HomeWarning, HouseholdForm, LlmCheck,
        LlmChecks, LlmSlotView, LlmTextField, LlmToggle, LlmTokenView, LlmView, Page, WizardForm,
        household_form_error, phrase_word, target_or_home, warning_label,
    };
    use crate::admin_console::checks::{CheckOutcome, CheckSlot, CheckSnapshot};
    use crate::admin_console::wizard::WizardStep;
    use crate::capture::eval::TitleMatch;
    use crate::domain::error::DomainError;

    fn home_view_for_tests(setup_incomplete: bool) -> HomeView {
        HomeView {
            version: "0.0.0-test",
            setup_incomplete,
            ready: true,
            readiness_problem: None,
            warnings: vec![HomeWarning {
                code: "llm_slot_unverified:primary".to_owned(),
                label: "слот LLM primary не проверен".to_owned(),
            }],
            has_warnings: true,
            tenancy: "single",
            households: 1,
            telegram_enabled: false,
            owner_targets: Vec::new(),
            owner_telegram_ready: false,
            telegram: HomeTelegram {
                ready: false,
                linked: None,
                candidate: None,
            },
            slots: vec![HomeSlot {
                label: "Основной слот",
                status: "не проверен".to_owned(),
                check_link: true,
            }],
        }
    }

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
            checks: LlmChecks {
                primary: None,
                fallback: None,
            },
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
            Page::Home(Box::new(home_view_for_tests(false))),
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
        let home = ConsolePage::new(Page::Home(Box::new(home_view_for_tests(false))))
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
    fn the_check_button_shows_the_set_size_and_the_saved_standard() {
        let mut view = llm_view_for_tests("задан");
        view.checks = LlmChecks {
            primary: Some(LlmCheck {
                slot: CheckSlot::Primary,
                legend: "Основной слот",
                phrases: 22,
                titles: TitleMatch::Normalized,
            }),
            fallback: None,
        };
        let html = ConsolePage::new(Page::Llm(Box::new(view)))
            .render()
            .expect("the page renders");

        assert!(
            html.contains("action=\"/admin/llm/check/primary\""),
            "{html}"
        );
        assert!(html.contains("Проверить — 22 фразы"), "{html}");
        assert!(
            html.contains("<option value=\"normalized\" selected>normalized</option>"),
            "the saved standard is the default: {html}"
        );
        assert!(
            !html.contains("action=\"/admin/llm/check/fallback\""),
            "{html}"
        );
    }

    #[test]
    fn the_check_page_polls_until_the_report_and_works_without_javascript() {
        let running = CheckSnapshot {
            slot: "primary",
            done: 3,
            total: 22,
            outcome: None,
        };
        let html = ConsolePage::new(Page::Check(Some(running)))
            .render()
            .expect("the page renders");
        assert!(html.contains("пройдено 3 из 22 фраз"), "{html}");
        assert!(html.contains("hx-trigger=\"every 2s\""), "{html}");
        assert!(
            html.contains("<noscript><p><a href=\"/admin/llm/check\">Обновить</a></p></noscript>"),
            "the no-JavaScript fallback is a link, not a page reload: {html}"
        );
        assert!(
            !html.contains("http-equiv=\"refresh\""),
            "the page must not carry a meta refresh in the body: {html}"
        );

        let finished = CheckSnapshot {
            slot: "primary",
            done: 22,
            total: 22,
            outcome: Some(CheckOutcome::Report {
                report: "Набор capture-v4: пройдено 21 из 22".to_owned(),
                passed: 21,
                total: 22,
                saved: true,
            }),
        };
        let html = ConsolePage::new(Page::Check(Some(finished)))
            .render()
            .expect("the page renders");
        assert!(!html.contains("hx-trigger=\"every 2s\""), "{html}");
        assert!(html.contains("пройдено 21 из 22"), "{html}");
        assert!(html.contains("Набор capture-v4"), "{html}");
    }

    #[test]
    fn every_readiness_warning_has_a_console_label() {
        let health = crate::http::health::WARNING_CODES
            .iter()
            .map(|code| (*code).to_owned())
            .chain([
                format!(
                    "{}:primary",
                    crate::http::health::WARNING_LLM_SLOT_UNVERIFIED
                ),
                format!(
                    "{}:primary:strict",
                    crate::http::health::WARNING_LLM_SLOT_UNVERIFIED
                ),
            ]);

        for code in health {
            let label = warning_label(&code);
            assert_ne!(
                label, "неизвестное предупреждение",
                "readiness warning `{code}` has no console label"
            );
            assert!(!label.is_empty(), "the label for `{code}` is empty");
        }
    }

    #[test]
    fn the_phrase_word_follows_the_number() {
        assert_eq!(phrase_word(1), "фраза");
        assert_eq!(phrase_word(2), "фразы");
        assert_eq!(phrase_word(4), "фразы");
        assert_eq!(phrase_word(5), "фраз");
        assert_eq!(phrase_word(11), "фраз");
        assert_eq!(phrase_word(21), "фраза");
        assert_eq!(phrase_word(22), "фразы");
        assert_eq!(phrase_word(25), "фраз");
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
