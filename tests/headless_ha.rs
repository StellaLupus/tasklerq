//! Headless-режим: сервис без Telegram отдаёт весь API, которым пользуется
//! компонент Home Assistant (`docs/ha-api.md` §9).

mod support;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use chrono::{Duration, Utc};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tasklerq::app::build_application;
use tasklerq::config::{Config, TelegramMode};
use tower::ServiceExt;
use uuid::Uuid;

async fn request(
    app: &Router,
    method: Method,
    uri: &str,
    token: Option<&str>,
    idempotency_key: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    if let Some(key) = idempotency_key {
        builder = builder.header("idempotency-key", key);
    }
    let body = match body {
        Some(body) => {
            builder = builder.header("content-type", "application/json");
            Body::from(serde_json::to_vec(&body).expect("json body"))
        }
        None => Body::empty(),
    };
    let response = app
        .clone()
        .oneshot(builder.body(body).expect("request should build"))
        .await
        .expect("router should respond");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body should be readable")
        .to_bytes();
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, json)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn headless_disabled_telegram_serves_the_home_assistant_scenario() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let database_url = support::database_url().await;

    // Тот же production-путь сборки конфигурации, но без токена бота и
    // webhook-секрета: в режиме `disabled` они не нужны.
    let config = Config::from_map([
        ("ENV", "development"),
        ("PUBLIC_BASE_URL", "http://127.0.0.1:18080"),
        ("DATABASE_URL", database_url.as_str()),
        ("TELEGRAM_MODE", "disabled"),
        ("LLM_CAPTURE_ENABLED", "false"),
    ])
    .expect("headless config without a bot token must load");
    assert_eq!(config.telegram.mode, TelegramMode::Disabled);
    assert!(config.telegram.bot_token.is_none());
    assert!(config.telegram.webhook_secret.is_none());

    let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let application = build_application(&config, db.pool.clone(), shutdown_rx)
        .await
        .expect("application should build in disabled mode");
    assert!(
        application.workers.is_empty(),
        "disabled Telegram must not start workers"
    );
    let app = application.router;

    let ha = support::issue_token(&db.pool, household.household_id, &["ha:read", "ha:write"]).await;
    let capture = support::issue_token(&db.pool, household.household_id, &["capture:write"]).await;

    // 1. Household: timezone нужен клиенту для календаря и unique_id.
    let (status, body) = request(
        &app,
        Method::GET,
        "/api/v1/household",
        Some(&ha),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["household"]["id"], household.household_id.to_string());
    assert_eq!(body["household"]["timezone"], "Europe/Moscow");
    assert_eq!(body["members"].as_array().expect("members").len(), 2);

    // 2. Полный снапшот без `since` и курсор для инкрементального.
    let (status, snapshot) =
        request(&app, Method::GET, "/api/v1/snapshot", Some(&ha), None, None).await;
    assert_eq!(status, StatusCode::OK, "{snapshot}");
    assert_eq!(snapshot["lists"].as_array().expect("lists").len(), 2);
    assert_eq!(snapshot["deleted_item_ids"], json!([]));
    let next_since = snapshot["next_since"]
        .as_str()
        .expect("next_since")
        .to_owned();

    // 3. Запись из HA: создать позицию, изменить её, получить конфликт версий.
    let item_uri = format!("/api/v1/lists/{}/items", household.tasks_list_id);
    let (status, item) = request(
        &app,
        Method::POST,
        &item_uri,
        Some(&ha),
        None,
        Some(json!({"title": "Проверить headless", "source": "ha_todo"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{item}");
    let item_id = item["id"].as_str().expect("item id").to_owned();
    assert_eq!(item["version"], 1);
    assert_eq!(item["source"], "ha_todo");

    let uri = format!("/api/v1/items/{item_id}");
    let (status, updated) = request(
        &app,
        Method::PATCH,
        &uri,
        Some(&ha),
        None,
        Some(json!({"version": 1, "title": "Проверить headless ещё раз"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["version"], 2);
    assert_eq!(updated["title"], "Проверить headless ещё раз");

    let (status, conflict) = request(
        &app,
        Method::PATCH,
        &uri,
        Some(&ha),
        None,
        Some(json!({"version": 1, "title": "Устаревшая версия"})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{conflict}");
    assert_eq!(conflict["error"]["code"], "conflict");
    assert_eq!(conflict["error"]["details"]["current_version"], 2);
    assert_eq!(
        conflict["error"]["details"]["item"]["title"],
        "Проверить headless ещё раз"
    );

    let (status, completed) = request(
        &app,
        Method::POST,
        &format!("/api/v1/items/{item_id}/complete"),
        Some(&ha),
        None,
        Some(json!({"version": 2})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{completed}");
    assert_eq!(completed["status"], "done");
    assert_eq!(completed["version"], 3);
    assert!(completed["completed_at"].is_string(), "{completed}");

    // 4. Удаление и появление id в инкрементальном снапшоте.
    let (status, body) = request(&app, Method::DELETE, &uri, Some(&ha), None, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    let timed_due = (Utc::now() + Duration::days(1)).to_rfc3339();
    let (status, timed_item) = request(
        &app,
        Method::POST,
        &item_uri,
        Some(&ha),
        None,
        Some(json!({"title": "Встреча headless", "due_at": timed_due})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{timed_item}");
    let timed_item_id = timed_item["id"].as_str().expect("timed item id").to_owned();

    let (status, incremental) = request(
        &app,
        Method::GET,
        &format!("/api/v1/snapshot?since={next_since}"),
        Some(&ha),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{incremental}");
    let deleted = incremental["deleted_item_ids"]
        .as_array()
        .expect("deleted_item_ids");
    assert!(
        deleted
            .iter()
            .any(|id| id.as_str() == Some(item_id.as_str())),
        "deleted item must be reported: {incremental}"
    );
    assert!(
        incremental["items"]
            .as_array()
            .expect("items")
            .iter()
            .any(|item| item["id"] == timed_item_id),
        "new item must be reported: {incremental}"
    );

    // 5. Calendar отдаёт timed-задачу в интервале.
    let today = Utc::now().date_naive();
    let start = (today - Duration::days(1)).format("%Y-%m-%d").to_string();
    let end = (today + Duration::days(3)).format("%Y-%m-%d").to_string();
    let (status, calendar) = request(
        &app,
        Method::GET,
        &format!("/api/v1/calendar?start={start}&end={end}"),
        Some(&ha),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{calendar}");
    let event = calendar["events"]
        .as_array()
        .expect("events")
        .iter()
        .find(|event| event["item_id"] == timed_item_id)
        .unwrap_or_else(|| panic!("timed event is missing: {calendar}"));
    assert_eq!(event["all_day"], false);
    assert_eq!(event["status"], "open");
    assert_eq!(event["title"], "Встреча headless");
    assert!(
        event["start"]
            .as_str()
            .is_some_and(|start| start.ends_with('Z')),
        "timed event must use RFC 3339 UTC: {event}"
    );

    // 6. Capture без LLM: fallback-позиция и отмена последнего голосового.
    let key = Uuid::now_v7().to_string();
    let (status, captured) = request(
        &app,
        Method::POST,
        "/api/v1/capture",
        Some(&capture),
        Some(&key),
        Some(json!({
            "text": "купить молоко",
            "source": "ha_voice",
            "mode": "auto",
            "hint": "shopping"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{captured}");
    assert_eq!(captured["status"], "applied");
    assert_eq!(captured["fallback"], true);
    assert!(captured["draft_id"].is_null());
    assert!(captured["expires_at"].is_null());
    assert_eq!(captured["clarifications"], json!([]));
    let items = captured["items"].as_array().expect("capture items");
    assert_eq!(items.len(), 1, "{captured}");
    assert_eq!(items[0]["title"], "купить молоко");
    assert!(items[0]["id"].is_string(), "{captured}");
    assert_eq!(items[0]["kind"], "shopping");
    assert_eq!(items[0]["list_id"], household.shopping_list_id.to_string());
    let speech = captured["speech"].as_str().expect("speech");
    assert!(speech.contains("Записал как есть"), "{speech}");
    let capture_id = captured["capture_id"]
        .as_str()
        .expect("capture_id")
        .to_owned();

    let (status, undone) = request(
        &app,
        Method::POST,
        "/api/v1/captures/undo-last",
        Some(&capture),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{undone}");
    assert_eq!(undone["status"], "undone");
    assert_eq!(undone["capture_id"], capture_id);
    assert_eq!(undone["removed"], 1);

    // 7. Webhook выключен: фактический ответ — 404 not_found.
    let (status, body) = request(
        &app,
        Method::POST,
        "/webhooks/telegram",
        None,
        None,
        Some(json!({"update_id": 1})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["error"]["code"], "not_found");
}
