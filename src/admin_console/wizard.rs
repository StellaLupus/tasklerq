//! Мастер первого запуска: вычисление шага и пропуски.
//!
//! Шаг вычисляется чистым [`wizard_step`] из снимка состояния; чтение снимка
//! из базы — [`read_state`]. Пропуски лежат в `settings` под ключами
//! `wizard.*_skipped` и читаются тем же механизмом, что остальные настройки.
//!
//! Шаг household допустимо пропустить, только если Telegram включён: без бота
//! household и владельца создать некому. Сохранённый пропуск при выключенном
//! Telegram поэтому игнорируется.

use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::PgPool;

use crate::config::TenancyMode;
use crate::domain::error::DomainError;
use crate::settings::{self, EffectiveLlm};
use crate::storage::repositories::{admin_accounts, households, tokens};

/// Путь страницы мастера.
pub(crate) const WIZARD_PATH: &str = "/admin/wizard";
/// Путь пропуска шага мастера.
pub(crate) const WIZARD_SKIP_PATH: &str = "/admin/wizard/skip";

/// Шаг мастера.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WizardStep {
    /// Учётной записи нет: первый вход.
    Setup,
    /// Telegram не настроен ни окружением, ни базой.
    Telegram,
    /// В `single` нет household.
    Household,
    /// LLM не задан ни окружением, ни базой.
    Llm,
    /// В `single` у household нет токенов.
    Token,
    /// Все шаги пройдены: главная.
    Done,
}

impl WizardStep {
    /// Значение поля `step` формы пропуска.
    pub const fn slug(self) -> Option<&'static str> {
        match self {
            Self::Telegram => Some("telegram"),
            Self::Household => Some("household"),
            Self::Llm => Some("llm"),
            Self::Token => Some("token"),
            Self::Setup | Self::Done => None,
        }
    }

    /// Шаг по значению поля формы; только пропускаемые шаги.
    pub fn from_slug(slug: &str) -> Option<Self> {
        match slug {
            "telegram" => Some(Self::Telegram),
            "household" => Some(Self::Household),
            "llm" => Some(Self::Llm),
            "token" => Some(Self::Token),
            _ => None,
        }
    }

    /// Ключ `settings`, которым отмечается пропуск шага.
    pub const fn setting_key(self) -> Option<&'static str> {
        match self {
            Self::Telegram => Some("wizard.telegram_skipped"),
            Self::Household => Some("wizard.household_skipped"),
            Self::Llm => Some("wizard.llm_skipped"),
            Self::Token => Some("wizard.token_skipped"),
            Self::Setup | Self::Done => None,
        }
    }

    /// Допустим ли пропуск шага в этом режиме.
    ///
    /// Household пропускается только при включённом Telegram: без бота его и
    /// владельца создать некому. Остальные шаги пропускаются всегда.
    pub const fn can_skip(self, telegram_enabled: bool) -> bool {
        match self {
            Self::Household => telegram_enabled,
            Self::Telegram | Self::Llm | Self::Token => true,
            Self::Setup | Self::Done => false,
        }
    }

    /// Куда вести после пропуска и после входа.
    pub const fn target_path(self) -> &'static str {
        match self {
            Self::Setup => "/admin/setup",
            Self::Done => "/admin",
            Self::Telegram | Self::Household | Self::Llm | Self::Token => WIZARD_PATH,
        }
    }

    /// Заголовок шага на странице мастера.
    pub const fn title(self) -> &'static str {
        match self {
            Self::Setup => "Первый вход",
            Self::Telegram => "Telegram",
            Self::Household => "Дом",
            Self::Llm => "Разбор записей",
            Self::Token => "Токен Home Assistant",
            Self::Done => "Tasklerq",
        }
    }

    /// Короткое пояснение шага.
    pub const fn description(self) -> &'static str {
        match self {
            Self::Setup => "Создайте учётную запись администратора.",
            Self::Telegram => {
                "Подключите бота Telegram: без него household и владельца создать некому."
            }
            Self::Household => {
                "Создайте household — дом, к которому относятся задачи, покупки и календарь."
            }
            Self::Llm => {
                "Настройте разбор записей: без LLM сервис принимает только текст как есть."
            }
            Self::Token => {
                "Выпустите токен для Home Assistant, чтобы отправлять записи из автоматизаций."
            }
            Self::Done => "Все шаги мастера пройдены.",
        }
    }
}

