//! Настройки консоли администратора: перечень ключей, хранение и источники.
//!
//! Значения собираются из трёх слоёв: окружение, база (`settings`) и умолчания.
//! Перечень ключей фиксирован в коде: неизвестные строки таблицы игнорируются,
//! чтобы откат на старый образ не ломал чтение. Обычные значения лежат в
//! `value` (jsonb), секреты — в `secret` (шифротекст [`crypto`]); для сборки
//! действующей конфигурации LLM есть [`effective_llm`], для Telegram —
//! [`effective_telegram`].
//!
//! Приоритет окружения над базой действует и для отдельного поля, и для группы
//! целиком: если группа задана окружением, значения базы для неё не
//! используются. Нерасшифрованный секрет делает всю группу ненастроенной, а не
//! «группой без токена».

pub mod crypto;

use std::fmt;
use std::time::Duration;

use serde_json::{Map, Value};
use sqlx::{PgConnection, PgPool};
use thiserror::Error;

use crate::config::{
    Config, DEFAULT_CONNECT_TIMEOUT_MS, DEFAULT_DAILY_QUOTA, DEFAULT_SLOT_TIMEOUT_MS, LlmConfig,
    SecretString, SlotConfig, TelegramConfig, TelegramMode, loggable_url, parse_bot_id,
    parse_bot_username, parse_connect_timeout_ms, parse_daily_quota, parse_extra_json,
    parse_slot_timeout_ms, parse_slot_url, parse_telegram_mode,
};
use crate::settings::crypto::{SecretCipher, SecretContext, SecretError, SettingScope, key_id};
use crate::storage::repositories::settings::{self as repository, StoredSecret, StoredSetting};

/// Источник действующего значения.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingSource {
    /// Значение из переменных окружения.
    Env,
    /// Значение из таблицы `settings`.
    Db,
    /// Значение по умолчанию.
    Default,
}

impl SettingSource {
    /// Короткое имя для консоли и журнала.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Env => "env",
            Self::Db => "db",
            Self::Default => "default",
        }
    }
}

/// Вид значения настройки.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingKind {
    /// Обычное значение в `value`.
    Plain,
    /// Секрет в `secret`, шифруется ключом `TASKLERQ_SECRET_KEY`.
    Secret,
}

/// Тип значения для консоли и проверки формы.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueKind {
    /// Логическое значение.
    Bool,
    /// Строка: адрес, модель, прочее.
    Text,
    /// Абсолютный `http(s)` URL.
    Url,
    /// Таймаут в миллисекундах.
    TimeoutMs,
    /// Таймаут установки соединения в миллисекундах: у него свои границы.
    ConnectTimeoutMs,
    /// JSON-объект дополнительных параметров запроса.
    ExtraJson,
    /// Целое число — суточная квота.
    Quota,
    /// Режим Telegram: `webhook`, `polling` или `disabled`.
    TelegramMode,
    /// Положительный идентификатор бота.
    BotId,
    /// Имя бота без `@`: латинские буквы, цифры и `_`.
    BotUsername,
}

/// Описание известного ключа настройки.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SettingKey {
    /// Имя в `settings.key`.
    pub name: &'static str,
    /// Уровень строки.
    pub scope: SettingScope,
    /// Обычное значение или секрет.
    pub kind: SettingKind,
    /// Ожидаемая форма значения.
    pub value_kind: ValueKind,
}

impl SettingKey {
    /// Это секрет: значение никогда не показывается.
    pub const fn is_secret(&self) -> bool {
        matches!(self.kind, SettingKind::Secret)
    }
}

/// Перечень ключей настроек, которые знает эта версия.
///
/// Все перечисленные ключи — уровня инсталляции: LLM, Telegram и шаги мастера.
/// `wizard.*_skipped` и `telegram.*` читаются только из базы: окружение задаёт
/// их своими переменными, а не ключами настроек.
pub const SETTING_KEYS: &[SettingKey] = &[
    SettingKey {
        name: "llm.enabled",
        scope: SettingScope::Installation,
        kind: SettingKind::Plain,
        value_kind: ValueKind::Bool,
    },
    SettingKey {
        name: "llm.primary.url",
        scope: SettingScope::Installation,
        kind: SettingKind::Plain,
        value_kind: ValueKind::Url,
    },
    SettingKey {
        name: "llm.primary.model",
        scope: SettingScope::Installation,
        kind: SettingKind::Plain,
        value_kind: ValueKind::Text,
    },
    SettingKey {
        name: "llm.primary.token",
        scope: SettingScope::Installation,
        kind: SettingKind::Secret,
        value_kind: ValueKind::Text,
    },
    SettingKey {
        name: "llm.primary.timeout_ms",
        scope: SettingScope::Installation,
        kind: SettingKind::Plain,
        value_kind: ValueKind::TimeoutMs,
    },
    SettingKey {
        name: "llm.primary.extra_json",
        scope: SettingScope::Installation,
        kind: SettingKind::Plain,
        value_kind: ValueKind::ExtraJson,
    },
    SettingKey {
        name: "llm.fallback.url",
        scope: SettingScope::Installation,
        kind: SettingKind::Plain,
        value_kind: ValueKind::Url,
    },
    SettingKey {
        name: "llm.fallback.model",
        scope: SettingScope::Installation,
        kind: SettingKind::Plain,
        value_kind: ValueKind::Text,
    },
    SettingKey {
        name: "llm.fallback.token",
        scope: SettingScope::Installation,
        kind: SettingKind::Secret,
        value_kind: ValueKind::Text,
    },
    SettingKey {
        name: "llm.fallback.timeout_ms",
        scope: SettingScope::Installation,
        kind: SettingKind::Plain,
        value_kind: ValueKind::TimeoutMs,
    },
    SettingKey {
        name: "llm.fallback.extra_json",
        scope: SettingScope::Installation,
        kind: SettingKind::Plain,
        value_kind: ValueKind::ExtraJson,
    },
    SettingKey {
        name: "llm.connect_timeout_ms",
        scope: SettingScope::Installation,
        kind: SettingKind::Plain,
        value_kind: ValueKind::ConnectTimeoutMs,
    },
    SettingKey {
        name: "llm.daily_quota",
        scope: SettingScope::Installation,
        kind: SettingKind::Plain,
        value_kind: ValueKind::Quota,
    },
    SettingKey {
        name: "wizard.llm_skipped",
        scope: SettingScope::Installation,
        kind: SettingKind::Plain,
        value_kind: ValueKind::Bool,
    },
    SettingKey {
        name: "wizard.household_skipped",
        scope: SettingScope::Installation,
        kind: SettingKind::Plain,
        value_kind: ValueKind::Bool,
    },
    SettingKey {
        name: "wizard.token_skipped",
        scope: SettingScope::Installation,
        kind: SettingKind::Plain,
        value_kind: ValueKind::Bool,
    },
    SettingKey {
        name: "telegram.mode",
        scope: SettingScope::Installation,
        kind: SettingKind::Plain,
        value_kind: ValueKind::TelegramMode,
    },
    SettingKey {
        name: "telegram.bot_token",
        scope: SettingScope::Installation,
        kind: SettingKind::Secret,
        value_kind: ValueKind::Text,
    },
    SettingKey {
        name: "telegram.bot_id",
        scope: SettingScope::Installation,
        kind: SettingKind::Plain,
        value_kind: ValueKind::BotId,
    },
    SettingKey {
        name: "telegram.bot_username",
        scope: SettingScope::Installation,
        kind: SettingKind::Plain,
        value_kind: ValueKind::BotUsername,
    },
    SettingKey {
        name: "telegram.public_base_url",
        scope: SettingScope::Installation,
        kind: SettingKind::Plain,
        value_kind: ValueKind::Url,
    },
    SettingKey {
        name: "telegram.webhook_url_override",
        scope: SettingScope::Installation,
        kind: SettingKind::Plain,
        value_kind: ValueKind::Url,
    },
    SettingKey {
        name: "telegram.webhook_secret",
        scope: SettingScope::Installation,
        kind: SettingKind::Secret,
        value_kind: ValueKind::Text,
    },
];

