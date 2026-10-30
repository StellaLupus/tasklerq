//! Разбор ответа LLM в типизированную структуру.
//!
//! Парсер отвечает только за форму: обязательные поля, типы, число элементов и
//! нормализацию объектов-обёрток Home Assistant. Доменные границы (длины,
//! даты, участники) проверяет [`super::validate`]: нарушение формы означает
//! «некорректный ответ» и запускает повтор или fallback, а нарушение домена
//! привязывается к элементу и показывается пользователю.
//!
//! Home Assistant не возвращает поля без значения, поэтому всё, кроме
//! `target_kind` и `title`, необязательно. Лишние поля, включая любые
//! идентификаторы, не читаются вовсе.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;
use uuid::Uuid;

use crate::domain::models::ListKind;

/// Максимум элементов в одном ответе.
pub const MAX_ITEMS: usize = 20;
/// Максимум уточняющих вопросов.
pub const MAX_CLARIFICATIONS: usize = 3;

/// Нарушение формы ответа. Сообщения не содержат текст ответа LLM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ParseError {
    /// Корень ответа — не объект.
    #[error("response is not an object")]
    NotAnObject,
    /// Нет поля `items`.
    #[error("response has no items field")]
    MissingItems,
    /// Элементов больше лимита: пользователя нужно попросить разделить текст.
    #[error("response has {count} items, at most {MAX_ITEMS} are allowed")]
    TooManyItems {
        /// Сколько элементов вернула LLM.
        count: usize,
    },
    /// Обязательное поле элемента отсутствует.
    #[error("item {index} has no {field}")]
    MissingField {
        /// Номер элемента с нуля.
        index: usize,
        /// Имя поля.
        field: &'static str,
    },
    /// Поле имеет недопустимый тип или значение.
    #[error("invalid {field}")]
    InvalidField {
        /// Номер элемента с нуля; `None` для полей верхнего уровня.
        index: Option<usize>,
        /// Имя поля.
        field: &'static str,
    },
}

impl ParseError {
    /// Короткий код для `llm_runs.error_code`.
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotAnObject => "not_an_object",
            Self::MissingItems => "missing_items",
            Self::TooManyItems { .. } => "too_many_items",
            Self::MissingField { .. } => "missing_field",
            Self::InvalidField { .. } => "invalid_field",
        }
    }
}

/// Правило повторения «как сказала LLM», до проверки диапазонов.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RawRecurrence {
    /// `daily`/`weekly`/`monthly`/`yearly`.
    pub freq: Option<String>,
    /// Интервал.
    pub interval: Option<i64>,
    /// `anchor_day`/`last_day`.
    pub month_rule: Option<String>,
    /// Дни недели ISO.
    pub weekdays: Vec<i64>,
    /// Дата окончания `YYYY-MM-DD`.
    pub until: Option<String>,
}

/// Элемент черновика «как сказала LLM». Хранится в `capture_drafts.parsed`,
/// поэтому новые поля должны иметь `#[serde(default)]`: черновик, записанный до
/// обновления, не должен переставать читаться.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RawItem {
    /// Тип списка.
    pub target_kind: ListKind,
    /// Имя списка.
    pub target_list: Option<String>,
    /// Название; может быть пустым — это ошибка домена, а не формы.
    pub title: String,
    /// Заметка.
    pub notes: Option<String>,
    /// Дата `YYYY-MM-DD`.
    pub due_date: Option<String>,
    /// Время `HH:MM`.
    pub due_time: Option<String>,
    /// Приоритет.
    pub priority: Option<String>,
    /// Исполнитель.
    pub assignee: Option<String>,
    /// Участник, выбранный пользователем при правке черновика. Парсер ответа
    /// LLM это поле не заполняет: идентификаторы из ответа модели игнорируются.
    #[serde(default)]
    pub assignee_id: Option<Uuid>,
    /// Теги.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Количество.
    pub quantity: Option<f64>,
    /// Единица.
    pub unit: Option<String>,
    /// Названия подзадач.
    #[serde(default)]
    pub subtasks: Vec<String>,
    /// Повторение.
    pub recurrence: Option<RawRecurrence>,
}

/// Разобранный ответ LLM.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParsedResponse {
    /// Элементы, не более [`MAX_ITEMS`].
    pub items: Vec<RawItem>,
    /// Вопросы, не более [`MAX_CLARIFICATIONS`].
    pub clarifications: Vec<String>,
}

