use std::collections::BTreeMap;
use std::io::Write;
use std::sync::{Arc, Mutex};

use super::env_source::parse_url;
use super::*;

#[derive(Clone, Default)]
struct LogBuffer(Arc<Mutex<Vec<u8>>>);

impl LogBuffer {
    fn contents(&self) -> String {
        let bytes = self.0.lock().expect("log buffer lock").clone();
        String::from_utf8(bytes).expect("logs are UTF-8")
    }
}

impl Write for LogBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("log buffer lock")
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'writer> tracing_subscriber::fmt::MakeWriter<'writer> for LogBuffer {
    type Writer = Self;

    fn make_writer(&'writer self) -> Self::Writer {
        self.clone()
    }
}

fn minimal_env() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("ENV".to_owned(), "development".to_owned()),
        (
            "PUBLIC_BASE_URL".to_owned(),
            "http://127.0.0.1:8080".to_owned(),
        ),
        (
            "DATABASE_URL".to_owned(),
            "postgres://tasklerq:db-password@127.0.0.1:5432/tasklerq?sslmode=disable".to_owned(),
        ),
    ])
}

fn base_env() -> BTreeMap<String, String> {
    let mut vars = minimal_env();
    vars.insert("TELEGRAM_MODE".to_owned(), "disabled".to_owned());
    vars.insert("LLM_CAPTURE_ENABLED".to_owned(), "false".to_owned());
    vars
}

fn config(pairs: BTreeMap<String, String>) -> Result<Config, ConfigError> {
    Config::from_map(pairs)
}

fn assert_missing(error: ConfigError, expected: &'static str) {
    match error {
        ConfigError::Missing { name } => assert_eq!(name, expected),
        other => panic!("expected missing {expected}, got {other:?}"),
    }
}

fn assert_invalid(error: ConfigError, expected: &'static str) {
    match error {
        ConfigError::Invalid { name, .. } => assert_eq!(name, expected),
        other => panic!("expected invalid {expected}, got {other:?}"),
    }
}

#[test]
fn minimal_development_config_is_valid() {
    let config = config(base_env()).expect("config should be valid");

    assert_eq!(config.env, Environment::Development);
    assert_eq!(config.telegram.mode, TelegramMode::Disabled);
    assert_eq!(config.database.max_connections, 10);
    assert!(config.capture.enabled);
    assert!(!config.llm.enabled);
    assert_eq!(config.retention.events_days, 365);
    assert_eq!(config.log_filter, DEFAULT_LOG_FILTER);
}

#[test]
fn defaults_match_documented_values() {
    let mut vars = minimal_env();
    for (name, value) in [
        ("TELEGRAM_BOT_TOKEN", "123:token"),
        ("TELEGRAM_WEBHOOK_SECRET", "secret"),
        (
            "TELEGRAM_WEBHOOK_URL",
            "http://127.0.0.1:8080/webhooks/telegram",
        ),
        ("HA_BASE_URL", "http://127.0.0.1:8123"),
        ("HA_TOKEN", "home-assistant-token"),
        ("HA_AI_TASK_ENTITY_ID", "ai_task.test"),
    ] {
        vars.insert(name.to_owned(), value.to_owned());
    }

    let config = config(vars).expect("defaults should produce a valid config");

    assert_eq!(config.bind_addr, "0.0.0.0:8080".parse().unwrap());
    assert_eq!(config.telegram.mode, TelegramMode::Webhook);
    assert!(config.llm.enabled);
    assert!(config.capture.enabled);
    assert_eq!(config.capture.voice_timeout_ms, 8_000);
    assert!(config.capture.voice_fallback_raw);
    assert_eq!(config.capture.rate_limit_per_min, 10);
    assert_eq!(config.capture.max_text_chars, 2_000);
    assert_eq!(config.llm.daily_quota, 200);
    assert_eq!(config.queue.max_attempts, 10);
    assert_eq!(config.queue.ready_lag_seconds, 300);
    assert_eq!(config.retention.updates_days, 7);
    assert_eq!(config.retention.drafts_days, 30);
    assert_eq!(config.retention.llm_runs_days, 90);
    assert_eq!(config.retention.events_days, 365);
    assert_eq!(config.retention.deleted_days, 30);
    assert_eq!(config.retention.outbox_days, 30);
    assert_eq!(config.retention.audit_days, 365);
    assert!(i64::from(MIN_AUDIT_DAYS) >= crate::storage::repositories::audit::JOURNAL_DAYS);
    assert_eq!(config.log_filter, DEFAULT_LOG_FILTER);
}

