use std::collections::BTreeMap;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

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
    assert_eq!(config.llm.enabled, Some(false));
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
        ("LLM_PRIMARY_URL", "https://llm.example.com/v1"),
        ("LLM_PRIMARY_MODEL", "anthropic/claude-haiku-5.5"),
    ] {
        vars.insert(name.to_owned(), value.to_owned());
    }

    let config = config(vars).expect("defaults should produce a valid config");

    assert_eq!(config.bind_addr, "0.0.0.0:8080".parse().unwrap());
    assert_eq!(config.telegram.mode, TelegramMode::Webhook);
    // Флаг больше не имеет умолчания `true`: без переменной он не задан.
    assert_eq!(config.llm.enabled, None);
    assert!(config.capture.enabled);
    assert_eq!(config.capture.voice_timeout_ms, 8_000);
    assert!(config.capture.voice_fallback_raw);
    assert_eq!(config.capture.rate_limit_per_min, 10);
    assert_eq!(config.capture.max_text_chars, 2_000);
    assert_eq!(config.llm.daily_quota, 200);
    assert_eq!(config.llm.connect_timeout, Duration::from_secs(1));
    let primary = config.llm.primary.expect("primary slot");
    assert_eq!(primary.timeout, Duration::from_secs(10));
    assert!(config.llm.fallback.is_none());
    assert!(config.deprecated_env.is_empty());
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
fn tenancy_defaults_to_single_without_bootstrap_owner() {
    let config = config(base_env()).expect("config should be valid");

    assert_eq!(config.tenancy.mode, TenancyMode::Single);
    assert_eq!(config.tenancy.bootstrap_owner, None);
}

#[test]
fn blank_bootstrap_owner_is_treated_as_missing() {
    // `.env.example` поставляется с пустым `BOOTSTRAP_OWNER_TELEGRAM_ID=`, а
    // окружение может содержать одно пробельное значение.
    for value in ["", "   "] {
        let mut vars = base_env();
        vars.insert("BOOTSTRAP_OWNER_TELEGRAM_ID".to_owned(), value.to_owned());

        let config = config(vars).expect("blank optional value should be ignored");
        assert_eq!(config.tenancy.bootstrap_owner, None, "value: {value:?}");
    }
}

#[test]
fn tenancy_mode_accepts_multi() {
    let mut vars = base_env();
    vars.insert("TENANCY_MODE".to_owned(), "multi".to_owned());

    let config = config(vars).expect("multi is a valid tenancy mode");
    assert_eq!(config.tenancy.mode, TenancyMode::Multi);
}

#[test]
fn invalid_tenancy_mode_lists_allowed_values() {
    let mut vars = base_env();
    vars.insert("TENANCY_MODE".to_owned(), "saas".to_owned());

    let error = config(vars).expect_err("unknown tenancy mode should be rejected");
    let message = error.to_string();
    assert_invalid(error, "TENANCY_MODE");
    assert!(
        message.contains("single"),
        "message lists single: {message}"
    );
    assert!(message.contains("multi"), "message lists multi: {message}");
}

#[test]
fn bootstrap_owner_telegram_id_must_be_positive() {
    for value in ["abc", "0", "-5"] {
        let mut vars = base_env();
        vars.insert("BOOTSTRAP_OWNER_TELEGRAM_ID".to_owned(), value.to_owned());

        let error = config(vars).expect_err("invalid Telegram id should be rejected");
        let message = error.to_string();
        assert_invalid(error, "BOOTSTRAP_OWNER_TELEGRAM_ID");
        assert!(
            message.contains("BOOTSTRAP_OWNER_TELEGRAM_ID"),
            "message names the parameter: {message}"
        );
        assert!(
            message.contains("positive integer"),
            "message describes the expected format: {message}"
        );
    }
}

#[test]
fn bootstrap_owner_telegram_id_is_loaded() {
    let mut vars = base_env();
    vars.insert("BOOTSTRAP_OWNER_TELEGRAM_ID".to_owned(), "111".to_owned());

    let config = config(vars).expect("positive Telegram id should be accepted");
    assert_eq!(config.tenancy.bootstrap_owner, Some(111));
}

