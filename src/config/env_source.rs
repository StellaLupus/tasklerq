//! Чтение переменных окружения и разбор URL.

use std::collections::BTreeMap;

use url::Url;

use super::{ConfigError, DEFAULT_LOG_FILTER};

/// Источник переменных окружения. Пустые значения считаются отсутствующими.
pub(super) struct EnvSource {
    vars: BTreeMap<String, String>,
}

impl EnvSource {
    pub(super) fn from_process() -> Self {
        Self::from_map(std::env::vars().collect())
    }

    pub(super) fn from_map(vars: BTreeMap<String, String>) -> Self {
        Self { vars }
    }

    pub(super) fn get(&self, name: &str) -> Option<&str> {
        self.vars
            .get(name)
            .map(String::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    }

    pub(super) fn required(&self, name: &'static str) -> Result<&str, ConfigError> {
        self.get(name).ok_or(ConfigError::Missing { name })
    }

    pub(super) fn required_url(
        &self,
        name: &'static str,
        require_https: bool,
    ) -> Result<Url, ConfigError> {
        let raw = self.required(name)?;
        parse_url(name, raw, require_https)
    }

    pub(super) fn url(
        &self,
        name: &'static str,
        require_https: bool,
    ) -> Result<Option<Url>, ConfigError> {
        self.get(name)
            .map(|raw| parse_url(name, raw, require_https))
            .transpose()
    }

    pub(super) fn secret(&self, name: &'static str) -> Option<super::SecretString> {
        self.get(name)
            .map(|value| super::SecretString::new(value.to_owned()))
    }

    pub(super) fn bool(&self, name: &'static str, default: bool) -> Result<bool, ConfigError> {
        match self.get(name) {
            None => Ok(default),
            Some("true") => Ok(true),
            Some("false") => Ok(false),
            Some(_) => Err(ConfigError::invalid(name, "expected `true` or `false`")),
        }
    }

    pub(super) fn u32(
        &self,
        name: &'static str,
        default: u32,
        min: u32,
        max: u32,
    ) -> Result<u32, ConfigError> {
        let Some(raw) = self.get(name) else {
            return Ok(default);
        };

        let value = raw.parse::<u32>().map_err(|_| {
            ConfigError::invalid(name, format!("expected an integer in range {min}..={max}"))
        })?;

        if !(min..=max).contains(&value) {
            return Err(ConfigError::invalid(
                name,
                format!("expected an integer in range {min}..={max}"),
            ));
        }

        Ok(value)
    }

    pub(super) fn log_filter(&self) -> String {
        self.get("RUST_LOG")
            .unwrap_or(DEFAULT_LOG_FILTER)
            .to_owned()
    }
}

pub(super) fn parse_url(
    name: &'static str,
    raw: &str,
    require_https: bool,
) -> Result<Url, ConfigError> {
    let url = Url::parse(raw).map_err(|_| {
        ConfigError::invalid(
            name,
            "expected an absolute URL, for example https://tasklerq.example.com",
        )
    })?;

    if !matches!(url.scheme(), "http" | "https") {
        return Err(ConfigError::invalid(
            name,
            "expected an http:// or https:// URL",
        ));
    }
    if url.host_str().is_none() {
        return Err(ConfigError::invalid(name, "URL must contain a host"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(ConfigError::invalid(
            name,
            "URL must not contain credentials",
        ));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(ConfigError::invalid(
            name,
            "URL must not contain a query or fragment",
        ));
    }
    if require_https && url.scheme() != "https" {
        return Err(ConfigError::invalid(
            name,
            "expected an HTTPS URL in production, for example https://tasklerq.example.com",
        ));
    }

    Ok(url)
}
