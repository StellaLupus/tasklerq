//! Запуск Telegram-части сервиса: приём, воркеры и доставка.
//!
//! [`TelegramSupervisor`] владеет текущим поколением: клиентом Bot API, его
//! задачами и собственным каналом остановки, производным от процессного
//! shutdown. Первое поколение запускается при сборке приложения; смена
//! поколения на лету появится вместе с консолью оператора.

use std::sync::{Arc, Mutex};

use sqlx::PgPool;
use tokio::sync::{Notify, watch};
use tokio::task::JoinHandle;
use url::Url;

use crate::config::{TelegramMode, TenancyConfig};
use crate::jobs::scheduler::Scheduler;
use crate::llm::LlmHandle;
use crate::security_audit::Audit;
use crate::settings::EffectiveTelegram;

use super::api::{DEFAULT_BASE_URL, HttpTelegramApi, TelegramApi};
use super::bot::Bot;
use super::capture_worker::CaptureWorker;
use super::outbox::OutboxWorker;
use super::webhook::{Ingress, IngressSlot};
use super::worker::{UPDATES_PER_MINUTE, UpdateWorker, poll};

/// Поколение Telegram: канал остановки и фоновые задачи.
struct Generation {
    /// Канал остановки поколения; срабатывает и от процессного shutdown.
    stop: watch::Sender<bool>,
    /// Задачи поколения; завершаются по сигналу `stop`.
    tasks: Vec<JoinHandle<()>>,
}

/// Супервизор поколений Telegram.
///
/// Хранит всё, что нужно для запуска следующего поколения: пул, снимок LLM,
/// журнал аудита, режим размещения, параметры очереди и процессный shutdown.
/// Здесь же лежит общий приём webhook, поэтому смена поколения не требует
/// пересборки роутера.
pub struct TelegramSupervisor {
    pool: PgPool,
    llm: LlmHandle,
    audit: Audit,
    tenancy: TenancyConfig,
    max_attempts: u32,
    update_limit: Option<u32>,
    shutdown: watch::Receiver<bool>,
    ingress: IngressSlot,
    generation: Mutex<Option<Generation>>,
}

/// Ошибка запуска Telegram-части.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    /// Не удалось создать HTTP-клиент.
    #[error("failed to build the Telegram client")]
    Client(#[from] reqwest::Error),
}

impl TelegramSupervisor {
    /// Создать супервизор. Поколение запускается методом [`Self::start`].
    ///
    /// `update_limit = None` использует боевой [`UPDATES_PER_MINUTE`].
    /// Переопределение нужно нагрузочному тесту, который проверяет обработчик,
    /// а не ветку лимита.
    pub fn new(
        pool: PgPool,
        shutdown: watch::Receiver<bool>,
        llm: LlmHandle,
        audit: Audit,
        tenancy: TenancyConfig,
        max_attempts: u32,
        update_limit: Option<u32>,
    ) -> Arc<Self> {
        Arc::new(Self {
            pool,
            llm,
            audit,
            tenancy,
            max_attempts,
            update_limit,
            shutdown,
            ingress: IngressSlot::default(),
            generation: Mutex::new(None),
        })
    }

    /// Общий приём webhook: текущее поколение и признак смены поколения.
    pub fn ingress(&self) -> IngressSlot {
        self.ingress.clone()
    }

    /// Отметить начало смены поколения.
    ///
    /// Вызывается перед заменой настроек: приём снимается, webhook отвечает
    /// `503` с `Retry-After`, пока новое поколение не встанет. В выключенном
    /// режиме (поколения нет и не будет) ответ остаётся `404`.
    pub fn begin_switch(&self) {
        self.ingress.begin_switch();
    }

    /// Запустить поколение по действующей конфигурации.
    ///
    /// Предыдущее поколение (если было) сначала останавливается. В режиме
    /// `disabled` ничего не запускается: API и capture продолжают работать без
    /// бота. `Ok(false)` — поколения нет.
    pub async fn start(&self, telegram: &EffectiveTelegram) -> Result<bool, StartError> {
        self.stop_generation().await;
        let started = self.start_generation(telegram).await?;
        // Новое поколение готово: webhook снова отвечает по действующему режиму.
        self.ingress.end_switch();
        Ok(started)
    }

