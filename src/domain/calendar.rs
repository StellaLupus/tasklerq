//! Календарные вхождения задач и событий.

use chrono::{DateTime, Duration, NaiveDate, Utc};
use chrono_tz::Tz;
use serde::Serialize;
use uuid::Uuid;

use super::error::DomainError;
use super::events::start_of_day;
use super::models::{Item, ItemStatus, ListKind};
use super::recurrence::{Due, MAX_CALENDAR_DAYS, MAX_CALENDAR_OCCURRENCES, parse_timezone};

/// Элемент, который разворачивается в календарные вхождения.
#[derive(Debug, Clone)]
pub struct CalendarItem {
    /// Строка элемента.
    pub item: Item,
    /// Тип списка элемента.
    pub kind: ListKind,
}

/// Время события календаря: дата для all-day, момент UTC для timed.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum CalendarTime {
    /// Вседневная дата.
    Date(NaiveDate),
    /// Момент времени.
    DateTime(DateTime<Utc>),
}

/// Вхождение задачи или события в календаре.
#[derive(Debug, Clone, Serialize)]
pub struct CalendarEvent {
    /// Идентификатор задачи.
    pub item_id: Uuid,
    /// Список.
    pub list_id: Uuid,
    /// Тип списка элемента.
    pub kind: ListKind,
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
    /// Ключ сортировки: локальная дата начала (вседневные раньше временных в
    /// тот же день), затем момент времени, затем детерминированный ключ
    /// вхождения. День всегда считается в часовом поясе `timezone`, иначе
    /// ночное событие попадало бы в предыдущий день по UTC.
    pub fn sort_key(&self, timezone: Tz) -> (NaiveDate, bool, Option<DateTime<Utc>>, String) {
        match &self.start {
            CalendarTime::Date(date) => (*date, false, None, self.occurrence_key.clone()),
            CalendarTime::DateTime(at) => (
                at.with_timezone(&timezone).date_naive(),
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

/// Развернуть вхождения одного элемента в интервале `[start, end)`.
///
/// У события `due_*` — начало, а конец берётся из `end_*`; длительность
/// повторяющегося события сохраняется в каждом вхождении. Чтобы вхождение,
/// начавшееся до интервала и ещё идущее в нём, попало в ответ, развёртка
/// событий запрашивается с началом, сдвинутым назад на длительность, а лишние
/// вхождения отбрасываются проверкой пересечения с интервалом.
pub fn expand_item(
    entry: &CalendarItem,
    start: NaiveDate,
    end: NaiveDate,
    household_timezone: Tz,
) -> Result<Vec<CalendarEvent>, DomainError> {
    let item = &entry.item;
    let (all_day_span, timed_span) = spans(item, entry.kind);
    let Some(recurrence) = &item.recurrence else {
        return Ok(single_event(
            entry,
            start,
            end,
            household_timezone,
            all_day_span,
            timed_span,
        ));
    };

    let timezone = parse_timezone(&recurrence.timezone)?;
    let current = match (item.due_at, item.due_date) {
        (Some(at), None) => Due::Timed(at),
        (None, Some(date)) => Due::AllDay(date),
        _ => recurrence.first_due(start)?,
    };

    let expansion_start = expansion_start(entry.kind, start, all_day_span, timed_span);
    let occurrences = recurrence.occurrences_between(current, expansion_start, end)?;

    // Перенесённое вхождение — исключение из правила. Показываем его на новом
    // сроке, а вхождения правила не позже слота и нового срока не дублируем.
    // Без слота срок стоит на своей дате правила, и обычная серия выводится
    // целиком.
    let current_date = current.local_date(timezone);
    let occurrences = if recurrence.occurrence_date.is_none()
        && recurrence
            .occurrences_between(current, current_date, current_date + Duration::days(1))?
            .contains(&current)
    {
        occurrences
    } else {
        let cutoff = recurrence
            .occurrence_date
            .map_or(current_date, |slot| current_date.max(slot));
        let mut occurrences: Vec<_> = occurrences
            .into_iter()
            .filter(|occurrence| occurrence.local_date(timezone) > cutoff)
            .collect();
        if included(
            current,
            entry.kind,
            all_day_span,
            timed_span,
            start,
            end,
            timezone,
        ) {
            occurrences.push(current);
        }
        occurrences.sort_by_key(|occurrence| match *occurrence {
            Due::AllDay(date) => (date, false, None),
            Due::Timed(at) => (at.with_timezone(&timezone).date_naive(), true, Some(at)),
        });
        occurrences
    };

    // Развёртка событий начата раньше интервала: оставляем только вхождения,
    // которые пересекаются с `[start, end)`, чтобы не отдавать прошлое.
    // Границы интервала считаются в поясе серии — том же, в котором `included`
    // проверяет перенесённое вхождение, иначе вхождение у границы терялось бы
    // при поясе серии, отличном от пояса household.
    let mut occurrences = occurrences;
    if entry.kind == ListKind::Events {
        occurrences.retain(|occurrence| {
            overlaps(
                *occurrence,
                entry.kind,
                all_day_span,
                timed_span,
                start,
                end,
                timezone,
            )
        });
    }

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
                    kind: entry.kind,
                    occurrence_key: format!("{}:{}", item.id, date),
                    start: CalendarTime::Date(date),
                    end: CalendarTime::Date(date + Duration::days(all_day_span)),
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
                    kind: entry.kind,
                    occurrence_key: format!("{}:{}", item.id, local.format("%Y-%m-%dT%H:%M:%S%:z")),
                    start: CalendarTime::DateTime(at),
                    end: CalendarTime::DateTime(at + timed_span),
                    all_day: false,
                    title: item.title.clone(),
                    status: item.status,
                });
            }
        }
    }

    Ok(events)
}

