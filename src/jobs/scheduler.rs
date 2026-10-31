//! Планировщик уведомлений: тик, напоминания, утренние сводки и дедупликация
//! доставок.
//!
//! Один тик делает три шага:
//!
//! 1. отменяет `pending`-доставки, вхождение которых больше не актуально
//!    (задача завершена, удалена, срок сдвинулся, получатель сменился или
//!    сводка выключена);
//! 2. планирует напоминания для открытых задач со временем, у которых наступил
//!    момент `due_at - lead_minutes` получателя, для вседневных задач,
//!    у которых наступило локальное `all_day_time` в день срока, и утренние
//!    сводки после локального `digest_time` получателя;
//! 3. ставит готовые `pending`-доставки в `outbox` одной транзакцией со сменой
//!    статуса и будит outbox-воркер.
//!
//! Время передаётся в [`Scheduler::tick`] снаружи, поэтому тесты двигают его без
//! ожидания. Ключ доставки включает пользователя, элемент, вхождение и тип:
//! повторный тик и перезапуск сервиса не создают второе сообщение. Следующее
//! вхождение повторяющейся задачи получает новый срок, а значит и новый
//! `occurrence_key`.
//!
//! Получатель — исполнитель, иначе автор-пользователь, иначе каждый активный
//! участник household с включёнными напоминаниями. Ошибка БД на одной записи
//! логируется и не прерывает обработку остальных.

use std::sync::Arc;
use std::time::Duration as StdDuration;

use chrono::{DateTime, Duration, NaiveDate, NaiveTime, SecondsFormat, Utc};
use chrono_tz::Tz;
use sqlx::{FromRow, PgConnection, PgPool};
use tokio::sync::{Notify, watch};
use uuid::Uuid;

use crate::domain::recurrence::{parse_timezone, resolve_local};
use crate::domain::settings::{MAX_LEAD_MINUTES, NotificationSettings, parse_hhmm};
use crate::telegram::bot::notify::{self, DigestLine};
use crate::telegram::outbox::{self, Outgoing};
use crate::telegram::{rich, ru};

/// Интервал планировщика.
pub const TICK_INTERVAL: StdDuration = StdDuration::from_secs(30);

/// Сколько кандидатов на доставку рассматривается за один тик.
const DELIVERY_BATCH: i64 = 100;

/// Время вседневного напоминания по умолчанию.
const DEFAULT_ALL_DAY_TIME: &str = "09:00";

/// Время утренней сводки по умолчанию.
const DEFAULT_DIGEST_TIME: &str = "08:30";

/// Ключ вхождения: срок в UTC с микросекундной точностью.
pub fn occurrence_key(due_at: DateTime<Utc>) -> String {
    due_at.to_rfc3339_opts(SecondsFormat::Micros, true)
}

/// Ключ вхождения вседневной задачи: локальная дата срока.
pub fn all_day_occurrence_key(due_date: NaiveDate) -> String {
    format!("date:{due_date}")
}

