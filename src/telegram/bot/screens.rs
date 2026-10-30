//! Сборка экранов: меню, списки, «Мои задачи» и поиск.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use uuid::Uuid;

use crate::domain::models::{Item, ItemPriority, ItemStatus, List, ListKind, User};
use crate::domain::search::SearchFilters;
use crate::storage::repositories::{items, lists, users};
use crate::telegram::rich::{Span, button};
use crate::telegram::ru;

use super::model::{Action, PAGE_SIZE, Screen, StatusFilter, Ui};
use super::{Ctx, Fail, card, forms, settings};

/// Собрать экран.
pub async fn build(ctx: &mut Ctx<'_>, screen: &Screen) -> Result<Ui, Fail> {
    match screen {
        Screen::Form => forms::view(ctx).await,
        Screen::Processing => Ok(super::capture::processing()),
        Screen::Draft { draft_id } => super::capture::view(ctx, *draft_id).await,
        Screen::Menu => menu(ctx).await,
        Screen::Welcome => Ok(welcome()),
        Screen::Add => Ok(add_choice()),
        Screen::Lists => lists_screen(ctx).await,
        Screen::List { list_id, page } => list(ctx, *list_id, *page).await,
        Screen::Item { item_id, back } => card::view(ctx, *item_id, back).await,
        Screen::Completed { item_id, back } => card::completed(ctx, *item_id, back).await,
        Screen::ConfirmDelete { item_id, back } => card::confirm_delete(ctx, *item_id, back).await,
        Screen::Mine { page } => mine(ctx, *page).await,
        Screen::Results {
            query,
            status,
            page,
        } => results(ctx, query, *status, *page).await,
        Screen::ConfirmClear { list_id } => super::shopping::confirm_clear(ctx, *list_id).await,
        Screen::Cleared { list_id, item_ids } => {
            super::shopping::cleared(ctx, *list_id, item_ids).await
        }
        Screen::Settings
        | Screen::Notifications
        | Screen::Timezones
        | Screen::Family
        | Screen::Member { .. }
        | Screen::Invites
        | Screen::Journal { .. }
        | Screen::ConfirmRemove { .. }
        | Screen::ConfirmTransfer { .. }
        | Screen::ConfirmLeave
        | Screen::InviteCreated { .. } => settings::view(ctx, screen).await,
    }
}

fn go(label: &str, to: Screen) -> crate::telegram::rich::Button<Action> {
    button(label, Action::Go { to })
}

/// Кнопка возврата.
pub fn back_button(label: &str, to: Screen) -> crate::telegram::rich::Button<Action> {
    go(label, to)
}

/// Сократить подпись кнопки.
pub fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut clipped: String = text.chars().take(max.saturating_sub(1)).collect();
    clipped.push('…');
    clipped
}

fn priority_marker(item: &Item) -> &'static str {
    match item.priority {
        ItemPriority::Urgent => " · 🔴 срочно",
        ItemPriority::High => " · 🟠 высокий",
        _ => "",
    }
}

