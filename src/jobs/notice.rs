//! Общая актуальность записи доставки.
//!
//! Одиночная доставка, отмена устаревших записей и сборка сводного сообщения
//! используют один снимок `notification_deliveries` с текущим состоянием
//! получателя, задачи и списка. Раньше у каждого пути была своя копия проверок,
//! и назначение из архивного списка уходило одиночным сообщением, но терялось
//! в сводном.

use chrono::{DateTime, Duration, NaiveDate, Utc};
use chrono_tz::Tz;
use sqlx::FromRow;
use uuid::Uuid;

use super::scheduler::{all_day_occurrence_key, occurrence_key, resolved_timezone};
use crate::domain::settings::NotificationSettings;
use crate::storage::repositories::notifications::NotificationKind;

/// Колонки записи доставки и связанных строк для [`NoticeRow`].
pub(crate) const NOTICE_ROW_COLUMNS: &str = "d.id, d.household_id, d.user_id, d.kind,
        d.occurrence_key, d.scheduled_at, d.deferred,
        i.id AS item_id, i.title AS item_title, i.status::text AS item_status,
        i.deleted_at AS item_deleted_at, i.due_at AS item_due_at,
        i.due_date AS item_due_date,
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
    pub scheduled_at: DateTime<Utc>,
    pub deferred: bool,
    pub item_id: Option<Uuid>,
    pub item_title: Option<String>,
    pub item_status: Option<String>,
    pub item_deleted_at: Option<DateTime<Utc>>,
    pub item_due_at: Option<DateTime<Utc>>,
    pub item_due_date: Option<NaiveDate>,
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
        match (self.item_due_at, self.item_due_date) {
            (Some(due_at), None) => Some(occurrence_key(due_at)),
            (None, Some(due_date)) => Some(all_day_occurrence_key(due_date)),
            _ => None,
        }
    }

    /// Задача существует, открыта и имеет название.
    fn item_alive(&self) -> bool {
        self.item_id.is_some()
            && self.item_deleted_at.is_none()
            && self.item_status.as_deref() == Some("open")
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

    /// Список задач доступен: существует, не удалён и не в архиве.
    fn list_visible(&self) -> bool {
        self.list_exists()
            && self.list_kind.as_deref() == Some("tasks")
            && self.list_archived != Some(true)
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
    /// `Snooze` не сверяется с текущим `occurrence_key`: его ключ — время
    /// отложенной доставки. Он отменяется только вместе с задачей, списком,
    /// household, получателем или выключенными напоминаниями.
    ///
    /// Назначение не зависит от напоминаний и архива списка: задача остаётся
    /// открытой и всё ещё назначена получателю. Список при этом должен
    /// существовать, иначе карточку не из чего собрать.
    pub(crate) fn is_stale(&self, now: DateTime<Utc>) -> bool {
        if !self.recipient_alive() {
            return true;
        }
        match self.kind {
            NotificationKind::Assignment => self.assignment_stale(),
            NotificationKind::Reminder | NotificationKind::Snooze => self.reminder_stale(now),
            NotificationKind::Digest => true,
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
        if self.is_snooze() {
            return false;
        }
        !self.item_valid()
            || !self.recipient_selected()
            || self.current_key().as_deref() != Some(self.occurrence_key.as_str())
            || !self.in_window(now)
    }
}
