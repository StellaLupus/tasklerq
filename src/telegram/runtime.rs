//! Запуск Telegram-части сервиса: приём, воркеры и доставка.

use std::sync::Arc;

use sqlx::PgPool;
use tokio::sync::{Notify, watch};
use tokio::task::JoinHandle;

use crate::config::{Config, TelegramMode};

use super::api::{DEFAULT_BASE_URL, HttpTelegramApi, TelegramApi};
use super::bot::Bot;
use super::outbox::OutboxWorker;
use super::webhook::Ingress;
use super::worker::{UpdateWorker, poll};

/// Запущенная Telegram-часть.
pub struct Runtime {
    /// Приём webhook; `None` вне режима `webhook`.
    pub ingress: Option<Ingress>,
    /// Фоновые задачи; завершаются по сигналу остановки.
    pub tasks: Vec<JoinHandle<()>>,
}

/// Ошибка запуска Telegram-части.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    /// Не удалось создать HTTP-клиент.
    #[error("failed to build the Telegram client")]
    Client(#[from] reqwest::Error),
}

/// Запустить воркеры согласно `TELEGRAM_MODE`. В режиме `disabled` ничего не
/// запускается: API и capture продолжают работать без бота.
pub async fn start(
    config: &Config,
    pool: &PgPool,
    shutdown: watch::Receiver<bool>,
) -> Result<Option<Runtime>, StartError> {
    let telegram = &config.telegram;
    let Some(token) = telegram.bot_token.clone() else {
        return Ok(None);
    };
    if telegram.mode == TelegramMode::Disabled {
        return Ok(None);
    }

    let api: Arc<dyn TelegramApi> = Arc::new(HttpTelegramApi::new(token, DEFAULT_BASE_URL)?);
    let wake_updates = Arc::new(Notify::new());
    let wake_outbox = Arc::new(Notify::new());
    let mut tasks = Vec::new();
    let mut ingress = None;

    match telegram.mode {
        TelegramMode::Webhook => {
            if let (Some(url), Some(secret)) = (&telegram.webhook_url, &telegram.webhook_secret) {
                match api.set_webhook(url.as_str(), secret.expose()).await {
                    Ok(()) => tracing::info!("telegram webhook registered"),
                    // Недоступность Telegram при старте не должна ронять сервис.
                    Err(error) => tracing::warn!(error = error.code(), "setWebhook failed"),
                }
                ingress = Some(Ingress::new(secret, wake_updates.clone()));
            }
        }
        TelegramMode::Polling => {
            tasks.push(tokio::spawn(poll(
                pool.clone(),
                api.clone(),
                wake_updates.clone(),
                shutdown.clone(),
            )));
        }
        TelegramMode::Disabled => {}
    }

    let bot = Bot::new(telegram.bot_username.clone());
    let updates = UpdateWorker::new(
        pool.clone(),
        Arc::new(bot),
        api.clone(),
        config.queue.max_attempts,
        wake_outbox.clone(),
    );
    tasks.push(tokio::spawn(updates.run(wake_updates, shutdown.clone())));

    let outbox = OutboxWorker::new(pool.clone(), api);
    tasks.push(tokio::spawn(outbox.run(wake_outbox, shutdown)));

    Ok(Some(Runtime { ingress, tasks }))
}
