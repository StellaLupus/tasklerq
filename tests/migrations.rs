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

/// Поиск по описанию и подзадачам опирается на trigram-индексы из 0015.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_trigram_indexes_are_created_for_notes_and_subtasks() {
    let db = support::empty_db().await;
    migrations::run(&db.pool)
        .await
        .expect("migrations should apply on a clean database");

    let indexes: Vec<(String, String)> = sqlx::query_as(
        "SELECT indexname, indexdef
         FROM pg_indexes
         WHERE schemaname = 'public'
           AND indexname IN ('items_notes_trgm_idx', 'subtasks_title_trgm_idx')
         ORDER BY indexname",
    )
    .fetch_all(&db.pool)
    .await
    .expect("index lookup");

    assert_eq!(
        indexes.len(),
        2,
        "0015 must create both trigram indexes: {indexes:?}"
    );
    for (name, definition) in &indexes {
        assert!(
            definition.contains("USING gin"),
            "{name} must be a GIN index: {definition}"
        );
        assert!(
            definition.contains("gin_trgm_ops"),
            "{name} must use gin_trgm_ops: {definition}"
        );
    }
    let items_notes = indexes
        .iter()
        .find(|(name, _)| name == "items_notes_trgm_idx")
        .expect("items_notes_trgm_idx must exist");
    assert!(
        items_notes.1.contains("notes gin_trgm_ops"),
        "items_notes_trgm_idx must index items.notes: {}",
        items_notes.1
    );
    let subtasks_title = indexes
        .iter()
        .find(|(name, _)| name == "subtasks_title_trgm_idx")
        .expect("subtasks_title_trgm_idx must exist");
    assert!(
        subtasks_title.1.contains("title gin_trgm_ops"),
        "subtasks_title_trgm_idx must index subtasks.title: {}",
        subtasks_title.1
    );
}

