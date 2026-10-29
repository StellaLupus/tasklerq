//! Репозиторий тегов.

use sqlx::PgConnection;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::models::Tag;
use crate::domain::validation::{MAX_TAG, normalize_text};

/// Найти или создать тег household. Имя уже должно быть в нижнем регистре.
pub async fn ensure_tag(
    conn: &mut PgConnection,
    household_id: Uuid,
    name: &str,
) -> Result<Tag, DomainError> {
    let name = normalize_text(name, MAX_TAG, "tag")?.to_lowercase();

    let tag = sqlx::query_as::<_, Tag>(
        "INSERT INTO tags (id, household_id, name)
         VALUES ($1, $2, $3)
         ON CONFLICT (household_id, lower(name))
         DO UPDATE SET name = tags.name
         RETURNING id, household_id, name, created_at",
    )
    .bind(Uuid::now_v7())
    .bind(household_id)
    .bind(&name)
    .fetch_one(conn)
    .await?;

    Ok(tag)
}

/// Заменить набор тегов элемента.
pub async fn set_item_tags(
    conn: &mut PgConnection,
    household_id: Uuid,
    item_id: Uuid,
    names: &[String],
) -> Result<Vec<Tag>, DomainError> {
    sqlx::query("DELETE FROM item_tags WHERE item_id = $1")
        .bind(item_id)
        .execute(&mut *conn)
        .await?;

    let mut tags = Vec::with_capacity(names.len());
    for name in names {
        let tag = ensure_tag(conn, household_id, name).await?;
        sqlx::query(
            "INSERT INTO item_tags (item_id, tag_id) VALUES ($1, $2)
             ON CONFLICT DO NOTHING",
        )
        .bind(item_id)
        .bind(tag.id)
        .execute(&mut *conn)
        .await?;
        tags.push(tag);
    }

    tags.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(tags)
}

/// Теги элемента.
pub async fn item_tags(conn: &mut PgConnection, item_id: Uuid) -> Result<Vec<Tag>, DomainError> {
    let tags = sqlx::query_as::<_, Tag>(
        "SELECT t.id, t.household_id, t.name, t.created_at
         FROM tags t
         JOIN item_tags it ON it.tag_id = t.id
         WHERE it.item_id = $1
         ORDER BY t.name",
    )
    .bind(item_id)
    .fetch_all(conn)
    .await?;

    Ok(tags)
}
