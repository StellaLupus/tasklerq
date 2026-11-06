//! Выполнения элемента, собранные из событий `completed`.
//!
//! Выполнение — это событие `completed`, которое никто не отменил. Правило
//! отбора используется разделом «Выполнено сегодня» и счётчиком меню, поэтому
//! живёт здесь, а не в экранах.

use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use chrono_tz::Tz;
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::domain::error::DomainError;

/// Название выполнения, если снимок `payload.before` его не сохранил.
pub const UNTITLED: &str = "Без названия";

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

/// Начало текущего дня в часовом поясе пользователя.
///
/// В некоторых поясах местной полуночи не существует из-за перехода на летнее
/// время: тогда берётся первый существующий местный час.
pub fn day_start(timezone: Tz, now: DateTime<Utc>) -> DateTime<Utc> {
    let date = now.with_timezone(&timezone).date_naive();
    (0..24)
        .find_map(|hour| {
            date.and_hms_opt(hour, 0, 0)
                .and_then(|local| timezone.from_local_datetime(&local).earliest())
        })
        .map(|local| local.with_timezone(&Utc))
        .expect("a local date has at least one existing hour")
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
    let rows = sqlx::query_as::<_, CompletionRow>(
        "SELECT e.id, e.item_id, e.actor_user_id, e.created_at, e.payload,
                i.list_id AS current_list_id
         FROM item_events e
         JOIN items i ON i.id = e.item_id AND i.household_id = e.household_id
         WHERE e.household_id = $1
           AND e.event_type = 'completed'
           AND e.undo_of IS NULL
           AND e.created_at >= $2
           AND i.deleted_at IS NULL
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
           )
         ORDER BY e.created_at DESC, e.id DESC",
    )
    .bind(household_id)
    .bind(since)
    .fetch_all(conn)
    .await?;

    Ok(rows.into_iter().map(Completion::from).collect())
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

        Self {
            event_id: row.id,
            item_id: row.item_id,
            list_id,
            title,
            due_at,
            due_date,
            completed_at: row.created_at,
            actor_user_id: row.actor_user_id,
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
