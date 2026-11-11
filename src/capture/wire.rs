//! Разбор ответа `capture-v4` в типизированные элементы с выражениями дат.
//!
//! Ответ — объект `{items, clarifications}`. Элемент — enum с тегом по `kind`:
//! задача, покупка или пункт чек-листа (общая форма) и событие. Все поля формы
//! обязательны, неизвестные поля запрещены, поэтому тихие компенсации прежнего
//! парсера (`{value: …}`, регистр перечней, строка вместо числа) здесь не
//! работают: такой ответ — ошибка разбора.
//!
//! Разбор проверяет только то, что схема выражает перечнями: `list` из списков
//! подходящего форме вида или пустая строка, `assignee` из имён участников или
//! пустая строка. Даты здесь не считаются: выражения превращает в строковые
//! даты [`super::convert`] по правилам [`super::dates`], а готовый черновик
//! проверяет [`super::validate`].

use serde::Deserialize;
use serde::de::Error as _;
use serde_json::{Map, Value};

use super::context::CaptureContext;
use super::contract::{FIELD_ORDER, FORMS, Field, Form, FormSpec};
use super::schema::{MAX_CLARIFICATIONS, MAX_ITEMS, ParseError};
use crate::domain::models::{ItemPriority, ListKind, MonthRule, RecurrenceFreq};

/// Разобранный ответ `capture-v4`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CaptureResponse {
    /// Элементы, не более [`MAX_ITEMS`].
    pub items: Vec<Item>,
    /// Уточнения, не более [`MAX_CLARIFICATIONS`].
    pub clarifications: Vec<Clarification>,
}

/// Элемент ответа. `shopping` и `checklist` используют одну форму.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "kind")]
pub enum Item {
    /// Задача.
    #[serde(rename = "tasks")]
    Task(TaskItem),
    /// Покупка.
    #[serde(rename = "shopping")]
    Shopping(ShoppingItem),
    /// Пункт чек-листа.
    #[serde(rename = "checklist")]
    Checklist(ShoppingItem),
    /// Событие.
    #[serde(rename = "events")]
    Event(EventItem),
}

impl Item {
    /// Форма элемента из таблицы [`FORMS`].
    pub fn form(&self) -> Form {
        match self {
            Self::Task(_) => Form::Task,
            Self::Shopping(_) | Self::Checklist(_) => Form::Shopping,
            Self::Event(_) => Form::Event,
        }
    }

    /// Вид списка элемента.
    pub fn kind(&self) -> ListKind {
        match self {
            Self::Task(_) => ListKind::Tasks,
            Self::Shopping(_) => ListKind::Shopping,
            Self::Checklist(_) => ListKind::Checklist,
            Self::Event(_) => ListKind::Events,
        }
    }
}

/// Задача.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskItem {
    /// Название списка; пусто — список не назван.
    #[serde(deserialize_with = "empty_as_none")]
    pub list: Option<String>,
    /// Название элемента.
    pub title: String,
    /// Срок.
    pub date: DateExpr,
    /// Время срока; пусто — не названо.
    #[serde(deserialize_with = "empty_as_none")]
    pub time: Option<String>,
    /// Повторение.
    pub repeat: Option<Repeat>,
    /// Исполнитель; пусто — не назван.
    #[serde(deserialize_with = "empty_as_none")]
    pub assignee: Option<String>,
    /// Приоритет; пусто — не назван.
    #[serde(deserialize_with = "priority_or_none")]
    pub priority: Option<ItemPriority>,
    /// Названия подзадач.
    pub subtasks: Vec<String>,
    /// Теги.
    pub tags: Vec<String>,
    /// Заметка; пусто — не названа.
    #[serde(deserialize_with = "empty_as_none")]
    pub notes: Option<String>,
    /// Вторая дата фразы.
    pub note_date: DateExpr,
}

/// Покупка или пункт чек-листа.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShoppingItem {
    /// Название списка; пусто — список не назван.
    #[serde(deserialize_with = "empty_as_none")]
    pub list: Option<String>,
    /// Название элемента.
    pub title: String,
    /// Срок.
    pub date: DateExpr,
    /// Время срока; пусто — не названо.
    #[serde(deserialize_with = "empty_as_none")]
    pub time: Option<String>,
    /// Исполнитель; пусто — не назван.
    #[serde(deserialize_with = "empty_as_none")]
    pub assignee: Option<String>,
    /// Приоритет; пусто — не назван.
    #[serde(deserialize_with = "priority_or_none")]
    pub priority: Option<ItemPriority>,
    /// Количество; `0` — не названо.
    #[serde(deserialize_with = "quantity_or_none")]
    pub quantity: Option<f64>,
    /// Единица; пусто — не названа.
    #[serde(deserialize_with = "empty_as_none")]
    pub unit: Option<String>,
    /// Теги.
    pub tags: Vec<String>,
    /// Заметка; пусто — не названа.
    #[serde(deserialize_with = "empty_as_none")]
    pub notes: Option<String>,
}