/// Найти ключ в фиксированном перечне.
pub fn setting_key(name: &str) -> Option<&'static SettingKey> {
    SETTING_KEYS.iter().find(|key| key.name == name)
}

/// Состояние секретного поля для консоли.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecretState {
    /// Секрет не задан ни одним слоем.
    NotSet,
    /// Секрет задан и расшифровывается.
    Set,
    /// Ключа шифрования нет: секрет нельзя прочитать.
    KeyMissing,
    /// Секрет не расшифровывается текущим или прежним ключом.
    Undecryptable,
}

impl SecretState {
    /// Секрет пригоден для запроса.
    pub const fn is_set(self) -> bool {
        matches!(self, Self::Set)
    }

    /// Короткое имя для консоли и журнала.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotSet => "not_set",
            Self::Set => "set",
            Self::KeyMissing => "key_missing",
            Self::Undecryptable => "undecryptable",
        }
    }
}

/// Причина, по которой сохранённые секреты нельзя прочитать.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecretProblem {
    /// Ни текущего, ни прежнего ключа нет.
    KeyMissing,
    /// Хотя бы одна строка не расшифровывается заданными ключами.
    Undecryptable,
}

impl SecretProblem {
    /// Код предупреждения readiness и стартового лога.
    pub const fn code(self) -> &'static str {
        match self {
            Self::KeyMissing => "secret_key_missing",
            Self::Undecryptable => "secret_undecryptable",
        }
    }
}

/// Состояние зашифрованных настроек в базе.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SecretHealth {
    /// Число строк с `secret IS NOT NULL`.
    secrets: usize,
    /// Причина, по которой их нельзя прочитать; `None` — всё читается.
    problem: Option<SecretProblem>,
}

impl SecretHealth {
    /// Число зашифрованных строк.
    pub const fn secrets(self) -> usize {
        self.secrets
    }

    /// Причина, по которой секреты нельзя прочитать.
    pub const fn problem(self) -> Option<SecretProblem> {
        self.problem
    }
}

/// Почему слот LLM не настроен.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotProblem {
    /// Ни одного поля слота нет ни в окружении, ни в базе.
    NotSet,
    /// В базе есть адрес без модели или модель без адреса.
    Incomplete,
    /// Значение из базы не прошло разбор; имени поля достаточно, значение не
    /// попадает в причину.
    InvalidField {
        /// Имя настройки, значение которой не разобралось.
        field: &'static str,
    },
    /// Секрет есть, но ключа шифрования нет.
    SecretKeyMissing,
    /// Секрет есть, но не расшифровывается текущим или прежним ключом.
    SecretUndecryptable,
}

impl SlotProblem {
    /// Короткий код для readiness и журнала.
    pub const fn code(self) -> &'static str {
        match self {
            Self::NotSet => "not_set",
            Self::Incomplete => "incomplete",
            Self::InvalidField { .. } => "invalid",
            Self::SecretKeyMissing => "secret_key_missing",
            Self::SecretUndecryptable => "secret_undecryptable",
        }
    }
}

/// Один известный ключ с действующим значением и источником.
///
/// У секрета `value` всегда `None`: показывается только [`SecretState`].
/// `Debug` скрывает значения `extra_json` (в них могут быть ключи провайдера) и
/// учётные данные/query адреса; сам [`Self::value`] доступен консоли.
#[derive(Clone, PartialEq)]
pub struct ResolvedSetting {
    /// Имя настройки.
    pub key: &'static str,
    /// Откуда взято действующее значение.
    pub source: SettingSource,
    /// Значение без секретов; `None` у секретов и незаданных ключей.
    pub value: Option<Value>,
    /// Состояние секрета; `Some` только у секретных ключей.
    pub secret: Option<SecretState>,
}

impl fmt::Debug for ResolvedSetting {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = formatter.debug_struct("ResolvedSetting");
        debug.field("key", &self.key).field("source", &self.source);

        if self.key.ends_with(".extra_json") {
            let keys: Option<Vec<&str>> = self.value.as_ref().map(|value| match value {
                Value::Object(map) => map.keys().map(String::as_str).collect(),
                _ => Vec::new(),
            });
            debug.field("value_keys", &keys);
        } else if self.key.ends_with(".url") {
            let value = self.value.as_ref().map(|value| match value {
                Value::String(raw) => loggable_url(raw),
                other => loggable_url(&other.to_string()),
            });
            debug.field("value", &value);
        } else {
            debug.field("value", &self.value);
        }

        debug.field("secret", &self.secret).finish()
    }
}

/// Действующий слот LLM.
#[derive(Clone, Debug)]
pub enum EffectiveSlot {
    /// Слот задан целиком одним источником.
    Configured {
        /// Источник всех полей слота.
        source: SettingSource,
        /// Разобранные поля.
        config: SlotConfig,
        /// Состояние токена.
        token: SecretState,
    },
    /// Слот не настроен; причина используется readiness и консолью.
    Unavailable {
        /// Причина без значений.
        problem: SlotProblem,
    },
}

impl EffectiveSlot {
    /// Поля слота; `None`, если слот ненастроен.
    pub fn config(&self) -> Option<&SlotConfig> {
        match self {
            Self::Configured { config, .. } => Some(config),
            Self::Unavailable { .. } => None,
        }
    }

    /// Источник полей слота; `None`, если слот ненастроен.
    pub const fn source(&self) -> Option<SettingSource> {
        match self {
            Self::Configured { source, .. } => Some(*source),
            Self::Unavailable { .. } => None,
        }
    }

    /// Слот готов к запросам.
    pub const fn is_configured(&self) -> bool {
        matches!(self, Self::Configured { .. })
    }

    /// Состояние токена слота.
    pub const fn token_state(&self) -> SecretState {
        match self {
            Self::Configured { token, .. } => *token,
            Self::Unavailable { problem } => match problem {
                SlotProblem::SecretKeyMissing => SecretState::KeyMissing,
                SlotProblem::SecretUndecryptable => SecretState::Undecryptable,
                SlotProblem::NotSet
                | SlotProblem::Incomplete
                | SlotProblem::InvalidField { .. } => SecretState::NotSet,
            },
        }
    }

    /// Причина ненастроенного слота.
    pub const fn problem(&self) -> Option<SlotProblem> {
        match self {
            Self::Configured { .. } => None,
            Self::Unavailable { problem } => Some(*problem),
        }
    }
}

/// Действующая конфигурация LLM: окружение, база и умолчания.
///
/// `Debug` не содержит токенов: [`SlotConfig`] печатает только признак токена, а
/// секретные значения в [`Self::settings`] всегда `None`. Значения `extra_json`
/// и учётные данные URL скрывает ручной `Debug` у [`ResolvedSetting`], поэтому
/// журналирование всей структуры не выдаёт ключи провайдера.
#[derive(Clone, Debug)]
pub struct EffectiveLlm {
    enabled: bool,
    enabled_source: SettingSource,
    primary: EffectiveSlot,
    fallback: EffectiveSlot,
    connect_timeout: Duration,
    connect_timeout_source: SettingSource,
    daily_quota: u32,
    daily_quota_source: SettingSource,
    settings: Vec<ResolvedSetting>,
}

