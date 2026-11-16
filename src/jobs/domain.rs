//! Доменный тикер: переход повторяющихся событий на следующее вхождение.
//!
//! Работает всегда, в том числе без Telegram: сценарий «только Home Assistant»
//! тоже должен продвигать серии событий. Остальные шаги [`crate::jobs::scheduler`]
//! остаются в Telegram-рантайме, потому что без бота доставлять некому.

use chrono::{DateTime, NaiveDate, NaiveTime, Utc};
use sqlx::{Acquire, FromRow, PgConnection, PgPool};
use tokio::sync::watch;
use uuid::Uuid;

use super::scheduler::TICK_INTERVAL;
use crate::domain::error::DomainError;
use crate::domain::events::{effective_end, start_of_day};
use crate::domain::models::{MonthRule, Recurrence, RecurrenceFreq};
use crate::domain::recurrence::{Due, parse_timezone};

/// Название шага в журнале ошибок.
const ADVANCE_STEP: &str = "advance events";

/// Доменный тикер.
pub struct DomainTicker {
    pool: PgPool,
}

impl DomainTicker {
    /// Создать тикер.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Один шаг на момент `now`.
    ///
    /// Ошибка пишется в лог с названием шага, как в `Scheduler::step`.
    pub async fn tick(&self, now: DateTime<Utc>) -> Result<u64, sqlx::Error> {
        match advance_recurring_events(&self.pool, now).await {
            Ok(rows) => Ok(rows),
            Err(error) => {
                tracing::error!(
                    step = ADVANCE_STEP,
                    %error,
                    "domain ticker step failed"
                );
                Err(error)
            }
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
                        tracing::error!(%error, "domain ticker tick failed");
                    }
                }
            }
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
            // том, с которого начался пересчёт. `last_advance` всегда отличается
            // от исходного срока: это уже следующее вхождение.
            return Ok(last_advance);
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
