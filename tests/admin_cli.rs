//! Задача 8.7: административный CLI через настоящий бинарник.
//!
//! Каждый тест получает отдельную базу на общем контейнере: счётчики очередей
//! и выгрузка не зависят от строк, созданных параллельными тестами.

mod support;

use std::process::{Output, Stdio};

use chrono::Utc;
use serde_json::Value;
use sqlx::postgres::PgPoolOptions;
use tasklerq::auth::tokens::{SCOPES, hash_token};
use tasklerq::storage::migrations;
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
    assert_eq!(columns.len(), 5, "id, name, timezone, members, created_at");
    assert_eq!(columns[1], "Семья CLI");
    assert_eq!(columns[2], "Asia/Yekaterinburg");
    assert_eq!(columns[3], "1", "one active member");
    assert!(!columns[4].is_empty(), "creation date is printed");

    for forbidden in ["token_hash", "tlq_", "secret", "password"] {
        assert!(
            !text.to_lowercase().contains(forbidden),
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
    let db = cli_database().await;
    let ours = support::create_household(&db.pool).await;
    let other = support::create_household(&db.pool).await;

    let mut item = support::new_item(ours.tasks_list_id, "Купить молоко");
    item.tags = vec!["молоко".to_owned()];
    item.subtasks = vec![support::subtask("Взять сумку")];
    let item = support::create_item(&db.pool, &ours, ours.tasks_list_id, &ours.owner(), item).await;
    assert_eq!(item.tags.len(), 1);

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
        .execute(&db.pool)
        .await
        .expect("soft delete");

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

    // Файл выгрузки создаётся с правами 0600.
    let dir = std::env::temp_dir().join(format!("tasklerq-export-{}", Uuid::now_v7().simple()));
    std::fs::create_dir_all(&dir).expect("temporary directory");
    let path = dir.join("export.json");
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
