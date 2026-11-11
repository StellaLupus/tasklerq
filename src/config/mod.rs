//! Конфигурация сервиса: чтение переменных окружения и fail-fast валидация.
//!
//! Секреты никогда не выводятся через [`std::fmt::Debug`] и не попадают в
//! сообщения об ошибках: [`SecretString`] печатает `<redacted>`.

mod env_source;
#[cfg(test)]
mod tests;

use std::fmt;
use std::net::SocketAddr;
use std::time::Duration;

use serde::Serialize;
use serde_json::{Map, Value};
use thiserror::Error;
use url::Url;

use crate::VERSION;

/// Минимум `RETENTION_AUDIT_DAYS`: журнал владельца обещает 90 дней.
const MIN_AUDIT_DAYS: u32 = 90;
use env_source::EnvSource;

/// Фильтр журналирования по умолчанию.
pub const DEFAULT_LOG_FILTER: &str = "info,tasklerq=debug";

/// Прочитать `RUST_LOG` из окружения процесса с теми же правилами, что и
/// остальная конфигурация: пробелы обрезаются, пустое значение — умолчание.
pub fn log_filter_from_env() -> String {
    EnvSource::from_process().log_filter()
}

/// Окружение, в котором запущен сервис.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Environment {
    /// Production: HTTPS обязателен, polling запрещён.
    Production,
    /// Локальная разработка и тесты.
    Development,
}

impl Environment {
    /// Строковое представление для журналов.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Production => "production",
            Self::Development => "development",
        }
    }

    /// Production-окружение требует HTTPS и запрещает polling.
    pub const fn is_production(self) -> bool {
        matches!(self, Self::Production)
    }

    fn parse(raw: &str) -> Result<Self, ConfigError> {
        match raw {
            "production" => Ok(Self::Production),
            "development" | "dev" => Ok(Self::Development),
            _ => Err(ConfigError::invalid(
                "ENV",
                "expected `production` or `development`",
            )),
        }
    }
}

/// Режим работы Telegram-бота.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TelegramMode {
    /// Приём обновлений через публичный webhook.
    Webhook,
    /// Long polling; допустим только в development.
    Polling,
    /// Бот полностью отключён; API и capture работают без Telegram.
    Disabled,
}

impl TelegramMode {
    /// Строковое представление для журналов.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Webhook => "webhook",
            Self::Polling => "polling",
            Self::Disabled => "disabled",
        }
    }

    fn parse(raw: &str) -> Result<Self, ConfigError> {
        match raw {
            "webhook" => Ok(Self::Webhook),
            "polling" => Ok(Self::Polling),
            "disabled" => Ok(Self::Disabled),
            _ => Err(ConfigError::invalid(
                "TELEGRAM_MODE",
                "expected `webhook`, `polling` or `disabled`",
            )),
        }
    }
}

/// Режим размещения инсталляции.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TenancyMode {
    /// Один household на инсталляцию.
    Single,
    /// Несколько household; онбординг в боте доступен каждому.
    Multi,
}

impl TenancyMode {
    fn parse(raw: &str) -> Result<Self, ConfigError> {
        match raw {
            "single" => Ok(Self::Single),
            "multi" => Ok(Self::Multi),
            _ => Err(ConfigError::invalid(
                "TENANCY_MODE",
                "expected `single` or `multi`",
            )),
        }
    }
}

/// Секретное значение: `Debug` и `Display` всегда скрывают содержимое.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretString(String);

impl SecretString {
    /// Создать секрет из строки.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Получить исходное значение. Использовать только для подключения и
    /// подписи запросов, не для журналирования.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

impl fmt::Display for SecretString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

/// Адрес для журнала и `Debug`: без учётных данных, query и fragment.
///
/// Неразбираемый адрес заменяется заглушкой: в нём может оказаться секрет,
/// который нельзя выводить.
pub(crate) fn loggable_url(raw: &str) -> String {
    let Ok(mut url) = Url::parse(raw) else {
        return "<invalid url>".to_owned();
    };
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_query(None);
    url.set_fragment(None);
    url.to_string()
}

/// Ошибка конфигурации. Сообщение содержит имя параметра, но не его значение.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ConfigError {
    /// Обязательная переменная отсутствует или пуста.
    #[error("missing required environment variable `{name}`")]
    Missing {
        /// Имя переменной.
        name: &'static str,
    },
    /// Значение переменной не прошло валидацию.
    #[error("invalid environment variable `{name}`: {reason}")]
    Invalid {
        /// Имя переменной.
        name: &'static str,
        /// Описание ожидаемого формата без значения переменной.
        reason: String,
    },
}

