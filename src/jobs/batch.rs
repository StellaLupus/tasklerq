//! Сводное сообщение: сборка из отложенных доставок, ручной выпуск и перенос.
//!
//! Отложенные тихими часами доставки (`deferred`) и групповой snooze действия
//! «Отложить все на час» (`batched`) в свой срок собираются в одно сообщение.
//! Ручной выпуск, баннер и перенос при всё ещё тихом интервале трогают только
//! `deferred`: групповой snooze ждёт своего срока. Snooze, собственный срок
//! которого ещё впереди, ручной выпуск не ускоряет: возвращает ему срок и
//! снимает признак отложенности.
//!
//! Сборка и смена статусов выполняются в одной транзакции, строки блокируются
//! `FOR UPDATE SKIP LOCKED`, а получатель — транзакционной advisory-блокировкой.
//! Поэтому два экземпляра планировщика не отправят сводное дважды и не соберут
//! его из части строк.

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use super::notice::{NOTICE_ROW_COLUMNS, NOTICE_ROW_FROM, NoticeRow};
use super::scheduler::{
    DELIVERY_BATCH, deliver_one_in_conn, enqueue_message, load_item_and_list, mark_cancelled,
    postpone_for_quiet, try_recipient_lock,
};
use super::{assignment, digest};
use crate::storage::repositories::notifications::{self, NotificationKind};
use crate::telegram::bot::notify;

/// Итог выпуска: сообщения в outbox и включённые в них уведомления.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Delivered {
    /// Поставлено в outbox сообщений.
    pub messages: u64,
    /// Уведомлений, включённых в эти сообщения.
    pub notifications: usize,
}

/// Отложенная доставка и её блок сводного сообщения.
struct BatchCandidate {
    delivery_id: Uuid,
    scheduled_at: DateTime<Utc>,
    entry: notify::BatchEntry,
}

/// Получатель и чат сводного сообщения.
struct BatchIdentity {
    household_id: Uuid,
    chat_id: i64,
    telegram_user_id: i64,
    timezone: Tz,
}

/// Собрать одно сводное сообщение на получателя из отложенных доставок.
pub(crate) async fn deliver_batches(pool: &PgPool, now: DateTime<Utc>) -> Result<u64, sqlx::Error> {
    let users: Vec<Uuid> = sqlx::query_scalar(
        "SELECT DISTINCT user_id
         FROM notification_deliveries
         WHERE status = 'pending' AND (deferred OR batched) AND scheduled_at <= $1
           AND kind IN ('reminder', 'snooze', 'assignment')
         ORDER BY user_id
         LIMIT $2",
    )
    .bind(now)
    .bind(DELIVERY_BATCH)
    .fetch_all(pool)
    .await?;

    let mut delivered = 0;
    for user_id in users {
        match deliver_batch_for_user(pool, user_id, now).await {
            Ok(rows) => delivered += rows.messages,
            Err(error) => tracing::error!(
                %user_id,
                %error,
                "failed to deliver notification batch"
            ),
        }
    }
    Ok(delivered)
}

/// SQL отложенных доставок получателя для сборки сводного сообщения.
///
/// `due_only` ограничивает выборку записями, срок которых уже наступил: так
/// работает тик. `quiet_only` оставляет только отложенные тихими часами: так
/// работает ручной выпуск, который не ускоряет групповой snooze и ещё не
/// наступивший срок самого snooze.
fn deferred_row_sql(due_only: bool, quiet_only: bool) -> String {
    let due = if due_only {
        "AND d.scheduled_at <= $2"
    } else {
        ""
    };
    let scope = if quiet_only {
        "AND d.deferred"
    } else {
        "AND (d.deferred OR d.batched)"
    };
    format!(
        "SELECT {NOTICE_ROW_COLUMNS}
         {NOTICE_ROW_FROM}
         WHERE d.user_id = $1 AND d.status = 'pending'
           {scope}
           {due}
           AND d.kind IN ('reminder', 'snooze', 'assignment')
         ORDER BY d.scheduled_at, d.id
         FOR UPDATE OF d SKIP LOCKED"
    )
}

async fn deliver_batch_for_user(
    pool: &PgPool,
    user_id: Uuid,
    now: DateTime<Utc>,
) -> Result<Delivered, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let delivered = deliver_deferred_batch(&mut tx, user_id, now, true, false).await?;
    tx.commit().await?;
    Ok(delivered)
}

