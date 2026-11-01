//! Воркер входящих обновлений.
//!
//! Обновление, его доменные эффекты и записи в `outbox` фиксируются одной
//! транзакцией: либо действие применено и экран поставлен в очередь, либо
//! ничего не изменилось и обновление будет повторено с задержкой. Telegram API
//! воркер вызывает только для быстрого ответа на callback — после коммита.

use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures_util::FutureExt;
use sqlx::PgPool;
use tokio::sync::{Notify, watch};

use crate::metrics;
use crate::rate_limit::RateLimiter;

use super::api::TelegramApi;
use super::bot::{Answer, Bot, Outcome};
use super::outbox::{self, Outgoing};
use super::queue::{self, Failure};
use super::rich;
use super::update::{self, Classified, Incoming};

/// Сколько обновлений пользователя обрабатывается за минуту.
pub const UPDATES_PER_MINUTE: u32 = 20;
/// Окно лимита обновлений, секунды.
const UPDATE_WINDOW_SECONDS: i64 = 60;

/// Итог одного прохода воркера.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pass {
    /// Очередь пуста.
    Idle,
    /// Обновление обработано.
    Processed,
    /// Обработка не удалась; повтор или `dead` записаны в очередь.
    Failed(Failure),
}

/// Обработчик обновления: граница, через которую тесты подменяют бота.
#[async_trait]
pub trait UpdateHandler: Send + Sync {
    /// Обработать сырое обновление в транзакции воркера.
    async fn handle(
        &self,
        conn: &mut sqlx::PgConnection,
        payload: &serde_json::Value,
        now: DateTime<Utc>,
    ) -> Result<Outcome, sqlx::Error>;
}

#[async_trait]
impl UpdateHandler for Bot {
    async fn handle(
        &self,
        conn: &mut sqlx::PgConnection,
        payload: &serde_json::Value,
        now: DateTime<Utc>,
    ) -> Result<Outcome, sqlx::Error> {
        let Ok(parsed) = update::parse(payload) else {
            // Такое обновление не прошло бы webhook; повтор ничего не изменит.
            tracing::warn!("stored telegram update is malformed; skipping");
            return Ok(Outcome::default());
        };
        match update::classify(&parsed) {
            Classified::Ignore => Ok(Outcome::default()),
            Classified::Handle(incoming) => Bot::handle(self, conn, incoming, now).await,
        }
    }
}

/// Воркер очереди `telegram_updates`.
pub struct UpdateWorker {
    pool: PgPool,
    handler: Arc<dyn UpdateHandler>,
    api: Arc<dyn TelegramApi>,
    max_attempts: u32,
    wake_outbox: Arc<Notify>,
    only_chat: Option<i64>,
    rate: Option<Arc<RateLimiter<i64>>>,
    notices: Option<Arc<RateLimiter<i64>>>,
}

impl UpdateWorker {
    /// Создать воркер. `wake_outbox` будит доставку после коммита.
    pub fn new(
        pool: PgPool,
        handler: Arc<dyn UpdateHandler>,
        api: Arc<dyn TelegramApi>,
        max_attempts: u32,
        wake_outbox: Arc<Notify>,
    ) -> Self {
        Self {
            pool,
            handler,
            api,
            max_attempts,
            wake_outbox,
            only_chat: None,
            rate: None,
            notices: None,
        }
    }

    /// Обрабатывать только обновления одного чата (для тестов).
    #[must_use]
    pub fn only_chat(mut self, chat_id: i64) -> Self {
        self.only_chat = Some(chat_id);
        self
    }

    /// Ограничить обработку обновлений пользователя `per_minute` в минуту.
    ///
    /// Сообщение о превышении в чат отправляется не чаще раза за окно; ответ
    /// на callback уходит на каждое превышение, чтобы убрать «часики».
    #[must_use]
    pub fn with_rate_limit(mut self, per_minute: u32) -> Self {
        self.rate = Some(Arc::new(RateLimiter::with_window(
            per_minute,
            UPDATE_WINDOW_SECONDS,
        )));
        self.notices = Some(Arc::new(RateLimiter::with_window(1, UPDATE_WINDOW_SECONDS)));
        self
    }

