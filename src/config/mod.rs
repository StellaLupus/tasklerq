//! Конфигурация сервиса: чтение переменных окружения и fail-fast валидация.
//!
//! Секреты никогда не выводятся через [`std::fmt::Debug`] и не попадают в
//! сообщения об ошибках: [`SecretString`] печатает `<redacted>`.

mod env_source;
mod network;
#[cfg(test)]
mod tests;

use std::fmt;
use std::net::SocketAddr;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::Serialize;
use serde_json::{Map, Value};
use thiserror::Error;
use url::Url;

use crate::VERSION;

pub use network::IpNet;

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

/// Ключ шифрования настроек: ровно 32 байта после base64-декодирования.
///
/// `Debug` и `Display` не показывают ни base64, ни байты: в стартовом логе
/// [`Config`] от ключа остаётся только признак «задан».
#[derive(Clone, PartialEq, Eq)]
pub struct SecretKey([u8; 32]);

impl SecretKey {
    /// Разобрать значение из `TASKLERQ_SECRET_KEY`: стандартный base64.
    ///
    /// Причина ошибки не повторяет значение: это ключ шифрования.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let bytes = BASE64
            .decode(raw)
            .map_err(|_| SECRET_KEY_FORMAT.to_owned())?;
        let bytes: [u8; 32] = bytes.try_into().map_err(|_| SECRET_KEY_FORMAT.to_owned())?;
        Ok(Self(bytes))
    }

    /// Байты ключа для шифрования и расшифровки, не для журналирования.
    pub const fn expose(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for SecretKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

impl fmt::Display for SecretKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

/// Ожидаемый формат ключа шифрования без значения.
const SECRET_KEY_FORMAT: &str =
    "expected base64-encoded 32 bytes, for example the output of `openssl rand -base64 32`";

/// Ключи шифрования секретных настроек: текущий и прежний.
///
/// Разбор тот же, что у полной конфигурации сервиса, но командам, которым не
/// нужны Telegram, LLM и публичный URL, достаточно этого набора.
#[derive(Clone, Debug)]
pub struct SecretKeyConfig {
    /// `TASKLERQ_SECRET_KEY`: им шифруются все записи.
    pub current: Option<SecretKey>,
    /// `TASKLERQ_SECRET_KEY_OLD`: им ещё можно расшифровывать после ротации.
    pub old: Option<SecretKey>,
}

impl SecretKeyConfig {
    /// Загрузить только ключи шифрования из окружения.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_source(&EnvSource::from_process())
    }

    fn from_source(source: &EnvSource) -> Result<Self, ConfigError> {
        Ok(Self {
            current: read_secret_key(source.get("TASKLERQ_SECRET_KEY"), "TASKLERQ_SECRET_KEY")?,
            old: read_secret_key(
                source.get("TASKLERQ_SECRET_KEY_OLD"),
                "TASKLERQ_SECRET_KEY_OLD",
            )?,
        })
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

/// Адрес listener консоли по умолчанию.
const DEFAULT_ADMIN_BIND_ADDR: &str = "0.0.0.0:8081";

/// Разобрать `ADMIN_BIND_ADDR` одинаково для сервиса и CLI.
///
/// Отсутствие значения (в том числе пустое) — адрес по умолчанию, `disabled` —
/// консоль выключена. Ошибка называет параметр, но не повторяет значение.
pub fn parse_admin_bind_addr(raw: Option<&str>) -> Result<Option<SocketAddr>, ConfigError> {
    match raw.map(str::trim).filter(|value| !value.is_empty()) {
        None => Ok(Some(
            DEFAULT_ADMIN_BIND_ADDR
                .parse()
                .expect("the default admin bind address is valid"),
        )),
        Some("disabled") => Ok(None),
        Some(raw) => raw.parse::<SocketAddr>().map(Some).map_err(|_| {
            ConfigError::invalid(
                "ADMIN_BIND_ADDR",
                "expected an address like 0.0.0.0:8081 or `disabled`",
            )
        }),
    }
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
    /// Явно заданный `LLM_CAPTURE_ENABLED`: `Some(..)` только если переменная
    /// есть в окружении. `None` — флаг не задан: он берётся из настроек
    /// консоли, а без них действует умолчание [`LlmConfig::capture_enabled`].
    pub enabled: Option<bool>,
    /// Основной слот; обязателен при явном `LLM_CAPTURE_ENABLED=true`.
    pub primary: Option<SlotConfig>,
    /// Резервный слот; настраивается целиком или отсутствует.
    pub fallback: Option<SlotConfig>,
    /// Таймаут установки соединения, общий для слотов.
    pub connect_timeout: Duration,
    /// Задан ли [`Self::connect_timeout`] явно в окружении. `false` — значение
    /// по умолчанию; хранимая настройка консоли тогда имеет приоритет над ним.
    pub connect_timeout_from_env: bool,
    /// Суточная квота разборов на household.
    pub daily_quota: u32,
    /// Задана ли [`Self::daily_quota`] явно в окружении: как у
    /// [`Self::connect_timeout_from_env`], отличает значение от умолчания.
    pub daily_quota_from_env: bool,
    /// Переменные слотов, заданные без `*_URL` и `*_MODEL`: они игнорируются,
    /// значения не разбирались. Печатаются предупреждением в стартовом логе.
    pub ignored_env: Vec<&'static str>,
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

    /// Включён ли разбор по правилу окружения без учёта базы.
    ///
    /// Явный `LLM_CAPTURE_ENABLED` побеждает. Без переменной действует
    /// умолчание: разбор включён, если основной слот задан окружением, иначе
    /// выключен. Так установки со слотами в `.env` без флага сохраняют прежнее
    /// поведение. После подключения настроек консоли (3.1) база встаёт между
    /// окружением и этим умолчанием; здесь она не участвует.
    pub fn capture_enabled(&self) -> bool {
        self.enabled.unwrap_or(self.primary.is_some())
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
    /// Окружение; по умолчанию `production`.
    pub env: Environment,
    /// Адрес HTTP-сервера.
    pub bind_addr: SocketAddr,
    /// Публичный базовый URL; сейчас в рантайме не используется.
    pub public_base_url: Option<Url>,
    /// Адрес listener консоли; `None` — консоль выключена
    /// (`ADMIN_BIND_ADDR=disabled`).
    pub admin_bind_addr: Option<SocketAddr>,
    /// Сети, от которых консоль принимает `X-Forwarded-*`; пусто — ни от кого.
    pub admin_trusted_proxies: Vec<IpNet>,
    /// Ключ шифрования настроек; `None` — секреты в базе недоступны.
    pub secret_key: Option<SecretKey>,
    /// Прежний ключ шифрования для чтения после ротации.
    pub secret_key_old: Option<SecretKey>,
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
        // Безопасное умолчание: HTTPS-адреса и запрет polling.
        let environment = Environment::parse(source.get("ENV").unwrap_or("production"))?;
        let public_base_url = source.url("PUBLIC_BASE_URL", environment.is_production())?;
        let database = DatabaseConfig::from_source(source)?;

        let bind_addr = source
            .get("BIND_ADDR")
            .unwrap_or("0.0.0.0:8080")
            .parse::<SocketAddr>()
            .map_err(|_| {
                ConfigError::invalid("BIND_ADDR", "expected an address like 0.0.0.0:8080")
            })?;

        let admin_bind_addr = parse_admin_bind_addr(source.get("ADMIN_BIND_ADDR"))?;

        let admin_trusted_proxies = match source.get("ADMIN_TRUSTED_PROXIES") {
            None => Vec::new(),
            Some(raw) => raw
                .split(',')
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(IpNet::parse)
                .collect::<Result<Vec<_>, _>>()
                .map_err(|reason| ConfigError::invalid("ADMIN_TRUSTED_PROXIES", reason))?,
        };

        let keys = SecretKeyConfig::from_source(source)?;
        let secret_key = keys.current;
        let secret_key_old = keys.old;

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
            admin_bind_addr,
            admin_trusted_proxies,
            secret_key,
            secret_key_old,
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
        let enabled = source.optional_bool("LLM_CAPTURE_ENABLED")?;
        let primary = read_slot(source, &PRIMARY_SLOT)?;
        let fallback = read_slot(source, &FALLBACK_SLOT)?;

        if enabled == Some(true) && primary.is_none() {
            // Наполовину заданный слот уже дал бы ошибку с именем недостающей
            // переменной, значит здесь не задано ничего.
            return Err(ConfigError::Missing {
                name: PRIMARY_SLOT.url,
            });
        }

        // Прочие переменные слота без адреса и модели не читаются: невалидный
        // `*_TIMEOUT_MS` не должен ронять запуск.
        let mut ignored_env = Vec::new();
        for (env, slot) in [(&PRIMARY_SLOT, &primary), (&FALLBACK_SLOT, &fallback)] {
            if slot.is_none() {
                ignored_env.extend(ignored_slot_env(source, env));
            }
        }

        let connect_timeout_raw = source.get("LLM_CONNECT_TIMEOUT_MS");
        let connect_timeout = match connect_timeout_raw {
            None => Duration::from_millis(u64::from(DEFAULT_CONNECT_TIMEOUT_MS)),
            Some(raw) => named("LLM_CONNECT_TIMEOUT_MS", parse_connect_timeout_ms(raw))?,
        };
        let daily_quota_raw = source.get("LLM_DAILY_QUOTA");
        let daily_quota = match daily_quota_raw {
            None => DEFAULT_DAILY_QUOTA,
            Some(raw) => named("LLM_DAILY_QUOTA", parse_daily_quota(raw))?,
        };

        Ok(LlmConfig {
            enabled,
            primary,
            fallback,
            connect_timeout,
            connect_timeout_from_env: connect_timeout_raw.is_some(),
            daily_quota,
            daily_quota_from_env: daily_quota_raw.is_some(),
            ignored_env,
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
        for name in &self.llm.ignored_env {
            tracing::warn!(
                variable = *name,
                "LLM slot environment variable is ignored: the slot URL and model are not set"
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
pub const DEFAULT_SLOT_TIMEOUT_MS: u32 = 10_000;

/// Границы таймаута запроса слота, миллисекунды.
const SLOT_TIMEOUT_MS_MIN: u32 = 100;
const SLOT_TIMEOUT_MS_MAX: u32 = 600_000;

/// Границы таймаута установки соединения, миллисекунды.
const CONNECT_TIMEOUT_MS_MIN: u32 = 10;
const CONNECT_TIMEOUT_MS_MAX: u32 = 60_000;
/// Умолчание таймаута установки соединения, миллисекунды.
pub const DEFAULT_CONNECT_TIMEOUT_MS: u32 = 1_000;

/// Умолчание суточной квоты разборов.
pub const DEFAULT_DAILY_QUOTA: u32 = 200;
const DAILY_QUOTA_MIN: u32 = 1;
const DAILY_QUOTA_MAX: u32 = 100_000;

/// Переменные Home Assistant, которые больше не читаются.
const LEGACY_ENV: [&str; 3] = ["HA_BASE_URL", "HA_TOKEN", "HA_AI_TASK_ENTITY_ID"];

/// Прочитать слот: адрес и модель задаются только вместе.
///
/// Слот считается заданным окружением, только если задан его адрес или модель.
/// Резервный слот настраивается целиком или отсутствует, поэтому половина
/// пары — ошибка запуска с именем недостающей переменной. Основной слот при
/// выключенном или незаданном разборе не обязателен, но заданный так же
/// проверяется.
fn read_slot(source: &EnvSource, env: &SlotEnv) -> Result<Option<SlotConfig>, ConfigError> {
    // Адрес разбирается до проверки пары: невалидный URL с недостающей моделью
    // и раньше давал ошибку адреса.
    let url = match source.get(env.url) {
        Some(raw) => Some(named(env.url, parse_slot_url(raw))?),
        None => None,
    };
    let model = source.get(env.model);

    match (url, model) {
        (None, None) => Ok(None),
        (None, Some(_)) => Err(ConfigError::Missing { name: env.url }),
        (Some(_), None) => Err(ConfigError::Missing { name: env.model }),
        (Some(url), Some(model)) => Ok(Some(SlotConfig {
            url,
            model: model.to_owned(),
            token: source.secret(env.token),
            timeout: match source.get(env.timeout_ms) {
                Some(raw) => named(env.timeout_ms, parse_slot_timeout_ms(raw))?,
                None => Duration::from_millis(u64::from(DEFAULT_SLOT_TIMEOUT_MS)),
            },
            extra: match source.get(env.extra_json) {
                Some(raw) => named(env.extra_json, parse_extra_json(raw))?,
                None => Map::new(),
            },
        })),
    }
}

/// Переменные слота, заданные без адреса и модели: их значения не читаются.
fn ignored_slot_env(source: &EnvSource, env: &SlotEnv) -> Vec<&'static str> {
    [env.token, env.timeout_ms, env.extra_json]
        .into_iter()
        .filter(|name| source.get(name).is_some())
        .collect()
}

/// Обернуть причину разбора в ошибку с именем параметра.
fn named<T>(name: &'static str, parsed: Result<T, String>) -> Result<T, ConfigError> {
    parsed.map_err(|reason| ConfigError::invalid(name, reason))
}

fn read_secret_key(
    raw: Option<&str>,
    name: &'static str,
) -> Result<Option<SecretKey>, ConfigError> {
    raw.map(|raw| named(name, SecretKey::parse(raw)))
        .transpose()
}

/// Разобрать адрес слота LLM: правила прежние, причина без значения.
///
/// Используется и `from_env`, и валидацией полей консоли (6.3).
pub fn parse_slot_url(raw: &str) -> Result<Url, String> {
    env_source::parse_url_reason(raw, false).map_err(str::to_owned)
}

/// Разобрать `LLM_*_TIMEOUT_MS` / значение поля консоли: границы прежние.
pub fn parse_slot_timeout_ms(raw: &str) -> Result<Duration, String> {
    bounded_u32(raw, SLOT_TIMEOUT_MS_MIN, SLOT_TIMEOUT_MS_MAX)
        .map(|value| Duration::from_millis(u64::from(value)))
}

/// Разобрать `LLM_CONNECT_TIMEOUT_MS` / значение поля консоли.
pub fn parse_connect_timeout_ms(raw: &str) -> Result<Duration, String> {
    bounded_u32(raw, CONNECT_TIMEOUT_MS_MIN, CONNECT_TIMEOUT_MS_MAX)
        .map(|value| Duration::from_millis(u64::from(value)))
}

/// Разобрать `LLM_DAILY_QUOTA` / значение поля консоли.
pub fn parse_daily_quota(raw: &str) -> Result<u32, String> {
    bounded_u32(raw, DAILY_QUOTA_MIN, DAILY_QUOTA_MAX)
}

/// Разобрать `LLM_*_EXTRA_JSON`: принимается только JSON-объект.
///
/// Текст ошибки не повторяет значение: в параметрах слота могут быть ключи
/// провайдера.
pub fn parse_extra_json(raw: &str) -> Result<Map<String, Value>, String> {
    match serde_json::from_str::<Value>(raw) {
        Ok(Value::Object(extra)) => Ok(extra),
        _ => Err(
            "expected a JSON object, for example {\"reasoning\":{\"effort\":\"minimal\"}}"
                .to_owned(),
        ),
    }
}

fn bounded_u32(raw: &str, min: u32, max: u32) -> Result<u32, String> {
    let reason = || format!("expected an integer in range {min}..={max}");
    let value = raw.parse::<u32>().map_err(|_| reason())?;
    if !(min..=max).contains(&value) {
        return Err(reason());
    }
    Ok(value)
}

/// Список заданных переменных, которые сервис больше не читает.
fn deprecated_env(source: &EnvSource) -> Vec<&'static str> {
    LEGACY_ENV
        .into_iter()
        .filter(|name| source.get(name).is_some())
        .collect()
}