/// Разобрать объект `data` из ответа `ai_task.generate_data`.
pub fn parse_response(data: &Value) -> Result<ParsedResponse, ParseError> {
    let root = data.as_object().ok_or(ParseError::NotAnObject)?;

    let items = match root.get("items") {
        Some(Value::Array(items)) => items,
        Some(Value::Null) | None => return Err(ParseError::MissingItems),
        Some(_) => {
            return Err(ParseError::InvalidField {
                index: None,
                field: "items",
            });
        }
    };
    if items.len() > MAX_ITEMS {
        return Err(ParseError::TooManyItems { count: items.len() });
    }

    let items = items
        .iter()
        .enumerate()
        .map(|(index, item)| parse_item(index, item))
        .collect::<Result<Vec<_>, _>>()?;

    // Вопросы — вторичное поле: испорченное значение не должно отбрасывать
    // валидные элементы и запускать повтор запроса к LLM.
    let mut clarifications = text_list(
        root.get("clarifications"),
        None,
        "clarifications",
        "question",
    )
    .unwrap_or_default();
    clarifications.truncate(MAX_CLARIFICATIONS);

    Ok(ParsedResponse {
        items,
        clarifications,
    })
}

fn parse_item(index: usize, value: &Value) -> Result<RawItem, ParseError> {
    let object = value.as_object().ok_or(ParseError::InvalidField {
        index: Some(index),
        field: "item",
    })?;
    let at = Some(index);

    let kind = required(object, index, "target_kind")?;
    let target_kind = match kind.as_str().map(|kind| kind.trim().to_lowercase()) {
        Some(kind) if kind == "tasks" => ListKind::Tasks,
        Some(kind) if kind == "shopping" => ListKind::Shopping,
        Some(kind) if kind == "checklist" => ListKind::Checklist,
        _ => {
            return Err(ParseError::InvalidField {
                index: at,
                field: "target_kind",
            });
        }
    };
    let title = required(object, index, "title")?
        .as_str()
        .ok_or(ParseError::InvalidField {
            index: at,
            field: "title",
        })?
        .to_owned();

    Ok(RawItem {
        target_kind,
        target_list: optional_text(object, at, "target_list")?,
        title,
        notes: optional_text(object, at, "notes")?,
        due_date: optional_text(object, at, "due_date")?,
        due_time: optional_text(object, at, "due_time")?,
        priority: optional_text(object, at, "priority")?,
        assignee: optional_text(object, at, "assignee")?,
        assignee_id: None,
        tags: text_list(object.get("tags"), at, "tags", "value")?,
        quantity: optional_number(object, at, "quantity")?,
        unit: optional_text(object, at, "unit")?,
        subtasks: text_list(object.get("subtasks"), at, "subtasks", "title")?,
        recurrence: parse_recurrence(object.get("recurrence"), at)?,
    })
}

fn required<'a>(
    object: &'a Map<String, Value>,
    index: usize,
    field: &'static str,
) -> Result<&'a Value, ParseError> {
    match object.get(field) {
        Some(Value::Null) | None => Err(ParseError::MissingField { index, field }),
        Some(value) => Ok(value),
    }
}

/// Строка; `null`, отсутствие и пустая строка дают `None`.
fn optional_text(
    object: &Map<String, Value>,
    index: Option<usize>,
    field: &'static str,
) -> Result<Option<String>, ParseError> {
    match object.get(field) {
        Some(Value::Null) | None => Ok(None),
        Some(Value::String(text)) if text.trim().is_empty() => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(ParseError::InvalidField { index, field }),
    }
}

fn optional_number(
    object: &Map<String, Value>,
    index: Option<usize>,
    field: &'static str,
) -> Result<Option<f64>, ParseError> {
    match object.get(field) {
        Some(Value::Null) | None => Ok(None),
        Some(Value::Number(number)) => number
            .as_f64()
            .map(Some)
            .ok_or(ParseError::InvalidField { index, field }),
        Some(_) => Err(ParseError::InvalidField { index, field }),
    }
}

/// Целое число: Gemini может вернуть `2.0`, но не `2.5`.
fn integer(value: &Value) -> Option<i64> {
    let number = value.as_f64()?;
    (number.is_finite() && number.fract() == 0.0 && number.abs() < 1e15).then_some(number as i64)
}

fn optional_integer(
    object: &Map<String, Value>,
    index: Option<usize>,
    field: &'static str,
) -> Result<Option<i64>, ParseError> {
    match object.get(field) {
        Some(Value::Null) | None => Ok(None),
        Some(value @ Value::Number(_)) => integer(value)
            .map(Some)
            .ok_or(ParseError::InvalidField { index, field }),
        Some(_) => Err(ParseError::InvalidField { index, field }),
    }
}