/// Событие.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventItem {
    /// Название списка; пусто — список не назван.
    #[serde(deserialize_with = "empty_as_none")]
    pub list: Option<String>,
    /// Название элемента.
    pub title: String,
    /// Начало.
    pub date: DateExpr,
    /// Время начала; пусто — не названо.
    #[serde(deserialize_with = "empty_as_none")]
    pub time: Option<String>,
    /// Дата конца.
    pub end: DateExpr,
    /// Время конца; пусто — не названо.
    #[serde(deserialize_with = "empty_as_none")]
    pub end_time: Option<String>,
    /// Повторение.
    pub repeat: Option<Repeat>,
    /// Участник; пусто — не назван.
    #[serde(deserialize_with = "empty_as_none")]
    pub assignee: Option<String>,
    /// Теги.
    pub tags: Vec<String>,
    /// Заметка; пусто — не названа.
    #[serde(deserialize_with = "empty_as_none")]
    pub notes: Option<String>,
}

/// Правило повторения.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Repeat {
    /// Частота.
    pub freq: RecurrenceFreq,
    /// Интервал.
    pub interval: i64,
    /// Дни недели ISO, 1–7.
    pub weekdays: Vec<i64>,
    /// Правило месячного повторения; пусто — не названо.
    #[serde(deserialize_with = "month_rule_or_none")]
    pub month_rule: Option<MonthRule>,
    /// Дата окончания серии.
    pub until: DateExpr,
}

/// Плоское выражение даты; неиспользуемые поля — ноль или пустая строка.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DateExpr {
    /// Вид выражения.
    pub kind: DateKind,
    /// Смещение, номер дня или признак «следующий» у дня недели и недели.
    pub n: i64,
    /// Единица смещения.
    pub unit: DateUnit,
    /// День недели.
    pub weekday: Weekday,
    /// Число месяца.
    pub day: i64,
    /// Месяц.
    pub month: i64,
    /// Год.
    pub year: i64,
}

/// Вид выражения даты.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DateKind {
    /// Дата не названа.
    None,
    /// Смещение в единицах.
    Offset,
    /// День недели.
    Weekday,
    /// Число, месяц и год.
    Date,
    /// Выходные.
    Weekend,
    /// Неделя.
    Week,
    /// Начало месяца.
    MonthStart,
    /// Конец месяца.
    MonthEnd,
}

/// Единица смещения.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum DateUnit {
    /// Не названа.
    #[serde(rename = "")]
    NotSet,
    /// Дни.
    #[serde(rename = "days")]
    Days,
    /// Недели.
    #[serde(rename = "weeks")]
    Weeks,
    /// Месяцы.
    #[serde(rename = "months")]
    Months,
    /// Годы.
    #[serde(rename = "years")]
    Years,
}

/// День недели.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum Weekday {
    /// Не назван.
    #[serde(rename = "")]
    NotSet,
    /// Понедельник.
    #[serde(rename = "monday")]
    Monday,
    /// Вторник.
    #[serde(rename = "tuesday")]
    Tuesday,
    /// Среда.
    #[serde(rename = "wednesday")]
    Wednesday,
    /// Четверг.
    #[serde(rename = "thursday")]
    Thursday,
    /// Пятница.
    #[serde(rename = "friday")]
    Friday,
    /// Суббота.
    #[serde(rename = "saturday")]
    Saturday,
    /// Воскресенье.
    #[serde(rename = "sunday")]
    Sunday,
}

/// Уточнение: номер элемента и тема вопроса.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Clarification {
    /// Номер элемента `items` с нуля.
    pub item: usize,
    /// Тема вопроса.
    pub topic: ClarificationTopic,
}

/// Тема уточняющего вопроса; значения совпадают с
/// [`CLARIFICATION_TOPICS`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
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

/// Разобрать текст ответа `capture-v4`.
///
/// Не JSON и не объект дают [`ParseError::NotAnObject`]. Ошибки формы не
/// содержат текст ответа: только номер элемента и имя поля.
pub fn parse_capture_response(
    text: &str,
    context: &CaptureContext,
) -> Result<CaptureResponse, ParseError> {
    let value: Value = serde_json::from_str(text).map_err(|_| ParseError::NotAnObject)?;
    let root = value.as_object().ok_or(ParseError::NotAnObject)?;

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
        .map(|(index, value)| parse_item(index, value, context))
        .collect::<Result<Vec<_>, _>>()?;

    // Вопросы — вторичное поле: испорченное значение не должно отбрасывать
    // валидные элементы и запускать повтор запроса к LLM.
    Ok(CaptureResponse {
        items,
        clarifications: parse_clarifications(root.get("clarifications")),
    })
}

