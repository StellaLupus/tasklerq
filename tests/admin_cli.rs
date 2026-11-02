//! Задача 8.7: административный CLI через настоящий бинарник.
//!
//! Каждый тест получает отдельную базу на общем контейнере: счётчики очередей
//! и выгрузка не зависят от строк, созданных параллельными тестами.

mod support;

use std::process::{Output, Stdio};

use axum::http::{Method, StatusCode};
use chrono::Utc;
use serde_json::Value;
use sqlx::postgres::PgPoolOptions;
use tasklerq::auth::tokens::{SCOPES, hash_token};
use tasklerq::storage::migrations;
use tasklerq::storage::repositories::invites;
use tokio::process::Command;
use uuid::Uuid;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

struct CliDatabase {
    url: String,
    pool: sqlx::PgPool,
}

async fn cli_database() -> CliDatabase {
    let name = format!("cli_admin_{}", Uuid::now_v7().simple());
    support::create_database(&name).await;
    let url = support::database_url_for(&name).await;
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&url)
        .await
        .expect("the CLI database should accept connections");
    migrations::run(&pool)
        .await
        .expect("migrations should apply");
    CliDatabase { url, pool }
}

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_tasklerq"))
}

async fn run(url: &str, args: &[&str]) -> Output {
    bin()
        .env("DATABASE_URL", url)
        .env("RUST_LOG", "error")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .await
        .expect("the CLI should run")
}

async fn run_with_tenancy(url: &str, mode: &str, args: &[&str]) -> Output {
    bin()
        .env("DATABASE_URL", url)
        .env("TENANCY_MODE", mode)
        .env("RUST_LOG", "error")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .await
        .expect("the CLI should run")
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("utf8")
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("utf8")
}

