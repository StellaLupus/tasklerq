//! Движок повторений.
//!
//! Правило хранится на элементе. Следующее вхождение считается от текущего
//! срока и якорной даты, а не от момента завершения, чтобы серия не дрейфовала.
//! Просроченные вхождения не копятся: возвращается первое вхождение строго
//! позже и текущего срока, и момента завершения.

use std::str::FromStr;

use chrono::{
    DateTime, Datelike, Duration, LocalResult, Months, NaiveDate, NaiveDateTime, NaiveTime,
    TimeZone, Utc, Weekday,
};
use chrono_tz::Tz;

use super::error::DomainError;
use super::models::{MonthRule, Recurrence, RecurrenceFreq, RecurrenceInput};

/// Максимальная длина интервала календарного запроса, дней.
pub const MAX_CALENDAR_DAYS: i64 = 366;
/// Максимальное число вхождений в одном календарном ответе.
pub const MAX_CALENDAR_OCCURRENCES: usize = 1000;
/// Предохранитель от бесконечного перебора (например, 31-е число).
const MAX_ITERATIONS: usize = 10_000;

/// Срок элемента: вседневный или со временем.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Due {
    /// Календарная дата без времени.
    AllDay(NaiveDate),
    /// Момент в UTC.
    Timed(DateTime<Utc>),
}

impl Due {
    /// Локальная дата в часовом поясе правила.
    pub fn local_date(self, timezone: Tz) -> NaiveDate {
        match self {
            Self::AllDay(date) => date,
            Self::Timed(at) => at.with_timezone(&timezone).date_naive(),
        }
    }

    /// Локальное время для timed-срока.
    pub fn local_time(self, timezone: Tz) -> Option<NaiveTime> {
        match self {
            Self::AllDay(_) => None,
            Self::Timed(at) => Some(at.with_timezone(&timezone).time()),
        }
    }
}

impl Recurrence {
    /// Разобрать IANA timezone правила.
    pub fn timezone(&self) -> Result<Tz, DomainError> {
        parse_timezone(&self.timezone)
    }

    /// Первое вхождение не раньше `today`, если срок при создании не задан.
    pub fn first_due(&self, today: NaiveDate) -> Result<Due, DomainError> {
        self.timezone()?;
        let mut candidate = self.anchor_occurrence();
        let mut iterations = 0;

        while candidate < today {
            candidate = self
                .next_date(candidate)
                .ok_or_else(|| DomainError::Validation("recurrence has no occurrence".into()))?;
            iterations += 1;
            if iterations > MAX_ITERATIONS {
                return Err(DomainError::Validation(
                    "recurrence has no occurrence in a reasonable range".into(),
                ));
            }
        }

        if self.until.is_some_and(|until| candidate > until) {
            return Err(DomainError::Validation(
                "recurrence ends before its first occurrence".into(),
            ));
        }

        Ok(Due::AllDay(candidate))
    }

    /// Следующее вхождение после текущего срока и момента завершения.
    pub fn next_after(
        &self,
        current: Due,
        completed_at: DateTime<Utc>,
    ) -> Result<Option<Due>, DomainError> {
        let timezone = self.timezone()?;
        let current_date = current.local_date(timezone);
        let completion_date = completed_at.with_timezone(&timezone).date_naive();
        let local_time = current.local_time(timezone);
        let mut candidate_date = current_date;
        let mut iterations = 0;

        loop {
            let Some(next_date) = self.next_date(candidate_date) else {
                return Ok(None);
            };
            candidate_date = next_date;

            if self.until.is_some_and(|until| candidate_date > until) {
                return Ok(None);
            }

            let next = match local_time {
                Some(time) => Due::Timed(resolve_local(timezone, candidate_date, time)?),
                None => Due::AllDay(candidate_date),
            };

            let after_current = candidate_date > current_date;
            let after_completion = candidate_date > completion_date;
            let timed_after_completion = match next {
                Due::Timed(at) => at > completed_at,
                Due::AllDay(_) => after_completion,
            };
            let timed_after_current = match (next, current) {
                (Due::Timed(at), Due::Timed(current_at)) => at > current_at,
                _ => after_current,
            };

            if timed_after_current && timed_after_completion {
                return Ok(Some(next));
            }

            iterations += 1;
            if iterations > MAX_ITERATIONS {
                return Err(DomainError::Validation(
                    "recurrence has no occurrence in a reasonable range".into(),
                ));
            }
        }
    }