/// Household и список задач на схеме до появления kind `events` (0016).
///
/// `support::create_household` создаёт список «Календарь», которого в старой
/// схеме ещё нет, поэтому миграционные тесты с данными до 0016 заводят
/// минимальный household сами.
async fn create_household_before_events(pool: &sqlx::PgPool) -> (Uuid, Uuid, Uuid) {
    let household_id = Uuid::now_v7();
    let owner_id = Uuid::now_v7();
    let tasks_list_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO households (id, name, timezone)
         VALUES ($1, 'Старый household', 'Europe/Moscow')",
    )
    .bind(household_id)
    .execute(pool)
    .await
    .expect("household before 0016");
    sqlx::query(
        "INSERT INTO users (
             id, household_id, telegram_user_id, telegram_chat_id, display_name,
             role, settings
         )
         VALUES ($1, $2, 6500000001, 6500000001, 'Владелец', 'owner', '{}'::jsonb)",
    )
    .bind(owner_id)
    .bind(household_id)
    .execute(pool)
    .await
    .expect("owner before 0016");
    sqlx::query(
        "INSERT INTO lists (
             id, household_id, kind, name, is_default, is_archived, position
         )
         VALUES ($1, $2, 'tasks', 'Задачи', true, false, 0)",
    )
    .bind(tasks_list_id)
    .bind(household_id)
    .execute(pool)
    .await
    .expect("tasks list before 0016");
    (household_id, owner_id, tasks_list_id)
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

    let (household_id, _, _) = create_household_before_events(&db.pool).await;
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO capture_drafts (
             id, household_id, source, mode, raw_text, status, created_at, expires_at
         )
         VALUES ($1, $2, 'telegram', 'draft', 'старый текст', 'pending', now(), now())",
    )
    .bind(id)
    .bind(household_id)
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
async fn snooze_origin_migration_keeps_existing_snooze_unbound() {
    let db = support::empty_db().await;
    let dir = std::env::temp_dir().join(format!(
        "tasklerq-migrations-0014-{}",
        Uuid::now_v7().simple()
    ));
    std::fs::create_dir_all(&dir).expect("temporary migration directory");
    copy_migrations_through(13, &dir);

    let before = Migrator::new(dir.as_path())
        .await
        .expect("a migrator through 0013 should resolve");
    before
        .run(&db.pool)
        .await
        .context("migrations through 0013 should apply")
        .expect("migrations through 0013");

    let (household_id, owner_id, tasks_list_id) = create_household_before_events(&db.pool).await;
    let item_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO items (
             id, household_id, list_id, title, status, priority, source, position, version
         )
         VALUES ($1, $2, $3, 'Отложенная задача', 'open', 'normal', 'api', 0, 1)",
    )
    .bind(item_id)
    .bind(household_id)
    .bind(tasks_list_id)
    .execute(&db.pool)
    .await
    .expect("item before 0014");
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO notification_deliveries (
             id, household_id, user_id, item_id, occurrence_key, kind,
             status, scheduled_at, created_at, updated_at
         )
         VALUES ($1, $2, $3, $4, 'snooze:1800000000', 'snooze',
                 'pending', now(), now(), now())",
    )
    .bind(id)
    .bind(household_id)
    .bind(owner_id)
    .bind(item_id)
    .execute(&db.pool)
    .await
    .expect("pending snooze before the migration");

    MIGRATOR
        .run(&db.pool)
        .await
        .expect("0014 must apply over an existing pending snooze");

    let origin: Option<String> =
        sqlx::query_scalar("SELECT origin_key FROM notification_deliveries WHERE id = $1")
            .bind(id)
            .fetch_one(&db.pool)
            .await
            .expect("snooze after the migration");
    assert_eq!(
        origin, None,
        "a snooze created before the migration must stay unbound"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn events_migration_adds_one_default_calendar_list_and_keeps_existing_lists() {
    let db = support::empty_db().await;
    let dir = std::env::temp_dir().join(format!(
        "tasklerq-migrations-0017-{}",
        Uuid::now_v7().simple()
    ));
    std::fs::create_dir_all(&dir).expect("temporary migration directory");
    copy_migrations_through(16, &dir);

    let before = Migrator::new(dir.as_path())
        .await
        .expect("a migrator through 0016 should resolve");
    before
        .run(&db.pool)
        .await
        .context("migrations through 0016 should apply")
        .expect("migrations through 0016");

    // Household и его списки созданы до 0017: два списка по умолчанию и
    // дополнительный чек-лист, чтобы позиция «Календаря» была после них.
    let household_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO households (id, name, timezone)
         VALUES ($1, 'Старый household', 'Europe/Moscow')",
    )
    .bind(household_id)
    .execute(&db.pool)
    .await
    .expect("household before the migration");
    for (kind, name, is_default, position) in [
        ("tasks", "Задачи", true, 0),
        ("shopping", "Покупки", true, 1),
        ("checklist", "Дача", false, 2),
    ] {
        sqlx::query(
            "INSERT INTO lists (
                 id, household_id, kind, name, is_default, is_archived, position
             )
             VALUES ($1, $2, $3::list_kind, $4, $5, false, $6)",
        )
        .bind(Uuid::now_v7())
        .bind(household_id)
        .bind(kind)
        .bind(name)
        .bind(is_default)
        .bind(position)
        .execute(&db.pool)
        .await
        .expect("list before the migration");
    }
    let before_lists: Vec<(Uuid, String, bool, i32)> = sqlx::query_as(
        "SELECT id, kind::text, is_default, position
         FROM lists
         WHERE household_id = $1 AND deleted_at IS NULL
         ORDER BY position, id",
    )
    .bind(household_id)
    .fetch_all(&db.pool)
    .await
    .expect("lists before the migration");
    assert_eq!(before_lists.len(), 3);

    MIGRATOR
        .run(&db.pool)
        .await
        .expect("0017 must apply over an existing household");

    let after_lists: Vec<(Uuid, String, bool, i32)> = sqlx::query_as(
        "SELECT id, kind::text, is_default, position
         FROM lists
         WHERE household_id = $1 AND deleted_at IS NULL
         ORDER BY position, id",
    )
    .bind(household_id)
    .fetch_all(&db.pool)
    .await
    .expect("lists after the migration");

    let events: Vec<&(Uuid, String, bool, i32)> = after_lists
        .iter()
        .filter(|(_, kind, _, _)| kind == "events")
        .collect();
    assert_eq!(
        events.len(),
        1,
        "0017 must create exactly one events list: {after_lists:?}"
    );
    let (event_id, _, is_default, position) = events[0];
    assert!(*is_default, "the events list must be the default one");
    assert_eq!(
        *position,
        before_lists
            .iter()
            .map(|(_, _, _, position)| *position)
            .max()
            .expect("existing lists")
            + 1,
        "the events list must go after the existing lists"
    );
    let name: String = sqlx::query_scalar("SELECT name FROM lists WHERE id = $1")
        .bind(event_id)
        .fetch_one(&db.pool)
        .await
        .expect("events list name");
    assert_eq!(name, "Календарь");

    let after_without_events: Vec<(Uuid, String, bool, i32)> = after_lists
        .into_iter()
        .filter(|(_, kind, _, _)| kind != "events")
        .collect();
    assert_eq!(
        after_without_events, before_lists,
        "the migration must keep the existing lists and their positions"
    );

    // sqlx не запускает применённую миграцию второй раз, поэтому проверяем
    // сам запрос вставки: его `NOT EXISTS` не должен создать второй список.
    let migration = include_str!("../migrations/0017_events.sql");
    let insert = migration
        .split_once("INSERT INTO lists")
        .expect("0017 must contain the list insertion")
        .1
        .trim()
        .trim_end_matches(';');
    sqlx::query(&format!("INSERT INTO lists {insert}"))
        .execute(&db.pool)
        .await
        .expect("the list insertion must be repeatable");
    let events_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM lists
         WHERE household_id = $1 AND kind = 'events' AND deleted_at IS NULL",
    )
    .bind(household_id)
    .fetch_one(&db.pool)
    .await
    .expect("events list count");
    assert_eq!(
        events_count, 1,
        "a repeated list insertion must not create a second events list"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn events_migration_uses_a_free_name_when_calendar_is_taken() {
    let db = support::empty_db().await;
    let dir = std::env::temp_dir().join(format!(
        "tasklerq-migrations-0017-name-{}",
        Uuid::now_v7().simple()
    ));
    std::fs::create_dir_all(&dir).expect("temporary migration directory");
    copy_migrations_through(16, &dir);

    let before = Migrator::new(dir.as_path())
        .await
        .expect("a migrator through 0016 should resolve");
    before
        .run(&db.pool)
        .await
        .context("migrations through 0016 should apply")
        .expect("migrations through 0016");

    let household_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO households (id, name, timezone)
         VALUES ($1, 'Household с занятым именем', 'Europe/Moscow')",
    )
    .bind(household_id)
    .execute(&db.pool)
    .await
    .expect("household before the migration");
    for (kind, name, is_default, position) in [
        ("tasks", "Задачи", true, 0),
        ("shopping", "Покупки", true, 1),
        ("checklist", "календарь", false, 2),
    ] {
        sqlx::query(
            "INSERT INTO lists (
                 id, household_id, kind, name, is_default, is_archived, position
             )
             VALUES ($1, $2, $3::list_kind, $4, $5, false, $6)",
        )
        .bind(Uuid::now_v7())
        .bind(household_id)
        .bind(kind)
        .bind(name)
        .bind(is_default)
        .bind(position)
        .execute(&db.pool)
        .await
        .expect("list before the migration");
    }

    MIGRATOR
        .run(&db.pool)
        .await
        .expect("0017 must apply when the household already has «календарь»");

    let events: Vec<(String, bool, i32)> = sqlx::query_as(
        "SELECT name, is_default, position
         FROM lists
         WHERE household_id = $1 AND kind = 'events' AND deleted_at IS NULL
         ORDER BY position, id",
    )
    .bind(household_id)
    .fetch_all(&db.pool)
    .await
    .expect("events lists after the migration");
    assert_eq!(
        events,
        vec![("Календарь (события)".to_owned(), true, 3)],
        "the migration must pick a free name and keep the old lists"
    );

    let calendar: (String, i32) = sqlx::query_as(
        "SELECT name, position FROM lists
         WHERE household_id = $1 AND kind = 'checklist' AND deleted_at IS NULL",
    )
    .bind(household_id)
    .fetch_one(&db.pool)
    .await
    .expect("the existing checklist");
    assert_eq!(
        calendar,
        ("календарь".to_owned(), 2),
        "the existing list must keep its name and position"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn llm_slots_migration_applies_on_a_clean_database() {
    let db = support::empty_db().await;
    migrations::run(&db.pool)
        .await
        .expect("migrations should apply on a clean database");

    let columns: Vec<(String, String)> = sqlx::query_as(
        "SELECT column_name, is_nullable
         FROM information_schema.columns
         WHERE table_schema = 'public'
           AND table_name = 'llm_runs'
           AND column_name IN ('slot', 'attempt')
         ORDER BY column_name",
    )
    .fetch_all(&db.pool)
    .await
    .expect("llm_runs columns");

    assert_eq!(
        columns,
        vec![
            ("attempt".to_owned(), "YES".to_owned()),
            ("slot".to_owned(), "YES".to_owned()),
        ],
        "both columns must be nullable for rows written before 0018"
    );

    let check_columns: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT column_name, data_type, is_nullable
         FROM information_schema.columns
         WHERE table_schema = 'public' AND table_name = 'llm_slot_checks'
         ORDER BY ordinal_position",
    )
    .fetch_all(&db.pool)
    .await
    .expect("llm_slot_checks columns");
    let names: Vec<&str> = check_columns
        .iter()
        .map(|(name, _, _)| name.as_str())
        .collect();
    assert_eq!(
        names,
        vec![
            "slot",
            "fingerprint",
            "fingerprint_parts",
            "passed",
            "total",
            "p95_ms",
            "report",
            "checked_at",
        ],
        "llm_slot_checks must keep the fingerprint, the counters, p95 and the report"
    );

    let primary_key: Vec<String> = sqlx::query_scalar(
        "SELECT a.attname
         FROM pg_index i
         JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = ANY (i.indkey)
         WHERE i.indrelid = 'public.llm_slot_checks'::regclass AND i.indisprimary",
    )
    .fetch_all(&db.pool)
    .await
    .expect("llm_slot_checks primary key");
    assert_eq!(
        primary_key,
        vec!["slot".to_owned()],
        "one row per slot: the slot is the primary key"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn llm_slot_settings_migration_applies_on_a_clean_database() {
    let db = support::empty_db().await;
    migrations::run(&db.pool)
        .await
        .expect("migrations should apply on a clean database");

    let columns: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT column_name, data_type, is_nullable
         FROM information_schema.columns
         WHERE table_schema = 'public' AND table_name = 'llm_slot_settings'
         ORDER BY ordinal_position",
    )
    .fetch_all(&db.pool)
    .await
    .expect("llm_slot_settings columns");

    let shape: Vec<(&str, &str, &str)> = columns
        .iter()
        .map(|(name, data_type, nullable)| (name.as_str(), data_type.as_str(), nullable.as_str()))
        .collect();
    assert_eq!(
        shape,
        vec![
            ("slot", "text", "NO"),
            ("titles", "text", "NO"),
            ("updated_at", "timestamp with time zone", "NO"),
        ],
        "one standard per slot, no nullable fields"
    );

    let primary_key: Vec<String> = sqlx::query_scalar(
        "SELECT a.attname
         FROM pg_index i
         JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = ANY (i.indkey)
         WHERE i.indrelid = 'public.llm_slot_settings'::regclass AND i.indisprimary",
    )
    .fetch_all(&db.pool)
    .await
    .expect("llm_slot_settings primary key");
    assert_eq!(
        primary_key,
        vec!["slot".to_owned()],
        "one row per slot: the slot is the primary key"
    );

    let titles_default: Option<String> = sqlx::query_scalar(
        "SELECT column_default
         FROM information_schema.columns
         WHERE table_schema = 'public' AND table_name = 'llm_slot_settings'
           AND column_name = 'titles'",
    )
    .fetch_one(&db.pool)
    .await
    .expect("titles default");
    assert!(
        titles_default
            .as_deref()
            .is_some_and(|default| default.contains("strict")),
        "a missing standard must default to strict: {titles_default:?}"
    );

    sqlx::query("INSERT INTO llm_slot_settings (slot, titles) VALUES ('primary', 'normalized')")
        .execute(&db.pool)
        .await
        .expect("a known standard must be accepted");

    let rejected =
        sqlx::query("INSERT INTO llm_slot_settings (slot, titles) VALUES ('fallback', 'loose')")
            .execute(&db.pool)
            .await;
    assert!(
        rejected.is_err(),
        "only strict and normalized are valid standards"
    );

    let blank =
        sqlx::query("INSERT INTO llm_slot_settings (slot, titles) VALUES ('   ', 'strict')")
            .execute(&db.pool)
            .await;
    assert!(blank.is_err(), "a blank slot name must be rejected");

    let duplicate =
        sqlx::query("INSERT INTO llm_slot_settings (slot, titles) VALUES ('primary', 'strict')")
            .execute(&db.pool)
            .await;
    assert!(
        duplicate.is_err(),
        "one row per slot: a duplicate is an error"
    );
}

fn error_code<T>(result: &Result<T, sqlx::Error>) -> Option<String> {
    match result {
        Err(sqlx::Error::Database(database)) => database.code().map(|code| code.into_owned()),
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
async fn insert_setting(
    pool: &sqlx::PgPool,
    scope: &str,
    household_id: Option<Uuid>,
    key: &str,
    value: Option<serde_json::Value>,
    secret: Option<Vec<u8>>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO settings (scope, household_id, key, value, secret)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(scope)
    .bind(household_id)
    .bind(key)
    .bind(value)
    .bind(secret)
    .execute(pool)
    .await
    .map(|_| ())
}

/// Таблица `settings`: форма, уровни и ограничения ключа и значения.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn settings_migration_enforces_scope_key_and_secret_rules() {
    let db = support::empty_db().await;
    migrations::run(&db.pool)
        .await
        .expect("migrations should apply on a clean database");

    let columns: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT column_name, data_type, is_nullable
         FROM information_schema.columns
         WHERE table_schema = 'public' AND table_name = 'settings'
         ORDER BY ordinal_position",
    )
    .fetch_all(&db.pool)
    .await
    .expect("settings columns");
    let shape: Vec<(&str, &str, &str)> = columns
        .iter()
        .map(|(name, data_type, nullable)| (name.as_str(), data_type.as_str(), nullable.as_str()))
        .collect();
    assert_eq!(
        shape,
        vec![
            ("scope", "text", "NO"),
            ("household_id", "uuid", "YES"),
            ("key", "text", "NO"),
            ("value", "jsonb", "YES"),
            ("secret", "bytea", "YES"),
            ("updated_at", "timestamp with time zone", "NO"),
        ],
        "settings must keep the value and the encrypted secret apart"
    );

    let updated_at_default: Option<String> = sqlx::query_scalar(
        "SELECT column_default
         FROM information_schema.columns
         WHERE table_schema = 'public' AND table_name = 'settings'
           AND column_name = 'updated_at'",
    )
    .fetch_one(&db.pool)
    .await
    .expect("updated_at default");
    assert!(
        updated_at_default
            .as_deref()
            .is_some_and(|default| default.contains("now()")),
        "updated_at must default to now(): {updated_at_default:?}"
    );

    let household_a = Uuid::now_v7();
    let household_b = Uuid::now_v7();
    for (id, name) in [(household_a, "Первый"), (household_b, "Второй")] {
        sqlx::query(
            "INSERT INTO households (id, name, timezone)
             VALUES ($1, $2, 'Europe/Moscow')",
        )
        .bind(id)
        .bind(name)
        .execute(&db.pool)
        .await
        .expect("household");
    }

    insert_setting(
        &db.pool,
        "installation",
        None,
        "llm.enabled",
        Some(json!({"enabled": true})),
        None,
    )
    .await
    .expect("an installation setting is valid");

    let duplicate_installation = insert_setting(
        &db.pool,
        "installation",
        None,
        "llm.enabled",
        Some(json!({"enabled": false})),
        None,
    )
    .await;
    assert_eq!(
        error_code(&duplicate_installation).as_deref(),
        Some("23505"),
        "installation keys must be unique even though household_id is NULL"
    );

    insert_setting(
        &db.pool,
        "household",
        Some(household_a),
        "llm.primary.url",
        Some(json!("https://llm.example/v1")),
        None,
    )
    .await
    .expect("a household setting is valid");

    let duplicate_household = insert_setting(
        &db.pool,
        "household",
        Some(household_a),
        "llm.primary.url",
        Some(json!("https://other.example/v1")),
        None,
    )
    .await;
    assert_eq!(
        error_code(&duplicate_household).as_deref(),
        Some("23505"),
        "a key must be unique within one household"
    );

    insert_setting(
        &db.pool,
        "household",
        Some(household_b),
        "llm.primary.url",
        Some(json!("https://third.example/v1")),
        None,
    )
    .await
    .expect("another household may set the same key");

    insert_setting(
        &db.pool,
        "installation",
        None,
        "llm.primary.url",
        Some(json!("https://installation.example/v1")),
        None,
    )
    .await
    .expect("the installation level may set the same key as a household");

    let both = insert_setting(
        &db.pool,
        "installation",
        None,
        "llm.primary.token",
        Some(json!("plain")),
        Some(vec![0, 1, 2]),
    )
    .await;
    assert_eq!(
        error_code(&both).as_deref(),
        Some("23514"),
        "value and secret are mutually exclusive"
    );

    let neither = insert_setting(
        &db.pool,
        "installation",
        None,
        "llm.primary.token",
        None,
        None,
    )
    .await;
    assert_eq!(
        error_code(&neither).as_deref(),
        Some("23514"),
        "a setting must carry either a value or a secret"
    );

    let installation_with_household = insert_setting(
        &db.pool,
        "installation",
        Some(household_a),
        "llm.daily_quota",
        Some(json!(200)),
        None,
    )
    .await;
    assert_eq!(
        error_code(&installation_with_household).as_deref(),
        Some("23514"),
        "an installation setting must not reference a household"
    );

    let household_without_household = insert_setting(
        &db.pool,
        "household",
        None,
        "llm.daily_quota",
        Some(json!(200)),
        None,
    )
    .await;
    assert_eq!(
        error_code(&household_without_household).as_deref(),
        Some("23514"),
        "a household setting must reference a household"
    );

    let unknown_scope = insert_setting(
        &db.pool,
        "user",
        Some(household_a),
        "llm.daily_quota",
        Some(json!(200)),
        None,
    )
    .await;
    assert_eq!(
        error_code(&unknown_scope).as_deref(),
        Some("23514"),
        "only the installation and household levels exist"
    );

    let blank_key = insert_setting(
        &db.pool,
        "installation",
        None,
        "   ",
        Some(json!(200)),
        None,
    )
    .await;
    assert_eq!(
        error_code(&blank_key).as_deref(),
        Some("23514"),
        "a blank key must be rejected"
    );

    // Настройки household живут вместе с ним, как остальные его данные.
    sqlx::query("DELETE FROM households WHERE id = $1")
        .bind(household_a)
        .execute(&db.pool)
        .await
        .expect("household deletion");
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM settings WHERE household_id = $1")
        .bind(household_a)
        .fetch_one(&db.pool)
        .await
        .expect("settings after household deletion");
    assert_eq!(left, 0, "household settings must be deleted with it");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn llm_slots_migration_keeps_existing_runs_without_a_slot() {
    let db = support::empty_db().await;
    let dir = std::env::temp_dir().join(format!(
        "tasklerq-migrations-0018-{}",
        Uuid::now_v7().simple()
    ));
    std::fs::create_dir_all(&dir).expect("temporary migration directory");
    copy_migrations_through(17, &dir);

    let before = Migrator::new(dir.as_path())
        .await
        .expect("a migrator through 0017 should resolve");
    before
        .run(&db.pool)
        .await
        .context("migrations through 0017 should apply")
        .expect("migrations through 0017");

    let (household_id, owner_id, _) = create_household_before_events(&db.pool).await;
    for (source, mode, status) in [
        ("telegram", "auto", "success"),
        ("ha_voice", "draft", "failure"),
    ] {
        sqlx::query(
            "INSERT INTO llm_runs (
                 id, household_id, user_id, source, mode, prompt_version,
                 status, error_code, latency_ms, input_tokens, output_tokens,
                 created_at
             )
             VALUES ($1, $2, $3, $4::capture_source, $5::capture_mode, 'capture-v3',
                     $6, NULL, 1200, 100, 20, now())",
        )
        .bind(Uuid::now_v7())
        .bind(household_id)
        .bind(owner_id)
        .bind(source)
        .bind(mode)
        .bind(status)
        .execute(&db.pool)
        .await
        .expect("llm run before 0018");
    }

    MIGRATOR
        .run(&db.pool)
        .await
        .expect("0018 must apply over existing llm_runs rows");

    let rows_with_slot: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM llm_runs WHERE slot IS NOT NULL OR attempt IS NOT NULL",
    )
    .fetch_one(&db.pool)
    .await
    .expect("old runs after the migration");
    assert_eq!(
        rows_with_slot, 0,
        "rows written before 0018 must keep NULL slot and attempt"
    );

    let table_exists: bool =
        sqlx::query_scalar("SELECT to_regclass('public.llm_slot_checks') IS NOT NULL")
            .fetch_one(&db.pool)
            .await
            .expect("llm_slot_checks existence");
    assert!(table_exists, "0018 must create llm_slot_checks");

    sqlx::query(
        "INSERT INTO llm_slot_checks (
             slot, fingerprint, fingerprint_parts, passed, total, p95_ms,
             report, checked_at
         )
         VALUES ('primary', 'abc', '{\"model\":\"m\"}'::jsonb, 20, 22, 3100,
                 '{\"phrases\":[]}'::jsonb, now())",
    )
    .execute(&db.pool)
    .await
    .expect("llm_slot_checks must accept a check result");

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

