//! Сценарии бота от лица пользователя: онбординг, навигация и формы.

mod support;

use std::sync::Arc;

use support::telegram::{Call, FakeTelegram, Harness, buttons};
use support::{db, next_telegram_id};
use tasklerq::telegram::api::TelegramError;
use tasklerq::telegram::bot::model::{Action, Screen, Ui};
use tasklerq::telegram::bot::{Bot, issue_and_render};
use tasklerq::telegram::callbacks::Scope;
use tasklerq::telegram::outbox::{self, Outgoing};
use tasklerq::telegram::rich::button;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn onboarding_creates_household_once() {
    let db = db().await;
    let id = next_telegram_id();
    let mut bot = Harness::new(&db.pool, id);

    bot.text("/start").await;
    bot.assert_screen_contains("Как назовём ваше пространство");

    bot.text("Семья Ивановых").await;
    bot.assert_screen_contains("выберите часовой пояс");

    bot.press("Москва").await;
    bot.assert_screen_contains("Созданы списки «Задачи» и «Покупки»");

    let households: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM users u JOIN households h ON h.id = u.household_id
         WHERE u.telegram_user_id = $1 AND h.name = 'Семья Ивановых'",
    )
    .bind(id)
    .fetch_one(&db.pool)
    .await
    .expect("count");
    assert_eq!(households, 1);

    // Повторный /start не создаёт второй household и открывает меню.
    bot.text("/start").await;
    bot.assert_screen_contains("Семья Ивановых · 0 задач");
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM users WHERE telegram_user_id = $1")
        .bind(id)
        .fetch_one(&db.pool)
        .await
        .expect("count");
    assert_eq!(total, 1);
}

/// Пользователь, прошедший онбординг.
async fn onboarded(pool: &sqlx::PgPool) -> Harness {
    let mut bot = Harness::new(pool, next_telegram_id());
    bot.text("/start").await;
    bot.text("Семья").await;
    bot.press("Москва").await;
    bot.press("меню").await;
    bot
}

async fn household_of(pool: &sqlx::PgPool, telegram_id: i64) -> uuid::Uuid {
    sqlx::query_scalar(
        "SELECT household_id FROM users WHERE telegram_user_id = $1 AND deleted_at IS NULL",
    )
    .bind(telegram_id)
    .fetch_one(pool)
    .await
    .expect("household")
}

/// Доставить отдельное сообщение чата (уведомление) и обновить `buried`.
async fn deliver_message(bot: &mut Harness, html: &str) {
    let mut conn = bot.pool.acquire().await.expect("conn");
    outbox::enqueue(
        &mut conn,
        bot.chat,
        &Outgoing::Message {
            html: html.to_owned(),
            rich: true,
        },
        None,
        bot.now,
    )
    .await
    .expect("enqueue")
    .expect("seq");
    bot.deliver().await;
}

/// Доставить уведомление с одной кнопкой и вернуть `(message_id, data)`.
async fn deliver_notice_button(bot: &mut Harness, label: &str, action: Action) -> (i64, String) {
    let household = household_of(&bot.pool, bot.chat).await;
    let ui = Ui::new()
        .heading("Напоминание")
        .row(vec![button(label, action)]);
    let html = {
        let mut conn = bot.pool.acquire().await.expect("conn");
        issue_and_render(
            &mut conn,
            bot.chat,
            Some(household),
            &ui,
            Scope::Notice,
            bot.now,
        )
        .await
        .expect("render notice")
    };
    deliver_message(bot, &html).await;
    let (message_id, html) = bot.latest();
    let data = buttons(&html)
        .into_iter()
        .find(|(text, _)| text.contains(label))
        .unwrap_or_else(|| panic!("button `{label}` not found in notice:\n{html}"))
        .1;
    (message_id, data)
}

fn is_alive(bot: &Harness, message_id: i64) -> bool {
    bot.fake
        .messages(bot.chat)
        .iter()
        .any(|(id, _)| *id == message_id)
}