/// Подготовленный выпуск: ничего ещё не поставлено в outbox.
enum PreparedBatch {
    /// Нечего выпускать.
    None,
    /// Одно уведомление обычным сообщением.
    Single {
        candidate: BatchCandidate,
        user_id: Uuid,
    },
    /// Сводное сообщение с готовым HTML.
    Message {
        identity: BatchIdentity,
        batch_id: Uuid,
        html: String,
        candidates: Vec<BatchCandidate>,
    },
}

impl PreparedBatch {
    fn notifications(&self) -> usize {
        match self {
            Self::None => 0,
            Self::Single { .. } => 1,
            Self::Message { candidates, .. } => candidates.len(),
        }
    }

    fn has_message(&self) -> bool {
        !matches!(self, Self::None)
    }
}

/// Подготовленный ручной выпуск отложенных тихими часами уведомлений.
pub(crate) struct PreparedRelease {
    user_id: Uuid,
    prepared: PreparedBatch,
    /// Фактическое число уведомлений для баннера, без утренней сводки.
    pub notifications: usize,
}

impl PreparedRelease {
    /// Выпускать нечего.
    pub fn is_empty(&self) -> bool {
        self.notifications == 0
    }
}

/// Собрать одно сводное сообщение на получателя из отложенных доставок.
///
/// При `due_only` одна оставшаяся доставка не отправляется здесь: её доставит
/// обычный путь. Ручной выпуск (`due_only = false`, `quiet_only = true`) доставляет
/// единственную отложенную тихими часами запись сразу, не дожидаясь её
/// зафиксированного времени; исключение — snooze, чей собственный срок ещё
/// впереди: ему возвращается этот срок и снимается признак отложенности.
pub(crate) async fn deliver_deferred_batch(
    conn: &mut PgConnection,
    user_id: Uuid,
    now: DateTime<Utc>,
    due_only: bool,
    quiet_only: bool,
) -> Result<Delivered, sqlx::Error> {
    let prepared = prepare_deferred_batch(&mut *conn, user_id, now, due_only, quiet_only).await?;
    let messages = u64::from(prepared.has_message());
    let notifications = commit_prepared_batch(conn, prepared, user_id, now).await?;
    Ok(Delivered {
        messages,
        notifications,
    })
}

/// Отобрать отложенные доставки, проверить их и собрать сообщения.
///
/// Выполняется под блокировкой получателя. В outbox ничего не ставится и
/// статусы не меняются: это позволяет показать экран с баннером раньше
/// сообщений, но всё ещё в одной транзакции с выпуском.
async fn prepare_deferred_batch(
    conn: &mut PgConnection,
    user_id: Uuid,
    now: DateTime<Utc>,
    due_only: bool,
    quiet_only: bool,
) -> Result<PreparedBatch, sqlx::Error> {
    if !try_recipient_lock(&mut *conn, user_id).await? {
        return Ok(PreparedBatch::None);
    }
    let rows: Vec<NoticeRow> = sqlx::query_as(&deferred_row_sql(due_only, quiet_only))
        .bind(user_id)
        .bind(now)
        .fetch_all(&mut *conn)
        .await?;

    let mut identity: Option<BatchIdentity> = None;
    let mut candidates = Vec::new();
    for row in rows {
        if row.is_stale(now) {
            mark_cancelled(&mut *conn, row.id, now).await?;
            continue;
        }
        // Ручной выпуск не ускоряет snooze: если его собственный срок ещё
        // впереди, а тихие часы успели перенести запись на их конец, возвращаем
        // записи собственный срок и снимаем признак отложенности. Дальше она
        // идёт общим путём и уходит не раньше своего срока.
        if !due_only
            && row.is_snooze()
            && let Some(at) = notifications::snooze_key_time(&row.occurrence_key)
            && at > now
        {
            restore_snooze_time(&mut *conn, row.id, at, now).await?;
            continue;
        }
        // Границы могли сдвинуть после переноса: в момент выпуска тихие часы
        // проверяются ещё раз и доставка снова фиксируется на их конце.
        if let Some(settings) = row.recipient_settings.as_ref()
            && postpone_for_quiet(&mut *conn, row.id, settings, row.timezone(), now).await?
        {
            continue;
        }
        let (Some(chat_id), Some(telegram_user_id)) = (row.telegram_chat_id, row.telegram_user_id)
        else {
            mark_cancelled(&mut *conn, row.id, now).await?;
            continue;
        };
        let Some((item, list)) = load_item_and_list(&mut *conn, &row, now).await? else {
            continue;
        };
        if identity.is_none() {
            identity = Some(BatchIdentity {
                household_id: row.household_id,
                chat_id,
                telegram_user_id,
                timezone: row.timezone(),
            });
        }
        candidates.push(BatchCandidate {
            delivery_id: row.id,
            scheduled_at: row.scheduled_at,
            entry: notify::BatchEntry {
                item_id: item.id,
                kind: row.kind,
                title: item.title.clone(),
                when: notify::when_line(
                    item.due_at,
                    item.due_date,
                    &list.name,
                    row.timezone(),
                    now,
                ),
                version: item.version,
                state: notify::BatchEntryState::Open,
            },
        });
    }

    if candidates.len() < 2 {
        // Ручной выпуск одного уведомления: обычное сообщение сразу, а не в
        // зафиксированное время.
        if candidates.len() == 1 && !due_only {
            return Ok(PreparedBatch::Single {
                candidate: candidates.pop().expect("one candidate"),
                user_id,
            });
        }
        return Ok(PreparedBatch::None);
    }
    let Some(identity) = identity else {
        return Ok(PreparedBatch::None);
    };
    candidates.sort_by_key(|candidate| (candidate.scheduled_at, candidate.delivery_id));
    let entries: Vec<notify::BatchEntry> = candidates
        .iter()
        .map(|candidate| candidate.entry.clone())
        .collect();

    let batch_id = Uuid::now_v7();
    let page = &entries[..entries.len().min(notify::BATCH_PAGE_SIZE)];
    let ui = notify::batch_ui(batch_id, 0, page, entries.len(), None, identity.timezone);
    let html = notify::render_notice(
        &mut *conn,
        identity.telegram_user_id,
        identity.household_id,
        &ui,
        now,
    )
    .await?;

    Ok(PreparedBatch::Message {
        identity,
        batch_id,
        html,
        candidates,
    })
}

