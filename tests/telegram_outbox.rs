//! Outbox: порядок, лимиты, `429`, повторы, замена сообщений и дедупликация.

mod support;

use std::time::{Duration as StdDuration, Instant};

use chrono::{DateTime, Duration, Utc};
use sqlx::{Acquire, PgPool};
use support::telegram::{Call, FakeTelegram};
use support::{db, next_telegram_id};
use tasklerq::telegram::api::TelegramError;
use tasklerq::telegram::outbox::{self, MAX_ATTEMPTS, OutboxWorker, Outgoing, Placement};
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
        self.screen_with(html, Placement::Visible).await
    }

    async fn screen_with(&self, html: &str, placement: Placement) -> i64 {
        let mut conn = self.pool.acquire().await.expect("conn");
        outbox::enqueue(
            &mut conn,
            self.chat,
            &Outgoing::Screen {
                html: html.into(),
                placement,
            },
            None,
            self.now,
        )
        .await
        .expect("enqueue")
        .expect("seq")
    }

    /// Поставить строку `screen` с произвольным payload (для старых или
    /// испорченных записей).
    async fn raw_screen(&self, payload: serde_json::Value) -> i64 {
        sqlx::query_scalar(
            "INSERT INTO outbox (chat_id, kind, payload, next_attempt_at, created_at)
             VALUES ($1, 'screen', $2, $3, $3)
             RETURNING seq",
        )
        .bind(self.chat)
        .bind(payload)
        .bind(self.now)
        .fetch_one(&self.pool)
        .await
        .expect("raw screen")
    }

    /// Поставить строку `message` с произвольным payload (для испорченных
    /// записей).
    async fn raw_message(&self, payload: serde_json::Value) -> i64 {
        sqlx::query_scalar(
            "INSERT INTO outbox (chat_id, kind, payload, next_attempt_at, created_at)
             VALUES ($1, 'message', $2, $3, $3)
             RETURNING seq",
        )
        .bind(self.chat)
        .bind(payload)
        .bind(self.now)
        .fetch_one(&self.pool)
        .await
        .expect("raw message")
    }

    /// `placement` последней ожидающей строки экрана.
    async fn pending_placement(&self) -> String {
        sqlx::query_scalar(
            "SELECT payload->>'placement' FROM outbox
             WHERE chat_id = $1 AND status = 'pending' AND kind = 'screen'
             ORDER BY seq DESC LIMIT 1",
        )
        .bind(self.chat)
        .fetch_one(&self.pool)
        .await
        .expect("pending placement")
    }

    async fn edit(&self, message_id: i64, html: &str) -> i64 {
        let mut conn = self.pool.acquire().await.expect("conn");
        outbox::enqueue(
            &mut conn,
            self.chat,
            &Outgoing::Edit {
                message_id,
                html: html.into(),
            },
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
                delete_after: None,
            },
            key,
            self.now,
        )
        .await
        .expect("enqueue")
    }

    async fn message_expiring(
        &self,
        html: &str,
        delete_after: u32,
        key: Option<&str>,
    ) -> Option<i64> {
        let mut conn = self.pool.acquire().await.expect("conn");
        outbox::enqueue(
            &mut conn,
            self.chat,
            &Outgoing::Message {
                html: html.into(),
                rich: true,
                delete_after: Some(delete_after),
            },
            key,
            self.now,
        )
        .await
        .expect("enqueue")
    }

    async fn delete(&self, message_id: i64) -> i64 {
        self.delete_keyed(message_id, None).await
    }

    async fn delete_keyed(&self, message_id: i64, key: Option<&str>) -> i64 {
        let mut conn = self.pool.acquire().await.expect("conn");
        outbox::enqueue(
            &mut conn,
            self.chat,
            &Outgoing::Delete { message_id },
            key,
            self.now,
        )
        .await
        .expect("enqueue")
        .expect("seq")
    }

    /// Создать строку вида чата, чтобы проверить `buried`.
    async fn put_view(&self) {
        let mut conn = self.pool.acquire().await.expect("conn");
        views::set(
            &mut conn,
            self.chat,
            "menu",
            &serde_json::json!({}),
            self.now,
        )
        .await
        .expect("view");
    }

    async fn set_buried(&self, buried: bool) {
        sqlx::query("UPDATE telegram_views SET buried = $2 WHERE chat_id = $1")
            .bind(self.chat)
            .bind(buried)
            .execute(&self.pool)
            .await
            .expect("buried");
    }

    async fn buried(&self) -> bool {
        let mut conn = self.pool.acquire().await.expect("conn");
        views::get(&mut conn, self.chat)
            .await
            .expect("view")
            .expect("view exists")
            .buried
    }

    async fn view_message(&self) -> Option<i64> {
        let mut conn = self.pool.acquire().await.expect("conn");
        views::get(&mut conn, self.chat)
            .await
            .expect("view")
            .expect("view exists")
            .message_id
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

    async fn payload_text(&self, seq: i64) -> String {
        sqlx::query_scalar("SELECT payload::text FROM outbox WHERE seq = $1")
            .bind(seq)
            .fetch_one(&self.pool)
            .await
            .expect("payload")
    }

    /// Строки удаления чата: `(seq, payload, next_attempt_at, dedupe_key)`.
    async fn delete_rows(&self) -> Vec<(i64, String, DateTime<Utc>, Option<String>)> {
        sqlx::query_as(
            "SELECT seq, payload::text, next_attempt_at, dedupe_key FROM outbox
             WHERE chat_id = $1 AND kind = 'delete'
             ORDER BY seq",
        )
        .bind(self.chat)
        .fetch_all(&self.pool)
        .await
        .expect("delete rows")
    }

    /// Подвинуть время стенда без прохода воркера.
    fn advance(&mut self, by: StdDuration) {
        self.clock += by;
        self.now += Duration::from_std(by).expect("duration");
    }
}

