//! Расчёт календарной даты из выражения `capture-v4`.
//!
//! Модель не вычисляет даты: она возвращает [`DateExpr`], а дату считает эта
//! чистая функция от выражения, роли поля, названного времени, текущего момента
//! и часового пояса household. «Сегодня» и «время уже прошло» определяются в
//! поясе household, а не в UTC.
//!
//! Ошибка [`DateError`] — ошибка элемента: недопустимое выражение (31 февраля,
//! неизвестный день недели, `n` вне `0..=1` у дня недели и недели) не роняет
//! остальные элементы черновика. Ошибка не несёт пользовательских данных, чтобы
//! её можно было записать кодом.

use chrono::{
    DateTime, Datelike, NaiveDate, NaiveTime, TimeDelta, Timelike, Utc, Weekday as ChronoWeekday,
};
use chrono_tz::Tz;
use thiserror::Error;

use super::wire::{DateExpr, DateKind, DateUnit, Weekday};
use crate::domain::recurrence::last_day_of_month;

/// Роль поля даты: начало периода считается иначе, чем его конец.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DateRole {
    /// Начало или срок элемента.
    Start,
    /// Конец события.
    End,
}

/// Недопустимое выражение даты. Сообщения не содержат данных ответа.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum DateError {
    /// У `kind=weekday` не назван день недели.
    #[error("weekday is not named")]
    UnknownWeekday,
    /// У `weekday` или `week` поле `n` вне `0..=1`.
    #[error("next flag must be 0 or 1")]
    InvalidNextFlag,
    /// У смещения не названа единица.
    #[error("offset unit is not named")]
    UnknownOffsetUnit,
    /// Число месяца вне `1..=31`.
    #[error("invalid day of month")]
    InvalidDay,
    /// Месяц вне `1..=12`.
    #[error("invalid month")]
    InvalidMonth,
    /// Назван год, но не назван месяц.
    #[error("month is required with a year")]
    MissingMonth,
    /// Такой даты нет в календаре.
    #[error("no such date")]
    InvalidDate,
    /// Выражение не помещается в поддерживаемый диапазон дат.
    #[error("date is out of range")]
    OutOfRange,
}

impl DateError {
    /// Короткий код для ошибки элемента.
    pub fn code(&self) -> &'static str {
        match self {
            Self::UnknownWeekday => "unknown_weekday",
            Self::InvalidNextFlag => "invalid_next_flag",
            Self::UnknownOffsetUnit => "unknown_offset_unit",
            Self::InvalidDay => "invalid_day",
            Self::InvalidMonth => "invalid_month",
            Self::MissingMonth => "missing_month",
            Self::InvalidDate => "invalid_date",
            Self::OutOfRange => "out_of_range",
        }
    }
}

/// Рассчитать дату из выражения.
///
/// `None` возвращается только для выражения «дата не названа»: у начала без
/// времени даты нет, у конца события она задаётся отдельно (время конца без
/// даты относится ко дню начала). Если время названо, а даты нет, срок
/// ставится на сегодня, а когда это время уже прошло — на завтра.
///
/// `now` — текущий момент в UTC; «сегодня» и сравнение времени считаются в
/// `timezone` household. Секунды текущего момента не учитываются: время с
/// точностью до минуты, как его видит модель в промпте.
pub fn resolve(
    expr: &DateExpr,
    role: DateRole,
    time: Option<NaiveTime>,
    now: DateTime<Utc>,
    timezone: Tz,
) -> Result<Option<NaiveDate>, DateError> {
    let local_now = now.with_timezone(&timezone);
    let today = local_now.date_naive();
    let now_minute = minute_of_day(local_now.time());

    match expr.kind {
        DateKind::None => Ok(unnamed_date(role, time, today, now_minute)),
        DateKind::Offset => offset(today, expr).map(Some),
        DateKind::Weekday => nearest_weekday(today, expr, time, now_minute).map(Some),
        DateKind::Date => explicit_date(today, expr).map(Some),
        DateKind::Weekend => weekend(today, role).map(Some),
        DateKind::Week => week(today, role, expr.n).map(Some),
        DateKind::MonthStart => month_start(today, expr.month).map(Some),
        DateKind::MonthEnd => month_end(today, expr.month).map(Some),
    }
}

