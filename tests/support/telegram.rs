//! Поддержка тестов Telegram: поддельный Bot API и сценарный стенд.

#![allow(dead_code)]

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration as StdDuration, Instant};

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use serde_json::{Value, json};
use sqlx::PgPool;
use tasklerq::telegram::api::{RichMessage, TelegramApi, TelegramError, WebhookInfo};
use tasklerq::telegram::bot::Bot;
use tasklerq::telegram::outbox::OutboxWorker;
use tasklerq::telegram::queue;
use tasklerq::telegram::worker::{Pass, UpdateHandler, UpdateWorker};
use tokio::sync::Notify;

/// Вызов Bot API, записанный подделкой.
#[derive(Debug, Clone, PartialEq)]
pub enum Call {
    SendRich { chat: i64, id: i64, html: String },
    EditRich { chat: i64, id: i64, html: String },
    Send { chat: i64, id: i64, html: String },
    EditText { chat: i64, id: i64, html: String },
    Delete { chat: i64, id: i64 },
}

#[derive(Default)]
struct State {
    next_id: i64,
    messages: HashMap<(i64, i64), String>,
    calls: Vec<Call>,
    failures: VecDeque<TelegramError>,
    edit_error: Option<TelegramError>,
    answers: Vec<(String, Option<String>, bool)>,
}

/// Поддельный Bot API: хранит сообщения и записывает вызовы.
#[derive(Default)]
pub struct FakeTelegram {
    state: Mutex<State>,
}

impl FakeTelegram {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Следующий вызов отправки или правки завершится этой ошибкой.
    pub fn fail_next(&self, error: TelegramError) {
        self.state.lock().expect("lock").failures.push_back(error);
    }

    /// Все правки будут отклоняться этой ошибкой (`None` — снять).
    pub fn reject_edits(&self, error: Option<TelegramError>) {
        self.state.lock().expect("lock").edit_error = error;
    }

    pub fn calls(&self) -> Vec<Call> {
        self.state.lock().expect("lock").calls.clone()
    }

    pub fn clear_calls(&self) {
        self.state.lock().expect("lock").calls.clear();
    }

    /// Сообщения, отправленные новыми (не правками).
    pub fn sent(&self) -> Vec<Call> {
        self.calls()
            .into_iter()
            .filter(|call| matches!(call, Call::SendRich { .. } | Call::Send { .. }))
            .collect()
    }

    /// Живые сообщения чата по возрастанию идентификатора.
    pub fn messages(&self, chat: i64) -> Vec<(i64, String)> {
        let state = self.state.lock().expect("lock");
        let mut messages: Vec<(i64, String)> = state
            .messages
            .iter()
            .filter(|((c, _), _)| *c == chat)
            .map(|((_, id), html)| (*id, html.clone()))
            .collect();
        messages.sort_by_key(|(id, _)| *id);
        messages
    }

    pub fn answers(&self) -> Vec<(String, Option<String>, bool)> {
        self.state.lock().expect("lock").answers.clone()
    }

    fn take_failure(state: &mut State) -> Option<TelegramError> {
        state.failures.pop_front()
    }
}

#[async_trait]
impl TelegramApi for FakeTelegram {
    async fn send_rich_message(
        &self,
        chat_id: i64,
        message: &RichMessage,
    ) -> Result<i64, TelegramError> {
        let mut state = self.state.lock().expect("lock");
        if let Some(error) = Self::take_failure(&mut state) {
            return Err(error);
        }
        state.next_id += 1;
        let id = state.next_id;
        state.messages.insert((chat_id, id), message.html.clone());
        state.calls.push(Call::SendRich {
            chat: chat_id,
            id,
            html: message.html.clone(),
        });
        Ok(id)
    }

    async fn edit_rich_message(
        &self,
        chat_id: i64,
        message_id: i64,
        message: &RichMessage,
    ) -> Result<(), TelegramError> {
        let mut state = self.state.lock().expect("lock");
        if let Some(error) = Self::take_failure(&mut state) {
            return Err(error);
        }
        if let Some(error) = state.edit_error.clone() {
            return Err(error);
        }
        if !state.messages.contains_key(&(chat_id, message_id)) {
            return Err(TelegramError::MessageGone);
        }
        if state.messages.get(&(chat_id, message_id)) == Some(&message.html) {
            return Err(TelegramError::NotModified);
        }
        state
            .messages
            .insert((chat_id, message_id), message.html.clone());
        state.calls.push(Call::EditRich {
            chat: chat_id,
            id: message_id,
            html: message.html.clone(),
        });
        Ok(())
    }

    async fn send_message(&self, chat_id: i64, html: &str) -> Result<i64, TelegramError> {
        let mut state = self.state.lock().expect("lock");
        if let Some(error) = Self::take_failure(&mut state) {
            return Err(error);
        }
        state.next_id += 1;
        let id = state.next_id;
        state.messages.insert((chat_id, id), html.to_owned());
        state.calls.push(Call::Send {
            chat: chat_id,
            id,
            html: html.to_owned(),
        });
        Ok(id)
    }

