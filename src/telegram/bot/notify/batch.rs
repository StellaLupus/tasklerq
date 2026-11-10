//! Сводное сообщение после тихих часов: блоки, пагинация и действия.
//!
//! Блок показывает актуальное состояние задачи из БД. Если задачу изменили
//! после выпуска, блок перерисовывается с новым названием, сроком и версией:
//! действие по устаревшей версии ничего не меняет, только обновляет сообщение.

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::models::{ItemStatus, ListKind};
use crate::jobs::keys::snooze_origin;
use crate::storage::repositories::items;
use crate::storage::repositories::notifications::{
    self, BatchDelivery, BatchState, NotificationKind,
};
use crate::telegram::rich::button;
use crate::telegram::ru;

use super::super::model::{Action, SnoozeUntil, Ui};
use super::super::{Ctx, Fail};
use super::{edit_notice, insert_snooze, open, snooze_time, title_spans, when_line};

/// Сколько блоков сводного сообщения помещается на страницу.
pub const BATCH_PAGE_SIZE: usize = 5;

/// Число страниц сводного сообщения; пустое сообщение — одна страница.
fn page_count(total: usize) -> usize {
    total.div_ceil(BATCH_PAGE_SIZE).max(1)
}

/// Подпись вида уведомления в блоке.
fn kind_label(kind: NotificationKind) -> &'static str {
    match kind {
        NotificationKind::Reminder => "⏰ напоминание",
        NotificationKind::Snooze => "😴 отложенное",
        NotificationKind::Assignment => "👤 назначение",
        NotificationKind::Digest => unreachable!("digest is not a batch entry"),
    }
}

fn is_assignment(kind: NotificationKind) -> bool {
    kind == NotificationKind::Assignment
}

/// Состояние блока сводного сообщения.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchEntryState {
    /// Задача открыта, действия доступны.
    Open,
    /// Событие: не выполняется и не откладывается, действия — перенос и
    /// открытие карточки.
    Event,
    /// Задача завершена.
    Done,
    /// Задача отложена до указанного момента.
    Snoozed(DateTime<Utc>),
    /// Задача удалена.
    Missing,
    /// Список, в котором лежит задача, отправлен в архив.
    ListArchived,
}

/// Блок сводного сообщения.
#[derive(Debug, Clone)]
pub struct BatchEntry {
    /// Элемент.
    pub item_id: Uuid,
    /// Вид уведомления.
    pub kind: NotificationKind,
    /// Название.
    pub title: String,
    /// Срок и список.
    pub when: String,
    /// Актуальная версия задачи.
    pub version: i32,
    /// Состояние блока.
    pub state: BatchEntryState,
    /// Блок события: у него свои подписи и нет общих действий.
    pub is_event: bool,
}

/// Применимые ко всему сообщению общие действия.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BatchActions {
    /// Есть открытые задачи или задача с отложенным напоминанием.
    pub complete_all: bool,
    /// Есть открытая задача, которую можно отложить.
    pub snooze_all: bool,
}

impl BatchActions {
    /// Посчитать действия по всем блокам сообщения, включая другие страницы.
    pub fn of(entries: &[BatchEntry]) -> Self {
        let mut actions = Self::default();
        for entry in entries {
            match entry.state {
                BatchEntryState::Open => {
                    actions.complete_all = true;
                    actions.snooze_all = true;
                }
                BatchEntryState::Snoozed(_) => actions.complete_all = true,
                _ => {}
            }
        }
        actions
    }
}

