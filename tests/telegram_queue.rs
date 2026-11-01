//! Очередь обновлений и воркер: дубликаты, повторы с задержкой и dead-letter.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use async_trait::async_trait;
use chrono::{DateTime, Duration, SubsecRound, Utc};
use serde_json::Value;
use sqlx::PgConnection;
use support::telegram::{FakeTelegram, Harness};
use support::{db, next_telegram_id};
use tasklerq::telegram::bot::Outcome;
use tasklerq::telegram::queue::{self, Failure};
use tasklerq::telegram::worker::{Pass, UPDATES_PER_MINUTE, UpdateHandler, UpdateWorker};
use tokio::sync::Notify;

/// Обработчик, который пишет эффект в базу и первые N раз падает уже после записи.
struct Flaky {
    failures_left: AtomicU32,
    marker: String,
}

#[async_trait]
impl UpdateHandler for Flaky {
    async fn handle(
        &self,
        conn: &mut PgConnection,
        _payload: &Value,
        _now: DateTime<Utc>,
    ) -> Result<Outcome, sqlx::Error> {
        // Эффект записан до ошибки: откат транзакции обязан его убрать.
        sqlx::query(
            "INSERT INTO tags (id, household_id, name) SELECT $1, id, $2 FROM households LIMIT 1",
        )
        .bind(uuid::Uuid::now_v7())
        .bind(&self.marker)
        .execute(&mut *conn)
        .await?;
        if self.failures_left.load(Ordering::SeqCst) > 0 {
            self.failures_left.fetch_sub(1, Ordering::SeqCst);
            return Err(sqlx::Error::PoolTimedOut);
        }
        Ok(Outcome::default())
    }
}

async fn ensure_household(pool: &sqlx::PgPool) {
    support::create_household(pool).await;
}

fn worker(
    pool: &sqlx::PgPool,
    chat: i64,
    handler: Arc<dyn UpdateHandler>,
    max: u32,
) -> UpdateWorker {
    UpdateWorker::new(
        pool.clone(),
        handler,
        FakeTelegram::new(),
        max,
        Arc::new(Notify::new()),
    )
    .only_chat(chat)
}

async fn status_of(pool: &sqlx::PgPool, update_id: i64) -> (String, i32, Option<String>) {
    sqlx::query_as(
        "SELECT status::text, attempts, last_error FROM telegram_updates WHERE update_id = $1",
    )
    .bind(update_id)
    .fetch_one(pool)
    .await
    .expect("status")
}