fn parse_item(index: usize, value: &Value, context: &CaptureContext) -> Result<Item, ParseError> {
    let object = value.as_object().ok_or(ParseError::InvalidField {
        index: Some(index),
        field: "item",
    })?;

    let kind = match object.get("kind") {
        Some(Value::String(kind)) => kind.as_str(),
        Some(_) => {
            return Err(ParseError::InvalidField {
                index: Some(index),
                field: "kind",
            });
        }
        None => {
            return Err(ParseError::MissingField {
                index,
                field: "kind",
            });
        }
    };
    let form = form_of_kind(kind).ok_or(ParseError::InvalidField {
        index: Some(index),
        field: "kind",
    })?;
    let spec = FORMS
        .iter()
        .find(|spec| spec.form == form)
        .expect("form is in the contract table");
    check_fields(object, index, spec)?;
    check_selections(object, index, spec.form, context)?;

    // Основной разбор — serde; диагностика ниже нужна только затем, чтобы
    // ошибка типа называла поле: сообщение serde этого не сообщает.
    serde_json::from_value(value.clone()).map_err(|_| diagnose_item(object, index, spec, context))
}

/// Проверить значения, которые схема ограничивает перечнями из контекста.
fn check_selections(
    object: &Map<String, Value>,
    index: usize,
    form: Form,
    context: &CaptureContext,
) -> Result<(), ParseError> {
    let list = object
        .get(Field::List.name())
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !list.is_empty() && !list_is_allowed(context, form, list) {
        return Err(ParseError::InvalidField {
            index: Some(index),
            field: Field::List.name(),
        });
    }

    let assignee = object
        .get(Field::Assignee.name())
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !assignee.is_empty() && !member_is_known(context, assignee) {
        return Err(ParseError::InvalidField {
            index: Some(index),
            field: Field::Assignee.name(),
        });
    }

    Ok(())
}

/// Проверить, что в объекте только поля формы и все они присутствуют.
fn check_fields(
    object: &Map<String, Value>,
    index: usize,
    spec: &FormSpec,
) -> Result<(), ParseError> {
    for key in object.keys() {
        if key == Field::Kind.name() {
            continue;
        }
        if spec.fields.iter().any(|field| field.name() == key) {
            continue;
        }
        // Поле чужой формы называем его контрактным именем; у совсем
        // неизвестного имени статичного имени нет.
        let field = known_field(key).map_or("item", Field::name);
        return Err(ParseError::InvalidField {
            index: Some(index),
            field,
        });
    }
    for field in spec.fields {
        if *field != Field::Kind && !object.contains_key(field.name()) {
            return Err(ParseError::MissingField {
                index,
                field: field.name(),
            });
        }
    }
    Ok(())
}

fn diagnose_item(
    object: &Map<String, Value>,
    index: usize,
    spec: &FormSpec,
    context: &CaptureContext,
) -> ParseError {
    for field in spec.fields {
        if let Some(error) = diagnose_field(object, index, spec.form, *field, context) {
            return error;
        }
    }
    ParseError::InvalidField {
        index: Some(index),
        field: "item",
    }
}

fn diagnose_field(
    object: &Map<String, Value>,
    index: usize,
    form: Form,
    field: Field,
    context: &CaptureContext,
) -> Option<ParseError> {
    let value = object.get(field.name())?;
    let invalid = || ParseError::InvalidField {
        index: Some(index),
        field: field.name(),
    };
    match field {
        Field::Kind => None,
        Field::List => match value.as_str() {
            Some("") => None,
            Some(name) if list_is_allowed(context, form, name) => None,
            _ => Some(invalid()),
        },
        Field::Assignee => match value.as_str() {
            Some("") => None,
            Some(name) if member_is_known(context, name) => None,
            _ => Some(invalid()),
        },
        Field::Priority => match value.as_str() {
            Some("") => None,
            Some(name) if priority_from_name(name).is_some() => None,
            _ => Some(invalid()),
        },
        Field::Date | Field::End | Field::NoteDate => {
            serde_json::from_value::<DateExpr>(value.clone())
                .err()
                .map(|_| invalid())
        }
        Field::Repeat => match value {
            Value::Null => None,
            _ => serde_json::from_value::<Repeat>(value.clone())
                .err()
                .map(|_| invalid()),
        },
        Field::Quantity => serde_json::from_value::<f64>(value.clone())
            .err()
            .map(|_| invalid()),
        Field::Subtasks | Field::Tags => serde_json::from_value::<Vec<String>>(value.clone())
            .err()
            .map(|_| invalid()),
        Field::Title | Field::Time | Field::EndTime | Field::Unit | Field::Notes => {
            value.as_str().is_none().then(invalid)
        }
    }
}

fn list_is_allowed(context: &CaptureContext, form: Form, name: &str) -> bool {
    context
        .lists
        .iter()
        .any(|list| form.list_kinds().contains(&list.kind) && list.name == name)
}

fn member_is_known(context: &CaptureContext, name: &str) -> bool {
    context
        .members
        .iter()
        .any(|member| member.display_name == name)
}

