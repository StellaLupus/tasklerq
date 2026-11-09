//! Разбор текста через LLM в Telegram: приём, карточка черновика и действия.
//!
//! Бот не ждёт LLM. Текст ставится в очередь `capture_jobs`, экран показывает
//! «Разбираю…», а воркер потом заменяет его карточкой черновика. Карточка —
//! обычный экран `Screen::Draft`: она перерисовывается из базы и показывает
//! черновик в том состоянии, в котором он находится (ждёт подтверждения,
//! сохранён, отменён или истёк).

use uuid::Uuid;

use crate::capture::draft::{self, DraftError, DraftView, DueChoice};
use crate::capture::schema::{MAX_ITEMS, RawItem, RawRecurrence};
use crate::capture::validate::{DraftItem, parse_date, parse_time};
use crate::domain::models::{
    DraftStatus, ItemPriority, ListKind, MonthRule, NewItem, RecurrenceFreq,
};
use crate::storage::repositories::capture_jobs::{self, NewJob};
use crate::storage::repositories::drafts::DRAFT_TTL_MINUTES;
use crate::storage::repositories::{items, lists};
use crate::telegram::rich::{Block, Span, button, inline_char_len, inline_len};
use crate::telegram::ru;
use crate::telegram::views;

use super::additions::add_parts;
use super::card::{describe_recurrence_draft, priority_label};
use super::forms;
use super::model::{
    Action, DuplicateChoice, Editor, EditorTarget, ItemDraft, RecurrenceDraft, Screen, Step,
    SubtaskDraft, Ui, addition_rows, duplicate_notice, list_screen,
};
use super::screens::nav_row;
use super::{Ctx, Fail};

mod clarify;

/// Максимальная длина текста на разбор.
pub const MAX_TEXT: usize = 2_000;

/// Сколько разборов пользователь может запустить за минуту.
pub const PARSES_PER_MINUTE: i64 = 10;

/// Принять свободный текст на разбор.
pub async fn intake(
    ctx: &mut Ctx<'_>,
    text: &str,
    default_list_id: Option<Uuid>,
) -> Result<(), Fail> {
    if !accept(ctx, text).await? {
        return Ok(());
    }
    let user_id = ctx.user()?.id;
    let household_id = ctx.household()?.id;
    let job = match default_list_id {
        Some(list_id) => {
            NewJob::telegram_text_in_list(household_id, user_id, ctx.chat_id, text, list_id)
        }
        None => NewJob::telegram_text(household_id, user_id, ctx.chat_id, text),
    };
    let job_id = capture_jobs::enqueue(&mut *ctx.conn, job, ctx.now).await?;
    ctx.present(Screen::Processing {
        job_id: Some(job_id),
    })
    .await
}

/// Принять текст на прямое добавление покупок в список с активного экрана.
///
/// Лимиты те же, что у обычного разбора. Экран списка остаётся на месте и
/// получает баннер «Добавляю…»: карточки ожидания здесь нет.
pub async fn intake_direct(ctx: &mut Ctx<'_>, text: &str, list_id: Uuid) -> Result<(), Fail> {
    if !accept(ctx, text).await? {
        return Ok(());
    }
    let user_id = ctx.user()?.id;
    let household_id = ctx.household()?.id;
    let job = NewJob::telegram_text_direct(household_id, user_id, ctx.chat_id, text, list_id);
    capture_jobs::enqueue(&mut *ctx.conn, job, ctx.now).await?;
    ctx.banner("Добавляю…");
    ctx.refresh().await
}

/// Проверить длину и частоту. Отказ показывается баннером на активном экране.
async fn accept(ctx: &mut Ctx<'_>, text: &str) -> Result<bool, Fail> {
    if !accept_length(ctx, text).await? {
        return Ok(false);
    }

    let user_id = ctx.user()?.id;
    let household_id = ctx.household()?.id;
    let recent = capture_jobs::count_recent_for_user(
        &mut *ctx.conn,
        household_id,
        user_id,
        ctx.now - chrono::Duration::minutes(1),
    )
    .await?;
    if recent >= PARSES_PER_MINUTE {
        ctx.banner(format!(
            "Слишком много сообщений на разбор: не больше {PARSES_PER_MINUTE} в минуту. \
             Подождите немного и отправьте снова."
        ));
        ctx.refresh().await?;
        return Ok(false);
    }
    Ok(true)
}

/// Проверить только длину: деление без разбора не вызывает LLM, поэтому
/// частота разборов его не ограничивает.
async fn accept_length(ctx: &mut Ctx<'_>, text: &str) -> Result<bool, Fail> {
    if text.chars().count() > MAX_TEXT {
        ctx.banner(format!(
            "Сообщение слишком длинное: не больше {MAX_TEXT} символов. \
             Сократите его или разделите на части."
        ));
        ctx.refresh().await?;
        return Ok(false);
    }
    Ok(true)
}

