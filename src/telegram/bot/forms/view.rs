//! Экраны форм.

use crate::domain::models::{Item, ItemPriority, ListKind, MonthRule, RecurrenceFreq};
use crate::domain::validation::{MAX_SUBTASKS, MAX_TAGS};
use crate::storage::repositories::{lists, tags, users};
use crate::telegram::rich::{Button, button};
use crate::telegram::ru;
use crate::telegram::views::{self, Lookup};

use super::actions::load_item_draft;
use super::{MAX_QUANTITY, lists_of_kind};
use crate::telegram::bot::card::{describe_recurrence_draft, priority_label};
use crate::telegram::bot::model::{
    Action, Choice, EditForm, Form, FormAct, ItemDraft, ItemForm, Screen, Step, Ui,
};
use crate::telegram::bot::onboarding;
use crate::telegram::bot::screens::{back_button, clip};
use crate::telegram::bot::{Ctx, Fail};

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
        Lookup::Active(conversation) => serde_json::from_value::<Form>(conversation.data).ok(),
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
            .row(vec![back_button("В меню", to)]));
    };

    match form {
        Form::Onboarding { name } => Ok(onboarding::view(name.as_deref())),
        Form::Create(form) => create_view(ctx, form).await,
        Form::Edit(form) => edit_view(ctx, form).await,
        Form::Search => Ok(Ui::new()
            .heading("Что ищем?")
            .text("Отправьте слово или фразу из названия, описания, тега или подзадачи.")
            .row(vec![cancel_button()])),
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
    }
}

async fn create_view(ctx: &mut Ctx<'_>, form: ItemForm) -> Result<Ui, Fail> {
    let shopping = form.kind != ListKind::Tasks;
    let mut ui = Ui::new();
    if let Some(error) = &form.error {
        ui = ui.bold(format!("⚠️ {error}"));
    }
    let optional_footer = |ui: Ui| {
        ui.row(vec![
            button("⏭ Дальше", Action::Form { act: FormAct::Skip }),
            go_step("👁 Предпросмотр", Step::Preview),
        ])
        .row(vec![cancel_button()])
    };

    match form.step {
        Step::List => {
            ui = ui.heading("В какой список?");
            for list in lists_of_kind(ctx, form.kind).await? {
                ui = ui.row(vec![choose(clip(&list.name, 40), Choice::List(list.id))]);
            }
            Ok(ui.row(vec![cancel_button()]))
        }
        Step::Preview => preview(ctx, form, ui).await,
        step if step.expects_text() && step != Step::Subtasks => {
            let ui = ui
                .heading(prompt_title(step, shopping))
                .text(prompt_hint(step, &form.draft));
            let ui = match step {
                Step::Quantity => ui.row(vec![
                    choose("1", Choice::Quantity(1.0)),
                    choose("2", Choice::Quantity(2.0)),
                    choose("3", Choice::Quantity(3.0)),
                    choose("5", Choice::Quantity(5.0)),
                ]),
                Step::Unit => units_row(ui),
                Step::Interval => ui.row(vec![
                    choose("Каждый раз", Choice::Interval(1)),
                    choose("Через раз", Choice::Interval(2)),
                ]),
                _ => ui,
            };
            Ok(if step == Step::Title {
                ui.row(vec![cancel_button()])
            } else if matches!(step, Step::Quantity | Step::Unit | Step::Notes) && shopping {
                ui.row(vec![
                    go_step("👁 Предпросмотр", Step::Preview),
                    cancel_button(),
                ])
            } else {
                optional_footer(ui)
            })
        }
        step => {
            let ui = ui.heading(prompt_title(step, shopping));
            let ui = choice_buttons(ctx, ui, step, &form.draft, None).await?;
            Ok(
                if matches!(step, Step::Tags | Step::Subtasks | Step::Weekdays) {
                    ui.row(vec![
                        button("✅ Готово", Action::Form { act: FormAct::Done }),
                        cancel_button(),
                    ])
                } else {
                    optional_footer(ui)
                },
            )
        }
    }
}

async fn edit_view(ctx: &mut Ctx<'_>, form: EditForm) -> Result<Ui, Fail> {
    let (item, draft) = load_item_draft(ctx, form.item_id).await?;
    let shopping = {
        let household_id = ctx.household()?.id;
        lists::get(&mut *ctx.conn, household_id, item.list_id)
            .await?
            .kind
            != ListKind::Tasks
    };
    let mut ui = Ui::new();
    if let Some(error) = &form.error {
        ui = ui.bold(format!("⚠️ {error}"));
    }
    let step = form.step;
    ui = ui
        .heading(prompt_title(step, shopping))
        .text(clip(&item.title, 80));
    if step.expects_text() && step != Step::Subtasks {
        ui = ui.text(prompt_hint(step, &draft));
        ui = match step {
            Step::Quantity => ui.row(vec![
                choose("1", Choice::Quantity(1.0)),
                choose("2", Choice::Quantity(2.0)),
                choose("3", Choice::Quantity(3.0)),
                choose("5", Choice::Quantity(5.0)),
            ]),
            Step::Unit => units_row(ui),
            Step::Interval => ui.row(vec![
                choose("Каждый раз", Choice::Interval(1)),
                choose("Через раз", Choice::Interval(2)),
            ]),
            Step::Notes => ui.row(vec![button(
                "🧹 Убрать",
                Action::Form { act: FormAct::Skip },
            )]),
            _ => ui,
        };
    } else {
        ui = choice_buttons(ctx, ui, step, &draft, Some(&item)).await?;
    }
    let done_label = if matches!(
        step,
        Step::Tags | Step::Subtasks | Step::Weekdays | Step::NewTag | Step::DueText
    ) {
        "✅ Готово"
    } else {
        "⬅️ Назад"
    };
    Ok(ui.row(vec![button(
        done_label,
        Action::Form { act: FormAct::Done },
    )]))
}

