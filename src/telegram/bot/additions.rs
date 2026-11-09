//! Прямое добавление частей текста в список покупок.
//!
//! Одно ядро для бота (`intake_split`) и воркера (`finish_direct_split`):
//! каждая часть становится позицией с количеством 1, повторы существующих
//! позиций собираются в выборы дубликатов, слишком длинные части пропускаются
//! и возвращаются вызывающему для баннера.

use chrono::{DateTime, NaiveDate, Utc};
use chrono_tz::Tz;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::models::{Actor, Item, ListKind};
use crate::domain::validation::validate_new_item;
use crate::storage::repositories::items;

use super::model::{DuplicateChoice, ItemDraft, merge_duplicate_choice, plan_additions};

/// Итог добавления частей.
pub(crate) struct Added {
    /// Созданные позиции.
    pub created: Vec<Item>,
    /// Выборы дубликатов, накопленные за все части.
    pub duplicates: Vec<DuplicateChoice>,
    /// Части, не прошедшие проверку домена.
    pub skipped: Vec<String>,
}

/// Добавить части текста в список покупок.
///
/// Созданная позиция сразу попадает в существующие: повтор в том же тексте
/// предлагается как дубликат, а не сохраняется молча.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn add_parts(
    conn: &mut PgConnection,
    household_id: Uuid,
    actor: &Actor,
    list_id: Uuid,
    timezone: Tz,
    today: NaiveDate,
    now: DateTime<Utc>,
    parts: Vec<String>,
) -> Result<Added, DomainError> {
    let mut existing = items::list_by_list(&mut *conn, household_id, list_id).await?;
    let mut new_items = Vec::new();
    for title in parts {
        let draft = ItemDraft {
            title,
            quantity: Some(1.0),
            ..ItemDraft::default()
        };
        let mut new_item = draft.to_new_item(list_id, timezone, ListKind::Shopping)?;
        // Экран задаёт целевой список: деление не уводит покупку в другой.
        new_item.list_id = list_id;
        new_items.push(new_item);
    }

    let mut added = Added {
        created: Vec::new(),
        duplicates: Vec::new(),
        skipped: Vec::new(),
    };
    for new_item in new_items {
        let (to_create, found) = plan_additions(&existing, vec![new_item]);
        for choice in found {
            merge_duplicate_choice(&mut added.duplicates, choice);
        }
        for new_item in to_create {
            let assignee = new_item.assignee_user_id;
            let title = new_item.title.clone();
            let validated =
                match validate_new_item(ListKind::Shopping, timezone.name(), new_item, today) {
                    Ok(validated) => validated,
                    // Слишком длинную часть не теряем из-за остальных: покажем её
                    // в баннере.
                    Err(DomainError::Validation(_)) => {
                        added.skipped.push(title);
                        continue;
                    }
                    Err(error) => return Err(error),
                };
            let item = items::create(
                &mut *conn,
                household_id,
                actor,
                list_id,
                validated,
                assignee,
                now,
            )
            .await?;
            existing.push(item.clone());
            added.created.push(item);
        }
    }
    Ok(added)
}