impl EffectiveLlm {
    /// Включён ли разбор.
    pub const fn enabled(&self) -> bool {
        self.enabled
    }

    /// Источник флага.
    pub const fn enabled_source(&self) -> SettingSource {
        self.enabled_source
    }

    /// Основной слот.
    pub const fn primary(&self) -> &EffectiveSlot {
        &self.primary
    }

    /// Резервный слот.
    pub const fn fallback(&self) -> &EffectiveSlot {
        &self.fallback
    }

    /// Таймаут установки соединения.
    pub const fn connect_timeout(&self) -> Duration {
        self.connect_timeout
    }

    /// Источник таймаута соединения.
    pub const fn connect_timeout_source(&self) -> SettingSource {
        self.connect_timeout_source
    }

    /// Суточная квота разборов.
    pub const fn daily_quota(&self) -> u32 {
        self.daily_quota
    }

    /// Источник квоты.
    pub const fn daily_quota_source(&self) -> SettingSource {
        self.daily_quota_source
    }

    /// Все известные ключи с источниками; секреты без значений.
    pub fn settings(&self) -> &[ResolvedSetting] {
        &self.settings
    }
}

/// Почему группа Telegram не настроена.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TelegramProblem {
    /// Ни одной строки группы нет ни в окружении, ни в базе.
    NotSet,
    /// Режим `webhook`, но нет секрета или адреса webhook.
    Incomplete,
    /// Значение из базы не прошло разбор; имени поля достаточно, значение не
    /// попадает в причину.
    InvalidField {
        /// Имя настройки, значение которой не разобралось.
        field: &'static str,
    },
    /// Токен или секрет webhook есть, но ключа шифрования нет.
    SecretKeyMissing,
    /// Токен или секрет webhook не расшифровывается текущим или прежним ключом.
    SecretUndecryptable,
}

impl TelegramProblem {
    /// Короткий код для журнала и консоли.
    pub const fn code(self) -> &'static str {
        match self {
            Self::NotSet => "not_set",
            Self::Incomplete => "incomplete",
            Self::InvalidField { .. } => "invalid",
            Self::SecretKeyMissing => "secret_key_missing",
            Self::SecretUndecryptable => "secret_undecryptable",
        }
    }
}

/// Действующая конфигурация Telegram: окружение, база и публичный адрес.
///
/// Группа целиком берётся из одного источника. `Debug` не содержит токена и
/// секрета webhook: [`TelegramConfig`] хранит их в [`SecretString`], а у
/// [`ResolvedSetting`] секретные значения всегда `None`.
#[derive(Clone, Debug)]
pub struct EffectiveTelegram {
    state: TelegramState,
    api_base_url: Option<url::Url>,
    settings: Vec<ResolvedSetting>,
}

#[derive(Clone, Debug)]
enum TelegramState {
    /// Группа задана целиком одним источником.
    Configured {
        /// Источник всех полей группы.
        source: SettingSource,
        /// Разобранные поля.
        config: TelegramConfig,
    },
    /// Группа не настроена; причина используется журналом и консолью.
    Unavailable {
        /// Причина без значений.
        problem: TelegramProblem,
    },
}

impl EffectiveTelegram {
    /// Поля группы; `None`, если группа ненастроена.
    pub fn config(&self) -> Option<&TelegramConfig> {
        match &self.state {
            TelegramState::Configured { config, .. } => Some(config),
            TelegramState::Unavailable { .. } => None,
        }
    }

    /// Источник полей группы; `None`, если группа ненастроена.
    pub const fn source(&self) -> Option<SettingSource> {
        match &self.state {
            TelegramState::Configured { source, .. } => Some(*source),
            TelegramState::Unavailable { .. } => None,
        }
    }

    /// Группа настроена (даже если режим `disabled`).
    pub const fn is_configured(&self) -> bool {
        matches!(&self.state, TelegramState::Configured { .. })
    }

    /// Бот включён: группа настроена и режим не `disabled`.
    pub fn is_enabled(&self) -> bool {
        self.config().is_some_and(|config| config.mode.is_enabled())
    }

    /// Действующий режим; у ненастроенной группы — `disabled`.
    pub fn mode(&self) -> TelegramMode {
        self.config()
            .map_or(TelegramMode::Disabled, |config| config.mode)
    }

    /// Служебный адрес Bot API из окружения.
    pub fn api_base_url(&self) -> Option<&url::Url> {
        self.api_base_url.as_ref()
    }

    /// Причина ненастроенной группы.
    pub const fn problem(&self) -> Option<TelegramProblem> {
        match &self.state {
            TelegramState::Configured { .. } => None,
            TelegramState::Unavailable { problem } => Some(*problem),
        }
    }

    /// Все известные ключи с источниками; секреты без значений.
    pub fn settings(&self) -> &[ResolvedSetting] {
        &self.settings
    }
}

/// Собрать действующую конфигурацию Telegram из окружения и уже прочитанных
/// строк.
///
/// Чистая функция: база прочитана заранее, расшифровка идёт через `cipher`.
/// Группа целиком берётся из одного источника: токен в окружении — вся группа
/// из окружения, иначе из базы, иначе бот выключен. Нерасшифрованный токен или
/// секрет webhook делает ненастроенной всю группу.
pub fn effective_telegram(
    config: &Config,
    stored: &[StoredSetting],
    cipher: &SecretCipher,
) -> EffectiveTelegram {
    let resolved = resolve_telegram(config, stored, cipher);
    let settings = telegram_settings_from(&resolved);

    EffectiveTelegram {
        state: resolved.state,
        api_base_url: config.telegram_api_base_url.clone(),
        settings,
    }
}

/// Прочитать настройки инсталляции и собрать действующую конфигурацию Telegram.
pub async fn read_effective_telegram(
    pool: &PgPool,
    config: &Config,
    cipher: &SecretCipher,
) -> Result<EffectiveTelegram, sqlx::Error> {
    let mut conn = pool.acquire().await?;
    let stored = repository::all(&mut conn, SettingScope::Installation.as_str(), None).await?;
    Ok(effective_telegram(config, &stored, cipher))
}

/// Собрать действующую конфигурацию LLM из окружения и уже прочитанных строк.
///
/// Чистая функция: база прочитана заранее, расшифровка идёт через `cipher`.
/// Слот целиком берётся из одного источника. В базе слот настроен, только если
/// есть и адрес, и модель; невалидное значение делает его ненастроенным с
/// причиной [`SlotProblem::InvalidField`] без значения. Нерасшифрованный токен
/// делает ненастроенным весь слот.
pub fn effective_llm(
    env: &LlmConfig,
    stored: &[StoredSetting],
    cipher: &SecretCipher,
) -> EffectiveLlm {
    let resolved = resolve_llm(env, stored, cipher);
    let settings = settings_from(stored, &resolved);

    EffectiveLlm {
        enabled: resolved.enabled,
        enabled_source: resolved.enabled_source,
        primary: resolved.primary,
        fallback: resolved.fallback,
        connect_timeout: resolved.connect_timeout,
        connect_timeout_source: resolved.connect_timeout_source,
        daily_quota: resolved.daily_quota,
        daily_quota_source: resolved.daily_quota_source,
        settings,
    }
}

