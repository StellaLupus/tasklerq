//! Карточка элемента и действия над ним: завершение, возврат, отмена и
//! удаление.

use std::collections::HashMap;

use chrono::{DateTime, Datelike, Duration, Utc};
use chrono_tz::Tz;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::models::{
    Field, Item, ItemPatch, ItemPriority, ItemStatus, List, ListKind, MonthRule, Recurrence,
    RecurrenceFreq,
};
use crate::domain::recurrence::{Due, resolve_local};
use crate::storage::repositories::{items, lists, users};
use crate::telegram::rich::{Span, button};
use crate::telegram::ru;

use super::model::{
    Action, QuickDue, RecurrenceDraft, Screen, Ui, screen_heading, screen_heading_names,
};
use super::screens::{clip, nav_row};
use super::{Ctx, Fail};

/// Сколько последних выполнений показывает карточка.
const RECENT_COMPLETIONS: usize = 5;

/// Название приоритета с маркером, понятным без цвета.
pub fn priority_label(priority: ItemPriority) -> &'static str {
    match priority {
        ItemPriority::Low => "🔵 Низкий",
        ItemPriority::Normal => "⚪ Обычный",
        ItemPriority::High => "🟠 Высокий",
        ItemPriority::Urgent => "🔴 Срочный",
    }
}

/// Описание правила повторения.
pub fn describe_recurrence(rule: &Recurrence) -> String {
    describe(
        rule.freq,
        rule.interval,
        &rule.byweekday,
        rule.month_rule,
        Some(rule.anchor_date.day()),
    )
}

/// Описание правила из черновика.
pub fn describe_recurrence_draft(rule: &RecurrenceDraft) -> String {
    describe(
        rule.freq,
        rule.interval,
        &rule.weekdays,
        rule.month_rule,
        None,
    )
}

fn describe(
    freq: RecurrenceFreq,
    interval: i16,
    weekdays: &[i16],
    month_rule: Option<MonthRule>,
    anchor_day: Option<u32>,
) -> String {
    let n = i64::from(interval);
    match freq {
        RecurrenceFreq::Daily => {
            if interval == 1 {
                "Каждый день".to_owned()
            } else {
                format!("Каждые {}", ru::days(n))
            }
        }
        RecurrenceFreq::Weekly => {
            let days: Vec<&str> = weekdays
                .iter()
                .map(|day| ru::weekday_short(u32::try_from(*day).unwrap_or(1)))
                .collect();
            let base = if interval == 1 {
                "Каждую неделю".to_owned()
            } else {
                format!("Каждые {n} {}", ru::plural(n, "неделю", "недели", "недель"))
            };
            format!("{base}: {}", days.join(", "))
        }
        RecurrenceFreq::Monthly => {
            let base = if interval == 1 {
                "Каждый месяц".to_owned()
            } else {
                format!("Каждые {n} {}", ru::plural(n, "месяц", "месяца", "месяцев"))
            };
            match (month_rule, anchor_day) {
                (Some(MonthRule::LastDay), _) => format!("{base}, в последний день"),
                (_, Some(day)) => format!("{base}, {day}-го"),
                (_, None) => format!("{base}, в этот же день"),
            }
        }
        RecurrenceFreq::Yearly => {
            if interval == 1 {
                "Каждый год".to_owned()
            } else {
                format!("Каждые {n} {}", ru::plural(n, "год", "года", "лет"))
            }
        }
    }
}

async fn load(ctx: &mut Ctx<'_>, item_id: Uuid) -> Result<(Item, List), Fail> {
    let household_id = ctx.household()?.id;
    let item = items::get(&mut *ctx.conn, household_id, item_id).await?;
    let list = lists::get(&mut *ctx.conn, household_id, item.list_id).await?;
    Ok((item, list))
}

/// Можно ли менять срок: только у открытой задачи из списка задач.
fn due_editable(item: &Item, list: &List) -> bool {
    item.status != ItemStatus::Done && list.kind == ListKind::Tasks
}

/// Доступен ли ввод срока; для выполненного элемента и покупки — нет.
pub(super) async fn due_editable_item(ctx: &mut Ctx<'_>, item_id: Uuid) -> Result<bool, Fail> {
    let (item, list) = load(ctx, item_id).await?;
    Ok(due_editable(&item, &list))
}

