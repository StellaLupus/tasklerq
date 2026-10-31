//! Outbox-доставка уведомлений: повторы, dead-letter и отсутствие дублей.
//!
//! Здесь проверяется именно уведомление, поставленное планировщиком: запись
//! `notification_deliveries`, её `outbox_seq` и связь с `message_id`
//! доставленного сообщения.

mod support;

use std::sync::Arc;
use std::time::{Duration as StdDuration, Instant};

use chrono::{DateTime, Duration, Timelike, Utc};
use sqlx::PgPool;
use support::telegram::FakeTelegram;
use support::{TestDb, TestHousehold};
use tasklerq::jobs::scheduler::Scheduler;
use tasklerq::telegram::api::TelegramError;
use tasklerq::telegram::outbox::{self, MAX_ATTEMPTS, OutboxWorker, Outgoing};
use tokio::sync::Notify;
use uuid::Uuid;

const ZERO: StdDuration = StdDuration::from_secs(0);
const SECOND: StdDuration = StdDuration::from_secs(1);

fn scheduler(pool: &PgPool) -> Scheduler {
    Scheduler::new(pool.clone(), Arc::new(Notify::new()))
}

fn rounded_now() -> DateTime<Utc> {
    Utc::now()
        .with_second(0)
        .and_then(|at| at.with_nanosecond(0))
        .expect("rounded time")
}

async fn household_without_extra_notifications(db: &TestDb) -> TestHousehold {
    let household = support::create_household(&db.pool).await;
    for user_id in [household.owner_id, household.member_id] {
        sqlx::query(
            "UPDATE users
             SET settings = jsonb_build_object(
                 'notifications',
                 jsonb_build_object(
                     'digest_enabled', false,
                     'quiet_hours_enabled', false,
                     'reminders_enabled', true
                 )
             )
             WHERE id = $1",
        )
        .bind(user_id)
        .execute(&db.pool)
        .await
        .expect("notification settings");
    }
    household
}

async fn create_reminder_task(
    db: &TestDb,
    household: &TestHousehold,
    title: &str,
    due_at: DateTime<Utc>,
) {
    let mut item = support::new_item(household.tasks_list_id, title);
    item.due_at = Some(due_at);
    item.assignee_user_id = Some(household.owner_id);
    support::create_item(
        &db.pool,
        household,
        household.tasks_list_id,
        &household.owner(),
        item,
    )
    .await;
}

#[derive(Debug, sqlx::FromRow)]
struct OutboxRow {
    status: String,
    attempts: i32,
    last_error: Option<String>,
    message_id: Option<i64>,
    next_attempt_at: DateTime<Utc>,
    dead_at: Option<DateTime<Utc>>,
}

struct Rig {
    pool: PgPool,
    fake: Arc<FakeTelegram>,
    worker: OutboxWorker,
    chat: i64,
    now: DateTime<Utc>,
    clock: Instant,
    delivery_id: Uuid,
    seq: i64,
}

impl Rig {
    /// Создать напоминание, поставить его в outbox планировщиком и подготовить
    /// воркер доставки для одного чата.
    async fn new(db: &TestDb, title: &str) -> Self {
        let household = household_without_extra_notifications(db).await;
        let due = rounded_now() + Duration::hours(3);
        let now = due - Duration::minutes(15);
        create_reminder_task(db, &household, title, due).await;

        let stats = scheduler(&db.pool).tick(now).await.expect("scheduler tick");
        assert_eq!(stats.delivered, 1, "reminder must be queued");

        let (delivery_id, seq): (Uuid, i64) = sqlx::query_as(
            "SELECT d.id, d.outbox_seq
             FROM notification_deliveries d
             JOIN items i ON i.id = d.item_id
             WHERE d.household_id = $1 AND d.kind = 'reminder' AND i.due_at = $2",
        )
        .bind(household.household_id)
        .bind(due)
        .fetch_one(&db.pool)
        .await
        .expect("notification delivery");

        let chat: i64 = sqlx::query_scalar("SELECT telegram_chat_id FROM users WHERE id = $1")
            .bind(household.owner_id)
            .fetch_one(&db.pool)
            .await
            .expect("chat id");
        let fake = FakeTelegram::new();
        let worker = OutboxWorker::new(db.pool.clone(), fake.clone()).only_chat(chat);

        Self {
            pool: db.pool.clone(),
            fake,
            worker,
            chat,
            now,
            clock: Instant::now(),
            delivery_id,
            seq,
        }
    }

