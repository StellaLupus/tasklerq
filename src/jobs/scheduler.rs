//! Планировщик уведомлений: тик, напоминания и общие части доставки.
//!
//! Сводка живёт в [`crate::jobs::digest`], уведомление о назначении — в
//! [`crate::jobs::assignment`]. Здесь остаются цикл, тик, общие операции над
//! `notification_deliveries`/`outbox` и напоминания.
//!
//! Один тик делает три шага:
//!
//! 1. отменяет `pending`-доставки, вхождение которых больше не актуально
//!    (задача завершена, удалена, срок сдвинулся, получатель сменился, сводка
//!    выключена или элемент назначен другому);
//! 2. планирует напоминания для открытых задач со временем, у которых наступил
//!    момент `due_at - lead_minutes` получателя, для вседневных задач,
//!    у которых наступило локальное `all_day_time` в день срока; сводки и
//!    уведомления о назначении уже стоят в очереди;
//! 3. ставит готовые `pending`-доставки в `outbox` одной транзакцией со сменой
//!    статуса и будит outbox-воркер.
//!
//! Время передаётся в [`Scheduler::tick`] снаружи, поэтому тесты двигают его без
//! ожидания. Ключ доставки включает пользователя, элемент, вхождение и тип:
//! повторный тик и перезапуск сервиса не создают второе сообщение. Следующее
//! вхождение повторяющейся задачи получает новый срок, а значит и новый
//! `occurrence_key`.
//!
//! Получатель напоминания — исполнитель, иначе автор-пользователь, иначе каждый
//! активный участник household с включёнными напоминаниями. Ошибка БД на одной
//! записи логируется и не прерывает обработку остальных.

use std::sync::Arc;
use std::time::Duration as StdDuration;

use chrono::{DateTime, Duration, NaiveDate, NaiveTime, SecondsFormat, Utc};
use chrono_tz::Tz;
use sqlx::{FromRow, PgConnection, PgPool};
use tokio::sync::{Notify, watch};
use uuid::Uuid;

use crate::domain::recurrence::{parse_timezone, resolve_local};
use crate::domain::settings::{MAX_LEAD_MINUTES, NotificationSettings, parse_hhmm};
use crate::jobs::{assignment, digest};
use crate::storage::repositories::notifications::{self, NewDelivery, NotificationKind};
use crate::telegram::outbox::{self, Outgoing};
use crate::telegram::{rich, ru};

/// Интервал планировщика.
pub const TICK_INTERVAL: StdDuration = StdDuration::from_secs(30);

/// Сколько кандидатов на доставку рассматривается за один тик.
pub(crate) const DELIVERY_BATCH: i64 = 100;

/// Время вседневного напоминания по умолчанию.
const DEFAULT_ALL_DAY_TIME: &str = "09:00";

/// Ключ вхождения: срок в UTC с микросекундной точностью.
pub fn occurrence_key(due_at: DateTime<Utc>) -> String {
    due_at.to_rfc3339_opts(SecondsFormat::Micros, true)
}

