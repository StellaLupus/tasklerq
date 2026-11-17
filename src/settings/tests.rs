//! Юнит-тесты чистой сборки действующей конфигурации LLM.

use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde_json::{Map, Value, json};
use uuid::Uuid;

use super::*;
use crate::config::{Config, LlmConfig, SecretKey, TelegramMode};

fn slot(url: &str, model: &str) -> SlotConfig {
    SlotConfig {
        url: url.parse().expect("url"),
        model: model.to_owned(),
        token: None,
        timeout: Duration::from_secs(10),
        extra: Map::new(),
    }
}

fn llm(enabled: Option<bool>, primary: Option<SlotConfig>) -> LlmConfig {
    LlmConfig {
        enabled,
        primary,
        fallback: None,
        connect_timeout: Duration::from_millis(u64::from(DEFAULT_CONNECT_TIMEOUT_MS)),
        connect_timeout_from_env: false,
        daily_quota: DEFAULT_DAILY_QUOTA,
        daily_quota_from_env: false,
        ignored_env: Vec::new(),
    }
}

fn cipher() -> SecretCipher {
    let key = SecretKey::parse(&BASE64.encode([0_u8; 32])).expect("32 zero bytes are a key");
    SecretCipher::new(Some(key), None)
}

fn cipher_for(byte: u8) -> SecretCipher {
    let key = SecretKey::parse(&BASE64.encode([byte; 32])).expect("32 bytes are a key");
    SecretCipher::new(Some(key), None)
}

fn cipher_without_key() -> SecretCipher {
    SecretCipher::new(None, None)
}

fn plain(key: &str, value: Value) -> StoredSetting {
    StoredSetting {
        key: key.to_owned(),
        value: Some(value),
        secret: None,
    }
}

fn secret(key: &str, plaintext: &str) -> StoredSetting {
    let context = SecretContext::installation(key);
    StoredSetting {
        key: key.to_owned(),
        value: None,
        secret: Some(
            cipher()
                .encrypt(&context, &SecretString::new(plaintext))
                .expect("encryption"),
        ),
    }
}

fn stored_slot(prefix: &str) -> Vec<StoredSetting> {
    vec![
        plain(
            &format!("{prefix}.url"),
            json!("https://db.example.test/v1"),
        ),
        plain(&format!("{prefix}.model"), json!("db/model")),
    ]
}

#[test]
fn an_absent_flag_enables_capture_when_the_primary_slot_comes_from_the_environment() {
    let env = llm(None, Some(slot("https://env.example.test/v1", "env/model")));

    let effective = effective_llm(&env, &[], &cipher());

    assert!(effective.enabled());
    assert_eq!(effective.enabled_source(), SettingSource::Default);
    assert_eq!(effective.primary().source(), Some(SettingSource::Env));
}

#[test]
fn an_absent_flag_keeps_capture_disabled_for_a_database_slot_without_llm_enabled() {
    let env = llm(None, None);
    let stored = stored_slot("llm.primary");

    let effective = effective_llm(&env, &stored, &cipher());

    assert!(!effective.enabled());
    assert_eq!(effective.enabled_source(), SettingSource::Default);
    assert!(effective.primary().is_configured());
    assert_eq!(effective.primary().source(), Some(SettingSource::Db));
}

