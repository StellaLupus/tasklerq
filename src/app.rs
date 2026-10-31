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
use crate::telegram::runtime::{self, StartError};

/// Собранное приложение и его фоновые задачи.
pub struct Application {
    /// Готовый HTTP-роутер.
    pub router: Router,
    /// Фоновые задачи Telegram; в режиме `disabled` список пуст.
    pub workers: Vec<JoinHandle<()>>,
}

/// Собрать состояние и роутер так же, как это делает `serve()`.
///
/// В режиме `TELEGRAM_MODE=disabled` без токена [`runtime::start`] ничего не
/// запускает: ingress и воркеры отсутствуют, а HTTP API и capture работают.
pub async fn build_application(
    config: &Config,
    pool: PgPool,
    shutdown: watch::Receiver<bool>,
) -> Result<Application, StartError> {
    let ai = runtime::build_ai(&config.llm);
    let breaker = Arc::new(CircuitBreaker::default());
    let telegram = runtime::start(config, &pool, shutdown, ai.clone(), breaker.clone()).await?;

    let mut state = AppState::new(pool);
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

    Ok(Application {
        router: build_router(state),
        workers,
    })
}