/// Ключ вхождения вседневной задачи: локальная дата срока.
pub fn all_day_occurrence_key(due_date: NaiveDate) -> String {
    format!("date:{due_date}")
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
        let cancelled = cancel_stale(&self.pool, now).await?
            + digest::cancel_stale(&self.pool, now).await?
            + assignment::cancel_stale(&self.pool, now).await?;
        let planned =
            plan_reminders(&self.pool, now).await? + digest::plan(&self.pool, now).await?;
        let delivered = deliver_due(&self.pool, now).await?
            + digest::deliver_due(&self.pool, now).await?
            + assignment::deliver_due(&self.pool, now).await?;

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

/// Пояс получателя: личный, иначе household, иначе UTC.
pub(crate) fn resolved_timezone(
    user_timezone: Option<&str>,
    household_timezone: Option<&str>,
) -> Tz {
    user_timezone
        .and_then(|value| parse_timezone(value).ok())
        .or_else(|| household_timezone.and_then(|value| parse_timezone(value).ok()))
        .unwrap_or(chrono_tz::UTC)
}

#[derive(FromRow)]
struct ReminderCandidate {
    item_id: Uuid,
    household_id: Uuid,
    user_id: Uuid,
    due_at: Option<DateTime<Utc>>,
    due_date: Option<NaiveDate>,
    settings: serde_json::Value,
    user_timezone: Option<String>,
    household_timezone: String,
}

/// Запланировать напоминания, момент которых уже наступил.
///
/// Для timed-задач окно задаёт `due_at - lead_minutes`; для вседневных —
/// локальные дата срока и `all_day_time` получателя. Вседневное напоминание
/// создаётся в течение дня срока, даже если момент `all_day_time` уже прошёл.
async fn plan_reminders(pool: &PgPool, now: DateTime<Utc>) -> Result<u64, sqlx::Error> {
    // Timed-срок ещё не прошёл и не может быть дальше максимального lead time.
    let horizon = now + Duration::minutes(i64::from(MAX_LEAD_MINUTES));
    // Календарные даты с запасом на часовые пояса от UTC-12 до UTC+14.
    let today = now.date_naive();
    let date_from = today - Duration::days(1);
    let date_to = today + Duration::days(1);

    let candidates: Vec<ReminderCandidate> = sqlx::query_as(
        // Ключ вхождения вычисляется здесь же, чтобы существующие `pending` и
        // `sent` отсекались до цикла планирования. Формат timed-ключа совпадает
        // с `occurrence_key`: UTC, микросекунды и суффикс `Z`.
        "WITH candidates AS (
             SELECT i.id AS item_id, i.household_id, i.due_at, i.due_date,
                    h.timezone AS household_timezone,
                    COALESCE(i.assignee_user_id, i.created_by_user_id) AS selected_user_id,
                    CASE
                        WHEN i.due_at IS NOT NULL
                        THEN to_char(
                            i.due_at AT TIME ZONE 'UTC',
                            'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"'
                        )
                        ELSE 'date:' || i.due_date::text
                    END AS occurrence_key
             FROM items i
             JOIN lists l
               ON l.id = i.list_id AND l.deleted_at IS NULL
              AND NOT l.is_archived AND l.kind = 'tasks'
             JOIN households h ON h.id = i.household_id AND h.deleted_at IS NULL
             WHERE i.status = 'open' AND i.deleted_at IS NULL
               AND (
                   (i.due_at IS NOT NULL AND i.due_at > $1 AND i.due_at <= $2)
                   OR (i.due_date IS NOT NULL AND i.due_date BETWEEN $3 AND $4)
               )
         )
         SELECT c.item_id, c.household_id, u.id AS user_id,
                c.due_at, c.due_date, u.settings,
                u.timezone AS user_timezone, c.household_timezone
         FROM candidates c
         JOIN users u
           ON u.household_id = c.household_id
          AND u.deleted_at IS NULL
          AND (c.selected_user_id IS NULL OR u.id = c.selected_user_id)
         WHERE NOT EXISTS (
             SELECT 1 FROM notification_deliveries d
             WHERE d.user_id = u.id AND d.item_id = c.item_id
               AND d.occurrence_key = c.occurrence_key
               AND d.kind = 'reminder'
               AND d.status IN ('pending', 'sent')
         )
         ORDER BY c.item_id, u.id",
    )
    .bind(now)
    .bind(horizon)
    .bind(date_from)
    .bind(date_to)
    .fetch_all(pool)
    .await?;

    let mut conn = pool.acquire().await?;
    let mut planned = 0;
    for candidate in candidates {
        let settings = NotificationSettings::from_settings(&candidate.settings);
        if !settings.reminders_enabled {
            continue;
        }
        let timezone = resolved_timezone(
            candidate.user_timezone.as_deref(),
            Some(&candidate.household_timezone),
        );

        let (key, scheduled_at) = match (candidate.due_at, candidate.due_date) {
            (Some(due_at), None) => {
                let scheduled_at = due_at - Duration::minutes(i64::from(settings.lead_minutes));
                if scheduled_at > now {
                    continue;
                }
                (occurrence_key(due_at), scheduled_at)
            }
            (None, Some(due_date)) => {
                if due_date != now.with_timezone(&timezone).date_naive() {
                    continue;
                }
                let time = parse_hhmm(&settings.all_day_time, "all-day reminder time")
                    .unwrap_or_else(|_| default_all_day_time());
                let scheduled_at = match resolve_local(timezone, due_date, time) {
                    Ok(scheduled_at) => scheduled_at,
                    Err(error) => {
                        tracing::error!(
                            item_id = %candidate.item_id,
                            user_id = %candidate.user_id,
                            %error,
                            "failed to resolve all-day reminder time"
                        );
                        continue;
                    }
                };
                if scheduled_at > now {
                    continue;
                }
                (all_day_occurrence_key(due_date), scheduled_at)
            }
            _ => continue,
        };

        match insert_reminder(&mut conn, &candidate, &key, scheduled_at, now).await {
            Ok(rows) => planned += rows,
            Err(error) => tracing::error!(
                item_id = %candidate.item_id,
                user_id = %candidate.user_id,
                %error,
                "failed to plan notification delivery"
            ),
        }
    }

    Ok(planned)
}