    /// Все вхождения серии в интервале `[start, end)`.
    ///
    /// `current` задаёт тип срока (all-day/timed) и локальное время. Развёртка
    /// ограничена [`MAX_CALENDAR_OCCURRENCES`].
    pub fn occurrences_between(
        &self,
        current: Due,
        start: NaiveDate,
        end: NaiveDate,
    ) -> Result<Vec<Due>, DomainError> {
        if end <= start {
            return Ok(Vec::new());
        }

        let timezone = self.timezone()?;
        let local_time = current.local_time(timezone);
        let mut candidate = self.anchor_occurrence();
        let mut iterations = 0;

        while candidate < start {
            candidate = match self.next_date(candidate) {
                Some(next) => next,
                None => return Ok(Vec::new()),
            };
            iterations += 1;
            if iterations > MAX_ITERATIONS {
                return Err(DomainError::Validation(
                    "recurrence has no occurrence in a reasonable range".into(),
                ));
            }
        }

        let mut result = Vec::new();
        while candidate < end {
            if self.until.is_some_and(|until| candidate > until) {
                break;
            }

            let occurrence = match local_time {
                Some(time) => Due::Timed(resolve_local(timezone, candidate, time)?),
                None => Due::AllDay(candidate),
            };
            result.push(occurrence);

            if result.len() > MAX_CALENDAR_OCCURRENCES {
                return Err(DomainError::Validation(format!(
                    "calendar interval produces more than {MAX_CALENDAR_OCCURRENCES} occurrences"
                )));
            }

            candidate = match self.next_date(candidate) {
                Some(next) => next,
                None => break,
            };
        }

        Ok(result)
    }

    /// Первое вхождение, определяемое якорем (до сдвига на `today`).
    fn anchor_occurrence(&self) -> NaiveDate {
        match (self.freq, self.month_rule) {
            (RecurrenceFreq::Monthly, Some(MonthRule::LastDay)) => {
                last_day_of_month(self.anchor_date.year(), self.anchor_date.month())
            }
            (RecurrenceFreq::Weekly, _) => {
                if weekday_in_set(self.anchor_date.weekday(), &self.byweekday) {
                    self.anchor_date
                } else {
                    self.next_date(self.anchor_date - Duration::days(1))
                        .unwrap_or(self.anchor_date)
                }
            }
            _ => self.anchor_date,
        }
    }

    /// Следующая дата по правилу без учёта завершения.
    fn next_date(&self, date: NaiveDate) -> Option<NaiveDate> {
        match self.freq {
            RecurrenceFreq::Daily => {
                date.checked_add_days(chrono::Days::new(u64::try_from(self.interval).unwrap_or(0)))
            }
            RecurrenceFreq::Weekly => self.next_weekly_date(date),
            RecurrenceFreq::Monthly => self.next_monthly_date(date),
            RecurrenceFreq::Yearly => self.next_yearly_date(date),
        }
    }

    fn next_weekly_date(&self, date: NaiveDate) -> Option<NaiveDate> {
        if self.byweekday.is_empty() {
            return None;
        }

        let anchor_monday = monday_of(self.anchor_date);
        let mut candidate = date.checked_add_days(chrono::Days::new(1))?;

        for _ in 0..(7 * usize::try_from(self.interval.max(1)).unwrap_or(1) + 7) {
            let candidate_monday = monday_of(candidate);
            let weeks = (candidate_monday - anchor_monday).num_days() / 7;
            let week_matches = weeks.rem_euclid(i64::from(self.interval)) == 0;
            if week_matches && weekday_in_set(candidate.weekday(), &self.byweekday) {
                return Some(candidate);
            }
            candidate = candidate.checked_add_days(chrono::Days::new(1))?;
        }

        None
    }

