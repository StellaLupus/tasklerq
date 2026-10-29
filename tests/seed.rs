//! Интеграционные тесты демонстрационных данных.

mod support;

use tasklerq::storage::seed;

async fn counts(pool: &sqlx::PgPool) -> (i64, i64, i64, i64, i64, i64, i64) {
    sqlx::query_as(
        "SELECT
             (SELECT count(*) FROM households),
             (SELECT count(*) FROM users),
             (SELECT count(*) FROM lists),
             (SELECT count(*) FROM items),
             (SELECT count(*) FROM subtasks),
             (SELECT count(*) FROM item_tags),
             (SELECT count(*) FROM item_events)",
    )
    .fetch_one(pool)
    .await
    .expect("counts")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn seed_is_idempotent_and_creates_demo_data() {
    let db = support::db().await;

    seed::run(&db.pool).await.expect("first seed");
    let first = counts(&db.pool).await;
    seed::run(&db.pool).await.expect("second seed");
    let second = counts(&db.pool).await;
    assert_eq!(first, second, "repeated seed must not create duplicates");

    let (households, users, lists, items, subtasks, item_tags, events) = first;
    assert_eq!(households, 1);
    assert_eq!(users, 2);
    assert_eq!(lists, 2);
    assert_eq!(items, 5);
    assert_eq!(subtasks, 2);
    assert_eq!(item_tags, 2);
    assert_eq!(events, 5);

    let (name, timezone): (String, String) =
        sqlx::query_as("SELECT name, timezone FROM households")
            .fetch_one(&db.pool)
            .await
            .expect("household");
    assert_eq!(name, "Демо-семья");
    assert_eq!(timezone, "Europe/Moscow");

    let (title, quantity, unit, status): (String, f64, String, String) = sqlx::query_as(
        "SELECT title, quantity::float8, unit, status::text
         FROM items WHERE title = 'Молоко'",
    )
    .fetch_one(&db.pool)
    .await
    .expect("milk");
    assert_eq!(title, "Молоко");
    assert_eq!(quantity, 2.0);
    assert_eq!(unit, "л");
    assert_eq!(status, "open");

    let (eggs_status, eggs_completed): (String, bool) = sqlx::query_as(
        "SELECT status::text, completed_at IS NOT NULL
         FROM items WHERE title = 'Яйца'",
    )
    .fetch_one(&db.pool)
    .await
    .expect("eggs");
    assert_eq!(eggs_status, "done");
    assert!(eggs_completed);

    let recurring: (String, i16, String) = sqlx::query_as(
        "SELECT recurrence_freq::text, recurrence_interval, recurrence_timezone
         FROM items WHERE title = 'Вынести мусор'",
    )
    .fetch_one(&db.pool)
    .await
    .expect("recurring task");
    assert_eq!(recurring, ("weekly".into(), 1, "Europe/Moscow".into()));
}
