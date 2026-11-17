//! Запуск Telegram-части сервиса: приём, воркеры и доставка.
//!
//! [`TelegramSupervisor`] владеет текущим поколением: клиентом Bot API, его
//! задачами и собственным каналом остановки. Первое поколение запускается при
//! сборке приложения, следующее — [`TelegramSupervisor::reload`] по настройкам
//! из базы. Процессный shutdown останавливает текущее поколение и ждёт его
//! задачи независимо от того, когда поколение запущено.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use sqlx::PgPool;
use tokio::sync::{Mutex as AsyncMutex, Notify, watch};
use tokio::task::JoinHandle;
use url::Url;

use crate::config::{Config, TelegramMode, TenancyConfig};
use crate::jobs::scheduler::Scheduler;
use crate::llm::LlmHandle;
use crate::security_audit::Audit;
use crate::settings::EffectiveTelegram;
use crate::settings::crypto::SecretCipher;

use super::api::{DEFAULT_BASE_URL, HttpTelegramApi, TelegramApi};
use super::bot::Bot;
use super::capture_worker::CaptureWorker;
use super::outbox::OutboxWorker;
use super::webhook::{Ingress, IngressSlot};
use super::worker::{UPDATES_PER_MINUTE, UpdateWorker, poll};

/// Сколько ждать кооперативной остановки поколения перед `abort`.
pub const STOP_TIMEOUT: Duration = Duration::from_secs(30);

/// Поколение Telegram: клиент Bot API, канал остановки и фоновые задачи.
struct Generation {
    /// Канал остановки поколения.
    stop: watch::Sender<bool>,
    /// Задачи поколения; завершаются по сигналу `stop`.
    tasks: Vec<JoinHandle<()>>,
    /// Клиент поколения. Пока не используется после остановки; понадобится
    /// смене бота для `deleteWebhook` прежним токеном.
    #[allow(dead_code)]
    api: Arc<dyn TelegramApi>,
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
    /// Окружение и параметры разбора настроек: те же, что при старте.
    config: Config,
    cipher: SecretCipher,
    ingress: IngressSlot,
    generation: Mutex<Option<Generation>>,
    /// Сериализует смены поколения, включая остановку по shutdown процесса.
    switch: AsyncMutex<()>,
    /// Сколько ждать кооперативной остановки; переопределяется тестом.
    stop_timeout: Mutex<Duration>,
}

/// Ошибка запуска Telegram-части.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    /// Не удалось создать HTTP-клиент.
    #[error("failed to build the Telegram client")]
    Client(#[from] reqwest::Error),
}

