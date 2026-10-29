//! Журнал аудита: членство и безопасность.
//!
//! Payload собирается вызывающей стороной из явных полей: коды приглашений,
//! токены и хеши в журнал не попадают. Имена хранятся снимком на момент
//! события, поэтому переименование не переписывает историю.

use chrono::{DateTime, Duration, Utc};
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::identity::{AuditEvent, AuditEventType};
use crate::domain::models::Actor;

/// Глубина журнала для владельца, дней.
pub const JOURNAL_DAYS: i64 = 90;
/// Максимальный размер страницы журнала.
pub const MAX_PAGE: i64 = 100;

const COLUMNS: &str =
    "id, household_id, actor_user_id, actor_type, event_type, payload, created_at";

/// Записать событие аудита.
pub async fn record(
    conn: &mut PgConnection,
    household_id: Option<Uuid>,
    actor: &Actor,
    event_type: AuditEventType,
    payload: Value,
    now: DateTime<Utc>,
) -> Result<AuditEvent, DomainError> {
    let event = sqlx::query_as::<_, AuditEvent>(&format!(
        "INSERT INTO audit_events (
             id, household_id, actor_user_id, actor_type, event_type, payload, created_at
         )
         VALUES ($1, $2, $3, $4, $5, $6, $7)
         RETURNING {COLUMNS}"
    ))
    .bind(Uuid::now_v7())
    .bind(household_id)
    .bind(actor.user_id)
    .bind(actor.actor_type)
    .bind(event_type)
    .bind(payload)
    .bind(now)
    .fetch_one(conn)
    .await?;

    Ok(event)
}

/// Позиция в журнале для постраничного чтения: события старше неё.
#[derive(Debug, Clone, Copy)]
pub struct JournalCursor {
    /// Время последнего полученного события.
    pub created_at: DateTime<Utc>,
    /// Идентификатор последнего полученного события.
    pub id: Uuid,
}

impl From<&AuditEvent> for JournalCursor {
    fn from(event: &AuditEvent) -> Self {
        Self {
            created_at: event.created_at,
            id: event.id,
        }
    }
}

/// Журнал членства за последние 90 дней, от новых событий к старым.
/// Доступен только владельцу.
pub async fn membership_journal(
    conn: &mut PgConnection,
    household_id: Uuid,
    actor: &Actor,
    now: DateTime<Utc>,
    before: Option<JournalCursor>,
    limit: i64,
) -> Result<Vec<AuditEvent>, DomainError> {
    super::users::require_current_owner(conn, household_id, actor).await?;
    let limit = limit.clamp(1, MAX_PAGE);
    let since = now - Duration::days(JOURNAL_DAYS);
    let types: Vec<String> = AuditEventType::MEMBERSHIP
        .iter()
        .map(|event_type| event_type.as_str().to_owned())
        .collect();

    let events = sqlx::query_as::<_, AuditEvent>(&format!(
        "SELECT {COLUMNS}
         FROM audit_events
         WHERE household_id = $1
           AND created_at >= $2
           AND event_type::text = ANY($3)
           AND ($4::timestamptz IS NULL OR (created_at, id) < ($4, $5))
         ORDER BY created_at DESC, id DESC
         LIMIT $6"
    ))
    .bind(household_id)
    .bind(since)
    .bind(&types)
    .bind(before.map(|cursor| cursor.created_at))
    .bind(before.map(|cursor| cursor.id).unwrap_or_else(Uuid::nil))
    .bind(limit)
    .fetch_all(conn)
    .await?;

    Ok(events)
}
