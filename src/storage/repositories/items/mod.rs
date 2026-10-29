//! Репозиторий элементов, жизненный цикл, журнал и поиск.

mod find;
mod lifecycle;
mod read;
mod row;
mod write;

pub use find::{SearchHit, search};
pub use lifecycle::{complete, reopen, restore, soft_delete, undo_last};
pub use read::{calendar_items, get, list_by_list, snapshot};
pub use write::{create, update};

use chrono::{DateTime, NaiveDate, Utc};
use serde_json::json;

use crate::domain::error::DomainError;
use crate::domain::models::{Item, MonthRule, Recurrence, RecurrenceFreq};
use crate::domain::recurrence::parse_timezone;

/// Окно отмены последнего действия.
pub const UNDO_WINDOW_MINUTES: i64 = 10;

/// Срок хранения мягко удалённых элементов.
pub const DELETED_RETENTION_DAYS: i64 = 30;

type RecurrenceColumns = (
    Option<RecurrenceFreq>,
    Option<i16>,
    Option<Vec<i16>>,
    Option<NaiveDate>,
    Option<String>,
    Option<NaiveDate>,
    Option<MonthRule>,
);

fn recurrence_values(recurrence: Option<&Recurrence>) -> RecurrenceColumns {
    match recurrence {
        Some(rule) => (
            Some(rule.freq),
            Some(rule.interval),
            (!rule.byweekday.is_empty()).then(|| rule.byweekday.clone()),
            rule.until,
            Some(rule.timezone.clone()),
            Some(rule.anchor_date),
            rule.month_rule,
        ),
        None => (None, None, None, None, None, None, None),
    }
}

fn version_conflict(current: &Item) -> DomainError {
    DomainError::conflict_with(
        "Task was modified",
        json!({"current_version": current.version, "item": current}),
    )
}

fn local_today(timezone: &str, now: DateTime<Utc>) -> Result<NaiveDate, DomainError> {
    let timezone = parse_timezone(timezone)?;
    Ok(now.with_timezone(&timezone).date_naive())
}