/// Разрешённые значения LLM без вида для консоли.
struct ResolvedLlm {
    enabled: bool,
    enabled_source: SettingSource,
    primary: EffectiveSlot,
    fallback: EffectiveSlot,
    connect_timeout: Duration,
    connect_timeout_source: SettingSource,
    daily_quota: u32,
    daily_quota_source: SettingSource,
}

fn resolve_llm(env: &LlmConfig, stored: &[StoredSetting], cipher: &SecretCipher) -> ResolvedLlm {
    let (enabled, enabled_source) = resolve_enabled(env, stored);
    let (connect_timeout, connect_timeout_source) = resolve_connect_timeout(env, stored);
    let (daily_quota, daily_quota_source) = resolve_daily_quota(env, stored);

    ResolvedLlm {
        enabled,
        enabled_source,
        primary: resolve_slot(env.primary.as_ref(), stored, cipher, SlotKeys::primary()),
        fallback: resolve_slot(env.fallback.as_ref(), stored, cipher, SlotKeys::fallback()),
        connect_timeout,
        connect_timeout_source,
        daily_quota,
        daily_quota_source,
    }
}

/// Прочитать настройки инсталляции и собрать действующую конфигурацию LLM.
pub async fn read_effective_llm(
    pool: &PgPool,
    env: &LlmConfig,
    cipher: &SecretCipher,
) -> Result<EffectiveLlm, sqlx::Error> {
    let mut conn = pool.acquire().await?;
    let stored = repository::all(&mut conn, SettingScope::Installation.as_str(), None).await?;
    Ok(effective_llm(env, &stored, cipher))
}

/// Проверить, что все зашифрованные настройки читаются заданными ключами.
///
/// Проверяются все строки с `secret IS NOT NULL`, включая неизвестные этой
/// версии ключи: при ротации их тоже придётся перешифровать. Каждая строка
/// расшифровывается со своим контекстом строки (уровень, household, имя
/// настройки), поэтому подменённый шифротекст не пройдёт проверку только по id
/// ключа. Открытый текст сразу отбрасывается.
pub async fn read_secret_health(
    pool: &PgPool,
    cipher: &SecretCipher,
) -> Result<SecretHealth, sqlx::Error> {
    let mut conn = pool.acquire().await?;
    let secrets = repository::secrets(&mut conn).await?;
    Ok(secret_health(&secrets, cipher))
}

fn secret_health(secrets: &[StoredSecret], cipher: &SecretCipher) -> SecretHealth {
    let problem = secrets.iter().find_map(|secret| {
        let context = secret_context(scope_of(&secret.scope), secret.household_id, &secret.key);
        match cipher.decrypt(&context, &secret.secret) {
            Ok(_) => None,
            Err(SecretError::KeyMissing) => Some(SecretProblem::KeyMissing),
            Err(SecretError::Undecryptable) => Some(SecretProblem::Undecryptable),
        }
    });

    SecretHealth {
        secrets: secrets.len(),
        problem,
    }
}

/// Итог ротации ключа шифрования: сколько строк перешифровано.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RotationReport {
    rotated: usize,
    already_current: usize,
}

impl RotationReport {
    /// Число строк, перешифрованных текущим ключом.
    pub const fn rotated(self) -> usize {
        self.rotated
    }

    /// Число строк, уже зашифрованных текущим ключом.
    pub const fn already_current(self) -> usize {
        self.already_current
    }
}

/// Ошибка ротации ключа. Не содержит ни значений, ни шифротекста.
#[derive(Debug, Error)]
pub enum RotationError {
    /// Текущий ключ не задан: перешифровывать нечем.
    #[error(
        "TASKLERQ_SECRET_KEY is not set: set the current encryption key, for example \
         the output of `openssl rand -base64 32`; nothing changed"
    )]
    KeyMissing,
    /// Строку не удалось расшифровать ни одним ключом; транзакция откатится.
    #[error(
        "secret setting {setting} cannot be decrypted by the current or old key; nothing changed"
    )]
    Undecryptable {
        /// Имя настройки и household без значения секрета.
        setting: String,
    },
    /// Ошибка базы.
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

/// Перешифровать все секреты текущим ключом в одной транзакции.
///
/// Строки читаются с `FOR UPDATE`: блокировка исключает конкурентную запись той
/// же строки во время перешифровки (сервис всегда шифрует текущим ключом).
/// Строки, уже зашифрованные текущим ключом, не
/// меняются (в том числе `updated_at`), но всё равно расшифровываются: так
/// подменённый тег обнаруживается и без смены id ключа. Контекст строки
/// строится из её собственных `scope`, `household_id` и `key`, поэтому
/// неизвестные этой версии ключи перешифровываются тоже. Первая же
/// нерасшифровываемая строка откатывает всю транзакцию.
pub async fn rotate_secret_key(
    pool: &PgPool,
    cipher: &SecretCipher,
) -> Result<RotationReport, RotationError> {
    let current_key_id = cipher.current_key_id().ok_or(RotationError::KeyMissing)?;
    let mut tx = pool.begin().await?;
    let secrets = repository::secrets_for_update(&mut tx).await?;
    let mut report = RotationReport::default();

    for secret in &secrets {
        let context = secret_context(scope_of(&secret.scope), secret.household_id, &secret.key);
        let plaintext = cipher
            .decrypt(&context, &secret.secret)
            .map_err(|error| rotation_error(secret, error))?;

        if key_id(&secret.secret) == Some(current_key_id) {
            report.already_current += 1;
            continue;
        }

        let blob = cipher
            .encrypt(&context, &plaintext)
            .map_err(|error| rotation_error(secret, error))?;
        repository::update_secret(
            &mut tx,
            &secret.scope,
            secret.household_id,
            &secret.key,
            &blob,
        )
        .await?;
        report.rotated += 1;
    }

    tx.commit().await?;
    Ok(report)
}

fn rotation_error(secret: &StoredSecret, error: SecretError) -> RotationError {
    match error {
        SecretError::KeyMissing => RotationError::KeyMissing,
        SecretError::Undecryptable => RotationError::Undecryptable {
            setting: rotation_setting_label(secret),
        },
    }
}

fn rotation_setting_label(secret: &StoredSecret) -> String {
    match secret.household_id {
        Some(household_id) => format!("`{}` (household {household_id})", secret.key),
        None => format!("`{}`", secret.key),
    }
}

/// Уровень строки из `settings.scope`.
///
/// CHECK таблицы допускает только `installation` и `household`; всё, что не
/// installation, считается household.
fn scope_of(scope: &str) -> SettingScope {
    if scope == SettingScope::Household.as_str() {
        SettingScope::Household
    } else {
        SettingScope::Installation
    }
}

/// Контекст шифротекста по уровню строки.
///
/// Единственное место построения AAD: запись, чтение слота и проверка
/// readiness используют одну функцию, поэтому уровень из перечня ключей и
/// уровень строки не расходятся. У household-ключа без идентификатора
/// контекста строки не существует, и такой вызов остаётся инсталляционным —
/// согласованность уровня обеспечивает CHECK таблицы при чтении и будущий
/// писатель household-настроек при записи.
fn secret_context(
    scope: SettingScope,
    household_id: Option<uuid::Uuid>,
    key: &str,
) -> SecretContext {
    match (scope, household_id) {
        (SettingScope::Household, Some(household_id)) => {
            SecretContext::household(household_id, key)
        }
        _ => SecretContext::installation(key),
    }
}

