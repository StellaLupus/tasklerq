//! Интеграционные тесты жизненного цикла элементов.

mod support;

use chrono::{Datelike, Duration, SubsecRound, Utc};
use tasklerq::domain::error::DomainError;
use tasklerq::domain::models::{EventType, ItemPatch, ItemStatus, ListKind};
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
async fn soft_delete_is_reverted_by_undo() {
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
    assert_eq!(undone.version, deleted.version + 1);
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undo_is_limited_to_the_actor_or_the_owner() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let mut conn = db.pool.acquire().await.expect("connection");

    let by_owner = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        support::new_item(household.tasks_list_id, "Создал владелец"),
    )
    .await;
    let by_member = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.member(),
        support::new_item(household.tasks_list_id, "Создал участник"),
    )
    .await;

    // Участник не может отменить чужое действие, и элемент остаётся на месте.
    assert!(matches!(
        items::undo_last(
            &mut conn,
            household.household_id,
            &household.member(),
            by_owner.id,
            Utc::now(),
        )
        .await,
        Err(DomainError::Forbidden(_))
    ));
    assert!(
        items::get(&mut conn, household.household_id, by_owner.id)
            .await
            .is_ok()
    );

    // Автор отменяет собственное действие.
    let undone = items::undo_last(
        &mut conn,
        household.household_id,
        &household.member(),
        by_member.id,
        Utc::now(),
    )
    .await
    .expect("actor can undo own action");
    assert!(undone.deleted_at.is_some());

    // Владелец может отменить действие участника.
    let by_member_again = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.member(),
        support::new_item(household.tasks_list_id, "Ещё от участника"),
    )
    .await;
    let undone = items::undo_last(
        &mut conn,
        household.household_id,
        &household.owner(),
        by_member_again.id,
        Utc::now(),
    )
    .await
    .expect("owner can undo any action");
    assert!(undone.deleted_at.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn assignee_must_belong_to_the_household() {
    use axum::http::{Method, StatusCode};
    use serde_json::json;

    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let foreign = support::create_household(&db.pool).await;
    let item = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        support::new_item(household.tasks_list_id, "Назначение"),
    )
    .await;

    let mut conn = db.pool.acquire().await.expect("connection");
    for stranger in [foreign.owner_id, foreign.member_id, uuid::Uuid::now_v7()] {
        let result = items::update(
            &mut conn,
            household.household_id,
            &household.owner(),
            item.id,
            item.version,
            ItemPatch {
                assignee_user_id: tasklerq::domain::models::Field::Value(stranger),
                ..ItemPatch::default()
            },
            "Europe/Moscow",
            Utc::now(),
        )
        .await;
        assert!(
            matches!(result, Err(DomainError::Validation(_))),
            "update must reject assignee {stranger}, got {result:?}"
        );
    }

    let token = support::issue_token(&db.pool, household.household_id, &["ha:write"]).await;
    let app = support::app(&db.pool);
    let (status, _) = support::request(
        &app,
        Method::POST,
        &format!("/api/v1/lists/{}/items", household.tasks_list_id),
        Some(&token),
        Some(json!({"title": "Чужому", "assignee_user_id": foreign.owner_id})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // Свой участник по-прежнему назначается.
    let assigned = items::update(
        &mut conn,
        household.household_id,
        &household.owner(),
        item.id,
        item.version,
        ItemPatch {
            assignee_user_id: tasklerq::domain::models::Field::Value(household.member_id),
            ..ItemPatch::default()
        },
        "Europe/Moscow",
        Utc::now(),
    )
    .await
    .expect("own member can be assigned");
    assert_eq!(assigned.assignee_user_id, Some(household.member_id));
}

/// Записи выполнений списка с начала суток до `now`.
async fn completions(
    conn: &mut sqlx::PgConnection,
    household: &support::TestHousehold,
    since: chrono::DateTime<Utc>,
) -> Vec<items::completions::Completion> {
    items::completions::for_list_since(conn, household.household_id, household.tasks_list_id, since)
        .await
        .expect("completions")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undone_completion_is_not_listed() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let created = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        support::new_item(household.tasks_list_id, "Вынести мусор"),
    )
    .await;
    let now = Utc::now().trunc_subsecs(6);
    let mut conn = db.pool.acquire().await.expect("connection");

    let completed = items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Some(created.version),
        "Europe/Moscow",
        now,
    )
    .await
    .expect("complete");
    assert_eq!(
        completions(&mut conn, &household, now - Duration::days(1))
            .await
            .len(),
        1
    );

    items::undo_last(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        now + Duration::seconds(1),
    )
    .await
    .expect("undo");

    assert!(
        completions(&mut conn, &household, now - Duration::days(1))
            .await
            .is_empty(),
        "an undone completion must disappear"
    );
    assert_eq!(
        completed.completed_at,
        Some(now),
        "completion is recorded at the actual moment"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undoing_a_rename_does_not_add_a_completion() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let created = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        support::new_item(household.tasks_list_id, "Вынести мусор"),
    )
    .await;
    let now = Utc::now().trunc_subsecs(6);
    let mut conn = db.pool.acquire().await.expect("connection");

    let completed = items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Some(created.version),
        "Europe/Moscow",
        now,
    )
    .await
    .expect("complete");
    items::update(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        completed.version,
        ItemPatch {
            title: Some("Вынести мусор и картон".into()),
            ..ItemPatch::default()
        },
        "Europe/Moscow",
        now + Duration::seconds(1),
    )
    .await
    .expect("rename");
    items::undo_last(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        now + Duration::seconds(2),
    )
    .await
    .expect("undo rename");

    let listed = completions(&mut conn, &household, now - Duration::days(1)).await;
    assert_eq!(
        listed.len(),
        1,
        "the compensating event is not a completion"
    );
    assert_eq!(listed[0].completed_at, now);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undoing_a_delete_does_not_add_a_completion() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let created = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        support::new_item(household.tasks_list_id, "Вынести мусор"),
    )
    .await;
    let now = Utc::now().trunc_subsecs(6);
    let mut conn = db.pool.acquire().await.expect("connection");

    items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Some(created.version),
        "Europe/Moscow",
        now,
    )
    .await
    .expect("complete");
    items::soft_delete(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        now + Duration::seconds(1),
    )
    .await
    .expect("delete");
    items::undo_last(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        now + Duration::seconds(2),
    )
    .await
    .expect("undo delete");

    let listed = completions(&mut conn, &household, now - Duration::days(1)).await;
    assert_eq!(
        listed.len(),
        1,
        "the compensating event is not a completion"
    );
    assert_eq!(listed[0].completed_at, now);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undoing_a_reopen_restores_the_original_completion() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let created = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        support::new_item(household.tasks_list_id, "Вынести мусор"),
    )
    .await;
    let now = Utc::now().trunc_subsecs(6);
    let mut conn = db.pool.acquire().await.expect("connection");

    let completed = items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Some(created.version),
        "Europe/Moscow",
        now,
    )
    .await
    .expect("complete");
    items::reopen(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Some(completed.version),
        now + Duration::seconds(1),
    )
    .await
    .expect("reopen");
    items::undo_last(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        now + Duration::seconds(2),
    )
    .await
    .expect("undo reopen");

    let listed = completions(&mut conn, &household, now - Duration::days(1)).await;
    assert_eq!(listed.len(), 1);
    assert_eq!(
        listed[0].completed_at, now,
        "the original completion keeps its time"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reopen_after_undoing_a_rename_hides_the_old_completion() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let created = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        support::new_item(household.tasks_list_id, "Вынести мусор"),
    )
    .await;
    let now = Utc::now().trunc_subsecs(6);
    let mut conn = db.pool.acquire().await.expect("connection");

    let completed = items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Some(created.version),
        "Europe/Moscow",
        now,
    )
    .await
    .expect("complete");
    items::update(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        completed.version,
        ItemPatch {
            title: Some("Вынести мусор и картон".into()),
            ..ItemPatch::default()
        },
        "Europe/Moscow",
        now + Duration::seconds(1),
    )
    .await
    .expect("rename");
    let restored = items::undo_last(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        now + Duration::seconds(2),
    )
    .await
    .expect("undo rename");
    items::reopen(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Some(restored.version),
        now + Duration::seconds(3),
    )
    .await
    .expect("reopen");

    assert!(
        completions(&mut conn, &household, now - Duration::days(1))
            .await
            .is_empty(),
        "a compensating completed event must not mask the reopen"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn second_completion_after_reopen_is_listed() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let created = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        support::new_item(household.tasks_list_id, "Вынести мусор"),
    )
    .await;
    let now = Utc::now().trunc_subsecs(6);
    let mut conn = db.pool.acquire().await.expect("connection");

    let completed = items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Some(created.version),
        "Europe/Moscow",
        now,
    )
    .await
    .expect("first completion");
    let reopened = items::reopen(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Some(completed.version),
        now + Duration::seconds(1),
    )
    .await
    .expect("reopen");
    items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Some(reopened.version),
        "Europe/Moscow",
        now + Duration::seconds(2),
    )
    .await
    .expect("second completion");

    let listed = completions(&mut conn, &household, now - Duration::days(1)).await;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].completed_at, now + Duration::seconds(2));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completion_list_id_prefers_the_snapshot_and_falls_back_to_the_item() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let created = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        support::new_item(household.tasks_list_id, "Вынести мусор"),
    )
    .await;
    let now = Utc::now().trunc_subsecs(6);
    let mut conn = db.pool.acquire().await.expect("connection");

    items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Some(created.version),
        "Europe/Moscow",
        now,
    )
    .await
    .expect("complete");

    sqlx::query("UPDATE items SET list_id = $2 WHERE id = $1")
        .bind(created.id)
        .bind(household.shopping_list_id)
        .execute(&mut *conn)
        .await
        .expect("move the item");

    let tasks = items::completions::for_list_since(
        &mut conn,
        household.household_id,
        household.tasks_list_id,
        now - Duration::days(1),
    )
    .await
    .expect("tasks completions");
    assert_eq!(
        tasks.len(),
        1,
        "the snapshot list wins over the current item list"
    );
    let shopping = items::completions::for_list_since(
        &mut conn,
        household.household_id,
        household.shopping_list_id,
        now - Duration::days(1),
    )
    .await
    .expect("shopping completions");
    assert!(shopping.is_empty());

    sqlx::query(
        "UPDATE item_events
         SET payload = jsonb_set(payload, '{before,list_id}', '\"not-a-uuid\"'::jsonb)
         WHERE id = $1",
    )
    .bind(tasks[0].event_id)
    .execute(&mut *conn)
    .await
    .expect("break the snapshot list");

    let shopping = items::completions::for_list_since(
        &mut conn,
        household.household_id,
        household.shopping_list_id,
        now - Duration::days(1),
    )
    .await
    .expect("fallback completions");
    assert_eq!(shopping.len(), 1, "a broken snapshot list falls back");

    sqlx::query(
        "UPDATE item_events
         SET payload = jsonb_set(payload, '{before}', (payload->'before') - 'list_id')
         WHERE id = $1",
    )
    .bind(tasks[0].event_id)
    .execute(&mut *conn)
    .await
    .expect("drop the snapshot list");

    let shopping = items::completions::for_list_since(
        &mut conn,
        household.household_id,
        household.shopping_list_id,
        now - Duration::days(1),
    )
    .await
    .expect("fallback completions");
    assert_eq!(shopping.len(), 1, "a missing snapshot list falls back");
    let tasks = items::completions::for_list_since(
        &mut conn,
        household.household_id,
        household.tasks_list_id,
        now - Duration::days(1),
    )
    .await
    .expect("tasks without snapshot list");
    assert!(tasks.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reopened_completion_without_undo_link_is_not_listed() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let created = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        support::new_item(household.tasks_list_id, "Вынести мусор"),
    )
    .await;
    let now = Utc::now();
    let mut conn = db.pool.acquire().await.expect("connection");

    let completed = items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Some(created.version),
        "Europe/Moscow",
        now,
    )
    .await
    .expect("complete");
    items::reopen(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Some(completed.version),
        now + Duration::seconds(1),
    )
    .await
    .expect("reopen");

    let events = tasklerq::storage::repositories::events::for_item(
        &mut conn,
        household.household_id,
        created.id,
    )
    .await
    .expect("events");
    let reopened = events
        .iter()
        .find(|event| event.event_type == EventType::Reopened)
        .expect("reopened event");
    assert!(
        reopened.undo_of.is_none(),
        "reopen still writes no undo link"
    );

    assert!(
        completions(&mut conn, &household, now - Duration::days(1))
            .await
            .is_empty(),
        "a completion returned to work must disappear"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completion_keeps_the_title_from_the_moment_of_completion() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let created = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        support::new_item(household.tasks_list_id, "Полить цветы"),
    )
    .await;
    let now = Utc::now();
    let mut conn = db.pool.acquire().await.expect("connection");

    let completed = items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Some(created.version),
        "Europe/Moscow",
        now,
    )
    .await
    .expect("complete");
    items::update(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        completed.version,
        ItemPatch {
            title: Some("Полить цветы на балконе".into()),
            ..ItemPatch::default()
        },
        "Europe/Moscow",
        now + Duration::seconds(1),
    )
    .await
    .expect("rename");

    let listed = completions(&mut conn, &household, now - Duration::days(1)).await;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].title, "Полить цветы");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completion_snapshot_without_title_is_tolerated() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let mut input = support::new_item(household.tasks_list_id, "Вынести мусор");
    input.due_at = Some(Utc::now() + Duration::hours(2));
    let created = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        input,
    )
    .await;
    let now = Utc::now();
    let mut conn = db.pool.acquire().await.expect("connection");

    let completed = items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Some(created.version),
        "Europe/Moscow",
        now,
    )
    .await
    .expect("complete");

    sqlx::query(
        "UPDATE item_events
         SET payload = jsonb_set(
             payload,
             '{before}',
             ((payload->'before') - 'title') || '{\"due_at\": \"not-a-date\"}'::jsonb
         )
         WHERE item_id = $1 AND event_type = 'completed'",
    )
    .bind(created.id)
    .execute(&mut *conn)
    .await
    .expect("break the snapshot");

    let listed = completions(&mut conn, &household, now - Duration::days(1)).await;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].title, items::completions::UNTITLED);
    assert_eq!(listed[0].due_at, None, "a broken due date is tolerated");
    assert_eq!(listed[0].item_id, completed.id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deleted_item_completions_are_not_listed() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let created = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        support::new_item(household.tasks_list_id, "Вынести мусор"),
    )
    .await;
    let now = Utc::now();
    let mut conn = db.pool.acquire().await.expect("connection");

    items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Some(created.version),
        "Europe/Moscow",
        now,
    )
    .await
    .expect("complete");
    items::soft_delete(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        now + Duration::seconds(1),
    )
    .await
    .expect("delete");

    assert!(
        completions(&mut conn, &household, now - Duration::days(1))
            .await
            .is_empty(),
        "deleted items must not contribute completions"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completions_are_filtered_by_list() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let now = Utc::now();
    let mut conn = db.pool.acquire().await.expect("connection");

    let task = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        support::new_item(household.tasks_list_id, "Задача"),
    )
    .await;
    let purchase = support::create_item(
        &db.pool,
        &household,
        household.shopping_list_id,
        &household.owner(),
        support::new_item(household.shopping_list_id, "Молоко"),
    )
    .await;
    let task_id = task.id;
    for item in [task, purchase] {
        items::complete(
            &mut conn,
            household.household_id,
            &household.owner(),
            item.id,
            Some(item.version),
            "Europe/Moscow",
            now,
        )
        .await
        .expect("complete");
    }

    let listed = completions(&mut conn, &household, now - Duration::days(1)).await;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].item_id, task_id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recurring_completions_keep_their_planned_due_dates() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let today = Utc::now().date_naive();
    let mut input = support::new_item(household.tasks_list_id, "Полить цветы");
    input.due_date = Some(today);
    input.recurrence = Some(support::recurrence(
        tasklerq::domain::models::RecurrenceFreq::Daily,
        Vec::new(),
    ));
    let created = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        input,
    )
    .await;
    let now = Utc::now();
    let mut conn = db.pool.acquire().await.expect("connection");

    let first = items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Some(created.version),
        "Europe/Moscow",
        now,
    )
    .await
    .expect("first completion");
    assert_eq!(first.status, ItemStatus::Open);

    // Второе вхождение: следующее вхождение уже наступило. Возвращаем срок и
    // снимаем отметку, чтобы `complete` записал ещё одно выполнение.
    let second_due = today + Duration::days(1);
    sqlx::query("UPDATE items SET completed_at = NULL, due_date = $2 WHERE id = $1")
        .bind(created.id)
        .bind(second_due)
        .execute(&mut *conn)
        .await
        .expect("prepare second occurrence");
    items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        None,
        "Europe/Moscow",
        now,
    )
    .await
    .expect("second completion");

    let listed = completions(&mut conn, &household, now - Duration::days(1)).await;
    assert_eq!(listed.len(), 2);
    let mut due_dates: Vec<_> = listed.iter().map(|entry| entry.due_date).collect();
    due_dates.sort();
    assert_eq!(due_dates, vec![Some(today), Some(second_due)]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reopen_after_the_last_occurrence_keeps_the_earlier_completion() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let today = Utc::now().date_naive();
    let tomorrow = today + Duration::days(1);
    let now = Utc::now().trunc_subsecs(6);
    let mut input = support::new_item(household.tasks_list_id, "Полить цветы");
    input.due_date = Some(today);
    let mut rule = support::recurrence(tasklerq::domain::models::RecurrenceFreq::Daily, Vec::new());
    rule.until = Some(tomorrow);
    input.recurrence = Some(rule);
    let created = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        input,
    )
    .await;
    let mut conn = db.pool.acquire().await.expect("connection");

    // Первое вхождение: серия продолжается, задача остаётся открытой.
    let first = items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Some(created.version),
        "Europe/Moscow",
        now,
    )
    .await
    .expect("complete the first occurrence");
    assert_eq!(first.status, ItemStatus::Open);

    // Последнее вхождение: следующего нет, задача становится выполненной.
    let second_at = now + Duration::days(1);
    let second = items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Some(first.version),
        "Europe/Moscow",
        second_at,
    )
    .await
    .expect("complete the last occurrence");
    assert_eq!(second.status, ItemStatus::Done);

    items::reopen(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Some(second.version),
        second_at + Duration::seconds(1),
    )
    .await
    .expect("reopen the last occurrence");

    let listed = completions(&mut conn, &household, now - Duration::days(1)).await;
    assert_eq!(
        listed.len(),
        1,
        "reopen hides only the last completion, not the earlier one"
    );
    assert_eq!(listed[0].completed_at, now);
}
