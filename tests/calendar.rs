//! Интеграционные тесты календаря.

mod support;

use axum::http::Method;
use chrono::{NaiveDate, TimeZone, Utc};
use serde_json::json;
use tasklerq::domain::calendar;
use tasklerq::domain::models::{ListKind, MonthRule, RecurrenceFreq};
use tasklerq::domain::recurrence::{Due, parse_timezone};
use tasklerq::storage::repositories::items;

use support::{date, utc};

async fn expand_household(
    pool: &sqlx::PgPool,
    household: &support::TestHousehold,
    start: NaiveDate,
    end: NaiveDate,
) -> Vec<calendar::CalendarEvent> {
    let mut conn = pool.acquire().await.expect("connection");
    let items = items::calendar_items(&mut conn, household.household_id)
        .await
        .expect("calendar items");
    let timezone = parse_timezone("Europe/Moscow").expect("timezone");
    let mut events = Vec::new();
    for entry in &items {
        events.extend(
            calendar::expand_item(entry, start, end, timezone).expect("expand calendar item"),
        );
    }
    events
}

fn json_time(time: &calendar::CalendarTime) -> serde_json::Value {
    serde_json::to_value(time).expect("serialize time")
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
    for entry in &items {
        events.extend(
            calendar::expand_item(entry, start, end, timezone).expect("expand calendar item"),
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
    for entry in &items {
        events.extend(
            calendar::expand_item(entry, date(2025, 11, 1), date(2025, 12, 10), timezone)
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
    for entry in &items {
        events.extend(
            calendar::expand_item(entry, date(2026, 11, 1), date(2026, 12, 1), timezone)
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
    for entry in &items {
        events.extend(
            calendar::expand_item(entry, date(2025, 11, 1), date(2025, 11, 30), timezone)
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
    for entry in &items {
        events.extend(
            calendar::expand_item(entry, date(2026, 11, 1), date(2026, 12, 1), timezone)
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
async fn calendar_shows_a_rescheduled_occurrence_with_the_series_time() {
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
            Utc.with_ymd_and_hms(2026, 11, 12, 9, 0, 0)
                .single()
                .expect("time"),
        ),
        "Europe/Moscow",
        Utc.with_ymd_and_hms(2026, 11, 10, 7, 0, 0)
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
    for entry in &items {
        events.extend(
            calendar::expand_item(entry, date(2026, 11, 1), date(2026, 12, 1), timezone)
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
            "2026-11-12T09:00:00Z",
            "2026-11-17T06:00:00Z",
            "2026-11-24T06:00:00Z"
        ],
        "the moved occurrence keeps its new time and later ones use the series time"
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn calendar_event_with_end_keeps_its_time_end() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;

    let mut input = support::new_item(household.events_list_id, "Маникюр");
    input.due_at = Some(utc(2026, 11, 11, 14, 0));
    input.end_at = Some(utc(2026, 11, 11, 15, 30));
    let event = support::create_item(
        &db.pool,
        &household,
        household.events_list_id,
        &household.owner(),
        input,
    )
    .await;

    let events = expand_household(&db.pool, &household, date(2026, 11, 1), date(2026, 12, 1)).await;
    let occurrence: Vec<_> = events
        .iter()
        .filter(|occurrence| occurrence.item_id == event.id)
        .collect();

    assert_eq!(occurrence.len(), 1);
    assert_eq!(occurrence[0].kind, ListKind::Events);
    assert!(!occurrence[0].all_day);
    assert_eq!(
        json_time(&occurrence[0].start),
        json!("2026-11-11T14:00:00Z")
    );
    assert_eq!(json_time(&occurrence[0].end), json!("2026-11-11T15:30:00Z"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn calendar_event_without_end_lasts_an_hour() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;

    let mut input = support::new_item(household.events_list_id, "Звонок");
    input.due_at = Some(utc(2026, 11, 11, 10, 0));
    let event = support::create_item(
        &db.pool,
        &household,
        household.events_list_id,
        &household.owner(),
        input,
    )
    .await;

    let events = expand_household(&db.pool, &household, date(2026, 11, 1), date(2026, 12, 1)).await;
    let occurrence: Vec<_> = events
        .iter()
        .filter(|occurrence| occurrence.item_id == event.id)
        .collect();

    assert_eq!(occurrence.len(), 1);
    assert_eq!(
        json_time(&occurrence[0].start),
        json!("2026-11-11T10:00:00Z")
    );
    assert_eq!(json_time(&occurrence[0].end), json!("2026-11-11T11:00:00Z"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn calendar_multi_day_event_ends_on_the_next_day() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;

    let mut input = support::new_item(household.events_list_id, "Отпуск");
    input.due_date = Some(date(2027, 3, 5));
    input.end_date = Some(date(2027, 3, 10));
    let event = support::create_item(
        &db.pool,
        &household,
        household.events_list_id,
        &household.owner(),
        input,
    )
    .await;

    let events = expand_household(&db.pool, &household, date(2027, 3, 1), date(2027, 4, 1)).await;
    let occurrence: Vec<_> = events
        .iter()
        .filter(|occurrence| occurrence.item_id == event.id)
        .collect();

    assert_eq!(occurrence.len(), 1);
    assert_eq!(occurrence[0].kind, ListKind::Events);
    assert!(occurrence[0].all_day);
    assert_eq!(json_time(&occurrence[0].start), json!("2027-03-05"));
    assert_eq!(
        json_time(&occurrence[0].end),
        json!("2027-03-11"),
        "the inclusive end date is returned exclusive"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn calendar_includes_an_event_that_started_before_the_interval() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;

    let mut input = support::new_item(household.events_list_id, "Командировка");
    input.due_date = Some(date(2027, 2, 27));
    input.end_date = Some(date(2027, 3, 2));
    let event = support::create_item(
        &db.pool,
        &household,
        household.events_list_id,
        &household.owner(),
        input,
    )
    .await;

    let events = expand_household(&db.pool, &household, date(2027, 3, 1), date(2027, 4, 1)).await;
    let occurrence: Vec<_> = events
        .iter()
        .filter(|occurrence| occurrence.item_id == event.id)
        .collect();

    assert_eq!(occurrence.len(), 1);
    assert_eq!(json_time(&occurrence[0].start), json!("2027-02-27"));
    assert_eq!(json_time(&occurrence[0].end), json!("2027-03-03"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn calendar_expands_a_recurring_event_with_its_duration() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;

    let mut input = support::new_item(household.events_list_id, "Тренировка");
    input.due_at = Some(utc(2026, 11, 3, 15, 0));
    input.end_at = Some(utc(2026, 11, 3, 16, 0));
    input.recurrence = Some(support::recurrence(RecurrenceFreq::Weekly, vec![2]));
    let event = support::create_item(
        &db.pool,
        &household,
        household.events_list_id,
        &household.owner(),
        input,
    )
    .await;

    let events = expand_household(&db.pool, &household, date(2026, 11, 1), date(2026, 12, 1)).await;
    let occurrence: Vec<_> = events
        .iter()
        .filter(|occurrence| occurrence.item_id == event.id)
        .collect();

    let starts: Vec<_> = occurrence
        .iter()
        .map(|occurrence| json_time(&occurrence.start))
        .collect();
    let ends: Vec<_> = occurrence
        .iter()
        .map(|occurrence| json_time(&occurrence.end))
        .collect();
    assert_eq!(
        starts,
        vec![
            json!("2026-11-03T15:00:00Z"),
            json!("2026-11-10T15:00:00Z"),
            json!("2026-11-17T15:00:00Z"),
            json!("2026-11-24T15:00:00Z")
        ]
    );
    assert_eq!(
        ends,
        vec![
            json!("2026-11-03T16:00:00Z"),
            json!("2026-11-10T16:00:00Z"),
            json!("2026-11-17T16:00:00Z"),
            json!("2026-11-24T16:00:00Z")
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn calendar_includes_a_recurring_event_started_before_the_interval() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;

    let mut input = support::new_item(household.events_list_id, "Отчётный период");
    input.due_date = Some(date(2026, 3, 27));
    input.end_date = Some(date(2026, 4, 2));
    let mut rule = support::recurrence(RecurrenceFreq::Monthly, Vec::new());
    rule.month_rule = Some(MonthRule::AnchorDay);
    input.recurrence = Some(rule);
    let event = support::create_item(
        &db.pool,
        &household,
        household.events_list_id,
        &household.owner(),
        input,
    )
    .await;

    let events = expand_household(&db.pool, &household, date(2026, 4, 1), date(2026, 5, 1)).await;
    let occurrence: Vec<_> = events
        .iter()
        .filter(|occurrence| occurrence.item_id == event.id)
        .collect();

    assert_eq!(
        occurrence
            .iter()
            .map(|occurrence| json_time(&occurrence.start))
            .collect::<Vec<_>>(),
        vec![json!("2026-03-27"), json!("2026-04-27")],
        "the occurrence started in March and the April one are not duplicated"
    );
    assert_eq!(
        occurrence
            .iter()
            .map(|occurrence| json_time(&occurrence.end))
            .collect::<Vec<_>>(),
        vec![json!("2026-04-03"), json!("2026-05-04")]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn calendar_drops_an_event_occurrence_before_the_interval() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;

    // Однодневное ежедневное событие: развёртка начинается на день раньше
    // запроса, но вхождение предыдущего дня не пересекается с интервалом.
    let mut input = support::new_item(household.events_list_id, "Зарядка");
    input.due_date = Some(date(2026, 11, 1));
    input.recurrence = Some(support::recurrence(RecurrenceFreq::Daily, Vec::new()));
    let event = support::create_item(
        &db.pool,
        &household,
        household.events_list_id,
        &household.owner(),
        input,
    )
    .await;

    let events =
        expand_household(&db.pool, &household, date(2026, 11, 10), date(2026, 11, 12)).await;
    let starts: Vec<_> = events
        .iter()
        .filter(|occurrence| occurrence.item_id == event.id)
        .map(|occurrence| json_time(&occurrence.start))
        .collect();

    assert_eq!(
        starts,
        vec![json!("2026-11-10"), json!("2026-11-11")],
        "the one-day occurrence of the previous day must not leak into the answer"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn calendar_drops_a_multi_day_occurrence_that_ends_at_the_interval_start() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;

    // Двухдневное ежедневное событие: вхождение 1 ноября заканчивается ровно
    // в начале интервала и не попадает в ответ, а следующие — попадают.
    let mut input = support::new_item(household.events_list_id, "Смена");
    input.due_date = Some(date(2026, 11, 1));
    input.end_date = Some(date(2026, 11, 2));
    input.recurrence = Some(support::recurrence(RecurrenceFreq::Daily, Vec::new()));
    let event = support::create_item(
        &db.pool,
        &household,
        household.events_list_id,
        &household.owner(),
        input,
    )
    .await;

    let events = expand_household(&db.pool, &household, date(2026, 11, 3), date(2026, 11, 6)).await;
    let starts: Vec<_> = events
        .iter()
        .filter(|occurrence| occurrence.item_id == event.id)
        .map(|occurrence| json_time(&occurrence.start))
        .collect();

    assert_eq!(
        starts,
        vec![
            json!("2026-11-02"),
            json!("2026-11-03"),
            json!("2026-11-04"),
            json!("2026-11-05")
        ],
        "an occurrence that ends exactly at the interval start is not included"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn calendar_shows_a_moved_multi_day_occurrence_started_before_the_interval() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;

    let mut input = support::new_item(household.events_list_id, "Командировка");
    input.due_date = Some(date(2026, 11, 2));
    input.end_date = Some(date(2026, 11, 4));
    input.recurrence = Some(support::recurrence(RecurrenceFreq::Weekly, vec![1]));
    let event = support::create_item(
        &db.pool,
        &household,
        household.events_list_id,
        &household.owner(),
        input,
    )
    .await;

    let mut conn = db.pool.acquire().await.expect("connection");
    items::reschedule_occurrence(
        &mut conn,
        household.household_id,
        &household.owner(),
        event.id,
        event.version,
        Due::AllDay(date(2026, 11, 5)),
        "Europe/Moscow",
        Utc.with_ymd_and_hms(2026, 11, 1, 12, 0, 0)
            .single()
            .expect("fixed moment"),
    )
    .await
    .expect("reschedule occurrence");
    drop(conn);

    let events =
        expand_household(&db.pool, &household, date(2026, 11, 6), date(2026, 11, 20)).await;
    let occurrences: Vec<_> = events
        .iter()
        .filter(|occurrence| occurrence.item_id == event.id)
        .collect();

    assert_eq!(
        occurrences
            .iter()
            .map(|occurrence| json_time(&occurrence.start))
            .collect::<Vec<_>>(),
        vec![
            json!("2026-11-05"),
            json!("2026-11-09"),
            json!("2026-11-16")
        ],
        "the moved multi-day occurrence started before the interval and is still running in it"
    );
    assert_eq!(
        occurrences
            .iter()
            .map(|occurrence| json_time(&occurrence.end))
            .collect::<Vec<_>>(),
        vec![
            json!("2026-11-08"),
            json!("2026-11-12"),
            json!("2026-11-19")
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn calendar_keeps_a_moved_timed_occurrence_at_the_interval_boundary_in_the_series_timezone() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;

    // Серия живёт в поясе UTC+14, household — в Москве. Перенесённое
    // вхождение 05:00 местного времени 6 ноября — это 15:00Z 5 ноября: по
    // поясу household оно уже вчерашнее, по поясу серии — начало интервала.
    let mut input = support::new_item(household.events_list_id, "Созвон");
    input.due_at = Some(utc(2026, 11, 6, 2, 0));
    let mut rule = support::recurrence(RecurrenceFreq::Weekly, vec![5]);
    rule.timezone = Some("Pacific/Kiritimati".into());
    input.recurrence = Some(rule);
    let event = support::create_item(
        &db.pool,
        &household,
        household.events_list_id,
        &household.owner(),
        input,
    )
    .await;

    let mut conn = db.pool.acquire().await.expect("connection");
    items::reschedule_occurrence(
        &mut conn,
        household.household_id,
        &household.owner(),
        event.id,
        event.version,
        Due::Timed(utc(2026, 11, 5, 15, 0)),
        "Europe/Moscow",
        Utc.with_ymd_and_hms(2026, 11, 1, 12, 0, 0)
            .single()
            .expect("fixed moment"),
    )
    .await
    .expect("reschedule occurrence");
    drop(conn);

    let events =
        expand_household(&db.pool, &household, date(2026, 11, 6), date(2026, 11, 13)).await;
    let starts: Vec<_> = events
        .iter()
        .filter(|occurrence| occurrence.item_id == event.id)
        .map(|occurrence| json_time(&occurrence.start))
        .collect();

    assert_eq!(
        starts,
        vec![json!("2026-11-05T15:00:00Z")],
        "the interval boundary is counted in the series timezone"
    );
}
