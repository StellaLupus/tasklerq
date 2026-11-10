//! Валидация доменных полей.

use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc};

use super::error::DomainError;
use super::events::MAX_EVENT_DAYS;
use super::models::{
    ItemPriority, ItemSource, ListKind, MonthRule, NewItem, Recurrence, RecurrenceFreq,
    RecurrenceInput, SubtaskInput,
};
use super::recurrence::{Due, last_day_of_month, parse_timezone, resolve_local};

/// Максимальная длина названия.
pub const MAX_TITLE: usize = 200;
/// Максимальная длина описания.
pub const MAX_NOTES: usize = 2_000;
/// Максимальная длина единицы измерения.
pub const MAX_UNIT: usize = 16;
/// Максимальное количество тегов.
pub const MAX_TAGS: usize = 10;
/// Максимальная длина тега.
pub const MAX_TAG: usize = 32;
/// Максимальное количество подзадач.
pub const MAX_SUBTASKS: usize = 50;

/// Проверенные поля нового элемента.
#[derive(Debug, Clone)]
pub struct ValidatedItem {
    /// Название.
    pub title: String,
    /// Описание.
    pub notes: Option<String>,
    /// Приоритет.
    pub priority: ItemPriority,
    /// Срок со временем.
    pub due_at: Option<DateTime<Utc>>,
    /// Вседневный срок.
    pub due_date: Option<NaiveDate>,
    /// Конец события со временем.
    pub end_at: Option<DateTime<Utc>>,
    /// Последний день вседневного события включительно.
    pub end_date: Option<NaiveDate>,
    /// Количество.
    pub quantity: Option<f64>,
    /// Единица.
    pub unit: Option<String>,
    /// Теги.
    pub tags: Vec<String>,
    /// Подзадачи.
    pub subtasks: Vec<SubtaskInput>,
    /// Повторение.
    pub recurrence: Option<Recurrence>,
    /// Источник.
    pub source: ItemSource,
}

/// Проверить и нормализовать новый элемент.
pub fn validate_new_item(
    list_kind: ListKind,
    household_timezone: &str,
    item: NewItem,
    today: NaiveDate,
) -> Result<ValidatedItem, DomainError> {
    let title = normalize_text(&item.title, MAX_TITLE, "title")?;
    let notes = item
        .notes
        .map(|notes| normalize_text_allow_empty(&notes, MAX_NOTES, "notes"))
        .transpose()?;

    if item.due_at.is_some() && item.due_date.is_some() {
        return Err(DomainError::Validation(
            "due_at and due_date are mutually exclusive".into(),
        ));
    }

    validate_fields_for_kind(
        list_kind,
        item.due_at,
        item.due_date,
        item.end_at,
        item.end_date,
        item.priority,
        item.quantity.is_some(),
        item.unit.is_some(),
        !item.subtasks.is_empty(),
    )?;

    // У события нет полей покупки: количество и единица не поддерживаются.
    let (quantity, unit) = match list_kind {
        ListKind::Tasks | ListKind::Events => (None, None),
        _ => (
            Some(validate_quantity(item.quantity)?),
            Some(normalize_unit(item.unit)),
        ),
    };

    let tags = normalize_tags(item.tags)?;
    let subtasks = normalize_subtasks(item.subtasks)?;

    let recurrence = match item.recurrence {
        Some(input) => {
            // 0009-events 5.1: повторение станет доступно и спискам событий.
            if list_kind != ListKind::Tasks {
                return Err(DomainError::Validation(
                    "recurrence is only allowed for task lists".into(),
                ));
            }
            Some(normalize_recurrence(
                input,
                household_timezone,
                item.due_at,
                item.due_date,
                today,
            )?)
        }
        None => None,
    };

    let (due_at, due_date) = normalize_due_for_recurrence(
        item.due_at,
        item.due_date,
        recurrence.as_ref(),
        household_timezone,
        today,
    )?;

    Ok(ValidatedItem {
        title,
        notes,
        priority: item.priority,
        due_at,
        due_date,
        end_at: item.end_at,
        end_date: item.end_date,
        quantity,
        unit,
        tags,
        subtasks,
        recurrence,
        source: item.source,
    })
}

