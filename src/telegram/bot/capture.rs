//! Разбор текста через LLM в Telegram: приём, карточка черновика и действия.
//!
//! Бот не ждёт LLM. Текст ставится в очередь `capture_jobs`, экран показывает
//! «Разбираю…», а воркер потом заменяет его карточкой черновика. Карточка —
//! обычный экран `Screen::Draft`: она перерисовывается из базы и показывает
//! черновик в том состоянии, в котором он находится (ждёт подтверждения,
//! сохранён, отменён или истёк).

use uuid::Uuid;

use crate::capture::draft::{self, DraftError, DraftView};
use crate::capture::schema::{RawItem, RawRecurrence};
use crate::capture::validate::{parse_date, parse_time};
use crate::domain::models::{
    DraftStatus, ItemPriority, ListKind, MonthRule, NewItem, RecurrenceFreq,
};
use crate::storage::repositories::capture_jobs::{self, NewJob};
use crate::storage::repositories::drafts::DRAFT_TTL_MINUTES;
use crate::storage::repositories::items;
use crate::telegram::rich::{Span, button};
use crate::telegram::ru;

use super::card::{describe_recurrence_draft, priority_label};
use super::forms;
use super::model::{
    Action, DraftTarget, ItemDraft, ItemForm, RecurrenceDraft, Screen, Step, SubtaskDraft, Ui,
};
use super::screens::back_button;
use super::{Ctx, Fail};

/// Максимальная длина текста на разбор.
pub const MAX_TEXT: usize = 2_000;

/// Принять свободный текст на разбор.
pub async fn intake(ctx: &mut Ctx<'_>, text: &str) -> Result<(), Fail> {
    let user_id = ctx.user()?.id;
    let household_id = ctx.household()?.id;
    if text.chars().count() > MAX_TEXT {
        return ctx
            .say(&format!(
                "Сообщение слишком длинное: не больше {MAX_TEXT} символов. \
                 Сократите его или разделите на части."
            ))
            .await;
    }

    capture_jobs::enqueue(
        &mut *ctx.conn,
        NewJob::telegram_text(household_id, user_id, ctx.chat_id, text),
        ctx.now,
    )
    .await?;
    ctx.present(Screen::Processing).await
}

/// Экран ожидания разбора.
pub fn processing() -> Ui {
    Ui::new()
        .heading("⏳ Разбираю сообщение…")
        .text("Это займёт несколько секунд. Задачи появятся после вашего подтверждения.")
        .row(vec![back_button("🏠 Меню", Screen::Menu)])
}

fn kind_icon(kind: ListKind) -> &'static str {
    match kind {
        ListKind::Tasks => "📋",
        ListKind::Shopping => "🛒",
        ListKind::Checklist => "☑️",
    }
}

fn plan_recurrence(item: &NewItem) -> Option<String> {
    let rule = item.recurrence.as_ref()?;
    Some(describe_recurrence_draft(&RecurrenceDraft {
        freq: rule.freq,
        interval: rule.interval.unwrap_or(1),
        weekdays: rule.byweekday.clone(),
        month_rule: rule.month_rule,
    }))
}

/// Экран черновика.
pub async fn view(ctx: &mut Ctx<'_>, draft_id: Uuid) -> Result<Ui, Fail> {
    let household_id = ctx.household()?.id;
    let view = draft::view(&mut *ctx.conn, household_id, draft_id, ctx.now).await?;

    match view.draft.status {
        DraftStatus::Applied => return saved(ctx, &view).await,
        DraftStatus::Cancelled | DraftStatus::Rejected => {
            return Ok(closed("Черновик отменён.", "Ничего не сохранено."));
        }
        DraftStatus::Expired => return Ok(expired()),
        DraftStatus::Pending if view.draft.is_expired(ctx.now) => return Ok(expired()),
        DraftStatus::Pending => {}
    }
    Ok(card(ctx, &view))
}

fn expired() -> Ui {
    closed(
        "Черновик устарел.",
        "Он хранится 15 минут. Отправьте текст ещё раз, и я разберу его заново.",
    )
}

fn closed(title: &str, hint: &str) -> Ui {
    Ui::new()
        .heading(title)
        .text(hint)
        .row(vec![back_button("🏠 Меню", Screen::Menu)])
}