    fn next_monthly_date(&self, date: NaiveDate) -> Option<NaiveDate> {
        let step = i32::from(self.interval);
        let mut year = date.year();
        let mut month = date.month();

        for _ in 0..MAX_ITERATIONS {
            let (next_year, next_month) = add_months(year, month, step);
            year = next_year;
            month = next_month;

            let candidate = match self.month_rule.unwrap_or(MonthRule::AnchorDay) {
                MonthRule::LastDay => last_day_of_month(year, month),
                MonthRule::AnchorDay => {
                    let Some(candidate) =
                        NaiveDate::from_ymd_opt(year, month, self.anchor_date.day())
                    else {
                        continue;
                    };
                    candidate
                }
            };

            if candidate > date {
                return Some(candidate);
            }
        }

        None
    }

    fn next_yearly_date(&self, date: NaiveDate) -> Option<NaiveDate> {
        let step = i32::from(self.interval);
        let mut year = date.year();

        for _ in 0..MAX_ITERATIONS {
            year = year.checked_add(step)?;
            if let Some(candidate) =
                NaiveDate::from_ymd_opt(year, self.anchor_date.month(), self.anchor_date.day())
                && candidate > date
            {
                return Some(candidate);
            }
        }

        None
    }
}

impl RecurrenceInput {
    /// Проверить и нормализовать правило.
    ///
    /// `today` используется, если якорная дата не задана; `household_timezone` —
    /// если не задан часовой пояс.
    pub fn normalize(
        self,
        household_timezone: &str,
        due: Option<Due>,
        today: NaiveDate,
    ) -> Result<Recurrence, DomainError> {
        let interval = self.interval.unwrap_or(1);
        if !(1..=365).contains(&interval) {
            return Err(DomainError::Validation(
                "recurrence interval must be between 1 and 365".into(),
            ));
        }

        let timezone = self
            .timezone
            .as_deref()
            .unwrap_or(household_timezone)
            .to_owned();
        let tz = parse_timezone(&timezone)?;

        let anchor_date = self
            .anchor_date
            .or_else(|| due.map(|due| due.local_date(tz)))
            .unwrap_or(today);

        let byweekday = if self.freq == RecurrenceFreq::Weekly {
            let mut days = self.byweekday;
            days.sort_unstable();
            days.dedup();
            if days.is_empty() {
                return Err(DomainError::Validation(
                    "weekly recurrence requires at least one weekday".into(),
                ));
            }
            if days.iter().any(|day| !(1..=7).contains(day)) {
                return Err(DomainError::Validation(
                    "weekday must be between 1 and 7".into(),
                ));
            }
            days
        } else {
            if !self.byweekday.is_empty() {
                return Err(DomainError::Validation(
                    "weekdays are only allowed for weekly recurrence".into(),
                ));
            }
            Vec::new()
        };

        let month_rule = if self.freq == RecurrenceFreq::Monthly {
            Some(self.month_rule.unwrap_or(MonthRule::AnchorDay))
        } else {
            if self.month_rule.is_some() {
                return Err(DomainError::Validation(
                    "month_rule is only allowed for monthly recurrence".into(),
                ));
            }
            None
        };

        if let Some(until) = self.until {
            if until < anchor_date {
                return Err(DomainError::Validation(
                    "recurrence until must not be before anchor date".into(),
                ));
            }
            let limit = anchor_date
                .checked_add_months(Months::new(60))
                .unwrap_or(anchor_date);
            if until > limit {
                return Err(DomainError::Validation(
                    "recurrence until must be within 5 years from anchor date".into(),
                ));
            }
        }

        Ok(Recurrence {
            freq: self.freq,
            interval,
            byweekday,
            until: self.until,
            timezone,
            anchor_date,
            month_rule,
        })
    }
}