/// Сводное сообщение после тихих часов.
///
/// `entries` — уже выбранная страница; `total` — число блоков во всём
/// сообщении. Блоки разделены [`crate::telegram::rich::View::rule`], под каждым
/// стоят его кнопки. Внизу — пагинация и общие действия.
pub fn batch_ui(
    batch_id: Uuid,
    page: usize,
    entries: &[BatchEntry],
    total: usize,
    summary: Option<(i32, i32)>,
    actions: BatchActions,
    tz: Tz,
) -> Ui {
    let pages = page_count(total);
    let count = i64::try_from(total).unwrap_or(i64::MAX);
    let mut ui = Ui::new().heading(format!(
        "🌙 Пока были тихие часы — {count} {}",
        ru::plural(count, "уведомление", "уведомления", "уведомлений")
    ));
    if let Some((completed, changed)) = summary {
        ui = ui.text(format!("Выполнено {completed} · {changed} уже изменились"));
    }

    for (index, entry) in entries.iter().enumerate() {
        if index > 0 {
            ui = ui.rule();
        }
        ui = ui.bold(kind_label(entry.kind));
        let head = |ui: Ui| ui.spans(title_spans(&entry.title)).text(&entry.when);
        ui = match entry.state {
            BatchEntryState::Missing if entry.is_event => ui.text("Событие удалено"),
            BatchEntryState::Missing => ui.text("Задача удалена"),
            BatchEntryState::ListArchived => ui.text("Список в архиве"),
            BatchEntryState::Done => head(ui).text("✅ Выполнено"),
            BatchEntryState::Snoozed(until) => head(ui).text(format!(
                "😴 Напомню в {}",
                until.with_timezone(&tz).format("%H:%M")
            )),
            BatchEntryState::Event => {
                let open = button(
                    "📋 Открыть",
                    Action::BatchOpen {
                        batch_id,
                        item_id: entry.item_id,
                    },
                );
                let move_button = button(
                    "📅 Перенести",
                    Action::BatchMove {
                        batch_id,
                        item_id: entry.item_id,
                    },
                );
                head(ui).row(vec![move_button, open])
            }
            BatchEntryState::Open => {
                let done = button(
                    ru::DONE_BUTTON,
                    Action::BatchComplete {
                        batch_id,
                        item_id: entry.item_id,
                        version: entry.version,
                    },
                );
                let open = button(
                    "📋 Открыть",
                    Action::BatchOpen {
                        batch_id,
                        item_id: entry.item_id,
                    },
                );
                let buttons = if is_assignment(entry.kind) {
                    vec![open, done]
                } else {
                    vec![
                        done,
                        button(
                            "😴 На час",
                            Action::BatchSnooze {
                                batch_id,
                                item_id: entry.item_id,
                                version: Some(entry.version),
                            },
                        ),
                        open,
                    ]
                };
                head(ui).row(buttons)
            }
        };
    }

    if pages > 1 {
        let mut row = Vec::new();
        if page > 0 {
            row.push(button(
                "←",
                Action::BatchPage {
                    batch_id,
                    page: u32::try_from(page - 1).unwrap_or(0),
                },
            ));
        }
        row.push(button(
            format!("{}/{}", page + 1, pages),
            Action::BatchPage {
                batch_id,
                page: u32::try_from(page).unwrap_or(0),
            },
        ));
        if page + 1 < pages {
            row.push(button(
                "→",
                Action::BatchPage {
                    batch_id,
                    page: u32::try_from(page + 1).unwrap_or(u32::MAX),
                },
            ));
        }
        ui = ui.rule().row(row);
    }

    let mut common = Vec::new();
    if actions.complete_all {
        common.push(button(
            ru::DONE_ALL_BUTTON,
            Action::BatchCompleteAll { batch_id },
        ));
    }
    if actions.snooze_all {
        common.push(button(
            "😴 Отложить все на час",
            Action::BatchSnoozeAll { batch_id },
        ));
    }
    if common.is_empty() {
        ui
    } else {
        ui.row(common)
    }
}

/// Перелистнуть сводное сообщение.
pub async fn batch_page(
    ctx: &mut Ctx<'_>,
    message_id: i64,
    batch_id: Uuid,
    page: u32,
) -> Result<(), Fail> {
    if load_batch(ctx, batch_id).await?.is_none() {
        return Ok(());
    }
    let rows = notifications::batch_deliveries(&mut *ctx.conn, batch_id).await?;
    let page = (page as usize).min(page_count(rows.len()) - 1);
    notifications::set_batch_page(
        &mut *ctx.conn,
        batch_id,
        i32::try_from(page).unwrap_or(0),
        ctx.now,
    )
    .await?;
    render_batch(ctx, message_id, batch_id).await
}