/// Список строк. Элемент — строка либо объект-обёртка `{wrapper: строка}`,
/// который создаёт селектор `object` Home Assistant. Пустые значения
/// отбрасываются; одиночное значение вместо массива не принимается.
fn text_list(
    value: Option<&Value>,
    index: Option<usize>,
    field: &'static str,
    wrapper: &'static str,
) -> Result<Vec<String>, ParseError> {
    let entries = match value {
        Some(Value::Null) | None => return Ok(Vec::new()),
        Some(Value::Array(entries)) => entries,
        Some(_) => return Err(ParseError::InvalidField { index, field }),
    };
    let mut texts = Vec::with_capacity(entries.len());
    for entry in entries {
        let text = match entry {
            Value::String(text) => text,
            Value::Object(object) => match object.get(wrapper) {
                Some(Value::String(text)) => text,
                Some(Value::Null) | None => continue,
                Some(_) => return Err(ParseError::InvalidField { index, field }),
            },
            Value::Null => continue,
            _ => return Err(ParseError::InvalidField { index, field }),
        };
        if !text.trim().is_empty() {
            texts.push(text.clone());
        }
    }
    Ok(texts)
}

fn parse_recurrence(
    value: Option<&Value>,
    index: Option<usize>,
) -> Result<Option<RawRecurrence>, ParseError> {
    let object = match value {
        Some(Value::Null) | None => return Ok(None),
        Some(Value::Object(object)) => object,
        Some(_) => {
            return Err(ParseError::InvalidField {
                index,
                field: "recurrence",
            });
        }
    };

    let recurrence = RawRecurrence {
        freq: optional_text(object, index, "freq")?,
        interval: optional_integer(object, index, "interval")?,
        month_rule: optional_text(object, index, "month_rule")?,
        weekdays: weekdays(object.get("weekdays"), index)?,
        until: optional_text(object, index, "until")?,
    };
    // Селектор без значений приходит как пустой объект: повторения нет.
    Ok((recurrence != RawRecurrence::default()).then_some(recurrence))
}