/// Дата не названа: время без даты ставится на сегодня или на завтра.
fn unnamed_date(
    role: DateRole,
    time: Option<NaiveTime>,
    today: NaiveDate,
    now_minute: u32,
) -> Option<NaiveDate> {
    if role == DateRole::End {
        return None;
    }
    let time = time?;
    if minute_of_day(time) < now_minute {
        today.checked_add_days(chrono::Days::new(1))
    } else {
        Some(today)
    }
}

/// Смещение на `n` дней, недель, месяцев или лет от сегодня.
fn offset(today: NaiveDate, expr: &DateExpr) -> Result<NaiveDate, DateError> {
    match expr.unit {
        DateUnit::Days => add_days(today, expr.n),
        DateUnit::Weeks => {
            let days = expr.n.checked_mul(7).ok_or(DateError::OutOfRange)?;
            add_days(today, days)
        }
        DateUnit::Months => add_months(today, expr.n),
        DateUnit::Years => {
            let months = expr.n.checked_mul(12).ok_or(DateError::OutOfRange)?;
            add_months(today, months)
        }
        DateUnit::NotSet => Err(DateError::UnknownOffsetUnit),
    }
}

/// Ближайший названный день недели.
///
/// `n=0` — ближайший такой день, включая сегодняшний; если названо время и
/// сегодня оно уже прошло, берётся тот же день через неделю. `n=1` — этот день
/// следующей календарной недели (неделя начинается с понедельника).
fn nearest_weekday(
    today: NaiveDate,
    expr: &DateExpr,
    time: Option<NaiveTime>,
    now_minute: u32,
) -> Result<NaiveDate, DateError> {
    let target = chrono_weekday(expr.weekday)?;
    match expr.n {
        0 => {
            let ahead =
                (target.num_days_from_monday() + 7 - today.weekday().num_days_from_monday()) % 7;
            let date = add_days(today, i64::from(ahead))?;
            let passed =
                date == today && time.is_some_and(|named| minute_of_day(named) < now_minute);
            if passed { add_days(date, 7) } else { Ok(date) }
        }
        1 => {
            let next_monday =
                add_days(today, i64::from(7 - today.weekday().num_days_from_monday()))?;
            add_days(next_monday, i64::from(target.num_days_from_monday()))
        }
        _ => Err(DateError::InvalidNextFlag),
    }
}

/// Число, месяц и год; месяц и год могут быть не названы (`0`).
fn explicit_date(today: NaiveDate, expr: &DateExpr) -> Result<NaiveDate, DateError> {
    if !(1..=31).contains(&expr.day) {
        return Err(DateError::InvalidDay);
    }
    let day = u32::try_from(expr.day).map_err(|_| DateError::InvalidDay)?;

    if expr.month == 0 {
        if expr.year != 0 {
            return Err(DateError::MissingMonth);
        }
        return nearest_day_of_month(today, day);
    }

    let month = month_number(expr.month)?;
    if expr.year == 0 {
        let candidate =
            NaiveDate::from_ymd_opt(today.year(), month, day).ok_or(DateError::InvalidDate)?;
        if candidate >= today {
            return Ok(candidate);
        }
        // В спеке «прошла в этом году → следующий год»: если такой даты в
        // следующем году нет (29 февраля), выражение недопустимо.
        return NaiveDate::from_ymd_opt(today.year() + 1, month, day).ok_or(DateError::InvalidDate);
    }

    let year = i32::try_from(expr.year).map_err(|_| DateError::OutOfRange)?;
    NaiveDate::from_ymd_opt(year, month, day).ok_or(DateError::InvalidDate)
}

/// Ближайшее такое число без месяца: в этом месяце, если ещё не прошло, иначе
/// в следующем месяце, где это число существует (31-е в ноябре — 31 декабря).
fn nearest_day_of_month(today: NaiveDate, day: u32) -> Result<NaiveDate, DateError> {
    let (mut year, mut month) = (today.year(), today.month());
    for _ in 0..=12 {
        if let Some(date) = NaiveDate::from_ymd_opt(year, month, day)
            && date >= today
        {
            return Ok(date);
        }
        (year, month) = next_month(year, month)?;
    }
    Err(DateError::InvalidDate)
}

