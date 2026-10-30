//! Репозиторий списков.

use chrono::{DateTime, Utc};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::models::{List, ListKind};
use crate::domain::validation::normalize_text;

/// Максимальное число списков в household, включая списки по умолчанию.
pub const MAX_LISTS: i64 = 20;

/// Все неудалённые списки household.
pub async fn list(conn: &mut PgConnection, household_id: Uuid) -> Result<Vec<List>, DomainError> {
    let lists = sqlx::query_as::<_, List>(
        "SELECT id, household_id, kind, name, is_default, is_archived, position,
                created_at, updated_at, deleted_at
         FROM lists
         WHERE household_id = $1 AND deleted_at IS NULL
         ORDER BY position, created_at, id",
    )
    .bind(household_id)
    .fetch_all(conn)
    .await?;

    Ok(lists)
}

/// Получить неудалённый список household.
pub async fn get(
    conn: &mut PgConnection,
    household_id: Uuid,
    list_id: Uuid,
) -> Result<List, DomainError> {
    get_with_deleted(conn, household_id, list_id, false).await
}

/// Получить список, скрывая архивные из клиентских операций.
pub async fn get_visible(
    conn: &mut PgConnection,
    household_id: Uuid,
    list_id: Uuid,
) -> Result<List, DomainError> {
    let list = get(conn, household_id, list_id).await?;
    if list.is_archived {
        return Err(DomainError::NotFound);
    }
    Ok(list)
}

async fn get_with_deleted(
    conn: &mut PgConnection,
    household_id: Uuid,
    list_id: Uuid,
    include_deleted: bool,
) -> Result<List, DomainError> {
    sqlx::query_as::<_, List>(
        "SELECT id, household_id, kind, name, is_default, is_archived, position,
                created_at, updated_at, deleted_at
         FROM lists
         WHERE id = $1 AND household_id = $2 AND ($3 OR deleted_at IS NULL)",
    )
    .bind(list_id)
    .bind(household_id)
    .bind(include_deleted)
    .fetch_optional(conn)
    .await?
    .ok_or(DomainError::NotFound)
}

/// Вид списка, включая мягко удалённый.
///
/// Нужен повторному подтверждению capture: его элементы переживают удаление
/// списка, и ответ обязан сохранить их исходный вид.
pub async fn kind(
    conn: &mut PgConnection,
    household_id: Uuid,
    list_id: Uuid,
) -> Result<Option<ListKind>, DomainError> {
    let kind = sqlx::query_scalar::<_, ListKind>(
        "SELECT kind FROM lists WHERE id = $1 AND household_id = $2",
    )
    .bind(list_id)
    .bind(household_id)
    .fetch_optional(conn)
    .await?;
    Ok(kind)
}

/// Создать дополнительный список.
///
/// Проверка лимита сериализуется блокировкой строки household (`FOR NO KEY
/// UPDATE`: создатели списков ждут друг друга, а проверки внешних ключей при
/// вставке элементов не блокируются): вызывающая
/// сторона обязана работать в транзакции (HTTP-слой всегда её открывает),
/// иначе параллельные create увидят одинаковый счётчик и обойдут лимит.
pub async fn create(
    conn: &mut PgConnection,
    household_id: Uuid,
    kind: ListKind,
    name: &str,
    now: DateTime<Utc>,
) -> Result<List, DomainError> {
    let name = normalize_text(name, 64, "list name")?;

    sqlx::query("SELECT id FROM households WHERE id = $1 FOR NO KEY UPDATE")
        .bind(household_id)
        .fetch_optional(&mut *conn)
        .await?
        .ok_or(DomainError::NotFound)?;

    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM lists WHERE household_id = $1 AND deleted_at IS NULL",
    )
    .bind(household_id)
    .fetch_one(&mut *conn)
    .await?;
    if count >= MAX_LISTS {
        return Err(DomainError::conflict(format!(
            "household cannot have more than {MAX_LISTS} lists"
        )));
    }

    let position: i32 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(position), -1) + 1
         FROM lists WHERE household_id = $1 AND deleted_at IS NULL",
    )
    .bind(household_id)
    .fetch_one(&mut *conn)
    .await?;

    let result = sqlx::query_as::<_, List>(
        "INSERT INTO lists (
             id, household_id, kind, name, is_default, is_archived, position,
             created_at, updated_at
         )
         VALUES ($1, $2, $3, $4, false, false, $5, $6, $6)
         RETURNING id, household_id, kind, name, is_default, is_archived, position,
                   created_at, updated_at, deleted_at",
    )
    .bind(Uuid::now_v7())
    .bind(household_id)
    .bind(kind)
    .bind(&name)
    .bind(position)
    .bind(now)
    .fetch_one(&mut *conn)
    .await;

    match result {
        Ok(list) => Ok(list),
        Err(error) if is_unique_violation(&error) => {
            Err(DomainError::conflict("list with this name already exists"))
        }
        Err(error) => Err(error.into()),
    }
}

