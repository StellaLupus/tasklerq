//! Общий каркас доставки уведомлений.
//!
//! Напоминание, назначение и сводка отличаются только выбором кандидатов,
//! правилом актуальности (см. [`super::notice`]) и сборкой сообщения. Всё
//! остальное — пересчёт времени, отмена устаревших записей, проверка тихих
//! часов под блокировкой, outbox и смена статуса — общее.

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use super::notice::{NOTICE_ROW_COLUMNS, NOTICE_ROW_FROM, NoticeRow};
use super::quiet::{desired_schedule, quiet_end_at};
use super::{assignment, digest, scheduler};
use crate::domain::error::DomainError;
use crate::domain::models::{Item, List};
use crate::domain::settings::NotificationSettings;
use crate::storage::repositories::notifications::NotificationKind;
use crate::storage::repositories::{items, lists};
use crate::telegram::outbox::{self, Outgoing};

/// Сколько кандидатов на доставку рассматривается за один тик.
pub(crate) const DELIVERY_BATCH: i64 = 100;

/// Виды уведомлений с общим каркасом доставки.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeliveryScope {
    /// Напоминание о сроке и snooze.
    Reminder,
    /// Уведомление о назначении.
    Assignment,
    /// Утренняя сводка.
    Digest,
}

impl DeliveryScope {
    /// Вид построчной доставки по типу; у сводки свой путь, поэтому `None`.
    pub(crate) fn of(kind: NotificationKind) -> Option<Self> {
        match kind {
            NotificationKind::Reminder | NotificationKind::Snooze => Some(Self::Reminder),
            NotificationKind::Assignment => Some(Self::Assignment),
            NotificationKind::Digest => None,
        }
    }

    /// Фильтр `kind` для запросов к `notification_deliveries`.
    fn filter(self) -> &'static str {
        match self {
            Self::Reminder => "d.kind IN ('reminder', 'snooze')",
            Self::Assignment => "d.kind = 'assignment'",
            Self::Digest => "d.kind = 'digest'",
        }
    }

    /// Блокировка получателя нужна путям, которые собираются в сводное
    /// сообщение. Сводка в него не входит и доставляется отдельно, поэтому,
    /// как и раньше, обходится без неё.
    fn locks_recipient(self) -> bool {
        !matches!(self, Self::Digest)
    }
}

/// Пересчитать `pending`-доставки вида по текущим настройкам получателя.
///
/// `intended` возвращает момент, в который доставка должна уйти по своей
/// природе, или `None`, если вхождение больше не планируется. Время берётся из
/// срока, ключа и настроек, а не из старой `scheduled_at`: смена lead time,
/// времени сводки, пояса или границ тихих часов сдвигает ещё не доставленную
/// запись.
pub(crate) async fn reschedule_pending(
    pool: &PgPool,
    now: DateTime<Utc>,
    scope: DeliveryScope,
    intended: impl Fn(&NoticeRow) -> Option<DateTime<Utc>>,
) -> Result<u64, sqlx::Error> {
    let rows = pending_rows(pool, scope).await?;

    let mut conn = pool.acquire().await?;
    let mut rescheduled = 0;
    for row in rows {
        // Момент выпуска отложенного тихими часами уведомления фиксируется при
        // переносе. Последующие тики не пересчитывают его: выключение или
        // смена границ тихих часов не ускоряет уже отложенную доставку.
        if row.deferred {
            continue;
        }
        let Some(base) = intended(&row) else {
            continue;
        };
        let desired = desired_schedule(base, now, row.timezone(), &row.settings());
        let deferred = desired != base;
        rescheduled += reschedule_if_changed(&mut conn, row.id, desired, deferred, now).await?;
    }
    Ok(rescheduled)
}