#[test]
fn environment_values_are_strict() {
    for (raw, expected) in [
        ("production", Some(Environment::Production)),
        ("development", Some(Environment::Development)),
        ("dev", Some(Environment::Development)),
        ("staging", None),
    ] {
        let result = Environment::parse(raw);
        assert_eq!(result.ok(), expected, "unexpected result for {raw}");
    }
}

#[test]
fn missing_database_url_fails() {
    let mut vars = base_env();
    vars.remove("DATABASE_URL");

    let error = config(vars).expect_err("DATABASE_URL is required");
    assert_missing(error, "DATABASE_URL");
}

#[test]
fn missing_public_base_url_fails() {
    let mut vars = base_env();
    vars.remove("PUBLIC_BASE_URL");

    let error = config(vars).expect_err("PUBLIC_BASE_URL is required");
    assert_missing(error, "PUBLIC_BASE_URL");
}

#[test]
fn empty_values_are_treated_as_missing() {
    let mut vars = base_env();
    vars.insert("DATABASE_URL".to_owned(), "   ".to_owned());

    let error = config(vars).expect_err("blank DATABASE_URL is missing");
    assert_missing(error, "DATABASE_URL");
}

#[test]
fn url_parser_covers_every_rejection_branch() {
    for (raw, require_https, expected) in [
        ("http://example.com", false, true),
        ("https://example.com/path", false, true),
        ("https://example.com", true, true),
        ("http://example.com", true, false),
        ("ftp://example.com", false, false),
        ("https://user:password@example.com", false, false),
        ("https://example.com/?token=1", false, false),
        ("https://example.com/#fragment", false, false),
        ("not a url", false, false),
    ] {
        let result = parse_url("TEST_URL", raw, require_https);
        assert_eq!(
            result.is_ok(),
            expected,
            "unexpected result for {raw}: {result:?}"
        );
        if let Err(error) = result {
            assert_invalid(error, "TEST_URL");
        }
    }
}

#[test]
fn public_base_url_rejects_credentials_query_and_fragment() {
    for raw in [
        "https://user:password@tasklerq.example.com",
        "https://tasklerq.example.com?token=1",
        "https://tasklerq.example.com#fragment",
    ] {
        let mut vars = base_env();
        vars.insert("PUBLIC_BASE_URL".to_owned(), raw.to_owned());

        let error = config(vars).expect_err("unsafe URL should be rejected");
        assert_invalid(error, "PUBLIC_BASE_URL");
    }
}

#[test]
fn production_requires_https_public_url() {
    let mut vars = base_env();
    vars.insert("ENV".to_owned(), "production".to_owned());
    vars.insert(
        "PUBLIC_BASE_URL".to_owned(),
        "http://tasklerq.example.com".to_owned(),
    );

    let error = config(vars).expect_err("HTTP is not allowed in production");
    let message = error.to_string();
    assert_invalid(error, "PUBLIC_BASE_URL");
    assert!(message.contains("HTTPS"));
}

#[test]
fn database_url_scheme_and_host_are_validated() {
    for raw in [
        "mysql://tasklerq:password@127.0.0.1/tasklerq",
        "postgres:///tasklerq",
        "not a url",
    ] {
        let mut vars = base_env();
        vars.insert("DATABASE_URL".to_owned(), raw.to_owned());

        let error = config(vars).expect_err("invalid DATABASE_URL should be rejected");
        assert_invalid(error, "DATABASE_URL");
    }

    let mut vars = base_env();
    vars.insert(
        "DATABASE_URL".to_owned(),
        "postgresql://tasklerq:password@127.0.0.1/tasklerq".to_owned(),
    );
    assert!(config(vars).is_ok(), "postgresql:// is a valid scheme");
}

#[test]
fn database_pool_size_is_bounded() {
    for value in ["4", "21", "many"] {
        let mut vars = base_env();
        vars.insert("DATABASE_MAX_CONNECTIONS".to_owned(), value.to_owned());

        let error = config(vars).expect_err("pool size should be rejected");
        assert_invalid(error, "DATABASE_MAX_CONNECTIONS");
    }

    for value in ["5", "20"] {
        let mut vars = base_env();
        vars.insert("DATABASE_MAX_CONNECTIONS".to_owned(), value.to_owned());

        let config = config(vars).expect("boundary pool size should be accepted");
        assert_eq!(
            config.database.max_connections,
            value.parse::<u32>().unwrap()
        );
    }
}

