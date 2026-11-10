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

use super::delivery::{
    self, DELIVERY_BATCH, DeliveryScope, cancel_if_stale, enqueue_message, load_item_and_list,
    ready_target, try_recipient_lock,
};
use super::digest;
use super::notice::{NOTICE_ROW_COLUMNS, NOTICE_ROW_FROM, NoticeRow};
use crate::storage::repositories::notifications;
use crate::telegram::bot::notify;

/// Режим выпуска отложенных доставок.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReleaseMode {
    /// Тик планировщика: выпускаются только доставки, срок которых наступил.
    Tick,
    /// Ручной выпуск из настроек: только отложенные тихими часами. Групповой
    /// snooze и ещё не наступивший срок самого snooze не ускоряются.
    Manual,
}

/// Отложенная доставка и её блок сводного сообщения.
struct BatchCandidate {
    delivery_id: Uuid,
    scheduled_at: DateTime<Utc>,
    scope: DeliveryScope,
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
            Ok(messages) => delivered += messages,
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
/// Тик ограничивает выборку записями, срок которых уже наступил. Ручной выпуск
/// оставляет только отложенные тихими часами: он не ускоряет групповой snooze
/// и ещё не наступивший срок самого snooze.
fn deferred_row_sql(mode: ReleaseMode) -> String {
    let due = match mode {
        ReleaseMode::Tick => "AND d.scheduled_at <= $2",
        ReleaseMode::Manual => "",
    };
    let scope = match mode {
        ReleaseMode::Tick => "AND (d.deferred OR d.batched)",
        ReleaseMode::Manual => "AND d.deferred",
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

/// Собрать одно сводное сообщение на получателя.
async fn deliver_batch_for_user(
    pool: &PgPool,
    user_id: Uuid,
    now: DateTime<Utc>,
) -> Result<u64, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let delivered = deliver_deferred_batch(&mut tx, user_id, now, ReleaseMode::Tick).await?;
    tx.commit().await?;
    Ok(delivered)
}

/// Подготовленный выпуск: ничего ещё не поставлено в outbox.
enum PreparedBatch {
    /// Нечего выпускать.
    None,
    /// Одно уведомление обычным сообщением.
    Single { candidate: BatchCandidate },
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
}

impl PreparedRelease {
    /// Фактическое число уведомлений для баннера, без утренней сводки.
    pub fn notifications(&self) -> usize {
        self.prepared.notifications()
    }

    /// Выпускать нечего.
    pub fn is_empty(&self) -> bool {
        self.notifications() == 0
    }
}

/// Собрать одно сводное сообщение на получателя из отложенных доставок.
///
/// По тику одна оставшаяся доставка не отправляется здесь: её доставит обычный
/// путь. Ручной выпуск доставляет единственную отложенную тихими часами запись
/// сразу, не дожидаясь её зафиксированного времени; исключение — snooze, чей
/// собственный срок ещё впереди: ему возвращается этот срок и снимается признак
/// отложенности.
async fn deliver_deferred_batch(
    conn: &mut PgConnection,
    user_id: Uuid,
    now: DateTime<Utc>,
    mode: ReleaseMode,
) -> Result<u64, sqlx::Error> {
    let prepared = prepare_deferred_batch(&mut *conn, user_id, now, mode).await?;
    let messages = u64::from(prepared.has_message());
    commit_prepared_batch(conn, prepared, user_id, now).await?;
    Ok(messages)
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
    mode: ReleaseMode,
) -> Result<PreparedBatch, sqlx::Error> {
    if !try_recipient_lock(&mut *conn, user_id).await? {
        return Ok(PreparedBatch::None);
    }
    let rows: Vec<NoticeRow> = sqlx::query_as(&deferred_row_sql(mode))
        .bind(user_id)
        .bind(now)
        .fetch_all(&mut *conn)
        .await?;

    let mut identity: Option<BatchIdentity> = None;
    let mut candidates = Vec::new();
    for row in rows {
        if cancel_if_stale(&mut *conn, &row, now).await? {
            continue;
        }
        // Ручной выпуск не ускоряет snooze: если его собственный срок ещё
        // впереди, а тихие часы успели перенести запись на их конец, возвращаем
        // записи собственный срок и снимаем признак отложенности. Дальше она
        // идёт общим путём и уходит не раньше своего срока.
        if matches!(mode, ReleaseMode::Manual)
            && row.is_snooze()
            && let Some(at) = notifications::snooze_key_time(&row.occurrence_key)
            && at > now
        {
            restore_snooze_time(&mut *conn, row.id, at, now).await?;
            continue;
        }
        let Some(scope) = DeliveryScope::of(row.kind) else {
            // Запрос не выбирает сводку: она доставляется своим путём.
            continue;
        };
        let Some((chat_id, telegram_user_id)) = ready_target(&mut *conn, &row, now).await? else {
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
        // 0009-events 7.3: событие в сводном сообщении пока рисуется общим
        // блоком задачи, а «Выполнить все»/«Отложить все на час» его не
        // пропускают. Отдельный блок события появится в 7.3.
        candidates.push(BatchCandidate {
            delivery_id: row.id,
            scheduled_at: row.scheduled_at,
            scope,
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
        if matches!(mode, ReleaseMode::Manual)
            && let Some(candidate) = candidates.pop()
        {
            return Ok(PreparedBatch::Single { candidate });
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
) -> Result<(), sqlx::Error> {
    match prepared {
        PreparedBatch::None => Ok(()),
        PreparedBatch::Single { candidate } => {
            delivery::deliver_one_in_conn(
                conn,
                candidate.scope,
                candidate.delivery_id,
                user_id,
                now,
                false,
            )
            .await?;
            Ok(())
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
            Ok(())
        }
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
        // Свой срок вне нового интервала: вернуть его и снять признак
        // отложенности, иначе запись ушла бы раньше срока.
        if let Some(at) = notifications::snooze_key_time(&occurrence_key)
            && at > end
        {
            restore_snooze_time(&mut *conn, id, at, now).await?;
            postponed += 1;
        }
    }

    // Остальные отложенные записи, включая snooze внутри интервала, ждут его
    // конца. Восстановленные выше уже не `deferred` и сюда не попадают.
    postponed += sqlx::query(
        "UPDATE notification_deliveries
         SET scheduled_at = $2, deferred = true, updated_at = $3
         WHERE user_id = $1 AND status = 'pending' AND deferred
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
    let prepared = prepare_deferred_batch(&mut *conn, user_id, now, ReleaseMode::Manual).await?;
    Ok(PreparedRelease { user_id, prepared })
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
