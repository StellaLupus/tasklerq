//! Интеграционные тесты репозиториев и транзакций.

mod support;

use chrono::Utc;
use tasklerq::domain::error::DomainError;
use tasklerq::domain::models::{ItemPriority, ItemStatus, ListKind, SubtaskInput};
use tasklerq::storage::repositories::{households, items, lists, tags};
use uuid::Uuid;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn household_creation_creates_owner_and_default_lists() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let mut conn = db.pool.acquire().await.expect("connection");

    let lists = lists::list(&mut conn, household.household_id)
        .await
        .expect("lists");
    assert_eq!(lists.len(), 2);
    assert!(
        lists
            .iter()
            .any(|list| list.kind == ListKind::Tasks && list.is_default)
    );
    assert!(
        lists
            .iter()
            .any(|list| list.kind == ListKind::Shopping && list.is_default)
    );

    let members = households::members(&mut conn, household.household_id)
        .await
        .expect("members");
    assert_eq!(members.len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transaction_rollback_leaves_no_household() {
    let db = support::db().await;
    let name = format!("Rollback {}", Uuid::now_v7());
    let mut tx = db.pool.begin().await.expect("transaction");

    let (household, _) = households::create_with_owner(
        &mut tx,
        &name,
        "Europe/Moscow",
        tasklerq::storage::repositories::households::NewOwner {
            telegram_user_id: 7_000_000_001,
            telegram_chat_id: 7_000_000_001,
            display_name: "Rollback Owner".into(),
            username: None,
            timezone: None,
        },
        Utc::now(),
    )
    .await
    .expect("household in transaction");
    tx.rollback().await.expect("rollback");

    let mut conn = db.pool.acquire().await.expect("connection");
    let exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM households WHERE id = $1)")
        .bind(household.id)
        .fetch_one(&mut *conn)
        .await
        .expect("query");
    assert!(!exists, "rolled back household must not be visible");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_limit_defaults_and_duplicate_names() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let mut conn = db.pool.acquire().await.expect("connection");

    for index in 0..18 {
        lists::create(
            &mut conn,
            household.household_id,
            if index % 2 == 0 {
                ListKind::Shopping
            } else {
                ListKind::Checklist
            },
            &format!("Список {index}"),
            Utc::now(),
        )
        .await
        .expect("list within limit");
    }

    let too_many = lists::create(
        &mut conn,
        household.household_id,
        ListKind::Checklist,
        "Двадцать первый",
        Utc::now(),
    )
    .await
    .expect_err("21st list must be rejected");
    assert!(matches!(too_many, DomainError::Conflict { .. }));

    let default_list = lists::list(&mut conn, household.household_id)
        .await
        .expect("lists")
        .into_iter()
        .find(|list| list.is_default)
        .expect("default");
    let delete_default = lists::delete(
        &mut conn,
        household.household_id,
        default_list.id,
        true,
        Utc::now(),
    )
    .await
    .expect_err("default list cannot be deleted");
    assert!(matches!(delete_default, DomainError::Conflict { .. }));

    let archive_default = lists::update(
        &mut conn,
        household.household_id,
        default_list.id,
        None,
        Some(true),
        true,
        Utc::now(),
    )
    .await
    .expect_err("default list cannot be archived");
    assert!(matches!(archive_default, DomainError::Conflict { .. }));

    let renamed = lists::update(
        &mut conn,
        household.household_id,
        default_list.id,
        Some("Продукты"),
        None,
        false,
        Utc::now(),
    )
    .await
    .expect("default list can be renamed");
    assert_eq!(renamed.name, "Продукты");

    let duplicate = lists::create(
        &mut conn,
        household.household_id,
        ListKind::Checklist,
        "продукты",
        Utc::now(),
    )
    .await
    .expect_err("duplicate name");
    assert!(matches!(duplicate, DomainError::Conflict { .. }));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tags_are_reused_case_insensitively() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let mut item = support::new_item(household.tasks_list_id, "Задача с тегами");
    item.tags = vec!["Дом".into(), "дом".into(), "срочно".into()];

    let item = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        item,
    )
    .await;
    assert_eq!(
        item.tags
            .iter()
            .map(|tag| tag.name.as_str())
            .collect::<Vec<_>>(),
        vec!["дом", "срочно"]
    );

    let mut second = support::new_item(household.tasks_list_id, "Вторая задача");
    second.tags = vec!["ДОМ".into()];
    let second = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.member(),
        second,
    )
    .await;

    let mut conn = db.pool.acquire().await.expect("connection");
    let tag_count: i64 = sqlx::query_scalar("SELECT count(*) FROM tags WHERE household_id = $1")
        .bind(household.household_id)
        .fetch_one(&mut *conn)
        .await
        .expect("tag count");
    assert_eq!(tag_count, 2);
    assert_eq!(second.tags.len(), 1);
    assert_eq!(second.tags[0].id, item.tags[0].id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subtask_progress_and_full_replacement() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let mut item = support::new_item(household.tasks_list_id, "Задача с подзадачами");
    item.priority = ItemPriority::High;
    item.subtasks = vec![
        SubtaskInput {
            title: "Первая".into(),
            status: Some(ItemStatus::Done),
        },
        SubtaskInput {
            title: "Вторая".into(),
            status: Some(ItemStatus::Open),
        },
    ];

    let item = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        item,
    )
    .await;
    assert_eq!(item.subtask_progress(), (1, 2));

    let patch = tasklerq::domain::models::ItemPatch {
        subtasks: Some(vec![support::subtask("Только одна")]),
        ..Default::default()
    };
    let mut conn = db.pool.acquire().await.expect("connection");
    let updated = items::update(
        &mut conn,
        household.household_id,
        &household.owner(),
        item.id,
        item.version,
        patch,
        "Europe/Moscow",
        Utc::now(),
    )
    .await
    .expect("update");
    assert_eq!(updated.subtasks.len(), 1);
    assert_eq!(updated.subtasks[0].title, "Только одна");
    assert_eq!(updated.priority, ItemPriority::High);

    let tags = tags::item_tags(&mut conn, item.id).await.expect("tags");
    assert!(tags.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_list_creates_respect_the_limit() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;

    // 2 списка по умолчанию + 17 дополнительных = 19: до лимита один слот.
    for index in 0..17 {
        let mut conn = db.pool.acquire().await.expect("connection");
        lists::create(
            &mut conn,
            household.household_id,
            ListKind::Checklist,
            &format!("Список {index}"),
            Utc::now(),
        )
        .await
        .expect("fill lists");
    }

    // Восемь параллельных create в отдельных транзакциях: ровно один должен
    // пройти, остальные — упереться в лимит 20.
    let household_id = household.household_id;
    let mut handles = Vec::new();
    for index in 0..8 {
        let pool = db.pool.clone();
        handles.push(tokio::spawn(async move {
            let mut tx = pool.begin().await.expect("transaction");
            let result = lists::create(
                &mut tx,
                household_id,
                ListKind::Checklist,
                &format!("Гонка {index}"),
                Utc::now(),
            )
            .await;
            match result {
                Ok(list) => {
                    tx.commit().await.expect("commit");
                    Ok(list)
                }
                Err(error) => {
                    tx.rollback().await.expect("rollback");
                    Err(error)
                }
            }
        }));
    }

    let mut created = 0;
    let mut conflicts = 0;
    for handle in handles {
        match handle.await.expect("join") {
            Ok(_) => created += 1,
            Err(DomainError::Conflict { .. }) => conflicts += 1,
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }
    assert_eq!(created, 1, "only one create must fit the last slot");
    assert_eq!(conflicts, 7);

    let mut conn = db.pool.acquire().await.expect("connection");
    let all = lists::list(&mut conn, household.household_id)
        .await
        .expect("lists");
    assert_eq!(
        all.len(),
        usize::try_from(lists::MAX_LISTS).expect("limit as usize")
    );
}