/// Ключ вхождения утренней сводки: локальная дата получателя.
pub fn digest_occurrence_key(date: NaiveDate) -> String {
    format!("digest:{date}")
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
        let cancelled =
            cancel_stale(&self.pool, now).await? + cancel_stale_digests(&self.pool, now).await?;
        let planned =
            plan_reminders(&self.pool, now).await? + plan_digests(&self.pool, now).await?;
        let delivered =
            deliver_due(&self.pool, now).await? + deliver_digests_due(&self.pool, now).await?;

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
               ON l.id = i.list_id AND l.deleted_at IS NULL AND l.kind = 'tasks'
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

    let mut planned = 0;
    for candidate in candidates {
        let settings = NotificationSettings::from_settings(&candidate.settings);
        if !settings.reminders_enabled {
            continue;
        }
        let timezone = candidate
            .user_timezone
            .as_deref()
            .and_then(|value| parse_timezone(value).ok())
            .or_else(|| parse_timezone(&candidate.household_timezone).ok())
            .unwrap_or(chrono_tz::UTC);

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

        match insert_delivery(pool, &candidate, &key, scheduled_at, now).await {
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

fn default_digest_time() -> NaiveTime {
    parse_hhmm(DEFAULT_DIGEST_TIME, "digest time").expect("valid default time")
}

#[derive(FromRow)]
struct DigestCandidate {
    user_id: Uuid,
    household_id: Uuid,
    settings: serde_json::Value,
    user_timezone: Option<String>,
    household_timezone: String,
}

/// Запланировать утренние сводки на текущий локальный день.
///
/// Сводка создаётся только после `digest_time` и живёт до конца локального дня:
/// если сервис был выключен в момент отправки, она уходит при первом тике в тот
/// же день. За прошлые дни запись не создаётся.
async fn plan_digests(pool: &PgPool, now: DateTime<Utc>) -> Result<u64, sqlx::Error> {
    let candidates: Vec<DigestCandidate> = sqlx::query_as(
        "SELECT u.id AS user_id, u.household_id, u.settings,
                u.timezone AS user_timezone, h.timezone AS household_timezone
         FROM users u
         JOIN households h ON h.id = u.household_id AND h.deleted_at IS NULL
         WHERE u.deleted_at IS NULL",
    )
    .fetch_all(pool)
    .await?;

    let mut planned = 0;
    for candidate in candidates {
        let settings = NotificationSettings::from_settings(&candidate.settings);
        if !settings.digest_enabled {
            continue;
        }
        let timezone = candidate
            .user_timezone
            .as_deref()
            .and_then(|value| parse_timezone(value).ok())
            .or_else(|| parse_timezone(&candidate.household_timezone).ok())
            .unwrap_or(chrono_tz::UTC);
        let local_date = now.with_timezone(&timezone).date_naive();
        let time = parse_hhmm(&settings.digest_time, "digest time")
            .unwrap_or_else(|_| default_digest_time());
        let scheduled_at = match resolve_local(timezone, local_date, time) {
            Ok(scheduled_at) => scheduled_at,
            Err(error) => {
                tracing::error!(
                    user_id = %candidate.user_id,
                    %error,
                    "failed to resolve digest time"
                );
                continue;
            }
        };
        if scheduled_at > now {
            continue;
        }

        match insert_digest_delivery(
            pool,
            candidate.household_id,
            candidate.user_id,
            &digest_occurrence_key(local_date),
            scheduled_at,
            now,
        )
        .await
        {
            Ok(rows) => planned += rows,
            Err(error) => tracing::error!(
                user_id = %candidate.user_id,
                %error,
                "failed to plan digest delivery"
            ),
        }
    }

    Ok(planned)
}

/// Вставить доставку, если такого актуального вхождения ещё нет.
///
/// `pending` и `sent` отфильтровываются в SQL, поэтому повторный тик не делает
/// лишних вставок. `cancelled` возвращается в `pending`, когда вхождение снова
/// стало актуальным (например, задачу завершили и сразу вернули).
async fn insert_delivery(
    pool: &PgPool,
    candidate: &ReminderCandidate,
    key: &str,
    scheduled_at: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Result<u64, sqlx::Error> {
    insert_delivery_row(
        pool,
        candidate.household_id,
        candidate.user_id,
        Some(candidate.item_id),
        "reminder",
        key,
        scheduled_at,
        now,
    )
    .await
}

async fn insert_digest_delivery(
    pool: &PgPool,
    household_id: Uuid,
    user_id: Uuid,
    key: &str,
    scheduled_at: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Result<u64, sqlx::Error> {
    insert_delivery_row(
        pool,
        household_id,
        user_id,
        None,
        "digest",
        key,
        scheduled_at,
        now,
    )
    .await
}

/// Вставить доставку, если такого актуального вхождения ещё нет.
///
/// `pending` и `sent` отфильтровываются в SQL, поэтому повторный тик не делает
/// лишних вставок. `cancelled` возвращается в `pending`, когда вхождение снова
/// стало актуальным (например, задачу завершили и сразу вернули).
#[allow(clippy::too_many_arguments)]
async fn insert_delivery_row(
    pool: &PgPool,
    household_id: Uuid,
    user_id: Uuid,
    item_id: Option<Uuid>,
    kind: &str,
    key: &str,
    scheduled_at: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Result<u64, sqlx::Error> {
    Ok(sqlx::query(
        "INSERT INTO notification_deliveries (
             id, household_id, user_id, item_id, occurrence_key, kind,
             status, scheduled_at, created_at, updated_at
         )
         SELECT $1, $2, $3, $4, $5, $6::notification_kind, 'pending', $7, $8, $8
         WHERE NOT EXISTS (
             SELECT 1 FROM notification_deliveries d
             WHERE d.user_id = $3 AND d.item_id IS NOT DISTINCT FROM $4
               AND d.occurrence_key = $5 AND d.kind = $6::notification_kind
               AND d.status IN ('pending', 'sent')
         )
         ON CONFLICT ON CONSTRAINT notification_deliveries_key_unique
         DO UPDATE SET
             status = 'pending',
             scheduled_at = EXCLUDED.scheduled_at,
             outbox_seq = NULL,
             updated_at = EXCLUDED.updated_at
         WHERE notification_deliveries.status = 'cancelled'",
    )
    .bind(Uuid::now_v7())
    .bind(household_id)
    .bind(user_id)
    .bind(item_id)
    .bind(key)
    .bind(kind)
    .bind(scheduled_at)
    .bind(now)
    .execute(pool)
    .await?
    .rows_affected())
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
        self.recipient_timezone
            .as_deref()
            .and_then(|value| parse_timezone(value).ok())
            .or_else(|| {
                self.household_timezone
                    .as_deref()
                    .and_then(|value| parse_timezone(value).ok())
            })
            .unwrap_or(chrono_tz::UTC)
    }

    /// Ключ текущего вхождения элемента.
    fn current_key(&self) -> Option<String> {
        match (self.due_at, self.due_date) {
            (Some(due_at), None) => Some(occurrence_key(due_at)),
            (None, Some(due_date)) => Some(all_day_occurrence_key(due_date)),
            _ => None,
        }
    }

    /// Элемент существует, открыт и лежит в списке задач.
    fn item_valid(&self) -> bool {
        self.item_id.is_some()
            && self.item_deleted_at.is_none()
            && self.item_status.as_deref() == Some("open")
            && self.title.is_some()
            && self.list_id.is_some()
            && self.list_kind.as_deref() == Some("tasks")
            && self.list_deleted_at.is_none()
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

const REMINDER_ROW_COLUMNS: &str = "d.id, d.household_id, d.user_id, d.occurrence_key,
        i.title, i.id AS item_id, i.status::text AS item_status,
        i.deleted_at AS item_deleted_at, i.due_at, i.due_date,
        i.assignee_user_id, i.created_by_user_id,
        l.id AS list_id, l.kind::text AS list_kind, l.deleted_at AS list_deleted_at,
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

#[derive(FromRow)]
struct DigestDeliveryRow {
    id: Uuid,
    household_id: Uuid,
    occurrence_key: String,
    telegram_user_id: Option<i64>,
    telegram_chat_id: Option<i64>,
    settings: Option<serde_json::Value>,
    user_deleted_at: Option<DateTime<Utc>>,
    user_household_id: Option<Uuid>,
    user_timezone: Option<String>,
    household_deleted_at: Option<DateTime<Utc>>,
    household_timezone: Option<String>,
}

impl DigestDeliveryRow {
    /// Пояс получателя, иначе household, иначе UTC.
    fn timezone(&self) -> Tz {
        self.user_timezone
            .as_deref()
            .and_then(|value| parse_timezone(value).ok())
            .or_else(|| {
                self.household_timezone
                    .as_deref()
                    .and_then(|value| parse_timezone(value).ok())
            })
            .unwrap_or(chrono_tz::UTC)
    }

    /// Сводка всё ещё включена у получателя.
    fn digest_enabled(&self) -> bool {
        self.settings
            .as_ref()
            .map(|settings| NotificationSettings::from_settings(settings).digest_enabled)
            .unwrap_or(false)
    }

    /// Запись ещё актуальна: пользователь и household живы, сводка включена,
    /// а ключ соответствует текущему локальному дню получателя.
    fn is_stale(&self, now: DateTime<Utc>) -> bool {
        self.user_deleted_at.is_some()
            || self.user_household_id != Some(self.household_id)
            || self.household_deleted_at.is_some()
            || !self.digest_enabled()
            || self.occurrence_key
                != digest_occurrence_key(now.with_timezone(&self.timezone()).date_naive())
    }
}

const DIGEST_ROW_COLUMNS: &str = "d.id, d.household_id, d.occurrence_key,
        u.telegram_user_id, u.telegram_chat_id, u.settings,
        u.deleted_at AS user_deleted_at, u.household_id AS user_household_id,
        u.timezone AS user_timezone, h.deleted_at AS household_deleted_at,
        h.timezone AS household_timezone";

/// Отменить `pending`-сводки, которые больше не должны быть доставлены.
async fn cancel_stale_digests(pool: &PgPool, now: DateTime<Utc>) -> Result<u64, sqlx::Error> {
    let sql = format!(
        "SELECT {DIGEST_ROW_COLUMNS}
         FROM notification_deliveries d
         LEFT JOIN users u ON u.id = d.user_id
         LEFT JOIN households h ON h.id = d.household_id
         WHERE d.status = 'pending' AND d.kind = 'digest'"
    );
    let rows: Vec<DigestDeliveryRow> = sqlx::query_as(&sql).fetch_all(pool).await?;

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
                "failed to cancel stale digest delivery"
            ),
        }
    }

    Ok(cancelled)
}

async fn cancel_delivery(pool: &PgPool, id: Uuid, now: DateTime<Utc>) -> Result<u64, sqlx::Error> {
    Ok(sqlx::query(
        "UPDATE notification_deliveries
         SET status = 'cancelled', updated_at = $2
         WHERE id = $1 AND status = 'pending'",
    )
    .bind(id)
    .bind(now)
    .execute(pool)
    .await?
    .rows_affected())
}

async fn mark_cancelled(
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

#[derive(FromRow)]
struct DigestItem {
    id: Uuid,
    title: String,
    due_at: Option<DateTime<Utc>>,
    due_date: Option<NaiveDate>,
}

struct DigestContent {
    today: Vec<DigestLine>,
    overdue: Vec<DigestLine>,
    shopping_open: i64,
    tasks_list: Option<Uuid>,
    shopping_list: Option<Uuid>,
}

/// Ключ сортировки задач сводки: timed по времени, затем вседневные по дате.
fn digest_sort_key(item: &DigestItem) -> (u8, Option<DateTime<Utc>>, Option<NaiveDate>, Uuid) {
    (
        u8::from(item.due_at.is_none()),
        item.due_at,
        item.due_date,
        item.id,
    )
}

/// Собрать содержимое сводки в момент доставки.
///
/// Разделы совпадают с экраном списка: timed-задача со сроком в прошлом и
/// вседневная задача с прошедшей датой — «Просрочено», остальные задачи с
/// локальной датой получателя — «Сегодня».
async fn digest_content(
    conn: &mut PgConnection,
    household_id: Uuid,
    timezone: Tz,
    now: DateTime<Utc>,
) -> Result<DigestContent, sqlx::Error> {
    let mut rows: Vec<DigestItem> = sqlx::query_as(
        "SELECT i.id, i.title, i.due_at, i.due_date
         FROM items i
         JOIN lists l
           ON l.id = i.list_id AND l.deleted_at IS NULL
          AND NOT l.is_archived AND l.kind = 'tasks'
         WHERE i.household_id = $1 AND i.status = 'open' AND i.deleted_at IS NULL
           AND (i.due_at IS NOT NULL OR i.due_date IS NOT NULL)",
    )
    .bind(household_id)
    .fetch_all(&mut *conn)
    .await?;
    rows.sort_by_key(digest_sort_key);

    let today = now.with_timezone(&timezone).date_naive();
    let mut today_lines = Vec::new();
    let mut overdue_lines = Vec::new();
    for row in rows {
        match (row.due_at, row.due_date) {
            (Some(due_at), None) if due_at < now => {
                overdue_lines.push(DigestLine {
                    title: row.title,
                    when: ru::due_label(Some(due_at), None, timezone, now).unwrap_or_default(),
                });
            }
            (Some(due_at), None) if due_at.with_timezone(&timezone).date_naive() == today => {
                today_lines.push(DigestLine {
                    title: row.title,
                    when: due_at.with_timezone(&timezone).format("%H:%M").to_string(),
                });
            }
            (None, Some(due_date)) if due_date < today => {
                overdue_lines.push(DigestLine {
                    title: row.title,
                    when: ru::date_label(due_date, today),
                });
            }
            (None, Some(due_date)) if due_date == today => {
                today_lines.push(DigestLine {
                    title: row.title,
                    when: "весь день".to_owned(),
                });
            }
            _ => {}
        }
    }

    let shopping_open: i64 = sqlx::query_scalar(
        "SELECT count(*)
         FROM items i
         JOIN lists l
           ON l.id = i.list_id AND l.deleted_at IS NULL
          AND NOT l.is_archived AND l.kind = 'shopping'
         WHERE i.household_id = $1 AND i.status = 'open' AND i.deleted_at IS NULL",
    )
    .bind(household_id)
    .fetch_one(&mut *conn)
    .await?;

    let lists: Vec<(String, Uuid)> = sqlx::query_as(
        "SELECT kind::text, id
         FROM lists
         WHERE household_id = $1 AND deleted_at IS NULL AND NOT is_archived
           AND kind IN ('tasks', 'shopping')
         ORDER BY is_default DESC, position, created_at, id",
    )
    .bind(household_id)
    .fetch_all(&mut *conn)
    .await?;
    let find = |wanted: &str| {
        lists
            .iter()
            .find(|(kind, _)| kind == wanted)
            .map(|(_, id)| *id)
    };

    Ok(DigestContent {
        today: today_lines,
        overdue: overdue_lines,
        shopping_open,
        tasks_list: find("tasks"),
        shopping_list: find("shopping"),
    })
}

/// Передать готовые сводки в outbox.
async fn deliver_digests_due(pool: &PgPool, now: DateTime<Utc>) -> Result<u64, sqlx::Error> {
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM notification_deliveries
         WHERE status = 'pending' AND kind = 'digest' AND scheduled_at <= $1
         ORDER BY scheduled_at, id
         LIMIT $2",
    )
    .bind(now)
    .bind(DELIVERY_BATCH)
    .fetch_all(pool)
    .await?;

    let mut delivered = 0;
    for id in ids {
        match deliver_digest(pool, id, now).await {
            Ok(rows) => delivered += rows,
            Err(error) => tracing::error!(
                delivery_id = %id,
                %error,
                "failed to deliver digest notification"
            ),
        }
    }
    Ok(delivered)
}

/// Доставить одну сводку: содержимое собирается здесь, а не при планировании.
async fn deliver_digest(pool: &PgPool, id: Uuid, now: DateTime<Utc>) -> Result<u64, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let sql = format!(
        "SELECT {DIGEST_ROW_COLUMNS}
         FROM notification_deliveries d
         LEFT JOIN users u ON u.id = d.user_id
         LEFT JOIN households h ON h.id = d.household_id
         WHERE d.id = $1 AND d.status = 'pending' AND d.kind = 'digest'
           AND d.scheduled_at <= $2
         FOR UPDATE OF d SKIP LOCKED"
    );
    let row: Option<DigestDeliveryRow> = sqlx::query_as(&sql)
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
    let (Some(chat_id), Some(telegram_user_id)) = (row.telegram_chat_id, row.telegram_user_id)
    else {
        mark_cancelled(&mut tx, row.id, now).await?;
        tx.commit().await?;
        return Ok(0);
    };

    let content = digest_content(&mut tx, row.household_id, row.timezone(), now).await?;
    let ui = notify::digest_ui(
        &content.today,
        &content.overdue,
        content.shopping_open,
        content.tasks_list,
        content.shopping_list,
    );
    let html = notify::render_notice(&mut tx, telegram_user_id, row.household_id, &ui, now).await?;
    let dedupe_key = format!("notification:digest:{}", row.id);
    let seq = outbox::enqueue(
        &mut tx,
        chat_id,
        &Outgoing::Message { html, rich: true },
        Some(&dedupe_key),
        now,
    )
    .await?;
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

/// Доставить одну запись: outbox и `sent` в одной транзакции.
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

    let seq = outbox::enqueue(
        &mut tx,
        chat_id,
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
