//! Настройки, участники, приглашения и журнал.

use serde_json::Value;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::identity::{AuditEvent, AuditEventType};
use crate::domain::models::{ActorType, UserRole};
use crate::domain::settings::{
    NotificationSettings, NotificationSettingsUpdate, ProfileUpdate, SUGGESTED_TIMEZONES,
    validate_timezone,
};
use crate::storage::repositories::{audit, invites, members, users};
use crate::telegram::rich::{Span, button};
use crate::telegram::ru;

use super::model::{Action, PAGE_SIZE, Screen, Setting, Ui};
use super::screens::{back_button, clip};
use super::{Ctx, Fail, display_timezone};

fn on_off(value: bool) -> &'static str {
    if value { "вкл" } else { "выкл" }
}

/// Собрать экран раздела «Настройки».
pub async fn view(ctx: &mut Ctx<'_>, screen: &Screen) -> Result<Ui, Fail> {
    match screen {
        Screen::Settings => settings(ctx),
        Screen::Notifications => notifications(ctx),
        Screen::Timezones => Ok(timezones()),
        Screen::Family => family(ctx).await,
        Screen::Member { user_id } => member(ctx, *user_id).await,
        Screen::Invites => invites_screen(ctx).await,
        Screen::InviteCreated { code, .. } => {
            let link = invites::deep_link(ctx.bot_username, code);
            Ok(Ui::new()
                .heading("Приглашение создано")
                .text(format!("Действует {}.", ru::hours(invites::TTL_HOURS)))
                .spans(vec![Span::Code(link)])
                .text("Отправьте ссылку тому, кого хотите пригласить. Повторно показать её нельзя.")
                .row(vec![
                    back_button("📨 Все приглашения", Screen::Invites),
                    back_button("⬅️ К участникам", Screen::Family),
                ]))
        }
        Screen::Journal { page } => journal(ctx, *page).await,
        Screen::ConfirmRemove { user_id } => confirm_remove(ctx, *user_id).await,
        Screen::ConfirmTransfer { user_id } => confirm_transfer(ctx, *user_id).await,
        Screen::ConfirmLeave => Ok(Ui::new()
            .heading("Выйти из пространства?")
            .text("Назначенные на вас задачи останутся без исполнителя. Вернуться можно только по новому приглашению.")
            .row(vec![
                button("🚪 Да, выйти", Action::Leave),
                back_button("Отмена", Screen::Settings),
            ])),
        _ => Ok(Ui::new()),
    }
}

fn settings(ctx: &Ctx<'_>) -> Result<Ui, Fail> {
    let user = ctx.user()?;
    let household = ctx.household()?;
    let notifications = NotificationSettings::from_settings(&user.settings);
    let zone = match user.timezone.as_deref() {
        Some(zone) => zone.to_owned(),
        None => format!("{} (как в пространстве)", household.timezone),
    };

    let mut ui = Ui::new()
        .heading("Настройки")
        .text(format!("👤 Профиль: {}", user.display_name))
        .text(format!("🌍 Часовой пояс: {zone}"))
        .text(format!(
            "🔔 Напоминания: {}, за {}",
            on_off(notifications.reminders_enabled),
            ru::minutes(i64::from(notifications.lead_minutes))
        ))
        .text(format!(
            "🌅 Сводка: {}, {}",
            on_off(notifications.digest_enabled),
            notifications.digest_time
        ))
        .text(format!(
            "🌙 Тихие часы: {}",
            if notifications.quiet_hours_enabled {
                format!("{}–{}", notifications.quiet_start, notifications.quiet_end)
            } else {
                "выкл".to_owned()
            }
        ))
        .row(vec![
            button("👤 Имя", Action::RenameStart),
            back_button("🌍 Время", Screen::Timezones),
        ])
        .row(vec![back_button("🔔 Уведомления", Screen::Notifications)]);

    if user.role == UserRole::Owner {
        ui = ui
            .row(vec![
                back_button("👥 Семья", Screen::Family),
                back_button("📜 Журнал", Screen::Journal { page: 0 }),
            ])
            .row(vec![button(
                "🗑 Удалить пространство",
                Action::HouseholdDeleteStart,
            )]);
    } else {
        ui = ui.row(vec![back_button("🚪 Выйти", Screen::ConfirmLeave)]);
    }
    Ok(ui.row(vec![back_button("⬅️ Назад", Screen::Menu)]))
}