fn default_all_day_time() -> NaiveTime {
    parse_hhmm(DEFAULT_ALL_DAY_TIME, "all-day reminder time").expect("valid default time")
}

/// Вставить напоминание, если такого актуального вхождения ещё нет.
async fn insert_reminder(
    conn: &mut PgConnection,
    candidate: &ReminderCandidate,
    key: &str,
    scheduled_at: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Result<u64, sqlx::Error> {
    let delivery = NewDelivery {
        household_id: candidate.household_id,
        user_id: candidate.user_id,
        item_id: Some(candidate.item_id),
        kind: NotificationKind::Reminder,
        occurrence_key: key,
        scheduled_at,
    };
    notifications::insert(conn, &delivery, now).await
}

#[derive(FromRow)]
struct ReminderRow {
    id: Uuid,
    household_id: Uuid,
    user_id: Uuid,
    occurrence_key: String,
    title: Option<String>,
    item_id: Option<Uuid>,
    item_status: Option<String>,
    item_deleted_at: Option<DateTime<Utc>>,
    due_at: Option<DateTime<Utc>>,
    due_date: Option<NaiveDate>,
    assignee_user_id: Option<Uuid>,
    created_by_user_id: Option<Uuid>,
    list_id: Option<Uuid>,
    list_kind: Option<String>,
    list_deleted_at: Option<DateTime<Utc>>,
    list_archived: Option<bool>,
    recipient_id: Option<Uuid>,
    recipient_deleted_at: Option<DateTime<Utc>>,
    recipient_household_id: Option<Uuid>,
    recipient_timezone: Option<String>,
    telegram_chat_id: Option<i64>,
    household_found: Option<Uuid>,
    household_deleted_at: Option<DateTime<Utc>>,
    household_timezone: Option<String>,
}

impl ReminderRow {
    /// Пояс получателя, иначе household, иначе UTC.
    fn timezone(&self) -> Tz {
        resolved_timezone(
            self.recipient_timezone.as_deref(),
            self.household_timezone.as_deref(),
        )
    }

    /// Ключ текущего вхождения элемента.
    fn current_key(&self) -> Option<String> {
        match (self.due_at, self.due_date) {
            (Some(due_at), None) => Some(occurrence_key(due_at)),
            (None, Some(due_date)) => Some(all_day_occurrence_key(due_date)),
            _ => None,
        }
    }

    /// Элемент существует, открыт и лежит в неархивном списке задач.
    fn item_valid(&self) -> bool {
        self.item_id.is_some()
            && self.item_deleted_at.is_none()
            && self.item_status.as_deref() == Some("open")
            && self.title.is_some()
            && self.list_id.is_some()
            && self.list_kind.as_deref() == Some("tasks")
            && self.list_deleted_at.is_none()
            && self.list_archived != Some(true)
            && (self.due_at.is_some() ^ self.due_date.is_some())
    }

    /// Получатель всё ещё выбран по правилу исполнитель → автор → участники.
    fn recipient_valid(&self) -> bool {
        if self.recipient_id.is_none()
            || self.recipient_deleted_at.is_some()
            || self.recipient_household_id != Some(self.household_id)
        {
            return false;
        }

        match self.assignee_user_id.or(self.created_by_user_id) {
            Some(selected) => selected == self.user_id,
            // Оба поля пусты — получателем может быть любой активный участник.
            None => true,
        }
    }

    /// Household существует.
    fn household_valid(&self) -> bool {
        self.household_found.is_some() && self.household_deleted_at.is_none()
    }

    /// Вседневное напоминание живёт только до конца локального дня срока.
    fn in_window(&self, now: DateTime<Utc>) -> bool {
        match self.due_date {
            Some(due_date) => now.with_timezone(&self.timezone()).date_naive() == due_date,
            None => true,
        }
    }

    /// Запись больше не должна быть доставлена.
    fn is_stale(&self, now: DateTime<Utc>) -> bool {
        !self.item_valid()
            || !self.recipient_valid()
            || !self.household_valid()
            || self.current_key().as_deref() != Some(self.occurrence_key.as_str())
            || !self.in_window(now)
    }
}

pub(crate) const REMINDER_ROW_COLUMNS: &str = "d.id, d.household_id, d.user_id, d.occurrence_key,
        i.title, i.id AS item_id, i.status::text AS item_status,
        i.deleted_at AS item_deleted_at, i.due_at, i.due_date,
        i.assignee_user_id, i.created_by_user_id,
        l.id AS list_id, l.kind::text AS list_kind, l.deleted_at AS list_deleted_at,
        l.is_archived AS list_archived,
        u.id AS recipient_id, u.deleted_at AS recipient_deleted_at,
        u.household_id AS recipient_household_id, u.timezone AS recipient_timezone,
        u.telegram_chat_id,
        h.id AS household_found, h.deleted_at AS household_deleted_at,
        h.timezone AS household_timezone";

/// Отменить `pending`-доставки, вхождение которых больше не актуально.
async fn cancel_stale(pool: &PgPool, now: DateTime<Utc>) -> Result<u64, sqlx::Error> {
    let sql = format!(
        "SELECT {REMINDER_ROW_COLUMNS}
         FROM notification_deliveries d
         LEFT JOIN items i ON i.id = d.item_id
         LEFT JOIN lists l ON l.id = i.list_id
         LEFT JOIN users u ON u.id = d.user_id
         LEFT JOIN households h ON h.id = d.household_id
         WHERE d.status = 'pending' AND d.kind = 'reminder'"
    );
    let rows: Vec<ReminderRow> = sqlx::query_as(&sql).fetch_all(pool).await?;

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
                "failed to cancel stale notification delivery"
            ),
        }
    }

    Ok(cancelled)
}

