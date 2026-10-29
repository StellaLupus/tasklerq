//! Валидация доменных полей.

use chrono::{DateTime, Datelike, NaiveDate, Utc};

use super::error::DomainError;
use super::models::{
    ItemPriority, ItemSource, ListKind, MonthRule, NewItem, Recurrence, RecurrenceFreq,
    RecurrenceInput, SubtaskInput,
};
use super::recurrence::{Due, last_day_of_month, parse_timezone, resolve_local, weekday_iso};

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

    let (quantity, unit) = if matches!(list_kind, ListKind::Tasks) {
        (None, None)
    } else {
        (
            Some(validate_quantity(item.quantity)?),
            Some(normalize_unit(item.unit)),
        )
    };

    let tags = normalize_tags(item.tags)?;
    let subtasks = normalize_subtasks(item.subtasks)?;

    let recurrence = match item.recurrence {
        Some(input) => {
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
        quantity,
        unit,
        tags,
        subtasks,
        recurrence,
        source: item.source,
    })
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
fn sanitize(value: &str) -> String {
    value
        .chars()
        .filter(|ch| !ch.is_control() || matches!(ch, '\n' | '\t'))
        .collect()
}

/// Проверить, что день недели входит в ISO-диапазон.
pub fn valid_weekdays(byweekday: &[i16]) -> bool {
    byweekday.iter().all(|day| (1..=7).contains(day))
}

/// ISO-номер дня недели.
pub fn iso_weekday(date: NaiveDate) -> i16 {
    weekday_iso(date.weekday())
}
