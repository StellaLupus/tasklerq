//! Задача 3.5: tenant-изоляция. Токен одного household не видит и не меняет
//! ресурсы другого: каждый эндпоинт отвечает `404`, не раскрывая существование.

mod support;

use axum::http::{Method, StatusCode};
use serde_json::{Value, json};
use uuid::Uuid;

const SECRET_TITLE: &str = "Секретная задача семьи А";

struct Fixture {
    victim: support::TestHousehold,
    item_id: Uuid,
    item_version: i32,
    attacker_token: String,
    attacker_list: Uuid,
    attacker_item: Uuid,
    attacker_item_version: i32,
}

async fn fixture(pool: &sqlx::PgPool) -> Fixture {
    let victim = support::create_household(pool).await;
    let mut secret = support::new_item(victim.tasks_list_id, SECRET_TITLE);
    secret.assignee_user_id = Some(victim.member_id);
    secret.tags = vec!["секрет".into()];
    secret.due_date = Some(chrono::Utc::now().date_naive());
    let secret =
        support::create_item(pool, &victim, victim.tasks_list_id, &victim.owner(), secret).await;

    let attacker = support::create_household(pool).await;
    let own = support::create_item(
        pool,
        &attacker,
        attacker.tasks_list_id,
        &attacker.owner(),
        support::new_item(attacker.tasks_list_id, "Моя задача"),
    )
    .await;
    let attacker_token =
        support::issue_token(pool, attacker.household_id, &["ha:read", "ha:write"]).await;

    Fixture {
        victim,
        item_id: secret.id,
        item_version: secret.version,
        attacker_token,
        attacker_list: attacker.tasks_list_id,
        attacker_item: own.id,
        attacker_item_version: own.version,
    }
}

fn assert_hidden(status: StatusCode, body: &Value, case: &str) {
    assert_eq!(status, StatusCode::NOT_FOUND, "{case}: {body}");
    assert_eq!(body["error"]["code"], "not_found", "{case}");
    let rendered = body.to_string();
    assert!(!rendered.contains(SECRET_TITLE), "{case}: leaked title");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreign_resources_are_not_found_on_every_endpoint() {
    let db = support::db().await;
    let f = fixture(&db.pool).await;
    let app = support::app(&db.pool);
    let token = Some(f.attacker_token.as_str());
    let list = f.victim.tasks_list_id;
    let item = f.item_id;
    let version = f.item_version;

    let cases: Vec<(&str, Method, String, Option<Value>)> = vec![
        (
            "list items",
            Method::GET,
            format!("/api/v1/lists/{list}/items"),
            None,
        ),
        (
            "create item in foreign list",
            Method::POST,
            format!("/api/v1/lists/{list}/items"),
            Some(json!({"title": "Подмена"})),
        ),
        (
            "rename foreign list",
            Method::PATCH,
            format!("/api/v1/lists/{list}"),
            Some(json!({"name": "Взлом"})),
        ),
        (
            "delete foreign list",
            Method::DELETE,
            format!("/api/v1/lists/{list}"),
            None,
        ),
        (
            "get item",
            Method::GET,
            format!("/api/v1/items/{item}"),
            None,
        ),
        (
            "patch item",
            Method::PATCH,
            format!("/api/v1/items/{item}"),
            Some(json!({"version": version, "title": "Взлом"})),
        ),
        (
            "delete item",
            Method::DELETE,
            format!("/api/v1/items/{item}"),
            None,
        ),
        (
            "complete item",
            Method::POST,
            format!("/api/v1/items/{item}/complete"),
            Some(json!({"version": version})),
        ),
        (
            "reopen item",
            Method::POST,
            format!("/api/v1/items/{item}/reopen"),
            Some(json!({})),
        ),
        (
            "undo item",
            Method::POST,
            format!("/api/v1/items/{item}/undo"),
            Some(json!({})),
        ),
        (
            "move own item to a foreign list",
            Method::PATCH,
            format!("/api/v1/items/{}", f.attacker_item),
            Some(json!({"version": f.attacker_item_version, "list_id": list})),
        ),
    ];

    for (case, method, uri, body) in cases {
        let (status, response) = support::request(&app, method, &uri, token, body).await;
        assert_hidden(status, &response, case);
    }

    // Те же запросы без токена закрыты аутентификацией.
    let (status, _) = support::request(
        &app,
        Method::GET,
        &format!("/api/v1/items/{item}"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Владелец и сам видит свои данные тем же запросом: изоляция не ломает доступ.
    let owner_token =
        support::issue_token(&db.pool, f.victim.household_id, &["ha:read", "ha:write"]).await;
    let (status, body) = support::request(
        &app,
        Method::GET,
        &format!("/api/v1/items/{item}"),
        Some(&owner_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["title"], SECRET_TITLE);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn collections_never_contain_foreign_data() {
    let db = support::db().await;
    let f = fixture(&db.pool).await;
    let app = support::app(&db.pool);
    let token = Some(f.attacker_token.as_str());
    let today = chrono::Utc::now().date_naive();
    let end = today + chrono::Duration::days(7);

    let uris = [
        "/api/v1/household".to_owned(),
        "/api/v1/lists".to_owned(),
        "/api/v1/search?status=all".to_owned(),
        "/api/v1/search?status=all&q=Секретная".to_owned(),
        "/api/v1/search?status=all&tag=секрет".to_owned(),
        format!(
            "/api/v1/search?status=all&list_id={}",
            f.victim.tasks_list_id
        ),
        format!(
            "/api/v1/search?status=all&assignee_user_id={}",
            f.victim.member_id
        ),
        format!("/api/v1/calendar?start={today}&end={end}"),
        "/api/v1/snapshot".to_owned(),
    ];

    for uri in uris {
        let (status, body) = support::request(&app, Method::GET, &uri, token, None).await;
        // Фильтр по чужому списку или исполнителю — 404 или пустой результат,
        // но никогда не данные чужого household.
        assert!(
            status == StatusCode::OK || status == StatusCode::NOT_FOUND,
            "{uri}: {status}"
        );
        let rendered = body.to_string();
        assert!(!rendered.contains(SECRET_TITLE), "{uri}: leaked title");
        assert!(
            !rendered.contains(&f.victim.member_id.to_string())
                && !rendered.contains(&f.victim.owner_id.to_string()),
            "{uri}: leaked a foreign user id"
        );
        assert!(
            !rendered.contains(&f.item_id.to_string()),
            "{uri}: leaked a foreign item id"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn assigning_a_foreign_user_is_rejected() {
    let db = support::db().await;
    let f = fixture(&db.pool).await;
    let app = support::app(&db.pool);
    let token = Some(f.attacker_token.as_str());

    let (status, body) = support::request(
        &app,
        Method::POST,
        &format!("/api/v1/lists/{}/items", f.attacker_list),
        token,
        Some(json!({"title": "Назначение", "assignee_user_id": f.victim.member_id})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    let (status, body) = support::request(
        &app,
        Method::PATCH,
        &format!("/api/v1/items/{}", f.attacker_item),
        token,
        Some(json!({
            "version": f.attacker_item_version,
            "assignee_user_id": f.victim.owner_id
        })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
}