/// Записать обычное значение известной настройки.
pub async fn write_value(
    conn: &mut PgConnection,
    key: &str,
    value: Value,
) -> Result<(), SettingError> {
    let descriptor = known_key(key)?;
    if descriptor.is_secret() {
        return Err(SettingError::PlainValueForSecret {
            key: descriptor.name,
        });
    }
    validate_value(descriptor, &value)?;
    repository::set_value(
        conn,
        descriptor.scope.as_str(),
        None,
        descriptor.name,
        &value,
    )
    .await?;
    Ok(())
}

/// Записать секрет известной настройки текущим ключом.
///
/// Без текущего ключа запись отклоняется до обращения к базе: несекретные
/// значения той же формы можно сохранять отдельными вызовами.
pub async fn write_secret(
    conn: &mut PgConnection,
    key: &str,
    value: &SecretString,
    cipher: &SecretCipher,
) -> Result<(), SettingError> {
    let descriptor = known_key(key)?;
    if !descriptor.is_secret() {
        return Err(SettingError::SecretValueForPlain {
            key: descriptor.name,
        });
    }
    if value.expose().trim().is_empty() {
        return Err(SettingError::InvalidValue {
            key: descriptor.name,
            reason: "expected a non-empty string".to_owned(),
        });
    }
    let context = secret_context(descriptor.scope, None, descriptor.name);
    let blob = cipher
        .encrypt(&context, value)
        .map_err(|error| match error {
            SecretError::KeyMissing => SettingError::SecretKeyMissing {
                key: descriptor.name,
            },
            other => SettingError::Cipher {
                key: descriptor.name,
                source: other,
            },
        })?;
    repository::set_secret(
        conn,
        descriptor.scope.as_str(),
        None,
        descriptor.name,
        &blob,
    )
    .await?;
    Ok(())
}

/// Удалить известную настройку; `true`, если строка была.
pub async fn delete(conn: &mut PgConnection, key: &str) -> Result<bool, SettingError> {
    let descriptor = known_key(key)?;
    Ok(repository::delete(conn, descriptor.scope.as_str(), None, descriptor.name).await?)
}

fn known_key(key: &str) -> Result<&'static SettingKey, SettingError> {
    setting_key(key).ok_or_else(|| SettingError::UnknownKey {
        key: key.to_owned(),
    })
}

/// Проверить форму значения теми же парсерами, что и чтение/запуск.
fn validate_value(key: &SettingKey, value: &Value) -> Result<(), SettingError> {
    let reason = match key.value_kind {
        ValueKind::Bool => json_bool(value)
            .is_none()
            .then(|| "expected `true` or `false`".to_owned()),
        ValueKind::Text => match value {
            Value::String(text) if !text.trim().is_empty() => None,
            _ => Some("expected a non-empty string".to_owned()),
        },
        ValueKind::Url => match value {
            Value::String(raw) => parse_slot_url(raw).err(),
            _ => Some("expected an absolute http(s) URL".to_owned()),
        },
        ValueKind::TimeoutMs => match value_text(value) {
            Some(raw) => parse_slot_timeout_ms(&raw).err(),
            None => Some("expected an integer number of milliseconds".to_owned()),
        },
        ValueKind::ConnectTimeoutMs => match value_text(value) {
            Some(raw) => parse_connect_timeout_ms(&raw).err(),
            None => Some("expected an integer number of milliseconds".to_owned()),
        },
        ValueKind::ExtraJson => match value {
            Value::Object(_) => None,
            Value::String(raw) => parse_extra_json(raw).err(),
            _ => Some("expected a JSON object".to_owned()),
        },
        ValueKind::Quota => match value_text(value) {
            Some(raw) => parse_daily_quota(&raw).err(),
            None => Some("expected an integer".to_owned()),
        },
        ValueKind::TelegramMode => match value {
            Value::String(raw) => parse_telegram_mode(raw).err(),
            _ => Some("expected `webhook`, `polling` or `disabled`".to_owned()),
        },
        ValueKind::BotId => match value_text(value) {
            Some(raw) => parse_bot_id(&raw).err(),
            None => Some("expected a positive integer".to_owned()),
        },
        ValueKind::BotUsername => match value {
            Value::String(raw) => parse_bot_username(raw).err(),
            _ => Some("expected a Telegram username like tasklerqbot".to_owned()),
        },
    };

    match reason {
        Some(reason) => Err(SettingError::InvalidValue {
            key: key.name,
            reason,
        }),
        None => Ok(()),
    }
}

fn value_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(_) => Some(value.to_string()),
        _ => None,
    }
}

/// Ошибка записи настройки. Не содержит значений и секретов.
#[derive(Debug, Error)]
pub enum SettingError {
    /// Ключа нет в фиксированном перечне.
    #[error("unknown setting key `{key}`")]
    UnknownKey {
        /// Имя настройки.
        key: String,
    },
    /// Секрет пытались записать обычным значением.
    #[error("setting `{key}` is secret and must be written as a secret value")]
    PlainValueForSecret {
        /// Имя настройки.
        key: &'static str,
    },
    /// Обычное значение пытались записать как секрет.
    #[error("setting `{key}` is not secret and must be written as a plain value")]
    SecretValueForPlain {
        /// Имя настройки.
        key: &'static str,
    },
    /// Значение не прошло разбор по типу ключа; причина без значения.
    #[error("invalid value for setting `{key}`: {reason}")]
    InvalidValue {
        /// Имя настройки.
        key: &'static str,
        /// Описание ожидаемого формата без значения.
        reason: String,
    },
    /// Текущий ключ шифрования не задан: секрет сохранить нельзя.
    #[error("cannot save secret setting `{key}`: a secret encryption key is required")]
    SecretKeyMissing {
        /// Имя настройки.
        key: &'static str,
    },
    /// Ошибка шифрования, кроме отсутствующего ключа.
    #[error("cannot encrypt secret setting `{key}`: {source}")]
    Cipher {
        /// Имя настройки.
        key: &'static str,
        /// Причина без открытого текста.
        #[source]
        source: SecretError,
    },
    /// Ошибка базы.
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

/// Ключи строк одного слота.
#[derive(Clone, Copy)]
struct SlotKeys {
    url: &'static str,
    model: &'static str,
    token: &'static str,
    timeout_ms: &'static str,
    extra_json: &'static str,
}

impl SlotKeys {
    const fn primary() -> Self {
        Self {
            url: "llm.primary.url",
            model: "llm.primary.model",
            token: "llm.primary.token",
            timeout_ms: "llm.primary.timeout_ms",
            extra_json: "llm.primary.extra_json",
        }
    }

