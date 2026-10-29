//! Календарные вхождения задач.

use chrono::{DateTime, Duration, NaiveDate, Utc};
use chrono_tz::Tz;
use serde::Serialize;
use uuid::Uuid;

use super::error::DomainError;
use super::models::{Item, ItemStatus};
use super::recurrence::{Due, MAX_CALENDAR_DAYS, MAX_CALENDAR_OCCURRENCES, parse_timezone};

/// Время события календаря: дата для all-day, момент UTC для timed.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum CalendarTime {
    /// Вседневная дата.
    Date(NaiveDate),
    /// Момент времени.
    DateTime(DateTime<Utc>),
}

/// Вхождение задачи в календаре.
#[derive(Debug, Clone, Serialize)]
pub struct CalendarEvent {
    /// Идентификатор задачи.
    pub item_id: Uuid,
    /// Список.
    pub list_id: Uuid,
    /// Детерминированный ключ вхождения.
    pub occurrence_key: String,
    /// Начало.
    pub start: CalendarTime,
    /// Конец (исключающий).
    pub end: CalendarTime,
    /// Вседневное событие.
    pub all_day: bool,
    /// Название.
    pub title: String,
    /// Статус.
    pub status: ItemStatus,
}

impl CalendarEvent {
    /// Ключ сортировки: дата начала (вседневные раньше временных в тот же
    /// день), затем время, затем детерминированный ключ вхождения.
    pub fn sort_key(&self) -> (NaiveDate, bool, Option<DateTime<Utc>>, String) {
        match &self.start {
            CalendarTime::Date(date) => (*date, false, None, self.occurrence_key.clone()),
            CalendarTime::DateTime(at) => (
                at.date_naive(),
                true,
                Some(*at),
                self.occurrence_key.clone(),
            ),
        }
    }
}

/// Проверить интервал календаря.
pub fn validate_range(start: NaiveDate, end: NaiveDate) -> Result<(), DomainError> {
    if end <= start {
        return Err(DomainError::Validation(
            "calendar end must be after start".into(),
        ));
    }
    if (end - start).num_days() > MAX_CALENDAR_DAYS {
        return Err(DomainError::Validation(format!(
            "calendar interval must be at most {MAX_CALENDAR_DAYS} days"
        )));
    }
    Ok(())
}

/// Развернуть вхождения одной задачи в интервале `[start, end)`.
pub fn expand_item(
    item: &Item,
    start: NaiveDate,
    end: NaiveDate,
    household_timezone: Tz,
) -> Result<Vec<CalendarEvent>, DomainError> {
    let Some(recurrence) = &item.recurrence else {
        return Ok(single_event(item, start, end, household_timezone));
    };

    let timezone = parse_timezone(&recurrence.timezone)?;
    let current = match (item.due_at, item.due_date) {
        (Some(at), None) => Due::Timed(at),
        (None, Some(date)) => Due::AllDay(date),
        _ => recurrence.first_due(start)?,
    };

    let occurrences = recurrence.occurrences_between(current, start, end)?;
    if occurrences.len() > MAX_CALENDAR_OCCURRENCES {
        return Err(DomainError::Validation(format!(
            "calendar interval produces more than {MAX_CALENDAR_OCCURRENCES} occurrences"
        )));
    }

    let mut events = Vec::with_capacity(occurrences.len());
    for occurrence in occurrences {
        match occurrence {
            Due::AllDay(date) => {
                events.push(CalendarEvent {
                    item_id: item.id,
                    list_id: item.list_id,
                    occurrence_key: format!("{}:{}", item.id, date),
                    start: CalendarTime::Date(date),
                    end: CalendarTime::Date(date + Duration::days(1)),
                    all_day: true,
                    title: item.title.clone(),
                    status: item.status,
                });
            }
            Due::Timed(at) => {
                let local = at.with_timezone(&timezone);
                events.push(CalendarEvent {
                    item_id: item.id,
                    list_id: item.list_id,
                    occurrence_key: format!("{}:{}", item.id, local.format("%Y-%m-%dT%H:%M:%S%:z")),
                    start: CalendarTime::DateTime(at),
                    end: CalendarTime::DateTime(at + Duration::hours(1)),
                    all_day: false,
                    title: item.title.clone(),
                    status: item.status,
                });
            }
        }
    }

    Ok(events)
}

fn single_event(
    item: &Item,
    start: NaiveDate,
    end: NaiveDate,
    household_timezone: Tz,
) -> Vec<CalendarEvent> {
    if let Some(date) = item.due_date {
        if date >= start && date < end {
            return vec![CalendarEvent {
                item_id: item.id,
                list_id: item.list_id,
                occurrence_key: format!("{}:{}", item.id, date),
                start: CalendarTime::Date(date),
                end: CalendarTime::Date(date + Duration::days(1)),
                all_day: true,
                title: item.title.clone(),
                status: item.status,
            }];
        }
        return Vec::new();
    }

    if let Some(at) = item.due_at {
        let date = at.with_timezone(&household_timezone).date_naive();
        if date >= start && date < end {
            return vec![CalendarEvent {
                item_id: item.id,
                list_id: item.list_id,
                occurrence_key: format!("{}:{}", item.id, at.format("%Y-%m-%dT%H:%M:%SZ")),
                start: CalendarTime::DateTime(at),
                end: CalendarTime::DateTime(at + Duration::hours(1)),
                all_day: false,
                title: item.title.clone(),
                status: item.status,
            }];
        }
    }

    Vec::new()
}