/// Пометить `pending`-доставку отменённой в текущей транзакции.
pub(crate) async fn mark_cancelled(
    conn: &mut PgConnection,
    id: Uuid,
    now: DateTime<Utc>,
) -> Result<u64, sqlx::Error> {
    Ok(sqlx::query(
        "UPDATE notification_deliveries
         SET status = 'cancelled', updated_at = $2
         WHERE id = $1 AND status = 'pending'",
    )
    .bind(id)
    .bind(now)
    .execute(&mut *conn)
    .await?
    .rows_affected())
}

/// Пометить доставку переданной в outbox.
pub(crate) async fn mark_sent(
    conn: &mut PgConnection,
    id: Uuid,
    seq: i64,
    now: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE notification_deliveries
         SET status = 'sent', outbox_seq = $2, updated_at = $3
         WHERE id = $1 AND status = 'pending'",
    )
    .bind(id)
    .bind(seq)
    .bind(now)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Поставить сообщение в outbox и вернуть его `seq`.
///
/// Повторная постановка после сбоя использует уже существующую запись по
/// `dedupe_key`.
pub(crate) async fn enqueue_message(
    conn: &mut PgConnection,
    chat_id: i64,
    dedupe_key: &str,
    html: String,
    rich: bool,
    now: DateTime<Utc>,
) -> Result<i64, sqlx::Error> {
    let outgoing = Outgoing::Message { html, rich };
    match outbox::enqueue(conn, chat_id, &outgoing, Some(dedupe_key), now).await? {
        Some(seq) => Ok(seq),
        None => {
            sqlx::query_scalar("SELECT seq FROM outbox WHERE dedupe_key = $1")
                .bind(dedupe_key)
                .fetch_one(&mut *conn)
                .await
        }
    }
}

