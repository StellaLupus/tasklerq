//! Уведомление о назначении: пересчёт времени и текст сообщения.
//!
//! Запись `notification_deliveries` (`kind = 'assignment'`) создаётся в той же
//! транзакции, что и смена `assignee_user_id` (см.
//! [`crate::storage::repositories::notifications::notify_assignment`]). Планировщик
//! только проверяет актуальность и передаёт сообщение в outbox.

use chrono::{DateTime, Utc};
use sqlx::{PgConnection, PgPool};

use super::delivery::{self, DeliveryScope};
use super::notice::NoticeRow;
use crate::domain::models::ItemStatus;
use crate::telegram::bot::notify;

/// Пересчитать время ещё не доставленных уведомлений о назначении.
///
/// Назначение живёт по моменту постановки записи: пока оно не отложено тихими
/// часами, смена границ сдвигает его. После переноса время фиксируется и тиками
/// не пересчитывается.
pub async fn reschedule(pool: &PgPool, now: DateTime<Utc>) -> Result<u64, sqlx::Error> {
    delivery::reschedule_pending(pool, now, DeliveryScope::Assignment, |row| {
        Some(row.created_at)
    })
    .await
}

/// Собрать сообщение о назначении; `None` — запись отменена.
pub(crate) async fn assignment_message(
    conn: &mut PgConnection,
    row: &NoticeRow,
    telegram_user_id: i64,
    now: DateTime<Utc>,
) -> Result<Option<String>, sqlx::Error> {
    let Some((item, list)) = delivery::load_item_and_list(conn, row, now).await? else {
        return Ok(None);
    };
    // Снимок строки уже проверен, но задачу читаем ещё раз: назначение могло
    // смениться до блокировки записи, и тогда сообщение устарело.
    if item.status != ItemStatus::Open || item.assignee_user_id != Some(row.user_id) {
        delivery::mark_cancelled(conn, row.id, now).await?;
        return Ok(None);
    }

    let ui = notify::assignment_ui(&item, &list, row.timezone(), now);
    notify::render_notice(conn, telegram_user_id, row.household_id, &ui, now)
        .await
        .map(Some)
}