async fn marker_count(pool: &sqlx::PgPool, marker: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM tags WHERE name = $1")
        .bind(marker)
        .fetch_one(pool)
        .await
        .expect("count")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_update_id_is_stored_and_applied_once() {
    let db = db().await;
    ensure_household(&db.pool).await;
    let chat = next_telegram_id();
    let handler = Arc::new(Flaky {
        failures_left: AtomicU32::new(0),
        marker: format!("dup-{chat}"),
    });
    let worker = worker(&db.pool, chat, handler, 10);
    let mut bot = Harness::new(&db.pool, chat);

    let update = bot.text_update("Ann", "привет");
    let id = update["update_id"].as_i64().expect("id");
    let mut conn = db.pool.acquire().await.expect("conn");
    assert!(
        queue::enqueue(&mut conn, id, &update, Utc::now())
            .await
            .expect("first")
    );
    assert!(
        !queue::enqueue(&mut conn, id, &update, Utc::now())
            .await
            .expect("second")
    );
    assert!(
        !queue::enqueue(&mut conn, id, &update, Utc::now())
            .await
            .expect("third")
    );

    assert_eq!(
        worker.run_once(Utc::now()).await.expect("pass"),
        Pass::Processed
    );
    assert_eq!(worker.run_once(Utc::now()).await.expect("pass"), Pass::Idle);
    assert_eq!(marker_count(&db.pool, &format!("dup-{chat}")).await, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn temporary_failure_is_retried_with_backoff_and_applied_once() {
    let db = db().await;
    ensure_household(&db.pool).await;
    let chat = next_telegram_id();
    let marker = format!("retry-{chat}");
    let handler = Arc::new(Flaky {
        failures_left: AtomicU32::new(2),
        marker: marker.clone(),
    });
    let worker = worker(&db.pool, chat, handler, 10);
    let mut bot = Harness::new(&db.pool, chat);
    let update = bot.text_update("Ann", "привет");
    let id = update["update_id"].as_i64().expect("id");
    // PostgreSQL хранит микросекунды.
    let t0 = Utc::now().trunc_subsecs(6);
    let mut conn = db.pool.acquire().await.expect("conn");
    queue::enqueue(&mut conn, id, &update, t0)
        .await
        .expect("enqueue");

    // Первая ошибка: эффект откатан, повтор через 1 секунду.
    let pass = worker.run_once(t0).await.expect("pass");
    assert_eq!(
        pass,
        Pass::Failed(Failure::Retry(t0 + Duration::seconds(1)))
    );
    assert_eq!(
        marker_count(&db.pool, &marker).await,
        0,
        "failed attempt must roll back"
    );
    assert_eq!(
        status_of(&db.pool, id).await,
        ("pending".into(), 1, Some("database".into()))
    );

    // До истечения задержки обновление не берётся.
    assert_eq!(
        worker
            .run_once(t0 + Duration::milliseconds(500))
            .await
            .expect("pass"),
        Pass::Idle
    );

    // Вторая ошибка: задержка удваивается.
    let t1 = t0 + Duration::seconds(1);
    assert_eq!(
        worker.run_once(t1).await.expect("pass"),
        Pass::Failed(Failure::Retry(t1 + Duration::seconds(2)))
    );

    // Успех: эффект применён ровно один раз.
    let t2 = t1 + Duration::seconds(2);
    assert_eq!(worker.run_once(t2).await.expect("pass"), Pass::Processed);
    assert_eq!(marker_count(&db.pool, &marker).await, 1);
    assert_eq!(status_of(&db.pool, id).await.0, "processed");
    assert_eq!(
        worker
            .run_once(t2 + Duration::hours(1))
            .await
            .expect("pass"),
        Pass::Idle
    );
    assert_eq!(marker_count(&db.pool, &marker).await, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exhausted_attempts_go_to_dead_letter_without_partial_effects() {
    let db = db().await;
    ensure_household(&db.pool).await;
    let chat = next_telegram_id();
    let marker = format!("dead-{chat}");
    let handler = Arc::new(Flaky {
        failures_left: AtomicU32::new(100),
        marker: marker.clone(),
    });
    let worker = worker(&db.pool, chat, handler, 3);
    let mut bot = Harness::new(&db.pool, chat);
    let update = bot.text_update("Ann", "привет");
    let id = update["update_id"].as_i64().expect("id");
    let mut now = Utc::now();
    let mut conn = db.pool.acquire().await.expect("conn");
    queue::enqueue(&mut conn, id, &update, now)
        .await
        .expect("enqueue");

    let mut outcomes = Vec::new();
    for _ in 0..3 {
        let pass = worker.run_once(now).await.expect("pass");
        if let Pass::Failed(Failure::Retry(at)) = pass {
            now = at;
        }
        outcomes.push(pass);
    }

    assert_eq!(outcomes[2], Pass::Failed(Failure::Dead));
    assert_eq!(
        status_of(&db.pool, id).await,
        ("dead".into(), 3, Some("database".into()))
    );
    assert_eq!(
        marker_count(&db.pool, &marker).await,
        0,
        "no partial effect may remain"
    );
    assert_eq!(
        worker
            .run_once(now + Duration::days(1))
            .await
            .expect("pass"),
        Pass::Idle
    );
    assert!(queue::counts(&mut conn).await.expect("counts").dead >= 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_workers_never_claim_the_same_update() {
    let db = db().await;
    let chat = next_telegram_id();
    let mut bot = Harness::new(&db.pool, chat);
    let update = bot.text_update("Ann", "привет");
    let id = update["update_id"].as_i64().expect("id");
    let now = Utc::now();
    let mut conn = db.pool.acquire().await.expect("conn");
    queue::enqueue(&mut conn, id, &update, now)
        .await
        .expect("enqueue");

    let mut first = db.pool.begin().await.expect("tx");
    let mut second = db.pool.begin().await.expect("tx");
    let claimed = queue::claim_next(&mut first, now, Some(chat))
        .await
        .expect("claim");
    let skipped = queue::claim_next(&mut second, now, Some(chat))
        .await
        .expect("claim");

    assert!(claimed.is_some());
    assert!(
        skipped.is_none(),
        "SKIP LOCKED must hide a row locked by another worker"
    );
}

/// Обработчик, который паникует после записи эффекта.
struct Panicking {
    marker: String,
}

#[async_trait]
impl UpdateHandler for Panicking {
    async fn handle(
        &self,
        conn: &mut PgConnection,
        _payload: &Value,
        _now: DateTime<Utc>,
    ) -> Result<Outcome, sqlx::Error> {
        sqlx::query(
            "INSERT INTO tags (id, household_id, name) SELECT $1, id, $2 FROM households LIMIT 1",
        )
        .bind(uuid::Uuid::now_v7())
        .bind(&self.marker)
        .execute(&mut *conn)
        .await?;
        panic!("poison update");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn panicking_handler_counts_as_failed_attempt_and_ends_in_dead_letter() {
    let db = db().await;
    ensure_household(&db.pool).await;
    let chat = next_telegram_id();
    let marker = format!("panic-{chat}");
    let worker = worker(
        &db.pool,
        chat,
        Arc::new(Panicking {
            marker: marker.clone(),
        }),
        3,
    );
    let mut bot = Harness::new(&db.pool, chat);
    let update = bot.text_update("Ann", "секретный текст");
    let id = update["update_id"].as_i64().expect("id");
    let mut now = Utc::now();
    let mut conn = db.pool.acquire().await.expect("conn");
    queue::enqueue(&mut conn, id, &update, now)
        .await
        .expect("enqueue");

    let mut outcomes = Vec::new();
    for _ in 0..3 {
        let pass = worker
            .run_once(now)
            .await
            .expect("a panic must not escape the worker");
        if let Pass::Failed(Failure::Retry(at)) = pass {
            now = at;
        }
        outcomes.push(pass);
    }

    assert!(matches!(outcomes[0], Pass::Failed(Failure::Retry(_))));
    assert_eq!(outcomes[2], Pass::Failed(Failure::Dead));
    assert_eq!(
        status_of(&db.pool, id).await,
        ("dead".into(), 3, Some("panic".into()))
    );
    assert_eq!(marker_count(&db.pool, &marker).await, 0);
    let payload: String =
        sqlx::query_scalar("SELECT payload::text FROM telegram_updates WHERE update_id = $1")
            .bind(id)
            .fetch_one(&db.pool)
            .await
            .expect("payload");
    assert!(
        !payload.contains("секретный текст"),
        "dead update must not keep user text: {payload}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn processed_update_does_not_keep_user_text() {
    let db = db().await;
    ensure_household(&db.pool).await;
    let chat = next_telegram_id();
    let handler = Arc::new(Flaky {
        failures_left: AtomicU32::new(0),
        marker: format!("redact-{chat}"),
    });
    let worker = worker(&db.pool, chat, handler, 10);
    let mut bot = Harness::new(&db.pool, chat);
    let update = bot.text_update("Ann", "/start INVITECODE");
    let id = update["update_id"].as_i64().expect("id");
    let mut conn = db.pool.acquire().await.expect("conn");
    queue::enqueue(&mut conn, id, &update, Utc::now())
        .await
        .expect("enqueue");

    assert_eq!(
        worker.run_once(Utc::now()).await.expect("pass"),
        Pass::Processed
    );

    let payload: String =
        sqlx::query_scalar("SELECT payload::text FROM telegram_updates WHERE update_id = $1")
            .bind(id)
            .fetch_one(&db.pool)
            .await
            .expect("payload");
    assert!(
        !payload.contains("INVITECODE"),
        "an invite code must not outlive processing: {payload}"
    );
}

/// Обработчик, отмечающий каждое выполненное обновление.
struct Recorder {
    prefix: String,
}

#[async_trait]
impl UpdateHandler for Recorder {
    async fn handle(
        &self,
        conn: &mut PgConnection,
        payload: &Value,
        _now: DateTime<Utc>,
    ) -> Result<Outcome, sqlx::Error> {
        let update_id = payload["update_id"].as_i64().unwrap_or_default();
        sqlx::query(
            "INSERT INTO tags (id, household_id, name) SELECT $1, id, $2 FROM households LIMIT 1",
        )
        .bind(uuid::Uuid::now_v7())
        .bind(format!("{}-{update_id}", self.prefix))
        .execute(&mut *conn)
        .await?;
        Ok(Outcome::default())
    }
}

/// Обработчик, который первый раз падает, а затем записывает эффект.
struct FailFirst {
    failed: std::sync::Mutex<Option<i64>>,
    prefix: String,
}

#[async_trait]
impl UpdateHandler for FailFirst {
    async fn handle(
        &self,
        conn: &mut PgConnection,
        payload: &Value,
        _now: DateTime<Utc>,
    ) -> Result<Outcome, sqlx::Error> {
        let update_id = payload["update_id"].as_i64().unwrap_or_default();
        {
            let mut failed = self.failed.lock().expect("lock");
            if failed.is_none() {
                *failed = Some(update_id);
                return Err(sqlx::Error::PoolTimedOut);
            }
        }
        sqlx::query(
            "INSERT INTO tags (id, household_id, name) SELECT $1, id, $2 FROM households LIMIT 1",
        )
        .bind(uuid::Uuid::now_v7())
        .bind(format!("{}-{update_id}", self.prefix))
        .execute(&mut *conn)
        .await?;
        Ok(Outcome::default())
    }
}

async fn recorded(pool: &sqlx::PgPool, prefix: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM tags WHERE name LIKE $1")
        .bind(format!("{prefix}-%"))
        .fetch_one(pool)
        .await
        .expect("count")
}

async fn enqueue_at(pool: &sqlx::PgPool, update: &Value, now: DateTime<Utc>) -> i64 {
    let id = update["update_id"].as_i64().expect("id");
    let mut conn = pool.acquire().await.expect("conn");
    queue::enqueue(&mut conn, id, update, now)
        .await
        .expect("enqueue");
    id
}

fn limited_worker(
    pool: &sqlx::PgPool,
    chat: i64,
    handler: Arc<dyn UpdateHandler>,
    fake: Arc<FakeTelegram>,
) -> UpdateWorker {
    UpdateWorker::new(pool.clone(), handler, fake, 10, Arc::new(Notify::new()))
        .only_chat(chat)
        .with_rate_limit(UPDATES_PER_MINUTE)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_twenty_first_message_in_a_minute_is_skipped_with_one_retry_notice() {
    let db = db().await;
    ensure_household(&db.pool).await;
    let chat = next_telegram_id();
    let fake = FakeTelegram::new();
    let prefix = format!("rl{chat}");
    let worker = limited_worker(
        &db.pool,
        chat,
        Arc::new(Recorder {
            prefix: prefix.clone(),
        }),
        fake.clone(),
    );
    let mut bot = Harness::new(&db.pool, chat);
    let now = Utc::now().trunc_subsecs(6);

    let mut ids = Vec::new();
    for _ in 0..22 {
        let update = bot.text_update("Ann", "привет");
        ids.push(enqueue_at(&db.pool, &update, now).await);
    }

    for _ in 0..20 {
        assert_eq!(worker.run_once(now).await.expect("pass"), Pass::Processed);
    }
    assert_eq!(recorded(&db.pool, &prefix).await, 20);

    // 21-е и 22-е обновления не выполняются, но помечаются обработанными.
    for _ in 0..2 {
        assert_eq!(worker.run_once(now).await.expect("pass"), Pass::Processed);
    }
    assert_eq!(
        recorded(&db.pool, &prefix).await,
        20,
        "updates over the limit must not reach the handler"
    );
    for id in ids {
        assert_eq!(status_of(&db.pool, id).await.0, "processed");
    }

    // В чат ушло ровно одно предупреждение с временем повтора.
    let notices: Vec<String> = sqlx::query_scalar(
        "SELECT payload->>'html' FROM outbox WHERE chat_id = $1 AND kind = 'message'",
    )
    .bind(chat)
    .fetch_all(&db.pool)
    .await
    .expect("notices");
    assert_eq!(notices.len(), 1, "one notice per window: {notices:?}");
    assert!(
        notices[0].contains("Попробуйте через"),
        "notice must tell when to retry: {}",
        notices[0]
    );

    // После окна обновление снова обрабатывается.
    let later = now + Duration::seconds(61);
    let update = bot.text_update("Ann", "привет");
    enqueue_at(&db.pool, &update, later).await;
    assert_eq!(worker.run_once(later).await.expect("pass"), Pass::Processed);
    assert_eq!(recorded(&db.pool, &prefix).await, 21);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_retried_update_does_not_consume_the_rate_limit_twice() {
    let db = db().await;
    ensure_household(&db.pool).await;
    let chat = next_telegram_id();
    let fake = FakeTelegram::new();
    let prefix = format!("retry{chat}");
    let worker = UpdateWorker::new(
        db.pool.clone(),
        Arc::new(FailFirst {
            failed: std::sync::Mutex::new(None),
            prefix: prefix.clone(),
        }),
        fake,
        10,
        Arc::new(Notify::new()),
    )
    .only_chat(chat)
    .with_rate_limit(2);
    let mut bot = Harness::new(&db.pool, chat);
    let now = Utc::now().trunc_subsecs(6);

    let first = bot.text_update("Ann", "привет");
    enqueue_at(&db.pool, &first, now).await;
    assert!(matches!(
        worker.run_once(now).await.expect("pass"),
        Pass::Failed(Failure::Retry(_))
    ));

    // Повтор уже учтённого обновления не отнимает второй слот.
    let retry_at = now + Duration::seconds(1);
    assert_eq!(
        worker.run_once(retry_at).await.expect("pass"),
        Pass::Processed
    );
    assert_eq!(recorded(&db.pool, &prefix).await, 1);

    // Новое действие пользователя ещё помещается в лимит из двух.
    let second = bot.text_update("Ann", "привет");
    enqueue_at(&db.pool, &second, retry_at).await;
    assert_eq!(
        worker.run_once(retry_at).await.expect("pass"),
        Pass::Processed
    );
    assert_eq!(recorded(&db.pool, &prefix).await, 2);

    // Третье новое действие уже сверх лимита.
    let third = bot.text_update("Ann", "привет");
    enqueue_at(&db.pool, &third, retry_at).await;
    assert_eq!(
        worker.run_once(retry_at).await.expect("pass"),
        Pass::Processed
    );
    assert_eq!(
        recorded(&db.pool, &prefix).await,
        2,
        "a retry must not consume a rate limit slot"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_twenty_first_callback_is_not_run_and_is_answered_with_retry_time() {
    let db = db().await;
    ensure_household(&db.pool).await;
    let chat = next_telegram_id();
    let fake = FakeTelegram::new();
    let prefix = format!("cb{chat}");
    let worker = limited_worker(
        &db.pool,
        chat,
        Arc::new(Recorder {
            prefix: prefix.clone(),
        }),
        fake.clone(),
    );
    let mut bot = Harness::new(&db.pool, chat);
    let now = Utc::now().trunc_subsecs(6);

    for _ in 0..21 {
        let update = bot.callback_update("Ann", "t:AAAAAAAAAAAAAAAA", 1);
        enqueue_at(&db.pool, &update, now).await;
    }

    for _ in 0..20 {
        assert_eq!(worker.run_once(now).await.expect("pass"), Pass::Processed);
    }
    assert_eq!(recorded(&db.pool, &prefix).await, 20);

    assert_eq!(worker.run_once(now).await.expect("pass"), Pass::Processed);
    assert_eq!(
        recorded(&db.pool, &prefix).await,
        20,
        "the 21st callback must not change data"
    );

    let answers = fake.answers();
    assert_eq!(
        answers.len(),
        1,
        "the callback must be answered: {answers:?}"
    );
    let text = answers[0].1.as_deref().expect("toast text");
    assert!(
        text.contains("Попробуйте через"),
        "toast must tell when to retry: {text}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sql_backoff_matches_the_rust_rule_for_every_attempt() {
    let db = db().await;
    let chat = next_telegram_id();
    let mut bot = Harness::new(&db.pool, chat);
    let update = bot.text_update("Ann", "привет");
    let id = update["update_id"].as_i64().expect("id");
    let now = Utc::now().trunc_subsecs(6);
    let mut conn = db.pool.acquire().await.expect("conn");
    queue::enqueue(&mut conn, id, &update, now)
        .await
        .expect("enqueue");

    for attempt in 1..=20 {
        let failure = queue::record_failure(&mut conn, id, "database", 1_000, now)
            .await
            .expect("failure");
        assert_eq!(
            failure,
            Failure::Retry(now + queue::backoff(attempt)),
            "attempt {attempt}"
        );
    }
}
