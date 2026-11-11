//! Общие типы элемента черновика и уточнений.
//!
//! Ответ `capture-v4` разбирает [`super::wire`], а здесь остались структуры,
//! которые переживают разбор: [`RawItem`] хранится в `capture_drafts.parsed`,
//! [`Clarification`] — в черновике и ответе, [`ParseError`] — общий код ошибки
//! формы для журнала и оценки.

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use super::context::CaptureContext;
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

/// Ошибка выражения даты, которую не удалось превратить в календарную дату.
///
/// Хранится рядом с элементом черновика, а не рядом с ответом целиком: ошибка
/// одного выражения не должна ронять остальные элементы. Валидатор показывает
/// её пользователю как ошибку даты, и элемент не сохраняется до исправления.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RawDateError {
    /// Поле элемента, к которому относится ошибка: `due_date`, `end_date`,
    /// `recurrence` или `note_date`.
    pub field: String,
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
    /// Элемент на весь день: время не задаётся, даже если модель или прежний
    /// черновик его назвали. Признак появляется при ответе «Весь день» на
    /// вопрос о времени.
    #[serde(default)]
    pub all_day: bool,
    /// Дата конца события `YYYY-MM-DD`, последний день включительно.
    #[serde(default)]
    pub end_date: Option<String>,
    /// Время конца события `HH:MM`.
    #[serde(default)]
    pub end_time: Option<String>,
    /// Ошибки выражений дат, если их не удалось посчитать. Хранятся все:
    /// исправление срока не должно молча терять ошибку второй даты.
    #[serde(default)]
    pub date_error: Vec<RawDateError>,
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

/// Уточнение: номер элемента `items` и тема вопроса.
///
/// Сервер сам решает, задавать ли вопрос: уточнение о несуществующем элементе
/// или о уже заполненном поле отбрасывается при сохранении черновика.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Clarification {
    /// Номер элемента `items` с нуля.
    pub item: usize,
    /// Тема вопроса.
    pub topic: ClarificationTopic,
}

/// Тема уточняющего вопроса; значения совпадают с
/// [`super::contract::CLARIFICATION_TOPICS`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClarificationTopic {
    /// Дата.
    Date,
    /// Время.
    Time,
    /// Исполнитель или участник.
    Assignee,
    /// Список.
    List,
}

impl ClarificationTopic {
    /// Короткий текст вопроса на русском: его показывают Telegram и внешний
    /// capture.
    pub fn question(self) -> &'static str {
        match self {
            Self::Date => "Когда?",
            Self::Time => "Во сколько?",
            Self::Assignee => "Кому поручить?",
            Self::List => "В какой список?",
        }
    }

    /// Значение темы в контракте `capture-v4`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Date => "date",
            Self::Time => "time",
            Self::Assignee => "assignee",
            Self::List => "list",
        }
    }

    /// Поле элемента уже заполнено, поэтому уточнение закрыто.
    ///
    /// `context` нужен только вопросу о списке: имя считается названным, лишь
    /// когда такой список есть в household. Если контекста нет (уточнения
    /// сохраняются при создании черновика раньше проверки), имя не закрывает
    /// вопрос: неизвестный список не должен молча превращаться в ответ.
    pub fn is_filled(self, item: &RawItem, context: Option<&CaptureContext>) -> bool {
        let named = |value: &Option<String>| {
            value
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty())
        };
        match self {
            Self::Date => item.due_date.is_some(),
            Self::Time => item.due_time.is_some() || item.all_day,
            Self::Assignee => named(&item.assignee) || item.assignee_id.is_some(),
            Self::List => {
                let Some(context) = context else {
                    return false;
                };
                let Some(name) = item
                    .target_list
                    .as_deref()
                    .filter(|name| !name.trim().is_empty())
                else {
                    return false;
                };
                context.find_list(item.target_kind, name).is_some()
            }
        }
    }
}

/// Оставить уточнения только о существующих элементах с незаполненным полем,
/// без повторов пары «элемент, тема» и не больше [`MAX_CLARIFICATIONS`] штук.
///
/// Одна и та же проверка нужна и ответу внешнего capture (до сохранения
/// черновика), и самому черновику при создании, дописывании и правке.
pub fn normalize_clarifications(
    items: &[RawItem],
    clarifications: Vec<Clarification>,
    context: Option<&CaptureContext>,
) -> Vec<Clarification> {
    let mut kept = Vec::new();
    for clarification in clarifications {
        if kept.len() == MAX_CLARIFICATIONS {
            break;
        }
        let Some(item) = items.get(clarification.item) else {
            continue;
        };
        // Модель иногда повторяет один и тот же вопрос: три одинаковых
        // уточнения не должны занимать весь лимит.
        if kept.contains(&clarification) {
            continue;
        }
        if clarification.topic.is_filled(item, context) {
            continue;
        }
        kept.push(clarification);
    }
    kept
}

