//! Контракт ответа `capture-v4`: таблица полей трёх форм и генератор JSON Schema.
//!
//! Таблица полей — единственное описание контракта: из неё собираются и схема
//! ответа, и примеры промпта (6.1), поэтому пример не может разойтись со
//! схемой. Порядок полей — «сначала решения» из design: `kind`, `list`,
//! `title`, `date`, `time`, `end`, `end_time`, `repeat`, `assignee`,
//! `priority`, `quantity`, `unit`, `subtasks`, `tags`, `notes`, `note_date`;
//! каждая форма берёт из него своё подмножество.
//!
//! Схема собирается на каждый запрос из списков и участников household. Она
//! остаётся простой для грамматики провайдеров: без `minimum`, `maximum`,
//! `maxItems`, `pattern`, `oneOf` и `description`; описание даты вынесено в
//! `$defs.date` и подключается по `$ref`, иначе Anthropic отвечает
//! `400 The compiled grammar is too large`.
//!
//! `serde_json` в проекте собран без `preserve_order`, поэтому объект схемы
//! здесь — [`SchemaValue`] со списком пар: `serde` сериализует его в порядке
//! таблицы, а не по алфавиту.

use serde::Serialize;
use serde::ser::{SerializeMap, Serializer};

use super::context::CaptureContext;
use crate::domain::models::ListKind;

/// Форма элемента ответа.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Form {
    /// Задача.
    Task,
    /// Покупка или пункт чек-листа.
    Shopping,
    /// Событие.
    Event,
}

impl Form {
    /// Значения поля `kind` для формы.
    pub fn kinds(self) -> &'static [&'static str] {
        match self {
            Self::Task => &["tasks"],
            Self::Shopping => &["shopping", "checklist"],
            Self::Event => &["events"],
        }
    }

    /// Виды списков, в которые попадают элементы формы.
    pub fn list_kinds(self) -> &'static [ListKind] {
        match self {
            Self::Task => &[ListKind::Tasks],
            Self::Shopping => &[ListKind::Shopping, ListKind::Checklist],
            Self::Event => &[ListKind::Events],
        }
    }
}

/// Поле элемента в порядке контракта.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    /// Вид элемента.
    Kind,
    /// Название списка.
    List,
    /// Название элемента.
    Title,
    /// Дата начала, срока или события.
    Date,
    /// Время начала или срока.
    Time,
    /// Дата конца события.
    End,
    /// Время конца события.
    EndTime,
    /// Правило повторения.
    Repeat,
    /// Исполнитель или участник.
    Assignee,
    /// Приоритет.
    Priority,
    /// Количество.
    Quantity,
    /// Единица измерения.
    Unit,
    /// Названия подзадач.
    Subtasks,
    /// Теги.
    Tags,
    /// Заметка.
    Notes,
    /// Вторая дата фразы; сервер дописывает её в заметку.
    NoteDate,
}

/// Порядок полей контракта: формы берут из него своё подмножество.
pub const FIELD_ORDER: [Field; 16] = [
    Field::Kind,
    Field::List,
    Field::Title,
    Field::Date,
    Field::Time,
    Field::End,
    Field::EndTime,
    Field::Repeat,
    Field::Assignee,
    Field::Priority,
    Field::Quantity,
    Field::Unit,
    Field::Subtasks,
    Field::Tags,
    Field::Notes,
    Field::NoteDate,
];

impl Field {
    /// Имя поля в ответе модели.
    pub fn name(self) -> &'static str {
        match self {
            Self::Kind => "kind",
            Self::List => "list",
            Self::Title => "title",
            Self::Date => "date",
            Self::Time => "time",
            Self::End => "end",
            Self::EndTime => "end_time",
            Self::Repeat => "repeat",
            Self::Assignee => "assignee",
            Self::Priority => "priority",
            Self::Quantity => "quantity",
            Self::Unit => "unit",
            Self::Subtasks => "subtasks",
            Self::Tags => "tags",
            Self::Notes => "notes",
            Self::NoteDate => "note_date",
        }
    }

    /// Значение «не названо» для примера промпта; `kind` — первое значение
    /// вида формы.
    pub fn example(self, form: Form) -> SchemaValue {
        match self {
            Self::Kind => SchemaValue::string(form.kinds()[0]),
            Self::List
            | Self::Title
            | Self::Time
            | Self::EndTime
            | Self::Assignee
            | Self::Priority
            | Self::Unit
            | Self::Notes => SchemaValue::string(""),
            Self::Date | Self::End | Self::NoteDate => date_example(),
            Self::Repeat => SchemaValue::Null,
            Self::Quantity => SchemaValue::Integer(0),
            Self::Subtasks | Self::Tags => SchemaValue::Array(Vec::new()),
        }
    }
}