#[test]
fn an_environment_slot_does_not_mix_database_fields() {
    let env = llm(
        None,
        Some(SlotConfig {
            token: None,
            timeout: Duration::from_secs(3),
            ..slot("https://env.example.test/v1", "env/model")
        }),
    );
    let mut stored = stored_slot("llm.primary");
    stored.extend([
        secret("llm.primary.token", "db-token"),
        plain("llm.primary.timeout_ms", json!(9_000)),
        plain(
            "llm.primary.extra_json",
            json!({"reasoning": {"effort": "minimal"}}),
        ),
    ]);

    let effective = effective_llm(&env, &stored, &cipher());
    let primary = effective.primary().config().expect("primary slot");

    assert_eq!(primary.url.as_str(), "https://env.example.test/v1");
    assert_eq!(primary.model, "env/model");
    assert!(
        primary.token.is_none(),
        "database token must not leak into an environment slot"
    );
    assert_eq!(primary.timeout, Duration::from_secs(3));
    assert!(primary.extra.is_empty());

    for key in [
        "llm.primary.url",
        "llm.primary.model",
        "llm.primary.token",
        "llm.primary.timeout_ms",
        "llm.primary.extra_json",
    ] {
        let setting = effective
            .settings()
            .iter()
            .find(|setting| setting.key == key)
            .expect("known key");
        assert_eq!(
            setting.source,
            SettingSource::Env,
            "{key} must come from the environment"
        );
    }
    let token = effective
        .settings()
        .iter()
        .find(|setting| setting.key == "llm.primary.token")
        .expect("token key");
    assert_eq!(token.secret, Some(SecretState::NotSet));
    assert_eq!(token.value, None);
}

#[test]
fn unknown_database_keys_are_ignored() {
    let stored = vec![plain("llm.future_setting", json!("from a newer version"))];

    let effective = effective_llm(&llm(None, None), &stored, &cipher());

    assert_eq!(effective.settings().len(), SETTING_KEYS.len());
    assert!(
        effective
            .settings()
            .iter()
            .all(|setting| setting.key != "llm.future_setting")
    );
}

#[test]
fn the_environment_flag_wins_over_the_database() {
    let stored = vec![plain("llm.enabled", json!(true))];
    let env = llm(
        Some(false),
        Some(slot("https://env.example.test/v1", "env/model")),
    );

    let effective = effective_llm(&env, &stored, &cipher());

    assert!(!effective.enabled());
    assert_eq!(effective.enabled_source(), SettingSource::Env);
}

#[test]
fn an_invalid_database_value_leaves_the_slot_unavailable_without_the_value() {
    let stored = vec![
        plain("llm.primary.url", json!("not a url SECRET-slot-value")),
        plain("llm.primary.model", json!("db/model")),
    ];

    let effective = effective_llm(&llm(None, None), &stored, &cipher());
    let problem = effective.primary().problem().expect("problem");

    assert_eq!(
        problem,
        SlotProblem::InvalidField {
            field: "llm.primary.url"
        }
    );
    assert!(!format!("{problem:?}").contains("SECRET-slot-value"));
}

#[test]
fn debug_output_does_not_contain_tokens_or_provider_secrets() {
    let stored = vec![
        plain(
            "llm.primary.url",
            json!("https://user:pass@db.example.test/v1?token=SECRET-query"),
        ),
        plain("llm.primary.model", json!("db/model")),
        plain("llm.primary.extra_json", json!({"api_key": "SECRET-extra"})),
        secret("llm.primary.token", "SECRET-token-value"),
    ];

    let effective = effective_llm(&llm(None, None), &stored, &cipher());
    let debug = format!("{effective:?}");

    for secret in [
        "SECRET-token-value",
        "SECRET-extra",
        "SECRET-query",
        "user:pass",
    ] {
        assert!(!debug.contains(secret), "{secret} leaked in {debug}");
    }
    assert!(
        debug.contains("api_key"),
        "extra keys are still named: {debug}"
    );
}