    async fn start_generation(&self, telegram: &EffectiveTelegram) -> Result<bool, StartError> {
        let Some(group) = telegram.config() else {
            return Ok(false);
        };
        let Some(token) = group.bot_token.clone() else {
            return Ok(false);
        };
        if group.mode == TelegramMode::Disabled {
            return Ok(false);
        }

        let api: Arc<dyn TelegramApi> = Arc::new(HttpTelegramApi::new(
            token,
            telegram
                .api_base_url()
                .map_or(DEFAULT_BASE_URL, Url::as_str),
        )?);
        let wake_updates = Arc::new(Notify::new());
        let wake_outbox = Arc::new(Notify::new());
        // Канал поколения отдельный от процессного: остановка поколения не
        // должна ждать останова процесса. Задача-мост переводит в него
        // процессный shutdown.
        let (stop, shutdown) = watch::channel(false);
        let mut tasks = Vec::new();

        match group.mode {
            TelegramMode::Webhook => {
                if let (Some(url), Some(secret)) = (&group.webhook_url, &group.webhook_secret) {
                    match api.set_webhook(url.as_str(), secret.expose()).await {
                        Ok(()) => tracing::info!("telegram webhook registered"),
                        // Недоступность Telegram при старте не должна ронять сервис.
                        Err(error) => tracing::warn!(error = error.code(), "setWebhook failed"),
                    }
                    self.ingress
                        .store(Some(Ingress::new(secret, wake_updates.clone())));
                }
            }
            TelegramMode::Polling => {
                tasks.push(tokio::spawn(poll(
                    self.pool.clone(),
                    api.clone(),
                    wake_updates.clone(),
                    shutdown.clone(),
                )));
            }
            TelegramMode::Disabled => {}
        }

        let bot = Bot::new(group.bot_username.clone())
            .with_llm(self.llm.clone())
            .with_tenancy_config(self.tenancy)
            .with_audit(self.audit.clone());
        let capture = CaptureWorker::new(
            self.pool.clone(),
            self.llm.clone(),
            bot.clone(),
            wake_outbox.clone(),
            self.audit.clone(),
        );
        tasks.push(tokio::spawn(capture.run(shutdown.clone())));
        let updates = UpdateWorker::new(
            self.pool.clone(),
            Arc::new(bot),
            api.clone(),
            self.max_attempts,
            wake_outbox.clone(),
            self.audit.clone(),
        )
        .with_rate_limit(self.update_limit.unwrap_or(UPDATES_PER_MINUTE));
        tasks.push(tokio::spawn(updates.run(wake_updates, shutdown.clone())));

        let scheduler = Scheduler::new(self.pool.clone(), wake_outbox.clone());
        tasks.push(tokio::spawn(scheduler.run(shutdown.clone())));

        let outbox = OutboxWorker::new(self.pool.clone(), api);
        tasks.push(tokio::spawn(outbox.run(wake_outbox, shutdown.clone())));

        tasks.push(spawn_shutdown_bridge(
            self.shutdown.clone(),
            stop.clone(),
            shutdown,
        ));

        *self.generation.lock().expect("telegram supervisor lock") =
            Some(Generation { stop, tasks });
        Ok(true)
    }

    /// Остановить текущее поколение и дождаться его задач.
    ///
    /// `false` — поколения не было. Процессный shutdown при этом не трогается:
    /// это база для смены поколения из консоли.
    async fn stop_generation(&self) -> bool {
        let generation = self
            .generation
            .lock()
            .expect("telegram supervisor lock")
            .take();
        let Some(generation) = generation else {
            return false;
        };
        self.ingress.store(None);
        // Сигнал обязателен и при уже сработавшем процессном shutdown: задачам
        // нужно увидеть `true`, а не закрытие канала.
        let _ = generation.stop.send(true);
        for task in generation.tasks {
            let _ = task.await;
        }
        true
    }