/// Образ без новой миграции стартует на базе, где она уже применена.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_migrate_warns_about_migrations_missing_from_the_image() {
    let name = format!("migrate_future_{}", Uuid::now_v7().simple());
    support::create_database(&name).await;
    let url = support::database_url_for(&name).await;

    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .expect("the new database should accept connections");
    migrations::run(&pool)
        .await
        .expect("the first migration run should succeed");

    // На базе без версий, которых нет в образе, предупреждения быть не должно.
    let clean = bin()
        .env("DATABASE_URL", &url)
        .env("RUST_LOG", "warn")
        .arg("migrate")
        .output()
        .await
        .expect("the migrate CLI should run");
    assert!(
        clean.status.success(),
        "the CLI must run on a clean schema: {}",
        String::from_utf8_lossy(&clean.stderr)
    );
    let clean_stdout = String::from_utf8_lossy(&clean.stdout);
    assert!(
        !clean_stdout.contains("unknown_versions"),
        "a schema with only embedded migrations must not warn: {clean_stdout}"
    );

    sqlx::query(
        "INSERT INTO _sqlx_migrations
             (version, description, success, checksum, execution_time)
         VALUES (99999999999999, 'future', true, '\\x00', 0)",
    )
    .execute(&pool)
    .await
    .expect("a migration unknown to the image should be recorded");

    let output = bin()
        .env("DATABASE_URL", &url)
        .env("RUST_LOG", "warn")
        .arg("migrate")
        .output()
        .await
        .expect("the migrate CLI should run");

    assert!(
        output.status.success(),
        "the CLI must not fail on a newer schema: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout
            .lines()
            .any(|line| { line.contains("unknown_versions") && line.contains("99999999999999") }),
        "the run must warn about migrations unknown to the image on one line: {stdout}"
    );
    let recorded: bool = sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1 FROM _sqlx_migrations WHERE version = 99999999999999
         )",
    )
    .fetch_one(&pool)
    .await
    .expect("the unknown migration should still be recorded");
    assert!(
        recorded,
        "the data written by a newer image must stay in the database"
    );
    assert!(
        migrations::all_applied(&pool).await.expect("applied check"),
        "readiness must ignore migrations that are missing from the image"
    );
    pool.close().await;
}

