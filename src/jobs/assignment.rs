//! Уведомление о назначении: отмена устаревших записей и доставка.
//!
//! Запись `notification_deliveries` (`kind = 'assignment'`) создаётся в той же
//! транзакции, что и смена `assignee_user_id` (см.
//! [`crate::storage::repositories::notifications::notify_assignment`]). Планировщик
//! только проверяет актуальность и передаёт сообщение в outbox.

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

use super::scheduler::{
    DELIVERY_BATCH, cancel_delivery, desired_schedule, enqueue_message, mark_cancelled, mark_sent,
    postpone_for_quiet, reschedule_if_changed, resolved_timezone,
};
use crate::domain::error::DomainError;
use crate::domain::models::ItemStatus;
use crate::domain::settings::NotificationSettings;
use crate::storage::repositories::{items, lists};
use crate::telegram::bot::notify;

#[derive(FromRow)]
struct AssignmentRow {
    id: Uuid,
    household_id: Uuid,
    user_id: Uuid,
    item_id: Option<Uuid>,
    telegram_user_id: Option<i64>,
    telegram_chat_id: Option<i64>,
    settings: Option<serde_json::Value>,
    user_deleted_at: Option<DateTime<Utc>>,
    user_household_id: Option<Uuid>,
    user_timezone: Option<String>,
    household_deleted_at: Option<DateTime<Utc>>,
    household_timezone: Option<String>,
    item_assignee_user_id: Option<Uuid>,
    item_status: Option<String>,
    item_deleted_at: Option<DateTime<Utc>>,
}

impl AssignmentRow {
    /// Пояс получателя, иначе household, иначе UTC.
    fn timezone(&self) -> Tz {
        resolved_timezone(
            self.user_timezone.as_deref(),
            self.household_timezone.as_deref(),
        )
    }

    /// Уведомления о назначении включены у получателя.
    fn assignment_enabled(&self) -> bool {
        self.settings
            .as_ref()
            .map(|settings| NotificationSettings::from_settings(settings).assignment_enabled)
            .unwrap_or(false)
    }

    /// Запись ещё актуальна: получатель и household живы, уведомления включены,
    /// элемент открыт и всё ещё назначен этому получателю.
    fn is_stale(&self) -> bool {
        self.user_deleted_at.is_some()
            || self.user_household_id != Some(self.household_id)
            || self.household_deleted_at.is_some()
            || !self.assignment_enabled()
            || self.item_deleted_at.is_some()
            || self.item_status.as_deref() != Some("open")
            || self.item_assignee_user_id != Some(self.user_id)
    }
}

const ASSIGNMENT_ROW_COLUMNS: &str = "d.id, d.household_id, d.user_id, d.item_id,
        u.telegram_user_id, u.telegram_chat_id, u.settings,
        u.deleted_at AS user_deleted_at, u.household_id AS user_household_id,
        u.timezone AS user_timezone, h.deleted_at AS household_deleted_at,
        h.timezone AS household_timezone,
        i.assignee_user_id AS item_assignee_user_id, i.status::text AS item_status,
        i.deleted_at AS item_deleted_at";

/// Пересчитать время ещё не доставленных уведомлений о назначении.
///
/// Назначение живёт по моменту постановки записи: смена границ тихих часов
/// сдвигает отложенное уведомление, а их отключение отпускает его на ближайшем
/// тике.
pub async fn reschedule(pool: &PgPool, now: DateTime<Utc>) -> Result<u64, sqlx::Error> {
    #[derive(FromRow)]
    struct PendingAssignment {
        id: Uuid,
        created_at: DateTime<Utc>,
        settings: serde_json::Value,
        user_timezone: Option<String>,
        household_timezone: String,
    }

    let rows: Vec<PendingAssignment> = sqlx::query_as(
        "SELECT d.id, d.created_at, u.settings, u.timezone AS user_timezone,
                h.timezone AS household_timezone
         FROM notification_deliveries d
         JOIN users u ON u.id = d.user_id
         JOIN households h ON h.id = d.household_id
         WHERE d.status = 'pending' AND d.kind = 'assignment'",
    )
    .fetch_all(pool)
    .await?;

    let mut conn = pool.acquire().await?;
    let mut rescheduled = 0;
    for row in rows {
        let settings = NotificationSettings::from_settings(&row.settings);
        let timezone =
            resolved_timezone(row.user_timezone.as_deref(), Some(&row.household_timezone));
        let desired = desired_schedule(row.created_at, now, timezone, &settings);
        let deferred = desired != row.created_at;
        rescheduled += reschedule_if_changed(&mut conn, row.id, desired, deferred, now).await?;
    }
    Ok(rescheduled)
}

