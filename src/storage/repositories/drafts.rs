//! Репозиторий черновиков capture.
//!
//! Таблица хранит и неподтверждённые черновики, и применённые capture:
//! `items.capture_id` ссылается на строку, пока её не удалит retention.

use chrono::{DateTime, Duration, Utc};
use serde_json::Value;
use sqlx::{FromRow, PgConnection};
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::models::{CaptureMode, CaptureSource, DraftStatus};

/// Время жизни неподтверждённого черновика.
pub const DRAFT_TTL_MINUTES: i64 = 15;

const COLUMNS: &str = "id, household_id, user_id, chat_id, source, mode, idempotency_key, \
    request_hash, raw_text, parsed, status, fallback, prompt_version, telegram_message_id, \
    created_at, expires_at, applied_at";

/// Строка `capture_drafts`.
#[derive(Debug, Clone, FromRow)]
pub struct Draft {
    /// Идентификатор; совпадает с `capture_id` внешнего capture.
    pub id: Uuid,
    /// Household.
    pub household_id: Uuid,
    /// Автор; у голосового capture его нет.
    pub user_id: Option<Uuid>,
    /// Чат Telegram.
    pub chat_id: Option<i64>,
    /// Источник.
    pub source: CaptureSource,
    /// Режим.
    pub mode: CaptureMode,
    /// Ключ идемпотентности.
    pub idempotency_key: Option<String>,
    /// Хеш тела запроса для сравнения повторов.
    pub request_hash: Option<String>,
    /// Исходный текст.
    pub raw_text: String,
    /// Структура черновика.
    pub parsed: Option<Value>,
    /// Статус.
    pub status: DraftStatus,
    /// Черновик собран без LLM.
    pub fallback: bool,
    /// Версия промпта.
    pub prompt_version: Option<String>,
    /// Сообщение с карточкой.
    pub telegram_message_id: Option<i64>,
    /// Момент создания.
    pub created_at: DateTime<Utc>,
    /// Момент истечения.
    pub expires_at: DateTime<Utc>,
    /// Момент применения.
    pub applied_at: Option<DateTime<Utc>>,
}

impl Draft {
    /// Срок жизни вышел к моменту `now`.
    pub fn is_expired(&self, now: DateTime<Utc>) -> bool {
        self.expires_at <= now
    }
}

/// Данные нового черновика.
#[derive(Debug, Clone)]
pub struct NewDraft {
    /// Идентификатор.
    pub id: Uuid,
    /// Household.
    pub household_id: Uuid,
    /// Автор.
    pub user_id: Option<Uuid>,
    /// Чат Telegram.
    pub chat_id: Option<i64>,
    /// Источник.
    pub source: CaptureSource,
    /// Режим.
    pub mode: CaptureMode,
    /// Ключ идемпотентности.
    pub idempotency_key: Option<String>,
    /// Хеш тела запроса.
    pub request_hash: Option<String>,
    /// Исходный текст, уже очищенный от управляющих символов.
    pub raw_text: String,
    /// Структура черновика.
    pub parsed: Value,
    /// Черновик собран без LLM.
    pub fallback: bool,
    /// Версия промпта.
    pub prompt_version: Option<String>,
}

/// Создать черновик со сроком жизни [`DRAFT_TTL_MINUTES`].
pub async fn insert(
    conn: &mut PgConnection,
    draft: NewDraft,
    now: DateTime<Utc>,
) -> Result<Draft, DomainError> {
    let row = sqlx::query_as::<_, Draft>(&format!(
        "INSERT INTO capture_drafts (
             id, household_id, user_id, chat_id, source, mode, idempotency_key,
             request_hash, raw_text, parsed, status, fallback, prompt_version,
             created_at, expires_at
         )
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, 'pending', $11, $12, $13, $14)
         RETURNING {COLUMNS}"
    ))
    .bind(draft.id)
    .bind(draft.household_id)
    .bind(draft.user_id)
    .bind(draft.chat_id)
    .bind(draft.source)
    .bind(draft.mode)
    .bind(draft.idempotency_key)
    .bind(draft.request_hash)
    .bind(draft.raw_text)
    .bind(draft.parsed)
    .bind(draft.fallback)
    .bind(draft.prompt_version)
    .bind(now)
    .bind(now + Duration::minutes(DRAFT_TTL_MINUTES))
    .fetch_one(conn)
    .await?;

    Ok(row)
}

