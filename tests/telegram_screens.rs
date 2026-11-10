//! Экраны покупок, поиска, настроек, семьи и уведомлений.

mod support;

use std::sync::Arc;

use axum::http::{Method, StatusCode};
use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveTime, TimeZone, Utc};
use sqlx::PgPool;
use support::telegram::{Call, FakeTelegram, Harness, button_rows};
use support::{TestHousehold, create_item, db, new_item, next_telegram_id};
use tasklerq::admin;
use tasklerq::config::TenancyMode;
use tasklerq::domain::error::DomainError;
use tasklerq::domain::models::{
    Actor, Field, ItemPatch, ItemPriority, ItemStatus, ListKind, RecurrenceFreq, UserRole,
};
use tasklerq::domain::recurrence::Due;
use tasklerq::domain::settings::NotificationSettings;
use tasklerq::jobs::scheduler::Scheduler;
use tasklerq::storage::repositories::{items, lists};
use tasklerq::telegram::bot::Bot;
use tasklerq::telegram::bot::model::{Action, Form, PAGE_SIZE, QuickDue, Screen, Ui};
use tasklerq::telegram::callbacks::{self, NewToken, Scope};
use tokio::sync::Notify;
use uuid::Uuid;

async fn onboarded_named(pool: &PgPool, name: &str) -> Harness {
    let mut bot = Harness::new(pool, next_telegram_id()).named(name);
    bot.text("/start").await;
    bot.text("Семья").await;
    bot.press("Москва").await;
    bot.press("меню").await;
    bot
}

/// Онбординг на стенде с включённым разбором текста.
async fn onboarded_with_llm(pool: &PgPool, name: &str) -> Harness {
    let handler = Bot::new("tasklerqbot")
        .with_llm_capture(true)
        .with_tenancy(TenancyMode::Multi);
    let mut bot = Harness::with_handler(
        pool,
        next_telegram_id(),
        Arc::new(handler),
        FakeTelegram::new(),
    )
    .named(name);
    bot.text("/start").await;
    bot.text("Семья").await;
    bot.press("Москва").await;
    bot.press("меню").await;
    bot
}

/// Онбординг на зафиксированном моменте: токены кнопок создаются уже в нём.
async fn onboarded_at(pool: &PgPool, name: &str, now: DateTime<Utc>) -> Harness {
    let mut bot = Harness::new(pool, next_telegram_id()).named(name);
    bot.set_now(now);
    bot.text("/start").await;
    bot.text("Семья").await;
    bot.press("Москва").await;
    bot.press("меню").await;
    bot
}

async fn household_for(pool: &PgPool, telegram_id: i64) -> TestHousehold {
    let (household_id, owner_id): (Uuid, Uuid) = sqlx::query_as(
        "SELECT household_id, id FROM users WHERE telegram_user_id = $1 AND deleted_at IS NULL",
    )
    .bind(telegram_id)
    .fetch_one(pool)
    .await
    .expect("user");
    let list = |kind: &'static str| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, Uuid>(
                "SELECT id FROM lists WHERE household_id = $1 AND kind = $2::list_kind AND is_default",
            )
            .bind(household_id)
            .bind(kind)
            .fetch_one(&pool)
            .await
            .expect("list")
        }
    };
    TestHousehold {
        household_id,
        owner_id,
        member_id: owner_id,
        tasks_list_id: list("tasks").await,
        shopping_list_id: list("shopping").await,
        events_list_id: list("events").await,
    }
}

async fn notification_settings(pool: &PgPool, telegram_id: i64) -> NotificationSettings {
    let value: serde_json::Value =
        sqlx::query_scalar("SELECT settings FROM users WHERE telegram_user_id = $1")
            .bind(telegram_id)
            .fetch_one(pool)
            .await
            .expect("settings");
    NotificationSettings::from_settings(&value)
}

async fn outbox_html(pool: &PgPool, chat_id: i64) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT payload->>'html' FROM outbox
         WHERE chat_id = $1 AND payload ? 'html'
         ORDER BY seq",
    )
    .bind(chat_id)
    .fetch_all(pool)
    .await
    .expect("outbox messages")
}

fn scheduler(pool: &PgPool) -> Scheduler {
    Scheduler::new(pool.clone(), Arc::new(Notify::new()))
}

fn local_utc(timezone: chrono_tz::Tz, date: NaiveDate, hour: u32, minute: u32) -> DateTime<Utc> {
    timezone
        .with_ymd_and_hms(date.year(), date.month(), date.day(), hour, minute, 0)
        .single()
        .expect("local time")
        .with_timezone(&Utc)
}

/// Пригласить участника: возвращает его стенд, уже присоединившийся.
async fn invite(owner: &mut Harness, pool: &PgPool, name: &str) -> Harness {
    owner.press("Настройки").await;
    owner.press("Семья").await;
    owner.press("Пригласить").await;
    owner.press("Создать приглашение").await;
    let screen = owner.screen();
    let marker = "start=";
    let start = screen.find(marker).expect("invite link") + marker.len();
    let code: String = screen[start..]
        .chars()
        .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '-' || *ch == '_')
        .collect();

    let mut member = Harness::sharing(pool, next_telegram_id(), owner.fake.clone()).named(name);
    member.text(&format!("/start {code}")).await;

    // Владелец возвращается в меню; уведомление о новом участнике доставлено.
    owner.press("К участникам").await;
    owner.press("Назад").await;
    owner.press("Назад").await;
    owner.deliver().await;
    member
}

/// Выпустить токен путём CLI (административный слой) в household теста.
async fn cli_token(
    pool: &PgPool,
    household_id: Uuid,
    name: &str,
) -> tasklerq::auth::tokens::IssuedToken {
    admin::create_token(
        pool,
        TenancyMode::Single,
        Some(household_id),
        name,
        &["ha:read".to_owned(), "ha:write".to_owned()],
        None,
        Utc::now(),
    )
    .await
    .expect("token should be issued")
}

