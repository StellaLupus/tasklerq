//! Утренняя сводка: планирование, содержимое и доставка.

use chrono::{DateTime, NaiveDate, NaiveTime, Utc};
use chrono_tz::Tz;
use sqlx::{FromRow, PgConnection, PgPool};
use uuid::Uuid;

use super::scheduler::{
    DELIVERY_BATCH, cancel_delivery, enqueue_message, mark_cancelled, mark_sent, resolved_timezone,
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
        if scheduled_at > now {
            continue;
        }

        let occurrence_key = digest_occurrence_key(local_date);
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

    let content = content(&mut tx, row.household_id, row.timezone(), now).await?;
    let ui = notify::digest_ui(
        &content.today,
        &content.overdue,
        content.shopping_open,
        content.tasks_list,
        content.shopping_list,
    );
    let html = notify::render_notice(&mut tx, telegram_user_id, row.household_id, &ui, now).await?;
    let dedupe_key = format!("notification:digest:{}", row.id);
    let seq = enqueue_message(&mut tx, chat_id, &dedupe_key, html, true, now).await?;
    mark_sent(&mut tx, row.id, seq, now).await?;

    tx.commit().await?;
    Ok(1)
}
