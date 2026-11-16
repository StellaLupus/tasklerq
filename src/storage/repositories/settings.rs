//! Настройки инсталляции и household: строки таблицы `settings`.
//!
//! Значение хранится в одной из двух колонок: обычное — в `value` (jsonb),
//! секрет — в `secret` (шифротекст формата из `settings::crypto`). Уровень
//! задаётся `scope`; у household есть `household_id`, у installation — `NULL`.
//! Уникальность и цель `ON CONFLICT` — индекс по выражению из миграции 0020:
//! `(scope, coalesce(household_id, nil), key)`.

use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

/// Nil UUID в индексе: настоящим идентификатором быть не может (ids — UUID v7).
const NIL_UUID: &str = "'00000000-0000-0000-0000-000000000000'::uuid";

/// Строка `settings` без привязки к уровню: ключ и ровно одно значение.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredSetting {
    /// Имя настройки (`settings.key`).
    pub key: String,
    /// Обычное значение; `None` у секрета.
    pub value: Option<Value>,
    /// Шифротекст секрета; `None` у обычного значения.
    pub secret: Option<Vec<u8>>,
}

/// Прочитать все строки одного уровня.
///
/// `scope` — `installation` или `household`; для installation `household_id`
/// должен быть `None`. Неизвестные ключи не отбрасываются здесь: их игнорирует
/// `settings::effective_llm`, чтобы откат на старый образ не ломал чтение.
pub async fn all(
    conn: &mut PgConnection,
    scope: &str,
    household_id: Option<Uuid>,
) -> Result<Vec<StoredSetting>, sqlx::Error> {
    let rows: Vec<(String, Option<Value>, Option<Vec<u8>>)> = sqlx::query_as(
        "SELECT key, value, secret
         FROM settings
         WHERE scope = $1 AND household_id IS NOT DISTINCT FROM $2
         ORDER BY key",
    )
    .bind(scope)
    .bind(household_id)
    .fetch_all(conn)
    .await?;

    Ok(rows
        .into_iter()
        .map(|(key, value, secret)| StoredSetting { key, value, secret })
        .collect())
}

/// Зашифрованная строка `settings` вместе с уровнем.
///
/// Уровень нужен проверке расшифровки: AAD зависит от `scope`, `household_id`
/// и `key`, поэтому одна и та же строка читается только своим контекстом.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredSecret {
    /// Значение `settings.scope`: `installation` или `household`.
    pub scope: String,
    /// Household строки; `None` у настройки инсталляции.
    pub household_id: Option<Uuid>,
    /// Имя настройки (`settings.key`).
    pub key: String,
    /// Шифротекст из `settings.secret`.
    pub secret: Vec<u8>,
}

/// Прочитать все зашифрованные строки всех уровней.
///
/// Неизвестные ключи тоже возвращаются: при ротации их придётся перешифровать.
pub async fn secrets(conn: &mut PgConnection) -> Result<Vec<StoredSecret>, sqlx::Error> {
    let rows: Vec<(String, Option<Uuid>, String, Vec<u8>)> = sqlx::query_as(
        "SELECT scope, household_id, key, secret
         FROM settings
         WHERE secret IS NOT NULL
         ORDER BY scope, household_id, key",
    )
    .fetch_all(conn)
    .await?;

    Ok(rows
        .into_iter()
        .map(|(scope, household_id, key, secret)| StoredSecret {
            scope,
            household_id,
            key,
            secret,
        })
        .collect())
}

/// Прочитать все зашифрованные строки всех уровней с блокировкой до конца
/// транзакции.
///
/// Нужно ротации: пока ключ меняется, сервис не должен записать новую строку
/// со старым ключом мимо перешифровки.
pub async fn secrets_for_update(conn: &mut PgConnection) -> Result<Vec<StoredSecret>, sqlx::Error> {
    let rows: Vec<(String, Option<Uuid>, String, Vec<u8>)> = sqlx::query_as(
        "SELECT scope, household_id, key, secret
         FROM settings
         WHERE secret IS NOT NULL
         ORDER BY scope, household_id, key
         FOR UPDATE",
    )
    .fetch_all(conn)
    .await?;

    Ok(rows
        .into_iter()
        .map(|(scope, household_id, key, secret)| StoredSecret {
            scope,
            household_id,
            key,
            secret,
        })
        .collect())
}

/// Записать новый шифротекст существующей секретной строки.
pub async fn update_secret(
    conn: &mut PgConnection,
    scope: &str,
    household_id: Option<Uuid>,
    key: &str,
    secret: &[u8],
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE settings
         SET secret = $4, updated_at = now()
         WHERE scope = $1 AND household_id IS NOT DISTINCT FROM $2 AND key = $3
           AND secret IS NOT NULL",
    )
    .bind(scope)
    .bind(household_id)
    .bind(key)
    .bind(secret)
    .execute(conn)
    .await?;
    Ok(())
}

/// Записать обычное значение, заменив прежнее (в том числе секрет).
pub async fn set_value(
    conn: &mut PgConnection,
    scope: &str,
    household_id: Option<Uuid>,
    key: &str,
    value: &Value,
) -> Result<(), sqlx::Error> {
    let sql = format!(
        "INSERT INTO settings (scope, household_id, key, value, updated_at)
         VALUES ($1, $2, $3, $4, now())
         ON CONFLICT (scope, coalesce(household_id, {NIL_UUID}), key) DO UPDATE SET
             value = EXCLUDED.value,
             secret = NULL,
             updated_at = EXCLUDED.updated_at"
    );
    sqlx::query(&sql)
        .bind(scope)
        .bind(household_id)
        .bind(key)
        .bind(value)
        .execute(conn)
        .await?;
    Ok(())
}

/// Записать секрет, заменив прежнее обычное значение.
pub async fn set_secret(
    conn: &mut PgConnection,
    scope: &str,
    household_id: Option<Uuid>,
    key: &str,
    secret: &[u8],
) -> Result<(), sqlx::Error> {
    let sql = format!(
        "INSERT INTO settings (scope, household_id, key, secret, updated_at)
         VALUES ($1, $2, $3, $4, now())
         ON CONFLICT (scope, coalesce(household_id, {NIL_UUID}), key) DO UPDATE SET
             value = NULL,
             secret = EXCLUDED.secret,
             updated_at = EXCLUDED.updated_at"
    );
    sqlx::query(&sql)
        .bind(scope)
        .bind(household_id)
        .bind(key)
        .bind(secret)
        .execute(conn)
        .await?;
    Ok(())
}

/// Удалить настройку; `true`, если строка была.
pub async fn delete(
    conn: &mut PgConnection,
    scope: &str,
    household_id: Option<Uuid>,
    key: &str,
) -> Result<bool, sqlx::Error> {
    let deleted = sqlx::query(
        "DELETE FROM settings
         WHERE scope = $1 AND household_id IS NOT DISTINCT FROM $2 AND key = $3",
    )
    .bind(scope)
    .bind(household_id)
    .bind(key)
    .execute(conn)
    .await?
    .rows_affected();
    Ok(deleted > 0)
}