/// Проверить поля элемента с учётом типа списка.
///
/// Общая проверка для создания и изменения: получает итоговое состояние
/// элемента. У события обязательно начало, конец того же вида, что начало, и не
/// раньше него, а приоритет, количество, единица и подзадачи не поддерживаются.
/// У элемента не из списка событий конца быть не может.
#[allow(clippy::too_many_arguments)]
pub fn validate_fields_for_kind(
    list_kind: ListKind,
    due_at: Option<DateTime<Utc>>,
    due_date: Option<NaiveDate>,
    end_at: Option<DateTime<Utc>>,
    end_date: Option<NaiveDate>,
    priority: ItemPriority,
    quantity_supplied: bool,
    unit_supplied: bool,
    has_subtasks: bool,
) -> Result<(), DomainError> {
    if list_kind != ListKind::Events {
        if end_at.is_some() || end_date.is_some() {
            return Err(DomainError::Validation(
                "end_at and end_date are only allowed for events".into(),
            ));
        }
        return Ok(());
    }

    if due_at.is_none() && due_date.is_none() {
        return Err(DomainError::Validation(
            "an event must have due_at or due_date".into(),
        ));
    }
    if end_at.is_some() && end_date.is_some() {
        return Err(DomainError::Validation(
            "end_at and end_date are mutually exclusive".into(),
        ));
    }
    if end_at.is_some() && due_at.is_none() {
        return Err(DomainError::Validation("end_at requires due_at".into()));
    }
    if end_date.is_some() && due_date.is_none() {
        return Err(DomainError::Validation("end_date requires due_date".into()));
    }

    if let (Some(start), Some(end)) = (due_at, end_at) {
        if end <= start {
            return Err(DomainError::Validation(
                "event end must be after its start".into(),
            ));
        }
        if end - start > Duration::days(MAX_EVENT_DAYS) {
            return Err(DomainError::Validation(format!(
                "event duration must be at most {MAX_EVENT_DAYS} days"
            )));
        }
    }

    if let (Some(start), Some(end)) = (due_date, end_date) {
        if end < start {
            return Err(DomainError::Validation(
                "event end must not be before its start".into(),
            ));
        }
        // `end_date` хранится включительно: 5 и 10 марта — шесть дней.
        if (end - start).num_days() + 1 > MAX_EVENT_DAYS {
            return Err(DomainError::Validation(format!(
                "event duration must be at most {MAX_EVENT_DAYS} days"
            )));
        }
    }

    if priority != ItemPriority::Normal {
        return Err(DomainError::Validation(
            "an event priority must be normal".into(),
        ));
    }
    if quantity_supplied || unit_supplied {
        return Err(DomainError::Validation(
            "quantity and unit are not supported for events".into(),
        ));
    }
    if has_subtasks {
        return Err(DomainError::Validation(
            "subtasks are not supported for events".into(),
        ));
    }

    Ok(())
}

/// Проверить и нормализовать правило повторения.
pub fn normalize_recurrence(
    input: RecurrenceInput,
    household_timezone: &str,
    due_at: Option<DateTime<Utc>>,
    due_date: Option<NaiveDate>,
    today: NaiveDate,
) -> Result<Recurrence, DomainError> {
    let due = match (due_at, due_date) {
        (Some(at), None) => Some(Due::Timed(at)),
        (None, Some(date)) => Some(Due::AllDay(date)),
        _ => None,
    };

    let recurrence = input.normalize(household_timezone, due, today)?;

    Ok(recurrence)
}

