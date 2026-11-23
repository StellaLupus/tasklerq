//! Экраны уведомлений и действия «Выполнено» / «Отложить» / «Открыть».
//!
//! Уведомления — отдельные сообщения, а не часть навигационного экрана,
//! поэтому их кнопки выпускаются с областью `notice` и редактируют своё
//! сообщение. Сами уведомления ставит в очередь планировщик (фаза 7): он
//! использует эти конструкторы экранов.

use chrono::{DateTime, Duration, NaiveDate, NaiveTime, Utc};
use chrono_tz::Tz;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::models::{Item, ItemStatus, List, ListKind, User};
use crate::domain::recurrence::resolve_local;
use crate::jobs::keys::snooze_origin;
use crate::storage::repositories::items::completions;
use crate::storage::repositories::notifications::{self, NewDelivery, NotificationKind};
use crate::storage::repositories::{items, lists};
use crate::telegram::callbacks::Scope;
use crate::telegram::outbox::{self, Outgoing};
use crate::telegram::rich::{Span, button};
use crate::telegram::ru;

use super::model::{Action, Screen, SnoozeUntil, Ui};
use super::screens::{back_button, clip};
use super::{Ctx, Fail, issue_and_render};

mod batch;

pub use batch::{
    BATCH_PAGE_SIZE, BatchActions, BatchEntry, BatchEntryState, batch_complete, batch_complete_all,
    batch_move, batch_open, batch_page, batch_snooze, batch_snooze_all, batch_ui,
};

/// Час, на который переносится напоминание «до завтра».
const MORNING: (u32, u32) = (9, 0);

/// Строка «срок · список» для уведомления или блока сводного сообщения.
pub(crate) fn when_line(
    due_at: Option<DateTime<Utc>>,
    due_date: Option<NaiveDate>,
    list_name: &str,
    tz: Tz,
    now: DateTime<Utc>,
) -> String {
    match ru::due_label(due_at, due_date, tz, now) {
        Some(due) if !list_name.is_empty() => {
            format!("{} · {}", ru::capitalize(&due), list_name)
        }
        Some(due) => ru::capitalize(&due),
        None => list_name.to_owned(),
    }
}

/// Название задачи в уведомлении или блоке сводного сообщения: жирным и
/// обрезанным до 120 символов.
pub(super) fn title_spans(title: &str) -> Vec<Span> {
    vec![Span::bold(clip(title, 120))]
}

/// Момент события строкой напоминания: «Сегодня, 14:00»; без значения —
/// прочерк.
fn event_moment(
    at: Option<DateTime<Utc>>,
    date: Option<NaiveDate>,
    tz: Tz,
    now: DateTime<Utc>,
) -> String {
    ru::due_label(at, date, tz, now).map_or_else(|| "—".to_owned(), |label| ru::capitalize(&label))
}

/// Напоминание о сроке.
pub fn reminder_ui(
    item: &Item,
    list: &List,
    tz: Tz,
    now: DateTime<Utc>,
    lead_minutes: Option<i64>,
) -> Ui {
    let heading = match lead_minutes {
        Some(minutes) if minutes > 0 => format!("⏰ Через {}", ru::minutes(minutes)),
        _ => "⏰ Пора".to_owned(),
    };
    if list.kind == ListKind::Events {
        // Событие не выполняется и не откладывается: у напоминания свои
        // действия — убрать, перенести начало или открыть карточку.
        return Ui::new()
            .heading(heading)
            .spans(title_spans(&item.title))
            .text(format!(
                "📅 Начало: {}",
                event_moment(item.due_at, item.due_date, tz, now)
            ))
            .text(format!(
                "🏁 Конец: {}",
                event_moment(item.end_at, item.end_date, tz, now)
            ))
            .row(vec![
                button(ru::DISMISS_BUTTON, Action::NoticeClose),
                button("📅 Перенести", Action::NoticeMove { item_id: item.id }),
            ])
            .row(vec![button(
                "📋 Открыть",
                Action::NoticeOpen { item_id: item.id },
            )]);
    }
    Ui::new()
        .heading(heading)
        .spans(title_spans(&item.title))
        .text(when_line(item.due_at, item.due_date, &list.name, tz, now))
        .row(vec![
            button(
                ru::DONE_BUTTON,
                Action::NoticeComplete {
                    item_id: item.id,
                    version: item.version,
                },
            ),
            button(
                "😴 Отложить на час",
                Action::Snooze {
                    item_id: item.id,
                    until: SnoozeUntil::OneHour,
                    version: Some(item.version),
                },
            ),
        ])
        .row(vec![
            button(
                "📅 Завтра в 09:00",
                Action::Snooze {
                    item_id: item.id,
                    until: SnoozeUntil::TomorrowMorning,
                    version: Some(item.version),
                },
            ),
            button("📋 Открыть", Action::NoticeOpen { item_id: item.id }),
        ])
        .row(vec![button(ru::DISMISS_BUTTON, Action::NoticeClose)])
}

/// Уведомление о назначении.
pub fn assignment_ui(item: &Item, list: &List, tz: Tz, now: DateTime<Utc>) -> Ui {
    Ui::new()
        .heading("👤 Вам назначили задачу")
        .spans(title_spans(&item.title))
        .text(when_line(item.due_at, item.due_date, &list.name, tz, now))
        .row(vec![
            button("📋 Открыть", Action::NoticeOpen { item_id: item.id }),
            button(
                ru::DONE_BUTTON,
                Action::NoticeComplete {
                    item_id: item.id,
                    version: item.version,
                },
            ),
        ])
        .row(vec![button(ru::DISMISS_BUTTON, Action::NoticeClose)])
}

