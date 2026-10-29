//! Интеграционные тесты поиска.

mod support;

use std::collections::HashSet;

use chrono::{Duration, Utc};
use tasklerq::domain::error::DomainError;
use tasklerq::domain::models::{ItemStatus, ListKind};
use tasklerq::domain::search::{SearchFilters, decode_cursor};
use tasklerq::storage::repositories::items;
use uuid::Uuid;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_matches_title_notes_tags_and_subtasks() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;

    let mut milk = support::new_item(household.tasks_list_id, "Купить молоко");
    milk.tags = vec!["Дом".into()];
    milk.subtasks = vec![support::subtask("Найти скидку")];
    let milk = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        milk,
    )
    .await;

    let mut call = support::new_item(household.tasks_list_id, "Позвонить маме");
    call.notes = Some("Спросить про молоко".into());
    let call = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        call,
    )
    .await;

    let mut clean = support::new_item(household.tasks_list_id, "Убраться на кухне");
    clean.subtasks = vec![support::subtask("Вытереть молоко со стола")];
    let clean = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        clean,
    )
    .await;

    let unrelated = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        support::new_item(household.tasks_list_id, "Полить цветы"),
    )
    .await;

    let mut conn = db.pool.acquire().await.expect("connection");
    let filters = SearchFilters {
        q: Some("молоко".into()),
        ..SearchFilters::default()
    };
    let (hits, _) = items::search(&mut conn, household.household_id, "Europe/Moscow", &filters)
        .await
        .expect("search by text");
    let ids: HashSet<Uuid> = hits.iter().map(|hit| hit.item.id).collect();
    assert_eq!(ids, HashSet::from([milk.id, call.id, clean.id]));
    assert!(!ids.contains(&unrelated.id));
    assert!(
        hits.iter()
            .all(|hit| hit.list_name == "Задачи" && hit.list_kind == ListKind::Tasks)
    );

    let filters = SearchFilters {
        q: Some("скидку".into()),
        ..SearchFilters::default()
    };
    let (hits, _) = items::search(&mut conn, household.household_id, "Europe/Moscow", &filters)
        .await
        .expect("search by subtask");
    assert_eq!(
        hits.iter().map(|hit| hit.item.id).collect::<Vec<_>>(),
        vec![milk.id]
    );

    let filters = SearchFilters {
        tag: Some("дом".into()),
        ..SearchFilters::default()
    };
    let (hits, _) = items::search(&mut conn, household.household_id, "Europe/Moscow", &filters)
        .await
        .expect("search by tag");
    assert_eq!(
        hits.iter().map(|hit| hit.item.id).collect::<Vec<_>>(),
        vec![milk.id]
    );
    assert_eq!(hits[0].item.tags[0].name, "дом");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_filters_status_list_assignee_and_due_range() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let foreign = support::create_household(&db.pool).await;

    let mut timed = support::new_item(household.tasks_list_id, "Встреча");
    timed.due_at = Some(Utc::now() + Duration::days(2));
    let timed = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        timed,
    )
    .await;

    let mut assigned = support::new_item(household.shopping_list_id, "Купить сыр");
    assigned.assignee_user_id = Some(household.member_id);
    assigned.due_date = Some(Utc::now().date_naive());
    let assigned = support::create_item(
        &db.pool,
        &household,
        household.shopping_list_id,
        &household.owner(),
        assigned,
    )
    .await;

    let done = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        support::new_item(household.tasks_list_id, "Старое дело"),
    )
    .await;
    let mut conn = db.pool.acquire().await.expect("connection");
    items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        done.id,
        None,
        "Europe/Moscow",
        Utc::now(),
    )
    .await
    .expect("complete");

    support::create_item(
        &db.pool,
        &foreign,
        foreign.tasks_list_id,
        &foreign.owner(),
        support::new_item(foreign.tasks_list_id, "Встреча"),
    )
    .await;

    let by_list = SearchFilters {
        list_id: Some(household.shopping_list_id),
        all_status: true,
        ..SearchFilters::default()
    };
    let (hits, _) = items::search(&mut conn, household.household_id, "Europe/Moscow", &by_list)
        .await
        .expect("filter by list");
    assert_eq!(
        hits.iter().map(|hit| hit.item.id).collect::<Vec<_>>(),
        vec![assigned.id]
    );

    let by_assignee = SearchFilters {
        assignee_user_id: Some(household.member_id),
        ..SearchFilters::default()
    };
    let (hits, _) = items::search(
        &mut conn,
        household.household_id,
        "Europe/Moscow",
        &by_assignee,
    )
    .await
    .expect("filter by assignee");
    assert_eq!(
        hits.iter().map(|hit| hit.item.id).collect::<Vec<_>>(),
        vec![assigned.id]
    );

    let open_only = SearchFilters::default();
    let (hits, _) = items::search(
        &mut conn,
        household.household_id,
        "Europe/Moscow",
        &open_only,
    )
    .await
    .expect("open only");
    assert!(!hits.iter().any(|hit| hit.item.id == done.id));

    let all_status = SearchFilters {
        all_status: true,
        ..SearchFilters::default()
    };
    let (hits, _) = items::search(
        &mut conn,
        household.household_id,
        "Europe/Moscow",
        &all_status,
    )
    .await
    .expect("all statuses");
    assert!(hits.iter().any(|hit| hit.item.id == done.id));
    assert!(
        hits.iter()
            .all(|hit| hit.item.household_id == household.household_id)
    );

    let done_only = SearchFilters {
        status: Some(ItemStatus::Done),
        ..SearchFilters::default()
    };
    let (hits, _) = items::search(
        &mut conn,
        household.household_id,
        "Europe/Moscow",
        &done_only,
    )
    .await
    .expect("done only");
    assert_eq!(
        hits.iter().map(|hit| hit.item.id).collect::<Vec<_>>(),
        vec![done.id]
    );

    let due_range = SearchFilters {
        due_from: Some(Utc::now() - Duration::days(1)),
        due_to: Some(Utc::now() + Duration::days(3)),
        ..SearchFilters::default()
    };
    let (hits, _) = items::search(
        &mut conn,
        household.household_id,
        "Europe/Moscow",
        &due_range,
    )
    .await
    .expect("due range");
    let ids: HashSet<Uuid> = hits.iter().map(|hit| hit.item.id).collect();
    assert!(ids.contains(&timed.id));
    assert!(ids.contains(&assigned.id));
    assert!(!ids.contains(&done.id));

    let invalid = SearchFilters {
        limit: 0,
        ..SearchFilters::default()
    };
    assert!(matches!(
        items::search(&mut conn, household.household_id, "Europe/Moscow", &invalid).await,
        Err(DomainError::Validation(_))
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_cursor_is_stable_and_skips_archived_lists() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;

    let mut created = Vec::new();
    for _ in 0..5 {
        created.push(
            support::create_item(
                &db.pool,
                &household,
                household.tasks_list_id,
                &household.owner(),
                support::new_item(household.tasks_list_id, "Одинаковый заголовок"),
            )
            .await
            .id,
        );
    }

    let mut conn = db.pool.acquire().await.expect("connection");
    let mut filters = SearchFilters {
        limit: 2,
        ..SearchFilters::default()
    };
    let mut seen = Vec::new();
    let mut extra_id = None;
    // Предохранитель: сломанный курсор не должен зацикливать тест.
    let max_pages = created.len() + 3;
    for page in 0.. {
        assert!(page < max_pages, "pagination did not terminate");
        let (hits, next) =
            items::search(&mut conn, household.household_id, "Europe/Moscow", &filters)
                .await
                .expect("search page");
        seen.extend(hits.iter().map(|hit| hit.item.id));
        if extra_id.is_none() {
            // Новая задача, созданная между страницами, не должна приводить к
            // повторному показу уже просмотренных записей.
            extra_id = Some(
                support::create_item(
                    &db.pool,
                    &household,
                    household.tasks_list_id,
                    &household.owner(),
                    support::new_item(household.tasks_list_id, "Одинаковый заголовок"),
                )
                .await
                .id,
            );
        }
        match next {
            Some(cursor) => {
                filters.cursor = Some(decode_cursor(&cursor).expect("cursor decodes"));
            }
            None => break,
        }
    }
    assert_eq!(seen.len(), created.len() + 1);
    assert_eq!(
        seen.iter().copied().collect::<HashSet<_>>().len(),
        created.len() + 1
    );
    assert!(created.iter().all(|id| seen.contains(id)));
    assert!(seen.contains(&extra_id.expect("extra item")));

    let mut conn = db.pool.acquire().await.expect("connection");
    let archived_list = tasklerq::storage::repositories::lists::create(
        &mut conn,
        household.household_id,
        ListKind::Checklist,
        "Архив",
        Utc::now(),
    )
    .await
    .expect("list");
    let hidden = support::create_item(
        &db.pool,
        &household,
        archived_list.id,
        &household.owner(),
        support::new_item(archived_list.id, "Скрытая задача"),
    )
    .await;
    sqlx::query("UPDATE lists SET is_archived = true WHERE id = $1")
        .bind(archived_list.id)
        .execute(&mut *conn)
        .await
        .expect("archive list");

    let filters = SearchFilters {
        q: Some("Скрытая".into()),
        all_status: true,
        ..SearchFilters::default()
    };
    let (hits, _) = items::search(&mut conn, household.household_id, "Europe/Moscow", &filters)
        .await
        .expect("archived list excluded");
    assert!(!hits.iter().any(|hit| hit.item.id == hidden.id));

    let deleted = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        support::new_item(household.tasks_list_id, "Удалённый текст"),
    )
    .await;
    items::soft_delete(
        &mut conn,
        household.household_id,
        &household.owner(),
        deleted.id,
        Utc::now(),
    )
    .await
    .expect("delete item");

    let filters = SearchFilters {
        q: Some("Удалённый".into()),
        all_status: true,
        ..SearchFilters::default()
    };
    let (hits, _) = items::search(&mut conn, household.household_id, "Europe/Moscow", &filters)
        .await
        .expect("deleted item excluded");
    assert!(hits.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn due_upper_bound_is_exclusive_at_local_midnight() {
    use chrono::TimeZone;
    use tasklerq::domain::recurrence::parse_timezone;
    use tasklerq::domain::search::{DueBound, parse_due_bound};

    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let timezone = parse_timezone("Europe/Moscow").expect("timezone");

    // Вседневная задача 3 ноября хранится как 3 ноября 00:00 по локальному
    // времени, то есть ровно на исключающей границе диапазона «2 ноября».
    let mut all_day = support::new_item(household.tasks_list_id, "Вседневная 3 ноября");
    all_day.due_date = chrono::NaiveDate::from_ymd_opt(2026, 11, 3);
    // 2026-11-02T21:00Z — тоже 3 ноября 00:00 MSK.
    let mut timed = support::new_item(household.tasks_list_id, "Ровно полночь 3 ноября");
    timed.due_at = Some(chrono::Utc.with_ymd_and_hms(2026, 11, 2, 21, 0, 0).unwrap());
    // Последняя секунда 2 ноября остаётся внутри диапазона.
    let mut last_second = support::new_item(household.tasks_list_id, "Последняя секунда 2 ноября");
    last_second.due_at = Some(
        chrono::Utc
            .with_ymd_and_hms(2026, 11, 2, 20, 59, 59)
            .unwrap(),
    );
    for item in [all_day, timed, last_second] {
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
    let search = |from: &'static str, to: &'static str| {
        let filters = SearchFilters {
            due_from: parse_due_bound(Some(from), DueBound::From, timezone).expect("from"),
            due_to: parse_due_bound(Some(to), DueBound::To, timezone).expect("to"),
            ..SearchFilters::default()
        };
        (filters, household.household_id)
    };

    let (filters, household_id) = search("2026-11-02", "2026-11-02");
    let (hits, _) = items::search(&mut conn, household_id, "Europe/Moscow", &filters)
        .await
        .expect("search");
    let titles: Vec<_> = hits.iter().map(|hit| hit.item.title.as_str()).collect();
    assert_eq!(titles, ["Последняя секунда 2 ноября"]);

    let (filters, household_id) = search("2026-11-03", "2026-11-03");
    let (hits, _) = items::search(&mut conn, household_id, "Europe/Moscow", &filters)
        .await
        .expect("search");
    assert_eq!(hits.len(), 2, "both items due on 3 November are found");
}
