//! Выполнения элемента, собранные из событий `completed`.
//!
//! Выполнение — это событие `completed`, которое никто не отменил. Правило
//! отбора используется разделом «Выполнено сегодня» и счётчиком меню, поэтому
//! живёт здесь, а не в экранах.

use chrono::{DateTime, NaiveDate, Utc};
use chrono_tz::Tz;
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::events::start_of_day;
use crate::domain::models::{Item, ItemStatus};

/// Название выполнения, если снимок `payload.before` его не сохранил.
pub const UNTITLED: &str = "Без названия";

/// Размер страницы журнала выполнений.
pub const JOURNAL_PAGE_SIZE: usize = 8;

/// Общее условие отбора выполнения для журнала и раздела «Выполнено сегодня».
///
/// Подставляется как фрагмент `WHERE` с `$1` — household. Список
/// `item_events` уже алиасирован как `e`, элемент — как `i`. События не имеют
/// состояния «выполнено» (0009-events, design решение 1), поэтому строка
/// события не попадает в выборки выполненных даже при устаревшей или битой
/// записи журнала.
const COMPLETION_SELECTION: &str = "\
    e.household_id = $1
    AND e.event_type = 'completed'
    AND e.undo_of IS NULL
    AND i.deleted_at IS NULL
    AND NOT EXISTS (
        SELECT 1 FROM lists el WHERE el.id = i.list_id AND el.kind = 'events'
    )
    AND NOT EXISTS (
        SELECT 1 FROM item_events undo WHERE undo.undo_of = e.id
    )
    AND NOT EXISTS (
        SELECT 1
        FROM item_events r
        WHERE r.household_id = e.household_id
          AND r.item_id = e.item_id
          AND r.event_type = 'reopened'
          AND r.undo_of IS NULL
          AND (r.created_at, r.id) > (e.created_at, e.id)
          AND NOT EXISTS (
              SELECT 1 FROM item_events u WHERE u.undo_of = r.id
          )
          AND NOT EXISTS (
              SELECT 1
              FROM item_events c
              WHERE c.household_id = e.household_id
                AND c.item_id = e.item_id
                AND c.event_type = 'completed'
                AND c.undo_of IS NULL
                AND (c.created_at, c.id) > (e.created_at, e.id)
                AND (c.created_at, c.id) < (r.created_at, r.id)
          )
    )";

/// Запись выполнения.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    /// Идентификатор события `completed`.
    pub event_id: Uuid,
    /// Элемент.
    pub item_id: Uuid,
    /// Список на момент выполнения.
    pub list_id: Uuid,
    /// Название на момент выполнения.
    pub title: String,
    /// Плановый срок со временем.
    pub due_at: Option<DateTime<Utc>>,
    /// Плановый срок без времени.
    pub due_date: Option<NaiveDate>,
    /// Фактическое время выполнения.
    pub completed_at: DateTime<Utc>,
    /// Кто выполнил.
    pub actor_user_id: Option<Uuid>,
    /// Версия элемента сразу после выполнения.
    pub version_after: Option<i32>,
}

/// Выполнение вместе с сырым снимком события.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionEvent {
    /// Запись выполнения.
    pub completion: Completion,
    /// Снимок события `completed`: `before` и `after`.
    pub payload: Value,
}

/// Запись журнала: выполнение и вычисленный плановый день.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalEntry {
    /// Выполнение.
    pub completion: Completion,
    /// Плановый день срока в поясе пользователя; `None` — «Без срока».
    pub planned_day: Option<NaiveDate>,
}

/// Страница журнала выполнений household.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalPage {
    /// Записи страницы.
    pub entries: Vec<JournalEntry>,
    /// Номер страницы с нуля после ограничения последней.
    pub page: u32,
    /// Всего страниц, минимум одна.
    pub pages: u32,
}

