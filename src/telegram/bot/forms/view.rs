//! Экраны форм.

use crate::domain::models::{ItemPriority, MonthRule, RecurrenceFreq};
use crate::domain::validation::{MAX_SUBTASKS, MAX_TAGS};
use crate::storage::repositories::{tags, users};
use crate::telegram::rich::{Button, button};
use crate::telegram::ru;
use crate::telegram::views::{self, Lookup};

use crate::telegram::bot::card::priority_label;
use crate::telegram::bot::model::{
    Action, Choice, Form, FormAct, ItemDraft, Screen, Step, TimeField, Ui,
};
use crate::telegram::bot::onboarding;
use crate::telegram::bot::screens::{clip, nav_row};
use crate::telegram::bot::{Ctx, Fail};

use super::editor;

// ---------------------------------------------------------------------
// Экраны
// ---------------------------------------------------------------------

fn cancel_button() -> Button<Action> {
    button(
        "✖ Отмена",
        Action::Form {
            act: FormAct::Cancel,
        },
    )
}

fn choose(label: impl Into<String>, choice: Choice) -> Button<Action> {
    button(
        label,
        Action::Form {
            act: FormAct::Choose(choice),
        },
    )
}

fn go_step(label: &str, step: Step) -> Button<Action> {
    button(
        label,
        Action::Form {
            act: FormAct::Go(step),
        },
    )
}

/// Текущий шаг формы.
pub async fn view(ctx: &mut Ctx<'_>) -> Result<Ui, Fail> {
    let form = match views::conversation(&mut *ctx.conn, ctx.chat_id, ctx.now).await? {
        Lookup::Active(conversation) => super::parse(conversation.data),
        _ => None,
    };
    let Some(form) = form else {
        let to = if ctx.user.is_some() {
            Screen::Menu
        } else {
            Screen::Form
        };
        return Ok(Ui::new()
            .heading("Форма устарела")
            .text("Начнём заново?")
            .row(nav_row(&Screen::Form, to)));
    };

    match form {
        Form::Onboarding { name } => Ok(onboarding::view(name.as_deref())),
        Form::Editor(editor_form) => editor::view(ctx, editor_form).await,
        Form::LegacyEdit => Ok(Ui::new()
            .heading("Правка устарела")
            .text("Начнём заново?")
            .row(nav_row(&Screen::Form, Screen::Menu))),
        Form::DueText { .. } => Ok(Ui::new()
            .heading("Введите срок")
            .text(ru::DUE_HINT)
            .row(vec![cancel_button()])),
        Form::Search => Ok(Ui::new().heading("Что ищем?").row(vec![cancel_button()])),
        Form::Rename => Ok(Ui::new()
            .heading("Как вас называть?")
            .text("Отправьте новое имя (до 64 символов).")
            .row(vec![cancel_button()])),
        Form::DeleteHousehold => {
            let name = ctx.household()?.name.clone();
            Ok(Ui::new()
                .heading("Удалить пространство?")
                .text(format!(
                    "Будут удалены все списки, задачи и покупки, доступ участников и токены. \
                     Для подтверждения отправьте название точно так: {name}"
                ))
                .row(vec![cancel_button()]))
        }
        Form::Lead => Ok(Ui::new()
            .heading("За сколько минут напоминать?")
            .text("Отправьте число от 5 до 1440.")
            .row(vec![cancel_button()])),
        Form::Time { field } => Ok(Ui::new()
            .heading(time_title(field))
            .text("Отправьте время в формате HH:MM, например 08:30.")
            .row(vec![cancel_button()])),
    }
}

fn time_title(field: TimeField) -> &'static str {
    match field {
        TimeField::Digest => "Во сколько присылать сводку?",
        TimeField::AllDay => "Во сколько напоминать о задачах без времени?",
        TimeField::QuietStart => "Когда начинаются тихие часы?",
        TimeField::QuietEnd => "Когда заканчиваются тихие часы?",
    }
}

