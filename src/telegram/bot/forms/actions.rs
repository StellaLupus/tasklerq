//! Обработка текста и нажатий кнопок формы.

use uuid::Uuid;

use crate::domain::models::{Item, List};
use crate::domain::recurrence::Due;
use crate::domain::settings::{NotificationSettingsUpdate, ProfileUpdate, parse_hhmm};
use crate::storage::repositories::{items, members, users};
use crate::telegram::ru;
use crate::telegram::views::{self};

use super::state::{apply_text, today};
use super::{clear, load, start_onboarding};
use crate::telegram::bot::model::{Form, FormAct, ItemDraft, Screen, Step, TimeField};
use crate::telegram::bot::onboarding;
use crate::telegram::bot::settings::{self, ReleaseTrigger};
use crate::telegram::bot::{Ctx, Fail};

use super::editor;

// ---------------------------------------------------------------------
// Текст
// ---------------------------------------------------------------------

/// Обработать текст при активной форме.
pub async fn on_text(ctx: &mut Ctx<'_>, form: Form, text: &str) -> Result<(), Fail> {
    match form {
        Form::Onboarding { name } => onboarding::on_text(ctx, name, text).await,
        Form::Editor(editor_form) => editor::on_text(ctx, editor_form, text).await,
        Form::LegacyEdit => legacy_expired(ctx).await,
        Form::DueText {
            item_id,
            version,
            back,
        } => due_text(ctx, item_id, version, back, text).await,
        Form::Search => super::search(ctx, text, Screen::Form).await,
        Form::Rename => rename(ctx, text).await,
        Form::DeleteHousehold => delete_household(ctx, text).await,
        Form::Lead => set_lead(ctx, text).await,
        Form::Time { field } => set_time(ctx, field, text).await,
    }
}

async fn legacy_expired(ctx: &mut Ctx<'_>) -> Result<(), Fail> {
    clear(ctx).await?;
    ctx.banner("Ваша правка устарела. Начнём заново?");
    if ctx.user.is_none() {
        return start_onboarding(ctx).await;
    }
    ctx.present(Screen::Menu).await
}

async fn rename(ctx: &mut Ctx<'_>, text: &str) -> Result<(), Fail> {
    let household_id = ctx.household()?.id;
    let actor = ctx.actor()?;
    let updated = users::update_profile(
        &mut *ctx.conn,
        household_id,
        &actor,
        ProfileUpdate {
            display_name: Some(text.to_owned()),
            ..ProfileUpdate::default()
        },
        ctx.now,
    )
    .await;
    match updated {
        Ok(user) => {
            ctx.user = Some(user);
            clear(ctx).await?;
            ctx.toast("Имя обновлено");
            ctx.present(Screen::Profile).await
        }
        Err(crate::domain::error::DomainError::Validation(_)) => {
            ctx.banner("Имя — от 1 до 64 символов.");
            ctx.present(Screen::Form).await
        }
        Err(error) => Err(error.into()),
    }
}

async fn set_lead(ctx: &mut Ctx<'_>, text: &str) -> Result<(), Fail> {
    let household_id = ctx.household()?.id;
    let actor = ctx.actor()?;
    let Some(minutes) = text.trim().parse::<u16>().ok() else {
        ctx.banner("Введите число минут, например 30.");
        return ctx.present(Screen::Form).await;
    };
    let updated = users::update_profile(
        &mut *ctx.conn,
        household_id,
        &actor,
        ProfileUpdate {
            notifications: Some(NotificationSettingsUpdate {
                lead_minutes: Some(minutes),
                ..NotificationSettingsUpdate::default()
            }),
            ..ProfileUpdate::default()
        },
        ctx.now,
    )
    .await;
    match updated {
        Ok(user) => {
            ctx.user = Some(user);
            clear(ctx).await?;
            ctx.present(Screen::Notifications).await
        }
        Err(crate::domain::error::DomainError::Validation(_)) => {
            ctx.banner("Напоминание — от 5 минут до 24 часов (1440 минут).");
            ctx.present(Screen::Form).await
        }
        Err(error) => Err(error.into()),
    }
}

async fn set_time(ctx: &mut Ctx<'_>, field: TimeField, text: &str) -> Result<(), Fail> {
    let household_id = ctx.household()?.id;
    let actor = ctx.actor()?;
    let value = text.trim();

    // Домен проверяет и формат, и несовпадение границ тихих часов.
    let updated = users::update_profile(
        &mut *ctx.conn,
        household_id,
        &actor,
        ProfileUpdate {
            notifications: Some(field.update(value)),
            ..ProfileUpdate::default()
        },
        ctx.now,
    )
    .await;
    match updated {
        Ok(user) => {
            clear(ctx).await?;
            settings::finish_settings_change(ctx, user, Screen::Notifications, field.trigger())
                .await
        }
        Err(crate::domain::error::DomainError::Validation(_)) => {
            ctx.banner(time_error(field, value));
            ctx.present(Screen::Form).await
        }
        Err(error) => Err(error.into()),
    }
}