/// Если срок не задан, взять первое вхождение; для `last_day` привести срок к
/// последнему дню месяца.
pub fn normalize_due_for_recurrence(
    due_at: Option<DateTime<Utc>>,
    due_date: Option<NaiveDate>,
    recurrence: Option<&Recurrence>,
    household_timezone: &str,
    today: NaiveDate,
) -> Result<(Option<DateTime<Utc>>, Option<NaiveDate>), DomainError> {
    let Some(recurrence) = recurrence else {
        return Ok((due_at, due_date));
    };

    let timezone_name = if recurrence.timezone.is_empty() {
        household_timezone
    } else {
        &recurrence.timezone
    };
    let timezone = parse_timezone(timezone_name)?;

    let mut due_at = due_at;
    let mut due_date = due_date;

    if due_at.is_none() && due_date.is_none() {
        match recurrence.first_due(today)? {
            Due::AllDay(date) => due_date = Some(date),
            Due::Timed(at) => due_at = Some(at),
        }
    }

    if recurrence.freq == RecurrenceFreq::Monthly
        && recurrence.month_rule == Some(MonthRule::LastDay)
    {
        if let Some(date) = due_date {
            due_date = Some(last_day_of_month(date.year(), date.month()));
        }
        if let Some(at) = due_at {
            let local = at.with_timezone(&timezone);
            let last = last_day_of_month(local.year(), local.month());
            if last != local.date_naive() {
                due_at = Some(resolve_local(timezone, last, local.time())?);
            }
        }
    }

    Ok((due_at, due_date))
}

/// Нормализовать и проверить теги.
pub fn normalize_tags(tags: Vec<String>) -> Result<Vec<String>, DomainError> {
    if tags.len() > MAX_TAGS {
        return Err(DomainError::Validation(format!(
            "at most {MAX_TAGS} tags are allowed"
        )));
    }

    let mut normalized = Vec::with_capacity(tags.len());
    for tag in tags {
        let tag = normalize_text(&tag, MAX_TAG, "tag")?.to_lowercase();
        if !normalized.contains(&tag) {
            normalized.push(tag);
        }
    }
    Ok(normalized)
}

/// Нормализовать и проверить подзадачи.
pub fn normalize_subtasks(subtasks: Vec<SubtaskInput>) -> Result<Vec<SubtaskInput>, DomainError> {
    if subtasks.len() > MAX_SUBTASKS {
        return Err(DomainError::Validation(format!(
            "at most {MAX_SUBTASKS} subtasks are allowed"
        )));
    }

    subtasks
        .into_iter()
        .map(|subtask| {
            Ok(SubtaskInput {
                title: normalize_text(&subtask.title, MAX_TITLE, "subtask title")?,
                status: subtask.status,
            })
        })
        .collect()
}

/// Максимальное количество при вводе пользователем: значение сверху ограничено
/// числом 9999.
///
/// Целым ввод не является: бот и capture принимают и дробную часть (`2.5`,
/// `0.001`). Доменная проверка [`validate_quantity`] допускает более широкий
/// предел `9999.999` и защищает хранение, поэтому константа общая для обоих
/// слоёв.
pub const MAX_INPUT_QUANTITY: f64 = 9_999.0;

/// Проверить количество.
pub fn validate_quantity(quantity: Option<f64>) -> Result<f64, DomainError> {
    let quantity = quantity.unwrap_or(1.0);
    if !quantity.is_finite() || quantity <= 0.0 || quantity > 9999.999 {
        return Err(DomainError::Validation(
            "quantity must be greater than 0 and at most 9999.999".into(),
        ));
    }
    let scaled = quantity * 1000.0;
    if (scaled - scaled.round()).abs() > 1e-6 {
        return Err(DomainError::Validation(
            "quantity supports at most three decimal places".into(),
        ));
    }
    Ok(quantity)
}

/// Нормализовать единицу измерения.
pub fn normalize_unit(unit: Option<String>) -> String {
    match unit.map(|unit| unit.trim().to_owned()) {
        Some(unit) if !unit.is_empty() => unit.chars().take(MAX_UNIT).collect(),
        _ => "шт".to_owned(),
    }
}