/// Выходные: начало — ближайшая суббота, в воскресенье — сегодня; конец —
/// воскресенье тех же выходных (в воскресенье — сегодня).
fn weekend(today: NaiveDate, role: DateRole) -> Result<NaiveDate, DateError> {
    let from_monday = today.weekday().num_days_from_monday();
    let saturday = if from_monday >= 5 {
        today
    } else {
        add_days(today, i64::from(5 - from_monday))?
    };
    match role {
        DateRole::Start => Ok(saturday),
        DateRole::End if today.weekday() == ChronoWeekday::Sun => Ok(today),
        DateRole::End => add_days(saturday, 1),
    }
}

/// Неделя: начало текущей недели — сегодня, следующей — её понедельник;
/// конец — воскресенье этой недели. `n` вне `0..=1` — ошибка.
fn week(today: NaiveDate, role: DateRole, n: i64) -> Result<NaiveDate, DateError> {
    let monday = add_days(today, -i64::from(today.weekday().num_days_from_monday()))?;
    match (n, role) {
        (0, DateRole::Start) => Ok(today),
        (0, DateRole::End) => add_days(monday, 6),
        (1, DateRole::Start) => add_days(monday, 7),
        (1, DateRole::End) => add_days(monday, 13),
        _ => Err(DateError::InvalidNextFlag),
    }
}

/// Начало месяца: первое число следующего месяца или названного; названный
/// месяц, который в этом году уже начался, берётся в следующем году.
fn month_start(today: NaiveDate, month: i64) -> Result<NaiveDate, DateError> {
    if month == 0 {
        let (year, month) = next_month(today.year(), today.month())?;
        return NaiveDate::from_ymd_opt(year, month, 1).ok_or(DateError::OutOfRange);
    }

    let month = month_number(month)?;
    let candidate = NaiveDate::from_ymd_opt(today.year(), month, 1).ok_or(DateError::OutOfRange)?;
    if candidate < today {
        return NaiveDate::from_ymd_opt(today.year() + 1, month, 1).ok_or(DateError::OutOfRange);
    }
    Ok(candidate)
}

/// Конец месяца: последний день текущего или названного месяца. Названный
/// прошедший месяц не переносится на следующий год: спека говорит буквально
/// «последний день текущего или названного месяца».
fn month_end(today: NaiveDate, month: i64) -> Result<NaiveDate, DateError> {
    let month = if month == 0 {
        today.month()
    } else {
        month_number(month)?
    };
    Ok(last_day_of_month(today.year(), month))
}

/// Месяц `1..=12`.
fn month_number(month: i64) -> Result<u32, DateError> {
    if !(1..=12).contains(&month) {
        return Err(DateError::InvalidMonth);
    }
    u32::try_from(month).map_err(|_| DateError::InvalidMonth)
}

/// Первый месяц после указанного.
fn next_month(year: i32, month: u32) -> Result<(i32, u32), DateError> {
    if month == 12 {
        let year = year.checked_add(1).ok_or(DateError::OutOfRange)?;
        Ok((year, 1))
    } else {
        Ok((year, month + 1))
    }
}

/// Прибавить дни с проверкой диапазона.
fn add_days(date: NaiveDate, days: i64) -> Result<NaiveDate, DateError> {
    let delta = TimeDelta::try_days(days).ok_or(DateError::OutOfRange)?;
    date.checked_add_signed(delta).ok_or(DateError::OutOfRange)
}

/// Прибавить месяцы; число ограничивается последним днём целевого месяца
/// (31 января + месяц — 28 февраля).
fn add_months(date: NaiveDate, months: i64) -> Result<NaiveDate, DateError> {
    let index = i64::from(date.year())
        .checked_mul(12)
        .and_then(|index| index.checked_add(i64::from(date.month0())))
        .and_then(|index| index.checked_add(months))
        .ok_or(DateError::OutOfRange)?;
    let year = i32::try_from(index.div_euclid(12)).map_err(|_| DateError::OutOfRange)?;
    let month = u32::try_from(index.rem_euclid(12) + 1).map_err(|_| DateError::OutOfRange)?;
    let last = checked_last_day_of_month(year, month)?;
    let day = date.day().min(last.day());
    NaiveDate::from_ymd_opt(year, month, day).ok_or(DateError::OutOfRange)
}