    /// Дождаться процессного shutdown и остановить поколение.
    ///
    /// Задача живёт в [`crate::app::Application::workers`], поэтому graceful
    /// shutdown процесса ждёт задачи поколения так же, как остальные воркеры.
    pub async fn run(self: Arc<Self>) {
        let mut shutdown = self.shutdown.clone();
        wait_stopped(&mut shutdown).await;
        self.stop_generation().await;
    }
}

/// Перевести процессный shutdown в канал остановки поколения.
fn spawn_shutdown_bridge(
    mut process: watch::Receiver<bool>,
    stop: watch::Sender<bool>,
    mut generation: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if !*process.borrow() {
            tokio::select! {
                _ = process.changed() => {}
                _ = wait_stopped(&mut generation) => return,
            }
        }
        let _ = stop.send(true);
    })
}

/// Дождаться `true` в канале остановки; закрытие отправителя тоже прекращает
/// ожидание.
async fn wait_stopped(shutdown: &mut watch::Receiver<bool>) {
    loop {
        if *shutdown.borrow() {
            return;
        }
        if shutdown.changed().await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use sqlx::postgres::PgPoolOptions;

    use super::*;
    use crate::config::TenancyMode;

    /// Супервизор без соединений с базой: тесты поколений в базу не ходят.
    fn test_supervisor(shutdown: watch::Receiver<bool>) -> Arc<TelegramSupervisor> {
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect_lazy("postgres://tasklerq:secret@127.0.0.1:1/tasklerq")
            .expect("lazy pool");
        TelegramSupervisor::new(
            pool.clone(),
            shutdown,
            LlmHandle::empty(),
            Audit::new(pool),
            TenancyConfig {
                mode: TenancyMode::Single,
                bootstrap_owner: None,
            },
            3,
            None,
        )
    }

    /// Поставить поколение из задач, которые ждут только его остановки.
    fn install_generation(
        supervisor: &TelegramSupervisor,
        process: &watch::Receiver<bool>,
        workers: usize,
        finished: &Arc<AtomicUsize>,
    ) {
        let (stop, stop_rx) = watch::channel(false);
        let mut tasks = Vec::new();
        for _ in 0..workers {
            let mut shutdown = stop_rx.clone();
            let finished = finished.clone();
            tasks.push(tokio::spawn(async move {
                wait_stopped(&mut shutdown).await;
                finished.fetch_add(1, Ordering::SeqCst);
            }));
        }
        tasks.push(spawn_shutdown_bridge(
            process.clone(),
            stop.clone(),
            stop_rx.clone(),
        ));
        *supervisor
            .generation
            .lock()
            .expect("telegram supervisor lock") = Some(Generation { stop, tasks });
    }

    async fn wait_finished(finished: &AtomicUsize, expected: usize) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while finished.load(Ordering::SeqCst) < expected {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("generation tasks must stop in time");
    }

    #[tokio::test]
    async fn process_shutdown_stops_generation_tasks() {
        let (process, process_rx) = watch::channel(false);
        let supervisor = test_supervisor(process_rx.clone());
        let finished = Arc::new(AtomicUsize::new(0));
        install_generation(&supervisor, &process_rx, 2, &finished);

        process.send(true).expect("process shutdown signal");

        // Задачи видят только сигнал поколения: он приходит из моста.
        wait_finished(&finished, 2).await;
        assert!(
            supervisor.stop_generation().await,
            "the generation handle must still be joinable"
        );
        assert!(supervisor.generation.lock().expect("lock").is_none());
    }

    #[tokio::test]
    async fn stopping_a_generation_leaves_the_process_shutdown_untouched() {
        let (_process, process_rx) = watch::channel(false);
        let supervisor = test_supervisor(process_rx.clone());
        let finished = Arc::new(AtomicUsize::new(0));
        install_generation(&supervisor, &process_rx, 2, &finished);

        assert!(supervisor.stop_generation().await);

        assert_eq!(finished.load(Ordering::SeqCst), 2);
        assert!(
            !*process_rx.borrow(),
            "stopping a generation must not signal process shutdown"
        );
        assert!(supervisor.generation.lock().expect("lock").is_none());
    }
}
