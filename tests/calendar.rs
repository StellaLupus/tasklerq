//! Интеграционные тесты календаря.

mod support;

use axum::http::Method;
use chrono::{NaiveDate, TimeZone, Utc};
use serde_json::json;
use tasklerq::domain::calendar;
use tasklerq::domain::models::{MonthRule, RecurrenceFreq};
use tasklerq::domain::recurrence::{Due, parse_timezone};
use tasklerq::storage::repositories::items;

fn date(year: i32, month: u32, day: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(year, month, day).expect("valid date")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn calendar_expands_all_day_timed_and_recurring_tasks() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;

    let mut all_day = support::new_item(household.tasks_list_id, "Вседневная");
    all_day.due_date = Some(date(2026, 11, 10));
    let all_day = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        all_day,
    )
    .await;

    let mut timed = support::new_item(household.tasks_list_id, "Встреча");
    timed.due_at = Some(
        Utc.with_ymd_and_hms(2026, 11, 10, 9, 0, 0)
            .single()
            .expect("time"),
    );
    let timed = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        timed,
    )
    .await;

    let mut recurring = support::new_item(household.tasks_list_id, "Еженедельно");
    recurring.due_date = Some(date(2026, 11, 9));
    recurring.recurrence = Some(support::recurrence(
        tasklerq::domain::models::RecurrenceFreq::Weekly,
        vec![1],
    ));
    let recurring = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        recurring,
    )
    .await;

    let mut shopping = support::new_item(household.shopping_list_id, "Молоко");
    shopping.due_date = Some(date(2026, 11, 10));
    let shopping = support::create_item(
        &db.pool,
        &household,
        household.shopping_list_id,
        &household.owner(),
        shopping,
    )
    .await;

    let mut conn = db.pool.acquire().await.expect("connection");
    let items = items::calendar_items(&mut conn, household.household_id)
        .await
        .expect("calendar items");
    let timezone = parse_timezone("Europe/Moscow").expect("timezone");
    let start = date(2026, 11, 9);
    let end = date(2026, 11, 30);

    let mut events = Vec::new();
    for item in &items {
        events.extend(
            calendar::expand_item(item, start, end, timezone).expect("expand calendar item"),
        );
    }

    let all_day_events: Vec<_> = events
        .iter()
        .filter(|event| event.item_id == all_day.id)
        .collect();
    assert_eq!(all_day_events.len(), 1);
    assert!(all_day_events[0].all_day);
    assert_eq!(
        serde_json::to_value(&all_day_events[0].start).expect("serialize"),
        json!("2026-11-10")
    );
    assert_eq!(
        serde_json::to_value(&all_day_events[0].end).expect("serialize"),
        json!("2026-11-11")
    );

    let timed_events: Vec<_> = events
        .iter()
        .filter(|event| event.item_id == timed.id)
        .collect();
    assert_eq!(timed_events.len(), 1);
    assert!(!timed_events[0].all_day);
    assert_eq!(
        serde_json::to_value(&timed_events[0].start).expect("serialize"),
        json!("2026-11-10T09:00:00Z")
    );
    assert_eq!(
        serde_json::to_value(&timed_events[0].end).expect("serialize"),
        json!("2026-11-10T10:00:00Z")
    );

    let recurring_events: Vec<_> = events
        .iter()
        .filter(|event| event.item_id == recurring.id)
        .collect();
    assert_eq!(recurring_events.len(), 3);
    let starts: Vec<_> = recurring_events
        .iter()
        .map(|event| {
            serde_json::to_value(&event.start)
                .expect("serialize")
                .as_str()
                .expect("date string")
                .to_owned()
        })
        .collect();
    assert_eq!(starts, vec!["2026-11-09", "2026-11-16", "2026-11-23"]);

    assert!(!events.iter().any(|event| event.item_id == shopping.id));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn calendar_shows_a_rescheduled_occurrence_on_its_new_date() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let anchor = date(2025, 11, 5);
    let mut input = support::new_item(household.tasks_list_id, "Оплатить счета");
    input.due_date = Some(anchor);
    let mut rule = support::recurrence(RecurrenceFreq::Monthly, Vec::new());
    rule.month_rule = Some(MonthRule::AnchorDay);
    input.recurrence = Some(rule);
    let created = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        input,
    )
    .await;

    let now = Utc
        .with_ymd_and_hms(2025, 11, 7, 12, 0, 0)
        .single()
        .expect("fixed moment");
    let mut conn = db.pool.acquire().await.expect("connection");
    items::reschedule_occurrence(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        created.version,
        Due::AllDay(date(2025, 11, 7)),
        "Europe/Moscow",
        now,
    )
    .await
    .expect("reschedule occurrence");

    let items = items::calendar_items(&mut conn, household.household_id)
        .await
        .expect("calendar items");
    let timezone = parse_timezone("Europe/Moscow").expect("timezone");
    let mut events = Vec::new();
    for item in &items {
        events.extend(
            calendar::expand_item(item, date(2025, 11, 1), date(2025, 12, 10), timezone)
                .expect("expand calendar item"),
        );
    }

    let starts: Vec<_> = events
        .iter()
        .filter(|event| event.item_id == created.id)
        .map(|event| {
            serde_json::to_value(&event.start)
                .expect("serialize")
                .as_str()
                .expect("date string")
                .to_owned()
        })
        .collect();
    assert_eq!(starts, vec!["2025-11-07", "2025-12-05"]);
    assert!(!starts.contains(&"2025-11-05".to_owned()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn calendar_shows_a_rescheduled_occurrence_earlier_than_its_slot() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let mut input = support::new_item(household.tasks_list_id, "Вынести мусор");
    input.due_date = Some(date(2026, 11, 10));
    input.recurrence = Some(support::recurrence(RecurrenceFreq::Weekly, vec![2]));
    let created = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        input,
    )
    .await;

    let mut conn = db.pool.acquire().await.expect("connection");
    items::reschedule_occurrence(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        created.version,
        Due::AllDay(date(2026, 11, 6)),
        "Europe/Moscow",
        Utc.with_ymd_and_hms(2026, 11, 6, 12, 0, 0)
            .single()
            .expect("fixed moment"),
    )
    .await
    .expect("reschedule earlier than the slot");

    let items = items::calendar_items(&mut conn, household.household_id)
        .await
        .expect("calendar items");
    let timezone = parse_timezone("Europe/Moscow").expect("timezone");
    let mut events = Vec::new();
    for item in &items {
        events.extend(
            calendar::expand_item(item, date(2026, 11, 1), date(2026, 12, 1), timezone)
                .expect("expand calendar item"),
        );
    }

    let starts: Vec<_> = events
        .iter()
        .filter(|event| event.item_id == created.id)
        .map(|event| {
            serde_json::to_value(&event.start)
                .expect("serialize")
                .as_str()
                .expect("date string")
                .to_owned()
        })
        .collect();
    assert_eq!(
        starts,
        vec!["2026-11-06", "2026-11-17", "2026-11-24"],
        "the moved occurrence replaces the rule dates up to its slot"
    );
    assert!(!starts.contains(&"2026-11-10".to_owned()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn calendar_shows_a_rescheduled_occurrence_later_than_its_slot() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let mut input = support::new_item(household.tasks_list_id, "Полить цветы");
    input.due_date = Some(date(2025, 11, 4));
    input.recurrence = Some(support::recurrence(RecurrenceFreq::Weekly, vec![2]));
    let created = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        input,
    )
    .await;

    let mut conn = db.pool.acquire().await.expect("connection");
    items::reschedule_occurrence(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        created.version,
        Due::AllDay(date(2025, 11, 12)),
        "Europe/Moscow",
        Utc.with_ymd_and_hms(2025, 11, 12, 12, 0, 0)
            .single()
            .expect("fixed moment"),
    )
    .await
    .expect("reschedule later than the slot");

    let items = items::calendar_items(&mut conn, household.household_id)
        .await
        .expect("calendar items");
    let timezone = parse_timezone("Europe/Moscow").expect("timezone");
    let mut events = Vec::new();
    for item in &items {
        events.extend(
            calendar::expand_item(item, date(2025, 11, 1), date(2025, 11, 30), timezone)
                .expect("expand calendar item"),
        );
    }

    let starts: Vec<_> = events
        .iter()
        .filter(|event| event.item_id == created.id)
        .map(|event| {
            serde_json::to_value(&event.start)
                .expect("serialize")
                .as_str()
                .expect("date string")
                .to_owned()
        })
        .collect();
    assert_eq!(
        starts,
        vec!["2025-11-12", "2025-11-18", "2025-11-25"],
        "the moved occurrence replaces the rule dates up to the moved due"
    );
    assert!(!starts.contains(&"2025-11-04".to_owned()));
    assert!(!starts.contains(&"2025-11-11".to_owned()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn calendar_shows_a_rescheduled_timed_occurrence() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let mut input = support::new_item(household.tasks_list_id, "Встреча");
    input.due_at = Some(
        Utc.with_ymd_and_hms(2026, 11, 10, 6, 0, 0)
            .single()
            .expect("time"),
    );
    input.recurrence = Some(support::recurrence(RecurrenceFreq::Weekly, vec![2]));
    let created = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        input,
    )
    .await;

    let mut conn = db.pool.acquire().await.expect("connection");
    items::reschedule_occurrence(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        created.version,
        Due::Timed(
            Utc.with_ymd_and_hms(2026, 11, 6, 6, 0, 0)
                .single()
                .expect("time"),
        ),
        "Europe/Moscow",
        Utc.with_ymd_and_hms(2026, 11, 6, 12, 0, 0)
            .single()
            .expect("fixed moment"),
    )
    .await
    .expect("reschedule timed occurrence");

    let items = items::calendar_items(&mut conn, household.household_id)
        .await
        .expect("calendar items");
    let timezone = parse_timezone("Europe/Moscow").expect("timezone");
    let mut events = Vec::new();
    for item in &items {
        events.extend(
            calendar::expand_item(item, date(2026, 11, 1), date(2026, 12, 1), timezone)
                .expect("expand calendar item"),
        );
    }

    let starts: Vec<_> = events
        .iter()
        .filter(|event| event.item_id == created.id)
        .map(|event| {
            serde_json::to_value(&event.start)
                .expect("serialize")
                .as_str()
                .expect("timestamp string")
                .to_owned()
        })
        .collect();
    assert_eq!(
        starts,
        vec![
            "2026-11-06T06:00:00Z",
            "2026-11-17T06:00:00Z",
            "2026-11-24T06:00:00Z"
        ],
        "a timed moved occurrence keeps its wall clock and hides earlier rule dates"
    );
    assert!(!starts.contains(&"2026-11-10T06:00:00Z".to_owned()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn calendar_http_requires_range_and_serializes_events() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let token = support::issue_token(&db.pool, household.household_id, &["ha:read"]).await;
    let app = support::app(&db.pool);

    let mut item = support::new_item(household.tasks_list_id, "Событие");
    item.due_date = Some(date(2026, 11, 10));
    support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        item,
    )
    .await;

    let (status, body) = support::request(
        &app,
        Method::GET,
        "/api/v1/calendar?start=2026-11-09&end=2026-11-30",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    let events = body["events"].as_array().expect("events array");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["all_day"], true);
    assert_eq!(events[0]["start"], "2026-11-10");
    assert_eq!(events[0]["end"], "2026-11-11");
    assert_eq!(events[0]["title"], "Событие");

    for uri in [
        "/api/v1/calendar",
        "/api/v1/calendar?start=2026-11-09",
        "/api/v1/calendar?start=2026-11-30&end=2026-11-09",
        "/api/v1/calendar?start=2026-11-01&end=2026-11-02x",
        "/api/v1/calendar?start=2025-01-01&end=2026-12-31",
    ] {
        let (status, body) = support::request(&app, Method::GET, uri, Some(&token), None).await;
        assert_eq!(
            status,
            axum::http::StatusCode::UNPROCESSABLE_ENTITY,
            "unexpected status for {uri}: {body}"
        );
        assert_eq!(body["error"]["code"], "validation_error");
    }

    let (status, _) = support::request(
        &app,
        Method::GET,
        "/api/v1/calendar?start=2026-11-09&end=2026-11-30",
        None,
        None,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::UNAUTHORIZED);
}