#[test]
fn settings_show_defaults_and_reject_invalid_database_scalars() {
    let defaults = effective_llm(&llm(None, None), &[], &cipher());
    for (name, expected) in [
        ("llm.enabled", json!(false)),
        ("llm.connect_timeout_ms", json!(1_000)),
        ("llm.daily_quota", json!(200)),
    ] {
        let setting = defaults
            .settings()
            .iter()
            .find(|setting| setting.key == name)
            .expect("known key");
        assert_eq!(setting.value.as_ref(), Some(&expected), "{name}");
        assert_eq!(setting.source, SettingSource::Default, "{name}");
    }

    let from_environment = effective_llm(
        &llm(
            Some(true),
            Some(slot("https://env.example.test/v1", "env/model")),
        ),
        &[],
        &cipher(),
    );
    assert_eq!(
        from_environment
            .settings()
            .iter()
            .find(|setting| setting.key == "llm.enabled")
            .and_then(|setting| setting.value.clone()),
        Some(json!(true))
    );

    let stored = vec![
        plain("llm.enabled", json!("yes")),
        plain("llm.connect_timeout_ms", json!("soon")),
        plain("llm.daily_quota", json!("many")),
    ];
    let invalid = effective_llm(&llm(None, None), &stored, &cipher());
    for (name, expected) in [
        ("llm.enabled", json!(false)),
        ("llm.connect_timeout_ms", json!(1_000)),
        ("llm.daily_quota", json!(200)),
    ] {
        let setting = invalid
            .settings()
            .iter()
            .find(|setting| setting.key == name)
            .expect("known key");
        assert_eq!(setting.value.as_ref(), Some(&expected), "{name}");
        assert_eq!(
            setting.source,
            SettingSource::Default,
            "an invalid database value must not claim source db for {name}"
        );
    }
}

#[test]
fn the_key_list_marks_exactly_the_token_keys_as_secret_and_names_are_unique() {
    let secret_keys = [
        "llm.primary.token",
        "llm.fallback.token",
        "telegram.bot_token",
        "telegram.webhook_secret",
    ];
    for key in SETTING_KEYS {
        assert_eq!(
            secret_keys.contains(&key.name),
            key.is_secret(),
            "{} must be marked consistently",
            key.name
        );
    }
    assert_eq!(
        SETTING_KEYS
            .iter()
            .map(|key| key.name)
            .collect::<std::collections::HashSet<_>>()
            .len(),
        SETTING_KEYS.len(),
        "keys are unique"
    );
}

#[test]
fn secret_contexts_are_built_from_the_setting_scope() {
    let household = Uuid::from_u64_pair(1, 2);
    let key = "llm.primary.token";

    assert_eq!(
        secret_context(SettingScope::Installation, None, key),
        SecretContext::installation(key)
    );
    assert_eq!(
        secret_context(SettingScope::Household, Some(household), key),
        SecretContext::household(household, key)
    );
}

/// Конфигурация с окружением, в котором Telegram задан только перечисленными
/// переменными.
fn telegram_config(pairs: &[(&str, &str)]) -> Config {
    let mut vars: Vec<(String, String)> = [
        ("ENV", "development"),
        (
            "DATABASE_URL",
            "postgres://tasklerq:secret@localhost:5432/tasklerq",
        ),
    ]
    .into_iter()
    .map(|(name, value)| (name.to_owned(), value.to_owned()))
    .collect();
    vars.extend(
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned())),
    );
    Config::from_map(vars).expect("test environment is valid")
}

fn telegram_env_group() -> Vec<(&'static str, &'static str)> {
    vec![
        ("TELEGRAM_MODE", "webhook"),
        ("TELEGRAM_BOT_TOKEN", "env-token"),
        ("TELEGRAM_WEBHOOK_SECRET", "env-webhook-secret"),
        (
            "TELEGRAM_WEBHOOK_URL",
            "http://127.0.0.1:8080/webhooks/telegram",
        ),
    ]
}

fn stored_telegram(mode: &str, token: Option<&str>) -> Vec<StoredSetting> {
    let mut stored = vec![plain("telegram.mode", json!(mode))];
    if let Some(token) = token {
        stored.push(secret("telegram.bot_token", token));
    }
    stored
}

fn telegram_setting<'a>(effective: &'a EffectiveTelegram, key: &str) -> &'a ResolvedSetting {
    effective
        .settings()
        .iter()
        .find(|setting| setting.key == key)
        .unwrap_or_else(|| panic!("known key {key}"))
}

