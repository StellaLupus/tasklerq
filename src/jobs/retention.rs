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
    /// Удалённые резервы ключей идемпотентности capture.
    pub purged_capture_requests: u64,
    /// Удалённые события аудита.
    pub purged_audit_events: u64,
    /// Удалённые приглашения: использованные, отозванные и истёкшие.
    pub purged_invites: u64,
    /// Удалённые входящие обновления Telegram.
    pub purged_updates: u64,
    /// Удалено завершённых заданий разбора.
    pub purged_capture_jobs: u64,
    /// Удалённые метаданные разборов LLM.
    pub purged_llm_runs: u64,
    /// Удалённые просроченные callback-токены.
    pub purged_callback_tokens: u64,
    /// Удалённые завершённые сообщения outbox: отправленные, пропущенные, мёртвые.
    pub purged_outbox: u64,
    /// Удалённые просроченные формы.
    pub purged_conversations: u64,
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
    let audit_cutoff = now - Duration::days(i64::from(config.audit_days));

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

    let purged_capture_requests = sqlx::query("DELETE FROM capture_requests WHERE created_at < $1")
        .bind(drafts_cutoff)
        .execute(&mut *tx)
        .await?
        .rows_affected();

    let purged_audit_events = sqlx::query("DELETE FROM audit_events WHERE created_at < $1")
        .bind(audit_cutoff)
        .execute(&mut *tx)
        .await?
        .rows_affected();

    // Приглашения хранятся столько же, сколько события аудита об их выдаче.
    let purged_invites = sqlx::query(
        "DELETE FROM invites
         WHERE COALESCE(used_at, revoked_at, expires_at) < $1",
    )
    .bind(audit_cutoff)
    .execute(&mut *tx)
    .await?
    .rows_affected();

    // Необработанные (`pending`) остаются: их ещё должен разобрать воркер.
    let purged_updates = sqlx::query(
        "DELETE FROM telegram_updates WHERE status IN ('processed', 'dead') AND received_at < $1",
    )
    .bind(now - Duration::days(i64::from(config.updates_days)))
    .execute(&mut *tx)
    .await?
    .rows_affected();

    let purged_capture_jobs = crate::storage::repositories::capture_jobs::purge(
        &mut tx,
        now - Duration::days(i64::from(config.updates_days)),
    )
    .await?;

    let purged_llm_runs = crate::storage::repositories::llm_runs::purge(
        &mut tx,
        now - Duration::days(i64::from(config.llm_runs_days)),
    )
    .await?;

    let purged_callback_tokens = crate::telegram::callbacks::purge_expired(&mut tx, now).await?;
    let purged_outbox = crate::telegram::outbox::purge(
        &mut tx,
        now - Duration::days(i64::from(config.outbox_days)),
    )
    .await?;
    let purged_conversations = crate::telegram::views::purge_conversations(&mut tx, now).await?;

    tx.commit().await?;
    Ok(RetentionStats {
        expired_drafts,
        purged_items,
        purged_lists,
        purged_households,
        purged_events,
        purged_drafts,
        purged_capture_requests,
        purged_audit_events,
        purged_invites,
        purged_updates,
        purged_capture_jobs,
        purged_llm_runs,
        purged_callback_tokens,
        purged_outbox,
        purged_conversations,
    })
}