fn card(ctx: &Ctx<'_>, view: &DraftView) -> Ui {
    let draft_id = view.draft.id;
    let items = &view.validated.items;
    let total = items.len();

    let mut ui = Ui::new().heading(if total == 0 {
        "Черновик пуст".to_owned()
    } else {
        format!(
            "Разобрал: {}",
            ru::positions(i64::try_from(total).unwrap_or(0))
        )
    });

    for (index, draft_item) in items.iter().enumerate() {
        let number = index + 1;
        ui = ui.spans(vec![Span::bold(format!(
            "{number}. {} {}",
            kind_icon(draft_item.kind),
            draft_item.title
        ))]);

        match &draft_item.item {
            Some(item) => {
                for line in details(ctx, view, draft_item, item) {
                    ui = ui.text(line);
                }
            }
            None => {
                for error in &draft_item.errors {
                    ui = ui.text(format!("⚠️ {}", error.message));
                }
            }
        }

        ui = ui.row(vec![
            button(
                format!("✏️ Изменить {number}"),
                Action::DraftEdit { draft_id, index },
            ),
            button(
                format!("🗑 Удалить {number}"),
                Action::DraftRemove { draft_id, index },
            ),
        ]);
    }

    let valid = view.validated.is_valid();
    if !valid && total > 0 {
        ui = ui.text("Исправьте или удалите отмеченные элементы, чтобы сохранить.");
    }
    if total == 0 {
        ui = ui.text("Все элементы удалены. Отмените черновик или отправьте текст заново.");
    }

    let until = view
        .draft
        .expires_at
        .with_timezone(&ctx.tz)
        .format("%H:%M")
        .to_string();
    ui = ui.text(format!(
        "Черновик действует {DRAFT_TTL_MINUTES} минут — до {until}."
    ));

    let mut buttons = Vec::new();
    if valid {
        buttons.push(button("✅ Сохранить всё", Action::DraftSave { draft_id }));
    }
    buttons.push(button("✖️ Отмена", Action::DraftCancel { draft_id }));
    ui.row(buttons)
}

fn details(
    ctx: &Ctx<'_>,
    view: &DraftView,
    draft_item: &crate::capture::validate::DraftItem,
    item: &NewItem,
) -> Vec<String> {
    let mut lines = Vec::new();

    if let Some(list) = view
        .context
        .lists
        .iter()
        .find(|list| list.id == item.list_id)
    {
        let auto = if draft_item.list_auto_selected {
            " (выбран автоматически)"
        } else {
            ""
        };
        lines.push(format!("Список: {}{auto}", list.name));
    }
    if let Some(due) = ru::due_label(
        draft_item.effective_due_at,
        draft_item.effective_due_date,
        ctx.tz,
        ctx.now,
    ) {
        lines.push(format!("Срок: {due}"));
    }
    if let Some(quantity) = item.quantity {
        lines.push(format!(
            "Количество: {}",
            ru::quantity_label(quantity, item.unit.as_deref())
        ));
    }
    if let Some(name) = item.assignee_user_id.and_then(|id| {
        view.context
            .members
            .iter()
            .find(|member| member.id == id)
            .map(|member| member.display_name.clone())
    }) {
        lines.push(format!("Исполнитель: {name}"));
    }
    if item.priority != ItemPriority::Normal {
        lines.push(format!("Приоритет: {}", priority_label(item.priority)));
    }
    if !item.tags.is_empty() {
        let tags: Vec<String> = item.tags.iter().map(|tag| format!("#{tag}")).collect();
        lines.push(format!("Теги: {}", tags.join(" ")));
    }
    if !item.subtasks.is_empty() {
        lines.push(format!("Подзадач: {}", item.subtasks.len()));
    }
    if let Some(rule) = plan_recurrence(item) {
        lines.push(format!("Повторение: {rule}"));
    }
    lines
}