/// Длительность элемента: вседневная в днях (включительно) и со временем.
///
/// У задачи конца нет, поэтому берутся прежние значения по умолчанию: один
/// день и один час. У события без конца — те же умолчания.
fn spans(item: &Item, kind: ListKind) -> (i64, Duration) {
    if kind != ListKind::Events {
        return (1, Duration::hours(1));
    }
    let all_day = item
        .end_date
        .and_then(|end| item.due_date.map(|start| (end - start).num_days() + 1))
        .unwrap_or(1);
    let timed = item
        .end_at
        .and_then(|end| item.due_at.map(|start| end - start))
        .unwrap_or_else(|| Duration::hours(1));
    (all_day, timed)
}

/// Начало развёртки: для событий — сдвинутое назад на длительность.
fn expansion_start(
    kind: ListKind,
    start: NaiveDate,
    all_day_span: i64,
    timed_span: Duration,
) -> NaiveDate {
    if kind != ListKind::Events {
        return start;
    }
    let seconds = timed_span.num_seconds().max(0);
    let timed_days = (seconds + 86_399) / 86_400;
    start - Duration::days(all_day_span.max(timed_days))
}

/// Попадает ли вхождение в интервал `[start, end)`.
///
/// У события учитывается пересечение: вхождение могло начаться до интервала и
/// ещё идти в нём. У задачи — прежнее правило: локальная дата начала внутри
/// интервала.
fn included(
    occurrence: Due,
    kind: ListKind,
    all_day_span: i64,
    timed_span: Duration,
    start: NaiveDate,
    end: NaiveDate,
    timezone: Tz,
) -> bool {
    if kind == ListKind::Events {
        return overlaps(
            occurrence,
            kind,
            all_day_span,
            timed_span,
            start,
            end,
            timezone,
        );
    }
    let date = occurrence.local_date(timezone);
    date >= start && date < end
}

/// Пересекается ли вхождение с интервалом `[start, end)`.
///
/// Для задач проверка не нужна: развёртка уже ограничена интервалом.
fn overlaps(
    occurrence: Due,
    kind: ListKind,
    all_day_span: i64,
    timed_span: Duration,
    start: NaiveDate,
    end: NaiveDate,
    timezone: Tz,
) -> bool {
    if kind != ListKind::Events {
        return true;
    }
    match occurrence {
        Due::AllDay(date) => date < end && date + Duration::days(all_day_span) > start,
        Due::Timed(at) => {
            at < start_of_day(timezone, end) && at + timed_span > start_of_day(timezone, start)
        }
    }
}

fn single_event(
    entry: &CalendarItem,
    start: NaiveDate,
    end: NaiveDate,
    household_timezone: Tz,
    all_day_span: i64,
    timed_span: Duration,
) -> Vec<CalendarEvent> {
    let item = &entry.item;

    if let Some(date) = item.due_date {
        if included(
            Due::AllDay(date),
            entry.kind,
            all_day_span,
            timed_span,
            start,
            end,
            household_timezone,
        ) {
            return vec![CalendarEvent {
                item_id: item.id,
                list_id: item.list_id,
                kind: entry.kind,
                occurrence_key: format!("{}:{}", item.id, date),
                start: CalendarTime::Date(date),
                end: CalendarTime::Date(date + Duration::days(all_day_span)),
                all_day: true,
                title: item.title.clone(),
                status: item.status,
            }];
        }
        return Vec::new();
    }

    if let Some(at) = item.due_at
        && included(
            Due::Timed(at),
            entry.kind,
            all_day_span,
            timed_span,
            start,
            end,
            household_timezone,
        )
    {
        return vec![CalendarEvent {
            item_id: item.id,
            list_id: item.list_id,
            kind: entry.kind,
            occurrence_key: format!("{}:{}", item.id, at.format("%Y-%m-%dT%H:%M:%SZ")),
            start: CalendarTime::DateTime(at),
            end: CalendarTime::DateTime(at + timed_span),
            all_day: false,
            title: item.title.clone(),
            status: item.status,
        }];
    }

    Vec::new()
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use chrono_tz::Europe::Moscow;

    use super::*;

    fn event(title: &str, start: CalendarTime) -> CalendarEvent {
        let all_day = matches!(start, CalendarTime::Date(_));
        CalendarEvent {
            item_id: Uuid::nil(),
            list_id: Uuid::nil(),
            kind: ListKind::Tasks,
            occurrence_key: title.to_owned(),
            end: start.clone(),
            start,
            all_day,
            title: title.to_owned(),
            status: ItemStatus::Open,
        }
    }

    #[test]
    fn events_are_ordered_by_household_local_date() {
        let day = |d| NaiveDate::from_ymd_opt(2026, 10, d).unwrap();
        // 01:00 MSK 29 октября — это 22:00Z 28 октября, но для household день 29-е.
        let night = Utc.with_ymd_and_hms(2026, 10, 28, 22, 0, 0).unwrap();
        let morning = Utc.with_ymd_and_hms(2026, 10, 29, 8, 0, 0).unwrap();

        let mut events = [
            event("timed-morning-29", CalendarTime::DateTime(morning)),
            event("timed-night-29", CalendarTime::DateTime(night)),
            event("all-day-29", CalendarTime::Date(day(29))),
            event("all-day-28", CalendarTime::Date(day(28))),
        ];
        events.sort_by_key(|event| event.sort_key(Moscow));

        let titles: Vec<_> = events.iter().map(|e| e.title.as_str()).collect();
        assert_eq!(
            titles,
            [
                "all-day-28",
                "all-day-29",
                "timed-night-29",
                "timed-morning-29"
            ]
        );
    }
}
