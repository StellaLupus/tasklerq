//! Сверка `Origin`/`Referer` изменяющих запросов консоли с origin запроса.
//!
//! Origin запроса — тройка (схема, хост, порт): схема из [`Client::scheme`],
//! хост и порт — разбор [`Client::host`] (значение `Host` или
//! `X-Forwarded-Host` от доверенного прокси). Порт по умолчанию — 80 для
//! `http` и 443 для `https`. Значение хоста разбирается, а не сравнивается
//! строкой: мусор, `user@host` и путь делают origin запроса неизвестным, и
//! запрос отклоняется.
//!
//! Источник — заголовок `Origin`; при его отсутствии `Referer`, у которого
//! берутся только схема, хост и порт. `Origin: null`, неразбираемое значение и
//! отсутствие обоих заголовков отклоняются. Если `Origin` есть, но не
//! совпадает, на `Referer` не откатываемся: иначе чужой сайт мог бы прислать
//! неверный `Origin` и верный `Referer`.
//!
//! Cookie консоли привязана к хосту, а не к порту, и `SameSite` не разделяет
//! соседние порты, поэтому проверяются **все** не-GET-запросы, включая вход,
//! setup и восстановление.

use std::fmt;

use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use url::Url;

use crate::http::client::{Client, Scheme};

/// Origin запроса или источника: схема, хост и порт.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Origin {
    scheme: Scheme,
    /// Хост в каноническом виде: домен в нижнем регистре, IPv6 в скобках.
    host: String,
    port: u16,
}

impl Origin {
    /// Совпадают ли origin'ы: схема и порт точно, хост без учёта регистра.
    fn matches(&self, other: &Self) -> bool {
        self.scheme == other.scheme
            && self.port == other.port
            && self.host.eq_ignore_ascii_case(&other.host)
    }
}

/// Строка origin в виде `схема://хост[:порт]`.
///
/// Порт по умолчанию для схемы не пишется: так же браузер сериализует
/// заголовок `Origin`.
impl fmt::Display for Origin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}://{}", self.scheme.as_str(), self.host)?;
        if self.port != default_port(self.scheme) {
            write!(formatter, ":{}", self.port)?;
        }
        Ok(())
    }
}

/// Совпадает ли источник изменяющего запроса с origin запроса.
///
/// `false` — запрос нужно отклонить: origin запроса неизвестен, источника нет
/// или они различаются.
pub fn allows(client: &Client, headers: &HeaderMap) -> bool {
    let Some(request) = request_origin(client) else {
        return false;
    };
    let Some(source) = source_origin(headers) else {
        return false;
    };
    request.matches(&source)
}

/// Origin запроса по клиенту: схема и разобранный `Host`.
///
/// `None`, если хоста нет или его нельзя разобрать на хост и порт.
pub fn request_origin(client: &Client) -> Option<Origin> {
    let authority = client.host.as_deref()?;
    authority_origin(client.scheme, authority)
}

/// Origin источника: `Origin`, а при его отсутствии — `Referer`.
///
/// Наличие `Origin` отменяет откат на `Referer`, даже если значение
/// неразбираемо.
pub fn source_origin(headers: &HeaderMap) -> Option<Origin> {
    if let Some(value) = headers.get(header::ORIGIN) {
        return origin_header(value.to_str().ok()?);
    }
    let value = headers.get(header::REFERER)?.to_str().ok()?;
    from_url(&Url::parse(value.trim()).ok()?)
}

/// Ответ на несовпадение: короткий текст без подробностей причины.
pub fn forbidden() -> Response {
    (StatusCode::FORBIDDEN, "Forbidden").into_response()
}

/// Разобрать `Host`/`X-Forwarded-Host` как authority и собрать origin.
///
/// Разбор строгий, без нормализации `Url::parse`: пустой порт (`nas:`),
/// обратный слэш, знаки табуляции и перевода строки, любые символы вне
/// `[A-Za-z0-9.\-:\[\]]` — уже не authority, и origin считается неизвестным.
/// `Url` молча превратил бы `nas:` в `nas:80`, а `nas\path` — в путь.
fn authority_origin(scheme: Scheme, authority: &str) -> Option<Origin> {
    if authority.is_empty()
        || !authority
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | ':' | '[' | ']'))
    {
        return None;
    }

    let (host, port) = split_authority(scheme, authority)?;
    Some(Origin { scheme, host, port })
}

