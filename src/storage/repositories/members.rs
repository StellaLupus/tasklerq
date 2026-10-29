//! Состав household: удаление участника, выход, передача владения и удаление
//! household.
//!
//! Все операции требуют транзакции: они меняют несколько таблиц, и частичное
//! применение оставило бы household без владельца или с висящими назначениями.
//! Функции возвращают уведомления; отправляет их Telegram-слой.

use chrono::{DateTime, Utc};
use serde_json::json;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::auth::access::require_user;
use crate::domain::error::DomainError;
use crate::domain::identity::{AuditEventType, MembershipChange, MembershipNotice, NoticeKind};
use crate::domain::models::{Actor, EventType, User, UserRole};

use super::{audit, events, invites, tokens, users};

/// Удалить участника из household. Только владелец.
///
/// Доступ отзывается немедленно: пользователь помечается удалённым, его
/// токены и приглашения отзываются, назначенные на него задачи теряют
/// исполнителя. Созданные им задачи остаются.
pub async fn remove(
    conn: &mut PgConnection,
    household_id: Uuid,
    actor: &Actor,
    target_id: Uuid,
    now: DateTime<Utc>,
) -> Result<MembershipChange, DomainError> {
    let owner = users::lock_and_require_owner(conn, household_id, actor).await?;

    if target_id == owner.id {
        return Err(DomainError::conflict_with(
            "the owner cannot remove themselves; transfer ownership or delete the household",
            json!({"reason": "owner_cannot_be_removed"}),
        ));
    }
    let target = users::get(conn, household_id, target_id).await?;

    let unassigned_items = detach(conn, household_id, &target, actor, now).await?;
    audit::record(
        conn,
        Some(household_id),
        actor,
        AuditEventType::MemberRemoved,
        json!({
            "user_id": target.id,
            "display_name": target.display_name,
            "actor_name": owner.display_name,
        }),
        now,
    )
    .await?;

    Ok(MembershipChange {
        notices: vec![MembershipNotice {
            recipient_user_id: target.id,
            chat_id: target.telegram_chat_id,
            kind: NoticeKind::AccessRevoked,
        }],
        unassigned_items,
    })
}

/// Выйти из household.
///
/// Владелец не может выйти: сначала он передаёт владение, а если он
/// единственный участник — удаляет household или приглашает других.
pub async fn leave(
    conn: &mut PgConnection,
    household_id: Uuid,
    actor: &Actor,
    now: DateTime<Utc>,
) -> Result<MembershipChange, DomainError> {
    let user_id = require_user(actor)?;
    users::lock_household(conn, household_id).await?;
    let user = users::get(conn, household_id, user_id).await?;

    if user.role == UserRole::Owner {
        let members = users::count_active(conn, household_id).await?;
        let (reason, message) = if members <= 1 {
            (
                "sole_owner",
                "the only member cannot leave; delete the household or invite someone first",
            )
        } else {
            (
                "transfer_required",
                "the owner must transfer ownership before leaving",
            )
        };
        return Err(DomainError::conflict_with(
            message,
            json!({"reason": reason}),
        ));
    }

    let unassigned_items = detach(conn, household_id, &user, actor, now).await?;
    audit::record(
        conn,
        Some(household_id),
        actor,
        AuditEventType::MemberLeft,
        json!({
            "user_id": user.id,
            "display_name": user.display_name,
        }),
        now,
    )
    .await?;

    let notices = users::owners(conn, household_id)
        .await?
        .into_iter()
        .map(|owner| MembershipNotice {
            recipient_user_id: owner.id,
            chat_id: owner.telegram_chat_id,
            kind: NoticeKind::MemberLeft {
                display_name: user.display_name.clone(),
            },
        })
        .collect();

    Ok(MembershipChange {
        notices,
        unassigned_items,
    })
}

/// Передать владение другому участнику. Только владелец.
///
/// Роли меняются в одной транзакции: сначала прежний владелец становится
/// участником, затем новый получает владение, чтобы уникальный индекс
/// «один владелец» не срабатывал.
pub async fn transfer_ownership(
    conn: &mut PgConnection,
    household_id: Uuid,
    actor: &Actor,
    new_owner_id: Uuid,
    now: DateTime<Utc>,
) -> Result<MembershipChange, DomainError> {
    let old_owner = users::lock_and_require_owner(conn, household_id, actor).await?;
    if new_owner_id == old_owner.id {
        return Err(DomainError::Validation(
            "ownership must be transferred to another member".into(),
        ));
    }
    let new_owner = users::get(conn, household_id, new_owner_id).await?;

    users::set_role(conn, household_id, old_owner.id, UserRole::Member, now).await?;
    users::set_role(conn, household_id, new_owner.id, UserRole::Owner, now).await?;

    for (user, from, to) in [
        (&old_owner, "owner", "member"),
        (&new_owner, "member", "owner"),
    ] {
        audit::record(
            conn,
            Some(household_id),
            actor,
            AuditEventType::RoleChanged,
            json!({
                "user_id": user.id,
                "display_name": user.display_name,
                "from": from,
                "to": to,
                "actor_name": old_owner.display_name,
            }),
            now,
        )
        .await?;
    }

    Ok(MembershipChange {
        notices: vec![MembershipNotice {
            recipient_user_id: new_owner.id,
            chat_id: new_owner.telegram_chat_id,
            kind: NoticeKind::OwnershipReceived,
        }],
        unassigned_items: 0,
    })
}