/// «Выполнено» в блоке сводного сообщения.
pub async fn batch_complete(
    ctx: &mut Ctx<'_>,
    message_id: i64,
    batch_id: Uuid,
    item_id: Uuid,
    version: i32,
) -> Result<(), Fail> {
    if !actionable(
        ctx,
        message_id,
        batch_id,
        item_id,
        "Событие нельзя выполнить",
    )
    .await?
    {
        return Ok(());
    }

    let household = ctx.household()?.clone();
    let actor = ctx.actor()?;
    let result = items::complete(
        &mut *ctx.conn,
        household.id,
        &actor,
        item_id,
        Some(version),
        &household.timezone,
        ctx.now,
    )
    .await;
    match result {
        Ok(_) => {
            set_batch_done(ctx, batch_id, item_id).await?;
            ctx.toast(ru::DONE_TOAST);
        }
        Err(DomainError::Database(error)) => return Err(Fail::Infra(error)),
        Err(DomainError::Conflict { .. } | DomainError::NotFound) => {
            // Задачу успели изменить: действие не повторяется, а блок
            // перерисовывается по текущему состоянию.
            match items::get(&mut *ctx.conn, household.id, item_id).await {
                Ok(item) if item.status == ItemStatus::Done => {
                    set_batch_done(ctx, batch_id, item_id).await?;
                    ctx.toast("Задача уже выполнена");
                }
                Ok(_) => ctx.toast("Задача уже изменилась"),
                Err(DomainError::Database(error)) => return Err(Fail::Infra(error)),
                Err(_) => {
                    set_batch_missing(ctx, batch_id, item_id).await?;
                    ctx.toast("Задача больше недоступна");
                }
            }
        }
        Err(error) => return Err(error.into()),
    }
    render_batch(ctx, message_id, batch_id).await
}

/// «На час» в блоке сводного сообщения.
pub async fn batch_snooze(
    ctx: &mut Ctx<'_>,
    message_id: i64,
    batch_id: Uuid,
    item_id: Uuid,
    version: Option<i32>,
) -> Result<(), Fail> {
    if !actionable(
        ctx,
        message_id,
        batch_id,
        item_id,
        "Событие нельзя отложить",
    )
    .await?
    {
        return Ok(());
    }
    if snooze_batch_item(ctx, batch_id, item_id, version, false).await? {
        ctx.toast("Отложено");
    }
    render_batch(ctx, message_id, batch_id).await
}

/// «Открыть» в блоке сводного сообщения.
pub async fn batch_open(
    ctx: &mut Ctx<'_>,
    message_id: i64,
    batch_id: Uuid,
    item_id: Uuid,
) -> Result<(), Fail> {
    if load_batch(ctx, batch_id).await?.is_none() {
        return Ok(());
    }
    let household_id = ctx.household()?.id;
    match items::get(&mut *ctx.conn, household_id, item_id).await {
        Ok(_) => open(ctx, message_id, item_id).await,
        Err(DomainError::Database(error)) => Err(Fail::Infra(error)),
        Err(_) => {
            set_batch_missing(ctx, batch_id, item_id).await?;
            ctx.toast("Задача больше недоступна");
            render_batch(ctx, message_id, batch_id).await
        }
    }
}

/// «📅 Перенести» в блоке события сводного сообщения.
pub async fn batch_move(
    ctx: &mut Ctx<'_>,
    message_id: i64,
    batch_id: Uuid,
    item_id: Uuid,
) -> Result<(), Fail> {
    if load_batch(ctx, batch_id).await?.is_none() {
        return Ok(());
    }
    let household_id = ctx.household()?.id;
    match items::get(&mut *ctx.conn, household_id, item_id).await {
        Ok(_) => super::open_due(ctx, item_id).await,
        Err(DomainError::Database(error)) => Err(Fail::Infra(error)),
        Err(_) => {
            set_batch_missing(ctx, batch_id, item_id).await?;
            ctx.toast("Событие больше недоступно");
            render_batch(ctx, message_id, batch_id).await
        }
    }
}

