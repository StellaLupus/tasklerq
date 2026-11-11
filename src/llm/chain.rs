//! Цепочка слотов: бюджет времени, переход на резервный слот и предохранитель.
//!
//! [`Slot`] — один OpenAI-совместимый эндпоинт со своим таймаутом и отдельным
//! [`CircuitBreaker`]. [`Chain`] исполняет разбор по основному и необязательному
//! резервному слоту и возвращает список попыток для записи в `llm_runs`.
//!
//! Время приходит снаружи: [`Clock`] даёт потраченное время, а `now` — момент
//! для предохранителей. Тесты подменяют часы и не ждут секунды.

use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

use super::{ChatBackend, ChatRequest, LlmError, OpenAiBackend};
use crate::capture::breaker::CircuitBreaker;
use crate::config::{LlmConfig, SlotConfig};
use crate::metrics;

/// Имя основного слота в `llm_runs.slot`.
pub const PRIMARY_SLOT: &str = "primary";
/// Имя резервного слота в `llm_runs.slot`.
pub const FALLBACK_SLOT: &str = "fallback";

/// Причина деградации, когда ни один слот не вызывался: предохранители
/// настроенных слотов разомкнуты.
const CIRCUIT_OPEN: &str = "circuit_open";

/// Часы разбора: разница двух значений даёт длительность попытки.
///
/// Реализация по умолчанию — [`SystemClock`]; тесты подменяют часы, чтобы не
/// ждать реальные секунды бюджета.
pub trait Clock: Send + Sync {
    /// Сколько времени прошло с начала работы.
    fn elapsed(&self) -> Duration;
}

/// Часы процесса на [`Instant`].
#[derive(Debug)]
pub struct SystemClock {
    start: Instant,
}

impl SystemClock {
    /// Запустить часы.
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn elapsed(&self) -> Duration {
        self.start.elapsed()
    }
}

/// Слот LLM: бэкенд, таймаут и собственный предохранитель.
pub struct Slot {
    name: &'static str,
    backend: Arc<dyn ChatBackend>,
    timeout: Duration,
    breaker: CircuitBreaker,
}

impl Slot {
    /// Создать слот с явным бэкендом. Нужен и рабочей сборке, и тестам с
    /// двойниками.
    pub fn new(name: &'static str, backend: Arc<dyn ChatBackend>, timeout: Duration) -> Self {
        Self {
            name,
            backend,
            timeout,
            breaker: CircuitBreaker::default(),
        }
    }

    /// Создать слот из конфигурации: адрес, модель, токен и дополнительные
    /// параметры уже разобраны из `LLM_*`.
    pub fn from_config(
        name: &'static str,
        config: &SlotConfig,
        connect_timeout: Duration,
    ) -> Result<Self, LlmError> {
        let backend = OpenAiBackend::from_slot(config, connect_timeout)?;
        Ok(Self::new(name, Arc::new(backend), config.timeout))
    }

    /// Имя слота для `llm_runs` и журнала.
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// Таймаут одного запроса модели.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Предохранитель слота.
    pub fn breaker(&self) -> &CircuitBreaker {
        &self.breaker
    }
}

impl fmt::Debug for Slot {
    /// Бэкенд не печатается: его `Debug` может содержать настройки провайдера.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Slot")
            .field("name", &self.name)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

/// Отказ разборщика ответа.
///
/// Разборщик — замыкание, переданное в [`Chain::run`]: оно не знает о слотах и
/// сообщает только вид отказа.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResponseFailure<E> {
    /// Ответ не прошёл разбор формы: переход на резервный слот, сбой в
    /// предохранителе.
    Shape {
        /// Короткий код для `llm_runs.error_code`.
        code: &'static str,
    },
    /// Ошибка пользователя (например, превышение числа элементов): перехода
    /// нет, предохранитель считает вызов успешным.
    User {
        /// Короткий код для `llm_runs.error_code`.
        code: &'static str,
        /// Ошибка для вызывающего; текст не хранится в `llm_runs`.
        error: E,
    },
}

/// Итог разбора цепочкой.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChainError<E> {
    /// Ни один слот не дал результата.
    Degraded {
        /// Код последней причины.
        reason: &'static str,
    },
    /// Ответ отвергнут как ошибка пользователя.
    User(E),
}

