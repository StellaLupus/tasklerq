//! Общий двойник слота LLM и построитель ответа `capture-v4`.
//!
//! Двойник заменяет [`ChatBackend`]: ответы и ошибки задаются очередью,
//! задержка — общая для всех вызовов, счётчик вызовов и запросы доступны
//! тесту. Ответы строятся полными объектами `capture-v4`: пропущенное поле
//! формы — ошибка разбора, поэтому умолчания задаёт [`v4`].

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tasklerq::llm::{
    Chain, ChatBackend, ChatCompletion, ChatMessage, ChatRequest, FALLBACK_SLOT, LlmError,
    PRIMARY_SLOT, Slot,
};

/// Один перехваченный запрос слота.
#[derive(Debug, Clone)]
pub struct CapturedRequest {
    /// Сообщения чата.
    pub messages: Vec<ChatMessage>,
    /// Имя схемы ответа.
    pub schema_name: String,
    /// JSON Schema ответа.
    pub schema: Value,
    /// Таймаут, переданный слоту.
    pub timeout: Duration,
}

/// Двойник слота LLM с очередью ответов.
pub struct BackendDouble {
    answers: Mutex<VecDeque<Result<Value, LlmError>>>,
    repeat: Option<Result<Value, LlmError>>,
    delay: Duration,
    calls: AtomicUsize,
    requests: Mutex<Vec<CapturedRequest>>,
}

impl BackendDouble {
    /// Двойник с очередью ответов; пустая очередь отвечает ошибкой транспорта.
    pub fn new(answers: Vec<Result<Value, LlmError>>) -> Arc<Self> {
        Self::from_parts(answers, None, Duration::ZERO)
    }

    /// Двойник, который отвечает одним и тем же значением сколько угодно раз.
    pub fn always(answer: Result<Value, LlmError>) -> Arc<Self> {
        Self::from_parts(Vec::new(), Some(answer), Duration::ZERO)
    }

    /// Двойник с задержкой перед каждым ответом.
    pub fn delayed(answers: Vec<Result<Value, LlmError>>, delay: Duration) -> Arc<Self> {
        Self::from_parts(answers, None, delay)
    }

    /// Двойник, который молчит дольше любого бюджета: внешний таймаут цепочки
    /// должен оборвать ожидание.
    pub fn hanging() -> Arc<Self> {
        Self::from_parts(Vec::new(), None, Duration::from_secs(30))
    }

    fn from_parts(
        answers: Vec<Result<Value, LlmError>>,
        repeat: Option<Result<Value, LlmError>>,
        delay: Duration,
    ) -> Arc<Self> {
        Arc::new(Self {
            answers: Mutex::new(answers.into()),
            repeat,
            delay,
            calls: AtomicUsize::new(0),
            requests: Mutex::new(Vec::new()),
        })
    }

    /// Сколько раз слот был вызван.
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// Таймауты, переданные слоту, в порядке вызовов.
    pub fn timeouts(&self) -> Vec<Duration> {
        self.requests()
            .into_iter()
            .map(|request| request.timeout)
            .collect()
    }

    /// Перехваченные запросы в порядке вызовов.
    pub fn requests(&self) -> Vec<CapturedRequest> {
        self.requests.lock().expect("requests lock").clone()
    }
}

#[async_trait]
impl ChatBackend for BackendDouble {
    async fn complete(
        &self,
        request: &ChatRequest<'_>,
        timeout: Duration,
    ) -> Result<ChatCompletion, LlmError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.requests
            .lock()
            .expect("requests lock")
            .push(CapturedRequest {
                messages: request.messages.to_vec(),
                schema_name: request.schema_name.to_owned(),
                schema: request.schema.clone(),
                timeout,
            });
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        let answer = self
            .answers
            .lock()
            .expect("answers lock")
            .pop_front()
            .or_else(|| self.repeat.clone())
            .unwrap_or(Err(LlmError::Transport));
        answer.map(|value| ChatCompletion {
            content: value.to_string(),
            input_tokens: Some(10),
            output_tokens: Some(5),
            model: "stub/model".to_owned(),
        })
    }
}

/// Цепочка из двойников: основной слот и необязательный резервный.
pub fn chain(primary: Arc<BackendDouble>, fallback: Option<Arc<BackendDouble>>) -> Chain {
    chain_with_timeouts(
        primary,
        Duration::from_secs(20),
        fallback,
        Duration::from_secs(20),
    )
}

/// Цепочка из двойников с явными таймаутами слотов.
pub fn chain_with_timeouts(
    primary: Arc<BackendDouble>,
    primary_timeout: Duration,
    fallback: Option<Arc<BackendDouble>>,
    fallback_timeout: Duration,
) -> Chain {
    chain_from(
        primary,
        primary_timeout,
        fallback.map(|fallback| (fallback, fallback_timeout)),
    )
}

