//! Юнит-тесты чистой сборки действующей конфигурации LLM.

use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde_json::{Map, Value, json};
use uuid::Uuid;

use super::*;
use crate::config::{LlmConfig, SecretKey};

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
    let secret_keys = ["llm.primary.token", "llm.fallback.token"];
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
