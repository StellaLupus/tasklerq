//! Планировщик уведомлений: тик, напоминания и общие части доставки.
//!
//! Сводка живёт в [`crate::jobs::digest`], уведомление о назначении — в
//! [`crate::jobs::assignment`]. Здесь остаются цикл, тик, общие операции над
//! `notification_deliveries`/`outbox` и напоминания.
//!
//! Один тик делает три шага:
//!
//! 1. отменяет `pending`-доставки, вхождение которых больше не актуально
//!    (задача завершена, удалена, срок сдвинулся, получатель сменился,
//!    напоминания или сводка выключены, элемент назначен другому);
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

use crate::domain::error::DomainError;
use crate::domain::recurrence::{parse_timezone, resolve_local};
use crate::domain::settings::{MAX_LEAD_MINUTES, NotificationSettings, parse_hhmm};
use crate::jobs::{assignment, digest};
use crate::storage::repositories::notifications::{
    self, NewDelivery, NotificationKind, snooze_key_time,
};
use crate::storage::repositories::{items, lists};
use crate::telegram::bot::notify;
use crate::telegram::outbox::{self, Outgoing};

use super::notice::{NOTICE_ROW_COLUMNS, NOTICE_ROW_FROM, NoticeRow};

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

        // Настройки читаются на каждом тике: новое время сводки, lead time,
        // пояс или границы тихих часов применяются без перезапуска. Время
        // считается идемпотентно, поэтому повторный тик не сбрасывает уже
        // отложенную запись обратно в прошлое.
        reschedule_reminders(&self.pool, now).await?;
        digest::reschedule(&self.pool, now).await?;
        assignment::reschedule(&self.pool, now).await?;

        // Сначала собираем отложенные тихими часами уведомления в одно
        // сообщение; одиночные и неотложенные записи затем уходят обычным
        // путём. Сводка в батч не входит и доставляется отдельно.
        let delivered = deliver_batches(&self.pool, now).await?
            + deliver_due(&self.pool, now).await?
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

/// Ближайший конец тихих часов, если `instant` попадает в интервал.
///
/// Начало интервала включительно, конец исключительно. Интервал может
/// переходить через полночь: 22:00–08:00 означает «с 22:00 до полуночи и с
/// полуночи до 08:00». Время считается в поясе получателя.
pub(crate) fn quiet_end_at(
    instant: DateTime<Utc>,
    timezone: Tz,
    settings: &NotificationSettings,
) -> Option<DateTime<Utc>> {
    if !settings.quiet_hours_enabled {
        return None;
    }
    let start = match parse_hhmm(&settings.quiet_start, "quiet hours start") {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, "invalid quiet hours start; delivering without quiet hours");
            return None;
        }
    };
    let end = match parse_hhmm(&settings.quiet_end, "quiet hours end") {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, "invalid quiet hours end; delivering without quiet hours");
            return None;
        }
    };
    if start == end {
        tracing::warn!("quiet hours start equals end; delivering without quiet hours");
        return None;
    }

    let local = instant.with_timezone(&timezone);
    let time = local.time();
    let date = local.date_naive();
    let inside = if start < end {
        time >= start && time < end
    } else {
        time >= start || time < end
    };
    if !inside {
        return None;
    }

    // Конец в тот же день, кроме случая, когда мы уже после начала интервала,
    // а конец — за полночь (22:00 → 08:00 следующего дня).
    let end_date = if start < end || time < end {
        date
    } else {
        date + Duration::days(1)
    };
    match resolve_local(timezone, end_date, end) {
        Ok(end) => Some(end),
        Err(error) => {
            tracing::warn!(
                %error,
                %timezone,
                %end_date,
                "failed to resolve quiet hours end; delivering without quiet hours"
            );
            None
        }
    }
}

/// Стабильное время доставки по базовому времени и текущим тихим часам.
///
/// `base` — время, когда уведомление должно было уйти по своей природе
/// (срок минус lead time, время сводки, момент назначения). Если оно ещё
/// впереди, тихие часы применяются к нему. Если уже прошло — к текущему
/// моменту: сервис мог проснуться внутри тихих часов.
///
/// Функция идемпотентна: пока запись остаётся в том же тихом интервале,
/// повторный расчёт возвращает то же время.
pub(crate) fn desired_schedule(
    base: DateTime<Utc>,
    now: DateTime<Utc>,
    timezone: Tz,
    settings: &NotificationSettings,
) -> DateTime<Utc> {
    if base > now {
        quiet_end_at(base, timezone, settings).unwrap_or(base)
    } else {
        quiet_end_at(now, timezone, settings).unwrap_or(base)
    }
}

