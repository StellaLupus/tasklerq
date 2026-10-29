//! Первое знакомство: создание household и присоединение по приглашению.

use sqlx::Acquire;

use crate::domain::error::DomainError;
use crate::domain::identity::NewMember;
use crate::domain::settings::validate_timezone;
use crate::storage::repositories::households::{self, NewOwner};
use crate::storage::repositories::invites;
use crate::telegram::rich::button;

use super::model::{Action, FormAct, Screen, Ui};
use super::{Ctx, Fail, display_timezone, forms};

const MAX_NAME: usize = 100;

fn new_member(ctx: &Ctx<'_>) -> NewMember {
    NewMember {
        telegram_user_id: ctx.sender.id,
        telegram_chat_id: ctx.chat_id,
        display_name: ctx.sender.display_name.clone(),
        username: ctx.sender.username.clone(),
    }
}

/// Команда `/start` с необязательным кодом приглашения.
pub async fn on_start(ctx: &mut Ctx<'_>, argument: &str) -> Result<(), Fail> {
    crate::telegram::views::clear_conversation(&mut *ctx.conn, ctx.chat_id).await?;

    if argument.is_empty() {
        return if ctx.user.is_some() {
            ctx.present(Screen::Menu).await
        } else {
            forms::start_onboarding(ctx).await
        };
    }
    if ctx.user.is_some() {
        ctx.banner("Вы уже состоите в пространстве, приглашение не нужно.");
        return ctx.present(Screen::Menu).await;
    }
    accept_invite(ctx, argument).await
}

async fn accept_invite(ctx: &mut Ctx<'_>, code: &str) -> Result<(), Fail> {
    let joiner = new_member(ctx);
    // Отказ приглашения не должен оставлять следов в транзакции воркера.
    let mut savepoint = ctx.conn.begin().await?;
    let result = invites::accept(&mut savepoint, code, joiner, ctx.now).await;
    match result {
        Ok(joined) => {
            savepoint.commit().await?;
            ctx.tz = display_timezone(Some(&joined.user), Some(&joined.household));
            ctx.banner(format!("Добро пожаловать в «{}»!", joined.household.name));
            ctx.user = Some(joined.user);
            ctx.household = Some(joined.household);
            ctx.deliver_notice_list(&joined.notices).await?;
            ctx.present(Screen::Menu).await
        }
        Err(error) => {
            savepoint.rollback().await?;
            let message = match &error {
                DomainError::NotFound => {
                    "Приглашение не найдено. Попросите владельца создать новое."
                }
                DomainError::Conflict { details, .. } => {
                    match details.get("reason").and_then(|reason| reason.as_str()) {
                        Some("used") => "Это приглашение уже использовано. Попросите новое.",
                        Some("revoked") => "Это приглашение отозвано. Попросите новое.",
                        Some("expired") => "Срок приглашения истёк. Попросите новое.",
                        Some("already_member") => "Вы уже состоите в пространстве.",
                        _ => "Не удалось принять приглашение. Попросите новое.",
                    }
                }
                _ => return Err(error.into()),
            };
            ctx.banner(message);
            forms::start_onboarding(ctx).await
        }
    }
}

/// Экран онбординга.
pub fn view(name: Option<&str>) -> Ui {
    match name {
        None => Ui::new()
            .heading("Привет! Я Tasklerq")
            .text("Помогу вести задачи и покупки всей семьёй.")
            .text("Как назовём ваше пространство? Отправьте название (до 100 символов).")
            .text("Если вас пригласили, откройте ссылку-приглашение от владельца."),
        Some(name) => Ui::new()
            .heading(format!("«{name}» — выберите часовой пояс"))
            .text("Нажмите кнопку или отправьте свой пояс текстом, например Asia/Novosibirsk.")
            .row(vec![
                zone("Москва (UTC+3)", "Europe/Moscow"),
                zone("Санкт-Петербург (UTC+3)", "Europe/Moscow"),
            ])
            .row(vec![
                zone("Калининград (UTC+2)", "Europe/Kaliningrad"),
                zone("Самара (UTC+4)", "Europe/Samara"),
            ])
            .row(vec![zone("Екатеринбург (UTC+5)", "Asia/Yekaterinburg")])
            .row(vec![button(
                "✖ Отмена",
                Action::Form {
                    act: FormAct::Cancel,
                },
            )]),
    }
}

fn zone(label: &str, zone: &str) -> crate::telegram::rich::Button<Action> {
    button(
        label,
        Action::OnboardZone {
            zone: zone.to_owned(),
        },
    )
}

/// Текст при активном онбординге.
pub async fn on_text(ctx: &mut Ctx<'_>, name: Option<String>, text: &str) -> Result<(), Fail> {
    match name {
        None => {
            let trimmed = text.trim();
            if trimmed.is_empty() || trimmed.chars().count() > MAX_NAME {
                ctx.banner("Название — от 1 до 100 символов.");
                return ctx.present(Screen::Form).await;
            }
            forms::put_onboarding(ctx, Some(trimmed.to_owned())).await?;
            ctx.present(Screen::Form).await
        }
        Some(name) => match validate_timezone(text) {
            Ok(zone) => create(ctx, &name, &zone).await,
            Err(_) => {
                ctx.banner("Не знаю такой часовой пояс. Пример: Europe/Moscow.");
                ctx.present(Screen::Form).await
            }
        },
    }
}

/// Выбор часового пояса кнопкой.
pub async fn choose_zone(ctx: &mut Ctx<'_>, zone: &str) -> Result<(), Fail> {
    let name =
        match crate::telegram::views::conversation(&mut *ctx.conn, ctx.chat_id, ctx.now).await? {
            crate::telegram::views::Lookup::Active(conversation) => {
                match serde_json::from_value::<super::model::Form>(conversation.data) {
                    Ok(super::model::Form::Onboarding { name: Some(name) }) => name,
                    _ => return forms::start_onboarding(ctx).await,
                }
            }
            _ => return forms::start_onboarding(ctx).await,
        };
    let zone = validate_timezone(zone)?;
    create(ctx, &name, &zone).await
}

async fn create(ctx: &mut Ctx<'_>, name: &str, zone: &str) -> Result<(), Fail> {
    let member = new_member(ctx);
    let onboarded = households::onboard(
        &mut *ctx.conn,
        name,
        zone,
        NewOwner {
            member,
            timezone: Some(zone.to_owned()),
        },
        ctx.now,
    )
    .await?;
    forms::finish(ctx).await?;
    ctx.tz = display_timezone(Some(&onboarded.user), Some(&onboarded.household));
    ctx.user = Some(onboarded.user);
    ctx.household = Some(onboarded.household);
    ctx.present(Screen::Welcome).await
}
