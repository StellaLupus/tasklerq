//! Поиск с курсорной пагинацией.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgConnection};
use uuid::Uuid;

use super::row::{ITEM_COLUMNS, ItemRow, hydrate_many};
use crate::domain::error::DomainError;
use crate::domain::models::{Item, ItemStatus, ListKind};
use crate::domain::search::{SearchCursor, SearchFilters};

/// Результат поиска.
#[derive(Debug, Clone)]
pub struct SearchHit {
    /// Элемент.
    pub item: Item,
    /// Название списка.
    pub list_name: String,
    /// Тип списка.
    pub list_kind: ListKind,
    /// Релевантность.
    pub rank: f64,
}

#[derive(FromRow)]
struct SearchRow {
    id: Uuid,
    rank: f64,
    sort_due: DateTime<Utc>,
    list_name: String,
    list_kind: ListKind,
}

/// Проекция и ранжирование: общая часть обоих вариантов запроса.
///
/// `$15` — сырой текст запроса; при пустом запросе он биндится как `NULL`, и
/// `plainto_tsquery`/бонус за название дают нулевую релевантность.
const SEARCH_PROJECTION: &str = "
        SELECT id, rank, sort_due, list_name, list_kind
        FROM (
            SELECT i.id AS id,
                   (
                       COALESCE(ts_rank(i.search_vector, plainto_tsquery('simple', $15)), 0)
                       + CASE WHEN $15::text IS NOT NULL
                                   AND strpos(lower(i.title), lower($15)) > 0
                              THEN 1.0 ELSE 0.0 END
                   )::float8 AS rank,
                   COALESCE(
                       i.due_at,
                       (i.due_date::timestamp AT TIME ZONE $13),
                       '9999-12-31 00:00:00+00'::timestamptz
                   ) AS sort_due,
                   l.name AS list_name,
                   l.kind AS list_kind
            FROM items i
            JOIN lists l ON l.id = i.list_id";

/// Фильтры, общие для обоих вариантов.
///
/// `$6` — `mine_for`: свои и общие задачи только в списках вида `tasks`.
/// У покупок и событий исполнителя обычно нет, и без условия на вид списка
/// они заполнили бы лимит страницы «Моих задач» и вытеснили задачи.
fn search_filters() -> String {
    format!(
        "
            WHERE i.household_id = $1
              AND i.deleted_at IS NULL
              AND l.deleted_at IS NULL
              AND NOT l.is_archived
              AND ($2::bool OR i.status = $3)
              AND ($4::uuid IS NULL OR i.list_id = $4)
              AND ($5::uuid IS NULL OR i.assignee_user_id = $5)
              AND ($6::uuid IS NULL OR (l.kind = 'tasks' AND {}))
              AND ($7::text IS NULL OR EXISTS (
                      SELECT 1 FROM item_tags it
                      JOIN tags t ON t.id = it.tag_id
                      WHERE it.item_id = i.id AND lower(t.name) = lower($7)
                  ))
              AND ($8::timestamptz IS NULL OR
                   COALESCE(i.due_at, (i.due_date::timestamp AT TIME ZONE $13)) >= $8)
              AND ($9::timestamptz IS NULL OR
                   COALESCE(i.due_at, (i.due_date::timestamp AT TIME ZONE $13)) < $9)",
        super::mine_predicate("$6")
    )
}

/// Условие курсора, порядок и размер страницы.
const SEARCH_TAIL: &str = "
        ) matched
        WHERE ($10::float8 IS NULL OR rank < $10 OR (rank = $10 AND (sort_due > $11 OR (sort_due = $11 AND id > $12))))
        ORDER BY rank DESC, sort_due ASC, id ASC
        LIMIT $14";

/// CTE кандидатов: строки отбираются по trigram-индексам и `search_vector`,
/// а не полным проходом по задачам household.
const SEARCH_CANDIDATES_CTE: &str = "
        WITH candidates AS (
            SELECT i.id AS id
            FROM items i
            WHERE i.household_id = $1
              AND (i.title ILIKE $16
                   OR i.notes ILIKE $16
                   OR i.search_vector @@ plainto_tsquery('simple', $15))
            UNION
            SELECT s.item_id AS id
            FROM subtasks s
            JOIN items i ON i.id = s.item_id
            WHERE i.household_id = $1
              AND s.title ILIKE $16
            UNION
            SELECT it.item_id AS id
            FROM tags t
            JOIN item_tags it ON it.tag_id = t.id
            WHERE t.household_id = $1
              AND t.name ILIKE $16
        )
        ";