fn notifications(ctx: &Ctx<'_>) -> Result<Ui, Fail> {
    let settings = NotificationSettings::from_settings(&ctx.user()?.settings);
    let toggle = |label: &str, value: bool, setting: Setting| {
        button(
            format!("{label}: {}", on_off(value)),
            Action::Toggle { setting },
        )
    };
    let lead = |minutes: u16| {
        let mark = if settings.lead_minutes == minutes {
            "● "
        } else {
            ""
        };
        button(format!("{mark}{minutes} мин"), Action::SetLead { minutes })
    };
    Ok(Ui::new()
        .heading("Уведомления")
        .text(format!(
            "Напоминать о сроках за {}. Выберите другое значение:",
            ru::minutes(i64::from(settings.lead_minutes))
        ))
        .row(vec![lead(5), lead(15), lead(30)])
        .row(vec![lead(60), button("Своё…", Action::LeadStart)])
        .row(vec![toggle(
            "🔔 Напоминания",
            settings.reminders_enabled,
            Setting::Reminders,
        )])
        .row(vec![toggle(
            "🌅 Сводка",
            settings.digest_enabled,
            Setting::Digest,
        )])
        .row(vec![toggle(
            "👤 О назначении",
            settings.assignment_enabled,
            Setting::Assignment,
        )])
        .row(vec![toggle(
            "🌙 Тихие часы",
            settings.quiet_hours_enabled,
            Setting::QuietHours,
        )])
        .row(vec![back_button("⬅️ Назад", Screen::Settings)]))
}

fn timezones() -> Ui {
    let mut ui = Ui::new().heading("Часовой пояс");
    for chunk in SUGGESTED_TIMEZONES.chunks(2) {
        ui = ui.row(
            chunk
                .iter()
                .map(|zone| {
                    button(
                        *zone,
                        Action::SetTimezone {
                            zone: (*zone).to_owned(),
                        },
                    )
                })
                .collect(),
        );
    }
    ui.row(vec![button(
        "Как в пространстве",
        Action::SetTimezone {
            zone: String::new(),
        },
    )])
    .row(vec![back_button("⬅️ Назад", Screen::Settings)])
}

async fn family(ctx: &mut Ctx<'_>) -> Result<Ui, Fail> {
    ctx.require_owner()?;
    let me = ctx.user()?.id;
    let household_id = ctx.household()?.id;
    let members = users::list_active(&mut *ctx.conn, household_id).await?;

    let mut ui = Ui::new().heading("Семья");
    let lines: Vec<String> = members
        .iter()
        .map(|member| {
            let icon = if member.role == UserRole::Owner {
                "👑"
            } else {
                "👤"
            };
            let you = if member.id == me { " (вы)" } else { "" };
            format!("{icon} {}{you}", member.display_name)
        })
        .collect();
    ui = ui.text(lines.join("\n"));
    ui = ui.row(vec![back_button("➕ Пригласить", Screen::Invites)]);
    for member in members.iter().filter(|member| member.id != me) {
        ui = ui.row(vec![back_button(
            &format!("👤 {}", clip(&member.display_name, 30)),
            Screen::Member { user_id: member.id },
        )]);
    }
    Ok(ui.row(vec![back_button("⬅️ Назад", Screen::Settings)]))
}

async fn member(ctx: &mut Ctx<'_>, user_id: Uuid) -> Result<Ui, Fail> {
    ctx.require_owner()?;
    let household_id = ctx.household()?.id;
    let member = users::get(&mut *ctx.conn, household_id, user_id).await?;
    Ok(Ui::new()
        .heading(member.display_name)
        .text(if member.role == UserRole::Owner {
            "Владелец"
        } else {
            "Участник"
        })
        .row(vec![
            button("👑 Передать владение", Action::TransferAsk { user_id }),
            button("🗑 Удалить", Action::MemberRemoveAsk { user_id }),
        ])
        .row(vec![back_button("⬅️ Назад", Screen::Family)]))
}

async fn confirm_remove(ctx: &mut Ctx<'_>, user_id: Uuid) -> Result<Ui, Fail> {
    ctx.require_owner()?;
    let household_id = ctx.household()?.id;
    let member = users::get(&mut *ctx.conn, household_id, user_id).await?;
    Ok(Ui::new()
        .heading(format!(
            "Удалить участника «{}»?",
            clip(&member.display_name, 40)
        ))
        .text("Доступ прекратится сразу, назначенные задачи останутся без исполнителя.")
        .row(vec![
            button("🗑 Да, удалить", Action::MemberRemove { user_id }),
            back_button("Отмена", Screen::Member { user_id }),
        ]))
}

