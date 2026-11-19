//! Страницы household: список с формой создания и карточка одного.
//!
//! Список берётся из [`admin::list_households`], карточка — из
//! [`admin::household_summary`]: консоль не пишет собственный SQL и не
//! повторяет правила выбора household. В `single` список сразу ведёт на
//! единственный household через [`admin::selected_household`], то есть по тому
//! же правилу, что CLI.
//!
//! Создание — тонкая обёртка над [`admin::create_household`]: форму разбирают
//! те же [`normalize_text`] и [`validate_timezone`], что шаг мастера, а правило
//! «один household в `single`» проверяет `src/admin`/репозиторий. Отказ домена
//! возвращает список с формой и текстом [`admin::describe`], ничего не меняя.
//!
//! На карточке household без владельца предлагается ссылка-claim: форма шлёт
//! `POST /admin/owner` с `target=<id>`, выпуск выполняет [`owner::issue`].
//!
//! Удаление — двухшаговое: `GET /admin/households/{id}/delete` спрашивает
//! название, а POST сверяет его с [`admin::household_name`] и вызывает
//! [`admin::delete_household`]. Своего SQL для удаления консоль не пишет.
//!
//! API-токены карточки — тонкая обёртка над [`admin::list_tokens`] и
//! [`admin::create_token`]: список показывает только префикс и метаданные, а
//! открытое значение выпущенного токена живёт только в теле ответа.

use askama::Template;
use axum::extract::{Form, Path, RawForm, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use chrono::{DateTime, NaiveDate, Utc};
use chrono_tz::Tz;
use serde::Deserialize;
use uuid::Uuid;

use super::owner;
use super::pages::{
    self, HOME_ASSISTANT_SCOPES, HouseholdForm, internal, render_household_delete,
    render_household_delete_error, render_household_detail, render_household_detail_error,
    render_household_list, render_household_token_issued, render_household_token_revoke,
    render_household_token_revoke_error,
};
use super::router::AdminState;
use super::session::AdminSession;
use crate::admin::{self, HouseholdSummary};
use crate::auth::tokens::{IssuedToken, SCOPES};
use crate::config::TenancyMode;
use crate::domain::error::DomainError;
use crate::domain::identity::confirmation_matches;
use crate::domain::models::{User, UserRole};
use crate::domain::recurrence::parse_timezone;
use crate::domain::settings::{DEFAULT_TIMEZONE, MAX_DISPLAY_NAME, validate_timezone};
use crate::domain::validation::normalize_text;
use crate::storage::repositories::tokens::ApiToken;

/// Отказ без подтверждения: пустое поле или его отсутствие.
const DELETE_EMPTY_CONFIRMATION: &str = "Введите название household, чтобы подтвердить удаление.";
/// Отказ при несовпадении названия.
const DELETE_NAME_MISMATCH: &str = "Название не совпадает с названием household.";
/// Отказ формы участника на пустой, нечисловой или неположительный Telegram ID.
const INVALID_TELEGRAM_ID: &str = "telegram id must be a positive integer";
/// Роль по умолчанию в форме участника.
const DEFAULT_MEMBER_ROLE: &str = "member";
/// Подсказка при попытке добавить второго владельца.
const OWNER_EXISTS_HINT: &str = "Владение передаётся в боте.";
/// Отказ формы токена при нечисловом сроке.
const INVALID_TOKEN_EXPIRY: &str = "Срок действия должен быть числом дней.";
/// Отказ отзыва без подтверждения.
const REVOKE_WITHOUT_CONFIRMATION: &str =
    "Подтвердите отзыв токена: интеграции с ним перестанут работать.";

/// Telegram ID для показа: только последние четыре цифры.
///
/// Для ID короче пяти цифр остаётся одно многоточие: раскрывать короткий ID
/// целиком нельзя.
fn mask_telegram_id(telegram_user_id: i64) -> String {
    let digits = telegram_user_id.unsigned_abs().to_string();
    match digits.len().checked_sub(4) {
        Some(start) if start > 0 => format!("…{}", &digits[start..]),
        _ => "…".to_owned(),
    }
}

/// Участник в секции карточки: без полного Telegram ID, chat id и username.
struct MemberView {
    /// Отображаемое имя.
    name: String,
    /// Роль по-русски.
    role: &'static str,
    /// Дата вступления без времени.
    joined_at: NaiveDate,
    /// Последние четыре цифры Telegram ID.
    telegram_id: String,
}

impl MemberView {
    /// Оставить только поля, которые можно показывать на карточке.
    fn new(user: User, timezone: Tz) -> Self {
        Self {
            name: user.display_name,
            role: match user.role {
                UserRole::Owner => "владелец",
                UserRole::Member => "участник",
            },
            joined_at: user.created_at.with_timezone(&timezone).date_naive(),
            telegram_id: mask_telegram_id(user.telegram_user_id),
        }
    }
}

/// Опция scopes формы токена: известные scopes с отметкой выбранных.
struct ScopeOption {
    /// Имя scope из [`SCOPES`].
    scope: &'static str,
    /// Флажок отмечен.
    checked: bool,
}

/// Построить options в порядке [`SCOPES`], отмечая выбранные.
fn scope_options(selected: impl Fn(&str) -> bool) -> Vec<ScopeOption> {
    SCOPES
        .iter()
        .map(|scope| ScopeOption {
            scope,
            checked: selected(scope),
        })
        .collect()
}

/// Строка секции API-токенов: без хеша и открытого значения.
struct TokenRow {
    /// Идентификатор для ссылки отзыва.
    id: Uuid,
    /// Название.
    name: String,
    /// Видимый префикс.
    prefix: String,
    /// Scopes через запятую.
    scopes: String,
    /// Время последнего использования или «не использовался».
    last_used: String,
    /// Состояние на момент запроса.
    state: &'static str,
    /// Срок действия в поясе household, если задан.
    expires_at: Option<String>,
    /// Токен активен: его можно отозвать.
    revocable: bool,
}

/// Токен можно отозвать: он не отозван и не истёк на момент `now`.
fn revocable(token: &ApiToken, now: DateTime<Utc>) -> bool {
    token.revoked_at.is_none() && !token.expires_at.is_some_and(|expires_at| expires_at <= now)
}

impl TokenRow {
    /// Собрать строку из записи `admin`; состояние — на момент `now`.
    fn new(token: ApiToken, timezone: Tz, now: DateTime<Utc>) -> Self {
        let expired = token.expires_at.is_some_and(|expires_at| expires_at <= now);
        let is_revocable = revocable(&token, now);
        let state = if token.revoked_at.is_some() {
            "отозван"
        } else if expired {
            "истёк"
        } else {
            "активен"
        };
        Self {
            id: token.id,
            name: token.name,
            prefix: token.prefix,
            scopes: token.scopes.join(", "),
            last_used: token.last_used_at.map_or_else(
                || "не использовался".to_owned(),
                |at| format_at(at, timezone),
            ),
            state,
            expires_at: token.expires_at.map(|at| format_at(at, timezone)),
            revocable: is_revocable,
        }
    }
}

/// Дата и время карточки в поясе household.
fn format_at(at: DateTime<Utc>, timezone: Tz) -> String {
    at.with_timezone(&timezone)
        .format("%Y-%m-%d %H:%M")
        .to_string()
}

/// Поля формы токена из сырого `application/x-www-form-urlencoded`.
///
/// Scopes приходят повторяющимися полями `scope`, поэтому `Form<T>` с
/// `serde_urlencoded` их не разберёт; новых зависимостей разбор не требует.
struct TokenFormInput {
    /// Название токена.
    name: String,
    /// Отмеченные scopes.
    scopes: Vec<String>,
    /// Срок в днях как введён: пусто — без срока.
    expires_in_days: String,
}

impl TokenFormInput {
    /// Разобрать тело формы; неизвестные поля игнорируются.
    fn parse(body: &[u8]) -> Self {
        let mut name = String::new();
        let mut scopes = Vec::new();
        let mut expires_in_days = String::new();
        for (key, value) in url::form_urlencoded::parse(body) {
            match key.as_ref() {
                "name" => name = value.into_owned(),
                "scope" => scopes.push(value.into_owned()),
                "expires_in_days" => expires_in_days = value.into_owned(),
                _ => {}
            }
        }
        Self {
            name,
            scopes,
            expires_in_days,
        }
    }
}

/// Срок из формы: пусто — без срока, нечисловое — отказ формы.
fn parse_expiry_days(raw: &str) -> Result<Option<i64>, &'static str> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    raw.parse().map(Some).map_err(|_| INVALID_TOKEN_EXPIRY)
}

