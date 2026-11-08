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
use crate::domain::models::{Item, ItemStatus, List};
use crate::domain::recurrence::resolve_local;
use crate::jobs::keys::snooze_origin;
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
    BATCH_PAGE_SIZE, BatchEntry, BatchEntryState, batch_complete, batch_complete_all, batch_open,
    batch_page, batch_snooze, batch_snooze_all, batch_ui,
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
                },
            ),
        ])
        .row(vec![
            button(
                "📅 Завтра в 09:00",
                Action::Snooze {
                    item_id: item.id,
                    until: SnoozeUntil::TomorrowMorning,
                },
            ),
            button("📋 Открыть", Action::NoticeOpen { item_id: item.id }),
        ])
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
}

/// Строка утренней сводки.
pub struct DigestLine {
    /// Название.
    pub title: String,
    /// Срок в человекочитаемом виде.
    pub when: String,
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
    let total_tasks = today.len() + overdue.len();
    let mut ui = Ui::new().heading("☀️ Доброе утро!");
    let line = |entry: &DigestLine| format!("☐ {} · {}", clip(&entry.title, 60), entry.when);
    let mut budget = MAX_LINES;
    for (title, entries) in [("Сегодня", today), ("Просрочено", overdue)] {
        if entries.is_empty() {
            continue;
        }
        let shown = entries.len().min(budget);
        let mut lines: Vec<String> = entries[..shown].iter().map(line).collect();
        if shown < entries.len() {
            lines.push(format!(
                "…и ещё {}",
                ru::tasks(i64::try_from(entries.len() - shown).unwrap_or(0))
            ));
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
    if total_tasks > MAX_LINES
        && let Some(list_id) = tasks_list
    {
        row.push(back_button(
            "Показать ещё",
            Screen::List {
                list_id,
                page: 0,
                edit: false,
                from_lists: false,
            },
        ));
    }
    ui.row(row)
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
        &Outgoing::Edit { message_id, html },
        None,
        ctx.now,
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
        Some((item, list)) => {
            let ui = done_ui(&item, list.id, None);
            edit_notice(ctx, message_id, &ui).await
        }
        None => {
            let ui = Ui::new()
                .heading("Задача больше недоступна")
                .row(vec![back_button(ru::MENU_BUTTON, Screen::Menu)]);
            edit_notice(ctx, message_id, &ui).await
        }
    }
}

/// «Выполнено» из уведомления.
pub async fn complete(
    ctx: &mut Ctx<'_>,
    message_id: i64,
    item_id: Uuid,
    version: i32,
) -> Result<(), Fail> {
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
        Ok(item) => {
            let next = if item.status == ItemStatus::Open {
                ru::due_label(item.due_at, item.due_date, ctx.tz, ctx.now)
            } else {
                None
            };
            ctx.toast(ru::DONE_TOAST);
            let ui = done_ui(&item, item.list_id, next.as_deref());
            edit_notice(ctx, message_id, &ui).await
        }
        Err(DomainError::Conflict { .. } | DomainError::NotFound) => {
            // Задачу успели изменить: кнопки обновляются, действие не повторяется.
            let toast = match load(ctx, item_id).await? {
                None => "Задача больше недоступна",
                Some((item, _)) if item.status == ItemStatus::Done => "Задача уже выполнена",
                Some(_) => "Задача уже изменилась",
            };
            ctx.toast(toast);
            show_current(ctx, message_id, item_id).await
        }
        Err(error) => Err(error.into()),
    }
}

/// Отменить завершение из уведомления.
pub async fn undo(ctx: &mut Ctx<'_>, message_id: i64, item_id: Uuid) -> Result<(), Fail> {
    let household_id = ctx.household()?.id;
    let actor = ctx.actor()?;
    match items::undo_last(&mut *ctx.conn, household_id, &actor, item_id, ctx.now).await {
        Ok(_) => ctx.toast("Отменено"),
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
/// сообщением. Ключ включает время snooze, поэтому повторное нажатие той же
/// кнопки не создаёт вторую доставку, а планировщик не путает её с вхождением
/// срока.
pub(super) async fn insert_snooze(
    ctx: &mut Ctx<'_>,
    item_id: Uuid,
    origin_key: &str,
    deliver_at: DateTime<Utc>,
    batched: bool,
) -> Result<(), Fail> {
    let occurrence_key = notifications::snooze_key(deliver_at);
    let delivery = NewDelivery {
        household_id: ctx.household()?.id,
        user_id: ctx.user()?.id,
        item_id: Some(item_id),
        kind: NotificationKind::Snooze,
        occurrence_key: &occurrence_key,
        origin_key: Some(origin_key),
        scheduled_at: deliver_at,
    };
    if batched {
        notifications::insert_batched(&mut *ctx.conn, &delivery, ctx.now).await?;
    } else {
        notifications::insert(&mut *ctx.conn, &delivery, ctx.now).await?;
    }
    Ok(())
}

/// «Отложить»: создаёт отдельную доставку, которую отправит планировщик.
pub async fn snooze(
    ctx: &mut Ctx<'_>,
    message_id: i64,
    item_id: Uuid,
    until: SnoozeUntil,
) -> Result<(), Fail> {
    let Some((item, _)) = load(ctx, item_id).await? else {
        ctx.toast("Задача больше недоступна");
        return show_current(ctx, message_id, item_id).await;
    };
    if item.status == ItemStatus::Done {
        ctx.toast("Задача уже выполнена");
        return show_current(ctx, message_id, item_id).await;
    }

    let deliver_at = snooze_time(until, ctx.tz, ctx.now)?;
    let origin_key = snooze_origin(item.due_at, item.due_date);
    insert_snooze(ctx, item_id, &origin_key, deliver_at, false).await?;

    let local = deliver_at.with_timezone(&ctx.tz);
    ctx.toast("Отложено");
    let text = format!(
        "😴 Напомню {}",
        ru::due_label(Some(deliver_at), None, ctx.tz, ctx.now)
            .unwrap_or_else(|| local.format("%H:%M").to_string())
    );
    let ui = Ui::new()
        .heading(text)
        .text(clip(&item.title, 120))
        .row(vec![button("📋 Открыть", Action::NoticeOpen { item_id })]);
    edit_notice(ctx, message_id, &ui).await
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

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
            })
            .collect();
        let ui = digest_ui(&lines, &[], 0, Some(Uuid::now_v7()), None);
        assert_eq!(ui.button_count(), 2);
        let tokens = vec!["t:AAAAAAAAAAAAAAAA".to_owned(); 2];
        let html = crate::telegram::rich::render(&ui, &tokens).html;

        assert_eq!(html.matches("☐ Задача").count(), 20);
        assert!(html.contains("…и ещё 5 задач"));
        assert!(html.contains("Показать ещё"));
    }

    #[test]
    fn empty_digest_says_the_day_is_free() {
        let ui = digest_ui(&[], &[], 0, None, None);
        let html = crate::telegram::rich::render(&ui, &[]).html;
        assert!(html.contains("День свободен."));
        assert!(!html.contains("Сегодня"));
        assert!(!html.contains("Просрочено"));
        assert!(!html.contains("Покупки"));
    }

    #[test]
    fn digest_with_only_shopping_keeps_the_purchase_count() {
        let ui = digest_ui(&[], &[], 7, None, None);
        let html = crate::telegram::rich::render(&ui, &[]).html;
        assert!(html.contains("Покупки: 7 позиций"));
    }
}
