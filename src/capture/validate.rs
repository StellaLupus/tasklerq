//! Серверная валидация элементов черновика.
//!
//! LLM не доверяют: каждый элемент проверяется независимо, ошибки
//! привязываются к элементу и собираются все сразу, чтобы пользователь мог
//! исправить их за один проход. Элемент с ошибкой не получает [`NewItem`], а
//! значит не может быть сохранён. Идентификаторы из ответа LLM сюда не
//! доходят: парсер их не читает, а список и исполнитель выбираются только из
//! контекста household.

use chrono::{DateTime, Months, NaiveDate, NaiveTime, Utc};
use uuid::Uuid;

use super::context::{CaptureContext, ListRef};
use super::schema::{ParsedResponse, RawItem, RawRecurrence};
use crate::domain::models::{
    ItemPriority, ItemSource, ListKind, MonthRule, NewItem, RecurrenceFreq, RecurrenceInput,
    SubtaskInput,
};
use crate::domain::recurrence::resolve_local;
use crate::domain::validation::{
    MAX_NOTES, MAX_SUBTASKS, MAX_TAG, MAX_TAGS, MAX_TITLE, MAX_UNIT, normalize_due_for_recurrence,
    normalize_recurrence, sanitize, validate_new_item, validate_quantity,
};

/// Максимальное количество позиции.
const MAX_QUANTITY: f64 = 9_999.0;
/// Максимум интервала повторения.
const MAX_INTERVAL: i64 = 365;
/// Горизонт сроков и окончания серии, месяцы.
const HORIZON_MONTHS: u32 = 60;

/// Ошибка элемента: поле, машинный код и текст для пользователя.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ItemError {
    /// Поле элемента, к которому относится ошибка.
    pub field: &'static str,
    /// Стабильный код для метрик и тестов.
    pub code: &'static str,
    /// Сообщение на русском.
    pub message: &'static str,
}

/// Код ошибки «исполнитель неоднозначен»: по нему экран предложит выбор.
pub const ASSIGNEE_AMBIGUOUS: &str = "assignee_ambiguous";

/// Элемент черновика после проверки.
#[derive(Debug, Clone)]
pub struct DraftItem {
    /// Тип списка из ответа LLM.
    pub kind: ListKind,
    /// Очищенное название; для показа, даже если элемент невалиден.
    pub title: String,
    /// Выбранный список.
    pub list_id: Option<Uuid>,
    /// Список назван LLM, но не найден, и выбран список по умолчанию.
    pub list_auto_selected: bool,
    /// Ошибки; пусто у валидного элемента.
    pub errors: Vec<ItemError>,
    /// Участники, подходящие под неоднозначное имя исполнителя.
    pub assignee_candidates: Vec<Uuid>,
    /// Данные для создания; `Some` только у валидного элемента.
    pub item: Option<NewItem>,
    /// Срок со временем таким, каким он станет после сохранения: домен
    /// подставляет первое вхождение серии без срока и сдвигает срок серии
    /// «в последний день месяца». Показывать нужно его, а не `item.due_at`.
    pub effective_due_at: Option<DateTime<Utc>>,
    /// Вседневный срок после сохранения (см. [`DraftItem::effective_due_at`]).
    pub effective_due_date: Option<NaiveDate>,
}

impl DraftItem {
    /// Элемент можно сохранять.
    pub fn is_valid(&self) -> bool {
        self.errors.is_empty()
    }
}

/// Проверенный черновик.
#[derive(Debug, Clone)]
pub struct ValidatedDraft {
    /// Элементы в порядке ответа LLM.
    pub items: Vec<DraftItem>,
    /// Уточняющие вопросы LLM.
    pub clarifications: Vec<String>,
}

impl ValidatedDraft {
    /// Все элементы валидны (и черновик не пуст).
    pub fn is_valid(&self) -> bool {
        !self.items.is_empty() && self.items.iter().all(DraftItem::is_valid)
    }
}

/// Проверить разобранный ответ LLM. `source` — источник создаваемых элементов.
pub fn validate_items(
    context: &CaptureContext,
    source: ItemSource,
    parsed: ParsedResponse,
) -> ValidatedDraft {
    ValidatedDraft {
        items: parsed
            .items
            .into_iter()
            .map(|raw| validate_item(context, source, raw))
            .collect(),
        clarifications: parsed.clarifications,
    }
}

