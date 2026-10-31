//! Golden set и чистая оценка извлечения элементов.
//!
//! Набор лежит в `tests/golden/capture.json`: русские фразы с явно
//! зафиксированным контекстом household (момент, часовой пояс, списки и
//! участники) и ожидаемым результатом. Оценка не ходит в сеть и не трогает БД:
//! она принимает объект `data`, который вернул бы Home Assistant, и прогоняет
//! его через настоящие [`parse_response`] и [`validate_items`], поэтому метрика
//! совпадает с продовым путём разбора.
//!
//! Правило сравнения: элемент совпал, если после нормализации (регистр,
//! пробелы, `ё`/`е`) равны заголовок, тип и список, исполнитель, срок и
//! повторение. Точность набора — `совпавшие / (ожидаемые + лишние)`: лишний
//! элемент, которого не было в ожидании, уменьшает точность так же, как
//! пропущенный. Для фраз, где ожидается уточнение, проверяются факт вопроса и
//! его тема (исполнитель или дата), а не формулировка.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use chrono::{DateTime, Timelike, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::context::{CaptureContext, ListRef, MemberRef};
use super::prompt;
use super::schema::parse_response;
use super::validate::{
    ASSIGNEE_AMBIGUOUS, DATE_INVALID, DATE_PAST, DraftItem, ValidatedDraft, validate_items,
};
use crate::domain::models::{ItemSource, ListKind, MonthRule, RecurrenceFreq, RecurrenceInput};
use crate::domain::recurrence::parse_timezone;

/// Минимальная точность, при которой версия промпта считается пригодной.
pub const MIN_ACCURACY: f64 = 0.90;

/// Списки и участники фиксированного контекста.
#[derive(Debug, Clone, Deserialize)]
pub struct ContextSpec {
    /// Текущий момент в RFC3339; набор не зависит от «сегодня».
    pub now: String,
    /// Часовой пояс IANA.
    pub timezone: String,
    /// Автор заметки; должен быть среди участников.
    #[serde(default)]
    pub author: Option<String>,
    /// Списки household.
    pub lists: Vec<ListSpec>,
    /// Участники household.
    pub members: Vec<MemberSpec>,
}

/// Список в фиксированном контексте.
#[derive(Debug, Clone, Deserialize)]
pub struct ListSpec {
    /// Название.
    pub name: String,
    /// Тип.
    pub kind: ListKind,
    /// Список по умолчанию для своего типа.
    #[serde(default)]
    pub default: bool,
}

/// Участник в фиксированном контексте.
#[derive(Debug, Clone, Deserialize)]
pub struct MemberSpec {
    /// Отображаемое имя.
    pub name: String,
    /// Telegram username без `@`.
    #[serde(default)]
    pub username: Option<String>,
}

