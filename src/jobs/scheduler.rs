//! Планировщик напоминаний: тик и планирование.
//!
//! Общий каркас доставки живёт в [`crate::jobs::delivery`], правило
//! актуальности — в [`crate::jobs::notice`], тихие часы и пояс получателя — в
//! [`crate::jobs::quiet`]. Сводка живёт в [`crate::jobs::digest`], сводное
//! сообщение — в [`crate::jobs::batch`], уведомление о назначении — в
//! [`crate::jobs::assignment`]. Здесь остаются цикл, тик и планирование
//! напоминаний.
//!
//! Один тик делает три шага:
//!
//! 1. отменяет `pending`-доставки, вхождение которых больше не актуально
//!    (задача завершена, удалена, срок сдвинулся, получатель сменился,
//!    напоминания или сводка выключены, элемент назначен другому);
//! 2. планирует напоминания для открытых задач и событий со временем, у
//!    которых наступил момент `due_at - lead_minutes` получателя, для
//!    вседневных — у которых наступило локальное `all_day_time` в день
//!    начала; сводки и уведомления о назначении уже стоят в очереди;
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

use std::future::Future;
use std::sync::Arc;
use std::time::Duration as StdDuration;

use chrono::{DateTime, Duration, NaiveDate, NaiveTime, Utc};
use sqlx::{Acquire, FromRow, PgConnection, PgPool};
use tokio::sync::{Notify, watch};
use uuid::Uuid;

use super::delivery::{self, DeliveryScope};
use super::notice::NoticeRow;
use super::quiet::resolved_timezone;
use super::{assignment, batch, digest};
use crate::domain::error::DomainError;
use crate::domain::events::{effective_end, start_of_day};
use crate::domain::models::{MonthRule, Recurrence, RecurrenceFreq};
use crate::domain::recurrence::{Due, parse_timezone, resolve_local};
use crate::domain::settings::{MAX_LEAD_MINUTES, NotificationSettings, parse_hhmm};
use crate::storage::repositories::notifications::{
    self, NewDelivery, NotificationKind, snooze_key_time,
};
use crate::telegram::bot::notify;

pub use super::keys::{all_day_occurrence_key, occurrence_key, snooze_origin};

/// Интервал планировщика.
pub const TICK_INTERVAL: StdDuration = StdDuration::from_secs(30);

