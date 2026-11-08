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
use tasklerq::security_audit::Audit;
use tasklerq::telegram::bot::model::Form;
use tasklerq::telegram::bot::{Bot, Outcome};
use tasklerq::telegram::queue::{self, Failure};
use tasklerq::telegram::update::{self, Classified};
use tasklerq::telegram::worker::{
    DEFERRED_QUEUE_LIMIT, Pass, UPDATES_PER_MINUTE, UpdateHandler, UpdateWorker,
};
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

/// Настоящий бот, который после обработки сообщает о сбое: проверяет, что
/// откат транзакции убирает и строку удаления входящего сообщения.
struct BotThenFail {
    bot: Bot,
    failures_left: AtomicU32,
}

#[async_trait]
impl UpdateHandler for BotThenFail {
    async fn handle(
        &self,
        conn: &mut PgConnection,
        payload: &Value,
        now: DateTime<Utc>,
    ) -> Result<Outcome, sqlx::Error> {
        let parsed = update::parse(payload).map_err(|error| {
            sqlx::Error::Protocol(format!("stored update is malformed: {error}"))
        })?;
        let Classified::Handle(incoming) = update::classify(&parsed) else {
            return Ok(Outcome::default());
        };
        let outcome = self.bot.handle(conn, incoming, now).await?;
        if self.failures_left.load(Ordering::SeqCst) > 0 {
            self.failures_left.fetch_sub(1, Ordering::SeqCst);
            return Err(sqlx::Error::PoolTimedOut);
        }
        Ok(outcome)
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
        Audit::new(pool.clone()),
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

/// Сообщения (не экраны), поставленные в outbox для чата.
async fn outbox_messages(pool: &sqlx::PgPool, chat: i64) -> Vec<Value> {
    sqlx::query_scalar(
        "SELECT payload FROM outbox
         WHERE chat_id = $1 AND kind = 'message'
         ORDER BY seq",
    )
    .bind(chat)
    .fetch_all(pool)
    .await
    .expect("outbox messages")
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
async fn rolled_back_processing_leaves_no_delete_row_and_the_retry_adds_one() {
    let db = db().await;
    ensure_household(&db.pool).await;
    let chat = next_telegram_id();
    let handler = Arc::new(BotThenFail {
        bot: Bot::new("tasklerqbot"),
        failures_left: AtomicU32::new(1),
    });
    let worker = worker(&db.pool, chat, handler, 10);
    let mut bot = Harness::new(&db.pool, chat);
    let update = bot.text_update("Ann", "привет");
    let id = update["update_id"].as_i64().expect("id");
    let t0 = Utc::now().trunc_subsecs(6);
    enqueue_at(&db.pool, &update, t0).await;

    async fn delete_rows(pool: &sqlx::PgPool, chat: i64) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM outbox WHERE chat_id = $1 AND kind = 'delete'")
            .bind(chat)
            .fetch_one(pool)
            .await
            .expect("delete rows")
    }

    // Сбой после обработки: транзакция откатывается вместе со строкой удаления.
    assert!(matches!(
        worker.run_once(t0).await.expect("pass"),
        Pass::Failed(Failure::Retry(_))
    ));
    assert_eq!(
        delete_rows(&db.pool, chat).await,
        0,
        "a rolled back update must not leave a delete row"
    );

    // Успешный повтор ставит удаление ровно один раз.
    let t1 = t0 + Duration::seconds(1);
    assert_eq!(worker.run_once(t1).await.expect("pass"), Pass::Processed);
    assert_eq!(
        delete_rows(&db.pool, chat).await,
        1,
        "the successful retry must delete the message once"
    );
    assert_eq!(status_of(&db.pool, id).await.0, "processed");
    let message_id = update["message"]["message_id"]
        .as_i64()
        .expect("message id");
    let deleted: Option<i64> = sqlx::query_scalar(
        "SELECT (payload->>'message_id')::bigint FROM outbox
         WHERE chat_id = $1 AND kind = 'delete'",
    )
    .bind(chat)
    .fetch_optional(&db.pool)
    .await
    .expect("deleted message");
    assert_eq!(deleted, Some(message_id));
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
async fn dead_letter_sends_one_neutral_message_after_exhausted_attempts() {
    let db = db().await;
    ensure_household(&db.pool).await;
    let chat = next_telegram_id();
    let worker = worker(
        &db.pool,
        chat,
        Arc::new(Flaky {
            failures_left: AtomicU32::new(100),
            marker: format!("notice-{chat}"),
        }),
        3,
    );
    let mut bot = Harness::new(&db.pool, chat);
    let update = bot.text_update("Ann", "секретный текст");
    let mut now = Utc::now();
    enqueue_at(&db.pool, &update, now).await;

    // Повторы пользователю ничего не шлют: сообщение только после dead.
    for _ in 0..2 {
        let Pass::Failed(Failure::Retry(at)) = worker.run_once(now).await.expect("pass") else {
            panic!("retry expected");
        };
        now = at;
        assert!(
            outbox_messages(&db.pool, chat).await.is_empty(),
            "a retry must not notify the user"
        );
    }

    assert_eq!(
        worker.run_once(now).await.expect("pass"),
        Pass::Failed(Failure::Dead)
    );
    let messages = outbox_messages(&db.pool, chat).await;
    assert_eq!(
        messages.len(),
        1,
        "a dead update must notify the user exactly once"
    );
    assert_eq!(
        messages[0]["html"],
        "Не получилось выполнить действие. Попробуйте ещё раз чуть позже."
    );
    assert_eq!(
        messages[0]["rich"], false,
        "the notice must not be a screen"
    );
    assert!(
        !messages[0].to_string().contains("секретный текст"),
        "the notice must not repeat user text"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn panicking_handler_also_notifies_user_after_dead_letter() {
    let db = db().await;
    ensure_household(&db.pool).await;
    let chat = next_telegram_id();
    let worker = worker(
        &db.pool,
        chat,
        Arc::new(Panicking {
            marker: format!("panic-notice-{chat}"),
        }),
        2,
    );
    let mut bot = Harness::new(&db.pool, chat);
    let update = bot.text_update("Ann", "привет");
    let mut now = Utc::now();
    enqueue_at(&db.pool, &update, now).await;

    let Pass::Failed(Failure::Retry(at)) = worker.run_once(now).await.expect("pass") else {
        panic!("retry expected");
    };
    now = at;
    assert!(outbox_messages(&db.pool, chat).await.is_empty());

    assert_eq!(
        worker.run_once(now).await.expect("pass"),
        Pass::Failed(Failure::Dead)
    );
    let messages = outbox_messages(&db.pool, chat).await;
    assert_eq!(messages.len(), 1);
    assert_eq!(
        messages[0]["html"],
        "Не получилось выполнить действие. Попробуйте ещё раз чуть позже."
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeated_dead_processing_does_not_send_a_second_message() {
    let db = db().await;
    ensure_household(&db.pool).await;
    let chat = next_telegram_id();
    let worker = worker(
        &db.pool,
        chat,
        Arc::new(Flaky {
            failures_left: AtomicU32::new(100),
            marker: format!("dedupe-{chat}"),
        }),
        2,
    );
    let mut bot = Harness::new(&db.pool, chat);
    let update = bot.text_update("Ann", "привет");
    let id = update["update_id"].as_i64().expect("id");
    let mut now = Utc::now();
    enqueue_at(&db.pool, &update, now).await;

    let Pass::Failed(Failure::Retry(at)) = worker.run_once(now).await.expect("pass") else {
        panic!("retry expected");
    };
    now = at;
    assert_eq!(
        worker.run_once(now).await.expect("pass"),
        Pass::Failed(Failure::Dead)
    );
    assert_eq!(outbox_messages(&db.pool, chat).await.len(), 1);

    // Обновление доставлено повторно после сбоя: та же попытка снова уходит в
    // dead-letter, но `dedupe_key` не даёт поставить второе сообщение.
    sqlx::query(
        "UPDATE telegram_updates
         SET status = 'pending', attempts = 1, payload = $2, available_at = $3
         WHERE update_id = $1",
    )
    .bind(id)
    .bind(&update)
    .bind(now + Duration::seconds(1))
    .execute(&db.pool)
    .await
    .expect("restore update");

    assert_eq!(
        worker
            .run_once(now + Duration::seconds(1))
            .await
            .expect("pass"),
        Pass::Failed(Failure::Dead)
    );
    assert_eq!(
        outbox_messages(&db.pool, chat).await.len(),
        1,
        "a repeated dead update must not duplicate the notice"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_callback_attempt_is_answered_without_text() {
    let db = db().await;
    ensure_household(&db.pool).await;
    let chat = next_telegram_id();
    let fake = FakeTelegram::new();
    let worker = UpdateWorker::new(
        db.pool.clone(),
        Arc::new(Flaky {
            failures_left: AtomicU32::new(100),
            marker: format!("callback-{chat}"),
        }),
        fake.clone(),
        10,
        Arc::new(Notify::new()),
        Audit::new(db.pool.clone()),
    )
    .only_chat(chat);
    let mut bot = Harness::new(&db.pool, chat);
    let update = bot.callback_update("Ann", "t:AAAAAAAAAAAAAAAA", 1);
    let query_id = update["callback_query"]["id"]
        .as_str()
        .expect("query id")
        .to_owned();
    let now = Utc::now();
    enqueue_at(&db.pool, &update, now).await;

    let Pass::Failed(Failure::Retry(at)) = worker.run_once(now).await.expect("pass") else {
        panic!("retry expected");
    };
    assert_eq!(
        fake.answers(),
        vec![(query_id.clone(), None, false)],
        "a failed callback must be answered without text or alert"
    );

    // Повторная неудачная попытка уже не отвечает: Telegram отклонил бы ответ.
    assert!(matches!(
        worker.run_once(at).await.expect("pass"),
        Pass::Failed(Failure::Retry(_))
    ));
    assert_eq!(answers_for(&fake, &query_id), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dead_callback_is_answered_once_and_gets_one_neutral_message() {
    let db = db().await;
    ensure_household(&db.pool).await;
    let chat = next_telegram_id();
    let fake = FakeTelegram::new();
    let worker = UpdateWorker::new(
        db.pool.clone(),
        Arc::new(Flaky {
            failures_left: AtomicU32::new(100),
            marker: format!("dead-callback-{chat}"),
        }),
        fake.clone(),
        2,
        Arc::new(Notify::new()),
        Audit::new(db.pool.clone()),
    )
    .only_chat(chat);
    let mut bot = Harness::new(&db.pool, chat);
    let update = bot.callback_update("Ann", "t:AAAAAAAAAAAAAAAA", 1);
    let query_id = update["callback_query"]["id"]
        .as_str()
        .expect("query id")
        .to_owned();
    let mut now = Utc::now();
    enqueue_at(&db.pool, &update, now).await;

    let Pass::Failed(Failure::Retry(at)) = worker.run_once(now).await.expect("pass") else {
        panic!("retry expected");
    };
    now = at;
    assert_eq!(
        worker.run_once(now).await.expect("pass"),
        Pass::Failed(Failure::Dead)
    );

    assert_eq!(
        answers_for(&fake, &query_id),
        1,
        "only the first failed attempt may answer the callback"
    );
    let messages = outbox_messages(&db.pool, chat).await;
    assert_eq!(
        messages.len(),
        1,
        "a dead callback must notify the user exactly once"
    );
    assert_eq!(
        messages[0]["html"],
        "Не получилось выполнить действие. Попробуйте ещё раз чуть позже."
    );
    assert_eq!(messages[0]["rich"], false);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dead_letter_without_chat_or_with_malformed_payload_sends_no_message() {
    let db = support::isolated_db().await;
    let chat = next_telegram_id();
    let now = Utc::now();
    let worker = UpdateWorker::new(
        db.pool.clone(),
        Arc::new(Flaky {
            failures_left: AtomicU32::new(100),
            marker: format!("no-chat-{chat}"),
        }),
        FakeTelegram::new(),
        1,
        Arc::new(Notify::new()),
        Audit::new(db.pool.clone()),
    );

    // Нажатие без сообщения: в payload нет чата.
    let mut bot = Harness::new(&db.pool, chat);
    let mut callback = bot.callback_update("Ann", "t:AAAAAAAAAAAAAAAA", 1);
    callback["callback_query"]
        .as_object_mut()
        .expect("callback object")
        .remove("message");
    let callback_id = callback["update_id"].as_i64().expect("id");
    let mut conn = db.pool.acquire().await.expect("conn");
    queue::enqueue(&mut conn, callback_id, &callback, now)
        .await
        .expect("enqueue");
    assert_eq!(
        worker.run_once(now).await.expect("pass"),
        Pass::Failed(Failure::Dead)
    );

    // Неразбираемое обновление: чат из него не извлечь.
    let malformed = serde_json::json!({
        "update_id": next_telegram_id(),
        "message": "not an update",
    });
    let malformed_id = malformed["update_id"].as_i64().expect("id");
    let mut conn = db.pool.acquire().await.expect("conn");
    queue::enqueue(&mut conn, malformed_id, &malformed, now)
        .await
        .expect("enqueue");
    assert_eq!(
        worker.run_once(now).await.expect("pass"),
        Pass::Failed(Failure::Dead)
    );

    let messages: i64 = sqlx::query_scalar("SELECT count(*) FROM outbox WHERE chat_id = $1")
        .bind(chat)
        .fetch_one(&db.pool)
        .await
        .expect("outbox count");
    assert_eq!(
        messages, 0,
        "an update without a chat cannot be answered with a notice"
    );
    let all: i64 = sqlx::query_scalar("SELECT count(*) FROM outbox")
        .fetch_one(&db.pool)
        .await
        .expect("outbox count");
    assert_eq!(all, 0);
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

/// Обработчик, запоминающий порядок выполненных обновлений.
#[derive(Default)]
struct Ordered {
    order: std::sync::Mutex<Vec<i64>>,
}

#[async_trait]
impl UpdateHandler for Ordered {
    async fn handle(
        &self,
        _conn: &mut PgConnection,
        payload: &Value,
        _now: DateTime<Utc>,
    ) -> Result<Outcome, sqlx::Error> {
        let update_id = payload["update_id"].as_i64().unwrap_or_default();
        self.order.lock().expect("order").push(update_id);
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

fn worker_with_limit(
    pool: &sqlx::PgPool,
    chat: i64,
    handler: Arc<dyn UpdateHandler>,
    fake: Arc<FakeTelegram>,
    per_minute: u32,
) -> UpdateWorker {
    UpdateWorker::new(
        pool.clone(),
        handler,
        fake,
        10,
        Arc::new(Notify::new()),
        Audit::new(pool.clone()),
    )
    .only_chat(chat)
    .with_rate_limit(per_minute)
}

fn limited_worker(
    pool: &sqlx::PgPool,
    chat: i64,
    handler: Arc<dyn UpdateHandler>,
    fake: Arc<FakeTelegram>,
) -> UpdateWorker {
    worker_with_limit(pool, chat, handler, fake, UPDATES_PER_MINUTE)
}

async fn available_at(pool: &sqlx::PgPool, update_id: i64) -> DateTime<Utc> {
    sqlx::query_scalar("SELECT available_at FROM telegram_updates WHERE update_id = $1")
        .bind(update_id)
        .fetch_one(pool)
        .await
        .expect("available_at")
}

async fn owner_chat(pool: &sqlx::PgPool, owner_id: uuid::Uuid) -> i64 {
    sqlx::query_scalar("SELECT telegram_user_id FROM users WHERE id = $1")
        .bind(owner_id)
        .fetch_one(pool)
        .await
        .expect("owner chat")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_twenty_first_message_is_deferred_until_the_slot_frees() {
    let db = db().await;
    let home = support::create_household(&db.pool).await;
    let chat = owner_chat(&db.pool, home.owner_id).await;
    let fake = FakeTelegram::new();
    let worker = worker_with_limit(
        &db.pool,
        chat,
        Arc::new(Bot::new("tasklerqbot")),
        fake.clone(),
        UPDATES_PER_MINUTE,
    );
    let mut bot = Harness::new(&db.pool, chat);
    let now = Utc::now().trunc_subsecs(6);

    let mut ids = Vec::new();
    for _ in 0..21 {
        let update = bot.text_update("Ann", "привет");
        ids.push(enqueue_at(&db.pool, &update, now).await);
    }

    for _ in 0..20 {
        assert_eq!(worker.run_once(now).await.expect("pass"), Pass::Processed);
    }
    let before: i64 =
        sqlx::query_scalar("SELECT coalesce(max(seq), 0) FROM outbox WHERE chat_id = $1")
            .bind(chat)
            .fetch_one(&db.pool)
            .await
            .expect("max seq");

    assert_eq!(
        worker.run_once(now).await.expect("pass"),
        Pass::Deferred,
        "the 21st update must wait for the slot, not be processed"
    );

    let rows: Vec<(String, Option<i64>)> = sqlx::query_as(
        "SELECT kind::text, coalesce((payload->>'message_id')::bigint, message_id)
         FROM outbox WHERE chat_id = $1 AND seq > $2 ORDER BY seq",
    )
    .bind(chat)
    .bind(before)
    .fetch_all(&db.pool)
    .await
    .expect("outbox rows");
    assert!(
        rows.is_empty(),
        "a deferred update must not write a message, screen or delete: {rows:?}"
    );
    assert_eq!(
        status_of(&db.pool, ids[20]).await,
        ("pending".into(), 0, None),
        "a deferred update stays pending without a failed attempt"
    );
    let retry_at = now + Duration::seconds(60);
    assert_eq!(available_at(&db.pool, ids[20]).await, retry_at);

    // В момент освобождения слота обновление выполняется как обычно и удаляет
    // своё входящее сообщение.
    assert_eq!(
        worker.run_once(retry_at).await.expect("pass"),
        Pass::Processed
    );
    assert_eq!(status_of(&db.pool, ids[20]).await.0, "processed");
    let deleted: Option<i64> = sqlx::query_scalar(
        "SELECT (payload->>'message_id')::bigint FROM outbox
         WHERE chat_id = $1 AND kind = 'delete' AND (payload->>'message_id')::bigint = $2",
    )
    .bind(chat)
    .bind(ids[20])
    .fetch_optional(&db.pool)
    .await
    .expect("deleted message");
    assert_eq!(deleted, Some(ids[20]));
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
        Audit::new(db.pool.clone()),
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

    // Третье новое действие уже сверх лимита: оно откладывается, а не
    // выполняется и не теряется.
    let third = bot.text_update("Ann", "привет");
    enqueue_at(&db.pool, &third, retry_at).await;
    assert_eq!(
        worker.run_once(retry_at).await.expect("pass"),
        Pass::Deferred
    );
    assert_eq!(
        recorded(&db.pool, &prefix).await,
        2,
        "a retry must not consume a rate limit slot"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_twenty_first_callback_is_answered_immediately_and_runs_after_the_slot_frees() {
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

    let mut ids = Vec::new();
    for _ in 0..21 {
        let update = bot.callback_update("Ann", "t:AAAAAAAAAAAAAAAA", 1);
        ids.push(enqueue_at(&db.pool, &update, now).await);
    }

    for _ in 0..20 {
        assert_eq!(worker.run_once(now).await.expect("pass"), Pass::Processed);
    }
    assert_eq!(recorded(&db.pool, &prefix).await, 20);

    assert_eq!(worker.run_once(now).await.expect("pass"), Pass::Deferred);
    assert_eq!(
        recorded(&db.pool, &prefix).await,
        20,
        "the 21st callback must not change data yet"
    );

    // «Часики» убирает пустой ответ без текста и без окна.
    let answers = fake.answers();
    assert_eq!(
        answers.len(),
        1,
        "the deferred callback must be answered immediately: {answers:?}"
    );
    assert_eq!(answers[0].1, None);
    assert!(!answers[0].2);
    assert_eq!(
        status_of(&db.pool, ids[20]).await,
        ("pending".into(), 0, None)
    );

    // Действие выполняется после освобождения слота.
    let retry_at = now + Duration::seconds(60);
    assert_eq!(
        worker.run_once(retry_at).await.expect("pass"),
        Pass::Processed
    );
    assert_eq!(recorded(&db.pool, &prefix).await, 21);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn three_deferred_form_answers_run_in_update_id_order() {
    let db = db().await;
    let home = support::create_household(&db.pool).await;
    let chat = owner_chat(&db.pool, home.owner_id).await;
    let fake = FakeTelegram::new();
    let mut bot = Harness::with_handler(
        &db.pool,
        chat,
        Arc::new(Bot::new("tasklerqbot")),
        fake.clone(),
    );
    bot.text("/menu").await;
    bot.press("Задачи").await;
    bot.press("Добавить").await;
    bot.text("План").await;
    bot.press("Подзадачи").await;
    bot.assert_screen_contains("Подзадачи");
    let active_id = bot.active().0;

    let worker = worker_with_limit(&db.pool, chat, Arc::new(Bot::new("tasklerqbot")), fake, 3);
    let t0 = (bot.now + Duration::seconds(1)).trunc_subsecs(6);
    let name = bot.name.clone();

    // Три безвредных callback занимают все три слота.
    for _ in 0..3 {
        let update = bot.callback_update(&name, "t:AAAAAAAAAAAAAAAA", active_id);
        enqueue_at(&db.pool, &update, t0).await;
    }
    for _ in 0..3 {
        assert_eq!(worker.run_once(t0).await.expect("pass"), Pass::Processed);
    }

    let mut ids = Vec::new();
    for text in ["Первая", "Вторая", "Третья"] {
        let update = bot.text_update(&name, text);
        ids.push(enqueue_at(&db.pool, &update, t0).await);
    }
    for id in &ids {
        assert_eq!(worker.run_once(t0).await.expect("pass"), Pass::Deferred);
        assert_eq!(status_of(&db.pool, *id).await, ("pending".into(), 0, None));
        assert_eq!(
            available_at(&db.pool, *id).await,
            t0 + Duration::seconds(60)
        );
    }

    let retry_at = t0 + Duration::seconds(60);
    for _ in 0..3 {
        assert_eq!(
            worker.run_once(retry_at).await.expect("pass"),
            Pass::Processed
        );
    }

    // Итоговое состояние формы соответствует порядку ответов.
    let data: Value = sqlx::query_scalar("SELECT data FROM conversation_states WHERE chat_id = $1")
        .bind(chat)
        .fetch_one(&db.pool)
        .await
        .expect("form state");
    let Ok(Form::Editor(editor)) = serde_json::from_value::<Form>(data) else {
        panic!("the editor should still be open");
    };
    assert_eq!(editor.draft.title, "План");
    let subtasks: Vec<&str> = editor
        .draft
        .subtasks
        .iter()
        .map(|subtask| subtask.title.as_str())
        .collect();
    assert_eq!(subtasks, ["Первая", "Вторая", "Третья"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_update_arriving_while_deferred_queues_behind_it() {
    let db = db().await;
    let chat = next_telegram_id();
    let fake = FakeTelegram::new();
    let handler = Arc::new(Ordered::default());
    let worker = limited_worker(&db.pool, chat, handler.clone(), fake);
    let mut bot = Harness::new(&db.pool, chat);
    let now = Utc::now().trunc_subsecs(6);

    for _ in 0..UPDATES_PER_MINUTE {
        let update = bot.text_update("Ann", "привет");
        enqueue_at(&db.pool, &update, now).await;
    }
    for _ in 0..UPDATES_PER_MINUTE {
        assert_eq!(worker.run_once(now).await.expect("pass"), Pass::Processed);
    }
    handler.order.lock().expect("order").clear();

    let first = bot.text_update("Ann", "первое");
    let first_id = enqueue_at(&db.pool, &first, now).await;
    assert_eq!(worker.run_once(now).await.expect("pass"), Pass::Deferred);

    // Новое обновление приходит во время ожидания и упирается в тот же слот.
    let later = now + Duration::seconds(30);
    let second = bot.text_update("Ann", "второе");
    let second_id = enqueue_at(&db.pool, &second, later).await;
    assert!(second_id > first_id);
    assert_eq!(worker.run_once(later).await.expect("pass"), Pass::Deferred);

    let retry_at = now + Duration::seconds(60);
    assert_eq!(available_at(&db.pool, first_id).await, retry_at);
    assert_eq!(available_at(&db.pool, second_id).await, retry_at);

    for _ in 0..2 {
        assert_eq!(
            worker.run_once(retry_at).await.expect("pass"),
            Pass::Processed
        );
    }
    assert_eq!(
        *handler.order.lock().expect("order"),
        vec![first_id, second_id],
        "the new update must queue behind the earlier deferred one"
    );
}

/// Сколько ответов на конкретный callback отправил фейковый Bot API.
fn answers_for(fake: &FakeTelegram, query_id: &str) -> usize {
    fake.answers()
        .into_iter()
        .filter(|(answered, _, _)| answered == query_id)
        .count()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_forty_first_message_is_dropped_when_the_deferred_queue_is_full() {
    let db = db().await;
    let home = support::create_household(&db.pool).await;
    let chat = owner_chat(&db.pool, home.owner_id).await;
    let fake = FakeTelegram::new();
    let worker = limited_worker(
        &db.pool,
        chat,
        Arc::new(Bot::new("tasklerqbot")),
        fake.clone(),
    );
    let mut bot = Harness::new(&db.pool, chat);
    let now = Utc::now().trunc_subsecs(6);

    let mut ids = Vec::new();
    for _ in 0..41 {
        let update = bot.text_update("Ann", "привет");
        ids.push(enqueue_at(&db.pool, &update, now).await);
    }

    // Первые 20 сообщений выполняются сразу.
    for _ in 0..UPDATES_PER_MINUTE {
        assert_eq!(worker.run_once(now).await.expect("pass"), Pass::Processed);
    }
    // Следующие 20 откладываются до освобождения слотов.
    for id in &ids[20..40] {
        assert_eq!(worker.run_once(now).await.expect("pass"), Pass::Deferred);
        assert_eq!(status_of(&db.pool, *id).await, ("pending".into(), 0, None));
        assert_eq!(
            available_at(&db.pool, *id).await,
            now + Duration::seconds(60)
        );
    }

    let before: i64 =
        sqlx::query_scalar("SELECT coalesce(max(seq), 0) FROM outbox WHERE chat_id = $1")
            .bind(chat)
            .fetch_one(&db.pool)
            .await
            .expect("max seq");

    // Очередь отложенных заполнена: 41-е сообщение отбрасывается.
    let dropped = ids[40];
    assert_eq!(
        worker.run_once(now).await.expect("pass"),
        Pass::Dropped,
        "the 41st message must be dropped, not deferred"
    );
    assert_eq!(
        status_of(&db.pool, dropped).await,
        ("processed".into(), 0, None),
        "a dropped update is marked processed without a failed attempt"
    );
    let rows: Vec<(String, Option<i64>)> = sqlx::query_as(
        "SELECT kind::text, coalesce((payload->>'message_id')::bigint, message_id)
         FROM outbox WHERE chat_id = $1 AND seq > $2 ORDER BY seq",
    )
    .bind(chat)
    .bind(before)
    .fetch_all(&db.pool)
    .await
    .expect("outbox rows");
    assert!(
        rows.is_empty(),
        "a dropped update must not write a message, screen or delete: {rows:?}"
    );
    let deleted: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM outbox
         WHERE chat_id = $1 AND kind = 'delete' AND (payload->>'message_id')::bigint = $2",
    )
    .bind(chat)
    .bind(dropped)
    .fetch_one(&db.pool)
    .await
    .expect("delete rows");
    assert_eq!(deleted, 0, "the dropped message must stay in the chat");
    let buried: bool = sqlx::query_scalar("SELECT buried FROM telegram_views WHERE chat_id = $1")
        .bind(chat)
        .fetch_one(&db.pool)
        .await
        .expect("view");
    assert!(
        buried,
        "a dropped text leaves its message in the chat, so the view is buried"
    );

    // Освободившиеся слоты выполняют оставшиеся 20 обновлений по `update_id`.
    let retry_at = now + Duration::seconds(60);
    for _ in 0..UPDATES_PER_MINUTE {
        assert_eq!(
            worker.run_once(retry_at).await.expect("pass"),
            Pass::Processed
        );
    }
    let deleted_ids: Vec<i64> = sqlx::query_scalar(
        "SELECT (payload->>'message_id')::bigint FROM outbox
         WHERE chat_id = $1 AND kind = 'delete' AND seq > $2 ORDER BY seq",
    )
    .bind(chat)
    .bind(before)
    .fetch_all(&db.pool)
    .await
    .expect("deleted ids");
    assert_eq!(
        deleted_ids,
        ids[20..40],
        "the deferred updates must run in update_id order"
    );
    for id in &ids[..40] {
        assert_eq!(status_of(&db.pool, *id).await.0, "processed");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_already_deferred_update_is_not_dropped_when_newer_updates_fill_the_queue() {
    let db = db().await;
    ensure_household(&db.pool).await;
    let chat = next_telegram_id();
    let fake = FakeTelegram::new();
    let handler = Arc::new(Ordered::default());
    let worker = worker_with_limit(&db.pool, chat, handler.clone(), fake, 1);
    let mut bot = Harness::new(&db.pool, chat);
    let t0 = Utc::now().trunc_subsecs(6);

    // Единственный слот занят.
    let filler = bot.text_update("Ann", "слот");
    enqueue_at(&db.pool, &filler, t0).await;
    assert_eq!(worker.run_once(t0).await.expect("pass"), Pass::Processed);
    handler.order.lock().expect("order").clear();

    // Старое обновление уже отложено лимитером и ждёт t0+30.
    let old = bot.text_update("Ann", "старое");
    let old_id = enqueue_at(&db.pool, &old, t0).await;
    sqlx::query("UPDATE telegram_updates SET available_at = $2 WHERE update_id = $1")
        .bind(old_id)
        .bind(t0 + Duration::seconds(30))
        .execute(&db.pool)
        .await
        .expect("defer old");

    // Двадцать более новых отложенных ждут позже: за время ожидания очередь
    // заполнилась.
    let mut newer = Vec::new();
    for _ in 0..DEFERRED_QUEUE_LIMIT {
        let update = bot.text_update("Ann", "новое");
        let id = enqueue_at(&db.pool, &update, t0 + Duration::seconds(10)).await;
        sqlx::query("UPDATE telegram_updates SET available_at = $2 WHERE update_id = $1")
            .bind(id)
            .bind(t0 + Duration::seconds(130))
            .execute(&db.pool)
            .await
            .expect("defer new");
        newer.push(id);
    }

    // Повторный отказ лимитера не отбрасывает старое обновление, хотя в
    // очереди уже двадцать более новых.
    assert_eq!(
        worker
            .run_once(t0 + Duration::seconds(30))
            .await
            .expect("pass"),
        Pass::Deferred,
        "an already deferred update must be deferred again, not dropped"
    );
    assert_eq!(
        status_of(&db.pool, old_id).await,
        ("pending".into(), 0, None)
    );
    assert_eq!(
        available_at(&db.pool, old_id).await,
        t0 + Duration::seconds(60)
    );

    // Оно выполняется раньше более новых, когда освобождается слот.
    assert_eq!(
        worker
            .run_once(t0 + Duration::seconds(60))
            .await
            .expect("pass"),
        Pass::Processed
    );
    assert_eq!(*handler.order.lock().expect("order"), vec![old_id]);
    assert_eq!(
        status_of(&db.pool, newer[0]).await.0,
        "pending",
        "newer deferred updates must still be waiting"
    );

    assert_eq!(
        worker
            .run_once(t0 + Duration::seconds(130))
            .await
            .expect("pass"),
        Pass::Processed
    );
    assert_eq!(
        *handler.order.lock().expect("order"),
        vec![old_id, newer[0]],
        "the older deferred update must run before the newer ones"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_row_waiting_for_a_retry_does_not_take_a_deferred_queue_slot() {
    let db = db().await;
    ensure_household(&db.pool).await;
    let chat = next_telegram_id();
    let fake = FakeTelegram::new();
    let worker = worker_with_limit(
        &db.pool,
        chat,
        Arc::new(Recorder {
            prefix: format!("retryslot{chat}"),
        }),
        fake,
        1,
    );
    let mut bot = Harness::new(&db.pool, chat);
    let t0 = Utc::now().trunc_subsecs(6);

    // Слот занят.
    let filler = bot.text_update("Ann", "слот");
    enqueue_at(&db.pool, &filler, t0).await;
    assert_eq!(worker.run_once(t0).await.expect("pass"), Pass::Processed);

    // Двадцать строк ждут повтора после сбоя: их отодвинул бэкофф, а не
    // лимитер, поэтому предел очереди они не занимают.
    let mut conn = db.pool.acquire().await.expect("conn");
    for _ in 0..DEFERRED_QUEUE_LIMIT {
        let update = bot.text_update("Ann", "сбой");
        let id = update["update_id"].as_i64().expect("id");
        queue::enqueue(&mut conn, id, &update, t0)
            .await
            .expect("enqueue");
        queue::record_failure(&mut conn, id, "database", 10, t0)
            .await
            .expect("failure");
    }

    let next = bot.text_update("Ann", "новое");
    let next_id = enqueue_at(&db.pool, &next, t0).await;
    assert_eq!(
        worker.run_once(t0).await.expect("pass"),
        Pass::Deferred,
        "retry rows must not fill the limit for new deferred updates"
    );
    assert_eq!(status_of(&db.pool, next_id).await.0, "pending");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_callback_over_the_deferred_queue_limit_is_dropped_with_an_empty_answer() {
    let db = db().await;
    ensure_household(&db.pool).await;
    let chat = next_telegram_id();
    let fake = FakeTelegram::new();
    let prefix = format!("dropcb{chat}");
    let worker = worker_with_limit(
        &db.pool,
        chat,
        Arc::new(Recorder {
            prefix: prefix.clone(),
        }),
        fake.clone(),
        1,
    );
    let mut bot = Harness::new(&db.pool, chat);
    let now = Utc::now().trunc_subsecs(6);

    // Первое нажатие занимает единственный слот лимита.
    let first = bot.callback_update("Ann", "t:AAAAAAAAAAAAAAAA", 1);
    let first_id = enqueue_at(&db.pool, &first, now).await;
    assert_eq!(worker.run_once(now).await.expect("pass"), Pass::Processed);

    // Очередь отложенных уже заполнена обновлениями того же пользователя.
    for _ in 0..20 {
        let update = bot.callback_update("Ann", "t:BBBBBBBBBBBBBBBB", 1);
        enqueue_at(&db.pool, &update, now + Duration::seconds(30)).await;
    }

    let dropped = bot.callback_update("Ann", "t:CCCCCCCCCCCCCCCC", 1);
    let dropped_id = enqueue_at(&db.pool, &dropped, now).await;
    assert_eq!(
        worker.run_once(now).await.expect("pass"),
        Pass::Dropped,
        "a callback over the deferred queue limit is dropped"
    );
    assert_eq!(
        recorded(&db.pool, &prefix).await,
        1,
        "the dropped callback must not change data"
    );
    let answers = fake.answers();
    assert_eq!(
        answers.len(),
        1,
        "a dropped callback must be answered once to remove the spinner: {answers:?}"
    );
    assert_eq!(answers[0].1, None, "the answer must have no text");
    assert!(!answers[0].2, "the answer must not be an alert");
    assert_eq!(status_of(&db.pool, dropped_id).await.0, "processed");
    assert_eq!(status_of(&db.pool, first_id).await.0, "processed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_repeatedly_deferred_callback_is_answered_once() {
    let db = db().await;
    ensure_household(&db.pool).await;
    let chat = next_telegram_id();
    let fake = FakeTelegram::new();
    let prefix = format!("repeatcb{chat}");
    let worker = worker_with_limit(
        &db.pool,
        chat,
        Arc::new(Recorder {
            prefix: prefix.clone(),
        }),
        fake.clone(),
        2,
    );
    let mut bot = Harness::new(&db.pool, chat);
    let t0 = Utc::now().trunc_subsecs(6);

    // Две отметки в лимите: t0 и t0+10.
    let first = bot.callback_update("Ann", "t:AAAAAAAAAAAAAAAA", 1);
    enqueue_at(&db.pool, &first, t0).await;
    assert_eq!(worker.run_once(t0).await.expect("pass"), Pass::Processed);
    let second = bot.callback_update("Ann", "t:BBBBBBBBBBBBBBBB", 1);
    enqueue_at(&db.pool, &second, t0 + Duration::seconds(10)).await;
    assert_eq!(
        worker
            .run_once(t0 + Duration::seconds(10))
            .await
            .expect("pass"),
        Pass::Processed
    );

    // Два новых нажатия откладываются до t0+60.
    let mut ids = Vec::new();
    for data in ["t:CCCCCCCCCCCCCCCC", "t:DDDDDDDDDDDDDDDD"] {
        let update = bot.callback_update("Ann", data, 1);
        ids.push(enqueue_at(&db.pool, &update, t0 + Duration::seconds(10)).await);
    }
    for id in &ids {
        assert_eq!(
            worker
                .run_once(t0 + Duration::seconds(10))
                .await
                .expect("pass"),
            Pass::Deferred
        );
        let retry_at = t0 + Duration::seconds(60);
        assert_eq!(available_at(&db.pool, *id).await, retry_at);
        assert_eq!(answers_for(&fake, &format!("cb{id}")), 1);
    }

    // Первое выполняется, второе снова упирается в лимит и откладывается до
    // t0+70. Пустой ответ на уже отвеченный query_id не повторяется.
    let retry_at = t0 + Duration::seconds(60);
    assert_eq!(
        worker.run_once(retry_at).await.expect("pass"),
        Pass::Processed
    );
    assert_eq!(
        worker.run_once(retry_at).await.expect("pass"),
        Pass::Deferred
    );
    assert_eq!(
        available_at(&db.pool, ids[1]).await,
        t0 + Duration::seconds(70)
    );
    assert_eq!(
        answers_for(&fake, &format!("cb{}", ids[1])),
        1,
        "a repeated deferral must not answer the same callback again"
    );
    assert_eq!(recorded(&db.pool, &prefix).await, 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn limiter_marks_at_different_times_keep_the_update_order() {
    let db = db().await;
    ensure_household(&db.pool).await;
    let chat = next_telegram_id();
    let fake = FakeTelegram::new();
    let handler = Arc::new(Ordered::default());
    let worker = worker_with_limit(&db.pool, chat, handler.clone(), fake, 2);
    let mut bot = Harness::new(&db.pool, chat);
    let t0 = Utc::now().trunc_subsecs(6);

    // Две отметки в лимите в разное время.
    let first = bot.text_update("Ann", "первое");
    enqueue_at(&db.pool, &first, t0).await;
    assert_eq!(worker.run_once(t0).await.expect("pass"), Pass::Processed);
    let second = bot.text_update("Ann", "второе");
    enqueue_at(&db.pool, &second, t0 + Duration::seconds(10)).await;
    assert_eq!(
        worker
            .run_once(t0 + Duration::seconds(10))
            .await
            .expect("pass"),
        Pass::Processed
    );
    handler.order.lock().expect("order").clear();

    let mut ids = Vec::new();
    for text in ["третье", "четвёртое", "пятое"] {
        let update = bot.text_update("Ann", text);
        ids.push(enqueue_at(&db.pool, &update, t0 + Duration::seconds(10)).await);
    }
    for id in &ids {
        assert_eq!(
            worker
                .run_once(t0 + Duration::seconds(10))
                .await
                .expect("pass"),
            Pass::Deferred
        );
        assert_eq!(
            available_at(&db.pool, *id).await,
            t0 + Duration::seconds(60)
        );
    }

    // На t0+60 освобождается место от самой старой отметки: выполняется первое
    // отложенное обновление, остальные снова ждут.
    let first_retry = t0 + Duration::seconds(60);
    assert_eq!(
        worker.run_once(first_retry).await.expect("pass"),
        Pass::Processed
    );
    for _ in 0..2 {
        assert_eq!(
            worker.run_once(first_retry).await.expect("pass"),
            Pass::Deferred
        );
    }
    assert_eq!(
        available_at(&db.pool, ids[1]).await,
        t0 + Duration::seconds(70)
    );
    assert_eq!(
        available_at(&db.pool, ids[2]).await,
        t0 + Duration::seconds(70)
    );
    assert_eq!(*handler.order.lock().expect("order"), vec![ids[0]]);

    // На t0+70 выполняется второе, третье откладывается до t0+120.
    let second_retry = t0 + Duration::seconds(70);
    assert_eq!(
        worker.run_once(second_retry).await.expect("pass"),
        Pass::Processed
    );
    assert_eq!(
        worker.run_once(second_retry).await.expect("pass"),
        Pass::Deferred
    );
    assert_eq!(
        available_at(&db.pool, ids[2]).await,
        t0 + Duration::seconds(120)
    );
    assert_eq!(*handler.order.lock().expect("order"), vec![ids[0], ids[1]]);

    assert_eq!(
        worker
            .run_once(t0 + Duration::seconds(120))
            .await
            .expect("pass"),
        Pass::Processed
    );
    assert_eq!(
        *handler.order.lock().expect("order"),
        vec![ids[0], ids[1], ids[2]]
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