impl ContextSpec {
    /// Собрать [`CaptureContext`] с детерминированными идентификаторами.
    pub fn to_context(&self) -> Result<CaptureContext, String> {
        let now = DateTime::parse_from_rfc3339(self.now.trim())
            .map_err(|_| format!("«{}» — не RFC3339", self.now))?
            .with_timezone(&Utc);
        let timezone = parse_timezone(&self.timezone)
            .map_err(|_| format!("неизвестный часовой пояс «{}»", self.timezone))?;
        let lists = self
            .lists
            .iter()
            .enumerate()
            .map(|(index, list)| ListRef {
                id: Uuid::from_u128(index as u128 + 1),
                name: list.name.clone(),
                kind: list.kind,
                is_default: list.default,
            })
            .collect();
        let members: Vec<MemberRef> = self
            .members
            .iter()
            .enumerate()
            .map(|(index, member)| MemberRef {
                id: Uuid::from_u128(1_000 + index as u128),
                display_name: member.name.clone(),
                username: member.username.clone(),
            })
            .collect();
        let author = match &self.author {
            None => None,
            Some(name) => Some(
                members
                    .iter()
                    .find(|member| &member.display_name == name)
                    .cloned()
                    .ok_or_else(|| format!("автор «{name}» не входит в участников"))?,
            ),
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

/// Golden set целиком.
#[derive(Debug, Clone, Deserialize)]
pub struct GoldenSet {
    /// Версия формата набора.
    pub version: u32,
    /// Контекст по умолчанию; фраза может его переопределить.
    pub context: ContextSpec,
    /// Фразы.
    pub phrases: Vec<GoldenPhrase>,
}

impl GoldenSet {
    /// Разобрать JSON набора.
    pub fn parse(text: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(text)
    }
}

/// Одна фраза набора.
#[derive(Debug, Clone, Deserialize)]
pub struct GoldenPhrase {
    /// Стабильный идентификатор для отчёта.
    pub id: String,
    /// Категория покрытия.
    pub category: Category,
    /// Русский текст.
    pub text: String,
    /// Контекст, если он отличается от контекста набора.
    #[serde(default)]
    pub context: Option<ContextSpec>,
    /// Ожидаемый результат.
    pub expect: Expectation,
}

impl GoldenPhrase {
    /// Контекст фразы: собственный или контекст набора.
    pub fn effective_context<'a>(&'a self, default: &'a ContextSpec) -> &'a ContextSpec {
        self.context.as_ref().unwrap_or(default)
    }
}

/// Что должна вернуть модель на фразу.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Expectation {
    /// Ожидаемые элементы; пусто — элементов быть не должно.
    #[serde(default)]
    pub items: Vec<ExpectedItem>,
    /// Ожидаемые темы уточнений.
    #[serde(default)]
    pub clarifications: Vec<ClarificationTopic>,
}

/// Ожидаемый элемент.
#[derive(Debug, Clone, Deserialize)]
pub struct ExpectedItem {
    /// Заголовок.
    pub title: String,
    /// Равноценные формулировки заголовка: «Оплатить квартиру» вместо
    /// «Оплачивать квартиру» — не ошибка разбора.
    #[serde(default)]
    pub alt_titles: Vec<String>,
    /// Тип списка.
    pub kind: ListKind,
    /// Название списка.
    pub list: String,
    /// Исполнитель; отсутствие означает, что его быть не должно.
    #[serde(default)]
    pub assignee: Option<String>,
    /// Срок `YYYY-MM-DD`; отсутствие означает, что срока быть не должно.
    #[serde(default)]
    pub due_date: Option<String>,
    /// Время `HH:MM`; отсутствие означает, что времени быть не должно.
    #[serde(default)]
    pub due_time: Option<String>,
    /// Повторение; отсутствие означает, что его быть не должно.
    #[serde(default)]
    pub recurrence: Option<ExpectedRecurrence>,
}

/// Ожидаемое повторение.
#[derive(Debug, Clone, Deserialize)]
pub struct ExpectedRecurrence {
    /// Частота.
    pub freq: RecurrenceFreq,
    /// Интервал; по умолчанию 1.
    #[serde(default)]
    pub interval: Option<u16>,
    /// Дни недели ISO.
    #[serde(default)]
    pub weekdays: Vec<u16>,
    /// Правило месяца.
    #[serde(default)]
    pub month_rule: Option<MonthRule>,
    /// Дата окончания `YYYY-MM-DD`.
    #[serde(default)]
    pub until: Option<String>,
}

/// Тема уточняющего вопроса.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClarificationTopic {
    /// Какой из участников.
    Assignee,
    /// Когда или во сколько.
    DueDate,
}

impl ClarificationTopic {
    /// Подпись для отчёта.
    pub fn label(self) -> &'static str {
        match self {
            Self::Assignee => "исполнитель",
            Self::DueDate => "дата",
        }
    }
}

/// Категория покрытия golden set.
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
}

impl Category {
    /// Все категории, которые обязан покрывать набор.
    pub const ALL: [Self; 9] = [
        Self::Simple,
        Self::Shopping,
        Self::DatesRelative,
        Self::DatesAbsolute,
        Self::TimeOfDay,
        Self::Recurrence,
        Self::Assignees,
        Self::Ambiguity,
        Self::Garbage,
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
        }
    }
}

/// Счётчики элементов.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Totals {
    /// Совпавшие элементы.
    pub matched: usize,
    /// Ожидаемые элементы.
    pub expected: usize,
    /// Лишние элементы.
    pub extra: usize,
}

impl Totals {
    /// Точность по правилу `совпавшие / (ожидаемые + лишние)`.
    ///
    /// Пустое ожидание без лишних элементов даёт `1.0`: мусорная фраза
    /// обработана правильно, если модель ничего не выдумала.
    pub fn accuracy(&self) -> f64 {
        let denominator = self.expected + self.extra;
        if denominator == 0 {
            1.0
        } else {
            self.matched as f64 / denominator as f64
        }
    }

    fn merge(&mut self, other: Self) {
        self.matched += other.matched;
        self.expected += other.expected;
        self.extra += other.extra;
    }
}

