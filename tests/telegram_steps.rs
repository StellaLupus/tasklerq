//! Шаг обработки в savepoint: `Fail::User` не оставляет частичных записей.

mod support;

use chrono::Utc;
use serde_json::json;
use sqlx::PgConnection;
use support::telegram::Harness;
use support::{create_household, db, next_telegram_id};
use tasklerq::storage::repositories::{households, users};
use tasklerq::telegram::bot::{Ctx, Fail};
use tasklerq::telegram::outbox::{self, Outgoing, Placement};
use tasklerq::telegram::update::Sender;
use tasklerq::telegram::views;

fn ctx(conn: &mut PgConnection, chat: i64) -> Ctx<'_> {
    Ctx {
        conn,
        now: Utc::now(),
        bot_username: "tasklerqbot",
        chat_id: chat,
        sender: Sender {
            id: chat,
            display_name: "Ann".into(),
            username: None,
        },
        user: None,
        household: None,
        tz: chrono_tz::UTC,
        toast: None,
        banner: None,
        placement: Placement::Visible,
        llm_capture: false,
        audit: None,
    }
}

/// Записать эффект шага: форму и сообщение в outbox.
async fn write_effects(ctx: &mut Ctx<'_>) {
    views::put_conversation(
        &mut *ctx.conn,
        ctx.chat_id,
        ctx.sender.id,
        "search",
        &json!({}),
        ctx.now,
    )
    .await
    .expect("form");
    outbox::enqueue(
        &mut *ctx.conn,
        ctx.chat_id,
        &Outgoing::Message {
            html: "<p>эффект</p>".into(),
            rich: false,
        },
        None,
        ctx.now,
    )
    .await
    .expect("outbox");
}

