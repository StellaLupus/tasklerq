//! Русская локализация: склонения, даты и разбор пользовательского ввода.

use chrono::{DateTime, Datelike, NaiveDate, NaiveTime, Utc};
use chrono_tz::Tz;

/// Выбрать форму существительного для числа: «1 задача», «2 задачи», «5 задач».
pub fn plural<'a>(n: i64, one: &'a str, few: &'a str, many: &'a str) -> &'a str {
    let n = n.unsigned_abs();
    let last_two = n % 100;
    let last = n % 10;
    if (11..=14).contains(&last_two) {
        many
    } else if last == 1 {
        one
    } else if (2..=4).contains(&last) {
        few
    } else {
        many
    }
}

/// «5 задач».
pub fn tasks(n: i64) -> String {
    format!("{n} {}", plural(n, "задача", "задачи", "задач"))
}

/// «5 покупок».
pub fn purchases(n: i64) -> String {
    format!("{n} {}", plural(n, "покупка", "покупки", "покупок"))
}

/// «5 позиций».
pub fn positions(n: i64) -> String {
    format!("{n} {}", plural(n, "позиция", "позиции", "позиций"))
}

/// «5 просроченных».
pub fn overdue(n: i64) -> String {
    format!(
        "{n} {}",
        plural(n, "просрочена", "просрочены", "просрочено")
    )
}

/// «5 минут».
pub fn minutes(n: i64) -> String {
    format!("{n} {}", plural(n, "минута", "минуты", "минут"))
}

/// «3 дня».
pub fn days(n: i64) -> String {
    format!("{n} {}", plural(n, "день", "дня", "дней"))
}

/// «2 часа».
pub fn hours(n: i64) -> String {
    format!("{n} {}", plural(n, "час", "часа", "часов"))
}

/// «5 результатов».
pub fn results(n: i64) -> String {
    format!(
        "{n} {}",
        plural(n, "результат", "результата", "результатов")
    )
}

/// Сокращённое название месяца в родительном падеже.
pub fn month_short(month: u32) -> &'static str {
    [
        "янв", "фев", "мар", "апр", "мая", "июн", "июл", "авг", "сен", "окт", "ноя", "дек",
    ]
    .get(usize::try_from(month).unwrap_or(1).saturating_sub(1))
    .copied()
    .unwrap_or("")
}

/// Сокращённое название дня недели (ISO 1..=7).
pub fn weekday_short(iso: u32) -> &'static str {
    ["пн", "вт", "ср", "чт", "пт", "сб", "вс"]
        .get(usize::try_from(iso).unwrap_or(1).saturating_sub(1))
        .copied()
        .unwrap_or("")
}

/// Подпись срока без времени.
pub const ALL_DAY: &str = "весь день";

/// Дата в человекочитаемом виде относительно `today`.
pub fn date_label(date: NaiveDate, today: NaiveDate) -> String {
    match (date - today).num_days() {
        -1 => "вчера".to_owned(),
        0 => "сегодня".to_owned(),
        1 => "завтра".to_owned(),
        2 => "послезавтра".to_owned(),
        _ if date.year() == today.year() => format!(
            "{}, {} {}",
            weekday_short(date.weekday().number_from_monday()),
            date.day(),
            month_short(date.month())
        ),
        _ => format!(
            "{} {} {}",
            date.day(),
            month_short(date.month()),
            date.year()
        ),
    }
}

/// Срок элемента: «сегодня», «завтра, 18:00», «3 ноя 2026, 18:00».
pub fn due_label(
    due_at: Option<DateTime<Utc>>,
    due_date: Option<NaiveDate>,
    timezone: Tz,
    now: DateTime<Utc>,
) -> Option<String> {
    let today = now.with_timezone(&timezone).date_naive();
    if let Some(due_at) = due_at {
        let local = due_at.with_timezone(&timezone);
        return Some(format!(
            "{}, {}",
            date_label(local.date_naive(), today),
            local.format("%H:%M")
        ));
    }
    due_date.map(|date| date_label(date, today))
}

/// Счётчик выполненных сегодня в шапке списка задач.
pub fn done_today_counter(count: usize) -> String {
    format!("✅ {count}")
}