/// Получить черновик household; чужой или несуществующий — `NotFound`.
pub async fn get(
    conn: &mut PgConnection,
    household_id: Uuid,
    draft_id: Uuid,
) -> Result<Draft, DomainError> {
    fetch(conn, household_id, draft_id, false).await
}

/// Получить черновик, заблокировав строку до конца транзакции: параллельные
/// подтверждения и правки выстраиваются в очередь.
pub async fn get_for_update(
    conn: &mut PgConnection,
    household_id: Uuid,
    draft_id: Uuid,
) -> Result<Draft, DomainError> {
    fetch(conn, household_id, draft_id, true).await
}

async fn fetch(
    conn: &mut PgConnection,
    household_id: Uuid,
    draft_id: Uuid,
    lock: bool,
) -> Result<Draft, DomainError> {
    let suffix = if lock { " FOR UPDATE" } else { "" };
    sqlx::query_as::<_, Draft>(&format!(
        "SELECT {COLUMNS} FROM capture_drafts
         WHERE id = $1 AND household_id = $2{suffix}"
    ))
    .bind(draft_id)
    .bind(household_id)
    .fetch_optional(conn)
    .await?
    .ok_or(DomainError::NotFound)
}

/// Заменить структуру ожидающего черновика.
pub async fn set_parsed(
    conn: &mut PgConnection,
    household_id: Uuid,
    draft_id: Uuid,
    parsed: &Value,
) -> Result<(), DomainError> {
    sqlx::query(
        "UPDATE capture_drafts SET parsed = $3
         WHERE id = $1 AND household_id = $2 AND status = 'pending'",
    )
    .bind(draft_id)
    .bind(household_id)
    .bind(parsed)
    .execute(conn)
    .await?;
    Ok(())
}

/// Перевести ожидающий черновик в конечный статус. Применённый черновик
/// получает `applied_at`.
pub async fn finish(
    conn: &mut PgConnection,
    household_id: Uuid,
    draft_id: Uuid,
    status: DraftStatus,
    now: DateTime<Utc>,
) -> Result<(), DomainError> {
    let applied_at = (status == DraftStatus::Applied).then_some(now);
    sqlx::query(
        "UPDATE capture_drafts SET status = $3, applied_at = $4
         WHERE id = $1 AND household_id = $2 AND status = 'pending'",
    )
    .bind(draft_id)
    .bind(household_id)
    .bind(status)
    .bind(applied_at)
    .execute(conn)
    .await?;
    Ok(())
}

/// Запомнить сообщение с карточкой черновика.
pub async fn set_message(
    conn: &mut PgConnection,
    household_id: Uuid,
    draft_id: Uuid,
    message_id: i64,
) -> Result<(), DomainError> {
    sqlx::query(
        "UPDATE capture_drafts SET telegram_message_id = $3
         WHERE id = $1 AND household_id = $2",
    )
    .bind(draft_id)
    .bind(household_id)
    .bind(message_id)
    .execute(conn)
    .await?;
    Ok(())
}

/// Привязать созданные элементы к capture.
pub async fn link_items(
    conn: &mut PgConnection,
    household_id: Uuid,
    draft_id: Uuid,
    item_ids: &[Uuid],
) -> Result<(), DomainError> {
    sqlx::query(
        "UPDATE items SET capture_id = $3
         WHERE household_id = $1 AND id = ANY($2)",
    )
    .bind(household_id)
    .bind(item_ids)
    .bind(draft_id)
    .execute(conn)
    .await?;
    Ok(())
}

/// Идентификаторы элементов capture в порядке создания, включая удалённые.
pub async fn linked_item_ids(
    conn: &mut PgConnection,
    household_id: Uuid,
    draft_id: Uuid,
) -> Result<Vec<Uuid>, DomainError> {
    let ids = sqlx::query_scalar(
        "SELECT id FROM items
         WHERE household_id = $1 AND capture_id = $2
         ORDER BY created_at, id",
    )
    .bind(household_id)
    .bind(draft_id)
    .fetch_all(conn)
    .await?;
    Ok(ids)
}
