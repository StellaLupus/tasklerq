//! Экраны покупок: список, очистка выполненных и возврат.

use uuid::Uuid;

use crate::domain::models::{Field, Item, ItemPatch, ItemStatus, List, ListKind};
use crate::domain::validation::validate_new_item;
use crate::storage::repositories::{items, lists};
use crate::telegram::rich::button;
use crate::telegram::ru;
use crate::telegram::views;

use super::card::{self, UndoOutcome};
use super::model::{
    Action, DuplicateChoice, ItemDraft, PAGE_SIZE, Screen, Ui, addition_rows, duplicate_notice,
    screen_heading, status_rows,
};
use super::screens::{back_button, clip, list_back, nav_row};
use super::{Ctx, Fail};

/// Подтверждение нужно, если выполненных позиций больше этого числа.
const CONFIRM_ABOVE: usize = 5;

/// Название и количество позиции — так же, как строка списка без отметки.
fn position_label(item: &Item) -> String {
    ru::position_label(&clip(&item.title, 36), item.quantity, item.unit.as_deref())
}

fn item_label(item: &Item) -> String {
    let mark = if item.status == ItemStatus::Done {
        "☑"
    } else {
        "☐"
    };
    format!("{mark} {}", position_label(item))
}

/// Список покупок или чек-лист.
pub async fn list_view(
    ctx: &mut Ctx<'_>,
    list: &List,
    page: u32,
    edit: bool,
    from_lists: bool,
) -> Result<Ui, Fail> {
    let household_id = ctx.household()?.id;
    let mut all = items::list_by_list(&mut *ctx.conn, household_id, list.id).await?;
    // Сначала «Купить», затем «Куплено»; порядок внутри групп задан репозиторием.
    all.sort_by_key(|item| item.status == ItemStatus::Done);
    let open = all
        .iter()
        .filter(|item| item.status == ItemStatus::Open)
        .count();

    let heading = if edit && !all.is_empty() {
        format!("{} · {}", list.name, ru::EDIT_MODE_TITLE)
    } else {
        format!("{} · {open}", list.name)
    };
    let trail = Screen::List {
        list_id: list.id,
        page: 0,
        edit,
        from_lists,
    }
    .trail(&heading, None);
    let add = button(
        "➕ Добавить",
        Action::Add {
            list_id: Some(list.id),
            kind: list.kind,
            back: Some(Screen::List {
                list_id: list.id,
                page,
                edit,
                from_lists,
            }),
        },
    );
    if all.is_empty() {
        let mut row = vec![add];
        row.extend(nav_row(
            &Screen::List {
                list_id: list.id,
                page: 0,
                edit: false,
                from_lists,
            },
            list_back(from_lists),
        ));
        return Ok(Ui::new()
            .heading(trail)
            .text("Список покупок пуст")
            .row(row));
    }
    let mut ui = Ui::new().heading(trail);

    let pages = all.len().div_ceil(PAGE_SIZE).max(1);
    let page = usize::try_from(page).unwrap_or(0).min(pages - 1);
    let (start, end) = (page * PAGE_SIZE, ((page + 1) * PAGE_SIZE).min(all.len()));
    let here = |page: usize| Screen::List {
        list_id: list.id,
        page: u32::try_from(page).unwrap_or(0),
        edit,
        from_lists,
    };

    let mut current: Option<ItemStatus> = None;
    for item in &all[start..end] {
        if current != Some(item.status) {
            ui = ui.bold(if item.status == ItemStatus::Open {
                "Купить"
            } else {
                "Куплено"
            });
            current = Some(item.status);
        }
        let action = if edit {
            Action::Go {
                to: Screen::Item {
                    item_id: item.id,
                    back: Box::new(here(page)),
                    completion_id: None,
                },
            }
        } else {
            match item.status {
                ItemStatus::Open => Action::Complete {
                    item_id: item.id,
                    version: item.version,
                    back: here(page),
                },
                ItemStatus::Done => Action::Reopen {
                    item_id: item.id,
                    version: item.version,
                    back: here(page),
                    completion_id: None,
                },
            }
        };
        ui = ui.row(vec![button(item_label(item), action)]);
    }

    if pages > 1 {
        let mut row = Vec::new();
        if page > 0 {
            row.push(back_button("←", here(page - 1)));
        }
        if page + 1 < pages {
            row.push(back_button("→", here(page + 1)));
        }
        ui = ui
            .text(format!("Страница {} из {pages}", page + 1))
            .row(row);
    }

    let mut footer = vec![add];
    if all.iter().any(|item| item.status == ItemStatus::Done) {
        footer.push(button(
            "🧹 Очистить",
            Action::ClearAsk {
                list_id: list.id,
                from_lists,
            },
        ));
    }
    let toggle = button(
        if edit {
            ru::EDIT_FINISH_BUTTON
        } else {
            ru::EDIT_BUTTON
        },
        Action::Go {
            to: Screen::List {
                list_id: list.id,
                page: u32::try_from(page).unwrap_or(0),
                edit: !edit,
                from_lists,
            },
        },
    );
    let mut nav = vec![button("🔎 Поиск", Action::SearchStart)];
    nav.extend(nav_row(&here(page), list_back(from_lists)));
    if edit {
        // «✓ Закончить правку» длиннее соседних подписей: в ряду из трёх на
        // узком Android-экране она обрезается, поэтому занимает свой ряд.
        Ok(ui.row(footer).row(vec![toggle]).row(nav))
    } else {
        footer.push(toggle);
        Ok(ui.row(footer).row(nav))
    }
}

