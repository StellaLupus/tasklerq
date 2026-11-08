//! Миграции: advisory lock, идемпотентность и откат сбойной миграции.

mod support;

use anyhow::Context;
use axum::http::{Method, StatusCode};
use serde_json::json;
use sqlx::migrate::Migrator;
use sqlx::postgres::PgPoolOptions;
use tasklerq::storage::migrations::{self, MIGRATOR};
use tokio::process::Command;
use uuid::Uuid;

/// `sqlx::migrate!` не должен отключать advisory lock.
#[test]
fn embedded_migrator_keeps_advisory_locking() {
    assert!(
        MIGRATOR.locking,
        "migrations must run under the sqlx advisory lock"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_runs_apply_each_migration_exactly_once() {
    let db = support::empty_db().await;

    let (first, second) = tokio::join!(migrations::run(&db.pool), migrations::run(&db.pool));
    first.expect("the first parallel migration run should succeed");
    second.expect("the second parallel migration run should succeed");

    let rows: Vec<(i64, i64, bool)> = sqlx::query_as(
        "SELECT version, count(*), bool_and(success)
         FROM _sqlx_migrations
         GROUP BY version
         ORDER BY version",
    )
    .fetch_all(&db.pool)
    .await
    .expect("migration rows");

    let expected: Vec<i64> = MIGRATOR.iter().map(|migration| migration.version).collect();
    let applied: Vec<i64> = rows.iter().map(|(version, _, _)| *version).collect();
    assert_eq!(applied, expected, "every embedded migration is recorded");
    assert!(
        rows.iter()
            .all(|(_, count, success)| *count == 1 && *success),
        "each migration must be applied once and marked successful: {rows:?}"
    );

    migrations::run(&db.pool)
        .await
        .expect("a repeated run is a no-op");
    let after: i64 = sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations")
        .fetch_one(&db.pool)
        .await
        .expect("migration count");
    assert_eq!(
        after,
        i64::try_from(expected.len()).expect("version count fits"),
        "a repeated run must not add rows"
    );
    assert!(
        migrations::all_applied(&db.pool)
            .await
            .expect("applied check"),
        "all embedded migrations should be applied"
    );
}

/// Скопировать встроенные миграции до версии `version` включительно.
fn copy_migrations_through(version: i64, target: &std::path::Path) {
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    for entry in std::fs::read_dir(source).expect("migrations directory") {
        let entry = entry.expect("migration entry");
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(prefix) = name.get(..4) else {
            continue;
        };
        let Ok(number) = prefix.parse::<i64>() else {
            continue;
        };
        if number <= version {
            std::fs::copy(entry.path(), target.join(name.as_ref())).expect("copy migration");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn screen_visibility_migration_applies_over_existing_rows() {
    let db = support::empty_db().await;
    let dir = std::env::temp_dir().join(format!(
        "tasklerq-migrations-0010-{}",
        Uuid::now_v7().simple()
    ));
    std::fs::create_dir_all(&dir).expect("temporary migration directory");
    copy_migrations_through(10, &dir);

    let before = Migrator::new(dir.as_path())
        .await
        .expect("a migrator through 0010 should resolve");
    before
        .run(&db.pool)
        .await
        .context("migrations through 0010 should apply")
        .expect("migrations through 0010");

    // Реалистичные строки до появления `delete` и `buried`: все виды и
    // статусы outbox, а также существующие экраны чатов.
    for (chat, kind, payload, status) in [
        (101, "screen", json!({"html": "<p>экран</p>"}), "pending"),
        (
            101,
            "message",
            json!({"html": "<p>текст</p>", "rich": true}),
            "sent",
        ),
        (
            101,
            "edit",
            json!({"html": "<p>правка</p>", "message_id": 7}),
            "skipped",
        ),
        (102, "screen", json!({"redacted": true}), "dead"),
    ] {
        sqlx::query(
            "INSERT INTO outbox (chat_id, kind, payload, status)
             VALUES ($1, $2::outbox_kind, $3, $4::outbox_status)",
        )
        .bind(chat)
        .bind(kind)
        .bind(payload)
        .bind(status)
        .execute(&db.pool)
        .await
        .expect("existing outbox row");
    }
    sqlx::query(
        "INSERT INTO telegram_views (chat_id, message_id, view_key, state)
         VALUES (101, 7, 'menu', $1), (102, NULL, 'empty', '{}'::jsonb)",
    )
    .bind(json!({"tab": "tasks"}))
    .execute(&db.pool)
    .await
    .expect("existing view");

    MIGRATOR
        .run(&db.pool)
        .await
        .expect("0011 must apply over existing data");

    let buried: Vec<(i64, bool)> =
        sqlx::query_as("SELECT chat_id, buried FROM telegram_views ORDER BY chat_id")
            .fetch_all(&db.pool)
            .await
            .expect("views after migration");
    assert_eq!(
        buried,
        vec![(101, false), (102, false)],
        "existing views must be visible after the migration"
    );

    let seq: i64 = sqlx::query_scalar(
        "INSERT INTO outbox (chat_id, kind, payload)
         VALUES (103, 'delete', $1)
         RETURNING seq",
    )
    .bind(json!({"message_id": 5}))
    .fetch_one(&db.pool)
    .await
    .expect("the new outbox kind must accept a delete row");
    assert!(seq > 0);

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn capture_draft_parts_migration_adds_the_parts_column_and_keeps_old_rows() {
    let db = support::empty_db().await;
    let dir = std::env::temp_dir().join(format!(
        "tasklerq-migrations-0012-{}",
        Uuid::now_v7().simple()
    ));
    std::fs::create_dir_all(&dir).expect("temporary migration directory");
    copy_migrations_through(12, &dir);

    let before = Migrator::new(dir.as_path())
        .await
        .expect("a migrator through 0012 should resolve");
    before
        .run(&db.pool)
        .await
        .context("migrations through 0012 should apply")
        .expect("migrations through 0012");

    let household = support::create_household(&db.pool).await;
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO capture_drafts (
             id, household_id, source, mode, raw_text, status, created_at, expires_at
         )
         VALUES ($1, $2, 'telegram', 'draft', 'старый текст', 'pending', now(), now())",
    )
    .bind(id)
    .bind(household.household_id)
    .execute(&db.pool)
    .await
    .expect("draft before the migration");

    MIGRATOR
        .run(&db.pool)
        .await
        .expect("0013 must apply over existing rows");

    let column: Option<(String, String, String, Option<String>)> = sqlx::query_as(
        "SELECT data_type, udt_name, is_nullable, column_default
         FROM information_schema.columns
         WHERE table_schema = 'public'
           AND table_name = 'capture_drafts'
           AND column_name = 'raw_parts'",
    )
    .fetch_optional(&db.pool)
    .await
    .expect("column lookup");
    let (data_type, udt_name, is_nullable, default) =
        column.expect("raw_parts must exist on a clean database");
    assert_eq!(data_type, "ARRAY");
    assert_eq!(udt_name, "_text");
    assert_eq!(is_nullable, "NO");
    assert!(
        default.as_deref().unwrap_or_default().contains("'{}'"),
        "the default must be an empty array: {default:?}"
    );

    let raw_parts: Vec<String> =
        sqlx::query_scalar("SELECT raw_parts FROM capture_drafts WHERE id = $1")
            .bind(id)
            .fetch_one(&db.pool)
            .await
            .expect("old row after the migration");
    assert!(
        raw_parts.is_empty(),
        "existing rows get an empty array, not a backfill: {raw_parts:?}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_migration_reports_its_version_rolls_back_and_blocks_readiness() {
    let db = support::empty_db().await;
    let dir = std::env::temp_dir().join(format!("tasklerq-migrations-{}", Uuid::now_v7().simple()));
    std::fs::create_dir_all(&dir).expect("temporary migration directory");
    std::fs::write(
        dir.join("0001_create_probe.sql"),
        "CREATE TABLE migration_probe (id integer PRIMARY KEY);\n",
    )
    .expect("first migration");
    std::fs::write(
        dir.join("0002_broken.sql"),
        "CREATE TABLE broken_probe (id integer PRIMARY KEY);\n\
         SELECT * FROM missing_table_after_partial_change;\n",
    )
    .expect("second migration");

    let migrator = Migrator::new(dir.as_path())
        .await
        .expect("a runtime migrator should resolve");
    let error = migrator
        .run(&db.pool)
        .await
        .context("database migration failed")
        .expect_err("the broken migration must fail");
    let text = format!("{error:#}");

    assert!(
        text.contains("migration 2"),
        "the error must name the failed migration version: {text}"
    );
    assert!(
        text.contains("missing_table_after_partial_change"),
        "the error must include the database reason: {text}"
    );

    let first_exists: bool =
        sqlx::query_scalar("SELECT to_regclass('public.migration_probe') IS NOT NULL")
            .fetch_one(&db.pool)
            .await
            .expect("first table check");
    assert!(first_exists, "the successful migration stays applied");

    let broken_exists: bool =
        sqlx::query_scalar("SELECT to_regclass('public.broken_probe') IS NOT NULL")
            .fetch_one(&db.pool)
            .await
            .expect("broken table check");
    assert!(
        !broken_exists,
        "the partial change of the failed migration must be rolled back"
    );

    let rows: Vec<(i64, bool)> =
        sqlx::query_as("SELECT version, success FROM _sqlx_migrations ORDER BY version")
            .fetch_all(&db.pool)
            .await
            .expect("migration rows");
    assert_eq!(
        rows,
        vec![(1, true)],
        "the failed migration is not recorded"
    );
    assert!(
        !migrations::all_applied(&db.pool)
            .await
            .expect("applied check"),
        "a database with a failed migration is not ready"
    );

    let app = support::app(&db.pool);
    let (status, _) = support::request(&app, Method::GET, "/health/ready", None, None).await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "readiness must fail while migrations are incomplete"
    );

    std::fs::remove_dir_all(&dir).ok();
}

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_tasklerq"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_migrate_applies_to_a_fresh_database_and_a_repeat_is_a_noop() {
    let name = format!("migrate_cli_{}", Uuid::now_v7().simple());
    support::create_database(&name).await;
    let url = support::database_url_for(&name).await;

    for attempt in 1..=2 {
        let output = bin()
            .env("DATABASE_URL", &url)
            .env("RUST_LOG", "error")
            .arg("migrate")
            .output()
            .await
            .expect("the migrate CLI should run");
        assert!(
            output.status.success(),
            "migrate run {attempt} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .expect("the migrated database should accept connections");
    let rows: Vec<(i64, i64, bool)> = sqlx::query_as(
        "SELECT version, count(*), bool_and(success)
         FROM _sqlx_migrations
         GROUP BY version
         ORDER BY version",
    )
    .fetch_all(&pool)
    .await
    .expect("migration rows");

    let expected: Vec<i64> = MIGRATOR.iter().map(|migration| migration.version).collect();
    let applied: Vec<i64> = rows.iter().map(|(version, _, _)| *version).collect();
    assert_eq!(applied, expected, "every embedded migration is recorded");
    assert!(
        rows.iter()
            .all(|(_, count, success)| *count == 1 && *success),
        "a repeated CLI run must not apply a migration twice: {rows:?}"
    );
    assert!(
        migrations::all_applied(&pool).await.expect("applied check"),
        "the fresh database is fully migrated"
    );
    pool.close().await;
}