/// Последний день месяца; год вне диапазона [`NaiveDate`] — ошибка.
///
/// В отличие от [`last_day_of_month`], не паникует: год приходит из поля `n`
/// выражения, которое модель может заполнить произвольно.
fn checked_last_day_of_month(year: i32, month: u32) -> Result<NaiveDate, DateError> {
    let (next_year, next_month) = next_month(year, month)?;
    NaiveDate::from_ymd_opt(next_year, next_month, 1)
        .and_then(|date| date.checked_sub_days(chrono::Days::new(1)))
        .ok_or(DateError::OutOfRange)
}

/// День недели контракта в тип `chrono`; пустое значение — ошибка.
fn chrono_weekday(weekday: Weekday) -> Result<ChronoWeekday, DateError> {
    match weekday {
        Weekday::Monday => Ok(ChronoWeekday::Mon),
        Weekday::Tuesday => Ok(ChronoWeekday::Tue),
        Weekday::Wednesday => Ok(ChronoWeekday::Wed),
        Weekday::Thursday => Ok(ChronoWeekday::Thu),
        Weekday::Friday => Ok(ChronoWeekday::Fri),
        Weekday::Saturday => Ok(ChronoWeekday::Sat),
        Weekday::Sunday => Ok(ChronoWeekday::Sun),
        Weekday::NotSet => Err(DateError::UnknownWeekday),
    }
}