/// Результат одной фразы.
#[derive(Debug, Clone)]
pub struct PhraseResult {
    /// Идентификатор фразы.
    pub id: String,
    /// Категория.
    pub category: Category,
    /// Счётчики элементов.
    pub totals: Totals,
    /// Ожидаемые уточнения получены.
    pub clarifications_ok: bool,
    /// Человекочитаемые причины провала.
    pub problems: Vec<String>,
    /// Ответ не разобран или контекст некорректен.
    pub error: Option<String>,
}

impl PhraseResult {
    /// Фраза обработана правильно: все элементы совпали, лишних нет, нужные
    /// уточнения получены.
    pub fn passed(&self) -> bool {
        self.error.is_none()
            && self.totals.extra == 0
            && self.totals.matched == self.totals.expected
            && self.clarifications_ok
    }

    /// Точность фразы.
    pub fn accuracy(&self) -> f64 {
        self.totals.accuracy()
    }
}

/// Результат всего набора.
#[derive(Debug, Clone)]
pub struct Report {
    /// Результаты в порядке набора.
    pub results: Vec<PhraseResult>,
}

impl Report {
    /// Суммарные счётчики.
    pub fn overall(&self) -> Totals {
        self.results
            .iter()
            .fold(Totals::default(), |mut totals, result| {
                totals.merge(result.totals);
                totals
            })
    }

    /// Счётчики по категориям.
    pub fn by_category(&self) -> BTreeMap<Category, Totals> {
        let mut totals: BTreeMap<Category, Totals> = BTreeMap::new();
        for result in &self.results {
            totals
                .entry(result.category)
                .or_default()
                .merge(result.totals);
        }
        totals
    }

    /// Сколько фраз прошло полностью.
    pub fn passed(&self) -> usize {
        self.results.iter().filter(|result| result.passed()).count()
    }

    /// Провалившиеся фразы.
    pub fn failed(&self) -> impl Iterator<Item = &PhraseResult> {
        self.results.iter().filter(|result| !result.passed())
    }
}

/// Оценить набор по ответам модели. Фразы без ответа считаются провалом.
pub fn evaluate(set: &GoldenSet, responses: &BTreeMap<String, Value>) -> Report {
    let results = set
        .phrases
        .iter()
        .map(|phrase| match responses.get(&phrase.id) {
            Some(data) => judge(&set.context, phrase, data),
            None => PhraseResult {
                id: phrase.id.clone(),
                category: phrase.category,
                totals: Totals {
                    expected: phrase.expect.items.len(),
                    ..Totals::default()
                },
                clarifications_ok: false,
                problems: vec!["нет ответа модели".to_owned()],
                error: Some("нет ответа модели".to_owned()),
            },
        })
        .collect();
    Report { results }
}

/// Оценить одну фразу по объекту `data`, который вернул Home Assistant.
pub fn judge(default_context: &ContextSpec, phrase: &GoldenPhrase, data: &Value) -> PhraseResult {
    let expected_items = phrase.expect.items.len();
    let expected_topics = phrase.expect.clarifications.len();
    let mut result = PhraseResult {
        id: phrase.id.clone(),
        category: phrase.category,
        totals: Totals {
            expected: expected_items + expected_topics,
            ..Totals::default()
        },
        clarifications_ok: expected_topics == 0,
        problems: Vec::new(),
        error: None,
    };

    let context = match phrase.effective_context(default_context).to_context() {
        Ok(context) => context,
        Err(error) => {
            result.error = Some(format!("контекст: {error}"));
            result.clarifications_ok = false;
            return result;
        }
    };
    let parsed = match parse_response(data) {
        Ok(parsed) => parsed,
        Err(error) => {
            result.error = Some(format!("ответ не разобран: {}", error.code()));
            result.clarifications_ok = false;
            return result;
        }
    };
    let validated = validate_items(&context, ItemSource::Telegram, parsed);
    let actual: Vec<ActualItem> = validated
        .items
        .iter()
        .map(|item| ActualItem::new(&context, item))
        .collect();

    let assignment = match_items(&phrase.expect.items, &actual);
    let mut used = vec![false; actual.len()];
    let mut matched = 0;
    for (index, slot) in assignment.iter().enumerate() {
        match slot {
            Some(slot) => {
                matched += 1;
                used[*slot] = true;
            }
            None => result.problems.push(format!(
                "не найден элемент «{}»",
                phrase.expect.items[index].title
            )),
        }
    }
    let extra = used.iter().filter(|used| !**used).count();
    for (slot, item) in actual.iter().enumerate() {
        if !used[slot] {
            result
                .problems
                .push(format!("лишний элемент «{}»", item.title));
        }
    }

    let topics = actual_topics(&validated, &context);
    let mut clarifications_matched = 0;
    for topic in &phrase.expect.clarifications {
        if topics.contains(topic) {
            clarifications_matched += 1;
        } else {
            result
                .problems
                .push(format!("нет уточнения по теме «{}»", topic.label()));
        }
    }
    result.clarifications_ok = clarifications_matched == expected_topics;
    // Ожидаемое уточнение — такая же единица метрики, как элемент: угадывание
    // без вопроса снижает точность, а не только помечает фразу.
    result.totals = Totals {
        matched: matched + clarifications_matched,
        expected: expected_items + expected_topics,
        extra,
    };

    result
}