async fn effects(conn: &mut PgConnection, chat: i64) -> (i64, i64) {
    let forms = sqlx::query_scalar("SELECT count(*) FROM conversation_states WHERE chat_id = $1")
        .bind(chat)
        .fetch_one(&mut *conn)
        .await
        .expect("forms");
    let messages = sqlx::query_scalar("SELECT count(*) FROM outbox WHERE chat_id = $1")
        .bind(chat)
        .fetch_one(&mut *conn)
        .await
        .expect("messages");
    (forms, messages)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn user_failure_rolls_back_the_step_but_keeps_earlier_writes() {
    let db = db().await;
    let chat = next_telegram_id();
    let mut tx = db.pool.begin().await.expect("tx");
    let mut ctx = ctx(&mut tx, chat);

    // Запись до шага (например, потреблённый токен) остаётся.
    sqlx::query("INSERT INTO telegram_views (chat_id, view_key) VALUES ($1, 'before')")
        .bind(chat)
        .execute(&mut *ctx.conn)
        .await
        .expect("before");

    let result = ctx
        .in_step(|ctx| {
            Box::pin(async move {
                write_effects(ctx).await;
                Err(Fail::User("нельзя".into()))
            })
        })
        .await;

    assert!(matches!(result, Err(Fail::User(message)) if message == "нельзя"));
    assert_eq!(effects(&mut tx, chat).await, (0, 0));
    let kept: i64 = sqlx::query_scalar("SELECT count(*) FROM telegram_views WHERE chat_id = $1")
        .bind(chat)
        .fetch_one(&mut *tx)
        .await
        .expect("kept");
    assert_eq!(kept, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn successful_step_keeps_its_writes() {
    let db = db().await;
    let chat = next_telegram_id();
    let mut tx = db.pool.begin().await.expect("tx");
    let mut ctx = ctx(&mut tx, chat);

    ctx.in_step(|ctx| {
        Box::pin(async move {
            write_effects(ctx).await;
            Ok(())
        })
    })
    .await
    .expect("step");

    assert_eq!(effects(&mut tx, chat).await, (1, 1));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transaction_stays_usable_after_a_database_error_inside_a_user_failure() {
    let db = db().await;
    let chat = next_telegram_id();
    let mut tx = db.pool.begin().await.expect("tx");
    let mut ctx = ctx(&mut tx, chat);

    ctx.in_step(|ctx| {
        Box::pin(async move {
            write_effects(ctx).await;
            // Ошибка PostgreSQL переводит транзакцию в «прерванное» состояние.
            let broken = sqlx::query("SELECT 1 / 0").execute(&mut *ctx.conn).await;
            assert!(broken.is_err());
            Err(Fail::User("не вышло".into()))
        })
    })
    .await
    .expect_err("user failure is returned");

    // Показ объяснения и экрана после сбоя возможен: транзакция жива.
    write_effects(&mut ctx).await;
    assert_eq!(effects(&mut tx, chat).await, (1, 1));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_form_step_answer_is_deleted_and_the_next_step_is_shown() {
    let db = db().await;
    let mut bot = Harness::new(&db.pool, next_telegram_id()).named("Ann");
    bot.text("/start").await;
    bot.text("Семья").await;
    bot.press("Москва").await;
    bot.press("меню").await;
    bot.press("Добавить").await;
    bot.press("Задачу").await;
    bot.assert_screen_contains("Как назвать задачу?");

    let update = bot.text_update("Ann", "Позвонить маме");
    let message_id = update["message"]["message_id"]
        .as_i64()
        .expect("incoming message id");
    bot.enqueue(update).await;
    bot.pump().await;

    let deleted: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM outbox
         WHERE chat_id = $1 AND kind = 'delete'
           AND coalesce((payload->>'message_id')::bigint, message_id) = $2",
    )
    .bind(bot.chat)
    .bind(message_id)
    .fetch_one(&db.pool)
    .await
    .expect("delete row");
    assert_eq!(deleted, 1, "the form answer must be deleted");
    bot.assert_screen_contains("Когда сделать?");
    bot.assert_screen_contains("Название: Позвонить маме");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_second_text_step_value_is_visible_and_escaped_on_the_next_step() {
    let db = db().await;
    let mut bot = Harness::new(&db.pool, next_telegram_id()).named("Ann");
    bot.text("/start").await;
    bot.text("Семья").await;
    bot.press("Москва").await;
    bot.press("меню").await;
    bot.press("Добавить").await;
    bot.press("Задачу").await;
    bot.assert_screen_contains("Как назвать задачу?");
    // На первом шаге ничего не принято: сводки нет.
    assert!(
        !bot.screen().contains("Название:"),
        "the first step should not show a summary:\n{}",
        bot.screen()
    );

    bot.text("Позвонить <маме> & папе").await;
    bot.assert_screen_contains("Когда сделать?");
    bot.assert_screen_contains("Название: Позвонить &lt;маме&gt; &amp; папе");

    bot.press("Дата и время").await;
    bot.text("завтра 18:30").await;
    bot.assert_screen_contains("Какой приоритет?");
    bot.assert_screen_contains("Срок: Завтра, 18:30");
    bot.assert_screen_contains("Название: Позвонить &lt;маме&gt; &amp; папе");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn user_failure_restores_member_and_timezone_in_the_context() {
    let db = db().await;
    let home = create_household(&db.pool).await;
    let mut tx = db.pool.begin().await.expect("tx");
    let user = users::get(&mut tx, home.household_id, home.owner_id)
        .await
        .expect("user");
    let household = households::get(&mut tx, home.household_id)
        .await
        .expect("household");
    let mut ctx = ctx(&mut tx, next_telegram_id());
    ctx.user = Some(user);
    ctx.household = Some(household);
    ctx.tz = chrono_tz::Europe::Moscow;

    // Как в «выходе из пространства»: шаг обнуляет участника и падает позже.
    ctx.in_step(|ctx| {
        Box::pin(async move {
            ctx.user = None;
            ctx.household = None;
            ctx.tz = chrono_tz::UTC;
            Err(Fail::User("не вышло".into()))
        })
    })
    .await
    .expect_err("user failure is returned");

    assert_eq!(ctx.user.as_ref().map(|user| user.id), Some(home.owner_id));
    assert_eq!(
        ctx.household.as_ref().map(|household| household.id),
        Some(home.household_id)
    );
    assert_eq!(ctx.tz, chrono_tz::Europe::Moscow);
}

/// Onboarding до главного меню.
async fn onboarded(pool: &sqlx::PgPool) -> Harness {
    let mut bot = Harness::new(pool, next_telegram_id()).named("Ann");
    bot.text("/start").await;
    bot.text("Семья").await;
    bot.press("Москва").await;
    bot.press("меню").await;
    bot
}

/// Household стенда с идентификаторами списков по умолчанию.
async fn stand_household(pool: &sqlx::PgPool, chat: i64) -> support::TestHousehold {
    let (household_id, owner_id): (uuid::Uuid, uuid::Uuid) =
        sqlx::query_as("SELECT household_id, id FROM users WHERE telegram_user_id = $1")
            .bind(chat)
            .fetch_one(pool)
            .await
            .expect("household");
    let list = |kind: &'static str| {
        sqlx::query_scalar(
            "SELECT id FROM lists WHERE household_id = $1 AND kind::text = $2
             ORDER BY is_default DESC, created_at LIMIT 1",
        )
        .bind(household_id)
        .bind(kind)
        .fetch_one(pool)
    };
    support::TestHousehold {
        household_id,
        owner_id,
        member_id: owner_id,
        tasks_list_id: list("tasks").await.expect("default tasks list"),
        shopping_list_id: list("shopping").await.expect("default shopping list"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edit_form_summary_shows_the_shopping_quantity() {
    let db = db().await;
    let mut bot = onboarded(&db.pool).await;
    let household = stand_household(&db.pool, bot.chat).await;
    let mut item = support::new_item(household.shopping_list_id, "Молоко");
    item.quantity = Some(2.0);
    item.unit = Some("л".into());
    support::create_item(
        &db.pool,
        &household,
        household.shopping_list_id,
        &household.owner(),
        item,
    )
    .await;

    bot.press("Покупки").await;
    bot.press("⋯").await;
    bot.press("Количество").await;

    bot.assert_screen_contains("Сколько?");
    bot.assert_screen_contains("Название: Молоко");
    bot.assert_screen_contains("Количество: 2 л");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edit_form_summary_shows_a_unit_without_quantity() {
    let db = db().await;
    let mut bot = onboarded(&db.pool).await;
    let household = stand_household(&db.pool, bot.chat).await;
    let mut item = support::new_item(household.shopping_list_id, "Хлеб");
    item.unit = Some("уп".into());
    let created = support::create_item(
        &db.pool,
        &household,
        household.shopping_list_id,
        &household.owner(),
        item,
    )
    .await;
    // Старая или внешняя позиция без количества: сводка должна показать единицу.
    sqlx::query("UPDATE items SET quantity = NULL WHERE id = $1")
        .bind(created.id)
        .execute(&db.pool)
        .await
        .expect("quantity without number");

    bot.press("Покупки").await;
    bot.press("⋯").await;
    bot.press("Единица").await;

    bot.assert_screen_contains("В чём измерять?");
    bot.assert_screen_contains("Название: Хлеб");
    bot.assert_screen_contains("Единица: уп");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_shopping_summary_keeps_the_chosen_quantity_and_unit() {
    let db = db().await;
    let mut bot = onboarded(&db.pool).await;

    bot.press("Добавить").await;
    bot.press("Покупку").await;
    bot.text("Молоко").await;
    bot.press("2").await;
    bot.assert_screen_contains("Количество: 2");
    bot.press("л").await;

    bot.assert_screen_contains("Количество: 2 л");
}
