//! Преобразование элемента `capture-v4` в [`RawItem`].
//!
//! Это шаг «wire → `RawItem`» из design, решение 5: выражения дат считаются
//! здесь, а не моделью, строковые даты остаются в черновике, а валидатор
//! проверяет уже готовые календарные значения. Ошибка выражения — ошибка
//! элемента, а не разбора: элемент остаётся в черновике с [`RawDateError`],
//! остальные элементы не затрагиваются.
//!
//! Правило времени конца без даты берётся из домена событий
//! ([`crate::domain::events::end_day`]), а первое вхождение повторения считает
//! [`crate::domain::recurrence::Recurrence::first_due`].

use chrono::{Datelike, NaiveDate, NaiveTime};

use super::context::CaptureContext;
use super::dates::{self, DateRole};
use super::schema::{RawDateError, RawItem, RawRecurrence};
use super::validate::parse_time;
use super::wire::{DateExpr, EventItem, Item, Repeat, ShoppingItem, TaskItem};
use crate::domain::events::end_day;
use crate::domain::models::{ListKind, MonthRule, RecurrenceFreq, RecurrenceInput};
use crate::domain::recurrence::Due;

/// Преобразовать элемент ответа в элемент черновика.
pub fn convert_item(item: &Item, context: &CaptureContext) -> RawItem {
    match item {
        Item::Task(item) => convert_task(item, context),
        Item::Shopping(item) => convert_shopping(item, ListKind::Shopping, context),
        Item::Checklist(item) => convert_shopping(item, ListKind::Checklist, context),
        Item::Event(item) => convert_event(item, context),
    }
}

/// Срок элемента: дата, названное время и ошибка выражения.
struct Start {
    date: Option<NaiveDate>,
    time: Option<NaiveTime>,
    raw_time: Option<String>,
    error: Option<RawDateError>,
}

/// Посчитать срок элемента.
///
/// Время передаётся в расчёт только у начала: правило «время уже прошло»
/// относится к сроку, а не к концу события.
fn resolve_start(expr: &DateExpr, time: Option<&str>, context: &CaptureContext) -> Start {
    let parsed = time.and_then(parse_time);
    match dates::resolve(expr, DateRole::Start, parsed, context.now, context.timezone) {
        Ok(date) => Start {
            date,
            time: parsed,
            raw_time: time.map(ToOwned::to_owned),
            error: None,
        },
        Err(error) => {
            tracing::debug!(
                code = error.code(),
                field = "due_date",
                "date expression rejected"
            );
            Start {
                date: None,
                time: parsed,
                raw_time: time.map(ToOwned::to_owned),
                error: Some(date_error("due_date")),
            }
        }
    }
}

fn convert_task(item: &TaskItem, context: &CaptureContext) -> RawItem {
    let mut start = resolve_start(&item.date, item.time.as_deref(), context);
    let (repeat, repeat_error) = convert_repeat(item.repeat.as_ref(), &mut start, context);
    let (second_date, note_error) = convert_note_date(&item.note_date, context);
    let mut error = Vec::new();
    error.extend(start.error.take());
    error.extend(repeat_error);
    error.extend(note_error);

    RawItem {
        target_kind: ListKind::Tasks,
        target_list: item.list.clone(),
        title: item.title.clone(),
        notes: append_note_date(item.notes.clone(), second_date),
        due_date: start.date.map(format_date),
        due_time: start.raw_time,
        all_day: false,
        end_date: None,
        end_time: None,
        date_error: error,
        priority: item.priority.map(priority_name).map(ToOwned::to_owned),
        assignee: item.assignee.clone(),
        assignee_id: None,
        tags: item.tags.clone(),
        quantity: None,
        unit: None,
        subtasks: item.subtasks.clone(),
        recurrence: repeat,
    }
}