/// Переименовать или архивировать список.
pub async fn update(
    conn: &mut PgConnection,
    household_id: Uuid,
    list_id: Uuid,
    name: Option<&str>,
    is_archived: Option<bool>,
    is_owner: bool,
    now: DateTime<Utc>,
) -> Result<List, DomainError> {
    let current = lock_list(conn, household_id, list_id).await?;

    if is_archived.is_some() && !is_owner {
        return Err(DomainError::Forbidden(
            "only the household owner can archive lists".into(),
        ));
    }
    if is_archived == Some(true) && current.is_default {
        return Err(DomainError::conflict("default list cannot be archived"));
    }

    let name = match name {
        Some(name) => normalize_text(name, 64, "list name")?,
        None => current.name.clone(),
    };

    let result = sqlx::query_as::<_, List>(
        "UPDATE lists
         SET name = $3,
             is_archived = COALESCE($4, is_archived),
             updated_at = $5
         WHERE id = $1 AND household_id = $2 AND deleted_at IS NULL
         RETURNING id, household_id, kind, name, is_default, is_archived, position,
                   created_at, updated_at, deleted_at",
    )
    .bind(list_id)
    .bind(household_id)
    .bind(&name)
    .bind(is_archived)
    .bind(now)
    .fetch_one(&mut *conn)
    .await;

    match result {
        Ok(list) => Ok(list),
        Err(error) if is_unique_violation(&error) => {
            Err(DomainError::conflict("list with this name already exists"))
        }
        Err(error) => Err(error.into()),
    }
}

/// Мягко удалить список и все его неудалённые позиции.
pub async fn delete(
    conn: &mut PgConnection,
    household_id: Uuid,
    list_id: Uuid,
    is_owner: bool,
    now: DateTime<Utc>,
) -> Result<List, DomainError> {
    let current = lock_list(conn, household_id, list_id).await?;

    if !is_owner {
        return Err(DomainError::Forbidden(
            "only the household owner can delete lists".into(),
        ));
    }
    if current.is_default {
        return Err(DomainError::conflict("default list cannot be deleted"));
    }

    sqlx::query(
        "UPDATE items
         SET deleted_at = $3, updated_at = $3, version = version + 1
         WHERE list_id = $1 AND household_id = $2 AND deleted_at IS NULL",
    )
    .bind(list_id)
    .bind(household_id)
    .bind(now)
    .execute(&mut *conn)
    .await?;

    let list = sqlx::query_as::<_, List>(
        "UPDATE lists
         SET deleted_at = $3, updated_at = $3
         WHERE id = $1 AND household_id = $2 AND deleted_at IS NULL
         RETURNING id, household_id, kind, name, is_default, is_archived, position,
                   created_at, updated_at, deleted_at",
    )
    .bind(list_id)
    .bind(household_id)
    .bind(now)
    .fetch_one(&mut *conn)
    .await?;

    Ok(list)
}

async fn lock_list(
    conn: &mut PgConnection,
    household_id: Uuid,
    list_id: Uuid,
) -> Result<List, DomainError> {
    sqlx::query_as::<_, List>(
        "SELECT id, household_id, kind, name, is_default, is_archived, position,
                created_at, updated_at, deleted_at
         FROM lists
         WHERE id = $1 AND household_id = $2 AND deleted_at IS NULL
         FOR UPDATE",
    )
    .bind(list_id)
    .bind(household_id)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(DomainError::NotFound)
}

fn is_unique_violation(error: &sqlx::Error) -> bool {
    matches!(
        error,
        sqlx::Error::Database(database) if database.code().as_deref() == Some("23505")
    )
}
