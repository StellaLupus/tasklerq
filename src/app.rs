//! Сборка приложения: Telegram-рантайм, состояние обработчиков и роутер.
//!
//! Эту функцию вызывают и `serve()` в `main.rs`, и интеграционные тесты, чтобы
//! тест проверял production-путь, а не отдельную ручную сборку.

use axum::Router;
use sqlx::PgPool;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::admin_console::router::{AdminState, build_router as build_admin_router};
use crate::admin_console::wizard::WizardSettings;
use crate::capture::slot_status::{LlmSlotHealth, SlotStatus, SlotStatusReport};
use crate::config::{Config, TelegramMode, TenancyMode};
use crate::http::api::capture::CaptureState;
use crate::http::router::{AppState, build_router};
use crate::jobs::domain::DomainTicker;
use crate::jobs::retention::RetentionWorker;
use crate::llm::{LlmHandle, LlmSettings, LlmSnapshot};
use crate::security_audit::Audit;
use crate::settings;
use crate::settings::crypto::SecretCipher;
use crate::storage::repositories::households;
use crate::telegram::runtime::{self, StartError};

/// Собранное приложение и его фоновые задачи.
pub struct Application {
    /// Готовый HTTP-роутер.
    pub router: Router,
    /// Роутер консоли; `None`, если `ADMIN_BIND_ADDR=disabled`.
    pub admin_router: Option<Router>,
    /// Фоновые задачи: Telegram (в режиме `disabled` их нет), retention и
    /// доменный тикер.
    pub workers: Vec<JoinHandle<()>>,
    /// Применение сохранённых настроек LLM к снимку без перезапуска: вход
    /// будущей консоли администратора и тестов.
    pub llm_settings: LlmSettings,
    /// Действующий режим Telegram: из окружения, базы или `disabled`.
    pub telegram_mode: TelegramMode,
}

/// Собрать состояние и роутер так же, как это делает `serve()`.
///
/// В режиме `TELEGRAM_MODE=disabled` без токена [`runtime::start`] ничего не
/// запускает: ingress и воркеры Telegram отсутствуют, а HTTP API и capture
/// работают. Retention и доменный тикер запускаются всегда и попадают в
/// [`Application::workers`].
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
    let secret_cipher = SecretCipher::from_config(config);
    warn_if_stored_secrets_unreadable(&pool, &secret_cipher).await;

    // Действующая конфигурация Telegram: окружение целиком при токене, иначе
    // база. Сбой чтения базы не роняет сервис — действует окружение.
    let telegram = build_effective_telegram(&pool, config, &secret_cipher).await;
    warn_if_bootstrap_owner_missing(config, &telegram);
    warn_if_multiple_households(config, &pool).await;

    let llm = build_llm_handle(&pool, config, &secret_cipher).await;
    let llm_settings = LlmSettings::new(
        pool.clone(),
        config.llm.clone(),
        secret_cipher.clone(),
        llm.clone(),
    );
    let llm_slots = std::sync::Arc::new(LlmSlotHealth::new(llm.clone()));
    warn_if_llm_slots_unverified(&llm_slots, &pool).await;

    // Один журнал событий на процесс: вебхук, воркеры, бот и capture-воркер
    // делят состояние подавления и счётчики отказов.
    let audit = Audit::new(pool.clone());
    let telegram_mode = telegram.mode();
    let telegram_runtime = runtime::start_with_update_limit(
        config,
        &telegram,
        &pool,
        shutdown.clone(),
        llm.clone(),
        audit.clone(),
        update_limit,
    )
    .await?;

    let mut state = AppState::new(pool.clone(), audit.clone())
        .with_tenancy_mode(config.tenancy.mode)
        .with_secret_cipher(secret_cipher.clone())
        .with_llm_slots(llm_slots.clone());
    let mut workers = Vec::new();
    let mut queue_lag_seconds = None;
    if let Some(runtime) = telegram_runtime {
        if let Some(ingress) = runtime.ingress {
            state = state.with_telegram(ingress);
        }
        state = state.with_queue_lag_limit(config.queue.ready_lag_seconds);
        queue_lag_seconds = Some(config.queue.ready_lag_seconds);
        workers = runtime.tasks;
    }
    if config.capture.enabled {
        state = state.with_capture(CaptureState::new(config.capture, llm));
    }

    // Консоль получает собственный роутер и listener. `disabled` не создаёт
    // ни того, ни другого.
    let admin_router = config.admin_bind_addr.map(|_| {
        let wizard = WizardSettings::new(config.tenancy.mode, telegram.is_enabled());
        let mut admin = AdminState::new(
            pool.clone(),
            config.admin_trusted_proxies.clone(),
            audit,
            llm_settings.clone(),
        )
        .with_wizard(wizard)
        .with_llm_slots(llm_slots);
        if let Some(seconds) = queue_lag_seconds {
            admin = admin.with_queue_lag_limit(seconds);
        }
        build_admin_router(admin)
    });

    // Retention и доменный тикер не зависят от Telegram: чистят базу и
    // переводят повторяющиеся события и без токена бота.
    workers.push(tokio::spawn(
        RetentionWorker::new(pool.clone(), config.retention).run(shutdown.clone()),
    ));
    workers.push(tokio::spawn(DomainTicker::new(pool).run(shutdown)));

    Ok(Application {
        router: build_router(state),
        admin_router,
        workers,
        llm_settings,
        telegram_mode,
    })
}