async fn confirm_transfer(ctx: &mut Ctx<'_>, user_id: Uuid) -> Result<Ui, Fail> {
    ctx.require_owner()?;
    let household_id = ctx.household()?.id;
    let member = users::get(&mut *ctx.conn, household_id, user_id).await?;
    Ok(Ui::new()
        .heading(format!(
            "Передать владение «{}»?",
            clip(&member.display_name, 40)
        ))
        .text("Вы станете обычным участником и не сможете управлять семьёй.")
        .row(vec![
            button("👑 Да, передать", Action::Transfer { user_id }),
            back_button("Отмена", Screen::Member { user_id }),
        ]))
}

async fn invites_screen(ctx: &mut Ctx<'_>) -> Result<Ui, Fail> {
    ctx.require_owner()?;
    let household_id = ctx.household()?.id;
    let actor = ctx.actor()?;
    let active = invites::list_active(&mut *ctx.conn, household_id, &actor, ctx.now).await?;

    let mut ui = Ui::new().heading("Приглашения");
    if active.is_empty() {
        ui = ui.text("Активных приглашений нет.");
    } else {
        ui = ui.text(format!(
            "Активных: {} из {}. Коды в базе не хранятся, поэтому ссылку можно увидеть только при создании.",
            active.len(),
            invites::MAX_ACTIVE
        ));
        for invite in &active {
            let until = invite.expires_at.with_timezone(&ctx.tz);
            ui = ui.row(vec![button(
                format!("🚫 Отозвать · до {}", until.format("%d.%m %H:%M")),
                Action::InviteRevoke {
                    invite_id: invite.id,
                },
            )]);
        }
    }
    Ok(ui
        .row(vec![button("➕ Создать приглашение", Action::InviteCreate)])
        .row(vec![back_button("⬅️ Назад", Screen::Family)]))
}

fn source_label(actor_type: ActorType) -> Option<&'static str> {
    match actor_type {
        ActorType::User => None,
        ActorType::System => Some("система"),
        ActorType::HomeAssistant => Some("Home Assistant"),
        ActorType::Voice => Some("голос"),
    }
}

fn payload_text(payload: &Value, key: &str) -> String {
    payload
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or("—")
        .to_owned()
}

fn role_label(role: &str) -> &'static str {
    match role {
        "owner" => "владелец",
        "member" => "участник",
        _ => "—",
    }
}

/// Строка журнала. В неё попадают только имена и роли из payload.
pub fn journal_line(event: &AuditEvent, tz: chrono_tz::Tz) -> String {
    let when = event.created_at.with_timezone(&tz);
    let stamp = format!(
        "{} {} {}",
        when.format("%-d"),
        ru::month_short(chrono::Datelike::month(&when)),
        when.format("%H:%M")
    );
    let name = payload_text(&event.payload, "display_name");
    let actor = payload_text(&event.payload, "actor_name");
    let what = match event.event_type {
        AuditEventType::HouseholdCreated => "Пространство создано".to_owned(),
        AuditEventType::MemberJoined => format!("Присоединился(-ась): {name}"),
        AuditEventType::MemberLeft => format!("Вышел(-ла): {name}"),
        AuditEventType::MemberRemoved => format!("Удалён участник: {name} (удалил: {actor})"),
        AuditEventType::RoleChanged => format!(
            "Роль {name}: {} → {}",
            role_label(&payload_text(&event.payload, "from")),
            role_label(&payload_text(&event.payload, "to"))
        ),
        AuditEventType::InviteCreated => "Создано приглашение".to_owned(),
        AuditEventType::InviteUsed => format!("Приглашение использовано: {actor}"),
        AuditEventType::InviteRevoked => "Приглашение отозвано".to_owned(),
        AuditEventType::HouseholdDeleted => "Пространство удалено".to_owned(),
        AuditEventType::TokenCreated
        | AuditEventType::TokenRevoked
        | AuditEventType::AuthFailed
        | AuditEventType::RateLimited => "Служебное событие".to_owned(),
    };
    match source_label(event.actor_type) {
        Some(source) => format!("{stamp} · {what} · источник: {source}"),
        None => format!("{stamp} · {what}"),
    }
}

