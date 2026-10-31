//! Планировщик уведомлений: тик, напоминания и дедупликация доставок.
//!
//! Один тик делает три шага:
//!
//! 1. отменяет `pending`-доставки, вхождение которых больше не актуально
//!    (задача завершена, удалена или срок сдвинулся);
//! 2. планирует напоминания для открытых задач со временем, у которых наступил
//!    момент `due_at - lead_minutes` получателя, а сам срок ещё не прошёл;
//! 3. ставит готовые `pending`-доставки в `outbox` одной транзакцией со сменой
//!    статуса и будит outbox-воркер.
//!
//! Время передаётся в [`Scheduler::tick`] снаружи, поэтому тесты двигают его без
//! ожидания. Ключ доставки включает пользователя, элемент, вхождение и тип:
//! повторный тик и перезапуск сервиса не создают второе сообщение. Следующее
//! вхождение повторяющейся задачи получает новый `due_at`, а значит и новый
//! `occurrence_key`.

use std::sync::Arc;
use std::time::Duration as StdDuration;

use chrono::{DateTime, Duration, SecondsFormat, Utc};
use sqlx::{FromRow, PgPool};
use tokio::sync::{Notify, watch};
use uuid::Uuid;

use crate::domain::recurrence::parse_timezone;
use crate::domain::settings::{MAX_LEAD_MINUTES, NotificationSettings};
use crate::telegram::outbox::{self, Outgoing};
use crate::telegram::{rich, ru};

/// Интервал планировщика.
pub const TICK_INTERVAL: StdDuration = StdDuration::from_secs(30);

/// Сколько кандидатов на доставку рассматривается за один тик.
const DELIVERY_BATCH: i64 = 100;

/// Ключ вхождения: срок в UTC с микросекундной точностью.
pub fn occurrence_key(due_at: DateTime<Utc>) -> String {
    due_at.to_rfc3339_opts(SecondsFormat::Micros, true)
}

/// Итог одного тика.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TickStats {
    /// Создано новых записей доставки.
    pub planned: u64,
    /// Передано в outbox сообщений.
    pub delivered: u64,
    /// Отменено устаревших записей.
    pub cancelled: u64,
}

/// Планировщик уведомлений.
pub struct Scheduler {
    pool: PgPool,
    wake_outbox: Arc<Notify>,
}

impl Scheduler {
    /// Создать планировщик.
    pub fn new(pool: PgPool, wake_outbox: Arc<Notify>) -> Self {
        Self { pool, wake_outbox }
    }

    /// Один тик на момент `now`.
    pub async fn tick(&self, now: DateTime<Utc>) -> Result<TickStats, sqlx::Error> {
        let cancelled = cancel_stale(&self.pool, now).await?;
        let planned = plan_reminders(&self.pool, now).await?;
        let delivered = deliver_due(&self.pool, now).await?;

        if delivered > 0 {
            self.wake_outbox.notify_one();
        }

        Ok(TickStats {
            planned,
            delivered,
            cancelled,
        })
    }

    /// Цикл до сигнала остановки.
    pub async fn run(self, mut shutdown: watch::Receiver<bool>) {
        let mut interval = tokio::time::interval(TICK_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            if *shutdown.borrow() {
                return;
            }
            tokio::select! {
                _ = shutdown.changed() => return,
                _ = interval.tick() => {
                    if let Err(error) = self.tick(Utc::now()).await {
                        tracing::error!(%error, "notification scheduler tick failed");
                    }
                }
            }
        }
    }
}

#[derive(FromRow)]
struct ReminderCandidate {
    item_id: Uuid,
    household_id: Uuid,
    user_id: Uuid,
    due_at: DateTime<Utc>,
    settings: serde_json::Value,
}