/// Строка списка household: без участников, готовности Telegram и формы.
pub(crate) struct HouseholdRow {
    /// Идентификатор для ссылки на карточку.
    id: Uuid,
    /// Название.
    name: String,
    /// Часовой пояс IANA.
    timezone: String,
    /// Есть ли активный владелец.
    has_owner: bool,
    /// Число активных участников.
    members: i64,
    /// Число открытых задач.
    open_tasks: i64,
    /// Число активных токенов.
    active_tokens: i64,
}

impl HouseholdRow {
    /// Собрать строку из сводки `admin`.
    fn new(household: HouseholdSummary) -> Self {
        Self {
            id: household.id,
            name: household.name,
            timezone: household.timezone,
            has_owner: household.owner_telegram_id.is_some(),
            members: household.members,
            open_tasks: household.open_tasks,
            active_tokens: household.active_tokens,
        }
    }
}

/// Данные карточки household; без Telegram ID владельца и содержимого задач.
pub(crate) struct HouseholdView {
    /// Идентификатор для ссылки на карточку и форм.
    id: Uuid,
    /// Название.
    name: String,
    /// Часовой пояс IANA.
    timezone: String,
    /// Есть ли активный владелец.
    has_owner: bool,
    /// Число активных участников.
    members: i64,
    /// Число открытых задач.
    open_tasks: i64,
    /// Число активных токенов.
    active_tokens: i64,
    /// Бот включён и его имя известно: ссылку-claim можно выпускать.
    telegram_ready: bool,
    /// Участники секции карточки.
    member_rows: Vec<MemberView>,
    /// Введённый Telegram ID формы участника: сохраняется при отказе.
    member_telegram_id: String,
    /// Выбранная роль формы участника.
    member_role: String,
    /// Введённое имя формы участника: сохраняется при отказе.
    member_name: String,
    /// Лимит длины имени участника из домена.
    member_name_max: usize,
    /// Строки API-токенов household.
    token_rows: Vec<TokenRow>,
    /// Введённое имя формы токена: сохраняется при отказе.
    token_name: String,
    /// Введённый срок формы токена: сохраняется при отказе.
    token_expires_in_days: String,
    /// Scopes формы токена с отметками.
    token_scope_options: Vec<ScopeOption>,
}

impl HouseholdView {
    /// Собрать карточку из сводки `admin`.
    fn new(household: HouseholdSummary) -> Self {
        Self {
            id: household.id,
            name: household.name,
            timezone: household.timezone,
            has_owner: household.owner_telegram_id.is_some(),
            members: household.members,
            open_tasks: household.open_tasks,
            active_tokens: household.active_tokens,
            telegram_ready: false,
            member_rows: Vec::new(),
            member_telegram_id: String::new(),
            member_role: DEFAULT_MEMBER_ROLE.to_owned(),
            member_name: String::new(),
            member_name_max: MAX_DISPLAY_NAME,
            token_rows: Vec::new(),
            token_name: String::new(),
            token_expires_in_days: String::new(),
            token_scope_options: scope_options(|scope| HOME_ASSISTANT_SCOPES.contains(&scope)),
        }
    }

