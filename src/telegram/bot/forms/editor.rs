//! Единый редактор: экран полей, экран одного поля и сохранение слиянием по
//! `touched`.

use std::collections::BTreeSet;

use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::models::{
    Field, Item, ItemPatch, ListKind, MonthRule, Recurrence, RecurrenceFreq,
};
use crate::domain::validation::validate_new_item;
use crate::storage::repositories::{items, lists, users};
use crate::telegram::rich::Button;
use crate::telegram::ru;

use super::state::{apply_choice, apply_text, today};
use super::view::{choice_buttons, prompt_hint, prompt_title, units_row};
use crate::telegram::bot::card::{describe_recurrence_draft, priority_label};
use crate::telegram::bot::model::{
    Action, Choice, Editor, EditorField, EditorTarget, Form, FormAct, ItemDraft, ItemSnapshot,
    RecurrenceDraft, Screen, Step, Ui, screen_heading, screen_heading_with,
};
use crate::telegram::bot::screens::clip;
use crate::telegram::bot::{Ctx, Fail, into_sqlx};

impl Editor {
    /// Элемент, который правит редактор; у создания его ещё нет.
    fn item_id(&self) -> Option<Uuid> {
        match &self.target {
            EditorTarget::New { .. } => None,
            EditorTarget::Item { item_id, .. } => Some(*item_id),
        }
    }

    /// Редактор создаёт новый элемент.
    fn is_new(&self) -> bool {
        matches!(self.target, EditorTarget::New { .. })
    }

    /// Куда вернуться после сохранения или отмены.
    fn back(&self) -> &Screen {
        match &self.target {
            EditorTarget::New { back, .. } | EditorTarget::Item { back, .. } => back,
        }
    }

    /// Снимок значений при открытии.
    fn base(&self) -> Option<&ItemSnapshot> {
        match &self.target {
            EditorTarget::New { .. } => None,
            EditorTarget::Item { base, .. } => Some(base),
        }
    }

    /// Экран редактора для `telegram_views`.
    fn screen(&self) -> Screen {
        Screen::Editor {
            item_id: self.item_id(),
            back: Box::new(self.back().clone()),
        }
    }
}

/// Сохранить редактор и показать его экран.
pub(super) async fn show(ctx: &mut Ctx<'_>, editor: Editor) -> Result<(), Fail> {
    let screen = editor.screen();
    super::put(ctx, &Form::Editor(editor)).await?;
    ctx.present(screen).await
}

/// Экран редактора из `telegram_views`: живое состояние или «устарела».
pub async fn screen(ctx: &mut Ctx<'_>, item_id: Option<Uuid>, back: &Screen) -> Result<Ui, Fail> {
    match super::load(ctx).await? {
        Some(Form::Editor(editor)) if editor.item_id() == item_id => view(ctx, editor).await,
        _ => Ok(expired_view(item_id, back)),
    }
}

fn expired_view(item_id: Option<Uuid>, back: &Screen) -> Ui {
    let screen = Screen::Editor {
        item_id,
        back: Box::new(back.clone()),
    };
    let mut row = vec![crate::telegram::rich::button(
        "Открыть карточку",
        Action::Go { to: back.clone() },
    )];
    if screen.depth() > 2 {
        row.push(crate::telegram::rich::button(
            ru::MENU_BUTTON,
            Action::Go { to: Screen::Menu },
        ));
    }
    screen_heading(&screen, "Правка устарела", None)
        .text("Начнём заново?")
        .row(row)
}

/// Экран редактора или открытого поля.
pub async fn view(ctx: &mut Ctx<'_>, editor: Editor) -> Result<Ui, Fail> {
    if editor.conflict.is_some() {
        return conflict_view(ctx, &editor).await;
    }
    if editor.duplicate.is_some() {
        return duplicate_screen(ctx, &editor).await;
    }
    match editor.field {
        Some(step) => field_view(ctx, &editor, step).await,
        None => main_view(ctx, &editor).await,
    }
}

async fn duplicate_screen(_ctx: &mut Ctx<'_>, editor: &Editor) -> Result<Ui, Fail> {
    let Some(duplicate) = &editor.duplicate else {
        return Ok(Ui::new());
    };
    let mut ui = Ui::new();
    if let Some(error) = &editor.error {
        ui = ui.bold(format!("⚠️ {error}"));
    }
    Ok(super::view::duplicate_view(ui, duplicate, &editor.draft))
}

fn button(label: impl Into<String>, act: FormAct) -> Button<Action> {
    crate::telegram::rich::button(label, Action::Form { act })
}

fn field_button(label: &str, step: Step) -> Button<Action> {
    button(label, FormAct::Go(step))
}

fn choice(label: impl Into<String>, choice: Choice) -> Button<Action> {
    button(label, FormAct::Choose(choice))
}

async fn list_of_editor(
    ctx: &mut Ctx<'_>,
    editor: &Editor,
) -> Result<crate::domain::models::List, Fail> {
    let household_id = ctx.household()?.id;
    Ok(lists::get(&mut *ctx.conn, household_id, editor.list_id).await?)
}