fn convert_shopping(item: &ShoppingItem, kind: ListKind, context: &CaptureContext) -> RawItem {
    let start = resolve_start(&item.date, item.time.as_deref(), context);

    RawItem {
        target_kind: kind,
        target_list: item.list.clone(),
        title: item.title.clone(),
        notes: item.notes.clone(),
        due_date: start.date.map(format_date),
        due_time: start.raw_time,
        all_day: false,
        end_date: None,
        end_time: None,
        date_error: start.error.into_iter().collect(),
        priority: item.priority.map(priority_name).map(ToOwned::to_owned),
        assignee: item.assignee.clone(),
        assignee_id: None,
        tags: item.tags.clone(),
        quantity: item.quantity,
        unit: item.unit.clone(),
        subtasks: Vec::new(),
        recurrence: None,
    }
}

fn convert_event(item: &EventItem, context: &CaptureContext) -> RawItem {
    let mut start = resolve_start(&item.date, item.time.as_deref(), context);
    let (repeat, repeat_error) = convert_repeat(item.repeat.as_ref(), &mut start, context);
    let (end_date, end_time, end_error) = convert_end(item, &start, context);
    let mut error = Vec::new();
    error.extend(start.error.take());
    error.extend(repeat_error);
    error.extend(end_error);

    RawItem {
        target_kind: ListKind::Events,
        target_list: item.list.clone(),
        title: item.title.clone(),
        notes: item.notes.clone(),
        due_date: start.date.map(format_date),
        due_time: start.raw_time,
        all_day: false,
        end_date,
        end_time,
        date_error: error,
        priority: None,
        assignee: item.assignee.clone(),
        assignee_id: None,
        tags: item.tags.clone(),
        quantity: None,
        unit: None,
        subtasks: Vec::new(),
        recurrence: repeat,
    }
}

/// Конец события: дата, время конца и ошибка выражения.
fn convert_end(
    item: &EventItem,
    start: &Start,
    context: &CaptureContext,
) -> (Option<String>, Option<String>, Option<RawDateError>) {
    let end_time = item.end_time.as_deref().and_then(parse_time);
    let raw_end_time = item.end_time.clone();

    // Время конца в расчёт даты не передаётся: «время уже прошло» относится к
    // началу, иначе день недели конца сдвинулся бы на неделю вперёд.
    match dates::resolve(
        &item.end,
        DateRole::End,
        None,
        context.now,
        context.timezone,
    ) {
        Err(error) => {
            tracing::debug!(
                code = error.code(),
                field = "end_date",
                "date expression rejected"
            );
            (None, raw_end_time, Some(date_error("end_date")))
        }
        Ok(Some(date)) => (Some(format_date(date)), raw_end_time, None),
        Ok(None) => match (start.date, end_time) {
            // Время без даты конца относится ко дню начала, а если оно не позже
            // времени начала — к следующему дню.
            (Some(start_date), Some(end_time)) => (
                Some(format_date(end_day(start_date, start.time, end_time))),
                raw_end_time,
                None,
            ),
            _ => (None, raw_end_time, None),
        },
    }
}

/// Повторение и первое вхождение серии.
///
/// `start` дополняется датой первого вхождения, если дата не названа (или
/// названа несовместимо с правилом). Названная дата сохраняется при ошибке
/// правила: её покажет валидатор как ошибку повторения.
fn convert_repeat(
    repeat: Option<&Repeat>,
    start: &mut Start,
    context: &CaptureContext,
) -> (Option<RawRecurrence>, Option<RawDateError>) {
    let Some(repeat) = repeat else {
        return (None, None);
    };

    // «Каждую неделю» и «каждые две недели» без названного дня недели модель
    // пишет как `weekly` с интервалом. Схема требует для `weekly` дни недели,
    // но смысл однозначен: N недель — это 7N дней. Приводим к `daily`, иначе
    // валидатор отвергает верный по смыслу ответ и спрашивает день недели,
    // которого в тексте нет.
    let (freq, interval) = normalize_freq(&repeat.freq, repeat.interval, &repeat.weekdays);

    let (until, until_error) = match dates::resolve(
        &repeat.until,
        // Дата окончания серии включается в неё, поэтому у выражений
        // «выходные» и «неделя» нужен конец периода, а не его начало.
        DateRole::End,
        None,
        context.now,
        context.timezone,
    ) {
        Ok(date) => (date, None),
        Err(error) => {
            tracing::debug!(
                code = error.code(),
                field = "recurrence",
                "date expression rejected"
            );
            (None, Some(date_error("recurrence")))
        }
    };

    if start.error.is_none() {
        start.date =
            first_occurrence(repeat, freq, interval, until, start.date, context).or(start.date);
    }

    let raw = RawRecurrence {
        freq: Some(freq_name(freq).to_owned()),
        interval: Some(interval),
        month_rule: repeat
            .month_rule
            .map(month_rule_name)
            .map(ToOwned::to_owned),
        weekdays: repeat.weekdays.clone(),
        until: until.map(format_date),
    };
    (Some(raw), until_error)
}

