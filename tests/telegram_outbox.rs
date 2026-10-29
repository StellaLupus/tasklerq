//! Outbox: порядок, лимиты, `429`, повторы, замена сообщений и дедупликация.

mod support;

use std::time::{Duration as StdDuration, Instant};

use chrono::{DateTime, Duration, Utc};
use sqlx::PgPool;
use support::telegram::{Call, FakeTelegram};
use support::{db, next_telegram_id};
use tasklerq::telegram::api::TelegramError;
use tasklerq::telegram::outbox::{self, MAX_ATTEMPTS, OutboxWorker, Outgoing};
use tasklerq::telegram::views;

struct Rig {
    pool: PgPool,
    fake: std::sync::Arc<FakeTelegram>,
    worker: OutboxWorker,
    chat: i64,
    clock: Instant,
    now: DateTime<Utc>,
}

impl Rig {
    fn new(pool: &PgPool) -> Self {
        let chat = next_telegram_id();
        let fake = FakeTelegram::new();
        Self {
            pool: pool.clone(),
            fake: fake.clone(),
            worker: OutboxWorker::new(pool.clone(), fake).only_chat(chat),
            chat,
            clock: Instant::now(),
            now: Utc::now(),
        }
    }

    async fn screen(&self, html: &str) -> i64 {
        let mut conn = self.pool.acquire().await.expect("conn");
        outbox::enqueue(
            &mut conn,
            self.chat,
            &Outgoing::Screen { html: html.into() },
            None,
            self.now,
        )
        .await
        .expect("enqueue")
        .expect("seq")
    }

    async fn message(&self, html: &str, key: Option<&str>) -> Option<i64> {
        let mut conn = self.pool.acquire().await.expect("conn");
        outbox::enqueue(
            &mut conn,
            self.chat,
            &Outgoing::Message {
                html: html.into(),
                rich: true,
            },
            key,
            self.now,
        )
        .await
        .expect("enqueue")
    }

    async fn pass(&mut self) -> usize {
        self.worker
            .run_once(self.now, self.clock)
            .await
            .expect("pass")
    }

    /// Пройти вперёд на `by` и сделать проход.
    async fn pass_after(&mut self, by: StdDuration) -> usize {
        self.clock += by;
        self.now += Duration::from_std(by).expect("duration");
        self.pass().await
    }

    async fn row(&self, seq: i64) -> (String, i32, Option<String>, Option<i64>) {
        sqlx::query_as(
            "SELECT status::text, attempts, last_error, message_id FROM outbox WHERE seq = $1",
        )
        .bind(seq)
        .fetch_one(&self.pool)
        .await
        .expect("row")
    }
}