/// Минуты от начала суток: секунды не учитываются.
fn minute_of_day(time: NaiveTime) -> u32 {
    time.hour() * 60 + time.minute()
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use chrono_tz::America::New_York;
    use chrono_tz::Europe::Moscow;

    use super::*;

    fn date(year: i32, month: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(year, month, day).expect("valid test date")
    }

    /// Вторник `2026-11-10` 14:32 по Москве — «сейчас» из спеки.
    fn spec_now() -> DateTime<Utc> {
        moscow(2026, 11, 10, 14, 32)
    }

    fn moscow(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
        Moscow
            .with_ymd_and_hms(year, month, day, hour, minute, 0)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn date_expr(kind: DateKind) -> DateExpr {
        DateExpr {
            kind,
            n: 0,
            unit: DateUnit::NotSet,
            weekday: Weekday::NotSet,
            day: 0,
            month: 0,
            year: 0,
        }
    }

    fn offset(n: i64, unit: DateUnit) -> DateExpr {
        DateExpr {
            n,
            unit,
            ..date_expr(DateKind::Offset)
        }
    }

    fn weekday_expr(weekday: Weekday, n: i64) -> DateExpr {
        DateExpr {
            n,
            weekday,
            ..date_expr(DateKind::Weekday)
        }
    }

    fn date_parts(day: i64, month: i64, year: i64) -> DateExpr {
        DateExpr {
            day,
            month,
            year,
            ..date_expr(DateKind::Date)
        }
    }

    fn period(kind: DateKind, n: i64, month: i64) -> DateExpr {
        DateExpr {
            n,
            month,
            ..date_expr(kind)
        }
    }

    fn resolve_start(expr: &DateExpr) -> Result<Option<NaiveDate>, DateError> {
        resolve(expr, DateRole::Start, None, spec_now(), Moscow)
    }

    fn start(expr: &DateExpr) -> NaiveDate {
        resolve_start(expr)
            .expect("expression is valid")
            .expect("start date is named")
    }

    fn end(expr: &DateExpr) -> NaiveDate {
        resolve(expr, DateRole::End, None, spec_now(), Moscow)
            .expect("expression is valid")
            .expect("end date is named")
    }

    #[test]
    fn offsets_from_today() {
        assert_eq!(start(&offset(1, DateUnit::Days)), date(2026, 11, 11));
        assert_eq!(start(&offset(2, DateUnit::Weeks)), date(2026, 11, 24));
        assert_eq!(start(&offset(0, DateUnit::Days)), date(2026, 11, 10));
        assert_eq!(start(&offset(-1, DateUnit::Days)), date(2026, 11, 9));
    }

    #[test]
    fn month_and_year_offsets_are_clamped_to_the_month_end() {
        let january = moscow(2026, 1, 31, 12, 0);
        let resolved = resolve(
            &offset(1, DateUnit::Months),
            DateRole::Start,
            None,
            january,
            Moscow,
        );
        assert_eq!(resolved, Ok(Some(date(2026, 2, 28))));

        let leap_day = moscow(2028, 2, 29, 12, 0);
        let resolved = resolve(
            &offset(1, DateUnit::Years),
            DateRole::Start,
            None,
            leap_day,
            Moscow,
        );
        assert_eq!(resolved, Ok(Some(date(2029, 2, 28))));

        let resolved = resolve(
            &offset(1, DateUnit::Months),
            DateRole::Start,
            None,
            leap_day,
            Moscow,
        );
        assert_eq!(resolved, Ok(Some(date(2028, 3, 29))));
    }

    #[test]
    fn huge_offsets_are_errors_without_panicking() {
        for expr in [
            offset(300_000, DateUnit::Years),
            offset(-300_000, DateUnit::Years),
            offset(i64::MAX / 12, DateUnit::Months),
            offset(i64::MIN / 12, DateUnit::Months),
        ] {
            assert_eq!(resolve_start(&expr), Err(DateError::OutOfRange), "{expr:?}");
        }
    }

    #[test]
    fn day_without_month_uses_the_nearest_day() {
        assert_eq!(start(&date_parts(15, 0, 0)), date(2026, 11, 15));
        assert_eq!(start(&date_parts(5, 0, 0)), date(2026, 12, 5));
        assert_eq!(start(&date_parts(10, 0, 0)), date(2026, 11, 10));

        // В ноябре 30 дней: 31-е переносится на декабрь.
        assert_eq!(start(&date_parts(31, 0, 0)), date(2026, 12, 31));

        // Апрель короче: 31-е ищется в следующем подходящем месяце.
        let april = moscow(2026, 4, 10, 9, 0);
        let resolved = resolve(&date_parts(31, 0, 0), DateRole::Start, None, april, Moscow);
        assert_eq!(resolved, Ok(Some(date(2026, 5, 31))));
    }

    #[test]
    fn date_and_month_without_year_use_the_next_year_when_passed() {
        assert_eq!(start(&date_parts(5, 3, 0)), date(2027, 3, 5));
        assert_eq!(start(&date_parts(5, 12, 0)), date(2026, 12, 5));
        assert_eq!(start(&date_parts(10, 11, 0)), date(2026, 11, 10));
        assert_eq!(start(&date_parts(1, 1, 0)), date(2027, 1, 1));
    }

    #[test]
    fn explicit_year_is_used_as_named() {
        assert_eq!(start(&date_parts(5, 3, 2027)), date(2027, 3, 5));
        assert_eq!(start(&date_parts(5, 3, 2025)), date(2025, 3, 5));
    }

    #[test]
    fn february_29_without_year_is_an_error_in_a_non_leap_year() {
        assert_eq!(
            resolve_start(&date_parts(29, 2, 0)),
            Err(DateError::InvalidDate)
        );

        let january_2028 = moscow(2028, 1, 10, 12, 0);
        let resolved = resolve(
            &date_parts(29, 2, 0),
            DateRole::Start,
            None,
            january_2028,
            Moscow,
        );
        assert_eq!(resolved, Ok(Some(date(2028, 2, 29))));
    }

    #[test]
    fn weekday_uses_the_nearest_day_including_today() {
        assert_eq!(start(&weekday_expr(Weekday::Friday, 0)), date(2026, 11, 13));
        assert_eq!(
            start(&weekday_expr(Weekday::Tuesday, 0)),
            date(2026, 11, 10)
        );
    }

    #[test]
    fn today_is_moved_a_week_ahead_when_the_named_time_has_passed() {
        let tuesday = weekday_expr(Weekday::Tuesday, 0);
        let time = NaiveTime::from_hms_opt(10, 0, 0).expect("valid time");
        assert_eq!(
            resolve(&tuesday, DateRole::Start, Some(time), spec_now(), Moscow),
            Ok(Some(date(2026, 11, 17)))
        );

        let time = NaiveTime::from_hms_opt(18, 0, 0).expect("valid time");
        assert_eq!(
            resolve(&tuesday, DateRole::Start, Some(time), spec_now(), Moscow),
            Ok(Some(date(2026, 11, 10)))
        );

        // Время, равное текущему, ещё не прошло.
        let time = NaiveTime::from_hms_opt(14, 32, 0).expect("valid time");
        assert_eq!(
            resolve(&tuesday, DateRole::Start, Some(time), spec_now(), Moscow),
            Ok(Some(date(2026, 11, 10)))
        );
    }

    #[test]
    fn sunday_with_a_passed_time_moves_to_the_next_sunday() {
        let sunday = moscow(2026, 11, 15, 9, 0);
        let expr = weekday_expr(Weekday::Sunday, 0);

        assert_eq!(
            resolve(&expr, DateRole::Start, None, sunday, Moscow),
            Ok(Some(date(2026, 11, 15)))
        );
        let time = NaiveTime::from_hms_opt(8, 0, 0).expect("valid time");
        assert_eq!(
            resolve(&expr, DateRole::Start, Some(time), sunday, Moscow),
            Ok(Some(date(2026, 11, 22)))
        );
    }

    #[test]
    fn next_weekday_is_in_the_next_calendar_week() {
        assert_eq!(
            start(&weekday_expr(Weekday::Wednesday, 1)),
            date(2026, 11, 18)
        );
        assert_eq!(start(&weekday_expr(Weekday::Monday, 1)), date(2026, 11, 16));
        assert_eq!(
            start(&weekday_expr(Weekday::Tuesday, 1)),
            date(2026, 11, 17)
        );
        assert_eq!(start(&weekday_expr(Weekday::Sunday, 1)), date(2026, 11, 22));

        // В воскресенье следующая календарная неделя начинается завтра.
        let sunday = moscow(2026, 11, 15, 9, 0);
        assert_eq!(
            resolve(
                &weekday_expr(Weekday::Monday, 1),
                DateRole::Start,
                None,
                sunday,
                Moscow
            ),
            Ok(Some(date(2026, 11, 16)))
        );
    }

    #[test]
    fn weekday_rejects_unknown_day_and_other_next_flags() {
        assert_eq!(
            resolve_start(&weekday_expr(Weekday::NotSet, 0)),
            Err(DateError::UnknownWeekday)
        );
        for n in [2, -1, 7] {
            assert_eq!(
                resolve_start(&weekday_expr(Weekday::Friday, n)),
                Err(DateError::InvalidNextFlag),
                "{n}"
            );
        }
    }

    #[test]
    fn weekend_start_and_end() {
        assert_eq!(start(&date_expr(DateKind::Weekend)), date(2026, 11, 14));
        assert_eq!(end(&date_expr(DateKind::Weekend)), date(2026, 11, 15));

        let saturday = moscow(2026, 11, 14, 10, 0);
        assert_eq!(
            resolve(
                &date_expr(DateKind::Weekend),
                DateRole::Start,
                None,
                saturday,
                Moscow
            ),
            Ok(Some(date(2026, 11, 14)))
        );
        assert_eq!(
            resolve(
                &date_expr(DateKind::Weekend),
                DateRole::End,
                None,
                saturday,
                Moscow
            ),
            Ok(Some(date(2026, 11, 15)))
        );

        let sunday = moscow(2026, 11, 15, 10, 0);
        assert_eq!(
            resolve(
                &date_expr(DateKind::Weekend),
                DateRole::Start,
                None,
                sunday,
                Moscow
            ),
            Ok(Some(date(2026, 11, 15)))
        );
        assert_eq!(
            resolve(
                &date_expr(DateKind::Weekend),
                DateRole::End,
                None,
                sunday,
                Moscow
            ),
            Ok(Some(date(2026, 11, 15)))
        );
    }

    #[test]
    fn week_start_and_end() {
        assert_eq!(start(&period(DateKind::Week, 0, 0)), date(2026, 11, 10));
        assert_eq!(end(&period(DateKind::Week, 0, 0)), date(2026, 11, 15));
        assert_eq!(start(&period(DateKind::Week, 1, 0)), date(2026, 11, 16));
        assert_eq!(end(&period(DateKind::Week, 1, 0)), date(2026, 11, 22));

        // Сценарий «Всю следующую неделю»: начало и конец с n=1.
        assert_eq!(
            (
                start(&period(DateKind::Week, 1, 0)),
                end(&period(DateKind::Week, 1, 0))
            ),
            (date(2026, 11, 16), date(2026, 11, 22))
        );
    }

    #[test]
    fn week_rejects_other_next_flags() {
        for n in [2, -1] {
            assert_eq!(
                resolve_start(&period(DateKind::Week, n, 0)),
                Err(DateError::InvalidNextFlag),
                "{n}"
            );
        }
    }

    #[test]
    fn month_start_and_end() {
        assert_eq!(
            start(&period(DateKind::MonthStart, 0, 0)),
            date(2026, 12, 1)
        );
        assert_eq!(end(&period(DateKind::MonthStart, 0, 0)), date(2026, 12, 1));
        assert_eq!(start(&period(DateKind::MonthEnd, 0, 0)), date(2026, 11, 30));

        assert_eq!(
            start(&period(DateKind::MonthStart, 0, 12)),
            date(2026, 12, 1)
        );
        assert_eq!(start(&period(DateKind::MonthStart, 0, 1)), date(2027, 1, 1));
        assert_eq!(
            start(&period(DateKind::MonthStart, 0, 11)),
            date(2027, 11, 1)
        );
        assert_eq!(
            start(&period(DateKind::MonthEnd, 0, 12)),
            date(2026, 12, 31)
        );
        assert_eq!(start(&period(DateKind::MonthEnd, 0, 2)), date(2026, 2, 28));
    }

    #[test]
    fn month_end_with_a_passed_named_month_stays_in_this_year() {
        // Спека: «последний день текущего или названного месяца». В отличие от
        // `month_start`, прошедший названный месяц не переносится на следующий
        // год — это буквальное правило спеки, зафиксированное тестом.
        assert_eq!(start(&period(DateKind::MonthEnd, 0, 3)), date(2026, 3, 31));
        assert_eq!(
            start(&period(DateKind::MonthEnd, 0, 10)),
            date(2026, 10, 31)
        );
    }

    #[test]
    fn month_start_rolls_over_the_year_in_december() {
        let december = moscow(2026, 12, 15, 12, 0);
        assert_eq!(
            resolve(
                &period(DateKind::MonthStart, 0, 0),
                DateRole::Start,
                None,
                december,
                Moscow
            ),
            Ok(Some(date(2027, 1, 1)))
        );
        assert_eq!(
            resolve(
                &period(DateKind::MonthEnd, 0, 0),
                DateRole::Start,
                None,
                december,
                Moscow
            ),
            Ok(Some(date(2026, 12, 31)))
        );
    }

    #[test]
    fn time_without_date_uses_today_or_tomorrow() {
        let none = date_expr(DateKind::None);
        let evening = NaiveTime::from_hms_opt(18, 0, 0).expect("valid time");
        assert_eq!(
            resolve(&none, DateRole::Start, Some(evening), spec_now(), Moscow),
            Ok(Some(date(2026, 11, 10)))
        );

        let morning = NaiveTime::from_hms_opt(9, 0, 0).expect("valid time");
        assert_eq!(
            resolve(&none, DateRole::Start, Some(morning), spec_now(), Moscow),
            Ok(Some(date(2026, 11, 11)))
        );

        assert_eq!(
            resolve(&none, DateRole::Start, None, spec_now(), Moscow),
            Ok(None)
        );
        // У конца события время без даты относится ко дню начала.
        assert_eq!(
            resolve(&none, DateRole::End, Some(morning), spec_now(), Moscow),
            Ok(None)
        );
    }

    #[test]
    fn year_rollover_for_offsets_and_the_next_week() {
        let end_of_december = moscow(2026, 12, 30, 12, 0);
        assert_eq!(
            resolve(
                &offset(1, DateUnit::Weeks),
                DateRole::Start,
                None,
                end_of_december,
                Moscow
            ),
            Ok(Some(date(2027, 1, 6)))
        );
        assert_eq!(
            resolve(
                &period(DateKind::Week, 1, 0),
                DateRole::Start,
                None,
                end_of_december,
                Moscow
            ),
            Ok(Some(date(2027, 1, 4)))
        );
        assert_eq!(
            resolve(
                &period(DateKind::Week, 1, 0),
                DateRole::End,
                None,
                end_of_december,
                Moscow
            ),
            Ok(Some(date(2027, 1, 10)))
        );
    }

    #[test]
    fn household_timezone_decides_today() {
        // 21:30 UTC — уже 00:30 следующего дня по Москве.
        let moscow_midnight = Utc.with_ymd_and_hms(2026, 11, 10, 21, 30, 0).unwrap();
        assert_eq!(
            resolve(
                &offset(1, DateUnit::Days),
                DateRole::Start,
                None,
                moscow_midnight,
                Moscow
            ),
            Ok(Some(date(2026, 11, 12)))
        );
        assert_eq!(
            resolve(
                &weekday_expr(Weekday::Tuesday, 0),
                DateRole::Start,
                None,
                moscow_midnight,
                Moscow
            ),
            Ok(Some(date(2026, 11, 17)))
        );

        // 03:30 UTC 11 ноября — ещё 22:30 10 ноября в Нью-Йорке.
        let new_york_evening = Utc.with_ymd_and_hms(2026, 11, 11, 3, 30, 0).unwrap();
        assert_eq!(
            resolve(
                &offset(1, DateUnit::Days),
                DateRole::Start,
                None,
                new_york_evening,
                New_York
            ),
            Ok(Some(date(2026, 11, 11)))
        );
    }

    #[test]
    fn time_without_date_near_midnight_in_household_timezone() {
        // 21:30 UTC — 00:30 11 ноября по Москве.
        let after_midnight = Utc.with_ymd_and_hms(2026, 11, 10, 21, 30, 0).unwrap();
        let none = date_expr(DateKind::None);

        let passed = NaiveTime::from_hms_opt(0, 15, 0).expect("valid time");
        assert_eq!(
            resolve(&none, DateRole::Start, Some(passed), after_midnight, Moscow),
            Ok(Some(date(2026, 11, 12)))
        );

        let ahead = NaiveTime::from_hms_opt(0, 45, 0).expect("valid time");
        assert_eq!(
            resolve(&none, DateRole::Start, Some(ahead), after_midnight, Moscow),
            Ok(Some(date(2026, 11, 11)))
        );
    }

    #[test]
    fn impossible_dates_are_errors() {
        assert_eq!(
            resolve_start(&date_parts(31, 2, 0)),
            Err(DateError::InvalidDate)
        );
        assert_eq!(
            resolve_start(&date_parts(31, 2, 2027)),
            Err(DateError::InvalidDate)
        );
        assert_eq!(
            resolve_start(&date_parts(30, 2, 2028)),
            Err(DateError::InvalidDate)
        );
    }

    #[test]
    fn invalid_parts_are_errors() {
        for day in [0, 32, -1] {
            assert_eq!(
                resolve_start(&date_parts(day, 0, 0)),
                Err(DateError::InvalidDay),
                "{day}"
            );
        }
        for month in [13, -1] {
            assert_eq!(
                resolve_start(&date_parts(1, month, 0)),
                Err(DateError::InvalidMonth),
                "{month}"
            );
            assert_eq!(
                resolve_start(&period(DateKind::MonthStart, 0, month)),
                Err(DateError::InvalidMonth),
                "{month}"
            );
            assert_eq!(
                resolve_start(&period(DateKind::MonthEnd, 0, month)),
                Err(DateError::InvalidMonth),
                "{month}"
            );
        }
        assert_eq!(
            resolve_start(&date_parts(1, 0, 2026)),
            Err(DateError::MissingMonth)
        );
        assert_eq!(
            resolve_start(&offset(1, DateUnit::NotSet)),
            Err(DateError::UnknownOffsetUnit)
        );
    }

    #[test]
    fn error_codes_are_stable() {
        let cases = [
            (DateError::UnknownWeekday, "unknown_weekday"),
            (DateError::InvalidNextFlag, "invalid_next_flag"),
            (DateError::UnknownOffsetUnit, "unknown_offset_unit"),
            (DateError::InvalidDay, "invalid_day"),
            (DateError::InvalidMonth, "invalid_month"),
            (DateError::MissingMonth, "missing_month"),
            (DateError::InvalidDate, "invalid_date"),
            (DateError::OutOfRange, "out_of_range"),
        ];
        for (error, code) in cases {
            assert_eq!(error.code(), code);
        }
    }
}