/// Частота и интервал в канонической форме схемы.
///
/// `weekly` без дней недели однозначно означает «раз в N недель», а схема
/// такое выражает только как `daily` с интервалом в днях.
fn normalize_freq(freq: &RecurrenceFreq, interval: i64, weekdays: &[i64]) -> (RecurrenceFreq, i64) {
    if *freq == RecurrenceFreq::Weekly && weekdays.is_empty() {
        (
            RecurrenceFreq::Daily,
            interval.saturating_mul(7).clamp(1, 365),
        )
    } else {
        (*freq, interval)
    }
}

/// Первое вхождение правила не раньше названной даты, а без неё — от сегодня.
///
/// Названная дата используется, только если совместима с правилом; иначе
/// берётся ближайшее вхождение по правилу. `None` — правило не удалось
/// разобрать: названную дату в этом случае оставляет вызывающий код, а ошибку
/// покажет валидатор.
fn first_occurrence(
    repeat: &Repeat,
    freq: RecurrenceFreq,
    interval: i64,
    until: Option<NaiveDate>,
    named: Option<NaiveDate>,
    context: &CaptureContext,
) -> Option<NaiveDate> {
    // Дни недели и правило месяца имеют смысл только для своей частоты; у
    // остальных частот их заполняет модель по привычке, и это не ошибка —
    // так же их отбрасывает валидатор.
    let weekly = freq == RecurrenceFreq::Weekly;
    let input = RecurrenceInput {
        freq,
        interval: i16::try_from(interval).ok(),
        byweekday: if weekly {
            repeat
                .weekdays
                .iter()
                .filter_map(|day| i16::try_from(*day).ok())
                .collect()
        } else {
            Vec::new()
        },
        until,
        timezone: None,
        anchor_date: named,
        month_rule: if freq == RecurrenceFreq::Monthly {
            repeat.month_rule
        } else {
            None
        },
    };
    let rule = input
        .normalize(context.timezone.name(), None, context.today())
        .ok()?;
    match rule.first_due(named.unwrap_or_else(|| context.today())) {
        Ok(Due::AllDay(date)) => Some(date),
        Ok(Due::Timed(_)) | Err(_) => None,
    }
}

/// Вторая дата фразы в том виде, в каком [`convert_item`] дописывает её в
/// заметку.
///
/// Нужна оценке (`eval.rs`): сравнение идёт по дате, а свободный текст заметки
/// модели не сравнивается. `None` — второй даты нет или элемент её не несёт.
pub fn second_date_text(item: &Item, context: &CaptureContext) -> Option<String> {
    match item {
        Item::Task(task) => convert_note_date(&task.note_date, context).0,
        _ => None,
    }
}

/// Вторая дата фразы в виде «20 ноября» (с годом, если он не текущий).
fn convert_note_date(
    expr: &DateExpr,
    context: &CaptureContext,
) -> (Option<String>, Option<RawDateError>) {
    match dates::resolve(expr, DateRole::Start, None, context.now, context.timezone) {
        Ok(Some(date)) => (Some(note_date_text(date, context.today().year())), None),
        Ok(None) => (None, None),
        Err(error) => {
            tracing::debug!(
                code = error.code(),
                field = "note_date",
                "date expression rejected"
            );
            (None, Some(date_error("note_date")))
        }
    }
}

