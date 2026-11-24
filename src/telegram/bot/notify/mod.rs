//! Экраны уведомлений и действия «Выполнено» / «Отложить» / «Открыть».
//!
//! Уведомления — отдельные сообщения, а не часть навигационного экрана,
//! поэтому их кнопки выпускаются с областью `notice` и редактируют своё
//! сообщение. Сами уведомления ставит в очередь планировщик (фаза 7): он
//! использует эти конструкторы экранов.

use chrono::{DateTime, Duration, NaiveDate, NaiveTime, Utc};
use chrono_tz::Tz;
use sha2::{Digest, Sha256};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::models::{Item, ItemStatus, List, ListKind, User};
use crate::domain::recurrence::resolve_local;
use crate::domain::settings::DigestScope;
use crate::jobs::digest::DigestSection;
use crate::jobs::keys::snooze_origin;
use crate::storage::repositories::items::completions;
use crate::storage::repositories::notifications::{self, NewDelivery, NotificationKind};
use crate::storage::repositories::{items, lists};
use crate::telegram::callbacks::Scope;
use crate::telegram::outbox::{self, Outgoing};
use crate::telegram::rich::{Block, Span, button};
use crate::telegram::ru;

use super::model::{Action, Screen, SnoozeUntil, Ui};
use super::screens::{back_button, clip};
use super::{Ctx, Fail, issue_and_render};

mod batch;

pub use batch::{
    BATCH_PAGE_SIZE, BatchActions, BatchEntry, BatchEntryState, batch_complete, batch_complete_all,
    batch_move, batch_open, batch_page, batch_snooze, batch_snooze_all, batch_ui,
};

/// Час, на который переносится напоминание «до завтра».
const MORNING: (u32, u32) = (9, 0);

/// Строка «срок · список» для уведомления или блока сводного сообщения.
pub(crate) fn when_line(
    due_at: Option<DateTime<Utc>>,
    due_date: Option<NaiveDate>,
    list_name: &str,
    tz: Tz,
    now: DateTime<Utc>,
) -> String {
    match ru::due_label(due_at, due_date, tz, now) {
        Some(due) if !list_name.is_empty() => {
            format!("{} · {}", ru::capitalize(&due), list_name)
        }
        Some(due) => ru::capitalize(&due),
        None => list_name.to_owned(),
    }
}

/// Название задачи в уведомлении или блоке сводного сообщения: жирным и
/// обрезанным до 120 символов.
pub(super) fn title_spans(title: &str) -> Vec<Span> {
    vec![Span::bold(clip(title, 120))]
}

/// Момент события строкой напоминания: «Сегодня, 14:00»; без значения —
/// прочерк.
fn event_moment(
    at: Option<DateTime<Utc>>,
    date: Option<NaiveDate>,
    tz: Tz,
    now: DateTime<Utc>,
) -> String {
    ru::due_label(at, date, tz, now).map_or_else(|| "—".to_owned(), |label| ru::capitalize(&label))
}

/// Напоминание о сроке.
pub fn reminder_ui(
    item: &Item,
    list: &List,
    tz: Tz,
    now: DateTime<Utc>,
    lead_minutes: Option<i64>,
) -> Ui {
    let heading = match lead_minutes {
        Some(minutes) if minutes > 0 => format!("⏰ Через {}", ru::minutes(minutes)),
        _ => "⏰ Пора".to_owned(),
    };
    if list.kind == ListKind::Events {
        // Событие не выполняется и не откладывается: у напоминания свои
        // действия — убрать, перенести начало или открыть карточку.
        return Ui::new()
            .heading(heading)
            .spans(title_spans(&item.title))
            .text(format!(
                "📅 Начало: {}",
                event_moment(item.due_at, item.due_date, tz, now)
            ))
            .text(format!(
                "🏁 Конец: {}",
                event_moment(item.end_at, item.end_date, tz, now)
            ))
            .row(vec![
                button(ru::DISMISS_BUTTON, Action::NoticeClose),
                button("📅 Перенести", Action::NoticeMove { item_id: item.id }),
            ])
            .row(vec![button(
                "📋 Открыть",
                Action::NoticeOpen { item_id: item.id },
            )]);
    }
    Ui::new()
        .heading(heading)
        .spans(title_spans(&item.title))
        .text(when_line(item.due_at, item.due_date, &list.name, tz, now))
        .row(vec![
            button(
                ru::DONE_BUTTON,
                Action::NoticeComplete {
                    item_id: item.id,
                    version: item.version,
                },
            ),
            button(
                "😴 Отложить на час",
                Action::Snooze {
                    item_id: item.id,
                    until: SnoozeUntil::OneHour,
                    version: Some(item.version),
                },
            ),
        ])
        .row(vec![
            button(
                "📅 Завтра в 09:00",
                Action::Snooze {
                    item_id: item.id,
                    until: SnoozeUntil::TomorrowMorning,
                    version: Some(item.version),
                },
            ),
            button("📋 Открыть", Action::NoticeOpen { item_id: item.id }),
        ])
        .row(vec![button(ru::DISMISS_BUTTON, Action::NoticeClose)])
}

/// Уведомление о назначении.
pub fn assignment_ui(item: &Item, list: &List, tz: Tz, now: DateTime<Utc>) -> Ui {
    Ui::new()
        .heading("👤 Вам назначили задачу")
        .spans(title_spans(&item.title))
        .text(when_line(item.due_at, item.due_date, &list.name, tz, now))
        .row(vec![
            button("📋 Открыть", Action::NoticeOpen { item_id: item.id }),
            button(
                ru::DONE_BUTTON,
                Action::NoticeComplete {
                    item_id: item.id,
                    version: item.version,
                },
            ),
        ])
        .row(vec![button(ru::DISMISS_BUTTON, Action::NoticeClose)])
}

/// Вид шапки сводки: утренняя или по запросу (0019, design, решения 5, 7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DigestKind {
    /// Утренняя сводка: «☀️ Доброе утро! · <дата>».
    Morning,
    /// Сводка по запросу (суффикс `#manual` в ключе доставки):
    /// «☀️ На сегодня · <дата>».
    Manual,
}

/// Состояние строки сводки на момент отрисовки (0019, design, решение 4).
///
/// Строки собирает [`crate::jobs::digest::digest_view`] по снимку доставки и
/// текущему состоянию задач; отрисовка только форматирует их и не ходит в БД.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DigestLineState {
    /// Открытая задача: кнопка «☐ …» (касание — выполнение).
    Open {
        /// Текущее название.
        title: String,
        /// Текущий срок со временем.
        due_at: Option<DateTime<Utc>>,
        /// Текущий срок без времени.
        due_date: Option<NaiveDate>,
        /// Версия задачи на момент отрисовки.
        version: i32,
        /// Текущий исполнитель.
        assignee: Option<Uuid>,
        /// Имя исполнителя по правилу [`member_name`]; `None` — метки нет.
        assignee_name: Option<String>,
    },
    /// Выполненное вхождение снимка: «✅ …», кнопка (касание — отмена) только
    /// при `can_undo`.
    Completed {
        /// Текущее название.
        title: String,
        /// Имя выполнившего по правилу [`member_name`]; `None` — без имени.
        actor_name: Option<String>,
        /// Текущий срок повторяющейся задачи для «следующее …».
        next_due_at: Option<DateTime<Utc>>,
        /// Текущий срок без времени повторяющейся задачи.
        next_due_date: Option<NaiveDate>,
        /// Версия задачи на момент отрисовки.
        version: i32,
        /// Получатель может отменить это выполнение ([`items::can_undo`]).
        can_undo: bool,
    },
    /// Задача удалена: текст без кнопки.
    Deleted {
        /// Название на момент удаления.
        title: String,
    },
    /// Список задачи в архиве: текст без кнопки.
    Archived {
        /// Текущее название.
        title: String,
    },
    /// Событие: текст, время — из снимка, а не из текущего состояния.
    Event {
        /// Текущее название.
        title: String,
        /// Время вхождения из снимка в человекочитаемом виде.
        when: String,
    },
    /// Событие удалено: текст без кнопки.
    EventDeleted {
        /// Название на момент удаления.
        title: String,
    },
}

/// Видимая строка сводки в порядке снимка.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DigestViewLine {
    /// Элемент: задача или событие.
    pub item_id: Uuid,
    /// Раздел строки на момент доставки.
    pub section: DigestSection,
    /// Состояние строки на момент отрисовки.
    pub state: DigestLineState,
}

