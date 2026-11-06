//! Экраны покупок, поиска, настроек, семьи и уведомлений.

mod support;

use std::sync::Arc;

use axum::http::{Method, StatusCode};
use chrono::{DateTime, Datelike, Duration, NaiveDate, TimeZone, Utc};
use sqlx::PgPool;
use support::telegram::{Call, Harness};
use support::{TestHousehold, create_item, db, new_item, next_telegram_id};
use tasklerq::admin;
use tasklerq::config::TenancyMode;
use tasklerq::domain::models::{Actor, UserRole};
use tasklerq::domain::settings::NotificationSettings;
use tasklerq::jobs::scheduler::Scheduler;
use tasklerq::storage::repositories::items;
use tasklerq::telegram::bot::model::{Action, Form, PAGE_SIZE, Screen, Ui};
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
    bot.assert_screen_contains("Сколько?");
    bot.press("2").await;
    bot.assert_screen_contains("В чём измерять?");
    bot.press("л").await;
    bot.assert_screen_contains("Количество: 2 л");
    bot.press("Сохранить").await;
    bot.assert_screen_contains("Покупки · 1");
    bot.assert_screen_contains("☐ Молоко · 2 л");

    // Дубликат: предложить увеличить количество.
    bot.press("Добавить").await;
    bot.text("молоко").await;
    bot.press("1").await;
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
    assert_eq!(
        support::telegram::buttons(&bot.screen())
            .iter()
            .filter(|(l, _)| l == "☐")
            .count(),
        8
    );
    bot.press("→").await;
    bot.assert_screen_contains("Страница 2 из 2");

    // Завершить единственную задачу второй страницы: возврат на последнюю
    // существующую страницу.
    bot.press("☐").await;
    bot.press("Назад").await;
    bot.assert_screen_contains("Задачи · 8");

    bot.press("Назад").await;
    bot.assert_screen_contains("Семья · 8 задач");
    bot.assert_screen_contains("Выполнено сегодня: <b>1</b>");
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

    bot.text("несуществующее").await; // не форма: превращается в черновик
    bot.press("Отмена").await;

    bot.press("Поиск").await;
    bot.text("zzzz").await;
    bot.assert_screen_contains("Ничего не нашлось");

    bot.press("Назад").await;
    bot.press("Мои задачи").await;
    bot.assert_screen_contains("Мои задачи · 1");
    bot.assert_screen_contains("Моя задача");
    assert!(!bot.screen().contains("Купить молоко домой"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn settings_profile_timezone_and_notifications() {
    let db = db().await;
    let mut bot = onboarded_named(&db.pool, "Alice").await;

    bot.press("Настройки").await;
    bot.assert_screen_contains("Часовой пояс: Europe/Moscow");
    bot.assert_screen_contains("Напоминания: вкл, за 15 минут");
    bot.assert_has_button("Семья");
    bot.assert_has_button("Журнал");

    bot.press("Время").await;
    bot.press("Asia/Yekaterinburg").await;
    bot.assert_screen_contains("Часовой пояс: Asia/Yekaterinburg");

    bot.press("Имя").await;
    bot.text("Алиса").await;
    bot.assert_screen_contains("Профиль: Алиса");

    bot.press("Уведомления").await;
    bot.press("Напоминания").await;
    bot.assert_screen_contains("Напоминания: выкл");
    bot.press("30 мин").await;
    bot.press("Назад").await;
    bot.assert_screen_contains("Напоминания: выкл, за 30 минут");

    // Своё значение вне диапазона отклоняется, форма остаётся открытой.
    bot.press("Уведомления").await;
    bot.press("Своё").await;
    bot.text("2").await;
    bot.assert_screen_contains("от 5 минут");
    bot.text("45").await;
    bot.assert_screen_contains("45");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn settings_edit_notification_times_and_reject_invalid_values() {
    let db = db().await;
    let mut bot = onboarded_named(&db.pool, "Alice").await;

    bot.press("Настройки").await;
    bot.assert_screen_contains("Вседневное: 09:00");
    bot.press("Уведомления").await;
    bot.assert_has_button("Время сводки: 08:30");
    bot.assert_has_button("Вседневные: 09:00");
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
    bot.assert_screen_contains("Время сводки: 07:45");
    assert_eq!(
        notification_settings(&db.pool, bot.chat).await.digest_time,
        "07:45"
    );

    // Время вседневного напоминания.
    bot.press("Вседневные").await;
    bot.text("10:15").await;
    bot.assert_screen_contains("Вседневные: 10:15");
    assert_eq!(
        notification_settings(&db.pool, bot.chat).await.all_day_time,
        "10:15"
    );

    // Начало и конец тихих часов не могут совпадать; прежний конец остаётся.
    bot.press("Начало тихих").await;
    bot.text("23:00").await;
    bot.assert_screen_contains("Начало тихих: 23:00");
    bot.press("Конец тихих").await;
    bot.text("23:00").await;
    bot.assert_screen_contains("не должны совпадать");
    let settings = notification_settings(&db.pool, bot.chat).await;
    assert_eq!(settings.quiet_start, "23:00");
    assert_eq!(settings.quiet_end, "08:00");

    bot.press("Отмена").await;
    bot.assert_screen_contains("Конец тихих: 08:00");
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
    bot.press("Сводка").await;
    assert!(
        !notification_settings(&db.pool, bot.chat)
            .await
            .digest_enabled
    );
    bot.press("Вседневные").await;
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
    bot.assert_screen_contains("Тихие часы: 22:00–08:00");
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
    bot.assert_screen_contains("Начало тихих: 22:00");
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

    owner.assert_has_button("Выйти");
    assert!(owner.find_button("Семья").is_none());
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
    bot.press("Задачу").await;
    bot.advance(Duration::minutes(16));
    bot.text("Просроченная форма").await;
    bot.assert_screen_contains("Форма устарела. Начнём заново?");
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
        Form::Create(form) => assert_eq!(form.draft.title, "Купить молоко"),
        other => panic!("expected a create form, got {other:?}"),
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

    // Пошаговая форма: ввод названия очищается до сохранения в черновике.
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
        Form::Create(form) => assert_eq!(form.draft.title, "Форма ввод"),
        other => panic!("expected a create form, got {other:?}"),
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

    // Карточка покупки.
    bot.press("Покупки").await;
    bot.press("⋯").await;
    screens.push(support::telegram::buttons(&bot.screen()));
    bot.press("Назад").await;
    bot.press("Назад").await;

    // Выбор тегов в форме создания.
    bot.press("Добавить").await;
    bot.press("Задачу").await;
    bot.text("Новая задача").await;
    bot.press("Без срока").await;
    bot.press("Обычный").await;
    bot.press("Без исполнителя").await;
    let tags = support::telegram::buttons(&bot.screen());
    assert!(
        tags.iter().any(|(label, _)| label == "Применить"),
        "{tags:?}"
    );
    screens.push(tags);
    bot.press("Отмена").await;

    // Выбор тегов в форме правки.
    bot.press("Задачи").await;
    bot.press("Оплатить интернет").await;
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
    let batch = notify::batch_ui(
        Uuid::now_v7(),
        0,
        &[BatchEntry {
            item_id: task.id,
            kind: NotificationKind::Reminder,
            title: task.title.clone(),
            when: "Сегодня · Задачи".to_owned(),
            version: task.version,
            state: BatchEntryState::Open,
        }],
        1,
        None,
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
    bot.assert_screen_contains("Семья · 0 задач");
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
    for label in ["Уведомления", "Время", "Журнал", "Интеграции"] {
        owner.press(label).await;
        assert_navigation(&owner, true);
        owner.press("Назад").await;
        owner.assert_screen_contains("Настройки");
    }

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
    member.press("Выйти").await;
    assert_navigation(&member, true);
    member.press("Назад").await;
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