/// С заглавной буквы.
pub fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// Заголовок первого контакта: у пространства уже есть владелец.
pub const PRIVATE_SPACE_TITLE: &str = "Это частное пространство";
/// Заголовок первого контакта: пространство без владельца или не настроено.
pub const NOT_CONFIGURED_TITLE: &str = "Пространство ещё не настроено";
/// Заголовок первого контакта: в `single` больше одного household.
pub const NEEDS_ADMIN_TITLE: &str = "Нужен администратор";
/// Экран первого контакта: у пространства уже есть владелец.
pub const ASK_INVITE: &str = "Попросите приглашение у владельца.";
/// Экран первого контакта: пространство без владельца.
pub const ADMIN_COMMAND_INTRO: &str = "Владельца назначает администратор командой:";
/// Экран первого контакта: инсталляция ещё не настроена.
pub const NOT_CONFIGURED: &str = "Обратитесь к администратору.";
/// Экран первого контакта: бот не назначает владельца сам.
pub const OWNER_NOT_ASSIGNED: &str = "Бот не назначает владельца сам.";
/// Экран первого контакта: в `single` больше одного household.
pub const NEEDS_ADMIN: &str = "Инсталляция требует вмешательства администратора.";
/// Нейтральное сообщение, когда обновление так и не удалось обработать.
///
/// Без деталей ошибки и пользовательского текста: сообщение уходит после
/// исчерпания повторов, когда причина уже неизвестна.
pub const INTERNAL_ERROR: &str = "Не получилось выполнить действие. Попробуйте ещё раз чуть позже.";

/// Кнопка выполнения задачи.
pub const DONE_BUTTON: &str = "✓ Выполнено";
/// Кнопка отметки покупки.
pub const BOUGHT_BUTTON: &str = "✓ Куплено";
/// Кнопка группового выполнения.
pub const DONE_ALL_BUTTON: &str = "✓ Выполнить все";
/// Кнопка закрытия выбора нескольких значений.
pub const APPLY_BUTTON: &str = "Применить";
/// Всплывающее сообщение после выполнения.
pub const DONE_TOAST: &str = "Выполнено";
/// Кнопка возврата на предыдущий экран.
pub const BACK_BUTTON: &str = "⬅️ Назад";
/// Кнопка возврата в главное меню.
pub const MENU_BUTTON: &str = "🏠 Меню";
/// Подсказка свободного текста в меню при включённом разборе.
pub const MENU_HINT: &str = "Напишите задачу или покупку — я разберу.";
/// Подсказка свободного текста в меню без разбора: текст сразу становится
/// названием задачи.
pub const MENU_HINT_NO_LLM: &str = "Напишите название задачи — открою редактор задачи.";
/// Подсказка свободного текста на экране поиска и результатов.
pub const SEARCH_HINT: &str = "Напишите запрос";
/// Баннер, когда редактор открыт, но ни одно поле не ждёт текста.
pub const EDITOR_DIRTY: &str = "Сначала сохраните или отмените правку.";
/// Подсказка свободного текста в списке: текст добавит позиции в этот список.
pub fn list_hint(name: &str) -> String {
    format!("Напишите — добавлю в «{name}»")
}
/// Раздел выполненных сегодня задач в списке.
pub const DONE_TODAY_TITLE: &str = "Выполнено сегодня";
/// Кнопка включения режима правки списка покупок.
pub const EDIT_BUTTON: &str = "✏️ Править";
/// Кнопка выключения режима правки списка покупок.
pub const EDIT_FINISH_BUTTON: &str = "✓ Закончить правку";
/// Пометка режима правки в заголовке списка покупок.
pub const EDIT_MODE_TITLE: &str = "правка";
/// Кнопка открытия списка из уведомления.
pub const OPEN_LIST_BUTTON: &str = "📋 Открыть список";

/// Команда CLI для назначения владельца с подставленным Telegram ID.
pub fn owner_command(telegram_id: i64) -> String {
    format!("tasklerq admin user add --telegram-id {telegram_id} --role owner")
}

/// Заголовок экрана «Интеграции».
pub const INTEGRATIONS_TITLE: &str = "Интеграции";
/// Экран «Интеграции» без активных токенов.
pub const INTEGRATIONS_EMPTY: &str = "Активных токенов нет.";
/// Кнопка выпуска токена для Home Assistant.
pub const ISSUE_HA_TOKEN_BUTTON: &str = "➕ Выпустить токен для HA";
/// Имя токена, выпускаемого в боте.
pub const HA_TOKEN_NAME: &str = "Home Assistant";
/// Предупреждение о сроке жизни сообщения с токеном.
pub const SECRET_MESSAGE_EXPIRES: &str = "Сообщение удалится через час";
/// Кнопка подтверждения, что токен сохранён.
pub const SECRET_SAVED_BUTTON: &str = "Я сохранил";
/// Ответ на кнопку «Я сохранил».
pub const SECRET_MESSAGE_DELETED: &str = "Сообщение удалено.";
/// Токен ещё не использовался.
pub const TOKEN_NEVER_USED: &str = "не использовался";
/// Пояснение на экране подтверждения отзыва токена.
pub const TOKEN_STOPS_WORKING: &str = "Интеграция, которая им пользуется, перестанет работать.";
/// Баннер после успешного отзыва токена.
pub const TOKEN_REVOKED: &str = "Токен отозван.";
/// Баннер, если токен уже был отозван раньше.
pub const TOKEN_ALREADY_REVOKED: &str = "Токен уже отозван.";