/// Вернуть snooze его собственному сроку.
///
/// Вызывается, когда срок ещё впереди: ручным выпуском или переносом после
/// правки тихих часов. Выпускать запись раньше нельзя. Она остаётся `pending`,
/// и обычный путь доставит её в срок или снова отложит, если тогда тихо.
async fn restore_snooze_time(
    conn: &mut PgConnection,
    id: Uuid,
    scheduled_at: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE notification_deliveries
         SET scheduled_at = $2, deferred = false, updated_at = $3
         WHERE id = $1 AND status = 'pending'",
    )
    .bind(id)
    .bind(scheduled_at)
    .bind(now)
    .execute(conn)
    .await?;
    Ok(())
}

/// Поставить подготовленный выпуск в outbox и отметить доставки.
async fn commit_prepared_batch(
    conn: &mut PgConnection,
    prepared: PreparedBatch,
    user_id: Uuid,
    now: DateTime<Utc>,
) -> Result<usize, sqlx::Error> {
    match prepared {
        PreparedBatch::None => Ok(0),
        PreparedBatch::Single { candidate, user_id } => {
            let delivered = deliver_single_deferred(conn, &candidate, user_id, now).await?;
            Ok(usize::from(delivered > 0))
        }
        PreparedBatch::Message {
            identity,
            batch_id,
            html,
            candidates,
        } => {
            notifications::create_batch(
                &mut *conn,
                batch_id,
                identity.household_id,
                user_id,
                identity.chat_id,
                now,
            )
            .await?;
            let dedupe_key = format!("notification:batch:{batch_id}");
            let seq =
                enqueue_message(&mut *conn, identity.chat_id, &dedupe_key, html, true, now).await?;
            for candidate in &candidates {
                mark_batch_sent(
                    &mut *conn,
                    candidate.delivery_id,
                    seq,
                    batch_id,
                    candidate.entry.version,
                    now,
                )
                .await?;
            }
            Ok(candidates.len())
        }
    }
}

/// Доставить единственную отложенную запись обычным сообщением.
async fn deliver_single_deferred(
    conn: &mut PgConnection,
    candidate: &BatchCandidate,
    user_id: Uuid,
    now: DateTime<Utc>,
) -> Result<u64, sqlx::Error> {
    match candidate.entry.kind {
        NotificationKind::Reminder | NotificationKind::Snooze => {
            deliver_one_in_conn(conn, candidate.delivery_id, user_id, now, false).await
        }
        NotificationKind::Assignment => {
            assignment::deliver_one_in_conn(conn, candidate.delivery_id, user_id, now, false).await
        }
        // Запрос сводного явно ограничен видами reminder/snooze/assignment.
        NotificationKind::Digest => unreachable!("digest is delivered separately"),
    }
}