async fn done_items(ctx: &mut Ctx<'_>, list_id: Uuid) -> Result<(List, Vec<Item>), Fail> {
    let household_id = ctx.household()?.id;
    let list = lists::get_visible(&mut *ctx.conn, household_id, list_id).await?;
    let done = items::list_by_list(&mut *ctx.conn, household_id, list_id)
        .await?
        .into_iter()
        .filter(|item| item.status == ItemStatus::Done)
        .collect();
    Ok((list, done))
}

/// Очистить выполненные позиции: с подтверждением, если их много.
pub async fn clear_ask(ctx: &mut Ctx<'_>, list_id: Uuid, from_lists: bool) -> Result<(), Fail> {
    let (_, done) = done_items(ctx, list_id).await?;
    if done.is_empty() {
        ctx.toast("Нечего очищать");
        return ctx
            .present(Screen::List {
                list_id,
                page: 0,
                edit: false,
                from_lists,
            })
            .await;
    }
    if done.len() > CONFIRM_ABOVE {
        return ctx
            .present(Screen::ConfirmClear {
                list_id,
                from_lists,
            })
            .await;
    }
    clear_do(ctx, list_id, from_lists).await
}

/// Подтверждение очистки.
pub async fn confirm_clear(ctx: &mut Ctx<'_>, list_id: Uuid, from_lists: bool) -> Result<Ui, Fail> {
    let (list, done) = done_items(ctx, list_id).await?;
    let screen = Screen::ConfirmClear {
        list_id,
        from_lists,
    };
    let mut row = vec![button(
        "🧹 Да, очистить",
        Action::ClearDo {
            list_id,
            from_lists,
        },
    )];
    row.extend(nav_row(
        &screen,
        Screen::List {
            list_id,
            page: 0,
            edit: false,
            from_lists,
        },
    ));
    Ok(screen_heading(
        &screen,
        &format!(
            "Удалить {}?",
            ru::positions(i64::try_from(done.len()).unwrap_or(0))
        ),
        Some(&list.name),
    )
    .text(format!(
        "Из списка «{}» будут убраны выполненные позиции. Это можно отменить в течение 10 минут.",
        list.name
    ))
    .row(row))
}

/// Удалить выполненные позиции.
pub async fn clear_do(ctx: &mut Ctx<'_>, list_id: Uuid, from_lists: bool) -> Result<(), Fail> {
    let household_id = ctx.household()?.id;
    let actor = ctx.actor()?;
    let (_, done) = done_items(ctx, list_id).await?;
    if done.is_empty() {
        ctx.toast("Нечего очищать");
        return ctx
            .present(Screen::List {
                list_id,
                page: 0,
                edit: false,
                from_lists,
            })
            .await;
    }
    let mut removed = Vec::with_capacity(done.len());
    for item in done {
        items::soft_delete(&mut *ctx.conn, household_id, &actor, item.id, ctx.now).await?;
        removed.push(item.id);
    }
    ctx.present(Screen::Cleared {
        list_id,
        item_ids: removed,
        from_lists,
    })
    .await
}

/// Результат очистки с кнопкой отмены.
pub async fn cleared(
    ctx: &mut Ctx<'_>,
    list_id: Uuid,
    item_ids: &[Uuid],
    from_lists: bool,
) -> Result<Ui, Fail> {
    let household_id = ctx.household()?.id;
    let parent = match lists::get_visible(&mut *ctx.conn, household_id, list_id).await {
        Ok(list) => Some(list.name),
        Err(crate::domain::error::DomainError::NotFound) => None,
        Err(error) => return Err(error.into()),
    };
    let screen = Screen::Cleared {
        list_id,
        item_ids: item_ids.to_vec(),
        from_lists,
    };
    let mut row = vec![button(
        "↩️ Отменить",
        Action::ClearUndo {
            list_id,
            item_ids: item_ids.to_vec(),
            from_lists,
        },
    )];
    row.extend(nav_row(
        &screen,
        Screen::List {
            list_id,
            page: 0,
            edit: false,
            from_lists,
        },
    ));
    Ok(screen_heading(
        &screen,
        &format!(
            "Убрано: {}",
            ru::positions(i64::try_from(item_ids.len()).unwrap_or(0))
        ),
        parent.as_deref(),
    )
    .row(row))
}