/// Цепочка из произвольных бэкендов.
pub fn chain_from(
    primary: Arc<BackendDouble>,
    primary_timeout: Duration,
    fallback: Option<(Arc<BackendDouble>, Duration)>,
) -> Chain {
    let primary: Arc<dyn ChatBackend> = primary;
    let fallback = fallback.map(|(backend, timeout)| {
        let backend: Arc<dyn ChatBackend> = backend;
        Slot::new(FALLBACK_SLOT, backend, timeout)
    });
    Chain::new(Slot::new(PRIMARY_SLOT, primary, primary_timeout), fallback)
}

/// Разомкнуть предохранитель основного слота.
pub fn open_primary(chain: &Chain, now: chrono::DateTime<chrono::Utc>) {
    for _ in 0..=tasklerq::capture::breaker::DEFAULT_THRESHOLD {
        chain.primary().breaker().record_failure(now);
    }
}

/// Разомкнуть предохранитель резервного слота.
pub fn open_fallback(chain: &Chain, now: chrono::DateTime<chrono::Utc>) {
    if let Some(fallback) = chain.fallback() {
        for _ in 0..=tasklerq::capture::breaker::DEFAULT_THRESHOLD {
            fallback.breaker().record_failure(now);
        }
    }
}

/// Построитель ответа `capture-v4`: полные элементы трёх форм.
pub mod v4 {
    use serde_json::{Value, json};

    /// Ответ с элементами и пустыми уточнениями.
    pub fn response(items: Vec<Value>) -> Value {
        json!({"items": items, "clarifications": []})
    }

    /// Ответ с элементами и уточнениями.
    pub fn response_with(items: Vec<Value>, clarifications: Value) -> Value {
        json!({"items": items, "clarifications": clarifications})
    }

    /// Выражение «дата не названа».
    pub fn date_none() -> Value {
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

    /// Смещение в днях, неделях, месяцах или годах.
    pub fn date_offset(n: i64, unit: &str) -> Value {
        json!({
            "kind": "offset",
            "n": n,
            "unit": unit,
            "weekday": "",
            "day": 0,
            "month": 0,
            "year": 0,
        })
    }

    /// День недели; `next` — «следующий», а не ближайший.
    pub fn date_weekday(weekday: &str, next: bool) -> Value {
        json!({
            "kind": "weekday",
            "n": if next { 1 } else { 0 },
            "unit": "",
            "weekday": weekday,
            "day": 0,
            "month": 0,
            "year": 0,
        })
    }

    /// Абсолютная дата; месяц и год `0` означают «не названы».
    pub fn date(day: i64, month: i64, year: i64) -> Value {
        json!({
            "kind": "date",
            "n": 0,
            "unit": "",
            "weekday": "",
            "day": day,
            "month": month,
            "year": year,
        })
    }

    /// Абсолютная дата из строки `YYYY-MM-DD`.
    pub fn date_ymd(text: &str) -> Value {
        let parsed = chrono::NaiveDate::parse_from_str(text.trim(), "%Y-%m-%d")
            .expect("due date is YYYY-MM-DD");
        date(
            i64::from(chrono::Datelike::day(&parsed)),
            i64::from(chrono::Datelike::month(&parsed)),
            i64::from(chrono::Datelike::year(&parsed)),
        )
    }

    /// Правило повторения.
    pub fn repeat(freq: &str, weekdays: Vec<i64>) -> Value {
        json!({
            "freq": freq,
            "interval": 1,
            "weekdays": weekdays,
            "month_rule": "",
            "until": date_none(),
        })
    }

    /// Задача с пустыми необязательными полями.
    pub fn task(title: &str) -> Value {
        json!({
            "kind": "tasks",
            "list": "",
            "title": title,
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

    /// Покупка с пустыми необязательными полями.
    pub fn shopping(title: &str) -> Value {
        json!({
            "kind": "shopping",
            "list": "",
            "title": title,
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

    /// Пункт чек-листа.
    pub fn checklist(title: &str) -> Value {
        let mut item = shopping(title);
        item["kind"] = json!("checklist");
        item
    }

    /// Событие с пустыми необязательными полями.
    pub fn event(title: &str) -> Value {
        json!({
            "kind": "events",
            "list": "",
            "title": title,
            "date": date_none(),
            "time": "",
            "end": date_none(),
            "end_time": "",
            "repeat": null,
            "assignee": "",
            "tags": [],
            "notes": "",
        })
    }

    /// Заменить поле элемента.
    pub fn with(mut item: Value, key: &str, value: Value) -> Value {
        item.as_object_mut()
            .expect("item is an object")
            .insert(key.to_owned(), value);
        item
    }
}