#[test]
fn an_environment_token_takes_the_whole_group_and_ignores_database_rows() {
    let env = telegram_config(&telegram_env_group());
    let stored = vec![
        plain("telegram.mode", json!("disabled")),
        plain("telegram.bot_username", json!("dbbot")),
        plain("telegram.bot_id", json!(42)),
        plain("telegram.public_base_url", json!("https://db.example.test")),
        secret("telegram.webhook_secret", "db-webhook-secret"),
        secret("telegram.bot_token", "db-token"),
    ];

    let effective = effective_telegram(&env, &stored, &cipher());

    assert_eq!(effective.source(), Some(SettingSource::Env));
    assert!(effective.is_enabled());
    let group = effective.config().expect("group");
    assert_eq!(group.mode, TelegramMode::Webhook);
    assert_eq!(
        group.bot_token.as_ref().map(SecretString::expose),
        Some("env-token")
    );
    assert_eq!(
        group.webhook_secret.as_ref().map(SecretString::expose),
        Some("env-webhook-secret")
    );
    assert_eq!(group.bot_username, "tasklerqbot");
    assert_eq!(
        group.webhook_url.as_ref().map(|url| url.as_str()),
        Some("http://127.0.0.1:8080/webhooks/telegram")
    );

    for key in [
        "telegram.mode",
        "telegram.bot_token",
        "telegram.bot_username",
        "telegram.webhook_secret",
    ] {
        assert_eq!(
            telegram_setting(&effective, key).source,
            SettingSource::Env,
            "{key} must come from the environment"
        );
    }
    assert_eq!(
        telegram_setting(&effective, "telegram.bot_token").value,
        None,
        "a secret is never shown"
    );
    assert_eq!(
        telegram_setting(&effective, "telegram.bot_token").secret,
        Some(SecretState::Set)
    );
    assert_eq!(
        telegram_setting(&effective, "telegram.bot_id").source,
        SettingSource::Default,
        "the database bot id does not leak into an environment group"
    );
}

#[test]
fn a_database_group_is_enabled_after_restart() {
    let env = telegram_config(&[]);
    let mut stored = stored_telegram("polling", Some("db-token"));
    stored.push(plain("telegram.bot_username", json!("@db_bot")));

    let effective = effective_telegram(&env, &stored, &cipher());

    assert!(effective.is_enabled());
    assert_eq!(effective.source(), Some(SettingSource::Db));
    let group = effective.config().expect("group");
    assert_eq!(group.mode, TelegramMode::Polling);
    assert_eq!(group.bot_username, "db_bot");
    assert_eq!(
        group.bot_token.as_ref().map(SecretString::expose),
        Some("db-token")
    );
    for key in [
        "telegram.mode",
        "telegram.bot_token",
        "telegram.bot_username",
    ] {
        assert_eq!(
            telegram_setting(&effective, key).source,
            SettingSource::Db,
            "{key}"
        );
    }
}

#[test]
fn no_rows_means_telegram_is_disabled() {
    let effective = effective_telegram(&telegram_config(&[]), &[], &cipher());

    assert!(!effective.is_configured());
    assert!(!effective.is_enabled());
    assert_eq!(effective.mode(), TelegramMode::Disabled);
    assert_eq!(effective.problem(), Some(TelegramProblem::NotSet));
}

#[test]
fn a_database_mode_disabled_turns_the_bot_off() {
    let effective = effective_telegram(
        &telegram_config(&[]),
        &stored_telegram("disabled", Some("db-token")),
        &cipher(),
    );

    assert!(effective.is_configured());
    assert!(!effective.is_enabled());
    assert_eq!(effective.mode(), TelegramMode::Disabled);
}

#[test]
fn an_undecryptable_token_makes_the_whole_group_unavailable() {
    let stored = stored_telegram("polling", Some("db-token"));

    let wrong = effective_telegram(&telegram_config(&[]), &stored, &cipher_for(2));
    assert!(!wrong.is_configured());
    assert_eq!(
        wrong.problem(),
        Some(TelegramProblem::SecretUndecryptable {
            field: "telegram.bot_token"
        })
    );
    assert_eq!(
        telegram_setting(&wrong, "telegram.bot_token").secret,
        Some(SecretState::Undecryptable)
    );

    let missing = effective_telegram(&telegram_config(&[]), &stored, &cipher_without_key());
    assert!(!missing.is_configured());
    assert_eq!(
        missing.problem(),
        Some(TelegramProblem::SecretKeyMissing {
            field: "telegram.bot_token"
        })
    );
}

