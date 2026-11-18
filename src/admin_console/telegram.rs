//! Страница «Telegram» и шаг мастера: подключение бота без перезапуска.
//!
//! `getMe` и `getWebhookInfo` идут отдельным клиентом Bot API с введённым или
//! сохранённым токеном: консоль не трогает текущее поколение до сохранения.
//! После записи настроек вызывается [`TelegramSupervisor::reload`], поэтому
//! webhook или polling начинают работать без перезапуска процесса.
//!
//! Секреты (токен и секрет webhook) не попадают в [`TelegramView`]: страница
//! показывает только их состояние, а значения читаются из базы и окружения.

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;
use url::Url;

use crate::auth::secrets::random_base64url;
use crate::config::{
    Config, DEFAULT_BOT_USERNAME, SecretString, TelegramMode, parse_bot_id, parse_bot_username,
    parse_slot_url, parse_telegram_mode,
};
use crate::domain::error::DomainError;
use crate::settings::webhook_address;
use crate::settings::{
    self, ResolvedSetting, SecretState, SettingError, SettingSource, TelegramProblem,
};
use crate::telegram::api::{DEFAULT_BASE_URL, HttpTelegramApi, TelegramApi};

use super::router::AdminState;

/// Путь страницы Telegram.
pub(crate) const TELEGRAM_PATH: &str = "/admin/telegram";
/// Путь перевыпуска секрета webhook.
pub(crate) const TELEGRAM_SECRET_PATH: &str = "/admin/telegram/secret";
/// Успешное сохранение.
pub(crate) const TELEGRAM_SAVED_PATH: &str = "/admin/telegram?saved=1";
/// Значение скрытого поля формы шага мастера.
pub(crate) const WIZARD_NEXT: &str = "wizard";

/// Токен не принят Telegram: сообщение одинаково для `401`, `404` и сети.
pub(crate) const TOKEN_REJECTED: &str = "Токен не принят Telegram.";
/// В production webhook регистрируется только по HTTPS.
pub(crate) const HTTPS_REQUIRED: &str =
    "В production публичный адрес и адрес webhook должны начинаться с https://.";
/// Токен из окружения перекрывает базу: страница только для чтения.
pub(crate) const ENV_READ_ONLY: &str =
    "Токен задан окружением TELEGRAM_BOT_TOKEN: страница доступна только для чтения.";

/// Поля формы Telegram.
#[derive(Clone, Debug, Default, Deserialize)]
pub(crate) struct TelegramForm {
    /// Режим: `webhook`, `polling` или `disabled`.
    #[serde(default)]
    pub(crate) mode: Option<String>,
    /// Новый токен; пустое поле оставляет сохранённый.
    #[serde(default)]
    pub(crate) token: Option<String>,
    /// Публичный адрес инсталляции.
    #[serde(default)]
    pub(crate) public_base_url: Option<String>,
    /// Переопределение адреса webhook.
    #[serde(default)]
    pub(crate) webhook_url_override: Option<String>,
    /// Возврат в мастер после сохранения.
    #[serde(default)]
    pub(crate) next: Option<String>,
}

impl TelegramForm {
    /// Сохранение запущено из шага мастера.
    pub(crate) fn is_wizard(&self) -> bool {
        self.next.as_deref() == Some(WIZARD_NEXT)
    }
}

/// Текстовое поле страницы: значение, источник и признак «только чтение».
pub(crate) struct TelegramField {
    /// Значение; секретов здесь не бывает.
    pub(crate) value: String,
    /// Подпись источника.
    pub(crate) source: &'static str,
    /// Значение задано окружением.
    pub(crate) readonly: bool,
}

