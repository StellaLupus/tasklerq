//! Утренняя сводка: планирование, содержимое и текст сообщения.

use chrono::{DateTime, NaiveDate, NaiveTime, Utc};
use chrono_tz::Tz;
use sqlx::{FromRow, PgConnection, PgPool};
use uuid::Uuid;

use super::delivery::{self, DeliveryScope};
use super::keys::digest_key_date;
use super::notice::NoticeRow;
use super::quiet::{desired_schedule, resolved_timezone};
use crate::domain::events::effective_end;
use crate::domain::models::ListKind;
use crate::domain::recurrence::resolve_local;
use crate::domain::settings::{NotificationSettings, parse_hhmm};
use crate::storage::repositories::notifications::{self, NewDelivery, NotificationKind};
use crate::telegram::bot::notify::{self, DigestLine};
use crate::telegram::ru;

pub use super::keys::digest_occurrence_key;

/// Время утренней сводки по умолчанию.
const DEFAULT_DIGEST_TIME: &str = "08:30";

fn default_digest_time() -> NaiveTime {
    parse_hhmm(DEFAULT_DIGEST_TIME, "digest time").expect("valid default time")
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
        cancelled += delivery::mark_cancelled(conn, id, now).await?;
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
    let mut last_error = None;
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
            origin_key: None,
            scheduled_at,
        };
        match notifications::insert(&mut conn, &delivery, now).await {
            Ok(rows) => planned += rows,
            Err(error) => {
                tracing::error!(
                    user_id = %candidate.user_id,
                    %error,
                    "failed to plan digest delivery"
                );
                // Остальные получатели всё равно планируются: ошибка одной
                // записи не должна отменять шаг целиком, но шаг обязан
                // сообщить о сбое вызывающему.
                last_error = Some(error);
            }
        }
    }

    match last_error {
        Some(error) => Err(error),
        None => Ok(planned),
    }
}

/// Пересчитать время ещё не доставленных сводок по текущим настройкам.
///
/// Базой служит время сводки из `occurrence_key` и текущего `digest_time`, а не
/// `created_at`: поздний тик не должен навсегда сдвигать сводку.
pub async fn reschedule(pool: &PgPool, now: DateTime<Utc>) -> Result<u64, sqlx::Error> {
    delivery::reschedule_pending(pool, now, DeliveryScope::Digest, digest_intended).await
}

/// Момент сводки по ключу её вхождения и текущим настройкам получателя.
fn digest_intended(row: &NoticeRow) -> Option<DateTime<Utc>> {
    let settings = row.settings();
    let time =
        parse_hhmm(&settings.digest_time, "digest time").unwrap_or_else(|_| default_digest_time());
    Some(
        digest_key_date(&row.occurrence_key)
            .and_then(|date| resolve_local(row.timezone(), date, time).ok())
            .unwrap_or(row.created_at),
    )
}

#[derive(FromRow)]
struct DigestItem {
    id: Uuid,
    title: String,
    kind: ListKind,
    due_at: Option<DateTime<Utc>>,
    due_date: Option<NaiveDate>,
    end_at: Option<DateTime<Utc>>,
    end_date: Option<NaiveDate>,
}

struct DigestContent {
    today: Vec<DigestLine>,
    overdue: Vec<DigestLine>,
    shopping_open: i64,
    tasks_list: Option<Uuid>,
    shopping_list: Option<Uuid>,
}

/// Ключ сортировки задач и событий сводки: timed по времени, затем
/// вседневные по дате.
fn digest_sort_key(item: &DigestItem) -> (u8, Option<DateTime<Utc>>, Option<NaiveDate>, Uuid) {
    (
        u8::from(item.due_at.is_none()),
        item.due_at,
        item.due_date,
        item.id,
    )
}

/// Событие на сегодня: начинается сегодня или началось раньше и ещё идёт.
fn event_is_today(
    item: &DigestItem,
    timezone: Tz,
    household_timezone: Tz,
    today: NaiveDate,
    now: DateTime<Utc>,
) -> bool {
    let (start_day, started) = match (item.due_at, item.due_date) {
        (Some(due_at), _) => (
            Some(due_at.with_timezone(&timezone).date_naive()),
            due_at <= now,
        ),
        (None, Some(due_date)) => (Some(due_date), due_date <= today),
        (None, None) => (None, false),
    };
    if start_day == Some(today) {
        return true;
    }
    started
        && effective_end(
            item.due_at,
            item.due_date,
            item.end_at,
            item.end_date,
            household_timezone,
        )
        .is_some_and(|end| end > now)
}

