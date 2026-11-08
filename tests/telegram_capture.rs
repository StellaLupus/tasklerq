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
use tasklerq::security_audit::Audit;
use tasklerq::telegram::bot::Bot;
use tasklerq::telegram::capture_worker::{CaptureWorker, DEFAULT_DAILY_QUOTA, MAX_ATTEMPTS, Pass};
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

/// Ответ LLM: больше 20 элементов, пользователя просят разделить текст.
fn too_many_items() -> Value {
    let items: Vec<Value> = (0..21)
        .map(|n| json!({"target_kind": "tasks", "title": format!("Дело {n}")}))
        .collect();
    json!({"items": items})
}

struct Stand {
    bot: Harness,
    worker: CaptureWorker,
    ai: Arc<StubAi>,
}

/// Сколько разборов в минуту допускает бот.
const PARSES_PER_MINUTE: i64 = 10;

async fn stand(pool: &PgPool, answers: Vec<Result<Value, HaError>>) -> Stand {
    stand_with(pool, answers, DEFAULT_DAILY_QUOTA, true).await
}

async fn stand_with(
    pool: &PgPool,
    answers: Vec<Result<Value, HaError>>,
    quota: u32,
    llm_capture: bool,
) -> Stand {
    let id = next_telegram_id();
    let handler = Bot::new("tasklerqbot")
        .with_llm_capture(llm_capture)
        .with_tenancy(tasklerq::config::TenancyMode::Multi);
    let ai = StubAi::new(answers);
    let mut bot = Harness::with_handler(pool, id, Arc::new(handler.clone()), FakeTelegram::new());
    bot.text("/start").await;
    bot.text("Семья").await;
    bot.press("Москва").await;
    bot.press("меню").await;

    let worker = CaptureWorker::new(
        pool.clone(),
        ai.clone(),
        handler,
        Arc::new(Notify::new()),
        Audit::new(pool.clone()),
    )
    .only_chat(id)
    .with_quota(quota);
    Stand { bot, worker, ai }
}