/// Проверить один элемент.
pub fn validate_item(context: &CaptureContext, source: ItemSource, raw: RawItem) -> DraftItem {
    let mut errors = Vec::new();
    let today = context.today();

    let title = clean(&raw.title);
    if title.is_empty() || title.chars().count() > MAX_TITLE {
        fail(
            &mut errors,
            "title",
            "title_length",
            "Название должно быть от 1 до 200 символов",
        );
    }

    let notes = raw
        .notes
        .as_deref()
        .map(clean)
        .filter(|notes| !notes.is_empty());
    if notes
        .as_ref()
        .is_some_and(|notes| notes.chars().count() > MAX_NOTES)
    {
        fail(
            &mut errors,
            "notes",
            "notes_length",
            "Описание слишком длинное",
        );
    }

    let priority = parse_priority(raw.priority.as_deref(), &mut errors);

    let (list, list_auto_selected) = resolve_list(context, &raw);
    if list.is_none() {
        fail(
            &mut errors,
            "target_list",
            "list_missing",
            "Нет подходящего списка",
        );
    }

    let (assignee_user_id, assignee_candidates) = match (raw.assignee_id, raw.assignee.as_deref()) {
        // Выбор пользователя в форме правки: проверяем только, что участник
        // всё ещё в household.
        (Some(id), _) => {
            if context.members.iter().any(|member| member.id == id) {
                (Some(id), Vec::new())
            } else {
                fail(
                    &mut errors,
                    "assignee",
                    "assignee_unknown",
                    "Такого участника нет",
                );
                (None, Vec::new())
            }
        }
        (None, None) => (None, Vec::new()),
        (None, Some(name)) => match resolve_assignee(context, name) {
            Assignee::Found(id) => (Some(id), Vec::new()),
            Assignee::Unknown => {
                fail(
                    &mut errors,
                    "assignee",
                    "assignee_unknown",
                    "Такого участника нет",
                );
                (None, Vec::new())
            }
            Assignee::Ambiguous(candidates) => {
                fail(
                    &mut errors,
                    "assignee",
                    ASSIGNEE_AMBIGUOUS,
                    "Уточните исполнителя: подходит несколько участников",
                );
                (None, candidates)
            }
        },
    };

    let (due_at, due_date) = resolve_due(context, &raw, &mut errors);

    let tags = clean_tags(&raw.tags, &mut errors);
    let subtasks = clean_subtasks(&raw.subtasks, &mut errors);

    let quantity = raw.quantity.and_then(|quantity| {
        if !(quantity.is_finite() && quantity > 0.0 && quantity <= MAX_QUANTITY) {
            fail(
                &mut errors,
                "quantity",
                "quantity_range",
                "Количество вне диапазона",
            );
            None
        } else if validate_quantity(Some(quantity)).is_err() {
            fail(
                &mut errors,
                "quantity",
                "quantity_precision",
                "Не больше трёх знаков после запятой",
            );
            None
        } else {
            Some(quantity)
        }
    });
    // Как и `normalize_unit` в домене, слишком длинную единицу усекаем молча:
    // это подпись, а не данные, и отказывать из-за неё нет смысла.
    let unit = raw
        .unit
        .as_deref()
        .map(clean)
        .filter(|unit| !unit.is_empty())
        .map(|unit| unit.chars().take(MAX_UNIT).collect::<String>());

    let recurrence = raw.recurrence.as_ref().and_then(|recurrence| {
        let start = due_date
            .or_else(|| due_at.map(|at| local_date(context, at)))
            .unwrap_or(today);
        validate_recurrence(
            recurrence,
            raw.target_kind,
            Schedule {
                context,
                start,
                due_at,
                due_date,
            },
            &mut errors,
        )
    });

    let confirmed = match (list, errors.is_empty()) {
        (Some(list), true) => {
            let item = NewItem {
                list_id: list.id,
                title: title.clone(),
                notes,
                priority,
                due_at,
                due_date,
                assignee_user_id,
                source,
                quantity,
                unit,
                tags,
                subtasks,
                recurrence,
            };
            confirm_domain(context, list.kind, item, today, &mut errors)
        }
        _ => None,
    };
    let (item, effective_due_at, effective_due_date) = match confirmed {
        Some(Normalized {
            item,
            due_at,
            due_date,
        }) => (Some(item), due_at, due_date),
        None => (None, due_at, due_date),
    };

    DraftItem {
        kind: raw.target_kind,
        title,
        list_id: list.map(|list| list.id),
        list_auto_selected,
        errors,
        assignee_candidates,
        item,
        effective_due_at,
        effective_due_date,
    }
}

/// Элемент, принятый доменом, и срок после нормализации.
struct Normalized {
    item: NewItem,
    due_at: Option<DateTime<Utc>>,
    due_date: Option<NaiveDate>,
}