/// Строка утренней сводки.
pub struct DigestLine {
    /// Название.
    pub title: String,
    /// Срок в человекочитаемом виде.
    pub when: String,
    /// Строка события, а не задачи: у неё своя отметка.
    pub is_event: bool,
}

/// Утренняя сводка.
pub fn digest_ui(
    today: &[DigestLine],
    overdue: &[DigestLine],
    shopping_open: i64,
    tasks_list: Option<Uuid>,
    shopping_list: Option<Uuid>,
) -> Ui {
    const MAX_LINES: usize = 20;
    let total_lines = today.len() + overdue.len();
    let mut hidden_event = false;
    let mut ui = Ui::new().heading("☀️ Доброе утро!");
    let line = |entry: &DigestLine| {
        let mark = if entry.is_event { "🗓" } else { "☐" };
        format!("{mark} {} · {}", clip(&entry.title, 60), entry.when)
    };
    let mut budget = MAX_LINES;
    for (title, entries) in [("Сегодня", today), ("Просрочено", overdue)] {
        if entries.is_empty() {
            continue;
        }
        let shown = entries.len().min(budget);
        let mut lines: Vec<String> = entries[..shown].iter().map(line).collect();
        if shown < entries.len() {
            let hidden = &entries[shown..];
            let rest = i64::try_from(hidden.len()).unwrap_or(0);
            let has_event = hidden.iter().any(|entry| entry.is_event);
            hidden_event |= has_event;
            // Смешанный хвост не называем задачами: в нём могут быть события.
            lines.push(if has_event {
                format!("…и ещё {rest}")
            } else {
                format!("…и ещё {}", ru::tasks(rest))
            });
        }
        budget -= shown;
        ui = ui.bold(title).text(lines.join("\n"));
    }
    if today.is_empty() && overdue.is_empty() {
        ui = if shopping_open == 0 {
            ui.text("День свободен.")
        } else {
            ui.text("На сегодня задач нет.")
        };
    }
    if shopping_open > 0 {
        ui = ui.text(format!("Покупки: {}", ru::positions(shopping_open)));
    }
    let mut row = Vec::new();
    if let Some(list_id) = tasks_list {
        row.push(back_button(
            "📋 Задачи",
            Screen::List {
                list_id,
                page: 0,
                edit: false,
                from_lists: false,
            },
        ));
    }
    if let Some(list_id) = shopping_list {
        row.push(back_button(
            "🛒 Покупки",
            Screen::List {
                list_id,
                page: 0,
                edit: false,
                from_lists: false,
            },
        ));
    }
    if total_lines > MAX_LINES {
        // 0009-events 8.2: скрытые события открываются в повестке, а скрытые
        // задачи — в списке задач. Если хвост смешанный, важнее события: в
        // списке задач их не видно.
        let more = if hidden_event || tasks_list.is_none() {
            Some(Screen::Agenda {
                list: None,
                page: 0,
            })
        } else {
            tasks_list.map(|list_id| Screen::List {
                list_id,
                page: 0,
                edit: false,
                from_lists: false,
            })
        };
        if let Some(target) = more {
            row.push(back_button("Показать ещё", target));
        }
    }
    ui.row(row)
        .row(vec![button(ru::DISMISS_BUTTON, Action::NoticeClose)])
}

/// Экран после действия над уведомлением.
pub fn done_ui(item: &Item, list_id: Uuid, next: Option<&str>) -> Ui {
    let mut ui = Ui::new().heading(format!("✅ Выполнено: {}", clip(&item.title, 100)));
    if let Some(next) = next {
        ui = ui.text(format!("Следующее: {next}"));
    }
    ui.row(vec![
        button("↩️ Отменить", Action::NoticeUndo { item_id: item.id }),
        back_button(
            ru::OPEN_LIST_BUTTON,
            Screen::List {
                list_id,
                page: 0,
                edit: false,
                from_lists: false,
            },
        ),
    ])
}

/// Экран «Задача больше недоступна».
fn gone_ui() -> Ui {
    Ui::new().heading("Задача больше недоступна").row(vec![
        back_button(ru::MENU_BUTTON, Screen::Menu),
        button(ru::DISMISS_BUTTON, Action::NoticeClose),
    ])
}

/// Имя участника для закрытого уведомления (0018, решение 5).
///
/// `member` — сведения об участнике, загруженные из household получателя
/// (`users::get(household получателя, …)` возвращает только активного
/// участника этого household): вызывающий обязан передать именно такого
/// участника, как в журнале выполнений. Имя не показывается, если выполнивший —
/// сам получатель, если выполнение без участника (`actor_user_id` пуст: HA,
/// интеграции) или если участник исключён из household / удалён (`member` пуст
/// или помечен удалённым). Формат без рода и склонения — отображаемое имя.
/// Тем же правилом пользуется имя исполнителя в «👤 Назначено»: если
/// исполнителя нельзя показать (исключён/удалён), имя не выводится.
pub(crate) fn member_name(
    actor_user_id: Option<Uuid>,
    recipient_user_id: Uuid,
    member: Option<&User>,
) -> Option<String> {
    let actor = actor_user_id?;
    if actor == recipient_user_id {
        return None;
    }
    let member = member?;
    if member.deleted_at.is_some() {
        return None;
    }
    Some(member.display_name.clone())
}