impl ConfigError {
    fn invalid(name: &'static str, reason: impl Into<String>) -> Self {
        Self::Invalid {
            name,
            reason: reason.into(),
        }
    }
}

/// Параметры подключения к PostgreSQL.
#[derive(Clone, Debug)]
pub struct DatabaseConfig {
    /// Строка подключения; в журналах всегда скрыта.
    pub url: SecretString,
    /// Максимальный размер пула; допустим диапазон 5..=20.
    pub max_connections: u32,
}

impl DatabaseConfig {
    /// Загрузить только параметры БД. Используется командами `migrate` и `seed`,
    /// которым не нужны Telegram, LLM и публичный URL.
    pub fn from_env() -> Result<Self, ConfigError> {
        let source = EnvSource::from_process();
        Self::from_source(&source)
    }

    fn from_source(source: &EnvSource) -> Result<Self, ConfigError> {
        let raw_url = source.required("DATABASE_URL")?;
        let parsed = Url::parse(raw_url).map_err(|_| {
            ConfigError::invalid(
                "DATABASE_URL",
                "expected a postgres:// connection string, for example \
                 postgres://tasklerq:password@postgres:5432/tasklerq",
            )
        })?;

        if !matches!(parsed.scheme(), "postgres" | "postgresql") {
            return Err(ConfigError::invalid(
                "DATABASE_URL",
                "expected a postgres:// connection string",
            ));
        }
        if parsed.host_str().is_none() {
            return Err(ConfigError::invalid(
                "DATABASE_URL",
                "connection string must contain a host",
            ));
        }

        let max_connections = source.u32("DATABASE_MAX_CONNECTIONS", 10, 5, 20)?;

        Ok(Self {
            url: SecretString::new(raw_url),
            max_connections,
        })
    }
}

/// Параметры Telegram.
#[derive(Clone, Debug)]
pub struct TelegramConfig {
    /// Режим работы.
    pub mode: TelegramMode,
    /// Токен бота; обязателен в режимах `webhook` и `polling`.
    pub bot_token: Option<SecretString>,
    /// Секрет заголовка webhook; обязателен в режиме `webhook`.
    pub webhook_secret: Option<SecretString>,
    /// Публичный URL webhook; обязателен в режиме `webhook`.
    pub webhook_url: Option<Url>,
    /// Базовый адрес Bot API; в тестах подменяется на mock-сервер.
    pub api_base_url: Option<Url>,
    /// Имя бота без `@` для ссылок-приглашений.
    pub bot_username: String,
}

/// Параметры режима размещения.
#[derive(Clone, Copy, Debug)]
pub struct TenancyConfig {
    /// Режим: `single` или `multi`.
    pub mode: TenancyMode,
    /// Telegram ID явно заданного первого владельца.
    pub bootstrap_owner: Option<i64>,
}

impl TenancyConfig {
    /// Загрузить только режим размещения из окружения.
    ///
    /// Нужно административным командам, которым не требуются Telegram, LLM и
    /// остальные параметры сервиса.
    pub fn from_env() -> Result<Self, ConfigError> {
        Config::tenancy_config(&EnvSource::from_process())
    }
}

/// Параметры внешнего capture.
#[derive(Clone, Copy, Debug)]
pub struct CaptureConfig {
    /// Включён ли `POST /api/v1/capture`.
    pub enabled: bool,
    /// Таймаут LLM для голосового capture, миллисекунды.
    pub voice_timeout_ms: u32,
    /// Создавать позицию из исходного текста при сбое LLM.
    pub voice_fallback_raw: bool,
    /// Лимит capture в минуту на токен.
    pub rate_limit_per_min: u32,
    /// Максимальная длина текста.
    pub max_text_chars: u32,
}

