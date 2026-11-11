//! Отрисовка запроса `capture-v4` из JSONL для проекта дообучения.
//!
//! Вход — описание контекста и фраза, выход — сообщения и схема в том виде, в
//! каком они уходят в `chat/completions`. Запрос собирает та же функция, что у
//! разбора ([`prompt::build_request`]), а части тела сериализует
//! [`crate::llm::request_payload`]; поэтому отрисовка не может разойтись с
//! боевым запросом.
//!
//! Идентификаторы списков и участников в описании контекста не нужны: в промпт
//! и схему попадают только имена. Модуль не знает о базе и сети.
//!
//! Входные структуры строгие (`deny_unknown_fields`): опечатка в имени поля —
//! ошибка строки, а не другой запрос молча. `context.lists` и `context.members`
//! можно не задавать — это пустые перечни; автор же обязан быть среди
//! участников, иначе контекст не соответствовал бы боевому.
//!
//! Текст фразы в сообщения об ошибках не попадает: он может быть
//! пользовательскими данными, а разбирать датасет удобнее по номеру строки.

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use serde::Deserialize;
use serde::Serialize;
use serde_json::{Map, Value};
use uuid::Uuid;

use super::context::{CaptureContext, ListRef, MemberRef};
use super::prompt;
use crate::domain::models::ListKind;
use crate::domain::recurrence::parse_timezone;
use crate::llm::request_payload;

/// Ошибка одной записи. Номер строки добавляет вызывающий.
#[derive(Debug)]
pub struct RenderError {
    /// Идентификатор записи, если его удалось прочитать.
    pub id: Option<Value>,
    /// Сообщение без текста фразы.
    pub message: String,
}

/// Запись выхода в порядке контракта.
#[derive(Debug, Serialize)]
struct RenderedRecord {
    id: Value,
    messages: Value,
    schema: Value,
    prompt_version: &'static str,
    contract_hash: String,
}

/// Запись входа; неизвестное поле — ошибка, чтобы опечатка не меняла запрос.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct InputRecord {
    /// Строка или число; возвращается как есть.
    id: Value,
    /// Контекст household.
    context: InputContext,
    /// Фраза пользователя.
    text: String,
}

/// Контекст записи.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct InputContext {
    /// Момент RFC 3339 с поясом.
    now: String,
    /// Пояс IANA household.
    timezone: String,
    /// Списки; без поля — пустой перечень.
    #[serde(default)]
    lists: Vec<InputList>,
    /// Имена участников; без поля — пустой перечень.
    #[serde(default)]
    members: Vec<String>,
    /// Имя автора или отсутствие автора.
    #[serde(default)]
    author: Option<String>,
}

/// Список в описании контекста.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct InputList {
    /// Название.
    name: String,
    /// Вид списка: `tasks`, `shopping`, `checklist` или `events`.
    kind: ListKind,
    /// Список по умолчанию для своего типа.
    #[serde(default)]
    default: bool,
}

/// Отрисовать одну строку JSONL: вернуть готовую строку выхода.
///
/// Ошибка содержит `id`, если он есть во входе, но никогда не содержит текст
/// фразы.
pub fn render_line(line: &str) -> Result<String, RenderError> {
    let value: Value = serde_json::from_str(line)
        .map_err(|_| RenderError::new(None, "строка не является JSON"))?;
    let id = value.get("id").and_then(record_id);
    let record = render_record(value).map_err(|message| RenderError {
        id: id.clone(),
        message,
    })?;
    serde_json::to_string(&record).map_err(|_| RenderError {
        id,
        message: "не удалось сериализовать запись".to_owned(),
    })
}

fn render_record(value: Value) -> Result<RenderedRecord, String> {
    // Типы проверяются до serde: в ошибке неверного типа serde печатает само
    // значение, а там может оказаться фраза пользователя.
    check_record_shape(&value)?;
    let record: InputRecord = serde_json::from_value(value).map_err(|error| safe_error(&error))?;
    let id = record_id(&record.id).ok_or_else(|| "id должен быть строкой или числом".to_owned())?;

    let context = record.context.into_context()?;
    let request = prompt::build_request(&context, &record.text);
    let payload = request_payload(&request.messages, &request.schema);
    Ok(RenderedRecord {
        id,
        messages: payload.messages,
        schema: payload.schema,
        prompt_version: request.prompt_version,
        contract_hash: request.contract_hash,
    })
}