/// Разрешённое представление сводки для отрисовки (0019, design, решения 1, 4, 5).
///
/// Собирается [`crate::jobs::digest::digest_view`] из снимка доставки и
/// текущего состояния задач; [`digest_ui`] только форматирует его, не ходит
/// в БД и не знает «сейчас»: все подписи относительны к [`Self::day`].
#[derive(Debug, Clone)]
pub struct DigestView {
    /// Доставка сводки: её несут кнопки строк.
    pub delivery_id: Uuid,
    /// Вид шапки.
    pub kind: DigestKind,
    /// День сводки из ключа доставки.
    pub day: NaiveDate,
    /// Пояс получателя для подписей сроков.
    pub timezone: Tz,
    /// Состав сводки получателя.
    pub scope: DigestScope,
    /// Получатель: для меток исполнителя и имён.
    pub recipient_id: Uuid,
    /// Видимые строки в порядке снимка (фильтр состава уже применён).
    pub lines: Vec<DigestViewLine>,
    /// Число открытых позиций покупок.
    pub shopping_open: i64,
    /// Список задач для нижней кнопки.
    pub tasks_list: Option<Uuid>,
    /// Список покупок для нижней кнопки.
    pub shopping_list: Option<Uuid>,
}

/// Подпись срока строки сводки относительно дня сводки, а не «сейчас»:
/// вседневная задача дня сводки — «весь день», timed-задача того же дня —
/// «ЧЧ:ММ», остальные — как [`ru::due_label`] / [`ru::date_label`] от дня.
fn digest_due_label(
    due_at: Option<DateTime<Utc>>,
    due_date: Option<NaiveDate>,
    timezone: Tz,
    day: NaiveDate,
) -> String {
    match (due_at, due_date) {
        (Some(due_at), None) => {
            let local = due_at.with_timezone(&timezone);
            if local.date_naive() == day {
                local.format("%H:%M").to_string()
            } else {
                format!(
                    "{}, {}",
                    ru::date_label(local.date_naive(), day),
                    local.format("%H:%M")
                )
            }
        }
        (None, Some(due_date)) if due_date == day => ru::ALL_DAY.to_owned(),
        (None, Some(due_date)) => ru::date_label(due_date, day),
        _ => String::new(),
    }
}

/// Текст строки сводки без кнопки.
fn digest_line_text(line: &DigestViewLine, view: &DigestView) -> String {
    match &line.state {
        DigestLineState::Completed {
            title,
            actor_name,
            next_due_at,
            next_due_date,
            ..
        } => {
            let mut text = format!("✅ {}", clip(title, 40));
            if let Some(name) = actor_name {
                text.push_str(&format!(" · {name}"));
            }
            let next = digest_due_label(*next_due_at, *next_due_date, view.timezone, view.day);
            if !next.is_empty() {
                text.push_str(&format!(" · следующее {next}"));
            }
            text
        }
        DigestLineState::Deleted { title } => format!("{} — удалена", clip(title, 40)),
        DigestLineState::Archived { title } => format!("{} — в архиве", clip(title, 40)),
        DigestLineState::Event { title, when } => {
            if when.is_empty() {
                format!("🗓 {}", clip(title, 60))
            } else {
                format!("🗓 {} · {}", clip(title, 60), when)
            }
        }
        DigestLineState::EventDeleted { title } => format!("{} — удалено", clip(title, 60)),
        DigestLineState::Open { .. } => unreachable!("open tasks are buttons"),
    }
}

/// Подпись кнопки открытой задачи: «☐ <название до 40> · <срок>[ · 👤 Имя | · 👥]».
fn digest_open_label(line: &DigestViewLine, view: &DigestView) -> String {
    let DigestLineState::Open {
        title,
        due_at,
        due_date,
        assignee,
        assignee_name,
        ..
    } = &line.state
    else {
        unreachable!("open task");
    };
    let mut label = format!("☐ {}", clip(title, 40));
    let due = digest_due_label(*due_at, *due_date, view.timezone, view.day);
    if !due.is_empty() {
        label.push_str(&format!(" · {due}"));
    }
    match view.scope {
        // «Все в семье»: назначенная другому — «👤 Имя»; имя уже разрешено
        // по правилу member_name (не получатель, не исключённый).
        DigestScope::All => {
            if let Some(name) = assignee_name {
                label.push_str(&format!(" · 👤 {name}"));
            }
        }
        // «Только мои»: общая задача — «👥»; чужих здесь нет, их скрыл фильтр.
        DigestScope::Mine => {
            if assignee.is_none() {
                label.push_str(" · 👥");
            }
        }
    }
    label
}

/// Утренняя сводка из снимка и текущего состояния (0019, design, решения 1, 4, 5).
///
/// Чистая отрисовка: не ходит в БД и не знает «сейчас» — все подписи
/// относительны к дню сводки из представления. Показываются первые 20 видимых
/// строк в порядке снимка; итоги и «Выполнено X из N» считаются по всем
/// видимым строкам.
pub fn digest_ui(view: &DigestView) -> Ui {
    const MAX_LINES: usize = 20;
    let date = ru::fixed_date_label(view.day, view.day);
    let heading = match view.kind {
        DigestKind::Morning => format!("☀️ Доброе утро! · {date}"),
        DigestKind::Manual => format!("☀️ На сегодня · {date}"),
    };
    let mut ui = Ui::new().heading(heading);

    let visible = &view.lines;
    let tasks_total = i64::try_from(
        visible
            .iter()
            .filter(|l| l.section != DigestSection::Event)
            .count(),
    )
    .unwrap_or(0);
    let events_total = i64::try_from(
        visible
            .iter()
            .filter(|l| l.section == DigestSection::Event)
            .count(),
    )
    .unwrap_or(0);
    let done_total = visible
        .iter()
        .filter(|l| matches!(l.state, DigestLineState::Completed { .. }))
        .count();

    if visible.is_empty() {
        ui = ui.text("День свободен.");
    } else {
        let mut totals = Vec::new();
        if tasks_total > 0 {
            totals.push(ru::tasks(tasks_total));
        }
        if events_total > 0 {
            totals.push(ru::events(events_total));
        }
        ui = ui.text(totals.join(" · "));
        if done_total > 0 {
            ui = ui.text(format!("Выполнено {done_total} из {tasks_total}"));
        }

        let shown = visible.len().min(MAX_LINES);
        let mut section: Option<&str> = None;
        let mut texts: Vec<String> = Vec::new();
        for line in &visible[..shown] {
            let title = match line.section {
                DigestSection::Today | DigestSection::Event => "Сегодня",
                DigestSection::Overdue => "Просрочено",
            };
            if section != Some(title) {
                if !texts.is_empty() {
                    ui = ui.text(texts.join("\n"));
                    texts.clear();
                }
                ui = ui.bold(title);
                section = Some(title);
            }
            let toggle = |version: i32| Action::DigestToggle {
                delivery_id: view.delivery_id,
                item_id: line.item_id,
                version,
            };
            match &line.state {
                DigestLineState::Open { version, .. } => {
                    if !texts.is_empty() {
                        ui = ui.text(texts.join("\n"));
                        texts.clear();
                    }
                    ui = ui.row(vec![button(
                        digest_open_label(line, view),
                        toggle(*version),
                    )]);
                }
                DigestLineState::Completed {
                    version,
                    can_undo: true,
                    ..
                } => {
                    if !texts.is_empty() {
                        ui = ui.text(texts.join("\n"));
                        texts.clear();
                    }
                    ui = ui.row(vec![button(digest_line_text(line, view), toggle(*version))]);
                }
                _ => texts.push(digest_line_text(line, view)),
            }
        }
        if !texts.is_empty() {
            ui = ui.text(texts.join("\n"));
        }
        if visible.len() > shown {
            let hidden = &visible[shown..];
            let rest = i64::try_from(hidden.len()).unwrap_or(0);
            let has_event = hidden
                .iter()
                .any(|line| line.section == DigestSection::Event);
            // Смешанный хвост не называем задачами: в нём могут быть события.
            ui = ui.text(if has_event {
                format!("…и ещё {rest}")
            } else {
                format!("…и ещё {}", ru::tasks(rest))
            });
        }
    }

    let mut row = Vec::new();
    if let Some(list_id) = view.tasks_list {
        row.push(back_button(
            "📋 Задачи",
            Screen::List {
                list_id,
                page: 0,
                edit: false,
                from_lists: false,
            },
        ));
    }
    if let Some(list_id) = view.shopping_list {
        row.push(back_button(
            &format!("🛒 Покупки · {}", view.shopping_open),
            Screen::List {
                list_id,
                page: 0,
                edit: false,
                from_lists: false,
            },
        ));
    }
    if visible.len() > MAX_LINES {
        // 0009-events 8.2: скрытые события открываются в повестке, а скрытые
        // задачи — в списке задач. Если хвост смешанный, важнее события: в
        // списке задач их не видно.
        let hidden_event = visible[MAX_LINES..]
            .iter()
            .any(|line| line.section == DigestSection::Event);
        let more = if hidden_event || view.tasks_list.is_none() {
            Some(Screen::Agenda {
                list: None,
                page: 0,
            })
        } else {
            view.tasks_list.map(|list_id| Screen::List {
                list_id,
                page: 0,
                edit: false,
                from_lists: false,
            })
        };
        if let Some(target) = more {
            row.push(back_button("Показать ещё", target));
        }
    }
    ui.row(row)
        .row(vec![button(ru::DISMISS_BUTTON, Action::NoticeClose)])
}

