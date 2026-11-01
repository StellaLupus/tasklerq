//! Экраны покупок, поиска, настроек, семьи и уведомлений.

mod support;

use std::sync::Arc;

use chrono::{DateTime, Datelike, Duration, NaiveDate, TimeZone, Utc};
use sqlx::PgPool;
use support::telegram::{Call, Harness};
use support::{TestHousehold, create_item, db, new_item, next_telegram_id};
use tasklerq::domain::models::{Actor, UserRole};
use tasklerq::domain::settings::NotificationSettings;
use tasklerq::jobs::scheduler::Scheduler;
use tasklerq::telegram::bot::model::{Action, Form};
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
    bot.assert_screen_contains("Удалить 6 позиций?");
    bot.press("Да, очистить").await;
    bot.assert_screen_contains("Убрано: 6 позиций");
    bot.press("К списку").await;
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
    bot.press("К списку").await;
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
    let payloads: Vec<String> =
        sqlx::query_scalar("SELECT payload::text FROM audit_events WHERE household_id IS NOT NULL")
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

    owner.press("Все приглашения").await;
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
        owner.press("Все приглашения").await;
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
            &Outgoing::Message { html, rich: true },
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
    let done = button_of("Готово");

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

    // «Готово» выполняется один раз; повторное нажатие ничего не делает.
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
            &Outgoing::Message { html, rich: true },
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
    use tasklerq::telegram::bot::model::{Action, Screen};
    use tasklerq::telegram::callbacks::{self, NewToken, Scope};

    let db = db().await;
    let mut owner = onboarded_named(&db.pool, "Алексей").await;
    let mut maria = invite(&mut owner, &db.pool, "Мария").await;
    let household_id = household_for(&db.pool, owner.chat).await.household_id;
    maria.press("Настройки").await;
    let (message_id, _) = maria.active();

    // Кнопок на эти экраны участнику не выдают, поэтому токены выпускаем вручную.
    for target in [Screen::Family, Screen::Journal { page: 0 }, Screen::Invites] {
        let payload = serde_json::to_value(Action::Go { to: target.clone() }).expect("payload");
        let mut conn = db.pool.acquire().await.expect("conn");
        let token = callbacks::issue_many(
            &mut conn,
            &[NewToken {
                telegram_user_id: maria.chat,
                household_id: Some(household_id),
                scope: Scope::Screen,
                action: "go".into(),
                payload,
                version: None,
            }],
            Utc::now(),
        )
        .await
        .expect("issue")
        .remove(0);
        drop(conn);

        maria.press_data(&token, message_id).await;

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