async fn menu(ctx: &mut Ctx<'_>) -> Result<Ui, Fail> {
    let household = ctx.household()?.clone();
    let counts = items::menu_counts(&mut *ctx.conn, household.id, ctx.tz.name(), ctx.now).await?;
    let all_lists = lists::list(&mut *ctx.conn, household.id).await?;
    let visible: Vec<&List> = all_lists.iter().filter(|list| !list.is_archived).collect();
    let open = items::open_counts_by_list(&mut *ctx.conn, household.id).await?;

    let default_of = |kind: ListKind| {
        visible
            .iter()
            .find(|list| list.kind == kind && list.is_default)
            .or_else(|| visible.iter().find(|list| list.kind == kind))
            .map(|list| list.id)
    };

    let summary = lists_line(&visible, &open);
    let mut ui = Ui::new()
        .heading(format!(
            "{} · {}",
            household.name,
            ru::tasks(counts.open_tasks)
        ))
        .spans(vec![
            Span::text("Просрочено: "),
            Span::bold(counts.overdue_tasks.to_string()),
            Span::text(" · Покупки: "),
            Span::bold(counts.open_shopping.to_string()),
            Span::text(" · Выполнено сегодня: "),
            Span::bold(counts.done_today.to_string()),
        ]);
    if !summary.is_empty() {
        ui = ui.text(format!("Списки: {summary}"));
    }

    let tasks_target = default_of(ListKind::Tasks)
        .map(|list_id| Screen::List { list_id, page: 0 })
        .unwrap_or(Screen::Lists);
    let shopping_target = default_of(ListKind::Shopping)
        .map(|list_id| Screen::List { list_id, page: 0 })
        .unwrap_or(Screen::Lists);

    ui = ui
        .row(vec![
            go("➕ Добавить", Screen::Add),
            go("🛒 Покупки", shopping_target),
        ])
        .row(vec![
            go("📋 Задачи", tasks_target),
            go("👤 Мои задачи", Screen::Mine { page: 0 }),
        ])
        .row(vec![
            button("🔎 Поиск", Action::SearchStart),
            go("⚙️ Настройки", Screen::Settings),
        ]);
    if visible.len() > 2 {
        ui = ui.row(vec![go("🗂 Все списки", Screen::Lists)]);
    }
    Ok(ui)
}

