//! Контрактные интеграционные тесты REST API v1.

mod support;

use std::collections::HashSet;

use axum::http::{Method, StatusCode};
use chrono::{Duration, Utc};
use serde_json::json;
use tasklerq::auth::tokens::hash_token;
use tasklerq::storage::repositories::tokens;
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
    assert_eq!(lists.as_array().expect("lists").len(), 2);

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
    assert_eq!(snapshot["lists"].as_array().expect("lists").len(), 2);
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