/// Доступен ли возврат выполнения в текущем состоянии элемента.
///
/// У выполненной задачи без повторения возврат — прежний `reopen`. В
/// остальных случаях, в том числе когда повторение сняли после выполнения и
/// задача снова открыта, элемент не должен меняться после выполнения: иначе
/// восстановленный из снимка срок затёр бы чужую правку.
pub fn revertible(item: &Item, completion: Option<&Completion>) -> bool {
    if item.status == ItemStatus::Done && item.recurrence.is_none() {
        return true;
    }
    completion.is_some_and(|completion| completion.version_after == Some(item.version))
}

/// Сырая строка выборки: снимок остаётся `jsonb`, чтобы разбирать поля
/// по отдельности и терпимо.
#[derive(sqlx::FromRow)]
struct CompletionRow {
    id: Uuid,
    item_id: Uuid,
    actor_user_id: Option<Uuid>,
    created_at: DateTime<Utc>,
    payload: Value,
    current_list_id: Uuid,
}

/// Сырая строка журнала: выполнение и плановый день из SQL.
#[derive(sqlx::FromRow)]
struct JournalRow {
    id: Uuid,
    item_id: Uuid,
    actor_user_id: Option<Uuid>,
    created_at: DateTime<Utc>,
    payload: Value,
    current_list_id: Uuid,
    planned_day: Option<NaiveDate>,
    total: i64,
}

/// Начало текущего дня в часовом поясе пользователя.
///
/// В некоторых поясах местной полуночи не существует из-за перехода на летнее
/// время: тогда берётся первый существующий местный час.
pub fn day_start(timezone: Tz, now: DateTime<Utc>) -> DateTime<Utc> {
    let date = now.with_timezone(&timezone).date_naive();
    start_of_day(timezone, date)
}

/// Выполнения household с `since`, начиная с поздних, по одному списку.
///
/// Выполнение — это только событие `completed`, которое само не является
/// компенсирующим: `undo_last` тоже пишет `completed`, когда возвращает
/// удаление, правку или возврат в работу выполненной задачи. Такие события
/// имеют `undo_of` и не создают нового выполнения.
///
/// Индекс `(household_id, created_at)` отсекает окно, а фильтр по списку идёт
/// в коде: `list_id` берётся из снимка выполнения, а у старых или битых
/// снимков — из текущего элемента.
pub async fn for_list_since(
    conn: &mut PgConnection,
    household_id: Uuid,
    list_id: Uuid,
    since: DateTime<Utc>,
) -> Result<Vec<Completion>, DomainError> {
    Ok(for_household_since(conn, household_id, since)
        .await?
        .into_iter()
        .filter(|completion| completion.list_id == list_id)
        .collect())
}

/// Выполнения household с `since` по всем спискам, начиная с поздних.
///
/// Правило отбора то же, что у [`for_list_since`]; фильтр по списку остаётся
/// вызывающему, потому что `list_id` берётся из снимка выполнения.
pub async fn for_household_since(
    conn: &mut PgConnection,
    household_id: Uuid,
    since: DateTime<Utc>,
) -> Result<Vec<Completion>, DomainError> {
    let sql = format!(
        "SELECT e.id, e.item_id, e.actor_user_id, e.created_at, e.payload,
                i.list_id AS current_list_id
         FROM item_events e
         JOIN items i ON i.id = e.item_id AND i.household_id = e.household_id
         WHERE {COMPLETION_SELECTION}
           AND e.created_at >= $2
         ORDER BY e.created_at DESC, e.id DESC"
    );
    let rows = sqlx::query_as::<_, CompletionRow>(&sql)
        .bind(household_id)
        .bind(since)
        .fetch_all(conn)
        .await?;

    Ok(rows.into_iter().map(Completion::from).collect())
}

/// Последние выполнения элемента, начиная с поздних.
///
/// Правило отбора то же, что у журнала и раздела «Выполнено сегодня». Индекс
/// `(item_id, created_at)` отсекает хвост.
pub async fn recent_for_item(
    conn: &mut PgConnection,
    household_id: Uuid,
    item_id: Uuid,
    limit: usize,
) -> Result<Vec<Completion>, DomainError> {
    Ok(selected_for_item(conn, household_id, item_id, limit)
        .await?
        .into_iter()
        .map(|event| event.completion)
        .collect())
}