fn lists_line(visible: &[&List], open: &HashMap<Uuid, i64>) -> String {
    visible
        .iter()
        .map(|list| {
            format!(
                "{} ({})",
                list.name,
                open.get(&list.id).copied().unwrap_or(0)
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn welcome() -> Ui {
    Ui::new()
        .heading("Готово!")
        .text("Созданы списки «Задачи» и «Покупки».")
        .text("Отправьте мне любую фразу — например: «завтра купить молоко и позвонить маме».")
        .row(vec![
            go("📋 Открыть меню", Screen::Menu),
            go("👥 Пригласить семью", Screen::Invites),
        ])
}

fn add_choice() -> Ui {
    Ui::new()
        .heading("Что добавить?")
        .row(vec![
            button(
                "📝 Задачу",
                Action::Add {
                    list_id: None,
                    kind: ListKind::Tasks,
                },
            ),
            button(
                "🛒 Покупку",
                Action::Add {
                    list_id: None,
                    kind: ListKind::Shopping,
                },
            ),
        ])
        .row(vec![go("⬅️ В меню", Screen::Menu)])
}

async fn lists_screen(ctx: &mut Ctx<'_>) -> Result<Ui, Fail> {
    let household_id = ctx.household()?.id;
    let all_lists = lists::list(&mut *ctx.conn, household_id).await?;
    let open = items::open_counts_by_list(&mut *ctx.conn, household_id).await?;

    let mut ui = Ui::new().heading("Все списки");
    for list in all_lists.iter().filter(|list| !list.is_archived) {
        let icon = if list.kind == ListKind::Tasks {
            "📋"
        } else {
            "🛒"
        };
        ui = ui.row(vec![go(
            &format!(
                "{icon} {} · {}",
                clip(&list.name, 30),
                open.get(&list.id).copied().unwrap_or(0)
            ),
            Screen::List {
                list_id: list.id,
                page: 0,
            },
        )]);
    }
    Ok(ui.row(vec![go("⬅️ В меню", Screen::Menu)]))
}

/// Раздел списка задач по сроку.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Section {
    Overdue,
    Today,
    Tomorrow,
    Later,
    NoDue,
}

impl Section {
    fn title(self) -> &'static str {
        match self {
            Self::Overdue => "Просрочено",
            Self::Today => "Сегодня",
            Self::Tomorrow => "Завтра",
            Self::Later => "Позже",
            Self::NoDue => "Без срока",
        }
    }
}

fn section_of(item: &Item, tz: Tz, now: DateTime<Utc>) -> Section {
    let today = now.with_timezone(&tz).date_naive();
    let due_day = match (item.due_at, item.due_date) {
        (Some(at), _) => {
            if at < now {
                return Section::Overdue;
            }
            at.with_timezone(&tz).date_naive()
        }
        (None, Some(date)) => date,
        (None, None) => return Section::NoDue,
    };
    match (due_day - today).num_days() {
        d if d < 0 => Section::Overdue,
        0 => Section::Today,
        1 => Section::Tomorrow,
        _ => Section::Later,
    }
}

fn sort_tasks(items: &mut [Item], tz: Tz, now: DateTime<Utc>) {
    let priority_rank = |item: &Item| match item.priority {
        ItemPriority::Urgent => 0,
        ItemPriority::High => 1,
        ItemPriority::Normal => 2,
        ItemPriority::Low => 3,
    };
    items.sort_by(|left, right| {
        section_of(left, tz, now)
            .cmp(&section_of(right, tz, now))
            .then(due_key(left).cmp(&due_key(right)))
            .then(priority_rank(left).cmp(&priority_rank(right)))
            .then(left.created_at.cmp(&right.created_at))
            .then(left.id.cmp(&right.id))
    });
}

fn due_key(item: &Item) -> (u8, Option<DateTime<Utc>>, Option<chrono::NaiveDate>) {
    (u8::from(item.due_at.is_none()), item.due_at, item.due_date)
}

/// Страница `page` из `total` элементов; номер ограничивается последней
/// страницей, чтобы после действия не открывалась пустая.
fn page_bounds(total: usize, page: u32) -> (usize, usize, usize, usize) {
    let pages = total.div_ceil(PAGE_SIZE).max(1);
    let page = usize::try_from(page).unwrap_or(0).min(pages - 1);
    let start = page * PAGE_SIZE;
    (start, (start + PAGE_SIZE).min(total), page, pages)
}

fn pager(ui: Ui, page: usize, pages: usize, make: impl Fn(usize) -> Screen) -> Ui {
    if pages <= 1 {
        return ui;
    }
    let mut row = Vec::new();
    if page > 0 {
        row.push(go("←", make(page - 1)));
    }
    if page + 1 < pages {
        row.push(go("→", make(page + 1)));
    }
    let mut ui = ui
        .text(format!("Страница {} из {pages}", page + 1))
        .row(row);
    if page > 1 {
        ui = ui.row(vec![go("⏮ В начало", make(0))]);
    }
    ui
}

async fn member_names(ctx: &mut Ctx<'_>) -> Result<HashMap<Uuid, String>, Fail> {
    let household_id = ctx.household()?.id;
    let members: Vec<User> = users::list_active(&mut *ctx.conn, household_id).await?;
    Ok(members
        .into_iter()
        .map(|member| (member.id, member.display_name))
        .collect())
}

/// Подпись строки задачи: название, срок, исполнитель.
pub fn task_label(
    item: &Item,
    tz: Tz,
    now: DateTime<Utc>,
    names: &HashMap<Uuid, String>,
) -> String {
    let mut label = clip(&item.title, 40);
    if let Some(due) = ru::due_label(item.due_at, item.due_date, tz, now) {
        label.push_str(&format!(" · {due}"));
    }
    label.push_str(priority_marker(item));
    if let Some(name) = item.assignee_user_id.and_then(|id| names.get(&id)) {
        label.push_str(&format!(" · 👤 {}", clip(name, 14)));
    }
    label
}

async fn list(ctx: &mut Ctx<'_>, list_id: Uuid, page: u32) -> Result<Ui, Fail> {
    let household_id = ctx.household()?.id;
    let list = lists::get_visible(&mut *ctx.conn, household_id, list_id).await?;
    if list.kind == ListKind::Tasks {
        tasks_list(ctx, &list, page).await
    } else {
        super::shopping::list_view(ctx, &list, page).await
    }
}

async fn tasks_list(ctx: &mut Ctx<'_>, list: &List, page: u32) -> Result<Ui, Fail> {
    let household_id = ctx.household()?.id;
    let all = items::list_by_list(&mut *ctx.conn, household_id, list.id).await?;
    let done = all
        .iter()
        .filter(|item| item.status == ItemStatus::Done)
        .count();
    let mut open: Vec<Item> = all
        .into_iter()
        .filter(|item| item.status == ItemStatus::Open)
        .collect();
    sort_tasks(&mut open, ctx.tz, ctx.now);

    let names = member_names(ctx).await?;
    let (start, end, page, pages) = page_bounds(open.len(), page);
    let here = |page: usize| Screen::List {
        list_id: list.id,
        page: u32::try_from(page).unwrap_or(0),
    };

    let mut ui = Ui::new().heading(format!("{} · {}", list.name, open.len()));
    if open.is_empty() {
        ui = ui.text("Пока нет задач");
        if done > 0 {
            ui = ui.text(format!("Выполнено ранее: {done}"));
        }
        return Ok(ui.row(vec![
            button(
                "➕ Добавить",
                Action::Add {
                    list_id: Some(list.id),
                    kind: ListKind::Tasks,
                },
            ),
            go("⬅️ Назад", Screen::Menu),
        ]));
    }

    let mut current: Option<Section> = None;
    for item in &open[start..end] {
        let section = section_of(item, ctx.tz, ctx.now);
        if current != Some(section) {
            ui = ui.bold(section.title());
            current = Some(section);
        }
        ui = ui.row(vec![
            button(
                "☐",
                Action::Complete {
                    item_id: item.id,
                    version: item.version,
                    back: here(page),
                },
            ),
            go(
                &task_label(item, ctx.tz, ctx.now, &names),
                Screen::Item {
                    item_id: item.id,
                    back: Box::new(here(page)),
                },
            ),
        ]);
    }
    if done > 0 {
        ui = ui.text(format!("Выполнено: {done}"));
    }
    let ui = pager(ui, page, pages, here);
    Ok(ui.row(vec![
        button(
            "➕ Добавить",
            Action::Add {
                list_id: Some(list.id),
                kind: ListKind::Tasks,
            },
        ),
        go("⬅️ Назад", Screen::Menu),
    ]))
}

async fn mine(ctx: &mut Ctx<'_>, page: u32) -> Result<Ui, Fail> {
    let household_id = ctx.household()?.id;
    let user_id = ctx.user()?.id;
    let filters = SearchFilters {
        assignee_user_id: Some(user_id),
        limit: crate::domain::search::MAX_LIMIT,
        ..SearchFilters::default()
    };
    let (hits, _) = items::search(&mut *ctx.conn, household_id, ctx.tz.name(), &filters).await?;
    let mut tasks: Vec<_> = hits
        .into_iter()
        .filter(|hit| hit.list_kind == ListKind::Tasks)
        .map(|hit| hit.item)
        .collect();
    sort_tasks(&mut tasks, ctx.tz, ctx.now);

    let names = member_names(ctx).await?;
    let (start, end, page, pages) = page_bounds(tasks.len(), page);
    let here = |page: usize| Screen::Mine {
        page: u32::try_from(page).unwrap_or(0),
    };

    let mut ui = Ui::new().heading(format!("Мои задачи · {}", tasks.len()));
    if tasks.is_empty() {
        return Ok(ui
            .text("Вам пока ничего не назначено")
            .row(vec![go("⬅️ Назад", Screen::Menu)]));
    }
    for item in &tasks[start..end] {
        ui = ui.row(vec![
            button(
                "☐",
                Action::Complete {
                    item_id: item.id,
                    version: item.version,
                    back: here(page),
                },
            ),
            go(
                &task_label(item, ctx.tz, ctx.now, &names),
                Screen::Item {
                    item_id: item.id,
                    back: Box::new(here(page)),
                },
            ),
        ]);
    }
    let ui = pager(ui, page, pages, here);
    Ok(ui.row(vec![go("⬅️ Назад", Screen::Menu)]))
}

async fn results(
    ctx: &mut Ctx<'_>,
    query: &str,
    status: StatusFilter,
    page: u32,
) -> Result<Ui, Fail> {
    let household_id = ctx.household()?.id;
    let filters = SearchFilters {
        q: Some(query.to_owned()),
        status: match status {
            StatusFilter::Done => Some(ItemStatus::Done),
            _ => Some(ItemStatus::Open),
        },
        all_status: status == StatusFilter::All,
        limit: crate::domain::search::MAX_LIMIT,
        ..SearchFilters::default()
    };
    let (hits, _) = items::search(&mut *ctx.conn, household_id, ctx.tz.name(), &filters).await?;

    let here = |page: usize| Screen::Results {
        query: query.to_owned(),
        status,
        page: u32::try_from(page).unwrap_or(0),
    };
    let filter_row = |label: &str, value: StatusFilter| {
        let mark = if value == status { "● " } else { "" };
        go(
            &format!("{mark}{label}"),
            Screen::Results {
                query: query.to_owned(),
                status: value,
                page: 0,
            },
        )
    };
    let filters_row = vec![
        filter_row("Открытые", StatusFilter::Open),
        filter_row("Выполненные", StatusFilter::Done),
        filter_row("Все", StatusFilter::All),
    ];

    let mut ui = Ui::new().heading("Поиск");
    if hits.is_empty() {
        return Ok(ui.text("Ничего не нашлось").row(filters_row).row(vec![
            button("✏️ Изменить запрос", Action::SearchStart),
            go("⬅️ Назад", Screen::Menu),
        ]));
    }

    let (start, end, page, pages) = page_bounds(hits.len(), page);
    ui = ui.text(format!(
        "Найдено {}{}",
        ru::results(i64::try_from(hits.len()).unwrap_or(0)),
        if hits.len() >= usize::try_from(crate::domain::search::MAX_LIMIT).unwrap_or(0) {
            " или больше"
        } else {
            ""
        }
    ));
    for hit in &hits[start..end] {
        let mark = if hit.item.status == ItemStatus::Done {
            "✅"
        } else {
            "☐"
        };
        let mut label = format!(
            "{mark} {} · {}",
            clip(&hit.item.title, 34),
            clip(&hit.list_name, 14)
        );
        if hit.item.status == ItemStatus::Done {
            label.push_str(" · выполнено");
        } else if hit.list_kind != ListKind::Tasks {
            if let Some(quantity) = hit.item.quantity {
                label.push_str(&format!(
                    " · {}",
                    ru::quantity_label(quantity, hit.item.unit.as_deref())
                ));
            }
        } else if let Some(due) = ru::due_label(hit.item.due_at, hit.item.due_date, ctx.tz, ctx.now)
        {
            label.push_str(&format!(" · {due}"));
        }
        ui = ui.row(vec![go(
            &label,
            Screen::Item {
                item_id: hit.item.id,
                back: Box::new(here(page)),
            },
        )]);
    }
    let ui = pager(ui, page, pages, here).row(filters_row);
    Ok(ui.row(vec![
        button("✏️ Изменить запрос", Action::SearchStart),
        go("⬅️ Назад", Screen::Menu),
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_is_clamped_to_last() {
        assert_eq!(page_bounds(0, 5), (0, 0, 0, 1));
        assert_eq!(page_bounds(8, 0), (0, 8, 0, 1));
        assert_eq!(page_bounds(9, 1), (8, 9, 1, 2));
        assert_eq!(page_bounds(9, 7), (8, 9, 1, 2));
    }

    #[test]
    fn long_labels_are_clipped() {
        assert_eq!(clip("короткий", 20), "короткий");
        assert_eq!(clip("очень-очень длинное название", 10), "очень-оче…");
        assert_eq!(clip("очень-очень длинное название", 10).chars().count(), 10);
    }
}