/// Экран «✅ Выполнено: <название>[ · <имя>]» — уведомление, закрытое
/// выполнением задачи не из него (0018, решение 4). Единственная кнопка —
/// «🗑 Убрать»: отменить выполнение из такого сообщения нельзя. Название
/// обрезается, как в [`done_ui`]. Вызывается шагом синхронизации.
pub(crate) fn completed_ui(title: &str, actor_name: Option<&str>) -> Ui {
    let mut heading = format!("✅ Выполнено: {}", clip(title, 100));
    if let Some(name) = actor_name {
        heading.push_str(&format!(" · {name}"));
    }
    Ui::new()
        .heading(heading)
        .row(vec![button(ru::DISMISS_BUTTON, Action::NoticeClose)])
}

/// Экран «👤 Назначено: <название>[ · <имя исполнителя>]» — уведомление,
/// закрытое назначением задачи другому участнику (0018, решение 4).
/// Единственная кнопка — «🗑 Убрать». Если имя исполнителя показать нельзя
/// (исключён/удалён), заголовок без имени. Название обрезается, как в
/// [`done_ui`]. Вызывается шагом синхронизации.
pub(crate) fn assigned_ui(title: &str, assignee_name: Option<&str>) -> Ui {
    let mut heading = format!("👤 Назначено: {}", clip(title, 100));
    if let Some(name) = assignee_name {
        heading.push_str(&format!(" · {name}"));
    }
    Ui::new()
        .heading(heading)
        .row(vec![button(ru::DISMISS_BUTTON, Action::NoticeClose)])
}

/// Собрать HTML уведомления для получателя: выпускает кнопки с областью
/// `notice`. `issued_at` — момент доставки, от него отсчитывается срок
/// действия кнопок.
pub async fn render_notice(
    conn: &mut PgConnection,
    telegram_user_id: i64,
    household_id: Uuid,
    ui: &Ui,
    issued_at: DateTime<Utc>,
) -> Result<String, sqlx::Error> {
    issue_and_render(
        conn,
        telegram_user_id,
        Some(household_id),
        ui,
        Scope::Notice,
        issued_at,
    )
    .await
}

