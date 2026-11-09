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

use std::future::Future;
use std::sync::Arc;
use std::time::Duration as StdDuration;

use chrono::{DateTime, Duration, NaiveDate, NaiveTime, Utc};
use sqlx::{FromRow, PgConnection, PgPool};
use tokio::sync::{Notify, watch};
use uuid::Uuid;

use super::delivery::{self, DeliveryScope};
use super::notice::NoticeRow;
use super::quiet::resolved_timezone;
use super::{assignment, batch, digest};
use crate::domain::recurrence::resolve_local;
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
        // не учитывался.
        let mut rescheduled = 0;

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
