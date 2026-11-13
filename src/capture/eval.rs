//! Оценка smoke-набора `capture-v4` для проверки слота.
//!
//! Набор (`src/capture/prompts/smoke-v4.json`, пункт 9.2) хранит версию контракта,
//! дату заморозки, фиксированное «сейчас» и фразы с контекстом в формате
//! `capture-render`. Эталон — полный ответ `capture-v4`; он проходит тот же
//! путь, что и ответ слота: [`wire`] → [`convert`] → [`validate`]. Поэтому
//! сравнение идёт по [`DraftItem`] после расчёта дат, а не по строкам ответа.
//!
//! Оценка не ходит в сеть и не трогает базу: прогон через слот и запись в
//! `llm_slot_checks` — задача команды `capture-eval` (9.3). Здесь чистая
//! функция [`judge`]: исход вызова слота (текст или ошибка клиента с кодом,
//! фактическая длительность) и таймаут слота дают причину провала.
//!
//! Названия сравниваются как множества основ слов (стеммер Snowball для
//! русского), без учёта регистра и порядка слов: «Оплатить интернет» и
//! «оплата интернета» — одно название.
//!
//! [`TitleMatch::Normalized`] дополнительно сводит видовые пары глаголов и
//! синонимы: «Вынести мусор» и «Выносить мусор» — одно название. Строгий режим
//! остаётся основным: нормализация нужна, чтобы отличить расхождение в смысле
//! от расхождения в форме.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;
use std::time::Duration;

use chrono::Timelike;
use rust_stemmers::{Algorithm, Stemmer};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::context::CaptureContext;
use super::convert::{convert_item, second_date_text};
use super::render::InputContext;
use super::schema::{Clarification, ClarificationTopic, RawItem, normalize_clarifications};
use super::validate::{DATE_MISSING, DraftItem, ItemError, validate_item};
use super::wire;
use crate::domain::models::{ItemSource, ListKind, MonthRule, RecurrenceFreq, RecurrenceInput};

/// Замороженный smoke-набор эталонных фраз.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SmokeSet {
    /// Версия контракта, для которой составлен набор.
    pub prompt_version: String,
    /// Дата заморозки эталонов.
    pub frozen_at: String,
    /// Контекст по умолчанию; фраза может его переопределить.
    pub context: InputContext,
    /// Фразы набора.
    pub phrases: Vec<SmokePhrase>,
}

impl SmokeSet {
    /// Разобрать JSON набора. Опечатка в имени поля — ошибка, а не другой набор;
    /// «сейчас» у всего набора одно.
    pub fn parse(text: &str) -> Result<Self, String> {
        let set: Self = serde_json::from_str(text).map_err(|error| error.to_string())?;
        set.check_single_now()?;
        Ok(set)
    }

    /// Контекст набора.
    pub fn context(&self) -> Result<CaptureContext, String> {
        self.context
            .clone()
            .into_context()
            .map_err(|error| format!("контекст набора: {error}"))
    }

    /// У фразы не может быть своего «сейчас»: дата набора должна быть общей,
    /// иначе эталон и ответ слота считались бы от разных моментов.
    fn check_single_now(&self) -> Result<(), String> {
        let now = self.context.now();
        for phrase in &self.phrases {
            if let Some(context) = &phrase.context
                && !same_moment(context.now(), now)
            {
                return Err(format!(
                    "фраза {}: «сейчас» в контексте фразы должно совпадать с «сейчас» набора",
                    phrase.id
                ));
            }
        }
        Ok(())
    }
}

/// Версия контракта набора — та же, что у действующего разбора.
///
/// Набор с другой версией не проверяет слот: эталоны относятся к прежнему
/// контракту, поэтому команда обязана ошибиться до вызовов LLM, а не сообщить
/// ложный провал слота. Проверка живёт в продуктовом коде, а не только в
/// тестах: её используют и `capture-eval`, и тест замороженного набора.
pub fn check_prompt_version(set: &SmokeSet) -> Result<(), String> {
    if set.prompt_version != super::prompt::PROMPT_VERSION {
        return Err(format!(
            "набор составлен для контракта {}, действующий — {}",
            set.prompt_version,
            super::prompt::PROMPT_VERSION
        ));
    }
    Ok(())
}

/// Один и тот же момент, даже если записан с другим смещением.
fn same_moment(left: &str, right: &str) -> bool {
    match (
        chrono::DateTime::parse_from_rfc3339(left),
        chrono::DateTime::parse_from_rfc3339(right),
    ) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

/// Одна фраза набора.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SmokePhrase {
    /// Стабильный идентификатор для отчёта.
    pub id: String,
    /// Категория покрытия.
    pub category: Category,
    /// Контекст, если он отличается от контекста набора.
    #[serde(default)]
    pub context: Option<InputContext>,
    /// Русский текст фразы.
    pub text: String,
    /// Эталон — полный ответ `capture-v4`.
    pub expect: Value,
}

impl SmokePhrase {
    /// Контекст фразы: собственный или контекст набора.
    pub fn effective_context<'a>(&'a self, default: &'a InputContext) -> &'a InputContext {
        self.context.as_ref().unwrap_or(default)
    }

    /// Контекст фразы, собранный для разбора.
    pub fn context(&self, default: &InputContext) -> Result<CaptureContext, String> {
        self.effective_context(default)
            .clone()
            .into_context()
            .map_err(|error| format!("фраза {}: контекст: {error}", self.id))
    }
}

/// Категория покрытия smoke-набора.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    /// Простые задачи.
    Simple,
    /// Покупки, в том числе несколько позиций в одной фразе.
    Shopping,
    /// Относительные даты.
    DatesRelative,
    /// Абсолютные даты.
    DatesAbsolute,
    /// Время суток.
    TimeOfDay,
    /// Повторения.
    Recurrence,
    /// Исполнители.
    Assignees,
    /// Неоднозначности, которые должны давать уточнения.
    Ambiguity,
    /// Мусор и не-задачи.
    Garbage,
    /// События с началом и концом.
    Events,
}

impl Category {
    /// Все категории, которые обязан покрывать набор.
    pub const ALL: [Self; 10] = [
        Self::Simple,
        Self::Shopping,
        Self::DatesRelative,
        Self::DatesAbsolute,
        Self::TimeOfDay,
        Self::Recurrence,
        Self::Assignees,
        Self::Ambiguity,
        Self::Garbage,
        Self::Events,
    ];

    /// Подпись для отчёта.
    pub fn label(self) -> &'static str {
        match self {
            Self::Simple => "простые задачи",
            Self::Shopping => "покупки",
            Self::DatesRelative => "относительные даты",
            Self::DatesAbsolute => "абсолютные даты",
            Self::TimeOfDay => "время суток",
            Self::Recurrence => "повторения",
            Self::Assignees => "исполнители",
            Self::Ambiguity => "неоднозначности",
            Self::Garbage => "мусор",
            Self::Events => "события",
        }
    }
}

/// Исход одного вызова слота.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlotAnswer {
    /// Текст ответа модели.
    Text(String),
    /// Ошибка клиента с коротким кодом (`LlmError::code`).
    Error {
        /// Код ошибки без пользовательских данных.
        code: String,
    },
}

/// Вызов слота: исход и фактическая длительность.
#[derive(Debug, Clone)]
pub struct SlotRun {
    /// Что вернул слот.
    pub answer: SlotAnswer,
    /// Сколько длился вызов.
    pub duration: Duration,
}

impl SlotRun {
    /// Слот ответил текстом за `duration`.
    pub fn text(text: impl Into<String>, duration: Duration) -> Self {
        Self {
            answer: SlotAnswer::Text(text.into()),
            duration,
        }
    }

    /// Слот завершился ошибкой клиента за `duration`.
    pub fn error(code: impl Into<String>, duration: Duration) -> Self {
        Self {
            answer: SlotAnswer::Error { code: code.into() },
            duration,
        }
    }
}

/// Расхождение «ожидали — получили» по одному полю.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Difference {
    /// Номер элемента фразы, если расхождение относится к элементу.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item: Option<usize>,
    /// Поле элемента.
    pub field: &'static str,
    /// Значение эталона.
    pub expected: String,
    /// Значение ответа слота.
    pub actual: String,
}

/// Причина провала фразы.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Failure {
    /// Ответ пришёл позже таймаута слота, даже если он верный.
    Timeout {
        /// Фактическая длительность в миллисекундах.
        actual_ms: u64,
    },
    /// Ошибка клиента слота.
    Transport {
        /// Код ошибки клиента.
        code: String,
    },
    /// Ответ не прошёл разбор формы.
    Schema {
        /// Код [`super::schema::ParseError`].
        code: String,
    },
    /// Элемент ответа не прошёл серверную валидацию.
    Validation {
        /// Номер элемента с нуля.
        item: usize,
        /// Поле с ошибкой.
        field: String,
        /// Код ошибки валидации.
        code: String,
    },
    /// Элементы разобраны, но отличаются от эталона.
    Mismatch {
        /// Перечень расхождений по полям.
        differences: Vec<Difference>,
    },
}

/// Результат одной фразы.
#[derive(Debug, Clone)]
pub struct PhraseResult {
    /// Идентификатор фразы.
    pub id: String,
    /// Категория.
    pub category: Category,
    /// Фактическая длительность вызова.
    pub duration: Duration,
    /// Причина провала; `None` — фраза пройдена.
    pub failure: Option<Failure>,
}

impl PhraseResult {
    /// Фраза пройдена: ответ верный и пришёл в пределах таймаута.
    pub fn passed(&self) -> bool {
        self.failure.is_none()
    }