    /// Часовой пояс household для дат карточки.
    ///
    /// Пояс валидируется при создании household ([`validate_timezone`]), поэтому
    /// ветка `Tz::UTC` недостижима; она оставлена, чтобы карточка не падала на
    /// повреждённых данных.
    fn timezone(&self) -> Tz {
        parse_timezone(&self.timezone).unwrap_or(Tz::UTC)
    }

    /// Выставить готовность Telegram для предложения ссылки-claim.
    fn with_telegram_ready(mut self, telegram_ready: bool) -> Self {
        self.telegram_ready = telegram_ready;
        self
    }

    /// Заполнить секцию участников; дата вступления — в поясе household.
    fn with_members(mut self, members: Vec<User>) -> Self {
        let timezone = self.timezone();
        self.member_rows = members
            .into_iter()
            .map(|user| MemberView::new(user, timezone))
            .collect();
        self
    }

    /// Заполнить секцию API-токенов; состояние — на момент `now`.
    fn with_tokens(mut self, tokens: Vec<ApiToken>, now: DateTime<Utc>) -> Self {
        let timezone = self.timezone();
        self.token_rows = tokens
            .into_iter()
            .map(|token| TokenRow::new(token, timezone, now))
            .collect();
        self
    }

    /// Показать введённые значения формы участника после отказа.
    fn with_member_form(mut self, form: &MemberForm) -> Self {
        self.member_telegram_id = form.telegram_id.clone().unwrap_or_default();
        self.member_role = form
            .role
            .clone()
            .unwrap_or_else(|| DEFAULT_MEMBER_ROLE.to_owned());
        self.member_name = form.name.clone().unwrap_or_default();
        self
    }

    /// Показать введённые значения формы токена после отказа.
    fn with_token_form(mut self, form: &TokenFormInput) -> Self {
        self.token_name = form.name.clone();
        self.token_expires_in_days = form.expires_in_days.clone();
        self.token_scope_options =
            scope_options(|scope| form.scopes.iter().any(|chosen| chosen.as_str() == scope));
        self
    }

    /// Отрисовать содержимое карточки без макета.
    pub(crate) fn render_body(&self) -> Result<String, askama::Error> {
        DetailBody { view: self }.render()
    }
}

/// Данные списка household и формы создания.
pub(crate) struct HouseholdList {
    /// Строки в порядке `list_households`.
    rows: Vec<HouseholdRow>,
    /// Активных household нет: показать пояснение вместо пустого списка.
    empty: bool,
    /// Показывать форму создания.
    show_form: bool,
    /// Название из формы: пусто при первом открытии.
    name: String,
    /// Часовой пояс из формы: умолчание мастера при первом открытии.
    timezone: String,
}

impl HouseholdList {
    /// Собрать список из сводок `admin`.
    fn new(households: Vec<HouseholdSummary>, show_form: bool) -> Self {
        let rows: Vec<HouseholdRow> = households.into_iter().map(HouseholdRow::new).collect();
        let empty = rows.is_empty();
        Self {
            rows,
            empty,
            // Без household создавать нечего кроме первого: форма нужна всегда.
            show_form: show_form || empty,
            name: String::new(),
            timezone: DEFAULT_TIMEZONE.to_owned(),
        }
    }

    /// Показать форму после отказа домена с введёнными значениями.
    fn with_submitted(mut self, name: &str, timezone: &str) -> Self {
        self.show_form = true;
        self.name = name.to_owned();
        self.timezone = timezone.to_owned();
        self
    }
}

/// Данные страницы подтверждения удаления household.
pub(crate) struct HouseholdDeleteView {
    /// Идентификатор для адреса формы.
    id: Uuid,
    /// Текущее название household.
    name: String,
    /// Введённое подтверждение: сохраняется при отказе.
    confirm_name: String,
}

impl HouseholdDeleteView {
    /// Собрать страницу для активного household.
    fn new(id: Uuid, name: String) -> Self {
        Self {
            id,
            name,
            confirm_name: String::new(),
        }
    }

    /// Показать введённое название после отказа.
    fn with_submitted(mut self, confirm_name: Option<&str>) -> Self {
        self.confirm_name = confirm_name.unwrap_or_default().to_owned();
        self
    }

    /// Отрисовать содержимое страницы без макета.
    pub(crate) fn render_body(&self) -> Result<String, askama::Error> {
        DeleteBody { view: self }.render()
    }
}

/// Страница выпущенного токена household: открытое значение показывается один
/// раз, в базу и журнал не попадает.
pub(crate) struct HouseholdTokenIssuedView {
    /// Идентификатор household для ссылки назад.
    household_id: Uuid,
    /// Название токена.
    name: String,
    /// Открытое значение; только в теле ответа.
    token: String,
    /// Scopes через запятую.
    scopes: String,
}

impl HouseholdTokenIssuedView {
    /// Собрать страницу из результата [`admin::create_token`].
    pub(crate) fn new(household_id: Uuid, issued: IssuedToken) -> Self {
        Self {
            household_id,
            name: issued.record.name,
            token: issued.token,
            scopes: issued.record.scopes.join(", "),
        }
    }

    /// Отрисовать содержимое страницы без макета.
    pub(crate) fn render_body(&self) -> Result<String, askama::Error> {
        TokenIssuedBody { view: self }.render()
    }
}

/// Поля формы подтверждения удаления.
#[derive(Deserialize)]
pub(crate) struct HouseholdDeleteForm {
    /// Название household; отсутствие поля и пустое значение — отказ.
    #[serde(default)]
    confirm_name: Option<String>,
}

/// Страница подтверждения отзыва токена household: имя и префикс без значения.
pub(crate) struct HouseholdTokenRevokeView {
    /// Идентификатор household для адреса формы и ссылки назад.
    household_id: Uuid,
    /// Идентификатор токена для адреса формы.
    token_id: Uuid,
    /// Название токена.
    name: String,
    /// Видимый префикс.
    prefix: String,
}