/// Вернуть очищенные позиции в течение окна отмены.
pub async fn clear_undo(
    ctx: &mut Ctx<'_>,
    list_id: Uuid,
    item_ids: &[Uuid],
    from_lists: bool,
) -> Result<(), Fail> {
    let household_id = ctx.household()?.id;
    let mut restored = 0_i64;
    let mut expired = false;
    for item_id in item_ids {
        // Позиция, которую уже вернули или изменили, не трогается: helper
        // сначала проверяет, что она всё ещё удалена.
        match card::undo_deleted(ctx, *item_id).await? {
            UndoOutcome::Restored(_) => restored += 1,
            UndoOutcome::Expired => expired = true,
            UndoOutcome::Nothing => {}
        }
    }
    if restored > 0 {
        ctx.toast(format!("Возвращено: {}", ru::positions(restored)));
    } else if expired {
        ctx.toast("Время отмены истекло");
    } else {
        ctx.toast("Нечего отменять");
    }
    let kind_ok = lists::get_visible(&mut *ctx.conn, household_id, list_id)
        .await
        // 0009-events 8.3: экран списка событий будет повесткой, а не списком
        // покупок; пока он попадает в не-задачи.
        .map(|list| list.kind != ListKind::Tasks)
        .unwrap_or(false);
    let target = if kind_ok {
        Screen::List {
            list_id,
            page: 0,
            edit: false,
            from_lists,
        }
    } else {
        Screen::Menu
    };
    ctx.present(target).await
}

/// Перерисовать список покупок после выбора по дубликату. Если пользователь
/// уже не в этом списке, экран не заменяется: баннер ставится на текущий.
async fn present_after_duplicate(ctx: &mut Ctx<'_>, list_id: Uuid) -> Result<(), Fail> {
    let current = views::get(&mut *ctx.conn, ctx.chat_id)
        .await?
        .and_then(|view| serde_json::from_value::<Screen>(view.state).ok());
    let screen = match current {
        Some(Screen::List {
            list_id: current_id,
            page,
            edit,
            from_lists,
        }) if current_id == list_id => Screen::List {
            list_id,
            page,
            edit,
            from_lists,
        },
        _ => return ctx.refresh().await,
    };
    ctx.present(screen).await
}

/// Показать результат выбора и, если остались дубликаты, следующий выбор.
async fn show_duplicate_result(
    ctx: &mut Ctx<'_>,
    list_id: Uuid,
    label: &str,
    rest: &[DuplicateChoice],
    draft_id: Option<Uuid>,
    created_ids: &[Uuid],
) -> Result<(), Fail> {
    let text = match rest.split_first() {
        Some(_) => {
            let mut text = format!("Добавлено: {label}.");
            if let Some(notice) = duplicate_notice(rest) {
                text.push(' ');
                text.push_str(&notice);
            }
            text
        }
        None => format!("Добавлено: {label}"),
    };
    let rows = addition_rows(rest, list_id, created_ids, draft_id);
    ctx.banner_rows(text, rows);
    present_after_duplicate(ctx, list_id).await
}

/// «➕ Увеличить»: обновить существующую позицию до суммы.
pub async fn duplicate_increase(
    ctx: &mut Ctx<'_>,
    item_id: Uuid,
    version: i32,
    quantity: f64,
    rest: Vec<DuplicateChoice>,
    draft_id: Option<Uuid>,
    created_ids: Vec<Uuid>,
) -> Result<(), Fail> {
    let household = ctx.household()?.clone();
    let actor = ctx.actor()?;
    let patch = ItemPatch {
        quantity: Field::Value(quantity),
        ..ItemPatch::default()
    };
    match items::update(
        &mut *ctx.conn,
        household.id,
        &actor,
        item_id,
        version,
        patch,
        &household.timezone,
        ctx.now,
    )
    .await
    {
        Ok(item) => {
            let label = position_label(&item);
            show_duplicate_result(ctx, item.list_id, &label, &rest, draft_id, &created_ids).await
        }
        Err(crate::domain::error::DomainError::Conflict { .. })
        | Err(crate::domain::error::DomainError::NotFound) => {
            if let Ok(current) = items::get(&mut *ctx.conn, household.id, item_id).await {
                let mut text = "Позиция изменилась, откройте список.".to_owned();
                if let Some(notice) = duplicate_notice(&rest) {
                    text.push(' ');
                    text.push_str(&notice);
                }
                let rows = addition_rows(&rest, current.list_id, &created_ids, draft_id);
                ctx.banner_rows(text, rows);
                return present_after_duplicate(ctx, current.list_id).await;
            }
            let text = "Позиция изменилась, откройте список";
            let rows = status_rows(&created_ids, draft_id);
            ctx.banner_rows(text, rows);
            ctx.refresh().await
        }
        Err(error) => Err(error.into()),
    }
}