/// Элемент, который получился бы из ответа модели после валидации.
struct ActualItem {
    title: String,
    kind: ListKind,
    list: Option<String>,
    assignee: Option<String>,
    due_date: Option<String>,
    due_time: Option<String>,
    recurrence: Option<RecurrenceInput>,
}

impl ActualItem {
    fn new(context: &CaptureContext, item: &DraftItem) -> Self {
        let list = item.list_id.and_then(|id| {
            context
                .lists
                .iter()
                .find(|list| list.id == id)
                .map(|list| list.name.clone())
        });
        let assignee = item
            .item
            .as_ref()
            .and_then(|new_item| new_item.assignee_user_id)
            .and_then(|id| {
                context
                    .members
                    .iter()
                    .find(|member| member.id == id)
                    .map(|member| member.display_name.clone())
            });
        let local = item
            .effective_due_at
            .map(|at| at.with_timezone(&context.timezone));
        let due_date = item
            .effective_due_date
            .map(|date| date.format("%Y-%m-%d").to_string())
            .or_else(|| local.map(|at| at.format("%Y-%m-%d").to_string()));
        let due_time = local.map(|at| format!("{:02}:{:02}", at.hour(), at.minute()));
        Self {
            title: item.title.clone(),
            kind: item.kind,
            list,
            assignee,
            due_date,
            due_time,
            recurrence: item
                .item
                .as_ref()
                .and_then(|new_item| new_item.recurrence.clone()),
        }
    }
}

/// Совпадение одного ожидаемого элемента с фактическим.
fn item_matches(expected: &ExpectedItem, actual: &ActualItem) -> bool {
    let title = fold(&actual.title);
    std::iter::once(&expected.title)
        .chain(&expected.alt_titles)
        .any(|candidate| fold(candidate) == title)
        && expected.kind == actual.kind
        && fold(&expected.list) == actual.list.as_deref().map(fold).unwrap_or_default()
        && expected.assignee.as_deref().map(fold) == actual.assignee.as_deref().map(fold)
        && expected.due_date == actual.due_date
        && expected.due_time == actual.due_time
        && recurrence_matches(expected.recurrence.as_ref(), actual.recurrence.as_ref())
}

fn recurrence_matches(
    expected: Option<&ExpectedRecurrence>,
    actual: Option<&RecurrenceInput>,
) -> bool {
    match (expected, actual) {
        (None, None) => true,
        (Some(expected), Some(actual)) => {
            let expected_interval = i16::try_from(expected.interval.unwrap_or(1)).unwrap_or(0);
            let expected_days: BTreeSet<i16> = expected
                .weekdays
                .iter()
                .filter_map(|day| i16::try_from(*day).ok())
                .collect();
            let actual_days: BTreeSet<i16> = actual.byweekday.iter().copied().collect();
            expected.freq == actual.freq
                && expected_interval == actual.interval.unwrap_or(1)
                && expected_days == actual_days
                && effective_month_rule(expected.freq, expected.month_rule)
                    == effective_month_rule(actual.freq, actual.month_rule)
                && expected.until.as_deref()
                    == actual
                        .until
                        .map(|date| date.format("%Y-%m-%d").to_string())
                        .as_deref()
        }
        _ => false,
    }
}

/// Правило месяца после нормализации: для месячного повторения отсутствие
/// правила означает `anchor_day`, для остальных частот правило не применяется.
fn effective_month_rule(freq: RecurrenceFreq, rule: Option<MonthRule>) -> Option<MonthRule> {
    match freq {
        RecurrenceFreq::Monthly => Some(rule.unwrap_or(MonthRule::AnchorDay)),
        _ => None,
    }
}

