//! Retention и purge: сроки хранения и физическое удаление.

use std::future::Future;
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
    /// Удалённые сводные сообщения уведомлений.
    pub purged_notification_batches: u64,
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

/// Выполнить `DELETE ... WHERE ... < $1` и вернуть число удалённых строк.
async fn delete_before(
    conn: &mut PgConnection,
    sql: &str,
    cutoff: DateTime<Utc>,
) -> Result<u64, sqlx::Error> {
    Ok(sqlx::query(sql)
        .bind(cutoff)
        .execute(&mut *conn)
        .await?
        .rows_affected())
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
    let updates_cutoff = now - Duration::days(i64::from(config.updates_days));
    let outbox_cutoff = now - Duration::days(i64::from(config.outbox_days));

    let expired_drafts = sqlx::query(
        "UPDATE capture_drafts
         SET status = 'expired'
         WHERE status = 'pending' AND expires_at <= $1",
    )
    .bind(now)
    .execute(&mut *conn)
    .await?
    .rows_affected();

    let purged_items = delete_before(
        &mut *conn,
        "DELETE FROM items WHERE deleted_at IS NOT NULL AND deleted_at < $1",
        deleted_cutoff,
    )
    .await?;

    let purged_lists = delete_before(
        &mut *conn,
        "DELETE FROM lists WHERE deleted_at IS NOT NULL AND deleted_at < $1",
        deleted_cutoff,
    )
    .await?;

    let purged_households = delete_before(
        &mut *conn,
        "DELETE FROM households WHERE deleted_at IS NOT NULL AND deleted_at < $1",
        deleted_cutoff,
    )
    .await?;

    let purged_events = delete_before(
        &mut *conn,
        "DELETE FROM item_events WHERE created_at < $1",
        events_cutoff,
    )
    .await?;

    // Исходный текст живёт в строке черновика: `raw_text` и `raw_parts`
    // очищаются вместе с ней по `RETENTION_DRAFTS_DAYS`.
    let purged_drafts = delete_before(
        &mut *conn,
        "DELETE FROM capture_drafts WHERE created_at < $1",
        drafts_cutoff,
    )
    .await?;

    let purged_capture_requests = delete_before(
        &mut *conn,
        "DELETE FROM capture_requests WHERE created_at < $1",
        drafts_cutoff,
    )
    .await?;

    let purged_audit_events = delete_before(
        &mut *conn,
        "DELETE FROM audit_events WHERE created_at < $1",
        audit_cutoff,
    )
    .await?;

    // Приглашения хранятся столько же, сколько события аудита об их выдаче.
    let purged_invites = delete_before(
        &mut *conn,
        "DELETE FROM invites
         WHERE COALESCE(used_at, revoked_at, expires_at) < $1",
        audit_cutoff,
    )
    .await?;

    // Необработанные (`pending`) остаются: их ещё должен разобрать воркер.
    let purged_updates = delete_before(
        &mut *conn,
        "DELETE FROM telegram_updates WHERE status IN ('processed', 'dead') AND received_at < $1",
        updates_cutoff,
    )
    .await?;

    let purged_capture_jobs =
        crate::storage::repositories::capture_jobs::purge(&mut *conn, updates_cutoff).await?;

    let purged_llm_runs = crate::storage::repositories::llm_runs::purge(
        &mut *conn,
        now - Duration::days(i64::from(config.llm_runs_days)),
    )
    .await?;

    let purged_callback_tokens = crate::telegram::callbacks::purge_expired(&mut *conn, now).await?;
    let purged_outbox = crate::telegram::outbox::purge(&mut *conn, outbox_cutoff).await?;

    // Завершённые доставки старше срока outbox. `pending` не трогаем никогда:
    // запись может ждать тихих часов или ещё не наступившего вхождения.
    //
    // Доставки батча, который ещё жив (его `updated_at` обновило листание или
    // «✓ Выполнить все»), не удаляем: иначе страница сводного опустеет, хотя
    // сообщение пользователь всё ещё открывает. Действия по отдельным блокам
    // («✓ Выполнено», «На час», «Открыть») обновляют только `notification_deliveries`
    // и `notification_batches.updated_at` не трогают.
    //
    // Для дедупликации безопасно: вхождение остаётся в таблице, пока его ещё
    // можно перепланировать, — timed до срока, вседневное и сводка до конца
    // локального дня; назначение и snooze вставляются однократно. Поэтому
    // `sent`/`cancelled` старше `RETENTION_OUTBOX_DAYS` уже не участвуют в
    // проверке уникальности.
    //
    // Инвариант: `RETENTION_OUTBOX_DAYS` >= 1 (минимум в `config/mod.rs`), а
    // `MAX_LEAD_MINUTES` = 1440 (`domain/settings.rs`) — напоминание о timed-сроке
    // не создаётся раньше чем за сутки до него. Если изменить любое из этих
    // значений, пересмотреть очистку: строка `sent`/`cancelled` должна
    // пережить окно, в котором её вхождение ещё может быть перепланировано.
    let purged_notification_deliveries = delete_before(
        &mut *conn,
        "DELETE FROM notification_deliveries d
         WHERE d.status IN ('sent', 'cancelled') AND d.updated_at < $1
           AND NOT EXISTS (
               SELECT 1 FROM notification_batches b
               WHERE b.id = d.batch_id AND b.updated_at >= $1
           )",
        outbox_cutoff,
    )
    .await?;

    // Сводное сообщение живёт, пока на него ссылается хоть одна доставка:
    // листание и «✓ Выполнить все» обновляют `updated_at` батча, а действия по
    // блокам — `updated_at` доставок, поэтому по одной метке нельзя решить, что
    // батч уже не нужен.
    // Завершённые доставки старше срока удалены выше; если ссылок не осталось и
    // сам батч старше срока, его можно удалить.
    let purged_notification_batches = delete_before(
        &mut *conn,
        "DELETE FROM notification_batches b
         WHERE b.updated_at < $1
           AND NOT EXISTS (
               SELECT 1 FROM notification_deliveries d
               WHERE d.batch_id = b.id
           )",
        outbox_cutoff,
    )
    .await?;

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
        purged_notification_batches,
        purged_conversations,
    })
}