/// Дописать вторую дату к заметке через пробел.
fn append_note_date(notes: Option<String>, second_date: Option<String>) -> Option<String> {
    match (notes, second_date) {
        (Some(notes), Some(date)) => {
            let notes = notes.trim_end();
            if notes.is_empty() {
                Some(date)
            } else {
                Some(format!("{notes} {date}"))
            }
        }
        (Some(notes), None) => Some(notes),
        (None, Some(date)) => Some(date),
        (None, None) => None,
    }
}

/// Читаемая дата: «20 ноября», а в другом году — «5 марта 2027».
fn note_date_text(date: NaiveDate, current_year: i32) -> String {
    let month = match date.month() {
        1 => "января",
        2 => "февраля",
        3 => "марта",
        4 => "апреля",
        5 => "мая",
        6 => "июня",
        7 => "июля",
        8 => "августа",
        9 => "сентября",
        10 => "октября",
        11 => "ноября",
        _ => "декабря",
    };
    if date.year() == current_year {
        format!("{} {month}", date.day())
    } else {
        format!("{} {month} {}", date.day(), date.year())
    }
}

fn format_date(date: NaiveDate) -> String {
    date.format("%Y-%m-%d").to_string()
}

fn date_error(field: &'static str) -> RawDateError {
    RawDateError {
        field: field.to_owned(),
    }
}

fn freq_name(freq: RecurrenceFreq) -> &'static str {
    match freq {
        RecurrenceFreq::Daily => "daily",
        RecurrenceFreq::Weekly => "weekly",
        RecurrenceFreq::Monthly => "monthly",
        RecurrenceFreq::Yearly => "yearly",
    }
}

fn month_rule_name(rule: MonthRule) -> &'static str {
    match rule {
        MonthRule::AnchorDay => "anchor_day",
        MonthRule::LastDay => "last_day",
    }
}

