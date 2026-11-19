//! Привязка Telegram к учётной записи администратора.
//!
//! Ссылка `adm_` выпускается в консоли и живёт 15 минут. Открывший её аккаунт
//! становится кандидатом; привязка вступает в силу только после подтверждения
//! в консоли. Подтверждение переносит кандидата в `telegram_user_id`, пишет
//! аудит и ставит в outbox сообщение боту этого аккаунта — одной транзакцией.
//!
//! Выпуск ссылки в аудит не пишется: отдельного события для него нет, как и у
//! ссылки-claim владельца.

use chrono::{DateTime, Utc};
use serde_json::json;

use crate::domain::error::DomainError;
use crate::domain::identity::AuditEventType;
use crate::domain::models::{Actor, ActorType};
use crate::storage::repositories::{admin_accounts, audit};
use crate::telegram::outbox::{self, Outgoing};
use crate::telegram::rich;

use super::codes::{self, Purpose};
use super::owner;
use super::router::AdminState;

/// Путь выпуска ссылки привязки.
pub(crate) const TELEGRAM_LINK_PATH: &str = "/admin/telegram-link";
/// Путь подтверждения кандидата.
pub(crate) const TELEGRAM_LINK_CONFIRM_PATH: &str = "/admin/telegram-link/confirm";
/// Путь отклонения кандидата.
pub(crate) const TELEGRAM_LINK_REJECT_PATH: &str = "/admin/telegram-link/reject";
/// Путь отвязки подтверждённого аккаунта.
pub(crate) const TELEGRAM_LINK_UNLINK_PATH: &str = "/admin/telegram-link/unlink";

/// Сообщение боту после подтверждения привязки.
const LINKED_MESSAGE: &str = "Аккаунт привязан к консоли администратора.";

/// Выпущенная ссылка: открытый код показывается один раз.
#[derive(Debug)]
pub(crate) struct IssuedTelegramLink {
    /// Ссылка `https://t.me/<бот>?start=adm_<код>`.
    pub(crate) link: String,
    /// Срок действия кода.
    pub(crate) expires_at: DateTime<Utc>,
}

/// Итог выпуска.
pub(crate) enum IssueOutcome {
    /// Ссылка выпущена.
    Issued(IssuedTelegramLink),
    /// Состояние не позволяет выпуск; текст без секретов.
    Rejected(String),
}

/// Выпустить ссылку привязки для единственной учётной записи.
///
/// Прежний код назначения гасится, ожидающий кандидат сбрасывается: новая
/// ссылка — новая попытка. Уже привязанная учётная запись ссылку не получает:
/// кандидата нельзя было бы подтвердить, и он остался бы скрытым. Подходящего
/// события аудита для выпуска нет, поэтому журнал не пишется.
pub(crate) async fn issue(
    state: &AdminState,
    now: DateTime<Utc>,
) -> Result<IssueOutcome, DomainError> {
    let (telegram_ready, username) = owner::telegram_bot(state).await?;
    if !telegram_ready {
        return Ok(IssueOutcome::Rejected(
            "Сначала подключите бота на странице «Telegram».".to_owned(),
        ));
    }

    let mut tx = state.pool.begin().await?;
    let linked = admin_accounts::get(&mut tx)
        .await?
        .and_then(|account| account.telegram_user_id);
    if linked.is_some() {
        tx.rollback().await?;
        return Ok(IssueOutcome::Rejected(
            "Аккаунт уже привязан, сначала отвяжите его.".to_owned(),
        ));
    }
    let issued = codes::issue_locked(&mut tx, Purpose::TelegramLink, None, now).await?;
    admin_accounts::reset_pending_telegram(&mut tx).await?;
    tx.commit().await?;

    let expires_at = issued
        .record
        .expires_at
        .expect("a telegram_link code always has an expiry");
    let link = format!(
        "https://t.me/{}?start=adm_{}",
        username.unwrap_or_default(),
        issued.code
    );

    Ok(IssueOutcome::Issued(IssuedTelegramLink {
        link,
        expires_at,
    }))
}

/// Итог обработки кандидата или привязки.
pub(crate) enum BindingOutcome {
    /// Изменение применено.
    Applied,
    /// Кандидат уже сменился или исчез; ничего не применено.
    Stale,
    /// Привязки не было; ничего не применено.
    NotLinked,
}

/// Подтвердить кандидата: перенести его в привязку, записать аудит и outbox.
pub(crate) async fn confirm(
    state: &AdminState,
    candidate: i64,
    now: DateTime<Utc>,
) -> Result<BindingOutcome, DomainError> {
    let mut tx = state.pool.begin().await?;
    let Some(confirmed) = admin_accounts::confirm_pending_telegram(&mut tx, candidate).await?
    else {
        tx.rollback().await?;
        return Ok(BindingOutcome::Stale);
    };

    audit::record(
        &mut tx,
        None,
        &Actor::system(ActorType::System),
        AuditEventType::AdminTelegramLinked,
        json!({ "telegram_user_id": confirmed.telegram_user_id }),
        now,
    )
    .await?;
    outbox::enqueue(
        &mut tx,
        confirmed.telegram_user_id,
        &Outgoing::Message {
            html: rich::escape(LINKED_MESSAGE),
            rich: false,
            delete_after: None,
            disable_preview: false,
        },
        None,
        now,
    )
    .await?;
    tx.commit().await?;

    Ok(BindingOutcome::Applied)
}

/// Отклонить кандидата без аудита и сообщения.
pub(crate) async fn reject(
    state: &AdminState,
    candidate: i64,
) -> Result<BindingOutcome, DomainError> {
    let mut tx = state.pool.begin().await?;
    if !admin_accounts::reject_pending_telegram(&mut tx, candidate).await? {
        tx.rollback().await?;
        return Ok(BindingOutcome::Stale);
    }
    tx.commit().await?;

    Ok(BindingOutcome::Applied)
}

/// Снять подтверждённую привязку и записать аудит.
pub(crate) async fn unlink(
    state: &AdminState,
    now: DateTime<Utc>,
) -> Result<BindingOutcome, DomainError> {
    let mut tx = state.pool.begin().await?;
    if !admin_accounts::unlink_telegram(&mut tx).await? {
        tx.rollback().await?;
        return Ok(BindingOutcome::NotLinked);
    }

    audit::record(
        &mut tx,
        None,
        &Actor::system(ActorType::System),
        AuditEventType::AdminTelegramUnlinked,
        json!({}),
        now,
    )
    .await?;
    tx.commit().await?;

    Ok(BindingOutcome::Applied)
}
