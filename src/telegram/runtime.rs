//! Запуск Telegram-части сервиса: приём, воркеры и доставка.

use std::sync::Arc;

use sqlx::PgPool;
use tokio::sync::{Notify, watch};
use tokio::task::JoinHandle;
use url::Url;

use crate::config::{Config, LlmConfig, TelegramMode};
use crate::jobs::scheduler::Scheduler;
use crate::llm::Chain;
use crate::security_audit::Audit;

use super::api::{DEFAULT_BASE_URL, HttpTelegramApi, TelegramApi};
use super::bot::Bot;
use super::capture_worker::CaptureWorker;
use super::outbox::OutboxWorker;
use super::webhook::Ingress;
use super::worker::{UPDATES_PER_MINUTE, UpdateWorker, poll};

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
    llm: Option<Arc<Chain>>,
    audit: Audit,
) -> Result<Option<Runtime>, StartError> {
    start_with_update_limit(config, pool, shutdown, llm, audit, None).await
}

/// Запустить Telegram-часть с явным лимитом обновлений на пользователя.
///
/// `update_limit = None` использует боевой [`UPDATES_PER_MINUTE`]. Переопределение
/// нужно нагрузочному тесту, который проверяет обработчик, а не ветку лимита.
pub async fn start_with_update_limit(
    config: &Config,
    pool: &PgPool,
    shutdown: watch::Receiver<bool>,
    llm: Option<Arc<Chain>>,
    audit: Audit,
    update_limit: Option<u32>,
) -> Result<Option<Runtime>, StartError> {
    let telegram = &config.telegram;
    let Some(token) = telegram.bot_token.clone() else {
        return Ok(None);
    };
    if telegram.mode == TelegramMode::Disabled {
        return Ok(None);
    }

    let api: Arc<dyn TelegramApi> = Arc::new(HttpTelegramApi::new(
        token,
        telegram
            .api_base_url
            .as_ref()
            .map_or(DEFAULT_BASE_URL, Url::as_str),
    )?);
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

    let bot = Bot::new(telegram.bot_username.clone())
        .with_llm_capture(llm.is_some())
        .with_tenancy_config(config.tenancy)
        .with_audit(audit.clone());
    if let Some(llm) = llm {
        let capture = CaptureWorker::new(
            pool.clone(),
            llm,
            bot.clone(),
            wake_outbox.clone(),
            audit.clone(),
        )
        .with_quota(config.llm.daily_quota);
        tasks.push(tokio::spawn(capture.run(shutdown.clone())));
    }
    let updates = UpdateWorker::new(
        pool.clone(),
        Arc::new(bot),
        api.clone(),
        config.queue.max_attempts,
        wake_outbox.clone(),
        audit,
    )
    .with_rate_limit(update_limit.unwrap_or(UPDATES_PER_MINUTE));
    tasks.push(tokio::spawn(updates.run(wake_updates, shutdown.clone())));

    let scheduler = Scheduler::new(pool.clone(), wake_outbox.clone());
    tasks.push(tokio::spawn(scheduler.run(shutdown.clone())));

    let outbox = OutboxWorker::new(pool.clone(), api);
    tasks.push(tokio::spawn(outbox.run(wake_outbox, shutdown)));

    Ok(Some(Runtime { ingress, tasks }))
}

/// Клиент разбора текста; `None`, если разбор выключен.
///
/// Ошибка построения бэкенда не роняет сервис: слоты не настроены — разбор
/// выключен, а свободный текст идёт в быстрое добавление. Текст предупреждения
/// не содержит адрес и токен слота.
pub fn build_llm(llm: &LlmConfig) -> Option<Arc<Chain>> {
    match Chain::from_config(llm) {
        Ok(Some(chain)) => Some(Arc::new(chain)),
        Ok(None) => None,
        Err(error) => {
            tracing::warn!(
                error = error.code(),
                "LLM capture is disabled: slot configuration is invalid"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::Map;

    use super::*;
    use crate::config::{SecretString, SlotConfig};

    fn slot() -> SlotConfig {
        SlotConfig {
            url: "http://127.0.0.1:8080/v1".parse().expect("url"),
            model: "local/model".to_owned(),
            token: Some(SecretString::new("token")),
            timeout: Duration::from_secs(10),
            extra: Map::new(),
        }
    }

    #[test]
    fn disabled_llm_capture_builds_no_chain() {
        let llm = LlmConfig {
            enabled: false,
            primary: Some(slot()),
            fallback: None,
            connect_timeout: Duration::from_millis(1000),
            daily_quota: 200,
        };
        assert!(build_llm(&llm).is_none());
    }

    #[test]
    fn configured_slots_build_a_chain() {
        let llm = LlmConfig {
            enabled: true,
            primary: Some(slot()),
            fallback: Some(slot()),
            connect_timeout: Duration::from_millis(1000),
            daily_quota: 200,
        };

        let chain = build_llm(&llm).expect("configured slots build a chain");
        assert_eq!(chain.primary().name(), "primary");
        assert_eq!(chain.fallback().map(|slot| slot.name()), Some("fallback"));
    }
}