/// Удалить household. Только владелец, с подтверждением названием.
///
/// Удаление мягкое: household и участники помечаются удалёнными, токены и
/// приглашения отзываются сразу. Физически данные удаляет retention через
/// `RETENTION_DELETED_DAYS`. Участники (кроме владельца) получают уведомление.
pub async fn delete_household(
    conn: &mut PgConnection,
    household_id: Uuid,
    actor: &Actor,
    confirmation: &str,
    now: DateTime<Utc>,
) -> Result<MembershipChange, DomainError> {
    let owner = users::lock_and_require_owner(conn, household_id, actor).await?;

    let name: String = sqlx::query_scalar("SELECT name FROM households WHERE id = $1")
        .bind(household_id)
        .fetch_one(&mut *conn)
        .await?;
    if confirmation.trim() != name {
        return Err(DomainError::Validation(
            "confirmation must match the household name".into(),
        ));
    }

    let members = users::list_active(conn, household_id).await?;

    sqlx::query("UPDATE households SET deleted_at = $2, updated_at = $2 WHERE id = $1")
        .bind(household_id)
        .bind(now)
        .execute(&mut *conn)
        .await?;
    sqlx::query(
        "UPDATE users SET deleted_at = $2, updated_at = $2
         WHERE household_id = $1 AND deleted_at IS NULL",
    )
    .bind(household_id)
    .bind(now)
    .execute(&mut *conn)
    .await?;
    let revoked_tokens = tokens::revoke_all(conn, household_id, now).await?;
    let revoked_invites = invites::revoke_all(conn, household_id, now).await?;

    audit::record(
        conn,
        Some(household_id),
        actor,
        AuditEventType::HouseholdDeleted,
        json!({
            "household_name": name,
            "members": members.len(),
            "revoked_tokens": revoked_tokens,
            "revoked_invites": revoked_invites,
            "actor_name": owner.display_name,
        }),
        now,
    )
    .await?;

    let notices = members
        .into_iter()
        .filter(|member| member.id != owner.id)
        .map(|member| MembershipNotice {
            recipient_user_id: member.id,
            chat_id: member.telegram_chat_id,
            kind: NoticeKind::HouseholdDeleted {
                household_name: name.clone(),
            },
        })
        .collect();

    Ok(MembershipChange {
        notices,
        unassigned_items: 0,
    })
}

/// Снять доступ пользователя: пометить удалённым, отозвать его приглашения и
/// токены, снять назначения. Возвращает число задач, потерявших исполнителя.
///
/// Токены, выпущенные участником, отзываются вместе с доступом: человек мог
/// сохранить их значение. Владелец перевыпустит нужные интеграции.
async fn detach(
    conn: &mut PgConnection,
    household_id: Uuid,
    user: &User,
    actor: &Actor,
    now: DateTime<Utc>,
) -> Result<u64, DomainError> {
    sqlx::query("UPDATE users SET deleted_at = $2, updated_at = $2 WHERE id = $1")
        .bind(user.id)
        .bind(now)
        .execute(&mut *conn)
        .await?;

    invites::revoke_created_by(conn, household_id, user.id, now).await?;
    tokens::revoke_created_by(conn, household_id, user.id, now).await?;

    let rows: Vec<(Uuid, i32, Option<DateTime<Utc>>)> = sqlx::query_as(
        "UPDATE items
         SET assignee_user_id = NULL, version = version + 1, updated_at = $3
         WHERE household_id = $1 AND assignee_user_id = $2
         RETURNING id, version, deleted_at",
    )
    .bind(household_id)
    .bind(user.id)
    .bind(now)
    .fetch_all(&mut *conn)
    .await?;

    let mut unassigned = 0;
    for (item_id, version, deleted_at) in rows {
        if deleted_at.is_some() {
            continue;
        }
        unassigned += 1;
        events::insert(
            conn,
            household_id,
            item_id,
            actor,
            EventType::Unassigned,
            json!({
                "reason": "member_removed",
                "user_id": user.id,
                "version": version,
            }),
            None,
            now,
        )
        .await?;
    }

    Ok(unassigned)
}