/// Страховка: доменная проверка репозитория должна согласиться с нашей, иначе
/// ошибка всплыла бы только при сохранении. Расхождение — ошибка в коде, а не
/// во вводе, поэтому причину пишем в журнал: текст ошибки домена фиксирован и
/// содержимого заметки не включает.
///
/// Возвращает элемент и срок после нормализации домена: его покажет карточка.
/// Сам `NewItem` остаётся исходным — при сохранении домен проходит по нему
/// заново и получает тот же результат, а подмена срока сдвинула бы якорь серии.
fn confirm_domain(
    context: &CaptureContext,
    list_kind: ListKind,
    item: NewItem,
    today: NaiveDate,
    errors: &mut Vec<ItemError>,
) -> Option<Normalized> {
    match validate_new_item(list_kind, context.timezone.name(), item.clone(), today) {
        Ok(validated) => Some(Normalized {
            item,
            due_at: validated.due_at,
            due_date: validated.due_date,
        }),
        Err(error) => {
            tracing::warn!(%error, "domain rejected a capture item that passed validation");
            fail(
                errors,
                "item",
                "domain_rejected",
                "Элемент не прошёл проверку",
            );
            None
        }
    }
}

fn fail(
    errors: &mut Vec<ItemError>,
    field: &'static str,
    code: &'static str,
    message: &'static str,
) {
    errors.push(ItemError {
        field,
        code,
        message,
    });
}

/// Список по имени среди списков нужного типа; иначе список по умолчанию.
fn resolve_list<'a>(context: &'a CaptureContext, raw: &RawItem) -> (Option<&'a ListRef>, bool) {
    let named = raw.target_list.as_deref().map(fold);
    if let Some(name) = &named {
        let found = context
            .lists
            .iter()
            .find(|list| list.kind == raw.target_kind && fold(&list.name) == *name);
        if found.is_some() {
            return (found, false);
        }
    }
    (context.default_list(raw.target_kind), named.is_some())
}

enum Assignee {
    Found(Uuid),
    Unknown,
    Ambiguous(Vec<Uuid>),
}

/// Участник по отображаемому имени или username; только точное совпадение.
fn resolve_assignee(context: &CaptureContext, name: &str) -> Assignee {
    let name = fold(name.trim().trim_start_matches('@'));
    let mut matches: Vec<Uuid> = Vec::new();
    for member in &context.members {
        let by_name = fold(&member.display_name) == name;
        let by_username = member
            .username
            .as_deref()
            .is_some_and(|username| fold(username) == name);
        if (by_name || by_username) && !matches.contains(&member.id) {
            matches.push(member.id);
        }
    }
    match matches.len() {
        0 => Assignee::Unknown,
        1 => Assignee::Found(matches[0]),
        _ => Assignee::Ambiguous(matches),
    }
}

fn clean_tags(tags: &[String], errors: &mut Vec<ItemError>) -> Vec<String> {
    let cleaned: Vec<String> = tags
        .iter()
        .map(|tag| clean(tag).trim_start_matches('#').trim().to_lowercase())
        .filter(|tag| !tag.is_empty())
        .collect();
    if cleaned.len() > MAX_TAGS {
        fail(errors, "tags", "tags_count", "Слишком много тегов");
    }
    if cleaned.iter().any(|tag| tag.chars().count() > MAX_TAG) {
        fail(errors, "tags", "tag_length", "Тег слишком длинный");
    }
    let mut unique = Vec::with_capacity(cleaned.len());
    for tag in cleaned {
        if !unique.contains(&tag) {
            unique.push(tag);
        }
    }
    unique
}

fn clean_subtasks(subtasks: &[String], errors: &mut Vec<ItemError>) -> Vec<SubtaskInput> {
    let cleaned: Vec<String> = subtasks
        .iter()
        .map(|title| clean(title))
        .filter(|title| !title.is_empty())
        .collect();
    if cleaned.len() > MAX_SUBTASKS {
        fail(
            errors,
            "subtasks",
            "subtasks_count",
            "Слишком много подзадач",
        );
    }
    if cleaned
        .iter()
        .any(|title| title.chars().count() > MAX_TITLE)
    {
        fail(
            errors,
            "subtasks",
            "subtask_length",
            "Название подзадачи слишком длинное",
        );
    }
    cleaned
        .into_iter()
        .map(|title| SubtaskInput {
            title,
            status: None,
        })
        .collect()
}

/// Срок элемента, к которому привязывается серия.
struct Schedule<'a> {
    context: &'a CaptureContext,
    /// Дата начала серии: срок, а без срока — сегодня.
    start: NaiveDate,
    due_at: Option<DateTime<Utc>>,
    due_date: Option<NaiveDate>,
}

