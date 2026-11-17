//! Группа настроек Telegram: окружение целиком при токене, иначе база.
//!
//! Группа целиком берётся из одного источника. Токен в окружении означает, что
//! все поля приходят из окружения с валидацией запуска; иначе поля читаются из
//! `settings`, а без строк бот выключен. Нерасшифрованный токен или секрет
//! webhook делает ненастроенной всю группу.

use serde_json::Value;
use sqlx::PgPool;

use crate::config::{
    Config, DEFAULT_BOT_USERNAME, SecretString, TelegramConfig, TelegramMode, parse_bot_id,
    parse_bot_username, parse_slot_url, parse_telegram_mode,
};
use crate::settings::crypto::{SecretCipher, SecretError, SettingScope};
use crate::storage::repositories::settings::{self as repository, StoredSetting};

use super::{
    ResolvedSetting, SETTING_KEYS, SecretState, SettingKey, SettingSource, default_setting,
    find_row, json_text, secret_context, setting_text,
};

/// Почему группа Telegram не настроена.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TelegramProblem {
    /// Ни одной строки группы нет ни в окружении, ни в базе.
    NotSet,
    /// Режим `webhook`, но нет секрета или адреса webhook.
    Incomplete {
        /// Ключ, которого не хватает: значение не попадает в причину.
        field: &'static str,
    },
    /// Значение из базы не прошло разбор; имени поля достаточно, значение не
    /// попадает в причину.
    InvalidField {
        /// Имя настройки, значение которой не разобралось.
        field: &'static str,
    },
    /// Токен или секрет webhook есть, но ключа шифрования нет.
    SecretKeyMissing {
        /// Имя секретной настройки, которую нельзя прочитать.
        field: &'static str,
    },
    /// Токен или секрет webhook не расшифровывается текущим или прежним ключом.
    SecretUndecryptable {
        /// Имя секретной настройки, которую нельзя прочитать.
        field: &'static str,
    },
}