/// Ошибка переключения поколения по сохранённым настройкам.
#[derive(Debug, thiserror::Error)]
pub enum ReloadError {
    /// Не удалось прочитать настройки из базы.
    #[error("could not read telegram settings")]
    Database(#[from] sqlx::Error),
    /// Не удалось запустить новое поколение.
    #[error(transparent)]
    Start(#[from] StartError),
}

impl TelegramSupervisor {
    /// Создать супервизор. Поколение запускается методом [`Self::start`].
    ///
    /// `update_limit = None` использует боевой [`UPDATES_PER_MINUTE`].
    /// Переопределение нужно нагрузочному тесту, который проверяет обработчик,
    /// а не ветку лимита.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pool: PgPool,
        shutdown: watch::Receiver<bool>,
        llm: LlmHandle,
        audit: Audit,
        tenancy: TenancyConfig,
        max_attempts: u32,
        update_limit: Option<u32>,
        config: Config,
        cipher: SecretCipher,
    ) -> Arc<Self> {
        Arc::new(Self {
            pool,
            llm,
            audit,
            tenancy,
            max_attempts,
            update_limit,
            shutdown,
            config,
            cipher,
            ingress: IngressSlot::default(),
            generation: Mutex::new(None),
            switch: AsyncMutex::new(()),
            stop_timeout: Mutex::new(STOP_TIMEOUT),
        })
    }

    /// Общий приём webhook: текущее поколение и признак смены поколения.
    pub fn ingress(&self) -> IngressSlot {
        self.ingress.clone()
    }

    /// Переопределить ожидание кооперативной остановки (для тестов).
    pub fn set_stop_timeout(&self, timeout: Duration) {
        *self.stop_timeout.lock().expect("telegram stop timeout") = timeout;
    }

    /// Отметить начало смены поколения.
    ///
    /// Вручную не вызывается: [`Self::start`] и [`Self::reload`] делают это
    /// сами и снимают признак на любом выходе.
    pub fn begin_switch(&self) {
        self.ingress.begin_switch();
    }

    /// Запустить поколение по действующей конфигурации.
    ///
    /// Предыдущее поколение (если было) сначала останавливается. В режиме
    /// `disabled` ничего не запускается: API и capture продолжают работать без
    /// бота. `Ok(false)` — поколения нет.
    pub async fn start(&self, telegram: &EffectiveTelegram) -> Result<bool, StartError> {
        let _switch = self.switch.lock().await;
        self.switch_to(telegram).await
    }

    /// Переключить поколение по действующей конфигурации из базы и окружения.
    ///
    /// Настройки читаются так же, как при старте, поэтому смена токена, секрета
    /// и режима не требует перезапуска. Ошибка чтения базы оставляет текущее
    /// поколение работать. `Ok(false)` — бот выключен по новой конфигурации.
    pub async fn reload(&self) -> Result<bool, ReloadError> {
        let _switch = self.switch.lock().await;
        let telegram =
            crate::settings::read_effective_telegram(&self.pool, &self.config, &self.cipher)
                .await?;
        Ok(self.switch_to(&telegram).await?)
    }

    /// Заменить поколение; вызывается под мьютексом переключения.
    async fn switch_to(&self, telegram: &EffectiveTelegram) -> Result<bool, StartError> {
        // Процесс уже завершается: `run` либо остановит текущее поколение, либо
        // уже сделал это. Новое поколение после его выхода никто бы не ждал.
        if *self.shutdown.borrow() {
            return Ok(false);
        }
        // Приём снимается на всё время переключения, а признак снимается на
        // любом выходе, включая ошибку сборки клиента.
        self.begin_switch();
        let _switch = SwitchGuard(&self.ingress);
        self.stop_generation().await;
        self.start_generation(telegram).await
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
        // должна ждать останова процесса. Процессный shutdown переводит в него
        // `run()` после завершения текущего переключения.
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
                // Пока у бота стоит webhook, `getUpdates` отвечает `409`.
                // Снимаем его до запуска polling, чтобы смена режима на лету
                // действительно начинала принимать обновления.
                match api.delete_webhook().await {
                    Ok(()) => tracing::info!("telegram webhook removed for polling"),
                    Err(error) => tracing::warn!(error = error.code(), "deleteWebhook failed"),
                }
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

        let outbox = OutboxWorker::new(self.pool.clone(), api.clone());
        tasks.push(tokio::spawn(outbox.run(wake_outbox, shutdown)));

        *self.generation.lock().expect("telegram supervisor lock") =
            Some(Generation { stop, tasks, api });
        Ok(true)
    }

    /// Остановить текущее поколение и дождаться его задач.
    ///
    /// `false` — поколения не было. Процессный shutdown при этом не трогается:
    /// это база для смены поколения из консоли. Если задача не остановилась за
    /// [`Self::set_stop_timeout`], она получает `abort`, а в журнал уходит
    /// ошибка без секретов.
    async fn stop_generation(&self) -> bool {
        let generation = self
            .generation
            .lock()
            .expect("telegram supervisor lock")
            .take();
        let Some(Generation { stop, tasks, .. }) = generation else {
            return false;
        };
        self.ingress.store(None);
        // Сигнал обязателен и при уже сработавшем shutdown: задачам нужно
        // увидеть `true`, а не закрытие канала.
        let _ = stop.send(true);

        let timeout = *self.stop_timeout.lock().expect("telegram stop timeout");
        let deadline = tokio::time::Instant::now() + timeout;
        let mut tasks = tasks;
        for index in 0..tasks.len() {
            if tokio::time::timeout_at(deadline, &mut tasks[index])
                .await
                .is_err()
            {
                tracing::error!(
                    timeout_ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
                    tasks = tasks.len(),
                    "telegram generation did not stop in time; aborting its tasks"
                );
                for task in &tasks[index..] {
                    task.abort();
                }
                for task in &mut tasks[index..] {
                    let _ = task.await;
                }
                break;
            }
        }
        true
    }

    /// Дождаться процессного shutdown и остановить поколение.
    ///
    /// Задача живёт в [`crate::app::Application::workers`] всегда, поэтому
    /// graceful shutdown процесса ждёт задачи текущего поколения, даже если оно
    /// запущено переключением после старта. Мьютекс переключения не даёт
    /// shutdown разойтись с идущей сменой поколения.
    pub async fn run(self: Arc<Self>) {
        let mut shutdown = self.shutdown.clone();
        wait_stopped(&mut shutdown).await;
        let _switch = self.switch.lock().await;
        self.stop_generation().await;
    }
}

/// Снять признак переключения при выходе из [`TelegramSupervisor::switch_to`].
struct SwitchGuard<'a>(&'a IngressSlot);