#[test]
fn a_webhook_group_from_the_database_requires_a_secret_and_an_address() {
    let missing_secret = effective_telegram(
        &telegram_config(&[]),
        &stored_telegram("webhook", Some("db-token")),
        &cipher(),
    );
    assert_eq!(
        missing_secret.problem(),
        Some(TelegramProblem::Incomplete {
            field: "telegram.webhook_secret"
        })
    );

    let mut stored = stored_telegram("webhook", Some("db-token"));
    stored.push(secret("telegram.webhook_secret", "db-webhook-secret"));
    stored.push(plain(
        "telegram.public_base_url",
        json!("https://db.example.test"),
    ));
    let complete = effective_telegram(&telegram_config(&[]), &stored, &cipher());
    assert!(complete.is_enabled());
    assert_eq!(
        complete
            .config()
            .and_then(|group| group.webhook_url.as_ref())
            .map(|url| url.as_str()),
        Some("https://db.example.test/webhooks/telegram")
    );
}

#[test]
fn the_webhook_address_chain_prefers_the_override_then_the_public_base() {
    let mut stored = stored_telegram("webhook", Some("db-token"));
    stored.push(secret("telegram.webhook_secret", "db-webhook-secret"));
    stored.push(plain(
        "telegram.public_base_url",
        json!("https://public.example.test/"),
    ));
    stored.push(plain(
        "telegram.webhook_url_override",
        json!("https://hooks.example.test/telegram"),
    ));

    let override_group = effective_telegram(&telegram_config(&[]), &stored, &cipher());
    assert_eq!(
        override_group
            .config()
            .and_then(|group| group.webhook_url.as_ref())
            .map(|url| url.as_str()),
        Some("https://hooks.example.test/telegram")
    );

    let mut without_override = stored.clone();
    without_override.retain(|row| row.key != "telegram.webhook_url_override");
    let public_group = effective_telegram(&telegram_config(&[]), &without_override, &cipher());
    assert_eq!(
        public_group
            .config()
            .and_then(|group| group.webhook_url.as_ref())
            .map(|url| url.as_str()),
        Some("https://public.example.test/webhooks/telegram"),
        "a trailing slash must not double the path separator"
    );

    // PUBLIC_BASE_URL окружения приоритетнее настройки базы.
    let with_env_base = telegram_config(&[("PUBLIC_BASE_URL", "https://env.example.test")]);
    let env_base_group = effective_telegram(&with_env_base, &without_override, &cipher());
    assert_eq!(
        env_base_group
            .config()
            .and_then(|group| group.webhook_url.as_ref())
            .map(|url| url.as_str()),
        Some("https://env.example.test/webhooks/telegram")
    );
    assert_eq!(
        telegram_setting(&env_base_group, "telegram.public_base_url").source,
        SettingSource::Env
    );
}

#[test]
fn a_database_webhook_address_must_be_https_in_production() {
    let mut stored = stored_telegram("webhook", Some("db-token"));
    stored.push(secret("telegram.webhook_secret", "db-webhook-secret"));
    stored.push(plain(
        "telegram.webhook_url_override",
        json!("http://hooks.example.test/telegram"),
    ));

    let dev = effective_telegram(&telegram_config(&[]), &stored, &cipher());
    assert!(dev.is_enabled(), "http is allowed in development");

    let production = telegram_config(&[
        ("ENV", "production"),
        ("PUBLIC_BASE_URL", "https://tasklerq.example.com"),
    ]);
    let rejected = effective_telegram(&production, &stored, &cipher());
    assert!(!rejected.is_configured());
    assert_eq!(
        rejected.problem(),
        Some(TelegramProblem::InvalidField {
            field: "telegram.webhook_url_override"
        })
    );
}