/// Обновить `scheduled_at`, только если время или флаг переноса изменились.
pub(crate) async fn reschedule_if_changed(
    conn: &mut PgConnection,
    id: Uuid,
    desired: DateTime<Utc>,
    deferred: bool,
    now: DateTime<Utc>,
) -> Result<u64, sqlx::Error> {
    Ok(sqlx::query(
        "UPDATE notification_deliveries
         SET scheduled_at = $2, deferred = deferred OR $3, updated_at = $4
         WHERE id = $1 AND status = 'pending'
           AND (scheduled_at <> $2 OR ($3 AND NOT deferred))",
    )
    .bind(id)
    .bind(desired)
    .bind(deferred)
    .bind(now)
    .execute(&mut *conn)
    .await?
    .rows_affected())
}

/// Перенести уже заблокированную запись, если её момент доставки в тихих часах.
///
/// Вызывается в `deliver_one` под `FOR UPDATE`: даже если другой экземпляр
/// планировщика успел поставить `scheduled_at <= now`, доставка не произойдёт
/// внутри тихого интервала.
pub(crate) async fn postpone_for_quiet(
    conn: &mut PgConnection,
    id: Uuid,
    settings: &serde_json::Value,
    timezone: Tz,
    now: DateTime<Utc>,
) -> Result<bool, sqlx::Error> {
    let settings = NotificationSettings::from_settings(settings);
    let Some(end) = quiet_end_at(now, timezone, &settings) else {
        return Ok(false);
    };
    sqlx::query(
        "UPDATE notification_deliveries
         SET scheduled_at = $2, deferred = true, updated_at = $3
         WHERE id = $1 AND status = 'pending'",
    )
    .bind(id)
    .bind(end)
    .bind(now)
    .execute(&mut *conn)
    .await?;
    Ok(true)
}

