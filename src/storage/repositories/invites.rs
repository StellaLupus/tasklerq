//! Приглашения в household.
//!
//! Код — 12 случайных байт в base64url (16 символов). В базе хранится только
//! SHA-256 хеш; сам код возвращается один раз при создании и не попадает ни в
//! журнал, ни в логи.

use chrono::{DateTime, Duration, Utc};
use serde_json::json;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::auth::secrets::random_base64url;
use crate::auth::tokens::hash_token;
use crate::domain::error::DomainError;
use crate::domain::identity::{
    AuditEventType, Invite, Joined, MembershipNotice, NewMember, NoticeKind,
};
use crate::domain::models::{Actor, UserRole};

use super::{audit, households, users};

/// Срок жизни приглашения, часов.
pub const TTL_HOURS: i64 = 72;
/// Максимум активных приглашений в household.
pub const MAX_ACTIVE: i64 = 5;
/// Допустимая длина кода.
const CODE_LENGTH: std::ops::RangeInclusive<usize> = 8..=16;

const COLUMNS: &str = "id, household_id, role, created_by_user_id, expires_at, \
    used_by_user_id, used_at, revoked_at, created_at";

/// Созданное приглашение и открытый код (показывается один раз).
#[derive(Debug, Clone)]
pub struct CreatedInvite {
    /// Запись приглашения.
    pub invite: Invite,
    /// Открытый код.
    pub code: String,
}

/// Deep link вида `https://t.me/<bot>?start=<code>`.
pub fn deep_link(bot_username: &str, code: &str) -> String {
    format!(
        "https://t.me/{}?start={code}",
        bot_username.trim_start_matches('@')
    )
}

/// Создать приглашение. Только владелец; не более пяти активных.
///
/// Лимит проверяется под блокировкой строки household, поэтому параллельные
/// запросы не обходят его. Вызывать внутри транзакции.
pub async fn create(
    conn: &mut PgConnection,
    household_id: Uuid,
    actor: &Actor,
    now: DateTime<Utc>,
) -> Result<CreatedInvite, DomainError> {
    let owner = users::lock_and_require_owner(conn, household_id, actor).await?;

    let active: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM invites
         WHERE household_id = $1 AND used_at IS NULL AND revoked_at IS NULL
           AND expires_at > $2",
    )
    .bind(household_id)
    .bind(now)
    .fetch_one(&mut *conn)
    .await?;
    if active >= MAX_ACTIVE {
        return Err(DomainError::conflict_with(
            format!("at most {MAX_ACTIVE} active invites are allowed"),
            json!({"reason": "invite_limit", "limit": MAX_ACTIVE}),
        ));
    }

    let code = random_base64url(12);
    let invite = sqlx::query_as::<_, Invite>(&format!(
        "INSERT INTO invites (id, household_id, code_hash, role, created_by_user_id,
                              expires_at, created_at)
         VALUES ($1, $2, $3, 'member', $4, $5, $6)
         RETURNING {COLUMNS}"
    ))
    .bind(Uuid::now_v7())
    .bind(household_id)
    .bind(hash_token(&code))
    .bind(owner.id)
    .bind(now + Duration::hours(TTL_HOURS))
    .bind(now)
    .fetch_one(&mut *conn)
    .await?;

    audit::record(
        conn,
        Some(household_id),
        actor,
        AuditEventType::InviteCreated,
        json!({"invite_id": invite.id, "expires_at": invite.expires_at}),
        now,
    )
    .await?;

    Ok(CreatedInvite { invite, code })
}

/// Активные приглашения household. Только владелец.
pub async fn list_active(
    conn: &mut PgConnection,
    household_id: Uuid,
    actor: &Actor,
    now: DateTime<Utc>,
) -> Result<Vec<Invite>, DomainError> {
    users::require_current_owner(conn, household_id, actor).await?;
    let invites = sqlx::query_as::<_, Invite>(&format!(
        "SELECT {COLUMNS} FROM invites
         WHERE household_id = $1 AND used_at IS NULL AND revoked_at IS NULL
           AND expires_at > $2
         ORDER BY created_at, id"
    ))
    .bind(household_id)
    .bind(now)
    .fetch_all(conn)
    .await?;

    Ok(invites)
}

/// Отозвать приглашение до использования. Только владелец.
///
/// Повторный отзыв идемпотентен. Использованное приглашение отозвать нельзя.
pub async fn revoke(
    conn: &mut PgConnection,
    household_id: Uuid,
    actor: &Actor,
    invite_id: Uuid,
    now: DateTime<Utc>,
) -> Result<Invite, DomainError> {
    users::lock_and_require_owner(conn, household_id, actor).await?;

    let invite = sqlx::query_as::<_, Invite>(&format!(
        "SELECT {COLUMNS} FROM invites
         WHERE id = $1 AND household_id = $2
         FOR UPDATE"
    ))
    .bind(invite_id)
    .bind(household_id)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(DomainError::NotFound)?;

    if invite.used_at.is_some() {
        return Err(DomainError::conflict_with(
            "invite has already been used",
            json!({"reason": "used"}),
        ));
    }
    if invite.revoked_at.is_some() {
        return Ok(invite);
    }

    let revoked = sqlx::query_as::<_, Invite>(&format!(
        "UPDATE invites SET revoked_at = $2 WHERE id = $1 RETURNING {COLUMNS}"
    ))
    .bind(invite_id)
    .bind(now)
    .fetch_one(&mut *conn)
    .await?;

    audit::record(
        conn,
        Some(household_id),
        actor,
        AuditEventType::InviteRevoked,
        json!({"invite_id": invite_id}),
        now,
    )
    .await?;

    Ok(revoked)
}

