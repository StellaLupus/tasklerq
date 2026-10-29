//! Воркер входящих обновлений.
//!
//! Обновление, его доменные эффекты и записи в `outbox` фиксируются одной
//! транзакцией: либо действие применено и экран поставлен в очередь, либо
//! ничего не изменилось и обновление будет повторено с задержкой. Telegram API
//! воркер вызывает только для быстрого ответа на callback — после коммита.

use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures_util::FutureExt;
use sqlx::PgPool;
use tokio::sync::{Notify, watch};

use super::api::TelegramApi;
use super::bot::{Bot, Outcome};
use super::queue::{self, Failure};
use super::update::{self, Classified};

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
        }
    }

    /// Обрабатывать только обновления одного чата (для тестов).
    #[must_use]
    pub fn only_chat(mut self, chat_id: i64) -> Self {
        self.only_chat = Some(chat_id);
        self
    }

    /// Обработать одно готовое обновление.
    pub async fn run_once(&self, now: DateTime<Utc>) -> Result<Pass, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let Some(job) = queue::claim_next(&mut tx, now, self.only_chat).await? else {
            return Ok(Pass::Idle);
        };

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
        Ok(Pass::Failed(failure))
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
