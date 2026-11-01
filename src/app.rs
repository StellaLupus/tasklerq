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
use crate::config::Config;
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
    let ai = runtime::build_ai(&config.llm);
    let breaker = Arc::new(CircuitBreaker::default());
    // Один журнал событий на процесс: вебхук, воркеры, бот и capture-воркер
    // делят состояние подавления и счётчики отказов.
    let audit = Audit::new(pool.clone());
    let telegram = runtime::start(
        config,
        &pool,
        shutdown.clone(),
        ai.clone(),
        breaker.clone(),
        audit.clone(),
    )
    .await?;

    let mut state = AppState::new(pool.clone()).with_audit(audit);
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