/// Один выполненный вызов слота.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attempt {
    /// Имя слота: `primary` или `fallback`.
    pub slot: &'static str,
    /// Номер попытки в разборе, с единицы.
    pub number: u16,
    /// Код ошибки; `None` у успешного вызова.
    pub error_code: Option<&'static str>,
    /// Ответ слота не прошёл разбор (форма или ошибка пользователя). Признак
    /// ведётся по виду отказа, а не по списку кодов: новый вариант разбора тоже
    /// попадёт в журнал `llm_validation_failed`.
    pub rejected_by_parse: bool,
    /// Длительность вызова.
    pub duration: Duration,
    /// Входные токены, если провайдер их вернул.
    pub input_tokens: Option<u32>,
    /// Выходные токены, если провайдер их вернул.
    pub output_tokens: Option<u32>,
    /// Имя модели из ответа, если провайдер его вернул.
    pub model: Option<String>,
}

impl Attempt {
    /// Вызов завершился успехом.
    pub fn is_success(&self) -> bool {
        self.error_code.is_none()
    }
}

/// Результат разбора и список выполненных попыток.
#[derive(Debug)]
pub struct ChainOutcome<T, E> {
    /// Разобранный ответ или отказ.
    pub result: Result<T, ChainError<E>>,
    /// Попытки в порядке выполнения; пусто, если предохранители не пустили.
    pub attempts: Vec<Attempt>,
}

/// Цепочка основного и необязательного резервного слота.
pub struct Chain {
    primary: Slot,
    fallback: Option<Slot>,
}

impl Chain {
    /// Собрать цепочку из готовых слотов.
    pub fn new(primary: Slot, fallback: Option<Slot>) -> Self {
        Self { primary, fallback }
    }

    /// Собрать цепочку из конфигурации. `None`, если разбор выключен или
    /// основной слот не задан.
    pub fn from_config(config: &LlmConfig) -> Result<Option<Self>, LlmError> {
        if !config.enabled {
            return Ok(None);
        }
        let Some(primary_config) = &config.primary else {
            return Ok(None);
        };
        let primary = Slot::from_config(PRIMARY_SLOT, primary_config, config.connect_timeout)?;
        let fallback = config
            .fallback
            .as_ref()
            .map(|slot| Slot::from_config(FALLBACK_SLOT, slot, config.connect_timeout))
            .transpose()?;
        Ok(Some(Self::new(primary, fallback)))
    }

    /// Основной слот.
    pub fn primary(&self) -> &Slot {
        &self.primary
    }

    /// Резервный слот, если настроен.
    pub fn fallback(&self) -> Option<&Slot> {
        self.fallback.as_ref()
    }

    /// Выполнить разбор в пределах бюджета источника.
    ///
    /// `budget` — бюджет источника (8 секунд для `ha_voice`, 20 для остальных);
    /// его выбирает вызывающий. `now` — момент для предохранителей, `clock` —
    /// источник потраченного времени. `parse` получает текст ответа и сообщает
    /// либо результат, либо отказ одного из двух видов.
    ///
    /// Каждый слот вызывается не больше одного раза. Переход на резервный —
    /// при ошибке транспорта, таймауте, любом `4xx`/`5xx`, пустом ответе и
    /// ответе, не прошедшем разбор формы. Ошибка пользователя перехода не
    /// вызывает и считается успехом предохранителя.
    pub async fn run<T, E, P>(
        &self,
        request: &ChatRequest<'_>,
        budget: Duration,
        now: DateTime<Utc>,
        clock: &dyn Clock,
        parse: P,
    ) -> ChainOutcome<T, E>
    where
        P: Fn(&str) -> Result<T, ResponseFailure<E>> + Send,
        T: Send,
        E: Send,
    {
        let mut attempts = Vec::new();
        let mut spent = Duration::ZERO;
        let mut number = 0u16;
        let mut reason = CIRCUIT_OPEN;

        if self.primary.breaker.allow(now) {
            // С резервным слотом основной получает не больше половины бюджета,
            // без резервного — весь бюджет в пределах своего таймаута.
            let allowance = if self.fallback.is_some() {
                self.primary.timeout.min(budget / 2)
            } else {
                self.primary.timeout.min(budget)
            };
            number += 1;
            let (attempt, outcome) = attempt(
                &self.primary,
                request,
                allowance,
                number,
                now,
                clock,
                &parse,
            )
            .await;
            spent = spent.saturating_add(attempt.duration);
            reason = attempt.error_code.unwrap_or(reason);
            attempts.push(attempt);
            match outcome {
                AttemptResult::Success(value) => {
                    return ChainOutcome {
                        result: Ok(value),
                        attempts,
                    };
                }
                AttemptResult::User(error) => {
                    return ChainOutcome {
                        result: Err(ChainError::User(error)),
                        attempts,
                    };
                }
                AttemptResult::Failed => {}
            }
        }

        if let Some(fallback) = &self.fallback {
            // Если основной был пропущен предохранителем, потрачено ноль и
            // резервный получает весь бюджет.
            let fallback_now = now + chrono::Duration::from_std(spent).unwrap_or_default();
            if fallback.breaker.allow(fallback_now) {
                let allowance = fallback.timeout.min(budget.saturating_sub(spent));
                number += 1;
                let (attempt, outcome) = attempt(
                    fallback,
                    request,
                    allowance,
                    number,
                    fallback_now,
                    clock,
                    &parse,
                )
                .await;
                reason = attempt.error_code.unwrap_or(reason);
                attempts.push(attempt);
                match outcome {
                    AttemptResult::Success(value) => {
                        return ChainOutcome {
                            result: Ok(value),
                            attempts,
                        };
                    }
                    AttemptResult::User(error) => {
                        return ChainOutcome {
                            result: Err(ChainError::User(error)),
                            attempts,
                        };
                    }
                    AttemptResult::Failed => {}
                }
            }
        }

        ChainOutcome {
            result: Err(ChainError::Degraded { reason }),
            attempts,
        }
    }
}