fn has_send(calls: &[Call]) -> bool {
    calls
        .iter()
        .any(|call| matches!(call, Call::SendRich { .. } | Call::Send { .. }))
}

/// Строки outbox: `(seq, вид, message_id из payload)`.
type OutboxRow = (i64, String, Option<i64>);

async fn max_outbox_seq(bot: &Harness) -> i64 {
    sqlx::query_scalar("SELECT coalesce(max(seq), 0) FROM outbox WHERE chat_id = $1")
        .bind(bot.chat)
        .fetch_one(&bot.pool)
        .await
        .expect("max outbox seq")
}

async fn outbox_since(bot: &Harness, seq: i64) -> Vec<OutboxRow> {
    sqlx::query_as(
        "SELECT seq, kind::text AS kind,
                coalesce((payload->>'message_id')::bigint, message_id) AS message_id
         FROM outbox
         WHERE chat_id = $1 AND seq > $2
         ORDER BY seq",
    )
    .bind(bot.chat)
    .bind(seq)
    .fetch_all(&bot.pool)
    .await
    .expect("outbox rows")
}

/// Отправить текст, зная `message_id` входящего сообщения, и вернуть строки
/// outbox, появившиеся за это обновление.
async fn send_text(bot: &mut Harness, text: &str) -> (i64, Vec<OutboxRow>) {
    let name = bot.name.clone();
    let update = bot.text_update(&name, text);
    let message_id = update["message"]["message_id"]
        .as_i64()
        .expect("incoming message id");
    let before = max_outbox_seq(bot).await;
    bot.enqueue(update).await;
    bot.pump().await;
    (message_id, outbox_since(bot, before).await)
}