/// Параметры одного слота LLM: OpenAI-совместимый адрес и модель.
#[derive(Clone)]
pub struct SlotConfig {
    /// Базовый адрес API, включая `/v1`.
    pub url: Url,
    /// Имя модели.
    pub model: String,
    /// Токен доступа; локальный слот работает без авторизации.
    pub token: Option<SecretString>,
    /// Таймаут одного запроса модели.
    pub timeout: Duration,
    /// Дополнительные параметры тела запроса из `LLM_*_EXTRA_JSON`.
    pub extra: Map<String, Value>,
}

impl fmt::Debug for SlotConfig {
    /// Печатает адрес без учётных данных и query, модель, признак токена и
    /// только имена дополнительных параметров: их значения могут быть ключами
    /// провайдера.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let extra_keys: Vec<&str> = self.extra.keys().map(String::as_str).collect();
        formatter
            .debug_struct("SlotConfig")
            .field("url", &loggable_url(self.url.as_str()))
            .field("model", &self.model)
            .field("has_token", &self.token.is_some())
            .field("timeout", &self.timeout)
            .field("extra_keys", &extra_keys)
            .finish()
    }
}

/// Параметры LLM-capture: слоты, таймаут соединения и квота.
#[derive(Clone, Debug)]
pub struct LlmConfig {
    /// Включён ли разбор текста.
    pub enabled: bool,
    /// Основной слот; обязателен при включённом разборе.
    pub primary: Option<SlotConfig>,
    /// Резервный слот; настраивается целиком или отсутствует.
    pub fallback: Option<SlotConfig>,
    /// Таймаут установки соединения, общий для слотов.
    pub connect_timeout: Duration,
    /// Суточная квота разборов на household.
    pub daily_quota: u32,
}

impl LlmConfig {
    /// Загрузить только параметры LLM-capture из окружения.
    ///
    /// Нужно подкоманде `capture-eval`: остальная конфигурация сервиса ей не
    /// требуется. Слоты читаются и при `LLM_CAPTURE_ENABLED=false`: проверка
    /// слота работает до включения разбора, если запрошенный слот задан.
    pub fn from_env() -> Result<Self, ConfigError> {
        Config::llm_config(&EnvSource::from_process())
    }
}

/// Параметры очередей.
#[derive(Clone, Copy, Debug)]
pub struct QueueConfig {
    /// Максимум попыток обработки обновления.
    pub max_attempts: u32,
    /// Порог задержки очереди для readiness, секунды.
    pub ready_lag_seconds: u32,
}

/// Сроки хранения данных, дни.
#[derive(Clone, Copy, Debug)]
pub struct RetentionConfig {
    /// Входящие Telegram-обновления.
    pub updates_days: u32,
    /// Черновики capture.
    pub drafts_days: u32,
    /// Метаданные LLM.
    pub llm_runs_days: u32,
    /// События задач.
    pub events_days: u32,
    /// Мягко удалённые данные.
    pub deleted_days: u32,
    /// Отправленный outbox.
    pub outbox_days: u32,
    /// События аудита.
    pub audit_days: u32,
}

/// Полная конфигурация сервиса.
#[derive(Clone, Debug)]
pub struct Config {
    /// Окружение.
    pub env: Environment,
    /// Адрес HTTP-сервера.
    pub bind_addr: SocketAddr,
    /// Публичный базовый URL.
    pub public_base_url: Url,
    /// PostgreSQL.
    pub database: DatabaseConfig,
    /// Telegram.
    pub telegram: TelegramConfig,
    /// Режим размещения.
    pub tenancy: TenancyConfig,
    /// Внешний capture.
    pub capture: CaptureConfig,
    /// LLM-capture.
    pub llm: LlmConfig,
    /// Заданные устаревшие переменные окружения (Home Assistant): они больше
    /// не читаются, но и не мешают запуску.
    pub deprecated_env: Vec<&'static str>,
    /// Очереди.
    pub queue: QueueConfig,
    /// Retention.
    pub retention: RetentionConfig,
    /// Фильтр `RUST_LOG`.
    pub log_filter: String,
}