/// Поля одной формы в порядке таблицы.
#[derive(Debug, Clone, Copy)]
pub struct FormSpec {
    /// Форма.
    pub form: Form,
    /// Поля формы.
    pub fields: &'static [Field],
}

impl FormSpec {
    /// Пример элемента формы: все значения «не названы».
    pub fn example(&self) -> SchemaValue {
        SchemaValue::Object(
            self.fields
                .iter()
                .map(|field| (field.name(), field.example(self.form)))
                .collect(),
        )
    }
}

const TASK_FIELDS: &[Field] = &[
    Field::Kind,
    Field::List,
    Field::Title,
    Field::Date,
    Field::Time,
    Field::Repeat,
    Field::Assignee,
    Field::Priority,
    Field::Subtasks,
    Field::Tags,
    Field::Notes,
    Field::NoteDate,
];

const SHOPPING_FIELDS: &[Field] = &[
    Field::Kind,
    Field::List,
    Field::Title,
    Field::Date,
    Field::Time,
    Field::Assignee,
    Field::Priority,
    Field::Quantity,
    Field::Unit,
    Field::Tags,
    Field::Notes,
];

const EVENT_FIELDS: &[Field] = &[
    Field::Kind,
    Field::List,
    Field::Title,
    Field::Date,
    Field::Time,
    Field::End,
    Field::EndTime,
    Field::Repeat,
    Field::Assignee,
    Field::Tags,
    Field::Notes,
];

/// Таблица трёх форм контракта в порядке `anyOf`.
pub const FORMS: [FormSpec; 3] = [
    FormSpec {
        form: Form::Task,
        fields: TASK_FIELDS,
    },
    FormSpec {
        form: Form::Shopping,
        fields: SHOPPING_FIELDS,
    },
    FormSpec {
        form: Form::Event,
        fields: EVENT_FIELDS,
    },
];

/// Темы уточняющих вопросов; текст вопроса и кнопки строит сервер.
pub const CLARIFICATION_TOPICS: [&str; 4] = ["date", "time", "assignee", "list"];

/// Узел JSON Schema с сохранённым порядком ключей объекта.
#[derive(Debug, Clone, PartialEq)]
pub enum SchemaValue {
    /// `null`.
    Null,
    /// Логическое значение.
    Bool(bool),
    /// Целое число.
    Integer(u32),
    /// Строка.
    String(String),
    /// Массив.
    Array(Vec<SchemaValue>),
    /// Объект: ключи идут в порядке сериализации.
    Object(Vec<(&'static str, SchemaValue)>),
}

impl SchemaValue {
    /// Строковое значение.
    pub fn string(value: impl Into<String>) -> Self {
        Self::String(value.into())
    }

    /// JSON-текст в порядке таблицы.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("schema is serializable")
    }

    /// Объект по списку пар.
    fn object(entries: Vec<(&'static str, SchemaValue)>) -> Self {
        Self::Object(entries)
    }

    /// Схема `{"type": "string", "enum": [...]}`.
    fn enumeration(values: impl IntoIterator<Item = impl Into<String>>) -> Self {
        let values = values
            .into_iter()
            .map(|value| Self::string(value))
            .collect::<Vec<_>>();
        Self::object(vec![
            ("type", Self::string("string")),
            ("enum", Self::Array(values)),
        ])
    }

    /// Схема `{"type": "<name>"}`.
    fn typed(name: &'static str) -> Self {
        Self::object(vec![("type", Self::string(name))])
    }
}

impl Serialize for SchemaValue {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Null => serializer.serialize_none(),
            Self::Bool(value) => serializer.serialize_bool(*value),
            Self::Integer(value) => serializer.serialize_u32(*value),
            Self::String(value) => serializer.serialize_str(value),
            Self::Array(items) => items.serialize(serializer),
            Self::Object(entries) => {
                let mut map = serializer.serialize_map(Some(entries.len()))?;
                for (key, value) in entries {
                    map.serialize_entry(key, value)?;
                }
                map.end()
            }
        }
    }
}