    const fn fallback() -> Self {
        Self {
            url: "llm.fallback.url",
            model: "llm.fallback.model",
            token: "llm.fallback.token",
            timeout_ms: "llm.fallback.timeout_ms",
            extra_json: "llm.fallback.extra_json",
        }
    }
}

/// Один слот: окружение целиком, иначе база, иначе ненастроен.
fn resolve_slot(
    env_slot: Option<&SlotConfig>,
    stored: &[StoredSetting],
    cipher: &SecretCipher,
    keys: SlotKeys,
) -> EffectiveSlot {
    if let Some(config) = env_slot {
        return EffectiveSlot::Configured {
            source: SettingSource::Env,
            token: if config.token.is_some() {
                SecretState::Set
            } else {
                SecretState::NotSet
            },
            config: config.clone(),
        };
    }

    let url_row = find_row(stored, keys.url);
    let model_row = find_row(stored, keys.model);
    let has_url = url_row.and_then(|row| row.value.as_ref()).is_some();
    let has_model = model_row.and_then(|row| row.value.as_ref()).is_some();
    match (has_url, has_model) {
        (false, false) => {
            return EffectiveSlot::Unavailable {
                problem: SlotProblem::NotSet,
            };
        }
        (false, true) | (true, false) => {
            return EffectiveSlot::Unavailable {
                problem: SlotProblem::Incomplete,
            };
        }
        (true, true) => {}
    }

    let Some(url_text) = url_row.and_then(setting_text) else {
        return invalid(keys.url);
    };
    let Ok(url) = parse_slot_url(&url_text) else {
        return invalid(keys.url);
    };

    let Some(model_text) = model_row.and_then(|row| match row.value.as_ref() {
        Some(Value::String(text)) => Some(text.clone()),
        _ => None,
    }) else {
        return invalid(keys.model);
    };
    let model = model_text.trim();
    if model.is_empty() {
        return invalid(keys.model);
    }

    let timeout = match find_row(stored, keys.timeout_ms) {
        None => Duration::from_millis(u64::from(DEFAULT_SLOT_TIMEOUT_MS)),
        Some(row) => match row.value.as_ref().map(json_text).as_deref() {
            Some(raw) => match parse_slot_timeout_ms(raw) {
                Ok(timeout) => timeout,
                Err(_) => return invalid(keys.timeout_ms),
            },
            None => return invalid(keys.timeout_ms),
        },
    };

    let extra = match find_row(stored, keys.extra_json) {
        None => Map::new(),
        Some(row) => match row.value.as_ref().map(json_text).as_deref() {
            Some(raw) => match parse_extra_json(raw) {
                Ok(extra) => extra,
                Err(_) => return invalid(keys.extra_json),
            },
            None => return invalid(keys.extra_json),
        },
    };

    let (token, token_state) = match find_row(stored, keys.token) {
        None => (None, SecretState::NotSet),
        Some(row) => match row.secret.as_deref() {
            None => (None, SecretState::NotSet),
            Some(blob) => {
                let context = secret_context(SettingScope::Installation, None, keys.token);
                match cipher.decrypt(&context, blob) {
                    Ok(secret) => (Some(secret), SecretState::Set),
                    Err(SecretError::KeyMissing) => {
                        return EffectiveSlot::Unavailable {
                            problem: SlotProblem::SecretKeyMissing,
                        };
                    }
                    Err(SecretError::Undecryptable) => {
                        return EffectiveSlot::Unavailable {
                            problem: SlotProblem::SecretUndecryptable,
                        };
                    }
                }
            }
        },
    };

    EffectiveSlot::Configured {
        source: SettingSource::Db,
        config: SlotConfig {
            url,
            model: model.to_owned(),
            token,
            timeout,
            extra,
        },
        token: token_state,
    }
}

const fn invalid(field: &'static str) -> EffectiveSlot {
    EffectiveSlot::Unavailable {
        problem: SlotProblem::InvalidField { field },
    }
}

/// Действующие поля Telegram и источники каждого ключа.
struct ResolvedTelegram {
    state: TelegramState,
    mode: Option<TelegramMode>,
    mode_source: SettingSource,
    token: SecretState,
    token_source: SettingSource,
    bot_id: Option<i64>,
    bot_id_source: SettingSource,
    bot_username: Option<String>,
    bot_username_source: SettingSource,
    public_base_url: Option<url::Url>,
    public_base_url_source: SettingSource,
    webhook_override: Option<url::Url>,
    webhook_override_source: SettingSource,
    webhook_secret: SecretState,
    webhook_secret_source: SettingSource,
}

/// Значение адреса из строки настроек.
enum DbUrl {
    /// Строки нет.
    Absent,
    /// Строка разобрана.
    Valid(url::Url),
    /// Строка есть, но значение невалидно.
    Invalid,
}

/// Разобрать адрес из строки настроек; в production — только https.
fn db_url(stored: &[StoredSetting], key: &str, require_https: bool) -> DbUrl {
    let Some(row) = find_row(stored, key) else {
        return DbUrl::Absent;
    };
    let Some(raw) = setting_text(row) else {
        return DbUrl::Invalid;
    };
    match parse_slot_url(&raw) {
        Ok(url) if !require_https || url.scheme() == "https" => DbUrl::Valid(url),
        _ => DbUrl::Invalid,
    }
}

/// Присоединить путь webhook к публичному адресу без двойного `/`.
fn webhook_address(base: &url::Url) -> url::Url {
    let mut url = base.clone();
    url.set_path(&format!(
        "{}/webhooks/telegram",
        url.path().trim_end_matches('/')
    ));
    url.set_query(None);
    url.set_fragment(None);
    url
}

/// Расшифровать секретную строку; пустое значение считается незаданным.
fn decrypt_setting(
    stored: &[StoredSetting],
    key: &'static str,
    cipher: &SecretCipher,
) -> (Option<SecretString>, SecretState) {
    let Some(row) = find_row(stored, key) else {
        return (None, SecretState::NotSet);
    };
    let Some(blob) = row.secret.as_deref() else {
        return (None, SecretState::NotSet);
    };
    let context = secret_context(SettingScope::Installation, None, key);
    match cipher.decrypt(&context, blob) {
        Ok(secret) if !secret.expose().trim().is_empty() => (Some(secret), SecretState::Set),
        Ok(_) => (None, SecretState::NotSet),
        Err(SecretError::KeyMissing) => (None, SecretState::KeyMissing),
        Err(SecretError::Undecryptable) => (None, SecretState::Undecryptable),
    }
}

/// Источник обычного ключа, который лежит в базе.
fn db_source(stored: &[StoredSetting], key: &str) -> SettingSource {
    if stored.iter().any(|row| row.key == key) {
        SettingSource::Db
    } else {
        SettingSource::Default
    }
}

fn resolve_telegram(
    config: &Config,
    stored: &[StoredSetting],
    cipher: &SecretCipher,
) -> ResolvedTelegram {
    let require_https = config.env.is_production();
    let env_group = config.telegram.as_ref();

    // Публичный адрес — общий: `PUBLIC_BASE_URL` приоритетнее настройки.
    let (public_base_url, public_base_url_source, public_base_invalid) =
        match (&config.public_base_url, env_group.is_some()) {
            (Some(url), _) => (Some(url.clone()), SettingSource::Env, false),
            (None, _) => match db_url(stored, "telegram.public_base_url", require_https) {
                DbUrl::Absent => (None, SettingSource::Default, false),
                DbUrl::Valid(url) => (Some(url), SettingSource::Db, false),
                DbUrl::Invalid => (None, SettingSource::Db, true),
            },
        };

    let (mode, mode_source) = match env_group {
        Some(group) => (Some(group.mode), SettingSource::Env),
        None => match find_row(stored, "telegram.mode") {
            None => (Some(TelegramMode::Webhook), SettingSource::Default),
            Some(row) => {
                let parsed = row
                    .value
                    .as_ref()
                    .map(json_text)
                    .and_then(|raw| parse_telegram_mode(&raw).ok());
                (parsed, SettingSource::Db)
            }
        },
    };

    let (bot_token, token, token_source) = match env_group {
        Some(group) => (
            group.bot_token.clone(),
            SecretState::Set,
            SettingSource::Env,
        ),
        None => {
            let (secret, state) = decrypt_setting(stored, "telegram.bot_token", cipher);
            (secret, state, db_source(stored, "telegram.bot_token"))
        }
    };

    let (bot_id, bot_id_source, bot_id_invalid) = match env_group {
        Some(_) => (None, SettingSource::Default, false),
        None => match find_row(stored, "telegram.bot_id") {
            None => (None, SettingSource::Default, false),
            Some(row) => {
                let parsed = row
                    .value
                    .as_ref()
                    .map(json_text)
                    .and_then(|raw| parse_bot_id(&raw).ok());
                (parsed, SettingSource::Db, parsed.is_none())
            }
        },
    };

    let (bot_username, bot_username_source, bot_username_invalid) = match env_group {
        Some(group) => (Some(group.bot_username.clone()), SettingSource::Env, false),
        None => match find_row(stored, "telegram.bot_username") {
            None => (
                Some("tasklerqbot".to_owned()),
                SettingSource::Default,
                false,
            ),
            Some(row) => {
                let parsed = row.value.as_ref().and_then(|value| match value {
                    Value::String(raw) => parse_bot_username(raw).ok(),
                    _ => None,
                });
                let invalid = parsed.is_none();
                (parsed, SettingSource::Db, invalid)
            }
        },
    };

    let (webhook_override, webhook_override_source, webhook_override_invalid) = match env_group {
        Some(_) => (None, SettingSource::Default, false),
        None => match db_url(stored, "telegram.webhook_url_override", require_https) {
            DbUrl::Absent => (None, SettingSource::Default, false),
            DbUrl::Valid(url) => (Some(url), SettingSource::Db, false),
            DbUrl::Invalid => (None, SettingSource::Db, true),
        },
    };

    let (webhook_secret_value, webhook_secret, webhook_secret_source) = match env_group {
        Some(group) => (
            group.webhook_secret.clone(),
            if group.webhook_secret.is_some() {
                SecretState::Set
            } else {
                SecretState::NotSet
            },
            SettingSource::Env,
        ),
        None => {
            let (secret, state) = decrypt_setting(stored, "telegram.webhook_secret", cipher);
            (secret, state, db_source(stored, "telegram.webhook_secret"))
        }
    };

    let state = match env_group {
        Some(group) => {
            // Группа из окружения уже прошла валидацию запуска: токен задан, а
            // для webhook есть секрет и адрес.
            match (mode, bot_token) {
                (Some(mode), Some(token)) => TelegramState::Configured {
                    source: SettingSource::Env,
                    config: TelegramConfig {
                        mode,
                        bot_token: Some(token),
                        webhook_secret: webhook_secret_value,
                        webhook_url: group.webhook_url.clone(),
                        bot_username: bot_username
                            .clone()
                            .unwrap_or_else(|| "tasklerqbot".to_owned()),
                    },
                },
                _ => TelegramState::Unavailable {
                    problem: TelegramProblem::NotSet,
                },
            }
        }
        None => {
            let problem = if mode.is_none() {
                Some(TelegramProblem::InvalidField {
                    field: "telegram.mode",
                })
            } else if bot_id_invalid {
                Some(TelegramProblem::InvalidField {
                    field: "telegram.bot_id",
                })
            } else if bot_username_invalid {
                Some(TelegramProblem::InvalidField {
                    field: "telegram.bot_username",
                })
            } else if public_base_invalid {
                Some(TelegramProblem::InvalidField {
                    field: "telegram.public_base_url",
                })
            } else if webhook_override_invalid {
                Some(TelegramProblem::InvalidField {
                    field: "telegram.webhook_url_override",
                })
            } else {
                None
            };
            let problem = problem
                .or(match token {
                    SecretState::KeyMissing => Some(TelegramProblem::SecretKeyMissing),
                    SecretState::Undecryptable => Some(TelegramProblem::SecretUndecryptable),
                    SecretState::NotSet | SecretState::Set => None,
                })
                .or(match webhook_secret {
                    SecretState::KeyMissing => Some(TelegramProblem::SecretKeyMissing),
                    SecretState::Undecryptable => Some(TelegramProblem::SecretUndecryptable),
                    SecretState::NotSet | SecretState::Set => None,
                });

            let mode = mode.unwrap_or(TelegramMode::Disabled);
            let webhook_url = webhook_override
                .clone()
                .or_else(|| public_base_url.as_ref().map(webhook_address));

            match problem {
                Some(problem) => TelegramState::Unavailable { problem },
                None if mode == TelegramMode::Disabled => TelegramState::Configured {
                    source: SettingSource::Db,
                    config: TelegramConfig {
                        mode,
                        bot_token,
                        webhook_secret: webhook_secret_value,
                        webhook_url,
                        bot_username: bot_username
                            .clone()
                            .unwrap_or_else(|| "tasklerqbot".to_owned()),
                    },
                },
                None if require_https && mode == TelegramMode::Polling => {
                    TelegramState::Unavailable {
                        problem: TelegramProblem::InvalidField {
                            field: "telegram.mode",
                        },
                    }
                }
                None if bot_token.is_none() => TelegramState::Unavailable {
                    problem: TelegramProblem::NotSet,
                },
                None if mode == TelegramMode::Webhook
                    && (webhook_secret != SecretState::Set || webhook_url.is_none()) =>
                {
                    TelegramState::Unavailable {
                        problem: TelegramProblem::Incomplete,
                    }
                }
                None => TelegramState::Configured {
                    source: SettingSource::Db,
                    config: TelegramConfig {
                        mode,
                        bot_token,
                        webhook_secret: webhook_secret_value,
                        webhook_url,
                        bot_username: bot_username
                            .clone()
                            .unwrap_or_else(|| "tasklerqbot".to_owned()),
                    },
                },
            }
        }
    };

    ResolvedTelegram {
        state,
        mode,
        mode_source,
        token,
        token_source,
        bot_id,
        bot_id_source,
        bot_username,
        bot_username_source,
        public_base_url,
        public_base_url_source,
        webhook_override,
        webhook_override_source,
        webhook_secret,
        webhook_secret_source,
    }
}

fn telegram_settings_from(resolved: &ResolvedTelegram) -> Vec<ResolvedSetting> {
    SETTING_KEYS
        .iter()
        .filter(|key| key.name.starts_with("telegram."))
        .map(|key| match key.name {
            "telegram.mode" => plain_resolved(
                key,
                resolved
                    .mode
                    .map(|mode| Value::String(mode.as_str().to_owned())),
                resolved.mode_source,
            ),
            "telegram.bot_id" => plain_resolved(
                key,
                resolved.bot_id.map(Value::from),
                resolved.bot_id_source,
            ),
            "telegram.bot_username" => plain_resolved(
                key,
                resolved.bot_username.clone().map(Value::String),
                resolved.bot_username_source,
            ),
            "telegram.public_base_url" => plain_resolved(
                key,
                resolved
                    .public_base_url
                    .as_ref()
                    .map(|url| Value::String(url.to_string())),
                resolved.public_base_url_source,
            ),
            "telegram.webhook_url_override" => plain_resolved(
                key,
                resolved
                    .webhook_override
                    .as_ref()
                    .map(|url| Value::String(url.to_string())),
                resolved.webhook_override_source,
            ),
            "telegram.bot_token" => ResolvedSetting {
                key: key.name,
                source: resolved.token_source,
                value: None,
                secret: Some(resolved.token),
            },
            "telegram.webhook_secret" => ResolvedSetting {
                key: key.name,
                source: resolved.webhook_secret_source,
                value: None,
                secret: Some(resolved.webhook_secret),
            },
            _ => default_setting(key),
        })
        .collect()
}

fn plain_resolved(
    key: &'static SettingKey,
    value: Option<Value>,
    source: SettingSource,
) -> ResolvedSetting {
    ResolvedSetting {
        key: key.name,
        source,
        value,
        secret: None,
    }
}

/// Флаг: явное окружение, затем база, затем умолчание окружения.
fn resolve_enabled(env: &LlmConfig, stored: &[StoredSetting]) -> (bool, SettingSource) {
    if let Some(enabled) = env.enabled {
        return (enabled, SettingSource::Env);
    }
    if let Some(value) = find_row(stored, "llm.enabled").and_then(|row| row.value.as_ref())
        && let Some(enabled) = json_bool(value)
    {
        return (enabled, SettingSource::Db);
    }
    // Умолчание: включён, если основной слот задан окружением. Слот только из
    // базы сам по себе разбор не включает — иначе обновление меняло бы
    // поведение без `llm.enabled`.
    (env.primary.is_some(), SettingSource::Default)
}

fn resolve_connect_timeout(env: &LlmConfig, stored: &[StoredSetting]) -> (Duration, SettingSource) {
    if env.connect_timeout_from_env {
        return (env.connect_timeout, SettingSource::Env);
    }
    if let Some(raw) = find_row(stored, "llm.connect_timeout_ms").and_then(setting_text)
        && let Ok(timeout) = parse_connect_timeout_ms(&raw)
    {
        return (timeout, SettingSource::Db);
    }
    (
        Duration::from_millis(u64::from(DEFAULT_CONNECT_TIMEOUT_MS)),
        SettingSource::Default,
    )
}

fn resolve_daily_quota(env: &LlmConfig, stored: &[StoredSetting]) -> (u32, SettingSource) {
    if env.daily_quota_from_env {
        return (env.daily_quota, SettingSource::Env);
    }
    if let Some(raw) = find_row(stored, "llm.daily_quota").and_then(setting_text)
        && let Ok(quota) = parse_daily_quota(&raw)
    {
        return (quota, SettingSource::Db);
    }
    (DEFAULT_DAILY_QUOTA, SettingSource::Default)
}

/// Все известные ключи с действующим значением и источником.
///
/// Неизвестные строки `stored` пропускаются: их могла записать более новая
/// версия. Секретные значения в результат не попадают — только [`SecretState`].
/// Значения и источники берутся из тех же разрешённых флага, таймаутов и слотов,
/// что и типизированные поля [`EffectiveLlm`], поэтому умолчания видны консоли.
pub fn resolve_settings(
    env: &LlmConfig,
    stored: &[StoredSetting],
    cipher: &SecretCipher,
) -> Vec<ResolvedSetting> {
    let resolved = resolve_llm(env, stored, cipher);
    settings_from(stored, &resolved)
}

fn settings_from(stored: &[StoredSetting], resolved: &ResolvedLlm) -> Vec<ResolvedSetting> {
    SETTING_KEYS
        .iter()
        .map(|key| {
            if let Some((slot, field)) = slot_of(resolved, key.name) {
                return slot_setting(key, slot, field, stored);
            }

            match key.name {
                "llm.enabled" => ResolvedSetting {
                    key: key.name,
                    source: resolved.enabled_source,
                    value: Some(Value::Bool(resolved.enabled)),
                    secret: None,
                },
                "llm.connect_timeout_ms" => ResolvedSetting {
                    key: key.name,
                    source: resolved.connect_timeout_source,
                    value: Some(Value::from(millis(resolved.connect_timeout))),
                    secret: None,
                },
                "llm.daily_quota" => ResolvedSetting {
                    key: key.name,
                    source: resolved.daily_quota_source,
                    value: Some(Value::from(resolved.daily_quota)),
                    secret: None,
                },
                "wizard.llm_skipped" | "wizard.household_skipped" | "wizard.token_skipped" => {
                    plain_from_db_or_default(key, stored)
                }
                _ => default_setting(key),
            }
        })
        .collect()
}

fn default_setting(key: &'static SettingKey) -> ResolvedSetting {
    ResolvedSetting {
        key: key.name,
        source: SettingSource::Default,
        value: None,
        secret: key.is_secret().then_some(SecretState::NotSet),
    }
}

fn plain_from_db_or_default(key: &'static SettingKey, stored: &[StoredSetting]) -> ResolvedSetting {
    match find_row(stored, key.name) {
        Some(row) => ResolvedSetting {
            key: key.name,
            source: SettingSource::Db,
            value: row.value.clone(),
            secret: None,
        },
        None => default_setting(key),
    }
}

/// Поле слота, к которому относится ключ.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SlotField {
    Url,
    Model,
    Token,
    TimeoutMs,
    ExtraJson,
}