/// Максимальное паросочетание «ожидаемые → фактические».
///
/// Жадного выбора мало: один фактический элемент может подойти двум
/// ожидаемым, и порядок не должен решать исход. Алгоритм Куна переставляет
/// уже занятые пары и находит максимум.
fn match_items(expected: &[ExpectedItem], actual: &[ActualItem]) -> Vec<Option<usize>> {
    let mut owner: Vec<Option<usize>> = vec![None; actual.len()];
    for index in 0..expected.len() {
        let mut seen = vec![false; actual.len()];
        augment(index, expected, actual, &mut seen, &mut owner);
    }
    let mut assignment = vec![None; expected.len()];
    for (slot, owner) in owner.iter().enumerate() {
        if let Some(index) = owner {
            assignment[*index] = Some(slot);
        }
    }
    assignment
}

fn augment(
    index: usize,
    expected: &[ExpectedItem],
    actual: &[ActualItem],
    seen: &mut [bool],
    owner: &mut [Option<usize>],
) -> bool {
    for slot in 0..actual.len() {
        if seen[slot] || !item_matches(&expected[index], &actual[slot]) {
            continue;
        }
        seen[slot] = true;
        match owner[slot] {
            None => {
                owner[slot] = Some(index);
                return true;
            }
            Some(current) => {
                if augment(current, expected, actual, seen, owner) {
                    owner[slot] = Some(index);
                    return true;
                }
            }
        }
    }
    false
}

/// Темы уточнений, которые фактически получил пользователь: вопросы модели и
/// ошибки валидации, по которым карточка предлагает выбор.
fn actual_topics(
    validated: &ValidatedDraft,
    context: &CaptureContext,
) -> BTreeSet<ClarificationTopic> {
    let mut topics = BTreeSet::new();
    for item in &validated.items {
        for error in &item.errors {
            if error.code == ASSIGNEE_AMBIGUOUS {
                topics.insert(ClarificationTopic::Assignee);
            }
            if error.field == "due_date" && (error.code == DATE_PAST || error.code == DATE_INVALID)
            {
                topics.insert(ClarificationTopic::DueDate);
            }
        }
    }
    for question in &validated.clarifications {
        topics.extend(clarification_topics(question, context));
    }
    topics
}

/// Темы свободно сформулированного вопроса. Проверяется тема, а не текст.
///
/// Вопрос может спрашивать о двух вещах сразу («кому поручить и когда?»),
/// поэтому возвращается набор. Упоминание участника считается темой
/// исполнителя только если вопрос не про дату: «когда позвонить Алексею?» —
/// это вопрос о времени, а не о том, кому звонить.
fn clarification_topics(question: &str, context: &CaptureContext) -> BTreeSet<ClarificationTopic> {
    // Основы слов, а не словоформы: модель спрашивает «на какую дату»,
    // «уточните время», «какое число».
    const DUE: [&str; 8] = [
        "когда",
        "во сколько",
        "какой день",
        "дат",
        "врем",
        "числ",
        "срок",
        "deadline",
    ];
    const ASSIGNEE: [&str; 5] = ["кто", "кому", "кого", "исполнител", "участник"];

    let text = fold(question);
    let asks_due = DUE.iter().any(|needle| text.contains(needle));
    let asks_assignee = ASSIGNEE.iter().any(|needle| text.contains(needle));
    let names_member = !asks_due
        && context.members.iter().any(|member| {
            let name = fold(&member.display_name);
            name.chars().count() >= 3 && text.contains(&name)
        });

    let mut topics = BTreeSet::new();
    if asks_due {
        topics.insert(ClarificationTopic::DueDate);
    }
    if asks_assignee || names_member {
        topics.insert(ClarificationTopic::Assignee);
    }
    topics
}

/// Нормализация для сравнения: регистр, пробелы, `ё`/`е`.
fn fold(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
        .replace('ё', "е")
}

/// Запись baseline для одной версии промпта.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BaselineEntry {
    /// Общая точность на golden set.
    pub accuracy: f64,
    /// SHA-256 отрисованного промпта и структуры ответа.
    pub prompt_hash: String,
    /// Когда выполнялся живой замер, RFC3339.
    pub date: String,
    /// Сколько фраз в наборе.
    pub phrases: usize,
    /// Сколько фраз прошло полностью.
    pub passed: usize,
}

