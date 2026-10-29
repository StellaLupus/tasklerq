//! Сценарии бота от лица пользователя: онбординг, навигация и формы.

mod support;

use support::telegram::Harness;
use support::{db, next_telegram_id};

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