/// «Выполнить все» в сводном сообщении.
///
/// Завершает все ещё открытые задачи, включая блоки, уже отложенные «На час»:
/// их snooze-доставка становится устаревшей и отменяется планировщиком. Блоки
/// «Выполнено», «Задача удалена» и «Список в архиве» пропускаются.
pub async fn batch_complete_all(
    ctx: &mut Ctx<'_>,
    message_id: i64,
    batch_id: Uuid,
) -> Result<(), Fail> {
    let Some(batch) = load_batch(ctx, batch_id).await? else {
        return Ok(());
    };
    // Повторное нажатие не пересчитывает итог и не меняет сообщение.
    if batch.result_completed.is_some() {
        return render_batch(ctx, message_id, batch_id).await;
    }

    let rows = notifications::batch_deliveries(&mut *ctx.conn, batch_id).await?;

    let household = ctx.household()?.clone();
    let actor = ctx.actor()?;
    let mut completed = 0;
    let mut changed = 0;
    for (item_id, row) in unique_items(&rows) {
        if !matches!(
            batch_entry(row, ctx.tz, ctx.now).state,
            BatchEntryState::Open | BatchEntryState::Snoozed(_)
        ) {
            continue;
        }
        let release_version = row.batch_version;

        match items::get(&mut *ctx.conn, household.id, item_id).await {
            Err(DomainError::Database(error)) => return Err(Fail::Infra(error)),
            Err(_) => {
                set_batch_missing(ctx, batch_id, item_id).await?;
                changed += 1;
                continue;
            }
            Ok(item) if item.status == ItemStatus::Done => {
                set_batch_done(ctx, batch_id, item_id).await?;
                continue;
            }
            Ok(item) => {
                if release_version.is_some_and(|version| version != item.version) {
                    changed += 1;
                    continue;
                }
                match items::complete(
                    &mut *ctx.conn,
                    household.id,
                    &actor,
                    item_id,
                    release_version,
                    &household.timezone,
                    ctx.now,
                )
                .await
                {
                    Ok(_) => {
                        set_batch_done(ctx, batch_id, item_id).await?;
                        completed += 1;
                    }
                    Err(DomainError::Database(error)) => return Err(Fail::Infra(error)),
                    Err(DomainError::NotFound) => {
                        set_batch_missing(ctx, batch_id, item_id).await?;
                        changed += 1;
                    }
                    Err(_) => changed += 1,
                }
            }
        }
    }

    notifications::set_batch_result(&mut *ctx.conn, batch_id, completed, changed, ctx.now).await?;
    ctx.toast(format!("Выполнено {completed}"));
    render_batch(ctx, message_id, batch_id).await
}

/// «Отложить все на час» в сводном сообщении.
///
/// Берёт только открытые блоки: у блока, уже отложенного «На час», свой snooze
/// сохраняется и второй не создаётся.
pub async fn batch_snooze_all(
    ctx: &mut Ctx<'_>,
    message_id: i64,
    batch_id: Uuid,
) -> Result<(), Fail> {
    if load_batch(ctx, batch_id).await?.is_none() {
        return Ok(());
    }
    let rows = notifications::batch_deliveries(&mut *ctx.conn, batch_id).await?;

    let mut snoozed = 0;
    for (item_id, row) in unique_items(&rows) {
        if batch_entry(row, ctx.tz, ctx.now).state != BatchEntryState::Open {
            continue;
        }
        if snooze_batch_item(ctx, batch_id, item_id, None, true).await? {
            snoozed += 1;
        }
    }

    if snoozed > 0 {
        ctx.toast("Отложено");
    }
    render_batch(ctx, message_id, batch_id).await
}

/// Проверить сообщение и блок перед действием: `false` — сообщение устарело
/// или действие уже невозможно. В последнем случае сообщение перерисовано, а
/// пользователю показана подсказка.
async fn actionable(
    ctx: &mut Ctx<'_>,
    message_id: i64,
    batch_id: Uuid,
    item_id: Uuid,
    event_refusal: &'static str,
) -> Result<bool, Fail> {
    let Some(rows) = batch_rows(ctx, batch_id).await? else {
        return Ok(false);
    };
    let Some(row) = rows.into_iter().find(|row| row.item_id == Some(item_id)) else {
        render_batch(ctx, message_id, batch_id).await?;
        return Ok(false);
    };
    let entry = batch_entry(&row, ctx.tz, ctx.now);
    if let Some(toast) = blocked_toast(entry.state, entry.is_event, event_refusal) {
        ctx.toast(toast);
        render_batch(ctx, message_id, batch_id).await?;
        return Ok(false);
    }
    Ok(true)
}