/// Данные страницы Telegram.
pub(crate) struct TelegramView {
    /// Страница открыта как шаг мастера: показать пропуск.
    pub(crate) wizard: bool,
    /// Настройки только что сохранены.
    pub(crate) saved: bool,
    /// Токен задан окружением: формы нет.
    pub(crate) readonly: bool,
    /// Имя бота без `@`; пусто, пока бот не подключён.
    pub(crate) bot_name: String,
    /// Идентификатор бота, если известен.
    pub(crate) bot_id: Option<i64>,
    /// Источник группы: окружение, консоль или «не настроена».
    pub(crate) source: &'static str,
    /// Действующий режим.
    pub(crate) mode: &'static str,
    /// В production polling недоступен.
    pub(crate) polling_allowed: bool,
    /// Выбран режим webhook.
    pub(crate) webhook_selected: bool,
    /// Выбран режим polling.
    pub(crate) polling_selected: bool,
    /// Выбран режим disabled.
    pub(crate) disabled_selected: bool,
    /// Состояние токена без значения.
    pub(crate) token_label: &'static str,
    /// Источник токена.
    pub(crate) token_source: &'static str,
    /// Публичный адрес.
    pub(crate) public_base_url: TelegramField,
    /// Переопределение адреса webhook.
    pub(crate) webhook_url_override: TelegramField,
    /// Адрес, который зарегистрирован или будет зарегистрирован.
    pub(crate) webhook_address: String,
    /// Секрет webhook задан.
    pub(crate) webhook_secret_set: bool,
    /// Кнопка перевыпуска доступна.
    pub(crate) reissue_allowed: bool,
    /// Сколько обновлений ждёт на стороне Telegram.
    pub(crate) pending_update_count: Option<u32>,
    /// Последняя ошибка доставки webhook.
    pub(crate) webhook_error_message: Option<String>,
    /// Время последней ошибки доставки.
    pub(crate) webhook_error_date: Option<String>,
    /// `getWebhookInfo` не удался.
    pub(crate) webhook_info_failed: bool,
}

/// Итог сохранения формы.
pub(crate) enum TelegramSubmitOutcome {
    /// Настройки сохранены; `apply_error` — `reload` не удался.
    Saved {
        /// Сообщение об ошибке применения без секретов.
        apply_error: Option<String>,
    },
    /// Ввод отклонён: страница показывается с этим сообщением.
    Rejected {
        /// HTTP-статус ответа.
        status: axum::http::StatusCode,
        /// Сообщение без значений секретов.
        message: String,
    },
}

/// Прочитать данные страницы.
///
/// `submitted` возвращает несекретные значения формы после отказа; токен из
/// формы не берётся никогда.
pub(crate) async fn view(
    state: &AdminState,
    submitted: Option<&TelegramForm>,
    saved: bool,
    wizard: bool,
) -> Result<TelegramView, DomainError> {
    let Some(config) = state.telegram_config() else {
        return Err(DomainError::NotFound);
    };
    let effective =
        settings::read_effective_telegram(&state.pool, config, state.llm_settings().cipher())
            .await?;
    let settings = effective.settings();
    let readonly = config.telegram.is_some();
    let mode = effective.mode();

    let bot_name = if effective.is_configured() || effective.bot_token().is_some() {
        text_of(settings, "telegram.bot_username")
    } else {
        String::new()
    };
    let bot_id = resolved(settings, "telegram.bot_id")
        .and_then(|setting| setting.value.as_ref())
        .and_then(value_text)
        .and_then(|raw| parse_bot_id(&raw).ok());

    let public_base_url = field_view(
        settings,
        "telegram.public_base_url",
        submitted.and_then(|form| form.public_base_url.as_deref()),
        readonly,
    );
    let webhook_url_override = field_view(
        settings,
        "telegram.webhook_url_override",
        submitted.and_then(|form| form.webhook_url_override.as_deref()),
        readonly,
    );
    let current_mode = submitted
        .and_then(|form| form.mode.as_deref())
        .and_then(|raw| parse_telegram_mode(raw).ok())
        .unwrap_or(mode);

    let webhook_secret = secret_state(settings, "telegram.webhook_secret");
    let mut webhook_address = configured_webhook_address(&effective, settings).unwrap_or_default();

    let mut pending_update_count = None;
    let mut webhook_error_message = None;
    let mut webhook_error_date = None;
    let mut webhook_info_failed = false;
    if let Some(token) = effective.bot_token() {
        match webhook_info(config, token).await {
            Some(info) => {
                pending_update_count = Some(info.pending_update_count);
                webhook_error_message = info.last_error_message;
                webhook_error_date = info.last_error_date.and_then(format_timestamp);
                if !info.url.is_empty() {
                    webhook_address = info.url;
                }
            }
            None => webhook_info_failed = true,
        }
    }

    Ok(TelegramView {
        wizard,
        saved,
        readonly,
        bot_name,
        bot_id,
        source: group_source_label(&effective, config),
        mode: current_mode.as_str(),
        polling_allowed: !config.env.is_production(),
        webhook_selected: current_mode == TelegramMode::Webhook,
        polling_selected: current_mode == TelegramMode::Polling,
        disabled_selected: current_mode == TelegramMode::Disabled,
        token_label: secret_label(secret_state(settings, "telegram.bot_token")),
        token_source: source_label(source_of(settings, "telegram.bot_token")),
        public_base_url,
        webhook_url_override,
        webhook_address,
        webhook_secret_set: webhook_secret.is_set(),
        reissue_allowed: !readonly
            && current_mode == TelegramMode::Webhook
            && effective.bot_token().is_some(),
        pending_update_count,
        webhook_error_message,
        webhook_error_date,
        webhook_info_failed,
    })
}

