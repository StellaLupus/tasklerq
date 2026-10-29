//! Retention и purge: сроки хранения и физическое удаление.

use chrono::{DateTime, Duration, Utc};
use sqlx::PgPool;

use crate::config::RetentionConfig;

/// Результат одного запуска retention.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RetentionStats {
    /// Просроченные черновики, помеченные `expired`.
    pub expired_drafts: u64,
    /// Физически удалённые элементы.
    pub purged_items: u64,
    /// Физически удалённые списки.
    pub purged_lists: u64,
    /// Физически удалённые household.
    pub purged_households: u64,
    /// Удалённые события.
    pub purged_events: u64,
    /// Удалённые черновики.
    pub purged_drafts: u64,
}

/// Выполнить retention на момент `now`.
pub async fn run(
    pool: &PgPool,
    config: &RetentionConfig,
    now: DateTime<Utc>,
) -> Result<RetentionStats, sqlx::Error> {
    let deleted_cutoff = now - Duration::days(i64::from(config.deleted_days));
    let events_cutoff = now - Duration::days(i64::from(config.events_days));
    let drafts_cutoff = now - Duration::days(i64::from(config.drafts_days));

    let mut tx = pool.begin().await?;

    let expired_drafts = sqlx::query(
        "UPDATE capture_drafts
         SET status = 'expired'
         WHERE status = 'pending' AND expires_at <= $1",
    )
    .bind(now)
    .execute(&mut *tx)
    .await?
    .rows_affected();

    let purged_items =
        sqlx::query("DELETE FROM items WHERE deleted_at IS NOT NULL AND deleted_at < $1")
            .bind(deleted_cutoff)
            .execute(&mut *tx)
            .await?
            .rows_affected();

    let purged_lists =
        sqlx::query("DELETE FROM lists WHERE deleted_at IS NOT NULL AND deleted_at < $1")
            .bind(deleted_cutoff)
            .execute(&mut *tx)
            .await?
            .rows_affected();

    let purged_households =
        sqlx::query("DELETE FROM households WHERE deleted_at IS NOT NULL AND deleted_at < $1")
            .bind(deleted_cutoff)
            .execute(&mut *tx)
            .await?
            .rows_affected();

    let purged_events = sqlx::query("DELETE FROM item_events WHERE created_at < $1")
        .bind(events_cutoff)
        .execute(&mut *tx)
        .await?
        .rows_affected();

    let purged_drafts = sqlx::query("DELETE FROM capture_drafts WHERE created_at < $1")
        .bind(drafts_cutoff)
        .execute(&mut *tx)
        .await?
        .rows_affected();

    tx.commit().await?;
    Ok(RetentionStats {
        expired_drafts,
        purged_items,
        purged_lists,
        purged_households,
        purged_events,
        purged_drafts,
    })
}
