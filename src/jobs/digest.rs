//! Утренняя сводка: планирование, содержимое и доставка.

use chrono::{DateTime, NaiveDate, NaiveTime, Utc};
use chrono_tz::Tz;
use sqlx::{FromRow, PgConnection, PgPool};
use uuid::Uuid;

use super::scheduler::{
    DELIVERY_BATCH, cancel_delivery, desired_schedule, enqueue_message, mark_cancelled, mark_sent,
    postpone_for_quiet, reschedule_if_changed, resolved_timezone,
};
use crate::domain::recurrence::resolve_local;
use crate::domain::settings::{NotificationSettings, parse_hhmm};
use crate::storage::repositories::notifications::{self, NewDelivery, NotificationKind};
use crate::telegram::bot::notify::{self, DigestLine};
use crate::telegram::ru;

/// Время утренней сводки по умолчанию.
const DEFAULT_DIGEST_TIME: &str = "08:30";

/// Ключ вхождения утренней сводки: локальная дата получателя.
pub fn digest_occurrence_key(date: NaiveDate) -> String {
    format!("digest:{date}")
}

fn default_digest_time() -> NaiveTime {
    parse_hhmm(DEFAULT_DIGEST_TIME, "digest time").expect("valid default time")
}

/// Дата из ключа `digest:YYYY-MM-DD`.
fn digest_key_date(key: &str) -> Option<NaiveDate> {
    key.strip_prefix("digest:")
        .and_then(|date| NaiveDate::parse_from_str(date, "%Y-%m-%d").ok())
}

