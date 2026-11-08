//! Записи доставки уведомлений.
//!
//! Тип уведомления — Rust-enum с `sqlx::Type`: опечатка в имени PostgreSQL-типа
//! ловится компилятором, а не SQL-сервером. Уведомление о назначении ставится
//! в той же транзакции, что и смена исполнителя, поэтому доставка не зависит
//! от выбора `EventType` и не теряется при обновлении, меняющем несколько
//! полей сразу.

use chrono::{DateTime, NaiveDate, Utc};
use sqlx::{FromRow, PgConnection};
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::models::{Actor, ItemStatus};

/// Тип уведомления; совпадает с PostgreSQL-типом `notification_kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "notification_kind", rename_all = "lowercase")]
pub enum NotificationKind {
    /// Напоминание о сроке.
    Reminder,
    /// Утренняя сводка.
    Digest,
    /// Назначение исполнителя.
    Assignment,
    /// Отложенное напоминание; ключ включает время snooze.
    Snooze,
}

impl NotificationKind {
    /// Значение типа для подписей и ключей.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reminder => "reminder",
            Self::Digest => "digest",
            Self::Assignment => "assignment",
            Self::Snooze => "snooze",
        }
    }
}

/// Ключ snooze-доставки: время, на которое пользователь отложил напоминание.
pub fn snooze_key(deliver_at: DateTime<Utc>) -> String {
    format!("snooze:{}", deliver_at.timestamp())
}

/// Итог действия над блоком сводного сообщения.
///
/// Хранится в `notification_deliveries.batch_state`; строка без состояния —
/// блок ещё открыт. Значение `changed` намеренно отсутствует: задача,
/// изменившаяся после выпуска, перерисовывается по текущему состоянию, а не
/// замирает без кнопок.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "text", rename_all = "lowercase")]
pub enum BatchState {
    /// Задача завершена действием из сводного сообщения.
    Done,
    /// Задача отложена действием из сводного сообщения.
    Snoozed,
    /// Задача удалена или недоступна.
    Missing,
}

/// Время из ключа snooze-доставки.
pub fn snooze_key_time(key: &str) -> Option<DateTime<Utc>> {
    let seconds = key.strip_prefix("snooze:")?.parse::<i64>().ok()?;
    DateTime::from_timestamp(seconds, 0)
}

/// Новая запись доставки.
pub struct NewDelivery<'a> {
    /// Household получателя.
    pub household_id: Uuid,
    /// Получатель.
    pub user_id: Uuid,
    /// Элемент; пусто у сводки.
    pub item_id: Option<Uuid>,
    /// Тип уведомления.
    pub kind: NotificationKind,
    /// Ключ вхождения: срок, дата сводки или id события назначения.
    pub occurrence_key: &'a str,
    /// Вхождение срока, из которого отложено snooze; у остальных видов пусто.
    pub origin_key: Option<&'a str>,
    /// Момент, начиная с которого запись можно доставлять.
    pub scheduled_at: DateTime<Utc>,
}

/// Вставить доставку, если такого актуального вхождения ещё нет.
///
/// `pending` и `sent` отфильтровываются в SQL, поэтому повторный тик не делает
/// лишних вставок. `cancelled` возвращается в `pending`, когда вхождение снова
/// стало актуальным (например, задачу завершили и сразу вернули).
pub async fn insert(
    conn: &mut PgConnection,
    delivery: &NewDelivery<'_>,
    now: DateTime<Utc>,
) -> Result<u64, sqlx::Error> {
    insert_with_flags(conn, delivery, false, now).await
}

/// Вставить групповую snooze-доставку действия «Отложить все на час».
///
/// `batched` означает «собрать в сводное в свой срок», но не «отложено тихими
/// часами»: ручной выпуск и баннер такую доставку не трогают. Если её срок сам
/// попадёт в тихие часы, тик выставит `deferred` по общему правилу.
pub async fn insert_batched(
    conn: &mut PgConnection,
    delivery: &NewDelivery<'_>,
    now: DateTime<Utc>,
) -> Result<u64, sqlx::Error> {
    insert_with_flags(conn, delivery, true, now).await
}