#[test]
fn every_telegram_setting_key_is_resolved_from_the_database() {
    let stored = vec![
        plain("telegram.mode", json!("polling")),
        secret("telegram.bot_token", "db-token"),
        plain("telegram.bot_id", json!(42)),
        plain("telegram.bot_username", json!("db_bot")),
        plain("telegram.public_base_url", json!("https://db.example.test")),
        plain(
            "telegram.webhook_url_override",
            json!("https://hooks.example.test/telegram"),
        ),
        secret("telegram.webhook_secret", "db-webhook-secret"),
    ];
    let effective = effective_telegram(&telegram_config(&[]), &stored, &cipher());

    let keys: Vec<_> = SETTING_KEYS
        .iter()
        .filter(|key| key.name.starts_with("telegram."))
        .collect();
    assert!(!keys.is_empty(), "the telegram key group must not be empty");
    for key in keys {
        assert_eq!(
            telegram_setting(&effective, key.name).source,
            SettingSource::Db,
            "{} must get a source from the stored row, not a default",
            key.name
        );
    }
}

#[test]
fn invalid_telegram_fields_from_the_database_name_the_key() {
    for (key, value) in [
        ("telegram.mode", json!("sometimes")),
        ("telegram.bot_id", json!("not-an-id")),
        ("telegram.bot_username", json!("bad name")),
        ("telegram.public_base_url", json!("not a url")),
    ] {
        let mut stored = stored_telegram("polling", Some("db-token"));
        stored.retain(|row| row.key != key);
        stored.push(plain(key, value));

        let effective = effective_telegram(&telegram_config(&[]), &stored, &cipher());

        assert!(!effective.is_configured(), "{key}");
        assert_eq!(
            effective.problem(),
            Some(TelegramProblem::InvalidField { field: key }),
            "{key}"
        );
    }
}

#[test]
fn polling_from_the_database_is_rejected_in_production() {
    let production = telegram_config(&[
        ("ENV", "production"),
        ("PUBLIC_BASE_URL", "https://tasklerq.example.com"),
    ]);

    let effective = effective_telegram(
        &production,
        &stored_telegram("polling", Some("db-token")),
        &cipher(),
    );

    assert!(!effective.is_configured());
    assert_eq!(
        effective.problem(),
        Some(TelegramProblem::InvalidField {
            field: "telegram.mode"
        })
    );
}

#[test]
fn a_webhook_url_in_the_environment_does_not_reach_a_database_group() {
    let config = telegram_config(&[("TELEGRAM_WEBHOOK_URL", "https://env.example.test/hook")]);
    assert!(config.telegram.is_none());
    assert_eq!(config.telegram_ignored_env, vec!["TELEGRAM_WEBHOOK_URL"]);

    let mut stored = stored_telegram("webhook", Some("db-token"));
    stored.push(secret("telegram.webhook_secret", "db-webhook-secret"));
    stored.push(plain(
        "telegram.public_base_url",
        json!("https://db.example.test"),
    ));

    let effective = effective_telegram(&config, &stored, &cipher());

    assert!(effective.is_enabled());
    assert_eq!(effective.source(), Some(SettingSource::Db));
    assert_eq!(
        effective
            .config()
            .and_then(|group| group.webhook_url.as_ref())
            .map(|url| url.as_str()),
        Some("https://db.example.test/webhooks/telegram"),
        "a lone TELEGRAM_WEBHOOK_URL without a token must not affect the address"
    );
}

#[test]
fn debug_output_does_not_contain_telegram_secrets() {
    let env = telegram_config(&telegram_env_group());
    let effective = effective_telegram(&env, &[], &cipher());
    let debug = format!("{effective:?}");

    for secret in ["env-token", "env-webhook-secret"] {
        assert!(!debug.contains(secret), "{secret} leaked in {debug}");
    }
}
