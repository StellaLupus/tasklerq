//! Карточка элемента и действия над ним: завершение, возврат, отмена и
//! удаление.

use std::collections::HashMap;

use chrono::Datelike;
use uuid::Uuid;

use crate::domain::models::{
    Item, ItemPriority, ItemStatus, List, ListKind, MonthRule, Recurrence, RecurrenceFreq,
};
use crate::storage::repositories::{items, lists, users};
use crate::telegram::rich::{Span, button};
use crate::telegram::ru;

use super::model::{Action, RecurrenceDraft, Screen, Step, Ui};
use super::screens::{back_button, clip};
use super::{Ctx, Fail};

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

async fn names(ctx: &mut Ctx<'_>) -> Result<HashMap<Uuid, String>, Fail> {
    let household_id = ctx.household()?.id;
    Ok(users::list_active(&mut *ctx.conn, household_id)
        .await?
        .into_iter()
        .map(|user| (user.id, user.display_name))
        .collect())
}

/// Карточка элемента.
pub async fn view(ctx: &mut Ctx<'_>, item_id: Uuid, back: &Screen) -> Result<Ui, Fail> {
    let (item, list) = load(ctx, item_id).await?;
    let names = names(ctx).await?;
    let here = Screen::Item {
        item_id,
        back: Box::new(back.clone()),
    };
    let shopping = list.kind != ListKind::Tasks;
    let done = item.status == ItemStatus::Done;

    let mut ui = Ui::new().heading(item.title.clone());
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
        ui = ui.details("Описание", notes.to_owned());
    }

    let edit = |label: &str, step: Step| {
        button(
            label,
            Action::Edit {
                item_id,
                step,
                back: here.clone(),
            },
        )
    };
    let main = if done {
        button(
            "↩️ Вернуть",
            Action::Reopen {
                item_id,
                version: item.version,
                back: here.clone(),
            },
        )
    } else {
        button(
            if shopping {
                "✓ Куплено"
            } else {
                "✓ Готово"
            },
            Action::Complete {
                item_id,
                version: item.version,
                back: back.clone(),
            },
        )
    };
    let delete = button(
        "🗑 Удалить",
        Action::DeleteAsk {
            item_id,
            back: back.clone(),
        },
    );

    if shopping {
        ui = ui
            .row(vec![main, edit("🔢 Количество", Step::Quantity)])
            .row(vec![
                edit("📏 Единица", Step::Unit),
                edit("📝 Заметка", Step::Notes),
            ])
            .row(vec![edit("👤 Исполнитель", Step::Assignee), delete]);
    } else {
        ui = ui
            .row(vec![main, edit("✏️ Изменить", Step::Title)])
            .row(vec![
                edit("📅 Срок", Step::Due),
                edit("🔴 Приоритет", Step::Priority),
            ])
            .row(vec![
                edit("👤 Исполнитель", Step::Assignee),
                edit("🏷 Теги", Step::Tags),
            ])
            .row(vec![
                edit("🧩 Подзадачи", Step::Subtasks),
                edit("🔁 Повторение", Step::Recurrence),
                edit("📝 Описание", Step::Notes),
            ])
            .row(vec![delete]);
    }
    Ok(ui.row(vec![back_button("⬅️ Назад", back.clone())]))
}

fn is_overdue(item: &Item, ctx: &Ctx<'_>) -> bool {
    let today = ctx.now.with_timezone(&ctx.tz).date_naive();
    match (item.due_at, item.due_date) {
        (Some(at), _) => at < ctx.now,
        (None, Some(date)) => date < today,
        (None, None) => false,
    }
}

/// Результат завершения задачи.
pub async fn completed(ctx: &mut Ctx<'_>, item_id: Uuid, back: &Screen) -> Result<Ui, Fail> {
    let (item, _) = load(ctx, item_id).await?;
    let mut ui = Ui::new().heading(format!("✅ Выполнено: {}", clip(&item.title, 80)));
    if item.status == ItemStatus::Open
        && let Some(next) = ru::due_label(item.due_at, item.due_date, ctx.tz, ctx.now)
    {
        ui = ui.text(format!("Следующее: {next}"));
    }
    Ok(ui.row(vec![
        button(
            "↩️ Отменить",
            Action::Undo {
                item_id,
                back: back.clone(),
            },
        ),
        back_button("📋 К списку", back.clone()),
    ]))
}

/// Подтверждение удаления.
pub async fn confirm_delete(ctx: &mut Ctx<'_>, item_id: Uuid, back: &Screen) -> Result<Ui, Fail> {
    let (item, _) = load(ctx, item_id).await?;
    let mut ui = Ui::new().heading(format!("Удалить «{}»?", clip(&item.title, 80)));
    if item.recurrence.is_some() {
        ui = ui.text("Будет удалена вся серия повторений.");
    }
    Ok(ui.row(vec![
        button(
            "🗑 Да, удалить",
            Action::DeleteDo {
                item_id,
                back: back.clone(),
            },
        ),
        back_button(
            "Отмена",
            Screen::Item {
                item_id,
                back: Box::new(back.clone()),
            },
        ),
    ]))
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
        ctx.present(Screen::Completed {
            item_id,
            back: Box::new(back),
        })
        .await
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
    back: Screen,
) -> Result<(), Fail> {
    let household_id = ctx.household()?.id;
    let actor = ctx.actor()?;
    items::reopen(
        &mut *ctx.conn,
        household_id,
        &actor,
        item_id,
        Some(version),
        ctx.now,
    )
    .await?;
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
    ctx.toast(format!("Удалено: {}", clip(&item.title, 60)));
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
