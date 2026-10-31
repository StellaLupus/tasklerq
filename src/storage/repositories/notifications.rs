//! Записи доставки уведомлений.
//!
//! Тип уведомления — Rust-enum с `sqlx::Type`: опечатка в имени PostgreSQL-типа
//! ловится компилятором, а не SQL-сервером. Уведомление о назначении ставится
//! в той же транзакции, что и смена исполнителя, поэтому доставка не зависит
//! от выбора `EventType` и не теряется при обновлении, меняющем несколько
//! полей сразу.

use chrono::{DateTime, Utc};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::models::Actor;

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
    Ok(sqlx::query(
        "INSERT INTO notification_deliveries (
             id, household_id, user_id, item_id, occurrence_key, kind,
             status, scheduled_at, created_at, updated_at
         )
         SELECT $1, $2, $3, $4, $5, $6, 'pending', $7, $8, $8
         WHERE NOT EXISTS (
             SELECT 1 FROM notification_deliveries d
             WHERE d.user_id = $3 AND d.item_id IS NOT DISTINCT FROM $4
               AND d.occurrence_key = $5 AND d.kind = $6
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
    .bind(delivery.household_id)
    .bind(delivery.user_id)
    .bind(delivery.item_id)
    .bind(delivery.occurrence_key)
    .bind(delivery.kind)
    .bind(delivery.scheduled_at)
    .bind(now)
    .execute(conn)
    .await?
    .rows_affected())
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
    let Some(assignee) = notice.assignee else {
        return Ok(0);
    };
    if notice.previous_assignee == Some(assignee) || notice.actor.user_id == Some(assignee) {
        return Ok(0);
    }

    let occurrence_key = notice.event_id.to_string();
    let delivery = NewDelivery {
        household_id: notice.household_id,
        user_id: assignee,
        item_id: Some(notice.item_id),
        kind: NotificationKind::Assignment,
        occurrence_key: &occurrence_key,
        scheduled_at: now,
    };
    Ok(insert(conn, &delivery, now).await?)
}