async fn names(ctx: &mut Ctx<'_>) -> Result<HashMap<Uuid, String>, Fail> {
    let household_id = ctx.household()?.id;
    Ok(users::list_active(&mut *ctx.conn, household_id)
        .await?
        .into_iter()
        .map(|user| (user.id, user.display_name))
        .collect())
}

/// Список в цепочке `back`: карточка из журнала тоже должна называть
/// настоящий список, а не служебное «Список».
fn list_id_in_chain(screen: &Screen) -> Option<Uuid> {
    match screen {
        Screen::List { list_id, .. } => Some(*list_id),
        Screen::History { back, .. } => list_id_in_chain(back),
        _ => None,
    }
}

/// Имя списка из цепочки `back` для хлебных крошек.
async fn parent_list_name(ctx: &mut Ctx<'_>, back: &Screen) -> Result<Option<String>, Fail> {
    let Some(list_id) = list_id_in_chain(back) else {
        return Ok(None);
    };
    let household_id = ctx.household()?.id;
    match lists::get_visible(&mut *ctx.conn, household_id, list_id).await {
        Ok(list) => Ok(Some(list.name)),
        Err(DomainError::NotFound) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Карточка элемента.
pub async fn view(
    ctx: &mut Ctx<'_>,
    item_id: Uuid,
    back: &Screen,
    completion_id: Option<Uuid>,
) -> Result<Ui, Fail> {
    let (item, list) = load(ctx, item_id).await?;
    let names = names(ctx).await?;
    let shopping = list.kind != ListKind::Tasks;
    let completions = if shopping {
        Vec::new()
    } else {
        let household_id = ctx.household()?.id;
        items::completions::recent_for_item(
            &mut *ctx.conn,
            household_id,
            item_id,
            RECENT_COMPLETIONS,
        )
        .await?
    };
    let here = Screen::Item {
        item_id,
        back: Box::new(back.clone()),
        completion_id,
    };
    let done = item.status == ItemStatus::Done;
    let parent = parent_list_name(ctx, back).await?;
    let mut ui = screen_heading(&here, &item.title, parent.as_deref());
    if done {
        ui = ui.text("✅ Выполнено");
    }
    if shopping && let Some(quantity) = item.quantity {
        ui = ui.text(format!(
            "Количество: {}",
            ru::quantity_label(quantity, item.unit.as_deref())
        ));
    }
    if let Some(due) = ru::due_label(item.due_at, item.due_date, ctx.tz, ctx.now) {
        let overdue = !done && is_overdue(&item, ctx);
        ui = ui.text(format!(
            "📅 {}{}",
            ru::capitalize(&due),
            if overdue {
                " · просрочено"
            } else {
                ""
            }
        ));
    }
    if !shopping {
        ui = ui.text(format!("Приоритет: {}", priority_label(item.priority)));
    }
    if !item.tags.is_empty() {
        let tags: Vec<&str> = item.tags.iter().map(|tag| tag.name.as_str()).collect();
        ui = ui.text(format!("🏷 {}", tags.join(", ")));
    }
    if let Some(name) = item.assignee_user_id.and_then(|id| names.get(&id)) {
        ui = ui.text(format!("👤 {name}"));
    }
    if let Some(rule) = &item.recurrence {
        ui = ui.text(format!("🔁 {}", describe_recurrence(rule)));
        if let Some(next) = next_occurrence(&item, ctx) {
            ui = ui.text(format!("Следующее: {next}"));
        }
    }
    let (done_count, total) = item.subtask_progress();
    if total > 0 {
        let lines: Vec<String> = item
            .subtasks
            .iter()
            .map(|subtask| {
                let mark = if subtask.status == ItemStatus::Done {
                    "☑"
                } else {
                    "☐"
                };
                format!("{mark} {}", subtask.title)
            })
            .collect();
        ui = ui.spans(vec![
            Span::text(format!("🧩 Подзадачи: {done_count} из {total}\n")),
            Span::text(lines.join("\n")),
        ]);
    }
    ui = ui.text(format!("Список: {}", list.name));
    if let Some(notes) = item
        .notes
        .as_deref()
        .filter(|notes| !notes.trim().is_empty())
    {
        ui = ui.quote(notes.to_owned());
    }
    if !completions.is_empty() {
        let lines: Vec<String> = completions
            .iter()
            .map(|completion| completion_line(completion, &names, ctx.tz, ctx.now))
            .collect();
        ui = ui.bold("Выполнения").collapsed_quote(lines.join("\n"));
    }

    // Для задачи возврат ведёт туда, откуда открыли карточку (журнал, список
    // или «Выполнено сегодня»); у покупки остаётся прежнее поведение.
    let origin = if shopping { here.clone() } else { back.clone() };
    let revert = |completion_id: Option<Uuid>| {
        button(
            "↩️ Вернуть",
            Action::Reopen {
                item_id,
                version: item.version,
                back: origin.clone(),
                completion_id,
            },
        )
    };
    // Кнопка есть ровно тогда, когда `revert_completion` пройдёт: выполнение
    // последнее, а повторяющаяся задача не менялась после него.
    let revertible = completion_id.is_some_and(|id| {
        completions.first().is_some_and(|latest| {
            latest.event_id == id
                && (item.recurrence.is_none() || latest.version_after == Some(item.version))
        })
    });

    let edit = button(
        ru::EDIT_BUTTON,
        Action::Edit {
            item_id,
            back: here.clone(),
        },
    );
    let main = if done {
        if shopping {
            Some(revert(None))
        } else if completion_id.is_some() {
            revertible.then(|| revert(completion_id))
        } else {
            Some(revert(None))
        }
    } else {
        Some(button(
            if shopping {
                ru::BOUGHT_BUTTON
            } else {
                ru::DONE_BUTTON
            },
            Action::Complete {
                item_id,
                version: item.version,
                back: back.clone(),
            },
        ))
    };
    let extra_revert = (!done && revertible).then(|| revert(completion_id));
    let delete = button(
        "🗑 Удалить",
        Action::DeleteAsk {
            item_id,
            back: back.clone(),
        },
    );
    let due_button = button(
        "📅 Срок",
        Action::Go {
            to: Screen::Due {
                item_id,
                back: Box::new(back.clone()),
            },
        },
    );

    if done || shopping {
        let mut row: Vec<_> = main.into_iter().collect();
        row.push(edit);
        ui = ui.row(row).row(vec![delete]);
    } else {
        let mut row: Vec<_> = main.into_iter().collect();
        row.push(due_button);
        ui = ui.row(row).row(vec![edit, delete]);
        if let Some(revert) = extra_revert {
            ui = ui.row(vec![revert]);
        }
    }
    Ok(ui.row(nav_row(&here, back.clone())))
}

/// Строка выполнения: план → факт, исполнитель и отклонение от плана.
fn completion_line(
    completion: &items::completions::Completion,
    names: &HashMap<Uuid, String>,
    tz: Tz,
    now: DateTime<Utc>,
) -> String {
    let today = now.with_timezone(&tz).date_naive();
    let plan = match (completion.due_at, completion.due_date) {
        (Some(at), _) => {
            let local = at.with_timezone(&tz);
            format!(
                "{}, {}",
                ru::fixed_date_label(local.date_naive(), today),
                local.format("%H:%M")
            )
        }
        (None, Some(date)) => ru::fixed_date_label(date, today),
        (None, None) => "без срока".to_owned(),
    };
    let actual = completion.completed_at.with_timezone(&tz);
    let mut line = format!(
        "{plan} → {}, {}",
        ru::fixed_date_label(actual.date_naive(), today),
        actual.format("%H:%M")
    );
    if let Some(name) = completion.actor_user_id.and_then(|id| names.get(&id)) {
        line.push_str(&format!(" · {name}"));
    }
    if let Some(deviation) = ru::completion_deviation(
        completion.due_at,
        completion.due_date,
        completion.completed_at,
        tz,
    ) {
        line.push_str(&format!(" · {deviation}"));
    }
    line
}

/// Экран быстрого срока.
pub async fn due(ctx: &mut Ctx<'_>, item_id: Uuid, back: &Screen) -> Result<Ui, Fail> {
    let (item, list) = load(ctx, item_id).await?;
    if !due_editable(&item, &list) {
        return view(ctx, item_id, back, None).await;
    }
    let here = Screen::Due {
        item_id,
        back: Box::new(back.clone()),
    };
    let title = clip(&item.title, 40);
    let mut ui = screen_heading_names(&here, "Срок", Some(&list.name), Some(&title), None);
    match ru::due_label(item.due_at, item.due_date, ctx.tz, ctx.now) {
        Some(due) => ui = ui.text(format!("Сейчас: {due}")),
        None => ui = ui.text("Сейчас без срока"),
    }

    let quick = |label: &str, due: QuickDue| {
        button(
            label,
            Action::DueSet {
                item_id,
                version: item.version,
                due,
                back: back.clone(),
            },
        )
    };
    ui = ui
        .row(vec![
            quick("Сегодня", QuickDue::Today),
            quick("Завтра", QuickDue::Tomorrow),
        ])
        .row(vec![
            quick("+1 неделя", QuickDue::WeekLater),
            button(
                "📅 Дата и время",
                Action::DueText {
                    item_id,
                    version: item.version,
                    back: back.clone(),
                },
            ),
        ]);
    if item.recurrence.is_none() {
        ui = ui.row(vec![quick("Без срока", QuickDue::None)]);
    }
    let card = Screen::Item {
        item_id,
        back: Box::new(back.clone()),
        completion_id: None,
    };
    Ok(ui.row(nav_row(&here, card)))
}

/// Сохранить выбранный быстрый срок и вернуться в карточку.
pub async fn set_due(
    ctx: &mut Ctx<'_>,
    item_id: Uuid,
    version: i32,
    choice: QuickDue,
    back: Screen,
) -> Result<(), Fail> {
    let (item, list) = load(ctx, item_id).await?;
    if !due_editable(&item, &list) {
        return ctx
            .present(Screen::Item {
                item_id,
                back: Box::new(back),
                completion_id: None,
            })
            .await;
    }
    if choice != QuickDue::None {
        let due = quick_due(&item, choice, ctx.now, ctx.tz)?;
        return apply_due(ctx, item_id, version, due, back).await;
    }

    let household = ctx.household()?.clone();
    let actor = ctx.actor()?;
    let patch = if item.due_at.is_some() {
        ItemPatch {
            due_at: Field::Null,
            ..ItemPatch::default()
        }
    } else if item.due_date.is_some() {
        ItemPatch {
            due_date: Field::Null,
            ..ItemPatch::default()
        }
    } else {
        ItemPatch::default()
    };
    let saved = items::update(
        &mut *ctx.conn,
        household.id,
        &actor,
        item_id,
        version,
        patch,
        &household.timezone,
        ctx.now,
    )
    .await;
    finish_due(ctx, item_id, saved, back).await
}

/// Перенести срок через `reschedule_occurrence` и вернуться в карточку.
pub async fn apply_due(
    ctx: &mut Ctx<'_>,
    item_id: Uuid,
    version: i32,
    due: Due,
    back: Screen,
) -> Result<(), Fail> {
    let (item, list) = load(ctx, item_id).await?;
    if !due_editable(&item, &list) {
        return ctx
            .present(Screen::Item {
                item_id,
                back: Box::new(back),
                completion_id: None,
            })
            .await;
    }
    let household = ctx.household()?.clone();
    let actor = ctx.actor()?;
    let saved = items::reschedule_occurrence(
        &mut *ctx.conn,
        household.id,
        &actor,
        item_id,
        version,
        due,
        &household.timezone,
        ctx.now,
    )
    .await;
    finish_due(ctx, item_id, saved, back).await
}

/// Баннер нового срока и карточка; конфликт показывает актуальную карточку.
async fn finish_due(
    ctx: &mut Ctx<'_>,
    item_id: Uuid,
    saved: Result<Item, DomainError>,
    back: Screen,
) -> Result<(), Fail> {
    match saved {
        Ok(item) => match ru::due_label(item.due_at, item.due_date, ctx.tz, ctx.now) {
            Some(due) => ctx.banner(format!("Срок: {due}")),
            None => ctx.banner("Срок снят"),
        },
        Err(DomainError::Conflict { .. }) => {
            ctx.banner(super::CHANGED_STATE_MESSAGE);
        }
        Err(error) => return Err(error.into()),
    }
    ctx.present(Screen::Item {
        item_id,
        back: Box::new(back),
        completion_id: None,
    })
    .await
}

/// Срок по кнопке быстрого выбора. Время суток и вседневность сохраняются.
fn quick_due(
    item: &Item,
    choice: QuickDue,
    now: DateTime<Utc>,
    tz: Tz,
) -> Result<Due, DomainError> {
    let today = now.with_timezone(&tz).date_naive();
    let current = item
        .due_at
        .map(|at| at.with_timezone(&tz).date_naive())
        .or(item.due_date);
    let date = match choice {
        QuickDue::Today => today,
        QuickDue::Tomorrow => today + Duration::days(1),
        QuickDue::WeekLater => current.map_or(today, |date| date.max(today)) + Duration::days(7),
        QuickDue::None => unreachable!("«Без срока» не использует дату правила"),
    };
    match item.due_at {
        Some(at) => Ok(Due::Timed(resolve_local(
            tz,
            date,
            at.with_timezone(&tz).time(),
        )?)),
        None => Ok(Due::AllDay(date)),
    }
}

/// Вхождение, которое станет сроком после выполнения текущего.
///
/// Совпадает с расчётом `complete`: текущий срок (или первое вхождение серии
/// без срока), слот `occurrence_date` и время серии `occurrence_time`
/// перенесённого вхождения.
fn next_occurrence(item: &Item, ctx: &Ctx<'_>) -> Option<String> {
    let rule = item.recurrence.as_ref()?;
    if item.status == ItemStatus::Done {
        return None;
    }
    let today = ctx.now.with_timezone(&ctx.tz).date_naive();
    // Ошибку расчёта намеренно не показываем: карточка — экран просмотра, и
    // без строки «Следующее» она должна открыться, а не упасть.
    let current = match (item.due_at, item.due_date) {
        (Some(at), _) => Due::Timed(at),
        (None, Some(date)) => Due::AllDay(date),
        (None, None) => rule.first_due(today).ok()?,
    };
    let next = rule.next_after(current, ctx.now).ok()??;
    match next {
        Due::Timed(at) => ru::due_label(Some(at), None, ctx.tz, ctx.now),
        Due::AllDay(date) => ru::due_label(None, Some(date), ctx.tz, ctx.now),
    }
}

fn is_overdue(item: &Item, ctx: &Ctx<'_>) -> bool {
    let today = ctx.now.with_timezone(&ctx.tz).date_naive();
    match (item.due_at, item.due_date) {
        (Some(at), _) => at < ctx.now,
        (None, Some(date)) => date < today,
        (None, None) => false,
    }
}

/// Результат удаления с кнопкой отмены.
pub async fn deleted(
    ctx: &mut Ctx<'_>,
    item_id: Uuid,
    title: &str,
    back: &Screen,
) -> Result<Ui, Fail> {
    let parent = parent_list_name(ctx, back).await?;
    let screen = Screen::Deleted {
        item_id,
        title: title.to_owned(),
        back: Box::new(back.clone()),
    };
    let ui = screen_heading(&screen, &format!("🗑 Удалено: {title}"), parent.as_deref());
    let mut row = vec![button(
        "↩️ Отменить",
        Action::DeleteUndo {
            item_id,
            back: back.clone(),
        },
    )];
    row.extend(nav_row(&screen, back.clone()));
    Ok(ui.row(row))
}

/// Подтверждение удаления.
pub async fn confirm_delete(ctx: &mut Ctx<'_>, item_id: Uuid, back: &Screen) -> Result<Ui, Fail> {
    let (item, _) = load(ctx, item_id).await?;
    let screen = Screen::ConfirmDelete {
        item_id,
        back: Box::new(back.clone()),
    };
    let parent = parent_list_name(ctx, back).await?;
    let mut ui = screen_heading(
        &screen,
        &format!("Удалить «{}»?", item.title),
        parent.as_deref(),
    );
    if item.recurrence.is_some() {
        ui = ui.text("Будет удалена вся серия повторений.");
    }
    let mut row = vec![button(
        "🗑 Да, удалить",
        Action::DeleteDo {
            item_id,
            back: back.clone(),
        },
    )];
    row.extend(nav_row(
        &screen,
        Screen::Item {
            item_id,
            back: Box::new(back.clone()),
            completion_id: None,
        },
    ));
    Ok(ui.row(row))
}

/// Завершить элемент.
pub async fn complete(
    ctx: &mut Ctx<'_>,
    item_id: Uuid,
    version: i32,
    back: Screen,
) -> Result<(), Fail> {
    let household = ctx.household()?.clone();
    let actor = ctx.actor()?;
    let item = items::complete(
        &mut *ctx.conn,
        household.id,
        &actor,
        item_id,
        Some(version),
        &household.timezone,
        ctx.now,
    )
    .await?;
    let list = lists::get(&mut *ctx.conn, household.id, item.list_id).await?;
    if list.kind == ListKind::Tasks {
        let mut text = format!("✅ Выполнено: {}", clip(&item.title, 80));
        if item.status == ItemStatus::Open
            && let Some(next) = ru::due_label(item.due_at, item.due_date, ctx.tz, ctx.now)
        {
            text.push_str(&format!("\nСледующее: {next}"));
        }
        ctx.banner_with(
            text,
            vec![button(
                "↩️ Отменить",
                Action::Undo {
                    item_id,
                    back: back.clone(),
                },
            )],
        );
        ctx.present(back).await
    } else {
        ctx.toast(format!("Отмечено: {}", clip(&item.title, 60)));
        ctx.present(back).await
    }
}

/// Вернуть элемент в работу.
pub async fn reopen(
    ctx: &mut Ctx<'_>,
    item_id: Uuid,
    version: i32,
    completion_id: Option<Uuid>,
    back: Screen,
) -> Result<(), Fail> {
    let household_id = ctx.household()?.id;
    let actor = ctx.actor()?;
    let (_, list) = load(ctx, item_id).await?;
    let result = if list.kind == ListKind::Tasks {
        let completion_id = match completion_id {
            Some(completion_id) => Some(completion_id),
            None => items::completions::latest_for_item(&mut *ctx.conn, household_id, item_id)
                .await?
                .map(|event| event.completion.event_id),
        };
        match completion_id {
            Some(completion_id) => {
                items::revert_completion(
                    &mut *ctx.conn,
                    household_id,
                    &actor,
                    item_id,
                    completion_id,
                    ctx.now,
                )
                .await
            }
            // Выполнения нет: старые данные за сроком хранения событий.
            None => {
                items::reopen(
                    &mut *ctx.conn,
                    household_id,
                    &actor,
                    item_id,
                    Some(version),
                    ctx.now,
                )
                .await
            }
        }
    } else {
        // Покупки возвращаются прежним путём.
        items::reopen(
            &mut *ctx.conn,
            household_id,
            &actor,
            item_id,
            Some(version),
            ctx.now,
        )
        .await
    };
    result?;
    ctx.toast("Возвращено в работу");
    ctx.present(back).await
}

/// Отменить последнее действие над элементом.
pub async fn undo(ctx: &mut Ctx<'_>, item_id: Uuid, back: Screen) -> Result<(), Fail> {
    let household_id = ctx.household()?.id;
    let actor = ctx.actor()?;
    items::undo_last(&mut *ctx.conn, household_id, &actor, item_id, ctx.now).await?;
    ctx.toast("Действие отменено");
    ctx.present(back).await
}

/// Показать подтверждение удаления.
pub async fn delete_ask(ctx: &mut Ctx<'_>, item_id: Uuid, back: Screen) -> Result<(), Fail> {
    ctx.present(Screen::ConfirmDelete {
        item_id,
        back: Box::new(back),
    })
    .await
}

/// Удалить элемент.
pub async fn delete_do(ctx: &mut Ctx<'_>, item_id: Uuid, back: Screen) -> Result<(), Fail> {
    let household_id = ctx.household()?.id;
    let actor = ctx.actor()?;
    let item = items::soft_delete(&mut *ctx.conn, household_id, &actor, item_id, ctx.now).await?;
    ctx.present(Screen::Deleted {
        item_id: item.id,
        title: item.title.clone(),
        back: Box::new(back),
    })
    .await
}

/// Итог отмены удаления одного элемента.
pub enum UndoOutcome {
    /// Удаление отменено; элемент возвращён (его название).
    Restored(String),
    /// Окно отмены истекло.
    Expired,
    /// Отменять нечего: действия нет или элемент уже вернули.
    Nothing,
}

/// Отменить удаление одного элемента, только если он всё ещё удалён.
///
/// Кнопка со старого экрана не должна откатывать другое действие: удаление —
/// только один из видов событий, которые умеет отменять `undo_last`.
pub async fn undo_deleted(ctx: &mut Ctx<'_>, item_id: Uuid) -> Result<UndoOutcome, Fail> {
    let household_id = ctx.household()?.id;
    let actor = ctx.actor()?;
    match items::is_deleted(&mut *ctx.conn, household_id, item_id).await {
        Ok(true) => {}
        Ok(false) | Err(DomainError::NotFound) => return Ok(UndoOutcome::Nothing),
        Err(error) => return Err(error.into()),
    }
    match items::undo_last(&mut *ctx.conn, household_id, &actor, item_id, ctx.now).await {
        Ok(item) => Ok(UndoOutcome::Restored(item.title)),
        // Истёкшее окно — единственный конфликт с машиночитаемой причиной.
        Err(DomainError::Conflict { details, .. })
            if details.get("reason").and_then(|value| value.as_str())
                == Some(items::UNDO_WINDOW_EXPIRED) =>
        {
            Ok(UndoOutcome::Expired)
        }
        // «Нечего отменять»: действия нет или элемент уже вернули.
        Err(DomainError::Conflict { .. } | DomainError::NotFound) => Ok(UndoOutcome::Nothing),
        Err(error) => Err(error.into()),
    }
}

/// Отменить удаление и вернуться к списку.
pub async fn delete_undo(ctx: &mut Ctx<'_>, item_id: Uuid, back: Screen) -> Result<(), Fail> {
    match undo_deleted(ctx, item_id).await? {
        UndoOutcome::Restored(title) => ctx.toast(format!("Возвращено: {}", clip(&title, 60))),
        UndoOutcome::Expired => ctx.toast("Время отмены истекло"),
        UndoOutcome::Nothing => ctx.toast("Нечего отменять"),
    }
    ctx.present(back).await
}

#[cfg(test)]
mod tests {
    use chrono::NaiveDate;

    use super::*;

    fn rule(freq: RecurrenceFreq, interval: i16) -> Recurrence {
        Recurrence {
            freq,
            interval,
            byweekday: vec![1, 3],
            until: None,
            timezone: "Europe/Moscow".into(),
            anchor_date: NaiveDate::from_ymd_opt(2026, 11, 5).expect("date"),
            month_rule: Some(MonthRule::AnchorDay),
            occurrence_date: None,
            occurrence_time: None,
        }
    }

    #[test]
    fn recurrence_is_described_in_russian() {
        assert_eq!(
            describe_recurrence(&rule(RecurrenceFreq::Daily, 1)),
            "Каждый день"
        );
        assert_eq!(
            describe_recurrence(&rule(RecurrenceFreq::Daily, 3)),
            "Каждые 3 дня"
        );
        assert_eq!(
            describe_recurrence(&rule(RecurrenceFreq::Weekly, 1)),
            "Каждую неделю: пн, ср"
        );
        assert_eq!(
            describe_recurrence(&rule(RecurrenceFreq::Monthly, 1)),
            "Каждый месяц, 5-го"
        );
        assert_eq!(
            describe_recurrence(&rule(RecurrenceFreq::Yearly, 2)),
            "Каждые 2 года"
        );
        let mut last = rule(RecurrenceFreq::Monthly, 1);
        last.month_rule = Some(MonthRule::LastDay);
        assert_eq!(describe_recurrence(&last), "Каждый месяц, в последний день");
    }
}