/// Результат сохранения с кнопкой отмены.
async fn saved(ctx: &mut Ctx<'_>, view: &DraftView) -> Result<Ui, Fail> {
    let household_id = ctx.household()?.id;
    let ids = crate::storage::repositories::drafts::linked_item_ids(
        &mut *ctx.conn,
        household_id,
        view.draft.id,
    )
    .await?;

    let mut created = Vec::new();
    for id in ids {
        match items::get(&mut *ctx.conn, household_id, id).await {
            Ok(item) => created.push(item),
            Err(crate::domain::error::DomainError::NotFound) => {}
            Err(error) => return Err(error.into()),
        }
    }

    if created.is_empty() {
        return Ok(closed("Сохранение отменено.", "Эти элементы удалены."));
    }

    let mut ui = Ui::new().heading(format!(
        "✅ Сохранено: {}",
        ru::positions(i64::try_from(created.len()).unwrap_or(0))
    ));
    for item in &created {
        let icon = match view
            .context
            .lists
            .iter()
            .find(|list| list.id == item.list_id)
        {
            Some(list) => kind_icon(list.kind),
            None => "•",
        };
        ui = ui.text(format!("{icon} {}", item.title));
    }

    let mut buttons = Vec::new();
    let undoable = view.draft.applied_at.is_some_and(|applied| {
        ctx.now - applied <= chrono::Duration::minutes(items::UNDO_WINDOW_MINUTES)
    });
    if undoable {
        buttons.push(button(
            "↩️ Отменить",
            Action::DraftUndo {
                draft_id: view.draft.id,
            },
        ));
    }
    buttons.push(back_button("🏠 Меню", Screen::Menu));
    Ok(ui.row(buttons))
}

fn fail(error: DraftError) -> Fail {
    match error {
        DraftError::Expired => {
            Fail::User("Черновик устарел: он хранится 15 минут. Отправьте текст ещё раз.".into())
        }
        DraftError::Cancelled => Fail::User("Этот черновик уже отменён.".into()),
        DraftError::Applied => Fail::User("Этот черновик уже сохранён.".into()),
        DraftError::NotApplied => Fail::User("Этот черновик ещё не сохранён.".into()),
        DraftError::UndoExpired => Fail::User(format!(
            "Время для отмены прошло: её можно сделать в течение {} минут.",
            items::UNDO_WINDOW_MINUTES
        )),
        DraftError::Changed => {
            Fail::User("Задачи уже изменены, поэтому отменить всё сразу нельзя.".into())
        }
        DraftError::NoSuchItem(_) => {
            Fail::User("Такого элемента в черновике уже нет. Показываю актуальный.".into())
        }
        DraftError::Invalid(_) => {
            Fail::User("Исправьте или удалите отмеченные элементы, чтобы сохранить.".into())
        }
        DraftError::Domain(error) => error.into(),
        DraftError::Database(error) => Fail::Infra(error),
    }
}

/// «Сохранить всё».
pub async fn save(ctx: &mut Ctx<'_>, draft_id: Uuid) -> Result<(), Fail> {
    let household_id = ctx.household()?.id;
    let actor = ctx.actor()?;
    let confirmed = draft::confirm(&mut *ctx.conn, household_id, &actor, draft_id, ctx.now)
        .await
        .map_err(fail)?;
    ctx.toast(if confirmed.replayed {
        "Уже сохранено"
    } else {
        "Сохранено"
    });
    ctx.present(Screen::Draft { draft_id }).await
}

/// «Отмена».
pub async fn cancel(ctx: &mut Ctx<'_>, draft_id: Uuid) -> Result<(), Fail> {
    let household_id = ctx.household()?.id;
    draft::cancel(&mut *ctx.conn, household_id, draft_id, ctx.now)
        .await
        .map_err(fail)?;
    ctx.toast("Отменено");
    ctx.present(Screen::Menu).await
}

/// «Удалить» элемент черновика.
pub async fn remove(ctx: &mut Ctx<'_>, draft_id: Uuid, index: usize) -> Result<(), Fail> {
    let household_id = ctx.household()?.id;
    draft::remove_item(&mut *ctx.conn, household_id, draft_id, index, ctx.now)
        .await
        .map_err(fail)?;
    ctx.toast("Удалено");
    ctx.present(Screen::Draft { draft_id }).await
}