const SECOND: StdDuration = StdDuration::from_secs(1);

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn screen_is_sent_once_then_edited_in_place() {
    let db = db().await;
    let mut rig = Rig::new(&db.pool);

    rig.screen("<p>первый</p>").await;
    assert_eq!(rig.pass().await, 1);
    rig.screen("<p>второй</p>").await;
    assert_eq!(rig.pass_after(SECOND).await, 1);

    let calls = rig.fake.calls();
    let Call::SendRich { id: sent_id, .. } = &calls[0] else {
        panic!("send expected: {calls:?}")
    };
    assert!(
        matches!(&calls[1], Call::EditRich { id, .. } if id == sent_id),
        "{calls:?}"
    );
    assert_eq!(rig.fake.messages(rig.chat).len(), 1);
    let mut conn = db.pool.acquire().await.expect("conn");
    let view = views::get(&mut conn, rig.chat)
        .await
        .expect("view")
        .expect("some");
    assert_eq!(view.message_id, Some(rig.fake.messages(rig.chat)[0].0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undeliverable_edit_deletes_old_message_and_sends_exactly_one_new() {
    let db = db().await;
    let mut rig = Rig::new(&db.pool);
    rig.screen("<p>первый</p>").await;
    rig.pass().await;
    let (old_id, _) = rig.fake.messages(rig.chat)[0].clone();
    rig.fake.reject_edits(Some(TelegramError::MessageGone));
    rig.fake.clear_calls();

    rig.screen("<p>второй</p>").await;
    assert_eq!(rig.pass_after(SECOND).await, 1);

    let calls = rig.fake.calls();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert!(matches!(&calls[0], Call::Delete { id, .. } if *id == old_id));
    let Call::SendRich { id: new_id, .. } = &calls[1] else {
        panic!("send expected: {calls:?}")
    };
    assert_ne!(*new_id, old_id);
    assert_eq!(rig.fake.messages(rig.chat).len(), 1);
    let mut conn = db.pool.acquire().await.expect("conn");
    let view = views::get(&mut conn, rig.chat)
        .await
        .expect("view")
        .expect("some");
    assert_eq!(view.message_id, Some(*new_id));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unchanged_content_is_not_an_error() {
    let db = db().await;
    let mut rig = Rig::new(&db.pool);
    rig.screen("<p>то же</p>").await;
    rig.pass().await;

    let seq = rig.screen("<p>то же</p>").await;
    assert_eq!(rig.pass_after(SECOND).await, 1);

    assert_eq!(rig.row(seq).await.0, "sent");
    assert_eq!(rig.fake.messages(rig.chat).len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn too_many_requests_defers_by_retry_after_without_losing_the_message() {
    let db = db().await;
    let mut rig = Rig::new(&db.pool);
    let seq = rig.message("<p>важное</p>", None).await.expect("seq");
    rig.fake.fail_next(TelegramError::RetryAfter(5));

    assert_eq!(rig.pass().await, 0);
    let (status, attempts, error, _) = rig.row(seq).await;
    assert_eq!(
        (status.as_str(), attempts, error.as_deref()),
        ("pending", 0, Some("retry_after"))
    );

    // Раньше срока сообщение не уходит.
    assert_eq!(rig.pass_after(StdDuration::from_secs(4)).await, 0);
    assert!(rig.fake.sent().is_empty());

    // После срока уходит ровно один раз.
    assert_eq!(rig.pass_after(StdDuration::from_secs(2)).await, 1);
    assert_eq!(rig.fake.sent().len(), 1);
    assert_eq!(rig.row(seq).await.0, "sent");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn temporary_failure_backs_off_keeps_order_and_then_dies() {
    let db = db().await;
    let mut rig = Rig::new(&db.pool);
    let head = rig.message("<p>первое</p>", None).await.expect("seq");
    let tail = rig.message("<p>второе</p>", None).await.expect("seq");

    // Пока первое сообщение ждёт повтора, второе его не обгоняет.
    rig.fake.fail_next(TelegramError::Transient("503".into()));
    assert_eq!(rig.pass().await, 0);
    assert_eq!(rig.row(head).await.1, 1);
    assert_eq!(rig.pass_after(SECOND / 2).await, 0);
    assert_eq!(rig.row(tail).await.0, "pending");
    assert!(rig.fake.sent().is_empty());

    // Затем порядок сохраняется: сначала первое, потом второе.
    assert_eq!(rig.pass_after(SECOND * 2).await, 1);
    assert_eq!(rig.pass_after(SECOND * 2).await, 1);
    let order: Vec<String> = rig
        .fake
        .sent()
        .into_iter()
        .map(|call| match call {
            Call::SendRich { html, .. } | Call::Send { html, .. } => html,
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(order, ["<p>первое</p>", "<p>второе</p>"]);

    // Постоянная недоступность: после MAX_ATTEMPTS сообщение в dead-letter.
    let doomed = rig.message("<p>обречено</p>", None).await.expect("seq");
    for _ in 0..MAX_ATTEMPTS {
        rig.fake
            .fail_next(TelegramError::Transient("timeout".into()));
        rig.pass_after(StdDuration::from_secs(400)).await;
    }
    assert_eq!(rig.row(doomed).await.0, "dead");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocked_bot_is_dead_immediately_and_does_not_block_the_chat() {
    let db = db().await;
    let mut rig = Rig::new(&db.pool);
    let first = rig.message("<p>a</p>", None).await.expect("seq");
    let second = rig.message("<p>b</p>", None).await.expect("seq");
    rig.fake
        .fail_next(TelegramError::Forbidden("blocked".into()));

    rig.pass().await;
    assert_eq!(
        rig.row(first).await,
        ("dead".into(), 0, Some("forbidden".into()), None)
    );

    rig.pass_after(SECOND).await;
    assert_eq!(rig.row(second).await.0, "sent");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn per_chat_rate_limit_is_one_message_per_second() {
    let db = db().await;
    let mut rig = Rig::new(&db.pool);
    for n in 0..3 {
        rig.message(&format!("<p>{n}</p>"), None).await;
    }

    assert_eq!(rig.pass().await, 1);
    assert_eq!(rig.pass_after(StdDuration::from_millis(400)).await, 0);
    assert_eq!(rig.pass_after(StdDuration::from_millis(400)).await, 0);
    assert_eq!(rig.pass_after(StdDuration::from_millis(300)).await, 1);
    assert_eq!(rig.pass_after(SECOND).await, 1);
    assert_eq!(rig.fake.sent().len(), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pending_screens_are_collapsed_to_the_latest() {
    let db = db().await;
    let mut rig = Rig::new(&db.pool);
    let first = rig.screen("<p>1</p>").await;
    rig.screen("<p>2</p>").await;
    rig.screen("<p>3</p>").await;

    assert_eq!(rig.pass().await, 1);

    let messages = rig.fake.messages(rig.chat);
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].1, "<p>3</p>");
    assert_eq!(rig.row(first).await.0, "skipped");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dedupe_key_prevents_duplicates_and_sent_payload_is_redacted() {
    let db = db().await;
    let mut rig = Rig::new(&db.pool);
    let key = format!("test:{}", rig.chat);

    let first = rig
        .message("<p>https://t.me/bot?start=SECRETCODE</p>", Some(&key))
        .await;
    let second = rig.message("<p>другое</p>", Some(&key)).await;
    assert!(first.is_some());
    assert!(second.is_none());

    rig.pass().await;

    let payload: String = sqlx::query_scalar("SELECT payload::text FROM outbox WHERE seq = $1")
        .bind(first.expect("seq"))
        .fetch_one(&db.pool)
        .await
        .expect("payload");
    assert!(
        !payload.contains("SECRETCODE"),
        "delivered content must not stay in the outbox: {payload}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scheduled_message_waits_without_blocking_later_ones() {
    let db = db().await;
    let mut rig = Rig::new(&db.pool);
    let later = rig.message("<p>позже</p>", None).await.expect("seq");
    sqlx::query("UPDATE outbox SET next_attempt_at = $2 WHERE seq = $1")
        .bind(later)
        .bind(rig.now + Duration::hours(1))
        .execute(&db.pool)
        .await
        .expect("schedule");
    let now_message = rig.message("<p>сейчас</p>", None).await.expect("seq");

    assert_eq!(rig.pass().await, 1);
    assert_eq!(rig.row(now_message).await.0, "sent");
    assert_eq!(rig.row(later).await.0, "pending");

    assert_eq!(rig.pass_after(StdDuration::from_secs(3_601)).await, 1);
    assert_eq!(rig.row(later).await.0, "sent");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn skipped_and_dead_messages_do_not_keep_their_text() {
    let db = db().await;
    let mut rig = Rig::new(&db.pool);

    // Экран, замещённый новым, пропускается.
    let replaced = rig.screen("<p>SKIPPEDSECRET</p>").await;
    rig.screen("<p>актуальный</p>").await;
    assert_eq!(rig.row(replaced).await.0, "skipped");

    // Сообщение, отклонённое Telegram, становится мёртвым.
    let doomed = rig
        .message("<p>https://t.me/bot?start=DEADSECRET</p>", None)
        .await
        .expect("seq");
    rig.pass().await;
    rig.fake
        .fail_next(TelegramError::Forbidden("blocked".into()));
    rig.pass_after(SECOND).await;
    assert_eq!(rig.row(doomed).await.0, "dead");

    for (seq, secret) in [(replaced, "SKIPPEDSECRET"), (doomed, "DEADSECRET")] {
        let payload: String = sqlx::query_scalar("SELECT payload::text FROM outbox WHERE seq = $1")
            .bind(seq)
            .fetch_one(&db.pool)
            .await
            .expect("payload");
        assert!(
            !payload.contains(secret),
            "{secret} must be wiped from the outbox: {payload}"
        );
    }
}