/// Сохранить настройки Telegram по форме.
///
/// Сначала `getMe` отдельным клиентом: неверный токен ничего не сохраняет и не
/// трогает действующее поколение. Затем группа пишется одной транзакцией и
/// применяется [`TelegramSupervisor::reload`].
pub(crate) async fn submit(
    state: &AdminState,
    form: &TelegramForm,
    now: DateTime<Utc>,
) -> Result<TelegramSubmitOutcome, DomainError> {
    let Some(config) = state.telegram_config() else {
        return Err(DomainError::NotFound);
    };
    if config.telegram.is_some() {
        return Ok(TelegramSubmitOutcome::Rejected {
            status: axum::http::StatusCode::FORBIDDEN,
            message: ENV_READ_ONLY.to_owned(),
        });
    }

    let effective =
        settings::read_effective_telegram(&state.pool, config, state.llm_settings().cipher())
            .await?;
    let settings = effective.settings();

    let Some(mode) = form
        .mode
        .as_deref()
        .and_then(|raw| parse_telegram_mode(raw).ok())
    else {
        return Ok(rejected_bad_request(
            "Неизвестный режим: выберите webhook, polling или disabled.",
        ));
    };
    if config.env.is_production() && mode == TelegramMode::Polling {
        return Ok(rejected_bad_request(
            "В production polling недоступен: используйте webhook по HTTPS.",
        ));
    }

    let public = match submitted_url(
        form.public_base_url.as_deref(),
        source_of(settings, "telegram.public_base_url") == SettingSource::Env,
        config.env.is_production(),
    ) {
        Ok(value) => value,
        Err(message) => return Ok(rejected_bad_request(&message)),
    };
    let override_url = match submitted_url(
        form.webhook_url_override.as_deref(),
        false,
        config.env.is_production(),
    ) {
        Ok(value) => value,
        Err(message) => return Ok(rejected_bad_request(&message)),
    };

    let proposed_public = match &public {
        SubmittedField::Keep => config.public_base_url.clone(),
        SubmittedField::Delete => None,
        SubmittedField::Set(url) => Some(url.clone()),
    };
    let proposed_override = match &override_url {
        SubmittedField::Keep => url_of(settings, "telegram.webhook_url_override"),
        SubmittedField::Delete => None,
        SubmittedField::Set(url) => Some(url.clone()),
    };
    if mode == TelegramMode::Webhook && proposed_override.is_none() && proposed_public.is_none() {
        return Ok(rejected_bad_request(
            "Для webhook нужен публичный адрес или отдельный адрес webhook.",
        ));
    }

    let submitted_token = if mode == TelegramMode::Disabled {
        // Выключение не требует действующего токена: его могли отозвать в
        // BotFather, а бота всё равно нужно остановить.
        None
    } else {
        form.token
            .as_deref()
            .map(str::trim)
            .filter(|token| !token.is_empty())
            .map(|token| SecretString::new(token.to_owned()))
    };
    let me = if mode == TelegramMode::Disabled {
        None
    } else {
        let Some(token) = submitted_token
            .clone()
            .or_else(|| effective.bot_token().cloned())
        else {
            return Ok(rejected_bad_request("Введите токен бота."));
        };
        let api = HttpTelegramApi::new(token, bot_api_base(config))
            .map_err(|error| DomainError::Validation(error.to_string()))?;
        match api.get_me().await {
            Ok(me) => Some(me),
            Err(_) => return Ok(rejected_bad_request(TOKEN_REJECTED)),
        }
    };
    let username = me.as_ref().map_or_else(String::new, |me| {
        me.username
            .as_deref()
            .and_then(|raw| parse_bot_username(raw).ok())
            .or_else(|| {
                let current = text_of(settings, "telegram.bot_username");
                (!current.is_empty()).then_some(current)
            })
            .unwrap_or_else(|| DEFAULT_BOT_USERNAME.to_owned())
    });

    let new_secret = if mode == TelegramMode::Webhook
        && !secret_state(settings, "telegram.webhook_secret").is_set()
    {
        Some(SecretString::new(random_base64url(32)))
    } else {
        None
    };

    let mut changes: Vec<(&'static str, &'static str)> = Vec::new();
    if text_of(settings, "telegram.mode") != mode.as_str() {
        changes.push(("telegram.mode", "changed"));
    }
    if let Some(me) = &me {
        if current_bot_id(settings) != Some(me.id) {
            changes.push(("telegram.bot_id", "changed"));
        }
        if text_of(settings, "telegram.bot_username") != username {
            changes.push(("telegram.bot_username", "changed"));
        }
    }
    if submitted_token.is_some() {
        changes.push(("telegram.bot_token", "set"));
    }
    collect_url_change(&mut changes, settings, "telegram.public_base_url", &public);
    collect_url_change(
        &mut changes,
        settings,
        "telegram.webhook_url_override",
        &override_url,
    );
    if new_secret.is_some() {
        changes.push(("telegram.webhook_secret", "set"));
    }

    let mut tx = state.pool.begin().await?;
    let apply = async {
        write_plain(
            &mut tx,
            "telegram.mode",
            Value::String(mode.as_str().to_owned()),
        )
        .await?;
        if let Some(me) = &me {
            write_plain(&mut tx, "telegram.bot_id", Value::from(me.id)).await?;
            write_plain(
                &mut tx,
                "telegram.bot_username",
                Value::String(username.clone()),
            )
            .await?;
        }
        if let Some(token) = &submitted_token {
            settings::write_secret(
                &mut tx,
                "telegram.bot_token",
                token,
                state.llm_settings().cipher(),
            )
            .await?;
        }
        write_url(&mut tx, "telegram.public_base_url", &public).await?;
        write_url(&mut tx, "telegram.webhook_url_override", &override_url).await?;
        if let Some(secret) = &new_secret {
            settings::write_secret(
                &mut tx,
                "telegram.webhook_secret",
                secret,
                state.llm_settings().cipher(),
            )
            .await?;
        }
        Ok::<(), SettingError>(())
    }
    .await;
    match apply {
        Ok(()) => tx.commit().await.map_err(DomainError::Database)?,
        Err(error) => {
            let _ = tx.rollback().await;
            return match reject_setting(error) {
                Some(outcome) => Ok(outcome),
                None => Err(DomainError::Validation("setting write failed".to_owned())),
            };
        }
    }

    for (key, value) in &changes {
        state.audit.admin_setting_changed(key, value, now).await;
    }

    Ok(TelegramSubmitOutcome::Saved {
        apply_error: apply_settings(state, mode).await,
    })
}

