//! Команда `/admin`: одноразовая ссылка восстановления доступа к консоли.
//!
//! Команда доступна только подтверждённой привязке Telegram к учётной записи
//! администратора (`admin_accounts.telegram_user_id`). Для остальных она
//! обрабатывается как неизвестная. Ссылка строится на `console_origin`,
//! который консоль запоминает при входе; без него подсказывается CLI.
//!
//! Код, сообщение в outbox и событие аудита пишутся в транзакции вызывающего:
//! либо всё, либо ничего. Ни код, ни ссылка в журнал не попадают.

use serde_json::json;

use crate::admin_console::codes::{self, Purpose, RECOVERY_TTL_MINUTES};
use crate::domain::identity::AuditEventType;
use crate::domain::models::{Actor, ActorType};
use crate::security_audit::{ADMIN_TELEGRAM_RECOVERY_LIMIT, LimitKey, RateLimitEvent};
use crate::storage::repositories::{admin_accounts, audit};
use crate::telegram::outbox::{self, Outgoing};
use crate::telegram::rich;

use super::{Ctx, Fail, unknown_command};

/// Сколько раз привязанный аккаунт может запросить ссылку за час.
pub const RECOVERY_REQUESTS_PER_HOUR: u32 = 3;
/// Окно лимита запросов восстановления, секунды.
pub const RECOVERY_WINDOW_SECONDS: i64 = 3_600;
/// Сколько живёт сообщение со ссылкой: столько же, сколько действует код.
pub const RECOVERY_MESSAGE_TTL_SECONDS: u32 = RECOVERY_TTL_MINUTES as u32 * 60;

/// Ответ, когда консоль ещё ни разу не открывалась.
const ORIGIN_UNKNOWN: &str = "Адрес консоли ещё неизвестен: консоль ни разу не открывалась \
     после обновления. Откройте консоль в браузере или выполните \
     tasklerq admin reset-password на сервере.";

/// Ответ на четвёртый запрос за час.
const TOO_OFTEN: &str = "Слишком часто, попробуйте позже.";

/// `/admin`: ссылка восстановления привязанному аккаунту.
pub(super) async fn on_admin(ctx: &mut Ctx<'_>) -> Result<(), Fail> {
    let telegram_user_id = ctx.sender.id;
    let Some(account) = admin_accounts::find_by_telegram(&mut *ctx.conn, telegram_user_id).await?
    else {
        return unknown_command(ctx).await;
    };

    let Some(origin) = account
        .console_origin
        .as_deref()
        .map(str::trim)
        .map(|origin| origin.trim_end_matches('/'))
        .filter(|origin| !origin.is_empty())
    else {
        ctx.say(ORIGIN_UNKNOWN).await?;
        return Ok(());
    };

    // Запросы без адреса консоли в лимит не входят: до этой проверки они не
    // доходят. Остальные считаются на аккаунт администратора.
    let decision = ctx.recovery_limit.check(telegram_user_id, ctx.now);
    if !decision.allowed {
        record_rate_limit(ctx, telegram_user_id, decision.retry_after).await;
        ctx.say(TOO_OFTEN).await?;
        return Ok(());
    }

    let issued = codes::issue_locked(&mut *ctx.conn, Purpose::Recovery, None, ctx.now).await?;
    let link = format!("{origin}/admin/recover/{}", issued.code);
    let escaped = rich::escape(&link);
    let html = format!(
        "Ссылка для восстановления доступа к консоли:\n<a href=\"{escaped}\">{escaped}</a>\n\n\
         Действует {RECOVERY_TTL_MINUTES} минут; сообщение удалится само."
    );
    // Событие фиксирует только выпуск: ни кода, ни ссылки в payload нет.
    audit::record(
        &mut *ctx.conn,
        None,
        &Actor::system(ActorType::System),
        AuditEventType::AdminRecoveryIssued,
        json!({}),
        ctx.now,
    )
    .await?;
    outbox::enqueue(
        &mut *ctx.conn,
        ctx.chat_id,
        &Outgoing::Message {
            html,
            rich: false,
            delete_after: Some(RECOVERY_MESSAGE_TTL_SECONDS),
            disable_preview: true,
        },
        None,
        ctx.now,
    )
    .await?;
    Ok(())
}

/// Записать отказ по лимиту отдельным соединением: отказ должен остаться в
/// журнале, даже если транзакция обновления откатится.
async fn record_rate_limit(ctx: &Ctx<'_>, telegram_user_id: i64, retry_after: Option<u64>) {
    let Some(audit) = ctx.audit else {
        return;
    };
    let retry_after = retry_after.unwrap_or(RECOVERY_WINDOW_SECONDS as u64);
    audit
        .rate_limited(
            RateLimitEvent {
                limit: ADMIN_TELEGRAM_RECOVERY_LIMIT,
                key: LimitKey::User(telegram_user_id),
                retry_after,
                household_id: None,
                actor: Actor::system(ActorType::System),
            },
            ctx.now,
        )
        .await;
}