    async fn row(&self) -> OutboxRow {
        sqlx::query_as(
            "SELECT status::text, attempts, last_error, message_id, next_attempt_at, dead_at
             FROM outbox WHERE seq = $1",
        )
        .bind(self.seq)
        .fetch_one(&self.pool)
        .await
        .expect("outbox row")
    }

    async fn delivery(&self) -> (String, Option<i64>, Option<i64>) {
        sqlx::query_as(
            "SELECT d.status::text, d.outbox_seq, o.message_id
             FROM notification_deliveries d
             LEFT JOIN outbox o ON o.seq = d.outbox_seq
             WHERE d.id = $1",
        )
        .bind(self.delivery_id)
        .fetch_one(&self.pool)
        .await
        .expect("notification delivery")
    }

    async fn outbox_rows(&self) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM outbox WHERE chat_id = $1")
            .bind(self.chat)
            .fetch_one(&self.pool)
            .await
            .expect("outbox count")
    }

    async fn pass(&mut self, by: StdDuration) -> usize {
        self.clock += by;
        self.now += Duration::from_std(by).expect("duration");
        self.worker
            .run_once(self.now, self.clock)
            .await
            .expect("outbox pass")
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transient_error_backs_off_exponentially_then_delivers_once() {
    let db = support::isolated_db().await;
    let mut rig = Rig::new(&db, "Позвонить врачу").await;
    let first_attempt_at = rig.now;

    rig.fake
        .fail_next(TelegramError::Transient("timeout".into()));
    assert_eq!(rig.pass(ZERO).await, 0);

    let row = rig.row().await;
    assert_eq!(row.status, "pending");
    assert_eq!(row.attempts, 1);
    assert_eq!(row.last_error.as_deref(), Some("transient"));
    assert_eq!(row.next_attempt_at, first_attempt_at + Duration::seconds(1));
    assert!(row.dead_at.is_none());
    assert!(rig.fake.sent().is_empty());

    // До сдвинутого времени повтор не выполняется.
    assert_eq!(rig.pass(StdDuration::from_millis(500)).await, 0);
    assert!(rig.fake.sent().is_empty());

    // Вторая ошибка сдвигает повтор уже на 2 секунды.
    rig.fake
        .fail_next(TelegramError::Transient("timeout".into()));
    assert_eq!(rig.pass(StdDuration::from_millis(500)).await, 0);
    let row = rig.row().await;
    assert_eq!(row.status, "pending");
    assert_eq!(row.attempts, 2);
    assert_eq!(row.next_attempt_at, first_attempt_at + Duration::seconds(3));
    assert!(row.dead_at.is_none());

    assert_eq!(rig.pass(SECOND).await, 0);
    assert!(rig.fake.sent().is_empty());

    // Следующая попытка успешна: ровно одно сообщение и сохранённый message_id.
    assert_eq!(rig.pass(SECOND).await, 1);
    let row = rig.row().await;
    assert_eq!(row.status, "sent");
    assert_eq!(row.attempts, 2);
    assert_eq!(row.last_error, None);
    assert!(row.dead_at.is_none());
    let message_id = row.message_id.expect("message id");
    let messages = rig.fake.messages(rig.chat);
    assert_eq!(messages.len(), 1, "{messages:?}");
    assert_eq!(messages[0].0, message_id);
    assert_eq!(rig.fake.sent().len(), 1);

    // По записи доставки сохранённый message_id доступен через outbox.
    assert_eq!(
        rig.delivery().await,
        ("sent".into(), Some(rig.seq), Some(message_id))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn five_failures_move_the_notification_to_dead_letter_with_time_and_metric() {
    let db = support::isolated_db().await;
    let mut rig = Rig::new(&db, "Оплатить счёт").await;
    let start = rig.now;

    let mut failure_at = start;
    for attempt in 1..=MAX_ATTEMPTS {
        if attempt > 1 {
            failure_at = rig.now + Duration::seconds(400);
        }
        rig.fake
            .fail_next(TelegramError::Transient("timeout".into()));
        assert_eq!(
            rig.pass(if attempt == 1 {
                ZERO
            } else {
                StdDuration::from_secs(400)
            })
            .await,
            0
        );

        let row = rig.row().await;
        if attempt < MAX_ATTEMPTS {
            assert_eq!(row.status, "pending", "attempt {attempt}");
            assert_eq!(row.attempts, attempt);
            assert!(row.dead_at.is_none(), "attempt {attempt}");
        } else {
            assert_eq!(row.status, "dead");
            assert_eq!(row.last_error.as_deref(), Some("transient"));
            assert_eq!(row.dead_at, Some(failure_at));
            assert_eq!(row.message_id, None);
        }
    }

    assert!(rig.fake.sent().is_empty());
    let mut conn = rig.pool.acquire().await.expect("conn");
    let counts = outbox::counts(&mut conn).await.expect("counts");
    assert_eq!(counts.pending, 0);
    assert_eq!(counts.dead, 1);

    // Шестой попытки нет: строка и её время не меняются.
    let dead = rig.row().await;
    assert_eq!(rig.pass(StdDuration::from_secs(400)).await, 0);
    let after = rig.row().await;
    assert_eq!(after.status, "dead");
    assert_eq!(after.attempts, dead.attempts);
    assert_eq!(after.next_attempt_at, dead.next_attempt_at);
    assert_eq!(after.dead_at, dead.dead_at);
    assert!(rig.fake.sent().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeated_processing_after_success_keeps_one_message_and_the_same_id() {
    let db = support::isolated_db().await;
    let mut rig = Rig::new(&db, "Купить билеты").await;

    assert_eq!(rig.pass(ZERO).await, 1);
    let delivered = rig.row().await;
    let message_id = delivered.message_id.expect("message id");
    assert_eq!(rig.fake.messages(rig.chat).len(), 1);

    // Повторный проход outbox не отправляет второе сообщение.
    assert_eq!(rig.pass(SECOND).await, 0);
    assert_eq!(rig.fake.messages(rig.chat).len(), 1);

    // Повторный тик планировщика тоже не создаёт новую доставку.
    let stats = scheduler(&rig.pool)
        .tick(rig.now)
        .await
        .expect("repeat tick");
    assert_eq!(stats.delivered, 0);
    assert_eq!(rig.outbox_rows().await, 1);
    assert_eq!(rig.fake.sent().len(), 1);

    // Повторная постановка с тем же dedupe_key не создаёт вторую строку.
    let key = format!("notification:reminder:{}", rig.delivery_id);
    let mut conn = rig.pool.acquire().await.expect("conn");
    let duplicate = outbox::enqueue(
        &mut conn,
        rig.chat,
        &Outgoing::Message {
            html: "<p>дубликат</p>".into(),
            rich: true,
        },
        Some(&key),
        rig.now,
    )
    .await
    .expect("enqueue");
    assert_eq!(duplicate, None);
    assert_eq!(rig.outbox_rows().await, 1);

    assert_eq!(rig.pass(SECOND).await, 0);
    assert_eq!(rig.fake.messages(rig.chat).len(), 1);
    assert_eq!(rig.row().await.message_id, Some(message_id));
    assert_eq!(
        rig.delivery().await,
        ("sent".into(), Some(rig.seq), Some(message_id))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_non_retryable_error_moves_the_notification_to_dead_letter_immediately() {
    let db = support::isolated_db().await;
    let mut rig = Rig::new(&db, "Заблокировал бот").await;
    let failed_at = rig.now;

    rig.fake
        .fail_next(TelegramError::Forbidden("blocked".into()));
    assert_eq!(rig.pass(ZERO).await, 0);

    let row = rig.row().await;
    assert_eq!(row.status, "dead");
    assert_eq!(row.attempts, 0);
    assert_eq!(row.last_error.as_deref(), Some("forbidden"));
    assert_eq!(row.dead_at, Some(failed_at));
    assert_eq!(row.message_id, None);
    assert!(rig.fake.sent().is_empty());

    let mut conn = rig.pool.acquire().await.expect("conn");
    let counts = outbox::counts(&mut conn).await.expect("counts");
    assert_eq!(counts.pending, 0);
    assert_eq!(counts.dead, 1);
}