/// Вставить доставку; `batched` отмечает групповой snooze. Признак `deferred`
/// при вставке всегда снят: его выставляет только перенос тихими часами.
async fn insert_with_flags(
    conn: &mut PgConnection,
    delivery: &NewDelivery<'_>,
    batched: bool,
    now: DateTime<Utc>,
) -> Result<u64, sqlx::Error> {
    Ok(sqlx::query(
        "INSERT INTO notification_deliveries (
             id, household_id, user_id, item_id, occurrence_key, origin_key, kind,
             status, scheduled_at, deferred, batched, created_at, updated_at
         )
         SELECT $1, $2, $3, $4, $5, $6, $7, 'pending', $8, false, $9, $10, $10
         WHERE NOT EXISTS (
             SELECT 1 FROM notification_deliveries d
             WHERE d.user_id = $3 AND d.item_id IS NOT DISTINCT FROM $4
               AND d.occurrence_key = $5 AND d.kind = $7
               AND d.origin_key IS NOT DISTINCT FROM $6
               AND d.status IN ('pending', 'sent')
         )
         ON CONFLICT ON CONSTRAINT notification_deliveries_key_unique
         DO UPDATE SET
             status = 'pending',
             origin_key = EXCLUDED.origin_key,
             scheduled_at = EXCLUDED.scheduled_at,
             deferred = EXCLUDED.deferred,
             batched = EXCLUDED.batched,
             outbox_seq = NULL,
             updated_at = EXCLUDED.updated_at
         WHERE notification_deliveries.status = 'cancelled'
            OR (notification_deliveries.status = 'pending'
                AND notification_deliveries.origin_key IS DISTINCT FROM EXCLUDED.origin_key)",
    )
    .bind(Uuid::now_v7())
    .bind(delivery.household_id)
    .bind(delivery.user_id)
    .bind(delivery.item_id)
    .bind(delivery.occurrence_key)
    .bind(delivery.origin_key)
    .bind(delivery.kind)
    .bind(delivery.scheduled_at)
    .bind(batched)
    .bind(now)
    .execute(conn)
    .await?
    .rows_affected())
}

/// Сводное сообщение, выпущенное после тихих часов.
#[derive(Debug, Clone, FromRow)]
pub struct Batch {
    /// Идентификатор.
    pub id: Uuid,
    /// Household получателя.
    pub household_id: Uuid,
    /// Получатель.
    pub user_id: Uuid,
    /// Чат получателя.
    pub chat_id: i64,
    /// Текущая страница с нуля.
    pub page: i32,
    /// Сколько задач завершило «✓ Выполнить все».
    pub result_completed: Option<i32>,
    /// Сколько задач уже изменились к моменту «✓ Выполнить все».
    pub result_changed: Option<i32>,
}

/// Создать запись сводного сообщения.
pub async fn create_batch(
    conn: &mut PgConnection,
    id: Uuid,
    household_id: Uuid,
    user_id: Uuid,
    chat_id: i64,
    now: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO notification_batches
             (id, household_id, user_id, chat_id, page, created_at, updated_at)
         VALUES ($1, $2, $3, $4, 0, $5, $5)",
    )
    .bind(id)
    .bind(household_id)
    .bind(user_id)
    .bind(chat_id)
    .bind(now)
    .execute(conn)
    .await?;
    Ok(())
}

/// Получить сводное сообщение.
pub async fn get_batch(conn: &mut PgConnection, id: Uuid) -> Result<Option<Batch>, sqlx::Error> {
    sqlx::query_as(
        "SELECT id, household_id, user_id, chat_id, page, result_completed, result_changed
         FROM notification_batches WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(conn)
    .await
}

/// Запомнить текущую страницу сводного сообщения.
pub async fn set_batch_page(
    conn: &mut PgConnection,
    id: Uuid,
    page: i32,
    now: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE notification_batches SET page = $2, updated_at = $3 WHERE id = $1")
        .bind(id)
        .bind(page)
        .bind(now)
        .execute(conn)
        .await?;
    Ok(())
}

/// Запомнить итог «✓ Выполнить все».
pub async fn set_batch_result(
    conn: &mut PgConnection,
    id: Uuid,
    completed: i32,
    changed: i32,
    now: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE notification_batches
         SET result_completed = $2, result_changed = $3, updated_at = $4
         WHERE id = $1",
    )
    .bind(id)
    .bind(completed)
    .bind(changed)
    .bind(now)
    .execute(conn)
    .await?;
    Ok(())
}

/// Доставка, входящая в сводное сообщение, с текущим состоянием задачи.
#[derive(Debug, Clone, FromRow)]
pub struct BatchDelivery {
    /// Идентификатор доставки.
    pub id: Uuid,
    /// Элемент; у сводки пусто, у батча всегда есть.
    pub item_id: Option<Uuid>,
    /// Тип уведомления.
    pub kind: NotificationKind,
    /// Версия задачи на момент выпуска.
    pub batch_version: Option<i32>,
    /// Итог действия над блоком.
    pub batch_state: Option<BatchState>,
    /// Время нового snooze.
    pub batch_snoozed_until: Option<DateTime<Utc>>,
    /// Момент выпуска.
    pub scheduled_at: DateTime<Utc>,
    /// Название задачи.
    pub title: Option<String>,
    /// Статус задачи.
    pub item_status: Option<ItemStatus>,
    /// Задача удалена.
    pub item_deleted_at: Option<DateTime<Utc>>,
    /// Текущая версия задачи.
    pub item_version: Option<i32>,
    /// Срок со временем.
    pub due_at: Option<DateTime<Utc>>,
    /// Срок на весь день.
    pub due_date: Option<NaiveDate>,
    /// Название списка.
    pub list_name: Option<String>,
    /// Список удалён.
    pub list_deleted_at: Option<DateTime<Utc>>,
    /// Список в архиве.
    pub list_archived: Option<bool>,
}

