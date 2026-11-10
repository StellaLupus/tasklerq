//! Контрактные интеграционные тесты REST API v1.

mod support;

use std::collections::HashSet;

use axum::http::{Method, StatusCode};
use chrono::{Duration, TimeZone, Utc};
use serde_json::{Value, json};
use tasklerq::auth::tokens::hash_token;
use tasklerq::domain::recurrence::Due;
use tasklerq::storage::repositories::{items, tokens};
use uuid::Uuid;

async fn issue_token_with(
    pool: &sqlx::PgPool,
    household_id: Uuid,
    scopes: &[&str],
    expires_at: Option<chrono::DateTime<Utc>>,
    revoked: bool,
) -> String {
    let token = format!("tlq_test_{}", Uuid::now_v7());
    let scopes: Vec<String> = scopes.iter().map(|scope| (*scope).to_owned()).collect();
    let prefix: String = token.chars().take(12).collect();
    let mut conn = pool.acquire().await.expect("connection");
    let created = tokens::create(
        &mut conn,
        household_id,
        "contract token",
        &hash_token(&token),
        &prefix,
        &scopes,
        expires_at,
        None,
        Utc::now(),
    )
    .await
    .expect("token");
    if revoked {
        sqlx::query("UPDATE api_tokens SET revoked_at = now() WHERE id = $1")
            .bind(created.id)
            .execute(&mut *conn)
            .await
            .expect("revoke");
    }
    token
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn authentication_and_scopes_are_enforced() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let foreign = support::create_household(&db.pool).await;
    let app = support::app(&db.pool);
    let read_token = support::issue_token(&db.pool, household.household_id, &["ha:read"]).await;
    let write_token = support::issue_token(&db.pool, household.household_id, &["ha:write"]).await;
    let expired = issue_token_with(
        &db.pool,
        household.household_id,
        &["ha:read", "ha:write"],
        Some(Utc::now() - Duration::hours(1)),
        false,
    )
    .await;
    let revoked = issue_token_with(
        &db.pool,
        household.household_id,
        &["ha:read", "ha:write"],
        None,
        true,
    )
    .await;

    for token in [
        None,
        Some("nope"),
        Some(expired.as_str()),
        Some(revoked.as_str()),
    ] {
        let (status, body) =
            support::request(&app, Method::GET, "/api/v1/lists", token, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "token: {token:?}");
        assert_eq!(body["error"]["code"], "unauthorized");
        assert!(body["error"]["request_id"].is_string());
    }

    let (status, _) =
        support::request(&app, Method::GET, "/api/v1/lists", Some(&read_token), None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = support::request(
        &app,
        Method::POST,
        "/api/v1/lists",
        Some(&read_token),
        Some(json!({"kind": "checklist", "name": "Чтение"})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"]["code"], "forbidden");

    let (status, _) =
        support::request(&app, Method::GET, "/api/v1/lists", Some(&write_token), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let foreign_token =
        support::issue_token(&db.pool, foreign.household_id, &["ha:read", "ha:write"]).await;
    let (status, body) = support::request(
        &app,
        Method::GET,
        &format!("/api/v1/items/{}", Uuid::now_v7()),
        Some(&foreign_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "not_found");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_and_item_crud_contract() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let foreign = support::create_household(&db.pool).await;
    let token =
        support::issue_token(&db.pool, household.household_id, &["ha:read", "ha:write"]).await;
    let foreign_token =
        support::issue_token(&db.pool, foreign.household_id, &["ha:read", "ha:write"]).await;
    let app = support::app(&db.pool);

    let (status, household_body) =
        support::request(&app, Method::GET, "/api/v1/household", Some(&token), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        household_body["household"]["id"],
        household.household_id.to_string()
    );
    assert_eq!(
        household_body["members"].as_array().expect("members").len(),
        2
    );

    let (status, lists) =
        support::request(&app, Method::GET, "/api/v1/lists", Some(&token), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(lists.as_array().expect("lists").len(), 3);

    let (status, created_list) = support::request(
        &app,
        Method::POST,
        "/api/v1/lists",
        Some(&token),
        Some(json!({"kind": "checklist", "name": "Дача"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let list_id = created_list["id"].as_str().expect("list id").to_owned();
    assert_eq!(created_list["name"], "Дача");
    assert_eq!(created_list["kind"], "checklist");

    let (status, body) = support::request(
        &app,
        Method::POST,
        "/api/v1/lists",
        Some(&token),
        Some(json!({"kind": "tasks", "name": "Ещё задачи"})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"]["code"], "validation_error");

    let (status, body) = support::request(
        &app,
        Method::POST,
        "/api/v1/lists",
        Some(&token),
        Some(json!({"kind": "checklist", "name": "дача"})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "conflict");

    let (status, renamed) = support::request(
        &app,
        Method::PATCH,
        &format!("/api/v1/lists/{list_id}"),
        Some(&token),
        Some(json!({"name": "Дача 2"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(renamed["name"], "Дача 2");

    let (status, archived) = support::request(
        &app,
        Method::PATCH,
        &format!("/api/v1/lists/{list_id}"),
        Some(&token),
        Some(json!({"is_archived": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(archived["is_archived"], true);
    let (status, _) = support::request(
        &app,
        Method::GET,
        &format!("/api/v1/lists/{list_id}/items"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    support::request(
        &app,
        Method::PATCH,
        &format!("/api/v1/lists/{list_id}"),
        Some(&token),
        Some(json!({"is_archived": false})),
    )
    .await;

    let (status, item) = support::request(
        &app,
        Method::POST,
        &format!("/api/v1/lists/{list_id}/items"),
        Some(&token),
        Some(json!({
            "title": "Купить гвозди",
            "notes": "Длинные",
            "priority": "high",
            "tags": ["Дом", "дом", "Дача"],
            "subtasks": [{"title": "Проверить наличие"}],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create item: {item}");
    let item_id = item["id"].as_str().expect("item id").to_owned();
    assert_eq!(item["version"], 1);
    assert_eq!(item["status"], "open");
    assert_eq!(item["tags"].as_array().expect("tags").len(), 2);
    assert_eq!(item["subtask_progress"]["done"], 0);
    assert_eq!(item["subtask_progress"]["total"], 1);

    let (status, fetched) = support::request(
        &app,
        Method::GET,
        &format!("/api/v1/items/{item_id}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fetched["title"], "Купить гвозди");

    let (status, updated) = support::request(
        &app,
        Method::PATCH,
        &format!("/api/v1/items/{item_id}"),
        Some(&token),
        Some(json!({"version": 1, "title": "Купить саморезы", "due_date": "2026-11-15"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(updated["title"], "Купить саморезы");
    assert_eq!(updated["version"], 2);

    // Повторный PATCH без фактических изменений не двигает версию, не
    // обновляет `updated_at` и не пишет событие.
    let item_uuid = Uuid::parse_str(&item_id).expect("item uuid");
    let events_before: i64 =
        sqlx::query_scalar("SELECT count(*) FROM item_events WHERE item_id = $1")
            .bind(item_uuid)
            .fetch_one(&db.pool)
            .await
            .expect("event count");
    let (status, unchanged) = support::request(
        &app,
        Method::PATCH,
        &format!("/api/v1/items/{item_id}"),
        Some(&token),
        Some(json!({"version": 2, "title": "Купить саморезы", "due_date": "2026-11-15"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "no-op patch: {unchanged}");
    assert_eq!(unchanged["version"], 2);
    assert_eq!(unchanged["title"], "Купить саморезы");
    assert_eq!(unchanged["updated_at"], updated["updated_at"]);
    let events_after: i64 =
        sqlx::query_scalar("SELECT count(*) FROM item_events WHERE item_id = $1")
            .bind(item_uuid)
            .fetch_one(&db.pool)
            .await
            .expect("event count");
    assert_eq!(events_after, events_before);

    // Проверка `version` идёт раньше определения no-op: тот же PATCH без
    // фактических изменений, но с устаревшей `version`, даёт `409`, а не `200`.
    let (status, body) = support::request(
        &app,
        Method::PATCH,
        &format!("/api/v1/items/{item_id}"),
        Some(&token),
        Some(json!({"version": 1, "title": "Купить саморезы", "due_date": "2026-11-15"})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "stale no-op patch: {body}");
    assert_eq!(body["error"]["code"], "conflict");
    assert_eq!(body["error"]["details"]["current_version"], 2);

    let (status, body) = support::request(
        &app,
        Method::PATCH,
        &format!("/api/v1/items/{item_id}"),
        Some(&token),
        Some(json!({"version": 1, "title": "Устаревшая версия"})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "conflict");
    assert_eq!(body["error"]["details"]["current_version"], 2);
    assert_eq!(body["error"]["details"]["item"]["title"], "Купить саморезы");

    let (status, body) = support::request(
        &app,
        Method::PATCH,
        &format!("/api/v1/items/{item_id}"),
        Some(&token),
        Some(json!({
            "version": 2,
            "due_at": "2026-11-15T10:00:00Z",
            "due_date": "2026-11-15"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"]["code"], "validation_error");

    let (status, completed) = support::request(
        &app,
        Method::POST,
        &format!("/api/v1/items/{item_id}/complete"),
        Some(&token),
        Some(json!({"version": 2})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(completed["status"], "done");
    assert!(completed["completed_at"].is_string());

    let (status, repeated) = support::request(
        &app,
        Method::POST,
        &format!("/api/v1/items/{item_id}/complete"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(repeated["version"], completed["version"]);

    let (status, reopened) = support::request(
        &app,
        Method::POST,
        &format!("/api/v1/items/{item_id}/reopen"),
        Some(&token),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(reopened["status"], "open");
    assert_eq!(reopened["due_date"], "2026-11-15");

    let (status, _) = support::request(
        &app,
        Method::DELETE,
        &format!("/api/v1/items/{item_id}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = support::request(
        &app,
        Method::GET,
        &format!("/api/v1/items/{item_id}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, restored) = support::request(
        &app,
        Method::POST,
        &format!("/api/v1/items/{item_id}/undo"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(restored["deleted_at"].is_null());
    let (status, _) = support::request(
        &app,
        Method::GET,
        &format!("/api/v1/items/{item_id}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, shopping) = support::request(
        &app,
        Method::POST,
        &format!("/api/v1/lists/{}/items", household.shopping_list_id),
        Some(&token),
        Some(json!({"title": "Сахар", "quantity": 2.5, "unit": "кг"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(shopping["quantity"], 2.5);
    assert_eq!(shopping["unit"], "кг");

    let (status, body) = support::request(
        &app,
        Method::POST,
        &format!("/api/v1/lists/{}/items", household.shopping_list_id),
        Some(&token),
        Some(json!({"title": "Ноль", "quantity": 0})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"]["code"], "validation_error");

    let (status, _) = support::request(
        &app,
        Method::POST,
        &format!("/api/v1/lists/{}/items", Uuid::now_v7()),
        Some(&token),
        Some(json!({"title": "Без списка"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _) = support::request(
        &app,
        Method::GET,
        &format!("/api/v1/items/{item_id}"),
        Some(&foreign_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = support::request(
        &app,
        Method::GET,
        &format!("/api/v1/lists/{}/items", household.tasks_list_id),
        Some(&foreign_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _) = support::request(
        &app,
        Method::DELETE,
        &format!("/api/v1/lists/{list_id}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = support::request(
        &app,
        Method::GET,
        &format!("/api/v1/lists/{list_id}/items"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, body) = support::request(
        &app,
        Method::DELETE,
        &format!("/api/v1/lists/{}", household.tasks_list_id),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "conflict");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_and_snapshot_contract() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let token =
        support::issue_token(&db.pool, household.household_id, &["ha:read", "ha:write"]).await;
    let app = support::app(&db.pool);

    let (status, first) = support::request(
        &app,
        Method::POST,
        &format!("/api/v1/lists/{}/items", household.tasks_list_id),
        Some(&token),
        Some(json!({"title": "Найти молоко", "due_date": "2026-11-20"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let first_id = first["id"].as_str().expect("item id").to_owned();
    let (status, second) = support::request(
        &app,
        Method::POST,
        &format!("/api/v1/lists/{}/items", household.tasks_list_id),
        Some(&token),
        Some(json!({"title": "Найти сыр"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let second_id = second["id"].as_str().expect("item id").to_owned();
    support::request(
        &app,
        Method::POST,
        &format!("/api/v1/items/{second_id}/complete"),
        Some(&token),
        None,
    )
    .await;
    let (status, calendar_item) = support::request(
        &app,
        Method::POST,
        &format!("/api/v1/lists/{}/items", household.tasks_list_id),
        Some(&token),
        Some(json!({"title": "Календарная", "due_date": "2026-11-25"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let calendar_item_id = calendar_item["id"].as_str().expect("item id").to_owned();
    let (status, _) = support::request(
        &app,
        Method::POST,
        &format!("/api/v1/lists/{}/items", household.tasks_list_id),
        Some(&token),
        Some(json!({"title": "Найти ключи"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, search) = support::request(
        &app,
        Method::GET,
        "/api/v1/search?q=Найти&limit=1",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(search["items"].as_array().expect("items").len(), 1);
    assert!(search["items"][0]["list_name"].is_string());
    assert!(search["items"][0]["rank"].is_number());
    let cursor = search["next_cursor"]
        .as_str()
        .expect("next cursor")
        .to_owned();

    let (status, page) = support::request(
        &app,
        Method::GET,
        &format!("/api/v1/search?q=Найти&limit=1&cursor={cursor}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["items"].as_array().expect("items").len(), 1);
    assert_ne!(page["items"][0]["id"], search["items"][0]["id"]);

    let (status, all) = support::request(
        &app,
        Method::GET,
        "/api/v1/search?q=Найти&status=all",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        all["items"]
            .as_array()
            .expect("items")
            .iter()
            .any(|item| item["id"] == second_id)
    );

    for uri in [
        "/api/v1/search?status=weird",
        "/api/v1/search?limit=0",
        "/api/v1/search?limit=abc",
        "/api/v1/search?cursor=not-base64",
        "/api/v1/search?list_id=nope",
        "/api/v1/search?due_from=2026-13-01",
    ] {
        let (status, body) = support::request(&app, Method::GET, uri, Some(&token), None).await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "unexpected status for {uri}: {body}"
        );
        assert_eq!(body["error"]["code"], "validation_error");
    }

    let (status, snapshot) =
        support::request(&app, Method::GET, "/api/v1/snapshot", Some(&token), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(snapshot["lists"].as_array().expect("lists").len(), 3);
    assert!(snapshot["server_time"].is_string());
    let next_since = snapshot["next_since"]
        .as_str()
        .expect("next_since")
        .to_owned();
    assert_eq!(
        snapshot["deleted_item_ids"]
            .as_array()
            .expect("deleted")
            .len(),
        0
    );
    assert!(
        snapshot["items"]
            .as_array()
            .expect("items")
            .iter()
            .any(|item| item["id"] == first_id)
    );

    let (status, _) = support::request(
        &app,
        Method::DELETE,
        &format!("/api/v1/items/{first_id}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, incremental) = support::request(
        &app,
        Method::GET,
        &format!("/api/v1/snapshot?since={next_since}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let deleted = incremental["deleted_item_ids"]
        .as_array()
        .expect("deleted ids");
    assert_eq!(deleted, &vec![json!(first_id)]);
    assert!(
        !incremental["items"]
            .as_array()
            .expect("items")
            .iter()
            .any(|item| item["id"] == first_id)
    );

    let (status, with_calendar) = support::request(
        &app,
        Method::GET,
        "/api/v1/snapshot?calendar_end=2026-12-31",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        with_calendar["calendar_events"]
            .as_array()
            .expect("calendar events")
            .iter()
            .any(|event| event["item_id"] == calendar_item_id)
    );

    for uri in [
        "/api/v1/snapshot?since=nope",
        "/api/v1/snapshot?calendar_end=nope",
    ] {
        let (status, body) = support::request(&app, Method::GET, uri, Some(&token), None).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["error"]["code"], "validation_error");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undo_without_action_returns_conflict() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let token =
        support::issue_token(&db.pool, household.household_id, &["ha:read", "ha:write"]).await;
    let app = support::app(&db.pool);

    let (status, item) = support::request(
        &app,
        Method::POST,
        &format!("/api/v1/lists/{}/items", household.tasks_list_id),
        Some(&token),
        Some(json!({"title": "Отменяемая"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let item_id = item["id"].as_str().expect("item id").to_owned();

    let (status, _) = support::request(
        &app,
        Method::POST,
        &format!("/api/v1/items/{item_id}/undo"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = support::request(
        &app,
        Method::POST,
        &format!("/api/v1/items/{item_id}/undo"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "conflict");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_resources_and_households_are_not_leaked() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let token =
        support::issue_token(&db.pool, household.household_id, &["ha:read", "ha:write"]).await;
    let app = support::app(&db.pool);
    let missing = Uuid::now_v7();

    for uri in [
        format!("/api/v1/items/{missing}"),
        format!("/api/v1/lists/{missing}/items"),
    ] {
        let (status, body) = support::request(&app, Method::GET, &uri, Some(&token), None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"]["code"], "not_found");
    }

    let (status, _) = support::request(
        &app,
        Method::PATCH,
        &format!("/api/v1/items/{missing}"),
        Some(&token),
        Some(json!({"version": 1, "title": "Нет"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, body) = support::request(
        &app,
        Method::POST,
        &format!("/api/v1/items/{missing}/complete"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "not_found");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_due_bounds_use_household_timezone() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let token =
        support::issue_token(&db.pool, household.household_id, &["ha:read", "ha:write"]).await;
    let app = support::app(&db.pool);

    // Household — Europe/Moscow (UTC+3). 2 ноября 01:00 MSK — это
    // 2026-11-01T22:00Z, а 3 ноября 02:30 MSK — 2026-11-02T23:30Z.
    let cases = [
        (
            json!({"title": "Рано 2 ноября", "due_at": "2026-11-01T22:00:00Z"}),
            true,
        ),
        (
            json!({"title": "Поздно 3 ноября", "due_at": "2026-11-02T23:30:00Z"}),
            false,
        ),
        (
            json!({"title": "Вседневная 2 ноября", "due_date": "2026-11-02"}),
            true,
        ),
        (
            json!({"title": "Вечер 1 ноября", "due_at": "2026-11-01T20:59:00Z"}),
            false,
        ),
    ];
    let mut ids = Vec::new();
    for (body, _) in &cases {
        let (status, item) = support::request(
            &app,
            Method::POST,
            &format!("/api/v1/lists/{}/items", household.tasks_list_id),
            Some(&token),
            Some(body.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        ids.push(item["id"].as_str().expect("item id").to_owned());
    }

    // Полуоткрытый интервал [2 ноября 00:00, 3 ноября 00:00) локального
    // времени: задача 3 ноября 02:30 MSK не должна попадать в выдачу.
    let (status, search) = support::request(
        &app,
        Method::GET,
        "/api/v1/search?due_from=2026-11-02&due_to=2026-11-02",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let found: HashSet<&str> = search["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|item| item["id"].as_str().expect("item id"))
        .collect();
    for (id, (_, expected)) in ids.iter().zip(&cases) {
        assert_eq!(
            found.contains(id.as_str()),
            *expected,
            "unexpected membership for {id}"
        );
    }

    // Расширение до 3 ноября включает и задачу 02:30 MSK.
    let (status, search) = support::request(
        &app,
        Method::GET,
        "/api/v1/search?due_from=2026-11-02&due_to=2026-11-03",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let found: HashSet<&str> = search["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|item| item["id"].as_str().expect("item id"))
        .collect();
    assert_eq!(found.len(), 3);
    assert!(found.contains(ids[0].as_str()));
    assert!(found.contains(ids[1].as_str()));
    assert!(found.contains(ids[2].as_str()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snapshot_truncates_calendar_instead_of_failing() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let token =
        support::issue_token(&db.pool, household.household_id, &["ha:read", "ha:write"]).await;
    let app = support::app(&db.pool);

    // Четыре ежедневные задачи на горизонте 300 дней дают ~1200 вхождений —
    // больше лимита снапшота.
    let end = (Utc::now() + Duration::days(300)).date_naive().to_string();
    for index in 0..4 {
        let (status, _) = support::request(
            &app,
            Method::POST,
            &format!("/api/v1/lists/{}/items", household.tasks_list_id),
            Some(&token),
            Some(json!({
                "title": format!("Ежедневная {index}"),
                "recurrence": {"freq": "daily"}
            })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
    }

    let (status, snapshot) = support::request(
        &app,
        Method::GET,
        &format!("/api/v1/snapshot?calendar_end={end}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "календарный лимит не должен ронять весь снапшот"
    );
    let events = snapshot["calendar_events"].as_array().expect("events");
    assert_eq!(events.len(), 1000);
    assert_eq!(snapshot["calendar_truncated"], json!(true));

    // Без calendar_end ответ не содержит календаря, но todo-данные на месте.
    let (status, snapshot) =
        support::request(&app, Method::GET, "/api/v1/snapshot", Some(&token), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        snapshot["calendar_events"]
            .as_array()
            .expect("events")
            .len(),
        0
    );
    assert_eq!(snapshot["calendar_truncated"], json!(false));
    assert_eq!(snapshot["items"].as_array().expect("items").len(), 4);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recurrence_occurrence_date_is_read_only() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let token =
        support::issue_token(&db.pool, household.household_id, &["ha:read", "ha:write"]).await;
    let app = support::app(&db.pool);

    let (status, created) = support::request(
        &app,
        Method::POST,
        &format!("/api/v1/lists/{}/items", household.tasks_list_id),
        Some(&token),
        Some(json!({
            "title": "Вынести мусор",
            "due_date": "2026-11-10",
            "recurrence": {"freq": "weekly", "byweekday": [2]}
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create item: {created}");
    assert_eq!(created["recurrence"]["occurrence_date"], json!(null));
    let item_id = created["id"].as_str().expect("item id").to_owned();

    let (status, fetched) = support::request(
        &app,
        Method::GET,
        &format!("/api/v1/items/{item_id}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        fetched["recurrence"]["occurrence_date"],
        json!(null),
        "the response exposes the slot as null without a reschedule"
    );

    let (status, patched) = support::request(
        &app,
        Method::PATCH,
        &format!("/api/v1/items/{item_id}"),
        Some(&token),
        Some(json!({
            "version": 1,
            "recurrence": {
                "freq": "weekly",
                "byweekday": [2],
                "anchor_date": "2026-11-10",
                "occurrence_date": "2026-11-03"
            }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "patch item: {patched}");
    assert_eq!(
        patched["recurrence"]["occurrence_date"],
        json!(null),
        "PATCH does not accept the read-only slot"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recurrence_occurrence_time_is_read_only() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let token =
        support::issue_token(&db.pool, household.household_id, &["ha:read", "ha:write"]).await;
    let app = support::app(&db.pool);

    let (status, created) = support::request(
        &app,
        Method::POST,
        &format!("/api/v1/lists/{}/items", household.tasks_list_id),
        Some(&token),
        Some(json!({
            "title": "Встреча",
            "due_at": "2026-11-10T06:00:00Z",
            "recurrence": {"freq": "weekly", "byweekday": [2]}
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create item: {created}");
    assert_eq!(created["recurrence"]["occurrence_time"], json!(null));
    let item_id = created["id"].as_str().expect("item id").to_owned();

    let (status, fetched) = support::request(
        &app,
        Method::GET,
        &format!("/api/v1/items/{item_id}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        fetched["recurrence"]["occurrence_time"],
        json!(null),
        "the response exposes the time slot as null without a reschedule"
    );

    let (status, patched) = support::request(
        &app,
        Method::PATCH,
        &format!("/api/v1/items/{item_id}"),
        Some(&token),
        Some(json!({
            "version": 1,
            "recurrence": {
                "freq": "weekly",
                "byweekday": [2],
                "anchor_date": "2026-11-10",
                "occurrence_time": "12:00:00"
            }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "patch item: {patched}");
    assert_eq!(
        patched["recurrence"]["occurrence_time"],
        json!(null),
        "PATCH does not accept the read-only time slot"
    );

    let mut conn = db.pool.acquire().await.expect("connection");
    let item = items::get(
        &mut conn,
        household.household_id,
        item_id.parse().expect("uuid"),
    )
    .await
    .expect("item");
    let moved_at = Utc
        .with_ymd_and_hms(2026, 11, 12, 9, 0, 0)
        .single()
        .expect("time");
    let moved = items::reschedule_occurrence(
        &mut conn,
        household.household_id,
        &household.owner(),
        item.id,
        item.version,
        Due::Timed(moved_at),
        "Europe/Moscow",
        Utc.with_ymd_and_hms(2026, 11, 10, 7, 0, 0)
            .single()
            .expect("time"),
    )
    .await
    .expect("reschedule occurrence");
    drop(conn);

    let (status, fetched) = support::request(
        &app,
        Method::GET,
        &format!("/api/v1/items/{item_id}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        fetched["recurrence"]["occurrence_date"],
        json!("2026-11-10")
    );
    assert_eq!(
        fetched["recurrence"]["occurrence_time"],
        json!("09:00:00"),
        "the response exposes the series time"
    );

    let (status, patched) = support::request(
        &app,
        Method::PATCH,
        &format!("/api/v1/items/{item_id}"),
        Some(&token),
        Some(json!({
            "version": moved.version,
            "recurrence": fetched["recurrence"].clone()
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "patch item: {patched}");
    assert_eq!(
        patched["version"],
        json!(moved.version),
        "a repeated rule must not bump the version"
    );
    assert_eq!(
        patched["recurrence"]["occurrence_date"],
        json!("2026-11-10")
    );
    assert_eq!(
        patched["recurrence"]["occurrence_time"],
        json!("09:00:00"),
        "a repeated rule keeps the series time slot"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn patch_rule_clears_the_occurrence_slot() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let token =
        support::issue_token(&db.pool, household.household_id, &["ha:read", "ha:write"]).await;
    let app = support::app(&db.pool);
    let slot = chrono::NaiveDate::from_ymd_opt(2026, 11, 10).expect("date");
    let moved_to = chrono::NaiveDate::from_ymd_opt(2026, 11, 6).expect("date");

    let (status, created) = support::request(
        &app,
        Method::POST,
        &format!("/api/v1/lists/{}/items", household.tasks_list_id),
        Some(&token),
        Some(json!({
            "title": "Проверить почту",
            "due_date": slot,
            "recurrence": {"freq": "weekly", "byweekday": [2]}
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create item: {created}");
    let item_id: Uuid = created["id"]
        .as_str()
        .expect("item id")
        .parse()
        .expect("uuid");

    let mut conn = db.pool.acquire().await.expect("connection");
    let item = items::get(&mut conn, household.household_id, item_id)
        .await
        .expect("item");
    let moved = items::reschedule_occurrence(
        &mut conn,
        household.household_id,
        &household.owner(),
        item.id,
        item.version,
        Due::AllDay(moved_to),
        "Europe/Moscow",
        Utc::now(),
    )
    .await
    .expect("reschedule occurrence");
    assert_eq!(
        moved.recurrence.as_ref().expect("rule").occurrence_date,
        Some(slot)
    );
    drop(conn);

    let (status, patched) = support::request(
        &app,
        Method::PATCH,
        &format!("/api/v1/items/{item_id}"),
        Some(&token),
        Some(json!({
            "version": moved.version,
            "recurrence": {
                "freq": "weekly",
                "byweekday": [3],
                "anchor_date": slot
            }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "patch rule: {patched}");
    assert_eq!(
        patched["recurrence"]["occurrence_date"],
        json!(null),
        "patching the rule clears the rescheduled slot"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn patch_notes_keeps_a_last_day_occurrence_moved_off_the_month_end() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let token =
        support::issue_token(&db.pool, household.household_id, &["ha:read", "ha:write"]).await;
    let app = support::app(&db.pool);
    let anchor = chrono::NaiveDate::from_ymd_opt(2026, 1, 31).expect("date");
    let moved_to = chrono::NaiveDate::from_ymd_opt(2026, 2, 7).expect("date");

    let (status, created) = support::request(
        &app,
        Method::POST,
        &format!("/api/v1/lists/{}/items", household.tasks_list_id),
        Some(&token),
        Some(json!({
            "title": "Отчёт за месяц",
            "due_date": anchor,
            "recurrence": {"freq": "monthly", "month_rule": "last_day"}
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create item: {created}");
    let item_id: Uuid = created["id"]
        .as_str()
        .expect("item id")
        .parse()
        .expect("uuid");

    let mut conn = db.pool.acquire().await.expect("connection");
    let item = items::get(&mut conn, household.household_id, item_id)
        .await
        .expect("item");
    let moved = items::reschedule_occurrence(
        &mut conn,
        household.household_id,
        &household.owner(),
        item.id,
        item.version,
        Due::AllDay(moved_to),
        "Europe/Moscow",
        Utc::now(),
    )
    .await
    .expect("reschedule occurrence");
    assert_eq!(moved.due_date, Some(moved_to));
    drop(conn);

    let (status, patched) = support::request(
        &app,
        Method::PATCH,
        &format!("/api/v1/items/{item_id}"),
        Some(&token),
        Some(json!({"version": moved.version, "notes": "только заметка"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "patch notes: {patched}");
    assert_eq!(
        patched["due_date"],
        json!("2026-02-07"),
        "editing notes must not normalize the moved due to the month end"
    );
    assert_eq!(
        patched["recurrence"]["occurrence_date"],
        json!("2026-01-31")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn patch_repeating_the_current_due_keeps_the_occurrence_slot() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let token =
        support::issue_token(&db.pool, household.household_id, &["ha:read", "ha:write"]).await;
    let app = support::app(&db.pool);
    let anchor = chrono::NaiveDate::from_ymd_opt(2026, 11, 3).expect("date");
    let slot = chrono::NaiveDate::from_ymd_opt(2026, 11, 10).expect("date");
    let moved_to = chrono::NaiveDate::from_ymd_opt(2026, 11, 6).expect("date");

    let (status, created) = support::request(
        &app,
        Method::POST,
        &format!("/api/v1/lists/{}/items", household.tasks_list_id),
        Some(&token),
        Some(json!({
            "title": "Проверить почту",
            "due_date": slot,
            "recurrence": {
                "freq": "weekly",
                "byweekday": [2],
                "anchor_date": anchor
            }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create item: {created}");
    let item_id: Uuid = created["id"]
        .as_str()
        .expect("item id")
        .parse()
        .expect("uuid");

    let mut conn = db.pool.acquire().await.expect("connection");
    let item = items::get(&mut conn, household.household_id, item_id)
        .await
        .expect("item");
    let moved = items::reschedule_occurrence(
        &mut conn,
        household.household_id,
        &household.owner(),
        item.id,
        item.version,
        Due::AllDay(moved_to),
        "Europe/Moscow",
        Utc::now(),
    )
    .await
    .expect("reschedule occurrence");
    assert_eq!(
        moved.recurrence.as_ref().expect("rule").occurrence_date,
        Some(slot)
    );
    assert_eq!(moved.recurrence.as_ref().expect("rule").anchor_date, anchor);
    drop(conn);

    let (status, patched) = support::request(
        &app,
        Method::PATCH,
        &format!("/api/v1/items/{item_id}"),
        Some(&token),
        Some(json!({"version": moved.version, "due_date": moved_to})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "patch item: {patched}");
    assert_eq!(
        patched["version"],
        json!(moved.version),
        "a repeated due must not bump the version"
    );
    assert_eq!(patched["due_date"], json!(moved_to));
    assert_eq!(
        patched["recurrence"]["occurrence_date"],
        json!(slot),
        "a repeated due keeps the rescheduled slot"
    );
    assert_eq!(
        patched["recurrence"]["anchor_date"],
        json!(anchor),
        "a repeated due does not move the anchor"
    );

    let mut conn = db.pool.acquire().await.expect("connection");
    let current = items::get(&mut conn, household.household_id, item_id)
        .await
        .expect("item");
    let completed = items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        current.id,
        Some(current.version),
        "Europe/Moscow",
        Utc.with_ymd_and_hms(2026, 11, 6, 12, 0, 0)
            .single()
            .expect("time"),
    )
    .await
    .expect("complete item");
    assert_eq!(
        completed.due_date,
        Some(chrono::NaiveDate::from_ymd_opt(2026, 11, 17).expect("date"))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn patch_repeating_the_current_rule_keeps_the_occurrence_slot() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let token =
        support::issue_token(&db.pool, household.household_id, &["ha:read", "ha:write"]).await;
    let app = support::app(&db.pool);
    let anchor = chrono::NaiveDate::from_ymd_opt(2026, 11, 3).expect("date");
    let slot = chrono::NaiveDate::from_ymd_opt(2026, 11, 10).expect("date");
    let moved_to = chrono::NaiveDate::from_ymd_opt(2026, 11, 6).expect("date");

    let (status, created) = support::request(
        &app,
        Method::POST,
        &format!("/api/v1/lists/{}/items", household.tasks_list_id),
        Some(&token),
        Some(json!({
            "title": "Проверить почту",
            "due_date": slot,
            "recurrence": {
                "freq": "weekly",
                "byweekday": [2],
                "anchor_date": anchor
            }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create item: {created}");
    let item_id: Uuid = created["id"]
        .as_str()
        .expect("item id")
        .parse()
        .expect("uuid");

    let mut conn = db.pool.acquire().await.expect("connection");
    let item = items::get(&mut conn, household.household_id, item_id)
        .await
        .expect("item");
    let moved = items::reschedule_occurrence(
        &mut conn,
        household.household_id,
        &household.owner(),
        item.id,
        item.version,
        Due::AllDay(moved_to),
        "Europe/Moscow",
        Utc::now(),
    )
    .await
    .expect("reschedule occurrence");
    drop(conn);

    let (status, fetched) = support::request(
        &app,
        Method::GET,
        &format!("/api/v1/items/{item_id}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fetched["recurrence"]["occurrence_date"], json!(slot));

    // Клиент возвращает объект `recurrence` из ответа API без изменений, вместе
    // с полем только для чтения `occurrence_date`.
    let (status, patched) = support::request(
        &app,
        Method::PATCH,
        &format!("/api/v1/items/{item_id}"),
        Some(&token),
        Some(json!({
            "version": moved.version,
            "recurrence": fetched["recurrence"].clone()
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "patch item: {patched}");
    assert_eq!(
        patched["version"],
        json!(moved.version),
        "a repeated rule must not bump the version"
    );
    assert_eq!(patched["due_date"], json!(moved_to));
    assert_eq!(
        patched["recurrence"]["occurrence_date"],
        json!(slot),
        "a repeated rule keeps the rescheduled slot"
    );
    assert_eq!(
        patched["recurrence"]["anchor_date"],
        json!(anchor),
        "a repeated rule does not move the anchor"
    );

    let mut conn = db.pool.acquire().await.expect("connection");
    let current = items::get(&mut conn, household.household_id, item_id)
        .await
        .expect("item");
    let completed = items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        current.id,
        Some(current.version),
        "Europe/Moscow",
        Utc.with_ymd_and_hms(2026, 11, 6, 12, 0, 0)
            .single()
            .expect("time"),
    )
    .await
    .expect("complete item");
    assert_eq!(
        completed.due_date,
        Some(chrono::NaiveDate::from_ymd_opt(2026, 11, 17).expect("date"))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn control_characters_are_removed_from_stored_user_text() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let token =
        support::issue_token(&db.pool, household.household_id, &["ha:read", "ha:write"]).await;
    let app = support::app(&db.pool);

    let (status, list) = support::request(
        &app,
        Method::POST,
        "/api/v1/lists",
        Some(&token),
        Some(json!({"kind": "checklist", "name": "Дача\u{7} 2"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{list}");
    assert_eq!(list["name"], "Дача 2");
    let list_id = list["id"].as_str().expect("list id").to_owned();

    let (status, item) = support::request(
        &app,
        Method::POST,
        &format!("/api/v1/lists/{list_id}/items"),
        Some(&token),
        Some(json!({
            "title": "Купить\u{0} молоко\u{1}",
            "notes": "Строка 1\nСтрока 2\u{7}",
            "tags": ["До\u{2}м"],
            "subtasks": [{"title": "Проверить\u{3}"}],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{item}");
    assert_eq!(item["title"], "Купить молоко");
    assert_eq!(item["notes"], "Строка 1\nСтрока 2");
    assert_eq!(item["tags"][0]["name"], "дом");
    assert_eq!(item["subtasks"][0]["title"], "Проверить");
    let item_id = item["id"].as_str().expect("item id").to_owned();

    let (status, updated) = support::request(
        &app,
        Method::PATCH,
        &format!("/api/v1/items/{item_id}"),
        Some(&token),
        Some(json!({"version": 1, "title": "Новое\u{0} имя", "notes": "a\u{1}b"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["title"], "Новое имя");
    assert_eq!(updated["notes"], "ab");

    let (status, renamed) = support::request(
        &app,
        Method::PATCH,
        &format!("/api/v1/lists/{list_id}"),
        Some(&token),
        Some(json!({"name": "Дача\u{4} 3"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{renamed}");
    assert_eq!(renamed["name"], "Дача 3");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_arrays_and_invalid_values_are_rejected_without_writes() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let token =
        support::issue_token(&db.pool, household.household_id, &["ha:read", "ha:write"]).await;
    let app = support::app(&db.pool);

    let item_count = |pool: sqlx::PgPool, household_id: Uuid| async move {
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM items WHERE household_id = $1 AND deleted_at IS NULL",
        )
        .bind(household_id)
        .fetch_one(&pool)
        .await
        .expect("item count")
    };
    let list_count = |pool: sqlx::PgPool, household_id: Uuid| async move {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM lists WHERE household_id = $1")
            .bind(household_id)
            .fetch_one(&pool)
            .await
            .expect("list count")
    };
    let items_before = item_count(db.pool.clone(), household.household_id).await;
    let lists_before = list_count(db.pool.clone(), household.household_id).await;
    let tags_before: i64 = sqlx::query_scalar("SELECT count(*) FROM tags WHERE household_id = $1")
        .bind(household.household_id)
        .fetch_one(&db.pool)
        .await
        .expect("tag count");

    let tags: Vec<String> = (0..1_000).map(|n| format!("tag{n}")).collect();
    let subtasks: Vec<Value> = (0..51)
        .map(|n| json!({"title": format!("шаг {n}")}))
        .collect();
    let cases = [
        (
            format!("/api/v1/lists/{}/items", household.shopping_list_id),
            json!({"title": "Покупка", "tags": tags.clone()}),
        ),
        (
            format!("/api/v1/lists/{}/items", household.tasks_list_id),
            json!({"title": "Задача", "subtasks": subtasks}),
        ),
        (
            format!("/api/v1/lists/{}/items", household.tasks_list_id),
            json!({"title": "Задача", "due_date": "2026-13-45"}),
        ),
        (
            format!("/api/v1/lists/{}/items", household.tasks_list_id),
            json!({"title": "Задача", "priority": "critical"}),
        ),
        (
            format!("/api/v1/lists/{}/items", household.shopping_list_id),
            json!({"title": "Покупка", "quantity": 10_000}),
        ),
        (
            "/api/v1/lists".to_owned(),
            json!({"kind": "books", "name": "Новый список"}),
        ),
    ];

    for (uri, body) in cases {
        let (status, response) =
            support::request(&app, Method::POST, &uri, Some(&token), Some(body.clone())).await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{body} must be rejected, got {status}: {response}"
        );
    }

    assert_eq!(
        item_count(db.pool.clone(), household.household_id).await,
        items_before,
        "a rejected request must not create an item"
    );
    assert_eq!(
        list_count(db.pool.clone(), household.household_id).await,
        lists_before,
        "a rejected request must not create a list"
    );
    let tags_after: i64 = sqlx::query_scalar("SELECT count(*) FROM tags WHERE household_id = $1")
        .bind(household.household_id)
        .fetch_one(&db.pool)
        .await
        .expect("tag count");
    assert_eq!(
        tags_after, tags_before,
        "a rejected request must not create a tag"
    );

    // PATCH проверяется отдельно: замена тегов и срока не должна применяться
    // частично, а версия элемента остаётся прежней.
    let (status, base) = support::request(
        &app,
        Method::POST,
        &format!("/api/v1/lists/{}/items", household.tasks_list_id),
        Some(&token),
        Some(json!({"title": "Эталон", "tags": ["дом"]})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{base}");
    let item_id = base["id"].as_str().expect("item id").to_owned();
    let item_uri = format!("/api/v1/items/{item_id}");

    let (status, response) = support::request(
        &app,
        Method::PATCH,
        &item_uri,
        Some(&token),
        Some(json!({"version": 1, "tags": tags})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "oversized tags must be rejected: {response}"
    );

    let (status, response) = support::request(
        &app,
        Method::PATCH,
        &item_uri,
        Some(&token),
        Some(json!({"version": 1, "due_date": "2026-13-45"})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "an invalid date must be rejected: {response}"
    );

    let (status, item) = support::request(&app, Method::GET, &item_uri, Some(&token), None).await;
    assert_eq!(status, StatusCode::OK, "{item}");
    assert_eq!(item["version"], 1);
    assert_eq!(item["title"], "Эталон");
    assert_eq!(item["due_date"], Value::Null);
    assert_eq!(item["tags"].as_array().expect("tags").len(), 1);
    assert_eq!(item["tags"][0]["name"], "дом");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_over_http_treats_sql_as_text_and_returns_200() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let token = support::issue_token(&db.pool, household.household_id, &["ha:read"]).await;
    let app = support::app(&db.pool);
    support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        support::new_item(household.tasks_list_id, "Обычная задача"),
    )
    .await;

    let (status, body) = support::request(
        &app,
        Method::GET,
        "/api/v1/search?q=%27%3B%20DROP%20TABLE%20items%3B%20--",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().expect("items").len(), 0);

    let (status, body) = support::request(
        &app,
        Method::GET,
        "/api/v1/search?q=%25",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().expect("items").len(), 0);

    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM items WHERE household_id = $1 AND deleted_at IS NULL",
    )
    .bind(household.household_id)
    .fetch_one(&db.pool)
    .await
    .expect("items table must still exist");
    assert_eq!(count, 1);
}
