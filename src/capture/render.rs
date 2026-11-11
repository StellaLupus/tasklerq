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
    let record: InputRecord =
        serde_json::from_value(value).map_err(|error| format!("неверная запись: {error}"))?;
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
