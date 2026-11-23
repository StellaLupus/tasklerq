//! Синхронизация отправленных уведомлений (0018, решение 4).
//!
//! Шаг тика `sync_completed_notices` перерисовывает уже отправленные копии
//! напоминаний, отложенных напоминаний и уведомлений о назначении, когда задачу
//! выполнили не из них или назначили другому участнику, и возвращает копию,
//! если причина закрытия исчезла до отложенного удаления. Задачу выполняют из
//! разных мест (бот, HTTP API, Home Assistant), поэтому синхронизация живёт в
//! планировщике, а не в обработчиках выполнения.
//!
//! Кандидаты — доставки `reminder`/`snooze`/`assignment` со `status = 'sent'`,
//! без сводного блока (`batch_id IS NULL`) и без отметки уборки
//! (`dismissed_at IS NULL`), задача не событие, сообщение отправлено не старше
//! 47 часов, получатель жив, в том же household и с чатом. Каждая запись
//! обрабатывается в своей транзакции под `FOR UPDATE SKIP LOCKED` с повторной
//! проверкой условий после блокировки. Правки идут с `resend_if_gone = false`:
//! удалённое пользователем сообщение не возвращается.

use chrono::{DateTime, Duration, NaiveDate, Utc};
use sqlx::{FromRow, PgConnection, PgPool};
use uuid::Uuid;

use super::keys::snooze_origin;
use super::notice::{NOTICE_ROW_COLUMNS, NOTICE_ROW_FROM, NoticeRow};
use crate::domain::error::DomainError;
use crate::domain::models::{ItemStatus, User};
use crate::storage::repositories::items::completions::{self, Completion};
use crate::storage::repositories::notifications::NotificationKind;
use crate::storage::repositories::{items, lists, users};
use crate::telegram::bot::model::Ui;
use crate::telegram::bot::notify;
use crate::telegram::outbox::{self, Outgoing};

/// Окно выборки по `updated_at`: запись должна быть свежей.
const UPDATED_WINDOW_HOURS: i64 = 48;
/// Сообщение должно быть отправлено не старше этого срока: автоудаление должно
/// успеть до лимита Telegram в 48 часов.
const SENT_WINDOW_HOURS: i64 = 47;

/// Условия кандидата, общие для выборки и повторной проверки под блокировкой.
///
/// SQL-условия живого получателя (`u.…`, `h.…`) — предфильтр: они должны
/// совпадать с [`NoticeRow::recipient_alive`], который остаётся проверкой после
/// блокировки. Условие `closed_at` отсекает закрытые доставки, чьё окно возврата
/// уже истекло: возврат возможен только до `closed_at + UNDO_WINDOW_MINUTES`
/// (для выполнения — до момента выполнения + окно, что не позже).
const CANDIDATE_WHERE: &str = "d.status = 'sent'
     AND d.kind IN ('reminder', 'snooze', 'assignment')
     AND d.batch_id IS NULL
     AND d.dismissed_at IS NULL
     AND d.updated_at >= $1
     AND o.message_id IS NOT NULL
     AND o.sent_at >= $2
     AND (d.closed_at IS NULL OR d.closed_at > $3)
     AND l.kind IS DISTINCT FROM 'events'
     AND u.id IS NOT NULL AND u.deleted_at IS NULL
     AND u.household_id = d.household_id
     AND h.id IS NOT NULL AND h.deleted_at IS NULL
     AND u.telegram_user_id IS NOT NULL
     AND u.telegram_chat_id IS NOT NULL";

/// Доставка со снимком связанных строк и полями закрытия/outbox.
#[derive(Debug, FromRow)]
struct SyncRow {
    #[sqlx(flatten)]
    notice: NoticeRow,
    closed_at: Option<DateTime<Utc>>,
    closed_reason: Option<String>,
    closed_by_event: Option<Uuid>,
    outbox_message_id: Option<i64>,
    outbox_created_at: DateTime<Utc>,
}

/// Колонки выборки: снимок [`NoticeRow`] плюс закрытие и outbox.
fn columns() -> String {
    format!(
        "{NOTICE_ROW_COLUMNS},
         d.closed_at, d.closed_reason, d.closed_by_event,
         o.message_id AS outbox_message_id,
         o.created_at AS outbox_created_at"
    )
}

/// Источники выборки: [`NOTICE_ROW_FROM`] плюс строка outbox доставки.
fn from() -> String {
    format!("{NOTICE_ROW_FROM} JOIN outbox o ON o.seq = d.outbox_seq")
}