/// Собрать JSON Schema ответа для одного разбора.
pub fn schema(context: &CaptureContext) -> SchemaValue {
    SchemaValue::object(vec![
        ("type", SchemaValue::string("object")),
        (
            "properties",
            SchemaValue::object(vec![
                ("items", items_schema(context)),
                ("clarifications", clarifications_schema()),
            ]),
        ),
        (
            "required",
            SchemaValue::Array(vec![
                SchemaValue::string("items"),
                SchemaValue::string("clarifications"),
            ]),
        ),
        ("additionalProperties", SchemaValue::Bool(false)),
        (
            "$defs",
            SchemaValue::object(vec![("date", date_definition())]),
        ),
    ])
}

fn items_schema(context: &CaptureContext) -> SchemaValue {
    let forms = FORMS
        .iter()
        .map(|form| form_schema(context, form))
        .collect::<Vec<_>>();
    SchemaValue::object(vec![
        ("type", SchemaValue::string("array")),
        (
            "items",
            SchemaValue::object(vec![("anyOf", SchemaValue::Array(forms))]),
        ),
    ])
}

fn form_schema(context: &CaptureContext, spec: &FormSpec) -> SchemaValue {
    let properties = spec
        .fields
        .iter()
        .map(|field| (field.name(), field_schema(context, spec.form, *field)))
        .collect::<Vec<_>>();
    let required = spec
        .fields
        .iter()
        .map(|field| SchemaValue::string(field.name()))
        .collect();
    SchemaValue::object(vec![
        ("type", SchemaValue::string("object")),
        ("properties", SchemaValue::Object(properties)),
        ("required", SchemaValue::Array(required)),
        ("additionalProperties", SchemaValue::Bool(false)),
    ])
}

fn field_schema(context: &CaptureContext, form: Form, field: Field) -> SchemaValue {
    match field {
        Field::Kind => SchemaValue::enumeration(form.kinds().iter().copied()),
        Field::List => SchemaValue::enumeration(list_names(context, form)),
        Field::Assignee => SchemaValue::enumeration(member_names(context)),
        Field::Priority => SchemaValue::enumeration(["", "low", "normal", "high", "urgent"]),
        Field::Title | Field::Time | Field::EndTime | Field::Unit | Field::Notes => {
            SchemaValue::typed("string")
        }
        Field::Date | Field::End | Field::NoteDate => {
            SchemaValue::object(vec![("$ref", SchemaValue::string("#/$defs/date"))])
        }
        Field::Repeat => SchemaValue::object(vec![(
            "anyOf",
            SchemaValue::Array(vec![repeat_object(), SchemaValue::typed("null")]),
        )]),
        Field::Quantity => SchemaValue::typed("number"),
        Field::Subtasks | Field::Tags => SchemaValue::object(vec![
            ("type", SchemaValue::string("array")),
            ("items", SchemaValue::typed("string")),
        ]),
    }
}

/// Названия списков формы и пустая строка — «список не назван».
fn list_names(context: &CaptureContext, form: Form) -> Vec<String> {
    let kinds = form.list_kinds();
    dedup_of(
        context
            .lists
            .iter()
            .filter(|list| kinds.contains(&list.kind))
            .map(|list| list.name.clone()),
    )
}

/// Отображаемые имена участников и пустая строка; одинаковые имена
/// схлопываются: повтор в `enum` неоднозначен, а неоднозначность и так
/// разрешает валидатор кнопочным вопросом.
fn member_names(context: &CaptureContext) -> Vec<String> {
    dedup_of(
        context
            .members
            .iter()
            .map(|member| member.display_name.clone()),
    )
}

/// Уникальные значения в порядке появления плюс пустая строка — «не названо».
fn dedup_of(values: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for value in values {
        if !names.contains(&value) {
            names.push(value);
        }
    }
    names.push(String::new());
    names
}

