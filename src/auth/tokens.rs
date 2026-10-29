//! API-токены: хеширование, проверка и контекст запроса.

use chrono::{DateTime, Utc};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::{PgConnection, PgPool};
use thiserror::Error;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::identity::AuditEventType;
use crate::domain::models::{Actor, ActorType, UserRole};
use crate::domain::validation::normalize_text;
use crate::storage::repositories::tokens::{self, ApiToken};
use crate::storage::repositories::{audit, households};

use super::secrets::random_base64url;

/// Префикс всех токенов Tasklerq.
pub const TOKEN_PREFIX: &str = "tlq_";
/// Длина видимого префикса, по которому токен узнают в списках.
const VISIBLE_PREFIX_LEN: usize = 12;
/// Поддерживаемые scopes.
pub const SCOPES: [&str; 3] = ["ha:read", "ha:write", "capture:write"];

/// Ошибка аутентификации или авторизации.
#[derive(Debug, Error)]
pub enum AuthError {
    /// Токен отсутствует, неизвестен, отозван или истёк.
    #[error("invalid or expired token")]
    Unauthorized,
    /// Токену не хватает scope.
    #[error("token does not have the required scope: {0}")]
    Forbidden(String),
    /// Ошибка базы данных.
    #[error("database error")]
    Database(#[from] sqlx::Error),
}

/// Контекст аутентифицированного запроса.
#[derive(Debug, Clone)]
pub struct AuthContext {
    /// Идентификатор токена.
    pub token_id: Uuid,
    /// Household токена.
    pub household_id: Uuid,
    /// Scopes токена.
    pub scopes: Vec<String>,
    /// Актор доменных операций.
    pub actor: Actor,
}

impl AuthContext {
    /// Проверить наличие scope.
    pub fn require_scope(&self, scope: &str) -> Result<(), AuthError> {
        if self.scopes.iter().any(|value| value == scope) {
            Ok(())
        } else {
            Err(AuthError::Forbidden(scope.to_owned()))
        }
    }
}

/// Выпущенный токен: открытое значение показывается один раз.
#[derive(Debug, Clone)]
pub struct IssuedToken {
    /// Открытое значение токена.
    pub token: String,
    /// Запись без хеша.
    pub record: ApiToken,
}

/// Сгенерировать токен: `tlq_` + 32 случайных байта в base64url.
pub fn generate_token() -> String {
    format!("{TOKEN_PREFIX}{}", random_base64url(32))
}

/// Проверить scopes: непустой набор известных значений без повторов.
pub fn validate_scopes(scopes: &[String]) -> Result<Vec<String>, DomainError> {
    let mut normalized: Vec<String> = Vec::with_capacity(scopes.len());
    for scope in scopes {
        let scope = scope.trim();
        if !SCOPES.contains(&scope) {
            return Err(DomainError::Validation(format!(
                "unknown scope: {scope}; allowed: {}",
                SCOPES.join(", ")
            )));
        }
        if !normalized.iter().any(|existing| existing == scope) {
            normalized.push(scope.to_owned());
        }
    }
    if normalized.is_empty() {
        return Err(DomainError::Validation(
            "at least one scope is required".into(),
        ));
    }
    Ok(normalized)
}

/// Выпустить токен household. `actor` попадает в аудит; секрет в аудит не
/// попадает, только идентификатор, префикс и scopes. Вызывать в транзакции.
pub async fn issue(
    conn: &mut PgConnection,
    household_id: Uuid,
    name: &str,
    scopes: &[String],
    expires_at: Option<DateTime<Utc>>,
    actor: &Actor,
    now: DateTime<Utc>,
) -> Result<IssuedToken, DomainError> {
    let name = normalize_text(name, 64, "token name")?;
    let scopes = validate_scopes(scopes)?;
    if let Some(expires_at) = expires_at
        && expires_at <= now
    {
        return Err(DomainError::Validation(
            "token expiry must be in the future".into(),
        ));
    }
    households::get(conn, household_id).await?;

    let token = generate_token();
    let prefix: String = token.chars().take(VISIBLE_PREFIX_LEN).collect();
    let record = tokens::create(
        conn,
        household_id,
        &name,
        &hash_token(&token),
        &prefix,
        &scopes,
        expires_at,
        actor.user_id,
        now,
    )
    .await?;

    audit::record(
        conn,
        Some(household_id),
        actor,
        AuditEventType::TokenCreated,
        json!({
            "token_id": record.id,
            "name": record.name,
            "prefix": record.prefix,
            "scopes": record.scopes,
            "expires_at": record.expires_at,
        }),
        now,
    )
    .await?;

    Ok(IssuedToken { token, record })
}

/// Отозвать токен. Повторный отзыв идемпотентен и не пишет второе событие.
/// Возвращает запись и признак «отозван сейчас».
pub async fn revoke(
    conn: &mut PgConnection,
    token_id: Uuid,
    actor: &Actor,
    now: DateTime<Utc>,
) -> Result<(ApiToken, bool), DomainError> {
    if let Some(record) = tokens::revoke(conn, token_id, now).await? {
        audit::record(
            conn,
            Some(record.household_id),
            actor,
            AuditEventType::TokenRevoked,
            json!({"token_id": record.id, "name": record.name, "prefix": record.prefix}),
            now,
        )
        .await?;
        return Ok((record, true));
    }

    let record = tokens::find_by_id(conn, token_id)
        .await?
        .ok_or(DomainError::NotFound)?;
    Ok((record, false))
}

/// SHA-256 токена в нижнем hexadecimal-представлении.
pub fn hash_token(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    let mut hash = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(hash, "{byte:02x}");
    }
    hash
}

/// Проверить Bearer-токен и вернуть контекст.
pub async fn authenticate(pool: &PgPool, token: &str) -> Result<Option<AuthContext>, AuthError> {
    if !token.starts_with("tlq_") {
        return Ok(None);
    }

    let token_hash = hash_token(token);
    let now = chrono::Utc::now();
    let mut conn = pool.acquire().await?;

    let Some(record) = tokens::find_valid(&mut conn, &token_hash, now)
        .await
        .map_err(|error| match error {
            crate::domain::error::DomainError::Database(error) => AuthError::Database(error),
            _ => AuthError::Unauthorized,
        })?
    else {
        return Ok(None);
    };

    tokens::touch(&mut conn, record.id, now)
        .await
        .map_err(|error| match error {
            crate::domain::error::DomainError::Database(error) => AuthError::Database(error),
            _ => AuthError::Unauthorized,
        })?;

    Ok(Some(AuthContext {
        token_id: record.id,
        household_id: record.household_id,
        scopes: record.scopes,
        actor: Actor {
            user_id: None,
            role: Some(UserRole::Owner),
            actor_type: ActorType::HomeAssistant,
        },
    }))
}
