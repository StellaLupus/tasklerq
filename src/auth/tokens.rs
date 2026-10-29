//! API-токены: хеширование, проверка и контекст запроса.

use sha2::{Digest, Sha256};
use sqlx::PgPool;
use thiserror::Error;
use uuid::Uuid;

use crate::domain::models::{Actor, ActorType, UserRole};
use crate::storage::repositories::tokens;

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
