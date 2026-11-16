//! Настройки: хранение, источники и действующая конфигурация LLM.
//!
//! Каждый тест получает отдельную базу: ключи настроек фиксированы, и общая
//! таблица `settings` иначе смешивала бы строки параллельных тестов.

mod support;

use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde_json::{Map, Value, json};
use sqlx::PgPool;
use tasklerq::app::build_application;
use tasklerq::config::{Config, LlmConfig, SecretKey, SecretString, SlotConfig};
use tasklerq::settings::crypto::{SecretCipher, SecretContext};
use tasklerq::settings::{
    self, SETTING_KEYS, SecretProblem, SecretState, SettingSource, SlotProblem, effective_llm,
    read_effective_llm,
};
use tasklerq::storage::repositories::settings as repository;
use tokio::sync::watch;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn env_llm(pairs: &[(&str, &str)]) -> LlmConfig {
    let mut vars: Vec<(String, String)> = [
        ("ENV", "development"),
        (
            "DATABASE_URL",
            "postgres://tasklerq:secret@localhost:5432/tasklerq",
        ),
        ("TELEGRAM_MODE", "disabled"),
    ]
    .into_iter()
    .map(|(name, value)| (name.to_owned(), value.to_owned()))
    .collect();
    vars.extend(
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned())),
    );
    Config::from_map(vars)
        .expect("test environment is valid")
        .llm
}

fn key(byte: u8) -> SecretKey {
    SecretKey::parse(&BASE64.encode([byte; 32])).expect("32 bytes are a key")
}

fn cipher(byte: u8) -> SecretCipher {
    SecretCipher::new(Some(key(byte)), None)
}

fn cipher_without_key() -> SecretCipher {
    SecretCipher::new(None, None)
}

async fn set_plain(pool: &PgPool, name: &str, value: Value) {
    let mut conn = pool.acquire().await.expect("connection");
    settings::write_value(&mut conn, name, value)
        .await
        .expect("plain setting is written");
}

async fn set_secret(pool: &PgPool, name: &str, value: &str, cipher: &SecretCipher) {
    let mut conn = pool.acquire().await.expect("connection");
    settings::write_secret(&mut conn, name, &SecretString::new(value), cipher)
        .await
        .expect("secret setting is written");
}

async fn row_count(pool: &PgPool, name: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM settings WHERE key = $1")
        .bind(name)
        .fetch_one(pool)
        .await
        .expect("count")
}

fn primary_slot() -> SlotConfig {
    SlotConfig {
        url: "https://db.example.test/v1".parse().expect("url"),
        model: "db/model".to_owned(),
        token: Some(SecretString::new("db-token")),
        timeout: Duration::from_secs(10),
        extra: Map::new(),
    }
}