/// Отозвать все активные приглашения household (удаление household).
pub(crate) async fn revoke_all(
    conn: &mut PgConnection,
    household_id: Uuid,
    now: DateTime<Utc>,
) -> Result<u64, DomainError> {
    let result = sqlx::query(
        "UPDATE invites SET revoked_at = $2
         WHERE household_id = $1 AND used_at IS NULL AND revoked_at IS NULL",
    )
    .bind(household_id)
    .bind(now)
    .execute(conn)
    .await?;

    Ok(result.rows_affected())
}

/// Отозвать активные приглашения, созданные пользователем.
pub(crate) async fn revoke_created_by(
    conn: &mut PgConnection,
    household_id: Uuid,
    user_id: Uuid,
    now: DateTime<Utc>,
) -> Result<u64, DomainError> {
    let result = sqlx::query(
        "UPDATE invites SET revoked_at = $3
         WHERE household_id = $1 AND created_by_user_id = $2
           AND used_at IS NULL AND revoked_at IS NULL",
    )
    .bind(household_id)
    .bind(user_id)
    .bind(now)
    .execute(conn)
    .await?;

    Ok(result.rows_affected())
}

/// Присоединиться к household по коду.
///
/// Неизвестный код — `NotFound`. Использованный, отозванный и истёкший коды —
/// `Conflict` с `details.reason` (`used`/`revoked`/`expired`), чтобы бот мог
/// ответить понятно; пользователь при этом не создаётся. Пользователь, уже
/// состоящий в household, получает `already_member`. Вызывать внутри
/// транзакции и откатывать её при любой ошибке.
pub async fn accept(
    conn: &mut PgConnection,
    code: &str,
    joiner: NewMember,
    now: DateTime<Utc>,
) -> Result<Joined, DomainError> {
    let code = code.trim();
    if !CODE_LENGTH.contains(&code.len()) {
        return Err(DomainError::NotFound);
    }
    users::lock_telegram_user(conn, joiner.telegram_user_id).await?;

    // Порядок блокировок household -> приглашение совпадает с удалением
    // household, поэтому параллельные операции не взаимоблокируются.
    let code_hash = hash_token(code);
    let household_id: Uuid =
        sqlx::query_scalar("SELECT household_id FROM invites WHERE code_hash = $1")
            .bind(&code_hash)
            .fetch_optional(&mut *conn)
            .await?
            .ok_or(DomainError::NotFound)?;
    // Приглашение в удалённый household не существует для пользователя.
    users::lock_household(conn, household_id).await?;

    let invite = sqlx::query_as::<_, Invite>(&format!(
        "SELECT {COLUMNS} FROM invites WHERE code_hash = $1 FOR UPDATE"
    ))
    .bind(&code_hash)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(DomainError::NotFound)?;

    if invite.used_at.is_some() {
        return Err(unusable("used", "invite has already been used"));
    }
    if invite.revoked_at.is_some() {
        return Err(unusable("revoked", "invite has been revoked"));
    }
    if invite.expires_at <= now {
        return Err(unusable("expired", "invite has expired"));
    }

    if users::find_active_by_telegram(conn, joiner.telegram_user_id)
        .await?
        .is_some()
    {
        return Err(DomainError::conflict_with(
            "user already belongs to a household",
            json!({"reason": "already_member"}),
        ));
    }

    let user = users::insert(
        conn,
        invite.household_id,
        UserRole::Member,
        &joiner,
        None,
        now,
    )
    .await?;

    sqlx::query("UPDATE invites SET used_by_user_id = $2, used_at = $3 WHERE id = $1")
        .bind(invite.id)
        .bind(user.id)
        .bind(now)
        .execute(&mut *conn)
        .await?;

    let actor = Actor::user(user.id, UserRole::Member);
    audit::record(
        conn,
        Some(invite.household_id),
        &actor,
        AuditEventType::InviteUsed,
        json!({"invite_id": invite.id, "actor_name": user.display_name}),
        now,
    )
    .await?;
    audit::record(
        conn,
        Some(invite.household_id),
        &actor,
        AuditEventType::MemberJoined,
        json!({
            "user_id": user.id,
            "display_name": user.display_name,
            "role": "member",
        }),
        now,
    )
    .await?;

    let notices = users::owners(conn, invite.household_id)
        .await?
        .into_iter()
        .map(|owner| MembershipNotice {
            recipient_user_id: owner.id,
            chat_id: owner.telegram_chat_id,
            kind: NoticeKind::MemberJoined {
                display_name: user.display_name.clone(),
            },
        })
        .collect();

    let household = households::get(conn, invite.household_id).await?;
    Ok(Joined {
        household,
        user,
        notices,
    })
}

fn unusable(reason: &str, message: &str) -> DomainError {
    DomainError::conflict_with(message, json!({"reason": reason}))
}