/// Первые блоки каждой задачи в порядке выпуска.
///
/// Задача может попасть в сводное несколько раз: например, напоминание и
/// назначение на один срок. Общие действия трогают её один раз.
fn unique_items(rows: &[BatchDelivery]) -> Vec<(Uuid, &BatchDelivery)> {
    let mut seen = Vec::new();
    let mut unique = Vec::new();
    for row in rows {
        let Some(item_id) = row.item_id else {
            continue;
        };
        if seen.contains(&item_id) {
            continue;
        }
        seen.push(item_id);
        unique.push((item_id, row));
    }
    unique
}

/// Подсказка для блока, по которому действие уже невозможно.
///
/// `event_refusal` — отказ для устаревшей кнопки на блоке события: событие не
/// выполняется и не откладывается. Удалённое событие тоже называется событием.
fn blocked_toast(
    state: BatchEntryState,
    is_event: bool,
    event_refusal: &'static str,
) -> Option<&'static str> {
    match state {
        BatchEntryState::Open => None,
        BatchEntryState::Event => Some(event_refusal),
        BatchEntryState::Done => Some("Задача уже выполнена"),
        BatchEntryState::Snoozed(_) => Some("Задача уже отложена"),
        BatchEntryState::Missing if is_event => Some("Событие больше недоступно"),
        BatchEntryState::Missing => Some("Задача больше недоступна"),
        BatchEntryState::ListArchived => Some("Список в архиве"),
    }
}

/// Создать snooze-доставку одной задачи сводного сообщения.
///
/// `batched` помечает доставку как групповой snooze: через час несколько таких
/// задач приходят одним сводным сообщением, но ручной выпуск их не ускоряет.
async fn snooze_batch_item(
    ctx: &mut Ctx<'_>,
    batch_id: Uuid,
    item_id: Uuid,
    version: Option<i32>,
    batched: bool,
) -> Result<bool, Fail> {
    let household_id = ctx.household()?.id;
    let item = match items::get(&mut *ctx.conn, household_id, item_id).await {
        Ok(item) => item,
        Err(DomainError::Database(error)) => return Err(Fail::Infra(error)),
        Err(_) => {
            set_batch_missing(ctx, batch_id, item_id).await?;
            ctx.toast("Задача больше недоступна");
            return Ok(false);
        }
    };
    if item.status == ItemStatus::Done {
        set_batch_done(ctx, batch_id, item_id).await?;
        ctx.toast("Задача уже выполнена");
        return Ok(false);
    }
    if version.is_some_and(|version| version != item.version) {
        ctx.toast("Задача уже изменилась");
        return Ok(false);
    }

    let deliver_at = snooze_time(SnoozeUntil::OneHour, ctx.tz, ctx.now)?;
    let origin_key = snooze_origin(item.due_at, item.due_date);
    insert_snooze(ctx, item_id, &origin_key, deliver_at, batched).await?;
    set_batch_snoozed(ctx, batch_id, item_id, deliver_at).await?;
    Ok(true)
}

async fn set_batch_done(ctx: &mut Ctx<'_>, batch_id: Uuid, item_id: Uuid) -> Result<(), Fail> {
    notifications::set_batch_state(
        &mut *ctx.conn,
        batch_id,
        item_id,
        BatchState::Done,
        None,
        ctx.now,
    )
    .await?;
    Ok(())
}

async fn set_batch_missing(ctx: &mut Ctx<'_>, batch_id: Uuid, item_id: Uuid) -> Result<(), Fail> {
    notifications::set_batch_state(
        &mut *ctx.conn,
        batch_id,
        item_id,
        BatchState::Missing,
        None,
        ctx.now,
    )
    .await?;
    Ok(())
}

async fn set_batch_snoozed(
    ctx: &mut Ctx<'_>,
    batch_id: Uuid,
    item_id: Uuid,
    until: DateTime<Utc>,
) -> Result<(), Fail> {
    notifications::set_batch_state(
        &mut *ctx.conn,
        batch_id,
        item_id,
        BatchState::Snoozed,
        Some(until),
        ctx.now,
    )
    .await?;
    Ok(())
}

/// Сводное сообщение получателя или `None`, если оно уже неактуально.
async fn load_batch(
    ctx: &mut Ctx<'_>,
    batch_id: Uuid,
) -> Result<Option<notifications::Batch>, Fail> {
    let user_id = ctx.user()?.id;
    let household_id = ctx.household()?.id;
    let batch = notifications::get_batch(&mut *ctx.conn, batch_id).await?;
    match batch {
        Some(batch) if batch.user_id == user_id && batch.household_id == household_id => {
            Ok(Some(batch))
        }
        _ => {
            ctx.toast("Это сообщение устарело");
            Ok(None)
        }
    }
}