impl Drop for SwitchGuard<'_> {
    fn drop(&mut self) {
        self.0.end_switch();
    }
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
    use crate::config::{SecretString, TenancyMode};

    /// Супервизор без соединений с базой: тесты поколений в базу не ходят.
    fn test_supervisor(shutdown: watch::Receiver<bool>) -> Arc<TelegramSupervisor> {
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect_lazy("postgres://tasklerq:secret@127.0.0.1:1/tasklerq")
            .expect("lazy pool");
        let config = Config::from_map([(
            "DATABASE_URL",
            "postgres://tasklerq:secret@127.0.0.1:1/tasklerq",
        )])
        .expect("test config");
        let cipher = SecretCipher::from_config(&config);
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
            config,
            cipher,
        )
    }

    /// Клиент поколения, который никуда не ходит: тесты не вызывают Bot API.
    fn idle_api() -> Arc<dyn TelegramApi> {
        Arc::new(
            HttpTelegramApi::new(SecretString::new("1:test"), DEFAULT_BASE_URL)
                .expect("http client"),
        )
    }

    /// Поставить поколение из задач, которые ждут только его остановки.
    fn install_generation(
        supervisor: &TelegramSupervisor,
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
        *supervisor
            .generation
            .lock()
            .expect("telegram supervisor lock") = Some(Generation {
            stop,
            tasks,
            api: idle_api(),
        });
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
        let supervisor = test_supervisor(process_rx);
        let finished = Arc::new(AtomicUsize::new(0));
        install_generation(&supervisor, 2, &finished);

        let run = tokio::spawn(supervisor.clone().run());
        process.send(true).expect("process shutdown signal");

        wait_finished(&finished, 2).await;
        run.await.expect("shutdown task must finish");
        assert!(
            supervisor.generation.lock().expect("lock").is_none(),
            "process shutdown must take the generation handle"
        );
    }

    #[tokio::test]
    async fn stopping_a_generation_leaves_the_process_shutdown_untouched() {
        let (_process, process_rx) = watch::channel(false);
        let supervisor = test_supervisor(process_rx.clone());
        let finished = Arc::new(AtomicUsize::new(0));
        install_generation(&supervisor, 2, &finished);

        assert!(supervisor.stop_generation().await);

        assert_eq!(finished.load(Ordering::SeqCst), 2);
        assert!(
            !*process_rx.borrow(),
            "stopping a generation must not signal process shutdown"
        );
        assert!(supervisor.generation.lock().expect("lock").is_none());
    }

    #[tokio::test]
    async fn a_switch_after_process_shutdown_starts_nothing() {
        let (_process, process_rx) = watch::channel(true);
        let supervisor = test_supervisor(process_rx);
        let finished = Arc::new(AtomicUsize::new(0));
        install_generation(&supervisor, 1, &finished);

        let config = Config::from_map([(
            "DATABASE_URL",
            "postgres://tasklerq:secret@127.0.0.1:1/tasklerq",
        )])
        .expect("test config");
        let cipher = SecretCipher::from_config(&config);
        let telegram = crate::settings::effective_telegram(&config, &[], &cipher);

        assert!(
            !supervisor
                .switch_to(&telegram)
                .await
                .expect("no generation after shutdown"),
            "a switch must not start a generation after process shutdown"
        );
        assert!(
            supervisor.generation.lock().expect("lock").is_some(),
            "the running generation is left for `run` to stop"
        );
    }

    #[tokio::test]
    async fn a_stuck_generation_is_aborted_after_the_timeout() {
        let (_process, process_rx) = watch::channel(false);
        let supervisor = test_supervisor(process_rx);
        let (stop, _stop_rx) = watch::channel(false);
        let task = tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        *supervisor
            .generation
            .lock()
            .expect("telegram supervisor lock") = Some(Generation {
            stop,
            tasks: vec![task],
            api: idle_api(),
        });
        supervisor.set_stop_timeout(Duration::from_millis(50));

        let stopped = tokio::time::timeout(Duration::from_secs(5), supervisor.stop_generation())
            .await
            .expect("the supervisor must not wait for a stuck task");

        assert!(stopped);
        assert!(supervisor.generation.lock().expect("lock").is_none());
    }
}
