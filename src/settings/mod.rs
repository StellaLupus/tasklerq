//! Настройки консоли администратора: перечень ключей, хранение и источники.
//!
//! Значения собираются из трёх слоёв: окружение, база (`settings`) и умолчания.
//! Перечень ключей фиксирован в коде: неизвестные строки таблицы игнорируются,
//! чтобы откат на старый образ не ломал чтение. Обычные значения лежат в
//! `value` (jsonb), секреты — в `secret` (шифротекст [`crypto`]); для сборки
//! действующей конфигурации LLM есть [`effective_llm`].
//!
//! Приоритет окружения над базой действует и для отдельного поля, и для слота
//! целиком: если слот задан окружением, значения базы для него не используются.
//! Нерасшифрованный секрет делает весь слот ненастроенным, а не «слотом без
//! токена».

pub mod crypto;

use std::fmt;
use std::time::Duration;

use serde_json::{Map, Value};
use sqlx::{PgConnection, PgPool};
use thiserror::Error;

use crate::config::{
    DEFAULT_CONNECT_TIMEOUT_MS, DEFAULT_DAILY_QUOTA, DEFAULT_SLOT_TIMEOUT_MS, LlmConfig,
    SecretString, SlotConfig, loggable_url, parse_connect_timeout_ms, parse_daily_quota,
    parse_extra_json, parse_slot_timeout_ms, parse_slot_url,
};
use crate::settings::crypto::{SecretCipher, SecretContext, SecretError, SettingScope};
use crate::storage::repositories::settings::{self as repository, StoredSetting};

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
/// Остальные `wizard.*` добавит 6.1. Ключи уровня household появятся вместе с
/// Telegram-настройками; пока все перечисленные — уровня инсталляции.
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
    ignored_env: Vec<&'static str>,
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

    /// Конфигурация для [`crate::llm::Chain::from_config`].
    ///
    /// Флаг становится явным: цепочка собирается по уже разрешённому правилу
    /// «окружение → база → умолчание» и не пересчитывает его.
    pub fn to_llm_config(&self) -> LlmConfig {
        LlmConfig {
            enabled: Some(self.enabled),
            primary: self.primary.config().cloned(),
            fallback: self.fallback.config().cloned(),
            connect_timeout: self.connect_timeout,
            connect_timeout_from_env: self.connect_timeout_source == SettingSource::Env,
            daily_quota: self.daily_quota,
            daily_quota_from_env: self.daily_quota_source == SettingSource::Env,
            ignored_env: self.ignored_env.clone(),
        }
    }
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
        ignored_env: env.ignored_env.clone(),
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
    let context = SecretContext::installation(descriptor.name);
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
                let context = SecretContext::installation(keys.token);
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
                "wizard.llm_skipped" => plain_from_db_or_default(key, stored),
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
