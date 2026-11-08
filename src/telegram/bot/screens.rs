//! Сборка экранов: меню, списки, «Мои задачи» и поиск.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use uuid::Uuid;

use crate::domain::models::{Item, ItemPriority, ItemStatus, List, ListKind};
use crate::domain::search::SearchFilters;
use crate::storage::repositories::{drafts, items, lists, users};
use crate::telegram::rich::{Block, Span, button};
use crate::telegram::ru;

use super::model::{Action, PAGE_SIZE, Screen, StatusFilter, Ui, screen_heading_names};
use super::{Ctx, Fail, card, forms, settings};

/// Собрать экран.
pub async fn build(ctx: &mut Ctx<'_>, screen: &Screen) -> Result<Ui, Fail> {
    let mut ui = match screen {
        Screen::Form => forms::view(ctx).await,
        Screen::Processing { job_id } => super::capture::processing(ctx, *job_id).await,
        Screen::Draft { draft_id } => super::capture::view(ctx, *draft_id).await,
        Screen::Menu => menu(ctx).await,
        Screen::Welcome => Ok(welcome()),
        Screen::FirstContact(contact) => Ok(super::onboarding::first_contact(contact)),
        Screen::Lists => lists_screen(ctx).await,
        Screen::List {
            list_id,
            page,
            edit,
            from_lists,
        } => list(ctx, *list_id, *page, *edit, *from_lists).await,
        Screen::Item {
            item_id,
            back,
            completion_id,
        } => card::view(ctx, *item_id, back, *completion_id).await,
        Screen::Editor { item_id, back } => forms::editor_screen(ctx, *item_id, back).await,
        Screen::Due { item_id, back } => card::due(ctx, *item_id, back).await,
        Screen::Deleted {
            item_id,
            title,
            back,
        } => card::deleted(ctx, *item_id, title, back).await,
        Screen::ConfirmDelete { item_id, back } => card::confirm_delete(ctx, *item_id, back).await,
        Screen::Mine { page } => mine(ctx, *page).await,
        Screen::History { page, back } => history(ctx, *page, back).await,
        Screen::Results {
            query,
            status,
            page,
        } => results(ctx, query, *status, *page).await,
        Screen::ConfirmClear {
            list_id,
            from_lists,
        } => super::shopping::confirm_clear(ctx, *list_id, *from_lists).await,
        Screen::Cleared {
            list_id,
            item_ids,
            from_lists,
        } => super::shopping::cleared(ctx, *list_id, item_ids, *from_lists).await,
        Screen::Settings
        | Screen::Profile
        | Screen::Danger
        | Screen::Notifications
        | Screen::LeadTime
        | Screen::Timezones
        | Screen::Family
        | Screen::Integrations { .. }
        | Screen::Member { .. }
        | Screen::Invites
        | Screen::Journal { .. }
        | Screen::ConfirmRemove { .. }
        | Screen::ConfirmTransfer { .. }
        | Screen::ConfirmRevokeToken { .. }
        | Screen::ConfirmLeave
        | Screen::InviteCreated { .. } => settings::view(ctx, screen).await,
    }?;
    // Маршрут нужен только экранам, у которых есть подсказка ввода: так
    // построение остальных экранов не делает лишних запросов.
    let hint = match screen {
        Screen::Menu | Screen::Form | Screen::List { .. } | Screen::Results { .. } => {
            super::routing::route(ctx, screen)
                .await?
                .hint(ctx.llm_capture)
        }
        _ => None,
    };
    if let Some(hint) = hint {
        ui = with_hint(ui, hint);
    }
    Ok(ui)
}

/// Вставить подсказку ввода перед первым рядом кнопок.
fn with_hint(mut ui: Ui, hint: String) -> Ui {
    let at = ui
        .blocks
        .iter()
        .position(|block| matches!(block, Block::Buttons(_)))
        .unwrap_or(ui.blocks.len());
    ui.blocks
        .insert(at, Block::Paragraph(vec![Span::Text(hint)]));
    ui
}

fn go(label: &str, to: Screen) -> crate::telegram::rich::Button<Action> {
    button(label, Action::Go { to })
}

/// Кнопка возврата.
pub fn back_button(label: &str, to: Screen) -> crate::telegram::rich::Button<Action> {
    go(label, to)
}

