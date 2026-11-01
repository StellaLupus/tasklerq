//! Сводное сообщение после тихих часов: блоки, пагинация и действия.
//!
//! Блок показывает актуальное состояние задачи из БД. Если задачу изменили
//! после выпуска, блок перерисовывается с новым названием, сроком и версией:
//! действие по устаревшей версии ничего не меняет, только обновляет сообщение.

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::models::ItemStatus;
use crate::storage::repositories::items;
use crate::storage::repositories::notifications::{
    self, BatchDelivery, BatchState, NewDelivery, NotificationKind,
};
use crate::telegram::bot::screens::clip;
use crate::telegram::rich::{Span, button};
use crate::telegram::ru;

use super::super::model::{Action, SnoozeUntil, Ui};
use super::super::{Ctx, Fail};
use super::{edit_notice, open, snooze_time, when_line};

/// Сколько блоков сводного сообщения помещается на страницу.
pub const BATCH_PAGE_SIZE: usize = 5;

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
    tz: Tz,
) -> Ui {
    let pages = total.div_ceil(BATCH_PAGE_SIZE).max(1);
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
        match entry.state {
            BatchEntryState::Missing => {
                ui = ui.text("Задача удалена");
                continue;
            }
            BatchEntryState::ListArchived => {
                ui = ui.text("Список в архиве");
                continue;
            }
            BatchEntryState::Done => {
                ui = ui
                    .spans(vec![Span::bold(clip(&entry.title, 120))])
                    .text(&entry.when)
                    .text("✅ Выполнено");
                continue;
            }
            BatchEntryState::Snoozed(until) => {
                ui = ui
                    .spans(vec![Span::bold(clip(&entry.title, 120))])
                    .text(&entry.when)
                    .text(format!(
                        "😴 Напомню в {}",
                        until.with_timezone(&tz).format("%H:%M")
                    ));
                continue;
            }
            BatchEntryState::Open => {}
        }

        ui = ui
            .spans(vec![Span::bold(clip(&entry.title, 120))])
            .text(&entry.when);
        let done = button(
            "✓ Готово",
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
                    },
                ),
                open,
            ]
        };
        ui = ui.row(buttons);
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

    ui.row(vec![
        button("✓ Завершить все", Action::BatchCompleteAll { batch_id }),
        button(
            "😴 Отложить все на час",
            Action::BatchSnoozeAll { batch_id },
        ),
    ])
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
    let pages = rows.len().div_ceil(BATCH_PAGE_SIZE).max(1);
    let page = (page as usize).min(pages - 1);
    notifications::set_batch_page(
        &mut *ctx.conn,
        batch_id,
        i32::try_from(page).unwrap_or(0),
        ctx.now,
    )
    .await?;
    render_batch(ctx, message_id, batch_id).await
}