/// Экран после действия над уведомлением.
pub fn done_ui(item: &Item, list_id: Uuid, next: Option<&str>) -> Ui {
    let mut ui = Ui::new().heading(format!("✅ Выполнено: {}", clip(&item.title, 100)));
    if let Some(next) = next {
        ui = ui.text(format!("Следующее: {next}"));
    }
    ui.row(vec![
        button("↩️ Отменить", Action::NoticeUndo { item_id: item.id }),
        back_button(
            ru::OPEN_LIST_BUTTON,
            Screen::List {
                list_id,
                page: 0,
                edit: false,
                from_lists: false,
            },
        ),
    ])
}

/// Экран «Задача больше недоступна».
fn gone_ui() -> Ui {
    Ui::new().heading("Задача больше недоступна").row(vec![
        back_button(ru::MENU_BUTTON, Screen::Menu),
        button(ru::DISMISS_BUTTON, Action::NoticeClose),
    ])
}

/// Имя участника для закрытого уведомления (0018, решение 5).
///
/// `member` — сведения об участнике, загруженные из household получателя
/// (`users::get(household получателя, …)` возвращает только активного
/// участника этого household): вызывающий обязан передать именно такого
/// участника, как в журнале выполнений. Имя не показывается, если выполнивший —
/// сам получатель, если выполнение без участника (`actor_user_id` пуст: HA,
/// интеграции) или если участник исключён из household / удалён (`member` пуст
/// или помечен удалённым). Формат без рода и склонения — отображаемое имя.
/// Тем же правилом пользуется имя исполнителя в «👤 Назначено»: если
/// исполнителя нельзя показать (исключён/удалён), имя не выводится.
pub(crate) fn member_name(
    actor_user_id: Option<Uuid>,
    recipient_user_id: Uuid,
    member: Option<&User>,
) -> Option<String> {
    let actor = actor_user_id?;
    if actor == recipient_user_id {
        return None;
    }
    let member = member?;
    if member.deleted_at.is_some() {
        return None;
    }
    Some(member.display_name.clone())
}

/// Экран «✅ Выполнено: <название>[ · <имя>]» — уведомление, закрытое
/// выполнением задачи не из него (0018, решение 4). Единственная кнопка —
/// «🗑 Убрать»: отменить выполнение из такого сообщения нельзя. Название
/// обрезается, как в [`done_ui`]. Вызывается шагом синхронизации.
pub(crate) fn completed_ui(title: &str, actor_name: Option<&str>) -> Ui {
    let mut heading = format!("✅ Выполнено: {}", clip(title, 100));
    if let Some(name) = actor_name {
        heading.push_str(&format!(" · {name}"));
    }
    Ui::new()
        .heading(heading)
        .row(vec![button(ru::DISMISS_BUTTON, Action::NoticeClose)])
}

/// Экран «👤 Назначено: <название>[ · <имя исполнителя>]» — уведомление,
/// закрытое назначением задачи другому участнику (0018, решение 4).
/// Единственная кнопка — «🗑 Убрать». Если имя исполнителя показать нельзя
/// (исключён/удалён), заголовок без имени. Название обрезается, как в
/// [`done_ui`]. Вызывается шагом синхронизации.
pub(crate) fn assigned_ui(title: &str, assignee_name: Option<&str>) -> Ui {
    let mut heading = format!("👤 Назначено: {}", clip(title, 100));
    if let Some(name) = assignee_name {
        heading.push_str(&format!(" · {name}"));
    }
    Ui::new()
        .heading(heading)
        .row(vec![button(ru::DISMISS_BUTTON, Action::NoticeClose)])
}

/// Собрать HTML уведомления для получателя: выпускает кнопки с областью
/// `notice`. `issued_at` — момент доставки, от него отсчитывается срок
/// действия кнопок.
pub async fn render_notice(
    conn: &mut PgConnection,
    telegram_user_id: i64,
    household_id: Uuid,
    ui: &Ui,
    issued_at: DateTime<Utc>,
) -> Result<String, sqlx::Error> {
    issue_and_render(
        conn,
        telegram_user_id,
        Some(household_id),
        ui,
        Scope::Notice,
        issued_at,
    )
    .await
}