    async fn edit_message_text(
        &self,
        chat_id: i64,
        message_id: i64,
        html: &str,
    ) -> Result<(), TelegramError> {
        let mut state = self.state.lock().expect("lock");
        if let Some(error) = Self::take_failure(&mut state) {
            return Err(error);
        }
        state
            .messages
            .insert((chat_id, message_id), html.to_owned());
        state.calls.push(Call::EditText {
            chat: chat_id,
            id: message_id,
            html: html.to_owned(),
        });
        Ok(())
    }

    async fn answer_callback_query(
        &self,
        callback_query_id: &str,
        text: Option<&str>,
        show_alert: bool,
    ) -> Result<(), TelegramError> {
        self.state.lock().expect("lock").answers.push((
            callback_query_id.to_owned(),
            text.map(str::to_owned),
            show_alert,
        ));
        Ok(())
    }

    async fn delete_message(&self, chat_id: i64, message_id: i64) -> Result<(), TelegramError> {
        let mut state = self.state.lock().expect("lock");
        state.messages.remove(&(chat_id, message_id));
        state.calls.push(Call::Delete {
            chat: chat_id,
            id: message_id,
        });
        Ok(())
    }

    async fn set_webhook(&self, _url: &str, _secret_token: &str) -> Result<(), TelegramError> {
        Ok(())
    }

    async fn get_webhook_info(&self) -> Result<WebhookInfo, TelegramError> {
        Ok(WebhookInfo {
            url: String::new(),
            pending_update_count: 0,
            last_error_message: None,
        })
    }

    async fn get_updates(&self, _offset: i64, _timeout: u32) -> Result<Vec<Value>, TelegramError> {
        Ok(Vec::new())
    }
}

/// Разобрать кнопки сообщения: `(подпись, callback_data)`.
pub fn buttons(html: &str) -> Vec<(String, String)> {
    let mut found = Vec::new();
    let marker = "<tg-button type=\"callback_data\" data=\"";
    let mut rest = html;
    while let Some(start) = rest.find(marker) {
        let after = &rest[start + marker.len()..];
        let Some(quote) = after.find('"') else { break };
        let data = after[..quote].to_owned();
        let Some(open) = after.find("\">") else { break };
        let label_start = open + 2;
        let Some(close) = after[label_start..].find("</tg-button>") else {
            break;
        };
        let label = unescape(&after[label_start..label_start + close]);
        found.push((label, data));
        rest = &after[label_start + close..];
    }
    found
}

pub fn unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&amp;", "&")
}

/// Сценарный стенд: один пользователь, настоящий бот и поддельный Telegram.
pub struct Harness {
    pub pool: PgPool,
    pub fake: Arc<FakeTelegram>,
    pub chat: i64,
    pub now: DateTime<Utc>,
    skew: Duration,
    pub name: String,
    screen_id: Option<i64>,
    updates: UpdateWorker,
    outbox: OutboxWorker,
    clock: Instant,
    next_update: i64,
}

static NEXT_UPDATE: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(1_000);

impl Harness {
    pub fn new(pool: &PgPool, telegram_id: i64) -> Self {
        Self::with_handler(
            pool,
            telegram_id,
            Arc::new(Bot::new("tasklerqbot")),
            FakeTelegram::new(),
        )
    }

    pub fn sharing(pool: &PgPool, telegram_id: i64, fake: Arc<FakeTelegram>) -> Self {
        Self::with_handler(pool, telegram_id, Arc::new(Bot::new("tasklerqbot")), fake)
    }

    pub fn with_handler(
        pool: &PgPool,
        telegram_id: i64,
        handler: Arc<dyn UpdateHandler>,
        fake: Arc<FakeTelegram>,
    ) -> Self {
        let wake = Arc::new(Notify::new());
        Self {
            pool: pool.clone(),
            fake: fake.clone(),
            chat: telegram_id,
            now: Utc::now(),
            skew: Duration::zero(),
            name: "Alice".to_owned(),
            screen_id: None,
            updates: UpdateWorker::new(pool.clone(), handler, fake.clone(), 10, wake)
                .only_chat(telegram_id),
            outbox: OutboxWorker::new(pool.clone(), fake).only_chat(telegram_id),
            clock: Instant::now(),
            next_update: NEXT_UPDATE.fetch_add(10_000, std::sync::atomic::Ordering::Relaxed),
        }
    }

    #[must_use]
    pub fn named(mut self, name: &str) -> Self {
        self.name = name.to_owned();
        self
    }

    /// Сдвинуть время стенда вперёд относительно реального.
    pub fn advance(&mut self, by: Duration) {
        self.skew += by;
        self.tick();
    }

    /// Время стенда следует за реальным плюс накопленный сдвиг.
    fn tick(&mut self) {
        self.now = Utc::now() + self.skew;
    }