impl HouseholdTokenRevokeView {
    /// Собрать страницу из токена, уже сверенного с household.
    fn new(household_id: Uuid, token: ApiToken) -> Self {
        Self {
            household_id,
            token_id: token.id,
            name: token.name,
            prefix: token.prefix,
        }
    }

    /// Отрисовать содержимое страницы без макета.
    pub(crate) fn render_body(&self) -> Result<String, askama::Error> {
        TokenRevokeBody { view: self }.render()
    }
}

/// Поля формы отзыва токена.
#[derive(Deserialize)]
pub(crate) struct TokenRevokeForm {
    /// `yes` из формы подтверждения; отсутствие или другое значение — отказ.
    #[serde(default)]
    confirm: Option<String>,
}

/// Поля формы добавления участника.
#[derive(Deserialize)]
pub(crate) struct MemberForm {
    /// Telegram ID: полный, только при добавлении.
    #[serde(default)]
    telegram_id: Option<String>,
    /// Роль: `member` по умолчанию или `owner`.
    #[serde(default)]
    role: Option<String>,
    /// Необязательное имя.
    #[serde(default)]
    name: Option<String>,
}

/// Разобранная форма участника для [`admin::add_user`].
struct MemberSubmission {
    /// Роль нового участника.
    role: UserRole,
    /// Положительный Telegram ID.
    telegram_id: i64,
    /// Имя, если администратор его указал.
    name: Option<String>,
}

/// Разобрать форму участника: поля ввода проверяются здесь, правила членства —
/// в `src/admin`.
fn parse_member_submission(form: &MemberForm) -> Result<MemberSubmission, DomainError> {
    let raw = form.telegram_id.as_deref().unwrap_or_default().trim();
    let telegram_id: i64 = raw
        .parse()
        .map_err(|_| DomainError::Validation(INVALID_TELEGRAM_ID.into()))?;
    if telegram_id <= 0 {
        return Err(DomainError::Validation(INVALID_TELEGRAM_ID.into()));
    }
    let role = match form.role.as_deref().unwrap_or(DEFAULT_MEMBER_ROLE) {
        "owner" => UserRole::Owner,
        DEFAULT_MEMBER_ROLE => UserRole::Member,
        _ => {
            return Err(DomainError::Validation(
                "role must be owner or member".into(),
            ));
        }
    };
    let name = form
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned);
    Ok(MemberSubmission {
        role,
        telegram_id,
        name,
    })
}

/// Отрисовать содержимое списка без макета.
pub(crate) fn render_list_body(view: &HouseholdList) -> Result<String, askama::Error> {
    ListBody { view }.render()
}

/// `GET /admin/households`: список; в `single` — сразу единственный household.
pub(crate) async fn list(State(state): State<AdminState>, _session: AdminSession) -> Response {
    let mode = state.wizard().tenancy;
    if mode == TenancyMode::Single {
        match admin::selected_household(&state.pool, mode).await {
            Ok(Some(household)) => {
                return Redirect::to(&format!("/admin/households/{}", household.id))
                    .into_response();
            }
            // Ноль или несколько household — показываем список.
            Ok(None) => {}
            Err(error) => return internal(error),
        }
    }

    match admin::list_households(&state.pool, Utc::now()).await {
        Ok(households) => {
            // В `single` форма нужна только когда household ещё нет.
            let show_form = mode == TenancyMode::Multi || households.is_empty();
            render_household_list(HouseholdList::new(households, show_form))
        }
        Err(error) => internal(error),
    }
}

/// `POST /admin/households`: создать household без владельца.
pub(crate) async fn create(
    State(state): State<AdminState>,
    _session: AdminSession,
    Form(form): Form<HouseholdForm>,
) -> Response {
    let now = Utc::now();
    let (name, timezone) = match normalize_text(&form.name, 100, "household name")
        .and_then(|name| validate_timezone(&form.timezone).map(|timezone| (name, timezone)))
    {
        Ok(values) => values,
        Err(error) => {
            return form_refusal(&state, now, &form, &error, error_status(&error)).await;
        }
    };

    match admin::create_household(&state.pool, state.wizard().tenancy, &name, &timezone, now).await
    {
        Ok(household) => {
            Redirect::to(&format!("/admin/households/{}", household.id)).into_response()
        }
        Err(DomainError::Database(error)) => internal(error),
        // Отказ домена (в `single` уже есть household, валидация повторилась в
        // репозитории) — страница формы, а не `303` и не `500`.
        Err(error) => form_refusal(&state, now, &form, &error, error_status(&error)).await,
    }
}

/// Список с формой и текстом отказа; введённые значения сохраняются.
async fn form_refusal(
    state: &AdminState,
    now: DateTime<Utc>,
    form: &HouseholdForm,
    error: &DomainError,
    status: StatusCode,
) -> Response {
    let message = admin::describe(error);
    match admin::list_households(&state.pool, now).await {
        Ok(households) => pages::render_household_list_error(
            HouseholdList::new(households, true).with_submitted(&form.name, &form.timezone),
            &message,
            status,
        ),
        Err(error) => internal(error),
    }
}