/// Синхронизировать отправленные уведомления; возвращает число изменённых
/// сообщений. Ошибка одной записи логируется и не прерывает остальные, но
/// возвращается наверх, чтобы тик сообщил её с названием шага.
pub(crate) async fn sync(pool: &PgPool, now: DateTime<Utc>) -> Result<u64, sqlx::Error> {
    let updated_since = now - Duration::hours(UPDATED_WINDOW_HOURS);
    let sent_since = now - Duration::hours(SENT_WINDOW_HOURS);
    let closed_since = now - Duration::minutes(items::UNDO_WINDOW_MINUTES);
    let sql = format!(
        "SELECT d.id {from} WHERE {CANDIDATE_WHERE} ORDER BY d.updated_at, d.id",
        from = from()
    );
    let ids: Vec<Uuid> = sqlx::query_scalar(&sql)
        .bind(updated_since)
        .bind(sent_since)
        .bind(closed_since)
        .fetch_all(pool)
        .await?;

    let mut changed = 0;
    let mut last_error = None;
    for id in ids {
        match sync_row(pool, id, now, updated_since, sent_since, closed_since).await {
            Ok(rows) => changed += rows,
            Err(error) => {
                tracing::error!(
                    delivery_id = %id,
                    %error,
                    "failed to sync notification delivery"
                );
                last_error = Some(error);
            }
        }
    }
    match last_error {
        Some(error) => Err(error),
        None => Ok(changed),
    }
}

/// Перечитать запись под блокировкой и обработать её по свежему снимку.
///
/// `0` — строку держит другая транзакция, она уже не подходит под условия или
/// менять нечего.
async fn sync_row(
    pool: &PgPool,
    id: Uuid,
    now: DateTime<Utc>,
    updated_since: DateTime<Utc>,
    sent_since: DateTime<Utc>,
    closed_since: DateTime<Utc>,
) -> Result<u64, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let sql = format!(
        "SELECT {columns} {from}
         WHERE d.id = $4 AND {CANDIDATE_WHERE}
         FOR UPDATE OF d SKIP LOCKED",
        columns = columns(),
        from = from()
    );
    let row: Option<SyncRow> = sqlx::query_as(&sql)
        .bind(updated_since)
        .bind(sent_since)
        .bind(closed_since)
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
    // SQL-условия живого получателя в выборке — предфильтр; после блокировки
    // проверку повторяет `NoticeRow::recipient_alive`.
    let changed = match row {
        Some(row) if row.notice.recipient_alive() => process(&mut tx, &row, now).await?,
        _ => 0,
    };
    tx.commit().await?;
    Ok(changed)
}

/// Закрыть копию по выполнению/назначению или вернуть её, если причина исчезла.
async fn process(
    conn: &mut PgConnection,
    row: &SyncRow,
    now: DateTime<Utc>,
) -> Result<u64, sqlx::Error> {
    let notice = &row.notice;
    if row.closed_at.is_none() {
        if let Some(completion) = matching_completion(conn, row).await? {
            return close_completed(conn, row, &completion, now).await;
        }
        // Закрыть по назначению: задача открыта, исполнитель есть и это не
        // получатель. Если в одном тике есть и выполнение, и назначение,
        // побеждает выполнение (проверено выше).
        if notice.item_alive()
            && matches!(notice.item_assignee_user_id, Some(assignee) if assignee != notice.user_id)
        {
            return close_assigned(conn, row, now).await;
        }
        return Ok(0);
    }
    maybe_return(conn, row, now).await
}

/// Действующее выполнение задачи, созданное после постановки доставки в outbox
/// и совпадающее с её вхождением.
async fn matching_completion(
    conn: &mut PgConnection,
    row: &SyncRow,
) -> Result<Option<Completion>, sqlx::Error> {
    let notice = &row.notice;
    let Some(item_id) = notice.item_id else {
        return Ok(None);
    };
    let completions =
        completions::since_for_item(conn, notice.household_id, item_id, row.outbox_created_at)
            .await
            .map_err(domain_db)?;
    Ok(completions
        .into_iter()
        .find(|completion| match notice.kind {
            NotificationKind::Reminder => occurrence_matches(
                &notice.occurrence_key,
                completion.due_at,
                completion.due_date,
            ),
            NotificationKind::Snooze => {
                notice.origin_key.as_deref()
                    == Some(snooze_origin(completion.due_at, completion.due_date).as_str())
            }
            NotificationKind::Assignment => true,
            NotificationKind::Digest => false,
        }))
}