impl TimeField {
    /// Обновление настройки, меняющее только это поле.
    fn update(self, value: &str) -> NotificationSettingsUpdate {
        let mut update = NotificationSettingsUpdate::default();
        let time = Some(value.to_owned());
        match self {
            Self::Digest => update.digest_time = time,
            Self::AllDay => update.all_day_time = time,
            Self::QuietStart => update.quiet_start = time,
            Self::QuietEnd => update.quiet_end = time,
        }
        update
    }

    /// Правка границ тихих часов может сделать текущий момент не тихим: тогда
    /// отложенные уведомления выпускаются сразу.
    fn trigger(self) -> Option<ReleaseTrigger> {
        matches!(self, Self::QuietStart | Self::QuietEnd).then_some(ReleaseTrigger::Boundaries)
    }
}

/// Текст ошибки ввода времени.
fn time_error(field: TimeField, value: &str) -> &'static str {
    match field {
        TimeField::QuietStart | TimeField::QuietEnd if parse_hhmm(value, "time").is_ok() => {
            "Начало и конец тихих часов не должны совпадать."
        }
        _ => "Введите время в формате HH:MM, например 08:30.",
    }
}

async fn delete_household(ctx: &mut Ctx<'_>, text: &str) -> Result<(), Fail> {
    let household = ctx.household()?.clone();
    let actor = ctx.actor()?;
    if text.trim() != household.name {
        ctx.banner("Название не совпало. Введите его точно или нажмите «Отмена».");
        return ctx.present(Screen::Form).await;
    }
    let change =
        members::delete_household(&mut *ctx.conn, household.id, &actor, text.trim(), ctx.now)
            .await?;
    ctx.deliver_notices(&change).await?;
    clear(ctx).await?;
    ctx.user = None;
    ctx.household = None;
    views::set(
        &mut *ctx.conn,
        ctx.chat_id,
        "menu",
        &serde_json::json!({"t": "menu"}),
        ctx.now,
    )
    .await?;
    ctx.say("Пространство удалено. Данные будут стёрты через 30 дней. Чтобы начать заново, отправьте /start.")
        .await
}

async fn due_text(
    ctx: &mut Ctx<'_>,
    item_id: Uuid,
    version: i32,
    back: Screen,
    text: &str,
) -> Result<(), Fail> {
    let (_, mut draft) = load_item_draft(ctx, item_id).await?;
    let today = today(ctx);
    let now_time = ctx.now.with_timezone(&ctx.tz).time();
    if let Err(message) = apply_text(Step::DueText, &mut draft, text, today, now_time) {
        ctx.banner(message);
        return ctx.present(Screen::Form).await;
    }
    let (due_at, due_date) = draft.due(ctx.tz)?;
    let due = match (due_at, due_date) {
        (Some(at), _) => Due::Timed(at),
        (None, Some(date)) => Due::AllDay(date),
        (None, None) => return Err(Fail::User(format!("Не понял срок. {}", ru::DUE_HINT))),
    };
    clear(ctx).await?;
    crate::telegram::bot::card::apply_due(ctx, item_id, version, due, back).await
}

pub(super) async fn load_item_draft(
    ctx: &mut Ctx<'_>,
    item_id: Uuid,
) -> Result<(Item, ItemDraft), Fail> {
    let household_id = ctx.household()?.id;
    let item = items::get(&mut *ctx.conn, household_id, item_id).await?;
    let draft = ItemDraft::from_item(&item, ctx.tz);
    Ok((item, draft))
}

// ---------------------------------------------------------------------
// Кнопки
// ---------------------------------------------------------------------

/// Обработать кнопку формы.
pub async fn on_action(ctx: &mut Ctx<'_>, act: FormAct) -> Result<(), Fail> {
    let Some(form) = load(ctx).await? else {
        return super::expired(ctx).await;
    };
    match form {
        Form::Editor(editor_form) => editor::on_action(ctx, editor_form, act).await,
        Form::LegacyEdit => legacy_expired(ctx).await,
        Form::DueText { item_id, back, .. } => {
            clear(ctx).await?;
            ctx.present(Screen::Item {
                item_id,
                back: Box::new(back),
            })
            .await
        }
        Form::Onboarding { .. }
        | Form::Search
        | Form::Rename
        | Form::DeleteHousehold
        | Form::Lead
        | Form::Time { .. } => {
            // У остальных форм одна кнопка — отмена.
            clear(ctx).await?;
            let back = match form {
                Form::Rename => Screen::Profile,
                Form::DeleteHousehold => Screen::Danger,
                Form::Lead | Form::Time { .. } => Screen::Notifications,
                _ => Screen::Menu,
            };
            if ctx.user.is_none() {
                return start_onboarding(ctx).await;
            }
            ctx.present(back).await
        }
    }
}

pub(super) async fn find_duplicate(
    ctx: &mut Ctx<'_>,
    list: &List,
    title: &str,
) -> Result<Option<crate::telegram::bot::model::Duplicate>, Fail> {
    let existing = items::list_by_list(&mut *ctx.conn, list.household_id, list.id).await?;
    Ok(crate::telegram::bot::model::Duplicate::find(
        &existing, title,
    ))
}