/// HTTP-статус отказа домена; ошибка базы остаётся `500`.
fn error_status(error: &DomainError) -> StatusCode {
    match error {
        DomainError::Validation(_) => StatusCode::UNPROCESSABLE_ENTITY,
        DomainError::NotFound => StatusCode::NOT_FOUND,
        DomainError::Forbidden(_) => StatusCode::FORBIDDEN,
        DomainError::Conflict { .. } => StatusCode::CONFLICT,
        DomainError::Database(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

/// `GET /admin/households/{id}`: карточка активного household.
///
/// Неизвестный, удалённый и нечисловой id отвечают одинаково `404`, не
/// раскрывая, какой из случаев встретился.
pub(crate) async fn detail(
    State(state): State<AdminState>,
    Path(raw_id): Path<String>,
    _session: AdminSession,
) -> Response {
    let Ok(household_id) = Uuid::parse_str(&raw_id) else {
        return not_found();
    };
    match load_card(&state, household_id, Utc::now()).await {
        Ok(view) => render_household_detail(view),
        Err(response) => *response,
    }
}

/// Загрузить карточку household или вернуть готовый ответ (`404`/`500`).
///
/// Ответ в `Err` упакован: он больше самой карточки, а `Result` с большим
/// `Err` не проходит clippy.
async fn load_card(
    state: &AdminState,
    household_id: Uuid,
    now: DateTime<Utc>,
) -> Result<HouseholdView, Box<Response>> {
    let household = match admin::household_summary(&state.pool, household_id, now).await {
        Ok(Some(household)) => household,
        Ok(None) => return Err(Box::new(not_found())),
        Err(error) => return Err(Box::new(internal(error))),
    };
    let members = match admin::list_members(&state.pool, household_id).await {
        Ok(members) => members,
        Err(DomainError::NotFound) => return Err(Box::new(not_found())),
        Err(error) => return Err(Box::new(internal(error))),
    };
    let tokens =
        match admin::list_tokens(&state.pool, state.wizard().tenancy, Some(household_id)).await {
            Ok(tokens) => tokens,
            Err(DomainError::NotFound) => return Err(Box::new(not_found())),
            Err(error) => return Err(Box::new(internal(error))),
        };

    // Готовность Telegram нужна только предложению ссылки-claim.
    let telegram_ready = if household.owner_telegram_id.is_some() {
        false
    } else {
        match owner::telegram_bot(state).await {
            Ok((ready, _)) => ready,
            Err(error) => return Err(Box::new(internal(error))),
        }
    };

    Ok(HouseholdView::new(household)
        .with_members(members)
        .with_tokens(tokens, now)
        .with_telegram_ready(telegram_ready))
}

/// `POST /admin/households/{id}/members`: добавить участника через домен.
///
/// Поля формы разбираются здесь, правила членства (второй владелец, Telegram ID
/// из другого household, `member_joined`) проверяет [`admin::add_user`]. Отказ
/// возвращает карточку с формой и введёнными значениями, ничего не меняя.
pub(crate) async fn add_member(
    State(state): State<AdminState>,
    Path(raw_id): Path<String>,
    _session: AdminSession,
    Form(form): Form<MemberForm>,
) -> Response {
    let Ok(household_id) = Uuid::parse_str(&raw_id) else {
        return not_found();
    };
    let now = Utc::now();
    let submission = match parse_member_submission(&form) {
        Ok(submission) => submission,
        Err(error) => {
            return member_refusal(&state, household_id, &form, &error, now, None).await;
        }
    };

    match admin::add_user(
        &state.pool,
        state.wizard().tenancy,
        Some(household_id),
        submission.role,
        submission.telegram_id,
        submission.name.as_deref(),
        now,
    )
    .await
    {
        Ok(_) => Redirect::to(&format!("/admin/households/{household_id}")).into_response(),
        Err(DomainError::Database(error)) => internal(error),
        Err(error) => {
            let hint = owner_exists(&error).then_some(OWNER_EXISTS_HINT);
            member_refusal(&state, household_id, &form, &error, now, hint).await
        }
    }
}

/// Карточка с формой участника и текстом отказа; удалённый household — `404`.
async fn member_refusal(
    state: &AdminState,
    household_id: Uuid,
    form: &MemberForm,
    error: &DomainError,
    now: DateTime<Utc>,
    hint: Option<&str>,
) -> Response {
    let view = match load_card(state, household_id, now).await {
        Ok(view) => view.with_member_form(form),
        Err(response) => return *response,
    };
    let message = admin::describe(error);
    render_household_detail_error(view, &message, hint, error_status(error))
}

/// Отказ «владелец уже есть»: подсказка ведёт к передаче владения в боте.
fn owner_exists(error: &DomainError) -> bool {
    matches!(error, DomainError::Conflict { details, .. } if details["reason"] == "owner_exists")
}

/// `POST /admin/households/{id}/tokens`: выпустить токен и показать значение
/// один раз.
///
/// Правила scopes и срока остаются в [`admin::create_token`]: консоль только
/// разбирает форму и передаёт отказ текстом [`admin::describe`]. Открытое
/// значение попадает только в тело ответа; в базу и аудит идут хеш и
/// метаданные. `Cache-Control: no-store` ставит слой заголовков консоли.
pub(crate) async fn add_token(
    State(state): State<AdminState>,
    Path(raw_id): Path<String>,
    _session: AdminSession,
    RawForm(body): RawForm,
) -> Response {
    let Ok(household_id) = Uuid::parse_str(&raw_id) else {
        return not_found();
    };
    let now = Utc::now();
    let form = TokenFormInput::parse(&body);
    let expires_in_days = match parse_expiry_days(&form.expires_in_days) {
        Ok(days) => days,
        Err(message) => {
            return token_refusal(
                &state,
                household_id,
                &form,
                message,
                StatusCode::UNPROCESSABLE_ENTITY,
            )
            .await;
        }
    };

    match admin::create_token(
        &state.pool,
        state.wizard().tenancy,
        Some(household_id),
        &form.name,
        &form.scopes,
        expires_in_days,
        now,
    )
    .await
    {
        Ok(issued) => {
            render_household_token_issued(HouseholdTokenIssuedView::new(household_id, issued))
        }
        Err(DomainError::Database(error)) => internal(error),
        Err(DomainError::NotFound) => not_found(),
        Err(error) => {
            let message = admin::describe(&error);
            let status = error_status(&error);
            token_refusal(&state, household_id, &form, &message, status).await
        }
    }
}

/// Карточка с формой токена и текстом отказа; удалённый household — `404`.
async fn token_refusal(
    state: &AdminState,
    household_id: Uuid,
    form: &TokenFormInput,
    message: &str,
    status: StatusCode,
) -> Response {
    let view = match load_card(state, household_id, Utc::now()).await {
        Ok(view) => view.with_token_form(form),
        Err(response) => return *response,
    };
    render_household_detail_error(view, message, None, status)
}

/// Токен household из пути; чужой, удалённый household и неизвестный токен —
/// `404`.
///
/// Отдельного SQL консоль не пишет: [`admin::list_tokens`] проверяет household и
/// отдаёт только его токены.
async fn household_token(
    state: &AdminState,
    household_id: Uuid,
    token_id: Uuid,
) -> Result<ApiToken, Box<Response>> {
    let tokens =
        match admin::list_tokens(&state.pool, state.wizard().tenancy, Some(household_id)).await {
            Ok(tokens) => tokens,
            Err(DomainError::NotFound) => return Err(Box::new(not_found())),
            Err(error) => return Err(Box::new(internal(error))),
        };
    tokens
        .into_iter()
        .find(|token| token.id == token_id)
        .ok_or_else(|| Box::new(not_found()))
}

/// `GET /admin/households/{id}/tokens/{token_id}/revoke`: подтверждение отзыва.
///
/// Страница только показывает имя и префикс; принадлежность токена household
/// проверяет [`household_token`], поэтому чужой токен и неверный UUID дают
/// `404`. Уже отозванный или истёкший токен отзывать нечего: `303` на карточку.
pub(crate) async fn revoke_token_page(
    State(state): State<AdminState>,
    Path((raw_id, raw_token_id)): Path<(String, String)>,
    _session: AdminSession,
) -> Response {
    let (Ok(household_id), Ok(token_id)) =
        (Uuid::parse_str(&raw_id), Uuid::parse_str(&raw_token_id))
    else {
        return not_found();
    };
    match household_token(&state, household_id, token_id).await {
        Ok(token) if !revocable(&token, Utc::now()) => household_card(household_id),
        Ok(token) => {
            render_household_token_revoke(HouseholdTokenRevokeView::new(household_id, token))
        }
        Err(response) => *response,
    }
}

/// `POST /admin/households/{id}/tokens/{token_id}/revoke`: отозвать с
/// подтверждением.
///
/// Без `confirm=yes` данные не меняются: страница возвращается с отказом. Чужой
/// токен — `404` до всякого отзыва. Уже отозванный или истёкший токен менять
/// нечего — `303` на карточку и без подтверждения. Сам отзыв выполняет
/// [`admin::revoke_household_token`], поэтому аудит и системный актор те же, что
/// у CLI.
pub(crate) async fn revoke_token(
    State(state): State<AdminState>,
    Path((raw_id, raw_token_id)): Path<(String, String)>,
    _session: AdminSession,
    Form(form): Form<TokenRevokeForm>,
) -> Response {
    let (Ok(household_id), Ok(token_id)) =
        (Uuid::parse_str(&raw_id), Uuid::parse_str(&raw_token_id))
    else {
        return not_found();
    };
    let now = Utc::now();
    let token = match household_token(&state, household_id, token_id).await {
        Ok(token) => token,
        Err(response) => return *response,
    };
    if !revocable(&token, now) {
        return household_card(household_id);
    }

    if form.confirm.as_deref() != Some("yes") {
        return render_household_token_revoke_error(
            HouseholdTokenRevokeView::new(household_id, token),
            REVOKE_WITHOUT_CONFIRMATION,
            StatusCode::UNPROCESSABLE_ENTITY,
        );
    }

    match admin::revoke_household_token(&state.pool, household_id, token_id, now).await {
        Ok(_) => Redirect::to(&format!("/admin/households/{household_id}")).into_response(),
        Err(DomainError::NotFound) => not_found(),
        Err(DomainError::Database(error)) => internal(error),
        Err(error) => render_household_token_revoke_error(
            HouseholdTokenRevokeView::new(household_id, token),
            &admin::describe(&error),
            error_status(&error),
        ),
    }
}

/// `GET /admin/households/{id}/delete`: подтверждение вводом названия.
///
/// Страница только читает название; активность household проверяет
/// [`admin::household_name`], поэтому удалённый и несуществующий id дают `404`.
pub(crate) async fn delete_page(
    State(state): State<AdminState>,
    Path(raw_id): Path<String>,
    _session: AdminSession,
) -> Response {
    let Ok(household_id) = Uuid::parse_str(&raw_id) else {
        return not_found();
    };
    match admin::household_name(&state.pool, household_id).await {
        Ok(name) => render_household_delete(HouseholdDeleteView::new(household_id, name)),
        Err(DomainError::NotFound) => not_found(),
        Err(error) => internal(error),
    }
}

/// `POST /admin/households/{id}/delete`: сверить название и мягко удалить.
///
/// Сравнение с текущим названием идёт здесь через [`confirmation_matches`] —
/// то же правило, что у CLI и бота: [`admin::delete_household`] не принимает
/// подтверждение. Пробелы по краям ввода не учитываются, регистр важен. При
/// отказе форма возвращается с сообщением, данные не меняются.
pub(crate) async fn delete(
    State(state): State<AdminState>,
    Path(raw_id): Path<String>,
    _session: AdminSession,
    Form(form): Form<HouseholdDeleteForm>,
) -> Response {
    let Ok(household_id) = Uuid::parse_str(&raw_id) else {
        return not_found();
    };
    let name = match admin::household_name(&state.pool, household_id).await {
        Ok(name) => name,
        Err(DomainError::NotFound) => return not_found(),
        Err(error) => return internal(error),
    };

    let confirmation = form.confirm_name.as_deref().unwrap_or_default();
    let refusal = if confirmation.trim().is_empty() {
        Some(DELETE_EMPTY_CONFIRMATION)
    } else if !confirmation_matches(&name, confirmation) {
        Some(DELETE_NAME_MISMATCH)
    } else {
        None
    };
    if let Some(message) = refusal {
        return delete_refusal(
            household_id,
            name,
            form.confirm_name.as_deref(),
            message,
            StatusCode::UNPROCESSABLE_ENTITY,
        );
    }

    match admin::delete_household(&state.pool, household_id, Utc::now()).await {
        Ok(_) => Redirect::to("/admin/households").into_response(),
        Err(DomainError::NotFound) => not_found(),
        Err(error) => delete_refusal(
            household_id,
            name,
            form.confirm_name.as_deref(),
            &admin::describe(&error),
            error_status(&error),
        ),
    }
}

/// Страница подтверждения с сообщением отказа; ввод сохраняется.
fn delete_refusal(
    household_id: Uuid,
    name: String,
    confirm_name: Option<&str>,
    message: &str,
    status: StatusCode,
) -> Response {
    render_household_delete_error(
        HouseholdDeleteView::new(household_id, name).with_submitted(confirm_name),
        message,
        status,
    )
}

/// `303` на карточку household: отзывать нечего или действие завершено.
fn household_card(household_id: Uuid) -> Response {
    Redirect::to(&format!("/admin/households/{household_id}")).into_response()
}

/// `404` без подробностей: какой из случаев скрыт, не сообщается.
fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "Not Found").into_response()
}

/// Содержимое списка household и формы создания.
#[derive(Template)]
#[template(
    source = r#"<header class="page-head">
<h1>Household</h1>
<p class="page-desc">Активные household инсталляции.</p>
</header>
<section class="panel">
<header class="panel-head"><h2>Активные household</h2></header>
{% if view.empty %}
<div class="panel-body"><p class="muted">Активных household нет. Создайте первый household формой ниже.</p></div>
{% else %}
<ul class="list">
{% for row in view.rows %}
<li>
<div class="row-text">
<a class="row-label" href="/admin/households/{{ row.id }}">{{ row.name }}</a>
<span class="row-desc">{{ row.timezone }} · владелец: {% if row.has_owner %}есть{% else %}нет{% endif %} · участники: {{ row.members }} · открытых задач: {{ row.open_tasks }} · активных токенов: {{ row.active_tokens }}</span>
</div>
</li>
{% endfor %}
</ul>
{% endif %}
</section>
{% if view.show_form %}
<form method="post" action="/admin/households">
<section class="panel">
<header class="panel-head"><h2>Новый household</h2></header>
<div class="panel-body grid-2">
<div class="field"><label for="household-create-name">Название</label>
<input id="household-create-name" name="name" type="text" value="{{ view.name }}" maxlength="100" required></div>
<div class="field"><label for="household-create-timezone">Часовой пояс</label>
<input id="household-create-timezone" class="mono" name="timezone" type="text" value="{{ view.timezone }}" maxlength="64" required></div>
</div>
<div class="panel-foot"><button type="submit" class="btn-primary">Создать household</button></div>
</section>
</form>
{% endif %}
"#,
    ext = "html"
)]
struct ListBody<'a> {
    /// Данные списка.
    view: &'a HouseholdList,
}

