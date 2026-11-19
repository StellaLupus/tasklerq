//! Ссылка-claim владельца: цели на главной, выпуск и QR-код.
//!
//! Ссылка выпускается для активного household без владельца, а в режиме
//! `single` — ещё и для пустой инсталляции (код без `household_id`). Открытое
//! значение кода возвращается один раз: в базе хранится только хеш, а страница
//! результата не сохраняется и при повторном открытии недоступна.
//!
//! QR-код рисуется на сервере крейтом `qrcode`; ссылка не уходит сторонним
//! сервисам.

use chrono::{DateTime, Utc};
use qrcode::QrCode;
use qrcode::render::svg;
use serde::Deserialize;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::config::TenancyMode;
use crate::domain::error::DomainError;
use crate::settings;
use crate::storage::repositories::{households, users};

use super::codes::{self, Purpose};
use super::router::AdminState;

/// Путь выпуска ссылки-claim.
pub(crate) const OWNER_PATH: &str = "/admin/owner";
/// Значение цели «пустая инсталляция».
pub(crate) const EMPTY_TARGET: &str = "empty";

/// Поля формы выпуска.
#[derive(Clone, Debug, Default, Deserialize)]
pub(crate) struct OwnerLinkForm {
    /// Идентификатор household или [`EMPTY_TARGET`].
    #[serde(default)]
    pub(crate) target: String,
}

/// Одна цель выпуска.
#[derive(Clone, Debug)]
pub(crate) struct OwnerTarget {
    /// Значение скрытого поля формы.
    pub(crate) value: String,
    /// Подпись цели без служебных данных.
    pub(crate) label: String,
}

/// Цели выпуска и готовность Telegram.
#[derive(Debug)]
pub(crate) struct OwnerTargets {
    /// Household без владельца и пустая инсталляция в `single`.
    pub(crate) targets: Vec<OwnerTarget>,
    /// Бот включён и его имя известно: ссылку можно выпускать.
    pub(crate) telegram_ready: bool,
}

/// Выпущенная ссылка: открытый код показывается один раз.
#[derive(Debug)]
pub(crate) struct IssuedOwnerLink {
    /// Ссылка `https://t.me/<бот>?start=own_<код>`.
    pub(crate) link: String,
    /// Срок действия кода.
    pub(crate) expires_at: DateTime<Utc>,
    /// SVG QR-кода, уже сгенерированный сервером.
    pub(crate) svg: String,
}

/// Итог выпуска.
pub(crate) enum OwnerIssueOutcome {
    /// Ссылка выпущена.
    Issued(IssuedOwnerLink),
    /// Цель или состояние не позволяют выпуск; текст без секретов.
    Rejected(String),
}

/// Собрать цели выпуска.
///
/// Список household читается одним запросом: активные дома без активного
/// владельца. Пустая инсталляция — цель только в `single` и только когда
/// активных household нет.
pub(crate) async fn targets(state: &AdminState) -> Result<OwnerTargets, DomainError> {
    let (telegram_ready, _) = telegram_bot(state).await?;
    let mut conn = state.pool.acquire().await?;
    let households = households::list_active_without_owner(&mut conn).await?;
    let empty_allowed = state.wizard().tenancy == TenancyMode::Single
        && households::count_active(&mut conn).await? == 0;
    drop(conn);

    let mut targets: Vec<OwnerTarget> = households
        .into_iter()
        .map(|household| OwnerTarget {
            value: household.id.to_string(),
            label: household.name,
        })
        .collect();
    if empty_allowed {
        targets.push(OwnerTarget {
            value: EMPTY_TARGET.to_owned(),
            label: "Пустая инсталляция".to_owned(),
        });
    }

    Ok(OwnerTargets {
        targets,
        telegram_ready,
    })
}