/// Время вседневного напоминания по умолчанию.
const DEFAULT_ALL_DAY_TIME: &str = "09:00";

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
    ///
    /// Каждый шаг выполняется отдельно: ошибка одного пишется с названием
    /// шага и не отменяет остальные. `Err` возвращается, если упал хотя бы
    /// один шаг, — ошибкой последнего из них.
    pub async fn tick(&self, now: DateTime<Utc>) -> Result<TickStats, sqlx::Error> {
        let mut stats = TickStats::default();
        let mut last_error = None;
        // Пересчёт времени не меняет `TickStats`: раньше его результат тоже
        // не учитывался. Автопереход событий тоже не влияет на счётчики
        // доставок.
        let mut rescheduled = 0;
        let mut advanced = 0;

        step(
            "cancel_stale reminders",
            delivery::cancel_stale(&self.pool, now, DeliveryScope::Reminder),
            &mut stats.cancelled,
            &mut last_error,
        )
        .await;
        step(
            "cancel_stale digests",
            delivery::cancel_stale(&self.pool, now, DeliveryScope::Digest),
            &mut stats.cancelled,
            &mut last_error,
        )
        .await;
        step(
            "cancel_stale assignments",
            delivery::cancel_stale(&self.pool, now, DeliveryScope::Assignment),
            &mut stats.cancelled,
            &mut last_error,
        )
        .await;
        step(
            "advance events",
            advance_recurring_events(&self.pool, now),
            &mut advanced,
            &mut last_error,
        )
        .await;
        step(
            "plan reminders",
            plan_reminders(&self.pool, now),
            &mut stats.planned,
            &mut last_error,
        )
        .await;
        step(
            "plan digest",
            digest::plan(&self.pool, now),
            &mut stats.planned,
            &mut last_error,
        )
        .await;

        // Настройки читаются на каждом тике: новое время сводки, lead time,
        // пояс или границы тихих часов применяются без перезапуска. Время
        // считается идемпотентно, поэтому повторный тик не сбрасывает уже
        // отложенную запись обратно в прошлое.
        step(
            "reschedule reminders",
            delivery::reschedule_pending(
                &self.pool,
                now,
                DeliveryScope::Reminder,
                reminder_intended,
            ),
            &mut rescheduled,
            &mut last_error,
        )
        .await;
        step(
            "reschedule digest",
            digest::reschedule(&self.pool, now),
            &mut rescheduled,
            &mut last_error,
        )
        .await;
        step(
            "reschedule assignments",
            assignment::reschedule(&self.pool, now),
            &mut rescheduled,
            &mut last_error,
        )
        .await;

        // Сначала собираем отложенные тихими часами уведомления в одно
        // сообщение; одиночные и неотложенные записи затем уходят обычным
        // путём. Сводка в батч не входит и доставляется отдельно.
        step(
            "deliver batches",
            batch::deliver_batches(&self.pool, now),
            &mut stats.delivered,
            &mut last_error,
        )
        .await;
        step(
            "deliver reminders",
            delivery::deliver_due(&self.pool, now, DeliveryScope::Reminder),
            &mut stats.delivered,
            &mut last_error,
        )
        .await;
        step(
            "deliver digests",
            delivery::deliver_due(&self.pool, now, DeliveryScope::Digest),
            &mut stats.delivered,
            &mut last_error,
        )
        .await;
        step(
            "deliver assignments",
            delivery::deliver_due(&self.pool, now, DeliveryScope::Assignment),
            &mut stats.delivered,
            &mut last_error,
        )
        .await;

        if stats.delivered > 0 {
            self.wake_outbox.notify_one();
        }

        match last_error {
            Some(error) => Err(error),
            None => Ok(stats),
        }
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

/// Выполнить один шаг тика.
///
/// Ошибка пишется в лог с названием шага и запоминается как последняя, но не
/// прерывает остальные шаги.
async fn step(
    name: &'static str,
    future: impl Future<Output = Result<u64, sqlx::Error>>,
    total: &mut u64,
    last_error: &mut Option<sqlx::Error>,
) {
    match future.await {
        Ok(rows) => *total += rows,
        Err(error) => {
            tracing::error!(step = name, %error, "notification scheduler step failed");
            *last_error = Some(error);
        }
    }
}

#[derive(FromRow)]
struct EventAdvanceCandidate {
    id: Uuid,
    household_id: Uuid,
    due_at: Option<DateTime<Utc>>,
    due_date: Option<NaiveDate>,
    end_at: Option<DateTime<Utc>>,
    end_date: Option<NaiveDate>,
    recurrence_freq: Option<RecurrenceFreq>,
    recurrence_interval: Option<i16>,
    recurrence_byweekday: Option<Vec<i16>>,
    recurrence_until: Option<NaiveDate>,
    recurrence_timezone: Option<String>,
    recurrence_anchor_date: Option<NaiveDate>,
    recurrence_month_rule: Option<MonthRule>,
    recurrence_occurrence_date: Option<NaiveDate>,
    recurrence_occurrence_time: Option<NaiveTime>,
    household_timezone: String,
}

impl EventAdvanceCandidate {
    fn recurrence(&self) -> Option<Recurrence> {
        let freq = self.recurrence_freq?;
        let anchor_date = self.recurrence_anchor_date?;
        Some(Recurrence {
            freq,
            interval: self.recurrence_interval.unwrap_or(1),
            byweekday: self.recurrence_byweekday.clone().unwrap_or_default(),
            until: self.recurrence_until,
            timezone: self.recurrence_timezone.clone().unwrap_or_default(),
            anchor_date,
            month_rule: self.recurrence_month_rule,
            occurrence_date: self.recurrence_occurrence_date,
            occurrence_time: self.recurrence_occurrence_time,
        })
    }

    fn due(&self) -> Option<Due> {
        match (self.due_at, self.due_date) {
            (Some(at), None) => Some(Due::Timed(at)),
            (None, Some(date)) => Some(Due::AllDay(date)),
            _ => None,
        }
    }
}

/// Новое вхождение события: начало, конец и сброшенный слот переноса.
struct EventAdvance {
    due_at: Option<DateTime<Utc>>,
    due_date: Option<NaiveDate>,
    end_at: Option<DateTime<Utc>>,
    end_date: Option<NaiveDate>,
}

/// Перевести прошедшие повторяющиеся события на первое непрошедшее вхождение.
///
/// Строки выбираются `FOR UPDATE SKIP LOCKED`, чтобы не столкнуться с правкой
/// пользователя. Пропущенные за простой вхождения перешагиваются сразу: для
/// каждого события ищется первое вхождение, эффективный конец которого позже
/// `now`. Журнал выполнений и `item_events` не пополняются.
async fn advance_recurring_events(pool: &PgPool, now: DateTime<Utc>) -> Result<u64, sqlx::Error> {
    let mut tx = pool.begin().await?;
    // Выражение эффективного конца повторяет `domain::events::effective_end`:
    // оба места нужно менять вместе. Точное решение всё равно принимает домен
    // в `plan_event_advance`.
    let candidates: Vec<EventAdvanceCandidate> = sqlx::query_as(
        "SELECT i.id, i.household_id, i.due_at, i.due_date, i.end_at, i.end_date,
                i.recurrence_freq, i.recurrence_interval, i.recurrence_byweekday,
                i.recurrence_until, i.recurrence_timezone, i.recurrence_anchor_date,
                i.recurrence_month_rule, i.recurrence_occurrence_date,
                i.recurrence_occurrence_time, h.timezone AS household_timezone
         FROM items i
         JOIN lists l
           ON l.id = i.list_id AND l.deleted_at IS NULL
          AND NOT l.is_archived AND l.kind = 'events'
         JOIN households h ON h.id = i.household_id AND h.deleted_at IS NULL
         WHERE i.deleted_at IS NULL AND i.recurrence_freq IS NOT NULL
           AND (
                 i.recurrence_until IS NULL
                 OR CASE
                      WHEN i.due_at IS NOT NULL
                        THEN (i.due_at AT TIME ZONE
                              COALESCE(NULLIF(i.recurrence_timezone, ''), h.timezone))::date
                      ELSE i.due_date
                    END < i.recurrence_until
               )
           AND COALESCE(
                 i.end_at,
                 CASE WHEN i.end_date IS NOT NULL
                      THEN ((i.end_date + 1)::timestamp AT TIME ZONE h.timezone)
                 END,
                 CASE WHEN i.due_at IS NOT NULL
                      THEN i.due_at + interval '1 hour'
                 END,
                 CASE WHEN i.due_date IS NOT NULL
                      THEN ((i.due_date + 1)::timestamp AT TIME ZONE h.timezone)
                 END
               ) <= $1
         ORDER BY i.id
         FOR UPDATE OF i SKIP LOCKED",
    )
    .bind(now)
    .fetch_all(&mut *tx)
    .await?;

    let mut advanced = 0;
    let mut first_error = None;
    for candidate in &candidates {
        let advance = match plan_event_advance(candidate, now) {
            Ok(Some(advance)) => advance,
            Ok(None) => continue,
            Err(error) => {
                // Битое правило — ошибка одной строки, а не шага: как в
                // `plan_reminders`, пишем её и переходим к следующему событию.
                tracing::error!(
                    item_id = %candidate.id,
                    %error,
                    "failed to plan recurring event advance"
                );
                continue;
            }
        };
        match apply_event_advance(&mut tx, candidate, &advance, now).await {
            Ok(rows) => advanced += rows,
            Err(error) => {
                // Ошибка одной строки не прерывает остальные; она всплывает в
                // конце, чтобы шаг попал в лог с своим именем.
                tracing::error!(
                    item_id = %candidate.id,
                    %error,
                    "failed to advance recurring event"
                );
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
    }

    tx.commit().await?;
    match first_error {
        Some(error) => Err(error),
        None => Ok(advanced),
    }
}

/// Найти первое вхождение события, эффективный конец которого позже `now`.
///
/// `next_after` получает момент начала предыдущего вхождения, а не `now`:
/// вхождение, которое уже началось, но ещё не закончилось, тоже подходит.
/// Поэтому цикл проверяет именно эффективный конец, перешагивая только
/// действительно прошедшие вхождения.
fn plan_event_advance(
    candidate: &EventAdvanceCandidate,
    now: DateTime<Utc>,
) -> Result<Option<EventAdvance>, DomainError> {
    let Some(rule) = candidate.recurrence() else {
        return Ok(None);
    };
    let Some(current) = candidate.due() else {
        return Ok(None);
    };
    let timezone = parse_timezone(&candidate.household_timezone)?;
    let rule_timezone = rule.timezone()?;

    // SQL-фильтр может выбрать строку шире или уже из-за различий в расчёте
    // полуночи; окончательное решение о том, что событие прошло, принимает
    // домен. Идущее сейчас вхождение не двигаем.
    let current_end = effective_end(
        candidate.due_at,
        candidate.due_date,
        candidate.end_at,
        candidate.end_date,
        timezone,
    );
    if current_end.is_some_and(|end| end > now) {
        return Ok(None);
    }

    let mut current_due = current;
    let mut current_moment = match current_due {
        Due::Timed(at) => at,
        Due::AllDay(date) => start_of_day(rule_timezone, date),
    };
    let mut last_advance: Option<EventAdvance> = None;
    loop {
        let Some(next_due) = rule.next_after(current_due, current_moment)? else {
            // Серия закончилась. Если после простоя нашлись прошедшие
            // вхождения, событие должно остаться на последнем из них, а не на
            // том, с которого начался пересчёт.
            return Ok(match last_advance {
                Some(advance)
                    if advance.due_at != candidate.due_at
                        || advance.due_date != candidate.due_date =>
                {
                    Some(advance)
                }
                _ => None,
            });
        };
        let advance = shift_event(candidate, next_due)?;
        let end = effective_end(
            advance.due_at,
            advance.due_date,
            advance.end_at,
            advance.end_date,
            timezone,
        );
        if end.is_some_and(|end| end > now) {
            return Ok(Some(advance));
        }
        last_advance = Some(advance);
        current_moment = match next_due {
            Due::Timed(at) => at,
            Due::AllDay(date) => start_of_day(rule_timezone, date),
        };
        current_due = next_due;
    }
}

/// Перенести начало на следующее вхождение и сдвинуть конец на ту же
/// длительность.
fn shift_event(candidate: &EventAdvanceCandidate, next: Due) -> Result<EventAdvance, DomainError> {
    match next {
        Due::Timed(at) => {
            let end_at = match (candidate.end_at, candidate.due_at) {
                (Some(end), Some(start)) => {
                    Some(end.checked_add_signed(at - start).ok_or_else(|| {
                        DomainError::Validation("event end is out of range".into())
                    })?)
                }
                _ => None,
            };
            Ok(EventAdvance {
                due_at: Some(at),
                due_date: None,
                end_at,
                end_date: None,
            })
        }
        Due::AllDay(date) => {
            let end_date = match (candidate.end_date, candidate.due_date) {
                (Some(end), Some(start)) => {
                    Some(end.checked_add_signed(date - start).ok_or_else(|| {
                        DomainError::Validation("event end is out of range".into())
                    })?)
                }
                _ => None,
            };
            Ok(EventAdvance {
                due_at: None,
                due_date: Some(date),
                end_at: None,
                end_date,
            })
        }
    }
}

/// Записать новое вхождение, не создавая записей в журналах.
async fn apply_event_advance(
    conn: &mut PgConnection,
    candidate: &EventAdvanceCandidate,
    advance: &EventAdvance,
    now: DateTime<Utc>,
) -> Result<u64, sqlx::Error> {
    // Вложенный savepoint: сбой одной строки откатывается отдельно и не
    // отменяет уже переведённые события.
    let mut savepoint = conn.begin().await?;
    let result = sqlx::query(
        "UPDATE items
         SET due_at = $3, due_date = $4, end_at = $5, end_date = $6,
             recurrence_occurrence_date = NULL, recurrence_occurrence_time = NULL,
             version = version + 1, updated_at = $7
         WHERE id = $1 AND household_id = $2 AND deleted_at IS NULL",
    )
    .bind(candidate.id)
    .bind(candidate.household_id)
    .bind(advance.due_at)
    .bind(advance.due_date)
    .bind(advance.end_at)
    .bind(advance.end_date)
    .bind(now)
    .execute(&mut *savepoint)
    .await;

    match result {
        Ok(done) => {
            savepoint.commit().await?;
            Ok(done.rows_affected())
        }
        Err(error) => {
            savepoint.rollback().await?;
            Err(error)
        }
    }
}

/// Момент, в который напоминание должно уйти по текущим настройкам.
///
/// `None` — вхождение больше не планируется: устарело или ключ повреждён.
fn reminder_intended(row: &NoticeRow) -> Option<DateTime<Utc>> {
    if row.is_snooze() {
        // Время snooze зафиксировано в ключе: повторный расчёт не должен
        // сдвигать его из-за позднего тика.
        return snooze_key_time(&row.occurrence_key);
    }
    if !row.item_valid() || row.current_key().as_deref() != Some(row.occurrence_key.as_str()) {
        return None;
    }
    let settings = row.settings();
    match (row.item_due_at, row.item_due_date) {
        (Some(due_at), None) => Some(due_at - Duration::minutes(i64::from(settings.lead_minutes))),
        (None, Some(due_date)) => {
            let time = parse_hhmm(&settings.all_day_time, "all-day reminder time")
                .unwrap_or_else(|_| default_all_day_time());
            resolve_local(row.timezone(), due_date, time).ok()
        }
        _ => None,
    }
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
/// Для timed-задач и событий окно задаёт `due_at - lead_minutes`; для
/// вседневных — локальные дата начала и `all_day_time` получателя. Вседневное
/// напоминание создаётся в течение дня начала, даже если момент `all_day_time`
/// уже прошёл. У многодневного события напоминание планируется только на день
/// начала.
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
              AND NOT l.is_archived AND l.kind IN ('tasks', 'events')
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
        origin_key: None,
        scheduled_at,
    };
    notifications::insert(conn, &delivery, now).await
}

/// Собрать сообщение напоминания; `None` — запись отменена.
pub(crate) async fn reminder_message(
    conn: &mut PgConnection,
    row: &NoticeRow,
    telegram_user_id: i64,
    now: DateTime<Utc>,
) -> Result<Option<String>, sqlx::Error> {
    let Some((item, list)) = delivery::load_item_and_list(conn, row, now).await? else {
        return Ok(None);
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
    let ui = notify::reminder_ui(&item, &list, row.timezone(), now, lead_minutes);
    notify::render_notice(conn, telegram_user_id, row.household_id, &ui, now)
        .await
        .map(Some)
}