impl fmt::Debug for Chain {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Chain")
            .field("primary", &self.primary)
            .field("fallback", &self.fallback)
            .finish()
    }
}

/// Итог одного вызова слота для цикла [`Chain::run`].
enum AttemptResult<T, E> {
    Success(T),
    User(E),
    Failed,
}

/// Вызвать слот и учесть результат в его предохранителе.
///
/// Ожидание ограничено снаружи: зависший транспорт не должен держать разбор
/// дольше разрешённого времени. Превышение — код таймаута.
async fn attempt<T, E, P>(
    slot: &Slot,
    request: &ChatRequest<'_>,
    allowance: Duration,
    number: u16,
    now: DateTime<Utc>,
    clock: &dyn Clock,
    parse: &P,
) -> (Attempt, AttemptResult<T, E>)
where
    P: Fn(&str) -> Result<T, ResponseFailure<E>> + Send,
    T: Send,
    E: Send,
{
    let started = clock.elapsed();
    let completed =
        tokio::time::timeout(allowance, slot.backend.complete(request, allowance)).await;
    let duration = match &completed {
        Ok(_) => clock.elapsed().saturating_sub(started),
        // Внешний таймаут: часы могли не успеть продвинуться, а ожидание
        // заняло весь разрешённый интервал.
        Err(_) => allowance,
    };
    let finished_at = now + chrono::Duration::from_std(duration).unwrap_or_default();

    let (error_code, rejected_by_parse, input_tokens, output_tokens, model, outcome) =
        match completed {
            Err(_) => {
                slot.breaker.record_failure(finished_at);
                (
                    Some(LlmError::Timeout.code()),
                    false,
                    None,
                    None,
                    None,
                    AttemptResult::Failed,
                )
            }
            Ok(Err(error)) => {
                if matches!(&error, LlmError::Unauthorized) {
                    tracing::error!(
                        slot = slot.name,
                        code = error.code(),
                        "llm slot rejected the token"
                    );
                }
                slot.breaker.record_failure(finished_at);
                (
                    Some(error.code()),
                    false,
                    None,
                    None,
                    None,
                    AttemptResult::Failed,
                )
            }
            Ok(Ok(completion)) => {
                let input_tokens = completion.input_tokens;
                let output_tokens = completion.output_tokens;
                let model = (!completion.model.is_empty()).then_some(completion.model);
                match parse(&completion.content) {
                    Ok(value) => {
                        slot.breaker.record_success(finished_at);
                        (
                            None,
                            false,
                            input_tokens,
                            output_tokens,
                            model,
                            AttemptResult::Success(value),
                        )
                    }
                    Err(ResponseFailure::Shape { code }) => {
                        slot.breaker.record_failure(finished_at);
                        (
                            Some(code),
                            true,
                            input_tokens,
                            output_tokens,
                            model,
                            AttemptResult::Failed,
                        )
                    }
                    Err(ResponseFailure::User { code, error }) => {
                        // Ошибка пользователя не размыкает предохранитель: слот
                        // ответил, повтор не исправит ввод.
                        slot.breaker.record_success(finished_at);
                        (
                            Some(code),
                            true,
                            input_tokens,
                            output_tokens,
                            model,
                            AttemptResult::User(error),
                        )
                    }
                }
            }
        };

    let status = if error_code.is_none() {
        "success"
    } else {
        "failure"
    };
    metrics::llm_request(slot.name, status);
    metrics::llm_latency(slot.name, duration.as_secs_f64());

    (
        Attempt {
            slot: slot.name,
            number,
            error_code,
            rejected_by_parse,
            duration,
            input_tokens,
            output_tokens,
            model,
        },
        outcome,
    )
}