/// Фоновое задание retention: первый прогон после старта, затем раз в сутки.
pub struct RetentionWorker {
    pool: PgPool,
    config: RetentionConfig,
    first_delay: StdDuration,
    interval: StdDuration,
}

impl RetentionWorker {
    /// Создать задание.
    pub fn new(pool: PgPool, config: RetentionConfig) -> Self {
        Self {
            pool,
            config,
            first_delay: FIRST_RUN_DELAY,
            interval: RUN_INTERVAL,
        }
    }

    /// Переопределить задержку первого прогона и интервал (для тестов).
    #[must_use]
    pub fn with_timing(mut self, first_delay: StdDuration, interval: StdDuration) -> Self {
        self.first_delay = first_delay;
        self.interval = interval;
        self
    }

    /// Один прогон на момент `now`.
    ///
    /// Возвращает `None`, если блокировку держит другой экземпляр. Итог
    /// пишется одним `info`-событием без пользовательских данных.
    pub async fn run_once(
        &self,
        now: DateTime<Utc>,
    ) -> Result<Option<RetentionStats>, sqlx::Error> {
        execute_once(&self.pool, &self.config, now).await
    }

    /// Цикл до сигнала остановки.
    pub async fn run(self, shutdown: watch::Receiver<bool>) {
        let RetentionWorker {
            pool,
            config,
            first_delay,
            interval,
        } = self;
        run_loop(
            move |now| {
                let pool = pool.clone();
                async move { execute_once(&pool, &config, now).await }
            },
            shutdown,
            first_delay,
            interval,
        )
        .await;
    }

    /// Цикл с подменённым прогоном. Нужен тестам, чтобы проверить, что ошибка
    /// и паника одного прогона не останавливают следующие.
    pub async fn run_with<F, Fut>(self, shutdown: watch::Receiver<bool>, run: F)
    where
        F: FnMut(DateTime<Utc>) -> Fut + Send,
        Fut: Future<Output = Result<Option<RetentionStats>, sqlx::Error>> + Send,
    {
        run_loop(run, shutdown, self.first_delay, self.interval).await;
    }
}

/// Один прогон под advisory-блокировкой с журналом итога.
///
/// Возвращает `None`, если блокировку держит другой экземпляр. Итог пишется
/// одним `info`-событием без пользовательских данных.
async fn execute_once(
    pool: &PgPool,
    config: &RetentionConfig,
    now: DateTime<Utc>,
) -> Result<Option<RetentionStats>, sqlx::Error> {
    let stats = run_locked(pool, config, now).await?;
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
            purged_notification_batches = stats.purged_notification_batches,
            purged_conversations = stats.purged_conversations,
            "retention finished"
        );
    }
    Ok(stats)
}

async fn run_loop<F, Fut>(
    mut run: F,
    mut shutdown: watch::Receiver<bool>,
    first_delay: StdDuration,
    interval: StdDuration,
) where
    F: FnMut(DateTime<Utc>) -> Fut + Send,
    Fut: Future<Output = Result<Option<RetentionStats>, sqlx::Error>> + Send,
{
    let mut next = tokio::time::Instant::now() + first_delay;
    loop {
        if *shutdown.borrow() {
            return;
        }
        tokio::select! {
            _ = shutdown.changed() => return,
            _ = tokio::time::sleep_until(next) => {
                // Ошибка или паника одного прогона не останавливает цикл.
                match AssertUnwindSafe(run(Utc::now())).catch_unwind().await {
                    Ok(Ok(Some(_))) => {}
                    Ok(Ok(None)) => {
                        tracing::info!("retention skipped: another instance holds the lock");
                    }
                    Ok(Err(error)) => tracing::error!(%error, "retention run failed"),
                    Err(_) => tracing::error!("retention run panicked"),
                }
                next = tokio::time::Instant::now() + interval;
            }
        }
    }
}