/// Совпадение вхождения по разобранным значениям, а не строкам: срок с долями
/// секунды должен совпасть точно.
fn occurrence_matches(
    key: &str,
    due_at: Option<DateTime<Utc>>,
    due_date: Option<NaiveDate>,
) -> bool {
    if let Some(date) = key.strip_prefix("date:") {
        return due_at.is_none() && NaiveDate::parse_from_str(date, "%Y-%m-%d").ok() == due_date;
    }
    let Ok(at) = DateTime::parse_from_rfc3339(key) else {
        return false;
    };
    due_date.is_none() && due_at == Some(at.with_timezone(&Utc))
}

/// Закрыть копию по выполнению: «✅ Выполнено», удаление на `completed_at + 10
/// минут` с ключом по событию, отметка `completed`.
async fn close_completed(
    conn: &mut PgConnection,
    row: &SyncRow,
    completion: &Completion,
    now: DateTime<Utc>,
) -> Result<u64, sqlx::Error> {
    let notice = &row.notice;
    let (chat_id, message_id, _) = target(row);
    let member = member_option(
        conn,
        notice.household_id,
        completion.actor_user_id,
        notice.user_id,
    )
    .await?;
    let name = notify::member_name(completion.actor_user_id, notice.user_id, member.as_ref());
    let ui = notify::completed_ui(&completion.title, name.as_deref());
    edit_closed(conn, row, &ui, now).await?;
    let key = format!(
        "notice-expire:{chat_id}:{message_id}:{}",
        completion.event_id
    );
    outbox::enqueue_at(
        conn,
        chat_id,
        &Outgoing::Delete {
            message_id,
            cleanup: true,
        },
        Some(&key),
        now,
        completion.completed_at + Duration::minutes(items::UNDO_WINDOW_MINUTES),
    )
    .await?;
    sqlx::query(
        "UPDATE notification_deliveries
         SET closed_at = $2, closed_reason = 'completed', closed_by_event = $3, updated_at = $2
         WHERE id = $1",
    )
    .bind(notice.id)
    .bind(now)
    .bind(completion.event_id)
    .execute(conn)
    .await?;
    Ok(1)
}

/// Закрыть копию по назначению другому: «👤 Назначено», удаление на `now + 10
/// минут`, отметка `assigned`.
async fn close_assigned(
    conn: &mut PgConnection,
    row: &SyncRow,
    now: DateTime<Utc>,
) -> Result<u64, sqlx::Error> {
    let notice = &row.notice;
    let (chat_id, message_id, _) = target(row);
    let assignee = notice.item_assignee_user_id.expect("assignee checked");
    let title = notice
        .item_title
        .clone()
        .unwrap_or_else(|| completions::UNTITLED.to_owned());
    let member = member_option(conn, notice.household_id, Some(assignee), notice.user_id).await?;
    let name = notify::member_name(Some(assignee), notice.user_id, member.as_ref());
    let ui = notify::assigned_ui(&title, name.as_deref());
    edit_closed(conn, row, &ui, now).await?;
    let key = format!(
        "notice-expire:{chat_id}:{message_id}:assigned:{}",
        now.timestamp_millis()
    );
    outbox::enqueue_at(
        conn,
        chat_id,
        &Outgoing::Delete {
            message_id,
            cleanup: true,
        },
        Some(&key),
        now,
        now + Duration::minutes(items::UNDO_WINDOW_MINUTES),
    )
    .await?;
    sqlx::query(
        "UPDATE notification_deliveries
         SET closed_at = $2, closed_reason = 'assigned', closed_by_event = NULL, updated_at = $2
         WHERE id = $1",
    )
    .bind(notice.id)
    .bind(now)
    .execute(conn)
    .await?;
    Ok(1)
}