/// Отпечаток отрисовки без токенов (design, решение 6): SHA-256 в hex от
/// текста экрана и подписей/действий его кнопок. Токены выпускаются при
/// [`render_notice`] и меняются при каждой отрисовке, поэтому в отпечаток не
/// входят: одинаковый `Ui` всегда даёт одинаковый отпечаток. По нему шаг
/// `sync_digests` и обработчик касания сводки решают, нужна ли правка
/// сообщения, а запись `digest_hash` хранит отпечаток последней отрисовки.
pub fn ui_fingerprint(ui: &Ui) -> String {
    let blocks: Vec<serde_json::Value> = ui
        .blocks
        .iter()
        .map(|block| match block {
            Block::Heading(text) => serde_json::json!({ "heading": text }),
            Block::Paragraph(spans) => serde_json::json!({
                "paragraph": spans
                    .iter()
                    .map(|span| match span {
                        Span::Text(text) => serde_json::json!({ "text": text }),
                        Span::Bold(text) => serde_json::json!({ "bold": text }),
                        Span::Italic(text) => serde_json::json!({ "italic": text }),
                        Span::Code(text) => serde_json::json!({ "code": text }),
                    })
                    .collect::<Vec<_>>()
            }),
            Block::Rule => serde_json::json!({ "rule": true }),
            Block::Details { summary, body } => {
                serde_json::json!({ "details": [summary, body] })
            }
            Block::Quote(text) => serde_json::json!({ "quote": text }),
            Block::CollapsedQuote(text) => serde_json::json!({ "collapsed_quote": text }),
            Block::Buttons(buttons) => serde_json::json!({
                "buttons": buttons
                    .iter()
                    .map(|button| serde_json::json!({
                        "label": button.label,
                        "action": button.action,
                    }))
                    .collect::<Vec<_>>()
            }),
        })
        .collect();
    let canonical = serde_json::to_string(&blocks).expect("fingerprint blocks serialize");
    Sha256::digest(canonical.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

async fn load(ctx: &mut Ctx<'_>, item_id: Uuid) -> Result<Option<(Item, List)>, Fail> {
    let household_id = ctx.household()?.id;
    match items::get(&mut *ctx.conn, household_id, item_id).await {
        Ok(item) => {
            let list = lists::get(&mut *ctx.conn, household_id, item.list_id).await?;
            Ok(Some((item, list)))
        }
        Err(DomainError::NotFound) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Заменить содержимое сообщения-уведомления.
pub(super) async fn edit_notice(ctx: &mut Ctx<'_>, message_id: i64, ui: &Ui) -> Result<(), Fail> {
    let household_id = ctx.household()?.id;
    let html = render_notice(&mut *ctx.conn, ctx.sender.id, household_id, ui, ctx.now).await?;
    outbox::enqueue(
        &mut *ctx.conn,
        ctx.chat_id,
        &Outgoing::Edit {
            message_id,
            html,
            resend_if_gone: true,
        },
        None,
        ctx.now,
    )
    .await?;
    Ok(())
}

/// Показать «✅ Выполнено» в сообщении-уведомлении и запланировать его
/// автоудаление (0017, решение 4).
///
/// Сообщение удаляется само через [`items::UNDO_WINDOW_MINUTES`] после того,
/// как стало «Выполнено»: отложенное `cleanup`-удаление с ключом
/// [`outbox::notice_expire_key`]. Прежние запланированные удаления этого
/// сообщения не отменяются — повторное «Выполнено» после отмены ставит новое
/// удаление, а удаление уже удалённого сообщения считается доставленным
/// (`MessageGone`). «↩️ Отменить» отменяет удаление через
/// [`outbox::cancel_scheduled_deletes`].
async fn show_done(
    ctx: &mut Ctx<'_>,
    message_id: i64,
    item: &Item,
    list_id: Uuid,
    next: Option<&str>,
) -> Result<(), Fail> {
    let ui = done_ui(item, list_id, next);
    edit_notice(ctx, message_id, &ui).await?;
    outbox::enqueue_at(
        &mut *ctx.conn,
        ctx.chat_id,
        &Outgoing::Delete {
            message_id,
            cleanup: true,
        },
        Some(&outbox::notice_expire_key(ctx.chat_id, message_id, ctx.now)),
        ctx.now,
        ctx.now + Duration::minutes(items::UNDO_WINDOW_MINUTES),
    )
    .await?;
    Ok(())
}

/// Показать актуальное состояние задачи в сообщении-уведомлении.
async fn show_current(ctx: &mut Ctx<'_>, message_id: i64, item_id: Uuid) -> Result<(), Fail> {
    match load(ctx, item_id).await? {
        Some((item, list)) if item.status == ItemStatus::Open => {
            let ui = reminder_ui(&item, &list, ctx.tz, ctx.now, None);
            edit_notice(ctx, message_id, &ui).await
        }
        Some((item, list)) => show_done(ctx, message_id, &item, list.id, None).await,
        None => edit_notice(ctx, message_id, &gone_ui()).await,
    }
}

/// Отметить доставки нажатого сообщения закрытыми по последнему действующему
/// выполнению задачи (0018, решение 4). Если выполнения нет (не должно
/// случиться), отметка не пишется.
async fn mark_closed_by_completion(
    ctx: &mut Ctx<'_>,
    household_id: Uuid,
    item_id: Uuid,
    message_id: i64,
) -> Result<(), Fail> {
    let Some(event) = completions::latest_for_item(&mut *ctx.conn, household_id, item_id).await?
    else {
        return Ok(());
    };
    notifications::mark_closed_completed(
        &mut *ctx.conn,
        ctx.chat_id,
        message_id,
        event.completion.event_id,
        ctx.now,
    )
    .await?;
    Ok(())
}

/// «Выполнено» из уведомления.
///
/// Доставки нажатого сообщения блокируются до выполнения и в той же
/// транзакции отмечаются закрытыми по выполнению (0018, решение 4): шаг
/// синхронизации не трогает сообщение, которое уже показывает «✅ Выполнено»
/// (`show_done`).
pub async fn complete(
    ctx: &mut Ctx<'_>,
    message_id: i64,
    item_id: Uuid,
    version: i32,
) -> Result<(), Fail> {
    let household = ctx.household()?.clone();
    let actor = ctx.actor()?;
    notifications::lock_message_deliveries(&mut *ctx.conn, ctx.chat_id, message_id).await?;
    let result = items::complete(
        &mut *ctx.conn,
        household.id,
        &actor,
        item_id,
        Some(version),
        &household.timezone,
        ctx.now,
    )
    .await;
    match result {
        Ok(item) => {
            let next = if item.status == ItemStatus::Open {
                ru::due_label(item.due_at, item.due_date, ctx.tz, ctx.now)
            } else {
                None
            };
            mark_closed_by_completion(ctx, household.id, item_id, message_id).await?;
            ctx.toast(ru::DONE_TOAST);
            show_done(ctx, message_id, &item, item.list_id, next.as_deref()).await
        }
        Err(DomainError::Conflict { .. } | DomainError::NotFound) => {
            // Задачу успели изменить: кнопки обновляются, действие не повторяется.
            let loaded = load(ctx, item_id).await?;
            let toast = match &loaded {
                None => "Задача больше недоступна",
                Some((item, _)) if item.status == ItemStatus::Done => "Задача уже выполнена",
                Some(_) => "Задача уже изменилась",
            };
            if matches!(&loaded, Some((item, _)) if item.status == ItemStatus::Done) {
                // Сообщение покажет «✅ Выполнено» — закрываем его и здесь,
                // иначе шаг синхронизации перерисует его второй раз.
                mark_closed_by_completion(ctx, household.id, item_id, message_id).await?;
            }
            ctx.toast(toast);
            show_current(ctx, message_id, item_id).await
        }
        Err(error) => Err(error.into()),
    }
}

/// Отменить завершение из уведомления.
///
/// При успешной отмене сообщение снова показывает напоминание, поэтому
/// запланированное автоудаление «✅ Выполнено» отменяется (0017, решение 4):
/// будущие `pending`-удаления этого сообщения переходят в `skipped`. Неудачная
/// отмена («Время отмены истекло») удаление не отменяет. Отметка закрытия
/// доставок этого сообщения снимается только при успешной отмене (0018,
/// решение 4): шаг синхронизации снова может закрыть возвращённое напоминание.
pub async fn undo(ctx: &mut Ctx<'_>, message_id: i64, item_id: Uuid) -> Result<(), Fail> {
    let household_id = ctx.household()?.id;
    let actor = ctx.actor()?;
    notifications::lock_message_deliveries(&mut *ctx.conn, ctx.chat_id, message_id).await?;
    match items::undo_last(&mut *ctx.conn, household_id, &actor, item_id, ctx.now).await {
        Ok(_) => {
            notifications::clear_closed(&mut *ctx.conn, ctx.chat_id, message_id).await?;
            ctx.toast("Отменено");
            outbox::cancel_scheduled_deletes(&mut *ctx.conn, ctx.chat_id, message_id, ctx.now)
                .await?;
        }
        Err(DomainError::Database(error)) => return Err(Fail::Infra(error)),
        Err(_) => ctx.toast("Время отмены истекло"),
    }
    show_current(ctx, message_id, item_id).await
}

/// «Открыть» из уведомления: карточка задачи в экране навигации.
pub(super) async fn open(ctx: &mut Ctx<'_>, message_id: i64, item_id: Uuid) -> Result<(), Fail> {
    if load(ctx, item_id).await?.is_none() {
        ctx.toast("Задача больше недоступна");
        return show_current(ctx, message_id, item_id).await;
    }
    ctx.present(Screen::Item {
        item_id,
        back: Box::new(Screen::Menu),
        completion_id: None,
    })
    .await
}

/// «🗑 Убрать» из любого уведомления: удалить сообщение из чата.
///
/// Удаление — уборка (`cleanup = true`): отказ Telegram не засыпает вид чата.
/// В той же транзакции записи доставки этого сообщения отмечаются
/// `dismissed_at`: их статус остаётся `sent`, поэтому напоминание о том же
/// вхождении повторно не планируется, а правки отправленных сообщений
/// (0018, 0019) убранное не трогают. Задачу, событие и отложенные
/// snooze-доставки уборка не меняет; о следующем вхождении напоминание
/// придёт по расписанию.
pub async fn close(ctx: &mut Ctx<'_>, message_id: i64) -> Result<(), Fail> {
    outbox::enqueue(
        &mut *ctx.conn,
        ctx.chat_id,
        &Outgoing::Delete {
            message_id,
            cleanup: true,
        },
        Some(&super::delete_dedupe_key(ctx.chat_id, message_id)),
        ctx.now,
    )
    .await?;
    notifications::mark_dismissed(&mut *ctx.conn, ctx.chat_id, message_id, ctx.now).await?;
    Ok(())
}

/// Открыть экран быстрого срока события из уведомления или сводного сообщения.
pub(super) async fn open_due(ctx: &mut Ctx<'_>, item_id: Uuid) -> Result<(), Fail> {
    ctx.present(Screen::Due {
        item_id,
        back: Box::new(Screen::Menu),
    })
    .await
}

/// «📅 Перенести» из напоминания о событии.
pub async fn event_due(ctx: &mut Ctx<'_>, item_id: Uuid) -> Result<(), Fail> {
    if load(ctx, item_id).await?.is_none() {
        ctx.toast("Событие больше недоступно");
        return Ok(());
    }
    open_due(ctx, item_id).await
}

pub(super) fn snooze_time(
    until: SnoozeUntil,
    tz: Tz,
    now: DateTime<Utc>,
) -> Result<DateTime<Utc>, DomainError> {
    let at = match until {
        SnoozeUntil::OneHour => now + Duration::hours(1),
        SnoozeUntil::TomorrowMorning => {
            let tomorrow = now.with_timezone(&tz).date_naive() + Duration::days(1);
            let morning =
                NaiveTime::from_hms_opt(MORNING.0, MORNING.1, 0).unwrap_or(NaiveTime::MIN);
            resolve_local(tz, tomorrow, morning)?
        }
    };
    // Ключ snooze хранит время с точностью до секунды; усечём и `scheduled_at`,
    // чтобы первому тику не пришлось перезаписывать его из-за долей секунды.
    Ok(DateTime::<Utc>::from_timestamp(at.timestamp(), 0).unwrap_or(at))
}

/// Поставить snooze-доставку задачи на `deliver_at`.
///
/// `batched` — групповой snooze: несколько таких задач приходят одним сводным
/// сообщением. `replaces_message_id` — сообщение «😴 Напомню …», которое
/// заменит эта доставка; у snooze из сводного сообщения пусто. Ключ включает
/// время snooze, поэтому повторное нажатие той же кнопки не создаёт вторую
/// доставку, а планировщик не путает её с вхождением срока.
///
/// Затёртый другим значением прежний id удаляется сразу: эта доставка его
/// уже не заменит (design 0017, решение 5).
pub(super) async fn insert_snooze(
    ctx: &mut Ctx<'_>,
    item_id: Uuid,
    origin_key: &str,
    deliver_at: DateTime<Utc>,
    batched: bool,
    replaces_message_id: Option<i64>,
) -> Result<(), Fail> {
    let occurrence_key = notifications::snooze_key(deliver_at);
    let delivery = NewDelivery {
        household_id: ctx.household()?.id,
        user_id: ctx.user()?.id,
        item_id: Some(item_id),
        kind: NotificationKind::Snooze,
        occurrence_key: &occurrence_key,
        origin_key: Some(origin_key),
        replaces_message_id,
        scheduled_at: deliver_at,
    };
    let overwritten =
        notifications::insert_snooze(&mut *ctx.conn, &delivery, batched, ctx.now).await?;
    if let Some(old) = overwritten {
        outbox::enqueue(
            &mut *ctx.conn,
            ctx.chat_id,
            &Outgoing::Delete {
                message_id: old,
                cleanup: true,
            },
            Some(&outbox::replace_key(ctx.chat_id, old)),
            ctx.now,
        )
        .await?;
        notifications::mark_dismissed(&mut *ctx.conn, ctx.chat_id, old, ctx.now).await?;
    }
    Ok(())
}

/// Экран «😴 Напомню …» после откладывания напоминания.
fn snoozed_ui(heading: String, title: &str, item_id: Uuid) -> Ui {
    Ui::new().heading(heading).text(clip(title, 120)).row(vec![
        button("📋 Открыть", Action::NoticeOpen { item_id }),
        button(ru::DISMISS_BUTTON, Action::NoticeClose),
    ])
}

/// «Отложить»: создаёт отдельную доставку, которую отправит планировщик.
pub async fn snooze(
    ctx: &mut Ctx<'_>,
    message_id: i64,
    item_id: Uuid,
    until: SnoozeUntil,
    version: Option<i32>,
) -> Result<(), Fail> {
    let Some((item, _)) = load(ctx, item_id).await? else {
        ctx.toast("Задача больше недоступна");
        return show_current(ctx, message_id, item_id).await;
    };
    if item.status == ItemStatus::Done {
        ctx.toast("Задача уже выполнена");
        return show_current(ctx, message_id, item_id).await;
    }
    if version.is_some_and(|version| version != item.version) {
        ctx.toast("Задача уже изменилась");
        return show_current(ctx, message_id, item_id).await;
    }

    let deliver_at = snooze_time(until, ctx.tz, ctx.now)?;
    let origin_key = snooze_origin(item.due_at, item.due_date);
    insert_snooze(
        ctx,
        item_id,
        &origin_key,
        deliver_at,
        false,
        Some(message_id),
    )
    .await?;

    let local = deliver_at.with_timezone(&ctx.tz);
    ctx.toast("Отложено");
    let text = format!(
        "😴 Напомню {}",
        ru::due_label(Some(deliver_at), None, ctx.tz, ctx.now)
            .unwrap_or_else(|| local.format("%H:%M").to_string())
    );
    let ui = snoozed_ui(text, &item.title, item_id);
    edit_notice(ctx, message_id, &ui).await
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;
    use crate::domain::models::{ItemPriority, ItemSource, User, UserRole};
    use crate::telegram::rich::{Block, Button};

    #[test]
    fn snooze_options_resolve_in_local_time() {
        let tz: Tz = "Europe/Moscow".parse().expect("tz");
        let now = Utc.with_ymd_and_hms(2026, 10, 30, 20, 30, 0).unwrap();

        assert_eq!(
            snooze_time(SnoozeUntil::OneHour, tz, now).expect("time"),
            Utc.with_ymd_and_hms(2026, 10, 30, 21, 30, 0).unwrap()
        );
        // 20:30 UTC — 23:30 30 октября в Москве, «завтра» — 31 октября, 09:00.
        assert_eq!(
            snooze_time(SnoozeUntil::TomorrowMorning, tz, now).expect("time"),
            Utc.with_ymd_and_hms(2026, 10, 31, 6, 0, 0).unwrap()
        );
        // 22:30 UTC — уже 01:30 31 октября в Москве, «завтра» — 1 ноября.
        let late = Utc.with_ymd_and_hms(2026, 10, 30, 22, 30, 0).unwrap();
        assert_eq!(
            snooze_time(SnoozeUntil::TomorrowMorning, tz, late).expect("time"),
            Utc.with_ymd_and_hms(2026, 11, 1, 6, 0, 0).unwrap()
        );
    }

    #[test]
    fn snooze_time_is_truncated_to_whole_seconds() {
        let tz: Tz = "Europe/Moscow".parse().expect("tz");
        let now =
            Utc.with_ymd_and_hms(2026, 10, 30, 20, 30, 0).unwrap() + Duration::milliseconds(750);

        let at = snooze_time(SnoozeUntil::OneHour, tz, now).expect("time");
        assert_eq!(at.timestamp_subsec_nanos(), 0);
        assert_eq!(at, Utc.with_ymd_and_hms(2026, 10, 30, 21, 30, 0).unwrap());
    }

    fn digest_day() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, 30).expect("digest day")
    }

    fn digest_view(lines: Vec<DigestViewLine>) -> DigestView {
        DigestView {
            delivery_id: Uuid::now_v7(),
            kind: DigestKind::Morning,
            day: digest_day(),
            timezone: "Europe/Moscow".parse().expect("tz"),
            scope: DigestScope::All,
            recipient_id: Uuid::now_v7(),
            lines,
            shopping_open: 0,
            tasks_list: Some(Uuid::now_v7()),
            shopping_list: None,
        }
    }

    fn open_task(title: &str) -> DigestViewLine {
        DigestViewLine {
            item_id: Uuid::now_v7(),
            section: DigestSection::Today,
            state: DigestLineState::Open {
                title: title.into(),
                due_at: None,
                due_date: Some(digest_day()),
                version: 1,
                assignee: None,
                assignee_name: None,
            },
        }
    }

    fn completed_task(title: &str, actor_name: Option<&str>, can_undo: bool) -> DigestViewLine {
        DigestViewLine {
            item_id: Uuid::now_v7(),
            section: DigestSection::Today,
            state: DigestLineState::Completed {
                title: title.into(),
                actor_name: actor_name.map(str::to_owned),
                next_due_at: None,
                next_due_date: None,
                version: 2,
                can_undo,
            },
        }
    }

    fn event_line(title: &str, when: &str) -> DigestViewLine {
        DigestViewLine {
            item_id: Uuid::now_v7(),
            section: DigestSection::Event,
            state: DigestLineState::Event {
                title: title.into(),
                when: when.into(),
            },
        }
    }

    fn digest_html(ui: &Ui) -> String {
        let tokens = vec!["t:AAAAAAAAAAAAAAAA".to_owned(); ui.button_count()];
        crate::telegram::rich::render(ui, &tokens).html
    }

    fn find_button<'a>(ui: &'a Ui, label: &str) -> &'a Button<Action> {
        ui.blocks
            .iter()
            .find_map(|block| match block {
                Block::Buttons(buttons) => buttons.iter().find(|button| button.label == label),
                _ => None,
            })
            .unwrap_or_else(|| panic!("button {label:?} not found"))
    }

    #[test]
    fn digest_is_limited_to_twenty_lines() {
        let view = digest_view((0..25).map(|n| open_task(&format!("Задача {n}"))).collect());
        let ui = digest_ui(&view);
        let html = digest_html(&ui);

        assert_eq!(html.matches("☐ Задача").count(), 20);
        assert!(html.contains("…и ещё 5 задач"));
        assert!(html.contains("Показать ещё"));
    }

    #[test]
    fn digest_limit_counts_events_and_tasks_together() {
        let mut lines: Vec<DigestViewLine> =
            (0..15).map(|n| open_task(&format!("Задача {n}"))).collect();
        lines.extend((0..10).map(|n| event_line(&format!("Событие {n}"), "14:00–15:00")));
        let view = digest_view(lines);
        let ui = digest_ui(&view);
        let html = digest_html(&ui);

        assert_eq!(html.matches("☐ Задача").count(), 15);
        assert_eq!(html.matches("🗓 Событие").count(), 5);
        assert!(html.contains("…и ещё 5"), "{html}");
        assert!(!html.contains("…и ещё 5 задач"), "{html}");
        assert!(html.contains("Показать ещё"), "{html}");
    }

    #[test]
    fn digest_show_more_opens_the_agenda_when_events_are_hidden() {
        let mut lines: Vec<DigestViewLine> =
            (0..15).map(|n| open_task(&format!("Задача {n}"))).collect();
        lines.extend((0..10).map(|n| event_line(&format!("Событие {n}"), "14:00–15:00")));
        let view = digest_view(lines);
        let ui = digest_ui(&view);
        let more = find_button(&ui, "Показать ещё");
        assert!(matches!(
            more.action,
            Action::Go {
                to: Screen::Agenda {
                    list: None,
                    page: 0
                }
            }
        ));
    }

    #[test]
    fn digest_show_more_keeps_the_task_list_when_only_tasks_are_hidden() {
        let view = digest_view((0..25).map(|n| open_task(&format!("Задача {n}"))).collect());
        let list_id = view.tasks_list.expect("tasks list");
        let ui = digest_ui(&view);
        let more = find_button(&ui, "Показать ещё");
        assert!(matches!(
            more.action,
            Action::Go {
                to: Screen::List { list_id: id, .. }
            } if id == list_id
        ));
    }

    #[test]
    fn empty_digest_says_the_day_is_free() {
        let mut view = digest_view(Vec::new());
        view.tasks_list = None;
        let ui = digest_ui(&view);
        let html = digest_html(&ui);
        assert!(html.contains("День свободен."));
        assert!(!html.contains("Сегодня"));
        assert!(!html.contains("Просрочено"));
        assert!(!html.contains("Покупки"));
    }

    #[test]
    fn digest_with_only_shopping_keeps_the_purchase_count() {
        let mut view = digest_view(Vec::new());
        view.tasks_list = None;
        view.shopping_open = 7;
        view.shopping_list = Some(Uuid::now_v7());
        let ui = digest_ui(&view);
        let html = digest_html(&ui);
        assert!(html.contains("День свободен."), "{html}");
        assert!(html.contains("🛒 Покупки · 7"), "{html}");
        assert!(!html.contains("задач"), "{html}");
    }

    #[test]
    fn digest_open_task_button_carries_the_toggle_action() {
        let view = digest_view(vec![open_task("Вынести мусор")]);
        let ui = digest_ui(&view);
        let rows = button_rows(&ui);
        assert_eq!(rows[0][0].label, "☐ Вынести мусор · весь день");
        assert!(matches!(
            rows[0][0].action,
            Action::DigestToggle { delivery_id, item_id, version: 1 }
                if delivery_id == view.delivery_id && item_id == view.lines[0].item_id
        ));
    }

    fn set_open_assignee(view: &mut DigestView, assignee: Option<Uuid>, name: Option<&str>) {
        let DigestLineState::Open {
            assignee: a,
            assignee_name: n,
            ..
        } = &mut view.lines[0].state
        else {
            panic!("open task");
        };
        *a = assignee;
        *n = name.map(str::to_owned);
    }

    #[test]
    fn digest_assignee_labels_follow_the_scope() {
        let recipient = Uuid::now_v7();
        let other = Uuid::now_v7();
        let mut view = digest_view(vec![open_task("Забрать посылку")]);
        view.recipient_id = recipient;

        // «Все в семье»: назначенная другому — «👤 Имя», своя и общая — без метки.
        set_open_assignee(&mut view, Some(other), Some("Маша"));
        let html = digest_html(&digest_ui(&view));
        assert!(
            html.contains("☐ Забрать посылку · весь день · 👤 Маша"),
            "{html}"
        );

        set_open_assignee(&mut view, Some(recipient), None);
        let html = digest_html(&digest_ui(&view));
        assert!(html.contains("☐ Забрать посылку · весь день"), "{html}");
        assert!(!html.contains("👤"), "{html}");

        set_open_assignee(&mut view, None, None);
        let html = digest_html(&digest_ui(&view));
        assert!(!html.contains("👤"), "{html}");
        assert!(!html.contains("👥"), "{html}");

        // «Только мои»: общая — «👥», своя — без метки.
        view.scope = DigestScope::Mine;
        let html = digest_html(&digest_ui(&view));
        assert!(
            html.contains("☐ Забрать посылку · весь день · 👥"),
            "{html}"
        );

        set_open_assignee(&mut view, Some(recipient), None);
        let html = digest_html(&digest_ui(&view));
        assert!(html.contains("☐ Забрать посылку · весь день"), "{html}");
        assert!(!html.contains("👥"), "{html}");
    }

    #[test]
    fn digest_completed_by_another_member_is_text_without_a_button() {
        let view = digest_view(vec![completed_task("Вынести мусор", Some("Маша"), false)]);
        let ui = digest_ui(&view);
        let html = digest_html(&ui);
        assert!(html.contains("✅ Вынести мусор · Маша"), "{html}");
        // Кнопки только нижние: строка выполненной задачи — текст.
        let rows = button_rows(&ui);
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert_eq!(row_labels(rows[0]), ["📋 Задачи"]);
    }

    #[test]
    fn digest_completed_by_another_member_is_a_button_for_the_owner() {
        let view = digest_view(vec![completed_task("Вынести мусор", Some("Маша"), true)]);
        let ui = digest_ui(&view);
        let rows = button_rows(&ui);
        assert_eq!(rows[0][0].label, "✅ Вынести мусор · Маша");
        assert!(matches!(
            rows[0][0].action,
            Action::DigestToggle { delivery_id, item_id, version: 2 }
                if delivery_id == view.delivery_id && item_id == view.lines[0].item_id
        ));
    }

    #[test]
    fn digest_completed_without_an_actor_has_no_name() {
        let view = digest_view(vec![completed_task("Вынести мусор", None, true)]);
        let ui = digest_ui(&view);
        let rows = button_rows(&ui);
        assert_eq!(rows[0][0].label, "✅ Вынести мусор");
    }

    #[test]
    fn digest_completed_recurring_shows_the_next_due() {
        let mut line = completed_task("Вынести мусор", None, true);
        let DigestLineState::Completed { next_due_date, .. } = &mut line.state else {
            panic!("completed");
        };
        *next_due_date = Some(digest_day() + Duration::days(1));
        let view = digest_view(vec![line]);
        let html = digest_html(&digest_ui(&view));
        assert!(
            html.contains("✅ Вынести мусор · следующее завтра"),
            "{html}"
        );
    }

    #[test]
    fn digest_deleted_and_archived_lines_are_text() {
        let view = digest_view(vec![
            DigestViewLine {
                item_id: Uuid::now_v7(),
                section: DigestSection::Today,
                state: DigestLineState::Deleted {
                    title: "Старая задача".into(),
                },
            },
            DigestViewLine {
                item_id: Uuid::now_v7(),
                section: DigestSection::Overdue,
                state: DigestLineState::Archived {
                    title: "Архивная задача".into(),
                },
            },
        ]);
        let ui = digest_ui(&view);
        let html = digest_html(&ui);
        assert!(html.contains("Старая задача — удалена"), "{html}");
        assert!(html.contains("Архивная задача — в архиве"), "{html}");
        let rows = button_rows(&ui);
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert_eq!(row_labels(rows[0]), ["📋 Задачи"]);
    }

    #[test]
    fn digest_event_keeps_the_snapshot_time() {
        let view = digest_view(vec![
            event_line("Бассейн", "07:00–08:00"),
            DigestViewLine {
                item_id: Uuid::now_v7(),
                section: DigestSection::Event,
                state: DigestLineState::EventDeleted {
                    title: "Встреча".into(),
                },
            },
        ]);
        let ui = digest_ui(&view);
        let html = digest_html(&ui);
        assert!(html.contains("🗓 Бассейн · 07:00–08:00"), "{html}");
        assert!(html.contains("Встреча — удалено"), "{html}");
        // События — строки без кнопок.
        let rows = button_rows(&ui);
        assert_eq!(rows.len(), 2, "{rows:?}");
    }

    #[test]
    fn digest_headings_follow_the_kind() {
        let view = digest_view(vec![open_task("Задача")]);
        assert_eq!(heading(&digest_ui(&view)), "☀️ Доброе утро! · пт, 30 окт");
        let mut manual = digest_view(vec![open_task("Задача")]);
        manual.kind = DigestKind::Manual;
        assert_eq!(heading(&digest_ui(&manual)), "☀️ На сегодня · пт, 30 окт");
    }

    #[test]
    fn digest_totals_count_visible_lines() {
        let mut lines = vec![
            open_task("Задача 1"),
            open_task("Задача 2"),
            completed_task("Задача 3", None, true),
            event_line("Событие", "14:00–15:00"),
        ];
        let view = digest_view(lines.clone());
        let html = digest_html(&digest_ui(&view));
        assert!(html.contains("3 задачи · 1 событие"), "{html}");
        assert!(html.contains("Выполнено 1 из 3"), "{html}");

        // Без выполненных строки «Выполнено» нет.
        lines[2] = open_task("Задача 3");
        let view = digest_view(lines);
        let html = digest_html(&digest_ui(&view));
        assert!(html.contains("3 задачи · 1 событие"), "{html}");
        assert!(!html.contains("Выполнено"), "{html}");
    }

    #[test]
    fn digest_empty_day_with_shopping_has_no_totals() {
        let mut view = digest_view(Vec::new());
        view.shopping_open = 2;
        view.shopping_list = Some(Uuid::now_v7());
        let ui = digest_ui(&view);
        let html = digest_html(&ui);
        assert_eq!(heading(&ui), "☀️ Доброе утро! · пт, 30 окт");
        assert!(html.contains("День свободен."), "{html}");
        assert!(!html.contains("задач"), "{html}");
        assert!(!html.contains("событи"), "{html}");
        assert!(html.contains("🛒 Покупки · 2"), "{html}");
    }

    #[test]
    fn digest_twenty_five_lines_with_the_first_completed() {
        let mut lines = vec![completed_task("Задача 0", None, true)];
        lines.extend((1..25).map(|n| open_task(&format!("Задача {n}"))));
        let view = digest_view(lines);
        let ui = digest_ui(&view);
        let html = digest_html(&ui);

        assert_eq!(html.matches("☐ Задача").count(), 19, "{html}");
        assert_eq!(html.matches("✅ Задача").count(), 1, "{html}");
        assert!(html.contains("…и ещё 5"), "{html}");
        assert!(html.contains("Выполнено 1 из 25"), "{html}");
        assert!(html.contains("Задача 19"), "{html}");
        assert!(!html.contains("Задача 20"), "{html}");
        // Выполненная строка осталась первой, на своём месте.
        assert!(
            html.find("✅ Задача 0").expect("completed") < html.find("☐ Задача 1").expect("open")
        );
    }

    #[test]
    fn digest_labels_are_relative_to_the_digest_day_not_now() {
        let day = digest_day();
        let tz: Tz = "Europe/Moscow".parse().expect("tz");
        let overdue = DigestViewLine {
            item_id: Uuid::now_v7(),
            section: DigestSection::Overdue,
            state: DigestLineState::Open {
                title: "Просроченная".into(),
                due_at: Some(
                    tz.from_local_datetime(
                        &(day - Duration::days(1)).and_hms_opt(12, 0, 0).unwrap(),
                    )
                    .unwrap()
                    .with_timezone(&Utc),
                ),
                due_date: None,
                version: 1,
                assignee: None,
                assignee_name: None,
            },
        };
        let mut recurring = completed_task("Ежедневная", None, true);
        let DigestLineState::Completed { next_due_date, .. } = &mut recurring.state else {
            panic!("completed");
        };
        *next_due_date = Some(day + Duration::days(1));
        let view = digest_view(vec![open_task("Сегодняшняя"), overdue, recurring]);

        // Отрисовка не принимает «сейчас»: подписи строятся от дня сводки,
        // поэтому повторная отрисовка того же представления даёт тот же
        // отпечаток, когда бы она ни произошла.
        let ui = digest_ui(&view);
        let html = digest_html(&ui);
        assert!(html.contains("☐ Сегодняшняя · весь день"), "{html}");
        assert!(html.contains("☐ Просроченная · вчера, 12:00"), "{html}");
        assert!(html.contains("✅ Ежедневная · следующее завтра"), "{html}");
        assert_eq!(ui_fingerprint(&ui), ui_fingerprint(&digest_ui(&view)));
    }

    #[test]
    fn ui_fingerprint_is_deterministic_hex() {
        let mut view = digest_view((0..3).map(|n| open_task(&format!("Задача {n}"))).collect());
        view.shopping_open = 2;
        view.shopping_list = Some(Uuid::now_v7());
        let ui = digest_ui(&view);

        let hash = ui_fingerprint(&ui);
        assert_eq!(hash.len(), 64, "{hash}");
        assert!(hash.chars().all(|ch| ch.is_ascii_hexdigit()), "{hash}");
        assert_eq!(hash, ui_fingerprint(&ui));
    }

    #[test]
    fn ui_fingerprint_changes_with_line_text_or_button_action() {
        let item_id = Uuid::now_v7();
        let base = Ui::new()
            .heading("☀️ Доброе утро!")
            .text("☐ Задача · сегодня")
            .row(vec![button("📋 Открыть", Action::NoticeOpen { item_id })]);
        let other_text = Ui::new()
            .heading("☀️ Доброе утро!")
            .text("☐ Другая задача · сегодня")
            .row(vec![button("📋 Открыть", Action::NoticeOpen { item_id })]);
        let other_action = Ui::new()
            .heading("☀️ Доброе утро!")
            .text("☐ Задача · сегодня")
            .row(vec![button("📋 Открыть", Action::NoticeClose)]);
        let other_label = Ui::new()
            .heading("☀️ Доброе утро!")
            .text("☐ Задача · сегодня")
            .row(vec![button("🗑 Убрать", Action::NoticeOpen { item_id })]);

        assert_ne!(ui_fingerprint(&base), ui_fingerprint(&other_text));
        assert_ne!(ui_fingerprint(&base), ui_fingerprint(&other_action));
        assert_ne!(ui_fingerprint(&base), ui_fingerprint(&other_label));
    }

    #[test]
    fn ui_fingerprint_does_not_depend_on_button_tokens() {
        let view = digest_view((0..3).map(|n| open_task(&format!("Задача {n}"))).collect());
        let ui = digest_ui(&view);

        let before = ui_fingerprint(&ui);
        // render_notice выпускает новые токены при каждой отрисовке: HTML
        // двух отрисовок одного Ui различается, а отпечаток — нет.
        let tokens = vec!["t:AAAAAAAAAAAAAAAA".to_owned(); ui.button_count()];
        let first = crate::telegram::rich::render(&ui, &tokens).html;
        let tokens = vec!["t:BBBBBBBBBBBBBBBB".to_owned(); ui.button_count()];
        let second = crate::telegram::rich::render(&ui, &tokens).html;
        assert_ne!(first, second);
        assert_eq!(before, ui_fingerprint(&ui));
    }

    #[test]
    fn ui_fingerprint_changes_when_a_digest_line_changes() {
        let view = digest_view(vec![open_task("Вынести мусор")]);
        let base = ui_fingerprint(&digest_ui(&view));

        let mut renamed = view.clone();
        let DigestLineState::Open { title, .. } = &mut renamed.lines[0].state else {
            panic!("open task");
        };
        *title = "Вынести мусор и пыль".into();
        assert_ne!(base, ui_fingerprint(&digest_ui(&renamed)));

        let mut completed = view.clone();
        completed.lines[0] = completed_task("Вынести мусор", None, true);
        completed.lines[0].item_id = view.lines[0].item_id;
        assert_ne!(base, ui_fingerprint(&digest_ui(&completed)));
    }

    fn notice_item(title: &str) -> Item {
        Item {
            id: Uuid::now_v7(),
            household_id: Uuid::nil(),
            list_id: Uuid::nil(),
            title: title.into(),
            notes: None,
            status: ItemStatus::Open,
            priority: ItemPriority::Normal,
            due_at: Some(Utc.with_ymd_and_hms(2026, 11, 23, 12, 0, 0).unwrap()),
            due_date: None,
            end_at: None,
            end_date: None,
            assignee_user_id: None,
            created_by_user_id: None,
            source: ItemSource::Telegram,
            capture_id: None,
            completed_at: None,
            completed_by_user_id: None,
            quantity: None,
            unit: None,
            position: 0,
            version: 1,
            recurrence: None,
            created_at: DateTime::UNIX_EPOCH,
            updated_at: DateTime::UNIX_EPOCH,
            deleted_at: None,
            tags: Vec::new(),
            subtasks: Vec::new(),
        }
    }

    fn notice_list(kind: ListKind, name: &str) -> List {
        List {
            id: Uuid::now_v7(),
            household_id: Uuid::nil(),
            kind,
            name: name.into(),
            is_default: true,
            is_archived: false,
            position: 0,
            created_at: DateTime::UNIX_EPOCH,
            updated_at: DateTime::UNIX_EPOCH,
            deleted_at: None,
        }
    }

    fn moscow_now() -> (Tz, DateTime<Utc>) {
        (
            "Europe/Moscow".parse().expect("tz"),
            Utc.with_ymd_and_hms(2026, 11, 23, 9, 0, 0).unwrap(),
        )
    }

    fn button_rows(ui: &Ui) -> Vec<&[Button<Action>]> {
        ui.blocks
            .iter()
            .filter_map(|block| match block {
                Block::Buttons(buttons) => Some(buttons.as_slice()),
                _ => None,
            })
            .collect()
    }

    fn row_labels(row: &[Button<Action>]) -> Vec<&str> {
        row.iter().map(|button| button.label.as_str()).collect()
    }

    #[test]
    fn task_reminder_has_dismiss_button_on_the_third_row() {
        let (tz, now) = moscow_now();
        let item = notice_item("Оплатить интернет");
        let list = notice_list(ListKind::Tasks, "Задачи");
        let ui = reminder_ui(&item, &list, tz, now, Some(15));

        let rows = button_rows(&ui);
        assert_eq!(rows.len(), 3, "{rows:?}");
        assert_eq!(row_labels(rows[0]), ["✓ Выполнено", "😴 Отложить на час"]);
        assert_eq!(row_labels(rows[1]), ["📅 Завтра в 09:00", "📋 Открыть"]);
        assert_eq!(row_labels(rows[2]), ["🗑 Убрать"]);
        assert!(matches!(rows[2][0].action, Action::NoticeClose));
    }

    #[test]
    fn event_reminder_replaces_close_with_dismiss() {
        let (tz, now) = moscow_now();
        let mut item = notice_item("Маникюр");
        item.end_at = Some(Utc.with_ymd_and_hms(2026, 11, 23, 13, 30, 0).unwrap());
        let list = notice_list(ListKind::Events, "События");
        let ui = reminder_ui(&item, &list, tz, now, Some(15));

        let rows = button_rows(&ui);
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert_eq!(row_labels(rows[0]), ["🗑 Убрать", "📅 Перенести"]);
        assert_eq!(row_labels(rows[1]), ["📋 Открыть"]);
        assert!(matches!(rows[0][0].action, Action::NoticeClose));
        assert!(
            !rows
                .iter()
                .flat_map(|row| row_labels(row))
                .any(|label| label.contains("Закрыть")),
            "{rows:?}"
        );
    }

    #[test]
    fn assignment_notice_has_dismiss_button_on_a_separate_row() {
        let (tz, now) = moscow_now();
        let item = notice_item("Купить торт");
        let list = notice_list(ListKind::Tasks, "Задачи");
        let ui = assignment_ui(&item, &list, tz, now);

        let rows = button_rows(&ui);
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert_eq!(row_labels(rows[0]), ["📋 Открыть", "✓ Выполнено"]);
        assert_eq!(row_labels(rows[1]), ["🗑 Убрать"]);
        assert!(matches!(rows[1][0].action, Action::NoticeClose));
    }

    #[test]
    fn digest_has_dismiss_button_on_the_last_row() {
        let mut view = digest_view((0..25).map(|n| open_task(&format!("Задача {n}"))).collect());
        view.shopping_open = 3;
        view.shopping_list = Some(Uuid::now_v7());
        let ui = digest_ui(&view);

        let rows = button_rows(&ui);
        let last = rows.last().expect("rows");
        assert_eq!(row_labels(last), ["🗑 Убрать"]);
        assert!(matches!(last[0].action, Action::NoticeClose));
        let bottom = rows
            .iter()
            .find(|row| row_labels(row).contains(&"📋 Задачи"))
            .expect("bottom row");
        assert_eq!(
            row_labels(bottom),
            ["📋 Задачи", "🛒 Покупки · 3", "Показать ещё"]
        );
    }

    #[test]
    fn empty_digest_has_dismiss_button() {
        let mut view = digest_view(Vec::new());
        view.tasks_list = None;
        let ui = digest_ui(&view);

        let rows = button_rows(&ui);
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(row_labels(rows[0]), ["🗑 Убрать"]);
        assert!(matches!(rows[0][0].action, Action::NoticeClose));
    }

    #[test]
    fn snoozed_screen_has_dismiss_button() {
        let item_id = Uuid::now_v7();
        let ui = snoozed_ui("😴 Напомню в 11:00".into(), "Позвонить маме", item_id);

        let rows = button_rows(&ui);
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(row_labels(rows[0]), ["📋 Открыть", "🗑 Убрать"]);
        assert!(matches!(
            rows[0][0].action,
            Action::NoticeOpen { item_id: id } if id == item_id
        ));
        assert!(matches!(rows[0][1].action, Action::NoticeClose));
    }

    #[test]
    fn gone_screen_has_dismiss_button() {
        let ui = gone_ui();

        let rows = button_rows(&ui);
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(row_labels(rows[0]), ["🏠 Меню", "🗑 Убрать"]);
        assert!(matches!(rows[0][1].action, Action::NoticeClose));
    }

    fn notice_member(id: Uuid, name: &str, deleted: bool) -> User {
        User {
            id,
            household_id: Uuid::nil(),
            telegram_user_id: 1,
            telegram_chat_id: 1,
            display_name: name.into(),
            username: None,
            role: UserRole::Member,
            timezone: None,
            settings: serde_json::json!({}),
            created_at: DateTime::UNIX_EPOCH,
            updated_at: DateTime::UNIX_EPOCH,
            deleted_at: deleted.then_some(DateTime::UNIX_EPOCH),
        }
    }

    fn heading(ui: &Ui) -> &str {
        match &ui.blocks[0] {
            Block::Heading(text) => text,
            other => panic!("expected a heading, got {other:?}"),
        }
    }

    fn assert_only_dismiss(ui: &Ui) {
        assert_eq!(ui.button_count(), 1, "{:?}", ui.blocks);
        let rows = button_rows(ui);
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(row_labels(rows[0]), ["🗑 Убрать"]);
        assert!(matches!(rows[0][0].action, Action::NoticeClose));
    }

    #[test]
    fn member_name_is_hidden_for_the_recipient_themselves() {
        let recipient = Uuid::now_v7();
        let member = notice_member(recipient, "Маша", false);
        assert_eq!(member_name(Some(recipient), recipient, Some(&member)), None);
    }

    #[test]
    fn member_name_is_hidden_without_an_actor() {
        let recipient = Uuid::now_v7();
        assert_eq!(member_name(None, recipient, None), None);
    }

    #[test]
    fn member_name_is_hidden_for_an_excluded_member() {
        let recipient = Uuid::now_v7();
        let actor = Uuid::now_v7();
        let excluded = notice_member(actor, "Маша", true);
        assert_eq!(member_name(Some(actor), recipient, Some(&excluded)), None);
        // Участника нет среди активных в household получателя — имени нет.
        assert_eq!(member_name(Some(actor), recipient, None), None);
    }

    #[test]
    fn member_name_is_shown_for_another_active_member() {
        let recipient = Uuid::now_v7();
        let actor = Uuid::now_v7();
        let member = notice_member(actor, "Маша", false);
        assert_eq!(
            member_name(Some(actor), recipient, Some(&member)),
            Some("Маша".to_owned())
        );
    }

    #[test]
    fn completed_screen_without_a_name_has_only_dismiss() {
        let ui = completed_ui("Вынести мусор", None);
        assert_eq!(heading(&ui), "✅ Выполнено: Вынести мусор");
        assert_only_dismiss(&ui);
    }

    #[test]
    fn completed_screen_with_a_name_appends_it() {
        let ui = completed_ui("Вынести мусор", Some("Маша"));
        assert_eq!(heading(&ui), "✅ Выполнено: Вынести мусор · Маша");
        assert_only_dismiss(&ui);
    }

    #[test]
    fn assigned_screen_shows_the_assignee_name_and_only_dismiss() {
        let ui = assigned_ui("Вынести мусор", Some("Маша"));
        assert_eq!(heading(&ui), "👤 Назначено: Вынести мусор · Маша");
        assert_only_dismiss(&ui);
    }

    #[test]
    fn assigned_screen_without_a_name_has_only_dismiss() {
        let ui = assigned_ui("Вынести мусор", None);
        assert_eq!(heading(&ui), "👤 Назначено: Вынести мусор");
        assert_only_dismiss(&ui);
    }

    #[test]
    fn closed_screens_clip_the_title_like_done_ui() {
        let long = "я".repeat(150);
        let clipped = format!("{}…", "я".repeat(99));
        let completed = completed_ui(&long, None);
        assert_eq!(heading(&completed), format!("✅ Выполнено: {clipped}"));
        let assigned = assigned_ui(&long, Some("Маша"));
        assert_eq!(
            heading(&assigned),
            format!("👤 Назначено: {clipped} · Маша")
        );
    }
}