/// «📄 Добавить отдельно»: создать вторую позицию с тем же названием.
#[allow(clippy::too_many_arguments)]
pub async fn duplicate_separate(
    ctx: &mut Ctx<'_>,
    list_id: Uuid,
    title: String,
    quantity: f64,
    unit: Option<String>,
    rest: Vec<DuplicateChoice>,
    draft_id: Option<Uuid>,
    created_ids: Vec<Uuid>,
) -> Result<(), Fail> {
    let household = ctx.household()?.clone();
    let actor = ctx.actor()?;
    let list = lists::get_visible(&mut *ctx.conn, household.id, list_id).await?;
    let draft = ItemDraft {
        title,
        quantity: Some(quantity),
        unit,
        ..ItemDraft::default()
    };
    let new_item = draft.to_new_item(list.id, ctx.tz, list.kind)?;
    let today = ctx.now.with_timezone(&ctx.tz).date_naive();
    let validated = validate_new_item(list.kind, ctx.tz.name(), new_item, today)?;
    let item = items::create(
        &mut *ctx.conn,
        household.id,
        &actor,
        list.id,
        validated,
        None,
        ctx.now,
    )
    .await?;
    let label = position_label(&item);
    show_duplicate_result(ctx, list.id, &label, &rest, draft_id, &created_ids).await
}

/// Перерисовать список после отмены добавления, только если он открыт.
/// Пропавший список и другой экран (например, карточка задачи) остаются как
/// есть.
async fn present_after_undo(ctx: &mut Ctx<'_>, list_id: Option<Uuid>) -> Result<(), Fail> {
    let Some(list_id) = list_id else {
        return ctx.refresh().await;
    };
    let household_id = ctx.household()?.id;
    match lists::get_visible(&mut *ctx.conn, household_id, list_id).await {
        Ok(_) => present_after_duplicate(ctx, list_id).await,
        Err(crate::domain::error::DomainError::NotFound) => ctx.refresh().await,
        Err(error) => Err(error.into()),
    }
}

/// Баннер отмены с кнопкой «Открыть» ожидающего черновика.
fn undo_banner(ctx: &mut Ctx<'_>, text: &str, draft_id: Option<Uuid>) {
    let rows = status_rows(&[], draft_id);
    ctx.banner_rows(text, rows);
}

/// «↩️ Отменить» в баннере прямого добавления: мягко удалить созданные
/// позиции, если с их создания прошло не больше 10 минут.
///
/// Окно считается от `created_at` позиции, а не от нажатия. Уже удалённые
/// позиции пропускаются: их нечего отменять. Чужое добавление отменяет только
/// автор или владелец household.
pub async fn added_undo(
    ctx: &mut Ctx<'_>,
    item_ids: &[Uuid],
    draft_id: Option<Uuid>,
) -> Result<(), Fail> {
    let household_id = ctx.household()?.id;
    let actor = ctx.actor()?;

    let mut alive = Vec::new();
    let mut list_id = None;
    for item_id in item_ids {
        match items::get(&mut *ctx.conn, household_id, *item_id).await {
            Ok(item) => {
                list_id.get_or_insert(item.list_id);
                alive.push(item);
            }
            Err(crate::domain::error::DomainError::NotFound) => {}
            Err(error) => return Err(error.into()),
        }
    }

    if alive.is_empty() {
        undo_banner(ctx, "Добавление уже отменено", draft_id);
        return present_after_undo(ctx, list_id).await;
    }

    let window = chrono::Duration::minutes(items::UNDO_WINDOW_MINUTES);
    if alive.iter().any(|item| ctx.now - item.created_at > window) {
        undo_banner(ctx, "Время отмены истекло", draft_id);
        return present_after_undo(ctx, list_id).await;
    }

    for item in &alive {
        items::soft_delete(&mut *ctx.conn, household_id, &actor, item.id, ctx.now).await?;
    }
    undo_banner(ctx, "Добавление отменено", draft_id);
    present_after_undo(ctx, list_id).await
}
