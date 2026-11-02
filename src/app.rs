//! Сборка приложения: Telegram-рантайм, состояние обработчиков и роутер.
//!
//! Эту функцию вызывают и `serve()` в `main.rs`, и интеграционные тесты, чтобы
//! тест проверял production-путь, а не отдельную ручную сборку.

use std::sync::Arc;

use axum::Router;
use sqlx::PgPool;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::capture::breaker::CircuitBreaker;
use crate::config::{Config, TelegramMode};
use crate::http::api::capture::CaptureState;
use crate::http::router::{AppState, build_router};
use crate::jobs::retention::RetentionWorker;
use crate::security_audit::Audit;
use crate::telegram::runtime::{self, StartError};

/// Собранное приложение и его фоновые задачи.
pub struct Application {
    /// Готовый HTTP-роутер.
    pub router: Router,
    /// Фоновые задачи: Telegram (в режиме `disabled` их нет) и retention.
    pub workers: Vec<JoinHandle<()>>,
}

/// Собрать состояние и роутер так же, как это делает `serve()`.
///
/// В режиме `TELEGRAM_MODE=disabled` без токена [`runtime::start`] ничего не
/// запускает: ingress и воркеры Telegram отсутствуют, а HTTP API и capture
/// работают. Retention запускается всегда и попадает в [`Application::workers`].
pub async fn build_application(
    config: &Config,
    pool: PgPool,
    shutdown: watch::Receiver<bool>,
) -> Result<Application, StartError> {
    build_application_with_update_limit(config, pool, shutdown, None).await
}

/// Собрать приложение с явным лимитом обновлений Telegram на пользователя.
///
/// `None` — боевой лимит. Переопределение используется нагрузочным тестом,
/// который должен провести все обновления через обработчик, а не через ветку
/// `rate_limited`.
pub async fn build_application_with_update_limit(
    config: &Config,
    pool: PgPool,
    shutdown: watch::Receiver<bool>,
    update_limit: Option<u32>,
) -> Result<Application, StartError> {
    warn_if_bootstrap_owner_missing(config);

    let ai = runtime::build_ai(&config.llm);
    let breaker = Arc::new(CircuitBreaker::default());
    // Один журнал событий на процесс: вебхук, воркеры, бот и capture-воркер
    // делят состояние подавления и счётчики отказов.
    let audit = Audit::new(pool.clone());
    let telegram = runtime::start_with_update_limit(
        config,
        &pool,
        shutdown.clone(),
        ai.clone(),
        breaker.clone(),
        audit.clone(),
        update_limit,
    )
    .await?;

    let mut state = AppState::new(pool.clone(), audit);
    let mut workers = Vec::new();
    if let Some(runtime) = telegram {
        if let Some(ingress) = runtime.ingress {
            state = state.with_telegram(ingress);
        }
        state = state.with_queue_lag_limit(config.queue.ready_lag_seconds);
        workers = runtime.tasks;
    }
    if config.capture.enabled {
        state = state.with_capture(CaptureState::new(
            config.capture,
            ai,
            breaker,
            config.llm.daily_quota,
        ));
    }

    // Retention не зависит от Telegram: чистит базу и без токена бота.
    workers.push(tokio::spawn(
        RetentionWorker::new(pool, config.retention).run(shutdown),
    ));

    Ok(Application {
        router: build_router(state),
        workers,
    })
}

/// Предупредить, что без явного первого владельца им станет первый написавший
/// боту. Текст не содержит секретов: в нём только имя переменной.
fn warn_if_bootstrap_owner_missing(config: &Config) {
    if config.telegram.mode != TelegramMode::Disabled && config.tenancy.bootstrap_owner.is_none() {
        tracing::warn!(
            "BOOTSTRAP_OWNER_TELEGRAM_ID is not set: the first person to write to the bot \
             will become the owner; set BOOTSTRAP_OWNER_TELEGRAM_ID to choose the owner explicitly"
        );
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    use super::*;

    const BOT_TOKEN: &str = "123456:SECRET-bot-token";
    const WEBHOOK_SECRET: &str = "SECRET-webhook-secret";

    #[derive(Clone, Default)]
    struct LogBuffer(Arc<Mutex<Vec<u8>>>);

    impl LogBuffer {
        fn contents(&self) -> String {
            let bytes = self.0.lock().expect("log buffer lock").clone();
            String::from_utf8(bytes).expect("logs are UTF-8")
        }
    }

    impl Write for LogBuffer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .expect("log buffer lock")
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'writer> tracing_subscriber::fmt::MakeWriter<'writer> for LogBuffer {
        type Writer = Self;

        fn make_writer(&'writer self) -> Self::Writer {
            self.clone()
        }
    }

    fn config(mode: &str, bootstrap_owner: Option<&str>) -> Config {
        let mut vars = BTreeMap::from([
            ("ENV".to_owned(), "development".to_owned()),
            (
                "PUBLIC_BASE_URL".to_owned(),
                "http://127.0.0.1:8080".to_owned(),
            ),
            (
                "DATABASE_URL".to_owned(),
                "postgres://tasklerq:db-password@127.0.0.1:5432/tasklerq".to_owned(),
            ),
            ("TELEGRAM_MODE".to_owned(), mode.to_owned()),
            ("TELEGRAM_BOT_TOKEN".to_owned(), BOT_TOKEN.to_owned()),
            (
                "TELEGRAM_WEBHOOK_SECRET".to_owned(),
                WEBHOOK_SECRET.to_owned(),
            ),
            (
                "TELEGRAM_WEBHOOK_URL".to_owned(),
                "http://127.0.0.1:8080/webhooks/telegram".to_owned(),
            ),
            ("LLM_CAPTURE_ENABLED".to_owned(), "false".to_owned()),
        ]);
        if let Some(owner) = bootstrap_owner {
            vars.insert("BOOTSTRAP_OWNER_TELEGRAM_ID".to_owned(), owner.to_owned());
        }
        Config::from_map(vars).expect("test config should be valid")
    }

    fn captured_warning(config: &Config) -> String {
        let buffer = LogBuffer::default();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_writer(buffer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            warn_if_bootstrap_owner_missing(config);
        });
        buffer.contents()
    }

    #[test]
    fn warns_when_telegram_is_enabled_without_bootstrap_owner() {
        for mode in ["webhook", "polling"] {
            let logs = captured_warning(&config(mode, None));

            assert!(
                logs.contains("BOOTSTRAP_OWNER_TELEGRAM_ID"),
                "{mode}: warning should name the parameter:\n{logs}"
            );
            assert!(
                logs.contains("first person"),
                "{mode}: warning should explain who becomes the owner:\n{logs}"
            );
            assert!(
                !logs.contains(BOT_TOKEN),
                "{mode}: warning leaked the token"
            );
            assert!(
                !logs.contains(WEBHOOK_SECRET),
                "{mode}: warning leaked the webhook secret"
            );
        }
    }

    #[test]
    fn no_warning_when_bootstrap_owner_is_set() {
        for mode in ["webhook", "polling"] {
            let logs = captured_warning(&config(mode, Some("111")));
            assert!(
                !logs.contains("BOOTSTRAP_OWNER_TELEGRAM_ID"),
                "{mode}: explicit owner should suppress the warning:\n{logs}"
            );
        }
    }

    #[test]
    fn no_warning_when_telegram_is_disabled() {
        let logs = captured_warning(&config("disabled", None));
        assert!(
            !logs.contains("BOOTSTRAP_OWNER_TELEGRAM_ID"),
            "disabled Telegram should not warn:\n{logs}"
        );
    }
}