/// Части текста без разбора: по запятым и переводам строк, без пустых.
pub(crate) fn split_titles(text: &str) -> Vec<String> {
    text.split([',', '\n'])
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Текст баннера после добавления без разбора. `skipped` — части, которые не
/// прошли проверку домена: их нужно назвать, а не терять молча.
pub(crate) fn split_banner(
    target_name: &str,
    added: &[String],
    duplicates: &[DuplicateChoice],
    skipped: &[String],
    on_list: bool,
) -> String {
    let mut text = if added.is_empty() {
        if duplicates.is_empty() && skipped.is_empty() {
            "Не нашёл в сообщении покупок.".to_owned()
        } else {
            String::new()
        }
    } else if on_list {
        format!(
            "Добавлено без разбора (количество не распознаётся): {}",
            added.join(", ")
        )
    } else {
        format!(
            "Добавлено в «{target_name}» без разбора (количество не распознаётся): {}",
            added.join(", ")
        )
    };
    if let Some(notice) = duplicate_notice(duplicates) {
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(&notice);
    }
    if !skipped.is_empty() {
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(&format!(
            "Не добавлено (слишком длинное название): {}",
            skipped.join(", ")
        ));
    }
    text
}

/// Добавить покупки без разбора на экране списка: текст делится по запятым и
/// переводам строк, каждая часть становится позицией с количеством 1.
pub async fn intake_split(ctx: &mut Ctx<'_>, text: &str, list_id: Uuid) -> Result<(), Fail> {
    if !accept_length(ctx, text).await? {
        return Ok(());
    }
    let household_id = ctx.household()?.id;
    let actor = ctx.actor()?;
    let list = lists::get_visible(&mut *ctx.conn, household_id, list_id).await?;
    if list.kind != ListKind::Shopping {
        return forms::quick_add_in_list(ctx, text, Some(list_id)).await;
    }

    let parts = split_titles(text);
    if parts.len() > MAX_ITEMS {
        ctx.banner(format!(
            "Слишком много позиций: не больше {MAX_ITEMS} за раз. Разделите текст"
        ));
        return ctx.refresh().await;
    }

    let timezone = ctx.tz;
    let today = ctx.now.with_timezone(&timezone).date_naive();
    let added = add_parts(
        &mut *ctx.conn,
        household_id,
        &actor,
        list.id,
        timezone,
        today,
        ctx.now,
        parts,
    )
    .await?;

    let titles: Vec<String> = added
        .created
        .iter()
        .map(|item| item.title.clone())
        .collect();
    let created_ids: Vec<Uuid> = added.created.iter().map(|item| item.id).collect();
    let rows = addition_rows(&added.duplicates, list.id, &created_ids, None);
    let banner = split_banner(&list.name, &titles, &added.duplicates, &added.skipped, true);
    ctx.banner_rows(banner, rows);

    let current = views::get(&mut *ctx.conn, ctx.chat_id)
        .await?
        .and_then(|view| serde_json::from_value::<Screen>(view.state).ok());
    ctx.present(list_screen(current.as_ref(), list.id)).await
}

/// Экран ожидания разбора.
pub async fn processing(ctx: &mut Ctx<'_>, job_id: Option<Uuid>) -> Result<Ui, Fail> {
    let text = match (
        job_id,
        ctx.user.as_ref().map(|user| user.id),
        ctx.household.as_ref().map(|household| household.id),
    ) {
        (Some(job_id), Some(user_id), Some(household_id)) => {
            capture_jobs::text_for_user(&mut *ctx.conn, household_id, user_id, job_id).await?
        }
        _ => None,
    };

    let mut ui = Ui::new();
    if let Some(text) = text.as_deref().filter(|text| !text.is_empty()) {
        ui = ui.quote(text);
    }
    let screen = Screen::Processing { job_id };
    Ok(ui
        .heading(screen.trail("⏳ Разбираю сообщение…", None))
        .text("Это займёт несколько секунд. Задачи появятся после вашего подтверждения.")
        .row(nav_row(&screen, Screen::Menu)))
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

    let screen = Screen::Draft { draft_id };
    match view.draft.status {
        DraftStatus::Applied => return saved(ctx, &view).await,
        DraftStatus::Cancelled | DraftStatus::Rejected => {
            return Ok(closed("Черновик отменён.", "Ничего не сохранено.", &screen));
        }
        DraftStatus::Expired => return Ok(expired(&screen)),
        DraftStatus::Pending if view.draft.is_expired(ctx.now) => return Ok(expired(&screen)),
        DraftStatus::Pending => {}
    }
    Ok(card(ctx, &view))
}

fn expired(screen: &Screen) -> Ui {
    closed(
        "Черновик устарел.",
        "Он хранится 15 минут. Отправьте текст ещё раз, и я разберу его заново.",
        screen,
    )
}

fn closed(title: &str, hint: &str, screen: &Screen) -> Ui {
    Ui::new()
        .heading(title)
        .text(hint)
        .row(nav_row(screen, Screen::Menu))
}

/// Лимит длины HTML карточки: запас под лимит сообщения Telegram.
const MAX_CARD_HTML_CHARS: usize = 4_096;

/// Разметка одной свёрнутой цитаты вместе с переводом строки.
const QUOTE_MARKUP: usize = "<blockquote expandable></blockquote>\n".len();

/// Части исходного текста для цитат карточки: каждая часть отдельно, а у
/// старого черновика без `raw_parts` — одна цитата из `raw_text`.
///
/// `available` — сколько символов HTML осталось после остальных блоков
/// карточки. Если все части помещаются, они показываются целиком; иначе делят
/// остаток поровну и обрезаются с многоточием.
fn card_quotes(view: &DraftView, available: usize) -> Vec<String> {
    let parts = if view.draft.raw_parts.is_empty() {
        if view.draft.raw_text.is_empty() {
            Vec::new()
        } else {
            vec![view.draft.raw_text.clone()]
        }
    } else {
        view.draft.raw_parts.clone()
    };
    if parts.is_empty() {
        return Vec::new();
    }

    let overhead = parts.len().saturating_mul(QUOTE_MARKUP);
    let budget = available.saturating_sub(overhead);
    let total: usize = parts.iter().map(|part| inline_len(part)).sum();
    if total <= budget {
        return parts;
    }

    let mut remaining = budget;
    let count = parts.len();
    parts
        .iter()
        .enumerate()
        .map(|(index, part)| {
            let share = remaining / (count - index);
            let clipped = clip_quote(part, share);
            remaining = remaining.saturating_sub(inline_len(&clipped));
            clipped
        })
        .collect()
}

/// Длина экрана в HTML без учёта цитат.
fn rendered_len(ui: &Ui) -> usize {
    let token = format!("t:{}", "0".repeat(16));
    let tokens = vec![token; ui.button_count()];
    crate::telegram::rich::render(ui, &tokens)
        .html
        .chars()
        .count()
}

/// Обрезать часть цитаты до бюджета экранированных символов.
fn clip_quote(text: &str, budget: usize) -> String {
    if inline_len(text) <= budget {
        return text.to_owned();
    }
    let mut out = String::new();
    let mut used = 0;
    // Место под многоточие: обрезка должна быть видна.
    let reserve = usize::from(budget > 0);
    for ch in text.chars() {
        let length = inline_char_len(ch);
        if used + length + reserve > budget {
            break;
        }
        used += length;
        out.push(ch);
    }
    if !out.is_empty() {
        out.push('…');
    }
    out
}

fn card(ctx: &Ctx<'_>, view: &DraftView) -> Ui {
    let draft_id = view.draft.id;
    let items = &view.validated.items;
    let total = items.len();

    let mut ui = Ui::new();
    ui = ui.heading(if total == 0 {
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

    ui = clarify::clarification(ui, view);

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
    ui = ui.row(buttons);

    let banner = ctx
        .banner
        .as_ref()
        .map_or(0, |banner| banner.html_len() + 1);
    let room = MAX_CARD_HTML_CHARS.saturating_sub(rendered_len(&ui).saturating_add(banner));
    let quotes: Vec<Block<Action>> = card_quotes(view, room)
        .into_iter()
        .filter(|quote| !quote.is_empty())
        .map(Block::Quote)
        .collect();
    if !quotes.is_empty() {
        ui.blocks.splice(0..0, quotes);
    }
    ui
}

fn details(ctx: &Ctx<'_>, view: &DraftView, draft_item: &DraftItem, item: &NewItem) -> Vec<String> {
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
        return Ok(closed(
            "Сохранение отменено.",
            "Эти элементы удалены.",
            &Screen::Draft {
                draft_id: view.draft.id,
            },
        ));
    }

    let mut ui = Ui::new().heading(format!(
        "✅ Сохранено: {}",
        ru::positions(i64::try_from(created.len()).unwrap_or(0))
    ));
    for item in &created {
        let icon = view
            .context
            .lists
            .iter()
            .find(|list| list.id == item.list_id)
            .map_or("•", |list| kind_icon(list.kind));
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
    buttons.extend(nav_row(
        &Screen::Draft {
            draft_id: view.draft.id,
        },
        Screen::Menu,
    ));
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
        DraftError::ClarificationLimit => Fail::User(
            "Больше трёх уточнений не задаю. Откройте «Изменить» и поправьте элемент вручную."
                .into(),
        ),
        DraftError::NoClarification => {
            Fail::User("Этот вопрос уже неактуален. Показываю актуальный черновик.".into())
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

/// Ответ на вопрос об исполнителе.
pub async fn assign(
    ctx: &mut Ctx<'_>,
    draft_id: Uuid,
    index: usize,
    user_id: Uuid,
) -> Result<(), Fail> {
    let household_id = ctx.household()?.id;
    draft::answer_assignee(
        &mut *ctx.conn,
        household_id,
        draft_id,
        index,
        user_id,
        ctx.now,
    )
    .await
    .map_err(fail)?;
    ctx.toast("Исполнитель выбран");
    ctx.present(Screen::Draft { draft_id }).await
}

/// Ответ на вопрос о дате: «сегодня» или «завтра».
pub async fn due(
    ctx: &mut Ctx<'_>,
    draft_id: Uuid,
    index: usize,
    choice: DueChoice,
) -> Result<(), Fail> {
    let household_id = ctx.household()?.id;
    draft::answer_due_date(
        &mut *ctx.conn,
        household_id,
        draft_id,
        index,
        choice,
        ctx.now,
    )
    .await
    .map_err(fail)?;
    ctx.toast("Срок уточнён");
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

/// «Изменить»: редактор пункта черновика.
pub async fn edit(ctx: &mut Ctx<'_>, draft_id: Uuid, index: usize) -> Result<(), Fail> {
    open_editor(ctx, draft_id, index, None).await
}

/// «Выбрать дату» в уточняющем вопросе: тот же редактор сразу на поле срока.
pub async fn edit_due(ctx: &mut Ctx<'_>, draft_id: Uuid, index: usize) -> Result<(), Fail> {
    open_editor(ctx, draft_id, index, Some(Step::Due)).await
}

async fn open_editor(
    ctx: &mut Ctx<'_>,
    draft_id: Uuid,
    index: usize,
    field: Option<Step>,
) -> Result<(), Fail> {
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

    let list_missing = checked.list_id.is_none();
    let list = match checked.list_id {
        Some(list_id) => lists::get_visible(&mut *ctx.conn, household_id, list_id).await?,
        None => default_list(ctx, checked.kind).await?,
    };
    let values = seed(raw, &view);
    let editor = Editor {
        target: EditorTarget::DraftItem {
            draft_id,
            index,
            kind: checked.kind,
            list_missing,
        },
        field_backup: if list_missing {
            None
        } else {
            field.map(|_| values.clone())
        },
        draft: values,
        touched: std::collections::BTreeSet::new(),
        field: if list_missing {
            Some(Step::List)
        } else {
            field
        },
        field_back: None,
        list_id: list.id,
        error: None,
        conflict: None,
        conflict_seen: None,
        duplicate: None,
    };
    forms::open_editor(ctx, editor).await
}

/// Список для пункта черновика без выбранного списка.
async fn default_list(
    ctx: &mut Ctx<'_>,
    kind: ListKind,
) -> Result<crate::domain::models::List, Fail> {
    let household_id = ctx.household()?.id;
    let all = lists::list(&mut *ctx.conn, household_id).await?;
    all.iter()
        .filter(|list| !list.is_archived)
        .find(|list| list.kind == kind && list.is_default)
        .or_else(|| {
            all.iter()
                .find(|list| !list.is_archived && list.kind == kind)
        })
        .or_else(|| {
            all.iter()
                .find(|list| !list.is_archived && list.kind == ListKind::Tasks)
        })
        .or_else(|| all.iter().find(|list| !list.is_archived))
        .cloned()
        .ok_or_else(|| Fail::User("Нет списка для сохранения.".into()))
}

/// Записать пункт в черновик и перепроверить его.
pub(super) async fn save_draft_item(
    ctx: &mut Ctx<'_>,
    draft_id: Uuid,
    index: usize,
    list_id: Uuid,
    values: &ItemDraft,
) -> Result<(), Fail> {
    let household_id = ctx.household()?.id;
    let list = lists::get_visible(&mut *ctx.conn, household_id, list_id).await?;
    let previous = draft::view(&mut *ctx.conn, household_id, draft_id, ctx.now)
        .await?
        .raw
        .get(index)
        .and_then(|raw| raw.recurrence.as_ref().and_then(|rule| rule.until.clone()));

    let item = to_raw(values, list.kind, &list.name, previous);
    draft::replace_item(&mut *ctx.conn, household_id, draft_id, index, item, ctx.now)
        .await
        .map_err(fail)?;
    Ok(())
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
fn seed(raw: &RawItem, view: &DraftView) -> ItemDraft {
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