/// Содержимое карточки household.
#[derive(Template)]
#[template(
    source = r#"<header class="page-head">
<h1>{{ view.name }}</h1>
<p class="page-desc">Household.</p>
</header>
<section class="panel">
<header class="panel-head"><h2>Household</h2></header>
<dl class="console-state">
<dt>Название</dt><dd>{{ view.name }}</dd>
<dt>Часовой пояс</dt><dd>{{ view.timezone }}</dd>
<dt>Владелец</dt><dd>{% if view.has_owner %}есть{% else %}нет{% endif %}</dd>
<dt>Участники</dt><dd>{{ view.members }}</dd>
<dt>Открытые задачи</dt><dd>{{ view.open_tasks }}</dd>
<dt>Активные токены</dt><dd>{{ view.active_tokens }}</dd>
</dl>
</section>
<section class="panel">
<header class="panel-head"><h2>Участники</h2></header>
{% if view.member_rows.is_empty() %}
<div class="panel-body"><p class="muted">В household пока нет участников.</p></div>
{% else %}
<ul class="list">
{% for member in view.member_rows %}
<li>
<div class="row-text">
<span class="row-label">{{ member.name }}</span>
<span class="row-desc">{{ member.role }} · вступил(а) {{ member.joined_at }} · Telegram ID {{ member.telegram_id }}</span>
</div>
</li>
{% endfor %}
</ul>
{% endif %}
<div class="panel-body">
<form method="post" action="/admin/households/{{ view.id }}/members">
<div class="field"><label for="member-telegram-id">Telegram ID</label>
<input id="member-telegram-id" name="telegram_id" type="text" inputmode="numeric" value="{{ view.member_telegram_id }}" required></div>
<div class="field"><label for="member-role">Роль</label>
<select id="member-role" name="role">
<option value="member"{% if view.member_role == "member" %} selected{% endif %}>участник</option>
<option value="owner"{% if view.member_role == "owner" %} selected{% endif %}>владелец</option>
</select></div>
<div class="field"><label for="member-name">Имя (необязательно)</label>
<input id="member-name" name="name" type="text" value="{{ view.member_name }}" maxlength="{{ view.member_name_max }}"></div>
<div class="actions"><button type="submit" class="btn-primary">Добавить участника</button></div>
</form>
</div>
</section>
{% if !view.has_owner %}
<section class="panel">
<header class="panel-head"><h2>Владелец</h2></header>
{% if view.telegram_ready %}
<div class="panel-body">
<p>Выпустите одноразовую ссылку для того, кто станет владельцем.</p>
<form method="post" action="/admin/owner">
<input type="hidden" name="target" value="{{ view.id }}">
<button type="submit">Выпустить ссылку</button>
</form>
</div>
{% else %}
<div class="panel-body"><p class="console-notice">Сначала подключите бота на странице <a href="/admin/telegram">Telegram</a>.</p></div>
{% endif %}
</section>
{% endif %}
<section class="panel">
<header class="panel-head"><h2>API-токены</h2></header>
{% if view.token_rows.is_empty() %}
<div class="panel-body"><p class="muted">Токенов пока нет.</p></div>
{% else %}
<ul class="list">
{% for token in view.token_rows %}
<li>
<div class="row-text">
<span class="row-label">{{ token.name }}</span>
<span class="row-desc">{{ token.prefix }} · scopes: {{ token.scopes }} · {{ token.state }} · последнее использование: {{ token.last_used }}{% match token.expires_at %}{% when Some with (expires_at) %} · срок до {{ expires_at }}{% when None %}{% endmatch %}</span>
</div>
{% if token.revocable %}
<a class="btn btn-danger" href="/admin/households/{{ view.id }}/tokens/{{ token.id }}/revoke">Отозвать</a>
{% endif %}
</li>
{% endfor %}
</ul>
{% endif %}
<div class="panel-body">
<form method="post" action="/admin/households/{{ view.id }}/tokens">
<div class="field"><label for="token-name">Название</label>
{# Лимит совпадает с normalize_text(name, 64, ...) в `auth::tokens::issue`: поле формы не даёт превысить доменный предел. #}
<input id="token-name" name="name" type="text" value="{{ view.token_name }}" maxlength="64" required></div>
<div class="field"><span class="field-head"><span>Scopes</span></span>
{% for option in view.token_scope_options %}
<label class="check"><input type="checkbox" name="scope" value="{{ option.scope }}"{% if option.checked %} checked{% endif %}> {{ option.scope }}</label>
{% endfor %}
</div>
<div class="field"><label for="token-expires">Срок действия, дней (необязательно)</label>
<input id="token-expires" name="expires_in_days" type="text" inputmode="numeric" value="{{ view.token_expires_in_days }}"></div>
<div class="actions"><button type="submit" class="btn-primary">Выпустить токен</button></div>
</form>
</div>
</section>
<section class="panel danger-zone">
<header class="panel-head"><h2>Опасная зона</h2></header>
<div class="row">
<div class="row-text">
<span class="row-label">Удаление household</span>
<span class="row-desc">Участники будут удалены, токены и приглашения отозваны.</span>
</div>
<a class="btn btn-danger" href="/admin/households/{{ view.id }}/delete">Удалить household</a>
</div>
</section>
"#,
    ext = "html"
)]
struct DetailBody<'a> {
    /// Данные карточки.
    view: &'a HouseholdView,
}

