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
//! Текст фразы в сообщения об ошибках не попадает: он может быть
//! пользовательскими данными, а разбирать датасет удобнее по номеру строки.

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use serde::Serialize;
use serde_json::Value;
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

/// Отрисовать одну строку JSONL: вернуть готовую строку выхода.
///
/// Ошибка содержит `id`, если он есть во входе, но никогда не содержит текст
/// фразы.
pub fn render_line(line: &str) -> Result<String, RenderError> {
    let value: Value = serde_json::from_str(line)
        .map_err(|_| RenderError::new(None, "строка не является JSON"))?;
    let id = value.get("id").and_then(record_id);
    let record = render_record(&value).map_err(|message| RenderError {
        id: id.clone(),
        message,
    })?;
    serde_json::to_string(&record).map_err(|_| RenderError {
        id,
        message: "не удалось сериализовать запись".to_owned(),
    })
}

fn render_record(value: &Value) -> Result<RenderedRecord, String> {
    let object = value
        .as_object()
        .ok_or_else(|| "запись должна быть JSON-объектом".to_owned())?;
    let id = match object.get("id") {
        Some(value @ (Value::String(_) | Value::Number(_))) => value.clone(),
        Some(_) => return Err("id должен быть строкой или числом".to_owned()),
        None => return Err("нет поля id".to_owned()),
    };
    let context = object
        .get("context")
        .ok_or_else(|| "нет поля context".to_owned())?;
    let context = context_from_json(context)?;
    let text = object
        .get("text")
        .and_then(Value::as_str)
        .ok_or_else(|| "нет поля text или оно не строка".to_owned())?;

    let request = prompt::build_request(&context, text);
    let payload = request_payload(&request.messages, &request.schema);
    Ok(RenderedRecord {
        id,
        messages: payload.messages,
        schema: payload.schema,
        prompt_version: request.prompt_version,
        contract_hash: request.contract_hash,
    })
}

fn context_from_json(value: &Value) -> Result<CaptureContext, String> {
    let object = value
        .as_object()
        .ok_or_else(|| "context должен быть JSON-объектом".to_owned())?;
    Ok(CaptureContext {
        now: parse_now(object.get("now"))?,
        timezone: parse_household_timezone(object.get("timezone"))?,
        lists: parse_lists(object.get("lists"))?,
        members: parse_members(object.get("members"))?,
        author: parse_author(object.get("author"))?,
    })
}

fn parse_now(value: Option<&Value>) -> Result<DateTime<Utc>, String> {
    let value = value
        .and_then(Value::as_str)
        .ok_or_else(|| "нет поля context.now или оно не строка".to_owned())?;
    DateTime::parse_from_rfc3339(value)
        .map(|moment| moment.with_timezone(&Utc))
        .map_err(|_| "context.now должен быть моментом RFC 3339 с поясом".to_owned())
}

fn parse_household_timezone(value: Option<&Value>) -> Result<Tz, String> {
    let value = value
        .and_then(Value::as_str)
        .ok_or_else(|| "нет поля context.timezone или оно не строка".to_owned())?;
    parse_timezone(value).map_err(|_| "context.timezone должен быть поясом IANA".to_owned())
}

fn parse_lists(value: Option<&Value>) -> Result<Vec<ListRef>, String> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let items = value
        .as_array()
        .ok_or_else(|| "context.lists должен быть массивом".to_owned())?;
    items.iter().map(parse_list).collect()
}

fn parse_list(value: &Value) -> Result<ListRef, String> {
    let object = value
        .as_object()
        .ok_or_else(|| "элемент context.lists должен быть объектом".to_owned())?;
    let name = object
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| "у списка нет поля name или оно не строка".to_owned())?;
    let kind = match object.get("kind").and_then(Value::as_str) {
        Some("tasks") => ListKind::Tasks,
        Some("shopping") => ListKind::Shopping,
        Some("checklist") => ListKind::Checklist,
        Some("events") => ListKind::Events,
        Some(_) | None => {
            return Err(
                "неизвестный вид списка: ожидается tasks, shopping, checklist или events"
                    .to_owned(),
            );
        }
    };
    let is_default = match object.get("default") {
        None => false,
        Some(Value::Bool(value)) => *value,
        Some(_) => return Err("признак default у списка должен быть булевым".to_owned()),
    };
    Ok(ListRef {
        id: Uuid::nil(),
        name: name.to_owned(),
        kind,
        is_default,
    })
}

fn parse_members(value: Option<&Value>) -> Result<Vec<MemberRef>, String> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let items = value
        .as_array()
        .ok_or_else(|| "context.members должен быть массивом".to_owned())?;
    items
        .iter()
        .map(|item| {
            item.as_str()
                .map(member)
                .ok_or_else(|| "имя участника должно быть строкой".to_owned())
        })
        .collect()
}

fn parse_author(value: Option<&Value>) -> Result<Option<MemberRef>, String> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(name)) => Ok(Some(member(name))),
        Some(_) => Err("context.author должен быть именем или null".to_owned()),
    }
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
