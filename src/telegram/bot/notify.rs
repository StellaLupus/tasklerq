//! Экраны уведомлений и действия «Готово» / «Отложить» / «Открыть».
//!
//! Уведомления — отдельные сообщения, а не часть навигационного экрана,
//! поэтому их кнопки выпускаются с областью `notice` и редактируют своё
//! сообщение. Сами уведомления ставит в очередь планировщик (фаза 7): он
//! использует эти конструкторы экранов.

use chrono::{DateTime, Duration, NaiveTime, Utc};
use chrono_tz::Tz;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::models::{Item, ItemStatus, List};
use crate::domain::recurrence::resolve_local;
use crate::storage::repositories::{items, lists};
use crate::telegram::callbacks::Scope;
use crate::telegram::outbox::{self, Outgoing};
use crate::telegram::rich::{Span, button};
use crate::telegram::ru;

use super::model::{Action, Screen, SnoozeUntil, Ui};
use super::screens::{back_button, clip};
use super::{Ctx, Fail, issue_and_render};

/// Час, на который переносится напоминание «до завтра».
const MORNING: (u32, u32) = (9, 0);

fn when_line(item: &Item, list: &List, tz: Tz, now: DateTime<Utc>) -> String {
    match ru::due_label(item.due_at, item.due_date, tz, now) {
        Some(due) => format!("{} · {}", ru::capitalize(&due), list.name),
        None => list.name.clone(),
    }
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
        .spans(vec![Span::bold(clip(&item.title, 120))])
        .text(when_line(item, list, tz, now))
        .row(vec![
            button(
                "✓ Готово",
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
        .spans(vec![Span::bold(clip(&item.title, 120))])
        .text(when_line(item, list, tz, now))
        .row(vec![
            button("📋 Открыть", Action::NoticeOpen { item_id: item.id }),
            button(
                "✓ Готово",
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
        ui = ui.text("На сегодня задач нет.");
    }
    if shopping_open > 0 {
        ui = ui.text(format!("Покупки: {}", ru::positions(shopping_open)));
    }
    let mut row = Vec::new();
    if let Some(list_id) = tasks_list {
        row.push(back_button("📋 Задачи", Screen::List { list_id, page: 0 }));
    }
    if let Some(list_id) = shopping_list {
        row.push(back_button("🛒 Покупки", Screen::List { list_id, page: 0 }));
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
        back_button("📋 К списку", Screen::List { list_id, page: 0 }),
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
async fn edit_notice(ctx: &mut Ctx<'_>, message_id: i64, ui: &Ui) -> Result<(), Fail> {
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
                .heading("Задача удалена")
                .row(vec![back_button("📋 В меню", Screen::Menu)]);
            edit_notice(ctx, message_id, &ui).await
        }
    }
}

/// «Готово» из уведомления.
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
            ctx.toast("Готово");
            let ui = done_ui(&item, item.list_id, next.as_deref());
            edit_notice(ctx, message_id, &ui).await
        }
        Err(DomainError::Conflict { .. } | DomainError::NotFound) => {
            // Задачу успели изменить: кнопки обновляются, действие не повторяется.
            ctx.toast("Задача уже изменилась");
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
pub async fn open(ctx: &mut Ctx<'_>, item_id: Uuid) -> Result<(), Fail> {
    if load(ctx, item_id).await?.is_none() {
        ctx.toast("Задача удалена");
        return ctx.present(Screen::Menu).await;
    }
    ctx.present(Screen::Item {
        item_id,
        back: Box::new(Screen::Menu),
    })
    .await
}

fn snooze_time(
    until: SnoozeUntil,
    tz: Tz,
    now: DateTime<Utc>,
) -> Result<DateTime<Utc>, DomainError> {
    match until {
        SnoozeUntil::OneHour => Ok(now + Duration::hours(1)),
        SnoozeUntil::TomorrowMorning => {
            let tomorrow = now.with_timezone(&tz).date_naive() + Duration::days(1);
            let morning =
                NaiveTime::from_hms_opt(MORNING.0, MORNING.1, 0).unwrap_or(NaiveTime::MIN);
            resolve_local(tz, tomorrow, morning)
        }
    }
}

/// «Отложить»: напоминание придёт позже новым сообщением.
pub async fn snooze(
    ctx: &mut Ctx<'_>,
    message_id: i64,
    item_id: Uuid,
    until: SnoozeUntil,
) -> Result<(), Fail> {
    let household_id = ctx.household()?.id;
    let Some((item, list)) = load(ctx, item_id).await? else {
        ctx.toast("Задача удалена");
        return show_current(ctx, message_id, item_id).await;
    };
    if item.status == ItemStatus::Done {
        ctx.toast("Задача уже выполнена");
        return show_current(ctx, message_id, item_id).await;
    }

    let deliver_at = snooze_time(until, ctx.tz, ctx.now)?;
    // Кнопки нового напоминания живут 24 часа от момента доставки.
    let ui = reminder_ui(&item, &list, ctx.tz, deliver_at, None);
    let html = render_notice(&mut *ctx.conn, ctx.sender.id, household_id, &ui, deliver_at).await?;
    let key = format!(
        "snooze:{}:{item_id}:{}",
        ctx.sender.id,
        deliver_at.timestamp()
    );
    if let Some(seq) = outbox::enqueue(
        &mut *ctx.conn,
        ctx.chat_id,
        &Outgoing::Message { html, rich: true },
        Some(&key),
        ctx.now,
    )
    .await?
    {
        sqlx::query("UPDATE outbox SET next_attempt_at = $2 WHERE seq = $1")
            .bind(seq)
            .bind(deliver_at)
            .execute(&mut *ctx.conn)
            .await?;
    }

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
    fn digest_is_limited_to_twenty_lines() {
        let lines: Vec<DigestLine> = (0..25)
            .map(|n| DigestLine {
                title: format!("Задача {n}"),
                when: "сегодня".into(),
            })
            .collect();
        let ui = digest_ui(&lines, &[], 0, None, None);
        let html = crate::telegram::rich::render(&ui, &[]).html;

        assert_eq!(html.matches("☐ Задача").count(), 20);
        assert!(html.contains("…и ещё 5 задач"));
    }

    #[test]
    fn empty_digest_says_so() {
        let ui = digest_ui(&[], &[], 7, None, None);
        let html = crate::telegram::rich::render(&ui, &[]).html;
        assert!(html.contains("На сегодня задач нет."));
        assert!(html.contains("Покупки: 7 позиций"));
    }
}