    /// Длительность в миллисекундах для отчёта.
    pub fn duration_ms(&self) -> u64 {
        duration_ms(self.duration)
    }

    /// Запись отчёта без текста ответа модели.
    fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "category": self.category,
            "passed": self.passed(),
            "duration_ms": self.duration_ms(),
            "reason": self.failure,
        })
    }
}

/// Оценка задержки слота по 95-му перцентилю.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LatencyGrade {
    /// До 2 секунд.
    Wonderful,
    /// До 4 секунд.
    Good,
    /// До 6 секунд.
    Acceptable,
    /// До 10 секунд.
    Slow,
    /// Больше 10 секунд.
    Unacceptable,
}

impl LatencyGrade {
    /// Оценка по длительности. Границы включительные: ровно 2 с — ещё
    /// «замечательно», 2 с и микрон — уже «хорошо».
    pub fn of(latency: Duration) -> Self {
        if latency <= Duration::from_secs(2) {
            Self::Wonderful
        } else if latency <= Duration::from_secs(4) {
            Self::Good
        } else if latency <= Duration::from_secs(6) {
            Self::Acceptable
        } else if latency <= Duration::from_secs(10) {
            Self::Slow
        } else {
            Self::Unacceptable
        }
    }

    /// Подпись для читаемого отчёта.
    pub fn label(self) -> &'static str {
        match self {
            Self::Wonderful => "замечательно",
            Self::Good => "хорошо",
            Self::Acceptable => "приемлемо",
            Self::Slow => "медленно",
            Self::Unacceptable => "неприемлемо",
        }
    }
}

/// Итог проверки набора.
#[derive(Debug, Clone)]
pub struct Report {
    /// Версия контракта набора.
    pub prompt_version: String,
    /// Дата заморозки набора.
    pub frozen_at: String,
    /// Режим сравнения названий, которым получен вердикт.
    pub titles: TitleMatch,
    /// Результаты в порядке набора.
    pub results: Vec<PhraseResult>,
}

impl Report {
    /// Сколько фраз в наборе.
    pub fn total(&self) -> usize {
        self.results.len()
    }

    /// Сколько фраз пройдено.
    pub fn passed_count(&self) -> usize {
        self.results.iter().filter(|result| result.passed()).count()
    }

    /// Слот прошёл проверку: пройдены все фразы.
    pub fn passed(&self) -> bool {
        self.total() > 0 && self.passed_count() == self.total()
    }

    /// Провалившиеся фразы.
    pub fn failed(&self) -> impl Iterator<Item = &PhraseResult> {
        self.results.iter().filter(|result| !result.passed())
    }

    /// 95-й перцентиль длительности по правилу ближайшего ранга.
    pub fn p95(&self) -> Duration {
        let mut values: Vec<Duration> = self.results.iter().map(|result| result.duration).collect();
        if values.is_empty() {
            return Duration::ZERO;
        }
        values.sort();
        let rank = (95 * values.len()).div_ceil(100).max(1);
        values[rank - 1]
    }

    /// Оценка задержки по [`Report::p95`].
    pub fn grade(&self) -> LatencyGrade {
        LatencyGrade::of(self.p95())
    }

    /// Отчёт для `llm_slot_checks.report`: причины и поля, без текста ответов.
    pub fn to_json(&self) -> Value {
        json!({
            "prompt_version": self.prompt_version,
            "frozen_at": self.frozen_at,
            "titles": self.titles.as_str(),
            "total": self.total(),
            "passed": self.passed_count(),
            "latency_p95_ms": duration_ms(self.p95()),
            "latency_grade": self.grade(),
            "phrases": self.results.iter().map(PhraseResult::to_json).collect::<Vec<_>>(),
        })
    }

    /// Читаемый отчёт: итог, каждая фраза с фактическим временем и причиной
    /// провала, вердикт по слоту.
    pub fn to_text(&self) -> String {
        let mut text = format!(
            "Набор {} от {}: пройдено {} из {}\nЗадержка p95: {} — {}\n",
            self.prompt_version,
            self.frozen_at,
            self.passed_count(),
            self.total(),
            seconds(self.p95()),
            self.grade().label(),
        );
        for result in &self.results {
            text.push_str(&format!(
                "{} ({}): {} — {}\n",
                result.id,
                result.category.label(),
                seconds(result.duration),
                failure_text(result.failure.as_ref()),
            ));
        }
        if self.passed() {
            text.push_str("Слот прошёл проверку\n");
        } else {
            text.push_str(&format!(
                "Слот не прошёл проверку: пройдено {} из {}\n",
                self.passed_count(),
                self.total()
            ));
        }
        text
    }
}

/// Оценить набор по исходам вызова слота.
///
/// Каждая фраза набора обязана иметь исход: пропуск — ошибка вызывающего кода,
/// а не провал слота.
pub fn evaluate(
    set: &SmokeSet,
    runs: &BTreeMap<String, SlotRun>,
    timeout: Duration,
) -> Result<Report, String> {
    evaluate_with(set, runs, timeout, TitleMatch::Strict)
}

/// Оценить набор в выбранном режиме сравнения названий.
///
/// Прогон (вызовы слота) не повторяется: один и тот же набор исходов можно
/// оценить и строго, и нормализованно.
pub fn evaluate_with(
    set: &SmokeSet,
    runs: &BTreeMap<String, SlotRun>,
    timeout: Duration,
    titles: TitleMatch,
) -> Result<Report, String> {
    let mut results = Vec::with_capacity(set.phrases.len());
    for phrase in &set.phrases {
        let run = runs
            .get(&phrase.id)
            .ok_or_else(|| format!("фраза {}: нет исхода вызова слота", phrase.id))?;
        results.push(judge_with(phrase, &set.context, run, timeout, titles)?);
    }
    Ok(Report {
        prompt_version: set.prompt_version.clone(),
        frozen_at: set.frozen_at.clone(),
        titles,
        results,
    })
}

/// Как сравнивать названия элементов.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TitleMatch {
    /// Только основы слов: «Вынести мусор» и «Выносить мусор» — разные.
    #[default]
    Strict,
    /// Основы плюс вид глагола и синонимы: «Вынести мусор» и
    /// «Выносить мусор» — одно название.
    Normalized,
}

impl TitleMatch {
    /// Имя режима для отчёта и логов.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Strict => "strict",
            Self::Normalized => "normalized",
        }
    }
}

/// Оценить одну фразу строго (совместимость).
///
/// См. [`judge_with`].
pub fn judge(
    phrase: &SmokePhrase,
    default_context: &InputContext,
    run: &SlotRun,
    timeout: Duration,
) -> Result<PhraseResult, String> {
    judge_with(phrase, default_context, run, timeout, TitleMatch::Strict)
}

/// Оценить одну фразу.
///
/// Сначала проверяется таймаут слота: верный, но медленный ответ — провал с
/// причиной [`Failure::Timeout`] и фактическим временем. Ошибка набора или
/// эталона — ошибка вызывающего кода, а не причина провала слота.
///
/// Ошибка валидации, которую объясняет уточнение ответа (событие без начала и
/// вопрос о дате), провалом не считается: контракт предписывает именно такой
/// ответ, а поле дозаполняется ответом на вопрос.
pub fn judge_with(
    phrase: &SmokePhrase,
    default_context: &InputContext,
    run: &SlotRun,
    timeout: Duration,
    titles: TitleMatch,
) -> Result<PhraseResult, String> {
    let context = phrase.context(default_context)?;
    let reference = reference(&context, &phrase.expect)
        .map_err(|error| format!("фраза {}: эталон: {error}", phrase.id))?;
    let mut result = PhraseResult {
        id: phrase.id.clone(),
        category: phrase.category,
        duration: run.duration,
        failure: None,
    };

    // Ошибка клиента важнее длительности, кроме собственного таймаута клиента:
    // это и есть причина `timeout`, и она несёт фактическое время ожидания.
    // Клиентский `timeout` — таймаут, даже если ожидание оказалось короче
    // таймаута слота: ответа всё равно нет, а причина провала — время.
    let text = match &run.answer {
        SlotAnswer::Error { code } => {
            result.failure = Some(if code == "timeout" {
                Failure::Timeout {
                    actual_ms: result.duration_ms(),
                }
            } else {
                Failure::Transport { code: code.clone() }
            });
            return Ok(result);
        }
        SlotAnswer::Text(text) => text,
    };

    if run.duration > timeout {
        result.failure = Some(Failure::Timeout {
            actual_ms: result.duration_ms(),
        });
        return Ok(result);
    }
    let response = match wire::parse_capture_response(text, &context) {
        Ok(response) => response,
        Err(error) => {
            result.failure = Some(Failure::Schema {
                code: error.code().to_owned(),
            });
            return Ok(result);
        }
    };
    let actual = into_draft(&context, response);
    if let Some((index, error)) = first_error(&actual.items, &actual.clarifications) {
        result.failure = Some(Failure::Validation {
            item: index,
            field: error.field.to_owned(),
            code: error.code.to_owned(),
        });
        return Ok(result);
    }

    let differences = compare_with(&reference, &actual, &context, titles);
    if !differences.is_empty() {
        result.failure = Some(Failure::Mismatch { differences });
    }
    Ok(result)
}

/// Разобранный ответ: элементы после валидации, вторые даты заметки и
/// уточнения. Один и тот же вид у эталона и у ответа слота.
struct Draft {
    items: Vec<DraftItem>,
    /// Вторая дата фразы по индексу элемента; свободный текст заметки в
    /// сравнение не входит.
    note_dates: Vec<Option<String>>,
    clarifications: Vec<Clarification>,
}