#[test]
fn bind_addr_is_validated() {
    let mut vars = base_env();
    vars.insert("BIND_ADDR".to_owned(), "not-an-address".to_owned());

    let error = config(vars).expect_err("invalid bind address should be rejected");
    assert_invalid(error, "BIND_ADDR");

    let mut vars = base_env();
    vars.insert("BIND_ADDR".to_owned(), "127.0.0.1:9090".to_owned());
    let config = config(vars).expect("valid bind address should be accepted");
    assert_eq!(config.bind_addr, "127.0.0.1:9090".parse().unwrap());
}

#[derive(Debug)]
enum TelegramExpected {
    Ok(TelegramMode),
    Missing(&'static str),
    Invalid(&'static str),
}

fn telegram_case(
    environment: &str,
    mode: Option<&str>,
    token: Option<&str>,
    secret: Option<&str>,
    webhook_url: Option<&str>,
) -> Result<TelegramConfig, ConfigError> {
    let mut vars = base_env();
    vars.insert("ENV".to_owned(), environment.to_owned());
    if environment == "production" {
        vars.insert(
            "PUBLIC_BASE_URL".to_owned(),
            "https://tasklerq.example.com".to_owned(),
        );
    }

    match mode {
        Some(mode) => {
            vars.insert("TELEGRAM_MODE".to_owned(), mode.to_owned());
        }
        None => {
            vars.remove("TELEGRAM_MODE");
        }
    }

    for (name, value) in [
        ("TELEGRAM_BOT_TOKEN", token),
        ("TELEGRAM_WEBHOOK_SECRET", secret),
        ("TELEGRAM_WEBHOOK_URL", webhook_url),
    ] {
        match value {
            Some(value) => {
                vars.insert(name.to_owned(), value.to_owned());
            }
            None => {
                vars.remove(name);
            }
        }
    }

    config(vars).map(|config| config.telegram)
}

#[test]
fn telegram_config_covers_modes_and_required_values() {
    let cases = [
        (
            "development",
            Some("disabled"),
            None,
            None,
            None,
            TelegramExpected::Ok(TelegramMode::Disabled),
        ),
        (
            "development",
            Some("disabled"),
            Some("123:token"),
            Some("secret"),
            Some("http://127.0.0.1:8080/webhooks/telegram"),
            TelegramExpected::Ok(TelegramMode::Disabled),
        ),
        (
            "development",
            Some("webhook"),
            None,
            Some("secret"),
            Some("http://127.0.0.1:8080/webhooks/telegram"),
            TelegramExpected::Missing("TELEGRAM_BOT_TOKEN"),
        ),
        (
            "development",
            Some("webhook"),
            Some("123:token"),
            None,
            Some("http://127.0.0.1:8080/webhooks/telegram"),
            TelegramExpected::Missing("TELEGRAM_WEBHOOK_SECRET"),
        ),
        (
            "development",
            Some("webhook"),
            Some("123:token"),
            Some("secret"),
            None,
            TelegramExpected::Missing("TELEGRAM_WEBHOOK_URL"),
        ),
        (
            "development",
            Some("webhook"),
            Some("123:token"),
            Some("secret"),
            Some("http://127.0.0.1:8080/webhooks/telegram"),
            TelegramExpected::Ok(TelegramMode::Webhook),
        ),
        (
            "production",
            Some("webhook"),
            Some("123:token"),
            Some("secret"),
            Some("https://tasklerq.example.com/webhooks/telegram"),
            TelegramExpected::Ok(TelegramMode::Webhook),
        ),
        (
            "production",
            Some("webhook"),
            Some("123:token"),
            Some("secret"),
            Some("http://tasklerq.example.com/webhooks/telegram"),
            TelegramExpected::Invalid("TELEGRAM_WEBHOOK_URL"),
        ),
        (
            "development",
            Some("polling"),
            None,
            None,
            None,
            TelegramExpected::Missing("TELEGRAM_BOT_TOKEN"),
        ),
        (
            "development",
            Some("polling"),
            Some("123:token"),
            None,
            None,
            TelegramExpected::Ok(TelegramMode::Polling),
        ),
        (
            "production",
            Some("polling"),
            Some("123:token"),
            None,
            None,
            TelegramExpected::Invalid("TELEGRAM_MODE"),
        ),
        (
            "development",
            None,
            Some("123:token"),
            Some("secret"),
            Some("http://127.0.0.1:8080/webhooks/telegram"),
            TelegramExpected::Ok(TelegramMode::Webhook),
        ),
    ];

    for (environment, mode, token, secret, webhook_url, expected) in cases {
        let result = telegram_case(environment, mode, token, secret, webhook_url);
        match expected {
            TelegramExpected::Ok(mode) => {
                let config = result.expect("telegram config should be valid");
                assert_eq!(config.mode, mode);
            }
            TelegramExpected::Missing(name) => assert_missing(result.unwrap_err(), name),
            TelegramExpected::Invalid(name) => assert_invalid(result.unwrap_err(), name),
        }
    }
}

#[derive(Debug)]
enum LlmExpected {
    Ok(bool),
    Missing(&'static str),
    Invalid(&'static str),
}

fn llm_case(
    enabled: Option<&str>,
    base_url: Option<&str>,
    token: Option<&str>,
    entity_id: Option<&str>,
) -> Result<LlmConfig, ConfigError> {
    let mut vars = base_env();

    match enabled {
        Some(enabled) => {
            vars.insert("LLM_CAPTURE_ENABLED".to_owned(), enabled.to_owned());
        }
        None => {
            vars.remove("LLM_CAPTURE_ENABLED");
        }
    }

    for (name, value) in [
        ("HA_BASE_URL", base_url),
        ("HA_TOKEN", token),
        ("HA_AI_TASK_ENTITY_ID", entity_id),
    ] {
        match value {
            Some(value) => {
                vars.insert(name.to_owned(), value.to_owned());
            }
            None => {
                vars.remove(name);
            }
        }
    }

    config(vars).map(|config| config.llm)
}

#[test]
fn llm_config_covers_defaults_and_required_values() {
    let cases = [
        (None, None, None, None, LlmExpected::Missing("HA_BASE_URL")),
        (Some("false"), None, None, None, LlmExpected::Ok(false)),
        (
            Some("true"),
            None,
            None,
            None,
            LlmExpected::Missing("HA_BASE_URL"),
        ),
        (
            Some("true"),
            Some("http://127.0.0.1:8123"),
            None,
            None,
            LlmExpected::Missing("HA_TOKEN"),
        ),
        (
            Some("true"),
            Some("http://127.0.0.1:8123"),
            Some("home-assistant-token"),
            None,
            LlmExpected::Missing("HA_AI_TASK_ENTITY_ID"),
        ),
        (
            Some("true"),
            Some("http://127.0.0.1:8123"),
            Some("home-assistant-token"),
            Some("ai_task.test"),
            LlmExpected::Ok(true),
        ),
        (
            Some("false"),
            Some("http://127.0.0.1:8123"),
            None,
            None,
            LlmExpected::Ok(false),
        ),
        (
            Some("true"),
            Some("ftp://127.0.0.1:8123"),
            Some("home-assistant-token"),
            Some("ai_task.test"),
            LlmExpected::Invalid("HA_BASE_URL"),
        ),
        (
            Some("sometimes"),
            None,
            None,
            None,
            LlmExpected::Invalid("LLM_CAPTURE_ENABLED"),
        ),
    ];

    for (enabled, base_url, token, entity_id, expected) in cases {
        let result = llm_case(enabled, base_url, token, entity_id);
        match expected {
            LlmExpected::Ok(enabled) => {
                let config = result.expect("LLM config should be valid");
                assert_eq!(config.enabled, enabled);
            }
            LlmExpected::Missing(name) => assert_missing(result.unwrap_err(), name),
            LlmExpected::Invalid(name) => assert_invalid(result.unwrap_err(), name),
        }
    }
}

#[test]
fn numeric_limits_are_enforced() {
    for (name, too_small, too_large) in [
        ("CAPTURE_VOICE_TIMEOUT_MS", "999", "60001"),
        ("CAPTURE_RATE_LIMIT_PER_MIN", "0", "1001"),
        ("CAPTURE_MAX_TEXT_CHARS", "99", "16001"),
        ("QUEUE_MAX_ATTEMPTS", "0", "101"),
        ("QUEUE_READY_LAG_SECONDS", "0", "86401"),
        ("RETENTION_EVENTS_DAYS", "0", "3651"),
        ("RETENTION_AUDIT_DAYS", "89", "3651"),
    ] {
        for value in [too_small, too_large] {
            let mut vars = base_env();
            vars.insert(name.to_owned(), value.to_owned());

            let error = config(vars).expect_err("out-of-range value should be rejected");
            assert_invalid(error, name);
        }
    }

    let mut vars = base_env();
    vars.insert("QUEUE_MAX_ATTEMPTS".to_owned(), "many".to_owned());
    let error = config(vars).expect_err("non-integer should be rejected");
    assert_invalid(error, "QUEUE_MAX_ATTEMPTS");
}

#[test]
fn boolean_values_are_strict() {
    for name in ["CAPTURE_API_ENABLED", "LLM_CAPTURE_ENABLED"] {
        let mut vars = base_env();
        vars.insert(name.to_owned(), "yes".to_owned());

        let error = config(vars).expect_err("boolean should be strict");
        assert_invalid(error, name);
    }
}

#[test]
fn retention_days_are_configurable() {
    let mut vars = base_env();
    vars.insert("RETENTION_UPDATES_DAYS".to_owned(), "14".to_owned());
    vars.insert("RETENTION_DRAFTS_DAYS".to_owned(), "60".to_owned());

    let config = config(vars).expect("config should be valid");
    assert_eq!(config.retention.updates_days, 14);
    assert_eq!(config.retention.drafts_days, 60);
}

#[test]
fn log_filter_is_trimmed_and_defaults_when_blank() {
    let mut vars = base_env();
    vars.insert("RUST_LOG".to_owned(), "  debug  ".to_owned());
    assert_eq!(config(vars).unwrap().log_filter, "debug");

    let mut vars = base_env();
    vars.insert("RUST_LOG".to_owned(), "   ".to_owned());
    assert_eq!(config(vars).unwrap().log_filter, DEFAULT_LOG_FILTER);
}

fn env_with_secrets() -> BTreeMap<String, String> {
    let mut vars = base_env();
    vars.insert("TELEGRAM_MODE".to_owned(), "webhook".to_owned());
    vars.insert(
        "TELEGRAM_BOT_TOKEN".to_owned(),
        "123456:telegram-secret-token".to_owned(),
    );
    vars.insert(
        "TELEGRAM_WEBHOOK_SECRET".to_owned(),
        "telegram-webhook-secret".to_owned(),
    );
    vars.insert(
        "TELEGRAM_WEBHOOK_URL".to_owned(),
        "http://127.0.0.1:8080/webhooks/telegram".to_owned(),
    );
    vars.insert("LLM_CAPTURE_ENABLED".to_owned(), "true".to_owned());
    vars.insert("HA_BASE_URL".to_owned(), "http://127.0.0.1:8123".to_owned());
    vars.insert(
        "HA_TOKEN".to_owned(),
        "home-assistant-secret-token".to_owned(),
    );
    vars.insert(
        "HA_AI_TASK_ENTITY_ID".to_owned(),
        "ai_task.google_ai_task_2".to_owned(),
    );
    vars
}

#[test]
fn secrets_are_not_exposed_in_debug_or_startup_log() {
    let config = config(env_with_secrets()).expect("config should be valid");
    let debug = format!("{config:?}");
    let buffer = LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_writer(buffer.clone())
        .finish();

    tracing::subscriber::with_default(subscriber, || config.log_startup());

    let logs = buffer.contents();
    for secret in [
        "db-password",
        "telegram-secret-token",
        "telegram-webhook-secret",
        "home-assistant-secret-token",
    ] {
        assert!(!debug.contains(secret), "Debug leaked {secret}");
        assert!(!logs.contains(secret), "startup log leaked {secret}");
    }

    assert!(debug.contains("<redacted>"));
    assert!(logs.contains("configuration loaded"));
    assert!(logs.contains("<redacted>"));
}

#[test]
fn errors_do_not_echo_secret_values() {
    let mut vars = base_env();
    vars.insert(
        "DATABASE_URL".to_owned(),
        "postgres://tasklerq:db-secret-password@[::1".to_owned(),
    );

    let error = config(vars).expect_err("invalid DATABASE_URL should be rejected");
    let message = error.to_string();
    assert_invalid(error, "DATABASE_URL");
    assert!(!message.contains("db-secret-password"));
}
