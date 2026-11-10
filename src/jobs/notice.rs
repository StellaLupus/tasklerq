//! Общая актуальность записи доставки.
//!
//! Одиночная доставка, отмена устаревших записей и сборка сводного сообщения
//! используют один снимок `notification_deliveries` с текущим состоянием
//! получателя, задачи и списка. Здесь же живёт правило актуальности для всех
//! видов уведомлений: у каждого вида свой [`is_stale`](NoticeRow::is_stale).

use chrono::{DateTime, Duration, NaiveDate, Utc};
use chrono_tz::Tz;
use sqlx::FromRow;
use uuid::Uuid;

use super::keys::{digest_occurrence_key, item_occurrence_key, snooze_origin};
use super::quiet::resolved_timezone;
use crate::domain::events::is_past;
use crate::domain::models::ItemStatus;
use crate::domain::settings::NotificationSettings;
use crate::storage::repositories::notifications::NotificationKind;

/// Колонки записи доставки и связанных строк для [`NoticeRow`].
pub(crate) const NOTICE_ROW_COLUMNS: &str = "d.id, d.household_id, d.user_id, d.kind,
        d.occurrence_key, d.origin_key, d.scheduled_at, d.deferred, d.created_at,
        i.id AS item_id, i.title AS item_title, i.status AS item_status,
        i.deleted_at AS item_deleted_at, i.due_at AS item_due_at,
        i.due_date AS item_due_date, i.end_at AS item_end_at,
        i.end_date AS item_end_date,
        i.assignee_user_id AS item_assignee_user_id,
        i.created_by_user_id AS item_created_by_user_id,
        l.id AS list_id, l.kind::text AS list_kind,
        l.deleted_at AS list_deleted_at, l.is_archived AS list_archived,
        u.id AS recipient_id, u.deleted_at AS recipient_deleted_at,
        u.household_id AS recipient_household_id, u.timezone AS recipient_timezone,
        u.settings AS recipient_settings, u.telegram_user_id, u.telegram_chat_id,
        h.id AS household_found, h.deleted_at AS household_deleted_at,
        h.timezone AS household_timezone";

/// Источники [`NoticeRow`]: доставка, задача, список, получатель, household.
pub(crate) const NOTICE_ROW_FROM: &str = "FROM notification_deliveries d
     LEFT JOIN items i ON i.id = d.item_id
     LEFT JOIN lists l ON l.id = i.list_id
     LEFT JOIN users u ON u.id = d.user_id
     LEFT JOIN households h ON h.id = d.household_id";

/// Запись доставки с текущим состоянием связанных строк.
#[derive(Debug, FromRow)]
pub(crate) struct NoticeRow {
    pub id: Uuid,
    pub household_id: Uuid,
    pub user_id: Uuid,
    pub kind: NotificationKind,
    pub occurrence_key: String,
    pub origin_key: Option<String>,
    pub scheduled_at: DateTime<Utc>,
    pub deferred: bool,
    pub created_at: DateTime<Utc>,
    pub item_id: Option<Uuid>,
    pub item_title: Option<String>,
    pub item_status: Option<ItemStatus>,
    pub item_deleted_at: Option<DateTime<Utc>>,
    pub item_due_at: Option<DateTime<Utc>>,
    pub item_due_date: Option<NaiveDate>,
    pub item_end_at: Option<DateTime<Utc>>,
    pub item_end_date: Option<NaiveDate>,
    pub item_assignee_user_id: Option<Uuid>,
    pub item_created_by_user_id: Option<Uuid>,
    pub list_id: Option<Uuid>,
    pub list_kind: Option<String>,
    pub list_deleted_at: Option<DateTime<Utc>>,
    pub list_archived: Option<bool>,
    pub recipient_id: Option<Uuid>,
    pub recipient_deleted_at: Option<DateTime<Utc>>,
    pub recipient_household_id: Option<Uuid>,
    pub recipient_timezone: Option<String>,
    pub recipient_settings: Option<serde_json::Value>,
    pub telegram_user_id: Option<i64>,
    pub telegram_chat_id: Option<i64>,
    pub household_found: Option<Uuid>,
    pub household_deleted_at: Option<DateTime<Utc>>,
    pub household_timezone: Option<String>,
}

impl NoticeRow {
    /// Пояс получателя, иначе household, иначе UTC.
    pub(crate) fn timezone(&self) -> Tz {
        resolved_timezone(
            self.recipient_timezone.as_deref(),
            self.household_timezone.as_deref(),
        )
    }

    /// Настройки получателя; у живого получателя они всегда есть.
    pub(crate) fn settings(&self) -> NotificationSettings {
        self.recipient_settings
            .as_ref()
            .map(NotificationSettings::from_settings)
            .unwrap_or_default()
    }

    /// Отложенное напоминание, а не напоминание о текущем вхождении срока.
    pub(crate) fn is_snooze(&self) -> bool {
        self.kind == NotificationKind::Snooze
    }

    /// Ключ текущего вхождения элемента.
    pub(crate) fn current_key(&self) -> Option<String> {
        item_occurrence_key(self.item_due_at, self.item_due_date)
    }

    /// Задача существует, открыта и имеет название.
    fn item_alive(&self) -> bool {
        self.item_id.is_some()
            && self.item_deleted_at.is_none()
            && self.item_status == Some(ItemStatus::Open)
            && self.item_title.is_some()
    }