fn field(text: &str, name: &str) -> String {
    text.lines()
        .find_map(|line| line.strip_prefix(&format!("{name}: ")))
        .unwrap_or_else(|| panic!("missing {name} in:\n{text}"))
        .to_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn token_create_help_lists_every_supported_scope() {
    let output = bin()
        .args(["admin", "token", "create", "--help"])
        .output()
        .await
        .expect("the CLI should run");
    assert!(output.status.success(), "{output:?}");
    let help = stdout(&output);

    for scope in SCOPES {
        assert!(
            help.contains(scope),
            "the help must list the supported scope {scope}:\n{help}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn household_list_shows_members_timezone_and_no_secrets() {
    let db = cli_database().await;

    let created = run(
        &db.url,
        &[
            "admin",
            "household",
            "create",
            "--name",
            "Семья CLI",
            "--timezone",
            "Asia/Yekaterinburg",
        ],
    )
    .await;
    assert!(created.status.success(), "{created:?}");
    let household_id = field(&stdout(&created), "id");

    let owner = run(
        &db.url,
        &[
            "admin",
            "user",
            "add",
            "--household",
            &household_id,
            "--telegram-id",
            "123456789",
            "--role",
            "owner",
            "--name",
            "Алексей",
        ],
    )
    .await;
    assert!(owner.status.success(), "{owner:?}");

    let listed = run(&db.url, &["admin", "household", "list"]).await;
    assert!(listed.status.success(), "{listed:?}");
    let text = stdout(&listed);

    let line = text
        .lines()
        .find(|line| line.starts_with(&household_id))
        .unwrap_or_else(|| panic!("household {household_id} missing:\n{text}"));
    let columns: Vec<&str> = line.split('\t').collect();
    assert_eq!(
        columns.len(),
        8,
        "id, name, timezone, owner, members, open tasks, active tokens, created_at"
    );
    assert_eq!(columns[1], "Семья CLI");
    assert_eq!(columns[2], "Asia/Yekaterinburg");
    assert_eq!(columns[3], "123456789", "owner Telegram id");
    assert_eq!(columns[4], "1", "one active member");
    assert_eq!(columns[5], "0", "no open tasks");
    assert_eq!(columns[6], "0", "no active tokens");
    assert!(!columns[7].is_empty(), "creation date is printed");

    for forbidden in ["token_hash", "tlq_", "secret", "password"] {
        assert!(
            !text.to_lowercase().contains(forbidden),
            "household list must not contain {forbidden}:\n{text}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn household_create_in_single_mode_rejects_a_second_household() {
    let db = cli_database().await;

    let first = run_with_tenancy(
        &db.url,
        "single",
        &["admin", "household", "create", "--name", "Первая"],
    )
    .await;
    assert!(first.status.success(), "{first:?}");

    let second = run_with_tenancy(
        &db.url,
        "single",
        &[
            "admin",
            "household",
            "create",
            "--name",
            "Вторая",
            "--owner-telegram-id",
            "222222222",
            "--owner-name",
            "Второй",
        ],
    )
    .await;
    assert!(!second.status.success(), "{second:?}");
    let combined = format!("{}{}", stdout(&second), stderr(&second));
    assert!(
        combined.contains("409"),
        "the CLI should report a 409 conflict:\n{combined}"
    );

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM households")
        .fetch_one(&db.pool)
        .await
        .expect("count");
    assert_eq!(count, 1);
    let owners: i64 =
        sqlx::query_scalar("SELECT count(*) FROM users WHERE telegram_user_id = 222222222")
            .fetch_one(&db.pool)
            .await
            .expect("owner count");
    assert_eq!(owners, 0, "the rejected owner must not be created");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn household_create_in_multi_mode_allows_a_second_household() {
    let db = cli_database().await;

    for name in ["Первая", "Вторая"] {
        let output = run_with_tenancy(
            &db.url,
            "multi",
            &["admin", "household", "create", "--name", name],
        )
        .await;
        assert!(output.status.success(), "{output:?}");
    }

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM households")
        .fetch_one(&db.pool)
        .await
        .expect("count");
    assert_eq!(count, 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn household_create_with_owner_creates_owner_and_default_lists() {
    let db = cli_database().await;

    let output = run(
        &db.url,
        &[
            "admin",
            "household",
            "create",
            "--name",
            "Семья с владельцем",
            "--timezone",
            "Asia/Yekaterinburg",
            "--owner-telegram-id",
            "123456789",
            "--owner-name",
            "Алексей",
        ],
    )
    .await;
    assert!(output.status.success(), "{output:?}");
    let text = stdout(&output);
    let household_id = field(&text, "id");
    let household_id = Uuid::parse_str(&household_id).expect("household id");
    assert_eq!(field(&text, "name"), "Семья с владельцем");
    assert_eq!(field(&text, "timezone"), "Asia/Yekaterinburg");
    assert_eq!(field(&text, "owner"), "123456789");

    let lists: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM lists WHERE household_id = $1 AND deleted_at IS NULL",
    )
    .bind(household_id)
    .fetch_one(&db.pool)
    .await
    .expect("lists");
    assert_eq!(lists, 2, "both default lists are created");

    let users: Vec<(i64, String, String)> = sqlx::query_as(
        "SELECT telegram_user_id, role::text, display_name FROM users
         WHERE household_id = $1 AND deleted_at IS NULL",
    )
    .bind(household_id)
    .fetch_all(&db.pool)
    .await
    .expect("users");
    assert_eq!(users.len(), 1, "exactly one owner is created");
    assert_eq!(users[0].0, 123456789);
    assert_eq!(users[0].1, "owner");
    assert_eq!(users[0].2, "Алексей");

    let chat_ids: Vec<i64> =
        sqlx::query_scalar("SELECT telegram_chat_id FROM users WHERE household_id = $1")
            .bind(household_id)
            .fetch_all(&db.pool)
            .await
            .expect("chat ids");
    assert_eq!(
        chat_ids,
        vec![123456789],
        "the private chat id equals the Telegram id"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn household_create_with_owner_requires_a_telegram_id() {
    let output = bin()
        .args([
            "admin",
            "household",
            "create",
            "--name",
            "Без ID",
            "--owner-name",
            "Алексей",
        ])
        .output()
        .await
        .expect("the CLI should run");

    assert!(!output.status.success(), "{output:?}");
    let combined = format!("{}{}", stdout(&output), stderr(&output));
    assert!(
        combined.contains("--owner-telegram-id"),
        "the owner name requires an owner Telegram id:\n{combined}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn household_create_rejects_a_non_positive_owner_telegram_id() {
    let output = bin()
        .args([
            "admin",
            "household",
            "create",
            "--name",
            "Нулевой ID",
            "--owner-telegram-id",
            "0",
        ])
        .output()
        .await
        .expect("the CLI should run");

    assert!(!output.status.success(), "{output:?}");
    let combined = format!("{}{}", stdout(&output), stderr(&output));
    assert!(
        combined.contains("--owner-telegram-id"),
        "the Telegram id must be positive:\n{combined}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn household_create_with_owner_rejects_a_telegram_id_from_another_household() {
    let db = cli_database().await;
    let telegram_id = "555555555";

    let first = run_with_tenancy(
        &db.url,
        "multi",
        &[
            "admin",
            "household",
            "create",
            "--name",
            "Первая",
            "--owner-telegram-id",
            telegram_id,
            "--owner-name",
            "Первый",
        ],
    )
    .await;
    assert!(first.status.success(), "{first:?}");

    let second = run_with_tenancy(
        &db.url,
        "multi",
        &[
            "admin",
            "household",
            "create",
            "--name",
            "Вторая",
            "--owner-telegram-id",
            telegram_id,
            "--owner-name",
            "Второй",
        ],
    )
    .await;
    assert!(!second.status.success(), "{second:?}");
    let combined = format!("{}{}", stdout(&second), stderr(&second));
    assert!(
        combined.contains("409"),
        "an occupied Telegram id is a conflict:\n{combined}"
    );

    let households: i64 = sqlx::query_scalar("SELECT count(*) FROM households")
        .fetch_one(&db.pool)
        .await
        .expect("households");
    assert_eq!(households, 1, "the failed command creates nothing");
    let users: i64 = sqlx::query_scalar("SELECT count(*) FROM users WHERE telegram_user_id = $1")
        .bind(telegram_id.parse::<i64>().expect("id"))
        .fetch_one(&db.pool)
        .await
        .expect("users");
    assert_eq!(users, 1, "the existing owner is unchanged");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn household_create_with_owner_rolls_back_when_the_owner_is_invalid() {
    let db = cli_database().await;
    let long_name = "я".repeat(65);

    let output = run(
        &db.url,
        &[
            "admin",
            "household",
            "create",
            "--name",
            "Откат",
            "--owner-telegram-id",
            "777777777",
            "--owner-name",
            &long_name,
        ],
    )
    .await;
    assert!(!output.status.success(), "{output:?}");
    let combined = format!("{}{}", stdout(&output), stderr(&output));
    assert!(
        combined.contains("422"),
        "an invalid owner name is a validation error:\n{combined}"
    );

    let households: i64 = sqlx::query_scalar("SELECT count(*) FROM households")
        .fetch_one(&db.pool)
        .await
        .expect("households");
    assert_eq!(
        households, 0,
        "the household inserted before the owner is rolled back"
    );
    let users: i64 = sqlx::query_scalar("SELECT count(*) FROM users")
        .fetch_one(&db.pool)
        .await
        .expect("users");
    assert_eq!(users, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn household_create_without_owner_prints_the_add_user_hint() {
    let db = cli_database().await;

    let output = run(
        &db.url,
        &["admin", "household", "create", "--name", "Без владельца"],
    )
    .await;
    assert!(output.status.success(), "{output:?}");
    let text = stdout(&output);
    let household_id = field(&text, "id");
    let expected = format!(
        "tasklerq admin user add --telegram-id <id> --role owner --household {household_id}"
    );
    assert!(
        text.lines().any(|line| line == expected),
        "the CLI should print the owner hint:\n{text}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn household_delete_revokes_access_and_records_a_system_audit() {
    let db = cli_database().await;
    let household = support::create_household(&db.pool).await;
    let app = support::app(&db.pool);
    let token = support::issue_token(&db.pool, household.household_id, &["ha:read"]).await;
    let now = Utc::now();
    let mut tx = db.pool.begin().await.expect("tx");
    let invite = invites::create(&mut tx, household.household_id, &household.owner(), now)
        .await
        .expect("invite");
    tx.commit().await.expect("commit");

    let household_id = household.household_id.to_string();
    let output = run(
        &db.url,
        &[
            "admin",
            "household",
            "delete",
            "--household",
            &household_id,
            "--yes",
        ],
    )
    .await;
    assert!(output.status.success(), "{output:?}");
    let text = stdout(&output);
    assert_eq!(field(&text, "id"), household_id);
    assert_eq!(field(&text, "members"), "2");
    assert_eq!(field(&text, "revoked_tokens"), "1");
    assert_eq!(field(&text, "revoked_invites"), "1");
    assert!(!text.contains(&token), "the token value is not printed");

    let (deleted_at,): (Option<chrono::DateTime<Utc>>,) =
        sqlx::query_as("SELECT deleted_at FROM households WHERE id = $1")
            .bind(household.household_id)
            .fetch_one(&db.pool)
            .await
            .expect("household row");
    assert!(deleted_at.is_some());
    let active_members: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM users WHERE household_id = $1 AND deleted_at IS NULL",
    )
    .bind(household.household_id)
    .fetch_one(&db.pool)
    .await
    .expect("active members");
    assert_eq!(active_members, 0, "all members are marked deleted");
    let members: i64 = sqlx::query_scalar("SELECT count(*) FROM users WHERE household_id = $1")
        .bind(household.household_id)
        .fetch_one(&db.pool)
        .await
        .expect("members");
    assert_eq!(members, 2, "the rows stay until retention");
    let (token_revoked,): (Option<chrono::DateTime<Utc>>,) =
        sqlx::query_as("SELECT revoked_at FROM api_tokens WHERE household_id = $1")
            .bind(household.household_id)
            .fetch_one(&db.pool)
            .await
            .expect("token");
    assert!(token_revoked.is_some());
    let (invite_revoked,): (Option<chrono::DateTime<Utc>>,) =
        sqlx::query_as("SELECT revoked_at FROM invites WHERE id = $1")
            .bind(invite.invite.id)
            .fetch_one(&db.pool)
            .await
            .expect("invite");
    assert!(invite_revoked.is_some());

    let (actor_type, actor_user_id, payload): (String, Option<Uuid>, Value) = sqlx::query_as(
        "SELECT actor_type::text, actor_user_id, payload
         FROM audit_events
         WHERE household_id = $1 AND event_type::text = 'household_deleted'
         ORDER BY created_at DESC, id DESC
         LIMIT 1",
    )
    .bind(household.household_id)
    .fetch_one(&db.pool)
    .await
    .expect("household_deleted audit");
    assert_eq!(actor_type, "system");
    assert_eq!(actor_user_id, None);
    assert_eq!(payload["members"].as_u64(), Some(2));
    assert_eq!(payload["revoked_tokens"].as_u64(), Some(1));
    assert_eq!(payload["revoked_invites"].as_u64(), Some(1));
    assert!(
        payload.get("actor_name").is_none(),
        "a system actor has no display name"
    );

    let (status, _) =
        support::request(&app, Method::GET, "/api/v1/household", Some(&token), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn household_delete_without_yes_in_non_interactive_mode_changes_nothing() {
    let db = cli_database().await;
    let household = support::create_household(&db.pool).await;
    let _token = support::issue_token(&db.pool, household.household_id, &["ha:read"]).await;
    let now = Utc::now();
    let mut tx = db.pool.begin().await.expect("tx");
    let invite = invites::create(&mut tx, household.household_id, &household.owner(), now)
        .await
        .expect("invite");
    tx.commit().await.expect("commit");

    let household_id = household.household_id.to_string();
    let output = run(
        &db.url,
        &["admin", "household", "delete", "--household", &household_id],
    )
    .await;
    assert!(!output.status.success(), "{output:?}");
    let combined = format!("{}{}", stdout(&output), stderr(&output));
    assert!(
        combined.contains("--yes"),
        "the CLI asks for --yes without a terminal:\n{combined}"
    );

    let (deleted_at,): (Option<chrono::DateTime<Utc>>,) =
        sqlx::query_as("SELECT deleted_at FROM households WHERE id = $1")
            .bind(household.household_id)
            .fetch_one(&db.pool)
            .await
            .expect("household row");
    assert!(deleted_at.is_none());
    let active_members: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM users WHERE household_id = $1 AND deleted_at IS NULL",
    )
    .bind(household.household_id)
    .fetch_one(&db.pool)
    .await
    .expect("active members");
    assert_eq!(active_members, 2);
    let (token_revoked,): (Option<chrono::DateTime<Utc>>,) =
        sqlx::query_as("SELECT revoked_at FROM api_tokens WHERE household_id = $1")
            .bind(household.household_id)
            .fetch_one(&db.pool)
            .await
            .expect("token");
    assert!(token_revoked.is_none());
    let (invite_revoked,): (Option<chrono::DateTime<Utc>>,) =
        sqlx::query_as("SELECT revoked_at FROM invites WHERE id = $1")
            .bind(invite.invite.id)
            .fetch_one(&db.pool)
            .await
            .expect("invite");
    assert!(invite_revoked.is_none());
    let audits: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events
         WHERE household_id = $1 AND event_type::text = 'household_deleted'",
    )
    .bind(household.household_id)
    .fetch_one(&db.pool)
    .await
    .expect("audit count");
    assert_eq!(audits, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn household_delete_reports_not_found_for_deleted_and_unknown_households() {
    let db = cli_database().await;
    let household = support::create_household(&db.pool).await;
    let household_id = household.household_id.to_string();

    let first = run(
        &db.url,
        &[
            "admin",
            "household",
            "delete",
            "--household",
            &household_id,
            "--yes",
        ],
    )
    .await;
    assert!(first.status.success(), "{first:?}");

    let again = run(
        &db.url,
        &[
            "admin",
            "household",
            "delete",
            "--household",
            &household_id,
            "--yes",
        ],
    )
    .await;
    assert!(!again.status.success(), "{again:?}");
    let combined = format!("{}{}", stdout(&again), stderr(&again));
    assert!(
        combined.contains("404"),
        "a second delete reports not found:\n{combined}"
    );

    let unknown = Uuid::now_v7().to_string();
    let missing = run(
        &db.url,
        &[
            "admin",
            "household",
            "delete",
            "--household",
            &unknown,
            "--yes",
        ],
    )
    .await;
    assert!(!missing.status.success(), "{missing:?}");
    let combined = format!("{}{}", stdout(&missing), stderr(&missing));
    assert!(
        combined.contains("404"),
        "an unknown household reports not found:\n{combined}"
    );

    let audits: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events WHERE event_type::text = 'household_deleted'",
    )
    .fetch_one(&db.pool)
    .await
    .expect("audit count");
    assert_eq!(audits, 1, "failed deletions write no audit");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn household_list_shows_owner_open_tasks_and_active_tokens_without_secrets() {
    let db = cli_database().await;
    let ours = support::create_household(&db.pool).await;
    let owner_telegram_id: i64 =
        sqlx::query_scalar("SELECT telegram_user_id FROM users WHERE id = $1")
            .bind(ours.owner_id)
            .fetch_one(&db.pool)
            .await
            .expect("owner telegram id");

    let other = run_with_tenancy(
        &db.url,
        "multi",
        &[
            "admin",
            "household",
            "create",
            "--name",
            "Без владельца",
            "--timezone",
            "Asia/Yekaterinburg",
        ],
    )
    .await;
    assert!(other.status.success(), "{other:?}");
    let other_id = Uuid::parse_str(&field(&stdout(&other), "id")).expect("other id");
    let other_tasks: Uuid = sqlx::query_scalar(
        "SELECT id FROM lists WHERE household_id = $1 AND kind = 'tasks' AND deleted_at IS NULL",
    )
    .bind(other_id)
    .fetch_one(&db.pool)
    .await
    .expect("other tasks list");

    let now = Utc::now();
    let insert_item = |list_id: Uuid, title: &'static str, status: &'static str, deleted: bool| {
        let pool = db.pool.clone();
        let household_id = if list_id == other_tasks {
            other_id
        } else {
            ours.household_id
        };
        async move {
            sqlx::query(
                "INSERT INTO items (id, household_id, list_id, title, status, completed_at,
                                    deleted_at, created_at, updated_at)
                 VALUES ($1, $2, $3, $4, $5::item_status,
                         CASE WHEN $5 = 'done' THEN now() ELSE NULL END,
                         CASE WHEN $6 THEN now() ELSE NULL END, $7, $7)",
            )
            .bind(Uuid::now_v7())
            .bind(household_id)
            .bind(list_id)
            .bind(title)
            .bind(status)
            .bind(deleted)
            .bind(now)
            .execute(&pool)
            .await
            .expect("item");
        }
    };

    // Открытая задача, выполненная и удалённая задачи, открытая покупка.
    insert_item(
        ours.tasks_list_id,
        "СЕКРЕТНАЯ ЗАДАЧА ОТКРЫТАЯ",
        "open",
        false,
    )
    .await;
    insert_item(
        ours.tasks_list_id,
        "СЕКРЕТНАЯ ЗАДАЧА ВЫПОЛНЕННАЯ",
        "done",
        false,
    )
    .await;
    insert_item(
        ours.tasks_list_id,
        "СЕКРЕТНАЯ ЗАДАЧА УДАЛЁННАЯ",
        "open",
        true,
    )
    .await;
    insert_item(ours.shopping_list_id, "СЕКРЕТНАЯ ПОКУПКА", "open", false).await;
    insert_item(other_tasks, "ЧУЖАЯ ОТКРЫТАЯ 1", "open", false).await;
    insert_item(other_tasks, "ЧУЖАЯ ОТКРЫТАЯ 2", "open", false).await;
    insert_item(other_tasks, "ЧУЖАЯ ВЫПОЛНЕННАЯ", "done", false).await;

    let active = support::issue_token(&db.pool, ours.household_id, &["ha:read"]).await;
    let revoked = support::issue_token(&db.pool, ours.household_id, &["ha:read"]).await;
    sqlx::query("UPDATE api_tokens SET revoked_at = now() WHERE token_hash = $1")
        .bind(hash_token(&revoked))
        .execute(&db.pool)
        .await
        .expect("revoke token");
    let expired = support::issue_token(&db.pool, ours.household_id, &["ha:read"]).await;
    sqlx::query(
        "UPDATE api_tokens SET expires_at = now() - interval '1 day' WHERE token_hash = $1",
    )
    .bind(hash_token(&expired))
    .execute(&db.pool)
    .await
    .expect("expire token");
    let other_active = support::issue_token(&db.pool, other_id, &["ha:read"]).await;

    let listed = run(&db.url, &["admin", "household", "list"]).await;
    assert!(listed.status.success(), "{listed:?}");
    let text = stdout(&listed);

    let line = |id: Uuid| -> Vec<String> {
        let id = id.to_string();
        text.lines()
            .find(|line| line.starts_with(&id))
            .unwrap_or_else(|| panic!("household {id} missing:\n{text}"))
            .split('\t')
            .map(str::to_owned)
            .collect()
    };

    let ours_columns = line(ours.household_id);
    assert_eq!(
        ours_columns.len(),
        8,
        "id, name, timezone, owner, members, open tasks, active tokens, created_at"
    );
    assert_eq!(ours_columns[0], ours.household_id.to_string());
    assert_eq!(ours_columns[2], "Europe/Moscow");
    assert_eq!(ours_columns[3], owner_telegram_id.to_string());
    assert_eq!(ours_columns[4], "2", "the owner and the member");
    assert_eq!(ours_columns[5], "1", "only the open task is counted");
    assert_eq!(
        ours_columns[6], "1",
        "revoked and expired tokens are not counted"
    );
    assert!(!ours_columns[7].is_empty(), "creation date is printed");

    let other_columns = line(other_id);
    assert_eq!(other_columns.len(), 8);
    assert_eq!(other_columns[1], "Без владельца");
    assert_eq!(other_columns[2], "Asia/Yekaterinburg");
    assert_eq!(other_columns[3], "-", "a household without owner shows -");
    assert_eq!(other_columns[4], "0");
    assert_eq!(other_columns[5], "2", "the completed task is not counted");
    assert_eq!(other_columns[6], "1");
    assert!(!other_columns[7].is_empty());

    for forbidden in [
        "СЕКРЕТНАЯ ЗАДАЧА",
        "СЕКРЕТНАЯ ПОКУПКА",
        "ЧУЖАЯ ОТКРЫТАЯ",
        "ЧУЖАЯ ВЫПОЛНЕННАЯ",
        "test token",
        "tlq_",
        active.as_str(),
        revoked.as_str(),
        expired.as_str(),
        other_active.as_str(),
    ] {
        assert!(
            !text.contains(forbidden),
            "household list must not contain {forbidden}:\n{text}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queue_stats_counts_statuses_without_payload_content() {
    let db = cli_database().await;
    let household = support::create_household(&db.pool).await;
    let now = Utc::now();

    for (update_id, status, available_at) in [
        (1_i64, "pending", now - chrono::Duration::seconds(90)),
        (2, "pending", now),
        (3, "processed", now),
        (4, "dead", now),
    ] {
        sqlx::query(
            "INSERT INTO telegram_updates
                 (update_id, payload, status, available_at, received_at)
             VALUES ($1, '{\"message\":{\"text\":\"PAYLOAD-SECRET-CONTENT\"}}'::jsonb,
                     $2::update_status, $3, $3)",
        )
        .bind(update_id)
        .bind(status)
        .bind(available_at)
        .execute(&db.pool)
        .await
        .expect("telegram update");
    }

    for (status, kind) in [
        ("pending", "message"),
        ("sent", "message"),
        ("dead", "message"),
        ("skipped", "screen"),
    ] {
        sqlx::query(
            "INSERT INTO outbox (chat_id, kind, payload, status, next_attempt_at, created_at)
             VALUES (1, $1::outbox_kind, '{\"html\":\"OUTBOX-SECRET-CONTENT\"}'::jsonb,
                     $2::outbox_status, $3, $3)",
        )
        .bind(kind)
        .bind(status)
        .bind(now)
        .execute(&db.pool)
        .await
        .expect("outbox row");
    }

    for status in ["pending", "running", "done", "dead"] {
        sqlx::query(
            "INSERT INTO capture_jobs
                 (id, household_id, chat_id, payload, status, available_at, created_at)
             VALUES ($1, $2, 1, '{\"text\":\"CAPTURE-SECRET-CONTENT\"}'::jsonb,
                     $3::capture_job_status, $4, $4)",
        )
        .bind(Uuid::now_v7())
        .bind(household.household_id)
        .bind(status)
        .bind(now)
        .execute(&db.pool)
        .await
        .expect("capture job");
    }

    for (index, status) in ["pending", "sent", "cancelled"].into_iter().enumerate() {
        sqlx::query(
            "INSERT INTO notification_deliveries
                 (id, household_id, user_id, item_id, occurrence_key, kind, status,
                  scheduled_at, created_at, updated_at)
             VALUES ($1, $2, $3, NULL, $4, 'reminder'::notification_kind,
                     $5::notification_status, $6, $6, $6)",
        )
        .bind(Uuid::now_v7())
        .bind(household.household_id)
        .bind(household.owner_id)
        .bind(format!("occurrence-{index}"))
        .bind(status)
        .bind(now)
        .execute(&db.pool)
        .await
        .expect("notification delivery");
    }

    let output = run(&db.url, &["admin", "queue", "stats"]).await;
    assert!(output.status.success(), "{output:?}");
    let text = stdout(&output);

    let section = |name: &str| -> String {
        let start = text
            .lines()
            .position(|line| line == format!("{name}:"))
            .unwrap_or_else(|| panic!("missing section {name}:\n{text}"));
        text.lines()
            .skip(start + 1)
            .take_while(|line| line.starts_with("  "))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let status_line = |name: &str, status: &str| -> i64 {
        section(name)
            .lines()
            .find_map(|line| line.strip_prefix(&format!("  {status}: ")))
            .unwrap_or_else(|| panic!("missing {name}.{status}:\n{text}"))
            .parse()
            .expect("status count")
    };

    assert_eq!(status_line("telegram_updates", "pending"), 2);
    assert_eq!(status_line("telegram_updates", "processed"), 1);
    assert_eq!(status_line("telegram_updates", "dead"), 1);
    assert_eq!(status_line("outbox", "pending"), 1);
    assert_eq!(status_line("outbox", "sent"), 1);
    assert_eq!(status_line("outbox", "dead"), 1);
    assert_eq!(status_line("outbox", "skipped"), 1);
    for status in ["pending", "running", "done", "dead"] {
        assert_eq!(status_line("capture_jobs", status), 1);
    }
    for status in ["pending", "sent", "cancelled"] {
        assert_eq!(status_line("notification_deliveries", status), 1);
    }

    let age = field(&text, "oldest_ready_age_seconds")
        .parse::<i64>()
        .expect("age in seconds");
    assert!(age >= 60, "the oldest ready update is about 90s old: {age}");

    for secret in [
        "PAYLOAD-SECRET-CONTENT",
        "OUTBOX-SECRET-CONTENT",
        "CAPTURE-SECRET-CONTENT",
        "message",
        "html",
    ] {
        assert!(
            !text.contains(secret),
            "queue stats must not print message content ({secret}):\n{text}"
        );
    }
}

fn webhook_env(url: &str, base_url: &str, token: &str) -> Command {
    let mut command = bin();
    command
        .env("DATABASE_URL", url)
        .env("ENV", "development")
        .env("PUBLIC_BASE_URL", "http://127.0.0.1:8080")
        .env("TELEGRAM_MODE", "polling")
        .env("TELEGRAM_BOT_TOKEN", token)
        .env("TELEGRAM_API_BASE_URL", base_url)
        .env("LLM_CAPTURE_ENABLED", "false")
        .env("RUST_LOG", "error");
    command
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn webhook_info_reports_state_without_the_bot_token() {
    const BOT_TOKEN: &str = "123456:SECRET-bot-token-webhook-info";
    const DB_PASSWORD: &str = "db-secret-password";

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/bot{BOT_TOKEN}/getWebhookInfo")))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "result": {
                "url": "https://user:password@example.test/webhooks/telegram?token=1",
                "pending_update_count": 7,
                "last_error_date": 1_700_000_000,
                "last_error_message": "connect https://user:password@example.test/hook failed",
                "max_connections": 40,
                "ip_address": "203.0.113.5"
            }
        })))
        .mount(&server)
        .await;

    let url = format!("postgres://tasklerq:{DB_PASSWORD}@127.0.0.1:5432/tasklerq");
    let output = webhook_env(&url, &server.uri(), BOT_TOKEN)
        .args(["admin", "webhook", "info"])
        .output()
        .await
        .expect("the CLI should run");
    assert!(output.status.success(), "{output:?}");

    let text = stdout(&output);
    assert!(
        text.contains("url: https://example.test/webhooks/telegram"),
        "{text}"
    );
    assert!(!text.contains("user:password"), "{text}");
    assert!(
        !text.contains("token=1"),
        "the webhook URL query must not be printed:\n{text}"
    );
    assert_eq!(field(&text, "pending_update_count"), "7");
    assert!(text.contains("last_error_date: 2023-11-14"), "{text}");
    assert_eq!(field(&text, "max_connections"), "40");
    assert_eq!(field(&text, "ip_address"), "203.0.113.5");

    for secret in [BOT_TOKEN, DB_PASSWORD, "user:password"] {
        assert!(!text.contains(secret), "stdout leaked {secret}:\n{text}");
        assert!(
            !stderr(&output).contains(secret),
            "stderr leaked {secret}:\n{}",
            stderr(&output)
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn webhook_info_network_error_hides_the_bot_token_and_database_password() {
    const BOT_TOKEN: &str = "654321:SECRET-bot-token-network-error";
    const DB_PASSWORD: &str = "db-secret-password";

    let url = format!("postgres://tasklerq:{DB_PASSWORD}@127.0.0.1:5432/tasklerq");
    let output = webhook_env(&url, "http://127.0.0.1:1", BOT_TOKEN)
        .args(["admin", "webhook", "info"])
        .output()
        .await
        .expect("the CLI should run");

    assert!(!output.status.success(), "{output:?}");
    let combined = format!("{}{}", stdout(&output), stderr(&output));
    for secret in [BOT_TOKEN, DB_PASSWORD] {
        assert!(
            !combined.contains(secret),
            "webhook info leaked {secret}:\n{combined}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn export_contains_own_data_only_and_no_secrets() {
    use tasklerq::domain::models::{ListKind, RecurrenceFreq};
    use tasklerq::storage::repositories::{items, lists};

    let db = cli_database().await;
    let ours = support::create_household(&db.pool).await;
    let other = support::create_household(&db.pool).await;

    let mut item = support::new_item(ours.tasks_list_id, "Купить молоко");
    item.tags = vec!["молоко".to_owned()];
    item.subtasks = vec![support::subtask("Взять сумку")];
    let item = support::create_item(&db.pool, &ours, ours.tasks_list_id, &ours.owner(), item).await;
    assert_eq!(item.tags.len(), 1);

    // Архивный список остаётся в выгрузке.
    let now = Utc::now();
    let mut conn = db.pool.acquire().await.expect("connection");
    let archived_list = lists::create(
        &mut conn,
        ours.household_id,
        ListKind::Checklist,
        "Архивный список",
        now,
    )
    .await
    .expect("list");
    lists::update(
        &mut conn,
        ours.household_id,
        archived_list.id,
        None,
        Some(true),
        true,
        now,
    )
    .await
    .expect("archive");

    // Выполненная задача.
    let completed = support::create_item(
        &db.pool,
        &ours,
        ours.tasks_list_id,
        &ours.owner(),
        support::new_item(ours.tasks_list_id, "Выполненная задача"),
    )
    .await;
    items::complete(
        &mut conn,
        ours.household_id,
        &ours.owner(),
        completed.id,
        None,
        "Europe/Moscow",
        now,
    )
    .await
    .expect("complete");

    // Задача с назначением и повторением.
    let mut recurring = support::new_item(ours.tasks_list_id, "Полить цветы");
    recurring.assignee_user_id = Some(ours.member_id);
    recurring.recurrence = Some(support::recurrence(RecurrenceFreq::Daily, Vec::new()));
    let recurring = support::create_item(
        &db.pool,
        &ours,
        ours.tasks_list_id,
        &ours.owner(),
        recurring,
    )
    .await;

    let foreign = support::create_item(
        &db.pool,
        &other,
        other.tasks_list_id,
        &other.owner(),
        support::new_item(other.tasks_list_id, "ЧУЖАЯ ЗАДАЧА"),
    )
    .await;

    let deleted = support::create_item(
        &db.pool,
        &ours,
        ours.tasks_list_id,
        &ours.owner(),
        support::new_item(ours.tasks_list_id, "УДАЛЁННАЯ ЗАДАЧА"),
    )
    .await;
    sqlx::query("UPDATE items SET deleted_at = now() WHERE id = $1")
        .bind(deleted.id)
        .execute(&mut *conn)
        .await
        .expect("soft delete");

    // Напоминание по задаче попадает в блок `reminders`.
    sqlx::query(
        "INSERT INTO notification_deliveries
             (id, household_id, user_id, item_id, occurrence_key, kind, status,
              scheduled_at, created_at, updated_at)
         VALUES ($1, $2, $3, $4, 'export-occurrence', 'reminder'::notification_kind,
                 'pending'::notification_status, $5, $5, $5)",
    )
    .bind(Uuid::now_v7())
    .bind(ours.household_id)
    .bind(ours.owner_id)
    .bind(recurring.id)
    .bind(now)
    .execute(&mut *conn)
    .await
    .expect("delivery");
    drop(conn);

    // Токены, callback-токены и llm_runs не должны попасть в выгрузку.
    let token = support::issue_token(&db.pool, ours.household_id, &["ha:read"]).await;
    let token_hash = hash_token(&token);
    sqlx::query(
        "INSERT INTO callback_tokens
             (token_hash, telegram_user_id, household_id, action, payload, expires_at)
         VALUES ('CALLBACK-SECRET-HASH', 1, $1, 'done', '{\"secret\":\"CALLBACK-PAYLOAD\"}', now())",
    )
    .bind(ours.household_id)
    .execute(&db.pool)
    .await
    .expect("callback token");
    sqlx::query(
        "INSERT INTO llm_runs
             (id, household_id, user_id, source, mode, status, latency_ms, created_at)
         VALUES ($1, $2, $3, 'telegram', 'auto', 'success', 1, now())",
    )
    .bind(Uuid::now_v7())
    .bind(ours.household_id)
    .bind(ours.owner_id)
    .execute(&db.pool)
    .await
    .expect("llm run");

    let household_id = ours.household_id.to_string();
    let output = run(&db.url, &["admin", "export", "--household", &household_id]).await;
    assert!(output.status.success(), "{output:?}");
    let text = stdout(&output);
    let export: Value = serde_json::from_str(&text).expect("stdout is JSON");
    assert_eq!(export["household"]["id"], household_id);
    assert_eq!(export["household"]["timezone"], "Europe/Moscow");

    let items = export["items"].as_array().expect("items array");
    let exported = items
        .iter()
        .find(|item| item["title"] == "Купить молоко")
        .expect("the created item is exported");
    assert_eq!(exported["title"], "Купить молоко");
    assert_eq!(exported["tags"][0]["name"], "молоко");
    assert_eq!(exported["subtasks"][0]["title"], "Взять сумку");

    // Архивный список остаётся в выгрузке.
    let lists = export["lists"].as_array().expect("lists array");
    let archived = lists
        .iter()
        .find(|list| list["name"] == "Архивный список")
        .expect("the archived list is exported");
    assert_eq!(archived["is_archived"], true);

    // Выполненная задача с назначением, повторением и напоминанием.
    let completed_export = items
        .iter()
        .find(|item| item["title"] == "Выполненная задача")
        .expect("the completed item is exported");
    assert_eq!(completed_export["status"], "done");

    let recurring_export = items
        .iter()
        .find(|item| item["title"] == "Полить цветы")
        .expect("the recurring item is exported");
    assert_eq!(
        recurring_export["assignee_user_id"],
        ours.member_id.to_string()
    );
    assert!(
        recurring_export["recurrence"].is_object(),
        "the recurrence rule is exported: {recurring_export}"
    );
    let reminders = recurring_export["reminders"]
        .as_array()
        .expect("reminders array");
    let reminder = reminders
        .iter()
        .find(|reminder| reminder["occurrence_key"] == "export-occurrence")
        .expect("the reminder attached to the item is exported");
    assert_eq!(reminder["kind"], "reminder");
    assert_eq!(reminder["status"], "pending");

    assert!(
        items.iter().all(|item| item["title"] != "ЧУЖАЯ ЗАДАЧА"),
        "another household must not be exported:\n{text}"
    );
    assert!(
        items.iter().all(|item| item["title"] != "УДАЛЁННАЯ ЗАДАЧА"),
        "soft-deleted items are excluded:\n{text}"
    );

    let members = export["members"].as_array().expect("members array");
    assert_eq!(members.len(), 2);
    for member in members {
        assert!(member.get("display_name").is_some());
        assert!(member.get("username").is_some());
        assert!(member.get("role").is_some());
        assert!(
            member.get("telegram_user_id").is_none(),
            "Telegram ids are not part of the export"
        );
        assert!(member.get("settings").is_some());
    }

    for forbidden in [
        token.as_str(),
        token_hash.as_str(),
        "token_hash",
        "CALLBACK-SECRET-HASH",
        "CALLBACK-PAYLOAD",
        "llm_runs",
        "outbox",
        "foreign-id",
    ] {
        assert!(
            !text.contains(forbidden),
            "export leaked {forbidden}:\n{text}"
        );
    }
    assert!(!text.contains(&foreign.id.to_string()));

    // Файл выгрузки создаётся с правами 0600, в том числе поверх файла с
    // чужими правами.
    let dir = std::env::temp_dir().join(format!("tasklerq-export-{}", Uuid::now_v7().simple()));
    std::fs::create_dir_all(&dir).expect("temporary directory");
    let path = dir.join("export.json");
    std::fs::write(&path, "прежнее содержимое").expect("pre-existing export file");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
            .expect("pre-existing permissions");
    }
    let to_file = run(
        &db.url,
        &[
            "admin",
            "export",
            "--household",
            &household_id,
            "--output",
            path.to_str().expect("path"),
        ],
    )
    .await;
    assert!(to_file.status.success(), "{to_file:?}");
    assert!(stdout(&to_file).is_empty(), "the file export is quiet");
    let written = std::fs::read_to_string(&path).expect("export file");
    assert!(written.contains("Купить молоко"));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "export file permissions");
    }

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn export_uses_the_only_household_when_not_specified() {
    let db = cli_database().await;
    let household = support::create_household(&db.pool).await;

    let output = run(&db.url, &["admin", "export"]).await;
    assert!(output.status.success(), "{output:?}");
    let export: Value = serde_json::from_str(&stdout(&output)).expect("stdout is JSON");
    assert_eq!(
        export["household"]["id"],
        household.household_id.to_string(),
        "the single household is used without --household"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multi_token_create_requires_household_even_when_unique() {
    let db = cli_database().await;
    let household = support::create_household(&db.pool).await;

    let without = run_with_tenancy(
        &db.url,
        "multi",
        &[
            "admin", "token", "create", "--name", "HA", "--scopes", "ha:read",
        ],
    )
    .await;
    assert!(!without.status.success(), "{without:?}");
    let combined = format!("{}{}", stdout(&without), stderr(&without));
    assert!(
        combined.contains("--household"),
        "multi must ask for --household:\n{combined}"
    );
    let tokens: i64 = sqlx::query_scalar("SELECT count(*) FROM api_tokens WHERE household_id = $1")
        .bind(household.household_id)
        .fetch_one(&db.pool)
        .await
        .expect("token count");
    assert_eq!(tokens, 0, "no token is created without --household");

    let household_id = household.household_id.to_string();
    let with = run_with_tenancy(
        &db.url,
        "multi",
        &[
            "admin",
            "token",
            "create",
            "--household",
            &household_id,
            "--name",
            "HA",
            "--scopes",
            "ha:read",
        ],
    )
    .await;
    assert!(with.status.success(), "{with:?}");
    let token = field(&stdout(&with), "token");
    assert!(token.starts_with("tlq_"), "the token is printed once");
    let tokens: i64 = sqlx::query_scalar("SELECT count(*) FROM api_tokens WHERE household_id = $1")
        .bind(household.household_id)
        .fetch_one(&db.pool)
        .await
        .expect("token count");
    assert_eq!(tokens, 1, "the explicit household gets the token");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multi_export_requires_household_even_when_unique() {
    let db = cli_database().await;
    let household = support::create_household(&db.pool).await;
    let dir = std::env::temp_dir().join(format!("tasklerq-export-{}", Uuid::now_v7().simple()));
    std::fs::create_dir_all(&dir).expect("temporary directory");
    let path = dir.join("export.json");
    let path_str = path.to_str().expect("path");

    let without =
        run_with_tenancy(&db.url, "multi", &["admin", "export", "--output", path_str]).await;
    assert!(!without.status.success(), "{without:?}");
    let combined = format!("{}{}", stdout(&without), stderr(&without));
    assert!(
        combined.contains("--household"),
        "multi must ask for --household:\n{combined}"
    );
    assert!(stdout(&without).is_empty(), "no JSON is written to stdout");
    assert!(!path.exists(), "no export file is created");

    let household_id = household.household_id.to_string();
    let with = run_with_tenancy(
        &db.url,
        "multi",
        &[
            "admin",
            "export",
            "--household",
            &household_id,
            "--output",
            path_str,
        ],
    )
    .await;
    assert!(with.status.success(), "{with:?}");
    let written = std::fs::read_to_string(&path).expect("export file");
    let export: Value = serde_json::from_str(&written).expect("file is JSON");
    assert_eq!(export["household"]["id"], household_id);

    std::fs::remove_dir_all(&dir).ok();
}
