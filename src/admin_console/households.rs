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

use askama::Template;
use axum::extract::{Form, Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use chrono::{DateTime, NaiveDate, Utc};
use serde::Deserialize;
use uuid::Uuid;

use super::owner;
use super::pages::{
    self, HouseholdForm, internal, render_household_delete, render_household_delete_error,
    render_household_detail, render_household_list,
};
use super::router::AdminState;
use super::session::AdminSession;
use crate::admin::{self, HouseholdSummary};
use crate::config::TenancyMode;
use crate::domain::error::DomainError;
use crate::domain::identity::confirmation_matches;
use crate::domain::models::{User, UserRole};
use crate::domain::settings::{DEFAULT_TIMEZONE, validate_timezone};
use crate::domain::validation::normalize_text;

/// Отказ без подтверждения: пустое поле или его отсутствие.
const DELETE_EMPTY_CONFIRMATION: &str = "Введите название household, чтобы подтвердить удаление.";
/// Отказ при несовпадении названия.
const DELETE_NAME_MISMATCH: &str = "Название не совпадает с названием household.";

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
    fn new(user: User) -> Self {
        Self {
            name: user.display_name,
            role: match user.role {
                UserRole::Owner => "владелец",
                UserRole::Member => "участник",
            },
            joined_at: user.created_at.date_naive(),
            telegram_id: mask_telegram_id(user.telegram_user_id),
        }
    }
}

/// Данные household для строки списка и карточки; без Telegram ID владельца и
/// содержимого задач.
pub(crate) struct HouseholdView {
    /// Идентификатор для ссылки на карточку и формы выпуска ссылки-claim.
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
    /// Участники для секции карточки; пусто в строке списка.
    member_rows: Vec<MemberView>,
}

impl HouseholdView {
    /// Собрать данные из сводки `admin`; готовность Telegram не нужна строке
    /// списка и по умолчанию выключена.
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
        }
    }

    /// Выставить готовность Telegram: её использует только карточка.
    fn with_telegram_ready(mut self, telegram_ready: bool) -> Self {
        self.telegram_ready = telegram_ready;
        self
    }

    /// Заполнить секцию участников карточки.
    fn with_members(mut self, members: Vec<User>) -> Self {
        self.member_rows = members.into_iter().map(MemberView::new).collect();
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
    rows: Vec<HouseholdView>,
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
        let rows: Vec<HouseholdView> = households.into_iter().map(HouseholdView::new).collect();
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

/// Поля формы подтверждения удаления.
#[derive(Deserialize)]
pub(crate) struct HouseholdDeleteForm {
    /// Название household; отсутствие поля и пустое значение — отказ.
    #[serde(default)]
    confirm_name: Option<String>,
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
    let household = match admin::household_summary(&state.pool, household_id, Utc::now()).await {
        Ok(Some(household)) => household,
        Ok(None) => return not_found(),
        Err(error) => return internal(error),
    };
    let members = match admin::list_members(&state.pool, household_id).await {
        Ok(members) => members,
        Err(DomainError::NotFound) => return not_found(),
        Err(error) => return internal(error),
    };

    // Готовность Telegram нужна только предложению ссылки-claim.
    let telegram_ready = if household.owner_telegram_id.is_some() {
        false
    } else {
        match owner::telegram_bot(&state).await {
            Ok((ready, _)) => ready,
            Err(error) => return internal(error),
        }
    };

    render_household_detail(
        HouseholdView::new(household)
            .with_members(members)
            .with_telegram_ready(telegram_ready),
    )
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

#[cfg(test)]
mod tests {
    use super::mask_telegram_id;

    #[test]
    fn telegram_ids_show_only_the_last_four_digits_or_nothing_short() {
        assert_eq!(mask_telegram_id(123_456_789), "…6789");
        assert_eq!(mask_telegram_id(1_234), "…");
        assert_eq!(mask_telegram_id(12), "…");
    }
}