/// Callback-токен на произвольное действие: так проверяются устаревшие и
/// подделанные кнопки, которых на экране уже нет.
async fn action_callback(
    pool: &PgPool,
    telegram_id: i64,
    household_id: Uuid,
    action: Action,
) -> String {
    let mut conn = pool.acquire().await.expect("connection");
    callbacks::issue_many(
        &mut conn,
        &[NewToken {
            telegram_user_id: telegram_id,
            household_id: Some(household_id),
            scope: Scope::Screen,
            action: action.name(),
            payload: serde_json::to_value(&action).expect("action payload"),
            version: None,
        }],
        Utc::now(),
    )
    .await
    .expect("callback should be issued")
    .remove(0)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn banner_with_a_button_is_shown_once_and_never_persisted() {
    let db = db().await;
    let mut bot = onboarded_named(&db.pool, "Alice").await;

    support::telegram::present_with_banner(
        &db.pool,
        bot.chat,
        bot.now,
        &Screen::Menu,
        Ui::new().heading("Экран под баннером"),
        "✅ Выполнено: Оплатить интернет",
        vec![tasklerq::telegram::rich::button(
            "↩️ Отменить",
            Action::Refresh,
        )],
    )
    .await;
    bot.deliver().await;

    let html = bot.screen();
    assert!(
        html.contains("✅ Выполнено: Оплатить интернет"),
        "banner should be visible:\n{html}"
    );
    assert!(
        support::telegram::buttons(&html)
            .iter()
            .any(|(label, _)| label.contains("Отменить")),
        "banner button should be visible:\n{html}"
    );

    let state: String =
        sqlx::query_scalar("SELECT state::text FROM telegram_views WHERE chat_id = $1")
            .bind(bot.chat)
            .fetch_one(&db.pool)
            .await
            .expect("saved screen state");
    assert!(
        !state.contains("Оплатить интернет"),
        "banner text must not be persisted: {state}"
    );

    // Кнопка баннера доходит до действия, а следующая отрисовка баннера не
    // показывает: он живёт только до ближайшего экрана.
    bot.press("Отменить").await;
    let html = bot.screen();
    assert!(!html.contains("Оплатить интернет"), "{html}");
    assert!(
        !html.contains("Отменить"),
        "the banner button must disappear with the banner:\n{html}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shopping_flow_covers_duplicates_purchase_and_clear() {
    let db = db().await;
    let mut bot = onboarded_named(&db.pool, "Alice").await;

    bot.press("Покупки").await;
    bot.press("Добавить").await;
    bot.assert_screen_contains("Что купить?");
    bot.text("Молоко").await;
    bot.assert_screen_contains("Количество: 1 шт");
    bot.press("Количество").await;
    bot.press("2").await;
    bot.assert_screen_contains("Количество: 2 шт");
    bot.press("Единица").await;
    bot.press("л").await;
    bot.assert_screen_contains("Количество: 2 л");
    bot.press("Сохранить").await;
    bot.assert_screen_contains("Покупки · 1");
    bot.assert_screen_contains("☐ Молоко · 2 л");

    // Дубликат: предложить увеличить количество.
    bot.press("Добавить").await;
    bot.text("молоко").await;
    bot.press("Количество").await;
    bot.press("1").await;
    bot.press("Единица").await;
    bot.press("л").await;
    bot.press("Сохранить").await;
    bot.assert_screen_contains("уже есть в списке (2 л)");
    bot.press("Увеличить до 3 л").await;
    bot.assert_screen_contains("☐ Молоко · 3 л");
    assert_eq!(bot.screen().matches("Молоко").count(), 1);

    // Отметка и возврат.
    bot.press("Молоко").await;
    bot.assert_screen_contains("Куплено");
    bot.assert_screen_contains("☑ Молоко");
    bot.press("Очистить").await;
    bot.assert_screen_contains("Убрано: 1 позиция");
    bot.press("Отменить").await;
    bot.assert_screen_contains("☑ Молоко");
    bot.press("☑ Молоко").await;
    bot.assert_screen_contains("☐ Молоко");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_rows_have_one_button_and_open_the_card_without_changing_the_task() {
    let db = db().await;
    let mut bot = onboarded_named(&db.pool, "Alice").await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);

    let mut urgent = new_item(household.tasks_list_id, "Срочная задача");
    urgent.priority = ItemPriority::Urgent;
    urgent.assignee_user_id = Some(household.owner_id);
    let urgent = create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &actor,
        urgent,
    )
    .await;

    let mut high = new_item(household.tasks_list_id, "Высокая задача");
    high.priority = ItemPriority::High;
    create_item(&db.pool, &household, household.tasks_list_id, &actor, high).await;

    let normal = create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &actor,
        new_item(household.tasks_list_id, "Обычная задача"),
    )
    .await;

    create_item(
        &db.pool,
        &household,
        household.shopping_list_id,
        &actor,
        new_item(household.shopping_list_id, "Молоко"),
    )
    .await;

    bot.press("Задачи").await;
    let buttons = support::telegram::buttons(&bot.screen());
    let labels: Vec<&str> = buttons.iter().map(|(label, _)| label.as_str()).collect();
    assert!(
        !labels.iter().any(|label| *label == "☐" || *label == "⋯"),
        "element rows must not keep service buttons: {labels:?}"
    );
    for title in ["Срочная задача", "Высокая задача", "Обычная задача"]
    {
        let rows: Vec<&(String, String)> = buttons
            .iter()
            .filter(|(label, _)| label.contains(title))
            .collect();
        assert_eq!(rows.len(), 1, "one full-width button per row: {title}");
        assert!(
            !rows[0].0.contains("👤"),
            "the row must not show the assignee: {}",
            rows[0].0
        );
    }
    let row = |title: &str| {
        buttons
            .iter()
            .find(|(label, _)| label.contains(title))
            .map(|(label, _)| label.as_str())
            .expect("row button")
    };
    assert!(row("Срочная задача").contains("🔴 срочно"), "{buttons:?}");
    assert!(row("Высокая задача").contains("🟠 высокий"), "{buttons:?}");
    assert!(!row("Обычная задача").contains("🔴"), "{buttons:?}");
    assert!(!row("Обычная задача").contains("🟠"), "{buttons:?}");

    let before = item_state(&db.pool, urgent.id).await;
    bot.press("Срочная задача").await;
    assert!(
        bot.screen().contains("<h3>Задачи › Срочная задача</h3>"),
        "the row must open the task card:\n{}",
        bot.screen()
    );
    assert_eq!(
        item_state(&db.pool, urgent.id).await,
        before,
        "opening the card must not change the task"
    );

    // «Мои задачи» и поиск используют ту же строку-кнопку.
    bot.press("Назад").await; // список
    bot.press("Назад").await; // меню
    bot.press("Мои задачи").await;
    assert_one_element_row(&bot.screen(), "Срочная задача");
    bot.press("Назад").await;

    bot.press("Поиск").await;
    bot.text("задача").await;
    for title in ["Срочная задача", "Высокая задача", "Обычная задача"]
    {
        assert_one_element_row(&bot.screen(), title);
    }
    assert!(
        !support::telegram::buttons(&bot.screen())
            .iter()
            .any(|(label, _)| label.contains("☐")),
        "results must not mark open items with ☐:\n{}",
        bot.screen()
    );

    // Статус результата — текстом: выполненная задача помечена ✅.
    sqlx::query("UPDATE items SET status = 'done', completed_at = now() WHERE id = $1")
        .bind(normal.id)
        .execute(&db.pool)
        .await
        .expect("mark done");
    bot.press("Все").await;
    let results = support::telegram::buttons(&bot.screen());
    let done = results
        .iter()
        .find(|(label, _)| label.contains("Обычная задача"))
        .map(|(label, _)| label.as_str())
        .expect("done task row");
    assert!(
        done.starts_with("✅ "),
        "done result must be marked: {done}"
    );

    bot.press("Назад").await;
    bot.press("Поиск").await;
    bot.text("молоко").await;
    assert_one_element_row(&bot.screen(), "Молоко");
    assert!(
        support::telegram::buttons(&bot.screen())
            .iter()
            .any(|(label, _)| label.contains("Молоко") && label.contains("Покупки")),
        "the result row must keep the list name:\n{}",
        bot.screen()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn row_taps_in_mine_and_search_open_the_card_and_back_returns() {
    let db = db().await;
    let mut bot = onboarded_named(&db.pool, "Alice").await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);

    let mut task = new_item(household.tasks_list_id, "Позвонить маме");
    task.assignee_user_id = Some(household.owner_id);
    create_item(&db.pool, &household, household.tasks_list_id, &actor, task).await;
    create_item(
        &db.pool,
        &household,
        household.shopping_list_id,
        &actor,
        new_item(household.shopping_list_id, "Молоко"),
    )
    .await;

    // «Мои задачи».
    bot.press("Мои задачи").await;
    assert_one_element_row(&bot.screen(), "Позвонить маме");
    bot.press("Позвонить маме").await;
    bot.assert_screen_contains("Мои задачи › Позвонить маме");
    bot.assert_has_button("⬅️ Назад");
    bot.press("Назад").await;
    bot.assert_screen_contains("Мои задачи · 1");

    // Поиск задачи.
    bot.press("Назад").await;
    bot.press("Поиск").await;
    bot.text("Позвонить").await;
    assert_one_element_row(&bot.screen(), "Позвонить маме");
    bot.press("Позвонить маме").await;
    bot.assert_screen_contains("Поиск › Позвонить маме");
    bot.assert_has_button("⬅️ Назад");
    bot.press("Назад").await;
    bot.assert_screen_contains("Найдено 1 результат");

    // Поиск покупки.
    bot.press("Назад").await;
    bot.press("Поиск").await;
    bot.text("Молоко").await;
    assert_one_element_row(&bot.screen(), "Молоко");
    bot.press("Молоко").await;
    bot.assert_screen_contains("Поиск › Молоко");
    bot.assert_has_button("⬅️ Назад");
    bot.press("Назад").await;
    bot.assert_screen_contains("Молоко · Покупки");
}

fn assert_one_element_row(screen: &str, title: &str) {
    let rows = support::telegram::button_rows(screen);
    let matching: Vec<_> = rows
        .iter()
        .filter(|row| row.iter().any(|(label, _)| label.contains(title)))
        .collect();
    assert_eq!(
        matching.len(),
        1,
        "exactly one row per element: {title}\n{screen}\n{rows:?}"
    );
    assert_eq!(
        matching[0].len(),
        1,
        "exactly one button per element row: {title}\n{screen}\n{:?}",
        matching[0]
    );
    assert!(
        !support::telegram::buttons(screen)
            .iter()
            .any(|(label, _)| label == "⋯"),
        "no «⋯» button belongs to an element row: {screen}"
    );
}

async fn item_state(pool: &PgPool, item_id: Uuid) -> (i32, String) {
    sqlx::query_as("SELECT version, status::text FROM items WHERE id = $1")
        .bind(item_id)
        .fetch_one(pool)
        .await
        .expect("item state")
}

async fn item_due(pool: &PgPool, item_id: Uuid) -> (Option<DateTime<Utc>>, Option<NaiveDate>) {
    sqlx::query_as("SELECT due_at, due_date FROM items WHERE id = $1")
        .bind(item_id)
        .fetch_one(pool)
        .await
        .expect("item due")
}

async fn saved_screen(pool: &PgPool, chat_id: i64) -> serde_json::Value {
    sqlx::query_scalar("SELECT state FROM telegram_views WHERE chat_id = $1")
        .bind(chat_id)
        .fetch_one(pool)
        .await
        .expect("saved screen")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shopping_row_toggles_the_purchase_and_stays_in_the_list() {
    let db = db().await;
    let mut bot = onboarded_named(&db.pool, "Alice").await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    let mut item = new_item(household.shopping_list_id, "Молоко");
    item.quantity = Some(2.0);
    item.unit = Some("л".into());
    let item = create_item(
        &db.pool,
        &household,
        household.shopping_list_id,
        &actor,
        item,
    )
    .await;

    bot.press("Покупки").await;
    bot.assert_screen_contains("Покупки · 1");
    bot.assert_screen_contains("☐ Молоко · 2 л");
    assert_one_element_row(&bot.screen(), "Молоко");

    bot.press("☐ Молоко · 2 л").await;
    bot.assert_screen_contains("Покупки · 0");
    bot.assert_screen_contains("☑ Молоко · 2 л");
    let state: serde_json::Value =
        sqlx::query_scalar("SELECT state FROM telegram_views WHERE chat_id = $1")
            .bind(bot.chat)
            .fetch_one(&db.pool)
            .await
            .expect("saved list state");
    assert_eq!(state["t"], serde_json::json!("list"));
    assert_eq!(state["edit"], serde_json::json!(false));
    assert_eq!(
        item_state(&db.pool, item.id).await.1,
        "done",
        "the row tap marks the purchase"
    );

    bot.press("☑ Молоко · 2 л").await;
    bot.assert_screen_contains("☐ Молоко · 2 л");
    assert_eq!(item_state(&db.pool, item.id).await.1, "open");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shopping_edit_mode_opens_the_card_and_is_kept_on_back() {
    let db = db().await;
    let mut bot = onboarded_named(&db.pool, "Alice").await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    let mut item = new_item(household.shopping_list_id, "Молоко");
    item.quantity = Some(2.0);
    item.unit = Some("л".into());
    let item = create_item(
        &db.pool,
        &household,
        household.shopping_list_id,
        &actor,
        item,
    )
    .await;

    bot.press("Покупки").await;
    bot.assert_has_button("✏️ Править");
    bot.press("Править").await;
    bot.assert_screen_contains("Покупки · правка");
    bot.assert_has_button("✓ Закончить правку");

    bot.press("☐ Молоко · 2 л").await;
    bot.assert_screen_contains("Покупки › Молоко");
    bot.assert_has_button("✓ Куплено");
    assert_eq!(
        item_state(&db.pool, item.id).await.1,
        "open",
        "a row in edit mode must not mark the purchase"
    );

    bot.press("Назад").await;
    bot.assert_screen_contains("Покупки · правка");
    bot.assert_has_button("✓ Закончить правку");

    bot.press("Закончить правку").await;
    bot.assert_screen_contains("Покупки · 1");
    assert!(!bot.screen().contains("правка"));

    // Повторный вход из меню открывает обычный режим.
    bot.press("Назад").await;
    bot.press("Покупки").await;
    bot.assert_screen_contains("Покупки · 1");
    assert!(!bot.screen().contains("правка"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shopping_edit_finish_button_has_its_own_row() {
    let db = db().await;
    let mut bot = onboarded_named(&db.pool, "Alice").await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    create_item(
        &db.pool,
        &household,
        household.shopping_list_id,
        &actor,
        new_item(household.shopping_list_id, "Молоко"),
    )
    .await;

    bot.press("Покупки").await;
    bot.press("Править").await;
    bot.assert_screen_contains("Покупки · правка");

    // «✓ Закончить правку» длиннее соседних подписей и в ряду из трёх
    // обрезается на узком Android-экране, поэтому должна занимать весь ряд.
    let rows = button_rows(&bot.screen());
    let finish_row = rows
        .iter()
        .find(|row| row.iter().any(|(label, _)| label == "✓ Закончить правку"))
        .unwrap_or_else(|| panic!("finish button should be present: {rows:?}"));
    assert_eq!(
        finish_row.len(),
        1,
        "«✓ Закончить правку» должна занимать весь ряд: {rows:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_shopping_list_in_edit_mode_drops_the_edit_marker() {
    let db = db().await;
    let mut bot = onboarded_named(&db.pool, "Alice").await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    create_item(
        &db.pool,
        &household,
        household.shopping_list_id,
        &actor,
        new_item(household.shopping_list_id, "Молоко"),
    )
    .await;

    bot.press("Покупки").await;
    bot.press("Править").await;
    bot.assert_screen_contains("Покупки · правка");
    bot.press("Молоко").await;
    bot.press("Удалить").await;
    bot.press("Да, удалить").await;
    bot.press("Назад").await;

    // Список опустел в режиме правки: заголовок и кнопки — как у обычного
    // пустого списка, чтобы из него был выход.
    bot.assert_screen_contains("Покупки · 0");
    bot.assert_screen_contains("Список покупок пуст");
    assert!(!bot.screen().contains("правка"), "{}", bot.screen());
    bot.assert_has_button("⬅️ Назад");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn old_list_state_without_edit_is_read_as_normal_mode() {
    let db = db().await;
    let mut bot = onboarded_named(&db.pool, "Alice").await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    let mut item = new_item(household.shopping_list_id, "Молоко");
    item.quantity = Some(2.0);
    item.unit = Some("л".into());
    create_item(
        &db.pool,
        &household,
        household.shopping_list_id,
        &actor,
        item,
    )
    .await;

    let old_state = serde_json::json!({
        "t": "list",
        "list_id": household.shopping_list_id,
        "page": 0,
    });
    sqlx::query("UPDATE telegram_views SET state = $1 WHERE chat_id = $2")
        .bind(old_state)
        .bind(bot.chat)
        .execute(&db.pool)
        .await
        .expect("old list state");

    let data = action_callback(&db.pool, bot.chat, household.household_id, Action::Refresh).await;
    let (message_id, _) = bot.active();
    bot.press_data(&data, message_id).await;

    bot.assert_screen_contains("Покупки · 1");
    bot.assert_screen_contains("☐ Молоко · 2 л");
    assert!(!bot.screen().contains("правка"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clearing_many_purchases_needs_confirmation() {
    let db = db().await;
    let mut bot = onboarded_named(&db.pool, "Alice").await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    for n in 0..6 {
        let item = create_item(
            &db.pool,
            &household,
            household.shopping_list_id,
            &actor,
            new_item(household.shopping_list_id, &format!("Товар {n}")),
        )
        .await;
        sqlx::query("UPDATE items SET status = 'done', completed_at = now() WHERE id = $1")
            .bind(item.id)
            .execute(&db.pool)
            .await
            .expect("done");
    }

    bot.press("Покупки").await;
    bot.press("Очистить").await;
    bot.assert_screen_contains("Покупки › Удалить 6 позиций?");
    bot.press("Да, очистить").await;
    bot.assert_screen_contains("Покупки › Убрано: 6 позиций");
    bot.press("Назад").await;
    bot.assert_screen_contains("Список покупок пуст");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shopping_list_from_all_lists_keeps_the_trail_and_returns_there() {
    let db = db().await;
    let mut bot = onboarded_named(&db.pool, "Alice").await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    create_item(
        &db.pool,
        &household,
        household.shopping_list_id,
        &actor,
        new_item(household.shopping_list_id, "Молоко"),
    )
    .await;
    let mut conn = db.pool.acquire().await.expect("connection");
    lists::create(
        &mut conn,
        household.household_id,
        ListKind::Checklist,
        "Дача",
        Utc::now(),
    )
    .await
    .expect("extra list");
    drop(conn);

    // Перерисовать меню: третий список включает «🗂 Все списки».
    bot.text("/start").await;
    bot.press("Все списки").await;
    bot.press("Покупки").await;
    assert!(
        bot.screen().contains("<h3>Все списки › Покупки · 1</h3>"),
        "a shopping list opened from «Все списки» must keep that parent in its trail:\n{}",
        bot.screen()
    );
    bot.assert_has_button("⬅️ Назад");
    bot.assert_has_button("🏠 Меню");

    // Карточка из такого списка тоже показывает полный путь.
    bot.press("Править").await;
    bot.press("Молоко").await;
    bot.assert_screen_contains("Все списки › Покупки › Молоко");
    bot.press("Назад").await;
    assert!(
        bot.screen()
            .contains("<h3>Все списки › Покупки · правка</h3>"),
        "the card must return to the list opened from «Все списки»:\n{}",
        bot.screen()
    );

    bot.press("Назад").await;
    bot.assert_screen_contains("<h3>Все списки</h3>");

    // Список из меню по-прежнему без «Все списки».
    bot.press("Назад").await;
    bot.press("Покупки").await;
    assert!(
        bot.screen().contains("<h3>Покупки · 1</h3>"),
        "a shopping list opened from the menu must not claim «Все списки» as parent:\n{}",
        bot.screen()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clearing_purchases_from_all_lists_keeps_the_origin() {
    let db = db().await;
    let mut bot = onboarded_named(&db.pool, "Alice").await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    for n in 0..6 {
        let item = create_item(
            &db.pool,
            &household,
            household.shopping_list_id,
            &actor,
            new_item(household.shopping_list_id, &format!("Товар {n}")),
        )
        .await;
        sqlx::query("UPDATE items SET status = 'done', completed_at = now() WHERE id = $1")
            .bind(item.id)
            .execute(&db.pool)
            .await
            .expect("done");
    }
    let mut conn = db.pool.acquire().await.expect("connection");
    lists::create(
        &mut conn,
        household.household_id,
        ListKind::Checklist,
        "Дача",
        Utc::now(),
    )
    .await
    .expect("extra list");
    drop(conn);

    bot.text("/start").await;
    bot.press("Все списки").await;
    bot.press("Покупки").await;
    bot.press("Очистить").await;
    bot.assert_screen_contains("Все списки › Покупки › Удалить 6 позиций?");
    bot.press("Да, очистить").await;
    bot.assert_screen_contains("Все списки › Покупки › Убрано: 6 позиций");
    bot.press("Назад").await;
    assert!(
        bot.screen().contains("<h3>Все списки › Покупки · 0</h3>"),
        "after clearing, «Назад» must return to the list opened from «Все списки»:\n{}",
        bot.screen()
    );
    bot.assert_screen_contains("Список покупок пуст");
    bot.press("Назад").await;
    bot.assert_screen_contains("<h3>Все списки</h3>");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undoing_cleared_purchases_from_all_lists_keeps_the_origin() {
    let db = db().await;
    let mut bot = onboarded_named(&db.pool, "Alice").await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    for n in 0..6 {
        let item = create_item(
            &db.pool,
            &household,
            household.shopping_list_id,
            &actor,
            new_item(household.shopping_list_id, &format!("Товар {n}")),
        )
        .await;
        sqlx::query("UPDATE items SET status = 'done', completed_at = now() WHERE id = $1")
            .bind(item.id)
            .execute(&db.pool)
            .await
            .expect("done");
    }
    let mut conn = db.pool.acquire().await.expect("connection");
    lists::create(
        &mut conn,
        household.household_id,
        ListKind::Checklist,
        "Дача",
        Utc::now(),
    )
    .await
    .expect("extra list");
    drop(conn);

    bot.text("/start").await;
    bot.press("Все списки").await;
    bot.press("Покупки").await;
    bot.press("Очистить").await;
    bot.press("Да, очистить").await;
    bot.assert_screen_contains("Все списки › Покупки › Убрано: 6 позиций");
    bot.press("Отменить").await;
    assert!(
        bot.screen().contains("<h3>Все списки › Покупки · 0</h3>"),
        "after undo, «Назад» must return to the list opened from «Все списки»:\n{}",
        bot.screen()
    );
    bot.press("Назад").await;
    bot.assert_screen_contains("<h3>Все списки</h3>");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lists_paginate_and_counters_follow_actions() {
    let db = db().await;
    let mut bot = onboarded_named(&db.pool, "Alice").await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    for n in 0..9 {
        create_item(
            &db.pool,
            &household,
            household.tasks_list_id,
            &actor,
            new_item(household.tasks_list_id, &format!("Задача {n}")),
        )
        .await;
    }

    bot.press("Задачи").await;
    bot.assert_screen_contains("Задачи · 9");
    bot.assert_screen_contains("Страница 1 из 2");
    let row_buttons: Vec<_> = support::telegram::buttons(&bot.screen())
        .into_iter()
        .filter(|(label, _)| label.starts_with("Задача "))
        .collect();
    assert_eq!(row_buttons.len(), 8, "one full-width button per task row");
    let labels: Vec<String> = support::telegram::buttons(&bot.screen())
        .into_iter()
        .map(|(label, _)| label)
        .collect();
    assert!(
        !labels.iter().any(|label| label == "☐" || label == "⋯"),
        "element rows must not keep service buttons: {labels:?}"
    );
    bot.press("→").await;
    bot.assert_screen_contains("Страница 2 из 2");

    // Завершить единственную задачу второй страницы: строка открывает
    // карточку, «✓ Выполнено» возвращает на ту же страницу с баннером.
    let task = support::telegram::buttons(&bot.screen())
        .into_iter()
        .map(|(label, _)| label)
        .find(|label| label.starts_with("Задача "))
        .expect("a task on the second page");
    bot.press(&task).await;
    bot.press("Выполнено").await;
    bot.assert_screen_contains("✅ Выполнено:");
    bot.assert_screen_contains("Задачи · 8");
    bot.press("Назад").await;
    bot.assert_screen_contains("📋 Задачи · 8");
    bot.assert_screen_contains("Выполнено: <b>1</b>");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completed_task_stays_in_done_today_section_and_opens_card() {
    let db = db().await;
    let now = local_utc(
        chrono_tz::Tz::Europe__Moscow,
        NaiveDate::from_ymd_opt(2027, 3, 15).expect("date"),
        13,
        47,
    );
    let mut bot = onboarded_at(&db.pool, "Alice", now).await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &actor,
        new_item(household.tasks_list_id, "Вынести мусор"),
    )
    .await;

    bot.press("Задачи").await;
    bot.press("Вынести мусор").await;
    bot.press("Выполнено").await;
    bot.assert_screen_contains("✅ Выполнено: Вынести мусор");
    bot.assert_screen_contains("Выполнено сегодня");
    bot.assert_screen_contains("✅ Вынести мусор · 13:47");
    assert!(
        !bot.screen().contains("Выполнено ранее"),
        "a task completed today must not be counted as earlier:\n{}",
        bot.screen()
    );

    bot.press("✅ Вынести мусор").await;
    bot.assert_screen_contains("Задачи › Вынести мусор");
    for label in ["↩️ Вернуть", "✏️ Править", "🗑 Удалить"] {
        bot.assert_has_button(label);
    }
    for label in ["📅 Срок", "✓ Выполнено"] {
        assert!(
            bot.find_button(label).is_none(),
            "a completed task card must not keep `{label}`:\n{}",
            bot.screen()
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn done_today_section_uses_the_users_day_boundary() {
    let db = db().await;
    let vladivostok = chrono_tz::Tz::Asia__Vladivostok;
    let now = local_utc(
        vladivostok,
        NaiveDate::from_ymd_opt(2027, 3, 15).expect("date"),
        23,
        30,
    );
    let mut bot = onboarded_at(&db.pool, "Alice", now).await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    sqlx::query("UPDATE users SET timezone = 'Asia/Vladivostok' WHERE telegram_user_id = $1")
        .bind(bot.chat)
        .execute(&db.pool)
        .await
        .expect("timezone");
    create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &actor,
        new_item(household.tasks_list_id, "Вынести мусор"),
    )
    .await;

    bot.press("Задачи").await;
    bot.press("Вынести мусор").await;
    bot.press("Выполнено").await;
    bot.assert_screen_contains("✅ Вынести мусор · 23:30");

    // Полночь во Владивостоке наступает раньше, чем в UTC: вчерашнее
    // выполнение больше не попадает в раздел.
    let next_day = local_utc(
        vladivostok,
        NaiveDate::from_ymd_opt(2027, 3, 16).expect("date"),
        0,
        10,
    );
    bot.set_now(next_day);
    bot.press("Назад").await;
    bot.press("Задачи").await;
    assert!(
        !bot.screen().contains("Выполнено сегодня"),
        "yesterday's completions must disappear:\n{}",
        bot.screen()
    );
    assert!(!bot.screen().contains("✅ Вынести мусор"));
    bot.assert_screen_contains("Выполнено ранее: 1");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recurring_task_is_shown_open_and_done_today() {
    let db = db().await;
    let today = NaiveDate::from_ymd_opt(2027, 3, 15).expect("date");
    let now = local_utc(chrono_tz::Tz::Europe__Moscow, today, 9, 0);
    let mut bot = onboarded_at(&db.pool, "Alice", now).await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    let mut input = new_item(household.tasks_list_id, "Полить цветы");
    input.due_date = Some(today);
    input.recurrence = Some(support::recurrence(
        tasklerq::domain::models::RecurrenceFreq::Daily,
        Vec::new(),
    ));
    create_item(&db.pool, &household, household.tasks_list_id, &actor, input).await;

    bot.press("Задачи").await;
    bot.press("Полить цветы").await;
    bot.press("Выполнено").await;
    bot.assert_screen_contains("Выполнено сегодня");
    bot.assert_screen_contains("✅ Полить цветы · 09:00");

    let labels: Vec<String> = support::telegram::buttons(&bot.screen())
        .into_iter()
        .map(|(label, _)| label)
        .collect();
    assert!(
        labels.iter().any(|label| label == "Полить цветы · завтра"),
        "the recurring task stays open with the next due date: {labels:?}"
    );
    assert_eq!(
        labels
            .iter()
            .filter(|label| label.contains("Полить цветы"))
            .count(),
        2,
        "the task is shown twice: open and completed today"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_recurring_occurrences_are_listed_in_done_today() {
    let db = db().await;
    let today = NaiveDate::from_ymd_opt(2027, 3, 15).expect("date");
    let now = local_utc(chrono_tz::Tz::Europe__Moscow, today, 9, 0);
    let mut bot = onboarded_at(&db.pool, "Alice", now).await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    let mut input = new_item(household.tasks_list_id, "Полить цветы");
    input.due_date = Some(today);
    input.recurrence = Some(support::recurrence(
        tasklerq::domain::models::RecurrenceFreq::Daily,
        Vec::new(),
    ));
    let task = create_item(&db.pool, &household, household.tasks_list_id, &actor, input).await;

    bot.press("Задачи").await;
    bot.press("Полить цветы").await;
    bot.press("Выполнено").await;
    bot.assert_screen_contains("✅ Полить цветы · 09:00");

    // Следующее вхождение уже наступило: снимаем отметку и выполняем его.
    sqlx::query("UPDATE items SET completed_at = NULL WHERE id = $1")
        .bind(task.id)
        .execute(&db.pool)
        .await
        .expect("prepare second occurrence");
    bot.set_now(now + Duration::minutes(1));
    bot.press("Полить цветы · завтра").await;
    bot.press("Выполнено").await;

    let labels: Vec<String> = support::telegram::buttons(&bot.screen())
        .into_iter()
        .map(|(label, _)| label)
        .collect();
    let completions: Vec<&String> = labels
        .iter()
        .filter(|label| label.starts_with("✅ Полить цветы"))
        .collect();
    assert_eq!(
        completions.len(),
        2,
        "two occurrences give two rows: {labels:?}"
    );
    assert!(
        labels
            .iter()
            .any(|label| label == "✅ Полить цветы · 09:00")
    );
    assert!(
        labels
            .iter()
            .any(|label| label == "✅ Полить цветы · 09:01")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn done_today_counter_is_visible_on_every_page() {
    let db = db().await;
    let now = local_utc(
        chrono_tz::Tz::Europe__Moscow,
        NaiveDate::from_ymd_opt(2027, 3, 15).expect("date"),
        10,
        0,
    );
    let mut bot = onboarded_at(&db.pool, "Alice", now).await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    for n in 0..10 {
        create_item(
            &db.pool,
            &household,
            household.tasks_list_id,
            &actor,
            new_item(household.tasks_list_id, &format!("Задача {n}")),
        )
        .await;
    }

    bot.press("Задачи").await;
    bot.press("Задача 0").await;
    bot.press("Выполнено").await;
    bot.assert_screen_contains("Задачи · 9 · ✅ 1");

    bot.press("→").await;
    bot.assert_screen_contains("Страница 2 из 2");
    bot.assert_screen_contains("Задачи · 9 · ✅ 1");
    bot.assert_screen_contains("Выполнено сегодня");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undone_completion_disappears_from_done_today() {
    let db = db().await;
    let now = local_utc(
        chrono_tz::Tz::Europe__Moscow,
        NaiveDate::from_ymd_opt(2027, 3, 15).expect("date"),
        13,
        47,
    );
    let mut bot = onboarded_at(&db.pool, "Alice", now).await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &actor,
        new_item(household.tasks_list_id, "Вынести мусор"),
    )
    .await;

    bot.press("Задачи").await;
    bot.press("Вынести мусор").await;
    bot.press("Выполнено").await;
    bot.assert_screen_contains("Выполнено сегодня");

    bot.press("Отменить").await;
    assert!(
        !bot.screen().contains("Выполнено сегодня"),
        "an undone completion must disappear:\n{}",
        bot.screen()
    );
    assert!(!bot.screen().contains("✅ Вынести мусор"));
    bot.assert_screen_contains("Вынести мусор");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn menu_counters_follow_a_completion() {
    let db = db().await;
    let now = local_utc(
        chrono_tz::Tz::Europe__Moscow,
        NaiveDate::from_ymd_opt(2027, 3, 15).expect("date"),
        10,
        0,
    );
    let mut bot = onboarded_at(&db.pool, "Alice", now).await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &actor,
        new_item(household.tasks_list_id, "Оплатить интернет"),
    )
    .await;

    bot.text("/start").await;
    bot.assert_screen_contains("📋 Задачи · 1");
    bot.assert_screen_contains("Выполнено: <b>0</b>");

    bot.press("Задачи").await;
    bot.press("Оплатить интернет").await;
    bot.press("Выполнено").await;
    bot.press("Назад").await;

    bot.assert_screen_contains("📋 Задачи · 0");
    bot.assert_screen_contains("Выполнено: <b>1</b>");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn menu_counters_keep_a_completed_recurring_task_open() {
    let db = db().await;
    let today = NaiveDate::from_ymd_opt(2027, 3, 15).expect("date");
    let now = local_utc(chrono_tz::Tz::Europe__Moscow, today, 9, 0);
    let mut bot = onboarded_at(&db.pool, "Alice", now).await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    let mut input = new_item(household.tasks_list_id, "Полить цветы");
    input.due_date = Some(today);
    input.recurrence = Some(support::recurrence(
        tasklerq::domain::models::RecurrenceFreq::Daily,
        Vec::new(),
    ));
    create_item(&db.pool, &household, household.tasks_list_id, &actor, input).await;

    bot.text("/start").await;
    bot.assert_screen_contains("📋 Задачи · 1");
    bot.assert_screen_contains("Выполнено: <b>0</b>");

    bot.press("Задачи").await;
    bot.press("Полить цветы").await;
    bot.press("Выполнено").await;
    bot.press("Назад").await;

    bot.assert_screen_contains("📋 Задачи · 1");
    bot.assert_screen_contains("Выполнено: <b>1</b>");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn menu_counters_do_not_repeat_the_lists_line() {
    let db = db().await;
    let mut bot = onboarded_named(&db.pool, "Alice").await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    for n in 0..12 {
        create_item(
            &db.pool,
            &household,
            household.tasks_list_id,
            &actor,
            new_item(household.tasks_list_id, &format!("Задача {n}")),
        )
        .await;
    }
    for n in 0..7 {
        create_item(
            &db.pool,
            &household,
            household.shopping_list_id,
            &actor,
            new_item(household.shopping_list_id, &format!("Позиция {n}")),
        )
        .await;
    }

    bot.text("/start").await;
    bot.assert_screen_contains("📋 Задачи · 12");
    bot.assert_screen_contains("🛒 Покупки · 7");
    assert!(
        !bot.screen().contains("Списки:"),
        "the lists line must be gone:\n{}",
        bot.screen()
    );
    bot.assert_screen_contains("Напишите название задачи — открою редактор задачи.");
    assert!(
        !bot.screen().contains("я разберу"),
        "without LLM the hint must not promise parsing:\n{}",
        bot.screen()
    );
    assert!(
        !bot.screen().contains("открою форму"),
        "without LLM the hint must not promise a form:\n{}",
        bot.screen()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn menu_hint_promises_parsing_when_llm_capture_is_enabled() {
    let db = db().await;
    let bot = onboarded_with_llm(&db.pool, "Alice").await;
    bot.assert_screen_contains("Напишите задачу или покупку — я разберу.");
    assert!(
        !bot.screen().contains("открою редактор"),
        "with LLM the hint must not promise an editor:\n{}",
        bot.screen()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn menu_and_mine_screen_use_the_same_assigned_counter() {
    let db = db().await;
    let mut bot = onboarded_named(&db.pool, "Alice").await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    for n in 0..60 {
        let mut item = new_item(household.tasks_list_id, &format!("Моя {n}"));
        item.assignee_user_id = Some(household.owner_id);
        create_item(&db.pool, &household, household.tasks_list_id, &actor, item).await;
    }

    bot.text("/start").await;
    bot.assert_screen_contains("👤 Мои задачи · 60");
    bot.press("Мои задачи").await;
    bot.assert_screen_contains("Мои задачи · 60");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn menu_today_counter_uses_the_users_day_boundary() {
    let db = db().await;
    let vladivostok = chrono_tz::Tz::Asia__Vladivostok;
    // Полночь во Владивостоке уже наступила, а в UTC ещё 15 марта.
    let now = local_utc(
        vladivostok,
        NaiveDate::from_ymd_opt(2027, 3, 16).expect("date"),
        1,
        0,
    );
    let today = NaiveDate::from_ymd_opt(2027, 3, 16).expect("date");
    let mut bot = onboarded_at(&db.pool, "Alice", now).await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    sqlx::query("UPDATE users SET timezone = 'Asia/Vladivostok' WHERE telegram_user_id = $1")
        .bind(bot.chat)
        .execute(&db.pool)
        .await
        .expect("timezone");

    let mut later_today = new_item(household.tasks_list_id, "Сегодня позже");
    later_today.due_at = Some(local_utc(vladivostok, today, 2, 0));
    create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &actor,
        later_today,
    )
    .await;
    let mut today_without_time = new_item(household.tasks_list_id, "Сегодня без времени");
    today_without_time.due_date = Some(today);
    create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &actor,
        today_without_time,
    )
    .await;
    let mut yesterday = new_item(household.tasks_list_id, "Вчера");
    yesterday.due_date = NaiveDate::from_ymd_opt(2027, 3, 15);
    create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &actor,
        yesterday,
    )
    .await;
    let mut tomorrow = new_item(household.tasks_list_id, "Завтра");
    tomorrow.due_date = NaiveDate::from_ymd_opt(2027, 3, 17);
    create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &actor,
        tomorrow,
    )
    .await;

    bot.text("/start").await;
    bot.assert_screen_contains("Просрочено: <b>1</b>");
    bot.assert_screen_contains("На сегодня: <b>2</b>");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_and_my_tasks() {
    let db = db().await;
    let mut bot = onboarded_named(&db.pool, "Alice").await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    create_item(
        &db.pool,
        &household,
        household.shopping_list_id,
        &actor,
        new_item(household.shopping_list_id, "Молоко"),
    )
    .await;
    create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &actor,
        new_item(household.tasks_list_id, "Купить молоко домой"),
    )
    .await;
    let mut mine = new_item(household.tasks_list_id, "Моя задача");
    mine.assignee_user_id = Some(household.owner_id);
    create_item(&db.pool, &household, household.tasks_list_id, &actor, mine).await;

    bot.press("Поиск").await;
    bot.assert_screen_contains("Что ищем?");
    bot.text("молоко").await;
    bot.assert_screen_contains("Найдено 2 результата");
    bot.assert_screen_contains("Покупки");
    bot.assert_screen_contains("Задачи");

    // На экране результатов текст запускает новый поиск, а не разбор.
    bot.text("несуществующее").await;
    bot.assert_screen_contains("Ничего не нашлось");

    bot.press("Изменить запрос").await;
    bot.text("zzzz").await;
    bot.assert_screen_contains("Ничего не нашлось");

    bot.press("Назад").await;
    bot.press("Мои задачи").await;
    bot.assert_screen_contains("Мои задачи · 1");
    bot.assert_screen_contains("Моя задача");
    assert!(!bot.screen().contains("Купить молоко домой"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn text_on_search_results_runs_a_new_search_instead_of_capture() {
    let db = db().await;
    let mut bot = onboarded_with_llm(&db.pool, "Alice").await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    create_item(
        &db.pool,
        &household,
        household.shopping_list_id,
        &actor,
        new_item(household.shopping_list_id, "Хлеб"),
    )
    .await;

    bot.press("Поиск").await;
    bot.assert_screen_contains("Напишите запрос");
    bot.text("молоко").await;
    bot.assert_screen_contains("Ничего не нашлось");
    bot.assert_screen_contains("Напишите запрос");

    bot.text("хлеб").await;
    bot.assert_screen_contains("Найдено 1 результат");
    bot.assert_screen_contains("Хлеб");

    let jobs: i64 = sqlx::query_scalar("SELECT count(*) FROM capture_jobs WHERE chat_id = $1")
        .bind(bot.chat)
        .fetch_one(&db.pool)
        .await
        .expect("capture jobs");
    assert_eq!(
        jobs, 0,
        "text on the results screen must not enqueue a capture job"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shopping_list_screen_hints_that_text_adds_positions() {
    let db = db().await;
    let mut bot = onboarded_named(&db.pool, "Alice").await;

    bot.press("Покупки").await;
    bot.assert_screen_contains("Напишите — добавлю в «Покупки»");
    bot.assert_screen_contains("(через запятую или с новой строки)");

    // Подсказка не врёт: без разбора текст делится на позиции, а не открывает
    // редактор задачи.
    bot.text("молоко, хлеб").await;
    bot.assert_screen_contains("Добавлено без разбора");
    bot.assert_screen_contains("молоко · 1 шт");
    bot.assert_screen_contains("хлеб · 1 шт");
    bot.assert_screen_contains("Покупки · 2");
    assert!(
        !bot.screen().contains("Название:"),
        "text on the shopping screen must not open the editor:\n{}",
        bot.screen()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn checklist_screen_uses_the_general_hint_without_the_list_name() {
    let db = db().await;
    let mut bot = onboarded_with_llm(&db.pool, "Alice").await;
    let household = household_for(&db.pool, bot.chat).await;
    let mut conn = db.pool.acquire().await.expect("connection");
    lists::create(
        &mut conn,
        household.household_id,
        ListKind::Checklist,
        "Дача",
        Utc::now(),
    )
    .await
    .expect("checklist list");
    drop(conn);

    // Третий список включает «Все списки» в меню.
    bot.text("/start").await;
    bot.press("Все списки").await;
    bot.press("Дача").await;

    bot.assert_screen_contains("Напишите задачу или покупку — я разберу.");
    assert!(
        !bot.screen().contains("добавлю в «Дача»"),
        "the checklist hint must not name the list:\n{}",
        bot.screen()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn settings_profile_timezone_and_notifications() {
    let db = db().await;
    let mut bot = onboarded_named(&db.pool, "Alice").await;

    bot.press("Настройки").await;
    bot.assert_screen_contains("👤 Alice");
    bot.assert_screen_contains("Часовой пояс: Europe/Moscow");
    bot.assert_screen_contains("Роль: владелец");
    bot.assert_has_button("Профиль");
    bot.assert_has_button("Уведомления");
    bot.assert_has_button("Опасное");
    bot.assert_has_button("Семья");
    bot.assert_has_button("Журнал");
    assert!(bot.find_button("Удалить пространство").is_none());
    assert!(bot.find_button("Выйти").is_none());
    assert!(!bot.screen().contains("Напоминания:"));
    assert!(!bot.screen().contains("Тихие часы:"));

    // Профиль показывает имя и пояс; выбор пояса возвращает в профиль.
    bot.press("Профиль").await;
    bot.assert_screen_contains("👤 Alice");
    bot.assert_screen_contains("Часовой пояс: Europe/Moscow");
    bot.press("Часовой пояс").await;
    bot.assert_screen_contains("Настройки › Профиль › Часовой пояс");
    bot.press("Asia/Yekaterinburg").await;
    bot.assert_screen_contains("Часовой пояс: Asia/Yekaterinburg");
    bot.assert_screen_contains("Настройки › Профиль");

    // Сохранение имени возвращает в профиль с новым именем.
    bot.press("Имя").await;
    bot.text("Алиса").await;
    bot.assert_screen_contains("Алиса");

    // Сводок на экране настроек нет: они в разделе уведомлений.
    bot.press("Назад").await;
    bot.assert_screen_contains("Настройки");
    bot.press("Уведомления").await;
    bot.assert_screen_contains("🔔 Напоминания: за 15 мин");

    // Выключенные напоминания скрывают уточняющие кнопки.
    bot.press_after("🔔 Напоминания", "Выкл").await;
    bot.assert_screen_contains("🔔 Напоминания: выкл");
    assert!(bot.find_button("⏱ За").is_none());
    assert!(bot.find_button("без времени").is_none());

    // Выбор срока на отдельном экране возвращает на уведомления.
    bot.press_after("🔔 Напоминания", "Вкл").await;
    bot.press("⏱ За 15 мин").await;
    bot.assert_screen_contains("Настройки › Уведомления › За сколько напоминать?");
    bot.press("30 мин").await;
    bot.assert_screen_contains("🔔 Напоминания: за 30 мин");

    // Своё значение вне диапазона отклоняется, форма остаётся открытой.
    bot.press("⏱ За 30 мин").await;
    bot.press("Своё").await;
    bot.text("2").await;
    bot.assert_screen_contains("от 5 минут");
    bot.text("45").await;
    bot.assert_screen_contains("Напоминания: за 45 мин");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dangerous_action_is_not_on_the_settings_screen() {
    let db = db().await;
    let mut owner = onboarded_named(&db.pool, "Алексей").await;
    let mut maria = invite(&mut owner, &db.pool, "Мария").await;
    let household = household_for(&db.pool, owner.chat).await;

    // Владелец: удаления нет на настройках, но оно есть на «Опасном».
    owner.press("Настройки").await;
    assert!(owner.find_button("Удалить пространство").is_none());
    owner.press("Опасное").await;
    owner.assert_screen_contains("⚠️ Опасное");
    owner.assert_has_button("Удалить пространство");
    owner.press("Удалить пространство").await;
    owner.assert_screen_contains("Для подтверждения отправьте название");
    owner.press("Отмена").await;
    owner.assert_screen_contains("⚠️ Опасное");

    // Участник: на «Опасном» только выход.
    maria.press("Настройки").await;
    maria.press("Опасное").await;
    maria.assert_has_button("Выйти");
    assert!(maria.find_button("Удалить пространство").is_none());

    // Прямое нажатие удаления по валидному токену отклоняется.
    let (message_id, _) = maria.active();
    let data = action_callback(
        &db.pool,
        maria.chat,
        household.household_id,
        Action::HouseholdDeleteStart,
    )
    .await;
    maria.press_data(&data, message_id).await;
    let toast = maria
        .fake
        .answers()
        .into_iter()
        .next_back()
        .and_then(|(_, text, _)| text);
    assert_eq!(toast.as_deref(), Some("Это доступно только владельцу."));
    assert!(
        !maria
            .screen()
            .contains("Для подтверждения отправьте название")
    );

    maria.press("Выйти").await;
    maria.assert_screen_contains("Выйти из пространства?");
    maria.press("Назад").await;
    maria.assert_screen_contains("⚠️ Опасное");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quiet_hours_can_be_switched_and_hide_boundaries_when_off() {
    let db = db().await;
    let mut bot = onboarded_named(&db.pool, "Alice").await;

    bot.press("Настройки").await;
    bot.press("Уведомления").await;
    bot.assert_screen_contains("🌙 Тихие часы: 22:00–08:00");
    bot.assert_has_button("Начало тихих: 22:00");
    bot.assert_has_button("Конец тихих: 08:00");

    bot.press_after("🌙 Тихие часы", "Выкл").await;
    bot.assert_screen_contains("🌙 Тихие часы: выкл");
    assert!(bot.find_button("Начало тихих").is_none());
    assert!(bot.find_button("Конец тихих").is_none());
    let screen = bot.screen();
    let quiet = &screen[screen.find("🌙 Тихие часы").expect("quiet block")..];
    assert!(
        support::telegram::buttons(quiet)
            .iter()
            .any(|(label, _)| label == "Вкл"),
        "the quiet-hours block must offer «Вкл»:\n{screen}"
    );

    bot.press_after("🌙 Тихие часы", "Вкл").await;
    bot.assert_screen_contains("🌙 Тихие часы: 22:00–08:00");
    bot.assert_has_button("Начало тихих: 22:00");
    bot.assert_has_button("Конец тихих: 08:00");
    let screen = bot.screen();
    let quiet = &screen[screen.find("🌙 Тихие часы").expect("quiet block")..];
    assert!(
        support::telegram::buttons(quiet)
            .iter()
            .any(|(label, _)| label == "Выкл"),
        "the toggle must name the action:\n{screen}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reminder_for_tasks_without_due_time_has_a_clear_label() {
    let db = db().await;
    let mut bot = onboarded_named(&db.pool, "Alice").await;

    bot.press("Настройки").await;
    bot.press("Уведомления").await;
    bot.assert_has_button("без времени — в 09:00");
    assert!(!bot.screen().contains("Вседневн"));

    bot.press("без времени").await;
    bot.assert_screen_contains("задачах без времени");
    assert!(!bot.screen().contains("Вседневн"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn settings_edit_notification_times_and_reject_invalid_values() {
    let db = db().await;
    let mut bot = onboarded_named(&db.pool, "Alice").await;

    bot.press("Настройки").await;
    assert!(!bot.screen().contains("Тихие часы:"));
    bot.press("Уведомления").await;
    bot.assert_screen_contains("🌅 Утренняя сводка: 08:30");
    bot.assert_has_button("Время сводки: 08:30");
    bot.assert_has_button("без времени — в 09:00");
    bot.assert_has_button("Начало тихих: 22:00");
    bot.assert_has_button("Конец тихих: 08:00");

    // Время сводки: ошибки формата не меняют прежнее значение.
    bot.press("Время сводки").await;
    bot.assert_screen_contains("Во сколько присылать сводку");
    for bad in ["25:00", "9-30"] {
        bot.text(bad).await;
        bot.assert_screen_contains("формате HH:MM");
        let settings = notification_settings(&db.pool, bot.chat).await;
        assert_eq!(settings.digest_time, "08:30", "{bad}");
    }
    bot.text("07:45").await;
    bot.assert_screen_contains("🌅 Утренняя сводка: 07:45");
    assert_eq!(
        notification_settings(&db.pool, bot.chat).await.digest_time,
        "07:45"
    );

    // Время напоминания о задачах без срока.
    bot.press("без времени").await;
    bot.assert_screen_contains("задачах без времени");
    bot.text("10:15").await;
    bot.assert_screen_contains("без времени — в 10:15");
    assert_eq!(
        notification_settings(&db.pool, bot.chat).await.all_day_time,
        "10:15"
    );

    // Начало и конец тихих часов не могут совпадать; прежний конец остаётся.
    bot.press("Начало тихих").await;
    bot.text("23:00").await;
    bot.assert_has_button("Начало тихих: 23:00");
    bot.press("Конец тихих").await;
    bot.text("23:00").await;
    bot.assert_screen_contains("не должны совпадать");
    let settings = notification_settings(&db.pool, bot.chat).await;
    assert_eq!(settings.quiet_start, "23:00");
    assert_eq!(settings.quiet_end, "08:00");

    bot.press("Отмена").await;
    bot.assert_has_button("Конец тихих: 08:00");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notification_time_changes_apply_on_the_next_scheduler_tick() {
    // Планировщик обрабатывает всю базу, поэтому нужна изолированная: иначе
    // тик доставит сводки пользователям из параллельных тестов.
    let db = support::isolated_db().await;
    let mut bot = onboarded_named(&db.pool, "Alice").await;
    let household = household_for(&db.pool, bot.chat).await;
    let moscow = chrono_tz::Europe::Moscow;
    let day = Utc::now().with_timezone(&moscow).date_naive() + Duration::days(1);

    bot.press("Настройки").await;
    bot.press("Уведомления").await;
    bot.press("Время сводки").await;
    bot.text("09:15").await;
    bot.assert_screen_contains("Время сводки: 09:15");

    let scheduler = scheduler(&db.pool);
    assert_eq!(
        scheduler
            .tick(local_utc(moscow, day, 9, 14))
            .await
            .expect("before digest")
            .delivered,
        0
    );
    assert_eq!(
        scheduler
            .tick(local_utc(moscow, day, 9, 15))
            .await
            .expect("new digest time")
            .delivered,
        1
    );
    let sent = outbox_html(&db.pool, bot.chat).await;
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert!(sent[0].contains("День свободен."), "{sent:?}");

    // Выключаем сводку и меняем время вседневного напоминания тем же
    // экземпляром планировщика — перезапуск не нужен.
    bot.press_after("🌅 Утренняя сводка", "Выкл").await;
    assert!(
        !notification_settings(&db.pool, bot.chat)
            .await
            .digest_enabled
    );
    bot.press("без времени").await;
    bot.text("14:00").await;

    let due_date = day + Duration::days(1);
    let mut item = new_item(household.tasks_list_id, "Вседневная задача");
    item.due_date = Some(due_date);
    item.assignee_user_id = Some(household.owner_id);
    create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &Actor::user(household.owner_id, UserRole::Owner),
        item,
    )
    .await;

    assert_eq!(
        scheduler
            .tick(local_utc(moscow, due_date, 13, 59))
            .await
            .expect("before all-day time")
            .delivered,
        0
    );
    assert_eq!(
        scheduler
            .tick(local_utc(moscow, due_date, 14, 0))
            .await
            .expect("new all-day time")
            .delivered,
        1
    );
    let sent = outbox_html(&db.pool, bot.chat).await;
    assert_eq!(sent.len(), 2, "{sent:?}");
    assert!(sent[1].contains("Вседневная задача"), "{sent:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bot_answers_user_actions_during_quiet_hours() {
    let db = db().await;
    let mut bot = onboarded_named(&db.pool, "Alice").await;
    let moscow = chrono_tz::Europe::Moscow;
    let local = bot.now.with_timezone(&moscow);
    let mut night = moscow
        .with_ymd_and_hms(local.year(), local.month(), local.day(), 23, 30, 0)
        .single()
        .expect("night")
        .with_timezone(&Utc);
    if night <= bot.now {
        night += Duration::days(1);
    }
    bot.advance(night - bot.now);
    let household = household_for(&db.pool, bot.chat).await;
    bot.fake.clear_calls();

    bot.press("Настройки").await;
    bot.assert_screen_contains("Настройки");
    assert!(
        !bot.screen().contains("Тихие часы:"),
        "the quiet hours summary is on the notifications screen now"
    );
    assert!(
        !bot.fake.calls().is_empty(),
        "бот должен ответить ночью сразу, не дожидаясь конца тихих часов"
    );
    let deliveries: i64 =
        sqlx::query_scalar("SELECT count(*) FROM notification_deliveries WHERE household_id = $1")
            .bind(household.household_id)
            .fetch_one(&db.pool)
            .await
            .expect("deliveries");
    assert_eq!(deliveries, 0);

    bot.press("Уведомления").await;
    bot.assert_screen_contains("🌙 Тихие часы: 22:00–08:00");
    bot.assert_has_button("Начало тихих: 22:00");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_timezone_from_a_button_is_rejected_and_the_previous_one_kept() {
    let db = db().await;
    let mut bot = onboarded_named(&db.pool, "Alice").await;
    let household = household_for(&db.pool, bot.chat).await;
    let action = Action::SetTimezone {
        zone: "Mars/Olympus".into(),
    };
    let token = {
        let mut conn = db.pool.acquire().await.expect("connection");
        let tokens = callbacks::issue_many(
            &mut conn,
            &[NewToken {
                telegram_user_id: bot.chat,
                household_id: Some(household.household_id),
                scope: Scope::Screen,
                action: action.name(),
                payload: serde_json::to_value(&action).expect("payload"),
                version: None,
            }],
            Utc::now(),
        )
        .await
        .expect("token");
        tokens.into_iter().next().expect("token")
    };
    let message_id = bot
        .fake
        .messages(bot.chat)
        .last()
        .map(|(id, _)| *id)
        .expect("active message");

    bot.press_data(&token, message_id).await;

    let timezone: Option<String> =
        sqlx::query_scalar("SELECT timezone FROM users WHERE telegram_user_id = $1")
            .bind(bot.chat)
            .fetch_one(&db.pool)
            .await
            .expect("timezone");
    assert_eq!(timezone.as_deref(), Some("Europe/Moscow"));
    assert!(
        bot.fake.answers().iter().any(|(_, text, _)| {
            text.as_deref()
                .is_some_and(|text| text.contains("проверьте введённые данные"))
        }),
        "бот должен сообщить об ошибке: {:?}",
        bot.fake.answers()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn owner_manages_family_and_journal_hides_secrets() {
    let db = db().await;
    let mut owner = onboarded_named(&db.pool, "Алексей").await;
    let mut member = invite(&mut owner, &db.pool, "Мария").await;
    member.assert_screen_contains("Добро пожаловать в «Семья»!");

    // Владелец получил уведомление о новом участнике.
    let notice = owner
        .fake
        .messages(owner.chat)
        .into_iter()
        .any(|(_, html)| html.contains("присоединился(-ась) Мария"));
    assert!(notice, "{:?}", owner.fake.messages(owner.chat));

    // Участник не видит разделов владельца.
    member.press("Настройки").await;
    assert!(member.find_button("Семья").is_none());
    assert!(member.find_button("Журнал").is_none());
    member.press("Опасное").await;
    member.assert_has_button("Выйти");

    // Журнал владельца: событие присоединения без кода и хешей.
    owner.press("Настройки").await;
    owner.press("Журнал").await;
    owner.assert_screen_contains("Присоединился(-ась): Мария");
    owner.assert_screen_contains("Создано приглашение");
    owner.assert_screen_contains("Приглашение использовано");
    let screen = owner.screen();
    assert!(!screen.contains("start="));
    assert!(!screen.contains("t.me"));

    let invite_codes: Vec<String> = sqlx::query_scalar("SELECT code_hash FROM invites")
        .fetch_all(&db.pool)
        .await
        .expect("hashes");
    for hash in invite_codes {
        assert!(!screen.contains(&hash));
    }
    let payloads: Vec<String> = sqlx::query_scalar(
        "SELECT payload::text FROM audit_events
         WHERE household_id = $1",
    )
    .bind(household_for(&db.pool, owner.chat).await.household_id)
    .fetch_all(&db.pool)
    .await
    .expect("payloads");
    assert!(
        payloads
            .iter()
            .all(|payload| !payload.contains("tlq_") && !payload.contains("start="))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invites_can_be_revoked_and_are_single_use() {
    let db = db().await;
    let mut owner = onboarded_named(&db.pool, "Алексей").await;
    owner.press("Настройки").await;
    owner.press("Семья").await;
    owner.press("Пригласить").await;
    owner.press("Создать приглашение").await;
    let screen = owner.screen();
    let start = screen.find("start=").expect("link") + 6;
    let code: String = screen[start..]
        .chars()
        .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '-' || *ch == '_')
        .collect();
    owner.assert_screen_contains("Действует 72 часа");

    owner.press("Назад").await;
    owner.assert_screen_contains("Активных: 1 из 5");
    owner.press("Отозвать").await;
    owner.assert_screen_contains("Активных приглашений нет");

    let mut late = Harness::sharing(&db.pool, next_telegram_id(), owner.fake.clone());
    late.text(&format!("/start {code}")).await;
    late.assert_screen_contains("Это приглашение отозвано");
    late.assert_screen_contains("Как назовём ваше пространство");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sixth_invite_within_an_hour_is_refused_with_retry_time() {
    let db = db().await;
    let mut owner = onboarded_named(&db.pool, "Алексей").await;
    owner.press("Настройки").await;
    owner.press("Семья").await;
    owner.press("Пригласить").await;

    for _ in 0..5 {
        owner.press("Создать приглашение").await;
        owner.press("Назад").await;
    }
    owner.press("Создать приглашение").await;

    let toast = owner
        .fake
        .answers()
        .into_iter()
        .filter_map(|(_, text, _)| text)
        .find(|text| text.contains("приглашений в час"))
        .expect("the owner should see the invite rate limit");
    assert!(
        toast.contains("Попробуйте через"),
        "the toast must tell when to retry: {toast}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn removal_leaving_and_transfer_notify_participants() {
    let db = db().await;
    let mut owner = onboarded_named(&db.pool, "Алексей").await;
    let mut maria = invite(&mut owner, &db.pool, "Мария").await;
    let mut boris = invite(&mut owner, &db.pool, "Борис").await;

    // Участник выходит сам.
    boris.press("Настройки").await;
    boris.press("Опасное").await;
    boris.press("Выйти").await;
    boris.assert_screen_contains("Выйти из пространства?");
    boris.press("Да, выйти").await;
    owner.deliver().await;
    let owner_notices: Vec<String> = owner
        .fake
        .messages(owner.chat)
        .into_iter()
        .map(|(_, h)| h)
        .collect();
    assert!(
        owner_notices
            .iter()
            .any(|html| html.contains("Борис вышел(-ла)"))
    );

    // Владелец удаляет участника; тот получает нейтральное уведомление.
    owner.press("Настройки").await;
    owner.press("Семья").await;
    owner.press("Мария").await;
    owner.press("Удалить").await;
    owner.assert_screen_contains("Удалить участника «Мария»?");
    owner.press("Да, удалить").await;
    owner.assert_screen_contains("Семья");
    maria.deliver().await;
    let maria_notices: Vec<String> = owner
        .fake
        .messages(maria.chat)
        .into_iter()
        .map(|(_, html)| html)
        .collect();
    assert!(
        maria_notices
            .iter()
            .any(|html| html.contains("доступ к семейному пространству прекращён"))
    );

    // Удалённый участник больше не работает с данными: бот предлагает начать заново.
    maria.text("/menu").await;
    maria.assert_screen_contains("Как назовём ваше пространство");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ownership_transfer_changes_available_sections() {
    let db = db().await;
    let mut owner = onboarded_named(&db.pool, "Алексей").await;
    let mut maria = invite(&mut owner, &db.pool, "Мария").await;

    owner.press("Настройки").await;
    owner.press("Семья").await;
    owner.press("Мария").await;
    owner.press("Передать владение").await;
    owner.press("Да, передать").await;
    assert!(
        owner
            .fake
            .answers()
            .iter()
            .any(|(_, text, _)| text.as_deref() == Some("Владение передано"))
    );

    owner.assert_screen_contains("Настройки");
    assert!(owner.find_button("Семья").is_none());
    owner.press("Опасное").await;
    owner.assert_has_button("Выйти");
    maria.deliver().await;
    let notice = maria
        .fake
        .messages(maria.chat)
        .into_iter()
        .any(|(_, h)| h.contains("Вам передано владение"));
    assert!(notice);
    maria.press("Настройки").await;
    maria.assert_has_button("Семья");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn household_deletion_requires_typing_its_name() {
    let db = db().await;
    let mut owner = onboarded_named(&db.pool, "Алексей").await;
    let mut maria = invite(&mut owner, &db.pool, "Мария").await;

    owner.press("Настройки").await;
    owner.press("Опасное").await;
    owner.press("Удалить пространство").await;
    owner.assert_screen_contains("Для подтверждения отправьте название");
    owner.text("Не то").await;
    owner.assert_screen_contains("Название не совпало");

    owner.text("Семья").await;
    let alive: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM households h JOIN users u ON u.household_id = h.id
         WHERE u.telegram_user_id = $1 AND h.deleted_at IS NULL",
    )
    .bind(owner.chat)
    .fetch_one(&db.pool)
    .await
    .expect("count");
    assert_eq!(alive, 0);

    maria.deliver().await;
    let notified = maria
        .fake
        .messages(maria.chat)
        .into_iter()
        .any(|(_, h)| h.contains("удалено владельцем"));
    assert!(notified);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_replayed_foreign_and_expired_buttons_are_handled() {
    let db = db().await;
    let mut bot = onboarded_named(&db.pool, "Alice").await;
    let mut other =
        Harness::sharing(&db.pool, next_telegram_id(), bot.fake.clone()).named("Mallory");

    // Повторное нажатие: действие не повторяется, экран актуален.
    let (message_id, html) = bot.active();
    let tasks = support::telegram::buttons(&html)
        .into_iter()
        .find(|(label, _)| label.contains("Задачи"))
        .expect("tasks")
        .1;
    bot.press_data(&tasks, message_id).await;
    bot.press_data(&tasks, message_id).await;
    assert!(
        bot.fake
            .answers()
            .iter()
            .any(|(_, text, _)| text.as_deref() == Some("Это действие уже выполнено."))
    );
    assert_eq!(bot.fake.messages(bot.chat).len(), 1);

    // Чужой токен: отказ без выполнения.
    let foreign = support::telegram::buttons(&bot.screen())[0].1.clone();
    other.press_data(&foreign, message_id).await;
    assert!(other.fake.answers().iter().any(|(_, text, alert)| *alert
        && text.as_deref() == Some("Эта кнопка предназначена другому пользователю.")));

    // Заменённое сообщение: кнопки старого сообщения устарели.
    let old_buttons = bot.buttons();
    let (old_id, _) = bot.active();
    bot.fake
        .reject_edits(Some(tasklerq::telegram::api::TelegramError::MessageGone));
    bot.press("Назад").await;
    bot.fake.reject_edits(None);
    let (new_id, _) = bot.active();
    assert_ne!(old_id, new_id);
    assert_eq!(
        bot.fake.messages(bot.chat).len(),
        1,
        "old message must be deleted"
    );
    bot.fake.clear_calls();
    bot.press_data(&old_buttons[0].1, old_id).await;
    assert!(
        bot.fake
            .answers()
            .iter()
            .any(|(_, text, _)| text.as_deref().is_some_and(|t| t.contains("устарело")))
    );
    assert!(bot.fake.sent().is_empty());

    // Истёкший токен.
    let data = bot.buttons()[0].1.clone();
    let (current, _) = bot.active();
    bot.advance(Duration::hours(25));
    bot.press_data(&data, current).await;
    assert!(
        bot.fake
            .answers()
            .iter()
            .any(|(_, text, _)| text.as_deref().is_some_and(|t| t.contains("устарело")))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_form_asks_to_start_over() {
    let db = db().await;
    let mut bot = onboarded_named(&db.pool, "Alice").await;
    bot.press("Добавить").await;
    bot.advance(Duration::minutes(16));
    bot.text("Просроченная форма").await;
    bot.assert_screen_contains("правка устарела");
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM conversation_states WHERE chat_id = $1")
            .bind(bot.chat)
            .fetch_one(&db.pool)
            .await
            .expect("count");
    assert_eq!(count, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notification_actions_are_idempotent() {
    use tasklerq::telegram::bot::notify;
    use tasklerq::telegram::outbox::{self, Outgoing};

    let db = support::isolated_db().await;
    let mut bot = onboarded_named(&db.pool, "Alice").await;
    // Планировщик не должен прислать сводку или отложить snooze тихими часами.
    sqlx::query(
        "UPDATE users
         SET settings = jsonb_set(
             settings,
             '{notifications}',
             '{\"digest_enabled\": false, \"quiet_hours_enabled\": false}'::jsonb,
             true
         )
         WHERE telegram_user_id = $1",
    )
    .bind(bot.chat)
    .execute(&db.pool)
    .await
    .expect("notification settings");
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    let item = create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &actor,
        new_item(household.tasks_list_id, "Оплатить интернет"),
    )
    .await;
    let list = tasklerq::storage::repositories::lists::get(
        &mut db.pool.acquire().await.expect("conn"),
        household.household_id,
        household.tasks_list_id,
    )
    .await
    .expect("list");

    // Напоминание ставится в outbox так же, как это сделает планировщик.
    let tz: chrono_tz::Tz = "Europe/Moscow".parse().expect("tz");
    let ui = notify::reminder_ui(&item, &list, tz, bot.now, Some(15));
    {
        let mut conn = db.pool.acquire().await.expect("conn");
        let html = notify::render_notice(&mut conn, bot.chat, household.household_id, &ui, bot.now)
            .await
            .expect("render");
        outbox::enqueue(
            &mut conn,
            bot.chat,
            &Outgoing::Message {
                html,
                rich: true,
                delete_after: None,
            },
            None,
            bot.now,
        )
        .await
        .expect("enqueue");
    }
    bot.deliver().await;
    bot.assert_latest_contains("⏰ Через 15 минут");
    bot.assert_latest_contains("Оплатить интернет");
    let (notice_id, notice_html) = bot.latest();
    let button_of = |label: &str| {
        support::telegram::buttons(&notice_html)
            .into_iter()
            .find(|(text, _)| text.contains(label))
            .expect("button")
            .1
    };
    let done = button_of("Выполнено");

    // Отложить: исходное сообщение обновляется, а доставку создаёт планировщик.
    let snooze = button_of("на час");
    bot.press_data(&snooze, notice_id).await;
    bot.assert_latest_contains("Напомню");
    let scheduled: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM notification_deliveries
         WHERE item_id = $1 AND kind = 'snooze' AND status = 'pending'",
    )
    .bind(item.id)
    .fetch_one(&db.pool)
    .await
    .expect("snooze deliveries");
    assert_eq!(scheduled, 1);

    // «Выполнено» выполняется один раз; повторное нажатие ничего не делает.
    bot.press_data(&done, notice_id).await;
    bot.assert_latest_contains("✅ Выполнено: Оплатить интернет");
    let status: String = sqlx::query_scalar("SELECT status::text FROM items WHERE id = $1")
        .bind(item.id)
        .fetch_one(&db.pool)
        .await
        .expect("status");
    assert_eq!(status, "done");
    let before = bot.fake.calls().len();
    bot.press_data(&done, notice_id).await;
    assert_eq!(
        bot.fake.calls().len(),
        before,
        "repeated tap must not change messages"
    );
    assert!(
        bot.fake
            .answers()
            .iter()
            .any(|(_, text, _)| text.as_deref() == Some("Это действие уже выполнено."))
    );
    let events: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM item_events WHERE item_id = $1 AND event_type = 'completed'",
    )
    .bind(item.id)
    .fetch_one(&db.pool)
    .await
    .expect("events");
    assert_eq!(events, 1);

    // Отмена возвращает напоминание с актуальной версией.
    let undo = support::telegram::buttons(&bot.latest().1)
        .into_iter()
        .find(|(text, _)| text.contains("Отменить"))
        .expect("undo")
        .1;
    bot.press_data(&undo, notice_id).await;
    bot.assert_latest_contains("⏰");

    // Отложенное напоминание приходит после наступления времени.
    let deliver_at = bot.now + Duration::minutes(61);
    scheduler(&db.pool)
        .tick(deliver_at)
        .await
        .expect("snooze tick");
    bot.advance(deliver_at - bot.now);
    bot.fake.clear_calls();
    bot.deliver().await;
    assert!(bot.fake.calls().iter().any(
        |call| matches!(call, Call::SendRich { html, .. } if html.contains("Оплатить интернет"))
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn open_from_notice_shows_card_in_navigation() {
    use tasklerq::telegram::bot::notify;
    use tasklerq::telegram::outbox::{self, Outgoing};

    let db = db().await;
    let mut bot = onboarded_named(&db.pool, "Alice").await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    let item = create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &actor,
        new_item(household.tasks_list_id, "Позвонить маме"),
    )
    .await;
    let list = tasklerq::storage::repositories::lists::get(
        &mut db.pool.acquire().await.expect("conn"),
        household.household_id,
        household.tasks_list_id,
    )
    .await
    .expect("list");
    let tz: chrono_tz::Tz = "Europe/Moscow".parse().expect("tz");
    let ui = notify::assignment_ui(&item, &list, tz, bot.now);
    {
        let mut conn = db.pool.acquire().await.expect("conn");
        let html = notify::render_notice(&mut conn, bot.chat, household.household_id, &ui, bot.now)
            .await
            .expect("render");
        outbox::enqueue(
            &mut conn,
            bot.chat,
            &Outgoing::Message {
                html,
                rich: true,
                delete_after: None,
            },
            None,
            bot.now,
        )
        .await
        .expect("enqueue");
    }
    bot.deliver().await;
    bot.assert_latest_contains("Вам назначили задачу");

    let (notice_id, notice_html) = bot.latest();
    let open = support::telegram::buttons(&notice_html)
        .into_iter()
        .find(|(text, _)| text.contains("Открыть"))
        .expect("open")
        .1;
    bot.press_data(&open, notice_id).await;
    // Карточка открылась отдельным экраном навигации, уведомление осталось.
    let messages = bot.fake.messages(bot.chat);
    assert!(
        messages
            .iter()
            .any(|(_, html)| html.contains("Вам назначили задачу"))
    );
    assert!(
        messages
            .iter()
            .any(|(_, html)| html.contains("Список: Задачи"))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn button_of_a_removed_member_is_stale_even_for_a_valid_token() {
    let db = db().await;
    let mut owner = onboarded_named(&db.pool, "Алексей").await;
    let mut maria = invite(&mut owner, &db.pool, "Мария").await;

    // У Марии на экране живые кнопки, выпущенные в household владельца.
    maria.press("Настройки").await;
    let (message_id, html) = maria.active();
    let data = support::telegram::buttons(&html)
        .into_iter()
        .find(|(label, _)| label.contains("Назад"))
        .expect("back button")
        .1;

    owner.press("Настройки").await;
    owner.press("Семья").await;
    owner.press("Мария").await;
    owner.press("Удалить").await;
    owner.press("Да, удалить").await;

    maria.press_data(&data, message_id).await;

    let toast = maria
        .fake
        .answers()
        .into_iter()
        .next_back()
        .and_then(|(_, text, _)| text);
    assert_eq!(toast.as_deref(), Some("Это действие уже неактуально."));
    maria.assert_screen_contains("Как назовём ваше пространство");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn member_cannot_open_owner_screens_even_with_a_valid_token() {
    let db = db().await;
    let mut owner = onboarded_named(&db.pool, "Алексей").await;
    let mut maria = invite(&mut owner, &db.pool, "Мария").await;
    let household = household_for(&db.pool, owner.chat).await;
    let household_id = household.household_id;
    let token = cli_token(&db.pool, household_id, "Home Assistant").await;
    maria.press("Настройки").await;
    let (message_id, _) = maria.active();
    assert!(maria.find_button("Интеграции").is_none());

    // Кнопок на эти экраны участнику не выдают, поэтому токены выпускаем вручную.
    for target in [
        Screen::Family,
        Screen::Journal { page: 0 },
        Screen::Invites,
        Screen::Integrations { page: 0 },
        Screen::ConfirmRevokeToken {
            token_id: token.record.id,
        },
    ] {
        let data = action_callback(
            &db.pool,
            maria.chat,
            household_id,
            Action::Go { to: target.clone() },
        )
        .await;

        maria.press_data(&data, message_id).await;

        let toast = maria
            .fake
            .answers()
            .into_iter()
            .next_back()
            .and_then(|(_, text, _)| text);
        assert_eq!(
            toast.as_deref(),
            Some("Это доступно только владельцу."),
            "target {target:?}"
        );
    }

    // Токены участнику не показываются и не меняются.
    maria.assert_screen_contains("Настройки");
    assert!(!maria.screen().contains("Home Assistant"));
    assert!(!maria.screen().contains(&token.record.prefix));
    let tokens_before: i64 =
        sqlx::query_scalar("SELECT count(*) FROM api_tokens WHERE household_id = $1")
            .bind(household_id)
            .fetch_one(&db.pool)
            .await
            .expect("token count");

    // Действия с токенами отклоняются и члену household.
    for action in [
        Action::IssueHaToken,
        Action::RevokeToken {
            token_id: token.record.id,
        },
    ] {
        let data = action_callback(&db.pool, maria.chat, household_id, action).await;
        maria.press_data(&data, message_id).await;
        let toast = maria
            .fake
            .answers()
            .into_iter()
            .next_back()
            .and_then(|(_, text, _)| text);
        assert_eq!(
            toast.as_deref(),
            Some("Недостаточно прав для этого действия."),
            "a member must not issue or revoke tokens"
        );
    }
    maria.assert_screen_contains("Настройки");
    let tokens_after: i64 =
        sqlx::query_scalar("SELECT count(*) FROM api_tokens WHERE household_id = $1")
            .bind(household_id)
            .fetch_one(&db.pool)
            .await
            .expect("token count");
    assert_eq!(
        tokens_after, tokens_before,
        "a rejected issue must not create a token"
    );
    let revoked_at: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT revoked_at FROM api_tokens WHERE id = $1")
            .bind(token.record.id)
            .fetch_one(&db.pool)
            .await
            .expect("token");
    assert!(revoked_at.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn user_html_is_escaped_on_list_and_card_screens() {
    let db = db().await;
    let mut bot = onboarded_named(&db.pool, "Alice").await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    let title = "<script>alert(1)</script> & <b>\"x\"</b>";
    let item = create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &actor,
        new_item(household.tasks_list_id, title),
    )
    .await;
    assert_eq!(item.title, title, "the title must be stored as plain text");

    bot.press("Задачи").await;
    let list = bot.screen();
    assert!(!list.contains("<script>"), "{list}");
    assert!(
        list.contains("&lt;script&gt;alert(1)&lt;/script&gt;"),
        "{list}"
    );
    assert!(list.contains("&amp;"), "{list}");
    assert!(!list.contains("<b>\"x\"</b>"), "{list}");

    bot.press("script").await;
    let card = bot.screen();
    assert!(!card.contains("<script>"), "{card}");
    assert!(
        card.contains("&lt;script&gt;alert(1)&lt;/script&gt;"),
        "{card}"
    );
    assert!(card.contains("&amp;"), "{card}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn control_characters_are_removed_from_bot_quick_add_and_form_input() {
    let db = db().await;
    let mut bot = onboarded_named(&db.pool, "Alice").await;

    // Быстрый ввод: черновик формы и сохранённый элемент не содержат
    // управляющих символов.
    bot.text("Купить\u{7} молоко\u{6}").await;
    let data: String =
        sqlx::query_scalar("SELECT data::text FROM conversation_states WHERE chat_id = $1")
            .bind(bot.chat)
            .fetch_one(&db.pool)
            .await
            .expect("conversation");
    let form: Form = serde_json::from_str(&data).expect("form data");
    match form {
        Form::Editor(editor) => assert_eq!(editor.draft.title, "Купить молоко"),
        other => panic!("expected an editor, got {other:?}"),
    }
    bot.press("Сохранить").await;
    let title: String = sqlx::query_scalar(
        "SELECT title FROM items WHERE household_id = (
             SELECT household_id FROM users WHERE telegram_user_id = $1
         ) AND deleted_at IS NULL",
    )
    .bind(bot.chat)
    .fetch_one(&db.pool)
    .await
    .expect("item");
    assert_eq!(title, "Купить молоко");

    // Редактор: ввод названия очищается до сохранения в черновике.
    bot.press("Добавить").await;
    bot.text("Форма\u{1} ввод\u{2}").await;
    let data: String =
        sqlx::query_scalar("SELECT data::text FROM conversation_states WHERE chat_id = $1")
            .bind(bot.chat)
            .fetch_one(&db.pool)
            .await
            .expect("conversation");
    let form: Form = serde_json::from_str(&data).expect("form data");
    match form {
        Form::Editor(editor) => assert_eq!(editor.draft.title, "Форма ввод"),
        other => panic!("expected an editor form, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn integrations_show_only_active_tokens_for_owner() {
    let db = db().await;
    let mut owner = onboarded_named(&db.pool, "Алексей").await;
    let household = household_for(&db.pool, owner.chat).await;

    let active = cli_token(&db.pool, household.household_id, "Home Assistant").await;
    let revoked = cli_token(&db.pool, household.household_id, "Отозванный").await;
    admin::revoke_token(&db.pool, revoked.record.id, Utc::now())
        .await
        .expect("revoke");
    let expired = cli_token(&db.pool, household.household_id, "Истёкший").await;
    sqlx::query("UPDATE api_tokens SET expires_at = now() - interval '1 hour' WHERE id = $1")
        .bind(expired.record.id)
        .execute(&db.pool)
        .await
        .expect("expire");

    // Запрос с токеном отмечает последнее использование.
    let app = support::app(&db.pool);
    let (status, _) = support::request(
        &app,
        Method::GET,
        "/api/v1/household",
        Some(&active.token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    owner.press("Настройки").await;
    owner.assert_has_button("Интеграции");
    owner.press("Интеграции").await;

    owner.assert_screen_contains("Интеграции");
    owner.assert_screen_contains("Home Assistant");
    owner.assert_screen_contains(&active.record.prefix);
    owner.assert_screen_contains("ha:read");
    owner.assert_screen_contains("ha:write");
    owner.assert_screen_contains("Последнее использование");
    assert!(!owner.screen().contains("не использовался"));
    assert!(!owner.screen().contains("Отозванный"));
    assert!(!owner.screen().contains("Истёкший"));
    assert!(!owner.screen().contains(&active.token));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn integrations_paginate_tokens_and_clamp_the_page() {
    let db = db().await;
    let mut owner = onboarded_named(&db.pool, "Алексей").await;
    let household = household_for(&db.pool, owner.chat).await;
    for index in 0..25 {
        cli_token(
            &db.pool,
            household.household_id,
            &format!("Токен {index:02}"),
        )
        .await;
    }

    owner.press("Настройки").await;
    owner.press("Интеграции").await;

    let pages = 25usize.div_ceil(PAGE_SIZE);
    let first = owner.screen();
    assert_eq!(
        first.matches("🔑 ").count(),
        PAGE_SIZE,
        "the first page holds exactly PAGE_SIZE tokens"
    );
    assert!(
        first.len() < 4096,
        "the page must fit into a Telegram message: {} chars",
        first.len()
    );
    assert!(first.contains(&format!("Страница 1 из {pages}")));
    assert!(first.contains("Выпустить токен"));
    assert!(first.contains("Назад"));
    assert!(owner.find_button("→").is_some(), "there is a next page");
    assert!(
        owner.find_button("←").is_none(),
        "the first page has no previous page"
    );
    let rows = first.matches("<tg-button-row>").count();
    assert!(
        rows <= PAGE_SIZE + 2,
        "pagination must not exceed other list screens: {rows} rows"
    );

    owner.press("→").await;
    assert_eq!(owner.screen().matches("🔑 ").count(), PAGE_SIZE);
    assert!(owner.screen().contains(&format!("Страница 2 из {pages}")));

    owner.press("→").await;
    owner.press("→").await;
    let last = owner.screen();
    assert_eq!(
        last.matches("🔑 ").count(),
        25 - PAGE_SIZE * (pages - 1),
        "the last page contains the remainder"
    );
    assert!(last.contains(&format!("Страница {pages} из {pages}")));
    assert!(
        owner.find_button("→").is_none(),
        "the last page has no next page"
    );

    // Страница за пределами открывает последнюю существующую.
    let data = action_callback(
        &db.pool,
        owner.chat,
        household.household_id,
        Action::Go {
            to: Screen::Integrations { page: 999 },
        },
    )
    .await;
    let (message_id, _) = owner.active();
    owner.press_data(&data, message_id).await;
    assert_eq!(
        owner.screen().matches("🔑 ").count(),
        25 - PAGE_SIZE * (pages - 1)
    );
    assert!(
        owner
            .screen()
            .contains(&format!("Страница {pages} из {pages}"))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn owner_revokes_token_and_api_rejects_it() {
    let db = db().await;
    let mut owner = onboarded_named(&db.pool, "Алексей").await;
    let household = household_for(&db.pool, owner.chat).await;
    let token = cli_token(&db.pool, household.household_id, "Home Assistant").await;

    owner.press("Настройки").await;
    owner.press("Интеграции").await;
    owner.press("Отозвать").await;
    owner.assert_screen_contains("Отозвать токен");
    owner.assert_screen_contains("перестанет работать");
    owner.press("Отозвать").await;

    owner.assert_screen_contains("Токен отозван.");
    owner.assert_screen_contains("Активных токенов нет.");
    let revoked_at: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT revoked_at FROM api_tokens WHERE id = $1")
            .bind(token.record.id)
            .fetch_one(&db.pool)
            .await
            .expect("token");
    assert!(revoked_at.is_some());

    let (actor_id, payload): (Option<Uuid>, serde_json::Value) = sqlx::query_as(
        "SELECT actor_user_id, payload FROM audit_events
         WHERE household_id = $1 AND event_type = 'token_revoked'
         ORDER BY created_at DESC, id DESC
         LIMIT 1",
    )
    .bind(household.household_id)
    .fetch_one(&db.pool)
    .await
    .expect("audit event");
    assert_eq!(actor_id, Some(household.owner_id));
    assert_eq!(payload["token_id"], serde_json::json!(token.record.id));

    // Повторная устаревшая кнопка не пишет второй аудит.
    let data = action_callback(
        &db.pool,
        owner.chat,
        household.household_id,
        Action::RevokeToken {
            token_id: token.record.id,
        },
    )
    .await;
    let (message_id, _) = owner.active();
    owner.press_data(&data, message_id).await;
    owner.assert_screen_contains("Токен уже отозван.");
    let events: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events
         WHERE household_id = $1 AND event_type = 'token_revoked'",
    )
    .bind(household.household_id)
    .fetch_one(&db.pool)
    .await
    .expect("audit count");
    assert_eq!(events, 1);

    let app = support::app(&db.pool);
    let (status, _) = support::request(
        &app,
        Method::GET,
        "/api/v1/household",
        Some(&token.token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreign_token_revoke_is_not_found_and_does_not_change_it() {
    let db = db().await;
    let mut owner = onboarded_named(&db.pool, "Алексей").await;
    let household = household_for(&db.pool, owner.chat).await;
    let other = support::create_household(&db.pool).await;
    let foreign = cli_token(&db.pool, other.household_id, "Чужой").await;

    owner.press("Настройки").await;
    owner.press("Интеграции").await;

    // Подделанный экран подтверждения: имя чужого токена не раскрывается.
    let data = action_callback(
        &db.pool,
        owner.chat,
        household.household_id,
        Action::Go {
            to: Screen::ConfirmRevokeToken {
                token_id: foreign.record.id,
            },
        },
    )
    .await;
    let (message_id, _) = owner.active();
    owner.press_data(&data, message_id).await;
    let toast = owner
        .fake
        .answers()
        .into_iter()
        .next_back()
        .and_then(|(_, text, _)| text);
    assert_eq!(
        toast.as_deref(),
        Some("Не нашёл это — возможно, оно уже удалено.")
    );
    owner.assert_screen_contains("Интеграции");
    assert!(!owner.screen().contains("Чужой"));
    assert!(!owner.screen().contains(&foreign.record.prefix));

    // Подделанное действие отзыва в обход экрана подтверждения.
    let data = action_callback(
        &db.pool,
        owner.chat,
        household.household_id,
        Action::RevokeToken {
            token_id: foreign.record.id,
        },
    )
    .await;
    let (message_id, _) = owner.active();
    owner.press_data(&data, message_id).await;
    let toast = owner
        .fake
        .answers()
        .into_iter()
        .next_back()
        .and_then(|(_, text, _)| text);
    assert_eq!(
        toast.as_deref(),
        Some("Не нашёл это — возможно, оно уже удалено.")
    );
    owner.assert_screen_contains("Интеграции");

    let revoked_at: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT revoked_at FROM api_tokens WHERE id = $1")
            .bind(foreign.record.id)
            .fetch_one(&db.pool)
            .await
            .expect("token");
    assert!(revoked_at.is_none());
    let events: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events
         WHERE household_id = $1 AND event_type = 'token_revoked'",
    )
    .bind(other.household_id)
    .fetch_one(&db.pool)
    .await
    .expect("audit count");
    assert_eq!(events, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn former_owner_cannot_revoke_token_with_stale_button() {
    let db = db().await;
    let mut owner = onboarded_named(&db.pool, "Алексей").await;
    let _member = invite(&mut owner, &db.pool, "Мария").await;
    let household = household_for(&db.pool, owner.chat).await;
    let token = cli_token(&db.pool, household.household_id, "Home Assistant").await;
    let data = action_callback(
        &db.pool,
        owner.chat,
        household.household_id,
        Action::RevokeToken {
            token_id: token.record.id,
        },
    )
    .await;

    owner.press("Настройки").await;
    owner.press("Семья").await;
    owner.press("Мария").await;
    owner.press("Передать владение").await;
    owner.press("Да, передать").await;
    owner.assert_screen_contains("Настройки");

    let (message_id, _) = owner.active();
    owner.press_data(&data, message_id).await;

    let toast = owner
        .fake
        .answers()
        .into_iter()
        .next_back()
        .and_then(|(_, text, _)| text);
    assert_eq!(
        toast.as_deref(),
        Some("Недостаточно прав для этого действия.")
    );
    let revoked_at: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT revoked_at FROM api_tokens WHERE id = $1")
            .bind(token.record.id)
            .fetch_one(&db.pool)
            .await
            .expect("token");
    assert!(revoked_at.is_none());
    let events: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events
         WHERE household_id = $1 AND event_type = 'token_revoked'",
    )
    .bind(household.household_id)
    .fetch_one(&db.pool)
    .await
    .expect("audit count");
    assert_eq!(events, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn member_cannot_undo_an_item_deleted_by_the_owner() {
    let db = db().await;
    let mut owner = onboarded_named(&db.pool, "Алексей").await;
    let mut member = invite(&mut owner, &db.pool, "Мария").await;

    let household = household_for(&db.pool, owner.chat).await;
    let item = create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        new_item(household.tasks_list_id, "Удалённая владельцем"),
    )
    .await;
    let mut conn = db.pool.acquire().await.expect("connection");
    items::soft_delete(
        &mut conn,
        household.household_id,
        &household.owner(),
        item.id,
        Utc::now(),
    )
    .await
    .expect("soft delete");

    let data = action_callback(
        &db.pool,
        member.chat,
        household.household_id,
        Action::DeleteUndo {
            item_id: item.id,
            back: Screen::List {
                list_id: household.tasks_list_id,
                page: 0,
                edit: false,
                from_lists: false,
            },
        },
    )
    .await;
    let (message_id, _) = member.active();
    member.press_data(&data, message_id).await;

    let toast = member
        .fake
        .answers()
        .into_iter()
        .next_back()
        .and_then(|(_, text, _)| text);
    assert_eq!(
        toast.as_deref(),
        Some("Недостаточно прав для этого действия.")
    );

    let deleted: bool =
        sqlx::query_scalar("SELECT deleted_at IS NOT NULL FROM items WHERE id = $1")
            .bind(item.id)
            .fetch_one(&db.pool)
            .await
            .expect("item");
    assert!(deleted, "a forbidden undo must not restore the item");
}

/// Текстовое представление строк таблицы для поиска утечки секрета.
async fn row_texts(pool: &PgPool, query: &str, id: i64) -> Vec<String> {
    sqlx::query_scalar(query)
        .bind(id)
        .fetch_all(pool)
        .await
        .expect("rows")
}

fn assert_no_token_in(rows: &[String], token: &str, secret: &str, table: &str) {
    for row in rows {
        assert!(!row.contains(token), "{table} leaked the full token: {row}");
        assert!(
            !row.contains(secret),
            "{table} leaked the token after its prefix: {row}"
        );
    }
}

async fn issue_ha_from_integrations(owner: &mut Harness) {
    owner.press("Настройки").await;
    owner.press("Интеграции").await;
    owner.press("Выпустить токен").await;
}

fn secret_token(html: &str) -> String {
    let marker = "<tg-spoiler>";
    let start = html
        .find(marker)
        .expect("the token must be spoiler-wrapped")
        + marker.len();
    let end = html[start..]
        .find("</tg-spoiler>")
        .expect("the spoiler must be closed");
    html[start..start + end].to_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn owner_issues_ha_token_once_and_no_secret_is_stored() {
    let db = db().await;
    let mut owner = onboarded_named(&db.pool, "Алексей").await;
    let household = household_for(&db.pool, owner.chat).await;

    owner.press("Настройки").await;
    owner.press("Интеграции").await;
    let (anchor_id, _) = owner.active();
    owner.press("Выпустить токен").await;

    // Новый токен: имя, области и отсутствие срока.
    let (token_id, name, prefix, scopes, expires_at): (
        Uuid,
        String,
        String,
        Vec<String>,
        Option<DateTime<Utc>>,
    ) = sqlx::query_as(
        "SELECT id, name, prefix, scopes, expires_at FROM api_tokens
         WHERE household_id = $1 ORDER BY created_at DESC, id DESC LIMIT 1",
    )
    .bind(household.household_id)
    .fetch_one(&db.pool)
    .await
    .expect("issued token");
    assert_eq!(name, "Home Assistant");
    assert_eq!(
        scopes,
        ["ha:read", "ha:write", "capture:write"]
            .map(str::to_owned)
            .to_vec()
    );
    assert!(expires_at.is_none(), "the HA token must not expire");

    // Сообщение с открытым значением — отдельное, под спойлером, с кнопкой.
    let (secret_id, secret_html) = owner.latest();
    assert_ne!(
        secret_id, anchor_id,
        "the token message must be sent as a new message"
    );
    let token = secret_token(&secret_html);
    assert!(token.starts_with("tlq_"), "{token}");
    assert_eq!(
        prefix,
        token.chars().take(12).collect::<String>(),
        "the visible prefix must match the stored one"
    );
    assert!(
        secret_html.contains("Сообщение удалится через час"),
        "{secret_html}"
    );
    assert!(secret_html.contains("Я сохранил"), "{secret_html}");

    // Якорь отредактирован на месте и уже показывает новый токен по префиксу.
    assert!(owner.screen().contains(&prefix), "{}", owner.screen());
    let calls = owner.fake.calls();
    let edit = calls
        .iter()
        .rposition(|call| {
            matches!(call, Call::EditRich { id, html, .. } if *id == anchor_id && html.contains(&prefix))
        })
        .expect("the integrations anchor must be edited in place");
    let send = calls
        .iter()
        .rposition(|call| {
            matches!(call, Call::SendRich { id, html, .. } if *id == secret_id && html.contains("<tg-spoiler>"))
        })
        .expect("the secret message must be sent");
    assert!(
        edit < send,
        "the anchor must be updated before the secret message arrives below it"
    );

    // Аудит: имя, области и актор-владелец, без открытого значения.
    let (actor_id, payload): (Option<Uuid>, serde_json::Value) = sqlx::query_as(
        "SELECT actor_user_id, payload FROM audit_events
         WHERE household_id = $1 AND event_type = 'token_created'
         ORDER BY created_at DESC, id DESC LIMIT 1",
    )
    .bind(household.household_id)
    .fetch_one(&db.pool)
    .await
    .expect("token audit");
    assert_eq!(actor_id, Some(household.owner_id));
    assert_eq!(payload["name"], serde_json::json!("Home Assistant"));
    assert_eq!(
        payload["scopes"],
        serde_json::json!(["ha:read", "ha:write", "capture:write"])
    );
    assert_eq!(payload["prefix"], serde_json::json!(prefix));

    // Автоудаление запланировано от доставки ровно через час.
    let (secret_message_id, sent_at): (Option<i64>, Option<DateTime<Utc>>) = sqlx::query_as(
        "SELECT message_id, sent_at FROM outbox
         WHERE chat_id = $1 AND kind = 'message' ORDER BY seq DESC LIMIT 1",
    )
    .bind(owner.chat)
    .fetch_one(&db.pool)
    .await
    .expect("secret outbox row");
    assert_eq!(secret_message_id, Some(secret_id));
    let sent_at = sent_at.expect("the secret message must be delivered");
    let (delete_target, delete_at, key): (i64, DateTime<Utc>, Option<String>) = sqlx::query_as(
        "SELECT (payload->>'message_id')::bigint, next_attempt_at, dedupe_key FROM outbox
         WHERE chat_id = $1 AND kind = 'delete' AND dedupe_key LIKE 'expire:%'",
    )
    .bind(owner.chat)
    .fetch_one(&db.pool)
    .await
    .expect("scheduled delete");
    assert_eq!(delete_target, secret_id);
    assert_eq!((delete_at - sent_at).num_seconds(), 3_600);
    let expected_key = format!("expire:{}:{secret_id}", owner.chat);
    assert_eq!(key.as_deref(), Some(expected_key.as_str()));

    // После доставки открытого значения нет ни в одной таблице.
    let secret = &token[prefix.len()..];
    let outbox = row_texts(
        &db.pool,
        "SELECT row_to_json(t)::text FROM outbox t WHERE chat_id = $1",
        owner.chat,
    )
    .await;
    let views = row_texts(
        &db.pool,
        "SELECT row_to_json(t)::text FROM telegram_views t WHERE chat_id = $1",
        owner.chat,
    )
    .await;
    let callbacks = row_texts(
        &db.pool,
        "SELECT row_to_json(t)::text FROM callback_tokens t WHERE telegram_user_id = $1",
        owner.chat,
    )
    .await;
    let conversations = row_texts(
        &db.pool,
        "SELECT row_to_json(t)::text FROM conversation_states t WHERE chat_id = $1",
        owner.chat,
    )
    .await;
    let audits: Vec<String> = sqlx::query_scalar(
        "SELECT row_to_json(t)::text FROM audit_events t WHERE household_id = $1",
    )
    .bind(household.household_id)
    .fetch_all(&db.pool)
    .await
    .expect("audit rows");
    for (table, rows) in [
        ("outbox", &outbox),
        ("telegram_views", &views),
        ("callback_tokens", &callbacks),
        ("conversation_states", &conversations),
        ("audit_events", &audits),
    ] {
        assert_no_token_in(rows, &token, secret, table);
    }
    assert!(
        sqlx::query_scalar::<_, Option<String>>("SELECT prefix FROM api_tokens WHERE id = $1")
            .bind(token_id)
            .fetch_one(&db.pool)
            .await
            .expect("stored token")
            .is_some(),
        "the prefix is deliberately stored for identification"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn saved_secret_button_deletes_the_message_and_does_not_redraw() {
    let db = db().await;
    let mut owner = onboarded_named(&db.pool, "Алексей").await;
    issue_ha_from_integrations(&mut owner).await;
    let (secret_id, secret_html) = owner.latest();
    let data = support::telegram::buttons(&secret_html)
        .into_iter()
        .find(|(label, _)| label.contains("Я сохранил"))
        .expect("saved button")
        .1;
    let screens_before: i64 =
        sqlx::query_scalar("SELECT count(*) FROM outbox WHERE chat_id = $1 AND kind = 'screen'")
            .bind(owner.chat)
            .fetch_one(&db.pool)
            .await
            .expect("screen count");

    owner.fake.clear_calls();
    owner.press_data(&data, secret_id).await;

    assert_eq!(
        owner.fake.calls(),
        vec![Call::Delete {
            chat: owner.chat,
            id: secret_id,
        }],
        "the saved button must only delete its own message"
    );
    let key = format!("delete:{}:{secret_id}", owner.chat);
    let (target, stored_key, status): (i64, Option<String>, String) = sqlx::query_as(
        "SELECT message_id, dedupe_key, status::text FROM outbox
         WHERE chat_id = $1 AND kind = 'delete' AND dedupe_key = $2",
    )
    .bind(owner.chat)
    .bind(&key)
    .fetch_one(&db.pool)
    .await
    .expect("button delete row");
    assert_eq!(target, secret_id);
    assert_eq!(stored_key.as_deref(), Some(key.as_str()));
    assert_eq!(status, "sent");

    let screens_after: i64 =
        sqlx::query_scalar("SELECT count(*) FROM outbox WHERE chat_id = $1 AND kind = 'screen'")
            .bind(owner.chat)
            .fetch_one(&db.pool)
            .await
            .expect("screen count");
    assert_eq!(
        screens_after, screens_before,
        "dismissing the secret must not enqueue a screen"
    );

    // Запланированное автоудаление никуда не делось.
    let expire_status: String = sqlx::query_scalar(
        "SELECT status::text FROM outbox WHERE chat_id = $1 AND kind = 'delete'
           AND dedupe_key LIKE 'expire:%'",
    )
    .bind(owner.chat)
    .fetch_one(&db.pool)
    .await
    .expect("expire row");
    assert_eq!(expire_status, "pending");

    let toast = owner
        .fake
        .answers()
        .into_iter()
        .next_back()
        .and_then(|(_, text, _)| text);
    assert_eq!(toast.as_deref(), Some("Сообщение удалено."));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn former_owner_can_dismiss_the_secret_message() {
    let db = db().await;
    let mut owner = onboarded_named(&db.pool, "Алексей").await;
    let _maria = invite(&mut owner, &db.pool, "Мария").await;
    issue_ha_from_integrations(&mut owner).await;
    let (secret_id, secret_html) = owner.latest();
    let data = support::telegram::buttons(&secret_html)
        .into_iter()
        .find(|(label, _)| label.contains("Я сохранил"))
        .expect("saved button")
        .1;

    owner.press("Назад").await;
    owner.press("Семья").await;
    owner.press("Мария").await;
    owner.press("Передать владение").await;
    owner.press("Да, передать").await;

    owner.fake.clear_calls();
    owner.press_data(&data, secret_id).await;

    assert!(
        owner.fake.calls().iter().any(
            |call| matches!(call, Call::Delete { chat, id } if *chat == owner.chat && *id == secret_id)
        ),
        "a former owner must still be able to remove the secret: {:?}",
        owner.fake.calls()
    );
    assert!(
        owner
            .fake
            .messages(owner.chat)
            .iter()
            .all(|(id, _)| *id != secret_id),
        "the secret message must be gone from the chat"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_saved_button_is_rejected_but_the_expire_row_survives() {
    let db = db().await;
    let mut owner = onboarded_named(&db.pool, "Алексей").await;
    let _maria = invite(&mut owner, &db.pool, "Мария").await;
    issue_ha_from_integrations(&mut owner).await;
    let (secret_id, secret_html) = owner.latest();
    let data = support::telegram::buttons(&secret_html)
        .into_iter()
        .find(|(label, _)| label.contains("Я сохранил"))
        .expect("saved button")
        .1;

    // Владелец передаёт владение и выходит: его текущий household меняется.
    owner.press("Назад").await;
    owner.press("Семья").await;
    owner.press("Мария").await;
    owner.press("Передать владение").await;
    owner.press("Да, передать").await;
    owner.press("Опасное").await;
    owner.press("Выйти").await;
    owner.press("Да, выйти").await;

    owner.fake.clear_calls();
    owner.press_data(&data, secret_id).await;

    let toast = owner
        .fake
        .answers()
        .into_iter()
        .next_back()
        .and_then(|(_, text, _)| text);
    assert_eq!(toast.as_deref(), Some("Это действие уже неактуально."));
    assert!(
        !owner
            .fake
            .calls()
            .iter()
            .any(|call| matches!(call, Call::Delete { id, .. } if *id == secret_id)),
        "a button from another household must not delete the message: {:?}",
        owner.fake.calls()
    );
    let button_rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM outbox WHERE chat_id = $1 AND kind = 'delete'
           AND dedupe_key = $2",
    )
    .bind(owner.chat)
    .bind(format!("delete:{}:{secret_id}", owner.chat))
    .fetch_one(&db.pool)
    .await
    .expect("button deletes");
    assert_eq!(button_rows, 0);

    let expire_status: String = sqlx::query_scalar(
        "SELECT status::text FROM outbox WHERE chat_id = $1 AND kind = 'delete'
           AND dedupe_key LIKE 'expire:%'",
    )
    .bind(owner.chat)
    .fetch_one(&db.pool)
    .await
    .expect("expire row");
    assert_eq!(
        expire_status, "pending",
        "the scheduled auto-delete must survive a rejected button"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn action_labels_do_not_say_gotovo() {
    use tasklerq::storage::repositories::notifications::NotificationKind;
    use tasklerq::telegram::bot::notify::{self, BatchEntry, BatchEntryState};

    let db = db().await;
    let mut bot = onboarded_named(&db.pool, "Alice").await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    let task = create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &actor,
        new_item(household.tasks_list_id, "Оплатить интернет"),
    )
    .await;
    create_item(
        &db.pool,
        &household,
        household.shopping_list_id,
        &actor,
        new_item(household.shopping_list_id, "Молоко"),
    )
    .await;

    let mut screens: Vec<Vec<(String, String)>> = Vec::new();

    // Карточка задачи.
    bot.press("Задачи").await;
    bot.press("Оплатить интернет").await;
    let card = support::telegram::buttons(&bot.screen());
    assert!(
        card.iter().any(|(label, _)| label == "✓ Выполнено"),
        "{card:?}"
    );
    screens.push(card);
    bot.press("Назад").await;
    bot.press("Назад").await;

    // Карточка покупки: режим правки открывает её из строки.
    bot.press("Покупки").await;
    bot.press("Править").await;
    bot.press("Молоко").await;
    let purchase = support::telegram::buttons(&bot.screen());
    assert!(
        purchase.iter().any(|(label, _)| label == "✓ Куплено"),
        "{purchase:?}"
    );
    screens.push(purchase);
    bot.press("Назад").await;
    bot.press("Назад").await;

    // Выбор тегов в редакторе создания.
    bot.press("Добавить").await;
    bot.text("Новая задача").await;
    bot.press("Срок").await;
    bot.press("Без срока").await;
    bot.press("Приоритет").await;
    bot.press("Обычный").await;
    bot.press("Исполнитель").await;
    bot.press("Без исполнителя").await;
    bot.press("Теги").await;
    let tags = support::telegram::buttons(&bot.screen());
    assert!(
        tags.iter().any(|(label, _)| label == "Применить"),
        "{tags:?}"
    );
    screens.push(tags);
    bot.press("Назад").await;
    bot.press("Отмена").await;

    // Выбор тегов в форме правки.
    bot.press("Задачи").await;
    bot.press("Оплатить интернет").await;
    bot.press("Править").await;
    bot.press("Теги").await;
    let edit_tags = support::telegram::buttons(&bot.screen());
    assert!(
        edit_tags.iter().any(|(label, _)| label == "Применить"),
        "{edit_tags:?}"
    );
    screens.push(edit_tags);
    bot.press("Применить").await;

    // Уведомление о задаче и сводное уведомление.
    let list = tasklerq::storage::repositories::lists::get(
        &mut db.pool.acquire().await.expect("conn"),
        household.household_id,
        household.tasks_list_id,
    )
    .await
    .expect("list");
    let tz: chrono_tz::Tz = "Europe/Moscow".parse().expect("tz");
    let reminder = notify::reminder_ui(&task, &list, tz, bot.now, Some(15));
    let entries = [BatchEntry {
        item_id: task.id,
        kind: NotificationKind::Reminder,
        title: task.title.clone(),
        when: "Сегодня · Задачи".to_owned(),
        version: task.version,
        state: BatchEntryState::Open,
        is_event: false,
    }];
    let actions = notify::BatchActions::of(&entries);
    let batch = notify::batch_ui(
        Uuid::now_v7(),
        0,
        &entries,
        entries.len(),
        None,
        actions,
        tz,
    );
    {
        let mut conn = db.pool.acquire().await.expect("conn");
        for ui in [&reminder, &batch] {
            let html =
                notify::render_notice(&mut conn, bot.chat, household.household_id, ui, bot.now)
                    .await
                    .expect("render notice");
            screens.push(support::telegram::buttons(&html));
        }
    }

    // Приветствие после создания пространства.
    let mut fresh = Harness::new(&db.pool, next_telegram_id()).named("Bob");
    fresh.text("/start").await;
    fresh.text("Дом").await;
    fresh.press("Москва").await;
    fresh.assert_screen_contains("Пространство создано");
    screens.push(support::telegram::buttons(&fresh.screen()));

    for labels in &screens {
        assert!(
            !labels.iter().any(|(label, _)| label.contains("Готово")),
            "a button still says «Готово»: {labels:?}"
        );
    }
}

fn assert_navigation(bot: &Harness, expect_back: bool) {
    let labels: Vec<String> = support::telegram::buttons(&bot.screen())
        .into_iter()
        .map(|(label, _)| label)
        .collect();
    assert!(
        !labels
            .iter()
            .any(|label| label.contains("В меню") || label.contains("К списку")),
        "a legacy navigation label is still shown: {labels:?}\n{}",
        bot.screen()
    );
    if expect_back {
        assert!(
            labels.iter().any(|label| label == "⬅️ Назад"),
            "no «⬅️ Назад» on screen: {labels:?}\n{}",
            bot.screen()
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_from_menu_has_back_to_menu_and_no_home() {
    let db = db().await;
    let mut bot = onboarded_named(&db.pool, "Alice").await;

    bot.press("Задачи").await;
    bot.assert_screen_contains("Задачи · 0");
    let labels: Vec<String> = support::telegram::buttons(&bot.screen())
        .into_iter()
        .map(|(label, _)| label)
        .collect();
    assert!(labels.iter().any(|label| label == "⬅️ Назад"), "{labels:?}");
    assert!(!labels.iter().any(|label| label == "🏠 Меню"), "{labels:?}");

    bot.press("Назад").await;
    bot.assert_screen_contains("📋 Задачи · 0");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn card_from_list_starts_with_the_list_trail_and_has_home() {
    let db = db().await;
    let mut bot = onboarded_named(&db.pool, "Alice").await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &actor,
        new_item(household.tasks_list_id, "Оплатить интернет"),
    )
    .await;

    bot.press("Задачи").await;
    bot.press("Оплатить интернет").await;

    assert!(
        bot.screen().contains("<h3>Задачи › Оплатить интернет</h3>"),
        "card heading must show the list trail:\n{}",
        bot.screen()
    );
    let labels: Vec<String> = support::telegram::buttons(&bot.screen())
        .into_iter()
        .map(|(label, _)| label)
        .collect();
    assert!(labels.iter().any(|label| label == "⬅️ Назад"), "{labels:?}");
    assert!(labels.iter().any(|label| label == "🏠 Меню"), "{labels:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn long_card_title_stays_visible_below_the_clipped_trail() {
    let db = db().await;
    let mut bot = onboarded_named(&db.pool, "Alice").await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    let title = "Очень длинное название задачи, которое не помещается в хлебные крошки";
    create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &actor,
        new_item(household.tasks_list_id, title),
    )
    .await;

    bot.press("Задачи").await;
    bot.press("Очень длинное").await;
    let screen = bot.screen();
    assert!(
        screen.contains("<h3>Задачи › Очень длинное"),
        "the card must start with its trail:\n{screen}"
    );
    assert!(
        screen.contains(title),
        "the full title must remain visible below the trail:\n{screen}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn long_task_title_is_fully_visible_in_delete_confirmation() {
    let db = db().await;
    let mut bot = onboarded_named(&db.pool, "Alice").await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    let title = "Очень длинное название задачи, которое точно не помещается в хлебные крошки";
    create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &actor,
        new_item(household.tasks_list_id, title),
    )
    .await;

    bot.press("Задачи").await;
    bot.press("Очень длинное").await;
    bot.press("Удалить").await;

    let question = format!("Удалить «{title}»?");
    let screen = bot.screen();
    assert!(
        screen.contains(&question),
        "the delete confirmation must keep the full title:\n{screen}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_from_all_lists_returns_to_all_lists() {
    let db = db().await;
    let mut bot = onboarded_named(&db.pool, "Alice").await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &actor,
        new_item(household.tasks_list_id, "Оплатить интернет"),
    )
    .await;
    let mut conn = db.pool.acquire().await.expect("connection");
    lists::create(
        &mut conn,
        household.household_id,
        ListKind::Checklist,
        "Дача",
        Utc::now(),
    )
    .await
    .expect("extra list");
    drop(conn);

    // Перерисовать меню: третий список включает «🗂 Все списки».
    bot.text("/start").await;
    bot.press("Все списки").await;
    bot.assert_screen_contains("<h3>Все списки</h3>");

    bot.press("Задачи").await;
    assert!(
        bot.screen().contains("<h3>Все списки › Задачи · 1</h3>"),
        "a list opened from «Все списки» must keep that parent in its trail:\n{}",
        bot.screen()
    );

    // Карточка из такого списка возвращается в него с тем же признаком.
    bot.press("Оплатить интернет").await;
    bot.assert_screen_contains("Все списки › Задачи › Оплатить интернет");
    bot.press("Назад").await;
    assert!(
        bot.screen().contains("<h3>Все списки › Задачи · 1</h3>"),
        "the card must return to the list opened from «Все списки»:\n{}",
        bot.screen()
    );

    bot.press("Назад").await;
    bot.assert_screen_contains("<h3>Все списки</h3>");

    // Список из меню по-прежнему возвращает в меню.
    bot.press("Назад").await;
    bot.press("Задачи").await;
    assert!(
        bot.screen().contains("<h3>Задачи · 1</h3>"),
        "a list opened from the menu must not claim «Все списки» as parent:\n{}",
        bot.screen()
    );
    bot.press("Назад").await;
    bot.assert_screen_contains("📋 Задачи · 1");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_screen_has_back_and_no_legacy_return_labels() {
    let db = db().await;
    let mut owner = onboarded_named(&db.pool, "Алексей").await;
    let mut member = invite(&mut owner, &db.pool, "Мария").await;
    let household = household_for(&db.pool, owner.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &actor,
        new_item(household.tasks_list_id, "Оплатить интернет"),
    )
    .await;
    for n in 0..6 {
        let item = create_item(
            &db.pool,
            &household,
            household.shopping_list_id,
            &actor,
            new_item(household.shopping_list_id, &format!("Товар {n}")),
        )
        .await;
        sqlx::query("UPDATE items SET status = 'done', completed_at = now() WHERE id = $1")
            .bind(item.id)
            .execute(&db.pool)
            .await
            .expect("done");
    }
    let _token = cli_token(&db.pool, household.household_id, "Home Assistant").await;

    // Меню — единственный экран без «Назад».
    assert_navigation(&owner, false);

    owner.press("Задачи").await;
    assert_navigation(&owner, true);
    owner.press("Оплатить интернет").await;
    assert_navigation(&owner, true);
    owner.press("Удалить").await;
    assert_navigation(&owner, true);
    owner.press("Назад").await; // карточка
    owner.press("Назад").await; // список
    owner.press("Назад").await; // меню

    owner.press("Мои задачи").await;
    assert_navigation(&owner, true);
    owner.press("Назад").await;

    owner.press("Поиск").await;
    owner.text("Оплатить").await;
    assert_navigation(&owner, true);
    owner.press("Назад").await;

    owner.press("Покупки").await;
    assert_navigation(&owner, true);
    owner.press("Очистить").await;
    assert_navigation(&owner, true);
    owner.press("Назад").await; // список
    owner.press("Назад").await; // меню

    owner.press("Настройки").await;
    assert_navigation(&owner, true);
    for label in ["Уведомления", "Профиль", "Опасное", "Журнал", "Интеграции"]
    {
        owner.press(label).await;
        assert_navigation(&owner, true);
        owner.press("Назад").await;
        owner.assert_screen_contains("Настройки");
    }

    // Часовой пояс — ребёнок профиля, возврат после выбора ведёт в профиль.
    owner.press("Профиль").await;
    owner.press("Часовой пояс").await;
    assert_navigation(&owner, true);
    owner.assert_screen_contains("Настройки › Профиль › Часовой пояс");
    owner.press("Назад").await;
    owner.assert_screen_contains("Настройки › Профиль");
    owner.press("Назад").await;
    owner.assert_screen_contains("Настройки");

    // Время напоминания — ребёнок экрана уведомлений.
    owner.press("Уведомления").await;
    owner.press("⏱ За 15 мин").await;
    assert_navigation(&owner, true);
    owner.assert_screen_contains("Настройки › Уведомления › За сколько напоминать?");
    owner.press("Назад").await;
    owner.assert_screen_contains("Уведомления");
    owner.press("Назад").await;
    owner.assert_screen_contains("Настройки");

    // Опасное действие ведёт в форму подтверждения, отмена возвращает назад.
    owner.press("Опасное").await;
    owner.press("Удалить пространство").await;
    owner.assert_screen_contains("Для подтверждения отправьте название");
    owner.assert_has_button("Отмена");
    owner.press("Отмена").await;
    owner.assert_screen_contains("⚠️ Опасное");
    owner.press("Назад").await;
    owner.assert_screen_contains("Настройки");

    owner.press("Семья").await;
    assert_navigation(&owner, true);
    owner.press("Пригласить").await;
    assert_navigation(&owner, true);
    owner.press("Создать приглашение").await;
    assert_navigation(&owner, true);
    owner.press("Назад").await; // приглашения
    assert_navigation(&owner, true);
    owner.press("Назад").await; // семья
    owner.assert_screen_contains("Семья");

    owner.press("Мария").await;
    assert_navigation(&owner, true);
    owner.press("Удалить").await;
    assert_navigation(&owner, true);
    owner.press("Назад").await; // карточка участника
    owner.press("Передать владение").await;
    assert_navigation(&owner, true);
    owner.press("Назад").await; // карточка участника
    owner.press("Назад").await; // семья
    owner.press("Назад").await; // настройки
    owner.press("Назад").await; // меню

    owner.press("Настройки").await;
    owner.press("Интеграции").await;
    owner.press("Отозвать").await;
    assert_navigation(&owner, true);
    assert!(
        owner
            .screen()
            .contains("Настройки › Интеграции › Отозвать токен"),
        "the token confirm screen must show its trail:\n{}",
        owner.screen()
    );

    // Участнику доступен только выход; у подтверждения тоже есть навигация.
    member.press("Настройки").await;
    assert_navigation(&member, true);
    member.press("Опасное").await;
    assert_navigation(&member, true);
    member.press("Выйти").await;
    assert_navigation(&member, true);
    member.press("Назад").await; // опасное
    assert_navigation(&member, true);
    member.press("Назад").await; // настройки
    member.assert_screen_contains("Настройки");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn long_member_name_keeps_the_full_confirmation_question() {
    let db = db().await;
    let mut owner = onboarded_named(&db.pool, "Алексей").await;
    let maria = invite(&mut owner, &db.pool, "Мария").await;
    let long_name = "Александра-Екатерина";
    sqlx::query("UPDATE users SET display_name = $1 WHERE telegram_user_id = $2")
        .bind(long_name)
        .bind(maria.chat)
        .execute(&db.pool)
        .await
        .expect("long member name");

    owner.press("Настройки").await;
    owner.press("Семья").await;
    owner.press("Александра").await;
    owner.press("Передать владение").await;

    let question = format!("Передать владение «{long_name}»?");
    let screen = owner.screen();
    assert!(
        screen.contains(&question),
        "the confirmation must keep the full recipient:\n{screen}"
    );
    assert_eq!(
        screen.matches(&question).count(),
        1,
        "the confirmation question must not be duplicated:\n{screen}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_card_shows_fields_and_only_action_buttons() {
    let db = db().await;
    let mut bot = onboarded_named(&db.pool, "Alice").await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    let mut task = new_item(household.tasks_list_id, "Оплатить интернет");
    task.priority = ItemPriority::High;
    task.tags = vec!["дом".into()];
    task.subtasks = vec![support::subtask("Найти договор")];
    task.assignee_user_id = Some(household.owner_id);
    create_item(&db.pool, &household, household.tasks_list_id, &actor, task).await;

    bot.press("Задачи").await;
    bot.press("Оплатить интернет").await;

    for label in ["✓ Выполнено", "📅 Срок", "✏️ Править", "🗑 Удалить"]
    {
        bot.assert_has_button(label);
    }
    for label in [
        "Приоритет",
        "Исполнитель",
        "Теги",
        "Подзадачи",
        "Повторение",
        "Описание",
    ] {
        assert!(
            bot.find_button(label).is_none(),
            "the card must not keep the field button `{label}`:\n{}",
            bot.screen()
        );
    }
    bot.assert_has_button("Назад");
    bot.assert_has_button("Меню");

    bot.assert_screen_contains("Приоритет: 🟠 Высокий");
    bot.assert_screen_contains("🏷 дом");
    bot.assert_screen_contains("👤 Alice");
    bot.assert_screen_contains("🧩 Подзадачи: 0 из 1");
    bot.assert_screen_contains("Список: Задачи");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn purchase_card_shows_fields_and_only_action_buttons() {
    let db = db().await;
    let mut bot = onboarded_named(&db.pool, "Alice").await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    let mut purchase = new_item(household.shopping_list_id, "Молоко");
    purchase.quantity = Some(2.0);
    purchase.unit = Some("л".into());
    purchase.notes = Some("Безлактозное".into());
    purchase.assignee_user_id = Some(household.owner_id);
    create_item(
        &db.pool,
        &household,
        household.shopping_list_id,
        &actor,
        purchase,
    )
    .await;

    bot.press("Покупки").await;
    bot.press("Править").await;
    bot.press("Молоко").await;

    for label in ["✓ Куплено", "✏️ Править", "🗑 Удалить"] {
        bot.assert_has_button(label);
    }
    for label in ["Количество", "Единица", "Заметка", "Исполнитель"]
    {
        assert!(
            bot.find_button(label).is_none(),
            "the purchase card must not keep the field button `{label}`:\n{}",
            bot.screen()
        );
    }
    bot.assert_has_button("Назад");
    bot.assert_has_button("Меню");

    bot.assert_screen_contains("Количество: 2 л");
    bot.assert_screen_contains("👤 Alice");
    bot.assert_screen_contains("Список: Покупки");
    bot.assert_screen_contains("Безлактозное");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bought_purchase_card_offers_reopen_edit_and_delete() {
    let db = db().await;
    let mut bot = onboarded_named(&db.pool, "Alice").await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    let mut purchase = new_item(household.shopping_list_id, "Молоко");
    purchase.quantity = Some(2.0);
    purchase.unit = Some("л".into());
    create_item(
        &db.pool,
        &household,
        household.shopping_list_id,
        &actor,
        purchase,
    )
    .await;

    bot.press("Покупки").await;
    bot.press("Молоко").await;
    bot.press("Править").await;
    bot.press("Молоко").await;

    bot.assert_screen_contains("Покупки › Молоко");
    for label in ["↩️ Вернуть", "✏️ Править", "🗑 Удалить"] {
        bot.assert_has_button(label);
    }
    for label in ["✓ Куплено", "✓ Выполнено"] {
        assert!(
            bot.find_button(label).is_none(),
            "a bought purchase card must not keep `{label}`:\n{}",
            bot.screen()
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn long_description_is_a_collapsible_quote_and_short_one_is_plain() {
    let db = db().await;
    let mut bot = onboarded_named(&db.pool, "Alice").await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    let mut long = new_item(household.tasks_list_id, "Длинное описание");
    long.notes = Some(
        "Первая строка описания\nВторая строка описания\nТретья строка описания\nЧетвёртая строка описания"
            .into(),
    );
    create_item(&db.pool, &household, household.tasks_list_id, &actor, long).await;
    let mut short = new_item(household.tasks_list_id, "Короткое описание");
    short.notes = Some("Одна короткая строка".into());
    create_item(&db.pool, &household, household.tasks_list_id, &actor, short).await;

    bot.press("Задачи").await;
    bot.press("Длинное описание").await;
    let html = bot.screen();
    assert!(
        html.contains("<blockquote expandable>"),
        "a long description must be collapsed:\n{html}"
    );
    assert!(
        html.contains("Четвёртая строка описания"),
        "the description must be shown in full:\n{html}"
    );
    bot.press("Назад").await;

    bot.press("Короткое описание").await;
    let html = bot.screen();
    assert!(
        html.contains("<blockquote>"),
        "a short description stays a plain quote:\n{html}"
    );
    assert!(
        !html.contains("<blockquote expandable>"),
        "a short description must not be collapsed:\n{html}"
    );
    assert!(html.contains("Одна короткая строка"), "{html}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recurring_card_shows_the_next_occurrence_after_a_moved_slot() {
    let db = db().await;
    let moscow = chrono_tz::Tz::Europe__Moscow;
    let now = local_utc(
        moscow,
        NaiveDate::from_ymd_opt(2026, 11, 6).expect("date"),
        12,
        0,
    );
    let mut bot = onboarded_at(&db.pool, "Alice", now).await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    let mut input = new_item(household.tasks_list_id, "Полить цветы");
    input.due_date = Some(NaiveDate::from_ymd_opt(2026, 11, 10).expect("date"));
    input.recurrence = Some(support::recurrence(RecurrenceFreq::Weekly, vec![2]));
    let task = create_item(&db.pool, &household, household.tasks_list_id, &actor, input).await;

    let mut conn = db.pool.acquire().await.expect("connection");
    items::reschedule_occurrence(
        &mut conn,
        household.household_id,
        &actor,
        task.id,
        task.version,
        Due::AllDay(NaiveDate::from_ymd_opt(2026, 11, 6).expect("date")),
        "Europe/Moscow",
        now,
    )
    .await
    .expect("move the occurrence");
    drop(conn);

    bot.press("Задачи").await;
    bot.press("Полить цветы").await;
    bot.assert_screen_contains("🔁 Каждую неделю: вт");
    bot.assert_screen_contains("Следующее: вт, 17 ноя");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recurring_card_without_a_moved_slot_shows_the_next_rule_occurrence() {
    let db = db().await;
    let moscow = chrono_tz::Tz::Europe__Moscow;
    let now = local_utc(
        moscow,
        NaiveDate::from_ymd_opt(2026, 11, 6).expect("date"),
        12,
        0,
    );
    let mut bot = onboarded_at(&db.pool, "Alice", now).await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    let mut input = new_item(household.tasks_list_id, "Полить цветы");
    input.due_date = Some(NaiveDate::from_ymd_opt(2026, 11, 10).expect("date"));
    input.recurrence = Some(support::recurrence(RecurrenceFreq::Weekly, vec![2]));
    create_item(&db.pool, &household, household.tasks_list_id, &actor, input).await;

    bot.press("Задачи").await;
    bot.press("Полить цветы").await;
    bot.assert_screen_contains("🔁 Каждую неделю: вт");
    bot.assert_screen_contains("Следующее: вт, 17 ноя");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn card_edit_opens_the_editor_and_save_returns_to_the_card() {
    let db = db().await;
    let mut bot = onboarded_named(&db.pool, "Alice").await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &actor,
        new_item(household.tasks_list_id, "Оплатить интернет"),
    )
    .await;

    bot.press("Задачи").await;
    bot.press("Оплатить интернет").await;
    bot.press("Править").await;
    bot.assert_screen_contains("Название: Оплатить интернет");
    bot.assert_has_button("💾 Сохранить");

    bot.press("Приоритет").await;
    bot.press("Срочный").await;
    bot.press("Сохранить").await;

    bot.assert_screen_contains("Задачи › Оплатить интернет");
    bot.assert_screen_contains("Приоритет: 🔴 Срочный");
    for label in ["✓ Выполнено", "📅 Срок", "✏️ Править", "🗑 Удалить"]
    {
        bot.assert_has_button(label);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn new_task_editor_crumbs_do_not_mention_the_card() {
    let db = db().await;
    let mut bot = onboarded_named(&db.pool, "Alice").await;

    bot.press("Задачи").await;
    bot.press("Добавить").await;

    bot.assert_screen_contains("Задачи › ");
    bot.assert_screen_contains("Как назвать задачу?");
    assert!(
        !bot.screen().contains("Карточка"),
        "a new item has no card in its path:\n{}",
        bot.screen()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn editor_from_mine_keeps_the_mine_parent_in_crumbs() {
    let db = db().await;
    let mut bot = onboarded_named(&db.pool, "Alice").await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    let mut task = new_item(household.tasks_list_id, "Позвонить маме");
    task.assignee_user_id = Some(household.owner_id);
    create_item(&db.pool, &household, household.tasks_list_id, &actor, task).await;

    bot.press("Мои задачи").await;
    bot.press("Позвонить маме").await;
    bot.press("Править").await;

    bot.assert_screen_contains("Мои задачи › Карточка › Позвонить маме");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quick_due_tomorrow_keeps_the_time_and_returns_to_the_card() {
    let db = db().await;
    let moscow = chrono_tz::Tz::Europe__Moscow;
    let today = NaiveDate::from_ymd_opt(2027, 3, 15).expect("date");
    let now = local_utc(moscow, today, 12, 0);
    let mut bot = onboarded_at(&db.pool, "Alice", now).await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    let mut input = new_item(household.tasks_list_id, "Оплатить интернет");
    input.due_at = Some(local_utc(moscow, today, 18, 0));
    let task = create_item(&db.pool, &household, household.tasks_list_id, &actor, input).await;

    bot.press("Задачи").await;
    bot.press("Оплатить интернет").await;
    bot.press("Срок").await;
    for label in [
        "Сегодня",
        "Завтра",
        "+1 неделя",
        "Дата и время",
        "Без срока",
    ] {
        bot.assert_has_button(label);
    }

    bot.press("Завтра").await;
    bot.assert_screen_contains("Срок: завтра, 18:00");
    bot.assert_screen_contains("Задачи › Оплатить интернет");
    bot.assert_has_button("Выполнено");
    assert!(
        !bot.screen().contains("Сохранить"),
        "quick due saves without a save button:\n{}",
        bot.screen()
    );
    assert_eq!(
        item_due(&db.pool, task.id).await,
        (
            Some(local_utc(moscow, today + Duration::days(1), 18, 0)),
            None
        ),
        "the local time of day is kept"
    );
    assert_eq!(
        saved_screen(&db.pool, bot.chat).await["t"],
        serde_json::json!("item"),
        "the active screen is the card"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quick_due_text_moves_only_the_recurring_occurrence() {
    let db = db().await;
    let moscow = chrono_tz::Tz::Europe__Moscow;
    let today = NaiveDate::from_ymd_opt(2027, 3, 5).expect("date");
    let now = local_utc(moscow, today, 9, 0);
    let mut bot = onboarded_at(&db.pool, "Alice", now).await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    let mut input = new_item(household.tasks_list_id, "Оплатить счета");
    input.due_date = Some(today);
    input.recurrence = Some(support::recurrence(
        tasklerq::domain::models::RecurrenceFreq::Monthly,
        Vec::new(),
    ));
    let task = create_item(&db.pool, &household, household.tasks_list_id, &actor, input).await;

    bot.press("Задачи").await;
    bot.press("Оплатить счета").await;
    bot.press("Срок").await;
    bot.press("Дата и время").await;
    bot.assert_screen_contains("Введите срок");
    bot.text("07.03").await;

    bot.assert_screen_contains("Срок: послезавтра");
    bot.assert_screen_contains("Задачи › Оплатить счета");
    let moved = items::get(
        &mut db.pool.acquire().await.expect("conn"),
        household.household_id,
        task.id,
    )
    .await
    .expect("moved occurrence");
    assert_eq!(
        moved.due_date,
        Some(NaiveDate::from_ymd_opt(2027, 3, 7).unwrap())
    );
    assert_eq!(
        moved.recurrence.as_ref().expect("rule").occurrence_date,
        Some(today),
        "the original rule date stays in the slot"
    );

    bot.press("Выполнено").await;
    bot.assert_screen_contains("Следующее: пн, 5 апр");
    let completed = items::get(
        &mut db.pool.acquire().await.expect("conn"),
        household.household_id,
        task.id,
    )
    .await
    .expect("completed occurrence");
    assert_eq!(
        completed.due_date,
        Some(NaiveDate::from_ymd_opt(2027, 4, 5).unwrap()),
        "the next occurrence comes from the monthly rule, not the moved date"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quick_due_date_and_time_keeps_the_recurring_series_time() {
    let db = db().await;
    let moscow = chrono_tz::Tz::Europe__Moscow;
    let today = NaiveDate::from_ymd_opt(2027, 3, 1).expect("date");
    let now = local_utc(moscow, today, 9, 0);
    let mut bot = onboarded_at(&db.pool, "Alice", now).await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    let first = NaiveDate::from_ymd_opt(2027, 3, 2).expect("date");
    let mut input = new_item(household.tasks_list_id, "Встреча");
    input.due_at = Some(local_utc(moscow, first, 9, 0));
    input.recurrence = Some(support::recurrence(
        tasklerq::domain::models::RecurrenceFreq::Weekly,
        vec![2],
    ));
    let task = create_item(&db.pool, &household, household.tasks_list_id, &actor, input).await;

    bot.press("Задачи").await;
    bot.press("Встреча").await;
    bot.press("Срок").await;
    bot.press("Дата и время").await;
    bot.text("чт 12:00").await;

    bot.assert_screen_contains("Срок: чт, 4 мар, 12:00");
    bot.assert_screen_contains("Следующее: вт, 9 мар, 09:00");
    let moved = items::get(
        &mut db.pool.acquire().await.expect("conn"),
        household.household_id,
        task.id,
    )
    .await
    .expect("moved occurrence");
    let rule = moved.recurrence.as_ref().expect("rule");
    assert_eq!(rule.occurrence_date, Some(first));
    assert_eq!(
        rule.occurrence_time,
        chrono::NaiveTime::from_hms_opt(9, 0, 0),
        "the slot remembers the series time"
    );

    bot.press("Выполнено").await;
    bot.assert_screen_contains("Следующее: вт, 9 мар, 09:00");
    let completed = items::get(
        &mut db.pool.acquire().await.expect("conn"),
        household.household_id,
        task.id,
    )
    .await
    .expect("completed occurrence");
    assert_eq!(
        completed.due_at,
        Some(local_utc(
            moscow,
            NaiveDate::from_ymd_opt(2027, 3, 9).expect("date"),
            9,
            0
        )),
        "the next occurrence keeps the series time"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quick_due_week_later_counts_from_due_but_not_before_today() {
    let db = db().await;
    let moscow = chrono_tz::Tz::Europe__Moscow;
    let today = NaiveDate::from_ymd_opt(2027, 3, 15).expect("date");
    let now = local_utc(moscow, today, 12, 0);
    let mut bot = onboarded_at(&db.pool, "Alice", now).await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);

    let make = |title: &'static str, due: Option<NaiveDate>| {
        let pool = db.pool.clone();
        let actor = actor.clone();
        async move {
            let mut input = new_item(household.tasks_list_id, title);
            input.due_date = due;
            create_item(&pool, &household, household.tasks_list_id, &actor, input).await
        }
    };
    let future = make(
        "Будущая",
        Some(NaiveDate::from_ymd_opt(2027, 3, 20).unwrap()),
    )
    .await;
    let overdue = make(
        "Просроченная",
        Some(NaiveDate::from_ymd_opt(2027, 3, 1).unwrap()),
    )
    .await;
    let undated = make("Без даты", None).await;

    bot.press("Задачи").await;
    for (title, expected) in [
        (
            "Будущая",
            NaiveDate::from_ymd_opt(2027, 3, 27).expect("date"),
        ),
        (
            "Просроченная",
            NaiveDate::from_ymd_opt(2027, 3, 22).expect("date"),
        ),
        (
            "Без даты",
            NaiveDate::from_ymd_opt(2027, 3, 22).expect("date"),
        ),
    ] {
        bot.press(title).await;
        bot.press("Срок").await;
        bot.press("+1 неделя").await;
        let item_id = match title {
            "Будущая" => future.id,
            "Просроченная" => overdue.id,
            _ => undated.id,
        };
        assert_eq!(
            item_due(&db.pool, item_id).await.1,
            Some(expected),
            "{title}: a week counts from the due date but not before today"
        );
        bot.press("Назад").await; // карточка
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quick_due_today_keeps_all_day_and_gives_all_day_to_a_task_without_due() {
    let db = db().await;
    let moscow = chrono_tz::Tz::Europe__Moscow;
    let today = NaiveDate::from_ymd_opt(2027, 3, 15).expect("date");
    let now = local_utc(moscow, today, 12, 0);
    let mut bot = onboarded_at(&db.pool, "Alice", now).await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);

    let mut all_day = new_item(household.tasks_list_id, "Вседневная");
    all_day.due_date = Some(NaiveDate::from_ymd_opt(2027, 3, 20).unwrap());
    let all_day = create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &actor,
        all_day,
    )
    .await;
    let undated = create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &actor,
        new_item(household.tasks_list_id, "Без срока"),
    )
    .await;

    bot.press("Задачи").await;
    for (title, item_id) in [("Вседневная", all_day.id), ("Без срока", undated.id)]
    {
        bot.press(title).await;
        bot.press("Срок").await;
        bot.press("Сегодня").await;
        assert_eq!(
            item_due(&db.pool, item_id).await,
            (None, Some(today)),
            "{title}: «Сегодня» keeps the deadline all-day"
        );
        bot.press("Назад").await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quick_due_today_with_a_passed_time_marks_the_card_overdue() {
    let db = db().await;
    let moscow = chrono_tz::Tz::Europe__Moscow;
    let today = NaiveDate::from_ymd_opt(2027, 3, 15).expect("date");
    let now = local_utc(moscow, today, 12, 0);
    let mut bot = onboarded_at(&db.pool, "Alice", now).await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    let mut input = new_item(household.tasks_list_id, "Утренняя проверка");
    input.due_at = Some(local_utc(moscow, today, 9, 0));
    let task = create_item(&db.pool, &household, household.tasks_list_id, &actor, input).await;

    bot.press("Задачи").await;
    bot.press("Утренняя проверка").await;
    bot.press("Срок").await;
    bot.press("Сегодня").await;

    assert_eq!(
        item_due(&db.pool, task.id).await,
        (Some(local_utc(moscow, today, 9, 0)), None),
        "«Сегодня» keeps the time of day even if it has already passed"
    );
    bot.assert_screen_contains("· просрочено");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quick_due_none_is_hidden_for_recurring_and_clears_an_ordinary_due() {
    let db = db().await;
    let moscow = chrono_tz::Tz::Europe__Moscow;
    let today = NaiveDate::from_ymd_opt(2027, 3, 15).expect("date");
    let now = local_utc(moscow, today, 12, 0);
    let mut bot = onboarded_at(&db.pool, "Alice", now).await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);

    let mut ordinary = new_item(household.tasks_list_id, "Обычная");
    ordinary.due_date = Some(today);
    let ordinary = create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &actor,
        ordinary,
    )
    .await;
    let mut recurring = new_item(household.tasks_list_id, "Повторяющаяся");
    recurring.due_date = Some(today);
    recurring.recurrence = Some(support::recurrence(
        tasklerq::domain::models::RecurrenceFreq::Daily,
        Vec::new(),
    ));
    let recurring = create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &actor,
        recurring,
    )
    .await;

    bot.press("Задачи").await;
    bot.press("Обычная").await;
    bot.press("Срок").await;
    bot.assert_has_button("Без срока");
    bot.press("Без срока").await;
    bot.assert_screen_contains("Срок снят");
    assert_eq!(
        item_due(&db.pool, ordinary.id).await,
        (None, None),
        "«Без срока» clears the due entirely"
    );
    bot.press("Назад").await;

    bot.press("Повторяющаяся").await;
    bot.press("Срок").await;
    assert!(
        bot.find_button("Без срока").is_none(),
        "a recurring task has no «Без срока» button:\n{}",
        bot.screen()
    );
    assert_eq!(
        item_due(&db.pool, recurring.id).await,
        (None, Some(today)),
        "opening the quick due screen changes nothing"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quick_due_none_on_a_task_without_a_due_shows_the_card_without_a_banner() {
    let db = db().await;
    let moscow = chrono_tz::Tz::Europe__Moscow;
    let today = NaiveDate::from_ymd_opt(2027, 3, 15).expect("date");
    let now = local_utc(moscow, today, 12, 0);
    let mut bot = onboarded_at(&db.pool, "Alice", now).await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    let task = create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &actor,
        new_item(household.tasks_list_id, "Без срока"),
    )
    .await;

    bot.press("Задачи").await;
    bot.press("Без срока").await;
    bot.press("Срок").await;
    bot.press("Без срока").await;

    assert!(
        !bot.screen().contains("Срок снят"),
        "a task that never had a due date must not get the «Срок снят» banner:\n{}",
        bot.screen()
    );
    assert_eq!(
        saved_screen(&db.pool, bot.chat).await["t"],
        serde_json::json!("item"),
        "the card must be shown"
    );
    let version: i32 = sqlx::query_scalar("SELECT version FROM items WHERE id = $1")
        .bind(task.id)
        .fetch_one(&db.pool)
        .await
        .expect("version");
    assert_eq!(
        version, task.version,
        "«Без срока» on an undated task must not save anything"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_quick_due_shows_the_current_card_and_keeps_the_new_due() {
    let db = db().await;
    let moscow = chrono_tz::Tz::Europe__Moscow;
    let today = NaiveDate::from_ymd_opt(2027, 3, 15).expect("date");
    let now = local_utc(moscow, today, 12, 0);
    let mut bot = onboarded_at(&db.pool, "Alice", now).await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    let mut input = new_item(household.tasks_list_id, "Задача");
    input.due_date = Some(today);
    let task = create_item(&db.pool, &household, household.tasks_list_id, &actor, input).await;

    bot.press("Задачи").await;
    bot.press("Задача").await;
    bot.press("Срок").await;

    let changed = NaiveDate::from_ymd_opt(2027, 3, 20).expect("date");
    sqlx::query("UPDATE items SET title = 'Обновлённая', due_date = $2, version = version + 1 WHERE id = $1")
        .bind(task.id)
        .bind(changed)
        .execute(&db.pool)
        .await
        .expect("concurrent change");

    bot.press("Завтра").await;
    bot.assert_screen_contains("Данные уже изменились");
    bot.assert_screen_contains("Задачи › Обновлённая");
    assert_eq!(
        item_due(&db.pool, task.id).await,
        (None, Some(changed)),
        "a stale choice must not save anything"
    );
    assert_eq!(
        saved_screen(&db.pool, bot.chat).await["t"],
        serde_json::json!("item"),
        "the actual card is shown after a conflict"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completed_task_ignores_the_due_screen_and_quick_due() {
    let db = db().await;
    let moscow = chrono_tz::Tz::Europe__Moscow;
    let today = NaiveDate::from_ymd_opt(2027, 3, 15).expect("date");
    let now = local_utc(moscow, today, 12, 0);
    let mut bot = onboarded_at(&db.pool, "Alice", now).await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    let mut input = new_item(household.tasks_list_id, "Оплатить интернет");
    input.due_date = Some(today);
    let task = create_item(&db.pool, &household, household.tasks_list_id, &actor, input).await;

    bot.press("Задачи").await;
    bot.press("Оплатить интернет").await;
    bot.press("Срок").await;

    sqlx::query(
        "UPDATE items SET status = 'done', completed_at = now(), version = version + 1 WHERE id = $1",
    )
    .bind(task.id)
    .execute(&db.pool)
    .await
    .expect("complete the task");

    // Неизвестная команда перерисовывает активный экран со свежей версией.
    bot.text("/refresh").await;
    bot.assert_screen_contains("Задачи › Оплатить интернет");
    assert!(
        bot.find_button("Завтра").is_none(),
        "a completed task must not show the due screen:\n{}",
        bot.screen()
    );
    bot.assert_has_button("Вернуть");

    let back = Screen::List {
        list_id: household.tasks_list_id,
        page: 0,
        edit: false,
        from_lists: false,
    };
    let (version, _) = item_state(&db.pool, task.id).await;
    let token = action_callback(
        &db.pool,
        bot.chat,
        household.household_id,
        Action::DueSet {
            item_id: task.id,
            version,
            due: QuickDue::Tomorrow,
            back: back.clone(),
        },
    )
    .await;
    let (message_id, _) = bot.active();
    bot.press_data(&token, message_id).await;
    assert_eq!(
        item_due(&db.pool, task.id).await,
        (None, Some(today)),
        "a quick due must not change a completed task"
    );
    bot.assert_screen_contains("Задачи › Оплатить интернет");
    assert!(
        !bot.screen().contains("Срок: завтра"),
        "a quick due must not report a saved due:\n{}",
        bot.screen()
    );

    let token = action_callback(
        &db.pool,
        bot.chat,
        household.household_id,
        Action::DueText {
            item_id: task.id,
            version,
            back,
        },
    )
    .await;
    let (message_id, _) = bot.active();
    bot.press_data(&token, message_id).await;
    bot.assert_screen_contains("Задачи › Оплатить интернет");
    assert!(
        !bot.screen().contains("Введите срок"),
        "a completed task must not open the due form:\n{}",
        bot.screen()
    );
    assert_eq!(
        item_due(&db.pool, task.id).await,
        (None, Some(today)),
        "the due form must not change a completed task"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn purchase_ignores_the_due_screen_and_quick_due() {
    let db = db().await;
    let moscow = chrono_tz::Tz::Europe__Moscow;
    let today = NaiveDate::from_ymd_opt(2027, 3, 15).expect("date");
    let now = local_utc(moscow, today, 12, 0);
    let mut bot = onboarded_at(&db.pool, "Alice", now).await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    let mut input = new_item(household.shopping_list_id, "Молоко");
    input.due_date = Some(today);
    let purchase = create_item(
        &db.pool,
        &household,
        household.shopping_list_id,
        &actor,
        input,
    )
    .await;

    let back = Screen::List {
        list_id: household.shopping_list_id,
        page: 0,
        edit: false,
        from_lists: false,
    };
    // Экран срока мог остаться в сохранённом состоянии с прежней версии.
    let due = Screen::Due {
        item_id: purchase.id,
        back: Box::new(back.clone()),
    };
    let mut conn = db.pool.acquire().await.expect("connection");
    tasklerq::telegram::views::set(
        &mut conn,
        bot.chat,
        "due",
        &serde_json::to_value(&due).expect("screen state"),
        bot.now,
    )
    .await
    .expect("saved due screen");
    drop(conn);

    bot.text("/refresh").await;
    bot.assert_screen_contains("Покупки › Молоко");
    assert!(
        bot.find_button("Завтра").is_none(),
        "a purchase must not show the due screen:\n{}",
        bot.screen()
    );

    let (version, _) = item_state(&db.pool, purchase.id).await;
    let token = action_callback(
        &db.pool,
        bot.chat,
        household.household_id,
        Action::DueSet {
            item_id: purchase.id,
            version,
            due: QuickDue::Tomorrow,
            back,
        },
    )
    .await;
    let (message_id, _) = bot.active();
    bot.press_data(&token, message_id).await;
    assert_eq!(
        item_due(&db.pool, purchase.id).await,
        (None, Some(today)),
        "a quick due must not change a purchase"
    );
    bot.assert_screen_contains("Покупки › Молоко");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn back_from_quick_due_returns_to_the_card_with_its_own_back() {
    let db = db().await;
    let moscow = chrono_tz::Tz::Europe__Moscow;
    let today = NaiveDate::from_ymd_opt(2027, 3, 15).expect("date");
    let now = local_utc(moscow, today, 12, 0);
    let mut bot = onboarded_at(&db.pool, "Alice", now).await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &actor,
        new_item(household.tasks_list_id, "Позвонить маме"),
    )
    .await;

    bot.press("Задачи").await;
    bot.press("Позвонить маме").await;
    bot.press("Срок").await;
    bot.assert_screen_contains("Задачи › Позвонить маме › Срок");
    bot.assert_has_button("Меню");
    bot.press("Назад").await;
    bot.assert_screen_contains("Задачи › Позвонить маме");
    bot.press("Назад").await;
    bot.assert_screen_contains("Задачи · 1");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quick_due_text_rejects_an_invalid_date_and_keeps_the_form() {
    let db = db().await;
    let moscow = chrono_tz::Tz::Europe__Moscow;
    let today = NaiveDate::from_ymd_opt(2027, 3, 15).expect("date");
    let now = local_utc(moscow, today, 12, 0);
    let mut bot = onboarded_at(&db.pool, "Alice", now).await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    let mut input = new_item(household.tasks_list_id, "Сдать отчёт");
    input.due_date = Some(today);
    let task = create_item(&db.pool, &household, household.tasks_list_id, &actor, input).await;

    bot.press("Задачи").await;
    bot.press("Сдать отчёт").await;
    bot.press("Срок").await;
    bot.press("Дата и время").await;
    bot.text("когда-нибудь").await;
    bot.assert_screen_contains("Не понял срок.");
    bot.assert_screen_contains("Введите срок");
    assert_eq!(
        item_due(&db.pool, task.id).await,
        (None, Some(today)),
        "an invalid date must not change the due"
    );

    bot.text("16.03").await;
    bot.assert_screen_contains("Срок: завтра");
    assert_eq!(
        item_due(&db.pool, task.id).await,
        (None, Some(NaiveDate::from_ymd_opt(2027, 3, 16).unwrap())),
        "the same form accepts a valid date afterwards"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn history_groups_completions_by_planned_day() {
    let db = db().await;
    let moscow = chrono_tz::Tz::Europe__Moscow;
    let today = NaiveDate::from_ymd_opt(2025, 11, 6).expect("date");
    let now = local_utc(moscow, today, 12, 0);
    let mut bot = onboarded_at(&db.pool, "Alice", now).await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    let due = NaiveDate::from_ymd_opt(2025, 11, 4).expect("date");
    let mut input = new_item(household.tasks_list_id, "Полить цветы");
    input.due_date = Some(due);
    let item = create_item(&db.pool, &household, household.tasks_list_id, &actor, input).await;
    let mut conn = db.pool.acquire().await.expect("connection");
    items::complete(
        &mut conn,
        household.household_id,
        &actor,
        item.id,
        Some(item.version),
        "Europe/Moscow",
        now,
    )
    .await
    .expect("complete");
    drop(conn);

    bot.press("Задачи").await;
    bot.assert_has_button("📜 История");
    bot.press("История").await;
    bot.assert_screen_contains("Вторник, 4 ноя");
    bot.assert_has_button("Полить цветы · Alice");
    bot.assert_has_button("⬅️ Назад");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn history_marks_a_planned_day_ahead() {
    let db = db().await;
    let moscow = chrono_tz::Tz::Europe__Moscow;
    let today = NaiveDate::from_ymd_opt(2024, 11, 6).expect("date");
    let now = local_utc(moscow, today, 12, 0);
    let mut bot = onboarded_at(&db.pool, "Alice", now).await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    let due = NaiveDate::from_ymd_opt(2024, 11, 8).expect("date");
    let mut input = new_item(household.tasks_list_id, "Сдать отчёт");
    input.due_date = Some(due);
    let item = create_item(&db.pool, &household, household.tasks_list_id, &actor, input).await;
    let mut conn = db.pool.acquire().await.expect("connection");
    items::complete(
        &mut conn,
        household.household_id,
        &actor,
        item.id,
        Some(item.version),
        "Europe/Moscow",
        now,
    )
    .await
    .expect("complete early");
    drop(conn);

    bot.press("Задачи").await;
    bot.press("История").await;
    bot.assert_screen_contains("Пятница, 8 ноя (впереди)");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn history_button_is_only_on_the_task_list_and_empty_journal_is_explained() {
    let db = db().await;
    let mut bot = onboarded_named(&db.pool, "Alice").await;

    bot.press("Задачи").await;
    bot.assert_has_button("📜 История");
    bot.press("История").await;
    bot.assert_screen_contains("Пока нет выполнений.");
    bot.press("Назад").await;
    bot.assert_screen_contains("Задачи");
    bot.press("Назад").await;

    bot.press("Покупки").await;
    assert!(
        bot.find_button("История").is_none(),
        "the shopping list must not offer the completion journal:\n{}",
        bot.screen()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn history_entry_opens_the_card_and_back_returns_to_the_same_page() {
    let db = db().await;
    let moscow = chrono_tz::Tz::Europe__Moscow;
    let today = NaiveDate::from_ymd_opt(2025, 11, 6).expect("date");
    let now = local_utc(moscow, today, 12, 0);
    let mut bot = onboarded_at(&db.pool, "Alice", now).await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    let mut conn = db.pool.acquire().await.expect("connection");
    for index in 0..9 {
        let mut input = new_item(household.tasks_list_id, &format!("Задача {index}"));
        input.due_date = Some(NaiveDate::from_ymd_opt(2025, 11, 4).expect("date"));
        let item = create_item(&db.pool, &household, household.tasks_list_id, &actor, input).await;
        items::complete(
            &mut conn,
            household.household_id,
            &actor,
            item.id,
            Some(item.version),
            "Europe/Moscow",
            now + Duration::minutes(index),
        )
        .await
        .expect("complete");
    }
    drop(conn);

    bot.press("Задачи").await;
    bot.press("История").await;
    bot.assert_screen_contains("Страница 1 из 2");
    bot.press("→").await;
    bot.assert_screen_contains("Страница 2 из 2");
    bot.assert_screen_contains("Вторник, 4 ноя");
    bot.press("Задача 0").await;
    bot.assert_screen_contains("Задачи › История › Задача 0");
    assert!(
        !bot.screen().contains("Список ›"),
        "the card opened from the journal must name the real list:\n{}",
        bot.screen()
    );
    bot.press("Назад").await;
    bot.assert_screen_contains("Страница 2 из 2");
    bot.assert_has_button("Задача 0 · Alice");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn card_shows_the_completion_block_with_plan_fact_actor_and_deviation() {
    let db = db().await;
    let moscow = chrono_tz::Tz::Europe__Moscow;
    let today = NaiveDate::from_ymd_opt(2025, 11, 6).expect("date");
    let now = local_utc(moscow, today, 13, 47);
    let mut bot = onboarded_at(&db.pool, "Мария", now).await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    let mut input = new_item(household.tasks_list_id, "Полить цветы");
    input.due_date = Some(NaiveDate::from_ymd_opt(2025, 11, 4).expect("date"));
    let item = create_item(&db.pool, &household, household.tasks_list_id, &actor, input).await;
    let mut conn = db.pool.acquire().await.expect("connection");
    items::complete(
        &mut conn,
        household.household_id,
        &actor,
        item.id,
        Some(item.version),
        "Europe/Moscow",
        now,
    )
    .await
    .expect("complete");
    drop(conn);

    bot.press("Задачи").await;
    bot.press("История").await;
    bot.press("Полить цветы · Мария").await;
    bot.assert_screen_contains("Выполнения");
    bot.assert_screen_contains("<blockquote expandable>");
    bot.assert_screen_contains("вт, 4 ноя → чт, 6 ноя, 13:47 · Мария · +2\u{a0}дня");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn card_completion_block_shows_at_most_five_latest_completions() {
    let db = db().await;
    let moscow = chrono_tz::Tz::Europe__Moscow;
    let today = NaiveDate::from_ymd_opt(2026, 11, 10).expect("date");
    let now = local_utc(moscow, today, 12, 0);
    let mut bot = onboarded_at(&db.pool, "Alice", now).await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    let start = NaiveDate::from_ymd_opt(2026, 11, 4).expect("date");
    let mut input = new_item(household.tasks_list_id, "Полить цветы");
    input.due_date = Some(start);
    input.recurrence = Some(support::recurrence(RecurrenceFreq::Daily, Vec::new()));
    let item = create_item(&db.pool, &household, household.tasks_list_id, &actor, input).await;
    let mut conn = db.pool.acquire().await.expect("connection");

    let mut version = item.version;
    for day in 4..=9 {
        let completed = items::complete(
            &mut conn,
            household.household_id,
            &actor,
            item.id,
            Some(version),
            "Europe/Moscow",
            local_utc(
                moscow,
                NaiveDate::from_ymd_opt(2026, 11, day).expect("date"),
                12,
                0,
            ),
        )
        .await
        .expect("complete");
        version = completed.version;
    }
    drop(conn);

    // Шесть выполнений с планом на 4–9 ноября; в журнале последнее — 9-го.
    bot.press("Задачи").await;
    bot.press("История").await;
    bot.press("Полить цветы").await;
    bot.assert_screen_contains("Выполнения");
    let screen = bot.screen();
    assert_eq!(
        screen.matches(" · Alice").count(),
        5,
        "the block must keep exactly the five latest completions:\n{screen}"
    );
    assert!(
        screen.contains("пн, 9 ноя → пн, 9 ноя, 12:00 · Alice"),
        "the latest completion is shown:\n{screen}"
    );
    assert!(
        !screen.contains("ср, 4 ноя"),
        "the oldest completion must be dropped:\n{screen}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn history_entry_offers_revert_only_for_the_latest_completion() {
    let db = db().await;
    let moscow = chrono_tz::Tz::Europe__Moscow;
    let today = NaiveDate::from_ymd_opt(2026, 11, 6).expect("date");
    let now = local_utc(moscow, today, 12, 0);
    let mut bot = onboarded_at(&db.pool, "Alice", now).await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    let mut input = new_item(household.tasks_list_id, "Полить цветы");
    input.due_date = Some(NaiveDate::from_ymd_opt(2026, 11, 4).expect("date"));
    input.recurrence = Some(support::recurrence(RecurrenceFreq::Daily, Vec::new()));
    let item = create_item(&db.pool, &household, household.tasks_list_id, &actor, input).await;
    let mut conn = db.pool.acquire().await.expect("connection");
    let first = items::complete(
        &mut conn,
        household.household_id,
        &actor,
        item.id,
        Some(item.version),
        "Europe/Moscow",
        local_utc(moscow, NaiveDate::from_ymd_opt(2026, 11, 4).unwrap(), 12, 0),
    )
    .await
    .expect("first completion");
    items::complete(
        &mut conn,
        household.household_id,
        &actor,
        item.id,
        Some(first.version),
        "Europe/Moscow",
        local_utc(moscow, NaiveDate::from_ymd_opt(2026, 11, 5).unwrap(), 12, 0),
    )
    .await
    .expect("second completion");
    drop(conn);

    bot.press("Задачи").await;
    bot.press("История").await;
    // Новое выполнение — 5 ноября, оно первое в журнале.
    bot.press("Полить цветы").await;
    bot.assert_has_button("↩️ Вернуть");
    bot.press("Назад").await;
    bot.assert_screen_contains("История");

    bot.press_after("Среда, 4 ноя", "Полить цветы").await;
    assert!(
        bot.find_button("↩️ Вернуть").is_none(),
        "an older completion must not be revertible:\n{}",
        bot.screen()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn done_today_row_of_a_recurring_task_opens_the_card_with_revert() {
    let db = db().await;
    let moscow = chrono_tz::Tz::Europe__Moscow;
    let today = NaiveDate::from_ymd_opt(2026, 11, 6).expect("date");
    let now = local_utc(moscow, today, 12, 0);
    let mut bot = onboarded_at(&db.pool, "Alice", now).await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    let mut input = new_item(household.tasks_list_id, "Полить цветы");
    input.due_date = Some(today);
    input.recurrence = Some(support::recurrence(RecurrenceFreq::Daily, Vec::new()));
    let item = create_item(&db.pool, &household, household.tasks_list_id, &actor, input).await;

    bot.press("Задачи").await;
    bot.press("Полить цветы").await;
    bot.press("Выполнено").await;
    bot.assert_screen_contains("Выполнено сегодня");
    bot.press("✅ Полить цветы").await;
    bot.assert_has_button("↩️ Вернуть");
    bot.press("Вернуть").await;

    assert_eq!(
        item_due(&db.pool, item.id).await,
        (None, Some(today)),
        "revert restores the planned due date"
    );
    assert!(
        !bot.screen().contains("Выполнено сегодня"),
        "the reverted completion must leave the done-today section:\n{}",
        bot.screen()
    );
}

/// Открыть выполненную задачу из поиска: это карточка без контекста выполнения.
async fn open_done_task_from_search(bot: &mut Harness, query: &str, title: &str) {
    bot.press("Поиск").await;
    bot.text(query).await;
    bot.press("Выполненные").await;
    bot.press(title).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_card_without_completion_context_does_not_revert_a_newer_completion() {
    let db = db().await;
    let moscow = chrono_tz::Tz::Europe__Moscow;
    let today = NaiveDate::from_ymd_opt(2026, 11, 6).expect("date");
    let now = local_utc(moscow, today, 12, 0);
    let mut bot = onboarded_at(&db.pool, "Alice", now).await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    let mut input = new_item(household.tasks_list_id, "Оплатить интернет");
    input.due_date = Some(today);
    let task = create_item(&db.pool, &household, household.tasks_list_id, &actor, input).await;

    let mut conn = db.pool.acquire().await.expect("connection");
    let first = items::complete(
        &mut conn,
        household.household_id,
        &actor,
        task.id,
        Some(task.version),
        "Europe/Moscow",
        now,
    )
    .await
    .expect("Alice completes");
    drop(conn);

    // Алиса держит карточку без контекста выполнения: кнопка помнит версию 2.
    open_done_task_from_search(&mut bot, "Оплатить", "Оплатить интернет").await;
    bot.assert_has_button("↩️ Вернуть");

    // Пока карточка открыта, Боб возвращает задачу и выполняет её снова.
    let mut conn = db.pool.acquire().await.expect("connection");
    let reopened = items::reopen(
        &mut conn,
        household.household_id,
        &actor,
        task.id,
        None,
        now + Duration::minutes(1),
    )
    .await
    .expect("Bob reopens");
    let completed_again = items::complete(
        &mut conn,
        household.household_id,
        &actor,
        task.id,
        Some(reopened.version),
        "Europe/Moscow",
        now + Duration::minutes(2),
    )
    .await
    .expect("Bob completes");
    drop(conn);
    assert_ne!(
        completed_again.version, first.version,
        "the stale card must carry an older version"
    );

    bot.press("Вернуть").await;

    let answers = bot.fake.answers();
    let (_, text, _) = answers.last().expect("an answer to the callback");
    assert!(
        text.as_deref()
            .is_some_and(|text| text.contains("Данные уже изменились")),
        "a race must be explained to the user: {answers:?}"
    );
    let mut conn = db.pool.acquire().await.expect("connection");
    let unchanged = items::get(&mut conn, household.household_id, task.id)
        .await
        .expect("item");
    drop(conn);
    assert_eq!(unchanged.status, ItemStatus::Done);
    assert_eq!(
        unchanged.version, completed_again.version,
        "the stale button must not revert Bob's completion"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_journal_card_does_not_revert_an_edit_of_a_task_without_recurrence() {
    let db = db().await;
    let moscow = chrono_tz::Tz::Europe__Moscow;
    let today = NaiveDate::from_ymd_opt(2026, 11, 6).expect("date");
    let tomorrow = today + Duration::days(1);
    let now = local_utc(moscow, today, 12, 0);
    let mut bot = onboarded_at(&db.pool, "Alice", now).await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    let mut input = new_item(household.tasks_list_id, "Оплатить интернет");
    input.due_date = Some(today);
    let task = create_item(&db.pool, &household, household.tasks_list_id, &actor, input).await;

    let mut conn = db.pool.acquire().await.expect("connection");
    let completed = items::complete(
        &mut conn,
        household.household_id,
        &actor,
        task.id,
        Some(task.version),
        "Europe/Moscow",
        now,
    )
    .await
    .expect("Alice completes");
    drop(conn);

    // Алиса открывает карточку из журнала: она помнит версию выполнения.
    bot.press("Задачи").await;
    bot.press("История").await;
    bot.press("Оплатить интернет").await;
    bot.assert_has_button("↩️ Вернуть");

    // Пока карточка открыта, Боб меняет срок задачи.
    let mut conn = db.pool.acquire().await.expect("connection");
    let edited = items::update(
        &mut conn,
        household.household_id,
        &actor,
        task.id,
        completed.version,
        ItemPatch {
            due_date: Field::Value(tomorrow),
            ..ItemPatch::default()
        },
        "Europe/Moscow",
        now + Duration::minutes(1),
    )
    .await
    .expect("Bob moves the due date");
    drop(conn);
    assert_ne!(edited.version, completed.version);

    bot.press("Вернуть").await;

    let answers = bot.fake.answers();
    let (_, text, _) = answers.last().expect("an answer to the callback");
    assert!(
        text.as_deref()
            .is_some_and(|text| text.contains("Данные уже изменились")),
        "a stale journal card must explain the conflict: {answers:?}"
    );
    let mut conn = db.pool.acquire().await.expect("connection");
    let unchanged = items::get(&mut conn, household.household_id, task.id)
        .await
        .expect("item");
    drop(conn);
    assert_eq!(unchanged.status, ItemStatus::Done);
    assert_eq!(unchanged.version, edited.version);
    assert_eq!(
        unchanged.due_date,
        Some(tomorrow),
        "the stale card must not restore the due date from the snapshot"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn removing_recurrence_after_completion_hides_revert_and_rejects_it() {
    let db = db().await;
    let moscow = chrono_tz::Tz::Europe__Moscow;
    let today = NaiveDate::from_ymd_opt(2026, 11, 6).expect("date");
    let now = local_utc(moscow, today, 12, 0);
    let mut bot = onboarded_at(&db.pool, "Alice", now).await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    let mut input = new_item(household.tasks_list_id, "Полить цветы");
    input.due_date = Some(today);
    input.recurrence = Some(support::recurrence(RecurrenceFreq::Daily, Vec::new()));
    let task = create_item(&db.pool, &household, household.tasks_list_id, &actor, input).await;

    let mut conn = db.pool.acquire().await.expect("connection");
    let completed = items::complete(
        &mut conn,
        household.household_id,
        &actor,
        task.id,
        Some(task.version),
        "Europe/Moscow",
        now,
    )
    .await
    .expect("complete");
    assert_eq!(completed.status, ItemStatus::Open);
    let completion_id =
        items::completions::latest_for_item(&mut conn, household.household_id, task.id)
            .await
            .expect("latest completion")
            .expect("a completion is recorded")
            .completion
            .event_id;
    let edited = items::update(
        &mut conn,
        household.household_id,
        &actor,
        task.id,
        completed.version,
        ItemPatch {
            recurrence: Field::Null,
            ..ItemPatch::default()
        },
        "Europe/Moscow",
        now + Duration::minutes(1),
    )
    .await
    .expect("remove the recurrence");
    assert_eq!(edited.status, ItemStatus::Open);
    assert!(edited.recurrence.is_none());
    drop(conn);

    bot.press("Задачи").await;
    bot.press("История").await;
    bot.press("Полить цветы").await;
    assert!(
        bot.find_button("↩️ Вернуть").is_none(),
        "a task reopened by removing its recurrence cannot be reverted:\n{}",
        bot.screen()
    );

    let mut conn = db.pool.acquire().await.expect("connection");
    let error = items::revert_completion(
        &mut conn,
        household.household_id,
        &actor,
        task.id,
        completion_id,
        now + Duration::minutes(2),
    )
    .await
    .expect_err("the completion is older than the recurrence edit");
    assert!(
        matches!(error, DomainError::Conflict { .. }),
        "expected a conflict, got {error:?}"
    );
    let unchanged = items::get(&mut conn, household.household_id, task.id)
        .await
        .expect("item");
    drop(conn);
    assert_eq!(unchanged.status, ItemStatus::Open);
    assert_eq!(unchanged.version, edited.version);
    assert_eq!(unchanged.due_date, edited.due_date);
    assert_eq!(unchanged.completed_at, edited.completed_at);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completed_task_without_recurrence_from_search_offers_revert() {
    let db = db().await;
    let moscow = chrono_tz::Tz::Europe__Moscow;
    let today = NaiveDate::from_ymd_opt(2026, 11, 6).expect("date");
    let now = local_utc(moscow, today, 12, 0);
    let mut bot = onboarded_at(&db.pool, "Alice", now).await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    let mut input = new_item(household.tasks_list_id, "Вынести мусор");
    input.due_date = Some(today);
    let task = create_item(&db.pool, &household, household.tasks_list_id, &actor, input).await;

    let mut conn = db.pool.acquire().await.expect("connection");
    items::complete(
        &mut conn,
        household.household_id,
        &actor,
        task.id,
        Some(task.version),
        "Europe/Moscow",
        now,
    )
    .await
    .expect("complete");
    drop(conn);

    open_done_task_from_search(&mut bot, "Вынести", "Вынести мусор").await;
    bot.assert_has_button("↩️ Вернуть");
    bot.press("Вернуть").await;

    assert_eq!(
        item_due(&db.pool, task.id).await,
        (None, Some(today)),
        "revert restores the planned due date"
    );
    assert_eq!(
        item_state(&db.pool, task.id).await,
        (task.version + 2, "open".into()),
        "a completed task without recurrence reopens"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reopen_without_completion_events_falls_back_to_plain_reopen() {
    let db = db().await;
    let moscow = chrono_tz::Tz::Europe__Moscow;
    let today = NaiveDate::from_ymd_opt(2026, 11, 6).expect("date");
    let now = local_utc(moscow, today, 12, 0);
    let mut bot = onboarded_at(&db.pool, "Alice", now).await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    let mut input = new_item(household.tasks_list_id, "Оплатить интернет");
    input.due_date = Some(today);
    let task = create_item(&db.pool, &household, household.tasks_list_id, &actor, input).await;

    let mut conn = db.pool.acquire().await.expect("connection");
    items::complete(
        &mut conn,
        household.household_id,
        &actor,
        task.id,
        Some(task.version),
        "Europe/Moscow",
        now,
    )
    .await
    .expect("complete");
    // События за сроком хранения: выполнения в журнале уже нет.
    sqlx::query("DELETE FROM item_events WHERE item_id = $1")
        .bind(task.id)
        .execute(&mut *conn)
        .await
        .expect("purge the events");
    drop(conn);

    open_done_task_from_search(&mut bot, "Оплатить", "Оплатить интернет").await;
    bot.assert_has_button("↩️ Вернуть");
    bot.press("Вернуть").await;

    assert_eq!(
        item_state(&db.pool, task.id).await,
        (task.version + 2, "open".into()),
        "without completion events the old reopen path still works"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completed_recurring_task_without_context_offers_revert_until_edited() {
    let db = db().await;
    let moscow = chrono_tz::Tz::Europe__Moscow;
    let today = NaiveDate::from_ymd_opt(2026, 11, 6).expect("date");
    let now = local_utc(moscow, today, 12, 0);
    let mut bot = onboarded_at(&db.pool, "Alice", now).await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    let mut input = new_item(household.tasks_list_id, "Полить цветы");
    input.due_date = Some(today);
    let mut rule = support::recurrence(RecurrenceFreq::Daily, Vec::new());
    rule.until = Some(today);
    input.recurrence = Some(rule);
    let task = create_item(&db.pool, &household, household.tasks_list_id, &actor, input).await;

    let mut conn = db.pool.acquire().await.expect("connection");
    let completed = items::complete(
        &mut conn,
        household.household_id,
        &actor,
        task.id,
        Some(task.version),
        "Europe/Moscow",
        now,
    )
    .await
    .expect("complete the last occurrence");
    assert_eq!(completed.status, ItemStatus::Done);
    drop(conn);

    open_done_task_from_search(&mut bot, "Полить", "Полить цветы").await;
    bot.assert_has_button("↩️ Вернуть");
    bot.press("Вернуть").await;
    assert_eq!(
        item_state(&db.pool, task.id).await,
        (completed.version + 1, "open".into()),
        "the last occurrence can be reverted"
    );
    assert_eq!(
        item_due(&db.pool, task.id).await,
        (None, Some(today)),
        "revert restores the planned due date"
    );

    // Снова выполняем и правим задачу после выполнения: возврат больше не
    // проходит, поэтому кнопки нет.
    let mut conn = db.pool.acquire().await.expect("connection");
    let done_again = items::complete(
        &mut conn,
        household.household_id,
        &actor,
        task.id,
        Some(completed.version + 1),
        "Europe/Moscow",
        now,
    )
    .await
    .expect("complete again");
    items::update(
        &mut conn,
        household.household_id,
        &actor,
        task.id,
        done_again.version,
        ItemPatch {
            title: Some("Полить цветы на балконе".into()),
            ..ItemPatch::default()
        },
        "Europe/Moscow",
        now,
    )
    .await
    .expect("edit after completion");
    drop(conn);

    bot.press("Выполненные").await;
    bot.press("Полить цветы").await;
    assert!(
        bot.find_button("↩️ Вернуть").is_none(),
        "a changed recurring task cannot be reverted:\n{}",
        bot.screen()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_list_from_all_lists_keeps_actions_and_navigation_on_separate_rows() {
    let db = db().await;
    let mut bot = onboarded_named(&db.pool, "Alice").await;
    let household = household_for(&db.pool, bot.chat).await;
    let mut conn = db.pool.acquire().await.expect("connection");
    lists::create(
        &mut conn,
        household.household_id,
        ListKind::Checklist,
        "Дача",
        Utc::now(),
    )
    .await
    .expect("extra list");
    drop(conn);

    // Третий список включает «Все списки», у такого списка есть «🏠 Меню».
    bot.text("/start").await;
    bot.press("Все списки").await;
    bot.press("Задачи").await;
    bot.assert_screen_contains("Все списки › Задачи · 0");

    let rows = button_rows(&bot.screen());
    let actions = rows
        .iter()
        .find(|row| row.iter().any(|(label, _)| label.contains("Добавить")))
        .expect("action row");
    assert_eq!(actions.len(), 2, "actions stay in one row: {rows:?}");
    assert!(
        actions.iter().any(|(label, _)| label.contains("История")),
        "the journal button stays next to «Добавить»: {rows:?}"
    );

    let navigation = rows
        .iter()
        .find(|row| row.iter().any(|(label, _)| label.contains("Назад")))
        .expect("navigation row");
    assert_eq!(navigation.len(), 2, "navigation has its own row: {rows:?}");
    assert!(
        navigation.iter().any(|(label, _)| label.contains("Меню")),
        "a list opened from «Все списки» still offers «🏠 Меню»: {rows:?}"
    );
    assert!(
        !navigation
            .iter()
            .any(|(label, _)| label.contains("Добавить") || label.contains("История")),
        "navigation must not share the action row: {rows:?}"
    );
}

// 0009-events 8.2: повестка строится из календарной развёртки по дням.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agenda_shows_a_mixed_day_in_spec_order_and_rows_open_cards() {
    let db = db().await;
    let moscow = chrono_tz::Europe::Moscow;
    let today = NaiveDate::from_ymd_opt(2027, 3, 15).expect("date");
    let now = local_utc(moscow, today, 10, 0);
    let mut bot = onboarded_at(&db.pool, "Alice", now).await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);

    let mut manicure = new_item(household.events_list_id, "Маникюр");
    manicure.due_at = Some(local_utc(moscow, today, 14, 0));
    manicure.end_at = Some(local_utc(moscow, today, 15, 30));
    let manicure = create_item(
        &db.pool,
        &household,
        household.events_list_id,
        &actor,
        manicure,
    )
    .await;

    let mut call = new_item(household.tasks_list_id, "Позвонить в банк");
    call.due_at = Some(local_utc(moscow, today, 16, 0));
    let call = create_item(&db.pool, &household, household.tasks_list_id, &actor, call).await;

    let mut internet = new_item(household.tasks_list_id, "Оплатить интернет");
    internet.due_date = Some(today);
    create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &actor,
        internet,
    )
    .await;

    bot.press("Повестка").await;
    bot.assert_screen_contains("Понедельник, 15 мар");
    let labels: Vec<String> = support::telegram::buttons(&bot.screen())
        .into_iter()
        .map(|(label, _)| label)
        .collect();
    assert_eq!(
        &labels[..3],
        [
            "☐ весь день Оплатить интернет",
            "14:00–15:30 Маникюр",
            "☐ 16:00 Позвонить в банк",
        ],
        "{labels:?}"
    );
    assert!(
        !labels
            .iter()
            .any(|label| label.contains("Маникюр") && label.contains('☐')),
        "an event row must not carry the open-task mark: {labels:?}"
    );

    // Сценарий «Тап по строке»: карточка задачи открывается, задача остаётся
    // открытой.
    let before_task = item_state(&db.pool, call.id).await;
    bot.press("Позвонить в банк").await;
    bot.assert_screen_contains("Повестка › Позвонить в банк");
    assert_eq!(
        item_state(&db.pool, call.id).await,
        before_task,
        "opening the card from the agenda must not change the task"
    );
    bot.press("Назад").await;
    bot.assert_screen_contains("14:00–15:30 Маникюр");

    let before = item_state(&db.pool, manicure.id).await;
    bot.press("Маникюр").await;
    bot.assert_screen_contains("Повестка › Маникюр");
    assert_eq!(
        item_state(&db.pool, manicure.id).await,
        before,
        "opening the card from the agenda must not change the event"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agenda_hides_a_past_event_and_a_done_task() {
    let db = db().await;
    let moscow = chrono_tz::Europe::Moscow;
    let today = NaiveDate::from_ymd_opt(2027, 3, 15).expect("date");
    let now = local_utc(moscow, today, 16, 0);
    let mut bot = onboarded_at(&db.pool, "Alice", now).await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);

    let mut manicure = new_item(household.events_list_id, "Маникюр");
    manicure.due_at = Some(local_utc(moscow, today, 14, 0));
    manicure.end_at = Some(local_utc(moscow, today, 15, 30));
    create_item(
        &db.pool,
        &household,
        household.events_list_id,
        &actor,
        manicure,
    )
    .await;

    let done = create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &actor,
        new_item(household.tasks_list_id, "Уже сделано"),
    )
    .await;
    sqlx::query("UPDATE items SET status = 'done', completed_at = $2 WHERE id = $1")
        .bind(done.id)
        .bind(now)
        .execute(&db.pool)
        .await
        .expect("mark done");

    bot.press("Повестка").await;
    assert!(
        !bot.screen().contains("Маникюр"),
        "a past event must not be shown:\n{}",
        bot.screen()
    );
    assert!(
        !bot.screen().contains("Уже сделано"),
        "a done task must not be shown:\n{}",
        bot.screen()
    );
    bot.assert_screen_contains("ничего не запланировано");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agenda_first_page_has_no_earlier_button_and_an_empty_state() {
    let db = db().await;
    let moscow = chrono_tz::Europe::Moscow;
    let today = NaiveDate::from_ymd_opt(2027, 3, 15).expect("date");
    let mut bot = onboarded_at(&db.pool, "Alice", local_utc(moscow, today, 10, 0)).await;

    bot.press("Повестка").await;
    bot.assert_screen_contains("ничего не запланировано");
    assert!(
        bot.find_button("Раньше").is_none(),
        "the first page must not offer «◀ Раньше»:\n{}",
        bot.screen()
    );
    bot.assert_has_button("Дальше");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agenda_pages_move_by_seven_days_and_stop_at_today() {
    let db = db().await;
    let moscow = chrono_tz::Europe::Moscow;
    let today = NaiveDate::from_ymd_opt(2027, 3, 15).expect("date");
    let mut bot = onboarded_at(&db.pool, "Alice", local_utc(moscow, today, 10, 0)).await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);

    let first_day = today;
    let last_first_page = today + Duration::days(6);
    let first_second_page = today + Duration::days(7);
    let last_second_page = today + Duration::days(13);
    for (title, day) in [
        ("Первый день", first_day),
        ("Конец первой недели", last_first_page),
        ("Начало второй недели", first_second_page),
        ("Конец второй недели", last_second_page),
    ] {
        let mut task = new_item(household.tasks_list_id, title);
        task.due_date = Some(day);
        create_item(&db.pool, &household, household.tasks_list_id, &actor, task).await;
    }

    bot.press("Повестка").await;
    bot.assert_screen_contains("Первый день");
    bot.assert_screen_contains("Конец первой недели");
    assert!(
        !bot.screen().contains("Начало второй недели")
            && !bot.screen().contains("Конец второй недели"),
        "the first page is exactly seven days:\n{}",
        bot.screen()
    );

    bot.press("Дальше").await;
    bot.assert_screen_contains("Начало второй недели");
    bot.assert_screen_contains("Конец второй недели");
    assert!(
        !bot.screen().contains("Первый день") && !bot.screen().contains("Конец первой недели"),
        "the next page must not repeat the first window:\n{}",
        bot.screen()
    );
    assert!(
        !bot.screen().contains("(впереди)"),
        "agenda day headings must not use the journal suffix:\n{}",
        bot.screen()
    );
    bot.assert_has_button("Раньше");

    bot.press("Раньше").await;
    bot.assert_screen_contains("Первый день");
    assert!(
        !bot.screen().contains("Начало второй недели"),
        "«◀ Раньше» must return to the first window:\n{}",
        bot.screen()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agenda_shows_each_occurrence_of_a_weekly_event() {
    let db = db().await;
    let moscow = chrono_tz::Europe::Moscow;
    let today = NaiveDate::from_ymd_opt(2027, 3, 15).expect("date");
    let mut bot = onboarded_at(&db.pool, "Alice", local_utc(moscow, today, 10, 0)).await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);

    let tuesday = today + Duration::days(1);
    let mut training = new_item(household.events_list_id, "Тренировка");
    training.due_at = Some(local_utc(moscow, tuesday, 18, 0));
    training.end_at = Some(local_utc(moscow, tuesday, 19, 0));
    training.recurrence = Some(support::recurrence(RecurrenceFreq::Weekly, vec![2, 4]));
    create_item(
        &db.pool,
        &household,
        household.events_list_id,
        &actor,
        training,
    )
    .await;

    bot.press("Повестка").await;
    let labels: Vec<String> = support::telegram::buttons(&bot.screen())
        .into_iter()
        .map(|(label, _)| label)
        .collect();
    let occurrences: Vec<&String> = labels
        .iter()
        .filter(|label| label.contains("Тренировка"))
        .collect();
    assert_eq!(
        occurrences.len(),
        2,
        "Tuesday and Thursday occurrences: {labels:?}"
    );
    for label in occurrences {
        assert_eq!(
            label, "18:00–19:00 Тренировка 🔁",
            "every occurrence carries the recurrence mark: {labels:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agenda_shows_a_multi_day_event_in_every_day_of_the_window() {
    let db = db().await;
    let moscow = chrono_tz::Europe::Moscow;
    let today = NaiveDate::from_ymd_opt(2027, 3, 3).expect("date");
    let mut bot = onboarded_at(&db.pool, "Alice", local_utc(moscow, today, 10, 0)).await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);

    let mut vacation = new_item(household.events_list_id, "Отпуск");
    vacation.due_date = NaiveDate::from_ymd_opt(2027, 3, 5);
    vacation.end_date = NaiveDate::from_ymd_opt(2027, 3, 10);
    create_item(
        &db.pool,
        &household,
        household.events_list_id,
        &actor,
        vacation,
    )
    .await;

    bot.press("Повестка").await;
    let rows: Vec<String> = support::telegram::buttons(&bot.screen())
        .into_iter()
        .map(|(label, _)| label)
        .filter(|label| label.contains("Отпуск"))
        .collect();
    assert_eq!(
        rows,
        vec!["весь день Отпуск".to_owned(); 5],
        "March 5..9 are inside the March 3..9 window:\n{}",
        bot.screen()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agenda_shows_thirty_rows_and_the_number_of_the_rest() {
    let db = db().await;
    let moscow = chrono_tz::Europe::Moscow;
    let today = NaiveDate::from_ymd_opt(2027, 3, 15).expect("date");
    let mut bot = onboarded_at(&db.pool, "Alice", local_utc(moscow, today, 10, 0)).await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);

    for n in 0..35 {
        let mut item = new_item(household.tasks_list_id, &format!("Задача {n:02}"));
        item.due_date = Some(today);
        create_item(&db.pool, &household, household.tasks_list_id, &actor, item).await;
    }

    bot.press("Повестка").await;
    let rows = support::telegram::buttons(&bot.screen())
        .into_iter()
        .filter(|(label, _)| label.starts_with("☐ весь день Задача"))
        .count();
    assert_eq!(rows, 30, "only the first thirty rows are shown");
    bot.assert_screen_contains("…и ещё 5");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn menu_has_the_agenda_button_and_counts_events_today() {
    let db = db().await;
    let moscow = chrono_tz::Europe::Moscow;
    let today = NaiveDate::from_ymd_opt(2027, 3, 15).expect("date");
    let now = local_utc(moscow, today, 10, 0);
    let mut bot = onboarded_at(&db.pool, "Alice", now).await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);

    let mut task = new_item(household.tasks_list_id, "Задача на сегодня");
    task.due_date = Some(today);
    create_item(&db.pool, &household, household.tasks_list_id, &actor, task).await;

    let mut event = new_item(household.events_list_id, "Маникюр");
    event.due_at = Some(local_utc(moscow, today, 14, 0));
    event.end_at = Some(local_utc(moscow, today, 15, 30));
    create_item(
        &db.pool,
        &household,
        household.events_list_id,
        &actor,
        event,
    )
    .await;

    bot.text("/start").await;
    bot.assert_screen_contains("На сегодня: <b>2</b>");
    bot.assert_has_button("🗓 Повестка");

    bot.press("Повестка").await;
    bot.assert_screen_contains("14:00–15:30 Маникюр");

    // Событие прошло: в счётчик «на сегодня» оно больше не входит.
    bot.set_now(local_utc(moscow, today, 16, 0));
    bot.text("/start").await;
    bot.assert_screen_contains("На сегодня: <b>1</b>");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn menu_hides_all_lists_for_the_default_lists_only() {
    let db = db().await;
    let mut bot = onboarded_named(&db.pool, "Alice").await;

    bot.text("/start").await;
    assert!(
        bot.find_button("Все списки").is_none(),
        "only the default lists must not offer «🗂 Все списки»:\n{}",
        bot.screen()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn menu_shows_all_lists_for_two_event_lists() {
    let db = db().await;
    let mut bot = onboarded_named(&db.pool, "Alice").await;
    let household = household_for(&db.pool, bot.chat).await;
    let mut conn = db.pool.acquire().await.expect("connection");
    lists::create(
        &mut conn,
        household.household_id,
        ListKind::Events,
        "Школа",
        Utc::now(),
    )
    .await
    .expect("second event list");
    drop(conn);

    bot.text("/start").await;
    bot.assert_has_button("Все списки");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agenda_shows_each_occurrence_of_a_recurring_task() {
    let db = db().await;
    let moscow = chrono_tz::Europe::Moscow;
    let today = NaiveDate::from_ymd_opt(2027, 3, 15).expect("date");
    let mut bot = onboarded_at(&db.pool, "Alice", local_utc(moscow, today, 10, 0)).await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);

    let mut watering = new_item(household.tasks_list_id, "Полить цветы");
    watering.due_date = Some(today);
    watering.recurrence = Some(support::recurrence(RecurrenceFreq::Daily, Vec::new()));
    create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &actor,
        watering,
    )
    .await;

    bot.press("Повестка").await;
    let occurrences: Vec<String> = support::telegram::buttons(&bot.screen())
        .into_iter()
        .map(|(label, _)| label)
        .filter(|label| label.contains("Полить цветы"))
        .collect();
    assert_eq!(
        occurrences.len(),
        7,
        "a daily task is shown on every day of the window: {occurrences:?}"
    );
    for label in occurrences {
        assert_eq!(label, "☐ весь день Полить цветы 🔁", "{label}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agenda_keeps_a_late_item_of_the_last_day_across_timezones() {
    let db = db().await;
    let moscow = chrono_tz::Europe::Moscow;
    let today = NaiveDate::from_ymd_opt(2027, 3, 15).expect("date");
    let mut bot = onboarded_at(&db.pool, "Alice", local_utc(moscow, today, 10, 0)).await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    // Пользователь остаётся в Москве, household — на Владивостоке: поздний
    // вечер последнего дня окна попадает в следующий день по household.
    sqlx::query("UPDATE households SET timezone = 'Asia/Vladivostok' WHERE id = $1")
        .bind(household.household_id)
        .execute(&db.pool)
        .await
        .expect("household timezone");

    let last_day = today + Duration::days(6);
    let mut task = new_item(household.tasks_list_id, "Поздняя задача");
    task.due_at = Some(local_utc(moscow, last_day, 20, 0));
    create_item(&db.pool, &household, household.tasks_list_id, &actor, task).await;

    bot.press("Повестка").await;
    bot.assert_screen_contains("Поздняя задача");
}

// 0009-events 8.3: первая страница повестки начинается разделом «Просрочено».
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agenda_first_page_starts_with_overdue_tasks_and_hides_a_past_event() {
    let db = db().await;
    let moscow = chrono_tz::Europe::Moscow;
    let today = NaiveDate::from_ymd_opt(2027, 3, 15).expect("date");
    let now = local_utc(moscow, today, 10, 0);
    let mut bot = onboarded_at(&db.pool, "Alice", now).await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);

    // Задача вчера — просрочена; событие вчера — просто прошло.
    let mut overdue = new_item(household.tasks_list_id, "Сдать показания");
    overdue.due_at = Some(local_utc(moscow, today - Duration::days(1), 18, 0));
    create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &actor,
        overdue,
    )
    .await;

    // Срок сегодняшнего утра уже прошёл: задача попадает только в
    // «Просрочено», а не дублируется ещё и в сегодняшнем дне.
    let mut morning = new_item(household.tasks_list_id, "Оплатить свет");
    morning.due_at = Some(local_utc(moscow, today, 8, 0));
    create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &actor,
        morning,
    )
    .await;

    // Вседневная задача на сегодня не просрочена.
    let mut all_day = new_item(household.tasks_list_id, "Полить цветы");
    all_day.due_date = Some(today);
    create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &actor,
        all_day,
    )
    .await;

    let mut past = new_item(household.events_list_id, "Собрание");
    past.due_at = Some(local_utc(moscow, today - Duration::days(1), 14, 0));
    past.end_at = Some(local_utc(moscow, today - Duration::days(1), 15, 30));
    create_item(&db.pool, &household, household.events_list_id, &actor, past).await;

    bot.press("Повестка").await;
    bot.assert_screen_contains("Просрочено");
    let labels: Vec<String> = support::telegram::buttons(&bot.screen())
        .into_iter()
        .map(|(label, _)| label)
        .collect();
    assert_eq!(
        &labels[..2],
        [
            "☐ Сдать показания · вчера, 18:00",
            "☐ Оплатить свет · сегодня, 08:00",
        ],
        "the overdue section goes first: {labels:?}"
    );
    assert_eq!(
        labels
            .iter()
            .filter(|label| label.contains("Оплатить свет"))
            .count(),
        1,
        "an overdue task must not repeat in today's day: {labels:?}"
    );
    assert!(
        labels
            .iter()
            .any(|label| label == "☐ весь день Полить цветы"),
        "an all-day task due today is not overdue: {labels:?}"
    );
    assert!(
        !bot.screen().contains("Собрание"),
        "a past event must not reach the overdue section:\n{}",
        bot.screen()
    );
}

// 0009-events 8.3: список событий открывает повестку только этого списка.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agenda_of_a_single_event_list_shows_only_its_events() {
    let db = db().await;
    let moscow = chrono_tz::Europe::Moscow;
    let today = NaiveDate::from_ymd_opt(2027, 3, 15).expect("date");
    let now = local_utc(moscow, today, 10, 0);
    let mut bot = onboarded_at(&db.pool, "Alice", now).await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    let mut conn = db.pool.acquire().await.expect("connection");
    let school = lists::create(
        &mut conn,
        household.household_id,
        ListKind::Events,
        "Школа",
        Utc::now(),
    )
    .await
    .expect("school list");
    drop(conn);

    let mut class = new_item(school.id, "Родительское собрание");
    class.due_at = Some(local_utc(moscow, today, 14, 0));
    class.end_at = Some(local_utc(moscow, today, 15, 0));
    create_item(&db.pool, &household, school.id, &actor, class).await;

    let mut manicure = new_item(household.events_list_id, "Маникюр");
    manicure.due_at = Some(local_utc(moscow, today, 16, 0));
    manicure.end_at = Some(local_utc(moscow, today, 17, 0));
    create_item(
        &db.pool,
        &household,
        household.events_list_id,
        &actor,
        manicure,
    )
    .await;

    let mut call = new_item(household.tasks_list_id, "Позвонить в банк");
    call.due_at = Some(local_utc(moscow, today, 18, 0));
    create_item(&db.pool, &household, household.tasks_list_id, &actor, call).await;

    let mut overdue = new_item(household.tasks_list_id, "Сдать показания");
    overdue.due_at = Some(local_utc(moscow, today - Duration::days(1), 18, 0));
    create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &actor,
        overdue,
    )
    .await;

    bot.text("/start").await;
    bot.press("Все списки").await;
    let button = support::telegram::buttons(&bot.screen())
        .into_iter()
        .find(|(label, _)| label.contains("Школа"))
        .expect("school list button");
    assert!(
        button.0.starts_with("🗓"),
        "an event list must have its own icon, not the shopping cart: {:?}",
        button.0
    );

    bot.press("Школа").await;
    assert!(
        bot.screen().contains("<h3>Школа</h3>"),
        "the list name must be the agenda heading:\n{}",
        bot.screen()
    );
    bot.assert_screen_contains("Родительское собрание");
    assert!(
        !bot.screen().contains("Маникюр") && !bot.screen().contains("Позвонить в банк"),
        "the single-list agenda must not show other lists' items:\n{}",
        bot.screen()
    );
    assert!(
        !bot.screen().contains("Просрочено"),
        "the single-list agenda must not have the overdue section:\n{}",
        bot.screen()
    );

    bot.press("Назад").await;
    bot.assert_screen_contains("<h3>Все списки</h3>");
}

// 0009-events 8.3: «Просрочено» входит в общий лимит и вытесняет дни окна.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agenda_overdue_section_fills_the_limit_and_displaces_days() {
    let db = db().await;
    let moscow = chrono_tz::Europe::Moscow;
    let today = NaiveDate::from_ymd_opt(2027, 3, 15).expect("date");
    let now = local_utc(moscow, today, 10, 0);
    let mut bot = onboarded_at(&db.pool, "Alice", now).await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);

    for n in 0..31 {
        let mut task = new_item(household.tasks_list_id, &format!("Просрочено {n:02}"));
        task.due_date = Some(today - Duration::days(1));
        create_item(&db.pool, &household, household.tasks_list_id, &actor, task).await;
    }
    for n in 0..3 {
        let mut task = new_item(household.tasks_list_id, &format!("Сегодня {n:02}"));
        task.due_date = Some(today);
        create_item(&db.pool, &household, household.tasks_list_id, &actor, task).await;
    }

    bot.press("Повестка").await;
    let overdue_rows = support::telegram::buttons(&bot.screen())
        .into_iter()
        .filter(|(label, _)| label.starts_with("☐ Просрочено"))
        .count();
    assert_eq!(overdue_rows, 30, "only the first thirty rows are shown");
    assert!(
        !bot.screen().contains("Сегодня 00"),
        "the overdue section must displace the days of the window:\n{}",
        bot.screen()
    );
    // Скрыты одна просроченная задача и три сегодняшние: счётчик считает всё.
    bot.assert_screen_contains("…и ещё 4");

    bot.press("Дальше").await;
    assert!(
        !bot.screen().contains("Просрочено"),
        "the next page must not repeat the overdue section:\n{}",
        bot.screen()
    );
}

// 0009-events 8.3: счётчик списка событий в «Все списки» не считает прошедшие.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn all_lists_event_counter_skips_past_events() {
    let db = db().await;
    let moscow = chrono_tz::Europe::Moscow;
    let today = NaiveDate::from_ymd_opt(2027, 3, 15).expect("date");
    let now = local_utc(moscow, today, 10, 0);
    let mut bot = onboarded_at(&db.pool, "Alice", now).await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    let mut conn = db.pool.acquire().await.expect("connection");
    let school = lists::create(
        &mut conn,
        household.household_id,
        ListKind::Events,
        "Школа",
        Utc::now(),
    )
    .await
    .expect("school list");
    drop(conn);

    let mut past = new_item(school.id, "Собрание");
    past.due_at = Some(local_utc(moscow, today - Duration::days(1), 14, 0));
    past.end_at = Some(local_utc(moscow, today - Duration::days(1), 15, 0));
    create_item(&db.pool, &household, school.id, &actor, past).await;

    let mut future = new_item(school.id, "Урок");
    future.due_at = Some(local_utc(moscow, today, 14, 0));
    future.end_at = Some(local_utc(moscow, today, 15, 0));
    create_item(&db.pool, &household, school.id, &actor, future).await;

    bot.text("/start").await;
    bot.press("Все списки").await;
    assert!(
        bot.screen().contains("🗓 Школа · 1"),
        "the event list counter must skip a passed event:\n{}",
        bot.screen()
    );
}

// 0009-events 9.2: карточка события показывает свои поля и действия.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn event_card_shows_the_event_fields_and_action_buttons() {
    let db = db().await;
    let moscow = chrono_tz::Europe::Moscow;
    let today = NaiveDate::from_ymd_opt(2027, 3, 15).expect("date");
    let now = local_utc(moscow, today, 10, 0);
    let mut bot = onboarded_at(&db.pool, "Alice", now).await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    let mut event = new_item(household.events_list_id, "Маникюр");
    event.due_at = Some(local_utc(moscow, today, 14, 0));
    event.end_at = Some(local_utc(moscow, today, 15, 30));
    event.tags = vec!["дом".into()];
    event.assignee_user_id = Some(household.owner_id);
    event.notes = Some("Записаться заранее".into());
    event.recurrence = Some(support::recurrence(RecurrenceFreq::Daily, Vec::new()));
    create_item(
        &db.pool,
        &household,
        household.events_list_id,
        &actor,
        event,
    )
    .await;

    bot.press("Повестка").await;
    bot.press("Маникюр").await;

    for label in ["📅 Перенести", "✏️ Править", "🗑 Удалить", "Назад", "Меню"]
    {
        bot.assert_has_button(label);
    }
    for label in ["✓ Выполнено", "📅 Срок", "На час"] {
        assert!(
            bot.find_button(label).is_none(),
            "the event card must not offer `{label}`:\n{}",
            bot.screen()
        );
    }
    assert!(
        !bot.screen().contains("Приоритет"),
        "an event has no priority:\n{}",
        bot.screen()
    );
    bot.assert_screen_contains("📅 Начало: Сегодня, 14:00");
    bot.assert_screen_contains("🏁 Конец: Сегодня, 15:30");
    bot.assert_screen_contains("🏷 дом");
    bot.assert_screen_contains("👤 Чьё событие: Alice");
    bot.assert_screen_contains("🔁 Каждый день");
    bot.assert_screen_contains("Список: Календарь");
    bot.assert_screen_contains("Записаться заранее");
}

// 0009-events 9.2: перенос события на завтра сохраняет длительность.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn event_transfer_to_tomorrow_keeps_the_duration() {
    let db = db().await;
    let moscow = chrono_tz::Europe::Moscow;
    let today = NaiveDate::from_ymd_opt(2027, 3, 15).expect("date");
    let now = local_utc(moscow, today, 10, 0);
    let mut bot = onboarded_at(&db.pool, "Alice", now).await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    let mut event = new_item(household.events_list_id, "Маникюр");
    event.due_at = Some(local_utc(moscow, today, 14, 0));
    event.end_at = Some(local_utc(moscow, today, 15, 30));
    let event = create_item(
        &db.pool,
        &household,
        household.events_list_id,
        &actor,
        event,
    )
    .await;

    bot.press("Повестка").await;
    bot.press("Маникюр").await;
    bot.press("Перенести").await;
    bot.assert_has_button("Сегодня");
    bot.assert_has_button("Завтра");
    bot.assert_has_button("+1 неделя");
    bot.assert_has_button("Дата и время");
    bot.press("Завтра").await;

    bot.assert_screen_contains("Начало: завтра, 14:00");
    bot.assert_screen_contains("🏁 Конец: Завтра, 15:30");
    bot.assert_has_button("📅 Перенести");
    let (due_at, end_at): (Option<DateTime<Utc>>, Option<DateTime<Utc>>) =
        sqlx::query_as("SELECT due_at, end_at FROM items WHERE id = $1")
            .bind(event.id)
            .fetch_one(&db.pool)
            .await
            .expect("event");
    assert_eq!(
        (due_at, end_at),
        (
            Some(local_utc(moscow, today + Duration::days(1), 14, 0)),
            Some(local_utc(moscow, today + Duration::days(1), 15, 30)),
        ),
        "the start moves to tomorrow and the duration stays the same"
    );
    assert_eq!(
        saved_screen(&db.pool, bot.chat).await["t"],
        serde_json::json!("item"),
        "after the transfer the active screen is the card"
    );
}

// 0009-events 9.2: у события нет варианта «Без срока».
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn event_quick_due_has_no_without_due_option() {
    let db = db().await;
    let moscow = chrono_tz::Europe::Moscow;
    let today = NaiveDate::from_ymd_opt(2027, 3, 15).expect("date");
    let now = local_utc(moscow, today, 10, 0);
    let mut bot = onboarded_at(&db.pool, "Alice", now).await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    let mut event = new_item(household.events_list_id, "Маникюр");
    event.due_at = Some(local_utc(moscow, today, 14, 0));
    event.end_at = Some(local_utc(moscow, today, 15, 30));
    create_item(
        &db.pool,
        &household,
        household.events_list_id,
        &actor,
        event,
    )
    .await;

    bot.press("Повестка").await;
    bot.press("Маникюр").await;
    bot.press("Перенести").await;

    assert!(
        bot.find_button("Без срока").is_none(),
        "an event must not lose its start:\n{}",
        bot.screen()
    );
}

// 0009-events 9.2: перенос повторяющегося события двигает только ближайшее
// вхождение, серия остаётся на своём времени.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn event_transfer_moves_only_the_nearest_occurrence() {
    let db = db().await;
    let moscow = chrono_tz::Europe::Moscow;
    let today = NaiveDate::from_ymd_opt(2027, 3, 15).expect("date");
    let tuesday = today + Duration::days(1);
    let now = local_utc(moscow, today, 10, 0);
    let mut bot = onboarded_at(&db.pool, "Alice", now).await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    let mut training = new_item(household.events_list_id, "Тренировка");
    training.due_at = Some(local_utc(moscow, tuesday, 18, 0));
    training.end_at = Some(local_utc(moscow, tuesday, 19, 0));
    training.recurrence = Some(support::recurrence(RecurrenceFreq::Weekly, vec![2, 4]));
    let training = create_item(
        &db.pool,
        &household,
        household.events_list_id,
        &actor,
        training,
    )
    .await;

    bot.press("Повестка").await;
    bot.press("Тренировка").await;
    bot.press("Перенести").await;
    bot.press("+1 неделя").await;

    let moved = items::get(
        &mut db.pool.acquire().await.expect("conn"),
        household.household_id,
        training.id,
    )
    .await
    .expect("moved occurrence");
    assert_eq!(
        moved.due_at,
        Some(local_utc(moscow, tuesday + Duration::days(7), 18, 0))
    );
    assert_eq!(
        moved.end_at,
        Some(local_utc(moscow, tuesday + Duration::days(7), 19, 0)),
        "the duration of the moved occurrence stays the same"
    );
    let rule = moved.recurrence.as_ref().expect("rule");
    assert_eq!(
        rule.occurrence_date,
        Some(tuesday),
        "the original series slot stays in the rule"
    );
    assert_eq!(
        rule.occurrence_time,
        Some(NaiveTime::from_hms_opt(18, 0, 0).unwrap())
    );
    bot.assert_screen_contains("Начало: вт, 23 мар, 18:00");
}

// 0009-events 9.2: текстовый ввод даты в быстром сроке события сохраняет вид
// начала и длительность.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn event_quick_due_text_keeps_the_time_and_duration() {
    let db = db().await;
    let moscow = chrono_tz::Europe::Moscow;
    let today = NaiveDate::from_ymd_opt(2027, 3, 15).expect("date");
    let now = local_utc(moscow, today, 10, 0);
    let mut bot = onboarded_at(&db.pool, "Alice", now).await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    let mut event = new_item(household.events_list_id, "Маникюр");
    event.due_at = Some(local_utc(moscow, today, 14, 0));
    event.end_at = Some(local_utc(moscow, today, 15, 30));
    let event = create_item(
        &db.pool,
        &household,
        household.events_list_id,
        &actor,
        event,
    )
    .await;

    bot.press("Повестка").await;
    bot.press("Маникюр").await;
    bot.press("Перенести").await;
    bot.press("Дата и время").await;
    bot.text("завтра").await;

    bot.assert_screen_contains("Начало: завтра, 14:00");
    bot.assert_screen_contains("🏁 Конец: Завтра, 15:30");
    let (due_at, end_at): (Option<DateTime<Utc>>, Option<DateTime<Utc>>) =
        sqlx::query_as("SELECT due_at, end_at FROM items WHERE id = $1")
            .bind(event.id)
            .fetch_one(&db.pool)
            .await
            .expect("event");
    assert_eq!(
        (due_at, end_at),
        (
            Some(local_utc(moscow, today + Duration::days(1), 14, 0)),
            Some(local_utc(moscow, today + Duration::days(1), 15, 30)),
        ),
        "a date without time keeps the start time of a timed event"
    );
}

// 0009-events 9.2: у вседневного события время начала отклоняется с
// пояснением, форма остаётся открытой.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn event_quick_due_text_rejects_a_time_for_an_all_day_event() {
    let db = db().await;
    let moscow = chrono_tz::Europe::Moscow;
    let today = NaiveDate::from_ymd_opt(2027, 3, 15).expect("date");
    let now = local_utc(moscow, today, 10, 0);
    let mut bot = onboarded_at(&db.pool, "Alice", now).await;
    let household = household_for(&db.pool, bot.chat).await;
    let actor = Actor::user(household.owner_id, UserRole::Owner);
    let tomorrow = today + Duration::days(1);
    let mut event = new_item(household.events_list_id, "Отпуск");
    event.due_date = Some(tomorrow);
    event.end_date = Some(tomorrow + Duration::days(2));
    let event = create_item(
        &db.pool,
        &household,
        household.events_list_id,
        &actor,
        event,
    )
    .await;

    bot.press("Повестка").await;
    bot.press("Отпуск").await;
    bot.press("Перенести").await;
    bot.press("Дата и время").await;
    bot.text("завтра 15:00").await;

    bot.assert_screen_contains("У события без времени начало задаётся датой.");
    bot.assert_screen_contains("Введите срок");
    let start: (Option<DateTime<Utc>>, Option<NaiveDate>) =
        sqlx::query_as("SELECT due_at, due_date FROM items WHERE id = $1")
            .bind(event.id)
            .fetch_one(&db.pool)
            .await
            .expect("event start");
    let end: (Option<DateTime<Utc>>, Option<NaiveDate>) =
        sqlx::query_as("SELECT end_at, end_date FROM items WHERE id = $1")
            .bind(event.id)
            .fetch_one(&db.pool)
            .await
            .expect("event end");
    assert_eq!(
        start,
        (None, Some(tomorrow)),
        "the refused input must not move the start"
    );
    assert_eq!(
        end,
        (None, Some(tomorrow + Duration::days(2))),
        "the refused input must not move the end"
    );
}
