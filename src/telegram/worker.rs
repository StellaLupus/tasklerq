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
use crate::rate_limit::{RateLimiter, retry_after_seconds};
use crate::security_audit::{Audit, LimitKey, RateLimitEvent};

use super::api::TelegramApi;
use super::bot::{Answer, Bot, Outcome};
use super::queue::{self, Failure};
use super::update::{self, Classified, Incoming};
use super::views;

/// Сколько обновлений пользователя обрабатывается за минуту.
pub const UPDATES_PER_MINUTE: u32 = 20;
/// Сколько обновлений пользователя может ждать освобождения слота.
pub const DEFERRED_QUEUE_LIMIT: i64 = 20;
/// Окно лимита обновлений, секунды.
const UPDATE_WINDOW_SECONDS: i64 = 60;

/// Итог одного прохода воркера.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pass {
    /// Очередь пуста.
    Idle,
    /// Обновление обработано.
    Processed,
    /// Обновление отложено до освобождения слота лимитера.
    Deferred,
    /// Отложенная очередь переполнена: обновление отброшено без выполнения.
    Dropped,
    /// Обработка не удалась; повтор или `dead` записаны в очередь.
    Failed(Failure),
}

/// Отложенное обновление, событие о сработавшем лимите и ответ на callback.
struct Limited {
    retry_at: DateTime<Utc>,
    outcome: Outcome,
    event: RateLimitEvent,
    /// Чат, в котором откладывается обновление.
    chat_id: i64,
    /// Это текст: при отбрасывании его сообщение остаётся в чате.
    text: bool,
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
    audit: Audit,
}

impl UpdateWorker {
    /// Создать воркер. `wake_outbox` будит доставку после коммита, `audit` —
    /// общий журнал событий процесса.
    pub fn new(
        pool: PgPool,
        handler: Arc<dyn UpdateHandler>,
        api: Arc<dyn TelegramApi>,
        max_attempts: u32,
        wake_outbox: Arc<Notify>,
        audit: Audit,
    ) -> Self {
        Self {
            audit,
            pool,
            handler,
            api,
            max_attempts,
            wake_outbox,
            only_chat: None,
            rate: None,
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
    /// Обновление сверх лимита остаётся `pending` до освобождения слота и затем
    /// выполняется как обычно. Ответ на callback уходит сразу, чтобы убрать
    /// «часики».
    #[must_use]
    pub fn with_rate_limit(mut self, per_minute: u32) -> Self {
        self.rate = Some(Arc::new(RateLimiter::with_window(
            per_minute,
            UPDATE_WINDOW_SECONDS,
        )));
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
        // создавать или менять данные. Оно остаётся `pending` до точного момента
        // освобождения слота и затем выполняется как обычно; строка `message` в
        // чат не ставится.
        //
        // Слот расходуется только на первой попытке: повтор уже учтённого
        // обновления не должен отнимать лимит у новых действий пользователя.
        if job.attempts == 0
            && let Some(limited) = self.rate_limited(&job, now).await?
        {
            // Без предела флудящий пользователь заставлял бы единственный воркер
            // заново проверять все отложенные обновления на каждом слоте.
            if queue::deferred_count(&mut tx, limited.chat_id, now).await? >= DEFERRED_QUEUE_LIMIT {
                queue::mark_processed(&mut tx, job.update_id, now).await?;
                if limited.text {
                    // Обработчик не увидит текст, поэтому сообщение остаётся в
                    // чате; следующий экран придёт новым сообщением ниже.
                    views::mark_buried(&mut tx, limited.chat_id).await?;
                }
                tx.commit().await?;
                tracing::warn!(
                    update_id = job.update_id,
                    chat_id = limited.chat_id,
                    limit = DEFERRED_QUEUE_LIMIT,
                    "deferred telegram queue is full; dropping update"
                );
                // Событие пишется после коммита отдельным соединением: отказ не
                // должен исчезнуть вместе с транзакцией ответа.
                self.audit.rate_limited(limited.event, now).await;
                metrics::telegram_update("dropped");
                log_update(job.update_id, "dropped", started);
                return Ok(Pass::Dropped);
            }
            queue::defer(&mut tx, job.update_id, limited.retry_at).await?;
            tx.commit().await?;
            // Событие пишется после коммита отдельным соединением: отказ не
            // должен исчезнуть вместе с транзакцией ответа.
            self.audit.rate_limited(limited.event, now).await;
            self.answer(limited.outcome).await;
            metrics::telegram_update("deferred");
            log_update(job.update_id, "deferred", started);
            return Ok(Pass::Deferred);
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

    /// Проверить лимит пользователя. `Some` — обновление отложено до
    /// `retry_at`; данные не изменены, на callback готов пустой ответ.
    async fn rate_limited(
        &self,
        job: &queue::Claimed,
        now: DateTime<Utc>,
    ) -> Result<Option<Limited>, sqlx::Error> {
        let Some(rate) = &self.rate else {
            return Ok(None);
        };
        let Ok(parsed) = update::parse(&job.payload) else {
            return Ok(None);
        };
        let Classified::Handle(incoming) = update::classify(&parsed) else {
            return Ok(None);
        };
        let (user_id, chat_id, text) = match &incoming {
            Incoming::Text {
                chat_id, sender, ..
            } => (sender.id, *chat_id, true),
            Incoming::Callback {
                chat_id, sender, ..
            } => (sender.id, *chat_id, false),
        };
        let decision = rate.check(user_id, now);
        if decision.allowed {
            return Ok(None);
        }
        // Отказ всегда имеет момент освобождения слота. Если лимитер его не
        // вернул, считаем ожиданием полного окна: обновление не должно тихо
        // пройти лимит.
        let retry_at = decision
            .retry_at
            .unwrap_or_else(|| now + chrono::Duration::seconds(rate.window_seconds()));
        let retry_after = retry_after_seconds(retry_at - now);
        tracing::warn!(
            limit = rate.limit(),
            user_id,
            retry_after,
            "telegram user is rate limited"
        );

        // Ответ на callback нужен сразу, чтобы убрать «часики»: после задержки
        // Telegram его уже не примет. Повторное откладывание того же нажатия
        // второй раз не отвечает. Действие выполнится позже, когда слот
        // освободится.
        let answer = match incoming {
            Incoming::Callback { query_id, .. } if !job.deferred => Some(Answer {
                query_id,
                text: None,
                alert: false,
            }),
            Incoming::Callback { .. } | Incoming::Text { .. } => None,
        };
        Ok(Some(Limited {
            retry_at,
            outcome: Outcome { answer },
            event: RateLimitEvent {
                limit: crate::security_audit::TELEGRAM_LIMIT,
                key: LimitKey::User(user_id),
                retry_after,
                household_id: None,
                actor: crate::domain::models::Actor::system(
                    crate::domain::models::ActorType::System,
                ),
            },
            chat_id,
            text,
        }))
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
                Ok(Ok(Pass::Processed | Pass::Deferred | Pass::Dropped)) => continue,
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