/// Снимок состояния для вычисления шага.
#[derive(Clone, Copy, Debug)]
pub struct WizardState {
    /// Учётная запись администратора существует.
    pub account_exists: bool,
    /// Режим размещения.
    pub tenancy: TenancyMode,
    /// Группа Telegram задана окружением или базой.
    pub telegram_configured: bool,
    /// Telegram включён: пропуск household допустим.
    pub telegram_enabled: bool,
    /// Активный household есть.
    pub household: bool,
    /// У активного household есть действующий токен.
    pub tokens: bool,
    /// Основной слот LLM задан окружением или базой.
    pub llm_configured: bool,
    /// Шаг Telegram пропущен.
    pub telegram_skipped: bool,
    /// Шаг household пропущен.
    pub household_skipped: bool,
    /// Шаг LLM пропущен.
    pub llm_skipped: bool,
    /// Шаг токена пропущен.
    pub token_skipped: bool,
}

/// Действующее состояние Telegram для шагов мастера.
///
/// Считается по окружению и базе при каждом чтении снимка, а не фиксируется
/// при старте: подключение бота в консоли сразу открывает пропуск household.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TelegramWizardState {
    /// Группа задана целиком окружением или базой (режим `disabled` тоже
    /// считается заданной группой).
    pub configured: bool,
    /// Бот включён: группа задана и режим не `disabled`.
    pub enabled: bool,
}

impl TelegramWizardState {
    /// Состояние для консоли, собранной без рантайма: группа не показывается
    /// шагом мастера, а включённость берётся из статического признака.
    pub const fn from_static(enabled: bool) -> Self {
        Self {
            configured: true,
            enabled,
        }
    }
}

/// Вычислить текущий шаг мастера.
///
/// Завершённые шаги не повторяются: Telegram виден, только пока группа не
/// задана и шаг не пропущен; household — пока его нет и пропуск допустим;
/// LLM — пока слот не задан и шаг не пропущен; токен — только в `single` и
/// только когда household уже есть.
pub fn wizard_step(state: &WizardState) -> WizardStep {
    if !state.account_exists {
        return WizardStep::Setup;
    }

    if !state.telegram_configured && !state.telegram_skipped {
        return WizardStep::Telegram;
    }

    let household_skipped = state.household_skipped
        && WizardStep::Household.can_skip(state.telegram_enabled)
        && state.tenancy == TenancyMode::Single;
    if state.tenancy == TenancyMode::Single && !state.household && !household_skipped {
        return WizardStep::Household;
    }

    if !state.llm_configured && !state.llm_skipped {
        return WizardStep::Llm;
    }

    if state.tenancy == TenancyMode::Single
        && state.household
        && !state.tokens
        && !state.token_skipped
    {
        return WizardStep::Token;
    }

    WizardStep::Done
}

/// Настройки, от которых зависит снимок мастера.
///
/// Окружение LLM и ключи шифрования сюда не входят: их читает тот же
/// [`LlmSettings`](crate::llm::LlmSettings), что применяет настройки разбора.
#[derive(Clone, Debug)]
pub struct WizardSettings {
    /// Режим размещения.
    pub tenancy: TenancyMode,
    /// Telegram включён (`TELEGRAM_MODE` не `disabled`).
    pub telegram_enabled: bool,
}

impl WizardSettings {
    /// Собрать настройки чтения снимка мастера.
    pub fn new(tenancy: TenancyMode, telegram_enabled: bool) -> Self {
        Self {
            tenancy,
            telegram_enabled,
        }
    }
}

impl Default for WizardSettings {
    fn default() -> Self {
        Self {
            tenancy: TenancyMode::Single,
            telegram_enabled: false,
        }
    }
}

/// Основной слот LLM задан действующей конфигурацией.
pub fn llm_configured(effective: &EffectiveLlm) -> bool {
    effective.primary().is_configured()
}

/// Прочитать снимок состояния мастера из базы.
///
/// Окружение LLM и ключи шифрования берутся из общего
/// [`LlmSettings`](crate::llm::LlmSettings): у мастера нет своей копии.
pub async fn read_state(
    pool: &PgPool,
    settings: &WizardSettings,
    llm: &crate::llm::LlmSettings,
    telegram: TelegramWizardState,
    now: DateTime<Utc>,
) -> Result<WizardState, DomainError> {
    let (account_exists, household, tokens) = {
        let mut conn = pool.acquire().await?;
        let account_exists = admin_accounts::get(&mut conn).await?.is_some();
        let household = households::count_active(&mut conn).await? > 0;
        let tokens = tokens::any_active(&mut conn, now).await?;
        (account_exists, household, tokens)
    };

    let effective = settings::read_effective_llm(pool, llm.env(), llm.cipher()).await?;
    let skipped = |key: Option<&str>| {
        key.is_some_and(|key| {
            effective
                .settings()
                .iter()
                .find(|setting| setting.key == key)
                .and_then(|setting| setting.value.as_ref())
                .is_some_and(is_true)
        })
    };

    Ok(WizardState {
        account_exists,
        tenancy: settings.tenancy,
        telegram_configured: telegram.configured,
        telegram_enabled: telegram.enabled,
        household,
        tokens,
        llm_configured: llm_configured(&effective),
        telegram_skipped: skipped(WizardStep::Telegram.setting_key()),
        household_skipped: skipped(WizardStep::Household.setting_key()),
        llm_skipped: skipped(WizardStep::Llm.setting_key()),
        token_skipped: skipped(WizardStep::Token.setting_key()),
    })
}