/// Сообщение serde без значения поля: имена ключей в ошибках `unknown field` и
/// `missing field` безопасны, остальные ошибки сводятся к общему тексту.
fn safe_error(error: &serde_json::Error) -> String {
    let text = error.to_string();
    let head = text.split(" at line ").next().unwrap_or(&text);
    if head.starts_with("unknown field") || head.starts_with("missing field") {
        format!("неверная запись: {head}")
    } else {
        "неверная запись: неверный тип или значение поля".to_owned()
    }
}

/// Проверить типы полей записи до `serde_json::from_value`.
///
/// Возвращает путь к полю и не повторяет значение: даже если в поле неверного
/// типа лежит фраза пользователя, она не попадает в сообщение.
fn check_record_shape(value: &Value) -> Result<(), String> {
    let record = value
        .as_object()
        .ok_or_else(|| "неверная запись: ожидался объект".to_owned())?;
    unknown_keys(record, "", &["id", "context", "text"])?;
    if !record.contains_key("id") {
        return Err("неверная запись: отсутствует поле `id`".to_owned());
    }
    if !record.contains_key("text") {
        return Err("неверная запись: отсутствует поле `text`".to_owned());
    }
    expect_string(record, "text", "text")?;

    let context = record
        .get("context")
        .ok_or_else(|| "неверная запись: отсутствует поле `context`".to_owned())?
        .as_object()
        .ok_or_else(|| "неверная запись: поле `context` должно быть объектом".to_owned())?;
    unknown_keys(
        context,
        "в context",
        &["now", "timezone", "lists", "members", "author"],
    )?;
    expect_string(context, "now", "context.now")?;
    expect_string(context, "timezone", "context.timezone")?;

    if let Some(lists) = context.get("lists") {
        let lists = lists.as_array().ok_or_else(|| {
            "неверная запись: поле `context.lists` должно быть массивом".to_owned()
        })?;
        for (index, list) in lists.iter().enumerate() {
            let path = format!("context.lists[{index}]");
            let list = list
                .as_object()
                .ok_or_else(|| format!("неверная запись: поле `{path}` должно быть объектом"))?;
            unknown_keys(list, &format!("в {path}"), &["name", "kind", "default"])?;
            expect_string(list, "name", &format!("{path}.name"))?;
            let kind = expect_string(list, "kind", &format!("{path}.kind"))?
                .ok_or_else(|| format!("неверная запись: отсутствует поле `{path}.kind`"))?;
            if !matches!(kind.trim(), "tasks" | "shopping" | "checklist" | "events") {
                return Err(format!(
                    "неверная запись: поле `{path}.kind` должно быть одним из: \
                     tasks, shopping, checklist, events"
                ));
            }
            if let Some(default) = list.get("default")
                && !default.is_boolean()
            {
                return Err(format!(
                    "неверная запись: поле `{path}.default` должно быть логическим"
                ));
            }
        }
    }

    if let Some(members) = context.get("members") {
        let members = members.as_array().ok_or_else(|| {
            "неверная запись: поле `context.members` должно быть массивом".to_owned()
        })?;
        for (index, member) in members.iter().enumerate() {
            if !member.is_string() {
                return Err(format!(
                    "неверная запись: поле `context.members[{index}]` должно быть строкой"
                ));
            }
        }
    }

    if let Some(author) = context.get("author")
        && !author.is_null()
        && !author.is_string()
    {
        return Err(
            "неверная запись: поле `context.author` должно быть строкой или null".to_owned(),
        );
    }

    Ok(())
}

fn unknown_keys(object: &Map<String, Value>, place: &str, allowed: &[&str]) -> Result<(), String> {
    if let Some(key) = object.keys().find(|key| !allowed.contains(&key.as_str())) {
        let place = if place.is_empty() {
            String::new()
        } else {
            format!(" {place}")
        };
        return Err(format!("неверная запись: неизвестное поле `{key}`{place}"));
    }
    Ok(())
}

fn expect_string<'a>(
    object: &'a Map<String, Value>,
    key: &str,
    path: &str,
) -> Result<Option<&'a str>, String> {
    match object.get(key) {
        None => Ok(None),
        Some(value) => value
            .as_str()
            .map(Some)
            .ok_or_else(|| format!("неверная запись: поле `{path}` должно быть строкой")),
    }
}