fn validate_recurrence(
    raw: &RawRecurrence,
    list_kind: ListKind,
    schedule: Schedule<'_>,
    errors: &mut Vec<ItemError>,
) -> Option<RecurrenceInput> {
    let Schedule { start, .. } = schedule;
    let errors_before = errors.len();

    if list_kind != ListKind::Tasks {
        fail(
            errors,
            "recurrence",
            "recurrence_kind",
            "Повторение доступно только для задач",
        );
    }

    let freq = match raw.freq.as_deref().map(fold).as_deref() {
        None => {
            fail(
                errors,
                "recurrence",
                "recurrence_freq",
                "Укажите частоту повторения",
            );
            None
        }
        Some("daily") => Some(RecurrenceFreq::Daily),
        Some("weekly") => Some(RecurrenceFreq::Weekly),
        Some("monthly") => Some(RecurrenceFreq::Monthly),
        Some("yearly") => Some(RecurrenceFreq::Yearly),
        Some(_) => {
            fail(
                errors,
                "recurrence",
                "recurrence_freq",
                "Неизвестная частота повторения",
            );
            None
        }
    };

    let interval = raw.interval.unwrap_or(1);
    if !(1..=MAX_INTERVAL).contains(&interval) {
        fail(
            errors,
            "recurrence",
            "recurrence_interval",
            "Интервал — от 1 до 365",
        );
    }

    // Дни недели и правило месяца имеют смысл только для своей частоты; у
    // остальных частот их заполняет модель по привычке, и это не ошибка.
    let weekdays = if freq == Some(RecurrenceFreq::Weekly) {
        if raw.weekdays.is_empty() {
            fail(
                errors,
                "recurrence",
                "recurrence_weekdays",
                "Укажите дни недели",
            );
        } else if raw.weekdays.iter().any(|day| !(1..=7).contains(day)) {
            fail(
                errors,
                "recurrence",
                "recurrence_weekdays",
                "День недели — от 1 до 7",
            );
        }
        raw.weekdays.clone()
    } else {
        Vec::new()
    };

    let month_rule = if freq == Some(RecurrenceFreq::Monthly) {
        match raw.month_rule.as_deref().map(fold).as_deref() {
            None => None,
            Some("anchor_day") => Some(MonthRule::AnchorDay),
            Some("last_day") => Some(MonthRule::LastDay),
            Some(_) => {
                fail(
                    errors,
                    "recurrence",
                    "recurrence_month_rule",
                    "Неизвестное правило месяца",
                );
                None
            }
        }
    } else {
        None
    };

    let until = match raw.until.as_deref() {
        None => None,
        Some(value) => match parse_date(value) {
            Some(until) if until >= start && until <= horizon(start) => Some(until),
            _ => {
                fail(
                    errors,
                    "recurrence",
                    "recurrence_until",
                    "Неверная дата окончания",
                );
                None
            }
        },
    };

    if errors.len() > errors_before {
        return None;
    }
    let input = RecurrenceInput {
        freq: freq?,
        interval: i16::try_from(interval).ok(),
        byweekday: weekdays
            .into_iter()
            .filter_map(|day| i16::try_from(day).ok())
            .collect(),
        until,
        timezone: None,
        anchor_date: None,
        month_rule,
    };

    // Отдельные поля верны, но вместе они могут не дать ни одного вхождения:
    // первое вхождение позже даты окончания. Домен отвергает такое при
    // сохранении, поэтому объясняем это здесь, а не общим «элемент не прошёл
    // проверку». Других причин отказа после проверок выше сейчас нет; если они
    // появятся, сообщение и код нужно разделить.
    let today = schedule.context.today();
    let timezone = schedule.context.timezone.name();
    let fits = normalize_recurrence(
        input.clone(),
        timezone,
        schedule.due_at,
        schedule.due_date,
        today,
    )
    .and_then(|rule| {
        normalize_due_for_recurrence(
            schedule.due_at,
            schedule.due_date,
            Some(&rule),
            timezone,
            today,
        )
    });
    if fits.is_err() {
        fail(
            errors,
            "recurrence",
            "recurrence_range",
            "Серия закончится раньше первого повторения",
        );
        return None;
    }
    Some(input)
}

/// Убрать управляющие символы и крайние пробелы.
fn clean(value: &str) -> String {
    sanitize(value).trim().to_owned()
}

/// Сравнение имён без регистра и различия «е»/«ё».
fn fold(value: &str) -> String {
    value.trim().to_lowercase().replace('ё', "е")
}