/// Разобранный ответ LLM в форме, которую ждут валидатор и черновик.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParsedResponse {
    /// Элементы, не более [`MAX_ITEMS`].
    pub items: Vec<RawItem>,
    /// Уточнения, не более [`MAX_CLARIFICATIONS`].
    pub clarifications: Vec<Clarification>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn raw_item(kind: ListKind, title: &str) -> RawItem {
        RawItem {
            target_kind: kind,
            target_list: None,
            title: title.to_owned(),
            notes: None,
            due_date: None,
            due_time: None,
            all_day: false,
            end_date: None,
            end_time: None,
            date_error: Vec::new(),
            priority: None,
            assignee: None,
            assignee_id: None,
            tags: Vec::new(),
            quantity: None,
            unit: None,
            subtasks: Vec::new(),
            recurrence: None,
        }
    }

    #[test]
    fn a_draft_saved_before_the_event_fields_still_reads() {
        // Черновик прежней версии не содержит `end_date`, `end_time` и
        // `date_error`: они должны читаться значениями по умолчанию.
        let old = json!({
            "target_kind": "tasks",
            "target_list": null,
            "title": "Позвонить маме",
            "notes": null,
            "due_date": "2026-11-11",
            "due_time": null,
            "priority": null,
            "assignee": null,
            "assignee_id": null,
            "tags": [],
            "quantity": null,
            "unit": null,
            "subtasks": [],
            "recurrence": null,
        });

        let item: RawItem = serde_json::from_value(old).expect("old draft reads");

        assert_eq!(item.title, "Позвонить маме");
        assert_eq!(item.due_date.as_deref(), Some("2026-11-11"));
        assert_eq!(item.end_date, None);
        assert_eq!(item.end_time, None);
        assert!(item.date_error.is_empty());
    }

    #[test]
    fn an_empty_date_error_array_reads_as_no_errors() {
        let mut draft = json!({
            "target_kind": "tasks",
            "target_list": null,
            "title": "Позвонить маме",
            "notes": null,
            "due_date": null,
            "due_time": null,
            "priority": null,
            "assignee": null,
            "assignee_id": null,
            "tags": [],
            "quantity": null,
            "unit": null,
            "subtasks": [],
            "recurrence": null,
            "date_error": [],
        });
        let item: RawItem = serde_json::from_value(draft.clone()).expect("draft reads");
        assert!(item.date_error.is_empty());

        draft["date_error"] = json!([{"field": "note_date"}]);
        let item: RawItem = serde_json::from_value(draft).expect("draft reads");
        assert_eq!(
            item.date_error,
            vec![RawDateError {
                field: "note_date".to_owned()
            }]
        );
    }

    #[test]
    fn a_draft_without_date_errors_reads() {
        let draft = json!({
            "target_kind": "tasks",
            "target_list": null,
            "title": "Позвонить маме",
            "notes": null,
            "due_date": null,
            "due_time": null,
            "end_date": null,
            "end_time": null,
            "priority": null,
            "assignee": null,
            "assignee_id": null,
            "tags": [],
            "quantity": null,
            "unit": null,
            "subtasks": [],
            "recurrence": null,
        });

        let item: RawItem = serde_json::from_value(draft).expect("draft reads");

        assert!(item.date_error.is_empty());
        assert!(!item.all_day, "an old draft is not all-day");
    }

    #[test]
    fn duplicate_clarifications_do_not_fill_the_limit() {
        let item = raw_item(ListKind::Tasks, "Позвонить");
        let duplicate = Clarification {
            item: 0,
            topic: ClarificationTopic::Date,
        };

        let normalized = normalize_clarifications(&[item], vec![duplicate; 3], None);

        assert_eq!(
            normalized,
            [duplicate],
            "three identical questions are one question"
        );
    }

    #[test]
    fn an_unknown_list_name_does_not_close_the_list_question() {
        let context = crate::capture::test_support::context();
        let question = Clarification {
            item: 0,
            topic: ClarificationTopic::List,
        };

        let mut unknown = raw_item(ListKind::Tasks, "Позвонить");
        unknown.target_list = Some("Несуществующий".to_owned());
        assert_eq!(
            normalize_clarifications(
                std::slice::from_ref(&unknown),
                vec![question],
                Some(&context)
            ),
            [question],
            "a name that is not a household list leaves the question open"
        );

        let mut known = raw_item(ListKind::Tasks, "Позвонить");
        known.target_list = Some("Дача".to_owned());
        assert!(
            normalize_clarifications(std::slice::from_ref(&known), vec![question], Some(&context))
                .is_empty(),
            "a real list of the right kind closes the question"
        );

        let mut other_kind = raw_item(ListKind::Tasks, "Позвонить");
        other_kind.target_list = Some("Покупки".to_owned());
        assert_eq!(
            normalize_clarifications(
                std::slice::from_ref(&other_kind),
                vec![question],
                Some(&context)
            ),
            [question],
            "a list of another kind does not answer the question"
        );
    }

    #[test]
    fn an_all_day_item_closes_the_time_question() {
        let mut item = raw_item(ListKind::Tasks, "Позвонить");
        item.due_date = Some("2026-11-11".to_owned());
        item.all_day = true;

        assert!(ClarificationTopic::Time.is_filled(&item, None));
    }

    #[test]
    fn error_codes_do_not_leak_content() {
        assert_eq!(
            ParseError::TooManyItems { count: 21 }.code(),
            "too_many_items"
        );
        assert_eq!(
            ParseError::InvalidField {
                index: Some(0),
                field: "title"
            }
            .to_string(),
            "invalid title"
        );
    }
}
