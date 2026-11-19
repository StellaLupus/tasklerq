//! Страницы household: список активных и карточка одного.
//!
//! Список берётся из [`admin::list_households`], карточка — из
//! [`admin::household_summary`]: консоль не пишет собственный SQL и не
//! повторяет правила выбора household. В `single` список сразу ведёт на
//! единственный household через [`admin::selected_household`], то есть по тому
//! же правилу, что CLI.
//!
//! Карточка — одна панель с общими данными [`admin::HouseholdSummary`].
//! Участники, токены и выгрузка появятся следующими панелями рядом, поэтому
//! страница не рисует пустых заглушек будущих разделов.

use askama::Template;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use chrono::Utc;
use uuid::Uuid;

use super::pages::{internal, render_household_detail, render_household_list};
use super::router::AdminState;
use super::session::AdminSession;
use crate::admin::{self, HouseholdSummary};
use crate::config::TenancyMode;

/// Строка списка household без Telegram ID владельца и содержимого задач.
struct HouseholdRow {
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

/// Данные списка household.
pub(crate) struct HouseholdList {
    /// Строки в порядке `list_households`.
    rows: Vec<HouseholdRow>,
    /// Активных household нет: показать пояснение вместо пустого списка.
    empty: bool,
}

impl HouseholdList {
    /// Собрать список из сводок `admin`.
    fn new(households: Vec<HouseholdSummary>) -> Self {
        let rows: Vec<HouseholdRow> = households
            .into_iter()
            .map(|household| HouseholdRow {
                id: household.id,
                name: household.name,
                timezone: household.timezone,
                has_owner: household.owner_telegram_id.is_some(),
                members: household.members,
                open_tasks: household.open_tasks,
                active_tokens: household.active_tokens,
            })
            .collect();
        let empty = rows.is_empty();
        Self { rows, empty }
    }
}

/// Данные карточки household.
pub(crate) struct HouseholdDetail {
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

impl HouseholdDetail {
    /// Собрать карточку из сводки `admin`.
    fn new(household: HouseholdSummary) -> Self {
        Self {
            name: household.name,
            timezone: household.timezone,
            has_owner: household.owner_telegram_id.is_some(),
            members: household.members,
            open_tasks: household.open_tasks,
            active_tokens: household.active_tokens,
        }
    }

    /// Отрисовать содержимое карточки без макета.
    pub(crate) fn render_body(&self) -> Result<String, askama::Error> {
        DetailBody { view: self }.render()
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
        Ok(households) => render_household_list(HouseholdList::new(households)),
        Err(error) => internal(error),
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
    match admin::household_summary(&state.pool, household_id, Utc::now()).await {
        Ok(Some(household)) => render_household_detail(HouseholdDetail::new(household)),
        Ok(None) => not_found(),
        Err(error) => internal(error),
    }
}

/// `404` без подробностей: какой из случаев скрыт, не сообщается.
fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "Not Found").into_response()
}

/// Содержимое списка household.
#[derive(Template)]
#[template(
    source = r#"<header class="page-head">
<h1>Household</h1>
<p class="page-desc">Активные household инсталляции.</p>
</header>
<section class="panel">
<header class="panel-head"><h2>Активные household</h2></header>
{% if view.empty %}
<div class="panel-body"><p class="muted">Активных household нет. Создайте household командой <code>tasklerq admin household create</code>: форма в консоли появится позже.</p></div>
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
"#,
    ext = "html"
)]
struct DetailBody<'a> {
    /// Данные карточки.
    view: &'a HouseholdDetail,
}
