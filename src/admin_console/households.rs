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

use askama::Template;
use axum::extract::{Form, Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use chrono::{DateTime, Utc};
use uuid::Uuid;

use super::owner;
use super::pages::{self, HouseholdForm, internal, render_household_detail, render_household_list};
use super::router::AdminState;
use super::session::AdminSession;
use crate::admin::{self, HouseholdSummary};
use crate::config::TenancyMode;
use crate::domain::error::DomainError;
use crate::domain::settings::{DEFAULT_TIMEZONE, validate_timezone};
use crate::domain::validation::normalize_text;

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
}

impl HouseholdView {
    /// Собрать данные из сводки `admin`.
    fn new(household: HouseholdSummary, telegram_ready: bool) -> Self {
        Self {
            id: household.id,
            name: household.name,
            timezone: household.timezone,
            has_owner: household.owner_telegram_id.is_some(),
            members: household.members,
            open_tasks: household.open_tasks,
            active_tokens: household.active_tokens,
            telegram_ready,
        }
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
        let rows: Vec<HouseholdView> = households
            .into_iter()
            .map(|household| HouseholdView::new(household, false))
            .collect();
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

    // Готовность Telegram нужна только предложению ссылки-claim.
    let telegram_ready = if household.owner_telegram_id.is_some() {
        false
    } else {
        match owner::telegram_bot(&state).await {
            Ok((ready, _)) => ready,
            Err(error) => return internal(error),
        }
    };

    render_household_detail(HouseholdView::new(household, telegram_ready))
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
"#,
    ext = "html"
)]
struct DetailBody<'a> {
    /// Данные карточки.
    view: &'a HouseholdView,
}