/// Разобрать IANA timezone.
pub fn parse_timezone(value: &str) -> Result<Tz, DomainError> {
    Tz::from_str(value)
        .map_err(|_| DomainError::Validation(format!("invalid IANA timezone: {value}")))
}

/// Проверить, что дата входит в набор дней недели ISO.
fn weekday_in_set(weekday: Weekday, days: &[i16]) -> bool {
    days.contains(&weekday_iso(weekday))
}

/// ISO-номер дня недели: понедельник = 1.
pub fn weekday_iso(weekday: Weekday) -> i16 {
    match weekday {
        Weekday::Mon => 1,
        Weekday::Tue => 2,
        Weekday::Wed => 3,
        Weekday::Thu => 4,
        Weekday::Fri => 5,
        Weekday::Sat => 6,
        Weekday::Sun => 7,
    }
}

/// Понедельник недели, содержащей дату.
fn monday_of(date: NaiveDate) -> NaiveDate {
    date - Duration::days(i64::from(date.weekday().num_days_from_monday()))
}

/// Последний день месяца.
pub(super) fn last_day_of_month(year: i32, month: u32) -> NaiveDate {
    let (next_year, next_month) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    NaiveDate::from_ymd_opt(next_year, next_month, 1)
        .and_then(|date| date.checked_sub_days(chrono::Days::new(1)))
        .expect("valid calendar month")
}

/// Прибавить месяцы к году/месяцу.
fn add_months(year: i32, month: u32, delta: i32) -> (i32, u32) {
    let total = year * 12 + i32::try_from(month).unwrap_or(1) - 1 + delta;
    (total.div_euclid(12), (total.rem_euclid(12) + 1) as u32)
}

