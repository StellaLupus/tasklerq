//! Backup и restore через те же скрипты и `docker exec`, что и на проде.

mod support;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use sqlx::postgres::PgPoolOptions;
use tasklerq::storage::migrations;
use uuid::Uuid;

struct TempDir(PathBuf);

impl TempDir {
    fn new(prefix: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("tasklerq-{prefix}-{}", Uuid::now_v7().simple()));
        std::fs::create_dir_all(&path).expect("temporary directory");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

fn script(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("scripts")
        .join(name)
}

fn env(
    container: &str,
    user: &str,
    database: &str,
    backup_dir: &Path,
    keep_days: &str,
) -> Vec<(String, String)> {
    vec![
        ("TASKLERQ_PG_CONTAINER".to_owned(), container.to_owned()),
        ("TASKLERQ_PG_USER".to_owned(), user.to_owned()),
        ("TASKLERQ_DB".to_owned(), database.to_owned()),
        (
            "TASKLERQ_BACKUP_DIR".to_owned(),
            backup_dir.to_string_lossy().into_owned(),
        ),
        ("TASKLERQ_BACKUP_KEEP_DAYS".to_owned(), keep_days.to_owned()),
        ("DOCKER".to_owned(), "docker".to_owned()),
    ]
}

fn run(script_path: &Path, args: &[&str], environment: &[(String, String)]) -> Output {
    Command::new(script_path)
        .args(args)
        .envs(environment.iter().map(|(name, value)| (name, value)))
        .output()
        .expect("the script should start")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn dumps(dir: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("backup directory")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("tasklerq-") && name.ends_with(".dump"))
        })
        .collect();
    found.sort();
    found
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn backup_then_restore_into_a_new_database_keeps_migrations_and_data() {
    let db = support::db().await;
    let container = support::container_id().await;
    let household = support::create_household(&db.pool).await;
    let item = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        support::new_item(household.tasks_list_id, "Купить лекарство"),
    )
    .await;

    let dir = TempDir::new("backup");
    let backup = run(
        &script("backup.sh"),
        &[],
        &env(&container, "tasklerq", "tasklerq", dir.path(), "14"),
    );
    assert!(
        backup.status.success(),
        "backup failed: {}",
        stderr(&backup)
    );

    let dump = dumps(dir.path())
        .into_iter()
        .next()
        .expect("the backup script should create one dump");

    let restored = format!("restore_{}", Uuid::now_v7().simple());
    let restore = run(
        &script("restore.sh"),
        &[dump.to_str().expect("dump path"), &restored],
        &env(&container, "tasklerq", "tasklerq", dir.path(), "14"),
    );
    assert!(
        restore.status.success(),
        "restore failed: {}",
        stderr(&restore)
    );

    // Повторный restore в ту же базу отказан: рабочую базу не перезаписываем.
    let repeated = run(
        &script("restore.sh"),
        &[dump.to_str().expect("dump path"), &restored],
        &env(&container, "tasklerq", "tasklerq", dir.path(), "14"),
    );
    assert!(
        !repeated.status.success(),
        "restore into an existing database must fail"
    );
    assert!(
        stderr(&repeated).contains("already exists"),
        "the refusal must be explicit: {}",
        stderr(&repeated)
    );

    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&support::database_url_for(&restored).await)
        .await
        .expect("the restored database should accept connections");

    assert!(
        migrations::all_applied(&pool)
            .await
            .expect("applied migrations"),
        "the restored database must keep the migration table"
    );

    let household_present: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM households WHERE id = $1)")
            .bind(household.household_id)
            .fetch_one(&pool)
            .await
            .expect("household check");
    assert!(household_present, "the household must be restored");

    let item_present: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM items WHERE id = $1 AND title = $2)")
            .bind(item.id)
            .bind("Купить лекарство")
            .fetch_one(&pool)
            .await
            .expect("item check");
    assert!(item_present, "the task must be restored");

    pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_backup_leaves_no_dump_and_retention_removes_only_old_files() {
    let container = support::container_id().await;

    let missing = TempDir::new("backup-missing");
    let failed = run(
        &script("backup.sh"),
        &[],
        &env(
            &container,
            "tasklerq",
            &format!("missing_{}", Uuid::now_v7().simple()),
            missing.path(),
            "14",
        ),
    );
    assert!(
        !failed.status.success(),
        "backup of a missing database must fail"
    );
    assert!(
        !stderr(&failed).is_empty(),
        "a failed backup must explain itself on stderr"
    );
    assert!(
        std::fs::read_dir(missing.path())
            .expect("backup directory")
            .next()
            .is_none(),
        "a failed backup must not leave a dump or a partial file in the directory"
    );

    let dir = TempDir::new("backup-retention");
    let old = dir.path().join("tasklerq-2000-01-01T000000.dump");
    let fresh = dir.path().join("tasklerq-2999-01-01T000000.dump");
    std::fs::write(&old, b"old").expect("old dump");
    std::fs::write(&fresh, b"fresh").expect("fresh dump");
    for (path, age) in [(&old, "20 days ago"), (&fresh, "2 days ago")] {
        let status = Command::new("touch")
            .args(["-d", age])
            .arg(path)
            .status()
            .expect("touch should run");
        assert!(status.success(), "touch failed for {}", path.display());
    }

    let backup = run(
        &script("backup.sh"),
        &[],
        &env(&container, "tasklerq", "tasklerq", dir.path(), "7"),
    );
    assert!(
        backup.status.success(),
        "backup failed: {}",
        stderr(&backup)
    );

    assert!(!old.exists(), "a dump older than the retention is deleted");
    assert!(fresh.exists(), "a fresh dump is kept");
    assert_eq!(
        dumps(dir.path()).len(),
        2,
        "one fresh dump plus the new backup remain"
    );
}