/// Перевыпустить секрет webhook и переключить поколение.
pub(crate) async fn reissue_secret(
    state: &AdminState,
    now: DateTime<Utc>,
) -> Result<TelegramSubmitOutcome, DomainError> {
    let Some(config) = state.telegram_config() else {
        return Err(DomainError::NotFound);
    };
    if config.telegram.is_some() {
        return Ok(TelegramSubmitOutcome::Rejected {
            status: axum::http::StatusCode::FORBIDDEN,
            message: ENV_READ_ONLY.to_owned(),
        });
    }

    let effective =
        settings::read_effective_telegram(&state.pool, config, state.llm_settings().cipher())
            .await?;
    if effective.bot_token().is_none() {
        return Ok(TelegramSubmitOutcome::Rejected {
            status: axum::http::StatusCode::CONFLICT,
            message: "Сначала подключите бота: токена ещё нет.".to_owned(),
        });
    }
    let mode = effective.mode();

    let secret = SecretString::new(random_base64url(32));
    let mut tx = state.pool.begin().await?;
    match settings::write_secret(
        &mut tx,
        "telegram.webhook_secret",
        &secret,
        state.llm_settings().cipher(),
    )
    .await
    {
        Ok(()) => tx.commit().await.map_err(DomainError::Database)?,
        Err(error) => {
            let _ = tx.rollback().await;
            return match reject_setting(error) {
                Some(outcome) => Ok(outcome),
                None => Err(DomainError::Validation("setting write failed".to_owned())),
            };
        }
    }
    state
        .audit
        .admin_setting_changed("telegram.webhook_secret", "set", now)
        .await;

    Ok(TelegramSubmitOutcome::Saved {
        apply_error: apply_settings(state, mode).await,
    })
}