fn repeat_object() -> SchemaValue {
    SchemaValue::object(vec![
        ("type", SchemaValue::string("object")),
        (
            "properties",
            SchemaValue::object(vec![
                (
                    "freq",
                    SchemaValue::enumeration(["daily", "weekly", "monthly", "yearly"]),
                ),
                ("interval", SchemaValue::typed("integer")),
                (
                    "weekdays",
                    SchemaValue::object(vec![
                        ("type", SchemaValue::string("array")),
                        (
                            "items",
                            SchemaValue::object(vec![
                                ("type", SchemaValue::string("integer")),
                                (
                                    "enum",
                                    SchemaValue::Array((1..=7).map(SchemaValue::Integer).collect()),
                                ),
                            ]),
                        ),
                    ]),
                ),
                (
                    "month_rule",
                    SchemaValue::enumeration(["", "anchor_day", "last_day"]),
                ),
                (
                    "until",
                    SchemaValue::object(vec![("$ref", SchemaValue::string("#/$defs/date"))]),
                ),
            ]),
        ),
        (
            "required",
            SchemaValue::Array(
                ["freq", "interval", "weekdays", "month_rule", "until"]
                    .into_iter()
                    .map(SchemaValue::string)
                    .collect(),
            ),
        ),
        ("additionalProperties", SchemaValue::Bool(false)),
    ])
}

/// Уточнения — пары «номер элемента, тема»: текст вопроса и кнопки строит
/// сервер. `item` — номер элемента `items` с нуля.
fn clarifications_schema() -> SchemaValue {
    SchemaValue::object(vec![
        ("type", SchemaValue::string("array")),
        (
            "items",
            SchemaValue::object(vec![
                ("type", SchemaValue::string("object")),
                (
                    "properties",
                    SchemaValue::object(vec![
                        ("item", SchemaValue::typed("integer")),
                        ("topic", SchemaValue::enumeration(CLARIFICATION_TOPICS)),
                    ]),
                ),
                (
                    "required",
                    SchemaValue::Array(vec![
                        SchemaValue::string("item"),
                        SchemaValue::string("topic"),
                    ]),
                ),
                ("additionalProperties", SchemaValue::Bool(false)),
            ]),
        ),
    ])
}

/// Описание даты-выражения: неиспользуемые поля — ноль или пустая строка.
/// У `kind=weekday` и `kind=week` поле `n` кодирует признак «следующий»: `0` —
/// ближайший такой день (включая сегодня) или текущая неделя, `1` — этот день
/// следующей календарной недели (с понедельника) или следующая неделя.
fn date_definition() -> SchemaValue {
    SchemaValue::object(vec![
        ("type", SchemaValue::string("object")),
        (
            "properties",
            SchemaValue::object(vec![
                (
                    "kind",
                    SchemaValue::enumeration([
                        "none",
                        "offset",
                        "weekday",
                        "date",
                        "weekend",
                        "week",
                        "month_start",
                        "month_end",
                    ]),
                ),
                ("n", SchemaValue::typed("integer")),
                (
                    "unit",
                    SchemaValue::enumeration(["", "days", "weeks", "months", "years"]),
                ),
                (
                    "weekday",
                    SchemaValue::enumeration([
                        "",
                        "monday",
                        "tuesday",
                        "wednesday",
                        "thursday",
                        "friday",
                        "saturday",
                        "sunday",
                    ]),
                ),
                ("day", SchemaValue::typed("integer")),
                ("month", SchemaValue::typed("integer")),
                ("year", SchemaValue::typed("integer")),
            ]),
        ),
        (
            "required",
            SchemaValue::Array(
                ["kind", "n", "unit", "weekday", "day", "month", "year"]
                    .into_iter()
                    .map(SchemaValue::string)
                    .collect(),
            ),
        ),
        ("additionalProperties", SchemaValue::Bool(false)),
    ])
}

