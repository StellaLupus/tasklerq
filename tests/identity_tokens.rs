//! Задача 3.1: API-токены и административный CLI.

mod support;

use axum::http::{Method, StatusCode};
use chrono::{Duration, Utc};
use tasklerq::admin;
use tasklerq::auth::tokens::{generate_token, hash_token, validate_scopes};
use tasklerq::domain::error::DomainError;
use tasklerq::domain::identity::AuditEventType;
use tokio::process::Command;
use uuid::Uuid;

fn scopes(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

#[test]
fn generated_tokens_have_prefix_and_entropy() {
    let token = generate_token();
    assert!(token.starts_with("tlq_"));
    assert_eq!(token.len(), 4 + 43, "32 random bytes in base64url");
    assert_ne!(token, generate_token());
}

#[test]
fn scopes_are_validated_and_deduplicated() {
    let valid = validate_scopes(&scopes(&[
        "ha:read",
        "ha:write",
        "ha:read",
        "capture:write",
    ]))
    .expect("known scopes");
    assert_eq!(valid, scopes(&["ha:read", "ha:write", "capture:write"]));
    assert!(validate_scopes(&[]).is_err());
    assert!(validate_scopes(&scopes(&["admin"])).is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn issued_token_is_stored_hashed_and_authenticates() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let app = support::app(&db.pool);

    let issued = admin::create_token(
        &db.pool,
        Some(household.household_id),
        "Home Assistant",
        &scopes(&["ha:read"]),
        Some(30),
        Utc::now(),
    )
    .await
    .expect("token");
    assert!(issued.token.starts_with("tlq_"));
    assert_eq!(issued.record.prefix, issued.token[..12]);
    assert!(issued.record.expires_at.is_some());
    assert!(issued.record.last_used_at.is_none());

    // В базе нет открытого значения: только SHA-256.
    let (stored_hash,): (String,) =
        sqlx::query_as("SELECT token_hash FROM api_tokens WHERE id = $1")
            .bind(issued.record.id)
            .fetch_one(&db.pool)
            .await
            .expect("row");
    assert_eq!(stored_hash, hash_token(&issued.token));
    assert_ne!(stored_hash, issued.token);
    let (status, _) = support::request(
        &app,
        Method::GET,
        "/api/v1/household",
        Some(&issued.token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (last_used,): (Option<chrono::DateTime<Utc>>,) =
        sqlx::query_as("SELECT last_used_at FROM api_tokens WHERE id = $1")
            .bind(issued.record.id)
            .fetch_one(&db.pool)
            .await
            .expect("row");
    assert!(last_used.is_some(), "last_used_at is updated on use");

    let (status, body) = support::request(
        &app,
        Method::POST,
        "/api/v1/lists",
        Some(&issued.token),
        Some(serde_json::json!({"kind": "checklist", "name": "Чужое право"})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "scope ha:write is missing");
    assert_eq!(body["error"]["code"], "forbidden");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revoked_and_expired_tokens_are_rejected() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let app = support::app(&db.pool);
    let now = Utc::now();

    let issued = admin::create_token(
        &db.pool,
        Some(household.household_id),
        "Temporary",
        &scopes(&["ha:read"]),
        None,
        now,
    )
    .await
    .expect("token");

    let (record, revoked_now) = admin::revoke_token(&db.pool, issued.record.id, now)
        .await
        .expect("revoke");
    assert!(revoked_now);
    assert!(record.revoked_at.is_some());

    let (status, _) = support::request(
        &app,
        Method::GET,
        "/api/v1/household",
        Some(&issued.token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Повторный отзыв идемпотентен и не пишет второе событие аудита.
    let (_, revoked_again) = admin::revoke_token(&db.pool, issued.record.id, now)
        .await
        .expect("revoke again");
    assert!(!revoked_again);
    let events: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events
         WHERE household_id = $1 AND event_type = 'token_revoked'",
    )
    .bind(household.household_id)
    .fetch_one(&db.pool)
    .await
    .expect("count");
    assert_eq!(events, 1);

    // Истёкший токен: срок задаётся в прошлом напрямую в базе.
    let expiring = admin::create_token(
        &db.pool,
        Some(household.household_id),
        "Expiring",
        &scopes(&["ha:read"]),
        Some(1),
        now,
    )
    .await
    .expect("token");
    sqlx::query("UPDATE api_tokens SET expires_at = $2 WHERE id = $1")
        .bind(expiring.record.id)
        .bind(now - Duration::minutes(1))
        .execute(&db.pool)
        .await
        .expect("expire");
    let (status, _) = support::request(
        &app,
        Method::GET,
        "/api/v1/household",
        Some(&expiring.token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn token_input_is_validated_and_audited_without_secret() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let now = Utc::now();

    for (name, token_scopes, days) in [
        ("   ", scopes(&["ha:read"]), None),
        ("Bad scope", scopes(&["root"]), None),
        ("No scope", Vec::new(), None),
        ("Past expiry", scopes(&["ha:read"]), Some(0)),
        ("Negative expiry", scopes(&["ha:read"]), Some(-1)),
        (
            "Too long",
            scopes(&["ha:read"]),
            Some(admin::MAX_TOKEN_EXPIRY_DAYS + 1),
        ),
        // Раньше `now + Duration::days(i64::MAX)` вызывало панику chrono.
        ("Overflow", scopes(&["ha:read"]), Some(i64::MAX)),
    ] {
        let result = admin::create_token(
            &db.pool,
            Some(household.household_id),
            name,
            &token_scopes,
            days,
            now,
        )
        .await;
        assert!(
            matches!(result, Err(DomainError::Validation(_))),
            "{name}: {result:?}"
        );
    }

    let missing = admin::create_token(
        &db.pool,
        Some(Uuid::now_v7()),
        "Orphan",
        &scopes(&["ha:read"]),
        None,
        now,
    )
    .await;
    assert!(matches!(missing, Err(DomainError::NotFound)));

    let issued = admin::create_token(
        &db.pool,
        Some(household.household_id),
        "Audited",
        &scopes(&["ha:read", "capture:write"]),
        None,
        now,
    )
    .await
    .expect("token");

    let payloads: Vec<(AuditEventType, serde_json::Value)> = sqlx::query_as(
        "SELECT event_type, payload FROM audit_events
         WHERE household_id = $1 AND event_type = 'token_created'",
    )
    .bind(household.household_id)
    .fetch_all(&db.pool)
    .await
    .expect("events");
    assert_eq!(payloads.len(), 1);
    let rendered = payloads[0].1.to_string();
    assert!(!rendered.contains(&issued.token));
    assert!(!rendered.contains(&hash_token(&issued.token)));
    assert!(rendered.contains(&issued.record.prefix));
}

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_tasklerq"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_creates_lists_and_revokes_tokens() {
    let db = support::db().await;
    let url = support::database_url().await;
    let household = support::create_household(&db.pool).await;
    let app = support::app(&db.pool);

    let output = bin()
        .env("DATABASE_URL", &url)
        .args([
            "admin",
            "token",
            "create",
            "--household",
            &household.household_id.to_string(),
            "--name",
            "cli token",
            "--scopes",
            "ha:read,ha:write",
            "--expires-in-days",
            "7",
        ])
        .output()
        .await
        .expect("cli should run");
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).expect("utf8");
    let field = |name: &str| {
        stdout
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{name}: ")))
            .unwrap_or_else(|| panic!("missing {name} in {stdout}"))
            .to_owned()
    };
    let token = field("token");
    let id = field("id");
    assert_eq!(field("scopes"), "ha:read,ha:write");

    let (status, _) =
        support::request(&app, Method::GET, "/api/v1/lists", Some(&token), None).await;
    assert_eq!(status, StatusCode::OK);

    let list = bin()
        .env("DATABASE_URL", &url)
        .args([
            "admin",
            "token",
            "list",
            "--household",
            &household.household_id.to_string(),
        ])
        .output()
        .await
        .expect("cli should run");
    assert!(list.status.success());
    let listing = String::from_utf8(list.stdout).expect("utf8");
    assert!(listing.contains(&id) && listing.contains("active"));
    assert!(!listing.contains(&token), "list must not print secrets");

    let revoke = bin()
        .env("DATABASE_URL", &url)
        .args(["admin", "token", "revoke", &id])
        .output()
        .await
        .expect("cli should run");
    assert!(revoke.status.success(), "{revoke:?}");

    let (status, _) =
        support::request(&app, Method::GET, "/api/v1/lists", Some(&token), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let invalid = bin()
        .env("DATABASE_URL", &url)
        .args([
            "admin",
            "token",
            "create",
            "--household",
            &household.household_id.to_string(),
            "--name",
            "bad",
            "--scopes",
            "root",
        ])
        .output()
        .await
        .expect("cli should run");
    assert!(!invalid.status.success());
    let stderr = String::from_utf8(invalid.stderr).expect("utf8");
    assert!(stderr.contains("422"), "{stderr}");
}
