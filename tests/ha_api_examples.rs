//! Примеры запросов из `docs/ha-api.md`, разделы 4.2, 4.4 и 6.
//!
//! Каждый пример отправляется тестовому серверу, чтобы документ не разошёлся с
//! кодом. Проверяются статус и ключевые поля ответа, описанные в документе.

mod support;

use axum::http::{Method, StatusCode};
use chrono::{NaiveDate, TimeZone, Timelike, Utc};
use serde_json::{Value, json};
use tasklerq::domain::models::{ListKind, RecurrenceFreq};

fn date(year: i32, month: u32, day: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(year, month, day).expect("valid date")
}

fn utc(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(year, month, day, hour, minute, 0)
        .single()
        .expect("valid instant")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn post_lists_creates_an_events_list() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let token =
        support::issue_token(&db.pool, household.household_id, &["ha:read", "ha:write"]).await;
    let app = support::app(&db.pool);

    let (status, body) = support::request(
        &app,
        Method::POST,
        "/api/v1/lists",
        Some(&token),
        Some(json!({"kind": "events", "name": "Школа"})),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["kind"], "events");
    assert_eq!(body["name"], "Школа");
    assert_eq!(body["is_default"], false);
    assert_eq!(body["is_archived"], false);
    assert_eq!(
        body["position"], 3,
        "the three default lists take positions 0..2"
    );
    assert_eq!(body["deleted_at"], Value::Null);

    let mut conn = db.pool.acquire().await.expect("connection");
    let lists = tasklerq::storage::repositories::lists::list(&mut conn, household.household_id)
        .await
        .expect("lists");
    assert!(
        lists
            .iter()
            .any(|list| list.kind == ListKind::Events && list.name == "Школа")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn post_item_creates_a_timed_event_with_end() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let token =
        support::issue_token(&db.pool, household.household_id, &["ha:read", "ha:write"]).await;
    let app = support::app(&db.pool);

    let (status, body) = support::request(
        &app,
        Method::POST,
        &format!("/api/v1/lists/{}/items", household.events_list_id),
        Some(&token),
        Some(json!({
            "title": "Маникюр",
            "due_at": "2026-11-11T14:00:00+03:00",
            "end_at": "2026-11-11T15:30:00+03:00"
        })),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["list_id"], household.events_list_id.to_string());
    assert_eq!(body["title"], "Маникюр");
    assert_eq!(body["status"], "open");
    assert_eq!(body["priority"], "normal");
    assert_eq!(body["due_at"], "2026-11-11T11:00:00Z");
    assert_eq!(body["due_date"], Value::Null);
    assert_eq!(body["end_at"], "2026-11-11T12:30:00Z");
    assert_eq!(body["end_date"], Value::Null);
    assert_eq!(body["quantity"], Value::Null);
    assert_eq!(body["unit"], Value::Null);
    assert_eq!(body["version"], 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn post_item_creates_a_multi_day_event() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let token =
        support::issue_token(&db.pool, household.household_id, &["ha:read", "ha:write"]).await;
    let app = support::app(&db.pool);

    let (status, body) = support::request(
        &app,
        Method::POST,
        &format!("/api/v1/lists/{}/items", household.events_list_id),
        Some(&token),
        Some(json!({"title": "Отпуск", "due_date": "2027-03-05", "end_date": "2027-03-10"})),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["due_date"], "2027-03-05");
    assert_eq!(body["end_date"], "2027-03-10");
    assert_eq!(body["due_at"], Value::Null);
    assert_eq!(body["end_at"], Value::Null);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn post_item_event_without_start_is_rejected() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let token =
        support::issue_token(&db.pool, household.household_id, &["ha:read", "ha:write"]).await;
    let app = support::app(&db.pool);

    let (status, body) = support::request(
        &app,
        Method::POST,
        &format!("/api/v1/lists/{}/items", household.events_list_id),
        Some(&token),
        Some(json!({"title": "Без начала"})),
    )
    .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["code"], "validation_error");
    assert_eq!(
        body["error"]["message"],
        "an event must have due_at or due_date"
    );
    assert!(body["error"]["details"].is_object());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn patch_item_shifts_the_event_end_with_its_start() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let token =
        support::issue_token(&db.pool, household.household_id, &["ha:read", "ha:write"]).await;
    let app = support::app(&db.pool);

    let (status, created) = support::request(
        &app,
        Method::POST,
        &format!("/api/v1/lists/{}/items", household.events_list_id),
        Some(&token),
        Some(json!({
            "title": "Маникюр",
            "due_at": "2026-11-11T14:00:00+03:00",
            "end_at": "2026-11-11T15:30:00+03:00"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let item_id = created["id"].as_str().expect("item id");

    let (status, body) = support::request(
        &app,
        Method::PATCH,
        &format!("/api/v1/items/{item_id}"),
        Some(&token),
        Some(json!({"version": 1, "due_at": "2026-11-12T16:00:00+03:00"})),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["due_at"], "2026-11-12T13:00:00Z");
    assert_eq!(
        body["end_at"], "2026-11-12T14:30:00Z",
        "the end is shifted by the same two hours"
    );
    assert_eq!(body["version"], 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn calendar_example_returns_kind_and_real_end() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let token = support::issue_token(&db.pool, household.household_id, &["ha:read"]).await;
    let app = support::app(&db.pool);

    let mut all_day_task = support::new_item(household.tasks_list_id, "Вседневная");
    all_day_task.due_date = Some(date(2026, 11, 10));
    support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        all_day_task,
    )
    .await;

    let mut timed_event = support::new_item(household.events_list_id, "Маникюр");
    timed_event.due_at = Some(utc(2026, 11, 11, 14, 0));
    timed_event.end_at = Some(utc(2026, 11, 11, 15, 30));
    support::create_item(
        &db.pool,
        &household,
        household.events_list_id,
        &household.owner(),
        timed_event,
    )
    .await;

    let mut multi_day_event = support::new_item(household.events_list_id, "Отпуск");
    multi_day_event.due_date = Some(date(2027, 3, 5));
    multi_day_event.end_date = Some(date(2027, 3, 10));
    support::create_item(
        &db.pool,
        &household,
        household.events_list_id,
        &household.owner(),
        multi_day_event,
    )
    .await;

    let (status, body) = support::request(
        &app,
        Method::GET,
        "/api/v1/calendar?start=2026-11-01&end=2027-04-01",
        Some(&token),
        None,
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    let events = body["events"].as_array().expect("events array");
    assert_eq!(
        events
            .iter()
            .map(|event| event["title"].as_str().unwrap_or_default())
            .collect::<Vec<_>>(),
        vec!["Вседневная", "Маникюр", "Отпуск"],
        "the example is ordered by start"
    );
    let by_title = |title: &str| {
        events
            .iter()
            .find(|event| event["title"] == title)
            .unwrap_or_else(|| panic!("{title} is missing: {body}"))
    };

    let task = by_title("Вседневная");
    assert_eq!(task["kind"], "tasks");
    assert_eq!(task["start"], "2026-11-10");
    assert_eq!(task["end"], "2026-11-11");

    let event = by_title("Маникюр");
    assert_eq!(event["kind"], "events");
    assert_eq!(event["start"], "2026-11-11T14:00:00Z");
    assert_eq!(event["end"], "2026-11-11T15:30:00Z");
    assert_eq!(event["all_day"], false);

    let multi_day = by_title("Отпуск");
    assert_eq!(multi_day["kind"], "events");
    assert_eq!(multi_day["start"], "2027-03-05");
    assert_eq!(
        multi_day["end"], "2027-03-11",
        "the inclusive end date is returned exclusive"
    );
    assert_eq!(multi_day["all_day"], true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn calendar_recurring_event_example_keeps_its_duration() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let token = support::issue_token(&db.pool, household.household_id, &["ha:read"]).await;
    let app = support::app(&db.pool);

    let mut input = support::new_item(household.events_list_id, "Тренировка");
    input.due_at = Some(utc(2026, 11, 3, 15, 0));
    input.end_at = Some(utc(2026, 11, 3, 16, 0));
    input.recurrence = Some(support::recurrence(RecurrenceFreq::Weekly, vec![2]));
    support::create_item(
        &db.pool,
        &household,
        household.events_list_id,
        &household.owner(),
        input,
    )
    .await;

    let (status, body) = support::request(
        &app,
        Method::GET,
        "/api/v1/calendar?start=2026-11-01&end=2026-12-01",
        Some(&token),
        None,
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    let occurrence = body["events"]
        .as_array()
        .expect("events array")
        .iter()
        .find(|event| event["title"] == "Тренировка")
        .expect("recurring event");
    assert_eq!(occurrence["kind"], "events");
    assert_eq!(occurrence["start"], "2026-11-03T15:00:00Z");
    assert_eq!(occurrence["end"], "2026-11-03T16:00:00Z");
    assert_eq!(
        body["events"]
            .as_array()
            .expect("events array")
            .iter()
            .filter(|event| event["title"] == "Тренировка")
            .count(),
        4
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snapshot_exposes_the_event_kind_and_end() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let token = support::issue_token(&db.pool, household.household_id, &["ha:read"]).await;
    let app = support::app(&db.pool);

    let due = (Utc::now() + chrono::Duration::days(1))
        .with_nanosecond(0)
        .expect("truncate to seconds");
    let end = due + chrono::Duration::minutes(90);
    let mut input = support::new_item(household.events_list_id, "Снапшот-событие");
    input.due_at = Some(due);
    input.end_at = Some(end);
    let event = support::create_item(
        &db.pool,
        &household,
        household.events_list_id,
        &household.owner(),
        input,
    )
    .await;

    let calendar_end = (Utc::now() + chrono::Duration::days(7))
        .format("%Y-%m-%d")
        .to_string();
    let (status, body) = support::request(
        &app,
        Method::GET,
        &format!("/api/v1/snapshot?calendar_end={calendar_end}"),
        Some(&token),
        None,
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    let occurrence = body["calendar_events"]
        .as_array()
        .expect("calendar events")
        .iter()
        .find(|occurrence| occurrence["item_id"] == event.id.to_string())
        .expect("event in the snapshot");
    assert_eq!(occurrence["kind"], "events");
    assert_eq!(
        occurrence["end"],
        end.format("%Y-%m-%dT%H:%M:%SZ").to_string()
    );
}