/// Содержимое страницы подтверждения удаления.
#[derive(Template)]
#[template(
    source = r#"<header class="page-head">
<h1>Удаление household</h1>
<p class="page-desc">Подтвердите удаление вводом названия.</p>
</header>
<section class="panel danger-zone">
<header class="panel-head"><h2>Удалить household</h2></header>
<div class="panel-body">
<p>Household «{{ view.name }}» и все участники будут помечены удалёнными. API-токены и приглашения будут отозваны. Отменить действие нельзя.</p>
<form method="post" action="/admin/households/{{ view.id }}/delete">
<div class="field"><label for="household-delete-confirm">Название household</label>
<input id="household-delete-confirm" name="confirm_name" type="text" value="{{ view.confirm_name }}" maxlength="100" required></div>
<div class="actions"><button type="submit" class="btn-danger">Удалить household</button></div>
</form>
</div>
</section>
"#,
    ext = "html"
)]
struct DeleteBody<'a> {
    /// Данные подтверждения.
    view: &'a HouseholdDeleteView,
}

/// Содержимое страницы подтверждения отзыва токена.
#[derive(Template)]
#[template(
    source = r#"<header class="page-head">
<h1>Отзыв API-токена</h1>
<p class="page-desc">Подтвердите отзыв токена.</p>
</header>
<section class="panel danger-zone">
<header class="panel-head"><h2>Отозвать токен</h2></header>
<div class="panel-body">
<p>Токен «{{ view.name }}» ({{ view.prefix }}) будет отозван. Интеграции с этим токеном перестанут работать. Отменить действие нельзя.</p>
<form method="post" action="/admin/households/{{ view.household_id }}/tokens/{{ view.token_id }}/revoke">
<input type="hidden" name="confirm" value="yes">
<div class="actions"><button type="submit" class="btn-danger">Отозвать токен</button></div>
</form>
</div>
</section>
"#,
    ext = "html"
)]
struct TokenRevokeBody<'a> {
    /// Данные подтверждения.
    view: &'a HouseholdTokenRevokeView,
}