/// Применить сохранённые настройки тем же супервизором, что у рантайма.
///
/// В режиме webhook после `reload` проверяется, что Telegram подтверждает
/// ожидаемый адрес: `start_generation` не превращает отказ `setWebhook` в
/// ошибку (недоступность Telegram при старте не должна ронять сервис), поэтому
/// страница сама сверяет `getWebhookInfo` с адресом из настроек.
async fn apply_settings(state: &AdminState, mode: TelegramMode) -> Option<String> {
    let supervisor = state.telegram_supervisor()?;
    if let Err(error) = supervisor.reload().await {
        return Some(format!(
            "Настройки сохранены, но применить их не удалось: {error}."
        ));
    }
    if mode != TelegramMode::Webhook {
        return None;
    }

    let config = state.telegram_config()?;
    let effective =
        settings::read_effective_telegram(&state.pool, config, state.llm_settings().cipher())
            .await
            .ok()?;
    let expected = configured_webhook_address(&effective, effective.settings())?;
    let token = effective.bot_token()?;
    match webhook_info(config, token).await {
        Some(info) if info.url == expected => None,
        Some(_) => Some(
            "Настройки сохранены, но Telegram не подтвердил адрес webhook: проверьте адрес и TLS."
                .to_owned(),
        ),
        None => {
            Some("Настройки сохранены, но проверить регистрацию webhook не удалось.".to_owned())
        }
    }
}

/// Значение адреса из формы.
enum SubmittedField {
    /// Поле задано окружением: не меняется.
    Keep,
    /// Поле очищено: строка удаляется.
    Delete,
    /// Новое значение.
    Set(Url),
}

fn submitted_url(
    submitted: Option<&str>,
    readonly: bool,
    require_https: bool,
) -> Result<SubmittedField, String> {
    if readonly {
        return Ok(SubmittedField::Keep);
    }
    let raw = submitted.unwrap_or_default().trim();
    if raw.is_empty() {
        return Ok(SubmittedField::Delete);
    }
    let url = parse_slot_url(raw).map_err(|_| "Адрес должен быть абсолютным URL.".to_owned())?;
    if require_https && url.scheme() != "https" {
        return Err(HTTPS_REQUIRED.to_owned());
    }
    Ok(SubmittedField::Set(url))
}

async fn write_plain(
    tx: &mut sqlx::PgConnection,
    key: &str,
    value: Value,
) -> Result<(), SettingError> {
    settings::write_value(tx, key, value).await
}

async fn write_url(
    tx: &mut sqlx::PgConnection,
    key: &str,
    value: &SubmittedField,
) -> Result<(), SettingError> {
    match value {
        SubmittedField::Keep => Ok(()),
        SubmittedField::Delete => {
            settings::delete(tx, key).await?;
            Ok(())
        }
        SubmittedField::Set(url) => {
            settings::write_value(tx, key, Value::String(url.to_string())).await
        }
    }
}

fn collect_url_change(
    changes: &mut Vec<(&'static str, &'static str)>,
    settings: &[ResolvedSetting],
    key: &'static str,
    submitted: &SubmittedField,
) {
    match submitted {
        SubmittedField::Keep => {}
        SubmittedField::Delete => {
            if resolved(settings, key).is_some() {
                changes.push((key, "deleted"));
            }
        }
        SubmittedField::Set(url) => {
            if text_of(settings, key) != url.to_string() {
                changes.push((key, "changed"));
            }
        }
    }
}

fn reject_setting(error: SettingError) -> Option<TelegramSubmitOutcome> {
    match error {
        SettingError::SecretKeyMissing { .. } => Some(TelegramSubmitOutcome::Rejected {
            status: axum::http::StatusCode::BAD_REQUEST,
            message: "Не удалось сохранить секрет: не задан ключ шифрования \
                      TASKLERQ_SECRET_KEY."
                .to_owned(),
        }),
        SettingError::InvalidValue { key, reason } => Some(rejected_bad_request(&format!(
            "Неверное значение поля `{key}`: {reason}."
        ))),
        SettingError::Cipher { .. } => Some(rejected_bad_request(
            "Не удалось зашифровать секрет; ничего не сохранено.",
        )),
        SettingError::Database(error) => {
            tracing::error!(%error, "telegram setting write failed");
            None
        }
        SettingError::UnknownKey { .. }
        | SettingError::PlainValueForSecret { .. }
        | SettingError::SecretValueForPlain { .. } => {
            tracing::error!("telegram setting write failed: key kind mismatch");
            None
        }
    }
}