    /// Обработать одно готовое обновление.
    pub async fn run_once(&self, now: DateTime<Utc>) -> Result<Pass, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let Some(job) = queue::claim_next(&mut tx, now, self.only_chat).await? else {
            return Ok(Pass::Idle);
        };
        let started = Instant::now();

        // Лимит проверяется до обработчика: обновление сверх лимита не должно
        // создавать или менять данные. Оно всё равно помечается обработанным,
        // чтобы Telegram не повторял доставку.
        //
        // Слот расходуется только на первой попытке: повтор уже учтённого
        // обновления не должен отнимать лимит у новых действий пользователя.
        if job.attempts == 0
            && let Some(outcome) = self.rate_limited(&mut tx, &job, now).await?
        {
            queue::mark_processed(&mut tx, job.update_id, now).await?;
            tx.commit().await?;
            self.wake_outbox.notify_one();
            self.answer(outcome).await;
            metrics::telegram_update("rate_limited");
            log_update(job.update_id, "rate_limited", started);
            return Ok(Pass::Processed);
        }

        // Паника обработчика считается неудачной попыткой, как и ошибка базы:
        // иначе «ядовитое» обновление роняло бы воркер при каждом перезапуске
        // и никогда не попало бы в dead-letter.
        let handled = AssertUnwindSafe(self.handler.handle(&mut tx, &job.payload, now))
            .catch_unwind()
            .await;
        let error_code = match handled {
            Ok(Ok(outcome)) => {
                queue::mark_processed(&mut tx, job.update_id, now).await?;
                tx.commit().await?;
                self.wake_outbox.notify_one();
                self.answer(outcome).await;
                metrics::telegram_update("processed");
                log_update(job.update_id, "processed", started);
                return Ok(Pass::Processed);
            }
            Ok(Err(error)) => {
                tracing::warn!(update_id = job.update_id, %error, "telegram update failed");
                "database"
            }
            Err(_) => {
                tracing::error!(
                    update_id = job.update_id,
                    "telegram update handler panicked"
                );
                "panic"
            }
        };

