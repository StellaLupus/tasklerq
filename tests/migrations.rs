//! Миграции: advisory lock, идемпотентность и откат сбойной миграции.

mod support;

use anyhow::Context;
use axum::http::{Method, StatusCode};
use sqlx::migrate::Migrator;
use tasklerq::storage::migrations::{self, MIGRATOR};
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