/// Колонки таблицы: имя, тип и допустимость NULL.
async fn table_shape(pool: &sqlx::PgPool, table: &str) -> Vec<(String, String, String)> {
    sqlx::query_as(
        "SELECT column_name, data_type, is_nullable
         FROM information_schema.columns
         WHERE table_schema = 'public' AND table_name = $1
         ORDER BY ordinal_position",
    )
    .bind(table)
    .fetch_all(pool)
    .await
    .expect("table columns")
}

async fn insert_admin_account(pool: &sqlx::PgPool, username: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO admin_accounts (id, username, password_hash)
         VALUES ($1, $2, '$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$aGFzaA')",
    )
    .bind(Uuid::now_v7())
    .bind(username)
    .execute(pool)
    .await
    .map(|_| ())
}

async fn insert_one_time_code(
    pool: &sqlx::PgPool,
    purpose: &str,
    household_id: Option<Uuid>,
    code_hash: &str,
    expires_in: Option<&str>,
) -> Result<(), sqlx::Error> {
    let expires_at = expires_in
        .map(|interval| format!("now() + interval '{interval}'"))
        .unwrap_or_else(|| "NULL".to_owned());
    sqlx::query(&format!(
        "INSERT INTO admin_one_time_codes (id, purpose, household_id, code_hash, expires_at)
         VALUES ($1, $2, $3, $4, {expires_at})"
    ))
    .bind(Uuid::now_v7())
    .bind(purpose)
    .bind(household_id)
    .bind(code_hash)
    .execute(pool)
    .await
    .map(|_| ())
}