fn rejected_bad_request(message: &str) -> TelegramSubmitOutcome {
    TelegramSubmitOutcome::Rejected {
        status: axum::http::StatusCode::BAD_REQUEST,
        message: message.to_owned(),
    }
}

/// Сколько ждать `getWebhookInfo` на странице, секунды.
///
/// Клиент Bot API рантайма ждёт 15 с; консоль не должна висеть столько при
/// недоступном Telegram, поэтому опрос ограничен короче.
const CONSOLE_INFO_TIMEOUT: Duration = Duration::from_secs(5);

async fn webhook_info(
    config: &Config,
    token: &SecretString,
) -> Option<crate::telegram::api::WebhookInfo> {
    let api = HttpTelegramApi::new(token.clone(), bot_api_base(config)).ok()?;
    tokio::time::timeout(CONSOLE_INFO_TIMEOUT, api.get_webhook_info())
        .await
        .ok()?
        .ok()
}

fn bot_api_base(config: &Config) -> &str {
    config
        .telegram_api_base_url
        .as_ref()
        .map_or(DEFAULT_BASE_URL, Url::as_str)
}

fn field_view(
    settings: &[ResolvedSetting],
    key: &str,
    submitted: Option<&str>,
    whole_group_readonly: bool,
) -> TelegramField {
    let source = source_of(settings, key);
    let readonly = whole_group_readonly || source == SettingSource::Env;
    let value = if readonly {
        text_of(settings, key)
    } else {
        submitted.map_or_else(|| text_of(settings, key), str::to_owned)
    };
    TelegramField {
        value,
        source: source_label(source),
        readonly,
    }
}

fn configured_webhook_address(
    effective: &settings::EffectiveTelegram,
    settings: &[ResolvedSetting],
) -> Option<String> {
    if let Some(url) = effective
        .config()
        .and_then(|config| config.webhook_url.as_ref())
    {
        return Some(url.to_string());
    }
    if let Some(url) = url_of(settings, "telegram.webhook_url_override") {
        return Some(url.to_string());
    }
    url_of(settings, "telegram.public_base_url").map(|base| webhook_address(&base).to_string())
}

fn current_bot_id(settings: &[ResolvedSetting]) -> Option<i64> {
    resolved(settings, "telegram.bot_id")
        .and_then(|setting| setting.value.as_ref())
        .and_then(value_text)
        .and_then(|raw| parse_bot_id(&raw).ok())
}

fn url_of(settings: &[ResolvedSetting], key: &str) -> Option<Url> {
    let raw = text_of(settings, key);
    if raw.is_empty() {
        return None;
    }
    Url::parse(&raw).ok()
}

fn resolved<'a>(settings: &'a [ResolvedSetting], key: &str) -> Option<&'a ResolvedSetting> {
    settings.iter().find(|setting| setting.key == key)
}

fn source_of(settings: &[ResolvedSetting], key: &str) -> SettingSource {
    resolved(settings, key).map_or(SettingSource::Default, |setting| setting.source)
}

fn text_of(settings: &[ResolvedSetting], key: &str) -> String {
    resolved(settings, key)
        .and_then(|setting| setting.value.as_ref())
        .and_then(value_text)
        .unwrap_or_default()
}

fn value_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(_) => Some(value.to_string()),
        _ => None,
    }
}

fn secret_state(settings: &[ResolvedSetting], key: &str) -> SecretState {
    resolved(settings, key)
        .and_then(|setting| setting.secret)
        .unwrap_or(SecretState::NotSet)
}

fn secret_label(state: SecretState) -> &'static str {
    match state {
        SecretState::NotSet => "не задан",
        SecretState::Set => "задан",
        SecretState::KeyMissing => "ключ шифрования не задан — введите токен заново",
        SecretState::Undecryptable => "не расшифровывается — введите токен заново",
    }
}

fn source_label(source: SettingSource) -> &'static str {
    match source {
        SettingSource::Env => "окружение",
        SettingSource::Db => "база",
        SettingSource::Default => "по умолчанию",
    }
}

fn group_source_label(effective: &settings::EffectiveTelegram, config: &Config) -> &'static str {
    if config.telegram.is_some() {
        return "окружение";
    }
    if effective.is_configured() {
        return "консоль";
    }
    match effective.problem() {
        Some(TelegramProblem::NotSet) | None => "не настроена",
        Some(_) => "консоль (есть ошибка)",
    }
}

fn format_timestamp(timestamp: i64) -> Option<String> {
    DateTime::from_timestamp(timestamp, 0).map(|value| value.to_rfc3339())
}
