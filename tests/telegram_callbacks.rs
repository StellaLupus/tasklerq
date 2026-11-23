//! Callback-токены: одноразовость, привязка к пользователю и срок жизни.

mod support;

use chrono::{DateTime, Duration, Utc};
use serde_json::json;
use support::telegram::{Call, Harness};
use support::{create_household, create_item, db, isolated_db, new_item, next_telegram_id};
use tasklerq::auth::tokens::hash_token;
use tasklerq::domain::models::Item;
use tasklerq::telegram::bot::model::{Action, QuickDue, Screen};
use tasklerq::telegram::callbacks::{self, Consumed, MAX_CALLBACK_BYTES, NewToken, Scope};

fn spec(user: i64, household: Option<uuid::Uuid>) -> NewToken {
    NewToken {
        telegram_user_id: user,
        household_id: household,
        scope: Scope::Screen,
        action: "go".into(),
        payload: json!({"a": "go"}),
        version: Some(3),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn valid_token_is_consumed_once() {
    let db = db().await;
    let household = create_household(&db.pool).await;
    let now = Utc::now();
    let mut conn = db.pool.acquire().await.expect("conn");

    let tokens = callbacks::issue_many(&mut conn, &[spec(1, Some(household.household_id))], now)
        .await
        .expect("issue");
    let token = &tokens[0];
    assert!(token.len() <= MAX_CALLBACK_BYTES);
    assert!(token.starts_with("t:"));

    let first = callbacks::consume(&mut conn, token, 1, now)
        .await
        .expect("consume");
    let Consumed::Valid(valid) = first else {
        panic!("valid expected, got {first:?}")
    };
    assert_eq!(valid.household_id, Some(household.household_id));
    assert_eq!(valid.version, Some(3));
    assert_eq!(valid.payload, json!({"a": "go"}));

    let second = callbacks::consume(&mut conn, token, 1, now)
        .await
        .expect("consume");
    assert!(
        matches!(second, Consumed::Replayed(Scope::Screen)),
        "{second:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn token_is_stored_only_as_hash() {
    let db = db().await;
    let now = Utc::now();
    let mut conn = db.pool.acquire().await.expect("conn");
    let tokens = callbacks::issue_many(&mut conn, &[spec(2, None)], now)
        .await
        .expect("issue");

    let raw: i64 = sqlx::query_scalar("SELECT count(*) FROM callback_tokens WHERE token_hash = $1")
        .bind(&tokens[0])
        .fetch_one(&db.pool)
        .await
        .expect("count");
    let hashed: i64 =
        sqlx::query_scalar("SELECT count(*) FROM callback_tokens WHERE token_hash = $1")
            .bind(hash_token(&tokens[0]))
            .fetch_one(&db.pool)
            .await
            .expect("count");

    assert_eq!((raw, hashed), (0, 1));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreign_user_cannot_use_or_burn_the_token() {
    let db = db().await;
    let now = Utc::now();
    let mut conn = db.pool.acquire().await.expect("conn");
    let tokens = callbacks::issue_many(&mut conn, &[spec(10, None)], now)
        .await
        .expect("issue");

    let stranger = callbacks::consume(&mut conn, &tokens[0], 11, now)
        .await
        .expect("consume");
    assert!(matches!(stranger, Consumed::Foreign), "{stranger:?}");

    // Чужая попытка не тратит токен владельца.
    let owner = callbacks::consume(&mut conn, &tokens[0], 10, now)
        .await
        .expect("consume");
    assert!(matches!(owner, Consumed::Valid(_)), "{owner:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_and_unknown_tokens_are_refused() {
    let db = db().await;
    let now = Utc::now();
    let mut conn = db.pool.acquire().await.expect("conn");
    let tokens = callbacks::issue_many(&mut conn, &[spec(20, None)], now)
        .await
        .expect("issue");

    let late = callbacks::consume(
        &mut conn,
        &tokens[0],
        20,
        now + Duration::hours(24) + Duration::seconds(1),
    )
    .await
    .expect("consume");
    let Consumed::Expired(expired) = late else {
        panic!("expired expected, got {late:?}")
    };
    assert_eq!(expired.scope, Scope::Screen);
    assert_eq!(expired.action, "go");
    assert_eq!(expired.payload, json!({"a": "go"}));
    assert_eq!(expired.version, Some(3));

    // Истёкший токен не помечается потреблённым.
    let consumed_at: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT consumed_at FROM callback_tokens WHERE token_hash = $1")
            .bind(hash_token(&tokens[0]))
            .fetch_one(&db.pool)
            .await
            .expect("token state");
    assert!(consumed_at.is_none());

    let unknown = callbacks::consume(&mut conn, "t:AAAAAAAAAAAAAAAA", 20, now)
        .await
        .expect("consume");
    assert!(matches!(unknown, Consumed::Unknown));
    let garbage = callbacks::consume(&mut conn, "что-то-ещё", 20, now)
        .await
        .expect("consume");
    assert!(matches!(garbage, Consumed::Unknown));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lifetime_cannot_exceed_24_hours() {
    let db = db().await;
    let mut conn = db.pool.acquire().await.expect("conn");
    callbacks::issue_many(&mut conn, &[spec(30, None)], Utc::now())
        .await
        .expect("issue");

    let result = sqlx::query(
        "INSERT INTO callback_tokens (token_hash, telegram_user_id, action, expires_at, created_at)
         VALUES ('x', 1, 'go', now() + INTERVAL '25 hours', now())",
    )
    .execute(&db.pool)
    .await;

    assert!(
        result.is_err(),
        "database must reject a lifetime above 24 hours"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parallel_consumption_executes_exactly_once() {
    let db = db().await;
    let now = Utc::now();
    let mut conn = db.pool.acquire().await.expect("conn");
    let tokens = callbacks::issue_many(&mut conn, &[spec(40, None)], now)
        .await
        .expect("issue");

    let mut handles = Vec::new();
    for _ in 0..8 {
        let pool = db.pool.clone();
        let token = tokens[0].clone();
        handles.push(tokio::spawn(async move {
            let mut conn = pool.acquire().await.expect("conn");
            callbacks::consume(&mut conn, &token, 40, now)
                .await
                .expect("consume")
        }));
    }
    let mut valid = 0;
    for handle in handles {
        if matches!(handle.await.expect("join"), Consumed::Valid(_)) {
            valid += 1;
        }
    }

    assert_eq!(valid, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_tokens_are_purged() {
    let db = db().await;
    let now = Utc::now();
    let mut conn = db.pool.acquire().await.expect("conn");
    let tokens = callbacks::issue_many(&mut conn, &[spec(50, None)], now - Duration::days(3))
        .await
        .expect("issue");

    let purged = callbacks::purge_expired(&mut conn, now, now)
        .await
        .expect("purge");

    assert!(purged >= 1);
    let left: i64 =
        sqlx::query_scalar("SELECT count(*) FROM callback_tokens WHERE token_hash = $1")
            .bind(hash_token(&tokens[0]))
            .fetch_one(&db.pool)
            .await
            .expect("count");
    assert_eq!(left, 0);
}

/// Токен области `notice` с действием `action` и полезной нагрузкой действия.
fn notice_spec(user: i64, action: &Action) -> NewToken {
    NewToken {
        telegram_user_id: user,
        household_id: None,
        scope: Scope::Notice,
        action: action.name(),
        payload: serde_json::to_value(action).expect("payload"),
        version: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_dismiss_tokens_are_kept_until_the_outbox_cutoff() {
    // Отдельная база: общий purge удалил бы истёкшие токены параллельных тестов.
    let db = isolated_db().await;
    let now = Utc::now();
    let mut conn = db.pool.acquire().await.expect("conn");
    // Отсечка outbox — 48 часов назад (нижняя граница хранения outbox).
    let outbox_cutoff = now - Duration::hours(48);

    let close = Action::NoticeClose;
    let complete = Action::NoticeComplete {
        item_id: uuid::Uuid::now_v7(),
        version: 1,
    };
    // «🗑 Убрать», выпущенный после отсечки outbox (30 часов назад), остаётся;
    // выпущенный до неё (50 часов назад) — удаляется. Истёкшие токен экрана и
    // «✓ Выполнено» удаляются как раньше.
    let recent_dismiss = callbacks::issue_many(
        &mut conn,
        &[notice_spec(70, &close)],
        now - Duration::hours(30),
    )
    .await
    .expect("issue recent dismiss")
    .remove(0);
    let old_dismiss = callbacks::issue_many(
        &mut conn,
        &[notice_spec(71, &close)],
        now - Duration::hours(50),
    )
    .await
    .expect("issue old dismiss")
    .remove(0);
    let screen = callbacks::issue_many(&mut conn, &[spec(72, None)], now - Duration::hours(30))
        .await
        .expect("issue screen")
        .remove(0);
    let notice_complete = callbacks::issue_many(
        &mut conn,
        &[notice_spec(73, &complete)],
        now - Duration::hours(30),
    )
    .await
    .expect("issue notice complete")
    .remove(0);

    callbacks::purge_expired(&mut conn, now, outbox_cutoff)
        .await
        .expect("purge");

    for (token, remains) in [
        (&recent_dismiss, true),
        (&old_dismiss, false),
        (&screen, false),
        (&notice_complete, false),
    ] {
        let left: i64 =
            sqlx::query_scalar("SELECT count(*) FROM callback_tokens WHERE token_hash = $1")
                .bind(hash_token(token))
                .fetch_one(&db.pool)
                .await
                .expect("count");
        assert_eq!(left, i64::from(remains), "token {token} remains={remains}");
    }
}

// 0009-events 9.2: устаревшие кнопки карточки события отказывают и не меняют
// событие.

/// Стенд владельца household и событие в его календаре: заготовка для
/// устаревших кнопок 9.2.
async fn event_stand(pool: &sqlx::PgPool) -> (Harness, Item, uuid::Uuid) {
    let household = create_household(pool).await;
    let telegram_id: i64 = sqlx::query_scalar("SELECT telegram_user_id FROM users WHERE id = $1")
        .bind(household.owner_id)
        .fetch_one(pool)
        .await
        .expect("owner");
    let bot = Harness::new(pool, telegram_id);
    let now = bot.now;
    let mut input = new_item(household.events_list_id, "Маникюр");
    input.due_at = Some(now + Duration::hours(2));
    input.end_at = Some(now + Duration::hours(3));
    let event = create_item(
        pool,
        &household,
        household.events_list_id,
        &household.owner(),
        input,
    )
    .await;
    (bot, event, household.household_id)
}

/// Callback-токен на действие, которого на экране уже нет.
async fn action_token(
    pool: &sqlx::PgPool,
    telegram_id: i64,
    household_id: uuid::Uuid,
    action: &Action,
    now: chrono::DateTime<Utc>,
) -> String {
    let mut conn = pool.acquire().await.expect("conn");
    callbacks::issue_many(
        &mut conn,
        &[NewToken {
            telegram_user_id: telegram_id,
            household_id: Some(household_id),
            scope: Scope::Screen,
            action: action.name(),
            payload: serde_json::to_value(action).expect("payload"),
            version: None,
        }],
        now,
    )
    .await
    .expect("token")
    .remove(0)
}

/// Событие не изменилось: ни состояние, ни сроки, ни версия.
async fn assert_event_unchanged(pool: &sqlx::PgPool, event: &Item) {
    let (status, due_at, end_at, version): (
        String,
        Option<chrono::DateTime<Utc>>,
        Option<chrono::DateTime<Utc>>,
        i32,
    ) = sqlx::query_as("SELECT status::text, due_at, end_at, version FROM items WHERE id = $1")
        .bind(event.id)
        .fetch_one(pool)
        .await
        .expect("event");
    assert_eq!((status.as_str(), version), ("open", event.version));
    assert_eq!((due_at, end_at), (event.due_at, event.end_at));
}

/// Экран, сохранённый за чатом: `t` — вид активного экрана.
async fn saved_screen_kind(pool: &sqlx::PgPool, chat_id: i64) -> String {
    let state: serde_json::Value =
        sqlx::query_scalar("SELECT state FROM telegram_views WHERE chat_id = $1")
            .bind(chat_id)
            .fetch_one(pool)
            .await
            .expect("saved screen");
    state["t"].as_str().expect("screen kind").to_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_complete_callback_on_an_event_is_refused_and_changes_nothing() {
    let db = db().await;
    let (mut bot, event, household_id) = event_stand(&db.pool).await;
    let action = Action::Complete {
        item_id: event.id,
        version: event.version,
        back: Screen::Agenda {
            list: None,
            page: 0,
        },
    };
    let token = action_token(&db.pool, bot.chat, household_id, &action, bot.now).await;

    bot.press_data(&token, 1).await;

    assert_event_unchanged(&db.pool, &event).await;
    let completions: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM item_events WHERE item_id = $1 AND event_type = 'completed'",
    )
    .bind(event.id)
    .fetch_one(&db.pool)
    .await
    .expect("completions");
    assert_eq!(completions, 0, "an event must not be completed");
    assert!(
        bot.fake.answers().iter().any(|(_, text, alert)| {
            *alert && text.as_deref() == Some("Событие нельзя выполнить.")
        }),
        "the stale button must be refused with an alert: {:?}",
        bot.fake.answers()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_without_due_callback_on_an_event_keeps_the_start() {
    let db = db().await;
    let (mut bot, event, household_id) = event_stand(&db.pool).await;
    let action = Action::DueSet {
        item_id: event.id,
        version: event.version,
        due: QuickDue::None,
        back: Screen::Agenda {
            list: None,
            page: 0,
        },
    };
    let token = action_token(&db.pool, bot.chat, household_id, &action, bot.now).await;

    bot.press_data(&token, 1).await;

    assert_event_unchanged(&db.pool, &event).await;
    assert_eq!(
        saved_screen_kind(&db.pool, bot.chat).await,
        "item",
        "the stale «Без срока» only reopens the event card"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_reopen_callback_on_an_event_is_refused_and_changes_nothing() {
    let db = db().await;
    let (mut bot, event, household_id) = event_stand(&db.pool).await;
    let action = Action::Reopen {
        item_id: event.id,
        version: event.version,
        back: Screen::Agenda {
            list: None,
            page: 0,
        },
        completion_id: None,
    };
    let token = action_token(&db.pool, bot.chat, household_id, &action, bot.now).await;

    bot.press_data(&token, 1).await;

    assert_event_unchanged(&db.pool, &event).await;
    assert!(
        bot.fake.answers().iter().any(|(_, text, alert)| {
            *alert && text.as_deref() == Some("Событие нельзя вернуть в работу.")
        }),
        "the stale button must be refused with an alert: {:?}",
        bot.fake.answers()
    );
}

// 0017-notification-cleanup 2.3: «🗑 Убрать» работает и после истечения токена.

/// Доставленное напоминание о задаче: стенд для кнопок уведомления.
async fn reminder_stand(pool: &sqlx::PgPool) -> (Harness, Item, i64, String) {
    let mut bot = support::notifications::onboarded(pool).await;
    support::notifications::quiet_off(pool, bot.chat).await;
    let household = support::notifications::household_for(pool, bot.chat).await;
    let due = bot.now + Duration::hours(3);
    let item =
        support::notifications::create_task(pool, &household, "Оплатить интернет", due, None).await;
    support::notifications::tick_and_deliver(&mut bot, pool, due - Duration::minutes(15)).await;
    let (message_id, html) = bot.latest();
    (bot, item, message_id, html)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_dismiss_in_a_reminder_deletes_the_message() {
    let db = isolated_db().await;
    let (mut bot, item, message_id, html) = reminder_stand(&db.pool).await;
    let dismiss = support::notifications::button_of(&html, "🗑 Убрать");
    let before = support::notifications::item_status(&db.pool, item.id).await;
    let calls_before = bot.fake.calls().len();

    // Кнопки устарели: токен выпущен 30 часов назад.
    bot.advance(Duration::hours(30));
    bot.press_data_pending(&dismiss, message_id).await;

    // Удаление уборки своего сообщения поставлено, payload цел.
    let payload: serde_json::Value = sqlx::query_scalar(
        "SELECT payload FROM outbox
         WHERE chat_id = $1 AND kind = 'delete' AND dedupe_key = $2",
    )
    .bind(bot.chat)
    .bind(format!("delete:{}:{message_id}", bot.chat))
    .fetch_one(&db.pool)
    .await
    .expect("cleanup delete row");
    assert_eq!(payload["message_id"], json!(message_id), "{payload}");
    assert_eq!(payload["cleanup"], json!(true), "{payload}");

    // Доставка отмечена убранной, статус прежний.
    let (status, dismissed_at): (String, Option<DateTime<Utc>>) = sqlx::query_as(
        "SELECT status::text, dismissed_at FROM notification_deliveries
         WHERE item_id = $1 AND kind = 'reminder'",
    )
    .bind(item.id)
    .fetch_one(&db.pool)
    .await
    .expect("reminder delivery");
    assert_eq!(status, "sent");
    assert!(
        dismissed_at.is_some(),
        "«🗑 Убрать» must mark the delivery dismissed"
    );

    // Тоста «устарело» нет.
    assert!(
        !bot.fake
            .answers()
            .iter()
            .any(|(_, text, _)| text.as_deref().is_some_and(|t| t.contains("устарело"))),
        "the expired dismiss must run without the stale toast: {:?}",
        bot.fake.answers()
    );

    bot.deliver().await;
    assert!(
        !bot.fake
            .messages(bot.chat)
            .into_iter()
            .any(|(id, _)| id == message_id),
        "the reminder must disappear from the chat"
    );
    // Экран навигации не перерисован: после нажатия только удаление.
    let calls = bot.fake.calls();
    assert!(
        calls[calls_before..]
            .iter()
            .all(|call| matches!(call, Call::Delete { .. })),
        "the navigation screen must not be redrawn: {:?}",
        &calls[calls_before..]
    );
    assert_eq!(
        support::notifications::item_status(&db.pool, item.id).await,
        before,
        "«🗑 Убрать» must not change the task"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_complete_in_a_reminder_is_refused_as_before() {
    let db = isolated_db().await;
    let (mut bot, item, message_id, html) = reminder_stand(&db.pool).await;
    let complete = support::notifications::button_of(&html, "Выполнено");
    let before = support::notifications::item_status(&db.pool, item.id).await;
    let calls_before = bot.fake.calls().len();

    bot.advance(Duration::hours(30));
    bot.press_data(&complete, message_id).await;

    assert!(
        bot.fake.answers().iter().any(|(_, text, _)| {
            text.as_deref() == Some("Это действие устарело. Открываю актуальный экран.")
        }),
        "the expired complete must keep the stale toast: {:?}",
        bot.fake.answers()
    );
    assert_eq!(
        support::notifications::item_status(&db.pool, item.id).await,
        before,
        "the expired complete must not change the task"
    );
    // Удаления нет: ни строки outbox, ни вызова Telegram; сообщение живо.
    let deletes: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM outbox
         WHERE chat_id = $1 AND kind = 'delete' AND dedupe_key = $2",
    )
    .bind(bot.chat)
    .bind(format!("delete:{}:{message_id}", bot.chat))
    .fetch_one(&db.pool)
    .await
    .expect("delete rows");
    assert_eq!(deletes, 0);
    let calls = bot.fake.calls();
    assert!(
        calls[calls_before..]
            .iter()
            .all(|call| !matches!(call, Call::Delete { .. })),
        "the expired complete must not delete anything: {:?}",
        &calls[calls_before..]
    );
    assert!(
        bot.fake
            .messages(bot.chat)
            .into_iter()
            .any(|(id, _)| id == message_id),
        "the reminder must stay in the chat"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreign_expired_dismiss_is_refused() {
    let db = isolated_db().await;
    let (mut bot, _item, message_id, html) = reminder_stand(&db.pool).await;
    let dismiss = support::notifications::button_of(&html, "🗑 Убрать");
    let mut other =
        Harness::sharing(&db.pool, next_telegram_id(), bot.fake.clone()).named("Mallory");

    bot.advance(Duration::hours(30));
    other.set_now(bot.now);
    other.press_data(&dismiss, message_id).await;

    assert!(
        other.fake.answers().iter().any(|(_, text, alert)| {
            *alert && text.as_deref() == Some("Эта кнопка предназначена другому пользователю.")
        }),
        "a foreign expired token must be refused with an alert: {:?}",
        other.fake.answers()
    );
    let deletes: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM outbox
         WHERE chat_id = $1 AND kind = 'delete' AND dedupe_key = $2",
    )
    .bind(bot.chat)
    .bind(format!("delete:{}:{message_id}", bot.chat))
    .fetch_one(&db.pool)
    .await
    .expect("delete rows");
    assert_eq!(deletes, 0, "a foreign press must not enqueue a delete");
    assert!(
        bot.fake
            .messages(bot.chat)
            .into_iter()
            .any(|(id, _)| id == message_id),
        "the reminder must stay in the chat"
    );
}