/// Дни недели: числа либо объекты-обёртки `{"day": число}`.
fn weekdays(value: Option<&Value>, index: Option<usize>) -> Result<Vec<i64>, ParseError> {
    const FIELD: &str = "weekdays";
    let entries = match value {
        Some(Value::Null) | None => return Ok(Vec::new()),
        Some(Value::Array(entries)) => entries,
        Some(_) => {
            return Err(ParseError::InvalidField {
                index,
                field: FIELD,
            });
        }
    };
    let mut days = Vec::with_capacity(entries.len());
    for entry in entries {
        let number = match entry {
            Value::Object(object) => match object.get("day") {
                Some(Value::Null) | None => continue,
                Some(day) => day,
            },
            Value::Null => continue,
            other => other,
        };
        let day = integer(number).ok_or(ParseError::InvalidField {
            index,
            field: FIELD,
        })?;
        days.push(day);
    }
    Ok(days)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn item(extra: Value) -> Value {
        let mut base = json!({"target_kind": "tasks", "title": "Позвонить маме"});
        base.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        base
    }

    fn parse_one(extra: Value) -> Result<RawItem, ParseError> {
        let mut parsed = parse_response(&json!({"items": [item(extra)]}))?;
        Ok(parsed.items.remove(0))
    }

    #[test]
    fn full_example_from_docs_is_parsed() {
        let data = json!({
            "items": [{
                "target_kind": "shopping",
                "target_list": "Покупки",
                "title": "Молоко",
                "notes": null,
                "due_date": "2026-10-30",
                "due_time": null,
                "priority": "normal",
                "assignee": null,
                "tags": ["продукты"],
                "quantity": 2,
                "unit": "л",
                "subtasks": [],
                "recurrence": null
            }],
            "clarifications": []
        });

        let parsed = parse_response(&data).unwrap();

        assert_eq!(parsed.items.len(), 1);
        let item = &parsed.items[0];
        assert_eq!(item.target_kind, ListKind::Shopping);
        assert_eq!(item.target_list.as_deref(), Some("Покупки"));
        assert_eq!(item.title, "Молоко");
        assert_eq!(item.due_date.as_deref(), Some("2026-10-30"));
        assert_eq!(item.due_time, None);
        assert_eq!(item.tags, ["продукты"]);
        assert_eq!(item.quantity, Some(2.0));
        assert_eq!(item.unit.as_deref(), Some("л"));
        assert!(parsed.clarifications.is_empty());
    }

    #[test]
    fn a_stray_all_day_field_is_ignored_whatever_its_type() {
        // Поля `all_day` в схеме нет, но модель или старый кэш могут его вернуть.
        for value in [json!(true), json!(false), json!("yes"), json!(null)] {
            let item = parse_one(json!({"all_day": value, "due_date": "2026-10-31"})).unwrap();
            assert_eq!(item.due_date.as_deref(), Some("2026-10-31"));
            assert_eq!(item.due_time, None);
        }
    }

    #[test]
    fn only_kind_and_title_are_required() {
        let item = parse_one(json!({})).unwrap();

        assert_eq!(item.target_kind, ListKind::Tasks);
        assert_eq!(item.title, "Позвонить маме");
        assert_eq!(item.target_list, None);
        assert_eq!(item.priority, None);
        assert!(item.tags.is_empty());
        assert!(item.subtasks.is_empty());
        assert_eq!(item.recurrence, None);
    }

    #[test]
    fn unknown_fields_and_identifiers_are_ignored() {
        let data = json!({
            "household_id": "00000000-0000-0000-0000-000000000001",
            "items": [item(json!({
                "id": "a", "item_id": "b", "household_id": "c",
                "list_id": "d", "action": "delete_all", "status": "done"
            }))],
            "debug": {"anything": true}
        });

        let parsed = parse_response(&data).unwrap();

        assert_eq!(parsed.items.len(), 1);
        assert_eq!(parsed.items[0].title, "Позвонить маме");
    }

    #[test]
    fn missing_or_malformed_items_are_rejected() {
        assert_eq!(parse_response(&json!([])), Err(ParseError::NotAnObject));
        assert_eq!(parse_response(&json!("x")), Err(ParseError::NotAnObject));
        assert_eq!(parse_response(&json!({})), Err(ParseError::MissingItems));
        assert_eq!(
            parse_response(&json!({"items": null})),
            Err(ParseError::MissingItems)
        );
        assert_eq!(
            parse_response(&json!({"items": {"title": "x"}})),
            Err(ParseError::InvalidField {
                index: None,
                field: "items"
            })
        );
        assert_eq!(
            parse_response(&json!({"items": ["строка"]})),
            Err(ParseError::InvalidField {
                index: Some(0),
                field: "item"
            })
        );
    }

    #[test]
    fn empty_items_array_is_a_valid_empty_response() {
        let parsed = parse_response(&json!({"items": []})).unwrap();

        assert!(parsed.items.is_empty());
    }

    #[test]
    fn missing_required_item_fields_name_the_item() {
        let no_title = json!({"items": [item(json!({})), {"target_kind": "tasks"}]});
        assert_eq!(
            parse_response(&no_title),
            Err(ParseError::MissingField {
                index: 1,
                field: "title"
            })
        );

        let no_kind = json!({"items": [{"title": "x"}]});
        assert_eq!(
            parse_response(&no_kind),
            Err(ParseError::MissingField {
                index: 0,
                field: "target_kind"
            })
        );
    }

    #[test]
    fn unknown_kind_and_wrong_types_are_invalid_fields() {
        let cases = [
            (json!({"target_kind": "todo"}), "target_kind"),
            (json!({"target_kind": 1}), "target_kind"),
            (json!({"title": 5}), "title"),
            (json!({"notes": ["a"]}), "notes"),
            (json!({"quantity": "много"}), "quantity"),
            (json!({"tags": "продукты"}), "tags"),
            (json!({"tags": [1]}), "tags"),
            (json!({"subtasks": [{"title": 1}]}), "subtasks"),
            (json!({"recurrence": "weekly"}), "recurrence"),
            (json!({"recurrence": {"interval": "1"}}), "interval"),
            (json!({"recurrence": {"weekdays": [2.5]}}), "weekdays"),
        ];
        for (extra, field) in cases {
            assert_eq!(
                parse_one(extra.clone()),
                Err(ParseError::InvalidField {
                    index: Some(0),
                    field
                }),
                "{extra}"
            );
        }
    }

    #[test]
    fn kind_is_case_and_space_insensitive() {
        let item = parse_one(json!({"target_kind": " Shopping "})).unwrap();

        assert_eq!(item.target_kind, ListKind::Shopping);
    }

    #[test]
    fn empty_title_is_kept_for_domain_validation() {
        let item = parse_one(json!({"title": ""})).unwrap();

        assert_eq!(item.title, "");
    }

    #[test]
    fn blank_optional_strings_become_none() {
        let item = parse_one(json!({"notes": "  ", "assignee": "", "due_date": ""})).unwrap();

        assert_eq!(item.notes, None);
        assert_eq!(item.assignee, None);
        assert_eq!(item.due_date, None);
    }

    #[test]
    fn exactly_twenty_items_pass_and_twenty_one_do_not() {
        let twenty: Vec<Value> = (0..20).map(|_| item(json!({}))).collect();
        assert_eq!(
            parse_response(&json!({"items": twenty}))
                .unwrap()
                .items
                .len(),
            20
        );

        let twenty_one: Vec<Value> = (0..21).map(|_| item(json!({}))).collect();
        assert_eq!(
            parse_response(&json!({"items": twenty_one})),
            Err(ParseError::TooManyItems { count: 21 })
        );
    }

    #[test]
    fn home_assistant_wrapper_objects_are_normalized() {
        let item = parse_one(json!({
            "tags": [{"value": "продукты"}, "семья", {"value": ""}, {}],
            "subtasks": [{"title": "Достать"}, "Позвонить", {"title": " "}],
        }))
        .unwrap();

        assert_eq!(item.tags, ["продукты", "семья"]);
        assert_eq!(item.subtasks, ["Достать", "Позвонить"]);
    }

    #[test]
    fn recurrence_wrappers_and_whole_numbers_are_normalized() {
        let item = parse_one(json!({
            "recurrence": {
                "freq": "weekly",
                "interval": 1.0,
                "weekdays": [{"day": 2}, 4.0],
                "until": null
            }
        }))
        .unwrap();

        assert_eq!(
            item.recurrence,
            Some(RawRecurrence {
                freq: Some("weekly".to_owned()),
                interval: Some(1),
                month_rule: None,
                weekdays: vec![2, 4],
                until: None,
            })
        );
    }

    #[test]
    fn empty_recurrence_object_means_no_recurrence() {
        let item = parse_one(json!({"recurrence": {}})).unwrap();
        assert_eq!(item.recurrence, None);

        let item = parse_one(json!({"recurrence": {"weekdays": [], "freq": ""}})).unwrap();
        assert_eq!(item.recurrence, None);
    }

    #[test]
    fn recurrence_without_freq_is_kept_for_validation() {
        let item = parse_one(json!({"recurrence": {"interval": 2}})).unwrap();

        let recurrence = item.recurrence.unwrap();
        assert_eq!(recurrence.freq, None);
        assert_eq!(recurrence.interval, Some(2));
    }

    #[test]
    fn out_of_range_numbers_survive_for_domain_validation() {
        let item = parse_one(json!({
            "quantity": 100000,
            "recurrence": {"freq": "daily", "interval": 0, "weekdays": [9]}
        }))
        .unwrap();

        assert_eq!(item.quantity, Some(100_000.0));
        let recurrence = item.recurrence.unwrap();
        assert_eq!(recurrence.interval, Some(0));
        assert_eq!(recurrence.weekdays, [9]);
    }

    #[test]
    fn clarifications_are_normalized_and_capped_at_three() {
        let data = json!({
            "items": [],
            "clarifications": [
                {"question": "Какой Алексей?"}, "Когда?", {"question": ""},
                {"question": "Где?"}, "Зачем?", "Сколько?"
            ]
        });

        let parsed = parse_response(&data).unwrap();

        assert_eq!(parsed.clarifications, ["Какой Алексей?", "Когда?", "Где?"]);
    }

    #[test]
    fn malformed_clarifications_do_not_discard_items() {
        for clarifications in [json!("Когда?"), json!([5]), json!([{"question": 1}])] {
            let data = json!({
                "items": [{"target_kind": "tasks", "title": "Позвонить"}],
                "clarifications": clarifications
            });

            let parsed = parse_response(&data).unwrap();

            assert_eq!(parsed.items.len(), 1);
            assert!(parsed.clarifications.is_empty());
        }
    }

    #[test]
    fn absent_clarifications_are_empty() {
        let parsed = parse_response(&json!({"items": []})).unwrap();

        assert!(parsed.clarifications.is_empty());
    }

    #[test]
    fn error_codes_and_messages_do_not_leak_content() {
        let error =
            parse_response(&json!({"items": [{"target_kind": "tasks", "title": 5}]})).unwrap_err();

        assert_eq!(error.code(), "invalid_field");
        assert_eq!(error.to_string(), "invalid title");
        assert_eq!(
            ParseError::TooManyItems { count: 21 }.code(),
            "too_many_items"
        );
    }
}
