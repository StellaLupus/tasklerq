//! Интеграционные тесты жизненного цикла элементов.

mod support;

use chrono::{Datelike, Duration, Utc};
use tasklerq::domain::error::DomainError;
use tasklerq::domain::models::{ItemPatch, ItemStatus, ListKind};
use tasklerq::storage::repositories::{items, lists};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn version_conflict_returns_current_version() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let created = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        support::new_item(household.tasks_list_id, "Версия"),
    )
    .await;
    assert_eq!(created.version, 1);

    let mut conn = db.pool.acquire().await.expect("connection");
    let patch = ItemPatch {
        title: Some("Новое имя".into()),
        ..ItemPatch::default()
    };
    let updated = items::update(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        created.version,
        patch.clone(),
        "Europe/Moscow",
        Utc::now(),
    )
    .await
    .expect("first update should win");
    assert_eq!(updated.version, 2);
    assert_eq!(updated.title, "Новое имя");

    let stale = items::update(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        created.version,
        patch,
        "Europe/Moscow",
        Utc::now(),
    )
    .await
    .expect_err("stale version must fail");
    match stale {
        DomainError::Conflict { details, .. } => {
            assert_eq!(details["current_version"], 2);
            assert_eq!(details["item"]["title"], "Новое имя");
        }
        other => panic!("expected conflict, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn complete_and_reopen_are_idempotent_and_restore_due() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let today = Utc::now().date_naive();
    let mut input = support::new_item(household.tasks_list_id, "Купить хлеб");
    input.due_date = Some(today);
    let created = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        input,
    )
    .await;

    let mut conn = db.pool.acquire().await.expect("connection");
    let completed = items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Some(created.version),
        "Europe/Moscow",
        Utc::now(),
    )
    .await
    .expect("complete");
    assert_eq!(completed.status, ItemStatus::Done);
    assert!(completed.completed_at.is_some());
    assert_eq!(completed.version, 2);

    let repeated = items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        None,
        "Europe/Moscow",
        Utc::now(),
    )
    .await
    .expect("repeated complete is idempotent");
    assert_eq!(repeated.version, 2);
    assert_eq!(repeated.completed_at, completed.completed_at);

    let reopened = items::reopen(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Some(repeated.version),
        Utc::now(),
    )
    .await
    .expect("reopen");
    assert_eq!(reopened.status, ItemStatus::Open);
    assert_eq!(reopened.due_date, Some(today));
    assert!(reopened.completed_at.is_none());
    assert_eq!(reopened.version, 3);

    let repeated_reopen = items::reopen(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        None,
        Utc::now(),
    )
    .await
    .expect("repeated reopen is idempotent");
    assert_eq!(repeated_reopen.version, 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recurring_completion_moves_due_date_forward_once() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let today = Utc::now().date_naive();
    let weekday = today.weekday().number_from_monday() as i16;
    let mut input = support::new_item(household.tasks_list_id, "Полить цветы");
    input.due_date = Some(today);
    input.recurrence = Some(support::recurrence(
        tasklerq::domain::models::RecurrenceFreq::Weekly,
        vec![weekday],
    ));
    let created = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        input,
    )
    .await;

    let mut conn = db.pool.acquire().await.expect("connection");
    let completed = items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        None,
        "Europe/Moscow",
        Utc::now(),
    )
    .await
    .expect("complete recurring");
    assert_eq!(completed.status, ItemStatus::Open);
    assert!(completed.completed_at.is_some());
    assert_eq!(completed.due_date, Some(today + Duration::days(7)));
    assert_eq!(completed.version, 2);

    let repeated = items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        None,
        "Europe/Moscow",
        Utc::now(),
    )
    .await
    .expect("repeated complete");
    assert_eq!(repeated.version, 2);
    assert_eq!(repeated.due_date, Some(today + Duration::days(7)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_restore_and_undo_round_trip() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let created = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        support::new_item(household.tasks_list_id, "Удалить меня"),
    )
    .await;

    let mut conn = db.pool.acquire().await.expect("connection");
    let deleted = items::soft_delete(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Utc::now(),
    )
    .await
    .expect("soft delete");
    assert!(deleted.deleted_at.is_some());
    assert_eq!(deleted.version, 2);
    assert!(matches!(
        items::get(&mut conn, household.household_id, created.id).await,
        Err(DomainError::NotFound)
    ));

    let restored = items::restore(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Utc::now(),
    )
    .await
    .expect("restore");
    assert!(restored.deleted_at.is_none());
    assert_eq!(restored.version, 3);

    let deleted_again = items::soft_delete(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Utc::now(),
    )
    .await
    .expect("delete again");
    let undone = items::undo_last(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Utc::now(),
    )
    .await
    .expect("undo delete");
    assert!(undone.deleted_at.is_none());
    assert_eq!(undone.version, deleted_again.version + 1);
    assert_eq!(undone.title, "Удалить меня");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undo_creation_soft_deletes_item() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let created = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        support::new_item(household.tasks_list_id, "Случайная запись"),
    )
    .await;

    let mut conn = db.pool.acquire().await.expect("connection");
    let undone = items::undo_last(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Utc::now(),
    )
    .await
    .expect("undo create");
    assert!(undone.deleted_at.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undo_complete_and_edit_restore_previous_state() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let today = Utc::now().date_naive();
    let mut input = support::new_item(household.tasks_list_id, "Исходное название");
    input.due_date = Some(today);
    let created = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        input,
    )
    .await;

    let mut conn = db.pool.acquire().await.expect("connection");
    let edited = items::update(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        created.version,
        ItemPatch {
            title: Some("Изменённое название".into()),
            ..ItemPatch::default()
        },
        "Europe/Moscow",
        Utc::now(),
    )
    .await
    .expect("edit");
    assert_eq!(edited.title, "Изменённое название");

    let undone_edit = items::undo_last(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Utc::now(),
    )
    .await
    .expect("undo edit");
    assert_eq!(undone_edit.title, "Исходное название");
    assert_eq!(undone_edit.version, edited.version + 1);

    let completed = items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        None,
        "Europe/Moscow",
        Utc::now(),
    )
    .await
    .expect("complete");
    assert_eq!(completed.status, ItemStatus::Done);

    let undone_complete = items::undo_last(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Utc::now(),
    )
    .await
    .expect("undo complete");
    assert_eq!(undone_complete.status, ItemStatus::Open);
    assert!(undone_complete.completed_at.is_none());
    assert_eq!(undone_complete.due_date, Some(today));
    assert_eq!(undone_complete.version, completed.version + 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeated_undo_does_not_roll_back_an_earlier_action() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let created = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        support::new_item(household.tasks_list_id, "Одна отмена"),
    )
    .await;

    let mut conn = db.pool.acquire().await.expect("connection");
    let completed = items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        None,
        "Europe/Moscow",
        Utc::now(),
    )
    .await
    .expect("complete");
    let undone = items::undo_last(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Utc::now(),
    )
    .await
    .expect("first undo");
    assert_eq!(undone.status, ItemStatus::Open);

    let second = items::undo_last(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Utc::now(),
    )
    .await
    .expect_err("second undo must not revert the create");
    assert!(matches!(second, DomainError::Conflict { .. }));
    let current = items::get(&mut conn, household.household_id, created.id)
        .await
        .expect("item stays");
    assert_eq!(current.status, ItemStatus::Open);
    assert!(current.deleted_at.is_none());
    assert_eq!(current.version, completed.version + 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_undo_window_is_rejected() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let created = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        support::new_item(household.tasks_list_id, "Поздно отменять"),
    )
    .await;

    let mut conn = db.pool.acquire().await.expect("connection");
    let error = items::undo_last(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Utc::now() + Duration::minutes(items::UNDO_WINDOW_MINUTES + 1),
    )
    .await
    .expect_err("undo window must expire");
    match error {
        DomainError::Conflict { message, details } => {
            assert!(message.contains("undo window"));
            assert!(details["event_id"].is_string());
        }
        other => panic!("expected conflict, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn member_cannot_delete_owner_item_and_foreign_household_is_hidden() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let foreign = support::create_household(&db.pool).await;
    let created = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        support::new_item(household.tasks_list_id, "Чужая задача"),
    )
    .await;

    let mut conn = db.pool.acquire().await.expect("connection");
    assert!(matches!(
        items::soft_delete(
            &mut conn,
            household.household_id,
            &household.member(),
            created.id,
            Utc::now(),
        )
        .await,
        Err(DomainError::Forbidden(_))
    ));
    assert!(matches!(
        items::get(&mut conn, foreign.household_id, created.id).await,
        Err(DomainError::NotFound)
    ));
    assert!(matches!(
        items::soft_delete(
            &mut conn,
            foreign.household_id,
            &foreign.owner(),
            created.id,
            Utc::now(),
        )
        .await,
        Err(DomainError::NotFound)
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restore_rejects_items_deleted_beyond_retention() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let created = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        support::new_item(household.tasks_list_id, "Старое удаление"),
    )
    .await;

    let mut conn = db.pool.acquire().await.expect("connection");
    let deleted_at = Utc::now() - Duration::days(items::DELETED_RETENTION_DAYS + 1);
    sqlx::query("UPDATE items SET deleted_at = $3 WHERE id = $1 AND household_id = $2")
        .bind(created.id)
        .bind(household.household_id)
        .bind(deleted_at)
        .execute(&mut *conn)
        .await
        .expect("backdate deletion");

    let error = items::restore(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Utc::now(),
    )
    .await
    .expect_err("retention must block restore");
    assert!(matches!(error, DomainError::Conflict { .. }));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undo_rejects_stale_event_after_list_deletion() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let mut conn = db.pool.acquire().await.expect("connection");

    // Дополнительный список, который можно удалить целиком.
    let list = lists::create(
        &mut conn,
        household.household_id,
        ListKind::Checklist,
        "Временный",
        Utc::now(),
    )
    .await
    .expect("list");

    let created = support::create_item(
        &db.pool,
        &household,
        list.id,
        &household.owner(),
        support::new_item(list.id, "Пропадёт со списком"),
    )
    .await;
    let edited = items::update(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        created.version,
        ItemPatch {
            title: Some("Изменённая".into()),
            ..ItemPatch::default()
        },
        "Europe/Moscow",
        Utc::now(),
    )
    .await
    .expect("edit");
    assert_eq!(edited.version, 2);

    // Удаление списка мягко удаляет элементы и поднимает version, не записывая
    // событий: последнее событие элемента устаревает.
    lists::delete(&mut conn, household.household_id, list.id, true, Utc::now())
        .await
        .expect("delete list");

    let error = items::undo_last(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Utc::now(),
    )
    .await
    .expect_err("undo must not resurrect an item in a deleted list");
    assert!(matches!(error, DomainError::Conflict { .. }));

    // Элемент остаётся удалённым вместе со списком.
    assert!(matches!(
        items::get(&mut conn, household.household_id, created.id).await,
        Err(DomainError::NotFound)
    ));
}