async fn main_view(ctx: &mut Ctx<'_>, editor: &Editor) -> Result<Ui, Fail> {
    let list = list_of_editor(ctx, editor).await?;
    let shopping = list.kind != ListKind::Tasks;
    let screen = editor.screen();
    let title = if editor.draft.title.trim().is_empty() {
        "(без названия)".to_owned()
    } else {
        clip(&editor.draft.title, 60)
    };
    let mut ui = screen_heading_with(&screen, &title, &[list.name.as_str(), "Карточка"]);
    if let Some(error) = &editor.error {
        ui = ui.bold(format!("⚠️ {error}"));
    }
    for line in values(ctx, editor, shopping).await? {
        ui = ui.text(line);
    }

    if shopping {
        ui = ui
            .row(vec![
                field_button("🔢 Количество", Step::Quantity),
                field_button("📏 Единица", Step::Unit),
            ])
            .row(vec![
                field_button("📝 Заметка", Step::Notes),
                field_button("👤 Исполнитель", Step::Assignee),
            ])
            .row(vec![
                field_button("📋 Список", Step::List),
                field_button("✏️ Название", Step::Title),
            ]);
    } else {
        ui = ui
            .row(vec![
                field_button("📅 Срок", Step::Due),
                field_button("🔴 Приоритет", Step::Priority),
            ])
            .row(vec![
                field_button("👤 Исполнитель", Step::Assignee),
                field_button("🏷 Теги", Step::Tags),
            ])
            .row(vec![
                field_button("🧩 Подзадачи", Step::Subtasks),
                field_button("🔁 Повторение", Step::Recurrence),
            ])
            .row(vec![
                field_button("📝 Описание", Step::Notes),
                field_button("📋 Список", Step::List),
                field_button("✏️ Название", Step::Title),
            ]);
    }

    ui = ui.row(vec![button("💾 Сохранить", FormAct::Save)]);
    let mut cancel = vec![button("✖ Отмена", FormAct::Cancel)];
    if screen.depth() > 2 {
        cancel.push(crate::telegram::rich::button(
            ru::MENU_BUTTON,
            Action::Go { to: Screen::Menu },
        ));
    }
    Ok(ui.row(cancel))
}

async fn field_view(ctx: &mut Ctx<'_>, editor: &Editor, step: Step) -> Result<Ui, Fail> {
    let list = list_of_editor(ctx, editor).await?;
    let shopping = list.kind != ListKind::Tasks;
    let screen = editor.screen();
    let mut ui = Ui::new();
    if let Some(error) = &editor.error {
        ui = ui.bold(format!("⚠️ {error}"));
    }
    for line in values(ctx, editor, shopping).await? {
        ui = ui.text(line);
    }
    ui = ui.heading(screen.trail_with(
        prompt_title(step, shopping),
        &[list.name.as_str(), "Карточка", "Редактор"],
    ));
    if step.expects_text() && step != Step::Subtasks {
        ui = ui.text(prompt_hint(step, &editor.draft));
    }

    match step {
        Step::List => {
            if editor.is_new() {
                // Создание из меню: «📋 Список» переключает и тип, как в
                // быстрой форме, поэтому показаны все списки.
                let household_id = ctx.household()?.id;
                let mut lists = lists::list(&mut *ctx.conn, household_id).await?;
                lists.retain(|list| !list.is_archived);
                lists.sort_by_key(|list| list.kind != ListKind::Tasks);
                for list in lists {
                    let icon = if list.kind == ListKind::Tasks {
                        "📋"
                    } else {
                        "🛒"
                    };
                    let mark = if list.id == editor.list_id {
                        "● "
                    } else {
                        ""
                    };
                    ui = ui.row(vec![choice(
                        format!("{mark}{icon} {}", clip(&list.name, 40)),
                        Choice::List(list.id),
                    )]);
                }
            } else {
                for list in super::lists_of_kind(ctx, list.kind).await? {
                    let mark = if list.id == editor.list_id {
                        "● "
                    } else {
                        ""
                    };
                    ui = ui.row(vec![choice(
                        format!("{mark}{}", clip(&list.name, 40)),
                        Choice::List(list.id),
                    )]);
                }
            }
        }
        Step::Due | Step::Priority | Step::Assignee | Step::Tags | Step::Subtasks => {
            ui = choice_buttons(ctx, ui, step, &editor.draft).await?;
        }
        Step::Recurrence => {
            ui = recurrence_buttons(ui, editor);
        }
        Step::Quantity => {
            ui = ui.row(vec![
                choice("1", Choice::Quantity(1.0)),
                choice("2", Choice::Quantity(2.0)),
                choice("3", Choice::Quantity(3.0)),
                choice("5", Choice::Quantity(5.0)),
            ]);
        }
        Step::Unit => {
            ui = units_row(ui);
        }
        Step::Interval => {
            ui = ui.row(vec![
                choice("Каждый раз", Choice::Interval(1)),
                choice("Через раз", Choice::Interval(2)),
            ]);
        }
        Step::Title
        | Step::DueText
        | Step::NewTag
        | Step::Weekdays
        | Step::MonthRule
        | Step::Notes
        | Step::Preview => {}
    }

    if matches!(
        step,
        Step::Tags | Step::Subtasks | Step::Weekdays | Step::Recurrence
    ) {
        ui = ui.row(vec![button(ru::APPLY_BUTTON, FormAct::Done)]);
    }
    let mut footer = vec![button(ru::BACK_BUTTON, FormAct::Back)];
    if step == Step::Title {
        // Начало создания: с поля «Название» можно отказаться целиком.
        footer.push(button("✖ Отмена", FormAct::Cancel));
    }
    if screen.depth() > 2 {
        footer.push(crate::telegram::rich::button(
            ru::MENU_BUTTON,
            Action::Go { to: Screen::Menu },
        ));
    }
    Ok(ui.row(footer))
}

