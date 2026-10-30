//! Очередь разбора: аренда, повторы, исчерпание попыток и редакция текста.

mod support;

use chrono::{Duration, Utc};
use sqlx::PgPool;
use tasklerq::storage::repositories::capture_jobs::{self, Failure, LEASE, NewJob};
use uuid::Uuid;

use support::TestHousehold;

/// Чат, уникальный для теста: очередь общая, а `only_chat` отделяет свои задания.
fn chat() -> i64 {
    support::next_telegram_id()
}

async fn enqueue(pool: &PgPool, household: &TestHousehold, chat_id: i64, text: &str) -> Uuid {
    let mut conn = pool.acquire().await.expect("connection");
    capture_jobs::enqueue(
        &mut conn,
        NewJob::telegram_text(household.household_id, household.owner_id, chat_id, text),
        Utc::now(),
    )
    .await
    .expect("enqueue")
}

async fn status(pool: &PgPool, id: Uuid) -> (String, i32, Option<String>, String) {
    sqlx::query_as(
        "SELECT status::text, attempts, last_error, payload::text FROM capture_jobs WHERE id = $1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .expect("job")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claim_takes_a_ready_job_once_and_counts_the_attempt() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let chat = chat();
    let id = enqueue(&db.pool, &household, chat, "купить молоко").await;
    let now = Utc::now();
    let mut conn = db.pool.acquire().await.expect("connection");

    let job = capture_jobs::claim_next(&mut conn, now, 3, Some(chat))
        .await
        .expect("claim")
        .expect("job");
    assert_eq!(job.id, id);
    assert_eq!(job.attempts, 1);
    assert_eq!(job.text(), Some("купить молоко"));

    let again = capture_jobs::claim_next(&mut conn, now, 3, Some(chat))
        .await
        .expect("claim");
    assert!(again.is_none(), "a leased job is not handed out twice");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_workers_never_claim_the_same_job() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let chat = chat();
    for n in 0..6 {
        enqueue(&db.pool, &household, chat, &format!("задача {n}")).await;
    }

    let mut tasks = Vec::new();
    for _ in 0..12 {
        let pool = db.pool.clone();
        tasks.push(tokio::spawn(async move {
            let mut conn = pool.acquire().await.expect("connection");
            capture_jobs::claim_next(&mut conn, Utc::now(), 3, Some(chat))
                .await
                .expect("claim")
                .map(|job| job.id)
        }));
    }
    let mut claimed = Vec::new();
    for task in tasks {
        if let Some(id) = task.await.expect("task") {
            claimed.push(id);
        }
    }

    let unique: std::collections::HashSet<_> = claimed.iter().collect();
    assert_eq!(claimed.len(), 6, "every job is claimed");
    assert_eq!(unique.len(), 6, "no job is claimed twice");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crashed_worker_job_returns_after_the_lease_expires() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let chat = chat();
    let id = enqueue(&db.pool, &household, chat, "купить хлеб").await;
    let now = Utc::now();
    let mut conn = db.pool.acquire().await.expect("connection");

    capture_jobs::claim_next(&mut conn, now, 3, Some(chat))
        .await
        .expect("claim")
        .expect("job");
    // Воркер «упал»: ни complete, ни record_failure.
    let during = capture_jobs::claim_next(&mut conn, now + Duration::seconds(30), 3, Some(chat))
        .await
        .expect("claim");
    assert!(during.is_none());

    let after =
        capture_jobs::claim_next(&mut conn, now + LEASE + Duration::seconds(1), 3, Some(chat))
            .await
            .expect("claim")
            .expect("job returns");
    assert_eq!(after.id, id);
    assert_eq!(after.attempts, 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn job_that_keeps_crashing_workers_ends_dead_without_its_text() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let chat = chat();
    let id = enqueue(&db.pool, &household, chat, "ядовитое сообщение").await;
    let mut conn = db.pool.acquire().await.expect("connection");

    let mut now = Utc::now();
    for _ in 0..3 {
        capture_jobs::claim_next(&mut conn, now, 3, Some(chat))
            .await
            .expect("claim")
            .expect("job");
        now += LEASE + Duration::seconds(1);
    }
    let fourth = capture_jobs::claim_next(&mut conn, now, 3, Some(chat))
        .await
        .expect("claim");
    assert!(fourth.is_none(), "attempts are exhausted");

    let (state, attempts, error, payload) = status(&db.pool, id).await;
    assert_eq!(state, "dead");
    assert_eq!(attempts, 3);
    assert_eq!(error.as_deref(), Some("lease_expired"));
    assert!(!payload.contains("ядовитое"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retryable_failure_requeues_with_backoff_until_attempts_run_out() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let chat = chat();
    let id = enqueue(&db.pool, &household, chat, "купить сыр").await;
    let mut conn = db.pool.acquire().await.expect("connection");
    let mut now = Utc::now();

    for attempt in 1..=2 {
        let job = capture_jobs::claim_next(&mut conn, now, 3, Some(chat))
            .await
            .expect("claim")
            .expect("job");
        assert_eq!(job.attempts, attempt);
        let failure = capture_jobs::record_failure(&mut conn, &job, "server_error", true, 3, now)
            .await
            .expect("failure");
        let Failure::Retry(at) = failure else {
            panic!("attempt {attempt} must be retried");
        };
        assert_eq!(at, now + capture_jobs::backoff(attempt));
        // До срока повтора задание недоступно.
        assert!(
            capture_jobs::claim_next(&mut conn, now, 3, Some(chat))
                .await
                .expect("claim")
                .is_none()
        );
        now = at;
    }

    let last = capture_jobs::claim_next(&mut conn, now, 3, Some(chat))
        .await
        .expect("claim")
        .expect("job");
    let failure = capture_jobs::record_failure(&mut conn, &last, "server_error", true, 3, now)
        .await
        .expect("failure");
    assert_eq!(failure, Failure::Dead);
    let (state, _, error, payload) = status(&db.pool, id).await;
    assert_eq!(state, "dead");
    assert_eq!(error.as_deref(), Some("server_error"));
    assert!(!payload.contains("купить сыр"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn permanent_failure_does_not_retry() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let chat = chat();
    enqueue(&db.pool, &household, chat, "купить масло").await;
    let mut conn = db.pool.acquire().await.expect("connection");
    let now = Utc::now();

    let job = capture_jobs::claim_next(&mut conn, now, 3, Some(chat))
        .await
        .expect("claim")
        .expect("job");
    let failure = capture_jobs::record_failure(&mut conn, &job, "rejected", false, 3, now)
        .await
        .expect("failure");

    assert_eq!(failure, Failure::Dead);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completed_job_keeps_no_text_and_is_purged_after_retention() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let chat = chat();
    let id = enqueue(&db.pool, &household, chat, "секретный подарок").await;
    let mut conn = db.pool.acquire().await.expect("connection");
    let now = Utc::now();

    let job = capture_jobs::claim_next(&mut conn, now, 3, Some(chat))
        .await
        .expect("claim")
        .expect("job");
    capture_jobs::complete(&mut conn, job.id, None, now)
        .await
        .expect("complete");

    let (state, _, _, payload) = status(&db.pool, id).await;
    assert_eq!(state, "done");
    assert!(!payload.contains("подарок"));

    capture_jobs::purge(&mut conn, now + Duration::days(1))
        .await
        .expect("purge");
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM capture_jobs WHERE id = $1")
        .bind(id)
        .fetch_one(&db.pool)
        .await
        .expect("count");
    assert_eq!(left, 0);
}