/// 64 hex-символа, как SHA-256 в нижнем регистре.
fn code_hash(fill: char) -> String {
    fill.to_string().repeat(64)
}

/// Таблицы консоли: форма колонок и новые значения `audit_event_type`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_console_migration_creates_tables_and_audit_values() {
    let db = support::empty_db().await;
    migrations::run(&db.pool)
        .await
        .expect("migrations should apply on a clean database");

    let accounts = table_shape(&db.pool, "admin_accounts").await;
    let accounts: Vec<(&str, &str, &str)> = accounts
        .iter()
        .map(|(name, data_type, nullable)| (name.as_str(), data_type.as_str(), nullable.as_str()))
        .collect();
    assert_eq!(
        accounts,
        vec![
            ("id", "uuid", "NO"),
            ("username", "text", "NO"),
            ("password_hash", "text", "NO"),
            ("created_at", "timestamp with time zone", "NO"),
            ("password_changed_at", "timestamp with time zone", "NO"),
            ("telegram_user_id", "bigint", "YES"),
            ("pending_telegram_user_id", "bigint", "YES"),
            ("pending_telegram_name", "text", "YES"),
            ("console_origin", "text", "YES"),
        ],
        "admin_accounts must keep the design fields, always have a password, \
         and carry the optional Telegram binding"
    );

    let sessions = table_shape(&db.pool, "admin_sessions").await;
    let sessions: Vec<(&str, &str, &str)> = sessions
        .iter()
        .map(|(name, data_type, nullable)| (name.as_str(), data_type.as_str(), nullable.as_str()))
        .collect();
    assert_eq!(
        sessions,
        vec![
            ("id", "uuid", "NO"),
            ("token_hash", "text", "NO"),
            ("created_at", "timestamp with time zone", "NO"),
            ("last_seen_at", "timestamp with time zone", "NO"),
        ],
        "7 and 30 day limits are computed from created_at and last_seen_at"
    );

    let codes = table_shape(&db.pool, "admin_one_time_codes").await;
    let codes: Vec<(&str, &str, &str)> = codes
        .iter()
        .map(|(name, data_type, nullable)| (name.as_str(), data_type.as_str(), nullable.as_str()))
        .collect();
    assert_eq!(
        codes,
        vec![
            ("id", "uuid", "NO"),
            ("purpose", "text", "NO"),
            ("household_id", "uuid", "YES"),
            ("code_hash", "text", "NO"),
            ("expires_at", "timestamp with time zone", "YES"),
            ("used_at", "timestamp with time zone", "YES"),
            ("created_at", "timestamp with time zone", "NO"),
        ],
        "a one-time code may carry no household and no expiry yet"
    );

    let values: Vec<String> =
        sqlx::query_scalar("SELECT unnest(enum_range(NULL::audit_event_type))::text")
            .fetch_all(&db.pool)
            .await
            .expect("audit_event_type values");
    for expected in [
        "admin_login",
        "admin_login_failed",
        "admin_setup_code_failed",
        "admin_account_created",
        "admin_password_changed",
        "admin_recovery_issued",
        "admin_setting_changed",
    ] {
        assert!(
            values.iter().any(|value| value == expected),
            "audit_event_type must contain {expected}: {values:?}"
        );
    }
}