fn recurrence_buttons(mut ui: Ui, editor: &Editor) -> Ui {
    ui = ui
        .row(vec![choice("Не повторять", Choice::Freq(None))])
        .row(vec![
            choice("Каждый день", Choice::Freq(Some(RecurrenceFreq::Daily))),
            choice("Каждую неделю", Choice::Freq(Some(RecurrenceFreq::Weekly))),
        ])
        .row(vec![
            choice("Каждый месяц", Choice::Freq(Some(RecurrenceFreq::Monthly))),
            choice("Каждый год", Choice::Freq(Some(RecurrenceFreq::Yearly))),
        ]);

    let Some(rule) = editor.draft.recurrence.as_ref() else {
        return ui;
    };
    match rule.freq {
        RecurrenceFreq::Weekly => {
            let buttons: Vec<Button<Action>> = (1..=7_i16)
                .map(|day| {
                    let mark = if rule.weekdays.contains(&day) {
                        "✓ "
                    } else {
                        ""
                    };
                    choice(
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
        RecurrenceFreq::Monthly => {
            let current = rule.month_rule;
            let mark = |value: MonthRule| {
                if current == Some(value) { "✓ " } else { "" }
            };
            ui = ui.row(vec![
                choice(
                    format!("{}В этот день месяца", mark(MonthRule::AnchorDay)),
                    Choice::MonthRule(MonthRule::AnchorDay),
                ),
                choice(
                    format!("{}В последний день", mark(MonthRule::LastDay)),
                    Choice::MonthRule(MonthRule::LastDay),
                ),
            ]);
        }
        RecurrenceFreq::Daily | RecurrenceFreq::Yearly => {}
    }

    ui = ui
        .row(vec![
            choice("Каждый раз", Choice::Interval(1)),
            choice("Через раз", Choice::Interval(2)),
        ])
        .row(vec![button("Другой интервал", FormAct::Go(Step::Interval))]);
    ui
}

/// Значения полей текстом: изменённые видны сразу, ещё до сохранения.
async fn values(ctx: &mut Ctx<'_>, editor: &Editor, shopping: bool) -> Result<Vec<String>, Fail> {
    let draft = &editor.draft;
    let title = if draft.title.trim().is_empty() {
        "(без названия)".to_owned()
    } else {
        clip(&draft.title, 80)
    };
    let mut lines = vec![format!("Название: {title}")];
    if shopping {
        lines.push(format!(
            "Количество: {}",
            ru::quantity_label(
                draft.quantity.unwrap_or(1.0),
                draft.unit.as_deref().or(Some("шт"))
            )
        ));
        lines.push(format!(
            "Единица: {}",
            clip(draft.unit.as_deref().unwrap_or("шт"), 16)
        ));
        lines.push(format!(
            "Заметка: {}",
            draft
                .notes
                .as_deref()
                .map_or_else(|| "—".to_owned(), |notes| clip(notes, 60))
        ));
    } else {
        let (due_at, due_date) = draft.due(ctx.tz)?;
        match ru::due_label(due_at, due_date, ctx.tz, ctx.now) {
            Some(due) => lines.push(format!("Срок: {}", ru::capitalize(&due))),
            None => lines.push("Срок: без срока".to_owned()),
        }
        lines.push(format!("Приоритет: {}", priority_label(draft.priority)));
        lines.push(format!(
            "Теги: {}",
            if draft.tags.is_empty() {
                "нет".to_owned()
            } else {
                clip(&draft.tags.join(", "), 80)
            }
        ));
        lines.push(format!(
            "Подзадачи: {}",
            if draft.subtasks.is_empty() {
                "нет".to_owned()
            } else {
                clip(
                    &draft
                        .subtasks
                        .iter()
                        .map(|subtask| subtask.title.as_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                    80,
                )
            }
        ));
        lines.push(format!(
            "Повторение: {}",
            draft
                .recurrence
                .as_ref()
                .map_or_else(|| "нет".to_owned(), describe_recurrence_draft)
        ));
        lines.push(format!(
            "Описание: {}",
            draft
                .notes
                .as_deref()
                .map_or_else(|| "—".to_owned(), |notes| clip(notes, 60))
        ));
    }
    lines.push(format!(
        "Исполнитель: {}",
        match draft.assignee {
            Some(id) => {
                let household_id = ctx.household()?.id;
                match users::get(&mut *ctx.conn, household_id, id).await {
                    Ok(user) => clip(&user.display_name, 40),
                    Err(DomainError::NotFound) => "—".to_owned(),
                    Err(error) => return Err(Fail::Infra(into_sqlx(error))),
                }
            }
            None => "—".to_owned(),
        }
    ));
    let list = list_of_editor(ctx, editor).await?;
    lines.push(format!("Список: {}", clip(&list.name, 40)));
    Ok(lines)
}

/// Сравнить значение поля черновика со снимком.
fn value_eq(
    field: EditorField,
    draft: &ItemDraft,
    list_id: Uuid,
    snapshot: &ItemSnapshot,
    timezone: chrono_tz::Tz,
) -> Result<bool, DomainError> {
    Ok(match field {
        EditorField::Title => draft.title == snapshot.title,
        EditorField::Due => draft.due(timezone)? == (snapshot.due_at, snapshot.due_date),
        EditorField::Priority => draft.priority == snapshot.priority,
        EditorField::Assignee => draft.assignee == snapshot.assignee,
        EditorField::Tags => sorted(&draft.tags) == sorted(&snapshot.tags),
        EditorField::Subtasks => draft.subtasks == snapshot.subtasks,
        EditorField::Recurrence => {
            draft_rule_eq(draft.recurrence.as_ref(), snapshot.recurrence.as_ref())
        }
        EditorField::Notes => draft.notes == snapshot.notes,
        EditorField::List => list_id == snapshot.list_id,
        EditorField::Quantity => draft.quantity == snapshot.quantity,
        EditorField::Unit => draft.unit == snapshot.unit,
    })
}

fn sorted(tags: &[String]) -> Vec<String> {
    let mut sorted = tags.to_vec();
    sorted.sort_unstable();
    sorted
}

/// Правило из черновика совпадает с сохранённым, если не считать слот
/// вхождения.
fn draft_rule_eq(ours: Option<&RecurrenceDraft>, saved: Option<&Recurrence>) -> bool {
    match (ours, saved) {
        (None, None) => true,
        (Some(ours), Some(saved)) => {
            ours.freq == saved.freq
                && ours.interval == saved.interval
                && ours.weekdays == saved.byweekday
                && ours.month_rule == saved.month_rule
        }
        _ => false,
    }
}

/// Поля `touched`, значения которых действительно отличаются от снимка.
fn changed_fields(
    editor: &Editor,
    base: &ItemSnapshot,
    timezone: chrono_tz::Tz,
) -> Result<BTreeSet<EditorField>, DomainError> {
    let mut changed = BTreeSet::new();
    for field in &editor.touched {
        if !value_eq(*field, &editor.draft, editor.list_id, base, timezone)? {
            changed.insert(*field);
        }
    }
    Ok(changed)
}

fn current_differs(field: EditorField, current: &ItemSnapshot, base: &ItemSnapshot) -> bool {
    match field {
        EditorField::Title => current.title != base.title,
        EditorField::Due => (current.due_at, current.due_date) != (base.due_at, base.due_date),
        EditorField::Priority => current.priority != base.priority,
        EditorField::Assignee => current.assignee != base.assignee,
        EditorField::Tags => sorted(&current.tags) != sorted(&base.tags),
        EditorField::Subtasks => current.subtasks != base.subtasks,
        EditorField::Recurrence => match (&current.recurrence, &base.recurrence) {
            (None, None) => false,
            (Some(current), Some(base)) => !current.same_rule(base),
            _ => true,
        },
        EditorField::Notes => current.notes != base.notes,
        EditorField::List => current.list_id != base.list_id,
        EditorField::Quantity => current.quantity != base.quantity,
        EditorField::Unit => current.unit != base.unit,
    }
}

/// «💾 Сохранить»: создание нового или слияние правки по `touched`.
async fn save(ctx: &mut Ctx<'_>, editor: Editor, allow_duplicate: bool) -> Result<(), Fail> {
    if editor.is_new() {
        return save_new(ctx, editor, allow_duplicate).await;
    }
    save_item(ctx, editor).await
}

/// Конфликты, затем один `items::update` по `touched`.
async fn save_item(ctx: &mut Ctx<'_>, mut editor: Editor) -> Result<(), Fail> {
    let item_id = editor
        .item_id()
        .ok_or_else(|| Fail::User("Редактор не выбрал элемент.".into()))?;
    let back = editor.back().clone();
    let household_id = ctx.household()?.id;
    let current = items::get(&mut *ctx.conn, household_id, item_id).await?;
    let current_snapshot = ItemSnapshot::from_item(&current);
    let base = editor
        .base()
        .cloned()
        .ok_or_else(|| Fail::User("Редактор не выбрал элемент.".into()))?;
    let changed = changed_fields(&editor, &base, ctx.tz)?;
    if changed.is_empty() {
        super::clear(ctx).await?;
        return ctx.present(back).await;
    }

    let mut conflicts = BTreeSet::new();
    for field in &changed {
        let current_changed = current_differs(*field, &current_snapshot, &base);
        let ours_equals_current = value_eq(
            *field,
            &editor.draft,
            editor.list_id,
            &current_snapshot,
            ctx.tz,
        )?;
        if current_changed && !ours_equals_current {
            conflicts.insert(*field);
        }
    }
    if !conflicts.is_empty() {
        editor.conflict = Some(conflicts);
        return show(ctx, editor).await;
    }
    write(ctx, editor, &current, &changed, back).await
}

/// Создать элемент из редактора.
async fn save_new(
    ctx: &mut Ctx<'_>,
    mut editor: Editor,
    allow_duplicate: bool,
) -> Result<(), Fail> {
    let household = ctx.household()?.clone();
    let actor = ctx.actor()?;
    if allow_duplicate {
        editor.duplicate = None;
    }
    let list = lists::get_visible(&mut *ctx.conn, household.id, editor.list_id).await?;
    let shopping = list.kind != ListKind::Tasks;

    if editor.draft.title.trim().is_empty() {
        editor.error = Some("Название не может быть пустым.".to_owned());
        editor.field = Some(Step::Title);
        return show(ctx, editor).await;
    }
    let (due_at, due_date) = editor.draft.due(ctx.tz)?;
    if due_at.is_some_and(|at| at < ctx.now) || due_date.is_some_and(|date| date < today(ctx)) {
        editor.error = Some("Срок уже прошёл.".to_owned());
        editor.field = Some(Step::Due);
        return show(ctx, editor).await;
    }
    if shopping
        && !allow_duplicate
        && let Some(existing) =
            super::actions::find_duplicate(ctx, &list, &editor.draft.title).await?
    {
        editor.duplicate = Some(existing);
        return show(ctx, editor).await;
    }

    let new_item = editor.draft.to_new_item(list.id, ctx.tz, list.kind)?;
    let validated = match validate_new_item(list.kind, &household.timezone, new_item, today(ctx)) {
        Ok(validated) => validated,
        Err(_) => {
            editor.error = Some("Не получилось сохранить: проверьте введённые данные.".to_owned());
            editor.field = None;
            return show(ctx, editor).await;
        }
    };
    let assignee = editor.draft.assignee;
    items::create(
        &mut *ctx.conn,
        household.id,
        &actor,
        list.id,
        validated,
        assignee,
        ctx.now,
    )
    .await?;

    let back = editor.back().clone();
    super::clear(ctx).await?;
    ctx.banner("Добавлено");
    ctx.present(created_screen(&back, list.id)).await
}

/// Куда вернуть после создания: в сохранённый список с прежним режимом.
fn created_screen(back: &Screen, list_id: Uuid) -> Screen {
    match back {
        Screen::List {
            edit, from_lists, ..
        } => Screen::List {
            list_id,
            page: 0,
            edit: *edit,
            from_lists: *from_lists,
        },
        _ => Screen::List {
            list_id,
            page: 0,
            edit: false,
            from_lists: false,
        },
    }
}

/// Записать изменения и вернуться на `back`.
async fn write(
    ctx: &mut Ctx<'_>,
    editor: Editor,
    current: &Item,
    changed: &BTreeSet<EditorField>,
    back: Screen,
) -> Result<(), Fail> {
    let household = ctx.household()?.clone();
    let actor = ctx.actor()?;
    let mut patch = ItemPatch::default();
    for field in changed {
        match field {
            EditorField::List => patch.list_id = Some(editor.list_id),
            EditorField::Due => {
                apply_due_patch(&mut patch, &editor.draft, current, ctx.tz)?;
            }
            _ => merge(&mut patch, editor.draft.patch_for(field.step(), ctx.tz)?),
        }
    }

    let saved = items::update(
        &mut *ctx.conn,
        household.id,
        &actor,
        current.id,
        current.version,
        patch,
        &household.timezone,
        ctx.now,
    )
    .await;
    match saved {
        Ok(_) => {
            super::clear(ctx).await?;
            ctx.banner("Сохранено");
            ctx.present(back).await
        }
        Err(DomainError::Validation(_)) => {
            let mut editor = editor;
            editor.error = Some("Проверьте введённые значения.".to_owned());
            show(ctx, editor).await
        }
        Err(DomainError::Conflict { .. }) => {
            super::clear(ctx).await?;
            ctx.banner(super::super::CHANGED_STATE_MESSAGE);
            ctx.present(back).await
        }
        Err(error) => Err(error.into()),
    }
}

/// Срок с учётом текущего значения: «Без срока» очищает только то, что
/// действительно задано, и не передаёт `null` в оба поля сразу.
fn apply_due_patch(
    patch: &mut ItemPatch,
    draft: &ItemDraft,
    current: &Item,
    timezone: chrono_tz::Tz,
) -> Result<(), DomainError> {
    let (due_at, due_date) = draft.due(timezone)?;
    match (due_at, due_date) {
        (Some(at), _) => patch.due_at = Field::Value(at),
        (None, Some(date)) => patch.due_date = Field::Value(date),
        (None, None) => {
            if current.due_at.is_some() {
                patch.due_at = Field::Null;
            }
            if current.due_date.is_some() {
                patch.due_date = Field::Null;
            }
        }
    }
    Ok(())
}

fn merge(target: &mut ItemPatch, source: ItemPatch) {
    // Деструктуризация без `..`: новое поле `ItemPatch` не потеряется молча,
    // а сломает сборку.
    let ItemPatch {
        title,
        notes,
        priority,
        due_at,
        due_date,
        assignee_user_id,
        quantity,
        unit,
        tags,
        subtasks,
        recurrence,
        list_id,
    } = source;
    if title.is_some() {
        target.title = title;
    }
    if !notes.is_missing() {
        target.notes = notes;
    }
    if priority.is_some() {
        target.priority = priority;
    }
    if !due_at.is_missing() {
        target.due_at = due_at;
    }
    if !due_date.is_missing() {
        target.due_date = due_date;
    }
    if !assignee_user_id.is_missing() {
        target.assignee_user_id = assignee_user_id;
    }
    if !quantity.is_missing() {
        target.quantity = quantity;
    }
    if !unit.is_missing() {
        target.unit = unit;
    }
    if tags.is_some() {
        target.tags = tags;
    }
    if subtasks.is_some() {
        target.subtasks = subtasks;
    }
    if !recurrence.is_missing() {
        target.recurrence = recurrence;
    }
    if list_id.is_some() {
        target.list_id = list_id;
    }
}

/// Ответ на кнопку от «Оставить мой» / «Оставить текущий».
async fn resolve_conflict(ctx: &mut Ctx<'_>, mut editor: Editor, mine: bool) -> Result<(), Fail> {
    let Some(conflicts) = editor.conflict.take() else {
        return show(ctx, editor).await;
    };
    let item_id = editor
        .item_id()
        .ok_or_else(|| Fail::User("Редактор не выбрал элемент.".into()))?;
    let back = editor.back().clone();
    if !mine {
        for field in &conflicts {
            editor.touched.remove(field);
        }
    }
    let household_id = ctx.household()?.id;
    let current = items::get(&mut *ctx.conn, household_id, item_id).await?;
    let base = editor
        .base()
        .cloned()
        .ok_or_else(|| Fail::User("Редактор не выбрал элемент.".into()))?;
    let changed = changed_fields(&editor, &base, ctx.tz)?;
    if changed.is_empty() {
        super::clear(ctx).await?;
        return ctx.present(back).await;
    }
    write(ctx, editor, &current, &changed, back).await
}

async fn cancel(ctx: &mut Ctx<'_>, editor: Editor) -> Result<(), Fail> {
    let back = editor.back().clone();
    super::clear(ctx).await?;
    ctx.toast("Отменено");
    ctx.present(back).await
}

async fn conflict_view(ctx: &mut Ctx<'_>, editor: &Editor) -> Result<Ui, Fail> {
    let item_id = editor
        .item_id()
        .ok_or_else(|| Fail::User("Редактор не выбрал элемент.".into()))?;
    let back = editor.back().clone();
    let household_id = ctx.household()?.id;
    let current = items::get(&mut *ctx.conn, household_id, item_id).await?;
    let current_draft = ItemDraft::from_item(&current, ctx.tz);
    let screen = Screen::Editor {
        item_id: Some(item_id),
        back: Box::new(back),
    };
    let mut ui = screen_heading(&screen, "Конфликт при сохранении", None);
    if let Some(conflicts) = &editor.conflict {
        for field in conflicts {
            let ours = field_value(ctx, *field, &editor.draft, editor.list_id).await?;
            let theirs = field_value(ctx, *field, &current_draft, current.list_id).await?;
            ui = ui
                .bold(format!("{}:", field_title(*field)))
                .text(format!("Мой: {ours}"))
                .text(format!("Сейчас: {theirs}"));
        }
    }
    let mut footer = vec![button(ru::BACK_BUTTON, FormAct::Back)];
    if screen.depth() > 2 {
        footer.push(crate::telegram::rich::button(
            ru::MENU_BUTTON,
            Action::Go { to: Screen::Menu },
        ));
    }
    Ok(ui
        .row(vec![
            button("Оставить мой", FormAct::Conflict { mine: true }),
            button("Оставить текущий", FormAct::Conflict { mine: false }),
        ])
        .row(footer))
}

fn field_title(field: EditorField) -> &'static str {
    match field {
        EditorField::Title => "Название",
        EditorField::Due => "Срок",
        EditorField::Priority => "Приоритет",
        EditorField::Assignee => "Исполнитель",
        EditorField::Tags => "Теги",
        EditorField::Subtasks => "Подзадачи",
        EditorField::Recurrence => "Повторение",
        EditorField::Notes => "Описание",
        EditorField::List => "Список",
        EditorField::Quantity => "Количество",
        EditorField::Unit => "Единица",
    }
}

async fn field_value(
    ctx: &mut Ctx<'_>,
    field: EditorField,
    draft: &ItemDraft,
    list_id: Uuid,
) -> Result<String, Fail> {
    Ok(match field {
        EditorField::Title => draft.title.clone(),
        EditorField::Due => {
            let (due_at, due_date) = draft.due(ctx.tz)?;
            ru::due_label(due_at, due_date, ctx.tz, ctx.now)
                .map_or_else(|| "без срока".to_owned(), |due| ru::capitalize(&due))
        }
        EditorField::Priority => priority_label(draft.priority).to_owned(),
        EditorField::Assignee => match draft.assignee {
            Some(id) => {
                let household_id = ctx.household()?.id;
                match users::get(&mut *ctx.conn, household_id, id).await {
                    Ok(user) => user.display_name,
                    Err(DomainError::NotFound) => "—".to_owned(),
                    Err(error) => return Err(Fail::Infra(into_sqlx(error))),
                }
            }
            None => "—".to_owned(),
        },
        EditorField::Tags => {
            if draft.tags.is_empty() {
                "нет".to_owned()
            } else {
                draft.tags.join(", ")
            }
        }
        EditorField::Subtasks => {
            if draft.subtasks.is_empty() {
                "нет".to_owned()
            } else {
                draft
                    .subtasks
                    .iter()
                    .map(|subtask| {
                        format!("{} {}", if subtask.done { "☑" } else { "☐" }, subtask.title)
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        }
        EditorField::Recurrence => draft
            .recurrence
            .as_ref()
            .map_or_else(|| "нет".to_owned(), describe_recurrence_draft),
        EditorField::Notes => draft.notes.clone().unwrap_or_else(|| "нет".to_owned()),
        EditorField::List => {
            let household_id = ctx.household()?.id;
            lists::get(&mut *ctx.conn, household_id, list_id)
                .await?
                .name
        }
        EditorField::Quantity => ru::quantity_label(
            draft.quantity.unwrap_or(1.0),
            draft.unit.as_deref().or(Some("шт")),
        ),
        EditorField::Unit => draft.unit.clone().unwrap_or_else(|| "шт".to_owned()),
    })
}

/// Обработать текст в открытом поле редактора.
pub(super) async fn on_text(ctx: &mut Ctx<'_>, mut editor: Editor, text: &str) -> Result<(), Fail> {
    let Some(step) = editor.field else {
        ctx.banner("Сначала сохраните или отмените правку.");
        return show(ctx, editor).await;
    };
    if !step.expects_text() {
        ctx.banner("Сначала сохраните или отмените правку.");
        return show(ctx, editor).await;
    }

    let today = today(ctx);
    let now_time = ctx.now.with_timezone(&ctx.tz).time();
    match apply_text(step, &mut editor.draft, text, today, now_time) {
        Ok(()) => {
            if let Some(field) = EditorField::from_step(step) {
                editor.touched.insert(field);
            }
            editor.error = None;
            match step {
                // Тег и подзадача добавляются по одному, экран остаётся открыт.
                Step::NewTag => {
                    editor.field = Some(Step::Tags);
                    editor.field_back = None;
                }
                Step::Subtasks => {
                    editor.field = Some(Step::Subtasks);
                    editor.field_back = None;
                }
                // Произвольный интервал вводится с подэкрана повторения:
                // после ввода возвращаемся туда, чтобы нажать «Применить».
                Step::Interval => {
                    if let Some(parent) = editor.field_back.take() {
                        editor.field = Some(parent);
                    } else {
                        editor.field = None;
                        editor.field_backup = None;
                    }
                }
                _ => {
                    editor.field = None;
                    editor.field_back = None;
                    editor.field_backup = None;
                }
            }
        }
        Err(message) => editor.error = Some(message),
    }
    show(ctx, editor).await
}

/// Обработать кнопку редактора.
pub(super) async fn on_action(
    ctx: &mut Ctx<'_>,
    mut editor: Editor,
    act: FormAct,
) -> Result<(), Fail> {
    match act {
        FormAct::Go(step) => {
            if editor.field.is_none() {
                editor.field_backup = Some(editor.draft.clone());
                editor.field_back = None;
            } else {
                // Подэкран поля: «Назад» вернёт на родительский экран, не
                // откатывая уже сделанные там изменения.
                editor.field_back = editor.field;
            }
            editor.field = Some(step);
            editor.error = None;
            show(ctx, editor).await
        }
        FormAct::Back => {
            // «Назад» из конфликта возвращает в редактор с нашими значениями:
            // выбор ещё не сделан, элемент не менялся.
            if editor.conflict.take().is_some() {
                editor.field = None;
                editor.field_back = None;
                editor.error = None;
                return show(ctx, editor).await;
            }
            if let Some(parent) = editor.field_back.take() {
                editor.field = Some(parent);
            } else {
                if let Some(backup) = editor.field_backup.take() {
                    editor.draft = backup;
                }
                editor.field = None;
            }
            editor.error = None;
            show(ctx, editor).await
        }
        FormAct::Done => {
            if let Some(step) = editor.field
                && let Some(field) = EditorField::from_step(step)
            {
                editor.touched.insert(field);
            }
            editor.field = None;
            editor.field_back = None;
            editor.field_backup = None;
            editor.error = None;
            show(ctx, editor).await
        }
        FormAct::Choose(choice) => {
            let Some(step) = editor.field else {
                return show(ctx, editor).await;
            };
            if step == Step::List {
                if let Choice::List(list_id) = choice {
                    let household_id = ctx.household()?.id;
                    let list = lists::get_visible(&mut *ctx.conn, household_id, list_id).await?;
                    if editor.is_new() {
                        switch_new_list(&mut editor, &list);
                    }
                    editor.list_id = list.id;
                    editor.touched.insert(EditorField::List);
                    editor.field = None;
                    editor.field_back = None;
                    editor.field_backup = None;
                    editor.error = None;
                } else {
                    editor.error = Some("Выберите список.".to_owned());
                }
                return show(ctx, editor).await;
            }

            let today = today(ctx);
            match apply_choice(&mut editor.draft, choice, today) {
                Ok(()) => {
                    let multi = matches!(
                        step,
                        Step::Tags
                            | Step::Subtasks
                            | Step::Weekdays
                            | Step::MonthRule
                            | Step::Recurrence
                    );
                    if multi {
                        editor.error = None;
                    } else {
                        if let Some(field) = EditorField::from_step(step) {
                            editor.touched.insert(field);
                        }
                        // Выбор на подэкране интервала возвращает на экран
                        // повторения, как и текстовый ввод.
                        if step == Step::Interval {
                            if let Some(parent) = editor.field_back.take() {
                                editor.field = Some(parent);
                            } else {
                                editor.field = None;
                                editor.field_backup = None;
                            }
                        } else {
                            editor.field = None;
                            editor.field_back = None;
                            editor.field_backup = None;
                        }
                        editor.error = None;
                    }
                }
                Err(message) => editor.error = Some(message),
            }
            show(ctx, editor).await
        }
        FormAct::Save => save(ctx, editor, false).await,
        FormAct::SaveAnyway => save(ctx, editor, true).await,
        FormAct::Increase {
            item_id,
            quantity,
            version,
        } => increase_duplicate(ctx, editor, item_id, quantity, version).await,
        FormAct::Cancel => cancel(ctx, editor).await,
        FormAct::Conflict { mine } => resolve_conflict(ctx, editor, mine).await,
        // У редактора нет «Дальше» и «Пропустить»: эти действия сюда не
        // приходят, но старый токен не должен ронять обработку.
        FormAct::Skip => show(ctx, editor).await,
    }
}

/// Переключение списка при создании сбрасывает поля другого типа.
fn switch_new_list(editor: &mut Editor, list: &crate::domain::models::List) {
    let old_kind = match &editor.target {
        EditorTarget::New { kind, .. } => *kind,
        EditorTarget::Item { .. } => return,
    };
    let was_shopping = old_kind != ListKind::Tasks;
    let shopping = list.kind != ListKind::Tasks;
    if was_shopping != shopping {
        if shopping {
            editor.draft.date = None;
            editor.draft.time = None;
            editor.draft.priority = crate::domain::models::ItemPriority::Normal;
            editor.draft.tags.clear();
            editor.draft.subtasks.clear();
            editor.draft.recurrence = None;
            for field in [
                EditorField::Due,
                EditorField::Priority,
                EditorField::Tags,
                EditorField::Subtasks,
                EditorField::Recurrence,
            ] {
                editor.touched.remove(&field);
            }
        } else {
            editor.draft.quantity = None;
            editor.draft.unit = None;
            editor.touched.remove(&EditorField::Quantity);
            editor.touched.remove(&EditorField::Unit);
        }
    }
    if let EditorTarget::New { kind, .. } = &mut editor.target {
        *kind = list.kind;
    }
    editor.duplicate = None;
}

/// Увеличить количество найденного дубликата и вернуться в список.
async fn increase_duplicate(
    ctx: &mut Ctx<'_>,
    editor: Editor,
    item_id: Uuid,
    quantity: f64,
    version: i32,
) -> Result<(), Fail> {
    let household = ctx.household()?.clone();
    let actor = ctx.actor()?;
    let patch = ItemPatch {
        quantity: Field::Value(quantity),
        ..ItemPatch::default()
    };
    items::update(
        &mut *ctx.conn,
        household.id,
        &actor,
        item_id,
        version,
        patch,
        &household.timezone,
        ctx.now,
    )
    .await?;
    let back = editor.back().clone();
    let list_id = editor.list_id;
    super::clear(ctx).await?;
    ctx.toast("Количество увеличено");
    ctx.present(created_screen(&back, list_id)).await
}

#[cfg(test)]
mod tests {
    use chrono::NaiveDate;

    use crate::domain::models::ItemPriority;

    use super::*;
    use crate::telegram::bot::model::SubtaskDraft;

    fn snapshot() -> ItemSnapshot {
        ItemSnapshot {
            list_id: Uuid::nil(),
            title: "Старое".into(),
            notes: None,
            due_at: None,
            due_date: None,
            priority: ItemPriority::Normal,
            assignee: None,
            tags: vec!["дом".into()],
            subtasks: vec![SubtaskDraft {
                title: "Позвонить".into(),
                done: false,
            }],
            recurrence: None,
            quantity: None,
            unit: None,
        }
    }

    fn draft() -> ItemDraft {
        ItemDraft {
            title: "Старое".into(),
            tags: vec!["дом".into()],
            subtasks: vec![SubtaskDraft {
                title: "Позвонить".into(),
                done: false,
            }],
            ..ItemDraft::default()
        }
    }

    #[test]
    fn touched_value_equal_to_the_snapshot_is_not_a_change() {
        let base = snapshot();
        let draft = draft();
        assert!(
            value_eq(
                EditorField::Title,
                &draft,
                base.list_id,
                &base,
                chrono_tz::UTC
            )
            .expect("title")
        );
        assert!(
            value_eq(
                EditorField::Tags,
                &draft,
                base.list_id,
                &base,
                chrono_tz::UTC
            )
            .expect("tags")
        );
        assert!(
            value_eq(
                EditorField::Subtasks,
                &draft,
                base.list_id,
                &base,
                chrono_tz::UTC
            )
            .expect("subtasks")
        );
    }

    #[test]
    fn recurrence_is_compared_without_the_occurrence_slot() {
        let mut base_rule = Recurrence {
            freq: RecurrenceFreq::Weekly,
            interval: 1,
            byweekday: vec![2, 5],
            until: None,
            timezone: "Europe/Moscow".into(),
            anchor_date: NaiveDate::from_ymd_opt(2026, 11, 3).expect("date"),
            month_rule: None,
            occurrence_date: None,
        };
        let mut current = base_rule.clone();
        current.occurrence_date = NaiveDate::from_ymd_opt(2026, 11, 4);
        assert!(current.same_rule(&base_rule));

        base_rule.interval = 2;
        assert!(!current.same_rule(&base_rule));
    }

    #[test]
    fn draft_rule_ignores_the_slot_and_anchor() {
        let rule = Recurrence {
            freq: RecurrenceFreq::Weekly,
            interval: 2,
            byweekday: vec![2, 5],
            until: None,
            timezone: "Europe/Moscow".into(),
            anchor_date: NaiveDate::from_ymd_opt(2026, 11, 3).expect("date"),
            month_rule: None,
            occurrence_date: Some(NaiveDate::from_ymd_opt(2026, 11, 5).expect("date")),
        };
        let draft = RecurrenceDraft {
            freq: RecurrenceFreq::Weekly,
            interval: 2,
            weekdays: vec![2, 5],
            month_rule: None,
        };
        assert!(draft_rule_eq(Some(&draft), Some(&rule)));
        assert!(!draft_rule_eq(None, Some(&rule)));
    }

    #[test]
    fn quantity_field_is_part_of_the_shopping_snapshot() {
        let base = snapshot();
        let mut draft = draft();
        draft.quantity = Some(2.0);
        assert!(
            !value_eq(
                EditorField::Quantity,
                &draft,
                base.list_id,
                &base,
                chrono_tz::UTC
            )
            .expect("quantity")
        );
    }
}