/// Перевести локальные дату и время в UTC.
///
/// Несуществующее время (весенний DST) сдвигается вперёд до первого
/// существующего; неоднозначное (осенний DST) трактуется как первое вхождение.
pub(super) fn resolve_local(
    timezone: Tz,
    date: NaiveDate,
    time: NaiveTime,
) -> Result<DateTime<Utc>, DomainError> {
    let naive = NaiveDateTime::new(date, time);

    match timezone.from_local_datetime(&naive) {
        LocalResult::Single(at) => Ok(at.with_timezone(&Utc)),
        LocalResult::Ambiguous(first, _second) => Ok(first.with_timezone(&Utc)),
        LocalResult::None => {
            for minutes in 1..=180 {
                let shifted = naive + Duration::minutes(minutes);
                if let LocalResult::Single(at) = timezone.from_local_datetime(&shifted) {
                    return Ok(at.with_timezone(&Utc));
                }
            }
            Err(DomainError::Validation(
                "local time does not exist in the recurrence timezone".into(),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Timelike};

    use super::*;

    fn date(year: i32, month: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(year, month, day).expect("valid date")
    }

    fn rule(
        freq: RecurrenceFreq,
        anchor: NaiveDate,
        timezone: &str,
        month_rule: Option<MonthRule>,
    ) -> Recurrence {
        Recurrence {
            freq,
            interval: 1,
            byweekday: Vec::new(),
            until: None,
            timezone: timezone.to_owned(),
            anchor_date: anchor,
            month_rule,
        }
    }

    #[test]
    fn monthly_31_skips_short_months() {
        let rule = rule(
            RecurrenceFreq::Monthly,
            date(2026, 1, 31),
            "Europe/Moscow",
            Some(MonthRule::AnchorDay),
        );

        let next = rule
            .next_after(
                Due::AllDay(date(2026, 1, 31)),
                Utc.with_ymd_and_hms(2026, 1, 31, 12, 0, 0)
                    .single()
                    .expect("time"),
            )
            .expect("next")
            .expect("occurrence");
        assert_eq!(next, Due::AllDay(date(2026, 3, 31)));
    }

    #[test]
    fn monthly_last_day_handles_leap_february() {
        let rule = rule(
            RecurrenceFreq::Monthly,
            date(2024, 1, 15),
            "Europe/Moscow",
            Some(MonthRule::LastDay),
        );

        let occurrences = rule
            .occurrences_between(
                Due::AllDay(date(2024, 1, 31)),
                date(2024, 1, 1),
                date(2024, 5, 1),
            )
            .expect("occurrences");
        assert_eq!(
            occurrences,
            vec![
                Due::AllDay(date(2024, 1, 31)),
                Due::AllDay(date(2024, 2, 29)),
                Due::AllDay(date(2024, 3, 31)),
                Due::AllDay(date(2024, 4, 30)),
            ]
        );
    }

    #[test]
    fn yearly_february_29_skips_non_leap_years() {
        let rule = rule(
            RecurrenceFreq::Yearly,
            date(2024, 2, 29),
            "Europe/Moscow",
            None,
        );

        let next = rule
            .next_after(Due::AllDay(date(2024, 2, 29)), Utc::now())
            .expect("next")
            .expect("occurrence");
        assert_eq!(next, Due::AllDay(date(2028, 2, 29)));
    }

    #[test]
    fn weekly_uses_weekdays_and_interval_weeks() {
        let mut rule = rule(
            RecurrenceFreq::Weekly,
            date(2026, 11, 3),
            "Europe/Moscow",
            None,
        );
        rule.byweekday = vec![2, 4];

        let next = rule
            .next_after(Due::AllDay(date(2026, 11, 3)), Utc::now())
            .expect("next")
            .expect("occurrence");
        assert_eq!(next, Due::AllDay(date(2026, 11, 5)));
    }

    #[test]
    fn weekly_interval_skips_odd_weeks() {
        let mut rule = rule(
            RecurrenceFreq::Weekly,
            date(2026, 11, 3),
            "Europe/Moscow",
            None,
        );
        rule.byweekday = vec![2];
        rule.interval = 2;

        let occurrences = rule
            .occurrences_between(
                Due::AllDay(date(2026, 11, 3)),
                date(2026, 11, 1),
                date(2026, 12, 1),
            )
            .expect("occurrences");
        assert_eq!(
            occurrences,
            vec![
                Due::AllDay(date(2026, 11, 3)),
                Due::AllDay(date(2026, 11, 17))
            ]
        );
    }

    #[test]
    fn monthly_interval_uses_anchor_day() {
        let mut rule = rule(
            RecurrenceFreq::Monthly,
            date(2026, 1, 15),
            "Europe/Moscow",
            Some(MonthRule::AnchorDay),
        );
        rule.interval = 2;

        let occurrences = rule
            .occurrences_between(
                Due::AllDay(date(2026, 1, 15)),
                date(2026, 1, 1),
                date(2026, 7, 1),
            )
            .expect("occurrences");
        assert_eq!(
            occurrences,
            vec![
                Due::AllDay(date(2026, 1, 15)),
                Due::AllDay(date(2026, 3, 15)),
                Due::AllDay(date(2026, 5, 15)),
            ]
        );
    }

    #[test]
    fn daily_interval_is_respected() {
        let mut rule = rule(
            RecurrenceFreq::Daily,
            date(2026, 11, 1),
            "Europe/Moscow",
            None,
        );
        rule.interval = 3;

        let next = rule
            .next_after(Due::AllDay(date(2026, 11, 1)), Utc::now())
            .expect("next")
            .expect("occurrence");
        assert_eq!(next, Due::AllDay(date(2026, 11, 4)));
    }

    #[test]
    fn until_ends_series() {
        let mut rule = rule(
            RecurrenceFreq::Daily,
            date(2026, 11, 1),
            "Europe/Moscow",
            None,
        );
        rule.until = Some(date(2026, 11, 2));

        let next = rule
            .next_after(Due::AllDay(date(2026, 11, 2)), Utc::now())
            .expect("next");
        assert_eq!(next, None);
    }

    #[test]
    fn overdue_series_skips_missed_occurrences() {
        let rule = rule(
            RecurrenceFreq::Weekly,
            date(2026, 11, 3),
            "Europe/Moscow",
            None,
        );
        let mut weekly = rule;
        weekly.byweekday = vec![2];

        let completed_at = Utc
            .with_ymd_and_hms(2026, 11, 24, 12, 0, 0)
            .single()
            .expect("time");
        let next = weekly
            .next_after(Due::AllDay(date(2026, 11, 3)), completed_at)
            .expect("next")
            .expect("occurrence");
        assert_eq!(next, Due::AllDay(date(2026, 12, 1)));
    }

    #[test]
    fn timed_occurrence_keeps_local_wall_clock_across_dst() {
        let timezone = chrono_tz::Europe::Berlin;
        let rule = rule(
            RecurrenceFreq::Daily,
            date(2026, 3, 28),
            "Europe/Berlin",
            None,
        );
        let current = Due::Timed(
            timezone
                .with_ymd_and_hms(2026, 3, 28, 9, 0, 0)
                .single()
                .expect("time")
                .with_timezone(&Utc),
        );

        let next = rule
            .next_after(
                current,
                Utc.with_ymd_and_hms(2026, 3, 28, 8, 0, 0)
                    .single()
                    .expect("time"),
            )
            .expect("next")
            .expect("occurrence");

        let Due::Timed(at) = next else {
            panic!("expected timed occurrence");
        };
        let local = at.with_timezone(&timezone);
        assert_eq!(local.date_naive(), date(2026, 3, 29));
        assert_eq!(local.hour(), 9);
    }

    #[test]
    fn nonexistent_local_time_shifts_forward_to_first_existing_hour() {
        let timezone = chrono_tz::Europe::Berlin;
        let resolved = resolve_local(
            timezone,
            date(2026, 3, 29),
            NaiveTime::from_hms_opt(2, 30, 0).unwrap(),
        )
        .expect("resolve");
        let local = resolved.with_timezone(&timezone);
        assert_eq!(local.hour(), 3);
        assert_eq!(local.minute(), 0);
    }

    #[test]
    fn ambiguous_local_time_uses_first_occurrence() {
        let timezone = chrono_tz::Europe::Berlin;
        let resolved = resolve_local(
            timezone,
            date(2026, 10, 25),
            NaiveTime::from_hms_opt(2, 30, 0).unwrap(),
        )
        .expect("resolve");
        let local = resolved.with_timezone(&timezone);
        assert_eq!(local.format("%z").to_string(), "+0200");
    }

    #[test]
    fn first_due_without_date_uses_next_month_end() {
        let rule = rule(
            RecurrenceFreq::Monthly,
            date(2026, 10, 29),
            "Europe/Moscow",
            Some(MonthRule::LastDay),
        );

        let first = rule.first_due(date(2026, 10, 29)).expect("first");
        assert_eq!(first, Due::AllDay(date(2026, 10, 31)));
    }

    #[test]
    fn normalization_rejects_invalid_rules() {
        let weekly = RecurrenceInput {
            freq: RecurrenceFreq::Weekly,
            interval: None,
            byweekday: Vec::new(),
            until: None,
            timezone: None,
            anchor_date: None,
            month_rule: None,
        };
        assert!(
            weekly
                .normalize("Europe/Moscow", None, date(2026, 10, 29))
                .is_err()
        );

        let monthly = RecurrenceInput {
            freq: RecurrenceFreq::Monthly,
            interval: Some(0),
            byweekday: Vec::new(),
            until: None,
            timezone: None,
            anchor_date: None,
            month_rule: None,
        };
        assert!(
            monthly
                .normalize("Europe/Moscow", None, date(2026, 10, 29))
                .is_err()
        );
    }
}