/// Страница выпущенного токена household: значение показывается один раз.
#[derive(Template)]
#[template(
    source = r#"<header class="page-head">
<h1>API-токен выпущен</h1>
<p class="page-desc">Скопируйте значение сейчас: после перехода оно больше не будет доступно.</p>
</header>
<div class="console-warn" role="status"><p>Вставьте токен в настройки Home Assistant. В списке токенов останется только префикс.</p></div>
<section class="panel">
<header class="panel-head"><h2>{{ view.name }}</h2></header>
<div class="panel-body">
<p class="muted">Scopes: {{ view.scopes }}</p>
<input id="issued-token" class="token-value" name="token" type="text" value="{{ view.token }}" readonly aria-label="API-токен">
</div>
<div class="panel-foot"><a class="btn btn-primary" href="/admin/households/{{ view.household_id }}">К household</a></div>
</section>
"#,
    ext = "html"
)]
struct TokenIssuedBody<'a> {
    /// Данные выпущенного токена.
    view: &'a HouseholdTokenIssuedView,
}

#[cfg(test)]
mod tests {
    use super::mask_telegram_id;

    #[test]
    fn telegram_ids_show_only_the_last_four_digits_or_nothing_short() {
        assert_eq!(mask_telegram_id(123_456_789), "…6789");
        assert_eq!(mask_telegram_id(12_345), "…2345");
        assert_eq!(mask_telegram_id(1_234), "…");
        assert_eq!(mask_telegram_id(12), "…");
    }
}