async fn stand_with_quota(
    pool: &PgPool,
    answers: Vec<Result<Value, HaError>>,
    quota: u32,
) -> Stand {
    stand_with(pool, answers, quota, true).await
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

    /// Последний `seq` outbox этого чата.
    async fn last_outbox_seq(&self) -> i64 {
        sqlx::query_scalar("SELECT coalesce(max(seq), 0) FROM outbox WHERE chat_id = $1")
            .bind(self.bot.chat)
            .fetch_one(&self.bot.pool)
            .await
            .expect("max outbox seq")
    }

    /// Строки outbox, появившиеся после `seq`: `(вид, message_id)`.
    async fn outbox_since(&self, seq: i64) -> Vec<(String, Option<i64>)> {
        sqlx::query_as(
            "SELECT kind::text AS kind, (payload->>'message_id')::bigint AS message_id
             FROM outbox
             WHERE chat_id = $1 AND seq > $2
             ORDER BY seq",
        )
        .bind(self.bot.chat)
        .bind(seq)
        .fetch_all(&self.bot.pool)
        .await
        .expect("outbox rows")
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

    /// Состояние активного экрана чата.
    async fn view_state(&self) -> String {
        sqlx::query_scalar("SELECT state::text FROM telegram_views WHERE chat_id = $1")
            .bind(self.bot.chat)
            .fetch_one(&self.bot.pool)
            .await
            .expect("view state")
    }
}

async fn household_id(pool: &PgPool, chat: i64) -> uuid::Uuid {
    sqlx::query_scalar("SELECT household_id FROM users WHERE telegram_user_id = $1")
        .bind(chat)
        .fetch_one(pool)
        .await
        .expect("household")
}

async fn rename_member(pool: &PgPool, chat: i64, name: &str, username: &str) {
    sqlx::query("UPDATE users SET display_name = $2, username = $3 WHERE telegram_user_id = $1")
        .bind(chat)
        .bind(name)
        .bind(username)
        .execute(pool)
        .await
        .expect("rename");
}

async fn add_member(
    pool: &PgPool,
    household_id: uuid::Uuid,
    name: &str,
    username: &str,
) -> uuid::Uuid {
    let id = uuid::Uuid::now_v7();
    let telegram_id = support::next_telegram_id();
    sqlx::query(
        "INSERT INTO users (
             id, household_id, telegram_user_id, telegram_chat_id, display_name,
             username, role, settings, created_at, updated_at
         )
         VALUES ($1, $2, $3, $3, $4, $5, 'member', '{}'::jsonb, now(), now())",
    )
    .bind(id)
    .bind(household_id)
    .bind(telegram_id)
    .bind(name)
    .bind(username)
    .execute(pool)
    .await
    .expect("member");
    id
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
async fn text_on_a_tasks_list_defaults_capture_to_that_list() {
    let db = db().await;
    let mut stand = stand(
        &db.pool,
        vec![Ok(json!({"items": [
            {"target_kind": "tasks", "title": "Покрасить забор", "target_list": "Задачи"}
        ]}))],
    )
    .await;
    let dacha = add_tasks_list(&db.pool, stand.bot.chat, "Дача").await;

    stand.bot.text("/start").await;
    stand.bot.press("Все списки").await;
    stand.bot.press("Дача").await;
    stand.bot.assert_screen_contains("Дача · 0");
    stand
        .bot
        .assert_screen_contains("Напишите — добавлю в «Дача»");

    stand.bot.text("покрасить забор").await;
    let payload: Value = sqlx::query_scalar(
        "SELECT payload FROM capture_jobs WHERE chat_id = $1
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(stand.bot.chat)
    .fetch_one(&db.pool)
    .await
    .expect("capture job payload");
    assert_eq!(
        payload["default_list_id"],
        json!(dacha),
        "the list screen must become the capture default"
    );

    assert_eq!(stand.work().await, Pass::Drafted);
    stand.bot.assert_screen_contains("Список: Дача");

    stand.bot.press("Сохранить всё").await;
    let saved: uuid::Uuid = sqlx::query_scalar(
        "SELECT i.list_id FROM items i
         JOIN users u ON u.household_id = i.household_id
         WHERE u.telegram_user_id = $1 AND i.title = 'Покрасить забор'",
    )
    .bind(stand.bot.chat)
    .fetch_one(&db.pool)
    .await
    .expect("saved task");
    assert_eq!(saved, dacha);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn text_on_a_tasks_list_keeps_an_explicitly_named_other_list() {
    let db = db().await;
    let mut stand = stand(
        &db.pool,
        vec![Ok(json!({"items": [
            {"target_kind": "tasks", "title": "Покрасить забор", "target_list": "Работа"}
        ]}))],
    )
    .await;
    let dacha = add_tasks_list(&db.pool, stand.bot.chat, "Дача").await;
    let work = add_tasks_list(&db.pool, stand.bot.chat, "Работа").await;

    stand.bot.text("/start").await;
    stand.bot.press("Все списки").await;
    stand.bot.press("Дача").await;
    stand.bot.assert_screen_contains("Дача · 0");

    stand.bot.text("покрасить забор").await;
    let payload: Value = sqlx::query_scalar(
        "SELECT payload FROM capture_jobs WHERE chat_id = $1
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(stand.bot.chat)
    .fetch_one(&db.pool)
    .await
    .expect("capture job payload");
    assert_eq!(payload["default_list_id"], json!(dacha));

    assert_eq!(stand.work().await, Pass::Drafted);
    stand.bot.assert_screen_contains("Список: Работа");
    assert_ne!(work, dacha);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn text_does_not_replace_an_open_editor() {
    let db = db().await;
    let mut stand = stand(&db.pool, vec![]).await;

    stand.bot.press("Добавить").await;
    stand.bot.text("Задача в редакторе").await;
    stand.bot.assert_has_button("💾 Сохранить");
    let jobs_before = stand.jobs().await;
    let calls_before = stand.ai.calls();

    stand.bot.text("покрасить забор").await;
    stand
        .bot
        .assert_screen_contains("Сначала сохраните или отмените правку");
    stand
        .bot
        .assert_screen_contains("Название: Задача в редакторе");
    stand.bot.assert_has_button("💾 Сохранить");
    assert_eq!(
        stand.jobs().await,
        jobs_before,
        "text must not enqueue a capture job while the editor is open"
    );
    assert_eq!(stand.ai.calls(), calls_before);
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
    stand.bot.assert_screen_contains("Название: Позвонить маме");
    stand.bot.assert_screen_contains("Позвонить маме");
    for (label, _) in stand.bot.buttons() {
        assert!(!label.contains("Дальше"), "draft editor: {label}");
        assert!(!label.contains("Пропустить"), "draft editor: {label}");
    }

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
async fn editing_a_draft_item_changes_only_the_draft_and_keeps_the_quote() {
    let db = db().await;
    let mut stand = stand(&db.pool, vec![Ok(two_items())]).await;
    let text = "позвонить маме и купить молоко";
    stand.capture(text).await;

    stand.bot.press("Изменить 1").await;
    stand.bot.assert_screen_contains("Название: Позвонить маме");
    stand.bot.press("Срок").await;
    stand.bot.press("Завтра").await;
    stand.bot.assert_screen_contains("Срок: Завтра");
    stand.bot.press("Сохранить").await;

    stand.bot.assert_screen_contains("Разобрал: 2 позиции");
    stand.bot.assert_screen_contains("Срок: завтра");
    let html = stand.bot.screen();
    assert!(
        html.contains("<blockquote>") && html.contains(text),
        "the quote must survive the return from the editor:\n{html}"
    );
    assert_eq!(
        stand.household_items().await,
        0,
        "editing a draft item creates nothing"
    );
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
    let html = stand.bot.screen();
    assert!(
        html.contains("<blockquote>"),
        "the quote must survive cancelling the edit:\n{html}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn draft_item_without_a_list_requires_the_list_field() {
    let db = db().await;
    let items = json!({"items": [{"target_kind": "checklist", "title": "Проверка"}]});
    let mut stand = stand(&db.pool, vec![Ok(items)]).await;
    stand.capture("сделать проверку").await;

    stand.bot.assert_screen_contains("Нет подходящего списка");
    stand.bot.press("Изменить 1").await;
    stand.bot.assert_screen_contains("В какой список?");

    stand.bot.press("Назад").await;
    stand.bot.press("Сохранить").await;
    stand.bot.assert_screen_contains("Выберите список.");
    stand.bot.assert_screen_contains("В какой список?");

    stand.bot.press("Задачи").await;
    stand.bot.assert_screen_contains("Список: Задачи");
    stand.bot.press("Сохранить").await;
    stand.bot.assert_screen_contains("Проверка");
    stand.bot.assert_screen_contains("Список: Задачи");
    assert_eq!(
        stand.household_items().await,
        0,
        "fixing a draft item creates nothing"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_draft_editor_returns_to_the_draft_without_a_card_promise() {
    let db = db().await;
    let mut stand = stand(&db.pool, vec![Ok(two_items())]).await;
    stand.capture("позвонить маме и купить молоко").await;

    stand.bot.press("Изменить 1").await;
    stand.bot.assert_screen_contains("Название: Позвонить маме");
    stand.bot.advance(Duration::minutes(16));
    stand.bot.text("Поздний ответ").await;

    stand.bot.assert_screen_contains("Правка устарела");
    stand.bot.assert_has_button("К черновику");
    assert!(
        stand.bot.find_button("Открыть карточку").is_none(),
        "a draft item is not a card:\n{}",
        stand.bot.screen()
    );
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
    let before = stand.last_outbox_seq().await;

    stand.bot.text(&"а".repeat(2_001)).await;

    stand.bot.assert_screen_contains("слишком длинное");
    let rows = stand.outbox_since(before).await;
    assert!(
        rows.iter().any(|(kind, _)| kind == "delete"),
        "the rejected text must be deleted: {rows:?}"
    );
    assert!(
        !rows.iter().any(|(kind, _)| kind == "message"),
        "a rejection is a banner, not a separate message: {rows:?}"
    );
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
    let mut stand = stand(&db.pool, vec![Ok(too_many_items())]).await;

    stand.bot.text("очень длинный список дел").await;
    assert_eq!(stand.work().await, Pass::Dead);

    stand.bot.assert_screen_contains("Разделите его на части");
    assert_eq!(stand.household_items().await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn too_many_items_do_not_open_the_circuit_breaker() {
    let db = db().await;
    let mut answers: Vec<Result<Value, HaError>> = (0..5).map(|_| Ok(too_many_items())).collect();
    answers.push(Ok(two_items()));
    let mut stand = stand(&db.pool, answers).await;

    for number in 1..=5 {
        stand
            .bot
            .text(&format!("очень длинный список {number}"))
            .await;
        assert_eq!(stand.work().await, Pass::Dead);
        stand.bot.assert_screen_contains("Разделите его на части");
        stand.bot.press("Отмена").await;
    }

    // Пользовательская ошибка не расходует порог предохранителя: шестое
    // сообщение всё ещё уходит в LLM.
    stand.capture("шестое дело").await;
    assert_eq!(stand.ai.calls(), 6);
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
async fn ambiguous_assignee_asks_and_the_choice_updates_the_draft_without_llm() {
    let db = db().await;
    let mut stand = stand(
        &db.pool,
        vec![Ok(json!({"items": [
            {"target_kind": "shopping", "title": "Торт", "assignee": "Алексей"}
        ]}))],
    )
    .await;
    let household = household_id(&db.pool, stand.bot.chat).await;
    rename_member(&db.pool, stand.bot.chat, "Алексей", "alex_a").await;
    let chosen = add_member(&db.pool, household, "Алексей", "alex_b").await;

    stand.capture("назначить Алексею купить торт").await;

    stand.bot.assert_screen_contains("Какой исполнитель");
    stand.bot.assert_has_button("alex_a");
    stand.bot.assert_has_button("alex_b");
    assert!(
        stand.bot.find_button("Сохранить всё").is_none(),
        "ambiguous assignee must block saving:\n{}",
        stand.bot.screen()
    );
    let calls = stand.ai.calls();

    stand.bot.press("alex_b").await;

    assert_eq!(
        stand.ai.calls(),
        calls,
        "the answer must not call the LLM again"
    );
    assert!(!stand.bot.screen().contains("Какой исполнитель"));
    stand.bot.assert_has_button("Сохранить всё");

    stand.bot.press("Сохранить всё").await;
    let assignee: Option<uuid::Uuid> = sqlx::query_scalar(
        "SELECT i.assignee_user_id FROM items i JOIN users u ON u.household_id = i.household_id
         WHERE u.telegram_user_id = $1",
    )
    .bind(stand.bot.chat)
    .fetch_one(&db.pool)
    .await
    .expect("assignee");
    assert_eq!(assignee, Some(chosen));
}

/// Карточка с прошедшей датой и вопросом о сроке.
async fn ambiguous_date_stand(db: &support::TestDb) -> (Stand, chrono::NaiveDate) {
    let timezone = chrono_tz::Europe::Moscow;
    let today = Utc::now().with_timezone(&timezone).date_naive();
    let past = (today - Duration::days(2)).format("%Y-%m-%d").to_string();
    let answer = json!({"items": [
        {"target_kind": "tasks", "title": "Оплатить счёт", "due_date": past}
    ]});
    let mut stand = stand(&db.pool, vec![Ok(answer)]).await;
    stand.capture("оплатить счёт позавчера").await;
    (stand, today)
}

async fn saved_due_date(stand: &Stand) -> Option<chrono::NaiveDate> {
    sqlx::query_scalar(
        "SELECT i.due_date FROM items i JOIN users u ON u.household_id = i.household_id
         WHERE u.telegram_user_id = $1",
    )
    .bind(stand.bot.chat)
    .fetch_one(&stand.bot.pool)
    .await
    .expect("due date")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ambiguous_date_offers_today_and_tomorrow_without_guessing() {
    let db = db().await;
    let (mut stand, today) = ambiguous_date_stand(&db).await;

    stand.bot.assert_screen_contains("Уточните срок");
    stand.bot.assert_has_button("Сегодня");
    stand.bot.assert_has_button("Завтра");
    stand.bot.assert_has_button("Выбрать дату");
    assert!(
        stand.bot.find_button("Сохранить всё").is_none(),
        "an ambiguous date must block saving:\n{}",
        stand.bot.screen()
    );
    let calls = stand.ai.calls();

    stand.bot.press("Завтра").await;

    assert_eq!(
        stand.ai.calls(),
        calls,
        "the answer must not call the LLM again"
    );
    assert!(!stand.bot.screen().contains("Уточните срок"));
    stand.bot.assert_has_button("Сохранить всё");

    stand.bot.press("Сохранить всё").await;
    assert_eq!(
        saved_due_date(&stand).await,
        Some(today + Duration::days(1))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn choosing_today_saves_todays_due_date() {
    let db = db().await;
    let (mut stand, today) = ambiguous_date_stand(&db).await;

    stand.bot.press("Сегодня").await;

    assert!(!stand.bot.screen().contains("Уточните срок"));
    stand.bot.assert_has_button("Сохранить всё");
    stand.bot.press("Сохранить всё").await;
    assert_eq!(saved_due_date(&stand).await, Some(today));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn choosing_a_date_opens_the_form_on_the_due_step() {
    let db = db().await;
    let (mut stand, _) = ambiguous_date_stand(&db).await;

    stand.bot.press("Выбрать дату").await;

    stand.bot.assert_screen_contains("Когда сделать?");
    stand.bot.assert_has_button("📅 Дата и время");
    assert_eq!(
        stand.ai.calls(),
        1,
        "opening the form must not parse the text again"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn after_three_clarifications_the_bot_offers_manual_editing() {
    let db = db().await;
    let items: Vec<Value> = (0..4)
        .map(|n| {
            json!({"target_kind": "tasks", "title": format!("Дело {n}"), "assignee": "Алексей"})
        })
        .collect();
    let mut stand = stand(&db.pool, vec![Ok(json!({"items": items}))]).await;
    let household = household_id(&db.pool, stand.bot.chat).await;
    rename_member(&db.pool, stand.bot.chat, "Алексей", "alex_a").await;
    add_member(&db.pool, household, "Алексей", "alex_b").await;

    stand.capture("четыре дела для Алексея").await;

    for _ in 0..3 {
        stand.bot.assert_screen_contains("Какой исполнитель");
        stand.bot.press("alex_b").await;
    }

    stand
        .bot
        .assert_screen_contains("Больше не буду уточнять автоматически");
    stand.bot.assert_screen_contains("Изменить");
    assert!(
        stand.bot.find_button("alex_b").is_none(),
        "no fourth question expected:\n{}",
        stand.bot.screen()
    );
    assert_eq!(stand.ai.calls(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_parse_opens_the_manual_form_with_the_source_text() {
    let db = db().await;
    let mut stand = stand(&db.pool, vec![Err(HaError::Rejected(400))]).await;

    stand.bot.text("купить молоко").await;
    assert_eq!(stand.work().await, Pass::Dead);

    stand
        .bot
        .assert_screen_contains("Не получилось разобрать сообщение");
    stand.bot.assert_screen_contains("Название: купить молоко");
    stand.bot.assert_screen_contains("Список: Задачи");
    for (label, _) in stand.bot.buttons() {
        assert!(!label.contains("Дальше"), "quick form: {label}");
        assert!(!label.contains("Пропустить"), "quick form: {label}");
    }
    stand.bot.assert_has_button("Сохранить");
    assert_eq!(stand.ai.calls(), 1);

    stand.bot.press("Сохранить").await;
    assert_eq!(stand.household_items().await, 1, "saving creates the task");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn circuit_breaker_skips_the_llm_after_five_failures_and_recovers() {
    let db = db().await;
    let mut answers: Vec<Result<Value, HaError>> =
        (0..5).map(|_| Err(HaError::Rejected(400))).collect();
    answers.push(Ok(two_items()));
    let mut stand = stand(&db.pool, answers).await;

    for number in 1..=5 {
        stand.bot.text(&format!("дело {number}")).await;
        assert_eq!(stand.work().await, Pass::Dead);
        stand
            .bot
            .assert_screen_contains(&format!("Название: дело {number}"));
        stand.bot.press("Отмена").await;
    }
    assert_eq!(stand.ai.calls(), 5);

    // Предохранитель разомкнут: шестое сообщение не уходит в LLM, а сразу
    // открывает ручную форму с исходным текстом.
    stand.bot.text("дело 6").await;
    assert_eq!(stand.work().await, Pass::Dead);
    assert_eq!(stand.ai.calls(), 5, "the LLM must not be called while open");
    stand.bot.assert_screen_contains("временно недоступен");
    stand.bot.assert_screen_contains("дело 6");
    stand.bot.press("Отмена").await;

    // Через две минуты разбор снова разрешён.
    stand.bot.advance(Duration::minutes(2));
    stand.bot.text("дело 7").await;
    assert_eq!(stand.work().await, Pass::Drafted);
    assert_eq!(stand.ai.calls(), 6);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_successful_parse_resets_the_breaker_failure_count() {
    let db = db().await;
    let answers = vec![
        Err(HaError::Rejected(400)),
        Err(HaError::Rejected(400)),
        Err(HaError::Rejected(400)),
        Err(HaError::Rejected(400)),
        Ok(two_items()),
        Err(HaError::Rejected(400)),
        Ok(two_items()),
    ];
    let mut stand = stand(&db.pool, answers).await;

    for number in 1..=4 {
        stand.bot.text(&format!("дело {number}")).await;
        assert_eq!(stand.work().await, Pass::Dead);
        stand.bot.press("Отмена").await;
    }

    // Успешный разбор обнуляет счётчик ошибок.
    stand.capture("дело 5").await;
    stand.bot.press("Отмена").await;

    stand.bot.text("дело 6").await;
    assert_eq!(stand.work().await, Pass::Dead);
    stand.bot.press("Отмена").await;

    // Без сброса счётчика пятая ошибка разомкнула бы предохранитель.
    stand.capture("дело 7").await;
    assert_eq!(stand.ai.calls(), 7);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn more_than_ten_parses_per_minute_are_rejected_without_llm() {
    let db = db().await;
    let mut stand = stand(&db.pool, vec![]).await;

    for number in 1..=PARSES_PER_MINUTE {
        stand.bot.text(&format!("дело {number}")).await;
    }
    assert_eq!(stand.jobs().await, PARSES_PER_MINUTE);

    let before = stand.last_outbox_seq().await;
    stand.bot.text("дело 11").await;
    stand.bot.assert_screen_contains("Слишком много сообщений");
    let rows = stand.outbox_since(before).await;
    assert!(
        rows.iter().any(|(kind, _)| kind == "delete"),
        "the rejected text must be deleted: {rows:?}"
    );
    assert!(
        !rows.iter().any(|(kind, _)| kind == "message"),
        "a rejection is a banner, not a separate message: {rows:?}"
    );
    assert_eq!(stand.jobs().await, PARSES_PER_MINUTE);
    assert_eq!(stand.ai.calls(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daily_quota_stops_parsing_and_opens_the_manual_form() {
    let db = db().await;
    let mut stand = stand_with_quota(&db.pool, vec![Ok(two_items()), Ok(two_items())], 2).await;

    stand.bot.text("первое дело").await;
    assert_eq!(stand.work().await, Pass::Drafted);
    stand.bot.text("второе дело").await;
    assert_eq!(stand.work().await, Pass::Drafted);

    stand.bot.text("третье дело").await;
    assert_eq!(stand.work().await, Pass::Dead);
    assert_eq!(stand.ai.calls(), 2, "the quota must block the LLM call");
    stand
        .bot
        .assert_screen_contains("автоматический разбор закончился");
    stand.bot.assert_screen_contains("третье дело");
    stand.bot.assert_has_button("Сохранить");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quota_ignores_runs_older_than_a_day() {
    let db = db().await;
    let mut stand = stand_with_quota(&db.pool, vec![Ok(two_items()), Ok(two_items())], 1).await;

    stand.capture("первое дело").await;
    let household = household_id(&db.pool, stand.bot.chat).await;
    sqlx::query("UPDATE llm_runs SET created_at = $2 WHERE household_id = $1")
        .bind(household)
        .bind(stand.bot.now - Duration::hours(25))
        .execute(&db.pool)
        .await
        .expect("age the run");

    stand.capture("второе дело").await;
    assert_eq!(stand.ai.calls(), 2, "the quota is a rolling day");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disabled_llm_opens_the_manual_form_without_queueing() {
    let db = db().await;
    let mut stand = stand_with(&db.pool, vec![], DEFAULT_DAILY_QUOTA, false).await;

    stand.bot.text("купить хлеб").await;

    stand.bot.assert_screen_contains("Название: купить хлеб");
    stand.bot.assert_screen_contains("купить хлеб");
    assert_eq!(stand.jobs().await, 0);
    assert_eq!(stand.ai.calls(), 0);
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_processing_screen_quotes_the_source_text_without_storing_it_in_the_view() {
    let db = db().await;
    let mut stand = stand(&db.pool, vec![Ok(two_items())]).await;
    let text = "завтра купить молоко и хлеб";

    stand.bot.text(text).await;

    let html = stand.bot.screen();
    let quote = html.find("<blockquote>").expect("quote block");
    let status = html.find("Разбираю сообщение").expect("processing status");
    assert!(quote < status, "the quote must come first:\n{html}");
    assert!(
        html.contains(text),
        "the quote must contain the text:\n{html}"
    );

    let state = stand.view_state().await;
    assert!(
        !state.contains(text) && !state.contains("молоко") && !state.contains("купить"),
        "the source text must not be stored in telegram_views.state: {state}"
    );
    assert!(
        state.contains("job_id"),
        "the view keeps the job id: {state}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_draft_card_quotes_the_source_text_without_storing_it_in_the_view() {
    let db = db().await;
    let mut stand = stand(&db.pool, vec![Ok(two_items())]).await;
    let text = "позвонить маме и купить молоко";

    stand.capture(text).await;

    let html = stand.bot.screen();
    let quote = html.find("<blockquote>").expect("quote block");
    let heading = html.find("Разобрал: 2 позиции").expect("card heading");
    assert!(quote < heading, "the quote must come first:\n{html}");
    assert!(
        html.contains(text),
        "the quote must contain the text:\n{html}"
    );

    let state = stand.view_state().await;
    assert!(
        !state.contains(text) && !state.contains("молоко") && !state.contains("позвонить"),
        "the source text must not be stored in telegram_views.state: {state}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_quote_survives_removing_and_editing_draft_items() {
    let db = db().await;
    let mut stand = stand(&db.pool, vec![Ok(two_items())]).await;
    let text = "позвонить маме и купить молоко";
    stand.capture(text).await;

    stand.bot.press("Удалить 1").await;
    let html = stand.bot.screen();
    assert!(html.contains("<blockquote>"), "after removal:\n{html}");
    assert!(html.contains(text), "after removal:\n{html}");

    stand.bot.press("Изменить 1").await;
    stand.bot.press("Сохранить").await;
    let html = stand.bot.screen();
    assert!(html.contains("<blockquote>"), "after editing:\n{html}");
    assert!(html.contains(text), "after editing:\n{html}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_long_source_text_is_collapsed_in_the_quote() {
    let db = db().await;
    let mut stand = stand(&db.pool, vec![Ok(two_items())]).await;
    let text = "строка один\nстрока два\nстрока три\nстрока четыре";

    stand.bot.text(text).await;

    let html = stand.bot.screen();
    assert!(
        html.contains("<blockquote expandable>"),
        "four lines must collapse:\n{html}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_quote_escapes_markup_on_the_processing_screen_and_in_the_card() {
    let db = db().await;
    let mut stand = stand(&db.pool, vec![Ok(two_items())]).await;
    let text = "<b>x</b> & y";
    let escaped = "&lt;b&gt;x&lt;/b&gt; &amp; y";

    stand.bot.text(text).await;
    let processing = stand.bot.screen();
    assert!(
        processing.contains(escaped),
        "the processing quote must be escaped:\n{processing}"
    );
    assert!(
        !processing.contains("<b>x</b>"),
        "the processing quote must not contain raw markup:\n{processing}"
    );

    assert_eq!(stand.work().await, Pass::Drafted);
    let card = stand.bot.screen();
    assert!(
        card.contains(escaped),
        "the draft card quote must be escaped:\n{card}"
    );
    assert!(
        !card.contains("<b>x</b>"),
        "the draft card quote must not contain raw markup:\n{card}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_long_source_text_is_collapsed_in_the_draft_card_quote() {
    let db = db().await;
    let mut stand = stand(&db.pool, vec![Ok(two_items())]).await;
    let text = "строка один\nстрока два\nстрока три\nстрока четыре";

    stand.capture(text).await;

    let html = stand.bot.screen();
    assert!(
        html.contains("<blockquote expandable>"),
        "four lines must collapse in the draft card too:\n{html}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closed_drafts_do_not_quote_the_source_text() {
    let db = db().await;
    let text = "позвонить маме и купить молоко";

    let mut saved = stand(&db.pool, vec![Ok(two_items())]).await;
    saved.capture(text).await;
    saved.bot.press("Сохранить всё").await;
    saved.bot.assert_screen_contains("Сохранено: 2 позиции");
    assert!(!saved.bot.screen().contains("<blockquote>"));

    let mut cancelled = stand(&db.pool, vec![Ok(two_items())]).await;
    cancelled.capture(text).await;
    cancelled.bot.press("Отмена").await;
    assert!(!cancelled.bot.screen().contains("<blockquote>"));

    let mut expired = stand(&db.pool, vec![Ok(two_items())]).await;
    expired.capture(text).await;
    expired.bot.advance(Duration::minutes(16));
    expired.bot.press("Сохранить всё").await;
    expired.bot.assert_screen_contains("Черновик устарел");
    assert!(!expired.bot.screen().contains("<blockquote>"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_old_processing_view_without_a_job_is_shown_without_a_quote() {
    let db = db().await;
    let mut stand = stand(&db.pool, vec![Ok(two_items())]).await;

    // Состояние, сохранённое до выкатки: без `job_id`.
    sqlx::query(
        "UPDATE telegram_views SET state = '{\"t\": \"processing\"}'::jsonb WHERE chat_id = $1",
    )
    .bind(stand.bot.chat)
    .execute(&db.pool)
    .await
    .expect("old view state");

    stand.bot.text("/nonsense").await;

    let html = stand.bot.screen();
    assert!(html.contains("Разбираю сообщение"), "{html}");
    assert!(!html.contains("<blockquote>"), "{html}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quick_form_switches_to_a_shopping_list_and_saves_there() {
    let db = db().await;
    let mut stand = stand_with(&db.pool, vec![], DEFAULT_DAILY_QUOTA, false).await;

    stand.bot.text("купить молоко 2 л").await;
    stand
        .bot
        .assert_screen_contains("Название: купить молоко 2 л");
    stand.bot.assert_has_button("Список");
    stand.bot.press("Список").await;
    stand.bot.assert_screen_contains("В какой список?");
    stand.bot.press("Покупки").await;

    // Предпросмотр покупки: количество вместо приоритета, название сохранено.
    stand.bot.assert_screen_contains("купить молоко 2 л");
    stand.bot.assert_screen_contains("Количество:");
    assert!(!stand.bot.screen().contains("Приоритет"));

    stand.bot.press("Сохранить").await;
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT i.title, l.kind::text
         FROM items i
         JOIN lists l ON l.id = i.list_id
         JOIN users u ON u.household_id = i.household_id
         WHERE u.telegram_user_id = $1 AND i.deleted_at IS NULL
         ORDER BY i.title",
    )
    .bind(stand.bot.chat)
    .fetch_all(&db.pool)
    .await
    .expect("items");
    assert_eq!(
        rows,
        vec![("купить молоко 2 л".to_owned(), "shopping".to_owned())],
        "the item must go to the shopping list, not to the default tasks list"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quick_form_can_pick_another_tasks_list_without_changing_the_type() {
    let db = db().await;
    let mut stand = stand_with(&db.pool, vec![], DEFAULT_DAILY_QUOTA, false).await;
    let second = add_second_tasks_list(&db.pool, stand.bot.chat).await;

    stand.bot.text("позвонить маме").await;
    stand.bot.press("Список").await;
    stand.bot.press("Работа").await;

    stand.bot.assert_screen_contains("Приоритет");
    assert!(!stand.bot.screen().contains("Количество:"));
    stand.bot.press("Сохранить").await;

    let saved: Vec<(String, uuid::Uuid)> = sqlx::query_as(
        "SELECT i.title, i.list_id
         FROM items i
         JOIN users u ON u.household_id = i.household_id
         WHERE u.telegram_user_id = $1 AND i.deleted_at IS NULL",
    )
    .bind(stand.bot.chat)
    .fetch_all(&db.pool)
    .await
    .expect("items");
    assert_eq!(saved, vec![("позвонить маме".to_owned(), second)]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn switching_to_shopping_drops_task_fields_and_back() {
    let db = db().await;
    let mut stand = stand_with(&db.pool, vec![], DEFAULT_DAILY_QUOTA, false).await;

    stand.bot.text("позвонить маме").await;
    stand.bot.press("Срок").await;
    stand.bot.press("Завтра").await;
    stand.bot.assert_screen_contains("Срок: Завтра");
    stand.bot.press("Теги").await;
    stand.bot.press("Новый тег").await;
    stand.bot.text("дом").await;
    stand.bot.press("Применить").await;
    stand.bot.assert_screen_contains("Теги: дом");

    stand.bot.press("Список").await;
    stand.bot.press("Покупки").await;
    assert!(
        !stand.bot.screen().contains("Срок:"),
        "the shopping editor must not keep the task due date:\n{}",
        stand.bot.screen()
    );
    assert!(
        !stand.bot.screen().contains("Теги:"),
        "the shopping editor must not keep task tags:\n{}",
        stand.bot.screen()
    );
    stand.bot.assert_screen_contains("Количество:");

    // Обратно в задачу: поля покупки сброшены, поля задачи пусты.
    stand.bot.press("Список").await;
    stand.bot.press("Задачи").await;
    stand.bot.assert_screen_contains("Срок: без срока");
    stand.bot.assert_screen_contains("Теги: нет");

    // Покупка без срока и тегов сохраняется.
    stand.bot.press("Список").await;
    stand.bot.press("Покупки").await;
    stand.bot.press("Сохранить").await;
    let saved: (
        String,
        Option<chrono::NaiveDate>,
        Option<chrono::DateTime<Utc>>,
    ) = sqlx::query_as(
        "SELECT l.kind::text, i.due_date, i.due_at
             FROM items i
             JOIN lists l ON l.id = i.list_id
             JOIN users u ON u.household_id = i.household_id
             WHERE u.telegram_user_id = $1 AND i.deleted_at IS NULL",
    )
    .bind(stand.bot.chat)
    .fetch_one(&db.pool)
    .await
    .expect("item");
    assert_eq!(saved, ("shopping".to_owned(), None, None));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn menu_create_opens_the_editor_with_the_list_switch() {
    let db = db().await;
    let mut stand = stand_with(&db.pool, vec![], DEFAULT_DAILY_QUOTA, false).await;

    stand.bot.press("Добавить").await;
    stand.bot.assert_screen_contains("Как назвать задачу?");
    stand.bot.text("Обычная задача").await;

    stand.bot.assert_screen_contains("Название: Обычная задача");
    stand.bot.assert_screen_contains("Список: Задачи");
    assert!(
        stand.bot.find_button("📋 Список").is_some(),
        "creation must offer the list switch:\n{}",
        stand.bot.screen()
    );
    assert!(
        stand.bot.find_button("Предпросмотр").is_none(),
        "creation must not have a preview step:\n{}",
        stand.bot.screen()
    );
}

async fn add_second_tasks_list(pool: &sqlx::PgPool, telegram_id: i64) -> uuid::Uuid {
    add_tasks_list(pool, telegram_id, "Работа").await
}

async fn add_tasks_list(pool: &sqlx::PgPool, telegram_id: i64, name: &str) -> uuid::Uuid {
    let household = household_id(pool, telegram_id).await;
    let list = uuid::Uuid::now_v7();
    sqlx::query(
        "INSERT INTO lists (
             id, household_id, kind, name, is_default, is_archived, position,
             created_at, updated_at
         )
         VALUES ($1, $2, 'tasks', $3, false, false, 10, now(), now())",
    )
    .bind(list)
    .bind(household)
    .bind(name)
    .execute(pool)
    .await
    .expect("second tasks list");
    list
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quick_form_can_return_from_list_step_to_the_editor() {
    let db = db().await;
    let mut stand = stand_with(&db.pool, vec![], DEFAULT_DAILY_QUOTA, false).await;

    stand.bot.text("позвонить маме").await;
    stand.bot.press("Список").await;
    stand.bot.assert_screen_contains("В какой список?");
    stand.bot.assert_has_button("Назад");

    stand.bot.press("Назад").await;
    stand.bot.assert_screen_contains("Название: позвонить маме");
    stand.bot.assert_screen_contains("Список: Задачи");

    let saved: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM items i
         JOIN users u ON u.household_id = i.household_id
         WHERE u.telegram_user_id = $1 AND i.deleted_at IS NULL",
    )
    .bind(stand.bot.chat)
    .fetch_one(&db.pool)
    .await
    .expect("items");
    assert_eq!(saved, 0, "returning from the list must not save the form");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn menu_editor_list_step_offers_lists_of_both_types() {
    let db = db().await;
    let mut stand = stand_with(&db.pool, vec![], DEFAULT_DAILY_QUOTA, false).await;
    add_second_tasks_list(&db.pool, stand.bot.chat).await;

    stand.bot.press("Добавить").await;
    stand.bot.text("Обычная задача").await;
    stand.bot.press("Список").await;
    stand.bot.assert_screen_contains("В какой список?");

    let labels: Vec<String> = stand
        .bot
        .buttons()
        .into_iter()
        .map(|(label, _)| label)
        .collect();
    assert!(
        labels.iter().any(|label| label.contains("Работа")),
        "the editor must offer other task lists: {labels:?}"
    );
    assert!(
        labels.iter().any(|label| label.contains("📋 Задачи"))
            && labels.iter().any(|label| label.contains("🛒 Покупки")),
        "the editor switches the type through the list screen: {labels:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quick_purchase_to_task_drops_quantity_and_unit() {
    let db = db().await;
    let mut stand = stand_with(&db.pool, vec![], DEFAULT_DAILY_QUOTA, false).await;

    stand.bot.text("Сыр").await;
    stand.bot.press("Список").await;
    stand.bot.press("Покупки").await;
    stand.bot.press("Количество").await;
    stand.bot.press("2").await;
    stand.bot.press("Единица").await;
    stand.bot.press("л").await;
    stand.bot.assert_screen_contains("Количество: 2 л");

    stand.bot.press("Список").await;
    stand.bot.press("Задачи").await;
    stand.bot.assert_screen_contains("Список: Задачи");
    assert!(
        !stand.bot.screen().contains("Количество:"),
        "the task editor must not keep the purchase quantity:\n{}",
        stand.bot.screen()
    );
    stand.bot.press("Сохранить").await;

    let saved: (String, Option<f64>, Option<String>) = sqlx::query_as(
        "SELECT l.kind::text, i.quantity::float8, i.unit
         FROM items i
         JOIN lists l ON l.id = i.list_id
         JOIN users u ON u.household_id = i.household_id
         WHERE u.telegram_user_id = $1 AND i.deleted_at IS NULL",
    )
    .bind(stand.bot.chat)
    .fetch_one(&db.pool)
    .await
    .expect("item");
    assert_eq!(saved, ("tasks".to_owned(), None, None));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quick_task_to_purchase_drops_priority_tags_and_recurrence() {
    let db = db().await;
    let mut stand = stand_with(&db.pool, vec![], DEFAULT_DAILY_QUOTA, false).await;

    stand.bot.text("Полить цветы").await;
    stand.bot.press("Приоритет").await;
    stand.bot.press("Высокий").await;
    stand.bot.press("Теги").await;
    stand.bot.press("Новый тег").await;
    stand.bot.text("дом").await;
    stand.bot.press("Применить").await;
    stand.bot.assert_screen_contains("Теги: дом");
    stand.bot.press("Подзадачи").await;
    stand.bot.press("Применить").await;
    stand.bot.press("Повторение").await;
    stand.bot.press("Каждый день").await;
    stand.bot.press("Применить").await;
    stand.bot.assert_screen_contains("Повторение: Каждый день");

    stand.bot.press("Список").await;
    stand.bot.press("Покупки").await;
    assert!(!stand.bot.screen().contains("Приоритет"));
    stand.bot.assert_screen_contains("Количество:");
    stand.bot.press("Сохранить").await;

    let saved: (String, String, bool, i64) = sqlx::query_as(
        "SELECT l.kind::text, i.priority::text,
                i.recurrence_freq IS NOT NULL,
                (SELECT count(*) FROM item_tags it WHERE it.item_id = i.id)
         FROM items i
         JOIN lists l ON l.id = i.list_id
         JOIN users u ON u.household_id = i.household_id
         WHERE u.telegram_user_id = $1 AND i.deleted_at IS NULL",
    )
    .bind(stand.bot.chat)
    .fetch_one(&db.pool)
    .await
    .expect("item");
    assert_eq!(
        saved,
        ("shopping".to_owned(), "normal".to_owned(), false, 0),
        "a purchase must not keep task-only fields"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quick_form_switching_to_shopping_finds_a_duplicate() {
    let db = db().await;
    let mut stand = stand_with(&db.pool, vec![], DEFAULT_DAILY_QUOTA, false).await;

    stand.bot.text("молоко").await;
    stand.bot.press("Список").await;
    stand.bot.press("Покупки").await;
    stand.bot.press("Сохранить").await;
    stand.bot.assert_screen_contains("Покупки · 1");

    stand.bot.text("Молоко").await;
    stand.bot.press("Список").await;
    stand.bot.press("Покупки").await;
    stand.bot.press("Сохранить").await;
    stand.bot.assert_screen_contains("уже есть в списке");

    let saved: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM items i
         JOIN users u ON u.household_id = i.household_id
         WHERE u.telegram_user_id = $1 AND i.deleted_at IS NULL",
    )
    .bind(stand.bot.chat)
    .fetch_one(&db.pool)
    .await
    .expect("items");
    assert_eq!(saved, 1, "the duplicate must not be saved as a second item");
}

async fn shopping_list_id(pool: &PgPool, telegram_id: i64) -> uuid::Uuid {
    sqlx::query_scalar(
        "SELECT l.id FROM lists l
         JOIN users u ON u.household_id = l.household_id
         WHERE u.telegram_user_id = $1 AND l.kind = 'shopping' AND l.is_default
         LIMIT 1",
    )
    .bind(telegram_id)
    .fetch_one(pool)
    .await
    .expect("shopping list")
}

async fn seed_shopping_item(
    pool: &PgPool,
    telegram_id: i64,
    title: &str,
    quantity: f64,
    unit: &str,
) -> uuid::Uuid {
    let household = household_id(pool, telegram_id).await;
    let list = shopping_list_id(pool, telegram_id).await;
    let id = uuid::Uuid::now_v7();
    sqlx::query(
        "INSERT INTO items (
             id, household_id, list_id, title, status, priority, source,
             quantity, unit, position, created_at, updated_at
         )
         VALUES ($1, $2, $3, $4, 'open', 'normal', 'telegram', $5, $6, 1, now(), now())",
    )
    .bind(id)
    .bind(household)
    .bind(list)
    .bind(title)
    .bind(quantity)
    .bind(unit)
    .execute(pool)
    .await
    .expect("seeded shopping item");
    id
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shopping_text_adds_positions_without_confirmation() {
    let db = db().await;
    let mut stand = stand(
        &db.pool,
        vec![Ok(json!({"items": [
            {"target_kind": "shopping", "title": "Молоко", "quantity": 2, "unit": "л"},
            {"target_kind": "shopping", "title": "Хлеб"}
        ]}))],
    )
    .await;
    let list = shopping_list_id(&db.pool, stand.bot.chat).await;

    stand.bot.press("Покупки").await;
    stand.bot.assert_screen_contains("Покупки · 0");

    stand.bot.text("молоко 2 л и хлеб").await;
    stand.bot.assert_screen_contains("Добавляю…");
    assert!(
        !stand.bot.screen().contains("Разбираю сообщение"),
        "the shopping list must stay on screen:\n{}",
        stand.bot.screen()
    );

    let payload: Value = sqlx::query_scalar(
        "SELECT payload FROM capture_jobs WHERE chat_id = $1
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(stand.bot.chat)
    .fetch_one(&db.pool)
    .await
    .expect("capture job payload");
    assert_eq!(payload["target"]["list_id"], json!(list));
    assert_eq!(payload["target"]["direct"], json!(true));
    assert!(
        payload.get("default_list_id").is_none(),
        "a direct job must carry only target: {payload}"
    );

    assert_eq!(stand.work().await, Pass::Drafted);

    stand
        .bot
        .assert_screen_contains("Добавлено: Молоко · 2 л, Хлеб");
    let state: Value = serde_json::from_str(&stand.view_state().await).expect("view json");
    assert_eq!(state["t"], json!("list"), "the screen must remain the list");

    let rows: Vec<(String, Option<f64>, Option<String>)> = sqlx::query_as(
        "SELECT i.title, i.quantity::float8, i.unit
         FROM items i
         JOIN users u ON u.household_id = i.household_id
         WHERE u.telegram_user_id = $1 AND i.deleted_at IS NULL
         ORDER BY i.title",
    )
    .bind(stand.bot.chat)
    .fetch_all(&db.pool)
    .await
    .expect("items");
    assert_eq!(
        rows,
        vec![
            ("Молоко".to_owned(), Some(2.0), Some("л".to_owned())),
            ("Хлеб".to_owned(), Some(1.0), Some("шт".to_owned())),
        ]
    );

    let drafts: i64 = sqlx::query_scalar("SELECT count(*) FROM capture_drafts WHERE chat_id = $1")
        .bind(stand.bot.chat)
        .fetch_one(&db.pool)
        .await
        .expect("drafts");
    assert_eq!(drafts, 0, "purchases only must not create a draft");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_task_from_the_shopping_screen_waits_in_the_draft() {
    let db = db().await;
    let mut stand = stand(
        &db.pool,
        vec![Ok(json!({"items": [
            {"target_kind": "shopping", "title": "Молоко"},
            {"target_kind": "tasks", "title": "Позвонить маме"}
        ]}))],
    )
    .await;

    stand.bot.press("Покупки").await;
    stand.bot.text("молоко и позвонить маме").await;
    assert_eq!(stand.work().await, Pass::Drafted);

    // Покупка уже в списке, задача ждёт подтверждения в черновике.
    let saved: Vec<String> = sqlx::query_scalar(
        "SELECT i.title FROM items i
         JOIN users u ON u.household_id = i.household_id
         WHERE u.telegram_user_id = $1 AND i.deleted_at IS NULL",
    )
    .bind(stand.bot.chat)
    .fetch_all(&db.pool)
    .await
    .expect("items");
    assert_eq!(saved, vec!["Молоко".to_owned()]);

    stand.bot.assert_screen_contains("Разобрал: 1 позиция");
    stand.bot.assert_screen_contains("Позвонить маме");
    stand.bot.assert_has_button("Сохранить всё");
    stand.bot.assert_screen_contains("Добавлено: Молоко");

    let parsed: Value = sqlx::query_scalar(
        "SELECT parsed FROM capture_drafts WHERE chat_id = $1
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(stand.bot.chat)
    .fetch_one(&db.pool)
    .await
    .expect("draft");
    assert_eq!(parsed["items"].as_array().expect("items").len(), 1);
    assert_eq!(parsed["items"][0]["title"], json!("Позвонить маме"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_direct_add_limit_is_ten_parses_per_minute() {
    let db = db().await;
    let mut stand = stand(&db.pool, vec![]).await;

    stand.bot.press("Покупки").await;
    for n in 0..10 {
        stand.bot.text(&format!("покупка {n}")).await;
    }
    assert_eq!(stand.jobs().await, 10);

    stand.bot.text("одиннадцатая покупка").await;

    stand
        .bot
        .assert_screen_contains("Слишком много сообщений на разбор");
    stand.bot.assert_screen_contains("Покупки ·");
    assert_eq!(
        stand.jobs().await,
        10,
        "the eleventh job must not be queued"
    );
    assert_eq!(
        stand.ai.calls(),
        0,
        "the limit must be checked before the LLM"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_direct_result_does_not_replace_a_screen_the_user_left() {
    let db = db().await;
    let mut stand = stand(
        &db.pool,
        vec![Ok(json!({"items": [
            {"target_kind": "shopping", "title": "Хлеб"}
        ]}))],
    )
    .await;

    stand.bot.press("Покупки").await;
    stand.bot.text("хлеб").await;
    stand.bot.press("Назад").await;
    stand.bot.assert_screen_contains("Покупки · 0");

    assert_eq!(stand.work().await, Pass::Drafted);

    stand
        .bot
        .assert_screen_contains("Добавлено в «Покупки»: Хлеб");
    let state: Value = serde_json::from_str(&stand.view_state().await).expect("view json");
    assert_eq!(
        state["t"],
        json!("menu"),
        "the user's screen must not be replaced"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_second_position_rolls_back_the_first() {
    let db = support::isolated_db().await;
    let mut stand = stand(
        &db.pool,
        vec![Ok(json!({"items": [
            {"target_kind": "shopping", "title": "Молоко"},
            {"target_kind": "shopping", "title": "Хлеб"}
        ]}))],
    )
    .await;

    sqlx::query(
        "CREATE FUNCTION fail_bread() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
             IF NEW.title = 'Хлеб' THEN
                 RAISE EXCEPTION 'boom';
             END IF;
             RETURN NEW;
         END
         $$",
    )
    .execute(&db.pool)
    .await
    .expect("function");
    sqlx::query(
        "CREATE TRIGGER fail_bread BEFORE INSERT ON items
         FOR EACH ROW EXECUTE FUNCTION fail_bread()",
    )
    .execute(&db.pool)
    .await
    .expect("trigger");

    stand.bot.press("Покупки").await;
    stand.bot.text("молоко и хлеб").await;

    let result = stand.worker.run_once(stand.bot.now).await;
    assert!(
        result.is_err(),
        "the second insert must fail the transaction"
    );

    let saved: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM items i
         JOIN users u ON u.household_id = i.household_id
         WHERE u.telegram_user_id = $1 AND i.deleted_at IS NULL",
    )
    .bind(stand.bot.chat)
    .fetch_one(&db.pool)
    .await
    .expect("items");
    assert_eq!(saved, 0, "the first position must be rolled back too");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_direct_parse_keeps_the_shopping_list_with_an_error_banner() {
    let db = db().await;
    let mut stand = stand(&db.pool, vec![Err(HaError::Rejected(400))]).await;

    stand.bot.press("Покупки").await;
    stand.bot.text("молоко и хлеб").await;
    assert_eq!(stand.work().await, Pass::Dead);

    stand
        .bot
        .assert_screen_contains("Не получилось разобрать сообщение");
    let state: Value = serde_json::from_str(&stand.view_state().await).expect("view json");
    assert_eq!(state["t"], json!("list"), "the shopping list must stay");
    assert_eq!(stand.household_items().await, 0, "nothing must be created");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_duplicate_is_not_saved_and_the_banner_offers_a_choice() {
    let db = db().await;
    let mut stand = stand(
        &db.pool,
        vec![Ok(json!({"items": [
            {"target_kind": "shopping", "title": "Молоко"},
            {"target_kind": "shopping", "title": "Хлеб"}
        ]}))],
    )
    .await;
    seed_shopping_item(&db.pool, stand.bot.chat, "Молоко", 2.0, "л").await;

    stand.bot.press("Покупки").await;
    stand.bot.assert_screen_contains("Молоко · 2 л");
    stand.bot.text("молоко и хлеб").await;
    assert_eq!(stand.work().await, Pass::Drafted);

    stand.bot.assert_screen_contains("Добавлено: Хлеб");
    stand.bot.assert_has_button("Увеличить до 3 л");
    stand.bot.assert_has_button("Добавить отдельно");

    let rows: Vec<(String, Option<f64>, Option<String>)> = sqlx::query_as(
        "SELECT i.title, i.quantity::float8, i.unit
         FROM items i
         JOIN users u ON u.household_id = i.household_id
         WHERE u.telegram_user_id = $1 AND i.deleted_at IS NULL
         ORDER BY i.title",
    )
    .bind(stand.bot.chat)
    .fetch_all(&db.pool)
    .await
    .expect("items");
    assert_eq!(
        rows,
        vec![
            ("Молоко".to_owned(), Some(2.0), Some("л".to_owned())),
            ("Хлеб".to_owned(), Some(1.0), Some("шт".to_owned())),
        ],
        "the duplicate must not be saved without a choice"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn increasing_a_duplicate_updates_the_existing_position() {
    let db = db().await;
    let mut stand = stand(
        &db.pool,
        vec![Ok(json!({"items": [
            {"target_kind": "shopping", "title": "Молоко"},
            {"target_kind": "shopping", "title": "Хлеб"}
        ]}))],
    )
    .await;
    seed_shopping_item(&db.pool, stand.bot.chat, "Молоко", 2.0, "л").await;

    stand.bot.press("Покупки").await;
    stand.bot.text("молоко и хлеб").await;
    stand.work().await;
    stand.bot.press("Увеличить до 3 л").await;

    stand.bot.assert_screen_contains("Добавлено: Молоко · 3 л");
    let rows: Vec<(String, Option<f64>, Option<String>)> = sqlx::query_as(
        "SELECT i.title, i.quantity::float8, i.unit
         FROM items i
         JOIN users u ON u.household_id = i.household_id
         WHERE u.telegram_user_id = $1 AND i.deleted_at IS NULL
         ORDER BY i.title",
    )
    .bind(stand.bot.chat)
    .fetch_all(&db.pool)
    .await
    .expect("items");
    assert_eq!(
        rows,
        vec![
            ("Молоко".to_owned(), Some(3.0), Some("л".to_owned())),
            ("Хлеб".to_owned(), Some(1.0), Some("шт".to_owned())),
        ],
        "increase must change the existing position, not add another"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn adding_a_duplicate_separately_creates_a_second_position() {
    let db = db().await;
    let mut stand = stand(
        &db.pool,
        vec![Ok(json!({"items": [
            {"target_kind": "shopping", "title": "Молоко"},
            {"target_kind": "shopping", "title": "Хлеб"}
        ]}))],
    )
    .await;
    seed_shopping_item(&db.pool, stand.bot.chat, "Молоко", 2.0, "л").await;

    stand.bot.press("Покупки").await;
    stand.bot.text("молоко и хлеб").await;
    stand.work().await;
    stand.bot.press("Добавить отдельно").await;

    stand.bot.assert_screen_contains("Добавлено: Молоко · 1 шт");
    let rows: Vec<(String, Option<f64>, Option<String>)> = sqlx::query_as(
        "SELECT i.title, i.quantity::float8, i.unit
         FROM items i
         JOIN users u ON u.household_id = i.household_id
         WHERE u.telegram_user_id = $1 AND i.deleted_at IS NULL
         ORDER BY i.title, i.quantity",
    )
    .bind(stand.bot.chat)
    .fetch_all(&db.pool)
    .await
    .expect("items");
    assert_eq!(
        rows,
        vec![
            ("Молоко".to_owned(), Some(1.0), Some("шт".to_owned())),
            ("Молоко".to_owned(), Some(2.0), Some("л".to_owned())),
            ("Хлеб".to_owned(), Some(1.0), Some("шт".to_owned())),
        ],
        "separate must create a second milk position"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_version_conflict_while_increasing_does_not_crash() {
    let db = db().await;
    let mut stand = stand(
        &db.pool,
        vec![Ok(json!({"items": [
            {"target_kind": "shopping", "title": "Молоко"},
            {"target_kind": "shopping", "title": "Хлеб"}
        ]}))],
    )
    .await;
    let milk = seed_shopping_item(&db.pool, stand.bot.chat, "Молоко", 2.0, "л").await;

    stand.bot.press("Покупки").await;
    stand.bot.text("молоко и хлеб").await;
    stand.work().await;
    sqlx::query("UPDATE items SET version = version + 1 WHERE id = $1")
        .bind(milk)
        .execute(&db.pool)
        .await
        .expect("bump version");

    stand.bot.press("Увеличить до 3 л").await;

    stand
        .bot
        .assert_screen_contains("Позиция изменилась, откройте список");
    let quantity: f64 = sqlx::query_scalar("SELECT quantity::float8 FROM items WHERE id = $1")
        .bind(milk)
        .fetch_one(&db.pool)
        .await
        .expect("quantity");
    assert_eq!(quantity, 2.0, "the stale increase must not be applied");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn several_duplicates_are_offered_one_by_one() {
    let db = db().await;
    let mut stand = stand(
        &db.pool,
        vec![Ok(json!({"items": [
            {"target_kind": "shopping", "title": "Молоко"},
            {"target_kind": "shopping", "title": "Хлеб"},
            {"target_kind": "shopping", "title": "Яйца"}
        ]}))],
    )
    .await;
    seed_shopping_item(&db.pool, stand.bot.chat, "Молоко", 2.0, "л").await;
    seed_shopping_item(&db.pool, stand.bot.chat, "Яйца", 10.0, "шт").await;

    stand.bot.press("Покупки").await;
    stand.bot.text("молоко, хлеб и яйца").await;
    assert_eq!(stand.work().await, Pass::Drafted);

    stand.bot.assert_screen_contains("Добавлено: Хлеб");
    stand.bot.assert_screen_contains("Уже в списке: «Молоко»");
    stand.bot.assert_has_button("Увеличить до 3 л");
    assert!(
        stand.bot.find_button("Увеличить до 11 шт").is_none(),
        "only the first duplicate must be offered:\n{}",
        stand.bot.screen()
    );

    stand.bot.press("Увеличить до 3 л").await;
    stand.bot.assert_screen_contains("Добавлено: Молоко · 3 л");
    stand.bot.assert_screen_contains("Уже в списке: «Яйца»");
    stand.bot.assert_has_button("Увеличить до 11 шт");

    stand.bot.press("Увеличить до 11 шт").await;
    stand.bot.assert_screen_contains("Добавлено: Яйца · 11 шт");

    let rows: Vec<(String, Option<f64>)> = sqlx::query_as(
        "SELECT i.title, i.quantity::float8
         FROM items i
         JOIN users u ON u.household_id = i.household_id
         WHERE u.telegram_user_id = $1 AND i.deleted_at IS NULL
         ORDER BY i.title",
    )
    .bind(stand.bot.chat)
    .fetch_all(&db.pool)
    .await
    .expect("items");
    assert_eq!(
        rows,
        vec![
            ("Молоко".to_owned(), Some(3.0)),
            ("Хлеб".to_owned(), Some(1.0)),
            ("Яйца".to_owned(), Some(11.0)),
        ],
        "both duplicates must be resolved without losing either"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_duplicate_choice_keeps_the_waiting_draft_card() {
    let db = db().await;
    let mut stand = stand(
        &db.pool,
        vec![Ok(json!({"items": [
            {"target_kind": "shopping", "title": "Молоко"},
            {"target_kind": "tasks", "title": "Позвонить маме"}
        ]}))],
    )
    .await;
    seed_shopping_item(&db.pool, stand.bot.chat, "Молоко", 2.0, "л").await;

    stand.bot.press("Покупки").await;
    stand.bot.text("молоко и позвонить маме").await;
    assert_eq!(stand.work().await, Pass::Drafted);
    stand.bot.assert_screen_contains("Позвонить маме");
    stand.bot.assert_screen_contains("Уже в списке: «Молоко»");

    stand.bot.press("Увеличить до 3 л").await;

    stand.bot.assert_screen_contains("Добавлено: Молоко · 3 л");
    stand.bot.assert_screen_contains("Позвонить маме");
    stand.bot.assert_has_button("Сохранить всё");
    let state: Value = serde_json::from_str(&stand.view_state().await).expect("view json");
    assert_eq!(
        state["t"],
        json!("draft"),
        "a duplicate choice must not replace the waiting draft card"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeated_duplicates_in_one_message_are_merged() {
    let db = db().await;
    let mut stand = stand(
        &db.pool,
        vec![Ok(json!({"items": [
            {"target_kind": "shopping", "title": "Молоко"},
            {"target_kind": "shopping", "title": "Молоко"}
        ]}))],
    )
    .await;
    let milk = seed_shopping_item(&db.pool, stand.bot.chat, "Молоко", 2.0, "л").await;

    stand.bot.press("Покупки").await;
    stand.bot.text("молоко и молоко").await;
    assert_eq!(stand.work().await, Pass::Drafted);

    stand.bot.assert_has_button("Увеличить до 4 л");
    assert!(
        stand.bot.find_button("Увеличить до 3 л").is_none(),
        "two mentions of the same item must be one choice:\n{}",
        stand.bot.screen()
    );
    stand.bot.press("Увеличить до 4 л").await;

    let quantity: f64 = sqlx::query_scalar("SELECT quantity::float8 FROM items WHERE id = $1")
        .bind(milk)
        .fetch_one(&db.pool)
        .await
        .expect("quantity");
    assert_eq!(quantity, 4.0);
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM items i
         JOIN users u ON u.household_id = i.household_id
         WHERE u.telegram_user_id = $1 AND i.title = 'Молоко' AND i.deleted_at IS NULL",
    )
    .bind(stand.bot.chat)
    .fetch_one(&db.pool)
    .await
    .expect("count");
    assert_eq!(count, 1);
}