fn reference(context: &CaptureContext, value: &Value) -> Result<Draft, String> {
    let response = wire::parse_capture_value(value, context)
        .map_err(|error| format!("ответ не разобран: {}", error.code()))?;
    let draft = into_draft(context, response);
    if let Some((index, error)) = first_error(&draft.items, &draft.clarifications) {
        return Err(format!(
            "элемент {index}: поле {} ({})",
            error.field, error.code
        ));
    }
    Ok(draft)
}

/// Превратить ответ `capture-v4` в элементы черновика тем же путём, что и
/// боевой разбор: wire → convert → validate.
fn into_draft(context: &CaptureContext, response: wire::CaptureResponse) -> Draft {
    let note_dates = response
        .items
        .iter()
        .map(|item| second_date_text(item, context))
        .collect();
    let raws: Vec<RawItem> = response
        .items
        .iter()
        .map(|item| convert_item(item, context))
        .collect();
    let items = raws
        .iter()
        .cloned()
        .map(|raw| validate_item(context, ItemSource::Telegram, raw))
        .collect();
    let clarifications = normalize_clarifications(&raws, response.clarifications, Some(context));
    Draft {
        items,
        note_dates,
        clarifications,
    }
}

/// Первая ошибка валидации, которую не объясняет уточнение: номер элемента и
/// поле.
fn first_error(
    items: &[DraftItem],
    clarifications: &[Clarification],
) -> Option<(usize, ItemError)> {
    items.iter().enumerate().find_map(|(index, item)| {
        item.errors
            .iter()
            .find(|error| !answered_by_clarification(clarifications, index, error))
            .map(|error| (index, *error))
    })
}

/// Незаполненное поле, о котором ответ задаёт вопрос, — не провал валидации:
/// контракт предписывает отвечать на событие без начала `date=none` и
/// уточнением `date`, а поле дозаполняется ответом на вопрос.
fn answered_by_clarification(
    clarifications: &[Clarification],
    item: usize,
    error: &ItemError,
) -> bool {
    error.field == "due_date"
        && error.code == DATE_MISSING
        && clarifications.iter().any(|clarification| {
            clarification.item == item && clarification.topic == ClarificationTopic::Date
        })
}

/// Поля элемента, по которым идёт сравнение с эталоном.
struct ItemView {
    kind: ListKind,
    title: String,
    list: String,
    assignee: String,
    date: Option<String>,
    time: Option<String>,
    end: Option<String>,
    repeat: Option<String>,
    quantity: Option<f64>,
    unit: Option<String>,
    /// Вторая дата фразы; свободный текст заметки не сравнивается.
    note_date: Option<String>,
}

impl ItemView {
    fn new(context: &CaptureContext, item: &DraftItem, note_date: Option<&str>) -> Self {
        let (date, time) = match item.effective_due_at {
            Some(at) => {
                let local = at.with_timezone(&context.timezone);
                (
                    Some(local.date_naive().to_string()),
                    Some(format!("{:02}:{:02}", local.hour(), local.minute())),
                )
            }
            None => (item.effective_due_date.map(|date| date.to_string()), None),
        };
        let list = item
            .list_id
            .and_then(|id| context.lists.iter().find(|list| list.id == id))
            .map(|list| list.name.clone())
            .unwrap_or_else(|| "—".to_owned());
        let new = item.item.as_ref();
        let assignee = new
            .and_then(|new| new.assignee_user_id)
            .and_then(|id| context.members.iter().find(|member| member.id == id))
            .map(|member| member.display_name.clone())
            .unwrap_or_else(|| "—".to_owned());
        Self {
            kind: item.kind,
            title: item.title.clone(),
            list,
            assignee,
            date,
            time,
            end: new.and_then(|new| end_text(context, new.end_at, new.end_date)),
            repeat: new
                .and_then(|new| new.recurrence.as_ref())
                .map(recurrence_text),
            quantity: new.and_then(|new| new.quantity),
            unit: new.and_then(|new| new.unit.clone()),
            note_date: note_date.map(ToOwned::to_owned),
        }
    }
}

fn end_text(
    context: &CaptureContext,
    end_at: Option<chrono::DateTime<chrono::Utc>>,
    end_date: Option<chrono::NaiveDate>,
) -> Option<String> {
    match (end_at, end_date) {
        (Some(at), _) => Some(
            at.with_timezone(&context.timezone)
                .format("%Y-%m-%d %H:%M")
                .to_string(),
        ),
        (None, Some(date)) => Some(date.to_string()),
        (None, None) => None,
    }
}

/// Канонический вид повторения для сравнения.
fn recurrence_text(rule: &RecurrenceInput) -> String {
    let mut parts = vec![
        freq_name(rule.freq).to_owned(),
        format!("интервал {}", rule.interval.unwrap_or(1)),
    ];
    if !rule.byweekday.is_empty() {
        let days = rule
            .byweekday
            .iter()
            .map(i16::to_string)
            .collect::<Vec<_>>()
            .join(",");
        parts.push(format!("дни {days}"));
    }
    // Для месячного повторения отсутствие правила и `anchor_day` — одно и
    // то же: так его понимает домен (`domain::recurrence`). У остальных частот
    // правило не применяется и в сравнение не входит.
    if rule.freq == RecurrenceFreq::Monthly {
        let month_rule = rule.month_rule.unwrap_or(MonthRule::AnchorDay);
        parts.push(month_rule_name(month_rule).to_owned());
    }
    if let Some(until) = rule.until {
        parts.push(format!("до {until}"));
    }
    parts.join(", ")
}

/// Сравнить ответ с эталоном. Пустой перечень — фразы совпали.
fn compare_with(
    expected: &Draft,
    actual: &Draft,
    context: &CaptureContext,
    titles: TitleMatch,
) -> Vec<Difference> {
    let expected_items: Vec<ItemView> = expected
        .items
        .iter()
        .zip(&expected.note_dates)
        .map(|(item, note_date)| ItemView::new(context, item, note_date.as_deref()))
        .collect();
    let actual_items: Vec<ItemView> = actual
        .items
        .iter()
        .zip(&actual.note_dates)
        .map(|(item, note_date)| ItemView::new(context, item, note_date.as_deref()))
        .collect();

    let mut differences = Vec::new();
    if expected_items.len() != actual_items.len() {
        differences.push(Difference {
            item: None,
            field: "число элементов",
            expected: expected_items.len().to_string(),
            actual: actual_items.len().to_string(),
        });
    }

    let pairs = pair_items(&expected_items, &actual_items, titles);
    for (expected_index, actual_index) in &pairs {
        match (expected_index, actual_index) {
            (Some(expected_index), Some(actual_index)) => compare_pair(
                &expected_items[*expected_index],
                *expected_index,
                &actual_items[*actual_index],
                &mut differences,
                titles,
            ),
            (Some(expected_index), None) => differences.push(Difference {
                item: Some(*expected_index),
                field: "элемент",
                expected: expected_items[*expected_index].title.clone(),
                actual: "—".to_owned(),
            }),
            (None, Some(actual_index)) => differences.push(Difference {
                item: Some(*actual_index),
                field: "лишний элемент",
                expected: "—".to_owned(),
                actual: actual_items[*actual_index].title.clone(),
            }),
            (None, None) => {}
        }
    }

    // Индекс элемента ответа переводится в индекс эталона: порядок элементов
    // не должен давать ложных расхождений по уточнениям.
    let mut expected_index_of = vec![None; actual_items.len()];
    for (expected_index, actual_index) in &pairs {
        if let (Some(expected_index), Some(actual_index)) = (expected_index, actual_index) {
            expected_index_of[*actual_index] = Some(*expected_index);
        }
    }
    let expected_topics = topics(&expected.clarifications);
    // Уточнение о непопарившемся элементе ответа не сравнивается: расхождение
    // по самому элементу уже записано как «лишний элемент», а индекс ответа в
    // пространстве эталона занят другим элементом и мог бы погасить
    // недостающее уточнение.
    let actual_topics: BTreeSet<(usize, ClarificationTopic)> = actual
        .clarifications
        .iter()
        .filter_map(|clarification| {
            let item = expected_index_of
                .get(clarification.item)
                .copied()
                .flatten()?;
            Some((item, clarification.topic))
        })
        .collect();
    for (item, topic) in expected_topics.difference(&actual_topics) {
        differences.push(Difference {
            item: Some(*item),
            field: "уточнение",
            expected: topic.name().to_owned(),
            actual: "—".to_owned(),
        });
    }
    for (item, topic) in actual_topics.difference(&expected_topics) {
        differences.push(Difference {
            item: Some(*item),
            field: "уточнение",
            expected: "—".to_owned(),
            actual: topic.name().to_owned(),
        });
    }

    differences
}

/// Множество пар «элемент, тема»: порядок не важен.
fn topics(clarifications: &[Clarification]) -> BTreeSet<(usize, ClarificationTopic)> {
    clarifications
        .iter()
        .map(|clarification| (clarification.item, clarification.topic))
        .collect()
}