impl Config {
    /// Загрузить конфигурацию из окружения процесса.
    pub fn from_env() -> Result<Self, ConfigError> {
        let source = EnvSource::from_process();
        Self::from_source(&source)
    }

    /// Загрузить конфигурацию из произвольного набора пар. Удобно для тестов.
    pub fn from_map<I, K, V>(vars: I) -> Result<Self, ConfigError>
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        let vars = vars
            .into_iter()
            .map(|(key, value)| (key.into(), value.into()))
            .collect();
        Self::from_source(&EnvSource::from_map(vars))
    }

    fn from_source(source: &EnvSource) -> Result<Self, ConfigError> {
        let environment = Environment::parse(source.required("ENV")?)?;
        let public_base_url =
            source.required_url("PUBLIC_BASE_URL", environment.is_production())?;
        let database = DatabaseConfig::from_source(source)?;

        let bind_addr = source
            .get("BIND_ADDR")
            .unwrap_or("0.0.0.0:8080")
            .parse::<SocketAddr>()
            .map_err(|_| {
                ConfigError::invalid("BIND_ADDR", "expected an address like 0.0.0.0:8080")
            })?;

        let telegram_mode = TelegramMode::parse(source.get("TELEGRAM_MODE").unwrap_or("webhook"))?;
        let telegram = Self::telegram_config(source, telegram_mode, environment)?;

        let tenancy = Self::tenancy_config(source)?;

        let capture = CaptureConfig {
            enabled: source.bool("CAPTURE_API_ENABLED", true)?,
            voice_timeout_ms: source.u32("CAPTURE_VOICE_TIMEOUT_MS", 8_000, 1_000, 60_000)?,
            voice_fallback_raw: source.bool("CAPTURE_VOICE_FALLBACK_RAW", true)?,
            rate_limit_per_min: source.u32("CAPTURE_RATE_LIMIT_PER_MIN", 10, 1, 1_000)?,
            max_text_chars: source.u32("CAPTURE_MAX_TEXT_CHARS", 2_000, 100, 2_000)?,
        };

        let llm = Self::llm_config(source)?;
        let deprecated_env = deprecated_env(source);

        let queue = QueueConfig {
            max_attempts: source.u32("QUEUE_MAX_ATTEMPTS", 10, 1, 100)?,
            ready_lag_seconds: source.u32("QUEUE_READY_LAG_SECONDS", 300, 1, 86_400)?,
        };

        let retention = RetentionConfig {
            updates_days: source.u32("RETENTION_UPDATES_DAYS", 7, 1, 3_650)?,
            drafts_days: source.u32("RETENTION_DRAFTS_DAYS", 30, 1, 3_650)?,
            llm_runs_days: source.u32("RETENTION_LLM_RUNS_DAYS", 90, 1, 3_650)?,
            events_days: source.u32("RETENTION_EVENTS_DAYS", 365, 1, 3_650)?,
            deleted_days: source.u32("RETENTION_DELETED_DAYS", 30, 1, 3_650)?,
            outbox_days: source.u32("RETENTION_OUTBOX_DAYS", 30, 1, 3_650)?,
            // Не меньше глубины журнала владельца (`audit::JOURNAL_DAYS`).
            audit_days: source.u32("RETENTION_AUDIT_DAYS", 365, MIN_AUDIT_DAYS, 3_650)?,
        };

        let log_filter = source.log_filter();

        Ok(Self {
            env: environment,
            bind_addr,
            public_base_url,
            database,
            telegram,
            tenancy,
            capture,
            llm,
            deprecated_env,
            queue,
            retention,
            log_filter,
        })
    }

    fn telegram_config(
        source: &EnvSource,
        mode: TelegramMode,
        environment: Environment,
    ) -> Result<TelegramConfig, ConfigError> {
        let bot_token = source.secret("TELEGRAM_BOT_TOKEN");
        let webhook_secret = source.secret("TELEGRAM_WEBHOOK_SECRET");
        let webhook_url = source.url("TELEGRAM_WEBHOOK_URL", environment.is_production())?;

        match mode {
            TelegramMode::Webhook => {
                if bot_token.is_none() {
                    return Err(ConfigError::Missing {
                        name: "TELEGRAM_BOT_TOKEN",
                    });
                }
                if webhook_secret.is_none() {
                    return Err(ConfigError::Missing {
                        name: "TELEGRAM_WEBHOOK_SECRET",
                    });
                }
                if webhook_url.is_none() {
                    return Err(ConfigError::Missing {
                        name: "TELEGRAM_WEBHOOK_URL",
                    });
                }
            }
            TelegramMode::Polling => {
                if environment.is_production() {
                    return Err(ConfigError::invalid(
                        "TELEGRAM_MODE",
                        "polling is not allowed when ENV=production; use webhook",
                    ));
                }
                if bot_token.is_none() {
                    return Err(ConfigError::Missing {
                        name: "TELEGRAM_BOT_TOKEN",
                    });
                }
            }
            TelegramMode::Disabled => {
                // Бот не нужен; остальные параметры могут отсутствовать.
            }
        }

        let bot_username = source
            .get("TELEGRAM_BOT_USERNAME")
            .unwrap_or("tasklerqbot")
            .trim_start_matches('@')
            .to_owned();
        if bot_username.is_empty()
            || !bot_username
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
        {
            return Err(ConfigError::invalid(
                "TELEGRAM_BOT_USERNAME",
                "expected a Telegram username like tasklerqbot",
            ));
        }

        // Служебный адрес Bot API: вне development разрешён только HTTPS,
        // чтобы токен бота не уходил по открытому каналу.
        let api_base_url = source.url("TELEGRAM_API_BASE_URL", environment.is_production())?;

        Ok(TelegramConfig {
            mode,
            bot_token,
            webhook_secret,
            webhook_url,
            api_base_url,
            bot_username,
        })
    }

    fn tenancy_config(source: &EnvSource) -> Result<TenancyConfig, ConfigError> {
        let mode = TenancyMode::parse(source.get("TENANCY_MODE").unwrap_or("single"))?;
        let bootstrap_owner = match source.get("BOOTSTRAP_OWNER_TELEGRAM_ID") {
            None => None,
            Some(raw) => {
                let value = raw
                    .parse::<i64>()
                    .ok()
                    .filter(|value| *value > 0)
                    .ok_or_else(|| {
                        ConfigError::invalid(
                            "BOOTSTRAP_OWNER_TELEGRAM_ID",
                            "expected a positive integer",
                        )
                    })?;
                Some(value)
            }
        };

        Ok(TenancyConfig {
            mode,
            bootstrap_owner,
        })
    }

    fn llm_config(source: &EnvSource) -> Result<LlmConfig, ConfigError> {
        let enabled = source.bool("LLM_CAPTURE_ENABLED", true)?;
        let primary = read_slot(source, &PRIMARY_SLOT)?;
        let fallback = read_slot(source, &FALLBACK_SLOT)?;

        if enabled && primary.is_none() {
            // Наполовину заданный слот уже дал бы ошибку с именем недостающей
            // переменной, значит здесь не задано ничего.
            return Err(ConfigError::Missing {
                name: PRIMARY_SLOT.url,
            });
        }

        let connect_timeout = Duration::from_millis(u64::from(source.u32(
            "LLM_CONNECT_TIMEOUT_MS",
            1_000,
            10,
            60_000,
        )?));
        let daily_quota = source.u32("LLM_DAILY_QUOTA", 200, 1, 100_000)?;

        Ok(LlmConfig {
            enabled,
            primary,
            fallback,
            connect_timeout,
            daily_quota,
        })
    }

    /// Записать безопасную сводку конфигурации в журнал.
    ///
    /// `SecretString` скрывает значения в `Debug`, поэтому печатать всю
    /// структуру безопасно и не нужно поддерживать отдельную копию полей.
    /// Оставшиеся переменные Home Assistant дают предупреждение без значений:
    /// это упрощает откат на предыдущий образ с тем же env-файлом.
    pub fn log_startup(&self) {
        for name in &self.deprecated_env {
            tracing::warn!(
                variable = *name,
                "environment variable is no longer used and can be removed"
            );
        }
        tracing::info!(
            version = VERSION,
            config = ?self,
            "configuration loaded"
        );
    }
}

