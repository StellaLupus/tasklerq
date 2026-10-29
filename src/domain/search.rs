//! Поиск: фильтры и курсорная пагинация.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::error::DomainError;
use super::models::ItemStatus;

/// Размер страницы по умолчанию.
pub const DEFAULT_LIMIT: i64 = 20;
/// Максимальный размер страницы.
pub const MAX_LIMIT: i64 = 50;

/// Фильтры поиска.
#[derive(Debug, Clone)]
pub struct SearchFilters {
    /// Текст для поиска.
    pub q: Option<String>,
    /// Статус; `None` означает «только открытые», если не задан `all_status`.
    pub status: Option<ItemStatus>,
    /// Не фильтровать по статусу.
    pub all_status: bool,
    /// Список.
    pub list_id: Option<Uuid>,
    /// Исполнитель.
    pub assignee_user_id: Option<Uuid>,
    /// Тег.
    pub tag: Option<String>,
    /// Начало диапазона сроков.
    pub due_from: Option<DateTime<Utc>>,
    /// Конец диапазона сроков.
    pub due_to: Option<DateTime<Utc>>,
    /// Размер страницы.
    pub limit: i64,
    /// Курсор.
    pub cursor: Option<SearchCursor>,
}

impl Default for SearchFilters {
    fn default() -> Self {
        Self {
            q: None,
            status: Some(ItemStatus::Open),
            all_status: false,
            list_id: None,
            assignee_user_id: None,
            tag: None,
            due_from: None,
            due_to: None,
            limit: DEFAULT_LIMIT,
            cursor: None,
        }
    }
}

impl SearchFilters {
    /// Проверить размер страницы.
    pub fn validate(&self) -> Result<(), DomainError> {
        if !(1..=MAX_LIMIT).contains(&self.limit) {
            return Err(DomainError::Validation(format!(
                "limit must be between 1 and {MAX_LIMIT}"
            )));
        }
        Ok(())
    }
}

/// Курсор поиска: релевантность, срок и идентификатор.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchCursor {
    /// Релевантность.
    pub rank: f64,
    /// Срок; для элементов без срока — [`null_due_sentinel`].
    pub due: DateTime<Utc>,
    /// Идентификатор.
    pub id: Uuid,
}

/// Значение сортировки для элементов без срока.
pub fn null_due_sentinel() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(9999, 12, 31, 0, 0, 0)
        .single()
        .expect("valid sentinel")
}

/// Закодировать курсор в base64url.
pub fn encode_cursor(cursor: &SearchCursor) -> Result<String, DomainError> {
    let json = serde_json::to_vec(cursor)
        .map_err(|error| DomainError::Validation(format!("invalid cursor: {error}")))?;
    Ok(URL_SAFE_NO_PAD.encode(json))
}

/// Декодировать курсор из base64url.
pub fn decode_cursor(value: &str) -> Result<SearchCursor, DomainError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| DomainError::Validation("invalid search cursor".into()))?;
    serde_json::from_slice(&bytes)
        .map_err(|_| DomainError::Validation("invalid search cursor".into()))
}

/// Разобрать диапазон дат для фильтра сроков.
pub fn parse_due_bound(
    value: Option<&str>,
    end_of_day: bool,
) -> Result<Option<DateTime<Utc>>, DomainError> {
    let Some(value) = value else {
        return Ok(None);
    };

    if let Ok(at) = DateTime::parse_from_rfc3339(value) {
        return Ok(Some(at.with_timezone(&Utc)));
    }

    let date = NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .map_err(|_| DomainError::Validation("due bound must be RFC 3339 or YYYY-MM-DD".into()))?;
    let time = if end_of_day {
        chrono::NaiveTime::from_hms_opt(23, 59, 59).expect("valid time")
    } else {
        chrono::NaiveTime::from_hms_opt(0, 0, 0).expect("valid time")
    };
    Ok(Some(Utc.from_utc_datetime(&date.and_time(time))))
}