/// Разделить authority на канонический хост и порт.
///
/// IPv6 допускается только в скобках: без них двоеточия неотличимы от
/// разделителя порта. Пустой порт и нецифровое значение — не authority.
fn split_authority(scheme: Scheme, authority: &str) -> Option<(String, u16)> {
    if let Some(rest) = authority.strip_prefix('[') {
        let (address, after) = rest.split_once(']')?;
        if address.is_empty()
            || !address
                .chars()
                .all(|ch| ch.is_ascii_hexdigit() || matches!(ch, ':' | '.'))
        {
            return None;
        }
        let address: std::net::Ipv6Addr = address.parse().ok()?;
        let port = match after {
            "" => default_port(scheme),
            _ => parse_port(after.strip_prefix(':')?)?,
        };
        return Some((format!("[{address}]"), port));
    }

    let (host, port) = match authority.split_once(':') {
        Some((host, port)) => (host, parse_port(port)?),
        None => (authority, default_port(scheme)),
    };
    if host.is_empty()
        || !host
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-'))
    {
        return None;
    }
    Some((host.to_ascii_lowercase(), port))
}

/// Порт по умолчанию для схемы.
const fn default_port(scheme: Scheme) -> u16 {
    match scheme {
        Scheme::Http => 80,
        Scheme::Https => 443,
    }
}

/// Порт обязан быть непустым числом в диапазоне `u16`; ноль не допускается.
fn parse_port(value: &str) -> Option<u16> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse::<u16>().ok().filter(|port| *port != 0)
}

/// Разобрать значение `Origin`.
///
/// Заголовок обязан содержать только origin: `null`, путь, query, fragment и
/// userinfo отклоняются. Браузер присылает сериализованный origin без пути.
fn origin_header(value: &str) -> Option<Origin> {
    let value = value.trim();
    if value.is_empty() || value.eq_ignore_ascii_case("null") {
        return None;
    }
    let url = Url::parse(value).ok()?;
    if !url.username().is_empty() || url.password().is_some() {
        return None;
    }
    if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
        return None;
    }
    from_url(&url)
}