/// Отменить `pending deferred`-сводки, которые утром совпали бы с новой.
async fn cancel_deferred_on_same_day(
    conn: &mut PgConnection,
    user_id: Uuid,
    current_key: &str,
    timezone: Tz,
    delivery_at: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Result<u64, sqlx::Error> {
    let rows: Vec<(Uuid, DateTime<Utc>)> = sqlx::query_as(
        "SELECT id, scheduled_at
         FROM notification_deliveries
         WHERE user_id = $1 AND kind = 'digest' AND status = 'pending'
           AND deferred AND occurrence_key <> $2",
    )
    .bind(user_id)
    .bind(current_key)
    .fetch_all(&mut *conn)
    .await?;

    let delivery_date = delivery_at.with_timezone(&timezone).date_naive();
    let mut cancelled = 0;
    for (id, scheduled_at) in rows {
        if scheduled_at.with_timezone(&timezone).date_naive() != delivery_date {
            continue;
        }
        cancelled += sqlx::query(
            "UPDATE notification_deliveries
             SET status = 'cancelled', updated_at = $2
             WHERE id = $1 AND status = 'pending'",
        )
        .bind(id)
        .bind(now)
        .execute(&mut *conn)
        .await?
        .rows_affected();
    }
    Ok(cancelled)
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
pub async fn plan(pool: &PgPool, now: DateTime<Utc>) -> Result<u64, sqlx::Error> {
    let candidates: Vec<DigestCandidate> = sqlx::query_as(
        "SELECT u.id AS user_id, u.household_id, u.settings,
                u.timezone AS user_timezone, h.timezone AS household_timezone
         FROM users u
         JOIN households h ON h.id = u.household_id AND h.deleted_at IS NULL
         WHERE u.deleted_at IS NULL",
    )
    .fetch_all(pool)
    .await?;

    let mut conn = pool.acquire().await?;
    let mut planned = 0;
    for candidate in candidates {
        let settings = NotificationSettings::from_settings(&candidate.settings);
        if !settings.digest_enabled {
            continue;
        }
        let timezone = resolved_timezone(
            candidate.user_timezone.as_deref(),
            Some(&candidate.household_timezone),
        );
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
        let occurrence_key = digest_occurrence_key(local_date);

        // Отложенная через полночь сводка прошлого дня и сегодняшняя не должны
        // прийти одним утром. Если сегодняшняя (с учётом тихих часов) попадёт
        // на ту же локальную дату, прошлую отменяем — её заменит новая.
        // При выключенных тихих часах отложенную запись, наоборот, отпускаем
        // на ближайшем тике.
        if settings.quiet_hours_enabled {
            let delivery_at = desired_schedule(scheduled_at, now, timezone, &settings);
            cancel_deferred_on_same_day(
                &mut conn,
                candidate.user_id,
                &occurrence_key,
                timezone,
                delivery_at,
                now,
            )
            .await?;
        }

        if scheduled_at > now {
            continue;
        }

        let delivery = NewDelivery {
            household_id: candidate.household_id,
            user_id: candidate.user_id,
            item_id: None,
            kind: NotificationKind::Digest,
            occurrence_key: &occurrence_key,
            scheduled_at,
        };
        match notifications::insert(&mut conn, &delivery, now).await {
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

/// Пересчитать время ещё не доставленных сводок по текущим настройкам.
///
/// Базой служит время сводки из `occurrence_key` и текущего `digest_time`, а не
/// `created_at`: поздний тик не должен навсегда сдвигать сводку. Отложенная
/// тихими часами сводка не пересчитывается вовсе: она уходит в своё
/// зафиксированное время, а выключение тихих часов её не ускоряет.
pub async fn reschedule(pool: &PgPool, now: DateTime<Utc>) -> Result<u64, sqlx::Error> {
    #[derive(FromRow)]
    struct PendingDigest {
        id: Uuid,
        occurrence_key: String,
        created_at: DateTime<Utc>,
        deferred: bool,
        settings: serde_json::Value,
        user_timezone: Option<String>,
        household_timezone: String,
    }

    let rows: Vec<PendingDigest> = sqlx::query_as(
        "SELECT d.id, d.occurrence_key, d.created_at, d.deferred, u.settings,
                u.timezone AS user_timezone, h.timezone AS household_timezone
         FROM notification_deliveries d
         JOIN users u ON u.id = d.user_id
         JOIN households h ON h.id = d.household_id
         WHERE d.status = 'pending' AND d.kind = 'digest'",
    )
    .fetch_all(pool)
    .await?;

    let mut conn = pool.acquire().await?;
    let mut rescheduled = 0;
    for row in rows {
        if row.deferred {
            continue;
        }
        let settings = NotificationSettings::from_settings(&row.settings);
        let timezone =
            resolved_timezone(row.user_timezone.as_deref(), Some(&row.household_timezone));
        let time = parse_hhmm(&settings.digest_time, "digest time")
            .unwrap_or_else(|_| default_digest_time());
        let base = digest_key_date(&row.occurrence_key)
            .and_then(|date| resolve_local(timezone, date, time).ok())
            .unwrap_or(row.created_at);
        let desired = desired_schedule(base, now, timezone, &settings);
        let deferred = desired != base;
        rescheduled += reschedule_if_changed(&mut conn, row.id, desired, deferred, now).await?;
    }
    Ok(rescheduled)
}

#[derive(FromRow)]
struct DigestDeliveryRow {
    id: Uuid,
    household_id: Uuid,
    occurrence_key: String,
    deferred: bool,
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
        resolved_timezone(
            self.user_timezone.as_deref(),
            self.household_timezone.as_deref(),
        )
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
    ///
    /// Сводка, отложенная тихими часами через полночь (например, в 23:00 при
    /// тихих часах до 08:00), остаётся актуальной до утра: иначе она была бы
    /// отменена в 00:00 и потерялась, а следующая сводка пришла бы только через
    /// сутки. Утром уходит ровно одна такая сводка, и следующая планируется на
    /// новый день.
    fn is_stale(&self, now: DateTime<Utc>) -> bool {
        if self.user_deleted_at.is_some()
            || self.user_household_id != Some(self.household_id)
            || self.household_deleted_at.is_some()
            || !self.digest_enabled()
        {
            return true;
        }
        let today = now.with_timezone(&self.timezone()).date_naive();
        let current = digest_occurrence_key(today);
        self.occurrence_key != current
            && !(self.deferred
                && self.occurrence_key == digest_occurrence_key(today - chrono::Duration::days(1)))
    }
}

const DIGEST_ROW_COLUMNS: &str = "d.id, d.household_id, d.occurrence_key, d.deferred,
        u.telegram_user_id, u.telegram_chat_id, u.settings,
        u.deleted_at AS user_deleted_at, u.household_id AS user_household_id,
        u.timezone AS user_timezone, h.deleted_at AS household_deleted_at,
        h.timezone AS household_timezone";

/// Отменить `pending`-сводки, которые больше не должны быть доставлены.
pub async fn cancel_stale(pool: &PgPool, now: DateTime<Utc>) -> Result<u64, sqlx::Error> {
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
async fn content(
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
                    when: ru::ALL_DAY.to_owned(),
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
pub async fn deliver_due(pool: &PgPool, now: DateTime<Utc>) -> Result<u64, sqlx::Error> {
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
        match deliver_one(pool, id, now).await {
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
async fn deliver_one(pool: &PgPool, id: Uuid, now: DateTime<Utc>) -> Result<u64, sqlx::Error> {
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

    let delivered = match row {
        Some(row) => deliver_row(&mut tx, row, now).await?,
        None => 0,
    };
    tx.commit().await?;
    Ok(delivered)
}

/// Немедленно выпустить отложенные сводки получателя.
///
/// Вызывается из обработчика настроек в его транзакции после сводного
/// сообщения. Записи блокируются `FOR UPDATE SKIP LOCKED`, поэтому
/// планировщик не отправит ту же сводку второй раз.
pub(crate) async fn release_deferred(
    conn: &mut PgConnection,
    user_id: Uuid,
    now: DateTime<Utc>,
) -> Result<u64, sqlx::Error> {
    let sql = format!(
        "SELECT {DIGEST_ROW_COLUMNS}
         FROM notification_deliveries d
         LEFT JOIN users u ON u.id = d.user_id
         LEFT JOIN households h ON h.id = d.household_id
         WHERE d.user_id = $1 AND d.status = 'pending' AND d.kind = 'digest'
           AND d.deferred
         ORDER BY d.scheduled_at, d.id
         FOR UPDATE OF d SKIP LOCKED"
    );
    let rows: Vec<DigestDeliveryRow> = sqlx::query_as(&sql)
        .bind(user_id)
        .fetch_all(&mut *conn)
        .await?;

    let mut delivered = 0;
    for row in rows {
        delivered += deliver_row(conn, row, now).await?;
    }
    Ok(delivered)
}

/// Собрать и поставить в outbox одну сводку, если она ещё актуальна.
async fn deliver_row(
    conn: &mut PgConnection,
    row: DigestDeliveryRow,
    now: DateTime<Utc>,
) -> Result<u64, sqlx::Error> {
    if row.is_stale(now) {
        mark_cancelled(&mut *conn, row.id, now).await?;
        return Ok(0);
    }
    if let Some(settings) = row.settings.as_ref()
        && postpone_for_quiet(&mut *conn, row.id, settings, row.timezone(), now).await?
    {
        return Ok(0);
    }
    let (Some(chat_id), Some(telegram_user_id)) = (row.telegram_chat_id, row.telegram_user_id)
    else {
        mark_cancelled(&mut *conn, row.id, now).await?;
        return Ok(0);
    };

    let content = content(&mut *conn, row.household_id, row.timezone(), now).await?;
    let ui = notify::digest_ui(
        &content.today,
        &content.overdue,
        content.shopping_open,
        content.tasks_list,
        content.shopping_list,
    );
    let html =
        notify::render_notice(&mut *conn, telegram_user_id, row.household_id, &ui, now).await?;
    let dedupe_key = format!("notification:digest:{}", row.id);
    let seq = enqueue_message(&mut *conn, chat_id, &dedupe_key, html, true, now).await?;
    mark_sent(&mut *conn, row.id, seq, now).await?;

    Ok(1)
}