/// Дата строго `YYYY-MM-DD`: `chrono` принимает и `2026-1-3`, и `+2026-11-03`,
/// а контракт промпта требует ровно десять символов.
pub(crate) fn parse_date(value: &str) -> Option<NaiveDate> {
    let value = value.trim();
    if value.len() != 10 {
        return None;
    }
    NaiveDate::parse_from_str(value, "%Y-%m-%d").ok()
}

/// Время `HH:MM`; секунды (`HH:MM:SS`) принимаются, потому что модели их
/// добавляют сами, а отказывать из-за них нет смысла.
pub(crate) fn parse_time(value: &str) -> Option<NaiveTime> {
    let value = value.trim();
    match value.len() {
        5 => NaiveTime::parse_from_str(value, "%H:%M").ok(),
        8 => NaiveTime::parse_from_str(value, "%H:%M:%S").ok(),
        _ => None,
    }
}

/// Результат разбора необязательной строки LLM.
enum Parsed<T> {
    /// Поля нет.
    Absent,
    /// Поле есть, но не разобралось.
    Invalid,
    Valid(T),
}

impl<T> Parsed<T> {
    fn of(value: Option<&str>, parse: fn(&str) -> Option<T>) -> Self {
        match value {
            None => Self::Absent,
            Some(value) => parse(value).map_or(Self::Invalid, Self::Valid),
        }
    }
}

fn parse_priority(value: Option<&str>, errors: &mut Vec<ItemError>) -> ItemPriority {
    match value.map(fold).as_deref() {
        None | Some("normal") => ItemPriority::Normal,
        Some("low") => ItemPriority::Low,
        Some("high") => ItemPriority::High,
        Some("urgent") => ItemPriority::Urgent,
        Some(_) => {
            fail(
                errors,
                "priority",
                "priority_unknown",
                "Неизвестный приоритет",
            );
            ItemPriority::Normal
        }
    }
}

/// Срок по дате и времени из ответа LLM: дата без времени даёт вседневный
/// срок, дата со временем — момент.
fn resolve_due(
    context: &CaptureContext,
    raw: &RawItem,
    errors: &mut Vec<ItemError>,
) -> (Option<DateTime<Utc>>, Option<NaiveDate>) {
    let today = context.today();
    let date = Parsed::of(raw.due_date.as_deref(), parse_date);
    let time = Parsed::of(raw.due_time.as_deref(), parse_time);

    let date = match (date, &time) {
        (Parsed::Valid(date), _) => date,
        (Parsed::Invalid, time) => {
            fail(errors, "due_date", "date_invalid", "Неверная дата");
            if matches!(time, Parsed::Invalid) {
                fail(errors, "due_time", "time_invalid", "Неверное время");
            }
            return (None, None);
        }
        (Parsed::Absent, Parsed::Invalid) => {
            fail(errors, "due_time", "time_invalid", "Неверное время");
            return (None, None);
        }
        (Parsed::Absent, Parsed::Valid(_)) => {
            fail(
                errors,
                "due_time",
                "time_without_date",
                "Укажите дату для времени",
            );
            return (None, None);
        }
        (Parsed::Absent, Parsed::Absent) => return (None, None),
    };

    if date < today {
        fail(errors, "due_date", "date_past", "Дата уже прошла");
    } else if date > horizon(today) {
        fail(errors, "due_date", "date_far", "Слишком далёкая дата");
    }
    match time {
        Parsed::Absent => (None, Some(date)),
        Parsed::Invalid => {
            fail(errors, "due_time", "time_invalid", "Неверное время");
            (None, None)
        }
        Parsed::Valid(time) => match resolve_local(context.timezone, date, time) {
            Ok(at) => {
                // Прошедшая дата уже названа ошибкой; здесь — сегодняшнее время,
                // которое уже наступило. Сравнение по минутам: модель видит
                // время без секунд.
                if date >= today && at < context.now_minute() {
                    fail(errors, "due_time", "time_past", "Это время уже прошло");
                }
                (Some(at), None)
            }
            Err(_) => {
                fail(errors, "due_time", "time_invalid", "Неверное время");
                (None, None)
            }
        },
    }
}

/// Самая поздняя допустимая дата: пять лет от `from`.
fn horizon(from: NaiveDate) -> NaiveDate {
    from.checked_add_months(Months::new(HORIZON_MONTHS))
        .unwrap_or(NaiveDate::MAX)
}

fn local_date(context: &CaptureContext, at: DateTime<Utc>) -> NaiveDate {
    at.with_timezone(&context.timezone).date_naive()
}

#[cfg(test)]
mod tests;