const SECOND: StdDuration = StdDuration::from_secs(1);

/// Сериализует тесты, которые могут увеличить `outbox_dead_total`: реестр
/// метрик общий на процесс, и параллельный dead-letter соседнего теста сломал
/// бы проверку «метрика не выросла».
static DEAD_METRIC: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn counter(text: &str, series: &str) -> f64 {
    text.lines()
        .find_map(|line| {
            let mut parts = line.split_whitespace();
            if parts.next()? != series {
                return None;
            }
            parts.next()?.parse().ok()
        })
        .unwrap_or(0.0)
}

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
async fn delivered_message_buries_the_view() {
    let db = db().await;
    let mut rig = Rig::new(&db.pool);
    rig.put_view().await;
    assert!(!rig.buried().await);

    rig.message("<p>напоминание</p>", None)
        .await
        .expect("message");
    assert_eq!(rig.pass().await, 1);

    assert!(
        rig.buried().await,
        "a delivered message must bury the active view"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn editing_a_notification_keeps_the_buried_flag() {
    let db = db().await;
    let mut rig = Rig::new(&db.pool);
    rig.put_view().await;
    rig.message("<p>уведомление</p>", None)
        .await
        .expect("message");
    rig.pass().await;

    for (buried, html) in [(false, "<p>правка 1</p>"), (true, "<p>правка 2</p>")] {
        rig.set_buried(buried).await;
        let message_id = rig.fake.messages(rig.chat).last().expect("notification").0;
        let seq = rig.edit(message_id, html).await;
        assert_eq!(rig.pass_after(SECOND).await, 1);
        assert_eq!(rig.row(seq).await.0, "sent");
        assert_eq!(
            rig.buried().await,
            buried,
            "an edit must keep the buried flag"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edit_fallback_with_a_new_message_buries_the_view() {
    let db = db().await;
    let mut rig = Rig::new(&db.pool);
    rig.put_view().await;
    rig.message("<p>уведомление</p>", None)
        .await
        .expect("message");
    rig.pass().await;
    rig.set_buried(false).await;
    rig.fake.reject_edits(Some(TelegramError::MessageGone));
    let message_id = rig.fake.messages(rig.chat).last().expect("notification").0;

    let seq = rig.edit(message_id, "<p>запасной</p>").await;
    assert_eq!(rig.pass_after(SECOND).await, 1);

    assert_eq!(rig.row(seq).await.0, "sent");
    assert!(
        rig.buried().await,
        "a new message sent instead of the edit lands below the anchor"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn screen_placement_picks_edit_or_new_message() {
    for placement in [Placement::InPlace, Placement::Visible, Placement::Fresh] {
        for buried in [false, true] {
            let db = db().await;
            let mut rig = Rig::new(&db.pool);
            rig.screen("<p>первый</p>").await;
            assert_eq!(rig.pass().await, 1);
            let old_id = rig.view_message().await.expect("active message");
            rig.set_buried(buried).await;
            rig.fake.clear_calls();

            rig.screen_with("<p>второй</p>", placement).await;
            assert_eq!(
                rig.pass_after(SECOND).await,
                1,
                "{placement:?} with buried={buried}"
            );

            let calls = rig.fake.calls();
            let replaces = matches!(placement, Placement::Fresh)
                || (placement == Placement::Visible && buried);
            if replaces {
                assert!(
                    calls
                        .iter()
                        .any(|call| matches!(call, Call::SendRich { id, .. } if *id != old_id)),
                    "{placement:?} with buried={buried}: new message expected: {calls:?}"
                );
                assert!(
                    calls
                        .iter()
                        .any(|call| matches!(call, Call::Delete { id, .. } if *id == old_id)),
                    "{placement:?} with buried={buried}: old anchor delete expected: {calls:?}"
                );
                assert!(
                    !rig.buried().await,
                    "{placement:?} with buried={buried}: a new screen is visible"
                );
            } else {
                assert_eq!(
                    calls,
                    vec![Call::EditRich {
                        chat: rig.chat,
                        id: old_id,
                        html: "<p>второй</p>".into(),
                    }],
                    "{placement:?} with buried={buried}: in-place edit expected"
                );
                assert_eq!(
                    rig.buried().await,
                    buried,
                    "{placement:?} with buried={buried}: an edit keeps buried"
                );
            }
            assert_eq!(
                rig.fake.messages(rig.chat).len(),
                1,
                "{placement:?} with buried={buried}: exactly one live screen"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn screen_without_active_message_is_always_sent_new() {
    for placement in [Placement::InPlace, Placement::Visible, Placement::Fresh] {
        let db = db().await;
        let mut rig = Rig::new(&db.pool);
        let seq = rig.screen_with("<p>экран</p>", placement).await;

        assert_eq!(rig.pass().await, 1, "{placement:?}");
        assert_eq!(rig.row(seq).await.0, "sent", "{placement:?}");
        assert!(
            matches!(rig.fake.calls().first(), Some(Call::SendRich { .. })),
            "{placement:?}: a new message expected"
        );
        assert!(
            !rig.buried().await,
            "{placement:?}: a new screen is visible"
        );
        assert!(rig.view_message().await.is_some(), "{placement:?}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_anchor_delete_does_not_block_a_fresh_screen() {
    for error in [
        TelegramError::MessageUndeletable,
        TelegramError::Transient("503".into()),
    ] {
        let db = db().await;
        let mut rig = Rig::new(&db.pool);
        rig.screen("<p>первый</p>").await;
        assert_eq!(rig.pass().await, 1);
        let old_id = rig.view_message().await.expect("active message");
        rig.fake.reject_deletes(Some(error));
        rig.fake.clear_calls();

        let seq = rig.screen_with("<p>второй</p>", Placement::Fresh).await;
        assert_eq!(rig.pass_after(SECOND).await, 1);

        assert_eq!(rig.row(seq).await.0, "sent");
        let new_id = rig.view_message().await.expect("new message");
        assert_ne!(new_id, old_id);
        assert!(
            rig.fake
                .calls()
                .iter()
                .any(|call| matches!(call, Call::SendRich { id, .. } if *id == new_id)),
            "the new screen must be sent even when the old anchor cannot be deleted"
        );
        assert!(!rig.buried().await);
        assert_eq!(
            rig.fake.messages(rig.chat).len(),
            2,
            "the old anchor stays when Telegram refuses to delete it"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn message_gone_on_in_place_edit_replaces_the_screen() {
    let db = db().await;
    let mut rig = Rig::new(&db.pool);
    rig.screen("<p>первый</p>").await;
    assert_eq!(rig.pass().await, 1);
    let old_id = rig.view_message().await.expect("active message");
    rig.fake.reject_edits(Some(TelegramError::MessageGone));
    rig.fake.clear_calls();

    let seq = rig.screen_with("<p>второй</p>", Placement::InPlace).await;
    assert_eq!(rig.pass_after(SECOND).await, 1);

    assert_eq!(rig.row(seq).await.0, "sent");
    let calls = rig.fake.calls();
    assert!(
        matches!(&calls[0], Call::Delete { id, .. } if *id == old_id),
        "{calls:?}"
    );
    let Call::SendRich { id: new_id, .. } = &calls[1] else {
        panic!("send expected: {calls:?}")
    };
    assert_ne!(*new_id, old_id);
    assert_eq!(rig.view_message().await, Some(*new_id));
    assert_eq!(rig.fake.messages(rig.chat).len(), 1);
    assert!(!rig.buried().await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn evicting_screen_keeps_the_strongest_placement() {
    let db = db().await;
    let mut rig = Rig::new(&db.pool);
    rig.screen("<p>первый</p>").await;
    assert_eq!(rig.pass().await, 1);
    let old_id = rig.view_message().await.expect("active message");
    rig.fake.clear_calls();

    let fresh = rig.screen_with("<p>старт</p>", Placement::Fresh).await;
    rig.screen_with("<p>кнопка</p>", Placement::InPlace).await;
    assert_eq!(rig.row(fresh).await.0, "skipped");

    assert_eq!(rig.pass_after(SECOND).await, 1);

    let calls = rig.fake.calls();
    assert!(
        calls.iter().any(|call| matches!(
            call,
            Call::SendRich { id, html, .. } if *id != old_id && html == "<p>кнопка</p>"
        )),
        "the evicting in_place screen must inherit fresh and be sent new: {calls:?}"
    );
    assert!(
        calls
            .iter()
            .any(|call| matches!(call, Call::Delete { id, .. } if *id == old_id)),
        "the old anchor must be deleted: {calls:?}"
    );
    assert!(
        !calls
            .iter()
            .any(|call| matches!(call, Call::EditRich { .. })),
        "an evicted fresh screen must not turn into an edit: {calls:?}"
    );
    assert_eq!(rig.fake.messages(rig.chat).len(), 1);
    assert_eq!(rig.fake.messages(rig.chat)[0].1, "<p>кнопка</p>");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn evicting_screen_takes_the_strongest_placement_in_any_order() {
    for (first, second, expected) in [
        (Placement::InPlace, Placement::Visible, "visible"),
        (Placement::Visible, Placement::InPlace, "visible"),
        (Placement::Visible, Placement::Fresh, "fresh"),
        (Placement::Fresh, Placement::Visible, "fresh"),
        (Placement::InPlace, Placement::Fresh, "fresh"),
        (Placement::Fresh, Placement::InPlace, "fresh"),
    ] {
        let db = db().await;
        let rig = Rig::new(&db.pool);
        rig.screen_with("<p>первый</p>", first).await;
        rig.screen_with("<p>второй</p>", second).await;

        assert_eq!(
            rig.pending_placement().await,
            expected,
            "{first:?} then {second:?} must end as {expected}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn screen_with_unknown_placement_is_dead_as_invalid_payload() {
    let _dead = DEAD_METRIC.lock().await;
    let db = db().await;
    let mut rig = Rig::new(&db.pool);
    let seq = rig
        .raw_screen(serde_json::json!({"html": "<p>экран</p>", "placement": "sometimes"}))
        .await;

    assert_eq!(rig.pass().await, 0);
    assert_eq!(
        rig.row(seq).await,
        ("dead".into(), 0, Some("invalid_payload".into()), None)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn message_with_invalid_delete_after_is_dead_as_invalid_payload() {
    let _dead = DEAD_METRIC.lock().await;
    let db = db().await;
    let mut rig = Rig::new(&db.pool);
    let seq = rig
        .raw_message(serde_json::json!({
            "html": "<p>секрет</p>",
            "rich": true,
            "delete_after": -1
        }))
        .await;

    assert_eq!(rig.pass().await, 0);
    assert_eq!(
        rig.row(seq).await,
        ("dead".into(), 0, Some("invalid_payload".into()), None)
    );
    assert!(
        rig.fake.sent().is_empty(),
        "an invalid delete_after must not send the message: {:?}",
        rig.fake.calls()
    );
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
    let _dead = DEAD_METRIC.lock().await;
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
    let _dead = DEAD_METRIC.lock().await;
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
    let _dead = DEAD_METRIC.lock().await;
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn successful_delete_is_sent_and_removes_the_message() {
    let db = db().await;
    let mut rig = Rig::new(&db.pool);
    let seq = rig.delete(55).await;

    assert_eq!(rig.pass().await, 1);

    assert_eq!(rig.row(seq).await.0, "sent");
    assert_eq!(
        rig.fake.calls(),
        vec![Call::Delete {
            chat: rig.chat,
            id: 55
        }]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_not_found_counts_as_sent() {
    let db = db().await;
    let mut rig = Rig::new(&db.pool);
    rig.fake.reject_deletes(Some(TelegramError::MessageGone));
    let seq = rig.delete(56).await;

    assert_eq!(rig.pass().await, 1);
    assert_eq!(rig.row(seq).await.0, "sent");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undeletable_message_is_skipped_and_buries_the_view_without_dead_metric() {
    let _dead = DEAD_METRIC.lock().await;
    let db = db().await;
    let mut rig = Rig::new(&db.pool);
    rig.put_view().await;
    rig.fake
        .reject_deletes(Some(TelegramError::MessageUndeletable));
    let before = counter(&tasklerq::metrics::render(), "outbox_dead_total");
    let seq = rig.delete(57).await;

    assert_eq!(rig.pass().await, 0);

    assert_eq!(
        rig.row(seq).await,
        (
            "skipped".into(),
            0,
            Some("message_undeletable".into()),
            None
        )
    );
    let buried: bool = sqlx::query_scalar("SELECT buried FROM telegram_views WHERE chat_id = $1")
        .bind(rig.chat)
        .fetch_one(&db.pool)
        .await
        .expect("buried");
    assert!(buried, "an undeletable user message must bury the view");
    let after = counter(&tasklerq::metrics::render(), "outbox_dead_total");
    assert_eq!(
        after, before,
        "an undeletable message is skipped, not a dead-letter"
    );
    let payload: String = sqlx::query_scalar("SELECT payload::text FROM outbox WHERE seq = $1")
        .bind(seq)
        .fetch_one(&db.pool)
        .await
        .expect("payload");
    assert_eq!(
        payload, r#"{"redacted": true}"#,
        "the skipped delete payload must be redacted"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn temporary_delete_failure_backs_off_and_keeps_the_row_pending() {
    let db = db().await;
    let mut rig = Rig::new(&db.pool);
    rig.fake
        .reject_deletes(Some(TelegramError::Transient("503".into())));
    let seq = rig.delete(58).await;

    assert_eq!(rig.pass().await, 0);
    let (status, attempts, error, _) = rig.row(seq).await;
    assert_eq!(
        (status.as_str(), attempts, error.as_deref()),
        ("pending", 1, Some("transient"))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn permanently_failed_delete_is_dead_and_buries_the_view() {
    let _dead = DEAD_METRIC.lock().await;
    let db = db().await;
    let mut rig = Rig::new(&db.pool);
    rig.put_view().await;
    rig.fake
        .reject_deletes(Some(TelegramError::BadRequest("bad".into())));
    let seq = rig.delete(61).await;

    assert_eq!(rig.pass().await, 0);

    assert_eq!(
        rig.row(seq).await,
        ("dead".into(), 0, Some("bad_request".into()), None)
    );
    assert!(
        rig.buried().await,
        "a dead delete leaves the user message in the chat, so the view is buried"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_exhausted_by_retries_is_dead_and_buries_the_view() {
    let _dead = DEAD_METRIC.lock().await;
    let db = db().await;
    let mut rig = Rig::new(&db.pool);
    rig.put_view().await;
    rig.fake
        .reject_deletes(Some(TelegramError::Transient("503".into())));
    let seq = rig.delete(62).await;

    for _ in 0..MAX_ATTEMPTS {
        rig.pass_after(StdDuration::from_secs(400)).await;
    }

    assert_eq!(rig.row(seq).await.0, "dead");
    assert!(
        rig.buried().await,
        "a delete that exhausted its retries leaves the user message in the chat"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_rate_limit_defers_without_spending_an_attempt() {
    let db = db().await;
    let mut rig = Rig::new(&db.pool);
    rig.fake.reject_deletes(Some(TelegramError::RetryAfter(5)));
    let seq = rig.delete(59).await;

    assert_eq!(rig.pass().await, 0);
    let (status, attempts, error, _) = rig.row(seq).await;
    assert_eq!(
        (status.as_str(), attempts, error.as_deref()),
        ("pending", 0, Some("retry_after"))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_then_screen_are_delivered_in_two_passes_at_the_same_clock() {
    let db = db().await;
    let mut rig = Rig::new(&db.pool);
    let delete = rig.delete(60).await;
    let screen = rig.screen("<p>экран</p>").await;

    // Кандидаты отбираются один раз: экран за ожидающим удалением попадёт
    // только во второй проход. Часы при этом не двигаются — удаление не
    // должно занимать поминутную квоту чата.
    assert_eq!(rig.pass().await, 1);
    assert_eq!(rig.row(delete).await.0, "sent");
    assert_eq!(rig.row(screen).await.0, "pending");

    assert_eq!(rig.pass().await, 1);
    assert_eq!(rig.row(screen).await.0, "sent");

    let calls = rig.fake.calls();
    assert!(matches!(&calls[0], Call::Delete { id, .. } if *id == 60));
    assert!(
        matches!(&calls[1], Call::SendRich { html, .. } if html == "<p>экран</p>"),
        "{calls:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delivered_message_schedules_delete_from_the_delivery_time() {
    let db = db().await;
    let mut rig = Rig::new(&db.pool);
    let seq = rig
        .message_expiring("<p>секрет</p>", 3_600, None)
        .await
        .expect("seq");

    // Доставили через 10 минут после постановки: отсчёт идёт от доставки.
    assert_eq!(rig.pass_after(StdDuration::from_secs(600)).await, 1);
    let delivered_at = rig.now;

    let (status, _, _, message_id) = rig.row(seq).await;
    let message_id = message_id.expect("message id");
    assert_eq!(status, "sent");
    assert_eq!(
        rig.payload_text(seq).await,
        r#"{"redacted": true}"#,
        "the delivered message payload must be wiped"
    );

    let deletes = rig.delete_rows().await;
    assert_eq!(deletes.len(), 1, "{deletes:?}");
    let (delete_seq, payload, next_attempt_at, key) = &deletes[0];
    assert_eq!(
        *payload,
        format!(r#"{{"message_id": {message_id}}}"#),
        "the delete row must carry only the message id"
    );
    assert_eq!(
        (*next_attempt_at - (delivered_at + Duration::seconds(3_600)))
            .num_milliseconds()
            .abs(),
        0,
        "the delete must be scheduled from delivery, not from enqueue"
    );
    let expected_key = format!("expire:{}:{message_id}", rig.chat);
    assert_eq!(key.as_deref(), Some(expected_key.as_str()));
    assert_eq!(rig.row(*delete_seq).await.0, "pending");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scheduled_delete_does_not_hold_the_chat_and_fires_on_time() {
    let db = db().await;
    let mut rig = Rig::new(&db.pool);
    rig.message_expiring("<p>секрет</p>", 3_600, None)
        .await
        .expect("seq");
    assert_eq!(rig.pass_after(StdDuration::from_secs(600)).await, 1);
    let message_id = rig.fake.messages(rig.chat)[0].0;
    let delete_seq = rig.delete_rows().await[0].0;

    // Следующее сообщение того же чата уходит сразу: отложенная строка
    // удаления очередь не держит.
    let next = rig.message("<p>следующее</p>", None).await.expect("seq");
    assert_eq!(rig.pass_after(SECOND).await, 1);
    assert_eq!(rig.row(next).await.0, "sent");
    assert_eq!(rig.row(delete_seq).await.0, "pending");

    // За секунду до срока удаления нет.
    assert_eq!(rig.pass_after(StdDuration::from_secs(3_598)).await, 0);
    assert!(
        !rig.fake
            .calls()
            .iter()
            .any(|call| matches!(call, Call::Delete { id, .. } if *id == message_id)),
        "the message must not be deleted before its hour is up"
    );

    // Ровно через час после доставки — удаление.
    assert_eq!(rig.pass_after(SECOND).await, 1);
    assert_eq!(rig.row(delete_seq).await.0, "sent");
    assert!(
        rig.fake.calls().iter().any(
            |call| matches!(call, Call::Delete { chat, id } if *chat == rig.chat && *id == message_id)
        ),
        "{:?}",
        rig.fake.calls()
    );
    assert!(
        rig.fake
            .messages(rig.chat)
            .iter()
            .all(|(id, _)| *id != message_id)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn button_delete_is_not_swallowed_by_expire_and_the_timer_is_message_gone() {
    let _dead = DEAD_METRIC.lock().await;
    let db = db().await;
    let mut rig = Rig::new(&db.pool);
    rig.message_expiring("<p>секрет</p>", 3_600, None)
        .await
        .expect("seq");
    assert_eq!(rig.pass_after(StdDuration::from_secs(600)).await, 1);
    let message_id = rig.fake.messages(rig.chat)[0].0;
    let timer = rig.delete_rows().await[0].0;

    // Кнопка «Я сохранил» через минуту: свой ключ, своя строка.
    rig.advance(StdDuration::from_secs(60));
    let button = rig
        .delete_keyed(
            message_id,
            Some(&format!("delete:{}:{message_id}", rig.chat)),
        )
        .await;
    assert!(
        rig.delete_rows().await.len() == 2,
        "the button delete must not be collapsed into the expire row"
    );
    assert_eq!(rig.pass().await, 1);
    assert_eq!(rig.row(button).await.0, "sent");
    assert!(rig.fake.messages(rig.chat).is_empty());
    assert_eq!(rig.row(timer).await.0, "pending");

    // В срок таймер получает от Telegram «сообщения нет» — это успех.
    rig.fake.reject_deletes(Some(TelegramError::MessageGone));
    let before = counter(&tasklerq::metrics::render(), "outbox_dead_total");
    assert_eq!(rig.pass_after(StdDuration::from_secs(3_540)).await, 1);
    assert_eq!(
        rig.row(timer).await,
        ("sent".into(), 0, None, Some(message_id))
    );
    let after = counter(&tasklerq::metrics::render(), "outbox_dead_total");
    assert_eq!(
        after, before,
        "MessageGone on the timer delete is a success, not a dead-letter"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn message_rows_without_delete_after_never_schedule_deletion() {
    let db = db().await;
    let mut rig = Rig::new(&db.pool);

    // Старая строка, вставленная SQL-ом без поля.
    let legacy = sqlx::query_scalar(
        "INSERT INTO outbox (chat_id, kind, payload, next_attempt_at, created_at)
         VALUES ($1, 'message', $2, $3, $3)
         RETURNING seq",
    )
    .bind(rig.chat)
    .bind(serde_json::json!({"html": "<p>старое</p>", "rich": true}))
    .bind(rig.now)
    .fetch_one(&db.pool)
    .await
    .expect("legacy");
    let plain = rig.message("<p>обычное</p>", None).await.expect("seq");

    assert_eq!(rig.pass().await, 1);
    assert_eq!(rig.pass_after(SECOND).await, 1);

    assert_eq!(rig.row(legacy).await.0, "sent");
    assert_eq!(rig.row(plain).await.0, "sent");
    assert!(
        rig.delete_rows().await.is_empty(),
        "a message without delete_after must not schedule a delete"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn existing_expire_row_is_not_duplicated() {
    let db = db().await;
    let mut rig = Rig::new(&db.pool);
    // Первая же отправка подделки получает id 1.
    let key = format!("expire:{}:1", rig.chat);
    let run_at = rig.now + Duration::hours(1);
    let mut conn = rig.pool.acquire().await.expect("conn");
    let existing = outbox::enqueue_at(
        &mut conn,
        rig.chat,
        &Outgoing::Delete { message_id: 1 },
        Some(&key),
        rig.now,
        run_at,
    )
    .await
    .expect("enqueue")
    .expect("seq");
    drop(conn);

    let message = rig
        .message_expiring("<p>секрет</p>", 3_600, None)
        .await
        .expect("seq");
    assert_eq!(rig.pass().await, 1);
    assert_eq!(rig.row(message).await.3, Some(1));

    let rows: Vec<(i64, DateTime<Utc>)> = sqlx::query_as(
        "SELECT seq, next_attempt_at FROM outbox
         WHERE chat_id = $1 AND kind = 'delete'
         ORDER BY seq",
    )
    .bind(rig.chat)
    .fetch_all(&db.pool)
    .await
    .expect("rows");
    assert_eq!(
        rows.len(),
        1,
        "a conflicting expire key must not add a second row"
    );
    assert_eq!(rows[0].0, existing);
    assert_eq!(
        (rows[0].1 - run_at).num_milliseconds().abs(),
        0,
        "the pre-existing row keeps its schedule"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_delivery_does_not_schedule_deletion() {
    let _dead = DEAD_METRIC.lock().await;
    let db = db().await;
    let mut rig = Rig::new(&db.pool);
    let seq = rig
        .message_expiring("<p>секрет</p>", 3_600, None)
        .await
        .expect("seq");
    rig.fake
        .fail_next(TelegramError::Forbidden("blocked".into()));

    assert_eq!(rig.pass().await, 0);

    assert_eq!(rig.row(seq).await.0, "dead");
    assert!(
        rig.delete_rows().await.is_empty(),
        "an undelivered message has no id to delete"
    );
}

/// Дождаться, пока соединение встанет в ожидание блокировки.
async fn wait_for_lock(pool: &PgPool, pid: i32) {
    let wait = async {
        loop {
            let waiting: bool = sqlx::query_scalar(
                "SELECT EXISTS (
                     SELECT 1 FROM pg_stat_activity
                     WHERE pid = $1 AND wait_event_type = 'Lock'
                 )",
            )
            .bind(pid)
            .fetch_one(pool)
            .await
            .expect("pg_stat_activity");
            if waiting {
                break;
            }
            tokio::time::sleep(StdDuration::from_millis(2)).await;
        }
    };
    tokio::time::timeout(StdDuration::from_secs(5), wait)
        .await
        .expect("the transaction must wait for the row lock");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enqueue_and_delivery_take_locks_in_the_same_order() {
    let db = db().await;
    let rig = Rig::new(&db.pool);
    rig.put_view().await;
    let in_flight = rig.screen("old").await;

    let now = rig.now;
    let chat = rig.chat;
    let mut delivering = db.pool.acquire().await.expect("delivery connection");
    let mut delivery_tx = delivering.begin().await.expect("delivery transaction");
    sqlx::query("SELECT seq FROM outbox WHERE seq = $1 FOR UPDATE")
        .bind(in_flight)
        .execute(&mut *delivery_tx)
        .await
        .expect("delivery claims the row");

    let mut updating = db.pool.acquire().await.expect("update connection");
    let update_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *updating)
        .await
        .expect("update pid");
    let update = tokio::spawn(async move {
        let mut update_tx = updating.begin().await.expect("update transaction");
        let enqueued = outbox::enqueue_screen(
            &mut update_tx,
            chat,
            "menu",
            &serde_json::json!({}),
            "new".into(),
            Placement::Visible,
            now,
        )
        .await;
        let committed = update_tx.commit().await;
        (enqueued, committed)
    });
    // Воркер обновлений берёт строки `outbox` раньше `telegram_views`, поэтому
    // он ждёт доставку, не удерживая запись вида.
    wait_for_lock(&db.pool, update_pid).await;

    let written = views::set_message(&mut delivery_tx, chat, 42, false, now).await;
    let delivery_committed = delivery_tx.commit().await;
    written.expect("delivery writes the view without a deadlock");
    delivery_committed.expect("delivery commit");

    let (enqueued, committed) = update.await.expect("update task");
    let seq = enqueued
        .expect("enqueue must not deadlock with the delivery")
        .expect("new screen");
    committed.expect("update commit");

    let status: String = sqlx::query_scalar("SELECT status::text FROM outbox WHERE seq = $1")
        .bind(in_flight)
        .fetch_one(&db.pool)
        .await
        .expect("in-flight row");
    assert_eq!(
        status, "skipped",
        "after the delivery releases the row, the new screen evicts it"
    );
    let pending: Vec<i64> = sqlx::query_scalar(
        "SELECT seq FROM outbox
         WHERE chat_id = $1 AND status = 'pending' AND kind = 'screen'
         ORDER BY seq",
    )
    .bind(chat)
    .fetch_all(&db.pool)
    .await
    .expect("pending screens");
    assert_eq!(pending, vec![seq], "only the new screen waits for delivery");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_screen_held_by_a_rolled_back_delivery_is_still_evicted() {
    let db = db().await;
    let rig = Rig::new(&db.pool);
    rig.put_view().await;
    let in_flight = rig.screen_with("old", Placement::Fresh).await;

    let now = rig.now;
    let chat = rig.chat;
    let mut delivering = db.pool.acquire().await.expect("delivery connection");
    let mut delivery_tx = delivering.begin().await.expect("delivery transaction");
    sqlx::query("SELECT seq FROM outbox WHERE seq = $1 FOR UPDATE")
        .bind(in_flight)
        .execute(&mut *delivery_tx)
        .await
        .expect("delivery claims the row");

    let mut updating = db.pool.acquire().await.expect("update connection");
    let update_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *updating)
        .await
        .expect("update pid");
    let update = tokio::spawn(async move {
        let mut update_tx = updating.begin().await.expect("update transaction");
        let enqueued = outbox::enqueue_screen(
            &mut update_tx,
            chat,
            "menu",
            &serde_json::json!({}),
            "new".into(),
            Placement::Visible,
            now,
        )
        .await;
        let committed = update_tx.commit().await;
        (enqueued, committed)
    });
    wait_for_lock(&db.pool, update_pid).await;

    // Доставка откатилась (например, лимитер не дал слот): строка A осталась
    // `pending`, и новый экран должен её вытеснить, а не обойти.
    delivery_tx.rollback().await.expect("delivery rollback");

    let (enqueued, committed) = update.await.expect("update task");
    let seq = enqueued.expect("enqueue").expect("new screen");
    committed.expect("update commit");

    let status_a: String = sqlx::query_scalar("SELECT status::text FROM outbox WHERE seq = $1")
        .bind(in_flight)
        .fetch_one(&db.pool)
        .await
        .expect("in-flight row");
    assert_eq!(status_a, "skipped", "the rolled-back screen is evicted");
    let pending: Vec<(i64, String)> = sqlx::query_as(
        "SELECT seq, payload->>'placement' FROM outbox
         WHERE chat_id = $1 AND status = 'pending' AND kind = 'screen'
         ORDER BY seq",
    )
    .bind(chat)
    .fetch_all(&db.pool)
    .await
    .expect("pending screens");
    assert_eq!(
        pending,
        vec![(seq, "fresh".to_owned())],
        "only B waits, and it inherits A's placement"
    );
}