/// Имена переменных одного слота LLM.
struct SlotEnv {
    url: &'static str,
    model: &'static str,
    token: &'static str,
    timeout_ms: &'static str,
    extra_json: &'static str,
}

const PRIMARY_SLOT: SlotEnv = SlotEnv {
    url: "LLM_PRIMARY_URL",
    model: "LLM_PRIMARY_MODEL",
    token: "LLM_PRIMARY_TOKEN",
    timeout_ms: "LLM_PRIMARY_TIMEOUT_MS",
    extra_json: "LLM_PRIMARY_EXTRA_JSON",
};

const FALLBACK_SLOT: SlotEnv = SlotEnv {
    url: "LLM_FALLBACK_URL",
    model: "LLM_FALLBACK_MODEL",
    token: "LLM_FALLBACK_TOKEN",
    timeout_ms: "LLM_FALLBACK_TIMEOUT_MS",
    extra_json: "LLM_FALLBACK_EXTRA_JSON",
};

/// Таймаут запроса слота по умолчанию, миллисекунды.
const DEFAULT_SLOT_TIMEOUT_MS: u32 = 10_000;

/// Переменные Home Assistant, которые больше не читаются.
const LEGACY_ENV: [&str; 3] = ["HA_BASE_URL", "HA_TOKEN", "HA_AI_TASK_ENTITY_ID"];