/// Учётка: ровно одна строка, непустое имя.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_accounts_migration_allows_a_single_account() {
    let db = support::empty_db().await;
    migrations::run(&db.pool)
        .await
        .expect("migrations should apply on a clean database");

    insert_admin_account(&db.pool, "admin")
        .await
        .expect("the first account is valid");

    let second = insert_admin_account(&db.pool, "another").await;
    assert_eq!(
        error_code(&second).as_deref(),
        Some("23505"),
        "the database must allow only one administrator account"
    );

    let blank = insert_admin_account(&db.pool, "   ").await;
    assert_eq!(
        error_code(&blank).as_deref(),
        Some("23514"),
        "a blank username must be rejected"
    );

    let accounts: i64 = sqlx::query_scalar("SELECT count(*) FROM admin_accounts")
        .fetch_one(&db.pool)
        .await
        .expect("accounts");
    assert_eq!(accounts, 1, "the rejected rows must not be stored");
}

/// Одноразовые коды: назначения, срок, household и уникальность хеша.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_one_time_codes_migration_enforces_purpose_expiry_and_household() {
    let db = support::empty_db().await;
    migrations::run(&db.pool)
        .await
        .expect("migrations should apply on a clean database");

    insert_one_time_code(&db.pool, "setup", None, &code_hash('a'), None)
        .await
        .expect("a setup code has no household and no expiry");

    insert_one_time_code(
        &db.pool,
        "recovery",
        None,
        &code_hash('b'),
        Some("15 minutes"),
    )
    .await
    .expect("a recovery code lives 15 minutes");

    let unknown = insert_one_time_code(&db.pool, "magic", None, &code_hash('c'), None).await;
    assert_eq!(
        error_code(&unknown).as_deref(),
        Some("23514"),
        "only the known purposes are accepted"
    );

    let household_id = Uuid::now_v7();
    sqlx::query("INSERT INTO households (id, name, timezone) VALUES ($1, 'Дом', 'Europe/Moscow')")
        .bind(household_id)
        .execute(&db.pool)
        .await
        .expect("household");

    let setup_with_household =
        insert_one_time_code(&db.pool, "setup", Some(household_id), &code_hash('d'), None).await;
    assert_eq!(
        error_code(&setup_with_household).as_deref(),
        Some("23514"),
        "in 0013 every purpose is installation-wide"
    );

    let recovery_without_expiry =
        insert_one_time_code(&db.pool, "recovery", None, &code_hash('e'), None).await;
    assert_eq!(
        error_code(&recovery_without_expiry).as_deref(),
        Some("23514"),
        "only a setup code may live without an expiry"
    );

    let duplicate = insert_one_time_code(
        &db.pool,
        "recovery",
        None,
        &code_hash('b'),
        Some("15 minutes"),
    )
    .await;
    assert_eq!(
        error_code(&duplicate).as_deref(),
        Some("23505"),
        "a code hash must be unique"
    );

    let raw_code = insert_one_time_code(&db.pool, "setup", None, "A3f-_code", None).await;
    assert_eq!(
        error_code(&raw_code).as_deref(),
        Some("23514"),
        "only a 64 hex character SHA-256 may be stored instead of the code"
    );

    // В 0013 строку с household вставить нельзя, поэтому проверяем определение
    // внешнего ключа по каталогу: 0014 будет выпускать коды household, и они
    // должны уходить вместе с ним.
    let foreign_key: Option<String> = sqlx::query_scalar(
        "SELECT pg_get_constraintdef(oid)
         FROM pg_constraint
         WHERE conrelid = 'admin_one_time_codes'::regclass AND contype = 'f'",
    )
    .fetch_optional(&db.pool)
    .await
    .expect("foreign key definition");
    let foreign_key = foreign_key.expect("admin_one_time_codes must reference households");
    assert!(
        foreign_key.contains("REFERENCES households(id)")
            && foreign_key.contains("ON DELETE CASCADE"),
        "household codes must be deleted with the household: {foreign_key}"
    );
}

/// Срок одноразового кода должен быть позже его создания.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_one_time_codes_migration_requires_expiry_after_creation() {
    let db = support::empty_db().await;
    migrations::run(&db.pool)
        .await
        .expect("migrations should apply on a clean database");

    for (fill, offset) in [('a', "now()"), ('b', "now() - interval '1 minute'")] {
        let expired = sqlx::query(&format!(
            "INSERT INTO admin_one_time_codes (id, purpose, code_hash, expires_at, created_at)
             VALUES ($1, 'recovery', $2, {offset}, now())"
        ))
        .bind(Uuid::now_v7())
        .bind(code_hash(fill))
        .execute(&db.pool)
        .await;
        assert_eq!(
            error_code(&expired).as_deref(),
            Some("23514"),
            "expires_at must be strictly later than created_at: {offset}"
        );
    }

    let valid = sqlx::query(
        "INSERT INTO admin_one_time_codes (id, purpose, code_hash, created_at, expires_at)
         VALUES ($1, 'recovery', $2, now() - interval '1 minute', now() + interval '14 minutes')",
    )
    .bind(Uuid::now_v7())
    .bind(code_hash('c'))
    .execute(&db.pool)
    .await;
    assert!(
        valid.is_ok(),
        "a recovery code created in the past may still be valid: {valid:?}"
    );
}