/// Обновить `scheduled_at`, только если время или флаг переноса изменились.
async fn reschedule_if_changed(
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

/// `pending`-доставки вида со снимком связанных строк.
async fn pending_rows(pool: &PgPool, scope: DeliveryScope) -> Result<Vec<NoticeRow>, sqlx::Error> {
    let sql = format!(
        "SELECT {NOTICE_ROW_COLUMNS}
         {NOTICE_ROW_FROM}
         WHERE d.status = 'pending' AND {}",
        scope.filter()
    );
    sqlx::query_as(&sql).fetch_all(pool).await
}

/// Отменить `pending`-доставки вида, вхождение которых больше не актуально.
pub(crate) async fn cancel_stale(
    pool: &PgPool,
    now: DateTime<Utc>,
    scope: DeliveryScope,
) -> Result<u64, sqlx::Error> {
    let rows = pending_rows(pool, scope).await?;

    let mut cancelled = 0;
    for row in rows {
        if !row.is_stale(now) {
            continue;
        }
        match cancel_delivery(pool, row.id, now).await {
            Ok(rows) => cancelled += rows,
            Err(error) => tracing::error!(
                delivery_id = %row.id,
                scope = ?scope,
                %error,
                "failed to cancel stale notification delivery"
            ),
        }
    }
    Ok(cancelled)
}

/// Передать готовые доставки вида в outbox.
pub(crate) async fn deliver_due(
    pool: &PgPool,
    now: DateTime<Utc>,
    scope: DeliveryScope,
) -> Result<u64, sqlx::Error> {
    let sql = format!(
        "SELECT d.id, d.user_id FROM notification_deliveries d
         WHERE d.status = 'pending' AND {}
           AND d.scheduled_at <= $1
         ORDER BY d.scheduled_at, d.id
         LIMIT $2",
        scope.filter()
    );
    let ids: Vec<(Uuid, Uuid)> = sqlx::query_as(&sql)
        .bind(now)
        .bind(DELIVERY_BATCH)
        .fetch_all(pool)
        .await?;

    let mut delivered = 0;
    for (id, user_id) in ids {
        match deliver_one(pool, scope, id, user_id, now).await {
            Ok(rows) => delivered += rows,
            Err(error) => tracing::error!(
                delivery_id = %id,
                scope = ?scope,
                %error,
                "failed to deliver notification"
            ),
        }
    }
    Ok(delivered)
}

/// Доставить одну запись: outbox и `sent` в одной транзакции.
async fn deliver_one(
    pool: &PgPool,
    scope: DeliveryScope,
    id: Uuid,
    user_id: Uuid,
    now: DateTime<Utc>,
) -> Result<u64, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let delivered = deliver_one_in_conn(&mut tx, scope, id, user_id, now, true).await?;
    tx.commit().await?;
    Ok(delivered)
}

/// Доставить одну запись в уже открытой транзакции.
///
/// `due_only = false` используется ручным выпуском: запись забирается сразу,
/// не дожидаясь зафиксированного времени. Запись блокируется `FOR UPDATE SKIP
/// LOCKED`, поэтому два экземпляра планировщика не обработают её одновременно.
/// `dedupe_key` выводится из идентификатора доставки, поэтому даже повторная
/// постановка не создаст второе сообщение. Недоступный получатель или
/// устаревшее вхождение переводят запись в `cancelled`, а не оставляют её
/// навсегда в `pending`.
pub(crate) async fn deliver_one_in_conn(
    conn: &mut PgConnection,
    scope: DeliveryScope,
    id: Uuid,
    user_id: Uuid,
    now: DateTime<Utc>,
    due_only: bool,
) -> Result<u64, sqlx::Error> {
    // Тот же получатель не должен одновременно собираться в сводное другим
    // экземпляром планировщика.
    if scope.locks_recipient() && !try_recipient_lock(&mut *conn, user_id).await? {
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
           AND {}
           {due}
         FOR UPDATE OF d SKIP LOCKED",
        scope.filter()
    );
    let row: Option<NoticeRow> = sqlx::query_as(&sql)
        .bind(id)
        .bind(now)
        .fetch_optional(&mut *conn)
        .await?;

    let Some(row) = row else {
        return Ok(0);
    };
    if cancel_if_stale(&mut *conn, &row, now).await? {
        return Ok(0);
    }
    let Some((chat_id, telegram_user_id)) = ready_target(&mut *conn, &row, now).await? else {
        return Ok(0);
    };
    let Some(html) = render(&mut *conn, scope, &row, telegram_user_id, now).await? else {
        return Ok(0);
    };

    let dedupe_key = delivery_dedupe_key(row.id, row.kind);
    let seq = enqueue_message(&mut *conn, chat_id, &dedupe_key, html, true, now).await?;
    mark_sent(&mut *conn, row.id, seq, now).await?;
    Ok(1)
}

/// Отменить запись, если её вхождение больше не актуально.
///
/// Возвращает `true`, если запись отменена и дальше её обрабатывать не нужно.
pub(crate) async fn cancel_if_stale(
    conn: &mut PgConnection,
    row: &NoticeRow,
    now: DateTime<Utc>,
) -> Result<bool, sqlx::Error> {
    if !row.is_stale(now) {
        return Ok(false);
    }
    mark_cancelled(&mut *conn, row.id, now).await?;
    Ok(true)
}

/// Проверить тихие часы и получателя перед сборкой сообщения.
///
/// Возвращает чат и Telegram-пользователя или `None`, если доставка отложена
/// до конца тихих часов либо отменена из-за недоступного получателя. Проверки
/// выполняются под `FOR UPDATE`: «сброшенная» запись не уйдёт внутри тихого
/// интервала.
pub(crate) async fn ready_target(
    conn: &mut PgConnection,
    row: &NoticeRow,
    now: DateTime<Utc>,
) -> Result<Option<(i64, i64)>, sqlx::Error> {
    // Границы могли сдвинуть после переноса: в момент выпуска тихие часы
    // проверяются ещё раз и доставка снова фиксируется на их конце.
    if let Some(settings) = row.recipient_settings.as_ref()
        && postpone_for_quiet(&mut *conn, row.id, settings, row.timezone(), now).await?
    {
        return Ok(None);
    }
    // `is_stale` уже проверил эти поля; явная проверка не даёт отправить
    // сообщение в чат 0 при будущих изменениях схемы.
    let (Some(chat_id), Some(telegram_user_id)) = (row.telegram_chat_id, row.telegram_user_id)
    else {
        mark_cancelled(&mut *conn, row.id, now).await?;
        return Ok(None);
    };
    Ok(Some((chat_id, telegram_user_id)))
}