/// Сравнить пару элементов по полям.
fn compare_pair(
    expected: &ItemView,
    expected_index: usize,
    actual: &ItemView,
    differences: &mut Vec<Difference>,
    titles: TitleMatch,
) {
    let mut differ = |field: &'static str, expected: String, actual: String| {
        differences.push(Difference {
            item: Some(expected_index),
            field,
            expected,
            actual,
        });
    };

    if !names_match(&expected.title, &actual.title, titles) {
        differ("название", expected.title.clone(), actual.title.clone());
    }
    if expected.kind != actual.kind {
        differ(
            "тип",
            kind_name(expected.kind).to_owned(),
            kind_name(actual.kind).to_owned(),
        );
    }
    if !names_match(&expected.list, &actual.list, titles) {
        differ("список", expected.list.clone(), actual.list.clone());
    }
    differ_optional("дата", &expected.date, &actual.date, &mut differ);
    differ_optional("время", &expected.time, &actual.time, &mut differ);
    differ_optional("конец", &expected.end, &actual.end, &mut differ);
    differ_optional("повторение", &expected.repeat, &actual.repeat, &mut differ);
    if expected.assignee != actual.assignee {
        differ(
            "исполнитель",
            expected.assignee.clone(),
            actual.assignee.clone(),
        );
    }
    if expected.quantity != actual.quantity {
        differ(
            "количество",
            optional_number(expected.quantity),
            optional_number(actual.quantity),
        );
    }
    if fold_opt(&expected.unit) != fold_opt(&actual.unit) {
        differ("единица", display(&expected.unit), display(&actual.unit));
    }
    if expected.note_date != actual.note_date {
        differ(
            "вторая дата",
            display(&expected.note_date),
            display(&actual.note_date),
        );
    }
}

fn differ_optional(
    field: &'static str,
    expected: &Option<String>,
    actual: &Option<String>,
    differ: &mut impl FnMut(&'static str, String, String),
) {
    if expected != actual {
        differ(field, display(expected), display(actual));
    }
}

/// Значение для отчёта: отсутствие — прочерк.
fn display(value: &Option<String>) -> String {
    value.clone().unwrap_or_else(|| "—".to_owned())
}

fn optional_number(value: Option<f64>) -> String {
    value.map_or_else(|| "—".to_owned(), number_text)
}

/// Число без лишней дробной части: `2`, а не `2.0`.
fn number_text(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

/// Значение единицы для сравнения: регистр, пробелы, `ё`/`е`.
fn fold_opt(value: &Option<String>) -> Option<String> {
    value.as_deref().map(fold)
}

fn fold(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
        .replace('ё', "е")
}

fn kind_name(kind: ListKind) -> &'static str {
    match kind {
        ListKind::Tasks => "tasks",
        ListKind::Shopping => "shopping",
        ListKind::Checklist => "checklist",
        ListKind::Events => "events",
    }
}

fn freq_name(freq: RecurrenceFreq) -> &'static str {
    match freq {
        RecurrenceFreq::Daily => "daily",
        RecurrenceFreq::Weekly => "weekly",
        RecurrenceFreq::Monthly => "monthly",
        RecurrenceFreq::Yearly => "yearly",
    }
}

fn month_rule_name(rule: MonthRule) -> &'static str {
    match rule {
        MonthRule::AnchorDay => "anchor_day",
        MonthRule::LastDay => "last_day",
    }
}

/// Названия совпали по основам слов, без учёта регистра и порядка слов.
fn names_match(expected: &str, actual: &str, titles: TitleMatch) -> bool {
    stems(expected, titles) == stems(actual, titles)
}

/// Множество основ слов названия.
fn stems(value: &str, titles: TitleMatch) -> BTreeSet<String> {
    static STEMMER: OnceLock<Stemmer> = OnceLock::new();
    let stemmer = STEMMER.get_or_init(|| Stemmer::create(Algorithm::Russian));
    value
        .split(|ch: char| !ch.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(|word| {
            let folded = fold_word(word);
            let stem = stemmer.stem(&folded).into_owned();
            match titles {
                TitleMatch::Strict => stem,
                TitleMatch::Normalized => canonical_stem(&folded, &stem).unwrap_or(stem),
            }
        })
        .filter(|stem| !stem.is_empty())
        .collect()
}

/// Слово в том виде, в каком его видит стеммер: нижний регистр, без `ё`.
fn fold_word(word: &str) -> String {
    word.to_lowercase().replace('ё', "е")
}

/// Свести основу слова к канону: синонимы и вид глагола.
///
/// Синонимы применяются к любой части речи. Вид глагола — только к
/// инфинитивам: иначе существительное с той же основой склеилось бы с
/// глаголом («наряд» и «нарядить», «пол» и «полить»), а это разные названия.
/// Пары заданы инфинитивами, а сравниваются основы, поэтому склонения
/// существительных из словаря синонимов попадают в канон автоматически.
fn canonical_stem(word: &str, stem: &str) -> Option<String> {
    if let Some(canonical) = lookup(&SYNONYM_STEMS, SYNONYMS, stem) {
        return Some(canonical);
    }
    if is_infinitive(word) {
        return lookup(&ASPECT_STEMS, ASPECT_PAIRS, stem);
    }
    None
}

/// Инфинитив по окончанию: `-ть`, `-ться`, `-ти`, `-чь`.
fn is_infinitive(word: &str) -> bool {
    word.ends_with("ться")
        || word.ends_with("ть")
        || word.ends_with("ти")
        || word.ends_with("чь")
}

fn lookup(
    map: &OnceLock<BTreeMap<String, String>>,
    pairs: &[(&str, &str)],
    stem: &str,
) -> Option<String> {
    map.get_or_init(|| stem_map(pairs)).get(stem).cloned()
}

/// Карта «основа варианта → основа канона» с тем же фолдингом, что и у слов.
fn stem_map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    let stemmer = Stemmer::create(Algorithm::Russian);
    let mut map = BTreeMap::new();
    for (variant, canonical) in pairs {
        map.insert(
            stemmer.stem(&fold_word(variant)).into_owned(),
            stemmer.stem(&fold_word(canonical)).into_owned(),
        );
    }
    map
}

/// Видовые пары глаголов: вариант → канон (несовершенный вид).
static ASPECT_PAIRS: &[(&str, &str)] = &[
    ("дать", "давать"),
    ("вынести", "выносить"),
    ("убраться", "убираться"),
    ("убрать", "убирать"),
    ("полить", "поливать"),
    ("проверить", "проверять"),
    ("отправить", "отправлять"),
    ("закрыть", "закрывать"),
    ("открыть", "открывать"),
    ("собрать", "собирать"),
    ("оплатить", "платить"),
    ("сдать", "сдавать"),
    ("обновить", "обновлять"),
    ("нарядить", "наряжать"),
    ("пересмотреть", "пересматривать"),
    ("передать", "передавать"),
    ("постирать", "стирать"),
    ("купить", "покупать"),
    ("приготовить", "готовить"),
    ("помыть", "мыть"),
    ("вымыть", "мыть"),
    ("принести", "приносить"),
    ("забрать", "забирать"),
    ("записать", "записывать"),
    ("позвонить", "звонить"),
    ("встретить", "встречать"),
    ("налить", "наливать"),
    ("починить", "чинить"),
    ("сходить", "ходить"),
    ("съездить", "ездить"),
    ("заказать", "заказывать"),
    ("достать", "доставать"),
    ("покормить", "кормить"),
    ("погулять", "гулять"),
    ("подготовить", "подготавливать"),
    ("показать", "показывать"),
    ("рассказать", "рассказывать"),
    ("спросить", "спрашивать"),
    ("ответить", "отвечать"),
    ("решить", "решать"),
    ("закончить", "заканчивать"),
    ("взять", "брать"),
    ("положить", "класть"),
    ("найти", "находить"),
    ("прочитать", "читать"),
    ("написать", "писать"),
    ("надеть", "надевать"),
    ("снять", "снимать"),
    ("почистить", "чистить"),
    ("погладить", "гладить"),
    ("сварить", "варить"),
    ("испечь", "печь"),
    ("пожарить", "жарить"),
];

/// Синонимы, которые в эталонах уже встречались как расхождение.
static SYNONYMS: &[(&str, &str)] = &[
    ("картошка", "картофель"),
    ("свёкла", "свекла"),
];

static ASPECT_STEMS: OnceLock<BTreeMap<String, String>> = OnceLock::new();
static SYNONYM_STEMS: OnceLock<BTreeMap<String, String>> = OnceLock::new();

/// Пары «эталон, ответ»: сначала по совпадению названий, затем оставшиеся по
/// порядку, чтобы расхождения в названии и полях были видны.
fn pair_items(
    expected: &[ItemView],
    actual: &[ItemView],
    titles: TitleMatch,
) -> Vec<(Option<usize>, Option<usize>)> {
    let mut owner: Vec<Option<usize>> = vec![None; actual.len()];
    for index in 0..expected.len() {
        let mut seen = vec![false; actual.len()];
        augment(index, expected, actual, &mut seen, &mut owner, titles);
    }
    let mut pairs = Vec::with_capacity(expected.len().max(actual.len()));
    let mut used = vec![false; actual.len()];
    for (actual_index, owner) in owner.iter().enumerate() {
        if let Some(expected_index) = owner {
            used[actual_index] = true;
            pairs.push((Some(*expected_index), Some(actual_index)));
        }
    }

    let mut free_expected = (0..expected.len()).filter(|index| !owner.contains(&Some(*index)));
    let mut free_actual = (0..actual.len()).filter(|index| !used[*index]);
    loop {
        match (free_expected.next(), free_actual.next()) {
            (Some(expected_index), Some(actual_index)) => {
                pairs.push((Some(expected_index), Some(actual_index)));
            }
            (Some(expected_index), None) => pairs.push((Some(expected_index), None)),
            (None, Some(actual_index)) => pairs.push((None, Some(actual_index))),
            (None, None) => break,
        }
    }
    pairs
}