impl TelegramProblem {
    /// Короткий код для журнала и консоли.
    pub const fn code(self) -> &'static str {
        match self {
            Self::NotSet => "not_set",
            Self::Incomplete { .. } => "incomplete",
            Self::InvalidField { .. } => "invalid",
            Self::SecretKeyMissing { .. } => "secret_key_missing",
            Self::SecretUndecryptable { .. } => "secret_undecryptable",
        }
    }

    /// Имя ключа, к которому относится причина; без значения.
    pub const fn field(self) -> Option<&'static str> {
        match self {
            Self::NotSet => None,
            Self::Incomplete { field }
            | Self::InvalidField { field }
            | Self::SecretKeyMissing { field }
            | Self::SecretUndecryptable { field } => Some(field),
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
    /// Токен, расшифрованный из выбранного источника; есть и при неполной
    /// группе (например, webhook без секрета), потому что им диагностируют
    /// `getWebhookInfo`.
    bot_token: Option<SecretString>,
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

    /// Расшифрованный токен бота независимо от полноты группы.
    ///
    /// Нужен диагностике `admin webhook info`: неполная группа (нет секрета
    /// или адреса webhook) всё ещё описывает бота, к которому можно обратиться.
    /// Нерасшифрованный или незаданный токен — `None`.
    pub fn bot_token(&self) -> Option<&SecretString> {
        self.bot_token.as_ref()
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
    let bot_token = resolved.token_value.clone();

    EffectiveTelegram {
        state: resolved.state,
        api_base_url: config.telegram_api_base_url.clone(),
        settings,
        bot_token,
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

/// Действующие поля Telegram и источники каждого ключа.
struct ResolvedTelegram {
    state: TelegramState,
    mode: Option<TelegramMode>,
    mode_source: SettingSource,
    token: SecretState,
    /// Расшифрованный токен выбранного источника; есть и у неполной группы.
    token_value: Option<SecretString>,
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
    //
    // Группа из окружения целиком игнорирует строки базы: без
    // `PUBLIC_BASE_URL` адрес не читается из `telegram.public_base_url`, как и
    // `bot_id` с `webhook_url_override`.
    let (public_base_url, public_base_url_source, public_base_invalid) =
        match (&config.public_base_url, env_group.is_some()) {
            (Some(url), _) => (Some(url.clone()), SettingSource::Env, false),
            (None, true) => (None, SettingSource::Default, false),
            (None, false) => match db_url(stored, "telegram.public_base_url", require_https) {
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
                Some(DEFAULT_BOT_USERNAME.to_owned()),
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

    let token_value = bot_token.clone();
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
                            .unwrap_or_else(|| DEFAULT_BOT_USERNAME.to_owned()),
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
                    SecretState::KeyMissing => Some(TelegramProblem::SecretKeyMissing {
                        field: "telegram.bot_token",
                    }),
                    SecretState::Undecryptable => Some(TelegramProblem::SecretUndecryptable {
                        field: "telegram.bot_token",
                    }),
                    SecretState::NotSet | SecretState::Set => None,
                })
                .or(match webhook_secret {
                    SecretState::KeyMissing => Some(TelegramProblem::SecretKeyMissing {
                        field: "telegram.webhook_secret",
                    }),
                    SecretState::Undecryptable => Some(TelegramProblem::SecretUndecryptable {
                        field: "telegram.webhook_secret",
                    }),
                    SecretState::NotSet | SecretState::Set => None,
                });

            let mode = mode.unwrap_or(TelegramMode::Disabled);
            let webhook_url = webhook_override
                .clone()
                .or_else(|| public_base_url.as_ref().map(webhook_address));

            match problem {
                Some(problem) => TelegramState::Unavailable { problem },
                None if require_https && mode == TelegramMode::Polling => {
                    TelegramState::Unavailable {
                        problem: TelegramProblem::InvalidField {
                            field: "telegram.mode",
                        },
                    }
                }
                None if bot_token.is_none() && mode != TelegramMode::Disabled => {
                    TelegramState::Unavailable {
                        problem: TelegramProblem::NotSet,
                    }
                }
                None if mode == TelegramMode::Webhook && webhook_secret != SecretState::Set => {
                    TelegramState::Unavailable {
                        problem: TelegramProblem::Incomplete {
                            field: "telegram.webhook_secret",
                        },
                    }
                }
                None if mode == TelegramMode::Webhook && webhook_url.is_none() => {
                    TelegramState::Unavailable {
                        problem: TelegramProblem::Incomplete {
                            field: "telegram.public_base_url",
                        },
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
                            .unwrap_or_else(|| DEFAULT_BOT_USERNAME.to_owned()),
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
        token_value,
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
        .map(|key| telegram_setting(key, resolved).unwrap_or_else(|| default_setting(key)))
        .collect()
}

/// Источник и значение одного ключа группы; `None` — у ключа нет ветки.
///
/// Отдельная функция нужна, чтобы перечень ключей и разбор не расходились:
/// тест `every_telegram_setting_key_is_resolved_from_the_database` проверяет
/// каждый `telegram.*` из [`SETTING_KEYS`].
fn telegram_setting(
    key: &'static SettingKey,
    resolved: &ResolvedTelegram,
) -> Option<ResolvedSetting> {
    match key.name {
        "telegram.mode" => Some(plain_resolved(
            key,
            resolved
                .mode
                .map(|mode| Value::String(mode.as_str().to_owned())),
            resolved.mode_source,
        )),
        "telegram.bot_id" => Some(plain_resolved(
            key,
            resolved.bot_id.map(Value::from),
            resolved.bot_id_source,
        )),
        "telegram.bot_username" => Some(plain_resolved(
            key,
            resolved.bot_username.clone().map(Value::String),
            resolved.bot_username_source,
        )),
        "telegram.public_base_url" => Some(plain_resolved(
            key,
            resolved
                .public_base_url
                .as_ref()
                .map(|url| Value::String(url.to_string())),
            resolved.public_base_url_source,
        )),
        "telegram.webhook_url_override" => Some(plain_resolved(
            key,
            resolved
                .webhook_override
                .as_ref()
                .map(|url| Value::String(url.to_string())),
            resolved.webhook_override_source,
        )),
        "telegram.bot_token" => Some(ResolvedSetting {
            key: key.name,
            source: resolved.token_source,
            value: None,
            secret: Some(resolved.token),
        }),
        "telegram.webhook_secret" => Some(ResolvedSetting {
            key: key.name,
            source: resolved.webhook_secret_source,
            value: None,
            secret: Some(resolved.webhook_secret),
        }),
        _ => None,
    }
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
