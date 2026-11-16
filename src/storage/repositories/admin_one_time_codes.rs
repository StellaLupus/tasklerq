//! Одноразовые коды консоли администратора.
//!
//! Код — 32 случайных байта (256 бит) в base64url. В базе хранится только
//! SHA-256 хеш в нижнем hexadecimal (`auth::tokens::hash_token`), а открытое
//! значение возвращается один раз при выпуске и больше нигде не читается.
//!
//! Проверка кода не сравнивает открытый текст в памяти: сервер хеширует ввод и
//! ищет строку по `code_hash` (уникальный индекс), поэтому сравнение выполняет
//! PostgreSQL по хешу. Отдельного сравнения за постоянное время здесь не
//! нужно; если в этом слое появится сравнение секретов в памяти, его следует
//! делать через `subtle`, а не `==`.
//!
//! `expires_at` задаёт вызывающая сторона: `setup` живёт без срока, `recovery` —
//! 15 минут. Ограничения назначений, срока и household держит схема
//! (`migrations/0021_admin_console.sql`).

use std::fmt;

use chrono::{DateTime, Utc};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::auth::secrets::random_base64url;
use crate::auth::tokens::hash_token;
use crate::domain::error::DomainError;
use crate::redaction::REDACTED;

/// Назначение `setup`: первый вход, пока учётной записи нет.
pub const PURPOSE_SETUP: &str = "setup";
/// Назначение `recovery`: восстановление доступа.
pub const PURPOSE_RECOVERY: &str = "recovery";
/// Длина случайной части кода в байтах: 256 бит, не меньше требуемых 128.
pub const CODE_BYTES: usize = 32;

const COLUMNS: &str = "id, purpose, household_id, expires_at, used_at, created_at";

/// Строка `admin_one_time_codes` без хеша.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct OneTimeCode {
    /// Идентификатор.
    pub id: Uuid,
    /// Назначение.
    pub purpose: String,
    /// Household для кодов уровня household; в 0013 всегда `None`.
    pub household_id: Option<Uuid>,
    /// Срок действия; `None` только у `setup`.
    pub expires_at: Option<DateTime<Utc>>,
    /// Когда код погашен.
    pub used_at: Option<DateTime<Utc>>,
    /// Время создания.
    pub created_at: DateTime<Utc>,
}

/// Выпущенный код: открытое значение показывается один раз.
#[derive(Clone)]
pub struct IssuedCode {
    /// Открытое значение кода.
    pub code: String,
    /// Строка без хеша.
    pub record: OneTimeCode,
}

impl fmt::Debug for IssuedCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("IssuedCode")
            .field("code", &REDACTED)
            .field("record", &self.record)
            .finish()
    }
}

/// Выпустить код и погасить прежние неиспользованные коды той же пары
/// `(purpose, household_id)`.
///
/// Оба оператора должны стать видимыми вместе, поэтому вызывать внутри
/// транзакции: [`crate::admin_console::codes::issue`] открывает её сама.
///
/// Сначала берётся `pg_advisory_xact_lock` от назначения (и household):
/// параллельный выпуск той же пары ждёт коммита и видит уже погашенные коды,
/// поэтому двух действующих кодов не остаётся.
pub async fn issue(
    conn: &mut PgConnection,
    purpose: &str,
    household_id: Option<Uuid>,
    expires_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Result<IssuedCode, DomainError> {
    lock_purpose(conn, purpose, household_id).await?;

    sqlx::query(
        "UPDATE admin_one_time_codes SET used_at = $3
         WHERE purpose = $1 AND household_id IS NOT DISTINCT FROM $2
           AND used_at IS NULL",
    )
    .bind(purpose)
    .bind(household_id)
    .bind(now)
    .execute(&mut *conn)
    .await?;

    let code = random_base64url(CODE_BYTES);
    let record = sqlx::query_as::<_, OneTimeCode>(&format!(
        "INSERT INTO admin_one_time_codes
             (id, purpose, household_id, code_hash, expires_at, created_at)
         VALUES ($1, $2, $3, $4, $5, $6)
         RETURNING {COLUMNS}"
    ))
    .bind(Uuid::now_v7())
    .bind(purpose)
    .bind(household_id)
    .bind(hash_token(&code))
    .bind(expires_at)
    .bind(now)
    .fetch_one(&mut *conn)
    .await?;

    Ok(IssuedCode { code, record })
}

/// Взять транзакционную блокировку выпуска кодов назначения.
///
/// Ключ — строка `admin_one_time_codes:<purpose>[:<household>]`, хешированная
/// функцией PostgreSQL: отдельной миграции для ключей не нужно. Блокировка
/// снимается коммитом или откатом транзакции вызывающего. Повторный вызов в той
/// же транзакции (например, `issue_setup_code` → `issue`) допустим: advisory
/// lock реентерабелен.
pub async fn lock_purpose(
    conn: &mut PgConnection,
    purpose: &str,
    household_id: Option<Uuid>,
) -> Result<(), DomainError> {
    let key = household_id.map_or_else(
        || format!("admin_one_time_codes:{purpose}"),
        |household_id| format!("admin_one_time_codes:{purpose}:{household_id}"),
    );
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(key)
        .execute(&mut *conn)
        .await?;

    Ok(())
}

/// Найти действующий код по открытому значению, не гася его.
///
/// Так проверяет ссылку восстановления `GET`: предпросмотр не должен её
/// сжигать. Условия те же, что у [`use_code`]: назначение, `used_at IS NULL` и
/// неистёкший срок; неизвестный, погашенный, истёкший и чужой код неотличимы.
pub async fn find_active(
    conn: &mut PgConnection,
    purpose: &str,
    code: &str,
    now: DateTime<Utc>,
) -> Result<Option<OneTimeCode>, DomainError> {
    let record = sqlx::query_as::<_, OneTimeCode>(&format!(
        "SELECT {COLUMNS} FROM admin_one_time_codes
         WHERE code_hash = $1 AND purpose = $2 AND used_at IS NULL
           AND (expires_at IS NULL OR expires_at > $3)"
    ))
    .bind(hash_token(code.trim()))
    .bind(purpose)
    .bind(now)
    .fetch_optional(&mut *conn)
    .await?;

    Ok(record)
}

/// Погасить код атомарно и вернуть его строку.
///
/// Повторный, истёкший, уже погашенный и код другого назначения неотличимы:
/// запрос не возвращает строку и функция отвечает [`DomainError::NotFound`].
/// Вызывать внутри транзакции вместе с действием, ради которого код выдан:
/// откат транзакции возвращает код в неиспользованное состояние.
///
/// Ввод обрезается по краям: код копируют из строки журнала, и пробелы вокруг
/// него не должны влиять на сравнение с хешем.
pub async fn use_code(
    conn: &mut PgConnection,
    purpose: &str,
    code: &str,
    now: DateTime<Utc>,
) -> Result<OneTimeCode, DomainError> {
    sqlx::query_as::<_, OneTimeCode>(&format!(
        "UPDATE admin_one_time_codes SET used_at = $3
         WHERE code_hash = $1 AND purpose = $2 AND used_at IS NULL
           AND (expires_at IS NULL OR expires_at > $3)
         RETURNING {COLUMNS}"
    ))
    .bind(hash_token(code.trim()))
    .bind(purpose)
    .bind(now)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(DomainError::NotFound)
}