/// Отменить `pending`-доставку через пул.
pub(crate) async fn cancel_delivery(
    pool: &PgPool,
    id: Uuid,
    now: DateTime<Utc>,
) -> Result<u64, sqlx::Error> {
    let mut conn = pool.acquire().await?;
    mark_cancelled(&mut conn, id, now).await
}

/// Передать готовые напоминания в outbox.
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
        match deliver_one(pool, id, now).await {
            Ok(rows) => delivered += rows,
            Err(error) => tracing::error!(
                delivery_id = %id,
                %error,
                "failed to deliver notification reminder"
            ),
        }
    }
    Ok(delivered)
}

/// Доставить одно напоминание: outbox и `sent` в одной транзакции.
///
/// Запись блокируется `FOR UPDATE SKIP LOCKED`, поэтому два экземпляра
/// планировщика не обработают её одновременно. `dedupe_key` выводится из
/// идентификатора доставки, поэтому даже повторная постановка не создаст второе
/// сообщение. Недоступный получатель или устаревшее вхождение переводят запись
/// в `cancelled`, а не оставляют её навсегда в `pending`.
async fn deliver_one(pool: &PgPool, id: Uuid, now: DateTime<Utc>) -> Result<u64, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let sql = format!(
        "SELECT {REMINDER_ROW_COLUMNS}
         FROM notification_deliveries d
         LEFT JOIN items i ON i.id = d.item_id
         LEFT JOIN lists l ON l.id = i.list_id
         LEFT JOIN users u ON u.id = d.user_id
         LEFT JOIN households h ON h.id = d.household_id
         WHERE d.id = $1 AND d.status = 'pending' AND d.kind = 'reminder'
           AND d.scheduled_at <= $2
         FOR UPDATE OF d SKIP LOCKED"
    );
    let row: Option<ReminderRow> = sqlx::query_as(&sql)
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

    // `is_stale` уже проверил эти поля; явная проверка не даёт отправить
    // сообщение в чат 0 или с пустым названием при будущих изменениях схемы.
    let (Some(title), Some(chat_id)) = (row.title.as_deref(), row.telegram_chat_id) else {
        mark_cancelled(&mut tx, row.id, now).await?;
        tx.commit().await?;
        return Ok(0);
    };
    let timezone = row.timezone();
    let due = ru::due_label(row.due_at, row.due_date, timezone, now).unwrap_or_default();
    let html = format!(
        "🔔 <b>{}</b>\n{}",
        rich::escape(title),
        rich::escape(&ru::capitalize(&due))
    );
    let dedupe_key = delivery_dedupe_key(row.id);

    let seq = enqueue_message(&mut tx, chat_id, &dedupe_key, html, false, now).await?;
    mark_sent(&mut tx, row.id, seq, now).await?;

    tx.commit().await?;
    Ok(1)
}

fn delivery_dedupe_key(id: Uuid) -> String {
    format!("notification:reminder:{id}")
}