/// Строка «последнее использование» для списка токенов.
pub fn token_last_used(
    last_used_at: Option<DateTime<Utc>>,
    timezone: Tz,
    now: DateTime<Utc>,
) -> String {
    match last_used_at {
        Some(at) => {
            let local = at.with_timezone(&timezone);
            let today = now.with_timezone(&timezone).date_naive();
            format!(
                "Последнее использование: {}, {}",
                date_label(local.date_naive(), today),
                local.format("%H:%M")
            )
        }
        None => TOKEN_NEVER_USED.to_owned(),
    }
}

/// Кнопка отзыва токена в списке.
pub fn token_revoke_button(name: &str, prefix: &str) -> String {
    format!("🚫 Отозвать «{name} {prefix}»")
}

/// Заголовок подтверждения отзыва токена.
pub fn confirm_revoke_token(name: &str, prefix: &str) -> String {
    format!("Отозвать токен «{name}» ({prefix})?")
}

/// Количество с единицей: «2 л», «1,5 кг».
pub fn quantity_label(quantity: f64, unit: Option<&str>) -> String {
    let mut number = format!("{quantity:.3}");
    while number.ends_with('0') {
        number.pop();
    }
    if number.ends_with('.') {
        number.pop();
    }
    let number = number.replace('.', ",");
    match unit.map(str::trim).filter(|unit| !unit.is_empty()) {
        Some(unit) => format!("{number} {unit}"),
        None => number,
    }
}

/// Позиция списка: название и количество через точку.
pub fn position_label(title: &str, quantity: Option<f64>, unit: Option<&str>) -> String {
    let mut label = title.to_owned();
    if let Some(quantity) = quantity {
        label.push_str(&format!(" · {}", quantity_label(quantity, unit)));
    }
    label
}

/// Результат разбора срока, введённого пользователем.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParsedDue {
    /// Дата; пусто, если введено только время.
    pub date: Option<NaiveDate>,
    /// Время; пусто для срока «на весь день».
    pub time: Option<NaiveTime>,
}

/// Подсказка с форматом срока.
pub const DUE_HINT: &str =
    "Примеры: «завтра», «чт 12:00», «25.12», «25.12.2026», «25.12 18:30» или «18:30».";

/// Разобрать срок: «сегодня», «завтра», «послезавтра», `ДД.ММ[.ГГГГ]`,
/// `ЧЧ:ММ` и их сочетания. `None` — формат не распознан.
pub fn parse_due(input: &str, today: NaiveDate) -> Option<ParsedDue> {
    let lowered = input.trim().to_lowercase();
    if lowered.is_empty() {
        return None;
    }

    let mut date = None;
    let mut time = None;
    for token in lowered.split_whitespace() {
        match token {
            "сегодня" => date = Some(today),
            "завтра" => date = Some(today + chrono::Duration::days(1)),
            "послезавтра" => date = Some(today + chrono::Duration::days(2)),
            _ if token.contains(':') => time = Some(parse_time(token)?),
            _ => date = Some(parse_date(token, today)?),
        }
    }
    if date.is_none() && time.is_none() {
        return None;
    }
    Some(ParsedDue { date, time })
}

fn parse_time(token: &str) -> Option<NaiveTime> {
    let (hours, minutes) = token.split_once(':')?;
    if hours.len() > 2 || minutes.len() != 2 {
        return None;
    }
    NaiveTime::from_hms_opt(hours.parse().ok()?, minutes.parse().ok()?, 0)
}

fn parse_date(token: &str, today: NaiveDate) -> Option<NaiveDate> {
    if let Some(weekday) = weekday_from_short(token) {
        let current = today.weekday().number_from_monday();
        let delta = (weekday + 7 - current) % 7;
        return Some(today + chrono::Duration::days(i64::from(delta)));
    }
    let parts: Vec<&str> = token.split(['.', '/', '-']).collect();
    match parts.as_slice() {
        [day, month] => {
            NaiveDate::from_ymd_opt(today.year(), month.parse().ok()?, day.parse().ok()?)
        }
        [day, month, year] => {
            let mut year: i32 = year.parse().ok()?;
            if year < 100 {
                year += 2000;
            }
            NaiveDate::from_ymd_opt(year, month.parse().ok()?, day.parse().ok()?)
        }
        _ => None,
    }
}