/// Сессии: уникальный хеш токена и порядок времён.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_sessions_migration_requires_a_unique_token_hash() {
    let db = support::empty_db().await;
    migrations::run(&db.pool)
        .await
        .expect("migrations should apply on a clean database");

    let hash = code_hash('f');
    sqlx::query("INSERT INTO admin_sessions (id, token_hash) VALUES ($1, $2)")
        .bind(Uuid::now_v7())
        .bind(&hash)
        .execute(&db.pool)
        .await
        .expect("a session with a SHA-256 token hash is valid");

    let duplicate = sqlx::query("INSERT INTO admin_sessions (id, token_hash) VALUES ($1, $2)")
        .bind(Uuid::now_v7())
        .bind(&hash)
        .execute(&db.pool)
        .await;
    assert_eq!(
        error_code(&duplicate).as_deref(),
        Some("23505"),
        "a token hash must be unique"
    );

    let raw_token = sqlx::query("INSERT INTO admin_sessions (id, token_hash) VALUES ($1, $2)")
        .bind(Uuid::now_v7())
        .bind("c2Vzc2lvbi10b2tlbi1pcy1ub3QtaGFzaGVk")
        .execute(&db.pool)
        .await;
    assert_eq!(
        error_code(&raw_token).as_deref(),
        Some("23514"),
        "the raw token must not fit the hash column"
    );

    let earlier_seen = sqlx::query(
        "INSERT INTO admin_sessions (id, token_hash, created_at, last_seen_at)
         VALUES ($1, $2, now(), now() - interval '1 minute')",
    )
    .bind(Uuid::now_v7())
    .bind(code_hash('e'))
    .execute(&db.pool)
    .await;
    assert_eq!(
        error_code(&earlier_seen).as_deref(),
        Some("23514"),
        "a session cannot be last seen before it was created"
    );
}

/// Новые события аудита пишутся и читаются через `AuditEventType`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_console_migration_round_trips_new_audit_events() {
    use tasklerq::domain::identity::AuditEventType;

    let db = support::empty_db().await;
    migrations::run(&db.pool)
        .await
        .expect("migrations should apply on a clean database");

    let expected = [
        AuditEventType::AdminLogin,
        AuditEventType::AdminLoginFailed,
        AuditEventType::AdminSetupCodeFailed,
        AuditEventType::AdminAccountCreated,
        AuditEventType::AdminPasswordChanged,
        AuditEventType::AdminRecoveryIssued,
        AuditEventType::AdminSettingChanged,
        AuditEventType::AdminTelegramLinked,
        AuditEventType::AdminTelegramUnlinked,
    ];
    for event_type in expected {
        sqlx::query(
            "INSERT INTO audit_events (id, household_id, actor_user_id, actor_type,
                                       event_type, payload)
             VALUES ($1, NULL, NULL, 'system', $2, '{}'::jsonb)",
        )
        .bind(Uuid::now_v7())
        .bind(event_type)
        .execute(&db.pool)
        .await
        .unwrap_or_else(|error| {
            panic!(
                "{} must be writable after the migration: {error}",
                event_type.as_str()
            )
        });
    }

    let stored: Vec<AuditEventType> = sqlx::query_scalar(
        "SELECT event_type FROM audit_events
         WHERE event_type::text LIKE 'admin\\_%'
         ORDER BY event_type::text",
    )
    .fetch_all(&db.pool)
    .await
    .expect("admin audit events");

    let mut stored: Vec<&str> = stored
        .iter()
        .map(|event_type| event_type.as_str())
        .collect();
    let mut expected: Vec<&str> = expected
        .iter()
        .map(|event_type| event_type.as_str())
        .collect();
    stored.sort_unstable();
    expected.sort_unstable();
    assert_eq!(stored, expected);
}

/// 0014: `owner_claim` — единственное назначение с household.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_telegram_migration_binds_codes_to_purposes_and_households() {
    let db = support::empty_db().await;
    migrations::run(&db.pool)
        .await
        .expect("migrations should apply on a clean database");

    let household_id = Uuid::now_v7();
    sqlx::query("INSERT INTO households (id, name, timezone) VALUES ($1, 'Дом', 'Europe/Moscow')")
        .bind(household_id)
        .execute(&db.pool)
        .await
        .expect("household");

    insert_one_time_code(
        &db.pool,
        "owner_claim",
        Some(household_id),
        &code_hash('1'),
        Some("24 hours"),
    )
    .await
    .expect("a claim code may point at a household without an owner");

    insert_one_time_code(
        &db.pool,
        "owner_claim",
        None,
        &code_hash('2'),
        Some("24 hours"),
    )
    .await
    .expect("a claim code of an empty single installation has no household");

    insert_one_time_code(
        &db.pool,
        "telegram_link",
        None,
        &code_hash('3'),
        Some("15 minutes"),
    )
    .await
    .expect("a telegram link code is installation-wide");

    insert_one_time_code(&db.pool, "setup", None, &code_hash('4'), None)
        .await
        .expect("setup still lives without an expiry and a household");

    // У `owner_claim` household — дом или NULL; у остальных назначений его нет.
    for (fill, purpose) in [('5', "telegram_link"), ('6', "setup"), ('7', "recovery")] {
        let with_household = insert_one_time_code(
            &db.pool,
            purpose,
            Some(household_id),
            &code_hash(fill),
            Some("15 minutes"),
        )
        .await;
        assert_eq!(
            error_code(&with_household).as_deref(),
            Some("23514"),
            "{purpose} must not reference a household"
        );
    }

    let unknown =
        insert_one_time_code(&db.pool, "magic", None, &code_hash('8'), Some("15 minutes")).await;
    assert_eq!(
        error_code(&unknown).as_deref(),
        Some("23514"),
        "only the four known purposes are accepted"
    );

    for (fill, purpose) in [('9', "telegram_link"), ('a', "owner_claim")] {
        let without_expiry =
            insert_one_time_code(&db.pool, purpose, None, &code_hash(fill), None).await;
        assert_eq!(
            error_code(&without_expiry).as_deref(),
            Some("23514"),
            "{purpose} must carry an expiry; only setup lives without one"
        );
    }
}