        // Сбой отката освободит строку вместе с соединением: попытку это не отменяет.
        if let Err(error) = tx.rollback().await {
            tracing::warn!(update_id = job.update_id, %error, "rollback failed");
        }
        let mut conn = self.pool.acquire().await?;
        let failure =
            queue::record_failure(&mut conn, job.update_id, error_code, self.max_attempts, now)
                .await?;
        if failure == Failure::Dead {
            tracing::error!(
                update_id = job.update_id,
                metric = "telegram_updates_dead",
                "telegram update moved to the dead-letter queue"
            );
        }
        let status = match failure {
            Failure::Dead => "dead",
            Failure::Retry(_) => "failed",
        };
        metrics::telegram_update(status);
        log_update(job.update_id, status, started);
        Ok(Pass::Failed(failure))
    }

    /// Проверить лимит пользователя. `Some` — обновление пропущено, в нём
    /// только ответ на callback; данные не изменены.
    async fn rate_limited(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        job: &queue::Claimed,
        now: DateTime<Utc>,
    ) -> Result<Option<Outcome>, sqlx::Error> {
        let Some(rate) = &self.rate else {
            return Ok(None);
        };
        let Ok(parsed) = update::parse(&job.payload) else {
            return Ok(None);
        };
        let Classified::Handle(incoming) = update::classify(&parsed) else {
            return Ok(None);
        };
        let user_id = match &incoming {
            Incoming::Text { sender, .. } | Incoming::Callback { sender, .. } => sender.id,
        };
        let decision = rate.check(user_id, now);
        if decision.allowed {
            return Ok(None);
        }

        let retry_after = decision
            .retry_after
            .unwrap_or_else(|| u64::try_from(rate.window_seconds()).unwrap_or(60));
        tracing::warn!(
            limit = rate.limit(),
            user_id,
            retry_after,
            "telegram user is rate limited"
        );

        let answer = match incoming {
            Incoming::Callback { query_id, .. } => Some(Answer {
                query_id,
                text: Some(rate_limited_text(retry_after)),
                alert: false,
            }),
            Incoming::Text {
                chat_id, sender, ..
            } => {
                let notify = self
                    .notices
                    .as_ref()
                    .is_some_and(|gate| gate.check(sender.id, now).allowed);
                if notify {
                    outbox::enqueue(
                        tx,
                        chat_id,
                        &Outgoing::Message {
                            html: rich::escape(&rate_limited_text(retry_after)),
                            rich: false,
                        },
                        None,
                        now,
                    )
                    .await?;
                }
                None
            }
        };
        Ok(Some(Outcome { answer }))
    }

    async fn answer(&self, outcome: Outcome) {
        let Some(answer) = outcome.answer else {
            return;
        };
        // Ответ на callback нужен только чтобы убрать «часики»; повторять его
        // бессмысленно, поэтому ошибка лишь пишется в журнал.
        if let Err(error) = self
            .api
            .answer_callback_query(&answer.query_id, answer.text.as_deref(), answer.alert)
            .await
        {
            tracing::debug!(error = error.code(), "answerCallbackQuery failed");
        }
    }

    /// Цикл воркера до сигнала остановки.
    pub async fn run(self, wake: Arc<Notify>, mut shutdown: watch::Receiver<bool>) {
        loop {
            if *shutdown.borrow() {
                return;
            }
            // Паника вне обработчика (в самой очереди) не должна останавливать бота.
            match AssertUnwindSafe(self.run_once(Utc::now()))
                .catch_unwind()
                .await
            {
                Ok(Ok(Pass::Processed)) => continue,
                Ok(Ok(Pass::Idle | Pass::Failed(_))) => {}
                Ok(Err(error)) => tracing::error!(%error, "telegram update pass failed"),
                Err(_) => tracing::error!("telegram update pass panicked"),
            }
            tokio::select! {
                _ = shutdown.changed() => {}
                () = wake.notified() => {}
                () = tokio::time::sleep(Duration::from_millis(500)) => {}
            }
        }
    }
}

/// Сообщение пользователю о превышении лимита; без пользовательского текста.
fn rate_limited_text(retry_after: u64) -> String {
    format!("Слишком часто. Попробуйте через {retry_after} с.")
}

/// Строка журнала об исходе обновления: только идентификатор, статус и время.
fn log_update(update_id: i64, status: &'static str, started: Instant) {
    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    tracing::info!(update_id, status, duration_ms, "telegram update finished");
}

/// Long polling для локальной разработки: те же обновления попадают в ту же
/// очередь, что и от webhook.
pub async fn poll(
    pool: PgPool,
    api: Arc<dyn TelegramApi>,
    wake: Arc<Notify>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut offset = 0_i64;
    loop {
        if *shutdown.borrow() {
            return;
        }
        let batch = tokio::select! {
            _ = shutdown.changed() => return,
            batch = api.get_updates(offset, 25) => batch,
        };
        let updates = match batch {
            Ok(updates) => updates,
            Err(error) => {
                tracing::warn!(error = error.code(), "getUpdates failed");
                tokio::time::sleep(Duration::from_secs(3)).await;
                continue;
            }
        };
        for raw in updates {
            let Some(update_id) = raw.get("update_id").and_then(serde_json::Value::as_i64) else {
                continue;
            };
            let handled = update::parse(&raw)
                .map(|parsed| matches!(update::classify(&parsed), Classified::Handle(_)))
                .unwrap_or(false);
            if handled {
                let stored = match pool.acquire().await {
                    Ok(mut conn) => queue::enqueue(&mut conn, update_id, &raw, Utc::now()).await,
                    Err(error) => Err(error),
                };
                if let Err(error) = stored {
                    // Offset не сдвинут: Telegram отдаст это обновление снова.
                    tracing::error!(%error, "failed to enqueue polled update");
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    break;
                }
                wake.notify_one();
            }
            offset = offset.max(update_id + 1);
        }
    }
}