fn is_true(value: &Value) -> bool {
    match value {
        Value::Bool(value) => *value,
        Value::String(text) => text == "true",
        _ => false,
    }
}

/// Записать отметку пропуска шага и вернуть записанный ключ.
pub async fn skip(pool: &PgPool, step: WizardStep) -> Result<&'static str, DomainError> {
    let key = step
        .setting_key()
        .ok_or_else(|| DomainError::Validation("step cannot be skipped".into()))?;

    let mut conn = pool.acquire().await?;
    settings::write_value(&mut conn, key, Value::Bool(true))
        .await
        .map_err(|error| match error {
            settings::SettingError::Database(error) => DomainError::Database(error),
            other => DomainError::Validation(other.to_string()),
        })?;

    Ok(key)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use base64::Engine;
    use base64::engine::general_purpose::STANDARD as BASE64;
    use serde_json::{Map, Value, json};

    use super::*;
    use crate::config::{LlmConfig, SecretKey, SlotConfig};
    use crate::settings::crypto::SecretCipher;
    use crate::settings::effective_llm;
    use crate::storage::repositories::settings::StoredSetting;

    fn base(tenancy: TenancyMode) -> WizardState {
        WizardState {
            account_exists: true,
            tenancy,
            telegram_configured: true,
            telegram_enabled: false,
            household: false,
            tokens: false,
            llm_configured: false,
            telegram_skipped: false,
            household_skipped: false,
            llm_skipped: false,
            token_skipped: false,
        }
    }

    fn cipher() -> SecretCipher {
        let key = SecretKey::parse(&BASE64.encode([0_u8; 32])).expect("key");
        SecretCipher::new(Some(key), None)
    }

    fn slot(url: &str, model: &str) -> SlotConfig {
        SlotConfig {
            url: url.parse().expect("url"),
            model: model.to_owned(),
            token: None,
            timeout: Duration::from_secs(10),
            extra: Map::new(),
        }
    }

    fn llm_with_primary() -> LlmConfig {
        LlmConfig {
            primary: Some(slot("https://env.example.test/v1", "env/model")),
            ..LlmConfig::empty()
        }
    }

    fn plain(key: &str, value: Value) -> StoredSetting {
        StoredSetting {
            key: key.to_owned(),
            value: Some(value),
            secret: None,
        }
    }

    #[test]
    fn an_unconfigured_telegram_comes_right_after_setup() {
        let mut state = base(TenancyMode::Single);
        state.telegram_configured = false;
        assert_eq!(wizard_step(&state), WizardStep::Telegram);

        state.telegram_skipped = true;
        assert_eq!(
            wizard_step(&state),
            WizardStep::Household,
            "a skipped Telegram step moves on to the household"
        );
    }

    #[test]
    fn a_configured_disabled_group_finishes_the_telegram_step() {
        let mut state = base(TenancyMode::Single);
        state.telegram_configured = true;
        state.telegram_enabled = false;
        assert_eq!(wizard_step(&state), WizardStep::Household);
    }

    #[test]
    fn a_missing_account_means_setup() {
        let mut state = base(TenancyMode::Single);
        state.account_exists = false;
        state.household = true;
        assert_eq!(wizard_step(&state), WizardStep::Setup);

        let mut state = base(TenancyMode::Multi);
        state.account_exists = false;
        assert_eq!(wizard_step(&state), WizardStep::Setup);
    }

    #[test]
    fn single_without_a_household_starts_at_the_household() {
        let state = base(TenancyMode::Single);
        assert_eq!(wizard_step(&state), WizardStep::Household);

        let mut telegram = state;
        telegram.telegram_enabled = true;
        assert_eq!(
            wizard_step(&telegram),
            WizardStep::Household,
            "Telegram alone does not skip the household step"
        );
    }

    #[test]
    fn a_saved_household_skip_is_ignored_without_telegram() {
        let mut state = base(TenancyMode::Single);
        state.household_skipped = true;
        state.telegram_enabled = false;
        assert_eq!(wizard_step(&state), WizardStep::Household);

        state.telegram_enabled = true;
        assert_eq!(wizard_step(&state), WizardStep::Llm);
    }

    #[test]
    fn multi_hides_the_household_and_token_steps() {
        let mut state = base(TenancyMode::Multi);
        assert_eq!(wizard_step(&state), WizardStep::Llm);

        state.llm_configured = true;
        assert_eq!(
            wizard_step(&state),
            WizardStep::Done,
            "multi never asks for a household token"
        );

        state.llm_skipped = true;
        state.household = false;
        assert_eq!(wizard_step(&state), WizardStep::Done);
    }

    #[test]
    fn an_existing_household_moves_on_to_llm() {
        let mut state = base(TenancyMode::Single);
        state.household = true;
        assert_eq!(wizard_step(&state), WizardStep::Llm);

        state.telegram_enabled = true;
        state.household_skipped = true;
        assert_eq!(
            wizard_step(&state),
            WizardStep::Llm,
            "a completed step is not repeated"
        );
    }

    #[test]
    fn a_configured_llm_moves_on_without_a_skip() {
        let mut state = base(TenancyMode::Single);
        state.household = true;
        state.llm_configured = true;
        assert_eq!(wizard_step(&state), WizardStep::Token);

        state.tokens = true;
        assert_eq!(wizard_step(&state), WizardStep::Done);
    }

    #[test]
    fn a_skipped_llm_moves_on_even_without_a_slot() {
        let mut state = base(TenancyMode::Single);
        state.household = true;
        state.llm_configured = false;
        state.llm_skipped = true;
        assert_eq!(wizard_step(&state), WizardStep::Token);

        state.tokens = true;
        assert_eq!(wizard_step(&state), WizardStep::Done);
    }

    #[test]
    fn a_skipped_token_finishes_the_wizard() {
        let mut state = base(TenancyMode::Single);
        state.household = true;
        state.llm_configured = true;
        state.token_skipped = true;
        assert_eq!(wizard_step(&state), WizardStep::Done);
    }

    #[test]
    fn a_token_step_needs_a_household() {
        let mut state = base(TenancyMode::Single);
        state.telegram_enabled = true;
        state.household_skipped = true;
        state.llm_configured = true;
        assert_eq!(
            wizard_step(&state),
            WizardStep::Done,
            "there is no household to issue a token for"
        );
    }

    #[test]
    fn an_unfinished_household_comes_before_llm() {
        let mut state = base(TenancyMode::Single);
        state.llm_configured = true;
        state.llm_skipped = true;
        assert_eq!(wizard_step(&state), WizardStep::Household);
    }

    #[test]
    fn environment_and_database_slots_both_count_as_configured() {
        let from_env = effective_llm(&llm_with_primary(), &[], &cipher());
        assert!(llm_configured(&from_env));

        let stored = vec![
            plain("llm.primary.url", json!("https://db.example.test/v1")),
            plain("llm.primary.model", json!("db/model")),
        ];
        let from_db = effective_llm(&LlmConfig::empty(), &stored, &cipher());
        assert!(llm_configured(&from_db));

        let none = effective_llm(&LlmConfig::empty(), &[], &cipher());
        assert!(!llm_configured(&none));
    }

    #[test]
    fn skip_slugs_and_keys_round_trip() {
        for step in [
            WizardStep::Telegram,
            WizardStep::Household,
            WizardStep::Llm,
            WizardStep::Token,
        ] {
            let slug = step.slug().expect("a skippable step");
            assert_eq!(WizardStep::from_slug(slug), Some(step));
            assert_eq!(step.target_path(), WIZARD_PATH);
            assert!(
                step.setting_key()
                    .expect("a skippable step has a key")
                    .starts_with("wizard.")
            );
            assert!(step.can_skip(true));
        }
        assert!(!WizardStep::Household.can_skip(false));
        assert!(WizardStep::Telegram.can_skip(false));
        assert!(WizardStep::Llm.can_skip(false));
        assert!(WizardStep::Token.can_skip(false));
        assert!(!WizardStep::Setup.can_skip(true));
        assert!(!WizardStep::Done.can_skip(true));
        assert_eq!(WizardStep::Setup.setting_key(), None);
        assert_eq!(WizardStep::Done.setting_key(), None);
        assert_eq!(WizardStep::from_slug("setup"), None);
        assert_eq!(WizardStep::from_slug("done"), None);
        assert_eq!(WizardStep::Done.target_path(), "/admin");
        assert_eq!(WizardStep::Setup.target_path(), "/admin/setup");
    }

    #[test]
    fn only_true_values_are_read_as_a_skip() {
        assert!(is_true(&Value::Bool(true)));
        assert!(is_true(&Value::String("true".to_owned())));
        assert!(!is_true(&Value::Bool(false)));
        assert!(!is_true(&Value::String("false".to_owned())));
        assert!(!is_true(&Value::Null));
    }
}
