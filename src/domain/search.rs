//! Поиск: фильтры и курсорная пагинация.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, NaiveDate, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::error::DomainError;
use super::models::ItemStatus;
use super::recurrence::resolve_local;

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
    /// Конец диапазона сроков (исключающий).
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
    /// Срок сортировки; для элементов без срока — `9999-12-31` (как в SQL
    /// запроса поиска).
    pub due: DateTime<Utc>,
    /// Идентификатор.
    pub id: Uuid,
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

/// Граница диапазона сроков.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DueBound {
    /// Включающее начало.
    From,
    /// Исключающий конец.
    To,
}

/// Разобрать границу диапазона сроков.
///
/// Дата `YYYY-MM-DD` трактуется в часовом поясе household: `From` — локальная
/// полночь дня, `To` — локальная полночь следующего дня. Диапазон
/// полуоткрытый, поэтому последняя секунда дня не теряется. Значение RFC 3339 —
/// точный момент: `From` включается, `To` исключается.
pub fn parse_due_bound(
    value: Option<&str>,
    bound: DueBound,
    timezone: Tz,
) -> Result<Option<DateTime<Utc>>, DomainError> {
    let Some(value) = value else {
        return Ok(None);
    };

    if let Ok(at) = DateTime::parse_from_rfc3339(value) {
        return Ok(Some(at.with_timezone(&Utc)));
    }

    let date = NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .map_err(|_| DomainError::Validation("due bound must be RFC 3339 or YYYY-MM-DD".into()))?;
    let day = match bound {
        DueBound::From => date,
        DueBound::To => date.succ_opt().ok_or_else(|| {
            DomainError::Validation("due bound is out of the supported date range".into())
        })?,
    };
    let midnight = chrono::NaiveTime::from_hms_opt(0, 0, 0).expect("valid time");
    Ok(Some(resolve_local(timezone, day, midnight)?))
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use chrono_tz::Europe::Moscow;

    use super::*;

    #[test]
    fn date_bounds_use_household_timezone_half_open() {
        let from = parse_due_bound(Some("2026-11-02"), DueBound::From, Moscow)
            .expect("from bound")
            .expect("some");
        let to = parse_due_bound(Some("2026-11-02"), DueBound::To, Moscow)
            .expect("to bound")
            .expect("some");

        // 00:00 и 01:00 MSK 2/3 ноября в UTC.
        assert_eq!(from, Utc.with_ymd_and_hms(2026, 11, 1, 21, 0, 0).unwrap());
        assert_eq!(to, Utc.with_ymd_and_hms(2026, 11, 2, 21, 0, 0).unwrap());
    }

    #[test]
    fn rfc3339_bound_is_exact_instant() {
        let from = parse_due_bound(Some("2026-11-02T00:30:00+03:00"), DueBound::From, Moscow)
            .expect("from bound")
            .expect("some");
        assert_eq!(from, Utc.with_ymd_and_hms(2026, 11, 1, 21, 30, 0).unwrap());
    }

    #[test]
    fn missing_and_invalid_bounds() {
        assert!(
            parse_due_bound(None, DueBound::From, Moscow)
                .expect("none bound")
                .is_none()
        );
        assert!(parse_due_bound(Some("2026-13-01"), DueBound::From, Moscow).is_err());
        assert!(parse_due_bound(Some("nope"), DueBound::To, Moscow).is_err());
    }
}