/// Доставки сводного сообщения в порядке выпуска.
pub async fn batch_deliveries(
    conn: &mut PgConnection,
    batch_id: Uuid,
) -> Result<Vec<BatchDelivery>, sqlx::Error> {
    sqlx::query_as(
        "SELECT d.id, d.item_id, d.kind AS kind, d.batch_version,
                d.batch_state, d.batch_snoozed_until, d.scheduled_at,
                i.title, i.status AS item_status,
                i.deleted_at AS item_deleted_at, i.version AS item_version,
                i.due_at, i.due_date,
                l.name AS list_name, l.deleted_at AS list_deleted_at,
                l.is_archived AS list_archived
         FROM notification_deliveries d
         LEFT JOIN items i ON i.id = d.item_id
         LEFT JOIN lists l ON l.id = i.list_id
         WHERE d.batch_id = $1
         ORDER BY d.scheduled_at, d.id",
    )
    .bind(batch_id)
    .fetch_all(conn)
    .await
}

/// Пометить все доставки задачи в сводном сообщении итогом действия.
pub async fn set_batch_state(
    conn: &mut PgConnection,
    batch_id: Uuid,
    item_id: Uuid,
    state: BatchState,
    snoozed_until: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE notification_deliveries
         SET batch_state = $3, batch_snoozed_until = $4, updated_at = $5
         WHERE batch_id = $1 AND item_id = $2",
    )
    .bind(batch_id)
    .bind(item_id)
    .bind(state)
    .bind(snoozed_until)
    .bind(now)
    .execute(conn)
    .await?;
    Ok(())
}

/// Параметры уведомления о назначении.
pub struct AssignmentNotice<'a> {
    /// Household элемента.
    pub household_id: Uuid,
    /// Элемент.
    pub item_id: Uuid,
    /// Автор действия.
    pub actor: &'a Actor,
    /// Исполнитель до изменения.
    pub previous_assignee: Option<Uuid>,
    /// Исполнитель после изменения.
    pub assignee: Option<Uuid>,
    /// Событие изменения; его id уникален для каждого назначения.
    pub event_id: Uuid,
}

/// Поставить уведомление о назначении в текущей транзакции.
///
/// Уведомление нужно только при смене исполнителя и только если назначают не
/// самому себе. Ключ вхождения — id события изменения: A→B→A создаёт три
/// события и два уведомления для A, а повторный PATCH того же исполнителя не
/// меняет `assignee_user_id` и ничего не вставляет.
pub async fn notify_assignment(
    conn: &mut PgConnection,
    notice: &AssignmentNotice<'_>,
    now: DateTime<Utc>,
) -> Result<u64, DomainError> {
    if notice.previous_assignee == notice.assignee {
        return Ok(0);
    }

    // Новое назначение вытесняет ещё не доставленные уведомления о прежних
    // назначениях этого элемента, независимо от получателя. Иначе A→B→A до
    // первого тика оставил бы A два одинаковых сообщения.
    sqlx::query(
        "UPDATE notification_deliveries
         SET status = 'cancelled', updated_at = $2
         WHERE item_id = $1 AND kind = 'assignment' AND status = 'pending'",
    )
    .bind(notice.item_id)
    .bind(now)
    .execute(&mut *conn)
    .await?;

    let Some(assignee) = notice.assignee else {
        return Ok(0);
    };
    if notice.actor.user_id == Some(assignee) {
        return Ok(0);
    }

    let occurrence_key = notice.event_id.to_string();
    let delivery = NewDelivery {
        household_id: notice.household_id,
        user_id: assignee,
        item_id: Some(notice.item_id),
        kind: NotificationKind::Assignment,
        occurrence_key: &occurrence_key,
        origin_key: None,
        scheduled_at: now,
    };
    Ok(insert(conn, &delivery, now).await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snooze_key_roundtrips_whole_seconds() {
        let at = DateTime::<Utc>::from_timestamp(1_800_000_000, 0).expect("timestamp");
        assert_eq!(snooze_key_time(&snooze_key(at)), Some(at));
    }

    #[test]
    fn snooze_key_rejects_foreign_and_malformed_values() {
        assert_eq!(snooze_key_time("reminder:1800000000"), None);
        assert_eq!(snooze_key_time("snooze:not-a-number"), None);
        assert_eq!(snooze_key_time("snooze:"), None);
        assert_eq!(snooze_key_time(""), None);
    }
}
