//! Экраны покупок: список, очистка выполненных и возврат.

use uuid::Uuid;

use crate::domain::models::{Item, ItemStatus, List, ListKind};
use crate::storage::repositories::{items, lists};
use crate::telegram::rich::button;
use crate::telegram::ru;

use super::model::{Action, PAGE_SIZE, Screen, Ui};
use super::screens::{back_button, clip};
use super::{Ctx, Fail};

/// Подтверждение нужно, если выполненных позиций больше этого числа.
const CONFIRM_ABOVE: usize = 5;

fn item_label(item: &Item) -> String {
    let mark = if item.status == ItemStatus::Done {
        "☑"
    } else {
        "☐"
    };
    let mut label = format!("{mark} {}", clip(&item.title, 36));
    if let Some(quantity) = item.quantity {
        label.push_str(&format!(
            " · {}",
            ru::quantity_label(quantity, item.unit.as_deref())
        ));
    }
    label
}

/// Список покупок или чек-лист.
pub async fn list_view(ctx: &mut Ctx<'_>, list: &List, page: u32) -> Result<Ui, Fail> {
    let household_id = ctx.household()?.id;
    let mut all = items::list_by_list(&mut *ctx.conn, household_id, list.id).await?;
    // Сначала «Купить», затем «Куплено»; порядок внутри групп задан репозиторием.
    all.sort_by_key(|item| item.status == ItemStatus::Done);
    let open = all
        .iter()
        .filter(|item| item.status == ItemStatus::Open)
        .count();

    let mut ui = Ui::new().heading(format!("{} · {open}", list.name));
    let add = button(
        "➕ Добавить",
        Action::Add {
            list_id: Some(list.id),
            kind: list.kind,
        },
    );
    if all.is_empty() {
        return Ok(ui
            .text("Список покупок пуст")
            .row(vec![add, back_button("⬅️ Назад", Screen::Menu)]));
    }

    let pages = all.len().div_ceil(PAGE_SIZE).max(1);
    let page = usize::try_from(page).unwrap_or(0).min(pages - 1);
    let (start, end) = (page * PAGE_SIZE, ((page + 1) * PAGE_SIZE).min(all.len()));
    let here = |page: usize| Screen::List {
        list_id: list.id,
        page: u32::try_from(page).unwrap_or(0),
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
        let toggle = match item.status {
            ItemStatus::Open => Action::Complete {
                item_id: item.id,
                version: item.version,
                back: here(page),
            },
            ItemStatus::Done => Action::Reopen {
                item_id: item.id,
                version: item.version,
                back: here(page),
            },
        };
        ui = ui.row(vec![
            button(item_label(item), toggle),
            back_button(
                "⋯",
                Screen::Item {
                    item_id: item.id,
                    back: Box::new(here(page)),
                },
            ),
        ]);
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
        footer.push(button("🧹 Очистить", Action::ClearAsk { list_id: list.id }));
    }
    Ok(ui.row(footer).row(vec![
        button("🔎 Поиск", Action::SearchStart),
        back_button("⬅️ Назад", Screen::Menu),
    ]))
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
pub async fn clear_ask(ctx: &mut Ctx<'_>, list_id: Uuid) -> Result<(), Fail> {
    let (_, done) = done_items(ctx, list_id).await?;
    if done.is_empty() {
        ctx.toast("Нечего очищать");
        return ctx.present(Screen::List { list_id, page: 0 }).await;
    }
    if done.len() > CONFIRM_ABOVE {
        return ctx.present(Screen::ConfirmClear { list_id }).await;
    }
    clear_do(ctx, list_id).await
}

/// Подтверждение очистки.
pub async fn confirm_clear(ctx: &mut Ctx<'_>, list_id: Uuid) -> Result<Ui, Fail> {
    let (list, done) = done_items(ctx, list_id).await?;
    Ok(Ui::new()
        .heading(format!("Удалить {}?", ru::positions(i64::try_from(done.len()).unwrap_or(0))))
        .text(format!("Из списка «{}» будут убраны выполненные позиции. Это можно отменить в течение 10 минут.", list.name))
        .row(vec![
            button("🧹 Да, очистить", Action::ClearDo { list_id }),
            back_button("Отмена", Screen::List { list_id, page: 0 }),
        ]))
}

/// Удалить выполненные позиции.
pub async fn clear_do(ctx: &mut Ctx<'_>, list_id: Uuid) -> Result<(), Fail> {
    let household_id = ctx.household()?.id;
    let actor = ctx.actor()?;
    let (_, done) = done_items(ctx, list_id).await?;
    if done.is_empty() {
        ctx.toast("Нечего очищать");
        return ctx.present(Screen::List { list_id, page: 0 }).await;
    }
    let mut removed = Vec::with_capacity(done.len());
    for item in done {
        items::soft_delete(&mut *ctx.conn, household_id, &actor, item.id, ctx.now).await?;
        removed.push(item.id);
    }
    ctx.present(Screen::Cleared {
        list_id,
        item_ids: removed,
    })
    .await
}

/// Результат очистки с кнопкой отмены.
pub async fn cleared(ctx: &mut Ctx<'_>, list_id: Uuid, item_ids: &[Uuid]) -> Result<Ui, Fail> {
    let _ = ctx;
    Ok(Ui::new()
        .heading(format!(
            "Убрано: {}",
            ru::positions(i64::try_from(item_ids.len()).unwrap_or(0))
        ))
        .row(vec![
            button(
                "↩️ Отменить",
                Action::ClearUndo {
                    list_id,
                    item_ids: item_ids.to_vec(),
                },
            ),
            back_button("🛒 К списку", Screen::List { list_id, page: 0 }),
        ]))
}

/// Вернуть очищенные позиции в течение окна отмены.
pub async fn clear_undo(ctx: &mut Ctx<'_>, list_id: Uuid, item_ids: &[Uuid]) -> Result<(), Fail> {
    let household_id = ctx.household()?.id;
    let actor = ctx.actor()?;
    let mut restored = 0_i64;
    for item_id in item_ids {
        // `undo_last` соблюдает окно в 10 минут и отменяет только удаление.
        match items::undo_last(&mut *ctx.conn, household_id, &actor, *item_id, ctx.now).await {
            Ok(_) => restored += 1,
            Err(crate::domain::error::DomainError::Database(error)) => {
                return Err(Fail::Infra(error));
            }
            Err(_) => {}
        }
    }
    if restored == 0 {
        ctx.toast("Время отмены истекло");
    } else {
        ctx.toast(format!("Возвращено: {}", ru::positions(restored)));
    }
    let kind_ok = lists::get_visible(&mut *ctx.conn, household_id, list_id)
        .await
        .map(|list| list.kind != ListKind::Tasks)
        .unwrap_or(false);
    let target = if kind_ok {
        Screen::List { list_id, page: 0 }
    } else {
        Screen::Menu
    };
    ctx.present(target).await
}
