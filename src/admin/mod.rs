//! Административный CLI: токены, household и пользователи.
//!
//! Функции этого модуля работают поверх пула и открывают собственные
//! транзакции; разбор аргументов и вывод находятся в `main.rs`. Остальные
//! команды (очереди, webhook, экспорт) добавляются в задаче 8.7.

use chrono::{DateTime, Duration, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::tokens::{self, IssuedToken};
use crate::domain::error::DomainError;
use crate::domain::identity::NewMember;
use crate::domain::models::{Actor, ActorType, Household, User, UserRole};
use crate::storage::repositories::tokens::ApiToken;
use crate::storage::repositories::{households, users};

/// Имя участника, если администратор его не указал.
pub const DEFAULT_MEMBER_NAME: &str = "Участник";

/// Максимальный срок токена, дней: десять лет.
pub const MAX_TOKEN_EXPIRY_DAYS: i64 = 3650;

/// Актор административных действий: не пользователь и не интеграция.
pub fn admin_actor() -> Actor {
    Actor::system(ActorType::System)
}

/// Выпустить токен household. Открытое значение возвращается один раз.
pub async fn create_token(
    pool: &PgPool,
    household_id: Option<Uuid>,
    name: &str,
    scopes: &[String],
    expires_in_days: Option<i64>,
    now: DateTime<Utc>,
) -> Result<IssuedToken, DomainError> {
    let expires_at = match expires_in_days {
        Some(days) if (1..=MAX_TOKEN_EXPIRY_DAYS).contains(&days) => Some(
            Duration::try_days(days)
                .and_then(|span| now.checked_add_signed(span))
                .ok_or_else(|| DomainError::Validation("expiry is out of range".into()))?,
        ),
        Some(_) => {
            return Err(DomainError::Validation(format!(
                "expiry must be between 1 and {MAX_TOKEN_EXPIRY_DAYS} days"
            )));
        }
        None => None,
    };

    let mut tx = pool.begin().await?;
    let household = resolve_household(&mut tx, household_id).await?;
    let issued = tokens::issue(
        &mut tx,
        household.id,
        name,
        scopes,
        expires_at,
        &admin_actor(),
        now,
    )
    .await?;
    tx.commit().await?;

    Ok(issued)
}

/// Отозвать токен. Второй элемент — был ли токен отозван сейчас.
pub async fn revoke_token(
    pool: &PgPool,
    token_id: Uuid,
    now: DateTime<Utc>,
) -> Result<(ApiToken, bool), DomainError> {
    let mut tx = pool.begin().await?;
    let result = tokens::revoke(&mut tx, token_id, &admin_actor(), now).await?;
    tx.commit().await?;

    Ok(result)
}

/// Токены household без секретов.
pub async fn list_tokens(
    pool: &PgPool,
    household_id: Option<Uuid>,
) -> Result<Vec<ApiToken>, DomainError> {
    let mut conn = pool.acquire().await?;
    let household = resolve_household(&mut conn, household_id).await?;
    crate::storage::repositories::tokens::list(&mut conn, household.id).await
}

/// Создать household без владельца.
pub async fn create_household(
    pool: &PgPool,
    name: &str,
    timezone: &str,
    now: DateTime<Utc>,
) -> Result<Household, DomainError> {
    let mut tx = pool.begin().await?;
    let household = households::create_empty(&mut tx, name, timezone, &admin_actor(), now).await?;
    tx.commit().await?;

    Ok(household)
}

/// Добавить участника. Без `household_id` используется единственный
/// существующий household.
pub async fn add_user(
    pool: &PgPool,
    household_id: Option<Uuid>,
    role: UserRole,
    telegram_user_id: i64,
    display_name: Option<&str>,
    now: DateTime<Utc>,
) -> Result<(Household, User), DomainError> {
    let mut tx = pool.begin().await?;
    let household = resolve_household(&mut tx, household_id).await?;

    // В приватном чате chat_id равен Telegram ID, поэтому CLI достаточно ID.
    let user = users::add(
        &mut tx,
        household.id,
        role,
        NewMember {
            telegram_user_id,
            telegram_chat_id: telegram_user_id,
            display_name: display_name.unwrap_or(DEFAULT_MEMBER_NAME).to_owned(),
            username: None,
        },
        &admin_actor(),
        now,
    )
    .await?;
    tx.commit().await?;

    Ok((household, user))
}

/// Household из аргумента или, если его не указали, единственный существующий.
pub async fn resolve_household(
    conn: &mut sqlx::PgConnection,
    household_id: Option<Uuid>,
) -> Result<Household, DomainError> {
    match household_id {
        Some(id) => households::get(conn, id).await,
        None => select_household(households::list_active(conn).await?),
    }
}

/// Выбрать household, когда администратор его не указал: допустим только
/// единственный.
pub fn select_household(mut all: Vec<Household>) -> Result<Household, DomainError> {
    match all.len() {
        0 => Err(DomainError::Validation(
            "no household exists; create one with `admin household create`".into(),
        )),
        1 => Ok(all.remove(0)),
        count => Err(DomainError::Validation(format!(
            "{count} households exist; pass --household"
        ))),
    }
}

/// Человекочитаемое описание ошибки для CLI с HTTP-подобным кодом.
pub fn describe(error: &DomainError) -> String {
    match error {
        DomainError::Validation(message) => format!("invalid input (422): {message}"),
        DomainError::NotFound => "not found (404)".into(),
        DomainError::Forbidden(message) => format!("forbidden (403): {message}"),
        DomainError::Conflict { message, .. } => format!("conflict (409): {message}"),
        DomainError::Database(error) => format!("database error: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn household(name: &str) -> Household {
        Household {
            id: Uuid::now_v7(),
            name: name.into(),
            timezone: "Europe/Moscow".into(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            deleted_at: None,
        }
    }

    #[test]
    fn implicit_household_must_be_unique() {
        assert!(select_household(Vec::new()).is_err());
        let single = household("A");
        assert_eq!(
            select_household(vec![single.clone()]).expect("single").id,
            single.id
        );
        assert!(select_household(vec![household("A"), household("B")]).is_err());
    }

    #[test]
    fn errors_carry_http_like_codes() {
        assert!(describe(&DomainError::conflict("x")).contains("409"));
        assert!(describe(&DomainError::NotFound).contains("404"));
    }
}