/// Перенести отложенные тихими часами записи получателя на конец интервала.
///
/// Вызывается после правки тихих часов, если сейчас по новым настройкам
/// по-прежнему тихо: выпускать нечего, но старое зафиксированное время больше
/// не соответствует новому интервалу. Групповой snooze не переносится: он ждёт
/// своего срока.
///
/// Snooze никогда не уходит раньше собственного срока: ему ставится
/// `max(конец интервала, свой срок)`. Если собственный срок позже конца нового
/// интервала, он и возвращается записи, а признак отложенности снимается — так
/// же, как при ручном выпуске.
pub(crate) async fn postpone_deferred_for_user(
    conn: &mut PgConnection,
    user_id: Uuid,
    end: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Result<u64, sqlx::Error> {
    let mut postponed = 0;
    let snoozes: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT id, occurrence_key FROM notification_deliveries
         WHERE user_id = $1 AND status = 'pending' AND deferred AND kind = 'snooze'",
    )
    .bind(user_id)
    .fetch_all(&mut *conn)
    .await?;
    for (id, occurrence_key) in snoozes {
        match notifications::snooze_key_time(&occurrence_key) {
            // Свой срок вне нового интервала: вернуть его и снять признак
            // отложенности, иначе запись ушла бы раньше срока.
            Some(at) if at > end => {
                restore_snooze_time(&mut *conn, id, at, now).await?;
                postponed += 1;
            }
            _ => {
                postponed += sqlx::query(
                    "UPDATE notification_deliveries
                     SET scheduled_at = $2, deferred = true, updated_at = $3
                     WHERE id = $1 AND status = 'pending' AND scheduled_at <> $2",
                )
                .bind(id)
                .bind(end)
                .bind(now)
                .execute(&mut *conn)
                .await?
                .rows_affected();
            }
        }
    }

    postponed += sqlx::query(
        "UPDATE notification_deliveries
         SET scheduled_at = $2, deferred = true, updated_at = $3
         WHERE user_id = $1 AND status = 'pending' AND deferred
           AND kind <> 'snooze'
           AND scheduled_at <> $2",
    )
    .bind(user_id)
    .bind(end)
    .bind(now)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    Ok(postponed)
}

/// Отобрать отложенные тихими часами уведомления для ручного выпуска.
///
/// Вызывается из обработчика настроек до показа экрана: число уведомлений
/// нужно для баннера, но сообщения должны попасть в outbox после экрана.
pub(crate) async fn prepare_release(
    conn: &mut PgConnection,
    user_id: Uuid,
    now: DateTime<Utc>,
) -> Result<PreparedRelease, sqlx::Error> {
    let prepared = prepare_deferred_batch(&mut *conn, user_id, now, false, true).await?;
    let notifications = prepared.notifications();
    Ok(PreparedRelease {
        user_id,
        prepared,
        notifications,
    })
}

/// Выпустить подготовленные уведомления, затем отложенную утреннюю сводку.
///
/// Вызывается после показа экрана в той же транзакции: так экран оказывается
/// в outbox раньше сводного и сводки.
pub(crate) async fn commit_release(
    conn: &mut PgConnection,
    release: PreparedRelease,
    now: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    commit_prepared_batch(&mut *conn, release.prepared, release.user_id, now).await?;
    digest::release_deferred(&mut *conn, release.user_id, now).await?;
    Ok(())
}

/// Пометить доставку переданной в outbox вместе со сводным сообщением.
async fn mark_batch_sent(
    conn: &mut PgConnection,
    id: Uuid,
    seq: i64,
    batch_id: Uuid,
    version: i32,
    now: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE notification_deliveries
         SET status = 'sent', outbox_seq = $2, batch_id = $3, batch_version = $4,
             batch_state = NULL, batch_snoozed_until = NULL, updated_at = $5
         WHERE id = $1 AND status = 'pending'",
    )
    .bind(id)
    .bind(seq)
    .bind(batch_id)
    .bind(version)
    .bind(now)
    .execute(&mut *conn)
    .await?;
    Ok(())
}