async fn journal(ctx: &mut Ctx<'_>, page: u32) -> Result<Ui, Fail> {
    ctx.require_owner()?;
    let household_id = ctx.household()?.id;
    let actor = ctx.actor()?;
    let events = audit::membership_journal(
        &mut *ctx.conn,
        household_id,
        &actor,
        ctx.now,
        None,
        audit::MAX_PAGE,
    )
    .await?;

    let mut ui = Ui::new().heading("Журнал за 90 дней");
    if events.is_empty() {
        return Ok(ui
            .text("Событий пока нет.")
            .row(vec![back_button("⬅️ Назад", Screen::Settings)]));
    }

    let pages = events.len().div_ceil(PAGE_SIZE).max(1);
    let page = usize::try_from(page).unwrap_or(0).min(pages - 1);
    let slice = &events[page * PAGE_SIZE..((page + 1) * PAGE_SIZE).min(events.len())];
    let lines: Vec<String> = slice
        .iter()
        .map(|event| journal_line(event, ctx.tz))
        .collect();
    ui = ui.text(lines.join("\n"));
    if i64::try_from(events.len()).is_ok_and(|count| count >= audit::MAX_PAGE) {
        ui = ui.text(format!("Показаны последние {} событий.", audit::MAX_PAGE));
    }
    if pages > 1 {
        let mut row = Vec::new();
        if page > 0 {
            row.push(back_button(
                "←",
                Screen::Journal {
                    page: u32::try_from(page - 1).unwrap_or(0),
                },
            ));
        }
        if page + 1 < pages {
            row.push(back_button(
                "→",
                Screen::Journal {
                    page: u32::try_from(page + 1).unwrap_or(0),
                },
            ));
        }
        ui = ui
            .text(format!("Страница {} из {pages}", page + 1))
            .row(row);
    }
    Ok(ui.row(vec![back_button("⬅️ Назад", Screen::Settings)]))
}

// ---------------------------------------------------------------------
// Действия
// ---------------------------------------------------------------------

/// Выбрать личный часовой пояс; пустая строка возвращает пояс household.
pub async fn set_timezone(ctx: &mut Ctx<'_>, zone: &str) -> Result<(), Fail> {
    let household_id = ctx.household()?.id;
    let actor = ctx.actor()?;
    let timezone = if zone.is_empty() {
        None
    } else {
        Some(validate_timezone(zone)?)
    };
    let user = users::update_profile(
        &mut *ctx.conn,
        household_id,
        &actor,
        ProfileUpdate {
            timezone: Some(timezone),
            ..ProfileUpdate::default()
        },
        ctx.now,
    )
    .await?;
    ctx.tz = display_timezone(Some(&user), ctx.household.as_ref());
    ctx.user = Some(user);
    ctx.toast("Часовой пояс обновлён");
    ctx.present(Screen::Settings).await
}

async fn update_notifications(
    ctx: &mut Ctx<'_>,
    update: NotificationSettingsUpdate,
) -> Result<(), Fail> {
    let household_id = ctx.household()?.id;
    let actor = ctx.actor()?;
    let user = users::update_profile(
        &mut *ctx.conn,
        household_id,
        &actor,
        ProfileUpdate {
            notifications: Some(update),
            ..ProfileUpdate::default()
        },
        ctx.now,
    )
    .await?;
    ctx.user = Some(user);
    ctx.present(Screen::Notifications).await
}

/// Переключить настройку уведомлений.
pub async fn toggle(ctx: &mut Ctx<'_>, setting: Setting) -> Result<(), Fail> {
    let current = NotificationSettings::from_settings(&ctx.user()?.settings);
    let update = match setting {
        Setting::Reminders => NotificationSettingsUpdate {
            reminders_enabled: Some(!current.reminders_enabled),
            ..Default::default()
        },
        Setting::Digest => NotificationSettingsUpdate {
            digest_enabled: Some(!current.digest_enabled),
            ..Default::default()
        },
        Setting::Assignment => NotificationSettingsUpdate {
            assignment_enabled: Some(!current.assignment_enabled),
            ..Default::default()
        },
        Setting::QuietHours => NotificationSettingsUpdate {
            quiet_hours_enabled: Some(!current.quiet_hours_enabled),
            ..Default::default()
        },
    };
    update_notifications(ctx, update).await
}