/// Собрать HTML сообщения по виду доставки; `None` — запись отменена.
async fn render(
    conn: &mut PgConnection,
    scope: DeliveryScope,
    row: &NoticeRow,
    telegram_user_id: i64,
    now: DateTime<Utc>,
) -> Result<Option<String>, sqlx::Error> {
    match scope {
        DeliveryScope::Reminder => {
            scheduler::reminder_message(conn, row, telegram_user_id, now).await
        }
        DeliveryScope::Assignment => {
            assignment::assignment_message(conn, row, telegram_user_id, now).await
        }
        DeliveryScope::Digest => digest::digest_message(conn, row, telegram_user_id, now).await,
    }
}

/// Ключ `dedupe_key` outbox для доставки.
fn delivery_dedupe_key(id: Uuid, kind: NotificationKind) -> String {
    format!("notification:{}:{id}", kind.as_str())
}

/// Отменить `pending`-доставку через пул.
async fn cancel_delivery(pool: &PgPool, id: Uuid, now: DateTime<Utc>) -> Result<u64, sqlx::Error> {
    let mut conn = pool.acquire().await?;
    mark_cancelled(&mut conn, id, now).await
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
async fn mark_sent(
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

/// Перенести уже заблокированную запись, если её момент доставки в тихих часах.
///
/// Вызывается под `FOR UPDATE`: даже если другой экземпляр планировщика успел
/// поставить `scheduled_at <= now`, доставка не произойдёт внутри тихого
/// интервала.
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

/// Ключ транзакционной блокировки получателя.
///
/// Последние восемь байт `user_id`: у UUIDv7 первые восемь байт заняты
/// временным префиксом и у соседних записей совпадают. Коллизии двух UUID
/// практически невозможны.
fn recipient_lock_key(user_id: Uuid) -> i64 {
    i64::from_be_bytes(
        user_id.as_bytes()[8..]
            .try_into()
            .expect("uuid has at least sixteen bytes"),
    )
}

/// Попробовать взять транзакционную блокировку получателя без ожидания.
///
/// Все пути доставки берут её до блокировки строк: тогда два экземпляра
/// планировщика не соберут сводное сообщение из части отложенных строк
/// (`SKIP LOCKED` пропустил бы строки, которые в этот момент доставляет
/// одиночный путь). Занятого получателя тик пропускает.
pub(crate) async fn try_recipient_lock(
    conn: &mut PgConnection,
    user_id: Uuid,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT pg_try_advisory_xact_lock($1)")
        .bind(recipient_lock_key(user_id))
        .fetch_one(conn)
        .await
}

/// Дождаться транзакционной блокировки получателя.
///
/// Планировщик держит её только на время одной транзакции тика, поэтому
/// обработчик бота может подождать: после коммита тика записи уже `sent`, и
/// ручной выпуск не создаст дубль.
pub(crate) async fn recipient_lock(
    conn: &mut PgConnection,
    user_id: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(recipient_lock_key(user_id))
        .execute(conn)
        .await?;
    Ok(())
}

/// Загрузить задачу и список доставки или отменить её.
///
/// Общий путь одиночной доставки, сводного сообщения и уведомления о
/// назначении: если задачу или список не удалось прочитать, запись переводится
/// в `cancelled`, а не остаётся навсегда в `pending`.
pub(crate) async fn load_item_and_list(
    conn: &mut PgConnection,
    row: &NoticeRow,
    now: DateTime<Utc>,
) -> Result<Option<(Item, List)>, sqlx::Error> {
    let Some(item_id) = row.item_id else {
        mark_cancelled(conn, row.id, now).await?;
        return Ok(None);
    };
    let item = match items::get(conn, row.household_id, item_id).await {
        Ok(item) => item,
        Err(DomainError::Database(error)) => return Err(error),
        Err(_) => {
            mark_cancelled(conn, row.id, now).await?;
            return Ok(None);
        }
    };
    let list = match lists::get(conn, row.household_id, item.list_id).await {
        Ok(list) => list,
        Err(DomainError::Database(error)) => return Err(error),
        Err(_) => {
            mark_cancelled(conn, row.id, now).await?;
            return Ok(None);
        }
    };
    Ok(Some((item, list)))
}