/// Последнее неотменённое выполнение элемента вместе со снимком события.
///
/// Нужно возврату выполнения: проверить, что оно последнее, и восстановить
/// состояние из `payload.before`.
pub async fn latest_for_item(
    conn: &mut PgConnection,
    household_id: Uuid,
    item_id: Uuid,
) -> Result<Option<CompletionEvent>, DomainError> {
    Ok(selected_for_item(conn, household_id, item_id, 1)
        .await?
        .into_iter()
        .next())
}

/// Выполнения элемента, начиная с поздних, не больше `limit` штук.
///
/// Одна выборка для блока «Выполнения» и возврата последнего выполнения:
/// различается только `LIMIT`.
async fn selected_for_item(
    conn: &mut PgConnection,
    household_id: Uuid,
    item_id: Uuid,
    limit: usize,
) -> Result<Vec<CompletionEvent>, DomainError> {
    let sql = format!(
        "SELECT e.id, e.item_id, e.actor_user_id, e.created_at, e.payload,
                i.list_id AS current_list_id
         FROM item_events e
         JOIN items i ON i.id = e.item_id AND i.household_id = e.household_id
         WHERE {COMPLETION_SELECTION}
           AND e.item_id = $2
         ORDER BY e.created_at DESC, e.id DESC
         LIMIT $3"
    );
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    let rows = sqlx::query_as::<_, CompletionRow>(&sql)
        .bind(household_id)
        .bind(item_id)
        .bind(limit)
        .fetch_all(conn)
        .await?;

    Ok(rows.into_iter().map(CompletionEvent::from).collect())
}

/// Страница журнала выполнений household.
///
/// Журнал ведётся только по задачам: покупки пишут то же событие `completed`,
/// но их текущий список — не список задач и в журнал не попадает.
///
/// Глубину журнала задаёт срок хранения событий: нижней границы по времени
/// нет. Плановый день считается в SQL из снимка `before`: `due_at` в поясе
/// пользователя, иначе `due_date`. Битая дата не роняет запрос — запись уходит
/// в «Без срока».
///
/// Порядок: плановый день от поздних к ранним, внутри дня факт выполнения по
/// убыванию и `id`, затем все записи без срока по тому же правилу.
pub async fn journal_page(
    conn: &mut PgConnection,
    household_id: Uuid,
    timezone: Tz,
    page: u32,
) -> Result<JournalPage, DomainError> {
    // Один запрос и один снимок: `count(*) OVER ()` считает все отобранные
    // записи, а OFFSET ограничивается последней страницей прямо в SQL. Иначе
    // между отдельными count и страницей новое выполнение развело бы `pages`
    // и содержимое.
    let sql = format!(
        "WITH selected AS (
             SELECT e.id, e.item_id, e.actor_user_id, e.created_at, e.payload,
                    i.list_id AS current_list_id,
                    count(*) OVER () AS total,
                    CASE
                        WHEN btrim(lower(e.payload->'before'->>'due_at'))
                                 NOT IN ('infinity', '-infinity')
                         AND pg_input_is_valid(e.payload->'before'->>'due_at', 'timestamptz')
                            THEN ((e.payload->'before'->>'due_at')::timestamptz AT TIME ZONE $2)::date
                        WHEN btrim(lower(e.payload->'before'->>'due_date'))
                                 NOT IN ('infinity', '-infinity')
                         AND pg_input_is_valid(e.payload->'before'->>'due_date', 'date')
                            THEN (e.payload->'before'->>'due_date')::date
                    END AS planned_day
             FROM item_events e
             JOIN items i ON i.id = e.item_id AND i.household_id = e.household_id
             JOIN lists l ON l.id = i.list_id AND l.household_id = i.household_id
             WHERE {COMPLETION_SELECTION}
               AND l.kind = 'tasks'
         ),
         paged AS (
             SELECT *
             FROM selected
             ORDER BY planned_day DESC NULLS LAST, created_at DESC, id DESC
             LIMIT $3
             OFFSET LEAST(
                 $4,
                 GREATEST((SELECT total FROM selected LIMIT 1) - 1, 0) / $3 * $3
             )
         )
         SELECT * FROM paged"
    );
    let rows = sqlx::query_as::<_, JournalRow>(&sql)
        .bind(household_id)
        .bind(timezone.name())
        .bind(i64::try_from(JOURNAL_PAGE_SIZE).unwrap_or(i64::MAX))
        .bind(
            i64::try_from(usize::try_from(page).unwrap_or(usize::MAX) * JOURNAL_PAGE_SIZE)
                .unwrap_or(i64::MAX),
        )
        .fetch_all(&mut *conn)
        .await?;

    let total = rows.first().map_or(0, |row| row.total);
    let total = usize::try_from(total).unwrap_or(usize::MAX);
    let pages = total.div_ceil(JOURNAL_PAGE_SIZE).max(1);
    let page = usize::try_from(page).unwrap_or(0).min(pages - 1);

    Ok(JournalPage {
        entries: rows.into_iter().map(JournalEntry::from).collect(),
        page: u32::try_from(page).unwrap_or(0),
        pages: u32::try_from(pages).unwrap_or(u32::MAX),
    })
}