/// Выбрать время напоминания.
pub async fn set_lead(ctx: &mut Ctx<'_>, minutes: u16) -> Result<(), Fail> {
    update_notifications(
        ctx,
        NotificationSettingsUpdate {
            lead_minutes: Some(minutes),
            ..Default::default()
        },
    )
    .await
}

/// Создать приглашение и показать ссылку.
pub async fn invite_create(ctx: &mut Ctx<'_>) -> Result<(), Fail> {
    ctx.require_owner()?;
    let household_id = ctx.household()?.id;
    let actor = ctx.actor()?;
    let created = match invites::create(&mut *ctx.conn, household_id, &actor, ctx.now).await {
        Ok(created) => created,
        Err(DomainError::Conflict { .. }) => {
            return Err(Fail::User(
                "Уже есть 5 активных приглашений. Отзовите ненужные и попробуйте снова.".into(),
            ));
        }
        Err(error) => return Err(error.into()),
    };
    ctx.present(Screen::InviteCreated {
        invite_id: created.invite.id,
        code: created.code,
    })
    .await
}

/// Отозвать приглашение.
pub async fn invite_revoke(ctx: &mut Ctx<'_>, invite_id: Uuid) -> Result<(), Fail> {
    ctx.require_owner()?;
    let household_id = ctx.household()?.id;
    let actor = ctx.actor()?;
    invites::revoke(&mut *ctx.conn, household_id, &actor, invite_id, ctx.now).await?;
    ctx.toast("Приглашение отозвано");
    ctx.present(Screen::Invites).await
}

/// Запросить подтверждение удаления участника.
pub async fn member_remove_ask(ctx: &mut Ctx<'_>, user_id: Uuid) -> Result<(), Fail> {
    ctx.present(Screen::ConfirmRemove { user_id }).await
}

/// Удалить участника.
pub async fn member_remove(ctx: &mut Ctx<'_>, user_id: Uuid) -> Result<(), Fail> {
    let household_id = ctx.household()?.id;
    let actor = ctx.actor()?;
    let change = match members::remove(&mut *ctx.conn, household_id, &actor, user_id, ctx.now).await
    {
        Ok(change) => change,
        Err(DomainError::Conflict { .. }) => {
            return Err(Fail::User("Владельца удалить нельзя.".into()));
        }
        Err(error) => return Err(error.into()),
    };
    ctx.deliver_notices(&change).await?;
    ctx.toast("Участник удалён");
    ctx.present(Screen::Family).await
}

/// Запросить подтверждение передачи владения.
pub async fn transfer_ask(ctx: &mut Ctx<'_>, user_id: Uuid) -> Result<(), Fail> {
    ctx.present(Screen::ConfirmTransfer { user_id }).await
}

/// Передать владение.
pub async fn transfer(ctx: &mut Ctx<'_>, user_id: Uuid) -> Result<(), Fail> {
    let household_id = ctx.household()?.id;
    let actor = ctx.actor()?;
    let change =
        members::transfer_ownership(&mut *ctx.conn, household_id, &actor, user_id, ctx.now).await?;
    ctx.deliver_notices(&change).await?;
    let me = ctx.user()?.id;
    ctx.user = Some(users::get(&mut *ctx.conn, household_id, me).await?);
    ctx.toast("Владение передано");
    ctx.present(Screen::Settings).await
}

/// Запросить подтверждение выхода.
pub async fn leave_ask(ctx: &mut Ctx<'_>) -> Result<(), Fail> {
    ctx.present(Screen::ConfirmLeave).await
}

/// Выйти из household.
pub async fn leave(ctx: &mut Ctx<'_>) -> Result<(), Fail> {
    let household_id = ctx.household()?.id;
    let actor = ctx.actor()?;
    let change = match members::leave(&mut *ctx.conn, household_id, &actor, ctx.now).await {
        Ok(change) => change,
        Err(DomainError::Conflict { .. }) => {
            return Err(Fail::User(
                "Владелец не может выйти: сначала передайте владение другому участнику.".into(),
            ));
        }
        Err(error) => return Err(error.into()),
    };
    ctx.deliver_notices(&change).await?;
    ctx.user = None;
    ctx.household = None;
    crate::telegram::views::clear_conversation(&mut *ctx.conn, ctx.chat_id).await?;
    ctx.say("Вы вышли из пространства. Чтобы присоединиться снова, понадобится новое приглашение.")
        .await
}