/// Загрузить блоки сводного сообщения или `None`, если оно устарело.
async fn batch_rows(ctx: &mut Ctx<'_>, batch_id: Uuid) -> Result<Option<Vec<BatchDelivery>>, Fail> {
    if load_batch(ctx, batch_id).await?.is_none() {
        return Ok(None);
    }
    Ok(Some(
        notifications::batch_deliveries(&mut *ctx.conn, batch_id).await?,
    ))
}

/// Перерисовать сводное сообщение по актуальному состоянию из БД.
async fn render_batch(ctx: &mut Ctx<'_>, message_id: i64, batch_id: Uuid) -> Result<(), Fail> {
    let Some(batch) = load_batch(ctx, batch_id).await? else {
        return Ok(());
    };
    let rows = notifications::batch_deliveries(&mut *ctx.conn, batch_id).await?;
    let entries: Vec<BatchEntry> = rows
        .iter()
        .map(|row| batch_entry(row, ctx.tz, ctx.now))
        .collect();
    let total = entries.len();
    let stored = batch.page.max(0) as usize;
    let page = stored.min(page_count(total) - 1);
    if page != stored {
        notifications::set_batch_page(
            &mut *ctx.conn,
            batch_id,
            i32::try_from(page).unwrap_or(0),
            ctx.now,
        )
        .await?;
    }
    let start = page * BATCH_PAGE_SIZE;
    let end = (start + BATCH_PAGE_SIZE).min(total);
    let summary = match (batch.result_completed, batch.result_changed) {
        (Some(completed), Some(changed)) => Some((completed, changed)),
        _ => None,
    };
    let actions = BatchActions::of(&entries);
    let ui = batch_ui(
        batch_id,
        page,
        &entries[start..end],
        total,
        summary,
        actions,
        ctx.tz,
    );
    edit_notice(ctx, message_id, &ui).await
}

fn batch_entry(row: &BatchDelivery, tz: Tz, now: DateTime<Utc>) -> BatchEntry {
    let kind = row.kind;
    let missing = row.item_id.is_none()
        || row.item_deleted_at.is_some()
        || row.title.is_none()
        || row.list_name.is_none()
        || row.list_deleted_at.is_some();
    // Архивный список скрывает действия у напоминаний и snooze. Назначение —
    // исключение: оно остаётся актуальным, поэтому блок показывает задачу и
    // работает так же, как одиночное уведомление о назначении.
    let list_archived = !is_assignment(kind) && row.list_archived == Some(true);
    let is_event = row.list_kind == Some(ListKind::Events);
    // Блок без собственного итога или с отменённым snooze показывает текущее
    // состояние задачи: отменённое «Напомню в …» больше ничего не обещает.
    let task_state = || {
        if row.item_status != Some(ItemStatus::Open) {
            BatchEntryState::Done
        } else {
            BatchEntryState::Open
        }
    };
    let state = if missing {
        BatchEntryState::Missing
    } else if list_archived {
        BatchEntryState::ListArchived
    } else if is_event {
        // Событие не выполняется и не откладывается: у блока свои действия.
        BatchEntryState::Event
    } else {
        match row.batch_state {
            Some(BatchState::Done) => BatchEntryState::Done,
            // Записи snooze могло уже не быть (retention удалил `sent` или
            // `cancelled`): это не отмена, поэтому блок остаётся отложенным.
            Some(BatchState::Snoozed) if !row.snooze_cancelled => {
                BatchEntryState::Snoozed(row.batch_snoozed_until.unwrap_or(now))
            }
            Some(BatchState::Snoozed) => task_state(),
            Some(BatchState::Missing) => BatchEntryState::Missing,
            None => task_state(),
        }
    };
    let list = row.list_name.clone().unwrap_or_default();
    let when = when_line(row.due_at, row.due_date, &list, tz, now);
    BatchEntry {
        item_id: row.item_id.unwrap_or_default(),
        kind,
        title: row.title.clone().unwrap_or_default(),
        when,
        version: row.item_version.or(row.batch_version).unwrap_or(0),
        state,
        is_event,
    }
}