/// Прочитать слот: адрес и модель задаются только вместе.
///
/// Резервный слот настраивается целиком или отсутствует, поэтому половина
/// пары — ошибка запуска с именем недостающей переменной. Основной слот при
/// выключенном разборе не обязателен, но заданный так же проверяется.
fn read_slot(source: &EnvSource, env: &SlotEnv) -> Result<Option<SlotConfig>, ConfigError> {
    let url = source.url(env.url, false)?;
    let model = source.get(env.model).map(ToOwned::to_owned);

    match (url, model) {
        (None, None) => Ok(None),
        (None, Some(_)) => Err(ConfigError::Missing { name: env.url }),
        (Some(_), None) => Err(ConfigError::Missing { name: env.model }),
        (Some(url), Some(model)) => Ok(Some(SlotConfig {
            url,
            model,
            token: source.secret(env.token),
            timeout: Duration::from_millis(u64::from(source.u32(
                env.timeout_ms,
                DEFAULT_SLOT_TIMEOUT_MS,
                100,
                600_000,
            )?)),
            extra: extra_params(source, env.extra_json)?,
        })),
    }
}

/// Разобрать `LLM_*_EXTRA_JSON`: принимается только JSON-объект.
///
/// Текст ошибки называет переменную, но не повторяет её значение: в параметрах
/// слота могут быть ключи провайдера.
fn extra_params(source: &EnvSource, name: &'static str) -> Result<Map<String, Value>, ConfigError> {
    let Some(raw) = source.get(name) else {
        return Ok(Map::new());
    };

    match serde_json::from_str::<Value>(raw) {
        Ok(Value::Object(extra)) => Ok(extra),
        _ => Err(ConfigError::invalid(
            name,
            "expected a JSON object, for example {\"reasoning\":{\"effort\":\"minimal\"}}",
        )),
    }
}

/// Список заданных переменных, которые сервис больше не читает.
fn deprecated_env(source: &EnvSource) -> Vec<&'static str> {
    LEGACY_ENV
        .into_iter()
        .filter(|name| source.get(name).is_some())
        .collect()
}