/// Шаблон `LIKE`/`ILIKE` для поиска подстроки: экранирует метасимволы
/// (сначала `\`, затем `%` и `_`) и оборачивает текст в `%…%`.
fn like_pattern(query: &str) -> String {
    let mut pattern = String::with_capacity(query.len() + 2);
    pattern.push('%');
    for character in query.chars() {
        if matches!(character, '\\' | '%' | '_') {
            pattern.push('\\');
        }
        pattern.push(character);
    }
    pattern.push('%');
    pattern
}

/// Поиск с курсорной пагинацией.
pub async fn search(
    conn: &mut PgConnection,
    household_id: Uuid,
    household_timezone: &str,
    filters: &SearchFilters,
) -> Result<(Vec<SearchHit>, Option<String>), DomainError> {
    filters.validate()?;

    let (cursor_rank, cursor_due, cursor_id) = match &filters.cursor {
        Some(cursor) => (Some(cursor.rank), Some(cursor.due), Some(cursor.id)),
        None => (None, None, None),
    };
    let query = filters
        .q
        .as_deref()
        .map(str::trim)
        .filter(|q| !q.is_empty());
    let tag = filters
        .tag
        .as_deref()
        .map(str::trim)
        .filter(|tag| !tag.is_empty());

    // Два текста: с CTE кандидатов (запрос непустой) и без него. Общие части
    // собраны в константах, различается только источник строк.
    let search_filters = search_filters();
    let sql = match query {
        Some(_) => format!(
            "{SEARCH_CANDIDATES_CTE}{SEARCH_PROJECTION}
            JOIN candidates c ON c.id = i.id{search_filters}{SEARCH_TAIL}"
        ),
        None => format!("{SEARCH_PROJECTION}{search_filters}{SEARCH_TAIL}"),
    };

    let mut request = sqlx::query_as::<_, SearchRow>(&sql)
        .bind(household_id)
        .bind(filters.all_status)
        .bind(filters.status.unwrap_or(ItemStatus::Open))
        .bind(filters.list_id)
        .bind(filters.assignee_user_id)
        .bind(filters.mine_for)
        .bind(tag)
        .bind(filters.due_from)
        .bind(filters.due_to)
        .bind(cursor_rank)
        .bind(cursor_due)
        .bind(cursor_id)
        .bind(household_timezone)
        .bind(filters.limit);
    request = match query {
        Some(query) => request.bind(query).bind(like_pattern(query)),
        // В SQL без текстового условия `$15` остаётся в ранжировании.
        None => request.bind(None::<&str>),
    };

    let rows = request.fetch_all(&mut *conn).await?;

    let ids: Vec<Uuid> = rows.iter().map(|row| row.id).collect();
    let item_rows = sqlx::query_as::<_, ItemRow>(&format!(
        "SELECT {ITEM_COLUMNS} FROM items i
         WHERE i.household_id = $1 AND i.id = ANY($2) AND i.deleted_at IS NULL"
    ))
    .bind(household_id)
    .bind(&ids)
    .fetch_all(&mut *conn)
    .await?;
    let mut found: HashMap<Uuid, Item> = hydrate_many(conn, item_rows)
        .await?
        .into_iter()
        .map(|item| (item.id, item))
        .collect();

    let mut hits = Vec::with_capacity(rows.len());
    for row in &rows {
        // Элемент мог быть удалён между выборкой страницы и загрузкой.
        let Some(item) = found.remove(&row.id) else {
            continue;
        };
        hits.push(SearchHit {
            item,
            list_name: row.list_name.clone(),
            list_kind: row.list_kind,
            rank: row.rank,
        });
    }

    let next_cursor = if i64::try_from(rows.len()).unwrap_or(0) >= filters.limit {
        rows.last()
            .map(|row| {
                crate::domain::search::encode_cursor(&SearchCursor {
                    rank: row.rank,
                    due: row.sort_due,
                    id: row.id,
                })
            })
            .transpose()?
    } else {
        None
    };

    Ok((hits, next_cursor))
}

#[cfg(test)]
mod tests {
    use super::like_pattern;

    #[test]
    fn like_pattern_wraps_plain_text() {
        assert_eq!(like_pattern("молоко"), "%молоко%");
    }

    #[test]
    fn like_pattern_escapes_backslash_percent_and_underscore() {
        // `a\%_b`: `\` → `\\`, `%` → `\%`, `_` → `\_`.
        assert_eq!(like_pattern(r"a\%_b"), r"%a\\\%\_b%");
        assert_eq!(like_pattern(r"C:\tmp"), r"%C:\\tmp%");
    }
}
