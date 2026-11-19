//! Первое знакомство: создание household и присоединение по приглашению.

use serde_json::json;
use sqlx::Acquire;
use uuid::Uuid;

use crate::admin_console::codes::{self, Purpose};
use crate::config::TenancyMode;
use crate::domain::error::DomainError;
use crate::domain::first_contact::{ClaimTarget, FirstContact, Installation, classify};
use crate::domain::identity::{AuditEventType, NewMember};
use crate::domain::models::Actor;
use crate::domain::settings::validate_timezone;
use crate::storage::repositories::admin_accounts;
use crate::storage::repositories::audit;
use crate::storage::repositories::households::{self, NewOwner};
use crate::storage::repositories::invites;
use crate::storage::repositories::users;
use crate::telegram::rich::{Span, button};
use crate::telegram::ru;

use super::model::{Action, FirstContactScreen, FormAct, Screen, Ui};
use super::{Ctx, Fail, display_timezone, forms};

const MAX_NAME: usize = 100;

/// Длина кода в ссылке `own_…` / `adm_…`: `random_base64url(16)`.
const CLAIM_CODE_LENGTH: usize = 22;

/// Ответ участнику, открывшему ссылку с кодом.
const MEMBER_BANNER: &str = "Вы уже состоите в пространстве, приглашение не нужно.";

fn new_member(ctx: &Ctx<'_>) -> NewMember {
    NewMember {
        telegram_user_id: ctx.sender.id,
        telegram_chat_id: ctx.chat_id,
        display_name: ctx.sender.display_name.clone(),
        username: ctx.sender.username.clone(),
    }
}