    /// У напоминания о сроке есть ровно один вид срока.
    pub(crate) fn item_valid(&self) -> bool {
        self.item_alive() && (self.item_due_at.is_some() ^ self.item_due_date.is_some())
    }

    /// Список существует и не удалён (архив допустим, например для назначения).
    fn list_exists(&self) -> bool {
        self.list_id.is_some() && self.list_deleted_at.is_none()
    }

    /// Список участвует в напоминаниях: задача или событие, не архив.
    fn list_visible(&self) -> bool {
        self.list_exists()
            && matches!(self.list_kind.as_deref(), Some("tasks" | "events"))
            && self.list_archived != Some(true)
    }

    /// Момент события вычисляется в поясе household: вседневные границы
    /// должны проходить одновременно для всех участников.
    fn household_timezone(&self) -> Tz {
        resolved_timezone(None, self.household_timezone.as_deref())
    }

    /// Событие уже прошло к текущему моменту.
    fn event_passed(&self, now: DateTime<Utc>) -> bool {
        self.list_kind.as_deref() == Some("events")
            && is_past(
                self.item_due_at,
                self.item_due_date,
                self.item_end_at,
                self.item_end_date,
                self.household_timezone(),
                now,
            )
    }

    /// Получатель существует, всё ещё в том же household и доступен в Telegram.
    fn recipient_alive(&self) -> bool {
        self.recipient_id.is_some()
            && self.recipient_deleted_at.is_none()
            && self.recipient_household_id == Some(self.household_id)
            && self.household_found.is_some()
            && self.household_deleted_at.is_none()
            && self.telegram_user_id.is_some()
            && self.telegram_chat_id.is_some()
    }

    /// Получатель напоминания всё ещё выбран по правилу
    /// исполнитель → автор → участники.
    fn recipient_selected(&self) -> bool {
        match self.item_assignee_user_id.or(self.item_created_by_user_id) {
            Some(selected) => selected == self.user_id,
            None => true,
        }
    }

    /// Вседневное напоминание живёт до конца локального дня срока.
    ///
    /// Перенесённое тихими часами через полночь напоминание не отменяется:
    /// оно уходит утром следующего дня, хотя локальная дата уже сменилась.
    fn in_window(&self, now: DateTime<Utc>) -> bool {
        match self.item_due_date {
            Some(due_date) => {
                let today = now.with_timezone(&self.timezone()).date_naive();
                due_date == today || (self.deferred && due_date + Duration::days(1) == today)
            }
            None => true,
        }
    }

    /// Запись больше не должна быть доставлена.
    ///
    /// Напоминание сверяется с текущим вхождением срока ([`current_key`]) и
    /// окном вседневной доставки. Напоминание о событии дополнительно
    /// устаревает, когда прошёл его эффективный конец. Snooze сверяется только
    /// со своим `origin_key`: он устаревает, когда [`snooze_origin`] по
    /// текущему сроку задачи даёт другое значение, в том числе когда привязки
    /// нет. Поэтому смена локальной даты и тихие часы snooze не отменяют.
    ///
    /// Назначение не зависит от напоминаний и архива списка: задача остаётся
    /// открытой и всё ещё назначена получателю. Список при этом должен
    /// существовать, иначе карточку не из чего собрать.
    ///
    /// [`current_key`]: NoticeRow::current_key
    pub(crate) fn is_stale(&self, now: DateTime<Utc>) -> bool {
        if !self.recipient_alive() {
            return true;
        }
        match self.kind {
            NotificationKind::Assignment => self.assignment_stale(),
            NotificationKind::Reminder | NotificationKind::Snooze => self.reminder_stale(now),
            NotificationKind::Digest => self.digest_stale(now),
        }
    }

    fn assignment_stale(&self) -> bool {
        !self.item_alive()
            || !self.list_exists()
            || !self.settings().assignment_enabled
            || self.item_assignee_user_id != Some(self.user_id)
    }

    fn reminder_stale(&self, now: DateTime<Utc>) -> bool {
        if !self.item_alive() || !self.list_visible() {
            return true;
        }
        if !self.settings().reminders_enabled {
            return true;
        }
        // Напоминание о событии, которое тихие часы вынесли за его конец,
        // бесполезно: событие уже прошло (design, решение 8).
        if self.event_passed(now) {
            return true;
        }
        if self.is_snooze() {
            return self.origin_key.as_deref()
                != Some(snooze_origin(self.item_due_at, self.item_due_date).as_str());
        }
        !self.item_valid()
            || !self.recipient_selected()
            || self.current_key().as_deref() != Some(self.occurrence_key.as_str())
            || !self.in_window(now)
    }

    /// Сводка актуальна, пока пользователь и household живы, сводка включена,
    /// а ключ соответствует текущему локальному дню получателя.
    ///
    /// Сводка, отложенная тихими часами через полночь (например, в 23:00 при
    /// тихих часах до 08:00), остаётся актуальной до утра: иначе она была бы
    /// отменена в 00:00 и потерялась, а следующая сводка пришла бы только через
    /// сутки. Утром уходит ровно одна такая сводка, и следующая планируется на
    /// новый день.
    fn digest_stale(&self, now: DateTime<Utc>) -> bool {
        if !self.settings().digest_enabled {
            return true;
        }
        let today = now.with_timezone(&self.timezone()).date_naive();
        let current = digest_occurrence_key(today);
        self.occurrence_key != current
            && !(self.deferred
                && self.occurrence_key == digest_occurrence_key(today - Duration::days(1)))
    }
}