fn priority_name(priority: crate::domain::models::ItemPriority) -> &'static str {
    use crate::domain::models::ItemPriority;
    match priority {
        ItemPriority::Low => "low",
        ItemPriority::Normal => "normal",
        ItemPriority::High => "high",
        ItemPriority::Urgent => "urgent",
    }
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, TimeZone, Utc};
    use chrono_tz::Europe::Moscow;
    use serde_json::{Value, json};

    use super::*;
    use crate::capture::test_support::{context, list};
    use crate::capture::validate::{
        DATE_INVALID, DATE_PAST, DraftItem, END_BEFORE_START, validate_item,
    };
    use crate::capture::wire::parse_capture_response;
    use crate::domain::models::ItemSource;

    /// Вторник `2026-11-10` 14:32 по Москве — «сейчас» из спеки.
    fn moscow(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
        Moscow
            .with_ymd_and_hms(year, month, day, hour, minute, 0)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn spec_context() -> CaptureContext {
        let mut context = context();
        context.now = moscow(2026, 11, 10, 14, 32);
        context
            .lists
            .push(list("Календарь", ListKind::Events, true));
        context
    }

    fn date_none() -> Value {
        json!({
            "kind": "none",
            "n": 0,
            "unit": "",
            "weekday": "",
            "day": 0,
            "month": 0,
            "year": 0,
        })
    }

    fn offset(n: i64) -> Value {
        json!({
            "kind": "offset",
            "n": n,
            "unit": "days",
            "weekday": "",
            "day": 0,
            "month": 0,
            "year": 0,
        })
    }

    fn weekday(day: &str, n: i64) -> Value {
        json!({
            "kind": "weekday",
            "n": n,
            "unit": "",
            "weekday": day,
            "day": 0,
            "month": 0,
            "year": 0,
        })
    }

    fn date(day: i64, month: i64, year: i64) -> Value {
        json!({
            "kind": "date",
            "n": 0,
            "unit": "",
            "weekday": "",
            "day": day,
            "month": month,
            "year": year,
        })
    }

    fn week(n: i64) -> Value {
        json!({
            "kind": "week",
            "n": n,
            "unit": "",
            "weekday": "",
            "day": 0,
            "month": 0,
            "year": 0,
        })
    }

    fn task() -> Value {
        json!({
            "kind": "tasks",
            "list": "",
            "title": "Полить цветы",
            "date": date_none(),
            "time": "",
            "repeat": null,
            "assignee": "",
            "priority": "",
            "subtasks": [],
            "tags": [],
            "notes": "",
            "note_date": date_none(),
        })
    }

    fn event() -> Value {
        json!({
            "kind": "events",
            "list": "",
            "title": "Маникюр",
            "date": date_none(),
            "time": "",
            "end": date_none(),
            "end_time": "",
            "repeat": null,
            "assignee": "",
            "tags": [],
            "notes": "",
        })
    }

    fn with(mut item: Value, key: &str, value: Value) -> Value {
        item.as_object_mut()
            .expect("item is an object")
            .insert(key.to_owned(), value);
        item
    }

    fn weekly(days: &[i64], until: Value) -> Value {
        json!({
            "freq": "weekly",
            "interval": 1,
            "weekdays": days,
            "month_rule": "",
            "until": until,
        })
    }

    fn daily(until: Value) -> Value {
        json!({
            "freq": "daily",
            "interval": 1,
            "weekdays": [],
            "month_rule": "",
            "until": until,
        })
    }

    fn convert(context: &CaptureContext, value: Value) -> RawItem {
        let text = json!({"items": [value], "clarifications": []}).to_string();
        let mut parsed = parse_capture_response(&text, context).expect("wire item is valid");
        convert_item(&parsed.items.remove(0), context)
    }

    fn check(context: &CaptureContext, value: Value) -> DraftItem {
        validate_item(context, ItemSource::Telegram, convert(context, value))
    }

    fn codes(item: &DraftItem) -> Vec<&'static str> {
        item.errors.iter().map(|error| error.code).collect()
    }

    #[test]
    fn repeat_without_date_starts_at_the_first_occurrence() {
        let context = spec_context();
        let item = convert(
            &context,
            with(task(), "repeat", weekly(&[1, 4], date_none())),
        );

        assert_eq!(item.due_date.as_deref(), Some("2026-11-12"));
        assert!(
            check(
                &context,
                with(task(), "repeat", weekly(&[1, 4], date_none()))
            )
            .is_valid()
        );
    }

    #[test]
    fn a_named_date_must_match_the_repetition_rule() {
        let context = spec_context();
        let value = with(
            with(task(), "date", weekday("monday", 0)),
            "repeat",
            weekly(&[1, 4], date_none()),
        );
        let item = convert(&context, value);

        assert_eq!(item.due_date.as_deref(), Some("2026-11-16"));

        // Вторник 10 ноября не входит в правило «понедельник и четверг»:
        // берётся ближайшее вхождение не раньше названной даты — четверг.
        let value = with(
            with(task(), "date", weekday("tuesday", 0)),
            "repeat",
            weekly(&[1, 4], date_none()),
        );
        let item = convert(&context, value);

        assert_eq!(item.due_date.as_deref(), Some("2026-11-12"));
    }

    #[test]
    fn monthly_last_day_without_date_uses_the_month_end() {
        let context = spec_context();
        let repeat = json!({
            "freq": "monthly",
            "interval": 1,
            "weekdays": [],
            "month_rule": "last_day",
            "until": date_none(),
        });
        let item = convert(&context, with(task(), "repeat", repeat));

        assert_eq!(item.due_date.as_deref(), Some("2026-11-30"));
    }

    #[test]
    fn the_second_date_is_appended_to_the_note() {
        let context = spec_context();
        let value = with(
            with(with(task(), "date", offset(1)), "notes", json!("приём")),
            "note_date",
            date(20, 11, 0),
        );
        let item = convert(&context, value.clone());

        assert_eq!(item.due_date.as_deref(), Some("2026-11-11"));
        assert_eq!(item.notes.as_deref(), Some("приём 20 ноября"));
        assert!(check(&context, value).is_valid());
    }

    #[test]
    fn a_relative_second_date_is_rendered_as_text() {
        let context = spec_context();
        let value = with(
            with(task(), "notes", json!("вылет")),
            "note_date",
            offset(2),
        );
        let item = convert(&context, value);

        assert_eq!(item.notes.as_deref(), Some("вылет 12 ноября"));
    }

    #[test]
    fn a_second_date_in_another_year_keeps_the_year() {
        let context = spec_context();
        let value = with(
            with(task(), "notes", json!("вылет")),
            "note_date",
            date(5, 3, 2027),
        );
        let item = convert(&context, value);

        assert_eq!(item.notes.as_deref(), Some("вылет 5 марта 2027"));
    }

    #[test]
    fn an_empty_note_gets_only_the_second_date() {
        let context = spec_context();
        let item = convert(&context, with(task(), "note_date", date(20, 11, 0)));
        assert_eq!(item.notes.as_deref(), Some("20 ноября"));

        let blank = with(
            with(task(), "notes", json!("  ")),
            "note_date",
            date(20, 11, 0),
        );
        assert_eq!(convert(&context, blank).notes.as_deref(), Some("20 ноября"));
    }

    #[test]
    fn an_event_within_one_day_has_start_and_end_times() {
        let context = spec_context();
        let value = with(
            with(with(event(), "date", offset(1)), "time", json!("14:00")),
            "end_time",
            json!("15:30"),
        );
        let raw = convert(&context, value.clone());
        assert_eq!(raw.target_kind, ListKind::Events);
        assert_eq!(raw.due_date.as_deref(), Some("2026-11-11"));
        assert_eq!(raw.due_time.as_deref(), Some("14:00"));
        assert_eq!(raw.end_date.as_deref(), Some("2026-11-11"));
        assert_eq!(raw.end_time.as_deref(), Some("15:30"));

        let item = check(&context, value);
        assert!(item.is_valid(), "{:?}", item.errors);
        let event = item.item.as_ref().expect("valid event");
        assert_eq!(event.due_at, Some(moscow(2026, 11, 11, 14, 0)));
        assert_eq!(event.end_at, Some(moscow(2026, 11, 11, 15, 30)));
        assert_eq!(
            item.list_id,
            context.default_list(ListKind::Events).map(|list| list.id)
        );
    }

    #[test]
    fn a_multi_day_event_keeps_the_last_day_inclusive() {
        let context = spec_context();
        let value = with(with(event(), "date", date(5, 3, 0)), "end", date(10, 3, 0));
        let raw = convert(&context, value.clone());
        assert_eq!(raw.due_date.as_deref(), Some("2027-03-05"));
        assert_eq!(raw.end_date.as_deref(), Some("2027-03-10"));

        let item = check(&context, value);
        assert!(item.is_valid(), "{:?}", item.errors);
        let event = item.item.as_ref().expect("valid event");
        assert_eq!(
            event.due_date,
            Some(NaiveDate::from_ymd_opt(2027, 3, 5).unwrap())
        );
        assert_eq!(
            event.end_date,
            Some(NaiveDate::from_ymd_opt(2027, 3, 10).unwrap())
        );
    }

    #[test]
    fn an_end_time_not_after_the_start_moves_to_the_next_day() {
        let context = spec_context();
        let value = with(
            with(with(event(), "date", offset(1)), "time", json!("15:00")),
            "end_time",
            json!("14:00"),
        );
        let raw = convert(&context, value.clone());
        assert_eq!(raw.end_date.as_deref(), Some("2026-11-12"));

        let item = check(&context, value);
        assert!(item.is_valid(), "{:?}", item.errors);
        let event = item.item.as_ref().expect("valid event");
        assert_eq!(event.end_at, Some(moscow(2026, 11, 12, 14, 0)));
    }

    #[test]
    fn a_recurring_event_starts_at_the_first_occurrence() {
        let context = spec_context();
        let value = with(
            with(
                with(event(), "time", json!("14:00")),
                "end_time",
                json!("15:00"),
            ),
            "repeat",
            weekly(&[1], date_none()),
        );
        let raw = convert(&context, value.clone());
        assert_eq!(raw.due_date.as_deref(), Some("2026-11-16"));
        assert_eq!(raw.end_date.as_deref(), Some("2026-11-16"));

        let item = check(&context, value);
        assert!(item.is_valid(), "{:?}", item.errors);
    }

    #[test]
    fn an_impossible_date_is_an_element_error_not_a_parse_error() {
        let context = spec_context();
        let raw = convert(&context, with(task(), "date", date(31, 2, 0)));

        assert_eq!(raw.due_date, None);
        assert_eq!(
            raw.date_error.first().map(|error| error.field.as_str()),
            Some("due_date")
        );

        let item = check(&context, with(task(), "date", date(31, 2, 0)));
        assert_eq!(codes(&item), [DATE_INVALID]);
        assert!(item.has_date_question());
    }

    #[test]
    fn one_bad_expression_does_not_discard_the_other_items() {
        let context = spec_context();
        let text = json!({
            "items": [
                with(task(), "date", date(31, 2, 0)),
                with(with(task(), "title", json!("Купить молоко")), "date", offset(1)),
            ],
            "clarifications": [],
        })
        .to_string();
        let items = parse_capture_response(&text, &context)
            .expect("one bad expression is not a parse error")
            .items;
        let raws: Vec<RawItem> = items
            .iter()
            .map(|item| convert_item(item, &context))
            .collect();

        assert!(!raws[0].date_error.is_empty());
        assert_eq!(raws[1].due_date.as_deref(), Some("2026-11-11"));

        let first = validate_item(&context, ItemSource::Telegram, raws[0].clone());
        let second = validate_item(&context, ItemSource::Telegram, raws[1].clone());
        assert_eq!(codes(&first), [DATE_INVALID]);
        assert!(second.is_valid(), "{:?}", second.errors);
    }

    #[test]
    fn an_event_end_before_its_start_is_an_end_error() {
        let context = spec_context();
        let value = with(
            with(event(), "date", date(10, 3, 2027)),
            "end",
            date(5, 3, 2027),
        );
        let item = check(&context, value);

        assert_eq!(codes(&item), [END_BEFORE_START]);
        assert_eq!(item.errors[0].field, "end_date");
    }

    #[test]
    fn time_today_already_passed_is_an_error_but_the_current_minute_is_not() {
        let context = spec_context();
        let passed = with(
            with(with(task(), "date", offset(0)), "time", json!("09:00")),
            "title",
            json!("Позвонить"),
        );
        let item = check(&context, passed);
        assert_eq!(codes(&item), ["time_past"]);

        let current = with(
            with(with(task(), "date", offset(0)), "time", json!("14:32")),
            "title",
            json!("Позвонить"),
        );
        assert!(check(&context, current).is_valid());
    }

    #[test]
    fn time_without_date_that_passed_moves_to_tomorrow_without_an_error() {
        let context = spec_context();
        let value = with(
            with(task(), "time", json!("09:00")),
            "title",
            json!("Позвонить"),
        );
        let raw = convert(&context, value.clone());

        assert_eq!(raw.due_date.as_deref(), Some("2026-11-11"));
        assert!(check(&context, value).is_valid());
    }

    #[test]
    fn a_date_with_an_explicit_past_year_is_an_error() {
        let context = spec_context();
        let value = with(
            with(task(), "date", date(5, 3, 2026)),
            "title",
            json!("Позвонить"),
        );
        let item = check(&context, value);

        assert_eq!(codes(&item), [DATE_PAST]);
    }

    #[test]
    fn a_series_ending_before_its_first_occurrence_is_an_error() {
        let context = spec_context();
        let value = with(task(), "repeat", weekly(&[1, 4], date(11, 11, 0)));
        let raw = convert(&context, value.clone());
        assert_eq!(raw.due_date, None);

        let item = check(&context, value);
        assert_eq!(codes(&item), ["recurrence_range"]);
    }

    #[test]
    fn a_zero_interval_is_an_element_error() {
        let context = spec_context();
        let repeat = json!({
            "freq": "daily",
            "interval": 0,
            "weekdays": [],
            "month_rule": "",
            "until": date_none(),
        });
        let value = with(with(task(), "date", offset(1)), "repeat", repeat);
        let raw = convert(&context, value.clone());
        assert_eq!(raw.due_date.as_deref(), Some("2026-11-11"));

        let item = check(&context, value);
        assert_eq!(codes(&item), ["recurrence_interval"]);
    }

    #[test]
    fn until_at_the_end_of_a_period_uses_the_last_day() {
        let context = spec_context();
        let value = with(with(task(), "date", offset(1)), "repeat", daily(week(0)));
        let raw = convert(&context, value);

        assert_eq!(
            raw.recurrence
                .as_ref()
                .and_then(|repeat| repeat.until.as_deref()),
            Some("2026-11-15"),
            "until is the inclusive end of the week, not its beginning"
        );
    }

    #[test]
    fn an_all_day_event_without_an_end_has_no_end() {
        let context = spec_context();
        let value = with(event(), "date", offset(1));
        let item = check(&context, value);

        assert!(item.is_valid(), "{:?}", item.errors);
        let event = item.item.as_ref().expect("valid event");
        assert_eq!(
            event.due_date,
            Some(NaiveDate::from_ymd_opt(2026, 11, 11).unwrap())
        );
        assert_eq!(event.end_date, None);
        assert_eq!(event.end_at, None);
    }

    #[test]
    fn an_impossible_date_with_time_has_only_the_date_error() {
        let context = spec_context();
        let value = with(with(task(), "date", date(31, 2, 0)), "time", json!("15:00"));
        let item = check(&context, value);

        assert_eq!(codes(&item), [DATE_INVALID]);
    }

    #[test]
    fn an_impossible_end_date_is_reported_on_the_end_field() {
        let context = spec_context();
        let value = with(with(event(), "date", offset(1)), "end", date(31, 2, 0));
        let raw = convert(&context, value.clone());
        assert_eq!(
            raw.date_error.first().map(|error| error.field.as_str()),
            Some("end_date")
        );

        let item = check(&context, value);
        assert_eq!(codes(&item), ["end_date_invalid"]);
        assert_eq!(item.errors[0].field, "end_date");
    }

    #[test]
    fn an_impossible_until_is_reported_on_recurrence() {
        let context = spec_context();
        let value = with(
            with(task(), "date", offset(1)),
            "repeat",
            daily(date(31, 2, 0)),
        );
        let raw = convert(&context, value.clone());
        assert_eq!(
            raw.date_error.first().map(|error| error.field.as_str()),
            Some("recurrence")
        );

        let item = check(&context, value);
        assert_eq!(codes(&item), ["recurrence_until"]);
    }

    #[test]
    fn an_impossible_note_date_is_reported_on_notes() {
        let context = spec_context();
        let value = with(with(task(), "date", offset(1)), "note_date", date(31, 2, 0));
        let raw = convert(&context, value.clone());
        assert_eq!(
            raw.date_error.first().map(|error| error.field.as_str()),
            Some("note_date")
        );

        let item = check(&context, value);
        assert_eq!(codes(&item), ["note_date_invalid"]);
        assert_eq!(item.errors[0].field, "notes");
    }

    #[test]
    fn a_bad_start_and_a_bad_note_date_keep_both_errors() {
        let context = spec_context();
        let value = with(
            with(task(), "date", date(31, 2, 0)),
            "note_date",
            date(31, 2, 0),
        );
        let raw = convert(&context, value.clone());

        let fields = raw
            .date_error
            .iter()
            .map(|error| error.field.as_str())
            .collect::<Vec<_>>();
        assert_eq!(fields, ["due_date", "note_date"]);

        let item = check(&context, value);
        assert_eq!(codes(&item), ["date_invalid", "note_date_invalid"]);
    }

    #[test]
    fn weekly_without_weekdays_becomes_daily_interval() {
        assert_eq!(
            normalize_freq(&RecurrenceFreq::Weekly, 1, &[]),
            (RecurrenceFreq::Daily, 7)
        );
        assert_eq!(
            normalize_freq(&RecurrenceFreq::Weekly, 2, &[]),
            (RecurrenceFreq::Daily, 14)
        );
        assert_eq!(
            normalize_freq(&RecurrenceFreq::Weekly, 2, &[2]),
            (RecurrenceFreq::Weekly, 2)
        );
        assert_eq!(
            normalize_freq(&RecurrenceFreq::Daily, 3, &[]),
            (RecurrenceFreq::Daily, 3)
        );
    }
}
