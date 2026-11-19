//! Задачи 4.1–4.3: состояние инсталляции, первый контакт и claim владельца.

mod support;

use std::sync::Arc;

use chrono::Utc;
use sqlx::PgPool;
use support::telegram::{FakeTelegram, Harness};
use support::{create_household, isolated_db, next_telegram_id};
use tasklerq::admin_console::codes::{self, Purpose};
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

/// Бот в режиме `multi` на отдельной базе.
fn multi_harness(pool: &PgPool, telegram_id: i64) -> Harness {
    let bot = Bot::new("tasklerqbot")
        .with_tenancy_config(TenancyConfig {
            mode: TenancyMode::Multi,
            bootstrap_owner: None,
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

/// Telegram ID участника созданного household.
async fn member_telegram_id(pool: &PgPool, member_id: Uuid) -> i64 {
    sqlx::query_scalar("SELECT telegram_user_id FROM users WHERE id = $1")
        .bind(member_id)
        .fetch_one(pool)
        .await
        .expect("member telegram id")
}

async fn create_admin_account(pool: &PgPool) {
    sqlx::query(
        "INSERT INTO admin_accounts (id, username, password_hash)
         VALUES ($1, 'admin', 'hash')",
    )
    .bind(Uuid::now_v7())
    .execute(pool)
    .await
    .expect("admin account");
}

/// Выпустить ссылку-claim и вернуть открытое значение кода.
async fn issue_claim(pool: &PgPool, household_id: Option<Uuid>) -> (String, Uuid) {
    let issued = codes::issue(pool, Purpose::OwnerClaim, household_id, Utc::now())
        .await
        .expect("owner claim code");
    (issued.code, issued.record.id)
}

async fn code_used_at(pool: &PgPool, id: Uuid) -> Option<chrono::DateTime<Utc>> {
    sqlx::query_scalar("SELECT used_at FROM admin_one_time_codes WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("code used_at")
}

async fn active_owners(pool: &PgPool, household_id: Uuid) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM users
         WHERE household_id = $1 AND role = 'owner' AND deleted_at IS NULL",
    )
    .bind(household_id)
    .fetch_one(pool)
    .await
    .expect("owner count")
}

async fn claim_audit_source(pool: &PgPool, household_id: Uuid) -> Option<String> {
    sqlx::query_scalar(
        "SELECT payload->>'source' FROM audit_events
         WHERE household_id = $1 AND event_type = 'member_joined'
         ORDER BY created_at DESC, id DESC LIMIT 1",
    )
    .bind(household_id)
    .fetch_one(pool)
    .await
    .ok()
    .flatten()
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
        Installation::Single {
            household_id: first.household_id,
            has_owner: true,
        }
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
        Installation::Single {
            household_id: first.household_id,
            has_owner: true,
        }
    );

    sqlx::query("UPDATE users SET deleted_at = now() WHERE id = $1")
        .bind(first.owner_id)
        .execute(&db.pool)
        .await
        .expect("soft delete owner");
    assert_eq!(
        installation(&db.pool).await,
        Installation::Single {
            household_id: first.household_id,
            has_owner: false,
        }
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
async fn empty_installation_with_an_admin_account_is_not_configured() {
    let db = isolated_db().await;
    create_admin_account(&db.pool).await;
    let sender = next_telegram_id();
    let mut bot = single_harness(&db.pool, sender, None);

    bot.text("/start").await;

    bot.assert_screen_contains("Обратитесь к администратору");
    assert!(
        !bot.screen().contains("Как назовём ваше пространство"),
        "the first sender must not get onboarding with an account:\n{}",
        bot.screen()
    );
    assert_eq!(form_count(&db.pool, sender).await, 0);
    assert_eq!(household_count(&db.pool).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multi_with_an_admin_account_still_opens_onboarding() {
    let db = isolated_db().await;
    create_admin_account(&db.pool).await;
    let sender = next_telegram_id();
    let mut bot = multi_harness(&db.pool, sender);

    bot.text("/start").await;

    bot.assert_screen_contains("Как назовём ваше пространство");
    assert_eq!(form_count(&db.pool, sender).await, 1);
    assert_eq!(household_count(&db.pool).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn household_without_owner_and_an_admin_account_offers_the_claim_link() {
    let db = isolated_db().await;
    let household = create_household(&db.pool).await;
    soft_delete_owner(&db.pool, household.owner_id).await;
    create_admin_account(&db.pool).await;

    let sender = next_telegram_id();
    let mut bot = single_harness(&db.pool, sender, None);
    bot.text("/start").await;

    let command = format!("tasklerq admin user add --telegram-id {sender} --role owner");
    bot.assert_screen_contains(&command);
    bot.assert_screen_contains("попросите у администратора ссылку");
    assert_eq!(form_count(&db.pool, sender).await, 0);
    assert_eq!(household_count(&db.pool).await, 1);
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
async fn bootstrap_owner_matching_sender_with_household_without_owner_becomes_the_owner() {
    let db = isolated_db().await;
    let household = create_household(&db.pool).await;
    soft_delete_owner(&db.pool, household.owner_id).await;

    let sender = next_telegram_id();
    let mut bot = single_harness(&db.pool, sender, Some(sender));
    bot.text("/start").await;

    bot.assert_screen_contains("Добавить");
    assert!(
        !bot.screen().contains("tasklerq admin user add"),
        "the configured owner must not get the CLI hint:\n{}",
        bot.screen()
    );
    assert_eq!(form_count(&db.pool, sender).await, 0);
    assert_eq!(household_count(&db.pool).await, 1);

    let owner: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM users
         WHERE household_id = $1 AND telegram_user_id = $2
           AND role = 'owner' AND deleted_at IS NULL",
    )
    .bind(household.household_id)
    .bind(sender)
    .fetch_optional(&db.pool)
    .await
    .expect("owner");
    assert!(
        owner.is_some(),
        "the configured sender must become the owner"
    );
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

    reconfigured.assert_screen_contains("Пространство создано");
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn owner_claim_link_makes_the_sender_the_owner() {
    let db = isolated_db().await;
    let household = create_household(&db.pool).await;
    soft_delete_owner(&db.pool, household.owner_id).await;
    let (code, code_id) = issue_claim(&db.pool, Some(household.household_id)).await;

    let sender = next_telegram_id();
    let mut bot = single_harness(&db.pool, sender, None);
    bot.text(&format!("/start own_{code}")).await;

    bot.assert_screen_contains("Добавить");
    assert_eq!(form_count(&db.pool, sender).await, 0);
    assert_eq!(household_count(&db.pool).await, 1);
    assert_eq!(active_owners(&db.pool, household.household_id).await, 1);

    let role: String = sqlx::query_scalar(
        "SELECT role::text FROM users WHERE telegram_user_id = $1 AND deleted_at IS NULL",
    )
    .bind(sender)
    .fetch_one(&db.pool)
    .await
    .expect("sender");
    assert_eq!(role, "owner");
    assert!(code_used_at(&db.pool, code_id).await.is_some());
    assert_eq!(
        claim_audit_source(&db.pool, household.household_id)
            .await
            .as_deref(),
        Some("claim")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_used_claim_link_shows_the_plain_start_screen() {
    let db = isolated_db().await;
    let household = create_household(&db.pool).await;
    soft_delete_owner(&db.pool, household.owner_id).await;
    let (code, _) = issue_claim(&db.pool, Some(household.household_id)).await;

    let first = next_telegram_id();
    let mut bot = single_harness(&db.pool, first, None);
    bot.text(&format!("/start own_{code}")).await;
    assert_eq!(active_owners(&db.pool, household.household_id).await, 1);

    let second = next_telegram_id();
    let mut again = single_harness(&db.pool, second, None);
    again.text(&format!("/start own_{code}")).await;

    again.assert_screen_contains("Это частное пространство");
    again.assert_screen_contains("приглашение у владельца");
    assert_eq!(active_owners(&db.pool, household.household_id).await, 1);
    let joined: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM users WHERE telegram_user_id = $1")
            .bind(second)
            .fetch_optional(&db.pool)
            .await
            .expect("second user");
    assert_eq!(joined, None, "the link must not create a second owner");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_member_with_an_invalid_claim_link_gets_the_plain_menu() {
    let db = isolated_db().await;
    let household = create_household(&db.pool).await;
    let telegram_id = member_telegram_id(&db.pool, household.member_id).await;
    let (code, code_id) = issue_claim(&db.pool, Some(household.household_id)).await;
    sqlx::query(
        "UPDATE admin_one_time_codes
         SET created_at = now() - interval '2 days',
             expires_at = now() - interval '1 day'
         WHERE id = $1",
    )
    .bind(code_id)
    .execute(&db.pool)
    .await
    .expect("expire code");
    let unknown = "A".repeat(22);

    let mut bot = multi_harness(&db.pool, telegram_id);
    for argument in [format!("own_{code}"), format!("own_{unknown}")] {
        bot.text(&format!("/start {argument}")).await;

        bot.assert_screen_contains("Добавить");
        assert!(
            !bot.screen().contains("Вы уже состоите в пространстве"),
            "`{argument}` must behave like a plain start for a member:\n{}",
            bot.screen()
        );
        assert!(
            !bot.screen().contains("Приглашение не найдено"),
            "`{argument}` must behave like a plain start:\n{}",
            bot.screen()
        );
    }
    assert!(code_used_at(&db.pool, code_id).await.is_none());
    assert_eq!(active_owners(&db.pool, household.household_id).await, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_member_claiming_their_own_household_link_becomes_the_owner() {
    let db = isolated_db().await;
    let household = create_household(&db.pool).await;
    let telegram_id = member_telegram_id(&db.pool, household.member_id).await;
    soft_delete_owner(&db.pool, household.owner_id).await;
    let (code, code_id) = issue_claim(&db.pool, Some(household.household_id)).await;

    let mut bot = multi_harness(&db.pool, telegram_id);
    bot.text(&format!("/start own_{code}")).await;

    bot.assert_screen_contains("Вы стали владельцем");
    assert_eq!(active_owners(&db.pool, household.household_id).await, 1);
    assert!(code_used_at(&db.pool, code_id).await.is_some());

    let role: String = sqlx::query_scalar(
        "SELECT role::text FROM users WHERE telegram_user_id = $1 AND deleted_at IS NULL",
    )
    .bind(telegram_id)
    .fetch_one(&db.pool)
    .await
    .expect("member");
    assert_eq!(role, "owner");

    let memberships: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM users
         WHERE household_id = $1 AND telegram_user_id = $2 AND deleted_at IS NULL",
    )
    .bind(household.household_id)
    .bind(telegram_id)
    .fetch_one(&db.pool)
    .await
    .expect("membership count");
    assert_eq!(memberships, 1, "the member must not get a second row");
    assert_eq!(
        claim_audit_source(&db.pool, household.household_id)
            .await
            .as_deref(),
        Some("claim")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_claim_leaves_exactly_one_owner() {
    let db = isolated_db().await;
    let household = create_household(&db.pool).await;
    soft_delete_owner(&db.pool, household.owner_id).await;
    let (code, code_id) = issue_claim(&db.pool, Some(household.household_id)).await;

    let first_id = next_telegram_id();
    let second_id = next_telegram_id();
    let mut first = single_harness(&db.pool, first_id, None);
    let mut second = single_harness(&db.pool, second_id, None);
    let start = format!("/start own_{code}");
    tokio::join!(first.text(&start), second.text(&start));

    assert_eq!(active_owners(&db.pool, household.household_id).await, 1);
    let owners: Vec<i64> = sqlx::query_scalar(
        "SELECT telegram_user_id FROM users
         WHERE household_id = $1 AND role = 'owner' AND deleted_at IS NULL",
    )
    .bind(household.household_id)
    .fetch_all(&db.pool)
    .await
    .expect("owners");
    assert_eq!(owners.len(), 1);
    assert!(owners[0] == first_id || owners[0] == second_id);
    assert!(code_used_at(&db.pool, code_id).await.is_some());

    let (loser_id, loser) = if owners[0] == first_id {
        (second_id, &second)
    } else {
        (first_id, &first)
    };
    let loser_row: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM users WHERE telegram_user_id = $1 AND deleted_at IS NULL",
    )
    .bind(loser_id)
    .fetch_optional(&db.pool)
    .await
    .expect("loser user");
    assert_eq!(loser_row, None, "the losing claim must not create a member");

    let mut plain = single_harness(&db.pool, next_telegram_id(), None);
    plain.text("/start").await;
    assert_eq!(
        loser.screen(),
        plain.screen(),
        "the losing claim must show the same screen as a plain start"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claim_on_an_empty_installation_consumes_the_code_only_at_creation() {
    let db = isolated_db().await;
    let foreign = next_telegram_id();
    let (code, code_id) = issue_claim(&db.pool, None).await;
    let sender = next_telegram_id();
    let mut bot = single_harness(&db.pool, sender, Some(foreign));

    bot.text(&format!("/start own_{code}")).await;
    bot.assert_screen_contains("Как назовём ваше пространство");
    assert_eq!(form_count(&db.pool, sender).await, 1);
    assert_eq!(household_count(&db.pool).await, 0);
    assert!(
        code_used_at(&db.pool, code_id).await.is_none(),
        "opening the link must not consume it"
    );

    bot.text("Семья").await;
    bot.assert_screen_contains("выберите часовой пояс");
    assert!(code_used_at(&db.pool, code_id).await.is_none());

    bot.press("Москва").await;
    bot.assert_screen_contains("Пространство создано");
    assert_eq!(household_count(&db.pool).await, 1);
    assert!(code_used_at(&db.pool, code_id).await.is_some());

    let role: String = sqlx::query_scalar(
        "SELECT role::text FROM users WHERE telegram_user_id = $1 AND deleted_at IS NULL",
    )
    .bind(sender)
    .fetch_one(&db.pool)
    .await
    .expect("sender");
    assert_eq!(role, "owner");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_expired_claim_does_not_create_the_household() {
    let db = isolated_db().await;
    let foreign = next_telegram_id();
    let (code, code_id) = issue_claim(&db.pool, None).await;
    let sender = next_telegram_id();
    let mut bot = single_harness(&db.pool, sender, Some(foreign));

    bot.text(&format!("/start own_{code}")).await;
    bot.text("Семья").await;
    bot.assert_screen_contains("выберите часовой пояс");

    sqlx::query(
        "UPDATE admin_one_time_codes
         SET created_at = now() - interval '2 days',
             expires_at = now() - interval '1 day'
         WHERE id = $1",
    )
    .bind(code_id)
    .execute(&db.pool)
    .await
    .expect("expire code");

    bot.press("Москва").await;

    bot.assert_screen_contains("Обратитесь к администратору");
    assert_eq!(household_count(&db.pool).await, 0);
    assert_eq!(form_count(&db.pool, sender).await, 0);
    assert!(code_used_at(&db.pool, code_id).await.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bootstrap_owner_of_a_console_created_household_becomes_the_owner() {
    let db = isolated_db().await;
    let household_id = create_external_household(&db.pool, None).await;

    let sender = next_telegram_id();
    let mut bot = single_harness(&db.pool, sender, Some(sender));
    bot.text("/start").await;

    bot.assert_screen_contains("Добавить");
    assert_eq!(active_owners(&db.pool, household_id).await, 1);
    let owner: Option<i64> = sqlx::query_scalar(
        "SELECT telegram_user_id FROM users
         WHERE household_id = $1 AND role = 'owner' AND deleted_at IS NULL",
    )
    .bind(household_id)
    .fetch_optional(&db.pool)
    .await
    .expect("owner");
    assert_eq!(owner, Some(sender));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_claim_links_in_multi_work_at_the_same_time() {
    let db = isolated_db().await;
    let first_household = create_household(&db.pool).await;
    let second_household = create_household(&db.pool).await;
    soft_delete_owner(&db.pool, first_household.owner_id).await;
    soft_delete_owner(&db.pool, second_household.owner_id).await;
    let (first_code, first_id) = issue_claim(&db.pool, Some(first_household.household_id)).await;
    let (second_code, second_id) = issue_claim(&db.pool, Some(second_household.household_id)).await;

    assert!(code_used_at(&db.pool, first_id).await.is_none());
    assert!(
        code_used_at(&db.pool, second_id).await.is_none(),
        "issuing the second link must not extinguish the first"
    );

    let first_user = next_telegram_id();
    let mut first = multi_harness(&db.pool, first_user);
    first.text(&format!("/start own_{first_code}")).await;

    let second_user = next_telegram_id();
    let mut second = multi_harness(&db.pool, second_user);
    second.text(&format!("/start own_{second_code}")).await;

    assert_eq!(
        active_owners(&db.pool, first_household.household_id).await,
        1
    );
    assert_eq!(
        active_owners(&db.pool, second_household.household_id).await,
        1
    );
    assert!(code_used_at(&db.pool, first_id).await.is_some());
    assert!(code_used_at(&db.pool, second_id).await.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_claim_arguments_behave_like_start_without_one() {
    let db = isolated_db().await;
    let household = create_household(&db.pool).await;
    soft_delete_owner(&db.pool, household.owner_id).await;
    let sender = next_telegram_id();
    let expected = format!("tasklerq admin user add --telegram-id {sender} --role owner");

    let setup = codes::issue(&db.pool, Purpose::Setup, None, Utc::now())
        .await
        .expect("setup code");
    let admin_link = codes::issue(&db.pool, Purpose::TelegramLink, None, Utc::now())
        .await
        .expect("telegram link code");
    let unknown = "A".repeat(22);

    let arguments = [
        format!("own_{}", setup.code),
        format!("own_{unknown}"),
        "own_short".to_owned(),
        format!("adm_{}", admin_link.code),
        "adm_short".to_owned(),
    ];

    let mut bot = single_harness(&db.pool, sender, None);
    bot.text("/start").await;
    bot.assert_screen_contains(&expected);

    for argument in arguments {
        bot.text(&format!("/start {argument}")).await;
        bot.assert_screen_contains(&expected);
        assert!(
            !bot.screen().contains("Приглашение не найдено"),
            "`{argument}` must behave like a plain start:\n{}",
            bot.screen()
        );
        assert_eq!(form_count(&db.pool, sender).await, 0);
        assert_eq!(household_count(&db.pool).await, 1);
    }

    assert!(code_used_at(&db.pool, setup.record.id).await.is_none());
    assert!(code_used_at(&db.pool, admin_link.record.id).await.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_member_opening_a_claim_link_does_not_consume_it() {
    let db = isolated_db().await;
    let member_household = create_household(&db.pool).await;
    let telegram_id: i64 = sqlx::query_scalar("SELECT telegram_user_id FROM users WHERE id = $1")
        .bind(member_household.member_id)
        .fetch_one(&db.pool)
        .await
        .expect("member telegram id");
    let target = create_household(&db.pool).await;
    soft_delete_owner(&db.pool, target.owner_id).await;
    let (code, code_id) = issue_claim(&db.pool, Some(target.household_id)).await;

    let mut bot = multi_harness(&db.pool, telegram_id);
    bot.text(&format!("/start own_{code}")).await;

    bot.assert_screen_contains("Вы уже состоите в пространстве");
    bot.assert_screen_contains("Добавить");
    assert!(code_used_at(&db.pool, code_id).await.is_none());
    assert_eq!(active_owners(&db.pool, target.household_id).await, 0);

    let household_id: Uuid =
        sqlx::query_scalar("SELECT household_id FROM users WHERE telegram_user_id = $1")
            .bind(telegram_id)
            .fetch_one(&db.pool)
            .await
            .expect("member");
    assert_eq!(household_id, member_household.household_id);
}