/// Вернуть копию, если причина закрытия исчезла до отложенного удаления.
async fn maybe_return(
    conn: &mut PgConnection,
    row: &SyncRow,
    now: DateTime<Utc>,
) -> Result<u64, sqlx::Error> {
    let notice = &row.notice;
    let closed_at = row.closed_at.expect("closed_at checked");
    let window = Duration::minutes(items::UNDO_WINDOW_MINUTES);
    let gone = match row.closed_reason.as_deref() {
        Some("completed") => {
            // Срок удаления отсчитывается от момента выполнения, а не отметки.
            let created =
                match row.closed_by_event {
                    Some(event_id) => sqlx::query_scalar(
                        "SELECT created_at FROM item_events WHERE id = $1 AND household_id = $2",
                    )
                    .bind(event_id)
                    .bind(notice.household_id)
                    .fetch_optional(&mut *conn)
                    .await?,
                    None => None,
                };
            if now >= created.unwrap_or(closed_at) + window {
                return Ok(0);
            }
            match row.closed_by_event {
                Some(event_id) => {
                    completions::active_for_event(conn, notice.household_id, event_id)
                        .await
                        .map_err(domain_db)?
                        .is_none()
                }
                None => true,
            }
        }
        Some("assigned") => {
            if now >= closed_at + window {
                return Ok(0);
            }
            notice
                .item_assignee_user_id
                .is_none_or(|assignee| assignee == notice.user_id)
        }
        _ => return Ok(0),
    };
    if !gone || !notice.item_alive() {
        return Ok(0);
    }
    let Some(item_id) = notice.item_id else {
        return Ok(0);
    };
    let item = match items::get(conn, notice.household_id, item_id).await {
        Ok(item) => item,
        Err(DomainError::Database(error)) => return Err(error),
        Err(_) => return Ok(0),
    };
    let list = match lists::get(conn, notice.household_id, item.list_id).await {
        Ok(list) => list,
        Err(DomainError::Database(error)) => return Err(error),
        Err(_) => return Ok(0),
    };
    if item.status != ItemStatus::Open {
        return Ok(0);
    }

    let (chat_id, message_id, telegram_user_id) = target(row);
    outbox::cancel_scheduled_deletes(conn, chat_id, message_id, now).await?;
    let ui = match notice.kind {
        NotificationKind::Assignment => notify::assignment_ui(&item, &list, notice.timezone(), now),
        _ => notify::reminder_ui(&item, &list, notice.timezone(), now, None),
    };
    let html = notify::render_notice(conn, telegram_user_id, notice.household_id, &ui, now).await?;
    outbox::enqueue(
        conn,
        chat_id,
        &Outgoing::Edit {
            message_id,
            html,
            resend_if_gone: false,
        },
        None,
        now,
    )
    .await?;
    sqlx::query(
        "UPDATE notification_deliveries
         SET closed_at = NULL, closed_reason = NULL, closed_by_event = NULL, updated_at = $2
         WHERE id = $1",
    )
    .bind(notice.id)
    .bind(now)
    .execute(conn)
    .await?;
    Ok(1)
}

/// Перерисовать закрытую копию правкой без повторной отправки.
async fn edit_closed(
    conn: &mut PgConnection,
    row: &SyncRow,
    ui: &Ui,
    now: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    let notice = &row.notice;
    let (chat_id, message_id, telegram_user_id) = target(row);
    let html = notify::render_notice(conn, telegram_user_id, notice.household_id, ui, now).await?;
    outbox::enqueue(
        conn,
        chat_id,
        &Outgoing::Edit {
            message_id,
            html,
            resend_if_gone: false,
        },
        None,
        now,
    )
    .await?;
    Ok(())
}

/// Чат, сообщение и Telegram-пользователь получателя (проверены выборкой).
fn target(row: &SyncRow) -> (i64, i64, i64) {
    (
        row.notice.telegram_chat_id.expect("chat checked"),
        row.outbox_message_id.expect("message_id checked"),
        row.notice.telegram_user_id.expect("telegram user checked"),
    )
}

/// Активный участник household получателя для имени; `None` — без участника,
/// сам получатель или исключённый/удалённый.
async fn member_option(
    conn: &mut PgConnection,
    household_id: Uuid,
    user_id: Option<Uuid>,
    recipient_user_id: Uuid,
) -> Result<Option<User>, sqlx::Error> {
    let Some(user_id) = user_id else {
        return Ok(None);
    };
    if user_id == recipient_user_id {
        return Ok(None);
    }
    match users::get(conn, household_id, user_id).await {
        Ok(user) => Ok(Some(user)),
        Err(DomainError::NotFound) => Ok(None),
        Err(DomainError::Database(error)) => Err(error),
        Err(_) => Ok(None),
    }
}

/// Ошибка БД доменного слоя как ошибка sqlx; прочие варианты не возникают у
/// запросов выборки выполнений.
fn domain_db(error: DomainError) -> sqlx::Error {
    match error {
        DomainError::Database(error) => error,
        other => sqlx::Error::Protocol(other.to_string()),
    }
}
