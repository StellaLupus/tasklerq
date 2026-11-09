//! Пояс получателя и тихие часы.

use chrono::{DateTime, Duration, Utc};
use chrono_tz::Tz;

use crate::domain::recurrence::{parse_timezone, resolve_local};
use crate::domain::settings::{NotificationSettings, parse_hhmm};

/// Пояс получателя: личный, иначе household, иначе UTC.
pub(crate) fn resolved_timezone(
    user_timezone: Option<&str>,
    household_timezone: Option<&str>,
) -> Tz {
    user_timezone
        .and_then(|value| parse_timezone(value).ok())
        .or_else(|| household_timezone.and_then(|value| parse_timezone(value).ok()))
        .unwrap_or(chrono_tz::UTC)
}

/// Ближайший конец тихих часов, если `instant` попадает в интервал.
///
/// Начало интервала включительно, конец исключительно. Интервал может
/// переходить через полночь: 22:00–08:00 означает «с 22:00 до полуночи и с
/// полуночи до 08:00». Время считается в поясе получателя.
pub(crate) fn quiet_end_at(
    instant: DateTime<Utc>,
    timezone: Tz,
    settings: &NotificationSettings,
) -> Option<DateTime<Utc>> {
    if !settings.quiet_hours_enabled {
        return None;
    }
    let start = match parse_hhmm(&settings.quiet_start, "quiet hours start") {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, "invalid quiet hours start; delivering without quiet hours");
            return None;
        }
    };
    let end = match parse_hhmm(&settings.quiet_end, "quiet hours end") {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, "invalid quiet hours end; delivering without quiet hours");
            return None;
        }
    };
    if start == end {
        tracing::warn!("quiet hours start equals end; delivering without quiet hours");
        return None;
    }

    let local = instant.with_timezone(&timezone);
    let time = local.time();
    let date = local.date_naive();
    let inside = if start < end {
        time >= start && time < end
    } else {
        time >= start || time < end
    };
    if !inside {
        return None;
    }

    // Конец в тот же день, кроме случая, когда мы уже после начала интервала,
    // а конец — за полночь (22:00 → 08:00 следующего дня).
    let end_date = if start < end || time < end {
        date
    } else {
        date + Duration::days(1)
    };
    match resolve_local(timezone, end_date, end) {
        Ok(end) => Some(end),
        Err(error) => {
            tracing::warn!(
                %error,
                %timezone,
                %end_date,
                "failed to resolve quiet hours end; delivering without quiet hours"
            );
            None
        }
    }
}

/// Стабильное время доставки по базовому времени и текущим тихим часам.
///
/// `base` — время, когда уведомление должно было уйти по своей природе
/// (срок минус lead time, время сводки, момент назначения). Если оно ещё
/// впереди, тихие часы применяются к нему. Если уже прошло — к текущему
/// моменту: сервис мог проснуться внутри тихих часов.
///
/// Функция идемпотентна: пока запись остаётся в том же тихом интервале,
/// повторный расчёт возвращает то же время.
pub(crate) fn desired_schedule(
    base: DateTime<Utc>,
    now: DateTime<Utc>,
    timezone: Tz,
    settings: &NotificationSettings,
) -> DateTime<Utc> {
    if base > now {
        quiet_end_at(base, timezone, settings).unwrap_or(base)
    } else {
        quiet_end_at(now, timezone, settings).unwrap_or(base)
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    #[test]
    fn quiet_end_is_shifted_forward_across_the_spring_dst_gap() {
        let timezone = chrono_tz::Europe::Berlin;
        let settings = NotificationSettings {
            quiet_start: "22:00".into(),
            quiet_end: "02:30".into(),
            ..NotificationSettings::default()
        };
        // 00:30 по Берлину 29 марта 2026 — это 23:30 UTC 28-го. В эту ночь
        // часы прыгают с 02:00 на 03:00, поэтому местного 02:30 не существует.
        let instant = Utc
            .with_ymd_and_hms(2026, 3, 28, 23, 30, 0)
            .single()
            .expect("instant");

        let end = quiet_end_at(instant, timezone, &settings).expect("inside quiet hours");

        assert_eq!(
            end,
            Utc.with_ymd_and_hms(2026, 3, 29, 1, 0, 0)
                .single()
                .expect("end")
        );
        assert_eq!(
            end.with_timezone(&timezone).format("%H:%M").to_string(),
            "03:00",
            "the nonexistent 02:30 moves forward to the first existing local time"
        );
    }
}