impl InputContext {
    fn into_context(self) -> Result<CaptureContext, String> {
        let now = DateTime::parse_from_rfc3339(&self.now)
            .map(|moment| moment.with_timezone(&Utc))
            .map_err(|_| "context.now должен быть моментом RFC 3339 с поясом".to_owned())?;
        let timezone = parse_household_timezone(&self.timezone)?;
        let lists = self
            .lists
            .into_iter()
            .map(|list| ListRef {
                id: Uuid::nil(),
                name: list.name,
                kind: list.kind,
                is_default: list.default,
            })
            .collect();
        let members = self
            .members
            .iter()
            .map(|name| member(name))
            .collect::<Vec<_>>();
        let author = match self.author {
            None => None,
            Some(name) if self.members.iter().any(|member_name| member_name == &name) => {
                Some(member(&name))
            }
            Some(_) => {
                return Err("context.author должен быть среди context.members".to_owned());
            }
        };

        Ok(CaptureContext {
            now,
            timezone,
            lists,
            members,
            author,
        })
    }
}

fn parse_household_timezone(value: &str) -> Result<Tz, String> {
    parse_timezone(value).map_err(|_| "context.timezone должен быть поясом IANA".to_owned())
}

fn member(name: &str) -> MemberRef {
    MemberRef {
        id: Uuid::nil(),
        display_name: name.to_owned(),
        username: None,
    }
}

/// Идентификатор записи: строка или число, возвращается как есть.
fn record_id(value: &Value) -> Option<Value> {
    match value {
        Value::String(_) | Value::Number(_) => Some(value.clone()),
        _ => None,
    }
}

impl RenderError {
    fn new(id: Option<Value>, message: impl Into<String>) -> Self {
        Self {
            id,
            message: message.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn line(context: Value) -> String {
        json!({"id": 1, "context": context, "text": "купить молоко"}).to_string()
    }

    fn context() -> Value {
        json!({
            "now": "2026-10-30T11:05:00+03:00",
            "timezone": "Europe/Moscow",
            "lists": [{"name": "Задачи", "kind": "tasks", "default": true}],
            "members": ["Анна", "Пётр"],
            "author": "Анна",
        })
    }

    fn error(input: &str) -> String {
        render_line(input).expect_err("record is invalid").message
    }

    #[test]
    fn valid_record_is_rendered() {
        let rendered = render_line(&line(context())).expect("record is valid");
        let value: Value = serde_json::from_str(&rendered).expect("output is JSON");
        assert_eq!(value["id"], 1);
        assert_eq!(value["prompt_version"], prompt::PROMPT_VERSION);
    }

    #[test]
    fn lists_and_members_are_optional() {
        let input = line(json!({
            "now": "2026-10-30T11:05:00+03:00",
            "timezone": "Europe/Moscow",
        }));
        assert!(render_line(&input).is_ok());
    }

    #[test]
    fn unknown_keys_are_errors_at_every_level() {
        let mut record = json!({
            "id": 1,
            "context": context(),
            "text": "купить молоко",
            "txt": "другой текст",
        });
        assert!(error(&record.to_string()).contains("txt"));

        record = json!({"id": 1, "context": context(), "text": "купить молоко"});
        record["context"]["member"] = json!(["Анна"]);
        let message = error(&record.to_string());
        assert!(message.contains("member"), "{message}");

        record = json!({"id": 1, "context": context(), "text": "купить молоко"});
        record["context"]["lists"][0]["defualt"] = json!(true);
        let message = error(&record.to_string());
        assert!(message.contains("defualt"), "{message}");
    }

    #[test]
    fn author_must_be_a_member() {
        let mut record = json!({"id": 1, "context": context(), "text": "купить молоко"});
        record["context"]["author"] = json!("Оля");
        assert_eq!(
            error(&record.to_string()),
            "context.author должен быть среди context.members"
        );

        record["context"]["author"] = Value::Null;
        assert!(render_line(&record.to_string()).is_ok());
    }

    #[test]
    fn unrelated_field_types_are_errors() {
        let mut record = json!({"id": 1, "context": context(), "text": "купить молоко"});
        record["context"]["lists"] = json!({});
        assert!(!error(&record.to_string()).is_empty());

        record["context"]["members"] = json!("Анна");
        assert!(!error(&record.to_string()).is_empty());

        record["context"]["lists"] = json!([]);
        record["text"] = json!(1);
        assert!(!error(&record.to_string()).is_empty());
    }
}
