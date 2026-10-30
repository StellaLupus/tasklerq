//! Сценарии разбора текста через LLM в Telegram: очередь, карточка черновика,
//! правка, сохранение и отмена.

mod support;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration as StdDuration;

use async_trait::async_trait;
use chrono::{Duration, Utc};
use serde_json::{Value, json};
use sqlx::PgPool;
use tasklerq::home_assistant::{AiTask, GenerateData, HaError};
use tasklerq::telegram::bot::Bot;
use tasklerq::telegram::capture_worker::{CaptureWorker, MAX_ATTEMPTS, Pass};
use tokio::sync::Notify;

use support::telegram::{FakeTelegram, Harness};
use support::{db, next_telegram_id};

/// Подмена Home Assistant: отдаёт заготовленные ответы по очереди.
struct StubAi {
    answers: Mutex<VecDeque<Result<Value, HaError>>>,
    calls: AtomicUsize,
    prompts: Mutex<Vec<String>>,
}

impl StubAi {
    fn new(answers: Vec<Result<Value, HaError>>) -> Arc<Self> {
        Arc::new(Self {
            answers: Mutex::new(answers.into()),
            calls: AtomicUsize::new(0),
            prompts: Mutex::new(Vec::new()),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl AiTask for StubAi {
    async fn check_entity(&self) -> Result<(), HaError> {
        Ok(())
    }

    async fn generate_data(
        &self,
        request: &GenerateData<'_>,
        _timeout: StdDuration,
    ) -> Result<Value, HaError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.prompts
            .lock()
            .expect("lock")
            .push(request.instructions.to_owned());
        self.answers
            .lock()
            .expect("lock")
            .pop_front()
            .unwrap_or(Err(HaError::Unreachable))
    }
}

fn date_in_days(days: i64) -> String {
    (Utc::now() + Duration::days(days))
        .format("%Y-%m-%d")
        .to_string()
}

/// Ответ LLM: задача со сроком и покупка.
fn two_items() -> Value {
    json!({"items": [
        {"target_kind": "tasks", "title": "Позвонить маме",
         "due_date": date_in_days(2), "due_time": "18:00"},
        {"target_kind": "shopping", "title": "Молоко", "quantity": 2, "unit": "л"}
    ]})
}

struct Stand {
    bot: Harness,
    worker: CaptureWorker,
    ai: Arc<StubAi>,
}

async fn stand(pool: &PgPool, answers: Vec<Result<Value, HaError>>) -> Stand {
    let id = next_telegram_id();
    let handler = Bot::new("tasklerqbot").with_llm_capture(true);
    let ai = StubAi::new(answers);
    let mut bot = Harness::with_handler(pool, id, Arc::new(handler.clone()), FakeTelegram::new());
    bot.text("/start").await;
    bot.text("Семья").await;
    bot.press("Москва").await;
    bot.press("меню").await;

    let worker = CaptureWorker::new(pool.clone(), ai.clone(), handler, Arc::new(Notify::new()))
        .only_chat(id);
    Stand { bot, worker, ai }
}

impl Stand {
    /// Задания этого чата: очередь общая, соседние тесты ставят свои.
    async fn jobs(&self) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM capture_jobs WHERE chat_id = $1")
            .bind(self.bot.chat)
            .fetch_one(&self.bot.pool)
            .await
            .expect("count jobs")
    }

    async fn household_items(&self) -> i64 {
        sqlx::query_scalar(
            "SELECT count(*) FROM items i JOIN users u ON u.household_id = i.household_id
             WHERE u.telegram_user_id = $1 AND i.deleted_at IS NULL",
        )
        .bind(self.bot.chat)
        .fetch_one(&self.bot.pool)
        .await
        .expect("count")
    }

    /// Один проход воркера и доставка результата.
    async fn work(&mut self) -> Pass {
        let pass = self
            .worker
            .run_once(self.bot.now)
            .await
            .expect("worker pass");
        self.bot.deliver().await;
        pass
    }

    /// Отправить текст и довести его до карточки.
    async fn capture(&mut self, text: &str) {
        self.bot.text(text).await;
        assert_eq!(self.work().await, Pass::Drafted);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn text_is_queued_and_the_card_replaces_the_waiting_screen() {
    let db = db().await;
    let mut stand = stand(&db.pool, vec![Ok(two_items())]).await;
    stand.bot.fake.clear_calls();

    stand
        .bot
        .text("завтра в 18:00 позвонить маме, купить молоко")
        .await;
    stand.bot.assert_screen_contains("Разбираю сообщение");
    assert_eq!(stand.ai.calls(), 0, "the bot must not wait for the LLM");
    assert_eq!(stand.jobs().await, 1);
    assert_eq!(stand.household_items().await, 0);

    assert_eq!(stand.work().await, Pass::Drafted);

    stand.bot.assert_screen_contains("Разобрал: 2 позиции");
    stand.bot.assert_screen_contains("Позвонить маме");
    stand.bot.assert_screen_contains("Молоко");
    stand.bot.assert_screen_contains("Количество: 2 л");
    stand.bot.assert_has_button("Сохранить всё");
    stand.bot.assert_has_button("Изменить 1");
    stand.bot.assert_has_button("Удалить 2");
    stand.bot.assert_has_button("Отмена");
    // Ожидание заменено карточкой в том же сообщении.
    assert_eq!(stand.bot.fake.messages(stand.bot.chat).len(), 1);
    assert_eq!(
        stand.household_items().await,
        0,
        "nothing before confirming"
    );

    let prompts = stand.ai.prompts.lock().expect("lock");
    assert!(prompts[0].contains("позвонить маме"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn save_all_creates_every_item_and_undo_removes_them() {
    let db = db().await;
    let mut stand = stand(&db.pool, vec![Ok(two_items())]).await;
    stand.capture("позвонить маме и купить молоко").await;

    stand.bot.press("Сохранить всё").await;
    stand.bot.assert_screen_contains("Сохранено: 2 позиции");
    stand.bot.assert_has_button("Отменить");
    assert_eq!(stand.household_items().await, 2);

    stand.bot.press("Отменить").await;
    stand.bot.assert_screen_contains("Отменил: 2 позиции");
    assert_eq!(stand.household_items().await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn double_tap_on_save_creates_items_once() {
    let db = db().await;
    let mut stand = stand(&db.pool, vec![Ok(two_items())]).await;
    stand.capture("позвонить маме и купить молоко").await;

    let (message_id, html) = stand.bot.active();
    let save = support::telegram::buttons(&html)
        .into_iter()
        .find(|(label, _)| label.contains("Сохранить всё"))
        .expect("save button")
        .1;
    stand.bot.press_data(&save, message_id).await;
    stand.bot.press_data(&save, message_id).await;

    assert_eq!(stand.household_items().await, 2);
    stand.bot.assert_screen_contains("Сохранено: 2 позиции");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn removing_an_item_changes_what_is_saved() {
    let db = db().await;
    let mut stand = stand(&db.pool, vec![Ok(two_items())]).await;
    stand.capture("позвонить маме и купить молоко").await;

    stand.bot.press("Удалить 1").await;
    stand.bot.assert_screen_contains("Разобрал: 1 позиция");
    assert!(!stand.bot.screen().contains("Позвонить маме"));

    stand.bot.press("Сохранить всё").await;
    assert_eq!(stand.household_items().await, 1);
    stand.bot.assert_screen_contains("Молоко");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_discards_the_draft() {
    let db = db().await;
    let mut stand = stand(&db.pool, vec![Ok(two_items())]).await;
    stand.capture("позвонить маме и купить молоко").await;

    stand.bot.press("Отмена").await;
    stand.bot.assert_screen_contains("Семья");
    assert_eq!(stand.household_items().await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn editing_an_item_goes_through_the_form_and_back_to_the_card() {
    let db = db().await;
    let mut stand = stand(&db.pool, vec![Ok(two_items())]).await;
    stand.capture("позвонить маме и купить молоко").await;

    stand.bot.press("Изменить 1").await;
    stand.bot.assert_screen_contains("Проверьте и сохраните");
    stand.bot.assert_screen_contains("Позвонить маме");

    stand.bot.press("Название").await;
    stand.bot.text("Позвонить бабушке").await;
    stand.bot.assert_screen_contains("Позвонить бабушке");

    stand.bot.press("Сохранить").await;
    stand.bot.assert_screen_contains("Разобрал: 2 позиции");
    stand.bot.assert_screen_contains("Позвонить бабушке");
    assert!(!stand.bot.screen().contains("Позвонить маме"));
    assert_eq!(stand.household_items().await, 0, "editing does not save");

    stand.bot.press("Сохранить всё").await;
    let titles: Vec<String> = sqlx::query_scalar(
        "SELECT i.title FROM items i JOIN users u ON u.household_id = i.household_id
         WHERE u.telegram_user_id = $1 ORDER BY i.title",
    )
    .bind(stand.bot.chat)
    .fetch_all(&db.pool)
    .await
    .expect("titles");
    assert_eq!(titles, ["Молоко", "Позвонить бабушке"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn leaving_the_edit_form_returns_to_the_unchanged_card() {
    let db = db().await;
    let mut stand = stand(&db.pool, vec![Ok(two_items())]).await;
    stand.capture("позвонить маме и купить молоко").await;

    stand.bot.press("Изменить 2").await;
    stand.bot.press("Отмена").await;

    stand.bot.assert_screen_contains("Разобрал: 2 позиции");
    stand.bot.assert_screen_contains("Молоко");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_item_blocks_saving_until_it_is_removed() {
    let db = db().await;
    let past = json!({"items": [
        {"target_kind": "tasks", "title": "Оплатить счёт", "due_date": date_in_days(-5)},
        {"target_kind": "shopping", "title": "Хлеб"}
    ]});
    let mut stand = stand(&db.pool, vec![Ok(past)]).await;
    stand.capture("оплатить счёт вчера, купить хлеб").await;

    stand.bot.assert_screen_contains("Дата уже прошла");
    assert!(
        stand.bot.find_button("Сохранить всё").is_none(),
        "{}",
        stand.bot.screen()
    );

    stand.bot.press("Удалить 1").await;
    stand.bot.assert_has_button("Сохранить всё");
    stand.bot.press("Сохранить всё").await;
    assert_eq!(stand.household_items().await, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_draft_is_not_saved() {
    let db = db().await;
    let mut stand = stand(&db.pool, vec![Ok(two_items())]).await;
    stand.capture("позвонить маме и купить молоко").await;

    stand.bot.advance(Duration::minutes(16));
    stand.bot.press("Сохранить всё").await;

    stand.bot.assert_screen_contains("Черновик устарел");
    assert_eq!(stand.household_items().await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn too_long_text_is_not_queued() {
    let db = db().await;
    let mut stand = stand(&db.pool, vec![]).await;
    stand.bot.text(&"а".repeat(2_001)).await;

    stand.bot.assert_latest_contains("слишком длинное");
    assert_eq!(stand.jobs().await, 0);
    assert_eq!(stand.ai.calls(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_request_ends_the_job_with_an_explanation() {
    let db = db().await;
    let mut stand = stand(&db.pool, vec![Err(HaError::Rejected(400))]).await;

    stand.bot.text("позвонить маме").await;
    assert_eq!(stand.work().await, Pass::Dead);

    stand.bot.assert_screen_contains("Не получилось разобрать");
    assert_eq!(stand.ai.calls(), 1, "a rejected request is not retried");
    assert_eq!(stand.household_items().await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timeout_is_not_retried() {
    let db = db().await;
    let mut stand = stand(&db.pool, vec![Err(HaError::Timeout)]).await;

    stand.bot.text("позвонить маме").await;
    assert_eq!(stand.work().await, Pass::Dead);

    assert_eq!(stand.ai.calls(), 1);
    stand.bot.assert_screen_contains("Не получилось разобрать");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_error_is_retried_with_backoff_and_then_succeeds() {
    let db = db().await;
    let mut stand = stand(&db.pool, vec![Err(HaError::Server(503)), Ok(two_items())]).await;

    stand.bot.text("позвонить маме и купить молоко").await;
    let first = stand.work().await;
    assert!(matches!(first, Pass::Retry(_)), "{first:?}");
    // Пока идёт повтор, пользователь по-прежнему видит ожидание.
    stand.bot.assert_screen_contains("Разбираю сообщение");
    assert_eq!(stand.work().await, Pass::Idle, "backoff has not elapsed");

    stand.bot.advance(Duration::seconds(30));
    assert_eq!(stand.work().await, Pass::Drafted);
    stand.bot.assert_screen_contains("Разобрал: 2 позиции");
    assert_eq!(stand.ai.calls(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persistent_server_errors_exhaust_attempts() {
    let db = db().await;
    let answers = (0..MAX_ATTEMPTS)
        .map(|_| Err(HaError::Unreachable))
        .collect();
    let mut stand = stand(&db.pool, answers).await;

    stand.bot.text("позвонить маме").await;
    let mut last = Pass::Idle;
    for _ in 0..MAX_ATTEMPTS {
        stand.bot.advance(Duration::minutes(5));
        last = stand.work().await;
    }

    assert_eq!(last, Pass::Dead);
    assert_eq!(stand.ai.calls(), MAX_ATTEMPTS as usize);
    stand.bot.assert_screen_contains("Не получилось разобрать");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn too_many_items_asks_to_split_the_text() {
    let db = db().await;
    let items: Vec<Value> = (0..21)
        .map(|n| json!({"target_kind": "tasks", "title": format!("Дело {n}")}))
        .collect();
    let mut stand = stand(&db.pool, vec![Ok(json!({"items": items}))]).await;

    stand.bot.text("очень длинный список дел").await;
    assert_eq!(stand.work().await, Pass::Dead);

    stand.bot.assert_screen_contains("Разделите его на части");
    assert_eq!(stand.household_items().await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn answer_without_items_says_nothing_was_found() {
    let db = db().await;
    let mut stand = stand(&db.pool, vec![Ok(json!({"items": []}))]).await;

    stand.bot.text("привет").await;
    assert_eq!(stand.work().await, Pass::Dead);

    stand.bot.assert_screen_contains("Не нашёл в сообщении");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn user_text_does_not_outlive_the_job() {
    let db = db().await;
    let mut stand = stand(&db.pool, vec![Ok(two_items())]).await;
    stand.capture("секретная заметка про подарок").await;

    let leaked: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM capture_jobs WHERE payload::text LIKE '%подарок%'",
    )
    .fetch_one(&db.pool)
    .await
    .expect("count");
    assert_eq!(leaked, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn card_shows_the_due_date_the_domain_will_assign_to_a_series() {
    let db = db().await;
    let series = json!({"items": [
        {"target_kind": "tasks", "title": "Выгулять собаку",
         "recurrence": {"freq": "daily"}}
    ]});
    let mut stand = stand(&db.pool, vec![Ok(series)]).await;
    stand.capture("каждый день выгуливать собаку").await;

    stand.bot.assert_screen_contains("Срок: сегодня");
    stand.bot.assert_screen_contains("Повторение: Каждый день");

    stand.bot.press("Сохранить всё").await;
    let due: Vec<Option<chrono::NaiveDate>> = sqlx::query_scalar(
        "SELECT i.due_date FROM items i JOIN users u ON u.household_id = i.household_id
         WHERE u.telegram_user_id = $1",
    )
    .bind(stand.bot.chat)
    .fetch_all(&db.pool)
    .await
    .expect("due");
    assert_eq!(due.len(), 1);
    assert!(due[0].is_some(), "the saved item has the shown due date");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn time_that_passes_while_the_draft_waits_is_shown_as_an_error() {
    let db = db().await;
    // Срок через пять минут: при разборе он в будущем.
    let soon = Utc::now().with_timezone(&chrono_tz::Europe::Moscow) + Duration::minutes(5);
    let answer = json!({"items": [
        {"target_kind": "tasks", "title": "Позвонить",
         "due_date": soon.format("%Y-%m-%d").to_string(),
         "due_time": soon.format("%H:%M").to_string()},
        {"target_kind": "shopping", "title": "Хлеб"}
    ]});
    let mut stand = stand(&db.pool, vec![Ok(answer)]).await;
    stand
        .capture("позвонить через пять минут и купить хлеб")
        .await;
    stand.bot.assert_has_button("Сохранить всё");

    // Черновик живёт 15 минут, но срок задачи наступает раньше.
    stand.bot.advance(Duration::minutes(8));
    stand.bot.press("Сохранить всё").await;

    // Ничего не сохранено, карточка объясняет причину и предлагает исправить.
    assert_eq!(stand.household_items().await, 0);
    stand.bot.assert_screen_contains("уже прош");
    assert!(stand.bot.find_button("Сохранить всё").is_none());
    stand.bot.assert_has_button("Изменить 1");

    // Удаление просроченного элемента возвращает возможность сохранить остальное.
    stand.bot.press("Удалить 1").await;
    stand.bot.assert_has_button("Сохранить всё");
    stand.bot.press("Сохранить всё").await;
    assert_eq!(stand.household_items().await, 1);
}
