//! Уведомление о назначении: отмена устаревших записей и доставка.
//!
//! Запись `notification_deliveries` (`kind = 'assignment'`) создаётся в той же
//! транзакции, что и смена `assignee_user_id` (см.
//! [`crate::storage::repositories::notifications::notify_assignment`]). Планировщик
//! только проверяет актуальность и передаёт сообщение в outbox.

use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

use super::notice::{NOTICE_ROW_COLUMNS, NOTICE_ROW_FROM, NoticeRow};
use super::scheduler::{
    DELIVERY_BATCH, cancel_delivery, desired_schedule, enqueue_message, mark_cancelled, mark_sent,
    postpone_for_quiet, reschedule_if_changed, resolved_timezone, try_recipient_lock,
};
use crate::domain::error::DomainError;
use crate::domain::models::ItemStatus;
use crate::domain::settings::NotificationSettings;
use crate::storage::repositories::{items, lists};
use crate::telegram::bot::notify;

/// Пересчитать время ещё не доставленных уведомлений о назначении.
///
/// Назначение живёт по моменту постановки записи: пока оно не отложено тихими
/// часами, смена границ сдвигает его. После переноса время фиксируется и тиками
/// не пересчитывается.
pub async fn reschedule(pool: &PgPool, now: DateTime<Utc>) -> Result<u64, sqlx::Error> {
    #[derive(FromRow)]
    struct PendingAssignment {
        id: Uuid,
        created_at: DateTime<Utc>,
        deferred: bool,
        settings: serde_json::Value,
        user_timezone: Option<String>,
        household_timezone: String,
    }

    let rows: Vec<PendingAssignment> = sqlx::query_as(
        "SELECT d.id, d.created_at, d.deferred, u.settings,
                u.timezone AS user_timezone, h.timezone AS household_timezone
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
        // Время выпуска отложенного назначения фиксируется при переносе.
        if row.deferred {
            continue;
        }
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
        "SELECT {NOTICE_ROW_COLUMNS}
         {NOTICE_ROW_FROM}
         WHERE d.status = 'pending' AND d.kind = 'assignment'"
    );
    let rows: Vec<NoticeRow> = sqlx::query_as(&sql).fetch_all(pool).await?;

    let mut cancelled = 0;
    for row in rows {
        if !row.is_stale(now) {
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
    let ids: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT id, user_id FROM notification_deliveries
         WHERE status = 'pending' AND kind = 'assignment' AND scheduled_at <= $1
         ORDER BY scheduled_at, id
         LIMIT $2",
    )
    .bind(now)
    .bind(DELIVERY_BATCH)
    .fetch_all(pool)
    .await?;

    let mut delivered = 0;
    for (id, user_id) in ids {
        match deliver_one(pool, id, user_id, now).await {
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
async fn deliver_one(
    pool: &PgPool,
    id: Uuid,
    user_id: Uuid,
    now: DateTime<Utc>,
) -> Result<u64, sqlx::Error> {
    let mut tx = pool.begin().await?;
    if !try_recipient_lock(&mut tx, user_id).await? {
        return Ok(0);
    }
    let sql = format!(
        "SELECT {NOTICE_ROW_COLUMNS}
         {NOTICE_ROW_FROM}
         WHERE d.id = $1 AND d.status = 'pending' AND d.kind = 'assignment'
           AND d.scheduled_at <= $2
         FOR UPDATE OF d SKIP LOCKED"
    );
    let row: Option<NoticeRow> = sqlx::query_as(&sql)
        .bind(id)
        .bind(now)
        .fetch_optional(&mut *tx)
        .await?;

    let Some(row) = row else {
        return Ok(0);
    };
    if row.is_stale(now) {
        mark_cancelled(&mut tx, row.id, now).await?;
        tx.commit().await?;
        return Ok(0);
    }
    if let Some(settings) = row.recipient_settings.as_ref()
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