/// «Готово» в блоке сводного сообщения.
pub async fn batch_complete(
    ctx: &mut Ctx<'_>,
    message_id: i64,
    batch_id: Uuid,
    item_id: Uuid,
    version: i32,
) -> Result<(), Fail> {
    let Some(rows) = batch_rows(ctx, batch_id).await? else {
        return Ok(());
    };
    let Some(row) = rows.iter().find(|row| row.item_id == Some(item_id)) else {
        return render_batch(ctx, message_id, batch_id).await;
    };
    if let Some(toast) = blocked_toast(batch_entry(row, ctx.tz, ctx.now).state) {
        ctx.toast(toast);
        return render_batch(ctx, message_id, batch_id).await;
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
            ctx.toast("Готово");
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
) -> Result<(), Fail> {
    let Some(rows) = batch_rows(ctx, batch_id).await? else {
        return Ok(());
    };
    let Some(row) = rows.iter().find(|row| row.item_id == Some(item_id)) else {
        return render_batch(ctx, message_id, batch_id).await;
    };
    if let Some(toast) = blocked_toast(batch_entry(row, ctx.tz, ctx.now).state) {
        ctx.toast(toast);
        return render_batch(ctx, message_id, batch_id).await;
    }
    if !snooze_batch_item(ctx, batch_id, item_id, false).await? {
        return render_batch(ctx, message_id, batch_id).await;
    }
    ctx.toast("Отложено");
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

/// «Завершить все» в сводном сообщении.
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
    let mut items_to_complete: Vec<(Uuid, Option<i32>)> = Vec::new();
    for row in &rows {
        if let Some(item_id) = row.item_id
            && !items_to_complete.iter().any(|(id, _)| *id == item_id)
        {
            items_to_complete.push((item_id, row.batch_version));
        }
    }

    let household = ctx.household()?.clone();
    let actor = ctx.actor()?;
    let mut completed = 0;
    let mut changed = 0;
    for (item_id, release_version) in items_to_complete {
        let Some(row) = rows.iter().find(|row| row.item_id == Some(item_id)) else {
            continue;
        };
        if !matches!(
            batch_entry(row, ctx.tz, ctx.now).state,
            BatchEntryState::Open | BatchEntryState::Snoozed(_)
        ) {
            continue;
        }

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
pub async fn batch_snooze_all(
    ctx: &mut Ctx<'_>,
    message_id: i64,
    batch_id: Uuid,
) -> Result<(), Fail> {
    if load_batch(ctx, batch_id).await?.is_none() {
        return Ok(());
    }
    let rows = notifications::batch_deliveries(&mut *ctx.conn, batch_id).await?;
    let mut items: Vec<Uuid> = Vec::new();
    for row in &rows {
        if let Some(item_id) = row.item_id
            && !items.contains(&item_id)
        {
            items.push(item_id);
        }
    }

    let mut snoozed = 0;
    for item_id in items {
        let Some(row) = rows.iter().find(|row| row.item_id == Some(item_id)) else {
            continue;
        };
        if !matches!(
            batch_entry(row, ctx.tz, ctx.now).state,
            BatchEntryState::Open
        ) {
            continue;
        }
        if snooze_batch_item(ctx, batch_id, item_id, true).await? {
            snoozed += 1;
        }
    }

    if snoozed > 0 {
        ctx.toast("Отложено");
    }
    render_batch(ctx, message_id, batch_id).await
}

/// Подсказка для блока, по которому действие уже невозможно.
fn blocked_toast(state: BatchEntryState) -> Option<&'static str> {
    match state {
        BatchEntryState::Open => None,
        BatchEntryState::Done => Some("Задача уже выполнена"),
        BatchEntryState::Snoozed(_) => Some("Задача уже отложена"),
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
    batched: bool,
) -> Result<bool, Fail> {
    let household_id = ctx.household()?.id;
    let user_id = ctx.user()?.id;
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

    let deliver_at = snooze_time(SnoozeUntil::OneHour, ctx.tz, ctx.now)?;
    let occurrence_key = notifications::snooze_key(deliver_at);
    let delivery = NewDelivery {
        household_id,
        user_id,
        item_id: Some(item_id),
        kind: NotificationKind::Snooze,
        occurrence_key: &occurrence_key,
        scheduled_at: deliver_at,
    };
    if batched {
        notifications::insert_batched(&mut *ctx.conn, &delivery, ctx.now).await?;
    } else {
        notifications::insert(&mut *ctx.conn, &delivery, ctx.now).await?;
    }
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
    let pages = total.div_ceil(BATCH_PAGE_SIZE).max(1);
    let page = (batch.page.max(0) as usize).min(pages - 1);
    if page != batch.page.max(0) as usize {
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
    let ui = batch_ui(batch_id, page, &entries[start..end], total, summary, ctx.tz);
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
    let state = if missing {
        BatchEntryState::Missing
    } else if list_archived {
        BatchEntryState::ListArchived
    } else {
        match row.batch_state {
            Some(BatchState::Done) => BatchEntryState::Done,
            Some(BatchState::Snoozed) => {
                BatchEntryState::Snoozed(row.batch_snoozed_until.unwrap_or(now))
            }
            Some(BatchState::Missing) => BatchEntryState::Missing,
            None => {
                if row.item_status != Some(ItemStatus::Open) {
                    BatchEntryState::Done
                } else {
                    BatchEntryState::Open
                }
            }
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
    }
}