/// Обрезать, удалить управляющие символы и проверить длину.
pub fn normalize_text(value: &str, max: usize, field: &str) -> Result<String, DomainError> {
    let cleaned = sanitize(value);
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        return Err(DomainError::Validation(format!(
            "{field} must not be empty"
        )));
    }
    if cleaned.chars().count() > max {
        return Err(DomainError::Validation(format!(
            "{field} must be at most {max} characters"
        )));
    }
    Ok(cleaned.to_owned())
}

/// Как [`normalize_text`], но пустая строка допустима.
pub fn normalize_text_allow_empty(
    value: &str,
    max: usize,
    field: &str,
) -> Result<String, DomainError> {
    let cleaned = sanitize(value);
    let cleaned = cleaned.trim();
    if cleaned.chars().count() > max {
        return Err(DomainError::Validation(format!(
            "{field} must be at most {max} characters"
        )));
    }
    Ok(cleaned.to_owned())
}

/// Удалить управляющие символы, кроме перевода строки и табуляции.
pub(crate) fn sanitize(value: &str) -> String {
    value
        .chars()
        .filter(|ch| !ch.is_control() || matches!(ch, '\n' | '\t'))
        .collect()
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use uuid::Uuid;

    use super::*;

    const TZ: &str = "Europe/Moscow";

    fn date(year: i32, month: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(year, month, day).expect("valid date")
    }

    fn utc(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(year, month, day, hour, minute, 0)
            .single()
            .expect("valid instant")
    }

    fn new_item(due_at: Option<DateTime<Utc>>, due_date: Option<NaiveDate>) -> NewItem {
        NewItem {
            list_id: Uuid::nil(),
            title: "Маникюр".into(),
            notes: None,
            priority: ItemPriority::Normal,
            due_at,
            due_date,
            end_at: None,
            end_date: None,
            assignee_user_id: None,
            source: ItemSource::Api,
            quantity: None,
            unit: None,
            tags: Vec::new(),
            subtasks: Vec::new(),
            recurrence: None,
        }
    }

    fn validate(kind: ListKind, item: NewItem) -> Result<ValidatedItem, DomainError> {
        validate_new_item(kind, TZ, item, date(2026, 11, 10))
    }

    fn assert_rejected(kind: ListKind, item: NewItem) {
        let error = validate(kind, item).expect_err("item must be rejected");
        assert!(matches!(error, DomainError::Validation(_)), "{error:?}");
    }

    #[test]
    fn event_with_time_is_accepted() {
        let start = utc(2026, 11, 11, 14, 0);

        let validated =
            validate(ListKind::Events, new_item(Some(start), None)).expect("timed event");

        assert_eq!(validated.due_at, Some(start));
        assert_eq!(validated.due_date, None);
    }

    #[test]
    fn all_day_event_is_accepted() {
        let start = date(2026, 12, 3);

        let validated =
            validate(ListKind::Events, new_item(None, Some(start))).expect("all-day event");

        assert_eq!(validated.due_date, Some(start));
        assert_eq!(validated.due_at, None);
    }

    #[test]
    fn event_without_start_is_rejected() {
        assert_rejected(ListKind::Events, new_item(None, None));
    }

    #[test]
    fn event_end_within_a_day_is_accepted() {
        let start = utc(2026, 11, 11, 14, 0);
        let end = utc(2026, 11, 11, 15, 30);
        let mut item = new_item(Some(start), None);
        item.end_at = Some(end);

        let validated = validate(ListKind::Events, item).expect("event with end");

        assert_eq!(validated.due_at, Some(start));
        assert_eq!(validated.end_at, Some(end));
    }

    #[test]
    fn multi_day_event_is_accepted() {
        let start = date(2027, 3, 5);
        let end = date(2027, 3, 10);
        let mut item = new_item(None, Some(start));
        item.end_date = Some(end);

        let validated = validate(ListKind::Events, item).expect("multi-day event");

        assert_eq!(validated.due_date, Some(start));
        assert_eq!(validated.end_date, Some(end));
    }

    #[test]
    fn event_end_before_start_is_rejected() {
        let mut item = new_item(Some(utc(2026, 11, 11, 14, 0)), None);
        item.end_at = Some(utc(2026, 11, 11, 13, 0));
        assert_rejected(ListKind::Events, item);

        let mut item = new_item(None, Some(date(2027, 3, 10)));
        item.end_date = Some(date(2027, 3, 5));
        assert_rejected(ListKind::Events, item);
    }

    #[test]
    fn event_end_of_another_kind_is_rejected() {
        let mut all_day_start_with_timed_end = new_item(None, Some(date(2026, 11, 11)));
        all_day_start_with_timed_end.end_at = Some(utc(2026, 11, 11, 15, 0));
        assert_rejected(ListKind::Events, all_day_start_with_timed_end);

        let mut timed_start_with_all_day_end = new_item(Some(utc(2026, 11, 11, 14, 0)), None);
        timed_start_with_all_day_end.end_date = Some(date(2026, 11, 11));
        assert_rejected(ListKind::Events, timed_start_with_all_day_end);
    }

    #[test]
    fn event_longer_than_366_days_is_rejected() {
        let start = utc(2026, 11, 11, 14, 0);

        let mut exactly_366 = new_item(Some(start), None);
        exactly_366.end_at = Some(start + Duration::days(MAX_EVENT_DAYS));
        validate(ListKind::Events, exactly_366).expect("366 days are allowed");

        let mut longer = new_item(Some(start), None);
        longer.end_at = Some(start + Duration::days(MAX_EVENT_DAYS) + Duration::minutes(1));
        assert_rejected(ListKind::Events, longer);

        let mut inclusive_longer = new_item(None, Some(date(2026, 1, 1)));
        inclusive_longer.end_date = Some(date(2027, 1, 2));
        assert_rejected(ListKind::Events, inclusive_longer);
    }

    #[test]
    fn event_of_a_participant_is_accepted() {
        let mut item = new_item(Some(utc(2026, 11, 11, 14, 0)), None);
        item.assignee_user_id = Some(Uuid::now_v7());
        item.notes = Some("Записаться заранее".into());
        item.tags = vec!["Маша".into()];

        let validated = validate(ListKind::Events, item).expect("event with a participant");

        assert_eq!(validated.notes.as_deref(), Some("Записаться заранее"));
        assert_eq!(validated.tags, vec!["маша".to_owned()]);
    }

    #[test]
    fn event_with_subtasks_is_rejected() {
        let mut item = new_item(Some(utc(2026, 11, 11, 14, 0)), None);
        item.subtasks = vec![SubtaskInput {
            title: "Купить лак".into(),
            status: None,
        }];
        assert_rejected(ListKind::Events, item);
    }

    #[test]
    fn event_with_non_normal_priority_is_rejected() {
        for priority in [ItemPriority::Low, ItemPriority::High, ItemPriority::Urgent] {
            let mut item = new_item(Some(utc(2026, 11, 11, 14, 0)), None);
            item.priority = priority;
            assert_rejected(ListKind::Events, item);
        }
    }

    #[test]
    fn event_with_quantity_is_rejected() {
        let mut item = new_item(Some(utc(2026, 11, 11, 14, 0)), None);
        item.quantity = Some(2.0);
        assert_rejected(ListKind::Events, item);
    }

    #[test]
    fn event_with_unit_is_rejected() {
        let mut item = new_item(Some(utc(2026, 11, 11, 14, 0)), None);
        item.unit = Some("шт".into());
        assert_rejected(ListKind::Events, item);
    }

    #[test]
    fn end_for_a_task_is_rejected() {
        let mut item = new_item(Some(utc(2026, 11, 11, 14, 0)), None);
        item.end_at = Some(utc(2026, 11, 11, 15, 0));
        assert_rejected(ListKind::Tasks, item);

        let mut item = new_item(None, Some(date(2026, 11, 11)));
        item.end_date = Some(date(2026, 11, 11));
        assert_rejected(ListKind::Tasks, item);
    }
}