#[tokio::test]
async fn environment_wins_over_the_database_and_keeps_the_whole_slot() {
    let db = support::isolated_db().await;
    set_plain(
        &db.pool,
        "llm.primary.url",
        json!("https://db.example.test/v1"),
    )
    .await;
    set_plain(&db.pool, "llm.primary.model", json!("db/model")).await;
    set_secret(&db.pool, "llm.primary.token", "db-token", &cipher(1)).await;
    set_plain(&db.pool, "llm.primary.timeout_ms", json!(9_000)).await;
    set_plain(
        &db.pool,
        "llm.primary.extra_json",
        json!({"reasoning": {"effort": "minimal"}}),
    )
    .await;

    let env = env_llm(&[
        ("LLM_PRIMARY_URL", "https://env.example.test/v1"),
        ("LLM_PRIMARY_MODEL", "env/model"),
        ("LLM_PRIMARY_TOKEN", "env-token"),
        ("LLM_PRIMARY_TIMEOUT_MS", "3000"),
        ("LLM_PRIMARY_EXTRA_JSON", r#"{"temperature":0.1}"#),
    ]);

    let effective = read_effective_llm(&db.pool, &env, &cipher(1))
        .await
        .expect("settings are read");

    assert!(effective.enabled());
    assert_eq!(effective.enabled_source(), SettingSource::Default);
    let primary = effective.primary();
    assert_eq!(primary.source(), Some(SettingSource::Env));
    let config = primary.config().expect("primary slot");
    assert_eq!(config.url.as_str(), "https://env.example.test/v1");
    assert_eq!(config.model, "env/model");
    assert_eq!(
        config.token.as_ref().map(SecretString::expose),
        Some("env-token")
    );
    assert_eq!(config.timeout, Duration::from_secs(3));
    assert_eq!(
        config.extra.get("temperature").and_then(Value::as_f64),
        Some(0.1),
        "database extra_json must not leak into an environment slot"
    );

    for name in [
        "llm.primary.url",
        "llm.primary.model",
        "llm.primary.token",
        "llm.primary.timeout_ms",
        "llm.primary.extra_json",
    ] {
        let setting = effective
            .settings()
            .iter()
            .find(|setting| setting.key == name)
            .unwrap_or_else(|| panic!("known key {name}"));
        assert_eq!(setting.source, SettingSource::Env, "{name}");
    }
}

#[tokio::test]
async fn a_database_slot_needs_both_address_and_model() {
    let db = support::isolated_db().await;
    set_plain(
        &db.pool,
        "llm.primary.url",
        json!("https://db.example.test/v1"),
    )
    .await;
    set_plain(&db.pool, "llm.primary.model", json!("db/model")).await;
    set_secret(&db.pool, "llm.primary.token", "db-token", &cipher(1)).await;
    set_plain(&db.pool, "llm.primary.timeout_ms", json!(4_000)).await;
    set_plain(
        &db.pool,
        "llm.primary.extra_json",
        json!({"reasoning": {"effort": "minimal"}}),
    )
    .await;

    let effective = read_effective_llm(&db.pool, &env_llm(&[]), &cipher(1))
        .await
        .expect("settings are read");

    let primary = effective.primary();
    assert_eq!(primary.source(), Some(SettingSource::Db));
    assert_eq!(primary.token_state(), SecretState::Set);
    let config = primary.config().expect("primary slot");
    assert_eq!(config.model, "db/model");
    assert_eq!(config.timeout, Duration::from_secs(4));
    assert_eq!(
        config
            .extra
            .get("reasoning")
            .and_then(|reasoning| reasoning.get("effort"))
            .and_then(Value::as_str),
        Some("minimal")
    );
    assert_eq!(
        config.token.as_ref().map(SecretString::expose),
        Some("db-token")
    );
    let token = effective
        .settings()
        .iter()
        .find(|setting| setting.key == "llm.primary.token")
        .expect("token key");
    assert_eq!(token.source, SettingSource::Db);
    assert_eq!(token.secret, Some(SecretState::Set));
    assert_eq!(token.value, None, "a secret is never shown");

    let mut conn = db.pool.acquire().await.expect("connection");
    settings::delete(&mut conn, "llm.primary.model")
        .await
        .expect("model is deleted");
    drop(conn);

    let effective = read_effective_llm(&db.pool, &env_llm(&[]), &cipher(1))
        .await
        .expect("settings are read");
    assert_eq!(effective.primary().problem(), Some(SlotProblem::Incomplete));
    assert!(!effective.primary().is_configured());
}

#[tokio::test]
async fn a_secret_without_a_key_is_not_written_and_plain_fields_still_are() {
    let db = support::isolated_db().await;
    let no_key = cipher_without_key();

    set_plain(
        &db.pool,
        "llm.primary.url",
        json!("https://db.example.test/v1"),
    )
    .await;

    let mut conn = db.pool.acquire().await.expect("connection");
    let error = settings::write_secret(
        &mut conn,
        "llm.primary.token",
        &SecretString::new("new-token"),
        &no_key,
    )
    .await
    .expect_err("a secret without a key must be rejected");
    assert!(
        matches!(error, settings::SettingError::SecretKeyMissing { .. }),
        "{error:?}"
    );
    drop(conn);

    assert_eq!(row_count(&db.pool, "llm.primary.token").await, 0);
    assert_eq!(row_count(&db.pool, "llm.primary.url").await, 1);
}

#[tokio::test]
async fn an_undecryptable_token_makes_the_whole_slot_unavailable() {
    let db = support::isolated_db().await;
    set_plain(
        &db.pool,
        "llm.primary.url",
        json!("https://db.example.test/v1"),
    )
    .await;
    set_plain(&db.pool, "llm.primary.model", json!("db/model")).await;
    set_secret(&db.pool, "llm.primary.token", "db-token", &cipher(1)).await;

    let wrong_key = read_effective_llm(&db.pool, &env_llm(&[]), &cipher(2))
        .await
        .expect("settings are read");
    assert!(!wrong_key.primary().is_configured());
    assert_eq!(
        wrong_key.primary().problem(),
        Some(SlotProblem::SecretUndecryptable)
    );
    assert_eq!(
        wrong_key.primary().token_state(),
        SecretState::Undecryptable
    );
    assert_eq!(
        wrong_key
            .settings()
            .iter()
            .find(|setting| setting.key == "llm.primary.token")
            .and_then(|setting| setting.secret),
        Some(SecretState::Undecryptable)
    );

    let no_key = read_effective_llm(&db.pool, &env_llm(&[]), &cipher_without_key())
        .await
        .expect("settings are read");
    assert!(!no_key.primary().is_configured());
    assert_eq!(
        no_key.primary().problem(),
        Some(SlotProblem::SecretKeyMissing)
    );
    assert_eq!(no_key.primary().token_state(), SecretState::KeyMissing);
}

#[tokio::test]
async fn unknown_keys_in_the_table_are_ignored() {
    let db = support::isolated_db().await;
    let mut conn = db.pool.acquire().await.expect("connection");
    // Строка от будущей версии: через публичный писатель такой ключ не создать.
    tasklerq::storage::repositories::settings::set_value(
        &mut conn,
        "installation",
        None,
        "llm.from_a_newer_version",
        &json!("value"),
    )
    .await
    .expect("an unknown row is written directly");
    drop(conn);

    let effective = read_effective_llm(&db.pool, &env_llm(&[]), &cipher(1))
        .await
        .expect("an unknown key must not break reading");

    assert_eq!(effective.settings().len(), SETTING_KEYS.len());
    assert!(
        effective
            .settings()
            .iter()
            .all(|setting| setting.key != "llm.from_a_newer_version")
    );
}

#[tokio::test]
async fn the_flag_follows_environment_then_database_then_default() {
    let db = support::isolated_db().await;
    set_plain(
        &db.pool,
        "llm.primary.url",
        json!("https://db.example.test/v1"),
    )
    .await;
    set_plain(&db.pool, "llm.primary.model", json!("db/model")).await;

    let default_off = read_effective_llm(&db.pool, &env_llm(&[]), &cipher(1))
        .await
        .expect("settings are read");
    assert!(
        !default_off.enabled(),
        "a database slot alone does not enable capture"
    );
    assert_eq!(default_off.enabled_source(), SettingSource::Default);
    assert_eq!(
        default_off
            .settings()
            .iter()
            .find(|setting| setting.key == "llm.enabled")
            .map(|setting| (setting.value.clone(), setting.source)),
        Some((Some(json!(false)), SettingSource::Default))
    );

    set_plain(&db.pool, "llm.enabled", json!(true)).await;
    let from_db = read_effective_llm(&db.pool, &env_llm(&[]), &cipher(1))
        .await
        .expect("settings are read");
    assert!(from_db.enabled());
    assert_eq!(from_db.enabled_source(), SettingSource::Db);

    let from_env = read_effective_llm(
        &db.pool,
        &env_llm(&[
            ("LLM_CAPTURE_ENABLED", "false"),
            ("LLM_PRIMARY_URL", "https://env.example.test/v1"),
            ("LLM_PRIMARY_MODEL", "env/model"),
        ]),
        &cipher(1),
    )
    .await
    .expect("settings are read");
    assert!(!from_env.enabled(), "an explicit environment flag wins");
    assert_eq!(from_env.enabled_source(), SettingSource::Env);

    set_plain(&db.pool, "llm.enabled", json!(false)).await;
    let enabled_by_environment = read_effective_llm(
        &db.pool,
        &env_llm(&[
            ("LLM_CAPTURE_ENABLED", "true"),
            ("LLM_PRIMARY_URL", "https://env.example.test/v1"),
            ("LLM_PRIMARY_MODEL", "env/model"),
        ]),
        &cipher(1),
    )
    .await
    .expect("settings are read");
    assert!(
        enabled_by_environment.enabled(),
        "an explicit environment true wins over database false"
    );
    assert_eq!(enabled_by_environment.enabled_source(), SettingSource::Env);
}

#[tokio::test]
async fn connect_timeout_and_quota_follow_environment_then_database_then_default() {
    let db = support::isolated_db().await;

    let defaults = read_effective_llm(&db.pool, &env_llm(&[]), &cipher(1))
        .await
        .expect("settings are read");
    assert_eq!(defaults.connect_timeout(), Duration::from_millis(1_000));
    assert_eq!(defaults.connect_timeout_source(), SettingSource::Default);
    assert_eq!(defaults.daily_quota(), 200);
    assert_eq!(defaults.daily_quota_source(), SettingSource::Default);
    for (name, expected) in [
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

    set_plain(&db.pool, "llm.connect_timeout_ms", json!(2_500)).await;
    set_plain(&db.pool, "llm.daily_quota", json!(300)).await;
    let from_db = read_effective_llm(&db.pool, &env_llm(&[]), &cipher(1))
        .await
        .expect("settings are read");
    assert_eq!(from_db.connect_timeout(), Duration::from_millis(2_500));
    assert_eq!(from_db.connect_timeout_source(), SettingSource::Db);
    assert_eq!(from_db.daily_quota(), 300);
    assert_eq!(from_db.daily_quota_source(), SettingSource::Db);

    let from_env = read_effective_llm(
        &db.pool,
        &env_llm(&[
            ("LLM_CONNECT_TIMEOUT_MS", "1500"),
            ("LLM_DAILY_QUOTA", "250"),
        ]),
        &cipher(1),
    )
    .await
    .expect("settings are read");
    assert_eq!(from_env.connect_timeout(), Duration::from_millis(1_500));
    assert_eq!(from_env.connect_timeout_source(), SettingSource::Env);
    assert_eq!(from_env.daily_quota(), 250);
    assert_eq!(from_env.daily_quota_source(), SettingSource::Env);
}

#[tokio::test]
async fn writes_upsert_and_delete_the_row() {
    let db = support::isolated_db().await;
    let mut conn = db.pool.acquire().await.expect("connection");

    settings::write_value(&mut conn, "llm.primary.model", json!("first/model"))
        .await
        .expect("first write");
    settings::write_value(&mut conn, "llm.primary.model", json!("second/model"))
        .await
        .expect("upsert");
    let value: Value = sqlx::query_scalar("SELECT value FROM settings WHERE key = $1")
        .bind("llm.primary.model")
        .fetch_one(&mut *conn)
        .await
        .expect("one row");
    assert_eq!(value, json!("second/model"));
    assert_eq!(row_count(&db.pool, "llm.primary.model").await, 1);

    settings::write_secret(
        &mut conn,
        "llm.primary.token",
        &SecretString::new("token"),
        &cipher(1),
    )
    .await
    .expect("secret write");
    let (value, secret): (Option<Value>, Option<Vec<u8>>) =
        sqlx::query_as("SELECT value, secret FROM settings WHERE key = $1")
            .bind("llm.primary.token")
            .fetch_one(&mut *conn)
            .await
            .expect("secret row");
    assert!(value.is_none());
    assert!(secret.is_some());

    assert!(
        settings::delete(&mut conn, "llm.primary.token")
            .await
            .expect("delete")
    );
    assert!(
        !settings::delete(&mut conn, "llm.primary.token")
            .await
            .expect("second delete")
    );
}

#[tokio::test]
async fn a_database_fallback_slot_is_resolved_separately() {
    let db = support::isolated_db().await;
    set_plain(
        &db.pool,
        "llm.fallback.url",
        json!("https://backup.example.test/v1"),
    )
    .await;
    set_plain(&db.pool, "llm.fallback.model", json!("backup/model")).await;

    let env = env_llm(&[
        ("LLM_PRIMARY_URL", "https://env.example.test/v1"),
        ("LLM_PRIMARY_MODEL", "env/model"),
    ]);
    let effective = read_effective_llm(&db.pool, &env, &cipher(1))
        .await
        .expect("settings are read");

    assert_eq!(effective.primary().source(), Some(SettingSource::Env));
    assert_eq!(effective.fallback().source(), Some(SettingSource::Db));
    assert_eq!(
        effective
            .fallback()
            .config()
            .map(|slot| slot.model.as_str()),
        Some("backup/model")
    );
    assert_eq!(effective.fallback().token_state(), SecretState::NotSet);
}

#[tokio::test]
async fn a_secret_written_by_key_a_reads_by_key_b_through_the_old_key() {
    let db = support::isolated_db().await;
    set_plain(
        &db.pool,
        "llm.primary.url",
        json!("https://db.example.test/v1"),
    )
    .await;
    set_plain(&db.pool, "llm.primary.model", json!("db/model")).await;
    set_secret(&db.pool, "llm.primary.token", "db-token", &cipher(1)).await;

    let rotated = SecretCipher::new(Some(key(2)), Some(key(1)));
    let effective = read_effective_llm(&db.pool, &env_llm(&[]), &rotated)
        .await
        .expect("settings are read");

    assert!(effective.primary().is_configured());
    assert_eq!(effective.primary().token_state(), SecretState::Set);
    assert_eq!(
        effective
            .primary()
            .config()
            .and_then(|slot| slot.token.as_ref())
            .map(SecretString::expose),
        Some("db-token")
    );
}

#[tokio::test]
async fn writes_reject_unknown_keys_and_mismatched_kinds() {
    let db = support::isolated_db().await;
    let mut conn = db.pool.acquire().await.expect("connection");

    let unknown = settings::write_value(&mut conn, "llm.unknown", json!(true))
        .await
        .expect_err("unknown key");
    assert!(matches!(unknown, settings::SettingError::UnknownKey { .. }));

    let secret_as_plain = settings::write_value(&mut conn, "llm.primary.token", json!("plain"))
        .await
        .expect_err("a secret cannot be written as a plain value");
    assert!(matches!(
        secret_as_plain,
        settings::SettingError::PlainValueForSecret { .. }
    ));

    let plain_as_secret = settings::write_secret(
        &mut conn,
        "llm.primary.model",
        &SecretString::new("secret"),
        &cipher(1),
    )
    .await
    .expect_err("a plain setting cannot be written as a secret");
    assert!(matches!(
        plain_as_secret,
        settings::SettingError::SecretValueForPlain { .. }
    ));

    for (name, value) in [
        ("llm.daily_quota", json!("abc")),
        ("llm.primary.model", json!(true)),
        ("llm.primary.url", json!("not a url SECRET-value")),
    ] {
        let invalid = settings::write_value(&mut conn, name, value)
            .await
            .expect_err("a value of the wrong shape must be rejected");
        assert!(
            matches!(invalid, settings::SettingError::InvalidValue { .. }),
            "{name}: {invalid:?}"
        );
        assert!(!invalid.to_string().contains("SECRET-value"));
    }
}

#[tokio::test]
async fn writes_use_the_same_bounds_as_the_readers() {
    let db = support::isolated_db().await;
    let mut conn = db.pool.acquire().await.expect("connection");

    for value in [9_u64, 60_001, 100_000] {
        let error = settings::write_value(&mut conn, "llm.connect_timeout_ms", json!(value))
            .await
            .expect_err("a connect timeout outside the reader bounds is rejected");
        assert!(
            matches!(error, settings::SettingError::InvalidValue { .. }),
            "{value}: {error:?}"
        );
    }
    for value in [10_u64, 50, 60_000] {
        settings::write_value(&mut conn, "llm.connect_timeout_ms", json!(value))
            .await
            .unwrap_or_else(|error| panic!("connect timeout {value} must be accepted: {error:?}"));
    }
    let effective = read_effective_llm(&db.pool, &env_llm(&[]), &cipher(1))
        .await
        .expect("settings are read");
    assert_eq!(effective.connect_timeout(), Duration::from_secs(60));
    assert_eq!(effective.connect_timeout_source(), SettingSource::Db);

    for value in [0_u64, 100_001] {
        let error = settings::write_value(&mut conn, "llm.daily_quota", json!(value))
            .await
            .expect_err("a quota outside the reader bounds is rejected");
        assert!(
            matches!(error, settings::SettingError::InvalidValue { .. }),
            "{value}: {error:?}"
        );
    }
    for value in [1_u64, 100_000] {
        settings::write_value(&mut conn, "llm.daily_quota", json!(value))
            .await
            .unwrap_or_else(|error| panic!("quota {value} must be accepted: {error:?}"));
    }
    let effective = read_effective_llm(&db.pool, &env_llm(&[]), &cipher(1))
        .await
        .expect("settings are read");
    assert_eq!(effective.daily_quota(), 100_000);

    for value in [99_u64, 600_001] {
        let error = settings::write_value(&mut conn, "llm.primary.timeout_ms", json!(value))
            .await
            .expect_err("a slot timeout outside the reader bounds is rejected");
        assert!(
            matches!(error, settings::SettingError::InvalidValue { .. }),
            "{value}: {error:?}"
        );
    }
    for value in [100_u64, 600_000] {
        settings::write_value(&mut conn, "llm.primary.timeout_ms", json!(value))
            .await
            .unwrap_or_else(|error| panic!("slot timeout {value} must be accepted: {error:?}"));
    }
    set_plain(
        &db.pool,
        "llm.primary.url",
        json!("https://db.example.test/v1"),
    )
    .await;
    set_plain(&db.pool, "llm.primary.model", json!("db/model")).await;
    let effective = read_effective_llm(&db.pool, &env_llm(&[]), &cipher(1))
        .await
        .expect("settings are read");
    assert_eq!(
        effective.primary().config().map(|slot| slot.timeout),
        Some(Duration::from_secs(600))
    );
}

#[tokio::test]
async fn blank_secrets_are_rejected_without_a_row() {
    let db = support::isolated_db().await;
    let mut conn = db.pool.acquire().await.expect("connection");

    for value in ["", " ", "\n\t "] {
        let error = settings::write_secret(
            &mut conn,
            "llm.primary.token",
            &SecretString::new(value),
            &cipher(1),
        )
        .await
        .expect_err("a blank secret must be rejected");
        assert!(
            matches!(error, settings::SettingError::InvalidValue { .. }),
            "{value:?}: {error:?}"
        );
    }
    drop(conn);

    assert_eq!(row_count(&db.pool, "llm.primary.token").await, 0);
}

#[tokio::test]
async fn stored_secrets_are_checked_with_their_own_row_context() {
    let db = support::isolated_db().await;
    set_secret(&db.pool, "llm.primary.token", "db-token", &cipher(1)).await;

    let healthy = settings::read_secret_health(&db.pool, &cipher(1))
        .await
        .expect("secret rows are read");
    assert_eq!(healthy.secrets(), 1);
    assert_eq!(healthy.problem(), None);

    let missing = settings::read_secret_health(&db.pool, &cipher_without_key())
        .await
        .expect("secret rows are read");
    assert_eq!(missing.secrets(), 1);
    assert_eq!(missing.problem(), Some(SecretProblem::KeyMissing));

    let wrong = settings::read_secret_health(&db.pool, &cipher(2))
        .await
        .expect("secret rows are read");
    assert_eq!(wrong.problem(), Some(SecretProblem::Undecryptable));

    let rotated = SecretCipher::new(Some(key(2)), Some(key(1)));
    let readable = settings::read_secret_health(&db.pool, &rotated)
        .await
        .expect("secret rows are read");
    assert_eq!(readable.problem(), None);
}

#[tokio::test]
async fn unknown_secret_keys_are_checked_too() {
    let db = support::isolated_db().await;
    let key = "llm.from_a_newer_version";
    let blob = cipher(1)
        .encrypt(
            &SecretContext::installation(key),
            &SecretString::new("SECRET-future-token"),
        )
        .expect("encryption");
    let mut conn = db.pool.acquire().await.expect("connection");
    repository::set_secret(&mut conn, "installation", None, key, &blob)
        .await
        .expect("an unknown secret row is written directly");
    drop(conn);

    let healthy = settings::read_secret_health(&db.pool, &cipher(1))
        .await
        .expect("secret rows are read");
    assert_eq!(healthy.secrets(), 1);
    assert_eq!(healthy.problem(), None);

    let wrong = settings::read_secret_health(&db.pool, &cipher(2))
        .await
        .expect("secret rows are read");
    assert_eq!(
        wrong.problem(),
        Some(SecretProblem::Undecryptable),
        "unknown keys are checked for rotation as well"
    );
}

#[test]
fn the_effective_config_builds_the_same_primary_slot_for_the_chain() {
    let slot = primary_slot();
    let env = LlmConfig {
        enabled: None,
        primary: Some(slot.clone()),
        fallback: None,
        connect_timeout: Duration::from_millis(1_000),
        connect_timeout_from_env: false,
        daily_quota: 200,
        daily_quota_from_env: false,
        ignored_env: Vec::new(),
    };

    let effective = effective_llm(&env, &[], &cipher(1));
    let chain = tasklerq::llm::Chain::from_effective(&effective)
        .expect("an enabled slot builds a chain")
        .expect("the primary slot is set");

    assert!(effective.enabled());
    assert_eq!(
        chain.primary().timeout(),
        Duration::from_secs(10),
        "the chain uses the slot timeout"
    );
    assert_eq!(
        effective.primary().config().expect("primary").model,
        slot.model
    );
    assert_eq!(effective.connect_timeout(), Duration::from_millis(1_000));
    assert_eq!(effective.daily_quota(), 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_slot_stored_only_in_settings_serves_http_capture_after_restart() {
    let db = support::isolated_db().await;
    let server = MockServer::start().await;
    let answer = support::llm::v4::response(vec![support::llm::v4::shopping("Молоко")]);
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "model": "db/model",
            "choices": [{"message": {"content": serde_json::to_string(&answer).expect("json")}}],
        })))
        .mount(&server)
        .await;

    // Окружение без LLM_*: слот задан только в settings, как после переноса из
    // `.env` в консоль (Migration Plan п.4).
    set_plain(
        &db.pool,
        "llm.primary.url",
        json!(format!("{}/v1", server.uri())),
    )
    .await;
    set_plain(&db.pool, "llm.primary.model", json!("db/model")).await;
    set_secret(&db.pool, "llm.primary.token", "db-token", &cipher(1)).await;
    set_plain(&db.pool, "llm.enabled", json!(true)).await;

    let secret_key = BASE64.encode([1u8; 32]);
    let config = Config::from_map([
        ("ENV", "development".to_owned()),
        ("PUBLIC_BASE_URL", "http://127.0.0.1:8080".to_owned()),
        (
            "DATABASE_URL",
            "postgres://tasklerq:secret@127.0.0.1:5432/tasklerq".to_owned(),
        ),
        ("TELEGRAM_MODE", "disabled".to_owned()),
        ("TASKLERQ_SECRET_KEY", secret_key),
    ])
    .expect("test config should be valid");
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let application = build_application(&config, db.pool.clone(), shutdown_rx)
        .await
        .expect("application");

    let household = support::create_household(&db.pool).await;
    let token = support::issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let (status, body) = support::request(
        &application.router,
        axum::http::Method::POST,
        "/api/v1/capture",
        Some(&token),
        Some(json!({"text": "купить молоко", "source": "api", "mode": "auto"})),
    )
    .await;

    assert_eq!(status, axum::http::StatusCode::OK, "{body}");
    assert_eq!(body["status"], "applied", "{body}");
    assert_eq!(
        body["fallback"], false,
        "the answer must come from the llm, not from the raw-text fallback: {body}"
    );
    assert_eq!(
        body["items"][0]["title"], "Молоко",
        "the title must be the model answer, not the source text: {body}"
    );
    assert_eq!(
        server.received_requests().await.expect("requests").len(),
        1,
        "the slot stored only in settings must receive the capture request"
    );

    shutdown_tx.send(true).expect("shutdown signal");
    for worker in application.workers {
        tokio::time::timeout(Duration::from_secs(5), worker)
            .await
            .expect("worker must stop on shutdown")
            .expect("worker task must join");
    }
}