fn units_row(ui: Ui) -> Ui {
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

fn prompt_title(step: Step, shopping: bool) -> &'static str {
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
        Step::Preview => "Проверьте и сохраните",
    }
}

fn prompt_hint(step: Step, draft: &ItemDraft) -> String {
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

/// Кнопки выбора для шагов без текстового ввода. `item` задан при
/// редактировании существующего элемента.
async fn choice_buttons(
    ctx: &mut Ctx<'_>,
    mut ui: Ui,
    step: Step,
    draft: &ItemDraft,
    item: Option<&Item>,
) -> Result<Ui, Fail> {
    let household_id = ctx.household()?.id;
    match step {
        Step::Due => {
            let current =
                ru::due_label(draft.due(ctx.tz)?.0, draft.due(ctx.tz)?.1, ctx.tz, ctx.now);
            if let Some(current) = current {
                ui = ui.text(format!("Сейчас: {current}"));
            }
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
            if let Some(rule) = &draft.recurrence {
                ui = ui.text(format!("Сейчас: {}", describe_recurrence_draft(rule)));
            }
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
            if item.is_some() {
                ui = ui.row(vec![go_step("🔢 Интервал", Step::Interval)]);
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
        | Step::Notes
        | Step::Preview => {}
    }
    Ok(ui)
}

async fn preview(ctx: &mut Ctx<'_>, form: ItemForm, mut ui: Ui) -> Result<Ui, Fail> {
    let household_id = ctx.household()?.id;
    let shopping = form.kind != ListKind::Tasks;
    let draft = &form.draft;

    if let Some(duplicate) = &form.duplicate {
        return Ok(duplicate_view(ui, duplicate, &form));
    }

    let list_name = match form.list_id {
        Some(list_id) => {
            lists::get(&mut *ctx.conn, household_id, list_id)
                .await?
                .name
        }
        None => "—".to_owned(),
    };
    let title = if draft.title.trim().is_empty() {
        "(без названия)".to_owned()
    } else {
        draft.title.clone()
    };
    ui = ui
        .bold("Проверьте и сохраните")
        .heading(title)
        .text(format!("Список: {list_name}"));

    if shopping {
        ui = ui.text(format!(
            "Количество: {}",
            ru::quantity_label(
                draft.quantity.unwrap_or(1.0),
                draft.unit.as_deref().or(Some("шт"))
            )
        ));
    }
    if let Some(due) = ru::due_label(draft.due(ctx.tz)?.0, draft.due(ctx.tz)?.1, ctx.tz, ctx.now) {
        ui = ui.text(format!("📅 {}", ru::capitalize(&due)));
    }
    if !shopping {
        ui = ui.text(format!("Приоритет: {}", priority_label(draft.priority)));
    }
    if let Some(assignee) = draft.assignee {
        let name = users::get(&mut *ctx.conn, household_id, assignee)
            .await
            .map(|user| user.display_name)
            .unwrap_or_else(|_| "—".to_owned());
        ui = ui.text(format!("👤 {name}"));
    }
    if !draft.tags.is_empty() {
        ui = ui.text(format!("🏷 {}", draft.tags.join(", ")));
    }
    if !draft.subtasks.is_empty() {
        ui = ui.text(format!("🧩 Подзадач: {}", draft.subtasks.len()));
    }
    if let Some(rule) = &draft.recurrence {
        ui = ui.text(format!("🔁 {}", describe_recurrence_draft(rule)));
    }
    if let Some(notes) = &draft.notes {
        ui = ui.details("Описание", notes.clone());
    }

    ui = ui.row(vec![
        button("💾 Сохранить", Action::Form { act: FormAct::Save }),
        go_step("✏️ Название", Step::Title),
    ]);
    ui = if shopping {
        ui.row(vec![
            go_step("🔢 Количество", Step::Quantity),
            go_step("📏 Единица", Step::Unit),
        ])
        .row(vec![
            go_step("📝 Заметка", Step::Notes),
            go_step("👤 Исполнитель", Step::Assignee),
        ])
    } else {
        ui.row(vec![
            go_step("📅 Срок", Step::Due),
            go_step("🔴 Приоритет", Step::Priority),
        ])
        .row(vec![
            go_step("👤 Исполнитель", Step::Assignee),
            go_step("🏷 Теги", Step::Tags),
        ])
        .row(vec![
            go_step("🧩 Подзадачи", Step::Subtasks),
            go_step("🔁 Повторение", Step::Recurrence),
        ])
    };
    Ok(ui.row(vec![cancel_button()]))
}

fn duplicate_view(
    ui: Ui,
    duplicate: &crate::telegram::bot::model::Duplicate,
    form: &ItemForm,
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

    let same_unit = form
        .draft
        .unit
        .as_deref()
        .or(Some("шт"))
        .map(str::to_lowercase)
        == duplicate.unit.as_deref().map(str::to_lowercase);
    let mut row = Vec::new();
    if let (Some(quantity), true) = (duplicate.quantity, same_unit) {
        let total = quantity + form.draft.quantity.unwrap_or(1.0);
        if total <= MAX_QUANTITY {
            row.push(button(
                format!(
                    "➕ Увеличить до {}",
                    ru::quantity_label(total, duplicate.unit.as_deref())
                ),
                Action::Form {
                    act: FormAct::Increase {
                        item_id: duplicate.item_id,
                        quantity: total,
                        version: duplicate.version,
                    },
                },
            ));
        }
    }
    row.push(button(
        "📄 Добавить отдельно",
        Action::Form {
            act: FormAct::SaveAnyway,
        },
    ));
    ui = ui.row(row);
    ui.row(vec![cancel_button()])
}
