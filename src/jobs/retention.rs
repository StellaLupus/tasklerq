//! Retention и purge: сроки хранения и физическое удаление.

use std::panic::AssertUnwindSafe;
use std::time::Duration as StdDuration;

use chrono::{DateTime, Duration, Utc};
use futures_util::FutureExt;
use sqlx::{PgConnection, PgPool};
use tokio::sync::watch;

use crate::config::RetentionConfig;

/// Задержка первого прогона после старта: порядка минут, чтобы не мешать
/// миграциям и прогреву, но не откладывать уборку до следующих суток.
pub const FIRST_RUN_DELAY: StdDuration = StdDuration::from_secs(60);

/// Интервал между прогонами: раз в сутки.
pub const RUN_INTERVAL: StdDuration = StdDuration::from_secs(24 * 60 * 60);

/// Фиксированный ключ advisory-блокировки retention.
///
/// Ключ общий для всех экземпляров сервиса: пока один чистит базу, второй
/// пропускает прогон. Значение не пересекается с блокировками по Telegram ID.
pub const ADVISORY_LOCK_KEY: i64 = 0x5254_4E54_4C4F_434B; // "RTNTLOCK"

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
    /// Удалённые завершённые записи доставки уведомлений.
    pub purged_notification_deliveries: u64,
    /// Удалённые просроченные формы.
    pub purged_conversations: u64,
}

/// Выполнить retention на момент `now`.
pub async fn run(
    pool: &PgPool,
    config: &RetentionConfig,
    now: DateTime<Utc>,
) -> Result<RetentionStats, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let stats = run_in_tx(&mut tx, config, now).await?;
    tx.commit().await?;
    Ok(stats)
}

/// Выполнить retention под advisory-блокировкой.
///
/// `None` означает, что блокировку держит другой экземпляр: прогон
/// пропускается, это не ошибка.
pub async fn run_locked(
    pool: &PgPool,
    config: &RetentionConfig,
    now: DateTime<Utc>,
) -> Result<Option<RetentionStats>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_xact_lock($1)")
        .bind(ADVISORY_LOCK_KEY)
        .fetch_one(&mut *tx)
        .await?;
    if !acquired {
        tx.rollback().await?;
        return Ok(None);
    }
    let stats = run_in_tx(&mut tx, config, now).await?;
    tx.commit().await?;
    Ok(Some(stats))
}

async fn run_in_tx(
    conn: &mut PgConnection,
    config: &RetentionConfig,
    now: DateTime<Utc>,
) -> Result<RetentionStats, sqlx::Error> {
    let deleted_cutoff = now - Duration::days(i64::from(config.deleted_days));
    let events_cutoff = now - Duration::days(i64::from(config.events_days));
    let drafts_cutoff = now - Duration::days(i64::from(config.drafts_days));
    let audit_cutoff = now - Duration::days(i64::from(config.audit_days));

    let expired_drafts = sqlx::query(
        "UPDATE capture_drafts
         SET status = 'expired'
         WHERE status = 'pending' AND expires_at <= $1",
    )
    .bind(now)
    .execute(&mut *conn)
    .await?
    .rows_affected();

    let purged_items =
        sqlx::query("DELETE FROM items WHERE deleted_at IS NOT NULL AND deleted_at < $1")
            .bind(deleted_cutoff)
            .execute(&mut *conn)
            .await?
            .rows_affected();

    let purged_lists =
        sqlx::query("DELETE FROM lists WHERE deleted_at IS NOT NULL AND deleted_at < $1")
            .bind(deleted_cutoff)
            .execute(&mut *conn)
            .await?
            .rows_affected();

    let purged_households =
        sqlx::query("DELETE FROM households WHERE deleted_at IS NOT NULL AND deleted_at < $1")
            .bind(deleted_cutoff)
            .execute(&mut *conn)
            .await?
            .rows_affected();

    let purged_events = sqlx::query("DELETE FROM item_events WHERE created_at < $1")
        .bind(events_cutoff)
        .execute(&mut *conn)
        .await?
        .rows_affected();

    let purged_drafts = sqlx::query("DELETE FROM capture_drafts WHERE created_at < $1")
        .bind(drafts_cutoff)
        .execute(&mut *conn)
        .await?
        .rows_affected();

    let purged_capture_requests = sqlx::query("DELETE FROM capture_requests WHERE created_at < $1")
        .bind(drafts_cutoff)
        .execute(&mut *conn)
        .await?
        .rows_affected();

    let purged_audit_events = sqlx::query("DELETE FROM audit_events WHERE created_at < $1")
        .bind(audit_cutoff)
        .execute(&mut *conn)
        .await?
        .rows_affected();

    // Приглашения хранятся столько же, сколько события аудита об их выдаче.
    let purged_invites = sqlx::query(
        "DELETE FROM invites
         WHERE COALESCE(used_at, revoked_at, expires_at) < $1",
    )
    .bind(audit_cutoff)
    .execute(&mut *conn)
    .await?
    .rows_affected();

    // Необработанные (`pending`) остаются: их ещё должен разобрать воркер.
    let purged_updates = sqlx::query(
        "DELETE FROM telegram_updates WHERE status IN ('processed', 'dead') AND received_at < $1",
    )
    .bind(now - Duration::days(i64::from(config.updates_days)))
    .execute(&mut *conn)
    .await?
    .rows_affected();

    let purged_capture_jobs = crate::storage::repositories::capture_jobs::purge(
        &mut *conn,
        now - Duration::days(i64::from(config.updates_days)),
    )
    .await?;

    let purged_llm_runs = crate::storage::repositories::llm_runs::purge(
        &mut *conn,
        now - Duration::days(i64::from(config.llm_runs_days)),
    )
    .await?;

    let purged_callback_tokens = crate::telegram::callbacks::purge_expired(&mut *conn, now).await?;
    let purged_outbox = crate::telegram::outbox::purge(
        &mut *conn,
        now - Duration::days(i64::from(config.outbox_days)),
    )
    .await?;

    // Завершённые доставки старше срока outbox. `pending` не трогаем никогда:
    // запись может ждать тихих часов или ещё не наступившего вхождения.
    //
    // Для дедупликации безопасно: вхождение остаётся в таблице, пока его ещё
    // можно перепланировать, — timed до срока, вседневное и сводка до конца
    // локального дня; назначение и snooze вставляются однократно. Поэтому
    // `sent`/`cancelled` старше `RETENTION_OUTBOX_DAYS` уже не участвуют в
    // проверке уникальности.
    let purged_notification_deliveries = sqlx::query(
        "DELETE FROM notification_deliveries
         WHERE status IN ('sent', 'cancelled') AND updated_at < $1",
    )
    .bind(now - Duration::days(i64::from(config.outbox_days)))
    .execute(&mut *conn)
    .await?
    .rows_affected();

    let purged_conversations = crate::telegram::views::purge_conversations(&mut *conn, now).await?;

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
        purged_notification_deliveries,
        purged_conversations,
    })
}