/// Начало и конец события для строки сводки.
fn event_when(item: &DigestItem, timezone: Tz, today: NaiveDate) -> String {
    match (item.due_at, item.due_date) {
        (Some(due_at), _) => {
            let start = due_at.with_timezone(&timezone);
            let start_day = start.date_naive();
            let prefix = if start_day == today {
                String::new()
            } else {
                format!("{}, ", ru::date_label(start_day, today))
            };
            match item.end_at {
                Some(end_at) => {
                    let end = end_at.with_timezone(&timezone);
                    if end.date_naive() == start_day {
                        format!("{prefix}{}–{}", start.format("%H:%M"), end.format("%H:%M"))
                    } else {
                        format!(
                            "{prefix}{} – {}, {}",
                            start.format("%H:%M"),
                            ru::date_label(end.date_naive(), today),
                            end.format("%H:%M")
                        )
                    }
                }
                None => format!("{prefix}{}", start.format("%H:%M")),
            }
        }
        (None, Some(due_date)) => match item.end_date {
            Some(end_date) if end_date > due_date => {
                format!(
                    "{} — {}",
                    ru::date_label(due_date, today),
                    ru::date_label(end_date, today)
                )
            }
            _ => ru::ALL_DAY.to_owned(),
        },
        (None, None) => String::new(),
    }
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
    household_timezone: Tz,
    now: DateTime<Utc>,
) -> Result<DigestContent, sqlx::Error> {
    let mut rows: Vec<DigestItem> = sqlx::query_as(
        "SELECT i.id, i.title, l.kind, i.due_at, i.due_date, i.end_at, i.end_date
         FROM items i
         JOIN lists l
           ON l.id = i.list_id AND l.deleted_at IS NULL
          AND NOT l.is_archived AND l.kind IN ('tasks', 'events')
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
        if row.kind == ListKind::Events {
            // Событие никогда не попадает в просроченные: прошедшее просто не
            // показывается, а начавшееся раньше и ещё идущее — в «Сегодня».
            if event_is_today(&row, timezone, household_timezone, today, now) {
                today_lines.push(DigestLine {
                    when: event_when(&row, timezone, today),
                    title: row.title,
                    is_event: true,
                });
            }
            continue;
        }
        match (row.due_at, row.due_date) {
            (Some(due_at), None) if due_at < now => {
                overdue_lines.push(DigestLine {
                    title: row.title,
                    when: ru::due_label(Some(due_at), None, timezone, now).unwrap_or_default(),
                    is_event: false,
                });
            }
            (Some(due_at), None) if due_at.with_timezone(&timezone).date_naive() == today => {
                today_lines.push(DigestLine {
                    title: row.title,
                    when: due_at.with_timezone(&timezone).format("%H:%M").to_string(),
                    is_event: false,
                });
            }
            (None, Some(due_date)) if due_date < today => {
                overdue_lines.push(DigestLine {
                    title: row.title,
                    when: ru::date_label(due_date, today),
                    is_event: false,
                });
            }
            (None, Some(due_date)) if due_date == today => {
                today_lines.push(DigestLine {
                    title: row.title,
                    when: ru::ALL_DAY.to_owned(),
                    is_event: false,
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

/// Собрать сообщение сводки; содержимое собирается здесь, а не при
/// планировании.
pub(crate) async fn digest_message(
    conn: &mut PgConnection,
    row: &NoticeRow,
    telegram_user_id: i64,
    now: DateTime<Utc>,
) -> Result<Option<String>, sqlx::Error> {
    let content = content(
        conn,
        row.household_id,
        row.timezone(),
        resolved_timezone(None, row.household_timezone.as_deref()),
        now,
    )
    .await?;
    let ui = notify::digest_ui(
        &content.today,
        &content.overdue,
        content.shopping_open,
        content.tasks_list,
        content.shopping_list,
    );
    notify::render_notice(conn, telegram_user_id, row.household_id, &ui, now)
        .await
        .map(Some)
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
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT d.id FROM notification_deliveries d
         WHERE d.user_id = $1 AND d.status = 'pending' AND d.kind = 'digest'
           AND d.deferred
         ORDER BY d.scheduled_at, d.id
         FOR UPDATE OF d SKIP LOCKED",
    )
    .bind(user_id)
    .fetch_all(&mut *conn)
    .await?;

    let mut delivered = 0;
    for id in ids {
        delivered +=
            delivery::deliver_one_in_conn(conn, DeliveryScope::Digest, id, user_id, now, false)
                .await?;
    }
    Ok(delivered)
}