/// ISO-номер дня недели по короткому русскому названию.
fn weekday_from_short(token: &str) -> Option<u32> {
    match token {
        "пн" => Some(1),
        "вт" => Some(2),
        "ср" => Some(3),
        "чт" => Some(4),
        "пт" => Some(5),
        "сб" => Some(6),
        "вс" => Some(7),
        _ => None,
    }
}

/// Разобрать положительное число с точкой или запятой.
pub fn parse_number(input: &str) -> Option<f64> {
    let value: f64 = input.trim().replace(',', ".").parse().ok()?;
    (value.is_finite() && value > 0.0).then_some(value)
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).expect("date")
    }

    #[test]
    fn plural_forms_follow_russian_rules() {
        let forms: Vec<String> = [0, 1, 2, 4, 5, 11, 12, 14, 21, 22, 25, 101, 111]
            .iter()
            .map(|n| tasks(*n))
            .collect();
        assert_eq!(
            forms,
            [
                "0 задач",
                "1 задача",
                "2 задачи",
                "4 задачи",
                "5 задач",
                "11 задач",
                "12 задач",
                "14 задач",
                "21 задача",
                "22 задачи",
                "25 задач",
                "101 задача",
                "111 задач"
            ]
        );
        assert_eq!(purchases(1), "1 покупка");
        assert_eq!(purchases(3), "3 покупки");
        assert_eq!(purchases(7), "7 покупок");
        assert_eq!(positions(2), "2 позиции");
    }

    #[test]
    fn dates_are_relative_then_absolute() {
        let today = date(2026, 10, 30);
        assert_eq!(date_label(date(2026, 10, 29), today), "вчера");
        assert_eq!(date_label(today, today), "сегодня");
        assert_eq!(date_label(date(2026, 10, 31), today), "завтра");
        assert_eq!(date_label(date(2026, 11, 1), today), "послезавтра");
        assert_eq!(date_label(date(2026, 11, 3), today), "вт, 3 ноя");
        assert_eq!(date_label(date(2027, 1, 5), today), "5 янв 2027");
    }

    #[test]
    fn due_label_uses_local_time() {
        let tz: Tz = "Europe/Moscow".parse().expect("tz");
        let now = Utc.with_ymd_and_hms(2026, 10, 30, 9, 0, 0).unwrap();
        let due = Utc.with_ymd_and_hms(2026, 10, 31, 15, 0, 0).unwrap();

        assert_eq!(
            due_label(Some(due), None, tz, now).as_deref(),
            Some("завтра, 18:00")
        );
        assert_eq!(
            due_label(None, Some(date(2026, 10, 30)), tz, now).as_deref(),
            Some("сегодня")
        );
        assert_eq!(due_label(None, None, tz, now), None);
    }

    #[test]
    fn due_input_accepts_documented_formats() {
        let today = date(2026, 10, 30);
        let time = NaiveTime::from_hms_opt(18, 30, 0);

        assert_eq!(
            parse_due("завтра", today),
            Some(ParsedDue {
                date: Some(date(2026, 10, 31)),
                time: None
            })
        );
        assert_eq!(
            parse_due("25.12", today),
            Some(ParsedDue {
                date: Some(date(2026, 12, 25)),
                time: None
            })
        );
        assert_eq!(
            parse_due("25.12.2027 18:30", today),
            Some(ParsedDue {
                date: Some(date(2027, 12, 25)),
                time
            })
        );
        assert_eq!(
            parse_due("18:30", today),
            Some(ParsedDue { date: None, time })
        );
        assert_eq!(
            parse_due("чт 12:00", today),
            Some(ParsedDue {
                date: Some(date(2026, 11, 5)),
                time: NaiveTime::from_hms_opt(12, 0, 0),
            }),
            "a short weekday picks the next such day, including today"
        );
        assert_eq!(parse_due("32.13", today), None);
        assert_eq!(parse_due("когда-нибудь", today), None);
        assert_eq!(parse_due("25:99", today), None);
        assert_eq!(parse_due("", today), None);
    }

    #[test]
    fn quantities_use_decimal_comma() {
        assert_eq!(quantity_label(2.0, Some("л")), "2 л");
        assert_eq!(quantity_label(1.5, Some("кг")), "1,5 кг");
        assert_eq!(quantity_label(10.0, None), "10");
        assert_eq!(parse_number("1,5"), Some(1.5));
        assert_eq!(parse_number("0"), None);
        assert_eq!(parse_number("abc"), None);
    }
}