pub(super) fn units_row(ui: Ui) -> Ui {
    ui.row(vec![
        choose("шт", Choice::Unit("шт".into())),
        choose("кг", Choice::Unit("кг".into())),
        choose("л", Choice::Unit("л".into())),
    ])
    .row(vec![
        choose("г", Choice::Unit("г".into())),
        choose("уп", Choice::Unit("уп".into())),
    ])
}

pub(super) fn prompt_title(step: Step, shopping: bool) -> &'static str {
    match step {
        Step::List => "В какой список?",
        Step::Title if shopping => "Что купить?",
        Step::Title => "Как назвать задачу?",
        Step::Due => "Когда сделать?",
        Step::DueText => "Введите срок",
        Step::Priority => "Какой приоритет?",
        Step::Assignee => "Кто исполнитель?",
        Step::Tags => "Теги",
        Step::NewTag => "Новый тег",
        Step::Subtasks => "Подзадачи",
        Step::Recurrence => "Как часто повторять?",
        Step::Weekdays => "По каким дням недели?",
        Step::MonthRule => "Какого числа месяца?",
        Step::Interval => "Повторять каждые сколько?",
        Step::Quantity => "Сколько?",
        Step::Unit => "В чём измерять?",
        Step::Notes => "Описание",
    }
}

pub(super) fn prompt_hint(step: Step, draft: &ItemDraft) -> String {
    match step {
        Step::Title => "Отправьте название сообщением (до 200 символов).".to_owned(),
        Step::DueText => ru::DUE_HINT.to_owned(),
        Step::NewTag => "Отправьте название тега (до 32 символов).".to_owned(),
        Step::Subtasks => format!(
            "Отправьте название подзадачи, чтобы добавить (до {MAX_SUBTASKS}). Сейчас: {}.",
            draft.subtasks.len()
        ),
        Step::Interval => "Отправьте число от 1 до 365 или выберите вариант.".to_owned(),
        Step::Quantity => "Отправьте число, например 2 или 1,5.".to_owned(),
        Step::Unit => "Выберите единицу или отправьте свою (до 16 символов).".to_owned(),
        Step::Notes => "Отправьте текст описания. Чтобы убрать — отправьте «-».".to_owned(),
        _ => String::new(),
    }
}