fn form_of_kind(kind: &str) -> Option<Form> {
    match kind {
        "tasks" => Some(Form::Task),
        "shopping" | "checklist" => Some(Form::Shopping),
        "events" => Some(Form::Event),
        _ => None,
    }
}

fn known_field(name: &str) -> Option<Field> {
    FIELD_ORDER
        .iter()
        .copied()
        .find(|field| field.name() == name)
}

fn priority_from_name(name: &str) -> Option<ItemPriority> {
    match name {
        "low" => Some(ItemPriority::Low),
        "normal" => Some(ItemPriority::Normal),
        "high" => Some(ItemPriority::High),
        "urgent" => Some(ItemPriority::Urgent),
        _ => None,
    }
}

fn month_rule_from_name(name: &str) -> Option<MonthRule> {
    match name {
        "anchor_day" => Some(MonthRule::AnchorDay),
        "last_day" => Some(MonthRule::LastDay),
        _ => None,
    }
}

/// Строка; пустая строка — «не названо».
fn empty_as_none<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let text = String::deserialize(deserializer)?;
    Ok((!text.is_empty()).then_some(text))
}

/// Приоритет; пустая строка — «не назван».
fn priority_or_none<'de, D>(deserializer: D) -> Result<Option<ItemPriority>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let text = String::deserialize(deserializer)?;
    if text.is_empty() {
        return Ok(None);
    }
    priority_from_name(&text)
        .map(Some)
        .ok_or_else(|| D::Error::custom("invalid priority"))
}

/// Количество; ноль — «не названо».
fn quantity_or_none<'de, D>(deserializer: D) -> Result<Option<f64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let quantity = f64::deserialize(deserializer)?;
    Ok((quantity != 0.0).then_some(quantity))
}

/// Правило месяца; пустая строка — «не названо».
fn month_rule_or_none<'de, D>(deserializer: D) -> Result<Option<MonthRule>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let text = String::deserialize(deserializer)?;
    if text.is_empty() {
        return Ok(None);
    }
    month_rule_from_name(&text)
        .map(Some)
        .ok_or_else(|| D::Error::custom("invalid month rule"))
}