/// Запланировать напоминания, момент которых уже наступил.
///
/// Получатель — исполнитель, иначе автор-пользователь. Если у выбранного
/// пользователя напоминания выключены, элемент пропускается. Fallback на всех
/// участников и вседневные задачи относятся к следующим пунктам.
async fn plan_reminders(pool: &PgPool, now: DateTime<Utc>) -> Result<u64, sqlx::Error> {
    // Срок ещё не прошёл и не может быть дальше максимального lead time.
    let horizon = now + Duration::minutes(i64::from(MAX_LEAD_MINUTES));
    let candidates: Vec<ReminderCandidate> = sqlx::query_as(
        "SELECT i.id AS item_id, i.household_id,
                COALESCE(i.assignee_user_id, i.created_by_user_id) AS user_id,
                i.due_at, u.settings
         FROM items i
         JOIN lists l
           ON l.id = i.list_id AND l.deleted_at IS NULL AND l.kind = 'tasks'
         JOIN users u
           ON u.id = COALESCE(i.assignee_user_id, i.created_by_user_id)
          AND u.household_id = i.household_id
          AND u.deleted_at IS NULL
         JOIN households h ON h.id = i.household_id AND h.deleted_at IS NULL
         WHERE i.status = 'open' AND i.deleted_at IS NULL
           AND i.due_at > $1 AND i.due_at <= $2
         ORDER BY i.due_at, i.id",
    )
    .bind(now)
    .bind(horizon)
    .fetch_all(pool)
    .await?;

    let mut planned = 0;
    for candidate in candidates {
        let settings = NotificationSettings::from_settings(&candidate.settings);
        if !settings.reminders_enabled {
            continue;
        }
        let scheduled_at = candidate.due_at - Duration::minutes(i64::from(settings.lead_minutes));
        if scheduled_at > now {
            continue;
        }

        planned += sqlx::query(
            "INSERT INTO notification_deliveries (
                 id, household_id, user_id, item_id, occurrence_key, kind,
                 status, scheduled_at, created_at, updated_at
             )
             VALUES ($1, $2, $3, $4, $5, 'reminder', 'pending', $6, $7, $7)
             ON CONFLICT DO NOTHING",
        )
        .bind(Uuid::now_v7())
        .bind(candidate.household_id)
        .bind(candidate.user_id)
        .bind(candidate.item_id)
        .bind(occurrence_key(candidate.due_at))
        .bind(scheduled_at)
        .bind(now)
        .execute(pool)
        .await?
        .rows_affected();
    }

    Ok(planned)
}

#[derive(FromRow)]
struct PendingReminder {
    id: Uuid,
    occurrence_key: String,
    active: bool,
    due_at: Option<DateTime<Utc>>,
}

/// Отменить `pending`-доставки, вхождение которых больше не актуально.
async fn cancel_stale(pool: &PgPool, now: DateTime<Utc>) -> Result<u64, sqlx::Error> {
    let rows: Vec<PendingReminder> = sqlx::query_as(
        "SELECT d.id, d.occurrence_key,
                (i.id IS NOT NULL
                 AND i.status = 'open'
                 AND i.deleted_at IS NULL
                 AND i.due_at IS NOT NULL
                 AND COALESCE(l.deleted_at IS NULL, false)) AS active,
                i.due_at
         FROM notification_deliveries d
         LEFT JOIN items i ON i.id = d.item_id
         LEFT JOIN lists l ON l.id = i.list_id
         WHERE d.status = 'pending' AND d.kind = 'reminder'",
    )
    .fetch_all(pool)
    .await?;

    let stale: Vec<Uuid> = rows
        .into_iter()
        .filter(|row| {
            !row.active
                || row
                    .due_at
                    .map(occurrence_key)
                    .as_deref()
                    .is_none_or(|key| key != row.occurrence_key)
        })
        .map(|row| row.id)
        .collect();

    if stale.is_empty() {
        return Ok(0);
    }

    Ok(sqlx::query(
        "UPDATE notification_deliveries
         SET status = 'cancelled', updated_at = $2
         WHERE id = ANY($1) AND status = 'pending'",
    )
    .bind(&stale)
    .bind(now)
    .execute(pool)
    .await?
    .rows_affected())
}

#[derive(FromRow)]
struct DueReminder {
    id: Uuid,
    occurrence_key: String,
    title: String,
    due_at: DateTime<Utc>,
    telegram_chat_id: i64,
    user_timezone: Option<String>,
    household_timezone: String,
}

