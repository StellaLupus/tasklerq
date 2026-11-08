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

/// Ключ текущего вхождения срока: ровно один вид срока, иначе [`None`].
pub fn item_occurrence_key(
    due_at: Option<DateTime<Utc>>,
    due_date: Option<NaiveDate>,
) -> Option<String> {
    match (due_at, due_date) {
        (Some(due_at), None) => Some(occurrence_key(due_at)),
        (None, Some(due_date)) => Some(all_day_occurrence_key(due_date)),
        _ => None,
    }
}

/// Привязка snooze к текущему вхождению срока; без срока — [`NO_DUE_ORIGIN`].
// Подключается к insert_snooze() и check старения в пункте 3.1; добавлена
// отдельным пунктом 2.2, чтобы вычислитель был один.
#[allow(dead_code)]
pub fn snooze_origin(due_at: Option<DateTime<Utc>>, due_date: Option<NaiveDate>) -> String {
    item_occurrence_key(due_at, due_date).unwrap_or_else(|| NO_DUE_ORIGIN.to_owned())
}

/// Привязка snooze задачи без срока: отдельное вхождение, отличимое от `NULL`.
#[allow(dead_code)]
pub const NO_DUE_ORIGIN: &str = "no-due";

/// Ключ вхождения утренней сводки: локальная дата получателя.
pub fn digest_occurrence_key(date: NaiveDate) -> String {
    format!("digest:{date}")
}

/// Дата из ключа `digest:YYYY-MM-DD`.
pub(crate) fn digest_key_date(key: &str) -> Option<NaiveDate> {
    key.strip_prefix("digest:")
        .and_then(|date| NaiveDate::parse_from_str(date, "%Y-%m-%d").ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn due() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 9, 12, 30, 0)
            .single()
            .expect("timed due")
    }

    fn date() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 11, 9).expect("all-day due")
    }

    #[test]
    fn item_occurrence_key_uses_the_timed_occurrence() {
        assert_eq!(
            item_occurrence_key(Some(due()), None),
            Some(occurrence_key(due()))
        );
    }

    #[test]
    fn item_occurrence_key_uses_the_all_day_occurrence() {
        assert_eq!(
            item_occurrence_key(None, Some(date())),
            Some(all_day_occurrence_key(date()))
        );
        assert_eq!(
            item_occurrence_key(None, Some(date())).as_deref(),
            Some("date:2026-11-09")
        );
    }

    #[test]
    fn item_occurrence_key_is_none_without_a_single_due() {
        assert_eq!(item_occurrence_key(None, None), None);
        assert_eq!(item_occurrence_key(Some(due()), Some(date())), None);
    }

    #[test]
    fn snooze_origin_falls_back_to_no_due() {
        assert_eq!(NO_DUE_ORIGIN, "no-due");
        assert_eq!(snooze_origin(Some(due()), None), occurrence_key(due()));
        assert_eq!(
            snooze_origin(None, Some(date())),
            all_day_occurrence_key(date())
        );
        assert_eq!(snooze_origin(None, None), NO_DUE_ORIGIN);
        assert_eq!(snooze_origin(Some(due()), Some(date())), NO_DUE_ORIGIN);
    }
}
