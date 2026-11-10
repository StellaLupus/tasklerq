//! События: эффективный конец и признак «событие прошло».
//!
//! Начало события хранится в `due_at`/`due_date`, конец — в
//! `end_at`/`end_date` (design, решения 2 и 3).

use chrono::{DateTime, Duration, NaiveDate, TimeZone, Utc};
use chrono_tz::Tz;

/// Максимальная длительность события в днях.
///
/// Тот же предел, что у интервала календаря.
pub const MAX_EVENT_DAYS: i64 = 366;

/// Момент, после которого событие считается прошедшим.
///
/// Заданный `end_at` берётся как есть, а `end_date` — последний день события
/// включительно, поэтому конец приходится на начало следующего дня в поясе
/// household. Без конца событие со временем длится час, вседневное — до конца
/// дня начала в поясе household. `None` — у элемента нет начала, и событием он
/// быть не может.
///
/// Вседневные границы считаются в поясе household, а не пользователя, чтобы
/// событие проходило для всех участников одновременно.
pub fn effective_end(
    due_at: Option<DateTime<Utc>>,
    due_date: Option<NaiveDate>,
    end_at: Option<DateTime<Utc>>,
    end_date: Option<NaiveDate>,
    household_timezone: Tz,
) -> Option<DateTime<Utc>> {
    if let Some(end) = end_at {
        return Some(end);
    }
    if let Some(last_day) = end_date {
        return last_day
            .succ_opt()
            .map(|next_day| start_of_day(household_timezone, next_day));
    }
    if let Some(start) = due_at {
        return Some(start + Duration::hours(1));
    }
    due_date.and_then(|date| {
        date.succ_opt()
            .map(|next_day| start_of_day(household_timezone, next_day))
    })
}

/// Событие прошло: эффективный конец не позже `now`.
pub fn is_past(
    due_at: Option<DateTime<Utc>>,
    due_date: Option<NaiveDate>,
    end_at: Option<DateTime<Utc>>,
    end_date: Option<NaiveDate>,
    household_timezone: Tz,
    now: DateTime<Utc>,
) -> bool {
    effective_end(due_at, due_date, end_at, end_date, household_timezone)
        .is_some_and(|end| end <= now)
}

/// Начало дня `date` в часовом поясе `timezone`.
///
/// В некоторых поясах местной полуночи не существует из-за перехода на летнее
/// время: тогда берётся первый существующий местный час.
fn start_of_day(timezone: Tz, date: NaiveDate) -> DateTime<Utc> {
    (0..24)
        .find_map(|hour| {
            date.and_hms_opt(hour, 0, 0)
                .and_then(|local| timezone.from_local_datetime(&local).earliest())
        })
        .map(|local| local.with_timezone(&Utc))
        .expect("a local date has at least one existing hour")
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use chrono_tz::{America::New_York, Asia::Vladivostok};

    use super::*;

    fn date(year: i32, month: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(year, month, day).expect("valid date")
    }

    fn utc(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(year, month, day, hour, minute, 0)
            .single()
            .expect("valid instant")
    }

    #[test]
    fn end_at_is_the_effective_end() {
        let start = utc(2026, 11, 11, 14, 0);
        let end = utc(2026, 11, 11, 15, 30);

        assert_eq!(
            effective_end(Some(start), None, Some(end), None, Vladivostok),
            Some(end)
        );
    }

    #[test]
    fn end_date_ends_at_the_next_local_midnight() {
        // Владивосток — UTC+10 без перехода на летнее время.
        let last_day = date(2026, 11, 22);

        assert_eq!(
            effective_end(
                None,
                Some(date(2026, 11, 20)),
                None,
                Some(last_day),
                Vladivostok
            ),
            Some(utc(2026, 11, 22, 14, 0)),
            "the inclusive last day ends at 2026-11-23 00:00 +10:00"
        );
    }

    #[test]
    fn timed_event_without_end_lasts_one_hour() {
        let start = utc(2026, 11, 11, 10, 0);

        assert_eq!(
            effective_end(Some(start), None, None, None, Vladivostok),
            Some(utc(2026, 11, 11, 11, 0))
        );
    }

    #[test]
    fn all_day_event_without_end_ends_after_its_day() {
        assert_eq!(
            effective_end(None, Some(date(2026, 11, 20)), None, None, Vladivostok),
            Some(utc(2026, 11, 20, 14, 0)),
            "the day ends at 2026-11-21 00:00 +10:00"
        );
    }

    #[test]
    fn all_day_end_uses_the_household_timezone_across_dst() {
        // 2026-03-08 — переход на летнее время в США, 2026-11-01 — обратно.
        assert_eq!(
            effective_end(
                None,
                Some(date(2026, 3, 7)),
                None,
                Some(date(2026, 3, 8)),
                New_York
            ),
            Some(utc(2026, 3, 9, 4, 0)),
            "after the spring transition New York is UTC-4"
        );
        assert_eq!(
            effective_end(
                None,
                Some(date(2026, 10, 31)),
                None,
                Some(date(2026, 11, 1)),
                New_York
            ),
            Some(utc(2026, 11, 2, 5, 0)),
            "after the autumn transition New York is UTC-5"
        );
    }

    #[test]
    fn event_with_end_is_past_after_its_end() {
        let start = utc(2026, 11, 11, 14, 0);
        let end = utc(2026, 11, 11, 15, 30);

        assert!(!is_past(
            Some(start),
            None,
            Some(end),
            None,
            New_York,
            utc(2026, 11, 11, 15, 29)
        ));
        assert!(is_past(
            Some(start),
            None,
            Some(end),
            None,
            New_York,
            utc(2026, 11, 11, 15, 30)
        ));
        assert!(is_past(
            Some(start),
            None,
            Some(end),
            None,
            New_York,
            utc(2026, 11, 11, 15, 31)
        ));
    }

    #[test]
    fn event_without_end_is_past_after_an_hour() {
        let start = utc(2026, 11, 11, 10, 0);

        assert!(!is_past(
            Some(start),
            None,
            None,
            None,
            New_York,
            utc(2026, 11, 11, 10, 30)
        ));
        assert!(is_past(
            Some(start),
            None,
            None,
            None,
            New_York,
            utc(2026, 11, 11, 11, 0)
        ));
    }

    #[test]
    fn all_day_event_is_past_at_the_next_local_midnight() {
        let start = date(2026, 11, 20);

        assert!(!is_past(
            None,
            Some(start),
            None,
            None,
            Vladivostok,
            utc(2026, 11, 20, 13, 59)
        ));
        assert!(is_past(
            None,
            Some(start),
            None,
            None,
            Vladivostok,
            utc(2026, 11, 20, 14, 0)
        ));
    }
}