/// Пересчитать `pending`-напоминания по текущим настройкам получателя.
///
/// Время берётся из срока и настроек, а не из старой `scheduled_at`: смена
/// lead time, времени вседневного напоминания или пояса сдвигает ещё не
/// доставленную запись. Уже отложенная тихими часами запись не
/// пересчитывается: её время зафиксировано до выпуска.
async fn reschedule_reminders(pool: &PgPool, now: DateTime<Utc>) -> Result<u64, sqlx::Error> {
    let sql = format!(
        "SELECT {NOTICE_ROW_COLUMNS}
         {NOTICE_ROW_FROM}
         WHERE d.status = 'pending' AND d.kind IN ('reminder', 'snooze')"
    );
    let rows: Vec<NoticeRow> = sqlx::query_as(&sql).fetch_all(pool).await?;

    let mut conn = pool.acquire().await?;
    let mut rescheduled = 0;
    for row in rows {
        // Момент выпуска отложенного тихими часами уведомления фиксируется при
        // переносе. Последующие тики не пересчитывают его: выключение или
        // смена границ тихих часов не ускоряет уже отложенную доставку.
        if row.deferred {
            continue;
        }
        let settings = row.settings();
        let timezone = row.timezone();
        let intended = if row.is_snooze() {
            // Время snooze зафиксировано в ключе: повторный расчёт не должен
            // сдвигать его из-за позднего тика.
            match snooze_key_time(&row.occurrence_key) {
                Some(at) => at,
                None => continue,
            }
        } else {
            if !row.item_valid()
                || row.current_key().as_deref() != Some(row.occurrence_key.as_str())
            {
                continue;
            }
            match (row.item_due_at, row.item_due_date) {
                (Some(due_at), None) => {
                    due_at - Duration::minutes(i64::from(settings.lead_minutes))
                }
                (None, Some(due_date)) => {
                    let time = parse_hhmm(&settings.all_day_time, "all-day reminder time")
                        .unwrap_or_else(|_| default_all_day_time());
                    match resolve_local(timezone, due_date, time) {
                        Ok(at) => at,
                        Err(_) => continue,
                    }
                }
                _ => continue,
            }
        };
        let desired = desired_schedule(intended, now, timezone, &settings);
        let deferred = desired != intended;
        rescheduled += reschedule_if_changed(&mut conn, row.id, desired, deferred, now).await?;
    }
    Ok(rescheduled)
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

/// Отменить `pending`-доставки, вхождение которых больше не актуально.
async fn cancel_stale(pool: &PgPool, now: DateTime<Utc>) -> Result<u64, sqlx::Error> {
    let sql = format!(
        "SELECT {NOTICE_ROW_COLUMNS}
         {NOTICE_ROW_FROM}
         WHERE d.status = 'pending' AND d.kind IN ('reminder', 'snooze')"
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

/// Отложенная тихими часами доставка и её блок сводного сообщения.
struct BatchCandidate {
    delivery_id: Uuid,
    scheduled_at: DateTime<Utc>,
    entry: notify::BatchEntry,
}

/// Получатель и чат сводного сообщения.
struct BatchIdentity {
    household_id: Uuid,
    chat_id: i64,
    telegram_user_id: i64,
    timezone: Tz,
}

/// Собрать одно сводное сообщение на получателя из отложенных доставок.
///
/// Сборка и смена статусов выполняются в одной транзакции, а строки
/// блокируются `FOR UPDATE SKIP LOCKED`, поэтому два экземпляра планировщика не
/// отправят батч дважды. Если после проверки актуальности осталась одна
/// доставка, она не отправляется здесь: её доставит обычный путь.
async fn deliver_batches(pool: &PgPool, now: DateTime<Utc>) -> Result<u64, sqlx::Error> {
    let users: Vec<Uuid> = sqlx::query_scalar(
        "SELECT DISTINCT user_id
         FROM notification_deliveries
         WHERE status = 'pending' AND deferred AND scheduled_at <= $1
           AND kind IN ('reminder', 'snooze', 'assignment')
         ORDER BY user_id
         LIMIT $2",
    )
    .bind(now)
    .bind(DELIVERY_BATCH)
    .fetch_all(pool)
    .await?;

    let mut delivered = 0;
    for user_id in users {
        match deliver_batch_for_user(pool, user_id, now).await {
            Ok(rows) => delivered += rows,
            Err(error) => tracing::error!(
                %user_id,
                %error,
                "failed to deliver notification batch"
            ),
        }
    }
    Ok(delivered)
}

/// SQL отложенных доставок получателя для сборки сводного сообщения.
///
/// `due_only` ограничивает выборку записями, срок которых уже наступил: так
/// работает тик. Ручной выпуск забирает все отложенные записи получателя.
fn deferred_row_sql(due_only: bool) -> String {
    let due = if due_only {
        "AND d.scheduled_at <= $2"
    } else {
        ""
    };
    format!(
        "SELECT {NOTICE_ROW_COLUMNS}
         {NOTICE_ROW_FROM}
         WHERE d.user_id = $1 AND d.status = 'pending' AND d.deferred
           {due}
           AND d.kind IN ('reminder', 'snooze', 'assignment')
         ORDER BY d.scheduled_at, d.id
         FOR UPDATE OF d SKIP LOCKED"
    )
}

/// Взять транзакционную блокировку получателя.
///
/// Все пути доставки берут её до блокировки строк: тогда два экземпляра
/// планировщика не соберут сводное сообщение из части отложенных строк
/// (`SKIP LOCKED` пропустил бы строки, которые в этот момент доставляет
/// одиночный путь). Ключ — последние восемь байт `user_id`: у UUIDv7 первые
/// восемь байт заняты временным префиксом и у соседних записей совпадают.
/// Коллизии двух UUID практически невозможны. Блокировка не ждёт: занятого
/// получателя тик пропускает.
pub(crate) async fn try_recipient_lock(
    conn: &mut PgConnection,
    user_id: Uuid,
) -> Result<bool, sqlx::Error> {
    let key = i64::from_be_bytes(
        user_id.as_bytes()[8..]
            .try_into()
            .expect("uuid has at least sixteen bytes"),
    );
    sqlx::query_scalar("SELECT pg_try_advisory_xact_lock($1)")
        .bind(key)
        .fetch_one(conn)
        .await
}

async fn deliver_batch_for_user(
    pool: &PgPool,
    user_id: Uuid,
    now: DateTime<Utc>,
) -> Result<u64, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let delivered = deliver_deferred_batch(&mut tx, user_id, now, true).await?;
    tx.commit().await?;
    Ok(delivered)
}

/// Собрать одно сводное сообщение на получателя из отложенных доставок.
///
/// Сборка и смена статусов выполняются в одной транзакции, а строки
/// блокируются `FOR UPDATE SKIP LOCKED`, поэтому два экземпляра планировщика не
/// отправят батч дважды. При `due_only` одна оставшаяся доставка не
/// отправляется здесь: её доставит обычный путь. Ручной выпуск (`due_only =
/// false`) доставляет единственную запись сразу, не дожидаясь её
/// зафиксированного времени.
pub(crate) async fn deliver_deferred_batch(
    conn: &mut PgConnection,
    user_id: Uuid,
    now: DateTime<Utc>,
    due_only: bool,
) -> Result<u64, sqlx::Error> {
    if !try_recipient_lock(&mut *conn, user_id).await? {
        return Ok(0);
    }
    let rows: Vec<NoticeRow> = sqlx::query_as(&deferred_row_sql(due_only))
        .bind(user_id)
        .bind(now)
        .fetch_all(&mut *conn)
        .await?;

    let mut identity: Option<BatchIdentity> = None;
    let mut candidates = Vec::new();
    for row in rows {
        if row.is_stale(now) {
            mark_cancelled(&mut *conn, row.id, now).await?;
            continue;
        }
        // Границы могли сдвинуть после переноса: в момент выпуска тихие часы
        // проверяются ещё раз и доставка снова фиксируется на их конце.
        if let Some(settings) = row.recipient_settings.as_ref()
            && postpone_for_quiet(&mut *conn, row.id, settings, row.timezone(), now).await?
        {
            continue;
        }
        let (Some(item_id), Some(chat_id), Some(telegram_user_id)) =
            (row.item_id, row.telegram_chat_id, row.telegram_user_id)
        else {
            mark_cancelled(&mut *conn, row.id, now).await?;
            continue;
        };
        let item = match items::get(&mut *conn, row.household_id, item_id).await {
            Ok(item) => item,
            Err(DomainError::Database(error)) => return Err(error),
            Err(_) => {
                mark_cancelled(&mut *conn, row.id, now).await?;
                continue;
            }
        };
        let list = match lists::get(&mut *conn, row.household_id, item.list_id).await {
            Ok(list) => list,
            Err(DomainError::Database(error)) => return Err(error),
            Err(_) => {
                mark_cancelled(&mut *conn, row.id, now).await?;
                continue;
            }
        };
        if identity.is_none() {
            identity = Some(BatchIdentity {
                household_id: row.household_id,
                chat_id,
                telegram_user_id,
                timezone: row.timezone(),
            });
        }
        candidates.push(BatchCandidate {
            delivery_id: row.id,
            scheduled_at: row.scheduled_at,
            entry: notify::BatchEntry {
                item_id,
                kind: row.kind,
                title: item.title.clone(),
                when: notify::when_line(
                    item.due_at,
                    item.due_date,
                    &list.name,
                    row.timezone(),
                    now,
                ),
                version: item.version,
                state: notify::BatchEntryState::Open,
            },
        });
    }

    if candidates.len() < 2 {
        // Ручной выпуск одного уведомления: обычное сообщение сразу, а не в
        // зафиксированное время.
        if candidates.len() == 1 && !due_only {
            let candidate = candidates.pop().expect("one candidate");
            return deliver_single_deferred(conn, &candidate, user_id, now).await;
        }
        return Ok(0);
    }
    let Some(identity) = identity else {
        return Ok(0);
    };
    candidates.sort_by_key(|candidate| (candidate.scheduled_at, candidate.delivery_id));
    let entries: Vec<notify::BatchEntry> = candidates
        .iter()
        .map(|candidate| candidate.entry.clone())
        .collect();

    let batch_id = Uuid::now_v7();
    notifications::create_batch(
        &mut *conn,
        batch_id,
        identity.household_id,
        user_id,
        identity.chat_id,
        now,
    )
    .await?;
    let page = &entries[..entries.len().min(notify::BATCH_PAGE_SIZE)];
    let ui = notify::batch_ui(batch_id, 0, page, entries.len(), None, identity.timezone);
    let html = notify::render_notice(
        &mut *conn,
        identity.telegram_user_id,
        identity.household_id,
        &ui,
        now,
    )
    .await?;
    let dedupe_key = format!("notification:batch:{batch_id}");
    let seq = enqueue_message(&mut *conn, identity.chat_id, &dedupe_key, html, true, now).await?;
    for candidate in candidates {
        mark_batch_sent(
            &mut *conn,
            candidate.delivery_id,
            seq,
            batch_id,
            candidate.entry.version,
            now,
        )
        .await?;
    }

    Ok(1)
}

/// Доставить единственную отложенную запись обычным сообщением.
async fn deliver_single_deferred(
    conn: &mut PgConnection,
    candidate: &BatchCandidate,
    user_id: Uuid,
    now: DateTime<Utc>,
) -> Result<u64, sqlx::Error> {
    match candidate.entry.kind {
        NotificationKind::Reminder | NotificationKind::Snooze => {
            deliver_one_in_conn(conn, candidate.delivery_id, user_id, now, false).await
        }
        NotificationKind::Assignment => {
            assignment::deliver_one_in_conn(conn, candidate.delivery_id, user_id, now, false).await
        }
        NotificationKind::Digest => Ok(0),
    }
}

/// Число актуальных отложенных уведомлений получателя без утренней сводки.
///
/// Используется для баннера ручного выпуска; сводка приходит отдельным
/// сообщением и в счётчик не входит.
pub(crate) async fn deferred_notice_count(
    conn: &mut PgConnection,
    user_id: Uuid,
    now: DateTime<Utc>,
) -> Result<u64, sqlx::Error> {
    let sql = format!(
        "SELECT {NOTICE_ROW_COLUMNS}
         {NOTICE_ROW_FROM}
         WHERE d.user_id = $1 AND d.status = 'pending' AND d.deferred
           AND d.kind IN ('reminder', 'snooze', 'assignment')
         ORDER BY d.scheduled_at, d.id"
    );
    let rows: Vec<NoticeRow> = sqlx::query_as(&sql)
        .bind(user_id)
        .fetch_all(&mut *conn)
        .await?;
    Ok(rows
        .iter()
        .filter(|row| !row.is_stale(now))
        .count()
        .try_into()
        .unwrap_or(u64::MAX))
}

/// Перенести все отложенные записи получателя на конец нового интервала.
///
/// Вызывается после правки тихих часов, если сейчас по новым настройкам
/// по-прежнему тихо: выпускать нечего, но старое зафиксированное время больше
/// не соответствует новому интервалу.
pub(crate) async fn postpone_deferred_for_user(
    conn: &mut PgConnection,
    user_id: Uuid,
    end: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Result<u64, sqlx::Error> {
    Ok(sqlx::query(
        "UPDATE notification_deliveries
         SET scheduled_at = $2, deferred = true, updated_at = $3
         WHERE user_id = $1 AND status = 'pending' AND deferred
           AND scheduled_at <> $2",
    )
    .bind(user_id)
    .bind(end)
    .bind(now)
    .execute(&mut *conn)
    .await?
    .rows_affected())
}

/// Немедленно выпустить все отложенные уведомления получателя.
///
/// Вызывается из обработчика настроек в его транзакции. Сводка уходит
/// отдельным сообщением после сводного.
pub(crate) async fn release_deferred_for_user(
    conn: &mut PgConnection,
    user_id: Uuid,
    now: DateTime<Utc>,
) -> Result<u64, sqlx::Error> {
    let mut delivered = deliver_deferred_batch(&mut *conn, user_id, now, false).await?;
    delivered += digest::release_deferred(&mut *conn, user_id, now).await?;
    Ok(delivered)
}

/// Пометить доставку переданной в outbox вместе со сводным сообщением.
async fn mark_batch_sent(
    conn: &mut PgConnection,
    id: Uuid,
    seq: i64,
    batch_id: Uuid,
    version: i32,
    now: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE notification_deliveries
         SET status = 'sent', outbox_seq = $2, batch_id = $3, batch_version = $4,
             batch_state = NULL, batch_snoozed_until = NULL, updated_at = $5
         WHERE id = $1 AND status = 'pending'",
    )
    .bind(id)
    .bind(seq)
    .bind(batch_id)
    .bind(version)
    .bind(now)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Передать готовые напоминания в outbox.
async fn deliver_due(pool: &PgPool, now: DateTime<Utc>) -> Result<u64, sqlx::Error> {
    let ids: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT id, user_id FROM notification_deliveries
         WHERE status = 'pending' AND kind IN ('reminder', 'snooze')
           AND scheduled_at <= $1
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
async fn deliver_one(
    pool: &PgPool,
    id: Uuid,
    user_id: Uuid,
    now: DateTime<Utc>,
) -> Result<u64, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let delivered = deliver_one_in_conn(&mut tx, id, user_id, now, true).await?;
    tx.commit().await?;
    Ok(delivered)
}

/// Доставить одно напоминание в уже открытой транзакции.
///
/// `due_only = false` используется ручным выпуском: запись забирается сразу,
/// не дожидаясь зафиксированного времени. Запись блокируется `FOR UPDATE SKIP
/// LOCKED`, поэтому два экземпляра планировщика не обработают её одновременно.
pub(crate) async fn deliver_one_in_conn(
    conn: &mut PgConnection,
    id: Uuid,
    user_id: Uuid,
    now: DateTime<Utc>,
    due_only: bool,
) -> Result<u64, sqlx::Error> {
    // Тот же получатель не должен одновременно собираться в сводное другим
    // экземпляром планировщика.
    if !try_recipient_lock(&mut *conn, user_id).await? {
        return Ok(0);
    }
    let due = if due_only {
        "AND d.scheduled_at <= $2"
    } else {
        ""
    };
    let sql = format!(
        "SELECT {NOTICE_ROW_COLUMNS}
         {NOTICE_ROW_FROM}
         WHERE d.id = $1 AND d.status = 'pending'
           AND d.kind IN ('reminder', 'snooze')
           {due}
         FOR UPDATE OF d SKIP LOCKED"
    );
    let row: Option<NoticeRow> = sqlx::query_as(&sql)
        .bind(id)
        .bind(now)
        .fetch_optional(&mut *conn)
        .await?;

    let Some(row) = row else {
        return Ok(0);
    };

    if row.is_stale(now) {
        mark_cancelled(&mut *conn, row.id, now).await?;
        return Ok(0);
    }
    let timezone = row.timezone();
    // Проверка под блокировкой закрывает гонку с другим экземпляром
    // планировщика: даже «сброшенная» запись не уйдёт внутри тихих часов.
    if let Some(settings) = row.recipient_settings.as_ref()
        && postpone_for_quiet(&mut *conn, row.id, settings, timezone, now).await?
    {
        return Ok(0);
    }

    // `is_stale` уже проверил эти поля; явная проверка не даёт отправить
    // сообщение в чат 0 или с пустым названием при будущих изменениях схемы.
    let (Some(chat_id), Some(telegram_user_id), Some(item_id)) =
        (row.telegram_chat_id, row.telegram_user_id, row.item_id)
    else {
        mark_cancelled(&mut *conn, row.id, now).await?;
        return Ok(0);
    };
    let item = match items::get(&mut *conn, row.household_id, item_id).await {
        Ok(item) => item,
        Err(DomainError::Database(error)) => return Err(error),
        Err(_) => {
            mark_cancelled(&mut *conn, row.id, now).await?;
            return Ok(0);
        }
    };
    let list = match lists::get(&mut *conn, row.household_id, item.list_id).await {
        Ok(list) => list,
        Err(DomainError::Database(error)) => return Err(error),
        Err(_) => {
            mark_cancelled(&mut *conn, row.id, now).await?;
            return Ok(0);
        }
    };

    // Snooze и вседневное напоминание показывают «Пора». Для timed-задачи
    // заголовок считает фактическое время до срока: тихие часы или поздний тик
    // уже могли сдвинуть доставку ближе к сроку.
    let lead_minutes = if row.is_snooze() || row.item_due_date.is_some() {
        None
    } else {
        row.item_due_at.and_then(|due| {
            let seconds = (due - now).num_seconds();
            (seconds > 0).then_some((seconds + 59) / 60)
        })
    };
    let ui = notify::reminder_ui(&item, &list, timezone, now, lead_minutes);
    let html =
        notify::render_notice(&mut *conn, telegram_user_id, row.household_id, &ui, now).await?;
    let dedupe_key = delivery_dedupe_key(row.id, row.kind);

    let seq = enqueue_message(&mut *conn, chat_id, &dedupe_key, html, true, now).await?;
    mark_sent(&mut *conn, row.id, seq, now).await?;

    Ok(1)
}

fn delivery_dedupe_key(id: Uuid, kind: NotificationKind) -> String {
    format!("notification:{}:{id}", kind.as_str())
}