/// Baseline по версиям промпта: `{"capture-v3": {...}}`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Baseline(pub BTreeMap<String, BaselineEntry>);

impl Baseline {
    /// Разобрать JSON baseline. Пустой файл — это `{}`.
    pub fn parse(text: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(text)
    }

    /// Прочитать baseline; отсутствующий файл означает «замера ещё не было».
    pub fn load(path: &Path) -> Result<Self, std::io::Error> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text).map_err(std::io::Error::other),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error),
        }
    }

    /// Записать baseline в файл с отступом, чтобы дифф был читаемым.
    pub fn save(&self, path: &Path) -> Result<(), std::io::Error> {
        let text = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(path, format!("{text}\n"))
    }

    /// Записей нет: живой замер ещё не выполнялся.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Запись для версии промпта.
    pub fn get(&self, version: &str) -> Option<&BaselineEntry> {
        self.0.get(version)
    }

    /// Сохранить результат замера для версии, не трогая другие версии.
    pub fn record(&mut self, version: &str, entry: BaselineEntry) {
        self.0.insert(version.to_owned(), entry);
    }
}

/// Итог проверки baseline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardOutcome {
    /// Baseline пуст: живой замер ещё не выполнялся.
    Skipped,
    /// Для версии есть свежий проходной замер.
    Passed,
}

/// Проверить, что версия промпта имеет свежий проходной замер.
///
/// Пустой baseline — не ошибка: замера ещё не было, и guard объясняет это
/// вызывающему коду. Любая запись означает, что замер был, поэтому для
/// текущей версии обязательны точность ≥ [`MIN_ACCURACY`] и совпадение хеша
/// исходников промпта.
pub fn check_baseline(
    baseline: &Baseline,
    version: &str,
    prompt_hash: &str,
) -> Result<GuardOutcome, String> {
    if baseline.is_empty() {
        return Ok(GuardOutcome::Skipped);
    }
    let entry = baseline.get(version).ok_or_else(|| {
        format!(
            "в tests/golden/baseline.json нет записи для {version}: \
             прогоните `tasklerq capture-eval`"
        )
    })?;
    if entry.accuracy < MIN_ACCURACY {
        return Err(format!(
            "точность {:.3} для {version} ниже {MIN_ACCURACY}: \
             прогоните `tasklerq capture-eval` и исправьте промпт",
            entry.accuracy
        ));
    }
    if entry.prompt_hash != prompt_hash {
        return Err(format!(
            "исходники промпта изменились без нового замера для {version}: \
             прогоните `tasklerq capture-eval`"
        ));
    }
    Ok(GuardOutcome::Passed)
}

/// Канонический контекст для хеша: значения фиксированы, поэтому хеш зависит
/// только от промпта, а не от текущей даты или household.
fn canonical_context() -> CaptureContext {
    let spec = ContextSpec {
        now: "2026-10-30T11:05:00Z".to_owned(),
        timezone: "Europe/Moscow".to_owned(),
        author: Some("Анна".to_owned()),
        lists: vec![
            ListSpec {
                name: "Задачи".to_owned(),
                kind: ListKind::Tasks,
                default: true,
            },
            ListSpec {
                name: "Покупки".to_owned(),
                kind: ListKind::Shopping,
                default: true,
            },
            ListSpec {
                name: "Дача".to_owned(),
                kind: ListKind::Tasks,
                default: false,
            },
        ],
        members: vec![
            MemberSpec {
                name: "Анна".to_owned(),
                username: Some("anna".to_owned()),
            },
            MemberSpec {
                name: "Пётр".to_owned(),
                username: None,
            },
        ],
    };
    spec.to_context()
        .expect("canonical prompt context is valid")
}

/// SHA-256 отрисованного промпта на фиксированном контексте и структуры
/// ответа.
///
/// Хеш покрывает не только шаблон, но и `build_instructions` (формат времени,
/// кавычки, нейтрализацию текста): правка любого из них меняет промпт и должна
/// требовать нового замера. Именно этот хеш пишется в baseline.
pub fn prompt_hash() -> String {
    let rendered = prompt::build_instructions(&canonical_context(), "проверка промпта");
    let mut digest = Sha256::new();
    digest.update(rendered.as_bytes());
    digest.update(b"\n--response-structure--\n");
    digest.update(prompt::response_structure().to_string().as_bytes());
    digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
