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
/// доставленную запись. Запись, уже отложенная тихими часами, тоже
/// пересчитывается — если тихие часы выключили, она уйдёт на следующем тике.
async fn reschedule_reminders(pool: &PgPool, now: DateTime<Utc>) -> Result<u64, sqlx::Error> {
    let sql = format!(
        "SELECT {REMINDER_ROW_COLUMNS}
         FROM notification_deliveries d
         LEFT JOIN items i ON i.id = d.item_id
         LEFT JOIN lists l ON l.id = i.list_id
         LEFT JOIN users u ON u.id = d.user_id
         LEFT JOIN households h ON h.id = d.household_id
         WHERE d.status = 'pending' AND d.kind IN ('reminder', 'snooze')"
    );
    let rows: Vec<ReminderRow> = sqlx::query_as(&sql).fetch_all(pool).await?;

    let mut conn = pool.acquire().await?;
    let mut rescheduled = 0;
    for row in rows {
        let settings = NotificationSettings::from_settings(&row.recipient_settings);
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
            match (row.due_at, row.due_date) {
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

#[derive(FromRow)]
struct ReminderRow {
    id: Uuid,
    household_id: Uuid,
    user_id: Uuid,
    occurrence_key: String,
    kind: String,
    deferred: bool,
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
    recipient_settings: serde_json::Value,
    telegram_user_id: Option<i64>,
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

    /// Отложенное напоминание, а не напоминание о текущем вхождении срока.
    fn is_snooze(&self) -> bool {
        self.kind == "snooze"
    }

    /// Элемент существует, открыт и лежит в неархивном списке задач.
    ///
    /// Срок не требуется: snooze остаётся актуальным, даже если у задачи
    /// поменяли срок после нажатия.
    fn item_alive(&self) -> bool {
        self.item_id.is_some()
            && self.item_deleted_at.is_none()
            && self.item_status.as_deref() == Some("open")
            && self.title.is_some()
            && self.list_id.is_some()
            && self.list_kind.as_deref() == Some("tasks")
            && self.list_deleted_at.is_none()
            && self.list_archived != Some(true)
    }

    /// У напоминания о сроке есть ровно один вид срока.
    fn item_valid(&self) -> bool {
        self.item_alive() && (self.due_at.is_some() ^ self.due_date.is_some())
    }

    /// Получатель существует и всё ещё в том же household.
    fn recipient_alive(&self) -> bool {
        self.recipient_id.is_some()
            && self.recipient_deleted_at.is_none()
            && self.recipient_household_id == Some(self.household_id)
    }

    /// Получатель напоминания всё ещё выбран по правилу
    /// исполнитель → автор → участники.
    fn recipient_valid(&self) -> bool {
        if !self.recipient_alive() {
            return false;
        }

        match self.assignee_user_id.or(self.created_by_user_id) {
            Some(selected) => selected == self.user_id,
            // Оба поля пусты — получателем может быть любой активный участник.
            None => true,
        }
    }

    /// Напоминания включены у получателя.
    fn reminders_enabled(&self) -> bool {
        NotificationSettings::from_settings(&self.recipient_settings).reminders_enabled
    }

    /// Household существует.
    fn household_valid(&self) -> bool {
        self.household_found.is_some() && self.household_deleted_at.is_none()
    }

    /// Вседневное напоминание живёт до конца локального дня срока.
    ///
    /// Перенесённое тихими часами через полночь напоминание не отменяется:
    /// оно уходит утром следующего дня, хотя локальная дата уже сменилась.
    fn in_window(&self, now: DateTime<Utc>) -> bool {
        match self.due_date {
            Some(due_date) => {
                let today = now.with_timezone(&self.timezone()).date_naive();
                due_date == today || (self.deferred && due_date + Duration::days(1) == today)
            }
            None => true,
        }
    }

    /// Запись больше не должна быть доставлена.
    ///
    /// Snooze не сверяется с текущим `occurrence_key`: его ключ — время
    /// отложенной доставки. Он отменяется только вместе с задачей, списком,
    /// household, получателем или выключенными напоминаниями: выключение
    /// настройки отменяет и отложенное пользователем напоминание.
    fn is_stale(&self, now: DateTime<Utc>) -> bool {
        if !self.item_alive() || !self.recipient_alive() || !self.household_valid() {
            return true;
        }
        if !self.reminders_enabled() {
            return true;
        }
        if self.is_snooze() {
            return false;
        }
        !self.item_valid()
            || !self.recipient_valid()
            || self.current_key().as_deref() != Some(self.occurrence_key.as_str())
            || !self.in_window(now)
    }
}

pub(crate) const REMINDER_ROW_COLUMNS: &str = "d.id, d.household_id, d.user_id, d.occurrence_key,
        d.kind::text AS kind, d.deferred,
        i.title, i.id AS item_id, i.status::text AS item_status,
        i.deleted_at AS item_deleted_at, i.due_at, i.due_date,
        i.assignee_user_id, i.created_by_user_id,
        l.id AS list_id, l.kind::text AS list_kind, l.deleted_at AS list_deleted_at,
        l.is_archived AS list_archived,
        u.id AS recipient_id, u.deleted_at AS recipient_deleted_at,
        u.household_id AS recipient_household_id, u.timezone AS recipient_timezone,
        u.settings AS recipient_settings, u.telegram_user_id, u.telegram_chat_id,
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
         WHERE d.status = 'pending' AND d.kind IN ('reminder', 'snooze')"
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
         WHERE d.id = $1 AND d.status = 'pending'
           AND d.kind IN ('reminder', 'snooze')
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
    let timezone = row.timezone();
    // Проверка под блокировкой закрывает гонку с другим экземпляром
    // планировщика: даже «сброшенная» запись не уйдёт внутри тихих часов.
    if postpone_for_quiet(&mut tx, row.id, &row.recipient_settings, timezone, now).await? {
        tx.commit().await?;
        return Ok(0);
    }

    // `is_stale` уже проверил эти поля; явная проверка не даёт отправить
    // сообщение в чат 0 или с пустым названием при будущих изменениях схемы.
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
    let list = match lists::get(&mut tx, row.household_id, item.list_id).await {
        Ok(list) => list,
        Err(DomainError::Database(error)) => return Err(error),
        Err(_) => {
            mark_cancelled(&mut tx, row.id, now).await?;
            tx.commit().await?;
            return Ok(0);
        }
    };

    // Snooze и вседневное напоминание показывают «Пора». Для timed-задачи
    // заголовок считает фактическое время до срока: тихие часы или поздний тик
    // уже могли сдвинуть доставку ближе к сроку.
    let lead_minutes = if row.is_snooze() || row.due_date.is_some() {
        None
    } else {
        row.due_at.and_then(|due| {
            let seconds = (due - now).num_seconds();
            (seconds > 0).then_some((seconds + 59) / 60)
        })
    };
    let ui = notify::reminder_ui(&item, &list, timezone, now, lead_minutes);
    let html = notify::render_notice(&mut tx, telegram_user_id, row.household_id, &ui, now).await?;
    let dedupe_key = delivery_dedupe_key(row.id, &row.kind);

    let seq = enqueue_message(&mut tx, chat_id, &dedupe_key, html, true, now).await?;
    mark_sent(&mut tx, row.id, seq, now).await?;

    tx.commit().await?;
    Ok(1)
}

fn delivery_dedupe_key(id: Uuid, kind: &str) -> String {
    format!("notification:{kind}:{id}")
}