/// Ряд навигации: «⬅️ Назад» на `back`, а глубже двух шагов ещё «🏠 Меню».
pub fn nav_row(screen: &Screen, back: Screen) -> Vec<crate::telegram::rich::Button<Action>> {
    let mut buttons = vec![back_button(ru::BACK_BUTTON, back)];
    if screen.depth() > 2 {
        buttons.push(back_button(ru::MENU_BUTTON, Screen::Menu));
    }
    buttons
}

/// Куда ведёт «⬅️ Назад» со списка: в «Все списки», если он оттуда открыт,
/// иначе в меню.
pub fn list_back(from_lists: bool) -> Screen {
    if from_lists {
        Screen::Lists
    } else {
        Screen::Menu
    }
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
    let user_id = ctx.user()?.id;
    let counts = items::menu_counts(&mut *ctx.conn, household.id, ctx.tz.name(), ctx.now).await?;
    let mine_count = items::open_assigned_tasks(&mut *ctx.conn, household.id, user_id).await?;
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
    let open_in = |list_id: Option<Uuid>, fallback: i64| {
        list_id.map_or(fallback, |list_id| open.get(&list_id).copied().unwrap_or(0))
    };
    let tasks_list = default_of(ListKind::Tasks);
    let shopping_list = default_of(ListKind::Shopping);
    let tasks_count = open_in(tasks_list, counts.open_tasks);
    let shopping_count = open_in(shopping_list, counts.open_shopping);

    let mut ui = Ui::new().heading(household.name).spans(vec![
        Span::text("Просрочено: "),
        Span::bold(counts.overdue_tasks.to_string()),
        Span::text(" · На сегодня: "),
        Span::bold(counts.due_today_tasks.to_string()),
        Span::text(" · Выполнено: "),
        Span::bold(counts.done_today.to_string()),
    ]);

    if let Some(pending) = drafts::latest_pending_telegram(
        &mut *ctx.conn,
        household.id,
        ctx.chat_id,
        Some(user_id),
        ctx.now,
    )
    .await?
    {
        ui = ui
            .text(format!(
                "📝 Ждёт подтверждения: {}",
                ru::points(pending.items)
            ))
            .row(vec![go(
                "Открыть",
                Screen::Draft {
                    draft_id: pending.draft_id,
                },
            )]);
    }

    let tasks_target = tasks_list
        .map(|list_id| Screen::List {
            list_id,
            page: 0,
            edit: false,
            from_lists: false,
        })
        .unwrap_or(Screen::Lists);
    let shopping_target = shopping_list
        .map(|list_id| Screen::List {
            list_id,
            page: 0,
            edit: false,
            from_lists: false,
        })
        .unwrap_or(Screen::Lists);

    ui = ui
        .row(vec![
            button(
                "➕ Добавить",
                Action::Add {
                    list_id: None,
                    kind: ListKind::Tasks,
                    back: None,
                },
            ),
            go(&format!("🛒 Покупки · {shopping_count}"), shopping_target),
        ])
        .row(vec![
            go(&format!("📋 Задачи · {tasks_count}"), tasks_target),
            go(
                &format!("👤 Мои задачи · {mine_count}"),
                Screen::Mine { page: 0 },
            ),
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

fn welcome() -> Ui {
    Ui::new()
        .heading("Пространство создано")
        .text("Созданы списки «Задачи» и «Покупки».")
        .text("Отправьте мне любую фразу — например: «завтра купить молоко и позвонить маме».")
        .row(vec![
            go("📋 Открыть меню", Screen::Menu),
            go("👥 Пригласить семью", Screen::Invites),
        ])
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
                edit: false,
                from_lists: true,
            },
        )]);
    }
    Ok(ui.row(nav_row(&Screen::Lists, Screen::Menu)))
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
pub(super) fn page_bounds(total: usize, page: u32) -> (usize, usize, usize, usize) {
    let pages = total.div_ceil(PAGE_SIZE).max(1);
    let page = usize::try_from(page).unwrap_or(0).min(pages - 1);
    let start = page * PAGE_SIZE;
    (start, (start + PAGE_SIZE).min(total), page, pages)
}