/// 0014: привязка Telegram и кандидат на привязку у единственной учётки.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_telegram_migration_enforces_the_telegram_binding() {
    let db = support::empty_db().await;
    migrations::run(&db.pool)
        .await
        .expect("migrations should apply on a clean database");

    insert_admin_account(&db.pool, "admin")
        .await
        .expect("the account is created before the binding");

    sqlx::query("UPDATE admin_accounts SET telegram_user_id = 7000000001")
        .execute(&db.pool)
        .await
        .expect("a confirmed telegram id is stored");
    sqlx::query("UPDATE admin_accounts SET telegram_user_id = NULL")
        .execute(&db.pool)
        .await
        .expect("NULL stays the unbound state");

    // Кандидат и его имя задаются только вместе.
    let only_id = sqlx::query("UPDATE admin_accounts SET pending_telegram_user_id = 7000000002")
        .execute(&db.pool)
        .await;
    assert_eq!(
        error_code(&only_id).as_deref(),
        Some("23514"),
        "a pending telegram id without a name is rejected"
    );

    let only_name = sqlx::query("UPDATE admin_accounts SET pending_telegram_name = 'Кандидат'")
        .execute(&db.pool)
        .await;
    assert_eq!(
        error_code(&only_name).as_deref(),
        Some("23514"),
        "a pending name without a telegram id is rejected"
    );

    sqlx::query(
        "UPDATE admin_accounts
         SET pending_telegram_user_id = 7000000002, pending_telegram_name = 'Кандидат'",
    )
    .execute(&db.pool)
    .await
    .expect("the pending candidate is stored with its name");

    sqlx::query(
        "UPDATE admin_accounts
         SET pending_telegram_user_id = NULL, pending_telegram_name = NULL,
             console_origin = 'http://192.168.1.10:8080'",
    )
    .execute(&db.pool)
    .await
    .expect("both pending fields may be cleared and the console origin stored");

    // Учётка одна на инсталляцию, поэтому второй строки для проверки
    // уникальности telegram_user_id нет. Проверяем сам уникальный индекс по
    // каталогу, а затем внутри транзакции временно снимаем индекс-одиночку:
    // повторная привязка того же id должна упереться именно в уникальность
    // telegram_user_id. Откат возвращает схему и единственную строку.
    let index_definition: Option<String> = sqlx::query_scalar(
        "SELECT indexdef FROM pg_indexes
         WHERE schemaname = 'public' AND tablename = 'admin_accounts'
           AND indexname = 'admin_accounts_telegram_user_id_idx'",
    )
    .fetch_optional(&db.pool)
    .await
    .expect("telegram index lookup");
    let index_definition =
        index_definition.expect("0014 must create a unique index on telegram_user_id");
    assert!(
        index_definition.starts_with("CREATE UNIQUE INDEX")
            && index_definition.contains("(telegram_user_id)"),
        "telegram_user_id must be unique: {index_definition}"
    );

    sqlx::query("UPDATE admin_accounts SET telegram_user_id = 7000000001")
        .execute(&db.pool)
        .await
        .expect("a confirmed telegram id before the duplicate attempt");

    let mut tx = db.pool.begin().await.expect("transaction");
    sqlx::query("DROP INDEX admin_accounts_singleton_idx")
        .execute(&mut *tx)
        .await
        .expect("the singleton index is dropped inside the transaction");
    let duplicate = sqlx::query(
        "INSERT INTO admin_accounts (id, username, password_hash, telegram_user_id)
         VALUES ($1, 'second', '$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$aGFzaA', 7000000001)",
    )
    .bind(Uuid::now_v7())
    .execute(&mut *tx)
    .await;
    assert_eq!(
        error_code(&duplicate).as_deref(),
        Some("23505"),
        "two accounts must not share one telegram id"
    );
    tx.rollback().await.expect("rollback");

    let accounts: Vec<(String, Option<i64>)> =
        sqlx::query_as("SELECT username, telegram_user_id FROM admin_accounts ORDER BY username")
            .fetch_all(&db.pool)
            .await
            .expect("accounts after the rollback");
    assert_eq!(
        accounts,
        vec![("admin".to_owned(), Some(7000000001))],
        "the rollback must keep the single account and its binding"
    );
}

/// 0014: значения аудита привязки Telegram.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_telegram_migration_adds_audit_values() {
    let db = support::empty_db().await;
    migrations::run(&db.pool)
        .await
        .expect("migrations should apply on a clean database");

    let values: Vec<String> =
        sqlx::query_scalar("SELECT unnest(enum_range(NULL::audit_event_type))::text")
            .fetch_all(&db.pool)
            .await
            .expect("audit_event_type values");
    for expected in ["admin_telegram_linked", "admin_telegram_unlinked"] {
        assert!(
            values.iter().any(|value| value == expected),
            "audit_event_type must contain {expected}: {values:?}"
        );
    }
}

/// 0014 применяется поверх базы 0021 и не теряет учётку и действующие коды.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_telegram_migration_keeps_existing_admin_rows() {
    let db = support::empty_db().await;
    let dir = std::env::temp_dir().join(format!(
        "tasklerq-migrations-0021-{}",
        Uuid::now_v7().simple()
    ));
    std::fs::create_dir_all(&dir).expect("temporary migration directory");
    copy_migrations_through(21, &dir);

    let before = Migrator::new(dir.as_path())
        .await
        .expect("a migrator through 0021 should resolve");
    before
        .run(&db.pool)
        .await
        .context("migrations through 0021 should apply")
        .expect("migrations through 0021");

    insert_admin_account(&db.pool, "admin")
        .await
        .expect("the account exists before 0014");
    let recovery_hash = code_hash('f');
    insert_one_time_code(
        &db.pool,
        "recovery",
        None,
        &recovery_hash,
        Some("15 minutes"),
    )
    .await
    .expect("an active recovery code exists before 0014");

    MIGRATOR
        .run(&db.pool)
        .await
        .expect("0014 must apply over existing admin rows");

    let account: (
        String,
        Option<i64>,
        Option<i64>,
        Option<String>,
        Option<String>,
    ) = sqlx::query_as(
        "SELECT username, telegram_user_id, pending_telegram_user_id,
                pending_telegram_name, console_origin
         FROM admin_accounts",
    )
    .fetch_one(&db.pool)
    .await
    .expect("the account after 0014");
    assert_eq!(
        account,
        ("admin".to_owned(), None, None, None, None),
        "the existing account stays and the new fields start empty"
    );

    let code: (String, Option<Uuid>) = sqlx::query_as(
        "SELECT purpose, household_id FROM admin_one_time_codes WHERE code_hash = $1",
    )
    .bind(&recovery_hash)
    .fetch_one(&db.pool)
    .await
    .expect("the recovery code after 0014");
    assert_eq!(
        (code.0.as_str(), code.1),
        ("recovery", None),
        "the existing recovery code keeps its purpose and household"
    );

    insert_one_time_code(
        &db.pool,
        "owner_claim",
        None,
        &code_hash('e'),
        Some("24 hours"),
    )
    .await
    .expect("the new purposes are accepted after the migration");

    std::fs::remove_dir_all(&dir).ok();
}
