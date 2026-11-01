//! Ключи вхождения доставки.
//!
//! Ключ отличает одно вхождение (срок, день сводки) от следующего. Он попадает
//! в проверки планирования и в snooze-время, поэтому формат фиксирован.

use chrono::{DateTime, NaiveDate, SecondsFormat, Utc};

/// Ключ вхождения: срок в UTC с микросекундной точностью.
pub fn occurrence_key(due_at: DateTime<Utc>) -> String {
    due_at.to_rfc3339_opts(SecondsFormat::Micros, true)
}

/// Ключ вхождения вседневной задачи: локальная дата срока.
pub fn all_day_occurrence_key(due_date: NaiveDate) -> String {
    format!("date:{due_date}")
}

/// Ключ вхождения утренней сводки: локальная дата получателя.
pub fn digest_occurrence_key(date: NaiveDate) -> String {
    format!("digest:{date}")
}

/// Дата из ключа `digest:YYYY-MM-DD`.
pub(crate) fn digest_key_date(key: &str) -> Option<NaiveDate> {
    key.strip_prefix("digest:")
        .and_then(|date| NaiveDate::parse_from_str(date, "%Y-%m-%d").ok())
}
