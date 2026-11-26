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

/// Привязка snooze задачи без срока: отдельное вхождение, отличимое от `NULL`.
pub const NO_DUE_ORIGIN: &str = "no-due";

/// Привязка snooze к текущему вхождению срока; без срока — [`NO_DUE_ORIGIN`].
pub fn snooze_origin(due_at: Option<DateTime<Utc>>, due_date: Option<NaiveDate>) -> String {
    item_occurrence_key(due_at, due_date).unwrap_or_else(|| NO_DUE_ORIGIN.to_owned())
}

/// Ключ вхождения утренней сводки: локальная дата получателя.
pub fn digest_occurrence_key(date: NaiveDate) -> String {
    format!("digest:{date}")
}

/// Дата из ключа сводки: `digest:YYYY-MM-DD` или
/// `digest:YYYY-MM-DD#manual:<unix_ms>`.
pub(crate) fn digest_key_date(key: &str) -> Option<NaiveDate> {
    let key = key.strip_prefix("digest:")?;
    let date = key.split_once('#').map_or(key, |(date, _)| date);
    NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()
}

/// Признак сводки по запросу: суффикс ключа начинается с `manual`.
pub(crate) fn digest_key_manual(key: &str) -> bool {
    key.split_once('#')
        .is_some_and(|(_, suffix)| suffix.starts_with("manual"))
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

    #[test]
    fn digest_key_date_parses_the_date_before_an_optional_suffix() {
        assert_eq!(digest_key_date("digest:2026-11-09"), Some(date()));
        assert_eq!(
            digest_key_date("digest:2026-11-09#manual:1731234567890"),
            Some(date())
        );
        assert_eq!(digest_key_date("digest:2026-11-09#other:1"), Some(date()));
    }

    #[test]
    fn digest_key_date_rejects_a_malformed_date() {
        assert_eq!(digest_key_date("digest:2026-13-09"), None);
        assert_eq!(digest_key_date("date:2026-11-09"), None);
    }

    #[test]
    fn digest_key_manual_reads_the_suffix() {
        assert!(!digest_key_manual("digest:2026-11-09"));
        assert!(digest_key_manual("digest:2026-11-09#manual:1731234567890"));
        assert!(!digest_key_manual("digest:2026-11-09#other:1"));
    }
}