pub(super) fn pager(ui: Ui, page: usize, pages: usize, make: impl Fn(usize) -> Screen) -> Ui {
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

/// Подпись строки задачи: название, срок и приоритет «срочно»/«высокий».
pub fn task_label(item: &Item, tz: Tz, now: DateTime<Utc>) -> String {
    let mut label = clip(&item.title, 40);
    if let Some(due) = ru::due_label(item.due_at, item.due_date, tz, now) {
        label.push_str(&format!(" · {due}"));
    }
    label.push_str(priority_marker(item));
    label
}

/// Подпись строки выполненной сегодня задачи: «✅ Название · 13:47».
pub fn completion_label(completion: &items::completions::Completion, tz: Tz) -> String {
    format!(
        "✅ {} · {}",
        clip(&completion.title, 40),
        completion.completed_at.with_timezone(&tz).format("%H:%M")
    )
}

async fn list(
    ctx: &mut Ctx<'_>,
    list_id: Uuid,
    page: u32,
    edit: bool,
    from_lists: bool,
) -> Result<Ui, Fail> {
    let household_id = ctx.household()?.id;
    let list = lists::get_visible(&mut *ctx.conn, household_id, list_id).await?;
    if list.kind == ListKind::Tasks {
        tasks_list(ctx, &list, page, from_lists).await
    } else {
        super::shopping::list_view(ctx, &list, page, edit, from_lists).await
    }
}

async fn tasks_list(
    ctx: &mut Ctx<'_>,
    list: &List,
    page: u32,
    from_lists: bool,
) -> Result<Ui, Fail> {
    let household_id = ctx.household()?.id;
    let all = items::list_by_list(&mut *ctx.conn, household_id, list.id).await?;
    let since = items::completions::day_start(ctx.tz, ctx.now);
    let done_today =
        items::completions::for_list_since(&mut *ctx.conn, household_id, list.id, since).await?;
    let done_today_ids: HashSet<Uuid> = done_today
        .iter()
        .map(|completion| completion.item_id)
        .collect();
    let done_earlier = all
        .iter()
        .filter(|item| item.status == ItemStatus::Done && !done_today_ids.contains(&item.id))
        .count();
    let mut open: Vec<Item> = all
        .into_iter()
        .filter(|item| item.status == ItemStatus::Open)
        .collect();
    sort_tasks(&mut open, ctx.tz, ctx.now);

    let (start, end, page, pages) = page_bounds(open.len(), page);
    let here = |page: usize| Screen::List {
        list_id: list.id,
        page: u32::try_from(page).unwrap_or(0),
        edit: false,
        from_lists,
    };

    let mut heading = format!("{} · {}", list.name, open.len());
    if !done_today.is_empty() {
        heading.push_str(&format!(" · {}", ru::done_today_counter(done_today.len())));
    }
    let mut ui = Ui::new().heading(here(page).trail(&heading, None));
    let last_page = page + 1 == pages;
    let show_done_today = last_page && !done_today.is_empty();
    if open.is_empty() {
        ui = ui.text("Пока нет задач");
    } else {
        let mut current: Option<Section> = None;
        for item in &open[start..end] {
            let section = section_of(item, ctx.tz, ctx.now);
            if current != Some(section) {
                ui = ui.bold(section.title());
                current = Some(section);
            }
            ui = ui.row(vec![go(
                &task_label(item, ctx.tz, ctx.now),
                Screen::Item {
                    item_id: item.id,
                    back: Box::new(here(page)),
                    completion_id: None,
                },
            )]);
        }
    }
    if done_earlier > 0 {
        let label = if open.is_empty() {
            "Выполнено ранее"
        } else {
            "Выполнено"
        };
        ui = ui.text(format!("{label}: {done_earlier}"));
    }
    if show_done_today {
        ui = done_today_section(ui, &done_today, ctx.tz, here(page));
    }
    let current = here(page);
    let ui = pager(ui, page, pages, here);
    let actions = vec![
        button(
            "➕ Добавить",
            Action::Add {
                list_id: Some(list.id),
                kind: ListKind::Tasks,
                back: Some(current.clone()),
            },
        ),
        go(
            "📜 История",
            Screen::History {
                page: 0,
                back: Box::new(current.clone()),
            },
        ),
    ];
    Ok(ui
        .row(actions)
        .row(nav_row(&current, list_back(from_lists))))
}

/// Последний раздел списка задач: выполнения этого списка с начала дня.
fn done_today_section(
    mut ui: Ui,
    done_today: &[items::completions::Completion],
    tz: Tz,
    back: Screen,
) -> Ui {
    ui = ui.bold(ru::DONE_TODAY_TITLE);
    for completion in done_today {
        ui = ui.row(vec![go(
            &completion_label(completion, tz),
            Screen::Item {
                item_id: completion.item_id,
                back: Box::new(back.clone()),
                completion_id: Some(completion.event_id),
            },
        )]);
    }
    ui
}

/// Журнал выполнений household: группы по плановому дню, от поздних к ранним.
async fn history(ctx: &mut Ctx<'_>, page: u32, back: &Screen) -> Result<Ui, Fail> {
    let household_id = ctx.household()?.id;
    let journal =
        items::completions::journal_page(&mut *ctx.conn, household_id, ctx.tz, page).await?;
    let names: HashMap<Uuid, String> = users::list_active(&mut *ctx.conn, household_id)
        .await?
        .into_iter()
        .map(|user| (user.id, user.display_name))
        .collect();
    let list_name = match back {
        Screen::List { list_id, .. } => {
            match lists::get_visible(&mut *ctx.conn, household_id, *list_id).await {
                Ok(list) => Some(list.name),
                Err(crate::domain::error::DomainError::NotFound) => None,
                Err(error) => return Err(error.into()),
            }
        }
        _ => None,
    };
    let screen = Screen::History {
        page: journal.page,
        back: Box::new(back.clone()),
    };
    let mut ui = screen_heading_names(&screen, "История", list_name.as_deref(), None, None);
    if journal.entries.is_empty() {
        return Ok(ui
            .text("Пока нет выполнений.")
            .row(nav_row(&screen, back.clone())));
    }

    let today = ctx.now.with_timezone(&ctx.tz).date_naive();
    let mut current: Option<Option<chrono::NaiveDate>> = None;
    for entry in &journal.entries {
        let day = entry.planned_day;
        if current != Some(day) {
            let title = day.map_or_else(
                || "Без срока".to_owned(),
                |day| ru::planned_day_label(day, today),
            );
            ui = ui.bold(title);
            current = Some(day);
        }
        let mut label = clip(&entry.completion.title, 40);
        if let Some(name) = entry.completion.actor_user_id.and_then(|id| names.get(&id)) {
            label.push_str(&format!(" · {name}"));
        }
        ui = ui.row(vec![go(
            &label,
            Screen::Item {
                item_id: entry.completion.item_id,
                back: Box::new(screen.clone()),
                completion_id: Some(entry.completion.event_id),
            },
        )]);
    }

    let here = |page: usize| Screen::History {
        page: u32::try_from(page).unwrap_or(0),
        back: Box::new(back.clone()),
    };
    let page = usize::try_from(journal.page).unwrap_or(0);
    let pages = usize::try_from(journal.pages).unwrap_or(1);
    let ui = pager(ui, page, pages, here);
    Ok(ui.row(nav_row(&screen, back.clone())))
}

async fn mine(ctx: &mut Ctx<'_>, page: u32) -> Result<Ui, Fail> {
    let household_id = ctx.household()?.id;
    let user_id = ctx.user()?.id;
    // Заголовок считает по тому же счётчику, что и кнопка меню: поиск ниже
    // ограничен MAX_LIMIT и не должен уменьшать число в заголовке.
    let total = items::open_assigned_tasks(&mut *ctx.conn, household_id, user_id).await?;
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

    let (start, end, page, pages) = page_bounds(tasks.len(), page);
    let here = |page: usize| Screen::Mine {
        page: u32::try_from(page).unwrap_or(0),
    };

    let mut ui = Ui::new().heading(here(page).trail(&format!("Мои задачи · {total}"), None));
    if tasks.is_empty() {
        return Ok(ui
            .text("Вам пока ничего не назначено")
            .row(nav_row(&here(page), Screen::Menu)));
    }
    for item in &tasks[start..end] {
        ui = ui.row(vec![go(
            &task_label(item, ctx.tz, ctx.now),
            Screen::Item {
                item_id: item.id,
                back: Box::new(here(page)),
                completion_id: None,
            },
        )]);
    }
    let current = here(page);
    let ui = pager(ui, page, pages, here);
    Ok(ui.row(nav_row(&current, Screen::Menu)))
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

    let mut ui = Ui::new().heading(here(0).trail("Поиск", None));
    if hits.is_empty() {
        let mut row = vec![button("✏️ Изменить запрос", Action::SearchStart)];
        row.extend(nav_row(&here(0), Screen::Menu));
        return Ok(ui.text("Ничего не нашлось").row(filters_row).row(row));
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
            "✅ "
        } else {
            ""
        };
        let mut label = format!(
            "{mark}{} · {}",
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
                completion_id: None,
            },
        )]);
    }
    let current = here(page);
    let ui = pager(ui, page, pages, here).row(filters_row);
    let mut row = vec![button("✏️ Изменить запрос", Action::SearchStart)];
    row.extend(nav_row(&current, Screen::Menu));
    Ok(ui.row(row))
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