/// Кнопки выбора для шагов без текстового ввода.
pub(super) async fn choice_buttons(
    ctx: &mut Ctx<'_>,
    mut ui: Ui,
    step: Step,
    draft: &ItemDraft,
) -> Result<Ui, Fail> {
    let household_id = ctx.household()?.id;
    match step {
        Step::Due => {
            ui = ui
                .row(vec![
                    choose("Сегодня", Choice::DueToday),
                    choose("Завтра", Choice::DueTomorrow),
                ])
                .row(vec![
                    go_step("📅 Дата и время", Step::DueText),
                    choose("Без срока", Choice::DueNone),
                ]);
        }
        Step::Priority => {
            ui = ui
                .row(vec![
                    choose(
                        priority_label(ItemPriority::Low),
                        Choice::Priority(ItemPriority::Low),
                    ),
                    choose(
                        priority_label(ItemPriority::Normal),
                        Choice::Priority(ItemPriority::Normal),
                    ),
                ])
                .row(vec![
                    choose(
                        priority_label(ItemPriority::High),
                        Choice::Priority(ItemPriority::High),
                    ),
                    choose(
                        priority_label(ItemPriority::Urgent),
                        Choice::Priority(ItemPriority::Urgent),
                    ),
                ]);
        }
        Step::Assignee => {
            for member in users::list_active(&mut *ctx.conn, household_id).await? {
                let mark = if draft.assignee == Some(member.id) {
                    "● "
                } else {
                    ""
                };
                ui = ui.row(vec![choose(
                    format!("{mark}{}", clip(&member.display_name, 30)),
                    Choice::Assignee(Some(member.id)),
                )]);
            }
            ui = ui.row(vec![choose("Без исполнителя", Choice::Assignee(None))]);
        }
        Step::Tags => {
            let known = tags::list(&mut *ctx.conn, household_id).await?;
            let mut names: Vec<String> = known.into_iter().map(|tag| tag.name).collect();
            for tag in &draft.tags {
                if !names.contains(tag) {
                    names.push(tag.clone());
                }
            }
            for chunk in names.chunks(2) {
                ui = ui.row(
                    chunk
                        .iter()
                        .map(|name| {
                            let mark = if draft.tags.contains(name) {
                                "✓ "
                            } else {
                                ""
                            };
                            choose(
                                format!("{mark}{}", clip(name, 24)),
                                Choice::TagToggle(name.clone()),
                            )
                        })
                        .collect(),
                );
            }
            ui = ui
                .text(format!("Выбрано: {} из {MAX_TAGS}", draft.tags.len()))
                .row(vec![go_step("➕ Новый тег", Step::NewTag)]);
        }
        Step::Subtasks => {
            for (index, subtask) in draft.subtasks.iter().enumerate() {
                let mark = if subtask.done { "☑" } else { "☐" };
                ui = ui.row(vec![
                    choose(
                        format!("{mark} {}", clip(&subtask.title, 34)),
                        Choice::SubtaskToggle(index),
                    ),
                    choose("✖", Choice::SubtaskRemove(index)),
                ]);
            }
            ui = ui.text(prompt_hint(Step::Subtasks, draft));
        }
        Step::Recurrence => {
            ui = ui
                .row(vec![choose("Не повторять", Choice::Freq(None))])
                .row(vec![
                    choose("Каждый день", Choice::Freq(Some(RecurrenceFreq::Daily))),
                    choose("Каждую неделю", Choice::Freq(Some(RecurrenceFreq::Weekly))),
                ])
                .row(vec![
                    choose("Каждый месяц", Choice::Freq(Some(RecurrenceFreq::Monthly))),
                    choose("Каждый год", Choice::Freq(Some(RecurrenceFreq::Yearly))),
                ]);
        }
        Step::Weekdays => {
            let selected = draft
                .recurrence
                .as_ref()
                .map(|rule| rule.weekdays.clone())
                .unwrap_or_default();
            let buttons: Vec<Button<Action>> = (1..=7_i16)
                .map(|day| {
                    let mark = if selected.contains(&day) { "✓ " } else { "" };
                    choose(
                        format!(
                            "{mark}{}",
                            ru::weekday_short(u32::try_from(day).unwrap_or(1))
                        ),
                        Choice::Weekday(day),
                    )
                })
                .collect();
            for chunk in buttons.chunks(4) {
                ui = ui.row(chunk.to_vec());
            }
        }
        Step::MonthRule => {
            ui = ui.row(vec![
                choose(
                    "В этот день месяца",
                    Choice::MonthRule(MonthRule::AnchorDay),
                ),
                choose("В последний день", Choice::MonthRule(MonthRule::LastDay)),
            ]);
        }
        Step::List
        | Step::Title
        | Step::DueText
        | Step::NewTag
        | Step::Interval
        | Step::Quantity
        | Step::Unit
        | Step::Notes => {}
    }
    Ok(ui)
}

pub(super) fn duplicate_view(
    ui: Ui,
    duplicate: &crate::telegram::bot::model::Duplicate,
    draft: &ItemDraft,
) -> Ui {
    let existing = match duplicate.quantity {
        Some(quantity) => format!(
            " ({})",
            ru::quantity_label(quantity, duplicate.unit.as_deref())
        ),
        None => String::new(),
    };
    let mut ui = ui.heading(format!(
        "«{}» уже есть в списке{existing}.",
        clip(&duplicate.title, 60)
    ));

    let mut row = Vec::new();
    if let Some(total) =
        duplicate.increase_total(draft.quantity.unwrap_or(1.0), draft.unit.as_deref())
    {
        row.push(button(
            duplicate.increase_label(total),
            Action::Form {
                act: FormAct::Increase {
                    item_id: duplicate.item_id,
                    quantity: total,
                    version: duplicate.version,
                },
            },
        ));
    }
    row.push(button(
        duplicate.separate_label(),
        Action::Form {
            act: FormAct::SaveAnyway,
        },
    ));
    ui = ui.row(row);
    ui.row(vec![cancel_button()])
}
