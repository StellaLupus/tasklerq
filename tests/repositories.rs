//! Интеграционные тесты репозиториев и транзакций.

mod support;

use chrono::{Duration, NaiveDate, TimeZone, Utc};
use tasklerq::config::TenancyMode;
use tasklerq::domain::error::DomainError;
use tasklerq::domain::identity::NewMember;
use tasklerq::domain::models::{
    Field, ItemPriority, ItemStatus, ListKind, RecurrenceFreq, SubtaskInput,
};
use tasklerq::storage::repositories::households::{self, NewOwner};
use tasklerq::storage::repositories::{items, lists, tags, users};
use uuid::Uuid;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn household_creation_creates_owner_and_default_lists() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let mut conn = db.pool.acquire().await.expect("connection");

    let lists = lists::list(&mut conn, household.household_id)
        .await
        .expect("lists");
    assert_eq!(lists.len(), 3);
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
    assert!(
        lists
            .iter()
            .any(|list| list.kind == ListKind::Events && list.is_default)
    );

    let members = users::list_active(&mut conn, household.household_id)
        .await
        .expect("members");
    assert_eq!(members.len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn event_ends_are_stored_read_and_patched() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let actor = household.owner();

    let start = Utc
        .with_ymd_and_hms(2026, 11, 10, 9, 0, 0)
        .single()
        .expect("start");
    let end = Utc
        .with_ymd_and_hms(2026, 11, 10, 11, 30, 0)
        .single()
        .expect("end");
    let mut timed = support::new_item(household.events_list_id, "Совещание");
    timed.due_at = Some(start);
    timed.end_at = Some(end);
    let timed = support::create_item(
        &db.pool,
        &household,
        household.events_list_id,
        &actor,
        timed,
    )
    .await;
    assert_eq!(timed.due_at, Some(start));
    assert_eq!(timed.end_at, Some(end));
    assert_eq!(timed.end_date, None);

    let mut conn = db.pool.acquire().await.expect("connection");
    let timed_read = items::get(&mut conn, household.household_id, timed.id)
        .await
        .expect("timed event");
    assert_eq!(timed_read.due_at, Some(start));
    assert_eq!(timed_read.end_at, Some(end));
    assert_eq!(timed_read.end_date, None);

    let first_day = NaiveDate::from_ymd_opt(2026, 11, 20).expect("first day");
    let last_day = NaiveDate::from_ymd_opt(2026, 11, 22).expect("last day");
    let mut all_day = support::new_item(household.events_list_id, "Поездка");
    all_day.due_date = Some(first_day);
    all_day.end_date = Some(last_day);
    let all_day = support::create_item(
        &db.pool,
        &household,
        household.events_list_id,
        &actor,
        all_day,
    )
    .await;
    assert_eq!(all_day.due_date, Some(first_day));
    assert_eq!(all_day.end_date, Some(last_day));
    assert_eq!(all_day.end_at, None);

    let all_day_read = items::get(&mut conn, household.household_id, all_day.id)
        .await
        .expect("all-day event");
    assert_eq!(all_day_read.due_date, Some(first_day));
    assert_eq!(all_day_read.end_date, Some(last_day));
    assert_eq!(all_day_read.end_at, None);

    let new_last_day = NaiveDate::from_ymd_opt(2026, 11, 23).expect("new last day");
    let patch = tasklerq::domain::models::ItemPatch {
        end_date: Field::Value(new_last_day),
        ..Default::default()
    };
    let all_day_updated = items::update(
        &mut conn,
        household.household_id,
        &actor,
        all_day.id,
        all_day.version,
        patch,
        "Europe/Moscow",
        Utc::now(),
    )
    .await
    .expect("move all-day end");
    assert_eq!(all_day_updated.end_date, Some(new_last_day));
    assert_eq!(all_day_updated.end_at, None);

    let patch = tasklerq::domain::models::ItemPatch {
        end_date: Field::Null,
        ..Default::default()
    };
    let all_day_cleared = items::update(
        &mut conn,
        household.household_id,
        &actor,
        all_day.id,
        all_day_updated.version,
        patch,
        "Europe/Moscow",
        Utc::now(),
    )
    .await
    .expect("clear all-day end");
    assert_eq!(all_day_cleared.end_date, None);
    assert_eq!(all_day_cleared.end_at, None);

    let both_ends = tasklerq::domain::models::ItemPatch {
        end_at: Field::Value(end),
        end_date: Field::Value(new_last_day),
        ..Default::default()
    };
    let error = items::update(
        &mut conn,
        household.household_id,
        &actor,
        all_day.id,
        all_day_cleared.version,
        both_ends,
        "Europe/Moscow",
        Utc::now(),
    )
    .await
    .expect_err("end_at and end_date cannot be patched together");
    assert!(matches!(error, DomainError::Validation(_)), "{error:?}");

    let new_end = Utc
        .with_ymd_and_hms(2026, 11, 10, 12, 45, 0)
        .single()
        .expect("new end");
    let patch = tasklerq::domain::models::ItemPatch {
        end_at: Field::Value(new_end),
        ..Default::default()
    };
    let updated = items::update(
        &mut conn,
        household.household_id,
        &actor,
        timed.id,
        timed.version,
        patch,
        "Europe/Moscow",
        Utc::now(),
    )
    .await
    .expect("move end");
    assert_eq!(updated.end_at, Some(new_end));

    let patch = tasklerq::domain::models::ItemPatch {
        end_at: Field::Null,
        ..Default::default()
    };
    let cleared = items::update(
        &mut conn,
        household.household_id,
        &actor,
        timed.id,
        updated.version,
        patch,
        "Europe/Moscow",
        Utc::now(),
    )
    .await
    .expect("clear end");
    assert_eq!(cleared.end_at, None);
    assert_eq!(cleared.end_date, None);

    let cleared_read = items::get(&mut conn, household.household_id, timed.id)
        .await
        .expect("updated event");
    assert_eq!(cleared_read.end_at, None);
    assert_eq!(cleared_read.end_date, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transaction_rollback_leaves_no_household() {
    let db = support::db().await;
    let name = format!("Rollback {}", Uuid::now_v7());
    let mut tx = db.pool.begin().await.expect("transaction");

    let telegram_id = 7_000_000_001;
    let onboarded = households::onboard(
        &mut tx,
        TenancyMode::Multi,
        &name,
        "Europe/Moscow",
        NewOwner {
            member: NewMember {
                telegram_user_id: telegram_id,
                telegram_chat_id: telegram_id,
                display_name: "Rollback Owner".into(),
                username: None,
            },
            timezone: None,
        },
        Utc::now(),
    )
    .await
    .expect("household in transaction");
    tx.rollback().await.expect("rollback");

    let mut conn = db.pool.acquire().await.expect("connection");
    let exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM households WHERE id = $1)")
        .bind(onboarded.household.id)
        .fetch_one(&mut *conn)
        .await
        .expect("query");
    assert!(!exists, "rolled back household must not be visible");

    let owner_exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM users WHERE telegram_user_id = $1)")
            .bind(telegram_id)
            .fetch_one(&mut *conn)
            .await
            .expect("query");
    assert!(!owner_exists, "rolled back owner must not be visible");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_limit_defaults_and_duplicate_names() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let mut conn = db.pool.acquire().await.expect("connection");

    for index in 0..17 {
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

    // 3 списка по умолчанию + 16 дополнительных = 19: до лимита один слот.
    for index in 0..16 {
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_list_creation_respects_the_limit() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let mut conn = db.pool.acquire().await.expect("connection");

    // Три списка по умолчанию + 16 = 19: остаётся ровно одно место.
    for index in 0..16 {
        lists::create(
            &mut conn,
            household.household_id,
            ListKind::Shopping,
            &format!("Список {index}"),
            Utc::now(),
        )
        .await
        .expect("list within limit");
    }
    drop(conn);

    // Транзакция A занимает последнее место и пока не коммитится.
    let mut first = db.pool.begin().await.expect("transaction");
    lists::create(
        &mut first,
        household.household_id,
        ListKind::Shopping,
        "Гонка A",
        Utc::now(),
    )
    .await
    .expect("first create takes the last slot");

    // Транзакция B стартует параллельно и не видит незакоммиченный список A.
    let pool = db.pool.clone();
    let household_id = household.household_id;
    let second = tokio::spawn(async move {
        let mut tx = pool.begin().await.expect("transaction");
        let result = lists::create(
            &mut tx,
            household_id,
            ListKind::Shopping,
            "Гонка B",
            Utc::now(),
        )
        .await;
        if result.is_ok() {
            tx.commit().await.expect("commit");
        }
        result
    });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    first.commit().await.expect("commit");

    let second = second.await.expect("task");
    assert!(
        matches!(second, Err(DomainError::Conflict { .. })),
        "second concurrent create must be rejected, got {second:?}"
    );

    let mut conn = db.pool.acquire().await.expect("connection");
    let total = lists::list(&mut conn, household.household_id)
        .await
        .expect("lists")
        .len();
    assert_eq!(total, 20);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn batch_hydration_keeps_tags_and_subtasks_per_item() {
    use tasklerq::domain::search::SearchFilters;

    let db = support::db().await;
    let household = support::create_household(&db.pool).await;

    let fixtures: [(&str, &[&str], &[&str]); 3] = [
        ("Первая", &["b-тег", "a-тег"], &["шаг 1", "шаг 2"]),
        ("Вторая", &[], &[]),
        ("Третья", &["c-тег"], &["единственный шаг"]),
    ];
    for (title, tags, steps) in fixtures {
        let mut item = support::new_item(household.tasks_list_id, title);
        item.tags = tags.iter().map(|tag| (*tag).to_owned()).collect();
        item.subtasks = steps.iter().map(|step| support::subtask(step)).collect();
        support::create_item(
            &db.pool,
            &household,
            household.tasks_list_id,
            &household.owner(),
            item,
        )
        .await;
    }

    let mut conn = db.pool.acquire().await.expect("connection");
    let summarize = |items: &[tasklerq::domain::models::Item]| {
        let mut summary: Vec<(String, Vec<String>, Vec<String>)> = items
            .iter()
            .map(|item| {
                (
                    item.title.clone(),
                    item.tags.iter().map(|tag| tag.name.clone()).collect(),
                    item.subtasks
                        .iter()
                        .map(|subtask| subtask.title.clone())
                        .collect(),
                )
            })
            .collect();
        summary.sort();
        summary
    };
    let expected = vec![
        (
            "Вторая".to_owned(),
            Vec::<String>::new(),
            Vec::<String>::new(),
        ),
        (
            "Первая".to_owned(),
            vec!["a-тег".to_owned(), "b-тег".to_owned()],
            vec!["шаг 1".to_owned(), "шаг 2".to_owned()],
        ),
        (
            "Третья".to_owned(),
            vec!["c-тег".to_owned()],
            vec!["единственный шаг".to_owned()],
        ),
    ];

    let listed = items::list_by_list(&mut conn, household.household_id, household.tasks_list_id)
        .await
        .expect("list");
    assert_eq!(summarize(&listed), expected);

    let (snapshot, _) = items::snapshot(&mut conn, household.household_id, None)
        .await
        .expect("snapshot");
    assert_eq!(summarize(&snapshot), expected);

    let (hits, _) = items::search(
        &mut conn,
        household.household_id,
        "Europe/Moscow",
        &SearchFilters::default(),
    )
    .await
    .expect("search");
    let found: Vec<_> = hits.into_iter().map(|hit| hit.item).collect();
    assert_eq!(summarize(&found), expected);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn menu_counts_use_local_today_and_completion_events() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let actor = household.owner();
    let tz = chrono_tz::Tz::Asia__Vladivostok;
    let now = tz
        .with_ymd_and_hms(2027, 3, 16, 1, 0, 0)
        .single()
        .expect("local now")
        .with_timezone(&Utc);
    let today = NaiveDate::from_ymd_opt(2027, 3, 16).expect("today");
    let yesterday = NaiveDate::from_ymd_opt(2027, 3, 15).expect("yesterday");
    let tomorrow = NaiveDate::from_ymd_opt(2027, 3, 17).expect("tomorrow");

    let mut later_today = support::new_item(household.tasks_list_id, "Сегодня позже");
    later_today.due_at = Some(
        tz.with_ymd_and_hms(2027, 3, 16, 2, 0, 0)
            .single()
            .expect("timed due")
            .with_timezone(&Utc),
    );
    support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &actor,
        later_today,
    )
    .await;
    let mut today_without_time = support::new_item(household.tasks_list_id, "Сегодня без времени");
    today_without_time.due_date = Some(today);
    support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &actor,
        today_without_time,
    )
    .await;
    let mut overdue = support::new_item(household.tasks_list_id, "Вчера");
    overdue.due_date = Some(yesterday);
    let overdue = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &actor,
        overdue,
    )
    .await;
    let mut later = support::new_item(household.tasks_list_id, "Завтра");
    later.due_date = Some(tomorrow);
    support::create_item(&db.pool, &household, household.tasks_list_id, &actor, later).await;

    let mut recurring = support::new_item(household.tasks_list_id, "Полить цветы");
    recurring.due_date = Some(today);
    recurring.recurrence = Some(tasklerq::domain::models::RecurrenceInput {
        freq: RecurrenceFreq::Daily,
        interval: Some(1),
        byweekday: Vec::new(),
        until: None,
        timezone: Some("Asia/Vladivostok".into()),
        anchor_date: None,
        month_rule: None,
    });
    let recurring = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &actor,
        recurring,
    )
    .await;

    support::create_item(
        &db.pool,
        &household,
        household.shopping_list_id,
        &actor,
        support::new_item(household.shopping_list_id, "Молоко"),
    )
    .await;
    let shopping_done = support::create_item(
        &db.pool,
        &household,
        household.shopping_list_id,
        &actor,
        support::new_item(household.shopping_list_id, "Хлеб"),
    )
    .await;

    let mut conn = db.pool.acquire().await.expect("connection");
    items::complete(
        &mut conn,
        household.household_id,
        &actor,
        overdue.id,
        Some(overdue.version),
        "Asia/Vladivostok",
        now,
    )
    .await
    .expect("complete a task");
    items::complete(
        &mut conn,
        household.household_id,
        &actor,
        recurring.id,
        Some(recurring.version),
        "Asia/Vladivostok",
        now,
    )
    .await
    .expect("complete a recurring task");
    items::complete(
        &mut conn,
        household.household_id,
        &actor,
        shopping_done.id,
        Some(shopping_done.version),
        "Asia/Vladivostok",
        now,
    )
    .await
    .expect("complete a shopping item");

    let counts = items::menu_counts(&mut conn, household.household_id, "Asia/Vladivostok", now)
        .await
        .expect("menu counts");

    // Открыты: две задачи на сегодня, завтра и повторяющаяся (она осталась
    // открытой со следующим вхождением).
    assert_eq!(counts.open_tasks, 4);
    assert_eq!(counts.overdue_tasks, 0);
    assert_eq!(counts.due_today_tasks, 2);
    assert_eq!(counts.open_shopping, 1);
    assert_eq!(
        counts.done_today, 2,
        "only task completion events count, recurring ones too"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn menu_counts_do_not_count_event_lists_as_shopping() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let actor = household.owner();
    let now = Utc::now();

    support::create_item(
        &db.pool,
        &household,
        household.shopping_list_id,
        &actor,
        support::new_item(household.shopping_list_id, "Молоко"),
    )
    .await;

    let mut event = support::new_item(household.events_list_id, "Встреча");
    event.due_date = Some(NaiveDate::from_ymd_opt(2027, 3, 16).expect("event date"));
    support::create_item(
        &db.pool,
        &household,
        household.events_list_id,
        &actor,
        event,
    )
    .await;

    let mut conn = db.pool.acquire().await.expect("connection");
    let counts = items::menu_counts(&mut conn, household.household_id, "Europe/Moscow", now)
        .await
        .expect("menu counts");

    assert_eq!(counts.open_tasks, 0);
    assert_eq!(
        counts.open_shopping, 1,
        "an open event must not count as a shopping item"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn menu_counts_include_all_day_and_ongoing_events_today() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let actor = household.owner();
    let moscow = chrono_tz::Europe::Moscow;
    let today = NaiveDate::from_ymd_opt(2027, 3, 16).expect("today");
    let yesterday = today.pred_opt().expect("yesterday");
    let tomorrow = today.succ_opt().expect("tomorrow");
    let now = moscow
        .with_ymd_and_hms(2027, 3, 16, 10, 0, 0)
        .single()
        .expect("local now")
        .with_timezone(&Utc);

    let mut all_day = support::new_item(household.events_list_id, "Весь день");
    all_day.due_date = Some(today);
    support::create_item(
        &db.pool,
        &household,
        household.events_list_id,
        &actor,
        all_day,
    )
    .await;

    let mut ongoing = support::new_item(household.events_list_id, "Идёт со вчера");
    ongoing.due_date = Some(yesterday);
    ongoing.end_date = Some(today);
    support::create_item(
        &db.pool,
        &household,
        household.events_list_id,
        &actor,
        ongoing,
    )
    .await;

    let mut past = support::new_item(household.events_list_id, "Прошлое");
    past.due_date = Some(yesterday);
    support::create_item(&db.pool, &household, household.events_list_id, &actor, past).await;

    let mut future = support::new_item(household.events_list_id, "Будущее");
    future.due_date = Some(tomorrow);
    support::create_item(
        &db.pool,
        &household,
        household.events_list_id,
        &actor,
        future,
    )
    .await;

    let mut conn = db.pool.acquire().await.expect("connection");
    let counts = items::menu_counts(&mut conn, household.household_id, "Europe/Moscow", now)
        .await
        .expect("menu counts");

    assert_eq!(
        counts.due_today_tasks, 2,
        "an all-day event and an event started yesterday and still running count; \
         a past and a future event do not"
    );
    assert_eq!(counts.overdue_tasks, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn event_counters_skip_events_older_than_the_sql_prefilter() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let actor = household.owner();
    let moscow = chrono_tz::Europe::Moscow;
    let today = NaiveDate::from_ymd_opt(2027, 3, 16).expect("today");
    let now = moscow
        .with_ymd_and_hms(2027, 3, 16, 10, 0, 0)
        .single()
        .expect("local now")
        .with_timezone(&Utc);

    let mut ancient = support::new_item(household.events_list_id, "Давнее");
    ancient.due_date = Some(today - Duration::days(7));
    support::create_item(
        &db.pool,
        &household,
        household.events_list_id,
        &actor,
        ancient,
    )
    .await;

    let mut current = support::new_item(household.events_list_id, "Сегодня");
    current.due_date = Some(today);
    support::create_item(
        &db.pool,
        &household,
        household.events_list_id,
        &actor,
        current,
    )
    .await;

    let mut conn = db.pool.acquire().await.expect("connection");
    let counts = items::menu_counts(&mut conn, household.household_id, "Europe/Moscow", now)
        .await
        .expect("menu counts");
    assert_eq!(
        counts.due_today_tasks, 1,
        "an event a week in the past must not affect today's counter"
    );

    let by_list =
        items::open_event_counts_by_list(&mut conn, household.household_id, "Europe/Moscow", now)
            .await
            .expect("event counts");
    assert_eq!(
        by_list.get(&household.events_list_id),
        Some(&1),
        "a week-old event must not be counted in «Все списки»"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn open_assigned_tasks_counts_only_open_own_tasks_in_visible_task_lists() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let owner = household.owner();

    let mut mine = support::new_item(household.tasks_list_id, "Моя задача");
    mine.assignee_user_id = Some(household.member_id);
    support::create_item(&db.pool, &household, household.tasks_list_id, &owner, mine).await;

    let mut done = support::new_item(household.tasks_list_id, "Выполненная");
    done.assignee_user_id = Some(household.member_id);
    let done =
        support::create_item(&db.pool, &household, household.tasks_list_id, &owner, done).await;

    let mut other = support::new_item(household.tasks_list_id, "Чужая");
    other.assignee_user_id = Some(household.owner_id);
    support::create_item(&db.pool, &household, household.tasks_list_id, &owner, other).await;

    let mut shopping = support::new_item(household.shopping_list_id, "Покупка");
    shopping.assignee_user_id = Some(household.member_id);
    support::create_item(
        &db.pool,
        &household,
        household.shopping_list_id,
        &owner,
        shopping,
    )
    .await;

    let mut conn = db.pool.acquire().await.expect("connection");
    let archived = lists::create(
        &mut conn,
        household.household_id,
        ListKind::Tasks,
        "Архив",
        Utc::now(),
    )
    .await
    .expect("archive list");
    let mut archived_item = support::new_item(archived.id, "В архиве");
    archived_item.assignee_user_id = Some(household.member_id);
    drop(conn);

    support::create_item(&db.pool, &household, archived.id, &owner, archived_item).await;

    let mut conn = db.pool.acquire().await.expect("connection");
    items::complete(
        &mut conn,
        household.household_id,
        &owner,
        done.id,
        Some(done.version),
        "Europe/Moscow",
        Utc::now(),
    )
    .await
    .expect("complete an assigned task");
    lists::update(
        &mut conn,
        household.household_id,
        archived.id,
        None,
        Some(true),
        true,
        Utc::now(),
    )
    .await
    .expect("archive the list");

    let count = items::open_assigned_tasks(&mut conn, household.household_id, household.member_id)
        .await
        .expect("assigned count");
    assert_eq!(
        count, 1,
        "only the open own task in a visible tasks list counts"
    );
}
