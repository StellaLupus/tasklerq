//! Задачи 4.1–4.2: состояние инсталляции и первый контакт в боте.

mod support;

use std::sync::Arc;

use chrono::Utc;
use sqlx::PgPool;
use support::telegram::{FakeTelegram, Harness};
use support::{create_household, isolated_db, next_telegram_id};
use tasklerq::config::{TenancyConfig, TenancyMode};
use tasklerq::domain::first_contact::Installation;
use tasklerq::domain::identity::NewMember;
use tasklerq::domain::models::{Actor, UserRole};
use tasklerq::security_audit::Audit;
use tasklerq::storage::repositories::households::{self, Creator, NewOwner};
use tasklerq::storage::repositories::invites;
use tasklerq::telegram::bot::Bot;
use uuid::Uuid;

async fn installation(pool: &PgPool) -> Installation {
    let mut conn = pool.acquire().await.expect("connection");
    households::installation(&mut conn)
        .await
        .expect("installation")
}

async fn create_invite(pool: &PgPool, household_id: Uuid, owner_id: Uuid) -> String {
    let mut tx = pool.begin().await.expect("tx");
    let created = invites::create(
        &mut tx,
        household_id,
        &Actor::user(owner_id, UserRole::Owner),
        Utc::now(),
    )
    .await
    .expect("invite");
    tx.commit().await.expect("commit");
    created.code
}

async fn soft_delete_owner(pool: &PgPool, owner_id: Uuid) {
    sqlx::query("UPDATE users SET deleted_at = now() WHERE id = $1")
        .bind(owner_id)
        .execute(pool)
        .await
        .expect("soft delete owner");
}

/// Создать household извне, как это делает `admin household create`.
async fn create_external_household(pool: &PgPool, owner: Option<i64>) -> Uuid {
    let mut conn = pool.acquire().await.expect("connection");
    let actor = tasklerq::admin::admin_actor();
    let name = "External household";
    let timezone = "Europe/Moscow";

    let household = match owner {
        Some(telegram_id) => {
            let owner = NewOwner {
                member: NewMember {
                    telegram_user_id: telegram_id,
                    telegram_chat_id: telegram_id,
                    display_name: "External owner".to_owned(),
                    username: None,
                },
                timezone: Some("Europe/Moscow".to_owned()),
            };
            let (household, _) = households::create_with_owner(
                &mut conn,
                TenancyMode::Single,
                name,
                timezone,
                owner,
                Creator::Admin(&actor),
                Utc::now(),
            )
            .await
            .expect("external household");
            household
        }
        None => households::create_empty(
            &mut conn,
            TenancyMode::Single,
            name,
            timezone,
            &actor,
            Utc::now(),
        )
        .await
        .expect("external household"),
    };
    household.id
}

/// Бот в режиме `single` на отдельной базе.
fn single_harness(pool: &PgPool, telegram_id: i64, bootstrap_owner: Option<i64>) -> Harness {
    let bot = Bot::new("tasklerqbot")
        .with_tenancy_config(TenancyConfig {
            mode: TenancyMode::Single,
            bootstrap_owner,
        })
        .with_audit(Audit::new(pool.clone()));
    Harness::with_handler(pool, telegram_id, Arc::new(bot), FakeTelegram::new())
}

async fn form_count(pool: &PgPool, chat: i64) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM conversation_states WHERE chat_id = $1")
        .bind(chat)
        .fetch_one(pool)
        .await
        .expect("form count")
}

async fn household_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM households WHERE deleted_at IS NULL")
        .fetch_one(pool)
        .await
        .expect("household count")
}