/// Схема, хост и порт URL; порт — явный или по умолчанию для схемы.
fn from_url(url: &Url) -> Option<Origin> {
    let scheme = match url.scheme() {
        "http" => Scheme::Http,
        "https" => Scheme::Https,
        _ => return None,
    };
    let host = url.host_str()?.to_owned();
    let port = url.port_or_known_default()?;
    Some(Origin { scheme, host, port })
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderMap, HeaderName, HeaderValue};

    use crate::http::client::{Client, Scheme};

    use super::{allows, request_origin};

    fn client(scheme: Scheme, host: &str) -> Client {
        Client {
            ip: "127.0.0.1".parse().expect("ip"),
            scheme,
            host: Some(host.to_owned()),
        }
    }

    fn headers(values: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in values {
            headers.append(
                HeaderName::from_bytes(name.as_bytes()).expect("header name"),
                HeaderValue::from_str(value).expect("header value"),
            );
        }
        headers
    }

    #[test]
    fn default_ports_belong_to_the_scheme() {
        // http://nas и http://nas:80 — один origin.
        assert!(allows(
            &client(Scheme::Http, "nas"),
            &headers(&[("origin", "http://nas:80")])
        ));
        assert!(allows(
            &client(Scheme::Http, "nas:80"),
            &headers(&[("origin", "http://nas")])
        ));
        // https://nas и https://nas:443 — один origin.
        assert!(allows(
            &client(Scheme::Https, "nas"),
            &headers(&[("origin", "https://nas:443")])
        ));
        assert!(allows(
            &client(Scheme::Https, "nas:443"),
            &headers(&[("origin", "https://nas")])
        ));
        // Явный порт другой схемы — уже другой origin.
        assert!(!allows(
            &client(Scheme::Http, "nas:443"),
            &headers(&[("origin", "https://nas:443")])
        ));
    }

    #[test]
    fn the_host_is_compared_without_case() {
        assert!(allows(
            &client(Scheme::Http, "NAS.Example:18081"),
            &headers(&[("origin", "http://nas.example:18081")])
        ));
        assert!(allows(
            &client(Scheme::Http, "nas.example:18081"),
            &headers(&[("origin", "http://NAS.EXAMPLE:18081")])
        ));
    }

    #[test]
    fn an_ipv6_host_is_compared_in_brackets() {
        assert!(allows(
            &client(Scheme::Http, "[fd00::1]:18081"),
            &headers(&[("origin", "http://[fd00::1]:18081")])
        ));
        // Другой порт и отсутствие скобок — не тот origin.
        assert!(!allows(
            &client(Scheme::Http, "[fd00::1]:18081"),
            &headers(&[("origin", "http://[fd00::1]:8123")])
        ));
        assert!(request_origin(&client(Scheme::Http, "fd00::1")).is_none());
    }

    #[test]
    fn the_referer_contributes_only_its_origin() {
        assert!(allows(
            &client(Scheme::Http, "nas:18081"),
            &headers(&[("referer", "http://nas:18081/admin/login?next=%2Fadmin#top")])
        ));
        assert!(!allows(
            &client(Scheme::Http, "nas:18081"),
            &headers(&[("referer", "http://nas:8123/admin/login")])
        ));
    }

    #[test]
    fn a_null_or_broken_origin_does_not_fall_back_to_the_referer() {
        for origin in ["null", "NULL", "not a url", "/relative", "http://nas/path"] {
            let headers = headers(&[
                ("origin", origin),
                ("referer", "http://nas:18081/admin/login"),
            ]);
            assert!(
                !allows(&client(Scheme::Http, "nas:18081"), &headers),
                "Origin {origin:?} must be refused even with a matching Referer"
            );
        }
    }

    #[test]
    fn a_missing_source_is_refused() {
        assert!(!allows(&client(Scheme::Http, "nas:18081"), &headers(&[])));
        // Пустые значения — тоже отсутствие источника.
        assert!(!allows(
            &client(Scheme::Http, "nas:18081"),
            &headers(&[("origin", "")])
        ));
        assert!(!allows(
            &client(Scheme::Http, "nas:18081"),
            &headers(&[("referer", "")])
        ));
    }

    #[test]
    fn a_valid_foreign_origin_does_not_fall_back_to_a_matching_referer() {
        let headers = headers(&[
            ("origin", "http://evil.example"),
            ("referer", "http://nas:18081/admin/login"),
        ]);
        assert!(
            !allows(&client(Scheme::Http, "nas:18081"), &headers),
            "a present Origin decides even when the Referer matches"
        );
    }

    #[test]
    fn strict_authority_rejects_empty_ports_and_control_characters() {
        for host in [
            "nas:",
            "nas\\",
            "nas\t18081",
            "nas\n18081",
            "nas\r18081",
            "nas:18 081",
            "na_s:18081",
            "nas:18081/",
        ] {
            assert!(
                request_origin(&client(Scheme::Http, host)).is_none(),
                "host {host:?} must not be normalized into an origin"
            );
        }
        assert!(
            request_origin(&client(Scheme::Http, "[fd00::1]:18081")).is_some(),
            "a bracketed IPv6 host with a port stays valid"
        );
    }

    #[test]
    fn garbage_in_the_request_host_makes_the_origin_unknown() {
        for host in [
            "",
            "user@nas:18081",
            "nas/path",
            "nas?query=1",
            "not a host",
        ] {
            assert!(
                request_origin(&client(Scheme::Http, host)).is_none(),
                "host {host:?} must not parse"
            );
            assert!(!allows(
                &client(Scheme::Http, host),
                &headers(&[("origin", "http://nas:18081")])
            ));
        }
        // `X-Forwarded-Host` от доверенного прокси разбирается так же.
        assert!(!allows(
            &client(Scheme::Https, "user@console.example"),
            &headers(&[("origin", "https://console.example")])
        ));
    }

    #[test]
    fn different_schemes_with_the_same_host_and_port_do_not_match() {
        assert!(!allows(
            &client(Scheme::Http, "nas:18081"),
            &headers(&[("origin", "https://nas:18081")])
        ));
        assert!(!allows(
            &client(Scheme::Https, "nas:18081"),
            &headers(&[("origin", "http://nas:18081")])
        ));
    }

    #[test]
    fn an_origin_with_a_userinfo_or_path_is_refused() {
        for origin in ["http://user@nas:18081", "http://nas:18081/path"] {
            assert!(!allows(
                &client(Scheme::Http, "nas:18081"),
                &headers(&[("origin", origin)])
            ));
        }
    }
}
