//! Страница очередей: числа по статусам и возраст готового обновления.
//!
//! Данные берутся из [`admin::queue_stats`]: только статусы и количества,
//! содержимое заданий не читается. Порог задержки — тот же, что у readiness на
//! главной ([`super::router::AdminState::queue_lag_limit`]); без порога возраст
//! показывается без выделения.
//!
//! Панель опрашивает себя через htmx раз в 10 секунд; без JavaScript ссылку
//! «Обновить» показывает `<noscript>`, как у страницы проверки LLM.

use askama::Template;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use chrono::{Duration, Utc};

use super::pages;
use super::router::AdminState;
use super::session::AdminSession;
use crate::admin::{self, QueueStats};
use crate::domain::error::DomainError;

/// Путь страницы очередей.
pub(crate) const QUEUES_PATH: &str = "/admin/queues";
/// Путь фрагмента панели для htmx.
pub(crate) const QUEUES_FRAGMENT_PATH: &str = "/admin/queues/fragment";

/// Очередь с количеством заданий по статусам.
struct QueueRow {
    /// Подпись очереди по-русски.
    label: &'static str,
    /// Статусы и количества через точку; пусто — «пусто».
    counts: String,
}

impl QueueRow {
    /// Собрать строку; пустая очередь показывается явно.
    fn new(label: &'static str, statuses: &[(String, i64)]) -> Self {
        let counts = if statuses.is_empty() {
            "пусто".to_owned()
        } else {
            statuses
                .iter()
                .map(|(status, count)| format!("{status}: {count}"))
                .collect::<Vec<_>>()
                .join(" · ")
        };
        Self { label, counts }
    }
}

/// Данные панели очередей.
pub(crate) struct QueuesView {
    /// Обновления Telegram, исходящие сообщения, разборы, доставки.
    queues: Vec<QueueRow>,
    /// Возраст самого старого готового обновления; «нет», если готовых нет.
    oldest_age: String,
    /// Порог readiness, который превышен; `None` — возраст в норме или порога нет.
    oldest_violation: Option<String>,
}

impl QueuesView {
    /// Собрать панель из статистики `admin` и порога readiness.
    fn new(stats: QueueStats, limit: Option<Duration>) -> Self {
        let age = stats
            .oldest_ready
            .map(|oldest| stats.now.signed_duration_since(oldest));
        let oldest_violation = match (age, limit) {
            (Some(age), Some(limit)) if age > limit => Some(format_age(limit.num_seconds())),
            _ => None,
        };

        Self {
            queues: vec![
                QueueRow::new("обновления Telegram", &stats.telegram_updates),
                QueueRow::new("исходящие сообщения", &stats.outbox),
                QueueRow::new("разборы", &stats.capture_jobs),
                QueueRow::new("доставки уведомлений", &stats.notification_deliveries),
            ],
            oldest_age: age.map_or_else(
                || "нет".to_owned(),
                |age| format_age(age.num_seconds().max(0)),
            ),
            oldest_violation,
        }
    }
}

/// Возраст в секундах или минутах: «45 с», «7 мин», «7 мин 5 с».
fn format_age(seconds: i64) -> String {
    let seconds = seconds.max(0);
    match (seconds / 60, seconds % 60) {
        (0, _) => format!("{seconds} с"),
        (minutes, 0) => format!("{minutes} мин"),
        (minutes, rest) => format!("{minutes} мин {rest} с"),
    }
}

/// `GET /admin/queues`: страница очередей.
pub(crate) async fn page(State(state): State<AdminState>, _session: AdminSession) -> Response {
    match load(&state).await {
        Ok(view) => pages::render_queues(view),
        Err(error) => pages::internal(error),
    }
}

/// `GET /admin/queues/fragment`: панель для htmx.
///
/// Панель опрашивает себя раз в 10 секунд; тот же шаблон встроен в страницу,
/// поэтому атрибуты опроса и `<noscript>`-ссылка у них общие.
pub(crate) async fn fragment(State(state): State<AdminState>, _session: AdminSession) -> Response {
    let view = match load(&state).await {
        Ok(view) => view,
        Err(error) => return pages::internal(error),
    };
    match render_panel(&view) {
        Ok(body) => (StatusCode::OK, Html(body)).into_response(),
        Err(error) => pages::internal(error),
    }
}

/// Собрать статистику и применить тот же порог задержки, что у главной.
async fn load(state: &AdminState) -> Result<QueuesView, DomainError> {
    let stats = admin::queue_stats(&state.pool, Utc::now()).await?;
    Ok(QueuesView::new(stats, state.queue_lag_limit()))
}

/// Отрисовать содержимое страницы без макета.
pub(crate) fn render_page_body(view: &QueuesView) -> Result<String, askama::Error> {
    let panel = render_panel(view)?;
    PageBody { panel }.render()
}

/// Отрисовать панель; она же — фрагмент htmx.
pub(crate) fn render_panel(view: &QueuesView) -> Result<String, askama::Error> {
    PanelBody {
        view,
        path: QUEUES_FRAGMENT_PATH,
    }
    .render()
}

/// Содержимое страницы очередей: заголовок и панель.
#[derive(Template)]
#[template(
    source = r#"<header class="page-head">
<h1>Очереди</h1>
<p class="page-desc">Состояние очередей инсталляции.</p>
</header>
{{ panel|safe }}
"#,
    ext = "html"
)]
struct PageBody {
    /// Уже отрисованная панель.
    panel: String,
}

/// Панель очередей; она же содержимое фрагмента для htmx.
#[derive(Template)]
#[template(
    source = r#"<div id="queues" class="panel" hx-get="{{ path }}" hx-trigger="every 10s" hx-swap="outerHTML">
<header class="panel-head"><h2>Очереди</h2></header>
<ul class="list">
{% for queue in view.queues %}
<li>
<div class="row-text">
<span class="row-label">{{ queue.label }}</span>
<span class="row-desc">{{ queue.counts }}</span>
</div>
</li>
{% endfor %}
</ul>
<div class="panel-body">
<p>Самое старое готовое обновление: {% match view.oldest_violation %}{% when Some with (limit) %}<span class="status status-bad">{{ view.oldest_age }}</span> — превышает порог readiness {{ limit }}{% when None %}<span class="status">{{ view.oldest_age }}</span>{% endmatch %}</p>
</div>
<noscript><p><a href="{{ path }}">Обновить</a></p></noscript>
</div>
"#,
    ext = "html"
)]
struct PanelBody<'a> {
    /// Данные панели.
    view: &'a QueuesView,
    /// Путь фрагмента для опроса и ссылки без JavaScript.
    path: &'static str,
}

#[cfg(test)]
mod tests {
    use super::format_age;

    #[test]
    fn ages_are_shown_in_seconds_or_minutes() {
        assert_eq!(format_age(0), "0 с");
        assert_eq!(format_age(59), "59 с");
        assert_eq!(format_age(60), "1 мин");
        assert_eq!(format_age(425), "7 мин 5 с");
        assert_eq!(format_age(-1), "0 с");
    }
}