impl From<CompletionRow> for Completion {
    fn from(row: CompletionRow) -> Self {
        let before = row.payload.get("before");
        let title = before
            .and_then(|before| before.get("title"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|title| !title.is_empty())
            .map_or_else(|| UNTITLED.to_owned(), str::to_owned);
        let due_at = field(before, "due_at");
        let due_date = field(before, "due_date");
        let list_id = field(before, "list_id").unwrap_or(row.current_list_id);
        let version_after = row
            .payload
            .get("after")
            .and_then(|after| after.get("version"))
            .and_then(Value::as_i64)
            .and_then(|version| i32::try_from(version).ok());

        Self {
            event_id: row.id,
            item_id: row.item_id,
            list_id,
            title,
            due_at,
            due_date,
            completed_at: row.created_at,
            actor_user_id: row.actor_user_id,
            version_after,
        }
    }
}

impl From<CompletionRow> for CompletionEvent {
    fn from(row: CompletionRow) -> Self {
        let payload = row.payload.clone();
        Self {
            completion: Completion::from(row),
            payload,
        }
    }
}

impl From<JournalRow> for JournalEntry {
    fn from(row: JournalRow) -> Self {
        let planned_day = row.planned_day;
        let completion = Completion::from(CompletionRow {
            id: row.id,
            item_id: row.item_id,
            actor_user_id: row.actor_user_id,
            created_at: row.created_at,
            payload: row.payload,
            current_list_id: row.current_list_id,
        });
        Self {
            completion,
            planned_day,
        }
    }
}

/// Разобрать одно поле снимка, не роняя запись из-за остальных.
fn field<T: serde::de::DeserializeOwned>(before: Option<&Value>, key: &str) -> Option<T> {
    before
        .and_then(|before| before.get(key))
        .and_then(|value| serde_json::from_value(value.clone()).ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn day_start_is_local_midnight() {
        let tz: Tz = "Asia/Vladivostok".parse().expect("timezone");
        let now = Utc.with_ymd_and_hms(2026, 3, 15, 15, 0, 0).unwrap();
        let expected = tz
            .with_ymd_and_hms(2026, 3, 16, 0, 0, 0)
            .single()
            .expect("local midnight")
            .with_timezone(&Utc);
        assert_eq!(day_start(tz, now), expected);
    }

    #[test]
    fn day_start_falls_back_when_midnight_does_not_exist() {
        let tz: Tz = "America/Santiago".parse().expect("timezone");
        let now = Utc.with_ymd_and_hms(2026, 9, 6, 12, 0, 0).unwrap();
        assert!(
            tz.with_ymd_and_hms(2026, 9, 6, 0, 0, 0).single().is_none(),
            "midnight is skipped by the DST transition"
        );
        let expected = tz
            .with_ymd_and_hms(2026, 9, 6, 1, 0, 0)
            .single()
            .expect("first existing local hour")
            .with_timezone(&Utc);
        assert_eq!(day_start(tz, now), expected);
    }
}