/// Передать готовые доставки в outbox.
async fn deliver_due(pool: &PgPool, now: DateTime<Utc>) -> Result<u64, sqlx::Error> {
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM notification_deliveries
         WHERE status = 'pending' AND kind = 'reminder' AND scheduled_at <= $1
         ORDER BY scheduled_at, id
         LIMIT $2",
    )
    .bind(now)
    .bind(DELIVERY_BATCH)
    .fetch_all(pool)
    .await?;

    let mut delivered = 0;
    for id in ids {
        delivered += deliver_one(pool, id, now).await?;
    }
    Ok(delivered)
}

/// Доставить одну запись: outbox и `sent` в одной транзакции.
///
/// Запись блокируется `FOR UPDATE SKIP LOCKED`, поэтому два экземпляра
/// планировщика не обработают её одновременно. `dedupe_key` выводится из
/// идентификатора доставки, поэтому даже повторная постановка не создаст второе
/// сообщение.
async fn deliver_one(pool: &PgPool, id: Uuid, now: DateTime<Utc>) -> Result<u64, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let row: Option<DueReminder> = sqlx::query_as(
        "SELECT d.id, d.occurrence_key,
                i.title, i.due_at, u.telegram_chat_id,
                u.timezone AS user_timezone, h.timezone AS household_timezone
         FROM notification_deliveries d
         JOIN items i ON i.id = d.item_id
         JOIN lists l
           ON l.id = i.list_id AND l.deleted_at IS NULL AND l.kind = 'tasks'
         JOIN users u
           ON u.id = d.user_id AND u.household_id = d.household_id
          AND u.deleted_at IS NULL
         JOIN households h ON h.id = d.household_id AND h.deleted_at IS NULL
         WHERE d.id = $1 AND d.status = 'pending' AND d.kind = 'reminder'
           AND d.scheduled_at <= $2
           AND i.status = 'open' AND i.deleted_at IS NULL AND i.due_at IS NOT NULL
         FOR UPDATE OF d SKIP LOCKED",
    )
    .bind(id)
    .bind(now)
    .fetch_optional(&mut *tx)
    .await?;

    let Some(row) = row else {
        return Ok(0);
    };

    if row.occurrence_key != occurrence_key(row.due_at) {
        sqlx::query(
            "UPDATE notification_deliveries
             SET status = 'cancelled', updated_at = $2
             WHERE id = $1 AND status = 'pending'",
        )
        .bind(row.id)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        return Ok(0);
    }

    let timezone = row
        .user_timezone
        .as_deref()
        .and_then(|value| parse_timezone(value).ok())
        .or_else(|| parse_timezone(&row.household_timezone).ok())
        .unwrap_or(chrono_tz::UTC);
    let due = ru::due_label(Some(row.due_at), None, timezone, now).unwrap_or_default();
    let html = format!(
        "🔔 <b>{}</b>\n{}",
        rich::escape(&row.title),
        rich::escape(&ru::capitalize(&due))
    );
    let dedupe_key = delivery_dedupe_key(row.id);

    let seq = outbox::enqueue(
        &mut tx,
        row.telegram_chat_id,
        &Outgoing::Message { html, rich: false },
        Some(&dedupe_key),
        now,
    )
    .await?;
    // Повторная постановка после сбоя: сообщение уже есть, используем его.
    let seq = match seq {
        Some(seq) => seq,
        None => {
            sqlx::query_scalar("SELECT seq FROM outbox WHERE dedupe_key = $1")
                .bind(&dedupe_key)
                .fetch_one(&mut *tx)
                .await?
        }
    };

    sqlx::query(
        "UPDATE notification_deliveries
         SET status = 'sent', outbox_seq = $2, updated_at = $3
         WHERE id = $1 AND status = 'pending'",
    )
    .bind(row.id)
    .bind(seq)
    .bind(now)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(1)
}

fn delivery_dedupe_key(id: Uuid) -> String {
    format!("notification:reminder:{id}")
}