async fn load(ctx: &mut Ctx<'_>, item_id: Uuid) -> Result<Option<(Item, List)>, Fail> {
    let household_id = ctx.household()?.id;
    match items::get(&mut *ctx.conn, household_id, item_id).await {
        Ok(item) => {
            let list = lists::get(&mut *ctx.conn, household_id, item.list_id).await?;
            Ok(Some((item, list)))
        }
        Err(DomainError::NotFound) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Заменить содержимое сообщения-уведомления.
pub(super) async fn edit_notice(ctx: &mut Ctx<'_>, message_id: i64, ui: &Ui) -> Result<(), Fail> {
    let household_id = ctx.household()?.id;
    let html = render_notice(&mut *ctx.conn, ctx.sender.id, household_id, ui, ctx.now).await?;
    outbox::enqueue(
        &mut *ctx.conn,
        ctx.chat_id,
        &Outgoing::Edit {
            message_id,
            html,
            resend_if_gone: true,
        },
        None,
        ctx.now,
    )
    .await?;
    Ok(())
}

/// Показать «✅ Выполнено» в сообщении-уведомлении и запланировать его
/// автоудаление (0017, решение 4).
///
/// Сообщение удаляется само через [`items::UNDO_WINDOW_MINUTES`] после того,
/// как стало «Выполнено»: отложенное `cleanup`-удаление с ключом
/// [`outbox::notice_expire_key`]. Прежние запланированные удаления этого
/// сообщения не отменяются — повторное «Выполнено» после отмены ставит новое
/// удаление, а удаление уже удалённого сообщения считается доставленным
/// (`MessageGone`). «↩️ Отменить» отменяет удаление через
/// [`outbox::cancel_scheduled_deletes`].
async fn show_done(
    ctx: &mut Ctx<'_>,
    message_id: i64,
    item: &Item,
    list_id: Uuid,
    next: Option<&str>,
) -> Result<(), Fail> {
    let ui = done_ui(item, list_id, next);
    edit_notice(ctx, message_id, &ui).await?;
    outbox::enqueue_at(
        &mut *ctx.conn,
        ctx.chat_id,
        &Outgoing::Delete {
            message_id,
            cleanup: true,
        },
        Some(&outbox::notice_expire_key(ctx.chat_id, message_id, ctx.now)),
        ctx.now,
        ctx.now + Duration::minutes(items::UNDO_WINDOW_MINUTES),
    )
    .await?;
    Ok(())
}

/// Показать актуальное состояние задачи в сообщении-уведомлении.
async fn show_current(ctx: &mut Ctx<'_>, message_id: i64, item_id: Uuid) -> Result<(), Fail> {
    match load(ctx, item_id).await? {
        Some((item, list)) if item.status == ItemStatus::Open => {
            let ui = reminder_ui(&item, &list, ctx.tz, ctx.now, None);
            edit_notice(ctx, message_id, &ui).await
        }
        Some((item, list)) => show_done(ctx, message_id, &item, list.id, None).await,
        None => edit_notice(ctx, message_id, &gone_ui()).await,
    }
}

/// Отметить доставки нажатого сообщения закрытыми по последнему действующему
/// выполнению задачи (0018, решение 4). Если выполнения нет (не должно
/// случиться), отметка не пишется.
async fn mark_closed_by_completion(
    ctx: &mut Ctx<'_>,
    household_id: Uuid,
    item_id: Uuid,
    message_id: i64,
) -> Result<(), Fail> {
    let Some(event) = completions::latest_for_item(&mut *ctx.conn, household_id, item_id).await?
    else {
        return Ok(());
    };
    notifications::mark_closed_completed(
        &mut *ctx.conn,
        ctx.chat_id,
        message_id,
        event.completion.event_id,
        ctx.now,
    )
    .await?;
    Ok(())
}

/// «Выполнено» из уведомления.
///
/// Доставки нажатого сообщения блокируются до выполнения и в той же
/// транзакции отмечаются закрытыми по выполнению (0018, решение 4): шаг
/// синхронизации не трогает сообщение, которое уже показывает «✅ Выполнено»
/// (`show_done`).
pub async fn complete(
    ctx: &mut Ctx<'_>,
    message_id: i64,
    item_id: Uuid,
    version: i32,
) -> Result<(), Fail> {
    let household = ctx.household()?.clone();
    let actor = ctx.actor()?;
    notifications::lock_message_deliveries(&mut *ctx.conn, ctx.chat_id, message_id).await?;
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
        Ok(item) => {
            let next = if item.status == ItemStatus::Open {
                ru::due_label(item.due_at, item.due_date, ctx.tz, ctx.now)
            } else {
                None
            };
            mark_closed_by_completion(ctx, household.id, item_id, message_id).await?;
            ctx.toast(ru::DONE_TOAST);
            show_done(ctx, message_id, &item, item.list_id, next.as_deref()).await
        }
        Err(DomainError::Conflict { .. } | DomainError::NotFound) => {
            // Задачу успели изменить: кнопки обновляются, действие не повторяется.
            let loaded = load(ctx, item_id).await?;
            let toast = match &loaded {
                None => "Задача больше недоступна",
                Some((item, _)) if item.status == ItemStatus::Done => "Задача уже выполнена",
                Some(_) => "Задача уже изменилась",
            };
            if matches!(&loaded, Some((item, _)) if item.status == ItemStatus::Done) {
                // Сообщение покажет «✅ Выполнено» — закрываем его и здесь,
                // иначе шаг синхронизации перерисует его второй раз.
                mark_closed_by_completion(ctx, household.id, item_id, message_id).await?;
            }
            ctx.toast(toast);
            show_current(ctx, message_id, item_id).await
        }
        Err(error) => Err(error.into()),
    }
}

/// Отменить завершение из уведомления.
///
/// При успешной отмене сообщение снова показывает напоминание, поэтому
/// запланированное автоудаление «✅ Выполнено» отменяется (0017, решение 4):
/// будущие `pending`-удаления этого сообщения переходят в `skipped`. Неудачная
/// отмена («Время отмены истекло») удаление не отменяет. Отметка закрытия
/// доставок этого сообщения снимается только при успешной отмене (0018,
/// решение 4): шаг синхронизации снова может закрыть возвращённое напоминание.
pub async fn undo(ctx: &mut Ctx<'_>, message_id: i64, item_id: Uuid) -> Result<(), Fail> {
    let household_id = ctx.household()?.id;
    let actor = ctx.actor()?;
    notifications::lock_message_deliveries(&mut *ctx.conn, ctx.chat_id, message_id).await?;
    match items::undo_last(&mut *ctx.conn, household_id, &actor, item_id, ctx.now).await {
        Ok(_) => {
            notifications::clear_closed(&mut *ctx.conn, ctx.chat_id, message_id).await?;
            ctx.toast("Отменено");
            outbox::cancel_scheduled_deletes(&mut *ctx.conn, ctx.chat_id, message_id, ctx.now)
                .await?;
        }
        Err(DomainError::Database(error)) => return Err(Fail::Infra(error)),
        Err(_) => ctx.toast("Время отмены истекло"),
    }
    show_current(ctx, message_id, item_id).await
}

/// «Открыть» из уведомления: карточка задачи в экране навигации.
pub(super) async fn open(ctx: &mut Ctx<'_>, message_id: i64, item_id: Uuid) -> Result<(), Fail> {
    if load(ctx, item_id).await?.is_none() {
        ctx.toast("Задача больше недоступна");
        return show_current(ctx, message_id, item_id).await;
    }
    ctx.present(Screen::Item {
        item_id,
        back: Box::new(Screen::Menu),
        completion_id: None,
    })
    .await
}

/// «🗑 Убрать» из любого уведомления: удалить сообщение из чата.
///
/// Удаление — уборка (`cleanup = true`): отказ Telegram не засыпает вид чата.
/// В той же транзакции записи доставки этого сообщения отмечаются
/// `dismissed_at`: их статус остаётся `sent`, поэтому напоминание о том же
/// вхождении повторно не планируется, а правки отправленных сообщений
/// (0018, 0019) убранное не трогают. Задачу, событие и отложенные
/// snooze-доставки уборка не меняет; о следующем вхождении напоминание
/// придёт по расписанию.
pub async fn close(ctx: &mut Ctx<'_>, message_id: i64) -> Result<(), Fail> {
    outbox::enqueue(
        &mut *ctx.conn,
        ctx.chat_id,
        &Outgoing::Delete {
            message_id,
            cleanup: true,
        },
        Some(&super::delete_dedupe_key(ctx.chat_id, message_id)),
        ctx.now,
    )
    .await?;
    notifications::mark_dismissed(&mut *ctx.conn, ctx.chat_id, message_id, ctx.now).await?;
    Ok(())
}

/// Открыть экран быстрого срока события из уведомления или сводного сообщения.
pub(super) async fn open_due(ctx: &mut Ctx<'_>, item_id: Uuid) -> Result<(), Fail> {
    ctx.present(Screen::Due {
        item_id,
        back: Box::new(Screen::Menu),
    })
    .await
}

/// «📅 Перенести» из напоминания о событии.
pub async fn event_due(ctx: &mut Ctx<'_>, item_id: Uuid) -> Result<(), Fail> {
    if load(ctx, item_id).await?.is_none() {
        ctx.toast("Событие больше недоступно");
        return Ok(());
    }
    open_due(ctx, item_id).await
}

pub(super) fn snooze_time(
    until: SnoozeUntil,
    tz: Tz,
    now: DateTime<Utc>,
) -> Result<DateTime<Utc>, DomainError> {
    let at = match until {
        SnoozeUntil::OneHour => now + Duration::hours(1),
        SnoozeUntil::TomorrowMorning => {
            let tomorrow = now.with_timezone(&tz).date_naive() + Duration::days(1);
            let morning =
                NaiveTime::from_hms_opt(MORNING.0, MORNING.1, 0).unwrap_or(NaiveTime::MIN);
            resolve_local(tz, tomorrow, morning)?
        }
    };
    // Ключ snooze хранит время с точностью до секунды; усечём и `scheduled_at`,
    // чтобы первому тику не пришлось перезаписывать его из-за долей секунды.
    Ok(DateTime::<Utc>::from_timestamp(at.timestamp(), 0).unwrap_or(at))
}

/// Поставить snooze-доставку задачи на `deliver_at`.
///
/// `batched` — групповой snooze: несколько таких задач приходят одним сводным
/// сообщением. `replaces_message_id` — сообщение «😴 Напомню …», которое
/// заменит эта доставка; у snooze из сводного сообщения пусто. Ключ включает
/// время snooze, поэтому повторное нажатие той же кнопки не создаёт вторую
/// доставку, а планировщик не путает её с вхождением срока.
///
/// Затёртый другим значением прежний id удаляется сразу: эта доставка его
/// уже не заменит (design 0017, решение 5).
pub(super) async fn insert_snooze(
    ctx: &mut Ctx<'_>,
    item_id: Uuid,
    origin_key: &str,
    deliver_at: DateTime<Utc>,
    batched: bool,
    replaces_message_id: Option<i64>,
) -> Result<(), Fail> {
    let occurrence_key = notifications::snooze_key(deliver_at);
    let delivery = NewDelivery {
        household_id: ctx.household()?.id,
        user_id: ctx.user()?.id,
        item_id: Some(item_id),
        kind: NotificationKind::Snooze,
        occurrence_key: &occurrence_key,
        origin_key: Some(origin_key),
        replaces_message_id,
        scheduled_at: deliver_at,
    };
    let overwritten =
        notifications::insert_snooze(&mut *ctx.conn, &delivery, batched, ctx.now).await?;
    if let Some(old) = overwritten {
        outbox::enqueue(
            &mut *ctx.conn,
            ctx.chat_id,
            &Outgoing::Delete {
                message_id: old,
                cleanup: true,
            },
            Some(&outbox::replace_key(ctx.chat_id, old)),
            ctx.now,
        )
        .await?;
        notifications::mark_dismissed(&mut *ctx.conn, ctx.chat_id, old, ctx.now).await?;
    }
    Ok(())
}

/// Экран «😴 Напомню …» после откладывания напоминания.
fn snoozed_ui(heading: String, title: &str, item_id: Uuid) -> Ui {
    Ui::new().heading(heading).text(clip(title, 120)).row(vec![
        button("📋 Открыть", Action::NoticeOpen { item_id }),
        button(ru::DISMISS_BUTTON, Action::NoticeClose),
    ])
}

/// «Отложить»: создаёт отдельную доставку, которую отправит планировщик.
pub async fn snooze(
    ctx: &mut Ctx<'_>,
    message_id: i64,
    item_id: Uuid,
    until: SnoozeUntil,
    version: Option<i32>,
) -> Result<(), Fail> {
    let Some((item, _)) = load(ctx, item_id).await? else {
        ctx.toast("Задача больше недоступна");
        return show_current(ctx, message_id, item_id).await;
    };
    if item.status == ItemStatus::Done {
        ctx.toast("Задача уже выполнена");
        return show_current(ctx, message_id, item_id).await;
    }
    if version.is_some_and(|version| version != item.version) {
        ctx.toast("Задача уже изменилась");
        return show_current(ctx, message_id, item_id).await;
    }

    let deliver_at = snooze_time(until, ctx.tz, ctx.now)?;
    let origin_key = snooze_origin(item.due_at, item.due_date);
    insert_snooze(
        ctx,
        item_id,
        &origin_key,
        deliver_at,
        false,
        Some(message_id),
    )
    .await?;

    let local = deliver_at.with_timezone(&ctx.tz);
    ctx.toast("Отложено");
    let text = format!(
        "😴 Напомню {}",
        ru::due_label(Some(deliver_at), None, ctx.tz, ctx.now)
            .unwrap_or_else(|| local.format("%H:%M").to_string())
    );
    let ui = snoozed_ui(text, &item.title, item_id);
    edit_notice(ctx, message_id, &ui).await
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;
    use crate::domain::models::{ItemPriority, ItemSource, User, UserRole};
    use crate::telegram::rich::{Block, Button};

    #[test]
    fn snooze_options_resolve_in_local_time() {
        let tz: Tz = "Europe/Moscow".parse().expect("tz");
        let now = Utc.with_ymd_and_hms(2026, 10, 30, 20, 30, 0).unwrap();

        assert_eq!(
            snooze_time(SnoozeUntil::OneHour, tz, now).expect("time"),
            Utc.with_ymd_and_hms(2026, 10, 30, 21, 30, 0).unwrap()
        );
        // 20:30 UTC — 23:30 30 октября в Москве, «завтра» — 31 октября, 09:00.
        assert_eq!(
            snooze_time(SnoozeUntil::TomorrowMorning, tz, now).expect("time"),
            Utc.with_ymd_and_hms(2026, 10, 31, 6, 0, 0).unwrap()
        );
        // 22:30 UTC — уже 01:30 31 октября в Москве, «завтра» — 1 ноября.
        let late = Utc.with_ymd_and_hms(2026, 10, 30, 22, 30, 0).unwrap();
        assert_eq!(
            snooze_time(SnoozeUntil::TomorrowMorning, tz, late).expect("time"),
            Utc.with_ymd_and_hms(2026, 11, 1, 6, 0, 0).unwrap()
        );
    }

    #[test]
    fn snooze_time_is_truncated_to_whole_seconds() {
        let tz: Tz = "Europe/Moscow".parse().expect("tz");
        let now =
            Utc.with_ymd_and_hms(2026, 10, 30, 20, 30, 0).unwrap() + Duration::milliseconds(750);

        let at = snooze_time(SnoozeUntil::OneHour, tz, now).expect("time");
        assert_eq!(at.timestamp_subsec_nanos(), 0);
        assert_eq!(at, Utc.with_ymd_and_hms(2026, 10, 30, 21, 30, 0).unwrap());
    }

    #[test]
    fn digest_is_limited_to_twenty_lines() {
        let lines: Vec<DigestLine> = (0..25)
            .map(|n| DigestLine {
                title: format!("Задача {n}"),
                when: "сегодня".into(),
                is_event: false,
            })
            .collect();
        let ui = digest_ui(&lines, &[], 0, Some(Uuid::now_v7()), None);
        assert_eq!(ui.button_count(), 3);
        let tokens = vec!["t:AAAAAAAAAAAAAAAA".to_owned(); 3];
        let html = crate::telegram::rich::render(&ui, &tokens).html;

        assert_eq!(html.matches("☐ Задача").count(), 20);
        assert!(html.contains("…и ещё 5 задач"));
        assert!(html.contains("Показать ещё"));
    }

    #[test]
    fn digest_limit_counts_events_and_tasks_together() {
        let mut lines: Vec<DigestLine> = (0..15)
            .map(|n| DigestLine {
                title: format!("Задача {n}"),
                when: "сегодня".into(),
                is_event: false,
            })
            .collect();
        lines.extend((0..10).map(|n| DigestLine {
            title: format!("Событие {n}"),
            when: "14:00–15:00".into(),
            is_event: true,
        }));
        let ui = digest_ui(&lines, &[], 0, Some(Uuid::now_v7()), None);
        let tokens = vec!["t:AAAAAAAAAAAAAAAA".to_owned(); 3];
        let html = crate::telegram::rich::render(&ui, &tokens).html;

        assert_eq!(html.matches("☐ Задача").count(), 15);
        assert_eq!(html.matches("🗓 Событие").count(), 5);
        assert!(html.contains("…и ещё 5"), "{html}");
        assert!(!html.contains("…и ещё 5 задач"), "{html}");
        assert!(html.contains("Показать ещё"), "{html}");
    }

    #[test]
    fn digest_show_more_opens_the_agenda_when_events_are_hidden() {
        let mut lines: Vec<DigestLine> = (0..15)
            .map(|n| DigestLine {
                title: format!("Задача {n}"),
                when: "сегодня".into(),
                is_event: false,
            })
            .collect();
        lines.extend((0..10).map(|n| DigestLine {
            title: format!("Событие {n}"),
            when: "14:00–15:00".into(),
            is_event: true,
        }));
        let ui = digest_ui(&lines, &[], 0, Some(Uuid::now_v7()), None);
        let more = ui
            .blocks
            .iter()
            .find_map(|block| match block {
                crate::telegram::rich::Block::Buttons(buttons) => {
                    buttons.iter().find(|button| button.label == "Показать ещё")
                }
                _ => None,
            })
            .expect("«Показать ещё» with a mixed tail");
        assert!(matches!(
            more.action,
            Action::Go {
                to: Screen::Agenda {
                    list: None,
                    page: 0
                }
            }
        ));
    }

    #[test]
    fn digest_show_more_keeps_the_task_list_when_only_tasks_are_hidden() {
        let lines: Vec<DigestLine> = (0..25)
            .map(|n| DigestLine {
                title: format!("Задача {n}"),
                when: "сегодня".into(),
                is_event: false,
            })
            .collect();
        let list_id = Uuid::now_v7();
        let ui = digest_ui(&lines, &[], 0, Some(list_id), None);
        let more = ui
            .blocks
            .iter()
            .find_map(|block| match block {
                crate::telegram::rich::Block::Buttons(buttons) => {
                    buttons.iter().find(|button| button.label == "Показать ещё")
                }
                _ => None,
            })
            .expect("«Показать ещё» with hidden tasks");
        assert!(matches!(
            more.action,
            Action::Go {
                to: Screen::List { list_id: id, .. }
            } if id == list_id
        ));
    }

    #[test]
    fn empty_digest_says_the_day_is_free() {
        let ui = digest_ui(&[], &[], 0, None, None);
        let tokens = vec!["t:AAAAAAAAAAAAAAAA".to_owned()];
        let html = crate::telegram::rich::render(&ui, &tokens).html;
        assert!(html.contains("День свободен."));
        assert!(!html.contains("Сегодня"));
        assert!(!html.contains("Просрочено"));
        assert!(!html.contains("Покупки"));
    }

    #[test]
    fn digest_with_only_shopping_keeps_the_purchase_count() {
        let ui = digest_ui(&[], &[], 7, None, None);
        let tokens = vec!["t:AAAAAAAAAAAAAAAA".to_owned()];
        let html = crate::telegram::rich::render(&ui, &tokens).html;
        assert!(html.contains("Покупки: 7 позиций"));
    }

    fn notice_item(title: &str) -> Item {
        Item {
            id: Uuid::now_v7(),
            household_id: Uuid::nil(),
            list_id: Uuid::nil(),
            title: title.into(),
            notes: None,
            status: ItemStatus::Open,
            priority: ItemPriority::Normal,
            due_at: Some(Utc.with_ymd_and_hms(2026, 11, 23, 12, 0, 0).unwrap()),
            due_date: None,
            end_at: None,
            end_date: None,
            assignee_user_id: None,
            created_by_user_id: None,
            source: ItemSource::Telegram,
            capture_id: None,
            completed_at: None,
            completed_by_user_id: None,
            quantity: None,
            unit: None,
            position: 0,
            version: 1,
            recurrence: None,
            created_at: DateTime::UNIX_EPOCH,
            updated_at: DateTime::UNIX_EPOCH,
            deleted_at: None,
            tags: Vec::new(),
            subtasks: Vec::new(),
        }
    }

    fn notice_list(kind: ListKind, name: &str) -> List {
        List {
            id: Uuid::now_v7(),
            household_id: Uuid::nil(),
            kind,
            name: name.into(),
            is_default: true,
            is_archived: false,
            position: 0,
            created_at: DateTime::UNIX_EPOCH,
            updated_at: DateTime::UNIX_EPOCH,
            deleted_at: None,
        }
    }

    fn moscow_now() -> (Tz, DateTime<Utc>) {
        (
            "Europe/Moscow".parse().expect("tz"),
            Utc.with_ymd_and_hms(2026, 11, 23, 9, 0, 0).unwrap(),
        )
    }

    fn button_rows(ui: &Ui) -> Vec<&[Button<Action>]> {
        ui.blocks
            .iter()
            .filter_map(|block| match block {
                Block::Buttons(buttons) => Some(buttons.as_slice()),
                _ => None,
            })
            .collect()
    }

    fn row_labels(row: &[Button<Action>]) -> Vec<&str> {
        row.iter().map(|button| button.label.as_str()).collect()
    }

    #[test]
    fn task_reminder_has_dismiss_button_on_the_third_row() {
        let (tz, now) = moscow_now();
        let item = notice_item("Оплатить интернет");
        let list = notice_list(ListKind::Tasks, "Задачи");
        let ui = reminder_ui(&item, &list, tz, now, Some(15));

        let rows = button_rows(&ui);
        assert_eq!(rows.len(), 3, "{rows:?}");
        assert_eq!(row_labels(rows[0]), ["✓ Выполнено", "😴 Отложить на час"]);
        assert_eq!(row_labels(rows[1]), ["📅 Завтра в 09:00", "📋 Открыть"]);
        assert_eq!(row_labels(rows[2]), ["🗑 Убрать"]);
        assert!(matches!(rows[2][0].action, Action::NoticeClose));
    }

    #[test]
    fn event_reminder_replaces_close_with_dismiss() {
        let (tz, now) = moscow_now();
        let mut item = notice_item("Маникюр");
        item.end_at = Some(Utc.with_ymd_and_hms(2026, 11, 23, 13, 30, 0).unwrap());
        let list = notice_list(ListKind::Events, "События");
        let ui = reminder_ui(&item, &list, tz, now, Some(15));

        let rows = button_rows(&ui);
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert_eq!(row_labels(rows[0]), ["🗑 Убрать", "📅 Перенести"]);
        assert_eq!(row_labels(rows[1]), ["📋 Открыть"]);
        assert!(matches!(rows[0][0].action, Action::NoticeClose));
        assert!(
            !rows
                .iter()
                .flat_map(|row| row_labels(row))
                .any(|label| label.contains("Закрыть")),
            "{rows:?}"
        );
    }

    #[test]
    fn assignment_notice_has_dismiss_button_on_a_separate_row() {
        let (tz, now) = moscow_now();
        let item = notice_item("Купить торт");
        let list = notice_list(ListKind::Tasks, "Задачи");
        let ui = assignment_ui(&item, &list, tz, now);

        let rows = button_rows(&ui);
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert_eq!(row_labels(rows[0]), ["📋 Открыть", "✓ Выполнено"]);
        assert_eq!(row_labels(rows[1]), ["🗑 Убрать"]);
        assert!(matches!(rows[1][0].action, Action::NoticeClose));
    }

    #[test]
    fn digest_has_dismiss_button_on_the_last_row() {
        let lines: Vec<DigestLine> = (0..25)
            .map(|n| DigestLine {
                title: format!("Задача {n}"),
                when: "сегодня".into(),
                is_event: false,
            })
            .collect();
        let ui = digest_ui(&lines, &[], 3, Some(Uuid::now_v7()), Some(Uuid::now_v7()));

        let rows = button_rows(&ui);
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert_eq!(
            row_labels(rows[0]),
            ["📋 Задачи", "🛒 Покупки", "Показать ещё"]
        );
        assert_eq!(row_labels(rows[1]), ["🗑 Убрать"]);
        assert!(matches!(rows[1][0].action, Action::NoticeClose));
    }

    #[test]
    fn empty_digest_has_dismiss_button() {
        let ui = digest_ui(&[], &[], 0, None, None);

        let rows = button_rows(&ui);
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(row_labels(rows[0]), ["🗑 Убрать"]);
        assert!(matches!(rows[0][0].action, Action::NoticeClose));
    }

    #[test]
    fn snoozed_screen_has_dismiss_button() {
        let item_id = Uuid::now_v7();
        let ui = snoozed_ui("😴 Напомню в 11:00".into(), "Позвонить маме", item_id);

        let rows = button_rows(&ui);
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(row_labels(rows[0]), ["📋 Открыть", "🗑 Убрать"]);
        assert!(matches!(
            rows[0][0].action,
            Action::NoticeOpen { item_id: id } if id == item_id
        ));
        assert!(matches!(rows[0][1].action, Action::NoticeClose));
    }

    #[test]
    fn gone_screen_has_dismiss_button() {
        let ui = gone_ui();

        let rows = button_rows(&ui);
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(row_labels(rows[0]), ["🏠 Меню", "🗑 Убрать"]);
        assert!(matches!(rows[0][1].action, Action::NoticeClose));
    }

    fn notice_member(id: Uuid, name: &str, deleted: bool) -> User {
        User {
            id,
            household_id: Uuid::nil(),
            telegram_user_id: 1,
            telegram_chat_id: 1,
            display_name: name.into(),
            username: None,
            role: UserRole::Member,
            timezone: None,
            settings: serde_json::json!({}),
            created_at: DateTime::UNIX_EPOCH,
            updated_at: DateTime::UNIX_EPOCH,
            deleted_at: deleted.then_some(DateTime::UNIX_EPOCH),
        }
    }

    fn heading(ui: &Ui) -> &str {
        match &ui.blocks[0] {
            Block::Heading(text) => text,
            other => panic!("expected a heading, got {other:?}"),
        }
    }

    fn assert_only_dismiss(ui: &Ui) {
        assert_eq!(ui.button_count(), 1, "{:?}", ui.blocks);
        let rows = button_rows(ui);
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(row_labels(rows[0]), ["🗑 Убрать"]);
        assert!(matches!(rows[0][0].action, Action::NoticeClose));
    }

    #[test]
    fn member_name_is_hidden_for_the_recipient_themselves() {
        let recipient = Uuid::now_v7();
        let member = notice_member(recipient, "Маша", false);
        assert_eq!(member_name(Some(recipient), recipient, Some(&member)), None);
    }

    #[test]
    fn member_name_is_hidden_without_an_actor() {
        let recipient = Uuid::now_v7();
        assert_eq!(member_name(None, recipient, None), None);
    }

    #[test]
    fn member_name_is_hidden_for_an_excluded_member() {
        let recipient = Uuid::now_v7();
        let actor = Uuid::now_v7();
        let excluded = notice_member(actor, "Маша", true);
        assert_eq!(member_name(Some(actor), recipient, Some(&excluded)), None);
        // Участника нет среди активных в household получателя — имени нет.
        assert_eq!(member_name(Some(actor), recipient, None), None);
    }

    #[test]
    fn member_name_is_shown_for_another_active_member() {
        let recipient = Uuid::now_v7();
        let actor = Uuid::now_v7();
        let member = notice_member(actor, "Маша", false);
        assert_eq!(
            member_name(Some(actor), recipient, Some(&member)),
            Some("Маша".to_owned())
        );
    }

    #[test]
    fn completed_screen_without_a_name_has_only_dismiss() {
        let ui = completed_ui("Вынести мусор", None);
        assert_eq!(heading(&ui), "✅ Выполнено: Вынести мусор");
        assert_only_dismiss(&ui);
    }

    #[test]
    fn completed_screen_with_a_name_appends_it() {
        let ui = completed_ui("Вынести мусор", Some("Маша"));
        assert_eq!(heading(&ui), "✅ Выполнено: Вынести мусор · Маша");
        assert_only_dismiss(&ui);
    }

    #[test]
    fn assigned_screen_shows_the_assignee_name_and_only_dismiss() {
        let ui = assigned_ui("Вынести мусор", Some("Маша"));
        assert_eq!(heading(&ui), "👤 Назначено: Вынести мусор · Маша");
        assert_only_dismiss(&ui);
    }

    #[test]
    fn assigned_screen_without_a_name_has_only_dismiss() {
        let ui = assigned_ui("Вынести мусор", None);
        assert_eq!(heading(&ui), "👤 Назначено: Вынести мусор");
        assert_only_dismiss(&ui);
    }

    #[test]
    fn closed_screens_clip_the_title_like_done_ui() {
        let long = "я".repeat(150);
        let clipped = format!("{}…", "я".repeat(99));
        let completed = completed_ui(&long, None);
        assert_eq!(heading(&completed), format!("✅ Выполнено: {clipped}"));
        let assigned = assigned_ui(&long, Some("Маша"));
        assert_eq!(
            heading(&assigned),
            format!("👤 Назначено: {clipped} · Маша")
        );
    }
}