/// Выпустить ссылку-claim для цели формы.
///
/// Сначала проверяется готовность Telegram и существование household. Выпуск
/// кода и проверка владельца (или пустой инсталляции) идут одной транзакцией:
/// недействительная цель откатывает и новый код, и гашение прежнего. Подходящего
/// значения `audit_event_type` для выпуска claim-ссылки нет, поэтому отдельное
/// событие не пишется: `admin_recovery_issued` относится к восстановлению
/// доступа, а не к назначению владельца.
pub(crate) async fn issue(
    state: &AdminState,
    target: &str,
    now: DateTime<Utc>,
) -> Result<OwnerIssueOutcome, DomainError> {
    let (telegram_ready, username) = telegram_bot(state).await?;
    if !telegram_ready {
        return Ok(OwnerIssueOutcome::Rejected(
            "Сначала подключите бота на странице «Telegram».".to_owned(),
        ));
    }

    let household_id = match parse_target(target) {
        Ok(household_id) => household_id,
        Err(message) => return Ok(OwnerIssueOutcome::Rejected(message)),
    };

    // Существование проверяется до вставки кода: у `household_id` есть внешний
    // ключ, и неизвестный id иначе превратился бы в ошибку базы. Владелец
    // проверяется уже в транзакции выпуска.
    if let Some(household_id) = household_id {
        let mut conn = state.pool.acquire().await?;
        match households::get(&mut conn, household_id).await {
            Ok(_) => {}
            Err(DomainError::NotFound) => {
                return Ok(OwnerIssueOutcome::Rejected(
                    "Household не найден.".to_owned(),
                ));
            }
            Err(error) => return Err(error),
        }
    }

    // Порядок блокировок повторяет claim: коды, затем household. Выпуск идёт
    // до проверки цели, но при недействительной цели транзакция откатывается,
    // поэтому новый код не остаётся, а прежний не гаснет.
    let mut tx = state.pool.begin().await?;
    let issued = codes::issue_locked(&mut tx, Purpose::OwnerClaim, household_id, now).await?;
    if let Err(message) = validate_target(&mut tx, household_id, state.wizard().tenancy).await? {
        tx.rollback().await?;
        return Ok(OwnerIssueOutcome::Rejected(message));
    }
    tx.commit().await?;
    let outstanding = issued
        .record
        .expires_at
        .expect("an owner_claim code always has an expiry");
    let link = format!(
        "https://t.me/{}?start=own_{}",
        username.unwrap_or_default(),
        issued.code
    );
    let svg = qr_svg(&link)?;

    Ok(OwnerIssueOutcome::Issued(IssuedOwnerLink {
        link,
        expires_at: outstanding,
        svg,
    }))
}

/// Готовность Telegram и имя бота без `@`.
pub(crate) async fn telegram_bot(
    state: &AdminState,
) -> Result<(bool, Option<String>), DomainError> {
    let Some(config) = state.telegram_config() else {
        return Ok((false, None));
    };
    let effective =
        settings::read_effective_telegram(&state.pool, config, state.llm_settings().cipher())
            .await?;
    let username = effective
        .config()
        .map(|config| config.bot_username.trim_start_matches('@').to_owned())
        .filter(|username| !username.is_empty());
    Ok((effective.is_enabled() && username.is_some(), username))
}

/// Разобрать цель формы: `None` — пустая инсталляция.
fn parse_target(target: &str) -> Result<Option<Uuid>, String> {
    let target = target.trim();
    if target == EMPTY_TARGET {
        return Ok(None);
    }
    Uuid::parse_str(target)
        .map(Some)
        .map_err(|_| "Неизвестная цель ссылки.".to_owned())
}

/// Проверить цель внутри транзакции выпуска.
async fn validate_target(
    conn: &mut PgConnection,
    household_id: Option<Uuid>,
    tenancy: TenancyMode,
) -> Result<Result<(), String>, DomainError> {
    let Some(household_id) = household_id else {
        if tenancy != TenancyMode::Single {
            return Ok(Err(
                "В режиме multi выберите household из списка.".to_owned()
            ));
        }
        return match households::lock_installation(conn, TenancyMode::Single).await {
            Ok(()) => Ok(Ok(())),
            Err(DomainError::Conflict { .. }) => Ok(Err("Инсталляция уже не пуста.".to_owned())),
            Err(error) => Err(error),
        };
    };

    match users::lock_household(conn, household_id).await {
        Ok(()) => {}
        Err(DomainError::NotFound) => return Ok(Err("Household не найден.".to_owned())),
        Err(error) => return Err(error),
    }
    if !users::owners(conn, household_id).await?.is_empty() {
        return Ok(Err("У household уже есть владелец.".to_owned()));
    }
    Ok(Ok(()))
}

/// Отрисовать QR-код ссылки в SVG без XML-декларации.
///
/// Декларация нужна отдельному `.svg`-файлу, а в HTML инлайн она лишняя:
/// браузер принял бы её за посторонний узел. Сама разметка остаётся
/// сгенерированной крейтом.
fn qr_svg(link: &str) -> Result<String, DomainError> {
    let code = QrCode::new(link.as_bytes())
        .map_err(|error| DomainError::Validation(format!("qr code: {error}")))?;
    let svg = code.render::<svg::Color>().min_dimensions(200, 200).build();
    let svg = svg
        .strip_prefix(r#"<?xml version="1.0" standalone="yes"?>"#)
        .unwrap_or(&svg);
    Ok(svg.to_owned())
}