/// Уточнения — вторичное поле: не-массив и испорченные записи пропускаются,
/// валидные элементы ответа сохраняются. Лимит считается по разобранным
/// записям: лишние сверх [`MAX_CLARIFICATIONS`] отбрасываются.
fn parse_clarifications(value: Option<&Value>) -> Vec<Clarification> {
    let entries = match value {
        Some(Value::Array(entries)) => entries,
        Some(Value::Null) | None | Some(_) => return Vec::new(),
    };
    entries
        .iter()
        .filter_map(|entry| serde_json::from_value::<Clarification>(entry.clone()).ok())
        .take(MAX_CLARIFICATIONS)
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::capture::contract;
    use crate::capture::test_support::context;

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

    fn task() -> Value {
        json!({
            "kind": "tasks",
            "list": "",
            "title": "Позвонить маме",
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

    fn shopping() -> Value {
        json!({
            "kind": "shopping",
            "list": "",
            "title": "Молоко",
            "date": date_none(),
            "time": "",
            "assignee": "",
            "priority": "",
            "quantity": 0,
            "unit": "",
            "tags": [],
            "notes": "",
        })
    }

    fn event() -> Value {
        json!({
            "kind": "events",
            "list": "",
            "title": "Маникюр",
            "date": date_none(),
            "time": "14:00",
            "end": date_none(),
            "end_time": "15:30",
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

    fn without(mut item: Value, key: &str) -> Value {
        item.as_object_mut().expect("item is an object").remove(key);
        item
    }

    fn response(items: Vec<Value>) -> String {
        json!({"items": items, "clarifications": []}).to_string()
    }

    fn parse(items: Vec<Value>) -> Result<CaptureResponse, ParseError> {
        parse_capture_response(&response(items), &context())
    }

    #[test]
    fn mixed_draft_has_a_kind_per_item() {
        let task = with(
            task(),
            "date",
            json!({"kind": "offset", "n": 1, "unit": "days", "weekday": "", "day": 0, "month": 0, "year": 0}),
        );
        let parsed = parse(vec![task, shopping()]).expect("mixed draft is valid");

        assert_eq!(parsed.items.len(), 2);
        match &parsed.items[0] {
            Item::Task(item) => {
                assert_eq!(item.title, "Позвонить маме");
                assert_eq!(item.date.kind, DateKind::Offset);
                assert_eq!(item.date.n, 1);
                assert_eq!(item.date.unit, DateUnit::Days);
            }
            other => panic!("not a task: {other:?}"),
        }
        match &parsed.items[1] {
            Item::Shopping(item) => {
                assert_eq!(item.title, "Молоко");
                assert_eq!(item.quantity, None);
                assert_eq!(item.unit, None);
            }
            other => panic!("not a shopping item: {other:?}"),
        }
        assert_eq!(parsed.items[0].kind(), ListKind::Tasks);
        assert_eq!(parsed.items[1].kind(), ListKind::Shopping);
    }

    #[test]
    fn checklist_uses_the_shopping_form() {
        let parsed = parse(vec![with(shopping(), "kind", json!("checklist"))]).expect("valid");

        assert_eq!(parsed.items[0].form(), Form::Shopping);
        assert_eq!(parsed.items[0].kind(), ListKind::Checklist);
        assert!(matches!(parsed.items[0], Item::Checklist(_)));
    }

    #[test]
    fn lists_must_belong_to_the_form_and_the_context() {
        let cases = [
            (with(task(), "list", json!("Аптека")), "task"),
            (with(task(), "list", json!("Покупки")), "task"),
            (with(task(), "list", json!("Дача ")), "task"),
            (with(shopping(), "list", json!("Задачи")), "shopping"),
            (with(event(), "list", json!("Покупки")), "event"),
        ];
        for (item, name) in cases {
            assert_eq!(
                parse(vec![item.clone()]),
                Err(ParseError::InvalidField {
                    index: Some(0),
                    field: "list"
                }),
                "{name}: {item}"
            );
        }

        let parsed = parse(vec![
            with(task(), "list", json!("Дача")),
            with(shopping(), "list", json!("Покупки")),
        ])
        .expect("lists of the right kind are allowed");
        match &parsed.items[0] {
            Item::Task(item) => assert_eq!(item.list.as_deref(), Some("Дача")),
            other => panic!("not a task: {other:?}"),
        }
        match &parsed.items[1] {
            Item::Shopping(item) => assert_eq!(item.list.as_deref(), Some("Покупки")),
            other => panic!("not a shopping item: {other:?}"),
        }
    }

    #[test]
    fn shopping_and_checklist_share_list_kinds() {
        let mut context = context();
        context.lists.push(crate::capture::test_support::list(
            "Уборка",
            ListKind::Checklist,
            false,
        ));
        let text = response(vec![
            with(shopping(), "list", json!("Уборка")),
            with(
                with(shopping(), "kind", json!("checklist")),
                "list",
                json!("Покупки"),
            ),
        ]);

        let parsed =
            parse_capture_response(&text, &context).expect("shopping form accepts both list kinds");
        assert_eq!(parsed.items[0].kind(), ListKind::Shopping);
        assert_eq!(parsed.items[1].kind(), ListKind::Checklist);
        match &parsed.items[0] {
            Item::Shopping(item) => assert_eq!(item.list.as_deref(), Some("Уборка")),
            other => panic!("not a shopping item: {other:?}"),
        }
        match &parsed.items[1] {
            Item::Checklist(item) => assert_eq!(item.list.as_deref(), Some("Покупки")),
            other => panic!("not a checklist item: {other:?}"),
        }
    }

    #[test]
    fn event_with_a_shopping_list_is_rejected() {
        let mut context = context();
        context.lists.push(crate::capture::test_support::list(
            "Аптека",
            ListKind::Shopping,
            false,
        ));
        let text = response(vec![with(event(), "list", json!("Аптека"))]);

        assert_eq!(
            parse_capture_response(&text, &context),
            Err(ParseError::InvalidField {
                index: Some(0),
                field: "list"
            })
        );
    }

    #[test]
    fn assignee_must_be_a_member() {
        assert_eq!(
            parse(vec![with(task(), "assignee", json!("Оля"))]),
            Err(ParseError::InvalidField {
                index: Some(0),
                field: "assignee"
            })
        );
        assert_eq!(
            parse(vec![with(task(), "assignee", json!("Анна "))]),
            Err(ParseError::InvalidField {
                index: Some(0),
                field: "assignee"
            })
        );

        let parsed = parse(vec![with(task(), "assignee", json!("Анна"))]).expect("member");
        match &parsed.items[0] {
            Item::Task(item) => assert_eq!(item.assignee.as_deref(), Some("Анна")),
            other => panic!("not a task: {other:?}"),
        }
    }

    #[test]
    fn quantity_on_task_is_a_parse_error() {
        assert_eq!(
            parse(vec![with(task(), "quantity", json!(1))]),
            Err(ParseError::InvalidField {
                index: Some(0),
                field: "quantity"
            })
        );
        assert_eq!(
            parse(vec![with(event(), "priority", json!("high"))]),
            Err(ParseError::InvalidField {
                index: Some(0),
                field: "priority"
            })
        );
    }

    #[test]
    fn zero_quantity_and_empty_unit_mean_not_named() {
        let parsed = parse(vec![
            with(shopping(), "quantity", json!(0)),
            with(with(shopping(), "quantity", json!(2.5)), "unit", json!("л")),
        ])
        .expect("quantities are valid");

        match &parsed.items[0] {
            Item::Shopping(item) => {
                assert_eq!(item.quantity, None);
                assert_eq!(item.unit, None);
            }
            other => panic!("not a shopping item: {other:?}"),
        }
        match &parsed.items[1] {
            Item::Shopping(item) => {
                assert_eq!(item.quantity, Some(2.5));
                assert_eq!(item.unit.as_deref(), Some("л"));
            }
            other => panic!("not a shopping item: {other:?}"),
        }
    }

    #[test]
    fn unknown_field_is_an_error_with_the_item_index() {
        assert_eq!(
            parse(vec![task(), with(task(), "all_day", json!(true))]),
            Err(ParseError::InvalidField {
                index: Some(1),
                field: "item"
            })
        );
    }

    #[test]
    fn unknown_fields_inside_nested_objects_are_rejected() {
        let date = with(date_none(), "extra", json!(1));
        assert_eq!(
            parse(vec![with(task(), "date", date)]),
            Err(ParseError::InvalidField {
                index: Some(0),
                field: "date"
            })
        );

        let repeat = json!({
            "freq": "weekly",
            "interval": 1,
            "weekdays": [],
            "month_rule": "",
            "until": date_none(),
            "extra": 1,
        });
        assert_eq!(
            parse(vec![with(task(), "repeat", repeat)]),
            Err(ParseError::InvalidField {
                index: Some(0),
                field: "repeat"
            })
        );
    }

    #[test]
    fn missing_and_null_required_fields_are_errors() {
        assert_eq!(
            parse(vec![task(), without(task(), "title")]),
            Err(ParseError::MissingField {
                index: 1,
                field: "title"
            })
        );
        assert_eq!(
            parse(vec![without(task(), "kind")]),
            Err(ParseError::MissingField {
                index: 0,
                field: "kind"
            })
        );
        assert_eq!(
            parse(vec![with(task(), "title", Value::Null)]),
            Err(ParseError::InvalidField {
                index: Some(0),
                field: "title"
            })
        );
        assert_eq!(
            parse(vec![without(task(), "repeat")]),
            Err(ParseError::MissingField {
                index: 0,
                field: "repeat"
            })
        );
    }

    #[test]
    fn unknown_kind_is_a_parse_error() {
        for kind in [json!("todo"), json!("Tasks"), json!(" tasks "), json!(1)] {
            assert_eq!(
                parse(vec![with(task(), "kind", kind.clone())]),
                Err(ParseError::InvalidField {
                    index: Some(0),
                    field: "kind"
                }),
                "{kind}"
            );
        }
    }

    #[test]
    fn v3_compensations_do_not_work() {
        // JSON Schema считает `1.0` целым (дробная часть нулевая), но контракт
        // v4 требует целое число без дробной части: это намеренное расхождение.
        let cases = [
            (with(task(), "tags", json!([{"value": "продукты"}])), "tags"),
            (with(task(), "kind", json!(" Tasks ")), "kind"),
            (with(shopping(), "quantity", json!("3")), "quantity"),
            (with(task(), "priority", json!("High")), "priority"),
            (
                with(
                    task(),
                    "date",
                    json!({"kind": "offset", "n": 1.0, "unit": "days", "weekday": "", "day": 0, "month": 0, "year": 0}),
                ),
                "date",
            ),
            (
                with(
                    task(),
                    "repeat",
                    json!({
                        "freq": "weekly",
                        "interval": 1.0,
                        "weekdays": [],
                        "month_rule": "",
                        "until": date_none(),
                    }),
                ),
                "repeat",
            ),
            (
                with(
                    task(),
                    "repeat",
                    json!({
                        "freq": "weekly",
                        "interval": 1,
                        "weekdays": [2.0],
                        "month_rule": "",
                        "until": date_none(),
                    }),
                ),
                "repeat",
            ),
        ];
        for (item, field) in cases {
            assert_eq!(
                parse(vec![item.clone()]),
                Err(ParseError::InvalidField {
                    index: Some(0),
                    field
                }),
                "{item}"
            );
        }
    }

    #[test]
    fn twenty_items_pass_and_twenty_one_fail() {
        let twenty = (0..20).map(|_| task()).collect::<Vec<_>>();
        assert_eq!(parse(twenty).expect("20 items fit").items.len(), 20);

        let twenty_one = (0..21).map(|_| task()).collect::<Vec<_>>();
        assert_eq!(
            parse(twenty_one),
            Err(ParseError::TooManyItems { count: 21 })
        );
    }

    #[test]
    fn empty_items_is_a_valid_empty_response() {
        let parsed = parse(Vec::new()).expect("empty response is valid");
        assert!(parsed.items.is_empty());
        assert!(parsed.clarifications.is_empty());

        let parsed = parse_capture_response(r#"{"items": []}"#, &context())
            .expect("missing clarifications are empty");
        assert!(parsed.items.is_empty());
        assert!(parsed.clarifications.is_empty());
    }

    #[test]
    fn malformed_root_is_rejected() {
        for text in ["not json", "[]", "null", "42"] {
            assert_eq!(
                parse_capture_response(text, &context()),
                Err(ParseError::NotAnObject),
                "{text}"
            );
        }
        for text in ["{}", r#"{"items": null}"#] {
            assert_eq!(
                parse_capture_response(text, &context()),
                Err(ParseError::MissingItems),
                "{text}"
            );
        }
        assert_eq!(
            parse_capture_response(r#"{"items": {}}"#, &context()),
            Err(ParseError::InvalidField {
                index: None,
                field: "items"
            })
        );
        assert_eq!(
            parse_capture_response(r#"{"items": ["x"]}"#, &context()),
            Err(ParseError::InvalidField {
                index: Some(0),
                field: "item"
            })
        );
    }

    #[test]
    fn clarifications_are_pairs_and_extras_are_dropped() {
        let text = json!({
            "items": [task(), shopping()],
            "clarifications": [
                {"item": 0, "topic": "date"},
                {"item": 1, "topic": "time"},
                {"item": 99, "topic": "assignee"},
                {"item": 0, "topic": "list"},
            ],
        })
        .to_string();

        let parsed = parse_capture_response(&text, &context()).expect("clarifications are valid");

        assert_eq!(parsed.clarifications.len(), MAX_CLARIFICATIONS);
        assert_eq!(parsed.clarifications[0].topic, ClarificationTopic::Date);
        assert_eq!(parsed.clarifications[2].item, 99);
    }

    #[test]
    fn every_contract_topic_is_accepted() {
        for topic in contract::CLARIFICATION_TOPICS {
            let text = json!({
                "items": [task()],
                "clarifications": [{"item": 0, "topic": topic}],
            })
            .to_string();
            let parsed = parse_capture_response(&text, &context()).expect("topic is known");
            assert_eq!(parsed.clarifications.len(), 1, "{topic}");
        }
    }

    #[test]
    fn repeat_is_parsed_and_null_means_no_repeat() {
        let with_repeat = with(
            task(),
            "repeat",
            json!({
                "freq": "weekly",
                "interval": 1,
                "weekdays": [1, 4],
                "month_rule": "last_day",
                "until": {"kind": "date", "n": 0, "unit": "", "weekday": "", "day": 1, "month": 11, "year": 2026},
            }),
        );
        let parsed = parse(vec![with_repeat]).expect("repeat is valid");
        match &parsed.items[0] {
            Item::Task(item) => {
                let repeat = item.repeat.as_ref().expect("repeat is present");
                assert_eq!(repeat.freq, RecurrenceFreq::Weekly);
                assert_eq!(repeat.interval, 1);
                assert_eq!(repeat.weekdays, [1, 4]);
                assert_eq!(repeat.month_rule, Some(MonthRule::LastDay));
                assert_eq!(repeat.until.kind, DateKind::Date);
            }
            other => panic!("not a task: {other:?}"),
        }

        let parsed = parse(vec![task()]).expect("null repeat is valid");
        match &parsed.items[0] {
            Item::Task(item) => assert_eq!(item.repeat, None),
            other => panic!("not a task: {other:?}"),
        }
    }

    #[test]
    fn malformed_clarifications_do_not_discard_items() {
        for clarifications in [json!("Когда?"), json!([{"question": "Когда?"}])] {
            let text = json!({"items": [task(), shopping()], "clarifications": clarifications})
                .to_string();
            let parsed = parse_capture_response(&text, &context()).expect("items survive");

            assert_eq!(parsed.items.len(), 2);
            assert!(parsed.clarifications.is_empty());
        }
    }

    #[test]
    fn malformed_clarification_entries_are_skipped() {
        for entry in [
            json!({"item": 0, "topic": "repeat"}),
            json!({"item": 0, "topic": "date", "question": "Когда?"}),
            json!({"item": -1, "topic": "date"}),
        ] {
            let text = json!({
                "items": [task()],
                "clarifications": [{"item": 0, "topic": "date"}, entry.clone()],
            })
            .to_string();
            let parsed = parse_capture_response(&text, &context()).expect("items survive");

            assert_eq!(parsed.items.len(), 1, "{entry}");
            assert_eq!(
                parsed.clarifications,
                [Clarification {
                    item: 0,
                    topic: ClarificationTopic::Date
                }],
                "{entry}"
            );
        }
    }

    #[test]
    fn error_messages_do_not_contain_the_response_text() {
        let secret = "секретный текст ответа";
        let cases = [
            with(task(), "list", json!(secret)),
            with(task(), "kind", json!(secret)),
            with(shopping(), "quantity", json!(secret)),
            with(task(), "title", Value::Null),
        ];
        for item in cases {
            let error = parse(vec![item]).expect_err("response is invalid");
            assert!(
                !error.to_string().contains(secret),
                "error leaks the response: {error}"
            );
        }

        let error = parse_capture_response(secret, &context()).expect_err("not JSON");
        assert!(!error.to_string().contains(secret));
    }

    #[test]
    fn every_form_example_from_the_contract_parses() {
        for spec in &FORMS {
            let example = serde_json::to_value(spec.example()).expect("example is serializable");
            let parsed = parse(vec![example]).expect("contract example parses");
            assert_eq!(parsed.items[0].form(), spec.form, "{:?}", spec.form);
        }
    }

    /// Полный ответ v4 из `docs/llm-capture.md` проходит разбор формы для
    /// контекста с теми списками и участниками, что в нём названы. Документ и
    /// тест связаны маркерами: каждый пример нельзя поменять, не проверив его.
    #[test]
    fn documented_example_parses() {
        const DOC: &str = include_str!("../../docs/llm-capture.md");
        const START: &str = "<!-- capture-v4-example:start -->";
        const END: &str = "<!-- capture-v4-example:end -->";

        let mut context = context();
        context.lists.push(crate::capture::test_support::list(
            "Уборка",
            ListKind::Checklist,
            false,
        ));
        context.lists.push(crate::capture::test_support::list(
            "Календарь",
            ListKind::Events,
            true,
        ));

        let mut examples = Vec::new();
        let mut rest = DOC;
        while let Some(start) = rest.find(START) {
            let block_start = start + START.len();
            let block_end = rest[block_start..]
                .find(END)
                .expect("the example end marker is present")
                + block_start;
            let block = rest[block_start..block_end].trim();
            let example = block
                .strip_prefix("```json")
                .and_then(|block| block.strip_suffix("```"))
                .map(str::trim)
                .expect("the example is a json code block");

            examples.push(
                parse_capture_response(example, &context)
                    .unwrap_or_else(|error| panic!("documented example does not parse: {error}")),
            );
            rest = &rest[block_end + END.len()..];
        }

        assert_eq!(examples.len(), 4, "every documented example is marked");
        assert_eq!(
            examples
                .iter()
                .map(|example| example.items.len())
                .collect::<Vec<_>>(),
            [5, 1, 1, 1]
        );

        let first = &examples[0];
        assert_eq!(first.items[0].form(), Form::Shopping);
        assert_eq!(first.items[1].form(), Form::Task);
        assert_eq!(first.items[2].kind(), ListKind::Checklist);
        assert_eq!(first.items[3].form(), Form::Event);
        assert_eq!(first.items[4].form(), Form::Event);
        assert_eq!(
            first.clarifications,
            [Clarification {
                item: 4,
                topic: ClarificationTopic::Date
            }]
        );
    }

    #[test]
    fn responses_valid_against_the_generated_schema_parse() {
        let mut context = context();
        context.lists.push(crate::capture::test_support::list(
            "Уборка",
            ListKind::Checklist,
            false,
        ));
        let schema =
            serde_json::to_value(contract::schema(&context)).expect("schema is serializable");
        let validator = jsonschema::validator_for(&schema).expect("schema compiles");

        let task = with(
            task(),
            "date",
            json!({"kind": "offset", "n": 2, "unit": "days", "weekday": "", "day": 0, "month": 0, "year": 0}),
        );
        let task = with(task, "list", json!("Дача"));
        let task = with(task, "assignee", json!("Анна"));
        let task = with(task, "priority", json!("high"));
        let task = with(
            task,
            "repeat",
            json!({
                "freq": "weekly",
                "interval": 1,
                "weekdays": [1, 4],
                "month_rule": "",
                "until": {"kind": "date", "n": 0, "unit": "", "weekday": "", "day": 1, "month": 11, "year": 2026},
            }),
        );

        let shopping_item = with(shopping(), "list", json!("Покупки"));
        let shopping_item = with(shopping_item, "quantity", json!(2));
        let shopping_item = with(shopping_item, "unit", json!("л"));
        let shopping_item = with(shopping_item, "tags", json!(["продукты"]));

        let checklist = with(shopping(), "kind", json!("checklist"));
        let checklist = with(checklist, "list", json!("Уборка"));

        let event = with(
            event(),
            "date",
            json!({"kind": "date", "n": 0, "unit": "", "weekday": "", "day": 1, "month": 11, "year": 2026}),
        );
        let event = with(
            event,
            "end",
            json!({"kind": "date", "n": 0, "unit": "", "weekday": "", "day": 1, "month": 11, "year": 2026}),
        );

        let items = vec![task, shopping_item, checklist, event];
        let text = json!({"items": items, "clarifications": []}).to_string();
        let value: Value = serde_json::from_str(&text).expect("response is JSON");
        assert!(
            validator.is_valid(&value),
            "response does not match the schema"
        );

        let parsed = parse_capture_response(&text, &context).expect("schema-valid response parses");
        assert_eq!(parsed.items.len(), 4);
        assert_eq!(parsed.items[2].kind(), ListKind::Checklist);
    }
}