/// Отменить `pending`-уведомления о назначении, которые больше не актуальны.
pub async fn cancel_stale(pool: &PgPool, now: DateTime<Utc>) -> Result<u64, sqlx::Error> {
    let sql = format!(
        "SELECT {ASSIGNMENT_ROW_COLUMNS}
         FROM notification_deliveries d
         LEFT JOIN users u ON u.id = d.user_id
         LEFT JOIN households h ON h.id = d.household_id
         LEFT JOIN items i ON i.id = d.item_id
         WHERE d.status = 'pending' AND d.kind = 'assignment'"
    );
    let rows: Vec<AssignmentRow> = sqlx::query_as(&sql).fetch_all(pool).await?;

    let mut cancelled = 0;
    for row in rows {
        if !row.is_stale() {
            continue;
        }
        match cancel_delivery(pool, row.id, now).await {
            Ok(rows) => cancelled += rows,
            Err(error) => tracing::error!(
                delivery_id = %row.id,
                %error,
                "failed to cancel stale assignment delivery"
            ),
        }
    }

    Ok(cancelled)
}

/// Передать готовые уведомления о назначении в outbox.
pub async fn deliver_due(pool: &PgPool, now: DateTime<Utc>) -> Result<u64, sqlx::Error> {
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM notification_deliveries
         WHERE status = 'pending' AND kind = 'assignment' AND scheduled_at <= $1
         ORDER BY scheduled_at, id
         LIMIT $2",
    )
    .bind(now)
    .bind(DELIVERY_BATCH)
    .fetch_all(pool)
    .await?;

    let mut delivered = 0;
    for id in ids {
        match deliver_one(pool, id, now).await {
            Ok(rows) => delivered += rows,
            Err(error) => tracing::error!(
                delivery_id = %id,
                %error,
                "failed to deliver assignment notification"
            ),
        }
    }
    Ok(delivered)
}

/// Доставить одно уведомление о назначении.
async fn deliver_one(pool: &PgPool, id: Uuid, now: DateTime<Utc>) -> Result<u64, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let sql = format!(
        "SELECT {ASSIGNMENT_ROW_COLUMNS}
         FROM notification_deliveries d
         LEFT JOIN users u ON u.id = d.user_id
         LEFT JOIN households h ON h.id = d.household_id
         LEFT JOIN items i ON i.id = d.item_id
         WHERE d.id = $1 AND d.status = 'pending' AND d.kind = 'assignment'
           AND d.scheduled_at <= $2
         FOR UPDATE OF d SKIP LOCKED"
    );
    let row: Option<AssignmentRow> = sqlx::query_as(&sql)
        .bind(id)
        .bind(now)
        .fetch_optional(&mut *tx)
        .await?;

    let Some(row) = row else {
        return Ok(0);
    };
    if row.is_stale() {
        mark_cancelled(&mut tx, row.id, now).await?;
        tx.commit().await?;
        return Ok(0);
    }
    if let Some(settings) = row.settings.as_ref()
        && postpone_for_quiet(&mut tx, row.id, settings, row.timezone(), now).await?
    {
        tx.commit().await?;
        return Ok(0);
    }
    let (Some(chat_id), Some(telegram_user_id), Some(item_id)) =
        (row.telegram_chat_id, row.telegram_user_id, row.item_id)
    else {
        mark_cancelled(&mut tx, row.id, now).await?;
        tx.commit().await?;
        return Ok(0);
    };

    let item = match items::get(&mut tx, row.household_id, item_id).await {
        Ok(item) => item,
        Err(DomainError::Database(error)) => return Err(error),
        Err(_) => {
            mark_cancelled(&mut tx, row.id, now).await?;
            tx.commit().await?;
            return Ok(0);
        }
    };
    if item.status != ItemStatus::Open || item.assignee_user_id != Some(row.user_id) {
        mark_cancelled(&mut tx, row.id, now).await?;
        tx.commit().await?;
        return Ok(0);
    }
    let list = match lists::get(&mut tx, row.household_id, item.list_id).await {
        Ok(list) => list,
        Err(DomainError::Database(error)) => return Err(error),
        Err(_) => {
            mark_cancelled(&mut tx, row.id, now).await?;
            tx.commit().await?;
            return Ok(0);
        }
    };

    let ui = notify::assignment_ui(&item, &list, row.timezone(), now);
    let html = notify::render_notice(&mut tx, telegram_user_id, row.household_id, &ui, now).await?;
    let dedupe_key = format!("notification:assignment:{}", row.id);
    let seq = enqueue_message(&mut tx, chat_id, &dedupe_key, html, true, now).await?;
    mark_sent(&mut tx, row.id, seq, now).await?;

    tx.commit().await?;
    Ok(1)
}