#[test]
fn database_url_preserves_sslmode() {
    let mut vars = base_env();
    vars.insert(
        "DATABASE_URL".to_owned(),
        "postgres://tasklerq:db-password@db.example:5432/tasklerq?sslmode=require".to_owned(),
    );

    let config = config(vars).expect("config with sslmode");

    // Настройку TLS выбирает строка подключения; конфигурация не должна её
    // терять или переписывать. Для внешней БД `sslmode=require` — 15.2.
    assert!(
        config.database.url.expose().contains("sslmode=require"),
        "sslmode must survive config parsing"
    );
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
fn public_base_url_is_optional() {
    let mut vars = base_env();
    vars.remove("PUBLIC_BASE_URL");

    let config = config(vars).expect("PUBLIC_BASE_URL is optional");
    assert_eq!(config.public_base_url, None);
}

#[test]
fn minimal_production_config_without_optional_variables() {
    // Минимальный запуск: ни `ENV`, ни `PUBLIC_BASE_URL`, ни консоли, ни
    // ключей шифрования, ни слотов LLM.
    let config = Config::from_map([
        (
            "DATABASE_URL",
            "postgres://tasklerq:db-password@127.0.0.1:5432/tasklerq?sslmode=disable",
        ),
        ("TELEGRAM_MODE", "disabled"),
    ])
    .expect("minimal production config should be valid");

    assert_eq!(config.env, Environment::Production);
    assert_eq!(config.public_base_url, None);
    assert_eq!(config.llm.enabled, None);
    assert!(config.llm.primary.is_none());
    assert!(config.llm.fallback.is_none());
    assert_eq!(
        config.admin_bind_addr,
        Some("0.0.0.0:8081".parse().unwrap())
    );
    assert!(config.admin_trusted_proxies.is_empty());
    assert!(config.secret_key.is_none());
    assert!(config.secret_key_old.is_none());
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

#[test]
fn admin_bind_addr_defaults_to_8081_and_accepts_disabled() {
    let config = config(base_env()).expect("config should be valid");
    assert_eq!(
        config.admin_bind_addr,
        Some("0.0.0.0:8081".parse().unwrap())
    );

    let config = config_with(&[("ADMIN_BIND_ADDR", "127.0.0.1:9091")])
        .expect("an explicit address is accepted");
    assert_eq!(
        config.admin_bind_addr,
        Some("127.0.0.1:9091".parse().unwrap())
    );

    let config =
        config_with(&[("ADMIN_BIND_ADDR", "disabled")]).expect("disabled turns the listener off");
    assert_eq!(config.admin_bind_addr, None);
}

#[test]
fn admin_bind_addr_errors_name_the_parameter_without_the_value() {
    for raw in ["localhost:abc", "not-an-address", "127.0.0.1"] {
        let error = config_with(&[("ADMIN_BIND_ADDR", raw)])
            .expect_err("invalid admin bind address should be rejected");
        let message = error.to_string();
        assert_invalid(error, "ADMIN_BIND_ADDR");
        assert!(
            !message.contains(raw),
            "error must not echo {raw}: {message}"
        );
        assert!(
            message.contains("disabled"),
            "error describes the disabled literal: {message}"
        );
    }
}

#[test]
fn trusted_proxies_parse_cidr_lists() {
    let config = config_with(&[("ADMIN_TRUSTED_PROXIES", "")])
        .expect("a blank value means no trusted proxies");
    assert!(config.admin_trusted_proxies.is_empty());

    let config = config_with(&[(
        "ADMIN_TRUSTED_PROXIES",
        " 10.0.0.0/8 , 172.16.0.0/12,2001:db8::/32 , 192.0.2.1 ",
    )])
    .expect("spaces around the list elements are allowed");
    assert_eq!(config.admin_trusted_proxies.len(), 4);
    assert_eq!(config.admin_trusted_proxies[0].to_string(), "10.0.0.0/8");
    assert_eq!(config.admin_trusted_proxies[3].to_string(), "192.0.2.1/32");
}

#[test]
fn trusted_proxies_reject_invalid_cidr_without_echoing_the_value() {
    for raw in [
        "10.0.0.0/33",
        "2001:db8::/129",
        "garbage",
        "10.0.0.0/not-a-number",
    ] {
        let error = config_with(&[("ADMIN_TRUSTED_PROXIES", raw)])
            .expect_err("invalid CIDR should be rejected");
        let message = error.to_string();
        assert_invalid(error, "ADMIN_TRUSTED_PROXIES");
        assert!(
            !message.contains(raw),
            "error must not echo {raw}: {message}"
        );
    }
}

#[test]
fn ip_net_contains_covers_ipv4_ipv6_and_prefix_boundaries() {
    let ipv4 = IpNet::parse("10.0.0.0/8").expect("IPv4 network");
    assert!(ipv4.contains("10.0.0.1".parse().unwrap()));
    assert!(ipv4.contains("10.255.255.255".parse().unwrap()));
    assert!(!ipv4.contains("11.0.0.0".parse().unwrap()));
    assert!(!ipv4.contains("2001:db8::1".parse().unwrap()));

    let host = IpNet::parse("192.0.2.7").expect("an address without a prefix is one host");
    assert_eq!(host.prefix_len(), 32);
    assert!(host.contains("192.0.2.7".parse().unwrap()));
    assert!(!host.contains("192.0.2.8".parse().unwrap()));

    let zero = IpNet::parse("0.0.0.0/0").expect("the default route matches any IPv4 address");
    assert!(zero.contains("203.0.113.5".parse().unwrap()));
    assert!(!zero.contains("2001:db8::1".parse().unwrap()));

    let ipv6 = IpNet::parse("2001:db8::/32").expect("IPv6 network");
    assert!(ipv6.contains("2001:db8::1".parse().unwrap()));
    assert!(ipv6.contains("2001:db8:ffff::1".parse().unwrap()));
    assert!(!ipv6.contains("2001:db9::1".parse().unwrap()));
    assert!(!ipv6.contains("10.0.0.1".parse().unwrap()));

    let v6_host = IpNet::parse("2001:db8::1").expect("an IPv6 address is a /128 network");
    assert_eq!(v6_host.prefix_len(), 128);
    assert!(v6_host.contains("2001:db8::1".parse().unwrap()));
    assert!(!v6_host.contains("2001:db8::2".parse().unwrap()));

    let v6_all = IpNet::parse("::/0").expect("the default route matches any IPv6 address");
    assert!(v6_all.contains("2001:db8::1".parse().unwrap()));
    assert!(!v6_all.contains("10.0.0.1".parse().unwrap()));
}

/// Base64 стандартного алфавита для 32 байт `0x00..=0x1f`.
const SECRET_KEY_BASE64: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";
/// Base64 стандартного алфавита для 16 байт: слишком короткий ключ.
const SHORT_SECRET_KEY_BASE64: &str = "AAECAwQFBgcICQoLDA0ODw==";

#[test]
fn secret_keys_are_decoded_into_thirty_two_bytes() {
    let config = config_with(&[
        ("TASKLERQ_SECRET_KEY", SECRET_KEY_BASE64),
        ("TASKLERQ_SECRET_KEY_OLD", SECRET_KEY_BASE64),
    ])
    .expect("valid keys are accepted");

    let expected: [u8; 32] = std::array::from_fn(|index| index as u8);
    assert_eq!(
        config.secret_key.as_ref().map(SecretKey::expose),
        Some(&expected)
    );
    assert_eq!(
        config.secret_key_old.as_ref().map(SecretKey::expose),
        Some(&expected)
    );
}

#[test]
fn secret_key_is_hidden_in_config_debug() {
    let config = config_with(&[
        ("TASKLERQ_SECRET_KEY", SECRET_KEY_BASE64),
        ("TASKLERQ_SECRET_KEY_OLD", SECRET_KEY_BASE64),
    ])
    .expect("valid keys are accepted");

    // Поле ключа целиком: у Option<SecretKey> от ключа остаётся только признак
    // «задан» и редактированный вид, а не десятичный список байт `[0, 1, ...]`.
    assert_eq!(format!("{:?}", config.secret_key), "Some(<redacted>)");
    assert_eq!(format!("{:?}", config.secret_key_old), "Some(<redacted>)");

    let debug = format!("{config:?}");
    assert!(
        debug.contains("secret_key: Some(<redacted>)"),
        "the current key field must be redacted: {debug}"
    );
    assert!(
        debug.contains("secret_key_old: Some(<redacted>)"),
        "the old key field must be redacted: {debug}"
    );
    assert!(!debug.contains(SECRET_KEY_BASE64));
    assert!(
        !debug.contains("[0, 1, 2"),
        "key bytes must not be printed as a decimal list: {debug}"
    );
}

#[test]
fn secret_key_errors_name_the_parameter_and_expected_format() {
    for (name, raw) in [
        ("TASKLERQ_SECRET_KEY", SHORT_SECRET_KEY_BASE64),
        ("TASKLERQ_SECRET_KEY", "not base64!"),
        ("TASKLERQ_SECRET_KEY_OLD", SHORT_SECRET_KEY_BASE64),
    ] {
        let error = config_with(&[(name, raw)]).expect_err("invalid key should be rejected");
        let message = error.to_string();
        assert_invalid(error, name);
        assert!(
            !message.contains(raw),
            "error must not echo the key: {message}"
        );
        assert!(
            message.contains("32 bytes"),
            "error describes the expected format: {message}"
        );
    }
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

const PRIMARY_URL: &str = "https://llm.example.com/v1";
const PRIMARY_MODEL: &str = "anthropic/claude-haiku-5.5";

/// Собрать конфигурацию из базового окружения и перечисленных пар.
fn config_with(pairs: &[(&str, &str)]) -> Result<Config, ConfigError> {
    let mut vars = base_env();
    for (name, value) in pairs {
        vars.insert((*name).to_owned(), (*value).to_owned());
    }
    config(vars)
}

fn llm_with(pairs: &[(&str, &str)]) -> Result<LlmConfig, ConfigError> {
    config_with(pairs).map(|config| config.llm)
}

/// Включённый разбор с полностью заданным основным слотом.
fn enabled_primary() -> Vec<(&'static str, &'static str)> {
    vec![
        ("LLM_CAPTURE_ENABLED", "true"),
        ("LLM_PRIMARY_URL", PRIMARY_URL),
        ("LLM_PRIMARY_MODEL", PRIMARY_MODEL),
    ]
}

#[test]
fn llm_requires_primary_slot_when_enabled() {
    let cases = [
        (vec![("LLM_CAPTURE_ENABLED", "true")], "LLM_PRIMARY_URL"),
        (
            vec![
                ("LLM_CAPTURE_ENABLED", "true"),
                ("LLM_PRIMARY_URL", PRIMARY_URL),
            ],
            "LLM_PRIMARY_MODEL",
        ),
    ];

    for (pairs, expected) in cases {
        let error = llm_with(&pairs).expect_err("primary slot is required");
        assert_missing(error, expected);
    }
}

#[test]
fn llm_enabled_flag_is_optional_and_only_true_requires_a_slot() {
    // `None` — переменная не задана; слоты берутся из консоли (2.3).
    let mut vars = base_env();
    vars.remove("LLM_CAPTURE_ENABLED");
    let absent = config(vars).expect("an absent flag does not require a slot");
    assert_eq!(absent.llm.enabled, None);
    assert!(absent.llm.primary.is_none());

    // Явный `false` без слота — успех.
    let disabled = config(base_env()).expect("an explicit false does not require a slot");
    assert_eq!(disabled.llm.enabled, Some(false));
    assert!(disabled.llm.primary.is_none());

    // Явный `true` без слота — прежняя ошибка с именем переменной.
    let error = llm_with(&[("LLM_CAPTURE_ENABLED", "true")])
        .expect_err("an explicit true requires the primary slot");
    assert_missing(error, "LLM_PRIMARY_URL");
}

#[test]
fn slot_variables_without_url_or_model_are_ignored() {
    let config = config_with(&[("LLM_PRIMARY_TIMEOUT_MS", "10000")])
        .expect("a lone slot variable must not stop startup");
    assert!(config.llm.primary.is_none());
    assert_eq!(config.llm.ignored_env, vec!["LLM_PRIMARY_TIMEOUT_MS"]);

    // Невалидное значение одиночной переменной даже не разбирается.
    let config = config_with(&[("LLM_PRIMARY_TIMEOUT_MS", "abc")])
        .expect("a lone invalid slot variable must not be parsed");
    assert_eq!(config.llm.ignored_env, vec!["LLM_PRIMARY_TIMEOUT_MS"]);

    let config = config_with(&[
        ("LLM_PRIMARY_TOKEN", "primary-token"),
        ("LLM_PRIMARY_TIMEOUT_MS", "10000"),
        ("LLM_PRIMARY_EXTRA_JSON", "not json"),
        ("LLM_FALLBACK_TOKEN", "fallback-token"),
        ("LLM_FALLBACK_TIMEOUT_MS", "abc"),
        ("LLM_FALLBACK_EXTRA_JSON", "[1]"),
    ])
    .expect("lone variables of both slots are ignored");
    assert_eq!(
        config.llm.ignored_env,
        vec![
            "LLM_PRIMARY_TOKEN",
            "LLM_PRIMARY_TIMEOUT_MS",
            "LLM_PRIMARY_EXTRA_JSON",
            "LLM_FALLBACK_TOKEN",
            "LLM_FALLBACK_TIMEOUT_MS",
            "LLM_FALLBACK_EXTRA_JSON",
        ]
    );
}

#[test]
fn ignored_slot_variable_is_logged_without_its_value() {
    let config =
        config_with(&[("LLM_PRIMARY_TIMEOUT_MS", "10000")]).expect("config should be valid");
    let buffer = LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_writer(buffer.clone())
        .finish();

    tracing::subscriber::with_default(subscriber, || config.log_startup());

    let logs = buffer.contents();
    assert!(
        logs.contains("LLM_PRIMARY_TIMEOUT_MS"),
        "the warning names the ignored variable: {logs}"
    );
    assert!(
        logs.contains("\"level\":\"WARN\""),
        "the warning is logged at WARN level: {logs}"
    );
    assert!(
        logs.contains("ignored"),
        "the warning says the variable is ignored: {logs}"
    );
    assert!(
        !logs.contains("10000"),
        "the warning must not contain the value: {logs}"
    );
}

#[test]
fn slot_parsers_use_the_same_limits_and_never_echo_the_value() {
    for raw in ["100", "600000"] {
        assert_eq!(
            parse_slot_timeout_ms(raw).expect("boundary timeout"),
            Duration::from_millis(raw.parse::<u64>().unwrap())
        );
    }
    for raw in ["99", "600001", "abc"] {
        let error = parse_slot_timeout_ms(raw).expect_err("out-of-range timeout");
        assert!(
            error.contains("100..=600000"),
            "reason describes the range: {error}"
        );
        assert!(!error.contains(raw), "reason must not echo {raw}: {error}");
    }

    for raw in ["10", "60000"] {
        assert_eq!(
            parse_connect_timeout_ms(raw).expect("boundary connect timeout"),
            Duration::from_millis(raw.parse::<u64>().unwrap())
        );
    }
    assert!(parse_connect_timeout_ms("9").is_err());
    assert!(parse_connect_timeout_ms("60001").is_err());

    for raw in ["1", "100000"] {
        assert_eq!(
            parse_daily_quota(raw).expect("boundary quota"),
            raw.parse::<u32>().unwrap()
        );
    }
    for raw in ["0", "100001", "many"] {
        let error = parse_daily_quota(raw).expect_err("out-of-range quota");
        assert!(
            error.contains("1..=100000"),
            "reason describes the range: {error}"
        );
        if raw == "many" {
            assert!(!error.contains(raw), "reason must not echo {raw}: {error}");
        }
    }

    let extra = parse_extra_json(r#"{"reasoning":{"effort":"minimal"}}"#).expect("JSON object");
    assert_eq!(
        extra.get("reasoning"),
        Some(&serde_json::json!({"effort": "minimal"}))
    );
    for raw in ["[1,2]", "\"text\"", "{not json"] {
        let error = parse_extra_json(raw).expect_err("not a JSON object");
        assert!(
            error.contains("JSON object"),
            "reason describes the format: {error}"
        );
        assert!(!error.contains(raw), "reason must not echo {raw}: {error}");
    }

    for raw in ["https://llm.example.com/v1", "http://127.0.0.1:8080/v1"] {
        assert_eq!(parse_slot_url(raw).expect("valid slot url").as_str(), raw);
    }
    for raw in [
        "ftp://llm.example.com/v1",
        "https://user:password@llm.example.com/v1",
        "https://llm.example.com/v1?token=1",
        "https://llm.example.com/v1#fragment",
        "not a url",
    ] {
        let error = parse_slot_url(raw).expect_err("invalid slot url");
        assert!(!error.contains(raw), "reason must not echo {raw}: {error}");
    }
}

#[test]
fn fallback_slot_must_be_configured_completely() {
    let mut pairs = enabled_primary();
    pairs.push(("LLM_FALLBACK_URL", "https://backup.example.com/v1"));
    assert_missing(
        llm_with(&pairs).expect_err("half fallback is rejected"),
        "LLM_FALLBACK_MODEL",
    );

    let mut pairs = enabled_primary();
    pairs.push(("LLM_FALLBACK_MODEL", "local/qwen3.8-4b"));
    assert_missing(
        llm_with(&pairs).expect_err("half fallback is rejected"),
        "LLM_FALLBACK_URL",
    );
}

#[test]
fn primary_slot_alone_means_no_fallback() {
    let llm = llm_with(&enabled_primary()).expect("primary slot is enough");
    let primary = llm.primary.expect("primary slot");

    assert_eq!(primary.url.as_str(), PRIMARY_URL);
    assert_eq!(primary.model, PRIMARY_MODEL);
    assert!(primary.token.is_none());
    assert!(primary.extra.is_empty());
    assert!(llm.fallback.is_none());
}

#[test]
fn both_slots_are_loaded_with_token_and_extra() {
    let pairs = [
        ("LLM_CAPTURE_ENABLED", "true"),
        ("LLM_PRIMARY_URL", PRIMARY_URL),
        ("LLM_PRIMARY_MODEL", PRIMARY_MODEL),
        ("LLM_PRIMARY_TOKEN", "primary-token"),
        (
            "LLM_PRIMARY_EXTRA_JSON",
            r#"{"reasoning":{"effort":"minimal"}}"#,
        ),
        ("LLM_FALLBACK_URL", "https://backup.example.com/v1"),
        ("LLM_FALLBACK_MODEL", "local/qwen3.8-4b"),
        ("LLM_FALLBACK_TOKEN", "fallback-token"),
    ];
    let llm = llm_with(&pairs).expect("both slots are configured");

    let primary = llm.primary.expect("primary slot");
    assert_eq!(
        primary.token.as_ref().map(SecretString::expose),
        Some("primary-token")
    );
    assert_eq!(
        primary.extra.get("reasoning"),
        Some(&serde_json::json!({"effort": "minimal"}))
    );

    let fallback = llm.fallback.expect("fallback slot");
    assert_eq!(fallback.url.as_str(), "https://backup.example.com/v1");
    assert_eq!(fallback.model, "local/qwen3.8-4b");
    assert_eq!(
        fallback.token.as_ref().map(SecretString::expose),
        Some("fallback-token")
    );
}

#[test]
fn llm_slot_timeouts_have_defaults_and_are_configurable() {
    let llm = llm_with(&enabled_primary()).expect("defaults");
    assert_eq!(
        llm.primary.expect("primary slot").timeout,
        Duration::from_secs(10)
    );
    assert_eq!(llm.connect_timeout, Duration::from_secs(1));

    let mut pairs = enabled_primary();
    pairs.push(("LLM_PRIMARY_TIMEOUT_MS", "2500"));
    pairs.push(("LLM_CONNECT_TIMEOUT_MS", "200"));
    let llm = llm_with(&pairs).expect("configured timeouts");
    assert_eq!(
        llm.primary.expect("primary slot").timeout,
        Duration::from_millis(2_500)
    );
    assert_eq!(llm.connect_timeout, Duration::from_millis(200));

    let mut pairs = enabled_primary();
    pairs.push(("LLM_PRIMARY_TIMEOUT_MS", "99"));
    assert_invalid(
        llm_with(&pairs).expect_err("too small slot timeout"),
        "LLM_PRIMARY_TIMEOUT_MS",
    );

    let mut vars = base_env();
    vars.insert("LLM_CONNECT_TIMEOUT_MS".to_owned(), "60001".to_owned());
    assert_invalid(
        config(vars).expect_err("too large connect timeout"),
        "LLM_CONNECT_TIMEOUT_MS",
    );
}

#[test]
fn extra_json_must_be_a_json_object() {
    let mut pairs = enabled_primary();
    pairs.push((
        "LLM_PRIMARY_EXTRA_JSON",
        r#"{"reasoning":{"effort":"minimal"}}"#,
    ));
    let llm = llm_with(&pairs).expect("JSON object is accepted");
    let extra = llm.primary.expect("primary slot").extra;
    assert_eq!(
        extra.get("reasoning"),
        Some(&serde_json::json!({"effort": "minimal"}))
    );
    assert_eq!(extra.len(), 1);

    for raw in ["[1,2]", "\"text\"", "{not json", "{\"key\": }"] {
        let mut pairs = enabled_primary();
        pairs.push(("LLM_PRIMARY_EXTRA_JSON", raw));

        let error = llm_with(&pairs).expect_err("not a JSON object is rejected");
        let message = error.to_string();
        assert_invalid(error, "LLM_PRIMARY_EXTRA_JSON");
        assert!(
            !message.contains(raw),
            "error must not echo the value {raw}: {message}"
        );
    }
}

#[test]
fn disabled_llm_does_not_require_slots_but_validates_given_ones() {
    let llm = llm_with(&[]).expect("no slots are fine when LLM is off");
    assert_eq!(llm.enabled, Some(false));
    assert!(llm.primary.is_none());
    assert!(llm.fallback.is_none());

    let pairs = [
        ("LLM_PRIMARY_URL", PRIMARY_URL),
        ("LLM_PRIMARY_MODEL", PRIMARY_MODEL),
        ("LLM_PRIMARY_TOKEN", "primary-token"),
    ];
    let llm = llm_with(&pairs).expect("configured slot is loaded while LLM is off");
    assert_eq!(llm.enabled, Some(false));
    assert!(llm.primary.is_some());

    assert_invalid(
        llm_with(&[("LLM_PRIMARY_URL", "ftp://llm.example.com/v1")])
            .expect_err("URL is validated even when LLM is off"),
        "LLM_PRIMARY_URL",
    );
    assert_missing(
        llm_with(&[("LLM_PRIMARY_URL", PRIMARY_URL)])
            .expect_err("half slot is rejected even when LLM is off"),
        "LLM_PRIMARY_MODEL",
    );
}

#[test]
fn legacy_home_assistant_variables_are_collected() {
    let mut pairs = enabled_primary();
    pairs.push(("HA_BASE_URL", "http://127.0.0.1:8123"));
    pairs.push(("HA_TOKEN", "home-assistant-secret-token"));
    pairs.push(("HA_AI_TASK_ENTITY_ID", "legacy.entity"));

    let config = config_with(&pairs).expect("legacy variables must not stop startup");
    assert_eq!(
        config.deprecated_env,
        vec!["HA_BASE_URL", "HA_TOKEN", "HA_AI_TASK_ENTITY_ID"]
    );

    // Значения устаревших переменных не читаются и не сохраняются.
    let debug = format!("{config:?}");
    assert!(!debug.contains("home-assistant-secret-token"));
    let llm = &config.llm;
    assert!(llm.fallback.is_none());
    assert!(llm.primary.is_some());
}

#[test]
fn slot_debug_hides_token_and_extra_values() {
    let mut pairs = enabled_primary();
    pairs.push(("LLM_PRIMARY_TOKEN", "SECRET-slot-token"));
    pairs.push((
        "LLM_PRIMARY_EXTRA_JSON",
        r#"{"api_key":"SECRET-extra-value","reasoning":{"effort":"minimal"}}"#,
    ));
    let config = config_with(&pairs).expect("config");

    let debug = format!("{config:?}");
    assert!(!debug.contains("SECRET-slot-token"));
    assert!(!debug.contains("SECRET-extra-value"));
    assert!(debug.contains("has_token: true"));
    assert!(debug.contains("extra_keys"));
    assert!(debug.contains("api_key"));
    assert!(debug.contains("reasoning"));
}

#[test]
fn numeric_limits_are_enforced() {
    for (name, too_small, too_large) in [
        ("CAPTURE_VOICE_TIMEOUT_MS", "999", "60001"),
        ("CAPTURE_RATE_LIMIT_PER_MIN", "0", "1001"),
        ("CAPTURE_MAX_TEXT_CHARS", "99", "2001"),
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
    vars.insert("LLM_PRIMARY_URL".to_owned(), PRIMARY_URL.to_owned());
    vars.insert("LLM_PRIMARY_MODEL".to_owned(), PRIMARY_MODEL.to_owned());
    vars.insert(
        "LLM_PRIMARY_TOKEN".to_owned(),
        "primary-slot-secret-token".to_owned(),
    );
    vars.insert(
        "LLM_PRIMARY_EXTRA_JSON".to_owned(),
        r#"{"api_key":"primary-extra-secret"}"#.to_owned(),
    );
    vars.insert(
        "LLM_FALLBACK_URL".to_owned(),
        "https://backup.example.com/v1".to_owned(),
    );
    vars.insert(
        "LLM_FALLBACK_MODEL".to_owned(),
        "local/qwen3.8-4b".to_owned(),
    );
    vars.insert(
        "LLM_FALLBACK_TOKEN".to_owned(),
        "fallback-slot-secret-token".to_owned(),
    );
    vars.insert(
        "HA_TOKEN".to_owned(),
        "home-assistant-secret-token".to_owned(),
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
        "primary-slot-secret-token",
        "primary-extra-secret",
        "fallback-slot-secret-token",
        "home-assistant-secret-token",
    ] {
        assert!(!debug.contains(secret), "Debug leaked {secret}");
        assert!(!logs.contains(secret), "startup log leaked {secret}");
    }

    assert!(debug.contains("<redacted>"));
    assert!(logs.contains("configuration loaded"));
    assert!(logs.contains("<redacted>"));
    // Устаревшая переменная Home Assistant названа, но её значение скрыто.
    assert!(logs.contains("HA_TOKEN"));
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

#[test]
fn telegram_api_base_url_allows_http_only_in_development() {
    let mut vars = base_env();
    vars.insert(
        "TELEGRAM_API_BASE_URL".to_owned(),
        "http://127.0.0.1:8080".to_owned(),
    );
    let loaded = config(vars).expect("http is allowed in development");
    assert_eq!(loaded.telegram.api_base_url.expect("url").scheme(), "http");

    let mut vars = base_env();
    vars.insert("ENV".to_owned(), "production".to_owned());
    vars.insert(
        "PUBLIC_BASE_URL".to_owned(),
        "https://tasklerq.example.com".to_owned(),
    );
    vars.insert(
        "TELEGRAM_API_BASE_URL".to_owned(),
        "http://127.0.0.1:8080".to_owned(),
    );
    assert_invalid(
        config(vars).expect_err("http must be rejected outside development"),
        "TELEGRAM_API_BASE_URL",
    );

    let mut vars = base_env();
    vars.insert("ENV".to_owned(), "production".to_owned());
    vars.insert(
        "PUBLIC_BASE_URL".to_owned(),
        "https://tasklerq.example.com".to_owned(),
    );
    vars.insert(
        "TELEGRAM_API_BASE_URL".to_owned(),
        "https://api.telegram.org".to_owned(),
    );
    let loaded = config(vars).expect("https is always allowed");
    assert_eq!(loaded.telegram.api_base_url.expect("url").scheme(), "https");
}

/// Активные строки `.env.example`: без комментариев и пустых строк.
fn env_example_active_vars() -> Vec<(String, String)> {
    include_str!("../../.env.example")
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| {
            let (key, value) = line
                .split_once('=')
                .expect("an active .env.example line contains `=`");
            (key.trim().to_owned(), value.trim().to_owned())
        })
        .collect()
}

#[test]
fn env_example_leaves_every_llm_field_to_the_console() {
    // Чистый `.env.example` не задаёт ни одного поля LLM: страница LLM в
    // консоли остаётся редактируемой, а ключи шифрования не заданы.
    let config =
        Config::from_map(env_example_active_vars()).expect(".env.example should be a valid config");

    assert_eq!(config.llm.enabled, None);
    assert!(config.llm.primary.is_none());
    assert!(config.llm.fallback.is_none());
    assert!(
        config.llm.ignored_env.is_empty(),
        "no slot variable may be active: {:?}",
        config.llm.ignored_env
    );
    assert!(config.secret_key.is_none());
    assert!(config.secret_key_old.is_none());
}
