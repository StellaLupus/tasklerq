//! Одноразовые callback-токены.
//!
//! В `callback_data` попадает только непрозрачный токен `t:` + 16 символов
//! base64url (18 байт при лимите Telegram в 64). Действие, полезная нагрузка и
//! версия ресурса лежат в базе; там же только SHA-256 хеш токена. Потребление
//! выполняется одним `UPDATE ... RETURNING`, поэтому два параллельных нажатия
//! не выполнят действие дважды.

use chrono::{DateTime, Duration, Utc};
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::auth::secrets::random_base64url;
use crate::auth::tokens::hash_token;

/// Префикс `callback_data` бота.
pub const PREFIX: &str = "t:";

/// Максимальный срок жизни токена.
pub const TTL: Duration = Duration::hours(24);

/// Лимит Telegram на длину `callback_data`, байт.
pub const MAX_CALLBACK_BYTES: usize = 64;

/// Где живёт кнопка.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Кнопка навигационного экрана; действует только на активное сообщение
    /// экрана чата.
    Screen,
    /// Кнопка отдельного сообщения (уведомление), не зависящая от экрана.
    Notice,
}

impl Scope {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Screen => "screen",
            Self::Notice => "notice",
        }
    }

    fn parse(value: &str) -> Self {
        if value == "notice" {
            Self::Notice
        } else {
            Self::Screen
        }
    }
}

/// Описание кнопки, для которой нужен токен.
#[derive(Debug, Clone)]
pub struct NewToken {
    /// Кому предназначена кнопка.
    pub telegram_user_id: i64,
    /// Household пользователя; пусто до онбординга.
    pub household_id: Option<Uuid>,
    /// Область действия.
    pub scope: Scope,
    /// Короткое имя действия (для диагностики).
    pub action: String,
    /// Полезная нагрузка.
    pub payload: Value,
    /// Версия ресурса, на которую рассчитано действие.
    pub version: Option<i32>,
}

/// Действующий токен после потребления.
#[derive(Debug, Clone)]
pub struct Token {
    /// Household пользователя на момент выпуска.
    pub household_id: Option<Uuid>,
    /// Область действия.
    pub scope: Scope,
    /// Имя действия.
    pub action: String,
    /// Полезная нагрузка.
    pub payload: Value,
    /// Версия ресурса.
    pub version: Option<i32>,
}

/// Итог потребления токена.
#[derive(Debug, Clone)]
pub enum Consumed {
    /// Токен действителен и теперь использован.
    Valid(Token),
    /// Токен уже был использован.
    Replayed(Scope),
    /// Срок действия истёк.
    Expired(Scope),
    /// Токен выпущен другому пользователю.
    Foreign,
    /// Такого токена нет.
    Unknown,
}

type ConsumedRow = (Option<Uuid>, String, String, Value, Option<i32>);
type TokenStateRow = (i64, Option<DateTime<Utc>>, DateTime<Utc>, String);

/// Сгенерировать значение `callback_data`.
pub fn generate() -> String {
    format!("{PREFIX}{}", random_base64url(12))
}

/// Похоже ли значение на токен бота.
pub fn looks_like_token(data: &str) -> bool {
    data.len() == PREFIX.len() + 16 && data.starts_with(PREFIX)
}

/// Выпустить токены для набора кнопок; порядок результата совпадает с `specs`.
pub async fn issue_many(
    conn: &mut PgConnection,
    specs: &[NewToken],
    now: DateTime<Utc>,
) -> Result<Vec<String>, sqlx::Error> {
    if specs.is_empty() {
        return Ok(Vec::new());
    }

    let tokens: Vec<String> = specs.iter().map(|_| generate()).collect();
    let hashes: Vec<String> = tokens.iter().map(|token| hash_token(token)).collect();
    let users: Vec<i64> = specs.iter().map(|spec| spec.telegram_user_id).collect();
    let households: Vec<Option<Uuid>> = specs.iter().map(|spec| spec.household_id).collect();
    let scopes: Vec<&str> = specs.iter().map(|spec| spec.scope.as_str()).collect();
    let actions: Vec<&str> = specs.iter().map(|spec| spec.action.as_str()).collect();
    let payloads: Vec<Value> = specs.iter().map(|spec| spec.payload.clone()).collect();
    let versions: Vec<Option<i32>> = specs.iter().map(|spec| spec.version).collect();

    sqlx::query(
        "INSERT INTO callback_tokens (
             token_hash, telegram_user_id, household_id, scope, action, payload, version,
             expires_at, created_at
         )
         SELECT h, u, hh, s, a, p, v, $8, $9
         FROM UNNEST($1::text[], $2::bigint[], $3::uuid[], $4::text[], $5::text[],
                     $6::jsonb[], $7::int[])
              AS t(h, u, hh, s, a, p, v)",
    )
    .bind(&hashes)
    .bind(&users)
    .bind(&households)
    .bind(&scopes)
    .bind(&actions)
    .bind(&payloads)
    .bind(&versions)
    .bind(now + TTL)
    .bind(now)
    .execute(conn)
    .await?;

    Ok(tokens)
}

/// Потребить токен от имени пользователя.
pub async fn consume(
    conn: &mut PgConnection,
    raw: &str,
    telegram_user_id: i64,
    now: DateTime<Utc>,
) -> Result<Consumed, sqlx::Error> {
    if !looks_like_token(raw) {
        return Ok(Consumed::Unknown);
    }
    let token_hash = hash_token(raw);

    let consumed: Option<ConsumedRow> = sqlx::query_as(
        "UPDATE callback_tokens
         SET consumed_at = $3
         WHERE token_hash = $1
           AND telegram_user_id = $2
           AND consumed_at IS NULL
           AND expires_at > $3
         RETURNING household_id, scope, action, payload, version",
    )
    .bind(&token_hash)
    .bind(telegram_user_id)
    .bind(now)
    .fetch_optional(&mut *conn)
    .await?;

    if let Some((household_id, scope, action, payload, version)) = consumed {
        return Ok(Consumed::Valid(Token {
            household_id,
            scope: Scope::parse(&scope),
            action,
            payload,
            version,
        }));
    }

    // Не потребили: выясняем причину, не раскрывая содержимое чужого токена.
    let row: Option<TokenStateRow> = sqlx::query_as(
        "SELECT telegram_user_id, consumed_at, expires_at, scope
         FROM callback_tokens WHERE token_hash = $1",
    )
    .bind(&token_hash)
    .fetch_optional(conn)
    .await?;

    Ok(match row {
        None => Consumed::Unknown,
        Some((owner, _, _, _)) if owner != telegram_user_id => Consumed::Foreign,
        Some((_, Some(_), _, scope)) => Consumed::Replayed(Scope::parse(&scope)),
        Some((_, None, _, scope)) => Consumed::Expired(Scope::parse(&scope)),
    })
}

/// Удалить токены, истёкшие до `cutoff`.
pub async fn purge_expired(
    conn: &mut PgConnection,
    cutoff: DateTime<Utc>,
) -> Result<u64, sqlx::Error> {
    Ok(
        sqlx::query("DELETE FROM callback_tokens WHERE expires_at < $1")
            .bind(cutoff)
            .execute(conn)
            .await?
            .rows_affected(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_fits_telegram_limit() {
        let token = generate();
        assert!(looks_like_token(&token));
        assert!(token.len() <= MAX_CALLBACK_BYTES);
        assert_ne!(token, generate());
    }

    #[test]
    fn foreign_shapes_are_not_tokens() {
        assert!(!looks_like_token("t:short"));
        assert!(!looks_like_token("x:AAAAAAAAAAAAAAAA"));
    }
}
