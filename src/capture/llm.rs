//! Один вызов LLM для capture: промпт, предохранитель и разбор ответа.
//!
//! Логика одинакова для Telegram-воркера и синхронного HTTP-capture, поэтому
//! вынесена сюда: тайминг, запись успеха/ошибки в [`CircuitBreaker`] и разбор
//! формы ответа не должны расходиться между путями.

use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

use super::breaker::CircuitBreaker;
use super::context::CaptureContext;
use super::prompt;
use super::schema::{ParseError, ParsedResponse, parse_response};
use crate::home_assistant::{AiTask, GenerateData, HaError};

/// Успешный разбор.
#[derive(Debug)]
pub struct Generated {
    /// Разобранный ответ.
    pub parsed: ParsedResponse,
    /// Длительность вызова.
    pub latency_ms: i64,
    /// Момент окончания вызова.
    pub finished_at: DateTime<Utc>,
}

/// Причина неудачи вызова.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// Home Assistant не ответил или ответил ошибкой.
    Ai(HaError),
    /// Ответ пришёл, но его форма не подходит.
    Parse(ParseError),
}

impl Failure {
    /// Короткий код для `llm_runs.error_code` без пользовательских данных.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Ai(error) => error.code(),
            Self::Parse(error) => error.code(),
        }
    }

    /// Home Assistant или сеть дали временную ошибку.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Ai(error) => error.is_retryable(),
            Self::Parse(_) => false,
        }
    }

    /// Ответ пришёл, но не прошёл разбор схемы: такой вызов можно повторить
    /// один раз. Слишком длинный список — ошибка пользователя, её повтор не
    /// исправит.
    pub fn is_parse_retryable(&self) -> bool {
        matches!(self, Self::Parse(error) if !matches!(error, ParseError::TooManyItems { .. }))
    }
}

/// Неудачный вызов; предохранитель уже учёл результат.
#[derive(Debug)]
pub struct Failed {
    /// Что именно пошло не так.
    pub failure: Failure,
    /// Длительность вызова.
    pub latency_ms: i64,
    /// Момент окончания вызова.
    pub finished_at: DateTime<Utc>,
}

/// Вызвать LLM и разобрать ответ. `now` — время до вызова; результат получает
/// время окончания, чтобы вызывающий не читал часы повторно.
///
/// `timeout` — бюджет ожидания источника. Он передаётся клиенту и
/// дополнительно ограничивает сам вызов: зависший транспорт не должен держать
/// голосовой запрос дольше бюджета. Превышение считается [`HaError::Timeout`],
/// поэтому предохранитель видит обычный сбой.
pub async fn generate(
    ai: &dyn AiTask,
    breaker: &CircuitBreaker,
    context: &CaptureContext,
    text: &str,
    now: DateTime<Utc>,
    timeout: Duration,
) -> Result<Generated, Failed> {
    let instructions = prompt::build_instructions(context, text);
    let structure = prompt::response_structure();
    let request = GenerateData {
        task_name: prompt::TASK_NAME,
        instructions: &instructions,
        structure: &structure,
    };
    let started = Instant::now();
    let generated = tokio::time::timeout(timeout, ai.generate_data(&request, timeout))
        .await
        .unwrap_or(Err(HaError::Timeout));
    let elapsed = started.elapsed();
    let latency_ms = i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX);
    let finished_at = now + chrono::Duration::from_std(elapsed).unwrap_or_default();

    match generated {
        Err(error) => {
            breaker.record_failure(finished_at);
            Err(Failed {
                failure: Failure::Ai(error),
                latency_ms,
                finished_at,
            })
        }
        Ok(data) => match parse_response(&data) {
            Ok(parsed) => {
                breaker.record_success(finished_at);
                Ok(Generated {
                    parsed,
                    latency_ms,
                    finished_at,
                })
            }
            Err(error) => {
                // Слишком длинный список — ошибка пользователя, а не сбой LLM:
                // предохранитель на неё не размыкается.
                if matches!(error, ParseError::TooManyItems { .. }) {
                    breaker.record_success(finished_at);
                } else {
                    breaker.record_failure(finished_at);
                }
                Err(Failed {
                    failure: Failure::Parse(error),
                    latency_ms,
                    finished_at,
                })
            }
        },
    }
}