/// Собрать действующую конфигурацию Telegram из базы и окружения.
///
/// Бот, настроенный только в консоли, начинает работать после перезапуска.
/// Сбой чтения базы не роняет сервис: действует конфигурация из окружения
/// (без строк настройки Telegram выключен), а в журнал уходит предупреждение без
/// значений настроек.
async fn build_effective_telegram(
    pool: &PgPool,
    config: &Config,
    cipher: &SecretCipher,
) -> settings::EffectiveTelegram {
    match settings::read_effective_telegram(pool, config, cipher).await {
        Ok(effective) => effective,
        Err(error) => {
            tracing::warn!(
                error = %error,
                "could not read telegram settings at startup; using environment only"
            );
            settings::effective_telegram(config, &[], cipher)
        }
    }
}

/// Собрать снимок LLM из окружения, базы и умолчаний.
///
/// Слоты, заданные только в консоли, начинают работать после перезапуска без
/// `LLM_*` в окружении. Сбой чтения базы не роняет сервис: действует
/// конфигурация из окружения и умолчаний, а в журнал уходит предупреждение без
/// значений настроек.
async fn build_llm_handle(pool: &PgPool, config: &Config, cipher: &SecretCipher) -> LlmHandle {
    let effective = match settings::read_effective_llm(pool, &config.llm, cipher).await {
        Ok(effective) => effective,
        Err(error) => {
            tracing::warn!(
                error = %error,
                "could not read llm settings at startup; using environment and defaults"
            );
            settings::effective_llm(&config.llm, &[], cipher)
        }
    };
    LlmHandle::new(LlmSnapshot::from_effective(effective))
}

/// Предупредить, что без явного первого владельца им станет первый написавший
/// боту. Текст не содержит секретов: в нём только имя переменной.
fn warn_if_bootstrap_owner_missing(config: &Config, telegram: &settings::EffectiveTelegram) {
    if telegram.is_enabled() && config.tenancy.bootstrap_owner.is_none() {
        tracing::warn!(
            "BOOTSTRAP_OWNER_TELEGRAM_ID is not set: the first person to write to the bot \
             will become the owner; set BOOTSTRAP_OWNER_TELEGRAM_ID to choose the owner explicitly"
        );
    }
}

/// Сообщить о деградации: в режиме `single` активных household больше одного.
///
/// Деградация — вычисляемое состояние (design D4), поэтому лог пишется один раз
/// при старте и содержит только число: названия и идентификаторы не нужны и
/// могут быть чувствительными. Сбой подсчёта не мешает запуску: недоступность
/// базы уже обработают readiness и рабочие задачи, а падение сервиса из-за
/// диагностической проверки недопустимо.
async fn warn_if_multiple_households(config: &Config, pool: &PgPool) {
    if config.tenancy.mode != TenancyMode::Single {
        return;
    }

    let counted = async {
        let mut conn = pool.acquire().await?;
        households::count_active(&mut conn).await
    }
    .await;

    match counted {
        Ok(active) if active > 1 => {
            tracing::error!(
                households = active,
                "single tenancy mode with multiple active households: creating new households \
                 is disabled; see the runbook for cleanup"
            );
        }
        Ok(_) => {}
        Err(error) => {
            tracing::warn!(
                error = %error,
                "could not count active households at startup"
            );
        }
    }
}

/// Предупредить, что сохранённые секреты нельзя расшифровать.
///
/// Проверяются все зашифрованные строки, включая неизвестные этой версии
/// ключи: при ротации их тоже придётся перешифровать. В строке нет имён
/// настроек и значений — только признак и число строк. Сбой чтения базы не
/// мешает запуску: его покажут readiness и рабочие задачи.
async fn warn_if_stored_secrets_unreadable(pool: &PgPool, cipher: &SecretCipher) {
    match settings::read_secret_health(pool, cipher).await {
        Ok(health) => {
            if let Some(problem) = health.problem() {
                tracing::warn!(
                    code = problem.code(),
                    secrets = health.secrets(),
                    "stored secrets cannot be decrypted: secret settings are unavailable until \
                     the encryption key is fixed"
                );
            }
        }
        Err(error) => {
            tracing::warn!(
                error = %error,
                "could not check stored secrets at startup"
            );
        }
    }
}

/// Записать в лог статус проверки каждого настроенного слота LLM.
///
/// Статус не блокирует работу: предупреждение помогает заметить непроверенный
/// или устаревший слот до первых разборов. Ошибка чтения базы — тоже только
/// предупреждение. В строках нет адреса, модели и токена: только имя слота и
/// статус.
async fn warn_if_llm_slots_unverified(health: &LlmSlotHealth, pool: &PgPool) {
    match health.refresh(pool, chrono::Utc::now()).await {
        Ok(statuses) => log_llm_slot_statuses(&statuses),
        Err(error) => {
            tracing::warn!(
                error = %error,
                "could not read llm slot check status at startup"
            );
            // Без значения метрики алерт `llm_slot_verified == 0` не сработал бы.
            health.mark_unverified();
        }
    }
}

fn log_llm_slot_statuses(statuses: &[SlotStatusReport]) {
    for report in statuses {
        let titles = report.titles.map(|titles| titles.as_str()).unwrap_or("-");
        match report.status {
            SlotStatus::Passed => {
                tracing::info!(
                    slot = report.slot,
                    status = report.status.code(),
                    titles,
                    "llm slot passed the smoke-set check"
                );
            }
            SlotStatus::Failed { passed, total } => {
                tracing::warn!(
                    slot = report.slot,
                    status = report.status.code(),
                    passed,
                    total,
                    titles,
                    "llm slot check has not passed; capture keeps working with this slot"
                );
            }
            other => {
                tracing::warn!(
                    slot = report.slot,
                    status = other.code(),
                    titles,
                    "llm slot check has not passed; capture keeps working with this slot"
                );
            }
        }
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
        let cipher = SecretCipher::from_config(config);
        let telegram = settings::effective_telegram(config, &[], &cipher);
        tracing::subscriber::with_default(subscriber, || {
            warn_if_bootstrap_owner_missing(config, &telegram);
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