/// Аргумент `/start`, разобранный до ветки «уже участник».
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartArgument<'a> {
    /// Аргумента нет: `/start` или `/start` с нераспознанным префиксом.
    None,
    /// Код приглашения household (16 символов без префикса).
    Invite(&'a str),
    /// Ссылка-claim владельца.
    OwnerClaim(&'a str),
    /// Ссылка привязки администратора.
    AdminLink(&'a str),
    /// Префикс есть, но длина или алфавит кода неверны.
    Invalid,
}

fn parse_start_argument(argument: &str) -> StartArgument<'_> {
    if argument.is_empty() {
        return StartArgument::None;
    }
    if let Some(code) = argument.strip_prefix("own_") {
        return if is_claim_code(code) {
            StartArgument::OwnerClaim(code)
        } else {
            StartArgument::Invalid
        };
    }
    if let Some(code) = argument.strip_prefix("adm_") {
        return if is_claim_code(code) {
            StartArgument::AdminLink(code)
        } else {
            StartArgument::Invalid
        };
    }
    StartArgument::Invite(argument)
}

fn is_claim_code(code: &str) -> bool {
    code.len() == CLAIM_CODE_LENGTH
        && code
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

/// Команда `/start` с необязательным кодом приглашения, claim или привязки.
pub async fn on_start(ctx: &mut Ctx<'_>, argument: &str) -> Result<(), Fail> {
    crate::telegram::views::clear_conversation(&mut *ctx.conn, ctx.chat_id).await?;

    match parse_start_argument(argument) {
        StartArgument::None | StartArgument::Invalid => default_start(ctx).await,
        StartArgument::OwnerClaim(code) => claim_start(ctx, code).await,
        // 5.1 добавит здесь запись кандидата по действующему коду
        // `telegram_link`; до этого префикс только распознаётся, а поведение
        // совпадает с `/start` без аргумента.
        StartArgument::AdminLink(_) => default_start(ctx).await,
        StartArgument::Invite(code) => {
            if ctx.user.is_some() {
                ctx.banner(MEMBER_BANNER);
                return ctx.present(Screen::Menu).await;
            }
            accept_invite(ctx, code).await
        }
    }
}

/// Поведение `/start` без ссылки.
async fn default_start(ctx: &mut Ctx<'_>) -> Result<(), Fail> {
    if ctx.user.is_some() {
        ctx.present(Screen::Menu).await
    } else {
        forms::start_onboarding(ctx).await
    }
}

/// `/start own_…`: назначить владельца или открыть онбординг пустой
/// инсталляции. Недействительный код ведёт себя как `/start` без аргумента.
async fn claim_start(ctx: &mut Ctx<'_>, code: &str) -> Result<(), Fail> {
    if ctx.user.is_some() {
        ctx.banner(MEMBER_BANNER);
        return ctx.present(Screen::Menu).await;
    }

    let Some(record) =
        codes::find_active(&mut *ctx.conn, Purpose::OwnerClaim, code, ctx.now).await?
    else {
        return default_start(ctx).await;
    };

    let Some(household_id) = record.household_id else {
        return claim_empty_installation(ctx, record.id).await;
    };

    // Код мог остаться активным после появления владельца: тогда это обычный
    // `/start` без ссылки.
    if !household_claimable(ctx, household_id).await? {
        return default_start(ctx).await;
    }

    let installation = households::installation(&mut *ctx.conn).await?;
    let admin_exists = admin_accounts::exists(&mut *ctx.conn).await?;
    let contact = classify(
        ctx.tenancy.mode,
        ctx.tenancy.bootstrap_owner,
        ctx.sender.id,
        installation,
        Some(ClaimTarget::Household(household_id)),
        admin_exists,
    );
    if contact != FirstContact::ClaimOwner(household_id) {
        return default_start(ctx).await;
    }

    if !claim_existing_owner(ctx, household_id, Some(code)).await? {
        return default_start(ctx).await;
    }
    Ok(())
}

/// `/start own_…` на пустой инсталляции: код запоминается в форме и гасится
/// только при создании household.
async fn claim_empty_installation(ctx: &mut Ctx<'_>, claim_id: Uuid) -> Result<(), Fail> {
    if ctx.tenancy.mode != TenancyMode::Single {
        return default_start(ctx).await;
    }

    let installation = households::installation(&mut *ctx.conn).await?;
    if installation != Installation::Empty {
        return default_start(ctx).await;
    }
    let admin_exists = admin_accounts::exists(&mut *ctx.conn).await?;
    let contact = classify(
        ctx.tenancy.mode,
        ctx.tenancy.bootstrap_owner,
        ctx.sender.id,
        installation,
        Some(ClaimTarget::EmptyInstallation),
        admin_exists,
    );
    if contact != FirstContact::Onboard {
        return default_start(ctx).await;
    }

    forms::open_onboarding(ctx, Some(claim_id)).await
}

async fn household_claimable(ctx: &mut Ctx<'_>, household_id: Uuid) -> Result<bool, Fail> {
    match households::get(&mut *ctx.conn, household_id).await {
        Ok(_) => Ok(users::owners(&mut *ctx.conn, household_id)
            .await?
            .is_empty()),
        Err(DomainError::NotFound) => Ok(false),
        Err(error) => Err(error.into()),
    }
}

/// Назначить отправителя владельцем существующего household.
///
/// С кодом (`Some`) гашение кода и вставка владельца идут одной транзакцией:
/// одновременный claim даёт ровно одного владельца, а проигравший не расходует
/// код. Без кода этот путь использует `BOOTSTRAP_OWNER_TELEGRAM_ID`.
///
/// `Ok(false)` — состояние изменилось (код уже погашен, владелец появился или
/// отправитель стал участником); вызывающий переклассифицирует состояние.
pub(super) async fn claim_existing_owner(
    ctx: &mut Ctx<'_>,
    household_id: Uuid,
    code: Option<&str>,
) -> Result<bool, Fail> {
    if ctx.user.is_some() {
        ctx.banner(MEMBER_BANNER);
        ctx.present(Screen::Menu).await?;
        return Ok(true);
    }

    let member = new_member(ctx);
    let now = ctx.now;
    let mut savepoint = ctx.conn.begin().await?;
    let result = async {
        if let Some(code) = code {
            let record = codes::use_code(&mut savepoint, Purpose::OwnerClaim, code, now).await?;
            if record.household_id != Some(household_id) {
                return Err(DomainError::NotFound);
            }
        }
        households::claim_owner(
            &mut savepoint,
            household_id,
            member,
            code.map(|_| "claim"),
            now,
        )
        .await
    }
    .await;

    match result {
        Ok((household, user)) => {
            savepoint.commit().await?;
            let text = format!("Вы стали владельцем пространства «{}».", household.name);
            ctx.tz = display_timezone(Some(&user), Some(&household));
            ctx.user = Some(user);
            ctx.household = Some(household);
            ctx.banner(text);
            ctx.present(Screen::Menu).await?;
            Ok(true)
        }
        Err(DomainError::NotFound) | Err(DomainError::Conflict { .. }) => {
            savepoint.rollback().await?;
            Ok(false)
        }
        Err(error) => Err(error.into()),
    }
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

/// Информационный экран первого контакта без формы онбординга.
pub fn first_contact(contact: &FirstContactScreen) -> Ui {
    match contact {
        FirstContactScreen::AskInvite => Ui::new()
            .heading(ru::PRIVATE_SPACE_TITLE)
            .text(ru::ASK_INVITE),
        FirstContactScreen::AdminCommand(telegram_id) => Ui::new()
            .heading(ru::NOT_CONFIGURED_TITLE)
            .text(ru::ADMIN_COMMAND_INTRO)
            .spans(vec![Span::Code(ru::owner_command(*telegram_id))])
            .text(ru::OWNER_NOT_ASSIGNED),
        FirstContactScreen::AdminCommandWithHint(telegram_id) => Ui::new()
            .heading(ru::NOT_CONFIGURED_TITLE)
            .text(ru::ADMIN_COMMAND_INTRO)
            .spans(vec![Span::Code(ru::owner_command(*telegram_id))])
            .text(ru::OWNER_NOT_ASSIGNED)
            .text(ru::ADMIN_LINK_HINT),
        FirstContactScreen::NotConfigured => Ui::new()
            .heading(ru::NOT_CONFIGURED_TITLE)
            .text(ru::NOT_CONFIGURED),
        FirstContactScreen::NeedsAdmin => Ui::new()
            .heading(ru::NEEDS_ADMIN_TITLE)
            .text(ru::NEEDS_ADMIN),
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
pub async fn on_text(
    ctx: &mut Ctx<'_>,
    name: Option<String>,
    claim_id: Option<Uuid>,
    text: &str,
) -> Result<(), Fail> {
    if ctx.user.is_some() {
        return forms::start_onboarding(ctx).await;
    }
    match name {
        None => {
            let trimmed = text.trim();
            if trimmed.is_empty() || trimmed.chars().count() > MAX_NAME {
                ctx.banner("Название — от 1 до 100 символов.");
                return ctx.present(Screen::Form).await;
            }
            forms::put_onboarding(ctx, Some(trimmed.to_owned()), claim_id).await?;
            ctx.present(Screen::Form).await
        }
        Some(name) => match validate_timezone(text) {
            Ok(zone) => create(ctx, &name, &zone, claim_id).await,
            Err(_) => {
                ctx.banner("Не знаю такой часовой пояс. Пример: Europe/Moscow.");
                ctx.present(Screen::Form).await
            }
        },
    }
}

/// Выбор часового пояса кнопкой.
pub async fn choose_zone(ctx: &mut Ctx<'_>, zone: &str) -> Result<(), Fail> {
    if ctx.user.is_some() {
        return forms::start_onboarding(ctx).await;
    }
    let (name, claim_id) =
        match crate::telegram::views::conversation(&mut *ctx.conn, ctx.chat_id, ctx.now).await? {
            crate::telegram::views::Lookup::Active(conversation) => {
                match serde_json::from_value::<super::model::Form>(conversation.data) {
                    Ok(super::model::Form::Onboarding {
                        name: Some(name),
                        claim_id,
                    }) => (name, claim_id),
                    _ => return forms::start_onboarding(ctx).await,
                }
            }
            _ => return forms::start_onboarding(ctx).await,
        };
    let zone = validate_timezone(zone)?;
    create(ctx, &name, &zone, claim_id).await
}

async fn create(
    ctx: &mut Ctx<'_>,
    name: &str,
    zone: &str,
    claim_id: Option<Uuid>,
) -> Result<(), Fail> {
    // Пока форма была открыта, отправителя могли добавить в household.
    // Онбординг больше не нужен: форма закрывается, показывается меню.
    if ctx.user.is_some() {
        return forms::start_onboarding(ctx).await;
    }

    // Код claim мог истечь или быть израсходован, пока форма была открыта.
    // Тогда household не создаётся: экран соответствует текущему состоянию.
    let claim = match claim_id {
        Some(id) => {
            match codes::find_active_by_id(&mut *ctx.conn, Purpose::OwnerClaim, id, ctx.now).await?
            {
                Some(record) if record.household_id.is_none() => Some(id),
                _ => return forms::start_onboarding(ctx).await,
            }
        }
        None => None,
    };

    // Форма могла быть открыта до того, как администратор задал
    // `BOOTSTRAP_OWNER_TELEGRAM_ID` или появился household. Перед вставкой
    // классификатор перепроверяется целиком: не-`Onboard` закрывает форму и
    // показывает экран нового состояния, household не создаётся.
    let installation = households::installation(&mut *ctx.conn).await?;
    let admin_exists = admin_accounts::exists(&mut *ctx.conn).await?;
    let contact = classify(
        ctx.tenancy.mode,
        ctx.tenancy.bootstrap_owner,
        ctx.sender.id,
        installation,
        claim.map(|_| ClaimTarget::EmptyInstallation),
        admin_exists,
    );
    if contact != FirstContact::Onboard {
        // Тот же путь, что при `household_exists`: `start_onboarding` закроет
        // форму онбординга и покажет экран по классификатору.
        return forms::start_onboarding(ctx).await;
    }

    let member = new_member(ctx);
    let now = ctx.now;
    // Последний шаг может проиграть гонку: household успели создать извне.
    // Вложенный savepoint (как в `accept_invite`) гасит конфликт, не трогая
    // savepoint шага `Ctx::in_step`. Состояние перечитывается классификатором.
    let mut savepoint = ctx.conn.begin().await?;
    if let Some(id) = claim {
        if users::find_active_by_telegram(&mut savepoint, member.telegram_user_id)
            .await?
            .is_some()
        {
            savepoint.rollback().await?;
            return forms::start_onboarding(ctx).await;
        }
        // Код гасится в той же транзакции, что и создание household.
        match codes::use_code_by_id(&mut savepoint, Purpose::OwnerClaim, id, now).await {
            Ok(record) if record.household_id.is_none() => {}
            Ok(_) | Err(DomainError::NotFound) => {
                savepoint.rollback().await?;
                return forms::start_onboarding(ctx).await;
            }
            Err(error) => return Err(error.into()),
        }
    }

    let result = households::onboard(
        &mut savepoint,
        ctx.tenancy.mode,
        name,
        zone,
        NewOwner {
            member,
            timezone: Some(zone.to_owned()),
        },
        now,
    )
    .await;
    let onboarded = match result {
        Ok(onboarded) if !onboarded.created => {
            // Отправитель успел стать участником: код возвращается откатом,
            // показывается меню.
            savepoint.rollback().await?;
            ctx.tz = display_timezone(Some(&onboarded.user), Some(&onboarded.household));
            ctx.user = Some(onboarded.user);
            ctx.household = Some(onboarded.household);
            return ctx.present(Screen::Menu).await;
        }
        Ok(onboarded) => {
            if claim.is_some() {
                let actor = Actor::user(onboarded.user.id, onboarded.user.role);
                audit::record(
                    &mut savepoint,
                    Some(onboarded.household.id),
                    &actor,
                    AuditEventType::MemberJoined,
                    json!({
                        "user_id": onboarded.user.id,
                        "display_name": onboarded.user.display_name,
                        "role": onboarded.user.role,
                        "source": "claim",
                    }),
                    now,
                )
                .await?;
            }
            savepoint.commit().await?;
            onboarded
        }
        Err(error) => {
            savepoint.rollback().await?;
            if matches!(
                &error,
                DomainError::Conflict { details, .. }
                    if details.get("reason").and_then(|reason| reason.as_str())
                        == Some("household_exists")
            ) {
                // Форму закроет `start_onboarding`; баннер не нужен: экран сам
                // объясняет новое состояние инсталляции.
                return forms::start_onboarding(ctx).await;
            }
            return Err(error.into());
        }
    };
    forms::finish(ctx).await?;
    ctx.tz = display_timezone(Some(&onboarded.user), Some(&onboarded.household));
    ctx.user = Some(onboarded.user);
    ctx.household = Some(onboarded.household);
    ctx.present(Screen::Welcome).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telegram::rich::Block;

    fn has_text(ui: &Ui, text: &str) -> bool {
        ui.blocks.iter().any(|block| {
            matches!(
                block,
                Block::Paragraph(spans)
                    if spans.iter().any(|span| span == &Span::Text(text.to_owned()))
            )
        })
    }

    #[test]
    fn admin_command_screen_shows_the_link_hint_only_with_an_account() {
        let without = first_contact(&FirstContactScreen::AdminCommand(42));
        assert!(!has_text(&without, ru::ADMIN_LINK_HINT));

        let with = first_contact(&FirstContactScreen::AdminCommandWithHint(42));
        assert!(has_text(&with, ru::ADMIN_LINK_HINT));
    }
}