async fn assert_joined(
    pool: &PgPool,
    code: &str,
    household_id: Uuid,
    bootstrap_owner: Option<i64>,
) {
    let joiner = next_telegram_id();
    let mut bot = single_harness(pool, joiner, bootstrap_owner);
    bot.text(&format!("/start {code}")).await;
    bot.assert_screen_contains("задач");

    let joined: Option<Uuid> = sqlx::query_scalar(
        "SELECT household_id FROM users
         WHERE telegram_user_id = $1 AND deleted_at IS NULL",
    )
    .bind(joiner)
    .fetch_optional(pool)
    .await
    .expect("joined household");
    assert_eq!(joined, Some(household_id));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn installation_counts_active_households_and_an_active_owner() {
    let db = isolated_db().await;
    assert_eq!(installation(&db.pool).await, Installation::Empty);

    let first = create_household(&db.pool).await;
    assert_eq!(
        installation(&db.pool).await,
        Installation::Single { has_owner: true }
    );

    let second = create_household(&db.pool).await;
    assert_eq!(installation(&db.pool).await, Installation::Multiple);

    sqlx::query("UPDATE households SET deleted_at = now() WHERE id = $1")
        .bind(second.household_id)
        .execute(&db.pool)
        .await
        .expect("soft delete household");
    assert_eq!(
        installation(&db.pool).await,
        Installation::Single { has_owner: true }
    );

    sqlx::query("UPDATE users SET deleted_at = now() WHERE id = $1")
        .bind(first.owner_id)
        .execute(&db.pool)
        .await
        .expect("soft delete owner");
    assert_eq!(
        installation(&db.pool).await,
        Installation::Single { has_owner: false }
    );

    sqlx::query("UPDATE households SET deleted_at = now() WHERE id = $1")
        .bind(first.household_id)
        .execute(&db.pool)
        .await
        .expect("soft delete household");
    assert_eq!(installation(&db.pool).await, Installation::Empty);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_on_empty_database_opens_onboarding() {
    let db = isolated_db().await;
    let sender = next_telegram_id();
    let mut bot = single_harness(&db.pool, sender, None);

    bot.text("/start").await;

    bot.assert_screen_contains("Как назовём ваше пространство");
    assert_eq!(form_count(&db.pool, sender).await, 1);
    assert_eq!(household_count(&db.pool).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_with_household_owner_asks_for_an_invite() {
    let db = isolated_db().await;
    let household = create_household(&db.pool).await;
    let code = create_invite(&db.pool, household.household_id, household.owner_id).await;
    let sender = next_telegram_id();
    let mut bot = single_harness(&db.pool, sender, None);

    bot.text("/start").await;

    bot.assert_screen_contains("приглашение у владельца");
    assert!(
        !bot.screen().contains("Как назовём ваше пространство"),
        "no onboarding form expected:\n{}",
        bot.screen()
    );
    assert_eq!(form_count(&db.pool, sender).await, 0);
    assert_eq!(household_count(&db.pool).await, 1);

    assert_joined(&db.pool, &code, household.household_id, None).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_with_household_without_owner_shows_the_admin_command() {
    let db = isolated_db().await;
    let household = create_household(&db.pool).await;
    let code = create_invite(&db.pool, household.household_id, household.owner_id).await;
    soft_delete_owner(&db.pool, household.owner_id).await;

    let sender = next_telegram_id();
    let mut bot = single_harness(&db.pool, sender, None);
    bot.text("/start").await;

    let command = format!("tasklerq admin user add --telegram-id {sender} --role owner");
    bot.assert_screen_contains(&command);
    assert_eq!(form_count(&db.pool, sender).await, 0);
    let member: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM users WHERE telegram_user_id = $1 AND deleted_at IS NULL",
    )
    .bind(sender)
    .fetch_optional(&db.pool)
    .await
    .expect("sender user");
    assert_eq!(member, None, "the passenger must not become a member");
    assert_eq!(household_count(&db.pool).await, 1);

    assert_joined(&db.pool, &code, household.household_id, None).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreign_bootstrap_owner_on_empty_database_is_not_configured() {
    let db = isolated_db().await;
    let sender = next_telegram_id();
    let foreign = next_telegram_id();
    let mut bot = single_harness(&db.pool, sender, Some(foreign));

    bot.text("/start").await;

    bot.assert_screen_contains("Обратитесь к администратору");
    let screen = bot.screen();
    assert!(
        !screen.contains(&foreign.to_string()),
        "the expected owner id must not be disclosed:\n{screen}"
    );
    assert!(
        !screen.contains("tasklerq admin user add"),
        "the foreign owner must not get the CLI hint:\n{screen}"
    );
    assert_eq!(form_count(&db.pool, sender).await, 0);
    assert_eq!(household_count(&db.pool).await, 0);

    let state: String =
        sqlx::query_scalar("SELECT state::text FROM telegram_views WHERE chat_id = $1")
            .bind(sender)
            .fetch_one(&db.pool)
            .await
            .expect("view state");
    assert!(
        !state.contains(&foreign.to_string()),
        "bootstrap_owner must not be stored in telegram_views: {state}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreign_bootstrap_owner_with_household_without_owner_is_not_configured() {
    let db = isolated_db().await;
    let household = create_household(&db.pool).await;
    let code = create_invite(&db.pool, household.household_id, household.owner_id).await;
    soft_delete_owner(&db.pool, household.owner_id).await;

    let sender = next_telegram_id();
    let foreign = next_telegram_id();
    let mut bot = single_harness(&db.pool, sender, Some(foreign));
    bot.text("/start").await;

    bot.assert_screen_contains("Обратитесь к администратору");
    let screen = bot.screen();
    assert!(
        !screen.contains(&foreign.to_string()),
        "the expected owner id must not be disclosed:\n{screen}"
    );
    assert!(
        !screen.contains("tasklerq admin user add"),
        "the foreign owner must not get the CLI hint:\n{screen}"
    );
    assert_eq!(form_count(&db.pool, sender).await, 0);

    assert_joined(&db.pool, &code, household.household_id, Some(foreign)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bootstrap_owner_matching_the_sender_opens_onboarding() {
    let db = isolated_db().await;
    let sender = next_telegram_id();
    let mut bot = single_harness(&db.pool, sender, Some(sender));

    bot.text("/start").await;

    bot.assert_screen_contains("Как назовём ваше пространство");
    assert_eq!(form_count(&db.pool, sender).await, 1);
    assert_eq!(household_count(&db.pool).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_households_require_an_admin() {
    let db = isolated_db().await;
    let first = create_household(&db.pool).await;
    let _second = create_household(&db.pool).await;
    let code = create_invite(&db.pool, first.household_id, first.owner_id).await;
    let sender = next_telegram_id();
    let mut bot = single_harness(&db.pool, sender, None);

    bot.text("/start").await;

    bot.assert_screen_contains("требует вмешательства администратора");
    assert_eq!(form_count(&db.pool, sender).await, 0);
    assert_eq!(household_count(&db.pool).await, 2);

    assert_joined(&db.pool, &code, first.household_id, None).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn text_without_a_user_in_the_invite_state_shows_the_same_screen() {
    let db = isolated_db().await;
    let _household = create_household(&db.pool).await;
    let sender = next_telegram_id();
    let mut bot = single_harness(&db.pool, sender, None);

    bot.text("привет").await;

    bot.assert_screen_contains("приглашение у владельца");
    assert!(
        !bot.screen().contains("Как назовём ваше пространство"),
        "no onboarding form expected:\n{}",
        bot.screen()
    );
    assert_eq!(form_count(&db.pool, sender).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn household_created_during_onboarding_asks_for_an_invite() {
    let db = isolated_db().await;
    let sender = next_telegram_id();
    let mut bot = single_harness(&db.pool, sender, None);

    bot.text("/start").await;
    bot.assert_screen_contains("Как назовём ваше пространство");
    bot.text("Семья").await;
    bot.assert_screen_contains("выберите часовой пояс");

    let owner = next_telegram_id();
    let household_id = create_external_household(&db.pool, Some(owner)).await;

    bot.press("Москва").await;

    bot.assert_screen_contains("Это частное пространство");
    bot.assert_screen_contains("Попросите приглашение у владельца");
    assert!(
        !bot.screen().contains("Данные уже изменились"),
        "the generic conflict banner must not hide the first-contact screen:\n{}",
        bot.screen()
    );
    assert_eq!(household_count(&db.pool).await, 1);
    assert_eq!(form_count(&db.pool, sender).await, 0);
    let member: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM users WHERE telegram_user_id = $1 AND deleted_at IS NULL",
    )
    .bind(sender)
    .fetch_optional(&db.pool)
    .await
    .expect("sender user");
    assert_eq!(member, None, "the sender must not become a member");
    let active: Uuid = sqlx::query_scalar("SELECT id FROM households WHERE deleted_at IS NULL")
        .fetch_one(&db.pool)
        .await
        .expect("active household");
    assert_eq!(active, household_id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn household_without_owner_created_during_onboarding_shows_the_admin_command() {
    let db = isolated_db().await;
    let sender = next_telegram_id();
    let mut bot = single_harness(&db.pool, sender, None);

    bot.text("/start").await;
    bot.text("Семья").await;

    create_external_household(&db.pool, None).await;
    bot.press("Москва").await;

    let command = format!("tasklerq admin user add --telegram-id {sender} --role owner");
    bot.assert_screen_contains(&command);
    bot.assert_screen_contains("Бот не назначает владельца сам");
    assert!(
        !bot.screen().contains("Данные уже изменились"),
        "the generic conflict banner must not hide the first-contact screen:\n{}",
        bot.screen()
    );
    assert_eq!(household_count(&db.pool).await, 1);
    assert_eq!(form_count(&db.pool, sender).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_after_an_external_household_closes_onboarding() {
    let db = isolated_db().await;
    let sender = next_telegram_id();
    let mut bot = single_harness(&db.pool, sender, None);

    bot.text("/start").await;
    bot.text("Семья").await;

    create_external_household(&db.pool, Some(next_telegram_id())).await;
    bot.press("Отмена").await;

    bot.assert_screen_contains("Попросите приглашение у владельца");
    assert_eq!(form_count(&db.pool, sender).await, 0);
    assert_eq!(household_count(&db.pool).await, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_after_an_owner_is_appointed_shows_the_menu() {
    let db = isolated_db().await;
    let household_id = create_external_household(&db.pool, None).await;
    let sender = next_telegram_id();
    let mut bot = single_harness(&db.pool, sender, None);

    bot.text("/start").await;
    bot.assert_screen_contains("admin user add");

    tasklerq::admin::add_user(
        &db.pool,
        TenancyMode::Single,
        Some(household_id),
        UserRole::Owner,
        sender,
        Some("Alice"),
        Utc::now(),
    )
    .await
    .expect("owner should be added");

    bot.text("/unknown").await;

    assert!(
        !bot.screen().contains("Пространство ещё не настроено"),
        "a fresh owner must not see the stale first-contact screen:\n{}",
        bot.screen()
    );
    bot.assert_screen_contains("Добавить");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bootstrap_owner_set_after_the_form_opened_blocks_onboarding() {
    let db = isolated_db().await;
    let sender = next_telegram_id();
    let bootstrap = next_telegram_id();
    let mut bot = single_harness(&db.pool, sender, None);

    bot.text("/start").await;
    bot.text("Семья").await;
    bot.assert_screen_contains("выберите часовой пояс");
    let data = bot.find_button("Москва").expect("zone button");
    let (message_id, _) = bot.active();

    // Администратор задал BOOTSTRAP_OWNER_TELEGRAM_ID уже после открытия формы.
    let mut reconfigured = single_harness(&db.pool, sender, Some(bootstrap));
    reconfigured.press_data(&data, message_id).await;

    reconfigured.assert_screen_contains("Обратитесь к администратору");
    let screen = reconfigured.screen();
    assert!(
        !screen.contains(&bootstrap.to_string()),
        "the expected owner id must not be disclosed:\n{screen}"
    );
    assert_eq!(household_count(&db.pool).await, 0);
    assert_eq!(form_count(&db.pool, sender).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bootstrap_owner_matching_sender_completes_onboarding_after_the_form_opened() {
    let db = isolated_db().await;
    let sender = next_telegram_id();
    let mut bot = single_harness(&db.pool, sender, None);

    bot.text("/start").await;
    bot.text("Семья").await;
    let data = bot.find_button("Москва").expect("zone button");
    let (message_id, _) = bot.active();

    // Тот же пользователь становится ожидаемым владельцем: онбординг разрешён.
    let mut reconfigured = single_harness(&db.pool, sender, Some(sender));
    reconfigured.press_data(&data, message_id).await;

    reconfigured.assert_screen_contains("Готово");
    assert_eq!(household_count(&db.pool).await, 1);
    assert_eq!(form_count(&db.pool, sender).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zone_button_after_the_sender_became_an_owner_shows_the_menu() {
    let db = isolated_db().await;
    let sender = next_telegram_id();
    let mut bot = single_harness(&db.pool, sender, None);

    bot.text("/start").await;
    bot.text("Семья").await;
    bot.assert_screen_contains("выберите часовой пояс");

    tasklerq::admin::create_household_with_owner(
        &db.pool,
        TenancyMode::Single,
        "Семья",
        "Europe/Moscow",
        sender,
        Some("Alice"),
        Utc::now(),
    )
    .await
    .expect("household with owner");

    bot.press("Москва").await;

    assert!(
        !bot.screen().contains("Это частное пространство"),
        "the new owner must see the menu, not the first-contact screen:\n{}",
        bot.screen()
    );
    bot.assert_screen_contains("Добавить");
    assert_eq!(form_count(&db.pool, sender).await, 0);
    assert_eq!(household_count(&db.pool).await, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zone_text_after_the_sender_became_an_owner_shows_the_menu() {
    let db = isolated_db().await;
    let sender = next_telegram_id();
    let mut bot = single_harness(&db.pool, sender, None);

    bot.text("/start").await;
    bot.text("Семья").await;
    bot.assert_screen_contains("выберите часовой пояс");

    tasklerq::admin::create_household_with_owner(
        &db.pool,
        TenancyMode::Single,
        "Семья",
        "Europe/Moscow",
        sender,
        Some("Alice"),
        Utc::now(),
    )
    .await
    .expect("household with owner");

    bot.text("Europe/Moscow").await;

    assert!(
        !bot.screen().contains("Это частное пространство"),
        "the new owner must see the menu, not the first-contact screen:\n{}",
        bot.screen()
    );
    bot.assert_screen_contains("Добавить");
    assert_eq!(form_count(&db.pool, sender).await, 0);
    assert_eq!(household_count(&db.pool).await, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_invite_code_shows_its_banner_over_the_first_contact_screen() {
    let db = isolated_db().await;
    let _household = create_household(&db.pool).await;
    let sender = next_telegram_id();
    let mut bot = single_harness(&db.pool, sender, None);

    bot.text("/start no-such-code").await;

    bot.assert_screen_contains("Приглашение не найдено");
    bot.assert_screen_contains("Попросите приглашение у владельца");
    assert_eq!(form_count(&db.pool, sender).await, 0);
}
