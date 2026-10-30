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
use thiserror::Error;
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

/// Ошибка сохранения ответа в резерве.
#[derive(Debug, Error)]
pub enum CompleteError {
    /// Резерв перехвачен другим запросом: коммитить capture опоздавшего
    /// запроса нельзя.
    #[error("the idempotency reservation was taken over by another request")]
    Lost,
    /// Ошибка базы данных.
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

/// Сохранить готовый ответ. Для `applied` и `draft` вызывается в транзакции
/// создания `capture_drafts`/`items`, чтобы ответ и строки capture коммитились
/// вместе.
///
/// `capture_id` — идентификатор владельца резерва: строку, которую успел
/// перехватить другой запрос, менять нельзя.
#[allow(clippy::too_many_arguments)]
pub async fn complete(
    conn: &mut PgConnection,
    household_id: Uuid,
    source: CaptureSource,
    key: &str,
    capture_id: Uuid,
    response: &Value,
    now: DateTime<Utc>,
) -> Result<(), CompleteError> {
    let updated = sqlx::query(
        "UPDATE capture_requests
         SET response = $5, completed_at = $6
         WHERE household_id = $1 AND source = $2 AND idempotency_key = $3
           AND capture_id = $4",
    )
    .bind(household_id)
    .bind(source)
    .bind(key)
    .bind(capture_id)
    .bind(response)
    .bind(now)
    .execute(conn)
    .await?;
    if updated.rows_affected() != 1 {
        return Err(CompleteError::Lost);
    }
    Ok(())
}

/// Освободить ключ после ошибки: повтор обработается заново. Чужой резерв
/// (перехваченный после 60 секунд) и уже записанный ответ остаются нетронутыми.
pub async fn release(
    conn: &mut PgConnection,
    household_id: Uuid,
    source: CaptureSource,
    key: &str,
    capture_id: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "DELETE FROM capture_requests
         WHERE household_id = $1 AND source = $2 AND idempotency_key = $3
           AND capture_id = $4 AND response IS NULL",
    )
    .bind(household_id)
    .bind(source)
    .bind(key)
    .bind(capture_id)
    .execute(conn)
    .await?;
    Ok(())
}

/// Резерв ключа, занятый текущим запросом.
///
/// Пока резерв не завершён и не освобождён, он удерживает ключ. `complete`
/// пишет ответ (в транзакции вызывающей стороны), [`Reservation::settle`]
/// подтверждает, что транзакция закоммичена, а [`Reservation::release`]
/// освобождает ключ после ошибки. Все операции идемпотентны и проверяют
/// `capture_id`, поэтому перехваченный резерв не будет испорчен.
#[derive(Debug)]
pub struct Reservation {
    household_id: Uuid,
    source: CaptureSource,
    key: String,
    capture_id: Uuid,
    /// Резерв больше не принадлежит этому запросу: ответ записан и закоммичен
    /// либо ключ освобождён.
    settled: bool,
}

impl Reservation {
    /// Новый резерв уже занятого ключа.
    pub fn new(household_id: Uuid, source: CaptureSource, key: &str, capture_id: Uuid) -> Self {
        Self {
            household_id,
            source,
            key: key.to_owned(),
            capture_id,
            settled: false,
        }
    }

    /// Записать ответ; коммит остаётся за вызывающей стороной.
    pub async fn complete(
        &mut self,
        conn: &mut PgConnection,
        response: &Value,
        now: DateTime<Utc>,
    ) -> Result<(), CompleteError> {
        if self.settled {
            return Ok(());
        }
        complete(
            conn,
            self.household_id,
            self.source,
            &self.key,
            self.capture_id,
            response,
            now,
        )
        .await
    }

    /// Проверить, что резерв всё ещё принадлежит запросу, и заблокировать
    /// строку до конца транзакции.
    ///
    /// Вызывается перед записью capture: блокировка сериализует опоздавшего и
    /// нового владельца, поэтому вставка `capture_drafts` не падает на
    /// уникальном индексе вместо [`CompleteError::Lost`].
    pub async fn verify(&self, conn: &mut PgConnection) -> Result<(), CompleteError> {
        let owned = sqlx::query_scalar::<_, i32>(
            "SELECT 1 FROM capture_requests
             WHERE household_id = $1 AND source = $2 AND idempotency_key = $3
               AND capture_id = $4
             FOR UPDATE",
        )
        .bind(self.household_id)
        .bind(self.source)
        .bind(&self.key)
        .bind(self.capture_id)
        .fetch_optional(conn)
        .await?;
        if owned.is_none() {
            return Err(CompleteError::Lost);
        }
        Ok(())
    }

    /// Ответ и строки capture закоммичены: освобождать нечего.
    pub fn settle(&mut self) {
        self.settled = true;
    }

    /// Резерв уже завершён или освобождён.
    pub fn is_settled(&self) -> bool {
        self.settled
    }

    /// Освободить ключ после ошибки. Если ответ уже записан, ничего не делает.
    pub async fn release(&mut self, conn: &mut PgConnection) {
        if self.settled {
            return;
        }
        if let Err(error) = release(
            conn,
            self.household_id,
            self.source,
            &self.key,
            self.capture_id,
        )
        .await
        {
            tracing::error!(%error, "could not release the idempotency key");
        }
        self.settled = true;
    }
}
