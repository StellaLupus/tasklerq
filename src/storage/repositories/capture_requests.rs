//! Резервы ключей идемпотентности внешнего capture.
//!
//! Строка `capture_requests` появляется до вызова LLM и живёт до завершения
//! запроса. Пока `response` пуст, повтор с тем же ключом получает `409`; после
//! успеха готовый ответ возвращается без нового разбора. Зависший резерв
//! перехватывается атомарным `UPDATE`, поэтому параллельные повторы не
//! запускают второй разбор.

use chrono::{DateTime, Duration, Utc};
use serde_json::Value;
use sqlx::{Connection, FromRow, PgConnection};
use uuid::Uuid;

use crate::domain::models::CaptureSource;

/// Сколько секунд резерв без ответа считается живым.
pub const STALE_SECONDS: i64 = 60;

/// Чем закончилась попытка занять ключ.
#[derive(Debug)]
pub enum Reserved {
    /// Ключ принадлежит этому запросу: можно вызывать LLM.
    Acquired,
    /// Первый запрос уже завершён; нужно вернуть сохранённый ответ.
    Completed(Value),
    /// Первый запрос ещё обрабатывается.
    InProgress,
    /// Тот же ключ передан с другим телом запроса.
    Mismatch,
}

#[derive(Debug, FromRow)]
struct Existing {
    request_hash: String,
    response: Option<Value>,
    created_at: DateTime<Utc>,
}

/// Занять ключ `key` или сообщить, что с ним уже произошло.
///
/// `ON CONFLICT DO NOTHING` и следующий `SELECT` идут в одной транзакции:
/// параллельная вставка ждёт коммита первого резерва и затем видит конфликт.
/// Резерв коммитится до возврата, иначе вызов LLM не увидел бы его.
#[allow(clippy::too_many_arguments)]
pub async fn reserve(
    conn: &mut PgConnection,
    household_id: Uuid,
    source: CaptureSource,
    key: &str,
    request_hash: &str,
    capture_id: Uuid,
    now: DateTime<Utc>,
) -> Result<Reserved, sqlx::Error> {
    let mut tx = conn.begin().await?;

    // Строка могла исчезнуть между конфликтом вставки и чтением: её удалил
    // освободивший ключ запрос. Тогда вставку можно повторить.
    for _ in 0..2 {
        let inserted = sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO capture_requests (
                 household_id, source, idempotency_key, request_hash, capture_id,
                 response, created_at, completed_at
             )
             VALUES ($1, $2, $3, $4, $5, NULL, $6, NULL)
             ON CONFLICT (household_id, source, idempotency_key) DO NOTHING
             RETURNING capture_id",
        )
        .bind(household_id)
        .bind(source)
        .bind(key)
        .bind(request_hash)
        .bind(capture_id)
        .bind(now)
        .fetch_optional(&mut *tx)
        .await?;

        if inserted.is_some() {
            tx.commit().await?;
            return Ok(Reserved::Acquired);
        }

        let Some(existing) = sqlx::query_as::<_, Existing>(
            "SELECT request_hash, response, created_at
             FROM capture_requests
             WHERE household_id = $1 AND source = $2 AND idempotency_key = $3",
        )
        .bind(household_id)
        .bind(source)
        .bind(key)
        .fetch_optional(&mut *tx)
        .await?
        else {
            continue;
        };

        if existing.request_hash != request_hash {
            tx.commit().await?;
            return Ok(Reserved::Mismatch);
        }

        if let Some(response) = existing.response {
            tx.commit().await?;
            return Ok(Reserved::Completed(response));
        }

        if existing.created_at >= now - Duration::seconds(STALE_SECONDS) {
            tx.commit().await?;
            return Ok(Reserved::InProgress);
        }

        // Первый запрос не оставил ответа и не уложился в таймаут LLM:
        // забираем резерв, только если его никто не перехватил раньше.
        let taken = sqlx::query_scalar::<_, Uuid>(
            "UPDATE capture_requests
             SET request_hash = $4, capture_id = $5, response = NULL,
                 completed_at = NULL, created_at = $6
             WHERE household_id = $1 AND source = $2 AND idempotency_key = $3
               AND response IS NULL AND created_at < $7
             RETURNING capture_id",
        )
        .bind(household_id)
        .bind(source)
        .bind(key)
        .bind(request_hash)
        .bind(capture_id)
        .bind(now)
        .bind(now - Duration::seconds(STALE_SECONDS))
        .fetch_optional(&mut *tx)
        .await?;

        tx.commit().await?;
        return Ok(if taken.is_some() {
            Reserved::Acquired
        } else {
            Reserved::InProgress
        });
    }

    tx.commit().await?;
    Ok(Reserved::InProgress)
}

/// Сохранить готовый ответ. Для `applied` и `draft` вызывается в транзакции
/// создания `capture_drafts`/`items`, чтобы ответ и строки capture коммитились
/// вместе.
pub async fn complete(
    conn: &mut PgConnection,
    household_id: Uuid,
    source: CaptureSource,
    key: &str,
    response: &Value,
    now: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE capture_requests
         SET response = $4, completed_at = $5
         WHERE household_id = $1 AND source = $2 AND idempotency_key = $3",
    )
    .bind(household_id)
    .bind(source)
    .bind(key)
    .bind(response)
    .bind(now)
    .execute(conn)
    .await?;
    Ok(())
}

/// Освободить ключ после ошибки: повтор обработается заново.
pub async fn release(
    conn: &mut PgConnection,
    household_id: Uuid,
    source: CaptureSource,
    key: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "DELETE FROM capture_requests
         WHERE household_id = $1 AND source = $2 AND idempotency_key = $3",
    )
    .bind(household_id)
    .bind(source)
    .bind(key)
    .execute(conn)
    .await?;
    Ok(())
}