    fn update_id(&mut self) -> i64 {
        self.next_update += 1;
        self.next_update
    }

    pub async fn enqueue(&mut self, payload: Value) {
        self.tick();
        let id = payload["update_id"].as_i64().expect("update id");
        let mut conn = self.pool.acquire().await.expect("connection");
        queue::enqueue(&mut conn, id, &payload, self.now)
            .await
            .expect("enqueue");
    }

    pub fn text_update(&mut self, name: &str, text: &str) -> Value {
        let id = self.update_id();
        json!({
            "update_id": id,
            "message": {
                "message_id": id,
                "date": 1_700_000_000,
                "chat": {"id": self.chat, "type": "private", "first_name": name},
                "from": {"id": self.chat, "is_bot": false, "first_name": name},
                "text": text
            }
        })
    }

    pub fn callback_update(&mut self, name: &str, data: &str, message_id: i64) -> Value {
        let id = self.update_id();
        json!({
            "update_id": id,
            "callback_query": {
                "id": format!("cb{id}"),
                "chat_instance": "x",
                "from": {"id": self.chat, "is_bot": false, "first_name": name},
                "data": data,
                "message": {
                    "message_id": message_id,
                    "date": 1_700_000_000,
                    "chat": {"id": self.chat, "type": "private", "first_name": name},
                    "text": "экран"
                }
            }
        })
    }

    /// Обработать очередь обновлений и доставить исходящие.
    pub async fn pump(&mut self) {
        self.tick();
        for _ in 0..50 {
            match self.updates.run_once(self.now).await.expect("update pass") {
                Pass::Processed => {}
                Pass::Idle | Pass::Failed(_) => break,
            }
        }
        self.deliver().await;
    }

    /// Доставить исходящие, пока очередь не опустеет.
    pub async fn deliver(&mut self) {
        self.tick();
        for _ in 0..30 {
            self.clock += StdDuration::from_secs(2);
            let sent = self
                .outbox
                .run_once(self.now, self.clock)
                .await
                .expect("outbox pass");
            let pending: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM outbox WHERE chat_id = $1 AND status = 'pending' AND next_attempt_at <= $2",
            )
            .bind(self.chat)
            .bind(self.now)
            .fetch_one(&self.pool)
            .await
            .expect("pending");
            if sent == 0 && pending == 0 {
                break;
            }
        }
        self.screen_id =
            sqlx::query_scalar("SELECT message_id FROM telegram_views WHERE chat_id = $1")
                .bind(self.chat)
                .fetch_optional(&self.pool)
                .await
                .expect("view")
                .flatten();
    }

    pub async fn text(&mut self, text: &str) {
        let name = self.name.clone();
        let update = self.text_update(&name, text);
        self.enqueue(update).await;
        self.pump().await;
    }

    /// Последнее живое сообщение чата (например, уведомление).
    pub fn latest(&self) -> (i64, String) {
        self.fake
            .messages(self.chat)
            .pop()
            .expect("chat should have a message")
    }

    /// Активное сообщение экрана навигации.
    pub fn active(&self) -> (i64, String) {
        let id = self.screen_id.expect("a screen should have been delivered");
        self.fake
            .messages(self.chat)
            .into_iter()
            .find(|(message_id, _)| *message_id == id)
            .unwrap_or_else(|| panic!("screen message {id} is not alive"))
    }

    pub fn assert_latest_contains(&self, needle: &str) {
        let html = self.latest().1;
        assert!(
            html.contains(needle),
            "latest message should contain `{needle}`:\n{html}"
        );
    }

    pub fn screen(&self) -> String {
        self.active().1
    }

    pub fn buttons(&self) -> Vec<(String, String)> {
        buttons(&self.screen())
    }

    pub fn find_button(&self, label: &str) -> Option<String> {
        self.buttons()
            .into_iter()
            .find(|(text, _)| text.contains(label))
            .map(|(_, data)| data)
    }

    /// Нажать кнопку активного экрана по части подписи.
    pub async fn press(&mut self, label: &str) {
        let (message_id, html) = self.active();
        let data = buttons(&html)
            .into_iter()
            .find(|(text, _)| text.contains(label))
            .unwrap_or_else(|| panic!("button `{label}` not found on screen:\n{html}"))
            .1;
        self.press_data(&data, message_id).await;
    }

    pub async fn press_data(&mut self, data: &str, message_id: i64) {
        let name = self.name.clone();
        let update = self.callback_update(&name, data, message_id);
        self.enqueue(update).await;
        self.pump().await;
    }

    pub fn assert_screen_contains(&self, needle: &str) {
        let screen = self.screen();
        assert!(
            screen.contains(needle),
            "screen should contain `{needle}`:\n{screen}"
        );
    }

    pub fn assert_has_button(&self, label: &str) {
        assert!(
            self.find_button(label).is_some(),
            "button `{label}` expected on screen:\n{}",
            self.screen()
        );
    }
}