fn delete_seq(rows: &[OutboxRow], message_id: i64) -> i64 {
    rows.iter()
        .find(|(_, kind, id)| kind == "delete" && *id == Some(message_id))
        .unwrap_or_else(|| panic!("no delete row for incoming message {message_id}: {rows:?}"))
        .0
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn navigation_edits_the_single_active_message() {
    let db = db().await;
    let mut bot = onboarded(&db.pool).await;
    bot.fake.clear_calls();

    bot.press("Задачи").await;
    bot.assert_screen_contains("Задачи · 0");
    bot.assert_screen_contains("Пока нет задач");
    bot.press("Назад").await;
    bot.assert_screen_contains("Семья · 0 задач");
    bot.press("Покупки").await;
    bot.assert_screen_contains("Список покупок пуст");

    // Ни одного нового сообщения: только правки активного.
    assert!(bot.fake.sent().is_empty(), "{:?}", bot.fake.sent());
    assert_eq!(bot.fake.messages(bot.chat).len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_form_validates_and_saves() {
    let db = db().await;
    let mut bot = onboarded(&db.pool).await;

    bot.press("Добавить").await;
    bot.press("Задачу").await;
    bot.assert_screen_contains("Как назвать задачу?");

    bot.text("Оплатить интернет").await;
    bot.assert_screen_contains("Когда сделать?");

    // Неверная дата: пример формата, форма остаётся открытой.
    bot.press("Дата и время").await;
    bot.text("когда-нибудь").await;
    bot.assert_screen_contains("Не понял срок");
    bot.assert_screen_contains("Примеры:");
    bot.text("01.01.2020").await;
    bot.assert_screen_contains("Дата уже прошла.");
    bot.text("завтра 18:30").await;
    bot.assert_screen_contains("Какой приоритет?");

    bot.press("Высокий").await;
    bot.assert_screen_contains("Кто исполнитель?");
    bot.press("Без исполнителя").await;
    bot.assert_screen_contains("Теги");
    bot.press("Новый тег").await;
    bot.text("дом").await;
    bot.assert_screen_contains("✓ дом");
    bot.press("Готово").await;
    bot.assert_screen_contains("Подзадачи");
    bot.text("Найти договор").await;
    bot.press("Готово").await;
    bot.assert_screen_contains("Как часто повторять?");
    bot.press("Не повторять").await;
    bot.assert_screen_contains("Проверьте и сохраните");
    bot.assert_screen_contains("Оплатить интернет");
    bot.assert_screen_contains("Завтра, 18:30");
    bot.assert_screen_contains("🏷 дом");

    bot.press("Сохранить").await;
    bot.assert_screen_contains("Задачи · 1");
    bot.assert_screen_contains("Завтра");
    bot.assert_screen_contains("Оплатить интернет · завтра, 18:30");

    let household = household_of(&db.pool, bot.chat).await;
    let (priority, subtasks): (String, i64) = sqlx::query_as(
        "SELECT i.priority::text, (SELECT count(*) FROM subtasks s WHERE s.item_id = i.id)
         FROM items i WHERE i.household_id = $1",
    )
    .bind(household)
    .fetch_one(&db.pool)
    .await
    .expect("item");
    assert_eq!((priority.as_str(), subtasks), ("high", 1));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_title_and_long_title_are_rejected() {
    let db = db().await;
    let mut bot = onboarded(&db.pool).await;
    bot.press("Добавить").await;
    bot.press("Задачу").await;

    bot.text(&"я".repeat(201)).await;
    bot.assert_screen_contains("Название слишком длинное");
    bot.assert_screen_contains("Как назвать задачу?");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn user_failure_still_deletes_the_incoming_text_and_shows_a_banner() {
    let db = db().await;
    let mut bot = Harness::new(&db.pool, next_telegram_id());

    // `/settings` без household заканчивается `Fail::User` через `recover`.
    let (message_id, rows) = send_text(&mut bot, "/settings").await;

    // Удаление входящего стоит до шага и не откатывается вместе с ним.
    let deleted = delete_seq(&rows, message_id);
    let screen = rows
        .iter()
        .find(|(_, kind, _)| kind == "screen")
        .unwrap_or_else(|| panic!("no screen row after user failure: {rows:?}"))
        .0;
    assert!(
        deleted < screen,
        "the incoming message must be deleted before the screen: {rows:?}"
    );
    bot.assert_screen_contains("Сначала создайте пространство");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn free_text_offers_a_draft_and_cancel_discards_it() {
    let db = db().await;
    let mut bot = onboarded(&db.pool).await;

    bot.text("позвонить маме").await;
    bot.assert_screen_contains("Проверьте и сохраните");
    bot.assert_screen_contains("позвонить маме");
    bot.press("Отмена").await;
    bot.assert_screen_contains("Семья · 0 задач");

    let household = household_of(&db.pool, bot.chat).await;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM items WHERE household_id = $1")
        .bind(household)
        .fetch_one(&db.pool)
        .await
        .expect("count");
    assert_eq!(count, 0);

    // Форма удалена: «Сохранить» из старого сообщения уже не сработает.
    bot.text("купить хлеб").await;
    bot.press("Сохранить").await;
    bot.assert_screen_contains("купить хлеб");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completing_shows_undo_and_next_occurrence_for_recurring_tasks() {
    let db = db().await;
    let mut bot = onboarded(&db.pool).await;

    bot.text("Полить цветы").await;
    bot.press("Повторение").await;
    bot.press("Каждый день").await;
    bot.assert_screen_contains("Повторять каждые сколько?");
    bot.press("Каждый раз").await;
    bot.assert_screen_contains("Проверьте и сохраните");
    bot.assert_screen_contains("🔁 Каждый день");
    bot.press("Сохранить").await;

    bot.press("☐").await;
    bot.assert_screen_contains("✅ Выполнено: Полить цветы");
    bot.assert_screen_contains("Следующее:");
    bot.assert_has_button("Отменить");

    bot.press("Отменить").await;
    bot.assert_screen_contains("Задачи · 1");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn card_shows_details_and_edit_applies_immediately() {
    let db = db().await;
    let mut bot = onboarded(&db.pool).await;
    bot.text("Сдать отчёт").await;
    bot.press("Сохранить").await;

    bot.press("Сдать отчёт").await;
    bot.assert_screen_contains("Приоритет: ⚪ Обычный");
    bot.assert_screen_contains("Список: Задачи");

    bot.press("Приоритет").await;
    bot.press("Срочный").await;
    bot.assert_screen_contains("Приоритет: 🔴 Срочный");

    bot.press("Изменить").await;
    bot.text("Сдать отчёт за квартал").await;
    bot.assert_screen_contains("Сдать отчёт за квартал");

    bot.press("Срок").await;
    bot.press("Сегодня").await;
    bot.assert_screen_contains("📅 Сегодня");

    // Назад возвращает список на прежнюю позицию.
    bot.press("Назад").await;
    bot.assert_screen_contains("Задачи · 1");
    bot.assert_screen_contains("🔴 срочно");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_asks_for_confirmation() {
    let db = db().await;
    let mut bot = onboarded(&db.pool).await;
    bot.text("Лишнее").await;
    bot.press("Сохранить").await;
    bot.press("Лишнее").await;

    bot.press("Удалить").await;
    bot.assert_screen_contains("Удалить «Лишнее»?");
    bot.press("Отмена").await;
    bot.assert_screen_contains("Приоритет:");

    bot.press("Удалить").await;
    bot.press("Да, удалить").await;
    bot.assert_screen_contains("Пока нет задач");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn button_on_buried_screen_edits_it_in_place() {
    let db = db().await;
    let mut bot = onboarded(&db.pool).await;
    let (anchor, _) = bot.active();

    deliver_message(&mut bot, "<p>напоминание</p>").await;
    assert_eq!(
        bot.active().0,
        anchor,
        "a notification must not move the view"
    );
    bot.fake.clear_calls();

    bot.press("Задачи").await;

    let calls = bot.fake.calls();
    assert!(
        !has_send(&calls),
        "a button on the anchor must edit it, not send a new screen: {calls:?}"
    );
    assert!(
        calls
            .iter()
            .any(|call| matches!(call, Call::EditRich { id, .. } if *id == anchor)),
        "the anchor must be edited in place: {calls:?}"
    );
    assert_eq!(bot.active().0, anchor);
    assert_eq!(bot.fake.messages(bot.chat).len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn text_after_notification_sends_a_new_screen_below() {
    let db = db().await;
    let mut bot = onboarded(&db.pool).await;
    let (anchor, _) = bot.active();

    deliver_message(&mut bot, "<p>напоминание</p>").await;
    let (notice, _) = bot.latest();
    bot.fake.clear_calls();

    bot.text("купить молоко").await;

    let calls = bot.fake.calls();
    assert!(
        calls
            .iter()
            .any(|call| matches!(call, Call::Delete { id, .. } if *id == anchor)),
        "the old anchor must be deleted: {calls:?}"
    );
    let (new_id, _) = bot.active();
    assert_ne!(new_id, anchor, "a buried screen must be sent anew");
    assert!(
        new_id > notice,
        "the new screen must be below the notification"
    );
    assert!(!is_alive(&bot, anchor), "the old anchor must be gone");
    assert!(is_alive(&bot, notice), "the notification must stay");
    bot.assert_screen_contains("Проверьте и сохраните");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notification_button_opens_a_new_screen_below() {
    let db = db().await;
    let mut bot = onboarded(&db.pool).await;
    bot.press("Настройки").await;
    let (anchor, _) = bot.active();

    let (notice, data) =
        deliver_notice_button(&mut bot, "📋 В меню", Action::Go { to: Screen::Menu }).await;
    bot.fake.clear_calls();

    bot.press_data(&data, notice).await;

    let calls = bot.fake.calls();
    assert!(
        calls
            .iter()
            .any(|call| matches!(call, Call::SendRich { .. })),
        "a notification button must open a new screen: {calls:?}"
    );
    assert!(
        calls
            .iter()
            .any(|call| matches!(call, Call::Delete { id, .. } if *id == anchor)),
        "the old anchor must be deleted: {calls:?}"
    );
    let (new_id, _) = bot.active();
    assert_ne!(new_id, anchor);
    assert!(new_id > notice);
    assert!(!is_alive(&bot, anchor));
    assert!(is_alive(&bot, notice));
    bot.assert_screen_contains("Семья · 0 задач");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_notice_button_on_the_anchor_message_is_not_edited_in_place() {
    let db = db().await;
    let mut bot = onboarded(&db.pool).await;
    let (anchor, _) = bot.active();

    // Якорь засыпан: после него в чате есть уведомления.
    deliver_message(&mut bot, "<p>напоминание</p>").await;
    let (notice, data) =
        deliver_notice_button(&mut bot, "📋 В меню", Action::Go { to: Screen::Menu }).await;
    bot.fake.clear_calls();

    // Кнопка уведомления нажата на сообщении с id активного якоря: область
    // токена всё равно не даёт ей править якорь на месте.
    bot.press_data(&data, anchor).await;

    let calls = bot.fake.calls();
    assert!(
        calls
            .iter()
            .any(|call| matches!(call, Call::SendRich { .. })),
        "a notice button must open a new screen: {calls:?}"
    );
    assert!(
        !calls
            .iter()
            .any(|call| matches!(call, Call::EditRich { id, .. } if *id == anchor)),
        "a notice button must not edit the anchor in place: {calls:?}"
    );
    assert!(
        calls
            .iter()
            .any(|call| matches!(call, Call::Delete { id, .. } if *id == anchor)),
        "the old anchor must be deleted: {calls:?}"
    );
    let (new_id, _) = bot.active();
    assert_ne!(new_id, anchor);
    assert!(is_alive(&bot, notice), "the notification must stay");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeated_start_and_menu_send_a_new_screen() {
    let db = db().await;
    let mut bot = onboarded(&db.pool).await;

    for command in ["/start", "/menu"] {
        let (anchor, _) = bot.active();
        bot.fake.clear_calls();

        bot.text(command).await;

        let calls = bot.fake.calls();
        assert_eq!(
            bot.fake.sent().len(),
            1,
            "{command} must send exactly one new screen: {calls:?}"
        );
        assert!(
            calls
                .iter()
                .any(|call| matches!(call, Call::Delete { id, .. } if *id == anchor)),
            "{command} must delete the old anchor: {calls:?}"
        );
        let (new_id, _) = bot.active();
        assert_ne!(new_id, anchor, "{command} must not edit the old anchor");
        assert!(!is_alive(&bot, anchor));
        bot.assert_screen_contains("Семья · 0 задач");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_sends_a_new_screen_when_edit_would_succeed() {
    let db = db().await;
    let mut bot = onboarded(&db.pool).await;
    let (anchor, _) = bot.active();
    let buried: bool = sqlx::query_scalar("SELECT buried FROM telegram_views WHERE chat_id = $1")
        .bind(bot.chat)
        .fetch_one(&db.pool)
        .await
        .expect("view");
    assert!(
        !buried,
        "the active screen must not be buried before /start"
    );

    // Telegram принимает правку: бот не может узнать, что пользователь удалил
    // сообщение только у себя.
    bot.fake.clear_calls();

    bot.text("/start").await;

    let calls = bot.fake.calls();
    assert_eq!(
        bot.fake.sent().len(),
        1,
        "/start must send a new screen even when an edit would be accepted: {calls:?}"
    );
    assert!(
        calls
            .iter()
            .any(|call| matches!(call, Call::Delete { id, .. } if *id == anchor)),
        "the old anchor must be deleted: {calls:?}"
    );
    assert_ne!(bot.active().0, anchor);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn screen_is_sent_even_when_the_old_anchor_cannot_be_deleted() {
    let db = db().await;
    let mut bot = onboarded(&db.pool).await;
    let (anchor, old_html) = bot.active();

    bot.fake
        .reject_deletes(Some(TelegramError::MessageUndeletable));
    bot.fake.clear_calls();

    bot.text("/start").await;

    let calls = bot.fake.calls();
    assert_eq!(bot.fake.sent().len(), 1, "{calls:?}");
    let (new_id, _) = bot.active();
    assert_ne!(new_id, anchor);
    assert!(is_alive(&bot, anchor), "the old anchor stays in the chat");
    bot.assert_screen_contains("Семья · 0 задач");

    // Кнопка старого сообщения обрабатывается как устаревшая.
    let data = buttons(&old_html)
        .into_iter()
        .find(|(label, _)| label.contains("Задачи"))
        .expect("task button on the old screen")
        .1;
    bot.fake.clear_calls();
    bot.press_data(&data, anchor).await;

    let calls = bot.fake.calls();
    assert!(
        !has_send(&calls),
        "a stale button must not send a screen: {calls:?}"
    );
    assert!(
        bot.fake
            .answers()
            .last()
            .and_then(|(_, text, _)| text.as_deref())
            .is_some_and(|text| text.contains("устарело")),
        "a stale button must answer with a toast: {:?}",
        bot.fake.answers()
    );
    // Действие не выполнилось: остались в меню, а не в списке задач.
    assert_eq!(bot.active().0, new_id);
    bot.assert_screen_contains("Семья · 0 задач");
    assert!(!bot.screen().contains("Пока нет задач"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn text_on_visible_screen_edits_it_in_place() {
    let db = db().await;
    let mut bot = onboarded(&db.pool).await;
    let (anchor, _) = bot.active();
    bot.fake.clear_calls();

    bot.text("купить молоко").await;

    let calls = bot.fake.calls();
    assert!(
        !has_send(&calls),
        "text on the visible screen must edit it: {calls:?}"
    );
    assert_eq!(bot.active().0, anchor);
    assert_eq!(bot.fake.messages(bot.chat).len(), 1);
    bot.assert_screen_contains("Проверьте и сохраните");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn incoming_text_is_deleted_before_the_screen_is_edited() {
    let db = db().await;
    let mut bot = onboarded(&db.pool).await;
    let (anchor, _) = bot.active();
    bot.fake.clear_calls();

    let (message_id, rows) = send_text(&mut bot, "купить молоко").await;

    let delete = delete_seq(&rows, message_id);
    let screen = rows
        .iter()
        .find(|(_, kind, _)| kind == "screen")
        .expect("the update must enqueue a screen")
        .0;
    assert!(
        delete < screen,
        "the incoming message must be deleted before the screen: {rows:?}"
    );

    let calls = bot.fake.calls();
    let deleted_at = calls
        .iter()
        .position(|call| matches!(call, Call::Delete { id, .. } if *id == message_id))
        .unwrap_or_else(|| panic!("the incoming message must be deleted: {calls:?}"));
    let edited_at = calls
        .iter()
        .position(|call| matches!(call, Call::EditRich { id, .. } if *id == anchor))
        .unwrap_or_else(|| panic!("the active screen must be edited: {calls:?}"));
    assert!(
        deleted_at < edited_at,
        "the delete must be delivered before the edit: {calls:?}"
    );
    assert_eq!(bot.active().0, anchor);
    bot.assert_screen_contains("Проверьте и сохраните");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_kind_of_incoming_text_is_deleted() {
    let db = db().await;

    // Команда у ещё не зарегистрированного пользователя.
    let mut newcomer = Harness::new(&db.pool, next_telegram_id());
    let (message_id, rows) = send_text(&mut newcomer, "/start").await;
    delete_seq(&rows, message_id);
    newcomer.assert_screen_contains("Как назовём ваше пространство");

    let mut bot = onboarded(&db.pool).await;

    // `/help` остаётся отдельным сообщением, но команда тоже удаляется.
    let (message_id, rows) = send_text(&mut bot, "/help").await;
    delete_seq(&rows, message_id);
    assert!(
        rows.iter().any(|(_, kind, _)| kind == "message"),
        "/help must stay a separate message: {rows:?}"
    );
    bot.assert_latest_contains("удаляю из чата после обработки");
    bot.assert_latest_contains("/start или /menu возвращают меню вниз чата");

    // Ответ на шаг формы.
    bot.press("Добавить").await;
    bot.press("Задачу").await;
    let (message_id, rows) = send_text(&mut bot, "Оплатить интернет").await;
    delete_seq(&rows, message_id);
    bot.assert_screen_contains("Когда сделать?");

    // Текст на разбор.
    let id = next_telegram_id();
    let handler = Bot::new("tasklerqbot").with_llm_capture(true);
    let mut parser = Harness::with_handler(&db.pool, id, Arc::new(handler), FakeTelegram::new());
    parser.text("/start").await;
    parser.text("Семья").await;
    parser.press("Москва").await;
    parser.press("меню").await;
    let (message_id, rows) = send_text(&mut parser, "купить молоко").await;
    delete_seq(&rows, message_id);
    parser.assert_screen_contains("Разбираю сообщение");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_delete_keeps_the_result_and_sends_the_next_screen_below() {
    let db = db().await;
    let mut bot = onboarded(&db.pool).await;
    let (anchor, _) = bot.active();
    bot.fake
        .reject_deletes(Some(TelegramError::MessageUndeletable));

    send_text(&mut bot, "купить молоко").await;

    let status: String = sqlx::query_scalar(
        "SELECT status::text FROM outbox
         WHERE chat_id = $1 AND kind = 'delete'
         ORDER BY seq DESC LIMIT 1",
    )
    .bind(bot.chat)
    .fetch_one(&db.pool)
    .await
    .expect("delete row");
    assert_eq!(status, "skipped", "an undeletable message is not an error");

    bot.assert_screen_contains("Проверьте и сохраните");
    let (new_id, _) = bot.active();
    assert_ne!(new_id, anchor, "a buried screen must be sent anew");
    assert!(new_id > anchor, "the new screen must be below the old one");
    assert!(is_alive(&bot, anchor), "the old anchor stays in the chat");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_command_is_a_banner_without_a_message() {
    let db = db().await;
    let mut bot = onboarded(&db.pool).await;
    let (anchor, _) = bot.active();
    bot.fake.clear_calls();

    let (message_id, rows) = send_text(&mut bot, "/nonsense").await;

    delete_seq(&rows, message_id);
    assert!(
        !rows.iter().any(|(_, kind, _)| kind == "message"),
        "an unknown command must not send a message: {rows:?}"
    );
    bot.assert_screen_contains("Не знаю такой команды");
    assert_eq!(
        bot.active().0,
        anchor,
        "the current screen must be redrawn in place"
    );
    assert!(!has_send(&bot.fake.calls()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn double_tap_on_a_buried_anchor_keeps_editing_it_in_place() {
    let db = db().await;
    let mut bot = onboarded(&db.pool).await;
    let (anchor, html) = bot.active();
    let tasks = buttons(&html)
        .into_iter()
        .find(|(label, _)| label.contains("Задачи"))
        .expect("tasks button")
        .1;

    deliver_message(&mut bot, "<p>напоминание</p>").await;
    let buried: bool = sqlx::query_scalar("SELECT buried FROM telegram_views WHERE chat_id = $1")
        .bind(bot.chat)
        .fetch_one(&db.pool)
        .await
        .expect("view");
    assert!(buried, "a notification must bury the anchor");
    bot.fake.clear_calls();

    bot.press_data(&tasks, anchor).await;
    bot.press_data(&tasks, anchor).await;

    let calls = bot.fake.calls();
    let edits = calls
        .iter()
        .filter(|call| matches!(call, Call::EditRich { id, .. } if *id == anchor))
        .count();
    assert_eq!(
        edits, 2,
        "a repeated tap on the anchor must edit it in place: {calls:?}"
    );
    assert!(
        !has_send(&calls),
        "a repeated tap must not send a new screen: {calls:?}"
    );
    assert!(
        !calls.iter().any(|call| matches!(call, Call::Delete { .. })),
        "a repeated tap must not delete the anchor: {calls:?}"
    );
    assert_eq!(bot.active().0, anchor);
}
