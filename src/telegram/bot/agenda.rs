//! Экран «Повестка»: события и задачи по дням из календарной развёртки.
//!
//! Окно — семь дней с сегодняшнего; страницы листаются назад и вперёд. Экран
//! не делает отдельной выборки: он разворачивает те же календарные вхождения,
//! что и `GET /calendar`, и раскладывает многодневные события по дням.

use chrono::{DateTime, Duration, NaiveDate, Utc};
use chrono_tz::Tz;
use uuid::Uuid;

use crate::domain::calendar::{self, CalendarEvent, CalendarTime};
use crate::domain::events::start_of_day;
use crate::domain::models::{ItemStatus, ListKind};
use crate::domain::recurrence::parse_timezone;
use crate::storage::repositories::{items, lists};
use crate::telegram::rich::button;
use crate::telegram::ru;

use super::model::{Action, Screen, Ui};
use super::screens::{clip, nav_row};
use super::{Ctx, Fail};

/// Дней в окне повестки.
const WINDOW_DAYS: i64 = 7;

/// Запас развёртки в днях по обе стороны окна.
///
/// `expand_item` режет интервал в поясе household или серии, а окно повестки
/// задано датами в поясе пользователя. Разница поясов — до 26 часов, поэтому
/// без запаса элемент у края окна выпадал бы из развёртки. Точную фильтрацию
/// по дням пользователя делает `collect_rows`.
const EXPANSION_MARGIN_DAYS: i64 = 2;

/// Максимум строк на экране: дальше показывается число оставшихся.
const MAX_ROWS: usize = 30;

/// Окно повестки и пояса, в которых считаются дни и конец события.
struct Window {
    start: NaiveDate,
    end: NaiveDate,
    user_tz: Tz,
    household_tz: Tz,
    now: DateTime<Utc>,
}

/// Строка повестки: один элемент в одном дне окна.
struct AgendaRow {
    day: NaiveDate,
    all_day: bool,
    time: Option<DateTime<Utc>>,
    occurrence_key: String,
    item_id: Uuid,
    label: String,
}

/// Собрать экран повестки.
pub async fn view(ctx: &mut Ctx<'_>, list: Option<Uuid>, page: u32) -> Result<Ui, Fail> {
    let household_id = ctx.household()?.id;
    let household_tz = parse_timezone(&ctx.household()?.timezone)?;
    let today = ctx.now.with_timezone(&ctx.tz).date_naive();
    let start = today + Duration::days(WINDOW_DAYS * i64::from(page));
    let end = start + Duration::days(WINDOW_DAYS);
    let window = Window {
        start,
        end,
        user_tz: ctx.tz,
        household_tz,
        now: ctx.now,
    };

    let mut entries = items::calendar_items(&mut *ctx.conn, household_id).await?;
    if let Some(list_id) = list {
        // 0009-events 8.3: повестка одного списка показывает только его события,
        // без задач.
        entries.retain(|entry| entry.item.list_id == list_id && entry.kind == ListKind::Events);
    }

    let mut rows = Vec::new();
    let expansion_start = start - Duration::days(EXPANSION_MARGIN_DAYS);
    let expansion_end = end + Duration::days(EXPANSION_MARGIN_DAYS);
    for entry in &entries {
        let recurring = entry.item.recurrence.is_some();
        for event in calendar::expand_item(entry, expansion_start, expansion_end, household_tz)? {
            collect_rows(&event, entry.kind, recurring, &window, &mut rows);
        }
    }
    rows.sort_by(|left, right| {
        (
            left.day,
            !left.all_day,
            left.time,
            &left.occurrence_key,
            left.item_id,
        )
            .cmp(&(
                right.day,
                !right.all_day,
                right.time,
                &right.occurrence_key,
                right.item_id,
            ))
    });

    let screen = Screen::Agenda { list, page };
    let title = match list {
        Some(list_id) => {
            lists::get_visible(&mut *ctx.conn, household_id, list_id)
                .await?
                .name
        }
        None => "Повестка".to_owned(),
    };
    let mut ui = Ui::new().heading(screen.trail(&title, None));

    let shown = rows.len().min(MAX_ROWS);
    if rows.is_empty() {
        // 0009-events 9.1: кнопка «➕ Событие» появится вместе с редактором
        // события; пока пустое состояние только предлагает его добавить.
        ui = ui.text("На эти дни ничего не запланировано — добавьте событие.");
    } else {
        let mut current_day = None;
        for row in &rows[..shown] {
            if current_day != Some(row.day) {
                ui = ui.bold(ru::day_label(row.day));
                current_day = Some(row.day);
            }
            // 0009-events 9.2: строка события откроет карточку события;
            // пока открывается существующая карточка элемента.
            ui = ui.row(vec![button(
                &row.label,
                Action::Go {
                    to: Screen::Item {
                        item_id: row.item_id,
                        back: Box::new(screen.clone()),
                        completion_id: None,
                    },
                },
            )]);
        }
        if rows.len() > shown {
            ui = ui.text(format!("…и ещё {}", rows.len() - shown));
        }
    }

    let mut navigation = Vec::new();
    if page > 0 {
        navigation.push(go(
            "◀ Раньше",
            Screen::Agenda {
                list,
                page: page - 1,
            },
        ));
    }
    navigation.push(go(
        "Дальше ▶",
        Screen::Agenda {
            list,
            page: page + 1,
        },
    ));
    Ok(ui.row(navigation).row(nav_row(&screen, Screen::Menu)))
}

