//! Один разбор через цепочку слотов LLM: запрос `capture-v4` и разбор ответа.
//!
//! Логика одинакова для Telegram-воркера и синхронного HTTP-capture, поэтому
//! вынесена сюда: сборка запроса, бюджет источника и превращение ответа
//! `capture-v4` в [`ParsedResponse`] не должны расходиться между путями.
//! Метрики запросов и длительности пишет [`Chain`], ошибку разбора формы —
//! [`generate`]: каждый ответ, не прошедший разбор, увеличивает
//! `llm_validation_failures_total`.

use std::time::Duration;

use chrono::{DateTime, Utc};

use super::context::CaptureContext;
use super::convert;
use super::prompt;
use super::schema::{ParseError, ParsedResponse};
use super::wire::{self, Clarification, ClarificationTopic};
use crate::llm::{Attempt, Chain, ChainError, ChatRequest, ResponseFailure, SystemClock};
use crate::metrics;

/// Бюджет разбора источников без голосового ограничения: Telegram и прочий
/// внешний capture. Голосовой источник задаёт свой бюджет из
/// `CAPTURE_VOICE_TIMEOUT_MS`.
pub const DEFAULT_BUDGET: Duration = Duration::from_secs(20);

/// Успешный разбор.
#[derive(Debug)]
pub struct Generated {
    /// Разобранный ответ в форме, которую ждут валидатор и черновик.
    pub parsed: ParsedResponse,
    /// Суммарная длительность выполненных попыток.
    pub latency_ms: i64,
    /// Момент окончания разбора.
    pub finished_at: DateTime<Utc>,
}

/// Причина неудачи разбора.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// Ни один слот не дал разобранный ответ.
    Degraded {
        /// Код последней причины: `circuit_open` или код ошибки слота.
        reason: &'static str,
        /// Код ответа, не прошедшего разбор формы, если такой был.
        validation_code: Option<&'static str>,
    },
    /// Ошибка пользователя (например, превышение числа элементов).
    User(ParseError),
}

impl Failure {
    /// Короткий код для `llm_runs.error_code` без пользовательских данных.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Degraded { reason, .. } => reason,
            Self::User(error) => error.code(),
        }
    }

    /// Код ответа, не прошедшего разбор формы. По нему пишется журнал
    /// безопасности; `None`, если ответа формы не было вовсе.
    pub fn validation_code(&self) -> Option<&'static str> {
        match self {
            Self::Degraded {
                validation_code, ..
            } => *validation_code,
            Self::User(error) => Some(error.code()),
        }
    }

    /// Повтор задания может помочь: сбой транспорта или сервера слота.
    /// Таймаут, ошибка формы и ошибка пользователя повтором не лечатся.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::Degraded {
                reason: "transport" | "server_error",
                ..
            }
        )
    }
}

/// Неудачный разбор.
#[derive(Debug)]
pub struct Failed {
    /// Что именно пошло не так.
    pub failure: Failure,
    /// Суммарная длительность выполненных попыток.
    pub latency_ms: i64,
    /// Момент окончания разбора.
    pub finished_at: DateTime<Utc>,
}

/// Итог разбора и список выполненных попыток для записи в `llm_runs`.
#[derive(Debug)]
pub struct Outcome {
    /// Разобранный ответ или отказ.
    pub result: Result<Generated, Failed>,
    /// Попытки в порядке выполнения; пусто, если предохранители не пустили.
    pub attempts: Vec<Attempt>,
}

/// Разобрать текст через цепочку слотов. `now` — время до вызова; результат
/// получает время окончания, чтобы вызывающий не читал часы повторно.
///
/// `budget` — бюджет источника: 8 секунд для `ha_voice`, 20 для остальных.
/// Цепочка сама делит его между слотами и пишет метрики с меткой слота.
pub async fn generate(
    chain: &Chain,
    context: &CaptureContext,
    text: &str,
    now: DateTime<Utc>,
    budget: Duration,
) -> Outcome {
    let request = prompt::build_request(context, text);
    let chat = ChatRequest {
        messages: &request.messages,
        schema_name: prompt::SCHEMA_NAME,
        schema: &request.schema,
    };

    let outcome = chain
        .run(&chat, budget, now, &SystemClock::new(), |content| {
            parse(content, context)
        })
        .await;

    let latency: Duration = outcome
        .attempts
        .iter()
        .map(|attempt| attempt.duration)
        .sum();
    let latency_ms = i64::try_from(latency.as_millis()).unwrap_or(i64::MAX);
    let finished_at = now + chrono::Duration::from_std(latency).unwrap_or_default();
    let validation_code = outcome
        .attempts
        .iter()
        .rev()
        .find(|attempt| attempt.rejected_by_parse)
        .and_then(|attempt| attempt.error_code);

    let result = match outcome.result {
        Ok(parsed) => Ok(Generated {
            parsed,
            latency_ms,
            finished_at,
        }),
        Err(ChainError::User(error)) => Err(Failed {
            failure: Failure::User(error),
            latency_ms,
            finished_at,
        }),
        Err(ChainError::Degraded { reason }) => Err(Failed {
            failure: Failure::Degraded {
                reason,
                validation_code,
            },
            latency_ms,
            finished_at,
        }),
    };

    Outcome {
        result,
        attempts: outcome.attempts,
    }
}

/// Разобрать текст ответа в элементы черновика.
///
/// Слишком длинный список — ошибка пользователя: цепочка не идёт в резервный
/// слот и не размыкает предохранитель. Остальные нарушения формы — «форма не
/// подходит»: переход на резервный слот и сбой предохранителя.
fn parse(
    content: &str,
    context: &CaptureContext,
) -> Result<ParsedResponse, ResponseFailure<ParseError>> {
    match wire::parse_capture_response(content, context) {
        Ok(response) => Ok(ParsedResponse {
            items: response
                .items
                .iter()
                .map(|item| convert::convert_item(item, context))
                .collect(),
            clarifications: response
                .clarifications
                .iter()
                .map(clarification_question)
                .collect(),
        }),
        Err(error) => {
            metrics::llm_validation_failure();
            if matches!(error, ParseError::TooManyItems { .. }) {
                Err(ResponseFailure::User {
                    code: error.code(),
                    error,
                })
            } else {
                Err(ResponseFailure::Shape { code: error.code() })
            }
        }
    }
}

/// Текст вопроса по теме уточнения.
///
/// Черновик хранит уточнения строками до пункта 8.1, а кнопочные экраны для тем
/// `date` и `assignee` строятся из ошибок валидатора. Эта функция лишь
/// сохраняет прежний вид вопросов модели; в 8.2 вопросы строятся по теме.
fn clarification_question(clarification: &Clarification) -> String {
    match clarification.topic {
        ClarificationTopic::Date => "Когда?",
        ClarificationTopic::Time => "Во сколько?",
        ClarificationTopic::Assignee => "Кому поручить?",
        ClarificationTopic::List => "В какой список?",
    }
    .to_owned()
}