/// Фоновое задание retention: первый прогон после старта, затем раз в сутки.
pub struct RetentionWorker {
    pool: PgPool,
    config: RetentionConfig,
}

impl RetentionWorker {
    /// Создать задание.
    pub fn new(pool: PgPool, config: RetentionConfig) -> Self {
        Self { pool, config }
    }

    /// Один прогон на момент `now`.
    ///
    /// Возвращает `None`, если блокировку держит другой экземпляр. Итог
    /// пишется одним `info`-событием без пользовательских данных.
    pub async fn run_once(
        &self,
        now: DateTime<Utc>,
    ) -> Result<Option<RetentionStats>, sqlx::Error> {
        let stats = run_locked(&self.pool, &self.config, now).await?;
        if let Some(stats) = stats {
            tracing::info!(
                expired_drafts = stats.expired_drafts,
                purged_items = stats.purged_items,
                purged_lists = stats.purged_lists,
                purged_households = stats.purged_households,
                purged_events = stats.purged_events,
                purged_drafts = stats.purged_drafts,
                purged_capture_requests = stats.purged_capture_requests,
                purged_audit_events = stats.purged_audit_events,
                purged_invites = stats.purged_invites,
                purged_updates = stats.purged_updates,
                purged_capture_jobs = stats.purged_capture_jobs,
                purged_llm_runs = stats.purged_llm_runs,
                purged_callback_tokens = stats.purged_callback_tokens,
                purged_outbox = stats.purged_outbox,
                purged_notification_deliveries = stats.purged_notification_deliveries,
                purged_conversations = stats.purged_conversations,
                "retention finished"
            );
        }
        Ok(stats)
    }

    /// Цикл до сигнала остановки.
    pub async fn run(self, mut shutdown: watch::Receiver<bool>) {
        let mut next = tokio::time::Instant::now() + FIRST_RUN_DELAY;
        loop {
            if *shutdown.borrow() {
                return;
            }
            tokio::select! {
                _ = shutdown.changed() => return,
                _ = tokio::time::sleep_until(next) => {
                    // Ошибка или паника одного прогона не останавливает цикл.
                    match AssertUnwindSafe(self.run_once(Utc::now())).catch_unwind().await {
                        Ok(Ok(Some(_))) => {}
                        Ok(Ok(None)) => {
                            tracing::info!("retention skipped: another instance holds the lock");
                        }
                        Ok(Err(error)) => tracing::error!(%error, "retention run failed"),
                        Err(_) => tracing::error!("retention run panicked"),
                    }
                    next = tokio::time::Instant::now() + RUN_INTERVAL;
                }
            }
        }
    }
}