/// «Отменить» сохранённый capture.
pub async fn undo(ctx: &mut Ctx<'_>, draft_id: Uuid) -> Result<(), Fail> {
    let household_id = ctx.household()?.id;
    let actor = ctx.actor()?;
    let removed = draft::undo(&mut *ctx.conn, household_id, &actor, draft_id, ctx.now)
        .await
        .map_err(fail)?;
    if removed.is_empty() {
        ctx.toast("Уже отменено");
    } else {
        ctx.banner(format!(
            "Отменил: {}.",
            ru::positions(i64::try_from(removed.len()).unwrap_or(0))
        ));
    }
    ctx.present(Screen::Menu).await
}

/// «Изменить»: форма создания, заполненная элементом черновика.
pub async fn edit(ctx: &mut Ctx<'_>, draft_id: Uuid, index: usize) -> Result<(), Fail> {
    let household_id = ctx.household()?.id;
    let view = draft::view(&mut *ctx.conn, household_id, draft_id, ctx.now).await?;
    if !view.is_open(ctx.now) {
        return Err(fail(match view.draft.status {
            DraftStatus::Applied => DraftError::Applied,
            DraftStatus::Pending | DraftStatus::Expired => DraftError::Expired,
            DraftStatus::Cancelled | DraftStatus::Rejected => DraftError::Cancelled,
        }));
    }
    let (Some(raw), Some(checked)) = (view.raw.get(index), view.validated.items.get(index)) else {
        return Err(fail(DraftError::NoSuchItem(index)));
    };

    let kind = checked.kind;
    let list_id = checked.list_id;
    let draft_item = seed(raw, &view, ctx.tz);
    forms::open_create(
        ctx,
        ItemForm {
            list_id,
            kind,
            step: if list_id.is_some() {
                Step::Preview
            } else {
                Step::List
            },
            draft: draft_item,
            from_preview: false,
            error: None,
            duplicate: None,
            target: Some(DraftTarget { draft_id, index }),
        },
    )
    .await
}

/// Сохранить результат формы в элемент черновика.
pub async fn save_form(ctx: &mut Ctx<'_>, mut form: ItemForm) -> Result<(), Fail> {
    let Some(target) = form.target else {
        return Ok(());
    };
    let household_id = ctx.household()?.id;
    let Some(list_id) = form.list_id else {
        form.step = Step::List;
        form.error = Some("Выберите список.".to_owned());
        return forms::open_create(ctx, form).await;
    };
    let list =
        crate::storage::repositories::lists::get_visible(&mut *ctx.conn, household_id, list_id)
            .await?;

    let previous = draft::view(&mut *ctx.conn, household_id, target.draft_id, ctx.now)
        .await?
        .raw
        .get(target.index)
        .and_then(|raw| raw.recurrence.as_ref().and_then(|rule| rule.until.clone()));

    let item = to_raw(&form.draft, list.kind, &list.name, previous);
    draft::replace_item(
        &mut *ctx.conn,
        household_id,
        target.draft_id,
        target.index,
        item,
        ctx.now,
    )
    .await
    .map_err(fail)?;

    forms::finish(ctx).await?;
    ctx.toast("Изменено");
    ctx.present(Screen::Draft {
        draft_id: target.draft_id,
    })
    .await
}

/// Выйти из формы правки назад к черновику.
pub async fn leave_form(ctx: &mut Ctx<'_>, target: DraftTarget) -> Result<(), Fail> {
    forms::finish(ctx).await?;
    ctx.toast("Отменено");
    ctx.present(Screen::Draft {
        draft_id: target.draft_id,
    })
    .await
}

fn parse_priority(value: &str) -> ItemPriority {
    match value.trim().to_lowercase().as_str() {
        "low" => ItemPriority::Low,
        "high" => ItemPriority::High,
        "urgent" => ItemPriority::Urgent,
        _ => ItemPriority::Normal,
    }
}