/// Максимальное паросочетание по совпадению названий (алгоритм Куна).
fn augment(
    index: usize,
    expected: &[ItemView],
    actual: &[ItemView],
    seen: &mut [bool],
    owner: &mut [Option<usize>],
    titles: TitleMatch,
) -> bool {
    for (slot, item) in actual.iter().enumerate() {
        if seen[slot] || !names_match(&expected[index].title, &item.title, titles) {
            continue;
        }
        seen[slot] = true;
        match owner[slot] {
            None => {
                owner[slot] = Some(index);
                return true;
            }
            Some(current) => {
                if augment(current, expected, actual, seen, owner, titles) {
                    owner[slot] = Some(index);
                    return true;
                }
            }
        }
    }
    false
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Секунды с одним знаком для читаемого отчёта.
fn seconds(duration: Duration) -> String {
    format!("{:.1} с", duration.as_secs_f64())
}

fn failure_text(failure: Option<&Failure>) -> String {
    match failure {
        None => "пройдено".to_owned(),
        Some(Failure::Timeout { actual_ms }) => format!(
            "timeout: ответ за {}",
            seconds(Duration::from_millis(*actual_ms))
        ),
        Some(Failure::Transport { code }) => format!("transport: {code}"),
        Some(Failure::Schema { code }) => format!("schema: {code}"),
        Some(Failure::Validation { item, field, code }) => {
            format!("validation: элемент {item}, поле {field} ({code})")
        }
        Some(Failure::Mismatch { differences }) => {
            let mut text = String::from("mismatch");
            for difference in differences {
                match difference.item {
                    Some(item) => text.push_str(&format!(
                        "\n  [{item}] {}: ожидали «{}», получили «{}»",
                        difference.field, difference.expected, difference.actual
                    )),
                    None => text.push_str(&format!(
                        "\n  {}: ожидали «{}», получили «{}»",
                        difference.field, difference.expected, difference.actual
                    )),
                }
            }
            text
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::capture::prompt::PROMPT_VERSION;

    fn context_value() -> Value {
        json!({
            "now": "2026-11-10T09:00:00+03:00",
            "timezone": "Europe/Moscow",
            "lists": [
                {"name": "Задачи", "kind": "tasks", "default": true},
                {"name": "Дача", "kind": "tasks", "default": false},
                {"name": "Покупки", "kind": "shopping", "default": true},
                {"name": "Календарь", "kind": "events", "default": true}
            ],
            "members": ["Анна", "Пётр"],
            "author": "Анна"
        })
    }

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

    fn date_ymd(text: &str) -> Value {
        let parsed = chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d").expect("date is valid");
        json!({
            "kind": "date",
            "n": 0,
            "unit": "",
            "weekday": "",
            "day": i64::from(chrono::Datelike::day(&parsed)),
            "month": i64::from(chrono::Datelike::month(&parsed)),
            "year": i64::from(chrono::Datelike::year(&parsed)),
        })
    }

    fn task(title: &str, date: Value) -> Value {
        json!({
            "kind": "tasks",
            "list": "",
            "title": title,
            "date": date,
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

    fn event(title: &str, date: Value) -> Value {
        json!({
            "kind": "events",
            "list": "",
            "title": title,
            "date": date,
            "time": "",
            "end": date_none(),
            "end_time": "",
            "repeat": null,
            "assignee": "",
            "tags": [],
            "notes": "",
        })
    }

    fn shopping(title: &str) -> Value {
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

    fn repeat(days: &[i64]) -> Value {
        json!({
            "freq": "weekly",
            "interval": 1,
            "weekdays": days,
            "month_rule": "",
            "until": date_none(),
        })
    }

    fn monthly(month_rule: &str) -> Value {
        json!({
            "freq": "monthly",
            "interval": 1,
            "weekdays": [],
            "month_rule": month_rule,
            "until": date_none(),
        })
    }

    fn with(mut item: Value, key: &str, value: Value) -> Value {
        item.as_object_mut()
            .expect("item is an object")
            .insert(key.to_owned(), value);
        item
    }

    fn response(items: Vec<Value>) -> String {
        json!({"items": items, "clarifications": []}).to_string()
    }

    fn phrase(id: &str, category: Category, expect: Value) -> SmokePhrase {
        serde_json::from_value(json!({
            "id": id,
            "category": category,
            "text": "фраза",
            "expect": expect,
        }))
        .expect("phrase")
    }

    fn set() -> SmokeSet {
        SmokeSet::parse(
            &json!({
                "prompt_version": PROMPT_VERSION,
                "frozen_at": "2026-11-11",
                "context": context_value(),
                "phrases": [
                    {
                        "id": "mismatch",
                        "category": "events",
                        "text": "маникюр 12 ноября",
                        "expect": {"items": [event("Маникюр", date_ymd("2026-11-12"))], "clarifications": []}
                    }
                ]
            })
            .to_string(),
        )
        .expect("set")
    }

    fn judge_one(phrase: &SmokePhrase, answer: &str, duration: Duration) -> PhraseResult {
        let context: InputContext = serde_json::from_value(context_value()).expect("context");
        judge(
            phrase,
            &context,
            &SlotRun::text(answer, duration),
            Duration::from_secs(20),
        )
        .expect("phrase is judged")
    }

    /// Расхождения для пары «один эталонный ответ — один ответ слота».
    fn judge_items(expected: Vec<Value>, actual: Vec<Value>) -> Vec<Difference> {
        let phrase = phrase(
            "field",
            Category::Simple,
            json!({"items": expected, "clarifications": []}),
        );
        let result = judge_one(&phrase, &response(actual), Duration::from_secs(1));
        match result.failure {
            Some(Failure::Mismatch { differences }) => differences,
            other => panic!("not a mismatch: {other:?}"),
        }
    }

    fn phrase_context(now: &str) -> Value {
        let mut context = context_value();
        context["now"] = json!(now);
        context
    }

    #[test]
    fn a_kind_difference_is_a_mismatch_with_the_type_pair() {
        let phrase = phrase(
            "kind",
            Category::Events,
            json!({"items": [event("Маникюр", date_ymd("2026-11-12"))], "clarifications": []}),
        );
        let answer = response(vec![task("Маникюр", date_ymd("2026-11-12"))]);

        let result = judge_one(&phrase, &answer, Duration::from_secs(1));

        let Some(Failure::Mismatch { differences }) = &result.failure else {
            panic!("not a mismatch: {:?}", result.failure);
        };
        assert!(
            differences.iter().any(|difference| {
                difference.field == "тип"
                    && difference.expected == "events"
                    && difference.actual == "tasks"
            }),
            "{differences:?}"
        );
    }

    #[test]
    fn a_title_in_another_form_matches_by_word_stems() {
        let phrase = phrase(
            "title",
            Category::Simple,
            json!({"items": [task("Оплатить интернет", date_none())], "clarifications": []}),
        );
        let answer = response(vec![task("оплата интернета", date_none())]);

        let result = judge_one(&phrase, &answer, Duration::from_secs(1));

        assert!(result.passed(), "{:?}", result.failure);
    }

    #[test]
    fn free_note_text_is_not_compared_but_the_second_date_is() {
        let expected = with(
            with(
                task("Позвонить врачу", date_none()),
                "notes",
                json!("приём"),
            ),
            "note_date",
            date_ymd("2026-11-20"),
        );
        let phrase = phrase(
            "notes",
            Category::Simple,
            json!({"items": [expected], "clarifications": []}),
        );
        let same_date = response(vec![with(
            with(
                task("Позвонить врачу", date_none()),
                "notes",
                json!("приём у врача"),
            ),
            "note_date",
            date_ymd("2026-11-20"),
        )]);

        let result = judge_one(&phrase, &same_date, Duration::from_secs(1));
        assert!(
            result.passed(),
            "free note text must not fail the phrase: {:?}",
            result.failure
        );

        let other_date = response(vec![with(
            with(
                task("Позвонить врачу", date_none()),
                "notes",
                json!("приём у врача"),
            ),
            "note_date",
            date_ymd("2026-11-21"),
        )]);
        let result = judge_one(&phrase, &other_date, Duration::from_secs(1));
        let differences = match result.failure {
            Some(Failure::Mismatch { differences }) => differences,
            other => panic!("not a mismatch: {other:?}"),
        };
        assert!(
            differences.iter().any(|difference| {
                difference.field == "вторая дата"
                    && difference.expected == "20 ноября"
                    && difference.actual == "21 ноября"
            }),
            "{differences:?}"
        );
    }

    #[test]
    fn each_field_difference_is_reported_separately() {
        let cases = [
            (
                "список",
                with(task("Полить цветы", date_none()), "list", json!("Задачи")),
                with(task("Полить цветы", date_none()), "list", json!("Дача")),
            ),
            (
                "исполнитель",
                with(task("Полить цветы", date_none()), "assignee", json!("Анна")),
                with(task("Полить цветы", date_none()), "assignee", json!("Пётр")),
            ),
            (
                "количество",
                with(shopping("Молоко"), "quantity", json!(2)),
                with(shopping("Молоко"), "quantity", json!(3)),
            ),
            (
                "единица",
                with(
                    with(shopping("Молоко"), "quantity", json!(2)),
                    "unit",
                    json!("л"),
                ),
                with(
                    with(shopping("Молоко"), "quantity", json!(2)),
                    "unit",
                    json!("кг"),
                ),
            ),
            (
                "дата",
                with(
                    task("Полить цветы", date_none()),
                    "date",
                    date_ymd("2026-11-12"),
                ),
                with(
                    task("Полить цветы", date_none()),
                    "date",
                    date_ymd("2026-11-13"),
                ),
            ),
            (
                "время",
                with(
                    with(
                        task("Полить цветы", date_none()),
                        "date",
                        date_ymd("2026-11-12"),
                    ),
                    "time",
                    json!("14:00"),
                ),
                with(
                    with(
                        task("Полить цветы", date_none()),
                        "date",
                        date_ymd("2026-11-12"),
                    ),
                    "time",
                    json!("15:00"),
                ),
            ),
            (
                "конец",
                with(
                    with(
                        event("Маникюр", date_ymd("2026-11-12")),
                        "time",
                        json!("14:00"),
                    ),
                    "end_time",
                    json!("15:00"),
                ),
                with(
                    with(
                        event("Маникюр", date_ymd("2026-11-12")),
                        "time",
                        json!("14:00"),
                    ),
                    "end_time",
                    json!("16:00"),
                ),
            ),
            (
                "повторение",
                with(
                    task("Полить цветы", date_ymd("2026-11-16")),
                    "repeat",
                    repeat(&[1, 4]),
                ),
                with(
                    task("Полить цветы", date_ymd("2026-11-16")),
                    "repeat",
                    repeat(&[1]),
                ),
            ),
            (
                "вторая дата",
                with(
                    with(task("Полить цветы", date_none()), "notes", json!("приём")),
                    "note_date",
                    date_ymd("2026-11-20"),
                ),
                with(
                    with(task("Полить цветы", date_none()), "notes", json!("приём")),
                    "note_date",
                    date_ymd("2026-11-21"),
                ),
            ),
        ];
        for (field, expected, actual) in cases {
            let differences = judge_items(vec![expected], vec![actual]);
            let fields: BTreeSet<&str> = differences.iter().map(|d| d.field).collect();
            assert_eq!(fields, BTreeSet::from([field]), "{field}: {differences:?}");
        }
    }

    #[test]
    fn missing_and_extra_items_are_reported() {
        let milk = task("Купить молоко", date_none());
        let call = task("Позвонить маме", date_none());
        let flowers = task("Полить цветы", date_none());

        let missing = judge_items(vec![milk.clone(), call.clone()], vec![milk.clone()]);
        let missing_fields: BTreeSet<&str> = missing.iter().map(|d| d.field).collect();
        assert_eq!(
            missing_fields,
            BTreeSet::from(["число элементов", "элемент"]),
            "{missing:?}"
        );

        let extra = judge_items(vec![milk.clone()], vec![milk, flowers]);
        let extra_fields: BTreeSet<&str> = extra.iter().map(|d| d.field).collect();
        assert_eq!(
            extra_fields,
            BTreeSet::from(["число элементов", "лишний элемент"]),
            "{extra:?}"
        );
    }

    #[test]
    fn clarifications_follow_paired_items_not_positions() {
        let expected = json!({
            "items": [
                task("Купить молоко", date_none()),
                task("Позвонить маме", date_none()),
            ],
            "clarifications": [{"item": 1, "topic": "date"}],
        });
        let phrase = phrase("order", Category::Ambiguity, expected);
        let answer = json!({
            "items": [
                task("Позвонить маме", date_none()),
                task("Купить молоко", date_none()),
            ],
            "clarifications": [{"item": 0, "topic": "date"}],
        })
        .to_string();

        let result = judge_one(&phrase, &answer, Duration::from_secs(1));

        assert!(result.passed(), "{:?}", result.failure);
    }

    #[test]
    fn an_event_without_a_date_and_a_date_question_is_not_a_validation_failure() {
        let expected = json!({
            "items": [event("Встреча с Петей", date_none())],
            "clarifications": [{"item": 0, "topic": "date"}],
        });
        let phrase = phrase("event-question", Category::Ambiguity, expected);
        let same = phrase.expect.to_string();

        let result = judge_one(&phrase, &same, Duration::from_secs(1));
        assert!(result.passed(), "{:?}", result.failure);

        let without_question = response(vec![event("Встреча с Петей", date_none())]);
        let result = judge_one(&phrase, &without_question, Duration::from_secs(1));
        let Some(Failure::Validation { item, field, code }) = result.failure else {
            panic!("not a validation failure: {:?}", result.failure);
        };
        assert_eq!(item, 0);
        assert_eq!(field, "due_date");
        assert_eq!(code, DATE_MISSING);
    }

    #[test]
    fn a_clarification_about_an_extra_item_does_not_hide_a_missing_one() {
        let expected = json!({
            "items": [
                task("Купить молоко", date_none()),
                task("Позвонить маме", date_none()),
            ],
            "clarifications": [{"item": 0, "topic": "date"}],
        });
        let phrase = phrase("extra", Category::Ambiguity, expected);
        let answer = json!({
            "items": [
                task("Полить цветы", date_none()),
                task("Купить молоко", date_none()),
            ],
            "clarifications": [{"item": 0, "topic": "date"}],
        })
        .to_string();

        let result = judge_one(&phrase, &answer, Duration::from_secs(1));

        let differences = match result.failure {
            Some(Failure::Mismatch { differences }) => differences,
            other => panic!("not a mismatch: {other:?}"),
        };
        assert!(
            differences.iter().any(|difference| {
                difference.field == "уточнение"
                    && difference.expected == "date"
                    && difference.actual == "—"
            }),
            "{differences:?}"
        );
    }

    #[test]
    fn a_monthly_repeat_without_a_rule_equals_anchor_day() {
        let anchor = phrase(
            "monthly",
            Category::Recurrence,
            json!({
                "items": [with(
                    task("Оплатить квартиру", date_ymd("2026-11-15")),
                    "repeat",
                    monthly("anchor_day"),
                )],
                "clarifications": [],
            }),
        );
        let no_rule = response(vec![with(
            task("Оплатить квартиру", date_ymd("2026-11-15")),
            "repeat",
            monthly(""),
        )]);

        let result = judge_one(&anchor, &no_rule, Duration::from_secs(1));
        assert!(result.passed(), "{:?}", result.failure);

        let anchor_thirtieth = phrase(
            "monthly-last",
            Category::Recurrence,
            json!({
                "items": [with(
                    task("Оплатить квартиру", date_ymd("2026-11-30")),
                    "repeat",
                    monthly("anchor_day"),
                )],
                "clarifications": [],
            }),
        );
        let last_day = response(vec![with(
            task("Оплатить квартиру", date_ymd("2026-11-30")),
            "repeat",
            monthly("last_day"),
        )]);

        let result = judge_one(&anchor_thirtieth, &last_day, Duration::from_secs(1));
        let differences = match result.failure {
            Some(Failure::Mismatch { differences }) => differences,
            other => panic!("not a mismatch: {other:?}"),
        };
        assert!(
            differences
                .iter()
                .any(|difference| difference.field == "повторение"),
            "{differences:?}"
        );
    }

    #[test]
    fn a_correct_answer_after_the_slot_timeout_fails_with_its_actual_time() {
        let phrase = phrase(
            "slow",
            Category::Simple,
            json!({"items": [task("Оплатить интернет", date_none())], "clarifications": []}),
        );
        let answer = response(vec![task("Оплатить интернет", date_none())]);
        let context: InputContext = serde_json::from_value(context_value()).expect("context");
        let run = SlotRun::text(answer, Duration::from_millis(5_100));

        let result = judge(&phrase, &context, &run, Duration::from_secs(4)).expect("judged");

        assert_eq!(result.failure, Some(Failure::Timeout { actual_ms: 5_100 }));
        assert!(result.to_json()["reason"]["actual_ms"] == json!(5_100));
    }

    #[test]
    fn a_client_error_is_a_transport_failure_with_its_code() {
        let phrase = phrase("transport", Category::Simple, json!({"items": []}));
        let context: InputContext = serde_json::from_value(context_value()).expect("context");
        let run = SlotRun::error("rate_limited", Duration::from_millis(200));

        let result = judge(&phrase, &context, &run, Duration::from_secs(4)).expect("judged");

        assert_eq!(
            result.failure,
            Some(Failure::Transport {
                code: "rate_limited".to_owned()
            })
        );
    }

    #[test]
    fn a_slow_client_error_keeps_its_transport_code() {
        let phrase = phrase("slow-error", Category::Simple, json!({"items": []}));
        let context: InputContext = serde_json::from_value(context_value()).expect("context");
        let run = SlotRun::error("rate_limited", Duration::from_millis(5_100));

        let result = judge(&phrase, &context, &run, Duration::from_secs(4)).expect("judged");

        assert_eq!(
            result.failure,
            Some(Failure::Transport {
                code: "rate_limited".to_owned()
            }),
            "a client error after the slot timeout still explains the failure"
        );
    }

    #[test]
    fn a_client_timeout_after_the_slot_timeout_is_a_timeout_with_the_actual_time() {
        let phrase = phrase("slow-timeout", Category::Simple, json!({"items": []}));
        let context: InputContext = serde_json::from_value(context_value()).expect("context");
        let run = SlotRun::error("timeout", Duration::from_secs(20));

        let result = judge(&phrase, &context, &run, Duration::from_secs(4)).expect("judged");

        assert_eq!(result.failure, Some(Failure::Timeout { actual_ms: 20_000 }));
    }

    #[test]
    fn a_client_timeout_before_the_slot_timeout_is_still_a_timeout() {
        let phrase = phrase("client-timeout", Category::Simple, json!({"items": []}));
        let context: InputContext = serde_json::from_value(context_value()).expect("context");
        // Клиент оборвал ожидание раньше таймаута слота: верного ответа нет,
        // значит причина — таймаут, а не транспорт с кодом.
        let run = SlotRun::error("timeout", Duration::from_millis(300));

        let result = judge(&phrase, &context, &run, Duration::from_secs(4)).expect("judged");

        assert_eq!(result.failure, Some(Failure::Timeout { actual_ms: 300 }));
    }

    #[test]
    fn an_unparsed_answer_is_a_schema_failure_with_the_parse_code() {
        let phrase = phrase("schema", Category::Simple, json!({"items": []}));

        let result = judge_one(&phrase, "не json", Duration::from_secs(1));

        assert_eq!(
            result.failure,
            Some(Failure::Schema {
                code: "not_an_object".to_owned()
            })
        );
    }

    #[test]
    fn an_element_rejected_by_validation_names_its_field() {
        let phrase = phrase(
            "validation",
            Category::Simple,
            json!({"items": [task("Позвонить маме", date_none())], "clarifications": []}),
        );
        let answer = response(vec![task("", date_none())]);

        let result = judge_one(&phrase, &answer, Duration::from_secs(1));

        let Some(Failure::Validation { item, field, code }) = &result.failure else {
            panic!("not a validation failure: {:?}", result.failure);
        };
        assert_eq!(*item, 0);
        assert_eq!(field, "title");
        assert_eq!(code, "title_length");
    }

    #[test]
    fn latency_grades_use_the_inclusive_boundaries() {
        let cases = [
            (Duration::ZERO, LatencyGrade::Wonderful),
            (Duration::from_millis(2_000), LatencyGrade::Wonderful),
            (Duration::from_micros(2_000_001), LatencyGrade::Good),
            (Duration::from_millis(4_000), LatencyGrade::Good),
            (Duration::from_micros(4_000_001), LatencyGrade::Acceptable),
            (Duration::from_millis(6_000), LatencyGrade::Acceptable),
            (Duration::from_micros(6_000_001), LatencyGrade::Slow),
            (Duration::from_millis(10_000), LatencyGrade::Slow),
            (
                Duration::from_micros(10_000_001),
                LatencyGrade::Unacceptable,
            ),
        ];
        for (duration, grade) in cases {
            assert_eq!(LatencyGrade::of(duration), grade, "{duration:?}");
        }
        assert_eq!(LatencyGrade::Wonderful.label(), "замечательно");
        assert_eq!(LatencyGrade::Good.label(), "хорошо");
        assert_eq!(LatencyGrade::Acceptable.label(), "приемлемо");
        assert_eq!(LatencyGrade::Slow.label(), "медленно");
        assert_eq!(LatencyGrade::Unacceptable.label(), "неприемлемо");
    }

    #[test]
    fn p95_uses_the_nearest_rank() {
        let result = |ms: u64| PhraseResult {
            id: ms.to_string(),
            category: Category::Simple,
            duration: Duration::from_millis(ms),
            failure: None,
        };
        let report = Report {
            prompt_version: PROMPT_VERSION.to_owned(),
            frozen_at: "2026-11-11".to_owned(),
            titles: TitleMatch::Strict,
            results: vec![result(1_000), result(2_000), result(3_000), result(4_000)],
        };
        assert_eq!(report.p95(), Duration::from_secs(4));

        let report = Report {
            prompt_version: PROMPT_VERSION.to_owned(),
            frozen_at: "2026-11-11".to_owned(),
            titles: TitleMatch::Strict,
            results: (1..=20).map(|seconds| result(seconds * 1_000)).collect(),
        };
        assert_eq!(report.p95(), Duration::from_secs(19));
    }

    #[test]
    fn the_report_counts_passed_phrases_and_fails_if_one_is_broken() {
        let good = phrase(
            "good",
            Category::Simple,
            json!({"items": [task("Позвонить маме", date_none())], "clarifications": []}),
        );
        let bad = phrase(
            "bad",
            Category::Simple,
            json!({"items": [task("Купить молоко", date_none())], "clarifications": []}),
        );
        let runs = BTreeMap::from([
            (
                "good".to_owned(),
                SlotRun::text(
                    response(vec![task("позвонить маме", date_none())]),
                    Duration::from_secs(1),
                ),
            ),
            (
                "bad".to_owned(),
                SlotRun::text(
                    response(vec![task("Купить хлеб", date_none())]),
                    Duration::from_secs(3),
                ),
            ),
        ]);
        let mut set = set();
        set.phrases = vec![good, bad];

        let report = evaluate(&set, &runs, Duration::from_secs(20)).expect("evaluated");

        assert_eq!(report.total(), 2);
        assert_eq!(report.passed_count(), 1);
        assert!(!report.passed());
        assert_eq!(report.grade(), LatencyGrade::Good);
        assert_eq!(report.failed().count(), 1);
        assert_eq!(report.p95(), Duration::from_secs(3));
        let text = report.to_text();
        assert!(text.contains("пройдено 1 из 2"), "{text}");
        assert!(text.contains("ожидали «Купить молоко»"), "{text}");
        assert!(
            text.contains("good (простые задачи): 1.0 с — пройдено"),
            "{text}"
        );
        assert!(
            text.contains("bad (простые задачи): 3.0 с — mismatch"),
            "{text}"
        );
        assert!(
            text.contains("Слот не прошёл проверку: пройдено 1 из 2"),
            "{text}"
        );
    }

    #[test]
    fn a_passed_slot_is_reported_as_passed() {
        let phrase = phrase(
            "good",
            Category::Simple,
            json!({"items": [task("Позвонить маме", date_none())], "clarifications": []}),
        );
        let runs = BTreeMap::from([(
            "good".to_owned(),
            SlotRun::text(
                response(vec![task("Позвонить маме", date_none())]),
                Duration::from_secs(1),
            ),
        )]);
        let mut set = set();
        set.phrases = vec![phrase];

        let report = evaluate(&set, &runs, Duration::from_secs(20)).expect("evaluated");

        assert!(report.passed());
        assert!(report.to_text().contains("Слот прошёл проверку"));
    }

    #[test]
    fn the_json_report_contains_fields_but_not_the_answer_text() {
        let phrase = phrase(
            "raw",
            Category::Events,
            json!({"items": [event("Маникюр", date_ymd("2026-11-12"))], "clarifications": []}),
        );
        let answer = response(vec![task("Маникюр", date_ymd("2026-11-12"))]);
        let context: InputContext = serde_json::from_value(context_value()).expect("context");
        let report = Report {
            prompt_version: PROMPT_VERSION.to_owned(),
            frozen_at: "2026-11-11".to_owned(),
            titles: TitleMatch::Strict,
            results: vec![
                judge(
                    &phrase,
                    &context,
                    &SlotRun::text(answer.clone(), Duration::from_secs(1)),
                    Duration::from_secs(20),
                )
                .expect("judged"),
            ],
        };

        let text = report.to_json().to_string();

        assert!(!text.contains(&answer), "the raw answer leaked: {text}");
        assert!(text.contains("\"kind\":\"mismatch\""), "{text}");
        assert!(text.contains("\"field\":\"тип\""), "{text}");
    }

    #[test]
    fn a_phrase_without_a_run_is_a_caller_error() {
        let set = set();
        let error = evaluate(&set, &BTreeMap::new(), Duration::from_secs(20))
            .expect_err("a missing run is an error");
        assert!(error.contains("нет исхода вызова слота"), "{error}");
    }

    #[test]
    fn every_phrase_uses_the_sets_now() {
        let phrase = |now: &str| {
            json!({
                "id": "now",
                "category": "simple",
                "text": "фраза",
                "context": phrase_context(now),
                "expect": {"items": [], "clarifications": []},
            })
        };
        let set = |now: &str| {
            json!({
                "prompt_version": PROMPT_VERSION,
                "frozen_at": "2026-11-11",
                "context": context_value(),
                "phrases": [phrase(now)],
            })
            .to_string()
        };

        SmokeSet::parse(&set("2026-11-10T09:00:00+03:00"))
            .expect("the same now in a phrase context is allowed");
        SmokeSet::parse(&set("2026-11-10T06:00:00Z"))
            .expect("the same moment with another offset is allowed");
        let error = SmokeSet::parse(&set("2026-11-11T09:00:00+03:00"))
            .expect_err("another now in a phrase context is an error");
        assert!(error.contains("«сейчас»"), "{error}");
    }

    const FROZEN_SMOKE_SET: &str = include_str!("prompts/smoke-v4.json");

    fn frozen_set() -> SmokeSet {
        SmokeSet::parse(FROZEN_SMOKE_SET).expect("набор разбирается")
    }

    #[test]
    fn the_frozen_smoke_set_matches_the_contract() {
        let set = frozen_set();
        check_prompt_version(&set).expect("версия контракта набора");
        assert_eq!(set.frozen_at, "2026-11-11");
        assert_eq!(set.phrases.len(), 22);

        let mut referenced_clarifications = 0;
        let mut expected_kinds = BTreeSet::new();
        let mut broken = Vec::new();
        for phrase in &set.phrases {
            let context = match phrase.context(&set.context) {
                Ok(context) => context,
                Err(error) => {
                    broken.push(format!("фраза {}: {error}", phrase.id));
                    continue;
                }
            };
            let draft = match reference(&context, &phrase.expect) {
                Ok(draft) => draft,
                Err(error) => {
                    broken.push(format!("фраза {}: {error}", phrase.id));
                    continue;
                }
            };
            referenced_clarifications += draft.clarifications.len();
            for item in phrase.expect["items"]
                .as_array()
                .expect("у эталона есть перечень элементов")
            {
                expected_kinds.insert(item["kind"].as_str().expect("вид элемента").to_owned());
            }
        }
        assert!(
            broken.is_empty(),
            "нужно решение: эталоны не проходят разбор формы и валидацию: {broken:#?}"
        );

        let categories: BTreeSet<Category> =
            set.phrases.iter().map(|phrase| phrase.category).collect();
        for category in Category::ALL {
            assert!(
                categories.contains(&category),
                "нет фразы категории {}",
                category.label()
            );
        }

        assert!(
            expected_kinds.contains("events"),
            "нет эталона события: {expected_kinds:?}"
        );
        let direct_shopping = set.phrases.iter().any(|phrase| {
            phrase.category == Category::Shopping
                && !phrase.text.to_lowercase().contains("купить")
                && phrase.expect["items"].as_array().is_some_and(|items| {
                    !items.is_empty() && items.iter().all(|item| item["kind"] == json!("shopping"))
                })
        });
        assert!(
            direct_shopping,
            "нет прямого добавления покупки без глагола «купить»"
        );
        assert!(referenced_clarifications > 0, "нет эталона с уточнением");
        assert!(
            set.phrases.iter().any(|phrase| phrase.expect["items"]
                .as_array()
                .is_some_and(|items| items.is_empty())),
            "нет мусорной фразы с пустым ответом"
        );
    }

    #[test]
    fn a_smoke_set_for_another_contract_fails_the_version_check() {
        let mut value: Value = serde_json::from_str(FROZEN_SMOKE_SET).expect("набор — JSON");
        value["prompt_version"] = json!("capture-v3");
        let set = SmokeSet::parse(&value.to_string()).expect("набор разбирается");

        let error =
            check_prompt_version(&set).expect_err("версия другого контракта — ошибка проверки");

        assert!(error.contains("capture-v3"), "{error}");
        assert!(error.contains(PROMPT_VERSION), "{error}");
    }

    #[test]
    fn the_set_is_read_from_json_with_a_fixed_now() {
        let set = set();
        let context = set.context().expect("context");
        assert_eq!(set.prompt_version, PROMPT_VERSION);
        assert_eq!(set.frozen_at, "2026-11-11");
        assert_eq!(
            context.now,
            chrono::DateTime::parse_from_rfc3339("2026-11-10T06:00:00Z")
                .expect("now")
                .with_timezone(&chrono::Utc)
        );
        assert_eq!(context.today().to_string(), "2026-11-10");
    }

    #[test]
    fn strict_keeps_verb_aspect_apart() {
        assert!(!names_match(
            "Вынести мусор",
            "Выносить мусор",
            TitleMatch::Strict
        ));
        assert!(!names_match("Картошка", "Картофель", TitleMatch::Strict));
    }

    #[test]
    fn normalized_merges_verb_aspect() {
        for (expected, actual) in [
            ("Вынести мусор", "Выносить мусор"),
            ("Полить фикус", "Поливать фикус"),
            ("Убраться в детской", "Убираться в детской"),
            ("Проверить уроки", "Проверять уроки"),
            ("Дать коту таблетку", "Давать коту таблетку"),
        ] {
            assert!(
                names_match(expected, actual, TitleMatch::Normalized),
                "{expected} ~ {actual}"
            );
        }
    }

    #[test]
    fn normalized_keeps_extra_words() {
        // Нормализация сводит вид глагола, но не выбрасывает слова: «за» в
        // названии — это разница в составе, а не в форме.
        assert!(!names_match(
            "Оплатить кружки",
            "Платить за кружки",
            TitleMatch::Normalized
        ));
    }

    #[test]
    fn normalized_merges_synonyms() {
        assert!(names_match("Картошка", "Картофель", TitleMatch::Normalized));
        assert!(names_match("купить картошку", "Купить картофель", TitleMatch::Normalized));
    }

    fn judge_mode(phrase: &SmokePhrase, answer: &str, titles: TitleMatch) -> PhraseResult {
        let context: InputContext = serde_json::from_value(context_value()).expect("context");
        judge_with(
            phrase,
            &context,
            &SlotRun::text(answer, Duration::from_secs(1)),
            Duration::from_secs(20),
            titles,
        )
        .expect("phrase is judged")
    }

    /// Режим должен доходить до сравнения через `judge_with`, а не только
    /// работать в `names_match` напрямую.
    /// Один и тот же прогон оценивается в двух режимах, и вердикт отчёта
    /// совпадает с выбранным режимом (это то, что печатает `capture-eval`).
    #[test]
    fn evaluate_with_reports_the_mode_and_its_verdict() {
        let set = SmokeSet::parse(
            &json!({
                "prompt_version": PROMPT_VERSION,
                "frozen_at": "2026-11-11",
                "context": context_value(),
                "phrases": [{
                    "id": "mode",
                    "category": Category::Simple,
                    "text": "вынести мусор",
                    "expect": {"items": [task("Вынести мусор", date_none())], "clarifications": []},
                }],
            })
            .to_string(),
        )
        .expect("set");

        let mut runs = BTreeMap::new();
        runs.insert(
            "mode".to_owned(),
            SlotRun::text(
                response(vec![task("Выносить мусор", date_none())]),
                Duration::from_millis(1),
            ),
        );

        let strict =
            evaluate_with(&set, &runs, Duration::from_secs(20), TitleMatch::Strict).expect("strict");
        let normalized = evaluate_with(&set, &runs, Duration::from_secs(20), TitleMatch::Normalized)
            .expect("normalized");

        assert!(!strict.passed(), "strict keeps the aspect apart");
        assert!(normalized.passed(), "normalized accepts the aspect pair");
        assert_eq!(strict.titles, TitleMatch::Strict);
        assert_eq!(normalized.titles, TitleMatch::Normalized);
        assert_eq!(normalized.to_json()["titles"], "normalized");
        assert_eq!(strict.to_json()["titles"], "strict");
    }

    #[test]
    fn normalized_mode_is_reachable_through_judge() {
        let phrase = phrase(
            "mode",
            Category::Simple,
            json!({"items": [task("Вынести мусор", date_none())], "clarifications": []}),
        );
        let answer = response(vec![task("Выносить мусор", date_none())]);

        assert!(
            judge_mode(&phrase, &answer, TitleMatch::Strict)
                .failure
                .is_some(),
            "strict keeps the aspect apart"
        );
        assert!(
            judge_mode(&phrase, &answer, TitleMatch::Normalized)
                .failure
                .is_none(),
            "normalized accepts the aspect pair"
        );
    }

    /// Сопоставление элементов тоже идёт по нормализованным названиям:
    /// переставленные элементы не должны давать «не найден».
    #[test]
    fn normalized_pairing_matches_items_in_any_order() {
        let phrase = phrase(
            "pair",
            Category::Simple,
            json!({
                "items": [
                    task("Вынести мусор", date_none()),
                    task("Полить фикус", date_none()),
                ],
                "clarifications": [],
            }),
        );
        let answer = response(vec![
            task("Поливать фикус", date_none()),
            task("Выносить мусор", date_none()),
        ]);

        assert!(judge_mode(&phrase, &answer, TitleMatch::Strict)
            .failure
            .is_some());
        assert!(
            judge_mode(&phrase, &answer, TitleMatch::Normalized)
                .failure
                .is_none(),
            "aspect pairs pair up regardless of order"
        );
    }

    /// Каждая пара словаря должна работать: иначе запись в словаре мертва.
    #[test]
    fn every_dictionary_pair_is_equivalent_in_normalized() {
        for (variant, canonical) in ASPECT_PAIRS.iter().copied() {
            assert!(
                names_match(variant, canonical, TitleMatch::Normalized),
                "{variant} ~ {canonical}"
            );
        }
        for (variant, canonical) in SYNONYMS.iter().copied() {
            assert!(
                names_match(variant, canonical, TitleMatch::Normalized),
                "{variant} ~ {canonical}"
            );
        }
    }

    /// Пары, на которых падали эталоны v0.9, обязаны быть в словаре.
    /// Табличный тест выше перебирает сам словарь и удаление записи не
    /// заметит; этот — заметит.
    #[test]
    fn aspect_dictionary_covers_the_observed_pairs() {
        let required = [
            ("дать", "давать"),
            ("вынести", "выносить"),
            ("убраться", "убираться"),
            ("полить", "поливать"),
            ("проверить", "проверять"),
            ("отправить", "отправлять"),
            ("закрыть", "закрывать"),
            ("собрать", "собирать"),
            ("оплатить", "платить"),
            ("сдать", "сдавать"),
            ("обновить", "обновлять"),
            ("нарядить", "наряжать"),
            ("пересмотреть", "пересматривать"),
            ("передать", "передавать"),
        ];
        for (variant, canonical) in required {
            assert!(
                ASPECT_PAIRS.contains(&(variant, canonical)),
                "{variant}/{canonical} пропала из словаря"
            );
            assert!(
                names_match(variant, canonical, TitleMatch::Normalized),
                "{variant} ~ {canonical}"
            );
        }
    }

    /// Вид глагола сводится только у инфинитивов: существительное с той же
    /// основой — другое название.
    #[test]
    fn normalized_does_not_merge_a_noun_with_its_verb() {
        for (noun, verb) in [("наряд", "нарядить"), ("пол", "полить")] {
            assert!(
                !names_match(noun, verb, TitleMatch::Normalized),
                "{noun} ≠ {verb}"
            );
        }
    }

    #[test]
    fn normalized_keeps_different_titles() {
        for (expected, actual) in [
            ("Подарок", "Подарок Игорю"),
            ("Бензин", "Бензин для генератора"),
            ("Визит сантехника", "Сантехник"),
            ("Погулять в парке", "Сходить в кино"),
        ] {
            assert!(
                !names_match(expected, actual, TitleMatch::Normalized),
                "{expected} ≠ {actual}"
            );
        }
    }
}