fn date_example() -> SchemaValue {
    SchemaValue::object(vec![
        ("kind", SchemaValue::string("none")),
        ("n", SchemaValue::Integer(0)),
        ("unit", SchemaValue::string("")),
        ("weekday", SchemaValue::string("")),
        ("day", SchemaValue::Integer(0)),
        ("month", SchemaValue::Integer(0)),
        ("year", SchemaValue::Integer(0)),
    ])
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::capture::test_support::{list, member};

    /// Контекст с списками всех видов и повторами имён.
    fn full_context() -> CaptureContext {
        let members = vec![
            member("Анна", Some("anna")),
            member("Пётр", None),
            member("Анна", Some("anna2")),
            member("Оля", None),
        ];
        let mut context = crate::capture::test_support::context();
        context.lists = vec![
            list("Задачи", ListKind::Tasks, true),
            list("Дача", ListKind::Tasks, false),
            list("Покупки", ListKind::Shopping, true),
            list("Аптека", ListKind::Shopping, false),
            list("Уборка", ListKind::Checklist, false),
            list("Календарь", ListKind::Events, true),
        ];
        context.members = members;
        context
    }

    fn form_schemas(schema: &SchemaValue) -> Vec<&SchemaValue> {
        let properties = object_value(schema, "properties");
        let items = object_value(properties, "items");
        let any_of = object_value(object_value(items, "items"), "anyOf");
        match any_of {
            SchemaValue::Array(forms) => forms.iter().collect(),
            other => panic!("anyOf is not an array: {other:?}"),
        }
    }

    fn object_value<'a>(value: &'a SchemaValue, key: &str) -> &'a SchemaValue {
        match value {
            SchemaValue::Object(entries) => entries
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| value)
                .unwrap_or_else(|| panic!("no {key} in {value:?}")),
            other => panic!("not an object: {other:?}"),
        }
    }

    fn properties(schema: &SchemaValue) -> &Vec<(&'static str, SchemaValue)> {
        match object_value(schema, "properties") {
            SchemaValue::Object(entries) => entries,
            other => panic!("not an object: {other:?}"),
        }
    }

    fn names(schema: &SchemaValue) -> Vec<String> {
        properties(schema)
            .iter()
            .map(|(name, _)| (*name).to_owned())
            .collect()
    }

    fn enum_values(schema: &SchemaValue) -> Vec<String> {
        match object_value(schema, "enum") {
            SchemaValue::Array(values) => values
                .iter()
                .map(|value| match value {
                    SchemaValue::String(value) => value.clone(),
                    SchemaValue::Integer(value) => value.to_string(),
                    other => panic!("not an enum value: {other:?}"),
                })
                .collect(),
            other => panic!("not an enum: {other:?}"),
        }
    }

    fn field(schema: &SchemaValue, name: &str) -> SchemaValue {
        properties(schema)
            .iter()
            .find(|(field, _)| *field == name)
            .map(|(_, value)| value.clone())
            .unwrap_or_else(|| panic!("no field {name}"))
    }

    fn form_property_names(schema: &SchemaValue) -> [Vec<String>; 3] {
        let forms = form_schemas(schema);
        [names(forms[0]), names(forms[1]), names(forms[2])]
    }

    fn expected_names(spec: &FormSpec) -> Vec<String> {
        spec.fields
            .iter()
            .map(|field| field.name().to_owned())
            .collect()
    }

    /// Обойти всё дерево и собрать ключи всех объектов.
    fn collect_keys(value: &SchemaValue, keys: &mut Vec<String>) {
        match value {
            SchemaValue::Object(entries) => {
                for (key, value) in entries {
                    keys.push((*key).to_owned());
                    collect_keys(value, keys);
                }
            }
            SchemaValue::Array(items) => {
                for item in items {
                    collect_keys(item, keys);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn form_properties_follow_the_field_table() {
        let schema = schema(&full_context());
        let actual = form_property_names(&schema);
        for (index, spec) in FORMS.iter().enumerate() {
            assert_eq!(actual[index], expected_names(spec), "{:?}", spec.form);
        }
    }

    #[test]
    fn form_fields_follow_the_contract_order() {
        for spec in &FORMS {
            let positions = spec
                .fields
                .iter()
                .map(|field| {
                    FIELD_ORDER
                        .iter()
                        .position(|candidate| candidate == field)
                        .expect("field is in the contract order")
                })
                .collect::<Vec<_>>();
            let mut sorted = positions.clone();
            sorted.sort_unstable();
            assert_eq!(positions, sorted, "{:?}", spec.form);
        }
    }

    #[test]
    fn object_keys_serialize_in_insertion_order() {
        let object = SchemaValue::object(vec![
            ("kind", SchemaValue::string("tasks")),
            ("list", SchemaValue::string("Задачи")),
            ("title", SchemaValue::string("")),
        ]);
        assert_eq!(
            object.to_json(),
            r#"{"kind":"tasks","list":"Задачи","title":""}"#
        );
    }

    #[test]
    fn lists_of_another_kind_are_absent_from_the_form() {
        let context = full_context();
        let schema = schema(&context);
        let forms = form_schemas(&schema);

        let task = enum_values(&field(forms[0], "list"));
        assert_eq!(task, ["Задачи", "Дача", ""], "tasks");
        let shopping = enum_values(&field(forms[1], "list"));
        assert_eq!(shopping, ["Покупки", "Аптека", "Уборка", ""], "shopping");
        let event = enum_values(&field(forms[2], "list"));
        assert_eq!(event, ["Календарь", ""], "events");

        assert!(!task.contains(&"Календарь".to_owned()));
        assert!(!task.contains(&"Покупки".to_owned()));
        assert!(!shopping.contains(&"Календарь".to_owned()));
        assert!(!event.contains(&"Покупки".to_owned()));
        assert!(!event.contains(&"Задачи".to_owned()));
    }

    #[test]
    fn duplicate_members_collapse_and_empty_choice_is_present() {
        let context = full_context();
        let schema = schema(&context);
        let forms = form_schemas(&schema);

        for form in forms {
            assert_eq!(
                enum_values(&field(form, "assignee")),
                ["Анна", "Пётр", "Оля", ""]
            );
        }
    }

    #[test]
    fn empty_choice_is_present_in_every_list_and_member_enum() {
        let schema = schema(&full_context());
        for form in form_schemas(&schema) {
            for name in ["list", "assignee"] {
                let values = enum_values(&field(form, name));
                let empty = values.last().expect("enum is not empty");
                assert!(empty.is_empty(), "{name}: {values:?}");
            }
        }
    }

    #[test]
    fn forms_have_distinct_fields() {
        let schema = schema(&full_context());
        let forms = form_schemas(&schema);

        let task = names(forms[0]);
        assert!(!task.contains(&"quantity".to_owned()));
        assert!(!task.contains(&"unit".to_owned()));
        assert!(!task.contains(&"end".to_owned()));

        let shopping = names(forms[1]);
        assert!(!shopping.contains(&"repeat".to_owned()));
        assert!(!shopping.contains(&"subtasks".to_owned()));
        assert!(!shopping.contains(&"note_date".to_owned()));

        let event = names(forms[2]);
        assert!(!event.contains(&"priority".to_owned()));
        assert!(!event.contains(&"quantity".to_owned()));
        assert!(!event.contains(&"subtasks".to_owned()));
    }

    #[test]
    fn every_object_is_strict_and_requires_all_properties() {
        let schema = schema(&full_context());
        let mut stack = vec![&schema];
        while let Some(value) = stack.pop() {
            match value {
                SchemaValue::Object(entries) => {
                    let has_properties = entries.iter().any(|(key, _)| *key == "properties");
                    if has_properties {
                        let properties = properties(value);
                        let required: Vec<String> = match object_value(value, "required") {
                            SchemaValue::Array(values) => values
                                .iter()
                                .map(|value| match value {
                                    SchemaValue::String(value) => value.clone(),
                                    other => panic!("not a name: {other:?}"),
                                })
                                .collect(),
                            other => panic!("required is not an array: {other:?}"),
                        };
                        let names = properties
                            .iter()
                            .map(|(name, _)| (*name).to_owned())
                            .collect::<Vec<_>>();
                        assert_eq!(required, names, "{value:?}");
                        assert_eq!(
                            object_value(value, "additionalProperties"),
                            &SchemaValue::Bool(false)
                        );
                    }
                    for (_, value) in entries {
                        stack.push(value);
                    }
                }
                SchemaValue::Array(items) => stack.extend(items.iter()),
                _ => {}
            }
        }
    }

    #[test]
    fn forbidden_keywords_are_absent() {
        let schema = schema(&full_context());
        let mut keys = Vec::new();
        collect_keys(&schema, &mut keys);
        for forbidden in [
            "minimum",
            "maximum",
            "maxItems",
            "pattern",
            "oneOf",
            "description",
        ] {
            assert!(
                !keys.iter().any(|key| key == forbidden),
                "{forbidden} is present"
            );
        }
    }

    #[test]
    fn date_description_lives_in_defs_and_is_referenced() {
        let schema = schema(&full_context());
        let defs = object_value(&schema, "$defs");
        let date = object_value(defs, "date");
        let date_fields = ["kind", "n", "unit", "weekday", "day", "month", "year"];
        assert_eq!(names(date), date_fields);

        let reference = SchemaValue::object(vec![("$ref", SchemaValue::string("#/$defs/date"))]);
        let forms = form_schemas(&schema);
        for (form, name) in [
            (forms[0], "date"),
            (forms[0], "note_date"),
            (forms[1], "date"),
            (forms[2], "date"),
            (forms[2], "end"),
        ] {
            assert_eq!(field(form, name), reference, "{name}");
        }

        // Дата окончания повторения — тоже выражение, а не абсолютная дата.
        for form in [forms[0], forms[2]] {
            let repeat = field(form, "repeat");
            let variants = match object_value(&repeat, "anyOf") {
                SchemaValue::Array(variants) => variants.clone(),
                other => panic!("anyOf is not an array: {other:?}"),
            };
            assert_eq!(field(&variants[0], "until"), reference);
        }
    }

    #[test]
    fn repeat_is_an_object_or_null() {
        let schema = schema(&full_context());
        let forms = form_schemas(&schema);
        for form in [forms[0], forms[2]] {
            let repeat = field(form, "repeat");
            let any_of = object_value(&repeat, "anyOf");
            match any_of {
                SchemaValue::Array(variants) => {
                    assert_eq!(variants.len(), 2);
                    assert_eq!(variants[1], SchemaValue::typed("null"));
                    assert_eq!(
                        variants[0],
                        repeat_object(),
                        "repeat object is inline, not a reference"
                    );
                }
                other => panic!("anyOf is not an array: {other:?}"),
            }
        }
    }

    #[test]
    fn clarification_is_an_item_and_a_topic() {
        let schema = schema(&full_context());
        let clarifications = object_value(object_value(&schema, "properties"), "clarifications");
        let item = object_value(clarifications, "items");
        assert_eq!(names(item), ["item", "topic"]);
        assert_eq!(
            enum_values(&field(item, "topic")),
            CLARIFICATION_TOPICS.to_vec()
        );
    }

    fn object_names(schema: &SchemaValue) -> Vec<String> {
        match schema {
            SchemaValue::Object(entries) => {
                entries.iter().map(|(name, _)| (*name).to_owned()).collect()
            }
            other => panic!("not an object: {other:?}"),
        }
    }

    #[test]
    fn empty_form_example_lists_all_fields_in_order() {
        for spec in &FORMS {
            let example = spec.example();
            assert_eq!(object_names(&example), expected_names(spec));
        }
    }

    /// Пример формы из таблицы проходит схему: тип поля и значение-образец
    /// описаны в разных местах и не могут разойтись.
    #[test]
    fn form_examples_validate_against_the_schema() {
        let schema = schema(&full_context());
        let value = serde_json::to_value(&schema).expect("schema is serializable");
        let validator = jsonschema::validator_for(&value).expect("schema compiles");
        for spec in &FORMS {
            let example = serde_json::to_value(spec.example()).expect("example is serializable");
            let instance = json!({"items": [example], "clarifications": []});
            assert!(validator.is_valid(&instance), "{:?}: {instance}", spec.form);
        }
    }

    /// 20 списков и 8 участников, схема проходит проверку метасхемой.
    #[test]
    fn large_household_schema_is_valid() {
        let mut context = crate::capture::test_support::context();
        context.lists.clear();
        for index in 0..5 {
            context.lists.push(list(
                &format!("Задачи {index}"),
                ListKind::Tasks,
                index == 0,
            ));
            context.lists.push(list(
                &format!("Покупки {index}"),
                ListKind::Shopping,
                index == 0,
            ));
            context.lists.push(list(
                &format!("Чек-лист {index}"),
                ListKind::Checklist,
                index == 0,
            ));
            context.lists.push(list(
                &format!("События {index}"),
                ListKind::Events,
                index == 0,
            ));
        }
        context.members = (0..8)
            .map(|index| member(&format!("Участник {index}"), None))
            .collect();
        assert_eq!(context.lists.len(), 20);
        assert_eq!(context.members.len(), 8);

        let value = serde_json::to_value(schema(&context)).expect("schema is serializable");
        jsonschema::meta::validate(&value).expect("schema matches its meta-schema");
        jsonschema::validator_for(&value).expect("schema compiles");
    }

    #[test]
    fn small_example_date_expression_is_complete() {
        let value = serde_json::to_value(date_example()).expect("date is serializable");
        assert_eq!(
            value,
            json!({
                "kind": "none",
                "n": 0,
                "unit": "",
                "weekday": "",
                "day": 0,
                "month": 0,
                "year": 0,
            })
        );
    }
}