/// Заполнить форму по элементу черновика. Что не удалось понять, остаётся
/// пустым: пользователь увидит это в предпросмотре и поправит.
fn seed(raw: &RawItem, view: &DraftView, _tz: chrono_tz::Tz) -> ItemDraft {
    let date = raw.due_date.as_deref().and_then(parse_date);
    let time = date.and(raw.due_time.as_deref().and_then(parse_time));
    let assignee = raw.assignee_id.or_else(|| {
        let name = raw
            .assignee
            .as_deref()?
            .trim()
            .trim_start_matches('@')
            .to_lowercase();
        let mut found = view.context.members.iter().filter(|member| {
            member.display_name.to_lowercase() == name
                || member
                    .username
                    .as_deref()
                    .is_some_and(|username| username.to_lowercase() == name)
        });
        match (found.next(), found.next()) {
            (Some(member), None) => Some(member.id),
            _ => None,
        }
    });
    let recurrence = raw.recurrence.as_ref().and_then(|rule| {
        let freq = match rule.freq.as_deref()?.trim().to_lowercase().as_str() {
            "daily" => RecurrenceFreq::Daily,
            "weekly" => RecurrenceFreq::Weekly,
            "monthly" => RecurrenceFreq::Monthly,
            "yearly" => RecurrenceFreq::Yearly,
            _ => return None,
        };
        let weekdays: Vec<i16> = rule
            .weekdays
            .iter()
            .filter_map(|day| i16::try_from(*day).ok())
            .filter(|day| (1..=7).contains(day))
            .collect();
        Some(RecurrenceDraft {
            freq,
            interval: rule
                .interval
                .and_then(|interval| i16::try_from(interval).ok())
                .filter(|interval| (1..=365).contains(interval))
                .unwrap_or(1),
            weekdays: if freq == RecurrenceFreq::Weekly && weekdays.is_empty() {
                vec![1]
            } else {
                weekdays
            },
            month_rule: (freq == RecurrenceFreq::Monthly).then(|| {
                match rule.month_rule.as_deref().map(str::trim) {
                    Some("last_day") => MonthRule::LastDay,
                    _ => MonthRule::AnchorDay,
                }
            }),
        })
    });

    ItemDraft {
        title: raw.title.clone(),
        notes: raw.notes.clone(),
        date,
        time,
        priority: raw
            .priority
            .as_deref()
            .map_or(ItemPriority::Normal, parse_priority),
        assignee,
        tags: raw.tags.clone(),
        subtasks: raw
            .subtasks
            .iter()
            .map(|title| SubtaskDraft {
                title: title.clone(),
                done: false,
            })
            .collect(),
        recurrence,
        quantity: raw.quantity,
        unit: raw.unit.clone(),
    }
}

/// Элемент черновика по заполненной форме.
fn to_raw(draft: &ItemDraft, kind: ListKind, list_name: &str, until: Option<String>) -> RawItem {
    let priority = match draft.priority {
        ItemPriority::Low => "low",
        ItemPriority::Normal => "normal",
        ItemPriority::High => "high",
        ItemPriority::Urgent => "urgent",
    };
    let recurrence = draft.recurrence.as_ref().map(|rule| RawRecurrence {
        freq: Some(
            match rule.freq {
                RecurrenceFreq::Daily => "daily",
                RecurrenceFreq::Weekly => "weekly",
                RecurrenceFreq::Monthly => "monthly",
                RecurrenceFreq::Yearly => "yearly",
            }
            .to_owned(),
        ),
        interval: Some(i64::from(rule.interval)),
        month_rule: rule.month_rule.map(|rule| {
            match rule {
                MonthRule::AnchorDay => "anchor_day",
                MonthRule::LastDay => "last_day",
            }
            .to_owned()
        }),
        weekdays: rule.weekdays.iter().map(|day| i64::from(*day)).collect(),
        until,
    });
    let shopping = kind != ListKind::Tasks;

    RawItem {
        target_kind: kind,
        target_list: Some(list_name.to_owned()),
        title: draft.title.clone(),
        notes: draft.notes.clone(),
        due_date: draft.date.map(|date| date.format("%Y-%m-%d").to_string()),
        due_time: draft
            .date
            .and(draft.time)
            .map(|time| time.format("%H:%M").to_string()),
        priority: Some(priority.to_owned()),
        assignee: None,
        assignee_id: draft.assignee,
        tags: draft.tags.clone(),
        quantity: shopping.then_some(draft.quantity).flatten(),
        unit: shopping.then(|| draft.unit.clone()).flatten(),
        subtasks: draft
            .subtasks
            .iter()
            .map(|subtask| subtask.title.clone())
            .collect(),
        recurrence,
    }
}