fn go(label: &str, to: Screen) -> crate::telegram::rich::Button<Action> {
    button(label, Action::Go { to })
}

/// Разложить вхождение по дням окна.
fn collect_rows(
    event: &CalendarEvent,
    kind: ListKind,
    recurring: bool,
    window: &Window,
    rows: &mut Vec<AgendaRow>,
) {
    if kind == ListKind::Events {
        if event_passed(event, window.household_tz, window.now) {
            return;
        }
    } else if event.status == ItemStatus::Done {
        return;
    }

    // Задача показывается только в свой день: конец у неё служебный.
    if kind != ListKind::Events {
        let day = match event.start {
            CalendarTime::Date(date) => date,
            CalendarTime::DateTime(at) => at.with_timezone(&window.user_tz).date_naive(),
        };
        if day >= window.start && day < window.end {
            let time = match event.start {
                CalendarTime::DateTime(at) => Some(at),
                CalendarTime::Date(_) => None,
            };
            rows.push(AgendaRow {
                day,
                all_day: event.all_day,
                time,
                occurrence_key: event.occurrence_key.clone(),
                item_id: event.item_id,
                label: row_label(event, kind, recurring, window.user_tz),
            });
        }
        return;
    }

    match (&event.start, &event.end) {
        (CalendarTime::Date(start_date), CalendarTime::Date(end_date)) => {
            // Вседневный конец исключающий: последний день — `end - 1`.
            let mut day = (*start_date).max(window.start);
            let last = (*end_date).min(window.end);
            while day < last {
                rows.push(AgendaRow {
                    day,
                    all_day: true,
                    time: None,
                    occurrence_key: event.occurrence_key.clone(),
                    item_id: event.item_id,
                    label: row_label(event, kind, recurring, window.user_tz),
                });
                day += Duration::days(1);
            }
        }
        (CalendarTime::DateTime(start_at), CalendarTime::DateTime(end_at)) => {
            let mut day = start_at
                .with_timezone(&window.user_tz)
                .date_naive()
                .max(window.start);
            let last = end_at.with_timezone(&window.user_tz).date_naive();
            while day < window.end && day <= last {
                let day_start = start_of_day(window.user_tz, day);
                let day_end = start_of_day(window.user_tz, day + Duration::days(1));
                if *start_at < day_end && *end_at > day_start {
                    rows.push(AgendaRow {
                        day,
                        all_day: false,
                        time: Some(*start_at),
                        occurrence_key: event.occurrence_key.clone(),
                        item_id: event.item_id,
                        label: row_label(event, kind, recurring, window.user_tz),
                    });
                }
                day += Duration::days(1);
            }
        }
        _ => {}
    }
}

/// Прошло ли вхождение события: конец календаря исключающий, а вседневный
/// конец дня считается в поясе household.
fn event_passed(event: &CalendarEvent, household_tz: Tz, now: DateTime<Utc>) -> bool {
    match &event.end {
        CalendarTime::Date(date) => start_of_day(household_tz, *date) <= now,
        CalendarTime::DateTime(at) => *at <= now,
    }
}

/// Подпись строки: «14:00–15:30 Маникюр» у события, «☐ 16:00 Задача» у задачи.
fn row_label(event: &CalendarEvent, kind: ListKind, recurring: bool, user_tz: Tz) -> String {
    let title = clip(&event.title, 40);
    let mut label = if kind == ListKind::Events {
        match (&event.start, &event.end) {
            (CalendarTime::Date(_), _) => format!("{} {title}", ru::ALL_DAY),
            (CalendarTime::DateTime(start), CalendarTime::DateTime(end)) => format!(
                "{}–{} {title}",
                start.with_timezone(&user_tz).format("%H:%M"),
                end.with_timezone(&user_tz).format("%H:%M")
            ),
            _ => title,
        }
    } else {
        match &event.start {
            CalendarTime::Date(_) => format!("☐ {} {title}", ru::ALL_DAY),
            CalendarTime::DateTime(start) => {
                format!(
                    "☐ {} {title}",
                    start.with_timezone(&user_tz).format("%H:%M")
                )
            }
        }
    };
    if recurring {
        label.push_str(" 🔁");
    }
    label
}