fn slot_of<'a>(resolved: &'a ResolvedLlm, name: &str) -> Option<(&'a EffectiveSlot, SlotField)> {
    let (slot, field) = if let Some(field) = name.strip_prefix("llm.primary.") {
        (&resolved.primary, field)
    } else {
        (&resolved.fallback, name.strip_prefix("llm.fallback.")?)
    };
    let field = match field {
        "url" => SlotField::Url,
        "model" => SlotField::Model,
        "token" => SlotField::Token,
        "timeout_ms" => SlotField::TimeoutMs,
        "extra_json" => SlotField::ExtraJson,
        _ => return None,
    };
    Some((slot, field))
}

fn slot_setting(
    key: &'static SettingKey,
    slot: &EffectiveSlot,
    field: SlotField,
    stored: &[StoredSetting],
) -> ResolvedSetting {
    match slot {
        EffectiveSlot::Configured {
            source,
            config,
            token,
        } => {
            let value = match field {
                SlotField::Url => Some(Value::String(config.url.to_string())),
                SlotField::Model => Some(Value::String(config.model.clone())),
                SlotField::Token => None,
                SlotField::TimeoutMs => Some(Value::from(millis(config.timeout))),
                SlotField::ExtraJson => Some(Value::Object(config.extra.clone())),
            };
            let field_source = if *source == SettingSource::Db
                && matches!(
                    field,
                    SlotField::Token | SlotField::TimeoutMs | SlotField::ExtraJson
                )
                && find_row(stored, key.name).is_none()
            {
                SettingSource::Default
            } else {
                *source
            };
            ResolvedSetting {
                key: key.name,
                source: field_source,
                value,
                secret: (field == SlotField::Token).then_some(*token),
            }
        }
        EffectiveSlot::Unavailable { .. } => {
            let row = find_row(stored, key.name);
            let source = if row.is_some() {
                SettingSource::Db
            } else {
                SettingSource::Default
            };
            let value = if field == SlotField::Token {
                None
            } else if let Some(row) = row {
                row.value.clone()
            } else if field == SlotField::TimeoutMs {
                Some(Value::from(DEFAULT_SLOT_TIMEOUT_MS))
            } else if field == SlotField::ExtraJson {
                Some(Value::Object(Map::new()))
            } else {
                None
            };
            ResolvedSetting {
                key: key.name,
                source,
                value,
                secret: (field == SlotField::Token).then(|| slot.token_state()),
            }
        }
    }
}

fn find_row<'a>(stored: &'a [StoredSetting], key: &str) -> Option<&'a StoredSetting> {
    stored.iter().find(|row| row.key == key)
}

fn setting_text(row: &StoredSetting) -> Option<String> {
    row.value.as_ref().map(json_text)
}

fn json_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn json_bool(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(value) => Some(*value),
        Value::String(text) => match text.as_str() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests;
